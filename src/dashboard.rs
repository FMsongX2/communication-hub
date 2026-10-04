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
    routing::{get, post},
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
        .route("/api/rooms", get(rooms).post(update_room))
        .route("/api/rooms/verify", post(verify_room))
        .route("/api/rooms/available", get(available_rooms))
        .route("/api/rooms/add", post(add_room))
        .route("/api/rooms/remove", post(remove_room))
        .route("/api/rooms/reset-context", post(reset_context))
        .route("/api/settings", post(update_settings))
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
    Ok(Json(
        json!({"generated_at":at,"hub":{"pid":std::process::id(),"dispatch_enabled":b.active.load(Ordering::SeqCst),"external_auto_send":b.sending.load(Ordering::SeqCst),"processing":b.processing.load(Ordering::SeqCst),"model":b.cfg.model,"effort":b.cfg.effort,"service_tier":b.cfg.service_tier},"source":source,"backend":{"online":backend_online,"checked_at":backend["checked_at"],"stale":!backend_fresh},"adapters":b.cfg.descriptors(),"counts":b.store.status().map_err(|_|StatusCode::SERVICE_UNAVAILABLE)?,"body_logging":b.store.store_bodies}),
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
async fn rooms(State(b): State<Board>) -> Result<Json<Value>, StatusCode> {
    let fail = |_| StatusCode::SERVICE_UNAVAILABLE;
    Ok(Json(json!({"rooms":b.store.rooms().map_err(fail)?,
        "answer_unapproved_rooms":b.store.answer_unapproved_rooms().map_err(fail)?,
        "yumi_configured":b.cfg.yumi.is_some()})))
}
#[derive(Deserialize)]
pub struct RoomUpdate {
    pub key: String,
    pub approved: bool,
    pub yui: bool,
    pub yumi: bool,
}
async fn update_room(
    State(b): State<Board>,
    Json(u): Json<RoomUpdate>,
) -> Result<Json<Value>, StatusCode> {
    match b.store.update_room(&u.key, u.approved, u.yui, u.yumi) {
        Ok(true) => Ok(Json(json!({"updated":true}))),
        Ok(false) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}
#[derive(Deserialize)]
pub struct RoomKey {
    pub key: String,
}
/// Runs the sender's list-only uniqueness check for a room; nothing is opened or written.
async fn verify_room(
    State(b): State<Board>,
    Json(r): Json<RoomKey>,
) -> Result<Json<Value>, StatusCode> {
    let adapter = crate::adapters::Kakao {
        cfg: (*b.cfg).clone(),
    };
    adapter
        .verify_room(&b.store, &r.key)
        .await
        .map(Json)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
}
/// The chat list's room names, read through the sender on the owner's request only.
async fn available_rooms(State(b): State<Board>) -> Result<Json<Value>, StatusCode> {
    crate::adapters::Kakao {
        cfg: (*b.cfg).clone(),
    }
    .list_rooms(&b.store)
    .await
    .map(Json)
    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
}
#[derive(Deserialize)]
pub struct RoomAdd {
    pub title: String,
    pub yui: bool,
    pub yumi: bool,
}
/// Adds a room by name once the chat list proves the name unique.
async fn add_room(
    State(b): State<Board>,
    Json(a): Json<RoomAdd>,
) -> Result<Json<Value>, StatusCode> {
    let receipt = crate::adapters::Kakao {
        cfg: (*b.cfg).clone(),
    }
    .verify_name(&a.title)
    .await
    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let Some(rows) = receipt["list_rows"]
        .as_i64()
        .filter(|_| receipt["status"] == "ready")
    else {
        return Ok(Json(
            json!({"added":false,"status":receipt["status"],"reason":receipt["reason"]}),
        ));
    };
    b.store
        .add_room_by_name("kakao", &b.cfg.kakao.account, &a.title, a.yui, a.yumi, rows)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(json!({"added":true,"list_rows":rows})))
}
/// Earlier exchanges in the room stop being passed to Yui and Yumi.
async fn reset_context(
    State(b): State<Board>,
    Json(r): Json<RoomKey>,
) -> Result<Json<Value>, StatusCode> {
    match b.store.reset_room_context(&r.key) {
        Ok(true) => Ok(Json(json!({"reset":true}))),
        Ok(false) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}
async fn remove_room(
    State(b): State<Board>,
    Json(r): Json<RoomKey>,
) -> Result<Json<Value>, StatusCode> {
    match b.store.remove_room(&r.key) {
        Ok(true) => Ok(Json(json!({"removed":true}))),
        Ok(false) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}
#[derive(Deserialize)]
pub struct Settings {
    pub answer_unapproved_rooms: bool,
}
async fn update_settings(
    State(b): State<Board>,
    Json(s): Json<Settings>,
) -> Result<Json<Value>, StatusCode> {
    b.store
        .set_answer_unapproved_rooms(s.answer_unapproved_rooms)
        .map(|_| Json(json!({"updated":true})))
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
}
/// Backend health only: a connection handshake every 15 s. Calls are stateless, so no thread is
/// read, resumed or created here.
pub async fn monitor(board: Board) {
    loop {
        let online = matches!(
            tokio::time::timeout(
                Duration::from_secs(4),
                Rpc::connect(&board.cfg.app_server_socket)
            )
            .await,
            Ok(Ok(_))
        );
        *board.backend.write().await = json!({"online":online,"checked_at":now()});
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
