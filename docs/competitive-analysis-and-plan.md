# SubHub — 竞品拆解与代码复用规划（v2：源码级 + 复用映射）

> 目标：一个用 **Rust** 开发、带 **原生 GUI（Tauri v2）** 的订阅聚合管理工具，集齐三家长处：
> 1. **Resin** 那样漂亮的 WebUI 与全面的展示（仪表盘、节点池、指标、日志）—— 以及**逐个订阅的健康度**
> 2. **BestSub** 的批量添加订阅 + 测速（可用性 / 延迟 / 带宽 / 流媒体解锁 / 出口地区）
> 3. **sub-store** 的合并节点 → 输出新订阅（多源聚合、转换、过滤、重命名、分享链接）
>
> 本版在 P0–P4 MVP 已落地的前提下，补做**源码级拆解**并给出**本次已借用的成熟代码映射**，把"集合百家之长"落到具体文件与函数。

---

## 0. 结论速览（更新）

**没有现成 app 同时满足全部需求。** 三家都是「服务端 / WebUI」形态（Go 或 Node），**没有一个是 Rust，也没有原生 GUI**，且 **Resin 的逐个订阅健康度是它独一份的能力**（BestSub / sub-store 都没有订阅级健康看板）。

| 需求 | Resin | BestSub | sub-store | SubHub 现状 |
|---|---|---|---|---|
| 漂亮 WebUI + 全面展示 | ✅ 最强 | 🟡 现代工具风 | 🟡 强但朴素 | ✅ 借鉴 Resin 主题 + 卡片 |
| **逐个订阅健康度** | ✅ 独有能力 | ❌ | ❌ | ✅ **已实现并验证** |
| 批量添加订阅 | 🟡 | ✅ 核心 | ✅ | ✅ |
| 节点测速（延迟/带宽/解锁） | 🟡 被动 | ✅ 主动+解锁 | ❌ | ✅ 延迟/可用性 + 出口地区 + 流媒体解锁 + 带宽测速 |
| 合并 → 输出新订阅 | 🟡 去重 | ✅ | ✅ 最强 | ✅ 算子管道 + 6 格式 |
| Rust 开发 | ❌ Go | ❌ Go | ❌ Node | ✅ |
| 原生 GUI | ❌ Web | ❌ Web | ❌ Web | ✅ Tauri v2 |

**方向未变**：以 BestSub 的功能骨架为蓝本，用 Resin 的仪表盘美学 + 健康度模型做界面与数据模型，用 sub-store 的合并/算子模型做输出层，整盘用 Rust + 原生 GUI 重写。本轮已把 Resin 健康模型、BestSub 的探测目标与 geo 通道、Resin 主题令牌**直接移植进代码**（见 §3 复用映射）。

---

## 1. 竞品逐项拆解（源码级）

### 1.1 Resin（Resinat/Resin）— 漂亮展示 + **逐个订阅健康度（本次重点）**

- **定位**：高性能智能代理池网关（proxy pool gateway），把海量订阅收敛成一个稳定、可观测的统一出口。
- **技术栈**：核心 **Go 1.25+**；WebUI 用 **Node + Vite**（`webui/`，默认 `http://127.0.0.1:2260`，`RESIN_ADMIN_TOKEN` 登录）；MIT；348 commits，2026-07 仍活跃。

#### 1.1.1 逐个订阅健康度模型（我们主要借鉴的部分）
源文件 `internal/service/control_plane_subscription.go` + `internal/api/handler_subscription.go`：

- `SubscriptionResponse`（API 契约）字段：
  `ID, Name, SourceType`(remote/local), `URL, Content, UpdateInterval,
  NodeCount, HealthyNodeCount, Ephemeral, IncrementalAliveNodes,
  EphemeralNodeEvictDelay, Enabled, CreatedAt,
  LastChecked, LastUpdated, LastError`
- `subToResponse(sub)`：**健康度在响应时派生**——遍历 `ManagedNodes()`，对每个未驱逐节点用 `Pool.MakeHealthyAndEnabledEvaluator()` 判断是否"健康且启用"，分别累加 `NodeCount` / `HealthyNodeCount`；再填 `LastChecked`(ns→RFC3339)、`LastUpdated`、`LastError`。**思路**：健康度不单独落库，而是从运行时节点池实时计算，保证单一数据源、永不失同步。
- `RefreshSubscription(id)`：触发 `Scheduler.UpdateSubscription(sub)` 立即刷新（拉取源 + 重算节点 + 更新时间戳）。
- `HandleRefreshSubscription` → `POST /api/v1/subscriptions/{id}/actions/refresh`，返回 `{"status":"ok"}`。
- `CleanupSubscriptionCircuitOpenNodes`：清理熔断/无出口节点（按 `IsCircuitOpen()` 或 `!HasOutbound() && LastError!=""` 判定）——这是"自动剔除坏节点"的成熟实现。
- `ListSubscriptions` 支持 `?enabled`、`?keyword`（按 id/name/url/source_type 匹配）、`?sort=`(name/created_at/last_checked/last_updated)、分页——前端订阅表格/卡片的筛选排序范式。

> **我们的映射**：`SubscriptionHealth` 字段集直接对照 `SubscriptionResponse`；`recompute()` 对照 `subToResponse()` 的派生逻辑（读时从 `proxies` 计算 node/healthy 计数与延迟）；`refresh_subscription` 端点语义对照 `HandleRefreshSubscription`。区别：Rust 侧用 `MutexGuard` 快照→释放→`await`→重新加锁，规避"持锁跨 await 不满足 Send"的编译错误（见 §3 行 2 注释）。

#### 1.1.2 展示 / 调度
- Dashboard：节点池状态、指标卡（KPI：流量/延迟/健康/租赁）、趋势图、地区/类型分布、结构化请求日志（按平台/账号/目标检索）。
- 左侧菜单：Subscriptions / Platforms（按地区/正则/订阅源过滤出独立节点池）/ 租赁 / 账号（粘性绑定）。
- 被动+主动健康、出站 IP 探测、延迟分析；P2C + 域名感知延迟加权选优；自动熔断。
- 订阅处理：远程 URL / 本地粘贴；sing-box JSON、Clash JSON/YAML、各类 URI、Base64；**跨订阅自动去重并共享健康状态**；热重载、增量刷新、持久化。
- **缺什么**：运行时网关而非订阅管理器——没有主动测速/流媒体解锁检测，没有"导出成新订阅链接"的一键能力，原生 GUI 与 Rust 均无。

### 1.2 BestSub（bestruirui/BestSub）— 批量添加 + **测速/解锁内部（源码级）**

- **定位**：高性能节点检测 / 订阅转换服务，Go，自带 WebUI + API。
- **技术栈**：**Go**；v1.x 单二进制；WebUI 端口 `8080`；测速引擎 **mihomo v1.19.26**；格式转换用 **bestruirui/SubWorker**（取代旧 Subconverter）。

#### 1.2.1 测速/检测内部（我们主要借鉴的部分）
源文件 `internal/core/check/checker/{alive,speed,tiktok,country}.go` + `internal/modules/country/*`：

