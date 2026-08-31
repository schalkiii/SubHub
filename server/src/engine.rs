//! Optional protocol-level checks via an external connection engine
//! (mihomo / sing-box). Best-effort: every failure yields `None` for that node,
//! so it never breaks the primary TCP-latency path. Enabled when the engine
//! binary is configured in the UI settings (fallback: `SUBHUB_ENGINE_BIN`).
use crate::Proxy;

use subhub_core::to_clash_meta;

use std::collections::BTreeMap;
use std::collections::HashSet;

use std::io::Write;
use std::sync::Arc;

use std::net::{TcpListener, TcpStream};

use std::process::Child;

use std::process::Command;

use std::time::{Duration, Instant};

/// Hard cap on simultaneously running engine processes. Each check spawns a
/// full mihomo/sing-box OS process per node; anything beyond a handful stalls
/// the whole machine, so the per-request concurrency is clamped to this.
const MAX_ENGINE_PROCESSES: usize = 4;

/// 单实例模式下每个 engine 进程承载的节点数上限。
///
/// 一个 mihomo 进程能可靠处理的 delay 请求数是有界的：把整批（数千）节点
/// 塞进同一个 config 后，实测 4897 个节点测到 4727 就彻底卡死，进程 3 小时
/// 不退出。分批让每批的 engine 进程在资源耗尽前就完成任务并被回收。
/// 取值权衡：太大 → mihomo 内部排队退化；太小 → 进程创建次数上升（每次
/// 含最长 15s 的冷启动等待）。100 在实测区间内兼顾两者。
const SHARED_BATCH_SIZE: usize = 100;

/// 单批内向同一个 mihomo 进程并发投递的 delay 请求数。
///
/// 与 `MAX_ENGINE_PROCESSES` 无关（后者约束 OS 进程数，此处是同一进程内的
/// HTTP 请求）。mihomo 用 goroutine 并发处理 delay，64 并发既填满其处理
/// 能力、又不会像「数千排队」那样耗尽内部资源导致卡死。
///
/// 实测卡死与并发无关：原先并发仅 4 时照样卡死，根因是**单进程累积测了
/// 数千个节点**后的资源泄漏。分批已消除累积，故批内并发可以放心拉高 ——
/// 实测 250 节点（含 234 个死节点，各需等满一次超时）并发 32 耗时 273s，
/// 提到 64 后死节点的超时等待被重叠，总耗时显著下降。
const SHARED_BATCH_CONCURRENCY: usize = 64;

/// RAII guard that guarantees the spawned engine process and its temp config
/// directory are cleaned up no matter how `with_engine` exits — including the
/// early `?` returns that happen *after* `cmd.spawn()` but *before* the old
/// explicit `child.kill()` calls (e.g. building the reqwest client for an
/// unsupported proxy URL). Without this, those paths leaked both the OS
/// process and the `subhub-engine-<port>` directory.
struct EngineGuard {
    child: Child,
    dir: std::path::PathBuf,
}

impl Drop for EngineGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Monotonic counter to make each engine temp dir unique even when two
/// `free_port()` calls race to the same OS-assigned port (the port is
/// released immediately after probing, so a later call can reuse it).
/// Including the pid + this counter guarantees a collision-free directory
/// name, so concurrent engine spawns never clobber each other's config.
static ENGINE_DIR_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Spin up the engine for one proxy, wait for its SOCKS5/mixed listener, hand
/// a proxied `reqwest::Client` to `f`, then tear the engine down. Returns
/// `None` if the engine can't start / the port never opens / the node isn't
/// exportable (so the engine couldn't run it anyway) —never throws.
async fn with_engine<R, F, Fut>(p: &Proxy, bin: &str, timeout_ms: u64, f: F) -> Option<R>
where
    F: FnOnce(reqwest::Client) -> Fut,

    Fut: std::future::Future<Output = R>,
{
    // Skip nodes the engine can't represent (e.g. `Other` type, or a node

    // missing required fields). Spawning an engine for them is wasted work —    // they can never be measured and the TCP path already handles liveness.

    if !p.is_exportable() {
        return None;
    }

    let port = free_port()?;

    let cfg = build_engine_config(p, port);

    let seq = ENGINE_DIR_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "subhub-engine-{}-{}-{}",
        std::process::id(),
        port,
        seq
    ));

    let _ = std::fs::create_dir_all(&dir);

    let cfg_path = dir.join("config.yaml");

    {
        let mut file = std::fs::File::create(&cfg_path).ok()?;

        let _ = file.write_all(cfg.as_bytes());
    }

    let mut cmd = Command::new(bin);
    // mihomo / clash-family engines take `-f` for the config file; sing-box takes
    // `-c`. The clash-verge-rev `verge-mihomo.exe` build in particular rejects
    // `-c` outright (flag not defined) and exits immediately, which previously
    // made every engine probe fail silently and fall back to TCP-only (fake)
    // measurements — nodes then showed a green "ping + bandwidth" that was never
    // a real proxy check (e.g. SubHub green while clash reports the node dead).
    let config_flag = if bin.to_ascii_lowercase().contains("mihomo")
        || bin.to_ascii_lowercase().contains("clash")
    {
        "-f"
    } else {
        "-c"
    };
    cmd.arg(config_flag)
        .arg(&cfg_path)
        // mihomo 的启动/转发日志走 **stdout**（实测如此，不是 stderr），是诊断
        // 「HTTP 探测为何失败」的唯一线索：此前把 stdout/stderr 全部丢弃，节点探测
        // 失败（502 / 连接失败 / DNS 解析失败）时完全无日志可查，只能看到「未实测」。
        // 两者都继承父进程 stderr，便于集中排查。
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    // Engines are console programs; without this flag every spawn pops up a
    // visible console window on Windows (hundreds during a speedtest).
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let child = cmd.spawn().ok()?;
    // Owned guard: drops the process + temp dir on every exit path (including
    // the `?` returns below that previously leaked both).
    let mut _guard = EngineGuard {
        child,
        dir: dir.clone(),
    };

    // 冷启动（首次测速，引擎二进制尚在 OS 缓存外、且本批并发拉起大量引擎进程）
    // 时 mihomo 起进程 + 开 mixed-port 常超过 4s；超时过短会让这些节点被误判
    // engine 不可用 → 回退 TCP → 错误标「可用」。放宽到 15s：端口一旦就绪立即返回，
    // 只兜底真正起不来的少数节点，不会拖慢已热身的引擎。
    if !engine_ready(&mut _guard.child, port, Duration::from_millis(15000)).await {
        return None;
    }

    // 测速走 mihomo 的 HTTP 代理通道（mixed-port 同时提供 HTTP/SOCKS5）。
    // 实测 `verge-mihomo` 的 SOCKS5 入站与 reqwest 的 socks5 客户端握手不兼容，
    // 所有经 SOCKS5 的 HTTPS 请求都 `error sending request`；改用 HTTP 代理的
    // CONNECT 隧道则正常转发，带宽/存活探测才能拿到真实结果。
    let proxy = reqwest::Proxy::all(format!("http://127.0.0.1:{port}")).ok()?;

    let client = reqwest::Client::builder()
        .proxy(proxy)
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .ok()?;

    let result = f(client).await;

    Some(result)
}

