//! Not measurements: the messages [`Reuse`] gives for a timeline that changes are the
//! ones made afresh from every item, after every change the SDK sends, while the room
//! changes in all the ways a message can; and only the messages of the items that
//! changed are made again.
//!
//! The room is the sample room of `measure` on its stand-in server, opened as `forward`
//! opens it; each step brings its change with the next sync (or asks the SDK for it), and
//! every batch of diffs the timeline sends is checked.

use std::collections::{HashSet, VecDeque};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local};
use futures::{Stream, StreamExt};
use matrix_sdk::config::SyncSettings;
use matrix_sdk::ruma::events::AnyMessageLikeEventContent;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::{EventId, OwnedUserId, RoomId, UserId, device_id};
use matrix_sdk::search_index::SearchIndexStoreKind;
use matrix_sdk_base::crypto::{EncryptionSettings, OlmMachine, encrypt_room_key_export};
use matrix_sdk_ui::eyeball_im::{Vector, VectorDiff};
use matrix_sdk_ui::timeline::{
    EventSendState, EventTimelineItem, Timeline, TimelineDetails, TimelineItem, VirtualTimelineItem,
};
use serde_json::{Value, json};

use super::measure::{ROOM, client_of, sample_room, show_all};
use crate::matrix::convert::{Reuse, convert_at};
use crate::matrix::spaces::fake;
use crate::model::{
    Attachment, AudioMeta, ImageInfo, ImageSource, ItemRef, MediaRef, Message, PollAnswer,
    PollInfo, Reaction, ReadBy, Reader, ThreadSummary,
};

/// Messages in the sample room.
const MESSAGES: usize = 40;
/// The message whose original is not in the timeline, and that original (which the
/// server gives out when asked).
const REPLY: &str = "$reply-to-gone";
const GONE: &str = "$gone";
/// The encrypted message, and what it says.
const SECRET: &str = "$secret";
const SECRET_TEXT: &str = "decrypted at last";
/// The event id the server gives the first message sent; the second is refused.
const SENT: &str = "$sent-by-us";
/// Who sends the encrypted message.
const SENDER: &str = "@crypto:localhost";

type Changes = Pin<Box<dyn Stream<Item = Vec<VectorDiff<Arc<TimelineItem>>>>>>;

/// The sample room, opened as `forward` opens it, with what it takes to change it.
struct Stage {
    server: fake::Server,
    /// The room's part of each sync to come, in order.
    syncs: Arc<Mutex<VecDeque<Value>>>,
    client: matrix_sdk::Client,
    room: matrix_sdk::Room,
    timeline: Timeline,
    items: Vector<Arc<TimelineItem>>,
    changes: Changes,
    /// The room keys of the encrypted message, exported.
    keys: String,
    reuse: Reuse,
    ignored: HashSet<OwnedUserId>,
    now: DateTime<Local>,
    /// The items at the last check, kept so that their addresses stay theirs, and
    /// whether everything is to be made again (what messages are made with changed).
    checked: Vector<Arc<TimelineItem>>,
    all_again: bool,
    /// The messages of the last check, made afresh.
    messages: Vec<Message>,
    /// The ids of the items whose message was made again since the step began.
    made: HashSet<String>,
    /// Over all checks: messages made and used again.
    totals: (usize, usize),
    checks: usize,
    /// Timestamps of what the steps bring: after everything in the sample room.
    clock: u64,
}

