//! Parser-only tests against captured protocol fixtures.
//!
//! The line classifier is fully exercised in src/tmux/parser.rs unit tests.
//! This file adds wire-shape integration: feeding multi-line fixtures with
//! mixed notifications, blocks, and pane output.

use tmons::tmux::escape;
use tmons::tmux::events::ControlEvent;
use tmons::tmux::parser::LineClassifier;

fn feed_all(s: &str) -> Vec<ControlEvent> {
    let mut clf = LineClassifier::new();
    let mut out = Vec::new();
    for line in s.split_inclusive('\n') {
        let line = line.strip_suffix('\n').unwrap_or(line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(ev) = clf.feed(line.as_bytes()) {
            out.push(ev);
        }
    }
    out
}

#[test]
fn fixture_command_block_returns_payload() {
    // A single command response: `%begin .. 1 1\nfoo\n%end .. 1 1\n`
    let events = feed_all("%begin 1234 1 0\nfoo\n%end 1234 1 0\n");
    assert_eq!(events.len(), 3);
    assert!(matches!(events[0], ControlEvent::BeginBlock { cmd_num: 1, .. }));
    match &events[1] {
        ControlEvent::BlockLine { cmd_num: 1, line } => assert_eq!(line.as_slice(), b"foo"),
        e => panic!("expected BlockLine, got {e:?}"),
    }
    assert!(matches!(events[2], ControlEvent::EndBlock { cmd_num: 1, .. }));
}

#[test]
fn fixture_output_decodes() {
    let events = feed_all("%output %0 hello\\012world\n");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ControlEvent::Output { pane, data } => {
            assert_eq!(pane, "%0");
            assert_eq!(data.as_slice(), b"hello\nworld");
        }
        e => panic!("got {e:?}"),
    }
}

#[test]
fn fixture_output_with_backslash_and_newline_pair() {
    let events = feed_all("%output %0 \\134\\012\n");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ControlEvent::Output { pane, data } => {
            assert_eq!(pane, "%0");
            assert_eq!(data.as_slice(), b"\\\n");
        }
        e => panic!("got {e:?}"),
    }
}

#[test]
fn fixture_long_output_reassembles_unchanged() {
    // 64 KiB payload encoded as printable ASCII.
    let payload: String = "x".repeat(65536);
    let line = format!("%output %0 {payload}\n");
    let events = feed_all(&line);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ControlEvent::Output { data, .. } => assert_eq!(data.len(), 65536),
        e => panic!("got {e:?}"),
    }
}

#[test]
fn fixture_error_block_resolved_as_error() {
    let events = feed_all("%begin 1 5 0\noops\n%error 1 5 0\n");
    assert_eq!(events.len(), 3);
    assert!(matches!(events[2], ControlEvent::ErrorBlock { cmd_num: 5, .. }));
}

#[test]
fn fixture_notifications_emitted_outside_block() {
    let stream = "%session-changed $0 main\n%window-add @2\n%client-detached\n";
    let events = feed_all(stream);
    assert_eq!(events.len(), 3);
    assert_eq!(
        events[0],
        ControlEvent::SessionChanged {
            session_id: "$0".into(),
            name: "main".into()
        }
    );
    assert_eq!(events[1], ControlEvent::WindowAdd { window_id: "@2".into() });
    assert_eq!(events[2], ControlEvent::ClientDetached);
}

#[test]
fn fixture_block_content_can_start_with_percent() {
    // Pane output containing what LOOKS LIKE a notification is content while
    // inside a block.
    let stream = "%begin 1 7 0\n%not-a-notification\n%end 1 7 0\n";
    let events = feed_all(stream);
    assert_eq!(events.len(), 3);
    match &events[1] {
        ControlEvent::BlockLine { cmd_num: 7, line } => {
            assert_eq!(line.as_slice(), b"%not-a-notification");
        }
        e => panic!("got {e:?}"),
    }
}

#[test]
fn escape_decoder_handles_full_byte_range() {
    // Round-trip every byte 0-255 through the decoder.
    let mut encoded = Vec::new();
    for b in 0u8..=255u8 {
        encoded.extend_from_slice(format!("\\{:03o}", b).as_bytes());
    }
    let decoded = escape::decode(&encoded);
    assert_eq!(decoded.len(), 256);
    for (i, &b) in decoded.iter().enumerate() {
        assert_eq!(b, i as u8, "byte {i} decoded to {b}");
    }
}
