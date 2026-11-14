//! What is unread: the "new" line in the open room's messages, and rooms marked as
//! unread (or read) by hand.
//!
//! The line shows where the user had read up to when the room was opened. Where that
//! is comes from the worker: the SDK keeps a read marker in a room's timeline (the
//! account's `m.fully_read`), and the first messages that arrive after opening a room
//! carry it. From then on the line is ours: it stays above the same message until the
//! room is opened again, while the SDK's marker moves on as soon as our own receipt
//! is through.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use super::App;
use crate::action::Pane;
use crate::matrix::MatrixCmd;
use crate::model::{ItemRef, Message, MsgKind, RoomKind};

/// Identity of the line among the open room's messages.
const LINE_ID: &str = "unread-line";

/// How long what the user just did to a room's mark stands against a room list that
/// says otherwise: the list may have been made before the server heard of it.
const WISH_HOLDS: Duration = Duration::from_secs(10);

/// A room that was open before the session's first sync was through gets its line
/// this long after that sync: what was missed while the app was not running arrives
/// in the room's messages a moment after the sync is reported.
const CATCH_UP: Duration = Duration::from_millis(700);

/// Where the SDK's read marker stood in the latest timeline snapshot.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
enum Marker {
    /// Absent (not loaded yet, above loaded history, or already past all events).
    #[default]
    Missing,
    /// Present right before the remote message with this event id.
    Before(String),
    /// Present in the timeline with no visible events after it (e.g. only ignored or
    /// hidden events remain unread).
    AtEnd,
}

/// An unread message identity so the "new" line can step past a sender that is ignored
/// while the room is open.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UnreadTarget {
    id: String,
    sender: Option<String>,
}

/// The "new" line of the open room.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
enum Line {
    /// There is none: nothing was unread, or the room has not been asked for.
    #[default]
    Off,
    /// The room was opened and where the line goes is not decided yet: its
    /// messages have not arrived since, or the session is still catching up.
    Waiting {
        /// The number of notifying messages the room had when it was opened.
        count: u32,
        /// Where the SDK's read marker was in the messages that arrived last.
        marker: Marker,
        /// Opened before the session's first sync was through: the messages that
        /// were there then are those of the last run.
        early: bool,
    },
    /// Above the first surviving message in `targets` (the frozen id, plus later
    /// unread candidates in order).
    At(Vec<UnreadTarget>),
}

#[derive(Debug, Default)]
pub struct Unread {
    line: Line,
    /// Per room, the newest message a receipt was sent for from here. The read
    /// marker lags behind it until the server has answered.
    read: HashMap<String, String>,
    /// Rooms the user just marked as unread (`true`) or took the mark from, and
    /// when: see `WISH_HOLDS`.
    wishes: HashMap<String, (bool, Instant)>,
    /// Room that opened unread or settled with a trailing `Marker::AtEnd`, so a read
    /// receipt is still sent on first view even if zero visible messages remain.
    pending_mark: Option<String>,
    /// Between the login and the first sync of the session.
    catching_up: bool,
    /// When that first sync was through.
    caught_up_at: Option<Instant>,
}

impl Unread {
    /// What to show for a room that the server says is marked (or not): the same,
    /// unless the user changed it a moment ago and the server has not caught up.
    pub(super) fn settle(&mut self, room_id: &str, server: bool) -> bool {
        match self.wishes.get(room_id) {
            Some((wish, at)) if *wish != server && at.elapsed() < WISH_HOLDS => *wish,
            Some(_) => {
                self.wishes.remove(room_id);
                server
            }
            None => server,
        }
    }

    /// The room is gone from the room list.
    pub(super) fn forget(&mut self, room_id: &str) {
        self.read.remove(room_id);
        self.wishes.remove(room_id);
        if self.pending_mark.as_deref() == Some(room_id) {
            self.pending_mark = None;
        }
    }

    /// Logged in: what the worker shows first is what it knew at the end of the
    /// last run.
    pub(super) fn session_begins(&mut self) {
        self.catching_up = true;
        self.caught_up_at = None;
    }

    /// The first sync of the session is through.
    pub(super) fn caught_up(&mut self) {
        if std::mem::take(&mut self.catching_up) {
            self.caught_up_at = Some(Instant::now());
        }
    }

    /// Whether the messages of a room that was opened early can be trusted to
    /// hold what was missed.
    fn settled(&self) -> bool {
        !self.catching_up && self.caught_up_at.is_none_or(|at| at.elapsed() >= CATCH_UP)
    }
}

/// The event id of a message the server has.
fn event_id(message: &Message) -> Option<&str> {
    match &message.item {
        Some(ItemRef::Event(id)) => Some(id),
        _ => None,
    }
}

/// A message of someone else (and not ignored) that can be new to the user.
fn countable(message: &Message, ignored: &BTreeSet<String>) -> bool {
    !message.own
        && !matches!(message.kind, MsgKind::System | MsgKind::ReadMarker)
        && event_id(message).is_some()
        && message
            .sender_id
            .as_deref()
            .is_none_or(|sender| !ignored.contains(sender))
}

fn targets_from(
    messages: &[Message],
    from: usize,
    ignored: &BTreeSet<String>,
) -> Vec<UnreadTarget> {
    messages[from..]
        .iter()
        .filter(|message| countable(message, ignored))
        .filter_map(|message| {
            event_id(message).map(|id| UnreadTarget {
                id: id.to_owned(),
                sender: message.sender_id.clone(),
            })
        })
        .collect()
}

fn prune_ignored_targets(line: &mut Line, ignored: &BTreeSet<String>) {
    if let Line::At(targets) = line {
        targets.retain(|target| {
            target
                .sender
                .as_deref()
                .is_none_or(|sender| !ignored.contains(sender))
        });
        if targets.is_empty() {
            *line = Line::Off;
        }
    }
}

/// The first unread message among `messages` (which hold no marker).
///
/// `marker`: where the SDK's read marker was, as the position of the message after
/// it. Without one (the message read last is not loaded, or was never told to the
/// server) `count` messages of other people at the end are taken as unread, all of
/// them if there are fewer. `read`: the event a receipt was sent for from here;
/// nothing up to it is unread, whatever the (older) marker and count say.
fn first_unread(
    messages: &[Message],
    marker: Option<usize>,
    count: u32,
    read: Option<&str>,
    ignored: &BTreeSet<String>,
) -> Option<usize> {
    let by_count = || {
        messages
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, message)| countable(message, ignored))
            .map(|(at, _)| at)
            .take(count as usize)
            .last()
    };
    let mut from = marker.or_else(by_count)?;
    if let Some(read) = read
        && let Some(at) = messages.iter().rposition(|m| event_id(m) == Some(read))
    {
        from = from.max(at + 1);
    }
    (from..messages.len()).find(|at| countable(&messages[*at], ignored))
}

impl App {
    /// The open room was (re)opened: where its "new" line goes is decided afresh by
    /// the messages that arrive for it next. Call before its counts are cleared.
    pub(super) fn begin_unread_line(&mut self) {
        // The demo has no worker, and its messages are what they are.
        if self.matrix.is_none() {
            return;
        }
        let Some(room) = self.rooms.get_mut(self.active) else {
            self.unread.line = Line::Off;
            self.unread.pending_mark = None;
            return;
        };
        let pending = room.kind != RoomKind::Invite
            && !room.marked_unread
            && (room.unread > 0 || room.bold || room.mention);
        self.unread.pending_mark = pending.then(|| room.id.clone());
        // What is kept from the last visit is shown until the messages arrive; the
        // line of the last visit is not part of that.
        room.messages.retain(|message| message.id != LINE_ID);
        self.unread.line = Line::Waiting {
            count: room.unread,
            marker: Marker::Missing,
            early: self.unread.catching_up,
        };
        self.timeline_rev += 1;
    }

