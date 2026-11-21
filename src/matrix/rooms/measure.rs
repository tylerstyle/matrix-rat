//! What an open room costs the worker, measured without a homeserver (see
//! `crate::measure` for how to run these): a room of a few thousand messages is synced
//! from the stand-in server in one go, its live timeline opened as `Timelines` opens
//! it, and the work `forward` does on every change of it is timed.
//!
//! The room is made up, the same on every run: people with display names (some of
//! them bridged), texts of mixed lengths, some formatted, replies, reactions, pictures,
//! files and read receipts. See [`sample_room`] for the mix.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use matrix_sdk::authentication::SessionTokens;
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::ruma::OwnedUserId;
use matrix_sdk::search_index::SearchIndexStoreKind;
use matrix_sdk::{AuthSession, Client, SessionMeta};
use matrix_sdk_ui::eyeball_im::Vector;
use matrix_sdk_ui::timeline::{Timeline, TimelineDetails, TimelineItem};
use serde_json::{Value, json};

use crate::matrix::spaces::fake;
use crate::measure::{Times, grew, heap_in_use, mb, time_runs};
use crate::model::Message;

const ROOM: &str = "!measure:localhost";
/// The word only the sample room's last message has (see [`wait_indexed`]).
const MARKER: &str = "zebrafinish";
/// Runs of each timed step (after one to warm up).
const RUNS: usize = 9;

/// Numbers that look random and are the same on every run.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn between(&mut self, low: u64, high: u64) -> usize {
        (low + self.below(high - low)) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    /// `len` characters of what event ids and media addresses are made of.
    fn token(&mut self, len: usize) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        (0..len)
            .map(|_| ALPHABET[self.below(ALPHABET.len() as u64) as usize] as char)
            .collect()
    }
}

const WORDS: &[&str] = &[
    "the",
    "build",
    "works",
    "again",
    "tomorrow",
    "meeting",
    "I",
    "think",
    "we",
    "should",
    "move",
    "it",
    "to",
    "Friday",
    "does",
    "anyone",
    "know",
    "why",
    "this",
    "fails",
    "on",
    "main",
    "ok",
    "thanks",
    "😀",
    "über",
    "café",
    "release",
    "notes",
    "server",
    "down",
    "back",
    "up",
    "looks",
    "good",
    "to",
    "me",
    "https://example.org/some/long/path?x=1",
    "a",
    "bit",
    "later",
    "maybe",
    "👍",
    "lunch",
    "anyone?",
    "the",
    "logs",
    "say",
    "timeout",
    "after",
    "30s",
    "yes",
    "no",
    "probably",
    "and",
    "then",
    "it",
    "crashed",
];

/// About `len` bytes of chat text.
fn words(rng: &mut Lcg, len: usize) -> String {
    let mut text = String::new();
    while text.len() < len {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(WORDS[rng.below(WORDS.len() as u64) as usize]);
    }
    text
}

/// Who writes in the room: `ME` first, then people of one server and of a bridge.
fn people(rng: &mut Lcg) -> Vec<(String, String)> {
    let mut people = vec![(fake::ME.to_owned(), "Alice Example".to_owned())];
    for n in 0..24 {
        let id = if n < 14 {
            format!("@{}{n}:matrix.org", ["sam", "kim", "robin", "alex"][n % 4])
        } else {
            format!(
                "@telegram_{}:t2bot.io",
                100_000_000 + rng.below(899_999_999)
            )
        };
        let len = rng.between(5, 24);
        let mut name = words(rng, len);
        let mut end = len.min(name.len());
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
        people.push((id, name.trim().to_owned()));
    }
    people
}

/// The sync's account of a joined room with `messages` messages (and the reactions on
/// them, which are events but no lines of their own), spread over 30 days:
///
/// - texts of 10 to 60 bytes (60%), 60 to 200 (25%), 200 to 600 (12%), 600 to 2000 (3%);
/// - 3% notices, 2% emotes, 4% pictures (with a thumbnail), 2% files;
/// - 15% of the texts formatted (HTML), 12% replies to one of the 50 before;
/// - 60% carry `m.mentions` as current clients send it, 5% mention someone;
/// - 15% have reactions: one to three emoji, from one to four people each;
/// - 10% are our own; read receipts of 20 people on the last 60.
fn sample_room(messages: usize) -> Value {
    let people = people(&mut Lcg(0x5eed));
    let mut state = vec![json!({
        "type": "m.room.create", "state_key": "", "sender": fake::ME,
        "event_id": "$create", "origin_server_ts": 1,
        "content": { "creator": fake::ME, "room_version": "10" },
    })];
    for (n, (id, name)) in people.iter().enumerate() {
        state.push(json!({
            "type": "m.room.member", "state_key": id, "sender": id,
            "event_id": format!("$member{n}"), "origin_server_ts": 2,
            "content": { "membership": "join", "displayname": name },
        }));
    }
    let start: u64 = 1_790_000_000_000;
    let step = 30 * 24 * 3600 * 1000 / messages.max(1) as u64;
    let (mut timeline, ids) = sample_timeline(&mut Lcg(0x5eed + 1), &people, start, step, messages);
    // The last message says a word no other does: once it can be found, everything
    // before it has been indexed for search too.
    if let Some(last) = timeline
        .iter_mut()
        .rev()
        .find(|event| event["type"] == "m.room.message")
    {
        last["content"] = json!({ "msgtype": "m.text", "body": format!("that is all {MARKER}") });
    }
    let mut receipts = serde_json::Map::new();
    for (n, (who, _)) in people.iter().enumerate().skip(1).take(20) {
        let at = &ids[ids.len() - 1 - (n * 3) % 60.min(ids.len())];
        let entry = receipts
            .entry(at.clone())
            .or_insert_with(|| json!({ "m.read": {} }));
        entry["m.read"][who] = json!({ "ts": start + messages as u64 * step });
    }
    json!({
        "state": { "events": state },
        "timeline": { "events": timeline, "limited": false },
        "ephemeral": { "events": [ { "type": "m.receipt", "content": receipts } ] },
        "summary": { "m.joined_member_count": people.len() },
    })
}

