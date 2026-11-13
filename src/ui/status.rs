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

    // Right side: essential clickable buttons. Palette is placed only into leftover slack
    // below so it never crowds toasts, security alerts, or context hints.
    let buttons = [
        (Button::Help, "F1".to_owned(), "help".to_owned()),
        (Button::Settings, "F2".to_owned(), "settings".to_owned()),
        (Button::Theme, "F3".to_owned(), app.theme.name.clone()),
    ];
    let mut right = area.right();
    for (button, label, text) in buttons.iter().rev() {
        let w = button_width(label, text);
        if right < area.x + w + 20 {
            break;
        }
        right -= w;
        draw_button(
            frame,
            &app.theme,
            app.hover,
            &mut app.hits,
            Rect::new(right, area.y, w, 1),
            (*button, label, text),
        );
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
    super::verify::status_hint(
        &app.verify,
        &app.modal,
        theme,
        app.hover,
        &mut app.hits,
        Rect::new(area.x, area.y, right - area.x, 1),
        &mut spans,
    );
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
                ("Alt-A", "attach file", Some(A::Attach)),
            ],
            Pane::Timeline if app.selected_failed() => vec![
                ("Enter", "try again", Some(A::Reply)),
                ("Del", "discard", Some(A::Delete)),
                ("Esc", "back", Some(A::Back)),
            ],
            Pane::Timeline => vec![
                ("v", "view", Some(A::Open)),
                ("Enter", "reply", Some(A::Reply)),
                ("t", "thread", Some(A::OpenThread)),
                ("e", "edit", Some(A::Edit)),
                ("r", "react", Some(A::React)),
                ("Del", "delete", Some(A::Delete)),
                ("c", "copy", Some(A::Copy)),
                ("s", "save", Some(A::Save)),
                ("Esc", "back", Some(A::Back)),
            ],
            Pane::Sidebar => vec![
                ("↑↓", "move", None),
                ("Enter", "open", Some(A::SidebarOpen)),
                ("type", "to filter", None),
            ],
        });
        if app.thread_open() {
            // Inside a thread: no thread of a thread, and Esc leads back to the room.
            hints.retain(|(_, text, _)| *text != "thread");
            if app.focus == Pane::Composer {
                hints.insert(0, ("Esc", "back to room", Some(A::CloseThread)));
            }
        }
        // Saving is only offered where there is something to save.
        let can_save = app.can_save();
        hints.retain(|(_, text, _)| *text != "save" || can_save);
        let picture = app
            .timeline
            .selected
            .and_then(|i| app.room()?.messages.get(i))
            .is_some_and(|message| message.image.is_some());
        hints.retain(|(_, text, _)| *text != "view" || picture);
        let mut col = area.x + spans.iter().map(|s| width(&s.content) as u16).sum::<u16>();
        let mut drew_hint = false;
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
            drew_hint = true;
            spans.push(Span::styled(k.to_owned(), key));
            spans.push(Span::styled(label, status));
        }

        // Fit the command palette button into unused space; the button starts with a space,
        // so subtracting the trailing 2-col hint gap still leaves at least one blank column.
        let used = col.saturating_sub(if drew_hint { 2 } else { 0 });
        let palette_w = button_width("Ctrl-P", "palette");
        if used + palette_w <= right {
            right -= palette_w;
            draw_button(
                frame,
                theme,
                app.hover,
                &mut app.hits,
                Rect::new(right, area.y, palette_w, 1),
                (Button::Palette, "Ctrl-P", "palette"),
            );
        }
    }
    let left = Rect::new(area.x, area.y, right - area.x, 1);
    frame.render_widget(Line::from(spans), left);
}

fn button_width(label: &str, text: &str) -> u16 {
    (width(label) + width(text) + 3) as u16
}

fn draw_button(
    frame: &mut Frame,
    theme: &crate::theme::Theme,
    hover: Option<HitId>,
    hits: &mut super::hit::HitMap,
    rect: Rect,
    (button, label, text): (Button, &str, &str),
) {
    let line = Line::from(vec![
        Span::styled(format!(" {label}"), theme.style(Role::StatusKey)),
        Span::styled(format!(" {text} "), theme.style(Role::Status)),
    ]);
    frame.render_widget(&line, rect);
    if hover == Some(HitId::Button(button)) {
        frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
    }
    hits.add(rect, HitId::Button(button));
}