/// 单个节点在一次 engine 进程内完成的探测结果集合。
///
/// 之所以把探测项聚合到一个结构体，是为了做到「一个节点 = 一个 engine 进程」：
/// 重构前每个探测项各自拉起一个进程（HTTP 延迟 / 带宽 / 出口国家 / 解锁共 4 项，
/// 仅测速的「延迟 + 带宽」两项就已经是 2N 个进程）。复用同一进程既省掉重复的
/// 冷启动（单次最长 15s 等待），也让 `MAX_ENGINE_PROCESSES` 真正成为全局上限
/// （原先两个 pass 并行会把并发翻倍到 8，与注释声明的硬上限 4 不符）。
///
/// `http_latency` / `bandwidth` 沿用三层 Option 语义（调用方 `persist_results`
/// 依赖它区分「引擎跑了但失败」与「引擎根本没跑」，否则可用/不可用会反复翻转）：
///   - `Some(Some(v))` 引擎已运行且探测成功
///   - `Some(None)`   引擎已运行但探测失败（节点死 / 目标不可达）
///   - `None`         引擎未运行（无二进制 / 节点不可导出被跳过）
#[derive(Debug, Default)]
pub struct EngineProbe {
    pub http_latency: Option<Option<u64>>,
    pub bandwidth: Option<Option<f64>>,
    /// 出口国家（2 字母码）；`None` = 引擎未运行或全部 geo channel 失败。
    pub country: Option<String>,
    pub unlock: subhub_core::model::ProxyUnlock,
}

/// 引擎探测完成时回调传回的结构（供 SSE 实时更新 UI）。携带 fingerprint
/// 让前端能定位到表格行并就地 patch，而不必等整批测完后重拉。
#[derive(Debug, Clone)]
pub struct EngineProgress {
    pub fingerprint: String,
    pub name: String,
    /// `Some(Some(ms))` = HTTP 探测成功；`Some(None)` = 探测失败；`None` = 未探测。
    pub http_latency: Option<Option<u64>>,
}

/// 本次探测要执行哪些项目。按需开启，避免为用不到的探测项付出请求与耗时
/// （例如仅做出口国家检测时不该去下载测速分片）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ProbePlan {
    pub http_latency: bool,
    pub bandwidth: bool,
    pub country: bool,
    pub unlock: bool,
}

/// 一次探测任务的全部参数。集中成结构体而非长位置参数列表，调用点可读性更好，
/// 也避免新增探测项时不断加长签名。
// 不派生 Debug：`Arc<dyn Fn(..)>` 里的回调闭包不实现 Debug。
#[derive(Clone)]
pub struct ProbeTask<'a> {
    /// 待探测节点
    pub proxies: &'a [Proxy],
    /// 引擎二进制路径
    pub bin: &'a str,
    /// 存活 / 出口地区 / 解锁探测的超时（ms）
    pub timeout_ms: u64,
    /// 带宽下载专用超时（ms）：存活探测用的 `timeout_ms` 远不够拉完一个测速分片，
    /// 调用方需传更大的值（见 `run_engine_passes`）。
    pub bandwidth_timeout_ms: u64,
    /// 并发上限，会被 clamp 到 `MAX_ENGINE_PROCESSES`
    pub concurrency: usize,
    pub plan: ProbePlan,
    /// 存活探测完成回调（SSE 进度用），携带 fingerprint + http_latency 供前端就地 patch。
    pub on_http: Option<Arc<dyn Fn(EngineProgress) + Send + Sync>>,
    /// 带宽探测完成回调（SSE 进度用）
    pub on_bw: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

/// 对每个节点**只拉起一次** engine 进程，在同一进程内串行完成 `plan` 要求的探测项，
/// 返回结果集合（与 `proxies` 同序）。
///
/// engine 的 client 超时取 `timeout_ms` 与 `bandwidth_timeout_ms` 的较大值 —— 它只是
/// 异常时的上限，不影响本就快速返回的存活探测。
///
/// `on_http` / `on_bw` 分别在**对应探测项完成时**（而非节点全部探测完成时）回调，
/// 因此 SSE 进度事件的时序与分阶段语义和重构前保持一致。
/// 只做「HTTP 延迟」探测时走单实例批量模式：拉起 **一个** engine 进程测完
/// 整批节点，而不是每节点一个进程。见 `probe_all_shared` 的动机说明。
fn is_http_only(plan: ProbePlan) -> bool {
    plan.http_latency && !plan.bandwidth && !plan.country && !plan.unlock
}

