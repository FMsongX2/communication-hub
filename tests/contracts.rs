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
        kakao: KakaoConfig {
            enabled: true,
            account: "owner".into(),
            legacy_state: root.join("legacy"),
            receiver_app: root.join("receiver.app"),
            sender_app: root.join("sender.app"),
            sender_ipc: root.join("ipc"),
        },
    }
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
    assert!(s.introduced(&e.conversation).unwrap());
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
    assert!(Plan::parse("unprefixed", &json!({})).is_err());
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
    bundle_fixture(&c, "67d39f5c/manifest.json", b"{\"ok\":true}");
    let artifact = attachments::prepare(&c, &e, &e.key(), "bundle").unwrap();
    let mut z =
        zip::ZipArchive::new(std::fs::File::open(artifact["path"].as_str().unwrap()).unwrap())
            .unwrap();
    let mut data = Vec::new();
    z.by_name("assign-context/67d39f5c/manifest.json")
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
async fn resumed_session_reloads_updated_policy() {
    let t = TempDir::new().unwrap();
    let c = config(t.path());
    let s = Store::open(c.state.clone()).unwrap();
    let e = event();
    s.save_session(&e.conversation, "fixture-thread").unwrap();
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
    let resume = calls
        .iter()
        .find(|x| x["method"] == "thread/resume")
        .unwrap();
    assert_eq!(resume["params"]["threadId"], "fixture-thread");
    assert!(
        resume["params"]["developerInstructions"]
            .as_str()
            .unwrap()
            .contains("fresh-policy-marker")
    );
    assert!(!calls.iter().any(|x| x["method"] == "thread/start"));
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
    assert_eq!(v["bindings"][0]["thread_id"], "sample-thread");
    assert_eq!(v["bindings"][0]["call_available"], false);
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
async fn dashboard_observation_never_starts_or_resumes_a_model_session() {
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
    assert!(methods.iter().any(|m| m == "thread/read"));
    assert!(
        methods
            .iter()
            .all(|m| matches!(m.as_str(), "initialize" | "initialized" | "thread/read"))
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
async fn updated_policy_is_injected_as_developer_context_once_per_revision() {
    use communication_hub::rpc::Rpc;
    let t = TempDir::new().unwrap();
    let c = config(t.path());
    let s = Store::open(c.state.clone()).unwrap();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let server = mock_server(
        &c.app_server_socket,
        calls.clone(),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let mut rpc = Rpc::connect(&c.app_server_socket).await.unwrap();
    let e = event();
    assert!(
        worker::apply_policy(
            &mut rpc,
            &s,
            &e,
            "fixture-thread",
            "owner policy revision one"
        )
        .await
        .unwrap()
    );
    assert!(
        !worker::apply_policy(
            &mut rpc,
            &s,
            &e,
            "fixture-thread",
            "owner policy revision one"
        )
        .await
        .unwrap()
    );
    assert!(
        worker::apply_policy(
            &mut rpc,
            &s,
            &e,
            "fixture-thread",
            "owner policy revision two"
        )
        .await
        .unwrap()
    );
    let calls = calls.lock().unwrap();
    let injects: Vec<_> = calls
        .iter()
        .filter(|c| c["method"] == "thread/inject_items")
        .collect();
    assert_eq!(injects.len(), 2);
    assert_eq!(injects[1]["params"]["items"][0]["role"], "developer");
    assert!(
        injects[1]["params"]["items"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("revision two")
    );
    assert!(!calls.iter().any(|c| c["method"] == "turn/start"));
    server.abort();
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
