# SubHub

用 **Rust + Tauri v2** 开发的代理订阅聚合工具 —— 把三家竞品的优点合到一起：

- **Resin** 风格的漂亮 Web 仪表盘 + 全面展示（类型环图 / 地区分布 / 订阅来源 / 可用·延迟统计）
- **BestSub** 的批量添加订阅 + 测速能力（TCP 延迟 / 可用性，可选外部引擎做协议级 HTTP 延迟）
- **sub-store** 的「合并节点 → 输出新订阅」能力，并扩展为 **算子管道**（筛选 / 排序 / 重命名）+ 多格式导出
- 全程 **Rust 核心**，并提供 **原生 GUI（Tauri 窗口，同时保留 WebUI）**

## 状态

**P0 ~ P5 全部完成并端到端验证。**

| 阶段 | 内容 | 状态 |
|---|---|---|
| P0 | 导入（批量 / 粘贴）→ 统一节点模型 → 去重合并 → 仪表盘 → 导出 | ✅ |
| P1 | 测速引擎：TCP 延迟 / 可用性 + 外部引擎协议级 HTTP 延迟钩子 | ✅ |
| P2 | Resin 级仪表盘：类型环图 / 地区分布 / 订阅来源 / 可用·延迟卡片 | ✅ |
| P3 | sub-store 式算子管道（筛选 / 排序 / 重命名）+ 多格式导出 | ✅ |
| P4 | 跨平台打包配置 + Windows 原生二进制验证 | ✅ |
| **P5** | **Resin 逐个订阅健康度 + 刷新 + 出口地区 + 流媒体解锁 + 带宽测速 + 趋势图 + 坏节点熔断清理** | ✅ |
| **P6** | **订阅添加后自动健康度检测 + 测速（Resin 式即时测试）· 支持通过代理拉取订阅 · 页面中文翻译打磨** | ✅ |
| **P7** | **SQLite 持久化（重启不丢订阅/测速结果）· 节点按订阅来源归属（sub-store 分组）+ 一键分组视图 · Resin 式定时自动刷新** | ✅ |
| **P8** | **本地订阅直接拉取地址（`GET /sub`，可编码算子变成常驻网址）· 节点列表分页 · 算子转换（Transform）图文说明与分享链接 · 应用图标重制 + 重新编译 exe** | ✅ |
| **P9** | **质量加固：订阅按健康度排序 · 导出自动去除无效节点（`other`/缺字段/已测不可用）· 修复地区列恒为 `OTHER` 的注入 bug · 修复前端 `unlock.summary()` 调用错误 · 引擎跳过不可导出节点 · clippy 0 警告 · 文档同步** | ✅ |
| **P10** | **节点列表全局排序（跨页，不再是只排当页 50 个）· WebUI 集中「设置」页 + 全局 Top-N 单一真相源 · 手动测速「仅测未测 / 仅失败」模式 · 合并导入防任意覆盖** | ✅ |
| **P11** | **节点地区识别增强（`region()` 名称/国旗/机场码/2 字母码分级匹配 + 真实节点库补全，OTHER 占比大幅下降）· 手动测速改为 SSE 流式并实时显示进度（当前节点 / Ping / 带宽）· 修复测速接口 405（POST→GET）** | ✅ |
| **P12** | **导出质量筛选（带宽下限 / 延迟上限，手动导出 + 常驻订阅网址皆可用）· 进度条悬浮常驻（滚动页面始终可见）** | ✅ |
| **P13** | **节点多选单独测速 + 列表「上次测速」时间列 · 可用性以引擎 gstatic generate_204 实测为准（对齐 clash-verge，消除「TCP 通但协议不通」的假绿）** | ✅ |

## 架构

```
┌─────────────────────────────────────────────────────────────┐
│  Tauri v2 原生窗口 (app crate)                                │
│   └─ 后台线程启动 axum 服务，窗口 URL 指向 http://127.0.0.1   │
└─────────────────────────────────────────────────────────────┘
                          │ 同一份 WebUI
        ┌─────────────────┴──────────────────┐
        ▼                                     ▼
┌───────────────┐                    ┌──────────────────────┐
│  axum 服务     │  JSON API + 静态   │  浏览器 (直接访问)    │
│  (server)     │  文件托管 WebUI    │  http://127.0.0.1:3005│
└───────┬───────┘                    └──────────────────────┘
        │
        ▼
┌───────────────┐
│  core 库       │  统一 Proxy 模型 / 解析(clash,sing-box,URI) / 合并去重 /
│               │  算子管道(筛选/排序/重命名) / 多格式导出(clash/v2ray/singbox/surge/base64)
└───────────────┘
```

- `core/` —— 纯 Rust 库：统一 `Proxy`/`Subscription` 模型、各类订阅/URI 解析、合并去重、算子管道、多格式导出。
- `server/` —— axum HTTP 服务 + 内置静态 WebUI 托管。独立可运行（浏览器访问即 WebUI）。
- `webui/` —— Resin 风格的静态前端（HTML/CSS/JS，无需构建步骤，直接由 server 托管）。
- `app/` —— Tauri v2 原生窗口外壳，后台线程跑同一个 server，窗口加载同一份 WebUI。

## 数据存储位置与持久化机制

这是被问得最多的问题，单独说明：

**订阅配置存在哪里？**
默认存放在仓库（或 exe 同机）的 **`data/subhub.db`** 这个 **SQLite** 文件里，路径可用环境变量 `SUBHUB_DB` 覆盖。

