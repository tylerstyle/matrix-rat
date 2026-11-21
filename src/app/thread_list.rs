//! The threads popup: the threads of a room, newest activity first, as the homeserver
//! lists them a page at a time, and those the room's loaded messages know of.

use crossterm::event::{KeyCode, KeyModifiers};

use super::App;
use crate::lineedit::EditKeys;
use crate::matrix::{MatrixCmd, ThreadPage};
use crate::modal::{Modal, ThreadsLoad, ThreadsPopup};
use crate::model::{ItemRef, MsgKind, ThreadInfo, fit_name};
use crate::text::first_line;

/// Longest start of a first message, and longest name, kept for the list.
const TEXT: usize = 200;
const NAME: usize = 64;

impl App {
    /// Opens the threads of the room that room-level commands are about: the open
    /// one, or in the room list the one under the cursor.
    pub(super) fn open_threads(&mut self) {
        let Some(index) = self.target_room() else {
            self.show_toast("No room is open");
            return;
        };
        let room = &self.rooms[index];
        if room.is_pending() {
            self.show_toast(room.kind.pending_hint());
            return;
        }
        let room_id = room.id.clone();
        let room_name = room.name.clone();
        let local = self.known_threads(index);
        let connected = self.matrix.is_some();
        let generation = self.next_threads_generation();
        self.open_modal(Modal::Threads(ThreadsPopup::new(
            room_id.clone(),
            room_name,
            generation,
            local,
            connected,
        )));
        if connected {
            self.send_matrix(MatrixCmd::ListThreads {
                room_id,
                from: None,
                mine: false,
                generation,
            });
        }
    }

    fn next_threads_generation(&mut self) -> u64 {
        self.threads.list_gen = self.threads.list_gen.wrapping_add(1);
        self.threads.list_gen
    }

