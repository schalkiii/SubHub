//! Durable store backed by SQLite (Resin/BestSub-style "don't lose your
//! subscriptions on restart").
//! Design note: we keep only the **database path** in memory, not a live
//! `rusqlite::Connection`. SQLite connections are not `Send`, so holding one
//! inside the axum `AppState` (which must be `Clone + Send + Sync`) would not
//! compile. Instead we open a fresh connection per operation. At human
//! interaction frequency this is more than fast enough, and it lets the whole
//! `Subscription` (proxies + health + speed-test results) round-trip through
//! serde with zero schema drift.
//! Persistence model: each subscription is stored as a single JSON blob in a
//! `subscriptions(id, data, updated_at, len, hash)` table. On every mutation we
//! snapshot the whole set, but row by row: a row whose content fingerprint
//! (`len` + `hash`) is unchanged is left alone, so a run that touches one
//! subscription no longer rewrites megabytes of unrelated JSON. Simple and
//! correct at this scale (dozens of subs, thousands of nodes).
use rusqlite::params;

use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use subhub_core::{Proxy, Subscription};

/// 内容摘要：长度 + 64 位散列。用来判断「这一行有没有变化」，
/// 避免把整段 JSON 读回来比对。两者同时碰撞的概率可以忽略，
/// 而且最坏的失败只是「少写一次」→ 下一次内容变化时仍会被写入。
fn content_fingerprint(s: &str) -> (i64, i64) {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    (s.len() as i64, h.finish() as i64)
}

#[derive(Clone)]

pub struct Db {
    path: PathBuf,
    /// `subscriptions` 表是否带有 `len` / `hash` 摘要列。旧库在 `open` 里补齐；
    /// 补不上（只读盘等）时退回「无条件重写整行」的旧行为。
    hashed: bool,
}

