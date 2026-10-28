import json
import sys
import urllib.error
import urllib.parse
import urllib.request

# Windows 控制台默认 GBK，脚本输出含中文/符号会 UnicodeEncodeError。
sys.stdout.reconfigure(encoding="utf-8", errors="replace")
sys.stderr.reconfigure(encoding="utf-8", errors="replace")

BASE = "http://127.0.0.1:3005"
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))  # bypass local proxy

# 注意：本测试会写入订阅数据。建议用独立 DB 运行以免污染默认库：
#   SUBHUB_DB=D:/tmp/sa_test2/subhub.db SUBHUB_PORT=3005 ./target/release/subhub-server.exe
# 然后再跑本脚本（BASE 默认 http://127.0.0.1:3005）。
# 导出相关断言依赖 export_filter：只有「可导出类型 + 未测或可用」的节点会出现在导出里，
# 因此测试里用于「必须被导出」的节点都指向 127.0.0.1:3005（可达 → available=True）。


def call(path, body=None, method=None):
    if method is None:
        method = "POST" if body is not None else "GET"
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        BASE + path, data=data, method=method,
        headers={"content-type": "application/json"} if data else {},
    )
    # 60s：无效代理用例会让服务端等满 reqwest 的 20s 超时后才记录 last_error。
    with OPENER.open(req, timeout=60) as r:
        ct = r.headers.get("content-type", "")
        text = r.read().decode()
        return json.loads(text) if "application/json" in ct else text


def sse(path, params):
    """GET an SSE endpoint and return (events, done_summary).

    /api/speedtest is a text/event-stream GET endpoint; the old JSON contract
    ({results, removed, threshold}) no longer exists.
    """
    qs = urllib.parse.urlencode(params)
    req = urllib.request.Request(f"{BASE}{path}?{qs}", method="GET")
    events, summary = [], None
    with OPENER.open(req, timeout=180) as r:
        for raw in r:
            line = raw.decode("utf-8", errors="replace").strip()
            if not line.startswith("data:"):
                continue
            payload = line[5:].strip()
            if not payload:
                continue
            ev = json.loads(payload)
            events.append(ev)
            if ev.get("type") == "Done":
                summary = ev
    return events, summary


CLASH = """
proxies:
  - name: "JP-VlessA"
    type: vless
    server: jp1.example.com
    port: 443
    uuid: 11111111-2222-3333-4444-555555555555
    network: ws
    ws-opts:
      path: /ws
    tls: true
  - name: "HK-VmessB"
    type: vmess
    server: hk1.example.com
    port: 443
    uuid: 99999999-8888-7777-6666-555555555555
    alterId: 0
    cipher: aes-128-gcm
    tls: true
"""

URIS = """
vmess://eyJ2IjoiMiIsInBzIjoiVVMtVm1lc3NDIiwiYWRkIjoidXMxLmV4YW1wbGUuY29tIiwicG9ydCI6IjQ0MyIsImlkIjoiYWFhYWFhYWEtYmJiYi1jY2NjLWRkZGQtZWVlZWVlZWVlZWVlIiwibmV0Ijoid3MiLCJwYXRoIjoiL3dzIn0=
trojan://pass123@kr1.example.com:443?peer=kr1.example.com#KR-TrojanD
ss://YWVzLTI1Ni1nY206cGFzcw==@sg1.example.com:8388#SG-SsE
ss://YWVzLTI1Ni1nY206cGFzcw==@hk2.example.com:8388#HK-SsA
"""


