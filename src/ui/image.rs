//! Drawing pictures: between the messages, small ones (room pictures, custom emoji),
//! and in the full-window viewer.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear};
use ratatui_image::sliced::{SignedPosition, SlicedImage, SlicedProtocol};

use super::hit::HitId;
use crate::app::App;
use crate::images::{Encoded, ImageStore, ViewerContent, fit_cells, human_size};
use crate::model::ImageInfo;
use crate::text::{truncate, width};
use crate::theme::{Role, Theme};

/// The part of a message's picture that is on screen.
pub struct Inline<'a> {
    pub info: &'a ImageInfo,
    /// Where the visible rows go.
    pub area: Rect,
    /// Rows of the picture scrolled out above `area`.
    pub skip: u16,
    /// Size of the whole picture in cells.
    pub size: (u16, u16),
    /// Whether this thumbnail represents a playable video (draws a play badge).
    pub play_mark: bool,
    /// A popup is open on top of the timeline.
    pub covered: bool,
}

pub fn draw_inline(frame: &mut Frame, images: &mut ImageStore, theme: &Theme, inline: Inline) {
    let Inline {
        info,
        area,
        skip,
        size,
        play_mark,
        covered,
    } = inline;
    if area.width == 0 || area.height == 0 {
        return;
    }
    images.request(info);
    let area = Rect {
        width: area.width.min(size.0),
        ..area
    };
    match images.inline(&info.key, size, covered) {
        Some(encoded) => {
            let pending = !encoded.shown;
            render(frame, theme, encoded, area, skip);
            if pending {
                images.sent_inline(&info.key, area.x, area.y);
            }
        }
        None => {
            let spinner = images.is_loading(&info.key).then(|| images.spinner());
            placeholder(frame, theme, area, skip, size, spinner, &info.name);
        }
    }
    // Keep the top-left cell `(area.x, area.y)` untouched because Kitty anchors
    // image placement data there; place the badge on the bottom-left row instead.
    if play_mark && area.height >= 2 && area.width >= 6 {
        frame.buffer_mut().set_stringn(
            area.x + 1,
            area.bottom() - 1,
            " ▶ ",
            3,
            theme.style(Role::UnreadBadge),
        );
    }
}

/// Draws a room's picture over the two cells of its coloured tile, once there is
/// one. Until then, and wherever pictures cannot be drawn, the tile stays.
pub fn draw_avatar(
    frame: &mut Frame,
    images: &mut ImageStore,
    theme: &Theme,
    url: &str,
    area: Rect,
    // Rows of the picture above `area` that are scrolled out of view.
    skip: u16,
    covered: bool,
) {
    let Some(encoded) = images.avatar(url, covered) else {
        return;
    };
    let pending = !encoded.shown;
    // The tile's colour stays behind a picture with transparent parts; its bold
    // letter goes.
    frame
        .buffer_mut()
        .set_style(area, Style::new().remove_modifier(Modifier::all()));
    render(frame, theme, encoded, area, skip);
    // Two rooms can share a picture. Only the first one drawn carries the data; the
    // others just refer to it and must not be mistaken for a failed transmission.
    let carries_data = frame
        .buffer_mut()
        .cell((area.x, area.y))
        .is_some_and(|cell| cell.symbol().starts_with('\x1b'));
    if pending && carries_data {
        images.sent_avatar(url, area.x, area.y);
    }
}

/// Draws a custom emoji's picture over the two cells kept for it, once it is there.
/// Until then, and wherever pictures cannot be drawn, the cells stay as they are.
pub fn draw_emote(
    frame: &mut Frame,
    images: &mut ImageStore,
    theme: &Theme,
    url: &str,
    area: Rect,
    covered: bool,
) {
    let Some(encoded) = images.emote(url, covered) else {
        return;
    };
    let pending = !encoded.shown;
    // The background (a selected message, a reaction chip) stays behind the
    // picture's transparent parts; underlines and the like of the text around do not
    // belong on it.
    frame
        .buffer_mut()
        .set_style(area, Style::new().remove_modifier(Modifier::all()));
    render(frame, theme, encoded, area, 0);
    // The same emoji is often on screen many times: only the first one drawn carries
    // the data, as with room pictures.
    let carries_data = frame
        .buffer_mut()
        .cell((area.x, area.y))
        .is_some_and(|cell| cell.symbol().starts_with('\x1b'));
    if pending && carries_data {
        images.sent_emote(url, area.x, area.y);
    }
}

/// Colored 2-cell avatar tile style shared across sidebar, profile popups, and composer.
pub fn avatar_tile_style(theme: &Theme, color_key: &str) -> Style {
    Style::new()
        .fg(theme.style(Role::UnreadBadge).fg.unwrap_or_default())
        .bg(theme.user_color(color_key))
        .add_modifier(Modifier::BOLD)
}

/// Renders a 2-cell colored avatar tile span with the user's or room's initial.
pub fn avatar_tile(theme: &Theme, color_key: &str, name: &str) -> Span<'static> {
    Span::styled(
        super::sidebar::initial(name),
        avatar_tile_style(theme, color_key),
    )
}