/// 单实例批量测速：只拉起 **一个** engine 进程，把整批节点塞进同一个 config，
/// 再用 Clash External Controller 的 `/proxies/{name}/delay` 逐个测 HTTP 延迟。
///
/// 相比原先「每节点一个进程」（`probe_one` → `with_engine`），进程创建次数从
/// N 降到 1。全量测速（数千节点）时，原方案会产生数千次 mihomo 进程
/// 创建/销毁 + 临时目录 IO + 每次最长 15s 的冷启动等待，把系统网络栈打爆 ——
/// 表现为同机运行的 Clash Verge 在测速期间网络异常（用户实测现象）。
/// 单实例还让测速整体快一个数量级，且不再需要 4 个进程并发抢占资源。
///
/// 仅用于 HTTP 延迟探测；带宽 / 出口地区 / 解锁需要经指定节点发**自定义请求**，
/// 单实例的 select 组无法指定出口，仍走 `probe_all` 的每节点一进程路径。
pub async fn probe_all_shared(task: ProbeTask<'_>) -> Vec<EngineProbe> {
    let ProbeTask {
        proxies,
        bin,
        timeout_ms,
        bandwidth_timeout_ms,
        concurrency,
        plan,
        ..
    } = task;

    let mut out: Vec<EngineProbe> = (0..proxies.len()).map(|_| EngineProbe::default()).collect();

    if !std::path::Path::new(bin).exists() || proxies.is_empty() {
        return out;
    }

    // 分批：整批节点（可达数千）不能塞进同一个 mihomo config。实测 4897 个
    // 节点单实例测到 ~96%（4727）后彻底卡死，engine 进程 3 小时不退出 ——
    // mihomo 内部连接池 / goroutine 被数千个排队中的 delay 请求耗尽，后续
    // 请求被接收但不再处理，SSE 进度条就此冻结。
    // 分批后每批独立进程、测完即被 EngineGuard drop，任一时点仍只有 1 个
    // engine 进程（不打爆系统），进程创建次数从 N 降到 ceil(N/BATCH)。
    let mut start = 0usize;
    while start < proxies.len() {
        let end = (start + SHARED_BATCH_SIZE).min(proxies.len());
        let batch = &proxies[start..end];

        // 回调直接透传：EngineProgress 自带 fingerprint，外层无需改写索引。
        let batch_out = probe_shared_batch(ProbeTask {
            proxies: batch,
            bin,
            timeout_ms,
            bandwidth_timeout_ms,
            concurrency,
            plan,
            on_http: task.on_http.clone(),
            on_bw: None,
        })
        .await;

        for (j, r) in batch_out.into_iter().enumerate() {
            out[start + j] = r;
        }
        start = end;
    }
    out
}

