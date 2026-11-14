use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};

use super::hit::HitId;
use crate::action::Pane;
use crate::app::{App, SideRow};
use crate::text::{truncate, width};
use crate::theme::Role;

/// The first letter of a name, in exactly two columns.
pub(super) fn initial(name: &str) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    let first = name
        .trim_start_matches(|c: char| !c.is_alphanumeric() && c.is_ascii())
        .graphemes(true)
        .next()
        .unwrap_or("?");
    // Upper case first: it can change the width ("ß" becomes "SS").
    let upper = first.to_uppercase();
    match (width(&upper), width(first)) {
        (1, _) => format!("{upper} "),
        (2, _) => upper,
        (_, 1) => format!("{first} "),
        (_, 2) => first.to_owned(),
        _ => "? ".to_owned(),
    }
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = super::pane_block(app, Pane::Sidebar, "Rooms");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.hits.add(area, HitId::Pane(Pane::Sidebar));
    super::roominfo::new_button(frame, app, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let focused = app.focus == Pane::Sidebar;

    // Filter line.
    let filter_area = Rect::new(inner.x, inner.y, inner.width, 1);
    let narrow_badge = (inner.width < 18)
        .then_some(match (app.sidebar.unread_only, app.sidebar.people_only) {
            (true, true) => " UP",
            (true, false) => " U",
            (false, true) => " P",
            (false, false) => "",
        })
        .filter(|s| !s.is_empty());
    let badge_w = narrow_badge.map_or(0, width) as u16;
    let filter_text_area = Rect::new(
        filter_area.x,
        filter_area.y,
        filter_area.width.saturating_sub(badge_w),
        1,
    );
    let mut filter_cursor = None;
    let filter = if app.sidebar.filter.is_empty() && !focused {
        Line::from(Span::styled(
            truncate(" Filter rooms…", filter_text_area.width as usize),
            app.theme.style(Role::Placeholder),
        ))
    } else if app.sidebar.filter.is_empty() {
        Line::from(Span::styled(
            truncate(" type to filter…", filter_text_area.width as usize),
            app.theme.style(Role::Placeholder),
        ))
    } else {
        let key_style = app.theme.style(Role::StatusKey);
        let max_cols = (filter_text_area.width as usize).saturating_sub(4);
        let view = app.sidebar.filter.view(max_cols, false);
        filter_cursor = (focused && !app.help_open && app.modal.is_none()).then(|| {
            Position::new(
                (filter_text_area.x + 3 + view.cursor_col as u16)
                    .min(filter_text_area.right().saturating_sub(1)),
                filter_text_area.y,
            )
        });
        Line::from(vec![Span::styled(" / ", key_style), Span::raw(view.text)])
    };
    frame.render_widget(&filter, filter_text_area);
    if let Some(badge) = narrow_badge {
        frame.render_widget(
            Line::from(Span::styled(badge, app.theme.style(Role::StatusKey))),
            Rect::new(filter_text_area.right(), filter_area.y, badge_w, 1),
        );
    }
    if let Some(pos) = filter_cursor {
        frame.set_cursor_position(pos);
    }
    app.hits.add(filter_area, HitId::RoomFilter);

    let mut top = 1;
    // Quick-filter chips ("Unread" / "People") on their own header row when wide.
    if inner.width >= 18 && inner.height > 3 {
        let chips_area = Rect::new(inner.x, inner.y + top, inner.width, 1);
        draw_filter_chips(frame, app, chips_area);
        top += 1;
    }

    // With spaces: one line that says which one the list is narrowed to, with arrows
    // to step through them and unread / other-spaces indicators.
    if !app.spaces.is_empty() && inner.height > top + 1 && inner.width >= 8 {
        let area = Rect::new(inner.x, inner.y + top, inner.width, 1);
        draw_space_header(frame, app, area);
        top += 1;
    }
    let list = Rect::new(
        inner.x,
        inner.y + top,
        inner.width,
        inner.height.saturating_sub(top),
    );
    let rows = app.sidebar_rows();
    let cursor_room = app.cursor_room();

    // Keep the keyboard cursor on screen (both of a room's lines, if it has two).
    let height = list.height as usize;
    let wide = app.room_previews;
    let lines = if wide { 2 } else { 1 };
    let tile_cols: u16 = if wide { 4 } else { 2 };
    app.images.set_avatar_cells((tile_cols, lines as u16));
    if app.sidebar.reveal_cursor {
        app.sidebar.reveal_cursor = false;
        if let Some(at) = rows
            .iter()
            .position(|r| matches!(r, SideRow::Room(i) if Some(*i) == cursor_room))
        {
            if at <= app.sidebar.scroll {
                // Also show the section header above the room, where there is room.
                app.sidebar.scroll = if height > lines {
                    at.saturating_sub(1)
                } else {
                    at
                };
            } else if at + lines > app.sidebar.scroll + height {
                // Never so far that the room's own first line leaves the top.
                app.sidebar.scroll = (at + lines).saturating_sub(height).min(at);
            }
        }
    }
    app.sidebar.scroll = app.sidebar.scroll.min(rows.len().saturating_sub(height));

    if rows.is_empty() {
        let filtered =
            !app.sidebar.filter.is_empty() || app.sidebar.unread_only || app.sidebar.people_only;
        let text = if filtered {
            " no rooms match"
        } else if app.space.is_some() {
            " no rooms here"
        } else {
            " no rooms yet"
        };
        let line = Line::from(Span::styled(text, app.theme.style(Role::Muted)));
        frame.render_widget(&line, list);
        return;
    }

    // Lines actually drawn: a room's two lines can be cut apart at either edge.
    let theme = &app.theme;
    let shown = rows.len().saturating_sub(app.sidebar.scroll).min(height);
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
                } else if room.bold || room.marked_unread {
                    Role::RoomUnread
                } else {
                    Role::Room
                };
                highlight(
                    frame,
                    app,
                    rect,
                    *index,
                    focused && Some(*index) == cursor_room,
                );

                let badge = match room.unread {
                    0 => String::new(),
                    n if n > 99 => " 99+ ".to_owned(),
                    n => format!(" {n} "),
                };
                let badge_width = width(&badge) as u16;
                // A small coloured tile with the room's initial stands in for its
                // picture: 1 column of margin, the tile, 1 of gap.
                let name_width = (rect.width.saturating_sub(3 + tile_cols + badge_width)) as usize;
                let name_style = theme.style(base);
                let tile = ratatui::style::Style::new()
                    .fg(theme.style(Role::UnreadBadge).fg.unwrap_or_default())
                    .bg(theme.user_color(&room.name))
                    .add_modifier(ratatui::style::Modifier::BOLD);
                // The open room keeps a bright bar at the edge, so it is easy to find
                // wherever the focus is.
                let marker = if active {
                    Span::styled("▌", theme.style(Role::BorderFocused))
                } else {
                    Span::raw(" ")
                };
                let line = Line::from(vec![
                    marker,
                    Span::styled(
                        if wide {
                            format!(" {} ", initial(&room.name))
                        } else {
                            initial(&room.name)
                        },
                        tile,
                    ),
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
                // The room's picture goes over the tile where the terminal can show one.
                // With two lines per room it is drawn with the second line, unless that
                // one is below the edge: then its upper half goes here.
                if (!wide || n + 1 == shown)
                    && rect.width > tile_cols + 1
                    && let Some(url) = &room.avatar
                {
                    let cells = Rect::new(rect.x + 1, rect.y, tile_cols, 1);
                    let covered = app.modal.is_some() || app.help_open || app.images.viewer_open();
                    super::image::draw_avatar(
                        frame,
                        &mut app.images,
                        theme,
                        url,
                        cells,
                        0,
                        covered,
                    );
                }
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
            SideRow::Preview(index) => {
                let room = &app.rooms[*index];
                highlight(
                    frame,
                    app,
                    rect,
                    *index,
                    focused && Some(*index) == cursor_room,
                );
                let marker = if *index == app.active {
                    Span::styled("▌", theme.style(Role::BorderFocused))
                } else {
                    Span::raw(" ")
                };
                let tile = ratatui::style::Style::new().bg(theme.user_color(&room.name));
                let text_width = rect.width.saturating_sub(3 + tile_cols) as usize;
                // Until the worker has said what the latest message is (and in the
                // demo), the newest loaded one stands in.
                // For the open room the loaded messages are the freshest source too:
                // the room list's own line follows a moment later.
                let loaded = room
                    .messages
                    .iter()
                    .rev()
                    .find(|m| !m.sender.is_empty() && !m.body.trim().is_empty())
                    .map(|m| {
                        let line = m
                            .body
                            .lines()
                            .map(str::trim)
                            .find(|line| !line.is_empty() && !line.starts_with('>'))
                            .unwrap_or_default();
                        if m.own {
                            format!("You: {line}")
                        } else if room.direct {
                            line.to_owned()
                        } else {
                            format!("{}: {line}", m.sender)
                        }
                    });
                let text = if *index == app.active {
                    loaded.as_deref().or(room.preview.as_deref())
                } else {
                    room.preview.as_deref().or(loaded.as_deref())
                }
                .unwrap_or_default();
                let muted = ratatui::style::Style {
                    fg: theme.style(Role::Muted).fg,
                    ..ratatui::style::Style::new()
                };
                let line = Line::from(vec![
                    marker,
                    // Normally the lower half of the tile; if the room's first line is
                    // scrolled out, this line carries the initial instead of a blank.
                    Span::styled(
                        if n == 0 {
                            format!(" {} ", initial(&room.name))
                        } else {
                            " ".repeat(tile_cols as usize)
                        },
                        tile.fg(theme.style(Role::UnreadBadge).fg.unwrap_or_default()),
                    ),
                    Span::raw(" "),
                    Span::styled(truncate(text, text_width), muted),
                ]);
                frame.render_widget(&line, rect);
                // The picture covers the tile on both lines; if the room's first line is
                // scrolled out above, its lower half is drawn here.
                if rect.width > tile_cols + 1
                    && let Some(url) = &room.avatar
                {
                    let (cells, skip) = if n > 0 {
                        (Rect::new(rect.x + 1, rect.y - 1, tile_cols, 2), 0)
                    } else {
                        (Rect::new(rect.x + 1, rect.y, tile_cols, 1), 1)
                    };
                    let covered = app.modal.is_some() || app.help_open || app.images.viewer_open();
                    super::image::draw_avatar(
                        frame,
                        &mut app.images,
                        theme,
                        url,
                        cells,
                        skip,
                        covered,
                    );
                }
                app.hits.add(rect, HitId::Room(*index));
            }
        }
    }
}

