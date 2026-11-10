//! Removing terminal escape sequences from console output, keeping a map back to the raw bytes.
//!
//! Console offsets count raw bytes, but agents match patterns against the text a terminal would show. The stripper
//! works on bytes, so invalid UTF-8 cannot shift positions, and it records where each kept byte came from, so a match
//! in the stripped text maps back to a raw offset.

/// Console output with escape sequences and the CR of each CRLF removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stripped {
    pub text: Vec<u8>,
    /// For each byte of `text`, its index in the raw input.
    pub origin: Vec<usize>,
    /// Raw bytes at the end that may start a sequence not yet complete; they are left out of `text`.
    pub pending: usize,
}

impl Stripped {
    /// The raw index just past the stripped byte range ending at `end` (exclusive).
    pub fn raw_end(&self, end: usize) -> usize {
        match end.checked_sub(1) {
            Some(last) => self.origin[last] + 1,
            None => 0,
        }
    }
}

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
/// The longest OSC body kept waiting for its end; real ones, such as a window title, are far shorter.
const OSC_MAX: usize = 4096;

/// Strips `raw`: CSI sequences (`ESC [` ... a final byte from `@` to `~`), OSC sequences (`ESC ]` ... BEL or
/// `ESC \`, bounded as `osc_end` describes), charset selections (`ESC (` or `ESC )` and one byte), any other two-byte `ESC` sequence, and the CR of
/// each CRLF. A sequence or a CR that the end of `raw` cuts off is held back as `pending`.
pub fn strip(raw: &[u8]) -> Stripped {
    let mut out = Stripped::default();
    let mut i = 0;
    while i < raw.len() {
        let start = i;
        let complete = match raw[i] {
            ESC => match raw.get(i + 1) {
                None => None,
                Some(b'[') => raw[i + 2..]
                    .iter()
                    .position(|b| (b'@'..=b'~').contains(b))
                    .map(|n| i + 2 + n + 1),
                Some(b']') => osc_end(raw, i + 2),
                Some(b'(' | b')') => (i + 3 <= raw.len()).then_some(i + 3),
                Some(_) => Some(i + 2),
            },
            b'\r' => match raw.get(i + 1) {
                None => None,
                Some(b'\n') => Some(i + 1),
                Some(_) => {
                    out.text.push(b'\r');
                    out.origin.push(i);
                    Some(i + 1)
                }
            },
            b => {
                out.text.push(b);
                out.origin.push(i);
                Some(i + 1)
            }
        };
        match complete {
            Some(next) => i = next,
            None => {
                out.pending = raw.len() - start;
                break;
            }
        }
    }
    out
}

/// Strips output that ends here, for display: an escape sequence cut off at the end is dropped, and a final CR is kept.
pub fn strip_final(raw: &[u8]) -> Vec<u8> {
    let s = strip(raw);
    let mut text = s.text;
    if s.pending > 0 && raw[raw.len() - s.pending] == b'\r' {
        text.push(b'\r');
    }
    text
}

/// The index just past an OSC sequence whose body starts at `from`, or `None` when it is not complete yet.
///
/// BEL or `ESC \` ends it. Line noise can start one that never ends, so CAN or SUB aborts it with the byte, and a
/// newline, another escape sequence, or `OSC_MAX` bytes of body abort it before that point, so the text after it
/// shows.
fn osc_end(raw: &[u8], from: usize) -> Option<usize> {
    let mut j = from;
    while j < raw.len() {
        if j - from >= OSC_MAX {
            return Some(j);
        }
        match raw[j] {
            BEL | CAN | SUB => return Some(j + 1),
            b'\n' => return Some(j),
            ESC => match raw.get(j + 1) {
                None => return None,
                Some(b'\\') => return Some(j + 2),
                Some(_) => return Some(j),
            },
            _ => j += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(raw: &[u8]) -> String {
        String::from_utf8(strip(raw).text).unwrap()
    }

    #[test]
    fn strips_sequences_and_crlf() {
        assert_eq!(text(b"\x1b[1;32mok\x1b[0m\r\n"), "ok\n");
        assert_eq!(text(b"\x1b]0;title\x07$ "), "$ ");
        assert_eq!(text(b"\x1b(Bplain"), "plain");
        assert_eq!(text(b"a\rb"), "a\rb", "a lone CR stays");
    }

    #[test]
    fn maps_stripped_positions_to_raw_offsets() {
        let raw = b"\x1b[1mlogin:\x1b[0m ";
        let s = strip(raw);
        assert_eq!(s.text, b"login: ");
        let at = s.text.windows(6).position(|w| w == b"login:").unwrap();
        assert_eq!(s.origin[at], 4);
        assert_eq!(
            s.raw_end(at + 6),
            10,
            "just past the colon, before the reset sequence"
        );
        assert_eq!(s.raw_end(0), 0);
    }

    #[test]
    fn holds_back_a_sequence_the_end_cuts_off() {
        let s = strip(b"ok\x1b[1");
        assert_eq!((s.text.as_slice(), s.pending), (&b"ok"[..], 3));
        let s = strip(b"ok\x1b]0;tit");
        assert_eq!((s.text.as_slice(), s.pending), (&b"ok"[..], 7));
        let s = strip(b"ok\r");
        assert_eq!((s.text.as_slice(), s.pending), (&b"ok"[..], 1));
        let s = strip(b"ok\x1b");
        assert_eq!(s.pending, 1);
    }

    #[test]
    fn an_unterminated_osc_ends_at_a_newline_or_a_control_byte() {
        assert_eq!(text(b"\x1b]noise\r\nlogin: "), "\nlogin: ");
        assert_eq!(text(b"\x1b]noise\x18login: "), "login: ", "CAN aborts it");
        assert_eq!(text(b"\x1b]noise\x1alogin: "), "login: ", "SUB aborts it");
        assert_eq!(
            text(b"\x1b]noise\x1b[1mlogin: "),
            "login: ",
            "a new sequence ends it"
        );
        let s = strip(b"ok\x1b]0;tit\x1b");
        assert_eq!(
            (s.text.as_slice(), s.pending),
            (&b"ok"[..], 8),
            "an ESC at the end may still start ST"
        );
    }

    #[test]
    fn an_osc_longer_than_the_cap_is_dropped() {
        let mut raw = b"\x1b]".to_vec();
        raw.extend(std::iter::repeat_n(b'x', OSC_MAX));
        raw.extend_from_slice(b"login: ");
        let s = strip(&raw);
        assert_eq!(s.text, b"login: ");
        assert_eq!(s.origin[0], 2 + OSC_MAX);
        assert_eq!(s.pending, 0);
    }

    #[test]
    fn invalid_utf8_keeps_its_bytes_and_positions() {
        let s = strip(b"\xff\xfelogin:");
        assert_eq!(s.text, b"\xff\xfelogin:");
        assert_eq!(s.origin[2], 2);
    }
}
