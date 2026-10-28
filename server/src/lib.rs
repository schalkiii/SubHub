use axum::{
    extract::{DefaultBodyLimit, OriginalUri},
    http::{header, HeaderValue, StatusCode},
    response::{sse::Sse, IntoResponse, Response},
    routing::{delete, get, patch, post},
    Router,
};

use serde::{Deserialize, Serialize};

use futures::stream;
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;

use std::net::SocketAddr;

use std::path::PathBuf;

use std::sync::{Arc, Mutex};

use subhub_core::{ops::*, speedtest::SpeedTestResult, Proxy, Subscription, Transform};

mod db;

mod engine;

mod handlers;

use handlers::detect::*;
use handlers::maintenance::*;
use handlers::nodes::*;
use handlers::settings::*;
use handlers::speedtest::*;
use handlers::subscription::*;

/// Poison-tolerant mutex locking.
///
/// `Mutex::lock().unwrap()` turns a single panic while holding the lock into a
/// *permanent* denial of service: every later request that touches the same
/// mutex panics on the `PoisonError`, so one bug (e.g. the old `redact_url`
/// out-of-bounds) kills the whole server until restart. Our guarded state is
/// plain data (Vec/OTP scalars) that stays structurally valid even if a panic
/// interrupted an update, so recovering the inner value is always safe here.
/// Use `lock_ok()` instead of `lock().unwrap()` everywhere in this crate.
trait LockExt<T> {
    fn lock_ok(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_ok(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| {
            eprintln!("[subhub] warning: recovered a poisoned mutex (a previous request panicked)");
            poisoned.into_inner()
        })
    }
}

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
        if hu.is_some() {
            u.upload = hu;
        }
        if hd.is_some() {
            u.download = hd;
        }
        if ht.is_some() {
            u.total = ht;
        }
        if he.is_some() {
            u.expire = he;
        }
    }
    (u.upload, u.download, u.total, u.expire)
}

pub type Store = Arc<Mutex<Vec<Subscription>>>;

/// Blocking entry point for embedding the server (e.g. inside the Tauri app).
/// Spins up a tokio runtime and serves until the process exits.
pub fn run_blocking() {
    // Reap engine processes orphaned by a previous crash / force-kill before
    // starting a fresh server. Without this, stale mihomo children accumulate
    // across restarts and clog the same ports / network stack the new instance
    // needs — which on a machine also running Clash Verge causes its network
    // to glitch during speed tests.
    crate::engine::cleanup_stale_engine_processes();

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

    /// independent node-health re-test interval (seconds). 0 = disabled. Unlike
    /// `auto_refresh_sec` (which re-fetches subscription *content* and only tests
    /// brand-new nodes), this scheduler periodically re-probes the connectivity /
    /// latency of EVERY node so previously-tested nodes get their availability
    /// refreshed on a timer. Runtime-configurable + persisted like the above.
    pub node_health_check_sec: Arc<Mutex<u64>>,

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

    /// 连续多少天「没有任何可用节点」则自动删除该订阅（0 = 禁用）。
    /// 通过 /api/settings 配置并持久化到 meta 表；手动 `POST /api/subscriptions/prune`
    /// 与定时调度器均会据此阈值清理长期不可用的订阅。
    pub remove_sub_after_days: Arc<Mutex<u64>>,

    /// 服务器监听地址（默认 0.0.0.0，允许局域网 / 公网其他设备访问 /sub 订阅）。
    /// 可用环境变量 `SUBHUB_BIND` 覆盖（如设为 127.0.0.1 仅本机）。
    pub bind_addr: Arc<Mutex<String>>,

    /// 生成订阅网址时使用的「外部可达主机」（如 192.168.10.111）。为空时 WebUI
    /// 回退到本机探测到的局域网 IP 或 localhost。让其他设备能用局域网 / 公网地址拉取。
    pub external_host: Arc<Mutex<Option<String>>>,

    /// GitHub 用户名（用于上传 Gist 远程订阅，持久化；token 不通过 get_settings 明文返回）。
    pub github_user: Arc<Mutex<Option<String>>>,

    /// GitHub personal access token（持久化；get_settings 只回传「是否已配置」）。
    pub github_token: Arc<Mutex<Option<String>>>,

    /// 已上传 Gist 的 id（持久化）。再次上传时更新同一 Gist（类似 sub-store 行为），
    /// 远程设备拿到的是稳定不变的拉取地址。
    pub gist_id: Arc<Mutex<Option<String>>>,

    /// 串行化 Gist 上传，避免并发上传创建出多个孤儿 Gist / 让稳定地址漂移。
    /// 用 tokio Mutex 是因为 gist_upload 在持有该锁期间会 await 网络请求
    /// （std MutexGuard 非 Send，跨 await 会导致 axum Handler 不满足）。
    pub gist_lock: Arc<tokio::sync::Mutex<()>>,

    /// 启动时探测到的本机局域网 IP（只读，供 WebUI 预填外部访问地址）。
    pub lan_ip: Option<String>,

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
        .lock_ok()
        .clone()
        .filter(|v| !v.trim().is_empty())
}

