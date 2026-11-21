//! Poll creation/editing form and voting chooser popups.

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::hit::{HitId, InputTarget};
use super::modal::{FIELD_LABEL, button, centered, field, popup, row};
use crate::app::App;
use crate::modal::{
    MAX_POLL_ANSWERS, MIN_POLL_ANSWERS, PollFormPopup, PollFormSlot, PollVotePopup,
};
use crate::text::{truncate, width, wrap};
use crate::theme::Role;

/// Draws the create/edit poll modal (`PollFormPopup`).
pub(super) fn draw_form(frame: &mut Frame, app: &mut App, area: Rect, form: &mut PollFormPopup) {
    let slots = form.slots();
    // Question + Answers + AddAnswer? + Kind + MaxSelections form the scrollable body;
    // Submit/Cancel buttons and the error/hint line stay pinned below.
    let body_slots_len = slots.len().saturating_sub(2);
    let w = area.width.saturating_sub(4).clamp(32, 66);
    let desired_h = (body_slots_len as u16 + 5).max(11);
    let h = desired_h.min(area.height.saturating_sub(2)).max(9);
    let Some(rect) = centered(area, w, h) else {
        return;
    };
    let title = if form.editing.is_some() {
        " Edit poll "
    } else {
        " Start a poll "
    };
    let inner = popup(
        frame,
        app,
        area,
        rect,
        title,
        "Tab move · Enter activate/submit · Esc cancel",
    );
    if inner.width < 16 || inner.height < 5 {
        return;
    }

    let body_h = inner.height.saturating_sub(3) as usize;
    let focused_body = form.focus.min(body_slots_len);
    if focused_body < body_slots_len {
        if focused_body < form.scroll {
            form.scroll = focused_body;
        } else if body_h > 0 && focused_body >= form.scroll + body_h {
            form.scroll = focused_body + 1 - body_h;
        }
    }
    let max_scroll = body_slots_len.saturating_sub(body_h);
    form.scroll = form.scroll.min(max_scroll);

    let theme = &app.theme;
    let mut cursor: Option<Position> = None;
    let can_remove = form.answers.len() > MIN_POLL_ANSWERS;

    for row_idx in 0..body_h {
        let slot_idx = form.scroll + row_idx;
        let Some(&slot) = slots.get(slot_idx) else {
            break;
        };
        if matches!(slot, PollFormSlot::Submit | PollFormSlot::Cancel) {
            break;
        }
        let r = row(inner, row_idx as u16);
        let focused = form.focus == slot_idx;

        match slot {
            PollFormSlot::Question => {
                app.hits.add(r, HitId::PollFormSlot(slot_idx));
                let (pos, input_rect) = field(
                    frame,
                    theme,
                    r,
                    "Question",
                    &mut form.question,
                    false,
                    focused,
                );
                app.hits
                    .add(input_rect, HitId::Input(InputTarget::PollQuestion));
                if pos.is_some() {
                    cursor = pos;
                }
            }
            PollFormSlot::Answer(i) => {
                let label = format!("Answer {}", i + 1);
                let field_w = if can_remove {
                    r.width.saturating_sub(4)
                } else {
                    r.width
                };
                let field_rect = Rect::new(r.x, r.y, field_w, 1);
                app.hits.add(field_rect, HitId::PollFormSlot(slot_idx));
                if let Some(edit) = form.answers.get_mut(i) {
                    let (pos, input_rect) =
                        field(frame, theme, field_rect, &label, edit, false, focused);
                    app.hits
                        .add(input_rect, HitId::Input(InputTarget::PollAnswer(i)));
                    if pos.is_some() {
                        cursor = pos;
                    }
                }
                if can_remove && r.width > field_w + 3 {
                    let rm_rect = Rect::new(r.x + field_w + 1, r.y, 3, 1);
                    frame.render_widget(
                        Line::from(Span::styled(" × ", theme.style(Role::Muted))),
                        rm_rect,
                    );
                    app.hits.add(rm_rect, HitId::PollFormRemove(i));
                }
            }
            PollFormSlot::AddAnswer => {
                app.hits.add(r, HitId::PollFormSlot(slot_idx));
                let label = format!("+ Add answer ({}/{})", form.answers.len(), MAX_POLL_ANSWERS);
                let style = if focused {
                    theme.style(Role::Pill)
                } else {
                    theme.style(Role::StatusKey)
                };
                frame.render_widget(
                    Line::from(vec![
                        Span::raw(" ".repeat(FIELD_LABEL.min(r.width as usize))),
                        Span::styled(
                            truncate(&label, r.width.saturating_sub(FIELD_LABEL as u16) as usize),
                            style,
                        ),
                    ]),
                    r,
                );
            }
            PollFormSlot::Kind => {
                app.hits.add(r, HitId::PollFormSlot(slot_idx));
                let label_style = if focused {
                    theme.style(Role::TitleFocused)
                } else {
                    theme.style(Role::Muted)
                };
                let value = if form.disclosed {
                    "Disclosed (live counts)"
                } else {
                    "Undisclosed (hidden until ended)"
                };
                let val_style = if focused {
                    theme.style(Role::Pill)
                } else {
                    theme.style(Role::Text)
                };
                let avail = r.width.saturating_sub(FIELD_LABEL as u16) as usize;
                frame.render_widget(
                    Line::from(vec![
                        Span::styled(format!("{:<FIELD_LABEL$}", "Kind"), label_style),
                        Span::styled(truncate(value, avail), val_style),
                    ]),
                    r,
                );
            }
            PollFormSlot::MaxSelections => {
                app.hits.add(r, HitId::PollFormSlot(slot_idx));
                let label_style = if focused {
                    theme.style(Role::TitleFocused)
                } else {
                    theme.style(Role::Muted)
                };
                let desc = if form.max_selections <= 1 {
                    "1 (single choice)".to_owned()
                } else {
                    format!(
                        "{} (up to {} choices)",
                        form.max_selections, form.max_selections
                    )
                };
                let mut x = r.x;
                frame.render_widget(
                    Line::from(Span::styled(
                        format!("{:<FIELD_LABEL$}", "Choices"),
                        label_style,
                    )),
                    Rect::new(x, r.y, (FIELD_LABEL as u16).min(r.width), 1),
                );
                x = x.saturating_add(FIELD_LABEL as u16);
                let rem = r.right().saturating_sub(x);
                if rem >= 9 {
                    let minus_rect = Rect::new(x, r.y, 3, 1);
                    frame.render_widget(
                        Line::from(Span::styled(" - ", theme.style(Role::Muted))),
                        minus_rect,
                    );
                    app.hits.add(minus_rect, HitId::PollFormMaxStep(-1));
                    x += 3;

                    let desc_w = rem.saturating_sub(6) as usize;
                    let text = truncate(&desc, desc_w);
                    let text_w = width(&text) as u16;
                    let val_style = if focused {
                        theme.style(Role::Pill)
                    } else {
                        theme.style(Role::Text)
                    };
                    frame.render_widget(
                        Line::from(Span::styled(format!(" {text} "), val_style)),
                        Rect::new(x, r.y, (text_w + 2).min(rem.saturating_sub(6)), 1),
                    );
                    let plus_x = (x + text_w + 2).min(r.right().saturating_sub(3));
                    if plus_x + 3 <= r.right() {
                        let plus_rect = Rect::new(plus_x, r.y, 3, 1);
                        frame.render_widget(
                            Line::from(Span::styled(" + ", theme.style(Role::Muted))),
                            plus_rect,
                        );
                        app.hits.add(plus_rect, HitId::PollFormMaxStep(1));
                    }
                }
            }
            PollFormSlot::Submit | PollFormSlot::Cancel => {}
        }
    }

    // Bottom pinned actions & error/hint line.
    let btn_y = inner.height - 2;
    let submit_slot = slots.len().saturating_sub(2);
    let cancel_slot = slots.len().saturating_sub(1);
    let submit_label = if form.editing.is_some() {
        "Save poll"
    } else {
        "Create poll"
    };
    let cancel_label = "Cancel";
    let submit_w = (submit_label.len() + 2) as u16;
    let cancel_w = (cancel_label.len() + 2) as u16;
    let submit_rect = Rect::new(inner.x, inner.y + btn_y, submit_w.min(inner.width), 1);
    let submit_focused = form.focus == submit_slot;
    button(frame, theme, submit_rect, submit_label, submit_focused);
    app.hits.add(submit_rect, HitId::PollFormSlot(submit_slot));

    let cancel_x = submit_rect.right().saturating_add(2);
    if cancel_x < inner.right() {
        let cancel_rect = Rect::new(
            cancel_x,
            inner.y + btn_y,
            cancel_w.min(inner.right() - cancel_x),
            1,
        );
        let cancel_focused = form.focus == cancel_slot;
        button(frame, theme, cancel_rect, cancel_label, cancel_focused);
        app.hits.add(cancel_rect, HitId::PollFormSlot(cancel_slot));
    }

    let note_rect = row(inner, inner.height - 1);
    let (msg, role) = match &form.error {
        Some(err) => (err.as_str(), Role::Error),
        None => (
            "2–20 answers · undisclosed hides counts until ended",
            Role::Muted,
        ),
    };
    frame.render_widget(
        Line::from(Span::styled(
            truncate(msg, note_rect.width as usize),
            theme.style(role),
        )),
        note_rect,
    );

    if let Some(pos) = cursor {
        frame.set_cursor_position(pos);
    }
}

