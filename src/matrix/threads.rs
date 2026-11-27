//! Threads: a timeline that shows one thread, the line under a message that started
//! one, and the list of a room's threads.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use matrix_sdk::deserialized_responses::TimelineEvent;
use matrix_sdk::room::ListThreadsOptions;
use matrix_sdk::ruma::api::client::threads::get_threads::v1::IncludeThreads;
use matrix_sdk::ruma::events::receipt::{Receipt, ReceiptThread, ReceiptType};
use matrix_sdk::ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedUserId, UInt, UserId,
};
use matrix_sdk::{Room, ThreadingSupport};
use matrix_sdk_ui::eyeball_im::Vector;
use matrix_sdk_ui::timeline::{
    MsgLikeContent, MsgLikeKind, RoomExt, Timeline, TimelineDetails, TimelineEventItemId,
    TimelineFocus, TimelineItem, TimelineReadReceiptTracking,
};

use super::convert::shown_name;
use super::safety::IgnoredSet;
use super::{SdkResult, moderation, search};
use crate::model::{ItemRef, Message, ThreadInfo, ThreadSummary};
use crate::rt;
use crate::text::{first_line, sanitize_line, truncate};

/// Replies in threads are shown in their thread only, so they must not count as
/// unread messages of the room either.
pub const SUPPORT: ThreadingSupport = ThreadingSupport::Enabled {
    with_subscriptions: false,
};

/// Longest start of the newest reply that is kept for the summary line.
const PREVIEW: usize = 80;

/// Cannot occur in a room id or an event id.
const SEPARATOR: char = '\n';

/// Names a timeline among the open ones: a room's, or one of its threads'.
pub fn key(room_id: &str, root: Option<&str>) -> String {
    match root {
        Some(root) => format!("{room_id}{SEPARATOR}{root}"),
        None => room_id.to_owned(),
    }
}

pub fn is_thread(key: &str) -> bool {
    key.contains(SEPARATOR)
}

/// A timeline of the thread that starts at `root`: that message, then the replies.
/// Whatever is sent through it goes into the thread, with the fallback that lets
/// clients without threads show it as a reply.
pub async fn open(room: &Room, root: &str) -> Result<Timeline> {
    let root_event_id = EventId::parse(root).map_err(|_| anyhow!("invalid event id"))?;
    room.timeline_builder()
        .with_internal_id_prefix(super::next_timeline_prefix())
        .with_focus(TimelineFocus::Thread { root_event_id })
        .track_read_marker_and_receipts(TimelineReadReceiptTracking::MessageLikeEvents)
        .build()
        .await
        .sdk_context("could not open the thread")
}

/// What to show under a message that started a thread. Text from other people is
/// made safe for the terminal here. A newest reply by someone `ignored` is not
/// previewed (the number of replies is the server's, and still counts it).
pub fn summary(content: &MsgLikeContent, ignored: &HashSet<OwnedUserId>) -> Option<ThreadSummary> {
    let summary = content.thread_summary.as_ref()?;
    // Every reply was deleted: nothing to announce.
    if summary.num_replies == 0 {
        return None;
    }
    let (latest, latest_sender) = if let TimelineDetails::Ready(event) = &summary.latest_event
        && !ignored.contains(&event.sender)
    {
        let text = match event.content.as_message() {
            Some(message) => sanitize_line(message.body()),
            None => "a message".to_owned(),
        };
        (
            Some((
                sanitize_line(&shown_name(&event.sender_profile, &event.sender)),
                truncate(text.trim(), PREVIEW),
            )),
            Some(event.sender.to_string()),
        )
    } else {
        (None, None)
    };
    Some(ThreadSummary {
        replies: summary.num_replies,
        latest,
        latest_sender,
        unread: false,
        demo_replies: Vec::new(),
    })
}

/// The newest reply read in each thread (by `key`) during this session. Servers that
/// keep one read receipt per room and user never confirm a thread's receipt, and the
/// thread would stay "new" although it was just read.
#[derive(Clone, Default)]
pub struct ReadMarks(Arc<Mutex<ReadMarksInner>>);

#[derive(Default)]
struct ReadMarksInner {
    /// Counts the requests to mark something read, in the order they were made.
    tickets: u64,
    /// Per thread: the ticket of the request, and the ids the newest reply read goes by.
    read: HashMap<String, (u64, Vec<String>)>,
}

impl ReadMarks {
    /// A place in the order of requests. Receipts are sent in the background and may
    /// finish in any order; a later request's mark is never replaced by an earlier one's.
    pub fn ticket(&self) -> u64 {
        match self.0.lock() {
            Ok(mut marks) => {
                marks.tickets += 1;
                marks.tickets
            }
            Err(_) => 0,
        }
    }

