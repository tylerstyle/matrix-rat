//! Drawing the room popups: "New chat or room" (join, direct chat, create, invite) and
//! "Room info" with its member list. Also the two small ways in for the mouse.

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::hit::{HitId, InputTarget};
use super::image::{avatar_tile, draw_avatar};
use super::modal::{button, centered, field, popup, row};
use crate::action::Action;
use crate::app::App;
use crate::manage::{
    BanPrompt, Click, Confirm, InfoPopup, InfoRow, NewPopup, PermAction, RoleChooser, RoomSetting,
    SettingSubview, Stage, address_entries, confirm_keys, notify_label, validate_permissions_draft,
};
use crate::matrix::{
    PreviewMembership, RoomHistory, RoomJoinRule, RoomKnockRequest, RoomMember, SpaceChild,
};
use crate::text::{sanitize_line, truncate, width, wrap};
use crate::theme::{Role, Theme};

const USER_SEARCH_MAX_ROWS: usize = 5;

/// The `+` at the right end of the room list's title: new chat or room.
pub fn new_button(frame: &mut Frame, app: &mut App, area: Rect) {
    // The title (" 1 Rooms ") keeps its place; the button only where both fit.
    if area.width < 16 || area.height == 0 {
        return;
    }
    let rect = Rect::new(area.right() - 4, area.y, 3, 1);
    let action = Action::NewRoom;
    let role = if app.hover == Some(HitId::Do(action)) {
        Role::Hover
    } else {
        Role::StatusKey
    };
    frame.render_widget(Line::from(Span::styled(" + ", app.theme.style(role))), rect);
    app.hits.add(rect, HitId::Do(action));
}

/// Makes the room's name in the title of the messages pane open the room info: that
/// room's, whichever pane has the focus.
pub fn title_hit(hits: &mut super::hit::HitMap, area: Rect, title: &str) {
    // " 2 " before the name, one space after it, inside the corner.
    let shown = (width(title) as u16 + 4).min(area.width.saturating_sub(2));
    hits.add(
        Rect::new(area.x + 1, area.y, shown, 1),
        hit(Click::OpenInfo),
    );
}

/// `text` cut or padded to exactly `columns` on screen.
pub(super) fn fit(text: &str, columns: usize) -> String {
    let mut out = truncate(text, columns);
    let used = width(&out);
    out.extend(std::iter::repeat_n(' ', columns.saturating_sub(used)));
    out
}

fn hit(click: Click) -> HitId {
    HitId::Do(Action::Manage(click))
}

// ----- new chat or room --------------------------------------------------------------

pub fn draw_new(frame: &mut Frame, app: &mut App, area: Rect, form: &mut NewPopup) {
    match form.stage {
        Stage::Choose => chooser(frame, app, area, form),
        Stage::Create | Stage::CreateSpace => create(frame, app, area, form),
        Stage::Browse => browse(frame, app, area, form),
        Stage::Preview => preview(frame, app, area, form),
        Stage::Join | Stage::Direct | Stage::Invite => one_field(frame, app, area, form),
    }
}

fn chooser(frame: &mut Frame, app: &mut App, area: Rect, form: &NewPopup) {
    let count = Stage::CHOICES.len() as u16;
    let Some(rect) = centered(area, 58, 3 * count + 3) else {
        return;
    };
    let inner = popup(
        frame,
        app,
        area,
        rect,
        Stage::Choose.title(),
        "↑↓ select · Enter · Esc close",
    );
    let theme = app.theme.clone();
    // Three rows each where there is room, otherwise packed.
    let pitch = if inner.height >= 3 * count { 3 } else { 2 };
    for (n, stage) in Stage::CHOICES.into_iter().enumerate() {
        let selected = form.choice == n;
        let top = 1.min(inner.height.saturating_sub(2 * count)) + n as u16 * pitch;
        let title = row(inner, top);
        let summary = row(inner, top + 1);
        let marker = if selected { "› " } else { "  " };
        let style = if selected {
            theme.style(Role::TitleFocused)
        } else {
            theme.style(Role::Text)
        };
        frame.render_widget(
            Line::from(vec![
                Span::styled(marker, theme.style(Role::TitleFocused)),
                Span::styled(format!("{} ", n + 1), theme.style(Role::StatusKey)),
                Span::styled(stage.title(), style),
            ]),
            title,
        );
        frame.render_widget(
            Line::from(Span::styled(
                format!(
                    "    {}",
                    truncate(stage.summary(), inner.width.saturating_sub(4) as usize)
                ),
                theme.style(Role::Muted),
            )),
            summary,
        );
        for rect in [title, summary] {
            let id = hit(Click::Stage(stage));
            if app.hover == Some(id) && !selected {
                frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
            }
            app.hits.add(rect, id);
        }
    }
}

/// Error, or what is being waited for, under a form.
fn message(frame: &mut Frame, theme: &Theme, form: &NewPopup, inner: Rect, top: u16) {
    let (text, role) = match (&form.busy, &form.error) {
        (Some(busy), _) => ((*busy).to_owned(), Role::Muted),
        (None, Some(error)) => (crate::text::sanitize(error), Role::Error),
        (None, None) => return,
    };
    let top = top.min(inner.height);
    frame.render_widget(
        Paragraph::new(text)
            .style(theme.style(role))
            .wrap(Wrap { trim: false }),
        Rect::new(inner.x, inner.y + top, inner.width, inner.height - top),
    );
}

fn footer(form: &NewPopup) -> &'static str {
    if form.busy.is_some() {
        "Esc close"
    } else if form.stage == Stage::Browse {
        if form.single {
            "↑↓ pick · Tab server · Enter preview · Esc close"
        } else {
            "↑↓ pick · Tab server · Enter preview · Esc back"
        }
    } else if form.stage == Stage::Preview {
        if form.single && !form.preview_from_browse {
            "Tab move · Enter confirm · Esc close"
        } else {
            "Tab move · Enter confirm · Esc back"
        }
    } else if matches!(form.stage, Stage::Direct | Stage::Invite) && !form.user_results.is_empty() {
        if form.single {
            "↑↓ pick · Enter · Esc close"
        } else {
            "↑↓ pick · Enter · Esc back"
        }
    } else if form.single {
        "Enter continue · Esc close"
    } else {
        "Enter continue · Esc back"
    }
}

fn one_field(frame: &mut Frame, app: &mut App, area: Rect, form: &mut NewPopup) {
    let show_search = matches!(form.stage, Stage::Direct | Stage::Invite)
        && form.busy.is_none()
        && (!form.user_results.is_empty() || form.user_searching);
    let list_rows = if show_search {
        form.user_results.len().clamp(1, USER_SEARCH_MAX_ROWS) as u16
    } else {
        0
    };
    let height = 11 + list_rows + u16::from(show_search);
    let Some(rect) = centered(area, 66, height) else {
        return;
    };
    let inner = popup(frame, app, area, rect, form.stage.title(), footer(form));
    let theme = app.theme.clone();
    let busy = form.busy.is_some();
    let (hint, label, action) = match form.stage {
        Stage::Join => (
            "An address like #room:server or !id:server, or a matrix.to link pasted as it is."
                .to_owned(),
            "Address",
            " Join ",
        ),
        Stage::Invite => (
            format!(
                "Who to invite to {}? Search by name or enter @name:server.",
                form.room.as_ref().map_or("this room", |(_, name)| name)
            ),
            "User ID",
            " Invite ",
        ),
        _ => (
            "Search by name or enter a user ID like @name:server. Any existing chat is opened."
                .to_owned(),
            "User ID",
            " Start chat ",
        ),
    };
    let hint_height = 2.min(inner.height);
    frame.render_widget(
        Paragraph::new(hint)
            .style(theme.style(Role::Muted))
            .wrap(Wrap { trim: true }),
        Rect::new(inner.x, inner.y, inner.width, hint_height),
    );
    let input = row(inner, 3);
    app.hits.add(input, hit(Click::Slot(0)));
    let value = match form.stage {
        Stage::Join => &mut form.address,
        _ => &mut form.user,
    };
    let (cursor, value_rect) = field(frame, &theme, input, label, value, false, !busy);
    if !busy {
        app.hits
            .add(value_rect, HitId::Input(InputTarget::NewField(0)));
    }
    if show_search {
        user_search_rows(frame, app, &theme, inner, form);
    }
    let btn_row = if show_search { 5 + list_rows } else { 5 };
    let buttons = row(inner, btn_row);
    if !busy && buttons.width > 0 {
        let w = (width(action) as u16).min(buttons.width);
        let rect = Rect::new(buttons.x, buttons.y, w, 1);
        button(frame, &theme, rect, action, true);
        app.hits.add(rect, hit(Click::Submit));
    }
    message(frame, &theme, form, inner, btn_row + 2);
    if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
}