/// `messages` messages of the mix by `people`, the first sent at `start` (in ms since
/// 1970) and one every `step` ms, with the reactions on them; and their event ids.
fn sample_timeline(
    rng: &mut Lcg,
    people: &[(String, String)],
    start: u64,
    step: u64,
    messages: usize,
) -> (Vec<Value>, Vec<String>) {
    let mut timeline = Vec::with_capacity(messages * 5 / 4);
    let mut ids: Vec<String> = Vec::with_capacity(messages);
    for n in 0..messages {
        let ts = start + n as u64 * step;
        let sender = if rng.chance(10) {
            0
        } else {
            1 + rng.below(people.len() as u64 - 1) as usize
        };
        let len = match rng.below(100) {
            0..60 => rng.between(10, 60),
            60..85 => rng.between(60, 200),
            85..97 => rng.between(200, 600),
            _ => rng.between(600, 2000),
        };
        let body = words(rng, len);
        let kind = rng.below(100);
        let mut content = match kind {
            0..4 => {
                let name = format!("IMG_2026{:04}.jpg", rng.below(10_000));
                json!({
                    "msgtype": "m.image", "body": name,
                    "url": format!("mxc://matrix.org/{}", rng.token(24)),
                    "info": {
                        "w": 1280, "h": 960, "size": 200_000 + rng.below(800_000),
                        "mimetype": "image/jpeg",
                        "thumbnail_url": format!("mxc://matrix.org/{}", rng.token(24)),
                        "thumbnail_info": {
                            "w": 800, "h": 600, "size": 40_000, "mimetype": "image/jpeg",
                        },
                    },
                })
            }
            4..6 => {
                let name = format!("report-{}.pdf", rng.below(1000));
                json!({
                    "msgtype": "m.file", "body": name, "filename": name,
                    "url": format!("mxc://matrix.org/{}", rng.token(24)),
                    "info": { "size": rng.below(5_000_000), "mimetype": "application/pdf" },
                })
            }
            6..9 => json!({ "msgtype": "m.notice", "body": body }),
            9..11 => json!({ "msgtype": "m.emote", "body": body }),
            _ => json!({ "msgtype": "m.text", "body": body }),
        };
        if kind >= 6 && rng.chance(15) {
            content["format"] = json!("org.matrix.custom.html");
            content["formatted_body"] = json!(format!(
                "<p>{} <b>{}</b> <a href=\"https://example.org/x\">link</a></p>",
                body,
                words(rng, 12)
            ));
        }
        if rng.chance(60) {
            content["m.mentions"] = if rng.chance(8) {
                let who = &people[1 + rng.below(people.len() as u64 - 1) as usize].0;
                json!({ "user_ids": [who] })
            } else {
                json!({})
            };
        }
        if !ids.is_empty() && rng.chance(12) {
            let back = 1 + rng.below(50.min(ids.len() as u64)) as usize;
            let target = &ids[ids.len() - back];
            content["m.relates_to"] = json!({ "m.in_reply_to": { "event_id": target } });
        }
        let id = format!("${}", rng.token(43));
        timeline.push(json!({
            "type": "m.room.message", "event_id": id, "sender": people[sender].0,
            "origin_server_ts": ts, "content": content,
        }));
        if rng.chance(15) {
            for key in 0..rng.between(1, 4) {
                for who in 0..rng.between(1, 5) {
                    let reactor = &people[(sender + 1 + who * 3 + key) % people.len()].0;
                    let emoji = ["👍", "❤️", "😂", "🎉", "👀", "🙏"][key];
                    timeline.push(json!({
                        "type": "m.reaction", "event_id": format!("${}", rng.token(43)),
                        "sender": reactor, "origin_server_ts": ts + 1 + who as u64,
                        "content": { "m.relates_to": {
                            "rel_type": "m.annotation", "event_id": id,
                            "key": emoji,
                        } },
                    }));
                }
            }
        }
        ids.push(id);
    }
    (timeline, ids)
}

/// A client of the stand-in server that has synced the sample room of `messages`
/// messages into its event cache (which is subscribed to first, as the app does).
struct Fixture {
    pub server: fake::Server,
    pub client: Client,
    pub room: matrix_sdk::Room,
    /// Heap before the client was made and after the sync.
    pub heap: (usize, usize),
}

async fn fixture(messages: usize) -> Fixture {
    fixture_with(messages, SearchIndexStoreKind::InMemory).await
}

/// [`fixture`], with the search index kept as `search` says. The heap is measured
/// once the search index has taken in the whole room, so that its work, which the SDK
/// does on a task of its own, is not counted in what is measured after.
async fn fixture_with(messages: usize, search: SearchIndexStoreKind) -> Fixture {
    let server = fake::Server::start(json!({ "join": { ROOM: sample_room(messages) } })).await;
    let before = heap_in_use();
    let (client, room) = client_of(&server, search).await;
    wait_indexed(&room).await;
    let after = heap_in_use();
    Fixture {
        server,
        client,
        room,
        heap: (before, after),
    }
}