    /// Whether the open room still waits for the messages that say where its "new"
    /// line goes. (A thread has no line; while one is shown the room waits on.)
    pub(super) fn unread_line_pending(&self) -> bool {
        matches!(self.unread.line, Line::Waiting { .. }) && !self.thread_open()
    }

    /// Whether the currently open room opened unread or had a trailing read marker so
    /// `maybe_mark_read` should still clear server unread state if no visible events exist.
    pub(super) fn has_pending_unread_mark(&self) -> bool {
        !self.thread_open()
            && self.unread.pending_mark.as_deref() == self.room_id().as_deref()
            && self
                .room()
                .is_some_and(|r| r.kind != RoomKind::Invite && !r.marked_unread)
    }

    /// A receipt is about to be sent for the open conversation, up to this event.
    pub(super) fn note_read(&mut self, newest: Option<&str>) {
        if self.thread_open() {
            return;
        }
        self.unread.pending_mark = None;
        if let (Some(room_id), Some(newest)) = (self.room_id(), newest) {
            self.unread.read.insert(room_id, newest.to_owned());
        }
        // Reading on takes back a mark set by hand; the worker does so on the server.
        self.unmark_unread(self.active);
    }

    /// Puts the "new" line into the messages of an update for the open room, in
    /// place of the SDK's marker; decides where it goes if that is still open.
    /// A thread gets no line. `detached`: the messages are a slice from somewhere
    /// in the room's history (after a jump to a message), which shows a line that
    /// is placed already but says nothing about where one belongs.
    pub(super) fn place_unread_line(
        &mut self,
        mut messages: Vec<Message>,
        detached: bool,
    ) -> Vec<Message> {
        let saw_marker = match messages
            .iter()
            .position(|message| message.kind == MsgKind::ReadMarker)
        {
            Some(at) => match messages[at..].iter().find_map(event_id) {
                Some(id) => Marker::Before(id.to_owned()),
                None => Marker::AtEnd,
            },
            None => Marker::Missing,
        };
        messages.retain(|message| message.kind != MsgKind::ReadMarker);
        prune_ignored_targets(&mut self.unread.line, &self.ignored_users);
        if self.thread_open() {
            return messages;
        }
        if detached {
            return self.insert_unread_line(messages);
        }
        // The newest word on where the marker is counts.
        if let Line::Waiting { marker, .. } = &mut self.unread.line {
            *marker = saw_marker;
        }
        self.with_unread_line(messages)
    }

    /// Decides where the line goes, if that is open and can be told from these
    /// messages (which hold no marker), and puts the line among them.
    fn with_unread_line(&mut self, messages: Vec<Message>) -> Vec<Message> {
        prune_ignored_targets(&mut self.unread.line, &self.ignored_users);
        if let Line::At(targets) = &mut self.unread.line
            && let Some(first) = targets.first()
            && let Some(at) = messages.iter().position(|m| event_id(m) == Some(&first.id))
        {
            let refreshed = targets_from(&messages, at, &self.ignored_users);
            if refreshed.is_empty() {
                self.unread.line = Line::Off;
            } else {
                *targets = refreshed;
            }
        }
        if let Line::Waiting {
            count,
            marker,
            early,
        } = &self.unread.line
        {
            let settled = !early || self.unread.settled();
            if settled && matches!(marker, Marker::AtEnd) {
                self.unread.line = Line::Off;
                if self
                    .room()
                    .is_some_and(|r| r.kind != RoomKind::Invite && !r.marked_unread)
                {
                    self.unread.pending_mark = self.room_id();
                }
            } else if settled && messages.iter().any(|message| event_id(message).is_some()) {
                let room = self.room();
                let read = room
                    .and_then(|room| self.unread.read.get(&room.id))
                    .map(String::as_str);
                let marker_pos = match marker {
                    Marker::Before(id) => messages.iter().position(|m| event_id(m) == Some(id)),
                    Marker::Missing | Marker::AtEnd => None,
                };
                // A room opened early had the count of the last run then.
                let count = match (early, room) {
                    (true, Some(room)) => room.unread.max(*count),
                    _ => *count,
                };
                let targets = first_unread(&messages, marker_pos, count, read, &self.ignored_users)
                    .map(|at| targets_from(&messages, at, &self.ignored_users))
                    .unwrap_or_default();
                self.unread.line = if targets.is_empty() {
                    Line::Off
                } else {
                    // The view starts at the line, unless the user (or a jump to
                    // a search hit) has already gone somewhere in the meantime.
                    self.timeline.reveal_unread = self.timeline.scroll == 0
                        && self.timeline.selected.is_none()
                        && self.pending_jump.is_none();
                    Line::At(targets)
                };
            }
        }
        self.insert_unread_line(messages)
    }

    /// Re-places or clears the "new" line when the ignored user list changes while a room is open.
    pub(super) fn refresh_unread_line_for_ignored(&mut self) {
        if self.thread_open() {
            return;
        }
        prune_ignored_targets(&mut self.unread.line, &self.ignored_users);
        let Some(room) = self.rooms.get(self.active) else {
            return;
        };
        let mut messages = room.messages.clone();
        messages.retain(|message| message.id != LINE_ID);
        let updated = self.insert_unread_line(messages);
        self.apply_timeline(updated);
    }

    /// Puts the line, if it is placed, above its message among `messages`.
    fn insert_unread_line(&self, mut messages: Vec<Message>) -> Vec<Message> {
        if let Line::At(targets) = &self.unread.line
            && let Some(first) = targets.first()
            && let Some(at) = messages.iter().position(|m| event_id(m) == Some(&first.id))
        {
            messages.insert(
                at,
                Message {
                    id: LINE_ID.to_owned(),
                    kind: MsgKind::ReadMarker,
                    ..Message::default()
                },
            );
        }
        messages
    }

    /// Called now and then: a room that was open before the session had caught up
    /// gets its line once it has, also if no more messages arrive for it.
    pub(super) fn settle_unread_line(&mut self) {
        let empty_at_end = matches!(
            self.unread.line,
            Line::Waiting {
                marker: Marker::AtEnd,
                ..
            }
        );
        if !self.unread_line_pending()
            || !self.unread.settled()
            || (self.messages().is_empty() && !empty_at_end)
            // What is shown is not the room's newest messages, or soon will not be.
            || self.timeline.detached
        {
            return;
        }
        let messages = self.with_unread_line(self.messages().to_vec());
        if self.unread_line_pending() {
            return;
        }
        if matches!(self.unread.line, Line::At(_)) {
            self.apply_timeline(messages);
        }
        // Drawn again: receipts wait for a frame that shows what is read.
        self.dirty = true;
    }

    /// Selects the first message below the "new" line and brings the line into view.
    pub(super) fn jump_to_unread(&mut self) {
        if self.modal.is_some() || self.images.viewer_open() {
            return;
        }
        let line = self
            .messages()
            .iter()
            .position(|message| message.kind == MsgKind::ReadMarker);
        let Some(first) = line.and_then(|line| self.find_selectable(line + 1, true)) else {
            // After a jump to a message the view shows some older part of the
            // room; the line may be elsewhere, or not placed yet.
            self.show_toast(if self.timeline.detached {
                "Go back to the newest messages to see what is unread"
            } else {
                "No unread messages here"
            });
            return;
        };
        self.cancel_pending_jump();
        self.help_open = false;
        self.set_focus(Pane::Timeline);
        self.timeline.selected = Some(first);
        self.timeline.reveal_unread = true;
        self.timeline.reveal_selected = true;
    }

