use crate::{
    config::{Config, atomic, private_dir},
    event::{Agent, Event, now},
    store::Store,
    worker,
};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{
    os::fd::AsRawFd,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::Notify,
};

pub const LIMIT: usize = 1_000_000;
#[derive(Debug)]
struct Rejected;
impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "request_rejected")
    }
}
impl std::error::Error for Rejected {}
pub async fn request(socket: &PathBuf, value: Value) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let stream = UnixStream::connect(socket).await?;
        let (r, mut w) = stream.into_split();
        w.write_all(&serde_json::to_vec(&value)?).await?;
        w.write_all(b"\n").await?;
        let line = read_line(BufReader::new(r)).await?;
        Ok(serde_json::from_slice(&line)?)
    })
    .await?
}
async fn read_line<R: tokio::io::AsyncRead + Unpin>(mut reader: BufReader<R>) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            bail!("unexpected_socket_eof")
        }
        let end = buf
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(buf.len());
        if bytes.len() + end > LIMIT {
            bail!("oversized_control_frame")
        }
        bytes.extend_from_slice(&buf[..end]);
        reader.consume(end);
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            return Ok(bytes);
        }
    }
}
pub async fn run(cfg: Config, no_receiver: bool) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    private_dir(&cfg.state)?;
    private_dir(cfg.socket.parent().unwrap())?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(cfg.state.join("daemon.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("hub_already_running")
    }
    if cfg.socket.exists() {
        std::fs::remove_file(&cfg.socket)?;
    }
    let listener = UnixListener::bind(&cfg.socket)?;
    std::fs::set_permissions(&cfg.socket, std::fs::Permissions::from_mode(0o600))?;
    let mut store = Store::open(cfg.state.clone())?;
    store.store_bodies = cfg.dashboard.as_ref().is_some_and(|d| d.store_body);
    store.import_legacy(&cfg)?;
    store.recover()?;
    let notify = Arc::new(Notify::new());
    let paused = crate::config::json_file(&cfg.state.join("control.json"))?["paused"] == true;
    let active = Arc::new(AtomicBool::new(cfg.dispatch_enabled && !paused));
    let sending = Arc::new(AtomicBool::new(cfg.external_auto_send && !paused));
    let processing = Arc::new(AtomicBool::new(false));
    let cfg = Arc::new(cfg);
    // Yumi's first call should not pay the CLI start-up either.
    let warm_cfg = cfg.clone();
    tokio::spawn(async move { crate::worker::warm_yumi(&warm_cfg).await });
    let board_tasks = crate::dashboard::start(
        cfg.clone(),
        store.clone(),
        active.clone(),
        sending.clone(),
        processing.clone(),
    )
    .await?;
    let processing_task = {
        let (cfg, store, notify, active, sending, processing) = (
            cfg.clone(),
            store.clone(),
            notify.clone(),
            active.clone(),
            sending.clone(),
            processing.clone(),
        );
        tokio::spawn(async move {
            loop {
                if active.load(Ordering::SeqCst) {
                    if sending.load(Ordering::SeqCst) {
                        // The deferred journal contains the prepared final Plan. Resuming never
                        // runs a model, and its sending lease obeys the usual uncertain recovery.
                        if let Ok(Some((key, e, plan))) = store.claim_deferred_delivery(now()) {
                            processing.store(true, Ordering::SeqCst);
                            let receipt = resume_locked_delivery(&cfg,&store,&key,&e,&plan).await
                                .unwrap_or_else(|_|json!({"status":"sending_uncertain","reason":"deferred_transport_outcome_uncertain"}));
                            if receipt["status"] != "lock_deferred" {
                                let _ = store.complete_delivery(&key, &receipt);
                            }
                            let saved = store
                                .delivery_receipt(&key)
                                .ok()
                                .flatten()
                                .unwrap_or(receipt);
                            let _ = store.finish_event(
                                &e.key(),
                                saved["status"].as_str().unwrap_or("sending_uncertain"),
                                saved["reason"].as_str().unwrap_or(""),
                            );
                            let _ = atomic(
                                &cfg.state.join("last-delivery.json"),
                                &json!({"event_key":e.key(),"resumed_saved_plan":true,"result":saved,"at":now()}),
                            );
                            processing.store(false, Ordering::SeqCst);
                            continue;
                        }
                    }
                    let event = match store.claim_next() {
                        Ok(e) => e,
                        Err(_) => {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            continue;
                        }
                    };
                    if let Some(e) = event {
                        processing.store(true, Ordering::SeqCst);
                        let result = worker::process(&cfg, &store, &e, &sending).await;
                        let (status, reason) = match &result {
                            Ok(r) => (
                                r["delivery"]["status"]
                                    .as_str()
                                    .unwrap_or("uncertain")
                                    .to_owned(),
                                String::new(),
                            ),
                            Err(error) if error.is::<crate::rpc::Uncertain>() => (
                                "ambiguous".into(),
                                "dispatch_or_delivery_outcome_uncertain".into(),
                            ),
                            Err(_) => ("held".into(), "policy_or_backend_preflight_failed".into()),
                        };
                        let _ = store.finish_event(&e.key(), &status, &reason);
                        if let Ok(result) = result {
                            let _ = atomic(
                                &cfg.state.join("last-delivery.json"),
                                &json!({"event_key":e.key(),"status":status,"model":cfg.model,"effort":cfg.effort,"result":result,"at":now()}),
                            );
                        }
                        processing.store(false, Ordering::SeqCst);
                        continue;
                    }
                }
                tokio::select! {_=notify.notified()=>{},_=tokio::time::sleep(Duration::from_secs(2))=>{}}
            }
        })
    };
    let receiver_task = if no_receiver {
        None
    } else {
        let cfg = cfg.clone();
        Some(tokio::spawn(async move {
            loop {
                let status = crate::config::json_file(&cfg.state.join("source-kakao.json"))
                    .unwrap_or(json!({}));
                if cfg.kakao.enabled && now() - status["received_at"].as_f64().unwrap_or(0.0) > 15.0
                {
                    let _ = tokio::process::Command::new("/usr/bin/open")
                        .args(["-n", "-g"])
                        .arg(&cfg.kakao.receiver_app)
                        .args(["--args", "--hub-socket"])
                        .arg(&cfg.socket)
                        .arg("--state-dir")
                        .arg(&cfg.kakao.legacy_state)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .kill_on_drop(true)
                        .status()
                        .await;
                }
                // The open-room watch lives in the sender app, which already holds Accessibility;
                // its own lock keeps a relaunch from starting a second copy.
                let watch = crate::config::json_file(&cfg.state.join("source-kakao-open.json"))
                    .unwrap_or(json!({}));
                if cfg.kakao.enabled
                    && cfg.kakao.watch_open_room
                    && now() - watch["received_at"].as_f64().unwrap_or(0.0) > 15.0
                {
                    let _ = tokio::process::Command::new("/usr/bin/open")
                        .args(["-n", "-g"])
                        .arg(&cfg.kakao.sender_app)
                        .args(["--args", "--watch-open", "--hub-socket"])
                        .arg(&cfg.socket)
                        .arg("--ipc-dir")
                        .arg(&cfg.kakao.sender_ipc)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .kill_on_drop(true)
                        .status()
                        .await;
                }
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
        }))
    };
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            _=tokio::signal::ctrl_c()=>break,
            _=term.recv()=>break,
            accepted=listener.accept()=>{
                let (stream,_)=accepted?;let(cfg,store,notify,active,sending,processing)=(cfg.clone(),store.clone(),notify.clone(),active.clone(),sending.clone(),processing.clone());
                tokio::spawn(async move{
                    if stream.peer_cred().map(|c|c.uid()).ok()!=Some(unsafe{libc::geteuid()}){return}
                    let (r,mut w)=stream.into_split();
                    let response=match tokio::time::timeout(Duration::from_secs(3),read_line(BufReader::new(r))).await{
                        Ok(Ok(line))=>match serde_json::from_slice::<Value>(&line){Ok(v)=>handle(&cfg,&store,&v,&notify,&active,&sending,&processing),Err(_)=>Err(Rejected.into())},_=>Err(Rejected.into())
                    };
                    let response=match response{Ok(v)=>json!({"ok":true,"result":v}),Err(e) if e.is::<Rejected>()=>json!({"ok":false,"error":"request_rejected"}),Err(_)=>json!({"ok":false,"error":"service_temporarily_unavailable"})};
                    if let Ok(bytes)=serde_json::to_vec(&response){let _=tokio::time::timeout(Duration::from_secs(2),async{w.write_all(&bytes).await?;w.write_all(b"\n").await}).await;}
                });
            }
        }
    }
    sending.store(false, Ordering::SeqCst);
    active.store(false, Ordering::SeqCst);
    processing_task.abort();
    if let Some(task) = receiver_task {
        task.abort()
    }
    if let Some((server, probe)) = board_tasks {
        server.abort();
        probe.abort();
        let _ = atomic(
            &cfg.state.join("dashboard-status.json"),
            &json!({"online":false,"at":now()}),
        );
    }
    let _ = std::fs::remove_file(&cfg.socket);
    drop(lock);
    Ok(())
}
/// A deferred plan retains the originally approved conversation, not arbitrary new UI text.
/// Recheck revocation, sister gate, context reset, current policy and total age before any write.
pub fn validate_deferred_authorization(cfg: &Config, e: &Event) -> Result<()> {
    let saved = e.metadata["__hub_authorization_stamp"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("authorization_changed_or_missing"))?;
    let current = crate::capabilities::authorization_stamp(cfg, e)
        .map_err(|_| anyhow::anyhow!("authorization_changed_or_missing"))?;
    if saved != current {
        bail!("authorization_changed_or_missing")
    }
    Ok(())
}
pub async fn resume_locked_delivery(
    cfg: &Config,
    store: &Store,
    key: &str,
    e: &Event,
    plan: &crate::event::Plan,
) -> Result<Value> {
    use crate::adapters::Adapter;
    let held = |reason: &str| json!({"status":"held","reason":reason});
    if !cfg.dispatch_enabled || !cfg.external_auto_send {
        return Ok(held("deferred_send_disabled"));
    }
    match crate::capabilities::delivery_gate(cfg, store, e) {
        Ok(Some(reason)) => return Ok(held(&format!("deferred_{reason}"))),
        Err(_) => return Ok(held("deferred_delivery_gate_unavailable")),
        Ok(None) => {}
    }
    if store.deferred_deadline(key)?.is_none_or(|at| at <= now()) {
        return Ok(held("deferred_delivery_expired"));
    }
    if validate_deferred_authorization(cfg, e).is_err() {
        return Ok(held("authorization_changed_or_missing"));
    }
    let kakao = crate::adapters::Kakao { cfg: cfg.clone() };
    let probe = kakao.event_session_state(e).await?;
    if store.retain_deferred_readiness(key, &probe)? {
        return Ok(store.delivery_receipt(key)?.unwrap());
    }
    // A failing read-only probe never licenses a speculative UI write.
    if probe["status"] != "ready" {
        return Ok(held("deferred_readiness_unavailable"));
    }
    if matches!(
        probe["open_target"]["session_state"].as_str(),
        Some("locked" | "inactive")
    ) && (!cfg.kakao.locked_ax_text || plan.bundle_id.is_some() || plan.sticker_id.is_some())
    {
        let mut rejected = probe;
        rejected["status"] = json!("held");
        rejected["reason"] = json!(if rejected["open_target"]["session_state"] == "locked" {
            "screen_locked"
        } else {
            "console_session_inactive"
        });
        rejected["defer_locked_delivery"] = json!(cfg.kakao.defer_locked_delivery);
        rejected["retry_deadline"] = json!(store.deferred_deadline(key)?);
        return Ok(rejected);
    }
    // The read-only readiness probe can take time. Recheck once more before staging or writing
    // anything so a policy/share change while waiting does not license the saved old reply.
    if validate_deferred_authorization(cfg, e).is_err() {
        return Ok(held("authorization_changed_or_missing"));
    }
    kakao.send(store, key, e, plan).await
}
fn handle(
    cfg: &Config,
    store: &Store,
    v: &Value,
    notify: &Notify,
    active: &AtomicBool,
    sending: &AtomicBool,
    processing: &AtomicBool,
) -> Result<Value> {
    match v["method"].as_str() {
        Some("status") => Ok(
            json!({"service":"communication-hub","pid":std::process::id(),"runtime":"rust","dispatch_enabled":active.load(Ordering::SeqCst),"external_auto_send":sending.load(Ordering::SeqCst),"processing":processing.load(Ordering::SeqCst),"model":cfg.model,"effort":cfg.effort,"service_tier":cfg.service_tier,"store":store.status()?,"kakao_source":crate::config::json_file(&cfg.state.join("source-kakao.json"))?,"kakao_open_room":crate::config::json_file(&cfg.state.join("source-kakao-open.json"))?}),
        ),
        Some("adapters") => Ok(cfg.descriptors()),
        Some("pause") => {
            atomic(
                &cfg.state.join("control.json"),
                &json!({"paused":true,"at":now()}),
            )?;
            active.store(false, Ordering::SeqCst);
            sending.store(false, Ordering::SeqCst);
            Ok(json!({"paused":true,"in_flight":processing.load(Ordering::SeqCst)}))
        }
        Some("resume") => {
            atomic(
                &cfg.state.join("control.json"),
                &json!({"paused":false,"at":now()}),
            )?;
            active.store(cfg.dispatch_enabled, Ordering::SeqCst);
            sending.store(cfg.external_auto_send, Ordering::SeqCst);
            notify.notify_one();
            Ok(json!({"resumed":true}))
        }
        Some("authorize_kakao_loco") => {
            authorize_loco(cfg, store, &v["params"], sending.load(Ordering::SeqCst))
        }
        Some("ingest_kakao_loco") => ingest_loco(
            cfg,
            store,
            &v["event"],
            notify,
            processing.load(Ordering::SeqCst) && sending.load(Ordering::SeqCst),
        ),
        Some("ingest_kakao") => {
            cfg.validate_channel("kakao", &cfg.kakao.account)
                .map_err(|_| Rejected)?;
            let raw = &v["event"];
            let status_file = match raw["kind"].as_str() {
                Some("source_status") => Some("source-kakao.json"),
                Some("open_room_status") => Some("source-kakao-open.json"),
                _ => None,
            };
            if let Some(file) = status_file {
                atomic(
                    &cfg.state.join(file),
                    &json!({"status":raw["status"],"at":raw["at"],"received_at":now()}),
                )?;
                return Ok(json!({"status":"heartbeat"}));
            }
            let event = if raw["source"] == crate::event::OPEN_ROOM_SOURCE {
                if !cfg.kakao.watch_open_room {
                    return Err(Rejected.into());
                }
                // The window shows only its title; an unregistered or ambiguous title is no call.
                let title = raw["chat_name"].as_str().unwrap_or("");
                let room = store
                    .room_by_title("kakao", &cfg.kakao.account, title)?
                    .ok_or(Rejected)?;
                Event::from_open_room(raw, room).map_err(|_| Rejected)?
            } else {
                Event::from_kakao(raw, &cfg.kakao.account).map_err(|_| Rejected)?
            };
            ingest(
                cfg,
                store,
                event,
                notify,
                processing.load(Ordering::SeqCst) && sending.load(Ordering::SeqCst),
                false,
            )
        }
        Some("ingest") => ingest(
            cfg,
            store,
            serde_json::from_value(v["event"].clone()).map_err(|_| Rejected)?,
            notify,
            processing.load(Ordering::SeqCst) && sending.load(Ordering::SeqCst),
            false,
        ),
        _ => Err(Rejected.into()),
    }
}
/// One notification may call Yui, Yumi or both; each called sister gets her own event and answer.
/// `working`: another call is being answered and sending is on, so a newly queued caller hears
/// right away that the sister is busy.
fn ingest(
    cfg: &Config,
    store: &Store,
    event: Event,
    notify: &Notify,
    working: bool,
    protocol: bool,
) -> Result<Value> {
    if protocol != (event.source == crate::event::LOCO_SOURCE) {
        return Err(Rejected.into());
    }
    if !protocol && cfg.kakao.loco_target(&event.conversation).is_some() {
        return Ok(json!({"status":"ignored","reason":"loco_owns_room"}));
    }
    cfg.validate_channel(&event.conversation.provider, &event.conversation.account)
        .map_err(|_| Rejected)?;
    let agents: Vec<Agent> = event
        .called_agents()
        .into_iter()
        .filter(|a| *a == Agent::Yui || cfg.yumi.is_some())
        .collect();
    if agents.is_empty() {
        // A sister's name without her exact tag is a near-miss call. Recording it (with the body
        // only under the operator's retention setting) shows why nobody answered.
        if let Some(agent) = Agent::ALL
            .into_iter()
            .find(|a| event.body.contains(a.name()))
        {
            let reason = if agent == Agent::Yumi && cfg.yumi.is_none() {
                "yumi_not_configured"
            } else {
                "name_without_exact_tag"
            };
            store.record_rejected(&event.for_agent(agent), reason)?;
        }
        return if protocol {
            Ok(json!({"status":"ignored","reason":"no_enabled_agent_tag"}))
        } else {
            Err(Rejected.into())
        };
    }
    if store.reported_by_other_source(&event)? {
        return Ok(json!({"status":"duplicate","reason":"reported_by_other_source"}));
    }
    // Only owner-approved rooms are answered; a new room waits on the dashboard for approval.
    store.note_room_seen(&event.conversation, &event.title)?;
    let room = store.room(&event.conversation)?.unwrap_or_default();
    let approved = room["approved"] == true;
    let initialized = store.initialized(&event.conversation)?;
    let mut results = Vec::new();
    let mut queued = false;
    for agent in agents {
        let e = event.for_agent(agent);
        let gate = if !approved && !store.answer_unapproved_rooms()? {
            Some("room_pending_approval")
        } else if approved && room[agent.name_key()] == false {
            Some("sister_disabled_in_room")
        } else {
            None
        };
        let mut rejection_reason = None;
        let status = match gate.map_or_else(
            || e.validate(now(), initialized),
            |r| Err(anyhow::anyhow!(r)),
        ) {
            Err(reason) => {
                store.record_rejected(&e, &reason.to_string())?;
                rejection_reason = Some(reason.to_string());
                "rejected"
            }
            Ok(()) if store.enqueue(&e)? => {
                queued = true;
                if working && cfg.external_auto_send {
                    let (cfg, store, e) = (cfg.clone(), store.clone(), e.clone());
                    tokio::spawn(async move {
                        let _ = crate::worker::notify_busy(&cfg, &store, &e).await;
                    });
                }
                "queued"
            }
            Ok(()) => "duplicate",
        };
        results.push(
            json!({"agent":agent,"event_key":e.key(),"status":status,"reason":rejection_reason}),
        );
    }
    if queued {
        notify.notify_one()
    }
    if !protocol && results.iter().all(|r| r["status"] == "rejected") {
        return Err(Rejected.into());
    }
    Ok(match results.len() {
        1 => results.remove(0),
        _ => json!({"events":results}),
    })
}

