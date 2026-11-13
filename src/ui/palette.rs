//! Drawing the command palette popup.

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use super::hit::HitId;
use super::modal::{centered, popup, row};
use crate::app::App;
use crate::palette::{Category, Palette};
use crate::text::{truncate, width};
use crate::theme::Role;

const WIDTH: u16 = 68;
const HEIGHT: u16 = 18;
const CATEGORY_WIDTH: usize = 9;

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect, palette: &mut Palette) {
    let Some(rect) = centered(area, WIDTH, HEIGHT) else {
        return;
    };
    let inner = popup(
        frame,
        app,
        area,
        rect,
        "Command palette",
        "↑↓ move · Enter run · Esc close",
    );
    let theme = &app.theme;
    if inner.width == 0 || inner.height < 2 {
        return;
    }

    // Search row.
    let search = row(inner, 0);
    let max_query = (search.width as usize).saturating_sub(2);
    let (search_line, query_cols) = if palette.query.is_empty() {
        (
            Line::from(vec![
                Span::styled("⌕ ", theme.style(Role::StatusKey)),
                Span::styled(
                    truncate("type a command, room, or theme…", max_query),
                    theme.style(Role::Placeholder),
                ),
            ]),
            0,
        )
    } else {
        let shown = tail_fit(&palette.query, max_query.saturating_sub(1));
        let cols = width(&shown) as u16;
        (
            Line::from(vec![
                Span::styled("⌕ ", theme.style(Role::StatusKey)),
                Span::raw(shown),
            ]),
            cols,
        )
    };
    frame.render_widget(search_line, search);
    frame.set_cursor_position(Position::new(
        (search.x + 2 + query_cols).min(search.right().saturating_sub(1)),
        search.y,
    ));

    // Optional separator rule + match count when there is room.
    let list_y = if inner.height >= 4 {
        let rule_rect = row(inner, 1);
        let count = format!(" {}/{}", palette.filtered.len(), palette.entries.len());
        let count_w = width(&count);
        if rule_rect.width as usize >= count_w + 6 {
            let line_w = rule_rect.width as usize - count_w;
            frame.render_widget(
                Line::from(vec![
                    Span::styled("─".repeat(line_w), theme.style(Role::Border)),
                    Span::styled(count, theme.style(Role::Muted)),
                ]),
                rule_rect,
            );
        } else {
            frame.render_widget(
                Line::from(Span::styled(
                    "─".repeat(rule_rect.width as usize),
                    theme.style(Role::Border),
                )),
                rule_rect,
            );
        }
        2
    } else {
        1
    };

    let list_height = inner.height.saturating_sub(list_y) as usize;
    if list_height == 0 {
        return;
    }
    palette.rows = list_height;

    if !palette.filtered.is_empty() {
        palette.selected = palette.selected.min(palette.filtered.len() - 1);
        if palette.reveal {
            palette.reveal = false;
            if palette.selected < palette.scroll {
                palette.scroll = palette.selected;
            } else if palette.selected >= palette.scroll + list_height {
                palette.scroll = palette.selected + 1 - list_height;
            }
        }
        palette.scroll = palette
            .scroll
            .min(palette.filtered.len().saturating_sub(list_height));
    } else {
        palette.selected = 0;
        palette.scroll = 0;
    }

    if palette.filtered.is_empty() {
        frame.render_widget(
            Line::from(Span::styled(
                "  no matching commands",
                theme.style(Role::Muted),
            )),
            row(inner, list_y),
        );
        return;
    }

    for (vis, &entry_idx) in palette
        .filtered
        .iter()
        .enumerate()
        .skip(palette.scroll)
        .take(list_height)
    {
        let rect = row(inner, list_y + (vis - palette.scroll) as u16);
        let entry = &palette.entries[entry_idx];
        let selected = vis == palette.selected;
        let hovered = app.hover == Some(HitId::Palette(vis));

        let marker = if selected { "› " } else { "  " };
        let marker_role = if entry.enabled {
            Role::TitleFocused
        } else {
            Role::Muted
        };
        let cat_width = if rect.width as usize >= 36 {
            CATEGORY_WIDTH
        } else {
            0
        };
        let fixed_left = 2 + cat_width;
        let avail = (rect.width as usize).saturating_sub(fixed_left);

        let hint_text = entry.hint.as_deref().unwrap_or("");
        let hint_cap = avail / 2;
        let shown_hint = if !hint_text.is_empty() && avail >= 12 {
            let h = truncate(hint_text, hint_cap);
            if width(&h) + 4 <= avail {
                h
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        let hint_w = width(&shown_hint);
        let title_cap = if hint_w > 0 {
            avail.saturating_sub(hint_w + 2)
        } else {
            avail
        };
        let shown_title = truncate(&entry.title, title_cap);
        let gap = if hint_w > 0 {
            avail.saturating_sub(width(&shown_title) + hint_w)
        } else {
            0
        };

        let title_role = if !entry.enabled {
            Role::Muted
        } else if selected {
            Role::TitleFocused
        } else {
            Role::Text
        };
        let hint_role = if !entry.enabled {
            Role::Muted
        } else if entry.category == Category::Action {
            Role::StatusKey
        } else {
            Role::Muted
        };

        let mut spans = vec![Span::styled(marker, theme.style(marker_role))];
        if cat_width > 0 {
            let cat_role = if entry.enabled {
                Role::SidebarSection
            } else {
                Role::Muted
            };
            spans.push(Span::styled(
                format!("{:<cat_width$}", entry.category.label()),
                theme.style(cat_role),
            ));
        }
        spans.push(Span::styled(shown_title, theme.style(title_role)));
        if hint_w > 0 {
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(Span::styled(shown_hint, theme.style(hint_role)));
        }

        frame.render_widget(Line::from(spans), rect);
        if selected {
            let bg = if entry.enabled {
                Role::MessageSelected
            } else {
                Role::RoomCursor
            };
            frame.buffer_mut().set_style(rect, theme.style(bg));
        } else if hovered && entry.enabled {
            frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
        }
        app.hits.add(rect, HitId::Palette(vis));
    }
}

/// Drops leading graphemes until `s` fits inside `max_cols`, keeping the tail visible.
fn tail_fit(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if width(s) <= max_cols {
        return s.to_owned();
    }
    let graphemes: Vec<&str> = s.graphemes(true).collect();
    let mut start = graphemes.len();
    let mut used = 0;
    while start > 0 {
        let gw = width(graphemes[start - 1]);
        if used + gw > max_cols {
            break;
        }
        used += gw;
        start -= 1;
    }
    graphemes[start..].concat()
}
