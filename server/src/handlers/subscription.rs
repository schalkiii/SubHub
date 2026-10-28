//! 订阅管理相关 HTTP handler：添加 / 导入 / 导出 / 列表 / 重命名 / 刷新 / 删除
//! 订阅，以及删除单个节点。
//!
//! 所有 handler 通过 `AppState.store` 访问订阅集合，变更落库统一走 `persist_all`；
//! 远端订阅的新增节点会经 `run_speedtest_core` 做一次自动测速。
use crate::*;
use crate::{
    client_with_proxy, engine_bin_of, fetch_subscription_text, now_ms, persist_all,
    persist_results, redact_url, run_speedtest_core, sub_name_from_url, sub_to_summary,
    usage_from_sources, LockExt, UsageTuple,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use subhub_core::{parse_subscription, Proxy, Subscription};
pub(crate) async fn health() -> &'static str {
    "ok"
}

/// 把一个「添加订阅」输入框中的条目展开为 0..N 个订阅 URL。
///
/// - 形如 `http(s)://...` 的普通链接：原样返回；
/// - 否则当作 base64 编码的「链接列表」（常见于以 base64 形式分享的订阅合集，
///   每行一个订阅 URL）尝试解码，逐行取出 http(s):// 链接。
pub(crate) fn expand_input_entry(raw: &str) -> Vec<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    // 已经是普通订阅链接（subconverter / v2rayN 大多直接给 URL）。
    if raw.contains("://") {
        return vec![raw.to_string()];
    }
    // 不是 URL：尝试按 base64 解码为「多行 URL 列表」。
    match subhub_core::b64_decode(raw) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            text.lines()
                .map(|l| l.trim())
                .filter(|l| l.starts_with("http://") || l.starts_with("https://"))
                .map(|l| l.to_string())
                .collect()
        }
        Err(_) => Vec::new(),
    }
}

