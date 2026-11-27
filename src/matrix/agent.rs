//! The worker's side of the agent channel (`matrix-rat serve`): sending a text,
//! reading the latest messages of a room, and watching rooms for news.
//!
//! It keeps apart from [`super::rooms::Timelines`], which serves the one room the app
//! has open: every read and every watched room gets a timeline of its own from the
//! SDK, on the same event cache and the same sync. Such a timeline is only looked at.
//! Nothing here sends a read receipt, moves the read marker, takes back a "marked as
//! unread" or says that someone is typing, so a room read through here stays unread
//! for the account. (A message that is *sent* counts as read up to itself on the
//! server; that is how Matrix counts, and no receipt of ours.) Nothing here joins,
//! leaves or creates a room either: a room the account is not in is refused.
//!
//! Each request runs in a task of its own with a time limit, and the number of
//! watched rooms and of running requests is bounded.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures::{FutureExt, Stream, StreamExt, pin_mut};
use matrix_sdk::deserialized_responses::{DeviceLinkProblem, VerificationLevel, VerificationState};
use matrix_sdk::ruma::events::room::message::{MessageType, RoomMessageEventContent};
use matrix_sdk::ruma::{EventId, RoomId, UserId};
use matrix_sdk::{Client, Room, RoomState};
use matrix_sdk_ui::eyeball_im::{Vector, VectorDiff};
use matrix_sdk_ui::timeline::{
    MsgLikeKind, RoomExt, Timeline, TimelineFocus, TimelineItem, TimelineItemContent,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

use super::MatrixEvent;
use super::paging::{self, PageError};
use super::safety::IgnoredSet;
use crate::agent::proto::{self, Msg};
use crate::logsafe;
use crate::model::Shield;

/// The newest messages of a watched room that are told on every change.
pub const WATCH_TAIL: usize = 100;
/// How far a watch goes back for a first message of a room that shows none, and for
/// how long (well within the time a `listen` waits to be let in).
const WATCH_LOAD_PAGES: usize = 3;
const WATCH_LOAD_BUDGET: Duration = Duration::from_secs(10);
/// Rooms watched at once.
pub const MAX_WATCHED: usize = 16;
/// Sends and reads running at once.
const MAX_TASKS: usize = 64;
/// How long the server may take to accept a message.
const SEND_TIMEOUT: Duration = Duration::from_secs(60);
/// How long opening a room's messages may take.
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
/// Events asked for at a time when a read needs older messages.
const PAGE: u16 = 40;
/// How often a read asks for older messages at most, how long one such request may
/// take, and how long all of them together.
const READ_PAGES: usize = 8;
const PAGE_TIMEOUT: Duration = Duration::from_secs(20);
const READ_BUDGET: Duration = Duration::from_secs(60);
/// How long a read waits for the messages it asked for to show in the timeline.
const SETTLE: Duration = Duration::from_secs(2);

// ----- what `serve` sees ----------------------------------------------------------------

pub enum AgentCmd {
    /// Send a text to a joined room, without the send queue: the answer
    /// ([`AgentEvent::Sent`]) says whether the server has it.
    Send {
        request: u64,
        room_id: String,
        /// Sent with the composer's Markdown and none of its slash commands.
        text: String,
    },
    /// The latest `limit` messages of a joined room, oldest first; answered with
    /// [`AgentEvent::Read`].
    Read {
        request: u64,
        room_id: String,
        limit: usize,
    },
    /// Tell the newest messages of a joined room now and on every change
    /// ([`AgentEvent::Watched`]), until [`AgentCmd::Unwatch`] or
    /// [`AgentEvent::WatchEnded`].
    Watch {
        room_id: String,
    },
    Unwatch {
        room_id: String,
    },
}

/// Never prints a room or a text.
impl std::fmt::Debug for AgentCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.shown(false))
    }
}

impl AgentCmd {
    /// `ids`: with the room's id, which only tests may ask for.
    pub(super) fn shown(&self, ids: bool) -> String {
        let id = |id: &str| ids.then(|| id.to_owned());
        let request = |request: &u64| Some(format!("request {request}"));
        let (name, parts) = match self {
            Self::Send {
                request: r,
                room_id,
                ..
            } => ("Send", vec![request(r), id(room_id)]),
            Self::Read {
                request: r,
                room_id,
                limit,
            } => (
                "Read",
                vec![request(r), id(room_id), Some(format!("limit {limit}"))],
            ),
            Self::Watch { room_id } => ("Watch", vec![id(room_id)]),
            Self::Unwatch { room_id } => ("Unwatch", vec![id(room_id)]),
        };
        logsafe::shape(name, parts)
    }
}

pub enum AgentEvent {
    /// Outcome of [`AgentCmd::Send`]: the id of the event, or a reason for the caller.
    Sent {
        request: u64,
        result: Result<String, String>,
    },
    /// Outcome of [`AgentCmd::Read`].
    Read {
        request: u64,
        result: Result<Vec<Msg>, String>,
    },
    /// The newest messages of a watched room (at most [`WATCH_TAIL`]), oldest first:
    /// sent once when the watch begins and again whenever its timeline changes.
    Watched { room_id: String, messages: Vec<Msg> },
    /// The room is not watched (any more); the reason is for the caller.
    WatchEnded { room_id: String, reason: String },
}

/// Never prints a room, an event, a message or the reason of a failure.
impl std::fmt::Debug for AgentEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.shown(false))
    }
}

impl AgentEvent {
    /// `full`: with ids and reasons, which only tests may ask for.
    pub(super) fn shown(&self, full: bool) -> String {
        let text = |text: &str| full.then(|| text.to_owned());
        let request = |request: &u64| Some(format!("request {request}"));
        let (name, parts) = match self {
            Self::Sent { request: r, result } => (
                "Sent",
                vec![
                    request(r),
                    Some(format!("ok: {}", result.is_ok())),
                    match result {
                        Ok(event_id) => text(event_id),
                        Err(reason) => text(reason),
                    },
                ],
            ),
            Self::Read { request: r, result } => (
                "Read",
                vec![
                    request(r),
                    Some(match result {
                        Ok(messages) => format!("{} messages", messages.len()),
                        Err(_) => "ok: false".to_owned(),
                    }),
                    match result {
                        Ok(_) => None,
                        Err(reason) => text(reason),
                    },
                ],
            ),
            Self::Watched { room_id, messages } => (
                "Watched",
                vec![text(room_id), Some(format!("{} messages", messages.len()))],
            ),
            Self::WatchEnded { room_id, reason } => {
                ("WatchEnded", vec![text(room_id), text(reason)])
            }
        };
        logsafe::shape(name, parts)
    }
}

// ----- the worker side ------------------------------------------------------------------

/// Lives as long as a session.
pub struct Agent {
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    ignored: watch::Receiver<IgnoredSet>,
    left: super::left::LeftRooms,
    /// By room id: the task that tells a watched room's news.
    watchers: HashMap<String, JoinHandle<()>>,
    /// Sends and reads that are running.
    tasks: JoinSet<()>,
}

impl Agent {
    pub fn new(
        client: Client,
        events: UnboundedSender<MatrixEvent>,
        ignored: watch::Receiver<IgnoredSet>,
        left: super::left::LeftRooms,
    ) -> Self {
        Self {
            client,
            events,
            ignored,
            left,
            watchers: HashMap::new(),
            tasks: JoinSet::new(),
        }
    }

