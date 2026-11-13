//! Find in room: Ctrl-F opens a small bar; typing jumps to the newest message that
//! contains the text, Enter and the arrows step through the others, and stepping past
//! the oldest loaded one loads older history and keeps looking.

use crossterm::event::{KeyCode, KeyModifiers};

use super::App;
use crate::action::Pane;
use crate::matrix::MatrixCmd;
use crate::modal::Modal;
use crate::model::{MsgKind, RoomKind};

/// Rounds of loading older history for one "find older" before giving up, so a word
/// that never occurs does not page through years of a busy room unasked.
const MAX_DIG: u8 = 15;

#[derive(Debug, Clone, Default)]
pub struct SearchBar {
    pub query: String,
    /// Id of the message the search is at, if any matches.
    pub hit: Option<String>,
    /// Position of that message among the matches (1 is the newest) and their number,
    /// as far as history is loaded.
    pub position: usize,
    pub total: usize,
    /// Rounds of older history still allowed while looking for an older match.
    pub digging: u8,
    /// Shown instead of the count: "no older matches" and the like.
    pub note: Option<&'static str>,
    /// Where the hit was in the message list when last seen, to stay near it should
    /// the message disappear.
    at: usize,
    /// Selection and scroll position when the bar was opened, to go back to when the
    /// search ends without a match.
    origin: (Option<usize>, usize),
    /// The room the search is in; the bar closes if another one is shown.
    room: String,
}

impl App {
    pub(super) fn open_search(&mut self) {
        let Some(room) = self.room().filter(|room| room.kind != RoomKind::Invite) else {
            return;
        };
        // Too small to draw the bar: an invisible bar would only swallow keys.
        let fits = self.screen.width >= 18 && self.screen.height >= 5;
        if self.modal.is_some() || self.images.viewer_open() || !fits {
            return;
        }
        let bar = SearchBar {
            origin: (self.timeline.selected, self.timeline.scroll),
            room: room.id.clone(),
            ..SearchBar::default()
        };
        self.open_modal(Modal::Search(bar));
    }

