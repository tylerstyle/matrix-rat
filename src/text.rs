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
pub(crate) fn unsafe_char(c: char) -> bool {
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

/// The first line of a message that has something on it and is not the quote of what
/// it answers, made safe for the terminal and cut to `max` columns. Empty if there is
/// no such line.
pub fn first_line(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !is_blank(line) && !line.starts_with('>'))
        .unwrap_or_default();
    sanitize_line(&truncate(line, max)).trim().to_owned()
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

/// Like [`wrap`], but a line ends only at a space: `Alt-Enter` and `--profile` stay in
/// one piece. Only what is wider than a line by itself (a long path) is split.
pub fn wrap_at_spaces(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split(' ').filter(|word| !word.is_empty()) {
            let gap = usize::from(!line.is_empty());
            if width(&line) + gap + width(word) <= max {
                if gap == 1 {
                    line.push(' ');
                }
                line.push_str(word);
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            if width(word) <= max {
                line.push_str(word);
                continue;
            }
            let mut parts = wrap(word, max);
            line = parts.pop().unwrap_or_default();
            lines.extend(parts);
        }
        lines.push(line);
    }
    lines
}

/// `s` in two: what fits into `max` columns, and the rest. Nothing is added or dropped.
pub fn split_at_width(s: &str, max: usize) -> (&str, &str) {
    let mut used = 0;
    for (at, g) in s.grapheme_indices(true) {
        used += width(g);
        if used > max {
            return s.split_at(at);
        }
    }
    (s, "")
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
    fn the_first_line_skips_quotes_and_blank_lines_and_is_made_safe() {
        assert_eq!(
            first_line("> what was said\n\n  the answer\nmore", 40),
            "the answer"
        );
        assert_eq!(first_line("a\u{1b}[31mred", 40), "a[31mred");
        assert_eq!(first_line("a long first line", 6), "a lon…");
        assert_eq!(first_line("> only a quote", 40), "");
        assert_eq!(first_line("", 40), "");
    }

    #[test]
    fn truncates_with_ellipsis() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("hello", 0), "");
        assert!(width(&truncate("日本語のテキスト", 5)) <= 5);
    }

    #[test]
    fn split_at_width_keeps_everything() {
        assert_eq!(split_at_width("hello", 3), ("hel", "lo"));
        assert_eq!(split_at_width("hello", 5), ("hello", ""));
        assert_eq!(split_at_width("hello", 0), ("", "hello"));
        // A wide character that does not fit whole goes to the rest.
        assert_eq!(split_at_width("a🎤b", 2), ("a", "🎤b"));
        assert_eq!(split_at_width("a🎤b", 3), ("a🎤", "b"));
    }

    #[test]
    fn wrap_at_spaces_keeps_hyphenated_words_whole() {
        let text = "Shift-Enter cannot be told from Enter here; Alt-Enter makes a new line";
        for max in 12..60 {
            let lines = wrap_at_spaces(text, max);
            assert_eq!(lines.join(" "), text, "{max}");
            assert!(lines.iter().all(|line| width(line) <= max), "{max}");
        }
        // `wrap` would end a line after "Alt-".
        assert_eq!(
            wrap_at_spaces("here; Alt-Enter makes", 12),
            ["here;", "Alt-Enter", "makes"]
        );
        // What is wider than a line is split, and nothing is lost.
        assert_eq!(
            wrap_at_spaces("in /a/very/long/path here", 8).concat(),
            "in/a/very/long/pathhere"
        );
        assert!(
            wrap_at_spaces("in /a/very/long/path here", 8)
                .iter()
                .all(|line| width(line) <= 8)
        );
        assert_eq!(wrap_at_spaces("", 8), [""]);
        assert_eq!(wrap_at_spaces("a\nb", 8), ["a", "b"]);
    }
}