    /// Starts what `command` asks for and returns at once.
    pub fn handle(&mut self, command: AgentCmd) {
        while self.tasks.try_join_next().is_some() {}
        self.watchers.retain(|_, watcher| !watcher.is_finished());
        match command {
            AgentCmd::Send {
                request,
                room_id,
                text,
            } => self.send(request, &room_id, text),
            AgentCmd::Read {
                request,
                room_id,
                limit,
            } => self.read(request, &room_id, limit),
            AgentCmd::Watch { room_id } => self.watch(room_id),
            AgentCmd::Unwatch { room_id } => {
                if let Some(watcher) = self.watchers.remove(&room_id) {
                    watcher.abort();
                }
            }
        }
    }

    pub fn stop(&mut self) {
        self.tasks.abort_all();
        for (_, watcher) in self.watchers.drain() {
            watcher.abort();
        }
    }

    fn emit(&self, event: AgentEvent) {
        let _ = self.events.send(MatrixEvent::Agent(event));
    }

    /// The room a request is for: one the account is in, and nothing else.
    fn room(&self, room_id: &str) -> Result<Room> {
        joined_room(&self.client, &self.left, room_id)
    }

    /// The room, unless as much is running already as may run at once.
    fn room_for_task(&self, room_id: &str) -> Result<Room> {
        if self.tasks.len() >= MAX_TASKS {
            bail!("too many requests at once");
        }
        self.room(room_id)
    }

    fn send(&mut self, request: u64, room_id: &str, text: String) {
        let room = match self.room_for_task(room_id) {
            Ok(room) => room,
            Err(err) => {
                return self.emit(AgentEvent::Sent {
                    request,
                    result: Err(format!("{err:#}")),
                });
            }
        };
        let events = self.events.clone();
        self.tasks.spawn(async move {
            let result = send_text(&room, &text).await;
            if let Err(err) = &result {
                tracing::warn!(why = %logsafe::kind_any(err), "agent: could not send");
            }
            let _ = events.send(MatrixEvent::Agent(AgentEvent::Sent {
                request,
                result: result.map_err(|err| format!("{err:#}")),
            }));
        });
    }

    fn read(&mut self, request: u64, room_id: &str, limit: usize) {
        let room = match self.room_for_task(room_id) {
            Ok(room) => room,
            Err(err) => {
                return self.emit(AgentEvent::Read {
                    request,
                    result: Err(format!("{err:#}")),
                });
            }
        };
        let events = self.events.clone();
        let ignored = self.ignored.clone();
        let limit = limit.clamp(1, proto::READ_MAX);
        self.tasks.spawn(async move {
            let result = read_latest(&room, limit, &ignored).await;
            if let Err(err) = &result {
                tracing::warn!(why = %logsafe::kind_any(err), "agent: could not read");
            }
            let _ = events.send(MatrixEvent::Agent(AgentEvent::Read {
                request,
                result: result.map_err(|err| format!("{err:#}")),
            }));
        });
    }

    fn watch(&mut self, room_id: String) {
        if self.watchers.contains_key(&room_id) {
            return;
        }
        let room = if self.watchers.len() >= MAX_WATCHED {
            Err(anyhow!("too many rooms are watched at once"))
        } else {
            self.room(&room_id)
        };
        let room = match room {
            Ok(room) => room,
            Err(err) => {
                return self.emit(AgentEvent::WatchEnded {
                    room_id,
                    reason: format!("{err:#}"),
                });
            }
        };
        let events = self.events.clone();
        let ignored = self.ignored.clone();
        let watcher = tokio::spawn(watch_room(room, room_id.clone(), events, ignored));
        self.watchers.insert(room_id, watcher);
    }
}

/// The room with this id, if the account is in it (and has not left it in this
/// session, whatever a late sync answer makes it look like).
pub(super) fn joined_room(
    client: &Client,
    left: &super::left::LeftRooms,
    room_id: &str,
) -> Result<Room> {
    let id = RoomId::parse(room_id).map_err(|_| anyhow!("invalid room id"))?;
    let room = client
        .get_room(&id)
        .ok_or_else(|| anyhow!("this room is not known"))?;
    if room.state() != RoomState::Joined || left.gone(&room) {
        bail!("the account is not in this room");
    }
    Ok(room)
}

/// A room's live timeline for looking at: with what was said in threads, and without
/// following the read marker (which nothing here moves).
async fn open(room: &Room) -> Result<Timeline> {
    let build = room
        .timeline_builder()
        .with_internal_id_prefix(super::next_timeline_prefix())
        .with_focus(TimelineFocus::Live {
            // A reply in a thread is an answer like any other.
            hide_threaded_events: false,
        })
        .build();
    tokio::time::timeout(OPEN_TIMEOUT, build)
        .await
        .map_err(|_| anyhow!("opening the room's messages timed out"))?
        .context("could not open the room's messages")
}

/// `text` as the message it is sent as: with the app's own Markdown, and without its
/// slash commands (a text that starts with a slash is a text).
fn content_of(text: &str) -> Result<RoomMessageEventContent> {
    super::rooms::content(&crate::format::message(text))
}

/// Sends `text` and waits for the server to accept it. Not through the send queue:
/// the caller wants to know now, not to have it tried again later.
async fn send_text(room: &Room, text: &str) -> Result<String> {
    let content = content_of(text)?;
    let sent = tokio::time::timeout(SEND_TIMEOUT, room.send(content))
        .await
        .map_err(|_| anyhow!("timed out; the message may or may not have been sent"))?
        .context("could not send")?;
    Ok(sent.response.event_id.to_string())
}

/// The latest `limit` messages of `room`, oldest first. Older ones are loaded (from
/// the store, then from the server) until there are enough, the room's start is
/// reached, or the budget of requests and of time is used up; what is there by then
/// is the answer.
async fn read_latest(
    room: &Room,
    limit: usize,
    ignored: &watch::Receiver<IgnoredSet>,
) -> Result<Vec<Msg>> {
    let timeline = open(room).await?;
    let own = room.own_user_id().to_owned();
    let (mut items, changes) = timeline.subscribe().await;
    pin_mut!(changes);
    let give_up = tokio::time::Instant::now() + READ_BUDGET;
    let mut pages = 0;
    let mut done = false;
    loop {
        // What the timeline has worked in meanwhile, without waiting for more.
        while let Some(Some(diffs)) = changes.next().now_or_never() {
            for diff in diffs {
                diff.apply(&mut items);
            }
        }
        let found = messages(&items, &own, &ignored.borrow(), limit);
        let at_start = items.iter().any(|item| item.is_timeline_start());
        if done || at_start || found.len() >= limit || pages == READ_PAGES {
            return Ok(found);
        }
        pages += 1;
        let limit_at = give_up.min(tokio::time::Instant::now() + PAGE_TIMEOUT);
        match tokio::time::timeout_at(limit_at, paging::backwards(&timeline, PAGE)).await {
            Ok(Ok(reached_start)) => done = reached_start,
            // All caches were cleared after a panic: the next page is of them.
            Ok(Err(PageError::Panicked { cleared: true })) => {}
            Ok(Err(err)) => {
                tracing::debug!(why = %logsafe::kind(&err), "agent: could not load older messages");
                done = true;
            }
            Err(_) => done = true,
        }
        // What was loaded reaches the timeline a moment later.
        if let Ok(Some(diffs)) = tokio::time::timeout(SETTLE, changes.next()).await {
            for diff in diffs {
                diff.apply(&mut items);
            }
        }
    }
}

