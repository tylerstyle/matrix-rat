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
    pub paginate_rounds: u8,
    /// Wait for the fresh `MatrixEvent::Timeline` from `OpenRoom` before paginating back.
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
        let mut hits = Vec::new();
        for room in &self.rooms {
            if room.kind == RoomKind::Invite {
                continue;
            }
            for (idx, msg) in room.messages.iter().enumerate().rev() {
                if matches!(msg.kind, MsgKind::System | MsgKind::ReadMarker) {
                    continue;
                }
                let lower_body = msg.body.to_lowercase();
                let lower_sender = msg.sender.to_lowercase();
                let all_match = terms
                    .iter()
                    .all(|t| lower_body.contains(t) || lower_sender.contains(t));
                if !all_match {
                    continue;
                }
                let snippet = window_snippet(&msg.body, query);
                if snippet.is_empty() {
                    continue;
                }
                hits.push(SearchHit {
                    room_id: room.id.clone(),
                    event_id: format!("demo:{idx}"),
                    room_name: room.name.clone(),
                    sender: msg.sender.clone(),
                    date: msg.day.clone(),
                    snippet,
                });
                if hits.len() >= DEMO_MAX_HITS {
                    return hits;
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

        let same_room = room_index == self.active && !self.thread_open();
        if !same_room {
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

        self.pending_jump = Some(PendingJump {
            room_id: hit.room_id,
            event_id: hit.event_id,
            paginate_rounds: MAX_JUMP_DIG,
            awaiting_timeline: !same_room && self.matrix.is_some(),
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

        if let Some(pos) =
            find_target_message(self.messages(), &jump.event_id, self.matrix.is_none())
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

fn find_target_message(messages: &[Message], target: &str, demo_mode: bool) -> Option<usize> {
    if demo_mode && let Some(idx) = target.strip_prefix("demo:").and_then(|s| s.parse().ok()) {
        return (idx < messages.len()).then_some(idx);
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
}