**如何持久化？**
- 核心用 `rusqlite` 的 **`bundled`** 特性在编译期把 SQLite 引擎一起打进二进制，**零外部依赖**（不需要系统装 SQLite）。
- 每个「订阅来源」以**整段 JSON 快照**的形式存进 `subscriptions` 表（`id` 主键 + `data` 文本 + `updated_at` 时间戳），节点、健康度、测速结果、出口地区、解锁矩阵全部跟着订阅一起落盘。
- 服务启动时调用 `Db::load_all()` 把库里所有订阅读回内存；之后**任何写操作**（添加 / 导入 / 删除 / 刷新 / 测速 / 出口探测 / 解锁探测 / 坏节点清理）执行完都会调用一次 `persist_all()` 把当前内存快照写回库。
- 设计上 `Db` 只持有**文件路径**，不持有 `Connection`（因为 `Connection` 不是 `Send`，而 axum 的 `AppState` 需要 `Send+Sync`）。每次读写都开一个短连接，避免跨 `.await` 持有连接导致生命周期 / 死锁问题。
- 如果 `SUBHUB_DB` 指向的文件打不开，会**自动退回纯内存模式**（重启即清空），应用照常运行，不会崩。

**重置 / 迁移数据**
- 想清空一切：直接**删除 `data/subhub.db`** 即可（重启后从空状态开始）。
- 想换机器：把这个 `.db` 文件复制过去、用 `SUBHUB_DB` 指过去就行。

## 构建与运行

### 前置要求
- Rust 工具链（≥ 1.80）
- Tauri v2 原生窗口需要 **WebView2 运行时**（Windows 上首次构建会自动拉取；也可系统已装）
- 浏览器即可使用 WebUI，**无需 Node / 前端构建**

> ⚠️ **端口说明**：默认端口为 **3005**（本机 AdGuardHome 已占用 3000）。可用环境变量覆盖：
> `SUBHUB_PORT=8080 cargo run -p subhub-server`

> 🔧 **测速引擎（可选）**：设置 `SUBHUB_ENGINE_BIN` 指向 mihomo / sing-box 二进制后，测速会对每个节点临时拉起引擎、走 SOCKS5 测量**协议级 HTTP 延迟**（未设置时仅做 TCP 连通性延迟）。例如：
> `SUBHUB_ENGINE_BIN=/path/to/mihomo cargo run -p subhub-server`

> 💾 **持久化（默认开启）**：订阅与测速结果默认存入 `data/subhub.db`（SQLite，`rusqlite` bundled 编译，**零外部依赖**），重启不丢。可用 `SUBHUB_DB=/path/to/app.db` 覆盖路径；设为空字符串 `SUBHUB_DB=` 退回纯内存（重启即清空）。删除该文件即可重置全部数据。

> ⏱️ **定时自动刷新（可选）**：设置 `SUBHUB_AUTO_REFRESH_SEC=1800`（秒）后，后台会按间隔自动重拉所有**远程**订阅并重新测速（Resin 式自动检测），remote 订阅会沿用各自的 `fetch_proxy`。默认关闭。

### 仅用 WebUI（推荐先验证）
```bash
cargo run -p subhub-server
# 浏览器打开 http://127.0.0.1:3005
```

### 原生窗口（Tauri v2）
```bash
cargo run -p subhub-app
```

### 打包分发
```bash
# 调试二进制（cargo，无安装包）
cargo build --release -p subhub-app      # 产物: target/release/subhub-app.exe

# 带安装包（NSIS / MSI on Windows, dmg / app on macOS, deb / AppImage on Linux）
cargo tauri build
```
Windows 上 `cargo tauri build` 会生成 `target/release/bundle/nsis/*.exe` 安装包；
macOS / Linux 同理生成对应平台包。三端共用同一份 Rust 代码与 WebUI。