fn browse(frame: &mut Frame, app: &mut App, area: Rect, form: &mut NewPopup) {
    let Some(rect) = centered(area, 76, 24) else {
        return;
    };
    let inner = popup(frame, app, area, rect, form.stage.title(), footer(form));
    let theme = app.theme.clone();
    let busy = form.busy.is_some();

    let search_rect = row(inner, NewPopup::DIR_SEARCH as u16);
    app.hits
        .add(search_rect, hit(Click::Slot(NewPopup::DIR_SEARCH)));
    let (search_cursor, search_val) = field(
        frame,
        &theme,
        search_rect,
        "Search",
        &mut form.dir_query,
        false,
        !busy && form.focus == NewPopup::DIR_SEARCH,
    );
    if !busy {
        app.hits.add(
            search_val,
            HitId::Input(InputTarget::NewField(NewPopup::DIR_SEARCH)),
        );
    }

    let server_rect = row(inner, NewPopup::DIR_SERVER as u16);
    app.hits
        .add(server_rect, hit(Click::Slot(NewPopup::DIR_SERVER)));
    let (server_cursor, server_val) = field(
        frame,
        &theme,
        server_rect,
        "Server",
        &mut form.dir_server,
        false,
        !busy && form.focus == NewPopup::DIR_SERVER,
    );
    if !busy {
        app.hits.add(
            server_val,
            HitId::Input(InputTarget::NewField(NewPopup::DIR_SERVER)),
        );
    }
    if form.dir_server.as_str().is_empty()
        && (busy || form.focus != NewPopup::DIR_SERVER)
        && let Some(hs) = &form.dir_default_server
        && server_val.width > 0
    {
        let hint = truncate(
            &format!("this server ({})", sanitize_line(hs)),
            server_val.width as usize,
        );
        frame.render_widget(
            Line::from(Span::styled(hint, theme.style(Role::Muted))),
            server_val,
        );
    }

    let status_rect = row(inner, 2);
    let (status, status_role) = if form.dir_searching {
        ("Searching public rooms…".to_owned(), Role::Muted)
    } else if let Some(err) = &form.dir_error {
        (sanitize_line(err), Role::Error)
    } else if form.dir_results.is_empty() {
        ("No matching public rooms found.".to_owned(), Role::Muted)
    } else {
        let plus = if form.dir_has_more { "+" } else { "" };
        let more = if form.dir_loading_more {
            " · loading more…"
        } else {
            ""
        };
        (
            format!("{}{} public rooms{more}", form.dir_results.len(), plus),
            Role::Muted,
        )
    };
    frame.render_widget(
        Line::from(Span::styled(
            truncate(&status, status_rect.width as usize),
            theme.style(status_role),
        )),
        status_rect,
    );

    let list_top = 3u16;
    let avail = inner.height.saturating_sub(list_top) as usize;
    let per_page = (avail / 2).max(1);
    form.dir_page = per_page;
    let len = form.dir_results.len();
    if len == 0 {
        form.dir_selected = 0;
        form.dir_scroll = 0;
    } else {
        if form.dir_selected >= len {
            form.dir_selected = len - 1;
        }
        if std::mem::take(&mut form.dir_reveal) {
            if form.dir_selected < form.dir_scroll {
                form.dir_scroll = form.dir_selected;
            } else if form.dir_selected >= form.dir_scroll + per_page {
                form.dir_scroll = form.dir_selected + 1 - per_page;
            }
        }
        let max_scroll = len.saturating_sub(per_page);
        if form.dir_scroll > max_scroll {
            form.dir_scroll = max_scroll;
        }
    }

    for (offset, idx) in (form.dir_scroll..len).take(per_page).enumerate() {
        let top = list_top + offset as u16 * 2;
        if top >= inner.height {
            break;
        }
        let r1 = row(inner, top);
        let r2 = row(inner, top + 1);
        let entry = &form.dir_results[idx];
        let selected = idx == form.dir_selected;
        let hovered = matches!(
            app.hover,
            Some(HitId::Do(Action::Manage(Click::BrowseRow(h)))) if h == idx
        );
        let marker = if selected { "› " } else { "  " };
        let clean_name = entry.name.as_deref().map(sanitize_line);
        let clean_alias = entry.alias.as_deref().map(sanitize_line);
        let clean_id = sanitize_line(&entry.room_id);
        let title_str = clean_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(clean_alias.as_deref())
            .unwrap_or(&clean_id);

        let mut badges: Vec<(&str, Role)> = Vec::new();
        if entry.joined {
            badges.push((" [joined]", Role::StatusOk));
        } else if entry.knock {
            badges.push((" [ask to join]", Role::Muted));
        }
        if entry.world_readable {
            badges.push((" [world readable]", Role::Muted));
        }
        let members_str = format!(" {}m", entry.joined_members);
        let reserved =
            width(&members_str) + badges.iter().map(|(b, _)| width(b)).sum::<usize>() + 6; // marker(2) + tile(2) + space(1) + safety(1)

        let title_budget = (r1.width as usize).saturating_sub(reserved).max(4);
        let title_shown = truncate(title_str, title_budget);
        let mut used = width(marker) + 3 + width(&title_shown);
        let alias_extra = clean_alias
            .as_deref()
            .filter(|a| *a != title_str && !a.is_empty());

        let name_style = if selected {
            theme.style(Role::TitleFocused)
        } else if hovered {
            theme.style(Role::Hover)
        } else {
            theme.style(Role::Text)
        };
        let mut spans = vec![
            Span::styled(marker, theme.style(Role::TitleFocused)),
            avatar_tile(&theme, &entry.room_id, title_str),
            Span::raw(" "),
            Span::styled(title_shown, name_style),
        ];
        if let Some(alias) = alias_extra {
            let room_for_alias =
                (r1.width as usize).saturating_sub(used + reserved.saturating_sub(6) + 2);
            if room_for_alias >= 6 {
                let a = truncate(alias, room_for_alias);
                used += 1 + width(&a);
                spans.push(Span::raw(" "));
                spans.push(Span::styled(a, theme.style(Role::Muted)));
            }
        }
        for (badge, role) in badges {
            if used + width(badge) + width(&members_str) <= r1.width as usize {
                used += width(badge);
                spans.push(Span::styled(badge, theme.style(role)));
            }
        }
        let gap = (r1.width as usize).saturating_sub(used + width(&members_str));
        if gap > 0 {
            spans.push(Span::raw(" ".repeat(gap)));
        }
        spans.push(Span::styled(members_str, theme.style(Role::Muted)));
        frame.render_widget(Line::from(spans), r1);

        if r2.height > 0 {
            let clean_topic = entry
                .topic
                .as_deref()
                .map(sanitize_line)
                .filter(|t| !t.is_empty());
            let sub = if let Some(topic) = clean_topic {
                topic
            } else if alias_extra.is_none() && clean_name.as_deref().is_some_and(|n| !n.is_empty())
            {
                clean_id.clone()
            } else {
                String::new()
            };
            if !sub.is_empty() {
                let text = format!(
                    "     {}",
                    truncate(&sub, r2.width.saturating_sub(5) as usize)
                );
                frame.render_widget(Line::from(Span::styled(text, theme.style(Role::Muted))), r2);
            }
            if hovered && !selected {
                frame.buffer_mut().set_style(r2, theme.style(Role::Hover));
            }
        }
        if hovered && !selected {
            frame.buffer_mut().set_style(r1, theme.style(Role::Hover));
        }
        if !busy {
            app.hits.add(r1, hit(Click::BrowseRow(idx)));
            if r2.height > 0 {
                app.hits.add(r2, hit(Click::BrowseRow(idx)));
            }
        }
    }

    if let Some(cursor) = search_cursor.or(server_cursor) {
        frame.set_cursor_position(cursor);
    }
}

fn preview(frame: &mut Frame, app: &mut App, area: Rect, form: &mut NewPopup) {
    let inner_w = area.width.min(66).saturating_sub(4).max(1) as usize;
    let has_reason = form.preview_has_reason();
    let msg_text = form
        .busy
        .map(str::to_owned)
        .or_else(|| form.error.as_deref().map(crate::text::sanitize));
    let msg_lines = msg_text
        .as_deref()
        .map_or(0u16, |t| wrap(t, inner_w).len().clamp(1, 3) as u16);
    let body_rows: u16 = if form.preview_loading {
        4
    } else if let Some(p) = &form.preview {
        let topic_rows = p
            .topic
            .as_deref()
            .map(sanitize_line)
            .filter(|t| !t.is_empty())
            .map_or(0u16, |t| wrap(&t, inner_w).len().min(3) as u16 + 1);
        2 + topic_rows + 6
    } else {
        4
    };
    let tail_rows = if msg_lines > 0 { 1 + msg_lines } else { 0 };
    let desired_h = (body_rows + u16::from(has_reason) * 2 + 1 + tail_rows + 2).max(7);
    let Some(rect) = centered(area, 66, desired_h) else {
        return;
    };
    let inner = popup(frame, app, area, rect, form.stage.title(), footer(form));
    let theme = app.theme.clone();
    let busy = form.busy.is_some();
    let mut y = 0u16;

    if form.preview_loading {
        let target = truncate(&sanitize_line(&form.preview_target), inner.width as usize);
        frame.render_widget(
            Line::from(Span::styled(target, theme.style(Role::TitleFocused))),
            row(inner, y),
        );
        y += 2;
        frame.render_widget(
            Line::from(Span::styled(
                "Loading room preview…",
                theme.style(Role::Muted),
            )),
            row(inner, y),
        );
        y += 2;
    } else if let Some(p) = &form.preview {
        let clean_name = p.name.as_deref().map(sanitize_line);
        let clean_alias = p.canonical_alias.as_deref().map(sanitize_line);
        let clean_id = sanitize_line(&p.room_id);
        let title_str = clean_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(clean_alias.as_deref())
            .unwrap_or(&clean_id);
        let membership_badge = match p.membership {
            PreviewMembership::Joined => Some((" · already joined", Role::StatusOk)),
            PreviewMembership::Invited => Some((" · invited", Role::TitleFocused)),
            PreviewMembership::Knocked => Some((" · request pending", Role::Muted)),
            PreviewMembership::Banned => Some((" · banned", Role::Error)),
            PreviewMembership::None => None,
        };
        let badge_w = membership_badge.map_or(0, |(b, _)| width(b));
        let mut head = vec![
            avatar_tile(&theme, &p.room_id, title_str),
            Span::raw(" "),
            Span::styled(
                truncate(
                    title_str,
                    (inner.width as usize).saturating_sub(3 + badge_w),
                ),
                theme.style(Role::TitleFocused).add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some((badge, role)) = membership_badge {
            head.push(Span::styled(badge, theme.style(role)));
        }
        frame.render_widget(Line::from(head), row(inner, y));
        y += 2;

        if let Some(topic) = &p.topic {
            let clean_topic = sanitize_line(topic);
            if !clean_topic.is_empty() {
                let non_topic = 10 + usize::from(has_reason) * 2;
                let max_topic = (inner.height as usize).saturating_sub(non_topic).min(3);
                let lines = wrap(&clean_topic, inner.width.max(1) as usize);
                for line in lines.into_iter().take(max_topic) {
                    if y + 5 < inner.height {
                        frame.render_widget(
                            Line::from(Span::styled(line, theme.style(Role::Muted))),
                            row(inner, y),
                        );
                        y += 1;
                    }
                }
                y += 1;
            }
        }

        let join_rule_str = p.join_rule.label();
        let (enc_str, enc_role) = match p.encrypted {
            Some(true) => ("on", Role::StatusOk),
            Some(false) => ("off · not encrypted", Role::Text),
            None => ("unknown", Role::Muted),
        };
        let history_str = if p.world_readable {
            "world readable (anyone can read)"
        } else {
            "members only"
        };
        let address = clean_alias
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| clean_id.clone());
        let facts: [(&str, String, Role); 5] = [
            ("Address", address, Role::Text),
            ("Members", p.joined_members.to_string(), Role::Text),
            ("Join rule", join_rule_str.to_owned(), Role::Text),
            ("Encryption", enc_str.to_owned(), enc_role),
            ("History", history_str.to_owned(), Role::Text),
        ];
        const LABEL_W: usize = 13;
        for (label, value, role) in facts {
            if y + usize::from(form.preview_has_reason()) as u16 * 2 + 2 >= inner.height {
                break;
            }
            let r = row(inner, y);
            let shown = truncate(&value, (r.width as usize).saturating_sub(LABEL_W));
            frame.render_widget(
                Line::from(vec![
                    Span::styled(format!("{label:<LABEL_W$}"), theme.style(Role::Muted)),
                    Span::styled(shown, theme.style(role)),
                ]),
                r,
            );
            y += 1;
        }
        y += 1;
    } else {
        let target = truncate(&sanitize_line(&form.preview_target), inner.width as usize);
        frame.render_widget(
            Line::from(Span::styled(target, theme.style(Role::TitleFocused))),
            row(inner, y),
        );
        y += 2;
        frame.render_widget(
            Line::from(Span::styled(
                truncate(
                    "Preview unavailable; you can still try joining or asking to join.",
                    inner.width as usize,
                ),
                theme.style(Role::Muted),
            )),
            row(inner, y),
        );
        y += 2;
    }

    let mut cursor = None;
    if form.preview_has_reason() && y < inner.height {
        let reason_rect = row(inner, y);
        app.hits.add(reason_rect, hit(Click::Slot(0)));
        let (c, val_rect) = field(
            frame,
            &theme,
            reason_rect,
            "Reason",
            &mut form.preview_reason,
            false,
            !busy && form.focus == 0,
        );
        if !busy {
            app.hits
                .add(val_rect, HitId::Input(InputTarget::NewField(0)));
        }
        cursor = c;
        y += 2;
    }

    let btn_row = row(inner, y);
    if !busy && btn_row.width > 0 {
        let base = usize::from(form.preview_has_reason());
        let mut x = btn_row.x;
        for (idx, btn) in form.preview_buttons().into_iter().enumerate() {
            let label = btn.label();
            let w = (width(label) as u16).min(btn_row.right().saturating_sub(x));
            if w == 0 {
                break;
            }
            let r = Rect::new(x, btn_row.y, w, 1);
            let focused = form.focus == base + idx;
            button(frame, &theme, r, label, focused);
            app.hits.add(r, hit(Click::PreviewBtn(idx)));
            x = x.saturating_add(w + 2);
        }
    }
    message(frame, &theme, form, inner, y + 2);
    if let Some(c) = cursor {
        frame.set_cursor_position(c);
    }
}

fn user_search_rows(frame: &mut Frame, app: &mut App, theme: &Theme, inner: Rect, form: &NewPopup) {
    if form.user_results.is_empty() {
        let r = row(inner, 4);
        if r.width > 0 {
            frame.render_widget(
                Line::from(Span::styled("  Searching…", theme.style(Role::Muted))),
                r,
            );
        }
        return;
    }
    for (i, person) in form
        .user_results
        .iter()
        .take(USER_SEARCH_MAX_ROWS)
        .enumerate()
    {
        let r = row(inner, 4 + i as u16);
        if r.width == 0 {
            continue;
        }
        let selected = form.user_selected == Some(i);
        let hovered = matches!(
            app.hover,
            Some(HitId::UserSearchRow(h) | HitId::UserSearchInfo(h)) if h == i
        );
        let marker = if selected { "› " } else { "  " };
        let clean_name = person
            .display_name
            .as_deref()
            .map(sanitize_line)
            .filter(|s| !s.is_empty());
        let clean_id = sanitize_line(&person.user_id);
        let tile_text = clean_name.as_deref().unwrap_or(&clean_id);
        let mut used = 2usize;
        let mut spans = vec![Span::styled(marker, theme.style(Role::TitleFocused))];
        if r.width > 5 {
            spans.push(super::image::avatar_tile(theme, &person.user_id, tile_text));
            spans.push(Span::raw(" "));
            used += 3;
        }
        let show_info = (selected || hovered) && (r.width as usize) > used + 12;
        let info_w: usize = if show_info { 7 } else { 0 };
        let avail = (r.width as usize).saturating_sub(used + info_w);
        let text_style = if selected {
            theme.style(Role::TitleFocused)
        } else if hovered {
            theme.style(Role::Hover)
        } else {
            theme.style(Role::Text)
        };
        match clean_name {
            Some(name) => {
                let id_len = width(&clean_id);
                if avail > id_len + 4 {
                    let name_max = avail - id_len - 2;
                    spans.push(Span::styled(truncate(&name, name_max), text_style));
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(clean_id, theme.style(Role::Muted)));
                } else {
                    spans.push(Span::styled(truncate(&name, avail), text_style));
                }
            }
            None => {
                spans.push(Span::styled(truncate(&clean_id, avail), text_style));
            }
        }
        frame.render_widget(Line::from(spans), r);
        app.hits.add(r, HitId::UserSearchRow(i));
        if show_info {
            let info_rect = Rect::new(r.right().saturating_sub(6), r.y, 6, 1);
            let info_role = if app.hover == Some(HitId::UserSearchInfo(i)) {
                Role::Hover
            } else {
                Role::Pill
            };
            frame.render_widget(
                Line::from(Span::styled(" info ", theme.style(info_role))),
                info_rect,
            );
            app.hits.add(info_rect, HitId::UserSearchInfo(i));
        }
        if r.width > 5
            && let Some(url) = &person.avatar
        {
            let cells = Rect::new(r.x + 2, r.y, 2, 1);
            super::image::draw_avatar(frame, &mut app.images, theme, url, cells, 0, false);
        }
    }
}

