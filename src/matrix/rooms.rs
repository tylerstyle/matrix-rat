//! Per-room work: timelines, sending, reactions, receipts, invites.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use futures::{StreamExt, pin_mut};
use matrix_sdk::room::edit::EditedContent;
use matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType;
use matrix_sdk::ruma::events::Mentions;
use matrix_sdk::ruma::events::room::message::{
    RoomMessageEventContent, RoomMessageEventContentWithoutRelation,
};
use matrix_sdk::ruma::{EventId, OwnedEventId, OwnedTransactionId, OwnedUserId, RoomId, UserId};
use matrix_sdk::{Client, Room, RoomMemberships};
use matrix_sdk_ui::eyeball_im::{Vector, VectorDiff};
use matrix_sdk_ui::timeline::{
    MsgLikeKind, RoomExt, Timeline, TimelineDetails, TimelineEventFocusThreadMode,
    TimelineEventItemId, TimelineFocus, TimelineItem, TimelineItemContent,
    TimelineReadReceiptTracking,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::safety::{IgnoredSet, is_ignored_in};
use super::{DetachedTimeline, MatrixCmd, MatrixEvent, drafts, threads};
use crate::format::Outgoing;
use crate::logsafe;
use crate::model::ItemRef;

/// Recently used timelines stay alive so going back to a room is instant and keeps the
/// history that was already loaded.
const KEEP_TIMELINES: usize = 6;
/// A cached timeline that was not used for this long, and is not shown, is dropped: it
/// holds about 6 KB per message it has loaded (its items, and the SDK's event cache of
/// the room, which cannot shrink while a timeline listens to it). The room's events
/// stay in the store; opened again, it starts from its latest ones and pages back.
const IDLE_TIMELINE: Duration = Duration::from_secs(20 * 60);
const PAGE: u16 = 40;

/// A timeline kept for a room (or the open thread), and when it was last used.
struct Cached {
    /// As `threads::key` names it.
    key: String,
    timeline: Arc<Timeline>,
    last_used: Instant,
}

pub(super) enum OpenedTimeline {
    Detached {
        room_id: String,
        thread: Option<String>,
        generation: u64,
        target_event: String,
        timeline: Arc<Timeline>,
    },
    Failed {
        generation: u64,
        reason: String,
    },
}

/// What the forwarder is streaming to the UI: a live room/thread timeline, or a
/// detached `/context` slice around an event.
enum StreamKind {
    Live,
    Detached {
        target_event: String,
        timeline: Arc<Timeline>,
        reached_start: Arc<AtomicBool>,
        reached_end: Arc<AtomicBool>,
    },
}

struct ActiveStream {
    room_id: String,
    thread: Option<String>,
    generation: u64,
    kind: StreamKind,
}

/// The view the worker was streaming right before a jump took over, kept in case the UI
/// had already cancelled that jump (`CancelOpenEvent`) before hearing of the switch.
struct BeforeJump {
    jump_generation: u64,
    previous: Option<ActiveStream>,
    at: Instant,
}

struct DetachedForward {
    target_event: String,
    reached_start: Arc<AtomicBool>,
    reached_end: Arc<AtomicBool>,
    /// Live timeline for the same room/thread, used only to count newly arriving tail messages.
    live: Option<Arc<Timeline>>,
}

pub struct Timelines {
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    /// Most recently used last.
    cache: VecDeque<Cached>,
    /// The cache keys of what is shown: the open room, and the open thread in it. Their
    /// timelines are never dropped for being idle.
    shown: Vec<String>,
    /// Forwards the open room's timeline to the UI.
    forwarder: Option<JoinHandle<()>>,
    /// The running request for older messages, if any.
    paginator: Option<JoinHandle<()>>,
    /// The running request for newer messages on a detached timeline, if any.
    forward_paginator: Option<JoinHandle<()>>,
    /// What `forwarder` is currently streaming (`None` when no room is open).
    active: Option<ActiveStream>,
    /// Saved when a jump replaces `active`, so a late `CancelOpenEvent` can restore it.
    before_jump: Option<BeforeJump>,
    /// Background `/context` request for `OpenEvent`, so slow servers never block sync.
    event_opener: Option<JoinHandle<()>>,
    pending_open_event_gen: Option<u64>,
    open_tx: UnboundedSender<OpenedTimeline>,
    open_rx: UnboundedReceiver<OpenedTimeline>,
    media: super::media::Media,
    previews: super::previews::Previews,
    /// Running requests for member lists (for sender names).
    member_fetches: tokio::task::JoinSet<()>,
    /// Set while a command for a thread is handled: the event the thread starts at.
    /// `timeline` then answers with that thread's timeline instead of the room's.
    thread: Option<String>,
    read_marks: threads::ReadMarks,
    uploads: super::upload::Uploads,
    /// Rooms and threads (named as by `threads::key`) whose stored draft the UI was
    /// told.
    drafts_told: HashSet<String>,
    /// Current ignored users (`m.ignored_user_list`), shared with open timelines.
    ignored: watch::Receiver<IgnoredSet>,
    /// Bumped whenever our own account/room member profile syncs.
    own_refresh: watch::Receiver<u64>,
    own_room_names: super::people::OwnRoomNames,
    unread_flags: UnreadFlags,
    pinned_cache: Arc<std::sync::Mutex<super::moderation::PinnedCache>>,
    /// Lookups of saved messages at a time, over all requests.
    saved_limit: Arc<tokio::sync::Semaphore>,
    /// The running request for a page of a room's threads, if any.
    thread_lister: Option<JoinHandle<()>>,
    /// Rooms left in this session (see [`super::left`]).
    left: super::left::LeftRooms,
}

impl Timelines {
    pub fn new(
        client: Client,
        events: UnboundedSender<MatrixEvent>,
        ignored: watch::Receiver<IgnoredSet>,
        own_refresh: watch::Receiver<u64>,
        own_room_names: super::people::OwnRoomNames,
        left: super::left::LeftRooms,
    ) -> Self {
        let (open_tx, open_rx) = unbounded_channel();
        Self {
            media: super::media::Media::new(client.clone(), events.clone()),
            previews: super::previews::Previews::new(client.clone(), events.clone()),
            member_fetches: tokio::task::JoinSet::new(),
            uploads: super::upload::Uploads::new(client.clone(), events.clone()),
            client,
            cache: VecDeque::new(),
            shown: Vec::new(),
            forwarder: None,
            paginator: None,
            forward_paginator: None,
            active: None,
            before_jump: None,
            event_opener: None,
            pending_open_event_gen: None,
            open_tx,
            open_rx,
            thread: None,
            read_marks: Default::default(),
            drafts_told: HashSet::new(),
            ignored,
            own_refresh,
            own_room_names,
            unread_flags: UnreadFlags::start(events.clone()),
            pinned_cache: Arc::default(),
            saved_limit: Arc::new(tokio::sync::Semaphore::new(super::saved::CONCURRENCY)),
            thread_lister: None,
            left,
            events,
        }
    }

    /// Drops cached live timelines so the next room open reflects a changed ignore list.
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    pub(super) async fn next_opened(&mut self) -> Option<OpenedTimeline> {
        self.open_rx.recv().await
    }

    fn cancel_open_event(&mut self) {
        if let Some(task) = self.event_opener.take() {
            task.abort();
        }
        self.pending_open_event_gen = None;
    }

    pub fn close(&mut self) {
        self.cancel_open_event();
        self.active = None;
        self.before_jump = None;
        // What was shown until now was in use until now.
        let now = Instant::now();
        for key in std::mem::take(&mut self.shown) {
            if let Some(cached) = self.cache.iter_mut().find(|cached| cached.key == key) {
                cached.last_used = now;
            }
        }
        for task in [
            self.forwarder.take(),
            self.paginator.take(),
            self.forward_paginator.take(),
        ]
        .into_iter()
        .flatten()
        {
            task.abort();
        }
        // The member lists being loaded are left to finish: the UI asks for a room's
        // list right before it opens the room (which closes the one before), asks once,
        // and files the answer under its room.
    }

    /// Waits briefly on shutdown for in-flight file sends to reach the SDK's send queue.
    pub async fn finish_uploads(&mut self, wait: std::time::Duration) {
        self.uploads.finish(wait).await;
    }

    /// At the end of the session: closes the timelines, and stops a request for the
    /// threads popup too (which, unlike them, is not bound to the open room).
    pub fn stop(&mut self) {
        self.close();
        self.member_fetches.abort_all();
        if let Some(task) = self.thread_lister.take() {
            task.abort();
        }
    }

    /// Message keys arrived from outside the usual ways (a key file): has every open
    /// timeline try again what it could not decrypt. (The SDK retries by itself for
    /// keys it sees arrive; this covers what that may have missed.)
    pub async fn retry_decryption(&self) {
        let mut timelines: Vec<Arc<Timeline>> = self
            .cache
            .iter()
            .map(|cached| cached.timeline.clone())
            .collect();
        if let Some(ActiveStream {
            kind: StreamKind::Detached { timeline, .. },
            ..
        }) = &self.active
        {
            timelines.push(timeline.clone());
        }
        tokio::spawn(async move {
            for timeline in timelines {
                // Which sessions does not matter: matrix-sdk-ui 0.19.1 retries
                // everything the timeline shows as undecryptable.
                timeline
                    .retry_decryption(std::iter::empty::<String>())
                    .await;
            }
        });
    }

    /// The room a command is for. Every command finds its room here.
    fn room(&self, room_id: &str) -> Result<Room> {
        let id = RoomId::parse(room_id).map_err(|_| anyhow!("invalid room id"))?;
        let room = self
            .client
            .get_room(&id)
            .ok_or_else(|| anyhow!("this room is not known yet"))?;
        // Left here, and brought back looking joined by a sync answer that was on its
        // way (see `super::left`): as gone as it was before that answer came.
        if room.state() == matrix_sdk::RoomState::Joined && self.left.gone(&room) {
            bail!("you are not in this room any more");
        }
        Ok(room)
    }

    async fn timeline(&mut self, room_id: &str) -> Result<Arc<Timeline>> {
        let key = threads::key(room_id, self.thread.as_deref());
        if let Some(timeline) = self.cached(&key) {
            return Ok(timeline);
        }
        let room = self.room(room_id)?;
        if let Some(root) = &self.thread {
            let timeline = Arc::new(threads::open(&room, root).await?);
            // One thread at a time stays alive besides the one shown; rooms keep their
            // places. The thread shown is not this one when the UI has not yet heard
            // that a jump took it to another thread: what it sends meanwhile for the
            // thread it still shows must not drop the one its forwarder watches (the
            // next command for it would open a second timeline of it).
            let shown = &self.shown;
            self.cache
                .retain(|cached| !threads::is_thread(&cached.key) || shown.contains(&cached.key));
            self.keep(key, timeline.clone());
            return Ok(timeline);
        }
        let timeline = Arc::new(room_timeline(&room).await?);
        // Sync only delivers the members it thinks are needed, which leaves many
        // senders (bridged users above all) without a name. Fetch the member list once;
        // the timeline then fills in the names of what is already shown.
        // Skipped when the list is already complete, and stopped with the session.
        self.spawn_members_if_needed(&room, &timeline);
        self.keep(key, timeline.clone());
        // The open thread's timeline does not take a room's place.
        let rooms = |cache: &VecDeque<Cached>| {
            cache
                .iter()
                .filter(|cached| !threads::is_thread(&cached.key))
                .count()
        };
        while rooms(&self.cache) > KEEP_TIMELINES {
            let oldest = self
                .cache
                .iter()
                .position(|cached| !threads::is_thread(&cached.key));
            self.cache.remove(oldest.expect("there are rooms"));
        }
        Ok(timeline)
    }

    /// The cached timeline named `key` (see `threads::key`), now the most recently used.
    fn cached(&mut self, key: &str) -> Option<Arc<Timeline>> {
        let at = self.cache.iter().position(|cached| cached.key == key)?;
        let mut cached = self.cache.remove(at).expect("position is valid");
        cached.last_used = Instant::now();
        let timeline = cached.timeline.clone();
        self.cache.push_back(cached);
        Some(timeline)
    }

    fn keep(&mut self, key: String, timeline: Arc<Timeline>) {
        self.cache.push_back(Cached {
            key,
            timeline,
            last_used: Instant::now(),
        });
    }

    /// The room `room_id` and, if given, the thread at `thread` in it are what the UI
    /// shows from now on (until [`Self::close`]).
    fn show(&mut self, room_id: &str, thread: Option<&str>) {
        self.shown = vec![room_id.to_owned()];
        if thread.is_some() {
            self.shown.push(threads::key(room_id, thread));
        }
    }

    /// When a cached timeline that is not shown (or a detached slice stashed across a
    /// jump) will have been idle for [`IDLE_TIMELINE`], if there is one.
    fn next_idle(&self) -> Option<Instant> {
        let cached = self
            .cache
            .iter()
            .filter(|cached| !self.shown.contains(&cached.key))
            .map(|cached| cached.last_used + IDLE_TIMELINE)
            .min();
        let stashed = self
            .before_jump
            .as_ref()
            .filter(|saved| {
                matches!(
                    &saved.previous,
                    Some(ActiveStream {
                        kind: StreamKind::Detached { .. },
                        ..
                    })
                )
            })
            .map(|saved| saved.at + IDLE_TIMELINE);
        match (cached, stashed) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Ends when a cached timeline has been idle long enough to be dropped (by
    /// [`Self::drop_idle`]); never while there is none. Borrows nothing: the session loop
    /// makes it anew each time round.
    pub(super) fn idle_due(&self) -> impl Future<Output = ()> + use<> {
        let next = self.next_idle();
        async move {
            match next {
                Some(at) => tokio::time::sleep_until(at.into()).await,
                None => std::future::pending().await,
            }
        }
    }

    /// Drops the cached timelines that have not been used for [`IDLE_TIMELINE`] at `now`
    /// and are not shown, and any pre-jump view stashed for at least that long. A task
    /// still at work with a timeline (a send, a page) keeps it until it is done; the next
    /// command for the room opens a new one.
    pub(super) fn drop_idle(&mut self, now: Instant) {
        let shown = &self.shown;
        self.cache.retain(|cached| {
            shown.contains(&cached.key)
                || now.saturating_duration_since(cached.last_used) < IDLE_TIMELINE
        });
        if self
            .before_jump
            .as_ref()
            .is_some_and(|saved| now.saturating_duration_since(saved.at) >= IDLE_TIMELINE)
        {
            self.before_jump = None;
        }
    }

    fn spawn_members_if_needed(&mut self, room: &Room, timeline: &Arc<Timeline>) {
        if !room.are_members_synced() {
            let members = timeline.clone();
            while self.member_fetches.try_join_next().is_some() {}
            self.member_fetches
                .spawn(async move { members.fetch_members().await });
        }
    }

    /// Loads one page of older messages into `self.paginator` and reports completion for
    /// `generation`. Any matching `MatrixCmd::Paginate` while this runs is coalesced.
    fn spawn_paginate(
        &mut self,
        timeline: Arc<Timeline>,
        detached_start: Option<Arc<AtomicBool>>,
        generation: u64,
    ) {
        let events = self.events.clone();
        self.paginator = Some(tokio::spawn(async move {
            let reached_start = match timeline.paginate_backwards(PAGE).await {
                Ok(reached_start) => reached_start,
                Err(err) => {
                    tracing::warn!(why = %logsafe::kind(&err), "could not load older messages");
                    // Report "done" so the UI does not retry in a loop.
                    true
                }
            };
            if let Some(flag) = detached_start {
                flag.store(reached_start, Ordering::Release);
            }
            let _ = events.send(MatrixEvent::Paginated {
                generation,
                reached_start,
            });
        }));
    }

    /// Tells the UI the stored draft of a room (or of the thread a command is being
    /// handled for) when it is opened for the first time. After that the UI has the
    /// newer text, so it is told only once.
    async fn tell_draft(&mut self, room: &Room, room_id: &str) {
        let thread = self.thread.clone();
        self.tell_draft_for(room, room_id, thread.as_deref()).await;
    }

    async fn tell_draft_for(&mut self, room: &Room, room_id: &str, thread: Option<&str>) {
        if !self.drafts_told.insert(threads::key(room_id, thread)) {
            return;
        }
        match drafts::load(room, thread).await {
            Ok(text) => {
                let _ = self.events.send(MatrixEvent::Draft {
                    room_id: room_id.to_owned(),
                    thread: thread.map(str::to_owned),
                    text,
                });
            }
            Err(err) => tracing::warn!(why = %logsafe::kind_any(&err), "could not load the draft"),
        }
    }

    /// Tells the user what went wrong, in full; the log only gets what kind of error
    /// it was, as its text can name rooms, people and servers.
    fn fail(&self, what: &'static str, err: anyhow::Error) {
        tracing::warn!(why = %logsafe::kind_any(&err), "{what}");
        let _ = self
            .events
            .send(MatrixEvent::ActionFailed(format!("{what}: {err:#}")));
    }

    /// A [`MatrixCmd::SendFiles`] that could not even start is answered like one whose
    /// first file could not be sent: the files go back to the composer, and the app
    /// knows that they are not being read.
    fn files_not_sent(&self, send: u64, err: impl std::fmt::Display) {
        tracing::warn!("files could not be sent");
        let _ = self.events.send(MatrixEvent::FilesNotSent {
            send,
            reason: format!("Could not send: {err}"),
        });
    }

    /// Runs `work` in the background and reports a failure to the user.
    fn spawn<F>(&self, what: &'static str, work: F)
    where
        F: Future<Output = Result<()>> + Send + 'static,
    {
        let events = self.events.clone();
        tokio::spawn(async move {
            if let Err(err) = work.await {
                tracing::warn!(why = %logsafe::kind_any(&err), "{what}");
                let _ = events.send(MatrixEvent::ActionFailed(format!("{what}: {err:#}")));
            }
        });
    }

    /// Looks up the room's live timeline, then runs `work` with it in the background.
    async fn with_timeline<F, Fut>(&mut self, room_id: &str, what: &'static str, work: F)
    where
        F: FnOnce(Arc<Timeline>) -> Fut,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        match self.timeline(room_id).await {
            Ok(timeline) => self.spawn(what, work(timeline)),
            Err(err) => self.fail(what, err),
        }
    }

    /// Returns the detached timeline when viewing it for `(room_id, self.thread)`,
    /// otherwise the live timeline, plus its `reached_start` flag if detached.
    async fn active_timeline(
        &mut self,
        room_id: &str,
    ) -> Result<(Arc<Timeline>, Option<Arc<AtomicBool>>)> {
        if let Some(ActiveStream {
            room_id: active_room,
            thread,
            kind:
                StreamKind::Detached {
                    timeline,
                    reached_start,
                    ..
                },
            ..
        }) = &self.active
            && active_room == room_id
            && *thread == self.thread
        {
            return Ok((timeline.clone(), Some(reached_start.clone())));
        }
        Ok((self.timeline(room_id).await?, None))
    }

    /// Runs `work` against whichever timeline (detached or live) the user currently has
    /// open for `(room_id, self.thread)`.
    async fn with_active_timeline<F, Fut>(&mut self, room_id: &str, what: &'static str, work: F)
    where
        F: FnOnce(Arc<Timeline>) -> Fut,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        match self.active_timeline(room_id).await {
            Ok((timeline, _)) => self.spawn(what, work(timeline)),
            Err(err) => self.fail(what, err),
        }
    }

    async fn open_event(
        &mut self,
        room_id: String,
        event_id: String,
        thread_root: Option<String>,
        generation: u64,
    ) {
        self.cancel_open_event();
        let jump_failed = |reason: String| {
            let _ = self
                .events
                .send(MatrixEvent::JumpFailed { generation, reason });
        };
        let room = match self.room(&room_id) {
            Ok(room) => room,
            Err(err) => return jump_failed(err.to_string()),
        };
        if room.state() != matrix_sdk::RoomState::Joined {
            return jump_failed("Join this room before jumping to a message in it.".to_owned());
        }
        let Ok(parsed_event) = EventId::parse(&event_id) else {
            return jump_failed("That link does not name a valid message.".to_owned());
        };
        if let Some(root) = &thread_root
            && EventId::parse(root).is_err()
        {
            return jump_failed("That link does not name a valid thread.".to_owned());
        }

        // Fast path: if the target event is already in a warm live timeline for this room
        // (or matching thread), switch straight to it without touching `/context`.
        let cache_key = threads::key(&room_id, thread_root.as_deref());
        if let Some(at) = self.cache.iter().position(|cached| cached.key == cache_key) {
            let cached = self.cache[at].timeline.clone();
            if let Some(item) = cached.item_by_event_id(&parsed_event).await {
                if is_ignored_in(&self.ignored, item.sender()) {
                    return jump_failed("That message is from an ignored user.".to_owned());
                }
                if item.content().is_redacted() {
                    return jump_failed("That message was deleted.".to_owned());
                }
                let timeline = self.cached(&cache_key).unwrap_or(cached);
                let previous = self.active.take();
                self.close();
                self.show(&room_id, thread_root.as_deref());
                self.before_jump = Some(BeforeJump {
                    jump_generation: generation,
                    previous,
                    at: Instant::now(),
                });
                self.active = Some(ActiveStream {
                    room_id: room_id.clone(),
                    thread: thread_root.clone(),
                    generation,
                    kind: StreamKind::Live,
                });
                self.tell_draft_for(&room, &room_id, thread_root.as_deref())
                    .await;
                self.forwarder = Some(tokio::spawn(forward(
                    room,
                    timeline,
                    room_id,
                    generation,
                    self.events.clone(),
                    thread_root,
                    self.read_marks.clone(),
                    None,
                    self.ignored.clone(),
                    self.own_refresh.clone(),
                    self.own_room_names.clone(),
                )));
                return;
            }
        }

        self.pending_open_event_gen = Some(generation);
        let open_tx = self.open_tx.clone();
        let ignored = self.ignored.clone();
        self.event_opener = Some(tokio::spawn(async move {
            // Check the target event before opening a `TimelineFocus::Event` cache so an
            // ignored, MSC4406-blocked, or missing event fails cleanly without leaving a
            // broken `event_focused` entry behind in the SDK's `EventCache`.
            match room.event(&parsed_event, None).await {
                Ok(ev) => {
                    if let Some(sender) = ev.sender()
                        && is_ignored_in(&ignored, &sender)
                    {
                        let _ = open_tx.send(OpenedTimeline::Failed {
                            generation,
                            reason: "That message is from an ignored user.".to_owned(),
                        });
                        return;
                    }
                }
                Err(err) => {
                    let _ = open_tx.send(OpenedTimeline::Failed {
                        generation,
                        reason: friendly_jump_message(&format!("{err:#}")),
                    });
                    return;
                }
            }
            let thread_mode = match &thread_root {
                Some(_) => TimelineEventFocusThreadMode::ForceThread,
                None => TimelineEventFocusThreadMode::Automatic {
                    hide_threaded_events: true,
                },
            };
            let outcome = room
                .timeline_builder()
                .with_focus(TimelineFocus::Event {
                    target: parsed_event.clone(),
                    num_context_events: PAGE / 2,
                    thread_mode,
                })
                .track_read_marker_and_receipts(TimelineReadReceiptTracking::MessageLikeEvents)
                .build()
                .await;
            let timeline = match outcome {
                Ok(timeline) => timeline,
                Err(err) => {
                    let _ = open_tx.send(OpenedTimeline::Failed {
                        generation,
                        reason: friendly_jump_error(&err),
                    });
                    return;
                }
            };
            let items = timeline.items().await;
            let target = items
                .iter()
                .filter_map(|item| item.as_event())
                .find(|event| event.event_id() == Some(&parsed_event));
            let Some(target) = target else {
                let _ = open_tx.send(OpenedTimeline::Failed {
                    generation,
                    reason: "Message could not be found.".to_owned(),
                });
                return;
            };
            if is_ignored_in(&ignored, target.sender()) {
                let _ = open_tx.send(OpenedTimeline::Failed {
                    generation,
                    reason: "That message is from an ignored user.".to_owned(),
                });
                return;
            }
            if matches!(
                target.content().as_msglike().map(|c| &c.kind),
                Some(MsgLikeKind::Redacted)
            ) {
                let _ = open_tx.send(OpenedTimeline::Failed {
                    generation,
                    reason: "That message was deleted.".to_owned(),
                });
                return;
            }
            let resolved_thread = thread_root.or_else(|| {
                timeline
                    .is_threaded()
                    .then(|| target.content().thread_root().map(|id| id.to_string()))
                    .flatten()
            });
            let _ = open_tx.send(OpenedTimeline::Detached {
                room_id,
                thread: resolved_thread,
                generation,
                target_event: event_id,
                timeline: Arc::new(timeline),
            });
        }));
    }

    pub(super) async fn on_opened(&mut self, opened: OpenedTimeline) {
        match opened {
            OpenedTimeline::Failed { generation, reason } => {
                if self.pending_open_event_gen != Some(generation) {
                    return;
                }
                self.pending_open_event_gen = None;
                self.event_opener = None;
                let _ = self
                    .events
                    .send(MatrixEvent::JumpFailed { generation, reason });
            }
            OpenedTimeline::Detached {
                room_id,
                thread,
                generation,
                target_event,
                timeline,
            } => {
                if self.pending_open_event_gen != Some(generation) {
                    return;
                }
                self.pending_open_event_gen = None;
                self.event_opener = None;
                let room = match self.room(&room_id) {
                    Ok(room) if room.state() == matrix_sdk::RoomState::Joined => room,
                    Ok(_) => {
                        let _ = self.events.send(MatrixEvent::JumpFailed {
                            generation,
                            reason: "Join this room before jumping to a message in it.".to_owned(),
                        });
                        return;
                    }
                    Err(err) => {
                        let _ = self.events.send(MatrixEvent::JumpFailed {
                            generation,
                            reason: err.to_string(),
                        });
                        return;
                    }
                };
                let previous = self.active.take();
                self.close();
                let prev_thread = self.thread.take();
                self.thread = thread.clone();
                let live = self.timeline(&room_id).await.ok();
                self.thread = prev_thread;
                // The live timeline behind the slice counts what arrives meanwhile.
                self.show(&room_id, thread.as_deref());
                let reached_start = Arc::new(AtomicBool::new(false));
                let reached_end = Arc::new(AtomicBool::new(false));
                self.before_jump = Some(BeforeJump {
                    jump_generation: generation,
                    previous,
                    at: Instant::now(),
                });
                self.active = Some(ActiveStream {
                    room_id: room_id.clone(),
                    thread: thread.clone(),
                    generation,
                    kind: StreamKind::Detached {
                        target_event: target_event.clone(),
                        timeline: timeline.clone(),
                        reached_start: reached_start.clone(),
                        reached_end: reached_end.clone(),
                    },
                });
                self.spawn_members_if_needed(&room, &timeline);
                self.tell_draft_for(&room, &room_id, thread.as_deref())
                    .await;
                self.forwarder = Some(tokio::spawn(forward(
                    room,
                    timeline,
                    room_id,
                    generation,
                    self.events.clone(),
                    thread,
                    self.read_marks.clone(),
                    Some(DetachedForward {
                        target_event,
                        reached_start,
                        reached_end,
                        live,
                    }),
                    self.ignored.clone(),
                    self.own_refresh.clone(),
                    self.own_room_names.clone(),
                )));
            }
        }
    }

    /// Reopens the view that was streaming before a jump when `CancelOpenEvent` arrives
    /// after the jump already switched the worker over. Sends `TimelineReset` ahead of
    /// the new forwarder so the app does not keep stale `reached_start`/`reached_end` or
    /// pagination-in-flight flags from while the old forwarder was stopped.
    async fn restore(&mut self, previous: Option<ActiveStream>) {
        self.close();
        let Some(view) = previous else {
            return;
        };
        let Ok(room) = self.room(&view.room_id) else {
            return;
        };
        if room.state() != matrix_sdk::RoomState::Joined {
            return;
        }
        let ActiveStream {
            room_id,
            thread,
            generation,
            kind,
        } = view;
        match kind {
            StreamKind::Live => {
                let prev_thread = self.thread.take();
                self.thread = thread.clone();
                let timeline = self.timeline(&room_id).await;
                self.thread = prev_thread;
                let Ok(timeline) = timeline else {
                    return;
                };
                let need_page = match &thread {
                    Some(root) => thread_needs_page(&timeline.items().await, root),
                    None => false,
                };
                self.show(&room_id, thread.as_deref());
                self.active = Some(ActiveStream {
                    room_id: room_id.clone(),
                    thread: thread.clone(),
                    generation,
                    kind: StreamKind::Live,
                });
                let _ = self.events.send(MatrixEvent::TimelineReset {
                    room_id: room_id.clone(),
                    generation,
                });
                self.forwarder = Some(tokio::spawn(forward(
                    room,
                    timeline.clone(),
                    room_id,
                    generation,
                    self.events.clone(),
                    thread,
                    self.read_marks.clone(),
                    None,
                    self.ignored.clone(),
                    self.own_refresh.clone(),
                    self.own_room_names.clone(),
                )));
                if need_page {
                    self.spawn_paginate(timeline, None, generation);
                }
            }
            StreamKind::Detached {
                target_event,
                timeline,
                reached_start,
                reached_end,
            } => {
                let prev_thread = self.thread.take();
                self.thread = thread.clone();
                let live = self.timeline(&room_id).await.ok();
                self.thread = prev_thread;
                self.show(&room_id, thread.as_deref());
                // The cache may have reloaded while the forwarder was down; start afresh.
                reached_start.store(false, Ordering::Release);
                reached_end.store(false, Ordering::Release);
                self.active = Some(ActiveStream {
                    room_id: room_id.clone(),
                    thread: thread.clone(),
                    generation,
                    kind: StreamKind::Detached {
                        target_event: target_event.clone(),
                        timeline: timeline.clone(),
                        reached_start: reached_start.clone(),
                        reached_end: reached_end.clone(),
                    },
                });
                let _ = self.events.send(MatrixEvent::TimelineReset {
                    room_id: room_id.clone(),
                    generation,
                });
                self.forwarder = Some(tokio::spawn(forward(
                    room,
                    timeline,
                    room_id,
                    generation,
                    self.events.clone(),
                    thread,
                    self.read_marks.clone(),
                    Some(DetachedForward {
                        target_event,
                        reached_start,
                        reached_end,
                        live,
                    }),
                    self.ignored.clone(),
                    self.own_refresh.clone(),
                    self.own_room_names.clone(),
                )));
            }
        }
    }

    pub async fn handle(&mut self, command: MatrixCmd) {
        match command {
            // Polls and stickers have a function of their own, on the heap: the SDK calls
            // in them are large, and everything in this function adds to the stack it
            // needs (twice, where a command is handled inside another).
            command @ (MatrixCmd::SendSticker { .. }
            | MatrixCmd::SendPoll { .. }
            | MatrixCmd::EditPoll { .. }
            | MatrixCmd::VotePoll { .. }
            | MatrixCmd::EndPoll { .. }) => {
                Box::pin(self.handle_poll_or_sticker(command)).await;
            }
            MatrixCmd::OpenRoom {
                room_id,
                generation,
            } => {
                self.close();
                let room = match self.room(&room_id) {
                    Ok(room) => room,
                    Err(err) => return self.fail("Could not open the room", err),
                };
                // An invitation has no readable timeline yet.
                if room.state() != matrix_sdk::RoomState::Joined {
                    return;
                }
                self.tell_draft(&room, &room_id).await;
                match self.timeline(&room_id).await {
                    Ok(timeline) => {
                        // A freshly created `TimelineFocus::Thread` starts from an empty
                        // `ThreadEventCache` unless sync happened to carry the root while
                        // the cache already existed; its root is only loaded once a
                        // backwards page reaches the start of `/relations`. Starting that
                        // first page here (through `self.paginator`, so a matching
                        // `MatrixCmd::Paginate` from the UI coalesces into it) ensures
                        // `OpenThread` never stays empty.
                        let need_page = match &self.thread {
                            Some(root) => thread_needs_page(&timeline.items().await, root),
                            None => false,
                        };
                        let thread = self.thread.clone();
                        self.show(&room_id, thread.as_deref());
                        self.active = Some(ActiveStream {
                            room_id: room_id.clone(),
                            thread: thread.clone(),
                            generation,
                            kind: StreamKind::Live,
                        });
                        self.forwarder = Some(tokio::spawn(forward(
                            room,
                            timeline.clone(),
                            room_id,
                            generation,
                            self.events.clone(),
                            self.thread.clone(),
                            self.read_marks.clone(),
                            None,
                            self.ignored.clone(),
                            self.own_refresh.clone(),
                            self.own_room_names.clone(),
                        )));
                        if need_page {
                            self.spawn_paginate(timeline, None, generation);
                        }
                    }
                    Err(err) => self.fail("Could not open the room", err),
                }
            }
            // No room is shown any more: nothing is forwarded, loaded or looked up
            // for one. The timelines stay cached for the next time they are opened.
            MatrixCmd::CloseRoom => self.close(),
            MatrixCmd::OpenEvent {
                room_id,
                event_id,
                thread_root,
                generation,
            } => {
                self.open_event(room_id, event_id, thread_root, generation)
                    .await;
            }
            MatrixCmd::ListThreads {
                room_id,
                from,
                mine,
                generation,
            } => self.list_threads(room_id, from, mine, generation),
            MatrixCmd::CancelOpenEvent { generation } => {
                if self.pending_open_event_gen == Some(generation) {
                    self.cancel_open_event();
                } else if let Some(saved) = self
                    .before_jump
                    .take_if(|saved| saved.jump_generation == generation)
                {
                    self.restore(saved.previous).await;
                }
            }
            MatrixCmd::Paginate {
                room_id,
                generation,
            } => {
                // Asking to page the jump's generation means the UI adopted it; drop any
                // stashed pre-jump view right away.
                if self
                    .before_jump
                    .as_ref()
                    .is_some_and(|saved| saved.jump_generation == generation)
                {
                    self.before_jump = None;
                }
                // One request at a time per open room.
                if self
                    .paginator
                    .as_ref()
                    .is_some_and(|task| !task.is_finished())
                {
                    return;
                }
                match self.active_timeline(&room_id).await {
                    Ok((timeline, detached_start)) => {
                        self.spawn_paginate(timeline, detached_start, generation);
                    }
                    Err(err) => {
                        self.fail("Could not load older messages", err);
                        // The UI waits for an answer to every request.
                        let _ = self.events.send(MatrixEvent::Paginated {
                            generation,
                            reached_start: true,
                        });
                    }
                }
            }
            MatrixCmd::PaginateForwards {
                room_id,
                generation,
            } => {
                if self
                    .before_jump
                    .as_ref()
                    .is_some_and(|saved| saved.jump_generation == generation)
                {
                    self.before_jump = None;
                }
                if self
                    .forward_paginator
                    .as_ref()
                    .is_some_and(|task| !task.is_finished())
                {
                    return;
                }
                let Some(ActiveStream {
                    room_id: active_room,
                    thread,
                    generation: active_gen,
                    kind:
                        StreamKind::Detached {
                            timeline,
                            reached_end,
                            ..
                        },
                }) = &self.active
                else {
                    let _ = self.events.send(MatrixEvent::PaginatedForwards {
                        generation,
                        reached_end: true,
                    });
                    return;
                };
                if *active_room != room_id || *thread != self.thread || *active_gen != generation {
                    let _ = self.events.send(MatrixEvent::PaginatedForwards {
                        generation,
                        reached_end: true,
                    });
                    return;
                }
                let timeline = timeline.clone();
                let reached_end_flag = reached_end.clone();
                let events = self.events.clone();
                self.forward_paginator = Some(tokio::spawn(async move {
                    let reached_end = match timeline.paginate_forwards(PAGE).await {
                        Ok(reached_end) => reached_end,
                        Err(err) => {
                            tracing::warn!(why = %logsafe::kind(&err), "could not load newer messages");
                            true
                        }
                    };
                    reached_end_flag.store(reached_end, Ordering::Release);
                    let _ = events.send(MatrixEvent::PaginatedForwards {
                        generation,
                        reached_end,
                    });
                }));
            }
            MatrixCmd::Send {
                room_id,
                message,
                reply_to,
            } => {
                self.with_timeline(&room_id, "Could not send", |timeline| async move {
                    match reply_to {
                        Some(event_id) => {
                            let event_id = EventId::parse(&event_id)
                                .map_err(|_| anyhow!("invalid event id"))?;
                            timeline
                                .send_reply(content_without_relation(&message)?, event_id)
                                .await?;
                        }
                        None => {
                            timeline.send(content(&message)?.into()).await?;
                        }
                    }
                    Ok(())
                })
                .await;
            }
            MatrixCmd::Edit {
                room_id,
                item,
                message,
            } => {
                self.with_active_timeline(&room_id, "Could not edit", |timeline| async move {
                    let content = EditedContent::RoomMessage(content_without_relation(&message)?);
                    timeline.edit(&item_id(&item)?, content).await?;
                    Ok(())
                })
                .await;
            }
            MatrixCmd::ToggleReaction {
                room_id,
                item,
                key,
                shortcode,
            } => {
                let extra = shortcode
                    .as_deref()
                    .and_then(super::emotes::reaction_shortcode);
                self.with_active_timeline(&room_id, "Could not react", |timeline| async move {
                    timeline
                        .toggle_reaction_with_extra_content(&item_id(&item)?, &key, extra)
                        .await?;
                    Ok(())
                })
                .await;
            }
            MatrixCmd::Redact {
                room_id,
                item,
                reason,
            } => {
                let cache = self.pinned_cache.clone();
                let rid = room_id.clone();
                let clean_reason = reason
                    .as_deref()
                    .map(crate::text::sanitize)
                    .filter(|s| !crate::text::is_blank(s));
                self.with_active_timeline(&room_id, "Could not delete", |timeline| async move {
                    let target = item_id(&item)?;
                    timeline.redact(&target, clean_reason.as_deref()).await?;
                    if let ItemRef::Event(id) = item {
                        cache
                            .lock()
                            .expect("pinned cache poisoned")
                            .invalidate_room_event(&rid, &id);
                    }
                    Ok(())
                })
                .await;
            }
            MatrixCmd::Pin {
                room_id,
                event_id,
                pinned,
            } => {
                let room = match self.room(&room_id) {
                    Ok(room) => room,
                    Err(err) => {
                        return self.fail("Could not update pinned message", err);
                    }
                };
                let events = self.events.clone();
                let cache = self.pinned_cache.clone();
                tokio::spawn(async move {
                    match super::moderation::pin(&room, &event_id, pinned, &cache).await {
                        Ok(ids) => {
                            let _ = events.send(MatrixEvent::PinnedEvents { room_id, ids });
                        }
                        Err(err) => {
                            let _ = events.send(MatrixEvent::ActionFailed(err));
                        }
                    }
                });
            }
            MatrixCmd::FetchPinned { room_id } => {
                let room = match self.room(&room_id) {
                    Ok(room) => room,
                    Err(err) => {
                        let _ = self.events.send(MatrixEvent::PinnedList {
                            room_id,
                            items: Vec::new(),
                            error: Some(err.to_string()),
                        });
                        return;
                    }
                };
                let events = self.events.clone();
                let cache = self.pinned_cache.clone();
                let ignored = self.ignored.borrow().clone();
                tokio::spawn(async move {
                    let (items, error) =
                        super::moderation::fetch_pinned(&room, &cache, &ignored).await;
                    let _ = events.send(MatrixEvent::PinnedList {
                        room_id,
                        items,
                        error,
                    });
                });
            }
            MatrixCmd::Saved(super::SavedCmd::Fetch { request, items }) => {
                use super::saved::Place;
                let wanted = items
                    .into_iter()
                    .take(super::saved::MAX_FETCH)
                    .map(|(room_id, event_id)| {
                        // A room the account has left is known still, but not for
                        // reading. One that is not known at all may yet come.
                        let place = match self.room(&room_id) {
                            Ok(room) =>
                            {
                                #[expect(
                                    clippy::wildcard_enum_match_arm,
                                    reason = "foreign matrix_sdk::RoomState"
                                )]
                                match room.state() {
                                    matrix_sdk::RoomState::Joined => Place::Joined(room),
                                    matrix_sdk::RoomState::Left | matrix_sdk::RoomState::Banned => {
                                        Place::Left
                                    }
                                    _ => Place::Unknown,
                                }
                            }
                            Err(_) => Place::Unknown,
                        };
                        (room_id, event_id, place)
                    })
                    .collect();
                tokio::spawn(super::saved::fetch(
                    request,
                    wanted,
                    self.pinned_cache.clone(),
                    self.saved_limit.clone(),
                    self.events.clone(),
                ));
            }
            MatrixCmd::Forward {
                from_room_id,
                event_id,
                to_room_id,
            } => {
                let from_room = match self.room(&from_room_id) {
                    Ok(room) => room,
                    Err(err) => {
                        let _ = self.events.send(MatrixEvent::ForwardResult {
                            to_room_id,
                            to_room_name: String::new(),
                            result: Err(err.to_string()),
                        });
                        return;
                    }
                };
                let to_room = match self.room(&to_room_id) {
                    Ok(room) => room,
                    Err(err) => {
                        let _ = self.events.send(MatrixEvent::ForwardResult {
                            to_room_id,
                            to_room_name: String::new(),
                            result: Err(err.to_string()),
                        });
                        return;
                    }
                };
                let client = self.client.clone();
                let events = self.events.clone();
                tokio::spawn(async move {
                    let raw_name = match to_room.cached_display_name() {
                        Some(name) => name.to_string(),
                        None => to_room
                            .display_name()
                            .await
                            .map(|name| name.to_string())
                            .unwrap_or_else(|_| to_room_id.clone()),
                    };
                    let to_room_name = crate::text::sanitize_line(&raw_name);
                    let result = super::moderation::forward_message(
                        &client, &from_room, &to_room, &event_id,
                    )
                    .await;
                    let _ = events.send(MatrixEvent::ForwardResult {
                        to_room_id,
                        to_room_name,
                        result,
                    });
                });
            }
            MatrixCmd::Retry { room_id, item } => {
                self.with_timeline(&room_id, "Could not retry", |timeline| async move {
                    let wanted = item_id(&item)?;
                    let items = timeline.items().await;
                    let handle = items
                        .iter()
                        .filter_map(|item| item.as_event())
                        .find(|event| event.identifier() == wanted)
                        .and_then(|event| event.local_echo_send_handle())
                        .ok_or_else(|| anyhow!("this message is no longer waiting"))?;
                    handle.unwedge().await?;
                    // The SDK stops a room's queue after any failure; nothing would
                    // be tried again without this.
                    timeline.room().send_queue().set_enabled(true);
                    Ok(())
                })
                .await;
            }
            MatrixCmd::RetryDecryption { room_id, event_id } => {
                let events = self.events.clone();
                let timeline = self.active_timeline(&room_id).await;
                let answer = move |result: Result<super::Retried, String>, event_id: String| {
                    let _ = events.send(MatrixEvent::DecryptionRetried {
                        room_id,
                        event_id,
                        result,
                    });
                };
                match timeline {
                    Ok((timeline, _)) => {
                        tokio::spawn(async move {
                            let result = decrypt_again(&timeline, &event_id).await;
                            if let Err(err) = &result {
                                tracing::warn!(why = %logsafe::kind_any(err), "could not retry decryption");
                            }
                            answer(result.map_err(|err| format!("{err:#}")), event_id);
                        });
                    }
                    Err(err) => answer(Err(format!("{err:#}")), event_id),
                }
            }
            MatrixCmd::FetchSource {
                request,
                room_id,
                item,
            } => {
                let events = self.events.clone();
                let answer = move |result: Result<super::EventSource, String>| {
                    let _ = events.send(MatrixEvent::Source {
                        request,
                        result: result.map(Box::new),
                    });
                };
                match self.active_timeline(&room_id).await {
                    Ok((timeline, _)) => {
                        tokio::spawn(async move {
                            answer(super::source::fetch(&timeline, &item).await);
                        });
                    }
                    Err(err) => answer(Err(format!("{err:#}"))),
                }
            }
            MatrixCmd::Typing { room_id, typing } => {
                if let Ok(room) = self.room(&room_id) {
                    tokio::spawn(async move {
                        if let Err(err) = room.typing_notice(typing).await {
                            tracing::debug!(why = %logsafe::kind(&err), "could not send typing notice");
                        }
                    });
                }
            }
            MatrixCmd::MarkRead { room_id, public } => {
                let receipt = receipt_type(public);
                let thread = self
                    .thread
                    .as_deref()
                    .map(|root| threads::key(&room_id, Some(root)));
                if let Ok(timeline) = self.timeline(&room_id).await {
                    // A thread that was read is remembered here as well (see
                    // `ReadMarks`): what its newest reply is now, in the order of the
                    // requests, and only once the receipt went through.
                    let mark = match thread {
                        Some(key) => Some((
                            key,
                            self.read_marks.ticket(),
                            threads::newest_reply(&timeline).await,
                        )),
                        None => None,
                    };
                    let marks = self.read_marks.clone();
                    let flags = self.unread_flags.clone();
                    tokio::spawn(async move {
                        let Some((key, ticket, newest)) = mark else {
                            // The room itself: the read marker follows the receipt.
                            if let Err(err) = mark_room_read(&timeline, receipt, &flags).await {
                                tracing::debug!(why = %logsafe::kind_any(&err), "could not mark the room as read");
                            }
                            return;
                        };
                        match timeline.mark_as_read(receipt).await {
                            // Also when nothing had to be sent: it was read already.
                            Ok(_) => {
                                if !newest.is_empty() {
                                    marks.set(key, ticket, newest);
                                }
                            }
                            Err(err) => {
                                tracing::debug!(why = %logsafe::kind(&err), "could not send read receipt")
                            }
                        }
                    });
                }
            }
            MatrixCmd::SetUnread { room_id, unread } => match self.room(&room_id) {
                Ok(room) => self.unread_flags.set(room, unread),
                Err(err) => self.fail("Could not mark the room", err),
            },
            MatrixCmd::MarkRoomsRead { room_ids, public } => {
                let receipt = receipt_type(public);
                // Timelines that are open anyway are used; the others are opened for
                // the moment only, so they do not push the recently used ones out.
                let rooms: Vec<(Room, Option<Arc<Timeline>>)> = room_ids
                    .iter()
                    .filter_map(|room_id| {
                        let room = self.room(room_id).ok()?;
                        let open = self
                            .cache
                            .iter()
                            .find(|cached| cached.key == *room_id)
                            .map(|cached| cached.timeline.clone());
                        Some((room, open))
                    })
                    .collect();
                let flags = self.unread_flags.clone();
                self.spawn("Could not mark everything as read", async move {
                    let mut failed = 0;
                    for (room, open) in rooms {
                        if room.state() != matrix_sdk::RoomState::Joined {
                            continue;
                        }
                        let done = async {
                            let timeline = match open {
                                Some(timeline) => timeline,
                                None => Arc::new(room_timeline(&room).await?),
                            };
                            mark_room_read(&timeline, receipt.clone(), &flags).await
                        };
                        if let Err(err) = done.await {
                            tracing::debug!(why = %logsafe::kind_any(&err), "could not mark a room as read");
                            failed += 1;
                        }
                    }
                    if failed > 0 {
                        bail!("{failed} rooms could not be marked");
                    }
                    Ok(())
                });
            }
            // Accepting an invitation.
            MatrixCmd::Join { room_id } => match self.room(&room_id) {
                Ok(room) => {
                    let left = self.left.clone();
                    self.spawn("Could not join", async move {
                        room.join().await?;
                        // In it: no longer a room that was left. Only now: until the join
                        // went through, one left here stays as gone as it was.
                        left.clear(room.room_id());
                        Ok(())
                    });
                }
                Err(err) => self.fail("Could not join", err),
            },
            // Declining an invitation, or taking a knock back.
            MatrixCmd::Leave { room_id } => match self.room(&room_id) {
                Ok(room) => {
                    let client = self.client.clone();
                    let left = self.left.clone();
                    self.spawn("Could not leave", async move {
                        let was_knocked = room.state() == matrix_sdk::RoomState::Knocked;
                        room.leave().await?;
                        // Remembered like any room left, so that a sync answer that was on
                        // its way cannot bring it back as one the user is in. (A new
                        // invitation still shows: that is news.)
                        left.mark(room.room_id());
                        if was_knocked {
                            super::manage::cleanup_left_room(&client, &room).await;
                        }
                        Ok(())
                    });
                }
                Err(err) => self.fail("Could not leave", err),
            },
            MatrixCmd::FetchAvatar { url } => self.media.fetch_avatar(url),
            MatrixCmd::FetchEmotePicture { url } => self.media.fetch_emote(url),
            MatrixCmd::LinkPreview { url } => self.previews.fetch(url),
            MatrixCmd::FetchImage {
                key,
                media,
                size,
                full,
            } => self.media.fetch(key, *media, size, full),
            MatrixCmd::SaveFile { file, dir, open } => self.media.save(file, dir, open),
            MatrixCmd::FetchAudio { id, file, dir } => self.media.fetch_audio(id, file, dir),
            MatrixCmd::CancelAudio => self.media.cancel_audio(),
            MatrixCmd::FetchSpeech { id, file, dir } => self.media.fetch_speech(id, file, dir),
            MatrixCmd::CancelSpeech { id } => self.media.cancel_speech(id),
            MatrixCmd::OpenThread {
                room_id,
                root,
                generation,
            } => {
                let open = MatrixCmd::OpenRoom {
                    room_id,
                    generation,
                };
                self.thread = Some(root);
                Box::pin(self.handle(open)).await;
                self.thread = None;
            }
            MatrixCmd::InThread { root, command } => {
                self.thread = Some(root);
                Box::pin(self.handle(*command)).await;
                self.thread = None;
            }
            MatrixCmd::SendFiles {
                send,
                room_id,
                files,
                caption,
                reply_to,
            } => {
                let reply_to = match reply_to.map(EventId::parse).transpose() {
                    Ok(reply_to) => reply_to,
                    Err(_) => return self.files_not_sent(send, "invalid event id"),
                };
                match self.timeline(&room_id).await {
                    Ok(timeline) => self.uploads.send(send, timeline, files, caption, reply_to),
                    Err(err) => self.files_not_sent(send, format!("{err:#}")),
                }
            }
            MatrixCmd::FetchMembers { room_id } => {
                let Ok(room) = self.room(&room_id) else {
                    return;
                };
                if room.state() != matrix_sdk::RoomState::Joined {
                    return;
                }
                while self.member_fetches.try_join_next().is_some() {}
                let events = self.events.clone();
                self.member_fetches.spawn(async move {
                    if let Some(event) = load_room_members(room, room_id).await {
                        let _ = events.send(event);
                    }
                });
            }
            MatrixCmd::SaveDraft { room_id, text } => {
                // Awaited here, not in a task of its own: drafts reach the store in
                // the order they were written, and the one the UI sends right before
                // `Shutdown` is on disk before the session loop gets to see that.
                // A failure is only logged; the text is still in the composer.
                let saved = match self.room(&room_id) {
                    Ok(room) => drafts::save(&room, self.thread.as_deref(), text).await,
                    Err(err) => Err(err),
                };
                if let Err(err) = saved {
                    tracing::warn!(why = %logsafe::kind_any(&err), "could not save the draft");
                }
            }
            MatrixCmd::ForgetDrafts { room_id, threads } => {
                // Told again, should the room come back.
                let in_room = threads::key(&room_id, Some(""));
                self.drafts_told
                    .retain(|key| *key != room_id && !key.starts_with(&in_room));
                // The list can lose a room for a moment; its drafts stay then.
                let still_in = self.room(&room_id).is_ok_and(|room| {
                    matches!(
                        room.state(),
                        matrix_sdk::RoomState::Joined
                            | matrix_sdk::RoomState::Invited
                            | matrix_sdk::RoomState::Knocked
                    )
                });
                if !still_in
                    && let Err(err) = drafts::forget(&self.client, &room_id, &threads).await
                {
                    tracing::warn!(why = %logsafe::kind_any(&err), "could not remove the drafts of a room");
                }
            }
            // Handled by the session loop.
            MatrixCmd::LoginPassword { .. }
            | MatrixCmd::LoginBrowser { .. }
            | MatrixCmd::ReauthPassword { .. }
            | MatrixCmd::ReauthBrowser
            | MatrixCmd::CancelLogin
            | MatrixCmd::Recover { .. }
            | MatrixCmd::Logout
            | MatrixCmd::Previews(_)
            | MatrixCmd::Shutdown
            | MatrixCmd::Verification(_)
            | MatrixCmd::Manage(_)
            | MatrixCmd::SearchMessages { .. }
            | MatrixCmd::CancelSearch
            | MatrixCmd::IgnoreUser { .. }
            | MatrixCmd::UnignoreUser { .. }
            | MatrixCmd::ReportContent { .. }
            | MatrixCmd::SearchUsers { .. }
            | MatrixCmd::Devices(_)
            | MatrixCmd::Keywords(_)
            | MatrixCmd::Agent(_)
            | MatrixCmd::Collect(_)
            | MatrixCmd::CancelUserSearch
            | MatrixCmd::SearchDirectory { .. }
            | MatrixCmd::DirectoryNextPage { .. }
            | MatrixCmd::CancelDirectorySearch
            | MatrixCmd::EnsureEmotePacks { .. }
            | MatrixCmd::FetchUserProfile { .. }
            | MatrixCmd::SetOwnDisplayName { .. }
            | MatrixCmd::SetOwnAvatar { .. }
            | MatrixCmd::RemoveOwnAvatar => {}
        }
    }

    /// Asks for a page of the room's threads in the background, in place of a request
    /// that may still be running (its answer would be dropped as stale anyway).
    fn list_threads(&mut self, room_id: String, from: Option<String>, mine: bool, generation: u64) {
        if let Some(task) = self.thread_lister.take() {
            task.abort();
        }
        let events = self.events.clone();
        let room = match self.room(&room_id) {
            Ok(room) if room.state() == matrix_sdk::RoomState::Joined => room,
            _ => {
                let _ = events.send(MatrixEvent::ThreadsFailed {
                    room_id,
                    generation,
                });
                return;
            }
        };
        let read = self.read_marks.clone();
        let ignored = self.ignored.borrow().clone();
        self.thread_lister = Some(tokio::spawn(async move {
            let event = match threads::list(&room, from, mine, &read, &ignored).await {
                Some((threads, next)) => MatrixEvent::Threads(threads::ThreadPage {
                    room_id,
                    generation,
                    threads,
                    next,
                }),
                None => MatrixEvent::ThreadsFailed {
                    room_id,
                    generation,
                },
            };
            let _ = events.send(event);
        }));
    }

    async fn handle_poll_or_sticker(&mut self, command: MatrixCmd) {
        #[expect(
            clippy::wildcard_enum_match_arm,
            reason = "only called from handle for poll and sticker commands"
        )]
        match command {
            MatrixCmd::SendSticker {
                room_id,
                name,
                url,
                dimensions,
                size,
                mime,
            } => {
                self.with_timeline(&room_id, "Could not send sticker", |timeline| async move {
                    let content = sticker_content(&name, url, dimensions, size, mime)?;
                    timeline.send(content.into()).await?;
                    Ok(())
                })
                .await;
            }
            MatrixCmd::SendPoll {
                room_id,
                question,
                answers,
                disclosed,
                max_selections,
            } => {
                self.with_timeline(&room_id, "Could not start poll", |timeline| async move {
                    let (block, fallback) =
                        poll_start_block(&question, &answers, disclosed, max_selections)?;
                    let content = matrix_sdk::ruma::events::poll::unstable_start::UnstablePollStartEventContent::New(
                        matrix_sdk::ruma::events::poll::unstable_start::NewUnstablePollStartEventContent::plain_text(
                            fallback, block,
                        ),
                    );
                    timeline.send(content.into()).await?;
                    Ok(())
                })
                .await;
            }
            MatrixCmd::EditPoll {
                room_id,
                item,
                question,
                answers,
                disclosed,
                max_selections,
            } => {
                self.with_active_timeline(&room_id, "Could not edit poll", |timeline| async move {
                    let (block, fallback) =
                        poll_start_block(&question, &answers, disclosed, max_selections)?;
                    let edited = EditedContent::PollStart {
                        fallback_text: fallback,
                        new_content: block,
                    };
                    timeline.edit(&item_id(&item)?, edited).await?;
                    Ok(())
                })
                .await;
            }
            MatrixCmd::VotePoll {
                room_id,
                poll_event_id,
                answers,
            } => {
                self.with_active_timeline(&room_id, "Could not vote", |timeline| async move {
                    let poll_id = EventId::parse(&poll_event_id)
                        .map_err(|_| anyhow!("invalid poll event id"))?;
                    let content = matrix_sdk::ruma::events::poll::unstable_response::UnstablePollResponseEventContent::new(
                        answers, poll_id,
                    );
                    timeline.send(content.into()).await?;
                    Ok(())
                })
                .await;
            }
            MatrixCmd::EndPoll {
                room_id,
                poll_event_id,
            } => {
                self.with_active_timeline(&room_id, "Could not end poll", |timeline| async move {
                    let poll_id = EventId::parse(&poll_event_id)
                        .map_err(|_| anyhow!("invalid poll event id"))?;
                    let fallback = end_poll_fallback(&timeline, &poll_id).await;
                    let content = matrix_sdk::ruma::events::poll::unstable_end::UnstablePollEndEventContent::new(
                        fallback, poll_id,
                    );
                    timeline.send(content.into()).await?;
                    Ok(())
                })
                .await;
            }
            _ => {}
        }
    }
}