## API（JSON）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET  | `/api/health`        | 健康检查 |
| GET  | `/api/subscriptions` | 订阅来源列表（**含逐个健康度**：`status` / `source_type` / `node_count` / `healthy_node_count` / `unknown_node_count` / `avg_latency_ms` / `best_latency_ms` / `last_checked_at` / `last_updated_at` / `last_error`） |
| POST | `/api/subscriptions` | 批量添加订阅（body: `{"urls":[...],"fetch_proxy":"http://127.0.0.1:7890"}`）。服务端**抓取后解析，并立即对每个节点自动做健康度检测 + 测速**（TCP 延迟 / 可用性；配 `SUBHUB_ENGINE_BIN` 时再加协议级 HTTP 延迟与带宽） |
| POST | `/api/subscriptions/:id/refresh` | **刷新单个订阅**（Resin 风格）：重拉源 → 重算节点 → 更新健康时间戳，返回 `{status, nodes, source}` |
| DELETE | `/api/subscriptions/:id` | 删除某个订阅来源（不存在的 id 返回 404） |
| GET  | `/api/subscriptions/export` | **备份 / 可移植导出全部订阅**：返回 `SubExportDoc`（`kind`/`version`/`exported_at`/`engine_bin`/`subscriptions[]`，每条含 `id`/`name`/`source`/`source_type`/`fetch_proxy`/`health_enabled`/`proxies`），用于整机备份或跨实例迁移 |
| POST | `/api/subscriptions/import` | **从备份恢复订阅**：按源 URL 幂等合并（重导入同一实例或跨实例的远程订阅不重复），保留内嵌节点结果（不重新抓取 / 测速），本地 / 粘贴类无 URL 订阅互不冲突 |
| GET  | `/api/settings`      | 读取当前全局设置（`use_proxy` / `auto_refresh_sec` / `default_fetch_proxy` / `top_n` / `engine_bin` / `remove_after_fails`） |
| POST | `/api/settings`      | 更新并**持久化**全局设置到 `meta` 表（重启不丢）；`default_fetch_proxy` 仅在代理 URL 校验通过后才写入 |
| POST | `/api/import`        | 粘贴订阅内容直接导入（body: `{"content":"..."}`，支持 clash yaml / sing-box json / base64 / 各类 URI 混合） |
| POST | `/api/geo-detect`    | **出口地区探测**（BestSub 风格）：经 `SUBHUB_ENGINE_BIN` 起引擎，逐节点跑 geo-IP 通道，写回 `Proxy.outbound_country`（无引擎时安全返回空） |
| POST | `/api/unlock-detect` | **流媒体解锁判定**（BestSub 风格）：经引擎逐节点探测 TikTok / Netflix / Disney+ / YouTube Premium / ChatGPT，写回 `Proxy.unlock`（无引擎时安全返回空矩阵） |
| POST | `/api/nodes/cleanup` | **坏节点熔断清理**（Resin 风格）：删除所有 `available==false` 的节点（保留未测节点），返回 `{status,removed}` |
| GET  | `/api/nodes/top`     | **跨订阅 Top-N 节点**：按综合评分返回全局最高的前 N 个节点（受全局 `top_n` 设置控制；`?n=N` 可临时覆盖，0=全部），每项含 `sub_id`/`sub_name`/`region`/`score` |
| GET  | `/api/proxies`       | 节点列表（可 `?type=ss&region=HK&q=foo` 过滤；**分页** `?page=1&page_size=50`，返回 `{total,page,page_size,items}`，`page_size` 上限 500；**全局排序** `?sort=name\|latency\|speed\|score&desc=1` 在**分页前对全部节点**生效，可用性主导——`available==false` 的节点无论按哪列都沉底；每项含 `outbound_country` / `unlock` / `download_speed_bps` / `region` / `sub_id` / `sub_name` —— `region` 为节点地区（HK/JP/US…，此前曾因未注入而恒为 `OTHER`，已修复）；`sub_id`/`sub_name` 标明节点归属的订阅来源，供「按订阅分组」视图使用） |
| GET  | `/api/dashboard`     | 仪表盘统计（总数 / 订阅数 / 可用·不可用·未测 / 平均·最佳延迟 / 按类型 / 按地区 / 每订阅） |
| GET  | `/api/trends`        | **趋势数据**（Resin 风格）：滚动窗口内的 `TrendPoint` 快照序列（总/可用/未测节点 + 平均延迟），供趋势图使用 |
| POST | `/api/export`        | 合并并导出（body: `{"format":"clash-meta","transform":{...},"sub_ids":[...],"top_n":N}`），返回 `{format,count,content}`，`top_n` 可临时覆盖全局设置 |
| GET  | `/sub`               | **本地订阅直接拉取地址**：合并所有（或 `?sub=id1,id2` 指定）订阅，可选 `?format=clash-meta` / `clash` / `v2ray` / `sing-box` / `surge` / `base64` 与算子参数 `?sort=latency&desc=1&rename_pat=.*HK-(.*)&rename_rep=HK-$1&q=关键词&region=HK&type=ss`，返回**纯文本订阅内容**（clash 类为 `text/plain`、v2ray/sing-box 为 `application/json`），无 JSON 包装、无需鉴权。把这个网址直接填进 mihomo / clash / v2rayN / sing-box 的「订阅」即可常驻拉取（详见下文「本地订阅地址」一节） |
| GET  | `/api/speedtest`     | 手动测速，返回 **SSE 流**（`text/event-stream`），WebUI 据此实时显示进度条与「正在测的节点 / Ping / 带宽」。查询参数：`timeout_ms`（默认 4000）、`concurrency`（默认 20）、`mode`（`all` 全部 / `untested` 只测 `last_tested_at==None` / `failed` 只测 `available==Some(false)`，复用自动刷新增量思路）、`ids`（可选，逗号分隔的节点 `fingerprint` 列表，**只测这些选中节点**，供节点页「测速选中」多选使用）。事件：`progress`（每个节点完成 **任一阶段** 发一条，含 `done/total/name/available/latency_ms/bandwidth_bps/phase`，`phase` 为 `tcp`/`http`/`bw`）与 `done`（汇总 `tested/reachable/avg_latency_ms`）。**进度总量覆盖全程**：无引擎时 `total = 节点数`（仅 TCP）；配置引擎时 `total = 节点数 × 3`（TCP + HTTP 延迟 + 带宽各占一段），所以进度条**真正到 100% 才结束**，不会在 TCP 完成后卡住。每个节点的 TCP 延迟 / 可用性（及可选 HTTP 延迟、下行带宽）测完即写回节点记录。**可用性判定对齐 clash-verge**：配置了 `SUBHUB_ENGINE_BIN` 且本批确有节点经引擎探测成功时，节点的「可用」以**引擎对 gstatic generate_204 的真实代理级 HTTP 探测**为准——TCP 能连但协议层不通的节点会被正确判为不可用，不再出现「SubHub 绿、clash-verge 红」的假绿；引擎未配置或本批全部探测失败时退回 TCP 结果，避免误杀 |
| POST | `/api/proxy-test`    | **代理可达性测试**（body: `{"proxy":"http://127.0.0.1:7890","url?":"https://..."}`）：经该代理发 HTTP GET（默认 gstatic `generate_204`），15s 超时，返回 `{ok,status|error}`，用于校验「拉取代理」是否可用 |

