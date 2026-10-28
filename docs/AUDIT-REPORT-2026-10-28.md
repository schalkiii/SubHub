# SubHub 项目审计报告（2026-10-28）

> 审计范围：`core/`、`server/`、`app/`、`webui/`、`tools/`、`docs/`、根目录文档与构建配置。
> 审计方式：全量源码阅读 + 关键路径推演 + `cargo test` / `cargo clippy` 基线验证 + 文档与实现逐条比对。
> 本文档既记录问题，也记录本轮实际处置结果（见第六节）；未修复项明确标为 pending。

---

## 一、现状描述（客观事实）

### 1.1 规模

| 范围 | 文件数 | 代码量 |
|---|---|---|
| Rust（core + server + app） | 19 个 `.rs` | 10,554 行 |
| WebUI（原生 JS/HTML/CSS） | 3 个 | 2,681 行 |
| 辅助脚本（Python） | 6 个 | — |
| 根目录文档 | `README.md` 46 KB / `CHANGELOG.md` 27 KB / `AGENTS.md` 2.3 KB | — |
| 规划与审计文档 | `docs/competitive-analysis-and-plan.md` 48 KB + 4 个 review HTML | — |

### 1.2 结构

- `core/`：纯库。`model.rs`（统一节点/订阅模型、指纹、地区识别）、`parse.rs`（clash YAML / sing-box JSON / 各类 URI）、`ops.rs`（筛选/排序/重命名/增量合并）、`export.rs`（6 种格式）、`score.rs`（综合评分）、`speedtest.rs`（TCP 探测）、`resources.rs`（探测目标与解锁判定）。
- `server/`：axum HTTP 服务。`lib.rs`（AppState、调度器、持久化与测速核心）、`engine.rs`（mihomo/sing-box 外部引擎）、`db.rs`（SQLite）、`handlers/*`（订阅、节点、测速、探测、维护、设置）。
- `app/`：Tauri v2 窗口壳，后台线程启动 server，窗口加载 `http://127.0.0.1:<port>`。
- `webui/`：无构建步骤的原生前端，由 axum 托管。
- 数据：SQLite 整订阅 JSON 快照（`subscriptions` 表）+ 设置键值（`meta` 表），写操作后全量落盘。

### 1.3 质量基线（本轮实测）

- `cargo test -p subhub-core -p subhub-server`：**85 个测试全通过**（core 71 + server 14）。
- `cargo clippy --release -p subhub-core -p subhub-server --all-targets -- -D warnings`：**0 警告**。
- 无 CI 配置、无 `rustfmt.toml` / `clippy.toml`、无 `LICENSE`。
- 端到端脚本 `docs/test_subhub.py` 仍按旧版 `/api/speedtest` JSON 契约编写，**当前会失败**（见 B6）。

### 1.4 整体水平

分层清晰、核心库无 IO、回归测试意识强（大量注释直接写明历史 bug 与修复原因），属于“单人长期迭代、功能密度高、文档偏多但部分与实现脱节”的项目。主要风险不在架构崩坏，而在**数据保护边界、协议导出保真度、测速状态机与文档契约漂移**。

---

## 二、七维度结论

### 维度 1：文档一致性 —— 发现问题

- README 与代码不一致：测速进度总量（声称 `节点数 × 3`，实际带宽阶段已关闭）、带宽能力（声称采集真实带宽，实际 `plan.bandwidth=false`）、`/api/proxies` 的 `desc=1`（服务端不认）、`top_n=0` 语义（声称覆盖全局，实际回落全局）、节点列表列（声称含速度列，实际已移除）、`/api/settings` 字段列表不完整。
- `app/build.rs` 注释声称“前端被重新嵌入 exe”，与 `AGENTS.md`“WebUI 不嵌入、直接读磁盘”矛盾（且当前静态目录使用编译期源码路径，分发场景确实会 404，见 B5）。
- `docs/test_subhub.py` 的测速断言基于旧 JSON 契约，与现行 SSE 实现冲突。

### 维度 2：文档缺漏 —— 发现问题

- 缺少**协议支持矩阵**：哪些协议可解析、可导出到哪些格式、哪些字段在导入/导出中会丢失（VLESS Reality、SS plugin、Hysteria2 obfs、TUIC 拥塞控制等）。
- 缺少**部署/分发说明**：`subhub-app.exe` 脱离源码目录后静态资源从哪里加载、需要携带哪些文件。
- `SettingsResp` 实际字段（`bind_addr` / `external_host` / `github_user` / `has_github_token` / `lan_ip`）未在 README API 表完整列出。
- `core` 的 `fingerprint` 身份规则（哪些字段参与、改名会换身份）只在代码注释中，未进文档。

### 维度 3：架构优化点 —— 有限问题

- 分层与依赖方向正确（app → server → core，core 无 IO）。
- `AppState` 以十余个独立 `Arc<Mutex<T>>` 承载配置，新增字段需 7 处同步；可考虑收敛为 `Settings` 子结构，但不是本轮必要改动。
- 静态 WebUI 目录使用 `env!("CARGO_MANIFEST_DIR")` 编译期路径，导致**分发包无法独立运行**（B5），是本维度唯一高优先级问题。
- `persist_all()` 每次全量重写订阅表；当前规模（数千节点）可接受，无需改。

### 维度 4：代码写法 —— 发现问题