pub(crate) async fn add_subscriptions(
    State(state): State<AppState>,

    Json(req): Json<AddReq>,
) -> Result<Json<CountResp>, (StatusCode, String)> {
    let use_proxy = *state.use_proxy.lock_ok();

    let effective_proxy: Option<String> = if use_proxy {
        match req.fetch_proxy.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(s) => Some(s.to_string()),
            None => {
                let d = state.default_fetch_proxy.lock_ok();
                d.as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string())
            }
        }
    } else {
        None
    };
    let proxy_opt = effective_proxy.as_deref();

    let mut new_subs: Vec<Subscription> = Vec::new();

    // 每个输入框条目可能是一个普通订阅 URL，也可能是一段 base64（解码后为
    // 多行订阅 URL 列表，常见于「以 base64 形式分享的订阅合集」）。自动展开后逐个添加。
    for raw in &req.urls {
        for url in expand_input_entry(raw) {
            let now = now_ms();

            let mut sub = Subscription::new(sub_name_from_url(&url), url.clone(), Vec::new());

            sub.fetch_proxy = effective_proxy.clone();

            sub.health.last_checked_at = Some(now);

            // fetch through the optional upstream proxy when the master switch is on
            let client = match client_with_proxy(proxy_opt) {
                Ok(c) => c,
                Err(e) => {
                    sub.health.last_error = Some(e);
                    new_subs.push(sub);
                    continue;
                }
            };

            match fetch_subscription_text(&client, &url).await {
                Ok((text, userinfo)) => {
                    let proxies = parse_subscription(&text);
                    let (up, dl, tot, exp) = usage_from_sources(&userinfo, &text);
                    let n = proxies.len();
                    sub.proxies = proxies;
                    sub.health.upload = up;
                    sub.health.download = dl;
                    sub.health.total = tot;
                    sub.health.expire = exp;
                    // 0 nodes is treated as a failed fetch: for a re-add it makes
                    // `merge_subscriptions_by_source` keep the previous node list
                    // instead of overwriting it with an empty one.
                    sub.health.last_updated_at = if n == 0 { None } else { Some(now) };
                    sub.health.last_error = if n == 0 {
                        Some("订阅返回 0 个节点".to_string())
                    } else {
                        None
                    };
                    eprintln!("fetched {}: {n} nodes", redact_url(&url));
                    if n == 0 {
                        // Diagnostics: a 200 response that parsed to zero nodes is
                        // almost always an unrecognised body format (e.g. a base64
                        // flavour or a clash key we don't unwrap). Dump the first
                        // 400 bytes so the operator can identify the shape.
                        let snippet: String = text.chars().take(400).collect();
                        eprintln!(
                            "  [subhub] warning: 0 nodes parsed from {}. body prefix ({}B): {:?}",
                            redact_url(&url),
                            snippet.len(),
                            snippet
                        );
                    }
                }
                Err(e) => {
                    sub.health.last_error = Some(e);
                }
            }

            new_subs.push(sub);
        }
    }

    // Remember the pull proxy as the default ONLY after at least one
    // subscription in this batch actually fetched successfully — a
    // syntactically-valid-but-unreachable proxy must not become the remembered
    // default and silently break every future pull.
    if let Some(p) = &effective_proxy {
        if !p.trim().is_empty()
            && new_subs
                .iter()
                .any(|s| s.health.last_error.is_none() && !s.proxies.is_empty())
        {
            *state.default_fetch_proxy.lock_ok() = Some(p.clone());
            if let Some(db) = state.db.as_ref() {
                db.meta_set("default_fetch_proxy", p);
            }
        }
    }

    // Merge into the durable store. `merge_subscriptions_by_source` is the
    // single source of truth for "re-add == refresh" (keyed by `source`, so a
    // duplicate URL updates in place instead of creating a second entry) and
    // returns `fresh_nodes` — the nodes that have never been measured. Only
    // those are tested, so re-adding an existing URL doesn't re-test every
    // node it contains.
    let (total, subscriptions, added, fresh_nodes) = {
        let mut guard = state.store.lock_ok();
        let (_created, added, fresh) =
            subhub_core::ops::merge_subscriptions_by_source(&mut guard, new_subs);
        let total: usize = guard.iter().map(|s| s.proxies.len()).sum();
        (total, guard.len(), added, fresh)
    };

    // Auto health + speedtest on first add. TCP latency always runs; HTTP
    // latency (and any enabled engine probe) uses the engine if set.
    if !fresh_nodes.is_empty() {
        let (tcp, http, bw) = run_speedtest_core(&state, &fresh_nodes, 4000, 30).await;

        persist_results(&state, &fresh_nodes, &tcp, &http, &bw);
    }

    persist_all(&state);

    Ok(Json(CountResp {
        added,

        total,

        subscriptions,
    }))
}

pub(crate) async fn import_raw(
    State(state): State<AppState>,
    Json(req): Json<ImportReq>,
) -> Json<CountResp> {
    let parsed = parse_subscription(&req.content);
    // Drop duplicate nodes *within* a single paste by fingerprint, so a
    // copy-paste of an already-deduplicated list (or a list with repeats)
    // never inflates the subscription with identical entries. Cross-paste
    // idempotency (matching an existing "pasted" sub) is a larger,
    // separate change and is intentionally left for a follow-up.
    let mut seen = std::collections::HashSet::new();
    let proxies: Vec<Proxy> = parsed
        .into_iter()
        .filter(|p| seen.insert(p.fingerprint()))
        .collect();

    let name = req
        .name
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("本地导入 {}", state.store.lock_ok().len() + 1));

    let now = now_ms();

    let mut sub = Subscription::new(name, "pasted".to_string(), proxies);

    sub.health.last_checked_at = Some(now);

    sub.health.last_updated_at = Some(now);

    let new_proxies = sub.proxies.clone();

    let (total, added) = {
        let mut guard = state.store.lock_ok();

        guard.push(sub);

        let total: usize = guard.iter().map(|s| s.proxies.len()).sum();

        let added = guard.last().map(|s| s.proxies.len()).unwrap_or(0);

        (total, added)
    };

    // auto speedtest for the freshly pasted nodes (TCP always; engine if set)

    if !new_proxies.is_empty() {
        let (tcp, http, bw) = run_speedtest_core(&state, &new_proxies, 4000, 30).await;

        persist_results(&state, &new_proxies, &tcp, &http, &bw);
    }

    persist_all(&state);

    Json(CountResp {
        added,

        total,

        subscriptions: state.store.lock_ok().len(),
    })
}

