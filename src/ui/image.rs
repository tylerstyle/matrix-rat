//! Drawing pictures: between the messages, and in the full-window viewer.

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
    /// A popup is open on top of the timeline.
    pub covered: bool,
}

pub fn draw_inline(frame: &mut Frame, images: &mut ImageStore, theme: &Theme, inline: Inline) {
    let Inline {
        info,
        area,
        skip,
        size,
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
}

/// Draws a room's picture over the two cells of its coloured tile, once there is
/// one. Until then, and wherever pictures cannot be drawn, the tile stays.
pub fn draw_avatar(
    frame: &mut Frame,
    images: &mut ImageStore,
    theme: &Theme,
    url: &str,
    area: Rect,
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
        .set_style(area, Style::new().remove_modifier(Modifier::BOLD));
    render(frame, theme, encoded, area, 0);
    if pending {
        images.sent_avatar(url, area.x, area.y);
    }
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
