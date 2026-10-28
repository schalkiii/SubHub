# Changelog

本项目遵循阶段式交付，每个 P 阶段对应一组功能闭环，每轮代码审计（Round）记录具体改动。

## [P23] 第二轮复核增强（导出命名 / Top-N / 备份保真 / 传输补全）

### Fixed

- **导出名去重与既有后缀冲突**：`ensure_unique_names` 现在把生成的 ` #N` 名称也登记到已用集合。此前 `["A", "A", "A #2"]` 会产出两个 `A #2`，客户端报 `duplicate name` 拒绝整份配置。
- **Top-N 只选可用节点**：`merge_export`、`/sub` 与 `/api/nodes/top` 在评分前先过滤 `is_usable()`（可导出且未判死），避免死节点 / Other 类型占用名额后被 `export_filter` 丢弃，最终不足 N 个。
- **备份保留健康与用量元数据**：`SubExportDoc` 升级到 `version: 2`，`SubExportItem` 新增 `last_checked_at` / `last_updated_at` / `last_healthy_at` / `last_error` / `upload` / `download` / `total` / `expire`；导入时原样恢复（旧备份缺字段时回退到原行为），流量卡片、错误状态与「连续 N 天无可用」计时不再在备份恢复后丢失。
- **Gist 扩展名与所有者**：上传 Gist 时按格式使用 `subscription.yaml` / `.json` / `.conf` / `.txt`，raw 地址使用 Gist 响应里的实际 owner，避免 v2ray/sing-box/surge 内容伪装成 yaml、或多账号 token 导致 raw URL 指向错误用户。
- **引擎进度 host 错位**：`EngineProgress` 直接携带 `server`，SSE 不再用回调计数反查输入列表；批次中出现不可导出节点（不回调）时后续节点的 host 不会错位。
- **VMess 端口与 alterId**：字符串端口 `"0"` 会被丢弃（此前直接 parse 成 0）；数字形式 `aid` 也能解析并做 u32 范围校验。
- **h2 / HTTP 传输补全**：解析 clash `h2-opts` 与 sing-box `transport.type=http`（host 数组），并导出到 clash `h2-opts`、v2ray `httpSettings`、sing-box `transport.type=http`。
- **REALITY 强制 TLS**：clash 导入时即使源里写 `tls: false`，只要存在 `reality-opts` 也按 TLS 处理。
- **订阅列表错误态**：`loadSubs` 请求失败时显示「订阅列表加载失败：…」，不再误显示「还没有订阅」。
- **分享 URL 重复筛选**：同一字段（q/region/type）出现多条 include 筛选时只保留第一条并提示，避免生成服务端无法反序列化的重复键 URL。
- **DB 读取顺序**：`load_all` 增加 `ORDER BY rowid`，重启后订阅顺序保持稳定。

### Changed

- 移除未使用的直接依赖：`tower`、`tower-http`（server）、`anyhow`（core/server）。

### Verification

- `cargo test -p subhub-core -p subhub-server`：**110 个测试全通过**（core 90 + server 20）。
- `cargo clippy --release -p subhub-core -p subhub-server --all-targets -- -D warnings`：0 警告。
- 隔离 DB 端到端：`docs/test_subhub.py` ALL CHECKS PASSED；额外验证 Top-N 不选中死节点、备份健康字段往返、分享 URL 重复筛选提示与订阅列表错误态（浏览器）。

## [P22] 全量复核修复（数据保护 / 协议保真 / 进度与文档一致性）

### Fixed

