//! Transcripts and translations under messages (see `crate::notes`), and the buttons
//! that ask for them.
//!
//! A note is rows of its own after everything else of its message: a quiet bar, a
//! small label, and the text wrapped like a message's. The text is a program's output:
//! it was made safe and bounded when it became a note, and is drawn as plain text.
//!
//! The buttons are not laid out with the rows: they are painted into the free end of a
//! row of their message when it is drawn, so what is selected or under the mouse can
//! change them without the rows being laid out again.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{Free, INDENT, Row};
use crate::app::App;
use crate::model::Message;
use crate::notes::{Note, NoteKind, NoteState, Notes};
use crate::text::{truncate, width, wrap};
use crate::theme::{Role, Theme};
use crate::ui::hit::HitId;

/// In front of every row of a note.
const BAR: &str = "┊ ";
/// Columns kept free between what a row says and its buttons.
const GAP: usize = 2;

/// Adds the notes of `room`'s messages under them (see [`add_under`]).
#[cfg(test)]
pub(super) fn add(
    rows: Vec<Row>,
    room: &crate::model::Room,
    notes: &Notes,
    current: &dyn Fn(&Message, NoteKind, &Note) -> bool,
    max_width: usize,
    theme: &Theme,
) -> Vec<Row> {
    let mut out = Vec::with_capacity(rows.len());
    let mut rows = rows.into_iter().peekable();
    while let Some(row) = rows.next() {
        let source = row.source;
        let start = out.len();
        out.push(row);
        while let Some(next) = rows.next_if(|next| next.source == source) {
            out.push(next);
        }
        if let Some(message) = room.messages.get(source) {
            add_under(&mut out, start, message, notes, current, max_width, theme);
        }
    }
    out
}

/// Adds the notes of `message` under its rows, which are those of `rows` from `start`
/// on. A hidden note takes no row, nor does one that `current` says is not of its
/// message as it is now (the message was edited or deleted since); and a message whose
/// last row does not select it (a line of its own) gets none.
pub(super) fn add_under(
    rows: &mut Vec<Row>,
    start: usize,
    message: &Message,
    notes: &Notes,
    current: &dyn Fn(&Message, NoteKind, &Note) -> bool,
    max_width: usize,
    theme: &Theme,
) {
    if notes.is_empty() {
        return;
    }
    let Some(last) = rows[start..].last().filter(|row| row.msg.is_some()) else {
        return;
    };
    let source = last.source;
    let Some(event) = message.event_id() else {
        return;
    };
    let body_width = max_width.saturating_sub(INDENT.len()).max(1);
    for (kind, note) in notes.of(event) {
        if !note.hidden && current(message, kind, note) {
            rows.extend(note_rows(kind, note, source, body_width, theme));
        }
    }
}

/// The rows of one note under the message `msg`.
fn note_rows(
    kind: NoteKind,
    note: &Note,
    msg: usize,
    body_width: usize,
    theme: &Theme,
) -> Vec<Row> {
    let room = body_width.saturating_sub(width(BAR)).max(1);
    let quiet = theme.style(Role::Muted);
    // Foreground only, so the row's background (selection, hover) shows through.
    let fg = |role: Role| Style {
        fg: theme.style(role).fg,
        ..Style::new()
    };
    let row = |spans: Vec<Span<'static>>| {
        let mut line = vec![Span::raw(INDENT), Span::styled(BAR, quiet)];
        line.extend(spans);
        Row {
            line: Line::from(line),
            msg: Some(msg),
            source: msg,
            hits: Vec::new(),
            image: None,
            emotes: Vec::new(),
            audio: None,
            free: super::Free::End,
        }
    };
    let label = format!("{}: ", kind.label());
    let (text, style) = match &note.state {
        NoteState::Running => {
            return vec![row(vec![Span::styled(
                truncate(kind.running(), room),
                quiet.add_modifier(Modifier::ITALIC),
            )])];
        }
        NoteState::Done(text) => (text.as_str(), fg(Role::ReplyQuote)),
        NoteState::Failed(why) => (why.as_str(), fg(Role::Error)),
    };
    wrap(&format!("{label}{text}"), room)
        .into_iter()
        .enumerate()
        .map(|(n, line)| match line.strip_prefix(&label) {
            Some(rest) if n == 0 => row(vec![
                Span::styled(label.clone(), quiet),
                Span::styled(rest.to_owned(), style),
            ]),
            _ => row(vec![Span::styled(line, style)]),
        })
        .collect()
}

