use crate::model::{Proxy, ProxyType};
use base64::Engine;
use percent_encoding::percent_decode_str;
use std::collections::HashMap;

/// Maximum number of nested base64 layers we'll unwrap. Real subscriptions
/// use at most one layer (base64 of yaml/uri-list); a crafted input of deeply
/// nested base64 must not recurse unboundedly (stack + CPU DoS).
const MAX_B64_DEPTH: usize = 3;

/// Maximum decoded size we'll accept from a base64 layer. The server already
/// caps the *fetched* body at 16 MiB, but parse_subscription is also fed
/// local imports — keep the same bound here so a pasted blob can't balloon.
const MAX_B64_DECODED_BYTES: usize = 16 * 1024 * 1024;

/// Entry point: given the raw text of a subscription, figure out the format
/// and parse it into a list of proxies. Robust to mixed content (a `proxies:`
/// YAML block with bare `vmess://`/`trojan://`/... URI lines appended).
pub fn parse_subscription(raw: &str) -> Vec<Proxy> {
    parse_subscription_depth(raw, 0)
}

fn parse_subscription_depth(raw: &str, depth: usize) -> Vec<Proxy> {
    // UTF-8 BOM 不属于 `trim()` 认的空白：带 BOM 的本地粘贴会让 base64 解码、
    // `starts_with('{')` 与 `proxies:` 检测同时失效，最后解析出 0 个节点且无提示。
    let raw = raw.trim_start_matches('\u{feff}').trim();
    if raw.is_empty() {
        return Vec::new();
    }

    // 1) base64-wrapped. Most clash subscriptions are base64 of a clash YAML,
    //    but a *lot* of sources (v2rayN / NekoBox / many "generic" sub
    //    converters, e.g. laodavip-style short links) instead ship base64 of a
    //    plain URI list (`vmess://`, `vless://`, `ss://` … one per line). Both
    //    shapes must be unwrapped here, otherwise the URI parser below runs on
    //    the still-encoded base64 and finds zero nodes. Depth-capped:
    //    see MAX_B64_DEPTH.
    if depth < MAX_B64_DEPTH {
        if let Ok(decoded) = b64_decode(raw) {
            if decoded.len() <= MAX_B64_DECODED_BYTES {
                let text = String::from_utf8_lossy(&decoded);
                if !text.trim().is_empty() && text.trim() != raw && looks_like_subscription(&text) {
                    return parse_subscription_depth(&text, depth + 1);
                }
            }
        }
    }

    let mut out = Vec::new();

    // 2) structured JSON (sing-box outbounds / v2ray-Xray config / clash json)
    if raw.starts_with('{') || raw.starts_with('[') {
        // sing-box 用 `type` + 扁平字段，v2ray/Xray 用 `protocol` +
        // `settings.vnext|servers`。两者的共同点是都有 `outbounds` —— 以前只认
        // 前者，导入任何 v2ray/Xray 配置（**包括本项目自己的 v2ray 导出**）
        // 都得到 0 个节点且毫无提示，表现为「订阅是空的」。
        if raw.contains("outbounds") || raw.starts_with('[') {
            if let Ok(p) = parse_singbox_json(raw) {
                out.extend(p);
            }
            if let Ok(p) = parse_v2ray_json(raw) {
                out.extend(p);
            }
        }
        if raw.contains("\"proxies\"") {
            out.extend(parse_clash_yaml(raw));
        }
    }

    // 3) YAML `proxies:` block (extract it tolerance so trailing URI lines
    //    at column 0 don't break the parse)
    out.extend(extract_yaml_proxies(raw));

    // 4) line-delimited URIs (also catches URIs in mixed docs). Only parse
    //    *known* proxy schemes — a subscription body often contains other
    //    `http(s)://` lines (e.g. `update-url:`, `https://.../rule`) that would
    //    otherwise be misinterpreted as Http-type "ghost" nodes.
    let mut ssr_skipped = 0usize;
    for line in raw.lines() {
        let line = line.trim();
        if let Some((scheme, rest)) = line.split_once("://") {
            if !is_known_scheme(scheme) {
                continue;
            }
            // SSR 单独计数：mihomo 不支持、必然被丢弃，逐行 eprintln 在
            // 5000 节点的订阅上会刷出 5000 行 stderr。
            if scheme.eq_ignore_ascii_case("ssr") {
                ssr_skipped += 1;
                continue;
            }
            // Bare `http(s)://host:port` lines with NO userinfo are almost
            // never proxy nodes — they're update URLs, rule-set links or
            // comments inside a mixed subscription document. Parsing them
            // would create untestable "ghost" Http nodes. Only treat an
            // http(s) line as a proxy when it carries `user:pass@host`
            // userinfo. All dedicated proxy schemes (vmess/ss/...) are
            // unambiguous and stay accepted as-is.
            let sl = scheme.to_ascii_lowercase();
            if (sl == "http" || sl == "https") && !rest.contains('@') {
                continue;
            }
            if let Some(p) = parse_uri(line) {
                out.push(p);
            }
        }
    }
    if ssr_skipped > 0 {
        eprintln!("[subhub] 跳过 {ssr_skipped} 个 SSR 节点（mihomo/Clash.Meta 不支持 SSR）");
    }

    out
}

/// Proxy URI schemes we know how to parse. Any `://` line whose scheme is not
/// in this set is left alone (avoids turning unrelated URLs into ghost nodes).
fn is_known_scheme(scheme: &str) -> bool {
    matches!(
        scheme.to_ascii_lowercase().as_str(),
        "vmess"
            | "vless"
            | "trojan"
            | "ss"
            | "ssr"
            | "hysteria2"
            | "hy2"
            | "hysteria"
            | "tuic"
            | "anytls"
            | "socks5"
            | "socks"
            | "http"
            | "https"
    )
}

/// Heuristic used to decide whether a base64-decoded blob is actually a
/// subscription we should parse, as opposed to a coincidental valid base64
/// string of unrelated text (which we must NOT recurse into, or we'd throw
/// away the original plain-text nodes).
///
/// True when the text is either a structured clash yaml / sing-box json
/// (carries `proxies:` / `outbounds`), or a plain URI list (one known proxy
/// scheme per line). The latter is the common base64-of-URIs shape that the
/// old `proxies:`-only check silently dropped.
fn looks_like_subscription(text: &str) -> bool {
    if text.contains("proxies:") || text.contains("\"outbounds\"") || text.contains("\"proxies\"") {
        return true;
    }
    text.lines().any(|l| {
        let l = l.trim();
        l.split_once("://")
            .is_some_and(|(scheme, _)| is_known_scheme(scheme))
    })
}

/// Extract only the `proxies:` block from a YAML-ish document and parse it.
/// Stops collecting at the first top-level (column-0) non-empty line, so a
/// `proxy-groups:` / `rules:` section — or bare URI lines — won't break it.
fn extract_yaml_proxies(raw: &str) -> Vec<Proxy> {
    let mut yaml_lines: Vec<&str> = Vec::new();
    let mut collecting = false;
    for line in raw.lines() {
        if !collecting {
            if line.starts_with("proxies:") {
                collecting = true;
                yaml_lines.push(line);
            }
        } else if line.trim().is_empty() {
            yaml_lines.push(line);
        } else if line.starts_with(' ') || line.starts_with('\t') || line.starts_with('-') {
            // accept indented entries AND column-0 list items
            // (`- name:` / `- {name:...}`), which many Clash subs use
            yaml_lines.push(line);
        } else {
            break;
        }
    }
    if yaml_lines.is_empty() {
        return Vec::new();
    }
    parse_clash_yaml(&yaml_lines.join("\n"))
}

pub fn b64_decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    let s = s.trim().replace(['\n', '\r'], "");
    // Try every common base64 flavour. V2Board / Xboard / sspanel-style panels
    // frequently emit URL-safe base64 (`-`/`_` instead of `+`/`/`), and many
    // sources omit trailing `=` padding. The old decoder only tried the
    // standard alphabet, so a URL-safe body failed to decode, the base64
    // unwrap in `parse_subscription_depth` never fired, and the whole
    // subscription parsed to zero nodes. Try all four combinations (standard /
    // url-safe × padded / unpadded) and take the first that succeeds.
    for eng in [
        base64::engine::general_purpose::STANDARD,
        base64::engine::general_purpose::STANDARD_NO_PAD,
        base64::engine::general_purpose::URL_SAFE,
        base64::engine::general_purpose::URL_SAFE_NO_PAD,
    ] {
        if let Ok(b) = eng.decode(&s) {
            return Ok(b);
        }
    }
    // Nothing decoded — surface STANDARD's error for a meaningful message.
    base64::engine::general_purpose::STANDARD.decode(&s)
}

/// Parse a clash YAML/JSON document that contains a `proxies:` list.
pub fn parse_clash_yaml(s: &str) -> Vec<Proxy> {
    let value: serde_yaml::Value = match serde_yaml::from_str(s) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    if let Some(proxies) = value.get("proxies").and_then(|p| p.as_sequence()) {
        for p in proxies {
            if let Some(proxy) = from_clash_value(p) {
                out.push(proxy);
            }
        }
    }
    out
}

/// Traffic-usage info parsed from a subscription document. `download` /
/// `upload` / `total` are bytes; `expire` is normalized to epoch milliseconds.
/// Any field the source doesn't report stays `None`.
#[derive(Debug, Clone, Default)]
pub struct SubscriptionUsage {
    pub upload: Option<u64>,
    pub download: Option<u64>,
    pub total: Option<u64>,
    pub expire: Option<u64>,
}

/// Extract the subscription-level traffic usage.
///
/// Clash-style subscriptions carry this in a top-level `info` block
/// (`download:` / `upload:` / `total:` / `expire:`) that precedes `proxies:`.
/// Most are base64-wrapped, so we decode first (the raw text is only used as
/// a fallback when it isn't valid base64). sing-box / v2ray bodies rarely
/// embed usage, so for those the `Subscription-Userinfo` *response header*
/// (handled by the server) is the authoritative source.
pub fn extract_subscription_usage(raw: &str) -> SubscriptionUsage {
    let raw = raw.trim();
    if raw.is_empty() {
        return SubscriptionUsage::default();
    }
    let text = match b64_decode(raw) {
        Ok(d) => String::from_utf8_lossy(&d).to_string(),
        Err(_) => raw.to_string(),
    };
    let mut u = SubscriptionUsage::default();
    if let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(&text) {
        // 用量字段可能在顶层，也可能嵌套在 `info:` 块里（两种写法都常见）。
        let pick = |key: &str| -> Option<u64> {
            yaml_u64(v.get(key)).or_else(|| yaml_u64(v.get("info").and_then(|i| i.get(key))))
        };
        u.download = pick("download");
        u.upload = pick("upload");
        u.total = pick("total");
        // `expire` 除了数字还常见日期字符串；数字再按数量级归一。
        let exp_val = v
            .get("expire")
            .or_else(|| v.get("info").and_then(|i| i.get("expire")));
        u.expire = match exp_val.and_then(|x| x.as_str()) {
            Some(txt) => match yaml_u64(exp_val) {
                Some(n) => Some(normalize_epoch(n)),
                None => date_str_to_epoch_ms(txt),
            },
            None => yaml_u64(exp_val).map(normalize_epoch),
        };
    }
    u
}

