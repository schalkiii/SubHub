use axum::{

    extract::{DefaultBodyLimit, Path, Query, State},

    http::{header, HeaderValue, StatusCode},

    routing::{delete, get, post},

    Json, Router,

};

use serde::{Deserialize, Serialize};

use std::collections::{HashMap, VecDeque};

use std::net::SocketAddr;

use std::path::PathBuf;

use std::sync::{Arc, Mutex};

use subhub_core::{

    apply_transform, export_str, ops::*, parse_subscription, speedtest::SpeedTestResult, Proxy,

    ProxyUnlock, Subscription, Transform,

};



mod db;

mod engine;



// ---- subscription traffic usage ----
/// (upload, download, total, expire) — traffic usage parsed from a
/// subscription's info block and/or its `Subscription-Userinfo` response header.
type UsageTuple = (Option<u64>, Option<u64>, Option<u64>, Option<u64>);

/// Parse a `Subscription-Userinfo` response header of the form
/// `upload=0; download=0; total=0; expire=0` (bytes / epoch seconds) into the
/// four usage fields. The header (when present) is the authoritative source;
/// the clash info block from the body is the fallback.
fn parse_userinfo_header(h: &str) -> UsageTuple {
    let mut up = None;
    let mut dl = None;
    let mut tot = None;
    let mut exp = None;
    for kv in h.split(';') {
        if let Some((k, v)) = kv.trim().split_once('=') {
            let val = v.trim().parse::<u64>().ok();
            match k.trim().to_ascii_lowercase().as_str() {
                "upload" => up = val,
                "download" => dl = val,
                "total" => tot = val,
                "expire" => exp = val.map(subhub_core::normalize_epoch),
                _ => {}
            }
        }
    }
    (up, dl, tot, exp)
}

/// Combine the response header (authoritative) with the clash info block
/// parsed from the response body. Returns (upload, download, total, expire).
fn usage_from_sources(userinfo: &Option<String>, body: &str) -> UsageTuple {
    let mut u = subhub_core::extract_subscription_usage(body);
    if let Some(h) = userinfo {
        let (hu, hd, ht, he) = parse_userinfo_header(h);
        if hu.is_some() { u.upload = hu; }
        if hd.is_some() { u.download = hd; }
        if ht.is_some() { u.total = ht; }
        if he.is_some() { u.expire = he; }
    }
    (u.upload, u.download, u.total, u.expire)
}

pub type Store = Arc<Mutex<Vec<Subscription>>>;



/// Blocking entry point for embedding the server (e.g. inside the Tauri app).
/// Spins up a tokio runtime and serves until the process exits.
pub fn run_blocking() {

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    rt.block_on(run_server());

}



#[derive(Clone)]

pub struct AppState {

    pub store: Store,

    /// rolling history of dashboard snapshots (trend charts)
    pub history: Arc<Mutex<VecDeque<TrendPoint>>>,

    /// optional durable store (SQLite); `None` = in-memory only (no persistence)
    pub db: Option<db::Db>,

    /// master switch: when false, no upstream proxy is used for fetching or
    /// refreshing subscriptions, even if a subscription stores a `fetch_proxy`.
    pub use_proxy: Arc<Mutex<bool>>,

    /// auto-refresh interval (seconds) for remote subscriptions. 0 = disabled.
    /// Runtime-configurable via /api/settings and persisted to the meta table,
    /// so it can change without restarting the server.
    pub auto_refresh_sec: Arc<Mutex<u64>>,

    /// default upstream proxy used to prefill the "pull proxy" box and as a
    /// fallback when adding a subscription without an explicit proxy. Persisted
    /// to the `meta` table so it survives restarts (single source of truth,
    /// replacing the old browser-local "remember" checkbox).
    pub default_fetch_proxy: Arc<Mutex<Option<String>>>,

    /// when exporting a "top-N" subscription, only the N highest-scoring nodes
    /// are kept (0 = all nodes). Runtime-configurable via /api/settings and
    /// persisted to the meta table.
    pub top_n: Arc<Mutex<u64>>,

    /// external speed-test engine (mihomo / sing-box compatible) for real proxy
    /// latency + bandwidth. Configured via the UI (/api/settings) and persisted
    /// to the meta table, falling back to the SUBHUB_ENGINE_BIN env var.
    pub engine_bin: Arc<Mutex<Option<String>>>,

    /// Auto-remove a node once it has been found unavailable for this many
    /// consecutive speed tests (0 = disabled, keep everything). Persisted to
    /// the `meta` table so it survives restarts and applies to every test path
    /// (manual speedtest, post-add test, refresh, scheduler).
    pub remove_after_fails: Arc<Mutex<u64>>,

    /// Serializes DB writes so two `save_all` transactions (each a
    /// DELETE + full re-INSERT of the subscription set) can never interleave
    /// and corrupt — or partially clobber — the persisted data. Mutations
    /// already go to the shared in-memory store in-place, so this is
    /// defense-in-depth against interleaved transactions, not a lost-update
    /// fix (that class is already avoided by the in-place mutation model).
    pub persist_lock: Arc<Mutex<()>>,

}

/// Resolve the external-engine binary path. `None` disables the engine (basic
/// TCP latency + throughput estimate only). The UI-persisted setting wins; the
/// env var `SUBHUB_ENGINE_BIN` is seeded at startup as a fallback.
fn engine_bin_of(state: &AppState) -> Option<String> {
    state
        .engine_bin
        .lock()
        .unwrap()
        .clone()
        .filter(|v| !v.trim().is_empty())
}



/// A single dashboard snapshot recorded over time (trend chart).
#[derive(Debug, Clone, Serialize)]

pub struct TrendPoint {

    pub t: u64,

    pub total: usize,

    pub available: usize,

    pub unavailable: usize,

    pub untested: usize,

    pub avg_latency_ms: Option<u64>,

    pub best_latency_ms: Option<u64>,

}



// ----------------------- request / response DTOs -----------------------



#[derive(Deserialize)]

pub struct AddReq {

    pub urls: Vec<String>,

    /// optional upstream proxy used to fetch every URL in this batch
    /// (e.g. "http://127.0.0.1:7890" or "socks5://127.0.0.1:1080")
    pub fetch_proxy: Option<String>,

}



#[derive(Deserialize)]

pub struct ImportReq {

    pub content: String,

    /// optional display name for the local subscription (e.g. "我的节点");
    /// when absent a name is auto-derived ("本地导入 N").
    pub name: Option<String>,

}



#[derive(Deserialize)]

pub struct ExportReq {

    pub format: Option<String>,

    /// optional transform pipeline (filter / sort / rename)
    pub transform: Option<Transform>,

    /// optional: only merge these subscription ids (default = all)
    pub sub_ids: Option<Vec<String>>,

    /// optional: when set (> 0), only the N highest-scoring nodes are exported
    pub top_n: Option<u64>,

}



#[derive(Deserialize)]

pub struct ListQuery {

    pub r#type: Option<String>,

    pub region: Option<String>,

    pub q: Option<String>,

    /// 1-based page index (default 1)
    pub page: Option<usize>,

    /// items per page (default 50, clamped to 1..=500)
    pub page_size: Option<usize>,

    /// global sort key applied BEFORE pagination: name | latency | speed | score
    pub sort: Option<String>,

    /// sort descending when true; ascending otherwise (default false)
    pub desc: Option<bool>,

}



/// Query params for `GET /api/subscriptions`. Supports a keyword search across
/// the subscription name and source URL (case-insensitive) so the WebUI can
/// offer a quick "find a subscription" box.
#[derive(Deserialize)]

pub struct SubListQuery {

    /// keyword matched against `name` and `source` (contains, case-insensitive)
    pub q: Option<String>,

}



/// Response wrapper for `/api/proxies` now that the list is paginated.
#[derive(Serialize)]

pub struct ProxiesResp {

    pub total: usize,

    pub page: usize,

    pub page_size: usize,

    pub items: Vec<serde_json::Value>,

}



/// Query params for the direct-pull local subscription URL (`GET /sub`).
/// Lets a proxy client (mihomo/clash/etc.) subscribe to the merged output and
/// optionally encode a small transform pipeline via the URL itself.
#[derive(Deserialize)]

pub struct SubQuery {

    pub format: Option<String>,

    /// comma-separated subscription ids to merge (default: all)
    pub sub: Option<String>,

    pub sort: Option<String>,

    pub desc: Option<String>,

    pub rename_pat: Option<String>,

    pub rename_rep: Option<String>,

    /// include-only filter on node name (contains, case-insensitive)
    pub q: Option<String>,