/// Waits until the search index can find the sample room's last message.
async fn wait_indexed(room: &matrix_sdk::Room) {
    for _ in 0..3000 {
        if room
            .search(MARKER, 1, None)
            .await
            .is_ok_and(|hits| !hits.is_empty())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the sample room was not indexed within a minute");
}

/// A client of `server`, signed in as `ME`, with the event cache subscribed to (as the
/// app has it) and synced once; and the sample room.
async fn client_of(
    server: &fake::Server,
    search: SearchIndexStoreKind,
) -> (Client, matrix_sdk::Room) {
    let client = Client::builder()
        .homeserver_url(&server.url)
        .with_threading_support(super::super::threads::SUPPORT)
        .search_index_store(search)
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
    let room_id = <&matrix_sdk::ruma::RoomId>::try_from(ROOM).unwrap();
    let room = client.get_room(room_id).unwrap();
    (client, room)
}

/// What `forward` keeps between changes of one open timeline.
struct Forward {
    readers: super::super::receipts::Names,
    dedup: super::super::convert::EventDedup,
    own_room_names: super::super::people::OwnRoomNames,
    ignored: tokio::sync::watch::Receiver<super::IgnoredSet>,
    _ignored_tx: tokio::sync::watch::Sender<super::IgnoredSet>,
    read_marks: super::super::threads::ReadMarks,
}

impl Forward {
    fn new() -> Self {
        let (_ignored_tx, ignored) = tokio::sync::watch::channel(super::IgnoredSet::default());
        Self {
            readers: Default::default(),
            dedup: Default::default(),
            own_room_names: Default::default(),
            ignored,
            _ignored_tx,
            read_marks: Default::default(),
        }
    }

    async fn prepare(
        &mut self,
        room: &matrix_sdk::Room,
        items: &Vector<Arc<TimelineItem>>,
    ) -> Vec<Message> {
        super::prepare_messages(
            room,
            items,
            room.own_user_id(),
            &self.ignored,
            &self.own_room_names,
            &mut self.readers,
            &mut self.dedup,
        )
        .await
    }

    /// Everything `forward` does with a change after applying it to its items, but
    /// sending: the look for replies to fetch (none are asked for here), the threads'
    /// unread state, the messages, the thread marks and whether the start is loaded.
    async fn on_change(
        &mut self,
        room: &matrix_sdk::Room,
        items: &Vector<Arc<TimelineItem>>,
    ) -> (Vec<Message>, bool) {
        let unavailable = items
            .iter()
            .filter_map(|item| item.as_event())
            .filter(|event| {
                event
                    .content()
                    .as_msglike()
                    .and_then(|content| content.in_reply_to.as_ref())
                    .is_some_and(|reply| matches!(reply.event, TimelineDetails::Unavailable))
            })
            .count();
        std::hint::black_box(unavailable);
        let unread = super::super::threads::unread(room, items, &self.read_marks).await;
        let mut messages = self.prepare(room, items).await;
        super::super::threads::annotate(&mut messages, false, &unread);
        let start = items.iter().any(|item| item.is_timeline_start());
        (messages, start)
    }
}

/// Times an async step `RUNS` times after one run to warm up.
async fn time_async<T>(mut step: impl AsyncFnMut() -> T) -> Times {
    std::hint::black_box(step().await);
    let mut all = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let start = Instant::now();
        let out = step().await;
        all.push(start.elapsed());
        drop(std::hint::black_box(out));
    }
    all.sort();
    Times(all)
}

/// The SDK's live timeline makes items of every event its room's event cache holds, but
/// a subscriber sees only the last 20 of them until it pages back (matrix-sdk-ui's
/// `MAXIMUM_NUMBER_OF_INITIAL_ITEMS`). Pages back over exactly what is loaded already, so
/// nothing is asked of the server, and gives all of it, as the app's `forward` has it
/// once the room has been scrolled to the top.
async fn show_all(timeline: &Timeline) -> Vector<Arc<TimelineItem>> {
    let all = timeline.items().await.len();
    let mut hidden = all.saturating_sub(timeline.subscribe().await.0.len());
    while hidden > 0 {
        let page = hidden.min(1000);
        let reached_start = timeline
            .paginate_backwards(u16::try_from(page).unwrap())
            .await
            .unwrap();
        assert!(!reached_start, "paging over loaded items asked the server");
        hidden -= page;
    }
    let (items, _) = timeline.subscribe().await;
    assert_eq!(items.len(), all);
    items
}

/// The messages `forward` sends for the sample room of `messages` messages: what the UI
/// holds for a room, made the real way.
pub(crate) async fn sample_messages(messages: usize) -> Vec<Message> {
    let fixture = fixture(messages).await;
    let timeline = super::room_timeline(&fixture.room).await.unwrap();
    let items = show_all(&timeline).await;
    let mut forward = Forward::new();
    forward.on_change(&fixture.room, &items).await.0
}

fn per(bytes: i64, count: usize) -> f64 {
    bytes as f64 / count.max(1) as f64
}

