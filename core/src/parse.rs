use crate::model::{Proxy, ProxyType};
use base64::Engine;
use percent_encoding::percent_decode_str;
use std::collections::HashMap;

/// Entry point: given the raw text of a subscription, figure out the format
/// and parse it into a list of proxies. Robust to mixed content (a `proxies:`
/// YAML block with bare `vmess://`/`trojan://`/... URI lines appended).
pub fn parse_subscription(raw: &str) -> Vec<Proxy> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }

    // 1) base64-wrapped (most clash subscriptions are base64 of yaml)
    if let Ok(decoded) = b64_decode(raw) {
        let text = String::from_utf8_lossy(&decoded);
        if !text.trim().is_empty() && text.trim() != raw && (text.contains("proxies:") || text.contains("\"outbounds\"") || text.contains("\"proxies\"")) {
            return parse_subscription(&text);
        }
    }

    let mut out = Vec::new();

    // 2) structured JSON (sing-box outbounds / clash json)
    if raw.starts_with('{') || raw.starts_with('[') {
        if raw.contains("outbounds") {
            if let Ok(p) = parse_singbox_json(raw) {
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
    for line in raw.lines() {
        let line = line.trim();
        if let Some((scheme, _)) = line.split_once("://") {
            if is_known_scheme(scheme) {
                if let Some(p) = parse_uri(line) {
                    out.push(p);
                }
            }
        }
    }

    out
}

/// Proxy URI schemes we know how to parse. Any `://` line whose scheme is not
/// in this set is left alone (avoids turning unrelated URLs into ghost nodes).
fn is_known_scheme(scheme: &str) -> bool {
    matches!(
        scheme.to_ascii_lowercase().as_str(),
        "vmess" | "vless" | "trojan" | "ss" | "ssr" | "hysteria2" | "hy2" | "hysteria" | "tuic"
            | "socks5" | "socks" | "http" | "https"
    )
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
        } else if line.starts_with(' ')
            || line.starts_with('\t')
            || line.starts_with('-')
        {
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

fn b64_decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    let s = s.trim().replace(['\n', '\r'], "");
    let s = match base64::engine::general_purpose::STANDARD.decode(&s) {
        Ok(b) => return Ok(b),
        Err(_) => s,
    };
    // try with padding
    let pad = format!("{}{}", s, "=".repeat((4 - s.len() % 4) % 4));
    base64::engine::general_purpose::STANDARD.decode(&pad)
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
        u.download = v.get("download").and_then(|x| x.as_u64());
        u.upload = v.get("upload").and_then(|x| x.as_u64());
        u.total = v.get("total").and_then(|x| x.as_u64());
        if let Some(exp) = v.get("expire").and_then(|x| x.as_u64()) {
            u.expire = Some(normalize_epoch(exp));
        }
    }
    u
}

/// Clash `expire` (and the `Subscription-Userinfo` header) report seconds;
/// the rest of the app uses epoch milliseconds, so promote sub-12-digit
/// values to ms. Values already in ms (>= 1e12) pass through untouched.
pub fn normalize_epoch(v: u64) -> u64 {
    if v < 1_000_000_000_000 {
        v.saturating_mul(1000)
    } else {
        v
    }
}

fn as_str<'a>(v: &'a serde_yaml::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

fn from_clash_value(v: &serde_yaml::Value) -> Option<Proxy> {
    let type_str = as_str(v, "type")?;
    let server = as_str(v, "server")?;
    // Reject out-of-range ports (e.g. 70000) instead of silently truncating to
    // a wrong value via `as u16` — a node with port 0 or >65535 can never connect.
    let port = match v.get("port").and_then(|x| x.as_u64()) {
        Some(p) if (1..=65535).contains(&p) => p as u16,
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
    p.alter_id = v.get("alterId").and_then(|x| x.as_u64()).map(|x| x as u32);
    p.cipher = as_str(v, "cipher").map(str::to_string);
    p.flow = as_str(v, "flow").map(str::to_string);
    p.password = as_str(v, "password").map(str::to_string);
    p.method = as_str(v, "method").map(str::to_string);
    p.network = as_str(v, "network").map(str::to_string);
    p.tls = v.get("tls").and_then(|x| x.as_bool());
    p.sni = as_str(v, "sni").or_else(|| as_str(v, "servername")).map(str::to_string);
    p.skip_cert_verify = v.get("skip-cert-verify").and_then(|x| x.as_bool());
    p.path = as_str(v, "path")
        .or_else(|| v.get("ws-opts").and_then(|w| w.get("path")).and_then(|x| x.as_str()))
        .map(str::to_string);
    p.host = as_str(v, "host")
        .or_else(|| {
            v.get("ws-opts")
                .and_then(|w| w.get("headers"))
                .and_then(|h| h.get("Host"))
                .and_then(|x| x.as_str())
        })
        .map(str::to_string);
    p.service_name = v
        .get("grpc-opts")
        .and_then(|g| g.get("grpc-service-name"))
        .and_then(|x| x.as_str())
        .map(str::to_string);
    Some(p)
}

fn parse_singbox_json(s: &str) -> Result<Vec<Proxy>, serde_json::Error> {
    let v: serde_json::Value = serde_json::from_str(s)?;
    let mut out = Vec::new();
    if let Some(outbounds) = v.get("outbounds").and_then(|o| o.as_array()) {
        for ob in outbounds {
            let type_str = ob.get("type").and_then(|x| x.as_str()).unwrap_or("");
            let server = match ob.get("server").and_then(|x| x.as_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let port = match ob.get("server_port").and_then(|x| x.as_u64()) {
                Some(p) if (1..=65535).contains(&p) => p as u16,
                _ => continue, // skip outbounds without a valid port
            };
            let name = ob.get("tag").and_then(|x| x.as_str()).unwrap_or(&server).to_string();
            let type_ = match type_str {
                "shadowsocks" => ProxyType::Ss,
                "trojan" => ProxyType::Trojan,
                "vmess" => ProxyType::Vmess,
                "vless" => ProxyType::Vless,
                "hysteria2" => ProxyType::Hysteria2,
                "tuic" => ProxyType::Tuic,
                "socks" => ProxyType::Socks5,
                "wireguard" => ProxyType::Wireguard,
                _ => continue,
            };
            let mut p = Proxy::new(name, type_, server, port);
            p.password = ob.get("password").and_then(|x| x.as_str()).map(str::to_string);
            p.method = ob.get("method").and_then(|x| x.as_str()).map(str::to_string);
            p.uuid = ob.get("uuid").and_then(|x| x.as_str()).map(str::to_string);
            // sing-box uses `tls: {}` to enable and `tls: false` to disable; a
            // bare `tls: true`/object means enabled. Don't treat a present key
            // as unconditionally enabled (that forced TLS onto plaintext nodes).
            p.tls = Some(match ob.get("tls") {
                Some(serde_json::Value::Bool(b)) => *b,
                Some(_) => true,
                None => false,
            });
            if let Some(tls) = ob.get("tls") {
                p.sni = tls.get("server_name").and_then(|x| x.as_str()).map(str::to_string);
                p.skip_cert_verify = tls.get("insecure").and_then(|x| x.as_bool());
            }
            // Transport (ws / grpc / h2) — sing-box nests this under
            // `transport: { type, path, headers, host, service_name }`.
            if let Some(transport) = ob.get("transport") {
                let ttype = transport.get("type").and_then(|x| x.as_str()).unwrap_or("");
                match ttype {
                    "ws" => {
                        p.network = Some("ws".to_string());
                        p.path = transport.get("path").and_then(|x| x.as_str()).map(str::to_string);
                        p.host = transport
                            .get("headers")
                            .and_then(|h| h.get("Host"))
                            .and_then(|x| x.as_str())
                            .map(str::to_string);
                    }
                    "grpc" => {
                        p.network = Some("grpc".to_string());
                        p.service_name = transport.get("service_name").and_then(|x| x.as_str()).map(str::to_string);
                    }
                    "h2" => {
                        p.network = Some("h2".to_string());
                        p.path = transport.get("path").and_then(|x| x.as_str()).map(str::to_string);
                        p.host = transport.get("host").and_then(|x| x.as_str()).map(str::to_string);
                    }
                    _ => {}
                }
            }
            out.push(p);
        }
    }
    Ok(out)
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
        "hysteria2" | "hy2" | "hysteria" => parse_hy2(&body, name),
        "tuic" => parse_tuic(&body, name),
        "socks5" | "socks" => parse_socks(&body, name, ProxyType::Socks5),
        "http" | "https" => parse_socks(&body, name, ProxyType::Http),
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
    let (userinfo, hostport) = match left.rsplit_once('@') {
        Some((u, h)) => (u.to_string(), h.to_string()),
        None => (String::new(), left.to_string()),
    };
    let (host, port) = if hostport.starts_with('[') {
        // ipv6 [::1]:port
        let end = hostport.find(']')?;
        let host = hostport[1..end].to_string();
        let port: u16 = hostport[end + 1..].trim_start_matches(':').parse().ok()?;
        if port == 0 {
            return None;
        }
        (host, port)
    } else {
        let mut it = hostport.rsplitn(2, ':');
        let port: u16 = it.next()?.parse().ok()?;
        let host = it.next()?.to_string();
        if port == 0 {
            return None;
        }
        (host, port)
    };
    let query = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), decode_percent(v)))
        .collect();
    Some(Auth { userinfo, host, port, query })
}

