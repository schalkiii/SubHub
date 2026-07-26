use crate::model::Proxy;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::sync::mpsc;
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
pub fn tcp_ping(host: &str, port: u16, timeout: Duration) -> Result<(Duration, Option<f64>), String> {
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
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("conn: {e}"))?;
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

/// Run TCP ping for every proxy with bounded concurrency. Returns one result
/// per proxy, order not guaranteed.
pub fn tcp_ping_all(proxies: &[Proxy], timeout_ms: u64, concurrency: usize) -> Vec<SpeedTestResult> {
    if proxies.is_empty() {
        return Vec::new();
    }
    let concurrency = concurrency.clamp(1, 64);
    let (tx, rx) = std::sync::mpsc::channel::<SpeedTestResult>();
    let queue: &Mutex<usize> = &Mutex::new(0usize);
    let timeout = Duration::from_millis(timeout_ms);

    thread::scope(|s| {
        for _ in 0..concurrency {
            let tx = tx.clone();
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
