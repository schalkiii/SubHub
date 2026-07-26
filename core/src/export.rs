use crate::model::{Proxy, ProxyType};
use base64::Engine;
use serde_json::json;
use serde_yaml::{Mapping, Value};

/// Export proxies in the requested format.
/// Supported: clash-meta (default), clash, v2ray, sing-box, surge, base64.
///
/// Invalid nodes are dropped before serialization (see [`export_filter`]):
/// nodes missing type-specific required fields, and nodes that were
/// speed-tested and found unavailable. This keeps the exported subscription
/// clean so a client's strict profile checker never chokes on a broken entry.
pub fn export_str(proxies: &[Proxy], format: &str) -> String {
    let valid = export_filter(proxies);
    match format {
        "v2ray" => to_v2ray_json(&valid),
        "sing-box" | "singbox" => to_singbox_json(&valid),
        "surge" => to_surge(&valid),
        "clash" => to_clash_meta(&valid),
        "base64" => {
            // base64 of the *clash-meta* YAML (not a URI list). Clients that
            // expect v2rayN-style base64 URI lists should use `v2ray` instead.
            base64::engine::general_purpose::STANDARD.encode(to_clash_meta(&valid).as_bytes())
        }
        _ => to_clash_meta(&valid), // clash-meta
    }
}

/// Select only the nodes that are safe to export: exportable (all required
/// type-specific fields present) AND not a confirmed-dead node
/// (`available == Some(false)`). Untested nodes (`available == None`) are kept.
pub fn export_filter(proxies: &[Proxy]) -> Vec<Proxy> {
    let valid: Vec<Proxy> = proxies.iter().filter(|p| p.is_usable()).cloned().collect();
    let removed = proxies.len() - valid.len();
    if removed > 0 {
        eprintln!("导出时去除 {removed} 个无效节点（缺必填字段或测速不可用）");
    }
    valid
}

// ===================== clash-meta / clash yaml =====================

/// Export proxies to a clash-meta / mihomo compatible YAML subscription.
/// Nodes missing type-specific required fields (e.g. SS without password,
/// Trojan without password) are silently **skipped** so that Clash's strict
/// profile checker never rejects the output.
pub fn to_clash_meta(proxies: &[Proxy]) -> String {
    let (list, skipped) = proxies.iter().fold(
        (Vec::new(), 0usize),
        |(mut ok, mut skip), p| {
            if p.is_exportable() {
                ok.push(to_clash_value(p));
                (ok, skip)
            } else {
                skip += 1;
                eprintln!(
                    "导出时跳过不完整节点: {} [{}:{}], 类型={} 缺少必填字段",
                    p.name, p.server, p.port, p.type_.as_str()
                );
                (ok, skip)
            }
        },
    );
    if skipped > 0 {
        eprintln!("共跳过 {skipped} 个不完整节点（clash 校验会拒绝缺必填字段的条目）");
    }
    let mut root = Mapping::new();
    root.insert(Value::from("proxies"), Value::Sequence(list));
    serde_yaml::to_string(&Value::Mapping(root)).unwrap_or_default()
}

