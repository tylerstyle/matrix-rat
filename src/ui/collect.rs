//! Drawing the popups of an archive and of catching up: a few sentences that wrap,
//! the "how far back" list, a summary that scrolls, and the buttons, which stay in
//! view at the bottom.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::hit::HitId;
use super::modal::{centered, popup};
use crate::app::App;
use crate::app::collect::{
    ArchivePopup, ArchiveStage, CLICK_CLOSE, CLICK_COPY, CLICK_GO, CatchUpPopup, CatchUpStage, Of,
    Popup, count, cut_note, thousands,
};
use crate::collect::{Cut, Span as Stretch};
use crate::files;
use crate::matrix::{ARCHIVE_EVENTS, LAST_EVENTS};
use crate::text::{sanitize_line, truncate, width, wrap_at_spaces};
use crate::theme::{Role, Theme};

/// The popup's width, where the screen has it.
const WIDTH: u16 = 72;
/// The longest room name in a title.
const NAME: usize = 40;

/// The buttons of each stage, with what a click on them is.
type Buttons = &'static [(&'static str, u8)];
const ASK: Buttons = &[(" Write archive ", CLICK_GO), (" Cancel ", CLICK_CLOSE)];
const CANCEL: Buttons = &[(" Cancel ", CLICK_CLOSE)];
const CLOSE: Buttons = &[(" Close ", CLICK_CLOSE)];
const ASK_NEWEST: Buttons = &[(" Summarise ", CLICK_GO), (" Cancel ", CLICK_CLOSE)];
const SUMMARY: Buttons = &[(" Copy ", CLICK_COPY), (" Close ", CLICK_CLOSE)];

/// A line of a popup, and the row of the "how far back" list it is, if one.
type Row = (Line<'static>, Option<u8>);

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect, which: &mut Popup) {
    match which {
        Popup::Archive(archive) => draw_archive(frame, app, area, archive),
        Popup::CatchUp(catch_up) => draw_catch_up(frame, app, area, catch_up),
    }
}

/// The columns inside the border and its padding.
fn room(area: Rect) -> usize {
    WIDTH.min(area.width.saturating_sub(2)).saturating_sub(4) as usize
}

/// `text` as lines of at most `room` columns in the colour of `role`.
fn sentences(theme: &Theme, text: &str, room: usize, role: Role) -> Vec<Row> {
    wrap_at_spaces(text, room.max(8))
        .into_iter()
        .map(|part| (Line::from(Span::styled(part, theme.style(role))), None))
        .collect()
}

fn blank() -> Row {
    (Line::default(), None)
}

