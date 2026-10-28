//! 维护类 handler：清理无效 / 连续失败的节点、淘汰长期无可用节点的订阅、
//! Top-N 聚合导出，以及订阅内容的构建（过滤 / 排序 / 重命名 / 格式化）。
use crate::*;
use crate::{flatten_dedup, now_ms, persist_all, resolve_top_n, transform_from_sub_query, LockExt};
use axum::{
    extract::{Query, State},
    http::{header, HeaderValue, StatusCode},
    Json,
};
use subhub_core::{apply_transform, export_str, region_fixup_for_export};
/// Resin circuit-breaker style cleanup: drop nodes that have been tested and
/// found unavailable (`available == Some(false)`) across all subscriptions.
/// Returns how many were removed. Keeps `untested` (None) nodes untouched.
pub(crate) async fn cleanup_bad(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut removed = 0usize;

    {
        let mut guard = state.store.lock_ok();

        for sub in guard.iter_mut() {
            let before = sub.proxies.len();

            sub.proxies.retain(|p| p.available != Some(false));

            let gone = before - sub.proxies.len();
            removed += gone;
            // 派生计数（node_count / healthy / avg / best）必须随节点列表一起更新：
            // 否则紧随其后的 persist_all 会把过期计数写进库，重启后所有直接读
            // 持久化健康数的口径都是旧的（与 delete_proxy 的行为也不一致）。
            if gone > 0 {
                sub.health.recompute(&sub.proxies);
            }
        }
    }

    persist_all(&state);

    Json(serde_json::json!({ "status": "ok", "removed": removed }))
}

/// 删除「连续 N 天没有任何可用节点」的订阅。N 取自 `remove_sub_after_days`（0 = 禁用）。
/// 判断规则：
/// - 订阅当前仍有任一可用节点（`available == Some(true)`）→ 保留；
/// - 否则以「最后有可用节点的时间」为基准（从未健康过则回退到最后检查时间），
///   距现在已超过 N 天则从 store 移除。
///
/// 返回被删除的订阅数量。
pub(crate) fn prune_dead_subscriptions(state: &AppState, now: u64) -> usize {
    let days = *state.remove_sub_after_days.lock_ok();
    if days == 0 {
        return 0;
    }
    // 把「天」换算成毫秒，注意 u64 溢出保护（与 auto-refresh 思路一致）。
    let cutoff_ms = days.saturating_mul(24 * 60 * 60 * 1000);
    let removed = {
        let mut guard = state.store.lock_ok();
        let before = guard.len();
        guard.retain(|sub| {
            // 当前仍有可用节点：保留。
            if sub.proxies.iter().any(|p| p.available == Some(true)) {
                return true;
            }
            // 否则以「最后健康时间」为基准；从未健康过用最后检查时间兜底。
            let ref_time = sub
                .health
                .last_healthy_at
                .or(sub.health.last_checked_at)
                .or(sub.health.last_updated_at);
            match ref_time {
                // 距参考时间已达阈值 → 删除；否则保留（连续无可用未满 N 天）。
                Some(t) => now.saturating_sub(t) < cutoff_ms,
                // 没有任何时间信息，保守保留。
                None => true,
            }
        });
        before - guard.len()
    };
    if removed > 0 {
        persist_all(state);
    }
    removed
}

/// POST /api/subscriptions/prune — 立即清理「连续 N 天无可用节点」的订阅。
pub(crate) async fn prune_subscriptions(State(state): State<AppState>) -> Json<serde_json::Value> {
    let removed = prune_dead_subscriptions(&state, now_ms());
    Json(serde_json::json!({ "status": "ok", "removed": removed }))
}

/// Return the top-N nodes by composite score as JSON. `n` comes from the query
/// parameter and falls back to the configured `top_n` setting (0 = all nodes).
/// Each item carries the same fields as /api/proxies plus a `score`.
pub(crate) async fn nodes_top(
    State(state): State<AppState>,
    Query(q): Query<TopNQuery>,
) -> Json<Vec<serde_json::Value>> {
    struct TopRow<'a> {
        p: &'a Proxy,
        sub_id: &'a str,
        sub_name: &'a str,
        score: f64,
        idx: usize,
    }

    let guard = state.store.lock_ok();
    let mut rows: Vec<TopRow> = Vec::new();
    let mut idx = 0usize;
    for sub in guard.iter() {
        for p in &sub.proxies {
            // Top-N 列表只展示可导出且未判死的节点，与导出/订阅口径一致。
            if p.is_usable() {
                rows.push(TopRow {
                    p,
                    sub_id: &sub.id,
                    sub_name: &sub.name,
                    score: score_proxy(p),
                    idx,
                });
                idx += 1;
            }
        }
    }
    // 客户端传来的 n 与设置项同样受 sanity 上限约束（0 = 全部）。
    let n = q.n.unwrap_or(*state.top_n.lock_ok()).min(100_000);
    // 排序走类型化的分数，而不是把每个节点先渲染成 JSON、再按字符串键查
    // `score` 比较；`idx` 兜底保证同分时仍是原来的插入顺序（稳定语义）。
    rows.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.idx.cmp(&b.idx))
    });
    let take = if n == 0 { rows.len() } else { n as usize };
    // 只序列化真正返回的这 take 条。
    let items: Vec<serde_json::Value> = rows
        .into_iter()
        .take(take)
        .map(|r| {
            let mut v = serde_json::to_value(r.p).unwrap_or(serde_json::Value::Null);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("sub_id".into(), serde_json::Value::String(r.sub_id.into()));
                obj.insert(
                    "sub_name".into(),
                    serde_json::Value::String(r.sub_name.into()),
                );
                obj.insert(
                    "region".into(),
                    serde_json::Value::String(r.p.effective_region()),
                );
                obj.insert(
                    "score".into(),
                    serde_json::to_value(r.score).unwrap_or(serde_json::Value::Null),
                );
            }
            v
        })
        .collect();
    Json(items)
}