/// A labelled switch with the same label column as `field`.
fn switch(
    frame: &mut Frame,
    theme: &Theme,
    rect: Rect,
    label: &str,
    value: Vec<Span<'static>>,
    focused: bool,
) {
    if rect.width == 0 {
        return;
    }
    let label_style = if focused {
        theme.style(Role::TitleFocused)
    } else {
        theme.style(Role::Muted)
    };
    let mut spans = vec![Span::styled(format!("{label:<13}"), label_style)];
    spans.extend(value);
    frame.render_widget(Line::from(spans), rect);
}

fn create(frame: &mut Frame, app: &mut App, area: Rect, form: &mut NewPopup) {
    let Some(rect) = centered(area, 66, 16) else {
        return;
    };
    let inner = popup(frame, app, area, rect, form.stage.title(), footer(form));
    let theme = app.theme.clone();
    let busy = form.busy.is_some();
    let focus = form.focus;
    let focused = |slot: usize| !busy && focus == slot;
    // Every other row when there is room.
    let pitch = if inner.height >= 12 { 2 } else { 1 };
    let at = |slot: usize| row(inner, (pitch - 1) + slot as u16 * pitch);
    let mut cursor = None;

    for (slot, label, value) in [
        (NewPopup::NAME, "Name", &mut form.name),
        (NewPopup::TOPIC, "Topic", &mut form.topic),
    ] {
        let rect = at(slot);
        app.hits.add(rect, hit(Click::Slot(slot)));
        let (pos, value_rect) = field(frame, &theme, rect, label, value, false, focused(slot));
        cursor = cursor.or(pos);
        if !busy {
            app.hits
                .add(value_rect, HitId::Input(InputTarget::NewField(slot)));
        }
    }

    let arrow = theme.style(Role::StatusKey);
    let access = at(NewPopup::ACCESS);
    let text = if form.public {
        "Public · anyone can join by its address"
    } else {
        "Private · by invitation only"
    };
    switch(
        frame,
        &theme,
        access,
        "Access",
        vec![
            Span::styled("◂ ", arrow),
            Span::styled(
                truncate(text, access.width.saturating_sub(17) as usize),
                theme.style(Role::Text),
            ),
            Span::styled(" ▸", arrow),
        ],
        focused(NewPopup::ACCESS),
    );
    app.hits.add(access, hit(Click::Flip(NewPopup::ACCESS)));

    let extra = at(NewPopup::EXTRA);
    if form.public {
        app.hits.add(extra, hit(Click::Slot(NewPopup::EXTRA)));
        if form.alias.starts_with('#') {
            let (pos, value_rect) = field(
                frame,
                &theme,
                extra,
                "Address",
                &mut form.alias,
                false,
                focused(NewPopup::EXTRA),
            );
            cursor = cursor.or(pos);
            if !busy {
                app.hits.add(
                    value_rect,
                    HitId::Input(InputTarget::NewField(NewPopup::EXTRA)),
                );
            }
        } else if extra.width > 0 {
            const LABEL: usize = 13;
            let room = (extra.width as usize).saturating_sub(LABEL + 2);
            let view = form.alias.view(room, false);
            let is_focused = focused(NewPopup::EXTRA);
            let label_style = if is_focused {
                theme.style(Role::TitleFocused)
            } else {
                theme.style(Role::Muted)
            };
            let underline = "_".repeat(room.saturating_sub(width(&view.text)));
            frame.render_widget(
                Line::from(vec![
                    Span::styled(format!("{:<LABEL$}", "Address"), label_style),
                    Span::raw("#"),
                    Span::raw(view.text),
                    Span::styled(underline, theme.style(Role::Border)),
                ]),
                extra,
            );
            if is_focused {
                let x =
                    extra.x + (LABEL + 1 + view.cursor_col).min(extra.width as usize - 1) as u16;
                cursor = cursor.or(Some(Position::new(x, extra.y)));
            }
            if !busy {
                app.hits.add(
                    Rect::new(
                        extra.x + LABEL as u16 + 1,
                        extra.y,
                        extra.width.saturating_sub(LABEL as u16 + 1),
                        1,
                    ),
                    HitId::Input(InputTarget::NewField(NewPopup::EXTRA)),
                );
            }
        }
    } else if form.stage == Stage::Create {
        let (dot, role) = if form.encrypted {
            ("● on", Role::StatusOk)
        } else {
            (
                "○ off · can be turned on in room settings later",
                Role::Muted,
            )
        };
        switch(
            frame,
            &theme,
            extra,
            "Encryption",
            vec![Span::styled(
                truncate(dot, extra.width.saturating_sub(13) as usize),
                theme.style(role),
            )],
            focused(NewPopup::EXTRA),
        );
        app.hits.add(extra, hit(Click::Flip(NewPopup::EXTRA)));
    }

    let buttons = at(NewPopup::BUTTON);
    if !busy && buttons.width > 0 {
        let label = if form.stage == Stage::CreateSpace {
            " Create space "
        } else {
            " Create room "
        };
        let w = (width(label) as u16).min(buttons.width);
        let rect = Rect::new(buttons.x, buttons.y, w, 1);
        button(frame, &theme, rect, label, form.focus == NewPopup::BUTTON);
        app.hits.add(rect, hit(Click::Submit));
    }
    message(
        frame,
        &theme,
        form,
        inner,
        (pitch - 1) + (NewPopup::BUTTON as u16 + 1) * pitch,
    );
    if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
}

// ----- room info ---------------------------------------------------------------------

const FACT_LABEL: usize = 12;
const ACTION_LABEL: usize = 18;
/// Lines of topic shown before it is cut.
const TOPIC_LINES: usize = 6;

