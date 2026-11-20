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
//! one, so no second copy of the room exists. A room with more gets its newest
//! messages archived, and the archive says so.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::{FutureExt, StreamExt, pin_mut};
use matrix_sdk::Room;
use matrix_sdk::ruma::events::fully_read::FullyReadEventContent;
use matrix_sdk::ruma::{OwnedUserId, UserId};
use matrix_sdk_ui::eyeball_im::Vector;
use matrix_sdk_ui::timeline::{
    RoomExt, Timeline, TimelineFocus, TimelineItem, TimelineReadReceiptTracking,
    VirtualTimelineItem,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::MatrixEvent;
use super::safety::IgnoredSet;
use crate::collect::{self, Cut, Span};
use crate::logsafe;
use crate::model::{ItemRef, Message, MsgKind};

/// The most events one archive takes; of a room with more, the newest.
pub const ARCHIVE_EVENTS: usize = 50_000;
/// How many events are loaded at most to find where the user stopped reading.
const UNREAD_EVENTS: usize = collect::SUMMARY_MESSAGES + 50;
/// Events asked for at a time.
const PAGE: u16 = 100;
/// How long opening a room's messages may take, and one request for older ones.
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
const PAGE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long collecting what is unread may take in all.
const UNREAD_BUDGET: Duration = Duration::from_secs(60);
/// How long a collection waits for the messages it asked for to show in the timeline.
const SETTLE: Duration = Duration::from_secs(2);

pub enum CollectCmd {
    /// The unread messages of a joined room as text for a summary; answered with
    /// [`CollectEvent::Unread`].
    Unread {
        request: u64,
        room_id: String,
        /// The first unread message as the app shows it (the "new" line of the open
        /// room), by its event id. Without it the account's read marker decides.
        from: Option<String>,
    },
    /// Write the messages of a joined room into a new Markdown file in `dir`;
    /// answered with [`CollectEvent::Archived`].
    Archive {
        request: u64,
        room_id: String,
        span: Span,
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
    /// There is more unread than this, or where the user stopped reading was not
    /// found: these are the newest.
    pub cut: bool,
}

/// Hand-written: the text is what people wrote.
impl std::fmt::Debug for UnreadText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnreadText")
            .field("messages", &self.messages)
            .field("cut", &self.cut)
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
            } => {
                let room = super::agent::joined_room(&self.client, &self.left, &room_id);
                let events = self.events.clone();
                let ignored = self.ignored.clone();
                self.running.spawn(async move {
                    let work = async {
                        let progress = Progress::new(&events, request);
                        unread(&room?, from, &ignored, &progress).await
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
                dir,
                room_name,
            } => {
                let room = super::agent::joined_room(&self.client, &self.left, &room_id);
                let events = self.events.clone();
                let ignored = self.ignored.clone();
                self.running.spawn(async move {
                    let result = async {
                        let progress = Progress::new(&events, request);
                        archive(&room?, span, dir, room_name, &ignored, &progress).await
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

/// When the oldest loaded event was sent, in milliseconds since 1970.
fn oldest(items: &Items) -> Option<u64> {
    items
        .iter()
        .find_map(|item| item.as_event())
        .map(|event| u64::from(event.timestamp().get()))
}

/// The items of `timeline` once `enough` says so, loading older messages page by page
/// until then. It also ends at the room's start, at `most` events ([`Cut::Limit`]) and
/// when a page cannot be loaded ([`Cut::LoadFailed`]); what is there by then is the
/// answer.
async fn load(
    timeline: &Timeline,
    most: usize,
    progress: &Progress<'_>,
    mut enough: impl FnMut(&Items) -> bool,
) -> (Items, Option<Cut>) {
    let (mut items, changes) = timeline.subscribe().await;
    pin_mut!(changes);
    let mut failed = false;
    let mut at_start = false;
    loop {
        // What the timeline has worked in meanwhile, without waiting for more.
        while let Some(Some(diffs)) = changes.next().now_or_never() {
            for diff in diffs {
                diff.apply(&mut items);
            }
        }
        let loaded = event_count(&items);
        if at_start || items.iter().any(|item| item.is_timeline_start()) || enough(&items) {
            return (items, None);
        }
        if loaded >= most {
            return (items, Some(Cut::Limit));
        }
        if failed {
            return (items, Some(Cut::LoadFailed));
        }
        progress.tell(loaded);
        match tokio::time::timeout(PAGE_TIMEOUT, timeline.paginate_backwards(PAGE)).await {
            Ok(Ok(reached_start)) => at_start = reached_start,
            Ok(Err(err)) => {
                tracing::debug!(why = %logsafe::kind(&err), "collect: could not load older messages");
                failed = true;
            }
            Err(_) => failed = true,
        }
        // What was loaded reaches the timeline a moment later.
        if let Ok(Some(diffs)) = tokio::time::timeout(SETTLE, changes.next()).await {
            for diff in diffs {
                diff.apply(&mut items);
            }
        }
    }
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

/// Collects what is unread in `room`.
async fn unread(
    room: &Room,
    from: Option<String>,
    ignored: &watch::Receiver<IgnoredSet>,
    progress: &Progress<'_>,
) -> Result<UnreadText> {
    let timeline = open(room).await?;
    let marker = read_marker(room).await;
    let (items, cut) = load(&timeline, UNREAD_EVENTS, progress, |items| {
        has_marker(items)
            || [&from, &marker]
                .into_iter()
                .flatten()
                .any(|id| has_event(items, id))
    })
    .await;
    let own = room.own_user_id().to_owned();
    let messages = super::convert::convert(&items, &own, &ignored.borrow());
    Ok(unread_text(
        &messages,
        from.as_deref(),
        marker.as_deref(),
        cut.is_some(),
        &chrono::Local,
    ))
}

fn event_id(message: &Message) -> Option<&str> {
    match &message.item {
        Some(ItemRef::Event(id)) => Some(id),
        _ => None,
    }
}

/// The text of what is unread among `messages` (oldest first, with the "new" line of
/// the SDK where it has one). Unread is what comes from the message `from` on; else
/// what comes after the SDK's line; else what comes after the message `marker`. With
/// none of them found, all of `messages` count and the text is marked as cut, as it
/// is when loading `stopped` before the room's start.
fn unread_text<Tz: chrono::TimeZone>(
    messages: &[Message],
    from: Option<&str>,
    marker: Option<&str>,
    stopped: bool,
    zone: &Tz,
) -> UnreadText
where
    Tz::Offset: std::fmt::Display,
{
    let place =
        |id: Option<&str>| id.and_then(|id| messages.iter().position(|m| event_id(m) == Some(id)));
    let start = place(from)
        .or_else(|| {
            messages
                .iter()
                .position(|m| m.kind == MsgKind::ReadMarker)
                .map(|line| line + 1)
        })
        .or_else(|| place(marker).map(|read| read + 1));
    let lines: Vec<String> = messages[start.unwrap_or(0)..]
        .iter()
        .filter_map(collect::entry)
        .map(|entry| collect::summary_line(zone, &entry))
        .collect();
    let (text, kept) = collect::summary_text(&lines);
    UnreadText {
        text,
        messages: kept,
        cut: kept < lines.len() || (start.is_none() && stopped),
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
    dir: PathBuf,
    room_name: String,
    ignored: &watch::Receiver<IgnoredSet>,
    progress: &Progress<'_>,
) -> Result<Archived> {
    let timeline = open(room).await?;
    let now = chrono::Local::now();
    let since = span.since(u64::try_from(now.timestamp_millis()).unwrap_or(0));
    let most = match span {
        // Nothing is asked of the server.
        Span::Loaded => 0,
        Span::Days(_) | Span::Everything => ARCHIVE_EVENTS,
    };
    let (items, cut) = load(&timeline, most, progress, |items| {
        since.is_some_and(|since| oldest(items).is_some_and(|oldest| oldest < since))
    })
    .await;
    // "What is loaded" is complete as it is.
    let cut = cut.filter(|_| span != Span::Loaded);
    progress.tell(event_count(&items));
    let own = room.own_user_id().to_owned();
    let ignored = ignored.borrow().clone();
    let stop = Arc::new(AtomicBool::new(false));
    // Given up while the file is written: the writing ends at the next message.
    let _guard = StopOnDrop(stop.clone());
    let written = tokio::task::spawn_blocking(move || {
        let messages = || messages_of(&items, &own, &ignored);
        write(messages, since, cut, &dir, &room_name, &now, &stop)
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

/// Writes the archive of `messages` (gone through twice, oldest first; those sent before `since` are left
/// out) into a new private file in `dir`, named after the room and the day. Gives the
/// file's path and how many messages are in it. Ends early, leaving no file, when
/// `stop` is set.
fn write<Tz: chrono::TimeZone, I: Iterator<Item = Message>>(
    messages: impl Fn() -> I,
    since: Option<u64>,
    cut: Option<Cut>,
    dir: &Path,
    room_name: &str,
    now: &chrono::DateTime<Tz>,
    stop: &AtomicBool,
) -> std::io::Result<(PathBuf, usize)>
where
    Tz::Offset: std::fmt::Display,
{
    let wanted =
        |message: &Message| since.is_none_or(|since| message.sent_at.is_some_and(|at| at >= since));
    let entries = || {
        messages()
            .filter(|message| wanted(message))
            .filter_map(|message| collect::entry(&message))
    };
    // Once through for the days the heading names; nothing is kept of it.
    let mut times = entries().filter_map(|entry| entry.at);
    let first = times.next();
    let last = times.last().or(first);
    let name = crate::archive::file_name(room_name, now.date_naive());
    let file = crate::files::NewFile::create(dir, &name)?;
    let mut writer = crate::archive::Writer::new(file, now.timezone());
    writer.header(room_name, now, first, last, cut)?;
    for entry in entries() {
        if stop.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("given up"));
        }
        writer.entry(&entry)?;
    }
    let (file, count) = writer.finish();
    Ok((file.finish()?, count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::tests::{AT, said};
    use chrono::Utc;

    fn with_id(id: &str, sender: &str, body: &str) -> Message {
        Message {
            item: Some(ItemRef::Event(id.to_owned())),
            ..said(sender, body)
        }
    }

    fn line() -> Message {
        Message {
            kind: MsgKind::ReadMarker,
            ..Message::default()
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("matrix-rat-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn unread_is_what_comes_from_the_apps_new_line_on() {
        let messages = [
            with_id("$1", "Ann", "old"),
            with_id("$2", "Bob", "new one"),
            line(),
            with_id("$3", "Ann", "new two"),
        ];
        // The app's own line wins over the SDK's and over the read marker.
        let unread = unread_text(&messages, Some("$2"), Some("$3"), false, &Utc);
        assert_eq!(
            unread.text,
            "[2026-11-20 14:03] Bob: new one\n[2026-11-20 14:03] Ann: new two\n"
        );
        assert_eq!((unread.messages, unread.cut), (2, false));
    }

    #[test]
    fn without_it_the_sdks_line_and_then_the_read_marker_decide() {
        let messages = [
            with_id("$1", "Ann", "old"),
            line(),
            with_id("$2", "Bob", "new one"),
            with_id("$3", "Ann", "new two"),
        ];
        let unread = unread_text(&messages, None, Some("$2"), false, &Utc);
        assert_eq!(unread.messages, 2);
        assert!(unread.text.starts_with("[2026-11-20 14:03] Bob: new one\n"));
        // An event the app named but that is not there: the same.
        assert_eq!(
            unread_text(&messages, Some("$9"), None, false, &Utc),
            unread
        );

        // No line (the SDK shows none): after the message the marker is at.
        let messages = [
            with_id("$1", "Ann", "old"),
            with_id("$2", "Bob", "read"),
            with_id("$3", "Ann", "new"),
        ];
        let unread = unread_text(&messages, None, Some("$2"), false, &Utc);
        assert_eq!(unread.text, "[2026-11-20 14:03] Ann: new\n");
        assert!(!unread.cut);
        // The marker at the newest message: nothing is unread.
        let unread = unread_text(&messages, None, Some("$3"), false, &Utc);
        assert_eq!(
            (unread.text.as_str(), unread.messages, unread.cut),
            ("", 0, false)
        );
    }

    #[test]
    fn where_reading_stopped_is_not_found_the_newest_are_taken_and_it_says_so() {
        let messages = [with_id("$1", "Ann", "one"), with_id("$2", "Bob", "two")];
        // Loading stopped before the marker came into view.
        let unread = unread_text(&messages, None, Some("$0"), true, &Utc);
        assert_eq!((unread.messages, unread.cut), (2, true));
        // The whole room is loaded and was never read: that is all of it.
        let unread = unread_text(&messages, None, None, false, &Utc);
        assert_eq!((unread.messages, unread.cut), (2, false));
    }

    #[test]
    fn more_unread_than_a_summary_takes_is_cut_to_the_newest() {
        let mut messages = vec![with_id("$read", "Ann", "read")];
        messages.extend(
            (0..collect::SUMMARY_MESSAGES + 5)
                .map(|n| with_id(&format!("${n}"), "Bob", &format!("m{n}"))),
        );
        let unread = unread_text(&messages, None, Some("$read"), false, &Utc);
        assert_eq!(unread.messages, collect::SUMMARY_MESSAGES);
        assert!(unread.cut);
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
        let messages = vec![old, first, line(), said("Bob", "bye")];
        let now = collect::local(&Utc, AT).unwrap();
        let stop = AtomicBool::new(false);
        let (path, count) = write(
            || messages.clone().into_iter(),
            Some(AT - 7 * day),
            None,
            &dir,
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
            || messages.clone().into_iter(),
            None,
            Some(Cut::Limit),
            &dir,
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
    fn an_archive_that_is_given_up_leaves_no_file() {
        let dir = temp("archive-stop");
        let now = collect::local(&Utc, AT).unwrap();
        let stop = AtomicBool::new(true);
        let messages = vec![said("Ann", "one")];
        assert!(
            write(
                || messages.clone().into_iter(),
                None,
                None,
                &dir,
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
        let (path, count) =
            write(std::iter::empty, None, None, &dir, "Plans", &now, &stop).unwrap();
        assert_eq!(count, 0);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("No messages.\n")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
