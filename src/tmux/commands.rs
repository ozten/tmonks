//! Builders for the tmux commands we send over control mode.
//!
//! Every command is constructed as a single line (terminated by `\n`) so the
//! single-writer task can dispatch and correlate by sequence number.
//!
//! These are simple `String`s rather than typed values because the surface is
//! small and the on-wire shape is what we ultimately care about. We test the
//! resulting strings.

use crate::tmux::events::PaneId;

/// `display-message -p -F '<fmt>'` — print a one-line render of `fmt`.
pub fn display_message(fmt: &str) -> String {
    format!("display-message -p -F '{}'\n", escape_single_quotes(fmt))
}

/// `display-message -p -F '<fmt>' -t %<pane>` — render against a specific pane.
pub fn display_message_for_pane(fmt: &str, pane: &PaneId) -> String {
    format!(
        "display-message -p -F '{}' -t {}\n",
        escape_single_quotes(fmt),
        pane
    )
}

/// `capture-pane -p -e -t %<pane> [-S <start>]` — render the pane's screen
/// content with ANSI escape sequences preserved. `start` controls how many
/// scrollback rows to include (negative pulls older content).
pub fn capture_pane(pane: &PaneId, start: Option<i32>) -> String {
    match start {
        Some(s) => format!("capture-pane -p -e -t {pane} -S {s}\n"),
        None => format!("capture-pane -p -e -t {pane}\n"),
    }
}

/// `capture-pane -p -e -t %<pane> -S -` — pull the entire scrollback.
pub fn capture_pane_all(pane: &PaneId) -> String {
    format!("capture-pane -p -e -t {pane} -S -\n")
}

/// `select-pane -t %<pane>` — make `pane` the active pane in its window.
pub fn select_pane(pane: &PaneId) -> String {
    format!("select-pane -t {pane}\n")
}

/// `refresh-client -C <cols>x<rows>` — tell the attached client (us) that the
/// client-side viewport changed dimensions.
pub fn refresh_client_dims(cols: u16, rows: u16) -> String {
    format!("refresh-client -C {cols}x{rows}\n")
}

/// `detach-client` — gracefully detach this control-mode client.
pub fn detach_client() -> String {
    "detach-client\n".to_string()
}

/// `list-sessions -F '<fmt>'` — used by the dashboard poller in Unit 5.
pub fn list_sessions(fmt: &str) -> String {
    format!("list-sessions -F '{}'\n", escape_single_quotes(fmt))
}

/// `send-keys -t %<pane> -l <text>` — send literal text to a pane. Note: this
/// is here for completeness; the pane WS forwards keystrokes via the
/// control-mode session's stdin (`tmux -CC` treats stdin as keystrokes for
/// the focused pane), not by issuing send-keys.
pub fn send_keys_literal(pane: &PaneId, text: &str) -> String {
    format!(
        "send-keys -t {pane} -l '{}'\n",
        escape_single_quotes(text)
    )
}

/// Escape single quotes for inclusion inside a `'...'`-delimited tmux command.
/// `'` → `'\''` is the standard shell trick; tmux's command parser follows
/// shell rules here.
fn escape_single_quotes(s: &str) -> String {
    s.replace('\'', "'\\''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_message_no_pane() {
        assert_eq!(display_message("#{pane_id}"), "display-message -p -F '#{pane_id}'\n");
    }

    #[test]
    fn capture_pane_with_start() {
        assert_eq!(
            capture_pane(&"%0".into(), Some(-10000)),
            "capture-pane -p -e -t %0 -S -10000\n"
        );
    }

    #[test]
    fn capture_pane_all_uses_dash() {
        assert_eq!(capture_pane_all(&"%0".into()), "capture-pane -p -e -t %0 -S -\n");
    }

    #[test]
    fn refresh_client_dims_format() {
        assert_eq!(refresh_client_dims(80, 24), "refresh-client -C 80x24\n");
    }

    #[test]
    fn escapes_single_quotes() {
        assert_eq!(escape_single_quotes("it's"), "it'\\''s");
        assert_eq!(
            display_message("can't render"),
            "display-message -p -F 'can'\\''t render'\n"
        );
    }
}
