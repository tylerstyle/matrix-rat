//! Composer completion state, keyboard/mouse integration, member loading and draft
//! mention preservation on the `App`.

use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyModifiers};
use matrix_sdk::ruma::events::Mentions;

use super::App;
use crate::action::Pane;
use crate::complete::{self, Completion, Context, Member, MentionMark, RoomTarget};
use crate::format::{self, Outgoing};
use crate::matrix::MatrixCmd;
use crate::model::{MsgKind, OWN_NAME, RoomKind};

#[derive(Debug, Default)]
struct RoomMembers {
    members: Vec<Member>,
    can_mention_room: bool,
}

#[derive(Debug, Default)]
pub struct CompleteState {
    pub active: Option<Completion>,
    pub(super) marks: Vec<MentionMark>,
    pub(super) edit_had_mentions: bool,
    draft_marks: HashMap<String, Vec<MentionMark>>,
    thread_marks: HashMap<(String, String), Vec<MentionMark>>,
    edit_stash_marks: Option<Vec<MentionMark>>,
    room_members: HashMap<String, RoomMembers>,
    /// Tracks which room already triggered a `FetchMembers` request for the current open.
    opened_for_room: Option<String>,
    /// Trigger byte start dismissed with `Esc`; cleared when editing text or leaving trigger.
    dismissed_start: Option<usize>,
}

impl CompleteState {
    pub(super) fn reset_session(&mut self) {
        self.active = None;
        self.marks.clear();
        self.edit_had_mentions = false;
        self.draft_marks.clear();
        self.thread_marks.clear();
        self.edit_stash_marks = None;
        self.room_members.clear();
        self.opened_for_room = None;
        self.dismissed_start = None;
    }

    pub(super) fn close_popup(&mut self) {
        self.active = None;
        self.dismissed_start = None;
    }
}

impl App {
    /// Requests room members from the Matrix worker at most once per room open (never on keystrokes).
    pub(super) fn request_members_for_open_room(&mut self) {
        let Some(room) = self.room() else {
            return;
        };
        if room.kind == RoomKind::Invite {
            self.complete.opened_for_room = None;
            return;
        }
        let room_id = room.id.clone();
        if self.complete.opened_for_room.as_deref() == Some(room_id.as_str()) {
            return;
        }
        self.complete.opened_for_room = Some(room_id.clone());
        self.send_matrix(MatrixCmd::FetchMembers { room_id });
    }

    /// Stores the member list received from the worker and refreshes an active `@` popup.
    pub(super) fn on_room_members(
        &mut self,
        room_id: String,
        members: Vec<Member>,
        can_mention_room: bool,
    ) {
        self.complete.room_members.insert(
            room_id.clone(),
            RoomMembers {
                members,
                can_mention_room,
            },
        );
        if self.room_id().as_deref() == Some(room_id.as_str()) {
            self.sync_completion();
        }
    }

    /// Drops the marks kept for the drafts of a room and its threads: the room is gone.
    pub(super) fn forget_room_marks(&mut self, room_id: &str) {
        self.complete.draft_marks.remove(room_id);
        self.complete
            .thread_marks
            .retain(|(room, _), _| room != room_id);
    }

    pub(super) fn save_room_marks(&mut self, room_id: &str) {
        let marks = std::mem::take(&mut self.complete.marks);
        if !marks.is_empty() {
            self.complete.draft_marks.insert(room_id.to_owned(), marks);
        } else {
            self.complete.draft_marks.remove(room_id);
        }
    }

    pub(super) fn restore_room_marks(&mut self, room_id: &str) {
        self.complete.marks = self
            .complete
            .draft_marks
            .remove(room_id)
            .unwrap_or_default();
        self.complete.close_popup();
    }

    pub(super) fn save_thread_marks(&mut self, room_id: &str, key: &str) {
        let marks = std::mem::take(&mut self.complete.marks);
        if !marks.is_empty() {
            self.complete
                .thread_marks
                .insert((room_id.to_owned(), key.to_owned()), marks);
        } else {
            self.complete
                .thread_marks
                .remove(&(room_id.to_owned(), key.to_owned()));
        }
    }

    pub(super) fn restore_thread_marks(&mut self, room_id: &str, key: &str) {
        self.complete.marks = self
            .complete
            .thread_marks
            .remove(&(room_id.to_owned(), key.to_owned()))
            .unwrap_or_default();
        self.complete.close_popup();
    }

