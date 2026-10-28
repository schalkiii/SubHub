# 协议字段参考索引

> 目的：SubHub 的解析 / 导出字段不是凭印象命名，而是对照 mihomo（Clash.Meta）、sing-box 与 Xray 的公开文档/实现逐项映射。本文是“模型字段 ↔ 各客户端字段”的单一索引，新增协议字段时先在这里登记，再改代码与测试。
>
> 参考时间：2026-10-28。客户端版本字段可能继续演进；以对应文档当前版本为准。

## 1. 参考来源

| 参考实现 | 用途 | 链接 |
|---|---|---|
| mihomo TUIC | `congestion-controller` / `udp-relay-mode` / `reduce-rtt` / `udp-over-stream` 命名 | https://wiki.metacubex.one/en/config/proxies/tuic/ |
| sing-box TUIC | `congestion_control` / `udp_relay_mode` / `udp_over_stream` / `zero_rtt_handshake` / `tls.alpn` | https://sing-box.sagernet.org/configuration/outbound/tuic/ |
| mihomo Hysteria2 | `up` / `down` 字符串、`obfs` / `obfs-password`、`ports` / `hop-interval`、`alpn` | https://wiki.metacubex.one/en/config/proxies/hysteria2/ |
| sing-box Hysteria2 | `up_mbps` / `down_mbps` 数字、`obfs.type` / `obfs.password`、`tls.alpn` | https://sing-box.sagernet.org/configuration/outbound/hysteria2/ |
| sing-box Shadowsocks | `plugin`: `obfs-local` / `v2ray-plugin`；`plugin_opts` 为分号分隔字符串 | https://sing-box.sagernet.org/configuration/outbound/shadowsocks/ |
| Shadowsocks SIP002 | `plugin=name;opts` 的 URI 形态 | https://shadowsocks.org/doc/sip002.html |
| Xray VLESS / REALITY | VLESS 出站与 REALITY 字段（`publicKey` / `shortId` / `serverName` / `fingerprint`） | https://xtls.github.io/config/outbounds/vless.html |
| Surge 策略类型表 | `[Proxy]` 关键字是**封闭集合**，决定 Surge 导出支持/跳过哪些协议 | https://manual.nssurge.com/policies/overview.html |
| Surge Mac / iOS Release Notes | 各协议最低版本与新键名（`salamander-password` / `gecko-password` / `port-hopping` / AnyTLS / `alpn`） | https://kb.nssurge.com/surge-knowledge-base/release-notes/surge-mac-legacy |
| NodeDeck `protocol-mapping.md` | 第三方项目维护的 mihomo ↔ Surge 逐项对照，含 [CS]/[C]/[S] 标注，用于交叉验证键名 | https://github.com/MingSpace/NodeDeck/blob/main/docs/protocol-mapping.md |

辅助速查（含源码行号锚点，便于进一步核对）：

- TUIC / Hysteria2 / Shadowsocks 字段对照：https://core-tutorial.argsment.com/zh/singbox/shadowsocks
- mihomo Hysteria2 字段表：https://core-tutorial.argsment.com/zh/mihomo/hysteria2

## 2. 字段映射

### 2.1 通用 TLS / ALPN

| 模型字段 | mihomo (clash-meta) | sing-box | v2ray/xray | 备注 |
|---|---|---|---|---|
| `alpn: Option<Vec<String>>` | `alpn: [h2, http/1.1]` | `tls.alpn: [...]` | `tlsSettings.alpn: [...]` | vmess / vless / trojan / hysteria2 / tuic 共用；REALITY 导出不写 ALPN（xray REALITY 不使用该字段） |

### 2.2 Shadowsocks 插件（SIP003）

| 模型字段 | SIP002 URI | mihomo | sing-box | Surge | v2ray |
|---|---|---|---|---|---|
| `plugin` | `plugin=obfs-local;...` | `plugin: obfs` 或 `v2ray-plugin` | `plugin: obfs-local` 或 `v2ray-plugin` | 仅 obfs（`obfs=`） | 不支持，导出时跳过 |
| `plugin_opts` | 分号分隔 `k=v` | `plugin-opts` 映射：`obfs`→`mode`、`obfs-host`→`host`；v2ray-plugin 保留 `mode/host/path/tls/mux` | 原样 `plugin_opts`（obfs 用 `obfs=` / `obfs-host=`） | `obfs-host` | — |