fn to_clash_value(p: &Proxy) -> Value {
    let mut m = Mapping::new();
    m.insert(Value::from("name"), Value::from(p.name.clone()));
    m.insert(Value::from("type"), Value::from(p.type_.as_str()));
    m.insert(Value::from("server"), Value::from(p.server.clone()));
    m.insert(Value::from("port"), Value::from(p.port));

    match p.type_ {
        ProxyType::Ss => {
            insert_opt(&mut m, "cipher", &p.method);
            insert_opt(&mut m, "password", &p.password);
        }
        ProxyType::Trojan => {
            insert_opt(&mut m, "password", &p.password);
            insert_opt(&mut m, "sni", &p.sni);
            insert_opt(&mut m, "flow", &p.flow);
            m.insert(Value::from("tls"), Value::from(true));
            insert_opt_bool(&mut m, "skip-cert-verify", p.skip_cert_verify);
        }
        ProxyType::Vmess => {
            insert_opt(&mut m, "uuid", &p.uuid);
            m.insert(Value::from("alterId"), Value::from(p.alter_id.unwrap_or(0)));
            insert_opt(&mut m, "cipher", &p.cipher);
            insert_opt(&mut m, "network", &p.network);
            insert_opt(&mut m, "sni", &p.sni);
            insert_transport_opts(&mut m, p);
            m.insert(Value::from("tls"), Value::from(p.tls.unwrap_or(false)));
            insert_opt_bool(&mut m, "skip-cert-verify", p.skip_cert_verify);
        }
        ProxyType::Vless => {
            insert_opt(&mut m, "uuid", &p.uuid);
            insert_opt(&mut m, "flow", &p.flow);
            insert_opt(&mut m, "network", &p.network);
            insert_opt(&mut m, "sni", &p.sni);
            insert_opt(&mut m, "client-fingerprint", &p.fingerprint);
            insert_transport_opts(&mut m, p);
            m.insert(Value::from("tls"), Value::from(p.tls.unwrap_or(false)));
        }
        ProxyType::Hysteria2 => {
            insert_opt(&mut m, "password", &p.password);
            insert_opt(&mut m, "sni", &p.sni);
            insert_opt_bool(&mut m, "skip-cert-verify", p.skip_cert_verify);
        }
        ProxyType::Tuic => {
            insert_opt(&mut m, "uuid", &p.uuid);
            insert_opt(&mut m, "password", &p.password);
            insert_opt(&mut m, "sni", &p.sni);
        }
        ProxyType::Socks5 | ProxyType::Http => {
            if let Some(extra) = &p.extra {
                if let Some(u) = extra.get("user").and_then(|x| x.as_str()) {
                    m.insert(Value::from("username"), Value::from(u.to_string()));
                }
            }
            insert_opt(&mut m, "password", &p.password);
        }
        _ => {}
    }
    Value::Mapping(m)
}

fn insert_opt(m: &mut Mapping, key: &str, val: &Option<String>) {
    if let Some(v) = val {
        if v.is_empty() {
            return;
        }
        let _ = m.insert(Value::from(key), Value::from(v.clone()));
    }
}

/// Emit nested `ws-opts` / `grpc-opts` only when there is a non-empty
/// path/host/service-name, matching clash-meta's expected structure.
fn insert_transport_opts(m: &mut Mapping, p: &Proxy) {
    let net = p.network.as_deref().unwrap_or("");
    let has_path = p.path.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
    let has_host = p.host.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
    let has_svc = p.service_name.as_deref().map(|s| !s.is_empty()).unwrap_or(false);

    if net == "ws" {
        if has_path || has_host {
            let mut ws = Mapping::new();
            insert_opt(&mut ws, "path", &p.path);
            if has_host {
                let mut headers = Mapping::new();
                headers.insert(Value::from("Host"), Value::from(p.host.clone().unwrap()));
                ws.insert(Value::from("headers"), Value::Mapping(headers));
            }
            m.insert(Value::from("ws-opts"), Value::Mapping(ws));
        }
    } else if net == "grpc" && has_svc {
        let mut grpc = Mapping::new();
        grpc.insert(
            Value::from("grpc-service-name"),
            Value::from(p.service_name.clone().unwrap()),
        );
        m.insert(Value::from("grpc-opts"), Value::Mapping(grpc));
    }
}

fn insert_opt_bool(m: &mut Mapping, key: &str, val: Option<bool>) {
    if let Some(v) = val {
        let _ = m.insert(Value::from(key), Value::from(v));
    }
}

// ===================== v2ray (v2rayN outbounds array) =====================

pub fn to_v2ray_json(proxies: &[Proxy]) -> String {
    // filter_map drops the protocols v2ray core cannot represent as outbounds
    // (hysteria2 / tuic / socks5 / http / wireguard) — emitting a `freedom`
    // (direct) outbound for them would silently break connectivity, so we
    // skip them instead.
    let arr: Vec<serde_json::Value> = proxies.iter().filter_map(v2ray_outbound).collect();
    serde_json::to_string_pretty(&json!(arr)).unwrap_or_default()
}

