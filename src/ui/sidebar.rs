use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::hit::HitId;
use crate::action::Pane;
use crate::app::{App, SideRow};
use crate::text::{truncate, width};
use crate::theme::Role;

/// The first letter of a name, in exactly two columns.
fn initial(name: &str) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    let first = name
        .trim_start_matches(|c: char| !c.is_alphanumeric() && c.is_ascii())
        .graphemes(true)
        .next()
        .unwrap_or("?");
    match width(first) {
        1 => format!("{} ", first.to_uppercase()),
        2 => first.to_owned(),
        _ => "? ".to_owned(),
    }
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = super::pane_block(app, Pane::Sidebar, "Rooms");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.hits.add(area, HitId::Pane(Pane::Sidebar));
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let theme = &app.theme;
    let focused = app.focus == Pane::Sidebar;

    // Filter line.
    let filter_area = Rect::new(inner.x, inner.y, inner.width, 1);
    let filter = if app.sidebar.filter.is_empty() && !focused {
        Line::from(Span::styled(
            " Filter rooms…",
            theme.style(Role::Placeholder),
        ))
    } else if app.sidebar.filter.is_empty() {
        Line::from(Span::styled(
            " type to filter…",
            theme.style(Role::Placeholder),
        ))
    } else {
        Line::from(vec![
            Span::styled(" / ", theme.style(Role::StatusKey)),
            Span::raw(app.sidebar.filter.clone()),
        ])
    };
    frame.render_widget(&filter, filter_area);
    app.hits.add(filter_area, HitId::RoomFilter);

    let list = Rect::new(
        inner.x,
        inner.y + 1,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let rows = app.sidebar_rows();
    let cursor_room = app.cursor_room();

    // Keep the keyboard cursor on screen.
    let height = list.height as usize;
    if app.sidebar.reveal_cursor {
        app.sidebar.reveal_cursor = false;
        if let Some(at) = rows
            .iter()
            .position(|r| matches!(r, SideRow::Room(i) if Some(*i) == cursor_room))
        {
            if at < app.sidebar.scroll + 1 {
                // Also show the section header above the first room.
                app.sidebar.scroll = at.saturating_sub(1);
            } else if at >= app.sidebar.scroll + height {
                app.sidebar.scroll = at + 1 - height;
            }
        }
    }
    app.sidebar.scroll = app.sidebar.scroll.min(rows.len().saturating_sub(height));

    if rows.is_empty() {
        let text = if app.sidebar.filter.is_empty() {
            " no rooms yet"
        } else {
            " no rooms match"
        };
        let line = Line::from(Span::styled(text, theme.style(Role::Muted)));
        frame.render_widget(&line, list);
        return;
    }

    for (n, row) in rows
        .iter()
        .skip(app.sidebar.scroll)
        .take(height)
        .enumerate()
    {
        let rect = Rect::new(list.x, list.y + n as u16, list.width, 1);
        match row {
            SideRow::Section(kind) => {
                let line = Line::from(Span::styled(
                    format!(" {}", kind.title()),
                    theme.style(Role::SidebarSection),
                ));
                frame.render_widget(&line, rect);
            }
            SideRow::Room(index) => {
                let room = &app.rooms[*index];
                let active = *index == app.active;
                let base = if active {
                    Role::RoomActive
                } else if room.bold {
                    Role::RoomUnread
                } else {
                    Role::Room
                };
                if active {
                    frame.buffer_mut().set_style(rect, theme.style(base));
                } else if focused && Some(*index) == cursor_room {
                    frame
                        .buffer_mut()
                        .set_style(rect, theme.style(Role::RoomCursor));
                } else if app.hover == Some(HitId::Room(*index)) {
                    frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
                }

                let badge = match room.unread {
                    0 => String::new(),
                    n if n > 99 => " 99+ ".to_owned(),
                    n => format!(" {n} "),
                };
                let badge_width = width(&badge) as u16;
                // A small coloured tile with the room's initial stands in for its
                // picture: 1 column of margin, 2 of tile, 1 of gap.
                let name_width = (rect.width.saturating_sub(5 + badge_width)) as usize;
                let name_style = theme.style(base);
                let tile = ratatui::style::Style::new()
                    .fg(theme.style(Role::UnreadBadge).fg.unwrap_or_default())
                    .bg(theme.user_color(&room.name))
                    .add_modifier(ratatui::style::Modifier::BOLD);
                let line = Line::from(vec![
                    Span::raw(" "),
                    Span::styled(initial(&room.name), tile),
                    Span::raw(" "),
                    Span::styled(
                        truncate(&room.name, name_width),
                        ratatui::style::Style {
                            fg: name_style.fg,
                            ..ratatui::style::Style::new()
                        }
                        .add_modifier(name_style.add_modifier),
                    ),
                ]);
                frame.render_widget(&line, rect);
                if badge_width > 0 && rect.width > badge_width + 4 {
                    let badge_rect =
                        Rect::new(rect.right() - badge_width - 1, rect.y, badge_width, 1);
                    let role = if room.mention {
                        Role::MentionBadge
                    } else {
                        Role::UnreadBadge
                    };
                    let line = Line::from(Span::styled(badge, theme.style(role)));
                    frame.render_widget(&line, badge_rect);
                }
                app.hits.add(rect, HitId::Room(*index));
            }
        }
    }
}
