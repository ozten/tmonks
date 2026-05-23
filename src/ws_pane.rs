//! Focused-pane WebSocket: `/ws/pane/{session_id}`.
//!
//! Wires the byte pipeline tmux ↔ browser:
//!
//!   tmux %output → outbound VT filter → 0x02 live frames → xterm.js
//!   xterm.js onData → 0x10 stdin frames → inbound VT filter → send-keys
//!
//! Binary framing (first byte = tag):
//!
//!   Server → client:
//!     0x01  seed (initial scrollback render)
//!     0x02  live (filtered %output bytes)
//!     0x13  scrollback-response (full scrollback, Unit 7's copy button)
//!     text  JSON error  { "err": "<reason>" }, sent before close codes
//!
//!   Client → server:
//!     0x10  stdin (bytes to send to the focused pane)
//!     0x11  resize ([cols u16 BE][rows u16 BE])
//!     0x12  request-scrollback (no payload)
//!
//! Close codes:
//!     1011  unexpected error (session not found, %client-detached, %exit,
//!           tmux child exited)
//!     1013  try again later (per-pane channel persistently overflowed —
//!           Unit 2's coalescing fallback gave up)

use std::time::Duration;

use axum::{
    extract::{
        Path, State,
        ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;

use crate::auth::check_origin_for_ws;
use crate::server::AppState;
use crate::tmux::{
    self, ControlEvent,
    commands as tcmd,
    control_mode::TmuxConfig,
};
use crate::vt_filter::InboundFilter;

// Tag bytes — see module docs.
pub const TAG_SEED: u8 = 0x01;
pub const TAG_LIVE: u8 = 0x02;
pub const TAG_SCROLLBACK_RESPONSE: u8 = 0x13;

pub const TAG_STDIN: u8 = 0x10;
pub const TAG_RESIZE: u8 = 0x11;
pub const TAG_REQUEST_SCROLLBACK: u8 = 0x12;

/// 5 MiB cap on the scrollback frame (matches Unit 7's expectation).
pub const SCROLLBACK_MAX: usize = 5 * 1024 * 1024;

/// Axum handler. The `auth_middleware` from Unit 1 has already validated the
/// Host header and cookie. The Origin check is upgrade-only and lives here.
pub async fn ws_pane_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(msg) = check_origin_for_ws(&headers) {
        tracing::warn!(reason = %msg, "/ws/pane Origin check failed");
        return (
            StatusCode::FORBIDDEN,
            format!("Origin check failed: {msg}\n"),
        )
            .into_response();
    }

    ws.on_upgrade(move |socket| async move {
        if let Err(e) = run_pane_session(socket, state, session_id).await {
            tracing::warn!(error = %format!("{e:#}"), "pane session ended with error");
        }
    })
}

async fn run_pane_session(
    mut socket: WebSocket,
    state: AppState,
    session_id: String,
) -> anyhow::Result<()> {
    let config = TmuxConfig {
        socket: state.socket.clone(),
        binary: None,
    };

    // Connect to the session.
    let connect = tmux::connect(config, &session_id, state.shutdown.child_token()).await;
    let (cm, mut events) = match connect {
        Ok(pair) => pair,
        Err(e) => {
            send_error_close(&mut socket, 1011, "session not found").await;
            anyhow::bail!("connect: {e:#}");
        }
    };

    // 1. Determine the active pane id.
    let pane_id = match tokio::time::timeout(
        Duration::from_secs(3),
        cm.send_command(tcmd::display_message("#{pane_id}")),
    )
    .await
    {
        Ok(Ok(bytes)) => parse_pane_id(&bytes),
        _ => None,
    };
    let Some(mut active_pane) = pane_id else {
        send_error_close(&mut socket, 1011, "could not determine active pane").await;
        anyhow::bail!("no pane id");
    };
    tracing::debug!(session = %session_id, pane = %active_pane, "pane session opened");

    // 2. Seed: capture the current screen state.
    let seed_bytes = match tokio::time::timeout(
        Duration::from_secs(5),
        cm.send_command(tcmd::capture_pane(&active_pane, Some(-10000))),
    )
    .await
    {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            send_error_close(&mut socket, 1011, &format!("capture-pane failed: {e:#}")).await;
            anyhow::bail!("capture-pane: {e:#}");
        }
        Err(_) => {
            send_error_close(&mut socket, 1011, "capture-pane timed out").await;
            anyhow::bail!("capture-pane timeout");
        }
    };

    // 3. Filter the captured seed and send as 0x01.
    let seed_filtered = crate::vt_filter::outbound::filter(&seed_bytes);
    let seed_frame = build_frame(TAG_SEED, &seed_filtered);
    if socket.send(Message::Binary(seed_frame.into())).await.is_err() {
        anyhow::bail!("client disconnected during seed");
    }

    // 4. Drain any `%output` events queued before now — see Unit 4 plan
    // "park-until-seed-`%end`". For alt-screen TUIs (the MVP target) these
    // bytes are already represented in the screen state we just sent.
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(0), events.recv()).await {
        // peeled; discard
    }

    // 5. Set up the inbound writer task. It drains the WS-receive half via a
    // channel; the run loop owns the WS-send half so we can interleave
    // outbound events.
    let (ws_outbound_tx, mut ws_outbound_rx) = mpsc::channel::<Message>(64);
    let cm_for_inbound = std::sync::Arc::new(cm);

    // 6. Outbound streaming filter (vte::Parser state preserved across chunks).
    let mut out_filter = crate::vt_filter::outbound::OutboundStream::new();

    let (mut ws_sink, mut ws_stream) = socket.split();

    let shutdown = state.shutdown.clone();
    let cm_writer = cm_for_inbound.clone();
    let session_id_for_log = session_id.clone();
    let active_pane_for_inbound = std::sync::Arc::new(tokio::sync::Mutex::new(active_pane.clone()));
    let active_pane_for_inbound_writer = active_pane_for_inbound.clone();

    let inbound_task = tokio::spawn(async move {
        let mut inbound_filter = InboundFilter::new();
        let mut parser: vte::Parser = vte::Parser::default();

        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                msg = ws_stream.next() => {
                    let Some(Ok(msg)) = msg else { break };
                    match msg {
                        Message::Binary(payload) => {
                            if payload.is_empty() { continue; }
                            let tag = payload[0];
                            let body = &payload[1..];
                            match tag {
                                TAG_STDIN => {
                                    if body.is_empty() { continue; }
                                    parser.advance(&mut inbound_filter, body);
                                    let filtered = inbound_filter.take();
                                    if !filtered.is_empty() {
                                        let pane = active_pane_for_inbound_writer.lock().await.clone();
                                        let _ = cm_writer
                                            .send_command_with_timeout(
                                                tcmd::send_keys_hex(&pane, &filtered),
                                                Duration::from_secs(2),
                                            )
                                            .await;
                                    }
                                }
                                TAG_RESIZE => {
                                    if body.len() != 4 {
                                        tracing::debug!("malformed resize frame: len {}", body.len());
                                        continue;
                                    }
                                    let cols = u16::from_be_bytes([body[0], body[1]]);
                                    let rows = u16::from_be_bytes([body[2], body[3]]);
                                    let _ = cm_writer
                                        .send_command_with_timeout(
                                            tcmd::refresh_client_dims(cols, rows),
                                            Duration::from_secs(2),
                                        )
                                        .await;
                                }
                                TAG_REQUEST_SCROLLBACK => {
                                    let pane = active_pane_for_inbound_writer.lock().await.clone();
                                    let result = cm_writer
                                        .send_command_with_timeout(
                                            tcmd::capture_pane_all(&pane),
                                            Duration::from_secs(10),
                                        )
                                        .await;
                                    let bytes = match result {
                                        Ok(mut b) => {
                                            if b.len() > SCROLLBACK_MAX {
                                                let marker = format!(
                                                    "[scrollback truncated to last {} MiB]\n",
                                                    SCROLLBACK_MAX / (1024 * 1024)
                                                );
                                                let keep = b.len() - SCROLLBACK_MAX + marker.len();
                                                b.drain(..keep);
                                                let mut prefixed = marker.into_bytes();
                                                prefixed.extend_from_slice(&b);
                                                prefixed
                                            } else {
                                                b
                                            }
                                        }
                                        Err(_) => Vec::new(),
                                    };
                                    let frame = build_frame(TAG_SCROLLBACK_RESPONSE, &bytes);
                                    let _ = ws_outbound_tx.send(Message::Binary(frame.into())).await;
                                }
                                _ => {
                                    tracing::debug!(tag = format!("0x{:02x}", tag), "unknown inbound tag");
                                }
                            }
                        }
                        Message::Close(_) | Message::Ping(_) | Message::Pong(_) => {
                            // Pings/pongs are auto-handled by axum's ws layer.
                            // Close ends the loop.
                            if matches!(msg, Message::Close(_)) { break }
                        }
                        Message::Text(_) => {
                            // Browser shouldn't send text frames. Ignore.
                        }
                    }
                }
            }
        }
        tracing::debug!(session = %session_id_for_log, "inbound task ended");
    });

    // 7. Main loop: route control-mode events to outbound frames; mux in
    // queued outbound responses from the inbound task.
    loop {
        tokio::select! {
            biased;
            _ = state.shutdown.cancelled() => {
                let _ = ws_sink.send(Message::Close(Some(CloseFrame {
                    code: 1001,
                    reason: Utf8Bytes::from_static("server shutting down"),
                }))).await;
                break;
            }
            queued = ws_outbound_rx.recv() => {
                let Some(msg) = queued else { break };
                if ws_sink.send(msg).await.is_err() { break }
            }
            event = events.recv() => {
                let Some(event) = event else { break };
                match event {
                    ControlEvent::Output { pane, data } => {
                        let pane_now = active_pane_for_inbound.lock().await.clone();
                        if pane != pane_now { continue }
                        out_filter.feed(&data);
                        let filtered = out_filter.take();
                        if !filtered.is_empty() {
                            let frame = build_frame(TAG_LIVE, &filtered);
                            if ws_sink.send(Message::Binary(frame.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    ControlEvent::WindowPaneChanged { pane, .. } => {
                        // Active pane within the session changed; update the
                        // filter without re-seeding.
                        *active_pane_for_inbound.lock().await = pane.clone();
                        active_pane = pane;
                    }
                    ControlEvent::ClientDetached => {
                        let _ = ws_sink.send(json_text(json!({"err": "detached"}))).await;
                        let _ = ws_sink.send(Message::Close(Some(CloseFrame {
                            code: 1011,
                            reason: Utf8Bytes::from_static("detached"),
                        }))).await;
                        break;
                    }
                    ControlEvent::Exit { .. } => {
                        let _ = ws_sink.send(json_text(json!({"err": "tmux child exited"}))).await;
                        let _ = ws_sink.send(Message::Close(Some(CloseFrame {
                            code: 1011,
                            reason: Utf8Bytes::from_static("child exited"),
                        }))).await;
                        break;
                    }
                    _ => {} // SessionChanged, WindowAdd, etc. — pane WS ignores
                }
            }
        }
    }

    inbound_task.abort();
    let _ = inbound_task.await;
    let _ = active_pane; // suppress unused-variable warning when no resize
    Ok(())
}

fn build_frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(tag);
    out.extend_from_slice(payload);
    out
}

fn json_text(v: serde_json::Value) -> Message {
    Message::Text(Utf8Bytes::from(v.to_string()))
}

async fn send_error_close(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(json_text(json!({ "err": reason })))
        .await;
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: Utf8Bytes::from(reason.to_string()),
        })))
        .await;
}

/// Parse the pane-id payload from a `display-message -p -F '#{pane_id}'`
/// response. tmux emits the format substitution followed by `\n`.
fn parse_pane_id(bytes: &[u8]) -> Option<tmux::PaneId> {
    let text = std::str::from_utf8(bytes).ok()?.trim();
    if text.starts_with('%') {
        Some(text.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pane_id_extracts_value() {
        assert_eq!(parse_pane_id(b"%5\n"), Some("%5".to_string()));
        assert_eq!(parse_pane_id(b"%42"), Some("%42".to_string()));
        assert_eq!(parse_pane_id(b"  %0  \n"), Some("%0".to_string()));
    }

    #[test]
    fn parse_pane_id_rejects_garbage() {
        assert_eq!(parse_pane_id(b"oops"), None);
        assert_eq!(parse_pane_id(b""), None);
    }

    #[test]
    fn build_frame_prefixes_tag() {
        let frame = build_frame(0x01, b"hello");
        assert_eq!(frame, vec![0x01, b'h', b'e', b'l', b'l', b'o']);
    }

    #[test]
    fn build_frame_empty_payload() {
        let frame = build_frame(0x12, &[]);
        assert_eq!(frame, vec![0x12]);
    }
}