- **存活检测 `alive.go`**：
  - 目标常量 `Alive.URL = https://www.gstatic.com/generate_204`，`ExptectCode = 204`。
  - `detect()`：用 `mihomo.Proxy(raw)` **经该节点代理**发 `GET`，判 `StatusCode == 204` 即存活；命中则记录延迟 `Delay.Update(ms)`。
  - 信号量限流（`sem := make(chan struct{}, threads)`），线程可配（默认 100）。
- **带宽测速 `speed.go`**：
  - `DownloadUrl = https://speed.cloudflare.com/__down?bytes=104857600`，`UploadUrl = https://speed.cloudflare.com/__up`。
  - `download()`：`io.LimitReader(resp.Body, size)` 丢弃读，按 `bytes/duration` 算速度；`upload()`：POST 一个 `trackingZeroReader`（零填充）按 `bytesRead/duration` 算上行。
  - 支持「达速即停」「跳过已有速度节点」。
- **流媒体解锁 `tiktok.go`**：
  - `detectTikTok()`：先 `GET https://www.tiktok.com/`，若响应体含 `"region":` 返回 `1`（解锁）；否则 `GET https://www.tiktok.com/api/passport/web/region/get/`，命中返回 `2`（IDC 解锁）；否则 `0`。带 UA 头。
  - Netflix/Disney 等同理另有 checker（本项目已借 TikTok 目标常量 + Netflix/Disney/YouTube/ChatGPT 启发式，完整解锁判定已完成，见 §4.1）。
- **出口地区 `country.go` + `modules/country/*`**：
  - `checker/country.go`：对每个未检测节点 `mihomo.Proxy(raw)` 后调 `country.GetCode(ctx, client.Client)`。
  - `modules/country/country.go` 的 `GetCode()`：**遍历注册通道，每个 5s 超时，返回首个非空国家码**。
  - 通道（`modules/country/channel/*.go`，按 `init()` 注册顺序）：
    1. `cloudflare.go` → `CloudflareCDN`：`https://cloudflare.com/cdn-cgi/trace`（`loc=XX` 文本解析）；`CloudflareSpeed`：`https://speed.cloudflare.com/meta`（`country` JSON）
    2. `commen.go` → 内部通用回退
    3. `freeip.go` → `https://free.freeipapi.com/api/json`（`countryCode`）
    4. `ip_sb.go` → `https://api.ip.sb/geoip`（`CountryCode`）
    5. `ipapi.go` → `https://ipapi.co/json`（`cc`）
    6. `ipwho.go` → `https://api.ip.sb/geoip`（`CountryCode`，与 ip_sb 同 URL）
    7. `myip.go` → `https://api.myip.com`（`country`）
    8. `reallyfreegeoip.go` → `https://reallyfreegeoip.org/json`（`country_code`）

> **我们的映射**：alive/speed/tiktok 的**目标常量**原样借用进 `core/src/resources.rs`；geo 的**通道优先级列表 + 解析策略**移植为 `GEO_CHANNELS` + `extract_country()`；`geo_one()` 复刻"每节点起引擎→逐个通道尝试→首个命中即返回"。带宽/解锁判定尚未接引擎，但端点与目标已就位。

#### 1.2.2 合并/转换/输出
合并多订阅、转 clash / mihomo、去重、按规则重命名、按解锁状态分类；保存至本地 / R2 / Gist / WebDAV / HTTP；高度自定义分享；模块化扩展（加一个文件即扩展检测/保存/通知）。
- **缺什么**：Go 非 Rust；UI 工具风非 Resin 级；无原生 GUI；算子灵活度不如 sub-store。

### 1.3 sub-store（sub-store-org/Sub-Store）— 合并 → 输出新订阅的标杆

- **定位**：高级订阅管理器，面向 QX / Loon / Surge / Stash / Egern / Shadowrocket / mihomo / sing-box / V2Ray 等。
- **技术栈**：**Node.js 后端 + 前端**；AGPLv3；1554 commits，极活跃。自托管 Web。

#### 1.3.1 转换 / 合并架构（我们主要借鉴的部分）
源目录 `backend/src/products/`：`proxy-utils.esm.js`、`resource-parser.loon.js`、`sub-store-0.js`、`sub-store-1.js`、`cron-sync-artifacts.js`。

- **parser ↔ producer 产品模型**：订阅先经 **parser**（识别协议/格式 → 统一节点对象），再经 **producer**（把节点对象重组为目标格式输出）。这是 sub-store 的核心心智模型，比"解析即导出"更清晰，便于加中间算子。
- **resource-parser 脚本**：每种源（订阅链接 / 本地 / 各类格式）一个 parser 脚本，统一汇入节点池——对应我们的 `core/src/parse`。
- **Subscription Formatting（算子）**：正则过滤/丢弃、地区/类型过滤、设属性（udp/tfo/skip-cert-verify）、加 flag、排序、正则排序/重命名/删除、**脚本算子（JS 直接改节点）**、解析域名到 IP。
- 输出目标极广：mihomo / sing-box / Surge / Loon / QX / Stash / Egern / Shadowrocket / V2Ray / Plain JSON；Collect 多订阅为单链接（Artifacts / share）。

> **我们的映射**：采用 sub-store 的 **parser(解析) ↔ producer(重组) 心智模型**（`core/src/parse` → 统一 `Proxy` → `core/src/ops` 算子链 → `core/src/export`）；算子链（筛选/排序/重命名）对齐其 Formatting 算子；脚本算子作为 P3 可选（嵌入 `rquickjs`/`boa` 引擎，语义对齐 sub-store 脚本算子）。

- **缺什么**：Node 非 Rust；UI 朴素；无原生 GUI；测速/解锁非内建核心（靠外部脚本/http-meta）。

---

## 2. 能力映射：各家所长 → 我们的实现

| 想要的能力 | 借鉴对象 | 本项目实现方式 |
|---|---|---|
| 精美仪表盘 + 全面展示 | Resin | WebUI 复刻：KPI 卡 + 类型/地区分布 + 订阅来源 + 可用·延迟统计；主题令牌沿用 Resin |
| **逐个订阅健康度** | Resin | `SubscriptionHealth`（`core/src/model.rs`）+ 刷新端点 + WebUI 健康卡片 |
| 批量添加订阅 | BestSub | 订阅面板批量粘贴多个 URL，状态徽标，「全部刷新」 |
| 测速（延迟/可用性） | BestSub | 经引擎 SOCKS5 测协议级 HTTP 延迟（gstatic 204）；TCP 连通性兜底 |
| 出口地区检测 | BestSub | `GEO_CHANNELS` 7 通道 + `detect_outbound_country` |
| 流媒体解锁 | BestSub | `STREAM_SERVICES`（TikTok/Netflix/Disney+/YouTube/ChatGPT）+ `detect_unlock` + 解锁列 |
| 合并 → 输出新订阅 | sub-store | parser↔producer 模型 + 算子链 + 6 格式导出 + 可分享 |

---