/// A button to paint on a message's row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NoteButton {
    pub kind: NoteKind,
    label: &'static str,
}

/// The buttons of a message: those that are always there, and those only there while
/// the message is selected or under the mouse.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Buttons {
    /// At the right end, wherever the mouse is.
    fixed: Vec<NoteButton>,
    /// To the left of those, so that none of them moves when these come and go.
    lit: Vec<NoteButton>,
}

impl Buttons {
    pub fn clear(&mut self) {
        self.fixed.clear();
        self.lit.clear();
    }
}

/// The buttons of `message`. The one that transcribes shows on every voice or audio
/// message it can be used on; the one that translates only while the message is `lit`
/// (selected, or under the mouse), or every text would carry one. Neither shows where
/// its program is not set up.
pub(super) fn buttons(app: &App, message: &Message, lit: bool) -> Buttons {
    let button = |kind: NoteKind| {
        let offer = app.note_offer(message, kind);
        offer.possible().then(|| NoteButton {
            kind,
            label: offer.word(kind),
        })
    };
    let mut buttons = Buttons::default();
    if message.is_sound() {
        buttons.fixed.extend(button(NoteKind::Transcript));
    }
    if lit {
        buttons.lit.extend(button(NoteKind::Translation));
    }
    buttons
}

/// Where on a row its buttons go, as the row says (see [`Free`]): after what the row
/// says, and before what is aligned to its right end if anything is (a code block's
/// "copy", a read marker). The columns the text takes, and the column the buttons must
/// end before; none fit on the row that names the sender.
pub(super) fn free_part(row: &Row, row_width: usize) -> (usize, usize) {
    match row.free {
        Free::End => (row.line.width(), row_width),
        Free::Before { text, at } => (
            usize::from(text),
            usize::from(at).saturating_sub(GAP).min(row_width),
        ),
        Free::Nowhere => (row_width, row_width),
    }
}

/// Where `buttons` go on a row whose text takes `taken` columns and where they must
/// end before column `end`: the column each starts at and its text. The fixed ones
/// are right-aligned by themselves, so they are in the same place whether the others
/// show or not; the others go to their left if there is room, and are left out if
/// not. `None`: the row has no room for the fixed ones (or, without any, for the
/// others): the next row of the message is tried.
pub(super) fn place(
    buttons: &Buttons,
    taken: usize,
    end: usize,
) -> Option<Vec<(u16, String, NoteKind)>> {
    let label = |button: &NoteButton| format!(" {} ", button.label);
    // One blank between two buttons.
    let span = |group: &[NoteButton]| {
        group
            .iter()
            .map(|b| width(&label(b)) + 1)
            .sum::<usize>()
            .saturating_sub(1)
    };
    let (anchored, optional): (&[NoteButton], &[NoteButton]) = if buttons.fixed.is_empty() {
        (&buttons.lit, &[])
    } else {
        (&buttons.fixed, &buttons.lit)
    };
    let needed = span(anchored);
    if anchored.is_empty() || taken + GAP + needed > end {
        return None;
    }
    let mut placed = Vec::new();
    let first = end - needed;
    let extra = span(optional);
    if !optional.is_empty() && taken + GAP + extra < first {
        let mut col = first - 1 - extra;
        for button in optional {
            let text = label(button);
            let text_width = width(&text);
            placed.push((col as u16, text, button.kind));
            col += text_width + 1;
        }
    }
    let mut col = first;
    for button in anchored {
        let text = label(button);
        let text_width = width(&text);
        placed.push((col as u16, text, button.kind));
        col += text_width + 1;
    }
    Some(placed)
}

