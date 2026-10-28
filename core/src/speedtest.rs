use crate::model::Proxy;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Result of a speed test for a single node. The HTTP/bandwidth/unlock fields
/// are filled in by the server layer when an external connection engine
/// (mihomo / sing-box) is configured; the TCP latency is always measured here.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SpeedTestResult {
    pub fingerprint: String,
    pub name: String,
    pub tcp_latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_latency_ms: Option<u64>,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_speed_bps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Measure raw TCP connect latency to `host:port`. This is a cheap
/// reachability + latency signal that works without any external engine.
pub fn tcp_ping(
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<(Duration, Option<f64>), String> {
    tcp_ping_cached(host, port, timeout, None)
}

/// [`tcp_ping`] with the optional per-run cache.
fn tcp_ping_cached(
    host: &str,
    port: u16,
    timeout: Duration,
    cache: Option<&RunCache>,
) -> ProbeOutcome {
    let key = (host.to_string(), port);
    if let Some(hit) = cache.and_then(|c| c.endpoint(&key)) {
        return hit;
    }
    let out = probe(host, port, timeout, cache);
    if let Some(c) = cache {
        c.remember_endpoint(key, &out);
    }
    out
}

/// 真正拨号一次：（可缓存的）解析 + connect + 吞吐估算。
fn probe(host: &str, port: u16, timeout: Duration, cache: Option<&RunCache>) -> ProbeOutcome {
    let addr = resolve(host, port, timeout, cache)?;
    let start = Instant::now();
    let mut stream =
        TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("conn: {e}"))?;
    let latency = start.elapsed();
    // Engine-free throughput estimate: measure how fast we can push data to the
    // node's socket. This is a raw TCP throughput proxy (NOT true proxy download
    // speed) and is surfaced in the 速度 column when no external engine
    // (SUBHUB_ENGINE_BIN) is configured to measure real bandwidth. When the
    // engine is configured its real measurement overrides this estimate.
    let bw_budget = timeout.min(Duration::from_millis(1000));
    let bw = measure_send_throughput(&mut stream, bw_budget);
    Ok((latency, bw))
}

/// 单个端点一次探测的结果：连接延迟 + 可选吞吐估计，或失败原因。
type ProbeOutcome = Result<(Duration, Option<f64>), String>;

/// 一轮 [`tcp_ping_all`] 内的共享缓存：域名解析结果 + TCP 端点探测结果。
///
/// 端点级缓存是这一轮里收益最大的一项：`tcp_ping` 测的是「这个 `host:port` 能不能
/// 连上、往返多久、能推多快」，与节点凭据完全无关，而机场通常把几十上百个节点挂在
/// 同一个 `host:port` 上（只有密码/路径不同）。逐节点重连同一个端点是纯重复劳动，
/// 不可达端点尤其贵——每个节点都要烧满整个 connect 超时。
///
/// 成功与失败都缓存（同一轮里死主机不必重试）。缓存的代价是一轮之内不会重新探测
/// 某个端点，这对一次测速的语义没有影响。
#[derive(Default)]
struct RunCache {
    dns: Mutex<HashMap<String, Result<SocketAddr, String>>>,
    endpoints: Mutex<HashMap<(String, u16), ProbeOutcome>>,
}

impl RunCache {
    fn dns_get(&self, host: &str) -> Option<Result<SocketAddr, String>> {
        self.dns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(host)
            .cloned()
    }

    fn dns_put(&self, host: &str, res: &Result<SocketAddr, String>) {
        self.dns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(host.to_string(), res.clone());
    }

    fn endpoint(&self, key: &(String, u16)) -> Option<ProbeOutcome> {
        self.endpoints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }

    fn remember_endpoint(&self, key: (String, u16), res: &Result<(Duration, Option<f64>), String>) {
        self.endpoints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, res.clone());
    }
}

/// 解析 `host:port`，比裸 `to_socket_addrs` 多三重保护：
/// - IP 字面量完全不进解析器：整库指向裸 IP 的节点集此前每个节点都要起一个线程；
/// - 同一主机在一轮测速里只解析一次：机场常把几百个节点挂在同一个 host 上
///   （只有端口/路径不同），解析失败的主机更糟——否则每个节点都要重新等满 DNS 预算；
/// - 解析本身有超时上限：`connect_timeout` **不覆盖**域名解析，卡死的解析器
///   （死主机、认证门户、断网）会挂住整轮测速，UI 永远等不到结果。
fn resolve(
    host: &str,
    port: u16,
    timeout: Duration,
    cache: Option<&RunCache>,
) -> Result<SocketAddr, String> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    if let Some(hit) = cache.and_then(|c| c.dns_get(host)) {
        return hit;
    }
    let out = resolve_uncached(host, port, timeout);
    if let Some(c) = cache {
        c.dns_put(host, &out);
    }
    out
}

