//! Composer completion popup rendered directly above the cursor.

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear};

use super::hit::HitId;
use crate::app::App;
use crate::complete::{Completion, Kind};
use crate::text::{truncate, width};
use crate::theme::Role;

const MAX_SHOWN: usize = 8;
const MIN_WIDTH: u16 = 24;
const MAX_WIDTH: u16 = 60;

pub fn draw(frame: &mut Frame, app: &mut App, chat_area: Rect, composer_top: u16) {
    if app.focus != crate::action::Pane::Composer
        || app.help_open
        || app.modal.is_some()
        || app.images.viewer_open()
    {
        return;
    }
    let Some((cx, cy)) = app.composer_view.cursor else {
        return;
    };
    let Some(completion) = app.complete.active.clone() else {
        return;
    };
    if completion.items.is_empty() {
        return;
    }

    let bounds = if chat_area.width >= MIN_WIDTH {
        chat_area
    } else {
        frame.area()
    };
    // Sit right above the composer's top border when typing on the first row so the
    // `"3 Message"` frame stays intact; otherwise sit right above the cursor row.
    let anchor_y = if cy <= composer_top.saturating_add(1) && composer_top >= bounds.y + 3 {
        composer_top
    } else {
        cy
    };
    let avail_top = anchor_y.saturating_sub(bounds.y);
    if bounds.width < 12 || avail_top < 3 {
        return;
    }

    let shown_rows = completion
        .items
        .len()
        .min(MAX_SHOWN)
        .min((avail_top - 2) as usize);
    if shown_rows == 0 {
        return;
    }

    let rect = popup_rect(bounds, cx, anchor_y, shown_rows, &completion);
    frame.render_widget(Clear, rect);

    let title = match completion.kind {
        Kind::Mention => " Members ",
        Kind::Emoji => " Emoji ",
        Kind::Room => " Rooms ",
        Kind::Command => " Commands ",
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(app.theme.style(Role::OverlayBorder))
        .title(Span::styled(title, app.theme.style(Role::TitleFocused)))
        .style(app.theme.style(Role::Text));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    app.hits.add(rect, HitId::Overlay);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let list_h = inner.height as usize;
    let selected = completion.selected.min(completion.items.len() - 1);
    let scroll = if selected < list_h {
        0
    } else {
        (selected + 1 - list_h).min(completion.items.len() - list_h)
    };

    for (row_offset, idx) in (scroll..completion.items.len().min(scroll + list_h)).enumerate() {
        let row_rect = Rect::new(inner.x, inner.y + row_offset as u16, inner.width, 1);
        let item = &completion.items[idx];
        let is_selected = idx == selected;
        let is_hovered = app.hover == Some(HitId::Complete(idx));

        let row_style = if is_selected {
            app.theme.style(Role::MessageSelected)
        } else if is_hovered {
            app.theme.style(Role::Hover)
        } else {
            app.theme.style(Role::Text)
        };
        let marker = if is_selected { "› " } else { "  " };
        let marker_style = if is_selected {
            app.theme.style(Role::TitleFocused).patch(row_style)
        } else {
            row_style
        };
        let secondary_style = if is_selected {
            row_style
        } else {
            app.theme.style(Role::Muted).patch(row_style)
        };

        let line = format_row(
            marker,
            marker_style,
            &item.primary,
            &item.secondary,
            row_style,
            secondary_style,
            inner.width as usize,
        );
        frame.render_widget(line, row_rect);
        app.hits.add(row_rect, HitId::Complete(idx));
    }

    // Keep hardware cursor at the composer text cursor rather than stealing it.
    frame.set_cursor_position(Position::new(cx, cy));
}

fn popup_rect(bounds: Rect, cx: u16, cy: u16, shown_rows: usize, completion: &Completion) -> Rect {
    let desired_w = completion
        .items
        .iter()
        .map(|item| {
            let gap = if item.secondary.is_empty() { 0 } else { 2 };
            width(&item.primary) + gap + width(&item.secondary) + 4
        })
        .max()
        .unwrap_or(MIN_WIDTH as usize) as u16;
    let width = desired_w.clamp(MIN_WIDTH, MAX_WIDTH).min(bounds.width);
    let height = (shown_rows as u16) + 2;
    let x = cx
        .saturating_sub(2)
        .max(bounds.x)
        .min(bounds.right().saturating_sub(width));
    let y = cy.saturating_sub(height).max(bounds.y);
    Rect::new(x, y, width, height)
}

fn format_row(
    marker: &'static str,
    marker_style: Style,
    primary: &str,
    secondary: &str,
    primary_style: Style,
    secondary_style: Style,
    total_width: usize,
) -> Line<'static> {
    let body_w = total_width.saturating_sub(width(marker));
    let (shown_primary, shown_secondary) = if secondary.is_empty() || body_w < 8 {
        (truncate(primary, body_w), String::new())
    } else if width(primary) + 2 + width(secondary) <= body_w {
        (primary.to_owned(), secondary.to_owned())
    } else {
        let max_p = (body_w / 2)
            .max(10)
            .min(width(primary))
            .min(body_w.saturating_sub(5));
        let p = truncate(primary, max_p);
        let rem = body_w.saturating_sub(width(&p) + 2);
        let s = if rem >= 3 {
            truncate(secondary, rem)
        } else {
            String::new()
        };
        (p, s)
    };

    let used = width(marker) + width(&shown_primary) + width(&shown_secondary);
    let gap = total_width.saturating_sub(used);

    let mut spans = vec![
        Span::styled(marker, marker_style),
        Span::styled(shown_primary, primary_style),
        Span::styled(" ".repeat(gap), primary_style),
    ];
    if !shown_secondary.is_empty() {
        spans.push(Span::styled(shown_secondary, secondary_style));
    }
    Line::from(spans)
}
