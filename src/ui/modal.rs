//! Popups: login, recovery key, emoji picker.

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

use super::hit::{HitId, InputTarget};
use crate::action::Action;
use crate::app::App;
use crate::images::EMOTE_CELLS;
use crate::keymap::Context;
use crate::lineedit::LineEdit;
use crate::matrix::ReauthKind;
use crate::modal::{
    EMOJI_CELL, EmojiPicker, IgnoredUsersPopup, LoginForm, Modal, Pick, PinnedPopup,
    ReasonConfirmKind, ReasonConfirmPopup, RecoveryForm, ReportForm, Setting, SettingsMenu,
};
use crate::text::{sanitize, sanitize_line, truncate, width};
use crate::theme::{Role, Theme};

pub(super) const FIELD_LABEL: usize = 13;

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    // Taken out while drawing so the popup state and the rest of the app can be
    // borrowed independently.
    let Some(mut modal) = app.modal.take() else {
        return;
    };
    match &mut modal {
        Modal::Login(form) => login(frame, app, area, form),
        Modal::Recovery(form) => recovery(frame, app, area, form),
        Modal::Emoji(picker) => emoji(frame, app, area, picker),
        Modal::Settings(menu) => settings(frame, app, area, menu),
        Modal::Verify(popup) => super::verify::draw(frame, app, area, popup),
        Modal::Palette(palette) => super::palette::draw(frame, app, area, palette),
        Modal::Search(bar) => super::search::draw(frame, app, area, bar),
        Modal::GlobalSearch(search) => super::global_search::draw(frame, app, area, search),
        Modal::New(form) => super::roominfo::draw_new(frame, app, area, form),
        Modal::RoomInfo(info) => super::roominfo::draw_info(frame, app, area, info),
        Modal::UserInfo(popup) => super::people::draw_user_info(frame, app, area, popup),
        Modal::OwnProfile(popup) => super::people::draw_own_profile(frame, app, area, popup),
        Modal::Menu(menu) => super::menu::draw(frame, app, area, menu),
        Modal::Keybinds(editor) => super::keybinds::draw(frame, app, area, editor),
        Modal::IgnoredUsers(popup) => ignored_users(frame, app, area, popup),
        Modal::Report(form) => report(frame, app, area, form),
        Modal::Pinned(popup) => pinned(frame, app, area, popup),
        Modal::ReasonConfirm(confirm) => reason_confirm(frame, app, area, confirm),
        Modal::Sessions(popup) => super::devices::draw_sessions(frame, app, area, popup),
        Modal::Keys(popup) => super::devices::draw_keys(frame, app, area, popup),
        Modal::PollForm(form) => super::poll::draw_form(frame, app, area, form),
        Modal::PollVote(vote) => super::poll::draw_vote(frame, app, area, vote),
        Modal::Stickers(picker) => super::stickers::draw(frame, app, area, picker),
        Modal::Details(details) => super::details::draw(frame, app, area, details),
        Modal::Threads(list) => super::threads::draw(frame, app, area, list),
        Modal::Keywords(popup) => super::keywords::draw(frame, app, area, popup),
        Modal::Doctor(popup) => super::doctor::draw(frame, app, area, popup),
        Modal::Saved(popup) => super::saved::draw(frame, app, area, popup),
    }
    app.modal = Some(modal);
}

/// Dims the screen, registers the backdrop and draws an empty popup frame.
/// Returns the area inside the border, with one column of padding.
pub(super) fn popup(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    rect: Rect,
    title: &str,
    footer: &str,
) -> Rect {
    let theme = &app.theme;
    frame
        .buffer_mut()
        .set_style(area, Style::new().add_modifier(Modifier::DIM));
    app.hits.add(area, HitId::Backdrop);
    app.hits.add(rect, HitId::Overlay);
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.style(Role::OverlayBorder))
        .style(theme.style(Role::Text))
        .title(Line::from(Span::styled(
            format!(" {title} "),
            theme.style(Role::TitleFocused),
        )))
        .title_bottom(
            Line::from(Span::styled(
                format!(" {footer} "),
                theme.style(Role::Muted),
            ))
            .right_aligned(),
        );
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    )
}

pub(super) fn centered(area: Rect, width: u16, height: u16) -> Option<Rect> {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(1));
    (width >= 20 && height >= 5).then(|| {
        Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        )
    })
}

pub(super) fn row(inner: Rect, n: u16) -> Rect {
    if n < inner.height {
        Rect::new(inner.x, inner.y + n, inner.width, 1)
    } else {
        Rect::default()
    }
}

/// A labelled one-line input. Returns where the cursor belongs (when focused) and
/// the rectangle of the editable value area for pointer hit-testing.
pub(super) fn field(
    frame: &mut Frame,
    theme: &Theme,
    rect: Rect,
    label: &str,
    value: &mut LineEdit,
    masked: bool,
    focused: bool,
) -> (Option<Position>, Rect) {
    if rect.width == 0 {
        return (None, Rect::default());
    }
    let room = (rect.width as usize).saturating_sub(FIELD_LABEL + 1);
    let view = value.view(room, masked);
    let label_style = if focused {
        theme.style(Role::TitleFocused)
    } else {
        theme.style(Role::Muted)
    };
    let cursor = Position::new(
        rect.x + (FIELD_LABEL + view.cursor_col).min(rect.width as usize - 1) as u16,
        rect.y,
    );
    let underline = "_".repeat(room.saturating_sub(width(&view.text)));
    let line = Line::from(vec![
        Span::styled(format!("{label:<FIELD_LABEL$}"), label_style),
        Span::raw(view.text),
        Span::styled(underline, theme.style(Role::Border)),
    ]);
    frame.render_widget(line, rect);
    let value_rect = Rect::new(
        rect.x + FIELD_LABEL as u16,
        rect.y,
        rect.width.saturating_sub(FIELD_LABEL as u16),
        1,
    );
    (focused.then_some(cursor), value_rect)
}

