//! Optional protocol-level checks via an external connection engine
//! (mihomo / sing-box). Best-effort: every failure yields `None` for that node,
//! so it never breaks the primary TCP-latency path. Enabled when the engine
//! binary is configured in the UI settings (fallback: `SUBHUB_ENGINE_BIN`).
use crate::Proxy;

use subhub_core::to_clash_meta;

use std::collections::BTreeMap;

use std::io::Write;
use std::sync::Arc;

use std::net::{TcpListener, TcpStream};

use std::process::Child;

use std::process::Command;

use std::time::{Duration, Instant};

/// Hard cap on simultaneously running engine processes. Each check spawns a
/// full mihomo/sing-box OS process per node; anything beyond a handful stalls
/// the whole machine, so the per-request concurrency is clamped to this.
const MAX_ENGINE_PROCESSES: usize = 4;

/// RAII guard that guarantees the spawned engine process and its temp config
/// directory are cleaned up no matter how `with_engine` exits — including the
/// early `?` returns that happen *after* `cmd.spawn()` but *before* the old
/// explicit `child.kill()` calls (e.g. building the reqwest client for an
/// unsupported proxy URL). Without this, those paths leaked both the OS
/// process and the `subhub-engine-<port>` directory.
struct EngineGuard {
    child: Child,
    dir: std::path::PathBuf,
}

impl Drop for EngineGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Monotonic counter to make each engine temp dir unique even when two
/// `free_port()` calls race to the same OS-assigned port (the port is
/// released immediately after probing, so a later call can reuse it).
/// Including the pid + this counter guarantees a collision-free directory
/// name, so concurrent engine spawns never clobber each other's config.
static ENGINE_DIR_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Spin up the engine for one proxy, wait for its SOCKS5/mixed listener, hand
/// a proxied `reqwest::Client` to `f`, then tear the engine down. Returns
/// `None` if the engine can't start / the port never opens / the node isn't
/// exportable (so the engine couldn't run it anyway) —never throws.
async fn with_engine<R, F, Fut>(p: &Proxy, bin: &str, timeout_ms: u64, f: F) -> Option<R>

where

    F: FnOnce(reqwest::Client) -> Fut,

    Fut: std::future::Future<Output = R>,

{

    // Skip nodes the engine can't represent (e.g. `Other` type, or a node

    // missing required fields). Spawning an engine for them is wasted work —    // they can never be measured and the TCP path already handles liveness.

    if !p.is_exportable() {

        return None;

    }

    let port = free_port()?;

    let cfg = build_engine_config(p, port);

    let seq = ENGINE_DIR_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "subhub-engine-{}-{}-{}",
        std::process::id(),
        port,
        seq
    ));

    let _ = std::fs::create_dir_all(&dir);

    let cfg_path = dir.join("config.yaml");

    {

        let mut file = std::fs::File::create(&cfg_path).ok()?;

        let _ = file.write_all(cfg.as_bytes());

    }



    let mut cmd = Command::new(bin);
    cmd.arg("-c")
        .arg(&cfg_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Engines are console programs; without this flag every spawn pops up a
    // visible console window on Windows (hundreds during a speedtest).
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let child = cmd.spawn().ok()?;
    // Owned guard: drops the process + temp dir on every exit path (including
    // the `?` returns below that previously leaked both).
    let mut _guard = EngineGuard {
        child,
        dir: dir.clone(),
    };

    if !engine_ready(&mut _guard.child, port, Duration::from_millis(4000)).await {
        return None;
    }

    let proxy = reqwest::Proxy::all(format!("socks5://127.0.0.1:{port}")).ok()?;

    let client = reqwest::Client::builder()

        .proxy(proxy)

        .timeout(Duration::from_millis(timeout_ms))

        .build()

        .ok()?;

    let result = f(client).await;

    Some(result)

}



/// HTTP latency (ms) for each proxy, in input order. `None` = not measured
/// (engine missing / node dead / timeout).
pub async fn engine_http_latency(

    proxies: &[Proxy],

    bin: &str,

    timeout_ms: u64,
    concurrency: usize,
) -> Vec<Option<u64>> {

    if !std::path::Path::new(bin).exists() {

        return vec![None; proxies.len()];

    }

    let sem = Arc::new(tokio::sync::Semaphore::new(
        concurrency.clamp(1, MAX_ENGINE_PROCESSES),
    ));
    let mut tasks = Vec::new();
    for p in proxies {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let p = p.clone();
        let bin = bin.to_string();
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            engine_latency_one(p, bin, timeout_ms).await
        }));
    }

    let mut out = Vec::with_capacity(proxies.len());

    for t in tasks {

        out.push(t.await.unwrap_or(None).flatten());

    }

    out

}



