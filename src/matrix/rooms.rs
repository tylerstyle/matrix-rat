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
use super::{MatrixCmd, MatrixEvent, threads};
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
}

impl Timelines {
    pub fn new(client: Client, events: UnboundedSender<MatrixEvent>) -> Self {
        Self {
            media: super::media::Media::new(client.clone(), events.clone()),
            member_fetches: tokio::task::JoinSet::new(),
            uploads: super::upload::Uploads::new(client.clone(), events.clone()),
            client,
            events,
            cache: VecDeque::new(),
            forwarder: None,
            paginator: None,
            thread: None,
            read_marks: Default::default(),
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
                    Err(err) => self.fail("Could not load older messages", format!("{err:#}")),
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
            // Handled by the session loop.
            MatrixCmd::LoginPassword { .. }
            | MatrixCmd::LoginBrowser { .. }
            | MatrixCmd::CancelLogin
            | MatrixCmd::Recover { .. }
            | MatrixCmd::Logout
            | MatrixCmd::Shutdown
            | MatrixCmd::Verification(_)
            | MatrixCmd::Manage(_) => {}
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
    let snapshot = |items: &Vector<Arc<TimelineItem>>, unread: HashSet<String>| {
        let mut messages = convert(items, &own);
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
    if events.send(snapshot(&items, marks)).is_err() {
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
                if events.send(snapshot(&items, marks)).is_err() {
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
            &[MentionMark::User {
                label: bob_member.display_name,
                user_id: bob_member.user_id,
            }],
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
}
