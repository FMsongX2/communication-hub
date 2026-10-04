use communication_hub::{
    attachments,
    config::{Config, KakaoConfig},
    event::{Conversation, Event, Plan, now},
    rpc::usage_limit,
    store::Store,
    worker,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tempfile::TempDir;

fn config(root: &Path) -> Config {
    std::fs::create_dir_all(root.join("legacy")).unwrap();
    std::fs::write(
        root.join("policy.md"),
        "---\nname: contact-other\n---\nPRIVATE를 공유하지 않는다.\n",
    )
    .unwrap();
    Config {
        state: root.join("state"),
        socket: root.join("hub.sock"),
        app_server_socket: root.join("app.sock"),
        contact_skill: root.join("policy.md"),
        lookup_workdir: root.to_owned(),
        external_auto_send: false,
        model: worker::MODEL.into(),
        effort: worker::EFFORT.into(),
        dispatch_enabled: false,
        dashboard: None,
        intro_text: None,
        service_tier: None,
        expressions: None,
        yumi: None,
        kakao: KakaoConfig {
            enabled: true,
            account: "owner".into(),
            legacy_state: root.join("legacy"),
            receiver_app: root.join("receiver.app"),
            sender_app: root.join("sender.app"),
            sender_ipc: root.join("ipc"),
            prewarm: false,
            watch_open_room: false,
            defer_locked_delivery: true,
            deferred_delivery_ttl_seconds: 86400,
            locked_ax_text: false,
        },
    }
}
/// The owner approves the fixture room on the dashboard before calls in it are answered.
fn approve_room(cfg: &Config, e: &Event) {
    let s = Store::open(cfg.state.clone()).unwrap();
    s.note_room_seen(&e.conversation, &e.title).unwrap();
    assert!(
        s.update_room(&e.conversation.key(), true, true, true)
            .unwrap()
    );
}
fn event() -> Event {
    Event {
        conversation: Conversation {
            provider: "kakao".into(),
            account: "owner".into(),
            id: "room1".into(),
        },
        id: "msg1".into(),
        body: "@[유이] ㅎㅇ".into(),
        title: "fixture".into(),
        occurred_at: now(),
        source: "kakao_notification_store".into(),
        metadata: json!({}),
        agent: Default::default(),
    }
}
#[test]
fn provider_account_conversation_are_independent() {
    let t = TempDir::new().unwrap();
    let s = Store::open(t.path().join("state")).unwrap();
    let e = event();
    s.save_session(&e.conversation, "first").unwrap();
    assert!(s.enqueue(&e).unwrap());
    let mut other = e.clone();
    other.conversation.provider = "discord".into();
    assert_ne!(e.key(), other.key());
    assert!(s.session(&other.conversation).unwrap().is_none());
    assert!(s.enqueue(&other).unwrap());
    other.conversation.provider = "kakao".into();
    other.conversation.account = "team".into();
    assert!(s.session(&other.conversation).unwrap().is_none());
    assert!(s.enqueue(&other).unwrap());
    assert!(s.save_session(&other.conversation, "first").is_err());
}
#[test]
fn duplicate_rewrapped_event_is_not_processed_twice() {
    let t = TempDir::new().unwrap();
    let s = Store::open(t.path().join("state")).unwrap();
    let e = event();
    assert!(s.enqueue(&e).unwrap());
    let mut e2 = e.clone();
    e2.body = "@[유이] changed formatting".into();
    assert!(!s.enqueue(&e2).unwrap());
    assert_eq!(s.claim_next().unwrap().unwrap().body, e.body);
    assert!(s.claim_next().unwrap().is_none());
}
#[test]
fn restart_quarantines_in_flight_work_and_never_reclaims_sending() {
    let t = TempDir::new().unwrap();
    let s = Store::open(t.path().join("state")).unwrap();
    let e = event();
    s.enqueue(&e).unwrap();
    s.claim_next().unwrap();
    let p = Plan {
        reply: "[System-유이] : fixture".into(),
        bundle_id: None,
        sticker_id: None,
    };
    s.prepare(&e.key(), &e, "final", &p).unwrap();
    assert!(s.claim_delivery(&e.key()).unwrap());
    s.recover().unwrap();
    assert!(s.claim_next().unwrap().is_none());
    assert!(!s.claim_delivery(&e.key()).unwrap());
    let st = s.status().unwrap();
    assert_eq!(st["events"][0][0], "ambiguous");
    assert_eq!(st["deliveries"][0][0], "sending_uncertain");
}
#[test]
fn concurrent_intake_does_not_wait_for_claimed_model_work() {
    let t = TempDir::new().unwrap();
    let s = Store::open(t.path().join("state")).unwrap();
    let e = event();
    s.enqueue(&e).unwrap();
    s.claim_next().unwrap();
    let threads: Vec<_> = (0..20)
        .map(|i| {
            let s = s.clone();
            let mut e = e.clone();
            e.id = format!("msg{i}extra");
            std::thread::spawn(move || {
                assert!(s.enqueue(&e).unwrap());
                assert!(!s.enqueue(&e).unwrap())
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let mut count = 0;
    while s.claim_next().unwrap().is_some() {
        count += 1
    }
    assert_eq!(count, 20);
}
#[test]
fn tags_freshness_completeness_and_echo_contract() {
    let at = now();
    let mut e = event();
    assert!(e.validate(at, false).is_ok());
    e.body = "[유이] followup".into();
    assert!(e.validate(at, false).is_err());
    assert!(e.validate(at, true).is_ok());
    for body in [
        "no tag",
        "[System-유이] : [유이] echo",
        "@[유이] truncated...",
        "@[유이] truncated…",
    ] {
        e.body = body.into();
        assert!(e.validate(at, true).is_err())
    }
    e.body = "@[유이] good".into();
    e.occurred_at = at - 301.0;
    assert!(e.validate(at, true).is_err());
    e.occurred_at = at + 6.0;
    assert!(e.validate(at, true).is_err());
}
#[test]
fn permissions_and_channel_capability_fail_closed() {
    use std::os::unix::fs::PermissionsExt;
    let t = TempDir::new().unwrap();
    let c = config(t.path());
    let s = Store::open(c.state.clone()).unwrap();
    assert_eq!(
        std::fs::metadata(&s.path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    for p in ["slack", "discord", "notion"] {
        assert!(c.validate_channel(p, "owner").is_err())
    }
    assert!(c.validate_channel("kakao", "other").is_err());
    assert_eq!(c.descriptors()[3]["kind"], "documents_and_comments");
}
#[test]
fn legacy_import_preserves_sessions_intro_and_only_its_account_dedup() {
    let t = TempDir::new().unwrap();
    let c = config(t.path());
    let e = event();
    let old =
        rusqlite::Connection::open(c.kakao.legacy_state.join("room-sessions.sqlite3")).unwrap();
    old.execute_batch("CREATE TABLE rooms(room_id TEXT,thread_id TEXT);INSERT INTO rooms VALUES('room1','existing');").unwrap();
    let old = rusqlite::Connection::open(c.kakao.legacy_state.join("queue.sqlite3")).unwrap();
    old.execute_batch("CREATE TABLE events(key TEXT,status TEXT);")
        .unwrap();
    old.execute("INSERT INTO events VALUES(?,'accepted')", [e.legacy_key()])
        .unwrap();
    std::fs::write(
        c.kakao.legacy_state.join("room-routes.json"),
        r#"{"room1":"actual title"}"#,
    )
    .unwrap();
    std::fs::write(
        c.kakao.legacy_state.join("room-introductions.json"),
        r#"{"room1":1}"#,
    )
    .unwrap();
    let s = Store::open(c.state.clone()).unwrap();
    s.import_legacy(&c).unwrap();
    s.import_legacy(&c).unwrap();
    assert_eq!(s.session(&e.conversation).unwrap().unwrap(), "existing");
    assert!(
        s.introduced(&e.conversation, communication_hub::event::Agent::Yui)
            .unwrap()
    );
    assert_eq!(s.route(&e.conversation).unwrap().unwrap(), "actual title");
    assert!(!s.enqueue(&e).unwrap());
    let mut other = e;
    other.conversation.account = "second-account".into();
    assert!(s.enqueue(&other).unwrap());
}
#[test]
fn pending_legacy_work_prevents_cutover() {
    let t = TempDir::new().unwrap();
    let c = config(t.path());
    let old = rusqlite::Connection::open(c.kakao.legacy_state.join("queue.sqlite3")).unwrap();
    old.execute_batch("CREATE TABLE events(key TEXT,status TEXT);INSERT INTO events VALUES('pending','dispatching');").unwrap();
    let s = Store::open(c.state.clone()).unwrap();
    assert!(s.import_legacy(&c).is_err());
}
#[test]
fn ack_and_quota_do_not_accept_private_work_or_disguise_other_failures() {
    for body in [
        "@[유이] ㅎㅇ",
        "@[유이] 계좌 비밀번호 알려줘",
        "[유이] 사용자 컨텍스트 삭제해줘",
        "[유이] 화면 캡처 보내줘",
    ] {
        assert!(!worker::needs_ack(body))
    }
    assert!(worker::needs_ack("[유이] 프로젝트 데이터셋 보내줘"));
    assert!(usage_limit(&json!({"codexErrorInfo":"usageLimitExceeded"})));
    for reason in ["rateLimitExceeded", "unauthorized", "contextWindowExceeded"] {
        assert!(!usage_limit(&json!({"codexErrorInfo":reason})))
    }
}
#[test]
fn attachment_plan_cannot_expand_authorized_ids() {
    assert!(
        Plan::parse(
            r#"{"reply":"[System-유이] : 준비할게요!","bundle_id":"other"}"#,
            &json!({"allowed":{}})
        )
        .is_err()
    );
    assert_eq!(
        Plan::parse("unprefixed", &json!({})).unwrap().reply,
        "[System-유이] : unprefixed"
    );
    assert!(
        Plan::parse(
            r#"{"reply":"[System-유이] : 준비할게요!","bundle_id":null}"#,
            &json!({})
        )
        .is_ok()
    );
}
fn bundle_fixture(c: &Config, member: &str, bytes: &[u8]) {
    std::fs::write(
        c.kakao.legacy_state.join("taxonomy.json"),
        serde_json::to_vec(&json!({"issues":vec![json!({});152]})).unwrap(),
    )
    .unwrap();
    let mut z = zip::ZipWriter::new(
        std::fs::File::create(c.kakao.legacy_state.join("result.zip")).unwrap(),
    );
    z.start_file(member, zip::write::SimpleFileOptions::default())
        .unwrap();
    z.write_all(bytes).unwrap();
    z.finish().unwrap();
    std::fs::write(c.kakao.legacy_state.join("attachment-bundles.json"),serde_json::to_vec(&json!({"rooms":{"room1":{"bundle":{"taxonomy":c.kakao.legacy_state.join("taxonomy.json"),"result_zip":c.kakao.legacy_state.join("result.zip"),"filename":"data.zip"}}}})).unwrap()).unwrap();
}
#[test]
fn zip_preserves_bytes_and_room_scope() {
    let t = TempDir::new().unwrap();
    let c = config(t.path());
    let e = event();
    bundle_fixture(&c, "abc12345/manifest.json", b"{\"ok\":true}");
    let artifact = attachments::prepare(&c, &e, &e.key(), "bundle").unwrap();
    let mut z =
        zip::ZipArchive::new(std::fs::File::open(artifact["path"].as_str().unwrap()).unwrap())
            .unwrap();
    let mut data = Vec::new();
    z.by_name("assign-context/abc12345/manifest.json")
        .unwrap()
        .read_to_end(&mut data)
        .unwrap();
    assert_eq!(data, b"{\"ok\":true}");
    let mut other = e;
    other.conversation.id = "different-team".into();
    assert!(attachments::prepare(&c, &other, &other.key(), "bundle").is_err());
}
#[test]
fn malicious_zip_and_changed_taxonomy_are_refused() {
    let t = TempDir::new().unwrap();
    let c = config(t.path());
    let e = event();
    for (name, data) in [
        ("../../secret", b"x".as_slice()),
        (".env", b"x".as_slice()),
        ("manifest.json", br#"{"api_key":"private"}"#.as_slice()),
    ] {
        bundle_fixture(&c, name, data);
        assert!(attachments::prepare(&c, &e, &e.key(), "bundle").is_err())
    }
    bundle_fixture(&c, "good", b"x");
    std::fs::write(
        c.kakao.legacy_state.join("taxonomy.json"),
        b"{\"issues\":[]}",
    )
    .unwrap();
    assert!(attachments::prepare(&c, &e, &e.key(), "bundle").is_err());
}
async fn mock_server(
    path: &Path,
    calls: Arc<std::sync::Mutex<Vec<Value>>>,
    count: Arc<AtomicUsize>,
) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        count.fetch_add(1, Ordering::SeqCst);
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) = ws.next().await {
            let v: Value = serde_json::from_str(&text).unwrap();
            calls.lock().unwrap().push(v.clone());
            let method = v["method"].as_str().unwrap_or("");
            if v.get("id").is_none() {
                continue;
            }
            let result = match method {
                "initialize" => json!({}),
                "thread/inject_items" => json!({}),
                "thread/start" | "thread/resume" => {
                    json!({"thread":{"id":"fixture-thread","status":{"type":"idle"}},"model":worker::MODEL,"reasoningEffort":worker::EFFORT})
                }
                "turn/start" => {
                    // Notifications intentionally precede the RPC reply, including commentary.
                    for msg in [
                        json!({"method":"item/completed","params":{"threadId":"fixture-thread","turnId":"fixture-turn","item":{"type":"agentMessage","phase":"commentary","text":"must not be sent"}}}),
                        json!({"method":"item/completed","params":{"threadId":"fixture-thread","turnId":"fixture-turn","item":{"type":"agentMessage","phase":"final_answer","text":"{\"reply\":\"[System-유이] : 검증 답변\",\"bundle_id\":null}"}}}),
                    ] {
                        ws.send(tokio_tungstenite::tungstenite::Message::Text(
                            msg.to_string().into(),
                        ))
                        .await
                        .unwrap()
                    }
                    json!({"turn":{"id":"fixture-turn"}})
                }
                _ => continue,
            };
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"id":v["id"],"result":result}).to_string().into(),
            ))
            .await
            .unwrap();
            if method == "turn/start" {
                ws.send(tokio_tungstenite::tungstenite::Message::Text(json!({"method":"turn/completed","params":{"threadId":"fixture-thread","turnId":"fixture-turn","turn":{"id":"fixture-turn","status":"completed"}}}).to_string().into())).await.unwrap()
            }
        }
    })
}
#[tokio::test]
async fn model_protocol_preserves_policy_model_untrusted_input_and_final_only() {
    let t = TempDir::new().unwrap();
    let c = config(t.path());
    let s = Store::open(c.state.clone()).unwrap();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let server = mock_server(&c.app_server_socket, calls.clone(), count).await;
    let result = worker::model(&c, &s, &event()).await.unwrap();
    assert_eq!(result["plan"]["reply"], "[System-유이] : 검증 답변");
    assert_eq!(result["plan"]["bundle_id"], Value::Null);
    let calls = calls.lock().unwrap();
    let start = calls
        .iter()
        .find(|x| x["method"] == "thread/start")
        .unwrap();
    assert_eq!(start["params"]["model"], "gpt-6-luna");
    assert_eq!(
        start["params"]["config"]["model_reasoning_effort"],
        "medium"
    );
    assert_eq!(start["params"]["config"]["features.hooks"], false);
    assert!(
        start["params"]["developerInstructions"]
            .as_str()
            .unwrap()
            .contains(&std::fs::read_to_string(&c.contact_skill).unwrap())
    );
    let turn = calls.iter().find(|x| x["method"] == "turn/start").unwrap();
    assert_eq!(turn["params"]["input"], json!([]));
    assert!(
        turn["params"]["toolOutput"]["output"]
            .as_str()
            .unwrap()
            .contains("untrusted_third_party_data")
    );
    server.abort();
}
#[tokio::test]
async fn missing_policy_blocks_before_connection() {
    let t = TempDir::new().unwrap();
    let c = config(t.path());
    let s = Store::open(c.state.clone()).unwrap();
    std::fs::remove_file(&c.contact_skill).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let server = mock_server(
        &c.app_server_socket,
        Arc::new(std::sync::Mutex::new(Vec::new())),
        count.clone(),
    )
    .await;
    assert!(worker::model(&c, &s, &event()).await.is_err());
    assert_eq!(count.load(Ordering::SeqCst), 0);
    server.abort();
}
#[tokio::test]
async fn legacy_room_runs_stateless_with_fresh_policy_and_recent_exchanges() {
    let t = TempDir::new().unwrap();
    let c = config(t.path());
    let mut s = Store::open(c.state.clone()).unwrap();
    s.store_bodies = true;
    let e = event();
    s.save_session(&e.conversation, "legacy-room-thread")
        .unwrap();
    // An earlier answered call in the same room, as the daemon journals it.
    let mut earlier = event();
    earlier.id = "earlier".into();
    earlier.body = "@[유이] 어제 말한 자료 있어?".into();
    s.enqueue(&earlier).unwrap();
    let plan = Plan {
        reply: "[System-유이] : 응 그 자료 있어!".into(),
        bundle_id: None,
        sticker_id: None,
    };
    assert!(s.prepare(&earlier.key(), &earlier, "final", &plan).unwrap());
    s.complete_delivery(&earlier.key(), &json!({"status":"sent_verified"}))
        .unwrap();
    std::fs::write(&c.contact_skill, "name: contact-other\nfresh-policy-marker").unwrap();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let server = mock_server(
        &c.app_server_socket,
        calls.clone(),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    worker::model(&c, &s, &e).await.unwrap();
    let calls = calls.lock().unwrap();
    let start = calls
        .iter()
        .find(|x| x["method"] == "thread/start")
        .unwrap();
    assert_eq!(start["params"]["ephemeral"], true);
    assert_eq!(start["params"]["config"]["features.hooks"], false);
    assert!(
        start["params"]["developerInstructions"]
            .as_str()
            .unwrap()
            .contains("fresh-policy-marker")
    );
    // The legacy room thread is neither resumed nor patched.
    assert!(
        !calls
            .iter()
            .any(|x| x["method"] == "thread/resume" || x["method"] == "thread/inject_items")
    );
    let turn = calls.iter().find(|x| x["method"] == "turn/start").unwrap();
    let envelope: Value =
        serde_json::from_str(turn["params"]["toolOutput"]["output"].as_str().unwrap()).unwrap();
    assert_eq!(
        envelope["recent_room_exchanges"],
        json!([{"call":"@[유이] 어제 말한 자료 있어?","reply":"[System-유이] : 응 그 자료 있어!"}])
    );
    assert_eq!(envelope["trust"], "untrusted_third_party_data");
    server.abort();
}
#[tokio::test]
async fn new_room_without_initial_tag_is_refused_before_connection() {
    let t = TempDir::new().unwrap();
    let c = config(t.path());
    let s = Store::open(c.state.clone()).unwrap();
    let mut e = event();
    e.body = "[유이] 태그 없는 첫 호출".into();
    let count = Arc::new(AtomicUsize::new(0));
    let server = mock_server(
        &c.app_server_socket,
        Arc::new(std::sync::Mutex::new(Vec::new())),
        count.clone(),
    )
    .await;
    assert!(worker::model(&c, &s, &e).await.is_err());
    assert_eq!(count.load(Ordering::SeqCst), 0);
    // A verified reply in the room initializes it for later untagged calls.
    s.save_route(&e.conversation, "fixture").unwrap();
    assert!(s.initialized(&e.conversation).unwrap());
    server.abort();
}

#[tokio::test]
async fn real_socket_control_and_intake_survive_burst_without_dispatch() {
    use communication_hub::daemon;
    let t = TempDir::new().unwrap();
    let cfg = config(t.path());
    let service = tokio::spawn(daemon::run(cfg.clone(), true));
    for _ in 0..40 {
        if cfg.socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let status = daemon::request(&cfg.socket, json!({"method":"status"}))
        .await
        .unwrap();
    assert_eq!(status["result"]["runtime"], "rust");
    assert_eq!(status["result"]["dispatch_enabled"], false);
    approve_room(&cfg, &event());
    for i in 0..20 {
        let mut e = event();
        e.id = format!("burst{i}");
        let packet = json!({"method":"ingest","event":e});
        assert_eq!(
            daemon::request(&cfg.socket, packet.clone()).await.unwrap()["result"]["status"],
            "queued"
        );
        assert_eq!(
            daemon::request(&cfg.socket, packet).await.unwrap()["result"]["status"],
            "duplicate"
        );
    }
    let mut bad = event();
    bad.conversation.provider = "slack".into();
    assert_eq!(
        daemon::request(&cfg.socket, json!({"method":"ingest","event":bad}))
            .await
            .unwrap()["ok"],
        false
    );
    let status = daemon::request(&cfg.socket, json!({"method":"status"}))
        .await
        .unwrap();
    assert_eq!(status["result"]["store"]["events"][0][1], 20);
    assert_eq!(
        daemon::request(&cfg.socket, json!({"method":"pause"}))
            .await
            .unwrap()["result"]["paused"],
        true
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&cfg.socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    service.abort();
}

#[test]
fn concurrent_atomic_writers_leave_valid_private_json() {
    use std::os::unix::fs::PermissionsExt;
    let t = TempDir::new().unwrap();
    let path = t.path().join("state/control.json");
    let threads: Vec<_> = (0..20)
        .map(|i| {
            let path = path.clone();
            std::thread::spawn(move || {
                communication_hub::config::atomic(&path, &json!({"writer":i})).unwrap()
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap()
    }
    assert!(
        serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap()["writer"]
            .is_number()
    );
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test]
async fn storage_failure_is_retryable_not_permanent_ingestion_rejection() {
    use communication_hub::daemon;
    let t = TempDir::new().unwrap();
    let cfg = config(t.path());
    let task = tokio::spawn(daemon::run(cfg.clone(), true));
    for _ in 0..50 {
        if cfg.socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    approve_room(&cfg, &event());
    // Make a valid DB temporarily unable to accept writes.
    let db = rusqlite::Connection::open(cfg.state.join("hub.sqlite3")).unwrap();
    db.execute_batch("BEGIN EXCLUSIVE;").unwrap();
    let response = daemon::request(&cfg.socket, json!({"method":"ingest","event":event()}))
        .await
        .unwrap();
    assert_eq!(response["error"], "service_temporarily_unavailable");
    db.execute_batch("ROLLBACK;").unwrap();
    assert_eq!(
        daemon::request(&cfg.socket, json!({"method":"ingest","event":event()}))
            .await
            .unwrap()["result"]["status"],
        "queued"
    );
    task.abort();
}

#[test]
fn call_logs_survive_payload_removal_and_respect_body_opt_in() {
    let t = TempDir::new().unwrap();
    let mut s = Store::open(t.path().join("state")).unwrap();
    let mut e = event();
    s.enqueue(&e).unwrap();
    s.finish_event(&e.key(), "held", "fixture").unwrap();
    assert_eq!(
        s.calls(None, None, 10).unwrap()["items"][0]["body"],
        Value::Null
    );
    s.store_bodies = true;
    e.id = "with-body".into();
    e.body = "[유이] <script>untrusted</script>".into();
    s.enqueue(&e).unwrap();
    s.finish_event(&e.key(), "sent_verified", "").unwrap();
    let records = s.calls(Some(&e.conversation.key()), None, 10).unwrap();
    assert_eq!(records["items"].as_array().unwrap().len(), 2);
    assert_eq!(records["items"][0]["body"], e.body);
    assert_eq!(records["items"][0]["trigger_kind"], "followup_tag");
    assert!(!s.enqueue(&e).unwrap());
    assert_eq!(
        s.calls(None, None, 100).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn historical_log_does_not_invent_original_body_or_trigger() {
    let t = TempDir::new().unwrap();
    let s = Store::open(t.path().join("state")).unwrap();
    let e = event();
    s.save_session(&e.conversation, "persisted").unwrap();
    s.save_route(&e.conversation, "fixture-room").unwrap();
    s.db()
        .unwrap()
        .execute(
            "INSERT INTO events VALUES('legacy','','sent_verified',NULL,1,2)",
            [],
        )
        .unwrap();
    let p = Plan {
        reply: "[System-유이] : fixture".into(),
        bundle_id: None,
        sticker_id: None,
    };
    s.prepare("receipt", &e, "final", &p).unwrap();
    s.db()
        .unwrap()
        .execute(
            "UPDATE deliveries SET event_key='legacy',receipt=?",
            [json!({"verified_chat_name":"fixture-room","status":"sent_verified"}).to_string()],
        )
        .unwrap();
    let logs = s.calls(Some(&e.conversation.key()), None, 10).unwrap();
    let r = &logs["items"][0];
    assert_eq!(r["historical_metadata_missing"], true);
    assert!(r["body"].is_null() && r["trigger_kind"].is_null());
    assert_eq!(r["conversation_key"], e.conversation.key());
}

#[test]
fn dashboard_auth_denies_missing_token_and_other_origins() {
    use axum::http::{HeaderMap, header};
    let token = "a".repeat(64);
    let origin = "http://127.0.0.1:43197";
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, "127.0.0.1:43197".parse().unwrap());
    assert!(!communication_hub::dashboard::authorized(
        &headers, &token, origin
    ));
    headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {token}").parse().unwrap(),
    );
    assert!(communication_hub::dashboard::authorized(
        &headers, &token, origin
    ));
    headers.insert(header::ORIGIN, "https://untrusted.example".parse().unwrap());
    assert!(!communication_hub::dashboard::authorized(
        &headers, &token, origin
    ));
    headers.remove(header::ORIGIN);
    headers.insert(header::HOST, "attacker.example:43197".parse().unwrap());
    assert!(!communication_hub::dashboard::authorized(
        &headers, &token, origin
    ));
}

#[test]
fn source_heartbeat_is_not_mistaken_for_online_forever() {
    use communication_hub::dashboard::source_health;
    let v = json!({"status":"watching_kakao_only","received_at":100});
    assert_eq!(source_health(true, &v, 102.0)["status"], "online");
    assert_eq!(source_health(true, &v, 111.0)["status"], "offline");
    assert_eq!(source_health(false, &v, 102.0)["status"], "disabled");
    assert_eq!(
        source_health(
            true,
            &json!({"status":"permission_or_storage_blocked","received_at":100}),
            102.0
        )["status"],
        "blocked"
    );
}

#[tokio::test]
async fn dashboard_private_api_requires_auth_and_serves_no_token_in_html() {
    use communication_hub::dashboard::{Board, router};
    use http_body_util::BodyExt;
    use std::sync::atomic::AtomicBool;
    use tower::ServiceExt;
    let t = TempDir::new().unwrap();
    let cfg = config(t.path());
    let store = Store::open(cfg.state.clone()).unwrap();
    let e = event();
    store
        .save_session(&e.conversation, "sample-thread")
        .unwrap();
    let token = "b".repeat(64);
    let board = Board {
        cfg: Arc::new(cfg),
        store,
        active: Arc::new(AtomicBool::new(false)),
        sending: Arc::new(AtomicBool::new(false)),
        processing: Arc::new(AtomicBool::new(false)),
        backend: Arc::new(tokio::sync::RwLock::new(json!({}))),
        token: token.clone(),
        origin: "http://127.0.0.1:43197".into(),
    };
    let app = router(board);
    let request = |path: &str, auth: bool| {
        let mut r = axum::http::Request::builder()
            .uri(path)
            .header("host", "127.0.0.1:43197");
        if auth {
            r = r.header("authorization", format!("Bearer {token}"));
        }
        r.body(axum::body::Body::empty()).unwrap()
    };
    let r = app
        .clone()
        .oneshot(request("/api/snapshot", false))
        .await
        .unwrap();
    assert_eq!(r.status(), axum::http::StatusCode::UNAUTHORIZED);
    let r = app.clone().oneshot(request("/", false)).await.unwrap();
    let html =
        String::from_utf8(r.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(!html.contains(&token) && !html.contains("sample-thread"));
    let r = app.oneshot(request("/api/snapshot", true)).await.unwrap();
    assert_eq!(r.headers()["cache-control"], "no-store");
    let v: Value =
        serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
    // Calls are stateless: legacy thread bindings are no longer part of the board.
    assert!(v["bindings"].is_null());
    assert!(!v.to_string().contains("sample-thread"));
}
#[tokio::test]
async fn dashboard_room_changes_need_the_token_and_take_effect() {
    use communication_hub::dashboard::{Board, router};
    use http_body_util::BodyExt;
    use std::sync::atomic::AtomicBool;
    use tower::ServiceExt;
    let t = TempDir::new().unwrap();
    let cfg = config(t.path());
    let store = Store::open(cfg.state.clone()).unwrap();
    let e = event();
    store.note_room_seen(&e.conversation, "fixture").unwrap();
    let token = "c".repeat(64);
    let app = router(Board {
        cfg: Arc::new(cfg),
        store: store.clone(),
        active: Arc::new(AtomicBool::new(false)),
        sending: Arc::new(AtomicBool::new(false)),
        processing: Arc::new(AtomicBool::new(false)),
        backend: Arc::new(tokio::sync::RwLock::new(json!({}))),
        token: token.clone(),
        origin: "http://127.0.0.1:43197".into(),
    });
    let post = |path: &str, body: Value, auth: bool| {
        let mut r = axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header("host", "127.0.0.1:43197")
            .header("content-type", "application/json");
        if auth {
            r = r.header("authorization", format!("Bearer {token}"));
        }
        r.body(axum::body::Body::from(body.to_string())).unwrap()
    };
    let change = json!({"key":e.conversation.key(),"approved":true,"yui":true,"yumi":false});
    let r = app
        .clone()
        .oneshot(post("/api/rooms", change.clone(), false))
        .await
        .unwrap();
    assert_eq!(r.status(), axum::http::StatusCode::UNAUTHORIZED);
    assert_eq!(
        store.room(&e.conversation).unwrap().unwrap()["approved"],
        false
    );
    let r = app
        .clone()
        .oneshot(post("/api/rooms", change, true))
        .await
        .unwrap();
    assert_eq!(r.status(), axum::http::StatusCode::OK);
    let room = store.room(&e.conversation).unwrap().unwrap();
    assert_eq!(
        (room["approved"].clone(), room["yumi"].clone()),
        (json!(true), json!(false))
    );
    let r = app
        .clone()
        .oneshot(post(
            "/api/settings",
            json!({"answer_unapproved_rooms":true}),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), axum::http::StatusCode::OK);
    assert!(store.answer_unapproved_rooms().unwrap());
    let get = axum::http::Request::builder()
        .uri("/api/rooms")
        .header("host", "127.0.0.1:43197")
        .header("authorization", format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let r = app.oneshot(get).await.unwrap();
    let v: Value =
        serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(v["rooms"][0]["title"], "fixture");
    assert_eq!(v["answer_unapproved_rooms"], true);
}

#[tokio::test]
async fn actual_rpc_usage_limit_produces_exact_fallback() {
    let t = TempDir::new().unwrap();
    let c = config(t.path());
    let s = Store::open(c.state.clone()).unwrap();
    let listener = tokio::net::UnixListener::bind(&c.app_server_socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) = ws.next().await {
            let v: Value = serde_json::from_str(&text).unwrap();
            if v.get("id").is_none() {
                continue;
            }
            let response = if v["method"] == "turn/start" {
                json!({"id":v["id"],"error":{"code":-32000,"data":{"codexErrorInfo":"usageLimitExceeded"}}})
            } else if v["method"] == "thread/start" {
                json!({"id":v["id"],"result":{"thread":{"id":"quota-thread"}}})
            } else {
                json!({"id":v["id"],"result":{}})
            };
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                response.to_string().into(),
            ))
            .await
            .unwrap();
        }
    });
    let result = worker::model(&c, &s, &event()).await.unwrap();
    assert_eq!(result["phase"], "usage_limit_fallback");
    assert_eq!(
        result["plan"]["reply"],
        "[System-유이] : 유이는 현재 잠에 들었어요.."
    );
    server.abort();
}

#[tokio::test]
async fn dashboard_backend_check_only_handshakes() {
    use communication_hub::dashboard::{Board, monitor};
    use std::sync::atomic::AtomicBool;
    let t = TempDir::new().unwrap();
    let cfg = config(t.path());
    let store = Store::open(cfg.state.clone()).unwrap();
    store
        .save_session(&event().conversation, "observed")
        .unwrap();
    let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let capture = calls.clone();
    let listener = tokio::net::UnixListener::bind(&cfg.app_server_socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) = ws.next().await {
            let v: Value = serde_json::from_str(&text).unwrap();
            let method = v["method"].as_str().unwrap_or("").to_owned();
            capture.lock().unwrap().push(method.clone());
            if v.get("id").is_none() {
                continue;
            }
            let result = if method == "thread/read" {
                json!({"thread":{"id":"observed","status":{"type":"notLoaded"}}})
            } else {
                json!({})
            };
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"id":v["id"],"result":result}).to_string().into(),
            ))
            .await
            .unwrap();
        }
    });
    let state = Arc::new(tokio::sync::RwLock::new(json!({})));
    let board = Board {
        cfg: Arc::new(cfg),
        store,
        active: Arc::new(AtomicBool::new(false)),
        sending: Arc::new(AtomicBool::new(false)),
        processing: Arc::new(AtomicBool::new(false)),
        backend: state.clone(),
        token: "c".repeat(64),
        origin: "http://127.0.0.1:43197".into(),
    };
    let probe = tokio::spawn(monitor(board));
    for _ in 0..100 {
        if state.read().await["online"] == true {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(state.read().await["online"], true);
    let methods = calls.lock().unwrap();
    assert!(
        methods
            .iter()
            .all(|m| matches!(m.as_str(), "initialize" | "initialized")),
        "{methods:?}"
    );
    probe.abort();
    server.abort();
}

#[test]
fn rejected_calls_are_visible_without_becoming_pending_jobs() {
    let t = TempDir::new().unwrap();
    let mut store = Store::open(t.path().join("state")).unwrap();
    store.store_bodies = true;
    let mut e = event();
    e.body = "[유이] first call without initialization".into();
    store.record_rejected(&e, "room_not_initialized").unwrap();
    assert!(store.claim_next().unwrap().is_none());
    let r = store.calls(None, None, 10).unwrap();
    assert_eq!(r["items"][0]["status"], "rejected");
    assert_eq!(r["items"][0]["reason"], "room_not_initialized");
    assert_eq!(r["items"][0]["body"], e.body);
}

#[tokio::test]
async fn fast_tier_is_forwarded_without_changing_model_or_effort() {
    let t = TempDir::new().unwrap();
    let mut c = config(t.path());
    c.service_tier = Some("priority".into());
    let s = Store::open(c.state.clone()).unwrap();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let server = mock_server(
        &c.app_server_socket,
        calls.clone(),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    worker::model(&c, &s, &event()).await.unwrap();
    let calls = calls.lock().unwrap();
    for method in ["thread/start", "turn/start"] {
        let params = &calls.iter().find(|v| v["method"] == method).unwrap()["params"];
        assert_eq!(params["serviceTier"], "priority");
        assert_eq!(params["model"], worker::MODEL);
    }
    assert_eq!(
        calls.iter().find(|v| v["method"] == "turn/start").unwrap()["params"]["effort"],
        "medium"
    );
    server.abort();
}

fn expression_fixture(t: &TempDir) -> (Config, Vec<u8>) {
    use zip::write::SimpleFileOptions;
    let mut cfg = config(t.path());
    let assets = t.path().join("assets");
    std::fs::create_dir(&assets).unwrap();
    let png = b"\x89PNG\r\n\x1a\nfixture".to_vec();
    let gif = b"GIF89afixture".to_vec();
    let mut zip = zip::ZipWriter::new(std::fs::File::create(assets.join("pictures.zip")).unwrap());
    zip.start_file("original.png", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(&png).unwrap();
    zip.start_file("original.gif", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(&gif).unwrap();
    zip.start_file("restricted.png", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"\x89PNG\r\n\x1a\nrestricted").unwrap();
    zip.finish().unwrap();
    let row = |id: &str, data: &[u8], format: &str, eligible: bool| json!({"id":id,"category":"인사","visual_meaning":"반가운 인사","suitable_situations":["인사"],"avoid_context":["슬픈 소식"],"random_eligible":eligible,"format":format,"sha256":communication_hub::event::digest(data),"archive":"pictures.zip","archive_path":if eligible {format!("original.{}",format.to_lowercase())} else {"restricted.png".into()}});
    std::fs::write(assets.join("catalog.json"),serde_json::to_vec(&json!({"items":[row("one",&png,"PNG",true),row("duplicate",&png,"PNG",true),row("animated",&gif,"GIF",true),row("restricted",b"\x89PNG\r\n\x1a\nrestricted","PNG",false)]})).unwrap()).unwrap();
    cfg.expressions = Some(communication_hub::config::ExpressionConfig {
        catalog: assets.join("catalog.json"),
        emoticons: None,
    });
    (cfg, png)
}
#[test]
fn expression_pick_is_png_only_random_and_respects_room_scoped_cooldown() {
    use communication_hub::{event::digest, expressions};
    let t = TempDir::new().unwrap();
    let (cfg, png) = expression_fixture(&t);
    let s = Store::open(cfg.state.clone()).unwrap();
    let e = event();
    let picks: std::collections::HashSet<String> = (0..64)
        .map(|_| expressions::pick(&cfg, &s, &e).unwrap().unwrap())
        .collect();
    // Fully random over PNGs (restricted included), one entry per image, never the GIF.
    assert!(!picks.contains("animated"));
    assert!(picks.contains("restricted"));
    assert!(picks.contains("one") ^ picks.contains("duplicate"));
    s.reserve_expression("attempt", &e.conversation, &digest(png), "인사")
        .unwrap();
    assert_eq!(
        expressions::pick(&cfg, &s, &e).unwrap().as_deref(),
        Some("restricted")
    );
    s.reserve_expression(
        "attempt-2",
        &e.conversation,
        &digest(b"\x89PNG\r\n\x1a\nrestricted"),
        "인사",
    )
    .unwrap();
    assert_eq!(expressions::pick(&cfg, &s, &e).unwrap(), None);
    let mut other = e.clone();
    other.conversation.id = "other-room".into();
    assert!(expressions::pick(&cfg, &s, &other).unwrap().is_some());
    let mut none = cfg.clone();
    none.expressions = None;
    assert_eq!(expressions::pick(&none, &s, &other).unwrap(), None);
}
#[test]
fn expression_preparation_preserves_bytes_and_never_sends_gif() {
    use communication_hub::{event::digest, expressions};
    let t = TempDir::new().unwrap();
    let (cfg, png) = expression_fixture(&t);
    let key = digest("fixture");
    let result = expressions::prepare(&cfg, &key, "one").unwrap();
    assert_eq!(
        std::fs::read(result["path"].as_str().unwrap()).unwrap(),
        png
    );
    assert!(expressions::prepare(&cfg, &key, "animated").is_err());
    assert!(expressions::prepare(&cfg, &key, "restricted").is_ok());
    assert!(expressions::prepare(&cfg, "../../invalid", "one").is_err());
}
#[test]
fn expression_catalog_rejects_hash_and_path_changes() {
    use communication_hub::{event::digest, expressions};
    let t = TempDir::new().unwrap();
    let (cfg, _) = expression_fixture(&t);
    let path = &cfg.expressions.as_ref().unwrap().catalog;
    let mut catalog: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    catalog["items"][0]["sha256"] = json!("0".repeat(64));
    std::fs::write(path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    assert!(expressions::prepare(&cfg, &digest("fixture"), "one").is_err());
    catalog["items"][0]["archive_path"] = json!("../private.png");
    std::fs::write(path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    assert!(expressions::prepare(&cfg, &digest("fixture"), "one").is_err());
}
#[test]
fn expression_plan_cannot_carry_bundle_and_sticker_together() {
    use communication_hub::expressions;
    let mut p = Plan {
        reply: "[System-유이] : hello".into(),
        bundle_id: None,
        sticker_id: Some("one".into()),
    };
    expressions::validate_plan(&p).unwrap();
    p.bundle_id = Some("bundle".into());
    assert!(expressions::validate_plan(&p).is_err());
    assert!(
        Plan::parse(
            r#"{"reply":"[System-유이] : hello","bundle_id":"bundle","sticker_id":"one"}"#,
            &json!({"bundle":{}})
        )
        .is_err()
    );
}
#[test]
fn sticker_failure_never_makes_a_verified_text_reply_uncertain() {
    use communication_hub::adapters::settle_sticker;
    let sticker = json!({"sticker_id":"one"});
    let mut both = json!({"status":"sent_verified","text_sent":true,"attachment_sent":true});
    assert!(settle_sticker(&mut both, sticker.clone()));
    assert_eq!(both["sticker"]["sent"], true);
    let mut image_lost = json!({"status":"sending","reason":"attachment_delivery_not_observed","text_sent":true,"attachment_sent":false});
    assert!(!settle_sticker(&mut image_lost, sticker.clone()));
    assert_eq!(image_lost["status"], "sent_verified");
    assert_eq!(image_lost["sticker"]["outcome"]["status"], "sending");
    // Text never verified: the native status is kept as is.
    let mut text_lost =
        json!({"status":"sending","reason":"text_enter_delivery_not_observed","text_sent":false});
    assert!(!settle_sticker(&mut text_lost, sticker));
    assert_eq!(text_lost["status"], "sending");
}
#[cfg(unix)]
#[test]
fn expression_staging_rejects_symlink_outside_ipc() {
    use communication_hub::{event::digest, expressions};
    let t = TempDir::new().unwrap();
    let (cfg, _) = expression_fixture(&t);
    std::fs::create_dir(&cfg.kakao.sender_ipc).unwrap();
    let outside = t.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, cfg.kakao.sender_ipc.join("file-jobs")).unwrap();
    assert!(expressions::prepare(&cfg, &digest("fixture"), "one").is_err());
    assert!(std::fs::read_dir(outside).unwrap().next().is_none());
}

#[test]
fn wire_prefix_is_generated_once_for_new_and_legacy_model_output() {
    use communication_hub::event::format_reply;
    assert_eq!(
        format_reply("본문만 작성했어!").unwrap(),
        "[System-유이] : 본문만 작성했어!"
    );
    assert_eq!(
        format_reply("[System-유이] : [System-유이] : 본문").unwrap(),
        "[System-유이] : 본문"
    );
    assert!(format_reply("[System-유이] : ").is_err());
    assert!(format_reply(&"x".repeat(8192)).is_err());
}
#[test]
fn concurrent_ack_intro_suppresses_a_second_intro_in_the_final_reply() {
    let t = TempDir::new().unwrap();
    let c = config(t.path());
    let s = Store::open(c.state.clone()).unwrap();
    let marker = "이 방에는 자기소개를 실제 전송한 기록이 있어";
    let (plain, _, _) = worker::instructions(&c, &s, &event(), false, None).unwrap();
    let (with_ack, _, _) = worker::instructions(&c, &s, &event(), true, None).unwrap();
    assert!(!plain.contains(marker));
    assert!(with_ack.contains(marker));
    // The model is told the runner attaches stickers, so it never picks or mentions one.
    assert!(!plain.contains("sticker_id"));
    // With no pool the model is told to use no face; with one it gets exactly the picked face.
    assert!(plain.contains("특수문자 표정을 넣지 마"));
    let (faced, _, _) = worker::instructions(&c, &s, &event(), false, Some("(˶>⩊<˶)")).unwrap();
    assert!(faced.contains("(˶>⩊<˶) 하나야") && !faced.contains("표정을 넣지 마"));
}
fn emoticon_pool(root: &Path, faces: &[&str]) -> std::path::PathBuf {
    let path = root.join("emoticons.json");
    let items: Vec<Value> = faces.iter().map(|f| json!({"text":f})).collect();
    std::fs::write(&path, serde_json::to_vec(&json!({"items":items})).unwrap()).unwrap();
    path
}
#[test]
fn emoticon_is_picked_by_code_and_appended_only_when_the_model_left_it_out() {
    use communication_hub::expressions;
    let t = TempDir::new().unwrap();
    let mut c = config(t.path());
    assert_eq!(expressions::pick_emoticon(&c, "").unwrap(), None);
    let faces = ["(ෆ˙ᵕ˙ෆ)♡", "(˶>⩊<˶)", "(๑ˊ͈ ꇴ ˋ͈)♡"];
    c.expressions = Some(communication_hub::config::ExpressionConfig {
        catalog: t.path().join("unused.json"),
        emoticons: Some(emoticon_pool(t.path(), &faces)),
    });
    let mut picked = std::collections::HashSet::new();
    for _ in 0..200 {
        let face = expressions::pick_emoticon(&c, "웅웅! 잠시만~(๑ˊ͈ ꇴ ˋ͈)♡")
            .unwrap()
            .unwrap();
        // The room's latest face is skipped while another one is available.
        assert_ne!(face, faces[2]);
        picked.insert(face);
    }
    assert_eq!(picked.len(), 2);
    assert_eq!(
        expressions::with_emoticon("안녕!", Some(faces[1])),
        "안녕! (˶>⩊<˶)"
    );
    assert_eq!(
        expressions::with_emoticon("안녕 (˶>⩊<˶) 반가워!", Some(faces[1])),
        "안녕 (˶>⩊<˶) 반가워!"
    );
    assert_eq!(expressions::with_emoticon("안녕!", None), "안녕!");
    // A malformed pool is refused rather than half-used.
    std::fs::write(
        t.path().join("emoticons.json"),
        br#"{"items":[{"text":"a\nb"}]}"#,
    )
    .unwrap();
    assert!(expressions::pick_emoticon(&c, "").is_err());
}

fn fake_claude(dir: &Path, result: &str) -> communication_hub::config::YumiConfig {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("fake-claude");
    // Records its arguments and the single message it receives, then answers like stream-json.
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{args}\"\nIFS= read -r line\nprintf '%s\\n' \"$line\" > \"{input}\"\necho '{{\"type\":\"system\",\"subtype\":\"init\"}}'\necho '{result}'\n",
            args = dir.join("args.txt").display(),
            input = dir.join("input.txt").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let persona = dir.join("persona.md");
    std::fs::write(&persona, "유미 페르소나 fixture").unwrap();
    communication_hub::config::YumiConfig {
        claude_bin: bin,
        persona,
        model: "claude-sonnet-5-5".into(),
        effort: "low".into(),
    }
}
fn yumi_event() -> Event {
    let mut e = event();
    e.body = "@[유미] 안녕!".into();
    e.agent = communication_hub::event::Agent::Yumi;
    e
}
#[tokio::test]
async fn yumi_call_runs_one_shot_isolated_claude_with_her_prefix() {
    let t = TempDir::new().unwrap();
    let mut c = config(t.path());
    c.yumi = Some(fake_claude(
        t.path(),
        r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"reply\":\"안녕! 나는 오빠의 여동생 유미야\",\"bundle_id\":null}"}"#,
    ));
    c.expressions = Some(communication_hub::config::ExpressionConfig {
        catalog: t.path().join("unused.json"),
        emoticons: Some(emoticon_pool(t.path(), &["(˶>⩊<˶)"])),
    });
    let s = Store::open(c.state.clone()).unwrap();
    let result = worker::model(&c, &s, &yumi_event()).await.unwrap();
    // The fake model ignored the picked face, so the hub appended it.
    assert_eq!(
        result["plan"]["reply"],
        "[System-유미] : 안녕! 나는 오빠의 여동생 유미야 (˶>⩊<˶)"
    );
    let args = std::fs::read_to_string(t.path().join("args.txt")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    for (flag, value) in [
        ("--setting-sources", ""),
        ("--tools", ""),
        ("--model", "claude-sonnet-5-5"),
        ("--effort", "low"),
        ("--input-format", "stream-json"),
    ] {
        let i = args.iter().position(|a| *a == flag).unwrap();
        assert_eq!(args[i + 1], value, "{flag}");
    }
    for flag in [
        "--strict-mcp-config",
        "--no-session-persistence",
        "--disable-slash-commands",
    ] {
        assert!(args.contains(&flag), "{flag}");
    }
    let prompt = args[args.iter().position(|a| *a == "--system-prompt").unwrap() + 1..].join("\n");
    assert!(prompt.contains("유미 페르소나 fixture") && prompt.contains("도구가 없어"));
    let input: Value =
        serde_json::from_str(&std::fs::read_to_string(t.path().join("input.txt")).unwrap())
            .unwrap();
    let content = input["message"]["content"].as_str().unwrap();
    assert!(content.contains("untrusted_third_party_data") && content.contains("yumi_introduced"));
    assert!(content.contains(r#""emoticon":"(˶>⩊<˶)""#));
}
#[tokio::test]
async fn yumi_usage_limit_becomes_her_sleeping_reply() {
    let t = TempDir::new().unwrap();
    let mut c = config(t.path());
    c.yumi = Some(fake_claude(
        t.path(),
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"Claude AI usage limit reached"}"#,
    ));
    let s = Store::open(c.state.clone()).unwrap();
    let result = worker::model(&c, &s, &yumi_event()).await.unwrap();
    assert_eq!(result["phase"], "usage_limit_fallback");
    assert_eq!(
        result["plan"]["reply"],
        "[System-유미] : 유미는 현재 잠에 들었어요.."
    );
}
#[test]
fn yumi_policy_swaps_the_speaker_and_refuses_an_ambiguous_source() {
    use communication_hub::event::Agent;
    let t = TempDir::new().unwrap();
    let c = config(t.path());
    std::fs::write(
        &c.contact_skill,
        "name: contact-other\n나는 오빠의 여동생 유이야. @[유이]로 불러줘",
    )
    .unwrap();
    let (yumi, _) = worker::contact_policy_for(&c, Agent::Yumi).unwrap();
    assert_eq!(
        yumi,
        "name: contact-other\n나는 오빠의 여동생 유미야. @[유미]로 불러줘"
    );
    std::fs::write(&c.contact_skill, "name: contact-other\n유이와 유미").unwrap();
    assert!(worker::contact_policy_for(&c, Agent::Yumi).is_err());
    assert!(worker::contact_policy_for(&c, Agent::Yui).is_ok());
}
#[test]
fn each_sister_has_her_own_tag_prefix_key_and_intro() {
    use communication_hub::event::{Agent, format_reply_as};
    let e = event();
    let mut both = e.clone();
    both.body = "@[유이] @[유미] 둘 다 안녕".into();
    assert_eq!(both.called_agents(), vec![Agent::Yui, Agent::Yumi]);
    // Yui keeps her historic key; Yumi's answer to the same message is a separate event.
    assert_eq!(both.for_agent(Agent::Yui).key(), both.key());
    assert_ne!(both.for_agent(Agent::Yumi).key(), both.key());
    assert!(both.for_agent(Agent::Yumi).validate(now(), false).is_ok());
    let mut yumi_only = e.clone();
    yumi_only.body = "[유미] 태그 없는 첫 호출".into();
    assert!(
        yumi_only
            .for_agent(Agent::Yumi)
            .validate(now(), false)
            .is_err()
    );
    assert!(
        yumi_only
            .for_agent(Agent::Yumi)
            .validate(now(), true)
            .is_ok()
    );
    let mut echo = e.clone();
    echo.body = "[System-유미] : [유이]를 불러줘!".into();
    assert!(echo.for_agent(Agent::Yui).validate(now(), true).is_err());
    assert_eq!(
        format_reply_as(Agent::Yumi, "[System-유미] : 안녕").unwrap(),
        "[System-유미] : 안녕"
    );
    let t = TempDir::new().unwrap();
    let s = Store::open(t.path().join("state")).unwrap();
    s.note_intro(
        &e.conversation,
        Agent::Yumi,
        "[System-유미] : 나는 오빠의 여동생 유미야",
        &json!({"status":"sent_verified"}),
    )
    .unwrap();
    assert!(s.introduced(&e.conversation, Agent::Yumi).unwrap());
    assert!(!s.introduced(&e.conversation, Agent::Yui).unwrap());
}
#[tokio::test]
async fn one_message_calling_both_sisters_queues_one_event_each() {
    use communication_hub::daemon;
    let t = TempDir::new().unwrap();
    let mut cfg = config(t.path());
    cfg.yumi = Some(communication_hub::config::YumiConfig {
        claude_bin: t.path().join("missing-claude"),
        persona: t.path().join("missing-persona.md"),
        model: "claude-sonnet-5-5".into(),
        effort: "low".into(),
    });
    let service = tokio::spawn(daemon::run(cfg.clone(), true));
    for _ in 0..40 {
        if cfg.socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let mut e = event();
    e.body = "@[유이] @[유미] 둘 다 안녕".into();
    approve_room(&cfg, &e);
    let r = daemon::request(&cfg.socket, json!({"method":"ingest","event":e}))
        .await
        .unwrap();
    let events = r["result"]["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|x| x["status"] == "queued"));
    assert_eq!(events[1]["agent"], "yumi");
    // A name without the exact tag is not a call, but it is recorded as a near miss.
    let mut near = event();
    near.id = "near-miss".into();
    near.body = "유미야 안녕".into();
    let r = daemon::request(&cfg.socket, json!({"method":"ingest","event":near}))
        .await
        .unwrap();
    assert_eq!(r["ok"], false);
    let calls = Store::open(cfg.state.clone())
        .unwrap()
        .calls(None, None, 10)
        .unwrap();
    assert!(
        calls.to_string().contains("name_without_exact_tag"),
        "{calls}"
    );
    service.abort();
}

#[tokio::test]
async fn unapproved_rooms_wait_for_the_owner_and_each_sister_can_be_switched_off() {
    use communication_hub::daemon;
    let t = TempDir::new().unwrap();
    let mut cfg = config(t.path());
    cfg.yumi = Some(communication_hub::config::YumiConfig {
        claude_bin: t.path().join("missing-claude"),
        persona: t.path().join("missing-persona.md"),
        model: "claude-sonnet-5-5".into(),
        effort: "low".into(),
    });
    let service = tokio::spawn(daemon::run(cfg.clone(), true));
    for _ in 0..40 {
        if cfg.socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let ingest = |id: &str, body: &str| {
        let mut e = event();
        e.id = id.into();
        e.body = body.into();
        daemon::request(&cfg.socket, json!({"method":"ingest","event":e}))
    };
    // A first call in an unknown room is held and the room is listed for approval.
    assert_eq!(ingest("first", "@[유이] 안녕").await.unwrap()["ok"], false);
    let s = Store::open(cfg.state.clone()).unwrap();
    let rooms = s.rooms().unwrap();
    assert_eq!(rooms.len(), 1);
    assert_eq!(rooms[0]["approved"], false);
    assert_eq!(rooms[0]["title"], "fixture");
    assert!(
        s.calls(None, None, 10)
            .unwrap()
            .to_string()
            .contains("room_pending_approval")
    );
    // Approved with Yumi switched off: Yui answers, Yumi's half of the call is held.
    let key = event().conversation.key();
    assert!(s.update_room(&key, true, true, false).unwrap());
    let r = ingest("second", "@[유이] @[유미] 둘 다").await.unwrap();
    assert_eq!(r["result"]["events"][0]["status"], "queued");
    assert_eq!(r["result"]["events"][1]["status"], "rejected");
    assert!(
        s.calls(None, None, 10)
            .unwrap()
            .to_string()
            .contains("sister_disabled_in_room")
    );
    // The owner may also choose to answer rooms that were never approved.
    let mut other = event();
    other.conversation.id = "room-two".into();
    other.id = "third".into();
    s.set_answer_unapproved_rooms(true).unwrap();
    let r = daemon::request(&cfg.socket, json!({"method":"ingest","event":other}))
        .await
        .unwrap();
    assert_eq!(r["result"]["status"], "queued");
    service.abort();
}
#[test]
fn rooms_in_use_before_the_registry_start_approved_and_verification_is_recorded() {
    let t = TempDir::new().unwrap();
    let state = t.path().join("state");
    let e = event();
    {
        let s = Store::open(state.clone()).unwrap();
        s.save_route(&e.conversation, "예전 방").unwrap();
        // Simulate a database from before the registry: no migration marker, no room rows.
        let db = rusqlite::Connection::open(state.join("hub.sqlite3")).unwrap();
        db.execute_batch("DELETE FROM rooms; DELETE FROM migrations WHERE name='rooms-v1';")
            .unwrap();
    }
    let s = Store::open(state).unwrap();
    let room = s.room(&e.conversation).unwrap().unwrap();
    assert_eq!(room["approved"], true);
    assert_eq!(room["title"], "예전 방");
    s.note_room_verified(&e.conversation, 321).unwrap();
    assert_eq!(
        s.room(&e.conversation).unwrap().unwrap()["verified_rows"],
        321
    );
    assert!(
        s.update_room(&e.conversation.key(), false, true, true)
            .unwrap()
    );
    assert_eq!(s.room(&e.conversation).unwrap().unwrap()["approved"], false);
}
#[test]
fn rooms_added_by_name_bind_on_first_call_and_removal_returns_them_to_pending() {
    let t = TempDir::new().unwrap();
    let s = Store::open(t.path().join("state")).unwrap();
    let e = event();
    s.add_room_by_name("kakao", "owner", "fixture", true, false, 186)
        .unwrap();
    let rooms = s.rooms().unwrap();
    assert_eq!(rooms[0]["conversation_id"], "name:fixture");
    assert_eq!(rooms[0]["approved"], true);
    assert!(s.room(&e.conversation).unwrap().is_none());
    // The first notification from a room with that title binds the registration to its real ID.
    s.note_room_seen(&e.conversation, "fixture").unwrap();
    let room = s.room(&e.conversation).unwrap().unwrap();
    assert_eq!(room["approved"], true);
    assert_eq!(room["yumi"], false);
    assert_eq!(room["verified_rows"], 186);
    assert_eq!(s.rooms().unwrap().len(), 1);
    // Removed rooms come back unapproved on their next call.
    assert!(s.remove_room(&e.conversation.key()).unwrap());
    s.note_room_seen(&e.conversation, "fixture").unwrap();
    assert_eq!(s.room(&e.conversation).unwrap().unwrap()["approved"], false);
    // Adding a name that is already known under its real ID approves it in place.
    s.add_room_by_name("kakao", "owner", "fixture", true, true, 190)
        .unwrap();
    let rooms = s.rooms().unwrap();
    assert_eq!(rooms.len(), 1);
    assert_eq!(rooms[0]["conversation_id"], "room1");
    assert_eq!(rooms[0]["approved"], true);
    assert_eq!(rooms[0]["verified_rows"], 190);
}
#[test]
fn context_reset_hides_earlier_exchanges_and_approval_initializes_the_room() {
    let t = TempDir::new().unwrap();
    let mut s = Store::open(t.path().join("state")).unwrap();
    s.store_bodies = true;
    let e = event();
    assert!(!s.initialized(&e.conversation).unwrap());
    s.note_room_seen(&e.conversation, "fixture").unwrap();
    assert!(
        s.update_room(&e.conversation.key(), true, true, true)
            .unwrap()
    );
    assert!(s.initialized(&e.conversation).unwrap());
    let answered = |id: &str, occurred: f64| {
        let mut call = event();
        call.id = id.into();
        call.occurred_at = occurred;
        call.body = format!("@[유이] {id}");
        s.enqueue(&call).unwrap();
        let plan = Plan {
            reply: format!("[System-유이] : {id} 답"),
            bundle_id: None,
            sticker_id: None,
        };
        s.prepare(&call.key(), &call, "final", &plan).unwrap();
        s.complete_delivery(&call.key(), &json!({"status":"sent_verified"}))
            .unwrap();
    };
    answered("before", now() - 60.0);
    assert_eq!(s.recent_exchanges(&e.conversation, 6).unwrap().len(), 1);
    assert!(s.reset_room_context(&e.conversation.key()).unwrap());
    assert!(s.recent_exchanges(&e.conversation, 6).unwrap().is_empty());
    answered("after", now() + 1.0);
    let recent = s.recent_exchanges(&e.conversation, 6).unwrap();
    assert_eq!(recent.len(), 1);
    assert!(recent[0].0.contains("after"));
}
#[tokio::test]
async fn busy_notice_is_a_fixed_reply_without_a_model() {
    use communication_hub::event::Agent;
    let t = TempDir::new().unwrap();
    let mut c = config(t.path());
    c.yumi = Some(fake_claude(
        t.path(),
        r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"reply\":\"unused\",\"bundle_id\":null}"}"#,
    ));
    let s = Store::open(c.state.clone()).unwrap();
    let e = event();
    approve_room(&c, &e);
    // With sending off the notice is journaled but not sent; its text needs no model.
    let receipt = worker::notify_busy(&c, &s, &e.for_agent(Agent::Yumi))
        .await
        .unwrap();
    assert_eq!(receipt["status"], "prepared_not_sent");
    let key = communication_hub::event::digest(format!("busy:{}", e.for_agent(Agent::Yumi).key()));
    assert!(s.has_delivery(&key).unwrap());
    let db = rusqlite::Connection::open(c.state.join("hub.sqlite3")).unwrap();
    let (phase, plan): (String, String) = db
        .query_row(
            "SELECT phase,plan FROM deliveries WHERE key=?",
            [&key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(phase, "busy");
    let reply: Value = serde_json::from_str(&plan).unwrap();
    let reply = reply["reply"].as_str().unwrap();
    assert!(
        worker::BUSY_TEXTS
            .iter()
            .any(|text| reply == format!("[System-유미] : {text}")),
        "{reply}"
    );
}
#[tokio::test]
async fn open_room_reports_resolve_by_title_and_never_double_a_notified_message() {
    use communication_hub::daemon;
    let t = TempDir::new().unwrap();
    let mut cfg = config(t.path());
    cfg.kakao.watch_open_room = true;
    let service = tokio::spawn(daemon::run(cfg.clone(), true));
    for _ in 0..40 {
        if cfg.socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    approve_room(&cfg, &event());
    let open = |title: &str, body: &str, observation: &str| {
        daemon::request(
            &cfg.socket,
            json!({"method":"ingest_kakao","event":{"source":"kakao_open_room","chat_name":title,"body":body,"occurred_at":now(),"observation_id":observation}}),
        )
    };
    let notified = |id: &str, body: &str| {
        let mut e = event();
        e.id = id.into();
        e.body = body.into();
        daemon::request(&cfg.socket, json!({"method":"ingest","event":e}))
    };
    // A call seen in the window in front reaches the room registered under that title.
    let r = open("fixture", "@[유이] 보고 있을 때", "0f5e2c1a-0001")
        .await
        .unwrap();
    assert_eq!(r["result"]["status"], "queued", "{r}");
    let calls = Store::open(cfg.state.clone())
        .unwrap()
        .calls(None, None, 10)
        .unwrap()
        .to_string();
    assert!(
        calls.contains("room1") && calls.contains("open:0f5e2c1a-0001"),
        "{calls}"
    );
    // The same message reported again by the other path is one message, in either order.
    let r = notified("n1", "@[유이] 보고 있을 때").await.unwrap();
    assert_eq!(r["result"]["status"], "duplicate", "{r}");
    assert_eq!(
        notified("n2", "@[유이] 알림 먼저").await.unwrap()["result"]["status"],
        "queued"
    );
    let r = open("fixture", "@[유이] 알림 먼저", "0f5e2c1a-0002")
        .await
        .unwrap();
    assert_eq!(r["result"]["status"], "duplicate", "{r}");
    // Two different messages are never merged, and an unregistered title is no call.
    let r = open("fixture", "@[유이] 다른 말", "0f5e2c1a-0003")
        .await
        .unwrap();
    assert_eq!(r["result"]["status"], "queued", "{r}");
    assert_eq!(
        open("unknown room", "@[유이] 누구", "0f5e2c1a-0004")
            .await
            .unwrap()["ok"],
        false
    );
    // Heartbeats from the watch land in their own status file.
    daemon::request(
        &cfg.socket,
        json!({"method":"ingest_kakao","event":{"kind":"open_room_status","status":"watching_focused_room","at":now()}}),
    )
    .await
    .unwrap();
    let status = daemon::request(&cfg.socket, json!({"method":"status"}))
        .await
        .unwrap();
    assert_eq!(
        status["result"]["kakao_open_room"]["status"],
        "watching_focused_room"
    );
    assert_eq!(status["result"]["kakao_source"], json!({}));
    service.abort();
}
#[tokio::test]
async fn open_room_reports_are_refused_unless_the_watch_is_enabled() {
    use communication_hub::daemon;
    let t = TempDir::new().unwrap();
    let cfg = config(t.path());
    let service = tokio::spawn(daemon::run(cfg.clone(), true));
    for _ in 0..40 {
        if cfg.socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    approve_room(&cfg, &event());
    let r = daemon::request(
        &cfg.socket,
        json!({"method":"ingest_kakao","event":{"source":"kakao_open_room","chat_name":"fixture","body":"@[유이] 안녕","occurred_at":now(),"observation_id":"0f5e2c1a-0009"}}),
    )
    .await
    .unwrap();
    assert_eq!(r["ok"], false);
    service.abort();
}
