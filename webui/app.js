const api = async (path, opts) => {
  const res = await fetch(path, opts);
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`HTTP ${res.status}: ${body}`);
  }
  const ct = res.headers.get("content-type") || "";
  return ct.includes("application/json") ? res.json() : res.text();
};

const postJson = (path, body) =>
  api(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });

const del = (path) => api(path, { method: "DELETE" });

// ---- global progress / hint banner ----
// Shows an indeterminate progress bar + descriptive text during long
// operations (add / import / refresh / detect / cleanup). The backend runs
// each op synchronously and returns once done, so this is an indeterminate
// indicator with a clear, operation-specific hint.
function showProgress(text) {
  const el = document.getElementById("progress");
  document.getElementById("progress-text").textContent = text;
  el.classList.remove("hidden");
}
function hideProgress() {
  document.getElementById("progress").classList.add("hidden");
  // 收起测速专用的实时进度区（仅测速时短暂显示）
  const detail = document.getElementById("progress-detail");
  if (detail) detail.classList.add("hidden");
}
// 测速专用：显示实时进度条 + 当前被测节点区，并重置进度
function showSpeedProgress(text) {
  showProgress(text);
  const detail = document.getElementById("progress-detail");
  if (detail) detail.classList.remove("hidden");
  const fill = document.getElementById("progress-fill");
  if (fill) fill.style.width = "0%";
  const cur = document.getElementById("progress-current");
  if (cur) cur.textContent = "";
}
// 根据单个 Progress 事件刷新进度条与“当前节点”行
function updateSpeedProgress(ev) {
  const { done, total, name, available, latency_ms, bandwidth_bps } = ev;
  const pct = total > 0 ? Math.round((done / total) * 100) : 0;
  const fill = document.getElementById("progress-fill");
  if (fill) fill.style.width = pct + "%";
  const cur = document.getElementById("progress-current");
  if (cur) {
    const lat = latency_ms != null ? latency_ms + " ms" : "超时";
    const bw = bandwidth_bps != null ? (bandwidth_bps / 1048576).toFixed(2) + " MB/s" : "—";
    const status = available ? "✓" : "✗";
    cur.textContent = `(${done}/${total}) ${status} ${name} · ${lat} · ${bw}`;
  }
}

// ---- nav ----
document.querySelectorAll(".nav-item").forEach((el) => {
  el.addEventListener("click", () => {
    document.querySelectorAll(".nav-item").forEach((n) => n.classList.remove("active"));
    document.querySelectorAll(".view").forEach((v) => v.classList.remove("active"));
    el.classList.add("active");
    document.getElementById("view-" + el.dataset.view).classList.add("active");
    if (el.dataset.view === "dashboard") loadDashboard();
    if (el.dataset.view === "nodes") loadNodes();
    if (el.dataset.view === "subscriptions") loadSubs("sub-list-manage", true);
    if (el.dataset.view === "merge") {
      ensureFilterRow();
      document.getElementById("btn-gen-url").click();
    }
  });
});

// ---- status ----
async function checkStatus() {
  try {
    await api("/api/health");
    const s = document.getElementById("status");
    s.textContent = "● 已连接";
    s.style.color = "var(--accent-2)";
  } catch {
    const s = document.getElementById("status");
    s.textContent = "● 离线";
    s.style.color = "var(--danger)";
  }
}

// ---- dashboard ----
const TYPE_COLORS = {
  ss: "#4f8cff",
  trojan: "#16a34a",
  vmess: "#d97706",
  vless: "#a855f7",
  hysteria2: "#06b6d4",
  tuic: "#ec4899",
  socks5: "#64748b",
  http: "#64748b",
  wireguard: "#64748b",
  other: "#64748b",
};

async function loadDashboard() {
  let d;
  try {
    d = await api("/api/dashboard");
  } catch (e) {
    // Dashboard summary failed (server error / non-JSON). Don't let that
    // break the whole tab: show an error card and still load the subscription
    // list + trend so the rest of the UI stays usable.
    const cards = document.getElementById("dash-cards");
    if (cards) {
      cards.innerHTML = `<div class="card err">仪表盘加载失败: ${escapeHtml(
        (e && e.message) || String(e)
      )}</div>`;
    }
    const donut = document.getElementById("type-donut");
    if (donut) donut.style.background = "#272d3a";
    loadSubs("sub-list", false);
    loadTrends();
    return;
  }
  const cards = document.getElementById("dash-cards");
  const latTxt = (v) => (v == null ? "—" : `${v} ms`);
  cards.innerHTML = `
    <div class="card"><div class="num">${d.total}</div><div class="label">节点总数</div></div>
    <div class="card"><div class="num">${d.subscriptions}</div><div class="label">订阅数</div></div>
    <div class="card"><div class="num up">${d.available}</div><div class="label">可用</div></div>
    <div class="card"><div class="num down">${d.unavailable}</div><div class="label">不可用</div></div>
    <div class="card"><div class="num">${latTxt(d.avg_latency_ms)}</div><div class="label">平均延迟</div></div>
    <div class="card"><div class="num">${latTxt(d.best_latency_ms)}</div><div class="label">最佳延迟</div></div>`;

  // type donut
  const entries = Object.entries(d.by_type || {}).sort((a, b) => b[1] - a[1]);
  const total = Math.max(1, d.total || 1);
  let acc = 0;
  const segs = entries
    .map(([k, v]) => {
      const start = (acc / total) * 360;
      acc += v;
      const end = (acc / total) * 360;
      const color = TYPE_COLORS[k] || "#64748b";
      return `${color} ${start}deg ${end}deg`;
    })
    .join(", ");
  const donut = document.getElementById("type-donut");
  donut.style.background = `conic-gradient(${segs || "#272d3a 0deg 360deg"})`;
  document.getElementById("type-legend").innerHTML = entries
    .map(
      ([k, v]) =>
        `<div class="legend-row"><span class="dotc" style="background:${
          TYPE_COLORS[k] || "#64748b"
        }"></span>${k}<b>${v}</b></div>`
    )
    .join("");

  renderBars("by-region", d.by_region);
  loadSubs("sub-list", false);
  loadTrends();
}

// ---- trend chart (vector / SVG, responsive) ----
let _trendPts = [];

async function loadTrends() {
  let pts = [];
  try {
    pts = await api("/api/trends");
  } catch {
    pts = [];
  }
  _trendPts = pts;
  drawTrend(pts);
  document.getElementById("trend-info").textContent =
    pts.length > 0 ? `${pts.length} 个采样点` : "暂无数据（先添加订阅或测速）";
}