fn draw_archive(frame: &mut Frame, app: &mut App, area: Rect, archive: &ArchivePopup) {
    let theme = app.theme.clone();
    let room = room(area);
    let title = format!(
        "Archive {}",
        truncate(sanitize_line(&archive.room_name).trim(), NAME)
    );
    let mut rows: Vec<Row> = Vec::new();
    let (footer, buttons): (&str, Buttons) = match &archive.stage {
        ArchiveStage::Asking => {
            rows.push((
                Line::from(Span::styled(
                    "How far back?",
                    theme.style(Role::SidebarSection),
                )),
                None,
            ));
            for (at, span) in archive.spans().iter().copied().enumerate() {
                let chosen = at == archive.selected;
                let mark = if chosen { "● " } else { "○ " };
                let mut label = span.label();
                if span == Stretch::Everything {
                    label = format!("{label}, at most {}", count(ARCHIVE_EVENTS, ""));
                }
                let style = if chosen {
                    theme.style(Role::Text).add_modifier(Modifier::BOLD)
                } else {
                    theme.style(Role::Text)
                };
                rows.push((
                    Line::from(Span::styled(
                        truncate(&format!("{mark}{label}"), room),
                        style,
                    )),
                    Some(at as u8),
                ));
            }
            rows.push(blank());
            rows.extend(sentences(
                &theme,
                "Text only: pictures, files and voice messages are named, not included.",
                room,
                Role::StatusKey,
            ));
            rows.extend(sentences(
                &theme,
                &format!(
                    "Written as Markdown to {}, readable only by you. A file that is \
                     there is never overwritten: the new one gets a number.",
                    files::display(&archive.dir.join(&archive.file_name))
                ),
                room,
                Role::Muted,
            ));
            ("↑↓ choose · Enter write · Esc cancel", ASK)
        }
        ArchiveStage::Running { loaded, .. } => {
            rows.extend(sentences(
                &theme,
                &format!("Loading messages… {loaded} so far."),
                room,
                Role::Text,
            ));
            rows.extend(sentences(
                &theme,
                "The file is written when they are all there. Esc gives it up; no file \
                 is left then.",
                room,
                Role::Muted,
            ));
            ("Esc cancel", CANCEL)
        }
        ArchiveStage::Done(archived) => {
            rows.extend(sentences(
                &theme,
                &format!(
                    "Archived {} to {}",
                    count(archived.messages, ""),
                    files::display(&archived.path)
                ),
                room,
                Role::StatusOk,
            ));
            if let Some(note) = cut_note(archived.cut) {
                rows.extend(sentences(&theme, &note, room, Role::StatusKey));
            }
            rows.extend(sentences(
                &theme,
                "Text only: what was sent as pictures, files or voice messages is not in it.",
                room,
                Role::Muted,
            ));
            ("Enter close", CLOSE)
        }
        ArchiveStage::Failed(why) => {
            rows.extend(sentences(
                &theme,
                &format!("Could not archive the room: {}", sanitize_line(why)),
                room,
                Role::Error,
            ));
            ("Enter close", CLOSE)
        }
    };
    let height = u16::try_from(rows.len())
        .unwrap_or(u16::MAX)
        .saturating_add(4);
    let Some(rect) = centered(area, WIDTH, height) else {
        return;
    };
    let inner = popup(frame, app, area, rect, &title, footer);
    if inner.width < 10 || inner.height == 0 {
        return;
    }
    let shown = inner.height.saturating_sub(2) as usize;
    for (n, (line, click)) in rows.into_iter().take(shown).enumerate() {
        let rect = Rect::new(inner.x, inner.y + n as u16, inner.width, 1);
        frame.render_widget(line, rect);
        if let Some(click) = click {
            app.hits.add(rect, HitId::Collect(click));
        }
    }
    draw_buttons(frame, app, &theme, inner, buttons);
}

fn draw_catch_up(frame: &mut Frame, app: &mut App, area: Rect, catch_up: &mut CatchUpPopup) {
    let theme = app.theme.clone();
    let room = room(area);
    let title = format!(
        "Catch up on {}",
        truncate(sanitize_line(&catch_up.room_name).trim(), NAME)
    );
    let program = sanitize_line(&catch_up.program);
    // What stays in view, and what scrolls under it.
    let mut head: Vec<Row> = Vec::new();
    let mut body: Vec<Row> = Vec::new();
    let (footer, buttons): (&str, Buttons) = match &catch_up.stage {
        CatchUpStage::Collecting { loaded, .. } => {
            let (what, its) = match catch_up.last {
                Some(1) => ("the last message".to_owned(), "Its"),
                Some(last) => (format!("the last {}", count(last, "")), "Their"),
                None => ("what is unread".to_owned(), "Its"),
            };
            head.extend(sentences(
                &theme,
                &format!("Loading {what}… {loaded} messages looked at."),
                room,
                Role::Text,
            ));
            head.extend(sentences(
                &theme,
                &format!("{its} text is then given to your summary command ({program})."),
                room,
                Role::Muted,
            ));
            ("Esc cancel", CANCEL)
        }
        CatchUpStage::Asking { messages, .. } => {
            head.extend(sentences(
                &theme,
                "Where you stopped reading in this room was not found.",
                room,
                Role::StatusKey,
            ));
            head.extend(sentences(
                &theme,
                &format!(
                    "Summarise the newest {} instead? Some of them you may have read. \
                     Their text is then given to your summary command ({program}).",
                    count(*messages, "")
                ),
                room,
                Role::Text,
            ));
            ("Enter summarise · Esc leave", ASK_NEWEST)
        }
        CatchUpStage::Summarising { messages, .. } => {
            head.extend(sentences(
                &theme,
                &format!(
                    "Waiting for {program} to summarise {}…",
                    count(*messages, "")
                ),
                room,
                Role::Text,
            ));
            head.extend(sentences(
                &theme,
                "Esc stops it. Nothing is posted to the room.",
                room,
                Role::Muted,
            ));
            ("Esc cancel", CANCEL)
        }
        CatchUpStage::Done {
            summary,
            messages,
            cut,
            of,
        } => {
            let what = summarised(*messages, *cut, *of, &program);
            head.extend(sentences(&theme, &what, room, Role::SidebarSection));
            head.extend(sentences(
                &theme,
                "Only you see this; nothing was posted to the room.",
                room,
                Role::Muted,
            ));
            head.push(blank());
            for line in summary.lines() {
                body.extend(sentences(&theme, &sanitize_line(line), room, Role::Text));
            }
            ("↑↓ scroll · c copy · Esc close", SUMMARY)
        }
        CatchUpStage::Nothing => {
            let nothing = match catch_up.last {
                Some(_) => "No messages were found in this room.",
                None => "Nothing is unread in this room.",
            };
            head.extend(sentences(&theme, nothing, room, Role::Text));
            ("Enter close", CLOSE)
        }
        CatchUpStage::NothingInNewest => {
            head.extend(sentences(
                &theme,
                &format!(
                    "No messages were found in the newest {} events of this room; catching \
                     up looks no further back.",
                    thousands(LAST_EVENTS)
                ),
                room,
                Role::Text,
            ));
            ("Enter close", CLOSE)
        }
        CatchUpStage::Failed(why) => {
            head.extend(sentences(&theme, &sanitize_line(why), room, Role::Error));
            ("Enter close", CLOSE)
        }
    };
    let height = u16::try_from(head.len() + body.len())
        .unwrap_or(u16::MAX)
        .saturating_add(4);
    let Some(rect) = centered(area, WIDTH, height.min(area.height.saturating_sub(2))) else {
        return;
    };
    // The summary's rows: what the head and the buttons leave.
    let page = (rect.height.saturating_sub(4) as usize)
        .saturating_sub(head.len())
        .max(1);
    catch_up.page = page;
    catch_up.scroll = catch_up.scroll.min(body.len().saturating_sub(page));
    let title = if body.len() > page {
        format!(
            "{title} · {}–{} of {}",
            catch_up.scroll + 1,
            (catch_up.scroll + page).min(body.len()),
            body.len()
        )
    } else {
        title
    };
    let inner = popup(frame, app, area, rect, &title, footer);
    if inner.width < 10 || inner.height == 0 {
        return;
    }
    let shown = inner.height.saturating_sub(2) as usize;
    let body = body.into_iter().skip(catch_up.scroll).take(page);
    for (n, (line, _)) in head.into_iter().chain(body).take(shown).enumerate() {
        frame.render_widget(line, Rect::new(inner.x, inner.y + n as u16, inner.width, 1));
    }
    draw_buttons(frame, app, &theme, inner, buttons);
}

