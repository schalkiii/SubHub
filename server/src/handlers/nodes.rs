//! 节点视图相关 handler：节点列表（分页 / 排序 / 过滤）、仪表盘汇总、健康趋势，
//! 以及节点出口地区（geo）探测。
use crate::*;
use crate::{
    apply_mode, engine_bin_of, flatten_dedup, now_ms, persist_all, sub_to_summary, LockExt,
};
use axum::{
    extract::{Query, State},
    Json,
};
use std::collections::HashMap;
use subhub_core::Proxy;
/// BestSub-style outbound geo detection: route each proxy through the external
/// engine and ask the geo-IP channels for the exit country. Persists the code
/// back onto each proxy. No-op (empty result) when no engine is configured.
pub(crate) async fn geo_detect(
    State(state): State<AppState>,

    Json(req): Json<SpeedTestReq>,
) -> Json<Vec<serde_json::Value>> {
    let timeout_ms = req.timeout_ms.unwrap_or(8000);

    let mut proxies: Vec<Proxy> = {
        let guard = state.store.lock_ok();

        flatten_dedup(&guard, None)
    };
    let mode = req.mode.clone().unwrap_or_default();
    if !mode.is_empty() {
        proxies = apply_mode(proxies, &mode);
    }

    let countries: Vec<Option<String>> = if let Some(bin) = engine_bin_of(&state) {
        engine::detect_outbound_country(&proxies, &bin, timeout_ms, proxies.len()).await
    } else {
        vec![None; proxies.len()]
    };

    let now = now_ms();

    // Index results by fingerprint once (O(N)) instead of a linear `.find()`

    // per stored proxy (which was O(N²) across thousands of nodes × results).

    let cc_by_fp: std::collections::HashMap<String, Option<String>> = proxies
        .iter()
        .zip(countries.iter())
        .map(|(p, cc)| (p.fingerprint(), cc.clone()))
        .collect();

    {
        let mut guard = state.store.lock_ok();

        for sub in guard.iter_mut() {
            for p in sub.proxies.iter_mut() {
                if let Some(cc) = cc_by_fp.get(&p.fingerprint()) {
                    // Always refresh: a probe run that returned `None` (no

                    // country / detection failed) must clear the stale value

                    // rather than leaving last run's country behind.

                    p.outbound_country = cc.clone();

                    p.last_tested_at = Some(now);
                }
            }
        }
    }

    persist_all(&state);

    Json(
        proxies
            .iter()
            .zip(countries.iter())
            .map(|(p, cc)| {
                serde_json::json!({

                    "name": p.name,

                    "server": p.server,

                    "outbound_country": cc,

                })
            })
            .collect(),
    )
}

/// Composite node score in [0, 100].
///
/// Thin wrapper over the single source of truth in `subhub_core::score_proxy`
/// so the score column shown by `list_proxies`, the Top-N selection in
/// `merge_export` / `sub_export` and the `sort.key == "score"` branch of
/// `ops::apply` can never disagree on ordering.
pub(crate) fn score_proxy(p: &Proxy) -> f64 {
    subhub_core::score_proxy(p)
}

