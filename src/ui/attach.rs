//! The files waiting to be sent, shown above the message text: one line each, with a
//! ✕ to take it off again.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::hit::{HitId, HitMap};
use crate::images::human_size;
use crate::text::{truncate, width};
use crate::theme::{Role, Theme};
use crate::upload::Chosen;

/// Rows given to waiting files at most; further files are counted in the last one.
const MAX_ROWS: usize = 4;
const CLOSE: &str = " ✕ ";

/// Rows needed to show `count` waiting files.
pub fn rows(count: usize) -> u16 {
    count.min(MAX_ROWS) as u16
}

pub fn draw(
    frame: &mut Frame,
    files: &[Chosen],
    theme: &Theme,
    hover: Option<HitId>,
    hits: &mut HitMap,
    area: Rect,
) {
    let available = area.height as usize;
    // With more files than rows, the last row says how many are not shown.
    let listed = if files.len() > available {
        available.saturating_sub(1)
    } else {
        files.len()
    };
    for (index, file) in files.iter().take(listed).enumerate() {
        let row = Rect::new(area.x, area.y + index as u16, area.width, 1);
        let size = format!(" · {}", human_size(file.size));
        let fixed = 3 + width(&size) + width(CLOSE) + 1;
        let name = truncate(
            &file.name,
            (row.width as usize).saturating_sub(fixed).max(1),
        );
        let label = format!("📎 {name}{size}");
        let label_width = (width(&label) as u16).min(row.width);
        frame.render_widget(
            Line::from(Span::styled(label, theme.style(Role::Banner))),
            row,
        );
        let close = Rect::new(row.x + label_width + 1, row.y, width(CLOSE) as u16, 1);
        if close.right() <= row.right() {
            let id = HitId::Attachment(index);
            let role = if hover == Some(id) {
                Role::TitleFocused
            } else {
                Role::Muted
            };
            frame.render_widget(Line::from(Span::styled(CLOSE, theme.style(role))), close);
            hits.add(close, id);
        }
    }
    if listed < files.len() && available > 0 {
        let row = Rect::new(area.x, area.y + listed as u16, area.width, 1);
        let more = files.len() - listed;
        let label = format!(
            "📎 … and {more} more file{}",
            if more == 1 { "" } else { "s" }
        );
        frame.render_widget(
            Line::from(Span::styled(
                truncate(&label, row.width as usize),
                theme.style(Role::Banner),
            )),
            row,
        );
    }
}