/// The worker's side of an open room of 5000 messages: what its live timeline and the
/// SDK's event cache hold per message, what is given back when no timeline is kept
/// (the event cache shrinks to its last chunk), and what one change of the timeline
/// costs `forward` (it converts every item, every time).
#[tokio::test]
#[ignore = "a measurement: run alone and in release, see crate::measure"]
async fn measure_worker_cost_of_an_open_room() {
    const MESSAGES: usize = 5000;
    if !crate::measure::wanted() {
        return;
    }
    let fixture = fixture(MESSAGES).await;
    let room = &fixture.room;
    let (h_client, h_synced) = fixture.heap;
    println!(
        "[sync] client + stores + event cache after syncing {MESSAGES} messages: {:+.1} MB ({:.0} B/message)",
        mb(grew(h_client, h_synced) as f64),
        per(grew(h_client, h_synced), MESSAGES)
    );

    // The live timeline, opened as `Timelines` opens it.
    let h0 = heap_in_use();
    let first = super::room_timeline(room).await.unwrap();
    let h1 = heap_in_use();
    let shown_first = first.subscribe().await.0.len();
    let items = show_all(&first).await;
    let (_, changes) = first.subscribe().await;
    let events = items
        .iter()
        .filter(|item| item.as_event().is_some())
        .count();
    println!(
        "[timeline] items: {} ({events} events), of which a subscriber sees {shown_first} \
         until it pages back; first timeline: {:+.1} MB ({:.0} B/item)",
        items.len(),
        mb(grew(h0, h1) as f64),
        per(grew(h0, h1), items.len())
    );
    // A second one on the same room shares the event cache: only its own items count.
    let h1 = heap_in_use();
    let second = super::room_timeline(room).await.unwrap();
    let (items2, changes2) = second.subscribe().await;
    let h2 = heap_in_use();
    println!(
        "[timeline] second timeline of the same room: {:+.1} MB ({:.0} B/item)",
        mb(grew(h1, h2) as f64),
        per(grew(h1, h2), items.len())
    );

    // One change: what `forward` does with it, but sending.
    let mut forward = Forward::new();
    let own: OwnedUserId = room.own_user_id().to_owned();
    let ignored: HashSet<OwnedUserId> = HashSet::new();
    let convert = time_runs(RUNS, || {
        std::hint::black_box(super::super::convert::convert(&items, &own, &ignored));
    });
    println!("[per change] convert of all items: {convert}");
    let prepare = time_async(async || forward.prepare(room, &items).await).await;
    println!("[per change] prepare_messages (convert, own name, receipts, dedup): {prepare}");
    let change = time_async(async || forward.on_change(room, &items).await).await;
    println!("[per change] all of forward's work on one change: {change}");

    // What the messages sent to the UI hold, and what replacing them costs there.
    let h3 = heap_in_use();
    let (messages, _) = forward.on_change(room, &items).await;
    let h4 = heap_in_use();
    println!(
        "[messages] {} messages: {:+.2} MB ({:.0} B/message, of which {} B inline)",
        messages.len(),
        mb(grew(h3, h4) as f64),
        per(grew(h3, h4), messages.len()),
        std::mem::size_of::<Message>()
    );
    let mut copies: Vec<Vec<Message>> = (0..RUNS).map(|_| messages.clone()).collect();
    let drop_old = time_runs(RUNS, || drop(copies.pop()));
    println!("[per change] dropping the previous Vec<Message> (the UI's old copy): {drop_old}");
    let clone = time_runs(RUNS, || drop(std::hint::black_box(messages.clone())));
    println!("[reference] a clone of the Vec<Message>: {clone}");

    // No timeline kept: the event cache has no subscriber left and shrinks to its last
    // chunk. What that gives back is what keeping a timeline in `Timelines::cache`
    // costs on top of its items. A timeline's tasks end, and its items go, on their
    // own time after it is dropped; so does the shrink: each step is given a second.
    async fn settled() -> usize {
        tokio::time::sleep(Duration::from_secs(1)).await;
        heap_in_use()
    }
    let events_before = room
        .event_cache()
        .await
        .unwrap()
        .0
        .events()
        .await
        .unwrap()
        .len();
    drop(messages);
    drop((items, changes, items2, changes2));
    let h5 = settled().await;
    drop(second);
    let h6 = settled().await;
    drop(first);
    let h7 = settled().await;
    let events_after = room
        .event_cache()
        .await
        .unwrap()
        .0
        .events()
        .await
        .unwrap()
        .len();
    println!(
        "[cache] dropping the second timeline: {:+.1} MB; the first, and with it the event \
         cache's last subscriber (it shrinks the room from {events_before} events in memory \
         to {events_after}): {:+.1} MB",
        mb(grew(h5, h6) as f64),
        mb(grew(h6, h7) as f64)
    );
    let timeline_bytes = -grew(h5, h6);
    let event_cache_bytes = -grew(h6, h7) - timeline_bytes;
    println!(
        "[cache] so one cached timeline of {MESSAGES} messages holds about {:.1} MB: \
         {:.0} B/message of timeline items and {:.0} B/message ({:.0} B/event) of event \
         cache in memory",
        mb((timeline_bytes + event_cache_bytes) as f64),
        per(timeline_bytes, MESSAGES),
        per(event_cache_bytes, MESSAGES),
        per(
            event_cache_bytes,
            events_before.saturating_sub(events_after)
        )
    );
    // The room is still known and synced after all this.
    assert!(fixture.client.get_room(room.room_id()).is_some());
    drop(fixture.server);
}

/// A room nobody opens: the SDK's event cache keeps every event sync brings in memory
/// (it shrinks a room to its last chunk only when the last subscriber of the room goes
/// away, or when a limited sync leaves a gap), so a quiet session grows with every
/// event of every room. Measures what each event costs while nobody looks, and what
/// subscribing to the room and letting go again gives back.
#[tokio::test]
#[ignore = "a measurement: run alone and in release, see crate::measure"]
async fn measure_event_cache_of_a_room_never_opened() {
    use std::sync::atomic::{AtomicU64, Ordering};
    const BATCHES: u64 = 20;
    const PER_BATCH: usize = 250;
    if !crate::measure::wanted() {
        return;
    }
    let people = people(&mut Lcg(0x5eed));
    let syncs = Arc::new(AtomicU64::new(0));
    let counter = syncs.clone();
    let server = fake::Server::answering(
        json!({ "join": { ROOM: sample_room(40) } }),
        Box::new(move |request| {
            if !request.path.ends_with("/sync") {
                return None;
            }
            // The first sync is the room as it is; each later one brings new messages,
            // as a live room does (not limited: no gap).
            let n = counter.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                return None;
            }
            let start = 1_800_000_000_000 + n * 1_000_000;
            let (events, _) = sample_timeline(&mut Lcg(n), &people, start, 1000, PER_BATCH);
            let sync = json!({
                "next_batch": format!("s{}", n + 1),
                "rooms": { "join": { ROOM: {
                    "timeline": { "events": events, "limited": false },
                } } },
            });
            Some(fake::Reply::Json(sync.to_string()))
        }),
    )
    .await;
    let (client, room) = client_of(&server, SearchIndexStoreKind::InMemory).await;
    wait_indexed(&room).await;
    let start = heap_in_use();
    let mut events = 0usize;
    for _ in 0..BATCHES {
        client.sync_once(SyncSettings::default()).await.unwrap();
        events += PER_BATCH;
    }
    let synced = heap_in_use();
    println!(
        "[unopened] {events} messages synced into a room nobody opened: {:+.1} MB \
         ({:.0} B/message, with the reactions on them; the test's store is in memory too)",
        mb(grew(start, synced) as f64),
        per(grew(start, synced), events)
    );
    // A subscriber that goes away lets the event cache shrink the room.
    let (cache, _drop_handles) = room.event_cache().await.unwrap();
    let (loaded, subscriber) = cache.subscribe().await.unwrap();
    let in_memory = loaded.len();
    drop((loaded, subscriber));
    let mut after = heap_in_use();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        after = heap_in_use();
    }
    let (left, subscriber) = cache.subscribe().await.unwrap();
    println!(
        "[unopened] events in memory before: {in_memory}, after one subscriber came and \
         went: {}; given back: {:.1} MB ({:.0} B per event that was in memory)",
        left.len(),
        mb(-grew(synced, after) as f64),
        per(-grew(synced, after), in_memory.saturating_sub(left.len()))
    );
    assert!(left.len() < in_memory, "{} < {in_memory}", left.len());
    drop((left, subscriber));
    assert!(syncs.load(Ordering::SeqCst) > BATCHES);
}

