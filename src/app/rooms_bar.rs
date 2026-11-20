//! The rooms bar's side of the app (the `rooms_bar` setting; drawn by
//! `ui::rooms_bar`): which rooms it shows, and the key that goes to the first of them.
//!
//! A room is in the bar when it has news that notifies (`Room::notifies`, the rule the
//! window title counts by) and is not the open one. The order is that of `rooms`,
//! which the worker hands over sorted by the time of the latest message, the newest
//! first; the room list keeps the same order inside each of its sections.

use super::App;

impl App {
    /// The rooms with new notifications, the newest first, without the open one.
    pub fn active_rooms(&self) -> Vec<usize> {
        let open = self.open_index();
        self.rooms
            .iter()
            .enumerate()
            .filter(|(index, room)| Some(*index) != open && room.notifies())
            .map(|(index, _)| index)
            .collect()
    }

    /// Opens the first room of the bar. Opening it takes it out of the bar, so the
    /// same key goes on to the next one.
    pub(super) fn open_active_room(&mut self) {
        match self.active_rooms().first() {
            // `show_room`: the room list follows, out of a space the room is not in.
            Some(&index) => self.show_room(index),
            None => self.show_toast("No room has new notifications"),
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::action::{Action, Pane};
    use crate::config::Config;
    use crate::model::{Room, RoomKind};

    fn room(id: &str, kind: RoomKind, unread: u32) -> Room {
        let mut room = Room::new(id, id, kind);
        room.unread = unread;
        room
    }

    /// The demo with five rooms; the first one is open.
    fn app() -> App {
        let mut app = App::demo();
        app.rooms = vec![
            room("!open", RoomKind::Room, 0),
            room("!quiet", RoomKind::Room, 0),
            room("!new", RoomKind::Person, 2),
            room("!older", RoomKind::Favourite, 7),
            room("!low", RoomKind::LowPriority, 1),
        ];
        app
    }

    fn names(app: &App) -> Vec<&str> {
        app.active_rooms()
            .into_iter()
            .map(|index| app.rooms[index].id.as_str())
            .collect()
    }

    fn open_id(app: &App) -> Option<&str> {
        app.room().map(|room| room.id.as_str())
    }

    fn toast(app: &App) -> Option<&str> {
        app.toast.as_ref().map(|(text, _)| text.as_str())
    }

    #[test]
    fn the_bar_holds_the_rooms_that_notify_in_the_order_of_the_list() {
        let mut app = app();
        // Whatever section a room is in: the order the worker gave, newest first.
        assert_eq!(names(&app), ["!new", "!older", "!low"]);
        // Muted: no news, as for the window title.
        app.rooms[3].muted = true;
        assert_eq!(names(&app), ["!new", "!low"]);
        // Bold or marked unread by hand, without a number: nothing that notifies.
        app.rooms[1].bold = true;
        app.rooms[1].marked_unread = true;
        assert_eq!(names(&app), ["!new", "!low"]);
        // Invitations and join requests are not messages.
        app.rooms.push(room("!invite", RoomKind::Invite, 1));
        app.rooms.push(room("!knock", RoomKind::Knock, 1));
        assert_eq!(names(&app), ["!new", "!low"]);
        // A mention is one of the messages counted already.
        app.rooms[4].mention = true;
        assert_eq!(names(&app), ["!new", "!low"]);
    }

    #[test]
    fn the_bar_counts_what_the_window_title_counts() {
        let mut app = app();
        app.rooms[3].muted = true;
        app.rooms.push(room("!invite", RoomKind::Invite, 4));
        let shown: u64 = app
            .active_rooms()
            .into_iter()
            .map(|index| u64::from(app.rooms[index].unread))
            .sum();
        // The open room has no count: the two agree.
        assert_eq!(shown, super::super::title::unread_count(&app.rooms));
    }

    #[test]
    fn the_open_room_is_not_in_the_bar_and_a_closed_one_is() {
        let mut app = app();
        // The server may still count messages of the room that is on screen.
        app.rooms[0].unread = 3;
        assert_eq!(names(&app), ["!new", "!older", "!low"]);
        // With no chat open, no room is the open one.
        app.perform(Action::CloseChat);
        assert!(app.chat_closed());
        assert_eq!(names(&app), ["!open", "!new", "!older", "!low"]);
    }

    #[test]
    fn the_key_opens_the_newest_active_room_and_then_the_next_one() {
        let mut app = app();
        app.on_event(Event::Key(KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::ALT,
        )));
        assert_eq!(open_id(&app), Some("!new"));
        assert_eq!(app.focus, Pane::Composer);
        // Opened, it has no count left and is out of the bar.
        assert_eq!(names(&app), ["!older", "!low"]);
        app.perform(Action::RoomActive);
        assert_eq!(open_id(&app), Some("!older"));
        app.perform(Action::RoomActive);
        assert_eq!(open_id(&app), Some("!low"));
        // Nothing is left: the room stays, and a line says why.
        assert_eq!(toast(&app), None);
        app.perform(Action::RoomActive);
        assert_eq!(open_id(&app), Some("!low"));
        assert_eq!(toast(&app), Some("No room has new notifications"));
    }

    #[test]
    fn the_key_works_with_no_chat_open_and_leaves_a_space_the_room_is_not_in() {
        use crate::matrix::SpaceSummary;
        let mut app = app();
        app.spaces = vec![SpaceSummary {
            id: "!space".into(),
            name: "Work".into(),
            level: 0,
            rooms: vec!["!open".into(), "!quiet".into()],
            ..Default::default()
        }];
        app.select_space(Some(0));
        assert!(app.space.is_some());
        app.perform(Action::CloseChat);
        assert!(!Action::RoomActive.needs_open_chat());
        app.perform(Action::RoomActive);
        assert_eq!(open_id(&app), Some("!new"));
        // The list shows all rooms again, with its cursor on the room.
        assert!(app.space.is_none());
        assert_eq!(app.cursor_room(), app.open_index());
    }

    #[test]
    fn closing_the_chat_is_an_action_that_does_what_escape_does_at_its_last_step() {
        let mut by_action = app();
        let mut by_escape = app();
        by_action.perform(Action::CloseChat);
        by_escape.perform(Action::Back);
        for app in [&by_action, &by_escape] {
            assert!(app.chat_closed());
            assert_eq!(app.focus, Pane::Sidebar);
            assert_eq!(app.open_index(), None);
            assert_eq!(app.cursor_room(), Some(0));
        }
        // It is about the open chat: with none there is nothing to close.
        assert!(Action::CloseChat.needs_open_chat());
        by_action.perform(Action::CloseChat);
        assert_eq!(toast(&by_action), Some("No chat is open"));
        // A draft waits with the room, as after Esc.
        let mut app = self::app();
        app.composer.insert("half a thought");
        app.perform(Action::CloseChat);
        assert!(app.composer.is_empty());
        assert_eq!(app.rooms[0].draft, "half a thought");
        // The `esc_closes_chat` setting is about the key, not about this action.
        let mut app = self::app();
        app.esc_closes_chat = false;
        app.perform(Action::Back);
        assert!(!app.chat_closed());
        app.perform(Action::CloseChat);
        assert!(app.chat_closed());
    }

    #[test]
    fn the_setting_is_on_by_default_and_its_switch_is_written_to_the_configuration() {
        use crate::modal::Setting;
        assert!(App::new(&Config::default(), std::path::PathBuf::new(), None).rooms_bar);
        let config = Config {
            rooms_bar: false,
            ..Config::default()
        };
        assert!(!App::new(&config, std::path::PathBuf::new(), None).rooms_bar);

        let mut app = app();
        let dir = std::env::temp_dir().join(format!("matrix-rat-rooms-bar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("config.toml");
        app.config_file = Some(file.clone());
        assert_eq!(app.setting_value(Setting::RoomsBar), "on");
        app.perform(Action::Setting(Setting::RoomsBar));
        assert!(!app.rooms_bar);
        assert_eq!(app.setting_value(Setting::RoomsBar), "off");
        assert!(!Config::load(&file).unwrap().rooms_bar);
        app.perform(Action::Setting(Setting::RoomsBar));
        assert!(app.rooms_bar);
        assert!(Config::load(&file).unwrap().rooms_bar);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_new_actions_and_the_setting_are_in_the_command_palette() {
        let mut app = app();
        app.perform(Action::OpenPalette);
        let titles: Vec<String> = match &app.modal {
            Some(crate::modal::Modal::Palette(palette)) => (0..)
                .map_while(|at| palette.entry_at(at))
                .map(|entry| entry.title.clone())
                .collect(),
            _ => panic!("no palette"),
        };
        for title in [
            "Newest room with notifications",
            "Close the chat and show the room list",
            "Rooms bar",
        ] {
            assert!(titles.iter().any(|t| t == title), "{title}: {titles:?}");
        }
    }
}
