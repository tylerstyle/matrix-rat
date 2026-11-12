//! The message input: a soft-wrapping multi-line text buffer.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use crate::text::width;

#[derive(Debug, Default, Clone)]
pub struct Composer {
    text: String,
    /// Byte offset into `text`, always on a grapheme boundary.
    cursor: usize,
}

/// One row as displayed. `range` excludes a trailing newline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualLine {
    pub range: Range<usize>,
    /// True when the row ends because of a newline or the end of the text,
    /// false when it was soft-wrapped.
    pub hard_end: bool,
}

impl Composer {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_owned();
        self.cursor = self.text.len();
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    /// Inserts at the cursor. Tabs become spaces and other control characters are
    /// dropped: they have no defined width and would corrupt the display.
    pub fn insert(&mut self, s: &str) {
        let clean: String = s
            .chars()
            .flat_map(|c| match c {
                '\t' => vec![' '; 4],
                '\n' => vec!['\n'],
                c if c.is_control() => vec![],
                c => vec![c],
            })
            .collect();
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    pub fn backspace(&mut self) {
        let prev = self.prev_boundary();
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
    }

    pub fn delete(&mut self) {
        let next = self.next_boundary();
        self.text.drain(self.cursor..next);
    }

    pub fn left(&mut self) {
        self.cursor = self.prev_boundary();
    }

    pub fn right(&mut self) {
        self.cursor = self.next_boundary();
    }

    pub fn word_left(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end();
        self.cursor = trimmed
            .unicode_word_indices()
            .next_back()
            .map_or(0, |(i, _)| i);
    }

    pub fn word_right(&mut self) {
        let after = &self.text[self.cursor..];
        self.cursor += after
            .unicode_word_indices()
            .next()
            .map_or(after.len(), |(i, w)| i + w.len());
    }

    pub fn delete_word_left(&mut self) {
        let end = self.cursor;
        self.word_left();
        self.text.drain(self.cursor..end);
    }

    fn prev_boundary(&self) -> usize {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self) -> usize {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.cursor, |g| self.cursor + g.len())
    }

    /// Splits the text into display rows of at most `max` columns, preferring to break
    /// after a space.
    pub fn layout(&self, max: usize) -> Vec<VisualLine> {
        let max = max.max(1);
        let mut lines = Vec::new();
        let mut offset = 0;
        for paragraph in self.text.split('\n') {
            let mut start = 0;
            let mut col = 0;
            let mut last_space_end = None;
            for (i, g) in paragraph.grapheme_indices(true) {
                let gw = width(g);
                if col + gw > max && i > start {
                    // Break after the last space on this row if there is one.
                    let brk = last_space_end.filter(|b| *b > start).unwrap_or(i);
                    lines.push(VisualLine {
                        range: offset + start..offset + brk,
                        hard_end: false,
                    });
                    start = brk;
                    col = width(&paragraph[start..i]);
                    last_space_end = None;
                    // The rest of the row plus a wide character may still not fit.
                    if col + gw > max && i > start {
                        lines.push(VisualLine {
                            range: offset + start..offset + i,
                            hard_end: false,
                        });
                        start = i;
                        col = 0;
                    }
                }
                col += gw;
                if g == " " {
                    last_space_end = Some(i + g.len());
                }
            }
            lines.push(VisualLine {
                range: offset + start..offset + paragraph.len(),
                hard_end: true,
            });
            offset += paragraph.len() + 1;
        }
        lines
    }

    /// Row and column of the cursor for a layout of the same width.
    pub fn cursor_position(&self, lines: &[VisualLine]) -> (usize, usize) {
        let row = self.cursor_row(lines);
        let col = width(&self.text[lines[row].range.start..self.cursor]);
        (row, col)
    }

    fn cursor_row(&self, lines: &[VisualLine]) -> usize {
        lines
            .iter()
            .position(|l| self.cursor < l.range.end || (self.cursor == l.range.end && l.hard_end))
            .unwrap_or(lines.len() - 1)
    }

    pub fn on_first_row(&self, max: usize) -> bool {
        self.cursor_row(&self.layout(max)) == 0
    }

    /// Moves the cursor to the given display row and column (as from a mouse click).
    pub fn place(&mut self, max: usize, row: usize, col: usize) {
        let lines = self.layout(max);
        let line = &lines[row.min(lines.len() - 1)];
        let slice = &self.text[line.range.clone()];
        let mut used = 0;
        let mut at = line.range.end;
        for (i, g) in slice.grapheme_indices(true) {
            let gw = width(g);
            if used + gw > col {
                at = line.range.start + i;
                break;
            }
            used += gw;
        }
        // The end of a soft-wrapped row is the start of the next one; stay on this row.
        if !line.hard_end && at == line.range.end && at > line.range.start {
            at = self.text[..at]
                .grapheme_indices(true)
                .next_back()
                .map_or(at, |(i, _)| i);
        }
        self.cursor = at;
    }