/// A folder of its own under the system's temporary one, removed again on drop.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("mr-measure-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    /// Bytes in the files under it.
    fn size(&self) -> u64 {
        fn walk(dir: &std::path::Path) -> u64 {
            std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| match entry.metadata() {
                    Ok(meta) if meta.is_dir() => walk(&entry.path()),
                    Ok(meta) => meta.len(),
                    Err(_) => 0,
                })
                .sum()
        }
        walk(&self.0)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The search index of a room as the app keeps it, in an encrypted folder: the SDK
/// decrypts each of its files whole into memory when it opens it (and opens them again
/// after every commit), against an unencrypted folder of the same messages, whose files
/// are mapped and not on the heap. Every room that gets a message keeps its index open
/// for the rest of the run.
#[tokio::test]
#[ignore = "a measurement: run alone and in release, see crate::measure"]
async fn measure_search_index_of_a_room() {
    const MESSAGES: usize = 5000;
    if !crate::measure::wanted() {
        return;
    }
    for encrypted in [false, true] {
        let dir = Scratch::new(if encrypted { "encrypted" } else { "plain" });
        let kind = if encrypted {
            SearchIndexStoreKind::EncryptedDirectory(dir.0.clone(), "measure".to_owned())
        } else {
            SearchIndexStoreKind::UnencryptedDirectory(dir.0.clone())
        };
        let fixture = fixture_with(MESSAGES, kind).await;
        let (before, after) = fixture.heap;
        let on_disk = dir.size();
        println!(
            "[search] {}: client after syncing and indexing {MESSAGES} messages: {:+.1} MB \
             ({:.0} B/message); the index on disk: {:.2} MB ({:.0} B/message)",
            if encrypted {
                "encrypted folder (as the app has it)"
            } else {
                "unencrypted folder"
            },
            mb(grew(before, after) as f64),
            per(grew(before, after), MESSAGES),
            mb(on_disk as f64),
            on_disk as f64 / MESSAGES as f64
        );
        // Searches of the room, with the index open already.
        let h0 = heap_in_use();
        let found = time_async(async || {
            fixture
                .room
                .search("release", 20, None)
                .await
                .unwrap()
                .len()
        })
        .await;
        let h1 = heap_in_use();
        println!(
            "[search] searching the room for a common word: {found}; heap after: {:+.2} MB",
            mb(grew(h0, h1) as f64)
        );
        drop(fixture);
    }
}

/// Not a measurement: the list of messages that the interface keeps is made with room
/// for the timeline's items and no more. Collected from a `filter_map`, it grew by
/// doubling, and up to as many messages again (784 bytes each) were kept unused for
/// every room shown: for 300 messages, room for 512.
#[tokio::test]
async fn messages_are_made_without_room_to_spare() {
    let fixture = fixture(300).await;
    let timeline = super::room_timeline(&fixture.room).await.unwrap();
    let items = show_all(&timeline).await;
    let own = fixture.room.own_user_id().to_owned();
    let messages = super::super::convert::convert(&items, &own, &HashSet::new());
    assert!(messages.len() >= 300, "{}", messages.len());
    assert!(
        messages.capacity() <= items.len(),
        "room for {} messages, {} items",
        messages.capacity(),
        items.len()
    );
}

/// The sample room is what it says: as many lines as messages, with the mix's parts.
#[tokio::test]
#[ignore = "a measurement: run alone and in release, see crate::measure"]
async fn measure_sample_room_has_the_mix() {
    if !crate::measure::wanted() {
        return;
    }
    let messages = sample_messages(400).await;
    // One message per event (the read marker comes on top).
    assert_eq!(messages.iter().filter(|m| m.item.is_some()).count(), 400);
    let count = |f: &dyn Fn(&Message) -> bool| messages.iter().filter(|m| f(m)).count();
    assert!(count(&|m: &Message| m.html.is_some()) > 0);
    assert!(count(&|m: &Message| m.reply_to.is_some()) > 0);
    assert!(count(&|m: &Message| !m.reactions.is_empty()) > 0);
    assert!(count(&|m: &Message| m.image.is_some()) > 0);
    assert!(count(&|m: &Message| m.file.is_some()) > 0);
    assert!(count(&|m: &Message| !m.read_by.is_empty()) > 0);
    assert!(count(&|m: &Message| m.own) > 0);
    // Replies name what they answer: the original is in the same sync.
    assert!(
        messages
            .iter()
            .filter_map(|m| m.reply_to.as_ref())
            .all(|(sender, _)| !sender.is_empty())
    );
}

/// A stand-in server whose first sync is the sample room of `messages` messages; each
/// later sync brings the next of `bursts` bursts of `per_burst` new messages (with the
/// reactions on them) at once, as a busy room does, and after them nothing. With the ids
/// of each burst's messages.
async fn server_with_bursts(
    messages: usize,
    bursts: usize,
    per_burst: usize,
) -> (fake::Server, Vec<Vec<String>>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let people = people(&mut Lcg(0x5eed));
    let start = 1_800_000_000_000;
    let (timelines, ids): (Vec<Vec<Value>>, Vec<Vec<String>>) = (0..bursts)
        .map(|n| {
            let at = start + n as u64 * 1_000_000;
            sample_timeline(&mut Lcg(0xb0057 + n as u64), &people, at, 1000, per_burst)
        })
        .unzip();
    let syncs = AtomicUsize::new(0);
    let server = fake::Server::answering(
        json!({ "join": { ROOM: sample_room(messages) } }),
        Box::new(move |request| {
            if !request.path.ends_with("/sync") {
                return None;
            }
            let n = syncs.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                return None;
            }
            let rooms = match timelines.get(n - 1) {
                Some(events) => json!({ "join": { ROOM: {
                    "timeline": { "events": events, "limited": false },
                } } }),
                None => json!({}),
            };
            let sync = json!({ "next_batch": format!("s{}", n + 1), "rooms": rooms });
            Some(fake::Reply::Json(sync.to_string()))
        }),
    )
    .await;
    (server, ids)
}