/// Clash `expire` (and the `Subscription-Userinfo` header) report seconds;
/// the rest of the app uses epoch milliseconds. 按数量级归一：
/// 秒(1e9~1e12) → 毫秒，毫秒原样，微秒/纳秒 → 除以相应倍数。
/// 以前只处理「小于 1e12 一律乘 1000」，所以 16 位的**微秒**时间戳会被当成
/// 毫秒原样返回，面板上显示成几万年后的到期时间。
pub fn normalize_epoch(v: u64) -> u64 {
    const MS: u64 = 1_000_000_000_000;
    const US: u64 = 1_000_000_000_000_000;
    const NS: u64 = 1_000_000_000_000_000_000;
    if v == 0 {
        0
    } else if v < MS {
        v.saturating_mul(1000) // 秒
    } else if v < US {
        v // 毫秒
    } else if v < NS {
        v / 1_000 // 微秒
    } else {
        v / 1_000_000 // 纳秒
    }
}

/// 用量字段取值：数字、数字字符串（`"100"`）、浮点（`3.0`）都接受。
/// 面板下发字符串数字很常见，旧写法只认 `as_u64` → 流量卡片直接空掉。
fn yaml_u64(v: Option<&serde_yaml::Value>) -> Option<u64> {
    let x = v?;
    x.as_u64()
        .or_else(|| {
            x.as_str()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .or_else(|| {
                    x.as_str()
                        .and_then(|s| s.trim().parse::<f64>().ok())
                        .map(|f| f.max(0.0) as u64)
                })
        })
        .or_else(|| {
            x.as_f64().and_then(|f| {
                if f.is_finite() && f >= 0.0 {
                    Some(f as u64)
                } else {
                    None
                }
            })
        })
}

