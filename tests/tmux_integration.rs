//! Integration tests against a real `tmux` server.
//!
//! These tests:
//!   1. Pick an isolated socket name in a `tempfile::TempDir`.
//!   2. `tmux new-session -d` to start the server with one session.
//!   3. `control_mode::connect` and drive commands.
//!   4. `tmux kill-server` cleanup.
//!
//! `#[ignore]` is NOT applied — they run by default. If tmux 3.4+ isn't on
//! PATH, they short-circuit early with a clear skip message rather than fail.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use tmons::tmux::commands;
use tmons::tmux::events::ControlEvent;
use tmons::tmux::{TmuxConfig, connect, probe_version};

fn tmux_available() -> bool {
    std::process::Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

struct TmuxFixture {
    socket: String,
    config: TmuxConfig,
    // Holds the temp dir so it lives until the fixture is dropped.
    #[allow(dead_code)]
    tempdir: tempfile::TempDir,
}

impl TmuxFixture {
    async fn start(session_name: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);

        let tempdir = tempfile::tempdir().unwrap();
        // The tmux socket is `$TMUX_TMPDIR/tmux-$UID/$socket`. We rely on the
        // socket name being unique per fixture (cargo test runs tests in
        // parallel within the same process). Don't set TMUX_TMPDIR — it's a
        // process-global env var and would race with other fixtures.
        let socket = format!("tmons-test-{}-{}", std::process::id(), n);
        let config = TmuxConfig {
            socket: Some(socket.clone()),
            binary: Some(PathBuf::from("tmux")),
        };

        // Kill any leftover server on this socket from a prior failed run.
        let _ = Command::new("tmux")
            .args(["-L", &socket, "kill-server"])
            .status()
            .await;

        // Start a new session detached.
        let status = Command::new("tmux")
            .args(["-L", &socket, "new-session", "-d", "-s", session_name])
            .status()
            .await
            .expect("spawn tmux new-session");
        assert!(status.success(), "tmux new-session failed");

        Self {
            socket,
            config,
            tempdir,
        }
    }

    async fn stop(self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .status()
            .await;
    }
}

#[tokio::test]
async fn probe_version_returns_string() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let cfg = TmuxConfig::default();
    let v = probe_version(&cfg).await.unwrap();
    assert!(v.starts_with("tmux"), "got {v:?}");
}

#[tokio::test]
async fn connect_drives_display_message_command() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let fx = TmuxFixture::start("itest").await;
    let token = CancellationToken::new();
    let (cm, mut events) = connect(fx.config.clone(), "itest", token.clone())
        .await
        .expect("connect");

    // Drain initial seed bytes (control-mode pumps a few %output frames on
    // attach). We don't care here — we just await a deterministic command.
    let drain = tokio::spawn(async move {
        loop {
            match tokio::time::timeout(Duration::from_millis(50), events.recv()).await {
                Ok(Some(ControlEvent::Exit { .. })) | Ok(None) => break,
                Ok(Some(_)) => continue,
                Err(_) => break,
            }
        }
    });

    let payload = cm
        .send_command(commands::display_message("ok-string-123"))
        .await
        .expect("display-message");

    let text = String::from_utf8_lossy(&payload);
    assert!(text.contains("ok-string-123"), "got {text:?}");

    cm.shutdown();
    drop(cm);
    let _ = drain.await;
    fx.stop().await;
}

#[tokio::test]
async fn detach_client_resolves_command_and_supervisor_exits() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let fx = TmuxFixture::start("itest2").await;
    let token = CancellationToken::new();
    let (cm, mut events) = connect(fx.config.clone(), "itest2", token.clone())
        .await
        .expect("connect");

    // Issue detach-client. We expect the supervisor to eventually emit Exit.
    let _ = cm.send_command(commands::detach_client()).await;

    // Drain events until we see Exit or a timeout.
    let saw_exit = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match events.recv().await {
                Some(ControlEvent::Exit { .. }) => return true,
                Some(_) => continue,
                None => return true,
            }
        }
    })
    .await
    .unwrap_or(false);

    assert!(saw_exit, "expected Exit event after detach-client");
    drop(cm);
    fx.stop().await;
}

#[tokio::test]
async fn cancellation_kills_child_within_timeout() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let fx = TmuxFixture::start("itest3").await;
    let token = CancellationToken::new();
    let (cm, mut events) = connect(fx.config.clone(), "itest3", token.clone())
        .await
        .expect("connect");

    let start = std::time::Instant::now();
    cm.shutdown();
    drop(cm);

    // Within 3 s the supervisor should send Exit.
    let saw_exit = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match events.recv().await {
                Some(ControlEvent::Exit { .. }) | None => return true,
                Some(_) => continue,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(saw_exit, "expected Exit within 3s of shutdown");
    assert!(start.elapsed() < Duration::from_secs(5), "shutdown too slow");

    fx.stop().await;
}
