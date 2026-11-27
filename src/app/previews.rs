//! Link previews on `App`: handing the requests of the last frame to the worker and
//! taking its answers. Which links are asked about is decided where the timeline is laid
//! out (`ui::timeline`), for the messages that are on screen; what is kept is in
//! `crate::previews`.

use super::App;
use crate::matrix::MatrixCmd;
use crate::previews::Raw;

impl App {
    /// There is a worker to ask (the demo has none).
    pub fn can_fetch_previews(&self) -> bool {
        self.matrix.is_some()
    }

    /// Hands the requests of the last frame to the worker (after a draw).
    pub(super) fn send_preview_requests(&mut self) {
        for url in self.previews.take_requests() {
            self.send_matrix(MatrixCmd::LinkPreview { url });
        }
    }

    /// The worker's answer (`None`: nothing to show). Answers for addresses nobody is
    /// waiting for any more are dropped by the store.
    pub(super) fn on_link_preview(&mut self, url: &str, result: Option<Raw>) {
        if self.previews.on_loaded(url, result) {
            // The rows are laid out again, where the view keeps its place as for a
            // picture that arrives.
            self.dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::action::{Action, Pane};
    use crate::config::LinkPreviews;
    use crate::input::{InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent};
    use crate::matrix::{MatrixEvent, MatrixHandle};
    use crate::modal::Setting;
    use crate::model::{ItemRef, Message, MsgKind, Room, RoomKind, SendState};

    const A: &str = "https://example.org/a";
    const B: &str = "https://example.org/b";

    fn message(n: usize, body: &str) -> Message {
        Message {
            id: format!("m{n}"),
            item: Some(ItemRef::Event(format!("$e{n}"))),
            sender: "ann".into(),
            sender_id: Some("@ann:example.org".into()),
            time: "12:00".into(),
            day: "Today".into(),
            body: body.into(),
            ..Message::default()
        }
    }

    type Fixture = (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>);

    fn app_with(messages: Vec<Message>, encrypted: bool, mode: LinkPreviews) -> Fixture {
        let mut app = App::demo();
        let (handle, commands) = MatrixHandle::for_test();
        app.matrix = Some(handle);
        let mut room = Room::new("!a", "Alpha", RoomKind::Room);
        room.encrypted = encrypted;
        room.messages = messages;
        app.rooms = vec![room];
        app.active = 0;
        app.link_previews = mode;
        app.screen = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.perform(Action::Focus(Pane::Timeline));
        // The view stays where a test puts it.
        app.timeline.reveal_selected = false;
        let terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        (app, terminal, commands)
    }

    /// One frame, and what the run loop does after it.
    fn draw(app: &mut App, terminal: &mut Terminal<TestBackend>) -> String {
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        app.after_draw();
        format!("{}", terminal.backend())
    }

    /// The addresses asked about since the last call.
    fn asked(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<String> {
        let mut urls = Vec::new();
        while let Ok(command) = commands.try_recv() {
            if let MatrixCmd::LinkPreview { url } = command {
                urls.push(url);
            }
        }
        urls
    }

    fn answer(app: &mut App, url: &str, title: &str, description: &str) {
        let some = |s: &str| (!s.is_empty()).then(|| s.to_owned());
        app.on_matrix(MatrixEvent::LinkPreview {
            url: url.to_owned(),
            result: Some(Raw {
                title: some(title),
                site: None,
                description: some(description),
            }),
        });
    }

    #[test]
    fn nothing_is_asked_unless_the_setting_is_on() {
        let (mut app, mut terminal, mut commands) = app_with(
            vec![message(0, &format!("look {A}"))],
            false,
            LinkPreviews::Off,
        );
        draw(&mut app, &mut terminal);
        assert!(asked(&mut commands).is_empty());
        // A fresh configuration is off, too.
        assert_eq!(
            App::new(&crate::config::Config::default(), Default::default(), None).link_previews,
            LinkPreviews::Off
        );
    }

    #[test]
    fn only_the_first_link_is_asked_about_and_once() {
        let (mut app, mut terminal, mut commands) = app_with(
            vec![message(0, &format!("look {A} and {B}"))],
            false,
            LinkPreviews::Unencrypted,
        );
        draw(&mut app, &mut terminal);
        assert_eq!(asked(&mut commands), [A]);
        // Every frame the link is on screen asks again; the store does not pass it on.
        draw(&mut app, &mut terminal);
        draw(&mut app, &mut terminal);
        assert!(asked(&mut commands).is_empty());
        // The failure is remembered, too.
        app.on_matrix(MatrixEvent::LinkPreview {
            url: A.into(),
            result: None,
        });
        draw(&mut app, &mut terminal);
        assert!(asked(&mut commands).is_empty());
    }

    #[test]
    fn encrypted_rooms_are_asked_about_only_for_all_rooms() {
        for (mode, encrypted, wanted) in [
            (LinkPreviews::Unencrypted, true, false),
            (LinkPreviews::Unencrypted, false, true),
            (LinkPreviews::All, true, true),
            (LinkPreviews::All, false, true),
            (LinkPreviews::Off, false, false),
            (LinkPreviews::Off, true, false),
        ] {
            let (mut app, mut terminal, mut commands) =
                app_with(vec![message(0, &format!("look {A}"))], encrypted, mode);
            draw(&mut app, &mut terminal);
            assert_eq!(
                asked(&mut commands).len(),
                usize::from(wanted),
                "{mode:?} encrypted: {encrypted}"
            );
        }
    }

    #[test]
    fn a_room_comes_from_the_server_as_encrypted_until_it_is_known_not_to_be() {
        let (mut app, mut terminal, mut commands) =
            app_with(Vec::new(), false, LinkPreviews::Unencrypted);
        let summary = |encrypted| crate::matrix::RoomSummary {
            id: "!a".into(),
            name: "Alpha".into(),
            category: crate::matrix::RoomCategory::Room,
            notifications: 0,
            preview: None,
            server_notifications: 0,
            mention: false,
            unread: false,
            marked_unread: false,
            avatar: None,
            alias: None,
            direct: false,
            muted: false,
            encrypted,
        };
        app.rooms[0].messages = vec![message(0, &format!("look {A}"))];
        app.on_matrix(MatrixEvent::Rooms(vec![summary(true)]));
        assert!(app.rooms[0].encrypted);
        draw(&mut app, &mut terminal);
        assert!(asked(&mut commands).is_empty());
        app.on_matrix(MatrixEvent::Rooms(vec![summary(false)]));
        assert!(!app.rooms[0].encrypted);
        draw(&mut app, &mut terminal);
        assert_eq!(asked(&mut commands), [A]);
    }

    #[test]
    fn only_web_links_of_ordinary_messages_from_people_not_ignored_are_asked_about() {
        let mut redacted = message(1, &format!("gone {A}"));
        redacted.kind = MsgKind::Redacted;
        let mut undecryptable = message(2, A);
        undecryptable.kind = MsgKind::Undecryptable;
        let mut ignored = message(3, &format!("ignored {A}"));
        ignored.sender_id = Some("@spam:example.org".into());
        let mut sending = message(4, &format!("sending {A}"));
        sending.state = SendState::Sending;
        let mut file = message(5, A);
        file.file = Some(crate::model::Attachment {
            key: "mxc://example.org/f".into(),
            name: "f.txt".into(),
            size: Some(1),
            kind: crate::model::MediaKind::File,
            source: Box::new(matrix_sdk::ruma::events::room::MediaSource::Plain(
                "mxc://example.org/f".into(),
            )),
            audio: None,
        });
        let (mut app, mut terminal, mut commands) = app_with(
            vec![
                message(0, "see https://matrix.to/#/@ann:example.org"),
                redacted,
                undecryptable,
                ignored,
                sending,
                file,
                // The first link decides, not the first one that would do.
                message(6, &format!("mail:me@example.org matrix:r/x:y then {B}")),
                message(7, "mailto:ann@example.org"),
            ],
            false,
            LinkPreviews::All,
        );
        app.ignored_users.insert("@spam:example.org".into());
        draw(&mut app, &mut terminal);
        assert!(asked(&mut commands).is_empty(), "nothing qualifies");
        // And one that does.
        app.rooms[0].messages.push(message(8, &format!("ok {B}")));
        app.timeline_rev += 1;
        draw(&mut app, &mut terminal);
        assert_eq!(asked(&mut commands), [B]);
    }

    #[test]
    fn only_what_is_on_screen_is_asked_about_and_only_a_few_at_a_time() {
        let messages = (0..60)
            .map(|n| message(n, &format!("n{n} https://example.org/{n}")))
            .collect();
        let (mut app, mut terminal, mut commands) = app_with(messages, false, LinkPreviews::All);
        draw(&mut app, &mut terminal);
        let first = asked(&mut commands);
        // The newest ones are on screen (a screenful is 24 rows); three at a time.
        assert_eq!(first.len(), crate::previews::MAX_IN_FLIGHT);
        let number = |url: &String| url.rsplit('/').next().unwrap().parse::<usize>().unwrap();
        assert!(first.iter().all(|url| number(url) >= 36), "{first:?}");
        // Answers make room for the next ones on the screen; none for what is not.
        let mut seen = first.clone();
        for round in 0..30 {
            for url in asked(&mut commands).iter().chain(&seen.clone()) {
                app.on_matrix(MatrixEvent::LinkPreview {
                    url: url.clone(),
                    result: None,
                });
            }
            draw(&mut app, &mut terminal);
            let more = asked(&mut commands);
            assert!(
                more.len() <= crate::previews::MAX_IN_FLIGHT,
                "round {round}"
            );
            seen.extend(more);
        }
        assert!(seen.iter().all(|url| number(url) >= 36), "{seen:?}");
        assert!(seen.len() >= 4, "the screen shows more than three links");
        // Scrolled to the oldest: those are asked about now.
        app.timeline.scroll = usize::MAX;
        draw(&mut app, &mut terminal);
        draw(&mut app, &mut terminal);
        let old = asked(&mut commands);
        assert!(
            !old.is_empty() && old.iter().all(|url| number(url) < 24),
            "{old:?}"
        );
    }

    #[test]
    fn a_preview_shows_under_its_message_and_is_plain_text() {
        let (mut app, mut terminal, mut commands) = app_with(
            vec![
                message(0, "first"),
                message(1, &format!("look {A}")),
                message(2, "last"),
            ],
            false,
            LinkPreviews::Unencrypted,
        );
        draw(&mut app, &mut terminal);
        assert_eq!(asked(&mut commands), [A]);
        answer(
            &mut app,
            A,
            "An <i>example</i> **page**",
            "A long description that goes on and on, so that it needs more than one line of \
             the pane to be read and then still goes on and on past the end of the second line \
             and gets cut there with an ellipsis, because two lines is all it is given",
        );
        let text = draw(&mut app, &mut terminal);
        insta::assert_snapshot!("link_preview", text);
        // Title, then at most two lines of description; the next message follows.
        let lines: Vec<&str> = text.lines().collect();
        let title = lines
            .iter()
            .position(|l| l.contains("An <i>example</i> **page**"));
        let title = title.expect("the title is shown as it is, not as markup");
        assert!(lines[title - 1].contains("look https://example.org/a"));
        assert!(lines[title + 1].contains("A long description"));
        assert!(lines[title + 2].contains('…'));
        assert!(lines[title + 3].contains("last") || lines[title + 4].contains("last"));
    }

    #[test]
    fn what_the_server_sends_cannot_reach_the_terminal_as_anything_but_text() {
        let (mut app, mut terminal, _commands) = app_with(
            vec![message(0, &format!("look {A}"))],
            false,
            LinkPreviews::All,
        );
        draw(&mut app, &mut terminal);
        answer(
            &mut app,
            A,
            "Title\u{1b}]52;c;AAAA\u{7}\u{202e}reversed",
            "Line one\r\nline\u{9b}2J two",
        );
        let text = draw(&mut app, &mut terminal);
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{202e}'),
            "{text}"
        );
        assert!(text.contains("Title]52;c;AAAAreversed"));
        assert!(text.contains("Line one line2J two"));
        // And no row of the screen is beyond the pane because of it.
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|cell| !cell.symbol().chars().any(char::is_control))
        );
    }

