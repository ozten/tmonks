//! The `tmux -CC attach` subprocess driver.
//!
//! Topology (per session, one of these per browser tab):
//!
//! ```text
//!     caller ── mpsc ──► writer task ── ChildStdin ──► tmux child
//!                            │
//!                            └─ owns: cmd-num counter,
//!                                     HashMap<num, oneshot::Sender<BlockResult>>,
//!                                     ChildStdin
//!
//!     tmux child ── ChildStdout ──► reader task ── mpsc ──► caller
//!                                       │
//!                                       └─ uses: LineClassifier,
//!                                                writer's pending-map
//!                                                (via inner mpsc) to fulfil
//!                                                command futures
//! ```
//!
//! The supervisor owns both tasks plus the `Child`, accepts a `CancellationToken`
//! for shutdown, and exposes `connect`/`send_command`/`events_rx` to callers.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use pty_process::{OwnedReadPty, OwnedWritePty, Size};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::tmux::commands;
use crate::tmux::events::ControlEvent;
use crate::tmux::parser::LineClassifier;

/// Default capacity of the event channel sent to callers. `Output` events are
/// coalesced when this fills up; notifications never are.
pub const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Coalesce attempts before we treat the channel as overflowing and close
/// the connection. Surfaces in Unit 4 as a WebSocket 1013.
pub const COALESCE_LIMIT: usize = 32;

/// Per-tmux-invocation configuration. Carried in `AppState` and threaded
/// through every spawned command.
#[derive(Clone, Debug, Default)]
pub struct TmuxConfig {
    pub socket: Option<String>,
    pub binary: Option<PathBuf>,
}

impl TmuxConfig {
    /// Build a plain `tokio::process::Command` for one-shot tmux invocations
    /// (capture-pane, list-sessions, etc.) that DON'T need a pty.
    pub fn command(&self) -> Command {
        let bin = self
            .binary
            .clone()
            .unwrap_or_else(|| PathBuf::from("tmux"));
        let mut cmd = Command::new(bin);
        if let Some(socket) = &self.socket {
            cmd.arg("-L").arg(socket);
        }
        cmd
    }

    /// Build a `pty_process::Command` for `tmux -CC attach` (which requires
    /// a controlling tty).
    pub fn pty_command(&self) -> pty_process::Command {
        let bin = self
            .binary
            .clone()
            .unwrap_or_else(|| PathBuf::from("tmux"));
        let mut cmd = pty_process::Command::new(bin);
        if let Some(socket) = &self.socket {
            cmd = cmd.arg("-L").arg(socket);
        }
        cmd
    }
}

/// A single response block from tmux: either Ok(bytes) or Err(stderr-ish).
pub type BlockResult = std::result::Result<Vec<u8>, String>;

/// A request from a caller to send a command and receive its block response.
pub struct PendingCommand {
    pub line: String,
    pub respond_to: oneshot::Sender<BlockResult>,
}

/// Handle to a running tmux control-mode session. Drop to begin shutdown.
pub struct ControlMode {
    /// Send commands to the writer task. Each callsite includes a oneshot to
    /// receive the response block.
    cmd_tx: mpsc::Sender<PendingCommand>,

    /// Single shutdown trigger.
    shutdown: CancellationToken,

    /// Join handles for the spawned tasks.
    #[allow(dead_code)]
    reader_handle: tokio::task::JoinHandle<()>,
    #[allow(dead_code)]
    writer_handle: tokio::task::JoinHandle<()>,
    #[allow(dead_code)]
    supervisor_handle: tokio::task::JoinHandle<()>,
}

impl ControlMode {
    /// Send a single command and await its block response (with a default
    /// 5-second timeout to surface protocol stalls).
    pub async fn send_command(&self, line: impl Into<String>) -> Result<Vec<u8>> {
        self.send_command_with_timeout(line, Duration::from_secs(5))
            .await
    }

    pub async fn send_command_with_timeout(
        &self,
        line: impl Into<String>,
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        let pending = PendingCommand {
            line: line.into(),
            respond_to: tx,
        };
        self.cmd_tx
            .send(pending)
            .await
            .context("control-mode writer has shut down")?;

        let result = tokio::time::timeout(timeout, rx)
            .await
            .context("command timed out")?
            .context("oneshot dropped before response")?;

        result.map_err(|e| anyhow::anyhow!("tmux error: {e}"))
    }

