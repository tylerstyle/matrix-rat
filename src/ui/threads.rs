//! The threads popup: a room's threads, newest activity first, with a filter and an
//! "only mine" switch.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::hit::{HitId, InputTarget};
use super::modal::{centered, field, popup, row};
use crate::app::App;
use crate::modal::{ThreadsLoad, ThreadsPopup};
use crate::model::ThreadInfo;
use crate::text::{sanitize_line, truncate, width};
use crate::theme::{Role, Theme};

const WIDTH: u16 = 76;
const HEIGHT: u16 = 22;
/// Rows above the list: the filter and the status line.
const LIST_Y: u16 = 2;

pub(super) fn draw(frame: &mut Frame, app: &mut App, area: Rect, list: &mut ThreadsPopup) {
    let Some(rect) = centered(area, WIDTH, HEIGHT) else {
        return;
    };
    let room = truncate(
        &sanitize_line(&list.room_name),
        (rect.width as usize).saturating_sub(16),
    );
    let inner = popup(
        frame,
        app,
        area,
        rect,
        &format!("Threads — {room}"),
        "Enter open · Tab only mine · Esc close",
    );
    let theme = app.theme.clone();
    if inner.width < 10 || inner.height == 0 {
        return;
    }
    let (cursor, filter_rect) = field(
        frame,
        &theme,
        row(inner, 0),
        "Filter",
        &mut list.filter,
        false,
        true,
    );
    app.hits
        .add(filter_rect, HitId::Input(InputTarget::ThreadsFilter));
    status_line(frame, app, &theme, row(inner, 1), list);

    // Two lines a thread where there is room for it.
    let space = inner.height.saturating_sub(LIST_Y) as usize;
    let card = if space >= 2 { 2 } else { 1 };
    let fit = space / card;
    list.page = fit;
    let rows = list.rows();
    if fit > 0 {
        if list.reveal {
            list.reveal = false;
            if list.selected < list.scroll {
                list.scroll = list.selected;
            } else if list.selected >= list.scroll + fit {
                list.scroll = list.selected + 1 - fit;
            }
        }
        list.scroll = list.scroll.min(rows.saturating_sub(fit));
        if rows == 0 {
            frame.render_widget(
                Line::from(Span::styled(
                    truncate(empty_text(list), inner.width as usize),
                    theme.style(Role::Muted),
                )),
                row(inner, LIST_Y),
            );
        }
        for offset in 0..fit.min(rows.saturating_sub(list.scroll)) {
            let index = list.scroll + offset;
            let y = inner.y + LIST_Y + (offset * card) as u16;
            let rect = Rect::new(inner.x, y, inner.width, card as u16);
            let selected = index == list.selected;
            let (hit, hovered) = match list.thread_at(index) {
                Some(thread) => {
                    thread_card(frame, &theme, rect, thread, selected);
                    let hit = HitId::ThreadRow(index);
                    (hit, app.hover == Some(hit))
                }
                None => {
                    let label = if list.load == ThreadsLoad::Loading {
                        "  Loading more threads…"
                    } else {
                        "  Load more threads"
                    };
                    frame.render_widget(
                        Line::from(Span::styled(
                            truncate(label, rect.width as usize),
                            theme.style(Role::Link),
                        )),
                        Rect::new(rect.x, rect.y, rect.width, 1),
                    );
                    (HitId::ThreadsMore, app.hover == Some(HitId::ThreadsMore))
                }
            };
            if selected {
                frame
                    .buffer_mut()
                    .set_style(rect, theme.style(Role::MessageSelected));
            } else if hovered {
                frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
            }
            app.hits.add(rect, hit);
        }
    }
    if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
}

/// How far the list has come, on the left; the "only mine" switch on the right.
fn status_line(frame: &mut Frame, app: &mut App, theme: &Theme, rect: Rect, list: &ThreadsPopup) {
    if rect.width == 0 {
        return;
    }
    let switch = format!("{} Only mine", if list.mine { "●" } else { "○" });
    let switch_width = width(&switch) as u16;
    let shows_switch = rect.width >= switch_width + 12;
    let room = if shows_switch {
        rect.width - switch_width - 2
    } else {
        rect.width
    };
    let (text, role) = status_text(list);
    frame.render_widget(
        Line::from(Span::styled(
            truncate(&text, room as usize),
            theme.style(role),
        )),
        Rect::new(rect.x, rect.y, room, 1),
    );
    if shows_switch {
        let at = Rect::new(rect.right() - switch_width, rect.y, switch_width, 1);
        let style = if app.hover == Some(HitId::ThreadsMine) {
            theme.style(Role::Hover)
        } else if list.mine {
            theme.style(Role::TitleFocused)
        } else {
            theme.style(Role::Muted)
        };
        frame.render_widget(Line::from(Span::styled(switch, style)), at);
        app.hits.add(at, HitId::ThreadsMine);
    }
}

