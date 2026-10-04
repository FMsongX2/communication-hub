use crate::{
    config::{Config, json_file, private_dir},
    event::{Conversation, Event, Plan, now},
};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Clone)]
pub struct Store {
    pub path: PathBuf,
}
impl Store {
    pub fn open(state: PathBuf) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        private_dir(&state)?;
        let s = Self {
            path: state.join("hub.sqlite3"),
        };
        let db = s.db()?;
        db.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS sessions(conversation TEXT PRIMARY KEY,thread TEXT NOT NULL UNIQUE);
            CREATE TABLE IF NOT EXISTS routes(conversation TEXT PRIMARY KEY,title TEXT,introduced INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS events(key TEXT PRIMARY KEY,payload TEXT NOT NULL,status TEXT NOT NULL,reason TEXT,created REAL NOT NULL,updated REAL NOT NULL);
            CREATE TABLE IF NOT EXISTS deliveries(key TEXT PRIMARY KEY,event_key TEXT NOT NULL,phase TEXT NOT NULL,plan TEXT NOT NULL,status TEXT NOT NULL,receipt TEXT);
            CREATE TABLE IF NOT EXISTS legacy_events(scope TEXT,key TEXT,PRIMARY KEY(scope,key));
            CREATE TABLE IF NOT EXISTS migrations(name TEXT PRIMARY KEY);")?;
        std::fs::set_permissions(&s.path, std::fs::Permissions::from_mode(0o600))?;
        Ok(s)
    }
    pub fn db(&self) -> Result<Connection> {
        let c = Connection::open(&self.path)?;
        c.busy_timeout(std::time::Duration::from_secs(2))?;
        Ok(c)
    }
    pub fn session(&self, c: &Conversation) -> Result<Option<String>> {
        Ok(self
            .db()?
            .query_row(
                "SELECT thread FROM sessions WHERE conversation=?",
                [c.key()],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub fn save_session(&self, c: &Conversation, thread: &str) -> Result<()> {
        self.db()?
            .execute("INSERT INTO sessions VALUES(?,?)", params![c.key(), thread])?;
        Ok(())
    }
    pub fn route(&self, c: &Conversation) -> Result<Option<String>> {
        Ok(self
            .db()?
            .query_row(
                "SELECT title FROM routes WHERE conversation=? AND title IS NOT NULL",
                [c.key()],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub fn save_route(&self, c: &Conversation, title: &str) -> Result<()> {
        self.db()?.execute("INSERT INTO routes(conversation,title) VALUES(?,?) ON CONFLICT(conversation) DO UPDATE SET title=excluded.title",params![c.key(),title])?;
        Ok(())
    }
    pub fn introduced(&self, c: &Conversation) -> Result<bool> {
        Ok(self
            .db()?
            .query_row(
                "SELECT introduced FROM routes WHERE conversation=?",
                [c.key()],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
            != 0)
    }
    pub fn note_intro(&self, c: &Conversation, reply: &str, receipt: &Value) -> Result<()> {
        if receipt["status"] == "sent_verified" && reply.contains("Codex[유이]") {
            self.db()?.execute("INSERT INTO routes(conversation,introduced) VALUES(?,1) ON CONFLICT(conversation) DO UPDATE SET introduced=1",[c.key()])?;
        }
        Ok(())
    }
    pub fn enqueue(&self, e: &Event) -> Result<bool> {
        let db = self.db()?;
        let scope =
            serde_json::to_string(&json!([e.conversation.provider, e.conversation.account]))?;
        if db
            .query_row(
                "SELECT 1 FROM legacy_events WHERE scope=? AND key=?",
                params![scope, e.legacy_key()],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .is_some()
        {
            return Ok(false);
        }
        Ok(db.execute(
            "INSERT OR IGNORE INTO events VALUES(?,?,'pending',NULL,?,?)",
            params![e.key(), serde_json::to_string(e)?, now(), now()],
        )? == 1)
    }
    pub fn claim_next(&self) -> Result<Option<Event>> {
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT key,payload FROM events WHERE status='pending' ORDER BY created LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((key, payload)) = row {
            tx.execute(
                "UPDATE events SET status='dispatching',updated=? WHERE key=?",
                params![now(), key],
            )?;
            tx.commit()?;
            Ok(Some(serde_json::from_str(&payload)?))
        } else {
            Ok(None)
        }
    }
    pub fn finish_event(&self, key: &str, status: &str, reason: &str) -> Result<()> {
        self.db()?.execute(
            "UPDATE events SET status=?,reason=?,payload='',updated=? WHERE key=?",
            params![status, reason, now(), key],
        )?;
        Ok(())
    }
    pub fn recover(&self) -> Result<()> {
        self.db()?.execute("UPDATE events SET status='ambiguous',reason='worker_restart_during_dispatch',payload='',updated=? WHERE status='dispatching'",[now()])?;
        self.db()?.execute(
            "UPDATE deliveries SET status='sending_uncertain' WHERE status='sending'",
            [],
        )?;
        Ok(())
    }
    pub fn prepare(&self, key: &str, e: &Event, phase: &str, p: &Plan) -> Result<bool> {
        Ok(self.db()?.execute(
            "INSERT OR IGNORE INTO deliveries VALUES(?,?,?,?,'prepared',NULL)",
            params![key, e.key(), phase, serde_json::to_string(p)?],
        )? == 1)
    }
    pub fn claim_delivery(&self, key: &str) -> Result<bool> {
        Ok(self.db()?.execute(
            "UPDATE deliveries SET status='sending' WHERE key=? AND status='prepared'",
            [key],
        )? == 1)
    }
    pub fn complete_delivery(&self, key: &str, r: &Value) -> Result<()> {
        self.db()?.execute(
            "UPDATE deliveries SET status=?,receipt=? WHERE key=?",
            params![
                r["status"].as_str().unwrap_or("sending_uncertain"),
                serde_json::to_string(r)?,
                key
            ],
        )?;
        Ok(())
    }
    pub fn status(&self) -> Result<Value> {
        let db = self.db()?;
        let mut q = db.prepare("SELECT status,count(*) FROM events GROUP BY status")?;
        let events = q
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut q = db.prepare("SELECT status,count(*) FROM deliveries GROUP BY status")?;
        let deliveries = q
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(
            json!({"events":events,"deliveries":deliveries,"sessions":db.query_row("SELECT count(*) FROM sessions",[],|r|r.get::<_,i64>(0))?}),
        )
    }
    pub fn import_legacy(&self, cfg: &Config) -> Result<()> {
        if !cfg.kakao.enabled {
            return Ok(());
        }
        let mut db = self.db()?;
        let tx = db.transaction()?;
        if tx
            .query_row("SELECT 1 FROM migrations WHERE name='kakao-v1'", [], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
            .is_some()
        {
            return Ok(());
        }
        let base = &cfg.kakao.legacy_state;
        let conv = |id: String| Conversation {
            provider: "kakao".into(),
            account: cfg.kakao.account.clone(),
            id,
        };
        let p = base.join("room-sessions.sqlite3");
        if p.exists() {
            let old = Connection::open_with_flags(p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            for row in old
                .prepare("SELECT room_id,thread_id FROM rooms")?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            {
                let (id, t) = row?;
                tx.execute(
                    "INSERT OR IGNORE INTO sessions VALUES(?,?)",
                    params![conv(id).key(), t],
                )?;
            }
        }
        let p = base.join("queue.sqlite3");
        if p.exists() {
            let old = Connection::open_with_flags(p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            for row in old
                .prepare("SELECT key,status FROM events")?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            {
                let (key, status) = row?;
                if matches!(status.as_str(), "pending" | "dispatching") {
                    anyhow::bail!("legacy_worker_has_pending_work")
                }
                let scope = serde_json::to_string(&json!(["kakao", cfg.kakao.account]))?;
                tx.execute(
                    "INSERT OR IGNORE INTO legacy_events VALUES(?,?)",
                    params![scope, key],
                )?;
            }
        }
        if let Some(routes) = json_file(&base.join("room-routes.json"))?.as_object() {
            for (id, title) in routes {
                tx.execute(
                    "INSERT OR IGNORE INTO routes(conversation,title) VALUES(?,?)",
                    params![conv(id.clone()).key(), title.as_str()],
                )?;
            }
        }
        if let Some(intros) = json_file(&base.join("room-introductions.json"))?.as_object() {
            for id in intros.keys() {
                tx.execute("INSERT INTO routes(conversation,introduced) VALUES(?,1) ON CONFLICT(conversation) DO UPDATE SET introduced=1",[conv(id.clone()).key()])?;
            }
        }
        tx.execute("INSERT INTO migrations VALUES('kakao-v1')", [])?;
        tx.commit()?;
        Ok(())
    }
}
