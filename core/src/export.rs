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
    use std::collections::HashSet;
    let mut used: HashSet<String> = HashSet::new();
    for p in proxies.iter_mut() {
        if used.insert(p.name.clone()) {
            continue;
        }
        // The counter must also claim the generated name: a later node whose
        // original name already equals "A #2" would otherwise collide with the
        // suffixed copy of an earlier "A".
        let base = p.name.clone();
        let mut i = 2;
        loop {
            let candidate = format!("{base} #{i}");
            if used.insert(candidate.clone()) {
                p.name = candidate;
                break;
            }
            i += 1;
        }
    }
}

// ===================== clash-meta / clash yaml =====================

/// Export proxies to a clash-meta / mihomo compatible YAML subscription.
/// Nodes missing type-specific required fields (e.g. SS without password,
/// Trojan without password) are silently **skipped** so that Clash's strict
/// profile checker never rejects the output.
pub fn to_clash_meta(proxies: &[Proxy]) -> String {
    let (list, skipped) = proxies
        .iter()
        .fold((Vec::new(), 0usize), |(mut ok, mut skip), p| {
            if p.is_exportable() {
                ok.push(to_clash_value(p));
                (ok, skip)
            } else {
                skip += 1;
                eprintln!(
                    "导出时跳过不完整节点: {} [{}:{}], 类型={} 缺少必填字段",
                    p.name,
                    p.server,
                    p.port,
                    p.type_.as_str()
                );
                (ok, skip)
            }
        });
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
            // SS 的加密方法可能存于 `cipher`（clash-meta YAML 订阅）或 `method`
            // （SIP002 URI 解析）任一字段，二者取非空者作为 clash-meta 的 `cipher`。
            // 旧代码只读 `method`，导致从 YAML 订阅导入的 SS 节点缺失 cipher →
            // mihomo 解析失败 → 引擎测速拿不到真实带宽（显示 0.0MB/s 的 TCP 估计值）。
            let ss_cipher = p.cipher.clone().or_else(|| p.method.clone());
            insert_opt(&mut m, "cipher", &ss_cipher);
            insert_opt(&mut m, "password", &p.password);
            if let Some((name, opts)) = clash_plugin(p) {
                m.insert(Value::from("plugin"), Value::from(name));
                if !opts.is_empty() {
                    m.insert(Value::from("plugin-opts"), Value::Mapping(opts));
                }
            }
        }
        ProxyType::Trojan => {
            insert_opt(&mut m, "password", &p.password);
            insert_opt(&mut m, "sni", &p.sni);
            insert_opt(&mut m, "flow", &p.flow);
            insert_opt(&mut m, "client-fingerprint", &p.fingerprint);
            m.insert(Value::from("tls"), Value::from(true));
            insert_opt_bool(&mut m, "skip-cert-verify", p.skip_cert_verify);
        }
        ProxyType::Vmess => {
            insert_opt(&mut m, "uuid", &p.uuid);
            m.insert(Value::from("alterId"), Value::from(p.alter_id.unwrap_or(0)));
            insert_opt(&mut m, "cipher", &p.cipher);
            insert_opt(&mut m, "network", &p.network);
            insert_opt(&mut m, "sni", &p.sni);
            insert_opt(&mut m, "client-fingerprint", &p.fingerprint);
            insert_transport_opts(&mut m, p);
            m.insert(Value::from("tls"), Value::from(p.tls.unwrap_or(false)));
            insert_opt_bool(&mut m, "skip-cert-verify", p.skip_cert_verify);
        }
        ProxyType::Vless => {
            insert_opt(&mut m, "uuid", &p.uuid);
            insert_opt(&mut m, "flow", &p.flow);
            insert_opt(&mut m, "network", &p.network);
            insert_opt(&mut m, "sni", &p.sni);
            // REALITY 必须带 client-fingerprint；链接未给 fp 时补 chrome，
            // 否则部分 mihomo 版本会拒绝该 outbound 或握手失败。
            let fp = if p.reality {
                Some(
                    p.fingerprint
                        .clone()
                        .unwrap_or_else(|| "chrome".to_string()),
                )
            } else {
                p.fingerprint.clone()
            };
            insert_opt(&mut m, "client-fingerprint", &fp);
            insert_opt_bool(&mut m, "skip-cert-verify", p.skip_cert_verify);
            insert_transport_opts(&mut m, p);
            m.insert(Value::from("tls"), Value::from(p.tls.unwrap_or(false)));
            if p.reality {
                let mut ro = Mapping::new();
                insert_opt(&mut ro, "public-key", &p.reality_public_key);
                insert_opt(&mut ro, "short-id", &p.reality_short_id);
                m.insert(Value::from("reality-opts"), Value::Mapping(ro));
            }
        }
        ProxyType::Hysteria2 => {
            insert_opt(&mut m, "password", &p.password);
            insert_opt(&mut m, "sni", &p.sni);
            insert_opt(&mut m, "obfs", &p.obfs);
            insert_opt(&mut m, "obfs-password", &p.obfs_password);
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
            // HTTPS 代理必须输出 `tls: true`，否则 mihomo 用明文 HTTP CONNECT
            // 去连 443 端口，握手必失败、节点被误判为不可用。
            //
            // 兜底：`https://` 链接在部分订阅里被下发成 `type: http` 且不带
            // tls 字段（解析侧也无从得知），此时按端口推断 —— 明文 HTTP 代理
            // 跑在 443 上极为罕见，而实测 24 个 443 端口 http 节点明文通过 0
            // 个、强制 tls 后通过 3 个，故 443 一律按 TLS 处理。
            let tls = p
                .tls
                .or(if p.port == 443 { Some(true) } else { None });
            insert_opt_bool(&mut m, "tls", tls);
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
    let has_svc = p
        .service_name
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);

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
    } else if (net == "h2" || net == "http") && (has_path || has_host) {
        // mihomo 的 HTTP/2 传输：`h2-opts.host` 是列表，`path` 是字符串。
        let mut h2 = Mapping::new();
        insert_opt(&mut h2, "path", &p.path);
        if has_host {
            h2.insert(
                Value::from("host"),
                Value::Sequence(vec![Value::from(p.host.clone().unwrap())]),
            );
        }
        m.insert(Value::from("h2-opts"), Value::Mapping(h2));
    }
}

