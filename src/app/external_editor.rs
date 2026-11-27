//! Writing the draft in `$VISUAL` / `$EDITOR` (`Action::ComposeInEditor`).
//!
//! The action only records what is to be done (`editor_request`): the run loop owns the
//! terminal, and hands it over for as long as the editor runs (`terminal::release`, then
//! `terminal::resume`, as for Ctrl-Z). The app does not read the keyboard meanwhile; the
//! Matrix worker is a task of its own and keeps syncing, its events wait in
//! [`HeldEvents`] (see [`hold_events_while`]) and are handled when the editor is done.
//! The result comes back through [`App::finish_editor`], which a test can call with any
//! outcome.

use std::collections::VecDeque;

use tokio::sync::mpsc::UnboundedReceiver;

use super::App;
use crate::action::Pane;
use crate::editor::{Failure, Outcome, Request};
use crate::matrix::MatrixEvent;

/// What the worker says while the editor has the terminal. None of it reaches the app
/// before the editor is done: a notification must not draw over the editor, and the
/// request is bound to the room it was made in. So it waits here, in the order it came.
///
/// Some events are snapshots that a later one of the same kind replaces whole: the open
/// timeline's messages, the room list, the spaces, who is typing, the count of new
/// messages behind a detached view. Each is kept once: a newer one takes the place of the
/// one held before (as if the worker had been quicker), so what came between is handled
/// after the newest state, never before an older one. A timeline of 5000 messages is
/// about 6 MB, and the open room changes with every message, reaction and receipt: in the
/// channel, one copy per change waited for as long as the edit took.
#[derive(Default)]
pub struct HeldEvents {
    events: VecDeque<MatrixEvent>,
    /// The events before this index came before a session began or ended: a snapshot
    /// after it is of another session, and never takes the place of one of them.
    session_start: usize,
}

/// What a snapshot is of: of two events that are of the same, the later says all the
/// earlier said. Two timelines are of the same when they are of the same room and
/// generation: each opening (a room, a thread, a detached slice) has a generation of its
/// own and one forwarder, which sends nothing but whole timelines.
#[derive(PartialEq, Eq)]
enum Snapshot<'a> {
    Timeline(&'a str, u64),
    Rooms,
    Spaces,
    Typing(u64),
    DetachedNew(u64),
}