## 3. 代码复用映射（核心交付：已借用的成熟代码）

> 原则：**直接移植成熟常量/算法，而非自己拍脑袋**。每个资产都标了源项目 + 源文件/函数 + 本项目落点 + 改编说明。所有借用均在源文件 doc-comment 中**署名致谢**。

| # | 借用的资产 | 来源项目 | 源文件 / 函数 | 本项目落点 | 借用方式 / 改编 |
|---|---|---|---|---|---|
| 1 | 订阅健康度字段集（`enabled/source_type/node_count/healthy_node_count/last_checked/last_updated/last_error`） | Resin | `control_plane_subscription.go` → `SubscriptionResponse` + `subToResponse()` | `core/src/model.rs` → `SubscriptionHealth` + `Subscription::health` | 字段集对照借用；健康度改为**读时从 `proxies` 派生** `recompute()`（Resin 是从 Pool 派生，思路一致：单一数据源） |
| 2 | 订阅刷新动作（立即重拉源 + 更新时间戳） | Resin | `handler_subscription.go` → `HandleRefreshSubscription` + `RefreshSubscription` | `server/src/lib.rs` → `refresh_subscription`（`POST /api/subscriptions/:id/refresh`） | 端点语义一致；因 Rust async **持锁跨 await 不满足 Send**，改为「快照→释放锁→`await`→重新加锁写回」两阶段（代码注释记录） |
| 3 | 存活探测目标（gstatic `generate_204`，期望 `204`） | BestSub | `checker/alive.go` → `Alive.URL` / `ExptectCode` / `detect()` | `core/src/resources.rs` → `ALIVE_TARGET` / `ALIVE_EXPECT_CODE`；`server/src/engine.rs` 存活探测 | 常量原样借用；探测改**经引擎 SOCKS5 出口**（与 BestSub `mihomo.Proxy` 思路一致） |
| 4 | 带宽测速端点（`__down?bytes=104857600` / `__up`） | BestSub | `checker/speed.go` → `DownloadUrl` / `UploadUrl` | `core/src/resources.rs` → `SPEED_DOWNLOAD_URL` / `SPEED_UPLOAD_URL`；`server/src/engine.rs` → `engine_bandwidth` | 常量原样借用；经引擎下载 `__down` 实测上下行字节→`Proxy.download_speed_bps`（接入完成） |
| 5 | 流媒体解锁（TikTok `region` 解析 + Netflix/Disney/YouTube/ChatGPT） | BestSub | `checker/tiktok.go` → `detectTikTok`（`"region":` 解析） | `core/src/resources.rs` → `STREAM_SERVICES` + `UnlockDetect::classify`；`server/src/engine.rs` → `detect_unlock`；`model.rs` → `ProxyUnlock` | TikTok 复刻 `detectTikTok` 的 `"region":` JSON 解析（含 IDC）；Netflix/Disney/YouTube 用社区通用 html/lang 判定；ChatGPT 用 block 串判定；结果写回 `Proxy.unlock` |
| 6 | 出口地区多通道探测 | BestSub | `modules/country/channel/*.go` + `country.go` → `GetCode()` | `core/src/resources.rs` → `GEO_CHANNELS` + `extract_country()`；`server/src/engine.rs` → `detect_outbound_country` / `geo_one` | 移植 **7 个通道**（合并两个 `api.ip.sb` 变体、省略内部 `commen` 回退）；`extract_country` 复刻「先 JSON 键后 `loc=XX`」；`geo_one` 复刻「每节点起引擎→逐通道尝试→首个命中即返回」 |
| 7 | 仪表盘美学 / 主题令牌 | Resin | `webui/src/styles/theme.css`（`--primary:#1470ff`、`--success:#048867`、`--danger:#c33a2f`、`--warning:#af7b00`、`--radius:18px`、玻璃拟态、徽章体系） | `webui/style.css` + `app.js` → `STATUS_META` | 调色板与状态色直接沿用；卡片/徽章布局借鉴 `platform-tile` / `badge` |
| 8 | 批量添加 + 手动/定时刷新订阅 | BestSub + Resin | BestSub 批量 checker 模型；Resin `ListSubscriptions`/`RefreshSubscription` | `server/src/lib.rs` `add_subscriptions`/`import_raw`/`refresh_subscription` + WebUI 批量粘贴 / 「全部刷新」「单订阅刷新」 | 批量导入 + 单/全刷新动作借用两者 |
| 9 | 合并节点→输出新订阅（算子管道） | sub-store | `backend/src/products/` parser↔producer + resource-parser 脚本 + 脚本算子 | `core/src/parse` → `Proxy` → `core/src/ops`(merge/transform) → `core/src/export` | 采用 sub-store 的 **parser↔producer** 心智模型；算子链（筛选/排序/重命名）对齐其 Formatting 算子；脚本算子 P3 可选 |
| 10 | 趋势图（trend-chart） | Resin | `dashboard` 滚动统计 + `trend-chart` 组件 | `server/src/lib.rs` → `TrendPoint` 滚动历史（`/api/trends`）+ `webui` canvas 折线 | 仪表盘每次刷新记录快照，WebUI 用 canvas 画「总/可用/未测节点 + 平均延迟」折线 |
| 11 | 熔断清理坏节点（circuit breaker） | Resin | `control_plane_subscription.go` → `CleanupSubscriptionCircuitOpenNodes` | `server/src/lib.rs` → `cleanup_bad`（`POST /api/nodes/cleanup`） | 语义一致：移除已测且不可用的节点（`available==Some(false)`），保留未测节点 |

---

## 4. 已落地实现状态

### 4.1 已实现并端到端验证 ✅
- **逐个订阅健康度**（`core/src/model.rs` `SubscriptionHealth` + `recompute()`）：`node_count / healthy_node_count / unknown_node_count / avg_latency_ms / best_latency_ms / status()`；`status()` ∈ {healthy, degraded, down, error, empty, untested, disabled, pending}。
- **订阅刷新端点** `POST /api/subscriptions/:id/refresh`：快照→释放锁→`await` 抓取→重新加锁写回，更新 `last_checked_at` / `last_updated_at` / `last_error`。
- **出口地区探测** `POST /api/geo-detect`：经 `SUBHUB_ENGINE_BIN` 起引擎，逐代理跑 `GEO_CHANNELS`，写回 `Proxy.outbound_country`。
- **流媒体解锁判定** `POST /api/unlock-detect`（BestSub 风格）：逐代理起引擎，跑 `STREAM_SERVICES`（TikTok/Netflix/Disney+/YouTube Premium/ChatGPT），`UnlockDetect::classify` 解析响应体，写回 `Proxy.unlock`（`TT✓HK NF✗` 紧凑摘要）。无引擎时安全返回空矩阵。
- **带宽测速** `POST /api/speedtest`（BestSub `speed.go` 风格）：经引擎下载 `SPEED_DOWNLOAD_URL` 实测下行字节/秒，写回 `Proxy.download_speed_bps`，节点表显示 MB/s。
- **坏节点熔断清理** `POST /api/nodes/cleanup`（Resin `CleanupSubscriptionCircuitOpenNodes` 风格）：删除 `available==Some(false)` 节点，保留未测节点。
- **Resin 风格趋势图**：仪表盘每次刷新记录 `TrendPoint`（`/api/trends`），WebUI 用 canvas 画「总/可用/未测节点 + 平均延迟」折线。
- **健康卡片 UI**（Resin 风格）：状态点 + 徽章、节点/健康/未测/平均·最佳延迟、`relTime()` 相对时间、错误展示、单卡刷新 + 删除 + 「全部刷新」。
- **主题令牌**：沿用 Resin 调色板（`#1470ff` / `#048867` / `#c33a2f` / `#af7b00`，`radius 18px`）。
- **e2e 冒烟测试**（`docs/test_subhub.py`）全绿：坏订阅→`error`+`last_error`；好订阅→`untested`（与 `down` 正确区分）；刷新生效；geo/unlock 无引擎时安全返回；6 格式导出；本地测速可达；解锁/趋势/清理端点均可用；WebUI 托管正常。