/// The look of a button: quiet until its message is selected or under the mouse.
pub(super) fn button_style(theme: &Theme, lit: bool, hovered: bool) -> Style {
    match (lit, hovered) {
        (_, true) => theme.style(Role::Pill).add_modifier(Modifier::BOLD),
        (true, false) => theme.style(Role::Pill),
        (false, false) => theme.style(Role::Muted),
    }
}

/// Whether the mouse is on this button.
pub(super) fn is_hovered(hover: Option<HitId>, msg: usize, kind: NoteKind) -> bool {
    hover == Some(HitId::NoteButton(msg, kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemRef, Room, RoomKind};
    use crate::ui::timeline::build_rows;

    fn message(id: &str, body: &str) -> Message {
        Message {
            id: id.to_owned(),
            item: Some(ItemRef::Event(format!("${id}"))),
            sender: "alice".to_owned(),
            time: "10:00".to_owned(),
            day: "Today".to_owned(),
            body: body.to_owned(),
            ..Message::default()
        }
    }

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                row.line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_note_is_rows_under_its_message_wrapped_and_labelled() {
        let theme = Theme::builtin(crate::theme::DEFAULT_THEME).unwrap();
        let images = crate::images::ImageStore::new(crate::images::Graphics::halfblocks());
        let mut room = Room::new("!r", "room", RoomKind::Room);
        room.messages = vec![message("a", "Guten Morgen"), message("b", "second")];
        let mut notes = Notes::default();
        let all = |_: &Message, _: NoteKind, _: &Note| true;
        let plain = build_rows(&room, 30, &theme, None, &images);
        // Without notes nothing changes.
        assert_eq!(
            texts(&add(plain.clone(), &room, &notes, &all, 30, &theme)),
            texts(&plain)
        );

        notes.set(
            "$a",
            NoteKind::Translation,
            NoteState::Done("Good morning to all of you over there".into()),
        );
        notes.set("$a", NoteKind::Transcript, NoteState::Running);
        notes.set("$b", NoteKind::Translation, NoteState::Failed("no".into()));
        notes.set(
            "$elsewhere",
            NoteKind::Translation,
            NoteState::Done("x".into()),
        );
        let rows = add(plain.clone(), &room, &notes, &all, 30, &theme);
        let shown = texts(&rows);
        let at = shown.iter().position(|row| row.contains("Guten")).unwrap();
        assert_eq!(
            shown[at + 1..at + 5],
            [
                "  ┊ transcribing…",
                "  ┊ translation: Good morning",
                "  ┊ to all of you over there",
                "  second",
            ]
        );
        assert_eq!(shown.last().unwrap(), "  ┊ translation: no");
        // Every row fits, belongs to its message and selects it.
        for row in &rows {
            assert!(row.line.width() <= 30);
        }
        assert_eq!(rows[at + 2].msg, Some(0));
        assert_eq!(rows[at + 2].source, 0);
        assert_eq!(rows.last().unwrap().source, 1);

        // Hidden: no rows; shown again: the same rows.
        notes.toggle_hidden("$a", NoteKind::Translation);
        notes.remove("$a", NoteKind::Transcript);
        let hidden = texts(&add(plain.clone(), &room, &notes, &all, 30, &theme));
        assert!(!hidden.iter().any(|row| row.contains("Good morning")));
        notes.toggle_hidden("$a", NoteKind::Translation);
        let again = texts(&add(plain.clone(), &room, &notes, &all, 30, &theme));
        assert!(again.iter().any(|row| row.contains("Good morning")));

        // A very narrow pane still gets rows that fit.
        for max in 1..12 {
            for row in add(
                build_rows(&room, max, &theme, None, &images),
                &room,
                &notes,
                &all,
                max,
                &theme,
            ) {
                assert!(row.line.width() <= max.max(INDENT.len() + width(BAR) + 1));
            }
        }
    }

    #[test]
    fn a_note_that_is_not_of_its_message_any_more_is_not_drawn() {
        let theme = Theme::builtin(crate::theme::DEFAULT_THEME).unwrap();
        let images = crate::images::ImageStore::new(crate::images::Graphics::halfblocks());
        let mut room = Room::new("!r", "room", RoomKind::Room);
        room.messages = vec![message("a", "Guten Morgen"), message("b", "second")];
        let mut notes = Notes::default();
        notes.set("$a", NoteKind::Translation, NoteState::Done("first".into()));
        notes.set("$b", NoteKind::Translation, NoteState::Done("other".into()));
        let plain = build_rows(&room, 30, &theme, None, &images);
        let only_b = |message: &Message, _: NoteKind, _: &Note| message.id == "b";
        let shown = texts(&add(plain, &room, &notes, &only_b, 30, &theme)).join("\n");
        assert!(
            !shown.contains("first") && shown.contains("other"),
            "{shown}"
        );
    }

    fn button(kind: NoteKind, label: &'static str) -> NoteButton {
        NoteButton { kind, label }
    }

    #[test]
    fn buttons_go_to_the_free_end_of_a_row_or_nowhere() {
        let hear = button(NoteKind::Transcript, "transcribe");
        let read = button(NoteKind::Translation, "translate");
        let one = Buttons {
            fixed: vec![hear],
            lit: Vec::new(),
        };
        // " transcribe " is twelve columns, and two are kept free before it.
        assert_eq!(
            place(&one, 20, 40),
            Some(vec![(28, " transcribe ".to_owned(), NoteKind::Transcript)])
        );
        assert_eq!(place(&one, 26, 40).unwrap()[0].0, 28);
        assert_eq!(place(&one, 27, 40), None);
        assert_eq!(place(&Buttons::default(), 0, 40), None);

        // The one that only shows on a lit message goes to the left: the other stays
        // where it was, to the column.
        let two = Buttons {
            fixed: vec![hear],
            lit: vec![read],
        };
        assert_eq!(
            place(&two, 5, 40).unwrap(),
            [
                (16, " translate ".to_owned(), NoteKind::Translation),
                (28, " transcribe ".to_owned(), NoteKind::Transcript),
            ]
        );
        // No room for both: the fixed one alone, still in its place, on the same row.
        assert_eq!(place(&two, 14, 40).map(|placed| placed.len()), Some(2));
        assert_eq!(place(&two, 15, 40), place(&one, 15, 40));
        assert_eq!(place(&two, 27, 40), None);
        // Alone, the lit one takes the right end.
        let lit = Buttons {
            fixed: Vec::new(),
            lit: vec![read],
        };
        assert_eq!(
            place(&lit, 5, 40),
            Some(vec![(29, " translate ".to_owned(), NoteKind::Translation)])
        );
        assert_eq!(place(&lit, 28, 40), None);
    }

    #[test]
    fn the_free_part_of_a_row_ends_before_its_read_marker() {
        // No marker: all of the row after its text, blanks in it or not.
        let mut row = Row::plain(
            Line::from(vec![
                Span::raw("  "),
                Span::raw("hello"),
                Span::raw(" "),
                Span::raw("(edited)"),
            ]),
            Some(0),
        );
        assert_eq!(free_part(&row, 40), (16, 40));
        // Text, blanks, marker: the buttons end two columns before the marker.
        row.free = Free::Before { text: 16, at: 37 };
        assert_eq!(free_part(&row, 40), (16, 35));
        // The row that names the sender has no room for them.
        row.free = Free::Nowhere;
        let (taken, end) = free_part(&row, 40);
        assert_eq!(place(&lit_one(), taken, end), None);
    }

    fn lit_one() -> Buttons {
        Buttons {
            fixed: Vec::new(),
            lit: vec![button(NoteKind::Translation, "translate")],
        }
    }
}