fn v2ray_stream(p: &Proxy) -> serde_json::Value {
    let net = p.network.clone().unwrap_or_else(|| "tcp".to_string());
    let mut s = json!({ "network": net });
    if p.tls.unwrap_or(false) {
        s["security"] = json!("tls");
        let mut tls = json!({ "allowInsecure": p.skip_cert_verify.unwrap_or(false) });
        if let Some(sni) = &p.sni {
            tls["serverName"] = json!(sni);
        }
        s["tlsSettings"] = tls;
    } else {
        s["security"] = json!("none");
    }
    match net.as_str() {
        "ws" => {
            let mut ws = json!({});
            if let Some(path) = &p.path {
                ws["path"] = json!(path);
            }
            if let Some(host) = &p.host {
                ws["headers"] = json!({ "Host": host });
            }
            s["wsSettings"] = ws;
        }
        "grpc" => {
            if let Some(svc) = &p.service_name {
                s["grpcSettings"] = json!({ "serviceName": svc, "multiMode": false });
            }
        }
        _ => {}
    }
    s
}

fn v2ray_outbound(p: &Proxy) -> Option<serde_json::Value> {
    let v = match p.type_ {
        ProxyType::Vmess => json!({
            "tag": p.name,
            "protocol": "vmess",
            "settings": { "vnext": [ {
                "address": p.server, "port": p.port,
                "users": [ { "id": p.uuid.clone().unwrap_or_default(), "alterId": p.alter_id.unwrap_or(0), "security": p.cipher.clone().unwrap_or_else(|| "auto".into()) } ]
            } ] },
            "streamSettings": v2ray_stream(p)
        }),
        ProxyType::Vless => json!({
            "tag": p.name,
            "protocol": "vless",
            "settings": { "vnext": [ {
                "address": p.server, "port": p.port,
                "users": [ { "id": p.uuid.clone().unwrap_or_default(), "flow": p.flow.clone().unwrap_or_default(), "encryption": "none" } ]
            } ] },
            "streamSettings": v2ray_stream(p)
        }),
        ProxyType::Trojan => json!({
            "tag": p.name,
            "protocol": "trojan",
            "settings": { "servers": [ {
                "address": p.server, "port": p.port,
                "password": p.password.clone().unwrap_or_default(),
                "flow": p.flow.clone().unwrap_or_default()
            } ] },
            "streamSettings": v2ray_stream(p)
        }),
        ProxyType::Ss => json!({
            "tag": p.name,
            "protocol": "shadowsocks",
            "settings": { "servers": [ {
                "address": p.server, "port": p.port,
                "method": p.method.clone().unwrap_or_else(|| "aes-256-gcm".into()),
                "password": p.password.clone().unwrap_or_default()
            } ] }
        }),
        // V2Ray core cannot represent these protocols as outbounds — skip
        // rather than emit a `freedom` (direct) outbound that would break the
        // node's connectivity.
        _ => return None,
    };
    Some(v)
}

// ===================== sing-box outbounds =====================

pub fn to_singbox_json(proxies: &[Proxy]) -> String {
    let arr: Vec<serde_json::Value> = proxies.iter().filter_map(singbox_outbound).collect();
    serde_json::to_string_pretty(&json!({ "outbounds": arr })).unwrap_or_default()
}

fn singbox_transport(p: &Proxy) -> Option<serde_json::Value> {
    let net = p.network.clone().unwrap_or_else(|| "tcp".to_string());
    match net.as_str() {
        "ws" => {
            let mut t = json!({ "type": "ws" });
            if let Some(path) = &p.path {
                t["path"] = json!(path);
            }
            if let Some(host) = &p.host {
                t["headers"] = json!({ "Host": host });
            }
            if let Some(sni) = &p.sni {
                t["server_name"] = json!(sni);
            }
            Some(t)
        }
        "grpc" => p.service_name.as_ref().map(|svc| {
            let mut t = json!({ "type": "grpc", "service_name": svc });
            if let Some(sni) = &p.sni {
                t["server_name"] = json!(sni);
            }
            t
        }),
        _ => None,
    }
}