### 4.2 下一步可继续借用的成熟代码（建议）
1. ~~**流媒体解锁判定**（BestSub `tiktok.go` / `netflix.go` / `disney.go`）~~ ✅ 已完成（`STREAM_SERVICES` + `detect_unlock`）。
2. ~~**带宽测速接入**（BestSub `speed.go`）~~ ✅ 已完成（`engine_bandwidth` + `download_speed_bps`）。
3. **sub-store parser/producer 深化**：把现有 `parse`/`export` 显式拆成 sub-store 式的 `parser` 注册表 + `producer` 注册表，便于加新格式/新源零改动（纯内部重构，用户无感，优先级低）。
4. ~~**Resin 的熔断清理（circuit breaker）**~~ ✅ 已完成（`/api/nodes/cleanup`）。
5. ~~**UI 趋势图（Resin `trend-chart`）**~~ ✅ 已完成（canvas 折线 + 平均延迟虚线）。
6. ~~**持久化（SQLite）**~~ ✅ 已完成（`rusqlite` bundled，默认 `data/subhub.db`，重启不丢订阅与测速结果；`SUBHUB_DB` 可覆盖/置空退回内存）。

---

## 5. 开发规划（更新）

### 5.1 技术选型（关键决策，已落定）
- **GUI：Tauri v2**（Rust 核心 + WebView 原生窗口）。已验证 `cargo build --release -p subhub-app` 产出 Windows 原生二进制。
- **测速 / 连接引擎：复用 mihomo / sing-box**（`SUBHUB_ENGINE_BIN` 开启），与 BestSub 一致；不重写代理协议。
- **解析 / 转换**：`core` 统一 `Proxy` 模型；解析器覆盖 clash(sing-box)/yaml/json/uri/base64；导出 6 格式；算子链对齐 sub-store。
- **存储 / 网络**：`rusqlite`（bundled 编译）持久化到 `data/subhub.db`（默认开启，`SUBHUB_DB` 可覆盖/置空退回内存态）；进程内 `Mutex<Vec<Subscription>>` 为热数据，`reqwest` 抓取（支持 `fetch_proxy` 经代理拉取）。

### 5.2 路线图（P0–P10 全部完成）
| 阶段 | 内容 | 状态 |
|---|---|---|
| P0 | 导入→统一模型→去重合并→仪表盘→导出 | ✅ |
| P1 | 测速引擎：TCP/可用性 + 外部引擎协议级延迟 | ✅ |
| **P1.5** | **Resin 逐个订阅健康度 + 刷新 + 出口地区**（本次） | ✅ |
| P2 | Resin 级仪表盘打磨 | ✅（基础版） |
| P3 | sub-store 式算子管道 + 多格式导出 | ✅ |
| P4 | 跨平台打包 + Windows 原生二进制 | ✅ |
| **P5** | **流媒体解锁判定 + 带宽测速 + Resin 趋势图 + 熔断清理** | ✅ 已完成（§4.1） |
| **P6** | **订阅添加后自动健康度检测 + 测速（Resin 式即时测试）+ 支持通过代理拉取订阅 + 页面中文翻译打磨** | ✅ 已完成 |
| **P7** | **SQLite 持久化（重启不丢）+ 节点按订阅来源归属（sub-store 分组）+ Resin 式定时自动刷新** | ✅ 已完成 |
| **P8** | **本地订阅直接拉取地址（`GET /sub`，可编码算子变成常驻网址）· 节点列表分页 · 算子转换（Transform）图文说明与分享链接 · 应用图标重制 + 重新编译 exe** | ✅ 已完成 |
| **P9** | **质量加固：健康度排序 · 导出去无效节点 · 地区注入 bug 修复 · 前端解锁调用修复 · 引擎跳过不可导出节点 · clippy 0 警告 · 文档同步** | ✅ 已完成 |
| **P10** | **节点列表全局排序（跨页）· WebUI 集中「设置」页 + 全局 Top-N 单一真相源 · 手动测速「仅测未测 / 仅失败」模式 · 合并导入防任意覆盖 · 删除不存在订阅返回 404** | ✅ 已完成 |

---

## 6. 风险与难点
- **引擎集成**：mihomo 配置生成与结果解析（已用 `to_clash_meta` + `mixed-port` SOCKS5 模式跑通存活/geo）。
- **持锁跨 await**：axum handler 中 `MutexGuard` 不能跨 `.await`（Send 失败）——已用两阶段快照/释放/重锁规避。
- **格式转换完备度**：Surge/Loon/QX/Stash 等小众目标已支持基础导出，脚本算子待补。
- **合规**：仅用于合法合规场景，遵守当地法律与服务条款。

---

## 7. 下一步建议
1. ~~**持久化（SQLite）**~~ ✅ 已完成（P7）。
2. **sub-store 式 `parser`/`producer` 注册表深化**：把现有 `parse`/`export` 显式拆成注册表，便于加新格式/新源零改动（纯内部重构，用户无感）。
3. **Resin 级细节打磨**：节点延迟直方图（当前仅有趋势折线）；导出订阅的「自动定时刷新」已在 P7 以 `SUBHUB_AUTO_REFRESH_SEC` 形式落地。

> P5 / P6 / P7 已全部落地：流媒体解锁、带宽测速、趋势图、坏节点熔断清理、订阅添加后自动健康度+测速、通过代理拉取订阅、中文翻译打磨、SQLite 持久化、节点按订阅分组、定时自动刷新均已实现并端到端验证。

---

## 8. Round E/F — 质量加固（代码审查 + 死代码清理 + clippy + 文档同步）

在 P0–P8 功能稳定后，做了一次针对「跨语言边界方法/字段混用」类 bug 的专项审查与质量加固，触发点是用户反馈：把 SubHub 的 `/sub` 订阅填进 Sparkle 报 `unsupport proxy type: other`；刷新订阅时前端报 `测速失败: p.unlock.summary is not a function`。

