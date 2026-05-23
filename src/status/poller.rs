//! Per-session status poller.
//!
//! For each visible session, the dashboard spawns a tokio task that:
//!
//! 1. Every cadence interval, runs
//!    `tmux <-L socket?> display-message -p '#{pane_id}\t#{pane_current_command}'`
//!    → resolves the active pane and the command running in it.
//! 2. Then runs `tmux capture-pane -p -e -t %<pane> -S -5` to read the last
//!    5 rendered rows. ANSI escapes are preserved in the output but the
//!    matchers strip enough to detect their markers.
//! 3. Runs the matcher: result is one of [`Status`] variants.
//! 4. Emits `PollerEvent::StatusChanged` only when the inferred status
//!    changes from the cached last-known value.
//!
//! Error backoff (per the plan):
//!
//! * Default cadence: 750ms.
//! * On 5 consecutive errors: emit `PollerEvent::Error` and switch cadence
//!   to 3s → 9s → 27s → cap 60s.
//! * On the first success after backoff: emit a normal status event
//!   (always, even if the inferred value matches the cached one) so the
//!   "unknown" badge clears in the UI, and reset cadence to 750ms.

use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::status::matchers::{Status, match_status};
use crate::tmux::TmuxConfig;

/// Baseline poll interval. After 5 consecutive errors we back off.
pub const BASE_INTERVAL: Duration = Duration::from_millis(750);
pub const ERROR_THRESHOLD: u32 = 5;
const BACKOFFS: &[Duration] = &[
    Duration::from_secs(3),
    Duration::from_secs(9),
    Duration::from_secs(27),
    Duration::from_secs(60),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollerEvent {
    StatusChanged {
        session_id: String,
        status: Status,
        command: String,
    },
    /// Error after `ERROR_THRESHOLD` consecutive failures. The UI surfaces a
    /// small inline marker; the next successful poll clears it.
    Error {
        session_id: String,
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct PollerHandle {
    pub session_id: String,
    cancel: CancellationToken,
}

impl PollerHandle {
    pub fn stop(&self) {
        self.cancel.cancel();
    }
}

pub fn spawn(
    session_id: String,
    config: TmuxConfig,
    events: mpsc::Sender<PollerEvent>,
    parent: &CancellationToken,
) -> PollerHandle {
    let cancel = parent.child_token();
    let handle_cancel = cancel.clone();
    tokio::spawn(poll_loop(session_id.clone(), config, events, cancel));
    PollerHandle {
        session_id,
        cancel: handle_cancel,
    }
}

async fn poll_loop(
    session_id: String,
    config: TmuxConfig,
    events: mpsc::Sender<PollerEvent>,
    cancel: CancellationToken,
) {
    let mut last_status: Option<Status> = None;
    let mut consecutive_errors: u32 = 0;
    let mut backed_off: bool = false;

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(interval_for(consecutive_errors)) => {}
        }

        let result = poll_once(&config, &session_id).await;

        match result {
            Ok((cmd, screen)) => {
                let status = match_status(&cmd, &screen);
                let must_emit = backed_off || last_status != Some(status);
                if must_emit {
                    let _ = events
                        .try_send(PollerEvent::StatusChanged {
                            session_id: session_id.clone(),
                            status,
                            command: cmd,
                        });
                    last_status = Some(status);
                }
                consecutive_errors = 0;
                backed_off = false;
            }
            Err(e) => {
                consecutive_errors = consecutive_errors.saturating_add(1);
                if consecutive_errors == ERROR_THRESHOLD {
                    let _ = events.try_send(PollerEvent::Error {
                        session_id: session_id.clone(),
                        message: format!("{e:#}"),
                    });
                    last_status = None;
                    backed_off = true;
                }
            }
        }
    }
}

fn interval_for(consecutive_errors: u32) -> Duration {
    if consecutive_errors < ERROR_THRESHOLD {
        return BASE_INTERVAL;
    }
    let idx = (consecutive_errors - ERROR_THRESHOLD) as usize;
    BACKOFFS.get(idx).copied().unwrap_or(*BACKOFFS.last().unwrap())
}

/// Single poll: returns (`pane_current_command`, captured screen text).
async fn poll_once(config: &TmuxConfig, session_id: &str) -> Result<(String, String)> {
    // 1. Identify the active pane + its command.
    let mut cmd = config.command();
    cmd.args([
        "display-message",
        "-p",
        "-F",
        "#{pane_id}\t#{pane_current_command}",
        "-t",
        session_id,
    ]);
    let dm = cmd.output().await?;
    if !dm.status.success() {
        anyhow::bail!(
            "display-message failed: {}",
            String::from_utf8_lossy(&dm.stderr)
        );
    }
    let dm_text = String::from_utf8_lossy(&dm.stdout);
    let (pane_id, command) = dm_text
        .trim()
        .split_once('\t')
        .ok_or_else(|| anyhow::anyhow!("malformed display-message output: {dm_text:?}"))?;

    // 2. Capture the last 5 rows of the pane.
    let mut cmd = config.command();
    cmd.args(["capture-pane", "-p", "-e", "-t", pane_id, "-S", "-5"]);
    let cp = cmd.output().await?;
    if !cp.status.success() {
        anyhow::bail!(
            "capture-pane failed: {}",
            String::from_utf8_lossy(&cp.stderr)
        );
    }
    let screen = String::from_utf8_lossy(&cp.stdout).into_owned();
    Ok((command.to_string(), screen))
}

/// List the sessions on the server, returning `(session_id, session_name)` pairs.
/// Treats "no server running" as an empty list (the empty-state for fresh boxes).
pub async fn list_sessions(config: &TmuxConfig) -> Result<Vec<(String, String)>> {
    let mut cmd = config.command();
    cmd.args(["list-sessions", "-F", "#{session_id}\t#{session_name}"]);
    let output = cmd.output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // tmux says "no server running on /…" when the socket file is missing
        // and "error connecting to /…" when -L points at a non-existent
        // socket. Both are the "fresh box, no tmux" empty state.
        if stderr.contains("no server running") || stderr.contains("error connecting") {
            return Ok(Vec::new());
        }
        anyhow::bail!("list-sessions failed: {stderr}");
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let items = text
        .lines()
        .filter_map(|line| {
            let (id, name) = line.split_once('\t')?;
            Some((id.to_string(), name.to_string()))
        })
        .collect();
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_ladder() {
        assert_eq!(interval_for(0), BASE_INTERVAL);
        assert_eq!(interval_for(4), BASE_INTERVAL);
        assert_eq!(interval_for(5), Duration::from_secs(3));
        assert_eq!(interval_for(6), Duration::from_secs(9));
        assert_eq!(interval_for(7), Duration::from_secs(27));
        assert_eq!(interval_for(8), Duration::from_secs(60));
        assert_eq!(interval_for(100), Duration::from_secs(60));
    }
}