- **刷新返回 0 节点不再清空订阅**：`do_refresh_one` 遇到 200 但解析出 0 个节点时，按失败处理（写入 `last_error` 并保留原有节点），避免上游一次空响应/格式变更就把整个订阅的节点与测速结果清空。
- **增量刷新保留全部测速字段**：`incremental_update` 现在同步保留 `tcp_latency_ms` / `http_latency_ms` / `consecutive_failures`。此前每次刷新都会把 TCP/HTTP 两列清空，并把自动移除的连续失败计数归零。
- **重复添加只测新增节点**：`merge_subscriptions_by_source` 额外返回 `fresh_nodes`，`POST /api/subscriptions` 只对真正新增的节点跑自动测速。此前重复添加同一 URL 会对该订阅全部节点重测，数千节点场景耗时与网络压力巨大。
- **TCP-only 健康检查不再误判未测节点**：配置引擎时，`run_health_check_core` 只跑裸 TCP；TCP 成功时保持上一轮判定（`available == None` 仍为未测），不再把未测/可用节点改成不可用，也不累加连续失败计数。只有 TCP 失败才降级。
- **SSE 测速进度可达 100%**：带宽探测已关闭，进度总量由 `节点数 × 3` 改为按实际阶段计（无引擎 `×1`、有引擎 `×2`）；前端在收到 `done` 时强制收尾到 100%，引擎整体起不来时也不会卡在中间百分比。
- **SS 加密方法跨格式一致**：v2ray / sing-box / surge 导出统一通过 `ss_method()` 读取 `method` 或 `cipher`。此前 YAML 导入的 SS 节点在以上格式中会回退成默认 `aes-256-gcm`，导出后无法连接。
- **Hysteria2 / TUIC 强制 TLS**：解析时标记 `tls=true`；sing-box 导出强制输出 TLS 块，避免整份配置校验失败。
- **补全 VLESS REALITY**：模型新增 `reality` / `reality_public_key` / `reality_short_id`；支持 URI（`security=reality&pbk=&sid=`）与 clash YAML（`reality-opts`）解析；导出到 clash-meta（`reality-opts` + 缺省 `client-fingerprint: chrome`）、sing-box（`tls.reality` + `utls`）、v2ray/xray（`security: reality` + `realitySettings`）。缺公钥的 REALITY 节点按不完整处理，不再导出成必然失败的普通 TLS。
- **WebUI 出口/解锁按钮结果判定**：出口检测改读 `outbound_country`、解锁检测改读摘要字段 `unlock`。此前字段名错配导致两个功能成功后仍提示“未检测到”。
- **`/api/proxies` 排序参数**：`desc` 同时接受 `1` 与 `true`（此前文档示例 `desc=1` 会触发 400）。
- **`top_n=0` 语义**：显式传 `0` 现在表示“全部节点”并覆盖全局 Top-N；只有参数缺省才回落全局设置（与 README 一致）。
- **`/api/proxies` / `/api/nodes/top` 的 `region` 口径**：统一为 `effective_region()`（实测出口优先），与列表列/筛选/地区分布一致。
- **指纹补全**：VMess/VLESS/TUIC 在存在 ws `host`、gRPC `service_name`、REALITY 参数时纳入指纹（空值不追加，避免给无这些字段的节点制造新指纹）。同服务器同 UUID 不同 Host/service 的节点不再被合并去重。
- **sing-box 导出细节**：uTLS 改为 `utls: { enabled, fingerprint }` 对象；SOCKS5/HTTP 导出用户名；HTTP 443/`tls` 节点导出 TLS。
- **WebUI 内嵌兜底**：静态资源改为「磁盘优先（`SUBHUB_WEBUI_DIR` → 源码 `webui/` → exe 同级）→ 内嵌副本兜底」；exe 分发到无源码的机器后 WebUI 不再 404。开发时仍保持“改前端重启即生效”。
- **文档/脚本同步**：`docs/test_subhub.py` 改为消费 `/api/speedtest` 的 SSE 流；README 修正进度总量、带宽状态、排序参数、设置字段，并新增协议支持矩阵与 WebUI 分发说明。

### Changed

- WebUI 合并/导出页移除“最小带宽”输入（当前不采集新带宽，阈值只能作用于历史数据）；后端 `min_bandwidth_bps` 字段保留用于兼容旧分享链接。
- 移除 `region()` 中过短的 `hong` 子串匹配（避免 `chongqing` 等被误判为 HK）。

### Verification

- `cargo test -p subhub-core -p subhub-server`：**102 个测试全通过**（core 84 + server 18）。
- `cargo clippy --release -p subhub-core -p subhub-server --all-targets -- -D warnings`：0 警告。
- 隔离 DB 启动 server 后运行 `docs/test_subhub.py` 端到端：ALL CHECKS PASSED。

## [P20] 自动清理无可用订阅 + 配置备份导出导入

### Added
- **连续 N 天无可用节点自动清理订阅**：新增设置项「无可用天数」（`remove_sub_after_days`，0 = 关闭，建议 30）。某订阅连续 N 天没有任何可用节点（即当前无任何 `available == Some(true)` 节点，且距最后一次有可用节点已超过 N 天）将被自动删除。判定基准为 `SubscriptionHealth.last_healthy_at`（在 `persist_results` 写出任一可用节点时刷新；从未健康过的订阅回退到 `last_checked_at`）。清理在「定时刷新」「节点健康重测」每轮结束时自动执行，也可 `POST /api/subscriptions/prune` 手动立即清理。
- **配置备份导出 / 导入**：设置页新增「配置备份」面板。
  - `GET /api/settings/export`：导出当前全局设置（代理 / 刷新 / 引擎 / Top-N / 外部访问 / GitHub 账号等）为带下载头的 JSON 文件，便于备份或迁移。出于安全，导出**不含** GitHub token 明文。
  - `POST /api/settings/import`：接受同一份设置 JSON 做**部分更新**（仅应用提供的字段），可附带 `github_token` 写回。设置读写逻辑抽离为 `build_settings_resp` / `apply_settings` 复用，避免与 `GET/POST /api/settings` 行为分叉。