/// Tries once more to read a message that could not be decrypted: fetches its key
/// from the key backup (if this device can use the backup) and has the SDK decrypt
/// again.
///
/// The SDK asks the user's other devices for a missing key by itself, once per
/// session; matrix-sdk 0.19.1 offers no way to ask them again.
async fn decrypt_again(timeline: &Timeline, event_id: &str) -> Result<super::Retried> {
    use matrix_sdk_ui::timeline::EncryptedMessage;

    use super::Retried;
    let event_id = EventId::parse(event_id).map_err(|_| anyhow!("invalid event id"))?;
    let Some(found) = timeline.item_by_event_id(&event_id).await else {
        return Err(anyhow!("this message is no longer loaded"));
    };
    let session_id = match found.content().as_unable_to_decrypt() {
        // Readable by now.
        None => return Ok(Retried::Decrypted),
        Some(EncryptedMessage::MegolmV1AesSha2 { session_id, .. }) => session_id.clone(),
        Some(_) => return Err(anyhow!("this kind of encryption is not supported")),
    };
    let room = timeline.room();
    let backups = room.client().encryption().backups();
    // `Ok(false)`: this device has no access to a backup. An error is usual too: the
    // backup does not have this key.
    if let Err(err) = backups.download_room_key(room.room_id(), &session_id).await {
        tracing::debug!(why = %logsafe::kind(&err), "no key from the backup");
    }
    timeline.retry_decryption([session_id]).await;
    // The answer shows in the timeline; it takes a moment.
    for _ in 0..15 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        match timeline.item_by_event_id(&event_id).await {
            Some(now) if now.content().as_unable_to_decrypt().is_some() => {}
            Some(_) => return Ok(Retried::Decrypted),
            // Decrypted into something that is not a message of its own, or dropped
            // from what the timeline holds: not ours to call a success.
            None => return Ok(Retried::Gone),
        }
    }
    Ok(Retried::StillLocked)
}

