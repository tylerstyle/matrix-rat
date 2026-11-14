//! Drafts that survive a restart. Unsent text already stays with its room or thread
//! while the app runs; here it is also handed to the worker, which keeps it in the
//! session's store: when a conversation is left, after a pause in typing, and at quit.
//! The worker reports what is stored when a conversation is opened for the first time.
//!
//! Only the text of a new message is kept. An edit is not a draft (the draft put aside
//! for it is), and a reply is stored as plain text: what it answered is not restored.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::attach::Scope;
use super::{App, ComposeMode};
use crate::matrix::MatrixCmd;
use crate::model::RoomKind;

/// How long nothing must be typed before the draft is saved, so that a crash or a
/// kill loses little.
pub(super) const IDLE: Duration = Duration::from_secs(3);

#[derive(Debug, Default)]
pub struct Drafts {
    /// What the store holds per conversation, as far as known: what the worker
    /// reported, or what it was last told to save.
    stored: HashMap<Scope, String>,
    /// The open conversation and its draft as last looked at, to notice changes.
    seen: Option<(Scope, String)>,
    /// The draft changed since that conversation was opened.
    touched: bool,
    /// When the draft last changed, if that has not been dealt with yet.
    changed: Option<Instant>,
}

impl App {
    /// The conversation whose draft is kept: the open room or thread, with an account
    /// (the demo stores nothing). Nothing is written "to" an invitation.
    fn draft_scope(&self) -> Option<Scope> {
        self.matrix.as_ref()?;
        if self.room()?.kind == RoomKind::Invite {
            return None;
        }
        self.attach_scope()
    }

    /// The unsent new message of the open conversation, as it is to be stored.
    fn draft_text(&self, scope: &Scope) -> String {
        // An edit is not a draft; what was put aside for it is.
        let text = match self.compose_mode {
            ComposeMode::Edit(_) => self.edit_stash.as_deref().unwrap_or_default(),
            ComposeMode::Normal | ComposeMode::Reply(_) => self.composer.text(),
        };
        // Neither is an `/upload` command; again, what was put aside for it is.
        let text = match crate::upload::command_argument(text) {
            Some(_) => self.attach.stashed(scope),
            None => text,
        };
        if text.trim().is_empty() {
            String::new()
        } else {
            text.to_owned()
        }
    }

    /// Looks at the draft again, to know whether and when it changed. Called after
    /// everything the user does, and before the draft is judged.
    pub(super) fn note_draft(&mut self) {
        let Some(scope) = self.draft_scope() else {
            self.drafts.seen = None;
            return;
        };
        let text = self.draft_text(&scope);
        match &mut self.drafts.seen {
            Some((seen, last)) if *seen == scope => {
                if *last != text {
                    *last = text;
                    self.drafts.touched = true;
                    self.drafts.changed = Some(Instant::now());
                }
            }
            // Another conversation: what is in the composer now was not typed just now.
            _ => {
                self.drafts.seen = Some((scope, text));
                self.drafts.touched = false;
                self.drafts.changed = None;
            }
        }
    }

    /// Hands the open conversation's draft to the worker if the store holds something
    /// else. Called before the conversation is left and before quitting.
    pub(super) fn save_draft(&mut self) {
        self.write_draft(false);
    }

    /// After a message was sent: its text is no draft any more. Unlike `save_draft`
    /// this also clears a stored draft the worker has not reported yet.
    pub(super) fn forget_sent_draft(&mut self) {
        self.write_draft(true);
    }

    /// `unknown_differs`: what to assume while the worker has not said what is stored.
    /// Normally that it is nothing, so an empty composer the user did not touch needs
    /// no command (and a draft they have not seen yet is not removed). An emptied one
    /// does: whatever is stored is older than what the user did.
    fn write_draft(&mut self, unknown_differs: bool) {
        self.note_draft();
        let Some(scope) = self.draft_scope() else {
            return;
        };
        let text = self.draft_text(&scope);
        self.drafts.changed = None;
        let same = match self.drafts.stored.get(&scope) {
            Some(stored) => *stored == text,
            None => text.is_empty() && !unknown_differs && !self.drafts.touched,
        };
        if same {
            return;
        }
        // Addressed to the open thread by `send_matrix`, like the scope says.
        self.send_matrix(MatrixCmd::SaveDraft {
            room_id: scope.0.clone(),
            text: text.clone(),
        });
        self.drafts.stored.insert(scope, text);
    }