### 8.1 用户反馈的两个 bug（根因同源：Rust 方法被当成 JS 字段/对象方法）
- **Sparkle `unsupport proxy type: other`**：`Proxy::is_exportable()` 此前对 `ProxyType::Other` 走 `_ => true`，导致 `other` 节点被导出，mihomo/clash-meta 直接拒绝。修复：把兜底分支改为显式枚举 `Socks5 | Http | Wireguard => true; _ => false`，`Other` 与缺必填字段的节点不再进入导出（`core/src/model.rs`）。
- **前端 `p.unlock.summary is not a function`**：`app.js` 对一个**已 JSON 序列化的普通对象**调用了 Rust 的方法 `.summary()`，序列化后的 JS 对象上根本不存在该方法。修复：新增纯 JS 的 `summaryUnlock()` 镜像 Rust `ProxyUnlock::summary()` 的摘要逻辑（`webui/app.js`）。

### 8.2 审查主动发现的第三个同类 bug（已修复）
- **地区列恒为 `OTHER`**：`renderNodes` 读取 `p.region`（Rust 方法，从未被序列化），导致前端永远拿不到地区，地区列恒显示 `OTHER`。根因与 `unlock.summary` **完全一致**——把 Rust 的方法当成 JS 的字段/对象方法使用。修复：`server/src/lib.rs` 的 `list_proxies` 在构造 JSON 时显式 `obj.insert("region", p.region())`，前端地区列恢复正常（HK/JP/US…）。
- **教训（后续约定）**：Rust 侧的 `Proxy` 方法/关联函数（如 `.region()`、`.summary()`、`ProxyUnlock::summary()`）无法在 JSON 序列化后的 JS 对象上调用。凡是前端需要的值，必须在 `list_proxies` / 各处 `serde_json` 构造时**显式注入为字段**。新增字段请遵守此约定，避免同类问题复发。

### 8.3 死代码 / 逻辑优化
- `server/src/engine.rs` 的 `with_engine` 增加 `if !p.is_exportable() { return None; }` 守卫：不可导出节点（如 `Other`、缺字段）不再拉起外部引擎做协议级测速，省资源、避免无意义报错。
- 多处 clippy 建议落地：整数除法改用 `checked_div`（防溢出）、`let_and_return` 改为直接返回 if 表达式、`map_or`/`map` 简化、`clamp` 替代手写范围限定、`collapsible_str_replace` 合并分支（`core/src/model.rs`、`core/src/export.rs`、`core/src/parse.rs`、`core/src/speedtest.rs`、`server/src/lib.rs`）。

### 8.4 Lint 结果
- `cargo clippy --release -p subhub-core -p subhub-server`：**0 警告**（此前 core 5 条 + server 3 条，已全部清零）。
- `cargo test -p subhub-core --lib`：增量合并 `incremental_update` 等测试通过。
- 全量 `cargo build --release` 通过（需先停掉占用 exe 的运行中进程，数据由 SQLite 持久化，停止安全）。

### 8.5 导出过滤规则（统一入口 `export_filter`）
所有导出路径（`POST /api/export`、`GET /sub`，6 种格式）在序列化前先经 `export_filter(proxies)`（`core/src/export.rs`）：
1. 用 `Proxy::is_usable()` = `is_exportable() && available != Some(false)` 判定；
2. 去掉 `Other` 类型、缺必填字段、已测但不可用（`available == Some(false)`）的节点；
3. **保留未测节点**（`available == None`），避免误删尚未测速的节点；
4. 服务端日志打印「导出时去除 N 个无效节点」，便于核对。

> 这套过滤直接解决了 Sparkle `unsupport proxy type: other`：导出时 `other` 节点已被剔除，mihomo/clash 拿到的永远是可识别的合法节点。

---

## 9. Round G —— 分模块代码审计（按模块逐条核对 + 修复）

在 P0–P9 功能稳定、clippy 已 0 警告的基础上，做了一次**逐模块（model / parse / export / ops / speedtest / server）**的定向审计，逐条核对审计清单并给出处理结论。本节的「修复」均为本次新增；「已确认/审查判定：保留」为核对后确认无需改动或属可接受限制。

### 9.1 core/src/model.rs

| 审计项 | 结论 / 处理 |
|---|---|
| `SUB_COUNTER` 重置导致 ID 冲突 | **已确认，无需改**：存在 `rebase_sub_counter(max_n)`，SQLite 加载后在 `server/src/lib.rs` 调用，把计数器抬到 `max+1`，避免新订阅覆盖旧 ID。 |
| `SubscriptionHealth::status()` 整数溢出 | **已修复**：原 `(healthy_node_count * 100) / node_count` 在大数下可能溢出。改为先提升为 `u64` 运算，并加 `node_count > 0` 守卫（`model.rs` ~L77）。 |
| `region()` 把 `russia` 误判成 `us` | **已修复**：原用单 substring 匹配（`"us".contains` 命中 russia）。改为两级匹配——长名称表（hongkong/japan/tokyo/seoul/usa…）用 `.contains()`，2 字母国家码（hk/tw/jp/kr/us/de/fr/uk/ru/ca/au…）按**整词 token**（非字母数字切分）匹配，返回 `OTHER` 若不命中。 |
| `fingerprint()` 含 `name` | **审查判定：保留**。含 name 使「同 IP:port、不同显示名」不合并。对聚合器而言，宁可少合并也不误删共享 IP 的不同节点（丢节点比重复更糟）；去重仍以 `type|server|port` 为主，name 仅作区分。 |
| `ProxyType::Other` 处理 | **已确认**（Round F 已修）：`is_exportable()` 对 `Other` 显式返回 `false`，不再进入导出。 |

### 9.2 core/src/parse.rs

| 审计项 | 结论 / 处理 |
|---|---|
| `b64_decode` 吞错误 | **审查判定：保留**。顶层 dispatch `if let Ok(decoded) = b64_decode(raw)` 静默回退到其它解析器——这是订阅格式嗅探的正常"试错"行为，某格式解不出就试下一种，全部失败才在上层报错。 |
| `parse_vmess` 数字端口兜底 | **已确认**：已存在数字端口回退逻辑。 |
| `extract_yaml_proxies` 嵌套过深 | **审查判定：保留**。当前为单层循环 + `if/else if/else`，结构清晰，`cargo clippy` 0 警告。 |
| `parse_ss` SIP002 误判 | **已修复**：旧逻辑把整段 base64 当 SIP002，导致错误解析。改为仅当 `decode_ss_userinfo` 解出**非空 `method`** 时才认定 SIP002；否则回退到整段 base64 解码（`parse.rs` ~L346）。 |
| sing-box `transport` 解析不完整 | **已修复**：补充解析 `transport: { type, path, headers, host, service_name }`，对 ws/grpc/h2 填 `network` / `path` / `host`（取自 `headers.Host`）/ `service_name`（grpc）（`parse.rs` ~L199）。 |

### 9.3 core/src/export.rs