- 业务正确性问题见第五节 A1–A9、B1–B8；其中 A1（刷新 0 节点清空订阅）、A3（重复添加全量重测）、A4（TCP-only 健康检查误判）会直接造成用户数据或状态损失。
- 大量无意义空行与逐行注释（`server/lib.rs`、`db.rs`、`engine.rs` 尤其明显），降低可读性；不影响正确性。
- `handlers/*` 使用 `use crate::*;` 引入全部父模块符号，边界模糊；可接受但不理想。
- `download_speed_bps` 命名与单位（bytes/sec）不一致，已有注释说明，属历史包袱；本轮不重命名，避免破坏持久化 JSON。

### 维度 5：测试覆盖率 —— 有缺口

已有 85 个单测，覆盖模型/解析/导出/算子/评分/DB/测速/引擎配置等。缺口：

- **无 handler 级测试**：刷新空结果保护、`top_n=0` 覆盖、`desc=1` 解析、SSE 进度总量均无测试。
- **无引擎运行时测试**：`probe_all_shared` / `delay_via_api` 仅有配置生成测试。
- **协议矩阵缺口**：VLESS Reality、SS plugin、Hysteria2/TUIC 的跨格式导出无断言。
- **前端无自动化测试**：geo/unlock 结果字段错配只能靠人工发现。
- **端到端脚本已失效**（B6），当前不能作为回归门禁。

### 维度 6：测试严谨性 —— 多数严谨，局部失效

- 优点：大量测试注释直接对应真实故障（`redact_url` panic、SS cipher、区域误判、排序沉底等），断言具体而非“跑通即过”。
- 问题：`docs/test_subhub.py` 与实现漂移后仍被 README 当作“已验证”凭证；这类“脚本还在但跑不过”的状态比没有脚本更危险。
- 缺少对**导出保真度**的自动化比对：例如同一 SS 节点在 6 种格式中的 method 是否一致、sing-box 的 hysteria2 是否带 TLS。

### 维度 7：工程规范 —— 发现问题

- 无 CI（GitHub Actions）执行 `cargo test` / `clippy` / `fmt --check`，质量门禁依赖人工。
- 根目录存在 0 字节 `nul` 文件（历史命令产物，已被 `.gitignore` 忽略但仍在工作区）。
- `docs/review-round-L/M/N/O.html` 为历史审查产物，与 README/CHANGELOG 的审计记录重复，存在多份“历史真相”。
- `docs/competitive-analysis-and-plan.md` 同时承担竞品分析、路线图、审计日志三种角色，随版本推进已难维护。
- 版本号固定 `0.1.0`，无发布标签策略说明；`Cargo.lock` 已提交（二进制项目正确）。

---

## 三、问题清单

严重程度定义：
- **高**：导致数据丢失、功能不可用、结果错误或用户被系统性误导。
- **中**：局部行为错误、性能浪费、契约不一致，有规避路径。
- **低**：可读性、历史残留、边界瑕疵。

