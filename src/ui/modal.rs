//! Popups: login, recovery key, emoji picker.

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

use super::hit::{HitId, InputTarget};
use crate::action::Action;
use crate::app::App;
use crate::keymap::Context;
use crate::lineedit::LineEdit;
use crate::matrix::ReauthKind;
use crate::modal::{
    EmojiPicker, IgnoredUsersPopup, LoginForm, Modal, RecoveryForm, ReportForm, Setting,
    SettingsMenu,
};
use crate::text::{sanitize, sanitize_line, truncate, width};
use crate::theme::{Role, Theme};

const FIELD_LABEL: usize = 13;

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
        Modal::Menu(menu) => super::menu::draw(frame, app, area, menu),
        Modal::Keybinds(editor) => super::keybinds::draw(frame, app, area, editor),
        Modal::IgnoredUsers(popup) => ignored_users(frame, app, area, popup),
        Modal::Report(form) => report(frame, app, area, form),
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
    const LABEL: u16 = 20;
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
    let Some(rect) = centered(area, 60, height) else {
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
    let visible = inner.height as usize;
    let scroll = (selected_line + 1).saturating_sub(visible);
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
            Setting::Theme | Setting::ThemeDark | Setting::ThemeLight | Setting::SidebarWidth => {
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
            | Setting::TypingNotices
            | Setting::ReadReceipts
            | Setting::ReadMarkers
            | Setting::RoomPreviews
            | Setting::Notifications => {
                let on = value == "on";
                let role = if on { Role::StatusOk } else { Role::Muted };
                let dot = if on { "● " } else { "○ " };
                spans.push(Span::styled(format!("{dot}{value}"), theme.style(role)));
            }
            Setting::DownloadDir => match (&mut menu.editing, selected) {
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
            Setting::Encryption => {
                let role = if value.starts_with("not") {
                    Role::Error
                } else {
                    Role::Muted
                };
                spans.push(Span::styled(truncate(&value, room), theme.style(role)));
            }
            Setting::IgnoredUsers => {
                let role = if value == "none" {
                    Role::Muted
                } else {
                    Role::Text
                };
                spans.push(Span::styled(truncate(&value, room), theme.style(role)));
            }
            Setting::Logout => {
                if menu.confirm_logout {
                    spans.push(Span::styled(
                        truncate("Enter again to log out, Esc to stay", room),
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
    // What the selected row does, at the bottom.
    let hint_top = if airy { y + 1 } else { y };
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

/// Columns per emoji cell: two for the glyph, one of spacing.
const CELL: u16 = 3;

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
    if inner.height < 3 || inner.width < CELL {
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

    // Grid, with the last row kept for the selected emoji's name.
    let grid = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 2);
    let columns = (grid.width / CELL).max(1) as usize;
    let rows = grid.height as usize;
    picker.columns = columns;
    picker.rows = rows;
    let total_rows = picker.results.len().div_ceil(columns);
    if picker.reveal {
        picker.reveal = false;
        let selected_row = picker.selected / columns;
        if selected_row < picker.scroll {
            picker.scroll = selected_row;
        } else if selected_row >= picker.scroll + rows {
            picker.scroll = selected_row + 1 - rows;
        }
    }
    picker.scroll = picker.scroll.min(total_rows.saturating_sub(rows));

    if picker.results.is_empty() {
        frame.render_widget(
            Line::from(Span::styled("no emoji found", theme.style(Role::Muted))),
            grid,
        );
    }
    let first = picker.scroll * columns;
    for (n, emoji) in picker
        .results
        .iter()
        .enumerate()
        .skip(first)
        .take(rows * columns)
    {
        let at = n - first;
        let cell = Rect::new(
            grid.x + (at % columns) as u16 * CELL,
            grid.y + (at / columns) as u16,
            CELL,
            1,
        );
        frame.render_widget(Line::from(emoji.as_str()), cell);
        // After drawing: rendering a wide glyph resets the style of its second column.
        if n == picker.selected {
            frame.buffer_mut().set_style(cell, theme.style(Role::Pill));
        } else if app.hover == Some(HitId::Emoji(n)) {
            frame.buffer_mut().set_style(cell, theme.style(Role::Hover));
        }
        app.hits.add(cell, HitId::Emoji(n));
    }

    if let Some(selected) = picker.selected() {
        let name = match selected.shortcode() {
            Some(code) => format!("{}  :{code}:", selected.name()),
            None => selected.name().to_owned(),
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