/// 探测本机用于访问公网 / 局域网的出口 IP（供 WebUI 预填「外部访问地址」）。
/// 通过 UDP 连接一个公网地址并读取本地侧地址——UDP connect 不会真正发包，
/// 仅由系统选择默认出口网卡，因此瞬时且无网络开销。多网卡环境返回默认路由网卡 IP。
fn detect_lan_ip() -> Option<String> {
    use std::net::UdpSocket;
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    let addr = sock.local_addr().ok()?;
    let ip = addr.ip().to_string();
    // 排除回环与未分配地址
    if ip == "0.0.0.0" || ip.starts_with("127.") {
        None
    } else {
        Some(ip)
    }
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
pub struct RenameSubReq {
    /// new display name for the subscription (trimmed; must be non-empty).
    pub name: String,
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

    /// global sort key applied BEFORE pagination:
    /// name | latency | tcplat | httplat | speed | score
    pub sort: Option<String>,

    /// sort descending when true; ascending otherwise (default false).
    /// Parsed as a string so both `desc=1` (documented) and `desc=true`
    /// (WebUI) work — serde rejects `1` for a bool.
    pub desc: Option<String>,
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

    /// 数值质量阈值（可选，单位 bps / ms）：导出时排除带宽低于 `min_bw` 或延迟
    /// 高于 `max_lat` 的节点。与手动导出 `Transform.min_bandwidth_bps` /
    /// `max_latency_ms` 对应，便于把「只要快节点」编码进常驻订阅网址。
    pub min_bw: Option<String>,

    pub max_lat: Option<String>,
}

#[derive(Deserialize)]

pub struct SpeedTestReq {
    pub timeout_ms: Option<u64>,

    pub concurrency: Option<usize>,

    /// Test-scope filter for the "仅测未测 / 仅失败" WebUI selector:
    /// - `None` / `""` / `"all"` -> test every node (default)
    /// - `"untested"` -> only nodes that have no latency data yet
    ///   (`tcp_latency_ms == None && http_latency_ms == None`), i.e. nodes
    ///   still showing "—" in both columns regardless of `last_tested_at`.
    /// - `"failed"`   -> only nodes currently marked unavailable
    ///   (`available == Some(false)`)
    ///
    /// Mirrors the incremental behaviour of the auto-refresh path so re-runs
    /// don't re-hammer already-known nodes.
    pub mode: Option<String>,

    /// Restrict the manual speedtest to specific nodes, identified by their
    /// `fingerprint`. The value is a **URL-encoded JSON array** of fingerprints
    /// (`ids=["<fp1>","<fp2>"]`), NOT a comma-joined string: fingerprints
    /// themselves contain commas (e.g. a vless+ws `path` field), so a comma
    /// delimiter would fragment them and silently match nothing — breaking the
    /// WebUI "测速选中节点" multi-select for exactly those nodes. When absent or
    /// empty, every node is tested. Fingerprints not found in the store are
    /// silently ignored.
    pub ids: Option<String>,
}

/// Live speed-test progress event, streamed to the WebUI over SSE so the test
/// view can show a progress bar and the node currently being reported. One
/// `Progress` is emitted per completed node; a single `Done` closes the stream
/// with the run summary. `type` is the SSE event name (`progress` / `done`).
#[derive(Serialize)]
#[serde(tag = "type")]
enum SpeedEvent {
    Progress(ProgressView),
    Done(SummaryView),
}

/// One node's in-progress result (emitted as the `progress` SSE event).
#[derive(Serialize)]
struct ProgressView {
    done: usize,
    total: usize,
    name: String,
    /// 节点 fingerprint —— 前端据此定位表格行并就地 patch（实时更新）。
    #[serde(skip_serializing_if = "Option::is_none")]
    fingerprint: Option<String>,
    available: bool,
    latency_ms: Option<u64>,
    /// TCP 延迟（TCP 阶段填入，供前端实时刷新 TCP 延迟列）。
    #[serde(skip_serializing_if = "Option::is_none")]
    tcp_latency_ms: Option<u64>,
    /// HTTP 延迟（引擎阶段填入，供前端实时刷新 HTTP 延迟列 + 可用状态）。
    #[serde(skip_serializing_if = "Option::is_none")]
    http_latency_ms: Option<u64>,
    bandwidth_bps: Option<f64>,
    /// Node host, so the WebUI can flag loopback/localhost addresses (their
    /// bandwidth is local memory copy, not a real proxy speed).
    server: String,
    /// Which stage produced this tick: "tcp" (TCP latency) or "http"
    /// (engine HTTP latency). Lets the WebUI label the live row correctly.
    phase: String,
}

/// Final summary (emitted as the `done` SSE event) — mirrors the old
/// per-node result shape so the WebUI can render the same post-run message.
#[derive(Serialize)]
struct SummaryView {
    tested: usize,
    reachable: usize,
    avg_latency_ms: Option<u64>,
    with_http: usize,
    removed: usize,
    threshold: u64,
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

    /// node-health re-test interval in seconds (0 = disabled). Independent of
    /// `auto_refresh_sec`: re-probes every node's connectivity/latency on a timer.
    pub node_health_check_sec: u64,

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

    /// 服务器监听地址（默认 0.0.0.0，允许局域网 / 公网访问）
    pub bind_addr: String,

    /// 生成订阅网址时使用的外部可达主机（如 192.168.10.111）；为空表示使用本机 IP
    pub external_host: Option<String>,

    /// GitHub 用户名（用于上传 Gist 远程订阅）
    pub github_user: Option<String>,

    /// 是否已配置 GitHub token（true = 已保存；出于安全不返回明文）
    pub has_github_token: bool,

    /// 连续多少天「没有任何可用节点」则自动删除该订阅（0 = 禁用）
    pub remove_sub_after_days: u64,

    /// 启动时探测到的本机局域网 IP（只读，供 WebUI 预填外部访问地址）
    pub lan_ip: Option<String>,
}

#[derive(Deserialize)]

pub struct SettingsReq {
    /// optional: when present, updates the auto-refresh interval (seconds)
    pub auto_refresh_sec: Option<u64>,

    /// optional: when present, updates the node-health re-test interval (seconds)
    pub node_health_check_sec: Option<u64>,

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

    /// optional: when present, updates the "auto-remove a subscription after N
    /// consecutive days with zero available nodes" threshold (0 = disabled)
    pub remove_sub_after_days: Option<u64>,

    /// optional: when present, updates the server bind address (e.g. "0.0.0.0"
    /// for LAN / "127.0.0.1" for localhost only). Takes effect after restart.
    pub bind_addr: Option<String>,

    /// optional: when present, updates the external host used in generated
    /// subscription URLs (e.g. "192.168.10.111"). Empty string clears it.
    pub external_host: Option<String>,

    /// optional: when present, updates the GitHub username for Gist upload.
    /// Empty string clears it.
    pub github_user: Option<String>,

    /// optional: when present, updates the GitHub personal access token for
    /// Gist upload. Empty string clears it. Never returned in get_settings.
    pub github_token: Option<String>,
}

/// One subscription as stored in an export file. `proxies` carries the full
/// node list (including any previously measured latency/bandwidth), so the
/// file is self-contained and re-importable offline.
#[derive(Serialize, Deserialize, Clone)]
pub struct SubExportItem {
    /// Informational id (sequential `sub_N`). Import matches subscriptions by
    /// `source` URL, never by this id, so a crafted file cannot overwrite an
    /// unrelated subscription; a re-imported remote sub keeps its local id.
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
                b = b.proxy(reqwest::Proxy::all(p).map_err(|e| format!("代理地址无效 {p}: {e}"))?);
            }
        }

        None => {
            b = b.no_proxy();
        }
    }

    b.build().map_err(|e| format!("创建请求客户端失败: {e}"))
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
    // Scheme allow-list: subscription sources must be plain http/https. A
    // `file://`, `ftp://` or other exotic scheme in a user-supplied URL is
    // either a typo or an SSRF/local-file-read attempt — reject it up front
    // with a clear message instead of letting reqwest produce an opaque
    // builder error (or, worse, a future reqwest feature actually fetch it).
    let lower = url.trim().to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(format!(
            "不支持的订阅 URL 协议（仅支持 http/https）: {}",
            redact_url(url)
        ));
    }
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
    if without_query.contains('@') {
        if let Some(scheme_end) = without_query.find("://") {
            let (scheme, rest) = without_query.split_at(scheme_end + 3);
            // Keep everything after the first '@' *inside `rest`*. The old
            // code indexed `rest` with an offset computed on `without_query`
            // (`at + 1`), which is out of range whenever the scheme prefix is
            // longer than the userinfo — e.g. "https://a@b" → rest="a@b"
            // (len 3) but at=9 → `rest[10..]` panics, poisoning the store
            // mutex and killing every subsequent request. `split_once` is
            // offset-free and cannot panic.
            let host_and_path = match rest.split_once('@') {
                Some((_userinfo, host)) => host,
                None => rest, // '@' was in the scheme part only (malformed URL)
            };
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
) -> (
    Vec<SpeedTestResult>,
    Vec<Option<Option<u64>>>,
    Vec<Option<Option<f64>>>,
) {
    let proxies_tcp = proxies.to_vec();

    // TCP 阶段的两处针对性调优。实测 250 节点全量测速：TCP 阶段独占 ~190s，
    // 而 engine HTTP 探测整批只要 ~76s —— TCP 才是全量测速的时间大头。
    //
    // 1) 更短的超时：TCP 握手要么很快完成、要么在 1~2s 内明确失败，给它完整
    //    的 `timeout_ms`（默认 8s）纯属浪费。截断到 3s，握手成功与否不受影响。
    let tcp_timeout = timeout_ms.min(3_000);
    // 2) 更高的并发：`concurrency` 是给**引擎探测**用的（每个节点要经代理
    //    发真实 HTTP 请求，压力重），但 TCP ping 只是一次裸握手，轻得多。
    //    沿用引擎的并发会让 250 节点排 31 波（实测 UI 传 8），光排队就吃掉
    //    绝大部分时间。抬高到至少 32 让 TCP 阶段快约 4 倍。
    let tcp_concurrency = concurrency.max(32);

    let t_tcp = std::time::Instant::now();
    let tcp_results = tokio::task::spawn_blocking(move || {
        subhub_core::tcp_ping_all(&proxies_tcp, tcp_timeout, tcp_concurrency, None)
    })
    .await
    .unwrap_or_default();
    eprintln!(
        "[speedtest][perf] TCP 阶段: {:.1}s ({} 节点, timeout={}ms, conc={})",
        t_tcp.elapsed().as_secs_f64(),
        proxies.len(),
        tcp_timeout,
        tcp_concurrency
    );

    let t_engine = std::time::Instant::now();
    let (engine_http, engine_bw) =
        run_engine_passes(state, proxies, timeout_ms, concurrency, None, None).await;
    eprintln!(
        "[speedtest][perf] engine 阶段: {:.1}s",
        t_engine.elapsed().as_secs_f64()
    );

    (tcp_results, engine_http, engine_bw)
}

/// Engine HTTP-latency + bandwidth passes (run concurrently, share the same
/// clamped engine-worker budget). Returns `(http_latency, bandwidth)` aligned
/// with `proxies` order. Skipped entirely (all `None`) when no engine binary is
/// configured — in that case the caller falls back to the TCP throughput estimate.
async fn run_engine_passes(
    state: &AppState,

    proxies: &[Proxy],

    timeout_ms: u64,

    concurrency: usize,

    on_http: Option<Arc<dyn Fn(engine::EngineProgress) + Send + Sync>>,

    on_bw: Option<Arc<dyn Fn(String) + Send + Sync>>,
) -> (Vec<Option<Option<u64>>>, Vec<Option<Option<f64>>>) {
    let engine_bin = engine_bin_of(state);

    // 每个节点只拉起**一个** engine 进程，在同一进程内先做存活延迟、再实测带宽。
    // 重构前两个 pass 各自为每个节点拉一个进程（共 2N 个），且并行执行会让同时
    // 存活的引擎进程翻倍（可达 8，与 MAX_ENGINE_PROCESSES=4 的硬上限声明不符）。
    // 复用进程后：进程数减半、冷启动（单次最长 15s）减半、并发上限真正生效。
    // on_http / on_bw 仍分别在**对应探测项完成**时回调，SSE 分阶段进度语义不变。
    if let Some(bin) = engine_bin {
        // 带宽下载用独立的大超时：默认 speedtest `timeout_ms`(4s) 是给 gstatic
        // 204 这种小存活探测用的，远不够拉完一个测速分片（原 100MB 在 4s 内几乎
        // 必超时）。这里至少给 30s，让 25MB/10MB/4MB 分片在受限节点上也能下完，
        // 产出现实带宽而非静默 `None`。
        let bw_timeout = timeout_ms.max(30_000);

        let probes = engine::probe_all(engine::ProbeTask {
            proxies,
            bin: &bin,
            timeout_ms,
            bandwidth_timeout_ms: bw_timeout,
            concurrency,
            plan: engine::ProbePlan {
                http_latency: true,
                // 带宽下载探测已关闭：用户只需「Google 204 真实连通」即可判定实测，
                // 下载测速分片既慢（30s 超时）又常因节点出口受限而失败。保留 http_latency
                // 即完成「实测」信号采集，测速速度大幅提升。
                bandwidth: false,
                ..Default::default()
            },
            on_http,
            on_bw,
        })
        .await;

        let mut h = Vec::with_capacity(probes.len());
        let mut b = Vec::with_capacity(probes.len());
        for p in probes {
            h.push(p.http_latency);
            b.push(p.bandwidth);
        }

        (h, b)
    } else {
        (vec![None; proxies.len()], vec![None; proxies.len()])
    }
}

/// Persist speed-test results back onto the matching proxies (by fingerprint)
/// and stamp `last_tested_at`. Used by both the manual speedtest endpoint and
/// the automatic post-add test so there is a single source of truth.
/// 判断节点 server 是否为回环/本地/未指定地址字面量。
///
/// 命中（如 127.0.0.53 这类 DNS stub、127.0.0.1、::1、0.0.0.0、localhost、
/// `[::1]`）的节点绕过了真实网络，测速会得到虚假带宽/延迟，写入结果时应跳过。
/// 仅对可静态解析的 IP/主机字面量生效；域名（如 example.com）无法判断则返回 false。
fn is_loopback_server(server: &str) -> bool {
    let s = server.trim().trim_matches(|c| c == '[' || c == ']');
    if s.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match s.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback() || ip.is_unspecified(),
        Err(_) => false,
    }
}