fn insert_opt_bool(m: &mut Mapping, key: &str, val: Option<bool>) {
    if let Some(v) = val {
        let _ = m.insert(Value::from(key), Value::from(v));
    }
}

/// SS 的加密方法可能存于 `method`（SIP002 URI）或 `cipher`（clash-meta YAML）
/// 任一字段。所有导出格式都必须走这里取同一个值，否则 YAML 导入的 SS 节点会被
/// 导出成默认的 `aes-256-gcm`，客户端握手必然失败。
fn ss_method(p: &Proxy) -> String {
    p.method
        .clone()
        .or_else(|| p.cipher.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "aes-256-gcm".to_string())
}

/// Split SIP002-style `k=v;k2=v2` plugin options. A bare flag (`tls`) means
/// `true`, matching v2ray-plugin / obfs option syntax.
fn parse_plugin_opts(opts: Option<&str>) -> Vec<(String, String)> {
    opts.unwrap_or("")
        .split(';')
        .filter_map(|kv| {
            let kv = kv.trim();
            if kv.is_empty() {
                return None;
            }
            Some(match kv.split_once('=') {
                Some((k, v)) => (k.trim().to_string(), v.trim().to_string()),
                None => (kv.to_string(), "true".to_string()),
            })
        })
        .collect()
}

fn ss_plugin(p: &Proxy) -> Option<(String, Vec<(String, String)>)> {
    let name = p.plugin.as_deref().filter(|s| !s.is_empty())?.to_string();
    Some((name, parse_plugin_opts(p.plugin_opts.as_deref())))
}