fn resolve_uncached(host: &str, port: u16, timeout: Duration) -> Result<SocketAddr, String> {
    let dns_budget = Duration::from_millis(2000).min(timeout);
    let (tx, rx) = mpsc::channel::<Result<SocketAddr, String>>();
    let host_owned = host.to_string();
    let _dns = thread::spawn(move || {
        let res = (host_owned.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| format!("dns: {e}"))
            .and_then(|mut it| it.next().ok_or_else(|| "no address resolved".to_string()));
        let _ = tx.send(res);
    });
    match rx.recv_timeout(dns_budget) {
        Ok(Ok(a)) => Ok(a),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("dns timeout".to_string()),
    }
}

/// Estimate raw TCP throughput to an already-connected peer by pushing data
/// for up to `budget` and dividing bytes sent by elapsed time. A write timeout
/// bounds the probe so a peer that stops acking cannot stall the measurement.
fn measure_send_throughput(stream: &mut TcpStream, budget: Duration) -> Option<f64> {
    use std::io::Write;
    let _ = stream.set_write_timeout(Some(budget));
    let buf = vec![0u8; 64 * 1024];
    let start = Instant::now();
    let mut total: u64 = 0;
    while start.elapsed() < budget {
        match stream.write(&buf) {
            Ok(n) => total += n as u64,
            Err(_) => break,
        }
    }
    let dt = start.elapsed().as_secs_f64();
    if dt <= 0.001 || total == 0 {
        None
    } else {
        Some(total as f64 / dt)
    }
}

/// Per-node progress emitted by `tcp_ping_all` as each node finishes. `done` is
/// the running completion count; `total` the node set size. Streamed to the
/// WebUI so the speed-test view can show a live progress bar plus the node
/// currently being reported (name + ping + TCP throughput estimate).
#[derive(Debug, Clone, serde::Serialize)]
pub struct TestProgress {
    pub done: usize,
    pub total: usize,
    pub name: String,
    pub fingerprint: String,
    pub available: bool,
    pub latency_ms: Option<u64>,
    pub bandwidth_bps: Option<f64>,
    /// Node `server` (host) so callers can tell loopback/localhost addresses
    /// apart from real ones and avoid surfacing their meaningless local-only
    /// throughput as if it were a proxy speed.
    pub server: String,
}

