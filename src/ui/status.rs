use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::hit::{Button, HitId};
use crate::action::Pane;
use crate::app::App;
use crate::text::width;
use crate::theme::Role;

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let theme = &app.theme;
    let status = theme.style(Role::Status);
    let key = theme.style(Role::StatusKey);

    // Right side: clickable buttons.
    let buttons = [
        (Button::Help, "F1".to_owned(), "help".to_owned()),
        (Button::Theme, "F3".to_owned(), app.theme.name.clone()),
    ];
    let mut right = area.right();
    for (button, label, text) in buttons.iter().rev() {
        let w = (width(label) + width(text) + 3) as u16;
        if right < area.x + w + 20 {
            break;
        }
        right -= w;
        let rect = Rect::new(right, area.y, w, 1);
        let line = Line::from(vec![
            Span::styled(format!(" {label}"), key),
            Span::styled(format!(" {text} "), status),
        ]);
        frame.render_widget(&line, rect);
        if app.hover == Some(HitId::Button(*button)) {
            frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
        }
        app.hits.add(rect, HitId::Button(*button));
    }

    // Left side: connection state, then a toast, or hints for the focused pane.
    let mut spans = vec![
        Span::styled(" ● ", theme.style(Role::StatusOk)),
        Span::styled("demo data  ", status),
    ];
    if let Some((text, _)) = &app.toast {
        spans.push(Span::styled(format!(" {text} "), theme.style(Role::Toast)));
    } else {
        let hints: &[(&str, &str)] = match app.focus {
            Pane::Composer => &[
                ("Enter", "send"),
                ("Shift-Enter", "line break"),
                ("↑", "select message"),
            ],
            Pane::Timeline => &[
                ("Enter", "reply"),
                ("e", "edit"),
                ("r", "react"),
                ("Del", "delete"),
                ("c", "copy"),
                ("Esc", "back"),
            ],
            Pane::Sidebar => &[("↑↓", "move"), ("Enter", "open"), ("type", "to filter")],
        };
        for (k, text) in hints {
            spans.push(Span::styled((*k).to_owned(), key));
            spans.push(Span::styled(format!(" {text}  "), status));
        }
    }
    let left = Rect::new(area.x, area.y, right - area.x, 1);
    frame.render_widget(Line::from(spans), left);
}