/// `expire: "2026-12-31 23:59:59"` / `"2026-12-31"` 这类日期写法（部分面板与
/// clash `info` 块会这样下发）转成 epoch 毫秒（按 UTC 解释）。
/// 解析失败返回 None，调用方回退到数字取值。
fn date_str_to_epoch_ms(s: &str) -> Option<u64> {
    let t = s.trim().replace('T', " ");
    let (date, time) = match t.split_once(' ') {
        Some((d, tm)) => (d, tm),
        None => (t.as_str(), ""),
    };
    let mut dp = date.split('-');
    let year: i64 = dp.next()?.parse().ok()?;
    let month: u32 = dp.next()?.parse().ok()?;
    let day: u32 = dp.next()?.parse().ok()?;
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut tp = time.split(':');
    let hour: u32 = tp.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let min: u32 = tp.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let sec: u32 = tp.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    if hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    // days-from-civil（Howard Hinnant）：把 civil 日期换成 epoch 天数。
    let y = year - if month <= 2 { 1 } else { 0 };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u64;
    let mp = (month + 9) % 12;
    let doy = (153 * mp as u64 + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era as u64 * 146097 + doe - 719468;
    Some(days * 86_400_000 + (hour as u64 * 3600 + min as u64 * 60 + sec as u64) * 1000)
}

fn as_str<'a>(v: &'a serde_yaml::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// 端口取值：数字与数字字符串都接受。v2rayN、部分订阅生成器和手写配置里
/// `port: "443"` 很常见，旧写法只认数字 → 整节点被静默丢弃，表现为
/// 「导入后节点数莫名少了一批」。0 与 >65535 一律无效（不能 `as u16` 截断）。
fn port_from_yaml(v: Option<&serde_yaml::Value>) -> Option<u16> {
    let x = v?;
    let p = x
        .as_u64()
        .or_else(|| x.as_str().and_then(|s| s.trim().parse::<u64>().ok()))?;
    if (1..=65535).contains(&p) {
        Some(p as u16)
    } else {
        None
    }
}

fn port_from_json(v: Option<&serde_json::Value>) -> Option<u16> {
    let x = v?;
    let p = x
        .as_u64()
        .or_else(|| x.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
        .or_else(|| {
            x.as_f64().and_then(|f| {
                if f.fract() == 0.0 {
                    Some(f as u64)
                } else {
                    None
                }
            })
        })?;
    if (1..=65535).contains(&p) {
        Some(p as u16)
    } else {
        None
    }
}

/// `alterId` / `aid` 同样有数字与字符串两种写法；超出 u32 视为无效而不是回绕。
fn u32_from_yaml(v: Option<&serde_yaml::Value>) -> Option<u32> {
    let x = v?;
    let n = x
        .as_u64()
        .or_else(|| x.as_str().and_then(|s| s.trim().parse::<u64>().ok()))?;
    u32::try_from(n).ok()
}

fn u32_from_json(v: Option<&serde_json::Value>) -> Option<u32> {
    let x = v?;
    let n = x
        .as_u64()
        .or_else(|| x.as_str().and_then(|s| s.trim().parse::<u64>().ok()))?;
    u32::try_from(n).ok()
}

fn parse_bool(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// ALPN in URIs is a comma-separated list (`h2,http/1.1`).
fn parse_alpn(v: Option<&String>) -> Option<Vec<String>> {
    let list: Vec<String> = v?
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    (!list.is_empty()).then_some(list)
}

/// YAML ALPN may be a sequence or a single string.
fn yaml_string_list(v: Option<&serde_yaml::Value>) -> Option<Vec<String>> {
    let v = v?;
    if let Some(s) = v.as_str() {
        let list: Vec<String> = s
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        return (!list.is_empty()).then_some(list);
    }
    let list: Vec<String> = v
        .as_sequence()?
        .iter()
        .filter_map(|x| x.as_str())
        .map(str::to_string)
        .collect();
    (!list.is_empty()).then_some(list)
}

/// Parse `"30 Mbps"` / `"30"` / `"30Mbps"` into Mbps.
fn parse_mbps(s: &str) -> Option<u64> {
    let lower = s.trim().to_ascii_lowercase();
    let numeric: String = lower
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let value: f64 = numeric.parse().ok()?;
    let unit = lower[numeric.len()..].trim();
    let factor = if unit.starts_with("gb") {
        1000.0
    } else if unit.starts_with("kb") {
        0.001
    } else if unit.starts_with("b") {
        0.000_001
    } else {
        // `mbps` / `mb` / 无单位一律按 Mbps。
        1.0
    };
    Some((value.max(0.0) * factor).round() as u64)
}

fn from_clash_value(v: &serde_yaml::Value) -> Option<Proxy> {
    let type_str = as_str(v, "type")?;
    let server = as_str(v, "server")?;
    // Reject out-of-range ports (e.g. 70000) instead of silently truncating to
    // a wrong value via `as u16` — a node with port 0 or >65535 can never connect.
    let port = match port_from_yaml(v.get("port")) {
        Some(p) => p,
        _ => return None,
    };
    let name = as_str(v, "name").unwrap_or(server).to_string();
    let type_ = match type_str.to_lowercase().as_str() {
        "ss" => ProxyType::Ss,
        "trojan" => ProxyType::Trojan,
        "vmess" => ProxyType::Vmess,
        "vless" => ProxyType::Vless,
        "hysteria2" | "hysteria-2" => ProxyType::Hysteria2,
        "tuic" => ProxyType::Tuic,
        "socks5" | "socks" => ProxyType::Socks5,
        "http" => ProxyType::Http,
        "wireguard" => ProxyType::Wireguard,
        _ => ProxyType::Other,
    };

    let mut p = Proxy::new(name, type_, server.to_string(), port);
    p.uuid = as_str(v, "uuid").map(str::to_string);
    p.alter_id = u32_from_yaml(v.get("alterId"));
    p.cipher = as_str(v, "cipher").map(str::to_string);
    p.flow = as_str(v, "flow").map(str::to_string);
    p.password = as_str(v, "password").map(str::to_string);
    p.method = as_str(v, "method").map(str::to_string);
    p.plugin = as_str(v, "plugin").map(str::to_string);
    p.plugin_opts = v
        .get("plugin-opts")
        .and_then(|po| po.as_mapping())
        .map(|map| {
            let mut parts: Vec<String> = Vec::new();
            for (k, val) in map {
                let Some(key) = k.as_str() else { continue };
                let value = match val {
                    serde_yaml::Value::Bool(b) => b.to_string(),
                    serde_yaml::Value::String(s) => s.clone(),
                    serde_yaml::Value::Number(n) => n.to_string(),
                    _ => continue,
                };
                parts.push(format!("{key}={value}"));
            }
            parts.join(";")
        })
        .filter(|s| !s.is_empty());
    p.obfs = as_str(v, "obfs").map(str::to_string);
    p.obfs_password = as_str(v, "obfs-password").map(str::to_string);
    p.alpn = yaml_string_list(v.get("alpn"));
    p.congestion_control = as_str(v, "congestion-controller")
        .or_else(|| as_str(v, "congestion_control"))
        .map(str::to_string);
    p.udp_relay_mode = as_str(v, "udp-relay-mode")
        .or_else(|| as_str(v, "udp_relay_mode"))
        .map(str::to_string);
    p.udp_over_stream = v
        .get("udp-over-stream")
        .or_else(|| v.get("udp_over_stream"))
        .and_then(|x| x.as_bool());
    p.zero_rtt_handshake = v
        .get("reduce-rtt")
        .or_else(|| v.get("zero_rtt_handshake"))
        .and_then(|x| x.as_bool());
    let mbps = |key: &str| {
        v.get(key)
            .and_then(|x| x.as_str().and_then(parse_mbps).or_else(|| x.as_u64()))
    };
    p.up_mbps = mbps("up");
    p.down_mbps = mbps("down");
    p.network = as_str(v, "network").map(str::to_string);
    p.tls = v.get("tls").and_then(|x| x.as_bool());
    p.sni = as_str(v, "sni")
        .or_else(|| as_str(v, "servername"))
        .map(str::to_string);
    p.skip_cert_verify = v.get("skip-cert-verify").and_then(|x| x.as_bool());
    // h2/http 传输的 host 在 clash 里是列表（也可能被写成标量）。
    let h2_host = v.get("h2-opts").and_then(|h| h.get("host")).and_then(|x| {
        x.as_str().map(str::to_string).or_else(|| {
            x.as_sequence()
                .and_then(|seq| seq.first())
                .and_then(|x| x.as_str())
                .map(str::to_string)
        })
    });
    p.path = as_str(v, "path")
        .or_else(|| {
            v.get("ws-opts")
                .and_then(|w| w.get("path"))
                .and_then(|x| x.as_str())
        })
        .or_else(|| {
            v.get("h2-opts")
                .and_then(|h| h.get("path"))
                .and_then(|x| x.as_str())
        })
        .map(str::to_string);
    p.host = as_str(v, "host")
        .map(str::to_string)
        .or_else(|| {
            v.get("ws-opts")
                .and_then(|w| w.get("headers"))
                .and_then(|h| h.get("Host"))
                .and_then(|x| x.as_str())
                .map(str::to_string)
        })
        .or(h2_host);
    p.service_name = v
        .get("grpc-opts")
        .and_then(|g| g.get("grpc-service-name"))
        .and_then(|x| x.as_str())
        .map(str::to_string);
    p.fingerprint = as_str(v, "client-fingerprint").map(str::to_string);
    if let Some(ro) = v.get("reality-opts") {
        p.reality = true;
        p.reality_public_key = ro
            .get("public-key")
            .and_then(|x| x.as_str())
            .map(str::to_string);
        p.reality_short_id = ro
            .get("short-id")
            .and_then(|x| x.as_str())
            .map(str::to_string);
        // REALITY is always carried over TLS; force it even when the source
        // explicitly says `tls: false` (that combination is invalid).
        p.tls = Some(true);
    }
    Some(p)
}

fn parse_singbox_json(s: &str) -> Result<Vec<Proxy>, serde_json::Error> {
    let v: serde_json::Value = serde_json::from_str(s)?;
    let mut out = Vec::new();
    let mut unknown_types: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if let Some(outbounds) = v.get("outbounds").and_then(|o| o.as_array()) {
        for ob in outbounds {
            let type_str = ob.get("type").and_then(|x| x.as_str()).unwrap_or("");
            let server = match ob.get("server").and_then(|x| x.as_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let port = match port_from_json(ob.get("server_port")) {
                Some(p) => p,
                _ => continue, // skip outbounds without a valid port
            };
            let name = ob
                .get("tag")
                .and_then(|x| x.as_str())
                .unwrap_or(&server)
                .to_string();
            let type_ = match type_str {
                "shadowsocks" => ProxyType::Ss,
                "trojan" => ProxyType::Trojan,
                "vmess" => ProxyType::Vmess,
                "vless" => ProxyType::Vless,
                "hysteria2" => ProxyType::Hysteria2,
                "tuic" => ProxyType::Tuic,
                "socks" => ProxyType::Socks5,
                "wireguard" => ProxyType::Wireguard,
                // 未知类型落到 `Other` 而不是丢弃：与 clash 分支口径一致，
                // 由导出侧统一过滤，用户至少能在节点列表里看到它。
                other => {
                    unknown_types.insert(other.to_string());
                    ProxyType::Other
                }
            };
            let mut p = Proxy::new(name, type_, server, port);
            p.password = ob
                .get("password")
                .and_then(|x| x.as_str())
                .map(str::to_string);
            p.method = ob
                .get("method")
                .and_then(|x| x.as_str())
                .map(str::to_string);
            p.uuid = ob.get("uuid").and_then(|x| x.as_str()).map(str::to_string);
            // sing-box 的 socks/http 把用户名放在 `username`（不是 uuid），
            // 此前不读 → 导出成无认证节点。与 clash 侧同一存放位置（extra.user）。
            p.extra = ob
                .get("username")
                .and_then(|x| x.as_str())
                .map(|u| serde_json::json!({ "user": u }));
            // vless 的 `flow`（xtls-rprx-vision 等）必须保留：丢了 flow 的
            // vision 节点在 xray 侧握手会失败。
            p.flow = ob.get("flow").and_then(|x| x.as_str()).map(str::to_string);
            if type_ == ProxyType::Ss {
                p.plugin = ob
                    .get("plugin")
                    .and_then(|x| x.as_str())
                    .map(str::to_string);
                p.plugin_opts = ob
                    .get("plugin_opts")
                    .and_then(|x| x.as_str())
                    .map(str::to_string);
            }
            if type_ == ProxyType::Hysteria2 {
                if let Some(obfs) = ob.get("obfs") {
                    p.obfs = obfs
                        .get("type")
                        .and_then(|x| x.as_str())
                        .map(str::to_string);
                    p.obfs_password = obfs
                        .get("password")
                        .and_then(|x| x.as_str())
                        .map(str::to_string);
                }
                p.up_mbps = ob.get("up_mbps").and_then(|x| x.as_u64());
                p.down_mbps = ob.get("down_mbps").and_then(|x| x.as_u64());
            }
            if type_ == ProxyType::Tuic {
                p.congestion_control = ob
                    .get("congestion_control")
                    .and_then(|x| x.as_str())
                    .map(str::to_string);
                p.udp_relay_mode = ob
                    .get("udp_relay_mode")
                    .and_then(|x| x.as_str())
                    .map(str::to_string);
                p.udp_over_stream = ob.get("udp_over_stream").and_then(|x| x.as_bool());
                p.zero_rtt_handshake = ob.get("zero_rtt_handshake").and_then(|x| x.as_bool());
            }
            // sing-box uses `tls: {}` to enable and `tls: false` to disable; a
            // bare `tls: true`/object means enabled. Don't treat a present key
            // as unconditionally enabled (that forced TLS onto plaintext nodes).
            p.tls = Some(match ob.get("tls") {
                Some(serde_json::Value::Bool(b)) => *b,
                // sing-box 1.11+ 用对象形态 `{ "enabled": false }`：只看「键存在」
                // 就判 TLS，会把明文节点强推成 TLS，握手必败。
                Some(serde_json::Value::Object(o)) => {
                    o.get("enabled").and_then(|x| x.as_bool()).unwrap_or(true)
                }
                Some(_) => true,
                None => false,
            });
            if let Some(tls) = ob.get("tls") {
                p.sni = tls
                    .get("server_name")
                    .and_then(|x| x.as_str())
                    .map(str::to_string);
                p.skip_cert_verify = tls.get("insecure").and_then(|x| x.as_bool());
                p.alpn = tls
                    .get("alpn")
                    .and_then(|x| x.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str())
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                    .filter(|v| !v.is_empty());
                // REALITY：sing-box 形如 `tls.reality.enabled` + `tls.reality.short_id`，
                // 公钥在 `tls.public_key`（顶层）或 `tls.reality.public_key`。丢掉这些
                // 字段会把 REALITY 节点降级成普通 TLS 节点——连不上且无从排查。
                let reality = tls.get("reality");
                let reality_on = reality
                    .and_then(|r| r.get("enabled"))
                    .and_then(|x| x.as_bool())
                    .unwrap_or_else(|| reality.is_some());
                if reality_on {
                    p.reality = true;
                    p.reality_public_key = tls
                        .get("public_key")
                        .and_then(|x| x.as_str())
                        .or_else(|| {
                            reality
                                .and_then(|r| r.get("public_key"))
                                .and_then(|x| x.as_str())
                        })
                        .map(str::to_string);
                    p.reality_short_id = reality
                        .and_then(|r| r.get("short_id"))
                        .and_then(|x| x.as_str())
                        .map(str::to_string);
                    // REALITY 一定跑在 TLS 之上
                    p.tls = Some(true);
                }
                p.fingerprint = tls
                    .get("utls")
                    .and_then(|u| u.get("fingerprint"))
                    .and_then(|x| x.as_str())
                    .or_else(|| tls.get("client_fingerprint").and_then(|x| x.as_str()))
                    .map(str::to_string);
            }
            // Transport (ws / grpc / h2) — sing-box nests this under
            // `transport: { type, path, headers, host, service_name }`.
            if let Some(transport) = ob.get("transport") {
                let ttype = transport.get("type").and_then(|x| x.as_str()).unwrap_or("");
                match ttype {
                    "ws" => {
                        p.network = Some("ws".to_string());
                        p.path = transport
                            .get("path")
                            .and_then(|x| x.as_str())
                            .map(str::to_string);
                        p.host = transport
                            .get("headers")
                            .and_then(|h| h.get("Host"))
                            .and_then(|x| x.as_str())
                            .map(str::to_string);
                    }
                    "grpc" => {
                        p.network = Some("grpc".to_string());
                        p.service_name = transport
                            .get("service_name")
                            .and_then(|x| x.as_str())
                            .map(str::to_string);
                    }
                    "h2" | "http" => {
                        p.network = Some("h2".to_string());
                        p.path = transport
                            .get("path")
                            .and_then(|x| x.as_str())
                            .map(str::to_string);
                        p.host = transport.get("host").and_then(|x| {
                            x.as_str().map(str::to_string).or_else(|| {
                                x.as_array()
                                    .and_then(|a| a.first())
                                    .and_then(|x| x.as_str())
                                    .map(str::to_string)
                            })
                        });
                    }
                    _ => {}
                }
            }
            out.push(p);
        }
    }
    if !unknown_types.is_empty() {
        // 一行汇总而不是每节点一行（5000 节点会刷出 5000 行 stderr）。
        let list: Vec<&str> = unknown_types.iter().map(String::as_str).collect();
        eprintln!(
            "[parse] sing-box 未知类型按 other 保留、导出时过滤: {}",
            list.join(", ")
        );
    }
    Ok(out)
}

// ----------------------- v2ray / Xray JSON -----------------------

/// 解析 v2ray / Xray 风格的配置 JSON：`outbounds[].protocol` +
/// `settings.vnext[].users[]`（vmess/vless）或 `settings.servers[]`
/// （trojan/shadowsocks），传输与 TLS 参数在 `streamSettings` 里。
///
/// 形状与 `export::to_v2ray_json` 一一对应，因此本项目导出的 v2rayN 配置
/// 可以被重新导入（裸数组形态同样支持——v2rayN 的 `outbounds.json` 就是数组）。
fn parse_v2ray_json(s: &str) -> Result<Vec<Proxy>, serde_json::Error> {
    let v: serde_json::Value = serde_json::from_str(s)?;
    let arr_owned;
    let arr = if let Some(a) = v.get("outbounds").and_then(|o| o.as_array()) {
        a
    } else if let Some(a) = v.as_array() {
        arr_owned = a.clone();
        &arr_owned
    } else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for ob in arr {
        if let Some(p) = v2ray_outbound(ob) {
            out.push(p);
        }
    }
    Ok(out)
}

/// 单个 v2ray/Xray outbound → Proxy。无法表示的协议返回 `None`
/// （hysteria / wireguard / tun / freedom 等不属于节点）。
fn v2ray_outbound(ob: &serde_json::Value) -> Option<Proxy> {
    let proto = ob.get("protocol").and_then(|x| x.as_str())?;
    let stream = ob.get("streamSettings");
    let tag = ob
        .get("tag")
        .and_then(|x| x.as_str())
        .unwrap_or(proto)
        .to_string();

    // vmess / vless 走 `settings.vnext[]`，trojan / shadowsocks 走 `settings.servers[]`。
    let target = ob
        .get("settings")
        .and_then(|st| st.get("vnext"))
        .and_then(|x| x.as_array())
        .and_then(|a| a.first().cloned())
        .or_else(|| {
            ob.get("settings")
                .and_then(|st| st.get("servers"))
                .and_then(|x| x.as_array())
                .and_then(|a| a.first().cloned())
        })?;
    // vless/vmess 的真实凭据在 users[0]，trojan/ss 直接在 servers[0]。
    let user = target
        .get("users")
        .and_then(|u| u.as_array())
        .and_then(|a| a.first().cloned())
        .unwrap_or_else(|| target.clone());

    let server = target
        .get("address")
        .and_then(|x| x.as_str())
        .or_else(|| user.get("address").and_then(|x| x.as_str()))?
        .to_string();
    let port = port_from_json(target.get("port").or(user.get("port")))?;

    let type_ = match proto {
        "vmess" => ProxyType::Vmess,
        "vless" => ProxyType::Vless,
        "trojan" => ProxyType::Trojan,
        "shadowsocks" => ProxyType::Ss,
        _ => return None,
    };
    let mut p = Proxy::new(tag, type_, server, port);
    p.uuid = user
        .get("id")
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .or_else(|| {
            user.get("password")
                .and_then(|x| x.as_str())
                .map(str::to_string)
        });
    p.password = user
        .get("password")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    p.cipher = user
        .get("security")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    p.method = user
        .get("method")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    p.alter_id = u32_from_json(user.get("alterId")).or_else(|| u32_from_json(user.get("aid")));
    p.flow = user
        .get("flow")
        .and_then(|x| x.as_str())
        .filter(|f| !f.is_empty())
        .map(str::to_string)
        .or_else(|| {
            target
                .get("flow")
                .and_then(|x| x.as_str())
                .map(str::to_string)
        });

    // streamSettings：传输 + 安全层
    if let Some(st) = stream {
        p.network = st
            .get("network")
            .and_then(|x| x.as_str())
            .map(str::to_string);
        let sec = st.get("security").and_then(|x| x.as_str()).unwrap_or("");
        p.tls = Some(matches!(sec, "tls" | "reality"));
        // 传输参数在 xray / v2ray 里是**每种传输一个对象**：
        // `wsSettings{path, headers.Host}`、`grpcSettings{serviceName}`、
        // `httpSettings{path, host[]}` / `xhttpSettings{}`。只读 `networkSettings`
        // 会拿不到 path/Host，导出的 ws 节点没有伪装头 → CDN 中转节点连不上。
        let ws = st.get("wsSettings");
        let grpc = st.get("grpcSettings");
        let http = st.get("httpSettings").or_else(|| st.get("xhttpSettings"));
        let ns = st.get("networkSettings"); // 少数生成器的统一命名，容错读取
        let text = |x: &serde_json::Value| -> Option<String> {
            x.as_str().map(str::to_string).or_else(|| {
                x.as_array()
                    .and_then(|a| a.first())
                    .and_then(|f| f.as_str())
                    .map(str::to_string)
            })
        };
        p.path = ws
            .and_then(|w| w.get("path"))
            .and_then(text)
            .or_else(|| http.and_then(|h| h.get("path")).and_then(text))
            .or_else(|| ns.and_then(|n| n.get("path")).and_then(text));
        p.host = ws
            .and_then(|w| w.get("headers"))
            .and_then(|h| h.get("Host"))
            .and_then(text)
            .or_else(|| http.and_then(|h| h.get("host")).and_then(text))
            .or_else(|| {
                ns.and_then(|n| n.get("headers"))
                    .and_then(|h| h.get("Host"))
                    .and_then(text)
            });
        p.service_name = grpc
            .and_then(|g| g.get("serviceName"))
            .and_then(text)
            .or_else(|| ns.and_then(|n| n.get("serviceName")).and_then(text));

        let tls = st.get("tlsSettings");
        let reality = st.get("realitySettings");
        p.sni = tls
            .and_then(|t| t.get("serverName"))
            .and_then(|x| x.as_str())
            .or_else(|| {
                reality
                    .and_then(|r| r.get("serverName"))
                    .and_then(|x| x.as_str())
            })
            .map(str::to_string);
        p.skip_cert_verify = tls
            .and_then(|t| t.get("allowInsecure"))
            .and_then(|x| x.as_bool())
            .or_else(|| {
                reality
                    .and_then(|r| r.get("allowInsecure"))
                    .and_then(|x| x.as_bool())
            });
        p.alpn = match tls.and_then(|t| t.get("alpn")) {
            Some(serde_json::Value::Array(a)) => {
                let list: Vec<String> = a
                    .iter()
                    .filter_map(|x| x.as_str())
                    .map(str::to_string)
                    .collect();
                (!list.is_empty()).then_some(list)
            }
            Some(serde_json::Value::String(str_val)) => parse_alpn(Some(&str_val.to_string())),
            _ => None,
        };
        // uTLS 客户端指纹：xray 放在 `tlsSettings.fingerprint`，
        // REALITY 放在 `realitySettings.fingerprint`。
        p.fingerprint = tls
            .and_then(|t| t.get("fingerprint"))
            .and_then(|x| x.as_str())
            .or_else(|| {
                reality
                    .and_then(|r| r.get("fingerprint"))
                    .and_then(|x| x.as_str())
            })
            .map(str::to_string);
        if sec == "reality" || reality.is_some() {
            p.reality = true;
            p.reality_public_key = reality
                .and_then(|r| r.get("publicKey").or(r.get("id")))
                .and_then(|x| x.as_str())
                .map(str::to_string);
            p.reality_short_id = reality
                .and_then(|r| r.get("shortId"))
                .and_then(|x| x.as_str())
                .map(str::to_string);
            p.tls = Some(true);
        }
    }
    Some(p)
}

// ----------------------- URI schemes -----------------------

pub fn parse_uri(line: &str) -> Option<Proxy> {
    let line = line.trim();
    let (scheme, rest) = line.split_once("://")?;
    let scheme = scheme.to_lowercase();
    let (body, name) = match rest.split_once('#') {
        Some((b, n)) => (b.to_string(), Some(decode_percent(n))),
        None => (rest.to_string(), None),
    };
    match scheme.as_str() {
        "vmess" => parse_vmess(&body, name),
        "vless" => parse_vless(&body, name),
        "trojan" => parse_trojan(&body, name),
        "ss" => parse_ss(&body, name),
        "ssr" => parse_ssr(&body, name),
        "anytls" => parse_anytls(&body, name),
        "hysteria2" | "hy2" | "hysteria" => parse_hy2(&body, name),
        "tuic" => parse_tuic(&body, name),
        "socks5" | "socks" => parse_socks(&body, name, ProxyType::Socks5, false),
        // `https://` 必须带 TLS；`http://` 才是明文。
        "http" => parse_socks(&body, name, ProxyType::Http, false),
        "https" => parse_socks(&body, name, ProxyType::Http, true),
        _ => None,
    }
}

fn decode_percent(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().to_string()
}

struct Auth {
    userinfo: String,
    host: String,
    port: u16,
    query: HashMap<String, String>,
}

fn split_authority(body: &str) -> Option<Auth> {
    let (left, query) = match body.split_once('?') {
        Some((l, q)) => (l, q),
        None => (body, ""),
    };
    // userinfo 也可能是百分号编码的（密码含 `%40` / `%3A` 很常见）。不解码就会
    // 把字面 `%40` 当密码发出去 → 认证失败、整节点全灭。
    let (userinfo, hostport) = match left.rsplit_once('@') {
        Some((u, h)) => (decode_percent(u), h.to_string()),
        None => (String::new(), left.to_string()),
    };
    let (host, port) = if hostport.starts_with('[') {
        // ipv6 [::1]:port
        let end = hostport.find(']')?;
        let host = hostport[1..end].to_string();
        // The authority may carry a trailing `/<path>` before the query is
        // stripped (e.g. `anytls://h:56147/?type=...`); tolerate it by parsing
        // only the leading digits of the port field.
        let port: u16 = hostport[end + 1..]
            .trim_start_matches(':')
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|s| s.parse().ok())?;
        if port == 0 {
            return None;
        }
        (host, port)
    } else {
        let mut it = hostport.rsplitn(2, ':');
        let port: u16 = it
            .next()?
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|s| s.parse().ok())?;
        let host = it.next()?.to_string();
        if port == 0 {
            return None;
        }
        (host, port)
    };
    // 键统一小写：`?Type=ws`、`?SNI=x`、`?Security=reality` 这类大写写法在真实
    // 订阅里存在，而读取侧全是小写字面量 —— 精确匹配会静默丢掉传输层/SNI，
    // 导出出一个看着正常但连不上的节点。
    let query = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_ascii_lowercase(), decode_percent(v)))
        .collect();
    Some(Auth {
        userinfo,
        host,
        port,
        query,
    })
}