/// GET /api/subscriptions/export — dump the full subscription list (with
/// each subscription's nodes and measured results) as a self-contained JSON
/// document for backup / transfer.
pub(crate) async fn export_subscriptions(State(state): State<AppState>) -> Json<SubExportDoc> {
    let subs = state.store.lock_ok().clone();
    let items: Vec<SubExportItem> = subs
        .into_iter()
        .map(|s| SubExportItem {
            id: s.id.clone(),
            uid: s.uid.clone(),
            name: s.name.clone(),
            source: s.source.clone(),
            source_type: s.health.source_type.clone(),
            fetch_proxy: s.fetch_proxy.clone(),
            health_enabled: s.health.enabled,
            proxies: s.proxies.clone(),
            last_checked_at: s.health.last_checked_at,
            last_updated_at: s.health.last_updated_at,
            last_healthy_at: s.health.last_healthy_at,
            last_error: s.health.last_error.clone(),
            upload: s.health.upload,
            download: s.health.download,
            total: s.health.total,
            expire: s.health.expire,
        })
        .collect();
    Json(SubExportDoc {
        kind: "subhub-subscriptions",
        version: 3,
        exported_at: now_ms(),
        engine_bin: engine_bin_of(&state),
        subscriptions: items,
    })
}

/// POST /api/subscriptions/import — restore subscriptions from an export file.
/// Merges by exported `id` (idempotent re-import into the same instance) and
/// by URL source (cross-instance merge of remote subscriptions); new entries
/// are appended, pasted/local entries with no URL never collide. Embedded
/// node results are preserved (no re-fetch, no re-test).
pub(crate) async fn import_subscriptions(
    State(state): State<AppState>,
    Json(req): Json<SubImportReq>,
) -> Json<SubImportResp> {
    let now = now_ms();
    let (added, replaced) = {
        let mut guard = state.store.lock_ok();
        // Index existing subs for merge by source URL only — a re-import of a
        // remote subscription is idempotent. We deliberately do NOT index by the
        // client-supplied `id`: ids are sequential (`sub_N`) and trusting them
        // would let a caller overwrite an unrelated existing subscription.
        let mut by_url: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (i, s) in guard.iter().enumerate() {
            if (s.source.starts_with("http://") || s.source.starts_with("https://"))
                && !s.source.trim().is_empty()
            {
                by_url.insert(s.source.clone(), i);
            }
        }
        let mut added = 0usize;
        let mut replaced = 0usize;
        for it in &req.subscriptions {
            let is_remote = it.source.starts_with("http://") || it.source.starts_with("https://");
            // Remote subs match by source URL; local subs have no URL, so they
            // match by the stable backup uid (never the sequential id). An old
            // backup without uid falls back to a *unique* local name match.
            let target = if is_remote {
                by_url.get(&it.source).copied()
            } else if !it.uid.trim().is_empty() {
                guard
                    .iter()
                    .position(|s| s.source == "pasted" && s.uid == it.uid)
            } else {
                let matches: Vec<usize> = guard
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.source == "pasted" && s.name == it.name)
                    .map(|(i, _)| i)
                    .collect();
                (matches.len() == 1).then(|| matches[0])
            };
            let mut sub = Subscription::new(it.name.clone(), it.source.clone(), it.proxies.clone());
            sub.fetch_proxy = it.fetch_proxy.clone();
            sub.health.enabled = it.health_enabled;
            sub.health.source_type = if is_remote {
                "remote".to_string()
            } else {
                it.source_type.clone()
            };
            // 新备份携带健康/用量元数据，原样恢复；旧备份缺这些字段时
            // 回退到“刚刚检查/更新”的旧行为。
            sub.health.last_checked_at = it.last_checked_at.or(Some(now));
            sub.health.last_updated_at = it.last_updated_at.or(Some(now));
            sub.health.last_healthy_at = it.last_healthy_at;
            sub.health.last_error = it.last_error.clone();
            sub.health.upload = it.upload;
            sub.health.download = it.download;
            sub.health.total = it.total;
            sub.health.expire = it.expire;
            if let Some(idx) = target {
                // keep the original id/uid so refresh/delete links and later
                // backup restores stay stable.
                sub.id = guard[idx].id.clone();
                let existing_uid = guard[idx].uid.clone();
                sub.uid = if existing_uid.is_empty() {
                    if it.uid.is_empty() {
                        sub.uid.clone()
                    } else {
                        it.uid.clone()
                    }
                } else {
                    existing_uid
                };
                guard[idx] = sub;
                replaced += 1;
            } else {
                // A local restore keeps the backup uid so importing the same
                // backup twice updates instead of appending a duplicate.
                if !is_remote && !it.uid.trim().is_empty() {
                    sub.uid = it.uid.clone();
                }
                guard.push(sub);
                added += 1;
            }
        }
        (added, replaced)
    };

    let (total, subscriptions) = {
        let guard = state.store.lock_ok();
        let total: usize = guard.iter().map(|s| s.proxies.len()).sum();
        (total, guard.len())
    };

    persist_all(&state);

    Json(SubImportResp {
        added,
        replaced,
        total,
        subscriptions,
    })
}