    /// include-only filter on region code (contains)
    pub region: Option<String>,

    pub r#type: Option<String>,

    /// optional: keep only the N highest-scoring nodes (e.g. `top_n=20`).
    /// Lets the standing subscription URL respect the same Top-N idea as the
    /// manual export — handy for a small "best nodes" subscribe link.
    pub top_n: Option<String>,

}



#[derive(Deserialize)]

pub struct SpeedTestReq {

    pub timeout_ms: Option<u64>,

    pub concurrency: Option<usize>,

    /// Test-scope filter for the "仅测未测 / 仅失败" WebUI selector:
    /// - `None` / `""` / `"all"` -> test every node (default)
    /// - `"untested"` -> only nodes that were never speed-tested
    ///   (`last_tested_at == None`)
    /// - `"failed"`   -> only nodes currently marked unavailable
    ///   (`available == Some(false)`)
    ///
    /// Mirrors the incremental behaviour of the auto-refresh path so re-runs
    /// don't re-hammer already-known nodes.
    pub mode: Option<String>,

}

/// Wrapper for `POST /api/speedtest` so the WebUI can show how many nodes were
/// auto-removed by the "remove after N consecutive failures" rule.
#[derive(Serialize)]

pub struct SpeedTestResp {

    pub results: Vec<SpeedTestResult>,

    /// nodes removed this run because they hit the `remove_after_fails` threshold
    pub removed: usize,

    /// the active threshold that was applied (0 = auto-remove disabled)
    pub threshold: u64,

}



#[derive(Deserialize)]

pub struct ProxyTestReq {

    /// upstream proxy to validate, e.g. "http://127.0.0.1:7890"
    pub proxy: String,

    /// optional URL to probe through the proxy (defaults to a connectivity check)
    pub url: Option<String>,

}



/// Global runtime settings (read/get + patch/post).
#[derive(Serialize, Deserialize)]

pub struct SettingsResp {

    /// master proxy switch: when false, no upstream proxy is used for fetch/refresh
    pub use_proxy: bool,

    /// auto-refresh interval in seconds for remote subscriptions (0 = disabled)
    pub auto_refresh_sec: u64,

    /// default upstream proxy prefilled into the "pull proxy" box (server-side
    /// source of truth, replaces the old browser-local "remember" checkbox)
    pub default_fetch_proxy: Option<String>,

    /// when exporting a "top-N" subscription, keep only the N highest-scoring
    /// nodes (0 = all nodes)
    pub top_n: u64,

    /// external speed-test engine binary path (mihomo / sing-box compatible);
    /// `None` = engine disabled (basic TCP latency + throughput estimate only)
    pub engine_bin: Option<String>,

    /// auto-remove a node after this many consecutive failed speed tests
    /// (0 = disabled)
    pub remove_after_fails: u64,

}


#[derive(Deserialize)]

pub struct SettingsReq {

    /// optional: when present, updates the auto-refresh interval (seconds)
    pub auto_refresh_sec: Option<u64>,

    /// optional: when present, updates the default pull proxy (empty string
    /// clears it)
    pub default_fetch_proxy: Option<String>,

    /// optional: when present, updates the master proxy switch (partial
    /// updates are allowed, so a settings POST may omit fields it doesn't want
    /// to change)
    pub use_proxy: Option<bool>,

    /// optional: when present, updates the "top-N" export size (0 = all nodes)
    pub top_n: Option<u64>,

    /// optional: when present, updates the external engine binary path
    /// (mihomo / sing-box). Empty string clears it (engine disabled).
    pub engine_bin: Option<String>,

    /// optional: when present, updates the "auto-remove after N consecutive
    /// failed speed tests" threshold (0 = disabled)
    pub remove_after_fails: Option<u64>,

}



/// One subscription as stored in an export file. `proxies` carries the full
/// node list (including any previously measured latency/bandwidth), so the
/// file is self-contained and re-importable offline.
#[derive(Serialize, Deserialize, Clone)]
pub struct SubExportItem {
    /// stable id (sequential `sub_N`); used for idempotent re-import into the
    /// same instance. Ignored when no matching id exists (e.g. cross-machine).
    pub id: String,
    pub name: String,
    pub source: String,
    pub source_type: String,
    pub fetch_proxy: Option<String>,
    pub health_enabled: bool,
    pub proxies: Vec<Proxy>,
}

/// Top-level document returned by `GET /api/subscriptions/export`.
#[derive(Serialize)]
pub struct SubExportDoc {
    pub kind: &'static str,
    pub version: u32,
    pub exported_at: u64,
    pub engine_bin: Option<String>,
    pub subscriptions: Vec<SubExportItem>,
}

/// Request body for `POST /api/subscriptions/import`.
#[derive(Deserialize)]
pub struct SubImportReq {
    pub subscriptions: Vec<SubExportItem>,
}

/// Result of importing a subscription list.
#[derive(Serialize)]
pub struct SubImportResp {
    /// new subscriptions added
    pub added: usize,
    /// existing subscriptions updated (matched by source URL)
    pub replaced: usize,
    /// total node count across all subscriptions after import
    pub total: usize,
    /// number of subscriptions after import
    pub subscriptions: usize,
}


#[derive(Serialize)]

pub struct CountResp {

    pub added: usize,

    pub total: usize,

    pub subscriptions: usize,

}



#[derive(Serialize)]

pub struct SubSummary {

    pub id: String,

    pub name: String,

    pub source: String,

    pub source_type: String,

    pub enabled: bool,

    pub count: usize,

    pub healthy: usize,

    pub unknown: usize,

    /// health percentage = healthy_node_count * 100 / node_count
    /// (None when the subscription has no nodes); used for the health-degree
    /// badge. Aligns with the `degraded` status threshold (< 50%).
    pub health_pct: Option<u8>,

    pub avg_latency_ms: Option<u64>,

    pub best_latency_ms: Option<u64>,

    pub status: String,

    pub last_checked_at: Option<u64>,

    pub last_updated_at: Option<u64>,

    pub last_error: Option<String>,

    /// traffic usage (clash info block / `Subscription-Userinfo` response
    /// header). All `Option` — absent for pasted/local subscriptions or those
    /// without usage info. `total == Some(0)` means "unlimited".
    pub upload: Option<u64>,

    pub download: Option<u64>,

    pub total: Option<u64>,

    pub expire: Option<u64>,

}



fn now_ms() -> u64 {

    std::time::SystemTime::now()

        .duration_since(std::time::UNIX_EPOCH)

        .map(|d| d.as_millis() as u64)

        .unwrap_or(0)

}



/// Build an HTTP client, optionally routed through an upstream proxy used to
/// fetch a remote subscription (supports `http` / `https` / `socks5`). Returns
/// an error string if the proxy URL is invalid so the caller can record it as
/// the subscription's `last_error`.
/// When no proxy is requested we call `.no_proxy()` so the request connects
/// directly and never silently falls back to a system `HTTP(S)_PROXY` env var.
/// This makes the global "use proxy" switch authoritative: off = truly direct.
fn client_with_proxy(proxy: Option<&str>) -> Result<reqwest::Client, String> {

    let mut b = reqwest::Client::builder()

        .timeout(std::time::Duration::from_secs(20))

        // Bound the redirect chain. Subscriptions sometimes 301 to HTTPS or a
        // short URL, but an unbounded chain is a redirect-bomb DoS vector
        // (a server that 302s to itself). We don't follow internal/metadata
        // endpoints because this is a single-user local tool and the body is
        // only ever parsed for proxies, never returned — still, keep it tight.
        .redirect(reqwest::redirect::Policy::limited(5))

        .user_agent(

            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36",

        );

    match proxy {

        Some(p) => {

            let p = p.trim();

            if p.is_empty() {

                b = b.no_proxy();

            } else {

                b = b.proxy(

                    reqwest::Proxy::all(p).map_err(|e| format!("代理地址无效 {p}: {e}"))?,

                );

            }

        }

        None => {

            b = b.no_proxy();

        }

    }

    b.build().map_err(|e| format!("创建请求客户端失— {e}"))

}

/// Maximum bytes we'll buffer from a remote subscription response. A hostile or
/// misconfigured URL can return an enormous body; `DefaultBodyLimit` only caps
/// *request* bodies, so we must cap the *response* ourselves to avoid an
/// easy memory-exhaustion DoS.
const MAX_SUB_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;