    pub(super) fn stash_marks_for_edit(
        &mut self,
        marks: Vec<MentionMark>,
        had_mentions: bool,
        already_editing: bool,
    ) {
        if !already_editing {
            self.complete.edit_stash_marks = Some(std::mem::take(&mut self.complete.marks));
        }
        self.complete.marks = marks;
        self.complete.edit_had_mentions = had_mentions;
        self.complete.close_popup();
    }

    pub(super) fn restore_marks_after_edit(&mut self) {
        self.complete.marks = self.complete.edit_stash_marks.take().unwrap_or_default();
        self.complete.edit_had_mentions = false;
        self.complete.close_popup();
    }

    /// Parses composer text together with accepted mention/room links and edit state.
    pub(super) fn format_outgoing(&self, text: &str) -> Outgoing {
        format::parse_with_mentions(text, &self.complete.marks, self.complete.edit_had_mentions)
    }

    pub(super) fn clear_completion_marks(&mut self) {
        self.complete.marks.clear();
        self.complete.edit_had_mentions = false;
        self.complete.close_popup();
    }

    /// Recomputes composer completion state from the current text and cursor.
    pub(super) fn sync_completion(&mut self) {
        if self.focus != Pane::Composer
            || self.help_open
            || self.modal.is_some()
            || self.images.viewer_open()
        {
            self.complete.active = None;
            return;
        }

        let recent = self.recent_speakers();
        let room_targets = self.room_targets();
        let demo_fallback;
        let (members, can_mention_room): (&[Member], bool) = match self
            .room_id()
            .and_then(|id| self.complete.room_members.get(&id))
        {
            Some(loaded) => (&loaded.members, loaded.can_mention_room),
            None if self.matrix.is_none() => {
                demo_fallback = self.demo_members(&recent);
                (&demo_fallback, true)
            }
            None => (&[], false),
        };

        let ctx = Context {
            members,
            recent_speakers: &recent,
            can_mention_room,
            rooms: &room_targets,
        };
        let prev_selected = self
            .complete
            .active
            .as_ref()
            .and_then(|c| c.current().map(|item| item.target.clone()));

        let mut next = complete::find(self.composer.text(), self.composer.cursor(), &ctx);
        match &mut next {
            None => {
                self.complete.dismissed_start = None;
                self.complete.active = None;
            }
            Some(completion) => {
                if self.complete.dismissed_start == Some(completion.range.start) {
                    self.complete.active = None;
                    return;
                }
                self.complete.dismissed_start = None;
                if let Some(prev) = prev_selected
                    && let Some(pos) = completion.items.iter().position(|item| item.target == prev)
                {
                    completion.selected = pos;
                }
                self.complete.active = next;
            }
        }
    }

    pub(super) fn allow_text_edit_reopen_completion(&mut self) {
        self.complete.dismissed_start = None;
        self.sync_completion();
    }