pub(crate) async fn list_subscriptions(
    State(state): State<AppState>,
    Query(q): Query<SubListQuery>,
) -> Json<Vec<SubSummary>> {
    let guard = state.store.lock_ok();

    let mut out: Vec<SubSummary> = guard.iter().map(sub_to_summary).collect();

    if let Some(kw) = q.q.filter(|s| !s.trim().is_empty()) {
        // Cap the search term (defense-in-depth) so an ultra-long string cannot
        // blow up the substring scan. No correctness impact on normal queries.
        let kw: String = kw.chars().take(256).collect::<String>().to_lowercase();
        out.retain(|s| {
            s.name.to_lowercase().contains(&kw) || s.source.to_lowercase().contains(&kw)
        });
    }

    Json(out)
}

/// Return the raw clash-meta YAML for a single node (looked up by fingerprint
/// across all subscriptions). `yaml` is `null` when no matching node exists.
pub(crate) async fn proxy_yaml(
    State(state): State<AppState>,
    Query(q): Query<SingleFp>,
) -> Json<serde_json::Value> {
    let guard = state.store.lock_ok();
    for sub in guard.iter() {
        for p in &sub.proxies {
            if p.fingerprint() == q.fp {
                let yaml = subhub_core::to_clash_meta(std::slice::from_ref(p));
                return Json(serde_json::json!({ "yaml": yaml, "name": p.name }));
            }
        }
    }
    Json(serde_json::json!({ "yaml": null, "name": null }))
}