/// 单批（≤ `SHARED_BATCH_SIZE` 个节点）用一个 engine 进程测完 HTTP 延迟。
/// 由 `probe_all_shared` 分批调用；`out` 顺序与传入的 `proxies` 一致。
async fn probe_shared_batch(task: ProbeTask<'_>) -> Vec<EngineProbe> {
    let ProbeTask {
        proxies,
        bin,
        timeout_ms,
        bandwidth_timeout_ms: _,
        concurrency: _,
        plan: _,
        on_http,
        on_bw: _,
    } = task;

    let mut out: Vec<EngineProbe> = (0..proxies.len()).map(|_| EngineProbe::default()).collect();

    if !std::path::Path::new(bin).exists() || proxies.is_empty() {
        return out;
    }

    let port = match free_port() {
        Some(p) => p,
        None => return out,
    };
    let ctrl_port = match free_port() {
        Some(p) => p,
        None => return out,
    };

    let (cfg, names) = build_multi_engine_config(proxies, port, ctrl_port);
    // 整批节点引擎都无法表示（不可导出）时不必启动进程。
    if names.iter().all(|n| n.is_none()) {
        return out;
    }

    let seq = ENGINE_DIR_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "subhub-engine-shared-{}-{}-{}",
        std::process::id(),
        port,
        seq
    ));
    let _ = std::fs::create_dir_all(&dir);
    let cfg_path = dir.join("config.yaml");
    {
        let mut file = match std::fs::File::create(&cfg_path) {
            Ok(f) => f,
            Err(_) => return out,
        };
        let _ = file.write_all(cfg.as_bytes());
    }

    let mut cmd = Command::new(bin);
    let config_flag = if bin.to_ascii_lowercase().contains("mihomo")
        || bin.to_ascii_lowercase().contains("clash")
    {
        "-f"
    } else {
        "-c"
    };
    cmd.arg(config_flag)
        .arg(&cfg_path)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let t_spawn = Instant::now();
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[engine] 启动 engine 失败: {e}");
            let _ = std::fs::remove_dir_all(&dir);
            return out;
        }
    };
    // RAII：无论从哪个分支返回，进程与临时目录都会被清理。
    let mut guard = EngineGuard {
        child,
        dir: dir.clone(),
    };

    if !engine_ready(&mut guard.child, port, Duration::from_millis(15000)).await {
        eprintln!("[engine] mixed-port {port} 未就绪");
        return out;
    }
    if !engine_ready(&mut guard.child, ctrl_port, Duration::from_millis(15000)).await {
        eprintln!("[engine] external-controller {ctrl_port} 未就绪");
        return out;
    }
    eprintln!(
        "[engine][perf] 批 {} 节点: mihomo 启动+就绪 {:.1}s",
        proxies.len(),
        t_spawn.elapsed().as_secs_f64()
    );

    // delay 是单个 HTTP 往返，超时沿用存活探测下限；它只是异常时的上限。
    // `.no_proxy()` 必须：这个 client 只用于调本机 mihomo 的 External
    // Controller API（127.0.0.1:{ctrl_port}），绝不能走系统代理。reqwest
    // 默认读 HTTP_PROXY / HTTPS_PROXY 环境变量，若用户手动设了（或被其他
    // 工具注入），localhost API 调用会被劫持到系统代理 → 请求发到 Clash
    // Verge:7890 → 形成无意义的代理环路，测速全超时。
    let probe_timeout = timeout_ms.max(HTTP_PROBE_MIN_TIMEOUT_MS);
    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(probe_timeout + 2000))
        .build()
    {
        Ok(c) => c,
        Err(_) => return out,
    };

    // 注意：曾在此处加过「预热」（先探测批内第一个节点并丢弃结果），实测
    // **无效且拖慢** —— 100 节点对照：并发 64 有重试时，无预热 48 通过/26s，
    // 有预热 49 通过/35s（并发 8 时甚至从 43 掉到 41）。冷启动成本已被
    // `delay_via_api` 自身的重试覆盖，预热只是重复付一次代价。
    // 并发问询 delay API：mihomo 内部对同一进程的测速请求会排队，但并发投递
    // 能让慢节点不阻塞队首之后的快节点。
    // 这里**不用** MAX_ENGINE_PROCESSES：那个常量限制的是 OS 进程数，而此处
    // 并发的是同一进程内的 HTTP 请求（mihomo 用 goroutine 处理，不受 4 影响）。
    // 沿用 4 会让 100 节点/批 × 数十批的全量测速慢到不可接受。
    let sem = Arc::new(tokio::sync::Semaphore::new(SHARED_BATCH_CONCURRENCY));
    let mut tasks = Vec::new();
    for (i, name_opt) in names.iter().enumerate() {
        let Some(name) = name_opt.clone() else {
            continue;
        };
        let permit = sem.clone().acquire_owned().await.unwrap();
        let client = client.clone();
        let target = subhub_core::resources::ALIVE_TARGET.to_string();
        let fp = proxies[i].fingerprint();
        let disp_name = proxies[i].name.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            let res = delay_via_api(&client, ctrl_port, &name, &target, probe_timeout).await;
            (i, fp, disp_name, res)
        }));
    }

    let t_probe = Instant::now();
    let mut ok_cnt = 0usize;
    for t in tasks {
        if let Ok((i, fp, disp_name, res)) = t.await {
            if res.is_some() {
                ok_cnt += 1;
            }
            out[i].http_latency = Some(res);
            if let Some(cb) = &on_http {
                cb(EngineProgress {
                    fingerprint: fp,
                    name: disp_name,
                    http_latency: out[i].http_latency,
                });
            }
        }
    }
    eprintln!(
        "[engine][perf] 批 {} 节点: delay 探测 {:.1}s，通过 {}/{}",
        proxies.len(),
        t_probe.elapsed().as_secs_f64(),
        ok_cnt,
        proxies.len()
    );
    out
}