impl Stage {
    /// The sample room with `MESSAGES` messages, a reply to a message that is not
    /// loaded and a message that cannot be decrypted, opened with all of it shown
    /// (`all`) or the last items only, as the SDK opens it.
    async fn new(all: bool) -> Self {
        let room_id = <&RoomId>::try_from(ROOM).unwrap();
        // The encrypted message: a session of the sender's, whose key comes later.
        let sender = <&UserId>::try_from(SENDER).unwrap();
        let machine = OlmMachine::new(sender, device_id!("CRYPTO")).await;
        machine
            .share_room_key(room_id, std::iter::empty(), EncryptionSettings::default())
            .await
            .unwrap();
        let plain = json!({ "msgtype": "m.text", "body": SECRET_TEXT });
        let plain = Raw::<AnyMessageLikeEventContent>::from_json_string(plain.to_string()).unwrap();
        let encrypted = machine
            .encrypt_room_event_raw(room_id, "m.room.message", &plain)
            .await
            .unwrap();
        let encrypted: Value = serde_json::from_str(encrypted.content.json().get()).unwrap();
        let exported = machine.store().export_room_keys(|_| true).await.unwrap();
        let keys = encrypt_room_key_export(&exported, "passphrase", 1).unwrap();

        let mut room = sample_room(MESSAGES);
        let first = room["timeline"]["events"][0].clone();
        let events = room["timeline"]["events"].as_array_mut().unwrap();
        events.push(json!({
            "type": "m.room.encrypted", "event_id": SECRET, "sender": SENDER,
            "origin_server_ts": 1_795_000_000_000u64, "content": encrypted,
        }));
        events.push(json!({
            "type": "m.room.message", "event_id": REPLY, "sender": first["sender"],
            "origin_server_ts": 1_795_000_000_001u64,
            "content": {
                "msgtype": "m.text", "body": "> what was said\n\nand the answer",
                "m.relates_to": { "m.in_reply_to": { "event_id": GONE } },
            },
        }));
        let gone = json!({
            "type": "m.room.message", "event_id": GONE, "sender": first["sender"],
            "room_id": ROOM, "origin_server_ts": 1_000u64,
            "content": { "msgtype": "m.text", "body": "what was said, long ago" },
        });

        let syncs = Arc::new(Mutex::new(VecDeque::<Value>::new()));
        let queued = syncs.clone();
        let synced = AtomicUsize::new(0);
        let sent = AtomicUsize::new(0);
        let server = fake::Server::answering(
            json!({ "join": { ROOM: room } }),
            Box::new(move |request| {
                let path = request.path.as_str();
                if path.ends_with("/sync") {
                    let n = synced.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        return None;
                    }
                    let rooms = match queued.lock().unwrap().pop_front() {
                        Some(room) => json!({ "join": { ROOM: room } }),
                        None => json!({}),
                    };
                    let sync = json!({ "next_batch": format!("s{}", n + 1), "rooms": rooms });
                    return Some(fake::Reply::Json(sync.to_string()));
                }
                // The room is not encrypted: sending asks before it sends.
                if request.method == "GET" && path.contains("/state/m.room.encryption") {
                    return Some(fake::Reply::Status(
                        404,
                        json!({ "errcode": "M_NOT_FOUND", "error": "not found" }).to_string(),
                    ));
                }
                if path.ends_with(&format!("/event/{GONE}")) {
                    return Some(fake::Reply::Json(gone.to_string()));
                }
                if request.method == "PUT" && path.contains("/send/") {
                    return Some(match sent.fetch_add(1, Ordering::SeqCst) {
                        0 => fake::Reply::Json(json!({ "event_id": SENT }).to_string()),
                        _ => fake::Reply::Status(
                            403,
                            json!({ "errcode": "M_FORBIDDEN", "error": "no" }).to_string(),
                        ),
                    });
                }
                None
            }),
        )
        .await;
        let (client, room) = client_of(&server, SearchIndexStoreKind::InMemory).await;
        let timeline = super::room_timeline(&room).await.unwrap();
        if all {
            show_all(&timeline).await;
        }
        let (items, changes) = timeline.subscribe().await;
        let mut stage = Self {
            server,
            syncs,
            client,
            room,
            timeline,
            items,
            changes: Box::pin(changes),
            keys,
            reuse: Reuse::default(),
            ignored: HashSet::new(),
            now: Local::now(),
            checked: Vector::new(),
            all_again: true,
            messages: Vec::new(),
            made: HashSet::new(),
            totals: (0, 0),
            checks: 0,
            clock: 1_796_000_000_000,
        };
        stage.check("opened");
        stage
    }

    /// Makes the messages of the items as they are now, both ways, and compares them;
    /// and checks that exactly the event items not there at the last check (by address)
    /// had their message made again.
    fn check(&mut self, step: &str) {
        let own = self.room.own_user_id().to_owned();
        let reused = self
            .reuse
            .convert_at(&self.items, &own, &self.ignored, &self.now);
        let fresh = convert_at(&self.items, &own, &self.ignored, &self.now);
        let reused_whole: Vec<String> = reused.iter().map(whole).collect();
        let fresh_whole: Vec<String> = fresh.iter().map(whole).collect();
        assert_eq!(
            reused_whole, fresh_whole,
            "check {} ({step}): used again is not made afresh",
            self.checks
        );
        let mut known: HashSet<usize> = if self.all_again {
            HashSet::new()
        } else {
            self.checked.iter().map(address).collect()
        };
        let (mut made, mut used) = (0, 0);
        for item in self.items.iter().filter(|item| item.as_event().is_some()) {
            if known.insert(address(item)) {
                made += 1;
                self.made.insert(item.unique_id().0.clone());
            } else {
                used += 1;
            }
        }
        assert_eq!(
            self.reuse.counts,
            (made, used),
            "check {} ({step}): made again and used again",
            self.checks
        );
        self.totals.0 += made;
        self.totals.1 += used;
        self.checks += 1;
        self.checked = self.items.clone();
        self.all_again = false;
        self.messages = fresh;
    }

    /// Takes the timeline's changes in, checking after every batch, until `done`.
    async fn until(&mut self, step: &str, done: impl Fn(&Vector<Arc<TimelineItem>>) -> bool) {
        while !done(&self.items) {
            let diffs = tokio::time::timeout(Duration::from_secs(20), self.changes.next())
                .await
                .unwrap_or_else(|_| panic!("{step}: no change within 20 s"))
                .expect("the timeline goes on");
            for diff in diffs {
                diff.apply(&mut self.items);
            }
            self.check(step);
        }
    }

    /// The next sync brings `room` (its `timeline`, `ephemeral` and the like).
    async fn sync(&mut self, room: Value) {
        self.syncs.lock().unwrap().push_back(room);
        self.client
            .sync_once(SyncSettings::default())
            .await
            .unwrap();
    }

    /// An event of `kind` by `sender`, after everything before it.
    fn event(&mut self, kind: &str, id: &str, sender: &str, content: Value) -> Value {
        self.clock += 1000;
        json!({
            "type": kind, "event_id": id, "sender": sender,
            "origin_server_ts": self.clock, "content": content,
        })
    }

    /// The message made of the event `id` at the last check.
    fn message(&self, id: &str) -> Option<&Message> {
        self.messages
            .iter()
            .find(|message| message.item == Some(ItemRef::Event(id.to_owned())))
    }

    /// The `n`th plain text message of the sample room by someone else, from the end,
    /// and who sent it.
    fn text_message(&self, n: usize) -> (String, String) {
        let own = self.room.own_user_id();
        self.items
            .iter()
            .rev()
            .filter_map(|item| item.as_event())
            .filter(|event| {
                event.sender() != own
                    && event.event_id().is_some_and(|id| id.as_str().len() == 44)
                    && event
                        .content()
                        .as_message()
                        .is_some_and(|message| message.body().len() > 3 && !message.is_edited())
                    && event.content().reactions().is_none_or(|r| r.is_empty())
            })
            .nth(n)
            .map(|event| {
                (
                    event.event_id().unwrap().to_string(),
                    event.sender().to_string(),
                )
            })
            .expect("the sample room has text messages")
    }

    /// Starts a step: what is made again is counted from here.
    fn begin(&mut self) {
        self.made.clear();
    }

    /// Asserts that the message of the event `id` was made again in this step.
    fn assert_made_again(&self, id: &str) {
        let unique = self
            .items
            .iter()
            .find(|item| {
                item.as_event()
                    .and_then(|event| event.event_id())
                    .is_some_and(|event_id| event_id.as_str() == id)
            })
            .map(|item| item.unique_id().0.clone())
            .expect("the event is in the timeline");
        assert!(self.made.contains(&unique), "{id} was not made again");
        // Not everything: the others were used again.
        assert!(
            self.made.len() * 2 < self.events(),
            "{} of {} made again",
            self.made.len(),
            self.events()
        );
    }

    fn events(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.as_event().is_some())
            .count()
    }

    /// Older messages, paged in at the front a few at a time, from what is loaded.
    async fn page_back(&mut self) {
        self.begin();
        let all = self.timeline.items().await.len();
        while self.items.len() < all {
            let before = self.items.len();
            self.timeline.paginate_backwards(10).await.unwrap();
            self.until("paged back", |items| items.len() > before).await;
        }
    }

    /// New messages at the end, their ids starting with `prefix`.
    async fn new_messages(&mut self, prefix: &str) -> Vec<String> {
        self.begin();
        let (_, someone) = self.text_message(0);
        let ids: Vec<String> = (0..3).map(|n| format!("{prefix}{n}")).collect();
        let events: Vec<Value> = ids
            .iter()
            .map(|id| {
                let content = json!({ "msgtype": "m.text", "body": format!("news {id}") });
                self.event("m.room.message", id, &someone, content)
            })
            .collect();
        self.sync(json!({ "timeline": { "events": events } })).await;
        let last = ids[2].clone();
        self.until("new messages", |items| event(items, &last).is_some())
            .await;
        ids
    }

    /// An edit of someone's message by them; with its id.
    async fn edit(&mut self) -> String {
        self.begin();
        let (target, sender) = self.text_message(1);
        let content = json!({
            "msgtype": "m.text", "body": "* said otherwise",
            "m.new_content": { "msgtype": "m.text", "body": "said otherwise" },
            "m.relates_to": { "rel_type": "m.replace", "event_id": target },
        });
        let edit = self.event("m.room.message", "$edit", &sender, content);
        self.sync(json!({ "timeline": { "events": [edit] } })).await;
        self.until("edit", |items| {
            event(items, &target)
                .and_then(|event| event.content().as_message())
                .is_some_and(|message| message.is_edited())
        })
        .await;
        self.assert_made_again(&target);
        target
    }

    /// A reaction by someone and one by us to a message; with its id.
    async fn react(&mut self) -> String {
        self.begin();
        let (target, sender) = self.text_message(2);
        let own = self.room.own_user_id().to_string();
        let content = json!({ "m.relates_to": {
            "rel_type": "m.annotation", "event_id": target, "key": "🦀",
        } });
        let theirs = self.event("m.reaction", "$crab1", &sender, content.clone());
        let ours = self.event("m.reaction", "$crab2", &own, content);
        self.sync(json!({ "timeline": { "events": [theirs, ours] } }))
            .await;
        self.until("reactions", |items| {
            event(items, &target)
                .and_then(|event| event.content().reactions())
                .and_then(|reactions| reactions.get("🦀"))
                .map(|by| by.len())
                == Some(2)
        })
        .await;
        self.assert_made_again(&target);
        target
    }

    /// A read receipt of someone's on a message; with its id.
    async fn receipt(&mut self) -> String {
        self.begin();
        let (target, _) = self.text_message(3);
        // Someone without a receipt yet: one never goes back to an earlier message.
        let reader = "@reader:localhost";
        let mut read = serde_json::Map::new();
        read.insert(reader.to_owned(), json!({ "ts": self.clock }));
        let mut content = serde_json::Map::new();
        content.insert(target.clone(), json!({ "m.read": read }));
        let receipt = json!({ "type": "m.receipt", "content": content });
        self.sync(json!({ "ephemeral": { "events": [receipt] } }))
            .await;
        self.until("receipt", |items| {
            event(items, &target).is_some_and(|event| {
                event
                    .read_receipts()
                    .keys()
                    .any(|user| user.as_str() == reader)
            })
        })
        .await;
        self.assert_made_again(&target);
        target
    }

    /// A message deleted by its sender; with its id.
    async fn redact(&mut self) -> String {
        self.begin();
        let (target, sender) = self.text_message(4);
        self.clock += 1000;
        let redaction = json!({
            "type": "m.room.redaction", "event_id": "$redaction", "sender": sender,
            "origin_server_ts": self.clock, "redacts": target,
            "content": { "redacts": target },
        });
        self.sync(json!({ "timeline": { "events": [redaction] } }))
            .await;
        self.until("redaction", |items| {
            event(items, &target).is_some_and(|event| event.content().is_redacted())
        })
        .await;
        self.assert_made_again(&target);
        target
    }

    /// Someone takes another name and picture; with the id of a message of theirs.
    async fn rename(&mut self) -> String {
        self.begin();
        let (target, sender) = self.text_message(5);
        self.clock += 1000;
        let member = json!({
            "type": "m.room.member", "state_key": sender, "sender": sender,
            "event_id": "$renamed", "origin_server_ts": self.clock,
            "content": {
                "membership": "join", "displayname": "Somebody Else",
                "avatar_url": "mxc://localhost/another",
            },
        });
        self.sync(json!({ "timeline": { "events": [member] } }))
            .await;
        self.until("new name", |items| {
            event(items, &target).is_some_and(|event| {
                matches!(event.sender_profile(), TimelineDetails::Ready(profile)
                    if profile.display_name.as_deref() == Some("Somebody Else"))
            })
        })
        .await;
        self.assert_made_again(&target);
        assert_eq!(
            self.message(&target).map(|m| m.sender.as_str()),
            Some("Somebody Else")
        );
        target
    }

    /// Two people join whose ids differ only in their servers, without display names
    /// (the first part of the id is the name then, to the SDK too), and say something
    /// each: their messages name them with their ids. Then one of them takes a name
    /// of their own, and the other's message names them as before.
    async fn namesakes(&mut self) {
        self.begin();
        let people = ["@namesake:localhost", "@namesake:elsewhere.example"];
        let ids = ["$from-here", "$from-there"];
        let mut events = Vec::new();
        for (n, who) in people.into_iter().enumerate() {
            self.clock += 1000;
            events.push(json!({
                "type": "m.room.member", "state_key": who, "sender": who,
                "event_id": format!("$namesake{n}"), "origin_server_ts": self.clock,
                "content": { "membership": "join" },
            }));
        }
        for (id, who) in ids.into_iter().zip(people) {
            let content = json!({ "msgtype": "m.text", "body": format!("said {id}") });
            events.push(self.event("m.room.message", id, who, content));
        }
        self.sync(json!({ "timeline": { "events": events } })).await;
        let ambiguous = |items: &Vector<Arc<TimelineItem>>, id: &str| {
            event(items, id).is_some_and(|event| {
                matches!(event.sender_profile(), TimelineDetails::Ready(profile)
                    if profile.display_name_ambiguous)
            })
        };
        let joins = ["$namesake0", "$namesake1"];
        self.until("namesakes", |items| {
            ids.iter().chain(&joins).all(|id| ambiguous(items, id))
        })
        .await;
        // Their joining is told apart too.
        let joined: Vec<&str> = joins
            .iter()
            .map(|id| self.message(id).map_or("", |m| m.body.as_str()))
            .collect();
        assert_eq!(
            joined,
            [
                "namesake (@namesake:localhost) joined",
                "namesake (@namesake:elsewhere.example) joined"
            ]
        );
        let named = |stage: &Self| -> Vec<(String, Option<String>)> {
            ids.iter()
                .map(|id| {
                    let message = stage.message(id).expect("their message");
                    (
                        message.sender_name().into_owned(),
                        message.sender_id.clone(),
                    )
                })
                .collect()
        };
        assert_eq!(
            named(self),
            [
                (
                    "namesake (@namesake:localhost)".to_owned(),
                    Some(people[0].to_owned())
                ),
                (
                    "namesake (@namesake:elsewhere.example)".to_owned(),
                    Some(people[1].to_owned())
                ),
            ]
        );

        self.clock += 1000;
        let renamed = json!({
            "type": "m.room.member", "state_key": people[1], "sender": people[1],
            "event_id": "$namesake-renamed", "origin_server_ts": self.clock,
            "content": { "membership": "join", "displayname": "Elsewhere" },
        });
        self.sync(json!({ "timeline": { "events": [renamed] } }))
            .await;
        self.until("one renamed", |items| {
            event(items, ids[0]).is_some() && !ids.iter().any(|id| ambiguous(items, id))
        })
        .await;
        assert_eq!(
            named(self),
            [
                ("namesake".to_owned(), Some(people[0].to_owned())),
                ("Elsewhere".to_owned(), Some(people[1].to_owned())),
            ]
        );

        // A third of the name is invited by us. Whom an invitation is for is said by
        // the name it gives them; the worker tells them apart from the namesakes it
        // has loaded.
        self.clock += 1000;
        let invite = json!({
            "type": "m.room.member", "state_key": "@namesake:third.example",
            "sender": self.room.own_user_id().as_str(),
            "event_id": "$namesake-invited", "origin_server_ts": self.clock,
            "content": { "membership": "invite" },
        });
        self.sync(json!({ "timeline": { "events": [invite] } }))
            .await;
        self.until("invited", |items| {
            event(items, "$namesake-invited").is_some()
        })
        .await;
        let invited = |messages: &[Message]| -> String {
            let invited = ItemRef::Event("$namesake-invited".to_owned());
            let found = messages.iter().find(|m| m.item.as_ref() == Some(&invited));
            found.map_or(String::new(), |m| m.body.clone())
        };
        let made = invited(&self.messages);
        assert!(made.starts_with("namesake was invited by "), "{made}");
        let mut loaded = self.messages.clone();
        crate::model::mark_namesakes(&mut loaded);
        let told = invited(&loaded);
        assert!(
            told.starts_with("namesake (@namesake:third.example) was invited by "),
            "{told}"
        );
    }

    /// The message replied to, which is not loaded, is fetched.
    async fn reply_details(&mut self) {
        self.begin();
        let reply = <&EventId>::try_from(REPLY).unwrap();
        self.timeline.fetch_details_for_event(reply).await.unwrap();
        self.until("reply details", |items| {
            event(items, REPLY)
                .and_then(|event| event.content().in_reply_to())
                .is_some_and(|reply| matches!(reply.event, TimelineDetails::Ready(_)))
        })
        .await;
        self.assert_made_again(REPLY);
        let quoted = self.message(REPLY).and_then(|m| m.reply_to.clone());
        assert_eq!(
            quoted.map(|(_, text)| text),
            Some("what was said, long ago".to_owned())
        );
    }

    /// A reply in a thread: its root gets a summary; with the root's id.
    async fn thread_reply(&mut self) -> String {
        self.begin();
        let (root, sender) = self.text_message(6);
        let content = json!({
            "msgtype": "m.text", "body": "in the thread",
            "m.relates_to": {
                "rel_type": "m.thread", "event_id": root, "is_falling_back": true,
                "m.in_reply_to": { "event_id": root },
            },
        });
        let reply = self.event("m.room.message", "$in-thread", &sender, content);
        self.sync(json!({ "timeline": { "events": [reply] } }))
            .await;
        self.until("thread reply", |items| {
            event(items, &root).is_some_and(|event| event.content().thread_summary().is_some())
        })
        .await;
        self.assert_made_again(&root);
        let thread = self.message(&root).and_then(|m| m.thread.as_ref());
        assert_eq!(thread.map(|t| t.replies), Some(1));
        root
    }

    /// A message of ours, sent (the server confirms it, then it comes back with a sync),
    /// and another that the server refuses.
    async fn send(&mut self) {
        self.begin();
        let content = RoomMessageEventContent::text_plain("on its way");
        self.timeline.send(content.into()).await.unwrap();
        self.until("sent", |items| {
            items.iter().any(|item| {
                item.as_event().is_some_and(|event| {
                    matches!(event.send_state(), Some(EventSendState::Sent { .. }))
                        || event.event_id().is_some_and(|id| id.as_str() == SENT)
                })
            })
        })
        .await;
        // The message as the server has it.
        let txn = self
            .server
            .requests()
            .into_iter()
            .find(|request| request.method == "PUT" && request.path.contains("/send/"))
            .and_then(|request| request.path.rsplit('/').next().map(str::to_owned))
            .expect("the message was sent");
        self.clock += 1000;
        let echo = json!({
            "type": "m.room.message", "event_id": SENT,
            "sender": self.room.own_user_id().as_str(),
            "origin_server_ts": self.clock,
            "content": { "msgtype": "m.text", "body": "on its way" },
            "unsigned": { "transaction_id": txn },
        });
        self.sync(json!({ "timeline": { "events": [echo] } })).await;
        self.until("remote echo", |items| {
            event(items, SENT).is_some_and(|event| !event.is_local_echo())
        })
        .await;
        assert_eq!(
            self.message(SENT).map(|m| m.state),
            Some(crate::model::SendState::Sent)
        );

        let content = RoomMessageEventContent::text_plain("refused");
        self.timeline.send(content.into()).await.unwrap();
        self.until("refused", |items| {
            items.iter().any(|item| {
                item.as_event().is_some_and(|event| {
                    matches!(
                        event.send_state(),
                        Some(EventSendState::SendingFailed { .. })
                    )
                })
            })
        })
        .await;
        let failed = self
            .messages
            .iter()
            .find(|m| m.state == crate::model::SendState::Failed)
            .expect("the refused message");
        assert_eq!(failed.body, "refused");
        assert!(failed.send_error.is_some());
    }

    /// The key of the encrypted message arrives (as an import).
    async fn decrypt(&mut self) {
        self.begin();
        assert_eq!(
            self.message(SECRET).map(|m| m.kind),
            Some(crate::model::MsgKind::Undecryptable)
        );
        static FILES: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "mr-reuse-keys-{}-{}",
            std::process::id(),
            FILES.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, &self.keys).unwrap();
        let imported = self
            .client
            .encryption()
            .import_room_keys(path.clone(), "passphrase")
            .await;
        let _ = std::fs::remove_file(&path);
        assert_eq!(imported.unwrap().imported_count, 1);
        self.until("decrypted", |items| {
            event(items, SECRET).is_some_and(|event| !event.content().is_unable_to_decrypt())
        })
        .await;
        self.assert_made_again(SECRET);
        assert_eq!(
            self.message(SECRET).map(|m| m.body.as_str()),
            Some(SECRET_TEXT)
        );
    }

    /// The read marker: put after a message (or after the messages of ours that follow
    /// it), then moved on to the end, where it is the last item and has no line.
    async fn read_marker(&mut self) {
        self.begin();
        let (target, _) = self.text_message(8);
        let fully_read = |id: &str| {
            json!({ "account_data": { "events": [
                { "type": "m.fully_read", "content": { "event_id": id } },
            ] } })
        };
        self.sync(fully_read(&target)).await;
        self.until("read marker", |items| marker_after(items).is_some())
            .await;
        let first = marker_after(&self.items);
        let (newer, _) = self.text_message(0);
        self.sync(fully_read(&newer)).await;
        self.until("read marker moved", |items| marker_after(items) != first)
            .await;
    }

    /// Someone is ignored, then no longer: everything is made again, each time.
    fn ignore(&mut self) {
        let (_, someone) = self.text_message(0);
        let before = self.messages.len();
        self.ignored.insert(someone.as_str().try_into().unwrap());
        self.all_again = true;
        self.check("ignored");
        assert!(self.messages.len() < before);
        assert_eq!(self.reuse.counts.1, 0, "nothing is used again");
        self.ignored.clear();
        self.all_again = true;
        self.check("no longer ignored");
        assert_eq!(self.messages.len(), before);
    }

    /// The next day: everything is made again ("Today" is "Yesterday").
    fn next_day(&mut self) {
        self.now += chrono::Duration::days(1);
        self.all_again = true;
        self.check("next day");
        assert_eq!(self.reuse.counts.1, 0, "nothing is used again");
    }
}