### 2.3 TUIC

| 模型字段 | mihomo | sing-box | 备注 |
|---|---|---|---|
| `congestion_control` | `congestion-controller` | `congestion_control` | `cubic` / `new_reno` / `bbr` |
| `udp_relay_mode` | `udp-relay-mode` | `udp_relay_mode` | `native` / `quic` |
| `udp_over_stream` | `udp-over-stream` | `udp_over_stream` | 与 `udp_relay_mode=quic` 冲突，由服务端决定取舍 |
| `zero_rtt_handshake` | `reduce-rtt` | `zero_rtt_handshake` | 0-RTT；mihomo 用 reduce-rtt 表达 |
| `alpn` | `alpn` | `tls.alpn` | 通常 `h3` |

### 2.4 Hysteria2

| 模型字段 | mihomo | sing-box | 备注 |
|---|---|---|---|
| `obfs` | `obfs`（`salamander` / `gecko`） | `obfs.type` | 空 = 关闭 |
| `obfs_password` | `obfs-password` | `obfs.password` | — |
| `up_mbps` | `up: "100 Mbps"` | `up_mbps: 100` | mihomo 是带单位字符串，sing-box 是数字 Mbps |
| `down_mbps` | `down: "200 Mbps"` | `down_mbps: 200` | 同上 |
| `alpn` | `alpn` | `tls.alpn` | 通常 `h3` |

### 2.5 VLESS REALITY

| 模型字段 | mihomo | sing-box | v2ray/xray |
|---|---|---|---|
| `reality` | `reality-opts` 是否存在 | `tls.reality.enabled` | `streamSettings.security = "reality"` |
| `reality_public_key` | `reality-opts.public-key` | `tls.reality.public_key` | `realitySettings.publicKey` |
| `reality_short_id` | `reality-opts.short-id` | `tls.reality.short_id` | `realitySettings.shortId` |
| `fingerprint` | `client-fingerprint` | `tls.utls.fingerprint` | `realitySettings.fingerprint` |

### 2.6 Surge `[Proxy]` 行

Surge 的类型关键字是**封闭集合**：`ss` / `snell` / `vmess` / `trojan` / `tuic`（= `tuic-v5`）/ `hysteria2` / `anytls` / `socks5`(`socks5-tls`) / `http`(`https`) / `wireguard` / `h2-connect` / `masque` / `trust-tunnel` / `ssh` / `external` 等，**没有 `vless`**，也没有 `flow=` / REALITY 参数页。

| 模型字段 | mihomo | Surge | 备注 |
|---|---|---|---|
| `sni` | `sni:` / `servername:` | `sni=` | vmess/vless 在 mihomo 侧是 `servername`，Surge 统一 `sni=` |
| `skip_cert_verify` | `skip-cert-verify:` | `skip-cert-verify=` | 同名 |
| `fingerprint`（uTLS 客户端指纹） | `client-fingerprint:` | `tls-fingerprint=` | **键名不同**；`server-cert-fingerprint-sha256=` 是服务器证书锁定，不是同一个东西 |
| `alpn` | `alpn: [..]` | `alpn=` | 多值需逗号 + 引号且要求 iOS 5.20+/Mac 6.7+，当前只输出单值 |
| `tls` | `tls: true` | `tls=true` | **只有 vmess 需要显式输出**（vmess 默认明文）；trojan/anytls/hysteria2/tuic 由协议隐含，不输出该键 |
| `network=ws` + `path` + `host` | `network: ws` + `ws-opts` | `ws=true, ws-path=, ws-headers=Host:x` | Surge 拍平、无 `network=` 键；grpc / h2 / httpupgrade 无对应 → 整节点跳过 |
| SS `plugin_opts` | `plugin: obfs` + `plugin-opts` | `obfs=` / `obfs-host=` / `obfs-uri=` | v2ray-plugin 不支持 → 跳过 |
| `obfs` + `obfs_password`（HY2） | `obfs:` + `obfs-password:` | `salamander-password=` / `gecko-password=` | **无 `obfs=` 键**，混淆类型由密码键名隐含；其它类型跳过 |
| `down_mbps` / `up_mbps` | `down:` / `up:`（带 ` Mbps`） | `download-bandwidth=`（纯数字） | Surge 没有上行带宽键 → 丢字段 |
| `congestion_control` / `udp_relay_mode` | `congestion-controller` / `udp-relay-mode` | —（无对应键） | TUIC 导出只保留 `uuid` / `password` / `version=5` |
| `extra.user` + `password`（SOCKS/HTTP） | `username:` / `password:` | 位置参数 `socks5, host, port, user, pass` | 只有密码没有用户名时无法定位 → 丢凭据并打日志 |