    #[test]
    fn late_and_unasked_answers_change_nothing() {
        let (mut app, mut terminal, mut commands) = app_with(
            vec![message(0, &format!("look {A}"))],
            false,
            LinkPreviews::All,
        );
        // Nobody asked about B.
        answer(&mut app, B, "Never asked", "");
        draw(&mut app, &mut terminal);
        assert_eq!(asked(&mut commands), [A]);
        // Logging out forgets everything; a late answer is not taken.
        app.previews.clear();
        answer(&mut app, A, "Too late", "");
        let text = draw(&mut app, &mut terminal);
        assert!(!text.contains("Too late") && !text.contains("Never asked"));
        // The address is asked about afresh for the next session.
        assert_eq!(asked(&mut commands), [A]);
    }

    #[test]
    fn turning_previews_off_hides_them_and_asks_nothing_more() {
        let (mut app, mut terminal, mut commands) = app_with(
            vec![message(0, &format!("look {A}"))],
            false,
            LinkPreviews::All,
        );
        draw(&mut app, &mut terminal);
        asked(&mut commands);
        answer(&mut app, A, "Visible", "");
        assert!(draw(&mut app, &mut terminal).contains("Visible"));
        app.link_previews = LinkPreviews::Off;
        app.timeline_rev += 1;
        assert!(!draw(&mut app, &mut terminal).contains("Visible"));
        assert!(asked(&mut commands).is_empty());
        // And it is there again at once when they are turned on again.
        app.link_previews = LinkPreviews::All;
        assert!(draw(&mut app, &mut terminal).contains("Visible"));
    }

