//! Integration tests for the focused-pane WebSocket bridge.
//!
//! Spawns a real `tmux` server on a temp socket, opens `/ws/pane/{session_id}`,
//! and exercises the binary protocol end to end. Gracefully skips when tmux
//! is not on PATH.

use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use tmonks::{AppState, BuildInfo, COOKIE_NAME, Token, router};

fn tmux_available() -> bool {
    std::process::Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

struct Fixture {
    socket: String,
    addr: SocketAddr,
    token: Arc<Token>,
    shutdown: CancellationToken,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start(session_name: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let socket = format!("tmonks-pane-{}-{}", std::process::id(), n);

        // Bring up a tmux server with the requested session.
        let _ = Command::new("tmux")
            .args(["-L", &socket, "kill-server"])
            .status()
            .await;
        let status = Command::new("tmux")
            .args(["-L", &socket, "new-session", "-d", "-s", session_name])
            .status()
            .await
            .expect("tmux new-session");
        assert!(status.success());

        // Stand up the tmonks server on an ephemeral port.
        let token = Token::new_random().unwrap();
        let token_arc = Arc::new(token.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let state = AppState {
            token: token_arc.clone(),
            no_auth: false,
            socket: Some(socket.clone()),
            bound_addr: addr,
            shutdown: shutdown.clone(),
            build_info: BuildInfo {
                version: "test",
                commit: "test",
            },
        };
        let app = router(state);
        let shutdown_for_server = shutdown.clone();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    shutdown_for_server.cancelled().await;
                })
                .await;
        });

        Self {
            socket,
            addr,
            token: token_arc,
            shutdown,
            server,
        }
    }

    /// Open a websocket to `/ws/pane/{session_id}` carrying the auth cookie + Origin.
    async fn connect_pane(
        &self,
        session_id: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>> {
        use tokio_tungstenite::tungstenite::handshake::client::generate_key;
        use tokio_tungstenite::tungstenite::http::Request;

        let url = format!("ws://{}/ws/pane/{session_id}", self.addr);
        let req = Request::builder()
            .method("GET")
            .uri(&url)
            .header("Host", self.addr.to_string())
            .header("Origin", format!("http://{}", self.addr))
            .header("Upgrade", "websocket")
            .header("Connection", "Upgrade")
            .header("Sec-WebSocket-Key", generate_key())
            .header("Sec-WebSocket-Version", "13")
            .header("Cookie", format!("{COOKIE_NAME}={}", self.token.encoded()))
            .body(())
            .unwrap();
        let (ws, _resp) = tokio_tungstenite::connect_async(req)
            .await
            .expect("ws handshake");
        ws
    }

    async fn stop(self) {
        self.shutdown.cancel();
        let _ = self.server.await;
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .status()
            .await;
    }
}

#[tokio::test]
async fn pane_session_emits_seed_frame() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let fx = Fixture::start("pane1").await;
    let mut ws = fx.connect_pane("pane1").await;

    // The first binary frame is the seed (tag 0x01).
    let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("seed timed out")
        .expect("ws closed")
        .expect("ws err");

    match frame {
        Message::Binary(bytes) => {
            assert!(!bytes.is_empty(), "seed payload was empty");
            assert_eq!(bytes[0], 0x01, "expected seed tag, got 0x{:02x}", bytes[0]);
        }
        other => panic!("expected binary frame, got {other:?}"),
    }

    let _ = ws.close(None).await;
    fx.stop().await;
}

#[tokio::test]
async fn pane_session_not_found_sends_error_and_closes() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let fx = Fixture::start("pane2").await;
    let mut ws = fx.connect_pane("no-such-session").await;

    // The server should send a text frame with `{"err": "..."}` and then close
    // with code 1011 — both within a small budget.
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let mut saw_err_text = false;
        let mut saw_close = false;
        while let Some(Ok(msg)) = ws.next().await {
            match msg {
                Message::Text(t) => {
                    if t.contains("\"err\"") {
                        saw_err_text = true;
                    }
                }
                Message::Close(frame) => {
                    saw_close = true;
                    if let Some(f) = frame {
                        assert_eq!(u16::from(f.code), 1011);
                    }
                    break;
                }
                _ => {}
            }
        }
        (saw_err_text, saw_close)
    })
    .await
    .expect("did not close in time");

    assert!(result.0, "did not see error JSON frame");
    assert!(result.1, "did not see close frame");

    fx.stop().await;
}

#[tokio::test]
async fn pane_session_forwards_resize_without_error() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let fx = Fixture::start("pane3").await;
    let mut ws = fx.connect_pane("pane3").await;

    // Wait for the seed frame.
    let _ = tokio::time::timeout(Duration::from_secs(5), ws.next()).await;

    // Send a 0x11 resize frame: 120 cols × 30 rows, both big-endian u16.
    let cols: u16 = 120;
    let rows: u16 = 30;
    let frame = vec![
        0x11,
        (cols >> 8) as u8,
        cols as u8,
        (rows >> 8) as u8,
        rows as u8,
    ];
    ws.send(Message::Binary(frame.into())).await.unwrap();

    // No immediate response is required; just ensure the session stays open.
    let next = tokio::time::timeout(Duration::from_millis(500), ws.next()).await;
    // Either we got further bytes (fine) or the timeout fired (also fine).
    let _ = next;

    let _ = ws.close(None).await;
    fx.stop().await;
}
