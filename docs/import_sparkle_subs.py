import json, urllib.request, sqlite3, time

BASE = "http://127.0.0.1:3007"
PROXY = "http://127.0.0.1:7890"  # Sparkle's local proxy

# (Sparkle name, url) — 8 valid external subs; skip AiO (relative path) & Mysub (self-ref)
SUBS = [
    ("Schakiii",   "https://ghfast.top/https://gist.githubusercontent.com/schalkiii/8536eac9fef13f966295dfe9a89484f1/raw/clash.yaml"),
    ("mxlsub free", "https://mxlsub.qzz.io/free"),
    ("宝可梦",       "https://link123.52pokemon66.cc/api/v1/client/subscribe?token=22521fb1bdb8a424f96fa1162c0bec11"),
    ("LD-luobosi", "https://api.wcc.best/sub?target=clash&url=vless%3A%2F%2F4a805850-a2c4-42b3-898a-798f0c19c609%4010086.cf.3666888.xyz%3A2095%3Ftype%3Dws%26path%3D%252Faba36820.xn--nxa.nyc.mn%26host%3Daba36820.xn--nxa.nyc.mn%23CF-VLESS-WARP&insert=false&emoji=true&list=false&tfo=false&scv=true&fdn=false&expand=true&sort=false&new_name=true"),
    ("BestClash",  "https://ghfast.top/https://raw.githubusercontent.com/PuddinCat/BestClash/refs/heads/main/proxies.yaml"),
    ("singbox.wwz.im", "https://singbox.wwz.im/"),
    ("BestSub",    "https://bestsub-a.bestrui.ggff.net/api/v1/share/node/yFx8UCwYLuFAEimH5b7xzGRDcAcwXaa5"),
    ("LD-risohu",  "https://ghfast.top/https://raw.githubusercontent.com/TopChina/proxy-list/refs/heads/main/clash_sub.yaml"),
]

urls = [u for _, u in SUBS]
body = json.dumps({"urls": urls, "fetch_proxy": PROXY}).encode()
req = urllib.request.Request(BASE + "/api/subscriptions", data=body,
                             headers={"Content-Type": "application/json"}, method="POST")
try:
    with urllib.request.urlopen(req, timeout=180) as r:
        resp = json.loads(r.read().decode())
    print("IMPORT RESPONSE:", json.dumps(resp, ensure_ascii=False))
except Exception as e:
    print("IMPORT ERROR:", repr(e))
    raise

# give auto-speedtest a moment, then fetch current subs
time.sleep(2)
with urllib.request.urlopen(BASE + "/api/subscriptions", timeout=30) as r:
    subs = json.loads(r.read().decode())
print(f"\nCurrent subscriptions in DB: {len(subs)}")
for s in subs:
    print(f"  id={s['id']}  name={s['name']!r}  url={s['url'][:60]!r}  nodes={len(s['proxies'])}  err={s['health'].get('last_error')}")

# patch names to Sparkle friendly names (match by url)
name_by_url = {u: n for n, u in SUBS}
patched = 0
for s in subs:
    target = name_by_url.get(s["url"])
    if target and s["name"] != target:
        s["name"] = target
        patched += 1
print(f"\nNames to patch: {patched}")

# write back via API? No — use direct sqlite update of the `data` json `name` field.
import os
db = os.path.join(os.path.dirname(__file__), "..", "target", "release", "data", "subhub.db")
db = os.path.abspath(db)
conn = sqlite3.connect(db)
cur = conn.cursor()
# refresh from db (server wrote its own snapshot); re-read and patch by url
rows = cur.execute("SELECT id, data FROM subscriptions").fetchall()
for rid, data in rows:
    d = json.loads(data)
    t = name_by_url.get(d.get("url"))
    if t and d.get("name") != t:
        d["name"] = t
        cur.execute("UPDATE subscriptions SET data=? WHERE id=?", (json.dumps(d, ensure_ascii=False), rid))
        print(f"  patched {rid} -> {t}")
conn.commit()
conn.close()
print("\nDONE. Names patched in target/release/data/subhub.db")
