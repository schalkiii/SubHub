use crate::model::{Proxy, Subscription};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// A declarative transform pipeline applied to a set of proxies before export
/// (or display). Mirrors sub-store's "operators" idea: filter -> sort ->
/// rename, applied in order. All fields are optional so the UI can send a
/// partial pipeline.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Transform {
    #[serde(default)]
    pub filters: Vec<FilterRule>,
    pub sort: Option<SortBy>,
    pub rename: Option<RenameRule>,
    /// 数值质量阈值（可选）：导出 / 订阅时排除「带宽低于下限」或「延迟高于上限」
    /// 的节点。单位：带宽 bps、延迟 ms。未测速（无对应数据）的节点按「保留」处理，
    /// 避免把还没测过的节点误删。
    #[serde(default)]
    pub min_bandwidth_bps: Option<u64>,
    #[serde(default)]
    pub max_latency_ms: Option<u64>,
}

/// Keep or drop nodes by matching a field.
/// - field: name | type | region | server
/// - mode: include (keep matches) | exclude (drop matches)
/// - match_: contains | regex | exact
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterRule {
    pub field: String,
    pub mode: String,
    pub match_: String,
    pub value: String,
}

/// Sort the resulting list.
/// - key: name | latency | speed | type | score
/// - desc: reverse order (for `speed` and `score` the natural ordering is
///   "best first" when `desc == true`; for `latency` the default is
///   "lowest first")
///
/// Availability dominates every key: a node explicitly marked unavailable
/// (`available == Some(false)`) is always moved to the bottom regardless of
/// its column value, so a dead node that still carries a stale latency or
/// bandwidth from a previous good test can never outrank a usable one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SortBy {
    pub key: String,
    pub desc: bool,
}

/// Regex-rename node names. `pattern` is a Rust regex; `replacement` may use
/// `$1`, `$2`, ... capture groups. Nodes whose name doesn't match are left
/// untouched (regex replace is a no-op on non-matching input).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameRule {
    pub pattern: String,
    pub replacement: String,
}

fn field_val(p: &Proxy, field: &str) -> String {
    match field {
        "name" => p.name.clone(),
        "type" => p.type_.as_str().to_string(),
        // 与节点列表「地区」列一致：优先实测出口地区，回退名字猜测
        "region" => p.effective_region(),
        "server" => p.server.clone(),
        _ => p.name.clone(),
    }
}

/// Run the full pipeline on a copy of the input list.
///
/// Returns `Err` (with a human-readable message) when a regex used in a filter
/// or the rename rule fails to compile, so the caller can surface the mistake
/// to the user instead of silently producing an empty (include-mode) or
/// 导出前地区名修正（**仅作用于传入的节点副本，绝不改动落库数据**）。
///
/// 对「名字未识别地区（`region()` 返回 `"OTHER"`）但实测出口地区
/// （`outbound_country`）已知」的节点，把出口国家码前置到名字前，使 Clash Verge
/// 等按名字匹配地区的客户端不再把它们归为「其他」。例如名字叫「其他」、出口实测为
/// HK 的节点会被重命名为「HK 其他」。
///
/// 同一导出集合内若重名（含与原名撞名），追加 ` #2` / ` #3` 后缀去重，与
/// `export::ensure_unique_names` 行为对齐（双重保险，最终输出必唯一）。
/// `export_str` 内部的 `ensure_unique_names` 仍会兜底，这里提前去重只是让重命名
/// 后的名字更干净、可读。
pub fn region_fixup_for_export(proxies: &mut [Proxy]) {
    use std::collections::HashMap;
    let mut seen: HashMap<String, usize> = HashMap::new();
    for p in proxies.iter_mut() {
        // 需要重命名的节点：region 判定为 OTHER 且已有实测出口地区
        let candidate = if p.region() == "OTHER" {
            p.outbound_country
                .as_ref()
                .filter(|c| !c.is_empty())
                .map(|code| format!("{} {}", code, p.name))
        } else {
            None
        };
        let base = candidate.unwrap_or_else(|| p.name.clone());
        let count = seen.entry(base.clone()).or_insert(0);
        *count += 1;
        if *count > 1 {
            let renamed = format!("{} #{}", base, *count);
            // 后缀名本身也登记，避免后续再次撞名
            seen.entry(renamed.clone()).or_insert(1);
            p.name = renamed;
        } else {
            p.name = base;
        }
    }
}

