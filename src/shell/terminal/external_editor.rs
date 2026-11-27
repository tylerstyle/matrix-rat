//! The external editor's turn at the terminal, as the shell sees it: while the editor
//! has the terminal (`release`, then `resume`, as for Ctrl-Z), the app does not read
//! the keyboard, and the Matrix worker, a task of its own, keeps syncing. What it says
//! meanwhile waits in [`HeldEvents`] (see [`hold_events_while`]) and is handed to the
//! app when the editor is done ([`next_matrix_event`]). The app's side, the request and
//! its outcome, is `App::take_editor_request` and `App::finish_editor`.

use std::collections::VecDeque;

use tokio::sync::mpsc::UnboundedReceiver;

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
        | MatrixEvent::LoggedOut
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
        | MatrixEvent::SearchHistory(_)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemRef, Message};

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