function drawTrend(pts) {
  const svg = document.getElementById("trend-svg");
  const H = 220;
  // viewBox width tracks the rendered pixel width so the vector never
  // stretches/distorts when the window is resized (1:1 scale).
  const W = Math.max(320, Math.floor(svg.parentElement.clientWidth) - 36);
  svg.setAttribute("viewBox", `0 0 ${W} ${H}`);
  svg.setAttribute("width", "100%");
  svg.setAttribute("height", String(H));

  if (pts.length < 2) {
    svg.innerHTML = `<text x="${W / 2}" y="${H / 2}" fill="#7a8499" font-size="14" text-anchor="middle">数据不足，执行一次测速或刷新后即有趋势。</text>`;
    document.getElementById("trend-legend").innerHTML = "";
    return;
  }

  const padL = 44,
    padR = 16,
    padT = 16,
    padB = 28;
  const plotW = W - padL - padR;
  const plotH = H - padT - padB;
  const n = pts.length;
  const maxTotal = Math.max(1, ...pts.map((p) => p.total));
  const maxLat = Math.max(1, ...pts.map((p) => p.avg_latency_ms || 0));
  const x = (i) => padL + (plotW * i) / (n - 1);
  const yOf = (v, max) => padT + plotH - (plotH * Math.min(v, max)) / max;

  const parts = [];

  // grid + y axis (node count)
  for (let g = 0; g <= 4; g++) {
    const y = padT + (plotH * g) / 4;
    parts.push(`<line x1="${padL}" y1="${y}" x2="${W - padR}" y2="${y}" stroke="rgba(255,255,255,0.08)"/>`);
    const val = Math.round((maxTotal * (4 - g)) / 4);
    parts.push(`<text x="8" y="${y + 4}" fill="#7a8499" font-size="11">${val}</text>`);
  }

  const poly = (key, max, color, dash) =>
    `<polyline points="${pts
      .map((p, i) => `${x(i).toFixed(1)},${yOf(p[key] || 0, max).toFixed(1)}`)
      .join(" ")}" fill="none" stroke="${color}" stroke-width="2" ${
      dash ? 'stroke-dasharray="4 4"' : ""
    }/>`;

  parts.push(poly("total", maxTotal, "#1470ff", false));
  parts.push(poly("available", maxTotal, "#048867", false));
  parts.push(poly("untested", maxTotal, "#af7b00", false));
  // average latency (right axis, dashed)
  parts.push(poly("avg_latency_ms", maxLat, "#c33a2f", true));

  svg.innerHTML = parts.join("");

  // legend (HTML below the SVG)
  const legend = [
    ["总节点", "#1470ff"],
    ["可用", "#048867"],
    ["未测", "#af7b00"],
    ["平均延迟", "#c33a2f"],
  ];
  document.getElementById("trend-legend").innerHTML = legend
    .map(([t, c]) => `<span class="lg-item"><i style="background:${c}"></i>${t}</span>`)
    .join("");
}

// redraw trend on window resize (vector stays crisp, no stretch)
let _trendResizeTimer = null;
window.addEventListener("resize", () => {
  clearTimeout(_trendResizeTimer);
  _trendResizeTimer = setTimeout(() => {
    if (_trendPts.length) drawTrend(_trendPts);
  }, 150);
});

function renderBars(id, obj) {
  const el = document.getElementById(id);
  const entries = Object.entries(obj).sort((a, b) => b[1] - a[1]);
  const max = Math.max(1, ...entries.map((e) => e[1]));
  el.innerHTML = entries
    .map(
      ([k, v]) => `
      <div class="bar-row">
        <div class="bar-label">${escapeHtml(k)}</div>
        <div class="bar-track"><div class="bar-fill" style="width:${(v / max) * 100}%"></div></div>
        <div class="bar-val">${v}</div>
      </div>`
    )
    .join("");
}

// ---- subscriptions (per-subscription health) ----
const STATUS_META = {
  healthy:  { label: "健康",     cls: "st-healthy" },
  degraded: { label: "部分可用", cls: "st-degraded" },
  down:     { label: "无可用节点", cls: "st-down" },
  error:    { label: "拉取失败",  cls: "st-error" },
  empty:    { label: "无节点",   cls: "st-empty" },
  untested: { label: "未测速",   cls: "st-pending" },
  disabled: { label: "已禁用",   cls: "st-disabled" },
  pending:  { label: "待检测",   cls: "st-pending" },
};

// Colour class for the per-subscription health-degree bar.
function healthPctCls(v) {
  if (v == null) return "hp-unknown";
  if (v >= 80) return "hp-good";
  if (v >= 50) return "hp-mid";
  return "hp-bad";
}
// Bar width (%) — 0 when unknown so the track shows empty.
function healthPctWidth(v) {
  return v == null ? 0 : v;
}

// ---- traffic usage rendering (clash info block / Subscription-Userinfo) ----
function formatBytes(n) {
  if (n == null) return "—";
  const u = ["B", "KB", "MB", "GB", "TB", "PB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i++;
  }
  return (i === 0 ? v : v.toFixed(v < 10 ? 2 : 1)) + " " + u[i];
}

