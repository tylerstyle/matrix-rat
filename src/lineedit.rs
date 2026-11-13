//! Single-line grapheme-aware text editing with horizontal scrolling and click placement.

use std::ops::Deref;

use crossterm::event::{KeyCode, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

use crate::text::{sanitize_line, width};

/// Result of applying a key, paste, or pointer action to a [`LineEdit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOutcome {
    /// Key was not handled or had no effect (e.g. Left at column 0).
    Ignored,
    /// Only the cursor moved; the underlying text did not change.
    Moved,
    /// The text changed.
    Changed,
}

impl EditOutcome {
    pub fn changed(self) -> bool {
        matches!(self, Self::Changed)
    }
}

/// Which editing shortcuts are active in a particular popup or widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditKeys {
    /// Plain `Left` / `Right` move the cursor (disabled where they navigate a grid or list).
    pub arrows: bool,
    /// Plain `Home` / `End` move to the ends of the input (disabled where they scroll a list).
    pub home_end: bool,
    /// Plain `Delete` deletes forward (disabled where it deletes a list selection).
    pub delete: bool,
    /// `Ctrl-K` kills from cursor to end of line.
    pub kill_to_end: bool,
}

impl EditKeys {
    pub const FULL: Self = Self {
        arrows: true,
        home_end: true,
        delete: true,
        kill_to_end: true,
    };
}

/// Visible slice of a [`LineEdit`] fitted inside a viewport width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineView {
    pub text: String,
    /// Terminal column of the cursor relative to the start of `text`, in `0..=max_cols`.
    pub cursor_col: usize,
}

/// State of a single-line text input: UTF-8 text, grapheme-boundary cursor, optional
/// byte cap, optional secret redaction in `Debug`, and horizontal scroll memory.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct LineEdit {
    text: String,
    /// Byte offset of the cursor; always on a grapheme cluster boundary.
    cursor: usize,
    max_bytes: Option<usize>,
    secret: bool,
    /// First display column shown in the last rendered viewport (in cells).
    scroll_col: usize,
}

impl std::fmt::Debug for LineEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.secret {
            f.write_str("LineEdit(\"<redacted>\")")
        } else {
            f.debug_tuple("LineEdit").field(&self.text).finish()
        }
    }
}

impl Deref for LineEdit {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

impl From<&str> for LineEdit {
    fn from(value: &str) -> Self {
        Self::with_text(value)
    }
}

impl From<String> for LineEdit {
    fn from(value: String) -> Self {
        Self::with_text(value)
    }
}

impl PartialEq<str> for LineEdit {
    fn eq(&self, other: &str) -> bool {
        self.text == other
    }
}

impl PartialEq<&str> for LineEdit {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

impl PartialEq<String> for LineEdit {
    fn eq(&self, other: &String) -> bool {
        self.text == *other
    }
}

impl LineEdit {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn secret() -> Self {
        Self {
            secret: true,
            ..Self::default()
        }
    }

    pub fn with_max_bytes(mut self, max: usize) -> Self {
        self.max_bytes = Some(max);
        self.clip_to_limit();
        self
    }

    pub fn with_text(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.len();
        Self {
            text,
            cursor,
            max_bytes: None,
            secret: false,
            scroll_col: 0,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Replaces the text (preserving limit and secret settings) and places the cursor at the end.
    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.clip_to_limit();
        self.cursor = self.text.len();
        self.scroll_col = 0;
    }

    /// Takes the edited string out, clearing the buffer while preserving limit and secret flags.
    pub fn take_string(&mut self) -> String {
        self.cursor = 0;
        self.scroll_col = 0;
        std::mem::take(&mut self.text)
    }

    #[cfg(test)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn clip_to_limit(&mut self) {
        let Some(limit) = self.max_bytes else {
            return;
        };
        if self.text.len() <= limit {
            return;
        }
        let mut end = 0;
        for (i, g) in self.text.grapheme_indices(true) {
            if i + g.len() > limit {
                break;
            }
            end = i + g.len();
        }
        self.text.truncate(end);
        self.cursor = self.snap_cursor(self.cursor.min(end));
    }

