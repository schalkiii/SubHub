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
pub(crate) fn ensure_unique_names(proxies: &mut [Proxy]) {
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
            insert_opt(&mut m, "network", &p.network);
            insert_opt(&mut m, "client-fingerprint", &p.fingerprint);
            insert_string_list(&mut m, "alpn", &p.alpn);
            insert_transport_opts(&mut m, p);
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
            insert_string_list(&mut m, "alpn", &p.alpn);
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
            insert_string_list(&mut m, "alpn", &p.alpn);
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
            insert_string_list(&mut m, "alpn", &p.alpn);
            if let Some(up) = p.up_mbps {
                m.insert(Value::from("up"), Value::from(format!("{up} Mbps")));
            }
            if let Some(down) = p.down_mbps {
                m.insert(Value::from("down"), Value::from(format!("{down} Mbps")));
            }
            insert_opt_bool(&mut m, "skip-cert-verify", p.skip_cert_verify);
        }
        ProxyType::Tuic => {
            insert_opt(&mut m, "uuid", &p.uuid);
            insert_opt(&mut m, "password", &p.password);
            insert_opt(&mut m, "sni", &p.sni);
            insert_string_list(&mut m, "alpn", &p.alpn);
            insert_opt(&mut m, "congestion-controller", &p.congestion_control);
            // UDP-over-Stream replaces the relay mode entirely (UDP rides the
            // authenticated stream instead of a QUIC relay); emitting both
            // gives the kernel two contradictory instructions.
            if !p.udp_over_stream.unwrap_or(false) {
                insert_opt(&mut m, "udp-relay-mode", &p.udp_relay_mode);
            }
            insert_opt_bool(&mut m, "udp-over-stream", p.udp_over_stream);
            insert_opt_bool(&mut m, "reduce-rtt", p.zero_rtt_handshake);
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
            let tls = p.tls.or(if p.port == 443 { Some(true) } else { None });
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

fn insert_string_list(m: &mut Mapping, key: &str, val: &Option<Vec<String>>) {
    if let Some(list) = val {
        if !list.is_empty() {
            let seq: Vec<Value> = list.iter().map(|s| Value::from(s.clone())).collect();
            let _ = m.insert(Value::from(key), Value::Sequence(seq));
        }
    }
}

/// SS 的加密方法可能存于 `method`（SIP002 URI）或 `cipher`（clash-meta YAML）
/// 任一字段。所有导出格式都必须走这里取同一个值，否则 YAML 导入的 SS 节点会被
/// 导出成默认的 `aes-256-gcm`，客户端握手必然失败。
fn ss_method(p: &Proxy) -> String {
    // 与 `Proxy::fingerprint()` 读同一个归一值（method → cipher），保证
    // 「导出用的方法」与「身份判定用的方法」永不背离。
    p.ss_method_value()
        .map(str::to_string)
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
        if let Some(alpn) = &p.alpn {
            tls["alpn"] = json!(alpn);
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
                eprintln!("[export] v2ray 不支持 SS 插件节点「{}」，已跳过", p.name);
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
    if let Some(alpn) = &p.alpn {
        if !alpn.is_empty() {
            t["alpn"] = json!(alpn);
        }
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
            if let Some(t) = singbox_transport(p) {
                o["transport"] = t;
            }
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
            if let Some(up) = p.up_mbps {
                o["up_mbps"] = json!(up);
            }
            if let Some(down) = p.down_mbps {
                o["down_mbps"] = json!(down);
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
            if let Some(cc) = &p.congestion_control {
                o["congestion_control"] = json!(cc);
            }
            if !p.udp_over_stream.unwrap_or(false) {
                if let Some(mode) = &p.udp_relay_mode {
                    o["udp_relay_mode"] = json!(mode);
                }
            }
            if let Some(v) = p.udp_over_stream {
                o["udp_over_stream"] = json!(v);
            }
            if let Some(v) = p.zero_rtt_handshake {
                o["zero_rtt_handshake"] = json!(v);
            }
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

/// Surge's `[Proxy]` type keywords are a closed set (Surge manual → Policies
/// Overview): `ss` / `vmess` / `trojan` / `tuic` / `hysteria2` / `anytls` /
/// `socks5` / `https` / `http`. There is **no `vless`** keyword and no
/// `flow=` / REALITY parameters, and `wireguard` lives in a separate
/// `[WireGuard]` section — emitting either would produce a line Surge cannot
/// parse, so the node is skipped with a log instead.
///
/// Transport-wise Surge implements TCP and WebSocket only: `grpc` / `h2` /
/// `httpupgrade` have no `[Proxy]` equivalent, and degrading such a node to a
/// plain TCP line yields a proxy that always fails the handshake.
fn surge_supported(p: &Proxy) -> bool {
    match p.type_ {
        ProxyType::Ss => match ss_plugin(p) {
            Some((name, _)) => {
                let ok = name.to_ascii_lowercase().contains("obfs");
                if !ok {
                    eprintln!(
                        "[export] surge 不支持 SS 插件「{name}」节点「{}」，已跳过",
                        p.name
                    );
                }
                ok
            }
            None => true,
        },
        ProxyType::Vmess | ProxyType::Trojan => match p.network.as_deref().unwrap_or("tcp") {
            "" | "tcp" | "ws" => true,
            other => {
                eprintln!(
                    "[export] surge 不支持 {other} 传输节点「{}」，已跳过",
                    p.name
                );
                false
            }
        },
        ProxyType::Vless => {
            eprintln!(
                "[export] surge 无 vless 协议，节点「{}」已跳过（请改用 clash-meta / sing-box）",
                p.name
            );
            false
        }
        ProxyType::Wireguard => {
            eprintln!(
                "[export] surge wireguard 需要独立 [WireGuard] 段，节点「{}」已跳过",
                p.name
            );
            false
        }
        ProxyType::Hysteria2 => {
            let obfs = p.obfs.as_deref().unwrap_or_default();
            if obfs.is_empty() {
                return true;
            }
            let known = matches!(obfs.to_ascii_lowercase().as_str(), "salamander" | "gecko");
            if !known {
                eprintln!(
                    "[export] surge 不支持 hysteria2 混淆「{obfs}」节点「{}」，已跳过",
                    p.name
                );
                return false;
            }
            if p.obfs_password.as_deref().unwrap_or_default().is_empty() {
                eprintln!(
                    "[export] surge hysteria2 节点「{}」缺少混淆密码，已跳过",
                    p.name
                );
                return false;
            }
            true
        }
        ProxyType::Tuic | ProxyType::Socks5 | ProxyType::Http | ProxyType::AnyTls => true,
        ProxyType::Other => false,
    }
}

/// Flat WebSocket parameters. `path` / `host` are only emitted for the `ws`
/// transport — a TCP node carrying them would advertise a transport it does
/// not use.
fn push_surge_ws(s: &mut String, p: &Proxy) {
    if p.network.as_deref() != Some("ws") {
        return;
    }
    s.push_str(", ws=true");
    if let Some(path) = &p.path {
        s.push_str(&format!(", ws-path={}", surge_safe(path)));
    }
    if let Some(host) = &p.host {
        s.push_str(&format!(", ws-headers=Host:{}", surge_safe(host)));
    }
}

/// TLS keys shared by Surge's TLS-based proxy types. Our `fingerprint` field
/// is mihomo's `client-fingerprint` (uTLS), whose Surge key is
/// `tls-fingerprint=`; `alpn=` is only emitted for a single value because a
/// comma-separated list needs quoting that older Surge builds reject.
///
/// `tls=` is deliberately *not* emitted: in Surge only vmess defaults to
/// plaintext and needs the explicit switch (trojan / anytls / hysteria2 / tuic
/// imply TLS from the protocol itself).
fn push_surge_tls(s: &mut String, p: &Proxy) {
    if let Some(sni) = &p.sni {
        s.push_str(&format!(", sni={}", surge_safe(sni)));
    }
    if p.skip_cert_verify.unwrap_or(false) {
        s.push_str(", skip-cert-verify=true");
    }
    if let Some(fp) = &p.fingerprint {
        s.push_str(&format!(", tls-fingerprint={}", surge_safe(fp)));
    }
    if let Some([single]) = p.alpn.as_deref() {
        s.push_str(&format!(", alpn={}", surge_safe(single)));
    }
}

/// SOCKS5 / HTTP take username and password as *positional* arguments
/// (`socks5, host, port, username, password`). A password without a username
/// has no slot, so it is dropped with a log rather than shifted into the
/// username field.
fn surge_positional_auth(p: &Proxy) -> String {
    let user = p
        .extra
        .as_ref()
        .and_then(|e| e.get("user"))
        .and_then(|x| x.as_str())
        .unwrap_or_default();
    if user.is_empty() {
        if p.password.is_some() {
            eprintln!(
                "[export] surge {} 节点「{}」只有密码没有用户名，凭据已丢弃",
                p.type_.as_str(),
                p.name
            );
        }
        return String::new();
    }
    match &p.password {
        Some(pw) => format!(", {}, {}", surge_safe(user), surge_safe(pw)),
        None => format!(", {}", surge_safe(user)),
    }
}

pub fn to_surge(proxies: &[Proxy]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for p in proxies {
        if !surge_supported(p) {
            continue;
        }
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
                // `surge_supported` already rejected the non-obfs plugins.
                if let Some((_, opts)) = ss_plugin(p) {
                    if let Some((_, mode)) = opts.iter().find(|(k, _)| k == "obfs" || k == "mode") {
                        s.push_str(&format!(", obfs={}", surge_safe(mode)));
                    }
                    if let Some((_, host)) =
                        opts.iter().find(|(k, _)| k == "obfs-host" || k == "host")
                    {
                        s.push_str(&format!(", obfs-host={}", surge_safe(host)));
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
                push_surge_tls(&mut s, p);
                push_surge_ws(&mut s, p);
                s
            }
            ProxyType::Vmess => {
                let mut s = format!(
                    "{} = vmess, {}, {}, username={}",
                    name,
                    p.server,
                    p.port,
                    p.uuid.clone().unwrap_or_default()
                );
                if let Some(cipher) = &p.cipher {
                    s.push_str(&format!(", encrypt-method={}", surge_safe(cipher)));
                }
                if p.tls.unwrap_or(false) {
                    s.push_str(", tls=true");
                }
                push_surge_tls(&mut s, p);
                push_surge_ws(&mut s, p);
                s
            }
            ProxyType::Hysteria2 => {
                let mut s = format!(
                    "{} = hysteria2, {}, {}, password={}",
                    name,
                    p.server,
                    p.port,
                    p.password.clone().unwrap_or_default()
                );
                // Surge has no `obfs=` key: the type is implied by the
                // password key. Bandwidth is a plain number (no ` Mbps`) and
                // only the download direction has a key.
                if let Some(obfs) = p.obfs.as_deref().filter(|o| !o.is_empty()) {
                    let key = if obfs.eq_ignore_ascii_case("gecko") {
                        "gecko-password"
                    } else {
                        "salamander-password"
                    };
                    s.push_str(&format!(
                        ", {key}={}",
                        p.obfs_password.clone().unwrap_or_default()
                    ));
                }
                if let Some(down) = p.down_mbps {
                    s.push_str(&format!(", download-bandwidth={down}"));
                }
                push_surge_tls(&mut s, p);
                s
            }
            ProxyType::Tuic => {
                // Surge needs the explicit `version=5`; without it it falls
                // back to the v4 token-only handshake and the node fails.
                // Congestion control / udp-relay-mode have no Surge key and are
                // left out.
                format!(
                    "{} = tuic, {}, {}, uuid={}, password={}, version=5",
                    name,
                    p.server,
                    p.port,
                    p.uuid.clone().unwrap_or_default(),
                    p.password.clone().unwrap_or_default()
                )
            }
            ProxyType::AnyTls => {
                let mut s = format!(
                    "{} = anytls, {}, {}, password={}",
                    name,
                    p.server,
                    p.port,
                    p.password.clone().unwrap_or_default()
                );
                push_surge_tls(&mut s, p);
                s
            }
            ProxyType::Socks5 => {
                let kw = if p.tls.unwrap_or(false) {
                    "socks5-tls"
                } else {
                    "socks5"
                };
                format!(
                    "{} = {kw}, {}, {}{}",
                    name,
                    p.server,
                    p.port,
                    surge_positional_auth(p)
                )
            }
            ProxyType::Http => {
                // Same rule as the clash export: port 443 is treated as TLS,
                // and Surge's TLS HTTP proxy is the `https` keyword.
                let kw = if p.tls.unwrap_or(false) || p.port == 443 {
                    "https"
                } else {
                    "http"
                };
                format!(
                    "{} = {kw}, {}, {}{}",
                    name,
                    p.server,
                    p.port,
                    surge_positional_auth(p)
                )
            }
            _ => continue,
        };
        lines.push(line);
    }

    // Dedupe on the finished lines: sanitizing can turn two distinct names into
    // the same string ("a,b" / "a_b"), and nodes skipped above must not shift
    // the ` #N` suffixes of the nodes that were actually emitted.
    // `surge_safe` strips `=` from the name, so the first " = " is the separator.
    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        match line.split_once(" = ") {
            Some((head, rest)) => {
                let mut name = head.to_string();
                if !seen_names.insert(name.clone()) {
                    let base = name.clone();
                    let mut i = 2;
                    loop {
                        let candidate = format!("{base} #{i}");
                        if seen_names.insert(candidate.clone()) {
                            name = candidate;
                            break;
                        }
                        i += 1;
                    }
                }
                out.push(format!("{name} = {rest}"));
            }
            None => out.push(line),
        }
    }
    out.join("\n")
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
        assert!(
            yaml.contains("tls: true"),
            "https proxy must emit tls: true"
        );
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
        assert!(
            v2.contains("chacha20-ietf-poly1305"),
            "v2ray 丢失 cipher: {v2}"
        );
        let sb = super::to_singbox_json(std::slice::from_ref(&p));
        assert!(
            sb.contains("chacha20-ietf-poly1305"),
            "sing-box 丢失 cipher: {sb}"
        );
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
        assert!(
            clash.contains("reality-opts"),
            "clash 缺 reality-opts: {clash}"
        );
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
        assert_eq!(sv["outbounds"][0]["transport"]["host"][0], "h2.example.com");
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
    fn surge_skips_vless_and_unsupported_transports() {
        // Surge 的 [Proxy] 类型表里没有 vless，传输层只有 TCP / WebSocket。
        // 输出这些节点只会得到一行 Surge 解析不了、或握手必败的配置。
        let mut vl = mk(ProxyType::Vless, "1.1.1.1", 443);
        vl.uuid = Some("u".into());
        vl.tls = Some(true);
        let mut grpc = mk(ProxyType::Vmess, "2.2.2.2", 443);
        grpc.uuid = Some("u".into());
        grpc.network = Some("grpc".into());
        let mut hy2_grpc = mk(ProxyType::Trojan, "3.3.3.3", 443);
        hy2_grpc.password = Some("pw".into());
        hy2_grpc.network = Some("h2".into());

        assert!(
            super::to_surge(std::slice::from_ref(&vl)).is_empty(),
            "vless 必须整节点跳过"
        );
        assert!(
            super::to_surge(std::slice::from_ref(&grpc)).is_empty(),
            "grpc 必须整节点跳过"
        );
        assert!(
            super::to_surge(std::slice::from_ref(&hy2_grpc)).is_empty(),
            "h2 必须整节点跳过"
        );
    }

    #[test]
    fn surge_websocket_params_are_flat_and_tls_key_is_vmess_only() {
        let mut vm = mk(ProxyType::Vmess, "vm.example.com", 443);
        vm.uuid = Some("uuid-1".into());
        vm.network = Some("ws".into());
        vm.path = Some("/ws-path".into());
        vm.host = Some("cdn.example.com".into());
        vm.tls = Some(true);
        vm.sni = Some("cdn.example.com".into());

        let vm_out = super::to_surge(std::slice::from_ref(&vm));
        assert!(vm_out.contains(", ws=true"), "{vm_out}");
        assert!(vm_out.contains("ws-path=/ws-path"), "{vm_out}");
        assert!(
            vm_out.contains("ws-headers=Host:cdn.example.com"),
            "{vm_out}"
        );
        assert!(vm_out.contains("tls=true"), "{vm_out}");
        // `network=` 不是 Surge 的关键字（Surge 用 ws=true 表示传输层）。
        assert!(!vm_out.contains("network="), "{vm_out}");

        let mut tr = mk(ProxyType::Trojan, "tr.example.com", 443);
        tr.password = Some("pw".into());
        tr.network = Some("ws".into());
        tr.path = Some("/t".into());
        tr.tls = Some(true);
        let tr_out = super::to_surge(std::slice::from_ref(&tr));
        assert!(tr_out.contains("ws-path=/t"), "{tr_out}");
        // trojan 由协议本身隐含 TLS，Surge 不认该键。
        assert!(!tr_out.contains("tls="), "{tr_out}");
    }

    #[test]
    fn surge_exports_hysteria2_tuic_anytls_socks_and_http() {
        let mut hy = mk(ProxyType::Hysteria2, "hy.example.com", 8443);
        hy.password = Some("pw".into());
        hy.obfs = Some("salamander".into());
        hy.obfs_password = Some("secret".into());
        hy.down_mbps = Some(200);
        hy.up_mbps = Some(100);
        let hy_out = super::to_surge(std::slice::from_ref(&hy));
        assert!(
            hy_out.starts_with("node = hysteria2, hy.example.com, 8443, password=pw"),
            "{hy_out}"
        );
        assert!(hy_out.contains("salamander-password=secret"), "{hy_out}");
        // Surge 的带宽键是纯数字，且只有下行键。
        assert!(hy_out.contains("download-bandwidth=200"), "{hy_out}");
        assert!(!hy_out.contains("100"), "{hy_out}");

        let mut tu = mk(ProxyType::Tuic, "tu.example.com", 443);
        tu.uuid = Some("u".into());
        tu.password = Some("pw".into());
        let tu_out = super::to_surge(std::slice::from_ref(&tu));
        assert!(tu_out.contains("uuid=u"), "{tu_out}");
        assert!(tu_out.contains("version=5"), "{tu_out}");

        let mut an = mk(ProxyType::AnyTls, "an.example.com", 56147);
        an.password = Some("tok".into());
        an.sni = Some("apple.com".into());
        an.skip_cert_verify = Some(true);
        let an_out = super::to_surge(std::slice::from_ref(&an));
        assert!(
            an_out.starts_with("node = anytls, an.example.com, 56147, password=tok"),
            "{an_out}"
        );
        assert!(an_out.contains("sni=apple.com"), "{an_out}");
        assert!(an_out.contains("skip-cert-verify=true"), "{an_out}");

        let mut so = mk(ProxyType::Socks5, "10.0.0.1", 1080);
        so.extra = Some(serde_json::json!({ "user": "bob" }));
        so.password = Some("secretpw".into());
        let so_out = super::to_surge(std::slice::from_ref(&so));
        // SOCKS5 的用户名/密码是位置参数。
        assert_eq!(so_out, "node = socks5, 10.0.0.1, 1080, bob, secretpw");

        let mut ht = mk(ProxyType::Http, "10.0.0.2", 443);
        ht.extra = Some(serde_json::json!({ "user": "bob" }));
        // 443 端口的 http 按 TLS 处理，与 clash 导出同一规则。
        assert_eq!(
            super::to_surge(std::slice::from_ref(&ht)),
            "node = https, 10.0.0.2, 443, bob"
        );
    }

    #[test]
    fn surge_hysteria2_unknown_obfs_is_skipped() {
        let mut hy = mk(ProxyType::Hysteria2, "hy.example.com", 8443);
        hy.password = Some("pw".into());
        hy.obfs = Some("gecko-ish".into());
        hy.obfs_password = Some("secret".into());
        assert!(super::to_surge(std::slice::from_ref(&hy)).is_empty());
    }

    #[test]
    fn surge_dedupes_names_that_collide_after_sanitizing() {
        let mut a = mk(ProxyType::Ss, "1.1.1.1", 8388);
        a.name = "a,b".into();
        a.method = Some("aes-256-gcm".into());
        a.password = Some("pw".into());
        let mut b = a.clone();
        b.name = "a_b".into();
        let out = super::to_surge(&[a, b]);
        let names: Vec<&str> = out
            .lines()
            .map(|l| l.split(" = ").next().unwrap())
            .collect();
        assert_eq!(names, vec!["a_b", "a_b #2"], "清洗后同名必须加后缀: {out}");
    }

    #[test]
    fn tuic_advanced_fields_exported() {
        let mut p = mk(ProxyType::Tuic, "tu.example.com", 443);
        p.uuid = Some("u".into());
        p.password = Some("pw".into());
        p.alpn = Some(vec!["h3".into()]);
        p.congestion_control = Some("bbr".into());
        p.udp_relay_mode = Some("quic".into());
        p.udp_over_stream = Some(false);
        p.zero_rtt_handshake = Some(true);

        let clash = super::to_clash_meta(std::slice::from_ref(&p));
        assert!(clash.contains("congestion-controller: bbr"), "{clash}");
        assert!(clash.contains("udp-relay-mode: quic"));
        assert!(clash.contains("alpn:"));

        let sb = super::to_singbox_json(std::slice::from_ref(&p));
        let sv: serde_json::Value = serde_json::from_str(&sb).unwrap();
        let o = &sv["outbounds"][0];
        assert_eq!(o["congestion_control"], "bbr");
        assert_eq!(o["udp_relay_mode"], "quic");
        assert_eq!(o["zero_rtt_handshake"], true);
        assert_eq!(o["tls"]["alpn"][0], "h3");
    }

    #[test]
    fn tuic_udp_over_stream_supersedes_relay_mode() {
        // UDP-over-Stream and udp-relay-mode are mutually exclusive ways to
        // carry UDP. Emitting both makes the kernel ignore one of them, so the
        // relay mode is dropped when udp-over-stream is on.
        let mut p = mk(ProxyType::Tuic, "tu.example.com", 443);
        p.uuid = Some("u".into());
        p.password = Some("pw".into());
        p.udp_relay_mode = Some("quic".into());
        p.udp_over_stream = Some(true);

        let clash = super::to_clash_meta(std::slice::from_ref(&p));
        assert!(clash.contains("udp-over-stream: true"), "{clash}");
        assert!(
            !clash.contains("udp-relay-mode"),
            "udp-over-stream 已开启时不得再写 udp-relay-mode: {clash}"
        );

        let sb = super::to_singbox_json(std::slice::from_ref(&p));
        let sv: serde_json::Value = serde_json::from_str(&sb).unwrap();
        let o = &sv["outbounds"][0];
        assert_eq!(o["udp_over_stream"], true);
        assert!(
            o.get("udp_relay_mode").is_none(),
            "sing-box 同时给出两种 UDP 承载方式: {o}"
        );
    }

    #[test]
    fn hysteria2_up_down_and_alpn_exported() {
        let mut p = mk(ProxyType::Hysteria2, "hy.example.com", 443);
        p.password = Some("pw".into());
        p.up_mbps = Some(100);
        p.down_mbps = Some(200);
        p.alpn = Some(vec!["h3".into()]);

        let clash = super::to_clash_meta(std::slice::from_ref(&p));
        assert!(clash.contains("up: 100 Mbps"), "{clash}");
        assert!(clash.contains("down: 200 Mbps"));
        assert!(clash.contains("alpn:"));

        let sb = super::to_singbox_json(std::slice::from_ref(&p));
        let sv: serde_json::Value = serde_json::from_str(&sb).unwrap();
        let o = &sv["outbounds"][0];
        assert_eq!(o["up_mbps"], 100);
        assert_eq!(o["down_mbps"], 200);
        assert_eq!(o["tls"]["alpn"][0], "h3");
    }

    #[test]
    fn vless_alpn_exported_to_clash_v2ray_and_singbox() {
        let mut p = mk(ProxyType::Vless, "v.example.com", 443);
        p.uuid = Some("u".into());
        p.tls = Some(true);
        p.alpn = Some(vec!["h2".into(), "http/1.1".into()]);

        let clash = super::to_clash_meta(std::slice::from_ref(&p));
        assert!(
            clash.contains("- h2") && clash.contains("http/1.1"),
            "{clash}"
        );

        let v2 = super::to_v2ray_json(std::slice::from_ref(&p));
        let vv: serde_json::Value = serde_json::from_str(&v2).unwrap();
        assert_eq!(vv[0]["streamSettings"]["tlsSettings"]["alpn"][0], "h2");

        let sb = super::to_singbox_json(std::slice::from_ref(&p));
        let sv: serde_json::Value = serde_json::from_str(&sb).unwrap();
        assert_eq!(sv["outbounds"][0]["tls"]["alpn"][1], "http/1.1");
    }

    #[test]
    fn trojan_websocket_transport_exported_to_clash_and_singbox() {
        // trojan 的 ws path / Host 此前只解析、不导出，客户端拿到的是裸 TCP
        // 条目，经 CDN 中转时握手必败。
        let mut p = mk(ProxyType::Trojan, "tr.example.com", 443);
        p.password = Some("pw".into());
        p.network = Some("ws".into());
        p.path = Some("/trojan".into());
        p.host = Some("cdn.example.com".into());
        p.sni = Some("cdn.example.com".into());

        let clash = super::to_clash_meta(std::slice::from_ref(&p));
        assert!(clash.contains("network: ws"), "{clash}");
        assert!(clash.contains("ws-opts:"), "{clash}");
        assert!(clash.contains("path: /trojan"), "{clash}");
        assert!(clash.contains("Host: cdn.example.com"), "{clash}");

        let sb = super::to_singbox_json(std::slice::from_ref(&p));
        let sv: serde_json::Value = serde_json::from_str(&sb).unwrap();
        let o = &sv["outbounds"][0];
        assert_eq!(o["transport"]["type"], "ws");
        assert_eq!(o["transport"]["path"], "/trojan");
        assert_eq!(o["transport"]["headers"]["Host"], "cdn.example.com");
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