/// Clash plugin name (`obfs` / `v2ray-plugin`) plus its option map. SIP002
/// option names (`obfs`, `obfs-host`) are translated to clash's (`mode`,
/// `host`); boolean-looking values stay YAML booleans.
fn clash_plugin(p: &Proxy) -> Option<(String, Mapping)> {
    let (name, opts) = ss_plugin(p)?;
    let lower = name.to_ascii_lowercase();
    let mut map = Mapping::new();
    if lower.contains("obfs") {
        for (k, v) in opts {
            let key = match k.as_str() {
                "obfs" => "mode",
                "obfs-host" => "host",
                other => other,
            };
            map.insert(Value::from(key), Value::from(v));
        }
        Some(("obfs".to_string(), map))
    } else if lower.contains("v2ray-plugin") {
        for (k, v) in opts {
            if k == "tls" || k == "mux" {
                let b = v == "1" || v.eq_ignore_ascii_case("true");
                map.insert(Value::from(k), Value::from(b));
            } else {
                map.insert(Value::from(k), Value::from(v));
            }
        }
        Some(("v2ray-plugin".to_string(), map))
    } else {
        for (k, v) in opts {
            map.insert(Value::from(k), Value::from(v));
        }
        Some((name, map))
    }
}

/// sing-box plugin name + options. sing-box uses `obfs-local` and the original
/// SIP002 `obfs`/`obfs-host` option names.
fn singbox_plugin(p: &Proxy) -> Option<(String, Option<String>)> {
    let (name, opts) = ss_plugin(p)?;
    let lower = name.to_ascii_lowercase();
    if lower.contains("obfs") {
        let joined = opts
            .into_iter()
            .map(|(k, v)| {
                let key = match k.as_str() {
                    "mode" => "obfs",
                    "host" => "obfs-host",
                    other => other,
                };
                format!("{key}={v}")
            })
            .collect::<Vec<_>>()
            .join(";");
        Some((
            "obfs-local".to_string(),
            (!joined.is_empty()).then_some(joined),
        ))
    } else if lower.contains("v2ray-plugin") {
        let joined = opts
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(";");
        Some((
            "v2ray-plugin".to_string(),
            (!joined.is_empty()).then_some(joined),
        ))
    } else {
        let joined = opts
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(";");
        Some((name, (!joined.is_empty()).then_some(joined)))
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
    if p.reality {
        // xray（v2rayN 内核）用 `security: reality` + `realitySettings`。
        s["security"] = json!("reality");
        let mut reality = json!({
            "fingerprint": p.fingerprint.clone().unwrap_or_else(|| "chrome".to_string()),
            "allowInsecure": p.skip_cert_verify.unwrap_or(false),
        });
        if let Some(sni) = &p.sni {
            reality["serverName"] = json!(sni);
        }
        if let Some(pk) = &p.reality_public_key {
            reality["publicKey"] = json!(pk);
        }
        if let Some(sid) = &p.reality_short_id {
            reality["shortId"] = json!(sid);
        }
        s["realitySettings"] = reality;
    } else if p.tls.unwrap_or(false) {
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
        "h2" | "http" => {
            let mut h2 = json!({});
            if let Some(path) = &p.path {
                h2["path"] = json!(path);
            }
            if let Some(host) = &p.host {
                h2["host"] = json!([host]);
            }
            s["httpSettings"] = h2;
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
        ProxyType::Ss => {
            // v2ray-core 的 shadowsocks outbound 不支持 obfs/v2ray-plugin；
            // 静默丢掉插件会得到一段看似正常但必然连不上的配置，直接跳过并提示。
            if p.plugin.as_deref().is_some_and(|s| !s.is_empty()) {
                eprintln!(
                    "[export] v2ray 不支持 SS 插件节点「{}」，已跳过",
                    p.name
                );
                return None;
            }
            json!({
                "tag": p.name,
                "protocol": "shadowsocks",
                "settings": { "servers": [ {
                    "address": p.server, "port": p.port,
                    "method": ss_method(p),
                    "password": p.password.clone().unwrap_or_default()
                } ] }
            })
        }
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
        "h2" | "http" => {
            // sing-box 把 HTTP/2 传输命名为 `http`，host 是数组。
            let mut t = json!({ "type": "http" });
            if let Some(path) = &p.path {
                t["path"] = json!(path);
            }
            if let Some(host) = &p.host {
                t["host"] = json!([host]);
            }
            if let Some(sni) = &p.sni {
                t["server_name"] = json!(sni);
            }
            Some(t)
        }
        _ => None,
    }
}

fn singbox_tls(p: &Proxy) -> Option<serde_json::Value> {
    singbox_tls_impl(p, p.tls.unwrap_or(false))
}

/// Hysteria2 / TUIC 固定使用 QUIC-over-TLS，即使节点没有显式 `tls` 标记也
/// 必须输出 TLS 块，否则 sing-box 配置校验失败、整份订阅不可用。
fn singbox_tls_forced(p: &Proxy) -> serde_json::Value {
    singbox_tls_impl(p, true).unwrap_or_else(|| json!({ "enabled": true }))
}

fn singbox_tls_impl(p: &Proxy, enabled: bool) -> Option<serde_json::Value> {
    if !enabled {
        return None;
    }
    let mut t = json!({ "enabled": true, "insecure": p.skip_cert_verify.unwrap_or(false) });
    if let Some(sni) = &p.sni {
        t["server_name"] = json!(sni);
    }
    if p.reality {
        let mut reality = json!({ "enabled": true });
        if let Some(pk) = &p.reality_public_key {
            reality["public_key"] = json!(pk);
        }
        if let Some(sid) = &p.reality_short_id {
            reality["short_id"] = json!(sid);
        }
        t["reality"] = reality;
        // REALITY 依赖 uTLS，缺省用 chrome（与 mihomo 侧保持一致）。
        t["utls"] = json!({
            "enabled": true,
            "fingerprint": p.fingerprint.clone().unwrap_or_else(|| "chrome".to_string()),
        });
    } else if let Some(f) = &p.fingerprint {
        // sing-box 期望的是 `utls: { enabled, fingerprint }` 对象，
        // 不是平铺的 `utls_fingerprint` 字段。
        t["utls"] = json!({ "enabled": true, "fingerprint": f });
    }
    Some(t)
}

fn insert_singbox_user(o: &mut serde_json::Value, p: &Proxy) {
    if let Some(user) = p
        .extra
        .as_ref()
        .and_then(|e| e.get("user"))
        .and_then(|x| x.as_str())
    {
        o["username"] = json!(user);
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
            o["method"] = json!(ss_method(p));
            o["password"] = json!(p.password.clone().unwrap_or_default());
            if let Some((name, opts)) = singbox_plugin(p) {
                o["plugin"] = json!(name);
                if let Some(opts) = opts {
                    o["plugin_opts"] = json!(opts);
                }
            }
            Some(o)
        }
        ProxyType::Hysteria2 => {
            o["type"] = json!("hysteria2");
            o["server"] = json!(p.server);
            o["server_port"] = json!(p.port);
            o["password"] = json!(p.password.clone().unwrap_or_default());
            if let Some(obfs) = &p.obfs {
                o["obfs"] = json!({
                    "type": obfs,
                    "password": p.obfs_password.clone().unwrap_or_default(),
                });
            }
            o["tls"] = singbox_tls_forced(p);
            Some(o)
        }
        ProxyType::Tuic => {
            o["type"] = json!("tuic");
            o["server"] = json!(p.server);
            o["server_port"] = json!(p.port);
            o["uuid"] = json!(p.uuid.clone().unwrap_or_default());
            o["password"] = json!(p.password.clone().unwrap_or_default());
            o["tls"] = singbox_tls_forced(p);
            Some(o)
        }
        ProxyType::Socks5 => {
            o["type"] = json!("socks");
            o["server"] = json!(p.server);
            o["server_port"] = json!(p.port);
            o["version"] = json!("5");
            insert_singbox_user(&mut o, p);
            if let Some(pw) = &p.password {
                o["password"] = json!(pw);
            }
            Some(o)
        }
        ProxyType::Http => {
            o["type"] = json!("http");
            o["server"] = json!(p.server);
            o["server_port"] = json!(p.port);
            insert_singbox_user(&mut o, p);
            if let Some(pw) = &p.password {
                o["password"] = json!(pw);
            }
            // 与 clash 导出一致：https:// 链接或 443 端口按 TLS 处理，
            // 否则 sing-box 会用明文去连 443，握手必失败。
            if p.tls.unwrap_or(false) || p.port == 443 {
                o["tls"] = singbox_tls_forced(p);
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
                p.type_, p.name
            );
            None
        }
    }
}

// ===================== surge =====================

/// Surge config is line/comma based: a node name containing a newline, comma
/// or `=` could break the line or inject another proxy entry. Replace those
/// characters before writing.
fn surge_safe(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\r' | '\n' | ',' | '=' => '_',
            c => c,
        })
        .collect()
}

