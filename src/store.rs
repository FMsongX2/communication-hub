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
    pub store_bodies: bool,
}
impl Store {
    pub fn open(state: PathBuf) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        private_dir(&state)?;
        let s = Self {
            path: state.join("hub.sqlite3"),
            store_bodies: false,
        };
        let db = s.db()?;
        db.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS sessions(conversation TEXT PRIMARY KEY,thread TEXT NOT NULL UNIQUE);
            CREATE TABLE IF NOT EXISTS routes(conversation TEXT PRIMARY KEY,title TEXT,introduced INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS events(key TEXT PRIMARY KEY,payload TEXT NOT NULL,status TEXT NOT NULL,reason TEXT,created REAL NOT NULL,updated REAL NOT NULL);
            CREATE TABLE IF NOT EXISTS deliveries(key TEXT PRIMARY KEY,event_key TEXT NOT NULL,phase TEXT NOT NULL,plan TEXT NOT NULL,status TEXT NOT NULL,receipt TEXT);
            CREATE TABLE IF NOT EXISTS legacy_events(scope TEXT,key TEXT,PRIMARY KEY(scope,key));
            CREATE TABLE IF NOT EXISTS migrations(name TEXT PRIMARY KEY);")?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS call_log(event_key TEXT PRIMARY KEY,conversation TEXT NOT NULL,message_id TEXT NOT NULL,occurred REAL NOT NULL,trigger_kind TEXT NOT NULL,notification_title TEXT NOT NULL,body TEXT);
            CREATE TABLE IF NOT EXISTS session_policy(conversation TEXT PRIMARY KEY,hash TEXT NOT NULL,applied REAL NOT NULL);
            CREATE TABLE IF NOT EXISTS expression_history(delivery TEXT PRIMARY KEY,conversation TEXT NOT NULL,sha256 TEXT NOT NULL,family TEXT NOT NULL,created REAL NOT NULL,verified INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX IF NOT EXISTS expression_room_time ON expression_history(conversation,created DESC);
            CREATE INDEX IF NOT EXISTS call_log_conversation ON call_log(conversation);")?;
        std::fs::set_permissions(&s.path, std::fs::Permissions::from_mode(0o600))?;
        Ok(s)
    }
    pub fn expression_history(&self, c: &Conversation) -> Result<Vec<(String, String)>> {
        let db = self.db()?;
        let mut q = db.prepare("SELECT sha256,family FROM expression_history WHERE conversation=? ORDER BY created DESC LIMIT 12")?;
        Ok(q.query_map([c.key()], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    // Reserve before UI side effects: an uncertain attempt also must not be repeated blindly.
    pub fn reserve_expression(
        &self,
        key: &str,
        c: &Conversation,
        sha: &str,
        family: &str,
    ) -> Result<bool> {
        Ok(self.db()?.execute(
            "INSERT OR IGNORE INTO expression_history VALUES(?,?,?,?,?,0)",
            params![key, c.key(), sha, family, now()],
        )? == 1)
    }
    pub fn verify_expression(&self, key: &str) -> Result<()> {
        self.db()?.execute(
            "UPDATE expression_history SET verified=1 WHERE delivery=?",
            [key],
        )?;
        Ok(())
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
    pub fn policy_hash(&self, c: &Conversation) -> Result<Option<String>> {
        Ok(self
            .db()?
            .query_row(
                "SELECT hash FROM session_policy WHERE conversation=?",
                [c.key()],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub fn note_policy(&self, c: &Conversation, hash: &str) -> Result<()> {
        self.db()?.execute("INSERT INTO session_policy VALUES(?,?,?) ON CONFLICT(conversation) DO UPDATE SET hash=excluded.hash,applied=excluded.applied",params![c.key(),hash,now()])?;
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
        if receipt["status"] == "sent_verified"
            && (reply.contains("Codex[유이]") || reply.contains("여동생 유이"))
        {
            self.db()?.execute("INSERT INTO routes(conversation,introduced) VALUES(?,1) ON CONFLICT(conversation) DO UPDATE SET introduced=1",[c.key()])?;
        }
        Ok(())
    }
    pub fn enqueue(&self, e: &Event) -> Result<bool> {
        self.record_event(e, "pending", None)
    }
    pub fn record_rejected(&self, e: &Event, reason: &str) -> Result<bool> {
        self.record_event(e, "rejected", Some(reason))
    }
    fn record_event(&self, e: &Event, status: &str, reason: Option<&str>) -> Result<bool> {
        let mut db = self.db()?;
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
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let added = tx.execute(
            "INSERT OR IGNORE INTO events VALUES(?,?,?,?,?,?)",
            params![
                e.key(),
                if status == "pending" {
                    serde_json::to_string(e)?
                } else {
                    String::new()
                },
                status,
                reason,
                now(),
                now()
            ],
        )? == 1;
        if added {
            tx.execute(
                "INSERT INTO call_log VALUES(?,?,?,?,?,?,?)",
                params![
                    e.key(),
                    e.conversation.key(),
                    e.id,
                    e.occurred_at,
                    if e.body.contains("@[유이]") {
                        "initial_tag"
                    } else {
                        "followup_tag"
                    },
                    e.title,
                    if self.store_bodies {
                        Some(e.body.as_str())
                    } else {
                        None
                    }
                ],
            )?;
        }
        tx.commit()?;
        Ok(added)
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
    pub fn bindings(&self) -> Result<Vec<Value>> {
        let db = self.db()?;
        let mut stmt=db.prepare("SELECT s.conversation,s.thread,r.title,r.introduced,(SELECT max(e.created) FROM call_log c JOIN events e ON c.event_key=e.key WHERE c.conversation=s.conversation),(SELECT count(*) FROM call_log c JOIN events e ON c.event_key=e.key WHERE c.conversation=s.conversation),(SELECT count(*) FROM call_log c JOIN events e ON c.event_key=e.key WHERE c.conversation=s.conversation AND e.status='dispatching') FROM sessions s LEFT JOIN routes r ON s.conversation=r.conversation ORDER BY s.conversation")?;
        let mut items = Vec::new();
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<f64>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })? {
            let (key, thread, title, introduced, last, count, busy) = row?;
            let parts: Vec<String> = serde_json::from_str(&key)?;
            items.push(json!({"key":key,"provider":parts[0],"account":parts[1],"conversation_id":parts[2],"thread_id":thread,"title":title,"introduced":introduced.unwrap_or(0)!=0,"last_call_at":last,"recorded_calls":count,"processing":busy>0}));
        }
        Ok(items)
    }
    pub fn calls(
        &self,
        conversation: Option<&str>,
        before: Option<f64>,
        limit: usize,
    ) -> Result<Value> {
        let db = self.db()?;
        let mut stmt=db.prepare("SELECT e.key,e.status,e.reason,e.created,e.updated,c.conversation,c.message_id,c.occurred,c.trigger_kind,c.notification_title,c.body FROM events e LEFT JOIN call_log c ON c.event_key=e.key WHERE (?1 IS NULL OR c.conversation=?1 OR c.conversation IS NULL) AND (?2 IS NULL OR e.created<?2) ORDER BY e.created DESC LIMIT ?3")?;
        let mut result = Vec::new();
        let mut cursor = Value::Null;
        let mut scanned = 0usize;
        for row in stmt.query_map(
            params![conversation, before, limit.clamp(1, 100) as i64],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, f64>(3)?,
                    r.get::<_, f64>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<f64>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, Option<String>>(10)?,
                ))
            },
        )? {
            let (
                key,
                status,
                reason,
                created,
                updated,
                scope,
                message_id,
                occurred,
                trigger,
                notification_title,
                body,
            ) = row?;
            scanned += 1;
            cursor = json!(created);
            let mut deliveries_stmt = db.prepare(
                "SELECT phase,status,receipt FROM deliveries WHERE event_key=? ORDER BY rowid",
            )?;
            let mut deliveries = Vec::new();
            for d in deliveries_stmt.query_map([&key], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })? {
                let (phase, status, receipt) = d?;
                let raw: Value = receipt
                    .map(|s| serde_json::from_str(&s))
                    .transpose()?
                    .unwrap_or(json!({}));
                deliveries.push(json!({"phase":phase,"status":status,"reason":raw["reason"],"verified_chat_name":raw["verified_chat_name"],"text_sent":raw["text_sent"],"attachment_sent":raw["attachment_sent"],"elapsed_seconds":raw["elapsed_seconds"]}));
            }
            // Only infer a legacy binding from a unique verified receipt title;
            // never invent the original prompt, caller or trigger tag.
            let mut scope = scope;
            if scope.is_none() {
                let title = deliveries
                    .iter()
                    .find_map(|d| d["verified_chat_name"].as_str());
                if let Some(title) = title {
                    let mut q =
                        db.prepare("SELECT conversation FROM routes WHERE title=? LIMIT 2")?;
                    let matches = q
                        .query_map([title], |r| r.get::<_, String>(0))?
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    if matches.len() == 1 {
                        scope = Some(matches[0].clone())
                    }
                }
            }
            if conversation.is_some_and(|wanted| scope.as_deref() != Some(wanted)) {
                continue;
            }
            result.push(json!({"event_key":key,"status":status,"reason":reason,"received_at":created,"updated_at":updated,"conversation_key":scope,"message_id":message_id,"occurred_at":occurred,"trigger_kind":trigger,"notification_title":notification_title,"sender_verified":false,"body":body,"historical_metadata_missing":trigger.is_none(),"deliveries":deliveries}));
        }
        Ok(json!({"items":result,"next_before":cursor,"has_more":scanned>=limit.clamp(1,100)}))
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
