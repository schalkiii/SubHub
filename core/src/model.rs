use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

static SUB_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Stable-ish unique id for a subscription (dependency-free).
pub fn new_sub_id() -> String {
    format!("sub_{}", SUB_COUNTER.fetch_add(1, Ordering::SeqCst))
}

/// Re-base the subscription id counter so the next `new_sub_id()` returns
/// `sub_{max+1}`. Call this after loading persisted subscriptions from
/// SQLite to avoid id collisions with already-stored entries.
pub fn rebase_sub_counter(max_existing: u64) {
    let current = SUB_COUNTER.load(Ordering::SeqCst);
    let target = max_existing.saturating_add(1);
    if target > current {
        SUB_COUNTER.store(target, Ordering::SeqCst);
    }
}

/// Per-subscription health — the "逐个订阅健康度" view inspired by Resin.
/// `last_checked_at` / `last_updated_at` / `last_error` are persisted on fetch;
/// the node/availability/latency counts are *derived* from `proxies` at read
/// time via `recompute` so they never drift out of sync.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SubscriptionHealth {
    pub enabled: bool,
    /// "remote" (fetched URL) or "local" (pasted content)
    pub source_type: String,
    /// epoch millis of the last fetch attempt
    pub last_checked_at: Option<u64>,
    /// epoch millis of the last *successful* fetch
    pub last_updated_at: Option<u64>,
    /// error message from the last failed fetch, if any
    pub last_error: Option<String>,

    // derived (filled by recompute)
    pub node_count: usize,
    pub healthy_node_count: usize,
    pub unknown_node_count: usize,
    pub avg_latency_ms: Option<u64>,
    pub best_latency_ms: Option<u64>,

    // Traffic usage (clash subscription info block / `Subscription-Userinfo`
    // response header). All `Option` so subscriptions without usage info
    // (or pasted/local sources) simply stay `None` and the UI hides it.
    /// bytes uploaded this period
    pub upload: Option<u64>,
    /// bytes downloaded this period
    pub download: Option<u64>,
    /// bytes total quota this period (0 = unlimited)
    pub total: Option<u64>,
    /// subscription expiry, epoch **milliseconds**
    pub expire: Option<u64>,
}

impl SubscriptionHealth {
    /// Aggregate status used for the UI status dot.
    pub fn status(&self) -> &'static str {
        if !self.enabled {
            return "disabled";
        }
        if self.last_checked_at.is_none() {
            return "pending";
        }
        if self.last_error.is_some() {
            if self.last_updated_at.is_none() {
                return "error"; // never succeeded
            }
            return "degraded"; // succeeded before, last check failed
        }
        if self.node_count == 0 {
            return "empty";
        }
        if self.healthy_node_count == 0 {
            // no alive nodes yet — but are they simply untested?
            return if self.unknown_node_count > 0 {
                "untested"
            } else {
                "down"
            };
        }
        // Promote to u64 before the multiply so a huge node count can't
        // overflow the intermediate (the audit flagged `healthy * 100` as a
        // potential usize overflow; at realistic sizes it can't happen, but
        // the checked math is free and future-proof).
        if self.node_count > 0
            && (self.healthy_node_count as u64) * 100 / (self.node_count as u64) < 50
        {
            return "degraded";
        }
        "healthy"
    }

    /// Recompute the derived counts/latency from the current proxies.
    pub fn recompute(&mut self, proxies: &[Proxy]) {
        self.node_count = proxies.len();
        let mut avail = 0usize;
        let mut unk = 0usize;
        let mut sum = 0u64;
        let mut n = 0u64;
        let mut best: Option<u64> = None;
        for p in proxies {
            match p.available {
                Some(true) => avail += 1,
                Some(false) => {}
                None => unk += 1,
            }
            if let Some(ms) = p.latency_ms {
                sum += ms;
                n += 1;
                best = Some(best.map_or(ms, |b| b.min(ms)));
            }
        }
        self.healthy_node_count = avail;
        self.unknown_node_count = unk;
        self.avg_latency_ms = sum.checked_div(n);
        self.best_latency_ms = best;
    }
}