function formatExpire(ms) {
  if (!ms) return null;
  const d = new Date(ms);
  if (isNaN(d.getTime())) return null;
  const pad = (x) => String(x).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

// Build the usage card block for a subscription, or "" when there is no usage
// info (pasted/local sources, or a sub that doesn't report usage).
function usageHtml(s) {
  const hasTotal = s.total != null;
  const hasUsed = s.download != null || s.upload != null;
  if (!hasTotal && !hasUsed) return "";

  const used = (s.download || 0) + (s.upload || 0);
  const total = s.total;
  let pct = 0;
  let totalStr;
  if (total === 0) {
    // Some providers report total=0 to mean "unlimited".
    totalStr = "无限流量";
  } else if (total != null) {
    pct = Math.min(100, Math.round((used / total) * 100));
    totalStr = formatBytes(total);
  } else {
    totalStr = "";
  }
  const usedStr = formatBytes(used);
  const cls = pct >= 90 ? "ub-bad" : pct >= 70 ? "ub-mid" : "ub-good";
  const expireStr = formatExpire(s.expire);

  return `
    <div class="sub-usage">
      <div class="usage-bar-wrap" title="已用 ${usedStr}${total != null && total > 0 ? " / 共 " + totalStr : ""}">
        <div class="usage-bar ${cls}" style="width:${pct}%"></div>
      </div>
      <div class="usage-meta">
        <span>${usedStr}${totalStr ? " / " + totalStr : ""}</span>
        ${expireStr ? `<span class="usage-expire">到期 ${expireStr}</span>` : ""}
      </div>
    </div>`;
}


// Mirror of Rust-side ProxyUnlock::summary() — builds a compact text like
// "TT✓HK NF✗ YT✓US" from a JSON-serialized unlock result.
const UNLOCK_SHORT = [
  ["tiktok", "TT"],
  ["netflix", "NF"],
  ["disney", "DS"],
  ["youtube", "YT"],
  ["chatgpt", "GPT"],
];
function summaryUnlock(u) {
  const s = u.services;
  if (!s || typeof s !== "object") return "—";
  const parts = [];
  for (const [id, short] of UNLOCK_SHORT) {
    const r = s[id];
    if (!r) continue;
    switch (r.status) {
      case "unlocked": parts.push(short + "✓" + (r.region || "")); break;
      case "blocked": parts.push(short + "✗"); break;
      case "failed": parts.push(short + "?"); break;
    }
  }
  return parts.length ? parts.join("  ") : "—";
}

function relTime(ms) {
  if (!ms) return "—";
  const diff = Date.now() - ms;
  if (diff < 0) return "刚刚";
  const s = Math.floor(diff / 1000);
  if (s < 60) return `${s} 秒前`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} 分钟前`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h} 小时前`;
  return `${Math.floor(h / 24)} 天前`;
}

async function loadSubs(targetId, withManage) {
  let list = [];
  try {
    const params = new URLSearchParams();
    const sq = document.getElementById("sub-q");
    if (sq && sq.value.trim()) params.set("q", sq.value.trim());
    const qs = params.toString();
    list = await api("/api/subscriptions" + (qs ? "?" + qs : ""));
  } catch {
    list = [];
  }
  // Sort by health degree (health_pct) descending — healthiest first.
  // Untracked subscriptions (health_pct == null) sink to the bottom.
  list.sort((a, b) => (b.health_pct ?? -1) - (a.health_pct ?? -1));
  const el = document.getElementById(targetId);
  if (!list.length) {
    el.innerHTML = `<div class="empty">还没有订阅，先在「订阅管理」添加或粘贴。</div>`;
    return;
  }
  const lat = (v) => (v == null ? "—" : `${v} ms`);
  el.innerHTML = list
    .map((s) => {
      const meta = STATUS_META[s.status] || STATUS_META.pending;
      return `
      <div class="sub-card" data-id="${s.id}">
        <div class="sub-card-top">
          <span class="dot ${meta.cls}"></span>
          <div class="sub-name">${escapeHtml(s.name)}</div>
          <span class="badge">${s.source_type === "remote" ? "远程" : "本地"}</span>
          <span class="badge ${meta.cls}">${meta.label}</span>
        </div>
        <div class="sub-card-meta">${escapeHtml(s.source)}</div>
        <div class="sub-card-stats">
          <div><b>${s.count}</b><span>节点</span></div>
          <div><b class="up">${s.healthy}</b><span>可用</span></div>
          <div><b>${s.unknown}</b><span>未测</span></div>
          <div><b>${lat(s.avg_latency_ms)}</b><span>平均延迟</span></div>
          <div><b>${lat(s.best_latency_ms)}</b><span>最佳延迟</span></div>
        </div>
        <div class="health-bar-wrap" title="健康度 = 可用节点 / 节点总数">
          <div class="health-bar ${healthPctCls(s.health_pct)}" style="width:${healthPctWidth(s.health_pct)}%"></div>
          <span class="health-pct">${s.health_pct == null ? "—" : s.health_pct + "%"}</span>
        </div>
        ${usageHtml(s)}
        <div class="sub-card-foot">
          <span>检测 ${relTime(s.last_checked_at)}</span>
          <span>更新 ${relTime(s.last_updated_at)}</span>
        </div>
        ${s.last_error ? `<div class="sub-err">⚠ ${escapeHtml(s.last_error)}</div>` : ""}
        ${
          withManage
            ? `<div class="sub-card-actions">
                 <button class="btn sub-refresh" data-id="${s.id}">刷新</button>
                 <button class="btn sub-del" data-id="${s.id}">删除</button>
               </div>`
            : ""
        }
      </div>`;
    })
    .join("");

  if (withManage) {
    el.querySelectorAll(".sub-del").forEach((b) =>
      b.addEventListener("click", async () => {
        await del("/api/subscriptions/" + b.dataset.id);
        loadSubs(targetId, true);
        loadDashboard();
      })
    );
    el.querySelectorAll(".sub-refresh").forEach((b) =>
      b.addEventListener("click", async () => {
        b.disabled = true;
        const old = b.textContent;
        b.textContent = "刷新中…";
        try {
          await postJson("/api/subscriptions/" + b.dataset.id + "/refresh", {});
        } catch (e) {
          console.error(e);
        }
        loadSubs(targetId, true);
        loadDashboard();
      })
    );
  }
}

// ---- subscriptions add / import ----
document.getElementById("btn-add").addEventListener("click", async () => {
  const urls = document
    .getElementById("sub-urls")
    .value.split("\n")
    .map((s) => s.trim())
    .filter(Boolean);
  if (!urls.length) return;
  const proxy = document.getElementById("sub-proxy").value.trim();
  const useProxy = document.getElementById("use-proxy").checked;
  const btn = document.getElementById("btn-add");
  btn.disabled = true;
  showProgress(
    `正在拉取并解析 ${urls.length} 个订阅${
      useProxy && proxy ? "（走代理）" : ""
    }，并完成自动测速…`
  );
  try {
    const r = await postJson("/api/subscriptions", {
      urls,
      fetch_proxy: proxy ? proxy : undefined,
    });
    document.getElementById("add-result").textContent =
      `已添加 ${r.added} 个节点，共 ${r.total} 个（${r.subscriptions} 个订阅），已自动测速`;
    loadSubs("sub-list-manage", true);
    loadDashboard();
  } catch (e) {
    document.getElementById("add-result").textContent = "添加失败：" + e.message;
  } finally {
    hideProgress();
    btn.disabled = false;
  }
});