/// all-kept (exclude-mode) export.
pub fn apply(proxies: &[Proxy], t: &Transform) -> Result<Vec<Proxy>, String> {
    // Precompile every regex filter once (not per-node) so a large list with
    // several regex rules doesn't pay a Regex::new cost for every node. A bad
    // regex is reported immediately rather than silently dropping the export.
    let compiled: Vec<(FilterRule, Option<Regex>)> = t
        .filters
        .iter()
        .map(|rule| {
            let re = if rule.match_ == "regex" {
                Some(
                    Regex::new(&rule.value)
                        .map_err(|e| format!("正则过滤器「{}」无效: {}", rule.value, e))?,
                )
            } else {
                None
            };
            Ok((rule.clone(), re))
        })
        .collect::<Result<Vec<_>, String>>()?;

    // 1) filters (combined as AND)
    let mut out: Vec<Proxy> = proxies
        .iter()
        .filter(|p| compiled.iter().all(|(rule, re)| keep(p, rule, re)))
        .cloned()
        .collect();

    // 1b) 数值质量阈值：按带宽下限 / 延迟上限过滤。未测速（无数据）的节点无法
    // 判断是否超标，按「保留」处理，避免把还没测过的节点误删。
    if let Some(min_bw) = t.min_bandwidth_bps {
        let min = min_bw as f64;
        out.retain(|p| match p.download_speed_bps {
            Some(b) => b >= min,
            None => true,
        });
    }
    if let Some(max_lat) = t.max_latency_ms {
        out.retain(|p| match p.latency_ms {
            Some(l) => l <= max_lat,
            None => true,
        });
    }

    // 2) rename
    if let Some(r) = &t.rename {
        let re = Regex::new(&r.pattern)
            .map_err(|e| format!("重命名正则「{}」无效: {}", r.pattern, e))?;
        // `\$1` in the replacement emits a literal `$1` instead of a capture
        // group. Rust's regex uses `$$` for a literal dollar, so map `\$`→`$$`.
        let repl = r.replacement.replace("\\$", "$$");
        for p in out.iter_mut() {
            p.name = re.replace(&p.name, repl.as_str()).to_string();
        }
    }

    // 3) sort
    //
    // Availability dominates: a node explicitly marked unavailable
    // (`available == Some(false)`) ALWAYS sinks to the bottom regardless of the
    // chosen column. Without this, a node that used to work (and still carries a
    // stale `latency_ms` / `download_speed_bps` from that earlier good test)
    // would outrank a healthy node when sorting by latency or speed. Only
    // genuinely usable nodes (available == Some(true) or untested == None)
    // participate in the column ordering.
    if let Some(s) = &t.sort {
        let avail_rank = |p: &Proxy| -> u8 {
            if p.available == Some(false) {
                1
            } else {
                0
            }
        };
        match s.key.as_str() {
            "latency" => out.sort_by(|a, b| {
                avail_rank(a).cmp(&avail_rank(b)).then_with(|| {
                    let av = a.latency_ms.unwrap_or(u64::MAX);
                    let bv = b.latency_ms.unwrap_or(u64::MAX);
                    if s.desc {
                        bv.cmp(&av)
                    } else {
                        av.cmp(&bv)
                    }
                })
            }),
            "tcplat" => out.sort_by(|a, b| {
                avail_rank(a).cmp(&avail_rank(b)).then_with(|| {
                    let av = a.tcp_latency_ms.unwrap_or(u64::MAX);
                    let bv = b.tcp_latency_ms.unwrap_or(u64::MAX);
                    if s.desc {
                        bv.cmp(&av)
                    } else {
                        av.cmp(&bv)
                    }
                })
            }),
            "httplat" => out.sort_by(|a, b| {
                avail_rank(a).cmp(&avail_rank(b)).then_with(|| {
                    let av = a.http_latency_ms.unwrap_or(u64::MAX);
                    let bv = b.http_latency_ms.unwrap_or(u64::MAX);
                    if s.desc {
                        bv.cmp(&av)
                    } else {
                        av.cmp(&bv)
                    }
                })
            }),
            "speed" => out.sort_by(|a, b| {
                // higher bandwidth is better -> `desc` (default) = fastest first
                avail_rank(a).cmp(&avail_rank(b)).then_with(|| {
                    let av = a.download_speed_bps.unwrap_or(0.0);
                    let bv = b.download_speed_bps.unwrap_or(0.0);
                    if s.desc {
                        // fastest first: b with the larger value ranks before a
                        bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
                    } else {
                        // slowest first
                        av.partial_cmp(&bv).unwrap_or(std::cmp::Ordering::Equal)
                    }
                })
            }),
            "type" => out.sort_by(|a, b| {
                avail_rank(a)
                    .cmp(&avail_rank(b))
                    .then_with(|| a.type_.as_str().cmp(b.type_.as_str()))
            }),
            "score" => out.sort_by(|a, b| {
                // Composite score (see crate::score::score_proxy); higher is
                // better, so the default/dense ordering is "best first" and
                // `desc == false` flips to worst first — mirroring the `speed`
                // key's convention. Availability still dominates (score_proxy
                // already returns 0 for dead nodes, but avail_rank keeps the
                // tie-break behaviour identical to every other key).
                avail_rank(a).cmp(&avail_rank(b)).then_with(|| {
                    let av = crate::score::score_proxy(a);
                    let bv = crate::score::score_proxy(b);
                    if s.desc {
                        bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
                    } else {
                        av.partial_cmp(&bv).unwrap_or(std::cmp::Ordering::Equal)
                    }
                })
            }),
            _ => out.sort_by(|a, b| {
                avail_rank(a)
                    .cmp(&avail_rank(b))
                    .then_with(|| a.name.cmp(&b.name))
            }),
        }
    }

    Ok(out)
}