pub(crate) async fn list_proxies(
    State(state): State<AppState>,

    Query(q): Query<ListQuery>,
) -> Json<ProxiesResp> {
    let guard = state.store.lock_ok();

    // sub-store model: every node belongs to a subscription, so we surface the

    // owning subscription's id + name alongside the node (used by the WebUI's

    // "group by subscription" view).

    let mut all: Vec<serde_json::Value> = Vec::new();

    for sub in guard.iter() {
        for p in &sub.proxies {
            if let Some(t) = &q.r#type {
                if p.type_.as_str() != t {
                    continue;
                }
            }

            if let Some(r) = &q.region {
                if !p.region().eq_ignore_ascii_case(r) {
                    continue;
                }
            }

            if let Some(s) = &q.q {
                let s: String = s.chars().take(256).collect::<String>().to_lowercase();
                // Quick keyword search across the most useful node fields:
                // name, server/host, region, and type. Case-insensitive.
                let matched = p.name.to_lowercase().contains(&s)
                    || p.server.to_lowercase().contains(&s)
                    || p.region().to_lowercase().contains(&s)
                    || p.type_.as_str().to_lowercase().contains(&s);

                if !matched {
                    continue;
                }
            }

            let mut v = serde_json::to_value(p).unwrap_or(serde_json::Value::Null);

            if let Some(obj) = v.as_object_mut() {
                obj.insert("sub_id".into(), serde_json::Value::String(sub.id.clone()));

                obj.insert(
                    "sub_name".into(),
                    serde_json::Value::String(sub.name.clone()),
                );

                // Stable per-node identity (`Proxy::fingerprint`) so the WebUI
                // can target a single node for deletion without relying on a
                // name that the user might rename.
                obj.insert(
                    "fingerprint".into(),
                    serde_json::Value::String(p.fingerprint()),
                );

                // `region` is a computed method on `Proxy`, not a serialized

                // field — surface it here so the WebUI can read `p.region`

                // (otherwise it would always fall back to "OTHER").

                obj.insert("region".into(), serde_json::Value::String(p.region()));

                // Composite score (0-100): lower latency + higher bandwidth +
                // availability + unlock coverage. Attached so the WebUI can sort
                // and filter, and the server can export the top-N.
                obj.insert(
                    "score".into(),
                    serde_json::to_value(score_proxy(p)).unwrap_or(serde_json::Value::Null),
                );
            }

            all.push(v);
        }
    }

    // Global sort applied BEFORE pagination so the first page always reflects
    // the chosen order across ALL nodes — not just the current page's 50.
    // Availability dominates: a dead node (available === false) always sinks to
    // the bottom regardless of the chosen column (mirrors the old client-side
    // rule, now done server-side on the full dataset).
    let sort_key = q.sort.clone().unwrap_or_else(|| "name".to_string());
    let sort_desc = q.desc.unwrap_or(false);
    all.sort_by(|a, b| {
        let ad = a.get("available").and_then(|x| x.as_bool()) == Some(false);
        let bd = b.get("available").and_then(|x| x.as_bool()) == Some(false);
        if ad != bd {
            return if ad {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            };
        }
        let ord = match sort_key.as_str() {
            "score" => {
                let av = a.get("score").and_then(|x| x.as_f64()).unwrap_or(f64::MIN);
                let bv = b.get("score").and_then(|x| x.as_f64()).unwrap_or(f64::MIN);
                av.partial_cmp(&bv).unwrap_or(std::cmp::Ordering::Equal)
            }
            "speed" => {
                let av = a
                    .get("download_speed_bps")
                    .and_then(|x| x.as_f64())
                    .unwrap_or(f64::MIN);
                let bv = b
                    .get("download_speed_bps")
                    .and_then(|x| x.as_f64())
                    .unwrap_or(f64::MIN);
                av.partial_cmp(&bv).unwrap_or(std::cmp::Ordering::Equal)
            }
            "latency" => {
                // untested (null latency) sorts last among available nodes
                let ao = a.get("latency_ms").and_then(|x| x.as_f64());
                let bo = b.get("latency_ms").and_then(|x| x.as_f64());
                match (ao, bo) {
                    (None, None) => std::cmp::Ordering::Equal,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
                }
            }
            "tcplat" => {
                let ao = a.get("tcp_latency_ms").and_then(|x| x.as_f64());
                let bo = b.get("tcp_latency_ms").and_then(|x| x.as_f64());
                match (ao, bo) {
                    (None, None) => std::cmp::Ordering::Equal,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
                }
            }
            "httplat" => {
                let ao = a.get("http_latency_ms").and_then(|x| x.as_f64());
                let bo = b.get("http_latency_ms").and_then(|x| x.as_f64());
                match (ao, bo) {
                    (None, None) => std::cmp::Ordering::Equal,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
                }
            }
            _ => {
                let an = a.get("name").and_then(|x| x.as_str()).unwrap_or("");
                let bn = b.get("name").and_then(|x| x.as_str()).unwrap_or("");
                an.cmp(bn)
            }
        };
        if sort_desc {
            ord.reverse()
        } else {
            ord
        }
    });

    let total = all.len();

    let page = q.page.unwrap_or(1).max(1);

    let page_size = q.page_size.unwrap_or(50).clamp(1, 500);

    let start = (page - 1) * page_size;

    let end = (start + page_size).min(total);

    let items: Vec<serde_json::Value> = if start < total {
        all[start..end].to_vec()
    } else {
        Vec::new()
    };

    Json(ProxiesResp {
        total,

        page,

        page_size,

        items,
    })
}