/// Fetch a subscription URL and return `(body_text, subscription-userinfo
/// header)`, rejecting responses that exceed `MAX_SUB_RESPONSE_BYTES`.
/// Prefers a `Content-Length` pre-check, then falls back to a post-read
/// length guard for chunked bodies.
async fn fetch_subscription_text(
    client: &reqwest::Client,
    url: &str,
) -> Result<(String, Option<String>), String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("fetch failed: {e}"))?;
    let userinfo = resp
        .headers()
        .get("subscription-userinfo")
        .or_else(|| resp.headers().get("Subscription-Userinfo"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if let Some(cl) = resp.content_length() {
        if cl > MAX_SUB_RESPONSE_BYTES {
            return Err(format!(
                "订阅响应过大 ({}B > {}B)",
                cl, MAX_SUB_RESPONSE_BYTES
            ));
        }
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("read body failed: {e}"))?;
    if bytes.len() as u64 > MAX_SUB_RESPONSE_BYTES {
        return Err(format!("订阅响应过大 (超过 {}B)", MAX_SUB_RESPONSE_BYTES));
    }
    Ok((String::from_utf8_lossy(&bytes).into_owned(), userinfo))
}

/// Strip userinfo (`user:pass@`) and the query string (`?...`) from a URL so it
/// can be logged or shown without leaking subscription auth tokens.
fn redact_url(url: &str) -> String {
    let without_query = match url.split_once('?') {
        Some((head, _)) => head,
        None => url,
    };
    if let Some(at) = without_query.find('@') {
        if let Some(scheme_end) = without_query.find("://") {
            let (scheme, rest) = without_query.split_at(scheme_end + 3);
            // keep everything after the first '@' inside `rest`
            let host_and_path = &rest[at + 1.min(rest.len())..];
            return format!("{}{}", scheme, host_and_path);
        }
    }
    without_query.to_string()
}



/// Shared speed-test core: always measures TCP latency; adds engine-based HTTP
/// latency + download bandwidth when `SUBHUB_ENGINE_BIN` is configured.
/// Returns `(tcp_results, engine_http, engine_bw)` —all aligned to the input
/// `proxies` order except `tcp_results` (which preserves completion order and
/// is keyed by fingerprint, so callers must match by `fingerprint()`).
/// Note: speed tests never go through any upstream proxy (`fetch_proxy` /
/// `use_proxy`).  TCP ping connects directly to each node's server:port;
/// engine-based tests use their own external binary config. This is
/// intentional —a proxy toggle should only affect *fetching* subscriptions,
/// not measuring raw node quality.
async fn run_speedtest_core(

    state: &AppState,

    proxies: &[Proxy],

    timeout_ms: u64,

    concurrency: usize,

) -> (Vec<SpeedTestResult>, Vec<Option<u64>>, Vec<Option<f64>>) {

    let engine_bin = engine_bin_of(state);

    let proxies_tcp = proxies.to_vec();

    let tcp_results = tokio::task::spawn_blocking(move || {

        subhub_core::tcp_ping_all(&proxies_tcp, timeout_ms, concurrency)

    })

    .await

    .unwrap_or_default();



    // Run the HTTP-latency and bandwidth engine passes concurrently instead of
    // sequentially — they share the same (clamped) engine-worker budget, so
    // running them at the same time roughly halves the engine portion of a
    // speedtest and removes the "tested 10 then nothing happens" feel.
    let (engine_http, engine_bw) = if let Some(bin) = engine_bin.clone() {

        let bin_http = bin.clone();
        let bin_bw = bin.clone();
        let proxies_http = proxies.to_vec();
        let proxies_bw = proxies.to_vec();
        let (h, b) = tokio::join!(
            engine::engine_http_latency(&proxies_http, &bin_http, timeout_ms, concurrency),
            engine::engine_bandwidth(&proxies_bw, &bin_bw, timeout_ms, concurrency),
        );
        (h, b)
    } else {
        (vec![None; proxies.len()], vec![None; proxies.len()])
    };



    (tcp_results, engine_http, engine_bw)

}



/// Persist speed-test results back onto the matching proxies (by fingerprint)
/// and stamp `last_tested_at`. Used by both the manual speedtest endpoint and
/// the automatic post-add test so there is a single source of truth.
fn persist_results(

    state: &AppState,

    proxies: &[Proxy],

    tcp: &[SpeedTestResult],

    http: &[Option<u64>],

    bw: &[Option<f64>],

) -> usize {

    let now = now_ms();

    let fp_to_idx: std::collections::HashMap<String, usize> = proxies

        .iter()

        .enumerate()

        .map(|(i, p)| (p.fingerprint(), i))

        .collect();

    let mut by_fp: std::collections::HashMap<String, (Option<u64>, bool, Option<f64>, bool)> =

        std::collections::HashMap::new();

    for r in tcp {

        let idx = fp_to_idx.get(&r.fingerprint).copied();

        let h = idx.and_then(|i| http.get(i)).and_then(|x| *x);

        let lat = r.tcp_latency_ms.or(h);

        // A node the engine successfully probed over its own tunnel is genuinely
        // reachable even if direct TCP to it was blocked, so count it alive.
        let avail = r.available || h.is_some();

        // Prefer the real engine bandwidth when present; otherwise fall back to
        // the engine-free TCP throughput estimate carried on the TCP result so
        // the 速度 column is populated even without SUBHUB_ENGINE_BIN. Only the
        // engine value is flagged as measured — the estimate is NOT, so the
        // composite score ignores it.
        let (dl, measured) = match idx.and_then(|i| bw.get(i)).and_then(|x| *x) {
            Some(b) => (Some(b), true),
            None => (r.download_speed_bps, false),
        };

        by_fp.insert(r.fingerprint.clone(), (lat, avail, dl, measured));

    }

    let threshold = *state.remove_after_fails.lock().unwrap();

    let mut removed: usize = 0;

    let mut guard = state.store.lock().unwrap();

    for sub in guard.iter_mut() {

        for p in sub.proxies.iter_mut() {

            if let Some((lat, avail, dl, measured)) = by_fp.get(&p.fingerprint()) {

                p.latency_ms = *lat;

                p.available = Some(*avail);

                p.last_tested_at = Some(now);

                if let Some(b) = dl {

                    p.download_speed_bps = Some(*b);

                }

                p.bandwidth_measured = *measured;

                // Consecutive-failure tracking for the auto-remove feature.
                // Only a *tested* failure (`Some(false)`) counts; an untested
                // node (`None`) is left alone, and any success resets the run.
                if *avail {
                    p.consecutive_failures = 0;
                } else {
                    p.consecutive_failures = p.consecutive_failures.saturating_add(1);
                }

            }

        }

        // Auto-remove nodes that have now failed `threshold` times in a row.
        // Disabled when threshold == 0. We keep nodes that are untested
        // (`available != Some(false)`) so a node simply never tested is safe.
        if threshold > 0 {
            let before = sub.proxies.len();
            sub.proxies.retain(|p| {
                !(p.available == Some(false) && u64::from(p.consecutive_failures) >= threshold)
            });
            removed += before - sub.proxies.len();
        }

    }

    removed

}



/// Snapshot the whole in-memory store into the durable DB (if configured).
/// No-op when persistence is disabled. Safe to call after any mutation so the
/// user never loses subscriptions / speed-test results on restart.
fn persist_all(state: &AppState) {

    if let Some(db) = &state.db {

        let subs = state.store.lock().unwrap().clone();

        // Serialize DB writes so concurrent `save_all` transactions can't
        // interleave (DELETE + full re-INSERT) and corrupt the store.
        let _g = state.persist_lock.lock().unwrap();
        db.save_all(&subs);

    }

}



/// Build a subscription summary (recomputing derived health from proxies).
fn sub_to_summary(s: &Subscription) -> SubSummary {

    let mut h = s.health.clone();

    h.recompute(&s.proxies);

    // safe division (also guards against `healthy * 100` overflow on huge

    // node counts by promoting to u64 before the multiply).

    let health_pct = (h.healthy_node_count as u64)

        .checked_mul(100)

        .and_then(|n| n.checked_div(h.node_count as u64))

        .map(|v| v as u8);

    SubSummary {

        id: s.id.clone(),

        name: s.name.clone(),

        source: s.source.clone(),

        source_type: h.source_type.clone(),

        enabled: h.enabled,

        count: h.node_count,

        healthy: h.healthy_node_count,

        unknown: h.unknown_node_count,

        health_pct,

        avg_latency_ms: h.avg_latency_ms,

        best_latency_ms: h.best_latency_ms,

        status: h.status().to_string(),

        last_checked_at: h.last_checked_at,

        last_updated_at: h.last_updated_at,

        last_error: h.last_error.clone(),

        upload: h.upload,

        download: h.download,

        total: h.total,

        expire: h.expire,

    }

}



#[derive(Serialize)]

pub struct DashboardResp {

    pub total: usize,

    pub by_type: HashMap<String, usize>,

    pub by_region: HashMap<String, usize>,

    pub subscriptions: usize,

    pub available: usize,

    pub unavailable: usize,

    pub untested: usize,

    pub avg_latency_ms: Option<u64>,

    pub best_latency_ms: Option<u64>,

    pub per_sub: Vec<SubSummary>,

}



// ----------------------- helpers -----------------------



fn flatten_dedup(subs: &[Subscription], only: Option<&[String]>) -> Vec<Proxy> {

    let mut collected: Vec<Proxy> = Vec::new();

    for s in subs {

        if let Some(ids) = only {

            if !ids.contains(&s.id) {

                continue;

            }

        }

        collected.extend_from_slice(&s.proxies);

    }

    subhub_core::merge(&collected)

}

/// Apply the `mode` test-scope filter from the WebUI's "仅测未测 / 仅失败"
/// selector. `all` / `""` / unknown values return the list unchanged.
fn apply_mode(proxies: Vec<Proxy>, mode: &str) -> Vec<Proxy> {
    let mut out = proxies;
    match mode {
        "untested" => out.retain(|p| p.last_tested_at.is_none()),
        "failed" => out.retain(|p| p.available == Some(false)),
        _ => {}
    }
    out
}



fn sub_name_from_url(url: &str) -> String {

    let u = url.trim();

    let host = u

        .split("//")

        .nth(1)

        .and_then(|s| s.split('/').next())

        .unwrap_or(u);

    let host = host.split(':').next().unwrap_or(host);

    if host.is_empty() {

        "订阅".to_string()

    } else {

        host.to_string()

    }

}



/// Build a `Transform` from the direct-pull URL query params. Supports the most
/// common operators so a shareable subscription link can encode its own
/// pipeline: name/region/type include-filters, sort, and regex rename.
fn transform_from_sub_query(q: &SubQuery) -> Option<Transform> {

    let mut filters: Vec<FilterRule> = Vec::new();

    if let Some(v) = &q.q {

        if !v.trim().is_empty() {

            filters.push(FilterRule {

                field: "name".into(),

                mode: "include".into(),

                match_: "contains".into(),

                value: v.clone(),

            });

        }

    }

    if let Some(v) = &q.region {

        if !v.trim().is_empty() {

            filters.push(FilterRule {

                field: "region".into(),

                mode: "include".into(),

                match_: "contains".into(),

                value: v.clone(),

            });

        }

    }

    if let Some(v) = &q.r#type {

        if !v.trim().is_empty() {

            filters.push(FilterRule {

                field: "type".into(),

                mode: "include".into(),

                match_: "exact".into(),

                value: v.clone(),

            });

        }

    }

    let sort = q.sort.as_ref().filter(|s| !s.is_empty()).map(|k| SortBy {

        key: k.clone(),

        desc: matches!(q.desc.as_deref(), Some("1") | Some("true")),

    });

    let rename = match (&q.rename_pat, &q.rename_rep) {

        (Some(p), Some(r)) if !p.is_empty() => Some(RenameRule {

            pattern: p.clone(),

            replacement: r.clone(),

        }),

        _ => None,

    };

    if filters.is_empty() && sort.is_none() && rename.is_none() {

        None

    } else {

        Some(Transform {

            filters,

            sort,

            rename,

        })

    }

}



