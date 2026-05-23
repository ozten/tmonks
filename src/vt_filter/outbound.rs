//! Outbound (tmux → browser) VT filter.
//!
//! Implements `vte::Perform` as an allowlist of re-emitted actions. Every
//! action that doesn't match the allowlist is dropped (with a `tracing::debug!`
//! diagnostic so the list can be tuned against new TUI behavior).
//!
//! Security drops (with test coverage in tests/vt_filter.rs):
//!   * **OSC 52** (clipboard write) — prevents an agent inside the pane from
//!     silently writing to the user's OS clipboard.
//!   * **OSC 8** with non-`http`/`https`/`mailto` URL schemes — prevents
//!     `javascript:` hyperlink injection.
//!   * **DEC mouse modes** 1000/1002/1003/1004/1005/1006/1015 — xterm.js
//!     would otherwise be the inner app's mouse-tracking proxy; we want
//!     browser-native selection.
//!   * **DCS / APC / SOS / PM** — these have caused CVEs in tmux, less, vim.
//!   * **Focus events** (`?1004`) — leak focus tracking that the user may
//!     prefer the browser handle independently.

use std::fmt::Write as _;

use vte::{Params, Parser, Perform};

/// Run `input` through the outbound filter and return the cleaned bytes.
pub fn filter(input: &[u8]) -> Vec<u8> {
    let mut perform = OutboundFilter::new();
    let mut parser: Parser = Parser::default();
    parser.advance(&mut perform, input);
    perform.take()
}

/// Streaming filter — accumulate input and read out the cleaned bytes when
/// it suits the caller. The internal `vte::Parser` retains state across
/// `feed()` calls so sequences split across chunks are handled correctly.
pub struct OutboundStream {
    parser: Parser,
    perform: OutboundFilter,
}

impl Default for OutboundStream {
    fn default() -> Self {
        Self::new()
    }
}

impl OutboundStream {
    pub fn new() -> Self {
        Self {
            parser: Parser::default(),
            perform: OutboundFilter::new(),
        }
    }

    pub fn feed(&mut self, input: &[u8]) {
        self.parser.advance(&mut self.perform, input);
    }

    pub fn take(&mut self) -> Vec<u8> {
        self.perform.take()
    }
}

/// DEC private mode codes we strip (never re-emit). These are all
/// mouse-tracking and focus-tracking modes; xterm.js / the browser handles
/// selection natively.
const BANNED_PRIVATE_MODES: &[u16] = &[
    1000, // X10 mouse tracking
    1002, // button-event mouse
    1003, // any-event mouse
    1004, // focus events
    1005, // UTF-8 mouse mode
    1006, // SGR mouse mode
    1015, // urxvt mouse mode
];

pub struct OutboundFilter {
    out: Vec<u8>,
}

impl Default for OutboundFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl OutboundFilter {
    pub fn new() -> Self {
        Self {
            out: Vec::with_capacity(1024),
        }
    }

    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    fn write_u8(&mut self, b: u8) {
        self.out.push(b);
    }

    fn write_bytes(&mut self, b: &[u8]) {
        self.out.extend_from_slice(b);
    }
}

impl Perform for OutboundFilter {
    fn print(&mut self, c: char) {
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        self.write_bytes(s.as_bytes());
    }

    fn execute(&mut self, byte: u8) {
        // Allow common C0 controls that pane redraws depend on.
        match byte {
            0x07 // BEL
            | 0x08 // BS
            | 0x09 // HT
            | 0x0a // LF
            | 0x0b // VT
            | 0x0c // FF
            | 0x0d // CR
            | 0x1b // ESC (shouldn't normally arrive as execute, defensive)
            => self.write_u8(byte),
            _ => {
                // SI/SO/etc. are rare in alt-screen TUIs and tend to be
                // hostile (charset switches outside our allowlist). Drop.
                tracing::debug!(byte = format!("0x{:02x}", byte), "drop execute byte");
            }
        }
    }

