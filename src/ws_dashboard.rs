//! Dashboard WebSocket: `/ws/dashboard`.
//!
//! Powers the sidebar:
//!   * Initial `{type:"sessions", items:[…]}` frame on upgrade.
//!   * Repeats every 2 s if the session set changes.
//!   * One per-session status poller (Unit 5/poller.rs) at 750 ms.
//!   * `{type:"status", session_id, status, command}` on transition.
//!   * `{type:"error", session_id, message}` after 5 consecutive poll errors.
//!
//! All frames are JSON text (in contrast to the pane channel's binary
//! framing). Frequency is low enough that JSON's ergonomics win over framing
//! overhead.

use std::collections::HashMap;
use std::time::Duration;

use axum::{
    extract::{
        State,
        ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::Serialize;
use serde_json::json;
use tokio::sync::mpsc;

use crate::auth::check_origin_for_ws;
use crate::server::AppState;
use crate::status::matchers::Status;
use crate::status::poller::{self, PollerEvent, PollerHandle};
use crate::tmux::TmuxConfig;

const SESSIONS_REPOLL: Duration = Duration::from_secs(2);

pub async fn ws_dashboard_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(msg) = check_origin_for_ws(&headers) {
        tracing::warn!(reason = %msg, "/ws/dashboard Origin check failed");
        return (
            StatusCode::FORBIDDEN,
            format!("Origin check failed: {msg}\n"),
        )
            .into_response();
    }

    ws.on_upgrade(move |socket| async move {
        if let Err(e) = run_dashboard(socket, state).await {
            tracing::warn!(error = %format!("{e:#}"), "dashboard ended with error");
        }
    })
}

#[derive(Debug, Serialize)]
struct SessionItem {
    id: String,
    name: String,
}

async fn run_dashboard(mut socket: WebSocket, state: AppState) -> anyhow::Result<()> {
    let config = TmuxConfig {
        socket: state.socket.clone(),
        binary: None,
    };

    let (poller_tx, mut poller_rx) = mpsc::channel::<PollerEvent>(128);
    let mut pollers: HashMap<String, PollerHandle> = HashMap::new();

    // First snapshot.
    let initial = poller::list_sessions(&config).await.unwrap_or_default();
    send_sessions(&mut socket, &initial).await?;
    for (sid, _) in &initial {
        spawn_poller(&mut pollers, sid.clone(), &config, &poller_tx, &state);
    }
    let mut last_sessions: Vec<(String, String)> = initial;

    let mut sessions_ticker = tokio::time::interval(SESSIONS_REPOLL);
    sessions_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            _ = state.shutdown.cancelled() => break,
            _ = sessions_ticker.tick() => {
                let now = poller::list_sessions(&config).await.unwrap_or_else(|e| {
                    tracing::warn!(error = %format!("{e:#}"), "list-sessions failed");
                    last_sessions.clone()
                });
                if now != last_sessions {
                    reconcile_pollers(&mut pollers, &now, &config, &poller_tx, &state);
                    if send_sessions(&mut socket, &now).await.is_err() { break }
                    last_sessions = now;
                }
            }
            ev = poller_rx.recv() => {
                let Some(ev) = ev else { break };
                let frame = match ev {
                    PollerEvent::StatusChanged { session_id, status, command } => {
                        json!({
                            "type": "status",
                            "session_id": session_id,
                            "status": status,
                            "command": command,
                        })
                    }
                    PollerEvent::Error { session_id, message } => {
                        let _ = status_to_string(Status::Unknown); // keeps Status import alive when poller types add fields
                        json!({
                            "type": "error",
                            "session_id": session_id,
                            "message": message,
                        })
                    }
                };
                if socket.send(json_text(frame)).await.is_err() { break }
            }
            msg = socket.recv() => {
                // Browser doesn't send anything on the dashboard channel in
                // v1. Treat any message as a heartbeat and ignore close.
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => continue,
                }
            }
        }
    }

    for handle in pollers.values() {
        handle.stop();
    }
    Ok(())
}

fn status_to_string(s: Status) -> &'static str {
    s.as_str()
}

async fn send_sessions(
    socket: &mut WebSocket,
    sessions: &[(String, String)],
) -> Result<(), axum::Error> {
    let items: Vec<SessionItem> = sessions
        .iter()
        .map(|(id, name)| SessionItem {
            id: id.clone(),
            name: name.clone(),
        })
        .collect();
    let frame = json!({ "type": "sessions", "items": items });
    socket.send(json_text(frame)).await
}

fn reconcile_pollers(
    pollers: &mut HashMap<String, PollerHandle>,
    now: &[(String, String)],
    config: &TmuxConfig,
    events: &mpsc::Sender<PollerEvent>,
    state: &AppState,
) {
    let now_ids: std::collections::HashSet<&str> = now.iter().map(|(s, _)| s.as_str()).collect();

    // Drop pollers for vanished sessions.
    pollers.retain(|id, handle| {
        if now_ids.contains(id.as_str()) {
            true
        } else {
            handle.stop();
            false
        }
    });

    // Spawn pollers for new sessions.
    for (id, _) in now {
        if !pollers.contains_key(id) {
            spawn_poller(pollers, id.clone(), config, events, state);
        }
    }
}

fn spawn_poller(
    pollers: &mut HashMap<String, PollerHandle>,
    session_id: String,
    config: &TmuxConfig,
    events: &mpsc::Sender<PollerEvent>,
    state: &AppState,
) {
    let handle = poller::spawn(
        session_id.clone(),
        config.clone(),
        events.clone(),
        &state.shutdown,
    );
    pollers.insert(session_id, handle);
}

fn json_text(v: serde_json::Value) -> Message {
    Message::Text(Utf8Bytes::from(v.to_string()))
}
