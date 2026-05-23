//! Security and behavior tests for the outbound and inbound VT filters.
//!
//! Bytes flow tmux → outbound filter → browser, and browser → inbound filter
//! → tmux. The outbound filter is the security-critical surface (an agent
//! inside the pane can produce arbitrary escape sequences, including OSC 52
//! clipboard writes and `javascript:` hyperlinks).

use tmons::vt_filter::{InboundFilter, OutboundFilter, inbound, outbound};
use vte::Parser;

fn out(input: &[u8]) -> Vec<u8> {
    outbound::filter(input)
}

fn inb(input: &[u8]) -> Vec<u8> {
    inbound::filter(input)
}

// ----- Outbound: DEC mouse modes stripped -----

#[test]
fn outbound_drops_mouse_1000() {
    assert_eq!(out(b"\x1b[?1000h"), b"");
    assert_eq!(out(b"\x1b[?1000l"), b"");
}

#[test]
fn outbound_drops_mouse_1006() {
    assert_eq!(out(b"\x1b[?1006h"), b"");
}

#[test]
fn outbound_drops_all_mouse_modes() {
    for code in &[1000, 1002, 1003, 1004, 1005, 1006, 1015] {
        let input = format!("\x1b[?{code}h").into_bytes();
        assert_eq!(out(&input), b"", "mouse code {code} not stripped");
    }
}

#[test]
fn outbound_keeps_25_cursor_visibility() {
    assert_eq!(out(b"\x1b[?25h"), b"\x1b[?25h");
    assert_eq!(out(b"\x1b[?25l"), b"\x1b[?25l");
}

#[test]
fn outbound_keeps_2004_bracketed_paste() {
    assert_eq!(out(b"\x1b[?2004h"), b"\x1b[?2004h");
}

#[test]
fn outbound_keeps_1049_alt_screen() {
    assert_eq!(out(b"\x1b[?1049h"), b"\x1b[?1049h");
}

#[test]
fn outbound_multi_param_strips_only_banned() {
    // ?25;1000h should become ?25h (1000 dropped).
    assert_eq!(out(b"\x1b[?25;1000h"), b"\x1b[?25h");
    // ?1000;1006h → both filtered, no surviving params.
    assert_eq!(out(b"\x1b[?1000;1006h"), b"");
}

// ----- Outbound: SGR + cursor moves pass -----

#[test]
fn outbound_keeps_sgr() {
    // Note: vte loses the distinction between `\x1b[0m` and `\x1b[m` (both
    // produce a single default-0 param). The filter normalises to the
    // no-param form, which xterm.js / tmux treats identically (reset SGR).
    assert_eq!(out(b"\x1b[31mred\x1b[0m"), b"\x1b[31mred\x1b[m");
}

#[test]
fn outbound_keeps_cursor_position() {
    assert_eq!(out(b"\x1b[5;10H"), b"\x1b[5;10H");
}

#[test]
fn outbound_keeps_erase_in_display() {
    assert_eq!(out(b"\x1b[2J"), b"\x1b[2J");
}

#[test]
fn outbound_keeps_literal_text() {
    assert_eq!(out(b"hello world\n"), b"hello world\n");
}

#[test]
fn outbound_keeps_utf8() {
    assert_eq!(out("héllo".as_bytes()), "héllo".as_bytes());
}

// ----- Outbound: mouse-event output dropped -----

#[test]
fn outbound_drops_sgr_mouse_report() {
    // ESC [ < 0 ; 10 ; 20 M
    assert_eq!(out(b"\x1b[<0;10;20M"), b"");
    assert_eq!(out(b"\x1b[<0;10;20m"), b"");
}

// ----- Outbound: OSC 52 clipboard drop -----

#[test]
fn outbound_drops_osc_52_clipboard_st_terminated() {
    // OSC 52 ; c ; <base64> ST  — clipboard write attempt
    let input = b"\x1b]52;c;SGVsbG8=\x1b\\";
    assert_eq!(out(input), b"");
}