    // ---- DCS dropped entirely ----
    fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: char) {
        tracing::debug!("drop DCS start");
    }
    fn put(&mut self, _: u8) {}
    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.is_empty() {
            return;
        }
        let code = params[0];

        // OSC 52 — clipboard. Drop unconditionally.
        if code == b"52" {
            tracing::debug!("drop OSC 52 (clipboard write attempt)");
            return;
        }

        // OSC 8 — hyperlink. Allow only safe schemes.
        if code == b"8" {
            // OSC 8 ; params ; URL  (params holds optional id)
            let url = params.get(2).copied().unwrap_or(b"");
            // An empty URL is the "end hyperlink" form — always safe.
            if !url.is_empty() {
                let Ok(url_str) = std::str::from_utf8(url) else {
                    tracing::debug!("drop OSC 8 (non-UTF8 URL)");
                    return;
                };
                let Ok(parsed) = url::Url::parse(url_str) else {
                    tracing::debug!(url = url_str, "drop OSC 8 (unparsable URL)");
                    return;
                };
                let scheme = parsed.scheme().to_ascii_lowercase();
                if !matches!(scheme.as_str(), "http" | "https" | "mailto") {
                    tracing::debug!(scheme = %scheme, "drop OSC 8 (unsafe scheme)");
                    return;
                }
            }
            self.emit_osc(params);
            return;
        }

        // Allowlist of OSC commands we know are benign and useful for browser
        // rendering. Expand carefully; each addition is a small surface
        // increase.
        let allowed = matches!(
            code,
            b"0" | b"1" | b"2"        // icon name + title
                | b"4"                 // indexed colour
                | b"7"                 // cwd notification
                | b"10" | b"11" | b"12"// fg/bg/cursor colour
                | b"104" | b"110" | b"111" | b"112" // reset colour
        );
        if allowed {
            self.emit_osc(params);
        } else {
            tracing::debug!(
                code = String::from_utf8_lossy(code).to_string(),
                "drop OSC (not in allowlist)"
            );
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            tracing::debug!("drop CSI (ignore=true)");
            return;
        }

        // DEC private mode set/reset (?Nh / ?Nl). Re-emit with mouse / focus
        // codes stripped from the param list.
        if intermediates == b"?" && (action == 'h' || action == 'l') {
            let surviving: Vec<u16> = params
                .iter()
                .map(|sub| sub.first().copied().unwrap_or(0))
                .filter(|n| !BANNED_PRIVATE_MODES.contains(n))
                .collect();
            if surviving.is_empty() {
                tracing::debug!("drop DEC private mode CSI (all params filtered)");
                return;
            }
            let mut buf = String::from("\x1b[?");
            for (i, n) in surviving.iter().enumerate() {
                if i > 0 {
                    buf.push(';');
                }
                let _ = write!(buf, "{n}");
            }
            buf.push(action);
            self.write_bytes(buf.as_bytes());
            return;
        }

        // Mouse-event output (CSI M ... legacy; CSI <...M / CSI <...m SGR).
        if intermediates.is_empty() && action == 'M' {
            // Could also be ANSI "Delete Line" (CSI Pn M). Ambiguous.
            // ANSI DL has params, mouse legacy doesn't. Treat empty-params
            // 'M' as mouse and drop; non-empty as DL and allow.
            if params.is_empty() || params.iter().all(|s| s.iter().all(|p| *p == 0)) {
                tracing::debug!("drop legacy mouse report");
                return;
            }
            self.emit_csi(params, intermediates, action);
            return;
        }
        if intermediates == b"<" && (action == 'M' || action == 'm') {
            tracing::debug!("drop SGR mouse report");
            return;
        }

        // Normal CSI: allowlist by action character.
        let standard_action = intermediates.is_empty()
            && matches!(
                action,
                // Cursor movement
                'A' | 'B' | 'C' | 'D' | 'E' | 'F' | 'G' | 'H' | 'f'
                | 'I' | 'Z'                       // tab forward/back
                | 'd' | 'e' | '`' | 'a'           // line / column positioning
                | 'J' | 'K' | 'L' | 'M' | 'P' | 'X' | 'S' | 'T' // erase / insert / delete / scroll
                | '@'                              // ICH insert chars
                | 'm'                              // SGR (colors, attrs)
                | 'r'                              // scrolling region
                | 's' | 'u'                        // save/restore cursor
                | 'h' | 'l'                        // mode set/reset (non-private)
                | 'n' | 'c'                        // device status / attrs report (passthrough OK)
                | 't'                              // window manipulation (mostly informational)
            );

        // DEC private save/restore (CSI ? Pm s / u) — allow.
        let dec_save_restore = intermediates == b"?" && (action == 's' || action == 'u');

        if standard_action || dec_save_restore {
            self.emit_csi(params, intermediates, action);
            return;
        }

        tracing::debug!(
            intermediates = String::from_utf8_lossy(intermediates).to_string(),
            action = %action,
            "drop CSI (not in allowlist)"
        );
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        // Allow charset selection ( ) * + with a final byte (B, 0, etc.),
        // DECSC / DECRC (`7` / `8`), and RIS (`c`).
        match (intermediates, byte) {
            (b"(", _) | (b")", _) | (b"*", _) | (b"+", _) => {
                let mut buf = vec![0x1b];
                buf.extend_from_slice(intermediates);
                buf.push(byte);
                self.write_bytes(&buf);
            }
            ([], b'7') | ([], b'8') | ([], b'c') | ([], b'=') | ([], b'>') | ([], b'D')
            | ([], b'E') | ([], b'H') | ([], b'M') => {
                self.write_bytes(&[0x1b, byte]);
            }
            _ => {
                tracing::debug!(
                    intermediates = String::from_utf8_lossy(intermediates).to_string(),
                    byte = format!("0x{:02x}", byte),
                    "drop ESC dispatch"
                );
            }
        }
    }
}

impl OutboundFilter {
    fn emit_csi(&mut self, params: &Params, intermediates: &[u8], action: char) {
        let mut buf = vec![0x1b, b'['];
        buf.extend_from_slice(intermediates);

        if !is_default_params(params) {
            let mut first = true;
            for sub in params.iter() {
                if !first {
                    buf.push(b';');
                }
                for (i, p) in sub.iter().enumerate() {
                    if i > 0 {
                        buf.push(b':');
                    }
                    let _ = write!(&mut StringVec(&mut buf), "{p}");
                }
                first = false;
            }
        }
        let mut tmp = [0u8; 4];
        buf.extend_from_slice(action.encode_utf8(&mut tmp).as_bytes());
        self.write_bytes(&buf);
    }

    fn emit_osc(&mut self, params: &[&[u8]]) {
        let mut buf = vec![0x1b, b']'];
        for (i, p) in params.iter().enumerate() {
            if i > 0 {
                buf.push(b';');
            }
            buf.extend_from_slice(p);
        }
        // ST = ESC \ (terminator).
        buf.extend_from_slice(&[0x1b, b'\\']);
        self.write_bytes(&buf);
    }
}

/// `vte` always emits at least one parameter (a default `0`) at dispatch time,
/// even when the on-wire sequence had none. To preserve the on-wire shape we
/// detect "single subparam with value `[0]`" and treat it as "no params".
fn is_default_params(params: &Params) -> bool {
    let mut it = params.iter();
    match (it.next(), it.next()) {
        (Some(sub), None) => sub == [0],
        _ => false,
    }
}

/// Tiny adapter so we can `write!` into a `Vec<u8>` cheaply.
struct StringVec<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for StringVec<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}
