//! HTTP server: router wiring, state, graceful shutdown.
//!
//! Route topology (Unit 1; subsequent units add `/ws/dashboard` and
//! `/ws/pane/:id`):
//!
//!   GET  /                  -> token-handshake (if ?t=) OR index page
//!   GET  /assets/<path...>  -> embedded static asset
//!   GET  /debug/state       -> JSON diagnostics (cookie-auth)
//!
//! The middleware order matters:
//!   TraceLayer (with redaction) → auth_middleware (Host + cookie)
//!   Origin check fires *inside* WebSocket upgrade handlers (Unit 4 / Unit 5),
//!   not as middleware, so it sees the upgrade request directly.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
    http::{Method, Response, StatusCode, Uri, header},
    middleware,
    response::IntoResponse,
    routing::get,
};
use tokio_util::sync::CancellationToken;
use tower::ServiceBuilder;
use tower_http::trace::{DefaultMakeSpan, DefaultOnRequest, TraceLayer};

use crate::assets::{self, apply_security_headers};
use crate::auth::{self, Token, TokenQuery};

#[derive(Clone)]
pub struct AppState {
    pub token: Arc<Token>,
    pub no_auth: bool,
    pub socket: Option<String>,
    pub bound_addr: SocketAddr,
    /// Single shared token cancelled on SIGINT/SIGTERM. Consumed by every
    /// spawned task (WS handlers, status pollers, control-mode supervisors)
    /// in Units 2, 4, 5, and 8.
    #[allow(dead_code)]
    pub shutdown: CancellationToken,
    pub build_info: BuildInfo,
}

#[derive(Clone, Debug)]
pub struct BuildInfo {
    pub version: &'static str,
    pub commit: &'static str,
}

impl BuildInfo {
    pub fn from_env() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            commit: option_env!("TMONS_GIT_COMMIT").unwrap_or("unknown"),
        }
    }
}

impl AppState {
    pub fn new(
        token: Token,
        no_auth: bool,
        socket: Option<String>,
        bound_addr: SocketAddr,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            token: Arc::new(token),
            no_auth,
            socket,
            bound_addr,
            shutdown,
            build_info: BuildInfo::from_env(),
        }
    }
}

/// Build the Axum router with all routes and middleware wired up.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/assets/{*path}", get(assets::serve))
        .route("/debug/state", get(debug_state))
        .route("/ws/pane/{session_id}", get(crate::ws_pane::ws_pane_handler))
        .layer(
            ServiceBuilder::new()
                .layer(
                    TraceLayer::new_for_http()
                        .make_span_with(DefaultMakeSpan::new().level(tracing::Level::INFO))
                        .on_request(DefaultOnRequest::new().level(tracing::Level::DEBUG))
                        // We do NOT call `.on_response()` with header-emitting defaults,
                        // and we never log query strings. The `Uri` debug impl in spans
                        // would include `?t=…`, so we suppress it via the request hook.
                        ,
                )
                .layer(middleware::from_fn_with_state(
                    state.clone(),
                    auth::auth_middleware,
                )),
        )
        .with_state(state)
}

async fn root(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    Query(q): Query<TokenQuery>,
) -> Response<Body> {
    // If we got here with `?t=`, the auth middleware skipped the cookie check
    // so this handler is responsible for verifying the token and minting the
    // cookie.
    if q.t.is_some() {
        return auth::token_redirect(State(state.clone()), Query(q)).await;
    }

    let _ = (method, uri);
    let index_html = crate::templates::index_page().into_string();

    apply_security_headers(Response::builder().status(StatusCode::OK))
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(index_html))
        .unwrap()
}

#[derive(serde::Serialize)]
struct DebugStateResponse {
    build: BuildPayload,
    bound_addr: String,
    no_auth: bool,
    socket: Option<String>,
}

#[derive(serde::Serialize)]
struct BuildPayload {
    version: &'static str,
    commit: &'static str,
}

/// `/debug/state` returns small JSON useful when something misbehaves.
/// Subsequent units (2-8) extend the payload with live session, child,
/// poller, and channel-depth state.
async fn debug_state(State(state): State<AppState>) -> impl IntoResponse {
    let body = DebugStateResponse {
        build: BuildPayload {
            version: state.build_info.version,
            commit: state.build_info.commit,
        },
        bound_addr: state.bound_addr.to_string(),
        no_auth: state.no_auth,
        socket: state.socket.clone(),
    };

    let mut response = Json(body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY, assets::CSP.parse().unwrap());
    headers.insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    headers.insert("X-Content-Type-Options", "nosniff".parse().unwrap());
    headers.insert("Referrer-Policy", "no-referrer".parse().unwrap());
    response
}

/// Bind, return both the bound address and a future that runs the server.
pub async fn bind(addr: SocketAddr) -> Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))
}

/// Run the server until `shutdown` is cancelled.
pub async fn serve(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: CancellationToken,
) -> Result<()> {
    let shutdown_future = async move {
        shutdown.cancelled().await;
    };

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_future)
        .await
        .context("axum::serve")
}