#[test]
fn outbound_drops_osc_52_bel_terminated() {
    let input = b"\x1b]52;c;SGVsbG8=\x07";
    assert_eq!(out(input), b"");
}

// ----- Outbound: OSC 8 hyperlink rules -----

#[test]
fn outbound_keeps_osc_8_https() {
    let input = b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\";
    let result = out(input);
    // Result should contain the hyperlink markers and the text.
    assert!(result.windows(20).any(|w| w == b"\x1b]8;;https://example"), "got {:?}", String::from_utf8_lossy(&result));
    assert!(result.windows(4).any(|w| w == b"link"), "got {:?}", String::from_utf8_lossy(&result));
}

#[test]
fn outbound_drops_osc_8_javascript_scheme() {
    let input = b"\x1b]8;;javascript:alert(1)\x1b\\xss\x1b]8;;\x1b\\";
    let result = out(input);
    // The dangerous OSC 8 is dropped, but the literal text "xss" passes
    // (xterm.js would render it as plain text).
    let s = String::from_utf8_lossy(&result);
    assert!(!s.contains("javascript"), "javascript: leaked into output: {s:?}");
    assert!(!s.contains("alert(1)"), "alert leaked: {s:?}");
}

#[test]
fn outbound_drops_osc_8_file_scheme() {
    let input = b"\x1b]8;;file:///etc/passwd\x1b\\";
    let result = out(input);
    let s = String::from_utf8_lossy(&result);
    assert!(!s.contains("file:"), "file: scheme leaked: {s:?}");
    assert!(!s.contains("/etc/passwd"));
}

#[test]
fn outbound_keeps_osc_8_mailto() {
    let input = b"\x1b]8;;mailto:user@example.com\x1b\\";
    let result = out(input);
    let s = String::from_utf8_lossy(&result);
    assert!(s.contains("mailto:"), "mailto: should pass: {s:?}");
}

#[test]
fn outbound_keeps_osc_8_close() {
    // Empty URL is the "end hyperlink" form.
    let input = b"\x1b]8;;\x1b\\";
    let result = out(input);
    assert!(!result.is_empty());
}

// ----- Outbound: DCS / APC / SOS / PM dropped -----

#[test]
fn outbound_drops_dcs() {
    // DCS = ESC P  ; payload ; ESC \
    let input = b"\x1bP1$rT\x1b\\";
    assert_eq!(out(input), b"");
}

#[test]
fn outbound_drops_focus_events() {
    // ?1004 set / reset
    assert_eq!(out(b"\x1b[?1004h"), b"");
    assert_eq!(out(b"\x1b[?1004l"), b"");
}

// ----- Outbound: OSC titles pass -----

#[test]
fn outbound_keeps_osc_2_window_title() {
    let input = b"\x1b]2;tmons\x1b\\";
    let result = out(input);
    assert!(!result.is_empty(), "OSC 2 dropped");
    assert!(result.windows(5).any(|w| w == b"tmons"));
}

// ----- Outbound: cross-chunk sequence safety -----

#[test]
fn outbound_streaming_handles_split_csi() {
    use tmons::vt_filter::outbound::OutboundStream;
    let mut s = OutboundStream::new();
    // First half: ESC [ ? 25
    s.feed(b"\x1b[?25");
    // Second half: ; 1000 h → should emit ESC [ ? 25 h (1000 stripped)
    s.feed(b";1000h");
    assert_eq!(s.take(), b"\x1b[?25h");
}

#[test]
fn outbound_streaming_handles_split_sgr() {
    use tmons::vt_filter::outbound::OutboundStream;
    let mut s = OutboundStream::new();
    s.feed(b"\x1b[3");
    s.feed(b"1mred\x1b[0m");
    // `\x1b[0m` normalised to `\x1b[m` (see outbound_keeps_sgr).
    assert_eq!(s.take(), b"\x1b[31mred\x1b[m");
}

// ----- Outbound: graceful recovery from invalid -----