fn parse_vmess(body: &str, name: Option<String>) -> Option<Proxy> {
    let decoded = b64_decode(body).ok()?;
    let json = String::from_utf8_lossy(&decoded);
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    let server = v.get("add")?.as_str()?.to_string();
    let port: u16 = v.get("port").and_then(|x| {
        let n = x
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| x.as_u64());
        n.filter(|n| (1..=65535).contains(n)).map(|n| n as u16)
    })?;
    let pname = name.unwrap_or_else(|| {
        v.get("ps")
            .and_then(|x| x.as_str())
            .unwrap_or(&server)
            .to_string()
    });
    let mut p = Proxy::new(pname, ProxyType::Vmess, server, port);
    p.uuid = v.get("id").and_then(|x| x.as_str()).map(str::to_string);
    p.alter_id = v
        .get("aid")
        .and_then(|x| {
            x.as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .or_else(|| x.as_u64())
        })
        .and_then(|n| u32::try_from(n).ok());
    p.cipher = v
        .get("scy")
        .or_else(|| v.get("cipher"))
        .and_then(|x| x.as_str())
        .map(str::to_string);
    if p.cipher.is_none() {
        p.cipher = Some("aes-128-gcm".to_string());
    }
    p.network = v.get("net").and_then(|x| x.as_str()).map(str::to_string);
    // vmess `tls` may be a string ("tls"/"true") or a JSON boolean; handle both.
    p.tls = Some(
        v.get("tls")
            .map(|t| {
                t.as_str()
                    .map(|s| s == "tls" || s == "true")
                    .unwrap_or(false)
                    || t.as_bool().unwrap_or(false)
            })
            .unwrap_or(false),
    );
    // vmess links often carry the TLS SNI as `peer` (or reuse `host` for the
    // WS Host header); falling back keeps TLS/WS nodes verifiable after export.
    p.sni = v
        .get("sni")
        .or_else(|| v.get("peer"))
        .or_else(|| v.get("host"))
        .and_then(|x| x.as_str())
        .map(str::to_string);
    p.path = v.get("path").and_then(|x| x.as_str()).map(str::to_string);
    p.host = v.get("host").and_then(|x| x.as_str()).map(str::to_string);
    p.alpn = match v.get("alpn") {
        Some(serde_json::Value::String(s)) => parse_alpn(Some(s)),
        Some(serde_json::Value::Array(a)) => {
            let list: Vec<String> = a
                .iter()
                .filter_map(|x| x.as_str())
                .map(str::to_string)
                .collect();
            (!list.is_empty()).then_some(list)
        }
        _ => None,
    };
    p.skip_cert_verify = v
        .get("allowInsecure")
        .or_else(|| v.get("insecure"))
        .and_then(|x| match x {
            serde_json::Value::Bool(b) => Some(*b),
            serde_json::Value::String(s) => Some(s == "1" || s == "true"),
            _ => None,
        });
    Some(p)
}

fn parse_vless(body: &str, name: Option<String>) -> Option<Proxy> {
    let a = split_authority(body)?;
    let mut p = Proxy::new(
        name.unwrap_or_else(|| a.host.clone()),
        ProxyType::Vless,
        a.host,
        a.port,
    );
    p.uuid = Some(a.userinfo);
    p.flow = a.query.get("flow").cloned();
    p.network = a
        .query
        .get("type")
        .or_else(|| a.query.get("network"))
        .cloned();
    let security = a.query.get("security").map(String::as_str).unwrap_or("");
    // 大小写不敏感：`?Security=None` 按精确比较会被当成「声明了某个安全层」，
    // 于是明文节点被强制加 TLS。
    p.tls = Some(!security.eq_ignore_ascii_case("none"));
    if security.eq_ignore_ascii_case("reality") {
        p.reality = true;
        p.reality_public_key = a.query.get("pbk").cloned().filter(|s| !s.is_empty());
        p.reality_short_id = a.query.get("sid").cloned();
    }
    p.sni = a.query.get("sni").cloned();
    p.fingerprint = a.query.get("fp").cloned();
    p.alpn = parse_alpn(a.query.get("alpn"));
    p.skip_cert_verify = a
        .query
        .get("allowinsecure")
        .or_else(|| a.query.get("insecure"))
        .map(|v| v == "1" || v == "true");
    p.path = a.query.get("path").cloned();
    p.host = a.query.get("host").cloned();
    p.service_name = a.query.get("servicename").cloned();
    Some(p)
}

fn parse_trojan(body: &str, name: Option<String>) -> Option<Proxy> {
    let a = split_authority(body)?;
    let mut p = Proxy::new(
        name.unwrap_or_else(|| a.host.clone()),
        ProxyType::Trojan,
        a.host,
        a.port,
    );
    p.password = Some(a.userinfo);
    p.tls = Some(true);
    p.sni = a.query.get("sni").or_else(|| a.query.get("peer")).cloned();
    p.flow = a.query.get("flow").cloned();
    p.network = a.query.get("type").cloned();
    p.path = a.query.get("path").cloned();
    p.host = a.query.get("host").cloned();
    p.fingerprint = a.query.get("fp").cloned();
    p.alpn = parse_alpn(a.query.get("alpn"));
    p.skip_cert_verify = a
        .query
        .get("allowinsecure")
        .or_else(|| a.query.get("skip-cert-verify"))
        .map(|v| v == "1" || v == "true");
    Some(p)
}