fn singbox_tls(p: &Proxy) -> Option<serde_json::Value> {
    if p.tls.unwrap_or(false) {
        let mut t = json!({ "enabled": true, "insecure": p.skip_cert_verify.unwrap_or(false) });
        if let Some(sni) = &p.sni {
            t["server_name"] = json!(sni);
        }
        if let Some(f) = &p.fingerprint {
            t["utls_fingerprint"] = json!(f);
        }
        Some(t)
    } else {
        None
    }
}

fn singbox_outbound(p: &Proxy) -> Option<serde_json::Value> {
    let mut o = json!({ "tag": p.name });
    match p.type_ {
        ProxyType::Vmess => {
            o["type"] = json!("vmess");
            o["server"] = json!(p.server);
            o["server_port"] = json!(p.port);
            o["uuid"] = json!(p.uuid.clone().unwrap_or_default());
            o["security"] = json!(p.cipher.clone().unwrap_or_else(|| "auto".into()));
            o["alter_id"] = json!(p.alter_id.unwrap_or(0));
            if let Some(t) = singbox_transport(p) {
                o["transport"] = t;
            }
            if let Some(tls) = singbox_tls(p) {
                o["tls"] = tls;
            }
            Some(o)
        }
        ProxyType::Vless => {
            o["type"] = json!("vless");
            o["server"] = json!(p.server);
            o["server_port"] = json!(p.port);
            o["uuid"] = json!(p.uuid.clone().unwrap_or_default());
            if let Some(flow) = &p.flow {
                o["flow"] = json!(flow);
            }
            o["packet_encoding"] = json!("xudp");
            if let Some(t) = singbox_transport(p) {
                o["transport"] = t;
            }
            if let Some(tls) = singbox_tls(p) {
                o["tls"] = tls;
            }
            Some(o)
        }
        ProxyType::Trojan => {
            o["type"] = json!("trojan");
            o["server"] = json!(p.server);
            o["server_port"] = json!(p.port);
            o["password"] = json!(p.password.clone().unwrap_or_default());
            if let Some(tls) = singbox_tls(p) {
                o["tls"] = tls;
            }
            Some(o)
        }
        ProxyType::Ss => {
            o["type"] = json!("shadowsocks");
            o["server"] = json!(p.server);
            o["server_port"] = json!(p.port);
            o["method"] = json!(p.method.clone().unwrap_or_else(|| "aes-256-gcm".into()));
            o["password"] = json!(p.password.clone().unwrap_or_default());
            Some(o)
        }
        ProxyType::Hysteria2 => {
            o["type"] = json!("hysteria2");
            o["server"] = json!(p.server);
            o["server_port"] = json!(p.port);
            o["password"] = json!(p.password.clone().unwrap_or_default());
            if let Some(tls) = singbox_tls(p) {
                o["tls"] = tls;
            }
            Some(o)
        }
        ProxyType::Tuic => {
            o["type"] = json!("tuic");
            o["server"] = json!(p.server);
            o["server_port"] = json!(p.port);
            o["uuid"] = json!(p.uuid.clone().unwrap_or_default());
            o["password"] = json!(p.password.clone().unwrap_or_default());
            if let Some(tls) = singbox_tls(p) {
                o["tls"] = tls;
            }
            Some(o)
        }
        ProxyType::Socks5 => {
            o["type"] = json!("socks");
            o["server"] = json!(p.server);
            o["server_port"] = json!(p.port);
            o["version"] = json!("5");
            if let Some(pw) = &p.password {
                o["password"] = json!(pw);
            }
            Some(o)
        }
        ProxyType::Http => {
            o["type"] = json!("http");
            o["server"] = json!(p.server);
            o["server_port"] = json!(p.port);
            if let Some(pw) = &p.password {
                o["password"] = json!(pw);
            }
            Some(o)
        }
        ProxyType::Wireguard => {
            eprintln!(
                "[export] sing-box 不支持 wireguard 节点「{}」，已跳过",
                p.name
            );
            None
        }
        _ => {
            eprintln!(
                "[export] sing-box 不支持的节点类型 {:?}「{}」，已跳过",
                p.type_,
                p.name
            );
            None
        }
    }
}

// ===================== surge =====================