/// 用 Clash External Controller 的 `/proxies/{name}/delay` 测该节点到 `target`
/// 的 HTTP 往返延迟（ms）。`None` = 不通。
///
/// mihomo 在节点不可用 / 超时时返回 `delay: 0`，必须视为失败，否则会把死节点
/// 记成「0ms 可用」。这与 `latency_via` 判定标准一致：拿到真实往返才算通。
async fn delay_via_api(
    client: &reqwest::Client,
    ctrl_port: u16,
    node_name: &str,
    target: &str,
    timeout_ms: u64,
) -> Option<u64> {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    let enc_name = utf8_percent_encode(node_name, NON_ALPHANUMERIC).to_string();
    let enc_url = utf8_percent_encode(target, NON_ALPHANUMERIC).to_string();
    let url = format!(
        "http://127.0.0.1:{}/proxies/{}/delay?url={}&timeout={}",
        ctrl_port, enc_name, enc_url, timeout_ms
    );
    // 区分两类失败，**只有前者才重试**（这是曾经改错又改回来的关键点）：
    //
    // 1. mihomo 给出**明确结论**：HTTP 200 + 响应体含 `delay` 字段。
    //    - `delay > 0` → 通，返回延迟
    //    - `delay == 0` → 节点确凿不通（不可达/被拒），**不重试**
    // 2. mihomo **没能完成探测**：HTTP 503 / 504，响应体是
    //    `{"message": "..."}`（如 `Timeout`）而**没有** `delay` 字段。
    //    这是瞬时故障（engine 冷启动未就绪、首个请求撞上初始化窗口、偶发
    //    抖动），**必须重试**——实测同一节点首次 504、第二次即返回
    //    `delay: 524`。
    //
    // 曾用 `j.get("delay").unwrap_or(0)` 把两类混为一谈：504 的响应体没有
    // delay 字段，被 `unwrap_or(0)` 当成「节点不通」直接返回 None 且**不重试**，
    // 导致冷启动/抖动节点被大规模误判为死节点（严重假阴性）。判据必须是
    // 「delay 字段**是否存在**」，而不是「取值是否为 0」。
    for attempt in 0..3 {
        match client.get(&url).send().await {
            Ok(r) => {
                let status = r.status();
                match r.json::<serde_json::Value>().await {
                    Ok(j) => {
                        // 只有 mihomo 真正给出了 delay 结论才定论。
                        if let Some(d) = j.get("delay").and_then(|v| v.as_u64()) {
                            return if d > 0 { Some(d) } else { None };
                        }
                        if attempt == 2 {
                            eprintln!("[engine] delay 探测失败 ({node_name}): HTTP {status} {j}");
                        }
                    }
                    Err(e) => eprintln!("[engine] delay 响应解析失败 ({node_name}): {e}"),
                }
            }
            Err(e) => eprintln!("[engine] delay 请求失败 ({node_name}): {e}"),
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    None
}

/// 构建「单实例多节点」config：整批节点塞进同一个 engine 进程，并开启
/// External Controller 供逐个调 `/proxies/{name}/delay` 测延迟。
///
/// 返回 `(config, names)`，`names[i]` 是 `proxies[i]` 在 config 中的唯一名字；
/// 不可导出（引擎无法表示）的节点为 `None`，引擎测不了、保持 TCP 结果。
fn build_multi_engine_config(
    proxies: &[Proxy],
    port: u16,
    ctrl_port: u16,
) -> (String, Vec<Option<String>>) {
    let mut cfg = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(cfg, "mixed-port: {port}");
    let _ = writeln!(cfg, "external-controller: 127.0.0.1:{ctrl_port}");
    // rule 模式 + MATCH,PROXY 强制所有流量经被测节点转发（global 会直连绕过代理）。
    let _ = writeln!(cfg, "mode: rule");
    let _ = writeln!(cfg, "log-level: warning");
    let _ = writeln!(cfg, "dns:");
    let _ = writeln!(cfg, "  enable: true");
    let _ = writeln!(cfg, "  ipv6: false");
    let _ = writeln!(cfg, "  enhanced-mode: normal");
    let _ = writeln!(cfg, "  nameserver:");
    let _ = writeln!(cfg, "    - 223.5.5.5");
    let _ = writeln!(cfg, "    - 119.29.29.29");

    let mut names: Vec<Option<String>> = Vec::with_capacity(proxies.len());
    let mut used: HashSet<String> = HashSet::new();
    let mut bodies: Vec<String> = Vec::new();
    let mut group: Vec<String> = Vec::new();

    for p in proxies {
        if !p.is_exportable() {
            names.push(None);
            continue;
        }
        // mihomo 的 /proxies/{name} 按**名字**寻址，重名节点会互相覆盖，
        // 导致测出来的延迟张冠李戴。给重名节点加 "#N" 后缀保证唯一。
        let mut name = p.name.clone();
        if used.contains(&name) {
            let mut i = 2;
            loop {
                let cand = format!("{} #{}", p.name, i);
                if !used.contains(&cand) {
                    name = cand;
                    break;
                }
                i += 1;
            }
        }
        used.insert(name.clone());

        let node = to_clash_meta(std::slice::from_ref(p));
        let body = node.trim_start_matches("proxies:").trim_start().to_string();
        bodies.push(replace_node_name(&body, &name));
        group.push(escape_yaml_scalar(&name));
        names.push(Some(name));
    }

    let _ = writeln!(cfg, "proxies:");
    for b in &bodies {
        let _ = writeln!(cfg, "{b}");
    }
    let _ = writeln!(cfg, "proxy-groups:");
    let _ = writeln!(cfg, "  - name: PROXY");
    let _ = writeln!(cfg, "    type: select");
    let _ = writeln!(cfg, "    proxies: [{}]", group.join(", "));
    let _ = writeln!(cfg, "rules:");
    let _ = writeln!(cfg, "  - MATCH,PROXY");

    (cfg, names)
}

/// 把 `to_clash_meta` 生成的节点 YAML 首行 `name:` 字段换成唯一名。
/// 重名节点必须改名，否则 mihomo 的 /proxies/{name} 会互相覆盖。
fn replace_node_name(body: &str, new_name: &str) -> String {
    let escaped = escape_yaml_scalar(new_name);
    match body.find("name:") {
        Some(pos) => {
            let line_end = body[pos..]
                .find('\n')
                .map(|i| pos + i)
                .unwrap_or(body.len());
            format!("{}name: {}{}", &body[..pos], escaped, &body[line_end..])
        }
        None => body.to_string(),
    }
}

pub async fn probe_all(task: ProbeTask<'_>) -> Vec<EngineProbe> {
    // 测速主路径（只测 HTTP 延迟）走单实例：一个 engine 进程测完整批节点。
    // 带宽 / 出口地区 / 解锁需要经指定节点发自定义请求，仍用每节点一进程。
    if is_http_only(task.plan) {
        return probe_all_shared(task).await;
    }
    // 先解构：tokio::spawn 要求 future 为 'static，不能在任务内借用 task 的引用字段。
    let ProbeTask {
        proxies,
        bin,
        timeout_ms,
        bandwidth_timeout_ms,
        concurrency,
        plan,
        on_http,
        on_bw,
    } = task;

    if !std::path::Path::new(bin).exists() {
        return (0..proxies.len()).map(|_| EngineProbe::default()).collect();
    }

    let sem = Arc::new(tokio::sync::Semaphore::new(
        concurrency.clamp(1, MAX_ENGINE_PROCESSES),
    ));
    let mut tasks = Vec::new();
    for p in proxies {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let p = p.clone();
        let bin = bin.to_string();
        let on_http = on_http.clone();
        let on_bw = on_bw.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            probe_one(
                p,
                bin,
                timeout_ms,
                bandwidth_timeout_ms,
                plan,
                on_http,
                on_bw,
            )
            .await
        }));
    }

    let mut out = Vec::with_capacity(proxies.len());

    for t in tasks {
        out.push(t.await.unwrap_or_default());
    }

    out
}

