use crate::{
    config::{Config, atomic, private_dir},
    event::{Event, now},
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
            json!({"service":"communication-hub","pid":std::process::id(),"runtime":"rust","dispatch_enabled":active.load(Ordering::SeqCst),"external_auto_send":sending.load(Ordering::SeqCst),"processing":processing.load(Ordering::SeqCst),"model":cfg.model,"effort":cfg.effort,"store":store.status()?,"kakao_source":crate::config::json_file(&cfg.state.join("source-kakao.json"))?}),
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
        Some("ingest_kakao") => {
            cfg.validate_channel("kakao", &cfg.kakao.account)
                .map_err(|_| Rejected)?;
            let raw = &v["event"];
            if raw["kind"] == "source_status" {
                atomic(
                    &cfg.state.join("source-kakao.json"),
                    &json!({"status":raw["status"],"at":raw["at"],"received_at":now()}),
                )?;
                return Ok(json!({"status":"heartbeat"}));
            }
            ingest(
                cfg,
                store,
                Event::from_kakao(raw, &cfg.kakao.account).map_err(|_| Rejected)?,
                notify,
            )
        }
        Some("ingest") => ingest(
            cfg,
            store,
            serde_json::from_value(v["event"].clone()).map_err(|_| Rejected)?,
            notify,
        ),
        _ => Err(Rejected.into()),
    }
}
fn ingest(cfg: &Config, store: &Store, event: Event, notify: &Notify) -> Result<Value> {
    cfg.validate_channel(&event.conversation.provider, &event.conversation.account)
        .map_err(|_| Rejected)?;
    if let Err(reason) = event.validate(now(), store.session(&event.conversation)?.is_some()) {
        if event.body.contains("[유이]") {
            store.record_rejected(&event, &reason.to_string())?;
        }
        return Err(Rejected.into());
    }
    let added = store.enqueue(&event)?;
    if added {
        notify.notify_one()
    }
    Ok(json!({"event_key":event.key(),"status":if added{"queued"}else{"duplicate"}}))
}
