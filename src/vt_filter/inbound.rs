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
//!     We don't forward them at all.
//!   * **OSC 52** — the page should not be able to inject a clipboard write
//!     into the user's shell via tmux.
//!   * **Sequences longer than [`MAX_SEQUENCE_BYTES`]** — defends against
//!     OOM / amplification attacks via giant CSI/OSC payloads.

use std::fmt::Write as _;

use vte::{Params, Parser, Perform};

/// Per-sequence byte cap. Anything larger gets dropped with a `tracing::warn!`.
pub const MAX_SEQUENCE_BYTES: usize = 4096;

/// Run `input` through the inbound filter and return the cleaned bytes.
///
/// This is the most common entry point — Unit 4 calls it once per inbound
/// browser-stdin frame, which is small (a few bytes per keystroke).
pub fn filter(input: &[u8]) -> Vec<u8> {
    let mut perform = InboundFilter::new();
    let mut parser: Parser = Parser::default();
    parser.advance(&mut perform, input);
    perform.take()
}

pub struct InboundFilter {
    out: Vec<u8>,
    /// Bytes accumulated for the current sequence (CSI/OSC). Reset on
    /// dispatch.
    seq_len: usize,
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
            seq_len: 0,
        }
    }

    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    fn over_limit(&mut self) -> bool {
        if self.seq_len > MAX_SEQUENCE_BYTES {
            tracing::warn!(
                seq_len = self.seq_len,
                "drop inbound CSI/OSC: exceeds {MAX_SEQUENCE_BYTES} bytes"
            );
            self.seq_len = 0;
            true
        } else {
            self.seq_len = 0;
            false
        }
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
        if self.over_limit() {
            return;
        }
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
        if self.over_limit() || ignore {
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