    pub fn set(&self, key: String, ticket: u64, newest: Vec<String>) {
        if let Ok(mut marks) = self.0.lock()
            && marks.read.get(&key).is_none_or(|(at, _)| *at < ticket)
        {
            marks.read.insert(key, (ticket, newest));
        }
    }

    fn covers(&self, key: &str, newest: &str) -> bool {
        self.0.lock().is_ok_and(|marks| {
            marks
                .read
                .get(key)
                .is_some_and(|(_, read)| read.iter().any(|id| id == newest))
        })
    }
}

/// The ids the newest reply of a thread's timeline goes by in the thread's summary:
/// its own, and that of its latest edit. Reactions, deletions and deleted messages
/// are not replies there, and neither are messages still on their way.
pub async fn newest_reply(timeline: &Timeline) -> Vec<String> {
    let items = timeline.items().await;
    let newest = items
        .iter()
        .rev()
        .filter_map(|item| item.as_event())
        .find(|event| {
            event.event_id().is_some()
                && event.is_remote_event()
                && !event
                    .content()
                    .as_msglike()
                    .is_some_and(|content| matches!(content.kind, MsgLikeKind::Redacted))
        });
    let Some(newest) = newest else {
        return Vec::new();
    };
    let mut ids: Vec<String> = newest
        .event_id()
        .map(|id| id.to_string())
        .into_iter()
        .collect();
    let edit = newest
        .latest_edit_json()
        .and_then(|edit| edit.get_field::<String>("event_id").ok().flatten());
    ids.extend(edit);
    ids
}

/// Event ids of the messages in `items` whose threads have a newest reply the user
/// has not read (see [`reply_unread`]). A reply by someone `ignored` is not one to
/// read. Only looks at what is stored; nothing is loaded or kept per thread.
pub async fn unread(
    room: &Room,
    items: &Vector<Arc<TimelineItem>>,
    read: &ReadMarks,
    ignored: &HashSet<OwnedUserId>,
) -> HashSet<String> {
    let mut unread = HashSet::new();
    let mut room_wide = RoomWideReceipts::default();
    for event in items.iter().filter_map(|item| item.as_event()) {
        let Some(root) = event.event_id() else {
            continue;
        };
        let Some(summary) = event.content().thread_summary() else {
            continue;
        };
        // Without knowing the newest reply there is nothing to compare with.
        let TimelineDetails::Ready(newest) = &summary.latest_event else {
            continue;
        };
        let TimelineEventItemId::EventId(newest_id) = &newest.identifier else {
            continue;
        };
        if ignored.contains(&newest.sender) {
            continue;
        }
        if reply_unread(
            room,
            root,
            newest_id,
            &newest.sender,
            newest.timestamp,
            read,
            &mut room_wide,
        )
        .await
        {
            unread.insert(root.to_string());
        }
    }
    unread
}

/// Whether `newest`, the newest reply in the thread that starts at `root`, is one the
/// user has not read: it is someone else's, and neither this session nor a read
/// receipt of the user's covers it.
async fn reply_unread(
    room: &Room,
    root: &EventId,
    newest: &EventId,
    sender: &UserId,
    at: MilliSecondsSinceUnixEpoch,
    read: &ReadMarks,
    room_wide: &mut RoomWideReceipts,
) -> bool {
    let own = room.own_user_id();
    if sender == own
        || read.covers(
            &key(room.room_id().as_str(), Some(root.as_str())),
            newest.as_str(),
        )
    {
        return false;
    }
    let covers = |seen_at: &EventId, receipt: &Receipt| {
        seen_at.as_str() == newest.as_str() || receipt.ts.is_some_and(|ts| ts >= at)
    };
    // A receipt for the thread, or one for the whole room as clients without
    // threads send it. The room's own ("main") receipt says nothing about threads.
    let thread = ReceiptThread::Thread(root.to_owned());
    for kind in [ReceiptType::Read, ReceiptType::ReadPrivate] {
        #[cfg(test)]
        count_load(room);
        if let Ok(Some((seen_at, receipt))) = room.load_user_receipt(kind, &thread, own).await
            && covers(&seen_at, &receipt)
        {
            return false;
        }
    }
    !room_wide
        .get(room)
        .await
        .iter()
        .any(|(seen_at, receipt)| covers(seen_at, receipt))
}

