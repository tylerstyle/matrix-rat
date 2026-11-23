//! Read markers: who has read up to a message, said quietly at the right end of the
//! message's last row.
//!
//! They sit in the free space of a row that is there anyway, so a receipt that moves
//! from one message to the next (which they do all the time) changes no row count
//! and nothing on screen jumps. Where there is little room the marker shrinks
//! (fewer initials, then just the tick), and on a row that is full to the last
//! columns the end of the text gives way to it. No row is ever added.
//!
//! What a row has at its right end already, a code block's "copy", stays there and
//! whole: the marker goes before it, as if the row ended there. Only where not even
//! the smallest marker fits before it does it give way to the marker. Nothing that
//! can be clicked lies under a marker.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::{Free, INDENT, Row};
use crate::images::EMOTE_CELLS;
use crate::model::{ReadBy, fit_name};
use crate::text::width;
use crate::theme::{Role, Theme};
use crate::ui::sidebar::initial;

/// Columns kept free between a message's text and its marker.
const GAP: usize = 2;
/// Narrowest view in which the end of a full row gives way to a marker.
const CUT_MIN: usize = 12;
/// Initials shown at most; the rest is counted.
const TILES: usize = 3;
/// Longest name spelled out, in columns.
const NAME: usize = 16;

/// Adds the read markers of `room`'s messages to their rows.
///
/// No row is ever added: how many rows a message takes does not depend on who has
/// read it, so nothing moves when receipts do. Where the last row is full to the
/// last columns, its end gives way to the smallest marker ("… ✓") for as long as
/// a receipt sits there.
#[cfg(test)]
pub(super) fn add(
    mut rows: Vec<Row>,
    room: &crate::model::Room,
    max_width: usize,
    theme: &Theme,
) -> Vec<Row> {
    let direct = room.kind == crate::model::RoomKind::Person;
    for at in 0..rows.len() {
        let source = rows[at].source;
        if rows.get(at + 1).is_some_and(|next| next.source == source) {
            continue;
        }
        if let Some(message) = room.messages.get(source) {
            mark(&mut rows[at], &message.read_by, direct, max_width, theme);
        }
    }
    rows
}

/// Adds the read marker of a message (those who read up to it: `read_by`) to `row`,
/// its last row. `direct`: in a chat with one person.
pub(super) fn mark(row: &mut Row, read_by: &ReadBy, direct: bool, max_width: usize, theme: &Theme) {
    if read_by.is_empty() {
        return;
    }
    // What the row says, and where the marker has to end: before what is aligned to
    // the row's end, if anything is, and the gap to it.
    let (text, end) = match row.free {
        Free::Before { text, at } => (usize::from(text), usize::from(at).saturating_sub(GAP)),
        Free::End | Free::Nowhere => (row.line.width(), max_width),
    };
    // A picture is painted over its rows: its width counts as taken.
    let taken = match row.image {
        Some(image) => text.max(INDENT.len() + image.cols as usize),
        None => text,
    };
    let free = end.saturating_sub(taken + GAP);
    let fits = marker(read_by, direct, theme, free);
    let cuts = row.image.is_none() && end >= CUT_MIN;
    if fits.is_none()
        && !cuts
        && let Free::Before { at, .. } = row.free
    {
        // Too narrow for a marker before what is aligned to the row's end: that
        // ("copy", which the menu has too) gives way, and the marker goes on the row
        // as on any other.
        take_aligned(row);
        row.hits.retain(|(from, ..)| *from < at);
        row.free = Free::End;
        return mark(row, read_by, direct, max_width, theme);
    }
    let (said, at) = if let Some(spans) = fits {
        let marker_width: usize = spans.iter().map(|span| width(&span.content)).sum();
        let aligned = take_aligned(row);
        let at = end - marker_width;
        row.line.spans.push(Span::raw(" ".repeat(at - text)));
        row.line.spans.extend(spans);
        put_back(row, end, aligned);
        // A voice or audio message that plays keeps it (see `draw`).
        if let Some(part) = &mut row.audio {
            part.marker = marker_width as u16;
        }
        (text, at)
    } else if cuts {
        // "… ✓" over the end of the text. (Nothing can be written over a
        // picture; who read that is in the menu.)
        let aligned = take_aligned(row);
        let at = end - 3;
        // A picture of two cells that the marker would split is not drawn, and its
        // cells go too: no half of a box is left before the "…".
        let from = row
            .emotes
            .iter()
            .map(|spot| usize::from(spot.col))
            .find(|col| *col < at && col + usize::from(EMOTE_CELLS.0) > at)
            .unwrap_or(at);
        cut(&mut row.line, from);
        // An emoji's picture cut into is not drawn over the marker, and what could be
        // clicked there (a link, the thread line) ends before it.
        row.emotes
            .retain(|spot| usize::from(spot.col) + usize::from(EMOTE_CELLS.0) <= at);
        let said = row.line.width();
        row.hits.retain_mut(|(from, to, _)| {
            // (What is aligned after the marker keeps its own.)
            if usize::from(*from) < end {
                *to = (*to).min(said as u16);
            }
            from < to
        });
        let muted = Style {
            fg: theme.style(Role::Muted).fg,
            ..Style::new()
        };
        let pad = at - said;
        row.line
            .spans
            .push(Span::styled(format!("…{} ✓", " ".repeat(pad)), muted));
        put_back(row, end, aligned);
        // Of that, a voice or audio message that plays keeps the tick: what was cut
        // is not shown meanwhile.
        if let Some(part) = &mut row.audio {
            part.marker = 2;
        }
        // The marker begins at the "…".
        (said, said)
    } else {
        return;
    };
    // The buttons of the message stay clear of the marker (see `notes`).
    if row.free != Free::Nowhere {
        row.free = Free::Before {
            text: said as u16,
            at: at as u16,
        };
    }
}

