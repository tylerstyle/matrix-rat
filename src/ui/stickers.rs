//! The sticker picker: the stickers of the image packs, by name, with a filter.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::hit::{HitId, InputTarget};
use super::modal::{centered, field, popup, row};
use crate::app::App;
use crate::modal::StickerPicker;
use crate::text::{truncate, width};
use crate::theme::Role;

pub(super) fn draw(frame: &mut Frame, app: &mut App, area: Rect, picker: &mut StickerPicker) {
    let list_rows = picker.filtered.len().clamp(3, 12) as u16;
    let Some(rect) = centered(area, 64, list_rows + 6) else {
        return;
    };
    let inner = popup(
        frame,
        app,
        area,
        rect,
        "Stickers",
        "Enter send · ↑↓ select · Esc close",
    );
    let theme = app.theme.clone();
    let (cursor, filter_rect) = field(
        frame,
        &theme,
        row(inner, 0),
        "Filter",
        &mut picker.filter,
        false,
        true,
    );
    app.hits
        .add(filter_rect, HitId::Input(InputTarget::StickerFilter));

    let count_line = if picker.loading && picker.all.is_empty() {
        "Loading sticker packs…".to_owned()
    } else if picker.all.is_empty() {
        "No stickers in your packs or this room's".to_owned()
    } else if picker.filter.as_str().trim().is_empty() {
        match picker.all.len() {
            1 => "1 sticker".to_owned(),
            n => format!("{n} stickers"),
        }
    } else {
        format!("{} of {} matching", picker.filtered.len(), picker.all.len())
    };
    frame.render_widget(
        Line::from(Span::styled(
            truncate(&count_line, inner.width as usize),
            theme.style(Role::Muted),
        )),
        row(inner, 1),
    );

    let list_y = 2u16;
    let rows = inner.height.saturating_sub(list_y) as usize;
    picker.page = rows;
    if rows > 0 {
        if picker.reveal {
            picker.reveal = false;
            if picker.selected < picker.scroll {
                picker.scroll = picker.selected;
            } else if picker.selected >= picker.scroll + rows {
                picker.scroll = picker.selected + 1 - rows;
            }
        }
        picker.scroll = picker
            .scroll
            .min(picker.filtered.len().saturating_sub(rows));
        if picker.filtered.is_empty() && !picker.all.is_empty() {
            frame.render_widget(
                Line::from(Span::styled(
                    truncate("No matching stickers.", inner.width as usize),
                    theme.style(Role::Muted),
                )),
                row(inner, list_y),
            );
        }
        for offset in 0..rows.min(picker.filtered.len().saturating_sub(picker.scroll)) {
            let index = picker.scroll + offset;
            let Some(sticker) = picker.sticker_at(index) else {
                break;
            };
            let rect = row(inner, list_y + offset as u16);
            if rect.width == 0 {
                continue;
            }
            let selected = index == picker.selected;
            let hovered = app.hover == Some(HitId::StickerRow(index));
            let style = if selected {
                theme.style(Role::TitleFocused)
            } else if hovered {
                theme.style(Role::Hover)
            } else {
                theme.style(Role::Text)
            };
            let marker = if selected { "› " } else { "  " };
            // The name first; the pack after it, as far as there is room.
            let room = (rect.width as usize).saturating_sub(width(marker));
            let name = truncate(&sticker.name, room);
            let left = room.saturating_sub(width(&name));
            let pack = if left >= 6 {
                truncate(&format!("  {}", sticker.pack_name), left)
            } else {
                String::new()
            };
            frame.render_widget(
                Line::from(vec![
                    Span::styled(marker, theme.style(Role::TitleFocused)),
                    Span::styled(name, style),
                    Span::styled(pack, theme.style(Role::Muted)),
                ]),
                rect,
            );
            app.hits.add(rect, HitId::StickerRow(index));
        }
    }

    if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
}