/// Sidecar callback immediately before each WRITE/upload. This checks the original journaled
/// lease; it never creates work, claims a new lease, or acquires the transport lock.
pub fn authorize_loco(cfg: &Config, store: &Store, params: &Value, sending: bool) -> Result<Value> {
    let held = |reason: &str| Ok(json!({"status":"held","reason":reason}));
    let Some(key) = params["delivery_id"]
        .as_str()
        .filter(|k| !k.is_empty() && k.len() <= 128)
    else {
        return held("invalid_delivery_id");
    };
    let Some(component) = params["component"]
        .as_str()
        .filter(|c| matches!(*c, "text" | "attachment"))
    else {
        return held("invalid_component");
    };
    let Some((event, plan, phase)) = store.sending_input(key)? else {
        return held("delivery_not_sending");
    };
    let Some((loco, chat_id)) = cfg.kakao.loco_target(&event.conversation) else {
        return held("loco_binding_not_active");
    };
    if loco.validate().is_err()
        || params["user_id"] != loco.expected_user_id
        || params["chat_id"] != chat_id
    {
        return held("loco_target_mismatch");
    }
    if component == "attachment" && plan.bundle_id.is_none() && plan.sticker_id.is_none() {
        return held("attachment_not_planned");
    }
    if !cfg.external_auto_send
        || (phase != "controlled"
            && (!sending
                || crate::config::json_file(&cfg.state.join("control.json"))?["paused"] == true))
    {
        return held("delivery_paused");
    }
    if let Some(reason) = crate::capabilities::delivery_gate(cfg, store, &event)? {
        return held(reason);
    }
    if validate_deferred_authorization(cfg, &event).is_err() {
        return held("authorization_changed_or_missing");
    }
    Ok(json!({"status":"authorized"}))
}