/// One line of the scrolling part of the room info view.
enum Entry {
    Text(Line<'static>),
    /// Index into the popup's rows.
    Row(usize, InfoRow),
    /// The heading of the member list, with the filter.
    Members,
}

fn fact(theme: &Theme, label: &str, value: String, role: Role, room: usize) -> Entry {
    Entry::Text(Line::from(vec![
        Span::styled(format!("{label:<FACT_LABEL$}"), theme.style(Role::Muted)),
        Span::styled(
            truncate(&value, room.saturating_sub(FACT_LABEL)),
            theme.style(role),
        ),
    ]))
}

pub fn draw_info(frame: &mut Frame, app: &mut App, area: Rect, info: &mut InfoPopup) {
    let Some(rect) = centered(area, 74, area.height.saturating_sub(2).min(40)) else {
        return;
    };
    let rows = info.rows();
    info.selected = info.selected.min(rows.len().saturating_sub(1));
    let current = rows.get(info.selected).copied();
    let footer = if info.editing.is_some() {
        "Enter save · Esc cancel"
    } else if matches!(current, Some(InfoRow::Member(_))) {
        "Enter info · type to filter · Esc close"
    } else if matches!(current, Some(InfoRow::KnockRequest(_))) {
        "Enter accept · Del decline · Alt-B ban · Esc close"
    } else if matches!(current, Some(InfoRow::Child(_))) && info.can_edit_children() {
        "Enter open · Del remove · Esc close"
    } else {
        "↑↓ select · Enter · Esc close"
    };
    let title = if info.space {
        "Space info"
    } else {
        "Room info"
    };
    let inner = popup(frame, app, area, rect, title, footer);
    let theme = app.theme.clone();
    if inner.width < 8 || inner.height == 0 {
        return;
    }
    let room = inner.width as usize;

    // Servers accept nearly anything as a room id; the screen does not.
    let room_id = crate::text::sanitize_line(&info.room_id);
    // What scrolls: the room's description, what can be done, the members.
    let mut entries: Vec<Entry> = Vec::new();
    let name_style = theme.style(Role::TitleFocused).add_modifier(Modifier::BOLD);
    entries.push(Entry::Text(Line::from(Span::styled(
        truncate(&crate::text::sanitize_line(&info.name), room),
        name_style,
    ))));
    match &info.details {
        Some(details) => {
            if !details.topic.is_empty() {
                let mut topic = wrap(&crate::text::sanitize(&details.topic), room);
                if topic.len() > TOPIC_LINES {
                    topic.truncate(TOPIC_LINES);
                    if let Some(last) = topic.last_mut() {
                        // One sign that there is more, also where the line was full.
                        let cut = truncate(last, room.saturating_sub(1));
                        *last = format!("{}…", cut.trim_end_matches('…'));
                    }
                }
                entries.extend(topic.into_iter().map(|line| Entry::Text(Line::from(line))));
            }
            entries.push(Entry::Text(Line::default()));
            if !info.pending() {
                let (alias, role) = match &details.alias {
                    Some(alias) => (alias.clone(), Role::Text),
                    None => ("none".to_owned(), Role::Muted),
                };
                entries.push(fact(&theme, "Address", alias, role, room));
            }
            let id_label = if info.space { "Space ID" } else { "Room ID" };
            entries.push(fact(&theme, id_label, room_id.clone(), Role::Text, room));
            if !info.pending() && !info.space {
                let (text, role) = match details.encrypted {
                    Some(true) => ("on", Role::StatusOk),
                    Some(false) => ("off · messages here are not encrypted", Role::Text),
                    None => ("unknown", Role::Muted),
                };
                entries.push(fact(&theme, "Encryption", text.to_owned(), role, room));
            }
            let members = match (details.joined, details.invited) {
                (joined, 0) => format!("{joined}"),
                (joined, invited) => format!("{joined} · {invited} invited"),
            };
            entries.push(fact(&theme, "Members", members, Role::Text, room));
        }
        None => {
            entries.push(Entry::Text(Line::default()));
            entries.push(fact(&theme, "Room ID", room_id.clone(), Role::Text, room));
            if info.error.is_none() {
                entries.push(Entry::Text(Line::from(Span::styled(
                    "Loading…",
                    theme.style(Role::Muted),
                ))));
            }
        }
    }
    entries.push(Entry::Text(Line::default()));
    let mut knock_heading_done = false;
    let mut heading_done = false;
    let mut banned_heading_done = false;
    for (index, row) in rows.iter().enumerate() {
        if matches!(row, InfoRow::KnockRequest(_)) && !knock_heading_done {
            knock_heading_done = true;
            let count = info
                .details
                .as_ref()
                .map_or(0, |details| details.knock_requests.len());
            entries.push(Entry::Text(Line::default()));
            entries.push(Entry::Text(Line::from(Span::styled(
                format!("Join requests ({count})"),
                theme.style(Role::SidebarSection),
            ))));
        }
        if matches!(row, InfoRow::AddChild) {
            let (count, more) = info
                .details
                .as_ref()
                .and_then(|details| details.space.as_ref())
                .map_or((0, false), |space| (space.children.len(), space.more));
            let heading = if more {
                format!("Rooms (the first {count})")
            } else {
                format!("Rooms ({count})")
            };
            entries.push(Entry::Text(Line::default()));
            entries.push(Entry::Text(Line::from(Span::styled(
                heading,
                theme.style(Role::SidebarSection),
            ))));
        }
        if matches!(row, InfoRow::SettingsToggle) {
            entries.push(Entry::Text(Line::default()));
        }
        if matches!(row, InfoRow::Member(_)) {
            if !heading_done {
                heading_done = true;
                entries.push(Entry::Text(Line::default()));
                entries.push(Entry::Members);
            }
            if info.member(*row).is_some_and(|m| m.banned) && !banned_heading_done {
                banned_heading_done = true;
                entries.push(Entry::Text(Line::default()));
                entries.push(Entry::Text(Line::from(Span::styled(
                    "Banned",
                    theme.style(Role::SidebarSection),
                ))));
            }
        }
        entries.push(Entry::Row(index, *row));
    }
    if !heading_done
        && info
            .details
            .as_ref()
            .is_some_and(|d| !d.members.is_empty() || d.members_loading)
    {
        entries.push(Entry::Text(Line::default()));
        entries.push(Entry::Members);
        if !info.filter.is_empty() {
            entries.push(Entry::Text(Line::from(Span::styled(
                "  no member matches",
                theme.style(Role::Muted),
            ))));
        }
    }

    // The last row is kept for what is going on; inline moderation prompts sit just above it.
    let status = inner.height >= 4;
    let prompt_rows: usize = if let Some(chooser) = &info.role_chooser {
        2 + chooser.options.len() + usize::from(chooser.confirm_last_admin)
    } else if info.ban_prompt.is_some() {
        3
    } else {
        0
    };
    let max_prompt = inner.height.saturating_sub(u16::from(status) + 2) as usize;
    let prompt_rows = prompt_rows.min(max_prompt);
    let page = (inner.height as usize)
        .saturating_sub(usize::from(status) + prompt_rows)
        .max(1);
    // A question is only open while it can be read: without the line it stands on,
    // or with its keys cut off, the second press must not count.
    if let Some(prompt) = confirm_prompt(info)
        && (!status || width(confirm_keys(&prompt)) > room)
    {
        info.confirm = None;
    }
    // A view of another size (the window was resized) shows the selection again.
    info.reveal |= info.page != page;
    info.page = page;
    // What has to be in view: the selected row, or, while a filter is typed that
    // nobody matches, the line it is typed on. A row chosen since is shown.
    let typing_blind = info.typing_blind();
    let selected_line = entries.iter().position(|entry| match entry {
        Entry::Members => typing_blind,
        Entry::Row(index, _) => !typing_blind && *index == info.selected,
        Entry::Text(_) => false,
    });
    if std::mem::take(&mut info.reveal)
        && let Some(line) = selected_line
    {
        if info.selected == 0 && line < page && !typing_blind {
            // The first row: show the description above it as well.
            info.scroll = 0;
        } else if line < info.scroll + 1 {
            // With the line above it, which may be the members' heading.
            info.scroll = line.saturating_sub(1);
        } else if line >= info.scroll + page {
            info.scroll = line + 1 - page;
        }
    }
    info.scroll = info.scroll.min(entries.len().saturating_sub(page));

    let busy = info.busy.is_some();
    let mut cursor = None;
    for (n, entry) in entries.iter().skip(info.scroll).take(page).enumerate() {
        let rect = Rect::new(inner.x, inner.y + n as u16, inner.width, 1);
        match entry {
            Entry::Text(line) => frame.render_widget(line, rect),
            Entry::Members => {
                let mut spans = vec![
                    Span::styled("Members", theme.style(Role::SidebarSection)),
                    Span::styled("  ⌕ ", theme.style(Role::StatusKey)),
                ];
                let note = match &info.details {
                    Some(details) if details.members_loading => "  loading…".to_owned(),
                    Some(details) if details.members_partial => {
                        format!("  only the {} known here", details.members.len())
                    }
                    Some(details) if details.members_error.is_some() => {
                        "  may be out of date".to_owned()
                    }
                    _ => String::new(),
                };
                let max_cols = room.saturating_sub(12);
                let hit_w = if info.filter.is_empty() {
                    spans.push(Span::styled(
                        "type to filter…",
                        theme.style(Role::Placeholder),
                    ));
                    rect.width.saturating_sub(11)
                } else {
                    let view = info.filter.view(max_cols, false);
                    let shown_w = width(&view.text);
                    if info.editing.is_none() {
                        let x = (rect.x + 11 + view.cursor_col as u16)
                            .min(rect.right().saturating_sub(1));
                        cursor = Some(Position::new(x, rect.y));
                    }
                    spans.push(Span::raw(view.text));
                    if note.is_empty() {
                        (max_cols + 1).min(rect.width.saturating_sub(11) as usize) as u16
                    } else {
                        (shown_w + 1).min(rect.width.saturating_sub(11) as usize) as u16
                    }
                };
                spans.push(Span::styled(note, theme.style(Role::Muted)));
                frame.render_widget(Line::from(spans), rect);
                if !busy && info.editing.is_none() {
                    app.hits.add(
                        Rect::new(rect.x + 11, rect.y, hit_w, 1),
                        HitId::Input(InputTarget::InfoFilter),
                    );
                }
            }
            Entry::Row(index, row) => {
                let selected = *index == info.selected;
                let at = info_row(frame, app, &theme, info, rect, *index, *row, selected, busy);
                cursor = at.or(cursor);
            }
        }
    }

    if let Some(prompt) = &mut info.ban_prompt
        && prompt_rows >= 2
    {
        if let Some(at) = draw_ban_prompt(
            frame,
            app,
            &theme,
            inner,
            status,
            prompt_rows,
            prompt,
            InputTarget::BanReason,
            hit(Click::Submit),
        ) {
            cursor = Some(at);
        }
    } else if let Some(chooser) = &info.role_chooser
        && prompt_rows >= 2
    {
        draw_role_chooser(
            frame,
            app,
            &theme,
            inner,
            status,
            prompt_rows,
            chooser,
            |idx| hit(Click::RoleOption(idx)),
        );
    }

    if status {
        let rect = row(inner, inner.height - 1);
        let more = if info.scroll + page < entries.len() {
            " ↓ more"
        } else {
            ""
        };
        let joined = info.selected_child().map(|child| app.child_joined(child));
        let (text, role) = status_text(info, current, joined);
        let space = room.saturating_sub(width(more));
        frame.render_widget(
            Line::from(vec![
                Span::styled(fit(&text, space), theme.style(role)),
                Span::styled(more, theme.style(Role::Muted)),
            ]),
            rect,
        );
    }
    if info.setting_view.is_some() {
        draw_setting_subview(frame, app, area, info);
    } else if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
}

/// The question that is open, if any. The keys come first, up to the `·`: a narrow
/// window may cut the explanation, never them (see `confirm_keys`).
fn confirm_prompt(info: &InfoPopup) -> Option<String> {
    let name = |user_id: &String| {
        info.details
            .iter()
            .flat_map(|details| &details.members)
            .find(|member| member.user_id == *user_id)
            .map(|member| member.name.clone())
            .or_else(|| {
                info.details
                    .iter()
                    .flat_map(|details| &details.knock_requests)
                    .find(|req| req.user_id == *user_id)
                    .map(|req| req.name.clone())
            })
            .unwrap_or_else(|| user_id.clone())
    };
    Some(match info.confirm.as_ref()? {
        Confirm::Leave => info.leave_confirm().to_owned(),
        Confirm::Remove(user_id) => format!(
            "Del again to remove, Esc to keep · {} will be out of the room",
            name(user_id)
        ),
        Confirm::DeclineKnock(user_id) => format!(
            "Del again to decline, Esc to keep · {} will not be invited",
            name(user_id)
        ),
        Confirm::Unban(user_id) => format!(
            "Del again to unban, Esc to keep · {} will be allowed back",
            name(user_id)
        ),
        Confirm::Ignore(user_id) => format!(
            "Confirm ignore, Esc to stay · {}'s messages and invites will be hidden",
            name(user_id)
        ),
        Confirm::Unignore(user_id) => format!(
            "Confirm unignore, Esc to stay · {}'s messages will show again",
            name(user_id)
        ),
        Confirm::NewChat(user_id) => format!(
            "Enter again to start one, Esc to stay · no chat with {} yet; they get an invitation",
            name(user_id)
        ),
        Confirm::RemoveFromSpace => format!(
            "Enter again to remove, Esc to keep · {} will no longer list this room",
            info.in_space
                .as_ref()
                .map_or("the space", |space| space.name.as_str())
        ),
        Confirm::RemoveChild(room_id) => format!(
            "Del again to remove, Esc to keep · {} will no longer be in this space",
            info.children()
                .iter()
                .find(|child| child.room_id == *room_id && !child.name.is_empty())
                .map_or("the room", |child| child.name.as_str())
        ),
    })
}

/// The bottom line: what is being asked, what is under way, what went wrong, or what
/// the selected row is for. `joined`: whether we are in the selected room of a space.
fn status_text(info: &InfoPopup, current: Option<InfoRow>, joined: Option<bool>) -> (String, Role) {
    // First, always: a question that can be answered must be the thing on screen.
    if let Some(prompt) = confirm_prompt(info) {
        return (crate::text::sanitize_line(&prompt), Role::Error);
    }
    if let Some(busy) = info.busy {
        return (busy.to_owned(), Role::Muted);
    }
    if let Some(error) = &info.error {
        return (crate::text::sanitize_line(error), Role::Error);
    }
    if let Some(notice) = &info.notice {
        return (crate::text::sanitize_line(notice), Role::StatusOk);
    }
    let members_error = info.details.as_ref().and_then(|d| d.members_error.as_ref());
    if let (Some(error), Some(InfoRow::Member(_))) = (members_error, current) {
        return (crate::text::sanitize_line(error), Role::Error);
    }
    if info.editing.is_some() {
        return ("Enter to save, Esc to cancel".to_owned(), Role::Muted);
    }
    if info.ban_prompt.is_some() {
        return ("Enter to ban, Esc to cancel".to_owned(), Role::Muted);
    }
    if info.role_chooser.is_some() {
        return (
            "↑↓ choose · Enter apply · Esc cancel".to_owned(),
            Role::Muted,
        );
    }
    let text = match current {
        Some(InfoRow::Favourite) => "Favourites are listed first in the room list.",
        Some(InfoRow::LowPriority) => "Low priority rooms are listed last in the room list.",
        Some(InfoRow::Notify) => "←→ choose which messages here notify you.",
        Some(InfoRow::Name | InfoRow::Topic) => "Enter to edit.",
        Some(InfoRow::Pinned) => "View and jump to pinned messages.",
        Some(InfoRow::Invite) => "Invite someone by user ID.",
        Some(InfoRow::OwnName) => "Set a display name just for this room (clear for default).",
        Some(InfoRow::AddToSpace) => "Pick one of your spaces to put this room into.",
        Some(InfoRow::RemoveFromSpace) => {
            return match &info.in_space {
                Some(space) if space.can_edit => (
                    format!("Takes this room out of {}; nobody leaves it.", space.name),
                    Role::Muted,
                ),
                Some(space) => (
                    format!(
                        "Read-only: you are not allowed to change the rooms of {}.",
                        space.name
                    ),
                    Role::Muted,
                ),
                None => (String::new(), Role::Muted),
            };
        }
        Some(InfoRow::AddChild) => {
            if info.can_edit_children() {
                "Pick one of your rooms to put into this space."
            } else {
                "Read-only: you are not allowed to add rooms to this space."
            }
        }
        Some(row @ InfoRow::Child(_)) => match info.child(row) {
            Some(child) => {
                return (
                    child_hint(child, joined.unwrap_or(false), info.can_edit_children()),
                    Role::Muted,
                );
            }
            None => "",
        },
        Some(InfoRow::Leave) if info.pending() => "",
        Some(InfoRow::Leave) => "Leaving also removes the room and its history from this device.",
        Some(InfoRow::SettingsToggle) => {
            if info.settings_open {
                "Enter or ← to collapse room settings."
            } else {
                "Enter or → to expand room settings."
            }
        }
        Some(InfoRow::Setting(setting)) => {
            let editable = info.setting_editable(setting);
            match setting {
                RoomSetting::JoinRule => {
                    let restricted = info
                        .details
                        .as_ref()
                        .and_then(|d| d.join_rule)
                        .is_some_and(|r| r.to_ruma().is_none());
                    if restricted {
                        "This room uses a restricted or custom join rule (read-only here)."
                    } else if editable {
                        "Enter to choose who can join (invite, knock, or public)."
                    } else {
                        "Read-only: you do not have permission to change who can join."
                    }
                }
                RoomSetting::History => {
                    if editable {
                        "Enter to choose who can read future message history."
                    } else {
                        "Read-only: you do not have permission to change history visibility."
                    }
                }
                RoomSetting::Encryption => {
                    if info.details.as_ref().and_then(|d| d.encrypted) == Some(true) {
                        "End-to-end encryption is on and cannot be turned off."
                    } else if editable {
                        "Enter to turn encryption on permanently (requires typing ENCRYPT)."
                    } else {
                        "Read-only: you do not have permission to enable encryption."
                    }
                }
                RoomSetting::Picture => {
                    if editable {
                        "Enter to set or remove the room picture."
                    } else {
                        "Read-only: you do not have permission to change the room picture."
                    }
                }
                RoomSetting::Addresses => {
                    if editable {
                        "Enter to manage main, alternate, and local room addresses."
                    } else {
                        "Enter to view addresses (read-only: you cannot edit room addresses)."
                    }
                }
                RoomSetting::Direct => {
                    "Enter or Space to mark or unmark this room as a direct chat."
                }
                RoomSetting::Permissions => {
                    if editable {
                        "Enter to change who can send, invite, kick, ban, redact, and edit."
                    } else {
                        "Enter to view who can send, invite, kick, ban, redact, and edit."
                    }
                }
            }
        }
        Some(row @ InfoRow::KnockRequest(_)) => match info.knock_request(row) {
            Some(req) => return (knock_hint(req), Role::Muted),
            None => "",
        },
        Some(row @ InfoRow::Member(_)) => match info.member(row) {
            Some(member) if member.own => {
                if member.role_editable {
                    "Alt-R: change your role"
                } else {
                    "This is you."
                }
            }
            Some(member) if member.banned => {
                if member.unbannable {
                    "Del: unban · Alt-I: ignore/unignore"
                } else {
                    "Alt-I: ignore/unignore"
                }
            }
            Some(member) => {
                return (member_hint(member), Role::Muted);
            }
            None => "",
        },
        None => "",
    };
    (text.to_owned(), Role::Muted)
}

fn child_hint(child: &SpaceChild, joined: bool, can_edit: bool) -> String {
    let mut parts = vec![if joined {
        "Enter: open"
    } else {
        "Enter: preview and join"
    }];
    if can_edit {
        parts.push(if child.suggested {
            "Space: not suggested"
        } else {
            "Space: suggested"
        });
        parts.push("Del: remove from the space");
    }
    parts.join(" · ")
}

fn member_hint(member: &RoomMember) -> String {
    let mut parts = vec!["Enter: user info"];
    if member.role_editable {
        parts.push("Alt-R: role");
    }
    if member.bannable {
        parts.push("Alt-B: ban");
    }
    parts.push("Alt-I: ignore/unignore");
    if member.removable {
        parts.push("Del: remove");
    }
    parts.join(" · ")
}

fn knock_hint(req: &RoomKnockRequest) -> String {
    let mut parts = Vec::new();
    if req.acceptable {
        parts.push("Enter: accept");
    }
    if req.declinable {
        parts.push("Del: decline");
    }
    if req.bannable {
        parts.push("Alt-B: decline & ban");
    }
    parts.join(" · ")
}

/// Draws one selectable row and registers what clicking it does. Returns where the
/// text cursor belongs if the row is being typed into.
#[expect(clippy::too_many_arguments)]
fn info_row(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    info: &mut InfoPopup,
    rect: Rect,
    index: usize,
    row: InfoRow,
    selected: bool,
    busy: bool,
) -> Option<Position> {
    let marker = Span::styled(
        if selected { "› " } else { "  " },
        theme.style(Role::TitleFocused),
    );
    let label_style = if selected {
        theme.style(Role::TitleFocused)
    } else {
        theme.style(Role::Text)
    };
    let hovered = matches!(
        app.hover,
        Some(HitId::Do(Action::Manage(
            Click::Row(i) | Click::Select(i) | Click::Step(i, _)
        ))) if i == index
    );
    let finish = |frame: &mut Frame, spans: Vec<Span<'static>>| {
        frame.render_widget(Line::from(spans), rect);
        if hovered && !selected {
            frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
        }
    };

