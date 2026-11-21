//! Collecting a stretch of a room's messages as text: for an archive of the room,
//! written to a file here, and for a summary of what is unread, handed to the app.
//!
//! Like the agent channel (`super::agent`) this keeps apart from the timeline of the
//! open room: a collection gets a timeline of its own from the SDK, on the same event
//! cache, and only looks at it. Nothing here sends a receipt or moves the read marker.
//!
//! Older messages are asked for page by page until the collection has what it was
//! asked for, with the count told to the app after every page. One collection runs at
//! a time; a new one, or [`CollectCmd::Cancel`], ends the one before, and a file that
//! was being written is removed.
//!
//! What bounds memory: the SDK's timeline holds the messages while they are loaded
//! (at most [`ARCHIVE_EVENTS`]); they are then turned into text and written one by
//! one, so the text of the room is never held as a whole. A room with more gets its
//! newest messages archived, and the archive says so.
//!
//! What it costs: the timeline here pages the room's event cache, which the open
//! room's own timeline shares. Every page loaded for an archive therefore also
//! reaches that timeline, which grows and tells the app its messages again. That is
//! why one archive takes so few events; loading history outside the live cache would
//! lift that.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::{FutureExt, StreamExt, pin_mut};
use matrix_sdk::Room;
use matrix_sdk::ruma::events::fully_read::FullyReadEventContent;
use matrix_sdk::ruma::{OwnedUserId, UserId};
use matrix_sdk_ui::eyeball_im::{Vector, VectorDiff};
use matrix_sdk_ui::timeline::{
    RoomExt, Timeline, TimelineFocus, TimelineItem, TimelineReadReceiptTracking, TimelineUniqueId,
    VirtualTimelineItem,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::MatrixEvent;
use super::safety::IgnoredSet;
use crate::collect::{self, Cut, Span};
use crate::logsafe;
use crate::model::Message;

/// The most events one archive takes; of a room with more, the newest.
pub const ARCHIVE_EVENTS: usize = 5_000;
/// How many events are loaded at most to find where the user stopped reading.
const UNREAD_EVENTS: usize = collect::SUMMARY_MESSAGES + 50;
/// How many of the oldest loaded events must be older than an archive wants before
/// loading ends.
const OLDEST: usize = 5;
/// Events asked for at a time.
const PAGE: u16 = 100;
/// Fewest events asked for in one page near a collection's limit, so a tail of
/// filtered events (reactions, edits) does not turn into one-event pages.
const MIN_PAGE: u16 = 20;
/// How long opening a room's messages may take, and one request for older ones.
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
const PAGE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long collecting what is unread may take in all.
const UNREAD_BUDGET: Duration = Duration::from_secs(60);
/// How long a collection waits for the messages it asked for to show in the timeline.
const SETTLE: Duration = Duration::from_secs(2);
/// How long a collection waits for a follow-up event-cache batch after a short lazy
/// unskip or an early `TimelineStart` marker.
const SETTLE_MORE: Duration = Duration::from_secs(1);

pub enum CollectCmd {
    /// The unread messages of a joined room as text for a summary; answered with
    /// [`CollectEvent::Unread`].
    Unread {
        request: u64,
        room_id: String,
        /// The first unread message as the app shows it (the "new" line of the open
        /// room), by its event id. Without it the account's read marker and its
        /// read receipt decide.
        from: Option<String>,
        /// How many of the newest messages to take where none of them is found;
        /// the answer then says that it is not known what is unread.
        newest: usize,
    },
    /// Write the messages of a joined room into a new Markdown file in `dir`;
    /// answered with [`CollectEvent::Archived`].
    Archive {
        request: u64,
        room_id: String,
        span: Span,
        /// For [`Span::Loaded`]: how many messages the app holds of the room. At
        /// least so many events are loaded (a timeline of its own starts with a
        /// handful, whatever the app has).
        held: usize,
        dir: PathBuf,
        /// The room's name as the app shows it, for the heading and the file name.
        room_name: String,
    },
    /// Give up the collection that runs; it is not answered.
    Cancel,
}

/// Never prints a room, an event, a name or a path.
impl std::fmt::Debug for CollectCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.shown(false))
    }
}

impl CollectCmd {
    /// `ids`: with the room's id, which only tests may ask for.
    pub(super) fn shown(&self, ids: bool) -> String {
        let id = |id: &str| ids.then(|| id.to_owned());
        let request = |request: &u64| Some(format!("request {request}"));
        let (name, parts) = match self {
            Self::Unread {
                request: r,
                room_id,
                from,
                ..
            } => (
                "Unread",
                vec![
                    request(r),
                    id(room_id),
                    Some(format!("from: {}", from.is_some())),
                ],
            ),
            Self::Archive {
                request: r,
                room_id,
                span,
                ..
            } => (
                "Archive",
                vec![request(r), id(room_id), Some(format!("{span:?}"))],
            ),
            Self::Cancel => ("Cancel", vec![]),
        };
        logsafe::shape(name, parts)
    }
}

/// What is unread in a room, as text.
#[derive(Clone, PartialEq, Eq)]
pub struct UnreadText {
    /// One message a line, oldest first (see [`collect::summary_line`]).
    pub text: String,
    /// How many messages are in it.
    pub messages: usize,
    /// There is more unread than this: these are the newest.
    pub cut: bool,
    /// Where the user stopped reading was found, so these are unread ones. Without
    /// it they are the newest messages of the room, read or not.
    pub known: bool,
}

/// Hand-written: the text is what people wrote.
impl std::fmt::Debug for UnreadText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnreadText")
            .field("messages", &self.messages)
            .field("cut", &self.cut)
            .field("known", &self.known)
            .finish_non_exhaustive()
    }
}

/// An archive that was written.
#[derive(Clone, PartialEq, Eq)]
pub struct Archived {
    pub path: PathBuf,
    pub messages: usize,
    pub cut: Option<Cut>,
}

/// Hand-written: the path names the user's folders and the room.
impl std::fmt::Debug for Archived {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Archived")
            .field("messages", &self.messages)
            .field("cut", &self.cut)
            .finish_non_exhaustive()
    }
}

pub enum CollectEvent {
    /// A collection goes on: so many events are loaded by now.
    Progress { request: u64, loaded: usize },
    /// Outcome of [`CollectCmd::Unread`]; the reason of a failure is for the user.
    Unread {
        request: u64,
        result: Result<UnreadText, String>,
    },
    /// Outcome of [`CollectCmd::Archive`]; the reason of a failure is for the user.
    Archived {
        request: u64,
        result: Result<Archived, String>,
    },
}

/// Never prints a text, a path or the reason of a failure.
impl std::fmt::Debug for CollectEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.shown(false))
    }
}

impl CollectEvent {
    /// `full`: with the reason of a failure, which only tests may ask for.
    pub(super) fn shown(&self, full: bool) -> String {
        let request = |request: &u64| Some(format!("request {request}"));
        let reason = |reason: &str| full.then(|| reason.to_owned());
        let (name, parts) = match self {
            Self::Progress { request: r, loaded } => (
                "Progress",
                vec![request(r), Some(format!("{loaded} loaded"))],
            ),
            Self::Unread { request: r, result } => (
                "Unread",
                vec![
                    request(r),
                    Some(match result {
                        Ok(unread) => format!("{} messages", unread.messages),
                        Err(_) => "ok: false".to_owned(),
                    }),
                    result.as_ref().err().and_then(|err| reason(err)),
                ],
            ),
            Self::Archived { request: r, result } => (
                "Archived",
                vec![
                    request(r),
                    Some(match result {
                        Ok(archived) => format!("{} messages", archived.messages),
                        Err(_) => "ok: false".to_owned(),
                    }),
                    result.as_ref().err().and_then(|err| reason(err)),
                ],
            ),
        };
        logsafe::shape(name, parts)
    }
}

// ----- the worker side ------------------------------------------------------------------

/// Lives as long as a session.
pub struct Collector {
    client: matrix_sdk::Client,
    events: UnboundedSender<MatrixEvent>,
    ignored: watch::Receiver<IgnoredSet>,
    left: super::left::LeftRooms,
    /// The collection that runs, if one does.
    running: JoinSet<()>,
}