/// The event `id` among `items`.
fn event<'a>(items: &'a Vector<Arc<TimelineItem>>, id: &str) -> Option<&'a EventTimelineItem> {
    items
        .iter()
        .filter_map(|item| item.as_event())
        .find(|event| {
            event
                .event_id()
                .is_some_and(|event_id| event_id.as_str() == id)
        })
}

/// The event the read marker stands after.
fn marker_after(items: &Vector<Arc<TimelineItem>>) -> Option<String> {
    let at = items
        .iter()
        .position(|item| matches!(item.as_virtual(), Some(VirtualTimelineItem::ReadMarker)))?;
    items
        .iter()
        .take(at)
        .rev()
        .find_map(|item| item.as_event()?.event_id().map(|id| id.to_string()))
}

fn address(item: &Arc<TimelineItem>) -> usize {
    Arc::as_ptr(item) as usize
}

/// Everything a message holds, written out (`Message` has no `PartialEq`, and its
/// `Debug` leaves out what it says). Destructured, so that a new field is not missed.
fn whole(message: &Message) -> String {
    let Message {
        id,
        item,
        kind,
        state,
        sender,
        sender_id,
        sender_ambiguous,
        time,
        day,
        sent_at,
        encrypted,
        body,
        own,
        edited,
        pinned,
        saved,
        html,
        reply_to,
        reply_sender,
        subject,
        reply_event,
        reactions,
        image,
        file,
        thread,
        poll,
        upload,
        send_error,
        shield,
        mentions,
        read_by,
        utd,
    } = message;
    let item = item.as_ref().map(|item| match item {
        ItemRef::Event(id) => format!("event {id}"),
        ItemRef::Transaction(id) => format!("transaction {id}"),
    });
    let reactions: Vec<String> = reactions
        .iter()
        .map(|Reaction { key, count, own }| format!("{key} {count} {own}"))
        .collect();
    let image = image.as_ref().map(
        |ImageInfo {
             key,
             source,
             name,
             caption,
             dimensions,
             size,
             sticker,
         }| {
            let source = match source {
                ImageSource::Demo => "demo".to_owned(),
                ImageSource::Matrix(media) => {
                    let MediaRef { full, thumbnail } = &**media;
                    format!("{full:?} {thumbnail:?}")
                }
            };
            format!("{key} {source} {name} {caption:?} {dimensions:?} {size:?} {sticker}")
        },
    );
    let file = file.as_ref().map(
        |Attachment {
             key,
             name,
             size,
             kind,
             source,
             audio,
         }| {
            let audio = audio.as_deref().map(
                |AudioMeta {
                     duration,
                     waveform,
                     mime,
                 }| format!("{duration:?} {waveform:?} {mime:?}"),
            );
            format!("{key} {name} {size:?} {kind:?} {source:?} {audio:?}")
        },
    );
    let thread = thread.as_ref().map(
        |ThreadSummary {
             replies,
             latest,
             latest_sender,
             unread,
             demo_replies,
         }| {
            let demo: Vec<String> = demo_replies.iter().map(whole).collect();
            format!("{replies} {latest:?} {latest_sender:?} {unread} {demo:?}")
        },
    );
    let poll = poll.as_ref().map(
        |PollInfo {
             question,
             disclosed,
             max_selections,
             answers,
             total_voters,
             ended,
             editable,
         }| {
            let answers: Vec<String> = answers
                .iter()
                .map(
                    |PollAnswer {
                         id,
                         text,
                         votes,
                         own,
                     }| format!("{id} {text} {votes} {own}"),
                )
                .collect();
            format!(
                "{question} {disclosed} {max_selections} {answers:?} {total_voters} {ended} \
                 {editable}"
            )
        },
    );
    let ReadBy { readers, total } = read_by;
    let readers: Vec<String> = readers
        .iter()
        .map(
            |Reader {
                 name,
                 id,
                 ambiguous,
                 time,
             }| format!("{name} {id:?} {ambiguous} {time}"),
        )
        .collect();
    format!(
        "id {id}; item {item:?}; kind {kind:?}; state {state:?}; sender {sender} \
         {sender_id:?} {sender_ambiguous}; time {time} {day} {sent_at:?}; encrypted {encrypted}; body {body}; \
         own {own}; edited {edited}; pinned {pinned}; saved {saved}; html {html:?}; \
         reply {reply_to:?} {reply_sender:?} {reply_event:?}; subject {subject:?}; reactions {reactions:?}; image {image:?}; \
         file {file:?}; thread {thread:?}; poll {poll:?}; upload {upload:?}; \
         send error {send_error:?}; shield {shield:?}; mentions {mentions:?}; \
         read by {readers:?} {total}; utd {utd:?}"
    )
}