/// 经代理完成一次「真实 HTTP 探测」需要：mihomo 建隧道 + 节点侧 TLS/ws 握手 +
/// 请求 gstatic 并返回。慢节点上这一串常需数秒，而 UI 传入的 `timeout_ms`
/// 默认只有 4s —— 直接拿它当上限，会让大量其实可用的节点在握手阶段就被判超时，
/// 统一回落成 502 /「未实测」（这正是此前 204 探测全军覆没、节点清一色
/// 「可用·未实测」的主因）。这里给存活探测一个 8s 下限；它只是异常时的上限，
/// 不会影响本就快速返回的探测，也不会拖快节点。
const HTTP_PROBE_MIN_TIMEOUT_MS: u64 = 8_000;

/// 拉起一次 engine，在同一进程/连接内完成 `plan` 要求的全部探测。
async fn probe_one(
    p: Proxy,
    bin: String,
    timeout_ms: u64,
    bandwidth_timeout_ms: u64,
    plan: ProbePlan,
    on_http: Option<Arc<dyn Fn(EngineProgress) + Send + Sync>>,
    on_bw: Option<Arc<dyn Fn(String) + Send + Sync>>,
) -> EngineProbe {
    let name = p.name.clone();
    let fp = p.fingerprint();
    // client 超时取本节点所需探测项的最大值：带宽下载需要长超时，而它只是异常时
    // 的上限，不会拖慢本就快速返回的存活 / geo / 解锁探测。
    let client_timeout_ms = if plan.bandwidth {
        timeout_ms.max(bandwidth_timeout_ms)
    } else {
        timeout_ms.max(HTTP_PROBE_MIN_TIMEOUT_MS)
    };
    with_engine(&p, &bin, client_timeout_ms, |c| async move {
        let mut out = EngineProbe::default();
        if plan.http_latency {
            out.http_latency = Some(latency_via(&c).await);
            if let Some(cb) = &on_http {
                cb(EngineProgress {
                    fingerprint: fp.clone(),
                    name: name.clone(),
                    http_latency: out.http_latency,
                });
            }
        }
        if plan.bandwidth {
            out.bandwidth = Some(bandwidth_via(&c, &name).await);
            if let Some(cb) = &on_bw {
                cb(name.clone());
            }
        }
        if plan.country {
            out.country = country_via(&c).await;
        }
        if plan.unlock {
            out.unlock = subhub_core::model::ProxyUnlock {
                services: unlock_via(&c).await,
            };
        }
        out
    })
    .await
    .unwrap_or_default()
}