/// The sample room opened as the app opens it: `Timelines` forwarding its live
/// timeline, and what it says to the interface.
struct Opened {
    timelines: super::Timelines,
    events: tokio::sync::mpsc::UnboundedReceiver<crate::matrix::MatrixEvent>,
    // Kept: `forward` ends when they are gone.
    _ignored: tokio::sync::watch::Sender<super::IgnoredSet>,
    _own_refresh: tokio::sync::watch::Sender<u64>,
}

impl Opened {
    async fn new(client: Client) -> Self {
        let (events, received) = tokio::sync::mpsc::unbounded_channel();
        let (_ignored, ignored) = tokio::sync::watch::channel(super::IgnoredSet::default());
        let (_own_refresh, own_refresh) = tokio::sync::watch::channel(0);
        let mut timelines = super::Timelines::new(
            client,
            events,
            ignored,
            own_refresh,
            super::super::people::OwnRoomNames::default(),
            super::super::left::LeftRooms::default(),
        );
        timelines
            .handle(crate::matrix::MatrixCmd::OpenRoom {
                room_id: ROOM.to_owned(),
                generation: 1,
            })
            .await;
        let mut opened = Self {
            timelines,
            events: received,
            _ignored,
            _own_refresh,
        };
        opened.next_timeline().await;
        opened
    }

    /// The next whole timeline the interface is sent.
    async fn next_timeline(&mut self) -> Vec<Message> {
        loop {
            if let Some(crate::matrix::MatrixEvent::Timeline { messages, .. }) =
                self.next_event().await
            {
                return messages;
            }
        }
    }

    async fn next_event(&mut self) -> Option<crate::matrix::MatrixEvent> {
        tokio::time::timeout(Duration::from_secs(60), self.events.recv())
            .await
            .expect("the worker says something within a minute")
    }

    /// Snapshots until one holds all of `wanted`; with the count.
    async fn until_all(&mut self, wanted: &[String]) -> (usize, Vec<Message>) {
        let mut snapshots = 0;
        loop {
            let messages = self.next_timeline().await;
            snapshots += 1;
            let shown: HashSet<&str> = messages.iter().filter_map(event_id).collect();
            if wanted.iter().all(|id| shown.contains(id.as_str())) {
                return (snapshots, messages);
            }
        }
    }
}

fn event_id(message: &Message) -> Option<&str> {
    match message.item.as_ref()? {
        crate::model::ItemRef::Event(id) => Some(id.as_str()),
        crate::model::ItemRef::Transaction(_) => None,
    }
}

/// Not a measurement: a burst of new messages in the open room (one sync with many)
/// reaches the interface in fewer snapshots than messages, and the last one has them
/// all, newest last. (`forward` converts the whole timeline for every snapshot.) It
/// pins the outcome, not how it comes about: one sync is likely one diff batch anyway.
/// That `forward` needs no batching of its own rests on matrix-sdk-ui 0.19.1's subscriber
/// merging every diff waiting at each poll; `measure_snapshots_of_bursts` shows it (126
/// pages back over 5000 messages: 2 to 4 snapshots).
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_messages_reaches_the_interface_in_fewer_snapshots() {
    const BURST: usize = 60;
    let (server, bursts) = server_with_bursts(40, 1, BURST).await;
    let (client, _room) = client_of(&server, SearchIndexStoreKind::InMemory).await;
    let mut opened = Opened::new(client.clone()).await;
    client.sync_once(SyncSettings::default()).await.unwrap();
    let (snapshots, messages) = opened.until_all(&bursts[0]).await;
    assert!(
        snapshots < BURST,
        "{snapshots} snapshots for {BURST} messages"
    );
    let last = messages.iter().rev().find_map(event_id);
    assert_eq!(last, bursts[0].last().map(String::as_str));
    drop(opened);
    drop(server);
}

/// How many snapshots `forward` sends the interface (each a conversion of the whole
/// timeline: 9 ms at 5000 messages) for news that comes in bursts: syncs right after
/// one another, each with new messages, and paging back over a room of 5000 messages a
/// page at a time, as scrolling to the top does.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement: run alone and in release, see crate::measure"]
async fn measure_snapshots_of_bursts() {
    const MESSAGES: usize = 5000;
    const BURSTS: usize = 20;
    const PER_BURST: usize = 25;
    if !crate::measure::wanted() {
        return;
    }
    let (server, bursts) = server_with_bursts(MESSAGES, BURSTS, PER_BURST).await;
    let (client, _room) = client_of(&server, SearchIndexStoreKind::InMemory).await;
    let mut opened = Opened::new(client.clone()).await;

    // Paging back to the start, one request after the other, as the app asks.
    let started = Instant::now();
    let (mut pages, mut snapshots) = (0, 0);
    loop {
        opened
            .timelines
            .handle(crate::matrix::MatrixCmd::Paginate {
                room_id: ROOM.to_owned(),
                generation: 1,
            })
            .await;
        pages += 1;
        let reached_start = loop {
            match opened.next_event().await {
                Some(crate::matrix::MatrixEvent::Timeline { .. }) => snapshots += 1,
                Some(crate::matrix::MatrixEvent::Paginated { reached_start, .. }) => {
                    break reached_start;
                }
                _ => {}
            }
        };
        if reached_start {
            break;
        }
    }
    // What is still on its way after the last page.
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(500), opened.events.recv()).await
    {
        if matches!(event, crate::matrix::MatrixEvent::Timeline { .. }) {
            snapshots += 1;
        }
    }
    println!(
        "[bursts] paging back over {MESSAGES} messages: {pages} pages, {snapshots} snapshots, \
         {:.2} s",
        started.elapsed().as_secs_f64()
    );

    // Syncs one right after the other, each with new messages.
    let started = Instant::now();
    for _ in 0..BURSTS {
        client.sync_once(SyncSettings::default()).await.unwrap();
    }
    let (snapshots, _) = opened.until_all(&bursts[BURSTS - 1]).await;
    println!(
        "[bursts] {BURSTS} syncs of {PER_BURST} messages each, one right after the other: \
         {snapshots} snapshots until the last message was shown, {:.2} s",
        started.elapsed().as_secs_f64()
    );
    drop(opened);
    drop(server);
}