| 审计项 | 结论 / 处理 |
|---|---|
| Surge 输出名字重复 | **已确认**（旧版已修）：当前 Surge 输出为 `Proxy = name, ...`，单名正确，无重复。 |
| v2ray 输出 `freedom`/`direct` 兜底 | **已修复**：旧实现把不支持类型（Hysteria2/Tuic/Socks5/Http/Wireguard）仍生成 `freedom`/`direct` outbound。`v2ray_outbound` 改为返回 `Option<Value>`，`_ =>` 直接 `return None`；`to_v2ray_json` 用 `filter_map` 丢弃不支持类型（`export.rs` ~L184/222/264）。 |
| base64 导出语义歧义 | **已修复**：加注释澄清——base64 分支是 clash-meta YAML 的 base64，并非 v2ray URI 列表。 |

### 9.4 core/src/ops.rs

| 审计项 | 结论 / 处理 |
|---|---|
| `keep()` 每个节点 `Regex::new` 重编译 | **已修复**：`apply()` 预先把每条过滤器编译成 `Vec<(FilterRule, Option<Regex>)>`，`keep()` 复用预编译正则，省去「大列表 × 多正则」每节点重编译开销（`ops.rs` ~L62/112）。 |
| 重命名 `\$` 转义失效 | **已修复**：旧实现 `\$`→`$`，但 Rust regex 把 `$` 当捕获组引用，无法输出字面 `$`。改为 `\$`→`$$`（regex 中 `$$` 即字面 `$`），并补单测 `rename_emits_literal_dollar_via_escaped_placeholder` 锁住行为（`ops.rs` ~L85）。 |

### 9.5 core/src/speedtest.rs

| 审计项 | 结论 / 处理 |
|---|---|
| `thread::scope` 锁竞争 | **审查判定：保留**。用 `Mutex<usize>` 仅做索引自增，持锁仅 3 条指令，`concurrency` 限 1–64，竞争可忽略。 |
| DNS 阻塞 | **审查判定：已知限制，接受**。`to_socket_addrs()` 的 DNS 解析不受 `connect_timeout` 约束（std 已知限制）。节点域名多为可达地址，DNS 超时由 OS 控制；彻底隔离需引入异步 DNS，超出本次范围。 |
| `test_one` 无 timeout / dns 区分 | **已确认**：`tcp_ping` 已用 `dns:` / `conn:` 错误前缀区分，且 `connect_timeout` 已应用（DNS 阶段无独立超时属上一条限制）。 |

### 9.6 server/src/lib.rs

| 审计项 | 结论 / 处理 |
|---|---|
| `flatten_dedup` O(N²) | **已确认**：已用 `fingerprint → index` HashMap（`lib.rs` ~L481）。 |
| `list_proxies` `serde_json::to_value` | **已确认**（Round F 已修）：逐节点构造 `obj` 并显式 `obj.insert("region", p.region())`（`lib.rs` ~L1467），前端地区列正常。 |
| `geo_detect` / `unlock_detect` O(N²) | **已修复**：改为一次性构造 `cc_by_fp` / `unlock_by_fp`（`HashMap<fingerprint, _>`），用 `p.fingerprint()` O(1) 反查，整体 O(N)（`lib.rs` ~L1337/1343/1355）。 |
| `do_refresh_one` fetch 错误处理 | **已确认**（Round F 已修）：刷新错误写入 `last_error` 且 `status` 置 `error`。 |

### 9.7 非审计项但阻塞编译的修复（本次一并处理）

- **`engine.rs` `format!` 块 lint 警告**：原 `mixed-port` 配置用 `\n\` 续行触发 `multi_line_directives`。改为用 `writeln!` 拼接到 `String`（已验证输出与原文一致，`engine.rs` ~L515）。
- **源码损坏修复**：`server/src/{db,engine}.rs` 存在非法 UTF-8（截断 em-dash 字节）、mojibake `?` 注释，以及文本模式写入导致的双重 CR。**二进制模式重读、归一为 LF、清理乱码注释**后编译通过。
- **命名回退**：此前误把 crate/env/db 标识改成 `subhub*`。按用户「**仅改显示名 SubHub，保留内部标识以保全既有数据与配置**」的决定，回退为 `subhub*` / `SUBHUB_*` / `subhub.db`（源码全局 grep 已无 `subhub`/`SUBHUB` 残留；仅显示名 `SubHub` 与 `target/` 构建缓存指纹残留，不影响源码）。
- **数据文件名对齐显示名（2026-07-19，用户新决策）**：用户随后把数据文件 `data/subhub.db` 改名为 `data/subhub.db`，并以**最小范围**落地——仅改源码默认路径（`server/src/db.rs` → `data/subhub.db`）+ 重编译；环境变量 `SUBHUB_DB` 与 crate 名 `subhub-*` 保持不变，旧 `subhub-*` 构建缓存指纹已清理。数据零丢失（改名前已备份并校验 SQLite 完整性 ok、10 条订阅完好）。

### 9.8 验证结果

- `cargo clippy --release -p subhub-core -p subhub-server`：**0 警告**（`empty_line_after_doc_comments`、`multi_line_directives` 全部清零）。
- `cargo test -p subhub-core --lib`：**3 测试通过**（含新增 rename 转义测试）。
- `docs/test_subhub.py` 端到端冒烟：**ALL CHECKS PASSED**（Other 类型排除、地区注入修复、6 格式导出、解锁探测无引擎安全返回等）。
- 生产实例（端口 3099）未受影响；新二进制需重启服务方可生效。

---

## 10. Round Q —— 双代理全面复查（后端 Rust + 前端 / 脚本独立审查）

在 Round G 之后，对后端（core + server）与前端 / 脚本（webui + docs）分别做独立审查，逐条核对源码真伪后实施修复：

### 10.1 后端 Rust 安全 / 健壮性（B 系列）
- **B1 `redact_url` 越界 panic（严重）**：旧实现用全串偏移去索引子串，`https://a@b` 这类「scheme 比 userinfo 长」的 URL 直接 panic 并毒化 store mutex，导致后续所有请求 500。改用 `split_once('@')`（无偏移、不可能 panic），并补回归单测。
- **B2 Mutex 毒化恢复**：全部 `lock().unwrap()`（约 60 处）替换为 poison 容忍的 `lock_ok()` 扩展 trait——一次 panic 不再永久打死整个服务；`core/speedtest.rs` 工作线程同样处理。
- **B3 幽灵节点**：订阅正文里无 `user:pass@` 的裸 `http(s)://host:port` 行（更新链接 / 规则 URL）不再被解析成不可用的 Http 节点；带 userinfo 的真实 http 代理不受影响。
- **B4 SSRF 防护**：`fetch_subscription_text` 增加 scheme 白名单（仅 http/https），`file://` 等一律前置拒绝。
- **B6 评分单一真相源**：`score_proxy` 移入 `subhub_core`（`core/src/score.rs`），server 侧改为薄包装；`ops::apply` 排序新增 `score` 分支（此前静默回退按名称排），UI 排序 / Top-N 导出 / 算子管道三方评分从此不可能不一致。
- **B8 base64 解析深度上限**：`parse_subscription` 递归解 base64 增加深度（3 层）与解码体积（16 MiB）上限，防构造输入递归炸栈。