/// Delete one node from its owning subscription. Unlike `delete_subscription`
/// (which drops the whole group), this removes just the targeted proxy and
/// recomputes that subscription's derived health. Returns 404 when the
/// subscription or the node (by fingerprint) is not found.
pub(crate) async fn delete_proxy(
    State(state): State<AppState>,
    Json(req): Json<DeleteProxyReq>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut found = false;
    let mut sub_name = String::new();
    {
        let mut guard = state.store.lock_ok();
        if let Some(sub) = guard.iter_mut().find(|s| s.id == req.sub_id) {
            let before = sub.proxies.len();
            sub.proxies.retain(|p| p.fingerprint() != req.fingerprint);
            found = sub.proxies.len() != before;
            if found {
                sub.health.recompute(&sub.proxies);
                sub_name = sub.name.clone();
            }
        }
    }
    if found {
        persist_all(&state);
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "ok",
                "removed": 1,
                "sub_id": req.sub_id,
                "sub_name": sub_name,
            })),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "status": "not_found",
                "sub_id": req.sub_id,
            })),
        )
    }
}

pub(crate) async fn delete_subscription(
    State(state): State<AppState>,

    Path(id): Path<String>,
) -> (StatusCode, Json<CountResp>) {
    let (total, subscriptions, found) = {
        let mut guard = state.store.lock_ok();

        let before = guard.len();

        guard.retain(|s| s.id != id);

        let found = guard.len() != before;

        let total: usize = guard.iter().map(|s| s.proxies.len()).sum();

        let subscriptions = guard.len();

        (total, subscriptions, found)
    };

    persist_all(&state);

    // Return 404 when the id matched no subscription so callers can tell a real
    // deletion from a silent no-op (e.g. a stale or already-deleted id).
    let status = if found {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };

    (
        status,
        Json(CountResp {
            added: 0,

            total,

            subscriptions,
        }),
    )
}

/// Rename a single subscription. Accepts a non-empty `name`; blanks are
/// rejected so a subscription never ends up with an empty display name.
/// Returns 404 when the id matches nothing.
pub(crate) async fn rename_subscription(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<RenameSubReq>,
) -> (StatusCode, Json<serde_json::Value>) {
    let name = req.name.trim();

    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "status": "empty_name" })),
        );
    }

    // 限制长度，避免异常超长 / 仅由控制字符构成的非空名写入（虽仅作展示，
    // 但仍防止存储与导出时被滥用）。与 set_settings 的字段上限保持一致量级。
    if name.len() > 256 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "status": "name_too_long" })),
        );
    }

    let mut found = false;
    {
        let mut guard = state.store.lock_ok();
        if let Some(sub) = guard.iter_mut().find(|s| s.id == id) {
            sub.name = name.to_string();
            found = true;
        }
    }

    if found {
        persist_all(&state);
        (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "ok", "id": id, "name": name })),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "status": "not_found", "id": id })),
        )
    }
}

