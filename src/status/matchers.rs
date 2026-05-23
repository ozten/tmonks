//! Screen-content matchers for the three supported agent CLIs.
//!
//! Each matcher is compiled in with its calibration version as a code comment.
//! When a CLI updates and the markers change, the workflow is:
//!
//!   1. Record a new fixture under `tests/fixtures/<cli>/<state>.txt`
//!   2. Update the matcher rules below.
//!   3. Bump the calibration version in the comment.
//!   4. Ship a new release.
//!
//! The matcher returns [`Status::Unknown`] in two distinct cases that the
//! dashboard frontend renders differently:
//!
//!   * **Recognised command + no marker hit**: agent recognised but its state
//!     can't be inferred. UI shows a `?` overlay (signals drift to the user).
//!   * **Unrecognised command**: not an agent we know how to monitor (a shell,
//!     vim, python). UI still shows a name and `?` badge; rendering and
//!     input work through Unit 4 regardless.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Idle,
    Working,
    NeedsInput,
    IdleNotify,
    Unknown,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::NeedsInput => "needs-input",
            Self::IdleNotify => "idle-notify",
            Self::Unknown => "unknown",
        }
    }
}

/// Calibration versions noted per matcher. Update the comment + fixture when
/// shipping a release.
pub const CALIBRATION: &[(&str, &str)] = &[
    ("claude", "2.x"),
    ("codex", "0.x"),
    ("opencode", "0.x"),
];

/// Inspect `screen` (the last several rendered rows of `pane_current_command`'s
/// pane) and infer the pane's current state.
pub fn match_status(command: &str, screen: &str) -> Status {
    let canonical = canonical_command(command);
    match canonical {
        Some("claude") => match_claude(screen),
        Some("codex") => match_codex(screen),
        Some("opencode") => match_opencode(screen),
        _ => Status::Unknown,
    }
}

/// Normalise the value reported by `#{pane_current_command}` to a short name
/// our matchers can dispatch on. tmux often reports the full process path or
/// the basename; we accept either.
fn canonical_command(cmd: &str) -> Option<&'static str> {
    let lower = cmd.trim().to_ascii_lowercase();
    if lower.contains("claude") {
        Some("claude")
    } else if lower.contains("codex") {
        Some("codex")
    } else if lower.contains("opencode") {
        Some("opencode")
    } else {
        None
    }
}

// ----- Claude Code (calibrated against 2.x) -----
//
// Markers (case-insensitive):
//   * `"esc to interrupt"` → Working (model generating)
//   * `"Do you want to proceed"` or numbered options `❯ 1.` near bottom →
//     NeedsInput (tool-use confirmation, model picker, etc.)
//   * `"waiting for your input"` → IdleNotify (the 30 s nudge after no reply)
//   * Otherwise → Idle (prompt is visible, no work in flight)
fn match_claude(screen: &str) -> Status {
    let lower = screen.to_ascii_lowercase();
    if lower.contains("esc to interrupt") {
        return Status::Working;
    }
    if lower.contains("do you want to proceed")
        || screen.contains("❯ 1.")
        || screen.contains("❯ 2.")
    {
        return Status::NeedsInput;
    }
    if lower.contains("waiting for your input") {
        return Status::IdleNotify;
    }
    Status::Idle
}

// ----- Codex CLI (calibrated against 0.x) -----
//
// Markers (case-insensitive):
//   * `"press esc to interrupt"` or `"esc to interrupt"` → Working
//   * `"approve?"` → NeedsInput (tool-use confirmation)
//   * Otherwise → Idle
fn match_codex(screen: &str) -> Status {
    let lower = screen.to_ascii_lowercase();
    if lower.contains("press esc to interrupt") || lower.contains("esc to interrupt") {
        return Status::Working;
    }
    if lower.contains("approve?") || lower.contains("[a]pprove") || lower.contains("(y/n)") {
        return Status::NeedsInput;
    }
    Status::Idle
}

// ----- opencode (calibrated against 0.x) -----
fn match_opencode(screen: &str) -> Status {
    let lower = screen.to_ascii_lowercase();
    if lower.contains("thinking") || lower.contains("working") {
        return Status::Working;
    }
    if lower.contains("approve") && lower.contains("?") {
        return Status::NeedsInput;
    }
    Status::Idle
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- claude -----

    #[test]
    fn claude_working() {
        assert_eq!(
            match_status("claude", "...\nesc to interrupt\n"),
            Status::Working
        );
        // Case-insensitive
        assert_eq!(
            match_status("claude", "ESC TO INTERRUPT"),
            Status::Working
        );
    }

    #[test]
    fn claude_needs_input_proceed() {
        let screen = "Do you want to proceed?\n  1. Yes\n  2. No\n";
        assert_eq!(match_status("claude", screen), Status::NeedsInput);
    }

    #[test]
    fn claude_needs_input_numbered_choice() {
        let screen = "❯ 1. Yes\n  2. No\n";
        assert_eq!(match_status("claude", screen), Status::NeedsInput);
    }

    #[test]
    fn claude_idle_notify() {
        assert_eq!(
            match_status("claude", "Claude is waiting for your input"),
            Status::IdleNotify
        );
    }

    #[test]
    fn claude_idle_when_no_markers() {
        let screen = "╭──────╮\n│ > _  │\n╰──────╯\n";
        assert_eq!(match_status("claude", screen), Status::Idle);
    }

    // ----- codex -----

    #[test]
    fn codex_working() {
        assert_eq!(
            match_status("codex", "Press Esc to interrupt"),
            Status::Working
        );
    }

    #[test]
    fn codex_idle_when_no_markers() {
        assert_eq!(match_status("codex", "$ "), Status::Idle);
    }

    // ----- unknown commands -----

    #[test]
    fn unknown_command_returns_unknown() {
        assert_eq!(match_status("vim", "anything"), Status::Unknown);
        assert_eq!(match_status("bash", "$ "), Status::Unknown);
        assert_eq!(match_status("python", ">>> "), Status::Unknown);
    }

    #[test]
    fn empty_screen_returns_idle_for_recognised_cmd() {
        assert_eq!(match_status("claude", ""), Status::Idle);
    }

    // ----- command name normalisation -----

    #[test]
    fn canonical_command_handles_full_paths() {
        assert_eq!(canonical_command("/usr/local/bin/claude"), Some("claude"));
        assert_eq!(canonical_command("node claude"), Some("claude"));
        assert_eq!(canonical_command("opencode"), Some("opencode"));
        assert_eq!(canonical_command("OpenCode"), Some("opencode"));
        assert_eq!(canonical_command("zsh"), None);
    }

    #[test]
    fn status_serializes_as_kebab() {
        let s = serde_json::to_string(&Status::NeedsInput).unwrap();
        assert_eq!(s, "\"needs-input\"");
        let s = serde_json::to_string(&Status::IdleNotify).unwrap();
        assert_eq!(s, "\"idle-notify\"");
    }
}