    fn snap_cursor(&self, raw: usize) -> usize {
        let raw = raw.min(self.text.len());
        self.text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .find(|&i| i >= raw)
            .unwrap_or(self.text.len())
    }

    fn prev_boundary(&self, at: usize) -> usize {
        self.text[..at]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self, at: usize) -> usize {
        self.text[at..]
            .graphemes(true)
            .next()
            .map_or(at, |g| at + g.len())
    }

    fn word_left_boundary(&self, at: usize) -> usize {
        self.text[..at]
            .trim_end()
            .unicode_word_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn word_right_boundary(&self, at: usize) -> usize {
        self.text[at..]
            .unicode_word_indices()
            .next()
            .map_or(self.text.len(), |(i, w)| at + i + w.len())
    }

    /// Inserts sanitized one-line text at the cursor, keeping grapheme clusters whole
    /// and stopping before any configured byte cap.
    pub fn insert(&mut self, raw: &str) -> EditOutcome {
        let clean = sanitize_line(raw);
        let slice = match self.max_bytes {
            None => clean.as_str(),
            Some(limit) => {
                let room = limit.saturating_sub(self.text.len());
                let mut take = 0;
                for g in clean.graphemes(true) {
                    if take + g.len() > room {
                        break;
                    }
                    take += g.len();
                }
                &clean[..take]
            }
        };
        if slice.is_empty() {
            return EditOutcome::Ignored;
        }
        let next = self.cursor + slice.len();
        self.text.insert_str(self.cursor, slice);
        self.cursor = self.snap_cursor(next);
        EditOutcome::Changed
    }

    /// Pastes the first line of `raw`, sanitized to one line.
    pub fn paste(&mut self, raw: &str) -> EditOutcome {
        let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
        let first = normalized.lines().next().unwrap_or("");
        self.insert(first)
    }

    pub fn clear(&mut self) -> EditOutcome {
        if self.text.is_empty() {
            return EditOutcome::Ignored;
        }
        self.text.clear();
        self.cursor = 0;
        self.scroll_col = 0;
        EditOutcome::Changed
    }

    pub fn backspace(&mut self) -> EditOutcome {
        if self.cursor == 0 {
            return EditOutcome::Ignored;
        }
        let prev = self.prev_boundary(self.cursor);
        self.text.drain(prev..self.cursor);
        self.cursor = self.snap_cursor(prev);
        EditOutcome::Changed
    }

    pub fn delete(&mut self) -> EditOutcome {
        if self.cursor >= self.text.len() {
            return EditOutcome::Ignored;
        }
        let next = self.next_boundary(self.cursor);
        self.text.drain(self.cursor..next);
        self.cursor = self.snap_cursor(self.cursor);
        EditOutcome::Changed
    }

    pub fn delete_word_left(&mut self) -> EditOutcome {
        if self.cursor == 0 {
            return EditOutcome::Ignored;
        }
        let to = self.word_left_boundary(self.cursor);
        if to == self.cursor {
            return EditOutcome::Ignored;
        }
        self.text.drain(to..self.cursor);
        self.cursor = self.snap_cursor(to);
        EditOutcome::Changed
    }

    pub fn kill_to_end(&mut self) -> EditOutcome {
        if self.cursor >= self.text.len() {
            return EditOutcome::Ignored;
        }
        self.text.truncate(self.cursor);
        EditOutcome::Changed
    }

    pub fn left(&mut self) -> EditOutcome {
        if self.cursor == 0 {
            return EditOutcome::Ignored;
        }
        self.cursor = self.prev_boundary(self.cursor);
        EditOutcome::Moved
    }

    pub fn right(&mut self) -> EditOutcome {
        if self.cursor >= self.text.len() {
            return EditOutcome::Ignored;
        }
        self.cursor = self.next_boundary(self.cursor);
        EditOutcome::Moved
    }

    pub fn word_left(&mut self) -> EditOutcome {
        let next = self.word_left_boundary(self.cursor);
        if next == self.cursor {
            return EditOutcome::Ignored;
        }
        self.cursor = next;
        EditOutcome::Moved
    }

    pub fn word_right(&mut self) -> EditOutcome {
        let next = self.word_right_boundary(self.cursor);
        if next == self.cursor {
            return EditOutcome::Ignored;
        }
        self.cursor = next;
        EditOutcome::Moved
    }

    pub fn home(&mut self) -> EditOutcome {
        if self.cursor == 0 {
            return EditOutcome::Ignored;
        }
        self.cursor = 0;
        EditOutcome::Moved
    }

    pub fn end(&mut self) -> EditOutcome {
        if self.cursor >= self.text.len() {
            return EditOutcome::Ignored;
        }
        self.cursor = self.text.len();
        EditOutcome::Moved
    }

    /// Places the cursor at `rel_col` columns from the start of the last rendered
    /// viewport, snapping to a grapheme cluster boundary.
    pub fn place(&mut self, rel_col: usize, masked: bool) -> EditOutcome {
        let target = self.scroll_col + rel_col;
        let mut used = 0;
        let mut at = self.text.len();
        for (i, g) in self.text.grapheme_indices(true) {
            let gw = if masked { 1 } else { width(g) };
            if used + gw > target {
                at = i;
                break;
            }
            used += gw;
        }
        if at == self.cursor {
            EditOutcome::Ignored
        } else {
            self.cursor = at;
            EditOutcome::Moved
        }
    }

    /// Handles standard line-editing keys filtered by `keys`.
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers, keys: EditKeys) -> EditOutcome {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let alt = mods.contains(KeyModifiers::ALT);
        match code {
            KeyCode::Char('b') if alt && !ctrl => self.word_left(),
            KeyCode::Char('f') if alt && !ctrl => self.word_right(),
            KeyCode::Char('w') if ctrl && !alt => self.delete_word_left(),
            KeyCode::Char('a') if ctrl && !alt => self.home(),
            KeyCode::Char('e') if ctrl && !alt => self.end(),
            KeyCode::Char('u') if ctrl && !alt => self.clear(),
            KeyCode::Char('k') if ctrl && !alt && keys.kill_to_end => self.kill_to_end(),
            KeyCode::Char(c) if !ctrl && !alt => self.insert(c.encode_utf8(&mut [0; 4])),
            KeyCode::Backspace if ctrl || alt => self.delete_word_left(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete if !ctrl && !alt && keys.delete => self.delete(),
            KeyCode::Left if ctrl && !alt => self.word_left(),
            KeyCode::Right if ctrl && !alt => self.word_right(),
            KeyCode::Left if !ctrl && !alt && keys.arrows => self.left(),
            KeyCode::Right if !ctrl && !alt && keys.arrows => self.right(),
            KeyCode::Home if !ctrl && !alt && keys.home_end => self.home(),
            KeyCode::End if !ctrl && !alt && keys.home_end => self.end(),
            _ => EditOutcome::Ignored,
        }
    }

    /// Computes the horizontal viewport of at most `max_cols` display columns that keeps
    /// the cursor visible, updating internal scroll state for subsequent draws and clicks.
    ///
    /// When `masked` is `true`, each grapheme cluster is rendered as one `•` column.
    pub fn view(&mut self, max_cols: usize, masked: bool) -> LineView {
        struct GraphemeSpan {
            byte_start: usize,
            col_start: usize,
            cols: usize,
        }

        let mut spans: Vec<GraphemeSpan> = Vec::new();
        let mut col = 0;
        let mut cursor_col = None;
        let mut next_gw = 0;
        for (byte_start, g) in self.text.grapheme_indices(true) {
            let gw = if masked { 1 } else { width(g) };
            if byte_start == self.cursor {
                cursor_col = Some(col);
                next_gw = gw;
            }
            spans.push(GraphemeSpan {
                byte_start,
                col_start: col,
                cols: gw,
            });
            col += gw;
        }
        let total_cols = col;
        let cursor_col = cursor_col.unwrap_or(total_cols);

        if max_cols == 0 {
            self.scroll_col = cursor_col;
            return LineView {
                text: String::new(),
                cursor_col: 0,
            };
        }

        // Pull scroll left if deletions shrank the tail so available space isn't wasted.
        let max_scroll = spans
            .iter()
            .find(|g| total_cols - g.col_start <= max_cols)
            .map_or(total_cols, |g| g.col_start);
        let mut scroll = self.scroll_col.min(max_scroll);
        scroll = spans
            .iter()
            .rfind(|g| g.col_start <= scroll)
            .map_or(0, |g| g.col_start);

        if cursor_col < scroll {
            scroll = cursor_col;
        }
        let target_right = if self.cursor < self.text.len() && next_gw > 0 && next_gw <= max_cols {
            cursor_col + next_gw
        } else {
            cursor_col
        };
        if target_right > scroll + max_cols {
            let need = target_right - max_cols;
            scroll = spans
                .iter()
                .find(|g| g.col_start >= need)
                .map_or(cursor_col, |g| g.col_start);
            scroll = scroll.min(cursor_col);
        }
        self.scroll_col = scroll;

        let start_idx = spans.partition_point(|g| g.col_start < scroll);
        let mut end_idx = start_idx;
        let mut used = 0;
        while end_idx < spans.len() {
            let gw = spans[end_idx].cols;
            if used + gw > max_cols {
                break;
            }
            used += gw;
            end_idx += 1;
        }

        let visible = if masked {
            "•".repeat(end_idx - start_idx)
        } else if start_idx == end_idx {
            String::new()
        } else {
            let byte_start = spans[start_idx].byte_start;
            let byte_end = spans.get(end_idx).map_or(self.text.len(), |g| g.byte_start);
            self.text[byte_start..byte_end].to_owned()
        };

        LineView {
            text: visible,
            cursor_col: cursor_col.saturating_sub(scroll).min(max_cols),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> (KeyCode, KeyModifiers) {
        (code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> (KeyCode, KeyModifiers) {
        (KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn alt(c: char) -> (KeyCode, KeyModifiers) {
        (KeyCode::Char(c), KeyModifiers::ALT)
    }

    #[test]
    fn insert_move_delete_and_words_on_graphemes() {
        let mut e = LineEdit::new();
        assert_eq!(e.insert("cafe\u{0301} 👍🏽 test"), EditOutcome::Changed);
        assert_eq!(e.as_str(), "cafe\u{0301} 👍🏽 test");

        // Left walks across whole grapheme clusters without splitting combining marks or skin tone.
        for _ in 0..7 {
            assert_eq!(e.left(), EditOutcome::Moved);
        }
        // Cursor is before " 👍🏽 test", right after "cafe\u{0301}"; Backspace deletes whole 'e\u{0301}'.
        assert_eq!(e.backspace(), EditOutcome::Changed);
        assert_eq!(e.as_str(), "caf 👍🏽 test");
        assert_eq!(e.insert("e\u{0301}"), EditOutcome::Changed);
        assert_eq!(e.as_str(), "cafe\u{0301} 👍🏽 test");

        // Deleting whole skin-tone emoji with a single Delete/Backspace.
        assert_eq!(e.right(), EditOutcome::Moved); // after space, before 👍🏽
        assert_eq!(e.delete(), EditOutcome::Changed); // removes whole 👍🏽
        assert_eq!(e.as_str(), "cafe\u{0301}  test");

        // Word jumps, word delete, kill to end, clear.
        assert_eq!(e.word_right(), EditOutcome::Moved); // end of "test"
        assert_eq!(e.delete_word_left(), EditOutcome::Changed);
        assert_eq!(e.as_str(), "cafe\u{0301}  ");
        assert_eq!(e.word_left(), EditOutcome::Moved);
        assert_eq!(e.cursor(), 0);
        assert_eq!(e.word_right(), EditOutcome::Moved);
        assert_eq!(e.insert("!"), EditOutcome::Changed);
        assert_eq!(e.as_str(), "cafe\u{0301}!  ");

        assert_eq!(e.home(), EditOutcome::Moved);
        assert_eq!(e.left(), EditOutcome::Ignored);
        assert_eq!(e.end(), EditOutcome::Moved);
        assert_eq!(e.right(), EditOutcome::Ignored);

        assert_eq!(e.word_left(), EditOutcome::Moved); // before 'c' -> wait: "cafe\u{0301}!  " -> word_right from 0 lands after e\u{0301}
        assert_eq!(e.word_right(), EditOutcome::Moved); // right before '!'
        assert_eq!(e.kill_to_end(), EditOutcome::Changed);
        assert_eq!(e.as_str(), "cafe\u{0301}");
        assert_eq!(e.clear(), EditOutcome::Changed);
        assert_eq!(e.as_str(), "");
        assert_eq!(e.clear(), EditOutcome::Ignored);
    }

    #[test]
    fn combining_mark_inserted_at_zero_before_combining_keeps_valid_boundary() {
        let mut e = LineEdit::with_text("\u{0301}");
        assert_eq!(e.home(), EditOutcome::Moved);
        assert_eq!(e.insert("e"), EditOutcome::Changed);
        assert_eq!(e.as_str(), "e\u{0301}");
        assert!(
            e.as_str()
                .grapheme_indices(true)
                .any(|(i, _)| i == e.cursor())
                || e.cursor() == e.len()
        );
    }

    #[test]
    fn key_dispatch_and_policy_flags() {
        let mut e = LineEdit::new();
        for c in "one two three".chars() {
            assert_eq!(
                e.handle_key(KeyCode::Char(c), KeyModifiers::NONE, EditKeys::FULL),
                EditOutcome::Changed
            );
        }
        assert_eq!(
            e.handle_key(alt('b').0, alt('b').1, EditKeys::FULL),
            EditOutcome::Moved
        );
        assert_eq!(
            e.handle_key(ctrl('k').0, ctrl('k').1, EditKeys::FULL),
            EditOutcome::Changed
        );
        assert_eq!(e.as_str(), "one two ");
        assert_eq!(
            e.handle_key(ctrl('w').0, ctrl('w').1, EditKeys::FULL),
            EditOutcome::Changed
        );
        assert_eq!(e.as_str(), "one ");
        assert_eq!(
            e.handle_key(ctrl('a').0, ctrl('a').1, EditKeys::FULL),
            EditOutcome::Moved
        );
        assert_eq!(
            e.handle_key(
                key(KeyCode::Delete).0,
                key(KeyCode::Delete).1,
                EditKeys::FULL
            ),
            EditOutcome::Changed
        );
        assert_eq!(e.as_str(), "ne ");
        assert_eq!(
            e.handle_key(ctrl('e').0, ctrl('e').1, EditKeys::FULL),
            EditOutcome::Moved
        );
        assert_eq!(
            e.handle_key(KeyCode::Backspace, KeyModifiers::ALT, EditKeys::FULL),
            EditOutcome::Changed
        );
        assert_eq!(e.as_str(), "");
        e.set("ne");

        let restricted = EditKeys {
            arrows: false,
            home_end: false,
            delete: false,
            kill_to_end: false,
        };
        assert_eq!(
            e.handle_key(KeyCode::Left, KeyModifiers::NONE, restricted),
            EditOutcome::Ignored
        );
        assert_eq!(
            e.handle_key(KeyCode::Home, KeyModifiers::NONE, restricted),
            EditOutcome::Ignored
        );
        assert_eq!(
            e.handle_key(KeyCode::Delete, KeyModifiers::NONE, restricted),
            EditOutcome::Ignored
        );
        assert_eq!(
            e.handle_key(ctrl('k').0, ctrl('k').1, restricted),
            EditOutcome::Ignored
        );
        // Ctrl-A/E and Ctrl-Left/Right still work even when plain arrows/Home/End are reserved.
        assert_eq!(
            e.handle_key(ctrl('a').0, ctrl('a').1, restricted),
            EditOutcome::Moved
        );
        assert_eq!(e.as_str(), "ne");
    }

    #[test]
    fn sanitize_and_byte_limit_preserve_whole_graphemes() {
        let mut e = LineEdit::new().with_max_bytes(8);
        assert_eq!(
            e.paste("ab\u{1b}[31m\u{202e}👍🏽zz\nsecond"),
            EditOutcome::Changed
        );
        // Wait: "ab[31m" is 6 bytes, "👍🏽" is 8 bytes (exceeds remaining 2 bytes) -> dropped whole.
        assert_eq!(e.as_str(), "ab[31m");

        let mut e2 = LineEdit::new().with_max_bytes(10);
        // "👍🏽" is 8 bytes; adding 'x' + "😀"(4 bytes) leaves 'x' and drops whole "😀".
        e2.insert("👍🏽x😀");
        assert_eq!(e2.as_str(), "👍🏽x");
    }

    #[test]
    fn horizontal_scroll_wide_chars_and_click_placement() {
        let mut e = LineEdit::with_text("aあいうb");
        // Widths: a(1), あ(2), い(2), う(2), b(1) = 8 cols. Viewport = 4 cols.
        let v = e.view(4, false);
        assert_eq!(v.text, "うb");
        assert_eq!(v.cursor_col, 3);

        // Move left onto 'う' (col 5), then 'い' (col 3), then 'あ' (col 1), then 'a' (col 0).
        e.left(); // on 'b' (col 7)
        let v = e.view(4, false);
        assert_eq!((v.text.as_str(), v.cursor_col), ("うb", 2));
        e.left(); // on 'う' (col 5)
        let v = e.view(4, false);
        assert_eq!((v.text.as_str(), v.cursor_col), ("うb", 0));
        e.left(); // on 'い' (col 3) -> cursor < scroll(5), scrolls left to col 3 ('い' + 'b'? No: "い" 2 cols + "う" 2 cols = 4 cols!)
        let v = e.view(4, false);
        assert_eq!((v.text.as_str(), v.cursor_col), ("いう", 0));
        e.left(); // on 'あ' (col 1) -> "あい" (4 cols)
        let v = e.view(4, false);
        assert_eq!((v.text.as_str(), v.cursor_col), ("あい", 0));

        // Click on 'い' (at rel_col 2 of "あい" starting at scroll_col=1 => col 3).
        assert_eq!(e.place(2, false), EditOutcome::Moved);
        e.insert("X");
        assert_eq!(e.as_str(), "aあXいうb");

        // Zero-width viewport never panics.
        let v0 = e.view(0, false);
        assert_eq!((v0.text.as_str(), v0.cursor_col), ("", 0));
    }

    #[test]
    fn masked_view_and_secret_debug() {
        let mut s = LineEdit::secret();
        s.insert("paß👍🏽w"); // 5 graphemes, >5 UTF-8 bytes
        assert_eq!(format!("{s:?}"), "LineEdit(\"<redacted>\")");
        assert!(!format!("{s:?}").contains("pa"));
        assert!(!format!("{s:?}").contains('5'));

        let v = s.view(3, true);
        assert_eq!(v.text, "•••");
        assert_eq!(v.cursor_col, 3);

        s.left();
        s.left(); // on "👍🏽" (grapheme index 3)
        let v = s.view(3, true);
        assert_eq!(v.text, "•••");
        assert_eq!(v.cursor_col, 1);

        // Click on the 0th bullet of current viewport (scroll_col = 2) -> before "👍🏽" -> no move.
        // Click on the 2nd bullet (col 4) -> before 'w' -> moves.
        assert_eq!(s.place(2, true), EditOutcome::Moved);
        s.insert("!");
        assert_eq!(s.as_str(), "paß👍🏽!w");
    }
}