document.getElementById("btn-import").addEventListener("click", async () => {
  const content = document.getElementById("raw-nodes").value;
  if (!content.trim()) return;
  const name = document.getElementById("import-name").value.trim();
  const btn = document.getElementById("btn-import");
  btn.disabled = true;
  showProgress("正在解析并导入节点，并完成自动测速…");
  try {
    const r = await postJson("/api/import", {
      content,
      name: name ? name : undefined,
    });
    document.getElementById("import-result").textContent =
      `已导入 ${r.added} 个节点，共 ${r.total} 个（${r.subscriptions} 个订阅），已自动测速`;
    loadSubs("sub-list-manage", true);
    loadDashboard();
  } catch (e) {
    document.getElementById("import-result").textContent = "导入失败：" + e.message;
  } finally {
    hideProgress();
    btn.disabled = false;
  }
});

// export the full subscription list (with node results) as a JSON file
document.getElementById("btn-export-subs").addEventListener("click", async () => {
  const btn = document.getElementById("btn-export-subs");
  btn.disabled = true;
  showProgress("正在导出订阅列表…");
  try {
    const r = await api("/api/subscriptions/export");
    const text = JSON.stringify(r, null, 2);
    const blob = new Blob([text], { type: "application/json" });
    const a = document.createElement("a");
    a.href = URL.createObjectURL(blob);
    a.download = `subhub-subscriptions.json`;
    a.click();
    document.getElementById("export-subs-result").textContent =
      `已导出 ${r.subscriptions.length} 个订阅、共 ${r.subscriptions.reduce(
        (n, s) => n + s.proxies.length,
        0
      )} 个节点`;
  } catch (e) {
    document.getElementById("export-subs-result").textContent =
      "导出失败：" + e.message;
  } finally {
    hideProgress();
    btn.disabled = false;
  }
});

// import a subscription list from a JSON file (merge by source URL)
document.getElementById("btn-import-subs").addEventListener("click", async () => {
  const fileInput = document.getElementById("import-subs-file");
  const file = fileInput.files && fileInput.files[0];
  if (!file) {
    document.getElementById("import-subs-result").textContent = "请先选择导出的 JSON 文件";
    return;
  }
  const btn = document.getElementById("btn-import-subs");
  btn.disabled = true;
  showProgress("正在导入订阅列表…");
  try {
    const text = await file.text();
    const doc = JSON.parse(text);
    const subs = Array.isArray(doc.subscriptions) ? doc.subscriptions : doc;
    const r = await postJson("/api/subscriptions/import", { subscriptions: subs });
    document.getElementById("import-subs-result").textContent =
      `已新增 ${r.added} 个、更新 ${r.replaced} 个订阅，共 ${r.subscriptions} 个订阅 / ${r.total} 个节点`;
    loadSubs("sub-list-manage", true);
    loadDashboard();
  } catch (e) {
    document.getElementById("import-subs-result").textContent =
      "导入失败：" + e.message;
  } finally {
    hideProgress();
    btn.disabled = false;
    fileInput.value = "";
  }
});

// test the configured pull-proxy before using it
document.getElementById("btn-proxy-test").addEventListener("click", async (e) => {
  const proxy = document.getElementById("sub-proxy").value.trim();
  if (!proxy) {
    alert("请先填写代理地址，例如 http://127.0.0.1:7890");
    return;
  }
  const btn = e.currentTarget;
  btn.disabled = true;
  showProgress(`正在通过 ${proxy} 测试连通性…`);
  try {
    const r = await postJson("/api/proxy-test", { proxy });
    if (r.ok) {
      alert(`代理可用 ✓（探测状态码 ${r.status}）`);
    } else {
      alert("代理不可用：\n" + (r.error || "未知错误"));
    }
  } catch (err) {
    alert("测试失败：" + err.message);
  } finally {
    hideProgress();
    btn.disabled = false;
  }
});

// ---- global "use proxy" master switch ----
// Backend is the source of truth: we load the current value on startup and
// push changes to /api/settings (persisted to the SQLite meta table).
async function loadSettings() {
  try {
    const s = await api("/api/settings");
    document.getElementById("use-proxy").checked = !!s.use_proxy;
    const minInput = document.getElementById("auto-refresh-min");
    if (minInput) minInput.value = Math.round((s.auto_refresh_sec || 0) / 60);
    // Prefill the node-health re-test interval (persisted setting, minutes).
    const healthInput = document.getElementById("health-check-min");
    if (healthInput) healthInput.value = Math.round((s.node_health_check_sec || 0) / 60);
    // Prefill the "pull proxy" box from the server-side default (single source
    // of truth, persisted in the meta table — replaces the old browser-local
    // "remember" checkbox).
    const proxyInput = document.getElementById("sub-proxy");
    if (proxyInput && s.default_fetch_proxy) proxyInput.value = s.default_fetch_proxy;
    // Prefill the global Top-N (persisted setting) into the Settings page —
    // the single place Top-N is configured; it drives both manual export and
    // the standing /sub subscribe URL. top_n = 0 means "all", toggle off.
    const topn = s.top_n != null ? s.top_n : 0;
    const topnEl = document.getElementById("topn");
    const topnOnEl = document.getElementById("topn-on");
    if (topnEl && topn > 0) topnEl.value = topn;
    if (topnOnEl) topnOnEl.checked = topn > 0;
    // Prefill the external speed-test engine binary path (persisted setting).
    const engineBin = document.getElementById("engine-bin");
    if (engineBin) engineBin.value = s.engine_bin || "";
    // Prefill the auto-remove threshold (persisted setting).
    const removeAfter = document.getElementById("remove-after");
    if (removeAfter) removeAfter.value = s.remove_after_fails != null ? s.remove_after_fails : 0;
  } catch {
    /* keep default (checked) if the API is unreachable */
  }
}

document.getElementById("use-proxy").addEventListener("change", async (e) => {
  const v = e.currentTarget.checked;
  try {
    await postJson("/api/settings", { use_proxy: v });
  } catch (err) {
    alert("保存代理开关失败：" + err.message);
  }
});

// Save the auto-refresh interval (minutes -> seconds) to the backend. The
// scheduler reads /api/settings live, so no restart is needed.
document.getElementById("btn-save-refresh").addEventListener("click", async () => {
  const min = parseInt(document.getElementById("auto-refresh-min").value, 10);
  const sec = (isNaN(min) ? 0 : Math.max(0, min)) * 60;
  const useProxy = document.getElementById("use-proxy").checked;
  const el = document.getElementById("refresh-save-result");
  try {
    const r = await postJson("/api/settings", {
      use_proxy: useProxy,
      auto_refresh_sec: sec,
    });
    el.style.color = "";
    el.textContent =
      r.auto_refresh_sec > 0
        ? `已保存：每 ${Math.round(r.auto_refresh_sec / 60)} 分钟自动刷新`
        : "已保存：已关闭定时刷新";
  } catch (err) {
    el.style.color = "var(--danger, #ef4444)";
    el.textContent = "保存失败：" + err.message;
  }
});

