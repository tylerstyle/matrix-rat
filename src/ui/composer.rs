use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};

use super::hit::{Button, HitId};
use crate::action::{Action, Pane};
use crate::app::{App, ComposeMode};
use crate::text::truncate;
use crate::theme::Role;

const MAX_TEXT_ROWS: usize = 6;

/// The symbols of the buttons at the right end of the composer's first row. They are
/// named in this one place, so that a setting can offer another set later.
pub struct ComposerSymbols {
    /// Records a voice message.
    pub mic: &'static str,
    /// In the microphone's place while a recording runs: a click stops it.
    pub stop: &'static str,
    /// Attaches a file.
    pub clip: &'static str,
    /// Opens the emoji picker.
    pub smiley: &'static str,
    /// The cells each symbol is given, whatever it is drawn with.
    pub cell: u16,
}

pub const SYMBOLS: ComposerSymbols = ComposerSymbols {
    mic: "🎤",
    stop: "■",
    clip: "📎",
    smiley: "☺",
    cell: 2,
};

/// What the microphone button shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mic {
    /// Recording is not offered here: there is no button.
    Hidden,
    Idle,
    /// A recording runs: the button stops it.
    Recording,
}

/// The buttons from left to right: microphone (where recording is offered),
/// paperclip, smiley.
fn buttons(mic: Mic) -> Vec<(&'static str, Action)> {
    let mut buttons = Vec::with_capacity(3);
    match mic {
        Mic::Hidden => {}
        Mic::Idle => buttons.push((SYMBOLS.mic, Action::RecordVoice)),
        Mic::Recording => buttons.push((SYMBOLS.stop, Action::RecordVoice)),
    }
    buttons.push((SYMBOLS.clip, Action::Attach));
    buttons.push((SYMBOLS.smiley, Action::OpenEmoji));
    buttons
}

/// Columns kept free at the right for the buttons: one of space before each.
fn reserve(mic: Mic) -> u16 {
    buttons(mic).len() as u16 * (1 + SYMBOLS.cell)
}

fn text_width(pane_width: u16, mic: Mic) -> usize {
    // Borders, one column of padding on each side, and the buttons.
    pane_width.saturating_sub(4 + reserve(mic)).max(1) as usize
}