/// Tells the newest messages of `room` now and whenever its timeline changes.
async fn watch_room(
    room: Room,
    room_id: String,
    events: UnboundedSender<MatrixEvent>,
    mut ignored: watch::Receiver<IgnoredSet>,
) {
    let ended = |reason: &str| {
        let _ = events.send(MatrixEvent::Agent(AgentEvent::WatchEnded {
            room_id: room_id.clone(),
            reason: reason.to_owned(),
        }));
    };
    let timeline = match open(&room).await {
        Ok(timeline) => timeline,
        Err(err) => {
            tracing::warn!(why = %logsafe::kind_any(&err), "agent: could not watch a room");
            return ended(&format!("{err:#}"));
        }
    };
    let own = room.own_user_id().to_owned();
    let (mut items, changes) = timeline.subscribe().await;
    pin_mut!(changes);
    // A room whose messages are not loaded yet looks empty, and what is loaded later
    // would all look new: older ones are loaded first until a message is there (a
    // page may hold none: who joined, what was set), within a few pages and a short
    // time, so that what is told first is what the room holds.
    let give_up = tokio::time::Instant::now() + WATCH_LOAD_BUDGET;
    for _ in 0..WATCH_LOAD_PAGES {
        while let Some(Some(diffs)) = changes.next().now_or_never() {
            for diff in diffs {
                diff.apply(&mut items);
            }
        }
        if !messages(&items, &own, &ignored.borrow(), 1).is_empty()
            || items.iter().any(|item| item.is_timeline_start())
        {
            break;
        }
        let page = paging::backwards(&timeline, PAGE);
        let done = match tokio::time::timeout_at(give_up, page).await {
            Ok(Ok(reached_start)) => reached_start,
            // All caches were cleared after a panic: the next page is of them.
            Ok(Err(PageError::Panicked { cleared: true })) => false,
            Ok(Err(err)) => {
                tracing::debug!(why = %logsafe::kind(&err), "agent: could not load a watched room's latest messages");
                break;
            }
            Err(_) => break,
        };
        // What was loaded reaches the timeline a moment later, also when it was
        // all the room has.
        if let Ok(Some(diffs)) = tokio::time::timeout(SETTLE, changes.next()).await {
            for diff in diffs {
                diff.apply(&mut items);
            }
        }
        if done {
            break;
        }
    }
    while let Some(Some(diffs)) = changes.next().now_or_never() {
        for diff in diffs {
            diff.apply(&mut items);
        }
    }
    let tell = |messages| {
        events
            .send(MatrixEvent::Agent(AgentEvent::Watched {
                room_id: room_id.clone(),
                messages,
            }))
            .is_ok()
    };
    if follow(items, changes, &own, &mut ignored, tell).await {
        ended("the room's messages are no longer followed");
    }
}

/// Tells (`tell`) the newest messages among `items` now, and again whenever
/// `changes` or the ignore list change them, until the changes end (`true`) or what
/// is told is no longer taken (`false`).
async fn follow(
    mut items: Vector<Arc<TimelineItem>>,
    mut changes: impl Stream<Item = Vec<VectorDiff<Arc<TimelineItem>>>> + Unpin,
    own: &UserId,
    ignored: &mut watch::Receiver<IgnoredSet>,
    mut tell: impl FnMut(Vec<Msg>) -> bool,
) -> bool {
    // What was told last. Most changes of a timeline change no message as it is
    // handed out (a read receipt, a date divider, an ignored user who said nothing
    // here): those are not told.
    let mut told: Option<Vec<Msg>> = None;
    // The changes ended right after a batch: that is told first.
    let mut ended = false;
    loop {
        let newest = messages(&items, own, &ignored.borrow(), WATCH_TAIL);
        if told.as_ref() != Some(&newest) {
            if !tell(newest.clone()) {
                return false;
            }
            told = Some(newest);
        }
        if ended {
            return true;
        }
        tokio::select! {
            diffs = changes.next() => {
                let Some(diffs) = diffs else {
                    return true;
                };
                for diff in diffs {
                    diff.apply(&mut items);
                }
                // What came meanwhile is taken in with it: one look for all of it.
                loop {
                    match changes.next().now_or_never() {
                        Some(Some(diffs)) => {
                            for diff in diffs {
                                diff.apply(&mut items);
                            }
                        }
                        Some(None) => {
                            ended = true;
                            break;
                        }
                        None => break,
                    }
                }
            }
            changed = ignored.changed() => {
                if changed.is_err() {
                    return false;
                }
            }
        }
    }
}

/// The newest `most` messages among `items`, oldest first, as they are handed out:
/// what people (and programs) said, not who joined or what was set. A message of
/// ours that the server has not confirmed has no id yet and is left out until it has
/// one. Deleted and unreadable messages are told as such. Everything is cleaned for a
/// terminal and bounded; a message whose own id or whose sender's id cannot be
/// handed out as it is, is left out (an id is never cut or changed). Each message is
/// told once, also when the timeline shows it twice (see [`events_once`]).
///
/// Only those are made: a timeline can hold far more (what a read loaded, a long
/// day), and this runs on every change of a watched room.
pub(super) fn messages(
    items: &Vector<Arc<TimelineItem>>,
    own: &UserId,
    ignored: &IgnoredSet,
    most: usize,
) -> Vec<Msg> {
    let now = chrono::Local::now();
    let mut left_out = 0usize;
    let mut messages: Vec<Msg> = events_once(items)
        .into_iter()
        .rev()
        .filter_map(|item| {
            let event = item.as_event()?;
            let event_id = event.event_id()?;
            let kind = kind(event.content())?;
            let (Some(event_id), Some(sender)) = (
                proto::clean_id(event_id.as_str()),
                proto::clean_id(event.sender().as_str()),
            ) else {
                left_out += 1;
                return None;
            };
            let encryption = event.encryption_info();
            let untrusted = strict_doubt(
                kind,
                encryption.map(|info| &info.verification_state),
                encryption.is_some_and(|info| info.forwarder.is_some()),
            );
            let message = super::convert::event_message(
                item.unique_id().0.clone(),
                event,
                own,
                ignored,
                &now,
            )?;
            let (body, truncated) = proto::bounded_body(message.body);
            Some(Msg {
                event_id,
                sender,
                sender_name: proto::clean_line(&message.sender, proto::MAX_NAME),
                timestamp_ms: message.sent_at.unwrap_or_default(),
                kind: kind.to_owned(),
                body,
                edited: message.edited,
                own: message.own,
                truncated,
                warning: message.shield.map(|shield| warning(shield).to_owned()),
                verified: untrusted.is_none(),
                untrusted: untrusted.map(str::to_owned),
            })
        })
        .take(most)
        .collect();
    messages.reverse();
    if left_out > 0 {
        tracing::debug!(
            left_out,
            "agent: messages with an id that cannot be handed out"
        );
    }
    messages
}