/// Persist speed-test results back onto the matching proxies (by fingerprint)
/// and stamp `last_tested_at`. Used by both the manual speedtest endpoint and
/// the automatic post-add test so there is a single source of truth.
fn persist_results(
    state: &AppState,

    proxies: &[Proxy],

    tcp: &[SpeedTestResult],

    http: &[Option<Option<u64>>],

    bw: &[Option<Option<f64>>],
) -> usize {
    let now = now_ms();

    let fp_to_idx: std::collections::HashMap<String, usize> = proxies
        .iter()
        .enumerate()
        .map(|(i, p)| (p.fingerprint(), i))
        .collect();

    // 可用性判定（对齐 clash-verge）：逐节点以引擎结果 h.is_some() 为权威——
    // 只要该节点本轮确实跑了引擎探测（HTTP 或带宽任一有结果），TCP 能连端口
    // 但代理隧道不通的节点会被正确判红（如 35.187.156.27:443 这类 Google 443
    // 端口）。仅当该节点本轮完全没跑引擎（全局未配置引擎，或自身不可导出被
    // 引擎跳过）时，才回退裸 TCP，避免「引擎未配置」场景下把节点全判红。

    // (latency_ms, tcp_available, engine_http_ms, download_speed_bps, bandwidth_measured, ran_engine)
    // ran_engine = 本轮引擎是否确实尝试过该节点（http/bw 外层为 Some）。仅当尝试过才以
    // 引擎结论为准；尝试过但探测失败（内层 None）即判不可用，避免回退不稳的裸 TCP 翻转。
    type PersistRow = (
        Option<u64>,
        bool,
        Option<u64>,
        Option<f64>,
        bool,
        bool,
        Option<u64>,
    );

    let mut by_fp: std::collections::HashMap<String, PersistRow> = std::collections::HashMap::new();

    for r in tcp {
        let idx = fp_to_idx.get(&r.fingerprint).copied();

        // http/bw 现在是 `Option<Option<_>>`（来自 run_engine_passes）：
        //   Some(Some(v)) = 引擎跑了且探测成功（v 为值）
        //   Some(None)    = 引擎跑了但探测失败（节点死 / 目标不可达）
        //   None          = 引擎未运行（无引擎二进制 / 节点不可导出被跳过）
        // 外层 Some 即「本轮引擎确实尝试过该节点」信号——旧代码 flatten 会丢弃它，
        // 导致引擎失败节点回退不稳的裸 TCP、可用/不可用反复翻转。此处保留。
        let h_attempted = idx
            .and_then(|i| http.get(i))
            .map(|x| x.is_some())
            .unwrap_or(false);
        let bw_attempted = idx
            .and_then(|i| bw.get(i))
            .map(|x| x.is_some())
            .unwrap_or(false);
        let ran_engine = h_attempted || bw_attempted;
        let h = idx.and_then(|i| http.get(i)).and_then(|x| *x).flatten();
        let lat = r.tcp_latency_ms.or(h);

        // 「实测」信号由「真实代理连通」决定：引擎确认存活——HTTP 访问 Google 204 通过
        // （h.is_some()），或真实带宽下载成功（bw.flatten().is_some()）。不再仅靠带宽下载，
        // 避免节点出口到测速分片目标受限时全部回退成 TCP 假估算（0.0MB/s / 未实测）。
        // 真实带宽只在引擎确实下到分片时存在；带宽探测关闭（仅做 204 连通校验）时为 None，
        // UI 据此显示「实测连通」而非编造的带宽值。
        let http_ok = h.is_some();
        let measured = http_ok
            || idx
                .and_then(|i| bw.get(i))
                .and_then(|x| *x)
                .flatten()
                .is_some();
        let dl = idx.and_then(|i| bw.get(i)).and_then(|x| *x).flatten();

        // Store raw TCP availability + engine HTTP result + 是否跑过引擎，供下方
        // 逐节点判定；最终 `available` 在第二个循环里决定（需要 ran_engine）。
        by_fp.insert(
            r.fingerprint.clone(),
            (
                lat,
                r.available,
                h,
                dl,
                measured,
                ran_engine,
                r.tcp_latency_ms,
            ),
        );
    }

    let threshold = *state.remove_after_fails.lock_ok();

    let mut removed: usize = 0;

    // 引擎是否配置：决定可用性判定以「HTTP 实测通过」为准，还是退化到 TCP 可达。
    let engine_configured = engine_bin_of(state).is_some();

    let mut guard = state.store.lock_ok();

    for sub in guard.iter_mut() {
        for p in sub.proxies.iter_mut() {
            if let Some((lat, tcp_avail, h, dl, measured, ran_engine, tcp_ms)) =
                by_fp.get(&p.fingerprint())
            {
                // 回环/本地节点（如 127.0.0.53）走本地回环会得到虚假带宽/延迟，
                // 直接跳过所有测速结果，标记为「未测」以免污染健康度与速度列。
                if is_loopback_server(&p.server) {
                    p.available = None;
                    p.latency_ms = None;
                    p.tcp_latency_ms = None;
                    p.http_latency_ms = None;
                    p.download_speed_bps = None;
                    p.bandwidth_measured = false;
                    continue;
                }

                // 逐节点可用性：以「真实代理连通」为唯一标准 —— 引擎确认能访问 Google 204。
                // 配置了引擎时必须 HTTP 实测通过（ran_engine && h.is_some()）才算可用，TCP 不再兜底：
                // TCP 能连上端口但 204 不通，说明代理隧道并没打通，这类节点在 clash 里同样连不上，
                // 若仍用 TCP 兜底标成「可用」，就会出现误导性的「可用·未实测」（曾经的
                // 🇺🇸美国-电信|5倍 即属此例）。引擎未覆盖的节点（不可导出被跳过）同样算不可用 ——
                // 无法验证代理连通，就不该标可用。
                // 未配置引擎时 TCP 是唯一能拿到的信号，退化为 TCP 可达即可用。
                // 关键：`ran_engine == false` 表示本轮**没有跑引擎**——即来自
                // `run_health_check_core` 的 TCP-only 定时健康检查（它只做裸
                // TCP ping，engine HTTP 结果整组传 None）。此时绝不能沿用
                // 「以引擎结论为准」的判定：h 恒为 None，会把**每一个**节点
                // 都判成不可用，一轮后台健康检查就清空整个面板的可用节点，
                // 同时把已测得的 http_latency_ms 抹成 None。
                // 用户实测可复现：手动测速后 41 个节点可用，健康检查跑完后
                // 全库 HTTP 延迟归零、所有订阅显示「无可用节点」。
                // TCP-only 模式（已配置引擎，但本轮只跑了裸 TCP —— 典型来源是
                // `run_health_check_core` 的定时健康检查）：本轮没有引擎结论，
                // 只允许「降级」，不允许把未测节点（None）改成其它状态，也不
                // 累加连续失败计数。否则一轮后台健康检查就会把刚导入、尚未
                // 实测的节点统统一成不可用，并最终触发自动移除。
                if engine_configured && !*ran_engine {
                    p.last_tested_at = Some(now);
                    p.tcp_latency_ms = *tcp_ms;
                    if !*tcp_avail {
                        p.available = Some(false);
                        p.latency_ms = None;
                        p.download_speed_bps = None;
                        p.bandwidth_measured = false;
                        p.consecutive_failures = p.consecutive_failures.saturating_add(1);
                    }
                    continue;
                }

                // 完整测速：配置引擎时以引擎实测为准（TCP 不兜底），
                // 未配置引擎时 TCP 是唯一信号。
                let avail = if engine_configured { h.is_some() } else { *tcp_avail };
                p.available = Some(avail);
                p.last_tested_at = Some(now);

                // 判定不可用时清除上一轮残留的延迟/带宽，避免「旧绿数据」
                // 继续误导。TCP / HTTP 延迟是客观测量值，仍如实记录：
                // 用户靠它们区分「完全不可达」与「TCP 通但隧道不通」。
                p.tcp_latency_ms = *tcp_ms;

                // 只有本轮真正跑过引擎才更新 HTTP 延迟，避免无引擎覆盖实测值。
                if *ran_engine {
                    p.http_latency_ms = *h;
                }

                if avail {
                    p.latency_ms = *lat;
                    // 同理：只有跑过引擎才有带宽结论可依据。
                    if *ran_engine {
                        if let Some(b) = dl {
                            p.download_speed_bps = Some(*b);
                        } else {
                            p.download_speed_bps = None;
                        }
                        p.bandwidth_measured = *measured;
                    }
                    // Consecutive-failure tracking for auto-remove: any
                    // success resets the run.
                    p.consecutive_failures = 0;
                } else {
                    p.latency_ms = None;
                    p.download_speed_bps = None;
                    p.bandwidth_measured = false;
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

        // 维护「最后有可用节点」时间戳：只要有任一节点当前可用，即刷新该时间。
        // 供「连续 N 天无可用节点则删除订阅」判断订阅已持续不可用多久。保留旧值
        // 直到重新测出可用节点，避免未测速 / 刷新拉到一群未测节点就被误判为「现在无可用」。
        if sub.proxies.iter().any(|p| p.available == Some(true)) {
            sub.health.last_healthy_at = Some(now);
        }
    }

    removed
}

/// Snapshot the whole in-memory store into the durable DB (if configured).
/// No-op when persistence is disabled. Safe to call after any mutation so the
/// user never loses subscriptions / speed-test results on restart.
fn persist_all(state: &AppState) {
    if let Some(db) = &state.db {
        let subs = state.store.lock_ok().clone();

        // Serialize DB writes so concurrent `save_all` transactions can't
        // interleave (DELETE + full re-INSERT) and corrupt the store.
        let _g = state.persist_lock.lock_ok();
        db.save_all(&subs);
    }
}

/// Periodic node-health re-test (the "节点健康定时重测" scheduler). Unlike a
/// full speedtest it runs the engine-free TCP ping only (latency +
/// reachability), which is the core health signal and stays bounded by
/// `tcp_ping_all`'s internal worker queue — a large node set is tested as a
/// controlled stream rather than all sockets at once. The external engine
/// (real HTTP latency / bandwidth) is intentionally skipped here so the
/// background loop stays light instead of spawning one engine process per node.
/// The `remove_after_fails` threshold still applies, so nodes that fail N
/// consecutive checks get pruned by this path too.
async fn run_health_check_core(
    state: &AppState,
    proxies: &[Proxy],
    timeout_ms: u64,
    concurrency: usize,
) -> usize {
    if proxies.is_empty() {
        return 0;
    }
    let proxies_tcp = proxies.to_vec();
    let tcp_results = tokio::task::spawn_blocking(move || {
        subhub_core::tcp_ping_all(&proxies_tcp, timeout_ms, concurrency, None)
    })
    .await
    .unwrap_or_default();
    // TCP-only: no engine HTTP latency / bandwidth to fold in.
    let none_u64: Vec<Option<Option<u64>>> = vec![None; proxies.len()];
    let none_f64: Vec<Option<Option<f64>>> = vec![None; proxies.len()];
    persist_results(state, proxies, &tcp_results, &none_u64, &none_f64)
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

    /// 平均带宽（bps）；仅统计已测速且有带宽数据的节点。未配置测速引擎时
    /// 该值为 TCP 吞吐估算，并非真实代理带宽。
    pub avg_bandwidth_bps: Option<f64>,

    /// 最佳（最大）带宽（bps），取所有已测速节点中的最大值。
    pub best_bandwidth_bps: Option<f64>,

    /// 平均 HTTP 延迟（ms）：仅统计经代理实测到 Google 204 的节点
    /// （`http_latency_ms` 有值），未实测节点不计入。
    pub avg_http_latency_ms: Option<u64>,

    /// 最佳（最小）HTTP 延迟（ms），取实测连通节点中的最小值。
    pub best_http_latency_ms: Option<u64>,

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
        // 以「确实没有延迟数据」判定未测过，而非「没打过时间戳」：
        // 节点可能已被测速流程跑过（last_tested_at 有值）却因 TCP 超时 /
        // 未跑引擎而 tcp/http 全是空（UI 上即 "— —"）。这类节点对用户而言
        // 仍是未测，必须被「仅未测过」纳入，否则会出现「看着没数据却测不到」的困惑。
        "untested" => out.retain(|p| p.tcp_latency_ms.is_none() && p.http_latency_ms.is_none()),
        "failed" => out.retain(|p| p.available == Some(false)),
        _ => {}
    }
    out
}

/// Parse the `desc` query parameter. The HTTP API documents `desc=1`; the
/// WebUI sends `desc=true`. Both mean descending.
pub(crate) fn parse_desc(v: &Option<String>) -> bool {
    v.as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .is_some_and(|s| s == "1" || s == "true" || s == "yes")
}

/// Resolve the effective Top-N from an explicit request parameter and the
/// global setting.
///
/// A present parameter always wins, including `0` (= no limit, "all nodes");
/// only an absent parameter falls back to the global setting. Callers that
/// documented `top_n=0` as an override depend on this distinction.
pub(crate) fn resolve_top_n(param: Option<u64>, global: u64) -> Option<u64> {
    match param {
        Some(0) => None,
        Some(n) => Some(n),
        None => (global > 0).then_some(global),
    }
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

    // 数值质量阈值：min_bw（带宽下限，bps）/ max_lat（延迟上限，ms）。解析失败的
    // 值视为「未设置」，不影响其余筛选。
    let min_bandwidth_bps = q.min_bw.as_ref().and_then(|s| s.parse::<u64>().ok());
    let max_latency_ms = q.max_lat.as_ref().and_then(|s| s.parse::<u64>().ok());

    if filters.is_empty()
        && sort.is_none()
        && rename.is_none()
        && min_bandwidth_bps.is_none()
        && max_latency_ms.is_none()
    {
        None
    } else {
        Some(Transform {
            filters,

            sort,

            rename,

            min_bandwidth_bps,

            max_latency_ms,
        })
    }
}

// ----------------------- handlers -----------------------

/// Request for the single-node raw YAML endpoint: resolve one node by its
/// stable `fingerprint` and return the exact clash-meta YAML SubHub feeds to the
/// speed-test engine (and therefore exports). Lets the user copy the precise
/// config into another client (e.g. clash-verge-rev) to 1:1 verify whether a
/// "SubHub can use it but client X cannot" divergence is a config/version
/// difference on the client side.
#[derive(Deserialize)]
struct SingleFp {
    fp: String,
}

/// Request to delete a single node from a subscription. `fingerprint` is the
/// stable per-node identity produced by `Proxy::fingerprint` (surfaced by
/// `GET /api/proxies` as the `fingerprint` field), so renaming a node never
/// breaks the match.
#[derive(Deserialize)]
struct DeleteProxyReq {
    sub_id: String,
    fingerprint: String,
}

#[derive(Deserialize)]
pub struct TopNQuery {
    /// optional override of the configured top-N export size (0 = all nodes)
    pub n: Option<u64>,
}

/// 上传 / 更新订阅到 GitHub Gist，供远程设备拉取（类似 sub-store 的 Gist 同步）。
/// 首次创建 Gist，之后复用 `state.gist_id` 更新同一 Gist，远程拉取地址保持稳定。
#[derive(Deserialize)]
struct GistUploadReq {
    // 与 SubQuery 同款的订阅生成参数
    format: Option<String>,
    sub: Option<String>,
    sort: Option<String>,
    desc: Option<String>,
    rename_pat: Option<String>,
    rename_rep: Option<String>,
    q: Option<String>,
    region: Option<String>,
    r#type: Option<String>,
    top_n: Option<String>,
    min_bw: Option<String>,
    max_lat: Option<String>,
    // 可选凭据覆盖（缺省时回退到「设置」页持久化的值）
    github_user: Option<String>,
    github_token: Option<String>,
}

#[derive(Serialize)]
struct GistUploadResp {
    ok: bool,
    /// 远程设备直接拉取的原始地址（始终取最新版本）
    url: String,
    gist_id: String,
    html_url: String,
}

// ----------------------- static WebUI hosting -----------------------

/// WebUI 磁盘副本位置。优先级：`SUBHUB_WEBUI_DIR` → 源码目录 → exe 同级
/// `webui/`（含 Tauri 资源目录布局）。开发时命中源码目录，改前端重启即生效；
/// 发行版脱离源码时返回 `None`，由内嵌副本兜底。
fn webui_dir() -> Option<&'static PathBuf> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        if let Ok(p) = std::env::var("SUBHUB_WEBUI_DIR") {
            let p = PathBuf::from(p);
            if p.is_dir() {
                return Some(p);
            }
        }
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../webui");
        if source.is_dir() {
            return Some(source);
        }
        if let Ok(exe) = std::env::current_exe() {
            let dir = exe.parent().unwrap_or_else(|| std::path::Path::new("."));
            for cand in [
                dir.join("webui"),
                dir.join("../webui"),
                dir.join("resources/webui"),
                dir.join("_up_/webui"),
            ] {
                if cand.is_dir() {
                    return Some(cand);
                }
            }
        }
        None
    })
    .as_ref()
}