## 3. 解析/导出的特殊约定

- **SS 加密方法归一**：URI 存 `method`，clash YAML 存 `cipher`；所有导出器与**节点指纹**统一走 `Proxy::ss_method_value()`（`method` → `cipher`）。若只有导出器归一、指纹仍读 `method`，同一节点的 YAML 形态与 base64 形态会得到两个指纹：`merge` 不去重（导出里出现 `A` / `A #2`），订阅换形态时增量刷新把全部 SS 节点判为新节点、丢掉已测延迟。
- **指纹的字段边界**：`type|server|port|name|cred`，其中 `name` / `server` 与凭据字段一样把 `|` 转义成 `#`。真实节点名常含 `|`（「🇺🇸美国-电信|5倍」），不转义就能让两个不同节点拼出同一串指纹而被静默去重。
- **SS 插件名归一**：clash 侧统一写 `obfs` / `v2ray-plugin`；sing-box 侧统一写 `obfs-local` / `v2ray-plugin`。未知插件名原样透传，选项按 `k=v` 保留。
- **Hysteria2 / TUIC**：模型统一 `up_mbps/down_mbps` 数字；导入 mihomo 的带单位字符串时按单位换算（见下条），导出 mihomo 时补 ` Mbps` 单位。
- **ALPN**：URI `alpn=h2,http/1.1` 按逗号拆分；clash 既接受序列也接受逗号字符串；sing-box/xray 均为数组。
- **REALITY 缺公钥**：`is_exportable() == false`，引擎与导出都会跳过，避免生成必然握手失败的普通 TLS 节点。
- **带宽单位换算**：mihomo 的 `up` / `down` 允许 `Gbps` / `Mbps` / `kbps` / `bps`，`parse_mbps()` 按单位换算成 Mbps 数字（`1 Gbps` → `1000`，`500 kbps` → `1`），不再只取数字前缀。
- **Surge 名称**：导出前 `surge_safe()` 清洗换行 / 逗号 / `=`，防止配置行注入；清洗后仍撞名的追加 ` #2` / ` #3`。去重发生在**行构造之后**，被跳过的节点不会占掉后缀序号。
- **Surge 不支持的协议 / 传输**：`vless`（Surge 无此协议）、`wireguard`（需独立 `[WireGuard]` 段）、vmess/trojan 的 `grpc` / `h2` / `httpupgrade`、SS 的 v2ray-plugin、Hysteria2 的非 salamander/gecko 混淆一律整节点跳过并打日志，而不是写出一行 Surge 解析不了或必然握手失败的配置。
- **TUIC UDP 承载互斥**：`udp-over-stream` 开启时不再输出 `udp-relay-mode`（clash）/ `udp_relay_mode`（sing-box）—— 两者是同一件事的两种实现，同时给出时内核只会认其中一个。

## 4. 已知未覆盖字段（明确记录，避免误以为“已支持全部”）

