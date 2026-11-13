//! The find bar: a small box at the top right of the screen that leaves the messages
//! visible, since they are what the search moves through.

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear};

use super::hit::HitId;
use crate::app::App;
use crate::app::search::SearchBar;
use crate::text::{truncate, width};
use crate::theme::Role;

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect, bar: &SearchBar) {
    let box_width = 46.min(area.width.saturating_sub(2));
    if box_width < 16 || area.height < 5 {
        return;
    }
    let rect = Rect::new(area.right() - box_width - 1, area.y + 1, box_width, 3);
    // Clicking anywhere else closes it; the messages stay undimmed.
    app.hits.add(area, HitId::Backdrop);
    app.hits.add(rect, HitId::Overlay);
    let theme = &app.theme;
    frame.render_widget(Clear, rect);
    let footer = "Enter older · ↓ newer · Esc";
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.style(Role::OverlayBorder))
        .style(theme.style(Role::Text))
        .title(Line::from(Span::styled(
            " Find ",
            theme.style(Role::TitleFocused),
        )))
        .title_bottom(
            Line::from(Span::styled(
                format!(
                    " {} ",
                    truncate(footer, box_width.saturating_sub(4) as usize)
                ),
                theme.style(Role::Muted),
            ))
            .right_aligned(),
        );
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let status = match (bar.note, bar.total) {
        (Some(note), _) => note.to_owned(),
        (None, 0) if bar.query.trim().is_empty() => String::new(),
        (None, 0) => "no match".to_owned(),
        (None, total) => format!("{}/{total}", bar.position),
    };
    let room = inner.width.saturating_sub(3) as usize;
    let status = truncate(&status, room.saturating_sub(8));
    let text_room = room.saturating_sub(width(&status) + 1);
    // The end of a long text, where typing happens.
    let mut shown = bar.query.as_str();
    {
        use unicode_segmentation::UnicodeSegmentation;
        let mut starts = bar.query.grapheme_indices(true).map(|(at, _)| at);
        while width(shown) > text_room {
            match starts.next() {
                Some(at) => shown = &bar.query[at..],
                None => break,
            }
        }
        if width(shown) > text_room {
            shown = "";
        }
    }
    let gap = room.saturating_sub(width(shown) + width(&status));
    let line = Line::from(vec![
        Span::styled(" ⌕ ", theme.style(Role::StatusKey)),
        Span::raw(shown.to_owned()),
        Span::raw(" ".repeat(gap)),
        Span::styled(status, theme.style(Role::Muted)),
    ]);
    frame.render_widget(line, inner);
    frame.set_cursor_position(Position::new(
        (inner.x + 3 + width(shown) as u16).min(inner.right().saturating_sub(1)),
        inner.y,
    ));
}