/// A logical subscription source: one remote URL (or a pasted block) that
/// resolved into a set of nodes. Grouping by source is what lets the UI
/// show per-subscription stats and lets the merge view combine chosen groups
/// (the sub-store model).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub name: String,
    /// original source: remote URL or "pasted"
    pub source: String,
    pub proxies: Vec<Proxy>,
    /// per-subscription health (Resin-style)
    pub health: SubscriptionHealth,
    /// optional upstream proxy used when fetching this remote subscription
    /// (e.g. "http://127.0.0.1:7890" or "socks5://127.0.0.1:1080"). Lets you
    /// pull a geo-blocked subscription through a working proxy.
    pub fetch_proxy: Option<String>,
}

impl Subscription {
    pub fn new(name: String, source: String, proxies: Vec<Proxy>) -> Self {
        let source_type = if source == "pasted" {
            "local".to_string()
        } else {
            "remote".to_string()
        };
        Subscription {
            id: new_sub_id(),
            name,
            source,
            proxies,
            fetch_proxy: None,
            health: SubscriptionHealth {
                enabled: true,
                source_type,
                ..Default::default()
            },
        }
    }

    pub fn node_count(&self) -> usize {
        self.proxies.len()
    }
}

/// Unified proxy node model. Every supported subscription format is parsed
/// into this struct so the rest of the app (merge, filter, export) only has
/// to deal with one representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proxy {
    pub name: String,
    pub type_: ProxyType,
    pub server: String,
    pub port: u16,

    // vmess / vless
    pub uuid: Option<String>,
    pub alter_id: Option<u32>,
    pub cipher: Option<String>, // vmess aes-128-gcm etc.
    pub flow: Option<String>,   // vless flow

    // ss / trojan
    pub password: Option<String>,
    pub method: Option<String>,

    // transport / tls
    pub network: Option<String>, // tcp / ws / grpc / h2
    pub tls: Option<bool>,
    pub sni: Option<String>,
    pub skip_cert_verify: Option<bool>,
    pub path: Option<String>,
    pub host: Option<String>,
    pub service_name: Option<String>, // grpc
    pub fingerprint: Option<String>,

    // speed-test results (populated by the speedtest engine)
    pub latency_ms: Option<u64>,
    pub available: Option<bool>,
    pub download_speed_bps: Option<f64>,
    /// Whether `download_speed_bps` is a *real* engine-measured bandwidth
    /// (`true`) or the engine-free TCP throughput estimate (`false`/absent).
    /// The 速度 column surfaces the value either way, but the composite score
    /// (and thus Top-N export) must ignore the estimate — it is raw socket
    /// throughput, not true proxy bandwidth, and would otherwise dominate the
    /// 40%-weight bandwidth component with meaningless numbers.
    ///
    /// UNITS: the value is in **bytes/sec**, NOT bits/sec, despite the `_bps`
    /// suffix. The scorer normalises it against 6_250_000 (== 50 Mbps) rather
    /// than 50_000_000 to stay consistent with this convention.
    #[serde(default)]
    pub bandwidth_measured: bool,
    pub last_tested_at: Option<u64>,

    /// Consecutive speed-test runs where this node was found unavailable
    /// (`available == Some(false)`). Reset to 0 on any successful test. Feeds
    /// the "auto-remove after N consecutive failures" feature
    /// (`remove_after_fails` setting); intentionally never counts untested
    /// (`available == None`) nodes so a node that was simply never tested is
    /// never auto-removed.
    #[serde(default)]
    pub consecutive_failures: u32,

    // outbound geo (populated by the geo-detect engine, BestSub-style)
    pub outbound_country: Option<String>,

    // streaming-unlock results (populated by the unlock-detect engine, BestSub-style)
    pub unlock: Option<ProxyUnlock>,

    // misc (kept as raw map so exotic fields survive round-trips)
    pub extra: Option<serde_json::Value>,
}

/// Per-service streaming-unlock probe result (BestSub-style unlock matrix,
/// extended with the common community streaming checks: Netflix / Disney+ /
/// YouTube Premium / ChatGPT). Mirrors the way BestSub's `checker/tiktok.go`
/// records a per-node `AliveStatus` for each streaming service.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UnlockResult {
    /// "unlocked" | "blocked" | "failed" | "unknown"
    pub status: String,
    /// 2-letter exit-region where the service is unlocked, when known
    pub region: Option<String>,
}