fn parse_vmess(body: &str, name: Option<String>) -> Option<Proxy> {
    let decoded = b64_decode(body).ok()?;
    let json = String::from_utf8_lossy(&decoded);
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    let server = v.get("add")?.as_str()?.to_string();
    let port: u16 = v.get("port").and_then(|x| {
        x.as_str().and_then(|s| s.parse().ok()).or_else(|| x.as_u64().map(|n| n as u16))
    })?;
    let pname = name.unwrap_or_else(|| v.get("ps").and_then(|x| x.as_str()).unwrap_or(&server).to_string());
    let mut p = Proxy::new(pname, ProxyType::Vmess, server, port);
    p.uuid = v.get("id").and_then(|x| x.as_str()).map(str::to_string);
    p.alter_id = v.get("aid").and_then(|x| x.as_str()).and_then(|s| s.parse().ok());
    p.cipher = v.get("scy").or_else(|| v.get("cipher")).and_then(|x| x.as_str()).map(str::to_string);
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
    p.sni = v.get("sni").and_then(|x| x.as_str()).map(str::to_string);
    p.path = v.get("path").and_then(|x| x.as_str()).map(str::to_string);
    p.host = v.get("host").and_then(|x| x.as_str()).map(str::to_string);
    Some(p)
}

fn parse_vless(body: &str, name: Option<String>) -> Option<Proxy> {
    let a = split_authority(body)?;
    let mut p = Proxy::new(name.unwrap_or_else(|| a.host.clone()), ProxyType::Vless, a.host, a.port);
    p.uuid = Some(a.userinfo);
    p.flow = a.query.get("flow").cloned();
    p.network = a.query.get("type").or_else(|| a.query.get("network")).cloned();
    p.tls = Some(a.query.get("security").map(|s| s != "none").unwrap_or(false));
    p.sni = a.query.get("sni").cloned();
    p.fingerprint = a.query.get("fp").cloned();
    p.path = a.query.get("path").cloned();
    p.host = a.query.get("host").cloned();
    p.service_name = a.query.get("serviceName").cloned();
    Some(p)
}