impl Collector {
    pub fn new(
        client: matrix_sdk::Client,
        events: UnboundedSender<MatrixEvent>,
        ignored: watch::Receiver<IgnoredSet>,
        left: super::left::LeftRooms,
    ) -> Self {
        Self {
            client,
            events,
            ignored,
            left,
            running: JoinSet::new(),
        }
    }

    /// Starts what `command` asks for, in place of what ran, and returns at once.
    pub fn handle(&mut self, command: CollectCmd) {
        self.stop();
        match command {
            CollectCmd::Cancel => {}
            CollectCmd::Unread {
                request,
                room_id,
                from,
                newest,
            } => {
                let room = super::agent::joined_room(&self.client, &self.left, &room_id);
                let events = self.events.clone();
                let ignored = self.ignored.clone();
                self.running.spawn(async move {
                    let work = async {
                        let progress = Progress::new(&events, request);
                        unread(&room?, from, newest, &ignored, &progress).await
                    };
                    let result = match tokio::time::timeout(UNREAD_BUDGET, work).await {
                        Ok(result) => result,
                        Err(_) => Err(anyhow!("loading the messages took too long")),
                    };
                    if let Err(err) = &result {
                        tracing::warn!(why = %logsafe::kind_any(err), "could not collect what is unread");
                    }
                    let _ = events.send(MatrixEvent::Collect(CollectEvent::Unread {
                        request,
                        result: result.map_err(|err| format!("{err:#}")),
                    }));
                });
            }
            CollectCmd::Archive {
                request,
                room_id,
                span,
                held,
                dir,
                room_name,
            } => {
                let room = super::agent::joined_room(&self.client, &self.left, &room_id);
                let events = self.events.clone();
                let ignored = self.ignored.clone();
                self.running.spawn(async move {
                    let result = async {
                        let progress = Progress::new(&events, request);
                        archive(&room?, span, held, dir, room_name, &ignored, &progress).await
                    }
                    .await;
                    if let Err(err) = &result {
                        tracing::warn!(why = %logsafe::kind_any(err), "could not archive a room");
                    }
                    let _ = events.send(MatrixEvent::Collect(CollectEvent::Archived {
                        request,
                        result: result.map_err(|err| format!("{err:#}")),
                    }));
                });
            }
        }
    }

    pub fn stop(&mut self) {
        self.running.abort_all();
        while self.running.try_join_next().is_some() {}
    }
}

/// Tells the app how far a collection is.
struct Progress<'a> {
    events: &'a UnboundedSender<MatrixEvent>,
    request: u64,
}

impl<'a> Progress<'a> {
    fn new(events: &'a UnboundedSender<MatrixEvent>, request: u64) -> Self {
        Self { events, request }
    }

    fn tell(&self, loaded: usize) {
        let _ = self
            .events
            .send(MatrixEvent::Collect(CollectEvent::Progress {
                request: self.request,
                loaded,
            }));
    }
}

/// A room's live timeline for looking at: with what was said in threads, and with the
/// account's read marker shown in it (which nothing here moves).
async fn open(room: &Room) -> Result<Timeline> {
    let build = room
        .timeline_builder()
        .with_focus(TimelineFocus::Live {
            hide_threaded_events: false,
        })
        .track_read_marker_and_receipts(TimelineReadReceiptTracking::MessageLikeEvents)
        .build();
    tokio::time::timeout(OPEN_TIMEOUT, build)
        .await
        .map_err(|_| anyhow!("opening the room's messages timed out"))?
        .context("could not open the room's messages")
}

type Items = Vector<Arc<TimelineItem>>;

fn event_count(items: &Items) -> usize {
    items
        .iter()
        .filter(|item| item.as_event().is_some())
        .count()
}

/// Whether what is loaded reaches back beyond `since` (milliseconds since 1970): the
/// oldest few events were all sent before it. One event alone does not say so: the
/// time is the sender's server's word, and a forged old one must not end an archive
/// of the last days early.
fn reaches_before(items: &Items, since: u64) -> bool {
    let mut oldest = items
        .iter()
        .filter_map(|item| item.as_event())
        .take(OLDEST)
        .peekable();
    oldest.peek().is_some() && oldest.all(|event| u64::from(event.timestamp().get()) < since)
}

trait LoadItem: Clone {
    type Key: Clone + PartialEq;

    fn is_event(&self) -> bool;
    fn is_start(&self) -> bool;
    fn key(&self) -> &Self::Key;
}

impl LoadItem for Arc<TimelineItem> {
    type Key = TimelineUniqueId;

    fn is_event(&self) -> bool {
        self.as_event().is_some()
    }

    fn is_start(&self) -> bool {
        self.is_timeline_start()
    }

    fn key(&self) -> &Self::Key {
        self.unique_id()
    }
}

/// How long [`load_from`] waits for one page's diffs to show on the stream.
#[derive(Clone, Copy)]
struct SettleTimes {
    page: Duration,
    more: Duration,
}

/// Applies `diffs` to `items`; returns `true` if the stream was cleared or reset.
fn apply_diffs<T: Clone>(items: &mut Vector<T>, diffs: Vec<VectorDiff<T>>) -> bool {
    let mut reset = false;
    for diff in diffs {
        if matches!(diff, VectorDiff::Clear | VectorDiff::Reset { .. }) {
            reset = true;
        }
        diff.apply(items);
    }
    reset
}

/// The oldest event item in `items` and its index, used to measure items and events
/// prepended ahead of it even if virtual items (like date dividers) at index 0 change.
struct PageAnchor<K> {
    oldest: Option<(K, usize)>,
    len: usize,
}

impl<K: Clone> PageAnchor<K> {
    fn of<T: LoadItem<Key = K>>(items: &Vector<T>) -> Self {
        let oldest = items
            .iter()
            .enumerate()
            .find_map(|(pos, item)| item.is_event().then(|| (item.key().clone(), pos)));
        Self {
            oldest,
            len: items.len(),
        }
    }
}

/// How many items and events were prepended ahead of `anchor`, or `None` if the
/// anchored oldest event is no longer in `items`.
fn prepended_before<T: LoadItem>(
    items: &Vector<T>,
    anchor: &PageAnchor<T::Key>,
) -> Option<(usize, usize)> {
    match &anchor.oldest {
        Some((key, old_pos)) => {
            let pos = items.iter().position(|item| item.key() == key)?;
            let added_items = pos.saturating_sub(*old_pos);
            let added_events = items
                .iter()
                .take(pos)
                .filter(|item| item.is_event())
                .count();
            Some((added_items, added_events))
        }
        None => {
            let added_items = items.len().saturating_sub(anchor.len);
            let added_events = items.iter().filter(|item| item.is_event()).count();
            Some((added_items, added_events))
        }
    }
}