// 内嵌副本保证 exe 分发到没有源码 / 资源目录的机器上时 WebUI 仍可用。
const EMBEDDED_INDEX_HTML: &[u8] = include_bytes!("../../webui/index.html");
const EMBEDDED_APP_JS: &[u8] = include_bytes!("../../webui/app.js");
const EMBEDDED_STYLE_CSS: &[u8] = include_bytes!("../../webui/style.css");

fn asset_content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn static_response(path: &str, bytes: Vec<u8>) -> Response {
    let mut resp = bytes.into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset_content_type(path)),
    );
    // WebUI 改动后重启即应生效，禁止 webview 缓存旧 app.js/style.css。
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    );
    resp
}

/// 静态资源入口：磁盘优先（开发热更新）→ 内嵌兜底（发行版自包含）。
async fn static_handler(OriginalUri(uri): OriginalUri) -> Response {
    let rel = uri.path().trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let safe = !rel.contains('\\')
        && !rel.contains(':')
        && !rel
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..");
    if safe {
        if let Some(dir) = webui_dir() {
            if let Ok(bytes) = std::fs::read(dir.join(rel)) {
                return static_response(rel, bytes);
            }
        }
        let embedded: Option<&'static [u8]> = match rel {
            "index.html" => Some(EMBEDDED_INDEX_HTML),
            "app.js" => Some(EMBEDDED_APP_JS),
            "style.css" => Some(EMBEDDED_STYLE_CSS),
            _ => None,
        };
        if let Some(bytes) = embedded {
            return static_response(rel, bytes.to_vec());
        }
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
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
            .filter_map(|s| {
                s.id.strip_prefix("sub_")
                    .and_then(|n| n.parse::<u64>().ok())
            })
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
        .map(|v| {
            !matches!(
                v.trim().to_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        })
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

    // Node-health re-test interval: env `SUBHUB_HEALTH_CHECK_SEC` sets the
    // default; a value persisted in meta (set from the UI) overrides it.
    // 0 = disabled. Independent of the subscription-content auto-refresh above.
    let node_health_check_default = std::env::var("SUBHUB_HEALTH_CHECK_SEC")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    let node_health_check_sec = db
        .as_ref()
        .and_then(|d| d.meta_get("node_health_check_sec"))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(node_health_check_default);

    // Default pull-proxy: server-side source of truth (replaces the old
    // browser-local "remember" checkbox). Env `SUBHUB_DEFAULT_PROXY` seeds it;
    // otherwise the value persisted in the meta table (set from the UI).
    let default_fetch_proxy = db
        .as_ref()
        .and_then(|d| d.meta_get("default_fetch_proxy"))
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("SUBHUB_DEFAULT_PROXY")
                .ok()
                .filter(|v| !v.trim().is_empty())
        });

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
        .or_else(|| {
            std::env::var("SUBHUB_ENGINE_BIN")
                .ok()
                .filter(|v| !v.trim().is_empty())
        });

    // Auto-remove threshold: persisted in meta (set from the UI). 0 = disabled.
    let remove_after_fails_default: u64 = 0;
    let remove_after_fails = db
        .as_ref()
        .and_then(|d| d.meta_get("remove_after_fails"))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(remove_after_fails_default);

    // 连续 N 天无可用节点则删除订阅：持久化在 meta（来自 UI）。0 = 禁用。
    let remove_sub_after_days_default: u64 = 0;
    let remove_sub_after_days = db
        .as_ref()
        .and_then(|d| d.meta_get("remove_sub_after_days"))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(remove_sub_after_days_default);

    // 绑定地址：env `SUBHUB_BIND` 设默认值（默认 0.0.0.0 = 允许局域网 / 公网访问）；
    // 持久化在 meta 的值（来自 UI）优先于默认值。
    let bind_addr_default = std::env::var("SUBHUB_BIND")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "0.0.0.0".to_string());
    let bind_addr = db
        .as_ref()
        .and_then(|d| d.meta_get("bind_addr"))
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(bind_addr_default);

    // 外部访问主机（生成订阅网址用）：持久化在 meta，无则空（回退本机 IP）。
    let external_host = db
        .as_ref()
        .and_then(|d| d.meta_get("external_host"))
        .filter(|v| !v.trim().is_empty());

    let github_user = db
        .as_ref()
        .and_then(|d| d.meta_get("github_user"))
        .filter(|v| !v.trim().is_empty());

    let github_token = db
        .as_ref()
        .and_then(|d| d.meta_get("github_token"))
        .filter(|v| !v.trim().is_empty());

    let gist_id = db
        .as_ref()
        .and_then(|d| d.meta_get("gist_id"))
        .filter(|v| !v.trim().is_empty());

    // 探测本机局域网出口 IP（只读，供 WebUI 预填外部访问地址）。
    let lan_ip = detect_lan_ip();

    let state = AppState {
        store: Arc::new(Mutex::new(initial)),

        history: Arc::new(Mutex::new(VecDeque::new())),

        db,

        use_proxy: Arc::new(Mutex::new(use_proxy)),

        auto_refresh_sec: Arc::new(Mutex::new(auto_refresh_sec)),

        node_health_check_sec: Arc::new(Mutex::new(node_health_check_sec)),

        default_fetch_proxy: Arc::new(Mutex::new(default_fetch_proxy)),

        top_n: Arc::new(Mutex::new(top_n)),

        engine_bin: Arc::new(Mutex::new(engine_bin_default)),
        remove_after_fails: Arc::new(Mutex::new(remove_after_fails)),
        remove_sub_after_days: Arc::new(Mutex::new(remove_sub_after_days)),
        bind_addr: Arc::new(Mutex::new(bind_addr)),
        external_host: Arc::new(Mutex::new(external_host)),
        github_user: Arc::new(Mutex::new(github_user)),
        github_token: Arc::new(Mutex::new(github_token)),
        gist_id: Arc::new(Mutex::new(gist_id)),
        gist_lock: Arc::new(tokio::sync::Mutex::new(())),
        lan_ip,
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
                let sec = *st.auto_refresh_sec.lock_ok();

                if sec == 0 {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;

                    continue;
                }

                tokio::time::sleep(std::time::Duration::from_secs(sec)).await;

                // re-check in case it was disabled (or changed) while we slept

                if *st.auto_refresh_sec.lock_ok() == 0 {
                    continue;
                }

                let ids: Vec<String> = {
                    let g = st.store.lock_ok();

                    g.iter()
                        .filter(|s| s.health.source_type == "remote")
                        .map(|s| s.id.clone())
                        .collect()
                };

                for id in ids {
                    // stop early if the user disabled auto-refresh mid-cycle

                    if *st.auto_refresh_sec.lock_ok() == 0 {
                        break;
                    }

                    if do_refresh_one(&st, &id).await.is_some() {
                        persist_all(&st);
                    }
                }

                // 一轮刷新结束后，清理「连续 N 天无可用节点」的订阅（阈值 > 0 时生效）。
                prune_dead_subscriptions(&st, now_ms());
            }
        });
    }

    // Node-health periodic re-test scheduler: independently of the
    // subscription-content auto-refresh above, this re-probes every node's
    // connectivity/latency on its own interval (`node_health_check_sec`, 0 =
    // disabled) and writes the results back. Concurrency is bounded by
    // `tcp_ping_all`'s internal worker queue (a pool of `concurrency` workers
    // pulling from a shared index), so a big node set is tested in a controlled
    // stream rather than all at once. The `remove_after_fails` threshold still
    // applies, so nodes that fail N consecutive checks get pruned here too.
    {
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                let sec = *st.node_health_check_sec.lock_ok();
                if sec == 0 {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    continue;
                }
                tokio::time::sleep(std::time::Duration::from_secs(sec)).await;
                // re-check in case it was disabled (or changed) while we slept
                if *st.node_health_check_sec.lock_ok() == 0 {
                    continue;
                }
                // Snapshot every node under a short lock, then release before the
                // (potentially long) test so we never hold the store across .await.
                let all: Vec<Proxy> = {
                    let g = st.store.lock_ok();
                    g.iter().flat_map(|s| s.proxies.iter().cloned()).collect()
                };
                if all.is_empty() {
                    continue;
                }
                // Bounded concurrency: a fixed worker pool regardless of how many
                // nodes exist, so we never open thousands of sockets simultaneously.
                let concurrency = 20usize;
                let timeout_ms = 4000u64;
                run_health_check_core(&st, &all, timeout_ms, concurrency).await;
                persist_all(&st);
                // 定时清理「连续 N 天无可用节点」的订阅（阈值 > 0 时生效）。
                prune_dead_subscriptions(&st, now_ms());
            }
        });
    }

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
        .route("/api/subscriptions/:id", patch(rename_subscription))
        .route("/api/subscriptions/:id/refresh", post(refresh_subscription))
        .route(
            "/api/subscriptions/:id/speedtest",
            get(speedtest_subscription),
        )
        .route("/api/subscriptions/export", get(export_subscriptions))
        .route("/api/subscriptions/import", post(import_subscriptions))
        .route("/api/subscriptions/prune", post(prune_subscriptions))
        .route("/api/import", post(import_raw))
        .route("/api/proxies", get(list_proxies))
        .route("/api/proxies/delete", post(delete_proxy))
        .route("/api/dashboard", get(dashboard))
        .route("/api/trends", get(trends))
        .route("/api/settings", get(get_settings))
        .route("/api/settings", post(set_settings))
        .route("/api/settings/export", get(export_settings))
        .route("/api/settings/import", post(import_settings))
        .route("/api/export", post(merge_export))
        .route("/sub", get(sub_export))
        .route("/sub/", get(sub_export))
        .route("/api/speedtest", get(speedtest))
        .route("/api/proxy-yaml", get(proxy_yaml))
        .route("/api/proxy-test", post(proxy_test))
        .route("/api/geo-detect", post(geo_detect))
        .route("/api/unlock-detect", post(unlock_detect))
        .route("/api/nodes/cleanup", post(cleanup_bad))
        .route("/api/nodes/top", get(nodes_top))
        .route("/api/gist/upload", post(gist_upload))
        // 静态资源：磁盘优先（开发热更新）→ 内嵌兜底（发行版自包含），
        // 响应统一带 `no-cache`，避免 webview 缓存旧 app.js/style.css。
        .fallback(static_handler)
        .with_state(state.clone());

    // 取出监听地址（state 已通过上面的 clone 移入 Router，这里读取原 state 副本）。
    let bind_addr_str = state.bind_addr.lock_ok().clone();

    let port: u16 = std::env::var("SUBHUB_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3005);

    // 监听地址：默认 0.0.0.0（允许局域网 / 公网其他设备访问 /sub）；
    // 可用 SUBHUB_BIND 或「设置」页设为 127.0.0.1 仅本机。解析失败回退 0.0.0.0。
    let addr: SocketAddr = format!("{bind_addr_str}:{port}")
        .parse()
        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], port)));

    println!("SubHub server listening on http://{addr} (bind: {bind_addr_str})");

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
    use crate::LockExt;
    use base64::Engine;
    use subhub_core::{Proxy, ProxyType};

    #[test]
    fn expand_input_entry_handles_urls_and_base64() {
        // 普通 URL 原样返回
        assert_eq!(
            super::expand_input_entry("https://a.com/sub"),
            vec!["https://a.com/sub".to_string()]
        );
        assert_eq!(
            super::expand_input_entry("  http://b.com/x  "),
            vec!["http://b.com/x".to_string()]
        );
        // base64 解码后是多行 URL 列表
        let list = "https://c.com/1\nhttps://c.com/2\n";
        let encoded = base64::engine::general_purpose::STANDARD.encode(list.as_bytes());
        assert_eq!(
            super::expand_input_entry(&encoded),
            vec!["https://c.com/1".to_string(), "https://c.com/2".to_string()]
        );
        // 非 URL、非 base64 → 空
        assert!(super::expand_input_entry("just some random text").is_empty());
        // 空输入
        assert!(super::expand_input_entry("").is_empty());
    }

    #[test]
    fn is_loopback_server_detects_local_addresses() {
        assert!(super::is_loopback_server("127.0.0.53"));
        assert!(super::is_loopback_server("127.0.0.1"));
        assert!(super::is_loopback_server("::1"));
        assert!(super::is_loopback_server("[::1]"));
        assert!(super::is_loopback_server("0.0.0.0"));
        assert!(super::is_loopback_server("localhost"));
        // 真实公网/域名不应被判为回环
        assert!(!super::is_loopback_server("1.2.3.4"));
        assert!(!super::is_loopback_server("example.com"));
    }

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
        assert!(
            s.is_finite(),
            "score must stay finite with NaN bandwidth, got {s}"
        );
    }

    #[test]
    fn apply_mode_untested_catches_data_less_nodes() {
        // 回归：节点被测速流程跑过（last_tested_at 有值）却因 TCP 超时 /
        // 未跑引擎而 tcp/http 均为空（UI 显示 "— —"）时，「仅未测过」必须将其
        // 纳入，不能因 last_tested_at 有值就误判为已测过而跳过。
        let mut no_data = Proxy::new("a".into(), ProxyType::Ss, "1.1.1.1".into(), 8388);
        no_data.last_tested_at = Some(123); // 跑过流程但无任何延迟数据
        assert!(no_data.tcp_latency_ms.is_none() && no_data.http_latency_ms.is_none());

        let mut has_data = Proxy::new("b".into(), ProxyType::Ss, "2.2.2.2".into(), 8389);
        has_data.tcp_latency_ms = Some(50);
        has_data.http_latency_ms = Some(80);

        let filtered = super::apply_mode(vec![no_data, has_data], "untested");
        assert_eq!(filtered.len(), 1, "仅未测过应只保留无延迟数据的节点");
        assert_eq!(filtered[0].server, "1.1.1.1");
    }

    #[test]
    fn redact_url_short_host_does_not_panic() {
        // Q/B1 regression: the old implementation indexed `rest` with an
        // offset computed on the whole string — "https://a@b" panicked
        // (rest="a@b", at=9 → rest[10..]), poisoning the store mutex and
        // killing the server. Any userinfo/host length combination must be
        // safe and the userinfo must be stripped.
        assert_eq!(super::redact_url("https://a@b"), "https://b");
        assert_eq!(
            super::redact_url("https://user:pass@example.com/path?token=secret"),
            "https://example.com/path"
        );
        assert_eq!(
            super::redact_url("https://example.com/x?k=v"),
            "https://example.com/x"
        );
        assert_eq!(super::redact_url("not a url"), "not a url");
        // '@' but no scheme separator: fall through untouched (minus query)
        assert_eq!(super::redact_url("user@host"), "user@host");
    }

    fn test_state() -> super::AppState {
        use std::collections::VecDeque;
        use std::sync::Arc;
        use std::sync::Mutex;
        use subhub_core::Subscription;

        super::AppState {
            store: Arc::new(Mutex::new(vec![Subscription::new(
                "s".to_string(),
                "pasted".to_string(),
                vec![],
            )])),
            history: Arc::new(Mutex::new(VecDeque::new())),
            db: None,
            use_proxy: Arc::new(Mutex::new(false)),
            auto_refresh_sec: Arc::new(Mutex::new(0)),
            node_health_check_sec: Arc::new(Mutex::new(0)),
            default_fetch_proxy: Arc::new(Mutex::new(None)),
            top_n: Arc::new(Mutex::new(0)),
            engine_bin: Arc::new(Mutex::new(None)),
            remove_after_fails: Arc::new(Mutex::new(0)),
            bind_addr: Arc::new(Mutex::new("127.0.0.1".to_string())),
            external_host: Arc::new(Mutex::new(None)),
            github_user: Arc::new(Mutex::new(None)),
            github_token: Arc::new(Mutex::new(None)),
            gist_id: Arc::new(Mutex::new(None)),
            gist_lock: Arc::new(tokio::sync::Mutex::new(())),
            remove_sub_after_days: Arc::new(Mutex::new(0)),
            lan_ip: None,
            persist_lock: Arc::new(Mutex::new(())),
        }
    }

    #[test]
    fn auto_remove_after_consecutive_failures() {
        // The "remove after N consecutive failures" rule: a node is dropped only
        // once it has been found unavailable `threshold` times in a row, and a
        // single successful test resets the counter.
        use subhub_core::speedtest::SpeedTestResult;

        let state = test_state();
        *state.remove_after_fails.lock_ok() = 3;

        let mut p = Proxy::new("n".to_string(), ProxyType::Ss, "1.2.3.4".to_string(), 8388);
        p.password = Some("pw".to_string());
        p.method = Some("aes-256-gcm".to_string());
        state.store.lock_ok()[0].proxies.push(p.clone());

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
        assert_eq!(
            super::persist_results(&state, &[p.clone()], &[down(&p)], &[None], &[None]),
            0
        );
        assert_eq!(
            super::persist_results(&state, &[p.clone()], &[down(&p)], &[None], &[None]),
            0
        );
        assert_eq!(
            state.store.lock_ok()[0].proxies.len(),
            1,
            "survives before threshold"
        );

        // At threshold: removed this run.
        assert_eq!(
            super::persist_results(&state, &[p.clone()], &[down(&p)], &[None], &[None]),
            1
        );
        assert_eq!(
            state.store.lock_ok()[0].proxies.len(),
            0,
            "removed at threshold"
        );

        // A success resets the counter (a flaky node is not removed after one blip).
        let mut p2 = Proxy::new("m".to_string(), ProxyType::Ss, "9.9.9.9".to_string(), 8388);
        p2.password = Some("pw".to_string());
        p2.method = Some("aes-256-gcm".to_string());
        state.store.lock_ok()[0].proxies.push(p2.clone());
        super::persist_results(&state, &[p2.clone()], &[down(&p2)], &[None], &[None]);
        super::persist_results(&state, &[p2.clone()], &[down(&p2)], &[None], &[None]);
        assert_eq!(
            state.store.lock_ok()[0].proxies.len(),
            1,
            "still alive at 2 fails"
        );
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
        let after = &state.store.lock_ok()[0].proxies[0];
        assert_eq!(after.consecutive_failures, 0, "success resets counter");
    }

    #[test]
    fn tcp_only_health_check_preserves_engine_verdict() {
        // 已配置引擎时，定时健康检查只跑 TCP：TCP 成功不得把未测节点
        // （None）判成不可用，也不得累加失败计数；只有 TCP 失败才降级。
        use subhub_core::speedtest::SpeedTestResult;

        let state = test_state();
        *state.engine_bin.lock_ok() = Some("dummy-engine".to_string());

        let mut p = Proxy::new("n".to_string(), ProxyType::Ss, "1.2.3.4".to_string(), 8388);
        p.password = Some("pw".to_string());
        p.method = Some("aes-256-gcm".to_string());
        p.http_latency_ms = Some(90);
        p.available = None;
        state.store.lock_ok()[0].proxies.push(p.clone());

        let tcp_ok = SpeedTestResult {
            fingerprint: p.fingerprint(),
            name: p.name.clone(),
            tcp_latency_ms: Some(20),
            http_latency_ms: None,
            available: true,
            download_speed_bps: None,
            error: None,
        };
        super::persist_results(
            &state,
            &[p.clone()],
            std::slice::from_ref(&tcp_ok),
            &[None],
            &[None],
        );
        {
            let guard = state.store.lock_ok();
            let after = &guard[0].proxies[0];
            assert_eq!(after.available, None, "未测节点不能因 TCP 通被判死");
            assert_eq!(after.consecutive_failures, 0, "不应累加失败计数");
            assert_eq!(after.tcp_latency_ms, Some(20));
            assert_eq!(after.http_latency_ms, Some(90), "不得抹掉上次 HTTP 实测");
        }

        // 上一轮引擎判可用 → TCP 通时保持可用。
        state.store.lock_ok()[0].proxies[0].available = Some(true);
        super::persist_results(&state, &[p.clone()], &[tcp_ok], &[None], &[None]);
        assert_eq!(state.store.lock_ok()[0].proxies[0].available, Some(true));

        // TCP 都不通 → 安全降级为不可用，并累加一次失败。
        let tcp_fail = SpeedTestResult {
            fingerprint: p.fingerprint(),
            name: p.name.clone(),
            tcp_latency_ms: None,
            http_latency_ms: None,
            available: false,
            download_speed_bps: None,
            error: Some("conn refused".to_string()),
        };
        super::persist_results(&state, &[p], &[tcp_fail], &[None], &[None]);
        let guard = state.store.lock_ok();
        let after = &guard[0].proxies[0];
        assert_eq!(after.available, Some(false));
        assert_eq!(after.consecutive_failures, 1);
    }

    #[test]
    fn parse_desc_accepts_documented_and_webui_forms() {
        assert!(super::parse_desc(&Some("1".into())));
        assert!(super::parse_desc(&Some("true".into())));
        assert!(super::parse_desc(&Some("TRUE".into())));
        assert!(!super::parse_desc(&Some("0".into())));
        assert!(!super::parse_desc(&Some("false".into())));
        assert!(!super::parse_desc(&None));
    }

    #[test]
    fn resolve_top_n_distinguishes_zero_override_from_absent() {
        // 参数缺省 → 用全局；显式 0 → 全部（覆盖全局）；显式 N → N。
        assert_eq!(super::resolve_top_n(None, 50), Some(50));
        assert_eq!(super::resolve_top_n(None, 0), None);
        assert_eq!(super::resolve_top_n(Some(0), 50), None);
        assert_eq!(super::resolve_top_n(Some(0), 0), None);
        assert_eq!(super::resolve_top_n(Some(10), 50), Some(10));
        assert_eq!(super::resolve_top_n(Some(10), 0), Some(10));
    }

    #[test]
    fn sse_progress_total_matches_active_stages() {
        assert_eq!(super::sse_unit_total(0, false), 0);
        assert_eq!(super::sse_unit_total(3, false), 3, "无引擎只有 TCP 阶段");
        assert_eq!(super::sse_unit_total(3, true), 6, "有引擎：TCP + HTTP 两段");
    }
}