fn parse_trojan(body: &str, name: Option<String>) -> Option<Proxy> {
    let a = split_authority(body)?;
    let mut p = Proxy::new(name.unwrap_or_else(|| a.host.clone()), ProxyType::Trojan, a.host, a.port);
    p.password = Some(a.userinfo);
    p.tls = Some(true);
    p.sni = a.query.get("sni").cloned();
    p.flow = a.query.get("flow").cloned();
    p.network = a.query.get("type").cloned();
    p.path = a.query.get("path").cloned();
    p.host = a.query.get("host").cloned();
    p.skip_cert_verify = a.query.get("allowInsecure").map(|v| v == "1" || v == "true");
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
                let mut p = Proxy::new(name.unwrap_or_else(|| a.host.clone()), ProxyType::Ss, a.host, a.port);
                p.method = Some(method);
                p.password = Some(password);
                return Some(p);
            }
        }
    }
    // legacy whole-base64 form
    if let Ok(decoded) = b64_decode(body) {
        let s = String::from_utf8_lossy(&decoded);
        if let Some(a) = split_authority(&s) {
            let (method, password) = decode_ss_userinfo(&a.userinfo);
            let mut p = Proxy::new(name.unwrap_or_else(|| a.host.clone()), ProxyType::Ss, a.host, a.port);
            p.method = Some(method);
            p.password = Some(password);
            return Some(p);
        }
    }
    None
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