    /// Handles keys inside `Pane::Composer` when a completion popup is open.
    /// Returns `false` immediately when no popup is active so normal bindings stay untouched.
    pub(super) fn complete_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        if self.focus != Pane::Composer || self.complete.active.is_none() {
            return false;
        }
        match (code, mods) {
            (KeyCode::Up, KeyModifiers::NONE) => {
                if let Some(completion) = &mut self.complete.active {
                    completion.move_by(-1);
                }
                true
            }
            (KeyCode::Down, KeyModifiers::NONE) => {
                if let Some(completion) = &mut self.complete.active {
                    completion.move_by(1);
                }
                true
            }
            (KeyCode::PageUp, KeyModifiers::NONE) => {
                if let Some(completion) = &mut self.complete.active {
                    completion.move_by(-5);
                }
                true
            }
            (KeyCode::PageDown, KeyModifiers::NONE) => {
                if let Some(completion) = &mut self.complete.active {
                    completion.move_by(5);
                }
                true
            }
            (KeyCode::Tab | KeyCode::Enter, KeyModifiers::NONE) => self.accept_completion(None),
            (KeyCode::Esc, KeyModifiers::NONE) => {
                if let Some(completion) = self.complete.active.take() {
                    self.complete.dismissed_start = Some(completion.range.start);
                }
                true
            }
            _ => false,
        }
    }

    pub(super) fn adjust_marks_for_cursor_edit(&mut self, before_cursor: usize, before_len: usize) {
        let after_len = self.composer.text().len();
        let after_cursor = self.composer.cursor();
        let suffix_len = after_len.saturating_sub(after_cursor);
        let edit_start = before_cursor.min(after_cursor);
        let old_end = before_len.saturating_sub(suffix_len).max(edit_start);
        self.adjust_marks_for_splice(edit_start, old_end, after_cursor);
    }

    pub(super) fn adjust_marks_for_splice(
        &mut self,
        edit_start: usize,
        old_end: usize,
        new_end: usize,
    ) {
        for mark in &mut self.complete.marks {
            mark.adjust_for_edit(edit_start, old_end, new_end);
        }
    }

    pub(super) fn insert_composer_text(&mut self, text: &str) {
        let before_len = self.composer.text().len();
        let before_cursor = self.composer.cursor();
        self.composer.insert(text);
        self.adjust_marks_for_cursor_edit(before_cursor, before_len);
    }

    pub(super) fn accept_completion(&mut self, index: Option<usize>) -> bool {
        let Some(completion) = self.complete.active.take() else {
            return false;
        };
        let idx = index.unwrap_or(completion.selected);
        let Some(candidate) = completion.items.get(idx) else {
            return false;
        };
        let (ins, mark) =
            complete::replacement(self.composer.text(), &completion.range, &candidate.target);
        let range = completion.range;
        self.composer.replace_range(range.clone(), &ins);
        self.adjust_marks_for_splice(range.start, range.end, self.composer.cursor());
        if let Some(mark) = mark {
            self.complete.marks.push(mark);
        }
        self.complete.dismissed_start = None;
        self.sync_completion();
        self.update_typing();
        true
    }

    fn recent_speakers(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for msg in self.messages().iter().rev() {
            if msg.own || matches!(msg.kind, MsgKind::System | MsgKind::ReadMarker) {
                continue;
            }
            if !msg.sender.is_empty() && seen.insert(msg.sender.clone()) {
                out.push(msg.sender.clone());
                if out.len() >= 50 {
                    break;
                }
            }
        }
        out
    }

    fn demo_members(&self, recent: &[String]) -> Vec<Member> {
        recent
            .iter()
            .filter(|name| *name != OWN_NAME)
            .map(|name| {
                let localpart: String = name
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                            c.to_ascii_lowercase()
                        } else {
                            '_'
                        }
                    })
                    .collect();
                Member {
                    user_id: format!("@{localpart}:matrix.org"),
                    display_name: name.clone(),
                }
            })
            .collect()
    }

    fn room_targets(&self) -> Vec<RoomTarget> {
        self.rooms
            .iter()
            .filter(|r| {
                r.kind != RoomKind::Invite && (r.kind != RoomKind::Person || r.alias.is_some())
            })
            .map(|r| RoomTarget {
                id: r.id.clone(),
                name: r.name.clone(),
                alias: r.alias.clone(),
            })
            .collect()
    }
}