| ID | 位置 | 问题 | 严重度 | 影响范围 |
|---|---|---|---|---|
| A1 | `server/src/handlers/subscription.rs:628-648` | 刷新时若远端返回 200 但解析出 0 个节点，`incremental_update` 返回空列表并直接覆盖，**整份订阅节点被清空** | 高 | 所有远程订阅；上游临时空响应/格式变更即触发，且随后 `persist_all` 落库 |
| A2 | `core/src/ops.rs:315-330` | `incremental_update` 只保留 `latency_ms/available/download/bandwidth_measured/last_tested_at/outbound_country/unlock`，**丢失 `tcp_latency_ms` / `http_latency_ms` / `consecutive_failures`** | 高 | 每次刷新/自动刷新后 TCP/HTTP 两列变空、自动移除计数被清零 |
| A3 | `server/src/handlers/subscription.rs:150-173` | 重复添加同一 URL 时对**该订阅全部节点**重新测速（`new_proxies` 取自整份新解析结果），与“只测新增节点”的增量语义矛盾 | 高 | 重复添加/批量粘贴含旧链接时，数千节点全量重测，耗时与网络压力剧增 |
| A4 | `server/src/lib.rs:1097-1115` | 已配置引擎时，TCP-only 健康检查对 `available == None` 的节点使用 `unwrap_or(false)`，**把未测节点判成不可用并累加失败计数** | 高 | 定时健康检查会把未测/正常节点刷红，叠加自动移除后可能误删节点 |
| A5 | `server/src/handlers/speedtest.rs:222-330` + `webui/app.js:73-142` | 带宽探测已关闭，但 SSE `total` 仍按 `节点数 × 3` 计；HTTP 阶段结束后进度最高只有 66%，**进度条永远到不了 100%** | 高 | 所有配置了引擎的测速；用户误以为任务卡死 |
| A6 | `webui/app.js:1523`、`webui/app.js:1552` | 出口地区检测读取 `c.country`（后端返回 `outbound_country`）；解锁检测读取 `u.services`（后端返回摘要字符串 `unlock`） | 高 | 两个按钮成功后仍提示“未检测到”，功能看似完全失效 |
| A7 | `core/src/export.rs:318`、`:430`、`:503` | SS 导出到 v2ray / sing-box / surge 时只读 `p.method`，YAML 导入节点的加密方法存在 `p.cipher`，会**回退成默认 `aes-256-gcm`** | 高 | 所有从 clash YAML 导入的 SS 节点；导出的 v2ray/sing-box/surge 配置无法连接 |
| A8 | `core/src/export.rs:434-453` | sing-box 导出 Hysteria2 / Tuic 时只有 `p.tls == Some(true)` 才写 `tls`，但这两个协议**永远要求 TLS**，解析侧也未置 true | 高 | sing-box 用户导出后整份配置校验失败/节点不可用 |
| A9 | `core/src/parse.rs:561-588` + `core/src/export.rs:132-140` | VLESS REALITY 的 `pbk` / `sid` / `security=reality` 完全未解析、未导出 | 高 | 当前订阅中占比很高的 REALITY 节点导出后变成普通 TLS，必然握手失败；引擎测速也会判死 |
| B1 | `server/src/lib.rs:325` | `/api/proxies` 的 `desc` 声明为 `Option<bool>`，文档与调用方使用的 `desc=1` 会触发 400 | 中 | 外部脚本/手工 API 调用；WebUI 用 `true/false` 暂未触发 |
| B2 | `server/src/handlers/maintenance.rs:144`、`:236` | `top_n=0` 被 `filter(|n| *n > 0)` 丢弃后回落全局设置，**无法用 0 临时关闭 Top-N**，与 README 相反 | 中 | `/api/export`、`/sub` 的一次性覆盖语义 |
| B3 | `server/src/handlers/nodes.rs:170`、`:101` | `/api/proxies` 与 `/api/nodes/top` 注入的 `region` 是名称推断值，而列表列/筛选/分布使用 `effective_region()`；外部消费者拿到两个口径 | 中 | API 一致性；UI 因自行回退暂未出错 |
| B4 | `core/src/model.rs:832-867` | 指纹未包含 ws `host` / grpc `service_name` / reality 参数；同服务器同 UUID 但 Host 不同的节点会被误判重复 | 中 | 合并去重可能丢节点；删除节点可能误删同名同 UUID 的另一条 |
| B5 | `server/src/lib.rs:1835-1838` + `app/build.rs:5` | 静态目录固定为编译期 `CARGO_MANIFEST_DIR/../webui`；exe 分发到无源码的机器后 WebUI 404；`build.rs` 注释与实际的“磁盘直读”矛盾 | 中 | 发布/迁移场景；本机开发不受影响 |
| B6 | `docs/test_subhub.py:225-241` | 仍按旧 JSON 契约调用 `/api/speedtest`（现为 GET + SSE），脚本必然失败 | 中 | 端到端回归门禁失效 |
| B7 | `core/src/export.rs:372`、`:455-472` | sing-box 的 uTLS 字段写成 `utls_fingerprint`（sing-box 期望 `utls: {enabled, fingerprint}`）；socks/http outbound 未导出 username、http 未导出 TLS | 中 | sing-box 导出保真度 |
| B8 | `webui/index.html:197-202`、`README.md:132/156/214/229`、`core/src/score.rs:29-43` | 带宽探测已关闭，但 UI 仍有“最小带宽”、文档仍宣传带宽采集、评分 40% 带宽权重永远拿不到分 | 中 | 用户按文档配置却无效；评分语义不透明 |
| B9 | `core/src/parse.rs:555-557`、`core/src/export.rs:122-130` | vmess 未解析 `sni/peer/allowInsecure`；clash YAML 未解析 `client-fingerprint`；vmess/trojan 导出不写 fp | 低 | 部分 TLS/WS 节点 SNI/指纹丢失 |
| B10 | `core/src/model.rs:402` | `NAME_TABLE` 中 `("hong", "HK")` 为短子串，可能误判含 `hong` 的其他词 | 低 | 地区分组误差（历史遗留，CHANGELOG P19 已登记暂缓） |
| B11 | `server/src/handlers/subscription.rs:687` | 刷新不存在的订阅返回 HTTP 200 + `status:error`，而删除/测速返回 404 | 低 | API 契约不一致 |
| B12 | `server/src/handlers/maintenance.rs:176` | 导出 `count` 仅按 `export_filter` 统计，未扣除 v2ray/sing-box/surge 因协议不支持而跳过的节点 | 低 | 展示计数偏大 |
| B13 | `server/src/handlers/subscription.rs:584`、`:310` | `SubExportItem.id` 注释称用于幂等恢复，实际按 source 合并；导入会重置用量与健康时间 | 低 | 代码注释误导 |
| B14 | `server/src/handlers/settings.rs:188-204` | 配置导出包含 `lan_ip` / `has_github_token` 等瞬时/派生字段，导入端忽略 | 低 | 备份文件含机器相关信息 |
| B15 | 根目录 `nul`、`docs/review-round-*.html` | 0 字节垃圾文件；4 份历史审查 HTML 与 README/CHANGELOG 重复 | 低 | 工作区整洁度与“唯一真相” |
| B16 | `docs/competitive-analysis-and-plan.md` | 竞品分析、路线图、审计日志三合一，已与当前状态脱节 | 低 | 新接手者需辨别历史信息 |
| B17 | `webui/index.html:180-185` | 合并/导出页排序下拉只有 name/latency/type，未暴露 score/speed（README 声称支持） | 低 | 功能可发现性 |
| B18 | `server/src/handlers/speedtest.rs:146` | Gist raw URL 使用设置里的 `github_user`；若 token 属于另一账号，URL 与 Gist 实际所有者不符 | 低 | 多账号场景 |
| B19 | `core/src/parse.rs:677-712` | Hysteria2 / TUIC 未解析 obfs、alpn、拥塞控制、insecure 等参数（P19 已登记暂缓） | 低 | 少数混淆/调优节点导出后不可用 |
| B20 | `core/src/parse.rs:612-666`、`core/src/export.rs` | SS `plugin` / `plugin-opts`（obfs、v2ray-plugin）未解析，导出即丢插件 | 低 | 使用 SS 插件的节点导出后不可用 |

---

## 四、改进建议（具体做法 / 文件清单 / 验证方式）

### P0（数据与功能正确性）

