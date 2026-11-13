//! Threads: the messages pane can show one thread instead of the room. While it does,
//! the room's own view (messages, selection, scroll position, draft) is put aside and
//! comes back untouched when the thread is left.

use std::collections::HashMap;

use super::{App, TimelineState};
use crate::action::Pane;
use crate::matrix::MatrixCmd;
use crate::model::{ItemRef, Message, MsgKind, RoomKind, ThreadSummary};

/// Identity of the thread's first message while it stands in for the thread's own
/// copy of it. Timelines number their items independently, so the id it has in the
/// room cannot be used inside the thread.
const ROOT_ID: &str = "thread-root";

#[derive(Debug, Default)]
pub struct Threads {
    /// The thread shown in the messages pane, if any.
    pub open: Option<ThreadView>,
    /// Unsent text per thread (room id and thread), kept while the thread is closed.
    drafts: HashMap<(String, String), String>,
}

impl Threads {
    /// Event id of the message the open thread starts at.
    pub fn open_root(&self) -> Option<&str> {
        self.open.as_ref()?.root.as_deref()
    }
}

#[derive(Debug)]
pub struct ThreadView {
    /// Event id of the message the thread starts at; `None` in the demo.
    root: Option<String>,
    /// Names the thread among the room's drafts.
    key: String,
    /// The first message as the room shows it.
    root_message: Message,
    /// Where that message is in the room (used by the demo only).
    root_index: usize,
    /// The room's view, to return to.
    room_messages: Vec<Message>,
    room_timeline: TimelineState,
    room_rows: Option<crate::ui::RowCache>,
    room_draft: String,
    focus: Pane,
    /// The worker has sent the thread's messages at least once.
    loaded: bool,
    /// The thread's own timeline holds the first message (it is loaded last, with the
    /// oldest replies). Until then only the room's timeline can act on it.
    root_loaded: bool,
}

impl App {
    /// Whether the messages pane shows a thread.
    pub fn thread_open(&self) -> bool {
        self.threads.open.is_some()
    }

    /// What names the open thread among the things kept per thread (drafts, files
    /// waiting to be sent).
    pub(super) fn thread_key(&self) -> Option<&str> {
        self.threads.open.as_ref().map(|view| view.key.as_str())
    }

    /// A thread is open and still shows only what the room knew of it.
    pub(super) fn thread_loading(&self) -> bool {
        self.threads
            .open
            .as_ref()
            .is_some_and(|view| view.root.is_some() && !view.loaded)
    }

    /// Shows the thread that starts at the message at `index`. A message without
    /// replies gets an empty thread; the first message sent there starts it.
    pub(super) fn open_thread(&mut self, index: usize) {
        if self.thread_open() {
            // Threads do not nest.
            self.show_toast("This is already a thread");
            return;
        }
        if self.room().is_none_or(|room| room.kind == RoomKind::Invite) {
            return;
        }
        let Some(message) = self.messages().get(index) else {
            return;
        };
        if matches!(message.kind, MsgKind::System | MsgKind::ReadMarker) {
            return;
        }
        let root = match (&message.item, self.matrix.is_some()) {
            (Some(ItemRef::Event(id)), true) => Some(id.clone()),
            (_, true) => return self.show_toast("That message has not been sent yet"),
            (_, false) => None,
        };
        let Some(room_id) = self.room_id() else {
            return;
        };
        let root_message = message.clone();

        // An edit belongs to the room's messages; the room's draft waits for the return.
        self.cancel_compose();
        let room_draft = self.composer.take();
        self.save_room_marks(&room_id);
        let key = root.clone().unwrap_or_else(|| format!("demo:{index}"));
        let draft = self
            .threads
            .drafts
            .remove(&(room_id.clone(), key.clone()))
            .unwrap_or_default();
        self.composer.set_text(&draft);
        self.restore_thread_marks(&room_id, &key);

        // Shown at once: the first message, and in the demo the replies it carries.
        let mut first = root_message.clone();
        first.id = ROOT_ID.to_owned();
        // Those are the room's receipts, not the thread's.
        first.read_by = Default::default();
        let replies = first.thread.take().map(|thread| thread.demo_replies);
        let mut shown = vec![first];
        shown.extend(replies.unwrap_or_default());

        let room = &mut self.rooms[self.active];
        self.repaint = true;
        self.threads.open = Some(ThreadView {
            root: root.clone(),
            key,
            root_message,
            root_index: index,
            room_messages: std::mem::replace(&mut room.messages, shown),
            room_timeline: std::mem::take(&mut self.timeline),
            room_rows: self.row_cache.take(),
            room_draft,
            focus: self.focus,
            loaded: false,
            root_loaded: false,
        });
        self.timeline_rev += 1;
        // A new generation: late updates for the room are dropped from here on.
        self.generation += 1;
        if let Some(root) = root {
            self.send_matrix(MatrixCmd::OpenThread {
                room_id,
                root,
                generation: self.generation,
            });
        }
        self.set_focus(Pane::Composer);
    }

