//! Decoder for `%output` payloads.
//!
//! tmux control mode emits pane output on a single line per chunk:
//!
//! ```text
//! %output %<pane_id> <encoded>\n
//! ```
//!
//! Bytes that don't render safely on the wire (newlines, ESC, backslash, and
//! anything below 0x20) are encoded as `\NNN` where NNN is a *three-digit
//! octal* representation of the byte. The literal `\` is encoded as `\134`.
//! Everything else (printable ASCII and raw UTF-8 multibyte) passes through.

/// Decode an `%output` payload from its on-wire form to raw bytes.
///
/// Invariants:
/// * `\` is always followed by exactly three octal digits.
/// * UTF-8 multibyte sequences pass through verbatim.
/// * Any sequence the parser doesn't recognise is emitted as-is (defensive;
///   surfaces in tests as a clear hex diff rather than silent corruption).
pub fn decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == b'\\' && i + 3 < input.len() {
            let d1 = input[i + 1];
            let d2 = input[i + 2];
            let d3 = input[i + 3];
            if is_octal(d1) && is_octal(d2) && is_octal(d3) {
                let n = ((d1 - b'0') as u16) * 64
                    + ((d2 - b'0') as u16) * 8
                    + ((d3 - b'0') as u16);
                out.push(n as u8);
                i += 4;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    out
}

#[inline]
fn is_octal(b: u8) -> bool {
    (b'0'..=b'7').contains(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_printable_through() {
        assert_eq!(decode(b"hello world"), b"hello world");
    }

    #[test]
    fn decodes_newline_escape() {
        // \012 = octal 12 = decimal 10 = '\n'
        assert_eq!(decode(b"hello\\012world"), b"hello\nworld");
    }

    #[test]
    fn decodes_backslash() {
        // \134 = octal 134 = decimal 92 = '\\'
        assert_eq!(decode(b"\\134\\012"), b"\\\n");
    }

    #[test]
    fn decodes_esc() {
        // \033 = octal 33 = decimal 27 = ESC
        assert_eq!(decode(b"\\033[31m"), b"\x1b[31m");
    }

    #[test]
    fn decodes_carriage_return() {
        // \015 = '\r'
        assert_eq!(decode(b"abc\\015def"), b"abc\rdef");
    }

    #[test]
    fn passes_utf8_through() {
        // "héllo" — the é is two bytes (0xc3 0xa9). Should not be touched.
        assert_eq!(decode("héllo".as_bytes()), "héllo".as_bytes());
    }

    #[test]
    fn handles_invalid_escape_gracefully() {
        // `\\abc` is not a valid octal escape — emit as-is.
        assert_eq!(decode(b"\\abc"), b"\\abc");
    }

    #[test]
    fn handles_trailing_backslash() {
        assert_eq!(decode(b"hello\\"), b"hello\\");
    }

    #[test]
    fn decodes_full_range_octal() {
        // \000 → NUL
        assert_eq!(decode(b"\\000"), &[0u8][..]);
        // \377 = octal 377 = 255 = 0xff
        assert_eq!(decode(b"\\377"), &[0xffu8][..]);
    }

    #[test]
    fn handles_consecutive_escapes() {
        assert_eq!(decode(b"\\015\\012"), b"\r\n");
    }

    #[test]
    fn handles_mixed_content() {
        // ANSI sequence with literal text interspersed.
        assert_eq!(
            decode(b"\\033[1;31mred\\033[0m"),
            b"\x1b[1;31mred\x1b[0m"
        );
    }
}