    if let Some(req) = info.knock_request(row) {
        app.hits.add(rect, hit(Click::Select(index)));
        knock_row(
            frame,
            app,
            theme,
            req,
            rect,
            marker,
            label_style,
            selected && !busy,
        );
        if hovered && !selected {
            frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
        }
        return None;
    }

    if let Some(child) = info.child(row) {
        app.hits.add(rect, hit(Click::Select(index)));
        let can_edit = info.can_edit_children();
        child_row(
            frame,
            app,
            theme,
            child,
            can_edit,
            rect,
            marker,
            label_style,
            selected && !busy,
        );
        if hovered && !selected {
            frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
        }
        return None;
    }

    if let Some(member) = info.member(row) {
        app.hits.add(rect, hit(Click::Select(index)));
        member_row(
            frame,
            app,
            theme,
            member,
            rect,
            marker,
            label_style,
            selected && !busy,
        );
        if hovered && !selected {
            frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
        }
        return None;
    }

    // Name or topic while it is typed: the whole row is the input.
    if let Some((editing, text)) = &mut info.editing
        && *editing == row
    {
        let label = match row {
            InfoRow::Name => "Name",
            InfoRow::OwnName => "Your name",
            _ => "Topic",
        };
        let value_x = rect.x + 2 + ACTION_LABEL as u16;
        let space = (rect.width as usize).saturating_sub(2 + ACTION_LABEL + 1);
        let view = text.view(space, false);
        let x = (value_x + view.cursor_col as u16).min(rect.right().saturating_sub(1));
        let rest = "_".repeat(space.saturating_sub(width(&view.text)));
        finish(
            frame,
            vec![
                marker,
                Span::styled(format!("{label:<ACTION_LABEL$}"), label_style),
                Span::raw(view.text),
                Span::styled(rest, theme.style(Role::Border)),
            ],
        );
        app.hits.add(
            Rect::new(value_x, rect.y, rect.right().saturating_sub(value_x), 1),
            HitId::Input(InputTarget::InfoEdit(row)),
        );
        return Some(Position::new(x, rect.y));
    }

    if row == InfoRow::SettingsToggle {
        let arrow = if info.settings_open { "▾" } else { "▸" };
        let style = if selected {
            theme.style(Role::TitleFocused).add_modifier(Modifier::BOLD)
        } else {
            theme.style(Role::SidebarSection)
        };
        let label = format!("{arrow} Room settings");
        let space = (rect.width as usize).saturating_sub(2);
        finish(
            frame,
            vec![marker, Span::styled(truncate(&label, space), style)],
        );
        app.hits.add(rect, hit(Click::Row(index)));
        return None;
    }

    let (label, label_style) = match row {
        InfoRow::Favourite => ("Favourite", label_style),
        InfoRow::LowPriority => ("Low priority", label_style),
        InfoRow::Notify => ("Notifications", label_style),
        InfoRow::Pinned => ("Pinned messages", label_style),
        InfoRow::Name => ("Rename…", label_style),
        InfoRow::Topic => ("Change topic…", label_style),
        InfoRow::Invite => ("Invite someone…", label_style),
        InfoRow::OwnName => ("Name in room…", label_style),
        InfoRow::AddToSpace => ("Add to a space…", label_style),
        InfoRow::RemoveFromSpace => ("Remove from space", label_style),
        InfoRow::AddChild => ("Add a room…", label_style),
        InfoRow::Leave => (info.leave_label(), theme.style(Role::Error)),
        InfoRow::Setting(setting) => (
            match setting {
                RoomSetting::JoinRule => "  Who can join",
                RoomSetting::History => "  History",
                RoomSetting::Encryption => "  Encryption",
                RoomSetting::Picture => "  Room picture",
                RoomSetting::Addresses => "  Addresses",
                RoomSetting::Direct => "  Direct chat",
                RoomSetting::Permissions => "  Permissions",
            },
            label_style,
        ),
        InfoRow::SettingsToggle
        | InfoRow::KnockRequest(_)
        | InfoRow::Member(_)
        | InfoRow::Child(_) => ("", label_style),
    };
    let mut spans = vec![
        marker,
        Span::styled(format!("{label:<ACTION_LABEL$}"), label_style),
    ];
    app.hits.add(rect, hit(Click::Row(index)));
    let value_x = rect.x + 2 + ACTION_LABEL as u16;
    let space = (rect.width as usize).saturating_sub(2 + ACTION_LABEL);
    match row {
        InfoRow::RemoveFromSpace => {
            if let Some(in_space) = &info.in_space {
                // A space we may not change says so on the row, as "Add a room…" does.
                let (tag, role) = if in_space.can_edit {
                    ("", Role::Text)
                } else {
                    (" (read-only)", Role::Muted)
                };
                let name = truncate(&in_space.name, space.saturating_sub(width(tag)));
                spans.push(Span::styled(name, theme.style(role)));
                spans.push(Span::styled(truncate(tag, space), theme.style(Role::Muted)));
            }
        }
        InfoRow::AddChild if !info.can_edit_children() => {
            spans.push(Span::styled(
                truncate("(read-only)", space),
                theme.style(Role::Muted),
            ));
        }
        InfoRow::OwnName => {
            let name = info
                .details
                .as_ref()
                .and_then(|d| d.own_display_name.as_deref())
                .map(sanitize_line)
                .filter(|s| !s.is_empty());
            match name {
                Some(name) => spans.push(Span::styled(
                    truncate(&name, space),
                    theme.style(Role::Text),
                )),
                None => spans.push(Span::styled(
                    truncate("default", space),
                    theme.style(Role::Muted),
                )),
            }
        }
        InfoRow::Setting(setting) => {
            if let Some(details) = &info.details {
                let editable = info.setting_editable(setting);
                let (value, role) = match setting {
                    RoomSetting::JoinRule => {
                        let base = details.join_rule.map_or("unknown", |r| r.label());
                        if editable {
                            (base.to_owned(), Role::Text)
                        } else {
                            (format!("{base} (read-only)"), Role::Muted)
                        }
                    }
                    RoomSetting::History => {
                        let base = details.history_visibility.map_or("unknown", |h| h.label());
                        if editable {
                            (base.to_owned(), Role::Text)
                        } else {
                            (format!("{base} (read-only)"), Role::Muted)
                        }
                    }
                    RoomSetting::Encryption => match details.encrypted {
                        Some(true) => ("on · cannot be turned off".to_owned(), Role::StatusOk),
                        Some(false) if editable => ("off · turn on…".to_owned(), Role::Text),
                        Some(false) => ("off (read-only)".to_owned(), Role::Muted),
                        None => ("unknown (read-only)".to_owned(), Role::Muted),
                    },
                    RoomSetting::Picture => {
                        let base = if details.avatar_url.is_some() {
                            "set"
                        } else {
                            "none"
                        };
                        if editable {
                            let role = if details.avatar_url.is_some() {
                                Role::Text
                            } else {
                                Role::Muted
                            };
                            (format!("{base}…"), role)
                        } else {
                            (format!("{base} (read-only)"), Role::Muted)
                        }
                    }
                    RoomSetting::Addresses => {
                        let entries = address_entries(details);
                        let base = if let Some(main) = &details.canonical_alias {
                            let extra = entries.len().saturating_sub(1);
                            if extra > 0 {
                                format!("{main} (+{extra})")
                            } else {
                                main.clone()
                            }
                        } else if !entries.is_empty() {
                            format!("{} unpublished", entries.len())
                        } else {
                            "none".to_owned()
                        };
                        if !editable {
                            (format!("{base} (read-only)"), Role::Muted)
                        } else if entries.is_empty() {
                            (base, Role::Muted)
                        } else {
                            (base, Role::Text)
                        }
                    }
                    RoomSetting::Direct => {
                        if details.direct {
                            ("● yes".to_owned(), Role::StatusOk)
                        } else {
                            ("○ no".to_owned(), Role::Muted)
                        }
                    }
                    RoomSetting::Permissions => {
                        if editable {
                            ("edit…".to_owned(), Role::Text)
                        } else {
                            ("view (read-only)".to_owned(), Role::Muted)
                        }
                    }
                };
                spans.push(Span::styled(truncate(&value, space), theme.style(role)));
            }
        }
        InfoRow::Favourite | InfoRow::LowPriority => {
            let on = info.details.as_ref().map_or(
                if row == InfoRow::LowPriority {
                    info.kind == crate::model::RoomKind::LowPriority
                } else {
                    info.kind == crate::model::RoomKind::Favourite
                },
                |d| {
                    if row == InfoRow::LowPriority {
                        d.low_priority
                    } else {
                        d.favourite
                    }
                },
            );
            let (text, role) = if on {
                ("● on", Role::StatusOk)
            } else {
                ("○ off", Role::Muted)
            };
            spans.push(Span::styled(text, theme.style(role)));
        }
        InfoRow::Notify => {
            let level = info.details.as_ref().and_then(|d| d.notify);
            let text = level.map_or("", notify_label);
            let arrow = theme.style(Role::StatusKey);
            spans.push(Span::styled("◂ ", arrow));
            spans.push(Span::styled(text, theme.style(Role::Text)));
            spans.push(Span::styled(" ▸", arrow));
            if value_x + 4 + (width(text) as u16) <= rect.right() {
                app.hits.add(
                    Rect::new(value_x, rect.y, 2, 1),
                    hit(Click::Step(index, -1)),
                );
                app.hits.add(
                    Rect::new(value_x + 2 + width(text) as u16, rect.y, 2, 1),
                    hit(Click::Step(index, 1)),
                );
            }
        }
        _ => {}
    }
    finish(frame, spans);
    None
}

