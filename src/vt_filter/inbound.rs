//! Inbound (browser → tmux) VT filter.
//!
//! Defends against a compromised browser context (XSS, hostile extension,
//! future bug) injecting dangerous escape sequences into the user's pane.
//!
//! Less aggressive than the outbound filter: most user input is plain text
//! and well-formed CSI (cursor keys, arrow keys, etc.) that we pass through.
//! We specifically drop:
//!
//!   * **DCS / APC / SOS / PM** — these have caused CVEs in tmux, less, vim.
//!     We don't forward them at all (Perform `hook`/`put`/`unhook` are no-ops).
//!   * **OSC 52** — the page should not be able to inject a clipboard write
//!     into the user's shell via tmux.
//!   * **Other OSC** — the browser has no reason to send OSC at all.
//!   * **Oversized chunks** — we hard-cap any single inbound frame at
//!     [`MAX_INBOUND_CHUNK_BYTES`]. vte enforces tighter bounds internally
//!     (`MAX_OSC_RAW = 1024`, `MAX_PARAMS = 32`), so a sequence cap is
//!     redundant; the chunk cap defends against amplification at the
//!     framing layer instead.

use std::fmt::Write as _;

use vte::{Params, Parser, Perform};

/// Per-call byte cap on inbound chunks. Browser keystrokes are tiny; a single
/// frame exceeding 4 KiB is anomalous and likely an attempt to amplify or
/// stress the parser. We drop the chunk wholesale and log a `warn!`.
///
/// Note: vte's internal limits already cap individual escape sequences
/// (`MAX_OSC_RAW` = 1024 bytes, `MAX_PARAMS` = 32 numeric params), so a
/// per-sequence cap on top would be redundant. The chunk cap below is the
/// defense the inbound filter actually provides.
pub const MAX_INBOUND_CHUNK_BYTES: usize = 4096;

/// Run `input` through the inbound filter and return the cleaned bytes.
///
/// This is the most common entry point — Unit 4 calls it once per inbound
/// browser-stdin frame, which is small (a few bytes per keystroke).
///
/// If `input` exceeds [`MAX_INBOUND_CHUNK_BYTES`], the entire chunk is
/// dropped and an empty vec is returned.
pub fn filter(input: &[u8]) -> Vec<u8> {
    if input.len() > MAX_INBOUND_CHUNK_BYTES {
        tracing::warn!(
            chunk_len = input.len(),
            "drop inbound frame: exceeds {MAX_INBOUND_CHUNK_BYTES} bytes"
        );
        return Vec::new();
    }
    let mut perform = InboundFilter::new();
    let mut parser: Parser = Parser::default();
    parser.advance(&mut perform, input);
    perform.take()
}

pub struct InboundFilter {
    out: Vec<u8>,
}

impl Default for InboundFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl InboundFilter {
    pub fn new() -> Self {
        Self {
            out: Vec::with_capacity(256),
        }
    }

    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }
}

impl Perform for InboundFilter {
    fn print(&mut self, c: char) {
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        self.out.extend_from_slice(s.as_bytes());
    }

    fn execute(&mut self, byte: u8) {
        // Browser input frequently includes BS / TAB / LF / CR / ESC and
        // C0 controls like Ctrl-C (ETX = 0x03). Pass all C0 through —
        // tmux's send-keys will handle them.
        self.out.push(byte);
    }

    // ---- DCS dropped entirely ----
    fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: char) {
        tracing::warn!("drop inbound DCS");
    }
    fn put(&mut self, _: u8) {}
    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.is_empty() {
            return;
        }
        if params[0] == b"52" {
            tracing::warn!("drop inbound OSC 52");
            return;
        }
        // Most other OSC from the browser doesn't make sense to forward, but
        // is unlikely to be malicious either. Drop conservatively — agents
        // running in the pane don't expect OSC from stdin.
        tracing::debug!(
            code = String::from_utf8_lossy(params[0]).to_string(),
            "drop inbound OSC (not expected from browser)"
        );
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }

        let mut buf = vec![0x1b, b'['];
        buf.extend_from_slice(intermediates);

        // `vte` always emits at least one default-0 param at dispatch even
        // when the on-wire sequence had none (`\x1b[A`). Detect and preserve
        // the "no params" shape to keep cursor-key encoding byte-faithful.
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
        self.out.extend_from_slice(&buf);
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        // Pass plain ESC sequences (cursor keys arrive as ESC[A etc., which
        // go through csi_dispatch). Charset selection / DECSC / DECRC are
        // benign here too.
        let mut buf = vec![0x1b];
        buf.extend_from_slice(intermediates);
        buf.push(byte);
        self.out.extend_from_slice(&buf);
    }
}

fn is_default_params(params: &Params) -> bool {
    let mut it = params.iter();
    match (it.next(), it.next()) {
        (Some(sub), None) => sub == [0],
        _ => false,
    }
}

struct StringVec<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for StringVec<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}