/// Nothing changed: every message is used again, none made again.
#[tokio::test]
async fn an_item_that_did_not_change_is_used_again() {
    let mut stage = Stage::new(true).await;
    let events = stage.events();
    stage.check("again");
    assert_eq!(stage.reuse.counts, (0, events));
    stage.check("and again");
    assert_eq!(stage.reuse.counts, (0, events));
}

#[tokio::test]
async fn an_edited_message_is_made_again() {
    let mut stage = Stage::new(true).await;
    let edited = stage.edit().await;
    let message = stage.message(&edited).unwrap();
    assert!(message.edited);
    assert_eq!(message.body, "said otherwise");
}

#[tokio::test]
async fn a_message_with_new_reactions_is_made_again() {
    let mut stage = Stage::new(true).await;
    let target = stage.react().await;
    let message = stage.message(&target).unwrap();
    let crab = message.reactions.iter().find(|r| r.key == "🦀").unwrap();
    assert_eq!((crab.count, crab.own), (2, true));
}

/// The receipt is not part of what `convert` makes (the receipts are named on the
/// whole list after it), but the SDK puts a new item in place: made again, the same.
#[tokio::test]
async fn a_message_with_a_new_receipt_is_made_again() {
    let mut stage = Stage::new(true).await;
    stage.receipt().await;
}