## 设置（统一配置）

左侧新增 **「设置」** 导航，把原先散落在「订阅管理」「合并 / 导出」等页面的配置集中到一处，并由**一套配置统一驱动**合并导出与网页订阅（`/sub`）：

- **全局 Top-N**：合并导出与 `GET /sub` 网页订阅**统一受此数值控制**（单一真相源）。设为 N 时两者都只输出评分最高的前 N 个节点；关闭（0 / 留空）则输出全部。WebUI 不再有独立的「导出 Top-N 开关」，避免多套配置互相打架。
- **代理**：开关 + 默认拉取代理地址（`http` / `https` / `socks5`）。该地址作为**服务端单一真相源**，用于抓取被墙的订阅源（替代原来浏览器 localStorage 里的「记住」复选框）；`add_subscriptions` 仅在校验（`client_with_proxy().is_ok()`）通过后才写入默认。
- **定时自动刷新**：自动刷新间隔（秒），等价于 `SUBHUB_AUTO_REFRESH_SEC`；0 / 留空则关闭。
- **测速引擎**：外部引擎二进制路径，等价于 `SUBHUB_ENGINE_BIN`；留空则仅做 TCP 连通性延迟。
- **自动移除**：连续不可用达到该阈值的节点在测速 / 刷新后自动熔断清理（见坏节点熔断清理）。

所有设置经 `POST /api/settings` 持久化到 `meta` 表，重启不丢。

## 算子转换（Transform）是什么、怎么用

**算子转换**借鉴自 **sub-store** 的「operators」概念：在「合并导出」之前，对节点集合套一条**声明式的处理管道**，按顺序执行 **筛选 → 重命名 → 排序**，从而把一堆杂乱的订阅整理成自己想要的样子，再导出。

典型用途：
- **筛选**：只要某个地区（如只要 HK）、只要某种类型（如只要 ss/trojan）、排除名字里带广告/测速的节点。
- **重命名**：把 `JP-xxx` 批量改成 `日本-xxx`，或用正则把节点名里的国旗/冗余前缀统一掉。
- **排序**：按延迟从低到高（配合测速结果），让最快的节点排在最前面，客户端自动选优。

### 在界面里怎么用
打开左侧 **「合并 / 导出」** 视图：
1. 「转换算子 (Transform)」面板里点 **+ 添加筛选**，选字段 / 包含或排除 / 匹配方式 / 值，可叠加多条（多条之间是「且」的关系）。
2. 排序选 `名称 / 延迟 / 类型` + 升/降序。
3. 重命名填「正则」与「替换为」（如 `.*HK-(.*)` → `HK-$1`）。
4. 选好下方「合并并生成新订阅」的格式，点 **合并并导出** 即可得到处理后的订阅文本（可复制 / 下载）。

### 在分享网址里怎么用（详见下一节）
算子在「本地订阅地址」里会被编码进网址参数，这样你分享 / 填进客户端的那个网址本身就已经带了筛选排序，客户端每次拉取都是处理好的结果。

### 在 API 里怎么用
`POST /api/export` 的 `transform` 字段支持 sub-store 式算子，按顺序执行 **筛选 → 重命名 → 排序**：

```json
{
  "format": "clash-meta",
  "transform": {
    "filters": [
      { "field": "region", "mode": "exclude", "match_": "exact", "value": "HK" },
      { "field": "name",   "mode": "include", "match_": "regex", "value": ".*JP.*" }
    ],
    "sort":   { "key": "latency", "desc": false },
    "rename": { "pattern": "JP-(.*)", "replacement": "日本-$1" },
    "min_bandwidth_bps": 5242880,
    "max_latency_ms": 300
  }
}
```
- `filters[].field`：`name` | `type` | `region` | `server`
- `filters[].mode`：`include`（保留匹配）| `exclude`（丢弃匹配）
- `filters[].match_`：`contains` | `regex` | `exact`
- `sort.key`：`name` | `latency` | `type` | `speed` | `score`；`sort.desc`：是否降序（`speed` 按下行带宽、`score` 按综合评分，二者均可用性主导——`available==false` 沉底）
- `rename`：`pattern` 为 Rust 正则，`replacement` 支持 `$1` `$2` 捕获组
- `min_bandwidth_bps`（可选）：带宽下限（**bps**）。导出时排除 `download_speed_bps` 低于该值的节点；**未测速（无带宽数据）的节点保留**，避免误删还没测的节点。
- `max_latency_ms`（可选）：延迟上限（**ms**）。导出时排除 `latency_ms` 高于该值的节点；**未测速（无延迟数据）的节点保留**。

## 本地订阅地址（直接拉取）

「合并 / 导出」视图最下方有一个 **「本地订阅地址（直接拉取）」** 面板。点 **生成地址** 会根据当前选择的**格式**和**算子**拼出一个网址，形如：