    /// Begin shutdown: send `detach-client`, then trigger the cancel token.
    /// `detach-client` is fire-and-forget here — the supervisor races it
    /// against a 2 s timeout before falling back to start_kill.
    pub fn shutdown(&self) {
        // Best-effort: don't block on the response. The supervisor's cancel
        // arm is what guarantees teardown.
        let _ = self.cmd_tx.try_send(PendingCommand {
            line: commands::detach_client(),
            respond_to: oneshot::channel().0,
        });
        self.shutdown.cancel();
    }
}

impl Drop for ControlMode {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Spawn `tmux -CC attach -t <session>` and return a [`ControlMode`] handle
/// plus a `Receiver` for [`ControlEvent`]s that aren't tied to a block.
///
/// Block-content lines (`%begin`…`%end`) are routed to the matching command's
/// oneshot and do NOT appear on this receiver. The receiver surfaces:
///   * `Output { pane, data }` — pane bytes from `%output`
///   * `SessionChanged`, `SessionsChanged`, `Window*`, `LayoutChange`,
///     `PaneModeChanged`, `ClientDetached`, `Exit` — control-mode notifications
///   * `Ignored` — for diagnostic visibility on unknown verbs
pub async fn connect(
    config: TmuxConfig,
    session: &str,
    parent_shutdown: CancellationToken,
) -> Result<(ControlMode, mpsc::Receiver<ControlEvent>)> {
    let (pty, pts) = pty_process::open().context("open pty pair")?;
    // tmux uses tcgetattr/winsize on attach; without a sane size it warns.
    // 80x24 is the conservative default; pane WS will refresh-client -C with
    // real dimensions once the browser reports them (Unit 8).
    pty.resize(Size::new(24, 80)).ok();

    let cmd = config
        .pty_command()
        .args(["-CC", "attach", "-t", session]);

    let child = cmd.spawn(pts).with_context(|| {
        format!(
            "spawn tmux: tmux -CC attach -t {session}{}",
            config
                .socket
                .as_deref()
                .map(|s| format!(" (socket: {s})"))
                .unwrap_or_default()
        )
    })?;

    let (read_pty, write_pty) = pty.into_split();

    let (cmd_tx, cmd_rx) = mpsc::channel::<PendingCommand>(64);
    let (event_tx, event_rx) = mpsc::channel::<ControlEvent>(EVENT_CHANNEL_CAPACITY);
    let (pending_tx, pending_rx) = mpsc::channel::<oneshot::Sender<BlockResult>>(64);

    let shutdown = parent_shutdown.child_token();

    // FIFO queue of oneshots waiting for tmux to emit their `%begin flags=1`
    // block. tmux's cmd_num is a server-wide counter and cannot be used for
    // correlation — explicit-command blocks are matched in send order.
    let pending = Arc::new(Mutex::new(VecDeque::<oneshot::Sender<BlockResult>>::new()));

    let writer_handle = tokio::spawn(writer_loop(
        cmd_rx,
        pending_tx,
        write_pty,
        shutdown.clone(),
    ));

    let reader_handle = tokio::spawn(reader_loop(
        read_pty,
        pending_rx,
        pending.clone(),
        event_tx.clone(),
        shutdown.clone(),
    ));

    let supervisor_handle =
        tokio::spawn(supervisor_loop(child, pending.clone(), event_tx, shutdown.clone()));

    Ok((
        ControlMode {
            cmd_tx,
            shutdown,
            reader_handle,
            writer_handle,
            supervisor_handle,
        },
        event_rx,
    ))
}

/// Owns `ChildStdin`. Receives `PendingCommand`s, assigns each a monotonic
/// command number, registers the oneshot, and writes the command line.
///
/// The reader uses `pending_tx` to learn about each new in-flight command —
/// this is the *only* synchronisation across the two tasks for command
/// correlation, ensuring no race between "write the command" and "match the
/// %begin".
async fn writer_loop(
    mut cmd_rx: mpsc::Receiver<PendingCommand>,
    pending_tx: mpsc::Sender<oneshot::Sender<BlockResult>>,
    mut write_pty: OwnedWritePty,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            req = cmd_rx.recv() => {
                let Some(req) = req else { break };

                // Register the oneshot in FIFO order BEFORE writing — the
                // reader may see the response before this function returns.
                if pending_tx.send(req.respond_to).await.is_err() {
                    break;
                }

                if let Err(e) = write_pty.write_all(req.line.as_bytes()).await {
                    tracing::warn!(error = %e, "writing tmux command failed; shutting down");
                    break;
                }
                if let Err(e) = write_pty.flush().await {
                    tracing::warn!(error = %e, "flushing tmux command failed; shutting down");
                    break;
                }
            }
        }
    }

    drop(write_pty);
}