// ----------------------- handlers -----------------------



async fn health() -> &'static str {

    "ok"

}



async fn add_subscriptions(

    State(state): State<AppState>,

    Json(req): Json<AddReq>,

) -> Result<Json<CountResp>, (StatusCode, String)> {

    let use_proxy = *state.use_proxy.lock().unwrap();

    let effective_proxy: Option<String> = if use_proxy {
        match req.fetch_proxy.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(s) => Some(s.to_string()),
            None => {
                let d = state.default_fetch_proxy.lock().unwrap();
                d.as_deref().filter(|s| !s.trim().is_empty()).map(|s| s.to_string())
            }
        }
    } else {
        None
    };
    let proxy_opt = effective_proxy.as_deref();

    let mut new_subs: Vec<Subscription> = Vec::new();

    for url in &req.urls {

        let url = url.trim();

        if url.is_empty() {

            continue;

        }

        let now = now_ms();

        let mut sub = Subscription::new(sub_name_from_url(url), url.to_string(), Vec::new());

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

        match fetch_subscription_text(&client, url).await {

            Ok((text, userinfo)) => {
                let proxies = parse_subscription(&text);
                let (up, dl, tot, exp) = usage_from_sources(&userinfo, &text);
                let n = proxies.len();
                sub.proxies = proxies;
                sub.health.upload = up;
                sub.health.download = dl;
                sub.health.total = tot;
                sub.health.expire = exp;
                sub.health.last_updated_at = Some(now);
                sub.health.last_error = None;
                eprintln!("fetched {}: {n} nodes", redact_url(url));
            }

            Err(e) => {
                sub.health.last_error = Some(e);
            }

        }

        new_subs.push(sub);

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
            *state.default_fetch_proxy.lock().unwrap() = Some(p.clone());
            if let Some(db) = state.db.as_ref() {
                db.meta_set("default_fetch_proxy", p);
            }
        }
    }

    let added: usize = new_subs.iter().map(|s| s.proxies.len()).sum();

    // only the freshly added nodes are tested (not the whole existing store)

    let new_proxies: Vec<Proxy> = new_subs.iter().flat_map(|s| s.proxies.clone()).collect();

    let (total, subscriptions) = {

        let mut guard = state.store.lock().unwrap();

        guard.extend(new_subs);

        let total: usize = guard.iter().map(|s| s.proxies.len()).sum();

        (total, guard.len())

    };



    // Auto health + speedtest on first add. TCP latency always runs; HTTP

    // latency + bandwidth use the engine if set.

    if !new_proxies.is_empty() {

        let (tcp, http, bw) = run_speedtest_core(&state, &new_proxies, 4000, 30).await;

        persist_results(&state, &new_proxies, &tcp, &http, &bw);

    }



    persist_all(&state);

    Ok(Json(CountResp {

        added,

        total,

        subscriptions,

    }))

}



async fn import_raw(State(state): State<AppState>, Json(req): Json<ImportReq>) -> Json<CountResp> {

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

        .unwrap_or_else(|| format!("本地导入 {}", state.store.lock().unwrap().len() + 1));

    let now = now_ms();

    let mut sub = Subscription::new(name, "pasted".to_string(), proxies);

    sub.health.last_checked_at = Some(now);

    sub.health.last_updated_at = Some(now);

    let new_proxies = sub.proxies.clone();

    let (total, added) = {

        let mut guard = state.store.lock().unwrap();

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

        subscriptions: state.store.lock().unwrap().len(),

    })

}