impl Db {
    /// Open (creating if needed) the database. Returns `None` when SQLite
    /// can't initialise, in which case the app transparently falls back to a
    /// pure in-memory store.
    pub fn open(path: Option<PathBuf>) -> Option<Db> {
        let path = path.unwrap_or_else(|| PathBuf::from("data/subhub.db"));

        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let conn = rusqlite::Connection::open(&path).ok()?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS subscriptions (

                id TEXT PRIMARY KEY,

                data TEXT NOT NULL,

                updated_at INTEGER NOT NULL

            )",
            [],
        )
        .ok()?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS meta (

                key TEXT PRIMARY KEY,

                value TEXT NOT NULL

            )",
            [],
        )
        .ok()?;

        // best-effort WAL for concurrent readers (the API + a background task)
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        // `synchronous` is a per-connection pragma (unlike journal_mode it is
        // *not* stored in the file), so it is re-applied in `connect()`.

        // 摘要列：让 `save_rows` 能跳过内容未变的行而不必读回整段 JSON。
        // `ALTER TABLE ADD COLUMN` 在列已存在时报错，忽略即可；随后用
        // `table_info` 确认列真的在，确认不了就走旧写入路径。
        let _ = conn.execute("ALTER TABLE subscriptions ADD COLUMN len INTEGER", []);
        let _ = conn.execute("ALTER TABLE subscriptions ADD COLUMN hash INTEGER", []);
        let hashed = conn
            .prepare("PRAGMA table_info(subscriptions)")
            .and_then(|mut stmt| {
                stmt.query_map([], |r| r.get::<_, String>(1))
                    .map(|rows| rows.flatten().collect::<Vec<String>>())
            })
            .map(|cols| cols.iter().any(|c| c == "hash") && cols.iter().any(|c| c == "len"))
            .unwrap_or(false);

        Some(Db { path, hashed })
    }

    /// Fresh connection per operation (see the module note), with the two
    /// pragmas that matter for correctness under concurrency:
    /// - `busy_timeout`: `meta_set` (settings writes) opens its own connection
    ///   while a `save_rows` transaction may be committing; without a timeout
    ///   SQLite returns SQLITE_BUSY immediately and the setting is silently
    ///   lost.
    /// - `synchronous = NORMAL`: with WAL this keeps commits from fsyncing the
    ///   whole database on every write. A power loss can drop the last
    ///   transaction, but the file stays consistent — an acceptable trade for a
    ///   local subscription cache that is rewritten on every mutation.
    fn connect(&self) -> Option<rusqlite::Connection> {
        let conn = rusqlite::Connection::open(&self.path).ok()?;
        let _ = conn.pragma_update(None, "busy_timeout", 5000);
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        Some(conn)
    }

    /// Load every persisted subscription (full serde round-trip).
    pub fn load_all(&self) -> Vec<Subscription> {
        let conn = match self.connect() {
            Some(c) => c,

            None => return Vec::new(),
        };

        // Stable insertion order: without ORDER BY SQLite may return rows in
        // any order, which would reshuffle the subscription list after restart.
        let mut stmt = match conn.prepare("SELECT data FROM subscriptions ORDER BY rowid") {
            Ok(s) => s,

            Err(_) => return Vec::new(),
        };

        let rows = match stmt.query_map([], |row| row.get::<_, String>(0)) {
            Ok(r) => r,

            Err(_) => return Vec::new(),
        };

        let mut out = Vec::new();

        for r in rows.flatten() {
            match serde_json::from_str::<Subscription>(&r) {
                Ok(sub) => out.push(sub),

                Err(e) => {
                    // A single corrupt subscription (or a schema bump) must
                    // not silently wipe the whole library. Try a node-level
                    // fallback: keep only the proxies that still parse.
                    eprintln!("[db] 订阅 JSON 解析失败，尝试按节点级容错恢复: {e}");

                    let Ok(val) = serde_json::from_str::<serde_json::Value>(&r) else {
                        continue;
                    };
                    // 先把兜底要用的顶层字段和节点列表读出来（下面要移动 `val`）。
                    let meta = match &val {
                        serde_json::Value::Object(o) => {
                            let read = |k: &str| -> Option<String> {
                                o.get(k)
                                    .and_then(|v| v.as_str())
                                    .filter(|s| !s.is_empty())
                                    .map(str::to_string)
                            };
                            o.get("proxies")
                                .and_then(|x| x.as_array())
                                .cloned()
                                .map(|arr| ((read("id"), read("name"), read("source")), arr))
                        }

                        _ => None,
                    };
                    let Some(((sub_id, sub_name, sub_source), arr)) = meta else {
                        continue;
                    };

                    let mut kept: Vec<Proxy> = Vec::new();
                    let mut dropped = 0usize;
                    for item in &arr {
                        match serde_json::from_value::<Proxy>(item.clone()) {
                            Ok(p) => kept.push(p),

                            Err(_) => dropped += 1,
                        }
                    }

                    // 用过滤后的节点列表重建一份，再按完整 schema 解析一次。
                    let repaired = {
                        let mut v = val.clone();
                        if let Some(o) = v.as_object_mut() {
                            o.insert(
                                "proxies".to_string(),
                                serde_json::to_value(&kept).unwrap_or(serde_json::Value::Null),
                            );
                        }
                        serde_json::from_value::<Subscription>(v)
                    };
                    match repaired {
                        Ok(sub) => {
                            if dropped > 0 {
                                eprintln!("[db] 已恢复订阅，跳过 {dropped} 个坏节点");
                            }
                            out.push(sub);
                        }

                        // 外层结构仍然解析不了（缺 `id`/`name`/`source` 这类顶层
                        // 字段，或某个字段换了类型）：只要节点还能读出来，就用能读到的
                        // 字段兜底重建一条。旧实现只在 `dropped > 0` 时才恢复，节点全部
                        // 合法、仅 schema 变化的整条订阅会被静默丢掉。
                        Err(e2) if !kept.is_empty() => {
                            let mut sub = Subscription::new(
                                sub_name.unwrap_or_else(|| "恢复的订阅".to_string()),
                                sub_source.unwrap_or_else(|| "pasted".to_string()),
                                kept,
                            );
                            if let Some(id) = sub_id {
                                sub.id = id;
                            }
                            eprintln!(
                                "[db] 顶层字段不兼容，恢复 {} 个节点（丢弃 {dropped} 个坏节点）: {e2}",
                                sub.proxies.len()
                            );
                            out.push(sub);
                        }

                        Err(e2) => eprintln!("[db] 订阅无法恢复，已跳过: {e2}"),
                    }
                }
            }
        }

        out
    }

    /// Snapshot the store **row by row**, rewriting only the subscriptions whose
    /// serialized content actually changed.
    ///
    /// The previous model was `DELETE FROM subscriptions` + re-INSERT of every
    /// subscription on *every* mutation. A speed test persists after each batch,
    /// so one run rewrote the whole library (thousands of nodes, megabytes of
    /// JSON) dozens of times. Semantics are unchanged — after the call the table
    /// holds exactly `rows` — but the write amplification drops to the delta.
    ///
    /// `rows` arrives pre-serialized: `persist_all` serializes while it already
    /// holds the store lock instead of deep-cloning every node just to write it.
    pub fn save_rows(&self, rows: &[(String, String)]) {
        let mut conn = match self.connect() {
            Some(c) => c,

            None => return,
        };

        let tx = match conn.transaction() {
            Ok(t) => t,

            Err(_) => return,
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let mut kept: std::collections::HashSet<String> = std::collections::HashSet::new();

        // 一次性读回每行的摘要（两个整数）用于比对：跳过内容未变的行而不必把
        // 整段 JSON 拉回内存再逐字符比。
        let prior: std::collections::HashMap<String, (i64, i64)> = if self.hashed {
            match tx.prepare("SELECT id, len, hash FROM subscriptions") {
                Ok(mut stmt) => stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?),
                        ))
                    })
                    .map(|rows| rows.flatten().collect())
                    .unwrap_or_default(),

                Err(_) => Default::default(),
            }
        } else {
            Default::default()
        };

        for (id, data) in rows {
            kept.insert(id.clone());
            // 内容未变的行保持原样：一轮测速里通常只有个别订阅发生变化。
            // 摘要列不可用（只读库等异常）时退回无条件重写。
            let fp = content_fingerprint(data);
            if self.hashed && prior.get(id.as_str()).is_some_and(|prev| *prev == fp) {
                continue;
            }
            let res = if self.hashed {
                tx.execute(
                    "INSERT OR REPLACE INTO subscriptions (id, data, updated_at, len, hash) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id, data, now, fp.0, fp.1],
                )
            } else {
                tx.execute(
                    "INSERT OR REPLACE INTO subscriptions (id, data, updated_at) VALUES (?1, ?2, ?3)",
                    params![id, data, now],
                )
            };
            if let Err(e) = res {
                eprintln!("DB 持久化失败(写入订阅 {id}): {e}");
            }
        }

        // 删除已从内存 store 里消失的订阅（手动删除 / 自动清理）。
        let existing: Vec<String> = match tx.prepare("SELECT id FROM subscriptions") {
            Ok(mut stmt) => stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default(),

            Err(_) => Vec::new(),
        };
        for gone in existing.iter().filter(|id| !kept.contains(id.as_str())) {
            if let Err(e) = tx.execute("DELETE FROM subscriptions WHERE id = ?1", params![gone]) {
                eprintln!("DB 持久化失败(删除订阅 {gone}): {e}");
            }
        }

        if let Err(e) = tx.commit() {
            eprintln!("DB 持久化提交失败(订阅未保存!): {e}");
        }
    }

    /// Serialize and snapshot the whole store. Prefer [`Db::save_rows`] when the
    /// caller already holds the store lock.
    pub fn save_all(&self, subs: &[Subscription]) {
        let rows: Vec<(String, String)> = subs
            .iter()
            .filter_map(|s| match serde_json::to_string(s) {
                Ok(d) => Some((s.id.clone(), d)),

                Err(e) => {
                    eprintln!("DB 持久化跳过一个订阅(序列化失败): {}: {e}", s.id);
                    None
                }
            })
            .collect();
        self.save_rows(&rows);
    }

    /// Read a single key/value setting from the `meta` table (e.g. the global
    /// "use_proxy" master switch). Returns `None` when the key is absent or the
    /// DB is unavailable.
    pub fn meta_get(&self, key: &str) -> Option<String> {
        let conn = self.connect()?;

        let mut stmt = conn.prepare("SELECT value FROM meta WHERE key = ?1").ok()?;

        let mut rows = stmt
            .query_map(params![key], |r| r.get::<_, String>(0))
            .ok()?;

        rows.next().and_then(|r| r.ok())
    }

    /// Persist a single key/value setting into the `meta` table. Best-effort;
    /// silently no-ops when the DB is unavailable.
    pub fn meta_set(&self, key: &str, value: &str) {
        let conn = match self.connect() {
            Some(c) => c,

            None => return,
        };

        let _ = conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use subhub_core::model::{Proxy, ProxyType, Subscription};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// 每个测试用一个唯一临时 db 路径，避免相互污染；返回 (Db, path)。
    fn temp_db() -> (Db, std::path::PathBuf) {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("subhub_db_test_{}_{}.db", std::process::id(), n));
        let _ = std::fs::remove_file(&path);
        let db = Db::open(Some(path.clone())).expect("open temp db");
        (db, path)
    }

    fn cleanup(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn save_load_round_trip_preserves_nodes() {
        let (db, path) = temp_db();
        let mut sub = Subscription::new("A".into(), "https://a.example/sub".into(), vec![]);
        let mut p = Proxy::new("n1".into(), ProxyType::Ss, "1.1.1.1".into(), 8388);
        p.available = Some(true);
        sub.proxies.push(p);
        db.save_all(&[sub]);
        let loaded = db.load_all();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "A");
        assert_eq!(loaded[0].proxies.len(), 1);
        assert_eq!(loaded[0].proxies[0].server, "1.1.1.1");
        assert_eq!(loaded[0].proxies[0].available, Some(true));
        cleanup(&path);
    }

    #[test]
    fn meta_get_set_round_trip() {
        let (db, path) = temp_db();
        assert_eq!(db.meta_get("k"), None);
        db.meta_set("k", "v");
        assert_eq!(db.meta_get("k"), Some("v".into()));
        cleanup(&path);
    }

    #[test]
    fn load_all_skips_fully_corrupt_subscription() {
        let (db, path) = temp_db();
        // 用真实 Subscription 序列化得到合法 JSON，避免手写缺字段导致误判。
        let valid = serde_json::to_string(&Subscription::new(
            "ok".into(),
            "https://a.example/sub".into(),
            vec![],
        ))
        .unwrap();
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute(
                "INSERT INTO subscriptions (id, data, updated_at) VALUES (?1, ?2, ?3)",
                rusqlite::params!["bad", "not valid json{", 1],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO subscriptions (id, data, updated_at) VALUES (?1, ?2, ?3)",
                rusqlite::params!["ok", valid, 1],
            )
            .unwrap();
        }
        let loaded = db.load_all();
        // 损坏行被跳过，正常行保留（注意 load_all 解析 JSON 内的 name，而非 SQL 主键）
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "ok");
        cleanup(&path);
    }

    #[test]
    fn load_all_recovers_partial_proxy_corruption() {
        let (db, path) = temp_db();
        // 外层合法、但 proxies 数组里混有一个坏节点：节点级容错应丢弃坏节点、
        // 保留好节点，而非整条丢弃。
        let mut sub = Subscription::new("ok".into(), "pasted".into(), vec![]);
        let mut good = Proxy::new("good".into(), ProxyType::Ss, "1.1.1.1".into(), 8388);
        good.cipher = Some("aes-128-gcm".into());
        good.password = Some("x".into());
        good.available = Some(true);
        sub.proxies.push(good);
        let mut val = serde_json::to_value(&sub).unwrap();
        if let Some(arr) = val.get_mut("proxies").and_then(|p| p.as_array_mut()) {
            arr.push(serde_json::json!({"bad": true}));
        }
        let data = serde_json::to_string(&val).unwrap();
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute(
                "INSERT INTO subscriptions (id, data, updated_at) VALUES (?1, ?2, ?3)",
                rusqlite::params!["ok", data, 1],
            )
            .unwrap();
        }
        let loaded = db.load_all();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].proxies.len(), 1, "坏节点被丢弃、好节点保留");
        cleanup(&path);
    }
}

