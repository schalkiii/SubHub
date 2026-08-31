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
//! `subscriptions(id, data, updated_at)` table. On every mutation we rewrite
//! the whole set —simple and correct at this scale (dozens of subs, thousands
//! of nodes).
use rusqlite::params;

use std::path::PathBuf;

use subhub_core::{Proxy, Subscription};

#[derive(Clone)]

pub struct Db {
    path: PathBuf,
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

        Some(Db { path })
    }

    fn connect(&self) -> Option<rusqlite::Connection> {
        rusqlite::Connection::open(&self.path).ok()
    }

    /// Load every persisted subscription (full serde round-trip).
    pub fn load_all(&self) -> Vec<Subscription> {
        let conn = match self.connect() {
            Some(c) => c,

            None => return Vec::new(),
        };

        let mut stmt = match conn.prepare("SELECT data FROM subscriptions") {
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

                    if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&r) {
                        if let Some(obj) = val.as_object_mut() {
                            if let Some(arr) = obj.get("proxies").and_then(|x| x.as_array()) {
                                let mut kept: Vec<Proxy> = Vec::new();

                                let mut dropped = 0usize;

                                for item in arr {
                                    match serde_json::from_value::<Proxy>(item.clone()) {
                                        Ok(p) => kept.push(p),

                                        Err(_) => dropped += 1,
                                    }
                                }

                                if dropped > 0 {
                                    obj.insert(
                                        "proxies".to_string(),
                                        serde_json::to_value(&kept)
                                            .unwrap_or(serde_json::Value::Null),
                                    );

                                    match serde_json::from_value::<Subscription>(val) {
                                        Ok(sub) => {
                                            out.push(sub);

                                            eprintln!("[db] 已恢复订阅，跳过 {dropped} 个坏节点");
                                        }

                                        Err(e2) => eprintln!("[db] 订阅无法恢复，已跳过: {e2}"),
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        out
    }

    /// Replace the entire subscription set. Subscriptions already carry their
    /// proxies, health and speed-test results, so a full snapshot is the
    /// simplest correct persistence model at this scale.
    pub fn save_all(&self, subs: &[Subscription]) {
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

        if let Err(e) = tx.execute("DELETE FROM subscriptions", []) {
            eprintln!("DB 持久化失败(清空旧订阅): {e}");
        }

        for s in subs {
            let data = match serde_json::to_string(s) {
                Ok(d) => d,

                Err(e) => {
                    eprintln!("DB 持久化跳过一个订阅(序列化失败): {}: {e}", s.id);
                    continue;
                }
            };

            if let Err(e) = tx.execute(
                "INSERT OR REPLACE INTO subscriptions (id, data, updated_at) VALUES (?1, ?2, ?3)",
                params![s.id, data, now],
            ) {
                eprintln!("DB 持久化失败(写入订阅 {}): {e}", s.id);
            }
        }

        if let Err(e) = tx.commit() {
            eprintln!("DB 持久化提交失败(订阅未保存!): {e}");
        }
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