/// All streaming-unlock results for a single proxy, keyed by service id
/// (e.g. "tiktok", "netflix", "disney", "youtube", "chatgpt").
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProxyUnlock {
    pub services: std::collections::BTreeMap<String, UnlockResult>,
}

impl ProxyUnlock {
    /// Compact text summary for the UI, e.g. "TT✓HK NF✗ YT✓US".
    pub fn summary(&self) -> String {
        const SHORT: &[(&str, &str)] = &[
            ("tiktok", "TT"),
            ("netflix", "NF"),
            ("disney", "DS"),
            ("youtube", "YT"),
            ("chatgpt", "GPT"),
        ];
        let mut parts: Vec<String> = Vec::new();
        for (id, short) in SHORT {
            if let Some(r) = self.services.get(*id) {
                match r.status.as_str() {
                    "unlocked" => {
                        let reg = r.region.as_deref().unwrap_or("");
                        parts.push(format!("{}{}{}", short, "✓", reg));
                    }
                    "blocked" => parts.push(format!("{}{}", short, "✗")),
                    "failed" => parts.push(format!("{}{}", short, "?")),
                    _ => {}
                }
            }
        }
        if parts.is_empty() {
            "—".to_string()
        } else {
            parts.join("  ")
        }
    }
}


#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyType {
    Ss,
    Trojan,
    Vmess,
    Vless,
    Hysteria2,
    Tuic,
    Socks5,
    Http,
    Wireguard,
    Other,
}

impl ProxyType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProxyType::Ss => "ss",
            ProxyType::Trojan => "trojan",
            ProxyType::Vmess => "vmess",
            ProxyType::Vless => "vless",
            ProxyType::Hysteria2 => "hysteria2",
            ProxyType::Tuic => "tuic",
            ProxyType::Socks5 => "socks5",
            ProxyType::Http => "http",
            ProxyType::Wireguard => "wireguard",
            ProxyType::Other => "other",
        }
    }
}

impl Proxy {
    pub fn new(name: String, type_: ProxyType, server: String, port: u16) -> Self {
        Proxy {
            name,
            type_,
            server,
            port,
            uuid: None,
            alter_id: None,
            cipher: None,
            flow: None,
            password: None,
            method: None,
            network: None,
            tls: None,
            sni: None,
            skip_cert_verify: None,
            path: None,
            host: None,
            service_name: None,
            fingerprint: None,
            latency_ms: None,
            available: None,
            download_speed_bps: None,
            bandwidth_measured: false,
            last_tested_at: None,
            consecutive_failures: 0,
            outbound_country: None,
            unlock: None,
            extra: None,
        }
    }

    /// Best-effort region guess from the node name / server host.
    ///
    /// Two-tier matching to avoid the old substring pitfall where `"us"`
    /// matched words like `"russia"`:
    ///   1. **long names / cities** first (substring — safe because the
    ///      needles are long enough not to collide with unrelated words);
    ///   2. then **2-letter country codes** matched as *whole tokens* only
    ///      (split on non-alphanumeric boundaries), so `"russia"` never
    ///      yields `"US"` and `"hk"` only matches a standalone `hk` token.
    pub fn region(&self) -> String {
        let hay = format!("{} {}", self.name.to_lowercase(), self.server.to_lowercase());

        const NAME_TABLE: &[(&str, &str)] = &[
            ("hongkong", "HK"),
            ("hong kong", "HK"),
            ("taiwan", "TW"),
            ("singapore", "SG"),
            ("japan", "JP"),
            ("tokyo", "JP"),
            ("osaka", "JP"),
            ("korea", "KR"),
            ("seoul", "KR"),
            ("usa", "US"),
            ("united states", "US"),
            ("los angeles", "US"),
            ("germany", "DE"),
            ("france", "FR"),
            ("britain", "GB"),
            ("netherlands", "NL"),
            ("russia", "RU"),
            ("canada", "CA"),
            ("australia", "AU"),
        ];
        const CODE_TABLE: &[(&str, &str)] = &[
            ("hk", "HK"),
            ("tw", "TW"),
            ("sg", "SG"),
            ("jp", "JP"),
            ("kr", "KR"),
            ("us", "US"),
            ("de", "DE"),
            ("fr", "FR"),
            ("uk", "GB"),
            ("gb", "GB"),
            ("nl", "NL"),
            ("ru", "RU"),
            ("ca", "CA"),
            ("au", "AU"),
        ];

        // 1) longer / more specific names take priority
        for (needle, code) in NAME_TABLE {
            if hay.contains(needle) {
                return code.to_string();
            }
        }
        // 2) 2-letter codes as whole tokens only
        let tokens: Vec<&str> = hay
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .collect();
        for t in &tokens {
            for (code, region) in CODE_TABLE {
                if t == code {
                    return region.to_string();
                }
            }
        }
        "OTHER".to_string()
    }