/// Select up to `n` nodes with the highest composite score.
///
/// Only *usable* nodes (`is_usable`: exportable and not confirmed dead) take
/// part. Exporting filters those out afterwards anyway; picking Top-N from the
/// full list would let dead/unsupported entries consume slots and yield fewer
/// than N nodes in the final output.
pub fn top_n_by_score(proxies: &[Proxy], n: usize) -> Vec<Proxy> {
    let mut scored: Vec<(f64, Proxy)> = proxies
        .iter()
        .filter(|p| p.is_usable())
        .map(|p| (crate::score::score_proxy(p), p.clone()))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(n).map(|(_, p)| p).collect()
}

fn keep(p: &Proxy, rule: &FilterRule, re: &Option<Regex>) -> bool {
    let val = field_val(p, &rule.field);
    let matched = match rule.match_.as_str() {
        "regex" => re.as_ref().map(|r| r.is_match(&val)).unwrap_or(false),
        "exact" => val == rule.value,
        _ => val.to_lowercase().contains(&rule.value.to_lowercase()),
    };
    match rule.mode.as_str() {
        "exclude" => !matched,
        _ => matched, // include (default)
    }
}

/// BestSub-style incremental update of a subscription's node list.
///
/// When a remote subscription is re-fetched, we don't want to throw away the
/// health / speed-test data we already measured for nodes that are still
/// present upstream. This function:
///   1. keeps every node that survives the refresh (matched by `fingerprint()`)
///      and **carries over** its previously measured latency columns
///      (`latency_ms` / `tcp_latency_ms` / `http_latency_ms`), `available`,
///      `download_speed_bps` / `bandwidth_measured`, `last_tested_at`,
///      `consecutive_failures`, `outbound_country` and `unlock`, so a refresh
///      neither blanks the latency columns nor resets the auto-remove counter;
///   2. appends **only the genuinely new** nodes (those whose fingerprint is
///      not in `old`) — these are the only ones that still need a speed test;
///   3. drops nodes that are no longer present in the freshly fetched `new`
///      list (the upstream removed them).
///
/// Returns `(merged, new_nodes)` where `merged` is the full updated node list
/// and `new_nodes` is the subset that should be speed-tested (the rest already
/// have valid, preserved results). Duplicates within `new` itself are also
/// de-duplicated (first occurrence wins) so a subscription never accumulates
/// intra-subscription duplicates.
pub fn incremental_update(old: &[Proxy], new: &[Proxy]) -> (Vec<Proxy>, Vec<Proxy>) {
    let old_map: HashMap<String, Proxy> =
        old.iter().map(|p| (p.fingerprint(), p.clone())).collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut merged: Vec<Proxy> = Vec::with_capacity(new.len());
    let mut new_nodes: Vec<Proxy> = Vec::new();

    for n in new {
        let fp = n.fingerprint();
        // de-dup within the freshly fetched list (keep first occurrence)
        if !seen.insert(fp.clone()) {
            continue;
        }
        if let Some(o) = old_map.get(&fp) {
            // surviving node: preserve its previously measured health fields
            // (the freshly parsed node has none yet right after a fetch).
            let mut merged_p = n.clone();
            merged_p.latency_ms = o.latency_ms;
            merged_p.tcp_latency_ms = o.tcp_latency_ms;
            merged_p.http_latency_ms = o.http_latency_ms;
            merged_p.available = o.available;
            merged_p.download_speed_bps = o.download_speed_bps;
            merged_p.bandwidth_measured = o.bandwidth_measured;
            merged_p.last_tested_at = o.last_tested_at;
            merged_p.consecutive_failures = o.consecutive_failures;
            merged_p.outbound_country = o.outbound_country.clone();
            merged_p.unlock = o.unlock.clone();
            merged.push(merged_p);
        } else {
            // genuinely new node: collect for (only) its speed test
            merged.push(n.clone());
            new_nodes.push(n.clone());
        }
    }

    (merged, new_nodes)
}

