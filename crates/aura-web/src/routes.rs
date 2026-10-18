use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aura_net::enroll::RateLimiter;
use aura_node::{Content, EntryId, NodeHandle, Status};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde::{Deserialize, Serialize};
use tokio_stream::{Stream, StreamExt, wrappers::WatchStream};

use crate::session::{self, Kind, QR_TTL_SECS, SESSION_COOKIE, SESSION_TTL_SECS};

const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const STYLE_CSS: &str = include_str!("../ui/style.css");
const MANIFEST: &str = include_str!("../ui/manifest.webmanifest");
const ICON_192: &[u8] = include_bytes!("../ui/icon-192.png");
const ICON_512: &[u8] = include_bytes!("../ui/icon-512.png");
const APPLE_ICON: &[u8] = include_bytes!("../ui/apple-touch-icon.png");

/// Largest text the phone may send.
const MAX_SEND_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub(crate) struct AppState {
    node: NodeHandle,
    limiter: Arc<Mutex<RateLimiter>>,
}

pub(crate) fn router(node: NodeHandle) -> Router {
    let state = AppState {
        node,
        limiter: Arc::new(Mutex::new(RateLimiter::new(5))),
    };
    Router::new()
        .route(
            "/",
            get(|| async { asset("text/html; charset=utf-8", INDEX_HTML) }),
        )
        .route(
            "/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }),
        )
        .route(
            "/style.css",
            get(|| async { asset("text/css; charset=utf-8", STYLE_CSS) }),
        )
        .route(
            "/manifest.webmanifest",
            get(|| async { asset("application/manifest+json", MANIFEST) }),
        )
        .route(
            "/icon-192.png",
            get(|| async { asset("image/png", ICON_192) }),
        )
        .route(
            "/icon-512.png",
            get(|| async { asset("image/png", ICON_512) }),
        )
        .route(
            "/apple-touch-icon.png",
            get(|| async { asset("image/png", APPLE_ICON) }),
        )
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/history", get(history))
        .route("/api/entries/{id}/image", get(image))
        .route("/api/events", get(events))
        .route("/api/send", post(send))
        .layer(DefaultBodyLimit::max(MAX_SEND_BYTES + 4096))
        .layer(middleware::map_response(security_headers))
        .with_state(state)
}

fn asset(content_type: &'static str, body: impl IntoResponse) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

async fn security_headers(mut res: Response) -> Response {
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self' blob:; style-src 'self'; script-src 'self'; \
             connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    res
}

// ------------------------------------------------------------------ errors

pub(crate) struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct Body {
            error: String,
        }
        (self.0, Json(Body { error: self.1 })).into_response()
    }
}

fn unavailable(e: anyhow::Error) -> ApiError {
    ApiError(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}"))
}

fn unauthorized() -> ApiError {
    ApiError(StatusCode::UNAUTHORIZED, "log in first".into())
}

// ------------------------------------------------------------------ sessions

fn session_cookie(token: &str, max_age: u64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; Path=/; Max-Age={max_age}; HttpOnly; Secure; SameSite=Strict"
    ))
    .expect("token is URL-safe base64")
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == SESSION_COOKIE)
        .map(|(_, v)| v.to_string())
}

/// Checks the session cookie. Returns a refreshed cookie so an active phone stays
/// logged in (sliding expiry).
async fn authorize(state: &AppState, headers: &HeaderMap) -> Result<HeaderValue, ApiError> {
    let key = state.node.web_session_key().await.map_err(unavailable)?;
    let token = cookie_token(headers).ok_or_else(unauthorized)?;
    session::verify(&key, Kind::Session, &token, session::now_secs()).ok_or_else(unauthorized)?;
    let fresh = session::issue(&key, Kind::Session, session::now_secs() + SESSION_TTL_SECS);
    Ok(session_cookie(&fresh, SESSION_TTL_SECS))
}

#[derive(Deserialize)]
struct LoginRequest {
    passphrase: Option<String>,
    qr_token: Option<String>,
}

async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(req): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    if !state
        .limiter
        .lock()
        .unwrap()
        .allow(peer.ip().to_canonical(), Instant::now())
    {
        return Err(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "too many attempts; wait a minute".into(),
        ));
    }
    let key = state.node.web_session_key().await.map_err(unavailable)?;
    let ok = match (&req.qr_token, &req.passphrase) {
        (Some(t), _) => session::verify(&key, Kind::QrLogin, t, session::now_secs()).is_some(),
        (None, Some(p)) => state.node.verify_passphrase(p).await.map_err(unavailable)?,
        (None, None) => false,
    };
    if !ok {
        state
            .limiter
            .lock()
            .unwrap()
            .record_failure(peer.ip().to_canonical(), Instant::now());
        let msg = if req.qr_token.is_some() {
            "this QR code expired; show a new one on the PC"
        } else {
            "wrong passphrase"
        };
        return Err(ApiError(StatusCode::UNAUTHORIZED, msg.into()));
    }
    state
        .limiter
        .lock()
        .unwrap()
        .record_success(peer.ip().to_canonical());
    let token = session::issue(&key, Kind::Session, session::now_secs() + SESSION_TTL_SECS);
    Ok((
        [(header::SET_COOKIE, session_cookie(&token, SESSION_TTL_SECS))],
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response())
}

