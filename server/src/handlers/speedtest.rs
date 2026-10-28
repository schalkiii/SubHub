//! 测速相关 handler：`POST /api/speedtest`（SSE 流式推送进度）、上游代理连通性
//! 自检，以及把聚合结果同步到 GitHub Gist。
use crate::*;
use crate::{
    apply_mode, client_with_proxy, engine_bin_of, flatten_dedup, is_loopback_server, persist_all,
    persist_results, redact_url, run_engine_passes, LockExt,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::Event,
    Json,
};
use std::sync::Arc;
use subhub_core::Proxy;
pub(crate) async fn gist_upload(
    State(state): State<AppState>,
    Json(req): Json<GistUploadReq>,
) -> Result<Json<GistUploadResp>, (StatusCode, String)> {
    // 串行化上传：避免并发上传各自用旧 gist_id 创建出多个孤儿 Gist，
    // 或让「稳定的远程拉取地址」在新旧 id 之间漂移。
    let _g = state.gist_lock.lock().await;
    // 1) 与 /sub 完全相同的产物，保证两种分发渠道一致。
    let q = SubQuery {
        format: req.format.clone(),
        sub: req.sub.clone(),
        sort: req.sort.clone(),
        desc: req.desc.clone(),
        rename_pat: req.rename_pat.clone(),
        rename_rep: req.rename_rep.clone(),
        q: req.q.clone(),
        region: req.region.clone(),
        r#type: req.r#type.clone(),
        top_n: req.top_n.clone(),
        min_bw: req.min_bw.clone(),
        max_lat: req.max_lat.clone(),
    };
    let fmt = req
        .format
        .clone()
        .unwrap_or_else(|| "clash-meta".to_string());
    let (content, _ct) = build_sub_content(&state, &q)?;
    let file_name = gist_file_name(&fmt);

    // 2) 解析 GitHub 凭据（请求体 > 持久化设置）。
    let user = req
        .github_user
        .clone()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            state
                .github_user
                .lock_ok()
                .clone()
                .filter(|v| !v.trim().is_empty())
        })
        .ok_or((
            StatusCode::BAD_REQUEST,
            "未配置 GitHub 用户名（请在「设置」页填写）".into(),
        ))?;
    let token = req
        .github_token
        .clone()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            state
                .github_token
                .lock_ok()
                .clone()
                .filter(|v| !v.trim().is_empty())
        })
        .ok_or((
            StatusCode::BAD_REQUEST,
            "未配置 GitHub token（请在「设置」页填写）".into(),
        ))?;

    // 3) 创建 / 更新 Gist（沿用 state.gist_id；若 PATCH 返回 404 说明 Gist 已被删，自动重建）。
    // GitHub 在部分网络不可直连：开启全局拉取代理时顺带走该代理；
    // 未配置代理时保持系统默认行为（尊重 HTTP(S)_PROXY）。
    let proxy = if *state.use_proxy.lock_ok() {
        state.default_fetch_proxy.lock_ok().clone()
    } else {
        None
    };
    let mut client_builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .user_agent("subhub");
    if let Some(p) = proxy.as_deref().filter(|s| !s.trim().is_empty()) {
        let parsed = reqwest::Proxy::all(p).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("拉取代理无效 {}: {e}", redact_url(p)),
            )
        })?;
        client_builder = client_builder.proxy(parsed);
    }
    let client = client_builder.build().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("创建 HTTP 客户端失败：{e}"),
        )
    })?;
    let mut files = serde_json::Map::new();
    files.insert(
        file_name.to_string(),
        serde_json::json!({ "content": content }),
    );
    let body = serde_json::json!({
        "description": "SubHub subscription",
        "public": true,
        "files": files
    });

    let mut id_opt = state.gist_id.lock_ok().clone();
    let (final_id, html_url, owner) = loop {
        let (path, is_patch) = match &id_opt {
            Some(id) => (format!("https://api.github.com/gists/{id}"), true),
            None => ("https://api.github.com/gists".to_string(), false),
        };
        let builder = if is_patch {
            client.patch(path.as_str())
        } else {
            client.post(path.as_str())
        };
        let resp = builder
            .bearer_auth(token.as_str())
            .header("User-Agent", "subhub")
            .header("Accept", "application/vnd.github+json")
            .json(&body)
            .send()
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, format!("GitHub 请求失败：{e}")))?;
        let status = resp.status();
        if !status.is_success() {
            // 复用旧 id 但 Gist 已被删除 → 放弃旧 id 重新创建
            if status == StatusCode::NOT_FOUND && id_opt.is_some() {
                id_opt = None;
                continue;
            }
            let msg = resp.text().await.unwrap_or_default();
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("GitHub 上传失败 (HTTP {status}): {msg}"),
            ));
        }
        let j: serde_json::Value = resp.json().await.map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("解析 GitHub 响应失败：{e}"),
            )
        })?;
        let id = j
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let hu = j
            .get("html_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // raw 地址必须使用 Gist 实际所有者；配置里的用户名可能和 token
        // 所属账号不同（多账号 / 组织 token）。
        let owner = j
            .get("owner")
            .and_then(|o| o.get("login"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        break (id, hu, owner);
    };

    // 4) 持久化 gist_id，保证下次更新同一 Gist。
    *state.gist_id.lock_ok() = Some(final_id.clone());
    if let Some(db) = &state.db {
        db.meta_set("gist_id", &final_id);
    }

    // 带上与格式匹配的文件名（yaml/json/conf/txt），让 GitHub 直接以正确
    // 扩展名返回，避免部分移动端客户端无法从裸 URL 识别格式而拉到空订阅。
    let owner = owner.unwrap_or(user);
    let raw_url = format!("https://gist.githubusercontent.com/{owner}/{final_id}/raw/{file_name}");
    Ok(Json(GistUploadResp {
        ok: true,
        url: raw_url,
        gist_id: final_id,
        html_url,
    }))
}

pub(crate) async fn speedtest(
    State(state): State<AppState>,

    Query(req): Query<SpeedTestReq>,
) -> Sse<impl stream::Stream<Item = Result<Event, Infallible>>> {
    let timeout_ms = req.timeout_ms.unwrap_or(4000);

    let concurrency = req.concurrency.unwrap_or(20);

    // snapshot proxies (release lock before the (blocking) test)
    let proxies: Vec<Proxy> = {
        let guard = state.store.lock_ok();
        flatten_dedup(&guard, None)
    };
    let mode = req.mode.clone().unwrap_or_default();
    let proxies = if mode.is_empty() {
        proxies
    } else {
        apply_mode(proxies, &mode)
    };

    // 可选「选中节点」过滤（WebUI 的多选测速）：只保留列出的 fingerprint。
    // `ids` 是 URL 编码的 JSON 数组字符串，按 JSON 解析，避免 fingerprint 内
    // 逗号（常见于 vless+ws 的 path）被分隔符误切导致静默匹配不到。
    let selected: std::collections::HashSet<String> = req
        .ids
        .as_deref()
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
        .map(|v| {
            v.into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let proxies = if selected.is_empty() {
        proxies
    } else {
        proxies
            .into_iter()
            .filter(|p| selected.contains(&p.fingerprint()))
            .collect()
    };

    run_speedtest_sse(state, proxies, timeout_ms, concurrency).await
}

/// Progress units for one SSE run: one per node for TCP, plus one per node for
/// the engine HTTP-latency pass when an engine is configured. Bandwidth is not
/// counted because it is disabled (see `run_engine_passes`).
pub(crate) fn sse_unit_total(node_count: usize, has_engine: bool) -> usize {
    node_count * if has_engine { 2 } else { 1 }
}

/// Shared SSE speedtest core: runs TCP ping (with live progress) then, when an
/// engine binary is configured, engine HTTP latency + bandwidth, persists the
/// results, and emits a final `Done` summary. Used by both the global
/// `/api/speedtest` endpoint and the per-subscription
/// `/api/subscriptions/:id/speedtest` endpoint so a single subscription can be
/// (re)tested in isolation.
pub(crate) async fn run_speedtest_sse(
    state: AppState,
    proxies: Vec<Proxy>,
    timeout_ms: u64,
    concurrency: usize,
) -> Sse<impl stream::Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<SpeedEvent>();

    tokio::spawn(async move {
        // Total progress units span the stages that actually run: TCP latency
        // and, when an engine is configured, engine HTTP latency. Bandwidth
        // probing is deliberately disabled (see run_engine_passes), so counting
        // a third unit per node would leave the bar stuck at 2/3 forever.
        let node_count = proxies.len() as u64;
        let has_engine = engine_bin_of(&state)
            .map(|b| std::path::Path::new(&b).exists())
            .unwrap_or(false);
        let unit_total = sse_unit_total(node_count as usize, has_engine);

        // TCP phase with a live-progress callback (runs in worker threads).
        let proxies_tcp = proxies.clone();
        let tx_tcp = tx.clone();
        let tcp_results = tokio::task::spawn_blocking(move || {
            let cb = move |p: subhub_core::speedtest::TestProgress| {
                // 回环节点（127.*/localhost/::1 等）的「带宽」只是往本地 socket
                // 灌数据的吞吐，是内存拷贝而非真实代理速度，实时进度里直接置空，
                // 避免把 6000+ MB/s 的假值展示给用户。
                let bandwidth_bps = if is_loopback_server(&p.server) {
                    None
                } else {
                    p.bandwidth_bps
                };
                let _ = tx_tcp.send(SpeedEvent::Progress(ProgressView {
                    done: p.done,
                    total: unit_total,
                    name: p.name,
                    fingerprint: Some(p.fingerprint.clone()),
                    available: p.available,
                    latency_ms: p.latency_ms,
                    tcp_latency_ms: p.latency_ms,
                    http_latency_ms: None,
                    bandwidth_bps,
                    server: p.server,
                    phase: "tcp".to_string(),
                }));
            };
            subhub_core::tcp_ping_all(&proxies_tcp, timeout_ms, concurrency, Some(&cb))
        })
        .await
        .unwrap_or_default();

        // Engine phase (real HTTP latency) when an engine is set. Each node
        // fires a progress tick so the bar keeps advancing through this stage.
        let (http, bw) = {
            let tx_http = tx.clone();
            let http_counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let http_off = node_count as usize;
            let ut = unit_total;
            let on_http: Option<Arc<dyn Fn(engine::EngineProgress) + Send + Sync>> = if has_engine {
                let tx = tx_http.clone();
                let c = http_counter.clone();
                Some(Arc::new(move |prog: engine::EngineProgress| {
                    let d = c.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    // server 由引擎回调直接携带，避免跳过不可导出节点后
                    // 用计数器索引输入列表导致 host 错位。
                    let server = prog.server;
                    // 引擎 HTTP 探测结果：Some(Some(ms)) = 探测成功（节点可用），
                    // Some(None) = 探测失败（节点不可用）。据此实时设置 available，
                    // 让前端在引擎阶段就能看到节点逐个亮起/变红。
                    let http_ok = prog.http_latency.flatten().is_some();
                    let http_lat = prog.http_latency.flatten();
                    let _ = tx.send(SpeedEvent::Progress(ProgressView {
                        done: http_off + d as usize,
                        total: ut,
                        name: prog.name,
                        fingerprint: Some(prog.fingerprint.clone()),
                        available: http_ok,
                        latency_ms: http_lat,
                        tcp_latency_ms: None,
                        http_latency_ms: http_lat,
                        bandwidth_bps: None,
                        server,
                        phase: "http".to_string(),
                    }));
                }))
            } else {
                None
            };
            run_engine_passes(&state, &proxies, timeout_ms, concurrency, on_http, None).await
        };

        let removed = persist_results(&state, &proxies, &tcp_results, &http, &bw);
        persist_all(&state);

        // Aggregate a summary for the final `done` event.
        let tested = tcp_results.len();
        let reachable = tcp_results.iter().filter(|r| r.available).count();
        let with_http = http
            .iter()
            .filter(|x| x.as_ref().is_some_and(|i| i.is_some()))
            .count();
        let latencies: Vec<u64> = tcp_results
            .iter()
            .filter_map(|r| r.tcp_latency_ms)
            .collect();
        let avg_latency_ms = if latencies.is_empty() {
            None
        } else {
            Some(latencies.iter().sum::<u64>() / latencies.len() as u64)
        };
        let threshold = *state.remove_after_fails.lock_ok();
        let _ = tx.send(SpeedEvent::Done(SummaryView {
            tested,
            reachable,
            avg_latency_ms,
            with_http,
            removed,
            threshold,
        }));
    });

    Sse::new(stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Some(ev) => {
                let data = serde_json::to_string(&ev).unwrap_or_default();
                Some((Ok(Event::default().data(data)), rx))
            }
            None => None,
        }
    }))
    .keep_alive(axum::response::sse::KeepAlive::default())
}