// Save the node-health periodic re-test interval (minutes -> seconds). A
// separate, independent toggle from subscription auto-refresh; the scheduler
// reads /api/settings live, so no restart is needed.
document.getElementById("btn-save-health").addEventListener("click", async () => {
  const min = parseInt(document.getElementById("health-check-min").value, 10);
  const sec = (isNaN(min) ? 0 : Math.max(0, min)) * 60;
  const el = document.getElementById("health-save-result");
  try {
    const r = await postJson("/api/settings", { node_health_check_sec: sec });
    el.style.color = "";
    el.textContent =
      r.node_health_check_sec > 0
        ? `已保存：每 ${Math.round(r.node_health_check_sec / 60)} 分钟自动重测节点健康`
        : "已保存：已关闭节点健康定时重测";
  } catch (err) {
    el.style.color = "var(--danger, #ef4444)";
    el.textContent = "保存失败：" + err.message;
  }
});

// Global Top-N — configured only on the Settings page. Any change is persisted
// immediately (top_n = 0 when the toggle is off = export/subscribe everything)
// and drives BOTH the manual export (/api/export) and the standing subscribe
// URL (/sub). A /sub URL or /api/export body may still pass top_n to override
// it for a one-off.
async function saveTopN() {
  const on = document.getElementById("topn-on").checked;
  const n = parseInt(document.getElementById("topn").value, 10);
  const nn = on && !isNaN(n) && n > 0 ? n : 0;
  const el = document.getElementById("topn-save-result");
  try {
    const r = await postJson("/api/settings", { top_n: nn });
    if (el) {
      el.style.color = "";
      el.textContent = r.top_n > 0 ? `已保存：Top-${r.top_n}` : "已保存：不限制（全部节点）";
    }
  } catch (err) {
    // A silent failure here means the user believes Top-N is active while the
    // /sub URL still exports everything — always surface the error.
    if (el) {
      el.style.color = "var(--danger, #ef4444)";
      el.textContent = "保存失败：" + err.message;
    }
  }
}
document.getElementById("topn-on").addEventListener("change", saveTopN);
document.getElementById("topn").addEventListener("change", saveTopN);

// Save the external speed-test engine binary path to the backend (persisted to
// the SQLite meta table). Empty value clears it (engine disabled → basic TCP
// latency + throughput estimate only). The next speedtest picks it up live.
document.getElementById("btn-save-engine").addEventListener("click", async () => {
  const raw = document.getElementById("engine-bin").value || "";
  const val = raw.trim();
  const el = document.getElementById("engine-save-result");
  try {
    const r = await postJson("/api/settings", { engine_bin: val });
    el.style.color = "";
    el.textContent = r.engine_bin
      ? `已保存引擎：${r.engine_bin}（下次测速生效）`
      : "已保存：已关闭测速引擎（仅基础 TCP 测速）";
  } catch (err) {
    el.style.color = "var(--danger, #ef4444)";
    el.textContent = "保存失败：" + err.message;
  }
});

// Save the auto-remove threshold (persisted to the SQLite meta table). 0
// disables auto-removal. The next speedtest applies it and reports how many
// nodes were removed.
document.getElementById("btn-save-remove").addEventListener("click", async () => {
  const raw = parseInt(document.getElementById("remove-after").value, 10);
  const n = isNaN(raw) ? 0 : Math.max(0, raw);
  const el = document.getElementById("remove-save-result");
  try {
    const r = await postJson("/api/settings", { remove_after_fails: n });
    el.style.color = "";
    el.textContent = r.remove_after_fails > 0
      ? `已保存：连续不可用 ${r.remove_after_fails} 次后自动移除`
      : "已保存：已关闭自动移除";
  } catch (err) {
    el.style.color = "var(--danger, #ef4444)";
    el.textContent = "保存失败：" + err.message;
  }
});

// ---- nodes ----
let _nodes = [];
let _groupBySub = false;
let _page = 1;
let _pageSize = 50;
let _total = 0;
let _sortField = null;   // name | latency | speed | score (null = 默认按 name)
let _sortDesc = false;   // 升序/降序，由 sortNodes 管理

async function loadNodes(page) {
  if (page !== undefined) _page = page;
  const q = document.getElementById("f-q").value;
  const type = document.getElementById("f-type").value;
  const region = document.getElementById("f-region").value;
  const params = new URLSearchParams();
  if (q) params.set("q", q);
  if (type) params.set("type", type);
  if (region) params.set("region", region);
  if (_sortField) params.set("sort", _sortField);
  if (_sortField) params.set("desc", _sortDesc ? "true" : "false");
  params.set("page", _page);
  params.set("page_size", _pageSize);
  let resp;
  try {
    resp = await api("/api/proxies?" + params.toString());
  } catch (err) {
    // Surface the failure in the table itself instead of leaving a silently
    // blank list (which reads as "you have no nodes").
    const tbody = document.querySelector("#nodes-table tbody");
    if (tbody) {
        tbody.innerHTML =
        `<tr><td colspan="13" style="text-align:center;color:var(--danger,#ef4444);padding:16px">` +
        `节点列表加载失败：${escapeHtml(err.message)}（可点击「刷新」重试）</td></tr>`;
    }
    return;
  }
  _nodes = resp.items || [];
  _total = resp.total || 0;
  // Clamp the page: totals shrink (auto-remove / deleting a subscription /
  // narrower filters), and a stale _page past the last page would render an
  // empty table with working-looking pager buttons.
  const totalPages = Math.max(1, Math.ceil(_total / _pageSize));
  if (_page > totalPages) {
    return loadNodes(totalPages);
  }
  renderNodes(_nodes);
  renderPager();
}

function renderPager() {
  const totalPages = Math.max(1, Math.ceil(_total / _pageSize));
  const startN = _total === 0 ? 0 : (_page - 1) * _pageSize + 1;
  const endN = Math.min(_total, _page * _pageSize);
  document.getElementById("pg-info").textContent =
    `第 ${_page}/${totalPages} 页 · 显示 ${startN}-${endN} / 共 ${_total} 个`;
  document.getElementById("pg-first").disabled = _page <= 1;
  document.getElementById("pg-prev").disabled = _page <= 1;
  document.getElementById("pg-next").disabled = _page >= totalPages;
  document.getElementById("pg-last").disabled = _page >= totalPages;
}

