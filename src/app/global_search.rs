//! Global search across loaded messages in all joined rooms (`Ctrl-G`).

use std::fmt;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::App;
use crate::action::{Action, Pane};
use crate::keymap::Context;
use crate::lineedit::{EditKeys, LineEdit};
use crate::matrix::search::{query_terms, window_snippet};
use crate::matrix::{MatrixCmd, SearchHit};
use crate::modal::Modal;
use crate::model::{ItemRef, Message, MsgKind, RoomKind};

/// Pause after typing before sending a search query to the index worker.
pub(crate) const SEARCH_DEBOUNCE: Duration = Duration::from_millis(200);
/// Maximum backward-pagination rounds when jumping to an older hit on a room switch.
const MAX_JUMP_DIG: u8 = 15;
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

#[derive(Debug, Clone)]
pub(super) struct PendingJump {
    pub room_id: String,
    pub event_id: String,
    pub thread_root: Option<String>,
    pub paginate_rounds: u8,
    /// Wait for the fresh `MatrixEvent::Timeline` from `OpenRoom` / `OpenThread` before
    /// digging or giving up.
    pub awaiting_timeline: bool,
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
            if room.kind == RoomKind::Invite {
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

        let Some(room_index) = self.rooms.iter().position(|r| r.id == hit.room_id) else {
            self.show_toast("That room is no longer available");
            return;
        };

        let in_target_thread = hit.thread_root.is_some()
            && room_index == self.active
            && self.thread_key() == hit.thread_root.as_deref();
        let in_target_room = room_index == self.active && !self.thread_open();
        if !in_target_thread && !in_target_room {
            // If the hit belongs to a room outside the active space filter, show all rooms
            // so the jumped-to conversation stays visible in the sidebar.
            if self.space.is_some() && !self.space_rooms.contains(&hit.room_id) {
                self.select_space(None);
            }
            self.open_room(room_index);
            if let Some(at) = self.visible_rooms().iter().position(|i| *i == room_index) {
                self.sidebar.cursor = at;
                self.sidebar.reveal_cursor = true;
            }
        }

        let awaiting_timeline = !in_target_thread && !in_target_room && self.matrix.is_some();
        self.pending_jump = Some(PendingJump {
            room_id: hit.room_id,
            event_id: hit.event_id,
            thread_root: hit.thread_root,
            paginate_rounds: MAX_JUMP_DIG,
            awaiting_timeline,
        });
        self.try_apply_pending_jump();
    }

    pub(super) fn on_jump_timeline(&mut self) {
        if let Some(jump) = &mut self.pending_jump {
            jump.awaiting_timeline = false;
        }
        self.try_apply_pending_jump();
    }

    pub(super) fn try_apply_pending_jump(&mut self) {
        let Some(jump) = self.pending_jump.clone() else {
            return;
        };
        if self.room_id().as_deref() != Some(jump.room_id.as_str()) {
            self.pending_jump = None;
            return;
        }

        let demo_mode = self.matrix.is_none();
        if let Some(root) = jump.thread_root.clone() {
            match self.thread_key() {
                Some(open_key) if open_key == root => {
                    self.try_select_or_paginate(jump, demo_mode, true);
                }
                Some(_) => {
                    self.pending_jump = None;
                }
                None => {
                    if jump.awaiting_timeline {
                        return;
                    }
                    if let Some(root_pos) =
                        find_target_message(self.messages(), &root, demo_mode, false)
                    {
                        self.open_thread(root_pos);
                        if self.thread_key() == Some(root.as_str()) {
                            self.pending_jump = Some(PendingJump {
                                room_id: jump.room_id,
                                event_id: jump.event_id,
                                thread_root: Some(root),
                                paginate_rounds: MAX_JUMP_DIG,
                                awaiting_timeline: self.matrix.is_some(),
                            });
                            if demo_mode {
                                self.try_apply_pending_jump();
                            }
                        } else {
                            self.pending_jump = None;
                        }
                        return;
                    }
                    self.paginate_for_jump(jump);
                }
            }
            return;
        }

        if self.thread_open() {
            self.pending_jump = None;
            return;
        }
        self.try_select_or_paginate(jump, demo_mode, false);
    }

    fn try_select_or_paginate(&mut self, jump: PendingJump, demo_mode: bool, thread_open: bool) {
        if let Some(pos) =
            find_target_message(self.messages(), &jump.event_id, demo_mode, thread_open)
        {
            self.set_focus(Pane::Timeline);
            self.timeline.selected = Some(pos);
            self.timeline.reveal_selected = true;
            if !jump.awaiting_timeline {
                self.pending_jump = None;
            }
            return;
        }

        if jump.awaiting_timeline {
            return;
        }
        self.paginate_for_jump(jump);
    }

    fn paginate_for_jump(&mut self, jump: PendingJump) {
        if self.timeline.loading_older {
            return;
        }
        if self.timeline.reached_start || jump.paginate_rounds == 0 {
            self.pending_jump = None;
            self.show_toast("Message is not in the loaded timeline");
            return;
        }

        if let Some(pending) = &mut self.pending_jump {
            pending.paginate_rounds -= 1;
        }
        self.timeline.loading_older = true;
        self.timeline.count_at_request = self.messages().len();
        self.send_matrix(MatrixCmd::Paginate {
            room_id: jump.room_id,
            generation: self.generation,
        });
    }
}

