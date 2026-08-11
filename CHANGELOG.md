# Changelog

本项目遵循阶段式交付，每个 P 阶段对应一组功能闭环，每轮代码审计（Round）记录具体改动。

## [P13] 节点多选测速 + 上次测速列 + 可用性对齐 gstatic

### Added
- **节点多选单独测速**：节点列表每行新增复选框（表头「全选本页」可一键勾选），勾选后点「测速选中」只测这些节点（`GET /api/speedtest?ids=...`，`ids` 为逗号分隔的 `fingerprint`），无需重测整份列表。
- **列表「上次测速」列**：节点表新增一列显示每个节点上次测速的本地日期时间（取自 `last_tested_at`），未测过的显示「—」，便于判断节点新鲜度。
- **可用性以 gstatic generate_204 实测为准**：配置了 `SUBHUB_ENGINE_BIN` 时，节点「可用」改由引擎经节点隧道对 `https://www.gstatic.com/generate_204`（期望 204）的真实代理级 HTTP 探测决定，与 clash-verge 的连通性测试一致。

### Changed
- 消除「假绿」：此前只要 TCP 能连就标为可用，导致「SubHub 显示可达、clash-verge 却测不通」的节点被误判为可用。现当引擎活跃时，TCP 通但协议层（TLS/SNI/传输/代理握手）不通的节点会被正确判为不可用；引擎未配置或本批全部探测失败时退回 TCP 结果，避免误杀全量节点。

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
