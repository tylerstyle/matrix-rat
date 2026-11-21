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
