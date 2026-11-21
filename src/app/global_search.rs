//! Global search across loaded messages in all joined rooms (`Ctrl-G`).

use std::fmt;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, TimelineState};
use crate::action::{Action, Pane};
use crate::keymap::Context;
use crate::lineedit::{EditKeys, LineEdit};
use crate::matrix::search::{query_terms, window_snippet};
use crate::matrix::{DetachedTimeline, MatrixCmd, SearchHit};
use crate::modal::Modal;
#[cfg(test)]
use crate::model::RoomKind;
use crate::model::{ItemRef, Message, MsgKind};

/// Pause after typing before sending a search query to the index worker.
pub(crate) const SEARCH_DEBOUNCE: Duration = Duration::from_millis(200);
const DEMO_MAX_HITS: usize = 200;

#[derive(Clone)]
pub struct GlobalSearch {
    pub query: LineEdit,
    pub generation: u64,
    pub pending_at: Option<Instant>,
    pub searching: bool,
    pub hits: Vec<SearchHit>,
    pub selected: usize,
    pub scroll: usize,
    /// Hit rows that fit cleanly on the last draw, used for PgUp/PgDn and wheel clamping.
    pub rows: usize,
    /// Keeps keyboard selection in view on the next draw.
    pub reveal: bool,
    /// Query corresponding to `hits` / `searching`, kept for snippet alignment and empty states.
    pub active_query: String,
    pub note: Option<String>,
}

/// Redacts query text, active query, and notes from logs/panics.
impl fmt::Debug for GlobalSearch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GlobalSearch")
            .field("generation", &self.generation)
            .field("pending", &self.pending_at.is_some())
            .field("searching", &self.searching)
            .field("hits", &self.hits.len())
            .field("selected", &self.selected)
            .finish()
    }
}

impl GlobalSearch {
    pub fn new(generation: u64) -> Self {
        Self {
            query: LineEdit::new(),
            generation,
            pending_at: None,
            searching: false,
            hits: Vec::new(),
            selected: 0,
            scroll: 0,
            rows: 8,
            reveal: false,
            active_query: String::new(),
            note: None,
        }
    }

    /// Moves the selection by `delta`, clamping to `hits`.
    pub fn step(&mut self, delta: isize) {
        if self.hits.is_empty() {
            return;
        }
        let max = self.hits.len() - 1;
        let next = (self.selected as isize + delta).clamp(0, max as isize) as usize;
        if next != self.selected {
            self.selected = next;
            self.reveal = true;
        }
    }

    /// Scrolls the hit list without moving the selection (mouse wheel).
    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.hits.len().saturating_sub(self.rows.max(1));
        self.scroll = (self.scroll as isize + delta).clamp(0, max as isize) as usize;
        self.reveal = false;
    }

    pub fn hit_at(&self, index: usize) -> Option<&SearchHit> {
        self.hits.get(index)
    }
}

#[derive(Clone)]
pub(super) struct PendingJump {
    pub generation: u64,
    pub room_id: String,
    pub event_id: String,
    pub thread_root: Option<String>,
}

/// Redacts room, event, and thread identifiers from logs and panic output.
impl fmt::Debug for PendingJump {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingJump")
            .field("generation", &self.generation)
            .field("has_thread", &self.thread_root.is_some())
            .finish()
    }
}

impl App {
    /// Opens the global message search modal.
    pub(super) fn open_global_search(&mut self) {
        if self.modal.is_some() || self.images.viewer_open() {
            return;
        }
        self.search_gen += 1;
        let generation = self.search_gen;
        self.open_modal(Modal::GlobalSearch(GlobalSearch::new(generation)));
    }

    /// Closes the global search modal and cancels any active worker search.
    pub(super) fn close_global_search(&mut self) {
        if matches!(self.modal, Some(Modal::GlobalSearch(_))) {
            self.modal = None;
            self.search_gen += 1;
            self.send_matrix(MatrixCmd::CancelSearch);
        }
    }

    pub(super) fn search_debounce_deadline(&self) -> Option<tokio::time::Instant> {
        match &self.modal {
            Some(Modal::GlobalSearch(state)) => {
                state.pending_at.map(|at| (at + SEARCH_DEBOUNCE).into())
            }
            _ => None,
        }
    }

    pub(super) fn tick_global_search(&mut self) {
        let due = matches!(
            &self.modal,
            Some(Modal::GlobalSearch(state))
                if state
                    .pending_at
                    .is_some_and(|at| at.elapsed() >= SEARCH_DEBOUNCE)
        );
        if due {
            self.dispatch_global_search();
        }
    }

    pub(super) fn on_global_search_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let key = KeyEvent::new(code, mods);
        let toggle_key = (mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            || matches!(code, KeyCode::F(_)))
            && self.keymap.lookup(Context::Global, key) == Some(Action::SearchEverywhere);
        if code == KeyCode::Esc || toggle_key {
            self.close_global_search();
            return;
        }