fn parse_ss(body: &str, name: Option<String>) -> Option<Proxy> {
    // SIP002: ss://base64(method:password)@host:port  OR  ss://user:pass@host:port
    // legacy: ss://base64(method:password@host:port)
    if let Some(a) = split_authority(body) {
        if !a.userinfo.is_empty() {
            let (method, password) = decode_ss_userinfo(&a.userinfo);
            // A valid SIP002 userinfo always decodes to `method:password`.
            // If the method comes back empty we can't trust this as SIP002
            // (e.g. a body that merely happens to contain '@'), so fall
            // through to the legacy whole-base64 decoder instead of emitting a
            // node with a missing cipher — Clash would reject it.
            if !method.is_empty() {
                let mut p = Proxy::new(
                    name.unwrap_or_else(|| a.host.clone()),
                    ProxyType::Ss,
                    a.host,
                    a.port,
                );
                p.method = Some(method);
                p.password = Some(password);
                apply_ss_plugin(&mut p, a.query.get("plugin").map(String::as_str));
                return Some(p);
            }
        }
    }
    // legacy whole-base64 form
    if let Ok(decoded) = b64_decode(body) {
        let s = String::from_utf8_lossy(&decoded);
        if let Some(a) = split_authority(&s) {
            let (method, password) = decode_ss_userinfo(&a.userinfo);
            let mut p = Proxy::new(
                name.unwrap_or_else(|| a.host.clone()),
                ProxyType::Ss,
                a.host,
                a.port,
            );
            p.method = Some(method);
            p.password = Some(password);
            apply_ss_plugin(&mut p, a.query.get("plugin").map(String::as_str));
            return Some(p);
        }
    }
    None
}

/// SIP002 `plugin=obfs-local;obfs=http;obfs-host=x`：第一个分号前是插件名，
/// 其余是 `k=v` 选项。SS 节点的插件会改变握手行为，必须参与指纹与导出。
fn apply_ss_plugin(p: &mut Proxy, raw: Option<&str>) {
    let Some(raw) = raw.filter(|s| !s.is_empty()) else {
        return;
    };
    let (name, opts) = raw.split_once(';').unwrap_or((raw, ""));
    p.plugin = Some(name.to_string());
    if !opts.is_empty() {
        p.plugin_opts = Some(opts.to_string());
    }
}

fn decode_ss_userinfo(u: &str) -> (String, String) {
    if let Ok(decoded) = b64_decode(u) {
        let s = String::from_utf8_lossy(&decoded);
        if let Some((m, p)) = s.split_once(':') {
            return (m.to_string(), p.to_string());
        }
    }
    if let Some((m, p)) = u.split_once(':') {
        return (m.to_string(), p.to_string());
    }
    (String::new(), u.to_string())
}

fn parse_ssr(_body: &str, _name: Option<String>) -> Option<Proxy> {
    // SSR is NOT supported by mihomo/Clash.Meta. Parsing it as `ProxyType::Ss`
    // would emit a `type: ss` node that fails to connect with no obvious cause,
    // so we drop every SSR node up front and log why it went missing.
    // (We don't even bother decoding — there is nothing usable to keep.)
    // 汇总日志在 `parse_subscription_depth` 的 URI 循环里输出，这里不再逐行打印。
    None
}

fn parse_hy2(body: &str, name: Option<String>) -> Option<Proxy> {
    let a = split_authority(body)?;
    let mut p = Proxy::new(
        name.unwrap_or_else(|| a.host.clone()),
        ProxyType::Hysteria2,
        a.host,
        a.port,
    );
    // userinfo may be "user:pass" or just "pass"
    if let Some((_, pass)) = a.userinfo.rsplit_once(':') {
        p.password = Some(pass.to_string());
    } else {
        p.password = Some(a.userinfo);
    }
    // Hysteria2 is always QUIC-over-TLS; the URI has no `security` param.
    p.tls = Some(true);
    p.sni = a.query.get("sni").cloned();
    p.fingerprint = a.query.get("fp").cloned();
    p.obfs = a.query.get("obfs").cloned();
    p.obfs_password = a
        .query
        .get("obfs-password")
        .or_else(|| a.query.get("obfs_password"))
        .cloned();
    p.alpn = parse_alpn(a.query.get("alpn"));
    p.up_mbps = a.query.get("up").and_then(|v| parse_mbps(v));
    p.down_mbps = a.query.get("down").and_then(|v| parse_mbps(v));
    p.skip_cert_verify = a
        .query
        .get("insecure")
        .or_else(|| a.query.get("allowinsecure"))
        .map(|v| v == "1" || v == "true");
    Some(p)
}

fn parse_tuic(body: &str, name: Option<String>) -> Option<Proxy> {
    let a = split_authority(body)?;
    let mut p = Proxy::new(
        name.unwrap_or_else(|| a.host.clone()),
        ProxyType::Tuic,
        a.host,
        a.port,
    );
    if let Some((u, pass)) = a.userinfo.rsplit_once(':') {
        p.uuid = Some(u.to_string());
        p.password = Some(pass.to_string());
    } else {
        p.password = Some(a.userinfo);
    }
    // TUIC is QUIC-over-TLS as well.
    p.tls = Some(true);
    p.sni = a.query.get("sni").cloned();
    p.fingerprint = a.query.get("fp").cloned();
    p.alpn = parse_alpn(a.query.get("alpn"));
    p.congestion_control = a
        .query
        .get("congestion_control")
        .or_else(|| a.query.get("congestion-controller"))
        .cloned();
    p.udp_relay_mode = a
        .query
        .get("udp_relay_mode")
        .or_else(|| a.query.get("udp-relay-mode"))
        .cloned();
    let bool_query = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| a.query.get(*k).map(|v| parse_bool(v)))
    };
    p.udp_over_stream = bool_query(&["udp_over_stream", "udp-over-stream"]);
    p.zero_rtt_handshake = bool_query(&[
        "zero_rtt_handshake",
        "zero-rtt-handshake",
        "reduce_rtt",
        "reduce-rtt",
    ]);
    p.skip_cert_verify = a
        .query
        .get("insecure")
        .or_else(|| a.query.get("allowinsecure"))
        .map(|v| v == "1" || v == "true");
    Some(p)
}

/// Parse an `anytls://` URI (mihomo / Clash.Meta native AnyTLS).
///
/// AnyTLS authenticates with a **password** (not a uuid) — the userinfo before
/// `@` is the password (mihomo's `anytls` outbound has no `uuid` field). The
/// query carries the same knobs as a vless link: `type`/`network` (transport),
/// `insecure` (skip-cert-verify), `fp` (client-fingerprint), `sni`. AnyTLS is
/// always TLS, so `tls` is forced true; mihomo has no top-level `tls:` switch for
/// it, so the clash exporter deliberately omits one.
fn parse_anytls(body: &str, name: Option<String>) -> Option<Proxy> {
    let a = split_authority(body)?;
    let mut p = Proxy::new(
        name.unwrap_or_else(|| a.host.clone()),
        ProxyType::AnyTls,
        a.host,
        a.port,
    );
    p.password = Some(a.userinfo);
    p.network = a
        .query
        .get("type")
        .or_else(|| a.query.get("network"))
        .cloned();
    p.tls = Some(true);
    p.sni = a.query.get("sni").cloned();
    p.fingerprint = a.query.get("fp").cloned();
    p.skip_cert_verify = a.query.get("insecure").map(|v| v == "1" || v == "true");
    Some(p)
}