pub(crate) async fn merge_export(
    State(state): State<AppState>,

    Json(req): Json<ExportReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let guard = state.store.lock_ok();

    let fmt = req.format.unwrap_or_else(|| "clash-meta".to_string());

    let base = flatten_dedup(&guard, req.sub_ids.as_deref());

    let transformed = match &req.transform {
        Some(t) => apply_transform(&base, t).map_err(|e| (StatusCode::BAD_REQUEST, e))?,

        None => base,
    };

    // Top-N filter: a `top_n` in the request body overrides the global
    // `top_n` setting; when neither specifies a positive N we keep everything.
    // This is what makes manual export honour the same single "Top-N"
    // configuration as the standing subscribe URL (/sub) — configured once in
    // the Settings page, applied everywhere.
    let global_top = *state.top_n.lock_ok();
    let top_n = resolve_top_n(req.top_n, global_top);
    let mut transformed = if let Some(n) = top_n {
        // Top-N 只从「可导出且未判死」的节点里选，避免死节点/未知类型
        // 占用名额后又被 export_filter 丢掉，导致最终不足 N 个。
        subhub_core::top_n_by_score(&transformed, usize::try_from(n).unwrap_or(usize::MAX))
    } else {
        transformed
    };

    // 导出前地区名修正（与 /sub 一致）：名字 OTHER 但实测出口地区已知的节点前置国家码，
    // 避免 Clash Verge 把它们归为「其他」。仅改导出副本，不影响落库数据。
    region_fixup_for_export(&mut transformed);

    let content = export_str(&transformed, &fmt);

    Ok(Json(serde_json::json!({

        "format": fmt,

        // report the count actually exported (invalid nodes are dropped)

        "count": subhub_core::export_filter(&transformed).len(),

        "content": content

    })))
}

/// Direct-pull local subscription endpoint. Merges every (or a subset of)
/// subscription, applies the optional URL-encoded transform, and returns the
/// resulting subscription as **raw text** with a client-friendly content type, so a proxy client (mihomo / clash / v2rayN / sing-box) can subscribe to
/// `http://127.0.0.1:3005/sub` directly. No JSON wrapper, no auth.
pub(crate) async fn sub_export(
    State(state): State<AppState>,

    Query(q): Query<SubQuery>,
) -> Result<impl axum::response::IntoResponse, (StatusCode, String)> {
    let (content, ct) = build_sub_content(&state, &q)?;

    Ok((
        [(header::CONTENT_TYPE, HeaderValue::from_static(ct))],
        content,
    ))
}

/// 根据订阅查询参数（格式 / 筛选 / 排序 / 重命名 / Top-N / 质量阈值）生成订阅文本
/// 与其 Content-Type。被 `GET /sub`（本地 / 局域网拉取）与 `POST /api/gist/upload`
/// （上传到 GitHub Gist 供远程设备拉取）共用，保证两种分发渠道产物完全一致。
pub(crate) fn build_sub_content(
    state: &AppState,
    q: &SubQuery,
) -> Result<(String, &'static str), (StatusCode, String)> {
    let only: Option<Vec<String>> = q.sub.as_ref().map(|s| {
        s.split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()
    });

    let base = {
        let guard = state.store.lock_ok();
        flatten_dedup(&guard, only.as_deref())
    };

    let t = transform_from_sub_query(q);

    let transformed = match &t {
        Some(t) => apply_transform(&base, t).map_err(|e| (StatusCode::BAD_REQUEST, e))?,
        None => base,
    };

    // Top-N trim on the standing subscription URL. A `top_n` in the URL
    // (if present and positive) overrides the global `top_n` setting; this is
    // what makes the standing subscription honour the same single Top-N
    // configuration as the manual export — both driven by the Settings page.
    let url_top = q.top_n.as_ref().and_then(|s| s.trim().parse::<u64>().ok());
    let global_top = *state.top_n.lock_ok();
    let top_n = resolve_top_n(url_top, global_top);
    let mut transformed = if let Some(n) = top_n {
        // 同上：Top-N 名额只分配给可导出且未判死的节点。
        subhub_core::top_n_by_score(&transformed, usize::try_from(n).unwrap_or(usize::MAX))
    } else {
        transformed
    };

    let fmt = q.format.clone().unwrap_or_else(|| "clash-meta".to_string());

    // 导出前地区名修正：名字未识别地区（region==OTHER）但实测出口地区已知的节点，
    // 把出口国家码前置到名字，避免 Clash Verge 按名字匹配时把它们归为「其他」。
    // 仅改导出副本，不影响落库数据。
    region_fixup_for_export(&mut transformed);

    let content = export_str(&transformed, &fmt);

    // JSON-ish formats get application/json; everything else is plain text
    // (clients parse by content, not by content-type, so text/plain is safe).
    let ct: &'static str = match fmt.as_str() {
        "v2ray" | "sing-box" | "singbox" => "application/json; charset=utf-8",
        _ => "text/plain; charset=utf-8",
    };

    Ok((content, ct))
}
