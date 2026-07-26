# BOOTSTRAP — 首次运行与身份/安全模型

本文件说明 subhub 的首次运行行为、身份初始化方式，以及**安全模型与暴露面**。
请在部署或把服务暴露到网络前先读完。

## 1. 身份初始化：当前为「无认证」设计

subhub 是**单租户本地聚合工具**，启动流程（`server/src/main.rs` → `run_server`）**不创建任何管理员账户、不生成密码、不颁发 token**。
`AppState` 中没有任何身份 / 用户 / 凭证 / 会话字段，`meta` 表也只持久化配置项
（`use_proxy` / `auto_refresh_sec` / `default_fetch_proxy` / `top_n` / `engine_bin` / `remove_after_fails`），**没有 admin / credential / initialized 这类键**。

因此：

- 所有 `/api/*` 端点（增删改订阅、测速、导出、设置）与直连的 `/sub` 导出端点都**无需鉴权**即可调用。
- 当前 **bind 地址硬编码为 `127.0.0.1`**（`server/src/lib.rs` 的 `SocketAddr::from(([127, 0, 0, 1], port))`），即**仅本机可访问**。这一绑定把「无认证」的风险限制在本地。

## 2. 风险说明

虽然默认只监听 loopback，但「无认证」仍意味着：

- 本机任意进程（含浏览器中打开的恶意网页，通过 `fetch('http://127.0.0.1:3005/...')`）都能**读取你的全部节点、触发代理探测、修改或删除订阅**。
- 若有人把 bind 改成 `0.0.0.0`（当前需改代码，无对应环境变量）且未加任何防护，服务会立即变成**公网可写的面板**。

> 结论：把 subhub 直接暴露到 `0.0.0.0` / 公网是**不安全**的，必须先加认证。

## 3. 推荐的暴露方式（如需远程访问）

不要直接把服务绑定到 `0.0.0.0`。若需从其他设备访问，请放在一个**带认证的反向代理**之后，例如：

- nginx / caddy 前置 Basic Auth 或 mTLS；
- 或 Cloudflare Tunnel 等带身份校验的隧道；
- 或在本仓库增加 token 中间件（见第 5 节路线图）。

## 4. 首次运行的环境变量

| 变量 | 作用 | 默认值 |
| --- | --- | --- |
| `SUBHUB_PORT` | HTTP 监听端口 | `3005` |
| `SUBHUB_DB` | SQLite 数据库文件路径 | 工作区下的 `subhub.db` |
| `SUBHUB_ENGINE_BIN` | 测速引擎可执行文件（mihomo / sing-box）路径；不配置则只做 TCP 延迟探测，不做真实带宽/出口地区/解锁检测 | 未设置 |
| `default_fetch_proxy` | 服务端默认抓取代理（单一真相源，替代旧版浏览器「记住」复选框） | 未设置 |

> 订阅抓取代理：引擎只用于测速；订阅本身的抓取走服务端配置的 `default_fetch_proxy`，避免在浏览器侧保存代理。

## 5. 安全路线图（尚未实现，待立项）

- [ ] 默认拒绝的 `Authorization: Bearer <token>` 中间件，除 `/api/health` 与静态资源外全部套用。
- [ ] 首次运行若 `meta` 无 `admin_token`，由 `run_server` 生成随机 token 打印到 stderr 并写入 `meta`。
- [ ] 显式拒绝 `0.0.0.0` 绑定，除非已配置 token 且经由 `SUBHUB_BIND=0.0.0.0` 显式开启。
- [ ] 为 `/sub` 直连导出端点增加可选的 `?token=` 校验。
- [ ] 订阅 / 节点 id 由顺序 `sub_N` 改为服务端随机 id（降低被枚举风险）。
