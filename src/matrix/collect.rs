//! Collecting a stretch of a room's messages as text: for an archive of the room,
//! written to a file here, and for a summary of what is unread or of the last
//! messages, handed to the app.
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
use matrix_sdk::ruma::events::fully_read::FullyReadEventContent;
use matrix_sdk::ruma::{OwnedUserId, UserId};
use matrix_sdk::{Room, RoomMemberships};
use matrix_sdk_ui::eyeball_im::{Vector, VectorDiff};
use matrix_sdk_ui::timeline::{
    RoomExt, Timeline, TimelineFocus, TimelineItem, TimelineReadReceiptTracking, TimelineUniqueId,
    VirtualTimelineItem,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::MatrixEvent;
use super::paging::{self, PageError};
use super::safety::IgnoredSet;
use crate::collect::{self, Cut, Span};
use crate::logsafe;
use crate::model::{Message, Namesakes};

/// The most events one archive takes; of a room with more, the newest.
pub const ARCHIVE_EVENTS: usize = 5_000;
/// How many events are loaded at most to find where the user stopped reading.
const UNREAD_EVENTS: usize = collect::SUMMARY_MESSAGES + 50;
/// How many events are loaded at most for the last messages asked for (`/catchup N`).
/// More than [`UNREAD_EVENTS`]: the bound counts every event loaded, also those that
/// are no message to summarise (state events that are not shown, those of ignored
/// people, the second copy of one the SDK delivered twice), and as many as a summary
/// holds may be asked for. A room of little else still ends somewhere.
pub const LAST_EVENTS: usize = 2 * collect::SUMMARY_MESSAGES;
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
    /// The last `count` messages of a joined room as text for a summary, read or not
    /// (`/catchup N`); answered with [`CollectEvent::Unread`] too.
    Last {
        request: u64,
        room_id: String,
        count: usize,
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
            Self::Last {
                request: r,
                room_id,
                count,
            } => (
                "Last",
                vec![request(r), id(room_id), Some(format!("{count} messages"))],
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

/// What is unread in a room, or its last messages, as text.
#[derive(Clone, PartialEq, Eq)]
pub struct UnreadText {
    /// One message a line, oldest first (see [`collect::summary_line`]).
    pub text: String,
    /// How many messages are in it.
    pub messages: usize,
    /// There is more unread (or more of the last messages asked for) than a summary
    /// holds: these are the newest.
    pub cut: bool,
    /// Where the user stopped reading was found, so these are unread ones. Without
    /// it they are the newest messages of the room, read or not.
    pub known: bool,
    /// Why loading ended before it found what it looked for, if it did: the room may
    /// have older messages than were looked at. `None`: it found it, or reached the
    /// room's first message.
    pub stopped: Option<Cut>,
}

/// Hand-written: the text is what people wrote.
impl std::fmt::Debug for UnreadText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnreadText")
            .field("messages", &self.messages)
            .field("cut", &self.cut)
            .field("known", &self.known)
            .field("stopped", &self.stopped)
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
    /// Outcome of [`CollectCmd::Unread`] and [`CollectCmd::Last`]; the reason of a
    /// failure is for the user.
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
            } => self.summary_text(request, &room_id, Wanted::Unread { from, newest }),
            CollectCmd::Last {
                request,
                room_id,
                count,
            } => self.summary_text(request, &room_id, Wanted::Last(count)),
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

    /// Collects the text for a summary that `wanted` says, and answers with
    /// [`CollectEvent::Unread`].
    fn summary_text(&mut self, request: u64, room_id: &str, wanted: Wanted) {
        let room = super::agent::joined_room(&self.client, &self.left, room_id);
        let events = self.events.clone();
        let ignored = self.ignored.clone();
        self.running.spawn(async move {
            let work = async {
                let progress = Progress::new(&events, request);
                let room = room?;
                match wanted {
                    Wanted::Unread { from, newest } => {
                        unread(&room, from, newest, &ignored, &progress).await
                    }
                    Wanted::Last(count) => last(&room, count, &ignored, &progress).await,
                }
            };
            let result = match tokio::time::timeout(UNREAD_BUDGET, work).await {
                Ok(result) => result,
                Err(_) => Err(anyhow!("loading the messages took too long")),
            };
            if let Err(err) = &result {
                tracing::warn!(why = %logsafe::kind_any(err), "could not collect the text for a summary");
            }
            let _ = events.send(MatrixEvent::Collect(CollectEvent::Unread {
                request,
                result: result.map_err(|err| format!("{err:#}")),
            }));
        });
    }

    pub fn stop(&mut self) {
        self.running.abort_all();
        while self.running.try_join_next().is_some() {}
    }
}

/// What a summary is to be of (see [`CollectCmd::Unread`] and [`CollectCmd::Last`]).
enum Wanted {
    Unread { from: Option<String>, newest: usize },
    Last(usize),
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
        .with_internal_id_prefix(super::next_timeline_prefix())
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

/// `items` with each event once: matrix-sdk-ui 0.19.1 can hold an event in a live
/// timeline twice (its builder reads the event cache a second time after subscribing to
/// it, and what came in between is applied again; fixed upstream after 0.19.1, PR
/// #7248). The repeated copy lands wherever the replayed update points, which can be
/// after newer events (seen in the server test while a flood came in: three messages
/// again after forty newer ones). So each event keeps its first copy's place, as the
/// room view does (`super::convert::EventDedup`), with the content of its last copy,
/// which the SDK's later updates (an edit, a decryption) reach; but a copy still
/// `locked` never stands in for one that was decrypted (see
/// [`super::convert::takes_over`]). The SDK places the read marker after the marker's
/// event searching from the back: a `marker` that follows a later copy moves to just
/// after the copy kept. Items without a key (virtual ones, a local echo) all stay.
fn without_repeats<T, K: std::hash::Hash + Eq>(
    items: Vector<T>,
    key: impl Fn(&T) -> Option<K>,
    locked: impl Fn(&T) -> bool,
    marker: impl Fn(&T) -> bool,
) -> Vector<T>
where
    T: Clone,
{
    use std::collections::hash_map::Entry;
    let mut items: Vec<T> = items.into_iter().collect();
    // The place of each event's first copy; whether each place stays.
    let mut first = std::collections::HashMap::new();
    let mut stays = vec![true; items.len()];
    // The last event met (its place, and where its first copy is), and the read marker
    // to move (its place, and the place it goes after).
    let mut last_event = None;
    let mut moved_marker = None;
    for at in 0..items.len() {
        if moved_marker.is_none()
            && marker(&items[at])
            && let Some((event, kept)) = last_event
            && kept < event
        {
            moved_marker = Some((at, kept));
        }
        let Some(key) = key(&items[at]) else {
            continue;
        };
        match first.entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(at);
                last_event = Some((at, at));
            }
            Entry::Occupied(slot) => {
                let kept = *slot.get();
                stays[at] = false;
                last_event = Some((at, kept));
                if super::convert::takes_over(locked(&items[kept]), locked(&items[at]), true) {
                    items.swap(kept, at);
                }
            }
        }
    }
    if let Some((at, _)) = moved_marker {
        stays[at] = false;
    }
    let mut out = Vector::new();
    for (at, item) in items.iter().enumerate() {
        if stays[at] {
            out.push_back(item.clone());
        }
        if let Some((from, after)) = moved_marker
            && after == at
        {
            out.push_back(items[from].clone());
        }
    }
    out
}