    /// Indexes of the messages that contain `needle`, oldest first.
    fn search_matches(&self, needle: &str) -> Vec<usize> {
        let needle = needle.trim().to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        self.messages()
            .iter()
            .enumerate()
            .filter(|(_, m)| !matches!(m.kind, MsgKind::System | MsgKind::ReadMarker))
            .filter(|(_, m)| {
                m.body.to_lowercase().contains(&needle) || m.sender.to_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Puts the search on the match at `index` of `matches` and shows that message.
    fn search_go(&mut self, matches: &[usize], index: usize) {
        let message = matches[index];
        let id = self.messages().get(message).map(|m| m.id.clone());
        self.timeline.selected = Some(message);
        self.timeline.reveal_selected = true;
        if let Some(Modal::Search(bar)) = &mut self.modal {
            bar.hit = id;
            bar.at = message;
            bar.total = matches.len();
            bar.position = matches.len() - index;
            bar.digging = 0;
            bar.note = None;
        }
    }

    /// The query changed: go to the newest match.
    fn search_restart(&mut self) {
        let Some(Modal::Search(bar)) = &self.modal else {
            return;
        };
        let matches = self.search_matches(&bar.query);
        if let Some(last) = matches.len().checked_sub(1) {
            self.search_go(&matches, last);
        } else if let Some(Modal::Search(bar)) = &mut self.modal {
            let empty = bar.query.trim().is_empty();
            bar.hit = None;
            bar.position = 0;
            bar.total = 0;
            bar.digging = 0;
            bar.note = (!empty).then_some("not in the loaded messages · Enter looks further back");
            // Nothing found: the view goes back to where the search started.
            let (selected, scroll) = bar.origin;
            self.timeline.selected = selected.filter(|i| *i < self.messages().len());
            self.timeline.scroll = scroll;
        }
    }

    /// Steps to the next older (`older`) or newer match. Past the oldest loaded one,
    /// older history is fetched and the search goes on when it arrives.
    pub(super) fn search_step(&mut self, older: bool) {
        let Some(Modal::Search(bar)) = &self.modal else {
            return;
        };
        if bar.query.trim().is_empty() {
            return;
        }
        let matches = self.search_matches(&bar.query);
        // Where the search stands: the hit if it is still there, else the selection.
        let at = bar
            .hit
            .as_ref()
            .and_then(|id| self.messages().iter().position(|m| m.id == *id))
            .or(self.timeline.selected);
        let next = match (at, older) {
            (Some(at), true) => matches.iter().rposition(|i| *i < at),
            (Some(at), false) => matches.iter().position(|i| *i > at),
            (None, _) => matches.len().checked_sub(1),
        };
        if let Some(next) = next {
            return self.search_go(&matches, next);
        }
        if older {
            self.search_dig(MAX_DIG);
        } else if let Some(Modal::Search(bar)) = &mut self.modal {
            bar.note = Some("no newer matches");
        }
    }

    /// Asks for older history to look in, or says why not.
    fn search_dig(&mut self, rounds: u8) {
        let can_load = self.matrix.is_some()
            && !self.timeline.reached_start
            && self
                .room()
                .is_some_and(|room| room.kind != RoomKind::Invite);
        let note = if !can_load {
            Some("no older matches")
        } else if rounds == 0 {
            Some("nothing found further back · Enter keeps looking")
        } else {
            None
        };
        if let Some(Modal::Search(bar)) = &mut self.modal {
            bar.digging = if note.is_some() { 0 } else { rounds };
            bar.note = note.or(Some("looking in older messages…"));
        }
        if note.is_some() || self.timeline.loading_older {
            return;
        }
        self.timeline.loading_older = true;
        self.timeline.count_at_request = self.messages().len();
        if let Some(room_id) = self.room_id() {
            self.send_matrix(MatrixCmd::Paginate {
                room_id,
                generation: self.generation,
            });
        }
    }

    /// The messages changed under the open bar (history arrived, a message came in or
    /// was deleted): keep the count right, drop a hit that is gone, and take up a
    /// match that has only now appeared.
    pub(super) fn search_sync(&mut self) {
        let Some(Modal::Search(bar)) = &self.modal else {
            return;
        };
        if bar.query.trim().is_empty() {
            return;
        }
        let matches = self.search_matches(&bar.query);
        let at = bar
            .hit
            .as_ref()
            .and_then(|id| self.messages().iter().position(|m| m.id == *id))
            .and_then(|message| matches.iter().position(|i| *i == message));
        let had_hit = bar.hit.is_some();
        let was_at = bar.at;
        match at {
            // Still there: only the numbers may have changed.
            Some(index) => {
                let message = matches[index];
                if let Some(Modal::Search(bar)) = &mut self.modal {
                    bar.at = message;
                    bar.total = matches.len();
                    bar.position = matches.len() - index;
                }
            }
            // The hit is gone, or there was none and now there are matches.
            None if !matches.is_empty() && (had_hit || !self.timeline.loading_older) => {
                // A hit that disappeared: the next older match from where it was, the
                // way the search was heading. No hit before: the newest.
                let index = if had_hit {
                    matches
                        .iter()
                        .rposition(|i| *i < was_at)
                        .or_else(|| matches.iter().position(|i| *i >= was_at))
                        .unwrap_or(0)
                } else {
                    matches.len() - 1
                };
                self.search_go(&matches, index);
            }
            None => {
                if let Some(Modal::Search(bar)) = &mut self.modal {
                    bar.hit = None;
                    bar.total = matches.len();
                    bar.position = 0;
                }
            }
        }
    }

    /// The open room changed (it was left, or another was opened): the search was
    /// about the old one.
    pub(super) fn search_check_room(&mut self) {
        if let Some(Modal::Search(bar)) = &self.modal
            && self.room().is_none_or(|room| room.id != bar.room)
        {
            self.modal = None;
        }
    }

    /// Older history arrived (or there is none): carry on with a search that was
    /// waiting for it.
    pub(super) fn search_after_load(&mut self) {
        let Some(Modal::Search(bar)) = &self.modal else {
            return;
        };
        if bar.digging == 0 {
            return;
        }
        let rounds = bar.digging - 1;
        let matches = self.search_matches(&bar.query);
        let at = bar
            .hit
            .as_ref()
            .and_then(|id| self.messages().iter().position(|m| m.id == *id));
        let older = match at {
            Some(at) => matches.iter().rposition(|i| *i < at),
            None => matches.len().checked_sub(1),
        };
        match older {
            Some(older) => self.search_go(&matches, older),
            None => self.search_dig(rounds),
        }
    }

    pub(super) fn on_search_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let plain = !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let Some(Modal::Search(bar)) = &mut self.modal else {
            return;
        };
        match (code, mods) {
            (KeyCode::Esc, _) => self.close_search(),
            (KeyCode::Char('f'), m) if m == KeyModifiers::CONTROL => self.close_search(),
            (KeyCode::Enter, m) if m.contains(KeyModifiers::SHIFT) => self.search_step(false),
            (KeyCode::Enter | KeyCode::Up, _) => self.search_step(true),
            (KeyCode::Down, _) => self.search_step(false),
            (KeyCode::Backspace, _) => {
                bar.query.pop();
                self.search_restart();
            }
            // Nothing that could reach the terminal as a control sequence.
            (KeyCode::Char(c), _)
                if plain && !crate::text::sanitize_line(c.encode_utf8(&mut [0; 4])).is_empty() =>
            {
                bar.query.push(c);
                self.search_restart();
            }
            _ => {}
        }
    }

    pub(super) fn search_paste(&mut self, text: &str) {
        if let Some(Modal::Search(bar)) = &mut self.modal {
            bar.query.push_str(&crate::text::sanitize_line(text));
            self.search_restart();
        }
    }

    pub(super) fn search_clear(&mut self) {
        if let Some(Modal::Search(bar)) = &mut self.modal {
            bar.query.clear();
            self.search_restart();
        }
    }

    /// Closing leaves the found message selected, ready to reply to or open. Without
    /// a match the view is as it was before the search.
    pub(super) fn close_search(&mut self) {
        let Some(Modal::Search(bar)) = self.modal.take() else {
            return;
        };
        let found = bar.hit.is_some();
        if !found {
            let (selected, scroll) = bar.origin;
            self.timeline.selected = selected.filter(|i| *i < self.messages().len());
            self.timeline.scroll = scroll;
        }
        if found {
            let selected = self.timeline.selected;
            self.set_focus(Pane::Timeline);
            self.timeline.selected = selected;
            self.timeline.reveal_selected = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyEvent};

    use super::*;
    use crate::action::Action;
    use crate::matrix::{MatrixEvent, MatrixHandle};
    use crate::model::{ItemRef, Message, Room};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn message(id: &str, body: &str) -> Message {
        Message {
            id: id.to_owned(),
            item: Some(ItemRef::Event(format!("${id}"))),
            sender: "ann".to_owned(),
            body: body.to_owned(),
            ..Message::default()
        }
    }

    fn app_with(bodies: &[&str]) -> (App, tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>) {
        let mut app = App::demo();
        let (handle, commands) = MatrixHandle::for_test();
        app.matrix = Some(handle);
        let mut room = Room::new("!a", "Alpha", RoomKind::Room);
        room.messages = bodies
            .iter()
            .enumerate()
            .map(|(i, body)| message(&i.to_string(), body))
            .collect();
        app.rooms = vec![room];
        app.active = 0;
        app.screen = ratatui::layout::Rect::new(0, 0, 100, 30);
        (app, commands)
    }

    fn bar(app: &App) -> &SearchBar {
        match &app.modal {
            Some(Modal::Search(bar)) => bar,
            other => panic!("no search bar: {other:?}"),
        }
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn typing_jumps_to_the_newest_match_and_enter_walks_back() {
        let (mut app, _commands) = app_with(&["a Rat", "b", "two rats", "c", "rat three"]);
        app.timeline.reached_start = true;
        app.perform(Action::Search);
        type_text(&mut app, "rat");
        assert_eq!(app.timeline.selected, Some(4));
        assert_eq!((bar(&app).position, bar(&app).total), (1, 3));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.timeline.selected, Some(2));
        app.on_event(key(KeyCode::Up));
        assert_eq!((app.timeline.selected, bar(&app).position), (Some(0), 3));
        // Nothing older, and no history left to load.
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.timeline.selected, Some(0));
        assert_eq!(bar(&app).note, Some("no older matches"));
        // Back towards the newest; narrowing the text starts from the newest again.
        app.on_event(key(KeyCode::Down));
        assert_eq!((app.timeline.selected, bar(&app).note), (Some(2), None));
        type_text(&mut app, "s");
        assert_eq!((app.timeline.selected, bar(&app).total), (Some(2), 1));
        // Esc leaves the found message selected in the messages pane.
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert_eq!(
            (app.focus, app.timeline.selected),
            (Pane::Timeline, Some(2))
        );
    }

    #[test]
    fn looking_further_back_loads_history_until_found_or_out_of_patience() {
        let (mut app, mut commands) = app_with(&["x", "y"]);
        let paginations = |commands: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>| {
            let mut n = 0;
            while let Ok(command) = commands.try_recv() {
                n += usize::from(matches!(command, MatrixCmd::Paginate { .. }));
            }
            n
        };
        app.perform(Action::Search);
        type_text(&mut app, "needle");
        assert!(bar(&app).hit.is_none());
        assert_eq!(paginations(&mut commands), 0);
        // Enter asks for older messages; each arrival without a match asks again.
        app.on_event(key(KeyCode::Enter));
        assert_eq!(paginations(&mut commands), 1);
        let older = |app: &mut App, bodies: &[&str]| {
            let mut messages: Vec<Message> = bodies
                .iter()
                .enumerate()
                .map(|(i, body)| message(&format!("old{}-{i}", bodies.len()), body))
                .collect();
            messages.extend(app.rooms[0].messages.iter().cloned());
            app.on_matrix(MatrixEvent::Timeline {
                room_id: "!a".into(),
                generation: app.generation,
                messages,
                reached_start: false,
            });
            app.on_matrix(MatrixEvent::Paginated {
                generation: app.generation,
                reached_start: false,
            });
        };
        older(&mut app, &["nothing here"]);
        assert_eq!(paginations(&mut commands), 1);
        assert!(bar(&app).hit.is_none());
        older(&mut app, &["the needle", "hay"]);
        assert_eq!(paginations(&mut commands), 0);
        assert_eq!(app.timeline.selected, Some(0));
        assert_eq!(app.rooms[0].messages[0].body, "the needle");
        assert_eq!(bar(&app).digging, 0);

        // A word that never occurs stops after a bounded number of rounds.
        app.on_event(key(KeyCode::Enter));
        let mut rounds = paginations(&mut commands);
        for n in 0..40 {
            older(&mut app, &[&format!("hay {n}")]);
            rounds += paginations(&mut commands);
        }
        assert_eq!(rounds, usize::from(MAX_DIG));
        assert!(
            bar(&app)
                .note
                .is_some_and(|note| note.starts_with("nothing found"))
        );
    }

    #[test]
    fn the_bar_follows_what_happens_to_the_messages() {
        let (mut app, mut commands) = app_with(&["x", "rat one", "y", "rat two"]);
        let update = |app: &mut App, messages: Vec<Message>| {
            app.on_matrix(MatrixEvent::Timeline {
                room_id: "!a".into(),
                generation: app.generation,
                messages,
                reached_start: false,
            });
        };
        app.perform(Action::Search);
        type_text(&mut app, "rat");
        assert_eq!((app.timeline.selected, bar(&app).total), (Some(3), 2));
        // A new match comes in: the count follows, the hit stays.
        let mut messages = app.rooms[0].messages.clone();
        messages.push(message("new", "another rat"));
        update(&mut app, messages.clone());
        assert_eq!((bar(&app).position, bar(&app).total), (2, 3));
        assert_eq!(app.timeline.selected, Some(3));
        // The hit is deleted: the search moves to the nearest older match instead of
        // jumping to the newest.
        messages.remove(3);
        update(&mut app, messages.clone());
        assert_eq!(bar(&app).hit.as_deref(), Some("1"));
        assert_eq!(bar(&app).total, 2);

        // History that answers a "look further back" may arrive after the worker said
        // it was done; the match is still taken up.
        type_text(&mut app, "zzz");
        assert!(bar(&app).hit.is_none());
        app.on_event(key(KeyCode::Enter));
        while commands.try_recv().is_ok() {}
        for _ in 0..MAX_DIG {
            app.on_matrix(MatrixEvent::Paginated {
                generation: app.generation,
                reached_start: false,
            });
        }
        assert_eq!(bar(&app).digging, 0);
        let mut older = vec![message("old", "ratzzz at last")];
        older.extend(messages.clone());
        update(&mut app, older);
        assert_eq!(bar(&app).hit.as_deref(), Some("old"));

        // A failed request for history does not leave the search waiting forever.
        type_text(&mut app, "q");
        app.on_event(key(KeyCode::Enter));
        assert!(app.timeline.loading_older);
        app.on_matrix(MatrixEvent::Paginated {
            generation: app.generation,
            reached_start: true,
        });
        assert!(!app.timeline.loading_older);
        assert_eq!(bar(&app).note, Some("no older matches"));

        // Without a match, closing puts the view back where the search began; and a
        // click outside closes like Esc.
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert_eq!(app.timeline.selected, None);
        // The room goes away under an open bar: the bar goes too.
        app.perform(Action::Search);
        type_text(&mut app, "rat");
        app.rooms[0].id = "!other".into();
        app.search_check_room();
        assert!(app.modal.is_none());
    }

    #[test]
    fn the_wheel_steps_through_the_matches() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        let (mut app, _commands) = app_with(&["rat one", "x", "rat two", "y", "rat three"]);
        app.timeline.reached_start = true;
        app.perform(Action::Search);
        type_text(&mut app, "rat");
        let wheel = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 10,
                row: 10,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert_eq!(app.timeline.selected, Some(4));
        // Up is towards older messages, as when scrolling them.
        app.on_event(wheel(MouseEventKind::ScrollUp));
        assert_eq!(app.timeline.selected, Some(2));
        app.on_event(wheel(MouseEventKind::ScrollUp));
        assert_eq!(app.timeline.selected, Some(0));
        app.on_event(wheel(MouseEventKind::ScrollDown));
        assert_eq!(app.timeline.selected, Some(2));
        assert!(matches!(app.modal, Some(Modal::Search(_))));
    }

    #[test]
    fn search_needs_a_room_and_matches_names_too() {
        let (mut app, _commands) = app_with(&["hello"]);
        app.perform(Action::Search);
        type_text(&mut app, "ANN");
        assert_eq!(bar(&app).total, 1);
        app.on_event(Event::Paste("\u{1b}[31m x\nsecond".into()));
        assert_eq!(bar(&app).query, "ANN[31m x");
        // Typed control characters never get into the text.
        app.on_event(key(KeyCode::Char('\u{202e}')));
        app.on_event(key(KeyCode::Char('\u{9b}')));
        assert_eq!(bar(&app).query, "ANN[31m x");
        app.on_event(key(KeyCode::Esc));
        // Not on an invitation, not without a room.
        app.rooms[0].kind = RoomKind::Invite;
        app.perform(Action::Search);
        assert!(app.modal.is_none());
        app.rooms.clear();
        app.perform(Action::Search);
        assert!(app.modal.is_none());
    }
}
