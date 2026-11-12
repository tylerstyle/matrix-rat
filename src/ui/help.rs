use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use super::hit::HitId;
use crate::app::App;
use crate::keymap::{Context, DEFAULT};
use crate::theme::{Role, Theme};

const MOUSE_HINT: &str =
    "Mouse: click to focus · wheel scrolls · drag the room list's edge to resize";

fn section(theme: &Theme, contexts: &[Context]) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    for context in contexts {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(
            context.title(),
            theme.style(Role::SidebarSection),
        )));
        for binding in DEFAULT.iter().filter(|b| b.context == *context) {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {:<12}", binding.label),
                    theme.style(Role::StatusKey),
                ),
                Span::raw(binding.description),
            ]));
        }
    }
    lines
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = &app.theme;
    let left = section(theme, &[Context::Global]);
    let right = section(
        theme,
        &[Context::Sidebar, Context::Timeline, Context::Composer],
    );

    // Two columns when there is room, otherwise one long one.
    let two_columns = area.width >= 100;
    let (width, rows) = if two_columns {
        (96, left.len().max(right.len()))
    } else {
        (
            56.min(area.width.saturating_sub(2)),
            left.len() + 1 + right.len(),
        )
    };
    let height = (rows as u16 + 4).min(area.height.saturating_sub(1));
    if width < 4 || height < 3 {
        return;
    }
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    app.hits.add(area, HitId::Backdrop);
    app.hits.add(popup, HitId::Overlay);

    // Dim everything behind the overlay.
    frame.buffer_mut().set_style(
        area,
        ratatui::style::Style::new().add_modifier(ratatui::style::Modifier::DIM),
    );
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.style(Role::OverlayBorder))
        .style(theme.style(Role::Text))
        .title(Line::from(Span::styled(
            " Keys ",
            theme.style(Role::TitleFocused),
        )))
        .title_bottom(
            Line::from(Span::styled(" Esc to close ", theme.style(Role::Muted))).right_aligned(),
        );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let inner = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    let [keys, hint] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
    if two_columns {
        let [a, b] = Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)]).areas(keys);
        frame.render_widget(Paragraph::new(left), a);
        frame.render_widget(Paragraph::new(right), b);
    } else {
        let mut lines = left;
        lines.push(Line::default());
        lines.extend(right);
        frame.render_widget(Paragraph::new(lines), keys);
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            MOUSE_HINT,
            theme.style(Role::Muted),
        ))),
        hint,
    );
}
