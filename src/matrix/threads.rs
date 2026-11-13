//! Threads: a timeline that shows one thread, and the line under a message that
//! started one.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use matrix_sdk::ruma::EventId;
use matrix_sdk::{Room, ThreadingSupport};
use matrix_sdk_ui::eyeball_im::Vector;
use matrix_sdk_ui::timeline::{
    MsgLikeContent, RoomExt, Timeline, TimelineDetails, TimelineEventItemId, TimelineFocus,
    TimelineItem, TimelineReadReceiptTracking,
};

use super::convert::display_name;
use crate::model::{ItemRef, Message, ThreadSummary};
use crate::text::{sanitize_line, truncate};

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
        .with_focus(TimelineFocus::Thread { root_event_id })
        .track_read_marker_and_receipts(TimelineReadReceiptTracking::MessageLikeEvents)
        .build()
        .await
        .context("could not open the thread")
}

/// What to show under a message that started a thread. Text from other people is
/// made safe for the terminal here.
pub fn summary(content: &MsgLikeContent) -> Option<ThreadSummary> {
    let summary = content.thread_summary.as_ref()?;
    // Every reply was deleted: nothing to announce.
    if summary.num_replies == 0 {
        return None;
    }
    let latest = match &summary.latest_event {
        TimelineDetails::Ready(event) => {
            let text = match event.content.as_message() {
                Some(message) => sanitize_line(message.body()),
                None => "a message".to_owned(),
            };
            Some((
                sanitize_line(&display_name(&event.sender_profile, &event.sender)),
                truncate(text.trim(), PREVIEW),
            ))
        }
        _ => None,
    };
    Some(ThreadSummary {
        replies: summary.num_replies,
        latest,
        unread: false,
        demo_replies: Vec::new(),
    })
}

/// The newest reply read in each thread (by `key`) during this session. Servers that
/// keep one read receipt per room and user never confirm a thread's receipt, and the
/// thread would stay "new" although it was just read.
#[derive(Clone, Default)]
pub struct ReadMarks(Arc<Mutex<HashMap<String, String>>>);

impl ReadMarks {
    pub fn set(&self, key: String, newest: String) {
        if let Ok(mut marks) = self.0.lock() {
            marks.insert(key, newest);
        }
    }

    fn covers(&self, key: &str, newest: &str) -> bool {
        self.0
            .lock()
            .is_ok_and(|marks| marks.get(key).is_some_and(|read| read == newest))
    }
}

/// Event ids of the messages in `items` whose threads have replies the user has not
/// read, going by what this device knows of each thread.
pub async fn unread(
    room: &Room,
    items: &Vector<Arc<TimelineItem>>,
    read: &ReadMarks,
) -> HashSet<String> {
    let client = room.client();
    let cache = client.event_cache();
    let mut unread = HashSet::new();
    for event in items.iter().filter_map(|item| item.as_event()) {
        let Some(event_id) = event.event_id() else {
            continue;
        };
        let Some(summary) = event.content().thread_summary() else {
            continue;
        };
        if let TimelineDetails::Ready(newest) = &summary.latest_event
            && let TimelineEventItemId::EventId(newest) = &newest.identifier
            && read.covers(
                &key(room.room_id().as_str(), Some(event_id.as_str())),
                newest.as_str(),
            )
        {
            continue;
        }
        let count = match cache.thread(room.room_id(), event_id).await {
            Ok((thread, _)) => thread.num_unread_messages().await.unwrap_or(0),
            Err(_) => 0,
        };
        if count > 0 {
            unread.insert(event_id.to_string());
        }
    }
    unread
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
        assert!(!marks.covers("room\n$root", "$one"));
        marks
            .clone()
            .set("room\n$root".to_owned(), "$one".to_owned());
        assert!(marks.covers("room\n$root", "$one"));
        // A newer reply, or another thread, is unread again.
        assert!(!marks.covers("room\n$root", "$two"));
        assert!(!marks.covers("room\n$other", "$one"));
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
                    }
                    _ => {}
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), wait)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
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
        use crate::matrix::{MatrixCmd, MatrixEvent, RoomCategory, spawn};
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
            password,
        });

        // A room both are in, without encryption (the events are read as sent below).
        let find_room = async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    MatrixEvent::Rooms(rooms) => {
                        let wanted = var("MATRIX_RAT_TEST_ROOM");
                        let shared = rooms.iter().find(|summary| {
                            summary.category != RoomCategory::Invite
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
            for hidden in ["one", "two", "three", "four"] {
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
            let bodies: Vec<&str> = m.iter().map(|m| m.body.as_str()).collect();
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

        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        syncing.abort();
        let _ = other.matrix_auth().logout().await;
        let _ = std::fs::remove_dir_all(root_dir);
    }

    async fn threads_open(room: &Room, root: &str) -> Timeline {
        open(room, root).await.expect("thread timeline")
    }
}