    /// Returns from the thread to the room, exactly where the user was.
    pub(super) fn close_thread(&mut self) {
        if self.leave_thread() {
            // The room's messages are shown as they were; this brings them up to date.
            self.request_timeline();
        }
    }

    /// Puts the room's view back without asking for its messages (for callers that
    /// do so themselves). False if no thread was open.
    pub(super) fn leave_thread(&mut self) -> bool {
        let Some(view) = self.threads.open.take() else {
            return false;
        };
        self.repaint = true;
        self.cancel_compose();
        let draft = self.composer.take();
        if !draft.trim().is_empty()
            && let Some(room_id) = self.room_id()
        {
            self.save_thread_marks(&room_id, &view.key);
            self.threads.drafts.insert((room_id, view.key), draft);
        } else {
            self.clear_completion_marks();
        }
        self.composer.set_text(&view.room_draft);
        if let Some(room_id) = self.room_id() {
            self.restore_room_marks(&room_id);
        }

        let mut deleted = None;
        if let Some(room) = self.rooms.get_mut(self.active) {
            let shown = std::mem::replace(&mut room.messages, view.room_messages);
            match &view.root {
                // What was read is no longer new; the next update has the real state.
                Some(root) => {
                    let first = room.messages.iter_mut().find(
                        |message| matches!(&message.item, Some(ItemRef::Event(id)) if id == root),
                    );
                    if let Some(thread) = first.and_then(|message| message.thread.as_mut()) {
                        thread.unread = false;
                    }
                }
                // The demo keeps a thread's replies with its first message.
                None => {
                    let (mut roots, replies): (Vec<_>, Vec<_>) =
                        shown.into_iter().partition(|m| m.id == ROOT_ID);
                    match (roots.pop(), room.messages.get_mut(view.root_index)) {
                        (Some(mut edited), Some(first)) => {
                            edited.id = std::mem::take(&mut first.id);
                            edited.thread = ThreadSummary::of(replies);
                            *first = edited;
                        }
                        // Deleted inside the thread: gone from the room too.
                        (None, Some(_)) => {
                            room.messages.remove(view.root_index);
                            deleted = Some(view.root_index);
                        }
                        _ => {}
                    }
                }
            }
        }
        self.timeline = view.room_timeline;
        if let Some(deleted) = deleted {
            // The selection stays on its message, or moves next to the gap.
            self.timeline.selected = match self.timeline.selected {
                Some(i) if i > deleted => Some(i - 1),
                Some(i) if i == deleted => self
                    .find_selectable(i.saturating_sub(1), false)
                    .or_else(|| self.find_selectable(i, true)),
                other => other,
            };
        }
        // Answers to what was asked for the room before are dropped as stale.
        self.timeline.loading_older = false;
        self.timeline.drawn = false;
        self.row_cache = view.room_rows;
        self.timeline_rev += 1;
        self.focus = view.focus;
        true
    }