function latencyClass(ms) {
  if (ms == null) return "lat-unknown";
  if (ms < 200) return "lat-good";
  if (ms < 500) return "lat-ok";
  return "lat-bad";
}

function scoreClass(s) {
  if (s == null) return "lat-unknown";
  if (s >= 60) return "lat-good";
  if (s >= 30) return "lat-ok";
  return "lat-bad";
}

function scoreText(s) {
  return s == null ? "—" : Number(s).toFixed(1);
}

function renderNodes(list) {
  const tbody = document.querySelector("#nodes-table tbody");
  const rowHtml = (p) => {
    const lat = p.latency_ms != null ? `${p.latency_ms} ms` : "—";
    const speed =
      p.download_speed_bps != null
        ? `${(p.download_speed_bps / 1_000_000).toFixed(1)} MB/s`
        : "—";
    const unlock = p.unlock ? summaryUnlock(p.unlock) : "—";
    const avail =
      p.available === false ? "down" : p.available === true ? "up" : "unknown";
    const dot =
      avail === "up"
        ? '<span class="dot up"></span>可用'
        : avail === "down"
        ? `<span class="dot down"></span>不可用${p.consecutive_failures > 0 ? " ×" + p.consecutive_failures : ""}`
        : '<span class="dot"></span>未测';
    return `<tr>
      <td>${escapeHtml(p.name)}</td>
      <td class="sub-cell" title="${escapeHtml(p.sub_name || "")}">${escapeHtml(p.sub_name || "—")}</td>
      <td><span class="tag">${p.type_}</span></td>
      <td>${escapeHtml(p.server)}</td>
      <td>${p.port}</td>
      <td>${escapeHtml(p.outbound_country || p.region || "OTHER")}</td>
      <td>${escapeHtml(p.outbound_country || "—")}</td>
      <td class="${latencyClass(p.latency_ms)}">${lat}</td>
      <td>${speed}</td>
      <td class="unlock-cell">${escapeHtml(unlock)}</td>
      <td>${dot}</td>
      <td class="${scoreClass(p.score)}">${scoreText(p.score)}</td>
      <td class="row-actions"><button class="btn btn-del-node" data-sub="${escapeHtml(p.sub_id)}" data-fp="${escapeHtml(p.fingerprint)}" data-name="${escapeHtml(p.name)}" title="删除该节点">✕</button></td>
    </tr>`;
  };

  if (_groupBySub) {
    // sub-store grouping: render nodes grouped by their source subscription
    const groups = {};
    for (const p of list) {
      const key = p.sub_name || "—";
      (groups[key] = groups[key] || []).push(p);
    }
    const ordered = Object.keys(groups).sort((a, b) => a.localeCompare(b));
    tbody.innerHTML = ordered
      .map((name) => {
        const rows = groups[name].map(rowHtml).join("");
        return `<tr class="group-row"><td colspan="13">${escapeHtml(name)} · ${groups[name].length} 个节点</td></tr>${rows}`;
      })
      .join("");
  } else {
    tbody.innerHTML = list.map(rowHtml).join("");
  }

  document.getElementById("nodes-count").textContent =
    `本页 ${list.length} 个 · 共 ${_total} 个节点`;

  const sel = document.getElementById("f-type");
  if (sel.options.length <= 1) {
    [...new Set(list.map((p) => p.type_))].forEach((t) => {
      const o = document.createElement("option");
      o.value = t;
      o.textContent = t;
      sel.appendChild(o);
    });
  }
}

document.getElementById("btn-group").addEventListener("click", (e) => {
  _groupBySub = !_groupBySub;
  e.currentTarget.classList.toggle("active", _groupBySub);
  renderNodes(_nodes);
});

function updateSortIndicators() {
  document
    .querySelectorAll("#nodes-table th[data-sort]")
    .forEach((th) => {
      const active = th.dataset.sort === _sortField;
      th.classList.toggle("sorted", active);
      th.classList.toggle("desc", active && _sortDesc);
    });
  document
    .getElementById("btn-sort-latency")
    .classList.toggle("active", _sortField === "latency");
  document
    .getElementById("btn-sort-score")
    .classList.toggle("active", _sortField === "score");
  document
    .getElementById("btn-sort-speed")
    .classList.toggle("active", _sortField === "speed");
}

function sortNodes(field) {
  if (_sortField === field) {
    _sortDesc = !_sortDesc; // 再次点击同列 → 切换升降序
  } else {
    _sortField = field;
    // 默认方向：评分/速度降序（高在前），延迟/名称升序（低在前）
    _sortDesc = field === "score" || field === "speed";
  }
  updateSortIndicators();
  // 全局排序由后端完成：重新拉取第一页即可得到全局最优的前 N 个，
  // 而不是仅对当前页的 50 个本地排序。
  loadNodes(1);
}

document.getElementById("btn-sort-latency").addEventListener("click", () => sortNodes("latency"));
document.getElementById("btn-sort-score").addEventListener("click", () => sortNodes("score"));
document.getElementById("btn-sort-speed").addEventListener("click", () => sortNodes("speed"));

// clickable column headers (延迟 / 速度 / 评分) for sorting
document.querySelector("#nodes-table thead").addEventListener("click", (e) => {
  const th = e.target.closest("th[data-sort]");
  if (th) sortNodes(th.dataset.sort);
});

// delegated delete handler for the per-node "✕" buttons (rows are re-rendered
// on every load, so we listen on the stable table element instead).
document.getElementById("nodes-table").addEventListener("click", async (e) => {
  const btn = e.target.closest(".btn-del-node");
  if (!btn) return;
  const name = btn.dataset.name || "该节点";
  if (!confirm(`确定从「${btn.dataset.sub}」删除节点「${name}」？`)) return;
  btn.disabled = true;
  try {
    await postJson("/api/proxies/delete", {
      sub_id: btn.dataset.sub,
      fingerprint: btn.dataset.fp,
    });
    await loadNodes();
    loadDashboard();
  } catch (err) {
    alert("删除失败：" + err.message);
    btn.disabled = false;
  }
});

