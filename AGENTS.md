# SubHub — 项目入口（给 AI Agent）

订阅聚合 + 测速 + 多格式导出工具：Rust 核心（解析/算子/测速/导出）+ axum WebUI + Tauri v2 桌面壳。

## 怎么跑
- WebUI 调试：`cargo run -p subhub-server` → http://127.0.0.1:3005
- 桌面端：`cargo run -p subhub-app`（系统托盘常驻）
- 发布：`cargo build --release -p subhub-app` → `target/release/subhub-app.exe`
- 端口 3005（`SUBHUB_PORT` 覆盖）；SQLite `data/subhub.db`（`SUBHUB_DB` 覆盖）；测速引擎 `SUBHUB_ENGINE_BIN`
- **仓库是 public：任何带 token 的订阅地址、面板分享链接、Gist/代理凭据一律不得提交**（含一次性脚本与测试用例——`docs/test_subhub.py` 只用 `127.0.0.1` + 占位密码）。真实订阅列表只留在本地 DB 与本地脚本里；需要保留带凭据的一次性脚本，放 `local/`（已 gitignore），不要放 `docs/`。
- **用户报障原文复制进测试 / 文档前必须先脱敏**：主机名换成 `*.example.com`、凭据换成全零 uuid 或占位串。AnyTLS 夹具就踩过这个坑——直接把用户在用节点的密码和真实域名写进了断言，事后要重写历史才能抹掉。

## 不踩坑的硬规则（改前必读）
- **用户实际跑的是 `subhub-app.exe`（Tauri 内嵌 server），不是 `subhub-server.exe`。** 任何改动要生效，必须 `cargo build --release -p subhub-app` + 重启 `subhub-app.exe`；只构建/重启 server 对用户**完全无效**。
- **构建前先 `Stop-Process subhub-app`**，否则 exe 被占用、`failed to remove file ... subhub-app.exe`。
- **DB 路径相对进程 CWD**：从 `target\release` 目录启动才读 `target/release/data/subhub.db`（真数据）；从项目根启动会读项目根的空库、误建空文件、节点全丢。重启前务必 `cd target/release` 或用 `-WorkingDirectory target/release` 启动。
- WebUI 静态资源：磁盘优先（源码 `webui/`，可用 `SUBHUB_WEBUI_DIR` 覆盖）→ exe 内嵌副本兜底。改前端重启即生效；发行版更新内嵌副本需重新构建 app。
- 新增全局设置字段需在 7 处同步（见 `.codebuddy/memory/MEMORY.md` 的「新增全局设置字段同步清单」），漏改会 E0063。

## 技术栈与目录
- `core/`：纯库（model/parse/ops/export/speedtest），无 IO。
- `server/`：axum HTTP + WebUI 托管 + 路由（`lib.rs`）+ `handlers/*`。
- `app/`：Tauri v2 原生窗口壳。
- `webui/`：原生 JS（`app.js` + `index.html` + `style.css`）。
- 数据一致性：任何写操作后 `persist_all()`；`Db` 只持文件路径不持 `Connection`（非 Send）。改了 `sub.proxies` 之后先 `health.recompute()` 再落库，否则过期的派生计数会被写进 SQLite。
- 跑端到端/基准要**整个目录删掉**再起服务：`subhub.db-wal` / `-shm` 会把上一轮数据重放回来，只删 `.db` 等于没清库（曾把它误判成行为回归）。
- 网络类单测不要「按节点数 accept N 次再 join」：测速有端点级缓存，同 `host:port` 的多个节点只拨号一次，固定次数的 accept 循环会在 Linux 上永久挂住 runner（CI 实测挂死 1 小时）。accept 用无限循环且不 join。

## 深层经验 / 踩坑
- 所有非显然的架构决策、测速根因、踩坑记录在 `.codebuddy/memory/MEMORY.md`（跨会话长期记忆，优先读它再动手）。注意该目录被 `.gitignore` 排除，**是本地 Agent 记忆、不随仓库分发**；克隆到另一台机器时它不存在，此时以本文件 + `CHANGELOG.md` + `docs/AUDIT-REPORT-*.md` 为准。
- 改 SS 相关逻辑时务必同时看 `core/src/model.rs::is_exportable` 与 `core/src/export.rs::to_clash_meta` 两处。
- 测速引擎单节点配置必须用 `mode: rule` + `rules: - MATCH,PROXY`，严禁 `mode: global`。