fn snapshot_of(event: &MatrixEvent) -> Option<Snapshot<'_>> {
    match event {
        // A reset is of its timeline too: a timeline after it is held after it (see
        // `resets_a_timeline`).
        MatrixEvent::Timeline {
            room_id,
            generation,
            ..
        }
        | MatrixEvent::TimelineReset {
            room_id,
            generation,
        } => Some(Snapshot::Timeline(room_id, *generation)),
        MatrixEvent::Rooms(_) => Some(Snapshot::Rooms),
        MatrixEvent::Spaces(_) => Some(Snapshot::Spaces),
        MatrixEvent::Typing { generation, .. } => Some(Snapshot::Typing(*generation)),
        MatrixEvent::DetachedNew { generation, .. } => Some(Snapshot::DetachedNew(*generation)),
        MatrixEvent::NeedLogin { .. }
        | MatrixEvent::LoginFailed(_)
        | MatrixEvent::BrowserLoginUrl(_)
        | MatrixEvent::LoggedIn { .. }
        | MatrixEvent::Sync(_)
        | MatrixEvent::CaughtUp
        | MatrixEvent::Recovery(_)
        | MatrixEvent::RecoverResult(_)
        | MatrixEvent::Paginated { .. }
        | MatrixEvent::PaginatedForwards { .. }
        | MatrixEvent::JumpFailed { .. }
        | MatrixEvent::ActionFailed(_)
        | MatrixEvent::Error(_)
        | MatrixEvent::Image { .. }
        | MatrixEvent::Security(_)
        | MatrixEvent::Verification(_)
        | MatrixEvent::CrossSigningSetUp(_)
        | MatrixEvent::Notice(_)
        | MatrixEvent::SuppressedNotice { .. }
        | MatrixEvent::NoticeClicked { .. }
        | MatrixEvent::FileSaved { .. }
        | MatrixEvent::AudioFetched { .. }
        | MatrixEvent::SpeechFetched { .. }
        | MatrixEvent::LinkPreview { .. }
        | MatrixEvent::Avatar { .. }
        | MatrixEvent::EmotePicture { .. }
        | MatrixEvent::UploadLimit(_)
        | MatrixEvent::FilesNotSent { .. }
        | MatrixEvent::FilesSent(_)
        | MatrixEvent::RoomMembers { .. }
        | MatrixEvent::Manage(_)
        | MatrixEvent::NeedReauth { .. }
        | MatrixEvent::SearchResults { .. }
        | MatrixEvent::Draft { .. }
        | MatrixEvent::IgnoredUsers(_)
        | MatrixEvent::IgnoreResult { .. }
        | MatrixEvent::ReportResult(_)
        | MatrixEvent::UserSearchResults { .. }
        | MatrixEvent::RoomPower { .. }
        | MatrixEvent::PinnedEvents { .. }
        | MatrixEvent::PinnedList { .. }
        | MatrixEvent::ForwardResult { .. }
        | MatrixEvent::Devices(_)
        | MatrixEvent::Keywords(_)
        | MatrixEvent::Saved(_)
        | MatrixEvent::DecryptionRetried { .. }
        | MatrixEvent::OwnProfile(_)
        | MatrixEvent::OwnProfileResult { .. }
        | MatrixEvent::Source { .. }
        | MatrixEvent::UserProfile { .. }
        | MatrixEvent::DirectoryResults(_)
        | MatrixEvent::DirectoryError { .. }
        | MatrixEvent::EmotePacks { .. }
        | MatrixEvent::Threads(_)
        | MatrixEvent::ThreadsFailed { .. }
        | MatrixEvent::Agent(_)
        | MatrixEvent::Collect(_) => None,
    }
}

/// The app forgets a session's rooms and timelines on these, and starts again.
fn begins_or_ends_a_session(event: &MatrixEvent) -> bool {
    matches!(
        event,
        MatrixEvent::NeedLogin { .. }
            | MatrixEvent::NeedReauth { .. }
            | MatrixEvent::LoggedIn { .. }
    )
}

/// The worker says so when the SDK cleared the room (a sync that missed messages, an
/// ignore-list change): the app forgets that the start was reached, where a timeline
/// only adds to what it knows. So it takes no earlier timeline's place (it would come
/// before what came between) and gives its own up to no later one (the reset would be
/// lost, and the room stuck short of its start, never asking for older messages): a
/// timeline after it is held after it.
fn resets_a_timeline(event: &MatrixEvent) -> bool {
    matches!(event, MatrixEvent::TimelineReset { .. })
}

impl HeldEvents {
    pub fn push(&mut self, event: MatrixEvent) {
        if begins_or_ends_a_session(&event) {
            self.events.push_back(event);
            self.session_start = self.events.len();
            return;
        }
        if resets_a_timeline(&event) {
            self.events.push_back(event);
            return;
        }
        // The last held one of the same: after a reset there can be two of a timeline,
        // and only the one after it may be replaced.
        let earlier = snapshot_of(&event)
            .and_then(|what| {
                self.events
                    .range(self.session_start..)
                    .rposition(|held| snapshot_of(held).as_ref() == Some(&what))
            })
            .filter(|&at| !resets_a_timeline(&self.events[self.session_start + at]));
        match earlier {
            Some(at) => self.events[self.session_start + at] = event,
            None => self.events.push_back(event),
        }
    }

    /// The event that came first.
    pub fn pop(&mut self) -> Option<MatrixEvent> {
        let event = self.events.pop_front()?;
        self.session_start = self.session_start.saturating_sub(1);
        Some(event)
    }
}