def main():
    print("== health ==")
    print(call("/api/health"))

    print("== add clash subscription ==")
    r = call("/api/subscriptions", {"urls": []})  # empty to ensure clean-ish
    # import clash block
    r = call("/api/import", {"content": CLASH})
    print("import clash ->", r)

    print("== import uris ==")
    r = call("/api/import", {"content": URIS})
    print("import uris ->", r)

    print("== auto speedtest + health on import (loopback is left untested) ==")
    # 127.0.0.1 这类回环节点会被 persist_results 刻意跳过：本地回环的延迟/吞吐是
    # 内存拷贝，不代表真实网络质量。导入后应保持「未测」，而不是假的 available=True。
    call("/api/import", {"content": "ss://YWVzLTI1Ni1nY206cGFzcw==@127.0.0.1:3005#LG-AutoTest"})
    pl = call("/api/proxies")["items"]
    auto = [p for p in pl if p["name"] == "LG-AutoTest"]
    print("  LG-AutoTest after auto-test:", auto[0] if auto else "n/a")
    assert auto, "auto-test node missing"
    assert auto[0]["available"] is None, "loopback must not be marked available"
    assert auto[0]["last_tested_at"] is None, "loopback must not get a fake test timestamp"
    print("  loopback correctly left untested on import: ✅")

    print("== add broken subscription (fast connection-refused) ==")
    call("/api/subscriptions", {"urls": ["http://127.0.0.1:9/subs"]})

    print("== list subscriptions (per-sub health) ==")
    subs = call("/api/subscriptions")
    for s in subs:
        print(f"  {s['id']} | {s['name']} | {s['source']} | {s['count']} nodes | "
              f"status={s['status']} | healthy={s['healthy']} | "
              f"checked={s['last_checked_at']} | err={s['last_error']}")
        assert "status" in s and "last_checked_at" in s and "healthy" in s
        assert "source_type" in s and "avg_latency_ms" in s

    broken = [s for s in subs if s["source"] == "http://127.0.0.1:9/subs"]
    assert broken, "broken subscription not recorded"
    assert broken[0]["status"] == "error", f"expected error status, got {broken[0]['status']}"
    assert broken[0]["last_error"], "expected last_error on broken sub"
    print("  ✅ broken sub correctly flagged error with last_error")

    print("== error messages must not leak subscription tokens ==")
    secret_url = "http://127.0.0.1:9/subs?token=SUPERSECRET"
    call("/api/subscriptions", {"urls": [secret_url]})
    secret_sub = next(
        s for s in call("/api/subscriptions") if s["source"] == secret_url
    )
    assert "SUPERSECRET" not in (secret_sub["last_error"] or ""), secret_sub
    print("  ✅ last_error is URL-redacted")

    print("== add subscription via unreachable proxy -> error ==")
    # 拉取代理只在总开关打开时生效；这里显式打开，避免受 SUBHUB_USE_PROXY=0 影响。
    call("/api/settings", {"use_proxy": True})
    try:
        call(
            "/api/subscriptions",
            {"urls": ["https://example.com/proxy-test"], "fetch_proxy": "http://127.0.0.1:1"},
        )
    finally:
        call("/api/settings", {"use_proxy": False})
    subs = call("/api/subscriptions")
    proxied = [s for s in subs if s.get("source") == "https://example.com/proxy-test"]
    assert proxied, "proxied subscription not recorded"
    assert proxied[0]["status"] == "error", f"expected error, got {proxied[0]['status']}"
    assert proxied[0]["last_error"], "expected last_error with proxy failure"
    print("  ✅ unreachable proxy correctly flagged:", proxied[0]["last_error"])

    print("== refresh a healthy subscription ==")
    healthy_sub = next(
        (s for s in call("/api/subscriptions") if s["source"] != "http://127.0.0.1:9/subs"), None
    )
    if healthy_sub:
        r = call(f"/api/subscriptions/{healthy_sub['id']}/refresh", {})
        print("  refresh ->", r)
        assert r.get("status") in ("ok", "error")

    print("== geo-detect (no engine -> empty, endpoint must not error) ==")
    r = call("/api/geo-detect", {"timeout_ms": 3000})
    print("  geo-detect returned", len(r), "entries")
    assert isinstance(r, list)

    print("== dashboard ==")
    d = call("/api/dashboard")
    print(json.dumps(d, ensure_ascii=False, indent=2)[:600])
    assert d["total"] >= 6, "dashboard total too low"
    assert "subscriptions" in d and d["subscriptions"] >= 2
    assert "available" in d and "untested" in d

    print("== list proxies (paginated) ==")
    resp = call("/api/proxies")
    pl = resp["items"]
    print(f"proxy total={resp['total']} page={resp['page']} page_size={resp['page_size']} this_page={len(pl)}")
    assert resp["total"] >= 6
    assert len(pl) >= 1 and len(pl) <= resp["page_size"]
    # every node must carry its owning subscription (sub-store grouping model)
    assert "sub_name" in pl[0], "node missing sub_name (subscription attribution)"
    assert "sub_id" in pl[0], "node missing sub_id"
    print("  ✅ nodes carry sub_name/sub_id (subscription attribution)")

    print("== pagination params ==")
    p1 = call("/api/proxies?page=1&page_size=2")
    assert p1["page"] == 1 and p1["page_size"] == 2
    assert len(p1["items"]) <= 2
    assert p1["total"] >= 6
    print("  ✅ pagination returns bounded page + total")

    print("== local subscription URL (GET /sub, direct pull) ==")
    sub_text = call("/sub")  # raw text, not JSON
    assert isinstance(sub_text, str), "/sub should return raw text"
    assert "proxies:" in sub_text, "/sub must emit clash-meta yaml"
    # export_filter drops tested-dead / other-type nodes; only usable nodes survive
    assert "LG-AutoTest" in sub_text, "/sub should contain usable merged nodes"
    assert "JP-VlessA" not in sub_text, "/sub must drop tested-dead nodes (export_filter)"
    # format override (v2ray is JSON, so call() returns a parsed list)
    sub_json = call("/sub?format=v2ray")
    assert isinstance(sub_json, list), "/sub?format=v2ray should be a JSON array"
    # export_filter keeps only usable nodes; assert the usable ss node is present
    assert len(sub_json) >= 1, "/sub?format=v2ray produced no usable nodes"
    assert any(o.get("tag") == "LG-AutoTest" for o in sub_json), "/sub?format=v2ray missing usable ss node"
    print("  ✅ GET /sub returns merged subscription text for direct client pull")

    print("== VLESS REALITY export fidelity ==")
    # 回环地址保持未测 → 不会被 export_filter 剔除，专测序列化字段。
    reality_uri = (
        "vless://11111111-2222-3333-4444-555555555555@127.0.0.1:3005"
        "?security=reality&pbk=PUBKEY&sid=ab12&fp=chrome&sni=www.example.com&type=tcp#REALITY-Test"
    )
    call("/api/import", {"content": reality_uri})
    clash_all = call("/api/export", {"format": "clash-meta"})["content"]
    assert "reality-opts" in clash_all and "public-key: PUBKEY" in clash_all
    sb_all = call("/api/export", {"format": "sing-box"})["content"]
    assert '"reality"' in sb_all and '"public_key": "PUBKEY"' in sb_all
    v2_all = call("/api/export", {"format": "v2ray"})["content"]
    assert '"realitySettings"' in v2_all and '"publicKey": "PUBKEY"' in v2_all
    print("  ✅ REALITY fields survive clash / sing-box / v2ray export")

    print("== 第七轮：格式识别与字段保真（导入侧） ==")
    # v2ray/Xray JSON：入口一直声称支持，实际解析出 0 个节点（含本项目自己的导出）。
    v2ray_doc = json.dumps(
        {
            "outbounds": [
                {
                    "tag": "R7-VMess-WS",
                    "protocol": "vmess",
                    "settings": {
                        "vnext": [
                            {
                                "address": "127.0.0.1",
                                "port": 3005,
                                "users": [{"id": "u-1", "alterId": "1", "security": "auto"}],
                            }
                        ]
                    },
                    "streamSettings": {
                        "network": "ws",
                        "security": "tls",
                        "tlsSettings": {"serverName": "sni.example"},
                        "wsSettings": {"path": "/vm", "headers": {"Host": "cdn.example"}},
                    },
                },
                {
                    "tag": "R7-VLESS-Reality",
                    "protocol": "vless",
                    "settings": {
                        "vnext": [
                            {
                                "address": "127.0.0.1",
                                "port": "3005",
                                "users": [{"id": "u-2", "flow": "xtls-rprx-vision", "encryption": "none"}],
                            }
                        ]
                    },
                    "streamSettings": {
                        "network": "tcp",
                        "security": "reality",
                        "realitySettings": {
                            "serverName": "www.r.com",
                            "publicKey": "R7PUB",
                            "shortId": "cd",
                            "fingerprint": "firefox",
                        },
                    },
                },
            ]
        }
    )
    call("/api/import", {"content": v2ray_doc, "name": "R7-v2ray"})
    got = {x["name"]: x for x in call("/api/proxies?q=R7-")["items"]}
    assert "R7-VMess-WS" in got and "R7-VLESS-Reality" in got, f"v2ray JSON 未导入: {list(got)}"
    assert got["R7-VLESS-Reality"]["reality"] is True, "realitySettings 必须标记 REALITY"
    assert got["R7-VLESS-Reality"]["reality_public_key"] == "R7PUB"
    assert got["R7-VLESS-Reality"]["flow"] == "xtls-rprx-vision", "flow 不能丢"
    v2_after = call("/api/export", {"format": "v2ray"})["content"]
    assert "R7PUB" in v2_after and "xtls-rprx-vision" in v2_after
    print("  ✅ v2ray/Xray JSON 导入 + REALITY/flow 保真（含字符串 port）")

    # clash YAML 字符串端口；sing-box `tls: {enabled: false}` 不得被强推 TLS
    call(
        "/api/import",
        {
            "content": 'proxies:\n  - name: "R7-StrPort"\n    type: ss\n    server: 127.0.0.1\n    port: "3005"\n    cipher: aes-256-gcm\n    password: x\n',
            "name": "R7-yaml",
        },
    )
    assert call("/api/proxies?q=R7-StrPort")["total"] == 1, "字符串端口被静默丢弃"
    call(
        "/api/import",
        {
            "content": json.dumps(
                {
                    "outbounds": [
                        {
                            "type": "trojan",
                            "tag": "R7-PlainTLS",
                            "server": "127.0.0.1",
                            "server_port": "3005",
                            "password": "pw",
                            "tls": {"enabled": False},
                        }
                    ]
                }
            ),
            "name": "R7-singbox",
        },
    )
    assert call("/api/proxies?q=R7-PlainTLS")["items"], "sing-box 字符串 server_port 未解析"
    yaml_txt = call("/api/export", {"format": "clash-meta"})["content"]
    assert "R7-StrPort" in yaml_txt, "字符串端口节点没进导出"
    print("  ✅ 字符串端口（clash / sing-box）与 tls.enabled=false 都被正确处理")

    # URI query 键大小写不敏感 + userinfo 百分号解码
    call(
        "/api/import",
        {
            "content": "trojan://p%40ss%3A2@127.0.0.1:3005?Type=ws&SNI=sni.example&Path=%2Ftj#R7-PctAuth",
            "name": "R7-uri",
        },
    )
    uri = call("/api/proxies?q=R7-PctAuth")["items"]
    assert uri and uri[0]["password"] == "p@ss:2", f"userinfo 未解码: {uri and uri[0]['password']}"
    assert "sni: sni.example" in yaml_txt or "sni: sni.example" in call(
        "/api/export", {"format": "clash-meta"}
    )["content"], "大写 SNI= 未被识别"
    print("  ✅ query 键大小写不敏感 + userinfo 百分号解码")

    # WireGuard 缺必填键：节点仍可见，但不得进 clash 导出（否则整份配置被 mihomo 拒载）
    call(
        "/api/import",
        {
            "content": 'proxies:\n  - name: "R7-WG"\n    type: wireguard\n    server: 1.2.3.4\n    port: 51820\n    public-key: k\n',
            "name": "R7-wg",
        },
    )
    assert call("/api/proxies?q=R7-WG")["total"] == 1, "WG 节点应仍被解析出来"
    assert "R7-WG" not in call("/api/export", {"format": "clash-meta"})["content"], (
        "缺 private-key 的 wireguard 条目会拖垮整份 clash 配置，必须被导出侧过滤"
    )

    # 粘贴内容解析为 0 节点 = 失败：返回 400，且不留下空订阅
    before_subs = len(call("/api/subscriptions"))
    try:
        call("/api/import", {"content": "<html><body>403 Forbidden</body></html>", "name": "R7-junk"})
        raise AssertionError("粘贴垃圾内容应返回 400")
    except urllib.error.HTTPError as e:
        assert e.code == 400, e.code
    assert len(call("/api/subscriptions")) == before_subs, "失败的导入不应留下空订阅"
    print("  ✅ WG 不进导出；垃圾粘贴返回 400 且不落空订阅")

    print("== SS plugin / Hysteria2 obfs export fidelity ==")
    call(
        "/api/import",
        {
            "content": "ss://YWVzLTI1Ni1nY206cGFzcw==@127.0.0.1:3005?plugin=obfs-local%3Bobfs%3Dhttp%3Bobfs-host%3Dexample.com#SS-PLUGIN"
        },
    )
    call(
        "/api/import",
        {
            "content": "hysteria2://pw@127.0.0.1:3005?obfs=salamander&obfs-password=secret#HY-OBFS"
        },
    )
    clash_all = call("/api/export", {"format": "clash-meta"})["content"]
    assert "plugin: obfs" in clash_all, "clash 缺少 SS 插件"
    assert "obfs: salamander" in clash_all, "clash 缺少 HY2 obfs"
    sb_all = call("/api/export", {"format": "sing-box"})["content"]
    assert '"plugin_opts"' in sb_all and "obfs-local" in sb_all
    assert '"salamander"' in sb_all
    v2_all = call("/api/export", {"format": "v2ray"})["content"]
    assert "SS-PLUGIN" not in v2_all, "v2ray 不应导出 SS 插件节点"
    assert "HY-OBFS" not in v2_all, "v2ray 不应导出 hysteria2 节点"
    print("  ✅ SS plugin / HY2 obfs survive clash/sing-box and are skipped by v2ray")

    print("== Surge type/transport boundaries ==")
    # Surge 的 [Proxy] 关键字表里没有 vless，传输层只有 TCP / WebSocket；
    # hysteria2 的 salamander 混淆用单键 salamander-password=。
    sg_all = call("/api/export", {"format": "surge"})["content"]
    assert "REALITY-Test" not in sg_all, "surge 不能导出 vless 节点（Surge 无该协议）"
    assert " = vless," not in sg_all, "surge 输出中出现 vless 行"
    assert "network=" not in sg_all, "surge 输出不应带非 Surge 关键字 network="
    hy_lines = [l for l in sg_all.splitlines() if "HY-OBFS" in l]
    assert hy_lines and "hysteria2" in hy_lines[0], f"surge 缺少 hysteria2 行: {hy_lines}"
    assert "salamander-password=secret" in hy_lines[0], hy_lines[0]
    ss_lines = [l for l in sg_all.splitlines() if "SS-PLUGIN" in l]
    assert ss_lines and "obfs=http" in ss_lines[0], f"surge 缺少 SS obfs: {ss_lines}"
    print("  ✅ Surge skips vless, has no network= key, exports hysteria2/SS obfs")

    print("== export filters out invalid (other-type) nodes (Round E/F regression) ==")
    # An unrecognized proxy type is parsed as ProxyType::Other and MUST NOT be exported,
    # otherwise mihomo/clash reject the whole subscription (Sparkle: 'unsupport proxy type: other').
    BAD = """
proxies:
  - name: "BAD-OtherNode"
    type: other
    server: somewhere.example.com
    port: 443
"""
    call("/api/import", {"content": BAD})
    allp = call("/api/proxies")["items"]
    bad = [p for p in allp if p["name"] == "BAD-OtherNode"]
    assert bad, "BAD-OtherNode not parsed/imported"
    assert bad[0]["type_"] == "other", f"expected type_=other, got {bad[0]['type_']}"
    sub_text = call("/sub")
    assert "BAD-OtherNode" not in sub_text, "/sub leaked other-type node (Sparkle 'unsupport proxy type: other')"
    r2 = call("/api/export", {"format": "clash-meta"})
    assert "BAD-OtherNode" not in r2["content"], "/api/export leaked other-type node"
    print("  ✅ other-type node typed 'other' and excluded from /sub + /api/export")

    print("== proxies carry region field (injected, not a Rust method) ==")
    # Regression: renderNodes reads p.region which was never serialized -> always 'OTHER'.
    # list_proxies now injects region explicitly; verify it is present and non-empty.
    resp = call("/api/proxies?q=HK-VmessB")
    items = resp["items"]
    assert items, "HK-VmessB not found"
    assert "region" in items[0], "node missing region field (injection bug regression)"
    print("  HK-VmessB region =", items[0]["region"])
    print("  ✅ region field injected into /api/proxies")

    print("== export formats (export_filter only keeps usable nodes) ==")
    for fmt in ["clash-meta", "clash", "v2ray", "sing-box", "surge", "base64"]:
        r = call("/api/export", {"format": fmt})
        print(f"  {r['format']}: {r['count']} nodes, {len(r['content'])} bytes")
        assert r["count"] >= 1, f"export produced no usable nodes for {fmt}"
        if fmt == "v2ray":
            assert '"outbounds"' not in r["content"]  # v2ray array form
            assert "ss" in r["content"] or "LG-AutoTest" in r["content"]
        if fmt == "sing-box":
            assert '"outbounds"' in r["content"]
        if fmt == "surge":
            assert " = " in r["content"]

    print("== transform: exclude HK + rename US -> UNITED (on usable nodes) ==")
    # add REACHABLE nodes (point at the local server) so they survive export_filter,
    # and name them so region() derives HK / US from the node name.
    call("/api/import", {"content": "ss://YWVzLTI1Ni1nY206cGFzcw==@127.0.0.1:3005#US-Test"})
    call("/api/import", {"content": "ss://YWVzLTI1Ni1nY206cGFzcw==@127.0.0.1:3005#HK-FilterTest"})
    r = call("/api/export", {
        "format": "clash-meta",
        "transform": {
            "filters": [{"field": "region", "mode": "exclude", "match_": "exact", "value": "HK"}],
            "sort": {"key": "name", "desc": False},
            "rename": {"pattern": "US-(.*)", "replacement": "UNITED-$1"},
        },
    })
    print("  transformed count:", r["count"])
    assert "UNITED-Test" in r["content"], "rename failed (US-Test -> UNITED-Test)"
    assert "HK-FilterTest" not in r["content"], "exclude HK (reachable) failed"
    assert "HK-VmessB" not in r["content"], "exclude HK (dead) failed"
    assert "HK-SsA" not in r["content"], "exclude HK (uri, dead) failed"

    print("== speedtest (local test node, SSE) ==")
    call("/api/import", {"content": "ss://YWVzLTI1Ni1nY206cGFzcw==@127.0.0.1:3005#LG-LocalTest"})
    events, summary = sse("/api/speedtest", {"timeout_ms": 3000, "concurrency": 10})
    assert summary is not None, "SSE stream must end with a Done event"
    assert "tested" in summary and "removed" in summary and "threshold" in summary
    progress = [e for e in events if e.get("type") == "Progress"]
    assert progress, "speedtest must emit Progress events"
    local = [e for e in progress if e.get("name") == "LG-LocalTest"]
    print("  LG-LocalTest:", local[-1] if local else "n/a")
    if local:
        assert local[-1]["available"] is True, "local should be reachable"
        assert local[-1].get("tcp_latency_ms") is not None

    print("== speedtest mode=untested (scoped SSE run) ==")
    events2, summary2 = sse(
        "/api/speedtest", {"timeout_ms": 2000, "concurrency": 10, "mode": "untested"}
    )
    assert summary2 is not None, "scoped run must end with a Done event"
    scoped = [e for e in events2 if e.get("type") == "Progress"]
    print(f"  mode=untested tested {summary2.get('tested', 0)} node(s), {len(scoped)} progress ticks")

    print("== global sort is cross-page (not per-page) ==")
    # The full ordering (one big page) must equal the concatenation of small
    # pages — i.e. sorting happens BEFORE pagination, server-side.
    full = call("/api/proxies?sort=latency&desc=false&page=1&page_size=1000")["items"]
    names_full = [p["name"] for p in full]
    names_paged, page = [], 1
    while True:
        chunk = call(f"/api/proxies?sort=latency&desc=false&page={page}&page_size=3")["items"]
        if not chunk:
            break
        names_paged.extend(p["name"] for p in chunk)
        page += 1
    assert names_paged == names_full, f"paged concat != full order\n paged={names_paged}\n full={names_full}"
    print(f"  ✅ {len(names_full)} nodes: page_size=3 concatenation matches global order")

    print("== settings persistence (top_n round-trip) ==")
    call("/api/settings", {"top_n": 7})
    s = call("/api/settings")
    assert s["top_n"] == 7, f"top_n not persisted, got {s['top_n']}"
    call("/api/settings", {"top_n": 0})  # restore
    assert call("/api/settings")["top_n"] == 0
    print("  ✅ top_n saved and restored via /api/settings")

    print("== scheduler intervals clamp to >= 60s ==")
    call("/api/settings", {"auto_refresh_sec": 1, "node_health_check_sec": 1})
    s = call("/api/settings")
    assert s["auto_refresh_sec"] >= 60, s
    assert s["node_health_check_sec"] >= 60, s
    call("/api/settings", {"auto_refresh_sec": 0, "node_health_check_sec": 0})
    print("  ✅ 1s intervals are clamped (0 still disables)")

    print("== gist upload goes through the pull proxy ==")
    call(
        "/api/settings",
        {
            "use_proxy": True,
            "default_fetch_proxy": "http://127.0.0.1:1",
            "github_user": "fake-user",
            "github_token": "fake-token",
        },
    )
    try:
        call("/api/gist/upload", {})
        raise AssertionError("gist upload through an unreachable proxy must fail")
    except urllib.error.HTTPError as e:
        body = e.read().decode()
        assert e.code == 502, (e.code, body)
        assert "GitHub 请求失败" in body, body
        assert "fake-token" not in body, body
    finally:
        call(
            "/api/settings",
            {
                "use_proxy": False,
                "default_fetch_proxy": "",
                "github_user": "",
                "github_token": "",
            },
        )
    print("  ✅ gist upload routed through proxy and failed cleanly")

    print("== delete / refresh non-existent subscription -> 404 ==")
    try:
        call("/api/subscriptions/does-not-exist", method="DELETE")
        raise AssertionError("deleting a bogus id must return HTTP 404")
    except urllib.error.HTTPError as e:
        assert e.code == 404, f"expected 404, got {e.code}"
    try:
        call("/api/subscriptions/does-not-exist/refresh", {})
        raise AssertionError("refreshing a bogus id must return HTTP 404")
    except urllib.error.HTTPError as e:
        assert e.code == 404, f"expected 404, got {e.code}"
    print("  ✅ bogus delete / refresh correctly rejected with 404")

    print("== import idempotency (same URL merges, no duplicate sub) ==")
    # importing the same remote source twice must not create two subscriptions
    before_ids = {s["id"] for s in call("/api/subscriptions")}
    call("/api/subscriptions", {"urls": ["http://127.0.0.1:9/subs"]})  # same broken URL as earlier
    after = call("/api/subscriptions")
    same_src = [s for s in after if s["source"] == "http://127.0.0.1:9/subs"]
    assert len(same_src) == 1, f"same source URL must merge into one sub, got {len(same_src)}"
    print(f"  ✅ re-adding same URL kept a single subscription (subs {len(before_ids)} -> {len(after)})")

    print("== local backup restore is idempotent via uid ==")
    doc = call("/api/subscriptions/export")
    assert doc["version"] == 3, doc
    local = next(
        s for s in doc["subscriptions"] if s["source"] == "pasted" and s["proxies"]
    )
    local = dict(local)
    local["name"] = "LOCAL-RESTORE"
    local["uid"] = "u_test_fixed"
    before = len(call("/api/subscriptions"))
    call("/api/subscriptions/import", {"subscriptions": [local]})
    after_first = len(call("/api/subscriptions"))
    call("/api/subscriptions/import", {"subscriptions": [local]})
    after_second = len(call("/api/subscriptions"))
    assert after_first == before + 1, (before, after_first)
    assert after_second == after_first, f"second restore duplicated: {after_first} -> {after_second}"
    restored = [
        s for s in call("/api/subscriptions") if s["name"] == "LOCAL-RESTORE"
    ]
    assert len(restored) == 1, restored
    doc2 = call("/api/subscriptions/export")
    kept = next(s for s in doc2["subscriptions"] if s["name"] == "LOCAL-RESTORE")
    assert kept["uid"] == "u_test_fixed", kept
    print("  ✅ repeated local restore updates in place and keeps uid")

    print("== same remote URL twice inside ONE import batch stays one sub ==")
    # by_url 原先只在批次开始前建索引，同批第二条会 miss 并再追加一条重复订阅。
    dup = {
        "id": "sub_900",
        "name": "DUP-REMOTE",
        "source": "http://127.0.0.1:9/dup-batch",
        "source_type": "remote",
        "fetch_proxy": None,
        "proxies": [],
    }
    before = len(call("/api/subscriptions"))
    r = call("/api/subscriptions/import", {"subscriptions": [dup, dict(dup)]})
    dups = [s for s in call("/api/subscriptions") if s["source"] == dup["source"]]
    assert len(dups) == 1, f"同批重复 URL 追加了 {len(dups)} 条订阅"
    assert r["added"] == 1 and r["replaced"] == 1, r
    assert len(call("/api/subscriptions")) == before + 1, (before, r)
    call(f"/api/subscriptions/{dups[0]['id']}", method="DELETE")
    print("  ✅ second entry in the same batch updates instead of duplicating")

    print("== unlock-detect (no engine -> list, must not error) ==")
    r = call("/api/unlock-detect", {"timeout_ms": 3000})
    print("  unlock-detect returned", len(r), "entries")
    assert isinstance(r, list)

    print("== trends ==")
    r = call("/api/trends")
    print("  trends points:", len(r))
    assert isinstance(r, list)
    if r:
        p = r[0]
        assert "total" in p and "available" in p and "t" in p
        print("  first trend point:", p)

    print("== cleanup bad nodes (circuit-breaker) ==")
    r = call("/api/nodes/cleanup", {})
    print("  cleanup ->", r)
    assert "removed" in r and "status" in r

    print("== delete a subscription ==")
    before = len(call("/api/subscriptions"))
    target = call("/api/subscriptions")[0]["id"]
    call(f"/api/subscriptions/{target}", method="DELETE")
    after = len(call("/api/subscriptions"))
    print(f"  subs {before} -> {after}")
    assert after == before - 1

    print("== index.html ==")
    html = OPENER.open(BASE + "/", timeout=10).read().decode()
    print("  bytes:", len(html), "| has SubHub:", "SubHub" in html,
          "| has 仪表盘:", "仪表盘" in html)

    print("\nALL CHECKS PASSED ✅")


if __name__ == "__main__":
    main()