**S1 刷新空结果保护（A1）**
- 做法：`do_refresh_one` 在 `fetched` 为空时视为失败：不替换节点，写入 `last_error`（如“订阅返回 0 个节点，已保留原有节点”），health 时间戳照常更新。
- 文件：`server/src/handlers/subscription.rs`。
- 验证：新增单测（提取纯函数或在 handler 层模拟）断言“空 fetched + 旧节点 → 旧节点保留、last_error 有值”；端到端用返回空正文的本地 URL 刷新。

**S2 增量更新保留新字段（A2）**
- 做法：`incremental_update` 同步复制 `tcp_latency_ms`、`http_latency_ms`、`consecutive_failures`。
- 文件：`core/src/ops.rs`。
- 验证：新增单测，构造带三类字段的旧节点，刷新后逐项断言相等。

**S3 重复添加只测新增（A3）**
- 做法：`merge_subscriptions_by_source` 返回 `(created, added_proxies, new_nodes)`；`add_subscriptions` 用 `new_nodes` 而非整份 `new_subs` 测速。`import_subscriptions` 忽略第三项。
- 文件：`core/src/ops.rs`、`server/src/handlers/subscription.rs`。
- 验证：更新现有 4 个合并单测；新增“重复添加仅返回新增节点”断言。

**S4 TCP-only 健康检查不误判（A4）**
- 做法：已配置引擎且未跑引擎时：TCP 失败 → 判不可用并累加失败；TCP 成功 → 保持原判定（`None` 仍为 `None`），只更新 TCP 延迟与时间戳，不累加失败。
- 文件：`server/src/lib.rs`。
- 验证：新增 3 条单测（None→保持 None；Some(true)→保持 true；TCP 失败→false + 计数）。

**S5 SSE 进度修正（A5）**
- 做法：后端 `unit_total = 节点数 × (has_engine ? 2 : 1)`，移除带宽阶段 tick；前端在收到 `Done` 时强制进度 100%。同步移除无用的 `with_bw` 统计。
- 文件：`server/src/handlers/speedtest.rs`、`webui/app.js`。
- 验证：无引擎/有引擎两种情况人工观察；单测提取 `progress_total(node_count, has_engine)` 断言。

**S6 前端结果字段修正（A6）**
- 做法：`c.outbound_country`；`u.unlock && u.unlock !== "—"`。
- 文件：`webui/app.js`。
- 验证：无引擎时点击两个按钮，应提示“需要配置引擎”而不是“未检测到”；有引擎时计数正确。

**S7 导出保真（A7/A8）**
- 做法：新增 `ss_method(p)`（`method.or(cipher)`，最后默认）；v2ray/sing-box/surge 全部改用；sing-box 的 hysteria2/tuic 强制输出 TLS；解析 hy2/tuic 时置 `tls=true`。
- 文件：`core/src/export.rs`、`core/src/parse.rs`。
- 验证：新增单测分别断言三种格式含真实 cipher、sing-box hysteria2/tuic 含 `"enabled": true`。

**S8 协议补全：VLESS REALITY（A9）**
- 做法：模型新增 `reality` / `reality_public_key` / `reality_short_id`（带 `serde(default)`）；URI 解析 `security=reality&pbk=&sid=`；clash 解析/导写 `reality-opts`；v2ray 导写 `security: reality` + `realitySettings`；sing-box 导写 `tls.reality` + `utls`；指纹纳入 pbk/sid。
- 文件：`core/src/model.rs`、`core/src/parse.rs`、`core/src/export.rs`、`core/src/ops.rs`（指纹相关测试）。
- 验证：新增 URI→clash / sing-box / v2ray 三组单测；真实 REALITY 节点手工导出比对。

### P1（契约一致性）

**S9 `/api/proxies?desc=1`（B1）**
- 做法：`ListQuery.desc` 改为 `Option<String>`，统一 `parse_desc()` 支持 `1/true`。
- 文件：`server/src/lib.rs`、`server/src/handlers/nodes.rs`。
- 验证：单测覆盖 `1` / `true` / `false` / 缺省。

**S10 `top_n=0` 覆盖语义（B2）**
- 做法：提取 `resolve_top_n(param, global)`：参数存在时 0 表示“全部”，缺省才回落全局。
- 文件：`server/src/lib.rs`（helper）、`server/src/handlers/maintenance.rs`。
- 验证：单测覆盖 `None/0/10` × `global 0/50` 六种组合。

**S11 region 口径统一（B3）**
- 做法：`/api/proxies` 与 `/api/nodes/top` 注入 `effective_region()`；保留 `outbound_country` 独立字段。
- 文件：`server/src/handlers/nodes.rs`、`server/src/handlers/maintenance.rs`。
- 验证：端到端断言“出口 HK、名字 OTHER”的节点 `region == "HK"`。

**S12 指纹补全（B4/B9）**
- 做法：对 vmess/vless/tuic 在基础凭证串后**按需追加** `h=<host>`、`svc=<service_name>`、`pk/sid`（空值不追加，避免全量指纹迁移）；vmess 解析 `peer/sni/allowInsecure`；clash 解析 `client-fingerprint`。
- 文件：`core/src/model.rs`、`core/src/parse.rs`、`core/src/export.rs`。
- 验证：新增“同 UUID 不同 Host 不合并”单测；现有合并测试仍通过。

**S13 sing-box 导出修正（B7）**
- 做法：`utls` 改为对象；socks/http 导出 username；http 按 `tls` 输出 TLS。
- 文件：`core/src/export.rs`。
- 验证：单测断言 `"utls"` 对象、username、http tls。

### P2（工程与文档）