    /// Takes a mark set by hand from the room, here only: for when the worker is
    /// told something that makes it take the mark back on the server.
    pub(super) fn unmark_unread(&mut self, index: usize) -> bool {
        let Some(room) = self.rooms.get_mut(index) else {
            return false;
        };
        if !std::mem::take(&mut room.marked_unread) {
            return false;
        }
        self.unread
            .wishes
            .insert(room.id.clone(), (false, Instant::now()));
        true
    }

    /// The room is being read or written in: a mark set by hand goes, here and on
    /// the server.
    pub(super) fn clear_marked_unread(&mut self, index: usize) {
        if self.unmark_unread(index)
            && let Some(matrix) = &self.matrix
        {
            // About the room, also while one of its threads is open.
            matrix.send(MatrixCmd::SetUnread {
                room_id: self.rooms[index].id.clone(),
                unread: false,
            });
        }
    }

    /// Marks a room as unread by hand: it looks unread in the room list (and in the
    /// user's other clients) until it is opened, marked as read or written in.
    pub(super) fn mark_room_unread(&mut self, index: usize) {
        let Some(room) = self.rooms.get_mut(index) else {
            return;
        };
        if room.kind == RoomKind::Invite {
            return;
        }
        room.marked_unread = true;
        self.unread
            .wishes
            .insert(room.id.clone(), (true, Instant::now()));
        if let Some(matrix) = &self.matrix {
            matrix.send(MatrixCmd::SetUnread {
                room_id: room.id.clone(),
                unread: true,
            });
        }
    }

    /// The open room: read if it shows as unread, unread if it does not.
    pub(super) fn toggle_unread(&mut self) {
        let Some(room) = self.room() else {
            return;
        };
        if room.kind == RoomKind::Invite {
            return self.show_toast("Accept the invitation first");
        }
        if room.shows_unread() {
            self.mark_room_read(self.active);
            self.show_toast("Marked as read");
        } else {
            self.mark_room_unread(self.active);
            self.show_toast("Marked as unread");
        }
    }

    /// The user said that the room at `index` is read. If it is the open one and its
    /// "new" line is still to be decided, there will be none: the read marker that
    /// arrives with its messages may be the old one, from before the server heard.
    pub(super) fn no_unread_line_after_marking_read(&mut self, index: usize) {
        if index == self.active && matches!(self.unread.line, Line::Waiting { .. }) {
            self.unread.line = Line::Off;
        }
        if self
            .rooms
            .get(index)
            .is_some_and(|r| self.unread.pending_mark.as_deref() == Some(r.id.as_str()))
        {
            self.unread.pending_mark = None;
        }
    }

    /// Marks every room that shows as unread as read.
    pub(super) fn mark_all_read(&mut self) {
        let unread: Vec<usize> = (0..self.rooms.len())
            .filter(|at| {
                let room = &self.rooms[*at];
                room.kind != RoomKind::Invite && room.shows_unread()
            })
            .collect();
        // The open room counts as read since it was opened, so it is not in that
        // list; its line, if still to be decided, is called off all the same.
        self.no_unread_line_after_marking_read(self.active);
        if unread.is_empty() {
            return self.show_toast("Nothing is unread");
        }
        let mut room_ids = Vec::with_capacity(unread.len());
        for at in unread {
            self.unmark_unread(at);
            let room = &mut self.rooms[at];
            // Shown at once; the next room list from the server has the last word.
            room.unread = 0;
            room.bold = false;
            room.mention = false;
            room_ids.push(room.id.clone());
        }
        let text = match room_ids.len() {
            1 => "Marked 1 room as read".to_owned(),
            n => format!("Marked {n} rooms as read"),
        };
        if let Some(matrix) = &self.matrix {
            matrix.send(MatrixCmd::MarkRoomsRead {
                room_ids,
                public: self.read_receipts,
            });
        }
        self.show_toast(text);
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::action::Action;
    use crate::matrix::{MatrixEvent, MatrixHandle, RoomCategory, RoomSummary};
    use crate::model::Room;

    fn message(n: usize, own: bool) -> Message {
        Message {
            id: format!("m{n}"),
            item: Some(ItemRef::Event(format!("$e{n}"))),
            sender: if own { "me" } else { "ann" }.to_owned(),
            body: format!("text {n}"),
            day: "Today".to_owned(),
            time: "10:00".to_owned(),
            own,
            ..Message::default()
        }
    }

    /// Messages `from..to` of someone else.
    fn others(range: std::ops::Range<usize>) -> Vec<Message> {
        range.map(|n| message(n, false)).collect()
    }

    /// The SDK's read marker, as the worker sends it.
    fn marker() -> Message {
        Message {
            id: "__read_marker".to_owned(),
            kind: MsgKind::ReadMarker,
            ..Message::default()
        }
    }

    fn with_marker(mut messages: Vec<Message>, before: usize) -> Vec<Message> {
        messages.insert(before, marker());
        messages
    }

    fn listed(id: &str, name: &str) -> RoomSummary {
        RoomSummary {
            id: id.to_owned(),
            name: name.to_owned(),
            category: RoomCategory::Room,
            notifications: 0,
            preview: None,
            server_notifications: 0,
            mention: false,
            unread: false,
            marked_unread: false,
            avatar: None,
            alias: None,
            direct: false,
            muted: false,
        }
    }

    fn unread(id: &str, name: &str, count: u64) -> RoomSummary {
        RoomSummary {
            notifications: count,
            unread: true,
            ..listed(id, name)
        }
    }

    fn marked(id: &str, name: &str) -> RoomSummary {
        RoomSummary {
            marked_unread: true,
            ..listed(id, name)
        }
    }

    /// Connected to a fake worker, with two rooms (`!a` Alpha, which is open, and
    /// `!b` Beta) and the window in front.
    fn connected() -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let mut app = App::demo();
        let (handle, mut commands) = MatrixHandle::for_test();
        app.matrix = Some(handle);
        app.rooms = vec![
            Room::new("!a", "Alpha", RoomKind::Room),
            Room::new("!b", "Beta", RoomKind::Room),
        ];
        app.active = 0;
        app.perform(Action::OpenRoom(0));
        let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        while commands.try_recv().is_ok() {}
        (app, terminal, commands)
    }

    /// Draws a frame and does what follows a draw (receipts above all).
    fn draw(app: &mut App, terminal: &mut Terminal<TestBackend>) -> String {
        terminal.draw(|f| crate::ui::draw(f, app)).unwrap();
        app.after_draw();
        format!("{}", terminal.backend())
    }