    /// Stable identity used for de-duplication and for mapping speed-test
    /// results back onto nodes.
    ///
    /// The credential portion includes every type-specific parameter that makes
    /// two nodes genuinely distinct even when they share name+server+port
    /// (common for multi-node providers reusing one hostname):
    ///   - VMess/Vless/TUIC: uuid + network + sni + tls + path + alter_id +
    ///     cipher + flow  (ws vs grpc, different SNI/TLS/path are distinct)
    ///   - SS: method|password
    ///   - Trojan/Hysteria2/Socks5/Http: password|sni|host|network|path
    ///   - Wireguard/Other: uuid|password|sni|host|network|path
    ///
    /// A `|` inside any field is escaped to `#` so a value containing the
    /// separator cannot be mistaken for a field boundary. `name` is part of the
    /// fingerprint, so renaming a node intentionally changes its identity and
    /// its previous test results are not carried over.
    pub fn fingerprint(&self) -> String {
        let s = |o: &Option<String>| o.clone().unwrap_or_default().replace('|', "#");
        let b = |o: &Option<bool>| {
            o.map(|v| if v { "1" } else { "0" })
                .unwrap_or_default()
                .to_string()
        };
        let n = |o: &Option<u32>| o.unwrap_or_default().to_string();

        let cred = match self.type_ {
            ProxyType::Vmess | ProxyType::Vless | ProxyType::Tuic => format!(
                "{}|{}|{}|{}|{}|{}|{}|{}",
                s(&self.uuid),
                s(&self.network),
                s(&self.sni),
                b(&self.tls),
                s(&self.path),
                n(&self.alter_id),
                s(&self.cipher),
                s(&self.flow)
            ),
            ProxyType::Ss => format!("{}|{}", s(&self.method), s(&self.password)),
            ProxyType::Trojan | ProxyType::Hysteria2 | ProxyType::Socks5 | ProxyType::Http => {
                format!(
                    "{}|{}|{}|{}|{}",
                    s(&self.password),
                    s(&self.sni),
                    s(&self.host),
                    s(&self.network),
                    s(&self.path)
                )
            }
            _ => format!(
                "{}|{}|{}|{}|{}|{}",
                s(&self.uuid),
                s(&self.password),
                s(&self.sni),
                s(&self.host),
                s(&self.network),
                s(&self.path)
            ),
        };
        format!(
            "{}|{}|{}|{}|{}",
            self.type_.as_str(),
            self.server,
            self.port,
            self.name,
            cred
        )
    }

    /// Whether this node has all type-specific required fields for a
    /// clash-meta / mihomo export.  Returns `false` when a critical field
    /// (password for SS/Trojan/Hysteria2, uuid for VMess/Vless/TUIC, etc.)
    /// is missing or empty — Clash's profile checker would reject the entry.
    pub fn is_exportable(&self) -> bool {
        let has = |o: &Option<String>| o.as_ref().is_some_and(|s| !s.is_empty());
        match self.type_ {
            ProxyType::Ss => has(&self.password) && has(&self.method),
            ProxyType::Trojan => has(&self.password),
            ProxyType::Vmess => has(&self.uuid),
            ProxyType::Vless => has(&self.uuid),
            ProxyType::Hysteria2 => has(&self.password),
            ProxyType::Tuic => has(&self.uuid) && has(&self.password),
            // Socks5 / Http / Wireguard: no strict required fields.
            // Other: unknown type that clash-meta/mihomo will reject outright → skip.
            ProxyType::Socks5 | ProxyType::Http | ProxyType::Wireguard => true,
            _ => false,
        }
    }

