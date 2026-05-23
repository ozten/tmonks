//! Line classifier for tmux control mode.
//!
//! Control-mode lines arrive one-per-`\n` from `tmux -CC attach`. Lines fall
//! into two categories:
//!
//!   1. **Notifications**: start with `%` and identify themselves by a verb
//!      like `%output`, `%session-changed`, `%begin`. Only ever appear in
//!      the `Outside` state.
//!   2. **Block content**: arrive between `%begin <n>` and `%end <n>` /
//!      `%error <n>`. These lines may themselves begin with `%` — that's
//!      *literal pane output*, not a notification. The protocol guarantees
//!      notifications cannot be interleaved with block content.
//!
//! We expose a tiny [`LineClassifier`] state machine that consumes whole
//! lines (newline stripped) and yields zero or one [`ControlEvent`] each.

use crate::tmux::escape;
use crate::tmux::events::ControlEvent;

#[derive(Debug, Default)]
pub struct LineClassifier {
    state: State,
}

#[derive(Debug, Default)]
enum State {
    #[default]
    Outside,
    InsideBlock {
        cmd_num: u32,
        flags: u32,
    },
}

impl LineClassifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true while the classifier is mid-block (i.e., between
    /// `%begin` and `%end`/`%error`).
    pub fn in_block(&self) -> bool {
        matches!(self.state, State::InsideBlock { .. })
    }

    /// Consume one logical line (without trailing `\n`) and produce zero
    /// or one events.
    pub fn feed(&mut self, line: &[u8]) -> Option<ControlEvent> {
        match self.state {
            State::Outside => self.classify_outside(line),
            State::InsideBlock { cmd_num, flags } => self.classify_inside(line, cmd_num, flags),
        }
    }
}

impl LineClassifier {
    fn classify_outside(&mut self, line: &[u8]) -> Option<ControlEvent> {
        let line_str = std::str::from_utf8(line).ok()?;

        if let Some(rest) = line_str.strip_prefix("%output ") {
            return parse_output(rest);
        }

        if let Some(rest) = line_str.strip_prefix("%begin ") {
            let (cmd_num, flags) = parse_block_header(rest)?;
            self.state = State::InsideBlock { cmd_num, flags };
            return Some(ControlEvent::BeginBlock { cmd_num, flags });
        }

        // %end / %error can appear "outside" only as malformed protocol
        // (an %end without a matching %begin). We surface them as Ignored
        // rather than panicking — defensive against tmux bugs / version
        // skew.

        if let Some(rest) = line_str.strip_prefix("%session-changed ") {
            // Format: `%session-changed $<sid> <name>`
            let mut parts = rest.splitn(2, ' ');
            let sid = parts.next()?.to_string();
            let name = parts.next().unwrap_or("").to_string();
            return Some(ControlEvent::SessionChanged {
                session_id: sid,
                name,
            });
        }

        if line_str == "%sessions-changed" {
            return Some(ControlEvent::SessionsChanged);
        }

        if let Some(rest) = line_str.strip_prefix("%window-add ") {
            return Some(ControlEvent::WindowAdd {
                window_id: rest.to_string(),
            });
        }

        if let Some(rest) = line_str.strip_prefix("%window-close ") {
            return Some(ControlEvent::WindowClose {
                window_id: rest.to_string(),
            });
        }

        if let Some(rest) = line_str.strip_prefix("%window-renamed ") {
            let mut parts = rest.splitn(2, ' ');
            let wid = parts.next()?.to_string();
            let name = parts.next().unwrap_or("").to_string();
            return Some(ControlEvent::WindowRenamed {
                window_id: wid,
                name,
            });
        }

        if let Some(rest) = line_str.strip_prefix("%window-pane-changed ") {
            // `%window-pane-changed @<wid> %<pid>`
            let mut parts = rest.split_whitespace();
            let wid = parts.next()?.to_string();
            let pid = parts.next()?.to_string();
            return Some(ControlEvent::WindowPaneChanged {
                window_id: wid,
                pane: pid,
            });
        }

        if let Some(rest) = line_str.strip_prefix("%layout-change ") {
            let mut parts = rest.splitn(2, ' ');
            let wid = parts.next()?.to_string();
            let layout = parts.next().unwrap_or("").to_string();
            return Some(ControlEvent::LayoutChange {
                window_id: wid,
                layout,
            });
        }

        if let Some(rest) = line_str.strip_prefix("%pane-mode-changed ") {
            return Some(ControlEvent::PaneModeChanged {
                pane: rest.to_string(),
            });
        }

        if line_str == "%client-detached" || line_str.starts_with("%client-detached ") {
            return Some(ControlEvent::ClientDetached);
        }

        if line_str == "%exit" || line_str.starts_with("%exit ") {
            return Some(ControlEvent::Exit { code: None });
        }

        if line_str.starts_with("%pause")
            || line_str.starts_with("%continue")
            || line_str.starts_with("%subscription-changed")
        {
            // Documented in the plan: classify-and-ignore.
            return Some(ControlEvent::Ignored {
                kind: line_str
                    .split(' ')
                    .next()
                    .unwrap_or(line_str)
                    .to_string(),
                raw: line.to_vec(),
            });
        }

        if line_str.starts_with('%') {
            // Unknown notification. Defensive: surface it so the test
            // suite catches new tmux verbs.
            return Some(ControlEvent::Ignored {
                kind: line_str
                    .split(' ')
                    .next()
                    .unwrap_or(line_str)
                    .to_string(),
                raw: line.to_vec(),
            });
        }

        // Plain line in Outside state — neither a notification nor block
        // content. Shouldn't happen in valid tmux output; ignore.
        None
    }

