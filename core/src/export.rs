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
///
/// Proxy names are also guaranteed unique (see [`ensure_unique_names`]) — clash /
/// v2ray / sing-box / surge all key a node by its name, so a duplicate name
/// would make the whole profile fail validation.
pub fn export_str(proxies: &[Proxy], format: &str) -> String {
    let mut valid = export_filter(proxies);
    ensure_unique_names(&mut valid);
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

/// Clash / v2ray / sing-box / surge all key a node by its (unique) name. Two
/// genuinely different nodes — e.g. from different subscriptions, or collapsed
/// to the same name by a rename rule — can share a display name. Exporting such
/// a set makes mihomo / clash-verge reject the profile with
/// `... is the duplicate name`, taking down the whole config. Rewrite names so
/// each is unique: the first occurrence keeps its original name; later
/// collisions get a ` #2`, ` #3` … suffix.
fn ensure_unique_names(proxies: &mut [Proxy]) {
    use std::collections::HashMap;
    let mut seen: HashMap<String, usize> = HashMap::new();
    for p in proxies.iter_mut() {
        let count = seen.entry(p.name.clone()).or_insert(0);
        *count += 1;
        if *count > 1 {
            p.name = format!("{} #{}", p.name, *count);
        }
    }
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
        ProxyType::AnyTls => {
            // AnyTLS authenticates with a password (not a uuid). It is always
            // TLS, but mihomo's `anytls` outbound has *no* top-level `tls:`
            // switch — emitting one would be rejected — so we deliberately
            // omit it. `client-fingerprint` carries the `fp` query param.
            insert_opt(&mut m, "password", &p.password);
            insert_opt(&mut m, "sni", &p.sni);
            insert_opt(&mut m, "client-fingerprint", &p.fingerprint);
            insert_opt_bool(&mut m, "skip-cert-verify", p.skip_cert_verify);
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
    let total = proxies.len();
    let arr: Vec<serde_json::Value> = proxies.iter().filter_map(v2ray_outbound).collect();
    let skipped = total - arr.len();
    if skipped > 0 {
        eprintln!("[subhub] v2ray 导出跳过 {skipped} 个不支持的节点（hysteria2/tuic/socks5/http/wireguard 等 v2ray-core 无法表示）");
    }
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
        // node's connectivity. This includes AnyTLS, which v2ray-core does not
        // support at all.
        ProxyType::AnyTls => return None,
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
            // AnyTLS is unsupported by sing-box as well as v2ray-core.
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

    #[test]
    fn anytls_to_clash_meta_emits_password_no_top_level_tls() {
        // AnyTLS → clash-meta must carry `password`, `sni`, `skip-cert-verify`,
        // `client-fingerprint`, and NO top-level `tls:` key (mihomo rejects one).
        let mut p = mk(ProxyType::AnyTls, "aws.host.xyz", 56147);
        p.password = Some("df28c004-87ca-40fb-ae5a-3b4ce9fb8654".into());
        p.sni = Some("updates.cdn-apple.com".into());
        p.fingerprint = Some("chrome".into());
        p.skip_cert_verify = Some(true);

        let yaml = super::to_clash_meta(&[p]);
        assert!(yaml.contains("type: anytls"), "type must be anytls");
        assert!(yaml.contains("password: df28c004-87ca-40fb-ae5a-3b4ce9fb8654"));
        assert!(yaml.contains("sni: updates.cdn-apple.com"));
        assert!(yaml.contains("skip-cert-verify: true"));
        assert!(yaml.contains("client-fingerprint: chrome"));
        // mihomo's anytls outbound has no tls: switch — must be absent.
        assert!(!yaml.contains("tls:"), "anytls must not emit a top-level tls key");
    }

    #[test]
    fn anytls_skipped_in_v2ray_and_singbox() {
        // v2ray-core / sing-box have no anytls support — exporting must skip
        // (return None), not emit a broken `direct` outbound.
        let mut p = mk(ProxyType::AnyTls, "aws.host.xyz", 56147);
        p.password = Some("pw".into());

        let v2 = super::to_v2ray_json(&[p.clone()]);
        let sb = super::to_singbox_json(&[p]);
        assert_eq!(v2, "[]", "v2ray must skip anytls (empty array)");
        assert!(!sb.contains("\"anytls\""), "sing-box must skip anytls");
        assert!(!sb.contains("\"aws.host.xyz\""), "sing-box must skip anytls node");
    }

    #[test]
    fn duplicate_display_names_are_made_unique_on_export() {
        // 两个不同节点（不同服务器）却共享显示名「US美国」（常见于地区重命名规则
        // 把同地区节点都改成同一名字，或不同订阅里本就重名）。导出若不处理，clash
        // 会报 `... is the duplicate name` 整份配置被拒。修复后：首个保留原名，
        // 后续撞名加 ` #2` 后缀，确保整份订阅可被客户端加载。
        let mut a = mk(ProxyType::Ss, "1.1.1.1", 8388);
        a.name = "US美国".to_string();
        a.method = Some("aes-256-gcm".into());
        a.password = Some("pw1".into());
        let mut b = mk(ProxyType::Ss, "2.2.2.2", 8388);
        b.name = "US美国".to_string();
        b.method = Some("aes-256-gcm".into());
        b.password = Some("pw2".into());

        let yaml = super::export_str(&[a, b], "clash-meta");
        let keep = yaml.matches("name: US美国").count();
        // 撞名节点加 ` #2` 后缀；serde_yaml 会对含 `#` 的名字加引号
        //（`name: 'US美国 #2'`），故用子串包含而非精确前缀匹配。
        let suffixed = yaml.contains("US美国 #2");
        assert_eq!(keep, 1, "恰好一个节点保留原名");
        assert!(suffixed, "撞名节点必须加后缀以保证唯一");
        // 两个节点的服务器地址都应出现在产物中（都保留了，只是名字不同）。
        assert!(yaml.contains("1.1.1.1"), "原节点 A 必须保留");
        assert!(yaml.contains("2.2.2.2"), "原节点 B 必须保留");
    }

    #[test]
    fn vmess_export_emits_required_protocol_fields() {
        // mihomo / clash-verge 的 VMess 节点必须带齐 uuid / alterId / cipher / tls
        // 等字段才能完成真实协议握手。导出链路（to_clash_meta）与 SubHub 引擎测速
        // 用的 build_engine_config 是同一份序列化产物，故导出 fidelity 是单一真相源
        // —— 节点在 SubHub 引擎侧能通、在 clash-verge 侧不通，差异来自两侧使用的
        // mihomo 核心二进制/版本，而非导出缺字段。这里锁定关键字段防回归。
        let mut p = mk(ProxyType::Vmess, "tw.example.com", 443);
        p.uuid = Some("uuid-1234".into());
        p.alter_id = Some(0);
        p.cipher = Some("auto".into());
        p.tls = Some(true);
        p.sni = Some("tw.example.com".into());
        let yaml = super::to_clash_meta(&[p]);
        assert!(yaml.contains("type: vmess"), "type must be vmess");
        assert!(yaml.contains("uuid: uuid-1234"));
        assert!(yaml.contains("alterId: 0"));
        assert!(yaml.contains("cipher: auto"));
        assert!(yaml.contains("tls: true"), "vmess tls 必须导出");
        assert!(yaml.contains("sni: tw.example.com"));
    }

    #[test]
    fn trojan_export_emits_tls_true_and_sni() {
        // Trojan 导出必须带 tls: true（to_clash_meta 对 Trojan 强制 tls），否则
        // clash-verge 侧协议握手失败。锁定该行为。
        let mut p = mk(ProxyType::Trojan, "tw2.example.com", 443);
        p.password = Some("pw".into());
        p.sni = Some("tw2.example.com".into());
        let yaml = super::to_clash_meta(&[p]);
        assert!(yaml.contains("type: trojan"));
        assert!(yaml.contains("password: pw"));
        assert!(yaml.contains("tls: true"), "trojan 导出必须带 tls: true");
        assert!(yaml.contains("sni: tw2.example.com"));
    }
}