fn status_text(list: &ThreadsPopup) -> (String, Role) {
    if list.load == ThreadsLoad::Failed {
        return (
            "Could not load the server's thread list".to_owned(),
            Role::Error,
        );
    }
    if list.load == ThreadsLoad::Loading && list.listed.is_empty() {
        return ("Loading threads…".to_owned(), Role::Muted);
    }
    let count = list.shown.len();
    let text = if !list.filter.as_str().trim().is_empty() {
        format!("{count} matching")
    } else {
        let noun = if count == 1 { "thread" } else { "threads" };
        let more = list.load != ThreadsLoad::Done;
        format!("{count} {noun}{}", if more { " so far" } else { "" })
    };
    (text, Role::Muted)
}

fn empty_text(list: &ThreadsPopup) -> &'static str {
    if list.load == ThreadsLoad::Loading {
        ""
    } else if !list.filter.as_str().trim().is_empty() {
        "No matching threads."
    } else if list.load == ThreadsLoad::Failed {
        "No threads in the loaded messages."
    } else if list.mine {
        "No threads you started or answered in."
    } else {
        "No threads in this room yet."
    }
}

/// Who started the thread, how many replies, whether some are new, and who answered
/// last and when; below, the start of the first message.
fn thread_card(frame: &mut Frame, theme: &Theme, rect: Rect, thread: &ThreadInfo, selected: bool) {
    let room = rect.width as usize;
    let marker = if selected { "› " } else { "  " };
    let latest = match (&thread.latest_sender, &thread.latest_when) {
        (Some(who), Some(when)) => format!("{} · {}", sanitize_line(who), sanitize_line(when)),
        (Some(who), None) => sanitize_line(who),
        (None, Some(when)) => sanitize_line(when),
        (None, None) => String::new(),
    };
    let latest = truncate(&latest, room / 3);
    let replies = match thread.replies {
        1 => " · 1 reply".to_owned(),
        n => format!(" · {n} replies"),
    };
    let new = if thread.unread { " · new" } else { "" };
    // What is left of the line once the marker and the newest reply have their place;
    // the count keeps its place before the name does.
    let left = room.saturating_sub(width(marker) + width(&latest) + 1);
    let counts = width(&replies) + width(new);
    let sender = truncate(
        &sanitize_line(&thread.sender),
        left.saturating_sub(counts).max(left.min(8)),
    );
    let mut used = width(&sender);
    let mut spans = vec![
        Span::styled(
            marker,
            theme.style(if selected {
                Role::TitleFocused
            } else {
                Role::Muted
            }),
        ),
        Span::styled(sender, theme.style(Role::Text).add_modifier(Modifier::BOLD)),
    ];
    let mut push = |text: &str, style: Style| {
        let text = truncate(text, left.saturating_sub(used));
        used += width(&text);
        spans.push(Span::styled(text, style));
    };
    push(&replies, theme.style(Role::Muted));
    // As the line under a message that started a thread says it.
    let link = Style {
        fg: theme.style(Role::Link).fg,
        ..Style::new()
    };
    push(new, link.add_modifier(Modifier::BOLD));
    if !latest.is_empty() {
        let gap = room.saturating_sub(width(marker) + used + width(&latest));
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(latest, theme.style(Role::Muted)));
    }
    frame.render_widget(Line::from(spans), Rect::new(rect.x, rect.y, rect.width, 1));
    if rect.height >= 2 {
        let text = truncate(&sanitize_line(&thread.text), room.saturating_sub(4));
        frame.render_widget(
            Line::from(vec![
                Span::raw("    "),
                Span::styled(text, theme.style(Role::Text)),
            ]),
            Rect::new(rect.x, rect.y + 1, rect.width, 1),
        );
    }
}
