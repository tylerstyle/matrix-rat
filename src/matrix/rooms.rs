//! Per-room work: timelines, sending, reactions, receipts, invites.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use futures::{StreamExt, pin_mut};
use matrix_sdk::room::edit::EditedContent;
use matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType;
use matrix_sdk::ruma::events::Mentions;
use matrix_sdk::ruma::events::room::message::{
    RoomMessageEventContent, RoomMessageEventContentWithoutRelation,
};
use matrix_sdk::ruma::{EventId, OwnedEventId, OwnedTransactionId, RoomId};
use matrix_sdk::{Client, Room, RoomMemberships};
use matrix_sdk_ui::eyeball_im::Vector;
use matrix_sdk_ui::timeline::{
    MsgLikeKind, RoomExt, Timeline, TimelineDetails, TimelineEventFocusThreadMode,
    TimelineEventItemId, TimelineFocus, TimelineItem, TimelineReadReceiptTracking,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use super::convert::convert;
use super::{DetachedTimeline, MatrixCmd, MatrixEvent, drafts, threads};
use crate::format::Outgoing;
use crate::model::ItemRef;

/// Recently used timelines stay alive so going back to a room is instant and keeps the
/// history that was already loaded.
const KEEP_TIMELINES: usize = 6;
const PAGE: u16 = 40;

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

struct DetachedSlot {
    room_id: String,
    thread: Option<String>,
    generation: u64,
    timeline: Arc<Timeline>,
    reached_start: Arc<AtomicBool>,
    reached_end: Arc<AtomicBool>,
}

struct DetachedForward {
    target_event: String,
    reached_start: Arc<AtomicBool>,
    reached_end: Arc<AtomicBool>,
}

pub struct Timelines {
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    cache: VecDeque<(String, Arc<Timeline>)>,
    /// Forwards the open room's timeline to the UI.
    forwarder: Option<JoinHandle<()>>,
    /// The running request for older messages, if any.
    paginator: Option<JoinHandle<()>>,
    /// The running request for newer messages on a detached timeline, if any.
    forward_paginator: Option<JoinHandle<()>>,
    /// Active event-focused timeline when viewing older history via `/context`.
    detached: Option<DetachedSlot>,
    /// Background `/context` request for `OpenEvent`, so slow servers never block sync.
    event_opener: Option<JoinHandle<()>>,
    pending_open_event_gen: Option<u64>,
    open_tx: UnboundedSender<OpenedTimeline>,
    open_rx: UnboundedReceiver<OpenedTimeline>,
    media: super::media::Media,
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
}

impl Timelines {
    pub fn new(client: Client, events: UnboundedSender<MatrixEvent>) -> Self {
        let (open_tx, open_rx) = unbounded_channel();
        Self {
            media: super::media::Media::new(client.clone(), events.clone()),
            member_fetches: tokio::task::JoinSet::new(),
            uploads: super::upload::Uploads::new(client.clone(), events.clone()),
            client,
            events,
            cache: VecDeque::new(),
            forwarder: None,
            paginator: None,
            forward_paginator: None,
            detached: None,
            event_opener: None,
            pending_open_event_gen: None,
            open_tx,
            open_rx,
            thread: None,
            read_marks: Default::default(),
            drafts_told: HashSet::new(),
        }
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
        self.detached = None;
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
        self.member_fetches.abort_all();
    }

    fn room(&self, room_id: &str) -> Result<Room> {
        let id = RoomId::parse(room_id).map_err(|_| anyhow!("invalid room id"))?;
        self.client
            .get_room(&id)
            .ok_or_else(|| anyhow!("this room is not known yet"))
    }

    async fn timeline(&mut self, room_id: &str) -> Result<Arc<Timeline>> {
        let key = threads::key(room_id, self.thread.as_deref());
        if let Some(at) = self.cache.iter().position(|(id, _)| *id == key) {
            // Move to the back: most recently used.
            let entry = self.cache.remove(at).expect("position is valid");
            let timeline = entry.1.clone();
            self.cache.push_back(entry);
            return Ok(timeline);
        }
        let room = self.room(room_id)?;
        if let Some(root) = &self.thread {
            let timeline = Arc::new(threads::open(&room, root).await?);
            // One thread at a time stays alive; rooms keep their places.
            self.cache.retain(|(id, _)| !threads::is_thread(id));
            self.cache.push_back((key, timeline.clone()));
            return Ok(timeline);
        }
        let timeline = room
            .timeline_builder()
            .with_focus(TimelineFocus::Live {
                // Replies in a thread are shown in the thread (see `threads`).
                hide_threaded_events: true,
            })
            .track_read_marker_and_receipts(TimelineReadReceiptTracking::MessageLikeEvents)
            .build()
            .await
            .context("could not open the room's messages")?;
        let timeline = Arc::new(timeline);
        // Sync only delivers the members it thinks are needed, which leaves many
        // senders (bridged users above all) without a name. Fetch the member list once;
        // the timeline then fills in the names of what is already shown.
        // Skipped when the list is already complete, and stopped with the session.
        self.spawn_members_if_needed(&room, &timeline);
        self.cache.push_back((key, timeline.clone()));
        // The open thread's timeline does not take a room's place.
        let rooms = |cache: &VecDeque<(String, Arc<Timeline>)>| {
            cache
                .iter()
                .filter(|(id, _)| !threads::is_thread(id))
                .count()
        };
        while rooms(&self.cache) > KEEP_TIMELINES {
            let oldest = self
                .cache
                .iter()
                .position(|(id, _)| !threads::is_thread(id));
            self.cache.remove(oldest.expect("there are rooms"));
        }
        Ok(timeline)
    }

    fn spawn_members_if_needed(&mut self, room: &Room, timeline: &Arc<Timeline>) {
        if !room.are_members_synced() {
            let members = timeline.clone();
            while self.member_fetches.try_join_next().is_some() {}
            self.member_fetches
                .spawn(async move { members.fetch_members().await });
        }
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
            Err(err) => tracing::warn!(?err, "could not load the draft"),
        }
    }

    fn fail(&self, what: &str, err: impl std::fmt::Display) {
        tracing::warn!(%err, "{what}");
        let _ = self
            .events
            .send(MatrixEvent::ActionFailed(format!("{what}: {err}")));
    }

    /// Runs `work` in the background and reports a failure to the user.
    fn spawn<F>(&self, what: &'static str, work: F)
    where
        F: Future<Output = Result<()>> + Send + 'static,
    {
        let events = self.events.clone();
        tokio::spawn(async move {
            if let Err(err) = work.await {
                tracing::warn!(?err, "{what}");
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
            Err(err) => self.fail(what, format!("{err:#}")),
        }
    }

    /// Returns the detached timeline when viewing it for `(room_id, self.thread)`,
    /// otherwise the live timeline, plus its `reached_start` flag if detached.
    async fn active_timeline(
        &mut self,
        room_id: &str,
    ) -> Result<(Arc<Timeline>, Option<Arc<AtomicBool>>)> {
        if let Some(detached) = &self.detached
            && detached.room_id == room_id
            && detached.thread == self.thread
        {
            return Ok((
                detached.timeline.clone(),
                Some(detached.reached_start.clone()),
            ));
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
            Err(err) => self.fail(what, format!("{err:#}")),
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
        if let Some(at) = self.cache.iter().position(|(id, _)| *id == cache_key) {
            let cached = self.cache[at].1.clone();
            if let Some(item) = cached.item_by_event_id(&parsed_event).await {
                if item.content().is_redacted() {
                    return jump_failed("That message was deleted.".to_owned());
                }
                let entry = self.cache.remove(at).expect("position is valid");
                let timeline = entry.1.clone();
                self.cache.push_back(entry);
                self.close();
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
                )));
                return;
            }
        }

        self.pending_open_event_gen = Some(generation);
        let open_tx = self.open_tx.clone();
        self.event_opener = Some(tokio::spawn(async move {
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
                self.close();
                let reached_start = Arc::new(AtomicBool::new(false));
                let reached_end = Arc::new(AtomicBool::new(false));
                self.detached = Some(DetachedSlot {
                    room_id: room_id.clone(),
                    thread: thread.clone(),
                    generation,
                    timeline: timeline.clone(),
                    reached_start: reached_start.clone(),
                    reached_end: reached_end.clone(),
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
                    }),
                )));
            }
        }
    }

    pub async fn handle(&mut self, command: MatrixCmd) {
        match command {
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
                        self.forwarder = Some(tokio::spawn(forward(
                            room,
                            timeline,
                            room_id,
                            generation,
                            self.events.clone(),
                            self.thread.clone(),
                            self.read_marks.clone(),
                            None,
                        )));
                    }
                    Err(err) => self.fail("Could not open the room", format!("{err:#}")),
                }
            }
            MatrixCmd::OpenEvent {
                room_id,
                event_id,
                thread_root,
                generation,
            } => {
                self.open_event(room_id, event_id, thread_root, generation)
                    .await;
            }
            MatrixCmd::CancelOpenEvent { generation } => {
                if self.pending_open_event_gen == Some(generation) {
                    self.cancel_open_event();
                }
            }
            MatrixCmd::Paginate {
                room_id,
                generation,
            } => {
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
                        let events = self.events.clone();
                        self.paginator = Some(tokio::spawn(async move {
                            let reached_start = match timeline.paginate_backwards(PAGE).await {
                                Ok(reached_start) => reached_start,
                                Err(err) => {
                                    tracing::warn!(?err, "could not load older messages");
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
                    Err(err) => {
                        self.fail("Could not load older messages", format!("{err:#}"));
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
                    .forward_paginator
                    .as_ref()
                    .is_some_and(|task| !task.is_finished())
                {
                    return;
                }
                let Some(detached) = &self.detached else {
                    let _ = self.events.send(MatrixEvent::PaginatedForwards {
                        generation,
                        reached_end: true,
                    });
                    return;
                };
                if detached.room_id != room_id
                    || detached.thread != self.thread
                    || detached.generation != generation
                {
                    let _ = self.events.send(MatrixEvent::PaginatedForwards {
                        generation,
                        reached_end: true,
                    });
                    return;
                }
                let timeline = detached.timeline.clone();
                let reached_end_flag = detached.reached_end.clone();
                let events = self.events.clone();
                self.forward_paginator = Some(tokio::spawn(async move {
                    let reached_end = match timeline.paginate_forwards(PAGE).await {
                        Ok(reached_end) => reached_end,
                        Err(err) => {
                            tracing::warn!(?err, "could not load newer messages");
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
            MatrixCmd::ToggleReaction { room_id, item, key } => {
                self.with_active_timeline(&room_id, "Could not react", |timeline| async move {
                    timeline.toggle_reaction(&item_id(&item)?, &key).await?;
                    Ok(())
                })
                .await;
            }
            MatrixCmd::Redact { room_id, item } => {
                self.with_active_timeline(&room_id, "Could not delete", |timeline| async move {
                    timeline.redact(&item_id(&item)?, None).await?;
                    Ok(())
                })
                .await;
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
            MatrixCmd::Typing { room_id, typing } => {
                if let Ok(room) = self.room(&room_id) {
                    tokio::spawn(async move {
                        if let Err(err) = room.typing_notice(typing).await {
                            tracing::debug!(?err, "could not send typing notice");
                        }
                    });
                }
            }
            MatrixCmd::MarkRead { room_id, public } => {
                let receipt = if public {
                    ReceiptType::Read
                } else {
                    ReceiptType::ReadPrivate
                };
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
                    tokio::spawn(async move {
                        match timeline.mark_as_read(receipt).await {
                            // Also when nothing had to be sent: it was read already.
                            Ok(_) => {
                                if let Some((key, ticket, newest)) = mark
                                    && !newest.is_empty()
                                {
                                    marks.set(key, ticket, newest);
                                }
                            }
                            Err(err) => tracing::debug!(?err, "could not send read receipt"),
                        }
                    });
                }
            }
            MatrixCmd::Join { room_id } => match self.room(&room_id) {
                Ok(room) => self.spawn("Could not join", async move {
                    room.join().await?;
                    Ok(())
                }),
                Err(err) => self.fail("Could not join", err),
            },
            MatrixCmd::Leave { room_id } => match self.room(&room_id) {
                Ok(room) => self.spawn("Could not leave", async move {
                    room.leave().await?;
                    Ok(())
                }),
                Err(err) => self.fail("Could not leave", err),
            },
            MatrixCmd::FetchAvatar { url } => self.media.fetch_avatar(url),
            MatrixCmd::FetchImage {
                key,
                media,
                size,
                full,
            } => self.media.fetch(key, *media, size, full),
            MatrixCmd::SaveFile { file, dir, open } => self.media.save(file, dir, open),
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
                room_id,
                files,
                caption,
                reply_to,
            } => {
                let reply_to = match reply_to.map(EventId::parse).transpose() {
                    Ok(reply_to) => reply_to,
                    Err(_) => return self.fail("Could not send", "invalid event id"),
                };
                match self.timeline(&room_id).await {
                    Ok(timeline) => self.uploads.send(timeline, files, caption, reply_to),
                    Err(err) => self.fail("Could not send", format!("{err:#}")),
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
                    tracing::warn!(?err, "could not save the draft");
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
                        matrix_sdk::RoomState::Joined | matrix_sdk::RoomState::Invited
                    )
                });
                if !still_in
                    && let Err(err) = drafts::forget(&self.client, &room_id, &threads).await
                {
                    tracing::warn!(?err, "could not remove the drafts of a room");
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
            | MatrixCmd::CancelSearch => {}
        }
    }
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
            tracing::warn!(?err, "could not load room members");
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

fn content(message: &Outgoing) -> Result<RoomMessageEventContent> {
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

fn friendly_jump_error(err: &matrix_sdk_ui::timeline::Error) -> String {
    let msg = format!("{err:#}");
    if msg.contains("404") || msg.contains("M_NOT_FOUND") {
        "Message could not be found.".to_owned()
    } else if msg.contains("403") || msg.contains("M_FORBIDDEN") {
        "You do not have permission to view that message.".to_owned()
    } else {
        format!("Could not open the message: {msg}")
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
) {
    let in_thread = thread.is_some();
    let own = room.own_user_id().to_owned();
    let (mut items, changes) = timeline.subscribe().await;
    pin_mut!(changes);
    let (_typing_guard, mut typing) = room.subscribe_to_typing_notifications();
    // Who has read up to where, by name.
    let mut readers = super::receipts::Names::default();
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
                None => match &thread {
                    // A thread's timeline has no start marker. Its first message is the
                    // last thing loaded, when there is nothing older left.
                    Some(root) => items
                        .iter()
                        .filter_map(|item| item.as_event()?.event_id())
                        .any(|id| id == root.as_str()),
                    None => items.iter().any(|item| item.is_timeline_start()),
                },
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
                        tracing::debug!(?err, "could not fetch the replied-to message");
                    }
                });
            }
        }
    };
    fetch_replies(&items);
    let marks = unread(&items).await;
    let mut messages = convert(&items, &own);
    readers.annotate(&room, &mut messages, &items, &own).await;
    if events.send(snapshot(&items, marks, messages)).is_err() {
        return;
    }
    loop {
        tokio::select! {
            diffs = changes.next() => {
                let Some(diffs) = diffs else { return };
                for diff in diffs {
                    diff.apply(&mut items);
                }
                fetch_replies(&items);
                let marks = unread(&items).await;
                let mut messages = convert(&items, &own);
                readers.annotate(&room, &mut messages, &items, &own).await;
                if events.send(snapshot(&items, marks, messages)).is_err() {
                    return;
                }
            }
            users = typing.recv() => {
                let names = match users {
                    Ok(users) => users
                        .iter()
                        .map(|u| crate::text::sanitize_line(u.localpart()))
                        .collect(),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                if events.send(MatrixEvent::Typing { generation, names }).is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use matrix_sdk::config::SyncSettings;

    use crate::complete::MentionMark;
    use crate::format::parse_with_mentions;
    use crate::matrix::{MatrixCmd, MatrixEvent, RoomCategory, spawn};
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
            password: "alicepass".to_owned(),
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
                        for listed in rooms
                            .into_iter()
                            .filter(|r| r.category != RoomCategory::Invite)
                        {
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

        // Alice opens the room and requests members; Bob should appear in RoomMembers.
        handle.send(MatrixCmd::OpenRoom {
            room_id: room_id.clone(),
            generation: 1,
        });
        handle.send(MatrixCmd::FetchMembers {
            room_id: room_id.clone(),
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
            password,
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
}