### 10.2 前端 / 脚本（F 系列 + 脚本修复）
- **前端 F1/F4/F5/F6**：节点列表加载失败在表格内显示错误（不再静默空白）；总数缩小后页码自动夹取到末页；仪表盘条形图 label HTML 转义（防节点名注入 XSS）；全局 Top-N 保存增加成功 / 失败反馈（不再静默失败）。
- **重复添加 URL 幂等合并（端到端测试新抓出）**：`POST /api/subscriptions` 重复添加同一 URL 此前会产生重复订阅条目；现改为按 `source` 就地更新（refresh 语义，`incremental_update` 保留存活节点已测健康数据；拉取失败时保留旧节点列表），与 `/api/import` 的合并行为一致。
- **脚本修复**：`docs/import_sparkle_subs.py` 字段名 `url`→`source`、`s['health']`→扁平 `SubSummary` 字段（原脚本必 KeyError）；`docs/test_subhub.py` speedtest 断言改为对象形状（`{results, removed, threshold}`），并新增跨页全局排序一致性、`top_n` 设置持久化往返、删除不存在订阅返回 404、同 URL 重复导入幂等合并、`mode=untested` 范围测速共 5 组端到端用例。

### 10.3 验证
- `cargo test -p subhub-core -p subhub-server`：**38 测试全过**（core 32 + server 6，本轮新增 12）；clippy 0 警告；release 构建通过。

---

## 11. Round R —— 引擎运行时与数据完整性复查

在 Round Q 全面修复基础上，对前几轮未覆盖的 `engine.rs`（引擎运行时）与「重复添加幂等合并」做专项复查，并落地一处可测性改进：

### 11.1 `engine.rs` 复查（结论：已加固，无需大改）
逐一核对引擎进程管理——RAII `EngineGuard` 保证子进程与临时目录在所有退出路径（含 `spawn` 之后的早退 `?`）均被清理；`EngineDirSeq` 计数器 + pid 保证并发拉起目录互不覆盖；`engine_ready` 在引擎进程自行退出时立即返回（不再傻等整段超时）。**引擎配置 YAML 注入已安全**：节点 `name` 经 `to_clash_meta` 由 `serde_yaml` 序列化（自动引号转义），`proxy-groups` 引用处再叠加 `escape_yaml_scalar` 双保险，攻击者控制的节点名无法逃逸标量注入任意配置。

### 11.2 Per-sub 刷新错误上抛（结论：已满足）
复查确认 `do_refresh_one` 失败时写入 `health.last_error` 并由 `refresh_subscription` 持久化，前端 `loadSubs` 重渲染经 `.sub-err` 红字展示，因此「webui 错误上抛」缺口实际已闭合，无需额外改动。

### 11.3 幂等合并提取为可单测纯函数（R2）
此前「重复 URL 按 source 就地合并」逻辑只存在于 server 的 `add_subscriptions` 闭包内、仅由端到端覆盖。现抽出为 `subhub_core::ops::merge_subscriptions_by_source`（单一真相源，同时被 `/api/subscriptions` 与 `/api/subscriptions/import` 复用），并修复返回 `added` 计数在「重复添加（实为 refresh）」时被整份节点数虚高的小瑕疵——现在只统计**真正新建**订阅的节点数。核心新增 4 个单测覆盖：新建计数、重复添加不重不漏、拉取失败时保留旧节点、refresh 保留存活节点已测健康。

### 11.4 验证
- `cargo clippy --release -p subhub-core -p subhub-server` 0 警告；`cargo test -p subhub-core -p subhub-server` **42 测试全过**（core 36 + server 6，本轮新增 4）；release 构建通过；`docs/test_subhub.py` 隔离库端到端 **ALL CHECKS PASSED**（含同 URL 重复导入幂等合并用例）。

---

## 12. Round S —— base64 包裹的 URI 列表订阅解析缺失修复

### 12.1 问题（症状）
用户反馈：订阅 `https://s4.laoda666.com/s/...`（laodavip 式短链）「应该有节点，但 subhub 拉取后显示没节点」。订阅源返回 **200 + base64 文本**，subhub 把它当作一条订阅存下，但节点数为 0、`status` 既非 `error` 也非空——典型的「拉到了却被解析成 0 节点」。

### 12.2 根因（`core/src/parse.rs::parse_subscription_depth`）
base64 解包分支只在「解码后文本包含 `proxies:` / `outbounds` / `"proxies"`」时才递归下去。这只覆盖**base64 的 clash YAML / sing-box JSON**。而该订阅是 **base64 的 URI 列表**（`vless://uuid@host:port?security=reality&...` 每行一个，解码后**不含**上述 YAML/JSON 标记），于是：
1. base64 分支不触发；
2. 后续 URI 解析直接遍历**未解码的 base64 原文**（`raw`），原文里没有 `://` 行 → 解析出 0 个节点。

这是大量「转换订阅 / v2rayN / NekoBox 式短链」的真实形态（base64-of-URIs），旧判定把它们整体漏掉了。

### 12.3 修复
新增 `looks_like_subscription(text)`，让 base64 解包在遇到「解码后是已知代理 scheme（`vmess`/`vless`/`trojan`/`ss`/`ssr`/`hysteria2`/`hy2`/`hysteria`/`tuic`/`socks5`/`socks`/`http`/`https`）的 URI 列表」时也递归下去：

```rust
fn looks_like_subscription(text: &str) -> bool {
    if text.contains("proxies:") || text.contains("\"outbounds\"") || text.contains("\"proxies\"") {
        return true;
    }
    text.lines().any(|l| {
        let l = l.trim();
        l.split_once("://").is_some_and(|(scheme, _)| is_known_scheme(scheme))
    })
}
```

原 `proxies:`/`outbounds` 判定作为该函数的第一分支保留，行为不退化；新增的 URI-list 判定用 `is_known_scheme` 复用既有方案白名单，避免把无关 URL 误判为订阅。`text.trim() != raw` 守卫仍防止无限递归，`MAX_B64_DEPTH` 仍封顶嵌套层数。

该修复同时作用于三条入口：`POST /api/subscriptions`（添加）、`do_refresh_one`（刷新）、`import_raw`（粘贴导入），因为它们都调用 `parse_subscription(&text)`。

### 12.4 验证
- 新增回归测试 `base64_of_uri_list_is_decoded`：base64 编码 `vless://` + `trojan://` URI 列表（模拟该订阅真实形态），断言解析出全部节点且类型正确。
- `cargo test -p subhub-core --lib parse::tests`：**15 测试全过**（含新增用例）。
- `cargo clippy --release -p subhub-core`：**0 警告**（新引入的 `map_or` 已改为 `is_some_and`）。
- `cargo build --release -p subhub-server`：**release 构建通过**。
- 注：`cargo build --release -p subhub-app` 因当前 `target/release/subhub-app.exe` 被运行中的进程占用（拒绝访问，os error 5）未能覆盖；代码与 server 完全一致，server release 已通过即可验证编译正确性。需退出正在运行的 subhub 后再行覆盖该 exe。

