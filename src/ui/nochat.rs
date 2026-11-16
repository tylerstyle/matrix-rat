//! What the chat area shows while no chat is open (see `App::close_chat`): the name of
//! the program and the few keys that lead on from here.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType};

use super::hit::HitId;
use crate::action::Action;
use crate::app::App;
use crate::keymap::Context;
use crate::text::{truncate, width};
use crate::theme::Role;

const NAME: &str = "matrix-rat";

/// The ways on from here, with the key each has right now. One without a key is not
/// shown: the room list and the status line still offer it to the mouse.
fn hints(app: &App) -> Vec<(String, &'static str, Action)> {
    // The room list has the focus while no chat is open: these are its keys.
    let key = |action: Action, last: bool| {
        let labels = app.keymap.labels_from(Context::Sidebar, action);
        let label = if last { labels.last() } else { labels.first() };
        label.map(|label| (*label).to_owned())
    };
    [
        (Action::SidebarOpen, false, "open the selected room"),
        // Of the two default keys the later one is the one named everywhere else
        // (as in the command palette).
        (Action::FocusRoomFilter, true, "find a room"),
        (Action::OpenPalette, false, "command palette"),
        (Action::ToggleHelp, false, "all keys"),
    ]
    .into_iter()
    .filter_map(|(action, last, text)| Some((key(action, last)?, text, action)))
    .collect()
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    // Not a pane: it cannot have the focus, so it has no number and is no place to
    // click on for it (only the lines below are, each for what its key does).
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(app.theme.style(Role::Border))
        .title(Line::from(Span::styled(
            " No chat open ",
            app.theme.style(Role::Title),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 4 || inner.height == 0 {
        return;
    }
    let hints = hints(app);
    let key_width = hints
        .iter()
        .map(|(key, _, _)| width(key))
        .max()
        .unwrap_or(0);
    let text_width = hints
        .iter()
        .map(|(_, text, _)| width(text))
        .max()
        .unwrap_or(0);
    // Keys in one column, flush right; what they do next to them.
    let block_width = (key_width + 2 + text_width).min(inner.width as usize);

    // The name with a line of air below it, where there is room for more than the keys.
    let titled = inner.height as usize >= hints.len() + 2;
    let rows = hints.len() + if titled { 2 } else { 0 };
    let top = inner.y + (inner.height.saturating_sub(rows as u16)) / 2;
    let left = inner.x + (inner.width - block_width as u16) / 2;
    let mut y = top;
    if titled {
        let name = truncate(NAME, inner.width as usize);
        let x = inner.x + (inner.width - width(&name) as u16) / 2;
        frame.render_widget(
            Line::from(Span::styled(name, app.theme.style(Role::TitleFocused))),
            Rect::new(x, y, inner.right() - x, 1),
        );
        y += 2;
    }
    let key_style = app.theme.style(Role::StatusKey);
    let text_style = app.theme.style(Role::Muted);
    for (key, text, action) in hints {
        if y >= inner.bottom() {
            break;
        }
        let rect = Rect::new(left, y, block_width as u16, 1);
        let pad = " ".repeat(key_width - width(&key));
        let room = block_width.saturating_sub(key_width + 2);
        frame.render_widget(
            Line::from(vec![
                Span::raw(pad),
                Span::styled(key, key_style),
                Span::raw("  "),
                Span::styled(truncate(text, room), text_style),
            ]),
            rect,
        );
        // The mouse's way: a click on a line does what its key does.
        app.hits.add(rect, HitId::Do(action));
        y += 1;
    }
}