/// The user's read receipts for the whole room, as clients without threads send
/// them. They are the same for every thread: loaded from the store once for all the
/// threads looked at together (a room's timeline can hold many), and only when one
/// of them needs them.
#[derive(Default)]
struct RoomWideReceipts(Option<Vec<(OwnedEventId, Receipt)>>);

impl RoomWideReceipts {
    async fn get(&mut self, room: &Room) -> &[(OwnedEventId, Receipt)] {
        if self.0.is_none() {
            let mut loaded = Vec::new();
            for kind in [ReceiptType::Read, ReceiptType::ReadPrivate] {
                #[cfg(test)]
                count_load(room);
                if let Ok(Some(receipt)) = room
                    .load_user_receipt(kind, &ReceiptThread::Unthreaded, room.own_user_id())
                    .await
                {
                    loaded.push(receipt);
                }
            }
            self.0 = Some(loaded);
        }
        self.0.as_deref().unwrap_or_default()
    }
}

/// The receipts loaded from the store so far, by room: tests count them.
#[cfg(test)]
pub(super) static RECEIPT_LOADS: std::sync::Mutex<std::collections::BTreeMap<String, usize>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

#[cfg(test)]
fn count_load(room: &Room) {
    *RECEIPT_LOADS
        .lock()
        .unwrap()
        .entry(room.room_id().to_string())
        .or_default() += 1;
}

/// Threads asked for at a time; more of a page than this is not looked at.
const LIST_PAGE: u32 = 25;
/// How long the homeserver has for a page of its thread list.
const LIST_TIMEOUT: Duration = Duration::from_secs(20);
/// Longest start of a thread's first message, and longest name, kept for the list.
const LIST_TEXT: usize = 200;
const LIST_NAME: usize = 64;
/// A continuation token longer than this is not one a sane server sends: the list
/// ends there.
const LIST_TOKEN: usize = 1024;

/// A page of a room's threads, the answer to [`super::MatrixCmd::ListThreads`].
pub struct ThreadPage {
    pub room_id: String,
    pub generation: u64,
    /// Newest activity first, as the server orders them.
    pub threads: Vec<ThreadInfo>,
    /// Where the list continues; `None` at its end.
    pub next: Option<String>,
}

/// Counts only: the room, the threads and the server's token stay out of logs.
impl std::fmt::Debug for ThreadPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadPage")
            .field("generation", &self.generation)
            .field("threads", &self.threads.len())
            .field("more", &self.next.is_some())
            .finish()
    }
}

/// One page of the room's threads from the homeserver's thread list, starting at
/// `from` (`None`: the newest). `mine` asks only for threads the user took part in.
/// The SDK decrypts the first messages of encrypted rooms. Threads started by ignored
/// users are left out, as their first message is in the room. `None` when the server
/// has no such list or could not give it; the reason is logged without the server's
/// words.
pub async fn list(
    room: &Room,
    from: Option<String>,
    mine: bool,
    read: &ReadMarks,
    ignored: &IgnoredSet,
) -> Option<(Vec<ThreadInfo>, Option<String>)> {
    let options = ListThreadsOptions {
        include_threads: if mine {
            IncludeThreads::Participated
        } else {
            IncludeThreads::All
        },
        from,
        limit: Some(UInt::from(LIST_PAGE)),
    };
    let roots = match rt::timeout(LIST_TIMEOUT, room.list_threads(options)).await {
        Ok(Ok(roots)) => roots,
        Ok(Err(err)) => {
            let status = err
                .as_client_api_error()
                .map(|err| err.status_code.as_u16());
            tracing::warn!(?status, "the thread list could not be loaded");
            return None;
        }
        Err(_) => {
            tracing::warn!("the thread list took too long");
            return None;
        }
    };
    let mut names = HashMap::new();
    let mut room_wide = RoomWideReceipts::default();
    let mut threads = Vec::new();
    for root in roots.chunk.into_iter().take(LIST_PAGE as usize) {
        if let Some(thread) = describe(room, root, read, ignored, &mut names, &mut room_wide).await
        {
            threads.push(thread);
        }
    }
    let next = roots
        .prev_batch_token
        .filter(|token| !token.is_empty() && token.len() <= LIST_TOKEN);
    Some((threads, next))
}

