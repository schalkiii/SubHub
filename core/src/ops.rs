use crate::model::Proxy;
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
/// - key: name | latency | speed | type
/// - desc: reverse order (for `speed` the default/dense ordering is
///   already "fastest first", so `desc` flips it to slowest first; for
///   `latency` the default is "lowest first")
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
        "region" => p.region(),
        "server" => p.server.clone(),
        _ => p.name.clone(),
    }
}

/// Run the full pipeline on a copy of the input list.
///
/// Returns `Err` (with a human-readable message) when a regex used in a filter
/// or the rename rule fails to compile, so the caller can surface the mistake
/// to the user instead of silently producing an empty (include-mode) or
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
                Some(Regex::new(&rule.value).map_err(|e| {
                    format!("正则过滤器「{}」无效: {}", rule.value, e)
                })?)
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
                avail_rank(a)
                    .cmp(&avail_rank(b))
                    .then_with(|| {
                        let av = a.latency_ms.unwrap_or(u64::MAX);
                        let bv = b.latency_ms.unwrap_or(u64::MAX);
                        if s.desc {
                            bv.cmp(&av)
                        } else {
                            av.cmp(&bv)
                        }
                    })
            }),
            "speed" => out.sort_by(|a, b| {
                // higher bandwidth is better -> `desc` (default) = fastest first
                avail_rank(a)
                    .cmp(&avail_rank(b))
                    .then_with(|| {
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
            _ => out.sort_by(|a, b| {
                avail_rank(a)
                    .cmp(&avail_rank(b))
                    .then_with(|| a.name.cmp(&b.name))
            }),
        }
    }

    Ok(out)
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
///      and **carries over** its previously measured `latency_ms` / `available`
///      / `download_speed_bps` / `last_tested_at` / `outbound_country` /
///      `unlock` so we don't re-test nodes that didn't change;
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
    let old_map: HashMap<String, Proxy> = old
        .iter()
        .map(|p| (p.fingerprint(), p.clone()))
        .collect();
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
            merged_p.available = o.available;
            merged_p.download_speed_bps = o.download_speed_bps;
            merged_p.bandwidth_measured = o.bandwidth_measured;
            merged_p.last_tested_at = o.last_tested_at;
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
        let (merged, new_nodes) = incremental_update(&old, &vec![x1, x2]);
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
        let (merged, _new_nodes) = incremental_update(&old, &vec![new_a]);

        let merged_a = merged.iter().find(|p| p.name == "A").unwrap();
        assert_eq!(merged_a.bandwidth_measured, true, "bandwidth_measured must survive refresh");
        assert_eq!(merged_a.download_speed_bps, Some(25_000_000.0));
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
        };
        let out = apply(&[p.clone()], &t).unwrap();
        assert_eq!(out[0].name, "$1-OK", "escaped \\$ must yield a literal dollar");

        // an unescaped `$1` still behaves as a capture-group reference
        let t2 = Transform {
            filters: vec![],
            sort: None,
            rename: Some(RenameRule {
                pattern: "US-(.*)".to_string(),
                replacement: "$1".to_string(),
            }),
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
        };
        let res = apply(&[p], &t);
        assert!(res.is_err(), "invalid regex must return Err, got Ok");
        assert!(res.unwrap_err().contains("无效"), "error message must name the bad regex");
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
        };
        let res = apply(&[p], &t);
        assert!(res.is_err(), "invalid rename regex must return Err");
        assert!(res.unwrap_err().contains("重命名正则"), "error must name the rename rule");
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
            sort: Some(SortBy { key: "latency".to_string(), desc: false }),
            rename: None,
        };
        let out = apply(&[dead.clone(), alive.clone()], &t_asc).unwrap();
        assert_eq!(out[0].name, "Alive", "available must outrank unavailable even with higher latency");
        assert_eq!(out[1].name, "Dead");

        // descending must also keep the unavailable node last
        let t_desc = Transform {
            filters: vec![],
            sort: Some(SortBy { key: "latency".to_string(), desc: true }),
            rename: None,
        };
        let out2 = apply(&[dead, alive], &t_desc).unwrap();
        assert_eq!(out2[0].name, "Alive", "descending: available still before unavailable");
        assert_eq!(out2[1].name, "Dead");
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
            sort: Some(SortBy { key: "speed".to_string(), desc: true }),
            rename: None,
        };
        let out = apply(&[dead, alive], &t).unwrap();
        assert_eq!(out[0].name, "Alive", "available must outrank dead even with lower speed");
        assert_eq!(out[1].name, "Dead");
    }
}