**S14 WebUI 分发兜底（B5）**
- 做法：静态资源改为“磁盘优先（env `SUBHUB_WEBUI_DIR` → 源码 `webui/` → exe 同级 `webui/`）→ 内嵌 `include_bytes!` 兜底”的自定义 handler；替换 `ServeDir`；修正 `app/build.rs` 注释。
- 文件：`server/src/lib.rs`、`app/build.rs`、`README.md`。
- 验证：临时改 `SUBHUB_WEBUI_DIR` 指向副本可生效；把 exe 复制到空目录仍能打开 UI。

**S15 端到端脚本修复（B6）**
- 做法：新增 SSE 读取函数，GET `/api/speedtest?...`，解析 `progress`/`done` 事件并对 `done` 断言。
- 文件：`docs/test_subhub.py`。
- 验证：隔离 DB 启动 server 后脚本全绿。

**S16 带宽口径收敛（B8）**
- 做法：UI 移除“最小带宽”输入；README/CHANGELOG 明确“当前版本不采集新带宽，评分带宽分量仅对历史实测数据生效”；保留 API 字段兼容。
- 文件：`webui/index.html`、`webui/app.js`、`README.md`、`CHANGELOG.md`。
- 验证：页面无失效控件；文档不再出现“真实下载带宽”承诺。

**S17 其他一致性（B10–B20）**
- 删除 `("hong","HK")`；刷新不存在返回 404；`count` 注释修正；`SubExportItem.id` 注释修正；merge 排序下拉补 score；Gist URL 注释风险；文档补协议矩阵与设置字段。
- 验证：对应单测/手工调用。

**S18 工程规范（维度 7）**
- 清理 `nul`（需用户确认）；将 `docs/review-round-*.html` 归档或在文档索引标注“历史”；补 CI（test + clippy + fmt）；统一 `cargo fmt`；补 LICENSE。
- 验证：CI 绿、`cargo fmt --check` 通过。

---

## 五、优先级与并行性

| 优先级 | 条目 | 可并行性 |
|---|---|---|
| P0 | S1、S2、S3、S4、S5、S6、S7、S8 | S2/S7/S8 可并行（core）；S1/S3/S4/S5 涉及 server 多文件，需串行协调；S6 独立（webui） |
| P1 | S9、S10、S11、S12、S13 | S9/S10/S11 同在 server，串行；S12/S13 在 core，可并行 |
| P2 | S14、S15、S16、S17、S18 | S14（server）与 S15（docs 脚本）可并行；S16/S17 涉及文档，最后串行 |

**并行分组（文件无重叠）**
1. Core 组：`core/src/ops.rs`、`model.rs`、`parse.rs`、`export.rs`。
2. Server 组：`server/src/lib.rs`、`handlers/subscription.rs`、`handlers/speedtest.rs`、`handlers/nodes.rs`、`handlers/maintenance.rs`。
3. WebUI 组：`webui/app.js`、`webui/index.html`。
4. Docs/脚本组：`README.md`、`CHANGELOG.md`、`AGENTS.md`、`docs/test_subhub.py`、`app/build.rs`。

依赖关系：A1/A3 依赖 core 的 merge 返回签名（S3）；A5 依赖 server + webui 同时改；S8 依赖 core 模型字段，之后 server 无需改；S14 与 S16 都改 `server/lib.rs` 与 `webui`，需串行收尾。

---

## 六、本轮实际处置结果

> 修复后基线：`cargo test -p subhub-core -p subhub-server` **102 个测试全通过**（core 84 + server 18）；clippy `-D warnings` 0 警告；隔离 DB 端到端 `docs/test_subhub.py` **ALL CHECKS PASSED**；浏览器冒烟（仪表盘 / 节点页 / 合并页 / 设置页、geo/unlock 成功路径）无控制台错误。

### 6.1 已修复（changed-and-verified）