### Changed
- `SubscriptionHealth` 新增 `last_healthy_at: Option<u64>` 字段（随订阅 JSON 快照持久化）。

## [P21] 订阅健康度增强：使用量展示 / base64 链接导入 / 回环虚假测速排除

### Added
- **订阅使用量展示**：自动解析订阅的 `Subscription-Userinfo` 响应头与 clash `info` 块（`download` / `upload` / `total` / `expire`），在「订阅管理」每张订阅卡片上显示流量用量进度条、已用 / 总量与到期日；无使用量信息的订阅自动隐藏。数据随订阅抓取 / 刷新自动刷新。
- **base64 形式订阅链接自动导入**：「添加订阅」输入框除普通 `http(s)://` 链接外，现在会自动识别**以 base64 形式分享的订阅链接集合**（解码后为多行订阅 URL 列表），逐条展开添加，无需手动展开 base64。

### Fixed
- **排除回环 / 本地地址的虚假测速**：节点 `server` 为回环 / 本地 / 未指定地址字面量（如 `127.0.0.53`、`127.0.0.1`、`::1`、`0.0.0.0`、`localhost`）时，绕过真实网络、测速会返回虚假带宽 / 延迟（如 6000+ MB/s）。`persist_results` 写回时跳过这类节点，标记为「未测」以免污染健康度与速度列；域名形式的节点不受影响。
- **速度列对回环节点标注「不适用」**：节点列表与实时测速进度中，回环节点的「速度」一律显示「不适用 / —」而非误导性的虚假大数值。实时进度（`progress` SSE 事件）新增 `server` 字段，后端在 TCP 阶段即对该类节点置空带宽、不再向前端广播 6000+ MB/s 的本地内存拷贝吞吐；前端 `isLoopback()` 与后端 `is_loopback_server()` 对齐判定。

### Changed
- 复用 `core::b64_decode`（多变体 base64 解码），`add_subscriptions` 与订阅内容解析共用同一套解码逻辑。

## [P21.1] 修复测速引擎启动参数（mihomo 用 -f 而非 -c）

### Fixed
- **测速引擎静默失效**：`engine.rs` 拉起外部引擎时硬编码 `-c` 传配置文件路径，但 clash-verge-rev 自带的 `verge-mihomo.exe`（mihomo 系）只认 `-f`，`-c` 会直接报 `flag provided but not defined: -c` 并退出。结果每个节点的引擎探测都失败、整体回退成 TCP-only 假测速——节点列表里 `bandwidth_measured` 全为 `false`，「ping + 带宽」只是 TCP 假信号（如 香港_99 显示 ≈0.39 Mbps 假带宽却真实不可用）。现在按引擎二进制名自适应选参：文件名含 `mihomo`/`clash` 用 `-f`，否则（sing-box）用 `-c`。修复后引擎能正常拉起并完成真实代理级探测，与 clash-verge 行为对齐。

## [P21.2] 带宽列区分「实测」与「估算」

### Changed
- **速度（带宽）列一眼区分真假带宽**：节点列表与实时测速进度中，「速度」单元格新增语义徽标——`实测`（绿色，= `bandwidth_measured: true`，经测速引擎真实代理下载测得、可信）、`估算`（琥珀色，= TCP 吞吐估算 / 未配置或未启用引擎 / 未实测，不代表真实可用带宽）、`回环`（灰色，= 本地地址测速无意义）。此前仅靠 `≈` 前缀区分，不够直观；现用颜色 + 文字标签，避免把「TCP 能连」误当成「节点可用」。列头 `title` 补充图例说明三种状态含义。
- 新增前端辅助函数 `bwBadge(p)`，节点列表与实时进度共用同一套渲染逻辑。

## [P21.3] 修复测速引擎 `mode: global` 导致全部节点测速失败

### Fixed
- **测速引擎统一「测不到速度」的根因**：`engine.rs` 为 mihomo 生成的单节点配置用 `mode: global`，但 clash-meta 的 GLOBAL 模式会忽略 `rules`、使用一个默认选为 `DIRECT` 的全局选择器，于是 mihomo 收到代理请求后**直接直连目标**、从不经被测节点转发。表现为「所有节点测速统一失败 / 带宽列全空」，与 clash 能正常测速形成反差。改为 `mode: rule` 并保留 `rules: - MATCH,PROXY`，强制所有流量经 PROXY 组（即被测节点）转发，与 clash-verge 行为对齐。实测日志由 `dial GLOBAL ... --> 目标直连` 变为 `match Match using PROXY[节点]`，确认节点真正生效。
- **移除调试期临时落盘**：删除 `last_engine_config.yaml` 与 `mihomo_engine.log` 的临时诊断写入，引擎进程 stdout/stderr 恢复为 `Stdio::null`，避免每次测速（每个节点一个进程）在 cwd 产生海量日志文件。