#[test]
fn outbound_recovers_from_invalid_csi() {
    // ESC [ ? a b c h → invalid params; vte skips with ignore=true.
    // Subsequent valid input should still be filtered.
    let mut combined = Vec::new();
    combined.extend_from_slice(b"\x1b[?abch");
    combined.extend_from_slice(b"\x1b[?25h");
    let result = out(&combined);
    // The valid ?25h should pass.
    assert!(
        result.windows(6).any(|w| w == b"\x1b[?25h"),
        "expected ?25h to survive, got {:?}",
        String::from_utf8_lossy(&result)
    );
}

// ----- Inbound -----

#[test]
fn inbound_passes_plain_text() {
    assert_eq!(inb(b"hello\r"), b"hello\r");
}

#[test]
fn inbound_passes_cursor_keys() {
    assert_eq!(inb(b"\x1b[A"), b"\x1b[A");
    assert_eq!(inb(b"\x1b[B"), b"\x1b[B");
    assert_eq!(inb(b"\x1b[D"), b"\x1b[D");
}

#[test]
fn inbound_passes_ctrl_c() {
    assert_eq!(inb(b"\x03"), b"\x03");
}

#[test]
fn inbound_passes_ctrl_d() {
    assert_eq!(inb(b"\x04"), b"\x04");
}

#[test]
fn inbound_drops_dcs() {
    let input = b"\x1bP_DCS_PAYLOAD\x1b\\";
    let result = inb(input);
    let s = String::from_utf8_lossy(&result);
    assert!(!s.contains("DCS_PAYLOAD"), "DCS leaked: {s:?}");
}

#[test]
fn inbound_drops_osc_52() {
    let input = b"\x1b]52;c;evil\x1b\\";
    let result = inb(input);
    let s = String::from_utf8_lossy(&result);
    assert!(!s.contains("evil"), "OSC 52 leaked: {s:?}");
}

#[test]
fn inbound_drops_other_osc() {
    // The browser shouldn't be sending OSC at all; drop conservatively.
    let input = b"\x1b]2;evil-title\x1b\\";
    let result = inb(input);
    let s = String::from_utf8_lossy(&result);
    assert!(!s.contains("evil-title"));
}

#[test]
fn inbound_handles_huge_sequence() {
    // 8 KiB CSI sequence: ESC [ <8k chars> m
    let mut input = b"\x1b[".to_vec();
    for _ in 0..8192 {
        input.push(b'1');
        input.push(b';');
    }
    input.extend_from_slice(b"m");
    input.extend_from_slice(b"\x1b[31m"); // followed by a valid SGR

    // After the dropped huge sequence, the valid one should still be
    // forwarded.
    let result = inb(&input);
    // Note: vte's MAX_OSC_RAW / param limits may also cap things below 4 KiB
    // already; we just check that subsequent valid input parses.
    assert!(
        result.windows(5).any(|w| w == b"\x1b[31m"),
        "subsequent valid CSI lost: {:?}",
        String::from_utf8_lossy(&result)
    );
}

#[test]
fn inbound_passes_utf8() {
    assert_eq!(inb("héllo".as_bytes()), "héllo".as_bytes());
}

// ----- Confirm Perform impls aren't reset across calls (for streaming use) -----

#[test]
fn outbound_filter_can_be_reused() {
    let mut perform = OutboundFilter::new();
    let mut parser: Parser = Parser::default();
    parser.advance(&mut perform, b"\x1b[?1000h");
    assert_eq!(perform.take(), b"");
    parser.advance(&mut perform, b"hello");
    assert_eq!(perform.take(), b"hello");
}

#[test]
fn inbound_filter_can_be_reused() {
    let mut perform = InboundFilter::new();
    let mut parser: Parser = Parser::default();
    parser.advance(&mut perform, b"hello");
    assert_eq!(perform.take(), b"hello");
    parser.advance(&mut perform, b"world");
    assert_eq!(perform.take(), b"world");
}
