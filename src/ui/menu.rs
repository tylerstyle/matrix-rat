//! Drawing the context menu: a small box next to the pointer. Nothing is dimmed, and
//! what lies around it stays as it is.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear};

use super::hit::HitId;
use crate::app::App;
use crate::menu::Menu;
use crate::text::{truncate, width};
use crate::theme::Role;

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect, menu: &Menu) {
    let Some((rect, first)) = menu.place(area) else {
        return;
    };
    let theme = &app.theme;
    frame.render_widget(Clear, rect);
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.style(Role::OverlayBorder))
        .style(theme.style(Role::Text));
    let title = truncate(&menu.title, (rect.width as usize).saturating_sub(4));
    if width(&title) > 1 {
        block = block.title(Line::from(Span::styled(
            format!(" {title} "),
            theme.style(Role::TitleFocused),
        )));
    }
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    // Clicks on the border are not clicks on what is underneath.
    app.hits.add(rect, HitId::Overlay);

    let room = (inner.width as usize).saturating_sub(2);
    let shown = menu.items.iter().enumerate().skip(first);
    for (n, (index, item)) in shown.take(inner.height as usize).enumerate() {
        let row = Rect::new(inner.x, inner.y + n as u16, inner.width, 1);
        let selected = index == menu.selected;
        if selected {
            frame
                .buffer_mut()
                .set_style(row, theme.style(Role::MessageSelected));
        }
        // The key gives way to the label where both do not fit.
        let armed = menu.armed == Some(index);
        let shown = menu.label(index);
        let hint = if armed {
            ""
        } else if width(shown) + 2 + width(&item.hint) <= room {
            item.hint.as_str()
        } else {
            ""
        };
        let label = truncate(shown, room.saturating_sub(width(hint)));
        let gap = room.saturating_sub(width(&label) + width(hint));
        let label_role = match (item.run.is_some(), selected) {
            _ if armed => Role::Error,
            (true, true) => Role::TitleFocused,
            _ => Role::Text,
        };
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                label,
                ratatui::style::Style {
                    fg: theme.style(label_role).fg,
                    ..ratatui::style::Style::new()
                },
            ),
            Span::raw(" ".repeat(gap)),
            Span::styled(
                hint.to_owned(),
                ratatui::style::Style {
                    fg: theme.style(Role::Muted).fg,
                    ..ratatui::style::Style::new()
                },
            ),
        ]);
        frame.render_widget(line, row);
        app.hits.add(row, HitId::Menu(index));
    }
}
