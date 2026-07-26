import json
import urllib.request

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
    with OPENER.open(req, timeout=10) as r:
        ct = r.headers.get("content-type", "")
        text = r.read().decode()
        return json.loads(text) if "application/json" in ct else text


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

    print("== auto speedtest + health on import ==")
    call("/api/import", {"content": "ss://YWVzLTI1Ni1nY206cGFzcw==@127.0.0.1:3005#LG-AutoTest"})
    pl = call("/api/proxies")["items"]
    auto = [p for p in pl if p["name"] == "LG-AutoTest"]
    print("  LG-AutoTest after auto-test:", auto[0] if auto else "n/a")
    assert auto, "auto-test node missing"
    assert auto[0]["available"] is True, "auto speedtest should mark local node available"
    assert auto[0]["latency_ms"] is not None, "auto speedtest should set latency"
    print("  auto health+speedtest on import: ✅")

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

    print("== add subscription via invalid proxy -> error ==")
    call("/api/subscriptions", {"urls": ["https://example.com/proxy-test"], "fetch_proxy": "not a valid proxy"})
    subs = call("/api/subscriptions")
    proxied = [s for s in subs if s.get("source") == "https://example.com/proxy-test"]
    assert proxied, "proxied subscription not recorded"
    assert proxied[0]["status"] == "error", f"expected error, got {proxied[0]['status']}"
    assert proxied[0]["last_error"], "expected last_error with proxy failure"
    print("  ✅ invalid proxy correctly flagged:", proxied[0]["last_error"])

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

    print("== speedtest (local test node) ==")
    call("/api/import", {"content": "ss://YWVzLTI1Ni1nY206cGFzcw==@127.0.0.1:3005#LG-LocalTest"})
    # /api/speedtest returns an OBJECT: {results: [...], removed: n, threshold: n}
    resp = call("/api/speedtest", {"timeout_ms": 3000, "concurrency": 10})
    assert isinstance(resp, dict) and "results" in resp, f"speedtest must return an object with results, got {type(resp)}"
    assert "removed" in resp and "threshold" in resp, "speedtest response must carry removed/threshold"
    res = resp["results"]
    local = [x for x in res if x["name"] == "LG-LocalTest"]
    print("  LG-LocalTest:", local[0] if local else "n/a")
    if local:
        assert local[0]["available"] is True, "local should be reachable"
        assert local[0]["tcp_latency_ms"] is not None

    print("== speedtest mode=untested (scoped run, object shape) ==")
    r = call("/api/speedtest", {"timeout_ms": 2000, "concurrency": 10, "mode": "untested"})
    assert isinstance(r, dict) and isinstance(r.get("results"), list)
    print(f"  mode=untested tested {len(r['results'])} node(s) (all-tested -> 0 is fine)")

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

    print("== delete non-existent subscription -> 404 ==")
    import urllib.error
    try:
        call("/api/subscriptions/does-not-exist", method="DELETE")
        raise AssertionError("deleting a bogus id must return HTTP 404")
    except urllib.error.HTTPError as e:
        assert e.code == 404, f"expected 404, got {e.code}"
    print("  ✅ bogus delete correctly rejected with 404")

    print("== import idempotency (same URL merges, no duplicate sub) ==")
    # importing the same remote source twice must not create two subscriptions
    before_ids = {s["id"] for s in call("/api/subscriptions")}
    call("/api/subscriptions", {"urls": ["http://127.0.0.1:9/subs"]})  # same broken URL as earlier
    after = call("/api/subscriptions")
    same_src = [s for s in after if s["source"] == "http://127.0.0.1:9/subs"]
    assert len(same_src) == 1, f"same source URL must merge into one sub, got {len(same_src)}"
    print(f"  ✅ re-adding same URL kept a single subscription (subs {len(before_ids)} -> {len(after)})")

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