    /// Addresses a command about messages to the open thread, if there is one.
    pub(super) fn in_thread(&self, command: MatrixCmd) -> MatrixCmd {
        let view = self.threads.open.as_ref();
        let root = view.and_then(|view| view.root.clone());
        // The thread's first message is a message of the room. While the thread's
        // timeline does not hold it yet, the room's is the one that can change it.
        if let (Some(root), Some(view)) = (&root, view)
            && !view.root_loaded
            && let MatrixCmd::Edit { item, .. }
            | MatrixCmd::ToggleReaction { item, .. }
            | MatrixCmd::Redact { item, .. } = &command
            && matches!(item, ItemRef::Event(id) if id == root)
        {
            return command;
        }
        match (root, command) {
            (
                Some(root),
                command @ (MatrixCmd::Send { .. }
                | MatrixCmd::Edit { .. }
                | MatrixCmd::ToggleReaction { .. }
                | MatrixCmd::Redact { .. }
                | MatrixCmd::Retry { .. }
                | MatrixCmd::Paginate { .. }
                | MatrixCmd::MarkRead { .. }
                | MatrixCmd::SendFiles { .. }),
            ) => MatrixCmd::InThread {
                root,
                command: Box::new(command),
            },
            (_, command) => command,
        }
    }

    /// The messages of an update for the open thread. The first message keeps one
    /// identity throughout, so a selection, reply or edit on it stays put: until the
    /// thread's own copy is loaded the one known from the room stands in for it, and
    /// the thread's copy then takes its place under the same id.
    pub(super) fn thread_snapshot(&mut self, mut messages: Vec<Message>) -> Vec<Message> {
        let Some(view) = &mut self.threads.open else {
            return messages;
        };
        let Some(root) = &view.root else {
            return messages;
        };
        view.loaded = true;
        let own = messages
            .iter_mut()
            .find(|m| matches!(&m.item, Some(ItemRef::Event(id)) if id == root));
        view.root_loaded = own.is_some();
        match own {
            Some(first) => first.id = ROOT_ID.to_owned(),
            None => {
                let mut first = view.root_message.clone();
                first.id = ROOT_ID.to_owned();
                first.thread = None;
                // Those are the room's receipts, not the thread's.
                first.read_by = Default::default();
                messages.insert(0, first);
            }
        }
        messages
    }
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
    use crate::action::Action;
    use crate::matrix::{MatrixEvent, MatrixHandle, SyncStatus};
    use crate::model::Room;

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
        // The backend prints a quote before each row; wide characters shift nothing here.
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