#[cfg(test)]
mod perf_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use subhub_core::model::{Proxy, ProxyType, Subscription};

    static SEQ: AtomicU64 = AtomicU64::new(1000);

    fn temp_db() -> (Db, std::path::PathBuf) {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("subhub_db_perf_{}_{}.db", std::process::id(), n));
        let _ = std::fs::remove_file(&path);
        let db = Db::open(Some(path.clone())).expect("open temp db");
        (db, path)
    }

    fn cleanup(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    fn stamp(path: &std::path::Path, id: &str) -> i64 {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.query_row(
            "SELECT updated_at FROM subscriptions WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap_or(-1)
    }

    #[test]
    fn save_rows_rewrites_only_changed_subscriptions() {
        let (db, path) = temp_db();
        let a = Subscription::new("A".into(), "https://a.example/sub".into(), vec![]);
        let b = Subscription::new("B".into(), "https://b.example/sub".into(), vec![]);
        db.save_all(&[a.clone(), b.clone()]);
        let (ta, tb) = (stamp(&path, &a.id), stamp(&path, &b.id));
        assert!(ta > 0 && tb > 0, "首轮应写入两行");

        // 内容完全不变：不应重写任何一行（旧实现是 DELETE 全表 + 重插）。
        db.save_all(&[a.clone(), b.clone()]);
        assert_eq!(
            (stamp(&path, &a.id), stamp(&path, &b.id)),
            (ta, tb),
            "未变化的订阅不应被重写"
        );

        // 只有 B 变化时，A 的时间戳保持不变。
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut b2 = b.clone();
        b2.name = "B-changed".into();
        db.save_all(&[a.clone(), b2]);
        assert_eq!(stamp(&path, &a.id), ta, "未变化的订阅被连带重写了");
        assert!(stamp(&path, &b.id) > tb, "变化的订阅应更新时间戳");
        let loaded = db.load_all();
        assert_eq!(
            loaded
                .iter()
                .find(|s| s.id == b.id)
                .map(|s| s.name.as_str()),
            Some("B-changed")
        );
        cleanup(&path);
    }

    #[test]
    fn save_rows_deletes_vanished_subscriptions() {
        // 快照语义不变：内存里没有的订阅必须从表里消失。
        let (db, path) = temp_db();
        let a = Subscription::new("A".into(), "https://a.example/sub".into(), vec![]);
        let b = Subscription::new("B".into(), "https://b.example/sub".into(), vec![]);
        db.save_all(&[a.clone(), b]);
        assert_eq!(db.load_all().len(), 2);
        db.save_all(&[a]);
        assert_eq!(db.load_all().len(), 1, "已删除的订阅必须落库");
        cleanup(&path);
    }

    #[test]
    fn load_all_recovers_subscription_with_changed_top_level_schema() {
        // 节点全部合法（dropped == 0）但顶层字段变了：旧实现只在 dropped > 0 时
        // 才恢复，整条订阅会被静默丢掉。
        let (db, path) = temp_db();
        let mut sub = Subscription::new("ok".into(), "pasted".into(), vec![]);
        sub.proxies.push(Proxy::new(
            "n1".into(),
            ProxyType::Ss,
            "1.1.1.1".into(),
            8388,
        ));
        let mut val = serde_json::to_value(&sub).unwrap();
        let obj = val.as_object_mut().unwrap();
        obj.remove("name");
        obj.remove("source");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute(
                "INSERT INTO subscriptions (id, data, updated_at) VALUES (?1, ?2, ?3)",
                params![sub.id, serde_json::to_string(&val).unwrap(), 1],
            )
            .unwrap();
        }
        let loaded = db.load_all();
        assert_eq!(loaded.len(), 1, "外层字段变化不得丢掉整条订阅");
        assert_eq!(loaded[0].proxies.len(), 1);
        assert_eq!(loaded[0].id, sub.id, "应沿用原 id 保持引用稳定");
        cleanup(&path);
    }
}
