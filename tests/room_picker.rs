use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use communication_hub::{
    capabilities,
    config::Config,
    daemon::{authorize_loco, ingest_loco},
    dashboard::{Board, router},
    event::{Conversation, now},
    loco,
    store::Store,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    sync::{Notify, RwLock},
};
use tower::ServiceExt;

fn setup() -> (TempDir, Config, Store, Router) {
    let t = TempDir::new().unwrap();
    let p = t.path();
    std::fs::write(
        p.join("policy.md"),
        "name: contact-other\nFixture sharing policy\n",
    )
    .unwrap();
    let cfg:Config=serde_json::from_value(json!({"state":p.join("state"),"socket":p.join("hub.sock"),"app_server_socket":p.join("app.sock"),"contact_skill":p.join("policy.md"),"lookup_workdir":p,"external_auto_send":true,"kakao":{"enabled":true,"account":"owner","legacy_state":p.join("legacy"),"receiver_app":p.join("receiver"),"sender_app":p.join("sender"),"sender_ipc":p.join("ipc"),"loco":{"socket":p.join("loco.sock"),"expected_user_id":"123","mode":"active","rooms":{}}}})).unwrap();
    let store = Store::open(cfg.state.clone()).unwrap();
    let app = router(Board {
        cfg: Arc::new(cfg.clone()),
        store: store.clone(),
        active: Arc::new(AtomicBool::new(false)),
        sending: Arc::new(AtomicBool::new(false)),
        processing: Arc::new(AtomicBool::new(false)),
        backend: Arc::new(RwLock::new(json!({}))),
        token: "test-secret".into(),
        origin: "http://127.0.0.1:4444".into(),
    });
    (t, cfg, store, app)
}
fn catalog() -> Value {
    json!({"status":"ready","user_id":"123","rooms":[{"chat_id":"456","name":"Same title","type":"MultiChat","member_count":3},{"chat_id":"789","name":"Same title","type":"DirectChat","member_count":2}]})
}
fn sidecar(
    cfg: &Config,
    store: &Store,
    catalog: Value,
    fail_refresh: bool,
) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(&cfg.kakao.loco.as_ref().unwrap().socket).unwrap();
    let cfg = cfg.clone();
    let store = store.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let q: Value = serde_json::from_str(&line).unwrap();
            let result = match q["method"].as_str().unwrap() {
                "list_rooms" => catalog.clone(),
                "refresh_policy" => {
                    if fail_refresh {
                        json!({"status":"held","user_id":"123","reason":"offline"})
                    } else {
                        let p = store.loco_policy(&cfg).unwrap();
                        json!({"status":"ready","user_id":"123","revision":p["revision"],"rooms":p["rooms"].as_array().unwrap().iter().map(|r|json!({"chat_id":r["chat_id"],"status":"ready"})).collect::<Vec<_>>()})
                    }
                }
                _ => panic!("unexpected RPC {q}"),
            };
            reader
                .get_mut()
                .write_all(
                    format!("{}\n", json!({"id":q["id"],"ok":true,"result":result})).as_bytes(),
                )
                .await
                .unwrap();
        }
    })
}
async fn api(app: &Router, path: &str, value: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .uri(path)
        .method(if value.is_some() { "POST" } else { "GET" })
        .header("host", "127.0.0.1:4444")
        .header("authorization", "Bearer test-secret")
        .header("content-type", "application/json");
    req = req.header("origin", "http://127.0.0.1:4444");
    let response = app
        .clone()
        .oneshot(
            req.body(value.map_or(Body::empty(), |v| Body::from(v.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}
fn count(store: &Store, table: &str) -> i64 {
    store
        .db()
        .unwrap()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}
fn conversation(key: &Value) -> Conversation {
    let [provider, account, id]: [String; 3] = serde_json::from_str(key.as_str().unwrap()).unwrap();
    Conversation {
        provider,
        account,
        id,
    }
}
fn input(epoch: &Value) -> Value {
    json!({"mode":"active","user_id":"123","chat_id":"456","log_id":"900","author_id":"111","sent_at":now(),"body":"@[유이] fixture","title":"Same title","approval_epoch":epoch})
}

#[tokio::test]
async fn actual_http_catalog_adds_distinct_ids_before_any_message_and_accepts_first_call() {
    let (_t, cfg, store, app) = setup();
    let peer = sidecar(&cfg, &store, catalog(), false);
    // A preexisting name alias must never be silently merged into either actual identity.
    store
        .add_room_by_name("kakao", "owner", "Same title", true, false, 2)
        .unwrap();
    let (status, list) = api(&app, "/api/rooms/available", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["transport"], "loco");
    assert_eq!(list["rooms"].as_array().unwrap().len(), 2);
    assert_eq!(list["rooms"][0]["registered"], false);
    let mut keys = Vec::new();
    for id in ["456", "789"] {
        let (status, result) = api(
            &app,
            "/api/rooms/add",
            Some(json!({"chat_id":id,"title":"forged display name","yui":true,"yumi":false})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(result["added"], true);
        assert_eq!(result["sync_status"], "ready");
        assert_eq!(result["room"]["name"], "Same title");
        keys.push(result["room"]["key"].clone());
    }
    assert_ne!(keys[0], keys[1]);
    assert_eq!(store.rooms().unwrap().len(), 3);
    assert_eq!(count(&store, "events"), 0);
    assert_eq!(count(&store, "deliveries"), 0);
    assert_eq!(count(&store, "call_log"), 0);
    let (_, registered) = api(&app, "/api/rooms", None).await;
    assert_eq!(registered["transport"], "loco");
    assert_eq!(registered["sync_status"], "ready");
    assert_eq!(
        registered["rooms"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["transport"] == "loco")
            .count(),
        2
    );
    let binding = store
        .loco_binding(&cfg, &conversation(&keys[0]))
        .unwrap()
        .unwrap();
    let accepted = ingest_loco(
        &cfg,
        &store,
        &input(&binding["approval_epoch"]),
        &Notify::new(),
        false,
    )
    .unwrap();
    assert_eq!(accepted["status"], "queued");
    assert_eq!(count(&store, "events"), 1);
    assert_eq!(count(&store, "deliveries"), 0);
    peer.abort();
}
#[tokio::test]
async fn unknown_ids_bad_ids_and_foreign_account_catalog_never_grant() {
    let (_t, cfg, store, app) = setup();
    let peer = sidecar(&cfg, &store, catalog(), false);
    for id in [
        json!("999"),
        json!("0"),
        json!(456),
        Value::Null,
        json!("456 OR 1=1"),
    ] {
        let (status, _) = api(
            &app,
            "/api/rooms/add",
            Some(json!({"chat_id":id,"yui":true,"yumi":true})),
        )
        .await;
        assert!(status.is_client_error());
    }
    let (status, _) = api(
        &app,
        "/api/rooms/add",
        Some(json!({"title":"Same title","yui":true,"yumi":true})),
    )
    .await;
    assert!(status.is_client_error());
    assert_eq!(count(&store, "rooms"), 0);
    peer.abort();
    // A separate fixture avoids socket races and account changes in the same authenticated session.
    let (_t2, cfg2, store2, app2) = setup();
    let mut wrong = catalog();
    wrong["user_id"] = json!("987");
    let peer2 = sidecar(&cfg2, &store2, wrong, false);
    let (status, _) = api(
        &app2,
        "/api/rooms/add",
        Some(json!({"chat_id":"456","yui":true,"yumi":true})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(count(&store2, "rooms"), 0);
    assert_eq!(count(&store2, "events"), 0);
    peer2.abort();
}
#[tokio::test]
async fn remove_restart_and_readd_cannot_revive_old_call_or_sending_lease() {
    let (_t, cfg, store, app) = setup();
    let peer = sidecar(&cfg, &store, catalog(), false);
    let (_, added) = api(
        &app,
        "/api/rooms/add",
        Some(json!({"chat_id":"456","yui":true,"yumi":false})),
    )
    .await;
    let key = added["room"]["key"].clone();
    let c = conversation(&key);
    let binding = store.loco_binding(&cfg, &c).unwrap().unwrap();
    let raw = input(&binding["approval_epoch"]);
    ingest_loco(&cfg, &store, &raw, &Notify::new(), false).unwrap();
    let payload: String = store
        .db()
        .unwrap()
        .query_row("SELECT payload FROM events", [], |r| r.get(0))
        .unwrap();
    let mut event: communication_hub::event::Event = serde_json::from_str(&payload).unwrap();
    event.metadata["__hub_authorization_stamp"] =
        json!(capabilities::authorization_stamp(&cfg, &event).unwrap());
    let plan = communication_hub::event::Plan {
        reply: "[System-유이] : fixture".into(),
        bundle_id: None,
        sticker_id: None,
    };
    store.prepare(&event.key(), &event, "final", &plan).unwrap();
    store.claim_delivery(&event.key()).unwrap();
    let q = json!({"delivery_id":event.key(),"user_id":"123","chat_id":"456","component":"text","approval_epoch":binding["approval_epoch"]});
    assert_eq!(
        authorize_loco(&cfg, &store, &q, true).unwrap()["status"],
        "authorized"
    );
    let (_, removed) = api(&app, "/api/rooms/remove", Some(json!({"key":key}))).await;
    assert_eq!(removed["removed"], true);
    assert_eq!(removed["sync_status"], "ready");
    assert!(loco::managed(&cfg, &store, &c).unwrap());
    assert!(loco::target(&cfg, &store, &c).unwrap().is_none());
    assert_eq!(
        authorize_loco(&cfg, &store, &q, true).unwrap()["status"],
        "held"
    );
    let restored = Store::open(cfg.state.clone()).unwrap();
    assert_eq!(restored.loco_policy(&cfg).unwrap()["rooms"], json!([]));
    assert_eq!(count(&restored, "call_log"), 1);
    assert_eq!(count(&restored, "deliveries"), 1);
    let (_, again) = api(
        &app,
        "/api/rooms/add",
        Some(json!({"chat_id":"456","yui":true,"yumi":false})),
    )
    .await;
    assert_eq!(again["room"]["key"], key);
    let new = store.loco_binding(&cfg, &c).unwrap().unwrap();
    assert_ne!(new["approval_epoch"], binding["approval_epoch"]);
    assert_eq!(
        ingest_loco(&cfg, &store, &raw, &Notify::new(), false).unwrap()["reason"],
        "loco_approval_epoch_changed"
    );
    assert!(
        capabilities::delivery_gate(&cfg, &store, &event)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        authorize_loco(&cfg, &store, &q, true).unwrap()["status"],
        "held"
    );
    assert!(communication_hub::daemon::validate_deferred_authorization(&cfg, &event).is_err());
    peer.abort();
}
#[tokio::test]
async fn failed_sync_reports_saved_pending_and_revocation_is_immediate() {
    let (_t, cfg, store, app) = setup();
    let peer = sidecar(&cfg, &store, catalog(), true);
    let (status, added) = api(
        &app,
        "/api/rooms/add",
        Some(json!({"chat_id":"456","yui":true,"yumi":false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(added["added"], true);
    assert_eq!(added["sync_status"], "pending");
    let c = conversation(&added["room"]["key"]);
    assert!(loco::target(&cfg, &store, &c).unwrap().is_some());
    let (_, removed) = api(
        &app,
        "/api/rooms/remove",
        Some(json!({"key":added["room"]["key"]})),
    )
    .await;
    assert_eq!(removed["removed"], true);
    assert_eq!(removed["sync_status"], "pending");
    assert!(loco::target(&cfg, &store, &c).unwrap().is_none());
    assert_eq!(count(&store, "events"), 0);
    peer.abort();
}
#[test]
fn static_seed_runs_once_and_tombstones_survive_even_when_config_still_lists_room() {
    let (_t, mut cfg, store, _) = setup();
    cfg.kakao
        .loco
        .as_mut()
        .unwrap()
        .rooms
        .insert("legacy".into(), "456".into());
    let c = Conversation {
        provider: "kakao".into(),
        account: "owner".into(),
        id: "legacy".into(),
    };
    store.note_room_seen(&c, "Legacy").unwrap();
    store.update_room(&c.key(), true, false, true).unwrap();
    store.seed_loco(&cfg).unwrap();
    let before = store.loco_binding(&cfg, &c).unwrap().unwrap();
    store.remove_room(&c.key()).unwrap();
    let reopened = Store::open(cfg.state.clone()).unwrap();
    assert_eq!(reopened.loco_policy(&cfg).unwrap()["rooms"], json!([]));
    let tombstone = reopened.loco_binding(&cfg, &c).unwrap().unwrap();
    assert_eq!(tombstone["deleted"], true);
    assert_ne!(tombstone["approval_epoch"], before["approval_epoch"]);
    assert!(loco::managed(&cfg, &reopened, &c).unwrap());
    cfg.kakao
        .loco
        .as_mut()
        .unwrap()
        .rooms
        .insert("invented".into(), "999".into());
    reopened.seed_loco(&cfg).unwrap();
    assert_eq!(count(&reopened, "loco_bindings"), 1);
}

#[tokio::test]
async fn approval_sister_and_context_mutations_update_epoch_and_policy_without_restart() {
    let (_t, cfg, store, app) = setup();
    let peer = sidecar(&cfg, &store, catalog(), false);
    let (_, added) = api(
        &app,
        "/api/rooms/add",
        Some(json!({"chat_id":"456","yui":true,"yumi":false})),
    )
    .await;
    let key = added["room"]["key"].clone();
    let c = conversation(&key);
    let mut epoch = store.loco_binding(&cfg, &c).unwrap().unwrap()["approval_epoch"].clone();
    for (approved, yui) in [(true, false), (false, true), (true, true)] {
        let (status, result) = api(
            &app,
            "/api/rooms",
            Some(json!({"key":key,"approved":approved,"yui":yui,"yumi":false})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(result["updated"], true);
        assert_eq!(result["sync_status"], "ready");
        let next = store.loco_binding(&cfg, &c).unwrap().unwrap()["approval_epoch"].clone();
        assert_ne!(next, epoch);
        epoch = next;
        assert_eq!(
            store.loco_policy(&cfg).unwrap()["rooms"]
                .as_array()
                .unwrap()
                .len(),
            usize::from(approved && yui)
        );
    }
    let (_, reset) = api(&app, "/api/rooms/reset-context", Some(json!({"key":key}))).await;
    assert_eq!(reset["reset"], true);
    assert_ne!(
        store.loco_binding(&cfg, &c).unwrap().unwrap()["approval_epoch"],
        epoch
    );
    assert_eq!(count(&store, "events"), 0);
    assert_eq!(count(&store, "deliveries"), 0);
    peer.abort();
}
#[test]
fn account_change_never_rebinds_another_users_room_or_falls_back_to_ax() {
    let (_t, mut cfg, store, _) = setup();
    let room = store
        .add_loco_room(&cfg, &catalog()["rooms"][0], true, false)
        .unwrap();
    let c = conversation(&room["key"]);
    cfg.kakao.loco.as_mut().unwrap().expected_user_id = "999".into();
    assert!(
        store
            .add_loco_room(&cfg, &catalog()["rooms"][0], true, false)
            .is_err()
    );
    assert!(loco::managed(&cfg, &store, &c).unwrap());
    assert!(loco::target(&cfg, &store, &c).unwrap().is_none());
    assert_eq!(store.loco_policy(&cfg).unwrap()["rooms"], json!([]));
}

#[tokio::test]
async fn quota_is_atomic_across_add_reapprove_and_sister_enable_paths() {
    let (_t, cfg, store, app) = setup();
    let mut rooms = Vec::new();
    for id in 1..=101 {
        rooms.push(json!({"chat_id":id.to_string(),"name":format!("Room {id}"),"type":"MultiChat","member_count":2}));
    }
    for room in rooms.iter().take(loco::MAX_POLICY_ROOMS) {
        store.add_loco_room(&cfg, room, true, false).unwrap();
    }
    let peer = sidecar(
        &cfg,
        &store,
        json!({"status":"ready","user_id":"123","rooms":rooms}),
        false,
    );
    let before = store.loco_policy(&cfg).unwrap();
    let registered = store.rooms().unwrap();
    let (status, rejected) = api(
        &app,
        "/api/rooms/add",
        Some(json!({"chat_id":"101","yui":true,"yumi":false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rejected,
        json!({"added":false,"status":"held","reason":"room_limit_reached","limit":100})
    );
    assert_eq!(store.loco_policy(&cfg).unwrap(), before);
    assert_eq!(store.rooms().unwrap(), registered);
    assert_eq!(count(&store, "loco_bindings"), 100);
    let (_, off) = api(
        &app,
        "/api/rooms/add",
        Some(json!({"chat_id":"101","yui":false,"yumi":false})),
    )
    .await;
    assert_eq!(off["added"], true);
    let key = off["room"]["key"].clone();
    let before = store.loco_policy(&cfg).unwrap();
    let registered = store.rooms().unwrap();
    let (status, rejected) = api(
        &app,
        "/api/rooms",
        Some(json!({"key":key,"approved":true,"yui":false,"yumi":true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rejected,
        json!({"updated":false,"status":"held","reason":"room_limit_reached","limit":100})
    );
    assert_eq!(store.loco_policy(&cfg).unwrap(), before);
    assert_eq!(store.rooms().unwrap(), registered);
    // The legacy title API cannot evade the same durable limit.
    assert_eq!(
        store
            .add_room_by_name("kakao", "owner", "Room 101", true, false, 101)
            .unwrap_err()
            .to_string(),
        "room_limit_reached"
    );
    assert_eq!(store.loco_policy(&cfg).unwrap(), before);
    let first = Conversation {
        provider: "kakao".into(),
        account: "owner".into(),
        id: "1".into(),
    };
    store.update_room(&first.key(), false, true, false).unwrap();
    store
        .update_room(key.as_str().unwrap(), true, true, false)
        .unwrap();
    let before = store.loco_policy(&cfg).unwrap();
    assert_eq!(before["rooms"].as_array().unwrap().len(), 100);
    let (_, rejected) = api(
        &app,
        "/api/rooms",
        Some(json!({"key":first.key(),"approved":true,"yui":true,"yumi":false})),
    )
    .await;
    assert_eq!(rejected["reason"], "room_limit_reached");
    assert_eq!(store.loco_policy(&cfg).unwrap(), before);
    store
        .update_room(key.as_str().unwrap(), true, false, false)
        .unwrap();
    store.update_room(&first.key(), true, true, false).unwrap();
    assert_eq!(
        store.loco_policy(&cfg).unwrap()["rooms"]
            .as_array()
            .unwrap()
            .len(),
        100
    );
    assert_eq!(count(&store, "events"), 0);
    assert_eq!(count(&store, "deliveries"), 0);
    peer.abort();
}
#[test]
fn static_policy_limits_and_oversized_identifiers_fail_before_partial_migration() {
    let (_t, mut cfg, store, _) = setup();
    cfg.kakao.loco.as_mut().unwrap().rooms = (1..=101)
        .map(|id| (id.to_string(), id.to_string()))
        .collect();
    assert_eq!(
        cfg.kakao
            .loco
            .as_ref()
            .unwrap()
            .validate()
            .unwrap_err()
            .to_string(),
        "room_limit_reached"
    );
    assert!(store.seed_loco(&cfg).is_err());
    assert_eq!(count(&store, "loco_bindings"), 0);
    cfg.kakao.loco.as_mut().unwrap().rooms =
        std::collections::BTreeMap::from([("x".repeat(257), "1".into())]);
    assert!(store.seed_loco(&cfg).is_err());
    assert_eq!(count(&store, "loco_bindings"), 0);
    cfg.kakao.loco.as_mut().unwrap().rooms =
        std::collections::BTreeMap::from([("x".repeat(256), "1".into())]);
    store.seed_loco(&cfg).unwrap();
    assert_eq!(count(&store, "loco_bindings"), 1);
}
#[test]
fn preexisting_oversized_runtime_policy_is_rejected_and_revocation_can_repair_it() {
    let (_t, cfg, store, _) = setup();
    store.seed_loco(&cfg).unwrap();
    let db = store.db().unwrap();
    for id in 1..=101 {
        let c = Conversation {
            provider: "kakao".into(),
            account: "owner".into(),
            id: id.to_string(),
        };
        db.execute(
            "INSERT INTO rooms(conversation,title,approved,yui,yumi,seen) VALUES(?,?,1,1,0,1)",
            rusqlite::params![c.key(), format!("Room {id}")],
        )
        .unwrap();
        db.execute(
            "INSERT INTO loco_bindings(conversation,user_id,chat_id) VALUES(?,'123',?)",
            rusqlite::params![c.key(), id.to_string()],
        )
        .unwrap();
    }
    assert_eq!(
        store.loco_policy(&cfg).unwrap_err().to_string(),
        "room_limit_reached"
    );
    let c = Conversation {
        provider: "kakao".into(),
        account: "owner".into(),
        id: "101".into(),
    };
    store.update_room(&c.key(), false, true, false).unwrap();
    assert_eq!(
        store.loco_policy(&cfg).unwrap()["rooms"]
            .as_array()
            .unwrap()
            .len(),
        100
    );
}

#[test]
fn seed_overflow_rolls_back_all_bindings_and_migration_marker() {
    let (_t, mut cfg, store, _) = setup();
    for id in 1..=100 {
        store
            .add_loco_room(
                &cfg,
                &json!({"chat_id":id.to_string(),"name":format!("Room {id}")}),
                true,
                false,
            )
            .unwrap();
    }
    let extra = Conversation {
        provider: "kakao".into(),
        account: "owner".into(),
        id: "101".into(),
    };
    store.note_room_seen(&extra, "Extra").unwrap();
    store.update_room(&extra.key(), true, true, false).unwrap();
    let db = store.db().unwrap();
    db.execute(
        "DELETE FROM migrations WHERE name='loco-bindings-v1:owner:123'",
        [],
    )
    .unwrap();
    let revision: i64 = db
        .query_row(
            "SELECT revision FROM loco_policy_revision WHERE id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    cfg.kakao
        .loco
        .as_mut()
        .unwrap()
        .rooms
        .insert("101".into(), "101".into());
    assert_eq!(
        store.seed_loco(&cfg).unwrap_err().to_string(),
        "room_limit_reached"
    );
    assert_eq!(count(&store, "loco_bindings"), 100);
    assert_eq!(
        db.query_row(
            "SELECT revision FROM loco_policy_revision WHERE id=1",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        revision
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM migrations WHERE name='loco-bindings-v1:owner:123'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}