## [P21.4] 修复 SS 节点被误判为「不完整」导致全部 0.0MB/s

### Fixed
- **0.0MB/s 的真正根因之一：SS 节点被整体丢弃**：`model.rs` 的 `is_exportable()` 对 SS 类型要求 `method` 字段非空，但**从 clash-meta YAML 订阅导入的 SS 节点其加密方法存在 `cipher` 字段、`method` 为 null**（只有 SIP002 URI 解析的节点才存到 `method`）。于是海量 SS 节点被 `is_exportable()` 误判为不完整 → 在 `to_clash_meta`（导出 / 引擎配置生成）和 `engine.rs::with_engine` 两处被整体跳过 → 引擎根本不测这些节点、直接回退 TCP 吞吐估计（≈0.0MB/s），`bandwidth_measured` 全为 `false`。修复：`is_exportable()` 对 SS 改为「`password` 且（`cipher` 或 `method` 任一非空）」；`to_clash_meta` 的 SS 分支同步从 `cipher.or(method)` 取值写 `cipher`，与解析侧两种存储位置对齐。
- **测速目标不可达 / 大分片超时**：`resources.rs` 的存活 / 带宽探测目标原为 Hetzner，本环境被封锁（`http=000`），且带宽分片以 10MB 优先，慢节点 30s 下不完 → 全失败回退 TCP 假测量。改为 **OVH 分片优先（1MB 档在前、10MB 档留给快节点、Hetzner 放最后兜底）**，存活目标改用 `https://proof.ovh.net/files/1Mb.dat`，让探测在多数网络下真正可达并能产出真实带宽。

### Verification
- 端到端验证：用 `to_clash_meta` 生成的引擎配置对 Oracle SS 节点（129.146.200.6）实测 `measured=True`、真实带宽 110–130 KB/s；手写与 `to_clash_meta` 两种路径均能在节点出口健康时下载到 1MB 分片。廉价 / 受限出口节点（Cloudflare 前置、worker 前端、抖动 AWS SS 等）仍会测不到，属节点质量问题，正确回退为 TCP 估计值，不再被误标「可用」。

## [P21.6] 地区列/筛选一致 + 导出按实测出口重命名

### Fixed
- **地区筛选与列表显示不一致**：节点列表「地区」列优先显示实测出口地区 `outbound_country`，但「地区筛选」与仪表盘「地区分布」仍用从名字猜测的 `region()`，导致显示 HK 的节点按「地区=HK」筛选却被排除。新增 `Proxy::effective_region()`（优先 `outbound_country`，回退 `region()`），列表筛选、关键字搜索的 region 维度、仪表盘地区分布，以及 `apply` transform 的 `region` 过滤器（影响 `/sub` URL 与手动导出的 region 过滤）统一改用它，四处与列表列一致。

### Added
- **导出按实测出口地区重命名**：`/sub` 在线订阅与「导出」两种渠道在导出前，对「名字未识别地区（`region()`==OTHER）但实测出口地区已知」的节点，把出口国家码前置到名字（如「其他」→「HK 其他」），使 Clash Verge 按名字匹配地区时不再把它们归为「其他」。重名时追加 ` #2`/` #3` 后缀去重；仅在导出副本上修改，绝不改动落库数据（`export_str` 的 `ensure_unique_names` 仍兜底去重）。新增回归测试 `region_fixup_renames_other_with_outbound_and_dedups` 与 `effective_region_prefers_outbound_country`。

## [P21.5] 修复「仅未测过」漏测无延迟数据的节点

### Fixed
- **「仅未测过」误漏节点**：选「仅未测过」测速时，部分 tcp / http 延迟均为 "— —"（从未拿到延迟数据）的节点未被纳入测试。根因：`apply_mode("untested")` 原以 `last_tested_at.is_none()` 判定未测过，但这些节点可能已被测速流程跑过（如 TCP-only 节点健康重测、添加时 TCP 超时）并被写入时间戳，只是那一轮没产出延迟值——`persist_results` 对每个经过的节点都会写 `last_tested_at`，导致「有时间戳但无数据」与「已测得数据」被混为一谈。现改为以「确实没有延迟数据」为准（`tcp_latency_ms == None && http_latency_ms == None`），使 UI 上显示 "— —" 的节点必定被「仅未测过」纳入。手动勾选测试不受影响（手动路径不走该过滤）。新增回归测试 `apply_mode_untested_catches_data_less_nodes`。

## [P19] 代码审计修复（R1 Round）