async fn reader_loop(
    read_pty: OwnedReadPty,
    mut pending_rx: mpsc::Receiver<oneshot::Sender<BlockResult>>,
    pending: Arc<Mutex<VecDeque<oneshot::Sender<BlockResult>>>>,
    event_tx: mpsc::Sender<ControlEvent>,
    shutdown: CancellationToken,
) {
    let mut reader = BufReader::new(read_pty);
    let mut clf = LineClassifier::new();

    /// Current in-flight explicit-command block. Set on `BeginBlock { flags:
    /// 1, .. }`, cleared on End/Error.
    struct InFlight {
        oneshot: oneshot::Sender<BlockResult>,
        buf: Vec<u8>,
    }
    let mut in_flight: Option<InFlight> = None;
    // Buffer for the *contents* of auto-emitted (flags=0) blocks — kept so
    // the test fixtures with manual flag=0 blocks still work, but otherwise
    // discarded.
    let mut auto_buf: Vec<u8> = Vec::new();

    let mut buf = Vec::with_capacity(4096);

    loop {
        buf.clear();
        let read = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            r = reader.read_until(b'\n', &mut buf) => r,
        };

        match read {
            Ok(0) => break, // EOF
            Ok(_) => {
                // Strip trailing \n / \r.
                let line = if buf.last() == Some(&b'\n') {
                    let mut end = buf.len() - 1;
                    if end > 0 && buf[end - 1] == b'\r' {
                        end -= 1;
                    }
                    &buf[..end]
                } else {
                    &buf[..]
                };

                let event = clf.feed(line);

                match event {
                    Some(ControlEvent::BeginBlock { flags: 1, .. }) => {
                        // Drain newly-queued oneshots so the FIFO is current.
                        let mut q = pending.lock().await;
                        while let Ok(sender) = pending_rx.try_recv() {
                            q.push_back(sender);
                        }
                        if let Some(oneshot) = q.pop_front() {
                            in_flight = Some(InFlight {
                                oneshot,
                                buf: Vec::new(),
                            });
                        } else {
                            // No pending caller — tmux emitted a flag=1 block
                            // we didn't ask for. Surface as Ignored and drop
                            // the content.
                            tracing::warn!(
                                "tmux emitted explicit-flag block with no pending caller"
                            );
                        }
                    }
                    Some(ControlEvent::BeginBlock { flags: _, .. }) => {
                        // Auto-emitted: reset auto_buf.
                        auto_buf.clear();
                    }
                    Some(ControlEvent::BlockLine { line, .. }) => {
                        if let Some(f) = in_flight.as_mut() {
                            f.buf.extend_from_slice(&line);
                            f.buf.push(b'\n');
                        } else {
                            auto_buf.extend_from_slice(&line);
                            auto_buf.push(b'\n');
                        }
                    }
                    Some(ControlEvent::EndBlock { flags: 1, .. }) => {
                        if let Some(f) = in_flight.take() {
                            let _ = f.oneshot.send(Ok(f.buf));
                        }
                    }
                    Some(ControlEvent::EndBlock { flags: _, .. }) => {
                        // Auto block ended; discard.
                        auto_buf.clear();
                    }
                    Some(ControlEvent::ErrorBlock { flags: 1, .. }) => {
                        if let Some(f) = in_flight.take() {
                            let msg = String::from_utf8_lossy(&f.buf).trim().to_string();
                            let _ = f.oneshot.send(Err(msg));
                        }
                    }
                    Some(ControlEvent::ErrorBlock { flags: _, .. }) => {
                        auto_buf.clear();
                    }
                    Some(ev @ ControlEvent::Output { .. }) => {
                        if forward_or_coalesce(&event_tx, ev).await.is_err() {
                            tracing::warn!(
                                "control-mode reader: consumer overflowed or gone; stopping"
                            );
                            break;
                        }
                    }
                    Some(ControlEvent::Exit { .. }) => {
                        let _ = event_tx.send(ControlEvent::Exit { code: None }).await;
                        break;
                    }
                    Some(ControlEvent::ClientDetached) => {
                        let _ = event_tx.send(ControlEvent::ClientDetached).await;
                    }
                    Some(ev) => {
                        let _ = event_tx.send(ev).await;
                    }
                    None => {}
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "tmux stdout read failed");
                break;
            }
        }
    }

    // Fail any in-flight + queued commands so callers don't hang.
    if let Some(f) = in_flight.take() {
        let _ = f
            .oneshot
            .send(Err("control-mode reader exited mid-block".to_string()));
    }
    let mut q = pending.lock().await;
    // Drain remaining registrations from the channel too.
    while let Ok(sender) = pending_rx.try_recv() {
        q.push_back(sender);
    }
    for sender in q.drain(..) {
        let _ = sender.send(Err("control-mode reader exited".to_string()));
    }
}