        let Some(Modal::GlobalSearch(state)) = &mut self.modal else {
            return;
        };
        let page = state.rows.max(1) as isize;
        match (code, mods) {
            (KeyCode::Up, _) => state.step(-1),
            (KeyCode::Down, _) => state.step(1),
            (KeyCode::Char('p'), m) if m == KeyModifiers::ALT => state.step(-1),
            (KeyCode::Char('n'), m) if m == KeyModifiers::CONTROL => state.step(1),
            (KeyCode::PageUp, _) => state.step(-page),
            (KeyCode::PageDown, _) => state.step(page),
            (KeyCode::Enter, _) => {
                if state.pending_at.is_some() && state.hits.is_empty() {
                    self.dispatch_global_search();
                    return;
                }
                let selected = state.selected;
                self.activate_global_search(selected);
            }
            _ => {
                if state.query.handle_key(code, mods, EditKeys::FULL).changed() {
                    self.on_global_search_query_changed();
                }
            }
        }
    }

    pub(super) fn global_search_paste(&mut self, text: &str) {
        let Some(Modal::GlobalSearch(state)) = &mut self.modal else {
            return;
        };
        if state.query.paste(text).changed() {
            self.on_global_search_query_changed();
        }
    }

    fn on_global_search_query_changed(&mut self) {
        self.search_gen += 1;
        let next_gen = self.search_gen;
        let was_searching = {
            let Some(Modal::GlobalSearch(state)) = &mut self.modal else {
                return;
            };
            let was_searching = state.searching;
            let trimmed_empty = state.query.trim().is_empty();
            state.generation = next_gen;
            state.hits.clear();
            state.selected = 0;
            state.scroll = 0;
            state.reveal = false;
            state.note = None;
            state.searching = false;
            if trimmed_empty {
                state.pending_at = None;
                state.active_query.clear();
            } else {
                state.pending_at = Some(Instant::now());
            }
            was_searching
        };
        if was_searching {
            self.send_matrix(MatrixCmd::CancelSearch);
        }
    }

    pub(super) fn dispatch_global_search(&mut self) {
        self.search_gen += 1;
        let generation = self.search_gen;
        let query = {
            let Some(Modal::GlobalSearch(state)) = &mut self.modal else {
                return;
            };
            state.pending_at = None;
            state.hits.clear();
            state.selected = 0;
            state.scroll = 0;
            state.reveal = false;
            state.note = None;
            state.generation = generation;
            let trimmed = state.query.trim().to_owned();
            state.active_query = trimmed.clone();
            if trimmed.is_empty() {
                state.searching = false;
                return;
            }
            state.searching = true;
            trimmed
        };
        self.dirty = true;

        if self.matrix.is_some() {
            self.send_matrix(MatrixCmd::SearchMessages { generation, query });
        } else {
            let hits = self.demo_global_search(&query);
            self.on_search_results(generation, hits, true, None);
        }
    }

    fn demo_global_search(&self, query: &str) -> Vec<SearchHit> {
        let terms: Vec<String> = query_terms(query)
            .into_iter()
            .map(str::to_lowercase)
            .collect();
        if terms.is_empty() {
            return Vec::new();
        }
        let make_hit = |room: &crate::model::Room,
                        msg: &Message,
                        event_id: String,
                        thread_root: Option<String>|
         -> Option<SearchHit> {
            if matches!(msg.kind, MsgKind::System | MsgKind::ReadMarker) {
                return None;
            }
            let lower_body = msg.body.to_lowercase();
            let lower_sender = msg.sender.to_lowercase();
            let all_match = terms
                .iter()
                .all(|t| lower_body.contains(t) || lower_sender.contains(t));
            if !all_match {
                return None;
            }
            let snippet = window_snippet(&msg.body, query);
            if snippet.is_empty() {
                return None;
            }
            Some(SearchHit {
                room_id: room.id.clone(),
                event_id,
                thread_root,
                room_name: room.name.clone(),
                sender: msg.sender.clone(),
                date: msg.day.clone(),
                snippet,
            })
        };
        let mut hits = Vec::new();
        for room in &self.rooms {
            if room.is_pending() {
                continue;
            }
            for (idx, msg) in room.messages.iter().enumerate().rev() {
                if let Some(thread) = &msg.thread {
                    for (ridx, reply) in thread.demo_replies.iter().enumerate().rev() {
                        if let Some(hit) = make_hit(
                            room,
                            reply,
                            format!("demo:{idx}:{ridx}"),
                            Some(format!("demo:{idx}")),
                        ) {
                            hits.push(hit);
                            if hits.len() >= DEMO_MAX_HITS {
                                return hits;
                            }
                        }
                    }
                }
                if let Some(hit) = make_hit(room, msg, format!("demo:{idx}"), None) {
                    hits.push(hit);
                    if hits.len() >= DEMO_MAX_HITS {
                        return hits;
                    }
                }
            }
        }
        hits
    }

    pub(super) fn on_search_results(
        &mut self,
        generation: u64,
        hits: Vec<SearchHit>,
        done: bool,
        note: Option<String>,
    ) {
        let Some(Modal::GlobalSearch(state)) = &mut self.modal else {
            return;
        };
        if state.generation != generation {
            return;
        }
        let was_empty = state.hits.is_empty();
        state.hits.extend(hits);
        if was_empty && !state.hits.is_empty() {
            state.selected = 0;
            state.reveal = true;
        }
        if done {
            state.searching = false;
            if note.is_some() {
                state.note = note;
            }
        }
    }

    pub(super) fn activate_global_search(&mut self, index: usize) {
        let hit = match &self.modal {
            Some(Modal::GlobalSearch(state)) => state.hit_at(index).cloned(),
            _ => None,
        };
        let Some(hit) = hit else {
            return;
        };
        self.close_global_search();
        self.jump_to_event(hit.room_id, hit.event_id, hit.thread_root);
    }

    /// Aborts any in-flight event jump without disturbing the currently visible view.
    pub(super) fn cancel_pending_jump(&mut self) {
        if let Some(pending) = self.pending_jump.take() {
            self.send_matrix(MatrixCmd::CancelOpenEvent {
                generation: pending.generation,
            });
        }
    }

    /// Jumps to `event_id` in `room_id` (optionally inside `thread_root`). Selects
    /// immediately if already in the active view; otherwise asks the worker to open
    /// the cached live timeline or an event-focused `/context` slice while keeping the
    /// active view intact until it arrives.
    pub(super) fn jump_to_event(
        &mut self,
        room_id: String,
        event_id: String,
        thread_root: Option<String>,
    ) {
        let Some(room_index) = self.rooms.iter().position(|r| r.id == room_id) else {
            self.show_toast("That room is no longer available");
            return;
        };
        if self.rooms[room_index].is_pending() {
            self.show_toast("Join this room before jumping to a message in it");
            return;
        }

        // With no chat open the room is not on screen, whichever it is.
        let open_here = Some(room_index) == self.open_index();
        let in_target_thread =
            thread_root.is_some() && open_here && self.thread_key() == thread_root.as_deref();
        let in_target_room = thread_root.is_none() && open_here && !self.thread_open();
        if (in_target_thread || in_target_room)
            && let Some(pos) = find_target_message(
                self.messages(),
                &event_id,
                self.matrix.is_none(),
                in_target_thread,
            )
        {
            self.cancel_pending_jump();
            self.set_focus(Pane::Timeline);
            self.timeline.selected = Some(pos);
            self.timeline.reveal_selected = true;
            return;
        }

        // An event link without a thread root can still point to a message (or the root)
        // in the currently open thread; select it right here if loaded.
        if thread_root.is_none()
            && open_here
            && self.thread_open()
            && let Some(pos) = find_target_message(self.messages(), &event_id, false, true)
        {
            self.cancel_pending_jump();
            self.set_focus(Pane::Timeline);
            self.timeline.selected = Some(pos);
            self.timeline.reveal_selected = true;
            return;
        }

        if self.matrix.is_none() {
            self.jump_in_demo(room_index, &room_id, &event_id, thread_root.as_deref());
            return;
        }

        self.cancel_pending_jump();
        let generation = self.allocate_gen();
        self.pending_jump = Some(PendingJump {
            generation,
            room_id: room_id.clone(),
            event_id: event_id.clone(),
            thread_root: thread_root.clone(),
        });
        self.send_matrix(MatrixCmd::OpenEvent {
            room_id,
            event_id,
            thread_root,
            generation,
        });
    }

    fn jump_in_demo(
        &mut self,
        room_index: usize,
        room_id: &str,
        event_id: &str,
        thread_root: Option<&str>,
    ) {
        if self.space.is_some() && !self.space_rooms.contains(room_id) {
            self.select_space(None);
        }
        let open_here = Some(room_index) == self.open_index();
        let in_target_thread =
            thread_root.is_some() && open_here && self.thread_key() == thread_root;
        let in_target_room = thread_root.is_none() && open_here && !self.thread_open();
        if !in_target_thread && !in_target_room {
            self.open_room(room_index);
            if let Some(at) = self.visible_rooms().iter().position(|i| *i == room_index) {
                self.sidebar.cursor = at;
                self.sidebar.reveal_cursor = true;
            }
        }
        if let Some(root) = thread_root
            && self.thread_key() != Some(root)
        {
            if let Some(root_pos) = find_target_message(self.messages(), root, true, false) {
                self.open_thread(root_pos);
            } else {
                self.show_toast("That message is not in this conversation");
                return;
            }
        }
        if let Some(pos) = find_target_message(self.messages(), event_id, true, self.thread_open())
        {
            self.set_focus(Pane::Timeline);
            self.timeline.selected = Some(pos);
            self.timeline.reveal_selected = true;
        } else {
            self.show_toast("That message is not in this conversation");
        }
    }

    pub(super) fn on_jump_arrived(
        &mut self,
        generation: u64,
        room_id: String,
        messages: Vec<Message>,
        reached_start: bool,
        detached: Option<DetachedTimeline>,
    ) {
        let Some(pending) = self.pending_jump.take() else {
            return;
        };
        if pending.generation != generation {
            return;
        }
        let Some(room_index) = self.rooms.iter().position(|r| r.id == room_id) else {
            // The worker switched to the jump already and streams a room no longer
            // listed: told the jump is off, it streams the view still shown again.
            self.send_matrix(MatrixCmd::CancelOpenEvent { generation });
            self.show_toast("That room is no longer available");
            return;
        };

        self.generation = generation;
        self.auto_opened = false;
        self.row_cache = None;

        if self.space.is_some() && !self.space_rooms.contains(&room_id) {
            self.select_space(None);
        }

        let want_thread = detached
            .as_ref()
            .and_then(|d| d.thread_root.clone())
            .or(pending.thread_root);

        match want_thread {
            Some(root) => {
                let same_thread = Some(room_index) == self.open_index()
                    && self.threads.open_root() == Some(root.as_str());
                if !same_thread {
                    self.leave_thread();
                    self.switch_room_state(room_index, false);
                    if let Some(at) = self.visible_rooms().iter().position(|i| *i == room_index) {
                        self.sidebar.cursor = at;
                        self.sidebar.reveal_cursor = true;
                    }
                    self.enter_thread_from_jump(root);
                }
            }
            None => {
                self.leave_thread();
                self.switch_room_state(room_index, false);
                if let Some(at) = self.visible_rooms().iter().position(|i| *i == room_index) {
                    self.sidebar.cursor = at;
                    self.sidebar.reveal_cursor = true;
                }
            }
        }

        let target_id = detached
            .as_ref()
            .map(|d| d.target_event.as_str())
            .unwrap_or(&pending.event_id);

        // The "new" line of the room: placed by these messages if they are the
        // room's newest and the room was not open before; never by a slice from
        // somewhere in its history. The message jumped to is what the view shows,
        // not the line (the view's state is made anew below).
        let messages = self.place_unread_line(messages, detached.is_some());
        let mut messages = if self.thread_open() {
            self.thread_snapshot(messages, reached_start)
        } else {
            messages
        };
        // Told apart with the room's members too, as any other list from the worker
        // (see `apply_timeline`).
        self.complete.mark_namesakes(&room_id, &mut messages);

        let mut timeline = TimelineState {
            reached_start,
            detached: detached.is_some(),
            reached_end: detached.as_ref().is_some_and(|d| d.reached_end),
            ..TimelineState::default()
        };
        timeline.selected = find_target_message(&messages, target_id, false, self.thread_open());
        timeline.reveal_selected = timeline.selected.is_some();

        // Capture any active reply or edit target by event id before replacing the slice
        // (cross-room or cross-thread switches above already reset `compose_mode`, and
        // a detached slice is a brand-new timeline with different local item ids).
        let compose_event = self.compose_target_event_id();

        // The room is the open one now, whatever was open (or not) before.
        self.rooms[room_index].messages = messages;
        self.remap_compose_mode_by_event(compose_event.as_deref());
        self.request_custom_reaction_emotes();
        self.follow_room_emotes();
        self.forget_notes_of_deleted();
        self.sync_pinned_flags(&room_id);
        self.sync_saved_flags(&room_id);
        self.timeline = timeline;
        self.timeline_rev += 1;
        self.repaint = true;
        self.set_focus(Pane::Timeline);
    }

    pub(super) fn on_jump_failed(&mut self, generation: u64, reason: String) {
        if self
            .pending_jump
            .as_ref()
            .is_some_and(|pending| pending.generation == generation)
        {
            self.pending_jump = None;
            self.show_toast(&reason);
        }
    }
}