```
http://127.0.0.1:3005/sub?format=clash-meta&sort=latency&desc=1&rename_pat=.*HK-(.*)&rename_rep=HK-%241
```

把这个网址 **直接填进 mihomo / clash / v2rayN / sing-box 的「订阅」** 即可，客户端会把它当成普通订阅一样定时拉取——底层等价于「实时合并所有订阅 + 套一遍算子 + 导出」后返回纯文本，无 JSON 包装、无需鉴权。

- 默认端口 `3005`，可用 `SUBHUB_PORT` 覆盖（网址里的端口要跟着改）。
- 只合并部分订阅：加 `&sub=id1,id2`（订阅 id 见 `GET /api/subscriptions`）。
- 支持的算子参数：`sort`（`name`/`latency`/`type`/`speed`/`score`）、`desc`（`1` 降序）、`rename_pat` / `rename_rep`、`q`（按名称包含筛选）、`region`（按地区包含）、`type`（按类型精确）、`top_n`（保留评分最高的前 N 个，0 / 留空 = 全部，覆盖全局设置）、`min_bw`（带宽下限，**bps**，排除更慢的节点）、`max_lat`（延迟上限，**ms**，排除更慢的节点；未测速节点保留）。
- 注意：分享网址只编码**包含类**筛选（名称/地区用 `contains`、类型用 `exact`）；**排除 / 正则类**筛选无法编码进网址，遇到时会提示，需要那种效果请改用「合并并导出」手动导出。

### 导出格式
`clash-meta`（默认）· `clash` · `v2ray`（v2rayN outbounds 数组）· `sing-box`（outbounds）· `surge` · `base64`（clash yaml 的 base64）。

### 导出会自动过滤无效节点
`POST /api/export` 与 `GET /sub` 在序列化前都会先跑一遍 `export_filter()`（见 `core/src/export.rs`），统一用 `Proxy::is_usable()`（`is_exportable() && available != Some(false)`）判定，所有格式（clash / v2ray / sing-box / surge / base64）共用：

- **跳过不可导出类型**：`ProxyType::Other`（clash-meta / mihomo 会直接拒绝并报 `unsupport proxy type: other`）以及缺少必填字段（如 vmess 缺 `uuid`、trojan 缺 `password`）的节点；
- **跳过已测但不可用**的节点（`available == Some(false)`），但**保留未测**（`available == None`）节点，避免误删还没来得及测速的节点。

服务端会在日志打印「导出时去除 N 个无效节点」，便于核对导出结果。这能避免把好节点之外的垃圾节点（尤其是 `other` 类型的「伪节点」）导出到 mihomo / clash 后整份订阅被拒。

- **导出代理名自动去重**：clash / v2ray / sing-box / surge 都按**代理名**作唯一键。两个不同节点（如地区重命名规则把同地区节点都改成同一名字、或不同订阅本就重名）共享显示名时，客户端会报 `... is the duplicate name` 而拒绝整份配置。导出时首个同名节点保留原名，后续撞名追加 ` #2` / ` #3` 后缀，确保订阅可被正常加载（两个节点都会保留，仅名字不同）。

## 节点地区识别（region 与 outbound_country）

节点地区有两个来源，WebUI「地区」列优先显示真实出口地区、回退到名称推断：

- **`region`（`Proxy::region()`，名称推断）**：纯前端无引擎时也能用。按 **4 级优先级** 解析节点名（与 `server` 地址）得到 2 字母国家码：
  1. **国旗 emoji** → 直接映射成对应国家码（最高优先级，最可靠）；
  2. **国家/地区全名**（中英文子串匹配，如 `香港`/`hong kong`/`tokyo`/`东京`/`首尔`/`乌克兰` 等 ~90 条）；
  3. **3 字母机场码**（如 `NRT`/`LAX`/`SIN`）；
  4. **2 字母国家码** + 安全前缀变体（如 `JP-`、`hk_`、`US|` 这类「码+分隔符」写法，但整词 token 匹配避免把 `russia` 误判成 `us`）。
  词典已从真实节点库（3382 个节点）反查补全，未知地区从 1122 个降到 701 个；剩余 701 个多为上游「其他地区 / 无意义名」，正确归为 `OTHER`。
- **`outbound_country`（出口地区，真实 geo-IP）**：配置 `SUBHUB_ENGINE_BIN` 后由 `POST /api/geo-detect` 经引擎逐节点跑 geo-IP 通道得到，是**真实出口位置**，比名称推断更准。WebUI 地区列取值顺序：`outbound_country || region || "OTHER"`。

> 提示：仅装了引擎并跑过 `/api/geo-detect`，出口地区才会填充；否则地区列走 `region()` 名称推断。