/// A room of a space: its name (or id, where nobody could name it), what it is,
/// and on the selected row what can be done.
#[expect(clippy::too_many_arguments)]
fn child_row(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    child: &SpaceChild,
    can_edit: bool,
    rect: Rect,
    marker: Span<'static>,
    name_style: ratatui::style::Style,
    with_buttons: bool,
) {
    let joined = app.child_joined(child);
    let room = (rect.width as usize).saturating_sub(2);
    let mut tags = Vec::new();
    if child.space {
        tags.push("space");
    }
    if child.suggested {
        tags.push("suggested");
    }
    if !joined {
        tags.push("not joined");
    }
    let mut tag = tags.join(" · ");
    let mut buttons: Vec<(&str, Click)> = Vec::new();
    if with_buttons {
        buttons.push((
            if joined { " open " } else { " preview " },
            Click::ChildOpen,
        ));
        if can_edit {
            buttons.push((
                if child.suggested {
                    " unsuggest "
                } else {
                    " suggest "
                },
                Click::ChildSuggest,
            ));
            buttons.push((" remove ", Click::ChildRemove));
        }
    }
    let mut buttons_width: usize = buttons.iter().map(|(label, _)| width(label) + 1).sum();
    let tag_width = width(&tag) + usize::from(!tag.is_empty());
    // What does not fit gives way: first the tag next to the buttons, then the buttons.
    if buttons_width + tag_width + 12 > room {
        if buttons_width > 0 && buttons_width + 12 <= room {
            tag.clear();
        } else {
            buttons.clear();
            buttons_width = 0;
            if tag_width + 12 > room {
                tag.clear();
            }
        }
    }
    let tag_width = width(&tag) + usize::from(!tag.is_empty());
    let left = room.saturating_sub(tag_width + buttons_width);
    let (name, style) = if child.name.is_empty() {
        (sanitize_line(&child.room_id), theme.style(Role::Muted))
    } else {
        (sanitize_line(&child.name), name_style)
    };
    let name = fit(&name, left);
    let role = if child.suggested {
        Role::StatusKey
    } else {
        Role::Muted
    };
    let used = 2 + width(&name);
    let pad = (rect.width as usize).saturating_sub(used + width(&tag) + buttons_width);
    let spans = vec![
        marker,
        Span::styled(name, style),
        Span::raw(" ".repeat(pad)),
        Span::styled(tag, theme.style(role)),
    ];
    frame.render_widget(Line::from(spans), rect);
    draw_pill_buttons(frame, app, theme, rect, buttons_width, &buttons);
}

#[expect(clippy::too_many_arguments)]
fn member_row(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    member: &RoomMember,
    rect: Rect,
    marker: Span<'static>,
    name_style: ratatui::style::Style,
    with_buttons: bool,
) {
    let ignored = app.is_user_ignored(&member.user_id);
    let room = (rect.width as usize).saturating_sub(2);
    // Right end: the role, "invited" or "ignored"; on the selected row what can be done.
    let mut tag = crate::manage::member_role_display(member.role, member.power_level).into_owned();
    if member.invited {
        tag = if tag.is_empty() {
            "invited".to_owned()
        } else {
            format!("{tag} · invited")
        };
    }
    if member.banned {
        tag = if tag.is_empty() {
            "banned".to_owned()
        } else {
            format!("{tag} · banned")
        };
    }
    if ignored {
        tag = if tag.is_empty() {
            "ignored".to_owned()
        } else {
            format!("{tag} · ignored")
        };
    }
    let mut buttons: Vec<(&str, Click)> = Vec::new();
    if with_buttons && !member.own {
        if !member.banned {
            buttons.push((" chat ", Click::MemberChat));
        }
        if member.role_editable {
            buttons.push((" role ", Click::MemberRole));
        }
        buttons.push((
            if ignored { " unignore " } else { " ignore " },
            Click::MemberIgnore,
        ));
        if member.bannable {
            buttons.push((" ban ", Click::MemberBan));
        }
        if member.removable {
            buttons.push((" remove ", Click::MemberRemove));
        }
        if member.unbannable {
            buttons.push((" unban ", Click::MemberUnban));
        }
    }
    if with_buttons && member.own && member.role_editable {
        buttons.push((" role ", Click::MemberRole));
    }
    let buttons_width: usize = buttons.iter().map(|(label, _)| width(label) + 1).sum();
    let mut buttons_width = buttons_width;
    let tag_width = width(&tag) + usize::from(!tag.is_empty());
    // What does not fit gives way: first the tag next to the buttons, then the buttons.
    if buttons_width + tag_width + 12 > room {
        if buttons_width > 0 && buttons_width + 12 <= room {
            tag.clear();
        } else {
            buttons.clear();
            buttons_width = 0;
            // Without buttons the tag may still be too much for a narrow row.
            if tag_width + 12 > room {
                tag.clear();
            }
        }
    }
    let tag_width = width(&tag) + usize::from(!tag.is_empty());
    let left = room.saturating_sub(tag_width + buttons_width);
    // The same on every row, whatever stands at its right end.
    let name_width = (room * 2 / 5).clamp(8, 26).min(left);
    let mut name = member.name.clone();
    if member.own {
        name.push_str(" (you)");
    }
    // Cut and padded by what they take on screen: names may hold wide characters.
    let name = format!("{} ", fit(&name, name_width.saturating_sub(1)));
    let id_width = left.saturating_sub(name_width);
    let id = fit(&member.user_id, id_width.saturating_sub(1));
    let used = 2 + width(&name) + width(&id);
    let mut spans = vec![
        marker,
        Span::styled(name, name_style),
        Span::styled(id, theme.style(Role::Muted)),
    ];
    let role = if ignored {
        Role::Error
    } else if member.role > crate::matrix::MemberRole::User {
        Role::StatusKey
    } else {
        Role::Muted
    };
    let pad = (rect.width as usize).saturating_sub(used + width(&tag) + buttons_width);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(tag, theme.style(role)));
    frame.render_widget(Line::from(spans), rect);
    draw_pill_buttons(frame, app, theme, rect, buttons_width, &buttons);
}

#[expect(clippy::too_many_arguments)]
fn knock_row(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    req: &RoomKnockRequest,
    rect: Rect,
    marker: Span<'static>,
    name_style: ratatui::style::Style,
    with_buttons: bool,
) {
    let room = (rect.width as usize).saturating_sub(2);
    let mut buttons: Vec<(&str, Click)> = Vec::new();
    if with_buttons {
        if req.acceptable {
            buttons.push((" accept ", Click::KnockAccept));
        }
        if req.declinable {
            buttons.push((" decline ", Click::KnockDecline));
        }
        if req.bannable {
            buttons.push((" ban ", Click::KnockBan));
        }
    }
    let mut buttons_width: usize = buttons.iter().map(|(label, _)| width(label) + 1).sum();
    if buttons_width + 12 > room {
        buttons.clear();
        buttons_width = 0;
    }
    let left = room.saturating_sub(buttons_width);
    let name_width = (room * 2 / 5).clamp(8, 22).min(left);
    let clean_name = sanitize_line(&req.name);
    let clean_id = sanitize_line(&req.user_id);
    let name = format!("{} ", fit(&clean_name, name_width.saturating_sub(1)));
    let rest_w = left.saturating_sub(name_width);
    let (id_part, reason_part) = match req.reason.as_deref().map(sanitize_line) {
        Some(reason) if !reason.is_empty() && rest_w > 12 => {
            let id_cap = (width(&clean_id) + 2).min(rest_w / 2).max(8);
            let id_str = format!("{} ", fit(&clean_id, id_cap.saturating_sub(1)));
            let r_space = rest_w.saturating_sub(width(&id_str));
            let r_str = if r_space >= 4 {
                fit(&format!("· {reason}"), r_space)
            } else {
                String::new()
            };
            (id_str, r_str)
        }
        _ => (fit(&clean_id, rest_w.saturating_sub(1)), String::new()),
    };
    let used = 2 + width(&name) + width(&id_part) + width(&reason_part);
    let mut spans = vec![
        marker,
        Span::styled(name, name_style),
        Span::styled(id_part, theme.style(Role::Muted)),
    ];
    if !reason_part.is_empty() {
        spans.push(Span::styled(reason_part, theme.style(Role::Text)));
    }
    let pad = (rect.width as usize).saturating_sub(used + buttons_width);
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    frame.render_widget(Line::from(spans), rect);
    draw_pill_buttons(frame, app, theme, rect, buttons_width, &buttons);
}

fn draw_pill_buttons(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    rect: Rect,
    buttons_width: usize,
    buttons: &[(&str, Click)],
) {
    let mut x = rect.right().saturating_sub(buttons_width as u16);
    for &(label, click) in buttons {
        let w = width(label) as u16;
        let button_rect = Rect::new(x, rect.y, w, 1);
        let id = hit(click);
        let role = if app.hover == Some(id) {
            Role::Hover
        } else {
            Role::Pill
        };
        frame.render_widget(
            Line::from(Span::styled(label, theme.style(role))),
            button_rect,
        );
        app.hits.add(button_rect, id);
        x += w + 1;
    }
}