/// Not a measurement: a room's timeline that was not used for `IDLE_TIMELINE` is dropped
/// from the cache unless it is shown, however long that is; the timer of the session
/// loop goes off for it; and the room opened again shows its messages, paged back from
/// the store without asking the server.
#[tokio::test(flavor = "multi_thread")]
async fn a_timeline_idle_for_long_is_dropped_and_opens_again_from_the_store() {
    use super::IDLE_TIMELINE;
    use crate::matrix::{MatrixCmd, MatrixEvent};
    const OTHER: &str = "!other:localhost";
    const MESSAGES: usize = 300;
    let minute = Duration::from_secs(60);
    let server = fake::Server::start(json!({ "join": {
        ROOM: sample_room(MESSAGES),
        OTHER: fake::joined(false, vec![]),
    } }))
    .await;
    let (client, _room) = client_of(&server, SearchIndexStoreKind::InMemory).await;
    let mut opened = Opened::new(client).await;
    let cached = |opened: &Opened| -> Vec<String> {
        let cache = &opened.timelines.cache;
        cache.iter().map(|cached| cached.key.clone()).collect()
    };
    // The sample room is left for another: from now on it is idle.
    opened
        .timelines
        .handle(MatrixCmd::OpenRoom {
            room_id: OTHER.to_owned(),
            generation: 2,
        })
        .await;
    let left = Instant::now();
    assert_eq!(cached(&opened), [ROOM, OTHER]);
    let next = opened.timelines.next_idle().unwrap();
    assert!(next <= left + IDLE_TIMELINE && next >= left + IDLE_TIMELINE - minute);

    // Not long enough: kept.
    opened.timelines.drop_idle(left + IDLE_TIMELINE - minute);
    assert_eq!(cached(&opened), [ROOM, OTHER]);
    // Long enough: dropped. The one shown stays, however long it has been.
    opened.timelines.drop_idle(left + IDLE_TIMELINE + minute);
    assert_eq!(cached(&opened), [OTHER]);
    opened.timelines.drop_idle(left + 100 * IDLE_TIMELINE);
    assert_eq!(cached(&opened), [OTHER]);
    assert_eq!(opened.timelines.next_idle(), None);

    // Opened again, it starts from its latest messages and pages back to the start
    // from the store.
    opened
        .timelines
        .handle(MatrixCmd::OpenRoom {
            room_id: ROOM.to_owned(),
            generation: 3,
        })
        .await;
    let mut shown = 0;
    let mut reached_start = false;
    for _ in 0..100 {
        opened
            .timelines
            .handle(MatrixCmd::Paginate {
                room_id: ROOM.to_owned(),
                generation: 3,
            })
            .await;
        loop {
            match opened.next_event().await {
                Some(MatrixEvent::Timeline {
                    generation: 3,
                    messages,
                    ..
                }) => shown = messages.iter().filter(|m| m.item.is_some()).count(),
                Some(MatrixEvent::Paginated {
                    generation: 3,
                    reached_start: start,
                }) => {
                    reached_start = start;
                    break;
                }
                _ => {}
            }
        }
        if reached_start {
            break;
        }
    }
    // The page that reached the start may be shown right after it said so.
    while shown < MESSAGES {
        if let Some(MatrixEvent::Timeline {
            generation: 3,
            messages,
            ..
        }) = opened.next_event().await
        {
            shown = messages.iter().filter(|m| m.item.is_some()).count();
        }
    }
    assert!(reached_start);
    assert_eq!(shown, MESSAGES);
    assert_eq!(cached(&opened), [OTHER, ROOM]);
    let asked: Vec<String> = server
        .requests()
        .into_iter()
        .map(|request| request.path)
        .filter(|path| path.ends_with("/messages"))
        .collect();
    assert!(asked.is_empty(), "{asked:?}");

    // The session loop's timer goes off once a timeline not shown has been idle long
    // enough. (Made so by putting its last use back, where the clock allows it.)
    if let Some(long_ago) = Instant::now().checked_sub(IDLE_TIMELINE + minute) {
        // Shown: its last use does not count.
        opened.timelines.cache[1].last_used = long_ago;
        let other = opened.timelines.cache[0].last_used;
        assert_eq!(opened.timelines.next_idle(), Some(other + IDLE_TIMELINE));
        opened
            .timelines
            .handle(MatrixCmd::OpenRoom {
                room_id: OTHER.to_owned(),
                generation: 4,
            })
            .await;
        // Leaving it was a use: it is idle from now on, not from long ago.
        assert!(opened.timelines.next_idle() > Some(long_ago + IDLE_TIMELINE));
        let at = opened
            .timelines
            .cache
            .iter()
            .position(|cached| cached.key == ROOM)
            .unwrap();
        opened.timelines.cache[at].last_used = long_ago;
        tokio::time::timeout(Duration::from_secs(10), opened.timelines.idle_due())
            .await
            .expect("the timer goes off");
        opened.timelines.drop_idle(Instant::now());
        assert_eq!(cached(&opened), [OTHER]);
    }
    drop(opened);
    drop(server);
}