/// What the list shows of the thread that starts at `root`.
async fn describe(
    room: &Room,
    root: TimelineEvent,
    read: &ReadMarks,
    ignored: &IgnoredSet,
    names: &mut HashMap<(OwnedRoomId, OwnedUserId), String>,
    room_wide: &mut RoomWideReceipts,
) -> Option<ThreadInfo> {
    let root_id = root.event_id()?.to_owned();
    let sender = root.sender()?;
    let own = room.own_user_id();
    if sender != own && ignored.contains(&sender) {
        return None;
    }
    let mut thread = ThreadInfo {
        root: root_id.to_string(),
        sender: short_name(room, &sender, names).await,
        text: first_message(&root),
        replies: root.thread_summary.summary().map_or(0, |s| s.num_replies),
        mine: sender == own || participated(&root),
        ..ThreadInfo::default()
    };
    // Who wrote the newest reply, and when, are readable in encrypted rooms too.
    if let Some(newest) = root.bundled_latest_thread_event()
        && let Some(who) = newest.sender()
        && (who == own || !ignored.contains(&who))
    {
        let at = newest.timestamp_raw();
        thread.latest_when = at.and_then(when);
        if let (Some(id), Some(at)) = (newest.event_id(), at) {
            thread.unread = reply_unread(room, &root_id, id, &who, at, read, room_wide).await;
        }
        thread.latest_sender = Some(short_name(room, &who, names).await);
    }
    Some(thread)
}

async fn short_name(
    room: &Room,
    user: &UserId,
    names: &mut HashMap<(OwnedRoomId, OwnedUserId), String>,
) -> String {
    crate::model::fit_name(
        &search::cached_sender_name(room, user, names).await,
        LIST_NAME,
    )
}

/// The first line of the message a thread starts at, or what stands in for it.
fn first_message(root: &TimelineEvent) -> String {
    if root.kind.is_utd() {
        return "Encrypted message".to_owned();
    }
    if moderation::is_event_redacted(root) {
        return "[message deleted]".to_owned();
    }
    let line = search::extract_event_text(root, &[])
        .map(|text| first_line(&text, LIST_TEXT))
        .unwrap_or_default();
    if line.is_empty() {
        "[message]".to_owned()
    } else {
        line
    }
}

/// Whether the server says the user took part in the thread, in the summary it adds
/// to the thread's first message.
fn participated(root: &TimelineEvent) -> bool {
    #[derive(serde::Deserialize)]
    struct Unsigned {
        #[serde(rename = "m.relations")]
        relations: Option<Relations>,
    }
    #[derive(serde::Deserialize)]
    struct Relations {
        #[serde(rename = "m.thread")]
        thread: Option<Bundled>,
    }
    #[derive(serde::Deserialize)]
    struct Bundled {
        #[serde(default)]
        current_user_participated: bool,
    }
    matches!(
        root.raw().get_field::<Unsigned>("unsigned"),
        Ok(Some(Unsigned {
            relations: Some(Relations {
                thread: Some(Bundled {
                    current_user_participated: true
                })
            })
        }))
    )
}

/// When something happened, in short: the time if it was today, else the day.
fn when(at: MilliSecondsSinceUnixEpoch) -> Option<String> {
    let (day, time) = moderation::format_day_time(at);
    match day.as_str() {
        "" => None,
        "Today" => Some(time),
        _ => Some(day),
    }
}