/// Refresh a remote subscription: re-fetch it and update its nodes + health.
/// Local (pasted) subscriptions just get re-stamped. `None` is returned when
/// the subscription id is unknown (so callers can 404).
/// This is the shared core used by both `POST /api/subscriptions/:id/refresh`
/// and the background auto-refresh scheduler. No `MutexGuard` is held across
/// `.await` —we snapshot under the lock, drop it, await the fetch, then
/// re-lock to persist. The upstream proxy is only used when the global
/// `use_proxy` master switch is on.
pub(crate) async fn do_refresh_one(state: &AppState, id: &str) -> Option<serde_json::Value> {
    let now = now_ms();

    let target = {
        let guard = state.store.lock_ok();

        guard.iter().find(|s| s.id == id).map(|s| {
            (
                s.source.clone(),
                s.health.source_type.clone(),
                s.fetch_proxy.clone(),
            )
        })
    };

    let (url, source_type, fetch_proxy) = target?;

    let use_proxy = *state.use_proxy.lock_ok();

    let proxy_opt = if use_proxy {
        fetch_proxy.as_deref().filter(|s| !s.trim().is_empty())
    } else {
        None
    };

    let client = match client_with_proxy(proxy_opt) {
        Ok(c) => c,

        Err(e) => {
            let msg = e.clone();

            let mut guard = state.store.lock_ok();

            if let Some(sub) = guard.iter_mut().find(|s| s.id == id) {
                sub.health.last_checked_at = Some(now);

                sub.health.last_error = Some(e);
            }

            return Some(serde_json::json!({ "status": "error", "error": msg }));
        }
    };

    let mut fetched: Option<Vec<Proxy>> = None;

    let mut fetch_err: Option<String> = None;

    // resolved traffic usage from the fetch, applied in Phase 2
    let mut usage_hint: Option<UsageTuple> = None;

    if source_type == "remote" {
        match fetch_subscription_text(&client, &url).await {
            Ok((text, userinfo)) => {
                let proxies = parse_subscription(&text);
                usage_hint = Some(usage_from_sources(&userinfo, &text));
                if proxies.is_empty() {
                    let snippet: String = text.chars().take(400).collect();
                    eprintln!(
                        "  [subhub] warning: refresh of {} parsed 0 nodes. body prefix ({}B): {:?}",
                        redact_url(&url),
                        snippet.len(),
                        snippet
                    );
                    // A 200 response that parses to zero nodes is treated as a
                    // failed refresh: replacing the node list with an empty one
                    // would silently wipe a working subscription on a transient
                    // upstream glitch / format change.
                    fetch_err = Some("订阅返回 0 个节点（已保留原有节点）".to_string());
                } else {
                    fetched = Some(proxies);
                }
            }

            Err(e) => fetch_err = Some(e),
        }
    }

    // Phase 2: re-lock to persist (no guard held across .await above).

    // BestSub-style incremental merge: surviving nodes keep their previously

    // measured health, only the genuinely new nodes are appended, and nodes

    // removed upstream are dropped. `new_nodes` is the subset that still needs

    // a speed test.

    let mut new_nodes: Vec<Proxy> = Vec::new();

    let result = {
        let mut guard = state.store.lock_ok();

        let sub = match guard.iter_mut().find(|s| s.id == id) {
            Some(s) => s,

            None => return None,
        };

        sub.health.last_checked_at = Some(now);

        if let Some(proxies) = fetched {
            let (merged, added) = incremental_update(&sub.proxies, &proxies);

            let total = merged.len();

            new_nodes = added;

            sub.proxies = merged;

            if let Some((up, dl, tot, exp)) = usage_hint {
                sub.health.upload = up;
                sub.health.download = dl;
                sub.health.total = tot;
                sub.health.expire = exp;
            }

            sub.health.last_updated_at = Some(now);

            sub.health.last_error = None;

            serde_json::json!({ "status": "ok", "nodes": total, "new_nodes": new_nodes.len(), "source": url })
        } else if source_type == "local" {
            sub.health.last_updated_at = Some(now);

            serde_json::json!({ "status": "ok", "nodes": sub.proxies.len(), "source": "local" })
        } else {
            sub.health.last_error = fetch_err;

            serde_json::json!({ "status": "error", "error": sub.health.last_error })
        }
    };

    // auto speedtest: only the genuinely new nodes (surviving nodes keep their

    // previously measured latency/availability, so we don't re-test everything

    // on every refresh — BestSub incremental-update behaviour).

    if source_type == "remote" && !new_nodes.is_empty() {
        let (tcp, http, bw) = run_speedtest_core(state, &new_nodes, 4000, 30).await;

        persist_results(state, &new_nodes, &tcp, &http, &bw);
    }

    Some(result)
}

pub(crate) async fn refresh_subscription(
    State(state): State<AppState>,

    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    match do_refresh_one(&state, &id).await {
        Some(v) => {
            persist_all(&state);
            Ok(Json(v))
        }
        // 与删除 / 单订阅测速保持一致：不存在的 id 返回 404，而不是
        // 200 + status:error 让调用方误以为请求成功。
        None => Err((StatusCode::NOT_FOUND, "subscription not found".into())),
    }
}
