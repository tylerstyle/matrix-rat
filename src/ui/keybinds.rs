//! Settings -> Key bindings editor popup.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::hit::HitId;
use super::modal::{button, centered, popup, row};
use crate::app::App;
use crate::keymap::{Context, Editor, EditorButton, EditorStage, action_spec, format_label};
use crate::text::{truncate, width};
use crate::theme::Role;

enum DisplayItem {
    Heading(Context),
    Row(usize),
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect, editor: &mut Editor) {
    let Some(rect) = centered(area, 76, 26) else {
        return;
    };
    let footer = match editor.stage {
        EditorStage::Browse => "↑↓ move · Enter rebind · Del unbind · r reset · Esc close",
        EditorStage::Capture => "Press new key · Esc cancel",
        EditorStage::Conflict { .. } => "Enter replace · Esc cancel",
    };
    let inner = popup(frame, app, area, rect, "Key bindings", footer);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let theme = app.theme.clone();

    // Top bar: preset selector (`◂ default ▸`) + edited badge.
    let preset_row = row(inner, 0);
    if preset_row.height > 0 && preset_row.width >= 16 {
        let preset_name = app.keymap.preset().name();
        let mut x = preset_row.x;
        let mut spans = vec![
            Span::styled("Preset  ", theme.style(Role::Muted)),
            Span::styled("◂ ", theme.style(Role::StatusKey)),
            Span::styled(preset_name, theme.style(Role::TitleFocused)),
            Span::styled(" ▸", theme.style(Role::StatusKey)),
        ];
        x += 8; // "Preset  "
        let prev_rect = Rect::new(x, preset_row.y, 2, 1);
        app.hits
            .add(prev_rect, HitId::KeybindBtn(EditorButton::PresetPrev));
        if app.hover == Some(HitId::KeybindBtn(EditorButton::PresetPrev)) {
            frame
                .buffer_mut()
                .set_style(prev_rect, theme.style(Role::Hover));
        }
        x += 2 + width(preset_name) as u16;
        let next_rect = Rect::new(x, preset_row.y, 2, 1);
        if next_rect.right() <= preset_row.right() {
            app.hits
                .add(next_rect, HitId::KeybindBtn(EditorButton::PresetNext));
            if app.hover == Some(HitId::KeybindBtn(EditorButton::PresetNext)) {
                frame
                    .buffer_mut()
                    .set_style(next_rect, theme.style(Role::Hover));
            }
        }
        if app.keymap.has_overrides() && preset_row.width >= 32 {
            let badge = "modified";
            let used = 12 + width(preset_name);
            let gap = (preset_row.width as usize).saturating_sub(used + badge.len());
            if gap > 0 {
                spans.push(Span::raw(" ".repeat(gap)));
                spans.push(Span::styled(badge, theme.style(Role::StatusKey)));
            }
        }
        frame.render_widget(Line::from(spans), preset_row);
    }

    // Reserve bottom status + buttons rows cleanly when height permits.
    let button_rows = u16::from(inner.height >= 5);
    let prompt_rows = u16::from(inner.height >= 6);
    let top_rows = u16::from(inner.height >= 3) + 1; // preset + rule (if >=3)
    if inner.height >= 3 {
        let rule = "─".repeat(inner.width as usize);
        frame.render_widget(
            Line::from(Span::styled(rule, theme.style(Role::Border))),
            row(inner, 1),
        );
    }

    let list_top = if inner.height >= 3 { top_rows } else { 1 };
    let list_height = inner
        .height
        .saturating_sub(list_top + button_rows + prompt_rows) as usize;

    // Build flat heading + row display list.
    let mut items = Vec::with_capacity(editor.rows().len() + Context::ALL.len());
    let mut selected_item = 0;
    let mut prev_ctx: Option<Context> = None;
    for (idx, r) in editor.rows().iter().enumerate() {
        if prev_ctx != Some(r.context) {
            items.push(DisplayItem::Heading(r.context));
            prev_ctx = Some(r.context);
        }
        if idx == editor.selected {
            selected_item = items.len();
        }
        items.push(DisplayItem::Row(idx));
    }

    if list_height > 0 {
        let max_scroll = items.len().saturating_sub(list_height);
        if editor.scroll > max_scroll {
            editor.scroll = max_scroll;
        }
        // Keep selected item visible when navigating by keyboard/click (including its heading).
        if editor.follow_selection && !items.is_empty() {
            let top_target = if selected_item > 0
                && matches!(items[selected_item - 1], DisplayItem::Heading(_))
            {
                selected_item - 1
            } else {
                selected_item
            };
            if top_target < editor.scroll {
                editor.scroll = top_target;
            } else if selected_item >= editor.scroll + list_height {
                editor.scroll = (selected_item + 1 - list_height).min(max_scroll);
            }
        }

        let key_col_w = 20usize.min((inner.width as usize) / 2);
        let desc_room = (inner.width as usize).saturating_sub(key_col_w + 4);

        for (offset, item) in items
            .iter()
            .skip(editor.scroll)
            .take(list_height)
            .enumerate()
        {
            let rect = row(inner, list_top + offset as u16);
            match *item {
                DisplayItem::Heading(ctx) => {
                    frame.render_widget(
                        Line::from(Span::styled(ctx.title(), theme.style(Role::SidebarSection))),
                        rect,
                    );
                }
                DisplayItem::Row(idx) => {
                    let r = editor.rows()[idx];
                    let selected = idx == editor.selected;
                    let labels = app.keymap.labels_in(r.context, r.action);
                    let modified = app
                        .keymap
                        .config_override_for(r.context, r.action)
                        .is_some();
                    let key_text = if selected && matches!(editor.stage, EditorStage::Capture) {
                        "<press a key>".to_owned()
                    } else if labels.is_empty() {
                        "—".to_owned()
                    } else {
                        labels.join(", ")
                    };
                    let key_shown = truncate(&key_text, key_col_w);
                    let desc_shown = truncate(r.description, desc_room);
                    let marker = if selected { "› " } else { "  " };
                    let desc_style = if selected {
                        theme.style(Role::TitleFocused)
                    } else {
                        theme.style(Role::Text)
                    };
                    let key_style = if selected && matches!(editor.stage, EditorStage::Capture) {
                        theme.style(Role::Pill)
                    } else if labels.is_empty() {
                        theme.style(Role::Muted)
                    } else {
                        theme.style(Role::StatusKey)
                    };
                    let gap = (inner.width as usize).saturating_sub(
                        2 + width(&desc_shown) + width(&key_shown) + usize::from(modified),
                    );
                    let mut spans = vec![
                        Span::styled(marker, theme.style(Role::TitleFocused)),
                        Span::styled(desc_shown, desc_style),
                        Span::raw(" ".repeat(gap)),
                    ];
                    if modified {
                        spans.push(Span::styled("*", theme.style(Role::StatusKey)));
                    }
                    spans.push(Span::styled(key_shown, key_style));
                    frame.render_widget(Line::from(spans), rect);

                    let hit = HitId::KeybindRow(idx);
                    if app.hover == Some(hit) && !selected {
                        frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
                    }
                    app.hits.add(rect, hit);
                }
            }
        }
    }

    // Prompt / feedback row.
    if prompt_rows == 1 {
        let prompt_rect = row(inner, inner.height - button_rows - 1);
        let (text, role) = match &editor.stage {
            EditorStage::Capture => {
                if let Some((msg, true)) = &editor.feedback {
                    (msg.clone(), Role::Error)
                } else if let Some(cur) = editor.current_row() {
                    (
                        format!(
                            "Press a key for {}: {} (Esc cancels)",
                            cur.context.title(),
                            cur.description
                        ),
                        Role::TitleFocused,
                    )
                } else {
                    ("Press a key (Esc cancels)".to_owned(), Role::TitleFocused)
                }
            }
            EditorStage::Conflict {
                code,
                mods,
                displaced,
            } => {
                let key_name = format_label(*code, *mods);
                let other_desc =
                    action_spec(*displaced).map_or("another action", |s| s.description);
                let ctx_title = editor
                    .current_row()
                    .map_or("this context", |r| r.context.title());
                (
                    format!("{key_name} is used by {other_desc} in {ctx_title}. Replace it?"),
                    Role::Error,
                )
            }
            EditorStage::Browse => {
                if let Some((msg, is_err)) = &editor.feedback {
                    (
                        msg.clone(),
                        if *is_err { Role::Error } else { Role::StatusOk },
                    )
                } else if let Some(cur) = editor.current_row()
                    && let Some(spec) = action_spec(cur.action)
                {
                    (
                        format!(
                            "config: [keys.{}] {} ",
                            cur.context.config_name(),
                            spec.name
                        ),
                        Role::Muted,
                    )
                } else {
                    (String::new(), Role::Muted)
                }
            }
        };
        if !text.is_empty() {
            frame.render_widget(
                Line::from(Span::styled(
                    truncate(&text, prompt_rect.width as usize),
                    theme.style(role),
                )),
                prompt_rect,
            );
        }
    }

    // Button bar row.
    if button_rows == 1 {
        let btn_row = row(inner, inner.height - 1);
        let btns: &[(EditorButton, &str, bool)] = match editor.stage {
            EditorStage::Conflict { .. } => &[
                (EditorButton::ConfirmReplace, " Replace ", true),
                (EditorButton::CancelPrompt, " Cancel ", false),
            ],
            EditorStage::Browse | EditorStage::Capture => &[
                (
                    EditorButton::Rebind,
                    " Rebind ",
                    matches!(editor.stage, EditorStage::Capture),
                ),
                (EditorButton::Unbind, " Unbind ", false),
                (EditorButton::Reset, " Reset ", false),
                (EditorButton::ResetAll, " Reset all ", false),
                (EditorButton::Close, " Close ", false),
            ],
        };
        let mut x = btn_row.x;
        for &(id, label, focused) in btns {
            let w = width(label) as u16;
            if x + w > btn_row.right() {
                break;
            }
            let r = Rect::new(x, btn_row.y, w, 1);
            let hit = HitId::KeybindBtn(id);
            button(frame, &theme, r, label, focused || app.hover == Some(hit));
            app.hits.add(r, hit);
            x += w + 2;
        }
    }
}
