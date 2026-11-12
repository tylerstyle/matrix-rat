//! Popups: login, recovery key, emoji picker.

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

use super::hit::HitId;
use crate::action::Action;
use crate::app::App;
use crate::modal::{EmojiPicker, LoginForm, Modal, RecoveryForm};
use crate::text::{truncate, width};
use crate::theme::{Role, Theme};

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
    }
    app.modal = Some(modal);
}

/// Dims the screen, registers the backdrop and draws an empty popup frame.
/// Returns the area inside the border, with one column of padding.
fn popup(
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

fn centered(area: Rect, width: u16, height: u16) -> Option<Rect> {
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

fn row(inner: Rect, n: u16) -> Rect {
    if n < inner.height {
        Rect::new(inner.x, inner.y + n, inner.width, 1)
    } else {
        Rect::default()
    }
}

/// A labelled one-line input. Returns where the cursor belongs when it has the focus.
fn field(
    frame: &mut Frame,
    theme: &Theme,
    rect: Rect,
    label: &str,
    value: &str,
    focused: bool,
) -> Option<Position> {
    if rect.width == 0 {
        return None;
    }
    const LABEL: usize = 13;
    let room = (rect.width as usize).saturating_sub(LABEL + 1);
    // Show the end of a value that is too long, where typing happens.
    let mut shown = value.to_owned();
    while width(&shown) > room && !shown.is_empty() {
        shown.remove(0);
    }
    let label_style = if focused {
        theme.style(Role::TitleFocused)
    } else {
        theme.style(Role::Muted)
    };
    let cursor = Position::new(
        rect.x + (LABEL + width(&shown)).min(rect.width as usize - 1) as u16,
        rect.y,
    );
    let underline = "_".repeat(room.saturating_sub(width(&shown)));
    let line = Line::from(vec![
        Span::styled(format!("{label:<LABEL$}"), label_style),
        Span::raw(shown),
        Span::styled(underline, theme.style(Role::Border)),
    ]);
    frame.render_widget(line, rect);
    focused.then_some(cursor)
}

fn button(frame: &mut Frame, theme: &Theme, rect: Rect, label: &str, focused: bool) {
    let style = if focused {
        theme.style(Role::Pill)
    } else {
        theme.style(Role::StatusKey)
    };
    frame.render_widget(Line::from(Span::styled(label.to_owned(), style)), rect);
}

fn login(frame: &mut Frame, app: &mut App, area: Rect, form: &LoginForm) {
    let Some(rect) = centered(area, 64, 15) else {
        return;
    };
    let footer = if form.busy.is_some() {
        "Esc cancel · Ctrl-Q quit"
    } else {
        "Tab next · Enter continue · Ctrl-Q quit"
    };
    let inner = popup(frame, app, area, rect, "Log in to Matrix", footer);
    let theme = &app.theme;
    let busy = form.busy.is_some();

    let masked = "•".repeat(form.password.chars().count());
    let fields = [
        ("Homeserver", form.homeserver.as_str()),
        ("Username", form.username.as_str()),
        ("Password", masked.as_str()),
    ];
    let mut cursor = None;
    for (n, (label, value)) in fields.into_iter().enumerate() {
        let rect = row(inner, 1 + n as u16 * 2);
        let at = field(frame, theme, rect, label, value, !busy && form.focus == n);
        cursor = cursor.or(at);
        app.hits.add(rect, HitId::Field(n));
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
        let w = (width(label) as u16).min(buttons.right().saturating_sub(x));
        let rect = Rect::new(x, buttons.y, w, buttons.height);
        button(frame, theme, rect, label, !busy && form.focus == slot);
        if !busy {
            app.hits.add(rect, HitId::Do(action));
        }
        x += w + 2;
    }

    let message_area = Rect::new(
        inner.x,
        inner.y + 9.min(inner.height),
        inner.width,
        inner.height.saturating_sub(9),
    );
    let message = if let Some(url) = &form.browser_url {
        Some((
            format!("Approve the login in your browser. If it did not open, visit:\n{url}"),
            Role::Muted,
        ))
    } else if let Some(busy) = &form.busy {
        Some((busy.clone(), Role::Muted))
    } else if let Some(error) = &form.error {
        Some((error.clone(), Role::Error))
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

fn recovery(frame: &mut Frame, app: &mut App, area: Rect, form: &RecoveryForm) {
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
    let cursor = field(
        frame,
        theme,
        row(inner, 4),
        "Recovery key",
        &form.key,
        !form.busy,
    );
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
        "Emoji",
        "↑↓←→ move · Enter insert · Esc close",
    );
    let theme = &app.theme;
    if inner.height < 3 || inner.width < CELL {
        return;
    }

    // Search field.
    let search = row(inner, 0);
    let line = if picker.query.is_empty() {
        Line::from(vec![
            Span::styled("⌕ ", theme.style(Role::StatusKey)),
            Span::styled("type to search…", theme.style(Role::Placeholder)),
        ])
    } else {
        Line::from(vec![
            Span::styled("⌕ ", theme.style(Role::StatusKey)),
            Span::raw(truncate(
                &picker.query,
                (search.width as usize).saturating_sub(3),
            )),
        ])
    };
    frame.render_widget(line, search);
    frame.set_cursor_position(Position::new(
        (search.x + 2 + width_of(&picker.query)).min(search.right().saturating_sub(1)),
        search.y,
    ));

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

fn width_of(text: &str) -> u16 {
    width(text) as u16
}