    fn classify_inside(&mut self, line: &[u8], cmd_num: u32, flags: u32) -> Option<ControlEvent> {
        // Critical invariant: while inside a block, lines starting with `%`
        // are CONTENT, not notifications. Only %end / %error close the block.
        if let Ok(s) = std::str::from_utf8(line) {
            if let Some(rest) = s.strip_prefix("%end ")
                && let Some((n, _f)) = parse_block_header(rest)
                && n == cmd_num
            {
                self.state = State::Outside;
                return Some(ControlEvent::EndBlock { cmd_num, flags });
            }
            if let Some(rest) = s.strip_prefix("%error ")
                && let Some((n, _f)) = parse_block_header(rest)
                && n == cmd_num
            {
                self.state = State::Outside;
                return Some(ControlEvent::ErrorBlock { cmd_num, flags });
            }
        }

        Some(ControlEvent::BlockLine {
            cmd_num,
            line: line.to_vec(),
        })
    }
}

fn parse_output(rest: &str) -> Option<ControlEvent> {
    // `%output %<pane> <encoded>`
    let (pane, encoded) = rest.split_once(' ')?;
    if !pane.starts_with('%') {
        return None;
    }
    let data = escape::decode(encoded.as_bytes());
    Some(ControlEvent::Output {
        pane: pane.to_string(),
        data,
    })
}