/// Waits for the events of one `paginate_backwards(ask)` call to show on `changes`.
/// Returns `false` if `changes` ended.
///
/// The SDK can split one page into two batches when the lazy skip buffer runs out
/// (`0 < skip_count < ask`: the skipped tail arrives first, while the disk or network
/// chunk arrives later from the event-cache task), and can emit `TimelineStart` ahead
/// of the final page's events when `reached_start` is true. Unrelated live-sync or
/// in-place update diffs do not cut the wait short.
async fn settle_page<T, S>(
    items: &mut Vector<T>,
    changes: &mut std::pin::Pin<&mut S>,
    mut anchor: PageAnchor<T::Key>,
    ask: u16,
    reached_start: bool,
    lazy_possible: &mut bool,
    times: SettleTimes,
) -> bool
where
    T: LoadItem,
    S: futures::Stream<Item = Vec<VectorDiff<T>>>,
{
    let ask = usize::from(ask);
    let mut deadline = tokio::time::Instant::now() + times.page;
    let mut waiting_second = false;
    let mut saw_start_wait = false;

    loop {
        let Ok(next) = tokio::time::timeout_at(deadline, changes.as_mut().next()).await else {
            return true;
        };
        let Some(diffs) = next else {
            return false;
        };
        if apply_diffs(items, diffs) {
            *lazy_possible = true;
            return true;
        }
        let Some((added_items, added_events)) = prepended_before(items, &anchor) else {
            *lazy_possible = true;
            return true;
        };
        let has_start = items.iter().any(LoadItem::is_start);

        if waiting_second {
            if added_events > 0 {
                return true;
            }
            continue;
        }

        if reached_start {
            if added_events > 0 && has_start {
                if added_items != ask {
                    *lazy_possible = false;
                }
                return true;
            }
            if *lazy_possible && added_items > 0 && added_items < ask && !has_start {
                *lazy_possible = false;
                waiting_second = true;
                anchor = PageAnchor::of(items);
                deadline = deadline.min(tokio::time::Instant::now() + times.more);
                continue;
            }
            if added_events > 0 {
                if added_items != ask {
                    *lazy_possible = false;
                }
                return true;
            }
            if has_start && !saw_start_wait {
                saw_start_wait = true;
                deadline = deadline.min(tokio::time::Instant::now() + times.more);
            }
            continue;
        }

        if *lazy_possible {
            if added_items == 0 {
                continue;
            }
            if added_items == ask {
                return true;
            }
            *lazy_possible = false;
            if added_items > ask {
                return true;
            }
            // `0 < added_items < ask`: likely the tail of the lazy skip buffer; wait
            // for the event-cache task's disk or network batch before asking again.
            waiting_second = true;
            anchor = PageAnchor::of(items);
            deadline = deadline.min(tokio::time::Instant::now() + times.more);
            continue;
        }

        if added_events > 0 {
            return true;
        }
    }
}

async fn load_from<T, S, F>(
    mut items: Vector<T>,
    changes: S,
    most: usize,
    mut on_progress: impl FnMut(usize),
    mut enough: impl FnMut(&Vector<T>) -> bool,
    mut paginate: impl FnMut(u16) -> F,
    times: SettleTimes,
) -> (Vector<T>, Option<Cut>)
where
    T: LoadItem,
    S: futures::Stream<Item = Vec<VectorDiff<T>>>,
    F: std::future::Future<Output = Option<bool>>,
{
    let changes = changes.fuse();
    pin_mut!(changes);
    let mut failed = false;
    let mut at_start = false;
    let mut lazy_possible = true;
    loop {
        // What the timeline has worked in meanwhile, without waiting for more.
        while let Some(next) = changes.next().now_or_never() {
            match next {
                Some(diffs) => {
                    if apply_diffs(&mut items, diffs) {
                        lazy_possible = true;
                    }
                }
                None => {
                    failed = true;
                    break;
                }
            }
        }
        let loaded = items.iter().filter(|item| item.is_event()).count();
        if at_start || items.iter().any(LoadItem::is_start) || enough(&items) {
            return (items, None);
        }
        if loaded >= most {
            return (items, Some(Cut::Limit));
        }
        if failed {
            return (items, Some(Cut::LoadFailed));
        }
        on_progress(loaded);
        let remaining = most.saturating_sub(loaded);
        let ask = PAGE
            .min(u16::try_from(remaining).unwrap_or(PAGE))
            .max(MIN_PAGE);
        let anchor = PageAnchor::of(&items);
        let Some(reached_start) = paginate(ask).await else {
            failed = true;
            continue;
        };
        at_start = reached_start;
        // What was loaded reaches the timeline a moment later.
        if !settle_page(
            &mut items,
            &mut changes,
            anchor,
            ask,
            reached_start,
            &mut lazy_possible,
            times,
        )
        .await
        {
            failed = true;
        }
    }
}

/// `items` with each event once, at its last place. A batch of events can reach a
/// timeline twice (seen in the server test while sync still brought the newest
/// messages of a flood): a message must not count, or be archived, twice. The later
/// copy is the one kept: it is the one the SDK's later updates (an edit, a decryption)
/// mostly reach, and the read marker is placed after it (the SDK looks for the
/// marker's event from the back). But keys that come in later (from a key file, say)
/// can decrypt the earlier copy: one that is `locked` gives way to one that is not
/// (see [`super::convert::takes_over`]), in its place. Items without a key (virtual
/// ones, a local echo) all stay.
fn without_repeats<T, K: std::hash::Hash + Eq>(
    items: Vector<T>,
    key: impl Fn(&T) -> Option<K>,
    locked: impl Fn(&T) -> bool,
) -> Vector<T>
where
    T: Clone,
{
    use std::collections::hash_map::Entry;
    // From the back: the first copy met is the later one, and where it is kept.
    let mut place = std::collections::HashMap::new();
    let mut kept: Vec<T> = Vec::with_capacity(items.len());
    for item in items.into_iter().rev() {
        let Some(key) = key(&item) else {
            kept.push(item);
            continue;
        };
        match place.entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(kept.len());
                kept.push(item);
            }
            Entry::Occupied(slot) => {
                let shown = &mut kept[*slot.get()];
                if super::convert::takes_over(locked(shown), locked(&item), false) {
                    *shown = item;
                }
            }
        }
    }
    kept.reverse();
    kept.into_iter().collect()
}

/// The event id of a timeline item, for [`without_repeats`].
fn event_key(item: &Arc<TimelineItem>) -> Option<String> {
    Some(item.as_event()?.event_id()?.to_string())
}

/// Whether a timeline item is an event that could not be decrypted, for
/// [`without_repeats`].
fn undecryptable(item: &Arc<TimelineItem>) -> bool {
    item.as_event()
        .is_some_and(|event| event.content().as_unable_to_decrypt().is_some())
}

/// The items of `timeline` once `enough` says so, loading older messages page by page
/// until then. It also ends at the room's start, at `most` events ([`Cut::Limit`]) and
/// when a page cannot be loaded ([`Cut::LoadFailed`]); what is there by then is the
/// answer.
async fn load(
    timeline: &Timeline,
    most: usize,
    progress: &Progress<'_>,
    enough: impl FnMut(&Items) -> bool,
) -> (Items, Option<Cut>) {
    let (items, changes) = timeline.subscribe().await;
    let times = SettleTimes {
        page: SETTLE,
        more: SETTLE_MORE,
    };
    load_from(
        items,
        changes,
        most,
        |loaded| progress.tell(loaded),
        enough,
        |ask| async move {
            match tokio::time::timeout(PAGE_TIMEOUT, timeline.paginate_backwards(ask)).await {
                Ok(Ok(reached_start)) => Some(reached_start),
                Ok(Err(err)) => {
                    tracing::debug!(why = %logsafe::kind(&err), "collect: could not load older messages");
                    None
                }
                Err(_) => None,
            }
        },
        times,
    )
    .await
}

/// The event the account's read marker (`m.fully_read`) is at, if it has one.
async fn read_marker(room: &Room) -> Option<String> {
    let raw = room
        .account_data_static::<FullyReadEventContent>()
        .await
        .ok()??;
    Some(raw.deserialize().ok()?.content.event_id.to_string())
}

fn has_event(items: &Items, event_id: &str) -> bool {
    items.iter().any(|item| {
        item.as_event()
            .and_then(|event| event.event_id())
            .is_some_and(|id| id.as_str() == event_id)
    })
}

fn has_marker(items: &Items) -> bool {
    items
        .iter()
        .any(|item| matches!(item.as_virtual(), Some(VirtualTimelineItem::ReadMarker)))
}

/// The newest event the account sent itself that the server has, by its id.
fn last_own(items: &Items) -> Option<String> {
    items.iter().rev().find_map(|item| {
        let event = item.as_event().filter(|event| event.is_own())?;
        Some(event.event_id()?.to_string())
    })
}

/// One place in a room's timeline, for finding where reading stopped: also the
/// events the app shows nothing of keep their place.
struct Slot {
    /// The event's id, if it is one that the server has.
    id: Option<String>,
    /// The SDK's "new" line.
    line: bool,
    /// The message as the app would show it, if it would.
    message: Option<Message>,
}

