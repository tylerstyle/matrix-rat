//! The rooms bar's side of the app (the `rooms_bar` setting; drawn by
//! `ui::rooms_bar`): which rooms it shows, and the key that goes to the first of them.
//!
//! A room is in the bar when it has news that notifies (`Room::notifies`, the rule the
//! window title counts by) and is not the open one. The order is that of `rooms`,
//! which the worker hands over sorted by the time of the latest message, the newest
//! first; the room list keeps the same order inside each of its sections.

use super::{App, ComposeMode};
use crate::action::Pane;

impl App {
    /// The rooms with new notifications, the newest first, without the open one.
    pub(crate) fn active_rooms(&self) -> Vec<usize> {
        let open = self.open_index();
        self.rooms
            .iter()
            .enumerate()
            .filter(|(index, room)| Some(*index) != open && room.notifies())
            .map(|(index, _)| index)
            .collect()
    }

    /// Opens the first room of the bar; pressed again, the one after the room it
    /// opened, in the order of `rooms`, and round to the first.
    ///
    /// Opening a room does not take it out of the bar for good: until the receipt
    /// is through, the next room list brings its count back. So the key remembers
    /// the room it opened (`active_from`) and goes on from there, or two rooms would
    /// take turns for ever. Any other change of room forgets it.
    pub(super) fn open_active_room(&mut self) {
        let active = self.active_rooms();
        let from = self
            .active_from
            .as_ref()
            .and_then(|id| self.rooms.iter().position(|room| room.id == *id))
            .filter(|at| Some(*at) == self.open_index());
        let next = from
            .and_then(|at| active.iter().copied().find(|index| *index > at))
            .or_else(|| active.first().copied());
        match next {
            Some(index) => {
                // `show_room`: the room list follows, out of a space the room is not in.
                self.show_room(index);
                self.active_from = self.rooms.get(index).map(|room| room.id.clone());
            }
            None => self.show_toast("No room has new notifications"),
        }
    }

    /// The "Rooms" label and the `(n)>` of the rooms bar, and `close_chat` by name:
    /// closes the chat, so that the room list is shown in its place.
    ///
    /// Unlike `Esc`, which gets here one step at a time, this is one click: an edit
    /// in progress would be given up by it, so it is refused as other things are
    /// during an edit. (A reply only loses what it answers; its text waits as the
    /// room's draft, as after any change of room.)
    pub(super) fn leave_for_room_list(&mut self) {
        if self.open_index().is_none() {
            // No rooms yet: there is no chat to close, but the list can be gone to.
            self.set_focus(Pane::Sidebar);
        } else if matches!(self.compose_mode, ComposeMode::Edit(_)) {
            self.show_toast("Finish or cancel the edit first");
        } else {
            self.close_chat();
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::action::Action;
    use crate::config::Config;
    use crate::input::{InputEvent, KeyCode, KeyEvent, KeyModifiers};
    use crate::matrix::{MatrixEvent, RoomCategory, RoomSummary};
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
    fn the_bar_and_the_window_title_agree_on_what_counts() {
        let count = |app: &App| super::super::title::unread_count(&app.rooms);
        let mut app = app();
        app.rooms = vec![
            room("!open", RoomKind::Room, 0),
            room("!plain", RoomKind::Room, 3),
            room("!muted", RoomKind::Room, 5),
            room("!invite", RoomKind::Invite, 4),
            room("!knock", RoomKind::Knock, 2),
            room("!by-hand", RoomKind::Room, 0),
            room("!muted-mention", RoomKind::Person, 6),
            room("!mention", RoomKind::Person, 2),
        ];
        app.rooms[2].muted = true;
        app.rooms[5].marked_unread = true;
        app.rooms[5].bold = true;
        app.rooms[6].muted = true;
        app.rooms[6].mention = true;
        app.rooms[7].mention = true;
        // Two rooms, five messages: the plain one and the mention.
        assert_eq!(names(&app), ["!plain", "!mention"]);
        assert_eq!(count(&app), 5);
        // Unmuted, the two others count with theirs.
        app.rooms[2].muted = false;
        app.rooms[6].muted = false;
        assert_eq!(
            names(&app),
            ["!plain", "!muted", "!muted-mention", "!mention"]
        );
        assert_eq!(count(&app), 16);
        // None at all.
        app.rooms.clear();
        assert_eq!(names(&app), Vec::<&str>::new());
        assert_eq!(count(&app), 0);
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
        app.on_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::ALT,
        )));
        assert_eq!(open_id(&app), Some("!new"));
        assert_eq!(app.focus, Pane::Composer);
        // Opened, it has no count left for now and is out of the bar.
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