/// Not a measurement: a room whose start was loaded gets a sync that says it missed
/// messages (limited, with a gap). The SDK then keeps only the latest messages, and
/// the room's start is no longer loaded: the interface must be told, or it never pages
/// back again (it asks for older messages only while the start is not reached).
#[tokio::test(flavor = "multi_thread")]
async fn a_limited_sync_after_the_start_was_loaded_says_the_timeline_was_reset() {
    use crate::matrix::{MatrixCmd, MatrixEvent};
    use std::sync::atomic::{AtomicUsize, Ordering};
    const MESSAGES: usize = 30;
    let people = people(&mut Lcg(0x5eed));
    let (missed, new_ids) = sample_timeline(&mut Lcg(0x9a9), &people, 1_800_000_000_000, 1000, 5);
    let syncs = AtomicUsize::new(0);
    let server = fake::Server::answering(
        json!({ "join": { ROOM: sample_room(MESSAGES) } }),
        Box::new(move |request| {
            if !request.path.ends_with("/sync") || syncs.fetch_add(1, Ordering::SeqCst) == 0 {
                return None;
            }
            let rooms = json!({ "join": { ROOM: {
                "timeline": { "events": missed, "limited": true, "prev_batch": "gap" },
            } } });
            let sync = json!({ "next_batch": "s2", "rooms": rooms });
            Some(fake::Reply::Json(sync.to_string()))
        }),
    )
    .await;
    let (client, _room) = client_of(&server, SearchIndexStoreKind::InMemory).await;
    let mut opened = Opened::new(client.clone()).await;

    // Paged back to the start, which the interface is told.
    let mut at_start = false;
    for _ in 0..20 {
        opened
            .timelines
            .handle(MatrixCmd::Paginate {
                room_id: ROOM.to_owned(),
                generation: 1,
            })
            .await;
        loop {
            match opened.next_event().await {
                Some(MatrixEvent::Timeline { reached_start, .. }) => at_start |= reached_start,
                Some(MatrixEvent::Paginated { reached_start, .. }) => {
                    at_start |= reached_start;
                    break;
                }
                _ => {}
            }
        }
        if at_start {
            break;
        }
    }
    assert!(at_start);

    // What was on its way from paging back.
    while let Ok(Some(_)) =
        tokio::time::timeout(Duration::from_millis(300), opened.events.recv()).await
    {}

    // The limited sync: the SDK keeps only the messages after the gap, and the
    // interface hears of it before the timeline that has them, which is not empty and
    // not at the start (so it cannot tell that the start was lost).
    client.sync_once(SyncSettings::default()).await.unwrap();
    let mut reset = false;
    loop {
        match opened.next_event().await {
            Some(MatrixEvent::TimelineReset {
                room_id,
                generation,
            }) => {
                assert_eq!((room_id.as_str(), generation), (ROOM, 1));
                reset = true;
            }
            Some(MatrixEvent::Timeline {
                messages,
                reached_start,
                ..
            }) => {
                let shown: HashSet<&str> = messages.iter().filter_map(event_id).collect();
                if new_ids.iter().all(|id| shown.contains(id.as_str())) {
                    assert!(reset, "the timeline came without a reset before it");
                    assert!(!reached_start);
                    assert_eq!(shown.len(), new_ids.len());
                    break;
                }
            }
            Some(_) => {}
            None => panic!("the worker stopped"),
        }
    }
    drop(opened);
    drop(server);
}

/// What dropping an idle timeline gives back: the sample room of 5000 messages opened as
/// the app opens it and scrolled to the top, then left for another room. Before
/// `IDLE_TIMELINE` its timeline stayed cached for as long as it was among the last six
/// rooms used; now it goes once it has been idle that long.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement: run alone and in release, see crate::measure"]
async fn measure_idle_timeline_given_back() {
    use crate::matrix::MatrixCmd;
    const MESSAGES: usize = 5000;
    const OTHER: &str = "!other:localhost";
    if !crate::measure::wanted() {
        return;
    }
    let server = fake::Server::start(json!({ "join": {
        ROOM: sample_room(MESSAGES),
        OTHER: fake::joined(false, vec![]),
    } }))
    .await;
    let (client, room) = client_of(&server, SearchIndexStoreKind::InMemory).await;
    wait_indexed(&room).await;
    let mut opened = Opened::new(client).await;
    // Scrolled to the top: the forwarder sees every item.
    let timeline = opened.timelines.cache[0].timeline.clone();
    show_all(&timeline).await;
    drop(timeline);
    opened
        .timelines
        .handle(MatrixCmd::OpenRoom {
            room_id: OTHER.to_owned(),
            generation: 2,
        })
        .await;
    // What was sent to the interface is not counted.
    async fn settled(opened: &mut Opened) -> usize {
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            while opened.events.try_recv().is_ok() {}
        }
        heap_in_use()
    }
    let in_memory = async || {
        let (cache, _drop_handles) = room.event_cache().await.unwrap();
        cache.events().await.unwrap().len()
    };
    let h0 = settled(&mut opened).await;
    let events_before = in_memory().await;
    let left = Instant::now();
    opened
        .timelines
        .drop_idle(left + super::IDLE_TIMELINE + Duration::from_secs(60));
    assert_eq!(opened.timelines.cache.len(), 1);
    let h1 = settled(&mut opened).await;
    let events_after = in_memory().await;
    println!(
        "[idle] the timeline of a room of {MESSAGES} messages, scrolled to the top and left, \
         dropped after {} minutes idle: {:+.1} MB (SDK events in memory {events_before} -> \
         {events_after}); before, it stayed cached",
        super::IDLE_TIMELINE.as_secs() / 60,
        mb(grew(h0, h1) as f64)
    );
    // Opened again: from its latest messages, as after a restart.
    let started = Instant::now();
    opened
        .timelines
        .handle(MatrixCmd::OpenRoom {
            room_id: ROOM.to_owned(),
            generation: 3,
        })
        .await;
    let first = loop {
        if let Some(crate::matrix::MatrixEvent::Timeline {
            generation: 3,
            messages,
            ..
        }) = opened.next_event().await
        {
            break messages;
        }
    };
    println!(
        "[idle] opened again: first snapshot after {:.1} ms with {} messages",
        started.elapsed().as_secs_f64() * 1000.0,
        first.iter().filter(|m| m.item.is_some()).count()
    );
    drop(opened);
    drop(server);
}
