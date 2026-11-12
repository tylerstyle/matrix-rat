//! Per-room work: timelines, sending, reactions, receipts, invites.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use futures::{StreamExt, pin_mut};
use matrix_sdk::room::edit::EditedContent;
use matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType;
use matrix_sdk::ruma::events::room::message::{
    RoomMessageEventContent, RoomMessageEventContentWithoutRelation,
};
use matrix_sdk::ruma::{EventId, OwnedEventId, OwnedTransactionId, RoomId};
use matrix_sdk::{Client, Room};
use matrix_sdk_ui::eyeball_im::Vector;
use matrix_sdk_ui::timeline::{
    RoomExt, Timeline, TimelineDetails, TimelineEventItemId, TimelineFocus, TimelineItem,
    TimelineReadReceiptTracking,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use super::convert::convert;
use super::{MatrixCmd, MatrixEvent};
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
}

impl Timelines {
    pub fn new(client: Client, events: UnboundedSender<MatrixEvent>) -> Self {
        Self {
            client,
            events,
            cache: VecDeque::new(),
            forwarder: None,
            paginator: None,
        }
    }

    pub fn close(&mut self) {
        for task in [self.forwarder.take(), self.paginator.take()]
            .into_iter()
            .flatten()
        {
            task.abort();
        }
    }

    fn room(&self, room_id: &str) -> Result<Room> {
        let id = RoomId::parse(room_id).map_err(|_| anyhow!("invalid room id"))?;
        self.client
            .get_room(&id)
            .ok_or_else(|| anyhow!("this room is not known yet"))
    }

    async fn timeline(&mut self, room_id: &str) -> Result<Arc<Timeline>> {
        if let Some(at) = self.cache.iter().position(|(id, _)| id == room_id) {
            // Move to the back: most recently used.
            let entry = self.cache.remove(at).expect("position is valid");
            let timeline = entry.1.clone();
            self.cache.push_back(entry);
            return Ok(timeline);
        }
        let room = self.room(room_id)?;
        let timeline = room
            .timeline_builder()
            .with_focus(TimelineFocus::Live {
                hide_threaded_events: false,
            })
            .track_read_marker_and_receipts(TimelineReadReceiptTracking::MessageLikeEvents)
            .build()
            .await
            .context("could not open the room's messages")?;
        let timeline = Arc::new(timeline);
        self.cache.push_back((room_id.to_owned(), timeline.clone()));
        while self.cache.len() > KEEP_TIMELINES {
            self.cache.pop_front();
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
            MatrixCmd::MarkRead { room_id } => {
                if let Ok(timeline) = self.timeline(&room_id).await {
                    tokio::spawn(async move {
                        if let Err(err) = timeline.mark_as_read(ReceiptType::Read).await {
                            tracing::debug!(?err, "could not send read receipt");
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
            // Handled by the session loop.
            MatrixCmd::LoginPassword { .. }
            | MatrixCmd::LoginBrowser { .. }
            | MatrixCmd::CancelLogin
            | MatrixCmd::Recover { .. }
            | MatrixCmd::Shutdown => {}
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

fn content(message: &Outgoing) -> Result<RoomMessageEventContent> {
    Ok(match message {
        Outgoing::Text { plain, html: None } => RoomMessageEventContent::text_plain(plain),
        Outgoing::Text {
            plain,
            html: Some(html),
        } => RoomMessageEventContent::text_html(plain, html),
        Outgoing::Emote { plain, html: None } => RoomMessageEventContent::emote_plain(plain),
        Outgoing::Emote {
            plain,
            html: Some(html),
        } => RoomMessageEventContent::emote_html(plain, html),
        Outgoing::Rejected(reason) => bail!("{reason}"),
    })
}

fn content_without_relation(message: &Outgoing) -> Result<RoomMessageEventContentWithoutRelation> {
    Ok(content(message)?.into())
}

/// Sends the whole timeline to the UI whenever it changes, and who is typing.
async fn forward(
    room: Room,
    timeline: Arc<Timeline>,
    room_id: String,
    generation: u64,
    events: UnboundedSender<MatrixEvent>,
) {
    let own = room.own_user_id().to_owned();
    let (mut items, changes) = timeline.subscribe().await;
    pin_mut!(changes);
    let (_typing_guard, mut typing) = room.subscribe_to_typing_notifications();
    let snapshot = |items: &Vector<Arc<TimelineItem>>| MatrixEvent::Timeline {
        room_id: room_id.clone(),
        generation,
        messages: convert(items, &own),
        reached_start: items.iter().any(|item| item.is_timeline_start()),
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
    if events.send(snapshot(&items)).is_err() {
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
                if events.send(snapshot(&items)).is_err() {
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