/// Merge incoming subscriptions into an existing store, keyed by `source`.
///
/// This is the single source of truth for the "re-add == refresh" semantics
/// shared by `POST /api/subscriptions` and `POST /api/subscriptions/import`:
///
/// * An incoming subscription whose `source` already exists is updated **in
///   place** (never duplicated) via [`incremental_update`], which preserves
///   the previously measured health of surviving nodes.
/// * A failed re-fetch (`health.last_error` set) keeps the *old* node list —
///   only the health / error bookkeeping is refreshed — exactly like a normal
///   `refresh`, so a transient network blip can't wipe a working node set.
/// * A subscription whose `source` is new is appended.
///
/// Returns `(created, added_proxies, new_nodes)` where `created` is the number
/// of *newly created* subscriptions (not merged ones), `added_proxies` is the
/// total node count of those newly created subscriptions, and `new_nodes` is
/// the subset that genuinely has never been measured before (all nodes of a
/// created subscription, plus only the fresh nodes of a merged one). Callers
/// use `new_nodes` for the post-add speed test so re-adding an existing URL
/// refreshes the subscription instead of re-testing every node it contains.
pub fn merge_subscriptions_by_source(
    existing: &mut Vec<Subscription>,
    incoming: Vec<Subscription>,
) -> (usize, usize, Vec<Proxy>) {
    let mut created = 0usize;
    let mut added_proxies = 0usize;
    let mut new_nodes: Vec<Proxy> = Vec::new();
    for sub in incoming {
        if let Some(e) = existing.iter_mut().find(|s| s.source == sub.source) {
            let (merged, fresh) = incremental_update(&e.proxies, &sub.proxies);
            // On a failed re-fetch keep the old node list (merged would be
            // empty / stale) — same as subscription refresh.
            if sub.health.last_error.is_none() {
                e.proxies = merged;
                e.health.last_updated_at = sub.health.last_updated_at;
                e.health.upload = sub.health.upload;
                e.health.download = sub.health.download;
                e.health.total = sub.health.total;
                e.health.expire = sub.health.expire;
                new_nodes.extend(fresh);
            }
            e.health.last_checked_at = sub.health.last_checked_at;
            e.health.last_error = sub.health.last_error.clone();
            if sub.fetch_proxy.is_some() {
                e.fetch_proxy = sub.fetch_proxy.clone();
            }
        } else {
            added_proxies += sub.proxies.len();
            new_nodes.extend(sub.proxies.iter().cloned());
            existing.push(sub);
            created += 1;
        }
    }
    (created, added_proxies, new_nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Proxy, ProxyType};

    fn node(name: &str, server: &str, port: u16) -> Proxy {
        Proxy::new(name.to_string(), ProxyType::Ss, server.to_string(), port)
    }

    #[test]
    fn incremental_update_keeps_surviving_health_drops_removed_adds_new() {
        // old store: A (tested, alive), B (tested, dead)
        let mut a = node("A", "10.0.0.1", 1);
        a.latency_ms = Some(120);
        a.available = Some(true);
        a.last_tested_at = Some(1000);
        let mut b = node("B", "10.0.0.2", 2);
        b.latency_ms = Some(900);
        b.available = Some(false);
        b.last_tested_at = Some(1000);
        let old = vec![a, b];

        // new fetch: B survives, C is genuinely new, A was removed upstream
        let new_b = node("B", "10.0.0.2", 2); // freshly parsed, no health yet
        let c = node("C", "10.0.0.3", 3);
        let new = vec![new_b, c];

        let (merged, new_nodes) = incremental_update(&old, &new);

        // A dropped, B kept, C added
        assert_eq!(merged.len(), 2, "A removed, B+C remain");
        let merged_b = merged.iter().find(|p| p.name == "B").unwrap();
        // B's previously measured health must be preserved (not re-tested)
        assert_eq!(merged_b.latency_ms, Some(900));
        assert_eq!(merged_b.available, Some(false));
        assert_eq!(merged_b.last_tested_at, Some(1000));
        // only C is new and needs a speed test
        assert_eq!(new_nodes.len(), 1);
        assert_eq!(new_nodes[0].name, "C");
        assert!(merged.iter().all(|p| p.name != "A"));
    }

    #[test]
    fn incremental_update_dedups_within_new() {
        let old: Vec<Proxy> = vec![];
        let x1 = node("X", "10.0.0.9", 9);
        let x2 = node("X", "10.0.0.9", 9); // duplicate fingerprint
        let (merged, new_nodes) = incremental_update(&old, &[x1, x2]);
        assert_eq!(merged.len(), 1, "intra-list duplicates collapsed");
        assert_eq!(new_nodes.len(), 1);
    }

    #[test]
    fn incremental_update_preserves_bandwidth_measured() {
        // Q2: a surviving engine-tested node must keep `bandwidth_measured`
        // across a refresh, otherwise score_proxy() drops its real bandwidth
        // signal and Top-N ranking silently changes.
        let mut a = node("A", "10.0.0.1", 1);
        a.download_speed_bps = Some(25_000_000.0);
        a.bandwidth_measured = true;
        a.latency_ms = Some(80);
        a.available = Some(true);
        let old = vec![a];

        // freshly parsed node has no health yet
        let new_a = node("A", "10.0.0.1", 1);
        let (merged, _new_nodes) = incremental_update(&old, &[new_a]);

        let merged_a = merged.iter().find(|p| p.name == "A").unwrap();
        assert!(
            merged_a.bandwidth_measured,
            "bandwidth_measured must survive refresh"
        );
        assert_eq!(merged_a.download_speed_bps, Some(25_000_000.0));
    }

    #[test]
    fn incremental_update_preserves_latency_columns_and_failure_streak() {
        // 回归：tcp_latency_ms / http_latency_ms / consecutive_failures 是在
        // 双延迟模型与自动移除上线后新增的字段；旧实现只复制 latency_ms，
        // 每次刷新都会把 TCP/HTTP 两列清空、并把连续失败计数归零。
        let mut old_node = node("A", "10.0.0.1", 1);
        old_node.latency_ms = Some(120);
        old_node.tcp_latency_ms = Some(30);
        old_node.http_latency_ms = Some(120);
        old_node.available = Some(true);
        old_node.consecutive_failures = 2;

        let (merged, _) = incremental_update(&[old_node], &[node("A", "10.0.0.1", 1)]);
        let m = &merged[0];
        assert_eq!(m.latency_ms, Some(120));
        assert_eq!(m.tcp_latency_ms, Some(30), "TCP 延迟必须跨刷新保留");
        assert_eq!(m.http_latency_ms, Some(120), "HTTP 延迟必须跨刷新保留");
        assert_eq!(m.consecutive_failures, 2, "连续失败计数必须保留");
    }

    #[test]
    fn rename_emits_literal_dollar_via_escaped_placeholder() {
        // `\$1` in the replacement must yield a literal `$1` (not a capture
        // group reference). Regression guard for the `\$` -> `$$` escaping fix
        // in apply()'s rename step.
        let p = node("US-Test", "10.0.0.5", 5);
        let t = Transform {
            filters: vec![],
            sort: None,
            rename: Some(RenameRule {
                pattern: "US-(.*)".to_string(),
                replacement: "\\$1-OK".to_string(),
            }),
            min_bandwidth_bps: None,
            max_latency_ms: None,
        };
        let out = apply(std::slice::from_ref(&p), &t).unwrap();
        assert_eq!(
            out[0].name, "$1-OK",
            "escaped \\$ must yield a literal dollar"
        );

        // an unescaped `$1` still behaves as a capture-group reference
        let t2 = Transform {
            filters: vec![],
            sort: None,
            rename: Some(RenameRule {
                pattern: "US-(.*)".to_string(),
                replacement: "$1".to_string(),
            }),
            min_bandwidth_bps: None,
            max_latency_ms: None,
        };
        let out2 = apply(&[p], &t2).unwrap();
        assert_eq!(out2[0].name, "Test", "unescaped $1 stays a capture group");
    }

    #[test]
    fn bad_regex_in_filter_returns_err_not_silent_empty() {
        // A malformed regex must surface as Err so the UI can show the error
        // instead of silently exporting nothing (include mode) or everything
        // (exclude mode). Regression guard for the T71 regex-compile fix.
        let p = node("US-Test", "10.0.0.5", 5);
        let t = Transform {
            filters: vec![FilterRule {
                field: "name".to_string(),
                mode: "include".to_string(),
                match_: "regex".to_string(),
                value: "[unclosed".to_string(),
            }],
            sort: None,
            rename: None,
            min_bandwidth_bps: None,
            max_latency_ms: None,
        };
        let res = apply(&[p], &t);
        assert!(res.is_err(), "invalid regex must return Err, got Ok");
        assert!(
            res.unwrap_err().contains("无效"),
            "error message must name the bad regex"
        );
    }

    #[test]
    fn bad_regex_in_rename_returns_err() {
        let p = node("US-Test", "10.0.0.5", 5);
        let t = Transform {
            filters: vec![],
            sort: None,
            rename: Some(RenameRule {
                pattern: "*badclass".to_string(),
                replacement: "x".to_string(),
            }),
            min_bandwidth_bps: None,
            max_latency_ms: None,
        };
        let res = apply(&[p], &t);
        assert!(res.is_err(), "invalid rename regex must return Err");
        assert!(
            res.unwrap_err().contains("重命名正则"),
            "error must name the rename rule"
        );
    }

    #[test]
    fn sort_latency_keeps_unavailable_at_bottom() {
        // Regression for "按延迟排序时大量不可用节点排在前排". A node
        // that used to work keeps a *stale* low latency after it dies; a
        // healthy node may have a higher latency. Availability must dominate:
        // the unavailable node (low latency) must sink below the available
        // one (high latency).
        let mut dead = node("Dead", "10.0.0.1", 1);
        dead.latency_ms = Some(10); // stale good value
        dead.available = Some(false);
        let mut alive = node("Alive", "10.0.0.2", 2);
        alive.latency_ms = Some(500);
        alive.available = Some(true);

        let t_asc = Transform {
            filters: vec![],
            sort: Some(SortBy {
                key: "latency".to_string(),
                desc: false,
            }),
            rename: None,
            min_bandwidth_bps: None,
            max_latency_ms: None,
        };
        let out = apply(&[dead.clone(), alive.clone()], &t_asc).unwrap();
        assert_eq!(
            out[0].name, "Alive",
            "available must outrank unavailable even with higher latency"
        );
        assert_eq!(out[1].name, "Dead");

        // descending must also keep the unavailable node last
        let t_desc = Transform {
            filters: vec![],
            sort: Some(SortBy {
                key: "latency".to_string(),
                desc: true,
            }),
            rename: None,
            min_bandwidth_bps: None,
            max_latency_ms: None,
        };
        let out2 = apply(&[dead, alive], &t_desc).unwrap();
        assert_eq!(
            out2[0].name, "Alive",
            "descending: available still before unavailable"
        );
        assert_eq!(out2[1].name, "Dead");
    }

    #[test]
    fn sort_score_orders_best_first_and_sinks_dead() {
        // Q/B6: `sort.key == "score"` must use the shared score_proxy —
        // best composite score first when desc == true, dead nodes always last
        // (they score 0 AND avail_rank sinks them).
        let mut fast = node("Fast", "10.0.0.1", 1);
        fast.available = Some(true);
        fast.latency_ms = Some(50);
        let mut slow = node("Slow", "10.0.0.2", 2);
        slow.available = Some(true);
        slow.latency_ms = Some(1500);
        let mut dead = node("Dead", "10.0.0.3", 3);
        dead.available = Some(false);
        dead.latency_ms = Some(1); // stale perfect latency must not matter

        let t = Transform {
            filters: vec![],
            sort: Some(SortBy {
                key: "score".to_string(),
                desc: true,
            }),
            rename: None,
            min_bandwidth_bps: None,
            max_latency_ms: None,
        };
        let out = apply(&[dead.clone(), slow.clone(), fast.clone()], &t).unwrap();
        assert_eq!(out[0].name, "Fast", "best score first");
        assert_eq!(out[1].name, "Slow");
        assert_eq!(out[2].name, "Dead", "dead node must sink to the bottom");

        // ascending flips the usable nodes but the dead one still sinks
        let t_asc = Transform {
            filters: vec![],
            sort: Some(SortBy {
                key: "score".to_string(),
                desc: false,
            }),
            rename: None,
            min_bandwidth_bps: None,
            max_latency_ms: None,
        };
        let out2 = apply(&[fast, dead, slow], &t_asc).unwrap();
        assert_eq!(out2[0].name, "Slow", "ascending: worst usable score first");
        assert_eq!(out2[1].name, "Fast");
        assert_eq!(
            out2[2].name, "Dead",
            "avail_rank keeps dead last even ascending"
        );
    }

    #[test]
    fn sort_speed_keeps_unavailable_at_bottom() {
        // Same dominance guarantee when sorting by bandwidth: a dead node
        // carrying a stale high `download_speed_bps` must not outrank a
        // live one.
        let mut dead = node("Dead", "10.0.0.1", 1);
        dead.download_speed_bps = Some(80_000_000.0); // stale, 80 MB/s
        dead.available = Some(false);
        let mut alive = node("Alive", "10.0.0.2", 2);
        alive.download_speed_bps = Some(5_000_000.0); // 5 MB/s
        alive.available = Some(true);

        let t = Transform {
            filters: vec![],
            sort: Some(SortBy {
                key: "speed".to_string(),
                desc: true,
            }),
            rename: None,
            min_bandwidth_bps: None,
            max_latency_ms: None,
        };
        let out = apply(&[dead, alive], &t).unwrap();
        assert_eq!(
            out[0].name, "Alive",
            "available must outrank dead even with lower speed"
        );
        assert_eq!(out[1].name, "Dead");
    }

    #[test]
    fn top_n_selects_only_usable_nodes() {
        // 死节点虽然可能带着旧的超低延迟，也不能占用 Top-N 名额；Other 类型
        // 同样会在导出前被过滤，不应参与选取。
        let mut dead = node("Dead", "10.0.0.1", 1);
        dead.password = Some("pw".into());
        dead.method = Some("aes-256-gcm".into());
        dead.available = Some(false);
        dead.latency_ms = Some(1);
        let mut alive_fast = node("AliveFast", "10.0.0.2", 2);
        alive_fast.password = Some("pw".into());
        alive_fast.method = Some("aes-256-gcm".into());
        alive_fast.available = Some(true);
        alive_fast.latency_ms = Some(50);
        let mut alive_slow = node("AliveSlow", "10.0.0.3", 3);
        alive_slow.password = Some("pw".into());
        alive_slow.method = Some("aes-256-gcm".into());
        alive_slow.available = Some(true);
        alive_slow.latency_ms = Some(500);
        let mut other = node("Other", "10.0.0.4", 4);
        other.type_ = ProxyType::Other;

        let picked = top_n_by_score(&[dead, other, alive_slow, alive_fast], 2);
        assert_eq!(picked.len(), 2, "应取到 2 个可用节点");
        assert_eq!(picked[0].name, "AliveFast", "评分高的排前");
        assert_eq!(picked[1].name, "AliveSlow");

        // n 大于可用节点数时返回全部可用节点，不报错。
        let mut single = node("a", "1.1.1.1", 1);
        single.password = Some("pw".into());
        single.method = Some("aes-256-gcm".into());
        let all = top_n_by_score(&[single], 10);
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn numeric_thresholds_drop_slow_high_latency_keep_untested() {
        // 排除带宽低于下限、延迟高于上限的节点；未测速（无数据）节点保留。
        let mut fast = node("Fast", "10.0.0.1", 1);
        fast.available = Some(true);
        fast.latency_ms = Some(50);
        fast.download_speed_bps = Some(50_000_000.0);
        let mut slow = node("Slow", "10.0.0.2", 2);
        slow.available = Some(true);
        slow.latency_ms = Some(1500);
        slow.download_speed_bps = Some(40_000_000.0);
        let mut weak = node("Weak", "10.0.0.3", 3);
        weak.available = Some(true);
        weak.latency_ms = Some(80);
        weak.download_speed_bps = Some(1_000_000.0); // 1 MB/s
                                                     // 未测速：无延迟 / 带宽数据
        let untested = node("Untested", "10.0.0.4", 4);

        let t = Transform {
            filters: vec![],
            sort: None,
            rename: None,
            min_bandwidth_bps: Some(5_000_000), // 5 MB/s 下限
            max_latency_ms: Some(500),
        };
        let out = apply(
            &[fast.clone(), slow.clone(), weak.clone(), untested.clone()],
            &t,
        )
        .unwrap();
        let names: Vec<&str> = out.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"Fast"), "快且低延迟应保留");
        assert!(!names.contains(&"Slow"), "高延迟应排除");
        assert!(!names.contains(&"Weak"), "低带宽应排除");
        assert!(
            names.contains(&"Untested"),
            "未测速（无数据）应保留，不误删"
        );
    }

    #[test]
    fn merge_creates_new_and_counts_only_added_proxies() {
        // Two brand-new subscriptions are both created; `added` reflects the
        // node count of the genuinely new subs (not any merged ones).
        let mut store: Vec<Subscription> = vec![];
        let a = Subscription::new(
            "A".into(),
            "https://a.example/sub".into(),
            vec![node("n1", "1.1.1.1", 1)],
        );
        let b = Subscription::new(
            "B".into(),
            "https://b.example/sub".into(),
            vec![node("n2", "2.2.2.2", 2), node("n3", "3.3.3.3", 3)],
        );

        let (created, added, fresh) = merge_subscriptions_by_source(&mut store, vec![a, b]);
        assert_eq!(created, 2, "both subscriptions are new");
        assert_eq!(added, 3, "three nodes across the two new subs");
        assert_eq!(fresh.len(), 3, "all nodes of new subs need a first test");
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn merge_does_not_duplicate_on_re_add() {
        // Re-adding an existing source URL must update in place, not create a
        // second entry (the bug Round Q's e2e test caught: duplicate URLs).
        let mut store: Vec<Subscription> = vec![];
        let first = Subscription::new(
            "A".into(),
            "https://a.example/sub".into(),
            vec![node("n1", "1.1.1.1", 1)],
        );
        merge_subscriptions_by_source(&mut store, vec![first]);
        assert_eq!(store.len(), 1);

        // re-add the same source with a different node set
        let again = Subscription::new(
            "A".into(),
            "https://a.example/sub".into(),
            vec![node("n2", "2.2.2.2", 2)],
        );
        let (created, added, fresh) = merge_subscriptions_by_source(&mut store, vec![again]);
        assert_eq!(created, 0, "no new subscription created on re-add");
        assert_eq!(added, 0, "re-add is a refresh, not an addition");
        assert_eq!(
            store.len(),
            1,
            "still a single subscription, not a duplicate"
        );
        assert_eq!(store[0].proxies.len(), 1);
        assert_eq!(
            store[0].proxies[0].name, "n2",
            "node list replaced by refresh"
        );
        assert_eq!(fresh.len(), 1, "only the genuinely new node is re-tested");
        assert_eq!(fresh[0].name, "n2");
    }

    #[test]
    fn merge_keeps_old_nodes_on_failed_refetch() {
        // A failed re-fetch (last_error set) must preserve the existing node
        // list and only record the error — never wipe working nodes.
        let mut store: Vec<Subscription> = vec![];
        let mut good = Subscription::new(
            "A".into(),
            "https://a.example/sub".into(),
            vec![node("n1", "1.1.1.1", 1), node("n2", "2.2.2.2", 2)],
        );
        good.health.last_error = None;
        merge_subscriptions_by_source(&mut store, vec![good]);

        let mut failed = Subscription::new("A".into(), "https://a.example/sub".into(), vec![]);
        failed.health.last_error = Some("connection refused".to_string());
        failed.health.last_checked_at = Some(999);
        let (created, added, fresh) = merge_subscriptions_by_source(&mut store, vec![failed]);

        assert_eq!(created, 0);
        assert_eq!(added, 0);
        assert!(fresh.is_empty(), "failed re-fetch has nothing to test");
        assert_eq!(store.len(), 1);
        assert_eq!(
            store[0].proxies.len(),
            2,
            "old nodes preserved on failed refetch"
        );
        assert_eq!(
            store[0].health.last_error.as_deref(),
            Some("connection refused")
        );
        assert_eq!(store[0].health.last_checked_at, Some(999));
    }

    #[test]
    fn merge_refresh_preserves_surviving_health() {
        // A successful refresh of an existing subscription must keep measured
        // health for surviving nodes (via incremental_update) and report the
        // merged total, not create a duplicate.
        let mut store: Vec<Subscription> = vec![];
        let mut existing = Subscription::new("A".into(), "https://a.example/sub".into(), vec![]);
        // pre-existing measured node
        let mut measured = node("keep", "1.1.1.1", 1);
        measured.latency_ms = Some(42);
        measured.available = Some(true);
        measured.last_tested_at = Some(100);
        existing.proxies = vec![measured];
        merge_subscriptions_by_source(&mut store, vec![existing]);

        // refresh returns the same node plus one genuinely new node
        let refresher = Subscription::new(
            "A".into(),
            "https://a.example/sub".into(),
            vec![node("keep", "1.1.1.1", 1), node("brandnew", "9.9.9.9", 9)],
        );
        let (created, added, fresh) = merge_subscriptions_by_source(&mut store, vec![refresher]);

        assert_eq!(created, 0);
        assert_eq!(
            added, 0,
            "re-add is a refresh; the new node is not 'added' by count"
        );
        assert_eq!(fresh.len(), 1, "only brandnew needs a test");
        assert_eq!(fresh[0].name, "brandnew");
        assert_eq!(store.len(), 1);
        assert_eq!(store[0].proxies.len(), 2, "surviving + new node present");
        let kept = store[0].proxies.iter().find(|p| p.name == "keep").unwrap();
        assert_eq!(
            kept.latency_ms,
            Some(42),
            "measured latency preserved across refresh"
        );
        assert!(store[0].proxies.iter().any(|p| p.name == "brandnew"));
    }

    #[test]
    fn region_fixup_renames_other_with_outbound_and_dedups() {
        // 名字无地区（region==OTHER）但实测出口地区已知 → 前置国家码重命名，
        // 让 Clash Verge 按名字匹配不再归为「其他」。
        let mut a = node("其他", "1.1.1.1", 1);
        a.region(); // 确保 region 判定为 OTHER（名字不含地区信号）
        a.outbound_country = Some("HK".into());
        let mut b = node("其他", "2.2.2.2", 2); // 同名、同出口，需去重
        b.outbound_country = Some("HK".into());
        let mut c = node("香港节点", "3.3.3.3", 3); // 名字已有地区 → 不改
        c.outbound_country = Some("JP".into());
        let d = node("未实测节点", "4.4.4.4", 4); // 无出口地区 → 不改
                                                  // region() 应为 OTHER（名字无地区信号）以验证「不改」分支
        assert_eq!(d.region(), "OTHER");

        let mut all = vec![a, b, c, d];
        region_fixup_for_export(&mut all);

        let by_name = |n: &str| all.iter().find(|p| p.server == n).unwrap();
        assert_eq!(by_name("1.1.1.1").name, "HK 其他");
        assert_eq!(by_name("2.2.2.2").name, "HK 其他 #2", "同名同出口需去重");
        assert_eq!(by_name("3.3.3.3").name, "香港节点", "名字已含地区不改");
        assert_eq!(by_name("4.4.4.4").name, "未实测节点", "无出口地区不改");
    }
}
