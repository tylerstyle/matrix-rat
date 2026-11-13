//! Width-aware text helpers.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Makes text from outside safe to put on the screen: control characters (which could
/// carry terminal escape sequences) are removed, tabs become spaces. Newlines stay.
pub fn sanitize(text: &str) -> String {
    if !text.contains(|c: char| unsafe_char(c) && c != '\n') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if unsafe_char(c) => {}
            c => out.push(c),
        }
    }
    out
}

/// Like [`sanitize`], for text that must stay on one line (names, reaction keys).
pub fn sanitize_line(text: &str) -> String {
    sanitize(text).replace('\n', " ")
}

/// Whether text shows nothing at all: empty, or only spaces and characters that take
/// no room or draw nothing (zero-width spaces and joiners, fillers, the blank Braille
/// cell). A name like that needs a stand-in.
pub fn is_blank(text: &str) -> bool {
    use unicode_width::UnicodeWidthChar;
    text.chars().all(|c| {
        c.is_whitespace()
            || c.width().unwrap_or(0) == 0
            || matches!(
                c,
                '\u{115f}' | '\u{1160}' | '\u{3164}' | '\u{ffa0}' | '\u{2800}' | '\u{00ad}'
            )
    })
}

/// Control characters, and the invisible direction overrides that can reorder the rest
/// of a terminal row.
fn unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
                | '\u{2028}' | '\u{2029}'
        )
}

/// Word-wraps `text` to `max` columns. Explicit newlines are kept; words longer than a
/// line are split. Always returns at least one line.
pub fn wrap(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut line_width = 0;
        for word in paragraph.split_word_bounds() {
            let word_width = width(word);
            if line_width + word_width <= max {
                line.push_str(word);
                line_width += word_width;
                continue;
            }
            if word.trim().is_empty() {
                // Whitespace at a wrap point is dropped.
                lines.push(std::mem::take(&mut line));
                line_width = 0;
                continue;
            }
            if word_width <= max {
                lines.push(std::mem::take(&mut line).trim_end().to_owned());
                line.push_str(word);
                line_width = word_width;
                continue;
            }
            // A single word wider than the line: split by grapheme.
            for g in word.graphemes(true) {
                let gw = width(g);
                if line_width + gw > max && !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                    line_width = 0;
                }
                line.push_str(g);
                line_width += gw;
            }
        }
        lines.push(line.trim_end().to_owned());
    }
    lines
}

/// Cuts `s` to at most `max` columns, ending with `…` when something was removed.
pub fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for g in s.graphemes(true) {
        let gw = width(g);
        if used + gw > max - 1 {
            break;
        }
        out.push_str(g);
        used += gw;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_that_shows_nothing_is_blank() {
        for blank in [
            "",
            "  \t",
            "\u{200b}",
            "\u{200c}\u{200d}\u{feff}",
            " \u{2060} ",
            "\u{3164}",
            "\u{2800}\u{00ad}",
            "\u{fe0f}",
        ] {
            assert!(is_blank(blank), "{blank:?}");
        }
        for shown in ["a", " . ", "日", "🐀", "a\u{200b}", "e\u{301}"] {
            assert!(!is_blank(shown), "{shown:?}");
        }
    }

    #[test]
    fn wraps_on_words() {
        assert_eq!(
            wrap("hello brave new world", 11),
            ["hello brave", "new world"]
        );
        assert_eq!(wrap("", 10), [""]);
        assert_eq!(wrap("a\n\nb", 10), ["a", "", "b"]);
    }

    #[test]
    fn splits_long_words_and_respects_wide_chars() {
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        for line in wrap("日本語のテキストを折り返す", 6) {
            assert!(width(&line) <= 6, "{line}");
        }
    }

    #[test]
    fn no_line_exceeds_width() {
        let text = "The quick brown fox — jumps over the lazy dog 🦊 again and again";
        for max in 1..30 {
            for line in wrap(text, max) {
                assert!(width(&line) <= max.max(2), "{max}: {line:?}");
            }
        }
    }

    #[test]
    fn sanitize_removes_escape_sequences() {
        assert_eq!(sanitize("plain\ntext"), "plain\ntext");
        assert_eq!(
            sanitize("a\u{1b}[31mred\u{7}\tb\r\u{9b}x\u{0}"),
            "a[31mred    bx"
        );
        assert_eq!(sanitize("abc\u{202e}def\u{2066}"), "abcdef");
        assert_eq!(sanitize_line("a\nb\tc"), "a b    c");
    }

    #[test]
    fn truncates_with_ellipsis() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("hello", 0), "");
        assert!(width(&truncate("日本語のテキスト", 5)) <= 5);
    }
}