document.getElementById("btn-speedtest").addEventListener("click", async (e) => {
  const btn = e.currentTarget;
  const resultEl = document.getElementById("speedtest-result");
  btn.disabled = true;
  if (resultEl) resultEl.textContent = "";
  const mode = document.getElementById("test-mode").value;
  const modeText = mode === "untested" ? "未测过的节点" : mode === "failed" ? "测速失败的节点" : "全部节点";
  showSpeedProgress(`正在测速${modeText}（TCP 延迟 + 速度估算；配置测速引擎可得真实带宽，可能需要一会儿）…`);
  const params = new URLSearchParams({ timeout_ms: 4000, concurrency: 20, mode });
  try {
    const resp = await fetch(`/api/speedtest?${params.toString()}`);
    if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
    // 按 SSE 逐事件读取：Progress（进度）/ Done（汇总）
    const reader = resp.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    let summary = null;
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      let idx;
      while ((idx = buf.indexOf("\n\n")) !== -1) {
        const chunk = buf.slice(0, idx);
        buf = buf.slice(idx + 2);
        for (const line of chunk.split("\n")) {
          if (!line.startsWith("data:")) continue;
          const payload = line.slice(5).trim();
          if (!payload) continue;
          let ev;
          try { ev = JSON.parse(payload); } catch { continue; }
          if (ev.type === "Progress") {
            updateSpeedProgress(ev);
          } else if (ev.type === "Done") {
            summary = ev;
          }
        }
      }
    }
    await loadNodes();
    loadDashboard();
    if (resultEl && summary) {
      const { tested, reachable, avg_latency_ms, with_http, with_bw, removed, threshold } = summary;
      let msg =
        `测速完成：共 ${tested} 个，可达 ${reachable} 个，` +
        `平均延迟 ${avg_latency_ms != null ? avg_latency_ms + " ms" : "—"}（含 HTTP 延迟 ${with_http} 个，带宽 ${with_bw} 个）`;
      if (removed > 0) msg += `；已自动移除连续不可用节点 ${removed} 个（阈值 ${threshold} 次）`;
      resultEl.textContent = msg;
    }
  } catch (err) {
    alert("测速失败: " + err.message);
  } finally {
    hideProgress();
    btn.disabled = false;
  }
});

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
  );
}

document.getElementById("btn-refresh").addEventListener("click", loadNodes);

document.getElementById("btn-geo").addEventListener("click", async (e) => {
  const btn = e.currentTarget;
  btn.disabled = true;
  const mode = document.getElementById("test-mode").value;
  const modeText = mode === "untested" ? "未测过的节点" : mode === "failed" ? "测速失败的节点" : "全部节点";
  showProgress(`正在检测${modeText}的出口地区…`);
  try {
    const countries = await postJson("/api/geo-detect", { timeout_ms: 8000, concurrency: 10, mode });
    await loadNodes();
    if (!countries || countries.length === 0) {
      alert(`当前范围（${modeText}）内没有可检测的节点。`);
    } else {
      const detected = countries.filter((c) => c && c.country).length;
      if (detected === 0) {
        alert("未检测到任何出口地区。\n出口地区检测需要先在「设置」中配置测速引擎（mihomo / sing-box 路径，即 SUBHUB_ENGINE_BIN）。未配置时该功能不生效。");
      } else {
        document.getElementById("nodes-count").textContent =
          `出口地区检测完成：${detected}/${countries.length} 个节点已识别出口地区（见节点表「出口」列）`;
      }
    }
  } catch (err) {
    alert("出口地区检测失败：请先配置 SUBHUB_ENGINE_BIN（mihomo / sing-box 路径）\n" + err.message);
  } finally {
    hideProgress();
    btn.disabled = false;
  }
});

document.getElementById("btn-unlock").addEventListener("click", async (e) => {
  const btn = e.currentTarget;
  btn.disabled = true;
  const mode = document.getElementById("test-mode").value;
  const modeText = mode === "untested" ? "未测过的节点" : mode === "failed" ? "测速失败的节点" : "全部节点";
  showProgress(`正在检测${modeText}的流媒体解锁情况（需配置 SUBHUB_ENGINE_BIN）…`);
  try {
    const r = await postJson("/api/unlock-detect", { timeout_ms: 8000, mode });
    await loadNodes();
    if (!r || r.length === 0) {
      alert(`当前范围（${modeText}）内没有可检测的节点。`);
    } else {
      const total = r.length;
      const anyUnlock = r.some((u) => u && u.services && Object.keys(u.services).length > 0);
      if (!anyUnlock) {
        alert("未检测到任何解锁信息。\n解锁检测需要先在「设置」中配置测速引擎（mihomo / sing-box 路径，即 SUBHUB_ENGINE_BIN）。未配置时该功能不生效。");
      } else {
        document.getElementById("nodes-count").textContent =
          `解锁检测完成：${total} 个节点（见节点表「解锁」列）`;
      }
    }
  } catch (err) {
    alert("解锁检测失败：请先配置 SUBHUB_ENGINE_BIN（mihomo / sing-box 路径）\n" + err.message);
  } finally {
    hideProgress();
    btn.disabled = false;
  }
});

document.getElementById("btn-cleanup").addEventListener("click", async (e) => {
  const btn = e.currentTarget;
  btn.disabled = true;
  showProgress("正在清理不可用节点…");
  try {
    const r = await postJson("/api/nodes/cleanup", {});
    await loadNodes();
    loadDashboard();
    alert(`已清理 ${r.removed} 个不可用节点`);
  } catch (err) {
    alert("清理失败: " + err.message);
  } finally {
    hideProgress();
    btn.disabled = false;
  }
});

document.getElementById("btn-refresh-all").addEventListener("click", async (e) => {
  const btn = e.currentTarget;
  btn.disabled = true;
  try {
    const list = await api("/api/subscriptions");
    const n = list.length;
    for (let i = 0; i < n; i++) {
      const s = list[i];
      showProgress(`正在刷新订阅（${i + 1}/${n}）：${s.name}`);
      await postJson("/api/subscriptions/" + s.id + "/refresh", {}).catch(() => {});
    }
    loadSubs("sub-list-manage", true);
    loadDashboard();
  } catch (err) {
    alert("刷新失败: " + err.message);
  } finally {
    hideProgress();
    btn.disabled = false;
  }
});
["f-q", "f-type", "f-region"].forEach((id) =>
  document.getElementById(id).addEventListener("input", () => {
    clearTimeout(window._t);
    window._t = setTimeout(() => {
      _page = 1;
      loadNodes();
    }, 250);
  })
);