/// POST /api/subscriptions/:id/speedtest — speed-test every node of a single
/// subscription in isolation, with the same live SSE progress stream as the
/// global `/api/speedtest`. Lets the user re-test one bad subscription without
/// touching the rest of the store. 404 when the id is unknown or has no nodes.
pub(crate) async fn speedtest_subscription(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(req): Query<SpeedTestReq>,
) -> Result<Sse<impl stream::Stream<Item = Result<Event, Infallible>>>, (StatusCode, String)> {
    let timeout_ms = req.timeout_ms.unwrap_or(4000);
    let concurrency = req.concurrency.unwrap_or(20);
    let proxies: Vec<Proxy> = {
        let guard = state.store.lock_ok();
        guard
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.proxies.clone())
            .unwrap_or_default()
    };
    if proxies.is_empty() {
        return Err((StatusCode::NOT_FOUND, "订阅不存在或没有节点".into()));
    }
    Ok(run_speedtest_sse(state, proxies, timeout_ms, concurrency).await)
}

/// Validate an upstream pull-proxy by fetching a probe URL through it. Used by
/// the WebUI's "测试代理" button so the user can confirm the proxy works
/// before relying on it to fetch blocked subscriptions.
pub(crate) async fn proxy_test(Json(req): Json<ProxyTestReq>) -> Json<serde_json::Value> {
    let proxy = req.proxy.trim();

    if proxy.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "代理地址为空" }));
    }

    let test_url = req
        .url
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "https://www.gstatic.com/generate_204".to_string());

    let client = match client_with_proxy(Some(proxy)) {
        Ok(c) => c,

        Err(e) => return Json(serde_json::json!({ "ok": false, "error": e })),
    };

    match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        client.get(&test_url).send(),
    )
    .await
    {
        Ok(Ok(r)) => Json(serde_json::json!({ "ok": true, "status": r.status().as_u16() })),

        Ok(Err(e)) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),

        Err(_) => Json(serde_json::json!({ "ok": false, "error": "代理连接超时 (15s)" })),
    }
}

/// Gist 文件名按格式选择扩展名：JSON / surge / base64 不应伪装成 yaml。
fn gist_file_name(format: &str) -> &'static str {
    match format {
        "v2ray" | "sing-box" | "singbox" => "subscription.json",
        "surge" => "subscription.conf",
        "base64" => "subscription.txt",
        _ => "subscription.yaml",
    }
}

#[cfg(test)]
mod tests {
    use super::gist_file_name;

    #[test]
    fn gist_file_name_matches_format() {
        assert_eq!(gist_file_name("clash-meta"), "subscription.yaml");
        assert_eq!(gist_file_name("clash"), "subscription.yaml");
        assert_eq!(gist_file_name("v2ray"), "subscription.json");
        assert_eq!(gist_file_name("sing-box"), "subscription.json");
        assert_eq!(gist_file_name("surge"), "subscription.conf");
        assert_eq!(gist_file_name("base64"), "subscription.txt");
    }
}