## 已验证（端到端冒烟测试 `docs/test_subhub.py`）
- 混合内容导入（clash yaml + vmess/trojan/vless/ss URI 混排 + base64）→ 正确解析、去重
- **逐个订阅健康度**（借 Resin `SubscriptionResponse` 模型）：`status`(healthy/degraded/down/error/empty/untested/disabled/pending)、`node_count` / `healthy_node_count` / `unknown_node_count` / `avg_latency_ms` / `best_latency_ms` 均随节点派生
- 坏订阅（不可达源）→ `status:"error"` + `last_error` 填充；好订阅正确区分为 `untested` 而非 `down`
- **添加后自动健康度检测 + 测速**：导入 / 添加订阅后立即对节点跑 TCP 测速，`available` / `latency_ms` 自动写回（本机可达节点 `available:true`、不可达 `available:false`），健康度卡片随之即时刷新
- **通过代理拉取订阅**：`POST /api/subscriptions` 的 `fetch_proxy` 字段（或 WebUI 批量添加面板的「拉取代理」）支持 `http` / `https` / `socks5` 代理，用于抓取被墙的订阅源；代理地址无效会令该订阅 `status:"error"` 并写入 `last_error`
- **订阅刷新** `POST /api/subscriptions/:id/refresh` 生效（重拉源、更新 `last_checked_at`/`last_updated_at`；remote 订阅会沿用其存储的 `fetch_proxy`）
- **出口地区探测** `POST /api/geo-detect` 无引擎时安全返回空列表、不报错；配置引擎后写回 `outbound_country`
- **流媒体解锁判定** `POST /api/unlock-detect` 无引擎时安全返回列表、不报错；配置引擎后写回 `Proxy.unlock`
- **坏节点熔断清理** `POST /api/nodes/cleanup` 正确删除不可用节点（保留未测节点）
- **趋势数据** `GET /api/trends` 返回滚动快照序列，WebUI canvas 折线图渲染
- 仪表盘含全部字段（订阅数 / 可用·不可用·未测 / 平均·最佳延迟 / 类型环图数据 / 每订阅）
- 六种格式导出均成功（v2ray 为数组、sing-box 含 `outbounds`、surge 为 `Proxy =` 行）
- 算子管道：按地区排除 + 正则重命名均生效
- 测速：指向本机可达端口的节点 `available:true / tcp_ms≈0`；不可达域名 `available:false`；延迟写回节点记录
- 订阅管理：列表 + 删除均生效
- **SQLite 持久化**：重启服务后订阅与测速结果仍在；删除 `data/subhub.db` 即重置
- 节点按订阅来源归属：每个节点带 `sub_name`，WebUI「按订阅分组」按钮可分组查看（sub-store 分组模型）
- WebUI 静态页正常托管（含订阅列、解锁列、速度列、趋势图、清理按钮、分组按钮）
- **订阅按健康度排序**：WebUI 订阅列表默认按 `health_pct` 降序，最健康的订阅排最前（最新的「好订阅优先」体验）
- **导出过滤无效节点**：`export_filter()` 去掉 `Other` 类型 / 缺必填字段 / 已测不可用节点；导入「好 SS + 坏 Other」后 `/sub` 打印「导出时去除 N 个无效节点」并正确排除坏节点，规避 Sparkle `unsupport proxy type: other`
- **地区字段修复**：`/api/proxies` 现正确注入 `region`，节点表地区列不再是恒为 `OTHER` 的历史 bug
- **前端解锁摘要修复**：WebUI 用纯 JS `summaryUnlock()` 替代对 JSON 对象调用 Rust 方法 `p.unlock.summary()`（跨语言边界的方法/字段不匹配 bug），刷新订阅不再报 `p.unlock.summary is not a function`
- **代码质量**：`cargo clippy --release -p subhub-core -p subhub-server` **零警告**；`with_engine` 跳过不可导出节点（避免为其拉起外部引擎）；`cargo test -p subhub-core --lib` 通过
- **Tauri v2 原生窗口**：`cargo check` + `cargo build --release` 均通过（Windows 原生二进制已产出）
- **节点列表全局排序**：`GET /api/proxies` 支持 `?sort=name|latency|speed|score&desc=1`，排序在**分页之前对全部节点**执行（不再是只排当前页 50 个）；可用性主导——`available==false` 的节点无论按哪列都沉底。端到端冒烟注入 55 节点，验证 50 个不可用节点跨页边界正确全局沉底。
- **设置页 + 全局 Top-N 单一真相源**：合并导出与 `GET /sub` 网页订阅统一受全局 Top-N 控制；`buildShareUrl` 已把 `top_n` 拼进分享网址。
- **手动测速「仅测未测 / 仅失败」模式**：`POST /api/speedtest` 的 `mode` 支持 `all`（默认）/ `untested`（`last_tested_at==None`）/ `failed`（`available==Some(false)`），复用自动刷新的增量思路；WebUI 节点页提供「测速范围」下拉。
- **合并导入防任意覆盖**：`import_subscriptions` 仅按订阅源 URL 做幂等合并，不再用请求体里的 `id` 去定位 / 覆盖已有订阅（防任意覆盖）。
- **删除语义**：`DELETE /api/subscriptions/:id` 对不存在的 id 返回 404（不再静默 200 no-op），调用方可区分「真删除」与「无此订阅」。

## 代码审计记录（Round G，分模块逐条核对）

对 `model / parse / export / ops / speedtest / server` 做了逐模块审计，逐条核对并修复。完整审计表（含「已确认无需改 / 审查判定保留 / 本次修复」三态结论）见 [`docs/competitive-analysis-and-plan.md` §9](docs/competitive-analysis-and-plan.md)。本轮**新增修复**：