pub fn to_surge(proxies: &[Proxy]) -> String {
    let mut lines = Vec::new();
    for p in proxies {
        let name = surge_safe(&p.name);
        let line = match p.type_ {
            ProxyType::Ss => {
                let mut s = format!(
                    "{} = ss, {}, {}, encrypt-method={}, password={}",
                    name,
                    p.server,
                    p.port,
                    ss_method(p),
                    p.password.clone().unwrap_or_default()
                );
                if let Some((plugin_name, opts)) = ss_plugin(p) {
                    let lower = plugin_name.to_ascii_lowercase();
                    if lower.contains("obfs") {
                        if let Some((_, mode)) = opts
                            .iter()
                            .find(|(k, _)| k == "obfs" || k == "mode")
                        {
                            s.push_str(&format!(", obfs={}", surge_safe(mode)));
                        }
                        if let Some((_, host)) = opts
                            .iter()
                            .find(|(k, _)| k == "obfs-host" || k == "host")
                        {
                            s.push_str(&format!(", obfs-host={}", surge_safe(host)));
                        }
                    } else {
                        eprintln!(
                            "[export] surge 不支持 SS 插件节点「{}」，已跳过",
                            p.name
                        );
                        continue;
                    }
                }
                s
            }
            ProxyType::Trojan => {
                let mut s = format!(
                    "{} = trojan, {}, {}, password={}",
                    name,
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
                    name,
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
                if p.network.as_deref() == Some("ws") {
                    s.push_str(", ws=true");
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
                    name,
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
        assert!(
            !out.contains("\"direct\""),
            "sing-box must not emit direct nodes"
        );
        assert!(out.contains("\"hysteria2\""), "hysteria2 outbound missing");
        assert!(out.contains("\"tuic\""), "tuic outbound missing");
        assert!(out.contains("\"socks\""), "socks outbound missing");
        assert!(out.contains("\"http\""), "http outbound missing");
        // wireguard is unsupported and must be skipped (4 of 5 nodes exported).
        let count = out.matches("\"tag\": \"node\"").count();
        assert_eq!(
            count, 4,
            "wireguard should be skipped (4 of 5 nodes exported)"
        );
    }

    #[test]
    fn anytls_to_clash_meta_emits_password_no_top_level_tls() {
        // AnyTLS → clash-meta must carry `password`, `sni`, `skip-cert-verify`,
        // `client-fingerprint`, and NO top-level `tls:` key (mihomo rejects one).
        let mut p = mk(ProxyType::AnyTls, "aws.host.xyz", 56147);
        p.password = Some("00000000-0000-4000-8000-000000000001".into());
        p.sni = Some("updates.cdn-apple.com".into());
        p.fingerprint = Some("chrome".into());
        p.skip_cert_verify = Some(true);

        let yaml = super::to_clash_meta(&[p]);
        assert!(yaml.contains("type: anytls"), "type must be anytls");
        assert!(yaml.contains("password: 00000000-0000-4000-8000-000000000001"));
        assert!(yaml.contains("sni: updates.cdn-apple.com"));
        assert!(yaml.contains("skip-cert-verify: true"));
        assert!(yaml.contains("client-fingerprint: chrome"));
        // mihomo's anytls outbound has no tls: switch — must be absent.
        assert!(
            !yaml.contains("tls:"),
            "anytls must not emit a top-level tls key"
        );
    }

    #[test]
    fn https_proxy_exports_tls_true() {
        // Regression guard for the https-proxy export fix: an Http proxy that
        // carries tls=true must emit `tls: true` in clash-meta, otherwise mihomo
        // would attempt a plaintext HTTP CONNECT to port 443 and fail.
        let mut p = mk(ProxyType::Http, "1.2.3.4", 443);
        p.tls = Some(true);
        let yaml = super::to_clash_meta(&[p]);
        assert!(yaml.contains("type: http"), "type must be http");
        assert!(yaml.contains("tls: true"), "https proxy must emit tls: true");
    }

    #[test]
    fn http_on_443_emits_tls_true_via_port_fallback() {
        // 443-port fallback: a clash subscription may ship `type: http` on port
        // 443 without an explicit tls field. Export must still emit `tls: true`.
        let p = mk(ProxyType::Http, "1.2.3.4", 443); // tls = None
        let yaml = super::to_clash_meta(&[p]);
        assert!(
            yaml.contains("tls: true"),
            "port-443 http must fall back to tls: true"
        );
    }

    #[test]
    fn http_on_non_443_stays_plaintext() {
        // A plaintext http proxy on a non-443 port must NOT get tls: true.
        let p = mk(ProxyType::Http, "1.2.3.4", 8080); // tls = None
        let yaml = super::to_clash_meta(&[p]);
        assert!(
            !yaml.contains("tls: true"),
            "non-443 http must stay plaintext"
        );
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
        assert!(
            !sb.contains("\"aws.host.xyz\""),
            "sing-box must skip anytls node"
        );
    }

    #[test]
    fn ss_cipher_fallback_used_across_all_formats() {
        // YAML 导入的 SS 节点把加密方法存进 `cipher`，`method` 为 None。
        // v2ray / sing-box / surge 都必须读同一个值，不能回退成默认
        // aes-256-gcm（否则导出的节点握手必失败）。
        let mut p = mk(ProxyType::Ss, "1.1.1.1", 8388);
        p.password = Some("pw".into());
        p.cipher = Some("chacha20-ietf-poly1305".into());

        let v2 = super::to_v2ray_json(std::slice::from_ref(&p));
        assert!(v2.contains("chacha20-ietf-poly1305"), "v2ray 丢失 cipher: {v2}");
        let sb = super::to_singbox_json(std::slice::from_ref(&p));
        assert!(sb.contains("chacha20-ietf-poly1305"), "sing-box 丢失 cipher: {sb}");
        let sg = super::to_surge(std::slice::from_ref(&p));
        assert!(
            sg.contains("encrypt-method=chacha20-ietf-poly1305"),
            "surge 丢失 cipher: {sg}"
        );
    }

    #[test]
    fn singbox_hysteria2_and_tuic_always_emit_tls() {
        let mut h2 = mk(ProxyType::Hysteria2, "1.1.1.1", 443);
        h2.password = Some("pw".into());
        let mut tuic = mk(ProxyType::Tuic, "2.2.2.2", 443);
        tuic.uuid = Some("u".into());
        tuic.password = Some("pw".into());
        let out = super::to_singbox_json(&[h2, tuic]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        for o in v["outbounds"].as_array().unwrap() {
            assert_eq!(o["tls"]["enabled"], true, "hysteria2/tuic 必须带 TLS: {o}");
        }
    }

    #[test]
    fn singbox_utls_is_object_and_http_exports_auth_and_tls() {
        let mut vless = mk(ProxyType::Vless, "1.1.1.1", 443);
        vless.uuid = Some("u".into());
        vless.tls = Some(true);
        vless.fingerprint = Some("chrome".into());

        let mut http = mk(ProxyType::Http, "2.2.2.2", 443);
        http.password = Some("pw".into());
        http.extra = Some(serde_json::json!({ "user": "alice" }));
        http.tls = Some(true);

        let out = super::to_singbox_json(&[vless, http]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let arr = v["outbounds"].as_array().unwrap();
        assert_eq!(arr[0]["tls"]["utls"]["fingerprint"], "chrome");
        assert!(
            arr[0]["tls"].get("utls_fingerprint").is_none(),
            "utls 必须是对象，不是平铺字段"
        );
        assert_eq!(arr[1]["username"], "alice");
        assert_eq!(arr[1]["password"], "pw");
        assert_eq!(arr[1]["tls"]["enabled"], true);
    }

    #[test]
    fn reality_exported_to_clash_singbox_v2ray() {
        let mut p = mk(ProxyType::Vless, "reality.example.com", 443);
        p.uuid = Some("u".into());
        p.network = Some("tcp".into());
        p.tls = Some(true);
        p.sni = Some("www.example.com".into());
        p.fingerprint = Some("chrome".into());
        p.reality = true;
        p.reality_public_key = Some("PK".into());
        p.reality_short_id = Some("ab12".into());

        let clash = super::to_clash_meta(std::slice::from_ref(&p));
        assert!(clash.contains("reality-opts"), "clash 缺 reality-opts: {clash}");
        assert!(clash.contains("public-key: PK"));
        assert!(clash.contains("short-id: ab12"));
        assert!(clash.contains("client-fingerprint: chrome"));

        let sb = super::to_singbox_json(std::slice::from_ref(&p));
        let sv: serde_json::Value = serde_json::from_str(&sb).unwrap();
        let tls = &sv["outbounds"][0]["tls"];
        assert_eq!(tls["reality"]["enabled"], true);
        assert_eq!(tls["reality"]["public_key"], "PK");
        assert_eq!(tls["reality"]["short_id"], "ab12");
        assert_eq!(tls["utls"]["fingerprint"], "chrome");

        let v2 = super::to_v2ray_json(std::slice::from_ref(&p));
        let vv: serde_json::Value = serde_json::from_str(&v2).unwrap();
        let stream = &vv[0]["streamSettings"];
        assert_eq!(stream["security"], "reality");
        assert_eq!(stream["realitySettings"]["publicKey"], "PK");
        assert_eq!(stream["realitySettings"]["shortId"], "ab12");
    }

    #[test]
    fn h2_transport_exported_to_all_formats() {
        let mut p = mk(ProxyType::Vless, "h2.example.com", 443);
        p.uuid = Some("u".into());
        p.network = Some("h2".into());
        p.tls = Some(true);
        p.host = Some("h2.example.com".into());
        p.path = Some("/h2".into());

        let clash = super::to_clash_meta(std::slice::from_ref(&p));
        assert!(clash.contains("h2-opts"), "clash 缺 h2-opts: {clash}");
        assert!(clash.contains("path: /h2"));
        assert!(clash.contains("h2.example.com"));

        let v2 = super::to_v2ray_json(std::slice::from_ref(&p));
        let vv: serde_json::Value = serde_json::from_str(&v2).unwrap();
        assert_eq!(vv[0]["streamSettings"]["httpSettings"]["path"], "/h2");
        assert_eq!(
            vv[0]["streamSettings"]["httpSettings"]["host"][0],
            "h2.example.com"
        );

        let sb = super::to_singbox_json(std::slice::from_ref(&p));
        let sv: serde_json::Value = serde_json::from_str(&sb).unwrap();
        assert_eq!(sv["outbounds"][0]["transport"]["type"], "http");
        assert_eq!(
            sv["outbounds"][0]["transport"]["host"][0],
            "h2.example.com"
        );
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
    fn ss_plugin_exported_to_clash_singbox_and_surge_obfs() {
        let mut p = mk(ProxyType::Ss, "1.1.1.1", 8388);
        p.method = Some("aes-256-gcm".into());
        p.password = Some("pw".into());
        p.plugin = Some("obfs-local".into());
        p.plugin_opts = Some("obfs=http;obfs-host=example.com".into());

        let clash = super::to_clash_meta(std::slice::from_ref(&p));
        assert!(clash.contains("plugin: obfs"), "clash 插件名错误: {clash}");
        assert!(clash.contains("mode: http"));
        assert!(clash.contains("host: example.com"));

        let sb = super::to_singbox_json(std::slice::from_ref(&p));
        let sv: serde_json::Value = serde_json::from_str(&sb).unwrap();
        assert_eq!(sv["outbounds"][0]["plugin"], "obfs-local");
        assert_eq!(
            sv["outbounds"][0]["plugin_opts"],
            "obfs=http;obfs-host=example.com"
        );

        // v2ray-core 无法表示 SS 插件：必须跳过而不是输出丢失插件的错误节点。
        assert_eq!(super::to_v2ray_json(std::slice::from_ref(&p)), "[]");

        let surge = super::to_surge(std::slice::from_ref(&p));
        assert!(surge.contains("obfs=http"));
        assert!(surge.contains("obfs-host=example.com"));
    }

    #[test]
    fn ss_v2ray_plugin_is_skipped_in_surge() {
        let mut p = mk(ProxyType::Ss, "1.1.1.1", 8388);
        p.method = Some("aes-256-gcm".into());
        p.password = Some("pw".into());
        p.plugin = Some("v2ray-plugin".into());
        p.plugin_opts = Some("mode=websocket;tls".into());
        assert!(super::to_surge(std::slice::from_ref(&p)).is_empty());
    }

    #[test]
    fn hysteria2_obfs_exported_to_clash_and_singbox() {
        let mut p = mk(ProxyType::Hysteria2, "hy.example.com", 443);
        p.password = Some("pw".into());
        p.obfs = Some("salamander".into());
        p.obfs_password = Some("secret".into());

        let clash = super::to_clash_meta(std::slice::from_ref(&p));
        assert!(clash.contains("obfs: salamander"));
        assert!(clash.contains("obfs-password: secret"));

        let sb = super::to_singbox_json(std::slice::from_ref(&p));
        let sv: serde_json::Value = serde_json::from_str(&sb).unwrap();
        assert_eq!(sv["outbounds"][0]["obfs"]["type"], "salamander");
        assert_eq!(sv["outbounds"][0]["obfs"]["password"], "secret");
    }

    #[test]
    fn surge_names_are_sanitized() {
        let mut p = mk(ProxyType::Ss, "1.1.1.1", 8388);
        p.name = "evil\nnew = x, y".into();
        p.method = Some("aes-256-gcm".into());
        p.password = Some("pw".into());
        let out = super::to_surge(std::slice::from_ref(&p));
        assert_eq!(out.lines().count(), 1, "节点名中的换行不得拆出新行: {out}");
        assert!(out.starts_with("evil_new _ x_ y = ss,"), "{out}");
    }

    #[test]
    fn unique_names_avoid_collision_with_existing_suffix() {
        // ["A", "A", "A #2"] 的第三个节点原名恰好等于前一个被改出的后缀名，
        // 旧的计数实现会让两个 "A #2" 同时存在。修复后必须全部唯一。
        let mut a = mk(ProxyType::Ss, "1.1.1.1", 8388);
        a.name = "A".into();
        a.method = Some("aes-256-gcm".into());
        a.password = Some("p1".into());
        let mut b = mk(ProxyType::Ss, "2.2.2.2", 8388);
        b.name = "A".into();
        b.method = Some("aes-256-gcm".into());
        b.password = Some("p2".into());
        let mut c = mk(ProxyType::Ss, "3.3.3.3", 8388);
        c.name = "A #2".into();
        c.method = Some("aes-256-gcm".into());
        c.password = Some("p3".into());

        let yaml = super::export_str(&[a, b, c], "clash-meta");
        let names: Vec<&str> = yaml
            .lines()
            .filter_map(|l| l.trim().strip_prefix("- name: "))
            .collect();
        assert_eq!(names.len(), 3);
        let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
        assert_eq!(unique.len(), 3, "导出名必须唯一，实际: {names:?}");
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