| ID | 处置 | 验证 |
|---|---|---|
| A1 | `do_refresh_one` 与 `add_subscriptions` 均把「200 + 0 节点」视为失败：刷新保留旧节点并记 `last_error`；重复添加走 `merge_subscriptions_by_source` 的失败分支，同样保留旧节点 | 端到端脚本 `check_refresh_empty`：空正文刷新后节点数仍为 1、错误已记录；重复添加同样保留 |
| A2 | `incremental_update` 复制 `tcp_latency_ms` / `http_latency_ms` / `consecutive_failures` | 新增单测 `incremental_update_preserves_latency_columns_and_failure_streak` |
| A3 | `merge_subscriptions_by_source` 返回 `fresh_nodes`；`add_subscriptions` 只测这些节点 | 4 个合并单测更新并断言 `fresh`；重复添加不再全量重测（代码路径） |
| A4 | `persist_results` 的 TCP-only 分支只降级：TCP 成功时保持原 `available`（None 仍 None），不累加失败 | 新增单测 `tcp_only_health_check_preserves_engine_verdict`（None/true/TCP 失败三种） |
| A5 | SSE `total = N × (has_engine ? 2 : 1)`；移除带宽 tick；前端收到 `done` 强制 100% | 单测 `sse_progress_total_matches_active_stages`；端到端观察到 `total` 等于节点数（无引擎） |
| A6 | geo 读 `outbound_country`；unlock 读摘要 `unlock` | 浏览器内注入 mock 响应：geo 显示「1/1 已识别」，unlock 显示「1 个节点」 |
| A7 | 新增 `ss_method()`，v2ray/sing-box/surge 统一使用 | 单测 `ss_cipher_fallback_used_across_all_formats` |
| A8 | 解析 hy2/tuic 置 `tls=true`；sing-box 强制输出 TLS 块 | 单测 `singbox_hysteria2_and_tuic_always_emit_tls` |
| A9 | 模型 + URI/clash 解析 + 三种导出格式支持 REALITY；缺公钥视为不完整；指纹纳入 reality | 单测 `vless_reality_uri_is_parsed` / `clash_reality_and_fingerprint_are_parsed` / `reality_exported_to_clash_singbox_v2ray`；端到端 REALITY 导出断言 |
| B1 | `ListQuery.desc` 改为字符串，统一 `parse_desc()` 支持 `1/true` | 单测 `parse_desc_accepts_documented_and_webui_forms`；端到端 `?desc=1` 降序断言 |
| B2 | 新增 `resolve_top_n()`：显式 0 = 全部并覆盖全局，缺省才回落 | 单测 `resolve_top_n_distinguishes_zero_override_from_absent`；端到端 global=2 → 2、`top_n=0` → 7 |
| B3 | `/api/proxies` 与 `/api/nodes/top` 注入 `effective_region()` | 端到端 `HK-VmessB region = HK`；代码审查 |
| B4 | 指纹按需追加 WS Host / gRPC service / REALITY 参数 | 单测 `fingerprint_distinguishes_ws_host_and_grpc_service` / `fingerprint_includes_reality_keys` |
| B5 | 静态资源改为磁盘优先 + 内嵌兜底；新增 `SUBHUB_WEBUI_DIR`；修正 build.rs 注释 | 以空目录作为 `SUBHUB_WEBUI_DIR` 启动，`/` 与 `/app.js` 仍由内嵌副本正常返回；未知路径 404 |
| B6 | 端到端脚本改为 GET + SSE 消费，断言 `Done` 汇总 | 脚本 ALL CHECKS PASSED |
| B7 | sing-box uTLS 改为对象；SOCKS/HTTP 导出用户名；HTTP 按 tls/443 导出 TLS | 单测 `singbox_utls_is_object_and_http_exports_auth_and_tls` |
| B8 | 移除 UI「最小带宽」；README/CHANGELOG/页面提示明确「当前不采集新带宽」；API 字段保留兼容 | 浏览器合并页生成 URL 不含 `min_bw`；文档 grep 无「真实带宽」承诺 |
| B9 | vmess 解析 `peer/sni/allowInsecure`；clash 解析 `client-fingerprint`；vmess/trojan clash 导出 fp | 单测 `vmess_peer_is_used_as_sni`；clash fp 测试 |
| B10 | 删除 `("hong","HK")` 短子串 | 单测 `region_no_false_positive_from_hong_substring` |
| B12 | README 明确 `count` 不含各格式自身跳过数 | 文档 |
| B13 | `SubExportItem.id` 注释改为「仅供参考，按 source 合并」 | 代码审查 |

### 6.2 未修复 / 待决（pending / out-of-scope）

| ID | 状态 | 原因 / 建议 |
|---|---|---|
| B11 | pending | 刷新不存在订阅仍返回 200+`status:error`；改成 404 会改变现有端到端脚本与前端错误处理契约，留作独立小改动 |
| B14 | pending | 配置导出含 `lan_ip` / `has_github_token` 派生字段；导入端已安全忽略，改动收益低 |
| B15 | pending（需确认） | 根目录 0 字节 `nul` 与 `docs/review-round-*.html` 属可删除候选；按收尾纪律需用户确认后再清理，未擅自删除 |
| B16 | pending | `docs/competitive-analysis-and-plan.md` 为历史规划/审计日志，建议后续归档到 `docs/history/` 或标注“历史文档” |
| B17 | pending | UI 排序下拉未暴露 score/speed；README 只承诺 UI 三种排序，API 已支持全部键 |
| B18 | pending | Gist raw URL 使用设置的 `github_user`；token/用户名不一致时 URL 所有者错误，建议从 API 响应推导 |
| B19 | pending | Hysteria2 / TUIC 高级参数（obfs/alpn/拥塞控制）未解析；已在 README「协议支持与已知限制」声明 |
| B20 | pending | SS `plugin` / `plugin-opts` 未解析；已在 README 声明限制 |
| T1–T5 | 部分完成 | 新增 12 个单测 + 修复端到端脚本；仍无 handler 级集成测试、无 CI、无前端自动化（浏览器冒烟为手工一次） |
| E1–E4 | pending | 无 CI / 无 rustfmt/clippy 配置 / 无 LICENSE / 历史文档重复；建议独立工程化任务 |

### 6.3 补充说明

- 本轮**未**执行 `cargo fmt` 全量格式化：项目现有排版包含大量历史空行，全量格式化会产生上千行无关 diff；建议单独一次“纯格式化提交”并加入 CI 门禁。
- 带宽能力的产品决策保持“关闭新采集”：评分中的带宽分量仅对历史实测数据生效。若后续要恢复，建议以独立设置项（默认关闭）接入，而不是重新打开全局带宽探测。
- 静态资源内嵌使用 `include_bytes!`（零新依赖），构建 `subhub-server` / `subhub-app` 时自动更新；开发时磁盘优先，保持“改前端重启即生效”的现有工作流。

---

## 七、第二轮复核增强（2026-10-28 续）

在第一轮修复后对导出命名、Top-N、备份保真、Gist 分发、引擎进度映射、传输协议与前端错误态做二次复查。

### 7.1 新发现问题与处置

