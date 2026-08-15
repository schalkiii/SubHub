# Changelog

本项目遵循阶段式交付，每个 P 阶段对应一组功能闭环，每轮代码审计（Round）记录具体改动。

## [P17] 订阅改名 + 仪表盘紧凑化与地区饼图

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
