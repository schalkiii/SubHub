use crate::model::Proxy;

/// Composite node score in [0, 100].
///
/// Combines (in priority order): availability, TCP latency (lower is better),
/// download bandwidth (higher is better) and streaming-unlock coverage. Each
/// component is normalised to [0, 1]; latency and bandwidth dominate while
/// unlock is a small bonus, so no single metric can fully dictate the ranking.
///
/// This is the single source of truth for node scoring: the server's
/// `list_proxies` (score column), `merge_export` / `sub_export` (Top-N
/// selection) and `ops::apply` (`sort.key == "score"`) all call this function,
/// so a UI sort by score and a Top-N export can never disagree on ordering.
pub fn score_proxy(p: &Proxy) -> f64 {
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
    // suffix — see model.rs), so 50 Mbps == 6_250_000 bytes/sec, not
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Proxy, ProxyType};

    fn node(name: &str) -> Proxy {
        Proxy::new(name.to_string(), ProxyType::Ss, "10.0.0.1".to_string(), 1)
    }

    #[test]
    fn failed_node_scores_zero() {
        let mut p = node("dead");
        p.available = Some(false);
        p.latency_ms = Some(10); // stale good latency must not matter
        assert_eq!(score_proxy(&p), 0.0);
    }

    #[test]
    fn untested_node_gets_neutral_score_not_zero() {
        let p = node("untested");
        let s = score_proxy(&p);
        assert!(s > 0.0, "untested node must not be zeroed (got {s})");
        // neutral 500 ms latency -> 0.45 * (1 - 500/2000) * 100 = 33.75
        assert!((s - 33.75).abs() < 1e-9, "expected 33.75, got {s}");
    }

    #[test]
    fn faster_node_scores_higher() {
        let mut fast = node("fast");
        fast.available = Some(true);
        fast.latency_ms = Some(50);
        let mut slow = node("slow");
        slow.available = Some(true);
        slow.latency_ms = Some(1500);
        assert!(score_proxy(&fast) > score_proxy(&slow));
    }

    #[test]
    fn unmeasured_bandwidth_is_ignored() {
        // TCP-estimated bandwidth (bandwidth_measured == false) must not
        // contribute to the score.
        let mut a = node("tcp-estimate");
        a.available = Some(true);
        a.latency_ms = Some(100);
        a.download_speed_bps = Some(50_000_000.0);
        a.bandwidth_measured = false;
        let mut b = node("no-bw");
        b.available = Some(true);
        b.latency_ms = Some(100);
        assert_eq!(score_proxy(&a), score_proxy(&b));
    }

    #[test]
    fn nan_bandwidth_does_not_poison_score() {
        let mut p = node("nan");
        p.available = Some(true);
        p.latency_ms = Some(100);
        p.download_speed_bps = Some(f64::NAN);
        p.bandwidth_measured = true;
        let s = score_proxy(&p);
        assert!(s.is_finite(), "score must stay finite, got {s}");
    }
}
