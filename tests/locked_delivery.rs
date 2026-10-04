use communication_hub::{
    capabilities,
    config::{Config, atomic},
    daemon,
    event::{Agent, Conversation, Event, Plan, now},
    store::Store,
};
use serde_json::{Value, json};
use tempfile::TempDir;

fn fixture() -> (TempDir, Config, Store, Event, Plan) {
    let t = TempDir::new().unwrap();
    let r = t.path();
    std::fs::write(
        r.join("policy.md"),
        "---\nname: contact-other\n---\nDo not share private data.\n",
    )
    .unwrap();
    let c:Config=serde_json::from_value(json!({"state":r.join("state"),"socket":r.join("hub.sock"),"app_server_socket":r.join("app.sock"),"contact_skill":r.join("policy.md"),"lookup_workdir":r,"external_auto_send":true,"dispatch_enabled":true,"kakao":{"enabled":true,"account":"fixture-owner","legacy_state":r.join("legacy"),"receiver_app":r.join("receiver.app"),"sender_app":r.join("sender.app"),"sender_ipc":r.join("ipc")}})).unwrap();
    let s = Store::open(c.state.clone()).unwrap();
    let mut e = Event {
        conversation: Conversation {
            provider: "kakao".into(),
            account: "fixture-owner".into(),
            id: "self-fixture".into(),
        },
        id: "msg-test".into(),
        body: "@[유이] approved ZIP".into(),
        title: "fixture-self-chat".into(),
        occurred_at: now(),
        source: "kakao_notification_store".into(),
        metadata: json!({}),
        agent: Agent::Yui,
    };
    s.note_room_seen(&e.conversation, &e.title).unwrap();
    s.update_room(&e.conversation.key(), true, true, true)
        .unwrap();
    e.metadata["__hub_authorization_stamp"] =
        json!(capabilities::authorization_stamp(&c, &e).unwrap());
    s.enqueue(&e).unwrap();
    s.claim_next().unwrap();
    let p = Plan {
        reply: "[System-유이] : prepared exact reply".into(),
        bundle_id: Some("approved-export".into()),
        sticker_id: None,
    };
    (t, c, s, e, p)
}
fn lock_receipt(deadline: f64) -> Value {
    json!({"status":"held","reason":"screen_locked","input_started":false,"side_effects_started":false,"text_sent":false,"attachment_sent":false,"defer_locked_delivery":true,"retry_deadline":deadline})
}
fn prepare_deferred(s: &Store, e: &Event, p: &Plan) {
    assert!(s.prepare(&e.key(), e, "final", p).unwrap());
    assert!(s.claim_delivery(&e.key()).unwrap());
    s.complete_delivery(&e.key(), &lock_receipt(now() + 86400.0))
        .unwrap();
    s.finish_event(&e.key(), "lock_deferred", "screen_locked")
        .unwrap();
}
#[test]
fn retained_plan_survives_restart_without_model_replay_or_duplicate_lease() {
    let (_t, c, s, e, p) = fixture();
    prepare_deferred(&s, &e, &p);
    let reopened = Store::open(c.state).unwrap();
    reopened.recover().unwrap();
    assert!(reopened.claim_next().unwrap().is_none());
    let (key, saved, plan) = reopened
        .claim_deferred_delivery(now() + 31.0)
        .unwrap()
        .unwrap();
    assert_eq!(key, e.key());
    assert_eq!(saved.body, e.body);
    assert_eq!(plan.reply, p.reply);
    assert_eq!(plan.bundle_id, p.bundle_id);
    assert!(
        reopened
            .claim_deferred_delivery(now() + 31.0)
            .unwrap()
            .is_none()
    );
    reopened.recover().unwrap();
    assert!(
        reopened
            .claim_deferred_delivery(now() + 32.0)
            .unwrap()
            .is_none()
    );
}
#[test]
fn uncertain_or_side_effecting_receipts_never_auto_retry() {
    for field in [
        "input_started",
        "side_effects_started",
        "text_sent",
        "attachment_sent",
    ] {
        let (_t, _c, s, e, p) = fixture();
        s.prepare(&e.key(), &e, "final", &p).unwrap();
        s.claim_delivery(&e.key()).unwrap();
        let mut r = lock_receipt(now() + 1000.0);
        r[field] = json!(true);
        s.complete_delivery(&e.key(), &r).unwrap();
        assert_ne!(
            s.delivery_receipt(&e.key()).unwrap().unwrap()["status"],
            "lock_deferred"
        );
        assert!(s.claim_deferred_delivery(now() + 100.0).unwrap().is_none());
    }
    for reason in [
        "sender_completion_not_observed",
        "locked_ax_send_action_outcome_uncertain",
        "console_session_state_unknown",
    ] {
        let (_t, _c, s, e, p) = fixture();
        s.prepare(&e.key(), &e, "final", &p).unwrap();
        s.claim_delivery(&e.key()).unwrap();
        let mut r = lock_receipt(now() + 1000.0);
        r["reason"] = json!(reason);
        s.complete_delivery(&e.key(), &r).unwrap();
        assert!(s.claim_deferred_delivery(now() + 100.0).unwrap().is_none());
    }
}
#[test]
fn ack_and_busy_not_replayed_after_unlock() {
    for phase in ["ack", "busy"] {
        let (_t, _c, s, e, p) = fixture();
        s.prepare(&e.key(), &e, phase, &p).unwrap();
        s.claim_delivery(&e.key()).unwrap();
        s.complete_delivery(&e.key(), &lock_receipt(now() + 1000.0))
            .unwrap();
        assert!(s.claim_deferred_delivery(now() + 31.0).unwrap().is_none());
    }
}
#[test]
fn total_deadline_is_not_extended_and_expiry_deletes_snapshot() {
    let (_t, _c, s, e, p) = fixture();
    s.prepare(&e.key(), &e, "final", &p).unwrap();
    s.claim_delivery(&e.key()).unwrap();
    let first = now() + 1000.0;
    s.complete_delivery(&e.key(), &lock_receipt(first)).unwrap();
    let (key, _, _) = s.claim_deferred_delivery(now() + 31.0).unwrap().unwrap();
    s.complete_delivery(&key, &lock_receipt(first + 86400.0))
        .unwrap();
    assert_eq!(s.deferred_deadline(&key).unwrap(), Some(first));
    assert!(s.claim_deferred_delivery(first + 1.0).unwrap().is_none());
    assert_eq!(
        s.delivery_receipt(&key).unwrap().unwrap()["reason"],
        "deferred_delivery_expired"
    );
    assert_eq!(s.deferred_deadline(&key).unwrap(), None);
}
#[tokio::test]
async fn room_removal_sister_disable_and_context_reset_block_before_native_launch() {
    for gate in ["removed", "disabled", "reset", "expired", "policy"] {
        let (_t, c, s, e, p) = fixture();
        prepare_deferred(&s, &e, &p);
        let (key, event, plan) = s.claim_deferred_delivery(now() + 31.0).unwrap().unwrap();
        match gate {
            "removed" => {
                s.remove_room(&e.conversation.key()).unwrap();
            }
            "disabled" => {
                s.update_room(&e.conversation.key(), true, false, true)
                    .unwrap();
            }
            "reset" => {
                s.reset_room_context(&e.conversation.key()).unwrap();
            }
            "expired" => {
                s.db()
                    .unwrap()
                    .execute("UPDATE delivery_inputs SET expires=?", [now() - 1.0])
                    .unwrap();
            }
            "policy" => {
                std::fs::remove_file(&c.contact_skill).unwrap();
            }
            _ => unreachable!(),
        };
        let r = daemon::resume_locked_delivery(&c, &s, &key, &event, &plan)
            .await
            .unwrap();
        assert_eq!(r["status"], "held");
        assert!(
            r["reason"].as_str().unwrap().starts_with("deferred_")
                || r["reason"] == "authorization_changed_or_missing"
        );
        assert!(
            !c.kakao.sender_ipc.exists(),
            "native never launched on revoked plan"
        );
    }
}
#[test]
fn delivery_success_removes_snapshot_and_releases_no_retry() {
    let (_t, _c, s, e, p) = fixture();
    prepare_deferred(&s, &e, &p);
    let (key, _, _) = s.claim_deferred_delivery(now() + 31.0).unwrap().unwrap();
    s.complete_delivery(
        &key,
        &json!({"status":"sent_verified","text_sent":true,"attachment_sent":true}),
    )
    .unwrap();
    assert!(s.claim_deferred_delivery(now() + 32.0).unwrap().is_none());
    assert_eq!(s.deferred_deadline(&key).unwrap(), None);
}
fn login_probe(status: &str) -> Value {
    json!({"status":"ready","input_started":false,"side_effects_started":false,"text_sent":false,"attachment_sent":false,"open_target":{"read_only":true,"kakao_status":status,"session_state":"unlocked","keyboard_events_posted":false,"clipboard_accessed":false}})
}
#[test]
fn login_readiness_only_keeps_existing_lock_deferred_final_plan() {
    let (_t, _c, s, e, p) = fixture();
    prepare_deferred(&s, &e, &p);
    let deadline = s.deferred_deadline(&e.key()).unwrap();
    for status in ["not_running", "login_required"] {
        let (key, _, _) = s.claim_deferred_delivery(now() + 31.0).unwrap().unwrap();
        assert!(
            s.retain_deferred_readiness(&key, &login_probe(status))
                .unwrap()
        );
        assert_eq!(
            s.delivery_receipt(&key).unwrap().unwrap()["reason"],
            "waiting_for_kakao_login"
        );
        assert_eq!(s.deferred_deadline(&key).unwrap(), deadline);
    }
    let (key, _, saved) = s.claim_deferred_delivery(now() + 31.0).unwrap().unwrap();
    assert!(
        !s.retain_deferred_readiness(&key, &login_probe("available_or_unknown"))
            .unwrap()
    );
    assert_eq!(saved.reply, p.reply);
    s.complete_delivery(
        &key,
        &json!({"status":"sent_verified","text_sent":true,"attachment_sent":true}),
    )
    .unwrap();
    assert!(s.claim_deferred_delivery(now() + 32.0).unwrap().is_none());
}
#[test]
fn readiness_cannot_resurrect_first_offline_request_or_uncertain_lease() {
    for status in ["held", "sending_uncertain"] {
        let (_t, _c, s, e, p) = fixture();
        s.prepare(&e.key(), &e, "final", &p).unwrap();
        s.claim_delivery(&e.key()).unwrap();
        s.complete_delivery(
            &e.key(),
            &json!({"status":status,"reason":"kakao_not_running_or_ambiguous"}),
        )
        .unwrap();
        assert!(
            !s.retain_deferred_readiness(&e.key(), &login_probe("not_running"))
                .unwrap()
        );
        assert!(s.claim_deferred_delivery(now() + 31.0).unwrap().is_none());
    }
    let (_t, _c, s, e, p) = fixture();
    prepare_deferred(&s, &e, &p);
    let (key, _, _) = s.claim_deferred_delivery(now() + 31.0).unwrap().unwrap();
    let mut probe = login_probe("login_required");
    probe["open_target"]["clipboard_accessed"] = json!(true);
    assert!(!s.retain_deferred_readiness(&key, &probe).unwrap());
}
#[test]
fn board_exposes_wait_as_wait_with_deadline_and_attempt_schedule() {
    let (_t, c, s, e, p) = fixture();
    prepare_deferred(&s, &e, &p);
    let calls = s.calls(None, None, 10).unwrap();
    let receipt = &calls["items"][0]["deliveries"][0];
    assert_eq!(receipt["status"], "lock_deferred");
    assert!(receipt["deferred"]["expires"].is_number());
    assert!(receipt["deferred"]["next_attempt"].is_number());
    assert_eq!(receipt["deferred"]["attempts"], 0);
    let kakao = &c.descriptors()[0];
    assert_eq!(kakao["defer_locked_delivery"], true);
    assert_eq!(kakao["locked_ax_text_live_verified"], false);
    assert_eq!(kakao["locked_attachment_supported"], false);
}
#[tokio::test]
async fn deferred_policy_change_and_unknown_or_legacy_stamp_never_launch_native() {
    for condition in ["policy_changed", "missing_stamp", "unknown_stamp"] {
        let (_t, c, s, mut e, mut p) = fixture();
        p.bundle_id = None;
        match condition {
            "missing_stamp" => {
                e.metadata
                    .as_object_mut()
                    .unwrap()
                    .remove("__hub_authorization_stamp");
            }
            "unknown_stamp" => {
                e.metadata["__hub_authorization_stamp"] = json!("unknown-legacy-approval");
            }
            _ => {}
        }
        prepare_deferred(&s, &e, &p);
        if condition == "policy_changed" {
            std::fs::write(
                &c.contact_skill,
                "name: contact-other\nRevised stricter sharing policy.\n",
            )
            .unwrap();
        }
        let (key, event, plan) = s.claim_deferred_delivery(now() + 31.0).unwrap().unwrap();
        let receipt = daemon::resume_locked_delivery(&c, &s, &key, &event, &plan)
            .await
            .unwrap();
        assert_eq!(receipt["reason"], "authorization_changed_or_missing");
        assert_eq!(receipt["status"], "held");
        assert!(
            !c.kakao.sender_ipc.exists(),
            "old textual reply must not reach native even for a probe"
        );
    }
}
#[tokio::test]
async fn deferred_text_cannot_outlive_project_binding_or_bundle_authorization() {
    for revoke in ["project", "bundle"] {
        let (t, c, s, mut e, mut p) = fixture();
        let root = t.path().join("shared-project");
        std::fs::create_dir_all(&root).unwrap();
        let registry = json!({"rooms":{e.conversation.key():["alpha"]},"projects":{"alpha":{"name":"Shared alpha","root":root,"git":false,"snapshot_max_age_secs":3600}}});
        atomic(&c.state.join(capabilities::REGISTRY), &registry).unwrap();
        let artifact = root.join("data.json");
        std::fs::write(&artifact, b"{}\n").unwrap();
        let bundles = json!({"rooms":{e.conversation.id.clone():{"approved-export":{"kind":"file","source":artifact,"filename":"shared.zip","description":"Approved data"}}}});
        atomic(
            &c.kakao.legacy_state.join("attachment-bundles.json"),
            &bundles,
        )
        .unwrap();
        e.metadata["__hub_authorization_stamp"] =
            json!(capabilities::authorization_stamp(&c, &e).unwrap());
        p.bundle_id = None;
        p.reply = "[System-유이] : Observed project status at the recorded time".into();
        prepare_deferred(&s, &e, &p);
        if revoke == "project" {
            let mut updated = registry;
            updated["rooms"][e.conversation.key()] = json!([]);
            atomic(&c.state.join(capabilities::REGISTRY), &updated).unwrap();
        } else {
            atomic(
                &c.kakao.legacy_state.join("attachment-bundles.json"),
                &json!({"rooms":{}}),
            )
            .unwrap();
        }
        let (key, event, plan) = s.claim_deferred_delivery(now() + 31.0).unwrap().unwrap();
        let receipt = daemon::resume_locked_delivery(&c, &s, &key, &event, &plan)
            .await
            .unwrap();
        assert_eq!(receipt["reason"], "authorization_changed_or_missing");
        assert!(!c.kakao.sender_ipc.exists());
    }
}
#[test]
fn valid_changes_to_unshared_other_project_do_not_revoke_original_approval() {
    let (t, c, _s, mut e, _p) = fixture();
    let alpha = t.path().join("alpha");
    let beta = t.path().join("beta");
    let newer = t.path().join("beta-new");
    for root in [&alpha, &beta, &newer] {
        std::fs::create_dir_all(root).unwrap();
    }
    let mut registry = json!({"rooms":{e.conversation.key():["alpha"],"another-room":["beta"]},"projects":{"alpha":{"name":"Shared alpha","root":alpha,"git":false,"snapshot_max_age_secs":3600},"beta":{"name":"Other beta","root":beta,"git":false,"snapshot_max_age_secs":3600}}});
    atomic(&c.state.join(capabilities::REGISTRY), &registry).unwrap();
    e.metadata["__hub_authorization_stamp"] =
        json!(capabilities::authorization_stamp(&c, &e).unwrap());
    registry["projects"]["beta"]["name"] = json!("Other beta renamed");
    registry["projects"]["beta"]["root"] = json!(newer);
    atomic(&c.state.join(capabilities::REGISTRY), &registry).unwrap();
    assert!(daemon::validate_deferred_authorization(&c, &e).is_ok());
}
#[tokio::test]
async fn staged_artifact_never_reaches_native_after_room_or_policy_revocation() {
    use communication_hub::{adapters::Kakao, attachments};
    for revoke in ["room", "policy"] {
        let (t, c, s, mut e, _p) = fixture();
        let source = t.path().join("shared-data.json");
        std::fs::write(&source, b"{}\n").unwrap();
        atomic(&c.kakao.legacy_state.join("attachment-bundles.json"),&json!({"rooms":{e.conversation.id.clone():{"approved":{"kind":"file","source":source,"filename":"shared.zip","description":"Approved data"}}}})).unwrap();
        e.metadata["__hub_authorization_stamp"] =
            json!(capabilities::authorization_stamp(&c, &e).unwrap());
        let artifact = attachments::prepare(&c, &e, &e.key(), "approved").unwrap();
        assert!(std::path::Path::new(artifact["path"].as_str().unwrap()).exists());
        if revoke == "room" {
            s.remove_room(&e.conversation.key()).unwrap();
        } else {
            std::fs::write(
                &c.contact_skill,
                "name: contact-other\nNow revised stricter policy\n",
            )
            .unwrap();
        }
        let receipt = Kakao { cfg: c.clone() }
            .authorized_native_request(&s, &e, &json!({"attachment_path":artifact["path"]}))
            .await
            .unwrap();
        assert_eq!(receipt["status"], "held");
        assert_eq!(receipt["side_effects_started"], false);
        assert!(
            !c.kakao.sender_ipc.join("sender-requests").exists(),
            "staged ZIP cannot retain permission to invoke the native helper"
        );
    }
}