    pub fn up(&mut self, max: usize) {
        self.vertical(max, -1);
    }

    pub fn down(&mut self, max: usize) {
        self.vertical(max, 1);
    }

    fn vertical(&mut self, max: usize, delta: isize) {
        let lines = self.layout(max);
        let (row, col) = self.cursor_position(&lines);
        let target = row as isize + delta;
        if target < 0 || target >= lines.len() as isize {
            return;
        }
        self.place(max, target as usize, col);
    }

    pub fn home(&mut self, max: usize) {
        let lines = self.layout(max);
        self.cursor = lines[self.cursor_row(&lines)].range.start;
    }

    pub fn end(&mut self, max: usize) {
        let lines = self.layout(max);
        let row = self.cursor_row(&lines);
        self.place(max, row, usize::MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(text: &str) -> Composer {
        let mut c = Composer::default();
        c.set_text(text);
        c
    }

    fn rows(c: &Composer, max: usize) -> Vec<&str> {
        c.layout(max)
            .into_iter()
            .map(|l| &c.text()[l.range])
            .collect()
    }

    #[test]
    fn editing_basics() {
        let mut c = Composer::default();
        c.insert("héllo");
        c.left();
        c.backspace();
        assert_eq!(c.text(), "hélo");
        c.delete();
        assert_eq!(c.text(), "hél");
        c.insert("p 👍🏽");
        c.backspace();
        assert_eq!(c.text(), "hélp ");
        assert_eq!(c.take(), "hélp ");
        assert!(c.is_empty());
    }

    #[test]
    fn layout_wraps_after_spaces_and_covers_all_text() {
        let c = with("hello brave new world\nsecond");
        assert_eq!(rows(&c, 12), ["hello brave ", "new world", "second"]);
        let lines = c.layout(12);
        assert!(!lines[0].hard_end && lines[1].hard_end && lines[2].hard_end);
        // Rows are contiguous apart from the newline.
        assert_eq!(lines[0].range.end, lines[1].range.start);
        assert_eq!(lines[1].range.end + 1, lines[2].range.start);
    }

    #[test]
    fn layout_hard_breaks_long_words() {
        let c = with("abcdefghij");
        assert_eq!(rows(&c, 4), ["abcd", "efgh", "ij"]);
        assert_eq!(rows(&with(""), 4), [""]);
        assert_eq!(rows(&with("a\n"), 4), ["a", ""]);
    }

    #[test]
    fn cursor_position_follows_wrapping() {
        let mut c = with("hello brave new world");
        let lines = c.layout(12);
        assert_eq!(c.cursor_position(&lines), (1, 9));
        c.home(12);
        assert_eq!(c.cursor_position(&c.layout(12)), (1, 0));
        c.up(12);
        assert_eq!(c.cursor_position(&c.layout(12)), (0, 0));
        assert!(c.on_first_row(12));
        c.end(12);
        // End of a soft-wrapped row stays on that row.
        assert_eq!(c.cursor_position(&c.layout(12)).0, 0);
        c.down(12);
        assert_eq!(c.cursor_position(&c.layout(12)).0, 1);
    }

    #[test]
    fn place_from_click() {
        let mut c = with("hello\nwörld");
        c.place(20, 1, 2);
        c.insert("X");
        assert_eq!(c.text(), "hello\nwöXrld");
        c.place(20, 0, 99);
        c.insert("!");
        assert_eq!(c.text(), "hello!\nwöXrld");
        c.place(20, 99, 0);
        c.insert(">");
        assert_eq!(c.text(), "hello!\n>wöXrld");
    }

    #[test]
    fn rows_never_exceed_the_width() {
        for text in [" bbb🐀", "a 日本語 b🐀🐀 cc", "🐀🐀🐀", " x 🐀"] {
            for max in 2..8 {
                let c = with(text);
                for row in rows(&c, max) {
                    assert!(width(row) <= max, "{text:?} at {max}: {row:?}");
                }
            }
        }
    }

    #[test]
    fn insert_sanitizes_control_characters() {
        let mut c = Composer::default();
        c.insert("a\tb\u{1b}[31m\r\nc\u{7}");
        assert_eq!(c.text(), "a    b[31m\nc");
    }

    #[test]
    fn word_movement() {
        let mut c = with("one two  three");
        c.word_left();
        c.insert("|");
        assert_eq!(c.text(), "one two  |three");
        c.word_left();
        c.word_left();
        c.word_right();
        c.insert("|");
        assert_eq!(c.text(), "one| two  |three");
        let mut c = with("one two");
        c.delete_word_left();
        assert_eq!(c.text(), "one ");
    }
}