    /// The threads that the loaded messages of the room at `index` know of, newest
    /// first. In the demo they are all there are.
    fn known_threads(&self, index: usize) -> Vec<ThreadInfo> {
        let connected = self.matrix.is_some();
        self.room_messages_of(index)
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(at, message)| {
                let thread = message.thread.as_ref()?;
                let root = match &message.item {
                    Some(ItemRef::Event(id)) => id.clone(),
                    _ if !connected => format!("demo:{at}"),
                    // Not on the server yet: it has no thread to list.
                    _ => return None,
                };
                let text = match message.kind {
                    MsgKind::System | MsgKind::ReadMarker => return None,
                    MsgKind::Redacted => "[message deleted]".to_owned(),
                    MsgKind::Undecryptable => "Encrypted message".to_owned(),
                    MsgKind::Text | MsgKind::Emote => first_line(&message.body, TEXT),
                };
                // Only the demo has the replies at hand, and with them their time.
                let newest = thread.demo_replies.last();
                // Named as in the timeline: with the user ID where someone else goes
                // by the same name (as the latest replier is), which stays whole.
                Some(ThreadInfo {
                    root,
                    sender: fit_name(&message.sender_name(), NAME),
                    text: if text.is_empty() {
                        "[message]".to_owned()
                    } else {
                        text
                    },
                    replies: thread.replies,
                    latest_sender: thread.latest.as_ref().map(|(who, _)| fit_name(who, NAME)),
                    latest_when: newest.map(|reply| {
                        if reply.day == "Today" {
                            reply.time.clone()
                        } else {
                            reply.day.clone()
                        }
                    }),
                    unread: thread.unread,
                    mine: message.own,
                })
            })
            .collect()
    }

    pub(super) fn on_threads_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Some(Modal::Threads(popup)) = &mut self.modal else {
            return;
        };
        let page = popup.page.max(1) as isize;
        match (code, mods) {
            (KeyCode::Esc, _) => {
                self.modal = None;
                return;
            }
            // Typing goes to the filter, so the switch is on Tab.
            (KeyCode::Tab | KeyCode::BackTab, _) => {
                self.toggle_threads_mine();
                return;
            }
            (KeyCode::Enter, _) => {
                let selected = popup.selected;
                self.activate_thread_row(selected);
                return;
            }
            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => popup.step(-1),
            (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => popup.step(1),
            (KeyCode::PageUp, _) => popup.step(-page),
            (KeyCode::PageDown, _) => popup.step(page),
            // Typing narrows what is loaded; it asks the server for nothing.
            _ => {
                if popup
                    .filter
                    .handle_key(code, mods, EditKeys::FULL)
                    .changed()
                {
                    refilter(popup);
                }
                return;
            }
        }
        self.maybe_load_more_threads(false);
    }

    pub(super) fn on_threads_paste(&mut self, first_line: &str) {
        if let Some(Modal::Threads(popup)) = &mut self.modal
            && popup.filter.paste(first_line).changed()
        {
            refilter(popup);
        }
    }

    pub(super) fn click_threads_filter(&mut self, column: usize) {
        if let Some(Modal::Threads(popup)) = &mut self.modal {
            popup.filter.place(column, false);
        }
    }

    /// The wheel moves the list a thread at a time.
    pub(super) fn scroll_threads(&mut self, delta: isize) {
        if let Some(Modal::Threads(popup)) = &mut self.modal {
            popup.scroll_by(delta.signum());
        }
        self.maybe_load_more_threads(true);
    }

    /// Lists only the threads the user took part in, or all of them again: the
    /// server's list starts over (the demo just looks at its own).
    pub(super) fn toggle_threads_mine(&mut self) {
        let connected = self.matrix.is_some();
        let generation = self.next_threads_generation();
        let Some(Modal::Threads(popup)) = &mut self.modal else {
            return;
        };
        popup.mine = !popup.mine;
        popup.restart(generation, connected);
        let command = MatrixCmd::ListThreads {
            room_id: popup.room_id.clone(),
            from: None,
            mine: popup.mine,
            generation,
        };
        if connected {
            self.send_matrix(command);
        }
    }

    /// Enter or a click on a row: opens its thread, or on the row after the threads
    /// loads more of them.
    pub(super) fn activate_thread_row(&mut self, row: usize) {
        let Some(Modal::Threads(popup)) = &mut self.modal else {
            return;
        };
        if row < popup.rows() {
            popup.selected = row;
        }
        let Some(thread) = popup.thread_at(row) else {
            if row == popup.shown.len() {
                self.load_more_threads();
            }
            return;
        };
        let (room_id, root) = (popup.room_id.clone(), thread.root.clone());
        self.modal = None;
        self.open_listed_thread(room_id, root);
    }

    /// A click on the row after the threads: selected, as Enter would have it.
    pub(super) fn click_threads_more(&mut self) {
        if let Some(Modal::Threads(popup)) = &self.modal {
            let row = popup.shown.len();
            self.activate_thread_row(row);
        }
    }

    fn maybe_load_more_threads(&mut self, by_view: bool) {
        let wanted = matches!(
            &self.modal,
            Some(Modal::Threads(popup))
                if if by_view { popup.shows_end() } else { popup.wants_more() }
        );
        if wanted {
            self.load_more_threads();
        }
    }

    /// Asks for the next page of the server's list, unless there is none or one is
    /// on its way already.
    pub(super) fn load_more_threads(&mut self) {
        let Some(Modal::Threads(popup)) = &mut self.modal else {
            return;
        };
        if popup.load != ThreadsLoad::More {
            return;
        }
        let Some(from) = popup.next.clone() else {
            return;
        };
        popup.load = ThreadsLoad::Loading;
        popup.asked = Some(from.clone());
        let command = MatrixCmd::ListThreads {
            room_id: popup.room_id.clone(),
            from: Some(from),
            mine: popup.mine,
            generation: popup.generation,
        };
        self.send_matrix(command);
    }

    /// A page of the server's list. Pages for another room or an earlier listing
    /// (before "only mine" was switched, or a popup closed since) are dropped.
    pub(super) fn on_threads(&mut self, page: ThreadPage) {
        if let Some(popup) = self.threads_popup_for(&page.room_id, page.generation) {
            popup.add_page(page.threads, page.next);
        }
    }

    pub(super) fn on_threads_failed(&mut self, room_id: &str, generation: u64) {
        if let Some(popup) = self.threads_popup_for(room_id, generation) {
            popup.failed();
        }
    }

    /// The open threads popup, if it waits for this answer.
    fn threads_popup_for(&mut self, room_id: &str, generation: u64) -> Option<&mut ThreadsPopup> {
        match &mut self.modal {
            Some(Modal::Threads(popup))
                if popup.room_id == room_id
                    && popup.generation == generation
                    && popup.load == ThreadsLoad::Loading =>
            {
                Some(popup)
            }
            _ => None,
        }
    }

    /// Shows the thread that starts at `root` in that room: the usual way where the
    /// room's loaded messages hold its first message, else by jumping to that message
    /// (which loads the part of the thread around it).
    fn open_listed_thread(&mut self, room_id: String, root: String) {
        let Some(index) = self.rooms.iter().position(|room| room.id == room_id) else {
            self.show_toast("That room is no longer available");
            return;
        };
        let open_here = Some(index) == self.open_index();
        if open_here && self.thread_key() == Some(root.as_str()) {
            return;
        }
        if self.matrix.is_none() {
            self.open_demo_thread(index, &root);
            return;
        }
        let loaded = open_here
            .then(|| {
                self.room_messages_of(index)
                    .iter()
                    .position(|m| matches!(&m.item, Some(ItemRef::Event(id)) if *id == root))
            })
            .flatten();
        match loaded {
            Some(at) => {
                // Threads do not nest: the one shown makes way.
                self.leave_thread();
                self.open_thread(at);
            }
            None => self.jump_to_event(room_id, root.clone(), Some(root)),
        }
    }

    /// The demo keeps every thread with its first message, at `demo:` and its place.
    fn open_demo_thread(&mut self, index: usize, root: &str) {
        let Some(at) = root
            .strip_prefix("demo:")
            .and_then(|n| n.parse::<usize>().ok())
        else {
            return;
        };
        if Some(index) != self.open_index() || self.thread_open() {
            self.open_room(index);
            if let Some(row) = self.visible_rooms().iter().position(|i| *i == index) {
                self.sidebar.cursor = row;
                self.sidebar.reveal_cursor = true;
            }
        }
        if self
            .messages()
            .get(at)
            .is_some_and(|message| message.thread.is_some())
        {
            self.open_thread(at);
        } else {
            self.show_toast("That thread is no longer there");
        }
    }
}

