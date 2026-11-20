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

use super::{INDENT, Row};
use crate::app::App;
use crate::model::{Message, Room};
use crate::notes::{Note, NoteKind, NoteState, Notes};
use crate::text::{truncate, width, wrap};
use crate::theme::{Role, Theme};
use crate::ui::hit::HitId;

/// In front of every row of a note.
const BAR: &str = "┊ ";
/// Columns kept free between what a row says and its buttons.
const GAP: usize = 2;

/// Adds the notes of `room`'s messages under them. A hidden note takes no row.
pub(super) fn add(
    rows: Vec<Row>,
    room: &Room,
    notes: &Notes,
    max_width: usize,
    theme: &Theme,
) -> Vec<Row> {
    if notes.is_empty() {
        return rows;
    }
    let body_width = max_width.saturating_sub(INDENT.len()).max(1);
    let mut out = Vec::with_capacity(rows.len());
    let mut rows = rows.into_iter().peekable();
    while let Some(row) = rows.next() {
        let source = row.source;
        let selectable = row.msg.is_some();
        out.push(row);
        if rows.peek().is_some_and(|next| next.source == source) || !selectable {
            continue;
        }
        let Some(event) = room.messages.get(source).and_then(Message::event_id) else {
            continue;
        };
        for (kind, note) in notes.of(event).filter(|(_, note)| !note.hidden) {
            out.extend(note_rows(kind, note, source, body_width, theme));
        }
    }
    out
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

/// The buttons of `message`. The one that transcribes shows on every voice or audio
/// message it can be used on; the one that translates only while the message is `lit`
/// (selected, or under the mouse), or every text would carry one. Neither shows where
/// its program is not set up.
pub(super) fn buttons(app: &App, message: &Message, lit: bool) -> Vec<NoteButton> {
    let mut buttons = Vec::new();
    if message.is_sound() || lit {
        let offer = app.note_offer(message, NoteKind::Transcript);
        if offer.possible() {
            buttons.push(NoteButton {
                kind: NoteKind::Transcript,
                label: offer.word(NoteKind::Transcript),
            });
        }
    }
    if lit {
        let offer = app.note_offer(message, NoteKind::Translation);
        // A translation that is there can be put away from any message it is under.
        if offer.possible() {
            buttons.push(NoteButton {
                kind: NoteKind::Translation,
                label: offer.word(NoteKind::Translation),
            });
        }
    }
    buttons
}

/// Where `buttons` go on a row that says `taken` columns of `row_width`: the column
/// each starts at and its text, right-aligned. `None` if the row has no room for them.
pub(super) fn place(
    buttons: &[NoteButton],
    taken: usize,
    row_width: usize,
) -> Option<Vec<(u16, String, NoteKind)>> {
    let labels: Vec<String> = buttons
        .iter()
        .map(|button| format!(" {} ", button.label))
        .collect();
    let total: usize =
        labels.iter().map(|label| width(label)).sum::<usize>() + labels.len().saturating_sub(1);
    if buttons.is_empty() || taken + GAP + total > row_width {
        return None;
    }
    let mut col = row_width - total;
    let mut placed = Vec::new();
    for (button, label) in buttons.iter().zip(labels) {
        let label_width = width(&label);
        placed.push((col as u16, label, button.kind));
        col += label_width + 1;
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
    use crate::model::{ItemRef, RoomKind};
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
        let plain = build_rows(&room, 30, &theme, None, &images);
        // Without notes nothing changes.
        assert_eq!(
            texts(&add(plain.clone(), &room, &notes, 30, &theme)),
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
        let rows = add(plain.clone(), &room, &notes, 30, &theme);
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
        let hidden = texts(&add(plain.clone(), &room, &notes, 30, &theme));
        assert!(!hidden.iter().any(|row| row.contains("Good morning")));
        notes.toggle_hidden("$a", NoteKind::Translation);
        let again = texts(&add(plain.clone(), &room, &notes, 30, &theme));
        assert!(again.iter().any(|row| row.contains("Good morning")));

        // A very narrow pane still gets rows that fit.
        for max in 1..12 {
            for row in add(
                build_rows(&room, max, &theme, None, &images),
                &room,
                &notes,
                max,
                &theme,
            ) {
                assert!(row.line.width() <= max.max(INDENT.len() + width(BAR) + 1));
            }
        }
    }

    #[test]
    fn buttons_go_to_the_free_end_of_a_row_or_nowhere() {
        let one = [NoteButton {
            kind: NoteKind::Transcript,
            label: "transcribe",
        }];
        // " transcribe " is twelve columns, and two are kept free before it.
        assert_eq!(
            place(&one, 20, 40),
            Some(vec![(28, " transcribe ".to_owned(), NoteKind::Transcript)])
        );
        assert_eq!(place(&one, 26, 40).unwrap()[0].0, 28);
        assert_eq!(place(&one, 27, 40), None);
        assert_eq!(place(&[], 0, 40), None);
        let two = [
            one[0],
            NoteButton {
                kind: NoteKind::Translation,
                label: "translate",
            },
        ];
        let placed = place(&two, 5, 40).unwrap();
        assert_eq!(
            placed,
            [
                (16, " transcribe ".to_owned(), NoteKind::Transcript),
                (29, " translate ".to_owned(), NoteKind::Translation),
            ]
        );
        assert_eq!(place(&two, 15, 40), None);
    }
}