| ID | 位置 | 问题 | 严重度 | 处置 |
|---|---|---|---|---|
| C1 | `core/src/export.rs::ensure_unique_names` | 生成 ` #2` 后未把该名称登记到已用集合；`["A","A","A #2"]` 会导出两个 `A #2`，客户端报 duplicate name 拒绝整份配置 | 高 | 改为 `HashSet` 循环分配唯一名并登记生成名；新增单测 `unique_names_avoid_collision_with_existing_suffix` |
| C2 | `server/src/handlers/maintenance.rs` | Top-N 在 `export_filter` 之前按全部节点评分，死节点 / Other 类型占用名额后又被过滤，最终可能不足 N 个 | 中 | 新增 `core::ops::top_n_by_score`（先 `is_usable` 再评分排序）；`merge_export`、`/sub`、`/api/nodes/top` 统一使用；新增单测 |
| C3 | `SubExportItem` / `import_subscriptions` | 备份不含健康时间戳、错误与流量用量；恢复后用量卡片消失、`last_healthy_at` 丢失影响自动清理计时 | 中 | `version: 2`，新增 8 个可选字段并原样恢复；旧备份缺字段时回退原行为；新增 round-trip 单测 |
| C4 | `handlers/speedtest.rs::gist_upload` | Gist 文件名固定 `subscription.yaml`，v2ray/sing-box/surge 内容扩展名错误；raw URL 使用设置里的用户名，与 token 实际 owner 不一致时 404 | 中 | 按格式选择 `.yaml/.json/.conf/.txt`；优先使用响应中的 `owner.login`；新增扩展名单测 |
| C5 | `server/src/engine.rs::EngineProgress` | SSE 用回调计数反查输入列表取 server；批次中不可导出节点不回调时，后续节点 host 错位 | 低 | `EngineProgress` 直接携带 `server`，SSE 改用该字段 |
| C6 | `core/src/parse.rs` / `export.rs` | h2 / HTTP 传输只解析 sing-box，clash `h2-opts` 与 v2ray `httpSettings` 未支持，导出即丢传输层 | 中 | 解析 clash `h2-opts`、sing-box `transport.type=http`（host 数组）；导出 clash / v2ray / sing-box；新增三格式单测 |
| C7 | `core/src/parse.rs::parse_vmess` | 字符串端口 `"0"` 被接受；数字形式 `aid` 丢失、超大值可能截断 | 低 | 统一 `u64` 解析 + `1..=65535` 过滤；`aid` 支持数字并做 u32 校验；新增单测 |
| C8 | `webui/app.js::loadSubs` | 请求失败被当作空列表，显示「还没有订阅」，掩盖服务端错误 | 低 | 区分 error 与空列表，显示「订阅列表加载失败：…」 |
| C9 | `webui/app.js::buildSubParams` | 多条同字段 include 筛选生成重复 `q`/`region`/`type` 键，服务端反序列化 400 | 中 | 每键只保留第一条并提示「同一字段多条筛选无法完整编码」；浏览器验证 |
| C10 | `server/src/db.rs`、`Cargo.toml` | `load_all` 无 ORDER BY，重启后订阅顺序可能变化；`tower`/`tower-http`/`anyhow` 已无引用 | 低 | 增加 `ORDER BY rowid`；移除未使用依赖 |

### 7.2 验证

- `cargo test -p subhub-core -p subhub-server`：**110 个测试全通过**（core 90 + server 20）。
- clippy `-D warnings` 0 警告。
- 隔离 DB 端到端 ALL CHECKS PASSED；额外脚本验证 Top-N 不选死节点、备份健康字段往返。
- 浏览器验证：分享 URL 重复筛选提示、订阅列表错误态、合并/设置页无控制台错误。

### 7.3 仍为 pending（与第一轮一致或新增）

- 刷新不存在订阅仍返回 200 + `status:error`（B11）。
- SS `plugin` / `plugin-opts`、Hysteria2 `obfs`/`alpn`、TUIC 拥塞控制等高级参数未解析（B19/B20，已在 README 声明）。
- 无 CI、无全量 `cargo fmt`、无 LICENSE；根目录 `nul` 与 `docs/review-round-*.html` 为待确认删除候选。
- 本地 / 粘贴类订阅的备份恢复仍靠「新增一条」而非幂等更新；如需完全幂等需引入内容指纹或受控 id 匹配（设计取舍，未在本轮改动）。

---

## 八、第三轮复核增强（2026-10-28 再续）

第三轮聚焦“剩余协议字段、隐私泄露面、分发通道与调度/名称边界”。

### 8.1 新发现问题与处置

| ID | 位置 | 问题 | 严重度 | 处置 |
|---|---|---|---|---|
| D1 | `core/src/parse.rs` / `export.rs` | SS `plugin` / `plugin-opts` 未解析，obfs/v2ray-plugin 节点导出后插件丢失；v2ray 导出会生成看似正常但必然连不上的节点 | 高 | 模型新增 `plugin`/`plugin_opts`；解析 SIP002 与 clash；导出 clash/sing-box/Surge（Surge 仅 obfs）；v2ray 跳过并告警；指纹纳入插件；补三格式测试 |
| D2 | `core/src/parse.rs` / `export.rs` | Hysteria2 `obfs` / `obfs-password` 未解析，sing-box/clash 导出丢混淆 | 中 | 模型新增 `obfs`/`obfs_password`；解析 URI/clash/sing-box，导出 clash/sing-box；指纹纳入；补测试 |
| D3 | `core/src/parse.rs::extract_subscription_usage` | 只读顶层用量字段，嵌套 `info:` 块（常见）被漏掉，用量卡片不显示 | 中 | 同时读取顶层与 `info:`（顶层优先）；新增嵌套/混合测试 |
| D4 | `server/src/lib.rs::fetch_subscription_text` / `client_with_proxy` | reqwest 错误 Display 含完整 URL（订阅 token / 代理凭据），写入 `last_error` 后显示在 UI | 高（隐私） | 使用 `without_url()`；代理错误用 `redact_url()`；端到端断言 `SUPERSECRET` 不出现在错误中 |
| D5 | `server/src/handlers/speedtest.rs::gist_upload` | Gist 请求不走全局拉取代理，GitHub 被墙时上传不可用；WebUI 无公开上传确认 | 中 | 开启代理时复用 `default_fetch_proxy`（60s 超时）；前端增加“公开 Gist”确认；端到端验证代理失败路径 |
| D6 | `server/src/handlers/subscription.rs::refresh_subscription` | 不存在的 id 返回 200 + `status:error`，与删除/测速 404 契约不一致 | 低 | 改为 404；端到端新增断言；前端刷新失败弹窗 |
| D7 | `server/src/handlers/settings.rs` + `run_server` | `auto_refresh_sec` / `node_health_check_sec` 可被设成 1 秒，后台循环高频打网络 | 中 | `clamp_schedule`：0 保持关闭，非零最小 60s；环境变量、meta 读取与 API 写入统一；单测覆盖 |
| D8 | `core/src/export.rs::to_surge` | 节点名含换行/逗号/`=` 会破坏 Surge 配置行，甚至注入额外代理条目 | 中（安全） | `surge_safe()` 清洗名称与 obfs 参数；新增换行注入测试 |