pub(super) fn find_target_message(
    messages: &[Message],
    target: &str,
    demo_mode: bool,
    thread_open: bool,
) -> Option<usize> {
    if demo_mode {
        if thread_open {
            if let Some(reply_idx) = target
                .strip_prefix("demo:")
                .and_then(|rest| rest.split_once(':'))
                .and_then(|(_, r)| r.parse::<usize>().ok())
            {
                let pos = reply_idx + 1;
                return (pos < messages.len()).then_some(pos);
            }
        } else if let Some(idx) = target.strip_prefix("demo:").and_then(|s| s.parse().ok()) {
            return (idx < messages.len()).then_some(idx);
        }
    }
    messages.iter().position(|m| {
        matches!(&m.item, Some(ItemRef::Event(id)) if id == target)
            || (!m.id.is_empty() && m.id == target)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::matrix::MatrixHandle;
    use crossterm::event::{Event, KeyEvent};
    use tokio::sync::mpsc::error::TryRecvError;

    fn test_app_with_matrix() -> (
        App,
        tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>,
        crate::matrix::MatrixHandle,
    ) {
        let (handle, rx) = MatrixHandle::for_test();
        let mut app = App::new(
            &Config::default(),
            std::env::temp_dir(),
            Some(handle.clone()),
        );
        app.rooms = vec![
            crate::model::Room::new("!a:example.org", "alpha", RoomKind::Room),
            crate::model::Room::new("!b:example.org", "beta", RoomKind::Room),
        ];
        app.rooms[0].messages.push(Message {
            id: "$ev_a1".into(),
            item: Some(ItemRef::Event("$ev_a1".into())),
            sender: "alice".into(),
            day: "Today".into(),
            body: "first room hello".into(),
            ..Message::default()
        });
        app.rooms[1].messages.push(Message {
            id: "$ev_b1".into(),
            item: Some(ItemRef::Event("$ev_b1".into())),
            sender: "bob".into(),
            day: "Yesterday".into(),
            body: "second room target needle".into(),
            ..Message::default()
        });
        (app, rx, handle)
    }

    fn key_event(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    #[test]
    fn debounce_and_generation_cancel_stale_results() {
        let (mut app, mut rx, _handle) = test_app_with_matrix();
        app.on_event(key_event(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert!(matches!(app.modal, Some(Modal::GlobalSearch(_))));

        // Typing arms debounce without firing a command right away.
        app.on_event(key_event(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

        // Rewind pending_at past debounce so tick dispatches generation g1.
        if let Some(Modal::GlobalSearch(s)) = &mut app.modal {
            s.pending_at = Some(Instant::now() - SEARCH_DEBOUNCE - Duration::from_millis(10));
        }
        app.on_tick();
        let g1 = match rx.try_recv().unwrap() {
            MatrixCmd::SearchMessages { generation, query } => {
                assert_eq!(query, "r");
                generation
            }
            other => panic!("unexpected cmd: {other:?}"),
        };

        // Typing another char cancels g1 and starts a new debounce timer.
        app.on_event(key_event(KeyCode::Char('u'), KeyModifiers::NONE));
        assert!(matches!(rx.try_recv().unwrap(), MatrixCmd::CancelSearch));

        if let Some(Modal::GlobalSearch(s)) = &mut app.modal {
            s.pending_at = Some(Instant::now() - SEARCH_DEBOUNCE - Duration::from_millis(10));
        }
        app.on_tick();
        let g2 = match rx.try_recv().unwrap() {
            MatrixCmd::SearchMessages { generation, query } => {
                assert_eq!(query, "ru");
                generation
            }
            other => panic!("unexpected cmd: {other:?}"),
        };
        assert!(g2 > g1);

        // Late results from stale g1 must be ignored cleanly.
        app.on_matrix(crate::matrix::MatrixEvent::SearchResults {
            generation: g1,
            hits: vec![SearchHit {
                room_id: "!a:example.org".into(),
                event_id: "$stale".into(),
                thread_root: None,
                room_name: "alpha".into(),
                sender: "alice".into(),
                date: "Today".into(),
                snippet: "stale hit".into(),
            }],
            done: true,
            note: None,
        });
        let Some(Modal::GlobalSearch(s)) = &app.modal else {
            panic!("modal closed unexpectedly");
        };
        assert!(s.hits.is_empty());
        assert!(s.searching);

        // Results for active g2 stream in and complete cleanly.
        app.on_matrix(crate::matrix::MatrixEvent::SearchResults {
            generation: g2,
            hits: vec![SearchHit {
                room_id: "!b:example.org".into(),
                event_id: "$ev_b1".into(),
                thread_root: None,
                room_name: "beta".into(),
                sender: "bob".into(),
                date: "Yesterday".into(),
                snippet: "second room target needle".into(),
            }],
            done: true,
            note: None,
        });
        let Some(Modal::GlobalSearch(s)) = &app.modal else {
            panic!("modal closed unexpectedly");
        };
        assert_eq!(s.hits.len(), 1);
        assert!(!s.searching);

        // Selecting and hitting Enter closes modal, cancels search, and dispatches OpenEvent
        // while keeping the current room intact until the target timeline arrives.
        app.on_event(key_event(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.modal.is_none());
        assert_eq!(app.active, 0);
        assert!(app.pending_jump.is_some());
        let mut cmds = Vec::new();
        while let Ok(c) = rx.try_recv() {
            cmds.push(c);
        }
        assert!(cmds.iter().any(|c| matches!(c, MatrixCmd::CancelSearch)));
        let jump_gen = cmds
            .iter()
            .find_map(|c| match c {
                MatrixCmd::OpenEvent {
                    room_id,
                    event_id,
                    thread_root,
                    generation,
                } if room_id == "!b:example.org"
                    && event_id == "$ev_b1"
                    && thread_root.is_none() =>
                {
                    Some(*generation)
                }
                _ => None,
            })
            .expect("expected OpenEvent(!b, $ev_b1)");

        app.on_matrix(crate::matrix::MatrixEvent::Timeline {
            room_id: "!b:example.org".into(),
            generation: jump_gen,
            messages: vec![Message {
                id: "$ev_b1".into(),
                item: Some(ItemRef::Event("$ev_b1".into())),
                sender: "bob".into(),
                body: "second room target needle".into(),
                ..Message::default()
            }],
            reached_start: true,
            detached: None,
        });
        assert_eq!(app.active, 1);
        assert_eq!(app.focus, Pane::Timeline);
        assert_eq!(app.timeline.selected, Some(0));
        assert!(!app.timeline.detached);
        assert!(app.pending_jump.is_none());
    }

    #[test]
    fn jump_opens_detached_event_timeline_and_preserves_view_until_arrival() {
        let (mut app, mut rx, _handle) = test_app_with_matrix();
        app.rooms[1].messages.clear();

        app.perform(Action::SearchEverywhere);
        let generation = match &app.modal {
            Some(Modal::GlobalSearch(s)) => s.generation,
            _ => panic!("modal missing"),
        };
        app.on_matrix(crate::matrix::MatrixEvent::SearchResults {
            generation,
            hits: vec![SearchHit {
                room_id: "!b:example.org".into(),
                event_id: "$older_b".into(),
                thread_root: None,
                room_name: "beta".into(),
                sender: "bob".into(),
                date: "Tue 10 Nov".into(),
                snippet: "old target message".into(),
            }],
            done: true,
            note: None,
        });
        app.on_event(key_event(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.active, 0, "view stays put while OpenEvent is in flight");
        assert!(app.pending_jump.is_some());

        let mut open_gen = 0;
        while let Ok(cmd) = rx.try_recv() {
            if let MatrixCmd::OpenEvent {
                room_id,
                event_id,
                generation,
                ..
            } = cmd
            {
                assert_eq!(room_id, "!b:example.org");
                assert_eq!(event_id, "$older_b");
                open_gen = generation;
            }
        }
        assert!(open_gen > 0);

        // Worker opens the /context slice around $older_b.
        app.on_matrix(crate::matrix::MatrixEvent::Timeline {
            room_id: "!b:example.org".into(),
            generation: open_gen,
            messages: vec![
                Message {
                    id: "$older_b".into(),
                    item: Some(ItemRef::Event("$older_b".into())),
                    sender: "bob".into(),
                    body: "old target message".into(),
                    ..Message::default()
                },
                Message {
                    id: "$newer_b".into(),
                    item: Some(ItemRef::Event("$newer_b".into())),
                    sender: "bob".into(),
                    body: "newest".into(),
                    ..Message::default()
                },
            ],
            reached_start: false,
            detached: Some(DetachedTimeline {
                target_event: "$older_b".into(),
                thread_root: None,
                reached_end: false,
            }),
        });
        assert_eq!(app.active, 1);
        assert_eq!(app.generation, open_gen);
        assert_eq!(app.focus, Pane::Timeline);
        assert_eq!(app.timeline.selected, Some(0));
        assert!(app.timeline.detached);
        assert!(!app.timeline.reached_end);
        assert!(app.pending_jump.is_none());
    }

    /// A jump whose room went away while it was opening (the user was removed from it)
    /// is called off, and the worker is told: it had switched to that room already, and
    /// streams the view still shown again only on a cancel.
    #[test]
    fn a_jump_into_a_room_gone_meanwhile_is_cancelled_at_the_worker() {
        let (mut app, mut rx, _handle) = test_app_with_matrix();
        let shown = app.generation;
        app.jump_to_event("!b:example.org".into(), "$older_b".into(), None);
        let open_gen = std::iter::from_fn(|| rx.try_recv().ok())
            .find_map(|cmd| match cmd {
                MatrixCmd::OpenEvent { generation, .. } => Some(generation),
                _ => None,
            })
            .expect("OpenEvent sent");
        app.rooms.retain(|room| room.id != "!b:example.org");

        app.on_matrix(crate::matrix::MatrixEvent::Timeline {
            room_id: "!b:example.org".into(),
            generation: open_gen,
            messages: Vec::new(),
            reached_start: false,
            detached: Some(DetachedTimeline {
                target_event: "$older_b".into(),
                thread_root: None,
                reached_end: false,
            }),
        });
        assert!(app.pending_jump.is_none());
        assert_eq!(app.generation, shown);
        let cancels: Vec<u64> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|cmd| match cmd {
                MatrixCmd::CancelOpenEvent { generation } => Some(generation),
                _ => None,
            })
            .collect();
        assert_eq!(cancels, [open_gen]);
    }

    #[test]
    fn clearing_query_stops_active_search() {
        let (mut app, mut rx, _handle) = test_app_with_matrix();
        app.perform(Action::SearchEverywhere);
        app.on_event(key_event(KeyCode::Char('a'), KeyModifiers::NONE));
        if let Some(Modal::GlobalSearch(s)) = &mut app.modal {
            s.pending_at = Some(Instant::now() - SEARCH_DEBOUNCE - Duration::from_millis(1));
        }
        app.on_tick();
        assert!(matches!(
            rx.try_recv().unwrap(),
            MatrixCmd::SearchMessages { .. }
        ));

        app.on_event(key_event(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(matches!(rx.try_recv().unwrap(), MatrixCmd::CancelSearch));
        let Some(Modal::GlobalSearch(s)) = &app.modal else {
            panic!("modal closed");
        };
        assert!(s.active_query.is_empty());
        assert!(!s.searching);
        assert!(s.pending_at.is_none());
    }

    #[test]
    fn debug_redacts_query_and_note() {
        let mut s = GlobalSearch::new(7);
        s.query.set("hunter2_secret");
        s.active_query = "hunter2_secret".into();
        s.note = Some("hunter2_note".into());
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("hunter2_secret"));
        assert!(!dbg.contains("hunter2_note"));
    }

    #[test]
    fn toggle_backdrop_wheel_hit_click_and_demo_search() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;

        let (mut app, mut rx, _handle) = test_app_with_matrix();
        // Ctrl-G opens and Ctrl-G again toggles closed while sending CancelSearch.
        app.on_event(key_event(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert!(matches!(app.modal, Some(Modal::GlobalSearch(_))));
        app.on_event(key_event(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert!(app.modal.is_none());
        assert!(matches!(rx.try_recv().unwrap(), MatrixCmd::CancelSearch));

        // Backdrop click closes GlobalSearch cleanly.
        app.perform(Action::SearchEverywhere);
        app.hits.clear();
        app.hits
            .add(Rect::new(0, 0, 100, 30), crate::ui::hit::HitId::Backdrop);
        app.on_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(app.modal.is_none());

        // Wheel scrolls hits and clicking a HitId::GlobalSearch row activates & jumps.
        let mut demo = App::demo();
        demo.perform(Action::SearchEverywhere);
        let gen_id = match &demo.modal {
            Some(Modal::GlobalSearch(s)) => s.generation,
            _ => panic!("modal missing"),
        };
        let mut lots = Vec::new();
        for i in 0..20 {
            lots.push(SearchHit {
                room_id: demo.rooms[0].id.clone(),
                event_id: format!("$e{i}"),
                thread_root: None,
                room_name: demo.rooms[0].name.clone(),
                sender: "alice".into(),
                date: "Today".into(),
                snippet: format!("hit {i} needle"),
            });
        }
        demo.on_search_results(gen_id, lots, true, None);
        if let Some(Modal::GlobalSearch(s)) = &mut demo.modal {
            s.rows = 5;
        }
        demo.on_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 50,
            row: 15,
            modifiers: KeyModifiers::NONE,
        }));
        let scrolled = match &demo.modal {
            Some(Modal::GlobalSearch(s)) => s.scroll,
            _ => 0,
        };
        assert_eq!(scrolled, 2);

        // Clicking hit row 0 jumps to that hit in demo mode.
        demo.hits.clear();
        demo.hits.add(
            Rect::new(10, 10, 50, 2),
            crate::ui::hit::HitId::GlobalSearch(0),
        );
        demo.on_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 15,
            row: 10,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(demo.modal.is_none());
    }

    #[test]
    fn a_click_on_a_drawn_card_opens_its_hit_and_one_on_the_query_opens_none() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (mut app, mut rx, _handle) = test_app_with_matrix();
        // A search with three hits, all in the same room from the same sender.
        let search = |app: &mut App, rx: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>| {
            app.perform(Action::SearchEverywhere);
            let Some(Modal::GlobalSearch(search)) = &mut app.modal else {
                panic!("modal missing");
            };
            search.query.set("needle");
            let generation = search.generation;
            let hits = (0..3)
                .map(|n| SearchHit {
                    room_id: "!b:example.org".into(),
                    event_id: format!("$hit{n}"),
                    thread_root: None,
                    room_name: "beta".into(),
                    sender: "bob".into(),
                    date: "Today".into(),
                    snippet: format!("needle number {n}"),
                })
                .collect();
            app.on_search_results(generation, hits, true, None);
            while rx.try_recv().is_ok() {}
        };
        search(&mut app, &mut rx);

        // Clicks go where the last frame drew things, as in the app.
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut draw = |app: &mut App| {
            terminal.draw(|f| crate::ui::draw(f, app)).unwrap();
            format!("{}", terminal.backend())
        };
        let click = |app: &mut App, row: usize| {
            app.on_event(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 20,
                row: row as u16,
                modifiers: KeyModifiers::NONE,
            }));
        };
        let opened = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>| {
            let mut opened = Vec::new();
            while let Ok(command) = rx.try_recv() {
                if let MatrixCmd::OpenEvent { event_id, .. } = command {
                    opened.push(event_id);
                }
            }
            opened
        };
        let row_of = |screen: &str, needle: &str| {
            screen
                .lines()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} not on screen:\n{screen}"))
        };

        // The query field is the query field: no hit opens, the search stays.
        let screen = draw(&mut app);
        click(&mut app, row_of(&screen, "⌕ needle"));
        assert!(opened(&mut rx).is_empty());
        assert!(matches!(app.modal, Some(Modal::GlobalSearch(_))));

        // Either line of a card opens the hit drawn there: its text, and the line
        // above it with room, sender and date.
        let screen = draw(&mut app);
        let text = row_of(&screen, "needle number 1");
        assert_eq!(
            app.hits.at(20, text as u16),
            Some(crate::ui::hit::HitId::GlobalSearch(1))
        );
        click(&mut app, text);
        assert_eq!(opened(&mut rx), ["$hit1"]);
        assert!(app.modal.is_none());

        search(&mut app, &mut rx);
        let screen = draw(&mut app);
        let head = row_of(&screen, "needle number 2") - 1;
        assert!(
            screen.lines().nth(head).unwrap().contains("beta"),
            "{screen}"
        );
        click(&mut app, head);
        assert_eq!(opened(&mut rx), ["$hit2"]);
        assert!(app.modal.is_none());
    }

    #[test]
    fn demo_mode_searches_and_jumps_without_matrix() {
        let mut demo = App::demo();
        let target_room_idx = demo
            .rooms
            .iter()
            .position(|r| r.messages.len() >= 2 && r.kind != RoomKind::Invite)
            .expect("demo has a room with messages");
        // Switch away to a different room so jumping proves room switch + selection.
        let other = (target_room_idx + 1) % demo.rooms.len();
        demo.open_room(other);
        let needle = demo.rooms[target_room_idx].messages[0]
            .body
            .split_whitespace()
            .find(|w| w.len() >= 4 && w.chars().all(char::is_alphanumeric))
            .unwrap()
            .to_owned();

        // Can open directly from Command Palette as well.
        demo.perform(Action::OpenPalette);
        assert!(matches!(demo.modal, Some(Modal::Palette(_))));
        for c in "Search all rooms".chars() {
            demo.on_event(key_event(KeyCode::Char(c), KeyModifiers::NONE));
        }
        demo.on_event(key_event(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(demo.modal, Some(Modal::GlobalSearch(_))));
        for c in needle.chars() {
            demo.on_event(key_event(KeyCode::Char(c), KeyModifiers::NONE));
        }
        demo.on_event(key_event(KeyCode::Enter, KeyModifiers::NONE));
        let Some(Modal::GlobalSearch(s)) = &demo.modal else {
            panic!("expected results list after Enter on debounced query");
        };
        assert!(!s.hits.is_empty());
        assert_eq!(s.active_query, needle);
    }

    #[test]
    fn thread_reply_hit_opens_detached_thread_and_reuses_already_open_thread() {
        let (mut app, mut rx, _handle) = test_app_with_matrix();
        app.rooms[1].messages.clear();

        let make_msg = |id: &str, body: &str| Message {
            id: id.into(),
            item: Some(ItemRef::Event(id.into())),
            sender: "bob".into(),
            body: body.into(),
            ..Message::default()
        };

        // Trigger global search with a hit inside an older thread in Beta (!b:example.org).
        app.perform(Action::SearchEverywhere);
        let search_gen = match &app.modal {
            Some(Modal::GlobalSearch(s)) => s.generation,
            _ => panic!("modal missing"),
        };
        app.on_matrix(crate::matrix::MatrixEvent::SearchResults {
            generation: search_gen,
            hits: vec![
                SearchHit {
                    room_id: "!b:example.org".into(),
                    event_id: "$reply_old".into(),
                    thread_root: Some("$thread_root_b".into()),
                    room_name: "beta".into(),
                    sender: "bob".into(),
                    date: "Tue 10 Nov".into(),
                    snippet: "old thread reply".into(),
                },
                SearchHit {
                    room_id: "!b:example.org".into(),
                    event_id: "$reply_newer".into(),
                    thread_root: Some("$thread_root_b".into()),
                    room_name: "beta".into(),
                    sender: "bob".into(),
                    date: "Tue 10 Nov".into(),
                    snippet: "newer thread reply".into(),
                },
            ],
            done: true,
            note: None,
        });
        app.on_event(key_event(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.active, 0);
        assert!(!app.thread_open());
        assert!(app.pending_jump.is_some());

        let mut open_event_gen = 0;
        while let Ok(cmd) = rx.try_recv() {
            if let MatrixCmd::OpenEvent {
                room_id,
                event_id,
                thread_root,
                generation,
            } = cmd
            {
                assert_eq!(room_id, "!b:example.org");
                assert_eq!(event_id, "$reply_old");
                assert_eq!(thread_root.as_deref(), Some("$thread_root_b"));
                open_event_gen = generation;
            }
        }
        assert!(open_event_gen > 0);

        // Detached thread slice arrives without the root yet: must open the thread cleanly
        // without inserting a blank root placeholder.
        app.on_matrix(crate::matrix::MatrixEvent::Timeline {
            room_id: "!b:example.org".into(),
            generation: open_event_gen,
            messages: vec![
                make_msg("$reply_old", "old thread reply"),
                make_msg("$reply_newer", "newer thread reply"),
            ],
            reached_start: false,
            detached: Some(DetachedTimeline {
                target_event: "$reply_old".into(),
                thread_root: Some("$thread_root_b".into()),
                reached_end: true,
            }),
        });
        assert_eq!(app.active, 1);
        assert_eq!(app.focus, Pane::Timeline);
        assert!(app.thread_open());
        assert_eq!(app.thread_key(), Some("$thread_root_b"));
        assert!(app.timeline.detached);
        assert!(app.timeline.reached_end);
        assert_eq!(app.messages().len(), 2);
        assert_eq!(
            app.timeline
                .selected
                .and_then(|i| app.messages()[i].item.as_ref()),
            Some(&ItemRef::Event("$reply_old".into()))
        );
        assert!(app.pending_jump.is_none());

        // Jumping to another reply in the SAME already-open thread reuses it directly.
        app.perform(Action::SearchEverywhere);
        let search_gen2 = match &app.modal {
            Some(Modal::GlobalSearch(s)) => s.generation,
            _ => panic!("modal missing"),
        };
        app.on_matrix(crate::matrix::MatrixEvent::SearchResults {
            generation: search_gen2,
            hits: vec![SearchHit {
                room_id: "!b:example.org".into(),
                event_id: "$reply_newer".into(),
                thread_root: Some("$thread_root_b".into()),
                room_name: "beta".into(),
                sender: "bob".into(),
                date: "Tue 10 Nov".into(),
                snippet: "newer thread reply".into(),
            }],
            done: true,
            note: None,
        });
        while rx.try_recv().is_ok() {}
        app.on_event(key_event(KeyCode::Enter, KeyModifiers::NONE));
        let mut cmds = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            cmds.push(cmd);
        }
        assert!(
            !cmds.iter().any(|c| matches!(
                c,
                MatrixCmd::OpenRoom { .. }
                    | MatrixCmd::OpenThread { .. }
                    | MatrixCmd::OpenEvent { .. }
            )),
            "already-open target thread should not reopen: {cmds:?}"
        );
        assert_eq!(
            app.timeline
                .selected
                .and_then(|i| app.messages()[i].item.as_ref()),
            Some(&ItemRef::Event("$reply_newer".into()))
        );
    }

    #[test]
    fn pending_jump_cancelled_on_selection_typing_scroll_and_popup_while_opening() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let start_opening =
            |app: &mut App, rx: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>| {
                app.rooms[0].messages = vec![
                    Message {
                        id: "$visible_0".into(),
                        item: Some(ItemRef::Event("$visible_0".into())),
                        sender: "alice".into(),
                        body: "zero".into(),
                        ..Message::default()
                    },
                    Message {
                        id: "$visible_1".into(),
                        item: Some(ItemRef::Event("$visible_1".into())),
                        sender: "alice".into(),
                        body: "one".into(),
                        ..Message::default()
                    },
                ];
                app.perform(Action::SearchEverywhere);
                let generation = match &app.modal {
                    Some(Modal::GlobalSearch(s)) => s.generation,
                    _ => panic!("modal missing"),
                };
                app.on_matrix(crate::matrix::MatrixEvent::SearchResults {
                    generation,
                    hits: vec![SearchHit {
                        room_id: "!b:example.org".into(),
                        event_id: "$late_target".into(),
                        thread_root: None,
                        room_name: "beta".into(),
                        sender: "bob".into(),
                        date: "Today".into(),
                        snippet: "late target".into(),
                    }],
                    done: true,
                    note: None,
                });
                app.on_event(key_event(KeyCode::Enter, KeyModifiers::NONE));
                let mut open_gen = 0;
                while let Ok(cmd) = rx.try_recv() {
                    if let MatrixCmd::OpenEvent { generation, .. } = cmd {
                        open_gen = generation;
                    }
                }
                assert!(app.pending_jump.is_some());
                open_gen
            };

        let deliver_late_open = |app: &mut App, open_gen: u64| {
            app.on_matrix(crate::matrix::MatrixEvent::Timeline {
                room_id: "!b:example.org".into(),
                generation: open_gen,
                messages: vec![Message {
                    id: "$late_target".into(),
                    item: Some(ItemRef::Event("$late_target".into())),
                    sender: "bob".into(),
                    body: "late target".into(),
                    ..Message::default()
                }],
                reached_start: true,
                detached: Some(DetachedTimeline {
                    target_event: "$late_target".into(),
                    thread_root: None,
                    reached_end: true,
                }),
            });
        };

        let has_cancel = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>,
                          expected_gen: u64| {
            let mut found = false;
            while let Ok(cmd) = rx.try_recv() {
                if matches!(cmd, MatrixCmd::CancelOpenEvent { generation } if generation == expected_gen)
                {
                    found = true;
                }
            }
            found
        };

        // 1. User moves selection via mouse click on a message while opening.
        {
            let (mut app, mut rx, _h) = test_app_with_matrix();
            let open_gen = start_opening(&mut app, &mut rx);
            app.hits.clear();
            app.hits.add(
                ratatui::layout::Rect::new(25, 5, 40, 1),
                crate::ui::hit::HitId::Message(0),
            );
            app.on_event(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 30,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }));
            assert!(app.pending_jump.is_none());
            assert!(has_cancel(&mut rx, open_gen));
            deliver_late_open(&mut app, open_gen);
            assert_eq!(app.active, 0);
            assert_eq!(app.timeline.selected, Some(0));
        }

        // 2. User types in the composer while opening.
        {
            let (mut app, mut rx, _h) = test_app_with_matrix();
            let open_gen = start_opening(&mut app, &mut rx);
            app.set_focus(Pane::Composer);
            app.on_event(key_event(KeyCode::Char('h'), KeyModifiers::NONE));
            assert!(app.pending_jump.is_none());
            assert!(has_cancel(&mut rx, open_gen));
            deliver_late_open(&mut app, open_gen);
            assert_eq!(app.active, 0);
            assert_eq!(app.focus, Pane::Composer);
            assert!(app.timeline.selected.is_none());
        }

        // 3. User scrolls the timeline with the mouse wheel while opening.
        {
            let (mut app, mut rx, _h) = test_app_with_matrix();
            let open_gen = start_opening(&mut app, &mut rx);
            app.hits.add(
                ratatui::layout::Rect::new(20, 0, 60, 20),
                crate::ui::hit::HitId::Pane(Pane::Timeline),
            );
            app.on_event(Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 30,
                row: 10,
                modifiers: KeyModifiers::NONE,
            }));
            assert!(app.pending_jump.is_none());
            assert!(has_cancel(&mut rx, open_gen));
            deliver_late_open(&mut app, open_gen);
            assert_eq!(app.active, 0);
            assert!(app.timeline.selected.is_none());
        }

        // 4. User opens another popup while opening.
        {
            let (mut app, mut rx, _h) = test_app_with_matrix();
            let open_gen = start_opening(&mut app, &mut rx);
            app.perform(Action::OpenSettings);
            assert!(app.pending_jump.is_none());
            assert!(has_cancel(&mut rx, open_gen));
            assert!(matches!(app.modal, Some(Modal::Settings(_))));
            deliver_late_open(&mut app, open_gen);
            assert_eq!(app.active, 0);
            assert!(app.timeline.selected.is_none());
        }

        // 5. User moves selection via key action (SelectOlder) while opening.
        {
            let (mut app, mut rx, _h) = test_app_with_matrix();
            let open_gen = start_opening(&mut app, &mut rx);
            app.perform(Action::SelectOlder);
            assert!(app.pending_jump.is_none());
            assert!(has_cancel(&mut rx, open_gen));
            assert_eq!(app.timeline.selected, Some(1));
            deliver_late_open(&mut app, open_gen);
            assert_eq!(app.active, 0);
            assert_eq!(app.timeline.selected, Some(1));
        }

        // 6. Demo mode finds a thread reply and jumps into that demo thread.
        let mut demo = App::demo();
        let (thread_room, thread_idx, thread_reply) = demo
            .rooms
            .iter()
            .enumerate()
            .find_map(|(ri, room)| {
                room.messages.iter().enumerate().find_map(|(mi, msg)| {
                    msg.thread
                        .as_ref()
                        .and_then(|t| t.demo_replies.first())
                        .map(|r| (ri, mi, r.body.clone()))
                })
            })
            .expect("demo has a thread reply");
        let needle = thread_reply
            .split_whitespace()
            .find(|w| w.len() >= 5 && w.chars().all(char::is_alphanumeric))
            .unwrap()
            .to_owned();
        let other = (thread_room + 1) % demo.rooms.len();
        demo.open_room(other);
        demo.perform(Action::SearchEverywhere);
        for c in needle.chars() {
            demo.on_event(key_event(KeyCode::Char(c), KeyModifiers::NONE));
        }
        demo.on_event(key_event(KeyCode::Enter, KeyModifiers::NONE));
        let Some(Modal::GlobalSearch(s)) = &mut demo.modal else {
            panic!("expected demo global search results");
        };
        let target_hit_idx = s
            .hits
            .iter()
            .position(|h| {
                h.room_id == demo.rooms[thread_room].id
                    && h.thread_root.as_deref() == Some(&format!("demo:{thread_idx}"))
            })
            .expect("demo global search should find the thread reply");
        s.selected = target_hit_idx;
        demo.on_event(key_event(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(demo.active, thread_room);
        assert!(demo.thread_open());
        assert_eq!(demo.timeline.selected, Some(1));
    }

    /// A slice jumped to is told apart as the worker's other lists are: with the
    /// room's members held, a line about someone who wrote nothing there but goes by
    /// a member's name names them with the ID.
    #[test]
    fn a_jump_tells_apart_whom_a_line_is_about_from_a_member_of_the_same_name() {
        let (mut app, _rx, _h) = test_app_with_matrix();
        app.on_matrix(crate::matrix::MatrixEvent::RoomMembers {
            room_id: "!a:example.org".into(),
            members: vec![crate::complete::Member {
                user_id: "@bob:example.org".into(),
                display_name: "bob".into(),
            }],
            can_mention_room: false,
        });
        app.pending_jump = Some(PendingJump {
            generation: 9,
            room_id: "!a:example.org".into(),
            event_id: "$ban".into(),
            thread_root: None,
        });
        app.generation = 9;
        app.on_jump_arrived(
            9,
            "!a:example.org".into(),
            vec![Message {
                id: "$ban".into(),
                item: Some(ItemRef::Event("$ban".into())),
                kind: MsgKind::System,
                sender: "carol".into(),
                sender_id: Some("@carol:example.org".into()),
                body: "bob was banned by carol".into(),
                subject: Some(("bob".into(), "@bob:elsewhere.org".into())),
                ..Message::default()
            }],
            false,
            Some(DetachedTimeline {
                target_event: "$ban".into(),
                reached_end: false,
                thread_root: None,
            }),
        );
        assert_eq!(app.rooms[0].id, "!a:example.org");
        assert_eq!(
            app.rooms[0].messages[0].body,
            "bob (@bob:elsewhere.org) was banned by carol"
        );
    }

    #[test]
    fn same_room_jump_remaps_or_cancels_compose_mode_and_syncs_emotes_and_notes() {
        use crate::app::ComposeMode;

        // 1. Same-room jump to a slice missing the message being edited cancels the edit
        //    and restores the stashed draft instead of editing an unrelated index.
        {
            let (mut app, _rx, _h) = test_app_with_matrix();
            app.rooms[0].messages.push(Message {
                id: "$ev_a2".into(),
                item: Some(ItemRef::Event("$ev_a2".into())),
                sender: "me".into(),
                own: true,
                body: "my message".into(),
                ..Message::default()
            });
            app.composer.set_text("unsent draft");
            app.begin_edit(1, "my message");
            assert_eq!(app.compose_mode, ComposeMode::Edit(1));

            app.pending_jump = Some(PendingJump {
                generation: 9,
                room_id: "!a:example.org".into(),
                event_id: "$ev_old".into(),
                thread_root: None,
            });
            app.generation = 9;
            app.on_jump_arrived(
                9,
                "!a:example.org".into(),
                vec![
                    Message {
                        id: "$ev_older".into(),
                        item: Some(ItemRef::Event("$ev_older".into())),
                        sender: "bob".into(),
                        body: "earlier".into(),
                        ..Message::default()
                    },
                    Message {
                        id: "$ev_old".into(),
                        item: Some(ItemRef::Event("$ev_old".into())),
                        sender: "bob".into(),
                        body: "different message at index 1".into(),
                        ..Message::default()
                    },
                ],
                false,
                Some(DetachedTimeline {
                    target_event: "$ev_old".into(),
                    reached_end: false,
                    thread_root: None,
                }),
            );
            assert_eq!(app.compose_mode, ComposeMode::Normal);
            assert_eq!(app.composer.text(), "unsent draft");
        }

        // 2. Same-room jump where the replied-to message moved to a new index remaps it.
        {
            let (mut app, _rx, _h) = test_app_with_matrix();
            app.compose_mode = ComposeMode::Reply(0);
            app.pending_jump = Some(PendingJump {
                generation: 10,
                room_id: "!a:example.org".into(),
                event_id: "$ev_before".into(),
                thread_root: None,
            });
            app.generation = 10;
            app.on_jump_arrived(
                10,
                "!a:example.org".into(),
                vec![
                    Message {
                        id: "$ev_before".into(),
                        item: Some(ItemRef::Event("$ev_before".into())),
                        sender: "bob".into(),
                        body: "before".into(),
                        ..Message::default()
                    },
                    Message {
                        id: "$ev_a1".into(),
                        item: Some(ItemRef::Event("$ev_a1".into())),
                        sender: "alice".into(),
                        body: "first room hello".into(),
                        ..Message::default()
                    },
                ],
                false,
                Some(DetachedTimeline {
                    target_event: "$ev_before".into(),
                    reached_end: true,
                    thread_root: None,
                }),
            );
            assert_eq!(app.compose_mode, ComposeMode::Reply(1));
        }

        // 3. Jump across rooms follows emote packs and drops notes of redacted messages.
        {
            let (mut app, mut rx, _h) = test_app_with_matrix();
            app.want_emotes();
            while rx.try_recv().is_ok() {}
            app.notes.notes.set(
                "$ev_del",
                crate::notes::NoteKind::Translation,
                crate::notes::NoteState::Done("cached".into()),
            );

            app.pending_jump = Some(PendingJump {
                generation: 11,
                room_id: "!b:example.org".into(),
                event_id: "$ev_del".into(),
                thread_root: None,
            });
            app.generation = 11;
            app.on_jump_arrived(
                11,
                "!b:example.org".into(),
                vec![Message {
                    id: "$ev_del".into(),
                    item: Some(ItemRef::Event("$ev_del".into())),
                    kind: crate::model::MsgKind::Redacted,
                    ..Message::default()
                }],
                true,
                Some(DetachedTimeline {
                    target_event: "$ev_del".into(),
                    reached_end: true,
                    thread_root: None,
                }),
            );
            let mut asked_emotes = false;
            while let Ok(cmd) = rx.try_recv() {
                if matches!(
                    cmd,
                    MatrixCmd::EnsureEmotePacks {
                        room_id: Some(ref id)
                    } if id == "!b:example.org"
                ) {
                    asked_emotes = true;
                }
            }
            assert!(
                asked_emotes,
                "jump to another room should follow emote packs"
            );
            assert!(
                app.notes
                    .notes
                    .get("$ev_del", crate::notes::NoteKind::Translation)
                    .is_none(),
                "jump should forget notes of redacted messages"
            );
        }
    }

    #[test]
    fn same_room_jump_remaps_compose_by_event_id_when_slice_ids_collide() {
        use crate::app::ComposeMode;

        // 1. Edit target has local timeline id "1" ($ev_mine); jump slice also starts at
        //    id "0", "1" where "1" is an unrelated own message ($ev_other_own). Must cancel
        //    the edit instead of silently editing the unrelated message.
        {
            let (mut app, _rx, _h) = test_app_with_matrix();
            app.rooms[0].messages = vec![
                Message {
                    id: "0".into(),
                    item: Some(ItemRef::Event("$ev_a1".into())),
                    sender: "alice".into(),
                    body: "hello".into(),
                    ..Message::default()
                },
                Message {
                    id: "1".into(),
                    item: Some(ItemRef::Event("$ev_mine".into())),
                    sender: "me".into(),
                    own: true,
                    body: "original".into(),
                    ..Message::default()
                },
            ];
            app.composer.set_text("unsent draft");
            app.begin_edit(1, "original");
            assert_eq!(app.compose_mode, ComposeMode::Edit(1));

            app.pending_jump = Some(PendingJump {
                generation: 12,
                room_id: "!a:example.org".into(),
                event_id: "$ev_other_own".into(),
                thread_root: None,
            });
            app.generation = 12;
            app.on_jump_arrived(
                12,
                "!a:example.org".into(),
                vec![
                    Message {
                        id: "0".into(),
                        item: Some(ItemRef::Event("$ev_older".into())),
                        sender: "bob".into(),
                        body: "older".into(),
                        ..Message::default()
                    },
                    Message {
                        id: "1".into(),
                        item: Some(ItemRef::Event("$ev_other_own".into())),
                        sender: "me".into(),
                        own: true,
                        body: "unrelated own message".into(),
                        ..Message::default()
                    },
                ],
                false,
                Some(DetachedTimeline {
                    target_event: "$ev_other_own".into(),
                    reached_end: false,
                    thread_root: None,
                }),
            );
            assert_eq!(app.compose_mode, ComposeMode::Normal);
            assert_eq!(app.composer.text(), "unsent draft");
        }

        // 2. Reply target is $ev_target (old id "0"); jump slice has an unrelated event
        //    at id "0" and $ev_target at index 1 with a new timeline id "t2-1". Must remap
        //    by event id to index 1, not stay at index 0.
        {
            let (mut app, _rx, _h) = test_app_with_matrix();
            app.rooms[0].messages = vec![Message {
                id: "0".into(),
                item: Some(ItemRef::Event("$ev_target".into())),
                sender: "alice".into(),
                body: "target".into(),
                ..Message::default()
            }];
            app.compose_mode = ComposeMode::Reply(0);
            app.pending_jump = Some(PendingJump {
                generation: 13,
                room_id: "!a:example.org".into(),
                event_id: "$ev_other".into(),
                thread_root: None,
            });
            app.generation = 13;
            app.on_jump_arrived(
                13,
                "!a:example.org".into(),
                vec![
                    Message {
                        id: "0".into(),
                        item: Some(ItemRef::Event("$ev_other".into())),
                        sender: "bob".into(),
                        body: "colliding id".into(),
                        ..Message::default()
                    },
                    Message {
                        id: "t2-1".into(),
                        item: Some(ItemRef::Event("$ev_target".into())),
                        sender: "alice".into(),
                        body: "target".into(),
                        ..Message::default()
                    },
                ],
                false,
                Some(DetachedTimeline {
                    target_event: "$ev_other".into(),
                    reached_end: true,
                    thread_root: None,
                }),
            );
            assert_eq!(app.compose_mode, ComposeMode::Reply(1));
        }

        // 3. Target with no event id (local echo) cannot be matched across timelines and
        //    is cancelled even if the new slice reuses the same local id.
        {
            let (mut app, _rx, _h) = test_app_with_matrix();
            app.rooms[0].messages = vec![Message {
                id: "0".into(),
                item: Some(ItemRef::Transaction("txn1".into())),
                sender: "me".into(),
                own: true,
                body: "sending".into(),
                ..Message::default()
            }];
            app.compose_mode = ComposeMode::Reply(0);
            app.pending_jump = Some(PendingJump {
                generation: 14,
                room_id: "!a:example.org".into(),
                event_id: "$ev_remote".into(),
                thread_root: None,
            });
            app.generation = 14;
            app.on_jump_arrived(
                14,
                "!a:example.org".into(),
                vec![Message {
                    id: "0".into(),
                    item: Some(ItemRef::Event("$ev_remote".into())),
                    sender: "bob".into(),
                    body: "remote".into(),
                    ..Message::default()
                }],
                true,
                Some(DetachedTimeline {
                    target_event: "$ev_remote".into(),
                    reached_end: true,
                    thread_root: None,
                }),
            );
            assert_eq!(app.compose_mode, ComposeMode::Normal);
        }
    }
}