async fn logout() -> Response {
    (
        [(header::SET_COOKIE, session_cookie("", 0))],
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

// ------------------------------------------------------------------ data

#[derive(Serialize)]
struct Item {
    id: String,
    kind: String,
    /// Full text for text and HTML entries.
    text: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    size: usize,
    origin: String,
    at_ms: u64,
    /// For search results: context around the match, split into highlighted parts.
    #[serde(skip_serializing_if = "Option::is_none")]
    snippet: Option<Vec<aura_node::SnippetPart>>,
}

/// Newest entries sent without a search; the full history can be 500 entries.
const RECENT_LIMIT: usize = 50;
const SEARCH_LIMIT: usize = 50;

#[derive(Deserialize)]
struct HistoryQuery {
    q: Option<String>,
}

fn origin_name(status: &Status, origin: &aura_node::DeviceId) -> String {
    let id = origin.to_string();
    status
        .members
        .iter()
        .find(|m| m.device_id == id)
        .map(|m| m.name.clone())
        .unwrap_or_else(|| origin.short())
}

async fn history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HistoryQuery>,
) -> Result<Response, ApiError> {
    let cookie = authorize(&state, &headers).await?;
    let status = state.node.status();
    let query = params.q.as_deref().map(str::trim).unwrap_or("");
    let items: Vec<Item> = if !query.is_empty() {
        state
            .node
            .search(query, SEARCH_LIMIT)
            .await
            .map_err(unavailable)?
            .into_iter()
            .map(|r| Item {
                size: r.text.as_ref().map_or(0, |t| t.len()),
                id: r.id,
                kind: r.kind,
                text: r.text,
                width: r.width,
                height: r.height,
                origin: r.origin,
                at_ms: r.at_ms,
                snippet: Some(r.snippet),
            })
            .collect()
    } else {
        recent_items(&state, &status).await?
    };
    let body = serde_json::json!({
        "group": status.group.map(|g| g.name),
        "served_by": status.device_name,
        "query": query,
        "items": items,
    });
    Ok(([(header::SET_COOKIE, cookie)], Json(body)).into_response())
}

async fn recent_items(state: &AppState, status: &Status) -> Result<Vec<Item>, ApiError> {
    let entries = state.node.entries().await.map_err(unavailable)?;
    Ok(entries
        .iter()
        .take(RECENT_LIMIT)
        .map(|e| {
            let (text, width, height) = match &e.content {
                Content::Text(t) | Content::Html { text: t, .. } => (Some(t.clone()), None, None),
                Content::ImagePng { width, height, .. } => (None, Some(*width), Some(*height)),
            };
            Item {
                id: e.id.to_hex(),
                kind: e.content.kind().to_string(),
                text,
                width,
                height,
                size: e.content.size(),
                origin: origin_name(status, &e.id.origin),
                at_ms: e.id.hlc.wall_ms,
                snippet: None,
            }
        })
        .collect())
}

async fn image(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    authorize(&state, &headers).await?;
    let id = EntryId::from_hex(&id).ok_or(ApiError(StatusCode::BAD_REQUEST, "bad id".into()))?;
    match state.node.entry(id).await.map_err(unavailable)? {
        Some(e) => match e.content {
            Content::ImagePng { png, .. } => Ok(asset("image/png", png)),
            _ => Err(ApiError(StatusCode::NOT_FOUND, "not an image".into())),
        },
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            "no longer in the history".into(),
        )),
    }
}

/// Sends `changed` whenever the history changes, so the page refetches.
async fn events(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    authorize(&state, &headers).await?;
    let mut last: Option<(Option<String>, usize)> = None;
    let stream = WatchStream::new(state.node.watch_status()).filter_map(move |s| {
        let sig = (s.history.first().map(|h| h.id.clone()), s.history.len());
        if last.as_ref() == Some(&sig) {
            return None;
        }
        last = Some(sig);
        Some(Ok(SseEvent::default().event("changed").data("")))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[derive(Deserialize)]
struct SendRequest {
    text: String,
}

async fn send(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SendRequest>,
) -> Result<Response, ApiError> {
    authorize(&state, &headers).await?;
    if req.text.trim().is_empty() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "nothing to send".into()));
    }
    if req.text.len() > MAX_SEND_BYTES {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "text is too long".into(),
        ));
    }
    state
        .node
        .publish(Content::Text(req.text))
        .await
        .map_err(|e| ApiError(StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(Json(serde_json::json!({ "ok": true })).into_response())
}

/// A short-lived login link for the QR code shown on a PC.
pub(crate) async fn qr_token(node: &NodeHandle) -> anyhow::Result<String> {
    let key = node.web_session_key().await?;
    Ok(session::issue(
        &key,
        Kind::QrLogin,
        session::now_secs() + QR_TTL_SECS,
    ))
}