fn parse_ssr(body: &str, name: Option<String>) -> Option<Proxy> {
    // ssr://base64(host:port:protocol:method:obfs:base64pass/?params)
    let decoded = b64_decode(body).ok()?;
    let s = String::from_utf8_lossy(&decoded);
    let (head, _params) = s.split_once("/?").unwrap_or((&s, ""));
    let parts: Vec<&str> = head.split(':').collect();
    if parts.len() < 6 {
        return None;
    }
    let server = parts[0].to_string();
    let port: u16 = parts[1].parse().ok().filter(|p| (1..=65535).contains(p))?;
    let method = parts[3].to_string();
    let password = b64_decode(parts[5]).ok().map(|b| String::from_utf8_lossy(&b).to_string()).unwrap_or_default();
    let mut p = Proxy::new(name.unwrap_or_else(|| server.clone()), ProxyType::Ss, server, port);
    p.method = Some(method);
    p.password = Some(password);
    Some(p)
}

fn parse_hy2(body: &str, name: Option<String>) -> Option<Proxy> {
    let a = split_authority(body)?;
    let mut p = Proxy::new(name.unwrap_or_else(|| a.host.clone()), ProxyType::Hysteria2, a.host, a.port);
    // userinfo may be "user:pass" or just "pass"
    if let Some((_, pass)) = a.userinfo.rsplit_once(':') {
        p.password = Some(pass.to_string());
    } else {
        p.password = Some(a.userinfo);
    }
    p.sni = a.query.get("sni").cloned();
    p.skip_cert_verify = a.query.get("insecure").map(|v| v == "1" || v == "true");
    Some(p)
}

fn parse_tuic(body: &str, name: Option<String>) -> Option<Proxy> {
    let a = split_authority(body)?;
    let mut p = Proxy::new(name.unwrap_or_else(|| a.host.clone()), ProxyType::Tuic, a.host, a.port);
    if let Some((u, pass)) = a.userinfo.rsplit_once(':') {
        p.uuid = Some(u.to_string());
        p.password = Some(pass.to_string());
    } else {
        p.password = Some(a.userinfo);
    }
    p.sni = a.query.get("sni").cloned();
    Some(p)
}

fn parse_socks(body: &str, name: Option<String>, t: ProxyType) -> Option<Proxy> {
    let a = split_authority(body)?;
    let mut p = Proxy::new(name.unwrap_or_else(|| a.host.clone()), t, a.host, a.port);
    if !a.userinfo.is_empty() {
        if let Some((u, pass)) = a.userinfo.rsplit_once(':') {
            p.extra = Some(serde_json::json!({"user": u}));
            p.password = Some(pass.to_string());
        }
    }
    Some(p)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_from_plain_yaml() {
        let raw = "download: 100\nupload: 50\ntotal: 1000\nexpire: 1700000000\nproxies:\n  - name: a\n";
        let u = extract_subscription_usage(raw);
        assert_eq!(u.download, Some(100));
        assert_eq!(u.upload, Some(50));
        assert_eq!(u.total, Some(1000));
        // expire given in seconds -> normalized to ms
        assert_eq!(u.expire, Some(1_700_000_000_000));
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
        assert!(parse_subscription(yaml).is_empty(), "port 0 must drop the node");
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
}

