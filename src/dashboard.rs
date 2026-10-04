use crate::{
    config::{Config, atomic, json_file},
    event::now,
    rpc::Rpc,
    store::Store,
};
use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::RwLock};

#[derive(Clone)]
pub struct Board {
    pub cfg: Arc<Config>,
    pub store: Store,
    pub active: Arc<AtomicBool>,
    pub sending: Arc<AtomicBool>,
    pub processing: Arc<AtomicBool>,
    pub backend: Arc<RwLock<Value>>,
    pub token: String,
    pub origin: String,
}
#[derive(Default, Deserialize)]
pub struct Filters {
    pub conversation: Option<String>,
    pub before: Option<f64>,
}
pub fn source_health(enabled: bool, source: &Value, at: f64) -> Value {
    let age = source["received_at"].as_f64().map(|t| (at - t).max(0.0));
    let status = if !enabled {
        "disabled"
    } else if age.is_none_or(|v| v > 10.0) {
        "offline"
    } else if source["status"] == "watching_kakao_only" {
        "online"
    } else {
        "blocked"
    };
    json!({"status":status,"age_seconds":age,"observed_at":source["received_at"],"reason":source["status"]})
}
pub fn authorized(headers: &axum::http::HeaderMap, token: &str, origin: &str) -> bool {
    let expected_host = origin.trim_start_matches("http://");
    if headers.get(header::HOST).and_then(|v| v.to_str().ok()) != Some(expected_host) {
        return false;
    }
    if headers
        .get(header::ORIGIN)
        .is_some_and(|h| h.to_str().ok() != Some(origin))
    {
        return false;
    }
    if headers
        .get("sec-fetch-site")
        .is_some_and(|h| h == "cross-site")
    {
        return false;
    }
    let Some(given) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
    else {
        return false;
    };
    given.len() == token.len()
        && given
            .as_bytes()
            .iter()
            .zip(token.as_bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}
async fn auth(State(board): State<Board>, req: Request, next: Next) -> Response {
    if !authorized(req.headers(), &board.token, &board.origin) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(req).await
}
async fn security(req: Request, next: Next) -> Response {
    let mut r = next.run(req).await;
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    h.insert("x-content-type-options", "nosniff".parse().unwrap());
    h.insert("referrer-policy", "no-referrer".parse().unwrap());
    h.insert("content-security-policy","default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; object-src 'none'; base-uri 'none'".parse().unwrap());
    r
}
pub fn router(board: Board) -> Router {
    let api = Router::new()
        .route("/api/snapshot", get(snapshot))
        .route("/api/calls", get(calls))
        .route_layer(middleware::from_fn_with_state(board.clone(), auth));
    Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../web/index.html")) }),
        )
        .route(
            "/board.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/board.js"),
                )
            }),
        )
        .route(
            "/board.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../web/board.css"),
                )
            }),
        )
        .merge(api)
        .with_state(board)
        .layer(middleware::from_fn(security))
}
async fn snapshot(State(b): State<Board>) -> Result<Json<Value>, StatusCode> {
    let at = now();
    let raw = json_file(&b.cfg.state.join("source-kakao.json"))
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let source = source_health(b.cfg.kakao.enabled, &raw, at);
    let backend = b.backend.read().await.clone();
    let backend_fresh = backend["checked_at"]
        .as_f64()
        .is_some_and(|t| at - t < 35.0);
    let backend_online = backend["online"] == true && backend_fresh;
    let mut bindings = b
        .store
        .bindings()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    for binding in &mut bindings {
        let key = binding["key"].as_str().unwrap().to_owned();
        binding["runtime"] = backend["sessions"][&key].clone();
        let runtime_fresh = backend_online
            && binding["runtime"]["observed_at"]
                .as_f64()
                .is_some_and(|t| at - t < 35.0);
        binding["runtime_stale"] = json!(!runtime_fresh);
        let configured = binding["provider"] == "kakao"
            && binding["account"] == b.cfg.kakao.account
            && b.cfg.kakao.enabled;
        binding["configured"] = json!(configured);
        binding["call_available"] = json!(
            configured
                && source["status"] == "online"
                && backend_online
                && runtime_fresh
                && matches!(
                    binding["runtime"]["state"].as_str(),
                    Some("idle" | "active" | "notLoaded")
                )
                && b.active.load(Ordering::SeqCst)
        );
    }
    Ok(Json(
        json!({"generated_at":at,"hub":{"pid":std::process::id(),"dispatch_enabled":b.active.load(Ordering::SeqCst),"external_auto_send":b.sending.load(Ordering::SeqCst),"processing":b.processing.load(Ordering::SeqCst),"model":b.cfg.model,"effort":b.cfg.effort,"service_tier":b.cfg.service_tier},"source":source,"backend":{"online":backend_online,"checked_at":backend["checked_at"],"stale":!backend_fresh},"bindings":bindings,"adapters":b.cfg.descriptors(),"counts":b.store.status().map_err(|_|StatusCode::SERVICE_UNAVAILABLE)?,"body_logging":b.store.store_bodies}),
    ))
}
async fn calls(
    State(b): State<Board>,
    Query(q): Query<Filters>,
) -> Result<Json<Value>, StatusCode> {
    b.store
        .calls(q.conversation.as_deref(), q.before, 50)
        .map(Json)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
}
pub async fn monitor(board: Board) {
    let mut offset = 0usize;
    loop {
        let mut result = json!({"online":false,"checked_at":now(),"sessions":{}});
        let bindings = board.store.bindings().unwrap_or_default();
        // Read-only observation: never resume, subscribe, create or run a turn.
        if let Ok(Ok(mut rpc)) = tokio::time::timeout(
            Duration::from_secs(4),
            Rpc::connect(&board.cfg.app_server_socket),
        )
        .await
        {
            result["online"] = json!(true);
            let length = bindings.len();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            for b in bindings.iter().cycle().skip(offset).take(length.min(256)) {
                if tokio::time::Instant::now() > deadline {
                    break;
                }
                offset = if length > 0 { (offset + 1) % length } else { 0 };
                let key = b["key"].as_str().unwrap();
                let read = tokio::time::timeout(
                    Duration::from_secs(3),
                    rpc.call(
                        "thread/read",
                        json!({"threadId":b["thread_id"],"includeTurns":false}),
                    ),
                )
                .await;
                result["sessions"][key] = match read {
                    Ok(Ok(v)) => json!({"state":v["thread"]["status"]["type"],"observed_at":now()}),
                    _ => json!({"state":"unknown","observed_at":now()}),
                };
            }
        }
        result["checked_at"] = json!(now());
        *board.backend.write().await = result;
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}
pub async fn start(
    cfg: Arc<Config>,
    store: Store,
    active: Arc<AtomicBool>,
    sending: Arc<AtomicBool>,
    processing: Arc<AtomicBool>,
) -> Result<Option<(tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>)>> {
    let Some(settings) = &cfg.dashboard else {
        return Ok(None);
    };
    let listener = match TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, settings.port)).await {
        Ok(l) => l,
        Err(_) => {
            atomic(
                &cfg.state.join("dashboard-status.json"),
                &json!({"online":false,"reason":"port_unavailable","at":now()}),
            )?;
            return Ok(None);
        }
    };
    let origin = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let token = crate::adapters::nonce()?;
    atomic(
        &cfg.state.join("dashboard-auth.json"),
        &json!({"token":token}),
    )?;
    atomic(
        &cfg.state.join("dashboard-status.json"),
        &json!({"online":true,"url":origin,"pid":std::process::id(),"started_at":now()}),
    )?;
    let board = Board {
        cfg,
        store,
        active,
        sending,
        processing,
        backend: Arc::new(RwLock::new(json!({}))),
        token,
        origin,
    };
    let app = router(board.clone());
    let probe = tokio::spawn(monitor(board.clone()));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
        let _ = atomic(
            &board.cfg.state.join("dashboard-status.json"),
            &json!({"online":false,"reason":"server_stopped","at":now()}),
        );
    });
    Ok(Some((server, probe)))
}
