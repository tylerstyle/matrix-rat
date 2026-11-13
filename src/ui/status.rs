use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::NARROW_WIDTH;
use super::hit::{Button, HitId};
use crate::action::{Action, Pane};
use crate::app::App;
use crate::matrix::SyncStatus;
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
        (Button::Settings, "F2".to_owned(), "settings".to_owned()),
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
    let (dot, dot_role, text) = match &app.sync {
        None => ("●", Role::StatusOk, "demo data".to_owned()),
        Some(SyncStatus::Synced) => ("●", Role::StatusOk, "synced".to_owned()),
        Some(SyncStatus::Connecting) => ("◌", Role::Muted, "connecting…".to_owned()),
        Some(SyncStatus::Offline) => ("○", Role::Error, "offline".to_owned()),
        Some(SyncStatus::Stopped) => ("○", Role::Muted, "stopped".to_owned()),
        Some(SyncStatus::Error(_)) => ("✕", Role::Error, "sync error".to_owned()),
    };
    let mut spans = vec![
        Span::styled(format!(" {dot} "), theme.style(dot_role)),
        Span::styled(format!("{text}  "), status),
    ];
    if let Some((text, _)) = &app.toast {
        spans.push(Span::styled(format!(" {text} "), theme.style(Role::Toast)));
    } else {
        use Action as A;
        let mut hints: Vec<(&str, &str, Option<Action>)> = Vec::new();
        if app.screen.width < NARROW_WIDTH && app.focus != Pane::Sidebar {
            // The room list is hidden at this width; this is the mouse's way to it.
            hints.push(("☰", "rooms", Some(A::Focus(Pane::Sidebar))));
        }
        hints.extend(match app.focus {
            Pane::Composer => vec![
                ("Enter", "send", Some(A::Send)),
                ("Shift-Enter", "line break", Some(A::Newline)),
                ("↑", "select message", Some(A::SelectOlder)),
            ],
            Pane::Timeline if app.selected_failed() => vec![
                ("Enter", "try again", Some(A::Reply)),
                ("Del", "discard", Some(A::Delete)),
                ("Esc", "back", Some(A::Back)),
            ],
            Pane::Timeline => vec![
                ("Enter", "reply", Some(A::Reply)),
                ("e", "edit", Some(A::Edit)),
                ("r", "react", Some(A::React)),
                ("Del", "delete", Some(A::Delete)),
                ("c", "copy", Some(A::Copy)),
                ("Esc", "back", Some(A::Back)),
            ],
            Pane::Sidebar => vec![
                ("↑↓", "move", None),
                ("Enter", "open", Some(A::SidebarOpen)),
                ("type", "to filter", None),
            ],
        });
        let mut col = area.x + spans.iter().map(|s| width(&s.content) as u16).sum::<u16>();
        for (k, text, action) in hints {
            let label = format!(" {text}  ");
            let hint_width = (width(k) + width(&label)) as u16;
            // A hint that does not fit whole is left out rather than cut off.
            if col + hint_width.saturating_sub(2) > right {
                break;
            }
            if let Some(action) = action {
                // Clickable area: the key and its label, without the trailing gap.
                let end = (col + hint_width.saturating_sub(2)).min(right);
                if end > col {
                    app.hits
                        .add(Rect::new(col, area.y, end - col, 1), HitId::Do(action));
                }
            }
            col += hint_width;
            spans.push(Span::styled(k.to_owned(), key));
            spans.push(Span::styled(label, status));
        }
    }
    let left = Rect::new(area.x, area.y, right - area.x, 1);
    frame.render_widget(Line::from(spans), left);
}
