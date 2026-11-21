//! Drawing the User Info and Own Profile popups.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::hit::{HitId, InputTarget};
use super::image::{Avatar, avatar_tile, draw_avatar};
use super::modal::{button, centered, field, popup, row};
use super::roominfo::{draw_ban_prompt, draw_role_chooser, fit};
use crate::app::App;
use crate::manage::{confirm_keys, member_role_display};
use crate::matrix::UserVerifyStatus;
use crate::people::{OwnProfilePopup, UserInfoCtx, UserInfoPopup, UserInfoRow};
use crate::text::{sanitize_line, truncate, width};
use crate::theme::{Role, Theme};

const FACT_LABEL: usize = 12;

enum Entry {
    Header {
        name: String,
        avatar: Option<String>,
    },
    UserId {
        id: String,
    },
    Text(Line<'static>),
    Row(usize, UserInfoRow),
}

pub fn draw_user_info(frame: &mut Frame, app: &mut App, area: Rect, info: &mut UserInfoPopup) {
    let Some(rect) = centered(area, 68, area.height.saturating_sub(2).min(36)) else {
        return;
    };
    let ctx = app.user_info_ctx(&info.user_id);
    let rows = info.rows(&ctx);
    info.selected = info.selected.min(rows.len().saturating_sub(1));
    let current = rows.get(info.selected).copied();
    let footer =
        if info.ban_prompt.is_some() || info.role_chooser.is_some() || info.confirm.is_some() {
            "Enter apply · Esc cancel"
        } else {
            "↑↓ select · Enter · c copy ID · Esc close"
        };
    let inner = popup(frame, app, area, rect, "User info", footer);
    let theme = app.theme.clone();
    if inner.width < 10 || inner.height == 0 {
        return;
    }
    let room = inner.width as usize;

    let display_name = sanitize_line(&info.display_name());
    let user_id = sanitize_line(&info.user_id);
    let avatar = info.avatar_url().map(str::to_owned);

    let mut entries: Vec<Entry> = Vec::new();
    entries.push(Entry::Header {
        name: if display_name.is_empty() {
            user_id.clone()
        } else {
            display_name
        },
        avatar,
    });
    entries.push(Entry::UserId { id: user_id });
    entries.push(Entry::Text(Line::default()));

    let (verify_text, verify_role) = match info.verify_status() {
        UserVerifyStatus::Verified => ("verified", Role::StatusOk),
        UserVerifyStatus::Unverified => ("unverified", Role::Muted),
        UserVerifyStatus::Violation => ("keys changed — verify again", Role::Error),
        UserVerifyStatus::NoKeys => ("no encryption keys", Role::Muted),
        UserVerifyStatus::Unknown if info.loading => ("checking…", Role::Muted),
        UserVerifyStatus::Unknown => ("unknown", Role::Muted),
    };
    entries.push(fact(
        &theme,
        "Security",
        verify_text.to_owned(),
        verify_role,
        room,
    ));

    if ctx.ignored {
        entries.push(fact(
            &theme,
            "Ignored",
            "yes · messages and invitations hidden".to_owned(),
            Role::Error,
            room,
        ));
    }

    if let Some(member) = info.effective_member() {
        let mut parts: Vec<String> = Vec::new();
        let role = member_role_display(member.role, member.power_level);
        if !role.is_empty() {
            parts.push(role.into_owned());
        }
        if member.invited {
            parts.push("invited".to_owned());
        }
        if member.banned {
            parts.push("banned".to_owned());
        }
        if parts.is_empty() {
            parts.push("member".to_owned());
        }
        let role_color = if member.banned {
            Role::Error
        } else if member.role > crate::matrix::MemberRole::User {
            Role::StatusKey
        } else {
            Role::Text
        };
        entries.push(fact(&theme, "In room", parts.join(" · "), role_color, room));
    }

    if info.loading && info.profile.is_none() {
        entries.push(Entry::Text(Line::from(Span::styled(
            "Loading profile…",
            theme.style(Role::Muted),
        ))));
    }

    entries.push(Entry::Text(Line::default()));
    entries.push(Entry::Text(Line::from(Span::styled(
        "Actions",
        theme.style(Role::SidebarSection),
    ))));

    let mut rooms_heading_done = false;
    let shared_heading = |count: usize, incomplete: bool| {
        if incomplete {
            format!("Shared rooms ({count}, maybe more)")
        } else {
            format!("Shared rooms ({count})")
        }
    };
    for (index, item) in rows.iter().enumerate() {
        if matches!(item, UserInfoRow::SharedRoom(_)) && !rooms_heading_done {
            rooms_heading_done = true;
            entries.push(Entry::Text(Line::default()));
            entries.push(Entry::Text(Line::from(Span::styled(
                shared_heading(info.shared_rooms().len(), info.shared_rooms_incomplete()),
                theme.style(Role::SidebarSection),
            ))));
        }
        entries.push(Entry::Row(index, *item));
    }
    if !rooms_heading_done && info.shared_rooms_incomplete() {
        entries.push(Entry::Text(Line::default()));
        entries.push(Entry::Text(Line::from(Span::styled(
            shared_heading(0, true),
            theme.style(Role::SidebarSection),
        ))));
    }

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

    if let Some(prompt) = info.confirm_prompt()
        && (!status || width(confirm_keys(&prompt)) > room)
    {
        info.confirm = None;
    }

    info.reveal |= info.page != page;
    info.page = page;
    let selected_line = entries
        .iter()
        .position(|e| matches!(e, Entry::Row(index, _) if *index == info.selected));
    if std::mem::take(&mut info.reveal)
        && let Some(line) = selected_line
    {
        if info.selected == 0 && line < page {
            info.scroll = 0;
        } else if line < info.scroll + 1 {
            info.scroll = line.saturating_sub(1);
        } else if line >= info.scroll + page {
            info.scroll = line + 1 - page;
        }
    }
    info.scroll = info.scroll.min(entries.len().saturating_sub(page));

    let busy = info.busy.is_some();
    let mut cursor = None;
    for (n, entry) in entries.iter().skip(info.scroll).take(page).enumerate() {
        let r = Rect::new(inner.x, inner.y + n as u16, inner.width, 1);
        match entry {
            Entry::Header { name, avatar } => {
                let mut spans = Vec::new();
                let name_avail = if r.width > 4 {
                    spans.push(avatar_tile(&theme, &info.user_id, name));
                    spans.push(Span::raw(" "));
                    room.saturating_sub(3)
                } else {
                    room
                };
                spans.push(Span::styled(
                    truncate(name, name_avail),
                    theme.style(Role::TitleFocused).add_modifier(Modifier::BOLD),
                ));
                frame.render_widget(Line::from(spans), r);
                if r.width > 4
                    && let Some(url) = avatar
                {
                    let cells = Rect::new(r.x, r.y, 2, 1);
                    let avatar = Avatar::whole(cells);
                    draw_avatar(frame, &mut app.images, &theme, url, avatar, false);
                }
            }
            Entry::UserId { id } => {
                let copy_label = " [copy] ";
                let copy_w = width(copy_label) as u16;
                if r.width > copy_w + 4 {
                    let id_max = (r.width - copy_w - 1) as usize;
                    let id_shown = truncate(id, id_max);
                    let id_w = width(&id_shown) as u16;
                    frame.render_widget(
                        Line::from(Span::styled(id_shown, theme.style(Role::Muted))),
                        r,
                    );
                    let copy_rect = Rect::new(r.x + id_w + 1, r.y, copy_w, 1);
                    let copy_role = if app.hover == Some(HitId::UserInfoCopy) {
                        Role::Hover
                    } else {
                        Role::Pill
                    };
                    frame.render_widget(
                        Line::from(Span::styled(copy_label, theme.style(copy_role))),
                        copy_rect,
                    );
                    app.hits.add(copy_rect, HitId::UserInfoCopy);
                } else {
                    frame.render_widget(
                        Line::from(Span::styled(truncate(id, room), theme.style(Role::Muted))),
                        r,
                    );
                }
            }
            Entry::Text(line) => frame.render_widget(line, r),
            Entry::Row(index, item) => {
                let selected = *index == info.selected;
                let hovered = app.hover == Some(HitId::UserInfoRow(*index));
                let marker = Span::styled(
                    if selected { "› " } else { "  " },
                    theme.style(Role::TitleFocused),
                );
                let mut spans = vec![marker];
                let avail = room.saturating_sub(2);
                match item {
                    UserInfoRow::Action(action) => {
                        let disabled = info.action_disabled_reason(*action, &ctx).is_some();
                        let style = if disabled {
                            theme.style(Role::Muted)
                        } else if action.is_destructive() {
                            theme.style(Role::Error)
                        } else if selected {
                            theme.style(Role::TitleFocused)
                        } else {
                            theme.style(Role::Text)
                        };
                        spans.push(Span::styled(
                            truncate(action.label(ctx.ignored), avail),
                            style,
                        ));
                    }
                    UserInfoRow::SharedRoom(room_idx) => {
                        if let Some(shared) = info.shared_rooms().get(*room_idx) {
                            let tag = match (shared.direct, shared.invited) {
                                (true, true) => " · direct · invited",
                                (true, false) => " · direct",
                                (false, true) => " · invited",
                                (false, false) => "",
                            };
                            let tag_w = width(tag);
                            let name_max = if avail > tag_w + 8 {
                                avail - tag_w
                            } else {
                                avail
                            };
                            let name_style = if selected {
                                theme.style(Role::TitleFocused)
                            } else {
                                theme.style(Role::Text)
                            };
                            spans.push(Span::styled(
                                truncate(&sanitize_line(&shared.name), name_max),
                                name_style,
                            ));
                            if avail > tag_w + 8 && !tag.is_empty() {
                                spans.push(Span::styled(tag, theme.style(Role::Muted)));
                            }
                        }
                    }
                }
                frame.render_widget(Line::from(spans), r);
                if hovered && !selected {
                    frame.buffer_mut().set_style(r, theme.style(Role::Hover));
                }
                if !busy {
                    app.hits.add(r, HitId::UserInfoRow(*index));
                }
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
            HitId::UserInfoBanSubmit,
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
            HitId::UserInfoRoleOption,
        );
    }

    if status {
        let status_rect = row(inner, inner.height - 1);
        let more = if info.scroll + page < entries.len() {
            " ↓ more"
        } else {
            ""
        };
        let (text, role) = user_status_text(info, current, &ctx);
        let left = truncate(&text, room.saturating_sub(width(more)));
        let gap = room.saturating_sub(width(&left) + width(more));
        frame.render_widget(
            Line::from(vec![
                Span::styled(left, theme.style(role)),
                Span::raw(" ".repeat(gap)),
                Span::styled(more, theme.style(Role::Muted)),
            ]),
            status_rect,
        );
    }

    if let Some(at) = cursor {
        frame.set_cursor_position(at);
    }
}

fn user_status_text(
    info: &UserInfoPopup,
    current: Option<UserInfoRow>,
    ctx: &UserInfoCtx,
) -> (String, Role) {
    if let Some(prompt) = info.confirm_prompt() {
        return (sanitize_line(&prompt), Role::Error);
    }
    if let Some(busy) = info.busy {
        return (busy.to_owned(), Role::Muted);
    }
    if let Some(error) = &info.error {
        return (sanitize_line(error), Role::Error);
    }
    if let Some(notice) = &info.notice {
        return (sanitize_line(notice), Role::StatusOk);
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
    match current {
        Some(UserInfoRow::Action(action)) => {
            if let Some(reason) = info.action_disabled_reason(action, ctx) {
                (reason.to_owned(), Role::Muted)
            } else {
                ("Enter to run · c to copy user ID.".to_owned(), Role::Muted)
            }
        }
        Some(UserInfoRow::SharedRoom(_)) => {
            ("Enter to open this shared room.".to_owned(), Role::Muted)
        }
        None => ("c to copy user ID.".to_owned(), Role::Muted),
    }
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

pub fn draw_own_profile(frame: &mut Frame, app: &mut App, area: Rect, form: &mut OwnProfilePopup) {
    let Some(rect) = centered(area, 66, area.height.saturating_sub(2).min(14)) else {
        return;
    };
    let inner = popup(
        frame,
        app,
        area,
        rect,
        "Your profile",
        "Tab next · Enter · Esc close",
    );
    let theme = app.theme.clone();
    if inner.width < 12 || inner.height < 6 {
        return;
    }
    let room = inner.width as usize;
    let busy = form.busy.is_some();
    let eff_name = sanitize_line(&form.effective_name());
    let color_key = form.user_id.as_deref().unwrap_or(&eff_name);

    // Row 0: Avatar tile/picture + effective display name (+ user ID if space).
    let r0 = row(inner, 0);
    if r0.width > 0 {
        let mut spans = Vec::new();
        let avail = if r0.width > 4 {
            spans.push(avatar_tile(&theme, color_key, &eff_name));
            spans.push(Span::raw(" "));
            room.saturating_sub(3)
        } else {
            room
        };
        let clean_id = form.user_id.as_deref().map(sanitize_line);
        match clean_id {
            Some(uid) if avail > width(&uid) + 8 => {
                let name_max = avail - width(&uid) - 2;
                spans.push(Span::styled(
                    truncate(&eff_name, name_max),
                    theme.style(Role::TitleFocused).add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::raw("  "));
                spans.push(Span::styled(uid, theme.style(Role::Muted)));
            }
            _ => {
                spans.push(Span::styled(
                    truncate(&eff_name, avail),
                    theme.style(Role::TitleFocused).add_modifier(Modifier::BOLD),
                ));
            }
        }
        frame.render_widget(Line::from(spans), r0);
        if r0.width > 4
            && let Some(url) = &form.current_avatar
        {
            let cells = Rect::new(r0.x, r0.y, 2, 1);
            let avatar = Avatar::whole(cells);
            draw_avatar(frame, &mut app.images, &theme, url, avatar, false);
        }
    }

    let mut cursor = None;

    // Row 2: Display name input.
    let r2 = row(inner, 2);
    let (at, name_rect) = field(
        frame,
        &theme,
        r2,
        "Display name",
        &mut form.display_name,
        false,
        !busy && form.focus == OwnProfilePopup::NAME_INPUT,
    );
    cursor = at.or(cursor);
    if !busy {
        app.hits
            .add(name_rect, HitId::Input(InputTarget::OwnProfileName));
    }

    // Row 3: Save name button.
    let r3 = row(inner, 3);
    if r3.width > 14 {
        let label = " [Save name] ";
        let w = (width(label) as u16).min(r3.width.saturating_sub(13));
        let btn_rect = Rect::new(r3.x + 13, r3.y, w, 1);
        button(
            frame,
            &theme,
            btn_rect,
            label,
            !busy && form.focus == OwnProfilePopup::SAVE_NAME,
        );
        if !busy {
            app.hits
                .add(btn_rect, HitId::OwnProfileSlot(OwnProfilePopup::SAVE_NAME));
        }
    }

    // Row 5: Avatar file path input.
    let r5 = row(inner, 5);
    let (at, path_rect) = field(
        frame,
        &theme,
        r5,
        "Avatar file",
        &mut form.avatar_path,
        false,
        !busy && form.focus == OwnProfilePopup::AVATAR_PATH,
    );
    cursor = at.or(cursor);
    if !busy {
        app.hits
            .add(path_rect, HitId::Input(InputTarget::OwnProfileAvatar));
    }

    // Row 6: Set avatar & optional Remove avatar buttons.
    let r6 = row(inner, 6);
    if r6.width > 14 {
        let mut x = r6.x + 13;
        let set_label = " [Set avatar] ";
        let set_w = (width(set_label) as u16).min(r6.right().saturating_sub(x));
        if set_w > 0 {
            let set_rect = Rect::new(x, r6.y, set_w, 1);
            button(
                frame,
                &theme,
                set_rect,
                set_label,
                !busy && form.focus == OwnProfilePopup::SET_AVATAR,
            );
            if !busy {
                app.hits
                    .add(set_rect, HitId::OwnProfileSlot(OwnProfilePopup::SET_AVATAR));
            }
            x += set_w + 1;
        }
        if form.current_avatar.is_some() && x < r6.right() {
            let rm_label = " [Remove avatar] ";
            let rm_w = (width(rm_label) as u16).min(r6.right().saturating_sub(x));
            if rm_w > 4 {
                let rm_rect = Rect::new(x, r6.y, rm_w, 1);
                let rm_style = if !busy && form.focus == OwnProfilePopup::REMOVE_AVATAR {
                    theme.style(Role::Pill)
                } else {
                    theme.style(Role::Error)
                };
                frame.render_widget(Line::from(Span::styled(rm_label, rm_style)), rm_rect);
                if !busy {
                    app.hits.add(
                        rm_rect,
                        HitId::OwnProfileSlot(OwnProfilePopup::REMOVE_AVATAR),
                    );
                }
            }
        }
    }

    // Row 8: Subtle hint when room permits.
    let r8 = row(inner, 8);
    if inner.height >= 10 && r8.width > 0 {
        frame.render_widget(
            Line::from(Span::styled(
                truncate("Tab completes image paths, or drop a picture here.", room),
                theme.style(Role::Muted),
            )),
            r8,
        );
    }

    // Bottom status row.
    if inner.height >= 4 {
        let status_rect = row(inner, inner.height - 1);
        let (msg, role) = if let Some(busy_msg) = form.busy {
            (busy_msg.to_owned(), Role::Muted)
        } else if let Some(err) = &form.error {
            (sanitize_line(err), Role::Error)
        } else if let Some(notice) = &form.notice {
            (sanitize_line(notice), Role::StatusOk)
        } else if form.user_id.is_none() {
            ("Sign in to save profile changes.".to_owned(), Role::Muted)
        } else {
            let hint = match form.focus {
                OwnProfilePopup::NAME_INPUT | OwnProfilePopup::SAVE_NAME => {
                    "Clear display name to reset to your localpart."
                }
                OwnProfilePopup::REMOVE_AVATAR => "Enter removes your current profile picture.",
                _ => "Enter PNG, JPEG, GIF, WebP or BMP file path.",
            };
            (hint.to_owned(), Role::Muted)
        };
        frame.render_widget(
            Line::from(Span::styled(truncate(&msg, room), theme.style(role))),
            status_rect,
        );
    }

    if let Some(at) = cursor {
        frame.set_cursor_position(at);
    }
    let _ = fit("", 0);
}