### Fixed
- **SSR 节点不再伪装成 SS 导出**：此前 `parse_ssr` 把 SSR 解析为 `ProxyType::Ss`，导出成 `type: ss` 后 mihomo/Clash.Meta 会按 SS 握手必然失败且无任何提示。现改为直接跳过 SSR 节点并打日志说明原因（mihomo 不支持 SSR）。
- **vmess 端口范围校验**：`parse_vmess` 里端口 `as u16` 直接截断（如 70000 → 4464 仍当作合法节点导出）。现对端口加 `(1..=65535)` 范围检查，超范围丢弃该节点。
- **v2ray 导出静默丢节点**：`to_v2ray_json` 过滤掉 hysteria2/tuic/socks5/http/wireguard 等 v2ray-core 无法表示的节点时没有任何提示，用户导出 v2ray 会发现节点凭空减少。现统计并 `eprintln!` 告警（与 sing-box 分支一致）。
- **Gist 上传并发竞态**：并发 `POST /api/gist/upload` 各自用旧 `gist_id` 做 PATCH，可能创建出多个孤儿 Gist、或让「稳定的远程拉取地址」漂移。新增 `state.gist_lock`（`tokio::sync::Mutex`，因上传过程跨 await 网络请求，std MutexGuard 非 Send）串行化整个上传流程。
- **订阅改名缺长度上限**：`PATCH /api/subscriptions/:id` 仅校验非空，未限制长度。现增加 256 字符上限，超长返回 `400 name_too_long`。
- **仪表盘 GET 污染趋势时间轴**：`GET /api/dashboard` 每次调用都 push 一个趋势点，WebUI 高频轮询会把趋势图时间轴按轮询频率刷满。现限频为最多每 60 秒记录一个趋势点。

### Audit（本次审计其余发现，暂未改动，记录备查）
- `parse_hy2` / `parse_tuic` 未解析 `obfs`/`congestion-controller`/`alpn` 等参数，部分混淆/拥塞控制配置导出后可能连不上（改动面大，暂缓）。
- `region()` 的 `NAME_TABLE` 用 `contains` 子串匹配，短词（如 `hong`）可能误判地区分组（暂缓，避免引入行为回归）。
- GitHub token 明文持久化到 SQLite meta 表（未加密；`get_settings` 已不回传明文）。
- `db.save_all` 采用 DELETE 全表 + 全量 INSERT，进程在中间崩溃有丢失窗口（有 `persist_lock` 串行保护）。



### Added
- **订阅重命名**：订阅管理页每张卡片新增「重命名」按钮，点击后名称原地变为输入框，支持「保存 / 取消」（回车提交、Esc 取消），通过新增 `PATCH /api/subscriptions/:id` 提交新名称；空名称被后端拒绝（`400 empty_name`），不存在的 id 返回 `404`。
- **仪表盘「当前订阅」紧凑排布**：仪表盘的订阅列表复用订阅管理页卡片结构，改为紧凑网格（`.sub-card.compact`，省略时间脚注与错误行），与订阅管理页视觉一致。
- **仪表盘「地区分布」改为饼图**：地区分布由横向条形图改为与「类型分布」一致的圆环饼图（`conic-gradient` + 图例），新增 `region-donut` / `region-legend` 渲染，按地区节点数降序着色。

### Changed
- 新增 `patchJson` 前端请求辅助（与 `postJson` 对称）。
- 抽象 `renderDonut(donutId, legendId, obj, colorMap)` 复用类型/地区两种饼图渲染；移除不再使用的 `renderBars`。

## [P18] 局域网访问 + GitHub Gist 远程同步

### Added
- **局域网 / 公网绑定**：服务默认绑定 `0.0.0.0`（可用环境变量 `SUBHUB_BIND` 覆盖），其他设备可直接通过本机局域网 IP（如 `192.168.10.111:3005`）拉取订阅。
- **外部访问地址设置**：设置页新增「外部访问（局域网/公网）」面板，可填写对外可达主机；生成订阅网址时按 `external_host → 本机局域网 IP（UDP 探测出口）→ 当前窗口主机` 回退，保证局域网/公网设备可拉取。
- **GitHub Gist 远程同步**：设置页新增「GitHub Gist 远程同步」面板（账号 + token，token 不回传明文）；「合并导出」页新增「上传到 Gist」按钮，将当前订阅（与 `/sub` 完全一致）上传到 Gist，返回 `gist.githubusercontent.com/.../raw` 远程拉取地址，供远程设备直接订阅。
- **`POST /api/gist/upload`**：复用 `build_sub_content` 保证产物与 `/sub` 一致；凭据取「请求体 > 设置页持久化值」；首次创建、之后复用 `state.gist_id` 更新同一 Gist（PATCH 返回 404 自动重建），并持久化 `gist_id`。

