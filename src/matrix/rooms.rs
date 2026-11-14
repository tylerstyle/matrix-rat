//! Per-room work: timelines, sending, reactions, receipts, invites.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

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
    RoomExt, Timeline, TimelineDetails, TimelineEventItemId, TimelineFocus, TimelineItem,
    TimelineReadReceiptTracking,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use super::convert::convert;
use super::{MatrixCmd, MatrixEvent, drafts, threads};
use crate::format::Outgoing;
use crate::model::ItemRef;

/// Recently used timelines stay alive so going back to a room is instant and keeps the
/// history that was already loaded.
const KEEP_TIMELINES: usize = 6;
const PAGE: u16 = 40;

pub struct Timelines {
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    cache: VecDeque<(String, Arc<Timeline>)>,
    /// Forwards the open room's timeline to the UI.
    forwarder: Option<JoinHandle<()>>,
    /// The running request for older messages, if any.
    paginator: Option<JoinHandle<()>>,
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
    unread_flags: UnreadFlags,
}

impl Timelines {
    pub fn new(client: Client, events: UnboundedSender<MatrixEvent>) -> Self {
        Self {
            media: super::media::Media::new(client.clone(), events.clone()),
            member_fetches: tokio::task::JoinSet::new(),
            uploads: super::upload::Uploads::new(client.clone(), events.clone()),
            client,
            cache: VecDeque::new(),
            forwarder: None,
            paginator: None,
            thread: None,
            read_marks: Default::default(),
            drafts_told: HashSet::new(),
            unread_flags: UnreadFlags::start(events.clone()),
            events,
        }
    }

    pub fn close(&mut self) {
        for task in [self.forwarder.take(), self.paginator.take()]
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
        let timeline = Arc::new(room_timeline(&room).await?);
        // Sync only delivers the members it thinks are needed, which leaves many
        // senders (bridged users above all) without a name. Fetch the member list once;
        // the timeline then fills in the names of what is already shown.
        // Skipped when the list is already complete, and stopped with the session.
        if !room.are_members_synced() {
            let members = timeline.clone();
            while self.member_fetches.try_join_next().is_some() {}
            self.member_fetches
                .spawn(async move { members.fetch_members().await });
        }
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

    /// Tells the UI the stored draft of a room (or of the thread a command is being
    /// handled for) when it is opened for the first time. After that the UI has the
    /// newer text, so it is told only once.
    async fn tell_draft(&mut self, room: &Room, room_id: &str) {
        let thread = self.thread.clone();
        if !self
            .drafts_told
            .insert(threads::key(room_id, thread.as_deref()))
        {
            return;
        }
        match drafts::load(room, thread.as_deref()).await {
            Ok(text) => {
                let _ = self.events.send(MatrixEvent::Draft {
                    room_id: room_id.to_owned(),
                    thread,
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

    /// Looks up the room's timeline, then runs `work` with it in the background.
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
                        )));
                    }
                    Err(err) => self.fail("Could not open the room", format!("{err:#}")),
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
                match self.timeline(&room_id).await {
                    Ok(timeline) => {
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
                self.with_timeline(&room_id, "Could not edit", |timeline| async move {
                    let content = EditedContent::RoomMessage(content_without_relation(&message)?);
                    timeline.edit(&item_id(&item)?, content).await?;
                    Ok(())
                })
                .await;
            }
            MatrixCmd::ToggleReaction { room_id, item, key } => {
                self.with_timeline(&room_id, "Could not react", |timeline| async move {
                    timeline.toggle_reaction(&item_id(&item)?, &key).await?;
                    Ok(())
                })
                .await;
            }
            MatrixCmd::Redact { room_id, item } => {
                self.with_timeline(&room_id, "Could not delete", |timeline| async move {
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
                                tracing::debug!(?err, "could not mark the room as read");
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
                            Err(err) => tracing::debug!(?err, "could not send read receipt"),
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
                            .find(|(id, _)| id == room_id)
                            .map(|(_, timeline)| timeline.clone());
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
                            tracing::debug!(?err, "could not mark a room as read");
                            failed += 1;
                        }
                    }
                    if failed > 0 {
                        bail!("{failed} rooms could not be marked");
                    }
                    Ok(())
                });
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
    let receipt = timeline.mark_as_read(receipt).await;
    let marker = timeline.mark_as_read(ReceiptType::FullyRead).await;
    receipt?;
    marker?;
    Ok(())
}

/// Sets and takes back "marked as unread" (`m.marked_unread` in a room's account
/// data). One request after the other, in the order they were asked for: marking a
/// room and opening it right away must not reach the server the other way round.
///
/// What the SDK knows of the flag is what the last sync said, which lags behind
/// what was sent from here a moment ago. So what was last sent counts until a sync
/// has confirmed it (or for a while at most), and a request is only skipped when
/// the room is known to be that way already.
#[derive(Clone)]
struct UnreadFlags(UnboundedSender<(Room, bool)>);

impl UnreadFlags {
    /// For this long at most what we sent is taken to be the state, whatever the
    /// last sync said.
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
                // The sync has caught up with what we sent: from here on it is
                // the one that knows (another client may change the flag too).
                if sent
                    .get(room.room_id())
                    .is_some_and(|(ours, _)| *ours == synced)
                {
                    sent.remove(room.room_id());
                }
                let now = match sent.get(room.room_id()) {
                    Some((ours, _)) => *ours,
                    None => synced,
                };
                if now == unread {
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
                        tracing::warn!(%err, "could not change the unread mark");
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
}

/// Sends the whole timeline to the UI whenever it changes, and who is typing.
/// `thread`: the timeline is not the room's but that of the thread starting at this event.
async fn forward(
    room: Room,
    timeline: Arc<Timeline>,
    room_id: String,
    generation: u64,
    events: UnboundedSender<MatrixEvent>,
    thread: Option<String>,
    read_marks: threads::ReadMarks,
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
            reached_start: match &thread {
                // A thread's timeline has no start marker. Its first message is the
                // last thing loaded, when there is nothing older left.
                Some(root) => items
                    .iter()
                    .filter_map(|item| item.as_event()?.event_id())
                    .any(|id| id == root.as_str()),
                None => items.iter().any(|item| item.is_timeline_start()),
            },
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

        use crate::matrix::{MatrixEvent, RoomCategory, spawn};
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
            password,
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
                                    summary.category != RoomCategory::Invite
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
            password,
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
                            summary.category != RoomCategory::Invite
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
}