    /// Every tick: saves the draft once nothing was typed for `wait`.
    pub(super) fn save_idle_draft(&mut self, wait: Duration) {
        self.note_draft();
        if self.drafts.changed.is_some_and(|at| at.elapsed() >= wait) {
            self.save_draft();
        }
    }

    /// The last words to the worker when the app quits: the open draft, then the
    /// order to stop. The worker takes them in this order.
    pub(super) fn shut_down_worker(&mut self) {
        self.save_draft();
        self.send_matrix(MatrixCmd::Shutdown);
    }

    /// The worker reports the draft the store holds for a room or thread that was
    /// opened for the first time. It never replaces something the user typed, and it
    /// is never dropped unseen: if nothing was written in that conversation since it
    /// was opened, it goes where its draft is (the composer, or behind an edit or an
    /// `/upload` command begun meanwhile); for a conversation that was left it waits
    /// with it. Only what the user wrote instead takes its place in the store.
    pub(super) fn on_stored_draft(
        &mut self,
        room_id: String,
        thread: Option<String>,
        text: String,
    ) {
        self.note_draft();
        let scope = (room_id, thread);
        // Something was saved since the room was opened: that is newer than the answer.
        if self.drafts.stored.contains_key(&scope) {
            return;
        }
        self.drafts.stored.insert(scope.clone(), text.clone());
        if text.is_empty() {
            return;
        }
        if self.draft_scope().as_ref() != Some(&scope) {
            let (room_id, thread) = scope;
            return self.keep_stored_draft(room_id, thread, text);
        }
        if self.drafts.touched || !self.draft_text(&scope).is_empty() {
            // What the user wrote instead takes its place in the store at the next
            // pause.
            self.drafts.changed = Some(Instant::now());
            return;
        }
        // Untouched and empty: the stored draft goes where `draft_text` looks.
        let editing = matches!(self.compose_mode, ComposeMode::Edit(_));
        let front = match editing {
            true => self.edit_stash.as_deref().unwrap_or_default(),
            false => self.composer.text(),
        };
        if crate::upload::command_argument(front).is_some() {
            // It comes back as the caption when the file is chosen or the command
            // given up.
            self.attach.stash_draft(scope.clone(), text.clone());
        } else if editing {
            // It comes back when the edit ends.
            self.edit_stash = Some(text.clone());
        } else {
            // Also into a reply that was begun but not written yet, as when a reply
            // is begun over a draft.
            self.composer.set_text(&text);
            // Mention marks are not stored with a draft: after a restart a restored
            // `@name` is plain text, and is sent as such unless it is completed again.
            self.clear_completion_marks();
            self.sync_completion();
        }
        self.drafts.seen = Some((scope, text));
    }

    /// A room is gone from the list (left, declined, or removed from it): nothing
    /// kept for its drafts stays, here or in the store. Should the room come back,
    /// it starts empty.
    pub(super) fn forget_room_drafts(&mut self, room_id: &str) {
        let mut threads = self.forget_thread_drafts(room_id);
        // Threads whose drafts are only in the store, as far as known.
        threads.extend(
            self.drafts
                .stored
                .keys()
                .filter(|(room, _)| room == room_id)
                .filter_map(|(_, thread)| thread.clone()),
        );
        threads.sort();
        threads.dedup();
        self.drafts.stored.retain(|(room, _), _| room != room_id);
        if matches!(&self.drafts.seen, Some(((room, _), _)) if room == room_id) {
            self.drafts.seen = None;
        }
        self.attach.forget_room(room_id);
        self.forget_room_marks(room_id);
        self.send_matrix(MatrixCmd::ForgetDrafts {
            room_id: room_id.to_owned(),
            threads,
        });
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::action::{Action, Pane};
    use crate::matrix::{MatrixEvent, MatrixHandle};
    use crate::model::{ItemRef, Message, Room};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
    }