async fn engine_latency_one(p: Proxy, bin: String, timeout_ms: u64) -> Option<Option<u64>> {

    with_engine(&p, &bin, timeout_ms, |c| async move {

        let start = Instant::now();

        let resp = c

            .get(subhub_core::resources::ALIVE_TARGET)

            .send()

            .await;

        let ms = start.elapsed().as_millis() as u64;

        let ok = resp

            .map(|r| r.status().as_u16() == subhub_core::resources::ALIVE_EXPECT_CODE)

            .unwrap_or(false);

        if ok {

            Some(ms)

        } else {

            None

        }

    })

    .await

}



/// Detect the outbound exit-country of each proxy (BestSub `country` pattern):
/// spin up the engine per proxy and ask the geo-IP channels for the exit IP's
/// country code. Returns `Option<String>` (2-letter code) per proxy, in input
/// order. `None` = engine missing / proxy dead / all channels failed.
pub async fn detect_outbound_country(

    proxies: &[Proxy],

    bin: &str,

    timeout_ms: u64,
    concurrency: usize,
) -> Vec<Option<String>> {

    if !std::path::Path::new(bin).exists() {

        return vec![None; proxies.len()];

    }

    let sem = Arc::new(tokio::sync::Semaphore::new(
        concurrency.clamp(1, MAX_ENGINE_PROCESSES),
    ));
    let mut tasks = Vec::new();
    for p in proxies {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let p = p.clone();
        let bin = bin.to_string();
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            geo_one(p, bin, timeout_ms).await
        }));
    }

    let mut out = Vec::with_capacity(proxies.len());

    for t in tasks {

        out.push(t.await.unwrap_or(None).flatten());

    }

    out

}



async fn geo_one(p: Proxy, bin: String, timeout_ms: u64) -> Option<Option<String>> {

    with_engine(&p, &bin, timeout_ms, |c| async move {

        let mut result: Option<String> = None;

        for ch in subhub_core::resources::GEO_CHANNELS {

            if let Ok(r) = c.get(ch.url).send().await {

                if let Ok(body) = r.text().await {

                    if let Some(cc) = subhub_core::resources::extract_country(&body) {

                        result = Some(cc);

                        break;

                    }

                }

            }

        }

        result

    })

    .await

}



/// Streaming-unlock matrix for each proxy (BestSub `checker/tiktok.go` style,
/// extended with Netflix/Disney/YouTube/ChatGPT). Returns a `ProxyUnlock` per
/// proxy, in input order. The map is empty when the engine is missing.
pub async fn detect_unlock(

    proxies: &[Proxy],

    bin: &str,

    timeout_ms: u64,
    concurrency: usize,
) -> Vec<subhub_core::model::ProxyUnlock> {

    if !std::path::Path::new(bin).exists() {

        return vec![subhub_core::model::ProxyUnlock::default(); proxies.len()];

    }

    let sem = Arc::new(tokio::sync::Semaphore::new(
        concurrency.clamp(1, MAX_ENGINE_PROCESSES),
    ));
    let mut tasks = Vec::new();
    for p in proxies {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let p = p.clone();
        let bin = bin.to_string();
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            unlock_one(p, bin, timeout_ms).await
        }));
    }

    let mut out = Vec::with_capacity(proxies.len());

    for t in tasks {

        out.push(t.await.unwrap_or_default());

    }

    out

}



async fn unlock_one(p: Proxy, bin: String, timeout_ms: u64) -> subhub_core::model::ProxyUnlock {

    let result = with_engine(&p, &bin, timeout_ms, |c| async move {

        let mut services: BTreeMap<String, subhub_core::model::UnlockResult> = BTreeMap::new();

        for svc in subhub_core::resources::STREAM_SERVICES {

            let u = match c.get(svc.url).send().await {

                Ok(r) => {

                    let status = r.status().as_u16();

                    match r.text().await {

                        Ok(body) => svc.detect.classify(status, &body),

                        Err(_) => subhub_core::model::UnlockResult {

                            status: "failed".into(),

                            region: None,

                        },

                    }

                }

                Err(_) => subhub_core::model::UnlockResult {

                    status: "failed".into(),

                    region: None,

                },

            };

            services.insert(svc.id.to_string(), u);

        }

        services

    })

    .await;

    match result {

        Some(map) => subhub_core::model::ProxyUnlock { services: map },

        None => subhub_core::model::ProxyUnlock::default(),

    }

}



/// Download bandwidth (bytes/s) for each proxy via cloudflare's `__down`
/// endpoint (BestSub `checker/speed.go`). `None` = engine missing / failed.
pub async fn engine_bandwidth(

    proxies: &[Proxy],

    bin: &str,

    timeout_ms: u64,
    concurrency: usize,
) -> Vec<Option<f64>> {

    if !std::path::Path::new(bin).exists() {

        return vec![None; proxies.len()];

    }

    let sem = Arc::new(tokio::sync::Semaphore::new(
        concurrency.clamp(1, MAX_ENGINE_PROCESSES),
    ));
    let mut tasks = Vec::new();
    for p in proxies {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let p = p.clone();
        let bin = bin.to_string();
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            bandwidth_one(p, bin, timeout_ms).await
        }));
    }

    let mut out = Vec::with_capacity(proxies.len());

    for t in tasks {

        out.push(t.await.unwrap_or(None).flatten());

    }

    out

}