### Changed
- `SettingsReq`/`SettingsResp` 新增 `bind_addr`、`external_host`、`github_user`、`has_github_token`、`lan_ip` 字段；`AppState` 新增对应状态与 `detect_lan_ip()` 出口 IP 探测。
- 抽象 `buildSubParams()` 供「生成地址」与「上传 Gist」复用同一份 `/sub` 查询参数；新增 `btn-gist-upload`、`btn-copy-gist` 前端处理器。
- **Gist 改为公开 + raw 带文件名**：`public` 改为 `true`（私密 Gist 的 raw 在手机端匿名拉取会被拒），raw URL 改为 `.../raw/subscription.yaml`（避免裸 `/raw` 的 302 跳转、并让 GitHub 返回正确 `text/yaml`，部分移动端客户端如 FIClash 才能识别格式）。

### Added（同轮补充 · 仪表盘带宽）
- **仪表盘展示平均带宽 / 最佳带宽**：`DashboardResp` 新增 `avg_bandwidth_bps` / `best_bandwidth_bps`，由 `dashboard()` 在聚合延迟的同时累加带宽（仅纳入有限正值、过滤脏 NaN），前端新增两张卡片并以 Mbps/Gbps 友好单位显示。


## [P16] 关闭最小化到托盘 + 修复任务栏图标

### Added
- **系统托盘常驻**：应用启动后在系统托盘显示图标，左键单击恢复窗口；右键菜单含「显示 SubHub / 退出」。
- **关闭最小化到托盘（非退出）**：点右上角关闭按钮不再结束进程，而是隐藏窗口到托盘（`on_window_event` 拦截 `WindowEvent::CloseRequested`，`prevent_close()` + `window.hide()`）；仅托盘菜单的「退出」会真正终止应用（含后台测速引擎线程）。

### Fixed
- **任务栏图标显示不正常**：根因是 `app/icons/` 下 `icon.ico` / `icon.png` / `icon_preview.png` 三个图标文件**均为 0 字节空文件**，Tauri 无可加载图标，只能回退为默认/空白图标。已用 `tauri icon` 从源图重新生成全套有效图标（`icon.ico` 8561B、`icon.png` 4891B、`icon_preview.png` 19948B 等），并在 `WebviewWindowBuilder` 上显式 `.icon(app.default_window_icon())` 关联应用默认图标，确保 Windows 任务栏图标正常显示。同时清理了与桌面程序无关的 `android/`、`ios/` 移动端图标目录。

### Changed
- `app/Cargo.toml`：`tauri` 启用 `tray-icon` feature（托盘所需）。

## [P15] 修复已配置引擎时的「假绿」误报（TCP 回退）

### Fixed
- **已配置引擎时不再回退裸 TCP 可用性**：P13 以「全局 `engine_active`（本批任一节点探测成功）」为开关，在「本批全部探测失败」时会退回裸 TCP 可用性，导致 `35.187.156.27:443`（Google 的 443 端口，TCP 必可连）这类「端口可连、代理隧道不通」的节点被误判为可用+有速度（用户侧 TWN vless 节点即此例，clash-verge 同款 YAML 实测不通）。现改为：**只要 `SUBHUB_ENGINE_BIN` 已配置且本批确有节点经引擎 gstatic generate_204 实测成功**（`engine_usable`），逐节点以引擎实测（`h.is_some()`）为可用性权威，引擎未能确认的节点一律判为不可用，彻底消除此类误报，行为与 clash-verge 一致。
- **修复「引擎整体故障时 mass-red」回归**：上一提交去掉了「引擎全失败则回退 TCP」的保护，导致一旦某次测速引擎临时没正常工作（超时过严 / verge-mihomo 未起），所有节点 `h` 全为 `None`、被整批判红。现恢复保护——`engine_usable` 为假（引擎未配置，或本批 0 个探测成功）时回退裸 TCP，保证不会 mass-red 让用户完全无法使用。该回退仅作为「引擎临时起进程探测」的兜底，正常测速（引擎工作）不受影响、不产生假绿。

### Added
- 实证验证：以真实 verge-mihomo 引擎对 TWN vless 节点做 gstatic 探测，结果为 `None`（不可用），确认误报来自 TCP 回退而非引擎判定错误。

### Fixed（同轮补充）
- **「测速选中」对含逗号 fingerprint 的节点静默失效**：`/api/speedtest` 的 `ids` 原用逗号拼接分隔，而 vless+ws 等节点的 fingerprint 本身含逗号（如 `path` 字段），被逗号切碎后永远匹配不到，导致这类节点「选中测速」实际测了 0 个、残留旧绿。`ids` 改为 URL 编码的 **JSON 数组**（`ids=["<fp1>","<fp2>"]`），WebUI 侧同步改为 `JSON.stringify(选中的指纹列表)`，彻底规避分隔符冲突。
- **判定不可用时清除旧 ping/带宽**：节点被判定为不可用（尤其是修复后由引擎实测纠正的「旧绿」节点）后，立即清空 `latency_ms` / `download_speed_bps` / `bandwidth_measured`，避免上一轮 TCP 回退留下的假延迟、假速度继续误导界面。仅当本次确有测量值（引擎带宽 / 延迟）时才写回。

