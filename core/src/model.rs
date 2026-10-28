use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

static SUB_COUNTER: AtomicU64 = AtomicU64::new(1);
static SUB_UID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Stable-ish unique id for a subscription (dependency-free).
pub fn new_sub_id() -> String {
    format!("sub_{}", SUB_COUNTER.fetch_add(1, Ordering::SeqCst))
}

/// Stable, unguessable identity used by backup/restore. Sequential `sub_N` ids
/// are user-visible and easy to spoof; local subscriptions have no source URL,
/// so they need their own key to make repeated imports idempotent.
pub fn new_sub_uid() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "u_{nanos:x}_{:x}",
        SUB_UID_COUNTER.fetch_add(1, Ordering::SeqCst)
    )
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
    /// 最后一次「拥有至少一个可用节点」的时间（epoch millis）。
    /// 供「连续 N 天无可用节点则自动删除订阅」功能衡量订阅已持续不可用多久：
    /// 当前无可用节点且距该时间已超过阈值时，视为「长期不可用」予以清理。
    pub last_healthy_at: Option<u64>,
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
            // 平均/最佳延迟只统计「未被判死」的节点：测速流程会在判死时清空
            // `latency_ms`，但增量刷新会把旧节点的两个字段一起带过来，历史数据
            // 或第三方写入都可能让死节点带着上轮的 10ms。让死节点参与平均值会
            // 出现「最佳延迟来自一个面板上显示为不可用的节点」。
            if p.available != Some(false) {
                if let Some(ms) = p.latency_ms {
                    sum += ms;
                    n += 1;
                    best = Some(best.map_or(ms, |b| b.min(ms)));
                }
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
    /// Stable backup/restore identity (see [`new_sub_uid`]). Old persisted
    /// entries lack it; `run_server` backfills them at startup.
    #[serde(default)]
    pub uid: String,
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
            uid: new_sub_uid(),
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

    /// Backfill the backup identity for subscriptions persisted before `uid`
    /// existed.
    pub fn ensure_uid(&mut self) {
        if self.uid.trim().is_empty() {
            self.uid = new_sub_uid();
        }
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

    // SS 插件（obfs / v2ray-plugin）与 Hysteria2 obfs。`plugin_opts` 保持
    // SIP002 的 `k=v;k2=v2` 原始形态，导出时再按目标格式转换。
    #[serde(default)]
    pub plugin: Option<String>,
    #[serde(default)]
    pub plugin_opts: Option<String>,
    #[serde(default)]
    pub obfs: Option<String>,
    #[serde(default)]
    pub obfs_password: Option<String>,

    // TLS ALPN and QUIC/protocol tuning. Field names follow the model's
    // canonical (mostly sing-box) form; exporters translate to each client:
    //   TUIC: congestion_control / udp_relay_mode / udp_over_stream /
    //         zero_rtt_handshake (mihomo: congestion-controller /
    //         udp-relay-mode / udp-over-stream / reduce-rtt)
    //   Hysteria2: up_mbps / down_mbps (mihomo: up / down strings)
    #[serde(default)]
    pub alpn: Option<Vec<String>>,
    #[serde(default)]
    pub congestion_control: Option<String>,
    #[serde(default)]
    pub udp_relay_mode: Option<String>,
    #[serde(default)]
    pub udp_over_stream: Option<bool>,
    #[serde(default)]
    pub zero_rtt_handshake: Option<bool>,
    #[serde(default)]
    pub up_mbps: Option<u64>,
    #[serde(default)]
    pub down_mbps: Option<u64>,

    // transport / tls
    pub network: Option<String>, // tcp / ws / grpc / h2
    pub tls: Option<bool>,
    pub sni: Option<String>,
    pub skip_cert_verify: Option<bool>,
    pub path: Option<String>,
    pub host: Option<String>,
    pub service_name: Option<String>, // grpc
    pub fingerprint: Option<String>,

    // VLESS REALITY (mihomo / xray): `security=reality` + `pbk` / `sid`.
    // Kept as explicit fields (not buried in `extra`) because every exporter
    // has to emit them in a protocol-specific shape; without them a REALITY
    // node degrades to plain TLS and can never complete a handshake.
    #[serde(default)]
    pub reality: bool,
    #[serde(default)]
    pub reality_public_key: Option<String>,
    #[serde(default)]
    pub reality_short_id: Option<String>,

    // speed-test results (populated by the speedtest engine)
    pub latency_ms: Option<u64>,
    /// 裸 TCP 建连到节点服务器的耗时（未走代理隧道，仅反映到节点的网络距离）。
    pub tcp_latency_ms: Option<u64>,
    /// 经代理隧道访问 Google 204 的 HTTP 往返耗时（实测连通性的真实端到端延迟）。
    pub http_latency_ms: Option<u64>,
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
    AnyTls,
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
            ProxyType::AnyTls => "anytls",
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
            plugin: None,
            plugin_opts: None,
            obfs: None,
            obfs_password: None,
            alpn: None,
            congestion_control: None,
            udp_relay_mode: None,
            udp_over_stream: None,
            zero_rtt_handshake: None,
            up_mbps: None,
            down_mbps: None,
            network: None,
            tls: None,
            sni: None,
            skip_cert_verify: None,
            path: None,
            host: None,
            service_name: None,
            fingerprint: None,
            reality: false,
            reality_public_key: None,
            reality_short_id: None,
            latency_ms: None,
            tcp_latency_ms: None,
            http_latency_ms: None,
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
    /// Four-tier matching, most authoritative first:
    ///   0. **flag emoji** — two consecutive Regional_Indicator codepoints map
    ///      directly to a 2-letter code (🇮🇸→IS, 🇦🇪→AE, 🇲🇾→MY …), covering any
    ///      country flag without an explicit lookup table;
    ///   1. **full names / Chinese / English country names** (substring — safe
    ///      because the needles are long enough not to collide with words);
    ///   2. **3-letter airport/city codes** as *whole tokens* only
    ///      (e.g. `hkg`→HK, `nrt`→JP, `ist`→TR);
    ///   3. **2-letter country codes** as whole tokens, plus a *safe prefix*
    ///      variant (`us-01`, `jp2` → US/JP) where the code is followed by a
    ///      non-alphabetic char — so `russia`/`usa`/`ukraine` never match.
    pub fn region(&self) -> String {
        // 0) flag emoji → 2-letter code (authoritative, highest priority)
        // 相邻两字符滑窗直接扫 `chars()`：旧实现先把名字物化成 `Vec<char>`，
        // 每个节点的每一次地区判定都多一次堆分配（/sub 一轮要判几千次）。
        let is_flag = |c: char| (0x1F1E6..=0x1F1FF).contains(&(c as u32));
        let mut prev: Option<char> = None;
        for b in self.name.chars() {
            if let Some(a) = prev {
                if is_flag(a) && is_flag(b) {
                    let c1 = (a as u32 - 0x1F1E6) as u8 + b'A';
                    let c2 = (b as u32 - 0x1F1E6) as u8 + b'A';
                    return format!("{}{}", c1 as char, c2 as char);
                }
            }
            prev = Some(b);
        }

        // 名字 + 服务器合并成一份小写文本，供子串表与码表匹配。逐字符
        // `to_lowercase` 直接写进目标串，省掉两个临时 String 和一次 `format!`。
        let mut hay = String::with_capacity(self.name.len() + self.server.len() + 1);
        hay.extend(self.name.chars().flat_map(char::to_lowercase));
        hay.push(' ');
        hay.extend(self.server.chars().flat_map(char::to_lowercase));

        // 1) full names / Chinese / English country names (substring, safe)
        const NAME_TABLE: &[(&str, &str)] = &[
            ("hongkong", "HK"),
            ("hong kong", "HK"),
            ("香港", "HK"),
            ("taiwan", "TW"),
            ("台湾", "TW"),
            ("臺灣", "TW"),
            ("singapore", "SG"),
            ("新加坡", "SG"),
            ("狮城", "SG"),
            ("japan", "JP"),
            ("日本", "JP"),
            ("tokyo", "JP"),
            ("东京", "JP"),
            ("osaka", "JP"),
            ("大阪", "JP"),
            ("korea", "KR"),
            ("韩国", "KR"),
            ("韓國", "KR"),
            ("seoul", "KR"),
            ("首尔", "KR"),
            ("首爾", "KR"),
            ("usa", "US"),
            ("united states", "US"),
            ("america", "US"),
            ("美国", "US"),
            ("美國", "US"),
            ("los angeles", "US"),
            ("germany", "DE"),
            ("德国", "DE"),
            ("deutschland", "DE"),
            ("france", "FR"),
            ("法国", "FR"),
            ("法國", "FR"),
            ("français", "FR"),
            ("britain", "GB"),
            ("英国", "GB"),
            ("英國", "GB"),
            ("united kingdom", "GB"),
            ("netherlands", "NL"),
            ("荷兰", "NL"),
            ("holland", "NL"),
            ("nederland", "NL"),
            ("russia", "RU"),
            ("俄罗斯", "RU"),
            ("俄羅斯", "RU"),
            ("rossiya", "RU"),
            ("canada", "CA"),
            ("加拿大", "CA"),
            ("australia", "AU"),
            ("澳大利亚", "AU"),
            ("澳洲", "AU"),
            ("malaysia", "MY"),
            ("马来西亚", "MY"),
            ("大马", "MY"),
            ("turkey", "TR"),
            ("土耳其", "TR"),
            ("argentina", "AR"),
            ("阿根廷", "AR"),
            ("thailand", "TH"),
            ("泰国", "TH"),
            ("india", "IN"),
            ("印度", "IN"),
            ("vietnam", "VN"),
            ("越南", "VN"),
            ("philippines", "PH"),
            ("菲律宾", "PH"),
            ("indonesia", "ID"),
            ("印尼", "ID"),
            ("brazil", "BR"),
            ("巴西", "BR"),
            ("mexico", "MX"),
            ("墨西哥", "MX"),
            ("spain", "ES"),
            ("西班牙", "ES"),
            ("italy", "IT"),
            ("意大利", "IT"),
            ("ukraine", "UA"),
            ("乌克兰", "UA"),
            ("kazakhstan", "KZ"),
            ("哈萨克斯坦", "KZ"),
            ("uzbekistan", "UZ"),
            ("乌兹别克", "UZ"),
            ("iran", "IR"),
            ("伊朗", "IR"),
            ("iraq", "IQ"),
            ("伊拉克", "IQ"),
            ("egypt", "EG"),
            ("埃及", "EG"),
            ("saudi", "SA"),
            ("沙特", "SA"),
            ("uae", "AE"),
            ("阿联酋", "AE"),
            ("迪拜", "AE"),
            ("south africa", "ZA"),
            ("南非", "ZA"),
            ("chile", "CL"),
            ("智利", "CL"),
            ("colombia", "CO"),
            ("哥伦比亚", "CO"),
            ("poland", "PL"),
            ("波兰", "PL"),
            ("sweden", "SE"),
            ("瑞典", "SE"),
            ("norway", "NO"),
            ("挪威", "NO"),
            ("finland", "FI"),
            ("芬兰", "FI"),
            ("denmark", "DK"),
            ("丹麦", "DK"),
            ("switzerland", "CH"),
            ("瑞士", "CH"),
            ("austria", "AT"),
            ("奥地利", "AT"),
            ("belgium", "BE"),
            ("比利时", "BE"),
            ("portugal", "PT"),
            ("葡萄牙", "PT"),
            ("greece", "GR"),
            ("希腊", "GR"),
            ("new zealand", "NZ"),
            ("新西兰", "NZ"),
            ("mongolia", "MN"),
            ("蒙古", "MN"),
            ("cambodia", "KH"),
            ("柬埔寨", "KH"),
            ("myanmar", "MM"),
            ("缅甸", "MM"),
            ("laos", "LA"),
            ("老挝", "LA"),
            ("nepal", "NP"),
            ("尼泊尔", "NP"),
            ("pakistan", "PK"),
            ("巴基斯坦", "PK"),
            ("bangladesh", "BD"),
            ("孟加拉", "BD"),
            ("sri lanka", "LK"),
            ("斯里兰卡", "LK"),
            ("ireland", "IE"),
            ("爱尔兰", "IE"),
            ("romania", "RO"),
            ("罗马尼亚", "RO"),
            ("hungary", "HU"),
            ("匈牙利", "HU"),
            ("luxembourg", "LU"),
            ("卢森堡", "LU"),
            ("croatia", "HR"),
            ("克罗地亚", "HR"),
            ("iceland", "IS"),
            ("冰岛", "IS"),
            ("israel", "IL"),
            ("以色列", "IL"),
            ("estonia", "EE"),
            ("爱沙尼亚", "EE"),
            ("latvia", "LV"),
            ("拉脱维亚", "LV"),
            ("bulgaria", "BG"),
            ("保加利亚", "BG"),
            ("slovakia", "SK"),
            ("斯洛伐克", "SK"),
            ("czech", "CZ"),
            ("捷克", "CZ"),
            ("lithuania", "LT"),
            ("立陶宛", "LT"),
            ("costa rica", "CR"),
            ("哥斯达黎加", "CR"),
            ("cyprus", "CY"),
            ("塞浦路斯", "CY"),
            ("georgia", "GE"),
            ("格鲁吉亚", "GE"),
            ("jordan", "JO"),
            ("约旦", "JO"),
            ("kyrgyzstan", "KG"),
            ("吉尔吉斯斯坦", "KG"),
            ("morocco", "MA"),
            ("摩洛哥", "MA"),
            ("malta", "MT"),
            ("马耳他", "MT"),
            ("peru", "PE"),
            ("秘鲁", "PE"),
            ("puerto rico", "PR"),
            ("波多黎各", "PR"),
            ("paraguay", "PY"),
            ("巴拉圭", "PY"),
            ("armenia", "AM"),
            ("亚美尼亚", "AM"),
            ("macau", "MO"),
            ("澳门", "MO"),
            ("ecuador", "EC"),
            ("厄瓜多尔", "EC"),
            ("algeria", "DZ"),
            ("阿尔及利亚", "DZ"),
            ("guatemala", "GT"),
            ("危地马拉", "GT"),
            ("ghana", "GH"),
            ("加纳", "GH"),
            ("kenya", "KE"),
            ("肯尼亚", "KE"),
            ("kuwait", "KW"),
            ("科威特", "KW"),
            ("nigeria", "NG"),
            ("尼日利亚", "NG"),
            ("nicaragua", "NI"),
            ("尼加拉瓜", "NI"),
            ("panama", "PA"),
            ("巴拿马", "PA"),
            ("uruguay", "UY"),
            ("乌拉圭", "UY"),
            ("china", "CN"),
            ("中国", "CN"),
        ];
        for (needle, code) in NAME_TABLE {
            if hay.contains(needle) {
                return code.to_string();
            }
        }

        // tokenize for code matching
        let tokens: Vec<&str> = hay
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .collect();

        // 2) 3-letter codes as whole tokens only (avoids substring pitfalls)
        const CODE3_TABLE: &[(&str, &str)] = &[
            ("hkg", "HK"),
            ("tpe", "TW"),
            ("taipei", "TW"),
            ("sin", "SG"),
            ("nrt", "JP"),
            ("tyo", "JP"),
            ("hnd", "JP"),
            ("kix", "JP"),
            ("icn", "KR"),
            ("lax", "US"),
            ("sfo", "US"),
            ("jfk", "US"),
            ("ord", "US"),
            ("fra", "DE"),
            ("ber", "DE"),
            ("mun", "DE"),
            ("par", "FR"),
            ("cdg", "FR"),
            ("lon", "GB"),
            ("lhr", "GB"),
            ("ams", "NL"),
            ("rtm", "NL"),
            ("yyz", "CA"),
            ("syd", "AU"),
            ("mel", "AU"),
            ("mow", "RU"),
            ("led", "RU"),
            ("svx", "RU"),
            ("ist", "TR"),
            ("kul", "MY"),
            ("bkk", "TH"),
            ("del", "IN"),
            ("bom", "IN"),
            ("dxb", "AE"),
            ("doh", "AE"),
            ("svo", "RU"),
            ("pek", "CN"),
            ("sha", "CN"),
            ("ctu", "CN"),
            ("can", "CN"),
            ("gru", "BR"),
            ("mad", "ES"),
            ("bcn", "ES"),
            ("fco", "IT"),
            ("zrh", "CH"),
            ("vie", "AT"),
            ("bru", "BE"),
            ("cph", "DK"),
            ("osl", "NO"),
            ("arn", "SE"),
            ("hel", "FI"),
            ("waw", "PL"),
            ("prg", "CZ"),
            ("bud", "HU"),
            ("otp", "RO"),
            ("dub", "IE"),
            ("lis", "PT"),
            ("ath", "GR"),
            ("gva", "CH"),
            ("sxf", "DE"),
            ("pmi", "ES"),
            ("agp", "ES"),
        ];
        for t in &tokens {
            for (code, region) in CODE3_TABLE {
                if t == code {
                    return region.to_string();
                }
            }
        }

        // 3) 2-letter codes: whole token OR safe prefix (code followed by a
        //    non-alphabetic char, e.g. `us-01` / `jp2`); this blocks `russia`,
        //    `usa`, `ukraine` from matching `ru` / `us` / `uk`.
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
            ("my", "MY"),
            ("tr", "TR"),
            ("ar", "AR"),
            ("th", "TH"),
            ("vn", "VN"),
            ("ph", "PH"),
            ("id", "ID"),
            ("br", "BR"),
            ("mx", "MX"),
            ("es", "ES"),
            ("it", "IT"),
            ("ua", "UA"),
            ("kz", "KZ"),
            ("uz", "UZ"),
            ("ir", "IR"),
            ("iq", "IQ"),
            ("eg", "EG"),
            ("sa", "SA"),
            ("ae", "AE"),
            ("za", "ZA"),
            ("cl", "CL"),
            ("co", "CO"),
            ("pl", "PL"),
            ("se", "SE"),
            ("no", "NO"),
            ("fi", "FI"),
            ("dk", "DK"),
            ("ch", "CH"),
            ("at", "AT"),
            ("be", "BE"),
            ("pt", "PT"),
            ("gr", "GR"),
            ("nz", "NZ"),
            ("mn", "MN"),
            ("kh", "KH"),
            ("mm", "MM"),
            ("la", "LA"),
            ("np", "NP"),
            ("pk", "PK"),
            ("bd", "BD"),
            ("lk", "LK"),
            ("ie", "IE"),
            ("ro", "RO"),
            ("hu", "HU"),
            ("lu", "LU"),
            ("hr", "HR"),
            ("is", "IS"),
            ("il", "IL"),
            ("ee", "EE"),
            ("lv", "LV"),
            ("bg", "BG"),
            ("sk", "SK"),
            ("cz", "CZ"),
            ("lt", "LT"),
            ("cr", "CR"),
            ("cy", "CY"),
            ("ge", "GE"),
            ("jo", "JO"),
            ("kg", "KG"),
            ("ma", "MA"),
            ("mt", "MT"),
            ("pe", "PE"),
            ("pr", "PR"),
            ("py", "PY"),
            ("am", "AM"),
            ("mo", "MO"),
            ("ec", "EC"),
            ("dz", "DZ"),
            ("gt", "GT"),
            ("gh", "GH"),
            ("ke", "KE"),
            ("kw", "KW"),
            ("ng", "NG"),
            ("ni", "NI"),
            ("pa", "PA"),
            ("uy", "UY"),
            ("cn", "CN"),
        ];
        for t in &tokens {
            for (code, region) in CODE_TABLE {
                let matches = t == code
                    || (t.len() > code.len()
                        && t.starts_with(code)
                        && !t[code.len()..].chars().next().unwrap().is_alphabetic());
                if matches {
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
    ///   - SS: 归一后的加密方法（`method` 或 `cipher`，见 [`Proxy::ss_method_value`]）
    ///     + password
    ///   - Trojan/Hysteria2/Socks5/Http: password|sni|host|network|path
    ///   - Wireguard/Other: uuid|password|sni|host|network|path
    ///
    /// A `|` inside any field — including `name` and `server`, which real
    /// subscriptions do contain (e.g. `🇺🇸美国-电信|5倍`) — is escaped to `#` so a
    /// value containing the separator can never be mistaken for a field
    /// boundary and make two different nodes look identical. `name` is part of
    /// the fingerprint, so renaming a node intentionally changes its identity
    /// and its previous test results are not carried over.
    pub fn fingerprint(&self) -> String {
        let s = |o: &Option<String>| o.clone().unwrap_or_default().replace('|', "#");
        let b = |o: &Option<bool>| {
            o.map(|v| if v { "1" } else { "0" })
                .unwrap_or_default()
                .to_string()
        };
        let n = |o: &Option<u32>| o.unwrap_or_default().to_string();
        let esc = |v: &str| v.replace('|', "#");

        let mut cred = match self.type_ {
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
            ProxyType::Ss => format!(
                "{}|{}",
                self.ss_method_value().unwrap_or_default().replace('|', "#"),
                s(&self.password)
            ),
            ProxyType::Trojan
            | ProxyType::Hysteria2
            | ProxyType::Socks5
            | ProxyType::Http
            | ProxyType::AnyTls => {
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

        // WS `Host` / gRPC `service_name` / REALITY keys / SS plugin /
        // Hysteria2 obfs distinguish genuinely different nodes that share
        // server+port+credential. Append them only when present so nodes
        // without these fields keep their existing fingerprint (and their
        // measured health survives the upgrade).
        let mut extra: Vec<String> = Vec::new();
        if matches!(
            self.type_,
            ProxyType::Vmess | ProxyType::Vless | ProxyType::Tuic
        ) {
            if let Some(h) = self.host.as_deref().filter(|s| !s.is_empty()) {
                extra.push(format!("h={}", h.replace('|', "#")));
            }
            if let Some(svc) = self.service_name.as_deref().filter(|s| !s.is_empty()) {
                extra.push(format!("svc={}", svc.replace('|', "#")));
            }
            if self.reality {
                extra.push("reality".to_string());
                if let Some(pk) = self.reality_public_key.as_deref().filter(|s| !s.is_empty()) {
                    extra.push(format!("pk={}", pk.replace('|', "#")));
                }
                if let Some(sid) = self.reality_short_id.as_deref().filter(|s| !s.is_empty()) {
                    extra.push(format!("sid={}", sid.replace('|', "#")));
                }
            }
        }
        if self.type_ == ProxyType::Ss {
            if let Some(plugin) = self.plugin.as_deref().filter(|s| !s.is_empty()) {
                extra.push(format!("plugin={}", plugin.replace('|', "#")));
            }
            if let Some(opts) = self.plugin_opts.as_deref().filter(|s| !s.is_empty()) {
                extra.push(format!("opts={}", opts.replace('|', "#")));
            }
        }
        if self.type_ == ProxyType::Hysteria2 {
            if let Some(obfs) = self.obfs.as_deref().filter(|s| !s.is_empty()) {
                extra.push(format!("obfs={}", obfs.replace('|', "#")));
            }
            if let Some(pw) = self.obfs_password.as_deref().filter(|s| !s.is_empty()) {
                extra.push(format!("opw={}", pw.replace('|', "#")));
            }
            if let Some(up) = self.up_mbps {
                extra.push(format!("up={up}"));
            }
            if let Some(down) = self.down_mbps {
                extra.push(format!("down={down}"));
            }
        }
        if let Some(alpn) = &self.alpn {
            if !alpn.is_empty() {
                extra.push(format!("alpn={}", alpn.join(",").replace('|', "#")));
            }
        }
        if self.type_ == ProxyType::Tuic {
            if let Some(cc) = &self.congestion_control {
                extra.push(format!("cc={}", cc.replace('|', "#")));
            }
            if let Some(mode) = &self.udp_relay_mode {
                extra.push(format!("urm={}", mode.replace('|', "#")));
            }
            if let Some(v) = self.udp_over_stream {
                extra.push(format!("uos={}", if v { "1" } else { "0" }));
            }
            if let Some(v) = self.zero_rtt_handshake {
                extra.push(format!("zrtt={}", if v { "1" } else { "0" }));
            }
        }
        if !extra.is_empty() {
            cred.push('|');
            cred.push_str(&extra.join("|"));
        }

        format!(
            "{}|{}|{}|{}|{}",
            self.type_.as_str(),
            esc(&self.server),
            self.port,
            esc(&self.name),
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
            // SS 的加密方法可能存于 `cipher`（clash-meta YAML 订阅）或 `method`
            // （SIP002 URI 解析）任一字段，二者满足其一即视为完整。旧代码只读
            // `method` 会把「从 YAML 订阅导入、cipher 有值而 method 为 null」的 SS
            // 节点误判为不完整而丢弃，导致引擎跳过该节点、只能回退 TCP 估计（0.0MB/s）。
            ProxyType::Ss => has(&self.password) && (has(&self.cipher) || has(&self.method)),
            ProxyType::Trojan | ProxyType::AnyTls => has(&self.password),
            ProxyType::Vmess => has(&self.uuid),
            // REALITY without a public key can only be exported as plain TLS,
            // which can never complete the handshake — treat it as incomplete
            // (and let the engine skip it) instead of emitting a dead node.
            ProxyType::Vless => has(&self.uuid) && (!self.reality || has(&self.reality_public_key)),
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
    merge_refs(&proxies.iter().collect::<Vec<_>>())
        .into_iter()
        .cloned()
        .collect()
}

/// [`merge`] 的借用版：按指纹去重但**不克隆节点**，返回指向输入元素的引用。
/// 仪表盘汇总、地区/类型分布这类只读遍历用它，可以省掉整库几千个 `Proxy`
/// （每个含 ~15 个可选字符串）的深拷贝。
pub fn merge_refs<'a>(proxies: &[&'a Proxy]) -> Vec<&'a Proxy> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<&Proxy> = Vec::new();
    for p in proxies {
        if seen.insert(p.fingerprint()) {
            out.push(*p);
        }
    }
    out
}

impl Proxy {
    /// SS 加密方法的归一取值：clash-meta YAML 订阅把它写在 `cipher`，SIP002
    /// URI 写在 `method`，同一物理节点只会填充其中一个。指纹与所有导出器都必须
    /// 读这同一个归一值，否则
    /// - 同一机场的 YAML / base64 两种订阅形态得到不同指纹，`merge` 不去重，
    ///   导出里出现 `A` 与 `A #2` 两条指向同一节点的条目；
    /// - 订阅在两种形态之间切换时 `incremental_update` 会把全部 SS 节点判为
    ///   新节点，已测延迟与连续失败计数被清空。
    pub fn ss_method_value(&self) -> Option<&str> {
        self.method
            .as_deref()
            .or(self.cipher.as_deref())
            .filter(|s| !s.is_empty())
    }

    /// 与节点列表「地区」列一致的「有效地区」：优先用实测出口地区
    /// `outbound_country`（经测速引擎真连测出的真实出口国家），仅在未做出口
    /// 检测（或检测为空）时回退到从名字猜测的 `region()`。让地区筛选 / 地区分布
    /// 与列表显示保持一致——列表列本身就是 `outbound_country || region`。
    pub fn effective_region(&self) -> String {
        self.outbound_country
            .as_ref()
            .filter(|c| !c.is_empty())
            .cloned()
            .unwrap_or_else(|| self.region())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_uid_is_stable_and_backfilled() {
        let a = Subscription::new("A".into(), "pasted".into(), vec![]);
        let b = Subscription::new("B".into(), "pasted".into(), vec![]);
        assert!(!a.uid.is_empty(), "新订阅必须有 uid");
        assert_ne!(a.uid, b.uid, "uid 必须唯一");

        let mut old = Subscription::new("old".into(), "pasted".into(), vec![]);
        old.uid.clear();
        old.ensure_uid();
        assert!(!old.uid.is_empty(), "旧订阅应回填 uid");
        let before = old.uid.clone();
        old.ensure_uid();
        assert_eq!(old.uid, before, "ensure_uid 不得覆盖已有值");
    }

    #[test]
    fn new_proxy_bandwidth_measured_defaults_false() {
        // bandwidth_measured marks whether download_speed_bps is a real engine
        // measurement (true) or just the engine-free TCP estimate (false). It
        // must start false so the scorer never credits an unmeasured estimate.
        let p = Proxy::new("n".to_string(), ProxyType::Ss, "1.2.3.4".to_string(), 8388);
        assert!(
            !p.bandwidth_measured,
            "bandwidth_measured must default to false"
        );
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
        assert_ne!(
            a.fingerprint(),
            b.fingerprint(),
            "different uuid must differ"
        );

        // Identical credentials must still dedupe.
        let mut c = Proxy::new("HK-01".into(), ProxyType::Vmess, "1.2.3.4".into(), 443);
        c.uuid = Some("uuid-A".into());
        assert_eq!(
            a.fingerprint(),
            c.fingerprint(),
            "same credential must dedupe"
        );

        // SS: different password must differ.
        let mut s1 = Proxy::new("SS-01".into(), ProxyType::Ss, "5.6.7.8".into(), 8388);
        s1.password = Some("pw1".into());
        s1.method = Some("aes-256-gcm".into());
        let mut s2 = Proxy::new("SS-01".into(), ProxyType::Ss, "5.6.7.8".into(), 8388);
        s2.password = Some("pw2".into());
        s2.method = Some("aes-256-gcm".into());
        assert_ne!(
            s1.fingerprint(),
            s2.fingerprint(),
            "different ss password must differ"
        );
    }

    #[test]
    fn is_exportable_ss_accepts_cipher_or_method() {
        // Regression guard for the SS exportability fix: an SS node imported from
        // a clash-meta YAML subscription stores its cipher in `cipher` with
        // `method` null. Either field must satisfy "complete"; previously only
        // `method` was checked, so these nodes were dropped (engine skipped →
        // 0.0MB/s). SIP002 URI imports use `method`, so both must work.
        let mut a = Proxy::new("SS-cipher".into(), ProxyType::Ss, "1.2.3.4".into(), 8388);
        a.password = Some("pw".into());
        a.cipher = Some("aes-128-gcm".into()); // method is None
        assert!(
            a.is_exportable(),
            "SS with cipher (YAML-import style) must be exportable"
        );

        let mut b = Proxy::new("SS-method".into(), ProxyType::Ss, "1.2.3.4".into(), 8388);
        b.password = Some("pw".into());
        b.method = Some("aes-256-gcm".into()); // cipher is None
        assert!(
            b.is_exportable(),
            "SS with method (SIP002 style) must be exportable"
        );

        // Missing both cipher and method → incomplete, must NOT be exportable.
        let mut c = Proxy::new("SS-nothing".into(), ProxyType::Ss, "1.2.3.4".into(), 8388);
        c.password = Some("pw".into());
        assert!(
            !c.is_exportable(),
            "SS with neither cipher nor method is incomplete"
        );
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
        assert_eq!(
            a.fingerprint(),
            c.fingerprint(),
            "identical Trojan must dedupe"
        );
    }

    #[test]
    fn fingerprint_distinguishes_ws_host_and_grpc_service() {
        // 同一 server:port:uuid 的多节点机场常用不同 WS Host / gRPC service
        // 区分线路；这些字段必须在指纹里，否则合并去重会丢节点。
        let mut a = Proxy::new("N".into(), ProxyType::Vless, "1.2.3.4".into(), 443);
        a.uuid = Some("u".into());
        a.network = Some("ws".into());
        a.host = Some("a.example.com".into());
        let mut b = a.clone();
        b.host = Some("b.example.com".into());
        assert_ne!(a.fingerprint(), b.fingerprint(), "不同 WS Host 不得合并");

        let mut c = a.clone();
        c.network = Some("grpc".into());
        c.service_name = Some("svc-a".into());
        let mut d = c.clone();
        d.service_name = Some("svc-b".into());
        assert_ne!(
            c.fingerprint(),
            d.fingerprint(),
            "不同 gRPC service 不得合并"
        );

        // 没有这些字段的节点保持原有指纹格式（不受本次改动影响）。
        let mut e = Proxy::new("N".into(), ProxyType::Vmess, "1.2.3.4".into(), 443);
        e.uuid = Some("u".into());
        let f = e.clone();
        assert_eq!(e.fingerprint(), f.fingerprint());
    }

    #[test]
    fn fingerprint_includes_reality_keys() {
        let mut a = Proxy::new("R".into(), ProxyType::Vless, "1.2.3.4".into(), 443);
        a.uuid = Some("u".into());
        a.reality = true;
        a.reality_public_key = Some("pk-a".into());
        a.reality_short_id = Some("01".into());
        let mut b = a.clone();
        b.reality_public_key = Some("pk-b".into());
        assert_ne!(
            a.fingerprint(),
            b.fingerprint(),
            "不同 REALITY 公钥不得合并"
        );
    }

    #[test]
    fn fingerprint_includes_ss_plugin_and_hysteria2_obfs() {
        let mut a = Proxy::new("S".into(), ProxyType::Ss, "1.2.3.4".into(), 8388);
        a.password = Some("pw".into());
        a.method = Some("aes-256-gcm".into());
        let mut b = a.clone();
        b.plugin = Some("obfs-local".into());
        b.plugin_opts = Some("obfs=http".into());
        assert_ne!(a.fingerprint(), b.fingerprint(), "SS 插件必须影响指纹");

        let mut h1 = Proxy::new("H".into(), ProxyType::Hysteria2, "5.6.7.8".into(), 443);
        h1.password = Some("pw".into());
        let mut h2 = h1.clone();
        h2.obfs = Some("salamander".into());
        assert_ne!(h1.fingerprint(), h2.fingerprint(), "HY2 obfs 必须影响指纹");

        let mut t1 = Proxy::new("T".into(), ProxyType::Tuic, "9.9.9.9".into(), 443);
        t1.uuid = Some("u".into());
        t1.password = Some("pw".into());
        let mut t2 = t1.clone();
        t2.congestion_control = Some("bbr".into());
        t2.alpn = Some(vec!["h3".into()]);
        assert_ne!(
            t1.fingerprint(),
            t2.fingerprint(),
            "TUIC 高级参数必须影响指纹"
        );
    }

    #[test]
    fn is_exportable_vless_reality_requires_public_key() {
        let mut p = Proxy::new("R".into(), ProxyType::Vless, "1.2.3.4".into(), 443);
        p.uuid = Some("u".into());
        p.reality = true;
        assert!(!p.is_exportable(), "REALITY 缺公钥无法握手，应判不完整");
        p.reality_public_key = Some("pk".into());
        assert!(p.is_exportable());
    }

    #[test]
    fn region_no_false_positive_from_hong_substring() {
        // 旧 NAME_TABLE 含短子串 "hong"，会把 chongqing 误判成 HK。
        assert_eq!(
            Proxy::new(
                "chongqing-01".into(),
                ProxyType::Vmess,
                "1.2.3.4".into(),
                443
            )
            .region(),
            "OTHER"
        );
        // 正常的 hong kong 写法仍识别为 HK。
        assert_eq!(
            Proxy::new(
                "Hong Kong 01".into(),
                ProxyType::Vmess,
                "1.2.3.4".into(),
                443
            )
            .region(),
            "HK"
        );
    }

    #[test]
    fn region_resolves_common_naming_styles() {
        // 中文地名
        assert_eq!(
            Proxy::new("香港节点01".into(), ProxyType::Vmess, "1.2.3.4".into(), 443).region(),
            "HK"
        );
        assert_eq!(
            Proxy::new("日本东京".into(), ProxyType::Vmess, "1.2.3.4".into(), 443).region(),
            "JP"
        );
        assert_eq!(
            Proxy::new("美国".into(), ProxyType::Vmess, "1.2.3.4".into(), 443).region(),
            "US"
        );
        // 3 字母机场码
        assert_eq!(
            Proxy::new("HK-HKG-001".into(), ProxyType::Vmess, "1.2.3.4".into(), 443).region(),
            "HK"
        );
        assert_eq!(
            Proxy::new("NRT 节点".into(), ProxyType::Vmess, "1.2.3.4".into(), 443).region(),
            "JP"
        );
    }

    #[test]
    fn region_prefix_variant_safe_from_false_positive() {
        // 带连字符/数字后缀的前缀变体应识别
        assert_eq!(
            Proxy::new("us-01".into(), ProxyType::Vmess, "1.2.3.4".into(), 443).region(),
            "US"
        );
        assert_eq!(
            Proxy::new("jp2".into(), ProxyType::Vmess, "1.2.3.4".into(), 443).region(),
            "JP"
        );
        // 整词误判需被排除：russia / ukraine 经 NAME_TABLE 正确识别为 RU / UA；
        // ruse 的 ru 前缀后跟字母，不得命中 2 字母码 → OTHER
        assert_eq!(
            Proxy::new(
                "russia-moscow".into(),
                ProxyType::Vmess,
                "1.2.3.4".into(),
                443
            )
            .region(),
            "RU"
        );
        assert_eq!(
            Proxy::new(
                "ukraine-kyiv".into(),
                ProxyType::Vmess,
                "1.2.3.4".into(),
                443
            )
            .region(),
            "UA"
        );
        assert_eq!(
            Proxy::new("ruse-node".into(), ProxyType::Vmess, "1.2.3.4".into(), 443).region(),
            "OTHER"
        );
    }

    #[test]
    fn region_flag_emoji_maps_to_code() {
        // 国旗 emoji 由两个 Regional_Indicator 直接映射为 2 字母代码
        assert_eq!(
            Proxy::new(
                "🇮🇸冰岛 按量不限时".into(),
                ProxyType::Vmess,
                "1.2.3.4".into(),
                443
            )
            .region(),
            "IS"
        );
        assert_eq!(
            Proxy::new("🇦🇪_1".into(), ProxyType::Vmess, "1.2.3.4".into(), 443).region(),
            "AE"
        );
        assert_eq!(
            Proxy::new(
                "🇲🇾 Malaysia".into(),
                ProxyType::Vmess,
                "1.2.3.4".into(),
                443
            )
            .region(),
            "MY"
        );
        // 中国国旗 → CN（上游常把国内节点标 🇨🇳）
        assert_eq!(
            Proxy::new("🇨🇳_1".into(), ProxyType::Vmess, "1.2.3.4".into(), 443).region(),
            "CN"
        );
    }

    #[test]
    fn region_more_countries_from_real_db() {
        // 真实库中大量被漏判的中文/英文国名
        assert_eq!(
            Proxy::new("爱尔兰节点".into(), ProxyType::Vmess, "1.2.3.4".into(), 443).region(),
            "IE"
        );
        assert_eq!(
            Proxy::new(
                "罗马尼亚 RO".into(),
                ProxyType::Vmess,
                "1.2.3.4".into(),
                443
            )
            .region(),
            "RO"
        );
        assert_eq!(
            Proxy::new(
                "土耳其 Istanbul".into(),
                ProxyType::Vmess,
                "1.2.3.4".into(),
                443
            )
            .region(),
            "TR"
        );
        assert_eq!(
            Proxy::new("阿根廷 AR".into(), ProxyType::Vmess, "1.2.3.4".into(), 443).region(),
            "AR"
        );
        // 上游明确的"其他地区"分组应保留为 OTHER（无地区信号）
        assert_eq!(
            Proxy::new("其他地区_1".into(), ProxyType::Vmess, "1.2.3.4".into(), 443).region(),
            "OTHER"
        );
    }

    #[test]
    fn effective_region_prefers_outbound_country() {
        // 名字无地区信号（region == OTHER）但实测出口地区已知时，
        // effective_region 应返回实测出口地区，而非 OTHER。
        let mut p = Proxy::new("其他".into(), ProxyType::Vmess, "1.2.3.4".into(), 443);
        assert_eq!(p.region(), "OTHER");
        p.outbound_country = Some("HK".into());
        assert_eq!(p.effective_region(), "HK");

        // 未做出口检测时回退到名字猜测
        let mut q = Proxy::new("香港节点".into(), ProxyType::Vmess, "1.2.3.4".into(), 443);
        assert_eq!(q.effective_region(), "HK");
        // 即使名字有地区、出口地区不同，也以实测出口为准
        q.outbound_country = Some("JP".into());
        assert_eq!(q.effective_region(), "JP");
    }

    #[test]
    fn ss_fingerprint_is_the_same_for_uri_and_yaml_forms() {
        // 同一物理节点：SIP002 URI 把加密方法写进 `method`，clash YAML 写进
        // `cipher`。两者必须得到同一指纹，否则 merge 不去重（导出同名两条）、
        // 订阅在 YAML/base64 形态间切换时增量刷新会把全部 SS 节点判为新节点。
        let mut from_uri = Proxy::new("SS-1".into(), ProxyType::Ss, "1.2.3.4".into(), 8388);
        from_uri.password = Some("pw".into());
        from_uri.method = Some("aes-256-gcm".into());
        let mut from_yaml = from_uri.clone();
        from_yaml.method = None;
        from_yaml.cipher = Some("aes-256-gcm".into());
        assert_eq!(
            from_uri.fingerprint(),
            from_yaml.fingerprint(),
            "SS 指纹必须读归一后的加密方法"
        );
        // 归一值仍然区分不同加密方法
        let mut other = from_yaml.clone();
        other.cipher = Some("chacha20-ietf-poly1305".into());
        assert_ne!(from_yaml.fingerprint(), other.fingerprint());
    }

    #[test]
    fn fingerprint_escapes_separator_in_name_and_server() {
        // 真实订阅名字常含 `|`（如「🇺🇸美国-电信|5倍」）。若 name / server 不转义，
        // `|` 会伪装成字段分隔符，让两个不同节点拼出同一串指纹而被 merge 静默去重。
        let a = Proxy::new("X|1|443|y".into(), ProxyType::Ss, "h".into(), 1);
        let b = Proxy::new("X".into(), ProxyType::Ss, "h|1|443|y".into(), 1);
        assert_ne!(
            a.fingerprint(),
            b.fingerprint(),
            "名字/地址里的 | 不得冒充字段边界"
        );
    }

    #[test]
    fn recompute_latency_excludes_dead_nodes() {
        // 判死节点即使仍带着上一轮的延迟数据，也不能进入平均/最佳延迟，
        // 否则「最佳延迟」会显示成一个面板上标红不可用的节点。
        let mut dead = Proxy::new("dead".into(), ProxyType::Ss, "1.1.1.1".into(), 1);
        dead.available = Some(false);
        dead.latency_ms = Some(10);
        let mut live = Proxy::new("live".into(), ProxyType::Ss, "2.2.2.2".into(), 2);
        live.available = Some(true);
        live.latency_ms = Some(120);
        let mut h = SubscriptionHealth::default();
        h.recompute(&[dead, live]);
        assert_eq!(h.healthy_node_count, 1);
        assert_eq!(h.best_latency_ms, Some(120), "死节点延迟不得参与统计");
        assert_eq!(h.avg_latency_ms, Some(120));
    }
}