/// Draws the keyboard/pointer voting chooser (`PollVotePopup`).
pub(super) fn draw_vote(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    popup_state: &mut PollVotePopup,
) {
    let w = area.width.saturating_sub(4).clamp(32, 64);
    let q_lines = wrap(&popup_state.question, w.saturating_sub(6) as usize)
        .len()
        .clamp(1, 2);
    let desired_h = (q_lines + popup_state.answers.len() + 6) as u16;
    let h = desired_h.clamp(10, area.height.saturating_sub(2).max(10));
    let Some(rect) = centered(area, w, h) else {
        return;
    };
    let footer = if popup_state.is_multi() {
        "↑↓ move · Space toggle · Enter vote · Esc cancel"
    } else {
        "↑↓ move · Space select · Enter vote · Esc cancel"
    };
    let inner = popup(frame, app, area, rect, " Vote in poll ", footer);
    if inner.width < 12 || inner.height < 6 {
        return;
    }
    let theme = &app.theme;
    let wrapped_q = wrap(&popup_state.question, inner.width as usize);
    let q_show = wrapped_q.len().clamp(1, 2);
    for (idx, text) in wrapped_q.iter().take(q_show).enumerate() {
        frame.render_widget(
            Line::from(Span::styled(
                text.clone(),
                theme.style(Role::Text).add_modifier(Modifier::BOLD),
            )),
            row(inner, idx as u16),
        );
    }
    let kind_note = if popup_state.is_multi() {
        format!("Choose up to {} answers", popup_state.effective_max())
    } else {
        "Single choice".to_owned()
    };
    let visibility_note = if popup_state.shows_counts() {
        "Disclosed"
    } else {
        "Undisclosed (counts hidden)"
    };
    frame.render_widget(
        Line::from(Span::styled(
            truncate(
                &format!("{kind_note} · {visibility_note}"),
                inner.width as usize,
            ),
            theme.style(Role::Muted),
        )),
        row(inner, q_show as u16),
    );

    let list_top = q_show + 2;
    let list_h = (inner.height as usize).saturating_sub(list_top + 2);
    if list_h > 0 && !popup_state.answers.is_empty() {
        if popup_state.cursor < popup_state.scroll {
            popup_state.scroll = popup_state.cursor;
        } else if popup_state.cursor >= popup_state.scroll + list_h {
            popup_state.scroll = popup_state.cursor + 1 - list_h;
        }
    }
    popup_state.scroll = popup_state
        .scroll
        .min(popup_state.answers.len().saturating_sub(list_h));

    for row_offset in 0..list_h {
        let idx = popup_state.scroll + row_offset;
        let Some(ans) = popup_state.answers.get(idx) else {
            break;
        };
        let r = row(inner, (list_top + row_offset) as u16);
        let picked = popup_state.is_selected(&ans.id);
        let cursor_here = popup_state.cursor == idx;
        let mark = match (popup_state.is_multi(), picked) {
            (false, true) => "● ",
            (false, false) => "○ ",
            (true, true) => "[✓] ",
            (true, false) => "[ ] ",
        };
        let suffix = if popup_state.shows_counts() {
            let pct = if popup_state.total_voters > 0 {
                ((u64::from(ans.votes) * 100) / u64::from(popup_state.total_voters)).min(100)
            } else {
                0
            };
            format!(" · {} ({pct}%)", ans.votes)
        } else {
            String::new()
        };
        let text_avail = (r.width as usize).saturating_sub(width(mark) + width(&suffix));
        let label = truncate(&ans.text, text_avail);
        let line_style = if cursor_here {
            theme.style(Role::MessageSelected)
        } else {
            theme.style(Role::Text)
        };
        let mark_style = if picked {
            theme.style(Role::StatusKey).add_modifier(Modifier::BOLD)
        } else {
            line_style
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(mark, mark_style),
                Span::styled(label, line_style),
                Span::styled(suffix, theme.style(Role::Muted)),
            ]))
            .style(line_style),
            r,
        );
        app.hits.add(r, HitId::PollVoteAnswer(idx));
    }

    let btn_y = inner.height - 2;
    let vote_rect = Rect::new(inner.x, inner.y + btn_y, 8.min(inner.width), 1);
    button(frame, theme, vote_rect, "Vote", false);
    app.hits.add(vote_rect, HitId::PollVoteBtn(true));
    let cancel_x = vote_rect.right().saturating_add(2);
    if cancel_x < inner.right() {
        let cancel_rect = Rect::new(
            cancel_x,
            inner.y + btn_y,
            8.min(inner.right() - cancel_x),
            1,
        );
        button(frame, theme, cancel_rect, "Cancel", false);
        app.hits.add(cancel_rect, HitId::PollVoteBtn(false));
    }

    let status_rect = row(inner, inner.height - 1);
    let (note, role) = match &popup_state.error {
        Some(err) => (err.clone(), Role::Error),
        None if popup_state.shows_counts() => (
            format!(
                "{} voter{}",
                popup_state.total_voters,
                if popup_state.total_voters == 1 {
                    ""
                } else {
                    "s"
                }
            ),
            Role::Muted,
        ),
        None => (
            "Results will show after the poll ends".to_owned(),
            Role::Muted,
        ),
    };
    frame.render_widget(
        Line::from(Span::styled(
            truncate(&note, status_rect.width as usize),
            theme.style(role),
        )),
        status_rect,
    );
}
