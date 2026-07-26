import sqlite3, json, os

DB = os.path.abspath("target/release/data/subhub.db")
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
name_by_src = {u: n for n, u in SUBS}

c = sqlite3.connect(DB)
cur = c.cursor()
rows = cur.execute("SELECT id, data FROM subscriptions").fetchall()
for rid, data in rows:
    d = json.loads(data)
    t = name_by_src.get(d.get("source"))
    if t and d.get("name") != t:
        d["name"] = t
        cur.execute("UPDATE subscriptions SET data=? WHERE id=?", (json.dumps(d, ensure_ascii=False), rid))
        print(f"  patched {rid[:8]} -> {t}")
c.commit()
print("\nFinal subscriptions:")
for rid, data in cur.execute("SELECT id, data FROM subscriptions").fetchall():
    d = json.loads(data)
    print(f"  {d['name']:<14} nodes={len(d['proxies']):<4} src={d['source'][:48]}")
c.close()
