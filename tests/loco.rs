use communication_hub::{
    config::{Config, LocoMode, atomic},
    daemon::ingest_loco,
    event::{Agent, Conversation, Event, LOCO_SOURCE, Plan, now},
    loco,
    store::Store,
    worker,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    sync::Notify,
};

fn fixture(mode: &str) -> (TempDir, Config, Store, Event) {
    let t = TempDir::new().unwrap();
    let p = t.path();
    std::fs::write(
        p.join("policy.md"),
        "name: contact-other\nFixture public sharing policy\n",
    )
    .unwrap();
    let cfg: Config = serde_json::from_value(json!({"state":p.join("state"),"socket":p.join("hub.sock"),
        "app_server_socket":p.join("app.sock"),"contact_skill":p.join("policy.md"),"lookup_workdir":p,
        "external_auto_send":true,"kakao":{"enabled":true,"account":"owner","legacy_state":p.join("legacy"),
        "receiver_app":p.join("receiver.app"),"sender_app":p.join("sender.app"),"sender_ipc":p.join("ipc"),
        "loco":{"socket":p.join("loco.sock"),"expected_user_id":"123","mode":mode,"rooms":{"registered-room":"456"}}}})).unwrap();
    let store = Store::open(cfg.state.clone()).unwrap();
    let event = Event {
        conversation: Conversation {
            provider: "kakao".into(),
            account: "owner".into(),
            id: "registered-room".into(),
        },
        id: "fixture".into(),
        body: "@[유이] inspect fixture".into(),
        title: "Fixture".into(),
        occurred_at: now(),
        source: LOCO_SOURCE.into(),
        metadata: json!({"approval_epoch":"1"}),
        agent: Agent::Yui,
    };
    store
        .note_room_seen(&event.conversation, &event.title)
        .unwrap();
    store
        .update_room(&event.conversation.key(), true, true, true)
        .unwrap();
    (t, cfg, store, event)
}
fn raw() -> Value {
    json!({"approval_epoch":"1","mode":"active","user_id":"123","chat_id":"456","log_id":"789","author_id":"123", "body":"@[유이] inspect fixture","sent_at":now(),"title":"Fixture"})
}
fn sent() -> Value {
    json!({"status":"sent_verified","transport":"loco","user_id":"123","chat_id":"456","text_sent":true,"attachment_sent":false,"input_started":true,"side_effects_started":true,"text_log_id":"900"})
}
fn fake(cfg: &Config, result: Value) -> tokio::task::JoinHandle<Value> {
    let listener = UnixListener::bind(&cfg.kakao.loco.as_ref().unwrap().socket).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(stream);
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        let response = json!({"id":request["id"],"ok":true,"result":result});
        r.get_mut()
            .write_all(format!("{response}\n").as_bytes())
            .await
            .unwrap();
        request
    })
}
#[test]
fn config_is_optional_and_mapping_is_one_to_one() {
    let (_t, mut cfg, _, _) = fixture("shadow");
    cfg.kakao.loco.as_ref().unwrap().validate().unwrap();
    let l = cfg.kakao.loco.as_mut().unwrap();
    l.rooms.insert("second".into(), "456".into());
    assert!(l.validate().is_err());
    l.rooms.remove("second");
    l.expected_user_id = "0".into();
    assert!(l.validate().is_err());
    cfg.kakao.loco = None;
    let mut v = serde_json::to_value(&cfg).unwrap();
    v["kakao"].as_object_mut().unwrap().remove("loco");
    assert!(
        serde_json::from_value::<Config>(v)
            .unwrap()
            .kakao
            .loco
            .is_none()
    );
}
#[test]
fn shadow_is_durable_no_dispatch_and_metadata_only() {
    let (_t, cfg, store, _) = fixture("shadow");
    let mut input = raw();
    input["mode"] = json!("shadow");
    let r = ingest_loco(&cfg, &store, &input, &Notify::new(), true).unwrap();
    assert_eq!(r["status"], "shadow");
    assert_eq!(r["durable"], true);
    assert_eq!(
        ingest_loco(&cfg, &store, &input, &Notify::new(), true).unwrap()["status"],
        "duplicate"
    );
    assert_eq!(
        store
            .db()
            .unwrap()
            .query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .db()
            .unwrap()
            .query_row("SELECT count(*) FROM deliveries", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .db()
            .unwrap()
            .query_row("SELECT count(*) FROM loco_ingress", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}
#[test]
fn ingress_requires_exact_account_existing_mapping_and_keeps_owner_calls() {
    let (_t, cfg, store, e) = fixture("active");
    for (field, value) in [
        ("user_id", "321"),
        ("chat_id", "654"),
        ("log_id", "0"),
        ("author_id", ""),
    ] {
        let mut input = raw();
        input[field] = json!(value);
        assert_eq!(
            ingest_loco(&cfg, &store, &input, &Notify::new(), false).unwrap()["status"],
            "rejected"
        );
    }
    let r = ingest_loco(&cfg, &store, &raw(), &Notify::new(), false).unwrap();
    assert_eq!(
        r["status"], "queued",
        "owner authored tagged messages remain valid calls"
    );
    assert_eq!(
        ingest_loco(&cfg, &store, &raw(), &Notify::new(), false).unwrap()["status"],
        "duplicate"
    );
    store.remove_room(&e.conversation.key()).unwrap();
    let mut input = raw();
    input["log_id"] = json!("790");
    assert_eq!(
        ingest_loco(&cfg, &store, &input, &Notify::new(), false).unwrap()["reason"],
        "loco_unmapped_chat"
    );
}
#[test]
fn non_calls_echoes_and_stale_messages_are_terminal_acknowledgements() {
    let (_t, cfg, store, _) = fixture("active");
    for (i, body) in [
        "normal discussion",
        "[System-유이] @[유이] echo",
        "@[유이] old call",
    ]
    .iter()
    .enumerate()
    {
        let mut input = raw();
        input["log_id"] = json!((1000 + i).to_string());
        input["body"] = json!(body);
        if i == 2 {
            input["sent_at"] = json!(now() - 600.0);
        }
        let r = ingest_loco(&cfg, &store, &input, &Notify::new(), false).unwrap();
        assert!(matches!(r["status"].as_str(), Some("ignored" | "rejected")));
        assert_eq!(r["durable"], true);
        assert_eq!(
            ingest_loco(&cfg, &store, &input, &Notify::new(), false).unwrap()["status"],
            "duplicate"
        );
    }
}
#[tokio::test]
async fn delivery_uses_exact_ids_and_validated_protocol_receipt() {
    let (_t, cfg, store, e) = fixture("active");
    let server = fake(&cfg, sent());
    let receipt = worker::deliver(
        &cfg,
        &store,
        &e,
        &e.key(),
        "final",
        &Plan {
            reply: "Fixture done".into(),
            bundle_id: None,
            sticker_id: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(receipt["status"], "sent_verified");
    let request = server.await.unwrap();
    assert_eq!(request["method"], "send");
    assert_eq!(request["params"]["chat_id"], "456");
    assert_eq!(request["params"]["expected_user_id"], "123");
    assert_eq!(request["params"]["delivery_id"], e.key());
    assert!(!cfg.kakao.sender_ipc.exists());
}
#[test]
fn invalid_or_partial_receipts_do_not_prove_delivery() {
    let (_t, cfg, _, _) = fixture("active");
    let l = cfg.kakao.loco.as_ref().unwrap();
    for (key, value) in [
        ("text_log_id", json!("0")),
        ("text_log_id", json!(900)),
        ("user_id", json!("999")),
        ("chat_id", json!("457")),
        ("text_sent", json!(false)),
    ] {
        let mut r = sent();
        r[key] = value;
        loco::validate_receipt(&mut r, l, "456", false);
        assert_eq!(r["status"], "sending_uncertain");
    }
    let mut r = sent();
    loco::validate_receipt(&mut r, l, "456", true);
    assert_eq!(r["status"], "sending_uncertain");
    let mut r = json!({"status":"held","transport":"loco","user_id":"123","chat_id":"456"});
    loco::validate_receipt(&mut r, l, "456", false);
    assert_eq!(r["status"], "sending_uncertain");
}
#[tokio::test]
async fn disconnection_after_dispatch_is_uncertain_without_ui_fallback() {
    let (_t, cfg, store, e) = fixture("active");
    let listener = UnixListener::bind(&cfg.kakao.loco.as_ref().unwrap().socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(stream);
        let mut text = String::new();
        r.read_line(&mut text).await.unwrap();
    });
    let receipt = worker::deliver(
        &cfg,
        &store,
        &e,
        &e.key(),
        "final",
        &Plan {
            reply: "Fixture".into(),
            bundle_id: None,
            sticker_id: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(receipt["status"], "sending_uncertain");
    server.await.unwrap();
    assert!(!cfg.kakao.sender_ipc.exists());
    assert_eq!(
        worker::deliver(
            &cfg,
            &store,
            &e,
            &e.key(),
            "final",
            &Plan {
                reply: "Fixture".into(),
                bundle_id: None,
                sticker_id: None
            }
        )
        .await
        .unwrap()["status"],
        "duplicate"
    );
}
#[tokio::test]
async fn pause_and_room_revocation_hold_before_dispatch() {
    for revoked in [true, false] {
        let (_t, cfg, store, e) = fixture("active");
        if revoked {
            store.remove_room(&e.conversation.key()).unwrap();
        } else {
            atomic(&cfg.state.join("control.json"), &json!({"paused":true})).unwrap();
        }
        let receipt = worker::deliver(
            &cfg,
            &store,
            &e,
            &e.key(),
            "final",
            &Plan {
                reply: "Fixture".into(),
                bundle_id: None,
                sticker_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(receipt["status"], "held");
        assert_eq!(receipt["side_effects_started"], false);
        assert_ne!(receipt["reason"], "loco_unavailable_before_dispatch");
    }
}
#[test]
fn transport_change_invalidates_saved_authorization() {
    let (_t, mut cfg, _, mut e) = fixture("shadow");
    e.metadata["__hub_authorization_stamp"] =
        json!(communication_hub::capabilities::authorization_stamp(&cfg, &e).unwrap());
    cfg.kakao.loco.as_mut().unwrap().mode = LocoMode::Active;
    assert!(communication_hub::daemon::validate_deferred_authorization(&cfg, &e).is_err());
}

#[test]
fn shadow_active_mode_mismatch_never_dispatches() {
    for (mode, wrong) in [("shadow", "active"), ("active", "shadow")] {
        let (_t, cfg, store, _) = fixture(mode);
        let mut input = raw();
        input["mode"] = json!(wrong);
        assert_eq!(
            ingest_loco(&cfg, &store, &input, &Notify::new(), true).unwrap()["reason"],
            "loco_mode_mismatch"
        );
        input.as_object_mut().unwrap().remove("mode");
        assert_eq!(
            ingest_loco(&cfg, &store, &input, &Notify::new(), true).unwrap()["reason"],
            "loco_mode_mismatch"
        );
        assert_eq!(
            store
                .db()
                .unwrap()
                .query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
fn auth_request(e: &Event, component: &str) -> Value {
    json!({"delivery_id":e.key(),"user_id":"123","chat_id":"456","component":component,"approval_epoch":e.metadata["approval_epoch"]})
}
fn stamp(cfg: &Config, e: &mut Event) {
    e.metadata["__hub_authorization_stamp"] =
        json!(communication_hub::capabilities::authorization_stamp(cfg, e).unwrap());
}
#[test]
fn dispatch_authorization_requires_original_sending_lease_and_exact_component() {
    use communication_hub::daemon::authorize_loco;
    let (_t, cfg, store, mut e) = fixture("active");
    let q = auth_request(&e, "text");
    assert_eq!(
        authorize_loco(&cfg, &store, &q, true).unwrap()["reason"],
        "delivery_not_sending"
    );
    stamp(&cfg, &mut e);
    let plan = Plan {
        reply: "[System-유이] : fixture".into(),
        bundle_id: None,
        sticker_id: None,
    };
    store.prepare(&e.key(), &e, "final", &plan).unwrap();
    assert_eq!(
        authorize_loco(&cfg, &store, &q, true).unwrap()["reason"],
        "delivery_not_sending"
    );
    store.claim_delivery(&e.key()).unwrap();
    assert_eq!(
        authorize_loco(&cfg, &store, &q, true).unwrap(),
        json!({"status":"authorized"})
    );
    assert_eq!(
        authorize_loco(&cfg, &store, &auth_request(&e, "attachment"), true).unwrap()["reason"],
        "attachment_not_planned"
    );
    for (key, value) in [
        ("user_id", "124"),
        ("chat_id", "457"),
        ("component", "arbitrary"),
    ] {
        let mut invalid = q.clone();
        invalid[key] = json!(value);
        assert_eq!(
            authorize_loco(&cfg, &store, &invalid, true).unwrap()["status"],
            "held"
        );
    }
    store
        .complete_delivery(&e.key(), &json!({"status":"sending_uncertain"}))
        .unwrap();
    assert_eq!(
        authorize_loco(&cfg, &store, &q, true).unwrap()["reason"],
        "delivery_not_sending"
    );
}
#[test]
fn component_authorization_rechecks_every_current_revocation_boundary() {
    use communication_hub::daemon::authorize_loco;
    for revoke in [
        "pause",
        "sending",
        "room",
        "policy",
        "sister",
        "reset",
        "mapping",
        "mode",
        "external_send",
    ] {
        let (_t, mut cfg, store, mut e) = fixture("active");
        stamp(&cfg, &mut e);
        let plan = Plan {
            reply: "[System-유이] : fixture".into(),
            bundle_id: None,
            sticker_id: Some("fixture-sticker".into()),
        };
        store.prepare(&e.key(), &e, "final", &plan).unwrap();
        store.claim_delivery(&e.key()).unwrap();
        assert_eq!(
            authorize_loco(&cfg, &store, &auth_request(&e, "text"), true).unwrap()["status"],
            "authorized"
        );
        match revoke {
            "pause" => atomic(&cfg.state.join("control.json"), &json!({"paused":true})).unwrap(),
            "room" => {
                store.remove_room(&e.conversation.key()).unwrap();
            }
            "policy" => {
                std::fs::write(&cfg.contact_skill, "name: contact-other\nChanged policy\n").unwrap()
            }
            "sister" => {
                store
                    .update_room(&e.conversation.key(), true, false, true)
                    .unwrap();
            }
            "reset" => {
                store
                    .db()
                    .unwrap()
                    .execute("UPDATE rooms SET context_reset_at=?", [now() + 1.0])
                    .unwrap();
            }
            "mapping" => {
                store
                    .db()
                    .unwrap()
                    .execute(
                        "UPDATE loco_bindings SET deleted=1,epoch=epoch+1 WHERE conversation=?",
                        [e.conversation.key()],
                    )
                    .unwrap();
            }
            "mode" => cfg.kakao.loco.as_mut().unwrap().mode = LocoMode::Shadow,
            "external_send" => cfg.external_auto_send = false,
            _ => {}
        }
        let result = authorize_loco(
            &cfg,
            &store,
            &auth_request(&e, "attachment"),
            revoke != "sending",
        )
        .unwrap();
        assert_eq!(result["status"], "held", "revoke={revoke}");
        assert!(result.get("body").is_none());
    }
}
#[test]
fn controlled_pause_exception_is_read_from_journal_not_request() {
    use communication_hub::daemon::authorize_loco;
    for phase in ["final", "controlled"] {
        let (_t, mut cfg, store, mut e) = fixture("active");
        stamp(&cfg, &mut e);
        store
            .prepare(
                &e.key(),
                &e,
                phase,
                &Plan {
                    reply: "fixture".into(),
                    bundle_id: None,
                    sticker_id: None,
                },
            )
            .unwrap();
        store.claim_delivery(&e.key()).unwrap();
        atomic(&cfg.state.join("control.json"), &json!({"paused":true})).unwrap();
        let mut q = auth_request(&e, "text");
        q["phase"] = json!("controlled");
        assert_eq!(
            authorize_loco(&cfg, &store, &q, false).unwrap()["status"],
            if phase == "controlled" {
                "authorized"
            } else {
                "held"
            }
        );
        cfg.external_auto_send = false;
        assert_eq!(
            authorize_loco(&cfg, &store, &q, false).unwrap()["status"],
            "held"
        );
    }
}

#[test]
fn configuration_rejection_is_not_durable_and_same_message_recovers_after_fix() {
    let (_t, mut cfg, store, _) = fixture("shadow");
    let input = raw();
    let rejected = ingest_loco(&cfg, &store, &input, &Notify::new(), false).unwrap();
    assert_eq!(rejected["reason"], "loco_mode_mismatch");
    assert_ne!(rejected["durable"], true);
    assert_eq!(
        store
            .db()
            .unwrap()
            .query_row("SELECT count(*) FROM loco_ingress", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    cfg.kakao.loco.as_mut().unwrap().mode = LocoMode::Active;
    let accepted = ingest_loco(&cfg, &store, &input, &Notify::new(), false).unwrap();
    assert_eq!(accepted["status"], "queued");
    assert_eq!(accepted["durable"], true);
    let duplicate = ingest_loco(&cfg, &store, &input, &Notify::new(), false).unwrap();
    assert_eq!(duplicate["status"], "duplicate");
    assert_eq!(duplicate["durable"], true);
}
#[test]
fn multi_agent_receipt_is_durable_only_after_outer_journal_commit() {
    let (_t, mut cfg, store, _) = fixture("active");
    cfg.yumi = Some(
        serde_json::from_value(
            json!({"claude_bin":"/fixture/claude", "persona":cfg.contact_skill}),
        )
        .unwrap(),
    );
    let mut input = raw();
    input["body"] = json!("@[유이] @[유미] inspect fixture");
    let accepted = ingest_loco(&cfg, &store, &input, &Notify::new(), false).unwrap();
    assert_eq!(accepted["durable"], true);
    assert_eq!(accepted["events"].as_array().unwrap().len(), 2);
    assert_eq!(
        store
            .db()
            .unwrap()
            .query_row("SELECT count(*) FROM loco_ingress", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}
