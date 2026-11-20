//! The unread count in the terminal's window title (the `window_title` setting):
//! `matrix-rat` while nothing waits, `matrix-rat (3)` while three messages do. No room
//! is ever named. The draw loop writes it, only when it changed; `terminal` saves the
//! title that was there and puts it back.

use super::App;
use crate::model::Room;

/// The count in the title: the numbers on the room list's badges added up, that is
/// the unread messages that notify, as the server counts them by the account's rules.
/// Messages, not rooms, so that it reads like the badges.
///
/// - Muted rooms do not count (their badge is not news either).
/// - A mention counts as the message it is; a room marked unread by hand has no
///   number on its badge and adds nothing.
/// - Invitations and join requests do not count: they are not messages, and the room
///   list shows them apart.
/// - Do not disturb changes nothing: the messages wait all the same.
pub fn unread_count(rooms: &[Room]) -> u64 {
    rooms
        .iter()
        .filter(|room| room.notifies())
        .fold(0u64, |sum, room| sum.saturating_add(u64::from(room.unread)))
}

/// The title for `count` unread messages.
pub fn title_text(count: u64) -> String {
    match count {
        0 => "matrix-rat".to_owned(),
        n => format!("matrix-rat ({n})"),
    }
}

/// What to write as the title now, if anything: never with the setting off, and only
/// when it differs from what was written last (`written`).
pub fn next_title(enabled: bool, written: Option<&str>, count: u64) -> Option<String> {
    if !enabled {
        return None;
    }
    let title = title_text(count);
    (written != Some(title.as_str())).then_some(title)
}

impl App {
    /// The title to write after this frame, if it changed; it counts as written.
    pub fn take_window_title(&mut self) -> Option<String> {
        let count = unread_count(&self.rooms);
        let title = next_title(self.window_title, self.title_written.as_deref(), count)?;
        self.title_written = Some(title.clone());
        Some(title)
    }

    /// The terminal was handed back and taken again (Ctrl-Z, the editor), and has the
    /// title from before again: ours is written anew.
    pub fn window_title_lost(&mut self) {
        self.title_written = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use crate::config::Config;
    use crate::model::RoomKind;

    fn room(id: &str, kind: RoomKind, unread: u32) -> Room {
        let mut room = Room::new(id, id, kind);
        room.unread = unread;
        room
    }

    #[test]
    fn the_count_adds_up_the_badges_of_rooms_that_are_not_muted() {
        assert_eq!(unread_count(&[]), 0);
        let mut rooms = vec![
            room("!a", RoomKind::Room, 3),
            room("!b", RoomKind::Person, 2),
            room("!c", RoomKind::Favourite, 0),
        ];
        assert_eq!(unread_count(&rooms), 5);
        // A mention is one of the messages counted already.
        rooms[1].mention = true;
        assert_eq!(unread_count(&rooms), 5);
        // Marked unread by hand, or bold without a number: no number, nothing added.
        rooms[2].marked_unread = true;
        rooms[2].bold = true;
        assert_eq!(unread_count(&rooms), 5);
        // Muted: not counted, whatever its number.
        rooms[0].muted = true;
        assert_eq!(unread_count(&rooms), 2);
        // Invitations and join requests are not messages.
        rooms.push(room("!i", RoomKind::Invite, 1));
        rooms.push(room("!k", RoomKind::Knock, 1));
        assert_eq!(unread_count(&rooms), 2);
        // Never overflows.
        let many: Vec<Room> = (0..3)
            .map(|n| room(&format!("!{n}"), RoomKind::Room, u32::MAX))
            .collect();
        assert_eq!(unread_count(&many), 3 * u64::from(u32::MAX));
    }

    #[test]
    fn the_title_names_no_room_and_is_written_only_when_it_changes() {
        assert_eq!(title_text(0), "matrix-rat");
        assert_eq!(title_text(12), "matrix-rat (12)");
        assert_eq!(next_title(true, None, 0), Some("matrix-rat".to_owned()));
        assert_eq!(next_title(true, Some("matrix-rat"), 0), None);
        assert_eq!(
            next_title(true, Some("matrix-rat"), 2),
            Some("matrix-rat (2)".to_owned())
        );
        assert_eq!(next_title(true, Some("matrix-rat (2)"), 2), None);
        // Off: nothing, ever.
        assert_eq!(next_title(false, None, 2), None);
        assert_eq!(next_title(false, Some("matrix-rat"), 2), None);
    }

    #[test]
    fn the_app_writes_the_title_once_per_change() {
        let mut app = App::demo();
        app.rooms = vec![room("!a", RoomKind::Room, 0), room("!b", RoomKind::Room, 0)];
        assert_eq!(app.take_window_title().as_deref(), Some("matrix-rat"));
        assert_eq!(app.take_window_title(), None);
        app.rooms[1].unread = 4;
        assert_eq!(app.take_window_title().as_deref(), Some("matrix-rat (4)"));
        assert_eq!(app.take_window_title(), None);
        // Do not disturb leaves the count as it is.
        app.do_not_disturb = true;
        assert_eq!(app.take_window_title(), None);
        // After Ctrl-Z or the editor the terminal shows the old title: written again.
        app.window_title_lost();
        assert_eq!(app.take_window_title().as_deref(), Some("matrix-rat (4)"));
        // Logged out: no rooms, nothing waits.
        app.rooms.clear();
        assert_eq!(app.take_window_title().as_deref(), Some("matrix-rat"));
    }

    #[test]
    fn with_the_setting_off_the_title_is_left_alone() {
        use crate::modal::Setting;
        let config = Config {
            window_title: false,
            ..Config::default()
        };
        let mut app = App::new(&config, std::path::PathBuf::new(), None);
        assert!(!app.window_title);
        app.rooms = vec![room("!a", RoomKind::Room, 3)];
        assert_eq!(app.take_window_title(), None);
        assert!(App::new(&Config::default(), std::path::PathBuf::new(), None).window_title);

        // The settings row (and the palette's entry) switches it and writes it down.
        let dir = std::env::temp_dir().join(format!("matrix-rat-title-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("config.toml");
        app.config_file = Some(file.clone());
        app.perform(Action::Setting(Setting::WindowTitle));
        assert!(app.window_title);
        assert!(Config::load(&file).unwrap().window_title);
        assert_eq!(app.setting_value(Setting::WindowTitle), "on");
        assert_eq!(app.take_window_title().as_deref(), Some("matrix-rat (3)"));
        assert_eq!(app.take_window_title(), None);
        app.perform(Action::Setting(Setting::WindowTitle));
        assert!(!app.window_title);
        assert!(!Config::load(&file).unwrap().window_title);
        assert_eq!(app.take_window_title(), None);
        // On again: written at once, even though the count is the same.
        app.perform(Action::Setting(Setting::WindowTitle));
        assert_eq!(app.take_window_title().as_deref(), Some("matrix-rat (3)"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
