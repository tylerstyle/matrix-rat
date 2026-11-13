use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};

use super::hit::{Button, HitId};
use crate::action::{Action, Pane};
use crate::app::{App, ComposeMode};
use crate::text::truncate;
use crate::theme::Role;

const MAX_TEXT_ROWS: usize = 6;
/// Columns kept free at the right for the attach and emoji buttons.
const EMOJI_BUTTON: u16 = 6;

fn text_width(pane_width: u16) -> usize {
    // Borders, one column of padding on each side, and the emoji button.
    pane_width.saturating_sub(4 + EMOJI_BUTTON).max(1) as usize
}

/// Height of the composer pane for the current text.
pub fn height(app: &App, pane_width: u16) -> u16 {
    let rows = app.composer.layout(text_width(pane_width)).len();
    let banner = u16::from(app.compose_mode != ComposeMode::Normal);
    let files = super::attach::rows(app.attachments().len());
    rows.clamp(1, MAX_TEXT_ROWS) as u16 + 2 + banner + files
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = super::pane_block(app, Pane::Composer, "Message");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.hits.add(area, HitId::Pane(Pane::Composer));
    if inner.width < 3 || inner.height == 0 {
        return;
    }
    let theme = &app.theme;
    let mut text_area = Rect::new(inner.x + 1, inner.y, inner.width - 2, inner.height);

    // Reply / edit banner.
    let banner = match app.compose_mode {
        ComposeMode::Normal => None,
        ComposeMode::Reply(i) => app
            .room()
            .and_then(|r| r.messages.get(i))
            .map(|m| format!("↩ Replying to {}: {}", m.sender, m.body.replace('\n', " "))),
        ComposeMode::Edit(_) => Some("✎ Editing your message".to_owned()),
    };
    if let Some(banner) = banner {
        let rect = Rect::new(text_area.x, text_area.y, text_area.width, 1);
        let close = " ✕ ";
        let label = truncate(&banner, (rect.width as usize).saturating_sub(4));
        frame.render_widget(
            Line::from(Span::styled(label, theme.style(Role::Banner))),
            rect,
        );
        if rect.width > 4 {
            let close_rect = Rect::new(rect.right() - 3, rect.y, 3, 1);
            frame.render_widget(
                Line::from(Span::styled(close, theme.style(Role::Muted))),
                close_rect,
            );
            app.hits
                .add(close_rect, HitId::Button(Button::CancelCompose));
        }
        text_area.y += 1;
        text_area.height = text_area.height.saturating_sub(1);
    }
    // Files waiting to be sent, one line each; the text keeps at least one row.
    let room_id = app.room().map(|room| room.id.as_str()).unwrap_or_default();
    let files = app.attach.files(room_id);
    let file_rows = super::attach::rows(files.len()).min(text_area.height.saturating_sub(1));
    if file_rows > 0 {
        let rect = Rect::new(text_area.x, text_area.y, text_area.width, file_rows);
        super::attach::draw(frame, files, theme, app.hover, &mut app.hits, rect);
        text_area.y += file_rows;
        text_area.height -= file_rows;
    }
    let has_files = !files.is_empty();
    if text_area.height == 0 {
        return;
    }

    // Attach and emoji buttons at the right end of the first text row.
    if text_area.width > EMOJI_BUTTON + 4 {
        text_area.width -= EMOJI_BUTTON;
        for (offset, symbol, action) in [(1, "📎", Action::Attach), (4, "☺", Action::OpenEmoji)]
        {
            let button = Rect::new(text_area.right() + offset, text_area.y, 2, 1);
            let role = if app.hover == Some(HitId::Do(action)) {
                Role::TitleFocused
            } else {
                Role::Muted
            };
            frame.render_widget(Line::from(Span::styled(symbol, theme.style(role))), button);
            app.hits.add(button, HitId::Do(action));
        }
    }

    let max = text_area.width as usize;
    let lines = app.composer.layout(max);
    let (cursor_row, cursor_col) = app.composer.cursor_position(&lines);
    let visible = text_area.height as usize;
    // Scroll the text so the cursor row stays visible.
    let first = if cursor_row >= visible {
        cursor_row + 1 - visible
    } else {
        0
    };
    app.composer_view = crate::app::ComposerView {
        area: text_area,
        width: max,
        first_row: first,
    };

    let focused = app.focus == Pane::Composer;
    if app.composer.is_empty() {
        let hint = if has_files && focused {
            "Add a caption, or press Enter to send"
        } else if focused {
            "Type a message…"
        } else {
            "Click here or press Alt-3 to write"
        };
        frame.render_widget(
            Line::from(Span::styled(hint, theme.style(Role::Placeholder))),
            text_area,
        );
    } else {
        // Dimmed whenever typing would not land here.
        let style = if !focused {
            theme.style(Role::Muted)
        } else {
            ratatui::style::Style::new()
        };
        for (n, line) in lines.iter().skip(first).take(visible).enumerate() {
            let rect = Rect::new(text_area.x, text_area.y + n as u16, text_area.width, 1);
            let text = app.composer.text()[line.range.clone()].to_owned();
            frame.render_widget(Line::from(Span::styled(text, style)), rect);
        }
    }
    app.hits.add(text_area, HitId::ComposerText);

    if focused && !app.help_open && app.modal.is_none() {
        let x = text_area.x + (cursor_col as u16).min(text_area.width.saturating_sub(1));
        let y = text_area.y + (cursor_row - first) as u16;
        frame.set_cursor_position(Position::new(x, y));
    }
}