/// The events among `items`, each once: where its first copy stands, as its last copy
/// has it, unless that one is still locked and another was decrypted. When a batch of
/// events reaches a timeline while matrix-sdk-ui 0.19 builds it, the timeline can
/// show the batch twice, and what happens to those events later (an edit, a
/// deletion) reaches only the trailing copy, while a decryption reaches the copy the
/// SDK finds first; the app keeps them the same way (`convert::EventDedup`). Told
/// twice, the stale copy would pass for a message changed back, and the trailing one
/// for a message that is new.
fn events_once(items: &Vector<Arc<TimelineItem>>) -> Vec<&TimelineItem> {
    let mut events: Vec<&TimelineItem> = Vec::new();
    let mut at: HashMap<&EventId, usize> = HashMap::new();
    for item in items {
        let Some(event_id) = item.as_event().and_then(|event| event.event_id()) else {
            continue;
        };
        match at.entry(event_id) {
            Entry::Occupied(first) => {
                let kept = &mut events[*first.get()];
                // Decryption goes one way only, and the SDK decrypts whichever copy
                // it finds by position (a late key, keys from a file), often the
                // first: a copy still locked never stands in for one that was
                // decrypted.
                if !locked(item) || locked(kept) {
                    *kept = item;
                }
            }
            Entry::Vacant(first) => {
                first.insert(events.len());
                events.push(item);
            }
        }
    }
    events
}

/// Whether `item` is an event that could not be decrypted (yet).
fn locked(item: &TimelineItem) -> bool {
    item.as_event()
        .is_some_and(|event| event.content().is_unable_to_decrypt())
}

/// The strict judgement of who sent a message: why it cannot be taken as written by
/// the user it names, `None` if it can. It can only if it came encrypted, from a
/// device that its owner has cross-signed, of a user whose identity this account has
/// verified, with a key that came from that device itself. This is stricter than the
/// shield the app shows (`convert::shield`, which does not doubt a sender whom nobody
/// verified): it is for those who act on a certain user's word.
///
/// `state`: what the SDK found out when it decrypted the event, `None` for an event
/// that was not encrypted. `forwarded`: its key was passed on by someone else.
fn strict_doubt(
    kind: &str,
    state: Option<&VerificationState>,
    forwarded: bool,
) -> Option<&'static str> {
    match kind {
        // Nothing in them that anyone said.
        "undecryptable" => return Some("undecryptable"),
        "redacted" => return Some("redacted"),
        _ => {}
    }
    let Some(state) = state else {
        // A room without encryption, or sent in the clear in one that has it.
        return Some("not_encrypted");
    };
    match state {
        VerificationState::Verified => forwarded.then_some("unconfirmed"),
        VerificationState::Unverified(level) => Some(match level {
            VerificationLevel::UnverifiedIdentity => "unverified_identity",
            VerificationLevel::VerificationViolation => "identity_changed",
            VerificationLevel::UnsignedDevice
            | VerificationLevel::None(DeviceLinkProblem::MissingDevice) => "unverified_device",
            VerificationLevel::None(DeviceLinkProblem::InsecureSource) => "unconfirmed",
            VerificationLevel::MismatchedSender => "sender_mismatch",
        }),
    }
}

/// What kind of message this is; `None` for what is not a message.
fn kind(content: &TimelineItemContent) -> Option<&'static str> {
    let TimelineItemContent::MsgLike(content) = content else {
        return None;
    };
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "foreign matrix_sdk_ui MsgLikeKind and ruma MessageType"
    )]
    let label = match &content.kind {
        MsgLikeKind::Message(message) => match message.msgtype() {
            MessageType::Text(_) => "text",
            MessageType::Notice(_) | MessageType::ServerNotice(_) => "notice",
            MessageType::Emote(_) => "emote",
            MessageType::Image(_) => "image",
            MessageType::File(_) => "file",
            MessageType::Video(_) => "video",
            MessageType::Audio(audio) if audio.voice.is_some() => "voice",
            MessageType::Audio(_) => "audio",
            MessageType::Location(_) => "location",
            _ => "other",
        },
        MsgLikeKind::Sticker(_) => "sticker",
        MsgLikeKind::Poll(_) => "poll",
        MsgLikeKind::Redacted => "redacted",
        MsgLikeKind::UnableToDecrypt(_) => "undecryptable",
        _ => "other",
    };
    Some(label)
}