/// Checks if `msg` originally carried `m.mentions` cleanly for tests and helpers.
#[allow(dead_code)]
pub(super) fn mentions_empty(mentions: &Option<Mentions>) -> bool {
    mentions
        .as_ref()
        .is_none_or(|m| m.user_ids.is_empty() && !m.room)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use crate::app::{Config, MatrixHandle, ui};
    use crate::complete::Kind;
    use crate::matrix::{MatrixEvent, RoomCategory, RoomSummary};
    use crate::model::{ItemRef, Message};
    use crate::ui::hit::HitId;
    use crossterm::event::{Event, KeyEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;
    use tokio::sync::mpsc::UnboundedReceiver;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn redraw(app: &mut App, terminal: &mut Terminal<TestBackend>) {
        terminal.draw(|f| ui::draw(f, app)).unwrap();
    }

    fn screen(app: &mut App, terminal: &mut Terminal<TestBackend>) -> String {
        redraw(app, terminal);
        format!("{}", terminal.backend())
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
    }

    fn connected() -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let (handle, rx) = MatrixHandle::for_test();
        let mut app = App::new(&Config::default(), PathBuf::new(), Some(handle));
        app.on_matrix(MatrixEvent::Rooms(vec![
            room_summary("!a", "Alpha", None),
            room_summary("!b", "Beta", None),
        ]));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        redraw(&mut app, &mut terminal);
        (app, terminal, rx)
    }

    fn remote(id: &str, sender: &str, body: &str) -> Message {
        Message {
            id: id.into(),
            item: Some(ItemRef::Event(format!("$ev{id}"))),
            sender: sender.into(),
            time: "10:00".into(),
            day: "Today".into(),
            body: body.into(),
            own: sender == "me",
            ..Message::default()
        }
    }

    fn room_summary(id: &str, name: &str, alias: Option<&str>) -> RoomSummary {
        RoomSummary {
            id: id.into(),
            name: name.into(),
            category: RoomCategory::Room,
            notifications: 0,
            server_notifications: 0,
            mention: false,
            unread: false,
            marked_unread: false,
            avatar: None,
            preview: None,
            alias: alias.map(str::to_owned),
        }
    }

    #[test]
    fn no_popup_keys_behave_as_before() {
        let (mut app, _terminal, mut commands) = connected();
        while commands.try_recv().is_ok() {}

        // Plain Enter sends a message normally.
        type_text(&mut app, "hello world");
        assert!(app.complete.active.is_none());
        app.on_event(key(KeyCode::Enter));
        let sent = std::iter::from_fn(|| commands.try_recv().ok())
            .find(|c| matches!(c, MatrixCmd::Send { .. }));
        assert!(matches!(
            sent,
            Some(MatrixCmd::Send {
                message: Outgoing::Text { plain, .. },
                ..
            }) if plain == "hello world"
        ));

        // Tab still moves focus to Sidebar when no popup is open.
        assert_eq!(app.focus, Pane::Composer);
        app.on_event(key(KeyCode::Tab));
        assert_eq!(app.focus, Pane::Sidebar);

        // Returning to Composer and pressing Esc returns to Composer/normal mode, not swallowed.
        app.perform(Action::Focus(Pane::Composer));
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.focus, Pane::Composer);
    }

    #[test]
    fn popup_lifecycle_tab_enter_esc_and_mouse_click() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::RoomMembers {
            room_id: "!a".into(),
            members: vec![
                Member {
                    user_id: "@alice:hs".into(),
                    display_name: "Alice".into(),
                },
                Member {
                    user_id: "@bob:hs".into(),
                    display_name: "Bob".into(),
                },
            ],
            can_mention_room: true,
        });
        while commands.try_recv().is_ok() {}

        // Typing `@` opens the mention popup and drawing renders it above the cursor.
        type_text(&mut app, "hi @a");
        assert_eq!(app.complete.active.as_ref().unwrap().kind, Kind::Mention);
        let rendered = screen(&mut app, &mut terminal);
        assert!(rendered.contains("Members"));
        assert!(rendered.contains("Alice"));
        assert!(rendered.contains("@alice:hs"));

        // Esc closes *only* the popup; the draft stays intact.
        app.on_event(key(KeyCode::Esc));
        assert!(app.complete.active.is_none());
        assert_eq!(app.composer.text(), "hi @a");

        // Next Esc works as before (reply/edit/thread/composer).
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.focus, Pane::Composer);

        // Typing another char opens and narrows again; Tab accepts display name and marks user.
        type_text(&mut app, "l");
        assert!(app.complete.active.is_some());
        app.on_event(key(KeyCode::Tab));
        assert!(app.complete.active.is_none());
        assert_eq!(app.composer.text(), "hi Alice ");

        // Sending produces m.mentions + matrix.to link.
        app.on_event(key(KeyCode::Enter));
        let sent = std::iter::from_fn(|| commands.try_recv().ok())
            .find_map(|c| match c {
                MatrixCmd::Send { message, .. } => Some(message),
                _ => None,
            })
            .expect("expected Send command");
        match sent {
            Outgoing::Text {
                plain,
                html,
                mentions,
            } => {
                assert_eq!(plain, "hi Alice");
                assert!(
                    html.unwrap()
                        .contains("href=\"https://matrix.to/#/@alice:hs\"")
                );
                let m = mentions.unwrap();
                assert!(m.user_ids.iter().any(|u| u.as_str() == "@alice:hs"));
            }
            other => panic!("unexpected outgoing: {other:?}"),
        }

        // Mouse click on a popup row accepts the clicked candidate.
        type_text(&mut app, "ping @");
        app.on_event(key(KeyCode::Down));
        redraw(&mut app, &mut terminal);
        let size = terminal.size().unwrap();
        let pos = (0..size.width)
            .flat_map(|x| (0..size.height).map(move |y| (x, y)))
            .find(|&(x, y)| matches!(app.hits.at(x, y), Some(HitId::Complete(1))));
        let (x, y) = pos.expect("expected clickable Complete(1) hit rect");
        app.on_event(crossterm::event::Event::Mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            },
        ));
        assert_eq!(app.composer.text(), "ping Bob ");
        assert!(app.complete.active.is_none());
    }

    #[test]
    fn emoji_room_and_slash_completions_work_and_keep_upload_path_completion() {
        let (mut app, _terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::Rooms(vec![
            room_summary("!a", "Alpha", Some("#alpha:hs")),
            room_summary("!r", "Rust", None),
        ]));
        while commands.try_recv().is_ok() {}

        // Emoji `:smi` + Enter inserts the character cleanly and closes the popup.
        type_text(&mut app, "yay :smi");
        assert_eq!(app.complete.active.as_ref().unwrap().kind, Kind::Emoji);
        app.on_event(key(KeyCode::Enter));
        assert!(app.composer.text().starts_with("yay "));
        assert!(!app.composer.text().contains(':'));
        assert!(app.complete.active.is_none());

        // `#ru` completes the room name/alias and sends a matrix.to room link without m.mentions.
        app.composer.take();
        app.clear_completion_marks();
        type_text(&mut app, "see #al");
        assert_eq!(app.complete.active.as_ref().unwrap().kind, Kind::Room);
        app.on_event(key(KeyCode::Tab));
        assert_eq!(app.composer.text(), "see #alpha:hs ");
        app.on_event(key(KeyCode::Enter));
        let sent = std::iter::from_fn(|| commands.try_recv().ok())
            .find_map(|c| match c {
                MatrixCmd::Send { message, .. } => Some(message),
                _ => None,
            })
            .expect("expected Send command");
        match sent {
            Outgoing::Text {
                plain,
                html,
                mentions,
            } => {
                assert_eq!(plain, "see #alpha:hs");
                assert!(
                    html.unwrap()
                        .contains("href=\"https://matrix.to/#/%23alpha:hs\"")
                );
                assert!(mentions.is_none());
            }
            other => panic!("unexpected outgoing: {other:?}"),
        }

        // Room without canonical alias uses name + `!id` matrix.to link.
        type_text(&mut app, "see #ru");
        app.on_event(key(KeyCode::Tab));
        assert_eq!(app.composer.text(), "see Rust ");
        app.on_event(key(KeyCode::Enter));
        let sent = std::iter::from_fn(|| commands.try_recv().ok())
            .find_map(|c| match c {
                MatrixCmd::Send { message, .. } => Some(message),
                _ => None,
            })
            .expect("expected Send command");
        match sent {
            Outgoing::Text { plain, html, .. } => {
                assert_eq!(plain, "see Rust");
                assert!(html.unwrap().contains("href=\"https://matrix.to/#/!r\""));
            }
            other => panic!("unexpected outgoing: {other:?}"),
        }

        // `/` lists slash commands; selecting `/upload` leaves `/upload ` for path completion.
        type_text(&mut app, "/up");
        assert_eq!(app.complete.active.as_ref().unwrap().kind, Kind::Command);
        app.on_event(key(KeyCode::Tab));
        assert_eq!(app.composer.text(), "/upload ");
        assert!(app.complete.active.is_none());
    }

    #[test]
    fn edit_and_thread_preserve_and_update_mentions() {
        let (mut app, _terminal, mut commands) = connected();
        let mut msg = remote("1", "me", "hi Alice");
        msg.html = Some("hi <a href=\"https://matrix.to/#/@alice:hs\">Alice</a>".into());
        msg.mentions = Some(Mentions::with_user_ids(["@alice:hs".try_into().unwrap()]));
        app.on_matrix(MatrixEvent::Timeline {
            room_id: "!a".into(),
            generation: 1,
            messages: vec![msg],
            reached_start: true,
            detached: None,
        });
        app.on_matrix(MatrixEvent::RoomMembers {
            room_id: "!a".into(),
            members: vec![Member {
                user_id: "@bob:hs".into(),
                display_name: "Bob".into(),
            }],
            can_mention_room: false,
        });
        while commands.try_recv().is_ok() {}

        // Begin edit -> original Alice mention is preserved, plus new Bob mention added via `@`.
        app.perform(Action::Focus(Pane::Timeline));
        app.perform(Action::Edit);
        assert_eq!(app.composer.text(), "hi Alice");
        type_text(&mut app, " and @bo");
        app.on_event(key(KeyCode::Tab));
        assert_eq!(app.composer.text(), "hi Alice and Bob ");
        app.on_event(key(KeyCode::Enter));

        let edit_cmd = std::iter::from_fn(|| commands.try_recv().ok())
            .find_map(|c| match c {
                MatrixCmd::Edit { message, .. } => Some(message),
                _ => None,
            })
            .expect("expected Edit command");
        match edit_cmd {
            Outgoing::Text {
                plain,
                html,
                mentions,
            } => {
                assert_eq!(plain, "hi Alice and Bob");
                let html = html.unwrap();
                assert!(html.contains("@alice:hs"));
                assert!(html.contains("@bob:hs"));
                let m = mentions.unwrap();
                assert_eq!(m.user_ids.len(), 2);
            }
            other => panic!("unexpected edit: {other:?}"),
        }

        // Open a thread and mention Bob inside the thread.
        app.open_thread(0);
        while commands.try_recv().is_ok() {}
        type_text(&mut app, "in thread @bo");
        app.on_event(key(KeyCode::Tab));
        assert_eq!(app.composer.text(), "in thread Bob ");
        app.on_event(key(KeyCode::Enter));
        let thread_cmd = std::iter::from_fn(|| commands.try_recv().ok())
            .find_map(|c| match c {
                MatrixCmd::InThread { command, .. } => Some(*command),
                _ => None,
            })
            .expect("expected InThread Send");
        match thread_cmd {
            MatrixCmd::Send {
                message: Outgoing::Text {
                    plain, mentions, ..
                },
                ..
            } => {
                assert_eq!(plain, "in thread Bob");
                assert!(
                    mentions
                        .unwrap()
                        .user_ids
                        .iter()
                        .any(|u| u.as_str() == "@bob:hs")
                );
            }
            other => panic!("unexpected thread cmd: {other:?}"),
        }
    }

    #[test]
    fn member_fetch_runs_at_most_once_per_room_open() {
        let (mut app, _terminal, mut commands) = connected();
        let initial_fetches = std::iter::from_fn(|| commands.try_recv().ok())
            .filter(|c| matches!(c, MatrixCmd::FetchMembers { room_id } if room_id == "!a"))
            .count();
        assert_eq!(initial_fetches, 1);

        // Typing `@` repeatedly does NOT re-fetch members.
        type_text(&mut app, "@@@");
        let after_typing = std::iter::from_fn(|| commands.try_recv().ok())
            .filter(|c| matches!(c, MatrixCmd::FetchMembers { .. }))
            .count();
        assert_eq!(after_typing, 0);

        // Re-opening the same already-open room does NOT re-fetch.
        app.open_room(0);
        let after_same_open = std::iter::from_fn(|| commands.try_recv().ok())
            .filter(|c| matches!(c, MatrixCmd::FetchMembers { .. }))
            .count();
        assert_eq!(after_same_open, 0);

        // Opening another non-invite room fetches once for that room.
        app.open_room(1);
        let after_switch = std::iter::from_fn(|| commands.try_recv().ok())
            .filter(|c| matches!(c, MatrixCmd::FetchMembers { room_id } if room_id == "!b"))
            .count();
        assert_eq!(after_switch, 1);
    }

    #[test]
    fn common_word_display_name_mentions_only_accepted_occurrence() {
        let (mut app, _terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::RoomMembers {
            room_id: "!a".into(),
            members: vec![Member {
                user_id: "@may:hs".into(),
                display_name: "May".into(),
            }],
            can_mention_room: false,
        });
        while commands.try_recv().is_ok() {}

        // Ordinary word "May" appears first; only the second one is completed via `@may`.
        type_text(&mut app, "May I talk to @may");
        app.on_event(key(KeyCode::Tab));
        assert_eq!(app.composer.text(), "May I talk to May ");

        // Editing before the accepted mention keeps the anchor on the accepted occurrence.
        app.on_event(key(KeyCode::Home));
        type_text(&mut app, "Well, ");
        assert_eq!(app.composer.text(), "Well, May I talk to May ");
        app.on_event(key(KeyCode::Enter));

        let sent = std::iter::from_fn(|| commands.try_recv().ok())
            .find_map(|c| match c {
                MatrixCmd::Send { message, .. } => Some(message),
                _ => None,
            })
            .expect("expected Send command");
        match sent {
            Outgoing::Text {
                plain,
                html,
                mentions,
            } => {
                assert_eq!(plain, "Well, May I talk to May");
                assert_eq!(
                    html.as_deref(),
                    Some("Well, May I talk to <a href=\"https://matrix.to/#/@may:hs\">May</a>")
                );
                let m = mentions.unwrap();
                assert_eq!(m.user_ids.len(), 1);
            }
            other => panic!("unexpected outgoing: {other:?}"),
        }
    }
}