| 协议 | 未覆盖字段 | 原因 / 后续 |
|---|---|---|
| Hysteria2 | `ports` / `hop-interval`（端口跳跃）、`obfs.min_packet_size` / `max_packet_size`（Gecko） | mihomo 为字符串端口列表，sing-box 为 `server_ports` 数组 + 时长，需专门转换与校验；当前不做静默降级 |
| TUIC | `heartbeat-interval` / `heartbeat`、`max_udp_relay_packet_size`、`fast-open`、`disable-sni`、`max-open-streams` | 单位/语义在 mihomo 与 sing-box 间不一致，需逐项转换；不影响主流 TUIC 节点 |
| SS | 外部插件（`shadow-tls` / `kcptun`） | sing-box 需要外部二进制，SubHub 不负责安装；当前仅内置 obfs-local / v2ray-plugin |
| VLESS | `packet-encoding` / `encryption` / ECH | 现有导出使用 xray 默认值；待有明确需求再补 |
| VLESS（Surge） | 整协议 | Surge 类型表里没有 `vless`，也没有 `flow=` / REALITY 参数；导出整节点跳过并提示改用 clash-meta / sing-box |
| Hysteria2（Surge） | `up`（上行带宽）、`ports` / `hop-interval` | Surge 只有 `download-bandwidth=` 与 `port-hopping=`（后者尚未建模） |
| TUIC（Surge） | `congestion-controller`、`udp-relay-mode`、`udp-over-stream`、`reduce-rtt` | Surge 的 TUIC 行只暴露 `uuid` / `password` / `version`，其余 QUIC 调参无对应键 |
| 通用（Surge） | `alpn` 多值 | Surge 的 `alpn=` 多值要加引号且要求 iOS 5.20+/Mac 6.7+，当前只在单值时输出 |
| WireGuard（Surge） | 整协议 | 需要独立 `[WireGuard]` 段（`private-key` / `peer = (...)`），与 `[Proxy]` 单行模型不同，暂未实现 |

## 5. 对应测试

| 能力 | 测试 |
|---|---|
| SS 插件解析 | `parse::tests::ss_plugin_parsed_from_uri_clash_and_singbox` |
| SS 插件导出 | `export::tests::ss_plugin_exported_to_clash_singbox_and_surge_obfs` / `ss_v2ray_plugin_is_skipped_in_surge` |
| Hysteria2 obfs / up/down / ALPN | `parse::tests::hysteria2_obfs_parsed_from_uri_and_singbox` / `hysteria2_up_down_alpn_parsed`；`export::tests::hysteria2_obfs_exported_to_clash_and_singbox` / `hysteria2_up_down_and_alpn_exported` |
| 带宽单位换算（Gbps/kbps） | `parse::tests::hysteria2_up_down_alpn_parsed`（`1 Gbps` → 1000、`500 kbps` → 1） |
| TUIC 高级参数 | `parse::tests::tuic_advanced_fields_parsed_from_uri_clash_and_singbox`；`export::tests::tuic_advanced_fields_exported` |
| TUIC UDP 承载互斥 | `export::tests::tuic_udp_over_stream_supersedes_relay_mode` |
| 通用 ALPN | `export::tests::vless_alpn_exported_to_clash_v2ray_and_singbox` |
| Trojan ws/grpc/h2 传输 | `export::tests::trojan_websocket_transport_exported_to_clash_and_singbox` |
| REALITY | `parse::tests::vless_reality_uri_is_parsed` / `clash_reality_and_fingerprint_are_parsed`；`export::tests::reality_exported_to_clash_singbox_v2ray` |
| Surge 类型与传输边界 | `export::tests::surge_skips_vless_and_unsupported_transports` / `surge_websocket_params_are_flat_and_tls_key_is_vmess_only` / `surge_hysteria2_unknown_obfs_is_skipped`；端到端 `docs/test_subhub.py` 的「Surge type/transport boundaries」 |
| Surge 多协议行 | `export::tests::surge_exports_hysteria2_tuic_anytls_socks_and_http` / `surge_dedupes_names_that_collide_after_sanitizing` |
| 导入同批重复远程 URL | 端到端 `docs/test_subhub.py` 的「same remote URL twice inside ONE import batch」 |
| 指纹覆盖 | `model::tests::fingerprint_includes_ss_plugin_and_hysteria2_obfs` / `fingerprint_includes_reality_keys` |
