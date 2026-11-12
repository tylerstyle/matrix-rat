use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};

use super::hit::{Button, HitId};
use crate::action::Pane;
use crate::app::App;
use crate::model::{Room, RoomKind};
use crate::text::{truncate, width, wrap};
use crate::theme::{Role, Theme};

/// One screen row of the timeline.
pub struct Row {
    line: Line<'static>,
    /// The message this row belongs to; `None` for dividers and spacing.
    msg: Option<usize>,
    /// Clickable column ranges inside the row.
    hits: Vec<(u16, u16, HitId)>,
}

impl Row {
    fn plain(line: Line<'static>, msg: Option<usize>) -> Self {
        Self {
            line,
            msg,
            hits: Vec::new(),
        }
    }
}

const INDENT: &str = "  ";

pub fn build_rows(room: &Room, max_width: usize, theme: &Theme) -> Vec<Row> {
    let body_width = max_width.saturating_sub(INDENT.len()).max(1);
    let text_style = Style::new().fg(theme.style(Role::Text).fg.unwrap_or_default());
    let mut rows = Vec::new();

    for (i, message) in room.messages.iter().enumerate() {
        let previous = i.checked_sub(1).map(|p| &room.messages[p]);
        let new_day = previous.is_none_or(|p| p.day != message.day);
        let show_header = new_day || previous.is_none_or(|p| p.sender != message.sender);

        if new_day {
            let label = truncate(&format!(" {} ", message.day), max_width);
            let fill = max_width.saturating_sub(width(&label));
            let left = fill / 2;
            rows.push(Row::plain(
                Line::from(Span::styled(
                    format!("{}{label}{}", "─".repeat(left), "─".repeat(fill - left)),
                    theme.style(Role::DateDivider),
                )),
                None,
            ));
        } else if show_header {
            rows.push(Row::plain(Line::default(), None));
        }

        if show_header {
            let color = Style::new().fg(theme.user_color(&message.sender));
            let mut spans = vec![Span::styled("● ", color)];
            let with_time = 2 + width(&message.sender) + 2 + width(&message.time) <= max_width;
            let name = truncate(&message.sender, max_width.saturating_sub(2));
            spans.push(Span::styled(name, color.add_modifier(Modifier::BOLD)));
            if with_time {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    message.time.clone(),
                    theme.style(Role::Timestamp),
                ));
            }
            rows.push(Row::plain(Line::from(spans), Some(i)));
        }

        if let Some((sender, quoted)) = &message.reply_to {
            let quote = truncate(
                &format!("┃ {sender}: {}", quoted.replace('\n', " ")),
                body_width,
            );
            rows.push(Row::plain(
                Line::from(vec![
                    Span::raw(INDENT),
                    Span::styled(quote, theme.style(Role::ReplyQuote)),
                ]),
                Some(i),
            ));
        }

        // The marker is wrapped together with the text so it can never overflow.
        const EDITED: &str = "(edited)";
        let mut body = if message.edited {
            wrap(&format!("{} {EDITED}", message.body), body_width)
        } else {
            wrap(&message.body, body_width)
        };
        let last = body.len() - 1;
        for (n, text) in body.drain(..).enumerate() {
            let mut spans = vec![Span::raw(INDENT)];
            match text.strip_suffix(EDITED) {
                Some(rest) if n == last && message.edited => {
                    spans.push(Span::styled(rest.to_owned(), text_style));
                    spans.push(Span::styled(EDITED, theme.style(Role::Muted)));
                }
                _ => spans.push(Span::styled(text, text_style)),
            }
            rows.push(Row::plain(Line::from(spans), Some(i)));
        }

        // Reaction chips, wrapped onto further rows when they do not fit.
        let mut spans = vec![Span::raw(INDENT)];
        let mut hits = Vec::new();
        let mut col = INDENT.len();
        for (r, reaction) in message.reactions.iter().enumerate() {
            let chip = truncate(
                &format!(" {} {} ", reaction.key, reaction.count),
                body_width,
            );
            let chip_width = width(&chip);
            if col + chip_width > max_width && !hits.is_empty() {
                rows.push(Row {
                    line: Line::from(std::mem::replace(&mut spans, vec![Span::raw(INDENT)])),
                    msg: Some(i),
                    hits: std::mem::take(&mut hits),
                });
                col = INDENT.len();
            }
            let role = if reaction.own {
                Role::ReactionOwn
            } else {
                Role::Reaction
            };
            spans.push(Span::styled(chip, theme.style(role)));
            hits.push((col as u16, (col + chip_width) as u16, HitId::Reaction(i, r)));
            col += chip_width;
            if col < max_width {
                spans.push(Span::raw(" "));
                col += 1;
            }
        }
        if !hits.is_empty() {
            rows.push(Row {
                line: Line::from(spans),
                msg: Some(i),
                hits,
            });
        }
    }
    rows
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let room = &app.rooms[app.active];
    let prefix = if room.kind == RoomKind::Person {
        "@"
    } else {
        "#"
    };
    let mut block = super::pane_block(app, Pane::Timeline, &format!("{prefix}{}", room.name));
    if let Some(who) = &room.typing {
        block = block.title_bottom(Line::from(Span::styled(
            format!(" {who} is typing… "),
            app.theme.style(Role::Muted),
        )));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.hits.add(area, HitId::Pane(Pane::Timeline));
    if inner.width < 2 || inner.height == 0 {
        return;
    }
    let theme = &app.theme;

    // The last column is kept free for the scrollbar.
    let content = Rect::new(inner.x, inner.y, inner.width - 1, inner.height);
    let rows = build_rows(room, content.width as usize, theme);
    let height = content.height as usize;
    let max_scroll = rows.len().saturating_sub(height);

    let view = &mut app.timeline;
    view.page = height;
    view.scroll = view.scroll.min(max_scroll);
    let mut top = max_scroll - view.scroll;
    if view.reveal_selected {
        view.reveal_selected = false;
        if let Some(selected) = view.selected {
            let first = rows.iter().position(|r| r.msg == Some(selected));
            let last = rows.iter().rposition(|r| r.msg == Some(selected));
            if let (Some(first), Some(last)) = (first, last) {
                if first < top {
                    top = first;
                } else if last >= top + height {
                    top = (last + 1 - height).min(first);
                }
                view.scroll = max_scroll - top.min(max_scroll);
                top = max_scroll - view.scroll;
            }
        }
    }
    let selected = view.selected;
    let scroll = view.scroll;

    // A short conversation sits at the bottom, next to the composer.
    let pad = height.saturating_sub(rows.len()) as u16;
    for (n, row) in rows.iter().skip(top).take(height).enumerate() {
        let rect = Rect::new(content.x, content.y + pad + n as u16, content.width, 1);
        if let Some(msg) = row.msg {
            if selected == Some(msg) {
                frame
                    .buffer_mut()
                    .set_style(rect, theme.style(Role::MessageSelected));
            } else if matches!(app.hover, Some(HitId::Message(m) | HitId::Reaction(m, _)) if m == msg)
            {
                frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
            }
            app.hits.add(rect, HitId::Message(msg));
        }
        frame.render_widget(&row.line, rect);
        for (start, end, id) in &row.hits {
            let start = (*start).min(rect.width);
            let end = (*end).min(rect.width);
            app.hits
                .add(Rect::new(rect.x + start, rect.y, end - start, 1), *id);
        }
    }

    if max_scroll > 0 {
        let mut state = ScrollbarState::new(max_scroll + 1).position(top);
        let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("┃")
            .track_style(theme.style(Role::Scrollbar))
            .thumb_style(theme.style(Role::ScrollbarThumb));
        frame.render_stateful_widget(bar, inner, &mut state);
    }

    if scroll > 0 {
        let label = " ↓ newest ";
        let label_width = width(label) as u16;
        if content.width > label_width + 1 {
            let rect = Rect::new(
                content.right() - label_width - 1,
                content.bottom() - 1,
                label_width,
                1,
            );
            let line = Line::from(Span::styled(label, theme.style(Role::Pill)));
            frame.render_widget(&line, rect);
            app.hits.add(rect, HitId::Button(Button::JumpToNewest));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::demo_rooms;
    use crate::theme::DEFAULT_THEME;

    #[test]
    fn rows_fit_and_group_by_sender() {
        let theme = Theme::builtin(DEFAULT_THEME).unwrap();
        let room = &demo_rooms()[0];
        for max in [10, 20, 37, 80] {
            let rows = build_rows(room, max, &theme);
            for row in &rows {
                assert!(row.line.width() <= max, "width {max}: {:?}", row.line);
            }
            // Two consecutive messages by bob share one header.
            let headers = rows
                .iter()
                .filter(|r| r.line.spans.first().is_some_and(|s| s.content == "● "))
                .count();
            assert_eq!(headers, room.messages.len() - 1);
        }
    }
}