/// Height of the composer pane for the current text.
pub fn height(app: &App, pane_width: u16) -> u16 {
    let rows = app
        .composer
        .layout(text_width(pane_width, app.mic_button()))
        .len();
    let banner = u16::from(app.compose_mode != ComposeMode::Normal);
    let files = super::attach::rows(app.attachments().len());
    rows.clamp(1, MAX_TEXT_ROWS) as u16 + 2 + banner + files
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let in_thread = app.thread_open();
    let title = if in_thread { "Thread reply" } else { "Message" };
    let block = super::pane_block(app, Pane::Composer, title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.hits.add(area, HitId::Pane(Pane::Composer));
    draw_own_badge(frame, app, area, title);
    // Until a row shows them, waiting files count as not seen.
    app.attach.hidden = !app.attachments().is_empty();
    if inner.width < 3 || inner.height == 0 {
        return;
    }
    let theme = &app.theme;
    let mut text_area = Rect::new(inner.x + 1, inner.y, inner.width - 2, inner.height);

    // Reply / edit banner.
    let banner = match app.compose_mode {
        ComposeMode::Normal => None,
        ComposeMode::Reply(i) => app
            .room()
            .and_then(|r| r.messages.get(i))
            .map(|m| format!("↩ Replying to {}: {}", m.sender, m.body.replace('\n', " "))),
        // With the key that gives it up: not in every preset is that Esc.
        ComposeMode::Edit(_) => Some(format!(
            "✎ Editing your message · {} cancel",
            app.keymap.cancel_compose_label()
        )),
    };
    if let Some(banner) = banner {
        let rect = Rect::new(text_area.x, text_area.y, text_area.width, 1);
        let close = " ✕ ";
        let label = truncate(&banner, (rect.width as usize).saturating_sub(4));
        frame.render_widget(
            Line::from(Span::styled(label, theme.style(Role::Banner))),
            rect,
        );
        if rect.width > 4 {
            let close_rect = Rect::new(rect.right() - 3, rect.y, 3, 1);
            frame.render_widget(
                Line::from(Span::styled(close, theme.style(Role::Muted))),
                close_rect,
            );
            app.hits
                .add(close_rect, HitId::Button(Button::CancelCompose));
        }
        text_area.y += 1;
        text_area.height = text_area.height.saturating_sub(1);
    }
    // Files waiting to be sent, one line each; the text keeps at least one row.
    let scope = app.attach_scope();
    let files = app.attach.files(scope.as_ref());
    let file_rows = super::attach::rows(files.len()).min(text_area.height.saturating_sub(1));
    if file_rows > 0 {
        let rect = Rect::new(text_area.x, text_area.y, text_area.width, file_rows);
        super::attach::draw(frame, files, theme, app.hover, &mut app.hits, rect);
        text_area.y += file_rows;
        text_area.height -= file_rows;
    }
    let has_files = !files.is_empty();
    // Without a row for them, Enter does not send them.
    app.attach.hidden = has_files && file_rows == 0;
    if text_area.height == 0 {
        return;
    }

    // The buttons at the right end of the first text row. Too narrow for them and
    // some text: none is drawn (the keys and the palette still do the same).
    let mic = app.mic_button();
    let reserve = reserve(mic);
    if text_area.width > reserve + 4 {
        text_area.width -= reserve;
        let mut x = text_area.right() + 1;
        for (symbol, action) in buttons(mic) {
            let button = Rect::new(x, text_area.y, SYMBOLS.cell, 1);
            let role = if app.hover == Some(HitId::Do(action)) {
                Role::TitleFocused
            } else if symbol == SYMBOLS.stop {
                Role::Error
            } else {
                Role::Muted
            };
            frame.render_widget(Line::from(Span::styled(symbol, theme.style(role))), button);
            app.hits.add(button, HitId::Do(action));
            x += SYMBOLS.cell + 1;
        }
    }

    let max = text_area.width as usize;
    let lines = app.composer.layout(max);
    let (cursor_row, cursor_col) = app.composer.cursor_position(&lines);
    let visible = text_area.height as usize;
    // Scroll the text so the cursor row stays visible.
    let first = if cursor_row >= visible {
        cursor_row + 1 - visible
    } else {
        0
    };
    let focused = app.focus == Pane::Composer;
    let cursor_pos = (focused && !app.help_open && app.modal.is_none()).then(|| {
        let x = text_area.x + (cursor_col as u16).min(text_area.width.saturating_sub(1));
        let y = text_area.y + (cursor_row - first) as u16;
        (x, y)
    });
    app.composer_view = crate::app::ComposerView {
        area: text_area,
        width: max,
        first_row: first,
        cursor: cursor_pos,
    };

    if app.composer.is_empty() {
        let hint = if has_files && focused {
            match app.keymap.label_in(
                crate::keymap::Context::Composer,
                crate::action::Action::Send,
            ) {
                Some(k) => format!("Add a caption, or press {k} to send"),
                None => "Add a caption".to_owned(),
            }
        } else if focused && in_thread {
            "Reply in thread…".to_owned()
        } else if focused {
            "Type a message…".to_owned()
        } else {
            match app
                .keymap
                .label_for(crate::action::Action::Focus(crate::action::Pane::Composer))
            {
                Some(k) => format!("Click here or press {k} to write"),
                None => "Click here to write".to_owned(),
            }
        };
        frame.render_widget(
            Line::from(Span::styled(hint, theme.style(Role::Placeholder))),
            text_area,
        );
    } else {
        // Dimmed whenever typing would not land here.
        let style = if !focused {
            theme.style(Role::Muted)
        } else {
            ratatui::style::Style::new()
        };
        for (n, line) in lines.iter().skip(first).take(visible).enumerate() {
            let rect = Rect::new(text_area.x, text_area.y + n as u16, text_area.width, 1);
            let text = app.composer.text()[line.range.clone()].to_owned();
            frame.render_widget(Line::from(Span::styled(text, style)), rect);
        }
    }
    app.hits.add(text_area, HitId::ComposerText);

    if let Some((x, y)) = cursor_pos {
        frame.set_cursor_position(Position::new(x, y));
    }
}

/// Compact own-profile pill on the composer's top border when signed in.
fn draw_own_badge(frame: &mut Frame, app: &mut App, area: Rect, title: &str) {
    if area.height == 0 || (app.own_profile.is_none() && app.user_id.is_none()) {
        return;
    }
    let name = app
        .own_profile
        .as_ref()
        .and_then(|p| p.display_or_localpart(app.room_id().as_deref(), app.user_id.as_deref()))
        .or_else(|| {
            app.user_id
                .as_deref()
                .map(crate::matrix::fallback_localpart)
        })
        .unwrap_or_else(|| "you".to_owned());
    let clean_name = crate::text::sanitize_line(&name);
    let display = if clean_name.is_empty() {
        "you".to_owned()
    } else {
        clean_name
    };
    let left_reserved = 6 + crate::text::width(title) as u16;
    let avail = area.width.saturating_sub(left_reserved + 2) as usize;
    if avail < 6 {
        return;
    }
    let max_name = avail.saturating_sub(5).min(24);
    let short_name = truncate(&display, max_name);
    let pill_w = (1 + 2 + 1 + crate::text::width(&short_name) + 1) as u16;
    if pill_w >= area.width.saturating_sub(left_reserved) {
        return;
    }
    let pill_x = area.right().saturating_sub(pill_w + 2);
    let pill_rect = Rect::new(pill_x, area.y, pill_w, 1);
    let hovered = app.hover == Some(HitId::Button(Button::OwnProfile));
    let theme = app.theme.clone();
    let label_role = if hovered { Role::Hover } else { Role::Title };
    let color_key = app.user_id.as_deref().unwrap_or(&display);
    let tile = super::image::avatar_tile(&theme, color_key, &display);
    let line = Line::from(vec![
        Span::raw(" "),
        tile,
        Span::raw(" "),
        Span::styled(short_name, theme.style(label_role)),
        Span::raw(" "),
    ]);
    frame.render_widget(line, pill_rect);
    app.hits.add(pill_rect, HitId::Button(Button::OwnProfile));
    if let Some(url) = app.own_profile.as_ref().and_then(|p| p.avatar.as_deref()) {
        let cells = Rect::new(pill_x + 1, area.y, 2, 1);
        let covered = app.modal.is_some() || app.help_open || app.images.viewer_open();
        super::image::draw_avatar(frame, &mut app.images, &theme, url, cells, 0, covered);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_room_kept_for_the_buttons_follows_which_are_shown() {
        // Two buttons of two cells with a space before each, as it always was.
        assert_eq!(reserve(Mic::Hidden), 6);
        assert_eq!(reserve(Mic::Idle), 9);
        assert_eq!(reserve(Mic::Recording), 9);
        assert_eq!(text_width(40, Mic::Hidden), 30);
        assert_eq!(text_width(40, Mic::Idle), 27);
        assert_eq!(text_width(3, Mic::Idle), 1);

        let symbols = |mic| -> Vec<&str> { buttons(mic).into_iter().map(|b| b.0).collect() };
        assert_eq!(symbols(Mic::Hidden), [SYMBOLS.clip, SYMBOLS.smiley]);
        assert_eq!(
            symbols(Mic::Idle),
            [SYMBOLS.mic, SYMBOLS.clip, SYMBOLS.smiley]
        );
        assert_eq!(
            symbols(Mic::Recording),
            [SYMBOLS.stop, SYMBOLS.clip, SYMBOLS.smiley]
        );
        // No symbol is wider than the cells it is given.
        for symbol in [SYMBOLS.mic, SYMBOLS.stop, SYMBOLS.clip, SYMBOLS.smiley] {
            assert!(crate::text::width(symbol) <= usize::from(SYMBOLS.cell));
        }
    }
}