/// Inline ban-reason prompt drawn at the bottom of Room Info and User Info.
#[expect(clippy::too_many_arguments)]
pub(super) fn draw_ban_prompt(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    inner: Rect,
    status: bool,
    prompt_rows: usize,
    prompt: &mut BanPrompt,
    reason_target: InputTarget,
    submit_hit: HitId,
) -> Option<Position> {
    let top = inner.bottom() - u16::from(status) - prompt_rows as u16;
    frame.render_widget(
        Line::from(Span::styled(
            "─".repeat(inner.width as usize),
            theme.style(Role::Border),
        )),
        Rect::new(inner.x, top, inner.width, 1),
    );
    let label_y = top + 1;
    let limit = inner.bottom() - u16::from(status);
    if label_y < limit {
        let head = format!("Ban {} — Reason (optional):", sanitize_line(&prompt.name));
        frame.render_widget(
            Line::from(Span::styled(
                fit(&head, inner.width as usize),
                theme.style(Role::Error).add_modifier(Modifier::BOLD),
            )),
            Rect::new(inner.x, label_y, inner.width, 1),
        );
    }
    let input_y = top + 2;
    if input_y >= limit {
        return None;
    }
    let btn = " [Ban] ";
    let btn_w = width(btn) as u16;
    let field_w = inner.width.saturating_sub(btn_w + 1);
    let max_cols = field_w.saturating_sub(2) as usize;
    let view = prompt.reason.view(max_cols, false);
    let field_text = if prompt.reason.is_empty() {
        Line::from(vec![
            Span::styled("› ", theme.style(Role::TitleFocused)),
            Span::styled("(optional)", theme.style(Role::Placeholder)),
        ])
    } else {
        Line::from(vec![
            Span::styled("› ", theme.style(Role::TitleFocused)),
            Span::styled(view.text, theme.style(Role::Text)),
        ])
    };
    frame.render_widget(field_text, Rect::new(inner.x, input_y, field_w, 1));
    app.hits.add(
        Rect::new(inner.x + 2, input_y, field_w.saturating_sub(2), 1),
        HitId::Input(reason_target),
    );
    let cursor = Position::new(
        (inner.x + 2 + view.cursor_col as u16).min(inner.x + field_w.saturating_sub(1)),
        input_y,
    );
    let btn_rect = Rect::new(inner.x + field_w + 1, input_y, btn_w, 1);
    let hovered = app.hover == Some(submit_hit);
    let bstyle = if hovered {
        theme
            .style(Role::Error)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        theme.style(Role::Error).add_modifier(Modifier::BOLD)
    };
    frame.render_widget(Line::from(Span::styled(btn, bstyle)), btn_rect);
    app.hits.add(btn_rect, submit_hit);
    Some(cursor)
}

/// Inline role picker drawn at the bottom of Room Info and User Info.
#[expect(clippy::too_many_arguments)]
pub(super) fn draw_role_chooser(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    inner: Rect,
    status: bool,
    prompt_rows: usize,
    chooser: &RoleChooser,
    option_hit: impl Fn(usize) -> HitId,
) {
    let top = inner.bottom() - u16::from(status) - prompt_rows as u16;
    frame.render_widget(
        Line::from(Span::styled(
            "─".repeat(inner.width as usize),
            theme.style(Role::Border),
        )),
        Rect::new(inner.x, top, inner.width, 1),
    );
    let head = format!("Change role for {}", sanitize_line(&chooser.name));
    frame.render_widget(
        Line::from(Span::styled(
            fit(&head, inner.width as usize),
            theme.style(Role::TitleFocused).add_modifier(Modifier::BOLD),
        )),
        Rect::new(inner.x, top + 1, inner.width, 1),
    );
    let mut y = top + 2;
    let limit = inner.bottom() - u16::from(status);
    for (idx, opt) in chooser.options.iter().enumerate() {
        if y >= limit {
            break;
        }
        let r = Rect::new(inner.x, y, inner.width, 1);
        let active = idx == chooser.selected;
        let hit_id = option_hit(idx);
        let hovered = app.hover == Some(hit_id);
        let style = if active {
            theme.style(Role::Pill)
        } else if hovered {
            theme.style(Role::Hover)
        } else {
            theme.style(Role::Text)
        };
        let (level, label) = *opt;
        let prefix = if active { "▸ " } else { "  " };
        let current_mark = if level == chooser.current_level {
            " (current)"
        } else {
            ""
        };
        let line = format!("{prefix}{label} ({level}){current_mark}");
        frame.buffer_mut().set_style(r, style);
        frame.render_widget(
            Line::from(Span::styled(fit(&line, inner.width as usize), style)),
            r,
        );
        app.hits.add(r, hit_id);
        y += 1;
    }
    if chooser.confirm_last_admin && y < limit {
        frame.render_widget(
            Line::from(Span::styled(
                fit(
                    "Last admin! Press Enter again to demote yourself.",
                    inner.width as usize,
                ),
                theme.style(Role::Error).add_modifier(Modifier::BOLD),
            )),
            Rect::new(inner.x, y, inner.width, 1),
        );
    }
}

fn draw_setting_subview(frame: &mut Frame, app: &mut App, area: Rect, info: &mut InfoPopup) {
    let Some(mut subview) = info.setting_view.take() else {
        return;
    };
    let theme = app.theme.clone();
    match &mut subview {
        SettingSubview::JoinRule { selected } => {
            let current = info.details.as_ref().and_then(|d| d.join_rule);
            draw_setting_picker(
                frame,
                app,
                &theme,
                area,
                info,
                "Who can join",
                "↑↓ choose · Enter apply · Esc back",
                &RoomJoinRule::EDITABLE,
                *selected,
                |r| Some(*r) == current,
                |r| r.label(),
                "Anyone can join a Public room; Knock lets people ask to join.",
                false,
            );
        }
        SettingSubview::History { selected } => {
            let current = info.details.as_ref().and_then(|d| d.history_visibility);
            draw_setting_picker(
                frame,
                app,
                &theme,
                area,
                info,
                "Who can read history",
                "↑↓ choose · Enter apply · Esc back",
                &RoomHistory::ALL,
                *selected,
                |h| Some(*h) == current,
                |h| h.label(),
                "WARNING: Changing history visibility only affects future messages.",
                true,
            );
        }
        SettingSubview::EncryptConfirm {
            confirm,
            focus,
            error,
        } => {
            draw_encrypt_subview(
                frame,
                app,
                &theme,
                area,
                info,
                confirm,
                *focus,
                error.as_deref(),
            );
        }
        SettingSubview::Picture {
            path,
            focus,
            error,
            notice,
        } => {
            draw_picture_subview(
                frame,
                app,
                &theme,
                area,
                info,
                path,
                *focus,
                error.as_deref(),
                notice.as_deref(),
            );
        }
        SettingSubview::Addresses {
            input,
            focus,
            selected,
            confirm_delete,
            error,
            notice,
        } => {
            draw_addresses_subview(
                frame,
                app,
                &theme,
                area,
                info,
                input,
                *focus,
                selected,
                confirm_delete.as_deref(),
                error.as_deref(),
                notice.as_deref(),
            );
        }
        SettingSubview::Permissions {
            draft,
            selected,
            error,
        } => {
            draw_permissions_subview(
                frame,
                app,
                &theme,
                area,
                info,
                draft,
                *selected,
                error.as_deref(),
            );
        }
    }
    info.setting_view = Some(subview);
}

#[expect(clippy::too_many_arguments)]
fn draw_setting_picker<T>(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    area: Rect,
    info: &InfoPopup,
    title: &str,
    footer: &str,
    options: &[T],
    selected: usize,
    is_current: impl Fn(&T) -> bool,
    label_of: impl Fn(&T) -> &'static str,
    note: &str,
    warning_bold: bool,
) {
    let Some(rect) = centered(area, 66, 13) else {
        return;
    };
    let inner = popup(frame, app, area, rect, title, footer);
    let room = inner.width as usize;
    if room < 10 || inner.height < 4 {
        return;
    }
    let note_style = if warning_bold {
        theme.style(Role::Error).add_modifier(Modifier::BOLD)
    } else {
        theme.style(Role::Muted)
    };
    let mut y = 0u16;
    for line in wrap(note, room).into_iter().take(2) {
        if y + 2 >= inner.height {
            break;
        }
        frame.render_widget(Line::from(Span::styled(line, note_style)), row(inner, y));
        y += 1;
    }
    y += 1;
    let busy = info.busy.is_some();
    let status_row = inner.height - 1;
    for (idx, opt) in options.iter().enumerate() {
        if y >= status_row {
            break;
        }
        let r = row(inner, y);
        let active = idx == selected;
        let hit_id = hit(Click::SettingChoice(idx));
        let hovered = app.hover == Some(hit_id);
        let style = if active {
            theme.style(Role::Pill)
        } else if hovered {
            theme.style(Role::Hover)
        } else {
            theme.style(Role::Text)
        };
        let prefix = if active { "▸ " } else { "  " };
        let cur = if is_current(opt) { " (current)" } else { "" };
        let text = format!("{prefix}{}{cur}", label_of(opt));
        frame.buffer_mut().set_style(r, style);
        frame.render_widget(Line::from(Span::styled(fit(&text, room), style)), r);
        if !busy {
            app.hits.add(r, hit_id);
        }
        y += 1;
    }
    draw_subview_status(
        frame,
        theme,
        row(inner, status_row),
        info.busy,
        info.error.as_deref(),
        info.notice.as_deref(),
        "",
    );
}

#[expect(clippy::too_many_arguments)]
fn draw_encrypt_subview(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    area: Rect,
    info: &InfoPopup,
    confirm: &mut crate::lineedit::LineEdit,
    focus: usize,
    error: Option<&str>,
) {
    let Some(rect) = centered(area, 66, 12) else {
        return;
    };
    let inner = popup(
        frame,
        app,
        area,
        rect,
        "Turn encryption ON",
        "Tab switch · Enter confirm · Esc cancel",
    );
    if inner.width < 12 || inner.height < 6 {
        return;
    }
    let room = inner.width as usize;
    let busy = info.busy.is_some();
    frame.render_widget(
        Line::from(Span::styled(
            fit("Turning on encryption cannot be undone.", room),
            theme.style(Role::Error).add_modifier(Modifier::BOLD),
        )),
        row(inner, 0),
    );
    frame.render_widget(
        Line::from(Span::styled(
            fit("You will not be able to turn encryption off later.", room),
            theme.style(Role::Text),
        )),
        row(inner, 1),
    );
    let (cursor, input_rect) = field(
        frame,
        theme,
        row(inner, 3),
        "Type ENCRYPT",
        confirm,
        false,
        !busy && focus == 0,
    );
    if !busy {
        app.hits
            .add(input_rect, HitId::Input(InputTarget::RoomSettingEncrypt));
    }
    draw_slot_buttons(
        frame,
        app,
        theme,
        row(inner, 5),
        focus,
        busy,
        &[(1, "[Turn encryption on]"), (2, "[Cancel]")],
    );
    draw_subview_status(
        frame,
        theme,
        row(inner, inner.height - 1),
        info.busy,
        error.or(info.error.as_deref()),
        None,
        "Type ENCRYPT in capitals, then press Enter.",
    );
    if let Some(at) = cursor {
        frame.set_cursor_position(at);
    }
}

#[expect(clippy::too_many_arguments)]
fn draw_picture_subview(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    area: Rect,
    info: &InfoPopup,
    path: &mut crate::lineedit::LineEdit,
    focus: usize,
    error: Option<&str>,
    notice: Option<&str>,
) {
    let Some(rect) = centered(area, 68, 13) else {
        return;
    };
    let inner = popup(
        frame,
        app,
        area,
        rect,
        "Room picture",
        "Tab path/focus · Enter apply · Esc back",
    );
    if inner.width < 14 || inner.height < 7 {
        return;
    }
    let room = inner.width as usize;
    let busy = info.busy.is_some();
    let avatar_url = info.details.as_ref().and_then(|d| d.avatar_url.clone());
    let has_avatar = avatar_url.is_some();

    let header_rect = row(inner, 0);
    let tile_rect = Rect::new(header_rect.x, header_rect.y, 2.min(header_rect.width), 1);
    let label = if has_avatar {
        format!("{} · Picture set", sanitize_line(&info.name))
    } else {
        format!("{} · No picture set", sanitize_line(&info.name))
    };
    frame.render_widget(
        Line::from(vec![
            avatar_tile(theme, &info.room_id, &info.name),
            Span::raw(" "),
            Span::styled(
                truncate(&label, room.saturating_sub(3)),
                theme.style(Role::Text),
            ),
        ]),
        header_rect,
    );
    if let Some(url) = avatar_url.as_deref()
        && tile_rect.width >= 2
    {
        draw_avatar(frame, &mut app.images, theme, url, tile_rect, 0, false);
    }

    let (cursor, input_rect) = field(
        frame,
        theme,
        row(inner, 2),
        "Image path",
        path,
        false,
        !busy && focus == 0,
    );
    if !busy {
        app.hits
            .add(input_rect, HitId::Input(InputTarget::RoomSettingAvatar));
    }
    frame.render_widget(
        Line::from(Span::styled(
            fit(
                "Drop an image file or press Tab inside Image path to complete.",
                room,
            ),
            theme.style(Role::Muted),
        )),
        row(inner, 3),
    );
    let mut buttons = vec![(1, "[Set picture]")];
    if has_avatar {
        buttons.push((2, "[Remove picture]"));
    }
    buttons.push((3, "[Back]"));
    draw_slot_buttons(frame, app, theme, row(inner, 5), focus, busy, &buttons);
    draw_subview_status(
        frame,
        theme,
        row(inner, inner.height - 1),
        info.busy,
        error.or(info.error.as_deref()),
        notice.or(info.notice.as_deref()),
        "Enter submits · Esc goes back",
    );
    if let Some(at) = cursor {
        frame.set_cursor_position(at);
    }
}