fn slots(
    items: &Items,
    own: &UserId,
    ignored: &std::collections::HashSet<OwnedUserId>,
) -> Vec<Slot> {
    let now = chrono::Local::now();
    items
        .iter()
        .map(|item| match item.as_event() {
            Some(event) => Slot {
                id: event.event_id().map(|id| id.to_string()),
                line: false,
                message: super::convert::event_message(
                    item.unique_id().0.clone(),
                    event,
                    own,
                    ignored,
                    &now,
                ),
            },
            None => Slot {
                id: None,
                line: matches!(item.as_virtual(), Some(VirtualTimelineItem::ReadMarker)),
                message: None,
            },
        })
        .collect()
}

/// Collects what is unread in `room`.
async fn unread(
    room: &Room,
    from: Option<String>,
    newest: usize,
    ignored: &watch::Receiver<IgnoredSet>,
    progress: &Progress<'_>,
) -> Result<UnreadText> {
    let timeline = open(room).await?;
    let own = room.own_user_id().to_owned();
    // Every sign of how far the account has read: its read marker, its receipt (as
    // the server has it, and as the timeline places it among what it shows), and
    // the last message it sent itself.
    let mut read = Vec::new();
    read.extend(read_marker(room).await);
    read.extend(
        timeline
            .latest_user_read_receipt(&own)
            .await
            .map(|(id, _)| id.to_string()),
    );
    let (items, _) = load(&timeline, UNREAD_EVENTS, progress, |items| {
        has_marker(items)
            || from.iter().chain(&read).any(|id| has_event(items, id))
            // The app's line, where it has one, is looked for beyond our own messages.
            || (from.is_none() && last_own(items).is_some())
    })
    .await;
    let items = without_repeats(items, event_key, undecryptable);
    read.extend(
        timeline
            .latest_user_read_receipt_timeline_event_id(&own)
            .await
            .map(|id| id.to_string()),
    );
    // What the account wrote itself, it has read up to.
    read.extend(last_own(&items));
    let slots = slots(&items, &own, &ignored.borrow());
    let read: Vec<&str> = read.iter().map(String::as_str).collect();
    Ok(unread_text(
        &slots,
        from.as_deref(),
        &read,
        newest,
        &chrono::Local,
    ))
}

/// The text of what is unread in `slots` (a room's timeline, oldest first).
///
/// Unread is what comes from the message `from` on (the app's own "new" line).
/// Without it: what comes after the last sign that the account has read up to there,
/// which is the SDK's line and each event in `read` (the read marker, the read
/// receipt, the account's own last message). An event the app shows nothing of still marks its place: what is unread
/// starts at the next message after it.
///
/// With no such place among what is loaded nothing is known: the answer then holds
/// the `newest` messages, read or not, and says so.
fn unread_text<Tz: chrono::TimeZone>(
    slots: &[Slot],
    from: Option<&str>,
    read: &[&str],
    newest: usize,
    zone: &Tz,
) -> UnreadText
where
    Tz::Offset: std::fmt::Display,
{
    let place = |id: &str| slots.iter().position(|slot| slot.id.as_deref() == Some(id));
    let start = from.and_then(place).or_else(|| {
        let line = slots.iter().rposition(|slot| slot.line);
        let read = read.iter().filter_map(|id| place(id));
        line.into_iter().chain(read).max().map(|at| at + 1)
    });
    let mut lines: Vec<String> = slots[start.unwrap_or(0)..]
        .iter()
        .filter_map(|slot| collect::entry(slot.message.as_ref()?))
        .map(|entry| collect::summary_line(zone, &entry))
        .collect();
    if start.is_none() {
        lines.drain(..lines.len().saturating_sub(newest));
    }
    let (text, kept) = collect::summary_text(&lines);
    UnreadText {
        text,
        messages: kept,
        cut: kept < lines.len(),
        known: start.is_some(),
    }
}

/// Sets its flag when dropped: the task that held it was given up.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Loads the messages of `room` as far back as `span` says and writes them into a new
/// file in `dir`.
async fn archive(
    room: &Room,
    span: Span,
    held: usize,
    dir: PathBuf,
    room_name: String,
    ignored: &watch::Receiver<IgnoredSet>,
    progress: &Progress<'_>,
) -> Result<Archived> {
    let now = chrono::Local::now();
    // The file first: a folder that cannot be written to is said at once, not after
    // the whole room was loaded. Nothing of it stays if this is given up.
    let name = crate::archive::file_name(&room_name, now.date_naive());
    let file = tokio::task::spawn_blocking(move || crate::files::NewFile::create(&dir, &name))
        .await
        .map_err(|_| anyhow!("could not write to the folder"))?
        .map_err(|err| anyhow!("could not write to the folder: {err}"))?;
    let timeline = open(room).await?;
    let since = span.since(u64::try_from(now.timestamp_millis()).unwrap_or(0));
    let (items, cut) = load(&timeline, ARCHIVE_EVENTS, progress, |items| match span {
        // As much as the app shows of the room, which it has loaded already.
        Span::Loaded => event_count(items) >= held,
        Span::Days(_) => since.is_some_and(|since| reaches_before(items, since)),
        Span::Everything => false,
    })
    .await;
    let items = without_repeats(items, event_key, undecryptable);
    progress.tell(event_count(&items));
    let own = room.own_user_id().to_owned();
    let ignored = ignored.borrow().clone();
    let stop = Arc::new(AtomicBool::new(false));
    // Given up while the file is written: the writing ends at the next message.
    let _guard = StopOnDrop(stop.clone());
    let written = tokio::task::spawn_blocking(move || {
        let messages = || messages_of(&items, &own, &ignored);
        write(file, messages, since, cut, &room_name, &now, &stop)
    })
    .await
    .map_err(|_| anyhow!("writing the file failed"))?;
    let (path, messages) = written.map_err(|err| anyhow!("could not write the file: {err}"))?;
    Ok(Archived {
        path,
        messages,
        cut,
    })
}

/// The items of a timeline as the messages the app would show, one by one.
fn messages_of<'a>(
    items: &'a Items,
    own: &'a UserId,
    ignored: &'a std::collections::HashSet<OwnedUserId>,
) -> impl Iterator<Item = Message> + 'a {
    let now = chrono::Local::now();
    items.iter().filter_map(move |item| {
        let event = item.as_event()?;
        super::convert::event_message(item.unique_id().0.clone(), event, own, ignored, &now)
    })
}

