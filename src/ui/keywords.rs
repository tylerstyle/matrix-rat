//! The notification keywords popup: what keywords do, a field to add one, the list with
//! a remove mark on the selected row, and a line for what is going on.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::hit::{HitId, InputTarget};
use super::modal::{centered, field, popup, row};
use crate::app::App;
use crate::keywords::{KeywordsPopup, Pending, confirm_prompt};
use crate::text::{sanitize_line, truncate, width, wrap};
use crate::theme::{Role, Theme};

const WIDTH: u16 = 72;
/// List rows the popup grows to; beyond, the list scrolls.
const MOST_ROWS: usize = 12;
const INTRO: &str = "Messages with one of these words notify you like a mention. Your \
                     server keeps them, so your other clients use them too.";
const REMOVE: &str = " remove ";

pub(super) fn draw(frame: &mut Frame, app: &mut App, area: Rect, state: &mut KeywordsPopup) {
    let inner_width = WIDTH.min(area.width.saturating_sub(2)).saturating_sub(4) as usize;
    let intro = wrap(INTRO, inner_width.max(1));
    let rows = state.keywords().len().clamp(3, MOST_ROWS);
    // The explanation, the field, a blank line before each, the list, the status line.
    let height = 2 + intro.len() + 1 + 2 + rows + 1;
    let Some(rect) = centered(area, WIDTH, height as u16) else {
        return;
    };
    let inner = popup(
        frame,
        app,
        area,
        rect,
        "Notification keywords",
        "Enter add · Del remove · ↑↓ select · Esc close",
    );
    let theme = app.theme.clone();
    if inner.width < 10 || inner.height < 3 {
        return;
    }
    // Where the screen is low, the explanation makes way for the list. It stays when
    // there is room for it and a blank line, the field and a blank line, three rows of
    // the list, and the status line.
    let mut y = 0u16;
    if inner.height as usize > intro.len() + 1 + 2 + 3 {
        for line in &intro {
            frame.render_widget(
                Line::from(Span::styled(line.as_str(), theme.style(Role::Muted))),
                row(inner, y),
            );
            y += 1;
        }
        y += 1;
    }
    let (cursor, input_rect) = field(
        frame,
        &theme,
        row(inner, y),
        "Add",
        &mut state.input,
        false,
        true,
    );
    app.hits.add(input_rect, HitId::Input(InputTarget::Keyword));
    y += if inner.height >= y + 4 { 2 } else { 1 };

    let status_y = inner.height - 1;
    let list = Rect::new(
        inner.x,
        inner.y + y,
        inner.width,
        status_y.saturating_sub(y),
    );
    draw_list(frame, app, &theme, list, state);

    let (text, role) = status_text(state);
    frame.render_widget(
        Line::from(Span::styled(
            truncate(&text, inner.width as usize),
            theme.style(role),
        )),
        row(inner, status_y),
    );
    if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
}

fn draw_list(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    area: Rect,
    state: &mut KeywordsPopup,
) {
    let rows = area.height as usize;
    state.page = rows;
    if rows == 0 {
        return;
    }
    let Some(keywords) = &state.list else {
        frame.render_widget(
            Line::from(Span::styled("Loading…", theme.style(Role::Muted))),
            Rect::new(area.x, area.y, area.width, 1),
        );
        return;
    };
    if keywords.is_empty() {
        frame.render_widget(
            Line::from(Span::styled(
                truncate("Type a word above and press Enter.", area.width as usize),
                theme.style(Role::Muted),
            )),
            Rect::new(area.x, area.y, area.width, 1),
        );
        return;
    }
    if state.reveal {
        state.reveal = false;
        if state.selected < state.scroll {
            state.scroll = state.selected;
        } else if state.selected >= state.scroll + rows {
            state.scroll = state.selected + 1 - rows;
        }
    }
    state.scroll = state.scroll.min(keywords.len().saturating_sub(rows));
    let mark_width = width(REMOVE) as u16;
    for (offset, keyword) in keywords.iter().skip(state.scroll).take(rows).enumerate() {
        let index = state.scroll + offset;
        let at = Rect::new(area.x, area.y + offset as u16, area.width, 1);
        let selected = index == state.selected;
        let hovered = matches!(
            app.hover,
            Some(HitId::KeywordRow(i) | HitId::KeywordRemove(i)) if i == index
        );
        let shows_mark = (selected || hovered) && at.width > mark_width + 6;
        let room = (at.width as usize).saturating_sub(
            2 + if shows_mark {
                mark_width as usize + 1
            } else {
                0
            },
        );
        let style = if selected {
            theme.style(Role::TitleFocused)
        } else if hovered {
            theme.style(Role::Hover)
        } else {
            theme.style(Role::Text)
        };
        frame.render_widget(
            Line::from(vec![
                Span::styled(
                    if selected { "› " } else { "  " },
                    theme.style(Role::TitleFocused),
                ),
                Span::styled(truncate(&sanitize_line(keyword), room), style),
            ]),
            at,
        );
        app.hits.add(at, HitId::KeywordRow(index));
        if shows_mark {
            let mark = Rect::new(at.right() - mark_width, at.y, mark_width, 1);
            let asked = state.confirm.as_ref() == Some(keyword);
            let role = if asked {
                Role::Error
            } else if app.hover == Some(HitId::KeywordRemove(index)) {
                Role::Hover
            } else {
                Role::Pill
            };
            frame.render_widget(Line::from(Span::styled(REMOVE, theme.style(role))), mark);
            app.hits.add(mark, HitId::KeywordRemove(index));
        }
    }
}

/// The bottom line: the question first, then what is under way, what went wrong, or
/// how many there are.
fn status_text(state: &KeywordsPopup) -> (String, Role) {
    if let Some(keyword) = &state.confirm {
        return (confirm_prompt(keyword), Role::Error);
    }
    match &state.pending {
        Some((_, Pending::Add(_))) => return ("Adding…".to_owned(), Role::Muted),
        Some((_, Pending::Remove(_))) => return ("Removing…".to_owned(), Role::Muted),
        None => {}
    }
    if let Some(error) = &state.error {
        return (sanitize_line(error), Role::Error);
    }
    let Some(keywords) = &state.list else {
        return (String::new(), Role::Muted);
    };
    let text = match (keywords.len(), state.total) {
        (_, 0) => "No keywords yet".to_owned(),
        (_, 1) => "1 keyword".to_owned(),
        (shown, total) if shown < total => format!("{shown} of {total} keywords shown"),
        (_, total) => format!("{total} keywords"),
    };
    (text, Role::Muted)
}