fn item_id(item: &ItemRef) -> Result<TimelineEventItemId> {
    Ok(match item {
        ItemRef::Event(id) => TimelineEventItemId::EventId(
            EventId::parse(id).map_err(|_| anyhow!("invalid event id"))?,
        ),
        ItemRef::Transaction(id) => {
            TimelineEventItemId::TransactionId(OwnedTransactionId::from(id.as_str()))
        }
    })
}

/// The `m.sticker` event for a sticker of a pack: the picture's address as the pack
/// gives it, the name as the text that stands for the picture, and what the pack says
/// about the file.
fn sticker_content(
    name: &str,
    url: String,
    dimensions: Option<(u32, u32)>,
    size: Option<u64>,
    mime: Option<String>,
) -> Result<matrix_sdk::ruma::events::sticker::StickerEventContent> {
    let mxc: matrix_sdk::ruma::OwnedMxcUri = url.into();
    if !mxc.is_valid() {
        bail!("invalid sticker mxc URL");
    }
    let mut info = matrix_sdk::ruma::events::room::ImageInfo::new();
    if let Some((w, h)) = dimensions {
        info.width = Some(w.into());
        info.height = Some(h.into());
    }
    if let Some(sz) = size.and_then(|s| matrix_sdk::ruma::UInt::try_from(s).ok()) {
        info.size = Some(sz);
    }
    if let Some(m) = mime {
        info.mimetype = Some(m);
    }
    let body = crate::text::sanitize_line(name);
    Ok(matrix_sdk::ruma::events::sticker::StickerEventContent::new(
        body, info, mxc,
    ))
}