    /// The room list as the server tells it: every count as it was, also that of
    /// rooms opened here, whose receipt is not through yet.
    fn listed(counts: &[(&str, u64)]) -> MatrixEvent {
        MatrixEvent::Rooms(
            counts
                .iter()
                .map(|(id, count)| RoomSummary {
                    id: (*id).into(),
                    name: (*id).into(),
                    category: RoomCategory::Room,
                    notifications: *count,
                    preview: None,
                    server_notifications: *count,
                    mention: false,
                    unread: *count > 0,
                    marked_unread: false,
                    avatar: None,
                    alias: None,
                    direct: false,
                    muted: false,
                    encrypted: false,
                })
                .collect(),
        )
    }

    #[test]
    fn the_key_goes_on_to_the_next_room_although_the_server_still_counts_the_last() {
        let counts = [("!open", 0), ("!a", 2), ("!b", 3), ("!quiet", 0), ("!c", 1)];
        let mut app = app();
        app.on_matrix(listed(&counts));
        assert_eq!(names(&app), ["!a", "!b", "!c"]);
        // Each press, then a room list that brings every count back.
        let mut opened = Vec::new();
        for _ in 0..5 {
            app.perform(Action::RoomActive);
            opened.push(open_id(&app).unwrap_or_default().to_owned());
            app.on_matrix(listed(&counts));
        }
        // On through all of them, and round: not a, b, a, b.
        assert_eq!(opened, ["!a", "!b", "!c", "!a", "!b"]);
        // A room opened in any other way: the key starts from the first again.
        let quiet = app.rooms.iter().position(|r| r.id == "!quiet").unwrap();
        app.perform(Action::OpenRoom(quiet));
        app.on_matrix(listed(&counts));
        app.perform(Action::RoomActive);
        assert_eq!(open_id(&app), Some("!a"));
        // Also after the chat was closed and the same room opened by hand.
        app.perform(Action::RoomActive);
        assert_eq!(open_id(&app), Some("!b"));
        app.perform(Action::CloseChat);
        let b = app.rooms.iter().position(|r| r.id == "!b").unwrap();
        app.perform(Action::OpenRoom(b));
        app.on_matrix(listed(&counts));
        app.perform(Action::RoomActive);
        assert_eq!(open_id(&app), Some("!a"));
        // A list in another order: on from where the room is now.
        app.on_matrix(listed(&[("!c", 1), ("!a", 2), ("!open", 0), ("!b", 3)]));
        app.perform(Action::RoomActive);
        assert_eq!(open_id(&app), Some("!b"));
        app.on_matrix(listed(&[("!c", 1), ("!a", 2), ("!open", 0), ("!b", 3)]));
        app.perform(Action::RoomActive);
        assert_eq!(open_id(&app), Some("!c"));
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
        // An edit in progress is not given up by one click: it is refused, as
        // other things are during an edit. Esc still gets there step by step.
        let mut app = App::demo();
        app.perform(Action::EditLast);
        assert!(matches!(app.compose_mode, ComposeMode::Edit(_)));
        let edited = app.composer.text().to_owned();
        app.perform(Action::CloseChat);
        assert!(!app.chat_closed());
        assert!(matches!(app.compose_mode, ComposeMode::Edit(_)));
        assert_eq!(app.composer.text(), edited);
        assert_eq!(toast(&app), Some("Finish or cancel the edit first"));
        app.perform(Action::Back);
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        app.perform(Action::CloseChat);
        assert!(app.chat_closed());
        // A reply is no obstacle: its text waits as the room's draft.
        let mut app = App::demo();
        app.perform(Action::SelectOlder);
        app.perform(Action::Reply);
        assert!(matches!(app.compose_mode, ComposeMode::Reply(_)));
        app.composer.insert("an answer");
        app.perform(Action::CloseChat);
        assert!(app.chat_closed());
        assert_eq!(app.rooms[0].draft, "an answer");
        // With no rooms at all there is no chat to close: the list is gone to.
        let mut app = App::demo();
        app.rooms.clear();
        assert_eq!(app.focus, Pane::Composer);
        app.perform(Action::CloseChat);
        assert!(!app.chat_closed());
        assert_eq!(app.focus, Pane::Sidebar);
        assert_eq!(toast(&app), None);
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
        app.set_config_file(&file);
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