### 12.5 加固：URL-safe base64 + 0 节点诊断（同大类第二例）

用户复报同类现象：`https://qijiavpn.salnc.kuaivpn.app/api/v1/client/subscribe?token=...` 也显示 0 节点。该源从本机环境返回 **403 Forbidden: Invalid Client**（后端按客户端/IP 做了校验，无法直接取到内容），但用户侧能拉到 200——即「拉到了却解析为 0 节点」的同一大类。

进一步排查 `salnc/kuaivpn`（V2Board / Xboard 系面板）订阅的编码习惯，发现上一轮只修了一半：

1. ✅ 已修（§12.3）：base64 包裹的 URI 列表 / clash YAML 不解包。
2. ⚠️ **未覆盖**：这类面板常用 **URL-safe base64**（`-`/`_` 代替标准字母表的 `+`/`/`）编码。`b64_decode` 只试了 `STANDARD` 字母表 → 解码失败 → base64 解包分支永不触发 → 整份订阅解析为 0 节点。

**修复（`core/src/parse.rs::b64_decode`）**：依次尝试 `STANDARD` / `STANDARD_NO_PAD` / `URL_SAFE` / `URL_SAFE_NO_PAD` 四种字母表，首个成功即返回。该改动对 `parse_vmess` / `parse_ssr` / `extract_subscription_usage` 等所有调用 `b64_decode` 的路径一并生效（vmess 链接等也常用 URL-safe base64）。

**诊断增强（`server/src/lib.rs` 的 `add_subscriptions` 与 `do_refresh_one`）**：订阅**拉取成功（200）但解析为 0 节点**时，打印前 400 字节原始 body 前缀到 stderr，便于确认未识别的格式（base64 变种 / 未支持的 clash 键等）。运行 `subhub-server` 时可直接在终端看到；Tauri 窗口模式日志不直达终端。

### 12.6 验证
- 新增回归测试 `base64_url_safe_uri_list_is_decoded`：构造**确实含 `-`/`_`** 的 URL-safe base64（尾部注入 `0xFF 0xFF` 使 STANDARD 编码出现 `/`，再替换为 `_`），断言 `parse_subscription` 解析出 2 个节点（vless + trojan）；尾部的非 UTF-8 垃圾行无 `://`，被忽略。
- `cargo test -p subhub-core --lib parse::tests`：**16 测试全过**（含本轮新增用例）。
- `cargo clippy --release -p subhub-core`：**0 警告**。
- `cargo build --release -p subhub-app`：**release 构建通过**（35.11s）；已结束占用 exe 的运行中实例后重编成功。

### 12.7 加固：`anytls://` 协议支持 + 端口后多余 `/` 解析（同大类第三例）

用户复报：粘贴的订阅内容解码后是 **base64 包裹的 `anytls://` URI 列表**（形如 `anytls://df28c004-...@aws.v4.jp.group1.mysterianet.xyz:56147/?type=tcp&insecure=1&fp=chrome&sni=updates.cdn-apple.com#...`），subhub 仍显示 0 节点。

排查发现两处根因：

1. ⚠️ **`anytls` 不在 `is_known_scheme` 内**：`parse_subscription_depth` 的 base64 解包判定 `looks_like_subscription` 依赖 `is_known_scheme`，而后者未收录 `anytls` → 解码后虽是 `anytls://` URI 列表，但 `looks_like_subscription` 返回 false → 解包不触发 → 整份解析为 0 节点。这是该订阅「拉到了却 0 节点」的直接原因（此前误以为 qijiavpn 只是 URL-safe 问题，实际是 anytls 未识别）。
2. ⚠️ **端口后多余 `/` 导致整条解析失败**：真实链接是 `host:56147/?type=...`，`?` 之前带一个**路径分隔 `/`**。`split_authority` 用 `hostport.rsplitn(2, ':')` 取端口时拿到的是 `56147/`，`u16::parse` 因尾部 `/` 失败 → 返回 `None` → 该节点被整体丢弃。即便修了 `is_known_scheme`，只要 URI 带这个 `/` 仍然 0 节点。

**修复：**

- `core/src/parse.rs`：
  - `is_known_scheme` 加入 `"anytls"`。
  - `parse_uri` match 加入 `"anytls" => parse_anytls(&body, name)`，新增 `parse_anytls`：userinfo 即 **password**（anytls 用密码鉴权，不是 uuid），`type`/`network` 取传输，`insecure=1/true` → `skip-cert-verify`，`fp` → `fingerprint`，`sni` → `sni`，`tls` 强制 `true`（AnyTLS 永远走 TLS）。
  - `split_authority` 在提取端口时**只取前导数字**（`split(|c| !c.is_ascii_digit).next()`），容忍端口后的 `/<path>`，ipv6 分支同样加固。该改动对所有走 `split_authority` 的协议（vless/trojan/hy2/tuic/socks/anytls）一并生效。
- `core/src/model.rs`：`ProxyType` 新增 `AnyTls` 变体（位于 `Wireguard` 之后、`Other` 之前），`as_str() => "anytls"`；`fingerprint` 的 `Trojan | Hysteria2 | Socks5 | Http | AnyTls` 分支纳入 anytls 凭据；`is_exportable` 中 `Trojan | AnyTls => has(&password)`。
- `core/src/export.rs`：
  - `to_clash_value` 新增 `AnyTls` 分支：输出 `password` / `sni` / `client-fingerprint` / `skip-cert-verify`，**不输出顶层 `tls:`**（mihomo 的 anytls outbound 无此开关，输出会被拒）。
  - `v2ray_outbound` / `singbox_outbound` 对 `AnyTls` 显式 `return None`（v2ray-core / sing-box 均不支持 anytls，跳过而非输出错误的 `direct` outbound）。`to_surge` 走 `_ => continue` 已自然跳过。

### 12.8 验证
- 新增回归测试：
  - `parse::tests::anytls_uri_is_parsed`：断言 `anytls://` 解析为 1 节点，server/port/password/`tls=true`/`skip-cert-verify=true`/`sni`/`fingerprint`/name 全部正确（含端口后 `/` 的形态）。
  - `parse::tests::base64_of_anytls_uri_list_is_decoded`：base64 编码 `anytls://` URI 列表，断言解包后解析出 2 个 anytls 节点。
  - `export::tests::anytls_to_clash_meta_emits_password_no_top_level_tls`：断言 clash-meta 输出含 `type: anytls` / `password` / `sni` / `skip-cert-verify: true` / `client-fingerprint`，且**不含** `tls:`。
  - `export::tests::anytls_skipped_in_v2ray_and_singbox`：断言 v2ray 输出为 `[]`、sing-box 不含 anytls 节点（跳过）。
- `cargo test -p subhub-core --lib`：**42 测试全过**（含本轮 4 个新增用例）。
- `cargo clippy --release -p subhub-core`：**0 警告**。
- `cargo build --release -p subhub-app`：**release 构建通过**（1m19s）。