### Fixed（同轮补充 · 进度条体验）
- **根因：前端改动一直「看似不生效」**：WebUI 由后台 axum 在运行时通过 `ServeDir` 直接从 `webui/` 目录提供（**不嵌入 exe**）。此前多轮改 CSS/JS 后「问题依旧」，真实原因是 **app 进程始终未关闭**（编译时报「拒绝访问、无法覆盖 exe」即为旧进程锁文件的铁证），旧进程持续加载旧前端，与代码改动无关。已让用户关闭并重启后生效。
- **加固：静态响应加 `no-cache`**：在 `ServeDir`（fallback 静态服务）外层包 `SetResponseHeaderLayer`（`Cache-Control: no-cache`），避免 webview 缓存旧 `app.js/style.css`，确保修改后重启必加载新前端。注意：该头加在 Router 顶层对 `fallback_service` 的响应**不生效**，必须直接包在 `ServeDir` 之上。
- **进度条悬浮失效（滚动后看不到）**：P12 用 `position: sticky`，但 `.content` 自身是滚动容器（`overflow:auto`），sticky 会随内容滚出视口。改为 `position: fixed` 悬浮视口顶部（`top:0`），并给 `body.has-progress .content` 加 `padding-top` 避让（JS 在显示/收起进度条时切换 `body` 的 `has-progress` 类）。
- **进度条来回跳（三千多↔八千多）**：后端三阶段（TCP / HTTP / BW）`done` 严格单调、分母 `total` 恒定，不会回退；观感「来回跳」主要来自横幅里那个 CSS 无限左右滑动的「不确定进度条」动画（`@keyframes prog`）。已改为测速时隐藏该动画条，仅保留确定性进度条；并额外让前端进度只增不减（记录「已见最大 `done`」与首次确定的 `total`）作为保险。

## [P13] 节点多选测速 + 上次测速列 + 可用性对齐 gstatic

### Added
- **节点多选单独测速**：节点列表每行新增复选框（表头「全选本页」可一键勾选），勾选后点「测速选中」只测这些节点（`GET /api/speedtest?ids=...`，`ids` 为逗号分隔的 `fingerprint`），无需重测整份列表。
- **列表「上次测速」列**：节点表新增一列显示每个节点上次测速的本地日期时间（取自 `last_tested_at`），未测过的显示「—」，便于判断节点新鲜度。
- **可用性以 gstatic generate_204 实测为准**：配置了 `SUBHUB_ENGINE_BIN` 时，节点「可用」改由引擎经节点隧道对 `https://www.gstatic.com/generate_204`（期望 204）的真实代理级 HTTP 探测决定，与 clash-verge 的连通性测试一致。

### Changed
- 消除「假绿」：此前只要 TCP 能连就标为可用，导致「SubHub 显示可达、clash-verge 却测不通」的节点被误判为可用。现引擎已配置时，TCP 通但协议层（TLS/SNI/传输/代理握手）不通的节点会被正确判为不可用；引擎未配置时才使用裸 TCP 结果。

## [P14] 单节点 YAML 复制（跨客户端 1:1 比对调试）

### Added
- **「复制节点 YAML」按钮**（节点表每行操作列「Y」）：调用 `GET /api/proxy-yaml?fp=<fingerprint>`，返回该节点经 `to_clash_meta` 序列化的 clash-meta 配置——与 SubHub 测速引擎、导出**同源同产物**。一键复制到剪贴板，可粘进 clash-verge-rev 等客户端直接测同一节点。
- 用途：当「SubHub 能通、clash-verge 不通」且两侧使用同一 mihomo 二进制时，用这份精确 YAML 在 clash-verge 1:1 复现——若通，则分歧在 clash-verge 的订阅预处理/缓存；若仍不通，则提供可复现样本继续排查协议层问题。

## [P12] 导出质量筛选 + 进度条悬浮常驻

### Added
- 导出算子新增数值质量阈值：`min_bandwidth_bps`（带宽下限，bps）与 `max_latency_ms`（延迟上限，ms）。导出时排除带宽低于下限或延迟高于上限的节点；**未测速（无数据）节点保留**，避免误删尚未测速的节点。
- 常驻订阅地址 `GET /sub` 新增 `min_bw`（bps）/ `max_lat`（ms）查询参数，与手动导出的数值阈值对应，便于把「只要快节点」编码进订阅网址。
- WebUI「转换算子」面板新增「最小带宽（MB/s）」「最大延迟（ms）」输入项，并自动编码进分享网址。