pub(super) fn button(frame: &mut Frame, theme: &Theme, rect: Rect, label: &str, focused: bool) {
    let style = if focused {
        theme.style(Role::Pill)
    } else {
        theme.style(Role::StatusKey)
    };
    frame.render_widget(Line::from(Span::styled(label.to_owned(), style)), rect);
}

fn settings(frame: &mut Frame, app: &mut App, area: Rect, menu: &mut SettingsMenu) {
    /// Room for the longest label ("Notification keywords") and a space.
    const LABEL: u16 = 22;
    let visible_settings: Vec<(usize, Setting)> = Setting::ALL
        .into_iter()
        .enumerate()
        .filter(|(_, s)| menu.shows(*s))
        .collect();
    let headings = visible_settings
        .iter()
        .filter(|(_, s)| s.heading().is_some())
        .count() as u16;
    let rows = visible_settings.len() as u16 + headings;
    // Blank lines between the groups only where there is room for them.
    let airy = area.height >= rows + headings + 6;
    let height = rows + if airy { headings + 1 } else { 0 } + 4;
    // The values keep their room however wide the label column is.
    let Some(rect) = centered(area, 40 + LABEL, height) else {
        return;
    };
    let inner = popup(
        frame,
        app,
        area,
        rect,
        "Settings",
        "↑↓ select · ←→ change · Enter · Esc close",
    );
    enum SettingLine {
        Heading(&'static str),
        Blank,
        Row(usize, Setting),
    }
    let mut lines = Vec::with_capacity(rows as usize + headings as usize);
    let mut selected_line = 0usize;
    for (pos, &(index, setting)) in visible_settings.iter().enumerate() {
        if let Some(heading) = setting.heading() {
            if airy && pos > 0 {
                lines.push(SettingLine::Blank);
            }
            lines.push(SettingLine::Heading(heading));
        }
        if menu.selected == index {
            selected_line = lines.len();
        }
        lines.push(SettingLine::Row(index, setting));
    }

    let theme = app.theme.clone();
    // The last lines belong to what the selected row does: they are kept free also
    // where the rows alone would fill the popup, and the rows scroll instead.
    let hint_lines: u16 = match inner.height {
        0..=5 => 0,
        6..=9 => 1,
        _ => 2,
    };
    let fits = lines.len() as u16 + hint_lines <= inner.height;
    let visible = if fits {
        lines.len()
    } else {
        inner.height.saturating_sub(hint_lines) as usize
    };
    let scroll = (selected_line + 1).saturating_sub(visible);
    // Where rows are out of sight, the edge they are beyond says so.
    let (above, below) = (scroll > 0, scroll + visible < lines.len());
    let mut y = 0u16;
    for line in lines.iter().skip(scroll).take(visible) {
        let rect = row(inner, y);
        y += 1;
        let (index, setting) = match *line {
            SettingLine::Blank => continue,
            SettingLine::Heading(heading) => {
                frame.render_widget(
                    Line::from(Span::styled(heading, theme.style(Role::Muted))),
                    rect,
                );
                continue;
            }
            SettingLine::Row(index, setting) => (index, setting),
        };
        if rect.width <= LABEL + 6 {
            continue;
        }
        let selected = menu.selected == index;
        let value = app.setting_value(setting);
        let label_style = if selected {
            theme.style(Role::TitleFocused)
        } else {
            theme.style(Role::Text)
        };
        let marker = if selected { "› " } else { "  " };
        let mut spans = vec![
            Span::styled(marker, theme.style(Role::TitleFocused)),
            Span::styled(
                format!("{:<width$}", setting.label(), width = LABEL as usize),
                label_style,
            ),
        ];
        let value_x = rect.x + 2 + LABEL;
        let room = rect.right().saturating_sub(value_x) as usize;
        app.hits.add(rect, HitId::Setting(index, 0));
        match setting {
            Setting::Theme
            | Setting::ThemeDark
            | Setting::ThemeLight
            | Setting::SidebarWidth
            | Setting::LinkPreviews => {
                let shown = truncate(&value, room.saturating_sub(4));
                let arrow = theme.style(Role::StatusKey);
                let end = value_x + 2 + width(&shown) as u16;
                spans.push(Span::styled("◂ ", arrow));
                spans.push(Span::styled(shown, theme.style(Role::Text)));
                spans.push(Span::styled(" ▸", arrow));
                app.hits
                    .add(Rect::new(value_x, rect.y, 2, 1), HitId::Setting(index, -1));
                app.hits
                    .add(Rect::new(end, rect.y, 2, 1), HitId::Setting(index, 1));
            }
            Setting::ThemePairing
            | Setting::Mouse
            | Setting::EscClosesChat
            | Setting::TypingNotices
            | Setting::ReadReceipts
            | Setting::DoNotDisturb
            | Setting::WindowTitle
            | Setting::ReadMarkers
            | Setting::RoomPreviews
            | Setting::Notifications => {
                let on = value == "on";
                let role = if on { Role::StatusOk } else { Role::Muted };
                let dot = if on { "● " } else { "○ " };
                spans.push(Span::styled(format!("{dot}{value}"), theme.style(role)));
            }
            Setting::DownloadDir | Setting::AudioPlayer => match (&mut menu.editing, selected) {
                (Some(typed), true) => {
                    let view = typed.view(room.saturating_sub(1), false);
                    frame.set_cursor_position(Position::new(
                        (value_x + view.cursor_col as u16).min(rect.right().saturating_sub(1)),
                        rect.y,
                    ));
                    let rest = "_".repeat(room.saturating_sub(width(&view.text)));
                    spans.push(Span::styled(view.text, theme.style(Role::Text)));
                    spans.push(Span::styled(rest, theme.style(Role::Border)));
                    app.hits.add(
                        Rect::new(value_x, rect.y, room as u16, 1),
                        HitId::Input(InputTarget::DownloadDir),
                    );
                }
                _ => spans.push(Span::styled(
                    truncate(&value, room),
                    theme.style(Role::Text),
                )),
            },
            Setting::Keys => spans.push(Span::styled(
                truncate(&value, room),
                theme.style(Role::StatusKey),
            )),
            Setting::Profile => {
                let role = if value.starts_with("not") || value.starts_with("signed out") {
                    Role::Muted
                } else {
                    Role::Text
                };
                spans.push(Span::styled(truncate(&value, room), theme.style(role)));
            }
            Setting::Encryption => {
                let role = if value.starts_with("not") {
                    Role::Error
                } else {
                    Role::Muted
                };
                spans.push(Span::styled(truncate(&value, room), theme.style(role)));
            }
            Setting::Sessions | Setting::Keywords | Setting::Doctor => spans.push(Span::styled(
                truncate(&value, room),
                theme.style(Role::Muted),
            )),
            Setting::KeyBackup => {
                // Red only for what needs doing; in progress or unknown is not that.
                let role = match crate::devices::backup_setting(app.verify.security).1 {
                    crate::devices::Tone::Warn => Role::Error,
                    _ => Role::Muted,
                };
                spans.push(Span::styled(truncate(&value, room), theme.style(role)));
            }
            Setting::IgnoredUsers => {
                let role = if value.eq_ignore_ascii_case("none") {
                    Role::Muted
                } else {
                    Role::Text
                };
                spans.push(Span::styled(truncate(&value, room), theme.style(role)));
            }
            Setting::Logout => {
                if menu.confirm_logout {
                    spans.push(Span::styled(
                        truncate("Enter again logs out · Esc stays", room),
                        theme.style(Role::Error),
                    ));
                } else {
                    spans.push(Span::styled(
                        truncate(&value, room),
                        theme.style(Role::Muted),
                    ));
                }
            }
        }
        frame.render_widget(Line::from(spans), rect);
        let hovered = matches!(app.hover, Some(HitId::Setting(i, _)) if i == index);
        if hovered && !selected {
            frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
        }
    }
    for (more, at, mark) in [(above, 0, "▲"), (below, y.saturating_sub(1), "▼")] {
        if more && inner.width > 2 && y > 0 {
            let rect = row(inner, at);
            let mark_rect = Rect::new(rect.right().saturating_sub(1), rect.y, 1, 1);
            frame.render_widget(
                Line::from(Span::styled(mark, theme.style(Role::Muted))),
                mark_rect,
            );
        }
    }
    // What the selected row does, at the bottom.
    let hint_top = if airy && fits { y + 1 } else { y };
    if hint_top < inner.height {
        let hint = Rect::new(
            inner.x,
            inner.y + hint_top,
            inner.width,
            (inner.height - hint_top).min(2),
        );
        frame.render_widget(
            Paragraph::new(menu.current().hint())
                .style(theme.style(Role::Muted))
                .wrap(Wrap { trim: true }),
            hint,
        );
    }
}

fn fixed_field(frame: &mut Frame, theme: &Theme, rect: Rect, label: &str, value: &str) {
    if rect.width == 0 {
        return;
    }
    const LABEL: usize = 13;
    let room = (rect.width as usize).saturating_sub(LABEL + 1);
    let shown = truncate(&sanitize_line(value), room);
    let line = Line::from(vec![
        Span::styled(format!("{label:<LABEL$}"), theme.style(Role::Muted)),
        Span::raw(shown),
    ]);
    frame.render_widget(line, rect);
}

fn login_button(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    label: &str,
    slot: usize,
    action: Action,
    form: &LoginForm,
) -> u16 {
    let busy = form.busy.is_some();
    let w = (width(label) as u16).min(area.width);
    let rect = Rect::new(area.x, area.y, w, area.height);
    button(frame, &app.theme, rect, label, !busy && form.focus == slot);
    if !busy {
        app.hits.add(rect, HitId::Do(action));
    }
    area.x + w + 2
}

fn login(frame: &mut Frame, app: &mut App, area: Rect, form: &mut LoginForm) {
    let Some(rect) = centered(area, 64, 15) else {
        return;
    };
    let quit = app
        .keymap
        .label_in(Context::Global, Action::Quit)
        .or_else(|| app.keymap.label_for(Action::Quit))
        .unwrap_or("Ctrl-Q");
    let footer = if form.busy.is_some() {
        format!("Esc cancel · {quit} quit")
    } else {
        format!("Tab next · Enter continue · {quit} quit")
    };
    let title = if form.is_reauth() {
        "Re-authenticate session"
    } else {
        "Log in to Matrix"
    };
    let inner = popup(frame, app, area, rect, title, &footer);
    let busy = form.busy.is_some();

    let mut cursor = None;
    let msg_start = if let Some(reauth) = &form.reauth {
        fixed_field(
            frame,
            &app.theme,
            row(inner, 1),
            "Homeserver",
            &form.homeserver,
        );
        fixed_field(frame, &app.theme, row(inner, 3), "Account", &reauth.user_id);
        match reauth.kind {
            ReauthKind::Password => {
                let pw_rect = row(inner, 5);
                let (at, value_rect) = field(
                    frame,
                    &app.theme,
                    pw_rect,
                    "Password",
                    &mut form.password,
                    true,
                    !busy && form.focus == 2,
                );
                cursor = at;
                app.hits.add(pw_rect, HitId::Field(2));
                if !busy {
                    app.hits
                        .add(value_rect, HitId::Input(InputTarget::Login(2)));
                }

                let buttons = row(inner, 7);
                login_button(
                    frame,
                    app,
                    buttons,
                    " Log in ",
                    LoginForm::PASSWORD_BUTTON,
                    Action::LoginSubmit,
                    form,
                );
                9
            }
            ReauthKind::Browser => {
                let buttons = row(inner, 5);
                login_button(
                    frame,
                    app,
                    buttons,
                    " Log in with browser ",
                    LoginForm::BROWSER_BUTTON,
                    Action::LoginBrowser,
                    form,
                );
                7
            }
        }
    } else {
        let fields = [
            (0usize, "Homeserver", &mut form.homeserver, false),
            (1usize, "Username", &mut form.username, false),
            (2usize, "Password", &mut form.password, true),
        ];
        for (n, label, value, masked) in fields {
            let rect = row(inner, 1 + n as u16 * 2);
            let (at, value_rect) = field(
                frame,
                &app.theme,
                rect,
                label,
                value,
                masked,
                !busy && form.focus == n,
            );
            cursor = cursor.or(at);
            app.hits.add(rect, HitId::Field(n));
            if !busy {
                app.hits
                    .add(value_rect, HitId::Input(InputTarget::Login(n)));
            }
        }

        let buttons = row(inner, 7);
        let labels = [
            (" Log in ", LoginForm::PASSWORD_BUTTON, Action::LoginSubmit),
            (
                " Log in with browser ",
                LoginForm::BROWSER_BUTTON,
                Action::LoginBrowser,
            ),
        ];
        let mut x = buttons.x;
        for (label, slot, action) in labels {
            let slice = Rect::new(
                x,
                buttons.y,
                buttons.right().saturating_sub(x),
                buttons.height,
            );
            x = login_button(frame, app, slice, label, slot, action, form);
        }
        9
    };

    let theme = &app.theme;

    let message_area = Rect::new(
        inner.x,
        inner.y + msg_start.min(inner.height),
        inner.width,
        inner.height.saturating_sub(msg_start),
    );
    let message = if let Some(url) = &form.browser_url {
        Some((
            format!("Approve the login in your browser. If it did not open, visit:\n{url}"),
            Role::Muted,
        ))
    } else if let Some(busy) = &form.busy {
        Some((sanitize(busy), Role::Muted))
    } else if let Some(error) = &form.error {
        Some((sanitize(error), Role::Error))
    } else if let Some(note) = &form.note {
        Some((sanitize(note), Role::Muted))
    } else {
        Some((
            "Use the browser login for matrix.org and other servers with single sign-on."
                .to_owned(),
            Role::Muted,
        ))
    };
    if let Some((text, role)) = message {
        frame.render_widget(
            Paragraph::new(text)
                .style(theme.style(role))
                .wrap(Wrap { trim: false }),
            message_area,
        );
    }
    if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
}

fn recovery(frame: &mut Frame, app: &mut App, area: Rect, form: &mut RecoveryForm) {
    let Some(rect) = centered(area, 70, 13) else {
        return;
    };
    let inner = popup(
        frame,
        app,
        area,
        rect,
        "Unlock encrypted history",
        "Enter unlock · Esc later",
    );
    let theme = &app.theme;
    frame.render_widget(
        Paragraph::new(
            "This is a new device. Enter your recovery key (or passphrase) so it can read \
             your encrypted messages.",
        )
        .style(theme.style(Role::Muted))
        .wrap(Wrap { trim: false }),
        Rect::new(
            inner.x,
            inner.y + 1.min(inner.height),
            inner.width,
            2.min(inner.height.saturating_sub(1)),
        ),
    );
    let (cursor, value_rect) = field(
        frame,
        theme,
        row(inner, 4),
        "Recovery key",
        &mut form.key,
        false,
        !form.busy,
    );
    if !form.busy {
        app.hits
            .add(value_rect, HitId::Input(InputTarget::Recovery));
    }
    let buttons = row(inner, 6);
    if !form.busy {
        let mut x = buttons.x;
        for (label, action, primary) in [
            (" Unlock ", Action::RecoverySubmit, true),
            (" Later ", Action::CloseModal, false),
        ] {
            let w = (width(label) as u16).min(buttons.right().saturating_sub(x));
            let rect = Rect::new(x, buttons.y, w, buttons.height);
            button(frame, theme, rect, label, primary);
            app.hits.add(rect, HitId::Do(action));
            x += w + 2;
        }
    }
    let (text, role) = match (&form.error, form.busy) {
        (_, true) => ("Unlocking…".to_owned(), Role::Muted),
        (Some(error), false) => (error.clone(), Role::Error),
        (None, false) => (String::new(), Role::Muted),
    };
    frame.render_widget(
        Paragraph::new(text)
            .style(theme.style(role))
            .wrap(Wrap { trim: false }),
        Rect::new(
            inner.x,
            inner.y + 8.min(inner.height),
            inner.width,
            inner.height.saturating_sub(8),
        ),
    );
    if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
}

fn emoji(frame: &mut Frame, app: &mut App, area: Rect, picker: &mut EmojiPicker) {
    // Sits above the composer's right end, next to the button that opens it.
    let anchor = app.composer_view.area;
    let width = 44.min(area.width.saturating_sub(2));
    let height = 15.min(area.height.saturating_sub(1));
    if width < 12 || height < 6 {
        return;
    }
    let right = if anchor.width > 0 {
        (anchor.right() + 5).min(area.right())
    } else {
        area.right()
    };
    let bottom = if anchor.height > 0 {
        anchor.y.saturating_sub(1).max(area.y + height)
    } else {
        area.bottom()
    };
    let rect = Rect::new(
        right.saturating_sub(width).max(area.x),
        bottom.saturating_sub(height).max(area.y),
        width,
        height,
    );
    let inner = popup(
        frame,
        app,
        area,
        rect,
        if picker.react_to.is_some() {
            "React"
        } else {
            "Emoji"
        },
        if picker.react_to.is_some() {
            "↑↓←→ move · Enter react · Esc close"
        } else {
            "↑↓←→ move · Enter insert · Esc close"
        },
    );
    let theme = &app.theme;
    if inner.height < 3 || inner.width < EMOJI_CELL {
        return;
    }

    // Search field.
    let search = row(inner, 0);
    let max_cols = (search.width as usize).saturating_sub(3);
    let view = picker.query.view(max_cols, false);
    let line = if picker.query.is_empty() {
        Line::from(vec![
            Span::styled("⌕ ", theme.style(Role::StatusKey)),
            Span::styled("type to search…", theme.style(Role::Placeholder)),
        ])
    } else {
        Line::from(vec![
            Span::styled("⌕ ", theme.style(Role::StatusKey)),
            Span::raw(view.text),
        ])
    };
    frame.render_widget(line, search);
    frame.set_cursor_position(Position::new(
        (search.x + 2 + view.cursor_col as u16).min(search.right().saturating_sub(1)),
        search.y,
    ));
    app.hits.add(
        Rect::new(search.x + 2, search.y, search.width.saturating_sub(2), 1),
        HitId::Input(InputTarget::EmojiQuery),
    );

    // Grid, with the last row kept for the selected emoji's name. Custom emoji take
    // wider cells, in rows of their own.
    let grid = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 2);
    picker.columns = (grid.width / EMOJI_CELL).max(1) as usize;
    picker.custom_columns = (grid.width / crate::modal::CUSTOM_CELL).max(1) as usize;
    let rows = grid.height as usize;
    picker.rows = rows;
    let total_rows = picker.grid_rows();
    if picker.reveal {
        picker.reveal = false;
        if let Some((selected_row, _, _)) = picker.cell_of(picker.selected) {
            if selected_row < picker.scroll {
                picker.scroll = selected_row;
            } else if selected_row >= picker.scroll + rows {
                picker.scroll = selected_row + 1 - rows;
            }
        }
    }
    picker.scroll = picker.scroll.min(total_rows.saturating_sub(rows));

    if picker.results.is_empty() {
        frame.render_widget(
            Line::from(Span::styled("no emoji found", theme.style(Role::Muted))),
            grid,
        );
    }
    let mut pictures: Vec<(Rect, String)> = Vec::new();
    let mut wanted: Vec<String> = Vec::new();
    for (n, at_row, x, cell_width) in picker.cells(picker.scroll..picker.scroll + rows) {
        let Some(pick) = picker.results.get(n) else {
            continue;
        };
        let cell = Rect::new(
            grid.x + x,
            grid.y + (at_row - picker.scroll) as u16,
            cell_width.min(grid.width.saturating_sub(x)),
            1,
        );
        if cell.width == 0 {
            continue;
        }
        match pick {
            Pick::Unicode(emoji) => frame.render_widget(Line::from(emoji.as_str()), cell),
            Pick::Custom(at) => {
                let Some(custom) = picker.custom.get(*at) else {
                    continue;
                };
                // Its picture where there is one, and always its name: without
                // pictures, the name is how it is found.
                let name = format!(":{}:", custom.shortcode);
                let room = usize::from(cell.width.saturating_sub(1));
                let picture =
                    app.images.emote_ready(&custom.url) && room > usize::from(EMOTE_CELLS.0) + 1;
                let text = if picture {
                    let room = room - usize::from(EMOTE_CELLS.0) - 1;
                    format!("{} {}", crate::rich::EMOTE_BOX, truncate(&name, room))
                } else {
                    truncate(&name, room)
                };
                frame.render_widget(
                    Line::from(Span::styled(text, theme.style(Role::Text))),
                    cell,
                );
                if picture {
                    pictures.push((
                        Rect::new(cell.x, cell.y, EMOTE_CELLS.0, EMOTE_CELLS.1),
                        custom.url.clone(),
                    ));
                }
                wanted.push(custom.url.clone());
            }
        }
        // After drawing: rendering a wide glyph resets the style of its second column.
        if n == picker.selected {
            frame.buffer_mut().set_style(cell, theme.style(Role::Pill));
        } else if app.hover == Some(HitId::Emoji(n)) {
            frame.buffer_mut().set_style(cell, theme.style(Role::Hover));
        }
        app.hits.add(cell, HitId::Emoji(n));
    }
    // Pictures last, over the highlight: it stays behind their transparent parts.
    for (rect, url) in pictures {
        super::image::draw_emote(frame, &mut app.images, theme, &url, rect, false);
    }
    // Those on screen are fetched, a few at a time (see `ImageStore::request_emotes`).
    app.images.request_emotes(wanted.iter().map(String::as_str));

    if let Some(selected) = picker.selected() {
        let name = match selected {
            Pick::Unicode(emoji) => match emoji.shortcode() {
                Some(code) => format!("{}  :{code}:", emoji.name()),
                None => emoji.name().to_owned(),
            },
            // The name, and which pack it is from.
            Pick::Custom(at) => picker
                .custom
                .get(at)
                .map(|custom| format!(":{}:  {}", custom.shortcode, custom.pack_name))
                .unwrap_or_default(),
        };
        frame.render_widget(
            Line::from(Span::styled(
                truncate(&name, inner.width as usize),
                theme.style(Role::Muted),
            )),
            row(inner, inner.height - 1),
        );
    }
}