- **model.rs**：`status()` 整数溢出（提升 u64 + `node_count>0` 守卫）；`region()` 不再把 `russia` 误判成 `us`（2 字母国家码按整词 token 匹配）。
- **parse.rs**：`parse_ss` 增加 SIP002 合法性守卫（解出非空 `method` 才认定 SIP002，否则回退整段 base64）；补充 sing-box `transport`（ws/grpc/h2）解析。
- **export.rs**：`v2ray` 导出跳过不支持类型（不再生成 `freedom`/`direct` 兜底 outbound）。
- **ops.rs**：`apply()` 预先编译过滤器正则（不再每节点 `Regex::new`）；重命名 `\$`→`$$` 正确输出字面 `$`（补单测）。
- **server/lib.rs**：`geo_detect` / `unlock_detect` 由 O(N²) 反查改为 O(N) 的 `fingerprint→值` HashMap。
- **server/engine.rs**：`mixed-port` 配置由 `\n\` 续行改为 `writeln!` 拼接，消除 `multi_line_directives` 警告。
- **非审计项**：修复 `db.rs`/`engine.rs` 的非法 UTF-8 与乱码注释、双重 CR；按「仅改显示名」决策把误改的 `subhub*` 内部标识全部回退为 `subhub*`/`SUBHUB_*`/`subhub.db`。

**验证**：`cargo clippy --release -p subhub-core -p subhub-server` **0 警告**；`cargo test -p subhub-core --lib` 3 测试通过；`docs/test_subhub.py` 端到端 **ALL CHECKS PASSED**。

## 代码审计记录（Round Q，双代理全面复查）

对后端 Rust（core + server）与前端/脚本（webui + docs）分别做独立审查，逐条核对源码真伪后实施修复：

- **B1 `redact_url` 越界 panic（严重）**：旧实现用全串偏移去索引子串，`https://a@b` 这类「scheme 比 userinfo 长」的 URL 直接 panic 并毒化 store mutex，导致后续所有请求 500。改用 `split_once('@')`（无偏移、不可能 panic），并补回归单测。
- **B2 Mutex 毒化恢复**：全部 `lock().unwrap()`（约 60 处）替换为 poison 容忍的 `lock_ok()` 扩展 trait——一次 panic 不再永久打死整个服务；`core/speedtest.rs` 工作线程同样处理。
- **B3 幽灵节点**：订阅正文里无 `user:pass@` 的裸 `http(s)://host:port` 行（更新链接 / 规则 URL）不再被解析成不可用的 Http 节点；带 userinfo 的真实 http 代理不受影响。
- **B4 SSRF 防护**：`fetch_subscription_text` 增加 scheme 白名单（仅 http/https），`file://` 等一律前置拒绝。
- **B6 评分单一真相源**：`score_proxy` 移入 `subhub_core`（`core/src/score.rs`），server 侧改为薄包装；`ops::apply` 排序新增 `score` 分支（此前静默回退按名称排），UI 排序 / Top-N 导出 / 算子管道三方评分从此不可能不一致。
- **B8 base64 解析深度上限**：`parse_subscription` 递归解 base64 增加深度（3 层）与解码体积（16 MiB）上限，防构造输入递归炸栈。
- **前端 F1/F4/F5/F6**：节点列表加载失败在表格内显示错误（不再静默空白）；总数缩小后页码自动夹取到末页；仪表盘条形图 label HTML 转义（防节点名注入 XSS）；全局 Top-N 保存增加成功 / 失败反馈（不再静默失败）。
- **重复添加 URL 幂等合并（端到端测试新抓出）**：`POST /api/subscriptions` 重复添加同一 URL 此前会产生重复订阅条目；现改为按 `source` 就地更新（refresh 语义，`incremental_update` 保留存活节点已测健康数据；拉取失败时保留旧节点列表），与 `/api/import` 的合并行为一致。
- **脚本修复**：`docs/import_sparkle_subs.py` 字段名 `url`→`source`、`s['health']`→扁平 `SubSummary` 字段（原脚本必 KeyError）；`docs/test_subhub.py` speedtest 断言改为对象形状（`{results, removed, threshold}`），并新增跨页全局排序一致性、`top_n` 设置持久化往返、删除不存在订阅返回 404、同 URL 重复导入幂等合并、`mode=untested` 范围测速共 5 组端到端用例。

**验证**：`cargo test -p subhub-core -p subhub-server` **38 测试全过**（core 32 + server 6，本轮新增 12）；clippy 0 警告；release 构建通过。

## 代码审计记录（Round R，引擎运行时与数据完整性复查）

在 Round Q 全面修复基础上，对前几轮未覆盖的 `engine.rs`（引擎运行时）与「重复添加幂等合并」做专项复查，并落地一处可测性改进：

- **`engine.rs` 复查（结论：已加固，无需大改）**：逐一核对引擎进程管理——RAII `EngineGuard` 保证子进程与临时目录在所有退出路径（含 `spawn` 之后的早退 `?`）均被清理；`EngineDirSeq` 计数器 + pid 保证并发拉起目录互不覆盖；`engine_ready` 在引擎进程自行退出时立即返回（不再傻等整段超时）。**引擎配置 YAML 注入已安全**：节点 `name` 经 `to_clash_meta` 由 `serde_yaml` 序列化（自动引号转义），`proxy-groups` 引用处再叠加 `escape_yaml_scalar` 双保险，攻击者控制的节点名无法逃逸标量注入任意配置。
- **Per-sub 刷新错误上抛（结论：已满足）**：复查确认 `do_refresh_one` 失败时写入 `health.last_error` 并由 `refresh_subscription` 持久化，前端 `loadSubs` 重渲染经 `.sub-err` 红字展示，因此「webui 错误上抛」缺口实际已闭合，无需额外改动。
- **幂等合并提取为可单测纯函数（R2）**：此前「重复 URL 按 source 就地合并」逻辑只存在于 server 的 `add_subscriptions` 闭包内、仅由端到端覆盖。现抽出为 `subhub_core::ops::merge_subscriptions_by_source`（单一真相源，同时被 `/api/subscriptions` 与 `/api/subscriptions/import` 复用），并修复返回 `added` 计数在「重复添加（实为 refresh）」时被整份节点数虚高的小瑕疵——现在只统计**真正新建**订阅的节点数。核心新增 4 个单测覆盖：新建计数、重复添加不重不漏、拉取失败时保留旧节点、refresh 保留存活节点已测健康。