/// The background that marks the open room, the keyboard cursor or the pointer.
fn highlight(frame: &mut Frame, app: &App, rect: Rect, index: usize, cursor: bool) {
    let theme = &app.theme;
    if index == app.active {
        frame
            .buffer_mut()
            .set_style(rect, theme.style(Role::RoomActive));
    } else if cursor {
        frame
            .buffer_mut()
            .set_style(rect, theme.style(Role::RoomCursor));
    } else if app.hover == Some(HitId::Room(index)) {
        frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
    }
}

fn draw_filter_chips(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = &app.theme;
    let pad_w: u16 = if area.width >= 19 { 1 } else { 0 };
    let gap: u16 = 2;
    let mut spans = Vec::new();
    if pad_w > 0 {
        spans.push(Span::raw(" "));
    }
    let mut x = area.x + pad_w;
    let mut chip_rects = Vec::with_capacity(2);
    for (idx, (label, active, action)) in [
        (
            "Unread",
            app.sidebar.unread_only,
            crate::action::Action::FilterUnread,
        ),
        (
            "People",
            app.sidebar.people_only,
            crate::action::Action::FilterPeople,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        if idx > 0 {
            spans.push(Span::raw("  "));
            x += gap;
        }
        let chip = format!("{} {label}", if active { "●" } else { "○" });
        let chip_w = width(&chip) as u16;
        let role = if active {
            Role::TitleFocused
        } else {
            Role::Muted
        };
        spans.push(Span::styled(chip, theme.style(role)));
        let hit = HitId::Do(action);
        let rect = Rect::new(x, area.y, chip_w.min(area.right().saturating_sub(x)), 1);
        app.hits.add(rect, hit);
        chip_rects.push((rect, hit));
        x += chip_w;
    }
    frame.render_widget(Line::from(spans), area);
    for (rect, hit) in chip_rects {
        if rect.width > 0 && app.hover == Some(hit) {
            frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
        }
    }
}

fn draw_space_header(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = &app.theme;
    let name = app.space_name().unwrap_or_else(|| "All rooms".to_owned());
    let cur = app.selected_space_unread();
    let other = app.other_spaces_unread();
    let badge_role = |mention: bool| {
        if mention {
            Role::MentionBadge
        } else {
            Role::UnreadBadge
        }
    };
    let mut cur_badge = cur.and_then(|c| {
        if c.notifications > 0 {
            Some((format!(" {}", c.notifications), badge_role(c.mention)))
        } else if c.unread {
            Some((" ●".to_owned(), Role::UnreadBadge))
        } else {
            None
        }
    });
    let mut other_badge = other.unread.then(|| {
        let text = if other.notifications > 0 {
            format!(" +{}", other.notifications)
        } else {
            " •".to_owned()
        };
        (text, badge_role(other.mention))
    });

    let avail = area.width as usize;
    let fixed = 5usize; // " ◂ " + " ▸"
    let min_label = 3usize.min(avail.saturating_sub(fixed));
    if let Some((text, _)) = &mut other_badge
        && fixed + width(text) + min_label > avail
    {
        if fixed + 2 + min_label <= avail {
            *text = " •".to_owned();
        } else {
            other_badge = None;
        }
    }
    let other_w = other_badge.as_ref().map_or(0, |(t, _)| width(t));
    if let Some((text, _)) = &cur_badge
        && fixed + width(text) + other_w + min_label > avail
    {
        cur_badge = None;
    }
    let cur_w = cur_badge.as_ref().map_or(0, |(t, _)| width(t));
    let label = truncate(&name, avail.saturating_sub(fixed + cur_w + other_w));

    let role = if app.space.is_some() {
        Role::TitleFocused
    } else {
        Role::Muted
    };
    let arrow = theme.style(Role::StatusKey);
    let mut spans = vec![
        Span::styled(" ◂ ", arrow),
        Span::styled(label, theme.style(role)),
    ];
    if let Some((text, badge)) = cur_badge {
        spans.push(Span::styled(text, theme.style(badge)));
    }
    spans.push(Span::styled(" ▸", arrow));
    if let Some((text, badge)) = other_badge {
        spans.push(Span::styled(text, theme.style(badge)));
    }
    frame.render_widget(Line::from(spans), area);
    app.hits
        .add(area, HitId::Do(crate::action::Action::SpaceNext));
    app.hits.add(
        Rect::new(area.x, area.y, 3, 1),
        HitId::Do(crate::action::Action::SpacePrev),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_are_always_two_columns() {
        for (name, tile) in [
            ("rust", "R "),
            ("#matrix", "M "),
            ("ßtraße", "SS"),
            ("日本語", "日"),
            ("", "? "),
            ("🐀 rats", "🐀"),
        ] {
            assert_eq!(initial(name), tile, "{name}");
            assert_eq!(width(&initial(name)), 2, "{name}");
        }
    }
}