/// Run TCP ping for every proxy with bounded concurrency. Returns one result
/// per proxy, order not guaranteed. When `on_progress` is `Some`, it is invoked
/// once per completed node (synchronously, inside a worker thread) so callers
/// can stream live progress. The callback is `Sync` because it is shared across
/// the worker threads, and returns whether the run should keep going: `false`
/// (typically a dropped SSE subscriber) stops scheduling the remaining nodes
/// instead of probing thousands of hosts nobody is watching any more.
pub fn tcp_ping_all(
    proxies: &[Proxy],
    timeout_ms: u64,
    concurrency: usize,
    on_progress: Option<&(dyn Fn(TestProgress) -> bool + Sync)>,
) -> Vec<SpeedTestResult> {
    if proxies.is_empty() {
        return Vec::new();
    }
    // worker 数不超过节点数：给 3 个节点起 64 个线程只是白白的 spawn 开销。
    let concurrency = concurrency.clamp(1, 64).min(proxies.len());
    let (tx, rx) = std::sync::mpsc::channel::<SpeedTestResult>();
    let queue: &Mutex<usize> = &Mutex::new(0usize);
    let cache = RunCache::default();
    let cancelled = AtomicBool::new(false);
    let done = Arc::new(AtomicUsize::new(0));
    let timeout = Duration::from_millis(timeout_ms);

    thread::scope(|s| {
        // 借用形式再交给各 worker（`&T` 是 Copy），否则 `move ||` 会把
        // `cache` / `cancelled` 移进第一个线程。
        let cache = &cache;
        let cancelled = &cancelled;
        for _ in 0..concurrency {
            let tx = tx.clone();
            let done = Arc::clone(&done);
            s.spawn(move || {
                loop {
                    if cancelled.load(Ordering::Relaxed) {
                        break;
                    }
                    let i = {
                        // Poison-tolerant: if a worker panicked mid-update the
                        // counter is still a valid usize; recover instead of
                        // cascading the panic to every other worker thread.
                        let mut g = queue.lock().unwrap_or_else(|e| e.into_inner());
                        if *g >= proxies.len() {
                            break;
                        }
                        let i = *g;
                        *g += 1;
                        i
                    };
                    let p = &proxies[i];
                    let r = test_one(p, timeout, Some(cache));
                    if let Some(cb) = on_progress {
                        let done_n = done.fetch_add(1, Ordering::Relaxed) + 1;
                        if !cb(TestProgress {
                            done: done_n,
                            total: proxies.len(),
                            name: r.name.clone(),
                            fingerprint: r.fingerprint.clone(),
                            available: r.available,
                            latency_ms: r.tcp_latency_ms,
                            bandwidth_bps: r.download_speed_bps,
                            server: p.server.clone(),
                        }) {
                            cancelled.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                    let _ = tx.send(r);
                }
            });
        }
    });
    drop(tx);
    rx.iter().collect()
}

fn test_one(p: &Proxy, timeout: Duration, cache: Option<&RunCache>) -> SpeedTestResult {
    match tcp_ping_cached(&p.server, p.port, timeout, cache) {
        Ok((d, bw)) => SpeedTestResult {
            fingerprint: p.fingerprint(),
            name: p.name.clone(),
            tcp_latency_ms: Some(d.as_millis() as u64),
            http_latency_ms: None,
            available: true,
            download_speed_bps: bw,
            error: None,
        },
        Err(e) => SpeedTestResult {
            fingerprint: p.fingerprint(),
            name: p.name.clone(),
            tcp_latency_ms: None,
            http_latency_ms: None,
            available: false,
            download_speed_bps: None,
            error: Some(e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Proxy, ProxyType};
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static PROGRESS_CALLS: AtomicUsize = AtomicUsize::new(0);
    static CANCEL_CALLS: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn tcp_ping_reaches_local_listener() {
        // 绑定本地监听，accept 后保持连接即可；tcp_ping 应能连上并返回延迟。
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let _ = s.read(&mut [0u8; 8]);
            }
        });
        let res = tcp_ping("127.0.0.1", port, Duration::from_millis(2000));
        let _ = handle.join();
        let (lat, _bw) = res.expect("本地监听应可达");
        assert!(lat.as_millis() < 2000, "loopback 延迟应极小");
    }

    #[test]
    fn tcp_ping_to_closed_port_errors() {
        // 端口 1 无监听，连接必失败；校验错误路径不 panic、返回 Err。
        let res = tcp_ping("127.0.0.1", 1, Duration::from_millis(1000));
        assert!(res.is_err(), "连向关闭端口必须返回 Err");
    }

    #[test]
    fn tcp_ping_all_empty_returns_empty() {
        let out = tcp_ping_all(&[], 1000, 4, None);
        assert!(out.is_empty());
    }

    #[test]
    fn tcp_ping_all_progress_and_availability() {
        PROGRESS_CALLS.store(0, Ordering::SeqCst);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            for _ in 0..3 {
                if let Ok((mut s, _)) = listener.accept() {
                    let _ = s.read(&mut [0u8; 8]);
                }
            }
        });
        let proxies: Vec<Proxy> = (0..3)
            .map(|i| Proxy::new(format!("n{i}"), ProxyType::Ss, "127.0.0.1".into(), port))
            .collect();
        let cb: &(dyn Fn(TestProgress) -> bool + Sync) = &|_p: TestProgress| {
            PROGRESS_CALLS.fetch_add(1, Ordering::SeqCst);
            true
        };
        let out = tcp_ping_all(&proxies, 2000, 4, Some(cb));
        let _ = handle.join();
        assert_eq!(out.len(), 3, "结果数 == 节点数");
        assert!(out.iter().all(|r| r.available), "loopback 节点应全部可达");
        assert_eq!(PROGRESS_CALLS.load(Ordering::SeqCst), 3, "每节点回调一次");
    }

    #[test]
    fn tcp_ping_all_stops_when_progress_callback_says_no() {
        // 进度回调返回 false = 订阅端已断开（SSE 关闭），剩余节点不应继续探测。
        // 独立计数器：`PROGRESS_CALLS` 由并行运行的另一个测试共用会互相干扰。
        CANCEL_CALLS.store(0, Ordering::SeqCst);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _acceptor = std::thread::spawn(move || {
            // 无限 accept：取消后再没有新连接打进来，固定次数的循环会让这里
            // 永远等在 accept 上、join 不返回。线程随测试进程一起回收。
            while let Ok((mut s, _)) = listener.accept() {
                let _ = s.read(&mut [0u8; 8]);
            }
        });
        let proxies: Vec<Proxy> = (0..12)
            .map(|i| Proxy::new(format!("n{i}"), ProxyType::Ss, "127.0.0.1".into(), port))
            .collect();
        let cb: &(dyn Fn(TestProgress) -> bool + Sync) = &|_p: TestProgress| {
            CANCEL_CALLS.fetch_add(1, Ordering::SeqCst);
            false
        };
        let out = tcp_ping_all(&proxies, 1000, 2, Some(cb));
        // 两个 worker 可能在看到取消标志前各回调一次，但绝不会继续跑完 12 个。
        assert!(
            CANCEL_CALLS.load(Ordering::SeqCst) <= 2,
            "取消后仍在推进度: {}",
            CANCEL_CALLS.load(Ordering::SeqCst)
        );
        assert!(
            out.len() < proxies.len(),
            "取消仍测完了全部 {} 个节点",
            proxies.len()
        );
    }

    #[test]
    fn resolve_treats_ip_literals_without_touching_the_resolver() {
        // 裸 IP 节点此前也要为每个节点起一个解析线程。
        let a = resolve("127.0.0.1", 8388, Duration::from_millis(500), None).unwrap();
        assert_eq!(a.ip().to_string(), "127.0.0.1");
        assert_eq!(a.port(), 8388);
        let b = resolve("::1", 1, Duration::from_millis(500), None).unwrap();
        assert!(b.ip().is_loopback());
    }

    #[test]
    fn dns_cache_reuses_one_result_per_host() {
        // 缓存命中与未命中都必须给出同一个结果（失败同样缓存，否则同一死主机
        // 上的每个节点都要重新等满 DNS 预算）。
        let cache = RunCache::default();
        let first = resolve("localhost", 1234, Duration::from_millis(2000), Some(&cache));
        let second = resolve("localhost", 1234, Duration::from_millis(2000), Some(&cache));
        assert_eq!(first.is_ok(), second.is_ok(), "两次结果应一致");
        if let Ok(a) = &first {
            assert_eq!(a, second.as_ref().unwrap());
        }
        assert!(
            cache.dns_get("localhost").is_some(),
            "域名解析结果应进入本轮缓存（无论成败）"
        );
        // IP 字面量走快速路径，不进缓存也不起线程。
        resolve("127.0.0.1", 1, Duration::from_millis(500), Some(&cache)).unwrap();
        assert!(cache.dns_get("127.0.0.1").is_none(), "IP 字面量不该进缓存");

        let bad = resolve(
            "no-such-host.invalid",
            443,
            Duration::from_millis(1200),
            Some(&cache),
        );
        assert!(bad.is_err());
        assert!(
            cache.dns_get("no-such-host.invalid").is_some(),
            "解析失败也应缓存，避免每个节点重等一遍"
        );
    }

    #[test]
    fn endpoint_results_are_reused_within_one_run() {
        // 同一 host:port 上挂着几十个节点时只应拨号一次：第二次必须拿到与第一次
        // 逐位相同的延迟（自己重新测不可能复现同一个 Duration）。
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _acceptor = std::thread::spawn(move || {
            while let Ok((mut s, _)) = listener.accept() {
                let _ = s.read(&mut [0u8; 8]);
            }
        });
        let cache = RunCache::default();
        let a = tcp_ping_cached("127.0.0.1", port, Duration::from_millis(1500), Some(&cache))
            .expect("loopback 可达");
        let b = tcp_ping_cached("127.0.0.1", port, Duration::from_millis(1500), Some(&cache))
            .expect("loopback 可达");
        assert_eq!(a.0, b.0, "第二次应命中端点缓存而不是重新拨号");
        assert_eq!(a.1, b.1);
    }

    #[test]
    fn tcp_ping_all_shares_the_endpoint_cache() {
        // 12 个节点共用一个端点：结果条数仍是节点数（前端每节点一个进度 tick），
        // 但延迟必须完全一致，说明底层只拨号了一次。
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _acceptor = std::thread::spawn(move || {
            for _ in 0..64 {
                if let Ok((mut s, _)) = listener.accept() {
                    let _ = s.read(&mut [0u8; 8]);
                }
            }
        });
        let proxies: Vec<Proxy> = (0..12)
            .map(|i| Proxy::new(format!("m{i}"), ProxyType::Ss, "127.0.0.1".into(), port))
            .collect();
        let out = tcp_ping_all(&proxies, 1500, 3, None);
        assert_eq!(out.len(), 12, "每个节点仍各有一条结果");
        let lat: Vec<u64> = out
            .iter()
            .map(|r| r.tcp_latency_ms.unwrap_or(u64::MAX))
            .collect();
        assert_eq!(
            lat.iter().collect::<std::collections::HashSet<_>>().len(),
            1,
            "同一端点的延迟应完全相同（说明只拨号一次）: {lat:?}"
        );
    }

    #[test]
    fn measure_send_throughput_returns_some_for_open_socket() {
        // 对保持打开的对端写数据，应在预算内返回 Some(>0) 吞吐估计。
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            if let Ok((s, _)) = listener.accept() {
                std::thread::sleep(Duration::from_millis(300)); // 保持连接
                drop(s);
            }
        });
        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let bw = measure_send_throughput(&mut client, Duration::from_millis(200));
        let _ = handle.join();
        assert!(bw.is_some_and(|b| b > 0.0), "开放 socket 应测得正吞吐");
    }
}