/// What a summary is of: `messages` of what `of` says, by `program`; `cut`: there
/// were more than a summary holds, and these are the newest.
fn summarised(messages: usize, cut: bool, of: Of, program: &str) -> String {
    match (of, cut) {
        (Of::Unread, false) => {
            format!("Summary of {}, by {program}.", count(messages, "unread"))
        }
        (Of::Unread, true) => format!(
            "Summary of the newest {} (there are more), by {program}.",
            count(messages, "unread")
        ),
        (Of::Newest, _) => format!(
            "Summary of the newest {}, read or not, by {program}.",
            count(messages, "")
        ),
        (Of::Last { .. }, true) => format!(
            "Summary of the newest {} (the text of more is too long for one summary), \
             by {program}.",
            count(messages, "")
        ),
        (Of::Last { asked, stopped }, false) => {
            let fewer = if messages >= asked {
                String::new()
            } else {
                match stopped {
                    None => " (the room has no more)".to_owned(),
                    Some(Cut::LoadFailed) => " (older ones could not be loaded)".to_owned(),
                    Some(Cut::Limit) => format!(
                        " (the room has more, but catching up looks at its newest {} events \
                         only)",
                        thousands(LAST_EVENTS)
                    ),
                }
            };
            format!(
                "Summary of the last {}{fewer}, by {program}.",
                count(messages, "")
            )
        }
    }
}

/// The buttons as clickable pills on the last row inside the popup.
fn draw_buttons(frame: &mut Frame, app: &mut App, theme: &Theme, inner: Rect, buttons: Buttons) {
    let mut x = inner.x;
    let y = inner.bottom().saturating_sub(1);
    for (label, click) in buttons {
        let left = inner.right().saturating_sub(x);
        let pill = (width(label) as u16).min(left);
        if pill == 0 {
            break;
        }
        let rect = Rect::new(x, y, pill, 1);
        let id = HitId::Collect(*click);
        frame.render_widget(
            Line::from(Span::styled(
                truncate(label, pill as usize),
                theme.style(Role::Pill),
            )),
            rect,
        );
        app.hits.add(rect, id);
        x = x.saturating_add(pill + 2);
    }
}