/// 探测 HTTP 存活延迟（ms）。单次探测对刚拉起的引擎/冷连接偶发瞬时超时，会造成
/// 「一轮可用一轮不可用」的翻转，故同一引擎实例内重试一次（复用同一 client，
/// 不再拉进程）以消除瞬时假阴性。
async fn latency_via(client: &reqwest::Client) -> Option<u64> {
    // mihomo 端口就绪（engine_ready）并不等于内部已完成初始化：刚拉起时发出的
    // 首个请求常撞上未就绪窗口，表现为 502 / 连接被强制关闭。手动复现时同一个
    // 节点「引擎内立刻探测失败、等几秒后探测成功（204）」，差异正出自这里。
    // 故失败后退避再试，最多 3 次：只拖慢本来就会失败的节点，成功的节点不受影响。
    for attempt in 0..3 {
        let start = Instant::now();
        match client
            .get(subhub_core::resources::ALIVE_TARGET)
            .send()
            .await
        {
            Ok(resp) => {
                let code = resp.status().as_u16();
                // 204 是 gstatic 的标准响应，但部分节点出口 / CDN / 缓存会返回 200 或 3xx。
                // 只要拿到 2xx/3xx 响应，就说明代理隧道确实打通并完成了一次真实 HTTP 往返，
                // 严格只认 204 会把这些「其实可用」的节点整批误判为不通 —— 此前 204 探测
                // 全军覆没、节点清一色「未实测」正是这个原因。4xx/5xx 才视为不通。
                // 同时把异常码打到 stderr，便于定位节点出口的真实行为。
                if (200..400).contains(&code) {
                    return Some(start.elapsed().as_millis() as u64);
                }
                eprintln!("[engine] 204 探测返回异常状态码: {code}");
            }
            Err(e) => eprintln!("[engine] 204 探测请求失败: {e}"),
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    None
}

/// 实测下载带宽（字节/秒）。跨多个候选端点逐级回退：每个端点先试一次，失败再试一次
/// （复用同一 client，不再拉进程），仍失败则换更小的分片，这样能在节点对大流量掐断
/// 时仍借助小分片产出真实测速，而不是静默 `None`。
async fn bandwidth_via(client: &reqwest::Client, name: &str) -> Option<f64> {
    let mut last_err: Option<String> = None;
    for url in subhub_core::resources::SPEED_DOWNLOAD_URLS {
        for attempt in 0..2 {
            let start = Instant::now();
            match client.get(*url).send().await {
                Ok(resp) => match resp.bytes().await {
                    Ok(bytes) => {
                        let secs = start.elapsed().as_secs_f64();
                        if secs > 0.0 && !bytes.is_empty() {
                            return Some(bytes.len() as f64 / secs);
                        }
                        last_err = Some(format!(
                            "empty body ({} bytes in {:.2}s)",
                            bytes.len(),
                            secs
                        ));
                    }
                    Err(e) => {
                        last_err = Some(format!("body read error: {e}"));
                        eprintln!(
                            "[bw] {} shard={} attempt={} body_err={}",
                            name, url, attempt, e
                        );
                    }
                },
                Err(e) => {
                    last_err = Some(format!("request error: {e}"));
                    eprintln!(
                        "[bw] {} shard={} attempt={} req_err={}",
                        name, url, attempt, e
                    );
                }
            }
        }
    }
    eprintln!("[bw] {} all shards failed: {:?}", name, last_err);
    None
}

/// 依次询问各 geo-IP channel，取第一个能解析出 2 字母国家码的响应。
async fn country_via(client: &reqwest::Client) -> Option<String> {
    for ch in subhub_core::resources::GEO_CHANNELS {
        if let Ok(r) = client.get(ch.url).send().await {
            if let Ok(body) = r.text().await {
                if let Some(cc) = subhub_core::resources::extract_country(&body) {
                    return Some(cc);
                }
            }
        }
    }
    None
}

/// 逐个流媒体服务探测解锁状态（BestSub `checker/tiktok.go` 风格）。
async fn unlock_via(
    client: &reqwest::Client,
) -> BTreeMap<String, subhub_core::model::UnlockResult> {
    let mut services: BTreeMap<String, subhub_core::model::UnlockResult> = BTreeMap::new();

    for svc in subhub_core::resources::STREAM_SERVICES {
        let u = match client.get(svc.url).send().await {
            Ok(r) => {
                let status = r.status().as_u16();

                match r.text().await {
                    Ok(body) => svc.detect.classify(status, &body),

                    Err(_) => subhub_core::model::UnlockResult {
                        status: "failed".into(),

                        region: None,
                    },
                }
            }

            Err(_) => subhub_core::model::UnlockResult {
                status: "failed".into(),

                region: None,
            },
        };

        services.insert(svc.id.to_string(), u);
    }

    services
}

/// Detect the outbound exit-country of each proxy (BestSub `country` pattern):
/// spin up the engine per proxy and ask the geo-IP channels for the exit IP's
/// country code. Returns `Option<String>` (2-letter code) per proxy, in input
/// order. `None` = engine missing / proxy dead / all channels failed.
pub async fn detect_outbound_country(
    proxies: &[Proxy],

    bin: &str,

    timeout_ms: u64,
    concurrency: usize,
) -> Vec<Option<String>> {
    probe_all(ProbeTask {
        proxies,
        bin,
        timeout_ms,
        bandwidth_timeout_ms: timeout_ms,
        concurrency,
        plan: ProbePlan {
            country: true,
            ..Default::default()
        },
        on_http: None,
        on_bw: None,
    })
    .await
    .into_iter()
    .map(|p| p.country)
    .collect()
}

/// Streaming-unlock matrix for each proxy (BestSub `checker/tiktok.go` style,
/// extended with Netflix/Disney/YouTube/ChatGPT). Returns a `ProxyUnlock` per
/// proxy, in input order. The map is empty when the engine is missing.
pub async fn detect_unlock(
    proxies: &[Proxy],

    bin: &str,

    timeout_ms: u64,
    concurrency: usize,
) -> Vec<subhub_core::model::ProxyUnlock> {
    probe_all(ProbeTask {
        proxies,
        bin,
        timeout_ms,
        bandwidth_timeout_ms: timeout_ms,
        concurrency,
        plan: ProbePlan {
            unlock: true,
            ..Default::default()
        },
        on_http: None,
        on_bw: None,
    })
    .await
    .into_iter()
    .map(|p| p.unlock)
    .collect()
}

/// Safely embed an arbitrary string as a YAML double-quoted flow scalar.
/// Node names come from subscription data (attacker-influenced), so a name
/// containing `"` or a newline could otherwise break out of the
/// `proxy-groups: [..]` flow sequence and inject arbitrary engine config
/// (e.g. `allow-lan`, `external-controller` + `secret`, poisoned `hosts`).
/// Wrapping in double quotes and escaping the dangerous characters prevents
/// that while keeping the node name intact.
fn escape_yaml_scalar(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Build a minimal single-node mihomo/sing-box-compatible config exposing a
/// SOCKS5/`mixed-port` listener on `port`.
fn build_engine_config(p: &Proxy, port: u16) -> String {
    let node = to_clash_meta(std::slice::from_ref(p));

    // `to_clash_meta` wraps in `proxies:\n  - ...`; reuse only the indented node.

    let node_body = node.trim_start_matches("proxies:").trim_start().to_string();

    let mut cfg = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(cfg, "mixed-port: {port}");
    // 用 `rule` 模式配合下方 `MATCH,PROXY` 规则，强制所有流量经 PROXY 组（即被测节点）
    // 转发。切勿改成 `global`：`global` 模式会忽略 rules、使用一个默认选为 DIRECT 的全局
    // 选择器，导致 mihomo 直接直连目标、不经代理节点——表现为「所有节点测速统一失败 /
    // 测不到带宽」，与 clash 能正常测速形成反差。
    let _ = writeln!(cfg, "mode: rule");
    // warning 而非 debug：全量测速会为每个节点拉起一个引擎进程，debug 级日志
    // 量随节点数线性爆炸（数千节点时日志刷屏且拖慢 IO）。warning 只在转发失败
    // 时输出，既保留「HTTP 探测为何失败」的关键线索，又不产生日常噪音。
    let _ = writeln!(cfg, "log-level: warning");
    // 引擎内需要解析「测速目标 / 节点 server」域名。缺少 dns 配置时 mihomo 对
    // 经代理的 HTTPS 目标建立隧道会失败（外层 reqwest 报 `error sending request`），
    // 导致所有带宽/存活探测连不上目标。显式配置公共 DNS 让隧道能正常建立。
    let _ = writeln!(cfg, "dns:");
    let _ = writeln!(cfg, "  enable: true");
    let _ = writeln!(cfg, "  ipv6: false");
    let _ = writeln!(cfg, "  enhanced-mode: normal");
    let _ = writeln!(cfg, "  nameserver:");
    let _ = writeln!(cfg, "    - 223.5.5.5");
    let _ = writeln!(cfg, "    - 119.29.29.29");
    let _ = writeln!(cfg, "proxies:");
    let _ = writeln!(cfg, "{node_body}");
    let _ = writeln!(cfg, "proxy-groups:");
    let _ = writeln!(cfg, "  - name: PROXY");
    let _ = writeln!(cfg, "    type: select");
    let _ = writeln!(cfg, "    proxies: [{}]", escape_yaml_scalar(&p.name));
    let _ = writeln!(cfg, "rules:");
    let _ = writeln!(cfg, "  - MATCH,PROXY");
    cfg
}

fn free_port() -> Option<u16> {
    let l = TcpListener::bind("127.0.0.1:0").ok()?;

    l.local_addr().ok().map(|a| a.port())
}

/// Kill any mihomo/sing-box processes whose command line references a
/// `subhub-engine` temp directory. Called once at server startup to reap
/// orphans left by a previous crash / force-kill (the `EngineGuard` RAII
/// only fires on normal exits; a `taskkill` without `/T` strands children).
///
/// Must be safe to call when zero such processes exist (the common case).
#[cfg(windows)]
pub fn cleanup_stale_engine_processes() {
    // PowerShell one-liner: find mihomo/clash/sing-box processes whose
    // CommandLine contains "subhub-engine", then force-stop each. Using
    // CommandLine (not just process name) avoids killing the user's own
    // Clash Verge / standalone mihomo that happens to share the binary.
    let ps = "Get-CimInstance Win32_Process -Filter \"Name like '%mihomo%' or Name like '%clash%' or Name like '%sing-box%'\" | Where-Object { $_.CommandLine -like '*subhub-engine*' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }";
    let _ = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", ps])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    // Also sweep stale temp dirs (orphaned config dirs from crashed runs).
    let prefix = "subhub-engine";
    if let Some(tmp) = std::env::temp_dir().to_str() {
        let _ = std::process::Command::new("powershell")
            .args([
                "-NoProfile", "-NonInteractive", "-Command",
                &format!(
                    "Get-ChildItem -Path '{}' -Directory -Filter '{}*' -ErrorAction SilentlyContinue | Remove-Item -Recurse -Force -ErrorAction SilentlyContinue",
                    tmp, prefix
                ),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

#[cfg(not(windows))]
pub fn cleanup_stale_engine_processes() {
    // On Unix, orphaned engine processes are reparented to init and will be
    // cleaned up by the OS. Stale temp dirs under /tmp are handled by tmpwatch.
}

/// Wait for the engine's listener to come up, but bail out the moment the
/// engine process *exits* on its own (crash / config error / wrong binary).
/// Without this, a misconfigured or incompatible engine would make every node
/// wait the full `wait_port` timeout before failing, so a 200-node speedtest
/// against a broken engine looks frozen ("tested 10 then nothing happens").
/// Returning early turns that into a fast stream of `None` results.
async fn engine_ready(child: &mut Child, port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        // If the engine already died, don't burn the rest of the budget.
        if child.try_wait().ok().flatten().is_some() {
            return false;
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::escape_yaml_scalar;

    #[test]
    fn escape_yaml_scalar_quotes_and_escapes() {
        // Plain name: wrapped in quotes, unchanged otherwise.
        assert_eq!(escape_yaml_scalar("My Node"), "\"My Node\"");
        // A name containing a double quote must be escaped, not break out of
        // the flow sequence (this is the N1 injection vector).
        assert_eq!(escape_yaml_scalar("ev\""), "\"ev\\\"\"");
        // A newline must be escaped, otherwise it would start a new YAML line
        // and let a crafted name inject arbitrary config.
        assert_eq!(escape_yaml_scalar("a\nb"), "\"a\\nb\"");
        // A backslash must be escaped so it isn't read as an escape sequence.
        assert_eq!(escape_yaml_scalar("a\\b"), "\"a\\\\b\"");
        // A realistic injection attempt: try to close the scalar and add a
        // malicious `allow-lan` key. The whole thing stays inside one quoted
        // scalar, so it can never become a second YAML key.
        let evil = "x\"\nallow-lan: true";
        let got = escape_yaml_scalar(evil);
        assert!(got.starts_with('"'), "must stay quoted");
        assert!(got.ends_with('"'), "must stay quoted");
        assert!(!got.contains("\nallow-lan"), "injection must be escaped");
    }

    #[test]
    fn build_engine_config_routes_through_proxy_via_rule_mode() {
        // 关键修复验证：engine 配置必须用 rule 模式 + MATCH,PROXY，强制流量经
        // 被测节点转发。global 模式会让 mihomo 直连目标、不经代理（表现为所有
        // 节点测速统一失败 / 测不到带宽），与 clash 能正常测速形成反差。
        let mut p = subhub_core::model::Proxy::new(
            "n1".into(),
            subhub_core::model::ProxyType::Vless,
            "example.com".into(),
            443,
        );
        p.uuid = Some("u".into());
        let cfg = super::build_engine_config(&p, 12345);
        assert!(cfg.contains("mode: rule"), "必须使用 rule 模式而非 global");
        assert!(
            !cfg.contains("mode: global"),
            "禁止 global 模式（会直连绕过代理）"
        );
        assert!(cfg.contains("MATCH,PROXY"), "必须将所有流量经 PROXY 组转发");
        assert!(cfg.contains("mixed-port: 12345"));
        assert!(cfg.contains("proxies:"));
        assert!(cfg.contains("example.com"));
    }
}
