//! ANSI escape sequences in container output.
//!
//! Logs keep their colors (SGR, `ESC [ … m`) on the wire so the dashboard
//! can paint them; everything that reads a line's words (its level, its
//! JSON, MCP and plain CLI output) uses [`strip`] first. Other sequences
//! (cursor moves, clearing, titles, hyperlinks) never mean anything in a
//! log, and raw control characters never reach a reader: [`sgr_only`]
//! removes both and keeps the colors, for a terminal.

use std::borrow::Cow;

/// What a sequence at some position is.
enum Seq {
    /// `ESC [ <digits ; :> m`: a color or text style.
    Sgr,
    /// Any other escape sequence, or a control character: dropped.
    Other,
}

/// The sequence or control character that starts at byte `i` (which is
/// one), and the byte it ends before. A sequence cut off by the end of the
/// text runs to the end; a malformed one ends where it went wrong.
fn scan(b: &[u8], i: usize) -> (Seq, usize) {
    // C1 controls arrive as UTF-8: U+0080..U+009F is C2 80..C2 9F.
    let (intro, mut j) = match b[i] {
        0x1b => match b.get(i + 1) {
            Some(&c @ 0x40..=0x5f) => (c, i + 2),
            // nF (ESC SP..'/' … final) and Fp/Fs (ESC '0'..'~').
            Some(0x20..=0x2f) => {
                let mut j = i + 1;
                while matches!(b.get(j), Some(0x20..=0x2f)) {
                    j += 1;
                }
                return match b.get(j) {
                    Some(0x30..=0x7e) => (Seq::Other, j + 1),
                    Some(_) => (Seq::Other, j),
                    None => (Seq::Other, b.len()),
                };
            }
            Some(0x60..=0x7e | 0x30..=0x3f) => return (Seq::Other, i + 2),
            // A lone ESC.
            _ => return (Seq::Other, i + 1),
        },
        0xc2 if matches!(b.get(i + 1), Some(0x80..=0x9f)) => (b[i + 1] - 0x40, i + 2),
        _ => return (Seq::Other, i + 1),
    };
    match intro {
        // CSI: parameters, intermediates, one final byte.
        b'[' => {
            let start = j;
            while matches!(b.get(j), Some(0x30..=0x3f)) {
                j += 1;
            }
            let params = &b[start..j];
            let inter = j;
            while matches!(b.get(j), Some(0x20..=0x2f)) {
                j += 1;
            }
            match b.get(j) {
                Some(b'm')
                    if inter == j
                        && params
                            .iter()
                            .all(|c| c.is_ascii_digit() || *c == b';' || *c == b':') =>
                {
                    (Seq::Sgr, j + 1)
                }
                Some(0x40..=0x7e) => (Seq::Other, j + 1),
                // Broken off: drop what's read, keep what follows.
                Some(_) => (Seq::Other, j),
                None => (Seq::Other, b.len()),
            }
        }
        // OSC, DCS, SOS, PM, APC: a string up to ST (ESC \ or U+009C) or BEL.
        b']' | b'P' | b'X' | b'^' | b'_' => {
            while j < b.len() {
                match b[j] {
                    0x07 => return (Seq::Other, j + 1),
                    0x1b if b.get(j + 1) == Some(&b'\\') => return (Seq::Other, j + 2),
                    0xc2 if b.get(j + 1) == Some(&0x9c) => return (Seq::Other, j + 2),
                    _ => j += 1,
                }
            }
            (Seq::Other, b.len())
        }
        // Any other two-byte escape (ESC D, ESC M, …).
        _ => (Seq::Other, j),
    }
}

/// Whether byte `i` starts an escape sequence or a control character
/// (anything below a space but tab and newline, DEL, and C1).
fn special(b: &[u8], i: usize) -> bool {
    match b[i] {
        b'\t' | b'\n' => false,
        0x00..=0x1f | 0x7f => true,
        0xc2 => matches!(b.get(i + 1), Some(0x80..=0x9f)),
        _ => false,
    }
}

