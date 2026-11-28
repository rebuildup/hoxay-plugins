//! Safety helpers enforcing `docs/contracts/tui-v1.md` §5 items 8 and 10.
//! Rust port of `packages/tui/src/safety.ts` (sanitizeDisplay,
//! BoundedBuffer, boundText). Keep semantics identical — the TS stack
//! still renders plugin surfaces during coexistence.

use std::collections::VecDeque;

// --- sanitize_display ---------------------------------------------------------

/// Strip everything that could act as a terminal control sequence or inject
/// layout-breaking characters into rendered output.
///
/// Removed:
/// - CSI sequences: ESC [ params final  (and the C1 CSI 0x9b form)
/// - OSC sequences: ESC ] ... terminated by BEL or ST (ESC \ or 0x9c)
/// - DCS/SOS/PM/APC: ESC P/X/^/_ ... ST
/// - Any remaining ESC byte and C0/C1 control characters
/// - Unicode bidi/format controls that can visually spoof text
///   (U+202A–U+202E, U+2066–U+2069, U+200E, U+200F, U+061C)
///
/// Replaced, not deleted, where a visible gap is safer than silent joins:
/// - tab → spaces to the next multiple of 4 columns within the string
/// - newline / carriage return → single space (rendered lines are
///   single-line by contract; multi-line content must be split *before*
///   sanitizing, then each line sanitized separately)
/// - other disallowed controls → U+FFFD (�)
///
/// The function is idempotent and allocation-light enough for per-frame use
/// on untrusted strings (filenames, agent output, remote data, errors).
pub fn sanitize_display(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < n {
        let cp = chars[i] as u32;
        if cp == 0x1b {
            // ESC: consume the whole escape sequence if recognizable.
            i = consume_escape(&chars, i);
            continue; // drop the sequence entirely
        }
        if cp == 0x9b {
            // C1 CSI: params until final byte 0x40–0x7e
            i += 1;
            while i < n {
                let c = chars[i] as u32;
                i += 1;
                if (0x40..=0x7e).contains(&c) {
                    break;
                }
            }
            continue;
        }
        if cp == 0x9d {
            // C1 OSC: until BEL or 0x9c
            i += 1;
            while i < n {
                let c = chars[i] as u32;
                i += 1;
                if c == 0x07 || c == 0x9c {
                    break;
                }
            }
            continue;
        }
        if matches!(cp, 0x90 | 0x98 | 0x9e | 0x9f) {
            // DCS/SOS/PM/APC: until ST (ESC \ or 0x9c)
            i += 1;
            while i < n {
                if chars[i] == '\u{1b}' && i + 1 < n && chars[i + 1] == '\\' {
                    i += 2;
                    break;
                }
                let c = chars[i] as u32;
                i += 1;
                if c == 0x9c {
                    break;
                }
            }
            continue;
        }
        if cp == 0x09 {
            // tab → spaces to next multiple of 4 (approximation of a tab stop)
            let col = display_cells(&out);
            let pad = 4 - (col % 4);
            let pad = if pad == 4 { 4 } else { pad };
            for _ in 0..pad {
                out.push(' ');
            }
            i += 1;
            continue;
        }
        if cp == 0x0a || cp == 0x0d {
            out.push(' ');
            i += 1;
            continue;
        }
        if (cp < 0x20 && cp != 0x09) || (0x7f..0xa0).contains(&cp) || cp == 0x2028 || cp == 0x2029 {
            out.push('\u{FFFD}');
            i += 1;
            continue;
        }
        if (0x202a..=0x202e).contains(&cp) // bidi embeddings/overrides
            || (0x2066..=0x2069).contains(&cp) // bidi isolates
            || cp == 0x200e // LRM
            || cp == 0x200f // RLM
            || cp == 0x061c
        // Arabic letter mark
        {
            i += 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Cheap cell-width tracker used only for tab expansion inside sanitize.
/// Width approximation for tab stops: count chars, treating common wide
/// ranges as 2 — mirrors the TS `displayCells` helper.
fn display_cells(s: &str) -> usize {
    let mut w = 0usize;
    for ch in s.chars() {
        let cp = ch as u32;
        w += if cp >= 0x1100 && (cp <= 0x115f || cp >= 0x2e80) {
            2
        } else {
            1
        };
    }
    w
}

/// Consume one escape sequence starting at `start` (chars[start] == ESC).
/// Returns the index just past the sequence. Malformed/unterminated
/// sequences consume through end of input (fail-safe: drop them).
fn consume_escape(chars: &[char], start: usize) -> usize {
    let n = chars.len();
    let Some(&second) = chars.get(start + 1) else {
        return start + 1;
    };
    let cp2 = second as u32;
    if cp2 == 0x5b {
        // CSI: parameter/intermediate bytes 0x20-0x3f, final 0x40-0x7e
        let mut i = start + 2;
        while i < n {
            let c = chars[i] as u32;
            i += 1;
            if (0x40..=0x7e).contains(&c) {
                break;
            }
            if !(0x20..=0x3f).contains(&c) {
                break; // malformed → stop
            }
        }
        return i;
    }
    if matches!(cp2, 0x5d | 0x50 | 0x58 | 0x5e | 0x5f) {
        // OSC / DCS / SOS / PM / APC: until BEL, ST (ESC \), or CAN/SUB
        let mut i = start + 2;
        while i < n {
            let c = chars[i] as u32;
            if c == 0x07 || c == 0x18 || c == 0x1a {
                return i + 1;
            }
            if c == 0x1b {
                if i + 1 < n && chars[i + 1] == '\\' {
                    return i + 2;
                }
                return i + 1; // another ESC starts a new sequence
            }
            i += 1;
        }
        return n;
    }
    if cp2 == 0x4f
        || cp2 == 0x4e
        || cp2 == 0x7e
        || (0x28..=0x2f).contains(&cp2) // charset selects ESC ( ) * + , - . /
        || (0x3c..=0x3f).contains(&cp2) // ESC < = > ?
        || ((0x40..=0x5f).contains(&cp2) && cp2 != 0x5b)
    {
        // SS3 (ESC O), SS2 (ESC N), charset selects, ESC =/>/ etc: consume
        // the introducer plus one more byte.
        return (start + 3).min(n);
    }
    // ESC + printable char (alt-key style in *input*) or an unrecognized
    // two-byte sequence: for display sanitization the safest reading is to
    // drop only the ESC introducer and keep the following character as text.
    start + 1
}

// --- bounded output ------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BoundedBufferOptions {
    /// Maximum retained lines. Oldest lines are dropped first. Default 500.
    pub max_lines: usize,
    /// Maximum retained characters across all lines. Default 200_000.
    pub max_chars: usize,
}

impl Default for BoundedBufferOptions {
    fn default() -> Self {
        BoundedBufferOptions {
            max_lines: 500,
            max_chars: 200_000,
        }
    }
}

/// A bounded line buffer for log/output surfaces (contract item 10).
/// Push sanitized or pre-sanitized lines; reads always return at most the
/// configured bounds. Deterministic — no timers, no async.
#[derive(Debug)]
pub struct BoundedBuffer {
    lines: VecDeque<String>,
    chars: usize,
    max_lines: usize,
    max_chars: usize,
    /// Number of lines dropped from the front.
    pub dropped: usize,
}

impl BoundedBuffer {
    pub fn new(opts: BoundedBufferOptions) -> Self {
        BoundedBuffer {
            lines: VecDeque::new(),
            chars: 0,
            max_lines: opts.max_lines,
            max_chars: opts.max_chars,
            dropped: 0,
        }
    }

    pub fn push(&mut self, line: &str) {
        let clean = sanitize_display(line);
        self.chars += clean.chars().count() + 1;
        self.lines.push_back(clean);
        self.evict();
    }

    pub fn push_many<I: IntoIterator<Item = String>>(&mut self, lines: I) {
        for l in lines {
            self.push(&l);
        }
    }

    fn evict(&mut self) {
        while self.lines.len() > self.max_lines || self.chars > self.max_chars {
            let Some(removed) = self.lines.pop_front() else {
                break;
            };
            self.chars = self.chars.saturating_sub(removed.chars().count() + 1);
            self.dropped += 1;
        }
    }

    /// All retained lines (oldest → newest).
    pub fn all(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }

    /// Last `n` lines.
    pub fn tail(&self, n: usize) -> Vec<String> {
        let skip = self.lines.len().saturating_sub(n);
        self.lines.iter().skip(skip).cloned().collect()
    }

    pub fn size(&self) -> usize {
        self.lines.len()
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.chars = 0;
    }
}

/// Truncate a single rendered line's *text* content to `max_chars` as a last
/// resort bound. ANSI styling is preserved because callers pass styled
/// lines only through the renderer, which bounds by cell width already.
pub fn bound_text(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_and_osc() {
        assert_eq!(sanitize_display("a\u{1b}[31mred\u{1b}[0m b"), "ared b");
        assert_eq!(
            sanitize_display("x\u{1b}]8;;http://e\u{7}l\u{1b}]8;;\u{7}y"),
            "xly"
        );
        assert_eq!(sanitize_display("c1\u{9b}31mZ"), "c1Z");
    }

    #[test]
    fn replaces_controls_and_newlines() {
        assert_eq!(sanitize_display("a\nb\rc"), "a b c");
        assert_eq!(sanitize_display("a\u{7}b"), "a\u{FFFD}b");
        // "tab" is 3 cells → pad to the next multiple of 4 = 1 space.
        assert_eq!(sanitize_display("tab\there"), "tab here");
        assert_eq!(sanitize_display("a\tb"), "a   b");
    }

    #[test]
    fn strips_bidi_controls() {
        assert_eq!(sanitize_display("a\u{202e}b\u{202c}c"), "abc");
        assert_eq!(sanitize_display("a\u{200e}b"), "ab");
    }

    #[test]
    fn is_idempotent() {
        let once = sanitize_display("a\u{1b}[1mb\u{9d}0;\u{7}c");
        assert_eq!(sanitize_display(&once), once);
    }

    #[test]
    fn bound_text_truncates() {
        assert_eq!(bound_text("hello", 10), "hello");
        assert_eq!(bound_text("hello world", 8), "hello w…");
    }

    #[test]
    fn bounded_buffer_evicts_oldest() {
        let mut buf = BoundedBuffer::new(BoundedBufferOptions {
            max_lines: 3,
            max_chars: 200_000,
        });
        for i in 0..5 {
            buf.push(&format!("line{}", i));
        }
        assert_eq!(buf.size(), 3);
        assert_eq!(buf.dropped, 2);
        assert_eq!(buf.all(), vec!["line2", "line3", "line4"]);
    }
}