/// Parse `<ts> <cmd_num> <flags>` and return `(cmd_num, flags)`.
fn parse_block_header(rest: &str) -> Option<(u32, u32)> {
    let mut parts = rest.split_whitespace();
    let _ts = parts.next()?;
    let cmd_num = parts.next()?.parse::<u32>().ok()?;
    let flags = parts.next()?.parse::<u32>().ok()?;
    Some((cmd_num, flags))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify_lines(lines: &[&[u8]]) -> Vec<ControlEvent> {
        let mut clf = LineClassifier::new();
        let mut out = Vec::new();
        for line in lines {
            if let Some(ev) = clf.feed(line) {
                out.push(ev);
            }
        }
        out
    }

    #[test]
    fn output_decodes_escapes() {
        let events = classify_lines(&[b"%output %0 hello\\012world"]);
        assert_eq!(events.len(), 1);
        match &events[0] {
            ControlEvent::Output { pane, data } => {
                assert_eq!(pane, "%0");
                assert_eq!(data.as_slice(), b"hello\nworld");
            }
            other => panic!("expected Output, got {other:?}"),
        }
    }

    #[test]
    fn session_changed_parsed() {
        let events = classify_lines(&[b"%session-changed $0 main"]);
        assert_eq!(
            events,
            vec![ControlEvent::SessionChanged {
                session_id: "$0".into(),
                name: "main".into()
            }]
        );
    }

    #[test]
    fn window_pane_changed_parsed() {
        let events = classify_lines(&[b"%window-pane-changed @3 %5"]);
        assert_eq!(
            events,
            vec![ControlEvent::WindowPaneChanged {
                window_id: "@3".into(),
                pane: "%5".into()
            }]
        );
    }

    #[test]
    fn block_classifies_pane_output_starting_with_percent_as_content() {
        // The pane prints a literal `%output ...` line. Inside a block, this
        // is CONTENT, not a notification. This is the critical invariant.
        let events = classify_lines(&[
            b"%begin 1 17 0",
            b"%output %0 fake-injection",
            b"%end 1 17 0",
        ]);
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], ControlEvent::BeginBlock { cmd_num: 17, flags: 0 }));
        match &events[1] {
            ControlEvent::BlockLine { cmd_num: 17, line } => {
                assert_eq!(line.as_slice(), b"%output %0 fake-injection");
            }
            other => panic!("expected BlockLine, got {other:?}"),
        }
        assert!(matches!(events[2], ControlEvent::EndBlock { cmd_num: 17, flags: 0 }));
    }

    #[test]
    fn end_block_matches_only_its_own_cmd_num() {
        // %end with a different cmd_num while inside block 17 must NOT close.
        // (Defensive: tmux shouldn't emit this, but we trust the wire.)
        let events = classify_lines(&[
            b"%begin 1 17 0",
            b"content-line",
            b"%end 1 99 0",
            b"%end 1 17 0",
        ]);
        assert_eq!(events.len(), 4);
        assert!(matches!(events[0], ControlEvent::BeginBlock { cmd_num: 17, flags: 0 }));
        match &events[1] {
            ControlEvent::BlockLine { cmd_num: 17, line } => {
                assert_eq!(line.as_slice(), b"content-line");
            }
            other => panic!("got {other:?}"),
        }
        match &events[2] {
            ControlEvent::BlockLine { cmd_num: 17, line } => {
                assert_eq!(line.as_slice(), b"%end 1 99 0");
            }
            other => panic!("expected BlockLine for mismatched %end, got {other:?}"),
        }
        assert!(matches!(events[3], ControlEvent::EndBlock { cmd_num: 17, flags: 0 }));
    }

    #[test]
    fn error_closes_block() {
        let events = classify_lines(&[
            b"%begin 1 5 0",
            b"oops",
            b"%error 1 5 0",
        ]);
        assert_eq!(events.len(), 3);
        assert!(matches!(events[2], ControlEvent::ErrorBlock { cmd_num: 5, flags: 0 }));
    }

    #[test]
    fn client_detached_emitted() {
        let events = classify_lines(&[b"%client-detached"]);
        assert_eq!(events, vec![ControlEvent::ClientDetached]);
    }

    #[test]
    fn exit_emitted() {
        let events = classify_lines(&[b"%exit"]);
        assert_eq!(events, vec![ControlEvent::Exit { code: None }]);
    }

    #[test]
    fn sessions_changed_emitted() {
        let events = classify_lines(&[b"%sessions-changed"]);
        assert_eq!(events, vec![ControlEvent::SessionsChanged]);
    }

    #[test]
    fn unknown_notification_surfaced_as_ignored() {
        let events = classify_lines(&[b"%brand-new-2030 some payload"]);
        match &events[0] {
            ControlEvent::Ignored { kind, .. } => assert_eq!(kind, "%brand-new-2030"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn classifier_reports_block_state() {
        let mut clf = LineClassifier::new();
        assert!(!clf.in_block());
        clf.feed(b"%begin 1 1 0");
        assert!(clf.in_block());
        clf.feed(b"some-content");
        assert!(clf.in_block());
        clf.feed(b"%end 1 1 0");
        assert!(!clf.in_block());
    }
}