    fn sent(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            seen.push(format!("{command:?}"));
        }
        seen
    }

    /// The worker's messages for the open room.
    fn arrive(app: &mut App, messages: Vec<Message>) {
        let room_id = app.room_id().unwrap();
        app.on_matrix(MatrixEvent::Timeline {
            room_id,
            generation: app.generation,
            messages,
            reached_start: true,
            detached: None,
        });
    }

    /// What the open room shows, the line as `--`.
    fn shown(app: &App) -> Vec<&str> {
        app.messages()
            .iter()
            .map(|message| match message.kind {
                MsgKind::ReadMarker => "--",
                _ => message.id.as_str(),
            })
            .collect()
    }

    /// Whether the room list shows that room in bold, as it does unread rooms.
    fn in_bold(app: &mut App, terminal: &mut Terminal<TestBackend>, name: &str) -> bool {
        let text = draw(app, terminal);
        let (row, line) = text
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains(name))
            .unwrap();
        let column = line[..line.find(name).unwrap()].chars().count();
        // The first line of the text is the frame's top border quote: rows match.
        let cell = &terminal.backend().buffer()[(column as u16 - 1, row as u16)];
        assert_eq!(cell.symbol(), &name[..1], "{text}");
        cell.modifier.contains(ratatui::style::Modifier::BOLD)
    }

    fn press(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        app.on_event(Event::Key(KeyEvent::new(code, mods)));
    }

    fn palette_titles(app: &mut App) -> Vec<String> {
        app.perform(Action::OpenPalette);
        let titles = match &app.modal {
            Some(crate::modal::Modal::Palette(palette)) => (0..)
                .map_while(|at| palette.entry_at(at))
                .map(|entry| entry.title.clone())
                .collect(),
            _ => panic!("no palette"),
        };
        app.modal = None;
        titles
    }

    #[test]
    fn the_first_unread_message_by_marker_by_count_and_by_what_was_read() {
        let all = others(0..5);
        let ignored = BTreeSet::new();
        // Where the marker was.
        assert_eq!(first_unread(&all, Some(3), 0, None, &ignored), Some(3));
        // Our own messages and system lines after it are not new to us.
        let mut mixed = all.clone();
        mixed[2].own = true;
        mixed[3].kind = MsgKind::System;
        assert_eq!(first_unread(&mixed, Some(2), 0, None, &ignored), Some(4));
        assert_eq!(first_unread(&mixed[..4], Some(2), 0, None, &ignored), None);
        // Without a marker the count says how many of the last are new; no more
        // than there are.
        assert_eq!(first_unread(&all, None, 2, None, &ignored), Some(3));
        assert_eq!(first_unread(&mixed, None, 2, None, &ignored), Some(1));
        assert_eq!(first_unread(&all, None, 9, None, &ignored), Some(0));
        assert_eq!(first_unread(&all, None, 0, None, &ignored), None);
        // A receipt we sent ourselves beats an older marker and a stale count.
        assert_eq!(
            first_unread(&all, Some(1), 0, Some("$e2"), &ignored),
            Some(3)
        );
        assert_eq!(first_unread(&all, None, 4, Some("$e3"), &ignored), Some(4));
        assert_eq!(first_unread(&all, Some(1), 3, Some("$e4"), &ignored), None);
        // One for a message that is not loaded says nothing.
        assert_eq!(
            first_unread(&all, Some(1), 0, Some("$gone"), &ignored),
            Some(1)
        );
    }

    #[test]
    fn the_line_is_placed_once_when_the_room_is_opened_and_stays() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::Rooms(vec![
            listed("!a", "Alpha"),
            unread("!b", "Beta", 2),
        ]));
        // What is kept of Beta from an earlier visit is shown until the worker's
        // messages arrive. No receipt goes out for that: it would move the marker
        // the line is read from.
        app.rooms[1].messages = others(0..2);
        app.perform(Action::OpenRoom(1));
        draw(&mut app, &mut terminal);
        assert_eq!(
            sent(&mut commands),
            ["FetchMembers(!b)", "OpenRoom(!b)", "Paginate(!b)"]
        );
        assert_eq!(shown(&app), ["m0", "m1"]);

        // The messages, with the SDK's marker after what was read.
        arrive(&mut app, with_marker(others(0..4), 2));
        assert_eq!(shown(&app), ["m0", "m1", "--", "m2", "m3"]);
        let text = draw(&mut app, &mut terminal);
        assert!(text.contains("─ new ─"), "{text}");
        // All of it fits: the newest is at the bottom and counts as read.
        assert_eq!(app.timeline.scroll, 0);
        assert_eq!(sent(&mut commands), ["MarkRead(!b)"]);

        // The receipt went out and the SDK's marker has moved to the end, where
        // the worker leaves it out: our line stays.
        arrive(&mut app, others(0..4));
        assert_eq!(shown(&app), ["m0", "m1", "--", "m2", "m3"]);
        // New messages arrive, and the marker turns up somewhere else.
        arrive(&mut app, with_marker(others(0..6), 4));
        assert_eq!(shown(&app), ["m0", "m1", "--", "m2", "m3", "m4", "m5"]);
        // Older messages are loaded above it.
        let mut longer = vec![message(90, false), message(91, true)];
        longer.extend(others(0..6));
        arrive(&mut app, longer);
        assert_eq!(app.messages()[4].kind, MsgKind::ReadMarker);
        assert_eq!(app.messages()[5].id, "m2");
        // The message it stands above is not loaded for a moment (the timeline was
        // reset): no line, and it is back with the message.
        arrive(&mut app, others(4..6));
        assert_eq!(shown(&app), ["m4", "m5"]);
        arrive(&mut app, others(0..6));
        assert_eq!(shown(&app)[2], "--");

        // Opened again with nothing unread: no line, not even for a moment.
        draw(&mut app, &mut terminal);
        sent(&mut commands);
        app.perform(Action::OpenRoom(1));
        assert!(!shown(&app).contains(&"--"));
        arrive(&mut app, others(0..6));
        assert_eq!(shown(&app), ["m0", "m1", "m2", "m3", "m4", "m5"]);
        assert!(!draw(&mut app, &mut terminal).contains("─ new ─"));
    }

    #[test]
    fn a_marker_that_lags_behind_our_own_receipt_is_not_taken() {
        let (mut app, mut terminal, mut commands) = connected();
        arrive(&mut app, others(0..3));
        draw(&mut app, &mut terminal);
        assert_eq!(sent(&mut commands), ["MarkRead(!a)"]);

        // Away and back before the server has answered: the marker still stands
        // where it stood, but all of this was read here a moment ago.
        app.perform(Action::OpenRoom(1));
        app.perform(Action::OpenRoom(0));
        arrive(&mut app, with_marker(others(0..3), 1));
        assert_eq!(shown(&app), ["m0", "m1", "m2"]);

        // The same with something new below what was read: the line is there.
        app.perform(Action::OpenRoom(1));
        app.perform(Action::OpenRoom(0));
        arrive(&mut app, with_marker(others(0..5), 1));
        assert_eq!(shown(&app), ["m0", "m1", "m2", "--", "m3", "m4"]);
    }

    #[test]
    fn without_a_marker_the_count_at_opening_places_the_line() {
        let (mut app, _terminal, _commands) = connected();
        app.on_matrix(MatrixEvent::Rooms(vec![
            listed("!a", "Alpha"),
            unread("!b", "Beta", 2),
        ]));
        app.perform(Action::OpenRoom(1));
        // The message read last is not among the loaded ones, so the SDK has no
        // place for its marker.
        let mut messages = others(0..4);
        messages.push(message(4, true));
        arrive(&mut app, messages);
        assert_eq!(shown(&app), ["m0", "m1", "--", "m2", "m3", "m4"]);

        // A list that arrives after opening does not move it, whatever it counts.
        app.on_matrix(MatrixEvent::Rooms(vec![
            listed("!a", "Alpha"),
            unread("!b", "Beta", 4),
        ]));
        arrive(&mut app, others(0..4));
        assert_eq!(shown(&app), ["m0", "m1", "--", "m2", "m3"]);
    }

    #[test]
    fn the_first_messages_decide_also_when_they_come_late() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::Rooms(vec![
            listed("!a", "Alpha"),
            unread("!b", "Beta", 1),
        ]));
        app.perform(Action::OpenRoom(1));
        sent(&mut commands);
        // Nothing loaded yet: nothing is decided, and nothing is read.
        arrive(&mut app, Vec::new());
        draw(&mut app, &mut terminal);
        assert!(
            sent(&mut commands)
                .iter()
                .all(|c| !c.starts_with("MarkRead"))
        );
        // An update for the room as it was asked for earlier changes nothing.
        app.on_matrix(MatrixEvent::Timeline {
            room_id: "!b".to_owned(),
            generation: app.generation - 1,
            messages: with_marker(others(0..3), 1),
            reached_start: true,
            detached: None,
        });
        assert!(app.messages().is_empty());
        arrive(&mut app, with_marker(others(0..3), 2));
        assert_eq!(shown(&app), ["m0", "m1", "--", "m2"]);
    }

    #[test]
    fn marking_the_open_room_read_before_its_messages_arrive_leaves_no_line() {
        let (mut app, _terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::Rooms(vec![
            listed("!a", "Alpha"),
            unread("!b", "Beta", 2),
        ]));
        app.perform(Action::OpenRoom(1));
        sent(&mut commands);
        // Said to be read while nothing is loaded; the messages then come with the
        // read marker of before.
        app.mark_room_read(1);
        arrive(&mut app, with_marker(others(0..3), 1));
        assert_eq!(shown(&app), ["m0", "m1", "m2"]);
        // The same through "mark all".
        app.on_matrix(MatrixEvent::Rooms(vec![
            unread("!a", "Alpha", 1),
            listed("!b", "Beta"),
        ]));
        app.perform(Action::OpenRoom(0));
        app.mark_all_read();
        arrive(&mut app, with_marker(others(0..3), 1));
        assert_eq!(shown(&app), ["m0", "m1", "m2"]);
    }

    #[test]
    fn a_room_open_when_the_session_starts_waits_for_what_was_missed() {
        let login = || MatrixEvent::LoggedIn {
            user_id: "@me:example.org".to_owned(),
        };
        let reads = |commands: &mut UnboundedReceiver<MatrixCmd>| {
            sent(commands)
                .into_iter()
                .filter(|command| command.starts_with("MarkRead"))
                .count()
        };
        // The session begins with what the last run knew: the room that is open
        // then shows the messages of that run, all read.
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(login());
        app.perform(Action::OpenRoom(0));
        arrive(&mut app, others(0..3));
        draw(&mut app, &mut terminal);
        assert_eq!(shown(&app), ["m0", "m1", "m2"]);
        // Nothing is decided, and nothing counts as read.
        assert_eq!(reads(&mut commands), 0);
        app.on_tick();
        assert!(app.unread_line_pending());

        // The first sync is through; what was missed arrives a moment later, and
        // the marker is where the user had read up to.
        app.on_matrix(MatrixEvent::CaughtUp);
        app.on_matrix(MatrixEvent::Rooms(vec![
            unread("!a", "Alpha", 2),
            listed("!b", "Beta"),
        ]));
        arrive(&mut app, with_marker(others(0..5), 3));
        app.on_tick();
        draw(&mut app, &mut terminal);
        assert_eq!(shown(&app), ["m0", "m1", "m2", "m3", "m4"]);
        assert_eq!(reads(&mut commands), 0);
        // That moment over, the line is placed, and the room is read from there.
        app.unread.caught_up_at = Some(Instant::now() - CATCH_UP);
        app.dirty = false;
        app.on_tick();
        assert!(app.dirty);
        assert_eq!(shown(&app), ["m0", "m1", "m2", "--", "m3", "m4"]);
        draw(&mut app, &mut terminal);
        assert_eq!(reads(&mut commands), 1);
        // Later ticks change nothing.
        app.dirty = false;
        app.on_tick();
        assert!(!app.dirty);

        // Without a marker among the messages, the count of the first sync does it
        // (the count at opening was the last run's).
        let (mut app, _terminal, _commands) = connected();
        app.on_matrix(login());
        app.perform(Action::OpenRoom(0));
        arrive(&mut app, others(0..3));
        app.on_matrix(MatrixEvent::CaughtUp);
        app.on_matrix(MatrixEvent::Rooms(vec![
            unread("!a", "Alpha", 2),
            listed("!b", "Beta"),
        ]));
        app.unread.caught_up_at = Some(Instant::now() - CATCH_UP);
        arrive(&mut app, others(0..5));
        assert_eq!(shown(&app), ["m0", "m1", "m2", "--", "m3", "m4"]);

        // Nothing was missed, and no more messages arrive: no line, and the room
        // is read as soon as that is known.
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(login());
        app.perform(Action::OpenRoom(0));
        arrive(&mut app, others(0..3));
        app.on_matrix(MatrixEvent::CaughtUp);
        draw(&mut app, &mut terminal);
        assert_eq!(reads(&mut commands), 0);
        app.unread.caught_up_at = Some(Instant::now() - CATCH_UP);
        app.on_tick();
        draw(&mut app, &mut terminal);
        assert_eq!(shown(&app), ["m0", "m1", "m2"]);
        assert_eq!(reads(&mut commands), 1);

        // A room opened once the first sync is through shows what is known now:
        // decided at once.
        let (mut app, _terminal, _commands) = connected();
        app.on_matrix(login());
        app.on_matrix(MatrixEvent::CaughtUp);
        app.perform(Action::OpenRoom(1));
        arrive(&mut app, with_marker(others(0..3), 1));
        assert_eq!(shown(&app), ["m0", "--", "m1", "m2"]);

        // Never online: nothing is decided and nothing is claimed to be read.
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(login());
        app.perform(Action::OpenRoom(0));
        arrive(&mut app, with_marker(others(0..3), 1));
        app.on_tick();
        draw(&mut app, &mut terminal);
        assert_eq!(shown(&app), ["m0", "m1", "m2"]);
        assert_eq!(reads(&mut commands), 0);
    }

    /// A slice of the open room's history around `$e{target}`, as it arrives for
    /// a jump with this generation.
    fn slice(
        app: &App,
        generation: u64,
        room: &str,
        messages: Vec<Message>,
        target: usize,
    ) -> MatrixEvent {
        let _ = app;
        MatrixEvent::Timeline {
            room_id: room.to_owned(),
            generation,
            messages,
            reached_start: false,
            detached: Some(crate::matrix::DetachedTimeline {
                target_event: format!("$e{target}"),
                thread_root: None,
                reached_end: false,
            }),
        }
    }

    /// Jumps to `$e{target}` in `room`; the generation the answer has to carry.
    fn jump(
        app: &mut App,
        commands: &mut UnboundedReceiver<MatrixCmd>,
        room: &str,
        target: usize,
    ) -> u64 {
        while commands.try_recv().is_ok() {}
        app.jump_to_event(room.to_owned(), format!("$e{target}"), None);
        match commands.try_recv() {
            Ok(MatrixCmd::OpenEvent { generation, .. }) => generation,
            other => panic!("{other:?}"),
        }
    }

    fn reads(commands: &mut UnboundedReceiver<MatrixCmd>) -> usize {
        sent(commands)
            .into_iter()
            .filter(|command| command.starts_with("MarkRead"))
            .count()
    }

    #[test]
    fn a_slice_of_older_messages_neither_places_the_line_nor_reads_anything() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::Rooms(vec![
            listed("!a", "Alpha"),
            unread("!b", "Beta", 2),
        ]));
        // A jump into the other room, to a message far back: the room is entered
        // with a slice of its history. Whatever marker and count say, that is not
        // where the line is decided, and nothing in it counts as read.
        let generation = jump(&mut app, &mut commands, "!b", 91);
        app.on_matrix(slice(
            &app,
            generation,
            "!b",
            with_marker(others(90..94), 1),
            91,
        ));
        assert_eq!(app.active, 1);
        assert!(app.timeline.detached);
        assert_eq!(shown(&app), ["m90", "m91", "m92", "m93"]);
        assert_eq!(app.messages()[app.timeline.selected.unwrap()].id, "m91");
        draw(&mut app, &mut terminal);
        app.on_tick();
        // More of the slice arrives.
        app.on_matrix(slice(
            &app,
            generation,
            "!b",
            with_marker(others(90..96), 3),
            91,
        ));
        draw(&mut app, &mut terminal);
        assert!(!shown(&app).contains(&"--"));
        assert!(app.unread_line_pending());
        assert_eq!(reads(&mut commands), 0);
        // In there, there is no unread line to go to; that is said.
        press(&mut app, KeyCode::Char('u'), KeyModifiers::ALT);
        assert_eq!(
            app.toast.as_ref().unwrap().0,
            "Go back to the newest messages to see what is unread"
        );
        assert!(app.timeline.detached);

        // Back at the newest messages the line is placed by them, as when the room
        // is opened, and from there on the room is read.
        app.perform(Action::JumpToNewest);
        assert!(!app.timeline.detached);
        assert_eq!(reads(&mut commands), 0);
        arrive(&mut app, with_marker(others(0..4), 2));
        assert_eq!(shown(&app)[2], "--");
        // The selection the user came back with is not moved to the line.
        assert!(!app.timeline.reveal_unread);
        draw(&mut app, &mut terminal);
        assert_eq!(reads(&mut commands), 1);

        // A jump within the room leaves the line where it is: it shows in the
        // slice if its message is there, and is back with the newest messages.
        let generation = jump(&mut app, &mut commands, "!b", 91);
        app.on_matrix(slice(&app, generation, "!b", others(90..94), 91));
        assert!(app.timeline.detached);
        assert!(!shown(&app).contains(&"--") && !app.unread_line_pending());
        let generation = jump(&mut app, &mut commands, "!b", 80);
        let mut around = others(80..82);
        around.extend(others(1..4));
        app.on_matrix(slice(&app, generation, "!b", around, 80));
        assert_eq!(shown(&app), ["m80", "m81", "m1", "--", "m2", "m3"]);
        // There the unread line can be gone to.
        app.toast = None;
        press(&mut app, KeyCode::Char('u'), KeyModifiers::ALT);
        assert!(app.toast.is_none());
        assert_eq!(app.messages()[app.timeline.selected.unwrap()].id, "m2");
        app.perform(Action::JumpToNewest);
        arrive(&mut app, others(0..5));
        assert_eq!(shown(&app), ["m0", "m1", "--", "m2", "m3", "m4"]);
    }

    #[test]
    fn a_jump_into_another_rooms_newest_messages_places_the_line_and_keeps_its_target() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::Rooms(vec![
            listed("!a", "Alpha"),
            unread("!b", "Beta", 40),
        ]));
        // The message jumped to is among the room's newest: the worker answers
        // with those, marker and all. Entering the room this way is opening it.
        let generation = jump(&mut app, &mut commands, "!b", 55);
        app.on_matrix(MatrixEvent::Timeline {
            room_id: "!b".to_owned(),
            generation,
            messages: with_marker(others(0..60), 10),
            reached_start: true,
            detached: None,
        });
        assert_eq!(app.active, 1);
        assert_eq!(app.generation, generation);
        assert_eq!(shown(&app)[10], "--");
        // The view is at the message jumped to, not at the line.
        assert!(!app.timeline.reveal_unread);
        assert_eq!(app.messages()[app.timeline.selected.unwrap()].id, "m55");
        let text = draw(&mut app, &mut terminal);
        assert!(
            text.contains("text 55") && !text.contains("─ new ─"),
            "{text}"
        );
        // Later updates keep the line, and Alt-U goes to it.
        arrive(&mut app, others(0..61));
        assert_eq!(shown(&app)[10], "--");
        press(&mut app, KeyCode::Char('u'), KeyModifiers::ALT);
        assert_eq!(app.messages()[app.timeline.selected.unwrap()].id, "m10");
        let text = draw(&mut app, &mut terminal);
        assert!(text.contains("─ new ─"), "{text}");
    }

    #[test]
    fn many_unread_messages_start_at_the_line_and_are_not_read_yet() {
        let (mut app, mut terminal, mut commands) = connected();
        app.perform(Action::OpenRoom(1));
        sent(&mut commands);
        arrive(&mut app, with_marker(others(0..60), 10));
        let text = draw(&mut app, &mut terminal);
        // The line is the first row of the messages; the newest is out of view.
        let rows: Vec<&str> = text.lines().collect();
        let line = rows.iter().position(|row| row.contains("─ new ─")).unwrap();
        assert!(rows[line - 1].contains("Beta"), "{text}");
        assert!(
            text.contains("text 10") && !text.contains("text 59"),
            "{text}"
        );
        assert!(app.timeline.scroll > 0);
        // Not at the newest message: nothing counts as read.
        assert_eq!(sent(&mut commands), Vec::<String>::new());
        // More arrive: the view stays where it is.
        arrive(&mut app, others(0..62));
        let text = draw(&mut app, &mut terminal);
        assert!(
            text.contains("─ new ─") && text.contains("text 10"),
            "{text}"
        );
        assert_eq!(sent(&mut commands), Vec::<String>::new());
        // Down at the newest, it is read.
        app.perform(Action::JumpToNewest);
        draw(&mut app, &mut terminal);
        assert_eq!(sent(&mut commands), ["MarkRead(!b)"]);
    }

    #[test]
    fn jumping_to_the_first_unread_message() {
        let (mut app, mut terminal, mut commands) = connected();
        // Nothing unread: a word about it, and nothing moves.
        arrive(&mut app, others(0..3));
        draw(&mut app, &mut terminal);
        press(&mut app, KeyCode::Char('u'), KeyModifiers::ALT);
        assert_eq!(app.toast.as_ref().unwrap().0, "No unread messages here");
        assert_eq!((app.focus, app.timeline.selected), (Pane::Composer, None));

        app.perform(Action::OpenRoom(1));
        arrive(&mut app, with_marker(others(0..60), 10));
        draw(&mut app, &mut terminal);
        // Scrolled away from it, to the newest message.
        app.perform(Action::JumpToNewest);
        let text = draw(&mut app, &mut terminal);
        assert!(!text.contains("─ new ─"));
        sent(&mut commands);

        app.toast = None;
        press(&mut app, KeyCode::Char('u'), KeyModifiers::ALT);
        let text = draw(&mut app, &mut terminal);
        assert!(app.toast.is_none());
        assert_eq!(app.focus, Pane::Timeline);
        // The message below the line is selected, and the line is in view.
        assert_eq!(app.messages()[app.timeline.selected.unwrap()].id, "m10");
        // The line itself cannot be selected: moving up steps over it.
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.messages()[app.timeline.selected.unwrap()].id, "m9");
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert!(
            text.contains("─ new ─") && text.contains("text 10"),
            "{text}"
        );

        // In a thread there is no line.
        app.perform(Action::OpenThread);
        assert!(app.thread_open());
        app.perform(Action::JumpToUnread);
        assert_eq!(app.toast.as_ref().unwrap().0, "No unread messages here");
        // From the command palette, back in the room.
        app.perform(Action::CloseThread);
        arrive(&mut app, others(0..60));
        app.perform(Action::JumpToNewest);
        app.perform(Action::Focus(Pane::Composer));
        app.perform(Action::OpenPalette);
        for c in "first unread".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.modal.is_none());
        assert_eq!(app.messages()[app.timeline.selected.unwrap()].id, "m10");
    }

    #[test]
    fn a_thread_has_no_line_and_the_rooms_line_survives_it() {
        let (mut app, mut terminal, mut commands) = connected();
        app.perform(Action::OpenRoom(1));
        arrive(&mut app, with_marker(others(0..4), 2));
        draw(&mut app, &mut terminal);
        app.timeline.selected = Some(0);
        app.perform(Action::OpenThread);
        // The thread's own timeline may carry the SDK's marker; it is not shown.
        let mut replies = with_marker(others(10..13), 1);
        replies.insert(0, message(0, false));
        arrive(&mut app, replies);
        assert!(!shown(&app).contains(&"--"));
        draw(&mut app, &mut terminal);
        // Back in the room the line is where it was, though the marker has moved.
        app.perform(Action::CloseThread);
        arrive(&mut app, others(0..5));
        assert_eq!(shown(&app), ["m0", "m1", "--", "m2", "m3", "m4"]);
        sent(&mut commands);
    }

    #[test]
    fn marking_by_hand_holds_against_a_list_made_before_the_server_knew() {
        let (mut app, mut terminal, mut commands) = connected();
        let before = vec![listed("!a", "Alpha"), listed("!b", "Beta")];
        let after = vec![listed("!a", "Alpha"), marked("!b", "Beta")];
        app.on_matrix(MatrixEvent::Rooms(before.clone()));
        app.mark_room_unread(1);
        assert_eq!(sent(&mut commands), ["SetUnread(!b, true)"]);
        assert!(app.rooms[1].marked_unread && app.rooms[1].shows_unread());
        // It looks like a room with unread messages; no count comes with it.
        assert!(in_bold(&mut app, &mut terminal, "Beta"));
        assert_eq!(app.rooms[1].unread, 0);
        // A room list from before the server heard of it does not take it back.
        app.on_matrix(MatrixEvent::Rooms(before.clone()));
        assert!(app.rooms[1].marked_unread);
        // The server agrees; from then on it has the say again.
        app.on_matrix(MatrixEvent::Rooms(after.clone()));
        assert!(app.rooms[1].marked_unread);
        app.on_matrix(MatrixEvent::Rooms(before.clone()));
        assert!(!app.rooms[1].shows_unread());
        assert!(!in_bold(&mut app, &mut terminal, "Beta"));
        sent(&mut commands);

        // Marked in another client; opening the room takes the mark back, also
        // against a list that still has it.
        app.on_matrix(MatrixEvent::Rooms(after.clone()));
        assert!(app.rooms[1].shows_unread());
        app.perform(Action::OpenRoom(1));
        assert_eq!(
            sent(&mut commands),
            ["SetUnread(!b, false)", "FetchMembers(!b)", "OpenRoom(!b)"]
        );
        app.on_matrix(MatrixEvent::Rooms(after.clone()));
        assert!(!app.rooms[1].shows_unread());
        app.on_matrix(MatrixEvent::Rooms(before.clone()));
        // ...and after that, the mark of another client counts again.
        app.on_matrix(MatrixEvent::Rooms(after));
        assert!(app.rooms[1].marked_unread);
        assert_eq!(sent(&mut commands), Vec::<String>::new());

        // What the user wanted does not stand for ever against the server.
        app.mark_room_unread(0);
        app.unread.wishes.get_mut("!a").unwrap().1 = Instant::now() - WISH_HOLDS;
        app.on_matrix(MatrixEvent::Rooms(before));
        assert!(!app.rooms[0].marked_unread);
    }

    #[test]
    fn a_mark_that_arrives_for_the_open_room_stays_until_it_is_read_or_written_in() {
        let (mut app, mut terminal, mut commands) = connected();
        arrive(&mut app, with_marker(others(0..3), 1));
        draw(&mut app, &mut terminal);
        assert_eq!(sent(&mut commands), ["MarkRead(!a)"]);
        assert_eq!(shown(&app), ["m0", "--", "m1", "m2"]);

        // Marked as unread in another client while we have the room open: it shows
        // as unread, nothing is sent, and the line stays where it is.
        let list = vec![marked("!a", "Alpha"), listed("!b", "Beta")];
        app.on_matrix(MatrixEvent::Rooms(list.clone()));
        draw(&mut app, &mut terminal);
        assert!(app.rooms[0].marked_unread && app.rooms[0].shows_unread());
        assert_eq!(shown(&app), ["m0", "--", "m1", "m2"]);
        assert_eq!(sent(&mut commands), Vec::<String>::new());

        // The next message read here takes it back (the worker does, with the
        // receipt), also against a list that still has the mark.
        arrive(&mut app, others(0..4));
        draw(&mut app, &mut terminal);
        assert_eq!(sent(&mut commands), ["MarkRead(!a)"]);
        assert!(!app.rooms[0].marked_unread);
        app.on_matrix(MatrixEvent::Rooms(list));
        assert!(!app.rooms[0].marked_unread);

        // Marked here, by the palette, for the open room; writing takes it back.
        let titles = palette_titles(&mut app);
        assert!(titles.iter().any(|t| t == "Mark this room as unread"));
        assert!(titles.iter().any(|t| t == "Mark all rooms as read"));
        app.perform(Action::ToggleUnread);
        assert_eq!(app.toast.as_ref().unwrap().0, "Marked as unread");
        assert_eq!(sent(&mut commands), ["SetUnread(!a, true)"]);
        draw(&mut app, &mut terminal);
        assert_eq!(sent(&mut commands), Vec::<String>::new());
        app.composer.set_text("hello");
        app.perform(Action::Send);
        assert_eq!(
            sent(&mut commands),
            ["Send(!a)", "SetUnread(!a, false)", "SaveDraft(!a, 0 bytes)"]
        );
        assert!(!app.rooms[0].shows_unread());

        // The same entry marks a room that shows as unread as read.
        app.perform(Action::ToggleUnread);
        sent(&mut commands);
        let titles = palette_titles(&mut app);
        assert!(titles.iter().any(|t| t == "Mark this room as read"));
        app.perform(Action::ToggleUnread);
        assert_eq!(app.toast.as_ref().unwrap().0, "Marked as read");
        assert_eq!(sent(&mut commands), ["MarkRead(!a)"]);
        assert!(!app.rooms[0].shows_unread());
    }

    #[test]
    fn marking_everything_as_read() {
        let (mut app, _terminal, mut commands) = connected();
        // Nothing to do: said so, and nothing is sent.
        app.perform(Action::MarkAllRead);
        assert_eq!(app.toast.as_ref().unwrap().0, "Nothing is unread");
        assert_eq!(sent(&mut commands), Vec::<String>::new());

        let mut invite = unread("!i", "Invited", 1);
        invite.category = RoomCategory::Invite;
        let mut mention = unread("!c", "Gamma", 1);
        mention.mention = true;
        app.on_matrix(MatrixEvent::Rooms(vec![
            listed("!a", "Alpha"),
            marked("!b", "Beta"),
            mention,
            invite,
            unread("!d", "Delta", 0),
        ]));
        app.read_receipts = false;
        app.perform(Action::MarkAllRead);
        let command = commands.try_recv().unwrap();
        match &command {
            MatrixCmd::MarkRoomsRead { room_ids, public } => {
                assert_eq!(room_ids, &["!b", "!c", "!d"]);
                // As the "read receipts" setting says.
                assert!(!public);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(format!("{command:?}"), "MarkRoomsRead(3 rooms)");
        assert_eq!(app.toast.as_ref().unwrap().0, "Marked 3 rooms as read");
        for room in &app.rooms {
            let invite = room.kind == RoomKind::Invite;
            assert_eq!(room.shows_unread(), invite, "{}", room.name);
        }
        // The mark set by hand stays away while the server catches up.
        let mut invite = listed("!i", "Invited");
        invite.category = RoomCategory::Invite;
        app.on_matrix(MatrixEvent::Rooms(vec![
            listed("!a", "Alpha"),
            marked("!b", "Beta"),
            listed("!c", "Gamma"),
            invite,
            listed("!d", "Delta"),
        ]));
        assert!(!app.rooms[1].shows_unread());
        assert_eq!(sent(&mut commands), Vec::<String>::new());
    }

    #[test]
    fn a_room_that_is_gone_or_a_session_that_ended_leaves_nothing_behind() {
        let (mut app, mut terminal, _commands) = connected();
        arrive(&mut app, others(0..2));
        draw(&mut app, &mut terminal);
        app.mark_room_unread(1);
        assert!(app.unread.read.contains_key("!a") && app.unread.wishes.contains_key("!b"));
        app.on_matrix(MatrixEvent::Rooms(vec![listed("!c", "Gamma")]));
        assert!(app.unread.read.is_empty() && app.unread.wishes.is_empty());
        app.mark_room_unread(0);
        app.on_matrix(MatrixEvent::NeedLogin { reason: None });
        assert!(app.unread.wishes.is_empty());
        assert_eq!(app.unread.line, Line::Off);
    }

    fn from(n: usize, sender: &str) -> Message {
        let mut msg = message(n, false);
        msg.sender = sender.to_owned();
        msg.sender_id = Some(format!("@{sender}:example.org"));
        msg
    }

    #[test]
    fn ignored_unread_messages_place_the_line_accurately() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::IgnoredUsers(vec!["@bob:example.org".into()]));
        app.on_matrix(MatrixEvent::Rooms(vec![
            listed("!a", "Alpha"),
            unread("!b", "Beta", 2),
        ]));

        // First unread message was from an ignored user (dropped by `convert`);
        // line lands above the next non-ignored unread message.
        app.perform(Action::OpenRoom(1));
        sent(&mut commands);
        arrive(&mut app, vec![from(0, "carol"), marker(), from(2, "dave")]);
        assert_eq!(shown(&app), ["m0", "--", "m2"]);
        press(&mut app, KeyCode::Char('u'), KeyModifiers::ALT);
        assert_eq!(app.messages()[app.timeline.selected.unwrap()].id, "m2");

        // Also if an un-dropped slice still carried an ignored sender, `first_unread`
        // skips it cleanly.
        assert_eq!(
            first_unread(
                &[from(0, "carol"), from(1, "bob"), from(2, "dave")],
                Some(1),
                0,
                None,
                &app.ignored_users,
            ),
            Some(2)
        );

        // ALL unread messages are from an ignored user: `convert` preserves trailing
        // `ReadMarker` so the line settles to `Off` instead of falling back to
        // `by_count` and sitting above already-read `m0`.
        app.on_matrix(MatrixEvent::Rooms(vec![
            listed("!a", "Alpha"),
            unread("!b", "Beta", 2),
        ]));
        app.perform(Action::OpenRoom(1));
        sent(&mut commands);
        arrive(&mut app, vec![from(0, "carol"), marker()]);
        assert_eq!(shown(&app), ["m0"]);
        assert!(!app.unread_line_pending());
        assert_eq!(app.unread.line, Line::Off);
        draw(&mut app, &mut terminal);
        assert_eq!(sent(&mut commands), ["MarkRead(!b)"]);

        // Room with ZERO visible messages (only ReadMarker + ignored unread messages):
        // must not remain stuck waiting or sit above nothing, and clears via MarkRead.
        app.on_matrix(MatrixEvent::Rooms(vec![
            listed("!a", "Alpha"),
            unread("!b", "Beta", 1),
        ]));
        app.perform(Action::OpenRoom(1));
        sent(&mut commands);
        arrive(&mut app, vec![marker()]);
        assert!(shown(&app).is_empty());
        assert!(!app.unread_line_pending());
        assert_eq!(app.unread.line, Line::Off);
        draw(&mut app, &mut terminal);
        assert_eq!(sent(&mut commands), ["MarkRead(!b)"]);
    }

    #[test]
    fn ignoring_a_sender_while_the_room_is_open_advances_or_clears_the_unread_line() {
        let (mut app, _terminal, _commands) = connected();
        app.on_matrix(MatrixEvent::Rooms(vec![
            listed("!a", "Alpha"),
            unread("!b", "Beta", 2),
        ]));
        app.perform(Action::OpenRoom(1));
        arrive(
            &mut app,
            vec![from(0, "carol"), marker(), from(1, "bob"), from(2, "dave")],
        );
        assert_eq!(shown(&app), ["m0", "--", "m1", "m2"]);

        // Ignore @bob while Beta is open: even with an intermediate empty `Clear`
        // timeline update before repagination, the line advances to @dave's message.
        app.on_matrix(MatrixEvent::IgnoredUsers(vec!["@bob:example.org".into()]));
        arrive(&mut app, Vec::new());
        arrive(&mut app, vec![from(0, "carol"), from(2, "dave")]);
        assert_eq!(shown(&app), ["m0", "--", "m2"]);

        // Now ignore @dave as well, testing the reverse order where `Timeline`
        // arrives right before `IgnoredUsers`: the line clears cleanly to `Off`.
        arrive(&mut app, vec![from(0, "carol")]);
        app.on_matrix(MatrixEvent::IgnoredUsers(vec![
            "@bob:example.org".into(),
            "@dave:example.org".into(),
        ]));
        assert_eq!(shown(&app), ["m0"]);
        assert_eq!(app.unread.line, Line::Off);
    }

    #[test]
    fn ignored_only_unread_updates_in_open_and_listed_rooms_mark_read() {
        let (mut app, mut terminal, mut commands) = connected();
        arrive(&mut app, vec![from(0, "carol")]);
        draw(&mut app, &mut terminal);
        assert_eq!(sent(&mut commands), ["MarkRead(!a)"]);

        // Ignored user sends a message in the already-open room Alpha: no new visible
        // message arrives, only the server unread state rises -> one MarkRead goes out.
        let mut bumped = unread("!a", "Alpha", 1);
        bumped.server_notifications = 1;
        app.on_matrix(MatrixEvent::Rooms(vec![
            bumped.clone(),
            unread("!b", "Beta", 1),
        ]));
        draw(&mut app, &mut terminal);
        assert_eq!(sent(&mut commands), ["MarkRead(!a)"]);
        // Redrawing before `/sync` settles must not loop MarkRead.
        draw(&mut app, &mut terminal);
        assert_eq!(sent(&mut commands), Vec::<String>::new());

        // Once `/sync` clears it and a second ignored message bumps it again, it clears
        // once more.
        app.on_matrix(MatrixEvent::Rooms(vec![
            listed("!a", "Alpha"),
            unread("!b", "Beta", 1),
        ]));
        let mut bumped2 = unread("!a", "Alpha", 1);
        bumped2.server_notifications = 1;
        app.on_matrix(MatrixEvent::Rooms(vec![bumped2, unread("!b", "Beta", 1)]));
        draw(&mut app, &mut terminal);
        assert_eq!(sent(&mut commands), ["MarkRead(!a)"]);

        // Unread count on another room whose only unread messages are ignored can
        // still be cleared in the room list with "Mark this room as read" or
        // "Mark all rooms as read".
        assert!(in_bold(&mut app, &mut terminal, "Beta"));
        app.mark_room_read(1);
        assert!(!app.rooms[1].shows_unread());
        assert!(!in_bold(&mut app, &mut terminal, "Beta"));
        assert!(sent(&mut commands).contains(&"MarkRead(!b)".to_owned()));
    }
}