// Subscription quick-search: filter the "当前订阅" list by name / source.
const subQ = document.getElementById("sub-q");
if (subQ) {
  subQ.addEventListener("input", () => {
    clearTimeout(window._tsub);
    window._tsub = setTimeout(() => loadSubs("sub-list-manage", true), 250);
  });
}

// ---- node list pagination ----
document.getElementById("pg-first").addEventListener("click", () => loadNodes(1));
document.getElementById("pg-prev").addEventListener("click", () =>
  loadNodes(Math.max(1, _page - 1))
);
document.getElementById("pg-next").addEventListener("click", () => loadNodes(_page + 1));
document.getElementById("pg-last").addEventListener("click", () => loadNodes(Math.max(1, Math.ceil(_total / _pageSize))));
document.getElementById("pg-size").addEventListener("change", (e) => {
  _pageSize = parseInt(e.target.value, 10) || 50;
  _page = 1;
  loadNodes();
});

// ---- local subscription URL (direct pull) ----
// Build a GET /sub URL that encodes the current format + operator transform,
// so it can be pasted straight into a proxy client's "subscription" field.
function buildShareUrl() {
  const format = document.getElementById("exp-format").value;
  const params = new URLSearchParams();
  params.set("format", format);
  let unsupported = false;
  document.querySelectorAll("#filters .filter-row").forEach((r) => {
    const f = r.querySelector(".f-field").value;
    const mode = r.querySelector(".f-mode").value;
    const m = r.querySelector(".f-match").value;
    const v = r.querySelector(".f-value").value.trim();
    if (!v) return;
    // The GET /sub URL supports include-filters only (name/region contains,
    // type exact). exclude / regex filters can't be encoded in the URL.
    if (mode !== "include") {
      unsupported = true;
      return;
    }
    if (f === "name" && m === "contains") params.append("q", v);
    else if (f === "region" && m === "contains") params.append("region", v);
    else if (f === "type" && m === "exact") params.append("type", v);
    else unsupported = true;
  });
  const sortKey = document.getElementById("op-sort-key").value;
  if (sortKey) {
    params.set("sort", sortKey);
    params.set("desc", document.getElementById("op-sort-dir").value === "true" ? "1" : "0");
  }
  const pat = document.getElementById("op-rename-pat").value.trim();
  const rep = document.getElementById("op-rename-rep").value;
  if (pat) {
    params.set("rename_pat", pat);
    params.set("rename_rep", rep);
  }
  return { url: window.location.origin + "/sub?" + params.toString(), unsupported };
}

document.getElementById("btn-gen-url").addEventListener("click", () => {
  const { url, unsupported } = buildShareUrl();
  document.getElementById("sub-url").value = url;
  document.getElementById("sub-url-info").textContent = unsupported
    ? "已生成（注意：排除 / 正则类筛选无法编码进网址，需改用「合并并导出」手动导出）"
    : "已生成。把此地址填到客户端「订阅」即可直接拉取。";
});

document.getElementById("btn-copy-url").addEventListener("click", async () => {
  const v = document.getElementById("sub-url").value;
  if (!v) return;
  try {
    await navigator.clipboard.writeText(v);
    document.getElementById("sub-url-info").textContent = "已复制到剪贴板。";
  } catch {
    document.getElementById("sub-url-info").textContent = "复制失败，请手动选择地址复制。";
  }
});

// ---- merge / export with operators ----
function ensureFilterRow() {
  const box = document.getElementById("filters");
  if (box.children.length === 0) addFilterRow();
}

function addFilterRow() {
  const box = document.getElementById("filters");
  const row = document.createElement("div");
  row.className = "filter-row";
  row.innerHTML = `
    <select class="f-field">
      <option value="name">名称</option>
      <option value="type">类型</option>
      <option value="region">地区</option>
      <option value="server">服务器</option>
    </select>
    <select class="f-mode">
      <option value="include">包含</option>
      <option value="exclude">排除</option>
    </select>
    <select class="f-match">
      <option value="contains">包含</option>
      <option value="regex">正则</option>
      <option value="exact">精确</option>
    </select>
    <input class="f-value" placeholder="值" />
    <button class="btn f-del">×</button>`;
  row.querySelector(".f-del").addEventListener("click", () => row.remove());
  box.appendChild(row);
}

document.getElementById("btn-add-filter").addEventListener("click", addFilterRow);

function buildTransform() {
  const filters = [...document.querySelectorAll("#filters .filter-row")]
    .map((r) => ({
      field: r.querySelector(".f-field").value,
      mode: r.querySelector(".f-mode").value,
      match_: r.querySelector(".f-match").value,
      value: r.querySelector(".f-value").value,
    }))
    .filter((f) => f.value.trim().length > 0);

  const sortKey = document.getElementById("op-sort-key").value;
  const sort = sortKey
    ? {
        key: sortKey,
        desc: document.getElementById("op-sort-dir").value === "true",
      }
    : null;

  const pat = document.getElementById("op-rename-pat").value.trim();
  const rep = document.getElementById("op-rename-rep").value;
  const rename = pat ? { pattern: pat, replacement: rep } : null;

  const t = { filters, sort, rename };
  if (!filters.length && !sort && !rename) return null;
  return t;
}

document.getElementById("btn-export").addEventListener("click", async () => {
  try {
    const format = document.getElementById("exp-format").value;
    const transform = buildTransform();
    // Top-N is now driven by the global setting (Settings page); both this
    // export and the standing /sub URL read it. A top_n on the URL/body would
    // still override, but the WebUI omits it so the unified config wins.
    const body = { format, transform };
    const r = await postJson("/api/export", body);
    document.getElementById("export-out").value = r.content;
    document.getElementById("export-info").textContent = `已导出 ${r.count} 个节点 (${r.format})`;
  } catch (e) {
    document.getElementById("export-out").value = "";
    document.getElementById("export-info").textContent = `导出失败: ${e.message}`;
  }
});

document.getElementById("btn-copy").addEventListener("click", async () => {
  await navigator.clipboard.writeText(document.getElementById("export-out").value);
});

document.getElementById("btn-download").addEventListener("click", () => {
  const fmt = document.getElementById("exp-format").value;
  const ext = fmt === "v2ray" || fmt === "sing-box" ? "json" : fmt === "surge" ? "conf" : "yaml";
  const blob = new Blob([document.getElementById("export-out").value], {
    type: "text/plain",
  });
  const a = document.createElement("a");
  a.href = URL.createObjectURL(blob);
  a.download = `subhub.${ext}`;
  a.click();
});

// ---- init ----
loadSettings();
checkStatus();
loadDashboard();
