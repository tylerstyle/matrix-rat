//! The "Saved messages" popup: the list of what was saved for later, of all rooms, in
//! the manner of the pinned messages; and the form that sets a reminder.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::hit::{HitId, InputTarget};
use super::modal::{centered, field, popup, row};
use crate::app::App;
use crate::saved::{
    Preset, RemindForm, Reminder, RoomLabel, SavedItem, SavedPopup, SavedText, Stage, WHEN_FORMS,
    when_label,
};
use crate::text::{sanitize_line, truncate, width};
use crate::theme::{Role, Theme};

const REMIND: &str = " remind ";
const REMOVE: &str = " remove ";
/// Rows an entry takes: who and where, then the reminder and the text.
const ENTRY_ROWS: usize = 2;
const FORM_WIDTH: u16 = 60;

pub(super) fn draw(frame: &mut Frame, app: &mut App, area: Rect, state: &mut SavedPopup) {
    if matches!(state.stage, Stage::Remind(_)) {
        draw_form(frame, app, area, state);
    } else {
        draw_list(frame, app, area, state);
    }
}

fn draw_list(frame: &mut Frame, app: &mut App, area: Rect, state: &mut SavedPopup) {
    let w = area.width.saturating_sub(4).clamp(24, 72);
    let h = area.height.saturating_sub(2).clamp(8, 22);
    let Some(rect) = centered(area, w, h) else {
        return;
    };
    let inner = popup(
        frame,
        app,
        area,
        rect,
        "Saved messages",
        "Enter go to · r reminder · Del remove · Esc close",
    );
    let theme = app.theme.clone();
    if inner.width < 10 || inner.height < 2 {
        return;
    }
    let len = app.bookmarks.store.len();
    if len == 0 {
        state.page = 0;
        let key = app
            .keymap
            .label_for(crate::action::Action::SaveForLater)
            .map(|key| format!("Select a message and press {key} to save it for later."))
            .unwrap_or_else(|| "A message's menu saves it for later.".to_owned());
        for (n, text) in ["No saved messages".to_owned(), key].iter().enumerate() {
            frame.render_widget(
                Line::from(Span::styled(
                    truncate(text, inner.width as usize),
                    theme.style(Role::Muted),
                )),
                row(inner, n as u16),
            );
        }
        return;
    }
    let visible = (inner.height as usize / ENTRY_ROWS).max(1);
    state.page = visible;
    state.clamp(len, visible);
    let end = (state.scroll + visible).min(len);
    // Only what is on screen is copied out of the app.
    let shown: Vec<SavedItem> = app.bookmarks.store.items()[state.scroll..end].to_vec();
    for (offset, item) in shown.iter().enumerate() {
        let index = state.scroll + offset;
        let y = inner.y + (offset * ENTRY_ROWS) as u16;
        let at = Rect::new(
            inner.x,
            y,
            inner.width,
            (ENTRY_ROWS as u16).min(inner.bottom().saturating_sub(y)),
        );
        draw_entry(frame, app, &theme, at, state, (index, item));
    }
}