fn find_target_message(
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

        // Selecting and hitting Enter closes modal, cancels, opens room beta, selects $ev_b1.
        app.on_event(key_event(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.modal.is_none());
        assert_eq!(app.active, 1);
        assert_eq!(app.focus, Pane::Timeline);
        assert_eq!(app.timeline.selected, Some(0));
        let mut cmds = Vec::new();
        while let Ok(c) = rx.try_recv() {
            cmds.push(c);
        }
        assert!(cmds.iter().any(|c| matches!(c, MatrixCmd::CancelSearch)));
        assert!(cmds.iter().any(|c| matches!(
            c,
            MatrixCmd::OpenRoom { room_id, .. } if room_id == "!b:example.org"
        )));
    }

    #[test]
    fn jump_waits_for_async_timeline_and_paginates_if_older() {
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
        assert_eq!(app.active, 1);
        assert!(app.pending_jump.is_some());

        // Drain initial CancelSearch + OpenRoom.
        let mut open_gen = 0;
        while let Ok(cmd) = rx.try_recv() {
            if let MatrixCmd::OpenRoom { generation, .. } = cmd {
                open_gen = generation;
            }
        }
        assert!(open_gen > 0);

        // Initial timeline arrives WITHOUT $older_b yet -> triggers Paginate.
        app.on_matrix(crate::matrix::MatrixEvent::Timeline {
            room_id: "!b:example.org".into(),
            generation: open_gen,
            messages: vec![Message {
                id: "$newer_b".into(),
                item: Some(ItemRef::Event("$newer_b".into())),
                sender: "bob".into(),
                body: "newest".into(),
                ..Message::default()
            }],
            reached_start: false,
        });
        assert!(matches!(
            rx.try_recv().unwrap(),
            MatrixCmd::Paginate { generation, .. } if generation == open_gen
        ));

        // Back-paginated page delivers $older_b -> selected and focused!
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
        });
        app.on_matrix(crate::matrix::MatrixEvent::Paginated {
            generation: open_gen,
            reached_start: false,
        });
        assert_eq!(app.focus, Pane::Timeline);
        assert_eq!(app.timeline.selected, Some(0));
        assert!(app.pending_jump.is_none());
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
    fn thread_reply_hit_opens_room_digs_for_root_opens_thread_and_selects_reply() {
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
        assert_eq!(app.active, 1);
        assert!(!app.thread_open());
        assert!(app.pending_jump.is_some());

        let mut open_room_gen = 0;
        while let Ok(cmd) = rx.try_recv() {
            if let MatrixCmd::OpenRoom { generation, .. } = cmd {
                open_room_gen = generation;
            }
        }
        assert!(open_room_gen > 0);

        // Initial room timeline does not yet contain $thread_root_b -> triggers room Paginate.
        app.on_matrix(crate::matrix::MatrixEvent::Timeline {
            room_id: "!b:example.org".into(),
            generation: open_room_gen,
            messages: vec![make_msg("$latest_b", "latest room msg")],
            reached_start: false,
        });
        assert!(!app.thread_open());
        let mut cmds = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            cmds.push(cmd);
        }
        assert!(cmds.iter().any(|c| matches!(
            c,
            MatrixCmd::Paginate { room_id, generation }
                if room_id == "!b:example.org" && *generation == open_room_gen
        )));

        // Room pagination brings $thread_root_b -> opens thread and sends OpenThread.
        app.on_matrix(crate::matrix::MatrixEvent::Timeline {
            room_id: "!b:example.org".into(),
            generation: open_room_gen,
            messages: vec![
                make_msg("$thread_root_b", "thread starter"),
                make_msg("$latest_b", "latest room msg"),
            ],
            reached_start: false,
        });
        app.on_matrix(crate::matrix::MatrixEvent::Paginated {
            generation: open_room_gen,
            reached_start: false,
        });
        assert!(app.thread_open());
        assert_eq!(app.thread_key(), Some("$thread_root_b"));

        let mut thread_gen = 0;
        while let Ok(cmd) = rx.try_recv() {
            if let MatrixCmd::OpenThread {
                room_id,
                root,
                generation,
            } = cmd
            {
                assert_eq!(room_id, "!b:example.org");
                assert_eq!(root, "$thread_root_b");
                thread_gen = generation;
            }
        }
        assert!(thread_gen > open_room_gen);

        // Initial thread timeline arrives without $reply_old yet -> paginates INSIDE the thread!
        app.on_matrix(crate::matrix::MatrixEvent::Timeline {
            room_id: "!b:example.org".into(),
            generation: thread_gen,
            messages: vec![make_msg("$reply_newer", "newer thread reply")],
            reached_start: false,
        });
        assert!(app.pending_jump.is_some());
        let mut cmds = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            cmds.push(cmd);
        }
        assert!(cmds.iter().any(|c| matches!(
            c,
            MatrixCmd::InThread { root, command }
                if root == "$thread_root_b"
                    && matches!(**command, MatrixCmd::Paginate { ref room_id, generation } if room_id == "!b:example.org" && generation == thread_gen)
        )), "expected InThread Paginate, got {cmds:?}");

        // Thread pagination brings $reply_old -> selects it inside the open thread!
        app.on_matrix(crate::matrix::MatrixEvent::Timeline {
            room_id: "!b:example.org".into(),
            generation: thread_gen,
            messages: vec![
                make_msg("$thread_root_b", "thread starter"),
                make_msg("$reply_old", "old thread reply"),
                make_msg("$reply_newer", "newer thread reply"),
            ],
            reached_start: true,
        });
        assert_eq!(app.focus, Pane::Timeline);
        assert!(app.thread_open());
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
            !cmds
                .iter()
                .any(|c| matches!(c, MatrixCmd::OpenRoom { .. } | MatrixCmd::OpenThread { .. })),
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
    fn pending_jump_dropped_on_selection_typing_scroll_and_popup_while_digging() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let start_digging =
            |app: &mut App, rx: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>| {
                app.rooms[1].messages = vec![
                    Message {
                        id: "$visible_0".into(),
                        item: Some(ItemRef::Event("$visible_0".into())),
                        sender: "bob".into(),
                        body: "zero".into(),
                        ..Message::default()
                    },
                    Message {
                        id: "$visible_1".into(),
                        item: Some(ItemRef::Event("$visible_1".into())),
                        sender: "bob".into(),
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
                    if let MatrixCmd::OpenRoom { generation, .. } = cmd {
                        open_gen = generation;
                    }
                }
                app.on_matrix(crate::matrix::MatrixEvent::Timeline {
                    room_id: "!b:example.org".into(),
                    generation: open_gen,
                    messages: app.rooms[1].messages.clone(),
                    reached_start: false,
                });
                assert!(app.pending_jump.is_some());
                open_gen
            };

        let deliver_late_page = |app: &mut App, open_gen: u64| {
            app.on_matrix(crate::matrix::MatrixEvent::Timeline {
                room_id: "!b:example.org".into(),
                generation: open_gen,
                messages: vec![
                    Message {
                        id: "$late_target".into(),
                        item: Some(ItemRef::Event("$late_target".into())),
                        sender: "bob".into(),
                        body: "late target".into(),
                        ..Message::default()
                    },
                    Message {
                        id: "$visible_0".into(),
                        item: Some(ItemRef::Event("$visible_0".into())),
                        sender: "bob".into(),
                        body: "zero".into(),
                        ..Message::default()
                    },
                    Message {
                        id: "$visible_1".into(),
                        item: Some(ItemRef::Event("$visible_1".into())),
                        sender: "bob".into(),
                        body: "one".into(),
                        ..Message::default()
                    },
                ],
                reached_start: true,
            });
            app.on_matrix(crate::matrix::MatrixEvent::Paginated {
                generation: open_gen,
                reached_start: true,
            });
        };

        // 1. User moves selection via mouse click on a message while digging.
        {
            let (mut app, mut rx, _h) = test_app_with_matrix();
            let open_gen = start_digging(&mut app, &mut rx);
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
            deliver_late_page(&mut app, open_gen);
            assert_eq!(app.timeline.selected, Some(1));
        }

        // 2. User types in the composer while digging.
        {
            let (mut app, mut rx, _h) = test_app_with_matrix();
            let open_gen = start_digging(&mut app, &mut rx);
            app.set_focus(Pane::Composer);
            app.on_event(key_event(KeyCode::Char('h'), KeyModifiers::NONE));
            assert!(app.pending_jump.is_none());
            deliver_late_page(&mut app, open_gen);
            assert_eq!(app.focus, Pane::Composer);
            assert!(app.timeline.selected.is_none());
        }

        // 3. User scrolls the timeline with the mouse wheel while digging.
        {
            let (mut app, mut rx, _h) = test_app_with_matrix();
            let open_gen = start_digging(&mut app, &mut rx);
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
            deliver_late_page(&mut app, open_gen);
            assert!(app.timeline.selected.is_none());
        }

        // 4. User opens another popup while digging.
        {
            let (mut app, mut rx, _h) = test_app_with_matrix();
            let open_gen = start_digging(&mut app, &mut rx);
            app.perform(Action::OpenSettings);
            assert!(app.pending_jump.is_none());
            assert!(matches!(app.modal, Some(Modal::Settings(_))));
            deliver_late_page(&mut app, open_gen);
            assert!(app.timeline.selected.is_none());
        }

        // 5. User moves selection via key action (SelectOlder) while digging.
        {
            let (mut app, mut rx, _h) = test_app_with_matrix();
            let open_gen = start_digging(&mut app, &mut rx);
            app.perform(Action::SelectOlder);
            assert!(app.pending_jump.is_none());
            assert_eq!(app.timeline.selected, Some(1));
            deliver_late_page(&mut app, open_gen);
            assert_eq!(app.timeline.selected, Some(2));
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
}