/// The private protocol ingress alone constructs authenticated transport metadata. Remote body
/// content never supplies a local authorization stamp or room binding.
pub fn ingest_loco(
    cfg: &Config,
    store: &Store,
    raw: &Value,
    notify: &Notify,
    working: bool,
) -> Result<Value> {
    use crate::{
        config::LocoMode,
        event::{Conversation, LOCO_SOURCE, digest},
        loco::numeric_id,
    };
    let rejected = |reason: &str| Ok(json!({"status":"rejected","reason":reason}));
    cfg.validate_channel("kakao", &cfg.kakao.account)
        .map_err(|_| Rejected)?;
    let Some(loco) = &cfg.kakao.loco else {
        return rejected("loco_not_configured");
    };
    if loco.validate().is_err() {
        return rejected("loco_invalid_configuration");
    }
    if raw["mode"] != json!(loco.mode) {
        return rejected("loco_mode_mismatch");
    }
    if raw["user_id"] != loco.expected_user_id {
        return rejected("loco_account_mismatch");
    }
    let Some(chat_id) = raw["chat_id"].as_str().filter(|s| numeric_id(s)) else {
        return rejected("loco_invalid_chat_id");
    };
    let Some((room_id, _)) = loco.rooms.iter().find(|(_, id)| *id == chat_id) else {
        return rejected("loco_unmapped_chat");
    };
    let Some(log_id) = raw["log_id"].as_str().filter(|s| numeric_id(s)) else {
        return rejected("loco_invalid_log_id");
    };
    let Some(author_id) = raw["author_id"].as_str().filter(|s| numeric_id(s)) else {
        return rejected("loco_invalid_author_id");
    };
    let Some(body) = raw["body"].as_str().filter(|s| s.len() <= 65536) else {
        return rejected("loco_invalid_body");
    };
    let Some(sent_at) = raw["sent_at"].as_f64().filter(|n| n.is_finite()) else {
        return rejected("loco_invalid_time");
    };
    let conversation = Conversation {
        provider: "kakao".into(),
        account: cfg.kakao.account.clone(),
        id: room_id.clone(),
    };
    // Requiring an existing binding prevents the legacy title-matching registration path from
    // implicitly approving a protocol room.
    if store.room(&conversation)?.is_none() {
        return rejected("loco_room_not_registered");
    }
    let key = digest(serde_json::to_vec(&json!([
        loco.expected_user_id,
        chat_id,
        log_id
    ]))?);
    if let Some(receipt) = store.loco_ack(&key)? {
        return Ok(json!({"status":"duplicate","prior":receipt,"durable":true}));
    }
    let event = Event {
        conversation,
        id: format!("loco:{log_id}"),
        body: body.into(),
        title: raw["title"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(200)
            .collect(),
        occurred_at: sent_at,
        source: LOCO_SOURCE.into(),
        agent: Agent::Yui,
        metadata: json!({"transport":"loco","user_id":loco.expected_user_id,"chat_id":chat_id,
            "author_id":author_id,"log_id":log_id,"sender_identity":"protocol_authenticated",
            "actual_mention_verified":false}),
    };
    let mut result = if loco.mode == LocoMode::Shadow {
        json!({"status":"shadow"})
    } else {
        ingest(cfg, store, event.clone(), notify, working, true)?
    };
    store.record_loco_ack(&key, &event, &result)?;
    // Only the committed journal allows the sidecar to consume its history cursor.
    result["durable"] = json!(true);
    Ok(result)
}

#[cfg(test)]
mod loco_ingress_tests {
    use super::*;
    #[test]
    fn active_mapped_rooms_suppress_ui_ingress_and_generic_cannot_spoof_loco() {
        let t = tempfile::TempDir::new().unwrap();
        let p = t.path();
        let cfg:Config=serde_json::from_value(json!({"state":p.join("state"),"socket":p.join("hub.sock"),"app_server_socket":p.join("app.sock"),"contact_skill":p.join("policy.md"),"lookup_workdir":p,"external_auto_send":false,"kakao":{"enabled":true,"account":"owner","legacy_state":p.join("legacy"),"receiver_app":p.join("receiver"),"sender_app":p.join("sender"),"sender_ipc":p.join("ipc"),"loco":{"socket":p.join("loco.sock"),"expected_user_id":"123","mode":"active","rooms":{"registered-room":"456"}}}})).unwrap();
        let store = Store::open(cfg.state.clone()).unwrap();
        let mut e:Event=serde_json::from_value(json!({"conversation":{"provider":"kakao","account":"owner","id":"registered-room"},"id":"1","body":"@[유이] task","title":"room","occurred_at":now(),"source":"kakao_notification_store"})).unwrap();
        for source in [
            crate::event::NOTIFICATION_SOURCE,
            crate::event::OPEN_ROOM_SOURCE,
        ] {
            e.source = source.into();
            assert_eq!(
                ingest(&cfg, &store, e.clone(), &Notify::new(), false, false).unwrap()["reason"],
                "loco_owns_room"
            );
        }
        e.source = crate::event::LOCO_SOURCE.into();
        assert!(ingest(&cfg, &store, e, &Notify::new(), false, false).is_err());
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