/// After the filter changed: the first thread that passes it is selected.
fn refilter(popup: &mut ThreadsPopup) {
    popup.refresh();
    popup.selected = 0;
    popup.scroll = 0;
    popup.reveal = true;
}

#[cfg(test)]
mod tests {
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::action::{Action, Pane};
    use crate::matrix::{DetachedTimeline, MatrixEvent, MatrixHandle, SyncStatus};
    use crate::model::{Message, Room, RoomKind, ThreadSummary};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
    }

    fn screen(app: &mut App, terminal: &mut Terminal<TestBackend>) -> String {
        terminal.draw(|f| crate::ui::draw(f, app)).unwrap();
        // The backend also reports what lies under the second half of a wide
        // character, which is left over from earlier frames.
        format!("{}", terminal.backend())
            .lines()
            .map(|line| line.split(" Hidden by").next().unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Clicks on the first occurrence of `needle` on the screen.
    fn click_on(app: &mut App, terminal: &mut Terminal<TestBackend>, needle: &str) {
        let text = screen(app, terminal);
        let (row, line) = text
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} is not on screen:\n{text}"));
        let before = &line[..line.find(needle).unwrap()];
        // The backend prints a quote before each row.
        let column = crate::text::width(before) as u16 - 1;
        app.on_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: column + 1,
            row: row as u16,
            modifiers: KeyModifiers::NONE,
        }));
    }

    fn remote(n: usize, sender: &str, body: &str) -> Message {
        Message {
            id: n.to_string(),
            item: Some(ItemRef::Event(format!("$ev{n}"))),
            sender: sender.into(),
            time: "10:00".into(),
            day: "Today".into(),
            body: body.into(),
            own: sender == "me",
            ..Message::default()
        }
    }

    /// Connected to a fake worker, on an 80x24 screen, with two rooms; in the first,
    /// message 5 started a thread with unread replies.
    fn connected() -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let mut app = App::demo();
        let (handle, commands) = MatrixHandle::for_test();
        app.matrix = Some(handle);
        app.sync = Some(SyncStatus::Synced);
        let mut alpha = Room::new("!a", "Alpha", RoomKind::Room);
        alpha.messages = (0..10)
            .map(|n| {
                remote(
                    n,
                    if n % 2 == 0 { "ann" } else { "ben" },
                    &format!("line {n}"),
                )
            })
            .collect();
        alpha.messages[5].thread = Some(ThreadSummary {
            replies: 3,
            latest: Some(("carol".into(), "sounds good".into())),
            latest_sender: None,
            unread: true,
            demo_replies: Vec::new(),
        });
        app.rooms = vec![alpha, Room::new("!b", "Beta", RoomKind::Room)];
        app.active = 0;
        app.generation = 1;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        screen(&mut app, &mut terminal);
        (app, terminal, commands)
    }

    /// The pages of threads asked for so far: room, where from, only mine, listing.
    fn asked(
        commands: &mut UnboundedReceiver<MatrixCmd>,
    ) -> Vec<(String, Option<String>, bool, u64)> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            if let MatrixCmd::ListThreads {
                room_id,
                from,
                mine,
                generation,
            } = command
            {
                seen.push((room_id, from, mine, generation));
            }
        }
        seen
    }

    /// Everything else that was sent, as the log would show it.
    fn sent(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            if !matches!(
                command,
                MatrixCmd::Typing { .. } | MatrixCmd::SaveDraft { .. } | MatrixCmd::InThread { .. }
            ) {
                seen.push(command.describe());
            }
        }
        seen
    }

    fn thread(n: usize, sender: &str, text: &str) -> ThreadInfo {
        ThreadInfo {
            root: format!("$root{n}"),
            sender: sender.into(),
            text: text.into(),
            replies: 2,
            latest_sender: Some("carol".into()),
            latest_when: Some("10:35".into()),
            ..ThreadInfo::default()
        }
    }

    fn popup(app: &App) -> &ThreadsPopup {
        match &app.modal {
            Some(Modal::Threads(popup)) => popup,
            other => panic!("no threads popup: {other:?}"),
        }
    }

    /// A page for the listing the popup waits for.
    fn page(app: &App, threads: Vec<ThreadInfo>, next: Option<&str>) -> MatrixEvent {
        let popup = popup(app);
        MatrixEvent::Threads(ThreadPage {
            room_id: popup.room_id.clone(),
            generation: popup.generation,
            threads,
            next: next.map(str::to_owned),
        })
    }

    fn roots(app: &App) -> Vec<&str> {
        popup(app).shown.iter().map(|t| t.root.as_str()).collect()
    }

    #[test]
    fn the_first_page_is_asked_for_and_listed_newest_activity_first() {
        let (mut app, mut terminal, mut commands) = connected();
        app.perform(Action::OpenThreads);
        let generation = popup(&app).generation;
        assert_eq!(
            asked(&mut commands),
            [("!a".into(), None, false, generation)]
        );
        // The loaded messages' threads wait for the server's order.
        assert!(roots(&app).is_empty());
        assert!(screen(&mut app, &mut terminal).contains("Loading threads…"));

        let mut known = thread(5, "ben", "line 5");
        known.root = "$ev5".into();
        app.on_matrix(page(
            &app,
            vec![
                thread(2, "dora", "the newest talk"),
                known,
                thread(1, "ann", "older"),
            ],
            None,
        ));
        assert_eq!(roots(&app), ["$root2", "$ev5", "$root1"]);
        assert_eq!(popup(&app).load, ThreadsLoad::Done);
        // What the room's messages say about a thread is the newer word on reading it.
        assert!(popup(&app).shown[1].unread);
        let text = screen(&mut app, &mut terminal);
        for shown in [
            "Threads — Alpha",
            "3 threads",
            "○ Only mine",
            "› dora · 2 replies",
            "the newest talk",
            "ben · 2 replies · new",
            "carol · 10:35",
            "Enter open · Tab only mine · Esc close",
        ] {
            assert!(text.contains(shown), "{shown:?} missing:\n{text}");
        }
    }

    /// A thread started by one of two of a name is listed with the starter's id, as
    /// its latest replier is.
    #[test]
    fn a_thread_of_one_of_two_of_a_name_names_its_starter_with_the_id() {
        let (mut app, _terminal, _commands) = connected();
        let root = &mut app.rooms[0].messages[5];
        root.sender_id = Some("@ben:elsewhere.org".into());
        root.sender_ambiguous = true;
        let listed = app.known_threads(0);
        let thread = listed.iter().find(|thread| thread.root == "$ev5").unwrap();
        assert_eq!(thread.sender, "ben (@ben:elsewhere.org)");
    }

    #[test]
    fn threads_only_the_loaded_messages_know_are_added_once_the_list_is_complete() {
        let (mut app, _terminal, mut commands) = connected();
        app.perform(Action::OpenThreads);
        asked(&mut commands);
        app.on_matrix(page(&app, vec![thread(2, "dora", "newest")], Some("t1")));
        assert_eq!(roots(&app), ["$root2"]);
        app.load_more_threads();
        app.on_matrix(page(&app, vec![thread(1, "ann", "older")], None));
        assert_eq!(roots(&app), ["$root2", "$root1", "$ev5"]);
    }

    #[test]
    fn answers_for_an_earlier_listing_or_another_room_are_dropped() {
        let (mut app, _terminal, mut commands) = connected();
        app.perform(Action::OpenThreads);
        let first = popup(&app).generation;
        // Only mine: the list starts over, as a listing of its own.
        app.on_event(key(KeyCode::Tab));
        let second = popup(&app).generation;
        assert_ne!(first, second);
        assert!(popup(&app).mine);
        assert_eq!(
            asked(&mut commands),
            [
                ("!a".into(), None, false, first),
                ("!a".into(), None, true, second)
            ]
        );
        // The first listing's answers come late.
        app.on_matrix(MatrixEvent::Threads(ThreadPage {
            room_id: "!a".into(),
            generation: first,
            threads: vec![thread(1, "ann", "not mine")],
            next: None,
        }));
        app.on_matrix(MatrixEvent::ThreadsFailed {
            room_id: "!a".into(),
            generation: first,
        });
        // And one for another room with the same number.
        app.on_matrix(MatrixEvent::Threads(ThreadPage {
            room_id: "!b".into(),
            generation: second,
            threads: vec![thread(2, "bob", "another room")],
            next: None,
        }));
        assert!(popup(&app).listed.is_empty());
        assert_eq!(popup(&app).load, ThreadsLoad::Loading);

        app.on_matrix(page(&app, vec![thread(3, "me", "mine")], None));
        // The thread of the loaded messages is not the user's.
        assert_eq!(roots(&app), ["$root3"]);

        // Closed and opened again: the old listing's page lands nowhere.
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        app.perform(Action::OpenThreads);
        let third = popup(&app).generation;
        assert!(third != first && third != second);
        app.on_matrix(MatrixEvent::Threads(ThreadPage {
            room_id: "!a".into(),
            generation: second,
            threads: vec![thread(3, "me", "mine")],
            next: None,
        }));
        assert!(popup(&app).listed.is_empty());
        assert!(!popup(&app).mine);
    }

    #[test]
    fn the_list_loads_more_at_its_end_and_stops_where_the_server_does() {
        let (mut app, mut terminal, mut commands) = connected();
        app.perform(Action::OpenThreads);
        let generation = popup(&app).generation;
        asked(&mut commands);
        let first: Vec<ThreadInfo> = (0..12)
            .map(|n| thread(100 - n, "ann", &format!("talk {n}")))
            .collect();
        app.on_matrix(page(&app, first, Some("t1")));
        assert_eq!(popup(&app).rows(), 13);
        screen(&mut app, &mut terminal);

        // Going down: near the end the next page is asked for, once.
        for _ in 0..9 {
            app.on_event(key(KeyCode::Down));
        }
        assert!(asked(&mut commands).is_empty());
        app.on_event(key(KeyCode::Down));
        assert_eq!(
            asked(&mut commands),
            [("!a".into(), Some("t1".into()), false, generation)]
        );
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Down));
        assert!(asked(&mut commands).is_empty());
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("Loading more threads…"), "{text}");
        // The selection stays in view.
        assert!(text.contains("talk 11"), "{text}");

        // What is listed already is not listed twice, and a server that answers with
        // the same place again has no more.
        app.on_matrix(page(
            &app,
            vec![thread(100, "ann", "talk 0"), thread(50, "bob", "older")],
            Some("t1"),
        ));
        assert_eq!(popup(&app).listed.len(), 13);
        assert_eq!(popup(&app).load, ThreadsLoad::Done);
        assert_eq!(roots(&app).last(), Some(&"$ev5"));
        app.on_event(key(KeyCode::PageDown));
        assert!(asked(&mut commands).is_empty());
    }

    #[test]
    fn the_last_row_loads_more_by_enter_a_click_or_the_wheel() {
        let (mut app, mut terminal, mut commands) = connected();
        app.perform(Action::OpenThreads);
        asked(&mut commands);
        app.on_matrix(page(&app, vec![thread(1, "ann", "one")], Some("t1")));
        click_on(&mut app, &mut terminal, "Load more threads");
        assert_eq!(asked(&mut commands)[0].1.as_deref(), Some("t1"));
        assert!(screen(&mut app, &mut terminal).contains("Loading more threads…"));

        app.on_matrix(page(&app, vec![thread(2, "bob", "two")], Some("t2")));
        // The selection is on the first new thread; below it, the row again.
        assert_eq!(popup(&app).selected, 1);
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(asked(&mut commands)[0].1.as_deref(), Some("t2"));
        assert!(app.modal.is_some());

        app.on_matrix(page(&app, vec![thread(3, "cid", "three")], Some("t3")));
        app.on_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 40,
            row: 12,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(asked(&mut commands)[0].1.as_deref(), Some("t3"));
    }

    #[test]
    fn the_filter_narrows_by_text_and_sender() {
        let (mut app, mut terminal, mut commands) = connected();
        app.perform(Action::OpenThreads);
        asked(&mut commands);
        app.on_matrix(page(
            &app,
            vec![
                thread(1, "ann", "Lunch plans"),
                thread(2, "bob", "release notes"),
                thread(3, "Carol", "more lunch"),
            ],
            None,
        ));
        type_text(&mut app, "lunch");
        assert_eq!(roots(&app), ["$root1", "$root3"]);
        assert!(screen(&mut app, &mut terminal).contains("2 matching"));
        app.on_event(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        type_text(&mut app, "carol");
        assert_eq!(roots(&app), ["$root3"]);
        type_text(&mut app, " release");
        assert!(roots(&app).is_empty());
        assert!(screen(&mut app, &mut terminal).contains("No matching threads."));
        // A paste goes into the filter too.
        app.on_event(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        app.on_event(Event::Paste("BOB".into()));
        assert_eq!(roots(&app), ["$root2"]);
        assert_eq!(popup(&app).selected, 0);
        // Nothing was asked of the server for it, also where it has more.
        assert!(asked(&mut commands).is_empty());
        app.on_event(key(KeyCode::Esc));
        app.perform(Action::OpenThreads);
        asked(&mut commands);
        app.on_matrix(page(&app, vec![thread(1, "ann", "one")], Some("t1")));
        type_text(&mut app, "zzz");
        assert!(roots(&app).is_empty());
        assert!(asked(&mut commands).is_empty());
    }

    #[test]
    fn a_failure_says_so_and_keeps_the_threads_of_the_loaded_messages() {
        let (mut app, mut terminal, mut commands) = connected();
        app.perform(Action::OpenThreads);
        let generation = popup(&app).generation;
        asked(&mut commands);
        app.on_matrix(MatrixEvent::ThreadsFailed {
            room_id: "!a".into(),
            generation,
        });
        assert_eq!(popup(&app).load, ThreadsLoad::Failed);
        assert_eq!(roots(&app), ["$ev5"]);
        let text = screen(&mut app, &mut terminal);
        for shown in [
            "Could not load the server's thread list",
            "ben · 3 replies · new",
            "line 5",
        ] {
            assert!(text.contains(shown), "{shown:?} missing:\n{text}");
        }
        // A failed later page keeps what came before.
        app.on_event(key(KeyCode::Tab));
        app.on_event(key(KeyCode::Tab));
        asked(&mut commands);
        app.on_matrix(page(&app, vec![thread(1, "ann", "one")], Some("t1")));
        app.load_more_threads();
        let generation = popup(&app).generation;
        app.on_matrix(MatrixEvent::ThreadsFailed {
            room_id: "!a".into(),
            generation,
        });
        assert_eq!(roots(&app), ["$root1", "$ev5"]);
    }

    #[test]
    fn a_thread_opens_the_usual_way_where_loaded_and_by_a_jump_where_not() {
        let (mut app, _terminal, mut commands) = connected();
        app.perform(Action::OpenThreads);
        asked(&mut commands);
        let mut loaded = thread(5, "ben", "line 5");
        loaded.root = "$ev5".into();
        app.on_matrix(page(&app, vec![thread(9, "dora", "far up"), loaded], None));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.thread_key(), Some("$ev5"));
        assert_eq!(sent(&mut commands), ["OpenThread(!a, $ev5)"]);

        // The same thread again: it is shown already.
        app.perform(Action::OpenThreads);
        let roots_now = {
            asked(&mut commands);
            app.on_matrix(page(&app, vec![thread(9, "dora", "far up")], None));
            roots(&app)
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>()
        };
        // The room's loaded messages are still known while its thread is shown.
        assert_eq!(roots_now, ["$root9", "$ev5"]);
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.thread_key(), Some("$ev5"));
        assert!(sent(&mut commands).is_empty());

        // Far up: a jump to its first message opens the thread around it, and the
        // thread shown until then stays until that has arrived.
        app.perform(Action::OpenThreads);
        asked(&mut commands);
        app.on_matrix(page(&app, vec![thread(9, "dora", "far up")], None));
        app.on_event(key(KeyCode::Enter));
        assert!(app.pending_jump.is_some());
        assert_eq!(app.thread_key(), Some("$ev5"));
        let jump = app.pending_jump.as_ref().unwrap().generation;
        assert_eq!(
            sent(&mut commands),
            [format!("OpenEvent(!a, generation {jump})")]
        );
        let mut root = remote(9, "dora", "far up");
        root.item = Some(ItemRef::Event("$root9".into()));
        let mut reply = remote(11, "ann", "a reply");
        reply.item = Some(ItemRef::Event("$reply".into()));
        app.on_matrix(MatrixEvent::Timeline {
            room_id: "!a".into(),
            generation: jump,
            messages: vec![root, reply],
            reached_start: true,
            detached: Some(DetachedTimeline {
                target_event: "$root9".into(),
                thread_root: Some("$root9".into()),
                reached_end: false,
            }),
        });
        assert_eq!(app.thread_key(), Some("$root9"));
        assert_eq!(app.timeline.selected, Some(0));
        assert_eq!(app.messages()[1].body, "a reply");
    }

    #[test]
    fn the_room_menu_lists_the_threads_of_the_room_under_the_cursor() {
        let (mut app, _terminal, mut commands) = connected();
        app.perform(Action::Focus(Pane::Sidebar));
        let beta = app.visible_rooms().iter().position(|i| *i == 1).unwrap();
        app.sidebar.cursor = beta;
        app.perform(Action::ContextMenu);
        let Some(Modal::Menu(menu)) = &app.modal else {
            panic!("no menu: {:?}", app.modal);
        };
        let at = menu
            .items
            .iter()
            .position(|item| item.label == "Threads")
            .unwrap();
        app.run_menu_at(at);
        assert_eq!(popup(&app).room_id, "!b");
        assert_eq!(popup(&app).room_name, "Beta");
        let generation = popup(&app).generation;
        assert_eq!(
            asked(&mut commands),
            [("!b".into(), None, false, generation)]
        );
        // Focus and cursor stay where they were; the open room too.
        assert_eq!(
            (app.focus, app.sidebar.cursor, app.active),
            (Pane::Sidebar, beta, 0)
        );

        // Opening one of its threads goes to that room.
        app.on_matrix(page(&app, vec![thread(1, "bob", "in beta")], None));
        app.on_event(key(KeyCode::Enter));
        let jump = app.pending_jump.as_ref().map(|jump| jump.generation);
        assert_eq!(
            sent(&mut commands),
            [format!("OpenEvent(!b, generation {})", jump.unwrap())]
        );
    }

    #[test]
    fn the_demo_lists_and_opens_its_thread() {
        let mut app = App::demo();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let welcome = app
            .rooms
            .iter()
            .position(|r| r.name == "matrix-rat")
            .unwrap();
        assert_ne!(app.active, welcome);
        // From the room list, for the room under the cursor.
        app.perform(Action::Focus(Pane::Sidebar));
        app.sidebar.cursor = app
            .visible_rooms()
            .iter()
            .position(|i| *i == welcome)
            .unwrap();
        app.perform(Action::OpenThreads);
        assert_eq!(popup(&app).load, ThreadsLoad::Done);
        assert_eq!(roots(&app), ["demo:0"]);
        let text = screen(&mut app, &mut terminal);
        for shown in [
            "1 thread",
            "› alice · 2 replies",
            "carol · 10:35",
            "Welcome to the demo.",
        ] {
            assert!(text.contains(shown), "{shown:?} missing:\n{text}");
        }
        // Only mine: the demo's thread is not the user's.
        app.on_event(key(KeyCode::Tab));
        assert!(roots(&app).is_empty());
        assert!(screen(&mut app, &mut terminal).contains("No threads you started or answered in."));
        app.on_event(key(KeyCode::Tab));

        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.active, welcome);
        assert!(app.thread_open());
        assert_eq!(app.messages().len(), 3);
        assert!(screen(&mut app, &mut terminal).contains("Threads work too."));

        // Listed again from inside it, and chosen: it stays as it is.
        app.perform(Action::OpenThreads);
        assert_eq!(roots(&app), ["demo:0"]);
        app.on_event(key(KeyCode::Enter));
        assert!(app.thread_open());
        assert_eq!(app.messages().len(), 3);
    }

    #[test]
    fn the_popup_fits_80_by_24_and_keeps_the_selection_in_view() {
        let (mut app, mut terminal, mut commands) = connected();
        app.perform(Action::OpenThreads);
        asked(&mut commands);
        let mut threads: Vec<ThreadInfo> = (0..14)
            .map(|n| thread(100 - n, "ann", &format!("a talk about topic {n}")))
            .collect();
        threads[1].unread = true;
        threads[2].replies = 1;
        threads[2].latest_sender = Some("a rather long display name of someone".into());
        threads[3].text = "x".repeat(300);
        app.on_matrix(page(&app, threads, None));
        insta::assert_snapshot!("threads_popup", screen(&mut app, &mut terminal));
        app.on_event(key(KeyCode::PageDown));
        app.on_event(key(KeyCode::PageDown));
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("› ben · 3 replies · new"), "{text}");
        assert!(
            text.contains("Enter open · Tab only mine · Esc close"),
            "{text}"
        );
    }

    #[test]
    fn logs_tell_nothing_about_the_room_or_its_threads() {
        let mut secret = thread(1, "alice", "classified plans");
        secret.root = "$secret-event".into();
        let popup = ThreadsPopup::new(
            "!secret:example.org".into(),
            "Secret room".into(),
            4,
            vec![secret.clone()],
            true,
        );
        let command = MatrixCmd::ListThreads {
            room_id: "!secret:example.org".into(),
            from: Some("secret-token".into()),
            mine: true,
            generation: 4,
        };
        let page = MatrixEvent::Threads(ThreadPage {
            room_id: "!secret:example.org".into(),
            generation: 4,
            threads: vec![secret.clone()],
            next: Some("secret-token".into()),
        });
        let failed = MatrixEvent::ThreadsFailed {
            room_id: "!secret:example.org".into(),
            generation: 4,
        };
        let logged = format!("{popup:?} {command:?} {page:?} {failed:?} {secret:?}");
        for hidden in ["secret", "Secret", "alice", "classified", "carol", "10:35"] {
            assert!(!logged.contains(hidden), "{hidden} in {logged}");
        }
    }
}