fn parse_socks(body: &str, name: Option<String>, t: ProxyType, tls: bool) -> Option<Proxy> {
    let a = split_authority(body)?;
    let mut p = Proxy::new(name.unwrap_or_else(|| a.host.clone()), t, a.host, a.port);
    if !a.userinfo.is_empty() {
        match a.userinfo.rsplit_once(':') {
            Some((u, pass)) => {
                p.extra = Some(serde_json::json!({ "user": u }));
                p.password = Some(pass.to_string());
            }
            // `socks5://pass@host:1080`（只有一段 userinfo）：旧写法什么都不做，
            // 结果导出成**无认证**节点。按 URI 惯例单段 userinfo 是口令。
            None => p.password = Some(a.userinfo.clone()),
        }
    }
    // `https://` 链接是 HTTPS 代理（HTTP over TLS）。此前 scheme 的 http/https
    // 都被塞进 `ProxyType::Http` 且不记录 TLS，导出成 clash 配置后退化成明文
    // `type: http`，mihomo 于是用明文 HTTP CONNECT 去连 443 端口 —— 握手必然
    // 失败，整批节点被误判为不可用（实测 24 个 443 端口 http 节点：明文通过
    // 0 个，强制 tls 后通过 3 个）。
    if tls {
        p.tls = Some(true);
    }
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_from_plain_yaml() {
        let raw =
            "download: 100\nupload: 50\ntotal: 1000\nexpire: 1700000000\nproxies:\n  - name: a\n";
        let u = extract_subscription_usage(raw);
        assert_eq!(u.download, Some(100));
        assert_eq!(u.upload, Some(50));
        assert_eq!(u.total, Some(1000));
        // expire given in seconds -> normalized to ms
        assert_eq!(u.expire, Some(1_700_000_000_000));
    }

    #[test]
    fn usage_from_nested_info_block() {
        // 部分订阅把用量放在顶层 `info:` 下，且可能与顶层字段混用。
        let raw = "info:\n  download: 200\n  upload: 100\n  total: 2000\n  expire: 1700000000\nproxies:\n  - name: a\n";
        let u = extract_subscription_usage(raw);
        assert_eq!(u.download, Some(200));
        assert_eq!(u.upload, Some(100));
        assert_eq!(u.total, Some(2000));
        assert_eq!(u.expire, Some(1_700_000_000_000));

        // 顶层字段优先于 info 块。
        let mixed = "download: 1\ninfo:\n  download: 2\n  total: 9\n";
        let u = extract_subscription_usage(mixed);
        assert_eq!(u.download, Some(1));
        assert_eq!(u.total, Some(9));
    }

    #[test]
    fn usage_from_base64() {
        // base64 of: download: 100\nupload: 50\ntotal: 1000\nexpire: 1700000000\n
        let raw = "ZG93bmxvYWQ6IDEwMAp1cGxvYWQ6IDUwCnRvdGFsOiAxMDAwCmV4cGlyZTogMTcwMDAwMDAwMAo=";
        let u = extract_subscription_usage(raw);
        assert_eq!(u.download, Some(100));
        assert_eq!(u.upload, Some(50));
        assert_eq!(u.total, Some(1000));
        assert_eq!(u.expire, Some(1_700_000_000_000));
    }

    #[test]
    fn usage_missing_fields_are_none() {
        let u = extract_subscription_usage("proxies:\n  - name: a\n");
        assert_eq!(u.download, None);
        assert_eq!(u.upload, None);
        assert_eq!(u.total, None);
        assert_eq!(u.expire, None);
    }

    #[test]
    fn usage_empty_input() {
        let u = extract_subscription_usage("");
        assert_eq!(u.download, None);
        assert_eq!(u.upload, None);
    }

    #[test]
    fn normalize_epoch_seconds_to_ms() {
        // seconds (10 digits) -> ms
        assert_eq!(normalize_epoch(1_700_000_000), 1_700_000_000_000);
        // already ms (13 digits) passes through
        assert_eq!(normalize_epoch(1_700_000_000_000), 1_700_000_000_000);
        // zero stays zero
        assert_eq!(normalize_epoch(0), 0);
    }

    #[test]
    fn clash_port_out_of_range_is_rejected() {
        // Port 70000 would silently truncate to 4464 via `as u16`; it must be
        // dropped instead (a node on port 0/>65535 can never connect).
        let yaml = "proxies:\n  - name: bad\n    type: ss\n    server: 1.2.3.4\n    port: 70000\n    cipher: aes-128-gcm\n    password: x\n";
        let out = parse_subscription(yaml);
        assert!(out.is_empty(), "out-of-range clash port must drop the node");
    }

    #[test]
    fn clash_port_zero_is_rejected() {
        let yaml = "proxies:\n  - name: z\n    type: ss\n    server: 1.2.3.4\n    port: 0\n    cipher: aes-128-gcm\n    password: x\n";
        assert!(
            parse_subscription(yaml).is_empty(),
            "port 0 must drop the node"
        );
    }

    #[test]
    fn singbox_tls_false_is_not_forced_true() {
        // Regression guard for H2: a `tls: false` outbound must keep tls = false
        // (the old code treated any present `tls` key as enabled).
        let json = r#"{
            "outbounds": [
                { "type": "shadowsocks", "tag": "plain", "server": "1.2.3.4", "server_port": 8388, "method": "aes-128-gcm", "password": "x", "tls": false }
            ]
        }"#;
        let out = parse_singbox_json(json).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].tls, Some(false), "tls:false must stay false");
    }

    #[test]
    fn vmess_tls_boolean_true_parsed() {
        // Regression guard for M5: vmess `tls` may be a JSON boolean; a boolean
        // `true` was previously misread as false.
        let b64 = base64::engine::general_purpose::STANDARD
            .encode(r#"{"add":"1.2.3.4","port":"443","id":"uuid","aid":"0","tls":true}"#);
        let uri = format!("vmess://{}", b64);
        let out = parse_subscription(&uri);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].tls, Some(true), "vmess tls:true (bool) must be true");
    }

    #[test]
    fn unknown_scheme_lines_are_not_ghost_nodes() {
        // A subscription body often carries `https://...` (update-url / rule
        // URLs). With M10 these must NOT become Http "ghost" nodes.
        let body = "proxies:\n  - name: real\n    type: ss\n    server: 1.2.3.4\n    port: 8388\n    cipher: aes-128-gcm\n    password: x\n\nhttps://example.com/rules.yaml\nupdate-url: https://example.com/upd\n";
        let out = parse_subscription(body);
        assert_eq!(out.len(), 1, "only the real clash node should parse");
        assert_eq!(out[0].name, "real");
    }

    #[test]
    fn uri_with_port_zero_is_skipped() {
        // split_authority must reject port 0 (M4) so we don't emit a dead node.
        let out = parse_subscription("ss://method:pass@1.2.3.4:0");
        assert!(out.is_empty(), "ss URI with port 0 must be skipped");
    }

    #[test]
    fn bare_http_url_without_userinfo_is_not_a_node() {
        // Q/B3: a standalone `https://host:port` line (no `user:pass@`) is an
        // update-url / rule link, not a proxy — it must not become a ghost
        // Http node.
        assert!(parse_subscription("https://example.com:8080/path").is_empty());
        assert!(parse_subscription("http://1.2.3.4:8080").is_empty());
    }

    #[test]
    fn http_proxy_with_userinfo_still_parses() {
        // ...but a genuine authenticated http proxy URI must keep working.
        let out = parse_subscription("http://user:pass@1.2.3.4:8080#MyHttp");
        assert_eq!(out.len(), 1, "http proxy with userinfo must parse");
        assert_eq!(out[0].server, "1.2.3.4");
        assert_eq!(out[0].port, 8080);
    }

    #[test]
    fn https_uri_sets_tls_true_but_plain_http_does_not() {
        // Regression guard for the https-proxy fix: `https://` must parse to
        // ProxyType::Http WITH tls=Some(true) so mihomo connects over TLS to
        // port 443; `http://` stays plaintext (tls None). Before the fix both
        // were flattened to a plaintext `type: http` and every 443 http node
        // failed the handshake (0/24 → 4/24 after the fix).
        let https = parse_subscription("https://user:pass@1.2.3.4:443#Secure");
        assert_eq!(https.len(), 1, "https proxy must parse");
        assert_eq!(https[0].type_.as_str(), "http", "https must be Http type");
        assert_eq!(https[0].tls, Some(true), "https:// must set tls=true");

        let http = parse_subscription("http://user:pass@1.2.3.4:8080#Plain");
        assert_eq!(http.len(), 1, "http proxy must parse");
        assert_eq!(http[0].type_.as_str(), "http");
        assert_eq!(http[0].tls, None, "plain http:// must not set tls");
    }

    #[test]
    fn base64_of_uri_list_is_decoded() {
        // A very common shape (v2rayN / NekoBox / laodavip-style short links):
        // the subscription body is base64 of a plain URI list, NOT a clash
        // yaml. The old parser only unwrapped base64 when the decoded text
        // contained `proxies:`, so these subs parsed to zero nodes. The real
        // laodavip sub is base64-of-`vless://` lines; mirror that here.
        let uri_list = "vless://e61f9468-4eac-43f2-99e4-ca057b3895de@free.us2.laodavip.com:443?mode=multi&security=reality&type=tcp#US2\r\nvless://e61f9468-4eac-43f2-99e4-ca057b3895de@free.us1.laodavip.com:443?type=tcp#US1\r\ntrojan://pass@hk.example.com:443?security=tls#HK\r\n";
        let b64 = base64::engine::general_purpose::STANDARD.encode(uri_list);
        let out = parse_subscription(&b64);
        assert_eq!(out.len(), 3, "base64-wrapped URI list must yield its nodes");
        let types: Vec<&str> = out.iter().map(|p| p.type_.as_str()).collect();
        assert!(types.contains(&"vless"));
        assert!(types.contains(&"trojan"));
    }

    #[test]
    fn base64_url_safe_uri_list_is_decoded() {
        // V2Board / Xboard / sspanel panels frequently emit URL-safe base64
        // (`-`/`_` instead of `+`/`/`). The old decoder only tried the standard
        // alphabet, so these subs decoded to nothing and parsed to zero nodes.
        // Build a genuinely URL-safe-only body: append two 0xFF bytes, whose
        // STANDARD base64 is "////" (index 63 = '/'), then swap '+'/'/' for
        // '-'/'_'. The trailing non-UTF8 junk decodes to replacement chars on a
        // line with no `://` and is ignored, so the node count stays 2.
        let mut bytes: Vec<u8> = b"vless://e61f9468-4eac-43f2-99e4-ca057b3895de@free.us2.laodavip.com:443?type=tcp#US2\r\ntrojan://pass@hk.example.com:443?security=tls#HK\r\n".to_vec();
        bytes.extend_from_slice(&[0xFF, 0xFF]);
        let std = base64::engine::general_purpose::STANDARD.encode(&bytes);
        assert!(
            std.contains('+') || std.contains('/'),
            "test setup: need a '+' or '/' to make a URL-safe form"
        );
        let b64 = std.replace('+', "-").replace('/', "_");
        assert_ne!(b64, std, "test setup: body must be genuinely URL-safe");
        let out = parse_subscription(&b64);
        assert_eq!(
            out.len(),
            2,
            "URL-safe base64 URI list must yield its nodes"
        );
        let types: Vec<&str> = out.iter().map(|p| p.type_.as_str()).collect();
        assert!(types.contains(&"vless"));
        assert!(types.contains(&"trojan"));
    }

    #[test]
    fn anytls_uri_is_parsed() {
        // AnyTLS authenticates with a password (NOT a uuid); the userinfo
        // before `@` is the password. The node in the third failing sub is
        // base64-of-`anytls://` URIs. Mirror that shape here.
        let line = "anytls://00000000-0000-4000-8000-000000000001@anytls.example.com:56147/?type=tcp&insecure=1&fp=chrome&sni=updates.cdn-apple.com#JP-AWS";
        let out = parse_subscription(line);
        assert_eq!(out.len(), 1, "anytls:// URI must parse to one node");
        let p = &out[0];
        assert_eq!(p.type_, ProxyType::AnyTls);
        assert_eq!(p.server, "anytls.example.com");
        assert_eq!(p.port, 56147);
        assert_eq!(
            p.password.as_deref(),
            Some("00000000-0000-4000-8000-000000000001")
        );
        // always TLS
        assert_eq!(p.tls, Some(true));
        // insecure=1 -> skip-cert-verify
        assert_eq!(p.skip_cert_verify, Some(true));
        assert_eq!(p.sni.as_deref(), Some("updates.cdn-apple.com"));
        assert_eq!(p.fingerprint.as_deref(), Some("chrome"));
        assert_eq!(p.name, "JP-AWS");
    }

    #[test]
    fn base64_of_anytls_uri_list_is_decoded() {
        // The real-world failing case: the body is base64 of an `anytls://`
        // URI list. The old `is_known_scheme` set did not include `anytls`, so
        // the base64 unwrap never fired and the sub parsed to zero nodes.
        let uri_list = "anytls://00000000-0000-4000-8000-000000000001@anytls.example.com:56147/?type=tcp&insecure=1&fp=chrome&sni=updates.cdn-apple.com#JP-AWS\r\nanytls://abc123@hk.example.com:443/?insecure=0&sni=hk.example.com#HK\r\n";
        let b64 = base64::engine::general_purpose::STANDARD.encode(uri_list);
        let out = parse_subscription(&b64);
        assert_eq!(
            out.len(),
            2,
            "base64-wrapped anytls URI list must yield its nodes"
        );
        assert!(out.iter().all(|p| p.type_ == ProxyType::AnyTls));
    }

    #[test]
    fn vless_reality_uri_is_parsed() {
        let line = "vless://11111111-2222-3333-4444-555555555555@reality.example.com:443?security=reality&pbk=PUBKEY123&sid=ab12&fp=chrome&sni=www.example.com&type=tcp&flow=xtls-rprx-vision#Reality-JP";
        let out = parse_subscription(line);
        assert_eq!(out.len(), 1);
        let p = &out[0];
        assert_eq!(p.type_, ProxyType::Vless);
        assert!(p.reality, "security=reality 必须标记 reality");
        assert_eq!(p.reality_public_key.as_deref(), Some("PUBKEY123"));
        assert_eq!(p.reality_short_id.as_deref(), Some("ab12"));
        assert_eq!(p.fingerprint.as_deref(), Some("chrome"));
        assert_eq!(p.tls, Some(true));
        assert!(p.is_exportable(), "带公钥的 REALITY 节点应可导出");
    }

    #[test]
    fn clash_reality_and_fingerprint_are_parsed() {
        let yaml = "proxies:\n  - name: R\n    type: vless\n    server: 1.2.3.4\n    port: 443\n    uuid: u\n    network: tcp\n    tls: true\n    client-fingerprint: chrome\n    reality-opts:\n      public-key: PK\n      short-id: ab12\n";
        let out = parse_subscription(yaml);
        assert_eq!(out.len(), 1);
        let p = &out[0];
        assert!(p.reality);
        assert_eq!(p.reality_public_key.as_deref(), Some("PK"));
        assert_eq!(p.reality_short_id.as_deref(), Some("ab12"));
        assert_eq!(p.fingerprint.as_deref(), Some("chrome"));
    }

    #[test]
    fn clash_h2_opts_are_parsed() {
        let yaml = "proxies:\n  - name: H2\n    type: vless\n    server: 1.2.3.4\n    port: 443\n    uuid: u\n    network: h2\n    tls: true\n    h2-opts:\n      host:\n        - h2.example.com\n      path: /h2\n";
        let out = parse_subscription(yaml);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].network.as_deref(), Some("h2"));
        assert_eq!(out[0].host.as_deref(), Some("h2.example.com"));
        assert_eq!(out[0].path.as_deref(), Some("/h2"));
    }

    #[test]
    fn singbox_http_transport_is_parsed() {
        let json = r#"{
            "outbounds": [
                { "type": "vless", "tag": "H", "server": "1.2.3.4", "server_port": 443, "uuid": "u",
                  "tls": {}, "transport": { "type": "http", "host": ["h.example.com"], "path": "/p" } }
            ]
        }"#;
        let out = parse_singbox_json(json).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].network.as_deref(), Some("h2"));
        assert_eq!(out[0].host.as_deref(), Some("h.example.com"));
        assert_eq!(out[0].path.as_deref(), Some("/p"));
    }

    #[test]
    fn vmess_port_and_numeric_aid_are_validated() {
        // port 为字符串 "0" 时旧代码会直接 parse 成 0；必须丢弃。
        let zero = base64::engine::general_purpose::STANDARD
            .encode(r#"{"add":"1.2.3.4","port":"0","id":"uuid","aid":"0"}"#);
        assert!(
            parse_subscription(&format!("vmess://{zero}")).is_empty(),
            "vmess port 0 必须丢弃"
        );

        // aid 为数字时也要保留（旧代码只读字符串）。
        let numeric_aid = base64::engine::general_purpose::STANDARD
            .encode(r#"{"add":"1.2.3.4","port":443,"id":"uuid","aid":2}"#);
        let out = parse_subscription(&format!("vmess://{numeric_aid}"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].alter_id, Some(2));
    }

    #[test]
    fn vmess_peer_is_used_as_sni() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(
            r#"{"add":"1.2.3.4","port":"443","id":"uuid","aid":"0","net":"ws","tls":"tls","peer":"cdn.example.com","host":"cdn.example.com","path":"/ws"}"#,
        );
        let out = parse_subscription(&format!("vmess://{b64}"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].sni.as_deref(), Some("cdn.example.com"));
        assert_eq!(out[0].tls, Some(true));
    }

    #[test]
    fn ss_plugin_parsed_from_uri_clash_and_singbox() {
        // SIP002: plugin 参数是 URL 编码的 `name;k=v;...`。
        let uri = "ss://YWVzLTI1Ni1nY206cGFzcw==@1.2.3.4:8388?plugin=obfs-local%3Bobfs%3Dhttp%3Bobfs-host%3Dexample.com#OBFS";
        let out = parse_subscription(uri);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].plugin.as_deref(), Some("obfs-local"));
        assert_eq!(
            out[0].plugin_opts.as_deref(),
            Some("obfs=http;obfs-host=example.com")
        );

        let yaml = "proxies:\n  - name: O\n    type: ss\n    server: 1.2.3.4\n    port: 8388\n    cipher: aes-128-gcm\n    password: pw\n    plugin: obfs\n    plugin-opts:\n      mode: http\n      host: example.com\n";
        let out = parse_subscription(yaml);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].plugin.as_deref(), Some("obfs"));
        let opts = out[0].plugin_opts.clone().unwrap_or_default();
        assert!(opts.contains("mode=http") && opts.contains("host=example.com"));

        let json = r#"{
            "outbounds": [
                { "type": "shadowsocks", "tag": "S", "server": "1.2.3.4", "server_port": 8388,
                  "method": "aes-128-gcm", "password": "pw",
                  "plugin": "obfs-local", "plugin_opts": "obfs=http;obfs-host=example.com" }
            ]
        }"#;
        let out = parse_singbox_json(json).unwrap();
        assert_eq!(out[0].plugin.as_deref(), Some("obfs-local"));
        assert_eq!(
            out[0].plugin_opts.as_deref(),
            Some("obfs=http;obfs-host=example.com")
        );
    }

    #[test]
    fn hysteria2_obfs_parsed_from_uri_and_singbox() {
        let uri = "hysteria2://pw@hy.example.com:443?obfs=salamander&obfs-password=secret#HY-OBFS";
        let out = parse_subscription(uri);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].obfs.as_deref(), Some("salamander"));
        assert_eq!(out[0].obfs_password.as_deref(), Some("secret"));

        let json = r#"{
            "outbounds": [
                { "type": "hysteria2", "tag": "H", "server": "hy.example.com", "server_port": 443,
                  "password": "pw", "tls": { "enabled": true },
                  "obfs": { "type": "salamander", "password": "secret" } }
            ]
        }"#;
        let out = parse_singbox_json(json).unwrap();
        assert_eq!(out[0].obfs.as_deref(), Some("salamander"));
        assert_eq!(out[0].obfs_password.as_deref(), Some("secret"));
    }

    #[test]
    fn tuic_advanced_fields_parsed_from_uri_clash_and_singbox() {
        let uri = "tuic://11111111-2222-3333-4444-555555555555:pw@tu.example.com:443?alpn=h3,spdy%2F3.1&congestion_control=bbr&udp_relay_mode=quic&udp_over_stream=1&zero_rtt_handshake=1&sni=tu.example.com#TU";
        let out = parse_subscription(uri);
        assert_eq!(out.len(), 1);
        let p = &out[0];
        assert_eq!(
            p.alpn.as_deref(),
            Some(&["h3".to_string(), "spdy/3.1".to_string()][..])
        );
        assert_eq!(p.congestion_control.as_deref(), Some("bbr"));
        assert_eq!(p.udp_relay_mode.as_deref(), Some("quic"));
        assert_eq!(p.udp_over_stream, Some(true));
        assert_eq!(p.zero_rtt_handshake, Some(true));

        let yaml = "proxies:\n  - name: T\n    type: tuic\n    server: tu.example.com\n    port: 443\n    uuid: u\n    password: pw\n    alpn:\n      - h3\n    congestion-controller: bbr\n    udp-relay-mode: native\n    udp-over-stream: true\n    reduce-rtt: true\n";
        let out = parse_subscription(yaml);
        assert_eq!(out[0].congestion_control.as_deref(), Some("bbr"));
        assert_eq!(out[0].udp_relay_mode.as_deref(), Some("native"));
        assert_eq!(out[0].udp_over_stream, Some(true));
        assert_eq!(out[0].zero_rtt_handshake, Some(true));
        assert_eq!(out[0].alpn.as_deref(), Some(&["h3".to_string()][..]));

        let json = r#"{
            "outbounds": [
                { "type": "tuic", "tag": "T", "server": "tu.example.com", "server_port": 443,
                  "uuid": "u", "password": "pw", "congestion_control": "cubic",
                  "udp_relay_mode": "quic", "udp_over_stream": true,
                  "zero_rtt_handshake": false, "tls": { "enabled": true, "alpn": ["h3"] } }
            ]
        }"#;
        let out = parse_singbox_json(json).unwrap();
        assert_eq!(out[0].congestion_control.as_deref(), Some("cubic"));
        assert_eq!(out[0].udp_relay_mode.as_deref(), Some("quic"));
        assert_eq!(out[0].udp_over_stream, Some(true));
        assert_eq!(out[0].zero_rtt_handshake, Some(false));
        assert_eq!(out[0].alpn.as_deref(), Some(&["h3".to_string()][..]));
    }

    #[test]
    fn hysteria2_up_down_alpn_parsed() {
        let uri = "hysteria2://pw@hy.example.com:443?up=100&down=200&alpn=h3#HY";
        let out = parse_subscription(uri);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].up_mbps, Some(100));
        assert_eq!(out[0].down_mbps, Some(200));
        assert_eq!(out[0].alpn.as_deref(), Some(&["h3".to_string()][..]));

        let yaml = "proxies:\n  - name: H\n    type: hysteria2\n    server: hy.example.com\n    port: 443\n    password: pw\n    up: \"30 Mbps\"\n    down: \"200 Mbps\"\n    alpn:\n      - h3\n";
        let out = parse_subscription(yaml);
        assert_eq!(out[0].up_mbps, Some(30));
        assert_eq!(out[0].down_mbps, Some(200));
        assert_eq!(out[0].alpn.as_deref(), Some(&["h3".to_string()][..]));

        // mihomo 允许 Gbps/kbps 单位；必须换算而不是只取数字前缀。
        let units = "proxies:\n  - name: H\n    type: hysteria2\n    server: hy.example.com\n    port: 443\n    password: pw\n    up: \"1 Gbps\"\n    down: \"500 kbps\"\n";
        let out = parse_subscription(units);
        assert_eq!(out[0].up_mbps, Some(1000));
        assert_eq!(out[0].down_mbps, Some(1));

        let json = r#"{
            "outbounds": [
                { "type": "hysteria2", "tag": "H", "server": "hy.example.com", "server_port": 443,
                  "password": "pw", "up_mbps": 50, "down_mbps": 300,
                  "tls": { "enabled": true, "alpn": ["h3"] } }
            ]
        }"#;
        let out = parse_singbox_json(json).unwrap();
        assert_eq!(out[0].up_mbps, Some(50));
        assert_eq!(out[0].down_mbps, Some(300));
        assert_eq!(out[0].alpn.as_deref(), Some(&["h3".to_string()][..]));
    }

    #[test]
    fn hysteria2_and_tuic_mark_tls_true() {
        let hy2 = parse_subscription("hysteria2://pass@hy.example.com:443?sni=hy.example.com#HY");
        assert_eq!(hy2.len(), 1);
        assert_eq!(hy2[0].tls, Some(true), "hysteria2 始终 TLS");

        let tuic = parse_subscription(
            "tuic://11111111-2222-3333-4444-555555555555:pass@tu.example.com:443?sni=tu.example.com#TU",
        );
        assert_eq!(tuic.len(), 1);
        assert_eq!(tuic[0].tls, Some(true), "tuic 始终 TLS");
        assert_eq!(tuic[0].sni.as_deref(), Some("tu.example.com"));
    }

    #[test]
    fn nested_base64_recursion_is_depth_capped() {
        // Q/B8: deeply nested base64 must not recurse unboundedly. Build a
        // payload wrapped in MORE layers than MAX_B64_DEPTH: parsing must
        // terminate (no stack overflow) and simply yield nothing.
        let inner = "proxies:\n  - name: n\n    type: ss\n    server: 1.2.3.4\n    port: 8388\n    cipher: aes-128-gcm\n    password: x\n";
        let mut wrapped = inner.to_string();
        for _ in 0..(MAX_B64_DEPTH + 2) {
            // keep a marker so the recursion heuristic keeps firing
            wrapped = base64::engine::general_purpose::STANDARD
                .encode(format!("proxies:\n# marker\n{}", wrapped));
        }
        let out = parse_subscription(&wrapped); // must return, not overflow
        assert!(out.is_empty(), "over-deep base64 nesting yields no nodes");

        // one legitimate layer still works
        let one = base64::engine::general_purpose::STANDARD.encode(inner);
        let out1 = parse_subscription(&one);
        assert_eq!(out1.len(), 1, "single base64 layer must still parse");
    }

    // ===================== 第七轮：格式识别与字段保真 =====================

    #[test]
    fn clash_string_port_and_aid_are_accepted() {
        // 真实生成器与手写配置常见 `port: "443"` / `alterId: "0"`。
        // 旧写法只认数字，整节点被静默丢弃（表现为导入后节点数莫名变少）。
        let yaml = "proxies:\n  - name: A\n    type: vmess\n    server: h.example\n    port: \"443\"\n    uuid: u1\n    alterId: \"0\"\n    cipher: auto\n";
        let out = parse_subscription(yaml);
        assert_eq!(out.len(), 1, "字符串端口必须能解析: {out:?}");
        assert_eq!(out[0].port, 443);
        assert_eq!(out[0].alter_id, Some(0));
    }

    #[test]
    fn bom_prefixed_subscription_is_parsed() {
        // UTF-8 BOM 不是 `trim()` 的空白：带 BOM 的本地粘贴此前会让所有
        // 格式检测同时失效，用户看到的是「0 个节点」。
        let yaml = "\u{feff}proxies:\n  - name: A\n    type: ss\n    server: 1.2.3.4\n    port: 8388\n    cipher: aes-256-gcm\n    password: x\n";
        let out = parse_subscription(yaml);
        assert_eq!(out.len(), 1, "带 BOM 的 YAML 必须照常解析");
    }

    #[test]
    fn singbox_string_port_tls_object_and_flow_are_parsed() {
        // sing-box：`server_port` 可能是字符串；`tls` 是对象且 `enabled: false`
        // 表示明文；vless 的 `flow` 与 REALITY 必须保留。
        let json = r#"{
          "outbounds": [
            { "type": "vless", "tag": "V", "server": "v.example", "server_port": "443",
              "uuid": "u-1", "flow": "xtls-rprx-vision",
              "tls": { "enabled": true, "server_name": "www.x.com",
                       "reality": { "enabled": true, "short_id": "ab12" },
                       "public_key": "PUB", "utls": { "fingerprint": "chrome" },
                       "alpn": ["h2"] } },
            { "type": "trojan", "tag": "T", "server": "t.example", "server_port": 443,
              "password": "pw", "tls": { "enabled": false } },
            { "type": "hysteria2", "tag": "H", "server": "h.example", "server_port": 443,
              "password": "hp" }
          ]
        }"#;
        let out = parse_subscription(json);
        assert_eq!(out.len(), 3, "三个 outbound 都要落地: {out:?}");
        let v = out.iter().find(|x| x.name == "V").unwrap();
        assert_eq!(v.port, 443);
        assert_eq!(v.flow.as_deref(), Some("xtls-rprx-vision"));
        assert!(v.reality, "tls.reality.enabled 必须标记 REALITY");
        assert_eq!(v.reality_public_key.as_deref(), Some("PUB"));
        assert_eq!(v.reality_short_id.as_deref(), Some("ab12"));
        assert_eq!(v.fingerprint.as_deref(), Some("chrome"));
        assert_eq!(v.sni.as_deref(), Some("www.x.com"));
        let t = out.iter().find(|x| x.name == "T").unwrap();
        assert_eq!(t.tls, Some(false), "tls.enabled=false 不能被当成启用 TLS");
    }

    #[test]
    fn singbox_socks_username_is_kept_and_unknown_type_becomes_other() {
        let json = r#"{
          "outbounds": [
            { "type": "socks", "tag": "S", "server": "10.0.0.1", "server_port": 1080,
              "username": "bob", "password": "pw" },
            { "type": "tor", "tag": "X", "server": "10.0.0.2", "server_port": 9050 }
          ]
        }"#;
        let out = parse_subscription(json);
        assert_eq!(out.len(), 2, "未知类型不得静默丢弃（与 clash 分支一致）");
        let so = out.iter().find(|x| x.name == "S").unwrap();
        assert_eq!(
            so.extra
                .as_ref()
                .and_then(|e| e.get("user"))
                .and_then(|x| x.as_str()),
            Some("bob")
        );
        let x = out.iter().find(|x| x.name == "X").unwrap();
        assert_eq!(x.type_, ProxyType::Other);
        assert!(!x.is_exportable(), "other 类型必须由导出侧过滤");
    }

    #[test]
    fn v2ray_json_config_is_parsed_including_reality() {
        // v2ray/Xray 形状（`protocol` + `settings.vnext/servers` + `streamSettings`）。
        // 入口注释一直声称支持，实际解析结果是 0 个节点。
        let json = r#"{
          "outbounds": [
            { "tag": "VM", "protocol": "vmess",
              "settings": { "vnext": [{ "address": "v.example", "port": 443,
                    "users": [{ "id": "u-1", "alterId": "1", "security": "auto" }] }] },
              "streamSettings": { "network": "ws", "security": "tls",
                "tlsSettings": { "serverName": "sni.example", "alpn": ["h2"], "fingerprint": "chrome" },
                "wsSettings": { "path": "/vm", "headers": { "Host": "cdn.example" } } } },
            { "tag": "VL", "protocol": "vless",
              "settings": { "vnext": [{ "address": "r.example", "port": "443",
                    "users": [{ "id": "u-2", "flow": "xtls-rprx-vision", "encryption": "none" }] }] },
              "streamSettings": { "network": "tcp", "security": "reality",
                "realitySettings": { "serverName": "www.r.com", "publicKey": "PUB2", "shortId": "cd", "fingerprint": "firefox" } } },
            { "tag": "TR", "protocol": "trojan",
              "settings": { "servers": [{ "address": "t.example", "port": 443, "password": "tpw" }] },
              "streamSettings": { "network": "tcp", "security": "tls", "tlsSettings": { "serverName": "t.example" } } },
            { "tag": "SS", "protocol": "shadowsocks",
              "settings": { "servers": [{ "address": "s.example", "port": 8388, "password": "spw", "method": "aes-256-gcm" }] },
              "streamSettings": { "network": "tcp", "security": "" } }
          ]
        }"#;
        let out = parse_subscription(json);
        assert_eq!(out.len(), 4, "v2ray 配置必须解析出 4 个节点: {out:?}");
        let by = |n: &str| out.iter().find(|x| x.name == n).unwrap().clone();
        let vm = by("VM");
        assert_eq!(
            (vm.type_, vm.port, vm.uuid.as_deref()),
            (ProxyType::Vmess, 443, Some("u-1"))
        );
        assert_eq!(vm.alter_id, Some(1), "alterId 的字符串写法也要接受");
        assert_eq!(
            (
                vm.network.as_deref(),
                vm.path.as_deref(),
                vm.host.as_deref()
            ),
            (Some("ws"), Some("/vm"), Some("cdn.example"))
        );
        assert_eq!(vm.sni.as_deref(), Some("sni.example"));
        assert_eq!(vm.fingerprint.as_deref(), Some("chrome"));
        assert_eq!(vm.tls, Some(true));
        let vl = by("VL");
        assert!(vl.reality && vl.tls == Some(true));
        assert_eq!(vl.reality_public_key.as_deref(), Some("PUB2"));
        assert_eq!(vl.reality_short_id.as_deref(), Some("cd"));
        assert_eq!(vl.flow.as_deref(), Some("xtls-rprx-vision"));
        assert_eq!(vl.port, 443, "字符串 port 也要能吃");
        let tr = by("TR");
        assert_eq!(
            (tr.type_, tr.password.as_deref()),
            (ProxyType::Trojan, Some("tpw"))
        );
        let ss = by("SS");
        assert_eq!(
            (ss.type_, ss.password.as_deref(), ss.method.as_deref()),
            (ProxyType::Ss, Some("spw"), Some("aes-256-gcm"))
        );
        assert_eq!(ss.tls, Some(false), "security 为空不得判成 TLS");
    }

    #[test]
    fn v2ray_export_round_trips_back_into_the_same_nodes() {
        // 本项目自己的 v2ray 导出必须能被本项目重新导入（裸数组形态）。
        let mut vm = Proxy::new("N1".into(), ProxyType::Vmess, "a.example".into(), 443);
        vm.uuid = Some("u-9".into());
        vm.network = Some("ws".into());
        vm.path = Some("/p".into());
        vm.host = Some("cdn.example".into());
        vm.tls = Some(true);
        vm.sni = Some("cdn.example".into());
        let exported = crate::export::to_v2ray_json(std::slice::from_ref(&vm));
        let back = parse_subscription(&exported);
        assert_eq!(back.len(), 1, "导出的 v2ray 数组应可重新导入: {exported}");
        assert_eq!(
            (
                back[0].name.as_str(),
                back[0].server.as_str(),
                back[0].port,
                back[0].uuid.as_deref().unwrap(),
                back[0].network.as_deref().unwrap(),
                back[0].path.as_deref().unwrap()
            ),
            ("N1", "a.example", 443, "u-9", "ws", "/p")
        );
        assert_eq!(back[0].sni.as_deref(), Some("cdn.example"));
    }

    #[test]
    fn uri_query_keys_and_security_are_case_insensitive() {
        // `?Type=ws&SNI=x` 与 `?Security=None` 都是真实下发写法。
        let out = parse_subscription(
            "vless://u-3@h.example:443?Type=ws&SNI=sni.example&Security=None&Path=/q&Host=cdn.example#Q",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].network.as_deref(), Some("ws"), "大写 Type 被忽略");
        assert_eq!(
            out[0].sni.as_deref(),
            Some("sni.example"),
            "大写 SNI 被忽略"
        );
        assert_eq!(out[0].path.as_deref(), Some("/q"), "大写 Path 被忽略");
        assert_eq!(
            out[0].host.as_deref(),
            Some("cdn.example"),
            "大写 Host 被忽略"
        );
        assert_eq!(out[0].tls, Some(false), "Security=None 不得被判成启用 TLS");
    }

    #[test]
    fn percent_encoded_userinfo_is_decoded() {
        // 密码里的 `%40` / `%3A` 不解码就会按字面发出 → 认证失败、整节点全灭。
        let out = parse_subscription("trojan://p%40ss%3A2@h.example:443?sni=h.example#P");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].password.as_deref(), Some("p@ss:2"));

        let socks = parse_subscription("socks5://bob:p%40w@10.0.0.1:1080#S");
        assert_eq!(socks.len(), 1);
        assert_eq!(socks[0].password.as_deref(), Some("p@w"));
        assert_eq!(
            socks[0]
                .extra
                .as_ref()
                .and_then(|e| e.get("user"))
                .and_then(|x| x.as_str()),
            Some("bob")
        );
    }

    #[test]
    fn socks_single_segment_userinfo_is_kept_as_password() {
        // `socks5://secret@host:1080`：旧写法既不填用户名也不填密码，
        // 导出成无认证节点。按 URI 惯例单段 userinfo 是口令。
        let out = parse_subscription("socks5://secret@10.0.0.1:1080#SO");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].password.as_deref(), Some("secret"));
    }

    #[test]
    fn usage_accepts_string_numbers_and_date_expires() {
        // `total: 3.0`、`expire: "2026-12-31 23:59:59"`、微秒时间戳都是真实写法。
        let yaml = "info:\n  download: \"100\"\n  upload: 50\n  total: 3.0\n  expire: \"2026-12-31 23:59:59\"\nproxies:\n  - name: a\n    type: ss\n    server: 1.2.3.4\n    port: 1\n    cipher: x\n    password: y\n";
        let u = extract_subscription_usage(yaml);
        assert_eq!(u.download, Some(100), "字符串数字应取值");
        assert_eq!(u.upload, Some(50));
        assert_eq!(u.total, Some(3), "浮点数字应取值");
        assert_eq!(
            u.expire,
            Some(1_798_761_599_000),
            "日期字符串应换成 epoch 毫秒: {:?}",
            u.expire
        );

        // 微秒 / 纳秒时间戳归一到毫秒（旧实现原样返回，面板显示几万年以后）。
        assert_eq!(normalize_epoch(1_700_000_000_000_000), 1_700_000_000_000);
        assert_eq!(
            normalize_epoch(1_700_000_000_000_000_000),
            1_700_000_000_000
        );
        assert_eq!(normalize_epoch(0), 0);
    }

    #[test]
    fn date_str_to_epoch_ms_matches_known_instants() {
        assert_eq!(date_str_to_epoch_ms("1970-01-01"), Some(0));
        assert_eq!(
            date_str_to_epoch_ms("2000-02-29 12:00"),
            Some(951_825_600_000)
        );
        assert_eq!(
            date_str_to_epoch_ms("2026-12-31T23:59:59"),
            Some(1_798_761_599_000)
        );
        // 非法日期一律 None，不能瞎猜。
        assert_eq!(date_str_to_epoch_ms("2026-13-01"), None);
        assert_eq!(date_str_to_epoch_ms("nonsense"), None);
    }
}
