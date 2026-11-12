//! Drawing. Each frame also rebuilds the hit map used for mouse handling.

mod composer;
mod help;
pub mod hit;
mod sidebar;
mod status;
mod timeline;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType};

use crate::action::Pane;
use crate::app::App;
use crate::theme::Role;
use hit::HitId;

/// Below this width the room list and the chat are shown one at a time.
pub const NARROW_WIDTH: u16 = 60;
pub const SIDEBAR_MIN: u16 = 16;

pub fn draw(frame: &mut Frame, app: &mut App) {
    app.hits.clear();
    let area = frame.area();
    app.screen = area;
    frame
        .buffer_mut()
        .set_style(area, app.theme.style(Role::Text));

    let [main, status_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);

    let narrow = area.width < NARROW_WIDTH;
    let (sidebar_area, chat_area) = if narrow {
        if app.focus == Pane::Sidebar {
            (Some(main), None)
        } else {
            (None, Some(main))
        }
    } else {
        let width = app
            .sidebar
            .width
            .clamp(SIDEBAR_MIN, max_sidebar(area.width));
        let [side, chat] =
            Layout::horizontal([Constraint::Length(width), Constraint::Fill(1)]).areas(main);
        (Some(side), Some(chat))
    };

    if let Some(area) = sidebar_area {
        sidebar::draw(frame, app, area);
        if !narrow {
            // The room list's right border doubles as the resize handle.
            let handle = Rect::new(area.right().saturating_sub(1), area.y, 1, area.height);
            app.hits.add(handle, HitId::Splitter);
        }
    }
    if let Some(area) = chat_area {
        let composer_height = composer::height(app, area.width);
        let [timeline_area, composer_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(composer_height)])
                .areas(area);
        timeline::draw(frame, app, timeline_area);
        composer::draw(frame, app, composer_area);
    }
    status::draw(frame, app, status_area);
    if app.help_open {
        help::draw(frame, app, area);
    }
}

pub fn max_sidebar(screen_width: u16) -> u16 {
    (screen_width / 2).max(SIDEBAR_MIN)
}

/// The bordered frame of a pane. The digit in the title is its `Alt-<n>` shortcut.
fn pane_block(app: &App, pane: Pane, title: &str) -> Block<'static> {
    let focused = app.focus == pane;
    let (border, title_style) = if focused {
        (Role::BorderFocused, Role::TitleFocused)
    } else {
        (Role::Border, Role::Title)
    };
    let digit = match pane {
        Pane::Sidebar => 1,
        Pane::Timeline => 2,
        Pane::Composer => 3,
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(app.theme.style(border))
        .title(Line::from(Span::styled(
            format!(" {digit} {title} "),
            app.theme.style(title_style),
        )))
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::action::{Action, Pane};
    use crate::app::App;

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| super::draw(frame, app)).unwrap();
        format!("{}", terminal.backend())
    }

    #[test]
    fn main_screen() {
        let mut app = App::demo();
        insta::assert_snapshot!(render(&mut app, 100, 30));
    }

    #[test]
    fn message_selected_and_reply() {
        let mut app = App::demo();
        app.perform(Action::SelectOlder);
        app.perform(Action::SelectOlder);
        app.perform(Action::Reply);
        app.composer.insert("Nice, congrats!\nSecond line");
        insta::assert_snapshot!(render(&mut app, 100, 30));
    }

    #[test]
    fn narrow_shows_one_pane() {
        let mut app = App::demo();
        insta::assert_snapshot!("narrow_chat", render(&mut app, 50, 20));
        app.perform(Action::Focus(Pane::Sidebar));
        insta::assert_snapshot!("narrow_rooms", render(&mut app, 50, 20));
    }

    #[test]
    fn help_overlay() {
        let mut app = App::demo();
        app.perform(Action::ToggleHelp);
        insta::assert_snapshot!(render(&mut app, 100, 30));
    }

    #[test]
    fn room_filter() {
        let mut app = App::demo();
        app.perform(Action::FocusRoomFilter);
        for c in "ma".chars() {
            app.type_char(c);
        }
        insta::assert_snapshot!(render(&mut app, 100, 16));
    }

    #[test]
    fn tiny_sizes_do_not_panic() {
        let mut app = App::demo();
        app.perform(Action::ToggleHelp);
        for (w, h) in [(1, 1), (3, 2), (10, 4), (20, 3), (61, 5), (200, 2)] {
            render(&mut app, w, h);
            app.perform(Action::FocusNext);
        }
    }
}