#[expect(clippy::too_many_arguments)]
fn draw_addresses_subview(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    area: Rect,
    info: &InfoPopup,
    input: &mut crate::lineedit::LineEdit,
    focus: usize,
    selected: &mut usize,
    confirm_delete: Option<&str>,
    error: Option<&str>,
    notice: Option<&str>,
) {
    let height = area.height.saturating_sub(2).clamp(12, 24);
    let Some(rect) = centered(area, 74, height) else {
        return;
    };
    let can_edit = info.setting_editable(RoomSetting::Addresses);
    let footer = if can_edit {
        "↑↓ list · Tab focus · m main · u unpublish · Del delete · Esc back"
    } else {
        "↑↓ list · Read-only · Esc back"
    };
    let inner = popup(frame, app, area, rect, "Room addresses", footer);
    if inner.width < 18 || inner.height < 7 {
        return;
    }
    let room = inner.width as usize;
    let busy = info.busy.is_some();
    let Some(details) = &info.details else {
        return;
    };
    let main_text = details
        .canonical_alias
        .as_deref()
        .map_or_else(|| "none".to_owned(), sanitize_line);
    frame.render_widget(
        Line::from(vec![
            Span::styled("Main address: ", theme.style(Role::Muted)),
            Span::styled(
                truncate(&main_text, room.saturating_sub(14)),
                theme.style(if details.canonical_alias.is_some() {
                    Role::Text
                } else {
                    Role::Muted
                }),
            ),
        ]),
        row(inner, 0),
    );
    let mut y = 1u16;
    if let Some(err) = &details.local_aliases_error {
        frame.render_widget(
            Line::from(Span::styled(truncate(err, room), theme.style(Role::Error))),
            row(inner, y),
        );
        y += 1;
    }
    let mut cursor = None;
    if can_edit {
        let (at, input_rect) = field(
            frame,
            theme,
            row(inner, y),
            "Address",
            input,
            false,
            !busy && focus == 0,
        );
        cursor = at;
        if !busy {
            app.hits
                .add(input_rect, HitId::Input(InputTarget::RoomSettingAlias));
        }
        y += 1;
        let mut buttons = vec![(1, "[Create local]"), (2, "[Publish alt]")];
        if details.canonical_alias.is_some() {
            buttons.push((3, "[Clear main]"));
        }
        draw_slot_buttons(frame, app, theme, row(inner, y), focus, busy, &buttons);
        y += 1;
        if let Some(target) = confirm_delete {
            frame.render_widget(
                Line::from(Span::styled(
                    truncate(
                        &format!("Delete local {target}? Press Del or click again to confirm."),
                        room,
                    ),
                    theme.style(Role::Error).add_modifier(Modifier::BOLD),
                )),
                row(inner, y),
            );
            y += 1;
        }
    }
    y += 1;
    let status_row = inner.height - 1;
    if y < status_row {
        frame.render_widget(
            Line::from(Span::styled(
                "Published & local addresses",
                theme.style(Role::SidebarSection),
            )),
            row(inner, y),
        );
        y += 1;
    }

    let entries = address_entries(details);
    *selected = (*selected).min(entries.len().saturating_sub(1));
    let avail = status_row.saturating_sub(y) as usize;
    if entries.is_empty() && avail > 0 {
        frame.render_widget(
            Line::from(Span::styled(
                "  No published or local addresses yet.",
                theme.style(Role::Muted),
            )),
            row(inner, y),
        );
    } else if avail > 0 {
        let skip = if *selected >= avail {
            *selected + 1 - avail
        } else {
            0
        };
        let own_hs = details.own_homeserver.as_deref();
        for (idx, entry) in entries.iter().enumerate().skip(skip).take(avail) {
            let r = row(inner, y);
            let is_sel = idx == *selected;
            let row_hit = hit(Click::AddressRow(idx));
            let hovered = app.hover == Some(row_hit);
            let mut tags = Vec::new();
            if entry.is_canonical {
                tags.push("main");
            }
            if entry.is_alt {
                tags.push("alt");
            }
            if entry.is_local {
                tags.push("local");
            }
            let tag_text = if tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", tags.join("·"))
            };
            let local_on_own_hs =
                entry.is_local && own_hs.is_none_or(|hs| entry.alias.ends_with(&format!(":{hs}")));
            let mut pills = Vec::new();
            if can_edit && is_sel && !busy {
                if !entry.is_canonical {
                    pills.push(("[Make main]", Click::AddressMakeMain(idx)));
                    if !entry.is_alt {
                        pills.push(("[Publish alt]", Click::AddressPublishAlt(idx)));
                    }
                }
                if entry.is_canonical || entry.is_alt {
                    pills.push(("[Unpublish]", Click::AddressUnpublish(idx)));
                }
                if local_on_own_hs {
                    pills.push(("[Delete local]", Click::AddressDeleteLocal(idx)));
                }
            }
            let pills_w: usize = if pills.is_empty() {
                0
            } else {
                pills.iter().map(|(l, _)| width(l) + 1).sum()
            };
            let alias_room = (r.width as usize).saturating_sub(2 + width(&tag_text) + pills_w);
            let line_style = if is_sel {
                theme.style(Role::TitleFocused)
            } else {
                theme.style(Role::Text)
            };
            let marker = if is_sel { "› " } else { "  " };
            frame.render_widget(
                Line::from(vec![
                    Span::styled(marker, theme.style(Role::TitleFocused)),
                    Span::styled(truncate(&entry.alias, alias_room), line_style),
                    Span::styled(tag_text, theme.style(Role::Muted)),
                ]),
                r,
            );
            if hovered && !is_sel {
                frame.buffer_mut().set_style(r, theme.style(Role::Hover));
            }
            if !busy {
                app.hits.add(r, row_hit);
                if !pills.is_empty() {
                    draw_pill_buttons(frame, app, theme, r, pills_w, &pills);
                }
            }
            y += 1;
        }
    }

    draw_subview_status(
        frame,
        theme,
        row(inner, status_row),
        info.busy,
        error.or(info.error.as_deref()),
        notice.or(info.notice.as_deref()),
        if can_edit {
            "Use #name (your homeserver) or #name:server."
        } else {
            "Read-only: you do not have permission to edit room addresses."
        },
    );
    if let Some(at) = cursor {
        frame.set_cursor_position(at);
    }
}

#[expect(clippy::too_many_arguments)]
fn draw_permissions_subview(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    area: Rect,
    info: &InfoPopup,
    draft: &crate::matrix::RoomPermissions,
    selected: usize,
    error: Option<&str>,
) {
    let Some(rect) = centered(area, 74, 15) else {
        return;
    };
    let editable = info.setting_editable(RoomSetting::Permissions);
    let footer = if editable {
        "↑↓ row · ←→ level (0 / 50 / 100) · Enter save · Esc back"
    } else {
        "Read-only · Esc back"
    };
    let inner = popup(frame, app, area, rect, "Room permissions", footer);
    if inner.width < 20 || inner.height < 10 {
        return;
    }
    let room = inner.width as usize;
    let busy = info.busy.is_some();
    let Some(details) = &info.details else {
        return;
    };
    let live_err = if editable {
        validate_permissions_draft(details, draft).err()
    } else {
        None
    };

    let (banner, banner_role) = if !editable {
        (
            "Read-only: you do not have permission to edit room permissions.".to_owned(),
            Role::Muted,
        )
    } else if details.own_is_creator {
        (
            "You hold creator infinite power in this room.".to_owned(),
            Role::StatusOk,
        )
    } else {
        (
            format!(
                "Your power level: {} · edits cannot exceed it or lock you out.",
                details.own_power_level
            ),
            Role::Muted,
        )
    };
    frame.render_widget(
        Line::from(Span::styled(fit(&banner, room), theme.style(banner_role))),
        row(inner, 0),
    );

    for (row_idx, action) in PermAction::ALL.iter().enumerate() {
        let r = row(inner, 2 + row_idx as u16);
        let is_sel = selected == row_idx;
        let val = action.get(draft);
        let mut x = r.x;
        let marker = if is_sel { "› " } else { "  " };
        let label = format!("{:<17}", action.label());
        frame.render_widget(
            Line::from(vec![
                Span::styled(marker, theme.style(Role::TitleFocused)),
                Span::styled(
                    label,
                    theme.style(if is_sel {
                        Role::TitleFocused
                    } else {
                        Role::Text
                    }),
                ),
            ]),
            Rect::new(x, r.y, r.width.min(19), 1),
        );
        x += 19;
        for &(level, level_label) in &PermAction::LEVELS {
            let pill = format!("[{level_label}]");
            let pw = width(&pill) as u16;
            if x + pw > r.right() {
                break;
            }
            let pr = Rect::new(x, r.y, pw, 1);
            let hit_id = hit(Click::PermSet(row_idx, level));
            let active = val == level;
            let hovered = editable && app.hover == Some(hit_id);
            let role = if active && editable {
                Role::Pill
            } else if active {
                Role::TitleFocused
            } else if hovered {
                Role::Hover
            } else if editable {
                Role::StatusKey
            } else {
                Role::Muted
            };
            frame.render_widget(Line::from(Span::styled(pill, theme.style(role))), pr);
            if editable && !busy {
                app.hits.add(pr, hit_id);
            }
            x += pw + 1;
        }
        if !PermAction::LEVELS.iter().any(|(l, _)| *l == val) && x + 6 <= r.right() {
            let custom = format!("({val})");
            let cw = width(&custom) as u16;
            frame.render_widget(
                Line::from(Span::styled(custom, theme.style(Role::Muted))),
                Rect::new(x, r.y, cw.min(r.right() - x), 1),
            );
        }
    }

    let action_y = 2 + PermAction::ALL.len() as u16 + 1;
    let btn_row = row(inner, action_y);
    let mut buttons = Vec::new();
    let btn_focus = if selected >= PermAction::ALL.len() {
        selected - PermAction::ALL.len()
    } else {
        usize::MAX
    };
    if editable {
        buttons.push((0, "[Save]"));
        buttons.push((1, "[Back]"));
    } else {
        buttons.push((1, "[Back]"));
    }
    draw_slot_buttons(frame, app, theme, btn_row, btn_focus, busy, &buttons);

    draw_subview_status(
        frame,
        theme,
        row(inner, inner.height - 1),
        info.busy,
        error.or(live_err.as_deref()).or(info.error.as_deref()),
        info.notice.as_deref(),
        if editable {
            "Pick Default (0), Moderator (50), or Admin (100), then Save."
        } else {
            "Read-only view of the room's action power levels."
        },
    );
}

fn draw_slot_buttons(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    rect: Rect,
    focus: usize,
    busy: bool,
    buttons: &[(usize, &str)],
) {
    let mut x = rect.x;
    for &(slot, label) in buttons {
        let w = width(label) as u16;
        if x + w > rect.right() {
            break;
        }
        let r = Rect::new(x, rect.y, w, 1);
        let id = hit(Click::SettingSlot(slot));
        let focused = focus == slot;
        if !focused && app.hover == Some(id) {
            frame.render_widget(
                Line::from(Span::styled(label.to_owned(), theme.style(Role::Hover))),
                r,
            );
        } else {
            button(frame, theme, r, label, focused);
        }
        if !busy {
            app.hits.add(r, id);
        }
        x += w + 2;
    }
}

fn draw_subview_status(
    frame: &mut Frame,
    theme: &Theme,
    rect: Rect,
    busy: Option<&str>,
    error: Option<&str>,
    notice: Option<&str>,
    default_hint: &str,
) {
    let (text, role) = if let Some(busy) = busy {
        (busy.to_owned(), Role::Muted)
    } else if let Some(err) = error {
        (sanitize_line(err), Role::Error)
    } else if let Some(ok) = notice {
        (sanitize_line(ok), Role::StatusOk)
    } else {
        (default_hint.to_owned(), Role::Muted)
    };
    frame.render_widget(
        Line::from(Span::styled(
            fit(&text, rect.width as usize),
            theme.style(role),
        )),
        rect,
    );
}