/// GET /api/subscriptions/export — dump the full subscription list (with
/// each subscription's nodes and measured results) as a self-contained JSON
/// document for backup / transfer.
async fn export_subscriptions(State(state): State<AppState>) -> Json<SubExportDoc> {
    let subs = state.store.lock().unwrap().clone();
    let items: Vec<SubExportItem> = subs
        .into_iter()
        .map(|s| SubExportItem {
            id: s.id.clone(),
            name: s.name.clone(),
            source: s.source.clone(),
            source_type: s.health.source_type.clone(),
            fetch_proxy: s.fetch_proxy.clone(),
            health_enabled: s.health.enabled,
            proxies: s.proxies.clone(),
        })
        .collect();
    Json(SubExportDoc {
        kind: "subhub-subscriptions",
        version: 1,
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
async fn import_subscriptions(
    State(state): State<AppState>,
    Json(req): Json<SubImportReq>,
) -> Json<SubImportResp> {
    let now = now_ms();
    let (added, replaced) = {
        let mut guard = state.store.lock().unwrap();
        // Index existing subs for merge by source URL only — a re-import of a
        // remote subscription is idempotent. We deliberately do NOT index by the
        // client-supplied `id`: ids are sequential (`sub_N`) and trusting them
        // would let a caller overwrite an unrelated existing subscription.
        let mut by_url: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
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
            // Match only by source URL (never the client-supplied id) so a
            // re-import of a remote subscription is idempotent without letting a
            // caller target/overwrite an arbitrary existing subscription.
            let target = if it.source.starts_with("http://") || it.source.starts_with("https://") {
                by_url.get(&it.source).copied()
            } else {
                None
            };
            let mut sub = Subscription::new(it.name.clone(), it.source.clone(), it.proxies.clone());
            sub.fetch_proxy = it.fetch_proxy.clone();
            sub.health.enabled = it.health_enabled;
            sub.health.source_type = if it.source.starts_with("http://")
                || it.source.starts_with("https://")
            {
                "remote".to_string()
            } else {
                it.source_type.clone()
            };
            sub.health.last_checked_at = Some(now);
            sub.health.last_updated_at = Some(now);
            if let Some(idx) = target {
                // keep the original id so refresh/delete links stay valid
                sub.id = guard[idx].id.clone();
                guard[idx] = sub;
                replaced += 1;
            } else {
                guard.push(sub);
                added += 1;
            }
        }
        (added, replaced)
    };

    let (total, subscriptions) = {
        let guard = state.store.lock().unwrap();
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



async fn list_subscriptions(
    State(state): State<AppState>,
    Query(q): Query<SubListQuery>,
) -> Json<Vec<SubSummary>> {

    let guard = state.store.lock().unwrap();

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



async fn delete_subscription(

    State(state): State<AppState>,

    Path(id): Path<String>,

) -> (StatusCode, Json<CountResp>) {

    let (total, subscriptions, found) = {

        let mut guard = state.store.lock().unwrap();

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
    let status = if found { StatusCode::OK } else { StatusCode::NOT_FOUND };

    (status, Json(CountResp {

        added: 0,

        total,

        subscriptions,

    }))

}



/// Refresh a remote subscription: re-fetch it and update its nodes + health.
/// Local (pasted) subscriptions just get re-stamped. `None` is returned when
/// the subscription id is unknown (so callers can 404).
/// This is the shared core used by both `POST /api/subscriptions/:id/refresh`
/// and the background auto-refresh scheduler. No `MutexGuard` is held across
/// `.await` —we snapshot under the lock, drop it, await the fetch, then
/// re-lock to persist. The upstream proxy is only used when the global
/// `use_proxy` master switch is on.
async fn do_refresh_one(state: &AppState, id: &str) -> Option<serde_json::Value> {

    let now = now_ms();

    let target = {

        let guard = state.store.lock().unwrap();

        guard.iter().find(|s| s.id == id).map(|s| {

            (

                s.source.clone(),

                s.health.source_type.clone(),

                s.fetch_proxy.clone(),

            )

        })

    };

    let (url, source_type, fetch_proxy) = target?;



    let use_proxy = *state.use_proxy.lock().unwrap();

    let proxy_opt = if use_proxy {

        fetch_proxy.as_deref().filter(|s| !s.trim().is_empty())

    } else {

        None

    };

    let client = match client_with_proxy(proxy_opt)

    {

        Ok(c) => c,

        Err(e) => {

            let msg = e.clone();

            let mut guard = state.store.lock().unwrap();

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
                let usage = usage_from_sources(&userinfo, &text);
                fetched = Some(proxies);
                usage_hint = Some(usage);
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

        let mut guard = state.store.lock().unwrap();

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



async fn refresh_subscription(

    State(state): State<AppState>,

    Path(id): Path<String>,

) -> Json<serde_json::Value> {

    match do_refresh_one(&state, &id).await {

        Some(v) => {

            persist_all(&state);

            Json(v)

        }

        None => Json(serde_json::json!({ "status": "error", "error": "not found" })),

    }

}



/// BestSub-style outbound geo detection: route each proxy through the external
/// engine and ask the geo-IP channels for the exit country. Persists the code
/// back onto each proxy. No-op (empty result) when no engine is configured.
async fn geo_detect(

    State(state): State<AppState>,

    Json(req): Json<SpeedTestReq>,

) -> Json<Vec<serde_json::Value>> {

    let timeout_ms = req.timeout_ms.unwrap_or(8000);

    let mut proxies: Vec<Proxy> = {

        let guard = state.store.lock().unwrap();

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

        let mut guard = state.store.lock().unwrap();

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



#[derive(Deserialize)]
pub struct TopNQuery {
    /// optional override of the configured top-N export size (0 = all nodes)
    pub n: Option<u64>,
}

/// Composite node score in [0, 100].
///
/// Combines (in priority order): availability, TCP latency (lower is better),
/// download bandwidth (higher is better) and streaming-unlock coverage. Each
/// component is normalised to [0, 1]; latency and bandwidth dominate while
/// unlock is a small bonus, so no single metric can fully dictate the ranking.
fn score_proxy(p: &Proxy) -> f64 {
    // Explicitly *failed* nodes score 0. Untested nodes (available == None)
    // are NOT zeroed — they get a neutral latency assumption so a Top-N export
    // run before any speed test does not silently drop every untested node to
    // the bottom of the ranking.
    let latency_ms = match p.available {
        Some(false) => return 0.0,
        Some(true) => p.latency_ms.unwrap_or(2000),
        None => 500, // neutral assumption for untested nodes
    };

    // Latency component: 0 ms -> 1.0, >= 2000 ms -> 0.0 (linear falloff).
    let lat = (latency_ms as f64).clamp(0.0, 2000.0);
    let latency_score = 1.0 - lat / 2000.0;

    // Bandwidth component: 0 -> 0.0, >= 50 Mbps -> 1.0 (sqrt = sub-linear).
    // NOTE: `download_speed_bps` is stored in **bytes/sec** (despite the `_bps`
    // suffix — see core/src/model.rs), so 50 Mbps == 6_250_000 bytes/sec, not
    // 50_000_000. Using the wrong figure would under-rank every measured node
    // by ~8x and corrupt Top-N export ordering. Only count it when the value
    // is a *real* engine measurement AND finite; without an engine
    // `download_speed_bps` is a raw TCP throughput estimate, and a NaN/Inf from
    // a misbehaving engine must not poison the sort.
    let bw = if p.bandwidth_measured && p.download_speed_bps.is_some_and(|b| b.is_finite()) {
        p.download_speed_bps.unwrap_or(0.0)
    } else {
        0.0
    };
    let bw_norm = (bw / 6_250_000.0).clamp(0.0, 1.0);
    let bw_score = bw_norm.sqrt();

    // Unlock coverage: fraction of detected services that report "unlocked".
    let unlock_score = match &p.unlock {
        Some(u) => {
            let total = u.services.len() as f64;
            if total == 0.0 {
                0.0
            } else {
                let ok = u.services.values().filter(|s| s.status == "unlocked").count() as f64;
                ok / total
            }
        }
        None => 0.0,
    };

    // Weighted combination; latency + bandwidth dominate, unlock is a bonus.
    let raw = 0.45 * latency_score + 0.40 * bw_score + 0.15 * unlock_score;
    (raw * 100.0).clamp(0.0, 100.0)
}


async fn list_proxies(

    State(state): State<AppState>,

    Query(q): Query<ListQuery>,

) -> Json<ProxiesResp> {

    let guard = state.store.lock().unwrap();

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

                obj.insert("sub_name".into(), serde_json::Value::String(sub.name.clone()));

                // `region` is a computed method on `Proxy`, not a serialized

                // field — surface it here so the WebUI can read `p.region`

                // (otherwise it would always fall back to "OTHER").

                obj.insert("region".into(), serde_json::Value::String(p.region()));

                // Composite score (0-100): lower latency + higher bandwidth +
                // availability + unlock coverage. Attached so the WebUI can sort
                // and filter, and the server can export the top-N.
                obj.insert("score".into(), serde_json::to_value(score_proxy(p)).unwrap_or(serde_json::Value::Null));

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
                    (Some(x), Some(y)) => {
                        x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
                    }
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



async fn dashboard(State(state): State<AppState>) -> Json<DashboardResp> {

    let guard = state.store.lock().unwrap();

    let all = flatten_dedup(&guard, None);

    let mut by_type: HashMap<String, usize> = HashMap::new();

    let mut by_region: HashMap<String, usize> = HashMap::new();

    let mut available = 0usize;

    let mut unavailable = 0usize;

    let mut untested = 0usize;

    let mut lat_sum = 0u64;

    let mut lat_n = 0u64;

    let mut best: Option<u64> = None;

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

    }

    let per_sub = guard.iter().map(sub_to_summary).collect();

    let avg_latency_ms = lat_sum.checked_div(lat_n);



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

        per_sub,

    };



    // record a rolling trend point

    {

        let mut h = state.history.lock().unwrap();

        h.push_back(TrendPoint {

            t: now_ms(),

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



    Json(resp)

}



/// Trend history (rolling window of dashboard snapshots).
async fn trends(State(state): State<AppState>) -> Json<Vec<TrendPoint>> {

    let h = state.history.lock().unwrap();

    Json(h.iter().cloned().collect())

}



/// Read global runtime settings.
async fn get_settings(State(state): State<AppState>) -> Json<SettingsResp> {

    Json(SettingsResp {

        use_proxy: *state.use_proxy.lock().unwrap(),

        auto_refresh_sec: *state.auto_refresh_sec.lock().unwrap(),

        default_fetch_proxy: state.default_fetch_proxy.lock().unwrap().clone(),

        top_n: *state.top_n.lock().unwrap(),

        engine_bin: state.engine_bin.lock().unwrap().clone(),
        remove_after_fails: *state.remove_after_fails.lock().unwrap(),
    })

}



/// Patch global runtime settings (persisted to the SQLite `meta` table).
async fn set_settings(

    State(state): State<AppState>,

    Json(req): Json<SettingsReq>,

) -> Json<SettingsResp> {

    if let Some(v) = req.use_proxy {
        *state.use_proxy.lock().unwrap() = v;
        if let Some(db) = &state.db {
            db.meta_set("use_proxy", if v { "1" } else { "0" });
        }
    }

    if let Some(sec) = req.auto_refresh_sec {

        *state.auto_refresh_sec.lock().unwrap() = sec;

        if let Some(db) = &state.db {

            db.meta_set("auto_refresh_sec", &sec.to_string());

        }

    }

    // default pull-proxy: empty string clears it (None), otherwise stored as-is
    if let Some(p) = &req.default_fetch_proxy {
        let val = p.trim();
        let next = if val.is_empty() { None } else { Some(val.to_string()) };
        *state.default_fetch_proxy.lock().unwrap() = next.clone();
        if let Some(db) = &state.db {
            db.meta_set("default_fetch_proxy", if next.is_some() { val } else { "" });
        }
    }

    // Top-N export size: persisted so it survives restarts.
    if let Some(n) = req.top_n {
        *state.top_n.lock().unwrap() = n;
        if let Some(db) = &state.db {
            db.meta_set("top_n", &n.to_string());
        }
    }

    // External engine binary: empty string clears it (engine disabled).
    if let Some(b) = &req.engine_bin {
        let val = b.trim();
        let next = if val.is_empty() { None } else { Some(val.to_string()) };
        *state.engine_bin.lock().unwrap() = next.clone();
        if let Some(db) = &state.db {
            db.meta_set("engine_bin", if next.is_some() { val } else { "" });
        }
    }

    // Auto-remove threshold: 0 disables auto-removal (the safe default).
    if let Some(n) = req.remove_after_fails {
        let n = n.min(1000); // sanity cap; 1000 consecutive failures is absurd
        *state.remove_after_fails.lock().unwrap() = n;
        if let Some(db) = &state.db {
            db.meta_set("remove_after_fails", &n.to_string());
        }
    }

    Json(SettingsResp {

        use_proxy: *state.use_proxy.lock().unwrap(),

        auto_refresh_sec: *state.auto_refresh_sec.lock().unwrap(),

        default_fetch_proxy: state.default_fetch_proxy.lock().unwrap().clone(),

        top_n: *state.top_n.lock().unwrap(),

        engine_bin: state.engine_bin.lock().unwrap().clone(),
        remove_after_fails: *state.remove_after_fails.lock().unwrap(),
    })

}



/// BestSub-style streaming-unlock detection: route each proxy through the
/// external engine and probe the streaming services, persisting the per-service
/// unlock matrix back onto each proxy. No-op (empty matrix) without an engine.
async fn unlock_detect(

    State(state): State<AppState>,

    Json(req): Json<SpeedTestReq>,

) -> Json<Vec<serde_json::Value>> {

    let timeout_ms = req.timeout_ms.unwrap_or(8000);

    let mut proxies: Vec<Proxy> = {

        let guard = state.store.lock().unwrap();

        flatten_dedup(&guard, None)

    };
    let mode = req.mode.clone().unwrap_or_default();
    if !mode.is_empty() {
        proxies = apply_mode(proxies, &mode);
    }

    let unlocks: Vec<ProxyUnlock> = if let Some(bin) = engine_bin_of(&state) {

        engine::detect_unlock(&proxies, &bin, timeout_ms, proxies.len()).await

    } else {

        vec![ProxyUnlock::default(); proxies.len()]

    };

    let now = now_ms();

    // Index results by fingerprint once (O(N)) instead of a linear `.find()`

    // per stored proxy (was O(N²)).

    let unlock_by_fp: std::collections::HashMap<String, ProxyUnlock> = proxies

        .iter()

        .zip(unlocks.iter())

        .map(|(p, u)| (p.fingerprint(), u.clone()))

        .collect();

    {

        let mut guard = state.store.lock().unwrap();

        for sub in guard.iter_mut() {

            for p in sub.proxies.iter_mut() {

                if let Some(u) = unlock_by_fp.get(&p.fingerprint()) {

                    p.unlock = Some(u.clone());

                    p.last_tested_at = Some(now);

                }

            }

        }

    }

    persist_all(&state);

    Json(

        proxies

            .iter()

            .zip(unlocks.iter())

            .map(|(p, u)| {

                serde_json::json!({

                    "name": p.name,

                    "server": p.server,

                    "unlock": u.summary(),

                })

            })

            .collect(),

    )

}



/// Resin circuit-breaker style cleanup: drop nodes that have been tested and
/// found unavailable (`available == Some(false)`) across all subscriptions.
/// Returns how many were removed. Keeps `untested` (None) nodes untouched.
async fn cleanup_bad(State(state): State<AppState>) -> Json<serde_json::Value> {

    let mut removed = 0usize;

    {

        let mut guard = state.store.lock().unwrap();

        for sub in guard.iter_mut() {

            let before = sub.proxies.len();

            sub.proxies.retain(|p| p.available != Some(false));

            removed += before - sub.proxies.len();

        }

    }

    persist_all(&state);

    Json(serde_json::json!({ "status": "ok", "removed": removed }))

}



/// Return the top-N nodes by composite score as JSON. `n` comes from the query
/// parameter and falls back to the configured `top_n` setting (0 = all nodes).
/// Each item carries the same fields as /api/proxies plus a `score`.
async fn nodes_top(
    State(state): State<AppState>,
    Query(q): Query<TopNQuery>,
) -> Json<Vec<serde_json::Value>> {
    let guard = state.store.lock().unwrap();
    let mut all: Vec<serde_json::Value> = Vec::new();
    for sub in guard.iter() {
        for p in &sub.proxies {
            let mut v = serde_json::to_value(p).unwrap_or(serde_json::Value::Null);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("sub_id".into(), serde_json::Value::String(sub.id.clone()));
                obj.insert("sub_name".into(), serde_json::Value::String(sub.name.clone()));
                obj.insert("region".into(), serde_json::Value::String(p.region()));
                obj.insert("score".into(), serde_json::to_value(score_proxy(p)).unwrap_or(serde_json::Value::Null));
            }
            all.push(v);
        }
    }
    let n = q.n.unwrap_or(*state.top_n.lock().unwrap());
    all.sort_by(|a, b| {
        let sa = a.get("score").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let sb = b.get("score").and_then(|x| x.as_f64()).unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    let take = if n == 0 { all.len() } else { n as usize };
    let items: Vec<serde_json::Value> = all.into_iter().take(take).collect();
    Json(items)
}


async fn merge_export(

    State(state): State<AppState>,

    Json(req): Json<ExportReq>,

) -> Result<Json<serde_json::Value>, (StatusCode, String)> {

    let guard = state.store.lock().unwrap();

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
    let global_top = *state.top_n.lock().unwrap();
    let top_n = req
        .top_n
        .filter(|n| *n > 0)
        .or(if global_top > 0 { Some(global_top) } else { None });
    let transformed = if let Some(n) = top_n {
        let mut scored: Vec<(f64, Proxy)> = transformed
            .into_iter()
            .map(|p| (score_proxy(&p), p))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().take(n as usize).map(|(_, p)| p).collect()
    } else {
        transformed
    };

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
/// resulting subscription as **raw text** with a client-friendly content type —/// so a proxy client (mihomo / clash / v2rayN / sing-box) can subscribe to
/// `http://127.0.0.1:3005/sub` directly. No JSON wrapper, no auth.
async fn sub_export(

    State(state): State<AppState>,

    Query(q): Query<SubQuery>,

) -> Result<impl axum::response::IntoResponse, (StatusCode, String)> {

    let only: Option<Vec<String>> = q.sub.as_ref().map(|s| {

        s.split(',')

            .map(|x| x.trim().to_string())

            .filter(|x| !x.is_empty())

            .collect()

    });

    let base = {

        let guard = state.store.lock().unwrap();

        flatten_dedup(&guard, only.as_deref())

    };

    let t = transform_from_sub_query(&q);

    let transformed = match &t {

        Some(t) => apply_transform(&base, t).map_err(|e| (StatusCode::BAD_REQUEST, e))?,

        None => base,

    };

    // Top-N trim on the standing subscription URL. A `top_n` in the URL
    // (if present and positive) overrides the global `top_n` setting; this is
    // what makes the standing subscription honour the same single Top-N
    // configuration as the manual export — both driven by the Settings page.
    let url_top = q
        .top_n
        .as_ref()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0);
    let global_top = *state.top_n.lock().unwrap();
    let top_n = url_top.or(if global_top > 0 { Some(global_top) } else { None });
    let transformed = if let Some(n) = top_n {
        let mut scored: Vec<(f64, Proxy)> = transformed
            .into_iter()
            .map(|p| (score_proxy(&p), p))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().take(n as usize).map(|(_, p)| p).collect()
    } else {
        transformed
    };

    let fmt = q.format.clone().unwrap_or_else(|| "clash-meta".to_string());

    let content = export_str(&transformed, &fmt);

    // JSON-ish formats get application/json; everything else is plain text

    // (clients parse by content, not by content-type, so text/plain is safe).

    let ct: &'static str = match fmt.as_str() {

        "v2ray" | "sing-box" | "singbox" => "application/json; charset=utf-8",

        _ => "text/plain; charset=utf-8",

    };

    Ok((

        [(header::CONTENT_TYPE, HeaderValue::from_static(ct))],

        content,

    ))

}



async fn speedtest(

    State(state): State<AppState>,

    Json(req): Json<SpeedTestReq>,

) -> Json<SpeedTestResp> {

    let timeout_ms = req.timeout_ms.unwrap_or(4000);

    let concurrency = req.concurrency.unwrap_or(20);



    // snapshot proxies (release lock before the (blocking) test)

    let mut proxies: Vec<Proxy> = {

        let guard = state.store.lock().unwrap();

        flatten_dedup(&guard, None)

    };
    let mode = req.mode.clone().unwrap_or_default();
    if !mode.is_empty() {
        proxies = apply_mode(proxies, &mode);
    }



    let (tcp, http, bw) = run_speedtest_core(&state, &proxies, timeout_ms, concurrency).await;

    let removed = persist_results(&state, &proxies, &tcp, &http, &bw);

    persist_all(&state);



    // Build the per-node result list: merge engine HTTP latency and bandwidth
    // into the TCP results. The WebUI aggregates these into a summary for
    // display, and /api/speedtest stays the source of truth for per-node data.
    // `tcp` (from `tcp_ping_all`) is ordered by *completion*, whereas
    // `http`/`bw` (from the engine tasks) are ordered by *input*. Zipping
    // them by position would assign the wrong latency/bandwidth to most
    // nodes. Align via fingerprint, the same way `persist_results` does.
    let fp_to_idx: std::collections::HashMap<String, usize> = proxies
        .iter()
        .enumerate()
        .map(|(i, p)| (p.fingerprint(), i))
        .collect();
    let mut results = tcp;
    for r in results.iter_mut() {
        if let Some(&idx) = fp_to_idx.get(&r.fingerprint) {
            if let Some(h) = http.get(idx).and_then(|x| *x) {
                r.http_latency_ms = Some(h);
            }
            // Only override the engine-free TCP throughput estimate with the
            // real engine bandwidth when the engine actually produced a
            // measurement.
            if let Some(b) = bw.get(idx).and_then(|x| *x) {
                r.download_speed_bps = Some(b);
            }
        }
    }
    Json(SpeedTestResp {
        results,
        removed,
        threshold: *state.remove_after_fails.lock().unwrap(),
    })
}



/// Validate an upstream pull-proxy by fetching a probe URL through it. Used by
/// the WebUI's "测试代理" button so the user can confirm the proxy works
/// before relying on it to fetch blocked subscriptions.
async fn proxy_test(Json(req): Json<ProxyTestReq>) -> Json<serde_json::Value> {

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



/// Standalone entry: serves the API + static WebUI on http://127.0.0.1:3005.
/// The Tauri app calls this from a background thread so the same UI is
/// available both inside the native window and from any browser.
pub async fn run_server() {

    // Durable store: open SQLite (default data/subhub.db, override with

    // SUBHUB_DB). When it can't be opened we transparently fall back to an

    // in-memory store so the app still runs.

    let db = db::Db::open(std::env::var("SUBHUB_DB").ok().map(PathBuf::from));

    let initial = db.as_ref().map(|d| d.load_all()).unwrap_or_default();

    // Rebase the subscription id counter so new ids never collide with

    // entries restored from SQLite.  Extract the numeric suffix from each

    // `sub_NN` id and set the counter to max+1.

    if !initial.is_empty() {

        let max_n = initial

            .iter()

            .filter_map(|s| s.id.strip_prefix("sub_").and_then(|n| n.parse::<u64>().ok()))

            .max()

            .unwrap_or(0);

        subhub_core::rebase_sub_counter(max_n);

    }

    eprintln!(

        "persistence: {}",

        if db.is_some() {

            "SQLite enabled"

        } else {

            "in-memory only"

        }

    );

    // Master proxy switch: default on (preserve prior behaviour). Override with

    // SUBHUB_USE_PROXY=0/false, otherwise fall back to the persisted `meta`

    // value, then to the default (true).

    let use_proxy_default = std::env::var("SUBHUB_USE_PROXY")

        .map(|v| !matches!(v.trim().to_lowercase().as_str(), "0" | "false" | "off" | "no"))

        .unwrap_or(true);

    let use_proxy = db

        .as_ref()

        .and_then(|d| d.meta_get("use_proxy"))

        .map(|v| v == "1" || v == "true")

        .unwrap_or(use_proxy_default);



    // Auto-refresh interval: env `SUBHUB_AUTO_REFRESH_SEC` sets the default;

    // a value persisted in the meta table (set from the UI) overrides it. 0 = disabled.

    let auto_refresh_default = std::env::var("SUBHUB_AUTO_REFRESH_SEC")

        .ok()

        .and_then(|s| s.parse::<u64>().ok())

        .unwrap_or(0);

    let auto_refresh_sec = db

        .as_ref()

        .and_then(|d| d.meta_get("auto_refresh_sec"))

        .and_then(|v| v.parse::<u64>().ok())

        .unwrap_or(auto_refresh_default);

    // Default pull-proxy: server-side source of truth (replaces the old
    // browser-local "remember" checkbox). Env `SUBHUB_DEFAULT_PROXY` seeds it;
    // otherwise the value persisted in the meta table (set from the UI).
    let default_fetch_proxy = db
        .as_ref()
        .and_then(|d| d.meta_get("default_fetch_proxy"))
        .filter(|v| !v.trim().is_empty())
        .or_else(|| std::env::var("SUBHUB_DEFAULT_PROXY").ok().filter(|v| !v.trim().is_empty()));



        // Top-N export size: persisted in meta (set from the UI). Default 50.
    let top_n_default: u64 = 50;
    let top_n = db
        .as_ref()
        .and_then(|d| d.meta_get("top_n"))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(top_n_default);

    // External engine binary (mihomo / sing-box compatible): the UI setting
    // persisted in meta takes precedence; SUBHUB_ENGINE_BIN env seeds it when
    // no value has been saved through the UI yet.
    let engine_bin_default = db
        .as_ref()
        .and_then(|d| d.meta_get("engine_bin"))
        .filter(|v| !v.trim().is_empty())
        .or_else(|| std::env::var("SUBHUB_ENGINE_BIN").ok().filter(|v| !v.trim().is_empty()));

    // Auto-remove threshold: persisted in meta (set from the UI). 0 = disabled.
    let remove_after_fails_default: u64 = 0;
    let remove_after_fails = db
        .as_ref()
        .and_then(|d| d.meta_get("remove_after_fails"))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(remove_after_fails_default);

    let state = AppState {

        store: Arc::new(Mutex::new(initial)),

        history: Arc::new(Mutex::new(VecDeque::new())),

        db,

        use_proxy: Arc::new(Mutex::new(use_proxy)),

        auto_refresh_sec: Arc::new(Mutex::new(auto_refresh_sec)),

        default_fetch_proxy: Arc::new(Mutex::new(default_fetch_proxy)),

        top_n: Arc::new(Mutex::new(top_n)),

        engine_bin: Arc::new(Mutex::new(engine_bin_default)),
        remove_after_fails: Arc::new(Mutex::new(remove_after_fails)),
        persist_lock: Arc::new(Mutex::new(())),


    };



    // Auto refresh: periodically re-fetch remote subscriptions and incrementally

    // update them (BestSub-style). The interval is runtime-configurable via

    // /api/settings and persisted to the meta table —no restart needed. When

    // `auto_refresh_sec` is 0 the scheduler idles (re-checked every 30s).

    {

        let st = state.clone();

        tokio::spawn(async move {

            loop {

                let sec = *st.auto_refresh_sec.lock().unwrap();

                if sec == 0 {

                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;

                    continue;

                }

                tokio::time::sleep(std::time::Duration::from_secs(sec)).await;

                // re-check in case it was disabled (or changed) while we slept

                if *st.auto_refresh_sec.lock().unwrap() == 0 {

                    continue;

                }

                let ids: Vec<String> = {

                    let g = st.store.lock().unwrap();

                    g.iter()

                        .filter(|s| s.health.source_type == "remote")

                        .map(|s| s.id.clone())

                        .collect()

                };

                for id in ids {

                    // stop early if the user disabled auto-refresh mid-cycle

                    if *st.auto_refresh_sec.lock().unwrap() == 0 {

                        break;

                    }

                    if do_refresh_one(&st, &id).await.is_some() {

                        persist_all(&st);

                    }

                }

            }

        });

    }



    let manifest = env!("CARGO_MANIFEST_DIR");

    let static_dir = PathBuf::from(manifest).join("../webui");



    let app = Router::new()
        // Bound request bodies (default axum has no limit). A paste/import of a
        // gigantic subscription list would otherwise be buffered into memory
        // unchecked — an easy local DoS. 8 MiB is far more than any real
        // subscription needs.
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))

        .route("/api/health", get(health))

        .route("/api/subscriptions", get(list_subscriptions))

        .route("/api/subscriptions", post(add_subscriptions))

        .route("/api/subscriptions/:id", delete(delete_subscription))

        .route("/api/subscriptions/:id/refresh", post(refresh_subscription))

        .route("/api/subscriptions/export", get(export_subscriptions))

        .route("/api/subscriptions/import", post(import_subscriptions))

        .route("/api/import", post(import_raw))

        .route("/api/proxies", get(list_proxies))

        .route("/api/dashboard", get(dashboard))

        .route("/api/trends", get(trends))

        .route("/api/settings", get(get_settings))

        .route("/api/settings", post(set_settings))

        .route("/api/export", post(merge_export))

        .route("/sub", get(sub_export))

        .route("/sub/", get(sub_export))

        .route("/api/speedtest", post(speedtest))

        .route("/api/proxy-test", post(proxy_test))

        .route("/api/geo-detect", post(geo_detect))

        .route("/api/unlock-detect", post(unlock_detect))

        .route("/api/nodes/cleanup", post(cleanup_bad))
        .route("/api/nodes/top", get(nodes_top))

        .with_state(state)

        .fallback_service(

            tower_http::services::ServeDir::new(static_dir)

                .append_index_html_on_directories(true),

        );



    let port: u16 = std::env::var("SUBHUB_PORT")

        .ok()

        .and_then(|s| s.parse().ok())

        .unwrap_or(3005);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    println!("SubHub server listening on http://{addr}");

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("failed to bind {addr}: {e}");
            std::process::exit(1);
        }
    };

    axum::serve(listener, app).await.unwrap();

}

#[cfg(test)]
mod tests {
    use subhub_core::{Proxy, ProxyType};

    #[test]
    fn score_untested_node_not_zeroed() {
        // P4: an untested node (available == None) must NOT score 0, otherwise
        // a Top-N export run before any speed test silently drops every node
        // to the bottom of the ranking.
        let p = Proxy::new("n".into(), ProxyType::Ss, "1.2.3.4".into(), 8388);
        let s = super::score_proxy(&p);
        assert!(s > 0.0, "untested node must get a neutral score, got {s}");
        assert!(s.is_finite(), "score must be finite");
    }

    #[test]
    fn score_failed_node_is_zero() {
        // Explicitly failed nodes still score 0.
        let mut p = Proxy::new("n".into(), ProxyType::Ss, "1.2.3.4".into(), 8388);
        p.available = Some(false);
        assert_eq!(super::score_proxy(&p), 0.0);
    }

    #[test]
    fn score_ignores_nan_bandwidth() {
        // P7: a NaN engine bandwidth must not poison the score (must stay finite).
        let mut p = Proxy::new("n".into(), ProxyType::Ss, "1.2.3.4".into(), 8388);
        p.available = Some(true);
        p.latency_ms = Some(100);
        p.bandwidth_measured = true;
        p.download_speed_bps = Some(f64::NAN);
        let s = super::score_proxy(&p);
        assert!(s.is_finite(), "score must stay finite with NaN bandwidth, got {s}");
    }

    #[test]
    fn auto_remove_after_consecutive_failures() {
        // The "remove after N consecutive failures" rule: a node is dropped only
        // once it has been found unavailable `threshold` times in a row, and a
        // single successful test resets the counter.
        use std::collections::VecDeque;
        use std::sync::Arc;
        use std::sync::Mutex;
        use subhub_core::speedtest::SpeedTestResult;
        use subhub_core::Subscription;

        let mut state = super::AppState {
            store: Arc::new(Mutex::new(vec![Subscription::new(
                "s".to_string(),
                "pasted".to_string(),
                vec![],
            )])),
            history: Arc::new(Mutex::new(VecDeque::new())),
            db: None,
            use_proxy: Arc::new(Mutex::new(false)),
            auto_refresh_sec: Arc::new(Mutex::new(0)),
            default_fetch_proxy: Arc::new(Mutex::new(None)),
            top_n: Arc::new(Mutex::new(0)),
            engine_bin: Arc::new(Mutex::new(None)),
            remove_after_fails: Arc::new(Mutex::new(3)),
            persist_lock: Arc::new(Mutex::new(())),
        };

        let mut p = Proxy::new("n".to_string(), ProxyType::Ss, "1.2.3.4".to_string(), 8388);
        p.password = Some("pw".to_string());
        p.method = Some("aes-256-gcm".to_string());
        state.store.lock().unwrap()[0].proxies.push(p.clone());

        let down = |p: &Proxy| SpeedTestResult {
            fingerprint: p.fingerprint(),
            name: p.name.clone(),
            tcp_latency_ms: None,
            http_latency_ms: None,
            available: false,
            download_speed_bps: None,
            error: Some("x".to_string()),
        };

        // Below threshold: never removed.
        assert_eq!(super::persist_results(&state, &[p.clone()], &[down(&p)], &[None], &[None]), 0);
        assert_eq!(super::persist_results(&state, &[p.clone()], &[down(&p)], &[None], &[None]), 0);
        assert_eq!(state.store.lock().unwrap()[0].proxies.len(), 1, "survives before threshold");

        // At threshold: removed this run.
        assert_eq!(super::persist_results(&state, &[p.clone()], &[down(&p)], &[None], &[None]), 1);
        assert_eq!(state.store.lock().unwrap()[0].proxies.len(), 0, "removed at threshold");

        // A success resets the counter (a flaky node is not removed after one blip).
        let mut p2 = Proxy::new("m".to_string(), ProxyType::Ss, "9.9.9.9".to_string(), 8388);
        p2.password = Some("pw".to_string());
        p2.method = Some("aes-256-gcm".to_string());
        state.store.lock().unwrap()[0].proxies.push(p2.clone());
        super::persist_results(&state, &[p2.clone()], &[down(&p2)], &[None], &[None]);
        super::persist_results(&state, &[p2.clone()], &[down(&p2)], &[None], &[None]);
        assert_eq!(state.store.lock().unwrap()[0].proxies.len(), 1, "still alive at 2 fails");
        let up = SpeedTestResult {
            fingerprint: p2.fingerprint(),
            name: p2.name.clone(),
            tcp_latency_ms: Some(50),
            http_latency_ms: None,
            available: true,
            download_speed_bps: None,
            error: None,
        };
        super::persist_results(&state, &[p2.clone()], &[up], &[None], &[None]);
        let after = &state.store.lock().unwrap()[0].proxies[0];
        assert_eq!(after.consecutive_failures, 0, "success resets counter");
    }
}