pub(crate) async fn dashboard(State(state): State<AppState>) -> Json<DashboardResp> {
    let guard = state.store.lock_ok();

    let all = flatten_dedup(&guard, None);

    let mut by_type: HashMap<String, usize> = HashMap::new();

    let mut by_region: HashMap<String, usize> = HashMap::new();

    let mut available = 0usize;

    let mut unavailable = 0usize;

    let mut untested = 0usize;

    let mut lat_sum = 0u64;

    let mut lat_n = 0u64;

    let mut best: Option<u64> = None;

    let mut bw_sum = 0f64;

    let mut bw_n = 0u64;

    let mut best_bw: Option<f64> = None;

    let mut http_lat_sum = 0u64;

    let mut http_lat_n = 0u64;

    let mut best_http: Option<u64> = None;

    for p in &all {
        *by_type.entry(p.type_.as_str().to_string()).or_insert(0) += 1;

        *by_region.entry(p.region()).or_insert(0) += 1;

        match p.available {
            Some(true) => available += 1,

            Some(false) => unavailable += 1,

            None => untested += 1,
        }

        if let Some(ms) = p.latency_ms {
            lat_sum += ms;

            lat_n += 1;

            best = Some(best.map_or(ms, |b| b.min(ms)));
        }

        // HTTP 延迟统计：仅纳入经代理实测到 Google 204 的节点（http_latency_ms 有值）。
        if let Some(ms) = p.http_latency_ms {
            http_lat_sum += ms;

            http_lat_n += 1;

            best_http = Some(best_http.map_or(ms, |b| b.min(ms)));
        }

        // 带宽统计：仅纳入「已实测（真实引擎）或 TCP 估算」且有数值的节点。
        // NaN guard 避免个别脏数据污染聚合结果。
        if let Some(b) = p.download_speed_bps {
            if b.is_finite() && b > 0.0 {
                bw_sum += b;

                bw_n += 1;

                best_bw = Some(best_bw.map_or(b, |x| x.max(b)));
            }
        }
    }

    let per_sub = guard.iter().map(sub_to_summary).collect();

    let avg_latency_ms = lat_sum.checked_div(lat_n);

    let avg_bandwidth_bps = if bw_n > 0 {
        Some(bw_sum / bw_n as f64)
    } else {
        None
    };

    // 除零时 checked_div 返回 None，等价于「无实测节点」。
    let avg_http_latency_ms = http_lat_sum.checked_div(http_lat_n);

    let resp = DashboardResp {
        total: all.len(),

        by_type,

        by_region,

        subscriptions: guard.len(),

        available,

        unavailable,

        untested,

        avg_latency_ms,

        best_latency_ms: best,

        avg_bandwidth_bps,

        best_bandwidth_bps: best_bw,

        avg_http_latency_ms,

        best_http_latency_ms: best_http,

        per_sub,
    };

    // record a rolling trend point — but throttle to at most once per 60s so
    // high-frequency dashboard polling doesn't pollute the trend timeline.
    // (Trend sampling is an event, not a per-render side effect.)

    {
        let now = now_ms();

        let mut h = state.history.lock_ok();

        let recent = h.back().is_some_and(|p| now.saturating_sub(p.t) < 60_000);

        if !recent {
            h.push_back(TrendPoint {
                t: now,

                total: resp.total,

                available: resp.available,

                unavailable: resp.unavailable,

                untested: resp.untested,

                avg_latency_ms: resp.avg_latency_ms,

                best_latency_ms: resp.best_latency_ms,
            });

            while h.len() > 240 {
                h.pop_front();
            }
        }
    }

    Json(resp)
}

/// Trend history (rolling window of dashboard snapshots).
pub(crate) async fn trends(State(state): State<AppState>) -> Json<Vec<TrendPoint>> {
    let h = state.history.lock_ok();

    Json(h.iter().cloned().collect())
}