/// Takes what is aligned to the right end of `row` (a code block's "copy") off it,
/// with the blanks before it: the row then holds what it says, and the spans taken off
/// are given back with the column they start at.
fn take_aligned(row: &mut Row) -> Option<(usize, Vec<Span<'static>>)> {
    let Free::Before { text, at } = row.free else {
        return None;
    };
    let aligned = split_off(&mut row.line, usize::from(at));
    split_off(&mut row.line, usize::from(text));
    Some((usize::from(at), aligned))
}

/// Puts what [`take_aligned`] took back where it was, after the marker that ends at
/// column `end`.
fn put_back(row: &mut Row, end: usize, aligned: Option<(usize, Vec<Span<'static>>)>) {
    if let Some((at, spans)) = aligned {
        row.line.spans.push(Span::raw(" ".repeat(at - end)));
        row.line.spans.extend(spans);
    }
}

/// Takes the spans of `line` from column `column` on off it; the column is where one
/// of them starts.
fn split_off(line: &mut Line<'static>, column: usize) -> Vec<Span<'static>> {
    let mut start = 0;
    let at = line
        .spans
        .iter()
        .position(|span| {
            let here = start;
            start += width(&span.content);
            here >= column
        })
        .unwrap_or(line.spans.len());
    line.spans.split_off(at)
}

/// Shortens `line` to at most `columns` columns, cutting between characters.
fn cut(line: &mut Line<'static>, columns: usize) {
    use unicode_segmentation::UnicodeSegmentation;
    let mut left = columns;
    let mut keep = 0;
    for span in &mut line.spans {
        let span_width = width(&span.content);
        if span_width <= left {
            left -= span_width;
            keep += 1;
            continue;
        }
        let mut text = String::new();
        for grapheme in span.content.graphemes(true) {
            if width(grapheme) > left {
                break;
            }
            left -= width(grapheme);
            text.push_str(grapheme);
        }
        span.content = text.into();
        keep += 1;
        break;
    }
    line.spans.truncate(keep);
}

/// The marker in the widest form that fits into `room` columns, if any does.
///
/// With one other person: "✓ read". In a room: "✓ " and the readers' initials in
/// their colours (the same tiles' letters as in the room list), "+N" for those not
/// shown; a single reader is named where there is room.
fn marker(
    read_by: &ReadBy,
    direct: bool,
    theme: &Theme,
    room: usize,
) -> Option<Vec<Span<'static>>> {
    if read_by.is_empty() {
        return None;
    }
    let muted = Style {
        fg: theme.style(Role::Muted).fg,
        ..Style::new()
    };
    let tick = || Span::styled("✓", muted);
    let mut forms: Vec<Vec<Span<'static>>> = Vec::new();
    if direct && read_by.total == 1 {
        forms.push(vec![Span::styled("✓ read", muted)]);
    } else {
        if read_by.total == 1
            && let Some(reader) = read_by.readers.first()
        {
            // With the user ID where someone else goes by the same name, which is kept
            // whole (only the name is held to `NAME`); in the colour of the ID, as the
            // reader's tile is.
            let shown = reader.shown_name();
            let id_width = width(&shown).saturating_sub(width(&reader.name));
            let name = fit_name(&shown, NAME + id_width);
            forms.push(vec![
                Span::styled("✓ ", muted),
                Span::styled(name, Style::new().fg(theme.user_color(reader.key()))),
            ]);
        }
        for shown in (1..=TILES.min(read_by.readers.len())).rev() {
            let mut spans = vec![tick()];
            for reader in &read_by.readers[..shown] {
                spans.push(Span::raw(" "));
                let tile = initial(&reader.name);
                spans.push(Span::styled(
                    tile.trim_end().to_owned(),
                    Style::new().fg(theme.user_color(reader.key())),
                ));
            }
            let more = read_by.total as usize - shown;
            if more > 0 {
                spans.push(Span::styled(format!(" +{more}"), muted));
            }
            forms.push(spans);
        }
        forms.push(vec![Span::styled(format!("✓ {}", read_by.total), muted)]);
    }
    forms.push(vec![tick()]);
    forms
        .into_iter()
        .find(|spans| spans.iter().map(|span| width(&span.content)).sum::<usize>() <= room)
}

#[cfg(test)]
mod tests {
    use super::super::build_rows;
    use super::*;
    use crate::images::{Graphics, ImageStore};
    use crate::model::{Message, Reader, Room, RoomKind};
    use crate::theme::DEFAULT_THEME;

    fn readers(names: &[&str], total: u32) -> ReadBy {
        ReadBy {
            readers: names
                .iter()
                .map(|name| Reader {
                    name: (*name).to_owned(),
                    time: "12:00".into(),
                    ..Default::default()
                })
                .collect(),
            total,
        }
    }

    fn message(sender: &str, body: &str) -> Message {
        Message {
            id: body.to_owned(),
            sender: sender.into(),
            time: "12:00".into(),
            day: "Today".into(),
            body: body.into(),
            own: sender == "you",
            ..Message::default()
        }
    }

    fn room(kind: RoomKind, messages: Vec<Message>) -> Room {
        let mut room = Room::new("!r:example.org", "talk", kind);
        room.messages = messages;
        room
    }

    fn lines(room: &Room, max_width: usize) -> Vec<String> {
        let theme = Theme::builtin(DEFAULT_THEME).unwrap();
        let images = ImageStore::new(Graphics::halfblocks());
        let rows = build_rows(room, max_width, &theme, None, &images);
        add(rows, room, max_width, &theme)
            .iter()
            .map(|row| {
                // (Narrower than that, the messages themselves do not fit.)
                assert!(
                    max_width < 10 || row.line.width() <= max_width,
                    "{:?}",
                    row.line
                );
                row.line.to_string()
            })
            .collect()
    }

    fn text(read_by: &ReadBy, direct: bool, room: usize) -> Option<String> {
        let theme = Theme::builtin(DEFAULT_THEME).unwrap();
        let spans = marker(read_by, direct, &theme, room)?;
        let text: String = spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(width(&text) <= room, "{text:?} in {room}");
        Some(text)
    }

    #[test]
    fn the_marker_shrinks_with_the_room_it_has() {
        let one = readers(&["bob"], 1);
        assert_eq!(text(&one, true, 40).as_deref(), Some("✓ read"));
        assert_eq!(text(&one, true, 5).as_deref(), Some("✓"));
        assert_eq!(text(&one, true, 0), None);
        // In a room the one reader is named, or stands as an initial.
        assert_eq!(text(&one, false, 40).as_deref(), Some("✓ bob"));
        assert_eq!(text(&one, false, 4).as_deref(), Some("✓ B"));

        let many = readers(&["bob", "carol", "dave", "erin"], 9);
        assert_eq!(text(&many, false, 40).as_deref(), Some("✓ B C D +6"));
        assert_eq!(text(&many, false, 9).as_deref(), Some("✓ B C +7"));
        assert_eq!(text(&many, false, 7).as_deref(), Some("✓ B +8"));
        assert_eq!(text(&many, false, 5).as_deref(), Some("✓ 9"));
        assert_eq!(text(&many, false, 2).as_deref(), Some("✓"));
        // A chat with one person that someone else reads too is shown like a room.
        assert_eq!(text(&many, true, 40).as_deref(), Some("✓ B C D +6"));
        // Wide initials and long names stay within bounds.
        let wide = readers(&["日本語", "ßtraße"], 2);
        assert_eq!(text(&wide, false, 40).as_deref(), Some("✓ 日 SS"));
        let long = readers(&["a very long name that goes on and on"], 1);
        assert_eq!(width(&text(&long, false, 40).unwrap()), 2 + NAME);
    }

    #[test]
    fn markers_sit_at_the_right_end_of_the_last_row() {
        let mut messages = vec![
            message("you", "Lunch at 12?"),
            message("bob", "Sure."),
            message("you", "See you there"),
        ];
        messages[2].read_by = readers(&["bob"], 1);
        let direct = room(RoomKind::Person, messages.clone());
        let plain = {
            let mut bare = direct.clone();
            bare.messages[2].read_by = ReadBy::default();
            lines(&bare, 40)
        };
        let marked = lines(&direct, 40);
        // No row more or less, and only the one row differs.
        assert_eq!(marked.len(), plain.len());
        let changed: Vec<usize> = (0..plain.len())
            .filter(|n| plain[*n] != marked[*n])
            .collect();
        assert_eq!(changed, [plain.len() - 1]);
        let row = marked.last().unwrap();
        assert!(row.starts_with("  See you there"), "{row:?}");
        assert!(row.ends_with("✓ read"), "{row:?}");
        assert_eq!(width(row), 40);

        // In a room: initials, after reactions and thread lines too.
        let mut busy = room(RoomKind::Room, messages);
        busy.messages[2].read_by = readers(&["bob", "carol"], 2);
        busy.messages[2].reactions = vec![crate::model::Reaction {
            key: "👍".into(),
            count: 1,
            own: false,
        }];
        let marked = lines(&busy, 40);
        let row = marked.last().unwrap();
        assert!(row.contains("👍 1") && row.ends_with("✓ B C"), "{row:?}");
        // Nobody: nothing.
        assert!(marked.iter().filter(|row| row.contains('✓')).count() == 1);
    }

    #[test]
    fn a_receipt_that_moves_changes_no_row_count() {
        let messages = vec![
            message("you", "one"),
            message("you", "two"),
            message("alice", "three"),
            message("you", "four"),
        ];
        let base = room(RoomKind::Room, messages);
        for max_width in [12, 20, 40, 80] {
            let bare = lines(&base, max_width).len();
            for at in 0..base.messages.len() {
                for who in [readers(&["bob"], 1), readers(&["bob", "carol", "dave"], 7)] {
                    let mut moved = base.clone();
                    moved.messages[at].read_by = who;
                    assert_eq!(lines(&moved, max_width).len(), bare, "{max_width}, {at}");
                }
            }
        }
        // Rows that are full to the edge: the end of the text gives way to the
        // marker, and still no row is added or moved.
        let full = "x".repeat(38);
        let base = room(
            RoomKind::Room,
            vec![
                message("you", &full),
                message("alice", "short"),
                message("you", &full),
            ],
        );
        let bare = lines(&base, 40);
        for at in 0..3 {
            let mut moved = base.clone();
            moved.messages[at].read_by = readers(&["bob"], 1);
            let rows = lines(&moved, 40);
            assert_eq!(rows.len(), bare.len(), "{at}");
            let changed: Vec<usize> = (0..rows.len()).filter(|n| rows[*n] != bare[*n]).collect();
            assert_eq!(changed.len(), 1, "{at}: {rows:?}");
            let row = &rows[changed[0]];
            assert_eq!(width(row), 40, "{row:?}");
            if at == 1 {
                assert!(row.ends_with("✓ bob"), "{row:?}");
            } else {
                assert_eq!(*row, format!("  {}… ✓", "x".repeat(35)));
            }
        }
        // Wide characters are cut whole, and a thread line gives way as well.
        let mut wide = room(RoomKind::Room, vec![message("you", &"日".repeat(19))]);
        wide.messages[0].read_by = readers(&["bob"], 1);
        let rows = lines(&wide, 40);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2], format!("  {}…  ✓", "日".repeat(17)));
        wide.messages[0].body = "short".into();
        wide.messages[0].thread = Some(crate::model::ThreadSummary {
            replies: 3,
            latest: Some(("carol".into(), "a long answer ".repeat(5))),
            latest_sender: None,
            unread: false,
            demo_replies: Vec::new(),
        });
        let rows = lines(&wide, 40);
        assert_eq!(rows.len(), 4);
        assert!(
            rows[3].starts_with("  💬 3 replies") && rows[3].ends_with("… ✓"),
            "{rows:?}"
        );
        assert_eq!(width(&rows[3]), 40);
    }

    /// A message that ends on a one-line code block: its "copy" stays whole where it
    /// always is, clickable on its own columns only; the marker goes before it, and
    /// still no row is added.
    #[test]
    fn a_marker_leaves_a_code_blocks_copy_label_whole() {
        use crate::ui::hit::HitId;
        let theme = Theme::builtin(DEFAULT_THEME).unwrap();
        let images = ImageStore::new(Graphics::halfblocks());
        let mut code = message("you", "fn main() {}");
        code.html = Some("<pre><code>fn main() {}\n</code></pre>".into());
        for max_width in [20, 21, 24, 30, 44, 80] {
            let mut room = room(RoomKind::Room, vec![code.clone()]);
            let bare = build_rows(&room, max_width, &theme, None, &images);
            let copy = |row: &Row| {
                row.hits
                    .iter()
                    .find(|hit| matches!(hit.2, HitId::CopyCode(0, 0)))
                    .map(|hit| (hit.0, hit.1))
            };
            let label = (max_width as u16 - 4, max_width as u16);
            assert_eq!(copy(bare.last().unwrap()), Some(label), "{max_width}");
            for who in [readers(&["bob"], 1), readers(&["bob", "carol", "dave"], 7)] {
                room.messages[0].read_by = who;
                let rows = add(
                    build_rows(&room, max_width, &theme, None, &images),
                    &room,
                    max_width,
                    &theme,
                );
                assert_eq!(rows.len(), bare.len(), "{max_width}");
                let row = rows.last().unwrap();
                let text = row.line.to_string();
                assert!(
                    text.ends_with("  copy") && text.contains('✓'),
                    "{max_width}: {text:?}"
                );
                assert_eq!(width(&text), max_width, "{text:?}");
                assert_eq!(copy(row), Some(label), "{max_width}: {text:?}");
                // Nothing is clicked where the marker is.
                let tick = width(&text[..text.find('✓').unwrap()]) as u16;
                assert!(
                    row.hits.iter().all(|hit| !(hit.0..hit.1).contains(&tick)),
                    "{max_width}: {text:?} {:?}",
                    row.hits
                );
            }
        }
    }

    /// One reader of a name someone else has too is named with the id where there is
    /// room, in the colour of the id as their tile would be.
    #[test]
    fn a_single_reader_is_named_with_the_id_and_coloured_by_it() {
        let theme = Theme::builtin(DEFAULT_THEME).unwrap();
        let images = ImageStore::new(Graphics::halfblocks());
        let mut said = message("bob", "hi");
        said.read_by = ReadBy {
            readers: vec![Reader {
                name: "carol".into(),
                id: Some("@carol:elsewhere.org".into()),
                ambiguous: true,
                time: "12:00".into(),
            }],
            total: 1,
        };
        let room = room(RoomKind::Room, vec![said]);
        let rows = add(
            build_rows(&room, 80, &theme, None, &images),
            &room,
            80,
            &theme,
        );
        let row = rows.last().unwrap();
        let text = row.line.to_string();
        assert!(text.ends_with("✓ carol (@carol:elsewhere.org)"), "{text:?}");
        let name = row
            .line
            .spans
            .iter()
            .find(|span| span.content.contains("carol"))
            .unwrap();
        assert_eq!(
            name.style.fg,
            Some(theme.user_color("@carol:elsewhere.org"))
        );
    }

    /// Where "copy" fits on a code block's row but a marker does not fit before it,
    /// the marker is not left out: the label gives way to it.
    #[test]
    fn in_a_narrow_view_the_copy_label_gives_way_to_a_marker() {
        use crate::ui::hit::HitId;
        let theme = Theme::builtin(DEFAULT_THEME).unwrap();
        let images = ImageStore::new(Graphics::halfblocks());
        for max_width in 12..=20 {
            // A line of code that leaves room for "copy" and no more.
            let line = "x".repeat(max_width - 7);
            let mut code = message("you", &line);
            code.html = Some(format!("<pre><code>{line}\n</code></pre>"));
            let mut room = room(RoomKind::Room, vec![code]);
            room.messages[0].read_by = ReadBy::default();
            let bare = build_rows(&room, max_width, &theme, None, &images);
            room.messages[0].read_by = readers(&["bob"], 1);
            let rows = add(
                build_rows(&room, max_width, &theme, None, &images),
                &room,
                max_width,
                &theme,
            );
            assert_eq!(rows.len(), bare.len(), "{max_width}");
            let row = rows.last().unwrap();
            let text = row.line.to_string();
            assert!(text.contains('✓'), "{max_width}: {text:?}");
            assert!(width(&text) <= max_width, "{max_width}: {text:?}");
            let tick = width(&text[..text.find('✓').unwrap()]) as u16;
            assert!(
                row.hits.iter().all(|hit| !(hit.0..hit.1).contains(&tick)),
                "{max_width}: {text:?} {:?}",
                row.hits
            );
            // "copy" is either whole, with its button, or not there at all.
            let copy = row
                .hits
                .iter()
                .any(|hit| matches!(hit.2, HitId::CopyCode(0, 0)));
            assert_eq!(copy, text.ends_with("copy"), "{max_width}: {text:?}");
        }
    }

    /// Where the end of a row gives way to "… ✓", what could be clicked there stops
    /// before it.
    #[test]
    fn a_row_cut_for_a_marker_is_not_clicked_under_it() {
        let theme = Theme::builtin(DEFAULT_THEME).unwrap();
        let images = ImageStore::new(Graphics::halfblocks());
        let mut threaded = message("you", "short");
        threaded.thread = Some(crate::model::ThreadSummary {
            replies: 3,
            latest: Some(("carol".into(), "a long answer ".repeat(5))),
            latest_sender: None,
            unread: false,
            demo_replies: Vec::new(),
        });
        threaded.read_by = readers(&["bob"], 1);
        let room = room(RoomKind::Room, vec![threaded]);
        let rows = add(
            build_rows(&room, 40, &theme, None, &images),
            &room,
            40,
            &theme,
        );
        let row = rows.last().unwrap();
        let text = row.line.to_string();
        assert!(text.ends_with("… ✓"), "{text:?}");
        assert!(!row.hits.is_empty(), "{text:?}");
        let dots = width(&text[..text.find('…').unwrap()]) as u16;
        assert!(
            row.hits.iter().all(|hit| hit.1 <= dots),
            "{text:?}: {:?}",
            row.hits
        );
    }

    /// A picture the "… ✓" would split is dropped with both of its cells: no half of
    /// its box is left before the "…", and the marker stays.
    #[test]
    fn a_picture_split_by_the_marker_leaves_no_half_box() {
        use crate::ui::timeline::EmoteSpot;
        let theme = Theme::builtin(DEFAULT_THEME).unwrap();
        // Wide 40: the marker's "…" is at column 37, the picture on columns 36 and 37.
        let mut row = Row {
            line: Line::from(format!("{}  ", "x".repeat(36))),
            msg: Some(0),
            source: 0,
            hits: Vec::new(),
            image: None,
            emotes: vec![EmoteSpot {
                col: 36,
                url: "mxc://a/b".into(),
            }],
            audio: None,
            free: Free::End,
        };
        mark(&mut row, &readers(&["bob"], 1), false, 40, &theme);
        let text = row.line.to_string();
        assert_eq!(text, format!("{}…  ✓", "x".repeat(36)));
        assert!(row.emotes.is_empty());
    }

    #[test]
    fn snapshots_of_a_direct_chat_and_a_room() {
        let mut messages = vec![
            message("you", "Lunch at 12?"),
            message("bob", "Sure. The usual place?"),
            message(
                "you",
                "Yes. I will bring the laptop so we can look at the theme files.",
            ),
            message("you", "See you there"),
        ];
        messages[2].read_by = readers(&["bob"], 1);
        let direct = room(RoomKind::Person, messages);
        insta::assert_snapshot!("read_markers_direct", lines(&direct, 44).join("\n"));

        let mut messages = vec![
            message("alice", "Has anyone tried the new event cache yet?"),
            message("you", "Yes, startup is instant now."),
            message("you", "Back-pagination survives restarts too."),
            message("dave", "Shipping a client on top of it this week"),
        ];
        messages[0].read_by = readers(&["erin"], 1);
        messages[1].read_by = readers(&["bob", "carol", "frank", "grace", "heidi"], 12);
        messages[3].read_by = readers(&["alice", "ßtraße"], 2);
        messages[3].edited = true;
        let busy = room(RoomKind::Room, messages);
        insta::assert_snapshot!("read_markers_room", lines(&busy, 44).join("\n"));
    }

    fn drawn(app: &mut crate::app::App) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        format!("{}", terminal.backend())
    }

    /// The rows of two screens that differ.
    fn differing(a: &str, b: &str) -> Vec<usize> {
        let (a, b): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
        assert_eq!(a.len(), b.len());
        (0..a.len()).filter(|n| a[*n] != b[*n]).collect()
    }

    /// Draws the demo's first room with the receipts of two people on the message
    /// at `at`, or on none.
    fn place(app: &mut crate::app::App, at: Option<usize>) -> String {
        for message in &mut app.rooms[0].messages {
            message.read_by = ReadBy::default();
        }
        if let Some(at) = at {
            app.rooms[0].messages[at].read_by = readers(&["bob", "carol"], 2);
        }
        app.timeline_rev += 1;
        drawn(app)
    }

    #[test]
    fn on_screen_receipts_move_without_moving_anything_else_and_can_be_switched_off() {
        use crate::action::Action;
        use crate::modal::Setting;

        let mut app = crate::app::App::demo();
        assert!(!drawn(&mut app).contains('✓'));
        // The demo's messages have no ids; give the scroll anchor something to hold.
        for (n, message) in app.rooms[0].messages.iter_mut().enumerate() {
            message.id = format!("m{n}");
        }
        // At the newest message, and scrolled up into the history.
        for scrolled in [false, true] {
            if scrolled {
                app.timeline.scroll = 3;
            }
            let bare = place(&mut app, None);
            let scroll = app.timeline.scroll;
            let on_fourth = place(&mut app, Some(3));
            let on_fifth = place(&mut app, Some(4));
            assert_eq!(app.timeline.scroll, scroll);
            // One row each gained a marker; every other row is where it was.
            assert_eq!(differing(&bare, &on_fourth).len(), 1, "{on_fourth}");
            assert_eq!(differing(&bare, &on_fifth).len(), 1, "{on_fifth}");
            assert_eq!(differing(&on_fourth, &on_fifth).len(), 2);
            assert!(on_fifth.contains("✓ B C"), "{on_fifth}");
        }

        // Switched off: as if nobody had read anything. And on again.
        let on = drawn(&mut app);
        app.perform(Action::Setting(Setting::ReadMarkers));
        assert!(!app.read_markers);
        app.toast = None;
        let off = drawn(&mut app);
        assert!(!off.contains("✓ B C"), "{off}");
        assert_eq!(differing(&on, &off).len(), 1);
        app.perform(Action::Setting(Setting::ReadMarkers));
        assert!(drawn(&mut app).contains("✓ B C"));
    }

    #[test]
    fn narrow_views() {
        let mut messages = vec![message("you", "hello there")];
        messages[0].read_by = readers(&["bob", "carol"], 2);
        let room = room(RoomKind::Room, messages);
        // Never wider than the view, and no trouble down to nothing at all.
        for max_width in 0..30 {
            lines(&room, max_width);
        }
    }
}