    /// Connected to a fake worker, with two rooms; the first has forty messages, and
    /// message 5 started a thread.
    fn connected() -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let mut app = App::demo();
        let (handle, commands) = MatrixHandle::for_test();
        app.matrix = Some(handle);
        app.sync = Some(SyncStatus::Synced);
        let mut alpha = Room::new("!a", "Alpha", RoomKind::Room);
        alpha.messages = (0..40)
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
            latest: Some(("carol".into(), "sounds good to me".into())),
            unread: true,
            demo_replies: Vec::new(),
        });
        app.rooms = vec![alpha, Room::new("!b", "Beta", RoomKind::Room)];
        app.active = 0;
        app.generation = 1;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        screen(&mut app, &mut terminal);
        (app, terminal, commands)
    }

    /// The next command that is not a typing notice.
    fn next(commands: &mut UnboundedReceiver<MatrixCmd>) -> Option<MatrixCmd> {
        loop {
            match commands.try_recv().ok()? {
                MatrixCmd::Typing { .. } => {}
                command => return Some(command),
            }
        }
    }

    /// The commands sent so far, without typing notices.
    fn drain(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<String> {
        let mut seen = Vec::new();
        while let Some(command) = next(commands) {
            seen.push(format!("{command:?}"));
        }
        seen
    }

    fn update(app: &App, generation: u64, messages: Vec<Message>) -> MatrixEvent {
        MatrixEvent::Timeline {
            room_id: app.rooms[app.active].id.clone(),
            generation,
            messages,
            reached_start: true,
        }
    }

    /// What the worker would send for the thread of message 5.
    fn thread_messages() -> Vec<Message> {
        vec![
            remote(0, "ben", "line 5"),
            remote(1, "ann", "first reply"),
            remote(2, "carol", "sounds good to me"),
        ]
        .into_iter()
        .enumerate()
        .map(|(n, mut message)| {
            // Ids are counted per timeline; only the first message is the room's.
            message.item = Some(ItemRef::Event(if n == 0 {
                "$ev5".to_owned()
            } else {
                format!("$reply{n}")
            }));
            message
        })
        .collect()
    }

    #[test]
    fn summary_line_and_thread_view_in_the_demo() {
        let mut app = App::demo();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let welcome = app
            .rooms
            .iter()
            .position(|r| r.name == "matrix-rat")
            .unwrap();
        app.open_room(welcome);
        insta::assert_snapshot!("thread_summary_line", screen(&mut app, &mut terminal));

        // A click on the line opens the thread; the composer writes into it.
        click_on(&mut app, &mut terminal, "2 replies");
        assert!(app.thread_open());
        assert_eq!(app.focus, Pane::Composer);
        insta::assert_snapshot!("thread_view", screen(&mut app, &mut terminal));
        type_text(&mut app, "on it");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.rooms[welcome].messages.len(), 4);
        assert!(screen(&mut app, &mut terminal).contains("on it"));

        // The clickable way back; the room now counts the new reply.
        click_on(&mut app, &mut terminal, "← back");
        assert!(!app.thread_open());
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("💬 3 replies · you: on it"), "{text}");
        assert_eq!(app.rooms[welcome].messages.len(), 1);
        let thread = app.rooms[welcome].messages[0].thread.as_ref().unwrap();
        assert_eq!(thread.demo_replies.len(), 3);
    }

    #[test]
    fn unread_threads_stand_out() {
        let (mut app, mut terminal, _) = connected();
        app.timeline.scroll = 1000;
        let text = screen(&mut app, &mut terminal);
        assert!(
            text.contains("💬 3 replies · new · carol: sounds good to me"),
            "{text}"
        );
        // Having been in the thread, its replies are no longer new.
        app.open_thread(5);
        app.close_thread();
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("💬 3 replies · carol: sounds"), "{text}");
    }

    #[test]
    fn opening_and_closing_restores_the_room_view() {
        let (mut app, mut terminal, mut commands) = connected();
        app.rooms[0].messages[5].thread.as_mut().unwrap().unread = false;
        app.timeline_rev += 1;
        // Reading older messages, with one of them selected.
        app.perform(Action::Focus(Pane::Timeline));
        app.timeline.selected = Some(5);
        app.timeline.reveal_selected = true;
        screen(&mut app, &mut terminal);
        let scroll = app.timeline.scroll;
        assert!(scroll > 0);
        let before = screen(&mut app, &mut terminal);
        drain(&mut commands);

        app.on_event(key(KeyCode::Char('t')));
        assert!(app.thread_open());
        assert_eq!(app.generation, 2);
        assert_eq!(drain(&mut commands), ["OpenThread(!a, $ev5)"]);
        // The first message is there at once, without its summary line.
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("Thread · #Alpha"), "{text}");
        assert!(
            text.contains("line 5") && !text.contains("line 6"),
            "{text}"
        );
        assert!(!text.contains("💬"), "{text}");
        assert!(text.contains("Reply in thread…"), "{text}");

        // Nothing of the thread has arrived yet, so nothing is marked read; the
        // replies are asked for.
        app.after_draw();
        assert!(app.timeline.marked_read.is_none());
        assert_eq!(drain(&mut commands), ["InThread($ev5, Paginate(!a))"]);

        app.on_matrix(update(&app, 2, thread_messages()));
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("first reply"), "{text}");
        assert_eq!(app.rooms[0].messages.len(), 3);
        // Reading the thread to its end marks the thread read, not the room.
        app.after_draw();
        assert_eq!(drain(&mut commands), ["InThread($ev5, MarkRead(!a))"]);

        // Esc leaves the thread from the composer (after leaving the messages).
        app.perform(Action::SelectOlder);
        app.on_event(key(KeyCode::Esc));
        assert!(app.thread_open());
        app.on_event(key(KeyCode::Esc));
        assert!(!app.thread_open());
        assert_eq!(app.generation, 3);
        assert_eq!(drain(&mut commands), ["OpenRoom(!a)"]);
        assert_eq!(app.rooms[0].messages.len(), 40);
        assert_eq!(app.timeline.selected, Some(5));
        assert_eq!(app.focus, Pane::Timeline);
        // The same screen as before.
        let after = screen(&mut app, &mut terminal);
        assert_eq!(app.timeline.scroll, scroll);
        assert_eq!(after, before);

        // The room's update keeps the selection on its message.
        let mut messages = app.rooms[0].messages.clone();
        messages.push(remote(40, "ann", "line 40"));
        app.on_matrix(update(&app, 3, messages));
        assert_eq!(app.timeline.selected, Some(5));
    }

    #[test]
    fn stale_updates_are_dropped() {
        let (mut app, _terminal, _commands) = connected();
        let room_update = update(&app, 1, vec![remote(99, "ann", "late for the room")]);
        app.open_thread(5);
        // The room's update arrives after the thread was opened.
        app.on_matrix(room_update);
        assert_eq!(app.rooms[0].messages.len(), 1);
        assert_eq!(app.rooms[0].messages[0].body, "line 5");
        // An update that does not hold the first message yet keeps showing it.
        app.on_matrix(update(&app, 2, Vec::new()));
        assert_eq!(app.rooms[0].messages.len(), 1);
        app.on_matrix(update(&app, 2, thread_messages()));
        assert_eq!(app.rooms[0].messages.len(), 3);
        // The thread's own copy of the first message goes by the same id.
        let ids: Vec<&str> = app.rooms[0]
            .messages
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids, [ROOT_ID, "1", "2"]);

        // And the other way round: the thread's update after going back.
        app.close_thread();
        app.on_matrix(update(
            &app,
            2,
            vec![remote(98, "ann", "late for the thread")],
        ));
        assert_eq!(app.rooms[0].messages.len(), 40);
        // A finished request for older replies does not concern the room either.
        app.timeline.loading_older = true;
        app.on_matrix(MatrixEvent::Paginated {
            generation: 2,
            reached_start: true,
        });
        assert!(app.timeline.loading_older && !app.timeline.reached_start);
    }

    #[test]
    fn commands_carry_the_thread_while_it_is_open() {
        let (mut app, mut terminal, mut commands) = connected();
        app.open_thread(5);
        app.on_matrix(update(&app, 2, thread_messages()));
        drain(&mut commands);

        type_text(&mut app, "hello thread");
        app.on_event(key(KeyCode::Enter));
        match next(&mut commands).unwrap() {
            MatrixCmd::InThread { root, command } => {
                assert_eq!(root, "$ev5");
                assert!(matches!(
                    *command,
                    MatrixCmd::Send { ref room_id, reply_to: None, .. } if room_id == "!a"
                ));
            }
            other => panic!("{other:?}"),
        }

        // A reply to a message of the thread stays in the thread.
        app.perform(Action::SelectOlder);
        app.perform(Action::Reply);
        type_text(&mut app, "yes");
        app.on_event(key(KeyCode::Enter));
        match next(&mut commands).unwrap() {
            MatrixCmd::InThread { root, command } => {
                assert_eq!(root, "$ev5");
                assert!(matches!(
                    *command,
                    MatrixCmd::Send { reply_to: Some(ref id), .. } if id == "$reply2"
                ));
            }
            other => panic!("{other:?}"),
        }

        // Reactions, deleting and loading older replies too.
        app.toggle_reaction(1, "👍");
        app.rooms[0].messages[2].own = true;
        app.delete_message(2);
        app.timeline.want_older = true;
        app.timeline.older_armed = true;
        app.timeline.reached_start = false;
        app.maybe_load_older();
        assert_eq!(
            drain(&mut commands),
            [
                "InThread($ev5, ToggleReaction(!a))",
                "InThread($ev5, Redact(!a))",
                "InThread($ev5, Paginate(!a))",
            ]
        );
        // Typing notices are the room's.
        app.perform(Action::Focus(Pane::Composer));
        app.typing_sent = None;
        type_text(&mut app, "x");
        assert!(matches!(
            commands.try_recv(),
            Ok(MatrixCmd::Typing { typing: true, .. })
        ));
        app.composer.take();

        // No thread inside a thread; a message that is not sent yet cannot start one.
        app.perform(Action::SelectOlder);
        app.on_event(key(KeyCode::Char('t')));
        assert!(screen(&mut app, &mut terminal).contains("already a thread"));
        app.close_thread();
        drain(&mut commands);
        app.rooms[0].messages[7].item = Some(ItemRef::Transaction("txn".into()));
        app.open_thread(7);
        assert!(!app.thread_open());

        // Back in the room, messages go to the room.
        app.perform(Action::Focus(Pane::Composer));
        type_text(&mut app, "hello room");
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).contains(&"Send(!a)".to_owned()));
    }

    #[test]
    fn drafts_of_the_thread_and_the_room_stay_apart() {
        let (mut app, _terminal, _commands) = connected();
        type_text(&mut app, "for the room");
        app.open_thread(5);
        assert_eq!(app.composer.text(), "");
        type_text(&mut app, "for the thread");
        app.close_thread();
        assert_eq!(app.composer.text(), "for the room");
        // Another thread has its own draft; this one finds its text again.
        app.open_thread(6);
        assert_eq!(app.composer.text(), "");
        app.close_thread();
        app.open_thread(5);
        assert_eq!(app.composer.text(), "for the thread");

        // An edit started in the room does not follow into the thread.
        app.close_thread();
        app.rooms[0].messages[9].own = true;
        app.timeline.selected = Some(9);
        app.perform(Action::Edit);
        assert_eq!(app.composer.text(), "line 9");
        app.open_thread(5);
        assert_eq!(app.compose_mode, crate::app::ComposeMode::Normal);
        assert_eq!(app.composer.text(), "for the thread");

        // Switching rooms leaves the thread; each draft stays where it was written.
        app.open_room(1);
        assert!(!app.thread_open());
        assert_eq!(app.composer.text(), "");
        assert_eq!(app.rooms[0].draft, "for the room");
        assert_eq!(app.rooms[0].messages.len(), 40);
        app.open_room(0);
        assert_eq!(app.composer.text(), "for the room");
        app.open_thread(5);
        assert_eq!(app.composer.text(), "for the thread");

        // Clicking the open room in the list is a way back as well.
        app.open_room(0);
        assert!(!app.thread_open());
        assert_eq!(app.composer.text(), "for the room");
        assert_eq!(app.rooms[0].messages.len(), 40);
    }

    #[test]
    fn a_vanished_room_or_session_takes_its_thread_along() {
        use crate::matrix::{RoomCategory, RoomSummary};
        let (mut app, _terminal, _commands) = connected();
        app.open_thread(5);
        app.on_matrix(MatrixEvent::Rooms(vec![RoomSummary {
            id: "!b".into(),
            name: "Beta".into(),
            category: RoomCategory::Room,
            notifications: 0,
            server_notifications: 0,
            mention: false,
            unread: false,
            avatar: None,
            alias: None,
        }]));
        assert!(!app.thread_open());
        assert_eq!(app.rooms[app.active].id, "!b");
        app.close_thread();
        assert!(app.rooms[app.active].messages.is_empty());
    }

    #[test]
    fn the_first_message_keeps_its_place_while_the_thread_loads() {
        let (mut app, mut terminal, mut commands) = connected();
        app.rooms[0].messages[5].own = true;
        app.open_thread(5);
        drain(&mut commands);

        // Selected and being answered before anything of the thread has arrived.
        app.perform(Action::SelectOlder);
        assert_eq!(app.timeline.selected, Some(0));
        app.perform(Action::Reply);
        assert_eq!(app.compose_mode, crate::app::ComposeMode::Reply(0));

        // The newest replies arrive first, without the first message.
        let replies: Vec<Message> = thread_messages().split_off(1);
        app.on_matrix(update(&app, 2, replies));
        assert_eq!(app.rooms[0].messages.len(), 3);
        assert_eq!(app.rooms[0].messages[0].id, ROOT_ID);
        assert_eq!(app.compose_mode, crate::app::ComposeMode::Reply(0));

        // Until the thread holds its first message, the room's timeline acts on it;
        // the other messages are the thread's.
        app.toggle_reaction(0, "👍");
        app.toggle_reaction(1, "👍");
        app.delete_message(0);
        assert_eq!(
            drain(&mut commands),
            [
                "ToggleReaction(!a)",
                "InThread($ev5, ToggleReaction(!a))",
                "Redact(!a)"
            ]
        );
        // A reply to it still belongs to the thread.
        type_text(&mut app, "answer");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(drain(&mut commands), ["InThread($ev5, Send(!a))"]);

        // Editing it, with the emoji picker open for it and the view scrolled, when
        // the thread's own copy takes over: everything stays on it.
        app.timeline.selected = Some(0);
        app.perform(Action::Edit);
        assert_eq!(app.compose_mode, crate::app::ComposeMode::Edit(0));
        assert_eq!(app.composer.text(), "line 5");
        let mut loaded = thread_messages();
        loaded[0].own = true;
        app.on_matrix(update(&app, 2, loaded));
        assert_eq!(app.rooms[0].messages.len(), 3);
        assert_eq!(app.rooms[0].messages[0].id, ROOT_ID);
        assert_eq!(app.compose_mode, crate::app::ComposeMode::Edit(0));
        assert_eq!(app.composer.text(), "line 5");
        screen(&mut app, &mut terminal);

        // From now on the thread's timeline has it.
        app.on_event(key(KeyCode::Enter));
        app.toggle_reaction(0, "👍");
        assert_eq!(
            drain(&mut commands),
            [
                "InThread($ev5, Edit(!a))",
                "InThread($ev5, ToggleReaction(!a))"
            ]
        );

        // A selection on it survives the same change, whatever the thread calls it.
        let (mut app, _terminal, _commands) = connected();
        app.open_thread(5);
        app.on_matrix(update(&app, 2, thread_messages().split_off(1)));
        app.perform(Action::Focus(Pane::Timeline));
        app.timeline.selected = Some(0);
        let mut loaded = thread_messages();
        loaded[0].id = "17".to_owned();
        app.on_matrix(update(&app, 2, loaded));
        assert_eq!(app.timeline.selected, Some(0));
        assert_eq!(app.focus, Pane::Timeline);
    }

    #[test]
    fn deleting_the_first_message_in_the_demo_removes_it_from_the_room() {
        let mut app = App::demo();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let rust = app.rooms.iter().position(|r| r.name == "rust").unwrap();
        app.open_room(rust);
        let count = app.rooms[rust].messages.len();
        // "Good to know, thanks." is ours; the message after it is selected.
        let own = app.rooms[rust].messages.iter().position(|m| m.own).unwrap();
        app.perform(Action::Focus(Pane::Timeline));
        app.timeline.selected = Some(own + 1);
        let after = app.rooms[rust].messages[own + 1].body.clone();
        app.open_thread(own);
        app.delete_message(0);
        app.close_thread();
        assert_eq!(app.rooms[rust].messages.len(), count - 1);
        assert!(app.rooms[rust].messages.iter().all(|m| !m.own));
        // The selection is still on the same message.
        let selected = app.timeline.selected.unwrap();
        assert_eq!(app.rooms[rust].messages[selected].body, after);
        screen(&mut app, &mut terminal);

        // Deleted while it was the selected one: the selection moves next to the gap.
        let mut app = App::demo();
        app.open_room(rust);
        app.perform(Action::Focus(Pane::Timeline));
        app.timeline.selected = Some(own);
        app.open_thread(own);
        app.delete_message(0);
        app.close_thread();
        assert_eq!(app.timeline.selected, Some(own - 1));
    }
}