/// Finishes the messages of a snapshot: marks the threads with unread replies, or,
/// inside a thread, drops the summary line (the replies are right there).
pub fn annotate(messages: &mut [Message], in_thread: bool, unread: &HashSet<String>) {
    for message in messages {
        if in_thread {
            message.thread = None;
        } else if let (Some(thread), Some(ItemRef::Event(id))) =
            (&mut message.thread, &message.item)
        {
            thread.unread = unread.contains(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::ruma::RoomId;

    use super::*;

    #[test]
    fn keys_tell_rooms_and_threads_apart() {
        assert_eq!(key("!room:example.org", None), "!room:example.org");
        assert!(!is_thread(&key("!room:example.org", None)));
        let thread = key("!room:example.org", Some("$event"));
        assert!(is_thread(&thread));
        assert_ne!(thread, key("!room:example.org", Some("$other")));
    }

    #[test]
    fn read_marks_cover_only_the_newest_reply_that_was_read() {
        let marks = ReadMarks::default();
        let key = "room\n$root".to_owned();
        assert!(!marks.covers(&key, "$one"));
        let (first, second) = (marks.ticket(), marks.ticket());
        marks.set(
            key.clone(),
            second,
            vec!["$two".to_owned(), "$edit".to_owned()],
        );
        // A message goes by its own id and by that of its latest edit.
        assert!(marks.covers(&key, "$two") && marks.covers(&key, "$edit"));
        // An earlier request that finishes late does not turn the mark back.
        marks
            .clone()
            .set(key.clone(), first, vec!["$one".to_owned()]);
        assert!(marks.covers(&key, "$two") && !marks.covers(&key, "$one"));
        // A newer reply, or another thread, is unread.
        assert!(!marks.covers(&key, "$three"));
        assert!(!marks.covers("room\n$other", "$two"));
    }

    #[test]
    fn summaries_are_marked_unread_and_hidden_inside_a_thread() {
        let root = |id: &str| Message {
            item: Some(ItemRef::Event(id.to_owned())),
            thread: Some(ThreadSummary {
                replies: 2,
                ..ThreadSummary::default()
            }),
            ..Message::default()
        };
        let unread = HashSet::from(["$b".to_owned()]);
        let mut messages = vec![root("$a"), root("$b"), Message::default()];
        annotate(&mut messages, false, &unread);
        assert!(!messages[0].thread.as_ref().unwrap().unread);
        assert!(messages[1].thread.as_ref().unwrap().unread);
        annotate(&mut messages, true, &unread);
        assert!(messages.iter().all(|m| m.thread.is_none()));
    }

    /// Waits for a timeline update of `generation` that `check` accepts.
    async fn until<T>(
        events: &mut tokio::sync::mpsc::UnboundedReceiver<crate::matrix::MatrixEvent>,
        what: &str,
        generation: u64,
        mut check: impl FnMut(&[Message]) -> Option<T>,
    ) -> T {
        use crate::matrix::MatrixEvent;
        // The last update of `generation` that was not yet it, told on a timeout: how
        // many messages, and each one's text, whether it has an event and is sent.
        let mut last = String::from("no update of this generation");
        let wait = async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err)
                    | MatrixEvent::Error(err)
                    | MatrixEvent::ActionFailed(err) => panic!("{what}: {err}"),
                    MatrixEvent::Timeline {
                        generation: seen,
                        messages,
                        ..
                    } if seen == generation => {
                        if let Some(found) = check(&messages) {
                            return found;
                        }
                        let shown: Vec<String> = messages
                            .iter()
                            .map(|m| {
                                let event = matches!(m.item, Some(ItemRef::Event(_)));
                                format!("{:?} event={event} {:?}", m.body, m.state)
                            })
                            .collect();
                        last = format!("{} messages: {shown:?}", messages.len());
                    }
                    _ => {}
                }
            }
        };
        match tokio::time::timeout(std::time::Duration::from_secs(30), wait).await {
            Ok(found) => found,
            Err(_) => panic!("timed out waiting for {what}; last seen: {last}"),
        }
    }

    /// The event id of the confirmed message with this text.
    fn sent(messages: &[Message], body: &str) -> Option<String> {
        messages.iter().find_map(|m| match &m.item {
            Some(ItemRef::Event(id))
                if m.body == body && m.state == crate::model::SendState::Sent =>
            {
                Some(id.clone())
            }
            _ => None,
        })
    }

    /// Threads against a local homeserver, with the worker as one user and a plain SDK
    /// client as a second one in the same unencrypted room:
    /// `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=... cargo test threads -- --ignored`
    /// (the second user defaults to bob/bobpass, the room to the first one both joined).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn threads_hide_summarize_show_and_send() {
        use crate::format::parse;
        use crate::matrix::{MatrixCmd, MatrixEvent, spawn};
        use crate::paths::Paths;
        use matrix_sdk::Client;
        use matrix_sdk::config::SyncSettings;
        use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;

        let var = |name: &str| std::env::var(name).ok();
        let homeserver =
            var("MATRIX_RAT_TEST_HOMESERVER").unwrap_or_else(|| "http://127.0.0.1:6167".into());
        let username = var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let other_name = var("MATRIX_RAT_TEST_USER2").unwrap_or_else(|| "bob".into());
        let other_password = var("MATRIX_RAT_TEST_PASSWORD2").unwrap_or_else(|| "bobpass".into());

        // The other user: a plain client, as any other Matrix program would be.
        let other = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .expect("client");
        other
            .matrix_auth()
            .login_username(&other_name, &other_password)
            .send()
            .await
            .expect("login of the second user");
        other.event_cache().subscribe().expect("event cache");
        other
            .sync_once(SyncSettings::default())
            .await
            .expect("sync");
        let syncing = tokio::spawn({
            let other = other.clone();
            async move {
                let _ = other.sync(SyncSettings::default()).await;
            }
        });

        let root_dir =
            std::env::temp_dir().join(format!("matrix-rat-thread-{}", std::process::id()));
        let paths = Paths {
            config_dir: root_dir.join("config"),
            data_dir: root_dir.join("data"),
            log_dir: root_dir.join("log"),
            runtime_dir: root_dir.join("run"),
            profile: "it".into(),
        };
        paths.create_dirs().unwrap();
        let (handle, mut events) = spawn(paths);
        handle.send(MatrixCmd::LoginPassword {
            homeserver,
            username,
            password: password.into(),
        });

        // A room both are in, without encryption (the events are read as sent below).
        let find_room = async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    MatrixEvent::Rooms(rooms) => {
                        let wanted = var("MATRIX_RAT_TEST_ROOM");
                        let shared = rooms.iter().find(|summary| {
                            !summary.is_pending()
                                && wanted.as_ref().is_none_or(|id| *id == summary.id)
                                && RoomId::parse(&summary.id)
                                    .ok()
                                    .and_then(|id| other.get_room(&id))
                                    .is_some_and(|room| {
                                        room.state() == matrix_sdk::RoomState::Joined
                                            && !room.encryption_state().is_encrypted()
                                    })
                        });
                        if let Some(summary) = shared {
                            return summary.id.clone();
                        }
                    }
                    _ => {}
                }
            }
        };
        let room_id = tokio::time::timeout(std::time::Duration::from_secs(30), find_room)
            .await
            .expect("no unencrypted room that both users have joined");
        let other_room = other.get_room(&RoomId::parse(&room_id).unwrap()).unwrap();

        let stamp = std::process::id();
        let text = |what: &str| format!("thread test {stamp}: {what}");
        let send = |root: Option<&str>, body: &str, reply_to: Option<&str>| {
            let command = MatrixCmd::Send {
                room_id: room_id.clone(),
                message: parse(body),
                reply_to: reply_to.map(str::to_owned),
            };
            handle.send(match root {
                Some(root) => MatrixCmd::InThread {
                    root: root.to_owned(),
                    command: Box::new(command),
                },
                None => command,
            });
        };

        // A message in the room, to hang the thread on.
        handle.send(MatrixCmd::OpenRoom {
            room_id: room_id.clone(),
            generation: 1,
        });
        send(None, &text("root"), None);
        let root = until(&mut events, "the first message", 1, |m| {
            sent(m, &text("root"))
        })
        .await;

        // An empty thread shows its first message; what is sent there starts the thread.
        handle.send(MatrixCmd::OpenThread {
            room_id: room_id.clone(),
            root: root.clone(),
            generation: 2,
        });
        until(&mut events, "the empty thread", 2, |m| {
            (m.len() == 1 && sent(m, &text("root")).as_deref() == Some(root.as_str())).then_some(())
        })
        .await;
        send(Some(&root), &text("one"), None);
        let one = until(&mut events, "the first reply", 2, |m| sent(m, &text("one"))).await;
        send(Some(&root), &text("two"), None);
        let two = until(&mut events, "the second reply", 2, |m| {
            sent(m, &text("two"))
        })
        .await;
        // A reply to a message of the thread.
        send(Some(&root), &text("three"), Some(&one));
        let three = until(&mut events, "the reply in the thread", 2, |m| {
            let at = m.iter().position(|m| m.body == text("three"))?;
            // It quotes what it answers; plain thread messages quote nothing.
            assert_eq!(
                m[at].reply_to.as_ref().map(|r| r.1.clone()),
                Some(text("one"))
            );
            assert!(m.iter().filter(|m| m.reply_to.is_some()).count() == 1);
            assert!(m.iter().all(|m| m.thread.is_none()));
            sent(m, &text("three"))
        })
        .await;

        // What went over the wire, as the other client receives it: the thread relation
        // with a fallback for clients without threads.
        let relation = |event_id: String| {
            let other_room = other_room.clone();
            async move {
                let id = EventId::parse(&event_id).unwrap();
                let event = other_room.event(&id, None).await.expect("event");
                let json: serde_json::Value =
                    serde_json::from_str(event.raw().json().get()).unwrap();
                json["content"]["m.relates_to"].clone()
            }
        };
        let of_two = relation(two.clone()).await;
        assert_eq!(of_two["rel_type"], "m.thread");
        assert_eq!(of_two["event_id"], root.as_str());
        assert_eq!(of_two["is_falling_back"], true);
        assert_eq!(of_two["m.in_reply_to"]["event_id"], one.as_str());
        let of_one = relation(one.clone()).await;
        assert_eq!(of_one["is_falling_back"], true);
        assert_eq!(of_one["m.in_reply_to"]["event_id"], root.as_str());
        let of_three = relation(three).await;
        assert_eq!(of_three["rel_type"], "m.thread");
        assert_eq!(of_three["event_id"], root.as_str());
        assert_eq!(of_three["m.in_reply_to"]["event_id"], one.as_str());
        assert_ne!(of_three["is_falling_back"], true);

        // The room hides the replies and counts them under the first message.
        handle.send(MatrixCmd::OpenRoom {
            room_id: room_id.clone(),
            generation: 3,
        });
        let in_room = |m: &[Message], replies: u32, latest: &str| {
            let first = m
                .iter()
                .find(|m| matches!(&m.item, Some(ItemRef::Event(id)) if *id == root))?;
            let thread = first.thread.clone()?;
            let newest = thread.latest.as_ref().map(|latest| latest.1.clone());
            (thread.replies == replies && newest == Some(text(latest))).then_some(())?;
            for hidden in ["one", "two", "three", "four", "five"] {
                assert!(
                    m.iter().all(|m| m.body != text(hidden)),
                    "{hidden} is in the room"
                );
            }
            Some(thread)
        };
        let thread = until(&mut events, "the summary", 3, |m| in_room(m, 3, "three")).await;
        assert!(!thread.unread, "our own replies are not unread");

        // The other user answers in the thread: counted, attributed and unread.
        let their_thread = threads_open(&other_room, &root).await;
        their_thread
            .send(RoomMessageEventContent::text_plain(text("four")).into())
            .await
            .expect("send from the second user");
        let thread = until(&mut events, "their reply in the summary", 3, |m| {
            in_room(m, 4, "four")
        })
        .await;
        let latest = thread.latest.expect("latest reply");
        assert!(
            latest.0.to_lowercase().contains(&other_name),
            "{}",
            latest.0
        );
        assert!(thread.unread, "a reply of someone else is unread");

        // Opening the thread shows it; reading it there clears the mark.
        handle.send(MatrixCmd::OpenThread {
            room_id: room_id.clone(),
            root: root.clone(),
            generation: 4,
        });
        until(&mut events, "their reply in the thread", 4, |m| {
            // The "new messages" line may or may not stand before their reply.
            let bodies: Vec<&str> = m
                .iter()
                .filter(|m| m.kind != crate::model::MsgKind::ReadMarker)
                .map(|m| m.body.as_str())
                .collect();
            let wanted = ["root", "one", "two", "three", "four"].map(text);
            (bodies == wanted).then_some(())
        })
        .await;
        handle.send(MatrixCmd::InThread {
            root: root.clone(),
            command: Box::new(MatrixCmd::MarkRead {
                room_id: room_id.clone(),
                public: true,
            }),
        });
        // Reading is noted a moment after the command; ask for the room until it shows.
        let mut generation = 4;
        let read = async {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                generation += 1;
                handle.send(MatrixCmd::OpenRoom {
                    room_id: room_id.clone(),
                    generation,
                });
                let thread = until(&mut events, "the room again", generation, |m| {
                    in_room(m, 4, "four")
                })
                .await;
                if !thread.unread {
                    return;
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(20), read)
            .await
            .expect("the thread stayed unread after it was read");

        // Once more, with a reaction as the last thing that happened in the thread:
        // what counts as read is the newest reply, not the reaction to it.
        their_thread
            .send(RoomMessageEventContent::text_plain(text("five")).into())
            .await
            .expect("send from the second user");
        generation += 1;
        handle.send(MatrixCmd::OpenThread {
            room_id: room_id.clone(),
            root: root.clone(),
            generation,
        });
        let five = until(&mut events, "the fifth reply", generation, |m| {
            sent(m, &text("five"))
        })
        .await;
        their_thread
            .toggle_reaction(
                &TimelineEventItemId::EventId(EventId::parse(&five).unwrap()),
                "👍",
            )
            .await
            .expect("reaction from the second user");
        until(&mut events, "the reaction", generation, |m| {
            let reacted = m.iter().find(|m| m.body == text("five"))?;
            (!reacted.reactions.is_empty()).then_some(())
        })
        .await;
        handle.send(MatrixCmd::InThread {
            root: root.clone(),
            command: Box::new(MatrixCmd::MarkRead {
                room_id: room_id.clone(),
                public: true,
            }),
        });
        let read = async {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                generation += 1;
                handle.send(MatrixCmd::OpenRoom {
                    room_id: room_id.clone(),
                    generation,
                });
                let thread = until(&mut events, "the room once more", generation, |m| {
                    in_room(m, 5, "five")
                })
                .await;
                if !thread.unread {
                    return;
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(20), read)
            .await
            .expect("the thread stayed unread after a reaction and reading it");

        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        syncing.abort();
        let _ = other.matrix_auth().logout().await;
        let _ = std::fs::remove_dir_all(root_dir);
    }

    async fn threads_open(room: &Room, root: &str) -> Timeline {
        open(room, root).await.expect("thread timeline")
    }

    /// The homeserver's thread list, through the worker: a fresh room with two threads,
    /// listed newest activity first, also when only the user's own are asked for.
    /// `cargo test thread_list -- --ignored` against a local homeserver (user alice
    /// unless `MATRIX_RAT_TEST_USER` says otherwise).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver (scripts/dev-homeserver.sh)"]
    async fn the_thread_list_shows_two_threads_newest_activity_first() {
        use matrix_sdk::Client;
        use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoom;
        use matrix_sdk::ruma::events::relation::Thread;
        use matrix_sdk::ruma::events::room::message::{Relation, RoomMessageEventContent};
        use std::time::Duration;

        use crate::matrix::{MatrixCmd, MatrixEvent, ThreadPage, spawn};
        use crate::paths::Paths;

        let var = |name: &str| std::env::var(name).ok();
        let homeserver =
            var("MATRIX_RAT_TEST_HOMESERVER").unwrap_or_else(|| "http://127.0.0.1:6167".into());
        let user = var("MATRIX_RAT_TEST_USER").unwrap_or_else(|| "alice".into());
        let password = var("MATRIX_RAT_TEST_PASSWORD").unwrap_or_else(|| "alicepass".into());

        // A room of its own, with two threads written by a plain client: the second
        // one's replies are the newest.
        let setup = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .expect("client");
        setup
            .matrix_auth()
            .login_username(&user, &password)
            .await
            .expect("login of the setup client");
        let room = setup
            .create_room(CreateRoom::new())
            .await
            .expect("create room");
        let room_id = room.room_id().to_string();
        let stamp = std::process::id();
        let text = |what: &str| format!("thread list test {stamp}: {what}");
        let send = |body: String, root: Option<matrix_sdk::ruma::OwnedEventId>| {
            let room = room.clone();
            async move {
                let mut content = RoomMessageEventContent::text_plain(body);
                if let Some(root) = root {
                    content.relates_to = Some(Relation::Thread(Thread::plain(root.clone(), root)));
                }
                room.send(content).await.expect("send").response.event_id
            }
        };
        let first = send(text("first"), None).await;
        send(text("an answer to the first"), Some(first.clone())).await;
        let second = send(text("second"), None).await;
        send(text("an answer to the second"), Some(second.clone())).await;
        send(text("another answer to the second"), Some(second.clone())).await;
        let _ = setup.matrix_auth().logout().await;

        let root_dir =
            std::env::temp_dir().join(format!("matrix-rat-thread-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root_dir);
        let paths = Paths {
            config_dir: root_dir.join("config"),
            data_dir: root_dir.join("data"),
            log_dir: root_dir.join("log"),
            runtime_dir: root_dir.join("run"),
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
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    MatrixEvent::Rooms(rooms) if rooms.iter().any(|r| r.id == room_id) => return,
                    _ => {}
                }
            }
        })
        .await
        .expect("the room did not show up");

        for (generation, mine) in [(1, false), (2, true)] {
            handle.send(MatrixCmd::ListThreads {
                room_id: room_id.clone(),
                from: None,
                mine,
                generation,
            });
            let page: ThreadPage = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match events.recv().await.expect("worker stopped") {
                        MatrixEvent::ThreadsFailed { .. } => panic!("the thread list failed"),
                        MatrixEvent::Threads(page) if page.generation == generation => {
                            return page;
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("no thread list");
            assert_eq!(page.room_id, room_id);
            let roots: Vec<&str> = page.threads.iter().map(|t| t.root.as_str()).collect();
            assert_eq!(roots, [second.as_str(), first.as_str()], "mine: {mine}");
            let (newest, oldest) = (&page.threads[0], &page.threads[1]);
            assert_eq!(newest.text, text("second"));
            assert_eq!(newest.replies, 2);
            assert_eq!(oldest.text, text("first"));
            assert_eq!(oldest.replies, 1);
            for thread in &page.threads {
                // All by the user: theirs, with nothing unread.
                assert!(thread.mine && !thread.unread, "{thread:?}");
                // Who replied last and when is only known where the server sends the
                // newest reply as a whole event; some (continuwuity) send its content
                // only. Either both are there or neither.
                assert_eq!(thread.latest_sender.is_some(), thread.latest_when.is_some());
                assert!(!thread.sender.is_empty());
            }
            assert!(page.next.is_none());
        }

        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = std::fs::remove_dir_all(root_dir);
    }
}