/// Waits for `editor` (the external editor, on a thread of its own) and meanwhile takes
/// what the worker says on `events` into `held`, where each snapshot is kept once. What
/// is waiting in the channel when the editor is done goes there too, behind the rest.
/// Nothing is handed to the app here. `events` is `None` in the demo.
pub async fn hold_events_while<T>(
    editor: impl Future<Output = T>,
    events: Option<&mut UnboundedReceiver<MatrixEvent>>,
    held: &mut HeldEvents,
) -> T {
    let Some(events) = events else {
        return editor.await;
    };
    tokio::pin!(editor);
    let mut open = true;
    let outcome = loop {
        tokio::select! {
            biased;
            outcome = &mut editor => break outcome,
            event = events.recv(), if open => match event {
                Some(event) => held.push(event),
                // The worker is gone (the app is ending): nothing more will come.
                None => open = false,
            },
        }
    };
    while let Ok(event) = events.try_recv() {
        held.push(event);
    }
    outcome
}

/// The worker's next event for the app: what was held while the editor ran comes
/// first, in its order, then the channel. Never ends for the demo, which has no worker.
pub async fn next_matrix_event(
    held: &mut HeldEvents,
    events: Option<&mut UnboundedReceiver<MatrixEvent>>,
) -> Option<MatrixEvent> {
    if let Some(event) = held.pop() {
        return Some(event);
    }
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

impl App {
    /// `Action::ComposeInEditor`.
    pub(super) fn request_editor(&mut self) {
        self.request_editor_with(self.probes.editor());
    }

    /// With the editor command already worked out (a test passes its own).
    pub(super) fn request_editor_with(&mut self, command: Result<Vec<String>, Failure>) {
        if self.room().is_none_or(|room| room.is_pending()) {
            self.show_toast("There is no chat to write in");
            return;
        }
        match command {
            Ok(argv) => {
                // No microphone stays open behind an editor occupying the terminal.
                self.stop_recording();
                self.set_focus(Pane::Composer);
                self.editor_request = Some(Request {
                    argv,
                    text: self.composer.text().to_owned(),
                });
            }
            Err(failure) => self.show_toast(failure.message()),
        }
    }

    /// What the run loop is to start, once.
    pub fn take_editor_request(&mut self) -> Option<Request> {
        self.editor_request.take()
    }

    /// The editor is done: its text goes into the composer (not sent), or the draft stays
    /// as it was and a toast says why.
    pub fn finish_editor(&mut self, outcome: Outcome) {
        self.dirty = true;
        match outcome {
            Outcome::Failed(failure) => self.show_toast(failure.message()),
            Outcome::Unchanged => {}
            Outcome::Text(text) => {
                self.set_focus(Pane::Composer);
                // Through `insert`: tabs and control characters are dealt with as for
                // anything typed or pasted.
                self.composer.take();
                self.composer.insert(&text);
                // Where a mention stood is not known any more: a mention completed
                // before is plain text now, and is sent as such unless it is completed
                // again (as with a draft restored after a restart). An edit that had
                // mentions still clears them on the server.
                let edit_had_mentions = self.complete.edit_had_mentions;
                self.clear_completion_marks();
                self.complete.edit_had_mentions = edit_had_mentions;
                self.update_typing();
                // Counts as a change of the draft, like anything typed.
                self.note_draft();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::action::Action;
    use crate::app::ComposeMode;
    use crate::complete::MentionMark;
    use crate::input::{InputEvent, KeyCode, KeyEvent, KeyModifiers};
    use crate::matrix::{MatrixCmd, MatrixHandle};
    use crate::model::{ItemRef, Message, Room, RoomKind};

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

    /// Connected to a fake worker, the composer has the focus.
    fn connected() -> (App, UnboundedReceiver<MatrixCmd>) {
        let mut app = App::demo();
        let (handle, commands) = MatrixHandle::for_test();
        app.matrix = Some(handle);
        let mut alpha = Room::new("!a", "Alpha", RoomKind::Room);
        alpha.messages = vec![message(0, false), message(1, true)];
        app.rooms = vec![alpha, Room::new("!b", "Beta", RoomKind::Room)];
        app.active = 0;
        app.screen = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.perform(Action::Focus(Pane::Composer));
        (app, commands)
    }

    fn argv() -> Result<Vec<String>, Failure> {
        Ok(vec!["fake-editor".to_owned()])
    }

    /// The whole flow with an editor that is a closure: what the run loop does, minus
    /// the terminal. Returns what the editor was given; `None` from the closure is an
    /// editor that fails.
    fn write_in_editor(app: &mut App, editor: impl FnOnce(&str) -> Option<String>) -> String {
        app.request_editor_with(argv());
        let request = app.take_editor_request().expect("a request");
        let dir = std::env::temp_dir().join(format!("matrix-rat-app-{}", std::process::id()));
        let given = request.text.clone();
        let outcome = crate::editor::edit(&request, &dir, |_, path| {
            let before = std::fs::read_to_string(path).unwrap();
            match editor(&before) {
                Some(text) => std::fs::write(path, text).map(|()| true),
                None => Ok(false),
            }
        });
        app.finish_editor(outcome);
        let _ = std::fs::remove_dir(&dir);
        given
    }

    fn saves(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            if let MatrixCmd::SaveDraft { room_id, text } = command {
                seen.push(format!("{room_id}={text}"));
            }
        }
        seen
    }

    #[test]
    fn the_draft_goes_out_and_the_result_comes_back_unsent() {
        let (mut app, mut commands) = connected();
        app.composer.insert("first line\nsecond");
        let given = write_in_editor(&mut app, |seen| Some(format!("{seen} and more\n")));
        assert_eq!(given, "first line\nsecond");
        assert_eq!(app.composer.text(), "first line\nsecond and more");
        // At the end, ready to go on; nothing was sent.
        assert_eq!(app.composer.cursor(), app.composer.text().len());
        assert_eq!(app.focus, Pane::Composer);
        assert!(app.editor_request.is_none());
        while let Ok(command) = commands.try_recv() {
            assert!(!format!("{command:?}").starts_with("Send"), "{command:?}");
        }
    }

    #[test]
    fn from_anywhere_it_writes_in_the_composer() {
        let (mut app, _commands) = connected();
        app.perform(Action::Focus(Pane::Timeline));
        write_in_editor(&mut app, |_| Some("hello".to_owned()));
        assert_eq!(app.focus, Pane::Composer);
        assert_eq!(app.composer.text(), "hello");
    }

    #[test]
    fn the_key_works_in_the_composer_only() {
        let alt_v = || InputEvent::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT));
        // What it does depends on $VISUAL / $EDITOR of the machine running the test: a
        // request, or the toast that says how to set one. Either way something happens.
        let (mut app, _commands) = connected();
        app.on_event(alt_v());
        assert!(
            app.editor_request.is_some()
                || app
                    .toast
                    .as_ref()
                    .is_some_and(|(text, _)| text.contains("$EDITOR"))
        );
        let (mut app, _commands) = connected();
        app.perform(Action::Focus(Pane::Timeline));
        app.on_event(alt_v());
        assert!(app.editor_request.is_none() && app.toast.is_none());
    }

    #[test]
    fn no_editor_means_a_hint_and_no_guess() {
        let (mut app, _commands) = connected();
        app.composer.insert("keep");
        app.request_editor_with(crate::editor::command(None, None));
        assert!(app.editor_request.is_none());
        let toast = &app.toast.as_ref().unwrap().0;
        assert!(
            toast.contains("$VISUAL") && toast.contains("$EDITOR"),
            "{toast}"
        );
        assert_eq!(app.composer.text(), "keep");
    }

    #[test]
    fn a_failed_editor_leaves_the_draft_and_says_so() {
        let (mut app, _commands) = connected();
        app.composer.insert("keep me");
        write_in_editor(&mut app, |_| None);
        assert_eq!(app.composer.text(), "keep me");
        assert!(app.toast.as_ref().unwrap().0.contains("draft is as it was"));
    }

    #[test]
    fn emptying_the_file_empties_the_draft_but_nothing_from_nothing() {
        let (mut app, mut commands) = connected();
        app.composer.insert("gone");
        app.note_draft();
        write_in_editor(&mut app, |_| Some(" \n".to_owned()));
        assert_eq!(app.composer.text(), "");
        app.save_idle_draft(Duration::ZERO);
        assert_eq!(saves(&mut commands), ["!a="]);

        write_in_editor(&mut app, |_| Some(String::new()));
        assert_eq!(app.composer.text(), "");
        assert!(app.toast.is_none());
    }

    #[test]
    fn control_characters_are_dealt_with_as_for_anything_typed() {
        let (mut app, _commands) = connected();
        write_in_editor(&mut app, |_| Some("a\u{1b}[2Jb\tc\r\nd\u{7}".to_owned()));
        assert_eq!(app.composer.text(), "a[2Jb    c\nd");
    }

    #[test]
    fn mentions_become_plain_text_and_the_draft_counts_as_changed() {
        let (mut app, mut commands) = connected();
        app.composer.insert("hi @ann");
        app.complete
            .marks
            .push(MentionMark::user("ann", "@ann:example.org").with_at(Some(3)));
        app.note_draft();
        write_in_editor(&mut app, |seen| Some(format!("well, {seen}")));
        assert_eq!(app.composer.text(), "well, hi @ann");
        // Not valid any more: where the mention was is not known.
        assert!(app.complete.marks.is_empty());
        assert!(app.complete.active.is_none());
        // And it is saved like typed text.
        app.save_idle_draft(Duration::ZERO);
        assert_eq!(saves(&mut commands), ["!a=well, hi @ann"]);
    }

    #[test]
    fn an_edit_stays_an_edit() {
        let (mut app, mut commands) = connected();
        app.perform(Action::Focus(Pane::Timeline));
        app.timeline.selected = Some(1);
        app.perform(Action::Edit);
        assert!(matches!(app.compose_mode, ComposeMode::Edit(1)));
        assert_eq!(app.composer.text(), "line 1");
        app.complete.edit_had_mentions = true;
        write_in_editor(&mut app, |seen| Some(format!("{seen}!")));
        assert_eq!(app.composer.text(), "line 1!");
        assert!(matches!(app.compose_mode, ComposeMode::Edit(1)));
        // The edit still clears the mentions the message had.
        assert!(app.complete.edit_had_mentions);
        // An edit is not a draft.
        app.save_idle_draft(Duration::ZERO);
        assert_eq!(saves(&mut commands), Vec::<String>::new());
    }

    #[test]
    fn a_reply_stays_a_reply() {
        let (mut app, _commands) = connected();
        app.perform(Action::Focus(Pane::Timeline));
        app.timeline.selected = Some(0);
        app.perform(Action::Reply);
        assert!(matches!(app.compose_mode, ComposeMode::Reply(0)));
        write_in_editor(&mut app, |_| Some("answer".to_owned()));
        assert!(matches!(app.compose_mode, ComposeMode::Reply(0)));
        assert_eq!(app.composer.text(), "answer");
    }

    #[test]
    fn it_works_in_the_demo_and_not_without_a_chat() {
        let mut app = App::demo();
        app.perform(Action::Focus(Pane::Composer));
        write_in_editor(&mut app, |_| Some("from the demo".to_owned()));
        assert_eq!(app.composer.text(), "from the demo");

        app.rooms.clear();
        app.request_editor_with(argv());
        assert!(app.editor_request.is_none());
    }

    #[test]
    fn the_request_does_not_show_the_draft() {
        let (mut app, _commands) = connected();
        app.composer.insert("private words");
        app.request_editor_with(argv());
        assert!(!format!("{:?}", app.editor_request).contains("private"));
    }

    /// The open timeline of `room` as the worker sends it: `count` messages.
    fn timeline(room: &str, generation: u64, count: usize) -> MatrixEvent {
        MatrixEvent::Timeline {
            room_id: room.to_owned(),
            generation,
            messages: (0..count).map(|n| message(n, false)).collect(),
            reached_start: false,
            detached: None,
        }
    }

    /// A room list of `count` rooms.
    fn rooms(count: usize) -> MatrixEvent {
        let summary = |n: usize| crate::matrix::RoomSummary {
            id: format!("!r{n}"),
            name: format!("Room {n}"),
            category: crate::matrix::RoomCategory::Room,
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
            encrypted: true,
        };
        MatrixEvent::Rooms((0..count).map(summary).collect())
    }

    fn failed(n: usize) -> MatrixEvent {
        MatrixEvent::ActionFailed(format!("failure {n}"))
    }

    fn drain(held: &mut HeldEvents) -> Vec<String> {
        std::iter::from_fn(|| held.pop())
            .map(|event| event.describe())
            .collect()
    }

    /// A long edit in a busy room: the worker goes on sending the whole open timeline on
    /// every change. Of those (and of the room lists) one is held, the newest, in the
    /// place of the first; every other event is held as it came, in order.
    #[tokio::test]
    async fn while_the_editor_runs_each_snapshot_is_held_once_and_the_rest_in_order() {
        let (worker, mut events) = tokio::sync::mpsc::unbounded_channel();
        let (done, wait) = std::sync::mpsc::channel::<()>();
        // The editor: a thread that blocks until the worker below has said everything.
        let editor = tokio::task::spawn_blocking(move || {
            wait.recv().unwrap();
            "edited"
        });
        let feeder = tokio::spawn(async move {
            for n in 1..=60 {
                worker.send(timeline("!a", 7, n)).unwrap();
                worker.send(rooms(n)).unwrap();
                if n % 20 == 0 {
                    worker.send(failed(n)).unwrap();
                }
                tokio::task::yield_now().await;
            }
            // Right before the editor ends: still taken in, behind the rest.
            worker.send(failed(99)).unwrap();
            worker.send(timeline("!a", 7, 61)).unwrap();
            done.send(()).unwrap();
            worker
        });
        let mut held = HeldEvents::default();
        let outcome = hold_events_while(editor, Some(&mut events), &mut held).await;
        assert_eq!(outcome.unwrap(), "edited");
        let _worker = feeder.await.unwrap();
        assert_eq!(
            drain(&mut held),
            [
                "Timeline(!a, generation 7, 61 messages, start: false, detached: false)",
                "Rooms(60)",
                "ActionFailed(failure 20)",
                "ActionFailed(failure 40)",
                "ActionFailed(failure 60)",
                "ActionFailed(failure 99)",
            ]
        );
        // Nothing was left behind in the channel.
        assert!(events.try_recv().is_err());
    }

    /// Snapshots of different things are kept apart: another room, another opening of
    /// the same room (a generation of its own), who types, the spaces. And none takes
    /// the place of one from before a session began or ended.
    #[test]
    fn only_a_snapshot_of_the_same_thing_and_session_is_replaced() {
        let mut held = HeldEvents::default();
        held.push(timeline("!a", 1, 1));
        held.push(timeline("!b", 1, 1));
        held.push(timeline("!a", 2, 1));
        held.push(MatrixEvent::Typing {
            generation: 2,
            names: vec!["ann".to_owned()],
        });
        held.push(MatrixEvent::Spaces(vec![Default::default()]));
        held.push(rooms(1));
        held.push(timeline("!a", 2, 2));
        held.push(MatrixEvent::Typing {
            generation: 2,
            names: Vec::new(),
        });
        held.push(MatrixEvent::Spaces(vec![Default::default(); 2]));
        held.push(MatrixEvent::NeedLogin { reason: None });
        held.push(rooms(2));
        held.push(MatrixEvent::LoggedIn {
            user_id: "@ann:example.org".to_owned(),
        });
        held.push(rooms(3));
        held.push(timeline("!a", 2, 3));
        held.push(rooms(4));
        assert_eq!(
            drain(&mut held),
            [
                "Timeline(!a, generation 1, 1 messages, start: false, detached: false)",
                "Timeline(!b, generation 1, 1 messages, start: false, detached: false)",
                "Timeline(!a, generation 2, 2 messages, start: false, detached: false)",
                "Typing(generation 2, 0)",
                "Spaces(2)",
                "Rooms(1)",
                "NeedLogin(None)",
                "Rooms(2)",
                "LoggedIn(@ann:example.org)",
                "Rooms(4)",
                "Timeline(!a, generation 2, 3 messages, start: false, detached: false)",
            ]
        );
        // Taking events out keeps the session's start where it is.
        held.push(rooms(5));
        held.push(MatrixEvent::NeedLogin { reason: None });
        held.push(rooms(6));
        assert_eq!(held.pop().unwrap().describe(), "Rooms(5)");
        held.push(rooms(7));
        assert_eq!(drain(&mut held), ["NeedLogin(None)", "Rooms(7)"]);
    }

    /// A reset of the timeline (the SDK cleared the room) is held where it came, after
    /// what came before it, and the timeline after it does not take its place, nor the
    /// one before: so the app still forgets that the start was reached, and only then
    /// hears whether the latest timeline has it. A timeline after it may take the place
    /// of the one after it.
    #[test]
    fn a_reset_timeline_is_held_where_it_came_and_never_replaced() {
        let start = |mut event: MatrixEvent| {
            if let MatrixEvent::Timeline { reached_start, .. } = &mut event {
                *reached_start = true;
            }
            event
        };
        let reset = |room: &str, generation| MatrixEvent::TimelineReset {
            room_id: room.to_owned(),
            generation,
        };
        let mut held = HeldEvents::default();
        held.push(start(timeline("!a", 1, 100)));
        held.push(MatrixEvent::Paginated {
            generation: 1,
            reached_start: true,
        });
        held.push(reset("!a", 1));
        held.push(timeline("!a", 1, 5));
        held.push(timeline("!a", 1, 6));
        held.push(reset("!a", 1));
        held.push(reset("!a", 1));
        held.push(timeline("!a", 1, 0));
        // Another room's timeline is no timeline of this reset.
        held.push(timeline("!b", 2, 3));
        held.push(timeline("!b", 2, 4));
        assert_eq!(
            drain(&mut held),
            [
                "Timeline(!a, generation 1, 100 messages, start: true, detached: false)",
                "Paginated(generation 1, start: true)",
                "TimelineReset(!a, generation 1)",
                "Timeline(!a, generation 1, 6 messages, start: false, detached: false)",
                "TimelineReset(!a, generation 1)",
                "TimelineReset(!a, generation 1)",
                "Timeline(!a, generation 1, 0 messages, start: false, detached: false)",
                "Timeline(!b, generation 2, 4 messages, start: false, detached: false)",
            ]
        );
    }

    /// A jump switches the worker to another room (`!b`, gen 2), then a late cancel
    /// restores the open room (`!a`, gen 1) with a reset and fresh timeline while the
    /// external editor holds events: `!b`'s snapshot does not disturb `!a`, and `!a`'s
    /// restored timeline stays behind its reset.
    #[test]
    fn a_late_cancel_restore_held_during_external_editor_replays_in_order() {
        let mut held = HeldEvents::default();
        held.push(timeline("!a", 1, 20));
        held.push(timeline("!b", 2, 5));
        held.push(MatrixEvent::TimelineReset {
            room_id: "!a".to_owned(),
            generation: 1,
        });
        held.push(timeline("!a", 1, 21));
        held.push(timeline("!a", 1, 22));
        assert_eq!(
            drain(&mut held),
            [
                "Timeline(!a, generation 1, 20 messages, start: false, detached: false)",
                "Timeline(!b, generation 2, 5 messages, start: false, detached: false)",
                "TimelineReset(!a, generation 1)",
                "Timeline(!a, generation 1, 22 messages, start: false, detached: false)",
            ]
        );
    }

    /// Back from the editor, the app gets what was held first, in order, then what the
    /// channel has.
    #[tokio::test]
    async fn what_was_held_comes_before_the_channel() {
        let (worker, mut events) = tokio::sync::mpsc::unbounded_channel();
        let mut held = HeldEvents::default();
        held.push(failed(1));
        held.push(failed(2));
        worker.send(failed(3)).unwrap();
        let mut seen = Vec::new();
        for _ in 0..3 {
            let event = next_matrix_event(&mut held, Some(&mut events)).await;
            seen.push(event.unwrap().describe());
        }
        assert_eq!(
            seen,
            [
                "ActionFailed(failure 1)",
                "ActionFailed(failure 2)",
                "ActionFailed(failure 3)"
            ]
        );
        // Without a worker (the demo) the editor is only waited for.
        let mut held = HeldEvents::default();
        assert_eq!(hold_events_while(async { 5 }, None, &mut held).await, 5);
        assert!(held.pop().is_none());
    }
}
