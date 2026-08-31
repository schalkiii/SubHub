use crate::model::Proxy;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
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
    // DNS resolution is NOT bounded by `connect_timeout`, so a stalled resolver
    // (dead host, captive portal, no network) would hang the whole speed test
    // and the UI would never receive a result. Resolve in a detached thread
    // with its own cap and bail out if it doesn't answer in time.
    let dns_budget = Duration::from_millis(2000).min(timeout);
    let (tx, rx) = mpsc::channel::<Result<std::net::SocketAddr, String>>();
    let host_owned = host.to_string();
    let _dns = std::thread::spawn(move || {
        let res = (host_owned.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| format!("dns: {e}"))
            .and_then(|mut it| it.next().ok_or_else(|| "no address resolved".to_string()));
        let _ = tx.send(res);
    });
    let addr = match rx.recv_timeout(dns_budget) {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("dns timeout".to_string()),
    };
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
/// the worker threads.
pub fn tcp_ping_all(
    proxies: &[Proxy],
    timeout_ms: u64,
    concurrency: usize,
    on_progress: Option<&(dyn Fn(TestProgress) + Sync)>,
) -> Vec<SpeedTestResult> {
    if proxies.is_empty() {
        return Vec::new();
    }
    let concurrency = concurrency.clamp(1, 64);
    let (tx, rx) = std::sync::mpsc::channel::<SpeedTestResult>();
    let queue: &Mutex<usize> = &Mutex::new(0usize);
    let done = Arc::new(AtomicUsize::new(0));
    let timeout = Duration::from_millis(timeout_ms);

    thread::scope(|s| {
        for _ in 0..concurrency {
            let tx = tx.clone();
            let done = Arc::clone(&done);
            s.spawn(move || {
                loop {
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
                    let r = test_one(p, timeout);
                    if let Some(cb) = on_progress {
                        let done_n = done.fetch_add(1, Ordering::Relaxed) + 1;
                        cb(TestProgress {
                            done: done_n,
                            total: proxies.len(),
                            name: r.name.clone(),
                            fingerprint: r.fingerprint.clone(),
                            available: r.available,
                            latency_ms: r.tcp_latency_ms,
                            bandwidth_bps: r.download_speed_bps,
                            server: p.server.clone(),
                        });
                    }
                    let _ = tx.send(r);
                }
            });
        }
    });
    drop(tx);
    rx.iter().collect()
}

fn test_one(p: &Proxy, timeout: Duration) -> SpeedTestResult {
    match tcp_ping(&p.server, p.port, timeout) {
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
        let cb: &(dyn Fn(TestProgress) + Sync) = &|_p: TestProgress| {
            PROGRESS_CALLS.fetch_add(1, Ordering::SeqCst);
        };
        let out = tcp_ping_all(&proxies, 2000, 4, Some(cb));
        let _ = handle.join();
        assert_eq!(out.len(), 3, "结果数 == 节点数");
        assert!(out.iter().all(|r| r.available), "loopback 节点应全部可达");
        assert_eq!(PROGRESS_CALLS.load(Ordering::SeqCst), 3, "每节点回调一次");
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