/// Writes the archive of `messages` (gone through twice, oldest first) into `file`
/// and gives it its name. Gives the file's path and how many messages are in it.
/// Ends early, leaving no file, when `stop` is set.
///
/// With `since`, the oldest messages are left out as long as they were sent before
/// it. Only those: a message between newer ones stays whatever time it claims, so
/// nobody can keep their message out of an archive with a forged date.
fn write<Tz: chrono::TimeZone, I: Iterator<Item = Message>>(
    file: crate::files::NewFile,
    messages: impl Fn() -> I,
    since: Option<u64>,
    cut: Option<Cut>,
    room_name: &str,
    now: &chrono::DateTime<Tz>,
    stop: &AtomicBool,
) -> std::io::Result<(PathBuf, usize)>
where
    Tz::Offset: std::fmt::Display,
{
    let too_old =
        |message: &Message| since.is_some_and(|since| message.sent_at.is_none_or(|at| at < since));
    let entries = || {
        messages()
            .skip_while(|message| too_old(message))
            .filter_map(|message| collect::entry(&message))
    };
    // Once through for the days the heading names; nothing is kept of it.
    let (mut first, mut last) = (None, None);
    for at in entries().filter_map(|entry| entry.at) {
        first = Some(first.map_or(at, |first: u64| first.min(at)));
        last = Some(last.map_or(at, |last: u64| last.max(at)));
    }
    let given_up = || std::io::Error::other("given up");
    let mut writer = crate::archive::Writer::new(file, now.timezone());
    writer.header(room_name, now, first, last, cut)?;
    for entry in entries() {
        if stop.load(Ordering::Relaxed) {
            return Err(given_up());
        }
        writer.entry(&entry)?;
    }
    let (file, count) = writer.finish();
    // Once more before the file gets its name: after that it is there to stay.
    if stop.load(Ordering::Relaxed) {
        return Err(given_up());
    }
    Ok((file.finish()?, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_that_reached_the_timeline_twice_counts_once() {
        // The shape the server test met: a block of four, again right after itself.
        let items: Vector<(Option<u32>, &str)> = [
            (None, "date"),
            (Some(470), "a"),
            (Some(471), "b"),
            (Some(472), "c"),
            (Some(471), "b again"),
            (Some(472), "c again"),
            (None, "marker"),
            (None, "echo"),
            (Some(473), "d"),
        ]
        .into_iter()
        .collect();
        let kept: Vec<&str> = without_repeats(items, |item| item.0, |_| false)
            .into_iter()
            .map(|item| item.1)
            .collect();
        assert_eq!(
            kept,
            ["date", "a", "b again", "c again", "marker", "echo", "d"]
        );
    }

    #[test]
    fn of_an_event_twice_the_later_copy_stays_where_it_is() {
        // The SDK puts the read marker after the copy that its later updates reach
        // (its search for the marker's place starts at the back): read up to b, and
        // c is not read. The later copies also carry edits and decryptions.
        let items: Vector<(Option<u32>, &str)> = [
            (Some(1), "a"),
            (Some(2), "b"),
            (Some(3), "c"),
            (Some(2), "b'"),
            (None, "read marker"),
            (Some(3), "c'"),
            (Some(4), "d"),
        ]
        .into_iter()
        .collect();
        let kept: Vec<&str> = without_repeats(items, |item| item.0, |_| false)
            .into_iter()
            .map(|item| item.1)
            .collect();
        assert_eq!(kept, ["a", "b'", "read marker", "c'", "d"]);
    }

    /// Keys that came in later (from a key file) decrypted the earlier copy of an event
    /// held twice: the later copy, still locked, keeps its place but not its content.
    #[test]
    fn of_an_event_twice_a_decrypted_copy_is_not_given_up_for_a_locked_one() {
        let items: Vector<(Option<u32>, &str, bool)> = [
            (Some(1), "a readable", false),
            (Some(2), "b locked", true),
            (Some(3), "c", false),
            (Some(1), "a locked", true),
            (None, "read marker", false),
            (Some(2), "b readable", false),
        ]
        .into_iter()
        .collect();
        let kept: Vec<&str> = without_repeats(items, |item| item.0, |item| item.2)
            .into_iter()
            .map(|item| item.1)
            .collect();
        assert_eq!(kept, ["c", "a readable", "read marker", "b readable"]);
        // The read marker between the copies: the decrypted one takes the later place,
        // after the marker, where the SDK put it; it is not moved back before it.
        let items: Vector<(Option<u32>, &str, bool)> = [
            (Some(1), "a readable", false),
            (Some(2), "b", false),
            (None, "read marker", false),
            (Some(1), "a locked", true),
            (Some(3), "c", false),
        ]
        .into_iter()
        .collect();
        let kept: Vec<&str> = without_repeats(items, |item| item.0, |item| item.2)
            .into_iter()
            .map(|item| item.1)
            .collect();
        assert_eq!(kept, ["b", "read marker", "a readable", "c"]);
    }
    use crate::collect::tests::{AT, said};
    use crate::files::NewFile;
    use chrono::Utc;

    fn with_id(id: &str, sender: &str, body: &str) -> Slot {
        Slot {
            id: Some(id.to_owned()),
            line: false,
            message: Some(said(sender, body)),
        }
    }

    /// An event the app shows nothing of (of someone ignored, say).
    fn hidden(id: &str) -> Slot {
        Slot {
            id: Some(id.to_owned()),
            line: false,
            message: None,
        }
    }

    fn line() -> Slot {
        Slot {
            id: None,
            line: true,
            message: None,
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("matrix-rat-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn unread_is_what_comes_from_the_apps_new_line_on() {
        let slots = [
            with_id("$1", "Ann", "old"),
            with_id("$2", "Bob", "new one"),
            line(),
            with_id("$3", "Ann", "new two"),
        ];
        // The app's own line wins over the SDK's and over the read marker.
        let unread = unread_text(&slots, Some("$2"), &["$3"], 50, &Utc);
        assert_eq!(
            unread.text,
            "[2026-11-20 14:03] Bob: new one\n[2026-11-20 14:03] Ann: new two\n"
        );
        assert_eq!(
            (unread.messages, unread.cut, unread.known),
            (2, false, true)
        );
    }

    #[test]
    fn without_it_the_last_sign_of_reading_decides() {
        let slots = [
            with_id("$1", "Ann", "old"),
            line(),
            with_id("$2", "Bob", "new one"),
            with_id("$3", "Ann", "new two"),
        ];
        let unread = unread_text(&slots, None, &["$1"], 50, &Utc);
        assert_eq!(unread.messages, 2);
        assert!(unread.known);
        assert!(unread.text.starts_with("[2026-11-20 14:03] Bob: new one\n"));
        // An event the app named but that is not there: the same.
        assert_eq!(unread_text(&slots, Some("$9"), &[], 50, &Utc), unread);

        // No line (the SDK shows none): after the message the marker is at.
        let slots = [
            with_id("$1", "Ann", "old"),
            with_id("$2", "Bob", "read"),
            with_id("$3", "Ann", "new"),
        ];
        let unread = unread_text(&slots, None, &["$2"], 50, &Utc);
        assert_eq!(unread.text, "[2026-11-20 14:03] Ann: new\n");
        assert!(unread.known && !unread.cut);
        // A read receipt further on than the read marker: what was read is read.
        assert_eq!(unread_text(&slots, None, &["$1", "$2"], 50, &Utc), unread);
        assert_eq!(unread_text(&slots, None, &["$2", "$1"], 50, &Utc), unread);
        // The marker at the newest message: nothing is unread, and that is known.
        let unread = unread_text(&slots, None, &["$3"], 50, &Utc);
        assert_eq!(
            (unread.text.as_str(), unread.messages, unread.known),
            ("", 0, true)
        );
    }

    #[test]
    fn a_marked_event_that_is_not_shown_still_marks_its_place() {
        let slots = [
            with_id("$1", "Ann", "old"),
            hidden("$2"),
            with_id("$3", "Bob", "new"),
        ];
        for unread in [
            unread_text(&slots, None, &["$2"], 50, &Utc),
            unread_text(&slots, Some("$2"), &[], 50, &Utc),
        ] {
            assert_eq!(unread.text, "[2026-11-20 14:03] Bob: new\n");
            assert!(unread.known);
        }
    }

    #[test]
    fn where_reading_stopped_is_not_found_only_the_newest_are_taken_and_it_says_so() {
        let slots = [
            with_id("$1", "Ann", "one"),
            with_id("$2", "Bob", "two"),
            with_id("$3", "Ann", "three"),
        ];
        // The marker is at an event that is not among what is loaded.
        let unread = unread_text(&slots, None, &["$0"], 2, &Utc);
        assert!(!unread.known);
        assert_eq!(unread.messages, 2);
        assert!(unread.text.starts_with("[2026-11-20 14:03] Bob: two\n"));
        // Nothing says how far the account has read: the same.
        assert!(!unread_text(&slots, None, &[], 50, &Utc).known);
        assert_eq!(unread_text(&slots, None, &[], 50, &Utc).messages, 3);
    }

    #[test]
    fn more_unread_than_a_summary_takes_is_cut_to_the_newest() {
        let mut slots = vec![with_id("$read", "Ann", "read")];
        slots.extend(
            (0..collect::SUMMARY_MESSAGES + 5)
                .map(|n| with_id(&format!("${n}"), "Bob", &format!("m{n}"))),
        );
        let unread = unread_text(&slots, None, &["$read"], 50, &Utc);
        assert_eq!(unread.messages, collect::SUMMARY_MESSAGES);
        assert!(unread.cut && unread.known);
        assert!(unread.text.starts_with("[2026-11-20 14:03] Bob: m5\n"));
    }

    #[test]
    fn an_archive_is_written_to_a_new_private_file_with_the_days_it_covers() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("archive-write");
        let day = 86_400_000;
        let mut old = said("Ann", "too old");
        old.sent_at = Some(AT - 10 * day);
        let mut first = said("Ann", "# hello");
        first.sent_at = Some(AT - 2 * day);
        let marker = Message {
            kind: crate::model::MsgKind::ReadMarker,
            ..Message::default()
        };
        let messages = vec![old, first, marker, said("Bob", "bye")];
        let now = collect::local(&Utc, AT).unwrap();
        let stop = AtomicBool::new(false);
        let name = crate::archive::file_name("Plans / 2026", now.date_naive());
        let (path, count) = write(
            NewFile::create(&dir, &name).unwrap(),
            || messages.clone().into_iter(),
            Some(AT - 7 * day),
            None,
            "Plans / 2026",
            &now,
            &stop,
        )
        .unwrap();
        assert_eq!(path, dir.join("Plans-2026-2026-11-20.md"));
        assert_eq!(count, 2);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# Plans / 2026\n\n"), "{text}");
        assert!(
            text.contains("Messages from 2026-11-18 to 2026-11-20.\n"),
            "{text}"
        );
        assert!(
            text.contains("Times are local time (UTC+00:00).\n"),
            "{text}"
        );
        assert!(text.contains("Text only:"), "{text}");
        assert!(
            text.contains("## 2026-11-18, Wednesday\n\n**14:03 Ann**\\\n\\# hello\n"),
            "{text}"
        );
        assert!(text.ends_with("**14:03 Bob**\\\nbye\n\n"), "{text}");
        assert!(!text.contains("too old"), "{text}");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        // Again: beside the first, which stays as it is.
        let (second, _) = write(
            NewFile::create(&dir, &name).unwrap(),
            || messages.clone().into_iter(),
            None,
            Some(Cut::Limit),
            "Plans / 2026",
            &now,
            &stop,
        )
        .unwrap();
        assert_eq!(second, dir.join("Plans-2026-2026-11-20 (1).md"));
        let all = std::fs::read_to_string(&second).unwrap();
        assert!(
            all.contains("too old") && all.contains("Not complete:"),
            "{all}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_forged_old_date_keeps_no_message_out_of_an_archive_of_the_last_days() {
        let dir = temp("archive-forged");
        let day = 86_400_000;
        let at = |body: &str, at: u64| {
            let mut message = said("Ann", body);
            message.sent_at = Some(at);
            message
        };
        let messages = vec![
            at("really old", AT - 20 * day),
            at("also old", AT - 19 * day),
            at("recent", AT - day),
            // Between newer ones, whatever it claims.
            at("claims to be from 1970", 1),
            at("newest", AT),
        ];
        let now = collect::local(&Utc, AT).unwrap();
        let stop = AtomicBool::new(false);
        let (path, count) = write(
            NewFile::create(&dir, "forged.md").unwrap(),
            || messages.clone().into_iter(),
            Some(AT - 7 * day),
            None,
            "Plans",
            &now,
            &stop,
        )
        .unwrap();
        assert_eq!(count, 3);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("claims to be from 1970"), "{text}");
        assert!(text.contains("recent") && text.contains("newest"), "{text}");
        assert!(!text.contains("old\n"), "{text}");
        // The days it covers are the oldest and the newest in it, in whatever order.
        assert!(
            text.contains("Messages from 1970-01-01 to 2026-11-20.\n"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_archive_that_is_given_up_leaves_no_file() {
        let dir = temp("archive-stop");
        let now = collect::local(&Utc, AT).unwrap();
        let stop = AtomicBool::new(true);
        let messages = vec![said("Ann", "one")];
        assert!(
            write(
                NewFile::create(&dir, "Plans.md").unwrap(),
                || messages.clone().into_iter(),
                None,
                None,
                "Plans",
                &now,
                &stop
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        // Given up after the last message, before the file has its name: the same.
        assert!(
            write(
                NewFile::create(&dir, "Plans.md").unwrap(),
                std::iter::empty,
                None,
                None,
                "Plans",
                &now,
                &stop
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_room_gives_an_archive_that_says_so() {
        let dir = temp("archive-empty");
        let now = collect::local(&Utc, AT).unwrap();
        let stop = AtomicBool::new(false);
        let (path, count) = write(
            NewFile::create(&dir, "Plans.md").unwrap(),
            std::iter::empty,
            None,
            None,
            "Plans",
            &now,
            &stop,
        )
        .unwrap();
        assert_eq!(count, 0);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("No messages.\n")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_that_cannot_be_written_to_is_said_before_anything_is_loaded() {
        // What `archive` does first, and what it then says.
        let dir = temp("archive-no-folder");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("in-the-way"), "a file, not a folder").unwrap();
        assert!(NewFile::create(&dir.join("in-the-way"), "Plans.md").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum FakeItem {
        Event(u32),
        Divider(u32),
        Start,
    }

    impl LoadItem for FakeItem {
        type Key = Self;

        fn is_event(&self) -> bool {
            matches!(self, Self::Event(_))
        }

        fn is_start(&self) -> bool {
            matches!(self, Self::Start)
        }

        fn key(&self) -> &Self::Key {
            self
        }
    }

    fn fake_events(range: std::ops::Range<u32>) -> Vector<FakeItem> {
        range.map(FakeItem::Event).collect()
    }

    fn push_front_events(range: std::ops::Range<u32>) -> Vec<VectorDiff<FakeItem>> {
        // Applied in order, pushing newest-of-batch first leaves `range` oldest-first.
        range
            .rev()
            .map(|id| VectorDiff::PushFront {
                value: FakeItem::Event(id),
            })
            .collect()
    }

    fn diff_stream(
        rx: tokio::sync::mpsc::UnboundedReceiver<Vec<VectorDiff<FakeItem>>>,
    ) -> impl futures::Stream<Item = Vec<VectorDiff<FakeItem>>> {
        futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|diffs| (diffs, rx))
        })
    }

    fn fast_settle() -> SettleTimes {
        SettleTimes {
            page: Duration::from_millis(500),
            more: Duration::from_millis(250),
        }
    }

    #[tokio::test]
    async fn loading_waits_for_the_cache_batch_after_a_short_lazy_page_and_does_not_overshoot() {
        // Reproduces a lazy-to-cache boundary (0 < skip_count < PAGE) followed by
        // 128-event disk chunks, with the read position at event 0. Without waiting
        // for the first call's follow-up disk chunk, a second page would be fired
        // while the first is still in flight and load past UNREAD_EVENTS into event 0.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let asked_in = asked.clone();

        let (items, cut) = load_from(
            fake_events(440..460),
            diff_stream(rx),
            UNREAD_EVENTS,
            |_| {},
            |items| items.iter().any(|item| item == &FakeItem::Event(0)),
            move |ask| {
                let tx = tx.clone();
                let call = {
                    let mut asked = asked_in.lock().unwrap();
                    asked.push(ask);
                    asked.len()
                };
                async move {
                    match call {
                        1 => {
                            // Lazy skip tail arrives at once (< ask); disk chunk arrives later.
                            let _ = tx.send(push_front_events(410..440));
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::from_millis(15)).await;
                                let _ = tx.send(push_front_events(282..410));
                            });
                            Some(false)
                        }
                        2 => {
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::from_millis(15)).await;
                                let _ = tx.send(push_front_events(154..282));
                            });
                            Some(false)
                        }
                        3 => {
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::from_millis(15)).await;
                                let _ = tx.send(push_front_events(26..154));
                            });
                            Some(false)
                        }
                        4 => {
                            // Reaching a 4th page would pull in the read marker at event 0.
                            let _ = tx.send(push_front_events(0..26));
                            Some(true)
                        }
                        _ => panic!("unexpected page {call}"),
                    }
                }
            },
            fast_settle(),
        )
        .await;

        assert_eq!(cut, Some(Cut::Limit));
        assert_eq!(*asked.lock().unwrap(), vec![100, 100, 44]);
        assert!(!items.iter().any(|item| item == &FakeItem::Event(0)));
        assert_eq!(items.len(), 434);
        assert_eq!(items.front(), Some(&FakeItem::Event(26)));
        assert_eq!(items.back(), Some(&FakeItem::Event(459)));
    }

    #[tokio::test]
    async fn loading_ignores_live_arrivals_and_updates_while_waiting_for_a_page() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let asked_in = asked.clone();

        let (items, cut) = load_from(
            fake_events(100..120),
            diff_stream(rx),
            120,
            |_| {},
            |_| false,
            move |ask| {
                let tx = tx.clone();
                asked_in.lock().unwrap().push(ask);
                async move {
                    let _ = tx.send(vec![VectorDiff::PushBack {
                        value: FakeItem::Event(120),
                    }]);
                    let _ = tx.send(vec![VectorDiff::Set {
                        index: 0,
                        value: FakeItem::Event(100),
                    }]);
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(15)).await;
                        let _ = tx.send(push_front_events(0..100));
                    });
                    Some(false)
                }
            },
            fast_settle(),
        )
        .await;

        assert_eq!(cut, Some(Cut::Limit));
        assert_eq!(*asked.lock().unwrap(), vec![100]);
        assert_eq!(items.len(), 121);
        assert_eq!(items.front(), Some(&FakeItem::Event(0)));
        assert_eq!(items.back(), Some(&FakeItem::Event(120)));
    }

    #[tokio::test]
    async fn loading_keeps_the_oldest_page_when_timeline_start_arrives_first() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        let (items, cut) = load_from(
            fake_events(30..50),
            diff_stream(rx),
            500,
            |_| {},
            |_| false,
            move |_| {
                let tx = tx.clone();
                async move {
                    let _ = tx.send(vec![VectorDiff::PushFront {
                        value: FakeItem::Start,
                    }]);
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(15)).await;
                        let diffs = (0..30)
                            .map(|id| VectorDiff::Insert {
                                index: usize::try_from(id).unwrap() + 1,
                                value: FakeItem::Event(id),
                            })
                            .collect();
                        let _ = tx.send(diffs);
                    });
                    Some(true)
                }
            },
            fast_settle(),
        )
        .await;

        assert_eq!(cut, None);
        assert_eq!(items.front(), Some(&FakeItem::Start));
        let events: Vec<u32> = items
            .iter()
            .filter_map(|item| match item {
                FakeItem::Event(id) => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(events, (0..50).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn loading_keeps_the_oldest_page_across_a_lazy_boundary_at_the_start() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let asked_in = asked.clone();

        let (items, cut) = load_from(
            fake_events(40..60),
            diff_stream(rx),
            500,
            |_| {},
            |_| false,
            move |ask| {
                let tx = tx.clone();
                asked_in.lock().unwrap().push(ask);
                async move {
                    // Lazy tail first, then TimelineStart, then the final cache chunk.
                    let _ = tx.send(push_front_events(30..40));
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        let _ = tx.send(vec![VectorDiff::PushFront {
                            value: FakeItem::Start,
                        }]);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        let diffs = (0..30)
                            .map(|id| VectorDiff::Insert {
                                index: usize::try_from(id).unwrap() + 1,
                                value: FakeItem::Event(id),
                            })
                            .collect();
                        let _ = tx.send(diffs);
                    });
                    Some(true)
                }
            },
            fast_settle(),
        )
        .await;

        assert_eq!(cut, None);
        assert_eq!(*asked.lock().unwrap(), vec![100]);
        assert_eq!(items.front(), Some(&FakeItem::Start));
        let events: Vec<u32> = items
            .iter()
            .filter_map(|item| match item {
                FakeItem::Event(id) => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(events, (0..60).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn loading_clamps_page_size_near_the_limit_with_a_minimum_floor() {
        // 30 remaining -> asks for 30, not PAGE (100).
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let asked_in = asked.clone();
        let (items, cut) = load_from(
            fake_events(30..350),
            diff_stream(rx),
            UNREAD_EVENTS,
            |_| {},
            |_| false,
            move |ask| {
                let tx = tx.clone();
                asked_in.lock().unwrap().push(ask);
                async move {
                    let count = u32::from(ask);
                    let _ = tx.send(push_front_events((30 - count)..30));
                    Some(false)
                }
            },
            fast_settle(),
        )
        .await;
        assert_eq!(cut, Some(Cut::Limit));
        assert_eq!(*asked.lock().unwrap(), vec![30]);
        assert_eq!(items.len(), UNREAD_EVENTS);

        // 5 remaining -> floored at MIN_PAGE (20).
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let asked_in = asked.clone();
        let (_, cut) = load_from(
            fake_events(20..365),
            diff_stream(rx),
            UNREAD_EVENTS,
            |_| {},
            |_| false,
            move |ask| {
                let tx = tx.clone();
                asked_in.lock().unwrap().push(ask);
                async move {
                    let _ = tx.send(push_front_events(0..u32::from(ask)));
                    Some(false)
                }
            },
            fast_settle(),
        )
        .await;
        assert_eq!(cut, Some(Cut::Limit));
        assert_eq!(*asked.lock().unwrap(), vec![MIN_PAGE]);
    }

    #[tokio::test]
    async fn loading_tracks_prepended_events_when_a_front_date_divider_is_replaced() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut initial = Vector::new();
        initial.push_back(FakeItem::Divider(1));
        for id in 50..70 {
            initial.push_back(FakeItem::Event(id));
        }

        let started = tokio::time::Instant::now();
        let (items, cut) = load_from(
            initial,
            diff_stream(rx),
            50,
            |_| {},
            |_| false,
            move |_| {
                let tx = tx.clone();
                async move {
                    // DateDividerAdjuster removes the front divider, prepends the page
                    // of 30 events and inserts a new front divider: net +30 items.
                    let mut diffs = vec![VectorDiff::Remove { index: 0 }];
                    diffs.extend(push_front_events(20..50));
                    diffs.push(VectorDiff::PushFront {
                        value: FakeItem::Divider(0),
                    });
                    let _ = tx.send(diffs);
                    Some(false)
                }
            },
            SettleTimes {
                page: Duration::from_secs(5),
                more: Duration::from_secs(5),
            },
        )
        .await;

        assert_eq!(cut, Some(Cut::Limit));
        assert!(started.elapsed() < Duration::from_secs(1));
        let events: Vec<u32> = items
            .iter()
            .filter_map(|item| match item {
                FakeItem::Event(id) => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(events, (20..70).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn loading_stops_at_once_when_pagination_fails() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let started = tokio::time::Instant::now();

        let (items, cut) = load_from(
            fake_events(0..20),
            diff_stream(rx),
            100,
            |_| {},
            |_| false,
            |_| async { None },
            SettleTimes {
                page: Duration::from_secs(5),
                more: Duration::from_secs(5),
            },
        )
        .await;

        assert_eq!(cut, Some(Cut::LoadFailed));
        assert_eq!(items.len(), 20);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// Against a real homeserver, through the worker, with two accounts: the test
    /// account (ours) and a second one that writes. In a fresh room of theirs:
    ///
    /// 1. "What is loaded now" of an open room that shows more than a timeline of
    ///    its own starts with: the archive holds every message the app holds.
    /// 2. With a known read position, what is unread is exactly what came after it.
    /// 3. With more unread than a summary holds: the newest unread, cut, where the
    ///    read position is still among what the SDK holds; otherwise the answer says
    ///    that it is not known what is unread, and holds only the newest asked for.
    ///
    /// `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=alicepass cargo test -- --ignored`
    /// (the second account: `MATRIX_RAT_TEST_USER2` and `MATRIX_RAT_TEST_PASSWORD2`,
    /// by default bob and bobpass). The third part sends several hundred messages.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn an_open_room_is_archived_whole_and_unread_is_what_came_after_reading_or_not_known() {
        use matrix_sdk::Client;
        use matrix_sdk::config::SyncSettings;
        use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoom;
        use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
        use tokio::sync::mpsc::UnboundedReceiver;

        use crate::matrix::{MatrixCmd, RoomCategory, spawn};
        use crate::model::MsgKind;

        let var = |name: &str| std::env::var(name).ok();
        let homeserver =
            var("MATRIX_RAT_TEST_HOMESERVER").unwrap_or_else(|| "http://127.0.0.1:6167".into());
        let username = var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let other_name = var("MATRIX_RAT_TEST_USER2").unwrap_or_else(|| "bob".into());
        let other_password = var("MATRIX_RAT_TEST_PASSWORD2").unwrap_or_else(|| "bobpass".into());
        let run = std::process::id();
        let within = Duration::from_secs;

        let other = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .expect("client");
        other
            .matrix_auth()
            .login_username(&other_name, &other_password)
            .initial_device_display_name("matrix-rat collect test")
            .send()
            .await
            .expect("login of the second user");
        other
            .sync_once(SyncSettings::default())
            .await
            .expect("sync");

        let root = std::env::temp_dir().join(format!("matrix-rat-collect-{run}"));
        let _ = std::fs::remove_dir_all(&root);
        let paths = crate::paths::Paths {
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

        // A room of the other's, without encryption (theirs is a plain client), that
        // we are invited to and join.
        let room_name = format!("collect test {run}");
        let mut create = CreateRoom::new();
        create.invite = vec![UserId::parse(&own_id).expect("a user id")];
        create.name = Some(room_name.clone());
        let other_room = other.create_room(create).await.expect("room");
        let room_id = other_room.room_id().to_string();
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

        let say = |body: String| {
            let other_room = other_room.clone();
            async move {
                other_room
                    .send(RoomMessageEventContent::text_plain(body))
                    .await
                    .expect("message of the second user");
            }
        };
        // The answer to a collection, whatever else the worker says meanwhile.
        async fn answer(events: &mut UnboundedReceiver<MatrixEvent>, request: u64) -> CollectEvent {
            let wait = async {
                loop {
                    let MatrixEvent::Collect(event) = events.recv().await.expect("worker stopped")
                    else {
                        continue;
                    };
                    let of = match &event {
                        CollectEvent::Unread { request, .. }
                        | CollectEvent::Archived { request, .. } => Some(*request),
                        CollectEvent::Progress { .. } => None,
                    };
                    if of == Some(request) {
                        return event;
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(120), wait)
                .await
                .expect("the collection is answered")
        }
        // The texts of the lines handed to a summary command.
        fn bodies(unread: &UnreadText) -> Vec<String> {
            unread
                .text
                .lines()
                .map(|line| line.rsplit(": ").next().unwrap_or_default().to_owned())
                .collect()
        }
        fn shape(unread: &UnreadText) -> String {
            let got = bodies(unread);
            format!(
                "known={}, messages={}, cut={}, first={:?}, last={:?}",
                unread.known,
                unread.messages,
                unread.cut,
                got.first(),
                got.last(),
            )
        }
        // What is unread, asked again until `done` accepts it (our sync may be behind
        // what the other one just sent).
        async fn unread_until(
            handle: &crate::matrix::MatrixHandle,
            events: &mut UnboundedReceiver<MatrixEvent>,
            room_id: &str,
            first_request: u64,
            newest: usize,
            done: impl Fn(&UnreadText) -> bool,
        ) -> UnreadText {
            let mut last = None;
            for attempt in 0..30 {
                let request = first_request + attempt;
                handle.send(MatrixCmd::Collect(CollectCmd::Unread {
                    request,
                    room_id: room_id.to_owned(),
                    from: None,
                    newest,
                }));
                let CollectEvent::Unread { result, .. } = answer(events, request).await else {
                    panic!("not the answer to what is unread");
                };
                let unread = result.expect("what is unread");
                if done(&unread) {
                    return unread;
                }
                last = Some(shape(&unread));
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            panic!("what is unread never came to what was expected; last: {last:?}");
        }

        // 1. More messages than a fresh timeline shows, all of them on our screen.
        let early: Vec<String> = (0..30).map(|n| format!("early-{run}-{n:02}")).collect();
        for body in &early {
            say(body.clone()).await;
        }
        handle.send(MatrixCmd::OpenRoom {
            room_id: room_id.clone(),
            generation: 1,
        });
        let shown = async {
            loop {
                let MatrixEvent::Timeline {
                    messages,
                    generation: 1,
                    ..
                } = events.recv().await.expect("worker stopped")
                else {
                    continue;
                };
                let has = |body: &String| messages.iter().any(|m| &m.body == body);
                if early.iter().all(has) {
                    return messages
                        .iter()
                        .filter(|m| m.kind != MsgKind::ReadMarker)
                        .count();
                }
                // Not all of them yet: older ones are asked for, as the app does
                // when one scrolls up.
                if has(&early[29]) {
                    handle.send(MatrixCmd::Paginate {
                        room_id: room_id.clone(),
                        generation: 1,
                    });
                }
            }
        };
        let held = tokio::time::timeout(within(60), shown)
            .await
            .expect("the open room shows all the messages");
        assert!(held >= 30, "{held}");
        let out = root.join("archives");
        handle.send(MatrixCmd::Collect(CollectCmd::Archive {
            request: 1,
            room_id: room_id.clone(),
            span: Span::Loaded,
            held,
            dir: out.clone(),
            room_name: room_name.clone(),
        }));
        let CollectEvent::Archived { result, .. } = answer(&mut events, 1).await else {
            panic!("not the answer to an archive");
        };
        let archived = result.expect("the archive is written");
        assert!(archived.path.starts_with(&out));
        assert!(archived.messages >= 30, "{}", archived.messages);
        let text = std::fs::read_to_string(&archived.path).expect("the archive");
        for body in &early {
            assert!(text.contains(body.as_str()), "{body} is not in the archive");
        }
        assert_eq!(archived.cut, None);

        // 2. We read everything; then three more come.
        handle.send(MatrixCmd::MarkRead {
            room_id: room_id.clone(),
            public: false,
        });
        // Reading is not answered: give it the time to reach the server, or what
        // comes next would be read along with the rest.
        tokio::time::sleep(within(2)).await;
        let news: Vec<String> = (0..3).map(|n| format!("news-{run}-{n}")).collect();
        for body in &news {
            say(body.clone()).await;
        }
        let unread = unread_until(&handle, &mut events, &room_id, 100, 50, |unread| {
            unread.known && unread.messages >= 3
        })
        .await;
        let info = shape(&unread);
        assert_eq!(bodies(&unread), news, "exactly what was not read ({info})");
        assert_eq!(unread.messages, news.len(), "unread count ({info})");
        assert!(!unread.cut, "unread should not be cut ({info})");

        // 3. So many more that the newest ones alone fill a summary. Whether the place
        // where we stopped reading is still among what the SDK holds depends on its
        // cache (an open room keeps what sync brought; `RoomEventCache` reloads stored
        // chunks whole, 128 events, whatever page was asked for), so either answer may
        // come, and each must be the truth:
        // - the place was found: exactly the newest unread, cut to `SUMMARY_MESSAGES`;
        // - it was not: that is said, and only the newest asked for are given.
        // What must never come is "known" with a place guessed among the flood.
        // (The unknown answer is pinned without a server by the tests of `unread_text`.)
        const STORE_CHUNK: usize = 128;
        let many = UNREAD_EVENTS + STORE_CHUNK + usize::from(MIN_PAGE) + 20;
        let flood: Vec<String> = (0..many).map(|n| format!("flood-{run}-{n:03}")).collect();
        for body in &flood {
            say(body.clone()).await;
        }
        let unread = unread_until(&handle, &mut events, &room_id, 200, 7, |unread| {
            unread.text.trim_end().ends_with(flood[many - 1].as_str())
        })
        .await;
        let info = shape(&unread);
        if unread.known {
            let newest = &flood[many - collect::SUMMARY_MESSAGES..];
            assert!(unread.cut, "more is unread than a summary holds ({info})");
            assert_eq!(
                unread.messages,
                collect::SUMMARY_MESSAGES,
                "a full summary ({info})"
            );
            assert_eq!(bodies(&unread), newest, "the newest unread ({info})");
        } else {
            let newest = &flood[many - 7..];
            assert!(!unread.cut, "newest slice should not be cut ({info})");
            assert_eq!(unread.messages, newest.len(), "message count ({info})");
            assert_eq!(bodies(&unread), newest, "newest bodies ({info})");
        }

        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = other.matrix_auth().logout().await;
        let _ = std::fs::remove_dir_all(root);
    }
}