#[tokio::test]
async fn a_deleted_message_is_made_again() {
    let mut stage = Stage::new(true).await;
    let target = stage.redact().await;
    assert_eq!(
        stage.message(&target).map(|m| m.kind),
        Some(crate::model::MsgKind::Redacted)
    );
}

#[tokio::test]
async fn a_message_of_someone_who_took_another_name_is_made_again() {
    let mut stage = Stage::new(true).await;
    stage.rename().await;
}

#[tokio::test]
async fn people_of_the_same_name_are_named_with_their_ids() {
    let mut stage = Stage::new(true).await;
    stage.namesakes().await;
}

#[tokio::test]
async fn a_reply_whose_original_arrives_is_made_again() {
    let mut stage = Stage::new(true).await;
    stage.reply_details().await;
}

#[tokio::test]
async fn a_message_that_started_a_thread_is_made_again() {
    let mut stage = Stage::new(true).await;
    stage.thread_reply().await;
}

#[tokio::test]
async fn a_message_of_ours_is_made_again_as_it_is_sent_and_refused() {
    let mut stage = Stage::new(true).await;
    stage.send().await;
}

#[tokio::test]
async fn a_message_that_can_be_decrypted_now_is_made_again() {
    let mut stage = Stage::new(true).await;
    stage.decrypt().await;
}

#[tokio::test]
async fn everything_is_made_again_when_the_ignore_list_or_the_day_changes() {
    let mut stage = Stage::new(true).await;
    stage.ignore();
    stage.next_day();
}

/// The main proof: one change after the other, of every kind, and after every batch of
/// diffs the timeline sends, what was used again is what is made afresh.
#[tokio::test]
async fn what_is_used_again_is_what_is_made_afresh_after_every_change() {
    let mut stage = Stage::new(false).await;
    stage.page_back().await;
    stage.new_messages("$new").await;
    stage.edit().await;
    stage.react().await;
    stage.receipt().await;
    stage.redact().await;
    stage.rename().await;
    stage.namesakes().await;
    stage.reply_details().await;
    stage.thread_reply().await;
    stage.send().await;
    stage.decrypt().await;
    stage.read_marker().await;
    stage.ignore();
    stage.new_messages("$newer").await;
    stage.next_day();
    stage.check("at last");
    let (made, used) = stage.totals;
    assert!(stage.checks > 20, "{} checks", stage.checks);
    // Most messages were used again.
    assert!(used > 2 * made, "{made} made, {used} used again");
}