    /// Whether this node is safe to hand to a proxy client (mihomo / clash /
    /// v2rayN / sing-box). Combines `is_exportable` with a liveness check:
    /// a node is *not* usable when it lacks required fields, or has been
    /// speed-tested and found unavailable (`available == Some(false)`).
    /// Untested nodes (`available == None`) are treated as usable — they may
    /// still work, so we keep them rather than dropping a potentially-good node.
    pub fn is_usable(&self) -> bool {
        self.is_exportable() && self.available != Some(false)
    }
}

/// Merge multiple proxy lists, dropping duplicates by fingerprint.
pub fn merge(proxies: &[Proxy]) -> Vec<Proxy> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<Proxy> = Vec::new();
    for p in proxies {
        if seen.insert(p.fingerprint()) {
            out.push(p.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_proxy_bandwidth_measured_defaults_false() {
        // bandwidth_measured marks whether download_speed_bps is a real engine
        // measurement (true) or just the engine-free TCP estimate (false). It
        // must start false so the scorer never credits an unmeasured estimate.
        let p = Proxy::new(
            "n".to_string(),
            ProxyType::Ss,
            "1.2.3.4".to_string(),
            8388,
        );
        assert!(!p.bandwidth_measured, "bandwidth_measured must default to false");
    }

    #[test]
    fn fingerprint_distinguishes_same_name_diff_credential() {
        // Two VMess nodes sharing name+server+port but with different uuid must
        // NOT collapse to one fingerprint — otherwise merge/flatten_dedup would
        // silently drop a real node.
        let mut a = Proxy::new("HK-01".into(), ProxyType::Vmess, "1.2.3.4".into(), 443);
        a.uuid = Some("uuid-A".into());
        let mut b = Proxy::new("HK-01".into(), ProxyType::Vmess, "1.2.3.4".into(), 443);
        b.uuid = Some("uuid-B".into());
        assert_ne!(a.fingerprint(), b.fingerprint(), "different uuid must differ");

        // Identical credentials must still dedupe.
        let mut c = Proxy::new("HK-01".into(), ProxyType::Vmess, "1.2.3.4".into(), 443);
        c.uuid = Some("uuid-A".into());
        assert_eq!(a.fingerprint(), c.fingerprint(), "same credential must dedupe");

        // SS: different password must differ.
        let mut s1 = Proxy::new("SS-01".into(), ProxyType::Ss, "5.6.7.8".into(), 8388);
        s1.password = Some("pw1".into());
        s1.method = Some("aes-256-gcm".into());
        let mut s2 = Proxy::new("SS-01".into(), ProxyType::Ss, "5.6.7.8".into(), 8388);
        s2.password = Some("pw2".into());
        s2.method = Some("aes-256-gcm".into());
        assert_ne!(s1.fingerprint(), s2.fingerprint(), "different ss password must differ");
    }

    #[test]
    fn fingerprint_distinguishes_trojan_same_addr_diff_sni_path() {
        // Q3: two Trojan nodes on the same host/port that differ only in SNI or
        // path are genuinely different nodes — they must NOT share a fingerprint.
        let mut a = Proxy::new("T".into(), ProxyType::Trojan, "1.2.3.4".into(), 443);
        a.password = Some("pw".into());
        a.sni = Some("a.example.com".into());
        a.path = Some("/v1".into());
        let mut b = Proxy::new("T".into(), ProxyType::Trojan, "1.2.3.4".into(), 443);
        b.password = Some("pw".into());
        b.sni = Some("b.example.com".into()); // different SNI
        b.path = Some("/v1".into());
        assert_ne!(
            a.fingerprint(),
            b.fingerprint(),
            "Trojan with different SNI must not collapse"
        );

        // Same SNI + path (everything else equal) must still dedupe.
        let mut c = Proxy::new("T".into(), ProxyType::Trojan, "1.2.3.4".into(), 443);
        c.password = Some("pw".into());
        c.sni = Some("a.example.com".into());
        c.path = Some("/v1".into());
        assert_eq!(a.fingerprint(), c.fingerprint(), "identical Trojan must dedupe");
    }
}