fn poll_start_block(
    question: &str,
    answers: &[String],
    disclosed: bool,
    max_selections: u32,
) -> Result<(
    matrix_sdk::ruma::events::poll::unstable_start::UnstablePollStartContentBlock,
    String,
)> {
    use matrix_sdk::ruma::events::poll::start::PollKind;
    use matrix_sdk::ruma::events::poll::unstable_start::{
        UnstablePollAnswer, UnstablePollAnswers, UnstablePollStartContentBlock,
    };

    let clean_question = crate::text::sanitize_line(question.trim());
    if clean_question.is_empty() {
        bail!("poll question cannot be empty");
    }
    let clean_answers: Vec<String> = answers
        .iter()
        .map(|s| crate::text::sanitize_line(s.trim()))
        .filter(|s| !s.is_empty())
        .collect();
    if clean_answers.len() < 2 {
        bail!("a poll needs at least two answers");
    }
    if clean_answers.len() > 20 {
        bail!("a poll can have at most 20 answers");
    }
    let cap = max_selections.clamp(1, clean_answers.len() as u32);
    let poll_answers: Vec<UnstablePollAnswer> = clean_answers
        .iter()
        .enumerate()
        .map(|(i, text)| UnstablePollAnswer::new(format!("a{}", i + 1), text.clone()))
        .collect();
    let wrapped = UnstablePollAnswers::try_from(poll_answers)
        .map_err(|err| anyhow!("invalid poll answers: {err}"))?;
    let mut block = UnstablePollStartContentBlock::new(clean_question.clone(), wrapped);
    block.kind = if disclosed {
        PollKind::Disclosed
    } else {
        PollKind::Undisclosed
    };
    block.max_selections = cap.into();
    let lines = clean_answers
        .iter()
        .enumerate()
        .map(|(i, ans)| format!("{}. {ans}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    Ok((block, format!("{clean_question}\n{lines}")))
}

async fn end_poll_fallback(timeline: &Timeline, poll_id: &EventId) -> String {
    let items = timeline.items().await;
    for item in items.iter() {
        let Some(event) = item.as_event() else {
            continue;
        };
        if event.event_id() != Some(poll_id) {
            continue;
        }
        if let TimelineItemContent::MsgLike(msg) = event.content()
            && let MsgLikeKind::Poll(poll) = &msg.kind
        {
            let results = poll.results();
            let counts: Vec<(&str, usize)> = results
                .answers
                .iter()
                .map(|a| {
                    let c = results.votes.get(&a.id).map_or(0, Vec::len);
                    (a.text.as_str(), c)
                })
                .collect();
            let top = counts.iter().map(|(_, c)| *c).max().unwrap_or(0);
            if top == 0 {
                return "The poll has ended with no votes.".to_owned();
            }
            let winners: Vec<&str> = counts
                .into_iter()
                .filter(|(_, c)| *c == top)
                .map(|(t, _)| t)
                .collect();
            return match winners.as_slice() {
                [one] => format!("Poll ended. Top answer: {one}"),
                many => format!("Poll ended. Top answers: {}", many.join(", ")),
            };
        }
    }
    "Poll ended.".to_owned()
}

const MAX_COMPLETION_MEMBERS: usize = 50_000;

async fn load_room_members(room: Room, room_id: String) -> Option<MatrixEvent> {
    let raw_members = if room.are_members_synced() {
        room.members_no_sync(RoomMemberships::JOIN).await
    } else {
        room.members(RoomMemberships::JOIN).await
    };
    let raw_members = match raw_members {
        Ok(members) => members,
        Err(err) => {
            tracing::warn!(why = %logsafe::kind(&err), "could not load room members");
            return None;
        }
    };
    let own = room.own_user_id().to_owned();
    let can_mention_room = room
        .power_levels()
        .await
        .is_ok_and(|pls| pls.user_can_trigger_room_notification(&own));
    let members = raw_members
        .into_iter()
        .take(MAX_COMPLETION_MEMBERS)
        .map(|member| {
            let user_id = crate::text::sanitize_line(member.user_id().as_str());
            let name = crate::text::sanitize_line(member.name());
            crate::complete::Member {
                display_name: if name.is_empty() {
                    user_id.clone()
                } else {
                    name
                },
                user_id,
            }
        })
        .collect();
    Some(MatrixEvent::RoomMembers {
        room_id,
        members,
        can_mention_room,
    })
}

pub(super) fn content(message: &Outgoing) -> Result<RoomMessageEventContent> {
    Ok(match message {
        Outgoing::Text {
            plain,
            html: None,
            mentions,
        } => apply_mentions(RoomMessageEventContent::text_plain(plain), mentions),
        Outgoing::Text {
            plain,
            html: Some(html),
            mentions,
        } => apply_mentions(RoomMessageEventContent::text_html(plain, html), mentions),
        Outgoing::Emote {
            plain,
            html: None,
            mentions,
        } => apply_mentions(RoomMessageEventContent::emote_plain(plain), mentions),
        Outgoing::Emote {
            plain,
            html: Some(html),
            mentions,
        } => apply_mentions(RoomMessageEventContent::emote_html(plain, html), mentions),
        Outgoing::Rejected(reason) => bail!("{reason}"),
    })
}

fn apply_mentions(
    mut content: RoomMessageEventContent,
    mentions: &Option<Mentions>,
) -> RoomMessageEventContent {
    if let Some(mentions) = mentions {
        content = content.add_mentions(mentions.clone());
    }
    content
}

fn content_without_relation(message: &Outgoing) -> Result<RoomMessageEventContentWithoutRelation> {
    Ok(content(message)?.into())
}

/// The timeline of a room as the app shows it.
async fn room_timeline(room: &Room) -> Result<Timeline> {
    room.timeline_builder()
        .with_focus(TimelineFocus::Live {
            // Replies in a thread are shown in the thread (see `threads`).
            hide_threaded_events: true,
        })
        .track_read_marker_and_receipts(TimelineReadReceiptTracking::MessageLikeEvents)
        .build()
        .await
        .context("could not open the room's messages")
}

fn receipt_type(public: bool) -> ReceiptType {
    if public {
        ReceiptType::Read
    } else {
        ReceiptType::ReadPrivate
    }
}

/// Marks a room as read up to its newest message: the receipt (public or private),
/// the read marker (`m.fully_read`, which only the user's own clients see: it is
/// where the "new" line of the next visit comes from) and the "marked as unread"
/// flag, which is taken back. Each is only sent if it changes something, and each
/// is tried whatever became of the others.
async fn mark_room_read(
    timeline: &Timeline,
    receipt: ReceiptType,
    flags: &UnreadFlags,
) -> Result<()> {
    // In line behind a mark that is still on its way to the server.
    flags.set(timeline.room().clone(), false);
    // A timeline opened just for this (see `MarkRoomsRead`) may hold nothing yet:
    // the room list keeps a single event per room, and that one may not be a
    // message. Without a newest event the SDK sends no receipt and says nothing, so
    // fetch a page first; and if there still is none, say so rather than report a
    // room as read that the server goes on counting as unread.
    if timeline.latest_event_id().await.is_none() {
        if let Err(err) = timeline.paginate_backwards(PAGE).await {
            tracing::debug!(why = %logsafe::kind(&err), "could not load messages to mark as read");
        }
        if timeline.latest_event_id().await.is_none() {
            // If no visible timeline item exists (e.g. a server that omits ignored events
            // from timeline pagination), fall back to the room's latest event or raw history
            // so marking as read still clears server unread counts.
            let room = timeline.room();
            if let Some(event_id) = fallback_latest_event_id(room).await {
                use matrix_sdk::ruma::events::receipt::ReceiptThread;
                let r = room
                    .send_single_receipt(receipt, ReceiptThread::Unthreaded, event_id.clone())
                    .await;
                let m = room
                    .send_single_receipt(
                        ReceiptType::FullyRead,
                        ReceiptThread::Unthreaded,
                        event_id,
                    )
                    .await;
                r?;
                m?;
                return Ok(());
            }
            bail!("no message to mark as read");
        }
    }
    let receipt = timeline.mark_as_read(receipt).await;
    let marker = timeline.mark_as_read(ReceiptType::FullyRead).await;
    receipt?;
    marker?;
    Ok(())
}

/// Finds the newest event id in `room` when its timeline holds no visible message items.
async fn fallback_latest_event_id(room: &Room) -> Option<matrix_sdk::ruma::OwnedEventId> {
    if let Some(id) = std::ops::Deref::deref(room).latest_event().event_id() {
        return Some(id);
    }
    let options = matrix_sdk::room::MessagesOptions::backward();
    match room.messages(options).await {
        Ok(page) => page
            .chunk
            .iter()
            .find_map(|event| event.event_id().map(ToOwned::to_owned)),
        Err(err) => {
            tracing::debug!(why = %logsafe::kind(&err), "could not query room messages to mark as read");
            None
        }
    }
}

/// Sets and takes back "marked as unread" (`m.marked_unread` in a room's account
/// data). One request after the other, in the order they were asked for: marking a
/// room and opening it right away must not reach the server the other way round.
///
/// What the SDK knows of the flag is what the last sync said, which lags behind
/// what was sent from here a moment ago. So what was last sent is remembered for a
/// while as a second witness, and a request is only skipped when nothing speaks
/// against the room being that way already.
#[derive(Clone)]
struct UnreadFlags(UnboundedSender<(Room, bool)>);

impl UnreadFlags {
    /// For this long what we sent is remembered next to what the last sync said.
    const TRUST_SENT: std::time::Duration = std::time::Duration::from_secs(30);

    /// Ends when the last copy of the handle is dropped.
    fn start(events: UnboundedSender<MatrixEvent>) -> Self {
        use matrix_sdk::ruma::events::marked_unread::MarkedUnreadEventContent;
        let (queue, mut wanted) = tokio::sync::mpsc::unbounded_channel::<(Room, bool)>();
        tokio::spawn(async move {
            let mut sent: std::collections::HashMap<
                matrix_sdk::ruma::OwnedRoomId,
                (bool, std::time::Instant),
            > = Default::default();
            while let Some((room, unread)) = wanted.recv().await {
                sent.retain(|_, (_, at)| at.elapsed() < Self::TRUST_SENT);
                let synced = room.is_marked_unread();
                // Skipping is only to spare the server a request that changes
                // nothing, so it needs both witnesses to agree: the last sync, and
                // what was last sent from here if the sync may not have caught up
                // with it. When they differ (our own request still on its way back,
                // or another client changed the flag meanwhile) the request goes
                // out: once too often does no harm, once too rarely leaves the room
                // marked the wrong way.
                let ours = sent.get(room.room_id()).map(|(ours, _)| *ours);
                if !Self::needs_request(synced, ours, unread) {
                    continue;
                }
                match room
                    .set_account_data(MarkedUnreadEventContent::new(unread))
                    .await
                {
                    Ok(_) => {
                        let at = std::time::Instant::now();
                        sent.insert(room.room_id().to_owned(), (unread, at));
                    }
                    Err(err) => {
                        tracing::warn!(why = %logsafe::kind(&err), "could not change the unread mark");
                        // Only the mark itself is worth a word: taking one back is
                        // a side effect of reading, which went fine.
                        if unread {
                            let _ = events.send(MatrixEvent::ActionFailed(format!(
                                "Could not mark the room as unread: {err}"
                            )));
                        }
                    }
                }
            }
        });
        Self(queue)
    }

    fn set(&self, room: Room, unread: bool) {
        let _ = self.0.send((room, unread));
    }

    /// Whether asking for `wanted` changes anything, given what the last sync said
    /// and what was last sent from here (if that is recent).
    fn needs_request(synced: bool, ours: Option<bool>, wanted: bool) -> bool {
        synced != wanted || ours.is_some_and(|ours| ours != wanted)
    }
}

fn friendly_jump_error(err: &matrix_sdk_ui::timeline::Error) -> String {
    friendly_jump_message(&format!("{err:#}"))
}

fn friendly_jump_message(msg: &str) -> String {
    if msg.contains("404") || msg.contains("M_NOT_FOUND") {
        "Message could not be found.".to_owned()
    } else if msg.contains("SENDER_IGNORED") || msg.contains("You have ignored this sender") {
        "That message is from an ignored user.".to_owned()
    } else if msg.contains("403") || msg.contains("M_FORBIDDEN") {
        "You do not have permission to view that message.".to_owned()
    } else {
        format!("Could not open the message: {msg}")
    }
}

fn countable_live_event(
    item: &TimelineItem,
    ignored: &IgnoredSet,
) -> Option<(OwnedEventId, OwnedUserId)> {
    let event = item.as_event()?;
    if event.is_own() || ignored.contains(event.sender()) {
        return None;
    }
    let TimelineItemContent::MsgLike(content) = event.content() else {
        return None;
    };
    if matches!(content.kind, MsgLikeKind::Redacted) {
        return None;
    }
    Some((event.event_id()?.to_owned(), event.sender().to_owned()))
}

/// Counts new remote messages appended to the tail of a live timeline while
/// the user is looking at a detached `/context` slice.
struct LiveTailCounter {
    items: Vector<Arc<TimelineItem>>,
    seen: HashSet<OwnedEventId>,
    counted: HashMap<OwnedEventId, OwnedUserId>,
}

impl LiveTailCounter {
    fn new(items: Vector<Arc<TimelineItem>>) -> Self {
        let seen = items
            .iter()
            .filter_map(|item| item.as_event()?.event_id().map(ToOwned::to_owned))
            .collect();
        Self {
            items,
            seen,
            counted: HashMap::new(),
        }
    }

    fn record_seen_item(seen: &mut HashSet<OwnedEventId>, item: &TimelineItem) {
        if let Some(id) = item.as_event().and_then(|event| event.event_id()) {
            seen.insert(id.to_owned());
        }
    }

    fn record_tail(&mut self, item: &TimelineItem, ignored: &IgnoredSet) {
        if let Some((id, sender)) = countable_live_event(item, ignored) {
            if self.seen.insert(id.clone()) {
                self.counted.insert(id, sender);
            }
        } else {
            Self::record_seen_item(&mut self.seen, item);
        }
    }

    fn apply_diffs(
        &mut self,
        diffs: Vec<VectorDiff<Arc<TimelineItem>>>,
        ignored: &IgnoredSet,
    ) -> Option<u32> {
        let prev = self.counted.len();
        let saw_reset = diffs
            .iter()
            .any(|diff| matches!(diff, VectorDiff::Clear | VectorDiff::Reset { .. }));
        if saw_reset {
            for diff in diffs {
                diff.apply(&mut self.items);
            }
            for item in &self.items {
                Self::record_seen_item(&mut self.seen, item);
            }
        } else {
            for diff in diffs {
                match &diff {
                    VectorDiff::PushBack { value } => self.record_tail(value, ignored),
                    VectorDiff::Append { values } => {
                        for value in values {
                            self.record_tail(value, ignored);
                        }
                    }
                    VectorDiff::Insert { index, value } => {
                        if !self
                            .items
                            .iter()
                            .skip(*index)
                            .any(|item| item.as_event().is_some())
                        {
                            self.record_tail(value, ignored);
                        } else {
                            Self::record_seen_item(&mut self.seen, value);
                        }
                    }
                    VectorDiff::PushFront { value } => {
                        Self::record_seen_item(&mut self.seen, value);
                    }
                    VectorDiff::Set { value, .. } => {
                        if let Some(id) = value.as_event().and_then(|event| event.event_id()) {
                            if self.counted.contains_key(id)
                                && countable_live_event(value, ignored).is_none()
                            {
                                self.counted.remove(id);
                            }
                            self.seen.insert(id.to_owned());
                        }
                    }
                    VectorDiff::PopFront
                    | VectorDiff::PopBack
                    | VectorDiff::Remove { .. }
                    | VectorDiff::Truncate { .. }
                    | VectorDiff::Clear
                    | VectorDiff::Reset { .. } => {}
                }
                diff.apply(&mut self.items);
            }
        }
        (self.counted.len() != prev).then_some(self.counted.len() as u32)
    }

    fn recheck_ignored(&mut self, ignored: &IgnoredSet) -> Option<u32> {
        let prev = self.counted.len();
        self.counted.retain(|_, sender| !ignored.contains(sender));
        (self.counted.len() != prev).then_some(self.counted.len() as u32)
    }
}

#[allow(clippy::too_many_arguments)]
async fn prepare_messages(
    room: &Room,
    items: &Vector<Arc<TimelineItem>>,
    own: &UserId,
    ignored: &watch::Receiver<IgnoredSet>,
    own_room_names: &super::people::OwnRoomNames,
    readers: &mut super::receipts::Names,
    dedup: &mut super::convert::EventDedup,
    reuse: &mut super::convert::Reuse,
) -> Vec<crate::model::Message> {
    // The set is taken out of the channel first: holding the borrow over a conversion of
    // the whole room would hold the channel's lock, and a change of the list would wait.
    let ignored = IgnoredSet::clone(&ignored.borrow());
    let mut messages = reuse.convert(items, own, &ignored);
    let own_name = super::people::room_own_display_name(room, own_room_names).await;
    super::convert::apply_own_display_name(&mut messages, own_name.as_deref());
    readers.annotate(room, &mut messages, items, own).await;
    dedup.dedup(&mut messages);
    messages
}

/// Whether an opened thread timeline needs its first backwards page loaded: the SDK's
/// `ThreadEventCache` starts empty (or with only live replies synced since it was made)
/// until a backwards page loads `/relations` and the root event.
fn thread_needs_page(items: &Vector<Arc<TimelineItem>>, root: &str) -> bool {
    thread_page_needed(
        items.iter().filter_map(|item| {
            item.as_event()
                .map(|event| event.event_id().map(|id| id.as_str()))
        }),
        root,
        usize::from(PAGE),
    )
}

fn thread_page_needed<'a>(
    event_ids: impl IntoIterator<Item = Option<&'a str>>,
    root: &str,
    page: usize,
) -> bool {
    let mut events = 0usize;
    for id in event_ids {
        if id == Some(root) {
            return false;
        }
        events += 1;
        if events >= page {
            return false;
        }
    }
    true
}

/// Whether the oldest message of a live timeline (`thread`: of that thread) is loaded.
fn start_loaded(items: &Vector<Arc<TimelineItem>>, thread: Option<&str>) -> bool {
    match thread {
        // A thread's timeline has no start marker. Its first message is the last thing
        // loaded, when there is nothing older left.
        Some(root) => items
            .iter()
            .filter_map(|item| item.as_event()?.event_id())
            .any(|id| id == root),
        None => items.iter().any(|item| item.is_timeline_start()),
    }
}

/// Sends the whole timeline to the UI whenever it changes, and who is typing.
/// `thread`: the timeline is not the room's but that of the thread starting at this event.
#[allow(clippy::too_many_arguments)]
async fn forward(
    room: Room,
    timeline: Arc<Timeline>,
    room_id: String,
    generation: u64,
    events: UnboundedSender<MatrixEvent>,
    thread: Option<String>,
    read_marks: threads::ReadMarks,
    detached: Option<DetachedForward>,
    mut ignored: watch::Receiver<IgnoredSet>,
    mut own_refresh: watch::Receiver<u64>,
    own_room_names: super::people::OwnRoomNames,
) {
    let in_thread = thread.is_some();
    let own = room.own_user_id().to_owned();
    let (mut items, changes) = timeline.subscribe().await;
    pin_mut!(changes);
    let (mut live_counter, mut live_changes) = match detached.as_ref().and_then(|d| d.live.as_ref())
    {
        Some(live) => {
            let (live_items, changes) = live.subscribe().await;
            (
                Some(LiveTailCounter::new(live_items)),
                Some(Box::pin(changes)),
            )
        }
        None => (None, None),
    };
    let (_typing_guard, mut typing) = room.subscribe_to_typing_notifications();
    // Who has read up to where, by name.
    let mut readers = super::receipts::Names::default();
    let mut dedup = super::convert::EventDedup::default();
    // What was made of each item, for the next change.
    let mut reuse = super::convert::Reuse::default();
    let snapshot = |items: &Vector<Arc<TimelineItem>>,
                    unread: HashSet<String>,
                    mut messages: Vec<crate::model::Message>| {
        threads::annotate(&mut messages, in_thread, &unread);
        MatrixEvent::Timeline {
            room_id: room_id.clone(),
            generation,
            messages,
            reached_start: match &detached {
                Some(d) => d.reached_start.load(Ordering::Acquire),
                None => start_loaded(items, thread.as_deref()),
            },
            detached: detached.as_ref().map(|d| DetachedTimeline {
                target_event: d.target_event.clone(),
                thread_root: thread.clone(),
                reached_end: d.reached_end.load(Ordering::Acquire),
            }),
        }
    };
    // Which threads have unread replies; nothing to look up inside a thread.
    let unread = async |items: &Vector<Arc<TimelineItem>>| {
        if in_thread {
            HashSet::new()
        } else {
            threads::unread(&room, items, &read_marks).await
        }
    };
    // Replies to messages that are not loaded only say "an earlier message" until the
    // original is fetched. Ask once per reply.
    let mut asked: HashSet<OwnedEventId> = HashSet::new();
    let mut fetch_replies = |items: &Vector<Arc<TimelineItem>>| {
        for event in items.iter().filter_map(|item| item.as_event()) {
            let unavailable = event
                .content()
                .as_msglike()
                .and_then(|content| content.in_reply_to.as_ref())
                .is_some_and(|reply| matches!(reply.event, TimelineDetails::Unavailable));
            if let (true, Some(event_id)) = (unavailable, event.event_id())
                && asked.insert(event_id.to_owned())
            {
                let timeline = timeline.clone();
                let event_id = event_id.to_owned();
                tokio::spawn(async move {
                    if let Err(err) = timeline.fetch_details_for_event(&event_id).await {
                        tracing::debug!(why = %logsafe::kind(&err), "could not fetch the replied-to message");
                    }
                });
            }
        }
    };
    fetch_replies(&items);
    let marks = unread(&items).await;
    let messages = prepare_messages(
        &room,
        &items,
        &own,
        &ignored,
        &own_room_names,
        &mut readers,
        &mut dedup,
        &mut reuse,
    )
    .await;
    if events.send(snapshot(&items, marks, messages)).is_err() {
        return;
    }
    super::manage::emit_room_power(&events, &room).await;
    let mut last_pinned: Vec<String> = match room.pinned_event_ids() {
        Some(ids) => ids.into_iter().map(|id| id.to_string()).collect(),
        None => room
            .load_pinned_events()
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
            .into_iter()
            .map(|id| id.to_string())
            .collect(),
    };
    if events
        .send(MatrixEvent::PinnedEvents {
            room_id: room_id.clone(),
            ids: last_pinned.clone(),
        })
        .is_err()
    {
        return;
    }
    let pin_stream = room.pinned_event_ids_stream();
    pin_mut!(pin_stream);
    let mut clear_paginators = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            diffs = changes.next() => {
                let Some(diffs) = diffs else { return };
                // A subscriber that fell behind gets the whole list again as a `Reset` in
                // place of the diffs it missed, a clear among them perhaps: taken as one.
                let saw_clear = diffs
                    .iter()
                    .any(|diff| matches!(diff, VectorDiff::Clear | VectorDiff::Reset { .. }));
                let had_start = detached.is_none() && start_loaded(&items, thread.as_deref());
                for diff in diffs {
                    diff.apply(&mut items);
                }
                // The SDK cleared the timeline, or the start that was loaded is gone (a
                // clear with local echoes comes as removals): the interface forgets that
                // the start was reached, which the timeline after this cannot tell it,
                // since one that is not at the start says nothing of what came before.
                // A sync that missed messages comes as a clear and the latest messages
                // in one batch, so the timeline after it is not empty.
                let reset = saw_clear
                    || (had_start && !start_loaded(&items, thread.as_deref()));
                // A slice around an event that the SDK reloads (it does so for every room
                // when the ignore list changes) is back to its first window, with gaps on
                // both sides again: what the pages had reached is forgotten, or the
                // timeline after the reset would say start and end were still reached.
                if reset && let Some(d) = &detached {
                    d.reached_start.store(false, Ordering::Release);
                    d.reached_end.store(false, Ordering::Release);
                }
                if reset
                    && events
                        .send(MatrixEvent::TimelineReset {
                            room_id: room_id.clone(),
                            generation,
                        })
                        .is_err()
                {
                    return;
                }
                while clear_paginators.try_join_next().is_some() {}
                if saw_clear
                    && items.is_empty()
                    && detached.is_none()
                    && room.state() == matrix_sdk::RoomState::Joined
                {
                    // Ignoring or unignoring someone makes the SDK clear the room's event cache;
                    // load a page immediately so the open chat recovers instead of waiting blank.
                    let timeline = timeline.clone();
                    let events = events.clone();
                    clear_paginators.spawn(async move {
                        if let Ok(true) = timeline.paginate_backwards(PAGE).await {
                            let _ = events.send(MatrixEvent::Paginated {
                                generation,
                                reached_start: true,
                            });
                        }
                    });
                }
                // What was made before a clear or a reset is not used after it. It would
                // be right (the same item is the same message), but nothing is assumed
                // of a timeline that started over, and it is rare.
                if saw_clear {
                    reuse.forget();
                }
                fetch_replies(&items);
                let marks = unread(&items).await;
                let messages = prepare_messages(
                    &room,
                    &items,
                    &own,
                    &ignored,
                    &own_room_names,
                    &mut readers,
                    &mut dedup,
                    &mut reuse,
                )
                .await;
                if events.send(snapshot(&items, marks, messages)).is_err() {
                    return;
                }
            }
            changed = ignored.changed() => {
                if changed.is_err() {
                    return;
                }
                let marks = unread(&items).await;
                let messages = prepare_messages(
                    &room,
                    &items,
                    &own,
                    &ignored,
                    &own_room_names,
                    &mut readers,
                    &mut dedup,
                    &mut reuse,
                )
                .await;
                if events.send(snapshot(&items, marks, messages)).is_err() {
                    return;
                }
                let new_count = {
                    let ignored_set = ignored.borrow();
                    live_counter
                        .as_mut()
                        .and_then(|counter| counter.recheck_ignored(&ignored_set))
                };
                if let Some(count) = new_count
                    && events.send(MatrixEvent::DetachedNew { generation, count }).is_err()
                {
                    return;
                }
            }
            changed = own_refresh.changed() => {
                if changed.is_err() {
                    return;
                }
                let marks = unread(&items).await;
                let messages = prepare_messages(
                    &room,
                    &items,
                    &own,
                    &ignored,
                    &own_room_names,
                    &mut readers,
                    &mut dedup,
                    &mut reuse,
                )
                .await;
                if events.send(snapshot(&items, marks, messages)).is_err() {
                    return;
                }
            }
            diffs = async {
                match live_changes.as_mut() {
                    Some(stream) => stream.next().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(diffs) = diffs else {
                    live_changes = None;
                    continue;
                };
                let new_count = {
                    let ignored_set = ignored.borrow();
                    live_counter
                        .as_mut()
                        .and_then(|counter| counter.apply_diffs(diffs, &ignored_set))
                };
                if let Some(count) = new_count
                    && events.send(MatrixEvent::DetachedNew { generation, count }).is_err()
                {
                    return;
                }
            }
            users = typing.recv() => {
                let names = match users {
                    Ok(users) => users
                        .iter()
                        .filter(|u| !is_ignored_in(&ignored, u))
                        .map(|u| crate::text::sanitize_line(u.localpart()))
                        .collect(),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                if events.send(MatrixEvent::Typing { generation, names }).is_err() {
                    return;
                }
            }
            pinned = pin_stream.next() => {
                let Some(ids) = pinned else { continue };
                let next: Vec<String> = ids.into_iter().map(|id| id.to_string()).collect();
                if next != last_pinned {
                    last_pinned = next.clone();
                    if events
                        .send(MatrixEvent::PinnedEvents {
                            room_id: room_id.clone(),
                            ids: next,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
pub(super) mod measure;

#[cfg(test)]
mod reuse;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    /// The picker's choice becomes an `m.sticker` event: the pack's address and what
    /// it says about the file, with the name, made safe, as the event's text.
    #[test]
    fn a_sticker_of_a_pack_becomes_a_sticker_event() {
        use matrix_sdk::ruma::events::sticker::StickerMediaSource;

        let url = "mxc://example.org/crab";
        let content = super::sticker_content(
            "Waving crab",
            url.to_owned(),
            Some((128, 96)),
            Some(4096),
            Some("image/gif".to_owned()),
        )
        .unwrap();
        assert_eq!(content.body, "Waving crab");
        assert!(matches!(&content.source, StickerMediaSource::Plain(sent) if sent.as_str() == url));
        let sent = serde_json::to_value(&content).unwrap();
        assert_eq!(sent["body"], "Waving crab");
        assert_eq!(sent["url"], url);
        assert_eq!(sent["info"]["w"], 128);
        assert_eq!(sent["info"]["h"], 96);
        assert_eq!(sent["info"]["size"], 4096);
        assert_eq!(sent["info"]["mimetype"], "image/gif");

        // A pack that says nothing about the file, or a size no event can hold: the
        // sticker goes without them.
        let bare =
            super::sticker_content("crab", url.to_owned(), None, Some(u64::MAX), None).unwrap();
        assert!(bare.info.width.is_none() && bare.info.height.is_none());
        assert!(bare.info.size.is_none() && bare.info.mimetype.is_none());

        // The name is the pack's, which someone else may have written.
        let hostile = super::sticker_content(
            "crab\u{1b}[2J\r\nsecond\u{202e}",
            url.to_owned(),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(hostile.body, "crab[2J second");

        // What is no `mxc://` address is not sent.
        for bad in ["", "mxc://", "https://example.org/crab", "example.org/crab"] {
            assert!(
                super::sticker_content("crab", bad.to_owned(), None, None, None).is_err(),
                "{bad}"
            );
        }
    }

    /// The session's timelines, for a client of the stand-in server.
    fn timelines_of(
        client: matrix_sdk::Client,
        left: &crate::matrix::left::LeftRooms,
    ) -> (
        super::Timelines,
        tokio::sync::mpsc::UnboundedReceiver<crate::matrix::MatrixEvent>,
    ) {
        let (events, received) = tokio::sync::mpsc::unbounded_channel();
        let (_, ignored) = tokio::sync::watch::channel(super::IgnoredSet::default());
        let (_, own_refresh) = tokio::sync::watch::channel(0);
        let timelines = super::Timelines::new(
            client,
            events,
            ignored,
            own_refresh,
            crate::matrix::people::OwnRoomNames::default(),
            left.clone(),
        );
        (timelines, received)
    }

    #[tokio::test]
    async fn a_room_left_here_is_not_opened_whatever_it_looks_like() {
        use crate::matrix::left::{LeftRooms, testing::Server};
        use crate::matrix::{MatrixCmd, MatrixEvent};
        use matrix_sdk::ruma::room_id;

        let room = room_id!("!gone:localhost");
        let client = Server {
            joined: vec![room.as_str()],
            ..Server::default()
        }
        .client()
        .await;
        let left = LeftRooms::default();
        left.mark(room);
        let (mut timelines, mut events) = timelines_of(client, &left);
        // Jumping to a message in it (a link, a search hit) fails at once.
        timelines
            .handle(MatrixCmd::OpenEvent {
                room_id: room.to_string(),
                event_id: "$message:localhost".to_owned(),
                thread_root: None,
                generation: 7,
            })
            .await;
        match events.try_recv() {
            Ok(MatrixEvent::JumpFailed { generation, reason }) => {
                assert_eq!(generation, 7);
                assert!(reason.contains("not in this room"), "{reason}");
            }
            other => panic!("expected the jump to fail, got {other:?}"),
        }
        // Opening it fails too, as does anything else done in it.
        timelines
            .handle(MatrixCmd::OpenRoom {
                room_id: room.to_string(),
                generation: 8,
            })
            .await;
        match events.try_recv() {
            Ok(MatrixEvent::ActionFailed(text)) => assert!(text.contains("not in this room")),
            other => panic!("expected opening to fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn closing_the_room_stops_its_timeline() {
        use crate::matrix::MatrixCmd;
        use crate::matrix::left::{LeftRooms, testing::Server};
        use matrix_sdk::ruma::room_id;

        let room = room_id!("!open:localhost");
        let client = Server {
            joined: vec![room.as_str()],
            ..Server::default()
        }
        .client()
        .await;
        let left = LeftRooms::default();
        let (mut timelines, _events) = timelines_of(client, &left);
        timelines
            .handle(MatrixCmd::OpenRoom {
                room_id: room.to_string(),
                generation: 1,
            })
            .await;
        let forwarder = timelines.forwarder.as_ref().map(|task| task.abort_handle());
        let forwarder = forwarder.expect("the open room's timeline is forwarded");
        // The UI shows no room any more.
        timelines.handle(MatrixCmd::CloseRoom).await;
        assert!(timelines.forwarder.is_none());
        assert!(
            timelines.paginator.is_none()
                && timelines.active.is_none()
                && timelines.before_jump.is_none()
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while !forwarder.is_finished() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the forwarder ends");
        // The room's timeline stays cached for the next time it is opened.
        assert_eq!(timelines.cache.len(), 1);
        // Nothing to hide, nothing shown: the command names no room.
        assert_eq!(format!("{:?}", MatrixCmd::CloseRoom), "CloseRoom");
        assert_eq!(MatrixCmd::CloseRoom.describe(), "CloseRoom");
    }

    #[tokio::test]
    async fn a_declined_invitation_is_remembered_as_left() {
        use crate::matrix::MatrixCmd;
        use crate::matrix::left::{LeftRooms, testing::Server};
        use matrix_sdk::ruma::room_id;

        let room = room_id!("!declined:localhost");
        let client = Server {
            invited: vec![room.as_str()],
            ..Server::default()
        }
        .client()
        .await;
        let left = LeftRooms::default();
        let (mut timelines, _events) = timelines_of(client, &left);
        timelines
            .handle(MatrixCmd::Leave {
                room_id: room.to_string(),
            })
            .await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while !left.is_left(room) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the declined invitation is remembered");
        // (The SDK forgets it. Brought back looking joined by a late answer, it now stays
        // hidden, while a new invitation shows: see the tests of `left`.)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn accepting_an_invitation_to_a_room_left_before_brings_it_back_only_when_it_works() {
        use crate::matrix::left::LeftRooms;
        use crate::matrix::spaces::fake;
        use crate::matrix::{MatrixCmd, MatrixEvent};
        use matrix_sdk::ruma::room_id;

        let (asked, refused, gone) = (
            room_id!("!asked:localhost"),
            room_id!("!refused:localhost"),
            room_id!("!gone:localhost"),
        );
        // Only the one invitation can be accepted: any other join gets no room back.
        let server = fake::Server::answering(
            serde_json::json!({
                "join": { "!gone:localhost": fake::joined(false, vec![]) },
                "invite": {
                    "!asked:localhost": fake::invited(false),
                    "!refused:localhost": fake::invited(false),
                },
            }),
            Box::new(|request| {
                (request.path == "/_matrix/client/v3/rooms/!asked:localhost/join")
                    .then(|| fake::Reply::Json(r#"{"room_id":"!asked:localhost"}"#.to_owned()))
            }),
        )
        .await;
        let client = fake::client(&server).await;
        let left = LeftRooms::default();
        for room in [asked, refused, gone] {
            left.mark(room);
        }
        let (mut timelines, mut events) = timelines_of(client, &left);
        let mut failure = async || {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match events.recv().await {
                        Some(MatrixEvent::ActionFailed(text)) => return text,
                        Some(_) => {}
                        None => panic!("the worker stopped"),
                    }
                }
            })
            .await
            .expect("expected the join to fail")
        };
        // Accepting fails: still a room that was left.
        timelines
            .handle(MatrixCmd::Join {
                room_id: refused.to_string(),
            })
            .await;
        assert!(failure().await.starts_with("Could not join"));
        assert!(left.is_left(refused));
        // A room left here that looks joined has no invitation to accept: it stays gone.
        timelines
            .handle(MatrixCmd::Join {
                room_id: gone.to_string(),
            })
            .await;
        assert!(failure().await.contains("not in this room"));
        assert!(left.is_left(gone));
        // Accepted: a room like any other.
        timelines
            .handle(MatrixCmd::Join {
                room_id: asked.to_string(),
            })
            .await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while left.is_left(asked) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the room is back once joined");
    }

    #[test]
    fn thread_page_needed_decides_when_an_opened_thread_needs_a_first_page() {
        // Empty or only unsent local echoes: still needs the first page (and root).
        assert!(super::thread_page_needed([], "$root", 40));
        assert!(super::thread_page_needed([None, None], "$root", 40));
        // Fewer than a page of replies, without the root: still needs the root loaded.
        assert!(super::thread_page_needed(
            [Some("$r1"), Some("$r2")],
            "$root",
            40
        ));
        // The root is already in the thread timeline: nothing more to ask for.
        assert!(!super::thread_page_needed([Some("$root")], "$root", 40));
        assert!(!super::thread_page_needed(
            [Some("$root"), Some("$r1")],
            "$root",
            40
        ));
        // A full page of cached replies already loaded: wait for the user to scroll.
        let full = std::iter::repeat_n(Some("$reply"), 40);
        assert!(!super::thread_page_needed(full, "$root", 40));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn opening_a_thread_without_replies_loads_its_root_even_after_sync_delivered_it() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::matrix::left::LeftRooms;
        use crate::matrix::spaces::fake;
        use crate::matrix::{MatrixCmd, MatrixEvent};
        use crate::model::ItemRef;
        use matrix_sdk::authentication::SessionTokens;
        use matrix_sdk::authentication::matrix::MatrixSession;
        use matrix_sdk::{AuthSession, Client, SessionMeta};
        use serde_json::json;

        let room = "!thread:localhost";
        let root = "$root:localhost";
        let root_event = json!({
            "type": "m.room.message",
            "room_id": room,
            "event_id": root,
            "sender": fake::ME,
            "origin_server_ts": 1_000,
            "content": { "msgtype": "m.text", "body": "start of the thread" }
        });
        let mut joined = fake::joined(false, vec![]);
        joined["timeline"]["events"] = json!([root_event.clone()]);

        let relations_hits = Arc::new(AtomicUsize::new(0));
        let hits = relations_hits.clone();
        let event_json = root_event.to_string();
        let server = fake::Server::answering(
            json!({ "join": { room: joined } }),
            Box::new(move |request| {
                if request.path.contains("/relations/") {
                    hits.fetch_add(1, Ordering::SeqCst);
                    Some(fake::Reply::Json(r#"{"chunk":[]}"#.to_owned()))
                } else if request.path.contains("/event/") {
                    Some(fake::Reply::Json(event_json.clone()))
                } else {
                    None
                }
            }),
        )
        .await;

        let client = Client::builder()
            .homeserver_url(&server.url)
            .with_threading_support(super::threads::SUPPORT)
            .build()
            .await
            .unwrap();
        client
            .restore_session(AuthSession::Matrix(MatrixSession {
                meta: SessionMeta {
                    user_id: fake::ME.try_into().unwrap(),
                    device_id: "FAKE".into(),
                },
                tokens: SessionTokens {
                    access_token: "token".into(),
                    refresh_token: None,
                },
            }))
            .await
            .unwrap();
        // Subscribing before sync ensures `$root` is delivered to the room's event cache
        // before `OpenThread` ever creates a `ThreadEventCache` for it — the race where
        // the SDK's thread cache otherwise starts and stays empty until paginated.
        client.event_cache().subscribe().unwrap();
        client.sync_once(SyncSettings::default()).await.unwrap();

        let left = LeftRooms::default();
        let (event_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        // Keep the watch senders alive so `forward` stays in its select loop.
        let (_ignored_tx, ignored) = tokio::sync::watch::channel(super::IgnoredSet::default());
        let (_own_refresh_tx, own_refresh) = tokio::sync::watch::channel(0);
        let mut timelines = super::Timelines::new(
            client,
            event_tx,
            ignored,
            own_refresh,
            crate::matrix::people::OwnRoomNames::default(),
            left,
        );

        timelines
            .handle(MatrixCmd::OpenThread {
                room_id: room.to_owned(),
                root: root.to_owned(),
                generation: 1,
            })
            .await;
        assert!(
            timelines.paginator.is_some(),
            "opening an empty thread starts its first page"
        );
        // A follow-up `Paginate` from the UI coalesces into the running initial page
        // rather than issuing a second `/relations` request.
        timelines
            .handle(MatrixCmd::InThread {
                root: root.to_owned(),
                command: Box::new(MatrixCmd::Paginate {
                    room_id: room.to_owned(),
                    generation: 1,
                }),
            })
            .await;

        let mut saw_root = false;
        let mut saw_paginated = false;
        let mut seen_events = Vec::new();
        let waited = tokio::time::timeout(Duration::from_secs(15), async {
            while !(saw_root && saw_paginated) {
                let event = events.recv().await.expect("worker channel open");
                match &event {
                    MatrixEvent::Timeline {
                        generation: 1,
                        messages,
                        reached_start,
                        ..
                    } => {
                        let summary: Vec<_> = messages
                            .iter()
                            .map(|m| (m.body.clone(), m.item.clone(), m.state))
                            .collect();
                        seen_events.push(format!(
                            "Timeline(gen=1, reached_start={reached_start}, msgs={summary:?})"
                        ));
                        if messages.len() == 1
                            && messages[0].body == "start of the thread"
                            && messages[0].item == Some(ItemRef::Event(root.to_owned()))
                            && *reached_start
                        {
                            saw_root = true;
                        }
                    }
                    MatrixEvent::Paginated {
                        generation: 1,
                        reached_start,
                    } => {
                        seen_events
                            .push(format!("Paginated(gen=1, reached_start={reached_start})"));
                        if *reached_start {
                            saw_paginated = true;
                        }
                    }
                    other => seen_events.push(format!("{other:?}")),
                }
            }
        })
        .await;
        if waited.is_err() {
            let paths: Vec<_> = server
                .requests()
                .into_iter()
                .map(|r| format!("{} {}?{}", r.method, r.path, r.query))
                .collect();
            panic!(
                "opening an empty thread timed out: saw_root={saw_root} saw_paginated={saw_paginated} \
                 relations_hits={} events={seen_events:?} requests={paths:?}",
                relations_hits.load(Ordering::SeqCst)
            );
        }
        assert_eq!(relations_hits.load(Ordering::SeqCst), 1);

        // Reopening the cached thread that already holds its root does not paginate again.
        timelines.handle(MatrixCmd::CloseRoom).await;
        timelines
            .handle(MatrixCmd::OpenThread {
                room_id: room.to_owned(),
                root: root.to_owned(),
                generation: 2,
            })
            .await;
        assert!(timelines.paginator.is_none());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await.expect("worker channel open") {
                    MatrixEvent::Timeline {
                        generation: 2,
                        messages,
                        reached_start: true,
                        ..
                    } if messages.len() == 1 && messages[0].body == "start of the thread" => {
                        return;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("reopening the cached thread shows the root immediately");
        assert_eq!(relations_hits.load(Ordering::SeqCst), 1);
    }

    /// The thread shown is never dropped from the cache for another thread. A command for
    /// a thread other than the one shown comes when the UI has not yet heard that a jump
    /// to a message took it to another thread: it marks read, or asks for older messages,
    /// in the thread it still shows. The thread jumped to keeps its timeline (the one its
    /// forwarder watches); a thread not shown still makes room for the next one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_command_for_another_thread_keeps_the_shown_thread() {
        use std::sync::Arc;

        use crate::matrix::left::LeftRooms;
        use crate::matrix::spaces::fake;
        use crate::matrix::{MatrixCmd, MatrixEvent};
        use matrix_sdk::authentication::SessionTokens;
        use matrix_sdk::authentication::matrix::MatrixSession;
        use matrix_sdk::{AuthSession, Client, SessionMeta};
        use serde_json::json;

        let room = "!threads:localhost";
        let roots = [
            "$root-a:localhost",
            "$root-b:localhost",
            "$root-c:localhost",
        ];
        let [a, b, c] = roots;
        let event = |root: &str| {
            json!({
                "type": "m.room.message",
                "room_id": room,
                "event_id": root,
                "sender": fake::ME,
                "origin_server_ts": 1_000,
                "content": { "msgtype": "m.text", "body": "a thread starts here" }
            })
        };
        let mut joined = fake::joined(false, vec![]);
        joined["timeline"]["events"] = json!(roots.map(event));
        let events_json = roots.map(|root| (root.to_owned(), event(root).to_string()));
        let server = fake::Server::answering(
            json!({ "join": { room: joined } }),
            Box::new(move |request| {
                if request.path.contains("/relations/") {
                    return Some(fake::Reply::Json(r#"{"chunk":[]}"#.to_owned()));
                }
                if request.path.contains("/event/") {
                    let (_, json) = events_json.iter().find(|(root, _)| {
                        let name = root.trim_start_matches('$').split(':').next();
                        name.is_some_and(|name| request.path.contains(name))
                    })?;
                    return Some(fake::Reply::Json(json.clone()));
                }
                None
            }),
        )
        .await;
        let client = Client::builder()
            .homeserver_url(&server.url)
            .with_threading_support(super::threads::SUPPORT)
            .build()
            .await
            .unwrap();
        client
            .restore_session(AuthSession::Matrix(MatrixSession {
                meta: SessionMeta {
                    user_id: fake::ME.try_into().unwrap(),
                    device_id: "FAKE".into(),
                },
                tokens: SessionTokens {
                    access_token: "token".into(),
                    refresh_token: None,
                },
            }))
            .await
            .unwrap();
        client.event_cache().subscribe().unwrap();
        client.sync_once(SyncSettings::default()).await.unwrap();

        let (event_tx, _events) = tokio::sync::mpsc::unbounded_channel::<MatrixEvent>();
        let (_ignored_tx, ignored) = tokio::sync::watch::channel(super::IgnoredSet::default());
        let (_own_refresh_tx, own_refresh) = tokio::sync::watch::channel(0);
        let mut timelines = super::Timelines::new(
            client,
            event_tx,
            ignored,
            own_refresh,
            crate::matrix::people::OwnRoomNames::default(),
            LeftRooms::default(),
        );
        let in_thread = |root: &str| MatrixCmd::InThread {
            root: root.to_owned(),
            command: Box::new(MatrixCmd::MarkRead {
                room_id: room.to_owned(),
                public: false,
            }),
        };
        let threads = |timelines: &super::Timelines| -> Vec<String> {
            let keys = timelines.cache.iter().map(|cached| cached.key.clone());
            keys.filter(|key| super::threads::is_thread(key)).collect()
        };
        let timeline_of = |timelines: &super::Timelines, root: &str| {
            let key = super::threads::key(room, Some(root));
            let cached = timelines.cache.iter().find(|cached| cached.key == key);
            cached.map(|cached| cached.timeline.clone())
        };
        let key = |root: &str| super::threads::key(room, Some(root));

        // Thread B is what is shown now (as after the jump); the UI still shows A.
        timelines
            .handle(MatrixCmd::OpenThread {
                room_id: room.to_owned(),
                root: b.to_owned(),
                generation: 1,
            })
            .await;
        let shown = timeline_of(&timelines, b).expect("the thread shown is cached");
        timelines.handle(in_thread(a)).await;
        assert_eq!(threads(&timelines), [key(b), key(a)]);
        // The UI hears of the jump: B's commands go to the timeline B was shown with.
        timelines.handle(in_thread(b)).await;
        let now = timeline_of(&timelines, b).expect("the thread shown is still cached");
        assert!(
            Arc::ptr_eq(&shown, &now),
            "the thread shown was opened anew"
        );

        // One more thread that is not shown takes the place of the one before it.
        timelines.handle(in_thread(c)).await;
        assert_eq!(threads(&timelines), [key(b), key(c)]);
        let now = timeline_of(&timelines, b).expect("the thread shown is still cached");
        assert!(Arc::ptr_eq(&shown, &now));
        drop(server);
    }

    /// A jump that finishes on the fast path switches the worker before the UI's
    /// `CancelOpenEvent` arrives. Once the cancel arrives, the worker must restore the
    /// room the UI still shows (under its original generation and back in `shown`), so
    /// subsequent synced messages keep reaching it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_late_cancel_after_a_fast_path_jump_restores_the_open_room() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::matrix::left::LeftRooms;
        use crate::matrix::spaces::fake;
        use crate::matrix::{MatrixCmd, MatrixEvent};
        use matrix_sdk::authentication::SessionTokens;
        use matrix_sdk::authentication::matrix::MatrixSession;
        use matrix_sdk::{AuthSession, Client, SessionMeta};
        use serde_json::json;

        let room_a = "!a:localhost";
        let room_b = "!b:localhost";
        let msg = |room: &str, id: &str, ts: u64, body: &str| {
            json!({
                "type": "m.room.message",
                "room_id": room,
                "event_id": id,
                "sender": fake::ME,
                "origin_server_ts": ts,
                "content": { "msgtype": "m.text", "body": body }
            })
        };
        let mut joined_a = fake::joined(false, vec![]);
        joined_a["timeline"]["events"] = json!([msg(room_a, "$a1:localhost", 1_000, "in a")]);
        let mut joined_b = fake::joined(false, vec![]);
        joined_b["timeline"]["events"] = json!([msg(room_b, "$b1:localhost", 1_000, "in b")]);
        let later_a = msg(room_a, "$a2:localhost", 2_000, "later in a");
        let syncs = AtomicUsize::new(0);
        let server = fake::Server::answering(
            json!({ "join": { room_a: joined_a, room_b: joined_b } }),
            Box::new(move |request| {
                if !request.path.ends_with("/sync") || syncs.fetch_add(1, Ordering::SeqCst) == 0 {
                    return None;
                }
                let sync = json!({
                    "next_batch": "s2",
                    "rooms": {
                        "join": {
                            room_a: {
                                "timeline": { "events": [later_a], "limited": false }
                            }
                        }
                    }
                });
                Some(fake::Reply::Json(sync.to_string()))
            }),
        )
        .await;
        let client = Client::builder()
            .homeserver_url(&server.url)
            .with_threading_support(super::threads::SUPPORT)
            .build()
            .await
            .unwrap();
        client
            .restore_session(AuthSession::Matrix(MatrixSession {
                meta: SessionMeta {
                    user_id: fake::ME.try_into().unwrap(),
                    device_id: "FAKE".into(),
                },
                tokens: SessionTokens {
                    access_token: "token".into(),
                    refresh_token: None,
                },
            }))
            .await
            .unwrap();
        client.event_cache().subscribe().unwrap();
        client.sync_once(SyncSettings::default()).await.unwrap();

        let (mut timelines, mut events, _watches) =
            live_timelines_of(client.clone(), &LeftRooms::default());
        let wait_body = async |events: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixEvent>,
                               want_room: &str,
                               want_gen: u64,
                               needle: &str| {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match events.recv().await {
                        Some(MatrixEvent::Timeline {
                            room_id,
                            generation,
                            messages,
                            ..
                        }) if room_id == want_room
                            && generation == want_gen
                            && messages.iter().any(|m| m.body == needle) =>
                        {
                            return;
                        }
                        Some(_) => {}
                        None => panic!("the worker stopped"),
                    }
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!("timed out waiting for {needle:?} in {want_room} gen {want_gen}")
            });
        };

        // Warm room B in cache, then open room A (gen 2).
        timelines
            .handle(MatrixCmd::OpenRoom {
                room_id: room_b.to_owned(),
                generation: 1,
            })
            .await;
        wait_body(&mut events, room_b, 1, "in b").await;
        timelines
            .handle(MatrixCmd::OpenRoom {
                room_id: room_a.to_owned(),
                generation: 2,
            })
            .await;
        wait_body(&mut events, room_a, 2, "in a").await;

        // Jump to $b1 in warm room B: the fast path switches the worker to gen 3 at once.
        timelines
            .handle(MatrixCmd::OpenEvent {
                room_id: room_b.to_owned(),
                event_id: "$b1:localhost".to_owned(),
                thread_root: None,
                generation: 3,
            })
            .await;
        wait_body(&mut events, room_b, 3, "in b").await;
        assert_eq!(timelines.shown, [room_b]);

        // Cancel arrives late, after the worker already switched to room B.
        timelines
            .handle(MatrixCmd::CancelOpenEvent { generation: 3 })
            .await;
        assert_eq!(
            timelines.shown,
            [room_a],
            "late cancel must put the old room back in shown"
        );

        // Expect TimelineReset then Timeline for room A under gen 2.
        let mut saw_reset = false;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Some(MatrixEvent::TimelineReset {
                        room_id,
                        generation,
                    }) if room_id == room_a && generation == 2 => {
                        saw_reset = true;
                    }
                    Some(MatrixEvent::Timeline {
                        room_id,
                        generation,
                        messages,
                        ..
                    }) if room_id == room_a
                        && generation == 2
                        && messages.iter().any(|m| m.body == "in a") =>
                    {
                        assert!(saw_reset, "restored timeline must follow TimelineReset");
                        return;
                    }
                    Some(_) => {}
                    None => panic!("the worker stopped"),
                }
            }
        })
        .await
        .expect("late cancel must restore room A under gen 2");

        // Live sync updates in room A must keep arriving under gen 2.
        client.sync_once(SyncSettings::default()).await.unwrap();
        wait_body(&mut events, room_a, 2, "later in a").await;

        // `drop_idle` drops unshown room B while keeping restored room A.
        timelines
            .drop_idle(std::time::Instant::now() + super::IDLE_TIMELINE + Duration::from_secs(1));
        let cached: Vec<&str> = timelines.cache.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(cached, [room_a]);
        drop(server);
    }

    /// Like [`timelines_of`], keeping the `ignored` and `own_refresh` watch senders
    /// alive so `forward` stays in its select loop across subsequent syncs and pages.
    fn live_timelines_of(
        client: matrix_sdk::Client,
        left: &crate::matrix::left::LeftRooms,
    ) -> (
        super::Timelines,
        tokio::sync::mpsc::UnboundedReceiver<crate::matrix::MatrixEvent>,
        (
            tokio::sync::watch::Sender<super::IgnoredSet>,
            tokio::sync::watch::Sender<u64>,
        ),
    ) {
        let (events, received) = tokio::sync::mpsc::unbounded_channel();
        let (ignored_tx, ignored) = tokio::sync::watch::channel(super::IgnoredSet::default());
        let (own_refresh_tx, own_refresh) = tokio::sync::watch::channel(0);
        let timelines = super::Timelines::new(
            client,
            events,
            ignored,
            own_refresh,
            crate::matrix::people::OwnRoomNames::default(),
            left.clone(),
        );
        (timelines, received, (ignored_tx, own_refresh_tx))
    }

    async fn fake_client_with_cache(url: &str) -> matrix_sdk::Client {
        use crate::matrix::spaces::fake;
        use matrix_sdk::authentication::SessionTokens;
        use matrix_sdk::authentication::matrix::MatrixSession;
        use matrix_sdk::{AuthSession, Client, SessionMeta};

        let client = Client::builder()
            .homeserver_url(url)
            .with_threading_support(super::threads::SUPPORT)
            .build()
            .await
            .unwrap();
        client
            .restore_session(AuthSession::Matrix(MatrixSession {
                meta: SessionMeta {
                    user_id: fake::ME.try_into().unwrap(),
                    device_id: "FAKE".into(),
                },
                tokens: SessionTokens {
                    access_token: "token".into(),
                    refresh_token: None,
                },
            }))
            .await
            .unwrap();
        client.event_cache().subscribe().unwrap();
        client.sync_once(SyncSettings::default()).await.unwrap();
        client
    }

    async fn wait_for_timeline(
        events: &mut tokio::sync::mpsc::UnboundedReceiver<crate::matrix::MatrixEvent>,
        want_room: &str,
        want_gen: u64,
        needle: &str,
    ) -> (bool, Option<crate::matrix::DetachedTimeline>) {
        use crate::matrix::MatrixEvent;

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match events.recv().await {
                    Some(MatrixEvent::Timeline {
                        room_id,
                        generation,
                        messages,
                        reached_start,
                        detached,
                    }) if room_id == want_room
                        && generation == want_gen
                        && messages.iter().any(|m| m.body == needle) =>
                    {
                        return (reached_start, detached);
                    }
                    Some(_) => {}
                    None => panic!("the worker stopped"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!("timed out waiting for {needle:?} in {want_room} gen {want_gen}")
        })
    }

    async fn wait_for_restored_timeline(
        events: &mut tokio::sync::mpsc::UnboundedReceiver<crate::matrix::MatrixEvent>,
        want_room: &str,
        want_gen: u64,
        needle: &str,
    ) -> (bool, Option<crate::matrix::DetachedTimeline>) {
        use crate::matrix::MatrixEvent;

        let mut saw_reset = false;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match events.recv().await {
                    Some(MatrixEvent::TimelineReset {
                        room_id,
                        generation,
                    }) if room_id == want_room && generation == want_gen => {
                        saw_reset = true;
                    }
                    Some(MatrixEvent::Timeline {
                        room_id,
                        generation,
                        messages,
                        reached_start,
                        detached,
                    }) if room_id == want_room
                        && generation == want_gen
                        && messages.iter().any(|m| m.body == needle) =>
                    {
                        assert!(saw_reset, "restored timeline must follow TimelineReset");
                        return (reached_start, detached);
                    }
                    Some(_) => {}
                    None => panic!("the worker stopped"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!("timed out waiting for restored {needle:?} in {want_room} gen {want_gen}")
        })
    }

    /// A `/context` jump that finishes (`on_opened`) before a late `CancelOpenEvent`
    /// arrives must restore the open thread (back in `shown`, with `TimelineReset` ahead
    /// of its timeline) so newly synced replies keep arriving in the thread.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_late_cancel_after_context_switch_restores_a_thread() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::matrix::MatrixCmd;
        use crate::matrix::left::LeftRooms;
        use crate::matrix::spaces::fake;
        use serde_json::json;

        let room_a = "!a:localhost";
        let room_b = "!b:localhost";
        let root_a = "$roota:localhost";
        let event_b = "$targetb:localhost";
        let root_ev = json!({
            "type": "m.room.message",
            "room_id": room_a,
            "event_id": root_a,
            "sender": fake::ME,
            "origin_server_ts": 1_000,
            "content": { "msgtype": "m.text", "body": "thread root" }
        });
        let target_ev = json!({
            "type": "m.room.message",
            "room_id": room_b,
            "event_id": event_b,
            "sender": fake::ME,
            "origin_server_ts": 1_000,
            "content": { "msgtype": "m.text", "body": "in b via context" }
        });
        let thread_reply = json!({
            "type": "m.room.message",
            "room_id": room_a,
            "event_id": "$reply1:localhost",
            "sender": fake::ME,
            "origin_server_ts": 2_000,
            "content": {
                "msgtype": "m.text",
                "body": "reply after restore",
                "m.relates_to": {
                    "rel_type": "m.thread",
                    "event_id": root_a,
                    "is_falling_back": true,
                    "m.in_reply_to": { "event_id": root_a }
                }
            }
        });
        let mut joined_a = fake::joined(false, vec![]);
        joined_a["timeline"]["events"] = json!([root_ev.clone()]);
        let joined_b = fake::joined(false, vec![]);
        let root_json = root_ev.to_string();
        let target_json = target_ev.to_string();
        let context_b = json!({
            "event": target_ev,
            "events_before": [],
            "events_after": [],
            "start": "prev_b",
            "end": "next_b",
            "state": []
        })
        .to_string();
        let syncs = AtomicUsize::new(0);
        let server = fake::Server::answering(
            json!({ "join": { room_a: joined_a, room_b: joined_b } }),
            Box::new(move |request| {
                if request.path.ends_with("/sync") {
                    if syncs.fetch_add(1, Ordering::SeqCst) == 0 {
                        return None;
                    }
                    let sync = json!({
                        "next_batch": "s2",
                        "rooms": {
                            "join": {
                                room_a: {
                                    "timeline": { "events": [thread_reply], "limited": false }
                                }
                            }
                        }
                    });
                    return Some(fake::Reply::Json(sync.to_string()));
                }
                if request.path.contains("/relations/") {
                    return Some(fake::Reply::Json(r#"{"chunk":[]}"#.to_owned()));
                }
                if request.path.contains("/context/") && request.path.contains("targetb") {
                    return Some(fake::Reply::Json(context_b.clone()));
                }
                if request.path.contains("/event/") {
                    if request.path.contains("roota") {
                        return Some(fake::Reply::Json(root_json.clone()));
                    }
                    if request.path.contains("targetb") {
                        return Some(fake::Reply::Json(target_json.clone()));
                    }
                }
                None
            }),
        )
        .await;

        let client = fake_client_with_cache(&server.url).await;
        let (mut timelines, mut events, _watches) =
            live_timelines_of(client.clone(), &LeftRooms::default());

        // Open the thread in room A under gen 1.
        timelines
            .handle(MatrixCmd::OpenThread {
                room_id: room_a.to_owned(),
                root: root_a.to_owned(),
                generation: 1,
            })
            .await;
        wait_for_timeline(&mut events, room_a, 1, "thread root").await;
        assert_eq!(
            timelines.shown,
            [room_a.to_owned(), super::threads::key(room_a, Some(root_a))]
        );

        // Start a `/context` jump to room B (not in cache) and let `on_opened` switch.
        timelines
            .handle(MatrixCmd::OpenEvent {
                room_id: room_b.to_owned(),
                event_id: event_b.to_owned(),
                thread_root: None,
                generation: 2,
            })
            .await;
        let opened = tokio::time::timeout(Duration::from_secs(10), timelines.next_opened())
            .await
            .expect("context jump finished")
            .expect("open channel stayed open");
        timelines.on_opened(opened).await;
        wait_for_timeline(&mut events, room_b, 2, "in b via context").await;
        assert_eq!(timelines.shown, [room_b]);

        // Cancel arrives late: worker restores thread A under gen 1 and puts it back in shown.
        timelines
            .handle(MatrixCmd::CancelOpenEvent { generation: 2 })
            .await;
        assert_eq!(
            timelines.shown,
            [room_a.to_owned(), super::threads::key(room_a, Some(root_a))]
        );
        wait_for_restored_timeline(&mut events, room_a, 1, "thread root").await;

        // A reply synced into the thread reaches gen 1.
        client.sync_once(SyncSettings::default()).await.unwrap();
        wait_for_timeline(&mut events, room_a, 1, "reply after restore").await;
        drop(server);
    }

    /// A late cancel while viewing a detached `/context` slice restores that slice under
    /// its original generation, resets its `reached_start`/`reached_end` flags (so
    /// pagination works again), and keeps `PaginateForwards` wired to it. When a jump is
    /// accepted instead of cancelled, `drop_idle` and matching `Paginate` clear the
    /// stashed slice so it is never leaked.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_late_cancel_from_a_detached_slice_restores_the_slice_and_forgets_its_bounds() {
        use std::sync::atomic::Ordering;

        use crate::matrix::left::LeftRooms;
        use crate::matrix::spaces::fake;
        use crate::matrix::{MatrixCmd, MatrixEvent};
        use serde_json::json;

        let room_a = "!a:localhost";
        let room_b = "!b:localhost";
        let a_old = "$aold:localhost";
        let b_old = "$bold:localhost";
        let ev = |room: &str, id: &str, ts: u64, body: &str| {
            json!({
                "type": "m.room.message",
                "room_id": room,
                "event_id": id,
                "sender": fake::ME,
                "origin_server_ts": ts,
                "content": { "msgtype": "m.text", "body": body }
            })
        };
        let a_old_ev = ev(room_a, a_old, 1_000, "old in a");
        let a_newer_ev = ev(room_a, "$anewer:localhost", 1_500, "newer in a");
        let b_old_ev = ev(room_b, b_old, 1_000, "old in b");
        let a_old_json = a_old_ev.to_string();
        let b_old_json = b_old_ev.to_string();
        let ctx_a = json!({
            "event": a_old_ev,
            "events_before": [],
            "events_after": [],
            "start": "prev_a",
            "end": "next_a",
            "state": []
        })
        .to_string();
        let ctx_b = json!({
            "event": b_old_ev,
            "events_before": [],
            "events_after": [],
            "start": "prev_b",
            "end": "next_b",
            "state": []
        })
        .to_string();
        let fwd_a = json!({ "chunk": [a_newer_ev], "start": "next_a" }).to_string();
        let empty_page = json!({ "chunk": [], "start": "prev_a" }).to_string();
        let server = fake::Server::answering(
            json!({
                "join": {
                    room_a: fake::joined(false, vec![]),
                    room_b: fake::joined(false, vec![]),
                }
            }),
            Box::new(move |request| {
                if request.path.contains("/context/") {
                    if request.path.contains("aold") {
                        return Some(fake::Reply::Json(ctx_a.clone()));
                    }
                    if request.path.contains("bold") {
                        return Some(fake::Reply::Json(ctx_b.clone()));
                    }
                }
                if request.path.contains("/event/") {
                    if request.path.contains("aold") {
                        return Some(fake::Reply::Json(a_old_json.clone()));
                    }
                    if request.path.contains("bold") {
                        return Some(fake::Reply::Json(b_old_json.clone()));
                    }
                }
                if request.path.contains("/messages") {
                    if request.query.contains("dir=f") {
                        return Some(fake::Reply::Json(fwd_a.clone()));
                    }
                    return Some(fake::Reply::Json(empty_page.clone()));
                }
                None
            }),
        )
        .await;

        let client = fake_client_with_cache(&server.url).await;
        let (mut timelines, mut events, _watches) =
            live_timelines_of(client, &LeftRooms::default());

        // Jump 1: open detached slice `$aold` in room A (gen 1).
        timelines
            .handle(MatrixCmd::OpenEvent {
                room_id: room_a.to_owned(),
                event_id: a_old.to_owned(),
                thread_root: None,
                generation: 1,
            })
            .await;
        let opened = tokio::time::timeout(Duration::from_secs(10), timelines.next_opened())
            .await
            .unwrap()
            .unwrap();
        timelines.on_opened(opened).await;
        let (_, detached) = wait_for_timeline(&mut events, room_a, 1, "old in a").await;
        assert_eq!(
            detached.as_ref().map(|d| d.target_event.as_str()),
            Some(a_old)
        );
        // Simulate that both bounds had been marked reached before the next jump.
        if let Some(super::ActiveStream {
            kind:
                super::StreamKind::Detached {
                    reached_start,
                    reached_end,
                    ..
                },
            ..
        }) = &timelines.active
        {
            reached_start.store(true, Ordering::Release);
            reached_end.store(true, Ordering::Release);
        } else {
            panic!("expected active detached slice");
        }

        // Jump 2: switch to detached slice `$bold` in room B (gen 2), stashing slice 1.
        timelines
            .handle(MatrixCmd::OpenEvent {
                room_id: room_b.to_owned(),
                event_id: b_old.to_owned(),
                thread_root: None,
                generation: 2,
            })
            .await;
        let opened = tokio::time::timeout(Duration::from_secs(10), timelines.next_opened())
            .await
            .unwrap()
            .unwrap();
        timelines.on_opened(opened).await;
        wait_for_timeline(&mut events, room_b, 2, "old in b").await;
        assert!(timelines.before_jump.is_some());
        assert!(timelines.next_idle().is_some());

        // Late cancel of gen 2 restores slice 1 with fresh bounds and `shown == [room_a]`.
        timelines
            .handle(MatrixCmd::CancelOpenEvent { generation: 2 })
            .await;
        assert_eq!(timelines.shown, [room_a]);
        let (reached_start, detached) =
            wait_for_restored_timeline(&mut events, room_a, 1, "old in a").await;
        assert!(!reached_start);
        let detached = detached.expect("restored view stays detached");
        assert_eq!(detached.target_event, a_old);
        assert!(!detached.reached_end);

        // Forward pagination still works on the restored slice.
        timelines
            .handle(MatrixCmd::PaginateForwards {
                room_id: room_a.to_owned(),
                generation: 1,
            })
            .await;
        let mut saw_newer = false;
        let mut saw_done = false;
        tokio::time::timeout(Duration::from_secs(10), async {
            while !(saw_newer && saw_done) {
                match events.recv().await {
                    Some(MatrixEvent::Timeline {
                        room_id,
                        generation: 1,
                        messages,
                        ..
                    }) if room_id == room_a && messages.iter().any(|m| m.body == "newer in a") => {
                        saw_newer = true;
                    }
                    Some(MatrixEvent::PaginatedForwards {
                        generation: 1,
                        reached_end: true,
                    }) => {
                        saw_done = true;
                    }
                    Some(_) => {}
                    None => panic!("the worker stopped"),
                }
            }
        })
        .await
        .expect("forward pagination worked on the restored detached slice");

        // An accepted jump drops `before_jump` on `drop_idle`...
        timelines
            .handle(MatrixCmd::OpenEvent {
                room_id: room_b.to_owned(),
                event_id: b_old.to_owned(),
                thread_root: None,
                generation: 3,
            })
            .await;
        let opened = tokio::time::timeout(Duration::from_secs(10), timelines.next_opened())
            .await
            .unwrap()
            .unwrap();
        timelines.on_opened(opened).await;
        assert!(timelines.before_jump.is_some());
        timelines
            .drop_idle(std::time::Instant::now() + super::IDLE_TIMELINE + Duration::from_secs(1));
        assert!(timelines.before_jump.is_none());

        // ...and drops it immediately when the UI paginates the jump's generation.
        timelines
            .handle(MatrixCmd::OpenEvent {
                room_id: room_a.to_owned(),
                event_id: a_old.to_owned(),
                thread_root: None,
                generation: 4,
            })
            .await;
        let opened = tokio::time::timeout(Duration::from_secs(10), timelines.next_opened())
            .await
            .unwrap()
            .unwrap();
        timelines.on_opened(opened).await;
        assert!(timelines.before_jump.is_some());
        timelines
            .handle(MatrixCmd::Paginate {
                room_id: room_a.to_owned(),
                generation: 4,
            })
            .await;
        assert!(timelines.before_jump.is_none());
        drop(server);
    }

    /// A late cancel when no room was open stops the jump's forwarder and empties `shown`.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_late_cancel_with_no_room_open_stops_the_jumps_forwarder() {
        use crate::matrix::MatrixCmd;
        use crate::matrix::left::LeftRooms;
        use crate::matrix::spaces::fake;
        use serde_json::json;

        let room_a = "!a:localhost";
        let mut joined_a = fake::joined(false, vec![]);
        joined_a["timeline"]["events"] = json!([{
            "type": "m.room.message",
            "room_id": room_a,
            "event_id": "$a1:localhost",
            "sender": fake::ME,
            "origin_server_ts": 1_000,
            "content": { "msgtype": "m.text", "body": "in a" }
        }]);
        let server = fake::Server::start(json!({ "join": { room_a: joined_a } })).await;
        let client = fake_client_with_cache(&server.url).await;
        let (mut timelines, mut events, _watches) =
            live_timelines_of(client, &LeftRooms::default());

        // Warm room A, then close the chat so no room is open.
        timelines
            .handle(MatrixCmd::OpenRoom {
                room_id: room_a.to_owned(),
                generation: 1,
            })
            .await;
        wait_for_timeline(&mut events, room_a, 1, "in a").await;
        timelines.handle(MatrixCmd::CloseRoom).await;
        assert!(timelines.active.is_none() && timelines.shown.is_empty());

        // Fast-path jump switches to room A under gen 2, then cancel arrives late.
        timelines
            .handle(MatrixCmd::OpenEvent {
                room_id: room_a.to_owned(),
                event_id: "$a1:localhost".to_owned(),
                thread_root: None,
                generation: 2,
            })
            .await;
        wait_for_timeline(&mut events, room_a, 2, "in a").await;
        let jump_forwarder = timelines
            .forwarder
            .as_ref()
            .expect("jump forwarder running")
            .abort_handle();

        timelines
            .handle(MatrixCmd::CancelOpenEvent { generation: 2 })
            .await;
        assert!(timelines.forwarder.is_none());
        assert!(timelines.active.is_none());
        assert!(timelines.before_jump.is_none());
        assert!(timelines.shown.is_empty());
        tokio::time::timeout(Duration::from_secs(5), async {
            while !jump_forwarder.is_finished() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("jump forwarder stopped");
        drop(server);
    }

    /// A cancel that arrives while the jump is still pending (or after a jump failed)
    /// leaves the open room's forwarder running without restarting or resetting it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancel_in_time_and_a_cancel_after_failure_leave_the_open_room_alone() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::matrix::left::LeftRooms;
        use crate::matrix::spaces::fake;
        use crate::matrix::{MatrixCmd, MatrixEvent};
        use serde_json::json;

        let room_a = "!a:localhost";
        let room_b = "!b:localhost";
        let msg = |room: &str, id: &str, ts: u64, body: &str| {
            json!({
                "type": "m.room.message",
                "room_id": room,
                "event_id": id,
                "sender": fake::ME,
                "origin_server_ts": ts,
                "content": { "msgtype": "m.text", "body": body }
            })
        };
        let mut joined_a = fake::joined(false, vec![]);
        joined_a["timeline"]["events"] = json!([msg(room_a, "$a1:localhost", 1_000, "in a")]);
        let later_a = msg(room_a, "$a2:localhost", 2_000, "later in a");
        let syncs = AtomicUsize::new(0);
        let server = fake::Server::answering(
            json!({
                "join": {
                    room_a: joined_a,
                    room_b: fake::joined(false, vec![]),
                }
            }),
            Box::new(move |request| {
                if request.path.ends_with("/sync") {
                    if syncs.fetch_add(1, Ordering::SeqCst) == 0 {
                        return None;
                    }
                    let sync = json!({
                        "next_batch": "s2",
                        "rooms": {
                            "join": {
                                room_a: {
                                    "timeline": { "events": [later_a], "limited": false }
                                }
                            }
                        }
                    });
                    return Some(fake::Reply::Json(sync.to_string()));
                }
                if request.path.contains("/event/") {
                    // Hold `/event/` indefinitely so the jump stays pending.
                    return Some(fake::Reply::Never);
                }
                None
            }),
        )
        .await;

        let client = fake_client_with_cache(&server.url).await;
        let (mut timelines, mut events, _watches) =
            live_timelines_of(client.clone(), &LeftRooms::default());

        timelines
            .handle(MatrixCmd::OpenRoom {
                room_id: room_a.to_owned(),
                generation: 1,
            })
            .await;
        wait_for_timeline(&mut events, room_a, 1, "in a").await;
        let original = timelines
            .forwarder
            .as_ref()
            .expect("room A forwarder running")
            .abort_handle();

        // 1. Cancel arrives in time while `/event/` is still in flight.
        timelines
            .handle(MatrixCmd::OpenEvent {
                room_id: room_b.to_owned(),
                event_id: "$b1:localhost".to_owned(),
                thread_root: None,
                generation: 2,
            })
            .await;
        assert_eq!(timelines.pending_open_event_gen, Some(2));
        timelines
            .handle(MatrixCmd::CancelOpenEvent { generation: 2 })
            .await;
        assert!(timelines.pending_open_event_gen.is_none());
        assert!(!original.is_finished(), "original forwarder stayed running");
        assert_eq!(timelines.shown, [room_a]);

        // 2. Jump fails (`JumpFailed`), then a late cancel arrives for it.
        timelines
            .handle(MatrixCmd::OpenEvent {
                room_id: "!missing:localhost".to_owned(),
                event_id: "$x:localhost".to_owned(),
                thread_root: None,
                generation: 3,
            })
            .await;
        timelines
            .handle(MatrixCmd::CancelOpenEvent { generation: 3 })
            .await;
        assert!(!original.is_finished(), "original forwarder stayed running");
        assert_eq!(timelines.shown, [room_a]);

        // Drain queued events: we saw JumpFailed(3), and no TimelineReset was emitted.
        let mut saw_failed = false;
        while let Ok(event) = events.try_recv() {
            match event {
                MatrixEvent::JumpFailed { generation: 3, .. } => saw_failed = true,
                MatrixEvent::TimelineReset { .. } => {
                    panic!("in-time / post-failure cancel must not reset the open room")
                }
                _ => {}
            }
        }
        assert!(saw_failed);

        // Live updates keep flowing on the original forwarder.
        client.sync_once(SyncSettings::default()).await.unwrap();
        wait_for_timeline(&mut events, room_a, 1, "later in a").await;
        drop(server);
    }

    /// Two jumps in a row that both switch before the UI cancels them still restore the
    /// original room the UI never left.
    #[tokio::test(flavor = "multi_thread")]
    async fn chained_jumps_cancelled_late_restore_the_original_room() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::matrix::MatrixCmd;
        use crate::matrix::left::LeftRooms;
        use crate::matrix::spaces::fake;
        use serde_json::json;

        let (room_a, room_b, room_c) = ("!a:localhost", "!b:localhost", "!c:localhost");
        let msg = |room: &str, id: &str, ts: u64, body: &str| {
            json!({
                "type": "m.room.message",
                "room_id": room,
                "event_id": id,
                "sender": fake::ME,
                "origin_server_ts": ts,
                "content": { "msgtype": "m.text", "body": body }
            })
        };
        let joined = |room: &str, id: &str, body: &str| {
            let mut j = fake::joined(false, vec![]);
            j["timeline"]["events"] = json!([msg(room, id, 1_000, body)]);
            j
        };
        let later_a = msg(room_a, "$a2:localhost", 2_000, "later in a");
        let syncs = AtomicUsize::new(0);
        let server = fake::Server::answering(
            json!({
                "join": {
                    room_a: joined(room_a, "$a1:localhost", "in a"),
                    room_b: joined(room_b, "$b1:localhost", "in b"),
                    room_c: joined(room_c, "$c1:localhost", "in c"),
                }
            }),
            Box::new(move |request| {
                if !request.path.ends_with("/sync") || syncs.fetch_add(1, Ordering::SeqCst) == 0 {
                    return None;
                }
                let sync = json!({
                    "next_batch": "s2",
                    "rooms": {
                        "join": {
                            room_a: {
                                "timeline": { "events": [later_a], "limited": false }
                            }
                        }
                    }
                });
                Some(fake::Reply::Json(sync.to_string()))
            }),
        )
        .await;

        let client = fake_client_with_cache(&server.url).await;
        let (mut timelines, mut events, _watches) =
            live_timelines_of(client.clone(), &LeftRooms::default());

        // Warm B and C, then open A (gen 3).
        timelines
            .handle(MatrixCmd::OpenRoom {
                room_id: room_b.to_owned(),
                generation: 1,
            })
            .await;
        wait_for_timeline(&mut events, room_b, 1, "in b").await;
        timelines
            .handle(MatrixCmd::OpenRoom {
                room_id: room_c.to_owned(),
                generation: 2,
            })
            .await;
        wait_for_timeline(&mut events, room_c, 2, "in c").await;
        timelines
            .handle(MatrixCmd::OpenRoom {
                room_id: room_a.to_owned(),
                generation: 3,
            })
            .await;
        wait_for_timeline(&mut events, room_a, 3, "in a").await;

        // Jump 1 (gen 4) switches to B; before the UI hears it, jump 2 cancels gen 4 and
        // asks for C (gen 5), which also switches fast; then the UI cancels gen 5 late.
        timelines
            .handle(MatrixCmd::OpenEvent {
                room_id: room_b.to_owned(),
                event_id: "$b1:localhost".to_owned(),
                thread_root: None,
                generation: 4,
            })
            .await;
        wait_for_timeline(&mut events, room_b, 4, "in b").await;
        timelines
            .handle(MatrixCmd::CancelOpenEvent { generation: 4 })
            .await;
        let mid_forwarder = timelines
            .forwarder
            .as_ref()
            .expect("mid-restore forwarder running")
            .abort_handle();
        timelines
            .handle(MatrixCmd::OpenEvent {
                room_id: room_c.to_owned(),
                event_id: "$c1:localhost".to_owned(),
                thread_root: None,
                generation: 5,
            })
            .await;
        wait_for_timeline(&mut events, room_c, 5, "in c").await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !mid_forwarder.is_finished() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("superseded forwarder finished");
        while events.try_recv().is_ok() {}
        timelines
            .handle(MatrixCmd::CancelOpenEvent { generation: 5 })
            .await;

        assert_eq!(timelines.shown, [room_a]);
        wait_for_restored_timeline(&mut events, room_a, 3, "in a").await;
        client.sync_once(SyncSettings::default()).await.unwrap();
        wait_for_timeline(&mut events, room_a, 3, "later in a").await;
        drop(server);
    }

    #[test]
    fn an_unread_mark_is_only_skipped_when_nothing_speaks_against_it() {
        let needs = super::UnreadFlags::needs_request;
        // Known to be that way: nothing to do.
        assert!(!needs(false, None, false));
        assert!(!needs(true, Some(true), true));
        // Marked here a moment ago, the sync still says otherwise, and now the room
        // is opened: the mark must be taken back.
        assert!(needs(false, Some(true), false));
        // We took the mark back, the sync confirmed it, then another client marked
        // the room: reading it here must take that mark back too.
        assert!(needs(true, Some(false), false));
        // And the other way round.
        assert!(needs(false, Some(true), true));
        assert!(needs(false, None, true));
    }

    use matrix_sdk::config::SyncSettings;

    use crate::complete::MentionMark;
    use crate::format::parse_with_mentions;
    use crate::matrix::{MatrixCmd, MatrixEvent, spawn};
    use crate::paths::Paths;

    /// Alice logs in, fetches room members, mentions Bob, and Bob receives the event with
    /// `m.mentions` containing Bob's user id.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver (scripts/dev-homeserver.sh)"]
    async fn alice_mentioning_bob_delivers_m_mentions() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
            .with_test_writer()
            .try_init();
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());

        let root = std::env::temp_dir().join(format!("matrix-rat-mention-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "it".into(),
        };
        paths.create_dirs().unwrap();
        let (handle, mut events) = spawn(paths);
        handle.send(MatrixCmd::LoginPassword {
            homeserver: homeserver.clone(),
            username: "alice".to_owned(),
            password: "alicepass".to_owned().into(),
        });

        let bob = matrix_sdk::Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .unwrap();
        bob.matrix_auth()
            .login_username("bob", "bobpass")
            .initial_device_display_name("matrix-rat mention test")
            .send()
            .await
            .unwrap();
        bob.event_cache().subscribe().unwrap();
        bob.sync_once(SyncSettings::default()).await.unwrap();
        let bob_id = bob.user_id().unwrap().to_owned();
        let bob_sync = tokio::spawn({
            let bob = bob.clone();
            async move {
                let _ = bob.sync(SyncSettings::default()).await;
            }
        });

        // Wait until Alice's worker reports a joined room that Bob is already in (preferring
        // unencrypted so history queries avoid undecryptable old events; falls back if needed).
        let bob_room = tokio::time::timeout(Duration::from_secs(60), async {
            let mut fallback = None;
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    MatrixEvent::Rooms(rooms) => {
                        for listed in rooms.into_iter().filter(|r| !r.is_pending()) {
                            let Ok(room_id) =
                                <&matrix_sdk::ruma::RoomId>::try_from(listed.id.as_str())
                            else {
                                continue;
                            };
                            let joined = if let Some(room) = bob.get_room(room_id)
                                && room.state() == matrix_sdk::RoomState::Joined
                            {
                                Some(room)
                            } else {
                                bob.join_room_by_id(room_id).await.ok()
                            };
                            if let Some(room) = joined {
                                if !room.encryption_state().is_encrypted() {
                                    return room;
                                }
                                fallback.get_or_insert(room);
                            }
                        }
                    }
                    _ => {}
                }
                if let Some(room) = fallback.take()
                    && events.is_empty()
                {
                    return room;
                }
            }
        })
        .await
        .expect("timed out finding a shared room between alice and bob");
        let room_id = bob_room.room_id().to_string();

        // Alice asks for the members and opens the room, in the order the UI does it
        // when switching rooms: opening must not stop the list from being loaded.
        handle.send(MatrixCmd::FetchMembers {
            room_id: room_id.clone(),
        });
        handle.send(MatrixCmd::OpenRoom {
            room_id: room_id.clone(),
            generation: 1,
        });
        let bob_member = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::ActionFailed(err) | MatrixEvent::Error(err) => {
                        panic!("worker error: {err}")
                    }
                    MatrixEvent::RoomMembers {
                        room_id: ev_room,
                        members,
                        ..
                    } if ev_room == room_id => {
                        if let Some(found) =
                            members.into_iter().find(|m| m.user_id == bob_id.as_str())
                        {
                            return found;
                        }
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for Bob in RoomMembers");

        // Send a message using the mention completion mark and verify Bob receives m.mentions.
        let needle = format!("mention-check-{}", std::process::id());
        let text = format!("{needle} {}", bob_member.display_name);
        let expected_uid = bob_member.user_id.clone();
        let outgoing = parse_with_mentions(
            &text,
            &[MentionMark::user(
                bob_member.display_name,
                bob_member.user_id,
            )],
            false,
        );
        handle.send(MatrixCmd::Send {
            room_id: room_id.clone(),
            message: outgoing,
            reply_to: None,
        });

        // Confirm Alice's worker sees the sent event on the timeline before polling Bob.
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::ActionFailed(err) | MatrixEvent::Error(err) => {
                        panic!("send failed: {err}")
                    }
                    MatrixEvent::Timeline {
                        room_id: ev_room,
                        messages,
                        ..
                    } if ev_room == room_id
                        && messages.iter().any(|m| {
                            m.body.contains(&needle)
                                && matches!(m.item, Some(crate::model::ItemRef::Event(_)))
                        }) =>
                    {
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for Alice send confirmation");

        let received = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let page = bob_room
                    .messages(matrix_sdk::room::MessagesOptions::backward())
                    .await
                    .unwrap();
                for event in page.chunk {
                    let Ok(matrix_sdk::ruma::events::AnySyncTimelineEvent::MessageLike(
                        matrix_sdk::ruma::events::AnySyncMessageLikeEvent::RoomMessage(
                            matrix_sdk::ruma::events::SyncMessageLikeEvent::Original(ev),
                        ),
                    )) = event.raw().deserialize()
                    else {
                        continue;
                    };
                    if let matrix_sdk::ruma::events::room::message::MessageType::Text(t) =
                        &ev.content.msgtype
                        && t.body.contains(&needle)
                    {
                        let formatted = t.formatted.as_ref().map(|f| f.body.as_str()).unwrap_or("");
                        assert!(formatted.contains(&expected_uid), "formatted: {formatted}");
                        return ev.content.mentions;
                    }
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        })
        .await
        .expect("timed out waiting for Bob to receive mention");
        let mentions = received.expect("message should carry m.mentions");
        assert!(mentions.user_ids.contains(&bob_id));

        bob_sync.abort();
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = bob.matrix_auth().logout().await;
        let _ = std::fs::remove_dir_all(root);
    }

    /// "Marked as unread" is shared between the clients of an account: a mark set by
    /// another client of the test user shows in the worker's room list and one set
    /// through the worker shows in the other client, and likewise when taken back.
    /// Marking a room as read takes the mark back too, and moves the read marker
    /// (`m.fully_read`), which is where the "new" line of the next visit comes from.
    /// `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=... cargo test -- --ignored`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver (scripts/dev-homeserver.sh)"]
    async fn a_room_marked_unread_shows_in_the_users_other_clients() {
        use std::time::Duration;

        use matrix_sdk::config::SyncSettings;
        use matrix_sdk::ruma::RoomId;
        use matrix_sdk::ruma::events::fully_read::FullyReadEventContent;
        use matrix_sdk::{Client, Room};

        use crate::matrix::{MatrixEvent, spawn};
        use crate::model::ItemRef;
        use crate::paths::Paths;

        let var = |name: &str| std::env::var(name).ok();
        let homeserver =
            var("MATRIX_RAT_TEST_HOMESERVER").unwrap_or_else(|| "http://127.0.0.1:6167".into());
        let username = var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");

        // The same user in another client.
        let other = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .expect("client");
        other
            .matrix_auth()
            .login_username(&username, &password)
            .initial_device_display_name("matrix-rat unread test")
            .send()
            .await
            .expect("login of the second client");
        let quick = || SyncSettings::default().timeout(Duration::from_secs(1));
        other.sync_once(quick()).await.expect("sync");

        let root = std::env::temp_dir().join(format!("matrix-rat-unread-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "it".into(),
        };
        paths.create_dirs().unwrap();
        let (handle, mut events) = spawn(paths);
        handle.send(MatrixCmd::LoginPassword {
            homeserver,
            username,
            password: password.into(),
        });

        // Waits for a room list in which the room is marked or not, as wanted.
        // `None` for the room: any joined room that both clients know.
        async fn listed(
            events: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixEvent>,
            other: &Client,
            room_id: Option<&str>,
            marked: bool,
        ) -> String {
            let wait = async {
                loop {
                    match events.recv().await.expect("worker stopped") {
                        MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                        MatrixEvent::ActionFailed(err) => panic!("{err}"),
                        MatrixEvent::Rooms(rooms) => {
                            let found = rooms.iter().find(|summary| match room_id {
                                Some(id) => summary.id == id,
                                None => {
                                    !summary.is_pending()
                                        && RoomId::parse(&summary.id)
                                            .ok()
                                            .and_then(|id| other.get_room(&id))
                                            .is_some_and(|room| {
                                                room.state() == matrix_sdk::RoomState::Joined
                                            })
                                }
                            });
                            if let Some(summary) = found
                                && (room_id.is_none() || summary.marked_unread == marked)
                            {
                                return summary.id.clone();
                            }
                        }
                        _ => {}
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(30), wait)
                .await
                .unwrap_or_else(|_| panic!("the room list never said marked: {marked}"))
        }

        // Syncs the other client until it sees the room marked or not, as wanted.
        async fn seen(other: &Client, room: &Room, marked: bool) {
            let quick = SyncSettings::default().timeout(Duration::from_secs(1));
            for _ in 0..30 {
                other.sync_once(quick.clone()).await.expect("sync");
                if room.is_marked_unread() == marked {
                    return;
                }
            }
            panic!("the other client never saw marked: {marked}");
        }

        let room_id = listed(&mut events, &other, None, false).await;
        let other_room = other.get_room(&RoomId::parse(&room_id).unwrap()).unwrap();
        // Start from a room that is not marked, whatever an earlier run left.
        other_room.set_unread_flag(false).await.expect("unmark");
        seen(&other, &other_room, false).await;
        listed(&mut events, &other, Some(&room_id), false).await;

        // Marked in the other client: the worker's room list says so.
        other_room.set_unread_flag(true).await.expect("mark");
        listed(&mut events, &other, Some(&room_id), true).await;
        // Taken back through the worker: the other client sees it gone.
        handle.send(MatrixCmd::SetUnread {
            room_id: room_id.clone(),
            unread: false,
        });
        seen(&other, &other_room, false).await;
        listed(&mut events, &other, Some(&room_id), false).await;

        // Marked through the worker: the other client sees it, and so does the list.
        handle.send(MatrixCmd::SetUnread {
            room_id: room_id.clone(),
            unread: true,
        });
        seen(&other, &other_room, true).await;
        listed(&mut events, &other, Some(&room_id), true).await;
        // Taken back there. (The other client goes by what its last sync said, so
        // it syncs before it is asked for the next change.)
        other_room.set_unread_flag(false).await.expect("unmark");
        listed(&mut events, &other, Some(&room_id), false).await;
        seen(&other, &other_room, false).await;

        // Marking many rooms as read takes the mark back as well, without the room
        // being open; a room that is not known is passed over.
        other_room.set_unread_flag(true).await.expect("mark");
        listed(&mut events, &other, Some(&room_id), true).await;
        handle.send(MatrixCmd::MarkRoomsRead {
            room_ids: vec!["!nowhere:example.org".into(), room_id.clone()],
            public: false,
        });
        seen(&other, &other_room, false).await;
        listed(&mut events, &other, Some(&room_id), false).await;

        // Marked once more, and then read here: a message is sent first, so that
        // there is a newest message that is known for certain.
        other_room.set_unread_flag(true).await.expect("mark");
        listed(&mut events, &other, Some(&room_id), true).await;
        let text = format!("unread test {}", std::process::id());
        handle.send(MatrixCmd::OpenRoom {
            room_id: room_id.clone(),
            generation: 1,
        });
        handle.send(MatrixCmd::Send {
            room_id: room_id.clone(),
            message: crate::format::parse(&text),
            reply_to: None,
        });
        let sent = async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::Timeline { messages, .. } => {
                        let ours = messages.iter().find(|m| m.body == text);
                        if let Some(ItemRef::Event(id)) = ours.and_then(|m| m.item.as_ref()) {
                            return id.clone();
                        }
                    }
                    MatrixEvent::ActionFailed(err) => panic!("{err}"),
                    _ => {}
                }
            }
        };
        let newest = tokio::time::timeout(Duration::from_secs(30), sent)
            .await
            .expect("our message was not sent");
        handle.send(MatrixCmd::MarkRead {
            room_id: room_id.clone(),
            public: false,
        });
        seen(&other, &other_room, false).await;
        listed(&mut events, &other, Some(&room_id), false).await;
        // The read marker is on the newest message, for all clients of the user.
        // (Our message counts as the room's newest once the server has sent it
        // back; marking again until then is what reading on does in the app.)
        let mut marker = None;
        for _ in 0..30 {
            handle.send(MatrixCmd::MarkRead {
                room_id: room_id.clone(),
                public: false,
            });
            other.sync_once(quick()).await.expect("sync");
            marker = other_room
                .account_data_static::<FullyReadEventContent>()
                .await
                .expect("account data")
                .and_then(|raw| raw.deserialize().ok())
                .map(|event| event.content.event_id.to_string());
            if marker.as_deref() == Some(newest.as_str()) {
                break;
            }
        }
        assert_eq!(marker.as_deref(), Some(newest.as_str()));

        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = other.matrix_auth().logout().await;
        let _ = std::fs::remove_dir_all(root);
    }

    /// Where the "new" line comes from: the timeline carries a read marker in front
    /// of what someone else wrote since the room was last marked as read, also when
    /// the room is opened afresh; marking the room as read takes it away.
    /// `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=... cargo test -- --ignored`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver (scripts/dev-homeserver.sh)"]
    async fn the_read_marker_stands_before_what_was_not_read() {
        use matrix_sdk::Client;
        use matrix_sdk::ruma::RoomId;
        use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;

        use crate::model::{Message, MsgKind};

        let var = |name: &str| std::env::var(name).ok();
        let homeserver =
            var("MATRIX_RAT_TEST_HOMESERVER").unwrap_or_else(|| "http://127.0.0.1:6167".into());
        let username = var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let other_name = var("MATRIX_RAT_TEST_USER2").unwrap_or_else(|| "bob".into());
        let other_password = var("MATRIX_RAT_TEST_PASSWORD2").unwrap_or_else(|| "bobpass".into());

        let other = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .expect("client");
        other
            .matrix_auth()
            .login_username(&other_name, &other_password)
            .initial_device_display_name("matrix-rat marker test")
            .send()
            .await
            .expect("login of the second user");
        other
            .sync_once(SyncSettings::default())
            .await
            .expect("sync");

        let root = std::env::temp_dir().join(format!("matrix-rat-marker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "it".into(),
        };
        paths.create_dirs().unwrap();
        let (handle, mut events) = spawn(paths);
        handle.send(MatrixCmd::LoginPassword {
            homeserver,
            username,
            password: password.into(),
        });

        // A room both have joined, without encryption (the other client is a plain
        // one and could not write there). By then the worker has also said that the
        // first sync is through.
        let find_room = async {
            let mut caught_up = false;
            let mut found = None;
            loop {
                if let (true, Some(room_id)) = (caught_up, &found) {
                    return String::clone(room_id);
                }
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    MatrixEvent::CaughtUp => {
                        assert!(!caught_up, "reported twice");
                        caught_up = true;
                    }
                    MatrixEvent::Rooms(rooms) => {
                        let shared = rooms.iter().find(|summary| {
                            !summary.is_pending()
                                && RoomId::parse(&summary.id)
                                    .ok()
                                    .and_then(|id| other.get_room(&id))
                                    .is_some_and(|room| {
                                        room.state() == matrix_sdk::RoomState::Joined
                                            && !room.encryption_state().is_encrypted()
                                    })
                        });
                        if let Some(summary) = shared {
                            found = Some(summary.id.clone());
                        }
                    }
                    _ => {}
                }
            }
        };
        let room_id = tokio::time::timeout(Duration::from_secs(30), find_room)
            .await
            .expect(
                "no unencrypted room that both users have joined, or no word of the first sync",
            );
        let other_room = other.get_room(&RoomId::parse(&room_id).unwrap()).unwrap();

        // Waits for messages of the room, as asked for with `generation`, that
        // `check` accepts.
        async fn until<T>(
            events: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixEvent>,
            generation: u64,
            what: &str,
            check: impl Fn(&[Message]) -> Option<T>,
        ) -> T {
            let wait = async {
                loop {
                    match events.recv().await.expect("worker stopped") {
                        MatrixEvent::Timeline {
                            messages,
                            generation: of,
                            ..
                        } if of == generation => {
                            if let Some(found) = check(&messages) {
                                return found;
                            }
                        }
                        MatrixEvent::ActionFailed(err) => panic!("{err}"),
                        _ => {}
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(30), wait)
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        }
        let markers = |messages: &[Message]| {
            messages
                .iter()
                .filter(|m| m.kind == MsgKind::ReadMarker)
                .count()
        };

        // We write and have read everything.
        let run = std::process::id();
        let ours = format!("marker test {run}: ours");
        handle.send(MatrixCmd::OpenRoom {
            room_id: room_id.clone(),
            generation: 1,
        });
        handle.send(MatrixCmd::Send {
            room_id: room_id.clone(),
            message: crate::format::parse(&ours),
            reply_to: None,
        });
        until(&mut events, 1, "our message to be sent", |messages| {
            let sent = messages.iter().find(|m| m.body == ours)?;
            matches!(sent.item, Some(crate::model::ItemRef::Event(_))).then_some(())
        })
        .await;
        handle.send(MatrixCmd::MarkRead {
            room_id: room_id.clone(),
            public: false,
        });

        // Reading is not answered: give it the time to reach the server, or what
        // comes next would be read along with the rest.
        tokio::time::sleep(Duration::from_secs(2)).await;

        // The other user writes twice: the marker stands right before the first.
        let first = format!("marker test {run}: first");
        let second = format!("marker test {run}: second");
        for text in [&first, &second] {
            other_room
                .send(RoomMessageEventContent::text_plain(text.as_str()))
                .await
                .expect("message of the second user");
        }
        let before_first = |messages: &[Message]| {
            let at = messages.iter().position(|m| m.body == first)?;
            messages.iter().find(|m| m.body == second)?;
            (at > 0 && messages[at - 1].kind == MsgKind::ReadMarker && markers(messages) == 1)
                .then_some(())
        };
        until(&mut events, 1, "the marker before the news", before_first).await;

        // Opened afresh (as after going to another room and back), the first thing
        // the worker sends has it there as well: that is what the app places its
        // "new" line by.
        handle.send(MatrixCmd::OpenRoom {
            room_id: room_id.clone(),
            generation: 2,
        });
        let at_once = until(&mut events, 2, "the room's messages", |messages| {
            Some(before_first(messages).is_some())
        })
        .await;
        assert!(at_once, "the first update after opening had no marker");

        // Read: the marker goes (to the end, where it is not shown).
        handle.send(MatrixCmd::MarkRead {
            room_id: room_id.clone(),
            public: false,
        });
        until(&mut events, 2, "the marker to go", |messages| {
            (markers(messages) == 0).then_some(())
        })
        .await;
        handle.send(MatrixCmd::OpenRoom {
            room_id: room_id.clone(),
            generation: 3,
        });
        let gone = until(&mut events, 3, "the room's messages", |messages| {
            Some(markers(messages) == 0)
        })
        .await;
        assert!(gone, "a marker though everything was read");

        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = other.matrix_auth().logout().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver (scripts/dev-homeserver.sh)"]
    async fn alice_jumps_to_unloaded_message_across_more_than_60_events() {
        use matrix_sdk::Client;
        use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoom;
        use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
        use std::time::Duration;

        let _ = tracing_subscriber::fmt()
            .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
            .with_test_writer()
            .try_init();
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let user = std::env::var("MATRIX_RAT_TEST_USER").unwrap_or_else(|_| "alice".to_owned());
        let password =
            std::env::var("MATRIX_RAT_TEST_PASSWORD").unwrap_or_else(|_| "alicepass".to_owned());

        let setup = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .expect("build temp client");
        setup
            .matrix_auth()
            .login_username(&user, &password)
            .await
            .expect("login temp client");
        let created = setup
            .create_room(CreateRoom::new())
            .await
            .expect("create room");
        let room_id = created.room_id().to_string();

        let mut first_event_id = None;
        for i in 0..65 {
            let sent = created
                .send(RoomMessageEventContent::text_plain(format!("msg-{i:03}")))
                .await
                .expect("send message");
            if i == 0 {
                first_event_id = Some(sent.response.event_id.to_string());
            }
        }
        let first_event_id = first_event_id.expect("recorded msg-000 event id");
        let _ = setup.matrix_auth().logout().await;

        let root = std::env::temp_dir().join(format!("matrix-rat-jump-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "it".into(),
        };
        paths.create_dirs().unwrap();
        let (handle, mut events) = spawn(paths);
        handle.send(MatrixCmd::LoginPassword {
            homeserver,
            username: user,
            password: password.into(),
        });

        tokio::time::timeout(Duration::from_secs(30), async {
            let mut synced = false;
            let mut saw_room = false;
            while !(synced && saw_room) {
                match events.recv().await.expect("worker event") {
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    MatrixEvent::Sync(crate::matrix::SyncStatus::Synced) => synced = true,
                    MatrixEvent::Rooms(list) if list.iter().any(|r| r.id == room_id) => {
                        saw_room = true;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for sync and room list");

        // Open the room normally: initial live slice contains msg-064 and omits msg-000.
        handle.send(MatrixCmd::OpenRoom {
            room_id: room_id.clone(),
            generation: 1,
        });
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match events.recv().await.expect("worker event") {
                    MatrixEvent::Error(err) | MatrixEvent::ActionFailed(err) => panic!("{err}"),
                    MatrixEvent::Timeline {
                        room_id: ev_room,
                        generation: 1,
                        messages,
                        detached: None,
                        ..
                    } if ev_room == room_id && messages.iter().any(|m| m.body == "msg-064") => {
                        assert!(
                            !messages.iter().any(|m| m.body == "msg-000"),
                            "initial live slice should not already hold msg-000"
                        );
                        return;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for initial live timeline");

        // Jump to msg-000 across >60 events via /context detached timeline.
        handle.send(MatrixCmd::OpenEvent {
            room_id: room_id.clone(),
            event_id: first_event_id.clone(),
            thread_root: None,
            generation: 2,
        });
        let initial_slice_len = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match events.recv().await.expect("worker event") {
                    MatrixEvent::JumpFailed { reason, .. } => panic!("jump failed: {reason}"),
                    MatrixEvent::Error(err) | MatrixEvent::ActionFailed(err) => panic!("{err}"),
                    MatrixEvent::Timeline {
                        room_id: ev_room,
                        generation: 2,
                        messages,
                        detached: Some(detached),
                        ..
                    } if ev_room == room_id => {
                        assert_eq!(detached.target_event, first_event_id);
                        assert!(!detached.reached_end);
                        assert!(
                            messages.iter().any(|m| m.body == "msg-000"),
                            "detached slice should load target msg-000"
                        );
                        return messages.len();
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for detached event timeline");

        // Paginate forwards from the detached slice and observe growth.
        handle.send(MatrixCmd::PaginateForwards {
            room_id: room_id.clone(),
            generation: 2,
        });
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut saw_forward_ack = false;
            let mut latest_len = initial_slice_len;
            loop {
                match events.recv().await.expect("worker event") {
                    MatrixEvent::Error(err) | MatrixEvent::ActionFailed(err) => panic!("{err}"),
                    MatrixEvent::PaginatedForwards { generation: 2, .. } => {
                        saw_forward_ack = true;
                    }
                    MatrixEvent::Timeline {
                        room_id: ev_room,
                        generation: 2,
                        messages,
                        ..
                    } if ev_room == room_id => {
                        latest_len = messages.len();
                    }
                    _ => {}
                }
                if saw_forward_ack && latest_len > initial_slice_len {
                    return;
                }
            }
        })
        .await
        .expect("timed out waiting for forward growth in detached timeline");

        // Return to the live timeline for the room.
        handle.send(MatrixCmd::OpenRoom {
            room_id: room_id.clone(),
            generation: 3,
        });
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match events.recv().await.expect("worker event") {
                    MatrixEvent::Error(err) | MatrixEvent::ActionFailed(err) => panic!("{err}"),
                    MatrixEvent::Timeline {
                        room_id: ev_room,
                        generation: 3,
                        messages,
                        detached: None,
                        ..
                    } if ev_room == room_id && messages.iter().any(|m| m.body == "msg-064") => {
                        return;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out returning to live timeline");

        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    #[ignore = "needs a local homeserver (scripts/dev-homeserver.sh)"]
    async fn detached_timeline_reports_new_live_messages() {
        use matrix_sdk::Client;
        use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoom;
        use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
        use std::time::Duration;

        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let user = std::env::var("MATRIX_RAT_TEST_USER").unwrap_or_else(|_| "alice".to_owned());
        let password =
            std::env::var("MATRIX_RAT_TEST_PASSWORD").unwrap_or_else(|_| "alicepass".to_owned());
        let bob_name = std::env::var("MATRIX_RAT_TEST_USER2").unwrap_or_else(|_| "bob".to_owned());
        let bob_pass =
            std::env::var("MATRIX_RAT_TEST_PASSWORD2").unwrap_or_else(|_| "bobpass".to_owned());

        let alice_setup = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .unwrap();
        alice_setup
            .matrix_auth()
            .login_username(&user, &password)
            .await
            .unwrap();
        let bob = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .unwrap();
        bob.matrix_auth()
            .login_username(&bob_name, &bob_pass)
            .await
            .unwrap();

        let created = alice_setup.create_room(CreateRoom::new()).await.unwrap();
        let room_id = created.room_id().to_string();
        created
            .invite_user_by_id(bob.user_id().unwrap())
            .await
            .unwrap();
        let bob_room = bob.join_room_by_id(created.room_id()).await.unwrap();

        let mut first_event_id = None;
        for i in 0..65 {
            let sent = created
                .send(RoomMessageEventContent::text_plain(format!("old-{i:03}")))
                .await
                .unwrap();
            if i == 0 {
                first_event_id = Some(sent.response.event_id.to_string());
            }
        }
        let first_event_id = first_event_id.unwrap();

        let root =
            std::env::temp_dir().join(format!("matrix-rat-detached-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "it".into(),
        };
        paths.create_dirs().unwrap();
        let (handle, mut events) = spawn(paths);
        handle.send(MatrixCmd::LoginPassword {
            homeserver,
            username: user,
            password: password.into(),
        });

        tokio::time::timeout(Duration::from_secs(30), async {
            let mut synced = false;
            let mut saw_room = false;
            while !(synced && saw_room) {
                match events.recv().await.expect("worker event") {
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    MatrixEvent::Sync(crate::matrix::SyncStatus::Synced) => synced = true,
                    MatrixEvent::Rooms(list) if list.iter().any(|r| r.id == room_id) => {
                        saw_room = true;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for sync");

        // Open live first so the live timeline is warm and cached.
        handle.send(MatrixCmd::OpenRoom {
            room_id: room_id.clone(),
            generation: 1,
        });
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match events.recv().await.expect("worker event") {
                    MatrixEvent::Timeline {
                        room_id: r,
                        generation: 1,
                        messages,
                        ..
                    } if r == room_id && messages.iter().any(|m| m.body == "old-064") => return,
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out opening live room");

        // Jump to msg-000 into a detached `/context` timeline.
        handle.send(MatrixCmd::OpenEvent {
            room_id: room_id.clone(),
            event_id: first_event_id,
            thread_root: None,
            generation: 2,
        });
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match events.recv().await.expect("worker event") {
                    MatrixEvent::Timeline {
                        room_id: r,
                        generation: 2,
                        detached: Some(_),
                        ..
                    } if r == room_id => return,
                    MatrixEvent::JumpFailed { reason, .. } => panic!("{reason}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out opening detached timeline");

        // Alice sending from another client should NOT increment new count; Bob sending 2 should.
        created
            .send(RoomMessageEventContent::text_plain("own later message"))
            .await
            .unwrap();
        bob_room
            .send(RoomMessageEventContent::text_plain("bob while detached 1"))
            .await
            .unwrap();
        bob_room
            .send(RoomMessageEventContent::text_plain("bob while detached 2"))
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match events.recv().await.expect("worker event") {
                    MatrixEvent::DetachedNew {
                        generation: 2,
                        count,
                    } if count >= 2 => {
                        assert_eq!(count, 2);
                        return;
                    }
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for DetachedNew count == 2");

        let _ = created.leave().await;
        let _ = alice_setup.matrix_auth().logout().await;
        let _ = bob.matrix_auth().logout().await;
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = std::fs::remove_dir_all(root);
    }
}