**验证**：`cargo clippy --release -p subhub-core -p subhub-server` 0 警告；`cargo test -p subhub-core -p subhub-server` **42 测试全过**（core 36 + server 6，本轮新增 4）；release 构建通过；`docs/test_subhub.py` 隔离库端到端 **ALL CHECKS PASSED**（含同 URL 重复导入幂等合并用例）。

## 代码审计记录（Round S，地区识别增强 + 测速进度流式化）

针对「大量节点被分入 OTHER」「测速无进度提示」「测速点击失败 405」三处问题，实施如下修复：

- **地区识别增强（`core/src/model.rs` 的 `region()`）**：重写为 4 级优先级匹配——① 国旗 emoji 直接映射；② 中英文国家/地区全名子串表（扩充到 ~90 条，含从真实节点库反查补全的乌克兰、哈萨克、柬埔寨、缅甸、老挝等）；③ 3 字母机场码表；④ 2 字母国家码 + 安全前缀变体（整词 token 匹配，杜绝 `russia`→`us` 误判）。真实节点库（3382 节点）验证：OTHER 从 1122 降到 701。
- **WebUI 地区列取值顺序（`webui/app.js`）**：改为 `outbound_country || region || "OTHER"`，优先展示真实出口地区。
- **测速改为 SSE 流式（`server/src/lib.rs` + `webui/app.js` + `index.html` + `style.css`）**：`/api/speedtest` 由一次性 JSON 响应改为 `GET` + `text/event-stream` 流式；后端 `tcp_ping_all(..., Some(&cb))` 每测完一个节点推送 `progress` 事件（含 `done/total/name/available/latency_ms/bandwidth_bps`），结束推 `done` 汇总；前端用 `fetch` + `ReadableStream` 消费，实时渲染进度条与「当前节点 / Ping / 带宽」。`server` 侧提取 `run_engine_passes()`，移除一次性 `SpeedTestResp` 结构。
- **修复测速进度「到 x/x 不结束」（`server/src/lib.rs` + `server/src/engine.rs` + `webui/app.js`）**：原进度只统计 TCP 阶段，配置引擎后「HTTP 延迟 + 带宽」阶段在后台静默运行、无反馈，进度条满格却迟迟不结束。改为将进度总量贯穿三阶段——无引擎 `total = 节点数`，有引擎 `total = 节点数 × 3`（TCP / HTTP 延迟 / 带宽各占一段），引擎两阶段通过新增的 `on_progress` 回调（`engine_http_latency` / `engine_bandwidth`）逐节点回报；每个 `progress` 事件带 `phase` 字段，前端据此标注「测 HTTP 延迟 / 测带宽」且不再把测量中的节点误显为「超时」。进度条现在**真正到 100% 才结束**。
- **修复测速 405**：路由由 `post(speedtest)` 改为 `get(speedtest)`（前端用 `fetch` GET 消费 SSE，原 POST 导致 405 Method Not Allowed）。
- **`core/src/speedtest.rs`**：`tcp_ping_all` 进度回调签名由泛型 `F` 改为 `Option<&(dyn Fn(TestProgress) + Sync)>`，消除编译期模糊（E0283/E0425），闭包标注 `subhub_core::speedtest::TestProgress`。
- **单测**：`core/src/model.rs` 新增 `region_resolves_common_naming_styles` / `region_prefix_variant_safe_from_false_positive`（`ukraine-kyiv`→`UA`、`ruse-node`→`OTHER`）/ `region_flag_emoji_maps_to_code` / `region_more_countries_from_real_db`。

**验证**：`cargo clippy --release -p subhub-core -p subhub-server` 0 警告；`cargo test -p subhub-core --lib` 全部通过（含本轮新增地区单测）；release 构建通过；WebUI 测速进度条与实时节点信息正常显示，405 不再出现。

## 复用的第三方引擎（可选）
测速与真实连通性验证不重写协议，调用本地 **mihomo (clash-meta)** 或 **sing-box** 作为连接引擎 —— 与 BestSub 思路一致。通过 `SUBHUB_ENGINE_BIN` 开启。
存活探测走 gstatic `generate_204`（期望 204）、出口地区走 7 个 geo-IP 通道、流媒体解锁（TikTok `region` 解析 + Netflix/Disney/YouTube/ChatGPT 启发式）、带宽测速走 cloudflare `__down` 端点 —— 这些探测目标与判定逻辑均**直接移植 / 借鉴自 BestSub 成熟实现**（见 `core/src/resources.rs` 文件头署名）；逐个订阅健康度与坏节点熔断清理借鉴 **Resin**；合并导出算子管道借鉴 **sub-store**。
