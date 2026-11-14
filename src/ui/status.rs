use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::NARROW_WIDTH;
use super::hit::{Button, HitId};
use crate::action::{Action, Pane};
use crate::app::App;
use crate::keymap::Context;
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

    let key_or_empty = |action| app.keymap.label_for(action).unwrap_or("").to_owned();

    // Right side: essential clickable buttons. Palette is placed only into leftover slack
    // below so it never crowds toasts, security alerts, or context hints.
    let buttons = [
        (
            Button::Help,
            key_or_empty(Action::ToggleHelp),
            "help".to_owned(),
        ),
        (
            Button::Settings,
            key_or_empty(Action::OpenSettings),
            "settings".to_owned(),
        ),
        (
            Button::Theme,
            key_or_empty(Action::CycleTheme),
            app.theme.name.clone(),
        ),
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
        let mut hints: Vec<(String, &str, Option<Action>)> = Vec::new();
        if app.screen.width < NARROW_WIDTH && app.focus != Pane::Sidebar {
            // The room list is hidden at this width; this is the mouse's way to it.
            hints.push(("☰".to_owned(), "rooms", Some(A::Focus(Pane::Sidebar))));
        }
        let hint_key = |ctx: Context, action: A| -> Option<String> {
            app.keymap
                .label_in(ctx, action)
                .or_else(|| app.keymap.label_in(Context::Global, action))
                .map(str::to_owned)
        };
        let push_action = |list: &mut Vec<(String, &'static str, Option<Action>)>,
                           ctx: Context,
                           action: A,
                           text: &'static str| {
            if let Some(k) = hint_key(ctx, action) {
                list.push((k, text, Some(action)));
            }
        };
        match app.focus {
            Pane::Composer => {
                push_action(&mut hints, Context::Composer, A::Send, "send");
                push_action(&mut hints, Context::Composer, A::Newline, "line break");
                // One key does both: which, depends on what is in the composer.
                let up = if app.last_editable().is_some() {
                    "edit last message"
                } else {
                    "select message"
                };
                push_action(&mut hints, Context::Composer, A::EditLast, up);
                // Only if it was given a key of its own here.
                push_action(
                    &mut hints,
                    Context::Composer,
                    A::SelectOlder,
                    "select message",
                );
                push_action(&mut hints, Context::Composer, A::Attach, "attach file");
            }
            Pane::Timeline if app.selected_failed() => {
                push_action(&mut hints, Context::Timeline, A::Reply, "try again");
                push_action(&mut hints, Context::Timeline, A::Delete, "discard");
                push_action(&mut hints, Context::Timeline, A::Back, "back");
            }
            Pane::Timeline => {
                push_action(&mut hints, Context::Timeline, A::Open, "view");
                push_action(&mut hints, Context::Timeline, A::Reply, "reply");
                push_action(&mut hints, Context::Timeline, A::OpenThread, "thread");
                push_action(&mut hints, Context::Timeline, A::Edit, "edit");
                push_action(&mut hints, Context::Timeline, A::React, "react");
                push_action(&mut hints, Context::Timeline, A::Delete, "delete");
                push_action(&mut hints, Context::Timeline, A::Copy, "copy");
                push_action(&mut hints, Context::Timeline, A::Save, "save");
                push_action(&mut hints, Context::Timeline, A::Back, "back");
            }
            Pane::Sidebar => {
                let move_key = match (
                    hint_key(Context::Sidebar, A::SidebarUp),
                    hint_key(Context::Sidebar, A::SidebarDown),
                ) {
                    (Some(u), Some(d)) => format!("{u}{d}"),
                    (Some(u), None) => u,
                    (None, Some(d)) => d,
                    (None, None) => String::new(),
                };
                if !move_key.is_empty() {
                    hints.push((move_key, "move", None));
                }
                push_action(&mut hints, Context::Sidebar, A::SidebarOpen, "open");
                hints.push(("type".to_owned(), "to filter", None));
            }
        }
        if app.thread_open() {
            // Inside a thread: no thread of a thread, and closing returns to the room.
            hints.retain(|(_, text, _)| *text != "thread");
            if app.focus == Pane::Composer
                && let Some(k) = hint_key(Context::Composer, A::CloseThread)
                    .or_else(|| hint_key(Context::Composer, A::Back))
                    .or_else(|| app.keymap.label_for(A::CloseThread).map(str::to_owned))
            {
                hints.insert(0, (k, "back to room", Some(A::CloseThread)));
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
            let hint_width = (width(&k) + width(&label)) as u16;
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
            spans.push(Span::styled(k, key));
            spans.push(Span::styled(label, status));
        }

        // Fit the command palette button into unused space; the button starts with a space,
        // so subtracting the trailing 2-col hint gap still leaves at least one blank column.
        let used = col.saturating_sub(if drew_hint { 2 } else { 0 });
        let palette_key = app.keymap.label_for(Action::OpenPalette).unwrap_or("");
        let palette_w = button_width(palette_key, "palette");
        if used + palette_w <= right {
            right -= palette_w;
            draw_button(
                frame,
                theme,
                app.hover,
                &mut app.hits,
                Rect::new(right, area.y, palette_w, 1),
                (Button::Palette, palette_key, "palette"),
            );
        }
    }
    let left = Rect::new(area.x, area.y, right - area.x, 1);
    frame.render_widget(Line::from(spans), left);
}

fn button_width(label: &str, text: &str) -> u16 {
    let key_cols = if label.is_empty() {
        0
    } else {
        width(label) + 1
    };
    (key_cols + width(text) + 2) as u16
}

fn draw_button(
    frame: &mut Frame,
    theme: &crate::theme::Theme,
    hover: Option<HitId>,
    hits: &mut super::hit::HitMap,
    rect: Rect,
    (button, label, text): (Button, &str, &str),
) {
    let mut spans = Vec::new();
    if !label.is_empty() {
        spans.push(Span::styled(
            format!(" {label}"),
            theme.style(Role::StatusKey),
        ));
    }
    spans.push(Span::styled(format!(" {text} "), theme.style(Role::Status)));
    let line = Line::from(spans);
    frame.render_widget(&line, rect);
    if hover == Some(HitId::Button(button)) {
        frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
    }
    hits.add(rect, HitId::Button(button));
}