async fn bandwidth_one(p: Proxy, bin: String, timeout_ms: u64) -> Option<Option<f64>> {

    with_engine(&p, &bin, timeout_ms, |c| async move {

        let start = Instant::now();

        match c

            .get(subhub_core::resources::SPEED_DOWNLOAD_URL)

            .send()

            .await

        {

            Ok(resp) => match resp.bytes().await {

                Ok(bytes) => {

                    let secs = start.elapsed().as_secs_f64();

                    if secs > 0.0 {

                        Some(bytes.len() as f64 / secs)

                    } else {

                        None

                    }

                }

                Err(_) => None,

            },

            Err(_) => None,

        }

    })

    .await

}



/// Safely embed an arbitrary string as a YAML double-quoted flow scalar.
/// Node names come from subscription data (attacker-influenced), so a name
/// containing `"` or a newline could otherwise break out of the
/// `proxy-groups: [..]` flow sequence and inject arbitrary engine config
/// (e.g. `allow-lan`, `external-controller` + `secret`, poisoned `hosts`).
/// Wrapping in double quotes and escaping the dangerous characters prevents
/// that while keeping the node name intact.
fn escape_yaml_scalar(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Build a minimal single-node mihomo/sing-box-compatible config exposing a
/// SOCKS5/`mixed-port` listener on `port`.
fn build_engine_config(p: &Proxy, port: u16) -> String {

    let node = to_clash_meta(std::slice::from_ref(p));

    // `to_clash_meta` wraps in `proxies:\n  - ...`; reuse only the indented node.

    let node_body = node.trim_start_matches("proxies:").trim_start().to_string();

    let mut cfg = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(cfg, "mixed-port: {port}");
    let _ = writeln!(cfg, "mode: global");
    let _ = writeln!(cfg, "log-level: error");
    let _ = writeln!(cfg, "proxies:");
    let _ = writeln!(cfg, "{node_body}");
    let _ = writeln!(cfg, "proxy-groups:");
    let _ = writeln!(cfg, "  - name: PROXY");
    let _ = writeln!(cfg, "    type: select");
    let _ = writeln!(cfg, "    proxies: [{}]", escape_yaml_scalar(&p.name));
    let _ = writeln!(cfg, "rules:");
    let _ = writeln!(cfg, "  - MATCH,PROXY");
    cfg

}



fn free_port() -> Option<u16> {

    let l = TcpListener::bind("127.0.0.1:0").ok()?;

    l.local_addr().ok().map(|a| a.port())

}



/// Wait for the engine's listener to come up, but bail out the moment the
/// engine process *exits* on its own (crash / config error / wrong binary).
/// Without this, a misconfigured or incompatible engine would make every node
/// wait the full `wait_port` timeout before failing, so a 200-node speedtest
/// against a broken engine looks frozen ("tested 10 then nothing happens").
/// Returning early turns that into a fast stream of `None` results.
async fn engine_ready(child: &mut Child, port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        // If the engine already died, don't burn the rest of the budget.
        if child.try_wait().ok().flatten().is_some() {
            return false;
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::escape_yaml_scalar;

    #[test]
    fn escape_yaml_scalar_quotes_and_escapes() {
        // Plain name: wrapped in quotes, unchanged otherwise.
        assert_eq!(escape_yaml_scalar("My Node"), "\"My Node\"");
        // A name containing a double quote must be escaped, not break out of
        // the flow sequence (this is the N1 injection vector).
        assert_eq!(escape_yaml_scalar("ev\""), "\"ev\\\"\"");
        // A newline must be escaped, otherwise it would start a new YAML line
        // and let a crafted name inject arbitrary config.
        assert_eq!(escape_yaml_scalar("a\nb"), "\"a\\nb\"");
        // A backslash must be escaped so it isn't read as an escape sequence.
        assert_eq!(escape_yaml_scalar("a\\b"), "\"a\\\\b\"");
        // A realistic injection attempt: try to close the scalar and add a
        // malicious `allow-lan` key. The whole thing stays inside one quoted
        // scalar, so it can never become a second YAML key.
        let evil = "x\"\nallow-lan: true";
        let got = escape_yaml_scalar(evil);
        assert!(got.starts_with('"'), "must stay quoted");
        assert!(got.ends_with('"'), "must stay quoted");
        assert!(!got.contains("\nallow-lan"), "injection must be escaped");
    }
}

