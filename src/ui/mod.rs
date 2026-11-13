//! Drawing. Each frame also rebuilds the hit map used for mouse handling.

mod composer;
mod help;
pub mod hit;
mod modal;
mod sidebar;
mod status;
mod timeline;
mod verify;

pub use timeline::RowCache;

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
    modal::draw(frame, app, area);
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
    fn emoji_picker() {
        let mut app = App::demo();
        render(&mut app, 100, 30);
        app.perform(Action::OpenEmoji);
        for c in "cat".chars() {
            app.on_event(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char(c),
                    crossterm::event::KeyModifiers::NONE,
                ),
            ));
        }
        insta::assert_snapshot!(render(&mut app, 100, 30));
    }

    #[test]
    fn message_kinds() {
        use crate::model::{Message, MsgKind, SendState};
        let mut app = App::demo();
        let base = |sender: &str, body: &str| Message {
            sender: sender.into(),
            time: "12:00".into(),
            day: "Today".into(),
            body: body.into(),
            ..Message::default()
        };
        let room = &mut app.rooms[0];
        room.typing = None;
        room.messages = vec![
            Message {
                kind: MsgKind::System,
                ..base("", "alice created the room")
            },
            base("alice", "A normal message."),
            Message {
                kind: MsgKind::Emote,
                ..base("bob", "waves")
            },
            Message {
                kind: MsgKind::Redacted,
                ..base("bob", "Message deleted")
            },
            Message {
                kind: MsgKind::Undecryptable,
                ..base("bob", "Unable to decrypt (waiting for keys)")
            },
            Message {
                kind: MsgKind::ReadMarker,
                ..Message::default()
            },
            Message {
                reply_to: Some((String::new(), "an earlier message".into())),
                ..base("alice", "A reply.")
            },
            Message {
                state: SendState::Sending,
                own: true,
                ..base("you", "On its way")
            },
            Message {
                state: SendState::Failed,
                own: true,
                ..base("you", "Did not make it")
            },
        ];
        insta::assert_snapshot!(render(&mut app, 70, 26));
    }

    #[test]
    fn formatted_messages() {
        use crate::model::Message;
        let mut app = App::demo();
        let html = |html: &str| Message {
            sender: "alice".into(),
            time: "12:00".into(),
            day: "Today".into(),
            body: "plain fallback".into(),
            html: Some(html.into()),
            ..Message::default()
        };
        let room = &mut app.rooms[0];
        room.typing = None;
        room.messages = vec![
            html(
                "Some <strong>bold</strong>, <em>italic</em>, <del>struck</del> and <code>code</code>.",
            ),
            html(
                "<blockquote><p>A quote that is long enough to need wrapping onto a second row here.</p></blockquote><p>And the answer.</p>",
            ),
            html("<ul><li>first</li><li>second</li></ul><ol><li>one</li><li>two</li></ol>"),
            html(
                "<pre><code class=\"language-rust\">fn main() {\n    println!(\"hi\");\n}\n</code></pre>",
            ),
            html(
                "A <a href=\"https://example.org\">link</a> and a <span data-mx-spoiler>secret</span>.",
            ),
        ];
        insta::assert_snapshot!("formatted_covered", render(&mut app, 64, 30));
        // Selecting the message uncovers its spoiler.
        app.perform(Action::SelectOlder);
        insta::assert_snapshot!("formatted_revealed", render(&mut app, 64, 30));
    }

    #[test]
    fn login_form() {
        let mut app = App::demo();
        app.rooms.clear();
        app.on_matrix(crate::matrix::MatrixEvent::NeedLogin { reason: None });
        insta::assert_snapshot!(render(&mut app, 100, 30));
    }

    #[test]
    fn tiny_sizes_do_not_panic() {
        use crate::modal::{EmojiPicker, LoginForm, Modal, RecoveryForm};
        let modals = [
            None,
            Some(Modal::Emoji(EmojiPicker::default())),
            Some(Modal::Login(LoginForm::new(Some("an error".into())))),
            Some(Modal::Recovery(RecoveryForm::default())),
        ];
        for modal in modals {
            let mut app = App::demo();
            app.perform(Action::ToggleHelp);
            app.modal = modal;
            for (w, h) in [
                (1, 1),
                (3, 2),
                (10, 4),
                (20, 3),
                (25, 7),
                (61, 5),
                (200, 2),
                (80, 24),
            ] {
                render(&mut app, w, h);
                if app.modal.is_none() {
                    app.perform(Action::FocusNext);
                }
            }
        }
    }
}
