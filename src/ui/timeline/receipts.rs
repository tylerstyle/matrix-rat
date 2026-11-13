//! Read markers: who has read up to a message, said quietly at the right end of the
//! message's last row.
//!
//! They sit in the free space of a row that is there anyway, so a receipt that moves
//! from one message to the next (which they do all the time) changes no row count
//! and nothing on screen jumps. Where there is little room the marker shrinks
//! (fewer initials, then just the tick), and on a row that is full to the last
//! columns the end of the text gives way to it. No row is ever added.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::{INDENT, Row};
use crate::model::{ReadBy, Room, RoomKind};
use crate::text::{truncate, width};
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
pub(super) fn add(mut rows: Vec<Row>, room: &Room, max_width: usize, theme: &Theme) -> Vec<Row> {
    let direct = room.kind == RoomKind::Person;
    for at in 0..rows.len() {
        let source = rows[at].source;
        if rows.get(at + 1).is_some_and(|next| next.source == source) {
            continue;
        }
        let read_by = match room.messages.get(source) {
            Some(message) if !message.read_by.is_empty() => &message.read_by,
            _ => continue,
        };
        let row = &mut rows[at];
        // A picture is painted over its rows: its width counts as taken.
        let text = row.line.width();
        let taken = match row.image {
            Some(image) => text.max(INDENT.len() + image.cols as usize),
            None => text,
        };
        let free = max_width.saturating_sub(taken + GAP);
        if let Some(spans) = marker(read_by, direct, theme, free) {
            let marker_width: usize = spans.iter().map(|span| width(&span.content)).sum();
            let pad = max_width - text - marker_width;
            row.line.spans.push(Span::raw(" ".repeat(pad)));
            row.line.spans.extend(spans);
        } else if row.image.is_none() && max_width >= CUT_MIN {
            // "… ✓" over the end of the text. (Nothing can be written over a
            // picture; who read that is in the menu.)
            cut(&mut row.line, max_width - 3);
            let muted = Style {
                fg: theme.style(Role::Muted).fg,
                ..Style::new()
            };
            let pad = max_width - 3 - row.line.width();
            row.line
                .spans
                .push(Span::styled(format!("…{} ✓", " ".repeat(pad)), muted));
        }
    }
    rows
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
            let name = truncate(&reader.name, NAME);
            forms.push(vec![Span::styled(format!("✓ {name}"), muted)]);
        }
        for shown in (1..=TILES.min(read_by.readers.len())).rev() {
            let mut spans = vec![tick()];
            for reader in &read_by.readers[..shown] {
                spans.push(Span::raw(" "));
                let tile = initial(&reader.name);
                spans.push(Span::styled(
                    tile.trim_end().to_owned(),
                    Style::new().fg(theme.user_color(&reader.name)),
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
    use crate::model::{Message, Reader};
    use crate::theme::DEFAULT_THEME;

    fn readers(names: &[&str], total: u32) -> ReadBy {
        ReadBy {
            readers: names
                .iter()
                .map(|name| Reader {
                    name: (*name).to_owned(),
                    time: "12:00".into(),
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