### 8.2 验证

- `cargo test -p subhub-core -p subhub-server`：**119 个测试全通过**（core 98 + server 21）。
- clippy `-D warnings` 0 警告。
- 隔离 DB 端到端 ALL CHECKS PASSED：新增 SS 插件/HY2 obfs 三格式导出、错误脱敏、调度 clamp、Gist 代理失败、刷新 404 断言。
- 浏览器验证：Gist 公开确认、刷新失败弹窗、合并/设置页无控制台错误。

### 8.3 仍为 pending

- TUIC 拥塞控制/`alpn`、Hysteria2 `alpn` 等高级参数未解析（已于 README 声明）。
- 本地 / 粘贴类订阅备份恢复非幂等（每次新增一条），需内容指纹或受控 id 匹配的设计取舍。
- 无 CI、无全量 `cargo fmt`、无 LICENSE；`nul` 与 `docs/review-round-*.html` 仍为待确认删除候选。
- 默认监听 `0.0.0.0` 且 API 无鉴权：这是为局域网 `/sub` 设计的既有行为，若在不可信网络使用建议设置 `SUBHUB_BIND=127.0.0.1` 或加防火墙。

---

## 九、第四轮收尾（2026-10-28 完成全部待处理项）

本轮按用户要求拉取 mihomo / sing-box / Xray 官方协议文档作为参考实现，建立字段索引，并补齐剩余可映射参数与工程门禁。

### 9.1 处置结果

| ID | 项 | 处置 | 验证 |
|---|---|---|---|
| E1 | 通用 ALPN | 模型 `alpn: Vec<String>`；URI/clash/sing-box 解析，clash/v2ray/sing-box 导出 | `tuic_advanced_fields_*`、`hysteria2_up_down_alpn_parsed`、`vless_alpn_exported_*` |
| E2 | TUIC 高级参数 | `congestion_control` / `udp_relay_mode` / `udp_over_stream` / `zero_rtt_handshake`；mihomo 与 sing-box 双命名映射 | `tuic_advanced_fields_parsed_from_uri_clash_and_singbox` / `tuic_advanced_fields_exported` |
| E3 | Hysteria2 上下行 | `up_mbps` / `down_mbps`；mihomo `"N Mbps"` ↔ sing-box 数字 | `hysteria2_up_down_alpn_parsed` / `hysteria2_up_down_and_alpn_exported` |
| E4 | 本地备份非幂等 | 新增 `Subscription.uid`，导出/导入携带；本地按 uid 幂等、旧备份按唯一同名兜底；启动回填旧库 | `subscription_uid_is_stable_and_backfilled`；端到端重复恢复断言 |
| E5 | 协议参考索引缺失 | 新增 `docs/protocol-reference-index.md`，记录来源链接、模型↔客户端字段映射、已覆盖测试与未覆盖字段 | 文档审查 |
| E6 | CI / fmt / LICENSE / 历史残留 | 新增 `.github/workflows/ci.yml`（test/clippy/fmt）；`cargo fmt --all`；MIT LICENSE；review HTML 归档到 `docs/history/`；删除 `nul` | `cargo fmt --check`、clippy、本地测试 |
| E7 | 默认绑定 / 无鉴权 | 保持既有局域网 `/sub` 设计，在 README/审计中明确风险与 `SUBHUB_BIND=127.0.0.1` 建议；不在本轮引入鉴权以免破坏 `/sub` 客户端兼容 | 文档 |

### 9.2 最终验证

- `cargo test -p subhub-core -p subhub-server`：**125 个测试全通过**（core 104 + server 21）。
- `cargo fmt --all -- --check` 通过；clippy `-D warnings` 0 警告。
- 隔离 DB 端到端 ALL CHECKS PASSED（含 uid 幂等、插件/高级参数导出、错误脱敏、Gist 代理、调度 clamp、刷新 404）。
- release 构建 `cargo build --release -p subhub-app` 通过。

### 9.3 仍明确不做的字段（已在索引中登记）

- Hysteria2 `ports` / `hop-interval` 端口跳跃与 Gecko 包大小；TUIC `heartbeat` / `fast-open` / `disable-sni` / `max-open-streams`；SS 外部插件（shadow-tls / kcptun）；VLESS `packet-encoding` / ECH。
- 这些字段在 mihomo 与 sing-box 间的单位/结构差异较大，贸然透传会产生“看似成功、实际错误”的配置；索引中已记录原因与后续入口。