/// The event id of a timeline item, for [`without_repeats`].
fn event_key(item: &Arc<TimelineItem>) -> Option<String> {
    Some(item.as_event()?.event_id()?.to_string())
}

/// Whether a timeline item is the account's read marker, for [`without_repeats`].
fn read_marker_item(item: &Arc<TimelineItem>) -> bool {
    matches!(item.as_virtual(), Some(VirtualTimelineItem::ReadMarker))
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
            match tokio::time::timeout(PAGE_TIMEOUT, paging::backwards(timeline, ask)).await {
                Ok(Ok(reached_start)) => Some(reached_start),
                // All caches were cleared after a panic: the timeline's reset comes through
                // its changes like any page, and the next page, of the cleared caches, is
                // asked for from what is there then. A second panic does not clear them
                // again (see `paging::backwards`), and ends the collection.
                Ok(Err(PageError::Panicked { cleared: true })) => Some(false),
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
    let (items, stopped) = load(&timeline, UNREAD_EVENTS, progress, |items| {
        has_marker(items)
            || from.iter().chain(&read).any(|id| has_event(items, id))
            // The app's line, where it has one, is looked for beyond our own messages.
            || (from.is_none() && last_own(items).is_some())
    })
    .await;
    #[cfg(test)]
    probe::record(&items, room).await;
    let items = without_repeats(items, event_key, undecryptable, read_marker_item);
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
    let members = member_names(room).await;
    Ok(UnreadText {
        stopped,
        ..unread_text(
            &slots,
            &members,
            from.as_deref(),
            &read,
            newest,
            &chrono::Local,
        )
    })
}

/// Collects the last `count` messages of `room`, read or not.
async fn last(
    room: &Room,
    count: usize,
    ignored: &watch::Receiver<IgnoredSet>,
    progress: &Progress<'_>,
) -> Result<UnreadText> {
    let timeline = open(room).await?;
    let own = room.own_user_id().to_owned();
    // Not borrowed across the waits below.
    let ignored = ignored.borrow().clone();
    let (items, stopped) = load(&timeline, LAST_EVENTS, progress, |items| {
        line_count(items, &own, &ignored) >= count
    })
    .await;
    let items = without_repeats(items, event_key, undecryptable, read_marker_item);
    let slots = slots(&items, &own, &ignored);
    let members = member_names(room).await;
    Ok(last_text(&slots, &members, count, stopped, &chrono::Local))
}

/// How many lines the text for a summary of `items` would have: each event once
/// (see [`without_repeats`]), and only those the app shows as a message.
fn line_count(
    items: &Items,
    own: &UserId,
    ignored: &std::collections::HashSet<OwnedUserId>,
) -> usize {
    let items = without_repeats(items.clone(), event_key, undecryptable, read_marker_item);
    messages_of(&items, own, ignored)
        .filter(|message| collect::entry(message).is_some())
        .count()
}

/// What the last collection of what is unread saw, in test builds only, for the server
/// tests to show when they fail: the timeline's texts in order before
/// [`without_repeats`], and the SDK's event cache's. The SDK's races that put a
/// message twice into a timeline are rare and depend on timing; when one shows, this
/// tells a copy the timeline repeated from an order the event cache itself holds.
#[cfg(test)]
pub(super) mod probe {
    use std::sync::Mutex;

    pub static SEEN: Mutex<Option<(Vec<String>, Vec<String>)>> = Mutex::new(None);

    pub async fn record(items: &super::Items, room: &matrix_sdk::Room) {
        use matrix_sdk_ui::timeline::VirtualTimelineItem;
        let timeline = items
            .iter()
            .filter_map(|item| match item.as_event() {
                Some(event) => Some(
                    event
                        .content()
                        .as_message()
                        .map_or_else(|| "(not a message)".to_owned(), |m| m.body().to_owned()),
                ),
                None => matches!(item.as_virtual(), Some(VirtualTimelineItem::ReadMarker))
                    .then(|| "|read marker|".to_owned()),
            })
            .collect();
        let mut cache = Vec::new();
        if let Ok((events, _drops)) = room.event_cache().await
            && let Ok(events) = events.events().await
        {
            use matrix_sdk::ruma::events::{
                AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent,
            };
            cache = events
                .iter()
                .map(|event| match event.raw().deserialize() {
                    Ok(AnySyncTimelineEvent::MessageLike(
                        AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(m)),
                    )) => m.content.body().to_owned(),
                    _ => "(not a message)".to_owned(),
                })
                .collect();
        }
        *SEEN.lock().unwrap() = Some((timeline, cache));
    }
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
/// the `newest` messages, read or not, and says so. People of one name among the
/// slots and the room's `members` (names and IDs) are told apart.
fn unread_text<Tz: chrono::TimeZone>(
    slots: &[Slot],
    members: &[(String, String)],
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
    let mut lines = summary_lines(slots, members, start.unwrap_or(0), zone);
    if start.is_none() {
        lines.drain(..lines.len().saturating_sub(newest));
    }
    let (text, kept) = collect::summary_text(&lines);
    UnreadText {
        text,
        messages: kept,
        cut: kept < lines.len(),
        known: start.is_some(),
        stopped: None,
    }
}

/// The text of the last `count` messages in `slots` (a room's timeline, oldest
/// first), read or not; `stopped` is why loading ended early, if it did. Where the
/// text of them all is more than a summary holds, the newest that fit, and it says
/// so. People of one name are told apart as in [`unread_text`].
fn last_text<Tz: chrono::TimeZone>(
    slots: &[Slot],
    members: &[(String, String)],
    count: usize,
    stopped: Option<Cut>,
    zone: &Tz,
) -> UnreadText
where
    Tz::Offset: std::fmt::Display,
{
    let mut lines = summary_lines(slots, members, 0, zone);
    lines.drain(..lines.len().saturating_sub(count));
    let (text, kept) = collect::summary_text(&lines);
    UnreadText {
        text,
        messages: kept,
        cut: kept < lines.len(),
        known: false,
        stopped,
    }
}

/// A line for a summary of each message in `slots` from the slot `from` on. Two
/// people of one name among all the slots and the room's `members` (names and IDs)
/// are told apart, as in the timeline.
fn summary_lines<Tz: chrono::TimeZone>(
    slots: &[Slot],
    members: &[(String, String)],
    from: usize,
    zone: &Tz,
) -> Vec<String>
where
    Tz::Offset: std::fmt::Display,
{
    let mut namesakes = Namesakes::default();
    for message in slots.iter().filter_map(|slot| slot.message.as_ref()) {
        namesakes.learn(message);
    }
    namesakes.learn_members(pairs(members));
    slots[from..]
        .iter()
        .filter_map(|slot| {
            let message = slot.message.as_ref()?;
            if !namesakes.touches(message) {
                return collect::entry(message);
            }
            let mut message = message.clone();
            namesakes.mark(&mut message);
            collect::entry(&message)
        })
        .map(|entry| collect::summary_line(zone, &entry))
        .collect()
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
    let items = without_repeats(items, event_key, undecryptable, read_marker_item);
    progress.tell(event_count(&items));
    let own = room.own_user_id().to_owned();
    let ignored = ignored.borrow().clone();
    let members = member_names(room).await;
    let stop = Arc::new(AtomicBool::new(false));
    // Given up while the file is written: the writing ends at the next message.
    let _guard = StopOnDrop(stop.clone());
    let written = tokio::task::spawn_blocking(move || {
        let messages = || messages_of(&items, &own, &ignored);
        write(
            file, messages, &members, since, cut, &room_name, &now, &stop,
        )
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

/// The names and user IDs of the room's joined members, as the store has them (the
/// server is not asked): a name is the display name, or the first part of the ID for
/// someone without one, as the messages name people.
async fn member_names(room: &Room) -> Vec<(String, String)> {
    let members = match room.members_no_sync(RoomMemberships::JOIN).await {
        Ok(members) => members,
        Err(err) => {
            tracing::debug!(why = %logsafe::kind(&err), "could not load the room's members");
            return Vec::new();
        }
    };
    members
        .iter()
        .map(|member| {
            let name = member
                .display_name()
                .map(crate::text::sanitize_line)
                .filter(|name| !crate::text::is_blank(name))
                .unwrap_or_else(|| member.user_id().localpart().to_owned());
            (name, member.user_id().to_string())
        })
        .collect()
}

/// `members` as names and IDs, for [`Namesakes::learn_members`].
fn pairs(members: &[(String, String)]) -> impl Iterator<Item = (&str, &str)> {
    members
        .iter()
        .map(|(name, id)| (name.as_str(), id.as_str()))
}

/// Writes the archive of `messages` (gone through twice, oldest first) into `file`
/// and gives it its name. Gives the file's path and how many messages are in it.
/// Ends early, leaving no file, when `stop` is set. People of one name among the
/// messages and the room's `members` (names and IDs) are told apart.
///
/// With `since`, the oldest messages are left out as long as they were sent before
/// it. Only those: a message between newer ones stays whatever time it claims, so
/// nobody can keep their message out of an archive with a forged date.
#[allow(clippy::too_many_arguments)]
fn write<Tz: chrono::TimeZone, I: Iterator<Item = Message>>(
    file: crate::files::NewFile,
    messages: impl Fn() -> I,
    members: &[(String, String)],
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
    let kept = || messages().skip_while(|message| too_old(message));
    // Once through for the days the heading names, and for who shares a name with
    // someone else in it or among the room's `members` (told apart as in the
    // timeline); nothing else is kept of it.
    let (mut first, mut last) = (None, None);
    let mut namesakes = Namesakes::default();
    for message in kept() {
        namesakes.learn(&message);
        if let Some(at) = collect::entry(&message).and_then(|entry| entry.at) {
            first = Some(first.map_or(at, |first: u64| first.min(at)));
            last = Some(last.map_or(at, |last: u64| last.max(at)));
        }
    }
    namesakes.learn_members(pairs(members));
    let entries = || {
        kept().filter_map(|mut message| {
            namesakes.mark(&mut message);
            collect::entry(&message)
        })
    };
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
        // A block of two, again right after itself.
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
        let kept: Vec<&str> =
            without_repeats(items, |item| item.0, |_| false, |item| item.1 == "marker")
                .into_iter()
                .map(|item| item.1)
                .collect();
        assert_eq!(
            kept,
            ["date", "a", "b again", "c again", "marker", "echo", "d"]
        );
    }

    /// The shape the server test met while a flood came in: three messages again
    /// after forty newer ones. They belong where they came first; the summary of the
    /// newest seven must not hold them.
    #[test]
    fn a_block_replayed_after_newer_events_stays_where_it_came_first() {
        let first = (470..=517).map(|n| (Some(n), "first"));
        let again = (473..=475).map(|n| (Some(n), "again"));
        let items: Vector<(Option<u32>, &str)> = first
            .clone()
            .take(514 - 470)
            .chain(again)
            .chain(first.skip(514 - 470))
            .collect();
        let kept = without_repeats(items, |item| item.0, |_| false, |_| false);
        let order: Vec<u32> = kept.iter().filter_map(|item| item.0).collect();
        assert_eq!(order, (470..=517).collect::<Vec<_>>());
        // The content is the later copy's, which the SDK's later updates reach.
        let again: Vec<u32> = kept
            .iter()
            .filter(|item| item.1 == "again")
            .filter_map(|item| item.0)
            .collect();
        assert_eq!(again, [473, 474, 475]);
    }

    #[test]
    fn of_an_event_twice_the_first_place_is_kept_and_the_read_marker_goes_with_it() {
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
        let kept: Vec<&str> = without_repeats(
            items,
            |item| item.0,
            |_| false,
            |item| item.1 == "read marker",
        )
        .into_iter()
        .map(|item| item.1)
        .collect();
        assert_eq!(kept, ["a", "b'", "read marker", "c'", "d"]);
        // Read up to b, which came again after newer ones: c and d stay unread.
        let items: Vector<(Option<u32>, &str)> = [
            (Some(1), "a"),
            (Some(2), "b"),
            (Some(3), "c"),
            (Some(4), "d"),
            (Some(2), "b'"),
            (None, "read marker"),
            (Some(5), "e"),
        ]
        .into_iter()
        .collect();
        let kept: Vec<&str> = without_repeats(
            items,
            |item| item.0,
            |_| false,
            |item| item.1 == "read marker",
        )
        .into_iter()
        .map(|item| item.1)
        .collect();
        assert_eq!(kept, ["a", "b'", "read marker", "c", "d", "e"]);
    }

    /// Keys that came in later (from a key file) decrypted the earlier copy of an event
    /// held twice: the first place is kept, with the decrypted content.
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
        let kept: Vec<&str> = without_repeats(
            items,
            |item| item.0,
            |item| item.2,
            |item| item.1 == "read marker",
        )
        .into_iter()
        .map(|item| item.1)
        .collect();
        assert_eq!(kept, ["a readable", "read marker", "b readable", "c"]);
        // The read marker after a first copy stays where it is.
        let items: Vector<(Option<u32>, &str, bool)> = [
            (Some(1), "a readable", false),
            (Some(2), "b", false),
            (None, "read marker", false),
            (Some(1), "a locked", true),
            (Some(3), "c", false),
        ]
        .into_iter()
        .collect();
        let kept: Vec<&str> = without_repeats(
            items,
            |item| item.0,
            |item| item.2,
            |item| item.1 == "read marker",
        )
        .into_iter()
        .map(|item| item.1)
        .collect();
        assert_eq!(kept, ["a readable", "b", "read marker", "c"]);
    }

    /// An event three times: the first place, the newest readable content, and the read
    /// marker that follows the third copy goes after the first.
    #[test]
    fn of_an_event_three_times_the_first_place_and_the_newest_readable_copy_are_kept() {
        let items: Vector<(Option<u32>, &str, bool)> = [
            (Some(1), "a", false),
            (Some(2), "b", false),
            (Some(1), "a locked", true),
            (Some(3), "c", false),
            (Some(1), "a edited", false),
            (None, "read marker", false),
            (Some(4), "d", false),
        ]
        .into_iter()
        .collect();
        let kept: Vec<&str> = without_repeats(
            items,
            |item| item.0,
            |item| item.2,
            |item| item.1 == "read marker",
        )
        .into_iter()
        .map(|item| item.1)
        .collect();
        assert_eq!(kept, ["a edited", "read marker", "b", "c", "d"]);
        // Locked, then readable, then locked again: the readable copy stays.
        let items: Vector<(Option<u32>, &str, bool)> = [
            (Some(1), "a locked", true),
            (Some(2), "b", false),
            (Some(1), "a readable", false),
            (Some(1), "a locked again", true),
            (Some(3), "c", false),
        ]
        .into_iter()
        .collect();
        let kept: Vec<&str> = without_repeats(items, |item| item.0, |item| item.2, |_| false)
            .into_iter()
            .map(|item| item.1)
            .collect();
        assert_eq!(kept, ["a readable", "b", "c"]);
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
        let unread = unread_text(&slots, &[], Some("$2"), &["$3"], 50, &Utc);
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
        let unread = unread_text(&slots, &[], None, &["$1"], 50, &Utc);
        assert_eq!(unread.messages, 2);
        assert!(unread.known);
        assert!(unread.text.starts_with("[2026-11-20 14:03] Bob: new one\n"));
        // An event the app named but that is not there: the same.
        assert_eq!(unread_text(&slots, &[], Some("$9"), &[], 50, &Utc), unread);

        // No line (the SDK shows none): after the message the marker is at.
        let slots = [
            with_id("$1", "Ann", "old"),
            with_id("$2", "Bob", "read"),
            with_id("$3", "Ann", "new"),
        ];
        let unread = unread_text(&slots, &[], None, &["$2"], 50, &Utc);
        assert_eq!(unread.text, "[2026-11-20 14:03] Ann: new\n");
        assert!(unread.known && !unread.cut);
        // A read receipt further on than the read marker: what was read is read.
        assert_eq!(
            unread_text(&slots, &[], None, &["$1", "$2"], 50, &Utc),
            unread
        );
        assert_eq!(
            unread_text(&slots, &[], None, &["$2", "$1"], 50, &Utc),
            unread
        );
        // The marker at the newest message: nothing is unread, and that is known.
        let unread = unread_text(&slots, &[], None, &["$3"], 50, &Utc);
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
            unread_text(&slots, &[], None, &["$2"], 50, &Utc),
            unread_text(&slots, &[], Some("$2"), &[], 50, &Utc),
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
        let unread = unread_text(&slots, &[], None, &["$0"], 2, &Utc);
        assert!(!unread.known);
        assert_eq!(unread.messages, 2);
        assert!(unread.text.starts_with("[2026-11-20 14:03] Bob: two\n"));
        // Nothing says how far the account has read: the same.
        assert!(!unread_text(&slots, &[], None, &[], 50, &Utc).known);
        assert_eq!(unread_text(&slots, &[], None, &[], 50, &Utc).messages, 3);
    }

    #[test]
    fn more_unread_than_a_summary_takes_is_cut_to_the_newest() {
        let mut slots = vec![with_id("$read", "Ann", "read")];
        slots.extend(
            (0..collect::SUMMARY_MESSAGES + 5)
                .map(|n| with_id(&format!("${n}"), "Bob", &format!("m{n}"))),
        );
        let unread = unread_text(&slots, &[], None, &["$read"], 50, &Utc);
        assert_eq!(unread.messages, collect::SUMMARY_MESSAGES);
        assert!(unread.cut && unread.known);
        assert!(unread.text.starts_with("[2026-11-20 14:03] Bob: m5\n"));
    }

    /// `/catchup N`: the newest N messages, oldest first, whatever was read; the
    /// "new" line and events the app shows nothing of are no messages.
    #[test]
    fn the_last_messages_are_the_newest_read_or_not() {
        let slots = [
            with_id("$1", "Ann", "one"),
            with_id("$2", "Bob", "two"),
            line(),
            with_id("$3", "Ann", "three"),
            hidden("$4"),
            with_id("$5", "Bob", "four"),
        ];
        let last = last_text(&slots, &[], 3, None, &Utc);
        assert_eq!(
            last.text,
            "[2026-11-20 14:03] Bob: two\n[2026-11-20 14:03] Ann: three\n\
             [2026-11-20 14:03] Bob: four\n"
        );
        assert_eq!(
            (last.messages, last.cut, last.known, last.stopped),
            (3, false, false, None)
        );
        assert_eq!(last_text(&slots, &[], 1, None, &Utc).messages, 1);
        assert!(
            last_text(&slots, &[], 1, None, &Utc)
                .text
                .ends_with("Bob: four\n")
        );
    }

    /// Fewer messages than asked for: all there are, and why loading ended is told.
    #[test]
    fn of_fewer_messages_than_asked_for_all_are_taken() {
        let slots = [with_id("$1", "Ann", "one"), with_id("$2", "Bob", "two")];
        let last = last_text(&slots, &[], 50, None, &Utc);
        assert_eq!(last.messages, 2);
        assert!(!last.cut && last.stopped.is_none());
        assert!(last.text.starts_with("[2026-11-20 14:03] Ann: one\n"));
        let last = last_text(&slots, &[], 50, Some(Cut::LoadFailed), &Utc);
        assert_eq!((last.messages, last.stopped), (2, Some(Cut::LoadFailed)));
        // A room without any message.
        let last = last_text(&[line(), hidden("$1")], &[], 50, None, &Utc);
        assert_eq!(
            (last.text.as_str(), last.messages, last.cut),
            ("", 0, false)
        );
    }

    /// The last N hold more text than a summary takes: the newest that fit, cut from
    /// the oldest, and it says so.
    #[test]
    fn the_last_messages_are_cut_from_the_oldest_where_their_text_is_too_long() {
        let slots: Vec<Slot> = (0..12)
            .map(|n| with_id(&format!("${n}"), "Ann", &format!("{n}{}", "x".repeat(4999))))
            .collect();
        let last = last_text(&slots, &[], 10, None, &Utc);
        // 10 asked for (2 to 11), of which the newest 4 fit.
        assert_eq!(last.messages, 4);
        assert!(last.cut);
        assert!(last.text.chars().count() <= collect::SUMMARY_CHARS + 1);
        assert!(
            last.text.starts_with("[2026-11-20 14:03] Ann: 8x"),
            "{}",
            &last.text[..40]
        );
        assert!(last.text.lines().last().unwrap().contains("Ann: 11x"));
        // All of them fit: not cut.
        let slots: Vec<Slot> = (0..collect::SUMMARY_MESSAGES + 5)
            .map(|n| with_id(&format!("${n}"), "Bob", &format!("m{n}")))
            .collect();
        let last = last_text(&slots, &[], collect::SUMMARY_MESSAGES, None, &Utc);
        assert_eq!(last.messages, collect::SUMMARY_MESSAGES);
        assert!(!last.cut);
        assert!(last.text.starts_with("[2026-11-20 14:03] Bob: m5\n"));
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
            &[],
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
            &[],
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

    /// Two people of one name, as the SDK may not tell apart (both without a display
    /// name, say): one by the timeline's `sender` and `sender_id`.
    fn namesakes() -> [Message; 2] {
        let named = |id: &str, body: &str| Message {
            sender_id: Some(id.to_owned()),
            ..said("bob", body)
        };
        [
            named("@bob:example.org", "here"),
            named("@bob:elsewhere.org", "there"),
        ]
    }

    /// What is unread names two people of one name with their IDs, as the timeline
    /// does: among all that is loaded, not only what is unread.
    #[test]
    fn unread_tells_apart_two_people_of_one_name() {
        let [here, there] = namesakes();
        let slot = |id: &str, message: Message| Slot {
            id: Some(id.to_owned()),
            line: false,
            message: Some(message),
        };
        let slots = [slot("$1", here), slot("$2", there)];
        let unread = unread_text(&slots, &[], None, &["$1"], 50, &Utc);
        assert_eq!(
            unread.text,
            "[2026-11-20 14:03] bob (@bob:elsewhere.org): there\n"
        );
        // The same of the last messages: the namesake beyond the last one counts.
        assert_eq!(last_text(&slots, &[], 1, None, &Utc).text, unread.text);
    }

    /// A line about someone who wrote nothing loaded (banned, say) and goes by the name
    /// of one of the room's members, and the member's.
    fn ban_and_member() -> (Message, Vec<(String, String)>) {
        let mut ban = said("carol", "bob was banned by carol");
        ban.kind = crate::model::MsgKind::System;
        ban.sender_id = Some("@carol:example.org".into());
        ban.subject = Some(("bob".into(), "@bob:elsewhere.org".into()));
        let members = vec![
            ("carol".to_owned(), "@carol:example.org".to_owned()),
            ("bob".to_owned(), "@bob:example.org".to_owned()),
        ];
        (ban, members)
    }

    /// What is unread counts the room's members too, as the timeline does: whom a
    /// line is about is named with the ID where a member goes by the same name.
    #[test]
    fn unread_tells_apart_whom_a_line_is_about_from_a_member_of_the_same_name() {
        let (ban, members) = ban_and_member();
        let slots = [Slot {
            id: Some("$ban".into()),
            line: false,
            message: Some(ban),
        }];
        let unread = unread_text(&slots, &members, None, &[], 50, &Utc);
        assert!(
            unread
                .text
                .contains("] (bob (@bob:elsewhere.org) was banned by carol)\n"),
            "{}",
            unread.text
        );
        assert_eq!(last_text(&slots, &members, 5, None, &Utc).text, unread.text);
    }

    /// An archive counts the room's members too.
    #[test]
    fn an_archive_tells_apart_whom_a_line_is_about_from_a_member_of_the_same_name() {
        let dir = temp("archive-member-namesake");
        let (ban, members) = ban_and_member();
        let now = collect::local(&Utc, AT).unwrap();
        let (path, _) = write(
            NewFile::create(&dir, "members.md").unwrap(),
            || std::iter::once(ban.clone()),
            &members,
            None,
            None,
            "Room",
            &now,
            &AtomicBool::new(false),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("bob (@bob:elsewhere.org) was banned by carol"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An archive names two people of one name with their IDs, as the timeline does.
    #[test]
    fn an_archive_tells_apart_two_people_of_one_name() {
        let dir = temp("archive-namesakes");
        let messages = namesakes();
        let now = collect::local(&Utc, AT).unwrap();
        let (path, _) = write(
            NewFile::create(&dir, "namesakes.md").unwrap(),
            || messages.clone().into_iter(),
            &[],
            None,
            None,
            "Room",
            &now,
            &AtomicBool::new(false),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("**14:03 bob (@bob:example.org)**\\\nhere\n"),
            "{text}"
        );
        assert!(
            text.contains("**14:03 bob (@bob:elsewhere.org)**\\\nthere\n"),
            "{text}"
        );
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
            &[],
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
                &[],
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
                &[],
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
            &[],
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

    /// What the server tests here share: a room of a second user's, without encryption
    /// (theirs is a plain client), that our worker, logged in, has joined.
    struct Joined {
        handle: crate::matrix::MatrixHandle,
        events: tokio::sync::mpsc::UnboundedReceiver<MatrixEvent>,
        other: matrix_sdk::Client,
        other_room: matrix_sdk::Room,
        room_id: String,
        room_name: String,
        root: std::path::PathBuf,
        run: u32,
    }

    async fn joined_room(tag: &str) -> Joined {
        use matrix_sdk::Client;
        use matrix_sdk::config::SyncSettings;
        use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoom;

        use crate::matrix::{MatrixCmd, RoomCategory, spawn};

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

        let root = std::env::temp_dir().join(format!("matrix-rat-collect-{tag}-{run}"));
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
        let room_name = format!("collect {tag} {run}");
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
        Joined {
            handle,
            events,
            other,
            other_room,
            room_id,
            room_name,
            root,
            run,
        }
    }

    // The answer to a collection, whatever else the worker says meanwhile.
    async fn answer(
        events: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixEvent>,
        request: u64,
    ) -> CollectEvent {
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
    // The texts in order, a run of consecutive numbers of one kind written as a range
    // ("flood-7-012..=015, flood-7-003"), so a failure shows the whole shape.
    fn runs(got: &[String]) -> String {
        let split = |text: &str| {
            let (kind, number) = text.rsplit_once('-')?;
            Some((kind.to_owned(), number.parse::<usize>().ok()?, number.len()))
        };
        let mut out = Vec::new();
        let mut i = 0;
        while i < got.len() {
            let Some((kind, first, width)) = split(&got[i]) else {
                out.push(got[i].clone());
                i += 1;
                continue;
            };
            let mut j = i + 1;
            while j < got.len()
                && split(&got[j]).is_some_and(|(k, n, _)| k == kind && n == first + (j - i))
            {
                j += 1;
            }
            out.push(if j - i > 1 {
                format!("{kind}-{first:0width$}..={}", first + (j - i - 1))
            } else {
                got[i].clone()
            });
            i = j;
        }
        out.join(", ")
    }
    // What the probe saw in the last collection: the timeline before
    // `without_repeats`, and the SDK's event cache.
    fn seen() -> String {
        let seen = super::probe::SEEN.lock().unwrap().clone();
        let Some((timeline, cache)) = seen else {
            return "the probe saw nothing".to_owned();
        };
        let repeated = {
            let mut met = std::collections::HashSet::new();
            timeline
                .iter()
                .filter(|text| !text.starts_with(['(', '|']) && !met.insert(*text))
                .count()
        };
        format!(
            "timeline ({} items, {repeated} repeated): [{}]; event cache ({} events): [{}]",
            timeline.len(),
            runs(&timeline),
            cache.len(),
            runs(&cache),
        )
    }
    fn shape(unread: &UnreadText) -> String {
        let got = bodies(unread);
        format!(
            "known={}, messages={}, cut={}, all=[{}]",
            unread.known,
            unread.messages,
            unread.cut,
            runs(&got),
        )
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
        use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
        use tokio::sync::mpsc::UnboundedReceiver;

        use crate::matrix::MatrixCmd;
        use crate::model::MsgKind;

        let Joined {
            handle,
            mut events,
            other,
            other_room,
            room_id,
            room_name,
            root,
            run,
        } = joined_room("archive").await;
        let within = Duration::from_secs;

        let say = |body: String| {
            let other_room = other_room.clone();
            async move {
                other_room
                    .send(RoomMessageEventContent::text_plain(body))
                    .await
                    .expect("message of the second user");
            }
        };
        // What is unread, asked again until `done` accepts it (our sync may be behind
        // what the other one just sent).
        async fn unread_until(
            handle: &crate::matrix::MatrixHandle,
            events: &mut UnboundedReceiver<MatrixEvent>,
            room_id: &str,
            first_request: u64,
            newest: usize,
            done: impl Fn(&UnreadText) -> bool,
        ) -> Result<UnreadText, String> {
            let mut last = String::new();
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
                    return Ok(unread);
                }
                last = shape(&unread);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(last)
        }
        // The newest texts of `room` in the server's order (`/messages`, oldest first), to
        // set beside ours when they disagree.
        async fn server_order(room: &matrix_sdk::Room, newest: u32) -> String {
            let mut options = matrix_sdk::room::MessagesOptions::backward();
            options.limit = newest.into();
            let Ok(page) = room.messages(options).await else {
                return "the server's order could not be read".to_owned();
            };
            let mut got: Vec<String> = page
                .chunk
                .iter()
                .filter_map(|event| {
                    let matrix_sdk::ruma::events::AnySyncTimelineEvent::MessageLike(
                        matrix_sdk::ruma::events::AnySyncMessageLikeEvent::RoomMessage(
                            matrix_sdk::ruma::events::SyncMessageLikeEvent::Original(message),
                        ),
                    ) = event.raw().deserialize().ok()?
                    else {
                        return None;
                    };
                    Some(message.content.body().to_owned())
                })
                .collect();
            got.reverse();
            runs(&got)
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
        .await
        .unwrap_or_else(|last| panic!("what is unread never came to the news; last: {last}"));
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
        let unread = match unread_until(&handle, &mut events, &room_id, 200, 7, |unread| {
            unread.text.trim_end().ends_with(flood[many - 1].as_str())
        })
        .await
        {
            Ok(unread) => unread,
            Err(last) => panic!(
                "what is unread never ended with the newest of the flood; last: {last}; \
                 the server's newest 40: {}",
                server_order(&other_room, 40).await
            ),
        };
        let info = format!("{}; {}", shape(&unread), seen());
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

    /// While a flood comes in, each collection opens a timeline as sync brings more:
    /// matrix-sdk-ui 0.19.1 can then hold messages twice, a copy even after newer ones
    /// (see [`without_repeats`]). What is collected must still be each message once,
    /// in the order sent.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn while_a_flood_comes_in_what_is_collected_is_in_order_and_once() {
        use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;

        use crate::matrix::MatrixCmd;

        let Joined {
            handle,
            mut events,
            other,
            other_room,
            room_id,
            root,
            run,
            ..
        } = joined_room("flood").await;
        // The ids of what was sent, in order: each collection starts a little before the
        // newest, so it loads little and many fit into the flood.
        let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let say = |room: matrix_sdk::Room, body: String| async move {
            room.send(RoomMessageEventContent::text_plain(body))
                .await
                .expect("message of the second user")
                .response
                .event_id
                .to_string()
        };
        let first = say(other_room.clone(), format!("storm-{run}-start")).await;
        const STORM: usize = 1500;
        let flood = tokio::spawn({
            let (room, sent) = (other_room.clone(), sent.clone());
            async move {
                for n in 0..STORM {
                    let id = say(room.clone(), format!("storm-{run}-{n:03}")).await;
                    sent.lock().unwrap().push(id);
                }
            }
        });
        let prefix = format!("storm-{run}-");
        let mut collections = 0;
        for request in 1000.. {
            let flooding = !flood.is_finished();
            // Every other one from the very first: that one pages back through what the
            // event cache keeps on disk while sync brings more.
            let from = {
                let sent = sent.lock().unwrap();
                sent.len()
                    .checked_sub(40)
                    .filter(|_| request % 2 == 0)
                    .map_or_else(|| first.clone(), |at| sent[at].clone())
            };
            handle.send(MatrixCmd::Collect(CollectCmd::Unread {
                request,
                room_id: room_id.clone(),
                from: Some(from),
                newest: 50,
            }));
            let CollectEvent::Unread { result, .. } = answer(&mut events, request).await else {
                panic!("not the answer to what is unread");
            };
            let unread = result.expect("what is unread");
            collections += 1;
            let numbers: Vec<usize> = bodies(&unread)
                .iter()
                .filter_map(|body| body.strip_prefix(&prefix)?.parse().ok())
                .collect();
            assert!(
                numbers.windows(2).all(|pair| pair[0] < pair[1]),
                "collection {collections} holds a message twice or out of order: {}; {}",
                shape(&unread),
                seen()
            );
            if !flooding {
                break;
            }
        }
        flood.await.expect("the flood was sent");
        // The worker's sync and store stop before their folder goes.
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = other.matrix_auth().logout().await;
        let _ = std::fs::remove_dir_all(root);
    }

    /// Against a real homeserver, through the worker, as `/catchup N` asks: in a fresh
    /// room of a second account,
    ///
    /// 1. more messages than asked for, all of them read: the last N are exactly the
    ///    newest N, oldest first;
    /// 2. three more, unread: the last N are the newest N, read or not, and collecting
    ///    them moved no read marker (what is unread is still exactly those three);
    /// 3. more asked for than the room has: all of it, and that the room has no more.
    ///
    /// Run as the first server test here; it sends some 70 messages.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn the_last_messages_are_the_newest_ones_read_or_not_and_move_no_read_marker() {
        use matrix_sdk::Client;
        use matrix_sdk::config::SyncSettings;
        use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoom;
        use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
        use tokio::sync::mpsc::UnboundedReceiver;

        use crate::matrix::{MatrixCmd, MatrixHandle, RoomCategory, spawn};

        const ASKED: usize = 40;
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

        let root = std::env::temp_dir().join(format!("matrix-rat-collect-last-{run}"));
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

        // A room of the other's, without encryption, that we are invited to and join.
        let mut create = CreateRoom::new();
        create.invite = vec![UserId::parse(&own_id).expect("a user id")];
        create.name = Some(format!("collect last test {run}"));
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
        async fn answer(
            events: &mut UnboundedReceiver<MatrixEvent>,
            request: u64,
        ) -> Result<UnreadText, String> {
            let wait = async {
                loop {
                    let MatrixEvent::Collect(event) = events.recv().await.expect("worker stopped")
                    else {
                        continue;
                    };
                    match event {
                        CollectEvent::Unread {
                            request: of,
                            result,
                        } if of == request => return result,
                        CollectEvent::Unread { .. }
                        | CollectEvent::Archived { .. }
                        | CollectEvent::Progress { .. } => {}
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(120), wait)
                .await
                .expect("the collection is answered")
        }
        // The texts of our test's lines, oldest first (not the lines of the room's
        // creation, which come with the start of the room).
        fn bodies(text: &UnreadText, run: u32) -> Vec<String> {
            text.text
                .lines()
                .filter_map(|line| line.rsplit(": ").next())
                .filter(|body| body.contains(&format!("-{run}-")))
                .map(str::to_owned)
                .collect()
        }
        // `command` asked again until `done` accepts the answer: our sync may be
        // behind what the other one just sent.
        async fn until(
            handle: &MatrixHandle,
            events: &mut UnboundedReceiver<MatrixEvent>,
            first_request: u64,
            command: impl Fn(u64) -> CollectCmd,
            done: impl Fn(&UnreadText) -> bool,
        ) -> UnreadText {
            let mut last = None;
            for attempt in 0..30 {
                let request = first_request + attempt;
                handle.send(MatrixCmd::Collect(command(request)));
                let text = answer(events, request).await.expect("the text");
                if done(&text) {
                    return text;
                }
                last = Some(format!("{text:?}"));
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            panic!("the answer never came to what was expected; last: {last:?}");
        }
        let last = |count: usize| {
            let room_id = room_id.clone();
            move |request| CollectCmd::Last {
                request,
                room_id: room_id.clone(),
                count,
            }
        };

        // Our joining is in our view before the other says anything: else it can reach our
        // timeline after their messages, and count as one of the last lines asked for. It
        // is the one joining after our invitation, whatever the names are.
        until(&handle, &mut events, 900, last(20), |text| {
            let lines: Vec<&str> = text.text.lines().collect();
            lines
                .iter()
                .position(|line| line.contains(" was invited by "))
                .is_some_and(|at| {
                    lines[at + 1..]
                        .iter()
                        .any(|line| line.ends_with(" joined)"))
                })
        })
        .await;

        // 1. More than asked for (and than a fresh timeline holds); all of them read.
        let said: Vec<String> = (0..ASKED + 25)
            .map(|n| format!("said-{run}-{n:02}"))
            .collect();
        for body in &said {
            say(body.clone()).await;
        }
        let newest = said.last().cloned().unwrap();
        until(&handle, &mut events, 1, last(ASKED), |text| {
            bodies(text, run).last() == Some(&newest)
        })
        .await;
        handle.send(MatrixCmd::MarkRead {
            room_id: room_id.clone(),
            public: false,
        });
        // Reading is not answered: give it the time to reach the server.
        tokio::time::sleep(within(2)).await;
        let text = until(&handle, &mut events, 100, last(ASKED), |_| true).await;
        assert_eq!(bodies(&text, run), said[said.len() - ASKED..], "{text:?}");
        assert_eq!(text.messages, ASKED, "{text:?}");
        assert!(
            !text.cut && !text.known && text.stopped.is_none(),
            "{text:?}"
        );

        // 2. Three more, unread: the last N, read or not; and then what is unread is
        // still exactly the three.
        let news: Vec<String> = (0..3).map(|n| format!("news-{run}-{n}")).collect();
        for body in &news {
            say(body.clone()).await;
        }
        let text = until(&handle, &mut events, 200, last(ASKED), |text| {
            bodies(text, run).last() == news.last()
        })
        .await;
        let mut expected = said[said.len() - (ASKED - news.len())..].to_vec();
        expected.extend(news.iter().cloned());
        assert_eq!(bodies(&text, run), expected, "{text:?}");
        assert_eq!(text.messages, ASKED, "{text:?}");
        let room = room_id.clone();
        let unread = until(
            &handle,
            &mut events,
            300,
            move |request| CollectCmd::Unread {
                request,
                room_id: room.clone(),
                from: None,
                newest: 50,
            },
            |unread| unread.known && unread.messages >= news.len(),
        )
        .await;
        assert_eq!(bodies(&unread, run), news, "{unread:?}");
        assert_eq!(unread.messages, news.len(), "{unread:?}");

        // 3. More asked for than the room has: all of it, back to its start.
        let text = until(
            &handle,
            &mut events,
            400,
            last(collect::SUMMARY_MESSAGES),
            |_| true,
        )
        .await;
        let mut all = said.clone();
        all.extend(news.iter().cloned());
        assert_eq!(bodies(&text, run), all, "{text:?}");
        assert!(text.messages < collect::SUMMARY_MESSAGES, "{text:?}");
        assert!(text.messages >= all.len(), "{text:?}");
        assert!(!text.cut && text.stopped.is_none(), "{text:?}");

        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = other.matrix_auth().logout().await;
        let _ = std::fs::remove_dir_all(root);
    }
}
