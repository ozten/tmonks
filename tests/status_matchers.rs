//! Fixture-based regression tests for the per-CLI status matchers.
//!
//! Each `tests/fixtures/<cli>/<state>.txt` is a recorded screen render. Adding
//! a fixture + bumping the calibration version is the dev workflow when a CLI
//! changes its marker text.

use tmons::status::matchers::{Status, match_status};

fn fixture(cli: &str, state: &str) -> String {
    let path = format!("tests/fixtures/{cli}/{state}.txt");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

// Each test runs the bundled fixture through `match_status` and asserts the
// expected state.

#[test]
fn claude_working_fixture() {
    assert_eq!(
        match_status("claude", &fixture("claude", "working")),
        Status::Working
    );
}

#[test]
fn claude_needs_input_fixture() {
    assert_eq!(
        match_status("claude", &fixture("claude", "needs-input")),
        Status::NeedsInput
    );
}

#[test]
fn claude_idle_notify_fixture() {
    assert_eq!(
        match_status("claude", &fixture("claude", "idle-notify")),
        Status::IdleNotify
    );
}

#[test]
fn claude_idle_fixture() {
    assert_eq!(
        match_status("claude", &fixture("claude", "idle")),
        Status::Idle
    );
}

#[test]
fn codex_working_fixture() {
    assert_eq!(
        match_status("codex", &fixture("codex", "working")),
        Status::Working
    );
}

#[test]
fn codex_idle_fixture() {
    assert_eq!(
        match_status("codex", &fixture("codex", "idle")),
        Status::Idle
    );
}

#[test]
fn codex_needs_input_fixture() {
    assert_eq!(
        match_status("codex", &fixture("codex", "needs-input")),
        Status::NeedsInput
    );
}

#[test]
fn opencode_working_fixture() {
    assert_eq!(
        match_status("opencode", &fixture("opencode", "working")),
        Status::Working
    );
}

#[test]
fn opencode_idle_fixture() {
    assert_eq!(
        match_status("opencode", &fixture("opencode", "idle")),
        Status::Idle
    );
}

#[test]
fn version_probe_compatibility_check() {
    use tmons::status::version_probe::version_compatible;
    assert!(version_compatible("2.x", "claude-code 2.5.0"));
    assert!(!version_compatible("2.x", "claude-code 3.0.0"));
}
