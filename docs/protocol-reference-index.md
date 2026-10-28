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

## 3. 解析/导出的特殊约定

- **SS 加密方法**：URI 存 `method`，clash YAML 存 `cipher`；所有导出器统一走 `export.rs::ss_method()`。
- **SS 插件名归一**：clash 侧统一写 `obfs` / `v2ray-plugin`；sing-box 侧统一写 `obfs-local` / `v2ray-plugin`。未知插件名原样透传，选项按 `k=v` 保留。
- **Hysteria2 / TUIC**：模型统一 `up_mbps/down_mbps` 数字；导入 mihomo 的 `"30 Mbps"` 时取数字部分，导出 mihomo 时补 ` Mbps` 单位。
- **ALPN**：URI `alpn=h2,http/1.1` 按逗号拆分；clash 既接受序列也接受逗号字符串；sing-box/xray 均为数组。
- **REALITY 缺公钥**：`is_exportable() == false`，引擎与导出都会跳过，避免生成必然握手失败的普通 TLS 节点。
- **Surge 名称**：导出前 `surge_safe()` 清洗换行 / 逗号 / `=`，防止配置行注入。

## 4. 已知未覆盖字段（明确记录，避免误以为“已支持全部”）

| 协议 | 未覆盖字段 | 原因 / 后续 |
|---|---|---|
| Hysteria2 | `ports` / `hop-interval`（端口跳跃）、`obfs.min_packet_size` / `max_packet_size`（Gecko） | mihomo 为字符串端口列表，sing-box 为 `server_ports` 数组 + 时长，需专门转换与校验；当前不做静默降级 |
| TUIC | `heartbeat-interval` / `heartbeat`、`max_udp_relay_packet_size`、`fast-open`、`disable-sni`、`max-open-streams` | 单位/语义在 mihomo 与 sing-box 间不一致，需逐项转换；不影响主流 TUIC 节点 |
| SS | 外部插件（`shadow-tls` / `kcptun`） | sing-box 需要外部二进制，SubHub 不负责安装；当前仅内置 obfs-local / v2ray-plugin |
| VLESS | `packet-encoding` / `encryption` / ECH | 现有导出使用 xray 默认值；待有明确需求再补 |

## 5. 对应测试

| 能力 | 测试 |
|---|---|
| SS 插件解析 | `parse::tests::ss_plugin_parsed_from_uri_clash_and_singbox` |
| SS 插件导出 | `export::tests::ss_plugin_exported_to_clash_singbox_and_surge_obfs` / `ss_v2ray_plugin_is_skipped_in_surge` |
| Hysteria2 obfs / up/down / ALPN | `parse::tests::hysteria2_obfs_parsed_from_uri_and_singbox` / `hysteria2_up_down_alpn_parsed`；`export::tests::hysteria2_obfs_exported_to_clash_and_singbox` / `hysteria2_up_down_and_alpn_exported` |
| TUIC 高级参数 | `parse::tests::tuic_advanced_fields_parsed_from_uri_clash_and_singbox`；`export::tests::tuic_advanced_fields_exported` |
| 通用 ALPN | `export::tests::vless_alpn_exported_to_clash_v2ray_and_singbox` |
| REALITY | `parse::tests::vless_reality_uri_is_parsed` / `clash_reality_and_fingerprint_are_parsed`；`export::tests::reality_exported_to_clash_singbox_v2ray` |
| 指纹覆盖 | `model::tests::fingerprint_includes_ss_plugin_and_hysteria2_obfs` / `fingerprint_includes_reality_keys` |