/// Forward an Output event, falling back to a bounded blocking send if the
/// channel is at capacity.
///
/// We never drop bytes mid-stream — the VT state machine downstream depends
/// on that invariant. But we also can't block the reader forever: if the WS
/// consumer dies without notice, an unbounded `send().await` would deadlock
/// the reader task. So we cap the back-off at [`COALESCE_LIMIT`] retries
/// with a short timeout each.
///
/// On persistent overflow, return `Err(())` so the caller (reader_loop) can
/// shut the WS down with code 1013.
async fn forward_or_coalesce(
    tx: &mpsc::Sender<ControlEvent>,
    ev: ControlEvent,
) -> Result<(), ()> {
    let mut current = match tx.try_send(ev) {
        Ok(()) => return Ok(()),
        Err(mpsc::error::TrySendError::Full(returned)) => returned,
        Err(mpsc::error::TrySendError::Closed(_)) => return Err(()),
    };
    for _ in 0..COALESCE_LIMIT {
        match tokio::time::timeout(Duration::from_millis(100), tx.reserve()).await {
            Ok(Ok(permit)) => {
                permit.send(current);
                return Ok(());
            }
            Ok(Err(_)) => return Err(()), // channel closed
            Err(_) => {
                // Timed out waiting for room. Try once more after yielding.
                match tx.try_send(current) {
                    Ok(()) => return Ok(()),
                    Err(mpsc::error::TrySendError::Full(returned)) => current = returned,
                    Err(mpsc::error::TrySendError::Closed(_)) => return Err(()),
                }
            }
        }
    }
    tracing::warn!(
        "control-mode channel persistently full after {COALESCE_LIMIT} attempts; closing"
    );
    Err(())
}

/// Owns the `Child` handle. Watches for cancel and child exit.
///
/// On cancel: send `detach-client` (via the writer is preferred, but at this
/// point the writer task may already be wound down — we go direct over a
/// scratch stdin write if needed). Wait up to 2 s for the child to exit
/// gracefully, then `start_kill`.
async fn supervisor_loop(
    mut child: Child,
    pending: Arc<Mutex<VecDeque<oneshot::Sender<BlockResult>>>>,
    event_tx: mpsc::Sender<ControlEvent>,
    shutdown: CancellationToken,
) {
    let exit = tokio::select! {
        biased;
        _ = shutdown.cancelled() => {
            // Cancellation path.
            // Try to wait for the child to exit on its own after the writer
            // has dropped stdin (which it does on cancel). Wait up to 2 s.
            match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
                Ok(Ok(status)) => Some(status),
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "child.wait() failed");
                    None
                }
                Err(_) => {
                    tracing::warn!("child did not exit within 2s; killing");
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    None
                }
            }
        }
        result = child.wait() => match result {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(error = %e, "child.wait() error");
                None
            }
        },
    };

    let exit_code = exit.and_then(|s| s.code());

    // Fail any still-pending commands.
    let mut pending = pending.lock().await;
    for sender in pending.drain(..) {
        let _ = sender.send(Err("control-mode supervisor exited".to_string()));
    }

    let _ = event_tx.send(ControlEvent::Exit { code: exit_code }).await;
    shutdown.cancel();
}

/// Probe `tmux -V` against the configured binary. Used by Unit 8's startup
/// check. Returns the raw version line so the parser there can decide what
/// to do.
pub async fn probe_version(config: &TmuxConfig) -> Result<String> {
    let mut cmd = config.command();
    cmd.arg("-V");
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = cmd
        .output()
        .await
        .with_context(|| "spawn tmux -V (is tmux installed and on PATH?)")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux -V exited non-zero: {stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