    fn message(n: usize, own: bool) -> Message {
        Message {
            id: n.to_string(),
            item: Some(ItemRef::Event(format!("$e{n}"))),
            sender: if own { "me" } else { "ann" }.to_owned(),
            body: format!("line {n}"),
            own,
            ..Message::default()
        }
    }

    /// Connected to a fake worker, with two rooms; the first is open and has messages
    /// (the second one is ours), the composer has the focus.
    fn connected() -> (App, UnboundedReceiver<MatrixCmd>) {
        let mut app = App::demo();
        let (handle, commands) = MatrixHandle::for_test();
        app.matrix = Some(handle);
        let mut alpha = Room::new("!a", "Alpha", RoomKind::Room);
        alpha.messages = vec![message(0, false), message(1, true), message(2, false)];
        app.rooms = vec![alpha, Room::new("!b", "Beta", RoomKind::Room)];
        app.active = 0;
        app.screen = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.perform(Action::Focus(Pane::Composer));
        (app, commands)
    }

    /// The draft commands sent so far, with their text: `!room[ in $thread]=text`.
    fn saves(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            match command {
                MatrixCmd::SaveDraft { room_id, text } => seen.push(format!("{room_id}={text}")),
                MatrixCmd::InThread { root, command } => {
                    if let MatrixCmd::SaveDraft { room_id, text } = *command {
                        seen.push(format!("{room_id} in {root}={text}"));
                    }
                }
                _ => {}
            }
        }
        seen
    }

    fn stored(room_id: &str, text: &str) -> MatrixEvent {
        MatrixEvent::Draft {
            room_id: room_id.to_owned(),
            thread: None,
            text: text.to_owned(),
        }
    }

    #[test]
    fn a_stored_draft_fills_the_empty_composer_of_the_open_room() {
        let (mut app, mut commands) = connected();
        app.on_matrix(stored("!a", "see you at @ann's"));
        assert_eq!(app.composer.text(), "see you at @ann's");
        // A restored `@name` is plain text.
        assert!(app.complete.marks.is_empty());

        // It is what the store holds: nothing is written back, now or when leaving.
        app.save_idle_draft(Duration::ZERO);
        app.perform(Action::OpenRoom(1));
        assert_eq!(saves(&mut commands), Vec::<String>::new());
        assert_eq!(app.composer.text(), "");
        app.perform(Action::OpenRoom(0));
        assert_eq!(app.composer.text(), "see you at @ann's");
    }

    #[test]
    fn a_stored_draft_never_replaces_what_the_user_typed() {
        let (mut app, mut commands) = connected();
        type_text(&mut app, "new");
        app.on_matrix(stored("!a", "old"));
        assert_eq!(app.composer.text(), "new");
        // What was typed takes the old draft's place in the store.
        app.save_idle_draft(Duration::ZERO);
        assert_eq!(saves(&mut commands), ["!a=new"]);
    }

    #[test]
    fn a_stored_draft_stays_out_after_typing_even_if_the_composer_is_empty_again() {
        let (mut app, mut commands) = connected();
        type_text(&mut app, "x");
        app.on_event(key(KeyCode::Backspace));
        assert!(app.composer.is_empty());
        app.on_matrix(stored("!a", "old"));
        assert_eq!(app.composer.text(), "");
        // The store follows the composer.
        app.save_idle_draft(Duration::ZERO);
        assert_eq!(saves(&mut commands), ["!a="]);
    }

    /// Nothing was saved, by a pause in typing, by leaving the room or by quitting.
    fn nothing_is_written(app: &mut App, commands: &mut UnboundedReceiver<MatrixCmd>) {
        app.save_idle_draft(Duration::ZERO);
        assert_eq!(saves(commands), Vec::<String>::new(), "after a pause");
        app.perform(Action::OpenRoom(1));
        assert_eq!(saves(commands), Vec::<String>::new(), "on leaving");
        app.perform(Action::OpenRoom(0));
        app.shut_down_worker();
        assert_eq!(saves(commands), Vec::<String>::new(), "at quit");
    }

    #[test]
    fn a_stored_draft_that_arrives_during_an_edit_waits_behind_it() {
        // The edit was begun (here by Up) before the worker said what is stored.
        let (mut app, mut commands) = connected();
        app.on_event(key(KeyCode::Up));
        assert_eq!(app.compose_mode, ComposeMode::Edit(1));
        app.on_matrix(stored("!a", "old"));
        assert_eq!(app.composer.text(), "line 1");
        // It is not removed from the store after a pause.
        app.save_idle_draft(Duration::ZERO);
        assert_eq!(saves(&mut commands), Vec::<String>::new());
        // It is the draft the edit gives back.
        app.perform(Action::CancelCompose);
        assert_eq!(app.composer.text(), "old");
        nothing_is_written(&mut app, &mut commands);
        assert_eq!(app.composer.text(), "old");

        // Leaving the room in the middle of the edit keeps it as well.
        let (mut app, mut commands) = connected();
        app.timeline.selected = Some(1);
        app.perform(Action::Edit);
        app.on_matrix(stored("!a", "old"));
        app.perform(Action::OpenRoom(1));
        assert_eq!(saves(&mut commands), Vec::<String>::new());
        assert_eq!(app.rooms[0].draft, "old");

        // And so does quitting.
        let (mut app, mut commands) = connected();
        app.on_event(key(KeyCode::Up));
        app.on_matrix(stored("!a", "old"));
        app.shut_down_worker();
        assert_eq!(saves(&mut commands), Vec::<String>::new());
    }

    #[test]
    fn a_stored_draft_that_arrives_in_an_unwritten_reply_becomes_its_text() {
        let (mut app, mut commands) = connected();
        app.timeline.selected = Some(0);
        app.perform(Action::Reply);
        app.on_matrix(stored("!a", "old"));
        assert_eq!(app.composer.text(), "old");
        assert_eq!(app.compose_mode, ComposeMode::Reply(0));
        nothing_is_written(&mut app, &mut commands);
        assert_eq!(app.composer.text(), "old");
    }

    #[test]
    fn a_stored_draft_that_arrives_during_an_upload_command_waits_behind_it() {
        let (mut app, mut commands) = connected();
        app.perform(Action::Attach);
        type_text(&mut app, "x");
        assert_eq!(app.composer.text(), "/upload x");
        app.on_matrix(stored("!a", "old"));
        assert_eq!(app.composer.text(), "/upload x");
        app.save_idle_draft(Duration::ZERO);
        assert_eq!(saves(&mut commands), Vec::<String>::new());
        // Esc gives the command up and the draft is there.
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.composer.text(), "old");
        nothing_is_written(&mut app, &mut commands);

        // Quitting with the command still typed keeps it too.
        let (mut app, mut commands) = connected();
        app.perform(Action::Attach);
        app.on_matrix(stored("!a", "old"));
        app.shut_down_worker();
        assert_eq!(saves(&mut commands), Vec::<String>::new());
    }

    #[test]
    fn only_what_the_user_wrote_takes_a_late_stored_drafts_place() {
        // Written into a reply.
        let (mut app, mut commands) = connected();
        app.timeline.selected = Some(0);
        app.perform(Action::Reply);
        type_text(&mut app, "yes");
        app.on_matrix(stored("!a", "old"));
        assert_eq!(app.composer.text(), "yes");
        app.save_idle_draft(Duration::ZERO);
        assert_eq!(saves(&mut commands), ["!a=yes"]);

        // Written before an edit was begun over it.
        let (mut app, mut commands) = connected();
        type_text(&mut app, "mine");
        app.timeline.selected = Some(1);
        app.perform(Action::Edit);
        app.on_matrix(stored("!a", "old"));
        app.perform(Action::CancelCompose);
        assert_eq!(app.composer.text(), "mine");
        app.save_idle_draft(Duration::ZERO);
        assert_eq!(saves(&mut commands), ["!a=mine"]);
    }

    #[test]
    fn a_draft_emptied_before_the_store_answered_stays_empty() {
        // Typed, erased and left: the user emptied the field, so what the store
        // holds is removed and its late answer does not come back.
        let (mut app, mut commands) = connected();
        type_text(&mut app, "x");
        app.on_event(key(KeyCode::Backspace));
        app.perform(Action::OpenRoom(1));
        assert_eq!(saves(&mut commands), ["!a="]);
        app.on_matrix(stored("!a", "old"));
        assert_eq!(app.rooms[0].draft, "");

        // Merely passing through a room removes nothing.
        let (mut app, mut commands) = connected();
        app.perform(Action::OpenRoom(1));
        assert_eq!(saves(&mut commands), Vec::<String>::new());
    }

    fn listed(id: &str) -> crate::matrix::RoomSummary {
        crate::matrix::RoomSummary {
            id: id.to_owned(),
            name: id.to_owned(),
            category: crate::matrix::RoomCategory::Room,
            notifications: 0,
            preview: None,
            server_notifications: 0,
            mention: false,
            unread: false,
            marked_unread: false,
            avatar: None,
            alias: None,
        }
    }

    /// The `ForgetDrafts` commands sent so far: `!room[$thread,…]`.
    fn forgotten(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            if let MatrixCmd::ForgetDrafts { room_id, threads } = command {
                seen.push(format!("{room_id}[{}]", threads.join(",")));
            }
        }
        seen
    }

    #[test]
    fn a_room_that_leaves_the_list_takes_its_drafts_along() {
        let (mut app, mut commands) = connected();
        // A draft in a thread of the first room, one in the room, and a second thread
        // whose draft is only known from the store.
        app.open_thread(0);
        type_text(&mut app, "thread words");
        app.close_thread();
        type_text(&mut app, "room words");
        app.on_matrix(MatrixEvent::Draft {
            room_id: "!a".to_owned(),
            thread: Some("$e2".to_owned()),
            text: "other thread".to_owned(),
        });
        app.perform(Action::OpenRoom(1));
        assert_eq!(app.rooms[0].draft, "room words");
        assert_eq!(forgotten(&mut commands), Vec::<String>::new());

        // The room is left (here or elsewhere): the list comes without it.
        app.on_matrix(MatrixEvent::Rooms(vec![listed("!b")]));
        assert_eq!(forgotten(&mut commands), ["!a[$e0,$e2]"]);
        assert!(app.drafts.stored.keys().all(|(room, _)| room != "!a"));

        // Rejoined later, it starts empty, and what the store says then counts.
        app.on_matrix(MatrixEvent::Rooms(vec![listed("!a"), listed("!b")]));
        assert_eq!(app.rooms[0].draft, "");
        app.perform(Action::OpenRoom(0));
        assert_eq!(app.composer.text(), "");
        app.on_matrix(stored("!a", "fresh"));
        assert_eq!(app.composer.text(), "fresh");
        // Nothing is asked to be forgotten for rooms that stay.
        assert_eq!(forgotten(&mut commands), Vec::<String>::new());
    }

    #[test]
    fn the_open_room_leaving_the_list_takes_its_drafts_along_too() {
        let (mut app, mut commands) = connected();
        type_text(&mut app, "abc");
        app.save_idle_draft(Duration::ZERO);
        app.on_matrix(MatrixEvent::Rooms(vec![listed("!b")]));
        assert_eq!(forgotten(&mut commands), ["!a[]"]);
        assert!(app.drafts.stored.is_empty());
        assert_eq!(app.room().map(|room| room.id.as_str()), Some("!b"));
        assert_eq!(app.composer.text(), "");
        // The room that is open now is not written to on account of the other.
        app.shut_down_worker();
        assert_eq!(saves(&mut commands), Vec::<String>::new());
    }

    #[test]
    fn a_stored_draft_for_a_room_that_was_left_waits_there() {
        let (mut app, mut commands) = connected();
        app.perform(Action::OpenRoom(1));
        app.on_matrix(stored("!a", "old"));
        assert_eq!(app.composer.text(), "");
        assert_eq!(app.rooms[0].draft, "old");
        app.perform(Action::OpenRoom(0));
        assert_eq!(app.composer.text(), "old");
        assert_eq!(saves(&mut commands), Vec::<String>::new());

        // Not if something was typed there before it was left: that was saved, and
        // the answer is older.
        let (mut app, mut commands) = connected();
        type_text(&mut app, "new");
        app.perform(Action::OpenRoom(1));
        assert_eq!(saves(&mut commands), ["!a=new"]);
        app.on_matrix(stored("!a", "old"));
        assert_eq!(app.rooms[0].draft, "new");

        // Nor over a draft that waits there for another reason.
        let (mut app, _commands) = connected();
        app.rooms[1].draft = "waiting".to_owned();
        app.on_matrix(stored("!b", "old"));
        assert_eq!(app.rooms[1].draft, "waiting");
    }

    #[test]
    fn leaving_a_room_saves_its_draft_if_it_changed() {
        let (mut app, mut commands) = connected();
        type_text(&mut app, "abc");
        app.perform(Action::OpenRoom(1));
        assert_eq!(saves(&mut commands), ["!a=abc"]);

        // Unchanged: nothing to save, in either room.
        app.perform(Action::OpenRoom(0));
        app.perform(Action::OpenRoom(1));
        assert_eq!(saves(&mut commands), Vec::<String>::new());

        // Emptied: the stored draft is removed.
        app.perform(Action::OpenRoom(0));
        for _ in 0..3 {
            app.on_event(key(KeyCode::Backspace));
        }
        app.perform(Action::OpenRoom(1));
        assert_eq!(saves(&mut commands), ["!a="]);
    }

    #[test]
    fn quitting_saves_the_open_draft_before_the_worker_is_stopped() {
        let (mut app, mut commands) = connected();
        type_text(&mut app, "abc");
        app.shut_down_worker();
        let mut last = Vec::new();
        while let Ok(command) = commands.try_recv() {
            if !matches!(command, MatrixCmd::Typing { .. }) {
                last.push(format!("{command:?}"));
            }
        }
        assert_eq!(last, ["SaveDraft(!a, 3 bytes)", "Shutdown"]);
    }

    #[test]
    fn a_pause_in_typing_saves_the_draft_once() {
        let (mut app, mut commands) = connected();
        type_text(&mut app, "abc");
        // Still typing, as far as the timer knows.
        app.save_idle_draft(Duration::from_secs(3600));
        assert_eq!(saves(&mut commands), Vec::<String>::new());
        app.save_idle_draft(Duration::ZERO);
        assert_eq!(saves(&mut commands), ["!a=abc"]);
        app.save_idle_draft(Duration::ZERO);
        app.on_tick();
        assert_eq!(saves(&mut commands), Vec::<String>::new());

        type_text(&mut app, "d");
        app.save_idle_draft(Duration::ZERO);
        assert_eq!(saves(&mut commands), ["!a=abcd"]);
    }

    #[test]
    fn sending_clears_the_stored_draft() {
        // A restored draft is sent.
        let (mut app, mut commands) = connected();
        app.on_matrix(stored("!a", "hello"));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.composer.text(), "");
        assert_eq!(saves(&mut commands), ["!a="]);

        // Saved at a pause, then sent.
        let (mut app, mut commands) = connected();
        app.on_matrix(stored("!a", ""));
        type_text(&mut app, "abc");
        app.save_idle_draft(Duration::ZERO);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(saves(&mut commands), ["!a=abc", "!a="]);

        // The store is known to hold nothing: nothing to clear.
        let (mut app, mut commands) = connected();
        app.on_matrix(stored("!a", ""));
        type_text(&mut app, "abc");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(saves(&mut commands), Vec::<String>::new());

        // Sent before the worker said what is stored: cleared to be sure, and the
        // late answer does not bring the old draft back.
        let (mut app, mut commands) = connected();
        type_text(&mut app, "abc");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(saves(&mut commands), ["!a="]);
        app.on_matrix(stored("!a", "old"));
        assert_eq!(app.composer.text(), "");
        app.perform(Action::OpenRoom(1));
        assert_eq!(app.rooms[0].draft, "");
    }

    #[test]
    fn an_edit_is_not_stored_but_the_draft_behind_it_is() {
        let (mut app, mut commands) = connected();
        type_text(&mut app, "draft");
        app.timeline.selected = Some(1);
        app.perform(Action::Edit);
        type_text(&mut app, " changed");
        assert_eq!(app.composer.text(), "line 1 changed");
        app.save_idle_draft(Duration::ZERO);
        assert_eq!(saves(&mut commands), ["!a=draft"]);

        // Sending the edit leaves the draft, in the composer and in the store.
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.composer.text(), "draft");
        app.shut_down_worker();
        assert_eq!(saves(&mut commands), Vec::<String>::new());
    }

    #[test]
    fn of_a_reply_the_text_is_stored() {
        let (mut app, mut commands) = connected();
        app.timeline.selected = Some(0);
        app.perform(Action::Reply);
        type_text(&mut app, "yes");
        app.shut_down_worker();
        assert_eq!(saves(&mut commands), ["!a=yes"]);
    }

    #[test]
    fn an_upload_command_is_not_stored_but_the_draft_behind_it_is() {
        let (mut app, mut commands) = connected();
        type_text(&mut app, "caption");
        app.perform(Action::Attach);
        assert_eq!(app.composer.text(), "/upload ");
        type_text(&mut app, "x");
        app.shut_down_worker();
        assert_eq!(saves(&mut commands), ["!a=caption"]);
    }

    #[test]
    fn a_thread_has_a_stored_draft_of_its_own() {
        let (mut app, mut commands) = connected();
        type_text(&mut app, "room words");
        app.open_thread(0);
        // The room's draft was saved on the way in.
        assert_eq!(saves(&mut commands), ["!a=room words"]);
        assert_eq!(app.composer.text(), "");

        // The thread's stored draft arrives.
        app.on_matrix(MatrixEvent::Draft {
            room_id: "!a".to_owned(),
            thread: Some("$e0".to_owned()),
            text: "thread words".to_owned(),
        });
        assert_eq!(app.composer.text(), "thread words");
        type_text(&mut app, "!");
        app.close_thread();
        assert_eq!(saves(&mut commands), ["!a in $e0=thread words!"]);
        assert_eq!(app.composer.text(), "room words");

        // Back in the thread the draft is the one kept in memory; quitting there
        // saves nothing new.
        app.open_thread(0);
        assert_eq!(app.composer.text(), "thread words!");
        app.shut_down_worker();
        assert_eq!(saves(&mut commands), Vec::<String>::new());
    }

    #[test]
    fn a_stored_draft_for_the_room_behind_a_thread_or_for_a_closed_thread_waits() {
        let (mut app, _commands) = connected();
        app.open_thread(0);
        app.on_matrix(stored("!a", "room words"));
        assert_eq!(app.composer.text(), "");
        app.close_thread();
        assert_eq!(app.composer.text(), "room words");
        app.on_matrix(MatrixEvent::Draft {
            room_id: "!a".to_owned(),
            thread: Some("$e0".to_owned()),
            text: "thread words".to_owned(),
        });
        assert_eq!(app.composer.text(), "room words");
        app.open_thread(0);
        assert_eq!(app.composer.text(), "thread words");
    }

    #[test]
    fn the_demo_stores_nothing() {
        let mut app = App::demo();
        app.perform(Action::Focus(Pane::Composer));
        type_text(&mut app, "abc");
        app.save_idle_draft(Duration::ZERO);
        app.perform(Action::OpenRoom(1));
        app.shut_down_worker();
        assert!(app.drafts.stored.is_empty());
        assert!(app.drafts.seen.is_none());
        // Drafts still stay with their room.
        app.perform(Action::OpenRoom(0));
        assert_eq!(app.composer.text(), "abc");
    }

    #[test]
    fn drafts_are_not_printed() {
        let command = MatrixCmd::SaveDraft {
            room_id: "!a".to_owned(),
            text: "secret".to_owned(),
        };
        assert_eq!(format!("{command:?}"), "SaveDraft(!a, 6 bytes)");
        assert_eq!(
            format!("{:?}", stored("!a", "secret")),
            "Draft(!a, 6 bytes)"
        );
        let event = MatrixEvent::Draft {
            room_id: "!a".to_owned(),
            thread: Some("$e0".to_owned()),
            text: "secret".to_owned(),
        };
        assert_eq!(format!("{event:?}"), "Draft(!a, $e0, 6 bytes)");
        let command = MatrixCmd::ForgetDrafts {
            room_id: "!a".to_owned(),
            threads: vec!["$e0".to_owned()],
        };
        assert_eq!(format!("{command:?}"), "ForgetDrafts(!a, 1 threads)");
    }
}