/// The word for a doubt about the sender.
fn warning(shield: Shield) -> &'static str {
    match shield {
        Shield::UnverifiedDevice => "unverified_device",
        Shield::IdentityChanged => "identity_changed",
        Shield::SenderMismatch => "sender_mismatch",
        Shield::NotEncrypted => "not_encrypted",
        Shield::Unconfirmed => "unconfirmed",
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use super::*;
    use crate::matrix::left::LeftRooms;
    use crate::matrix::left::testing::Server;

    fn agent_of(client: Client, left: &LeftRooms) -> (Agent, UnboundedReceiver<MatrixEvent>) {
        let (events, received) = unbounded_channel();
        let (_, ignored) = watch::channel(IgnoredSet::default());
        (Agent::new(client, events, ignored, left.clone()), received)
    }

    async fn next(events: &mut UnboundedReceiver<MatrixEvent>) -> AgentEvent {
        let event = tokio::time::timeout(Duration::from_secs(20), events.recv())
            .await
            .expect("an answer in time")
            .expect("the agent is alive");
        match event {
            MatrixEvent::Agent(event) => event,
            other => panic!("not an agent event: {other:?}"),
        }
    }

    #[test]
    fn commands_and_events_name_no_room_no_event_and_no_reason() {
        let commands = [
            AgentCmd::Send {
                request: 1,
                room_id: "!room:example.org".into(),
                text: "the launch codes".into(),
            },
            AgentCmd::Read {
                request: 2,
                room_id: "!room:example.org".into(),
                limit: 20,
            },
            AgentCmd::Watch {
                room_id: "!room:example.org".into(),
            },
            AgentCmd::Unwatch {
                room_id: "!room:example.org".into(),
            },
        ];
        let shown: Vec<String> = commands.iter().map(|c| format!("{c:?}")).collect();
        assert_eq!(
            shown,
            [
                "Send(request 1)",
                "Read(request 2, limit 20)",
                "Watch",
                "Unwatch"
            ]
        );
        assert_eq!(
            commands[1].shown(true),
            "Read(request 2, !room:example.org, limit 20)"
        );
        let message = Msg {
            event_id: "$one:example.org".into(),
            sender: "@ann:example.org".into(),
            sender_name: "Ann".into(),
            timestamp_ms: 1,
            kind: "text".into(),
            body: "the launch codes".into(),
            edited: false,
            own: false,
            truncated: false,
            warning: None,
            verified: true,
            untrusted: None,
        };
        let events = [
            AgentEvent::Sent {
                request: 1,
                result: Ok("$sent:example.org".into()),
            },
            AgentEvent::Sent {
                request: 1,
                result: Err("#garden:example.org refused".into()),
            },
            AgentEvent::Read {
                request: 2,
                result: Ok(vec![message.clone()]),
            },
            AgentEvent::Read {
                request: 2,
                result: Err("#garden:example.org refused".into()),
            },
            AgentEvent::Watched {
                room_id: "!room:example.org".into(),
                messages: vec![message],
            },
            AgentEvent::WatchEnded {
                room_id: "!room:example.org".into(),
                reason: "#garden:example.org is gone".into(),
            },
        ];
        let shown: Vec<String> = events.iter().map(|e| format!("{e:?}")).collect();
        assert_eq!(
            shown,
            [
                "Sent(request 1, ok: true)",
                "Sent(request 1, ok: false)",
                "Read(request 2, 1 messages)",
                "Read(request 2, ok: false)",
                "Watched(1 messages)",
                "WatchEnded",
            ]
        );
        assert_eq!(
            events[5].shown(true),
            "WatchEnded(!room:example.org, #garden:example.org is gone)"
        );
        assert_eq!(
            events[0].shown(true),
            "Sent(request 1, ok: true, $sent:example.org)"
        );
    }

    #[test]
    fn every_doubt_about_a_sender_has_its_word() {
        let words: Vec<&str> = [
            Shield::UnverifiedDevice,
            Shield::IdentityChanged,
            Shield::SenderMismatch,
            Shield::NotEncrypted,
            Shield::Unconfirmed,
        ]
        .into_iter()
        .map(warning)
        .collect();
        assert_eq!(
            words,
            [
                "unverified_device",
                "identity_changed",
                "sender_mismatch",
                "not_encrypted",
                "unconfirmed"
            ]
        );
    }

    /// The strict judgement, branch by branch: only an encrypted message from a
    /// verified user's cross-signed device, with a key from that device, passes.
    #[test]
    fn only_a_verified_sender_passes_the_strict_judgement() {
        use VerificationLevel as Level;
        let unverified = VerificationState::Unverified;
        let verified = VerificationState::Verified;
        assert_eq!(strict_doubt("text", Some(&verified), false), None);
        for kind in ["notice", "emote", "image", "file", "poll", "other"] {
            assert_eq!(strict_doubt(kind, Some(&verified), false), None, "{kind}");
        }
        // Not encrypted: a room without encryption, or sent in the clear.
        assert_eq!(strict_doubt("text", None, false), Some("not_encrypted"));
        // Every way of not being verified, the one the app does not show included.
        for (level, why) in [
            (Level::UnverifiedIdentity, "unverified_identity"),
            (Level::VerificationViolation, "identity_changed"),
            (Level::UnsignedDevice, "unverified_device"),
            (
                Level::None(DeviceLinkProblem::MissingDevice),
                "unverified_device",
            ),
            (
                Level::None(DeviceLinkProblem::InsecureSource),
                "unconfirmed",
            ),
            (Level::MismatchedSender, "sender_mismatch"),
        ] {
            let state = unverified(level);
            assert_eq!(strict_doubt("text", Some(&state), false), Some(why));
            assert_eq!(strict_doubt("text", Some(&state), true), Some(why));
        }
        // A key that someone else passed on is not the sender's own word for it.
        assert_eq!(
            strict_doubt("text", Some(&verified), true),
            Some("unconfirmed")
        );
        // What could not be read and what was deleted says nothing, whoever sent it.
        for state in [None, Some(&verified)] {
            assert_eq!(
                strict_doubt("undecryptable", state, false),
                Some("undecryptable")
            );
            assert_eq!(strict_doubt("redacted", state, false), Some("redacted"));
        }
    }

    /// A text is sent with the app's Markdown, and a leading slash is text.
    #[test]
    fn a_text_is_sent_with_markdown_and_without_slash_commands() {
        let sent = |text: &str| match content_of(text).expect("a message").msgtype {
            MessageType::Text(text) => (text.body, text.formatted.map(|html| html.body)),
            other => panic!("not a text: {other:?}"),
        };
        let (body, html) = sent("**done** with /etc/hosts");
        assert_eq!(body, "**done** with /etc/hosts");
        let html = html.expect("formatted");
        assert!(html.contains("<strong>done</strong>"), "{html}");
        // Not an emote, and not refused as an unknown command.
        assert_eq!(sent("/me waves").0, "/me waves");
        assert_eq!(sent("/unknown thing").0, "/unknown thing");
        assert_eq!(sent("plain words"), ("plain words".to_owned(), None));
    }

    /// Only a room the account is in is sent to, read or watched: not one it was
    /// invited to (nothing here accepts an invitation), not one it left, not one it
    /// does not know, and nothing that is not a room id.
    #[tokio::test]
    async fn a_room_the_account_is_not_in_is_refused() {
        use matrix_sdk::ruma::room_id;

        let (joined, gone, invited) = (
            room_id!("!joined:localhost"),
            room_id!("!gone:localhost"),
            room_id!("!invited:localhost"),
        );
        let server = Server {
            joined: vec![joined.as_str(), gone.as_str()],
            invited: vec![invited.as_str()],
            ..Server::default()
        };
        let seen = server.seen.clone();
        let client = server.client().await;
        let left = LeftRooms::default();
        left.mark(gone);
        let (mut agent, mut events) = agent_of(client.clone(), &left);
        let before = seen.lock().unwrap().len();

        for (request, room) in [
            gone.as_str(),
            invited.as_str(),
            "!unknown:localhost",
            "Agents",
            "",
        ]
        .into_iter()
        .enumerate()
        {
            let request = request as u64;
            agent.handle(AgentCmd::Send {
                request,
                room_id: room.to_owned(),
                text: "hello".into(),
            });
            match next(&mut events).await {
                AgentEvent::Sent {
                    request: answered,
                    result: Err(_),
                } => assert_eq!(answered, request),
                other => panic!("sending to {room:?} was not refused: {other:?}"),
            }
            agent.handle(AgentCmd::Read {
                request,
                room_id: room.to_owned(),
                limit: 10,
            });
            match next(&mut events).await {
                AgentEvent::Read {
                    request: answered,
                    result: Err(_),
                } => assert_eq!(answered, request),
                other => panic!("reading {room:?} was not refused: {other:?}"),
            }
            agent.handle(AgentCmd::Watch {
                room_id: room.to_owned(),
            });
            match next(&mut events).await {
                AgentEvent::WatchEnded { room_id, .. } => assert_eq!(room_id, room),
                other => panic!("watching {room:?} was not refused: {other:?}"),
            }
        }
        assert!(agent.watchers.is_empty());
        // None of it reached the server: nothing was joined, sent or asked.
        let asked = seen.lock().unwrap()[before..].to_vec();
        assert!(asked.is_empty(), "{asked:?}");
        // The invitation is still an invitation.
        assert_eq!(
            client.get_room(invited).unwrap().state(),
            RoomState::Invited
        );
        agent.stop();
    }

    /// Watching and reading a joined room tells its messages and sends the server
    /// nothing that marks the room as read (no receipt, no read marker, no typing).
    #[tokio::test]
    async fn watching_and_reading_a_room_sends_no_receipt() {
        use matrix_sdk::ruma::room_id;

        let room = room_id!("!joined:localhost");
        let server = Server {
            joined: vec![room.as_str()],
            ..Server::default()
        };
        let seen = server.seen.clone();
        let client = server.client().await;
        let left = LeftRooms::default();
        let (mut agent, mut events) = agent_of(client, &left);
        let before = seen.lock().unwrap().len();

        agent.handle(AgentCmd::Watch {
            room_id: room.to_string(),
        });
        match next(&mut events).await {
            AgentEvent::Watched { room_id, messages } => {
                assert_eq!(room_id, room.as_str());
                assert!(messages.len() <= WATCH_TAIL);
            }
            other => panic!("not watched: {other:?}"),
        }
        // Asked for twice, watched once.
        agent.handle(AgentCmd::Watch {
            room_id: room.to_string(),
        });
        assert_eq!(agent.watchers.len(), 1);

        agent.handle(AgentCmd::Read {
            request: 7,
            room_id: room.to_string(),
            limit: 5,
        });
        loop {
            match next(&mut events).await {
                AgentEvent::Read { request, result } => {
                    assert_eq!(request, 7);
                    assert!(result.expect("read").len() <= 5);
                    break;
                }
                // The watched room may have news meanwhile.
                AgentEvent::Watched { .. } => {}
                other => panic!("not read: {other:?}"),
            }
        }

        agent.handle(AgentCmd::Unwatch {
            room_id: room.to_string(),
        });
        assert!(agent.watchers.is_empty());
        agent.stop();

        let asked = seen.lock().unwrap()[before..].to_vec();
        for path in &asked {
            for forbidden in [
                "/receipt",
                "/read_markers",
                "/typing",
                "/join",
                "/leave",
                "/send/",
            ] {
                assert!(!path.contains(forbidden), "{forbidden} in {asked:?}");
            }
        }
    }

    /// The room of the tests below that run on a fake homeserver.
    const SAID: &str = "!said:localhost";

    /// A joined room whose messages are the texts `said` (each an event id, its sender
    /// and what it says), on a fake homeserver that runs as long as it is kept.
    async fn room_saying(
        said: &[(&str, &str, &str)],
    ) -> (crate::matrix::spaces::fake::Server, Room) {
        let events: Vec<_> = said
            .iter()
            .zip(10u64..)
            .map(|((id, sender, body), at)| text_event(id, sender, body, at))
            .collect();
        room_holding(events).await
    }

    /// A text event `id` from `sender` that says `body`, sent at `at`.
    fn text_event(id: &str, sender: &str, body: &str, at: u64) -> serde_json::Value {
        serde_json::json!({
            "type": "m.room.message", "event_id": id, "sender": sender,
            "origin_server_ts": at,
            "content": { "msgtype": "m.text", "body": body },
        })
    }

    /// A joined room whose timeline holds `events`, on a fake homeserver that runs as
    /// long as it is kept.
    async fn room_holding(
        events: Vec<serde_json::Value>,
    ) -> (crate::matrix::spaces::fake::Server, Room) {
        use matrix_sdk::authentication::SessionTokens;
        use matrix_sdk::authentication::matrix::MatrixSession;
        use matrix_sdk::config::SyncSettings;
        use serde_json::json;

        use crate::matrix::spaces::fake;

        let create = json!({ "creator": fake::ME, "room_version": "10" });
        let room = json!({
            "state": { "events": [
                fake::state("m.room.create", "", create),
                fake::state("m.room.member", fake::ME, json!({ "membership": "join" })),
            ] },
            "timeline": { "events": events, "limited": false },
        });
        let server = fake::Server::start(json!({ "join": { SAID: room } })).await;
        let client = Client::builder()
            .homeserver_url(&server.url)
            .build()
            .await
            .unwrap();
        client
            .restore_session(matrix_sdk::AuthSession::Matrix(MatrixSession {
                meta: matrix_sdk::SessionMeta {
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
        // Before the sync, so that the room's events reach its timeline.
        client.event_cache().subscribe().unwrap();
        client.sync_once(SyncSettings::default()).await.unwrap();
        let room = client.get_room(<&RoomId>::try_from(SAID).unwrap()).unwrap();
        (server, room)
    }

    /// The items of a timeline of a room that holds the texts `said` (each an event id
    /// and what it says, all by the account itself), as the SDK shows them.
    async fn shown(said: &[(&str, &str)]) -> Vector<Arc<TimelineItem>> {
        let me = crate::matrix::spaces::fake::ME;
        let said: Vec<_> = said.iter().map(|(id, body)| (*id, me, *body)).collect();
        let (server, room) = room_saying(&said).await;
        let ids: Vec<&str> = said.iter().map(|(id, _, _)| *id).collect();
        shown_in(server, room, &ids).await
    }

    /// The items of the timeline of `room` once the events `ids` show in it.
    async fn shown_in(
        _server: crate::matrix::spaces::fake::Server,
        room: Room,
        ids: &[&str],
    ) -> Vector<Arc<TimelineItem>> {
        let timeline = open(&room).await.unwrap();
        for _ in 0..500 {
            let items = timeline.items().await;
            let there = ids.iter().all(|id| {
                items.iter().any(|item| {
                    item.as_event()
                        .and_then(|event| event.event_id())
                        .is_some_and(|event_id| event_id.as_str() == *id)
                })
            });
            if there {
                return items;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the messages did not show within five seconds");
    }

    /// The ids and texts of `messages`.
    fn said(messages: &[Msg]) -> Vec<(&str, &str)> {
        messages
            .iter()
            .map(|message| (message.event_id.as_str(), message.body.as_str()))
            .collect()
    }

    #[tokio::test]
    async fn a_message_the_timeline_shows_twice_is_told_once_as_it_is_now() {
        let before = shown(&[("$a", "first"), ("$b", "as it was")]).await;
        let after = shown(&[("$a", "first"), ("$b", "as it is now")]).await;
        // A batch shown twice, where only the trailing copy heard of the change.
        let mut items = before;
        items.extend(
            after
                .iter()
                .filter(|item| item.as_event().is_some())
                .cloned(),
        );
        let own = UserId::parse(crate::matrix::spaces::fake::ME).unwrap();
        let none = IgnoredSet::default();
        assert_eq!(
            said(&messages(&items, &own, &none, 10)),
            [("$a", "first"), ("$b", "as it is now")]
        );
        // Only the newest are made, when no more are wanted.
        assert_eq!(
            said(&messages(&items, &own, &none, 1)),
            [("$b", "as it is now")]
        );
    }

    /// Of a message the timeline shows twice, the SDK decrypts the copy it finds by
    /// position when a key comes late (or from a key file), which can be the first:
    /// the other, still locked, does not hide it, wherever it stands.
    #[tokio::test]
    async fn a_message_shown_twice_is_told_decrypted_when_one_copy_is() {
        let me = crate::matrix::spaces::fake::ME;
        let readable = shown(&[("$a", "first"), ("$b", "readable now")]).await;
        let sealed = serde_json::json!({
            "type": "m.room.encrypted", "event_id": "$b", "sender": me,
            "origin_server_ts": 11,
            "content": {
                "algorithm": "m.megolm.v1.aes-sha2",
                "ciphertext": "AwgAEhAkDXaGc2tOy3FDaXVjnHhVb0hXSDEc",
                "device_id": "ELSEWHERE",
                "sender_key": "aV9BpqYFqJpKYmgERyGv/6QyKMcgLqxM05V0gvzg9Yk",
                "session_id": "SNrH6yKQiXGqNx0Fbr8RL3FT1LtFTQ02dS3OFOsBq6w",
            },
        });
        let (server, room) = room_holding(vec![text_event("$a", me, "first", 10), sealed]).await;
        let locked = shown_in(server, room, &["$a", "$b"]).await;
        let own = UserId::parse(me).unwrap();
        let none = IgnoredSet::default();
        let kinds = |items: &Vector<Arc<TimelineItem>>| {
            messages(items, &own, &none, 10)
                .into_iter()
                .map(|message| (message.event_id, message.kind))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            kinds(&locked)[1],
            ("$b".to_owned(), "undecryptable".to_owned())
        );
        let events = |items: &Vector<Arc<TimelineItem>>| {
            items
                .iter()
                .filter(|item| item.as_event().is_some())
                .cloned()
                .collect::<Vec<_>>()
        };
        // Decrypted first, the trailing copy still locked; and the other way round.
        let mut first_decrypted = readable.clone();
        first_decrypted.extend(events(&locked));
        let mut last_decrypted = locked.clone();
        last_decrypted.extend(events(&readable));
        for items in [first_decrypted, last_decrypted] {
            assert_eq!(
                said(&messages(&items, &own, &none, 10)),
                [("$a", "first"), ("$b", "readable now")]
            );
            assert_eq!(kinds(&items)[1], ("$b".to_owned(), "text".to_owned()));
        }
    }

    /// A watched room is told when what its messages say changes, and not for every
    /// change of its timeline.
    #[tokio::test]
    async fn a_watched_room_is_told_again_only_when_a_message_changed() {
        use matrix_sdk::ruma::OwnedUserId;

        const BOB: &str = "@bob:localhost";
        let (_server, room) = room_saying(&[("$a", BOB, "hello")]).await;
        let (events, mut told) = unbounded_channel();
        let (ignore, ignored) = watch::channel(IgnoredSet::default());
        let watching = tokio::spawn(watch_room(room, SAID.to_owned(), events, ignored));
        loop {
            match next(&mut told).await {
                AgentEvent::Watched { messages, .. } if !messages.is_empty() => {
                    assert_eq!(said(&messages), [("$a", "hello")]);
                    break;
                }
                AgentEvent::Watched { .. } => {}
                other => panic!("not watched: {other:?}"),
            }
        }
        // Someone who said nothing here is ignored: no message changes.
        let nobody: OwnedUserId = "@nobody:localhost".try_into().unwrap();
        ignore.send_replace(Arc::new([nobody.clone()].into()));
        let again = tokio::time::timeout(Duration::from_millis(500), told.recv()).await;
        assert!(again.is_err(), "told again although nothing changed");
        // The one who said something is: their message is gone.
        let bob: OwnedUserId = BOB.try_into().unwrap();
        ignore.send_replace(Arc::new([nobody, bob].into()));
        match next(&mut told).await {
            AgentEvent::Watched { messages, .. } => assert!(messages.is_empty()),
            other => panic!("not watched: {other:?}"),
        }
        watching.abort();
    }

    /// The last change before the changes end is told before the watch ends.
    #[tokio::test]
    async fn a_last_change_before_the_end_is_told() {
        let last = shown(&[("$a", "hello")]).await;
        let (_ignore, mut ignored) = watch::channel(IgnoredSet::default());
        let own = UserId::parse(crate::matrix::spaces::fake::ME).unwrap();
        let changes = futures::stream::iter([vec![VectorDiff::Reset { values: last }]]);
        let mut told = Vec::new();
        let tell = |messages| {
            told.push(messages);
            true
        };
        assert!(follow(Vector::new(), changes, &own, &mut ignored, tell).await);
        let told: Vec<_> = told.iter().map(|messages| said(messages)).collect();
        assert_eq!(told, [vec![], vec![("$a", "hello")]]);
    }

    // ----- against a real homeserver ----------------------------------------------------

    /// The whole channel against a real homeserver, in an encrypted room, with two
    /// accounts: the test account (the one `serve` runs as) and a second one (the
    /// person who is asked and answers). The account logs in and accepts the room's
    /// invitation with the worker, as with the app; then `serve` runs on the saved
    /// session, and `rooms`, `listen`, `read` and `send` go over its socket.
    ///
    /// `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=alicepass cargo test -- --ignored`
    /// (the second account: `MATRIX_RAT_TEST_USER2` and `MATRIX_RAT_TEST_PASSWORD2`,
    /// by default bob and bobpass).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn serve_sends_reads_and_listens_in_an_encrypted_room_and_leaves_it_unread() {
        use matrix_sdk::config::SyncSettings;
        use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoom;
        use matrix_sdk::ruma::events::InitialStateEvent;
        use matrix_sdk::ruma::events::receipt::{ReceiptThread, ReceiptType};
        use matrix_sdk::ruma::events::room::encryption::RoomEncryptionEventContent;
        use matrix_sdk::ruma::events::room::member::MembershipState;
        use matrix_sdk::ruma::events::room::message::{
            OriginalSyncRoomMessageEvent, RoomMessageEventContent,
        };
        use matrix_sdk::ruma::{OwnedUserId, UserId};

        use crate::agent::client::{ClientError, Conn};
        use crate::agent::proto::Code;
        use crate::agent::serve::{Limits, Served};
        use crate::matrix::{MatrixCmd, RoomCategory, SecretStoreMode, spawn_with};

        let var = |name: &str| std::env::var(name).ok();
        let homeserver =
            var("MATRIX_RAT_TEST_HOMESERVER").unwrap_or_else(|| "http://127.0.0.1:6167".into());
        let username = var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let other_name = var("MATRIX_RAT_TEST_USER2").unwrap_or_else(|| "bob".into());
        let other_password = var("MATRIX_RAT_TEST_PASSWORD2").unwrap_or_else(|| "bobpass".into());
        let stamp = std::process::id();
        let within = |seconds: u64| Duration::from_secs(seconds);

        // The person: a plain SDK client that syncs all the time (it hands out the
        // room's keys and takes in what the account sends).
        let person = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .expect("client");
        person
            .matrix_auth()
            .login_username(&other_name, &other_password)
            .initial_device_display_name("matrix-rat agent test")
            .send()
            .await
            .expect("login of the second user");
        person
            .sync_once(SyncSettings::default())
            .await
            .expect("sync");
        let person_id: OwnedUserId = person.user_id().expect("logged in").to_owned();
        // What the person receives, decrypted: the event's id, its text and its HTML.
        let (received, mut inbox) = unbounded_channel::<(String, String, Option<String>)>();
        person.add_event_handler(move |event: OriginalSyncRoomMessageEvent| {
            let received = received.clone();
            async move {
                if let MessageType::Text(text) = &event.content.msgtype {
                    let _ = received.send((
                        event.event_id.to_string(),
                        text.body.clone(),
                        text.formatted.as_ref().map(|html| html.body.clone()),
                    ));
                }
            }
        });
        let person_sync = tokio::spawn({
            let person = person.clone();
            async move { person.sync(SyncSettings::default()).await }
        });

        // A profile of its own, in a folder short enough for a socket's address.
        let root = std::env::temp_dir().join(format!("mr-agent-e2e-{stamp}"));
        let _ = std::fs::remove_dir_all(&root);
        let paths = crate::paths::Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "it".into(),
        };
        paths.create_dirs().unwrap();

        // Once, with the worker as the app drives it: log in, and accept the
        // invitation to the person's encrypted room. (`serve` does neither.)
        let name = format!("agent test {stamp}");
        let (handle, mut events) = spawn_with(paths.clone(), SecretStoreMode::File);
        handle.send(MatrixCmd::LoginPassword {
            homeserver: homeserver.clone(),
            username,
            password: password.into(),
        });
        let logged_in = async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    MatrixEvent::LoggedIn { user_id } => return user_id,
                    _ => {}
                }
            }
        };
        let own_id = tokio::time::timeout(within(30), logged_in)
            .await
            .expect("login in time");
        let own_user = UserId::parse(&own_id).expect("a user id");
        let encrypted = || {
            let mut create = CreateRoom::new();
            create.invite = vec![own_user.clone()];
            create.initial_state = vec![
                InitialStateEvent::with_empty_state_key(
                    RoomEncryptionEventContent::with_recommended_defaults(),
                )
                .to_raw_any(),
            ];
            create
        };
        let mut create = encrypted();
        create.name = Some(name.clone());
        let person_room = person.create_room(create).await.expect("room");
        let room_id = person_room.room_id().to_string();
        let joined = async {
            let mut asked = false;
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::Error(err) | MatrixEvent::ActionFailed(err) => panic!("{err}"),
                    MatrixEvent::Rooms(rooms) => {
                        match rooms.iter().find(|room| room.id == room_id) {
                            Some(room) if room.category == RoomCategory::Invite => {
                                if !asked {
                                    asked = true;
                                    handle.send(MatrixCmd::Join {
                                        room_id: room_id.clone(),
                                    });
                                }
                            }
                            Some(_) => return,
                            None => {}
                        }
                    }
                    _ => {}
                }
            }
        };
        tokio::time::timeout(within(60), joined)
            .await
            .expect("the invitation arrives and is accepted");
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        drop(handle);

        // A second invitation, which nobody accepts: `serve` must leave it alone.
        let mut create = encrypted();
        create.name = Some(format!("agent test invitation {stamp}"));
        let invitation = person.create_room(create).await.expect("room");

        // `serve`, on the saved session.
        let served = match Served::start(&paths, SecretStoreMode::File, Limits::default()).await {
            Ok(served) => served,
            Err(err) => panic!("serve did not start on the saved session: {err:?}"),
        };
        let socket = paths.agent_socket();
        assert!(socket.exists());

        // The room is listed, under its name, as encrypted.
        let listed = async {
            loop {
                let rooms = Conn::open(&socket)
                    .await
                    .expect("connect")
                    .rooms()
                    .await
                    .expect("rooms");
                if let Some(room) = rooms.into_iter().find(|room| room.id == room_id) {
                    return room;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };
        let room = tokio::time::timeout(within(60), listed)
            .await
            .expect("the joined room is listed");
        assert_eq!(room.name, name);
        assert!(room.encrypted);

        // A listener, by the room's name. It is in once the first sync is through.
        let mut listener = Conn::open(&socket)
            .await
            .expect("connect")
            .listen(&name)
            .await
            .expect("listen");
        assert_eq!(listener.room_id(), room_id);

        // The person asks.
        let question = format!("what is the answer, {stamp}?");
        let asked = person_room
            .send(RoomMessageEventContent::text_plain(&question))
            .await
            .expect("the person's message");
        let asked_id = asked.response.event_id.to_string();

        // The listener hears it, decrypted (after, perhaps, a line that says it
        // could not be decrypted yet).
        let heard = async {
            loop {
                let message = listener.next().await.expect("the stream goes on");
                if message.event_id == asked_id && message.kind == "text" {
                    return message;
                }
            }
        };
        let heard = tokio::time::timeout(within(60), heard)
            .await
            .expect("the person's message is heard");
        assert_eq!(heard.body, question);
        assert_eq!(heard.sender, person_id.as_str());
        assert!(!heard.own && !heard.edited);
        // The account never verified the person: not their word in the strict
        // sense, and the message says why.
        assert!(!heard.verified && heard.untrusted.is_some());

        // `read` has it too, by the room's id.
        let mut conn = Conn::open(&socket).await.expect("connect");
        let (read_room, messages) = conn.read(&room_id, 50).await.expect("read");
        assert_eq!(read_room, room_id);
        let found = messages
            .iter()
            .find(|message| message.event_id == asked_id)
            .expect("the person's message is read");
        assert_eq!(found.body, question);
        assert_eq!(found.kind, "text");

        // Having listened and read, the room is still unread for the account: its
        // own view counts the message as unread, and stays so...
        let unread = async {
            while !served.unread(&room_id).await {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        };
        tokio::time::timeout(within(30), unread)
            .await
            .expect("the room counts as unread for the account");
        tokio::time::sleep(within(4)).await;
        let (_, again) = conn.read(&room_id, 50).await.expect("read");
        assert!(again.iter().any(|message| message.event_id == asked_id));
        assert!(
            served.unread(&room_id).await,
            "reading through serve marked the room as read"
        );
        // ...and the person sees no read receipt of the account on the message.
        let receipt = person_room
            .load_user_receipt(ReceiptType::Read, &ReceiptThread::Unthreaded, &own_user)
            .await
            .expect("receipts");
        assert!(
            receipt.is_none_or(|(event_id, _)| event_id.as_str() != asked_id),
            "reading through serve sent a read receipt"
        );

        // The account answers through the socket, with Markdown.
        while inbox.try_recv().is_ok() {}
        let answer = format!("**42**, says `{stamp}`");
        let sent_id = conn.send(&room_id, &answer).await.expect("send");
        assert!(sent_id.starts_with('$'), "{sent_id}");
        // The person reads it: decrypted, formatted, under the id `send` told.
        let arrived = async {
            loop {
                let (event_id, body, html) = inbox.recv().await.expect("the person syncs");
                if event_id == sent_id {
                    return (body, html);
                }
            }
        };
        let (body, html) = tokio::time::timeout(within(60), arrived)
            .await
            .expect("the answer arrives");
        assert_eq!(body, answer);
        let html = html.expect("the answer is formatted");
        assert!(html.contains("<strong>42</strong>"), "{html}");

        // The listener hears the account's own message too, marked as its own.
        let own_heard = async {
            loop {
                let message = listener.next().await.expect("the stream goes on");
                if message.event_id == sent_id {
                    return message;
                }
            }
        };
        let own_heard = tokio::time::timeout(within(60), own_heard)
            .await
            .expect("the account's own message is heard");
        assert!(own_heard.own);
        assert_eq!(own_heard.body, answer);
        assert_eq!(own_heard.sender, own_id);

        // A room the account is not in cannot be reached: not the one it was only
        // invited to, and not one it has nothing to do with.
        let elsewhere = person
            .create_room(CreateRoom::new())
            .await
            .expect("another room");
        for unreachable in [invitation.room_id().as_str(), elsewhere.room_id().as_str()] {
            match conn.send(unreachable, "hello").await {
                Err(ClientError::Refused(refusal)) => assert_eq!(refusal.code, Code::NotFound),
                other => panic!("a room the account is not in was not refused: {other:?}"),
            }
        }
        // The invitation is still an invitation: `serve` accepted nothing.
        let rooms = Conn::open(&socket)
            .await
            .expect("connect")
            .rooms()
            .await
            .expect("rooms");
        assert!(
            rooms
                .iter()
                .all(|room| room.id != invitation.room_id().as_str())
        );
        let membership = invitation
            .get_member_no_sync(&own_user)
            .await
            .expect("members")
            .map(|member| member.membership().clone());
        assert_eq!(membership, Some(MembershipState::Invite));

        // Stopping removes the socket; a client then says that nothing runs.
        drop(listener);
        served.stop().await;
        assert!(!socket.exists(), "the socket is removed on the way out");
        assert!(matches!(
            Conn::open(&socket).await,
            Err(ClientError::NotRunning)
        ));

        person_sync.abort();
        for room in [person_room, invitation, elsewhere] {
            let _ = room.leave().await;
        }
        let _ = person.matrix_auth().logout().await;
        let _ = std::fs::remove_dir_all(root);
    }
}