### Changed
- WebUI 全局进度条改为 `position: sticky` 悬浮常驻：长列表滚动时进度提示始终固定在内容区顶部可见，无需回滚页面即可看到测速 / 导出进度。

### Fixed
- **导出撞名导致配置被拒**：两个不同节点（如不同地区重命名规则把同地区节点都改成同一名字、或不同订阅本就重名）共享显示名时，clash / clash-verge 校验报 `... is the duplicate name` 而整份订阅无法加载。`export_str` 现在统一对代理名去重——首个同名保留原名，后续加 ` #2` / ` #3` 后缀，clash / v2ray / sing-box / surge 全部受益（均按名为键）。

## [P11] 地区识别增强 + 测速进度流式化 + 405 修复

### Added
- `Proxy::region()` 4 级地区识别：国旗 emoji → 中英文国家全名（~90 条，含真实节点库补全）→ 3 字母机场码 → 2 字母国家码 + 安全前缀变体。
- 手动测速 `/api/speedtest` 改为 SSE 流式（`text/event-stream`），实时推送 `progress` / `done` 事件。
- WebUI 测速进度条 + 实时「当前节点 / Ping / 带宽」展示。
- `region()` 单测：常见命名风格解析、前缀变体防误判、国旗 emoji 映射、真实库国家补全。

### Changed
- `/api/speedtest` 由 `POST`（body）改为 `GET`（query：`timeout_ms` / `concurrency` / `mode`）。
- WebUI 地区列取值顺序：`outbound_country || region || "OTHER"`。
- `tcp_ping_all` 进度回调签名为 `Option<&(dyn Fn(TestProgress) + Sync)>`。
- `engine_http_latency` / `engine_bandwidth` 新增 `on_progress` 回调参数（逐节点回报进度）。

### Fixed
- 测速接口 405 Method Not Allowed（路由 POST → GET）。
- **测速进度「到 x/x 不结束」**：原进度仅统计 TCP 阶段，配置引擎后「HTTP 延迟 + 带宽」阶段静默运行、无反馈。现进度总量贯穿三阶段（无引擎 `total=节点数`；有引擎 `total=节点数×3`），引擎两阶段经 `on_progress` 逐节点回报，带 `phase` 字段；进度条真正到 100% 才结束。
- `region()` 曾把 `russia` 误判为 `us`（整词 token 匹配修复）；真实节点库 OTHER 由 1122 降至 701。

## [P10] 全局排序 + 设置页 + 增量测速 + 合并防覆盖
- 节点列表全局排序（跨页）。
- WebUI 集中「设置」页 + 全局 Top-N 单一真相源。
- 手动测速「仅测未测 / 仅失败」模式。
- 合并导入防任意覆盖（按 source 幂等）。
- `DELETE /api/subscriptions/:id` 对不存在 id 返回 404。

## [P9] 质量加固
- 订阅按健康度排序。
- 导出自动去除无效节点（other / 缺字段 / 已测不可用）。
- 修复地区列恒为 `OTHER` 的注入 bug。
- 修复前端 `unlock.summary()` 调用错误。
- clippy 0 警告。

## [P8] 本地订阅地址 + 分页 + 算子分享 + 图标重制
- `GET /sub` 常驻订阅拉取地址。
- 节点列表分页。
- 算子转换图文说明与分享链接。
- 应用图标重制 + 重新编译 exe。

## [P7] SQLite 持久化 + 订阅分组 + 定时刷新
- SQLite 持久化（重启不丢订阅 / 测速结果）。
- 节点按订阅来源归属 + 一键分组视图。
- Resin 式定时自动刷新。

## [P6] 即时测试 + 代理拉取 + 中文打磨
- 订阅添加后自动健康度检测 + 测速。
- 支持通过代理拉取订阅。
- 页面中文翻译打磨。

## [P5] 单个订阅健康度 + 出口 / 解锁 / 带宽 / 趋势 / 熔断
- 逐个订阅健康度 + 刷新。
- 出口地区探测（geo-detect）+ 流媒体解锁判定。
- 带宽测速 + 趋势图 + 坏节点熔断清理。

## [P0–P4] 基础能力
- P0：导入 / 统一模型 / 合并去重 / 仪表盘 / 导出。
- P1：测速引擎（TCP 延迟 + 引擎 HTTP 延迟钩子）。
- P2：Resin 级仪表盘（类型环图 / 地区分布 / 来源 / 延迟卡片）。
- P3：sub-store 式算子管道 + 多格式导出。
- P4：跨平台打包 + Windows 原生二进制验证。
