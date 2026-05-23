//! Integration: spin up real tmux, run `poller::list_sessions` and a
//! `poller::spawn` task, observe events.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use tmons::status::matchers::Status;
use tmons::status::poller::{self, PollerEvent};
use tmons::tmux::TmuxConfig;

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
    config: TmuxConfig,
}

impl Fixture {
    async fn start(session_name: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let socket = format!("tmons-dash-{}-{}", std::process::id(), n);
        let config = TmuxConfig {
            socket: Some(socket.clone()),
            binary: Some(PathBuf::from("tmux")),
        };

        let _ = Command::new("tmux")
            .args(["-L", &socket, "kill-server"])
            .status()
            .await;
        let status = Command::new("tmux")
            .args(["-L", &socket, "new-session", "-d", "-s", session_name])
            .status()
            .await
            .expect("new-session");
        assert!(status.success());
        Self { socket, config }
    }

    async fn stop(self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .status()
            .await;
    }
}

#[tokio::test]
async fn list_sessions_empty_when_no_server() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    // Use a socket that has no server running.
    let config = TmuxConfig {
        socket: Some(format!("tmons-no-server-{}", std::process::id())),
        binary: Some(PathBuf::from("tmux")),
    };
    let result = poller::list_sessions(&config).await.unwrap();
    assert!(result.is_empty());
}

#[tokio::test]
async fn list_sessions_returns_active_sessions() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let fx = Fixture::start("dash1").await;
    let sessions = poller::list_sessions(&fx.config).await.unwrap();
    assert_eq!(sessions.len(), 1, "got {sessions:?}");
    assert_eq!(sessions[0].1, "dash1");
    assert!(sessions[0].0.starts_with('$'));
    fx.stop().await;
}

#[tokio::test]
async fn poller_emits_initial_status_for_known_command() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let fx = Fixture::start("dash2").await;
    let (tx, mut rx) = mpsc::channel::<PollerEvent>(16);
    let cancel = CancellationToken::new();
    let _handle = poller::spawn(
        "dash2".to_string(),
        fx.config.clone(),
        tx,
        &cancel,
    );

    let event = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
    cancel.cancel();
    fx.stop().await;

    let event = event.expect("timed out").expect("channel closed");
    match event {
        PollerEvent::StatusChanged { session_id, status, command: _ } => {
            assert_eq!(session_id, "dash2");
            // Shell session (no agent) → Unknown.
            assert_eq!(status, Status::Unknown);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn poller_does_not_emit_on_unchanged_status() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let fx = Fixture::start("dash3").await;
    let (tx, mut rx) = mpsc::channel::<PollerEvent>(16);
    let cancel = CancellationToken::new();
    let _handle = poller::spawn(
        "dash3".to_string(),
        fx.config.clone(),
        tx,
        &cancel,
    );

    // First event should arrive quickly.
    let first = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
    assert!(first.is_ok());

    // Now wait 1.5s: status doesn't change, so no further events expected.
    let next = tokio::time::timeout(Duration::from_millis(1500), rx.recv()).await;
    cancel.cancel();
    fx.stop().await;
    assert!(next.is_err(), "got unexpected status event: {next:?}");
}