fn ignored_users(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    popup_state: &mut IgnoredUsersPopup,
) {
    let list_rows = popup_state.filtered.len().clamp(3, 12) as u16;
    let Some(rect) = centered(area, 64, list_rows + 6) else {
        return;
    };
    let inner = popup(
        frame,
        app,
        area,
        rect,
        "Ignored users",
        "Enter / Del unignore · ↑↓ select · Esc close",
    );
    let theme = app.theme.clone();
    let (cursor, filter_rect) = field(
        frame,
        &theme,
        row(inner, 0),
        "Filter",
        &mut popup_state.filter,
        false,
        true,
    );
    app.hits
        .add(filter_rect, HitId::Input(InputTarget::IgnoredFilter));

    let count_line = if popup_state.all.is_empty() {
        "No ignored users".to_owned()
    } else if popup_state.filter.as_str().trim().is_empty() {
        match popup_state.all.len() {
            1 => "1 ignored user".to_owned(),
            n => format!("{n} ignored users"),
        }
    } else {
        format!(
            "{} of {} matching",
            popup_state.filtered.len(),
            popup_state.all.len()
        )
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
    popup_state.page = rows;
    if rows == 0 {
        if let Some(cursor) = cursor {
            frame.set_cursor_position(cursor);
        }
        return;
    }
    if popup_state.reveal {
        popup_state.reveal = false;
        if popup_state.selected < popup_state.scroll {
            popup_state.scroll = popup_state.selected;
        } else if popup_state.selected >= popup_state.scroll + rows {
            popup_state.scroll = popup_state.selected + 1 - rows;
        }
    }
    popup_state.scroll = popup_state
        .scroll
        .min(popup_state.filtered.len().saturating_sub(rows));

    if popup_state.filtered.is_empty() {
        let empty = if popup_state.all.is_empty() {
            "Use the message menu or Room info to ignore someone."
        } else {
            "No matching users."
        };
        frame.render_widget(
            Line::from(Span::styled(
                truncate(empty, inner.width as usize),
                theme.style(Role::Muted),
            )),
            row(inner, list_y),
        );
    } else {
        let button_label = " unignore ";
        let button_w = width(button_label) as u16;
        for (offset, &all_idx) in popup_state
            .filtered
            .iter()
            .skip(popup_state.scroll)
            .take(rows)
            .enumerate()
        {
            let index = popup_state.scroll + offset;
            let rect = row(inner, list_y + offset as u16);
            if rect.width == 0 {
                continue;
            }
            let selected = index == popup_state.selected;
            let marker = if selected { "› " } else { "  " };
            let user = popup_state
                .all
                .get(all_idx)
                .map(|s| sanitize_line(s))
                .unwrap_or_default();
            let hovered = app.hover == Some(HitId::IgnoredRow(index));
            let text_style = if selected {
                theme.style(Role::TitleFocused)
            } else if hovered {
                theme.style(Role::Hover)
            } else {
                theme.style(Role::Text)
            };
            let show_button = selected || hovered;
            let label_max = (rect.width as usize).saturating_sub(
                2 + if show_button {
                    button_w as usize + 1
                } else {
                    0
                },
            );
            let spans = vec![
                Span::styled(marker, theme.style(Role::TitleFocused)),
                Span::styled(truncate(&user, label_max), text_style),
            ];
            frame.render_widget(Line::from(spans), rect);
            app.hits.add(rect, HitId::IgnoredRow(index));

            if show_button && rect.width > button_w + 4 {
                let btn_rect = Rect::new(rect.right() - button_w, rect.y, button_w, 1);
                let btn_role = if app.hover == Some(HitId::IgnoredUnignore(index)) {
                    Role::Hover
                } else {
                    Role::Pill
                };
                frame.render_widget(
                    Line::from(Span::styled(button_label, theme.style(btn_role))),
                    btn_rect,
                );
                app.hits.add(btn_rect, HitId::IgnoredUnignore(index));
            }
        }
    }

    if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
}

fn report(frame: &mut Frame, app: &mut App, area: Rect, form: &mut ReportForm) {
    let height = if form.can_ignore { 11 } else { 9 };
    let Some(rect) = centered(area, 64, height) else {
        return;
    };
    let footer = if form.can_ignore {
        "Tab move · Space toggle · Enter send · Esc cancel"
    } else {
        "Tab move · Enter send · Esc cancel"
    };
    let inner = popup(frame, app, area, rect, "Report message", footer);
    let theme = app.theme.clone();

    let prompt = format!(
        "Report {}'s message to the homeserver administrators.",
        sanitize_line(&form.sender_name)
    );
    frame.render_widget(
        Paragraph::new(prompt)
            .style(theme.style(Role::Muted))
            .wrap(Wrap { trim: true }),
        Rect::new(inner.x, inner.y, inner.width, 2.min(inner.height)),
    );

    let reason_rect = row(inner, 2);
    app.hits.add(reason_rect, HitId::Field(ReportForm::REASON));
    let (cursor, value_rect) = field(
        frame,
        &theme,
        reason_rect,
        "Reason",
        &mut form.reason,
        false,
        form.focus == ReportForm::REASON,
    );
    app.hits
        .add(value_rect, HitId::Input(InputTarget::ReportReason));

    let submit_row = if let Some(toggle_slot) = form.toggle_slot() {
        let rect = row(inner, 4);
        if rect.width > 0 {
            let focused = form.focus == toggle_slot;
            let mark = if form.also_ignore { "[x]" } else { "[ ]" };
            let label_style = if focused {
                theme.style(Role::TitleFocused)
            } else {
                theme.style(Role::Text)
            };
            frame.render_widget(
                Line::from(vec![
                    Span::styled(
                        format!("{mark} "),
                        theme.style(if form.also_ignore {
                            Role::StatusOk
                        } else {
                            Role::Muted
                        }),
                    ),
                    Span::styled("Also ignore this user", label_style),
                ]),
                rect,
            );
            app.hits.add(rect, HitId::ReportIgnoreToggle);
        }
        6
    } else {
        4
    };

    let btn_row = row(inner, submit_row);
    let label = " Send report ";
    if btn_row.width > 0 {
        let w = (width(label) as u16).min(btn_row.width);
        let btn_rect = Rect::new(btn_row.x, btn_row.y, w, 1);
        let btn_focused =
            form.focus == form.submit_slot() || app.hover == Some(HitId::ReportSubmit);
        button(frame, &theme, btn_rect, label, btn_focused);
        app.hits.add(btn_rect, HitId::ReportSubmit);
    }

    if let Some(error) = &form.error {
        frame.render_widget(
            Line::from(Span::styled(
                truncate(&sanitize_line(error), inner.width as usize),
                theme.style(Role::Error),
            )),
            row(inner, submit_row + 1),
        );
    }

    if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
}

fn pinned(frame: &mut Frame, app: &mut App, area: Rect, popup_state: &mut PinnedPopup) {
    let w = area.width.saturating_sub(4).clamp(24, 72);
    let h = area.height.saturating_sub(2).clamp(8, 22);
    let Some(rect) = centered(area, w, h) else {
        return;
    };
    let room = sanitize_line(&popup_state.room_name);
    let title = format!(
        " Pinned — {} ",
        truncate(&room, w.saturating_sub(14) as usize)
    );
    let footer = if popup_state.can_pin {
        "Enter jump · Del/u unpin · Esc close"
    } else {
        "Enter jump · Esc close"
    };
    let inner = popup(frame, app, area, rect, &title, footer);
    let theme = &app.theme;
    if inner.width < 10 || inner.height < 2 {
        return;
    }

    if popup_state.loading {
        frame.render_widget(
            Line::from(Span::styled(
                "Loading pinned messages…",
                theme.style(Role::Muted),
            )),
            row(inner, 0),
        );
        return;
    }
    if let Some(error) = &popup_state.error {
        frame.render_widget(
            Paragraph::new(sanitize(error))
                .style(theme.style(Role::Error))
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }
    if popup_state.items.is_empty() {
        frame.render_widget(
            Line::from(Span::styled("No pinned messages", theme.style(Role::Muted))),
            row(inner, 0),
        );
        return;
    }

    let rows_per_item = 2usize;
    let visible = ((inner.height as usize) / rows_per_item).max(1);
    popup_state.page = visible;
    popup_state.clamp_scroll(visible);
    let start = popup_state.scroll;
    let end = (start + visible).min(popup_state.items.len());

    for (vis, index) in (start..end).enumerate() {
        let item = &popup_state.items[index];
        let y = inner.y + (vis * rows_per_item) as u16;
        let selected = index == popup_state.selected;
        let hovered_row = app.hover == Some(HitId::PinnedRow(index));
        let hovered_unpin = app.hover == Some(HitId::PinnedUnpin(index));
        let row_area = Rect::new(inner.x, y, inner.width, rows_per_item as u16);

        let bg_style = if selected {
            theme.style(Role::Pill)
        } else if hovered_row || hovered_unpin {
            theme.style(Role::Hover)
        } else {
            theme.style(Role::Text)
        };
        frame.buffer_mut().set_style(row_area, bg_style);
        app.hits.add(row_area, HitId::PinnedRow(index));

        let sender = sanitize_line(&item.sender);
        let date = if !item.day.is_empty() && !item.time.is_empty() {
            format!("{} {}", sanitize_line(&item.day), sanitize_line(&item.time))
        } else if !item.time.is_empty() {
            sanitize_line(&item.time)
        } else {
            sanitize_line(&item.day)
        };
        let unpin_label = " unpin ";
        let unpin_w = if popup_state.can_pin {
            width(unpin_label) as u16
        } else {
            0
        };
        let header_w = inner.width.saturating_sub(unpin_w + 1) as usize;
        let sender_w = header_w.saturating_sub(width(&date) + 3);
        let sender_shown = truncate(&sender, sender_w);
        let mut head_spans = vec![Span::styled(
            sender_shown,
            bg_style
                .patch(theme.style(Role::TitleFocused))
                .add_modifier(Modifier::BOLD),
        )];
        if !date.is_empty() {
            head_spans.push(Span::styled(
                format!(" · {date}"),
                bg_style.patch(theme.style(Role::Muted)),
            ));
        }
        frame.render_widget(
            Line::from(head_spans),
            Rect::new(inner.x, y, inner.width.saturating_sub(unpin_w), 1),
        );

        if popup_state.can_pin && inner.width > unpin_w + 4 {
            let ux = inner.right() - unpin_w;
            let urect = Rect::new(ux, y, unpin_w, 1);
            let ustyle = if hovered_unpin {
                theme.style(Role::Pill).add_modifier(Modifier::BOLD)
            } else {
                theme.style(Role::Error)
            };
            frame.render_widget(Line::from(Span::styled(unpin_label, ustyle)), urect);
            app.hits.add(urect, HitId::PinnedUnpin(index));
        }

        if y + 1 < inner.bottom() {
            let snippet_raw = if item.redacted {
                "[message deleted]".to_owned()
            } else {
                sanitize_line(&item.snippet)
            };
            let snippet = truncate(&snippet_raw, inner.width.saturating_sub(2) as usize);
            let sstyle = if item.redacted {
                bg_style
                    .patch(theme.style(Role::Muted))
                    .add_modifier(Modifier::ITALIC)
            } else {
                bg_style.patch(theme.style(Role::Text))
            };
            frame.render_widget(
                Line::from(vec![Span::raw("  "), Span::styled(snippet, sstyle)]),
                Rect::new(inner.x, y + 1, inner.width, 1),
            );
        }
    }
}

fn reason_confirm(frame: &mut Frame, app: &mut App, area: Rect, form: &mut ReasonConfirmPopup) {
    let w = area.width.saturating_sub(4).clamp(28, 60);
    let h = 11u16.min(area.height.saturating_sub(2)).max(7);
    let Some(rect) = centered(area, w, h) else {
        return;
    };
    let (title, question, detail, action_label): (&str, &str, String, &str) = match &form.kind {
        ReasonConfirmKind::Redact {
            sender_name,
            preview,
            ..
        } => {
            let detail = if preview.is_empty() {
                sender_name.clone()
            } else {
                format!("{sender_name}: {preview}")
            };
            (
                " Delete message ",
                "Delete this message from the room?",
                detail,
                "Delete",
            )
        }
        ReasonConfirmKind::Ban {
            user_id, user_name, ..
        } => (
            " Ban user ",
            "Ban this user from the room?",
            if user_name.is_empty() {
                user_id.clone()
            } else {
                user_name.clone()
            },
            "Ban",
        ),
        ReasonConfirmKind::Kick {
            user_id, user_name, ..
        } => (
            " Remove from room ",
            "Remove this member from the room?",
            if user_name.is_empty() {
                user_id.clone()
            } else {
                user_name.clone()
            },
            "Remove",
        ),
        ReasonConfirmKind::Unban {
            user_id, user_name, ..
        } => (
            " Unban user ",
            "Let this user back into the room?",
            if user_name.is_empty() {
                user_id.clone()
            } else {
                user_name.clone()
            },
            "Unban",
        ),
    };
    let inner = popup(
        frame,
        app,
        area,
        rect,
        title,
        "Tab move · Enter confirm · Esc cancel",
    );
    let theme = &app.theme;
    if inner.width < 12 || inner.height < 5 {
        return;
    }

    frame.render_widget(
        Line::from(Span::styled(
            truncate(question, inner.width as usize),
            theme.style(Role::Text).add_modifier(Modifier::BOLD),
        )),
        row(inner, 0),
    );
    let detail_line = sanitize_line(&detail);
    frame.render_widget(
        Line::from(Span::styled(
            truncate(&detail_line, inner.width as usize),
            theme.style(Role::Muted),
        )),
        row(inner, 1),
    );

    let reason_y = if inner.height >= 7 { 3 } else { 2 };
    let reason_rect = row(inner, reason_y);
    app.hits
        .add(reason_rect, HitId::Field(ReasonConfirmPopup::REASON));
    let on_reason = form.on_reason();
    let label = "Reason";
    let input_x = reason_rect.x + FIELD_LABEL as u16;
    let input_w = reason_rect.width.saturating_sub(FIELD_LABEL as u16) as usize;
    let label_style = if on_reason {
        theme.style(Role::TitleFocused).add_modifier(Modifier::BOLD)
    } else {
        theme.style(Role::Muted)
    };
    let (shown, cursor_col) = if form.reason.is_empty() && !on_reason {
        ("(optional)".to_owned(), 0)
    } else if form.reason.is_empty() {
        (String::new(), 0)
    } else {
        let v = form.reason.view(input_w.saturating_sub(1), false);
        (v.text, v.cursor_col)
    };
    let val_style = if form.reason.is_empty() && !on_reason {
        theme.style(Role::Placeholder)
    } else {
        theme.style(Role::Text)
    };
    frame.render_widget(
        Line::from(vec![
            Span::styled(format!("{label:<FIELD_LABEL$}"), label_style),
            Span::styled(shown, val_style),
        ]),
        reason_rect,
    );
    if input_w > 0 {
        let input_rect = Rect::new(input_x, reason_rect.y, input_w as u16, 1);
        app.hits
            .add(input_rect, HitId::Input(InputTarget::ReasonConfirm));
        if on_reason {
            frame.set_cursor_position(Position::new(
                (input_x + cursor_col as u16).min(reason_rect.right().saturating_sub(1)),
                reason_rect.y,
            ));
        }
    }

    let btn_y = inner.bottom().saturating_sub(1);
    let submit_text = format!(" [{action_label}] ");
    let cancel_text = " [Cancel] ";
    let submit_w = width(&submit_text) as u16;
    let cancel_w = width(cancel_text) as u16;
    if inner.width > submit_w + cancel_w {
        let submit_rect = Rect::new(inner.x, btn_y, submit_w, 1);
        let cancel_rect = Rect::new(inner.x + submit_w + 1, btn_y, cancel_w, 1);
        let submit_focused = form.on_submit() || app.hover == Some(HitId::ReasonConfirmBtn(true));
        let cancel_focused = form.on_cancel() || app.hover == Some(HitId::ReasonConfirmBtn(false));
        let submit_style = if submit_focused {
            theme
                .style(Role::Error)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            theme.style(Role::Error)
        };
        let cancel_style = if cancel_focused {
            theme.style(Role::Pill).add_modifier(Modifier::BOLD)
        } else {
            theme.style(Role::Muted)
        };
        frame.render_widget(
            Line::from(Span::styled(submit_text, submit_style)),
            submit_rect,
        );
        frame.render_widget(
            Line::from(Span::styled(cancel_text, cancel_style)),
            cancel_rect,
        );
        app.hits.add(submit_rect, HitId::ReasonConfirmBtn(true));
        app.hits.add(cancel_rect, HitId::ReasonConfirmBtn(false));
    }
}