pub fn to_surge(proxies: &[Proxy]) -> String {
    let mut lines = Vec::new();
    for p in proxies {
        let line = match p.type_ {
            ProxyType::Ss => format!(
                "{} = ss, {}, {}, encrypt-method={}, password={}",
                p.name,
                p.server,
                p.port,
                p.method.clone().unwrap_or_else(|| "aes-256-gcm".into()),
                p.password.clone().unwrap_or_default()
            ),
            ProxyType::Trojan => {
                let mut s = format!(
                    "{} = trojan, {}, {}, password={}",
                    p.name,
                    p.server,
                    p.port,
                    p.password.clone().unwrap_or_default()
                );
                if let Some(sni) = &p.sni {
                    s.push_str(&format!(", sni={sni}"));
                }
                if p.tls.unwrap_or(false) {
                    s.push_str(", tls=true");
                }
                if p.skip_cert_verify.unwrap_or(false) {
                    s.push_str(", skip-cert-verify=true");
                }
                s
            }
            ProxyType::Vmess => {
                let mut s = format!(
                    "{} = vmess, {}, {}, username={}, network={}",
                    p.name,
                    p.server,
                    p.port,
                    p.uuid.clone().unwrap_or_default(),
                    p.network.clone().unwrap_or_else(|| "tcp".into())
                );
                if p.tls.unwrap_or(false) {
                    s.push_str(", tls=true");
                }
                if let Some(sni) = &p.sni {
                    s.push_str(&format!(", sni={sni}"));
                }
                if let Some(path) = &p.path {
                    s.push_str(&format!(", ws-path={path}"));
                }
                if let Some(host) = &p.host {
                    s.push_str(&format!(", ws-headers=Host:{host}"));
                }
                s
            }
            ProxyType::Vless => {
                let mut s = format!(
                    "{} = vless, {}, {}, username={}",
                    p.name,
                    p.server,
                    p.port,
                    p.uuid.clone().unwrap_or_default()
                );
                if let Some(flow) = &p.flow {
                    s.push_str(&format!(", flow={flow}"));
                }
                if p.tls.unwrap_or(false) {
                    s.push_str(", tls=true");
                }
                if let Some(sni) = &p.sni {
                    s.push_str(&format!(", sni={sni}"));
                }
                s
            }
            _ => continue,
        };
        lines.push(line);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use crate::model::{Proxy, ProxyType};

    fn mk(t: ProxyType, server: &str, port: u16) -> Proxy {
        Proxy::new("node".to_string(), t, server.to_string(), port)
    }

    #[test]
    fn singbox_export_emits_real_types_not_direct() {
        // Non-core node types (hysteria2/tuic/socks5/http) must get real
        // outbounds; previously they were emitted as `"type":"direct"` (broken)
        // or silently dropped. Wireguard is unsupported and must be skipped.
        let mut h2 = mk(ProxyType::Hysteria2, "1.1.1.1", 443);
        h2.password = Some("pw".into());
        let mut tuic = mk(ProxyType::Tuic, "2.2.2.2", 8443);
        tuic.uuid = Some("u".into());
        tuic.password = Some("pw".into());
        let mut socks = mk(ProxyType::Socks5, "3.3.3.3", 1080);
        socks.password = Some("pw".into());
        let mut http = mk(ProxyType::Http, "4.4.4.4", 8080);
        http.password = Some("pw".into());
        let wg = mk(ProxyType::Wireguard, "5.5.5.5", 51820);

        let out = super::to_singbox_json(&[h2, tuic, socks, http, wg]);
        assert!(!out.contains("\"direct\""), "sing-box must not emit direct nodes");
        assert!(out.contains("\"hysteria2\""), "hysteria2 outbound missing");
        assert!(out.contains("\"tuic\""), "tuic outbound missing");
        assert!(out.contains("\"socks\""), "socks outbound missing");
        assert!(out.contains("\"http\""), "http outbound missing");
        // wireguard is unsupported and must be skipped (4 of 5 nodes exported).
        let count = out.matches("\"tag\": \"node\"").count();
        assert_eq!(count, 4, "wireguard should be skipped (4 of 5 nodes exported)");
    }
}