    #[test]
    fn the_view_stays_where_it_is_when_a_preview_arrives() {
        let messages = (0..40)
            .map(|n| {
                let body = if n == 30 {
                    format!("n30 {A}")
                } else {
                    format!("n{n} filler")
                };
                message(n, &body)
            })
            .collect();
        let (mut app, mut terminal, mut commands) = app_with(messages, false, LinkPreviews::All);
        // Reading older messages: the newest are below the view.
        app.timeline.scroll = 6;
        draw(&mut app, &mut terminal);
        draw(&mut app, &mut terminal);
        assert_eq!(asked(&mut commands), [A]);
        let row_of = |text: &str, needle: &str| {
            text.lines()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle} is not on screen:\n{text}"))
        };
        let before = draw(&mut app, &mut terminal);
        answer(&mut app, A, "A page", "Its description");
        let after = draw(&mut app, &mut terminal);
        assert!(after.contains("A page"));
        // What was below the message stays where it was; what was above moves up by
        // what the preview needs.
        assert_eq!(row_of(&before, "n33 filler"), row_of(&after, "n33 filler"));
        assert_eq!(row_of(&before, "n31 filler"), row_of(&after, "n31 filler"));
        assert!(row_of(&after, "n29 filler") < row_of(&before, "n29 filler"));
        assert_eq!(app.timeline.scroll, 6);
    }

    #[test]
    fn a_preview_under_the_last_row_of_the_view_comes_into_view_like_a_picture_does() {
        let messages = (0..40)
            .map(|n| {
                let body = if n == 30 {
                    format!("n30 {A}")
                } else {
                    format!("n{n} filler")
                };
                message(n, &body)
            })
            .collect();
        let (mut app, mut terminal, mut commands) = app_with(messages, false, LinkPreviews::All);
        // The message with the link is the bottom row of the view.
        app.timeline.scroll = 9;
        draw(&mut app, &mut terminal);
        draw(&mut app, &mut terminal);
        assert_eq!(asked(&mut commands), [A]);
        let row_of = |text: &str, needle: &str| text.lines().position(|l| l.contains(needle));
        let before = draw(&mut app, &mut terminal);
        let bottom = row_of(&before, "n30 https").unwrap();
        answer(&mut app, A, "A page", "Its description");
        let after = draw(&mut app, &mut terminal);
        // The view keeps its bottom row: the same message is still its last. The preview
        // is under it, so the text moves up by the rows the preview takes; nothing jumps
        // to the newest message.
        assert_eq!(row_of(&after, "Its description"), Some(bottom));
        assert_eq!(row_of(&after, "n30 https"), Some(bottom - 2));
        assert_eq!(app.timeline.scroll, 9);
        assert!(!after.contains("n39 filler"));
    }

    #[test]
    fn the_preview_belongs_to_its_message_and_o_still_opens_the_link() {
        let (mut app, mut terminal, mut commands) = app_with(
            vec![message(0, &format!("look {A}")), message(1, "other")],
            false,
            LinkPreviews::All,
        );
        draw(&mut app, &mut terminal);
        asked(&mut commands);
        answer(&mut app, A, "Preview title", "Preview text");
        let text = draw(&mut app, &mut terminal);
        // A click on the preview selects the message (nothing else happens).
        let (row, line) = text
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("Preview title"))
            .unwrap();
        let column = line.trim_start_matches('"').find("Preview").unwrap();
        let before = line.trim_start_matches('"')[..column].chars().count();
        app.timeline.selected = Some(1);
        app.on_event(InputEvent::Mouse(MouseEvent {
            kind: crate::input::MouseEventKind::Down(MouseButton::Left),
            column: before as u16 + 1,
            row: row as u16,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.timeline.selected, Some(0));
        assert!(app.modal.is_none());
        let opened = |app: &mut App| {
            app.on_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('o'),
                KeyModifiers::NONE,
            )));
            app.take_opened()
        };
        assert_eq!(opened(&mut app), [A]);
    }

    #[test]
    fn the_setting_cycles_and_is_saved() {
        let dir = std::env::temp_dir().join(format!("matrix-rat-linkprev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        let (mut app, _terminal, _commands) = app_with(Vec::new(), false, LinkPreviews::Off);
        app.set_config_file(&file);
        assert_eq!(app.setting_value(Setting::LinkPreviews), "off");
        for (value, saved) in [
            ("unencrypted rooms", "unencrypted"),
            ("all rooms", "all"),
            ("off", "off"),
        ] {
            app.perform(Action::Setting(Setting::LinkPreviews));
            assert_eq!(app.setting_value(Setting::LinkPreviews), value);
            let config = crate::config::Config::load(&file).unwrap();
            assert_eq!(config.link_previews.name(), saved);
            assert!(app.toast.as_ref().unwrap().0.contains(value));
        }
        // The hint says what the cost is.
        let hint = Setting::LinkPreviews.hint();
        assert!(hint.contains("homeserver") && hint.contains("encrypted"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