/// Puts an encoded picture into the frame, leaving out the first `skip` rows and
/// whatever does not fit into `area`.
fn render(frame: &mut Frame, theme: &Theme, encoded: &Encoded, area: Rect, skip: u16) {
    let position = SignedPosition {
        x: 0,
        y: -(skip.min(i16::MAX as u16) as i16),
    };
    frame.render_widget(SlicedImage::new(&encoded.protocol, position), area);
    if matches!(encoded.protocol, SlicedProtocol::Halfblocks(_)) {
        // Half blocks are 24-bit colours; bring them down where the terminal has fewer.
        let buffer = frame.buffer_mut();
        let downgrade = |color: Color| match color {
            Color::Rgb(r, g, b) => theme.color(r, g, b),
            other => other,
        };
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.fg = downgrade(cell.fg);
                    cell.bg = downgrade(cell.bg);
                }
            }
        }
    }
}

/// A frame the size of the picture, with its name in the middle.
fn placeholder(
    frame: &mut Frame,
    theme: &Theme,
    area: Rect,
    skip: u16,
    size: (u16, u16),
    spinner: Option<&str>,
    name: &str,
) {
    let style = theme.style(Role::Muted);
    let (cols, rows) = (usize::from(area.width), size.1);
    let label = match spinner {
        Some(spinner) => format!("{spinner} {name}"),
        None => name.to_owned(),
    };
    let framed = cols >= 6 && rows >= 3;
    let label_row = if framed { rows / 2 } else { 0 };
    for n in 0..area.height {
        let row = skip + n;
        let text = if !framed {
            if row == label_row {
                truncate(&label, cols)
            } else {
                String::new()
            }
        } else if row == 0 {
            format!("╭{}╮", "─".repeat(cols - 2))
        } else if row + 1 == rows {
            format!("╰{}╯", "─".repeat(cols - 2))
        } else if row == label_row {
            let label = truncate(&label, cols - 4);
            let fill = cols - 4 - width(&label);
            format!(
                "│ {}{label}{} │",
                " ".repeat(fill / 2),
                " ".repeat(fill - fill / 2)
            )
        } else {
            format!("│{}│", " ".repeat(cols - 2))
        };
        frame.render_widget(
            Line::from(Span::styled(text, style)),
            Rect::new(area.x, area.y + n, area.width, 1),
        );
    }
}

/// The viewer: one picture as large as the window allows. Any click closes it.
pub fn draw_viewer(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(viewer) = &app.images.viewer else {
        return;
    };
    let theme = &app.theme;
    let name = viewer.info.name.clone();
    let mut title = name.clone();
    if let Some((w, h)) = viewer.original {
        title.push_str(&format!(" · {w}×{h}"));
    }
    if let Some(size) = viewer.info.size {
        title.push_str(&format!(" · {}", human_size(size)));
    }
    let title = truncate(&title, usize::from(area.width.saturating_sub(6)));
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.style(Role::OverlayBorder))
        .style(theme.style(Role::Text))
        .title(Line::from(Span::styled(
            format!(" {title} "),
            theme.style(Role::TitleFocused),
        )))
        .title_bottom(
            Line::from(Span::styled(" Esc close ", theme.style(Role::Muted))).right_aligned(),
        );
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    app.hits.add(area, HitId::Backdrop);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let covered = app.modal.is_some() || app.help_open;
    let font = app.images.graphics.font;
    let size = app
        .images
        .viewer_pixels()
        .map(|pixels| fit_cells(pixels, font, (inner.width, inner.height)))
        .unwrap_or((1, 1));
    let mut sent = None;
    let message = match app.images.viewer_content(size, covered) {
        ViewerContent::Ready(encoded) => {
            let rect = Rect::new(
                inner.x + (inner.width - size.0.min(inner.width)) / 2,
                inner.y + (inner.height - size.1.min(inner.height)) / 2,
                size.0.min(inner.width),
                size.1.min(inner.height),
            );
            render(frame, theme, encoded, rect, 0);
            if !encoded.shown {
                sent = Some((rect.x, rect.y));
            }
            None
        }
        ViewerContent::Loading => Some((format!("Loading {name}…"), true, Role::Muted)),
        ViewerContent::Preparing => Some(("Preparing the picture…".to_owned(), true, Role::Muted)),
        ViewerContent::Failed(reason) => Some((format!("{name}: {reason}"), false, Role::Error)),
    };
    if let Some((x, y)) = sent {
        app.images.sent_viewer(x, y);
    }
    if let Some((text, busy, role)) = message {
        let text = if busy {
            format!("{} {text}", app.images.spinner())
        } else {
            text
        };
        let text = truncate(&text, usize::from(inner.width));
        let line =
            Line::from(Span::styled(text, app.theme.style(role))).alignment(Alignment::Center);
        frame.render_widget(
            line,
            Rect::new(inner.x, inner.y + inner.height / 2, inner.width, 1),
        );
    }
}

/// Runs last in a frame: checks which picture transmissions survived whatever was drawn
/// after them.
pub fn verify(frame: &mut Frame, app: &mut App) {
    let buffer = frame.buffer_mut();
    app.images.verify(|x, y| {
        buffer
            .cell((x, y))
            .is_some_and(|cell| cell.symbol().starts_with('\x1b'))
    });
}