fn draw_entry(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    at: Rect,
    state: &SavedPopup,
    (index, item): (usize, &SavedItem),
) {
    let clock = app.bookmarks.clock;
    let (now, now_local) = (clock.now_secs(), clock.now_local());
    let selected = index == state.selected;
    let hovered = matches!(
        app.hover,
        Some(HitId::SavedRow(i) | HitId::SavedRemind(i) | HitId::SavedRemove(i)) if i == index
    );
    let base = if selected {
        theme.style(Role::Pill)
    } else if hovered {
        theme.style(Role::Hover)
    } else {
        theme.style(Role::Text)
    };
    frame.buffer_mut().set_style(at, base);
    app.hits.add(at, HitId::SavedRow(index));
    let details = state.details_of(&item.room_id, &item.event_id);

    // Where and who, when it was saved, and the two marks.
    let room = match app.saved_room_label(&item.room_id) {
        RoomLabel::Name(name) => sanitize_line(&name),
        RoomLabel::Unknown => "…".to_owned(),
        RoomLabel::Gone => "a room you left".to_owned(),
    };
    let who = match details.map(|found| sanitize_line(&found.sender)) {
        Some(sender) if !sender.is_empty() => format!("{room} · {sender}"),
        _ => room,
    };
    let saved = clock
        .local_of(item.saved_at)
        .map(|when| format!(" · saved {}", when_label(when, now_local)))
        .unwrap_or_default();
    let (remind_w, remove_w) = (width(REMIND) as u16, width(REMOVE) as u16);
    let marks = at.width > remind_w + remove_w + 14;
    let head_w = if marks {
        at.width - remind_w - remove_w - 2
    } else {
        at.width
    } as usize;
    // The name gives way to the time only so far.
    let saved = if head_w >= width(&saved) + 8 {
        saved
    } else {
        String::new()
    };
    frame.render_widget(
        Line::from(vec![
            Span::styled(
                truncate(&who, head_w - width(&saved)),
                base.patch(theme.style(Role::TitleFocused))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(saved, base.patch(theme.style(Role::Muted))),
        ]),
        Rect::new(at.x, at.y, head_w as u16, 1),
    );
    if marks {
        let remove = Rect::new(at.right() - remove_w, at.y, remove_w, 1);
        let remind = Rect::new(remove.x - 1 - remind_w, at.y, remind_w, 1);
        app.hits
            .add(Rect::new(remind.right(), at.y, 1, 1), HitId::SavedGap);
        for (rect, label, hit, role) in [
            (remind, REMIND, HitId::SavedRemind(index), Role::StatusKey),
            (remove, REMOVE, HitId::SavedRemove(index), Role::Error),
        ] {
            let style = if app.hover == Some(hit) {
                theme.style(Role::Pill).add_modifier(Modifier::BOLD)
            } else {
                theme.style(role)
            };
            frame.render_widget(Line::from(Span::styled(label, style)), rect);
            app.hits.add(rect, hit);
        }
    }
    if at.height < 2 {
        return;
    }

    // The reminder, and what the message says.
    let label = |when: i64| {
        clock
            .local_of(when)
            .map(|when| when_label(when, now_local))
            .unwrap_or_default()
    };
    let reminder = match item.reminder(now) {
        Reminder::None => None,
        Reminder::Waiting(when) => Some((format!("reminder {}", label(when)), Role::StatusKey)),
        Reminder::Due(when) => Some((format!("due since {}", label(when)), Role::Error)),
        Reminder::Done(when) => Some((format!("done ({})", label(when)), Role::Muted)),
    };
    let (text, plain) = match details {
        None => ("…".to_owned(), false),
        Some(found) => match found.text {
            SavedText::Text => (sanitize_line(&found.snippet), true),
            SavedText::Redacted => ("[message deleted]".to_owned(), false),
            SavedText::Undecryptable => ("[cannot be decrypted]".to_owned(), false),
            SavedText::Unavailable => ("[message could not be loaded]".to_owned(), false),
            SavedText::RoomGone => ("[not available: you left the room]".to_owned(), false),
        },
    };
    let mut spans = vec![Span::raw("  ")];
    let mut room_left = (at.width as usize).saturating_sub(2);
    if let Some((label, role)) = reminder {
        let label = truncate(&label, room_left);
        room_left = room_left.saturating_sub(width(&label) + 3);
        spans.push(Span::styled(label, base.patch(theme.style(role))));
        spans.push(Span::styled(" · ", base.patch(theme.style(Role::Muted))));
    }
    let style = if plain {
        base.patch(theme.style(Role::Text))
    } else {
        base.patch(theme.style(Role::Muted))
            .add_modifier(Modifier::ITALIC)
    };
    spans.push(Span::styled(truncate(&text, room_left), style));
    frame.render_widget(Line::from(spans), Rect::new(at.x, at.y + 1, at.width, 1));
}

fn draw_form(frame: &mut Frame, app: &mut App, area: Rect, state: &mut SavedPopup) {
    let back = if state.form_only {
        "Enter set · ↑↓ choose · Esc close"
    } else {
        "Enter set · ↑↓ choose · Esc back"
    };
    let Stage::Remind(form) = &mut state.stage else {
        return;
    };
    // The question, the choices, the hint under the typed time, the line for what
    // went wrong.
    let height = 2 + 1 + form.rows() + 1 + 1;
    let Some(rect) = centered(area, FORM_WIDTH, height as u16) else {
        return;
    };
    let inner = popup(frame, app, area, rect, "Remind me", back);
    let theme = app.theme.clone();
    if inner.width < 10 || inner.height < 3 {
        return;
    }
    let clock = app.bookmarks.clock;
    let now = clock.now_local();
    frame.render_widget(
        Line::from(Span::styled(
            "Remind me of this message:",
            theme.style(Role::Muted),
        )),
        row(inner, 0),
    );
    let mark = |selected: bool| {
        Span::styled(
            if selected { "› " } else { "  " },
            theme.style(Role::TitleFocused),
        )
    };
    let choice_style = |selected: bool, hovered: bool| {
        if selected {
            theme.style(Role::TitleFocused)
        } else if hovered {
            theme.style(Role::Hover)
        } else {
            theme.style(Role::Text)
        }
    };
    let mut y = 1u16;
    for (index, preset) in Preset::ALL.iter().enumerate() {
        let at = row(inner, y);
        let selected = form.selected == index;
        let hovered = app.hover == Some(HitId::ReminderChoice(index));
        let when = preset
            .when(now)
            .and_then(|when| clock.resolve(when).ok())
            .and_then(|secs| clock.local_of(secs))
            .map(|when| format!("  {}", when_label(when, now)))
            .unwrap_or_default();
        frame.render_widget(
            Line::from(vec![
                mark(selected),
                Span::styled(preset.label(), choice_style(selected, hovered)),
                Span::styled(when, theme.style(Role::Muted)),
            ]),
            at,
        );
        app.hits.add(at, HitId::ReminderChoice(index));
        y += 1;
    }

    let typed = form.selected == RemindForm::TYPED;
    let at = row(inner, y);
    frame.render_widget(Line::from(mark(typed)), at);
    app.hits.add(at, HitId::ReminderChoice(RemindForm::TYPED));
    let (cursor, input) = field(
        frame,
        &theme,
        Rect::new(at.x + 2, at.y, at.width.saturating_sub(2), at.height),
        "At",
        &mut form.input,
        false,
        typed,
    );
    app.hits.add(input, HitId::Input(InputTarget::Reminder));
    y += 1;
    frame.render_widget(
        Line::from(Span::styled(
            truncate(&format!("  e.g. {WHEN_FORMS}"), inner.width as usize),
            theme.style(Role::Muted),
        )),
        row(inner, y),
    );
    y += 1;

    if form.has_reminder {
        let at = row(inner, y);
        let selected = form.selected == RemindForm::CLEAR;
        let hovered = app.hover == Some(HitId::ReminderChoice(RemindForm::CLEAR));
        frame.render_widget(
            Line::from(vec![
                mark(selected),
                Span::styled("no reminder", choice_style(selected, hovered)),
            ]),
            at,
        );
        app.hits.add(at, HitId::ReminderChoice(RemindForm::CLEAR));
        y += 1;
    }
    if let Some(error) = &form.error {
        frame.render_widget(
            Line::from(Span::styled(
                truncate(&sanitize_line(error), inner.width as usize),
                theme.style(Role::Error),
            )),
            row(inner, y),
        );
    }
    if let Some(cursor) = cursor.filter(|_| typed) {
        frame.set_cursor_position(cursor);
    }
}
