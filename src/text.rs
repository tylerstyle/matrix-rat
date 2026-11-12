//! Width-aware text helpers.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
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
    fn truncates_with_ellipsis() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("hello", 0), "");
        assert!(width(&truncate("日本語のテキスト", 5)) <= 5);
    }
}