fn filter(s: &str, keep_sgr: bool) -> Cow<'_, str> {
    let b = s.as_bytes();
    if !(0..b.len()).any(|i| special(b, i)) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let (mut i, mut from) = (0, 0);
    while i < b.len() {
        if !special(b, i) {
            i += 1;
            continue;
        }
        out.push_str(&s[from..i]);
        let (kind, end) = scan(b, i);
        if keep_sgr && matches!(kind, Seq::Sgr) {
            // C1's one-character CSI written as ESC [, which every terminal reads.
            out.push_str("\x1b[");
            // (ESC [ and U+009B are both two bytes.)
            out.push_str(&s[i + 2..end]);
        }
        i = end;
        from = end;
    }
    out.push_str(&s[from..]);
    Cow::Owned(out)
}

/// `s` as plain text: no escape sequences and no control characters but
/// tab and newline. An OSC 8 hyperlink keeps its text.
pub fn strip(s: &str) -> Cow<'_, str> {
    filter(s, false)
}

/// `s` with only its colors and styles (SGR) kept: for a terminal.
pub fn sgr_only(s: &str) -> Cow<'_, str> {
    filter(s, true)
}

/// A log line's level guessed from its words, colors aside: `error` for
/// error, panic or fatal, `warn` for warn, else `info`.
pub fn level_of(line: &str) -> &'static str {
    let lower = strip(line).to_ascii_lowercase();
    if lower.contains("error") || lower.contains("panic") || lower.contains("fatal") {
        "error"
    } else if lower.contains("warn") {
        "warn"
    } else {
        "info"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GHOST: &str = "[2026-11-10 06:47:15] \x1b[36mINFO\x1b[39m \"GET /ghost/api/admin/labels/?limit=100&page=1\" \x1b[32m200\x1b[39m 36ms";

    #[test]
    fn ghost_line() {
        assert_eq!(
            strip(GHOST),
            "[2026-11-10 06:47:15] INFO \"GET /ghost/api/admin/labels/?limit=100&page=1\" 200 36ms"
        );
        assert_eq!(sgr_only(GHOST), GHOST);
        assert_eq!(level_of(GHOST), "info");
    }

    #[test]
    fn levels_ignore_colors() {
        assert_eq!(level_of("\x1b[31mERROR\x1b[0m boom"), "error");
        assert_eq!(level_of("\x1b[1;33mWARN\x1b[22;39m slow"), "warn");
        assert_eq!(level_of("\x1b[38;2;255;0;0mready\x1b[0m"), "info");
    }

    #[test]
    fn plain_text_untouched() {
        let s = "plain line, tab\tand ünïcødé ✓";
        assert!(matches!(strip(s), Cow::Borrowed(_)));
        assert!(matches!(sgr_only(s), Cow::Borrowed(_)));
        assert_eq!(strip(s), s);
    }

    #[test]
    fn non_sgr_dropped() {
        // Cursor moves, clear line, a private mode, a charset, a title.
        assert_eq!(strip("\x1b[2K\x1b[1Gdone \x1b[?25hok\x1b(B"), "done ok");
        assert_eq!(
            sgr_only("\x1b[2K\x1b[32mdone\x1b[0m\x1b[3A"),
            "\x1b[32mdone\x1b[0m"
        );
        assert_eq!(strip("\x1b]0;my title\x07after"), "after");
        // An OSC 8 hyperlink keeps its text.
        assert_eq!(
            strip("see \x1b]8;;https://pyo.dev\x1b\\the docs\x1b]8;;\x1b\\."),
            "see the docs."
        );
        // C1 CSI (U+009B) and stray controls.
        assert_eq!(sgr_only("\u{9b}31mred\u{9b}2J\r\x08\x00"), "\x1b[31mred");
        assert_eq!(strip("\u{9b}31mred\u{85}"), "red");
        assert_eq!(strip("a\x1b7b\x1b8c\x1bMd"), "abcd");
        // 256 colors and truecolor are colors; a private mode isn't.
        assert_eq!(
            sgr_only("\x1b[38;5;196ma\x1b[38:2::1:2:3mb\x1b[?1mc"),
            "\x1b[38;5;196ma\x1b[38:2::1:2:3mbc"
        );
    }

    #[test]
    fn truncated_or_malformed() {
        assert_eq!(strip("ok \x1b[38;5;2"), "ok ");
        assert_eq!(strip("ok \x1b"), "ok ");
        assert_eq!(strip("ok \x1b]8;;http://x"), "ok ");
        // A sequence broken by another character ends there.
        assert_eq!(strip("a\x1b[31\x07b"), "ab");
        assert_eq!(strip("a\x1b[3é"), "aé");
    }
}
