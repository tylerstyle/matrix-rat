//! A change of ours that could not be sent (a message, an edit, a deletion, a reaction)
//! is shown so, holds up what we send after it in the room, and is tried again
//! (`Retry`) or thrown away (`Discard`, or `Redact` for a message): on a stand-in server
//! that refuses what it is told to, the way it is told to.
//!
//! 503 is a failure the SDK takes as passing: it tries three times, then stops the
//! room's queue and keeps the request. 403 is one it does not: the request is parked
//! ("wedged") and blocks the room's queue, which stays on.

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};

use serde_json::json;
use tokio::sync::mpsc::UnboundedReceiver;

use super::tests::{fake_client_with_cache, live_timelines_of, message_of, said, timeline_where};
use crate::format::Outgoing;
use crate::matrix::spaces::fake;
use crate::matrix::{MatrixCmd, MatrixEvent};
use crate::model::{ItemRef, Message, MsgKind, SendState};

const ROOM: &str = "!failed:localhost";
/// A message of ours, and one of bob's, on the server from the start.
const MINE: &str = "$mine:localhost";
const BOBS: &str = "$bobs:localhost";

/// What the stand-in server refuses, while it refuses.
#[derive(Clone, Copy)]
enum Refused {
    /// Edits.
    Edit,
    /// Our new message saying this.
    Message(&'static str),
    Reaction,
    Deletion,
}

impl Refused {
    fn matches(self, request: &fake::Request) -> bool {
        let message = request.path.contains("/send/m.room.message/");
        let edit = request.body.contains("m.new_content");
        match self {
            Self::Edit => message && edit,
            Self::Message(text) => {
                message && !edit && request.body.contains(&format!("\"{text}\""))
            }
            Self::Reaction => request.path.contains("/send/m.reaction/"),
            Self::Deletion => request.path.contains("/redact/"),
        }
    }
}

/// The room on the stand-in server, open, with what it takes to change it.
struct Stage {
    server: fake::Server,
    client: matrix_sdk::Client,
    timelines: super::Timelines,
    events: UnboundedReceiver<MatrixEvent>,
    _watches: (
        tokio::sync::watch::Sender<super::IgnoredSet>,
        tokio::sync::watch::Sender<u64>,
    ),
    /// The status what is refused gets; 0 while nothing is.
    status: Arc<AtomicU16>,
}

impl Stage {
    async fn new(refused: Refused, status: u16) -> Self {
        let mut joined = fake::joined(false, vec![]);
        joined["timeline"]["events"] = json!([
            said(ROOM, MINE, fake::ME, "original", None),
            said(ROOM, BOBS, "@bob:localhost", "from bob", None),
        ]);
        let status = Arc::new(AtomicU16::new(status));
        let refusing = status.clone();
        let sent = AtomicU64::new(0);
        let server = fake::Server::answering(
            json!({ "join": { ROOM: joined } }),
            Box::new(move |request| {
                let path = request.path.as_str();
                // The room is not encrypted: sending asks before it sends.
                if request.method == "GET" && path.contains("/state/m.room.encryption") {
                    return Some(fake::Reply::Status(
                        404,
                        json!({ "errcode": "M_NOT_FOUND", "error": "not found" }).to_string(),
                    ));
                }
                if request.method != "PUT"
                    || !(path.contains("/send/") || path.contains("/redact/"))
                {
                    return None;
                }
                let status = refusing.load(Ordering::SeqCst);
                if status != 0 && refused.matches(request) {
                    let errcode = if status == 403 {
                        "M_FORBIDDEN"
                    } else {
                        "M_UNKNOWN"
                    };
                    return Some(fake::Reply::Status(
                        status,
                        json!({ "errcode": errcode, "error": "no" }).to_string(),
                    ));
                }
                let n = sent.fetch_add(1, Ordering::SeqCst);
                Some(fake::Reply::Json(
                    json!({ "event_id": format!("$sent{n}:localhost") }).to_string(),
                ))
            }),
        )
        .await;
        let client = fake_client_with_cache(&server.url).await;
        let (mut timelines, mut events, watches) =
            live_timelines_of(client.clone(), &crate::matrix::left::LeftRooms::default());
        timelines
            .handle(MatrixCmd::OpenRoom {
                room_id: ROOM.to_owned(),
                generation: 1,
            })
            .await;
        timeline_where(&mut events, ROOM, 1, |messages| {
            message_of(messages, MINE).is_some() && message_of(messages, BOBS).is_some()
        })
        .await;
        Self {
            server,
            client,
            timelines,
            events,
            _watches: watches,
            status,
        }
    }

    /// The server takes everything from now on.
    fn accept(&self) {
        self.status.store(0, Ordering::SeqCst);
    }

    /// The first messages of the room that `ready` accepts.
    async fn until(&mut self, ready: impl Fn(&[Message]) -> bool) -> Vec<Message> {
        timeline_where(&mut self.events, ROOM, 1, ready).await
    }

    async fn handle(&mut self, command: MatrixCmd) {
        self.timelines.handle(command).await;
    }

    async fn send(&mut self, text: &str) {
        self.handle(MatrixCmd::Send {
            room_id: ROOM.to_owned(),
            message: text_of(text),
            reply_to: None,
        })
        .await;
    }

    async fn edit_mine(&mut self, text: &str) {
        self.handle(MatrixCmd::Edit {
            room_id: ROOM.to_owned(),
            item: ItemRef::Event(MINE.to_owned()),
            message: text_of(text),
        })
        .await;
    }

    async fn retry(&mut self, item: ItemRef) {
        self.handle(MatrixCmd::Retry {
            room_id: ROOM.to_owned(),
            item,
        })
        .await;
    }

    async fn discard(&mut self, item: ItemRef) {
        self.handle(MatrixCmd::Discard {
            room_id: ROOM.to_owned(),
            item,
        })
        .await;
    }

    /// Whether the room's send queue is on.
    fn queue_on(&self) -> bool {
        let room_id = <&matrix_sdk::ruma::RoomId>::try_from(ROOM).unwrap();
        self.client
            .get_room(room_id)
            .unwrap()
            .send_queue()
            .is_enabled()
    }

    /// How many requests of this kind the server was sent.
    fn sent(&self, refused: Refused) -> usize {
        self.server
            .requests()
            .iter()
            .filter(|request| request.method == "PUT" && refused.matches(request))
            .count()
    }
}

fn text_of(text: &str) -> Outgoing {
    Outgoing::Text {
        plain: text.to_owned(),
        html: None,
        mentions: None,
    }
}

/// Our message on its way (a local echo) that says `body`.
fn echo<'a>(messages: &'a [Message], body: &str) -> Option<&'a Message> {
    messages.iter().find(|message| {
        message.body == body && matches!(message.item, Some(ItemRef::Transaction(_)))
    })
}

fn held(messages: &[Message], body: &str) -> bool {
    echo(messages, body).is_some_and(|m| m.state == SendState::Sending && m.held)
}

/// Our message saying `body` went out: the server took it (once it has, the SDK may show
/// the event in place of the local echo).
fn gone_out(messages: &[Message], body: &str) -> bool {
    messages
        .iter()
        .any(|m| m.body == body && m.own && m.state == SendState::Sent && !m.held)
}

fn transaction(messages: &[Message], body: &str) -> ItemRef {
    echo(messages, body)
        .and_then(|m| m.item.clone())
        .unwrap_or_else(|| panic!("no message saying {body:?}"))
}

/// An edit the server answers 503: after the SDK's three tries it has failed, the
/// room's queue is off, and a message sent after it is held up. Discarding the edit
/// brings the old text back, and the held message goes out with no `Retry`: the queue
/// is turned on again after the abort.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_that_failed_for_now_is_discarded_and_what_waited_goes_out() {
    let mut stage = Stage::new(Refused::Edit, 503).await;
    stage.edit_mine("changed").await;
    let messages = stage
        .until(|messages| message_of(messages, MINE).is_some_and(|m| m.edit_error.is_some()))
        .await;
    let mine = message_of(&messages, MINE).unwrap();
    assert_eq!(mine.body, "changed");
    assert!(mine.edit_error.is_some());
    assert!(mine.failed_any());
    assert!(
        !stage.queue_on(),
        "a passing failure stops the room's queue"
    );

    stage.send("after").await;
    stage.until(|messages| held(messages, "after")).await;

    stage.discard(ItemRef::Event(MINE.to_owned())).await;
    let messages = stage
        .until(|messages| {
            message_of(messages, MINE).is_some_and(|m| m.body == "original")
                && gone_out(messages, "after")
        })
        .await;
    let mine = message_of(&messages, MINE).unwrap();
    assert_eq!(mine.edit_error, None);
    assert!(!mine.edited);
    assert!(stage.queue_on());
}

/// The same with a message of ours that failed for now in place of the edit, thrown
/// away with Del (`Redact` on it): the message held up behind it goes out.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_that_failed_for_now_is_discarded_and_what_waited_goes_out() {
    let mut stage = Stage::new(Refused::Message("first"), 503).await;
    stage.send("first").await;
    let messages = stage
        .until(|messages| echo(messages, "first").is_some_and(|m| m.state == SendState::Failed))
        .await;
    let first = transaction(&messages, "first");
    assert!(!stage.queue_on());
    stage.send("after").await;
    stage.until(|messages| held(messages, "after")).await;

    stage
        .handle(MatrixCmd::Redact {
            room_id: ROOM.to_owned(),
            item: first,
            reason: None,
        })
        .await;
    let messages = stage
        .until(|messages| echo(messages, "first").is_none() && gone_out(messages, "after"))
        .await;
    assert!(messages.iter().all(|m| !m.failed_any()));
    assert!(stage.queue_on());
}

/// A message that failed for now is sent again by `Retry` once the server takes it: the
/// queue is turned on again after the retry.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_that_failed_for_now_is_sent_by_retry() {
    let mut stage = Stage::new(Refused::Message("first"), 503).await;
    stage.send("first").await;
    let messages = stage
        .until(|messages| echo(messages, "first").is_some_and(|m| m.state == SendState::Failed))
        .await;
    assert!(!stage.queue_on());
    stage.accept();
    stage.retry(transaction(&messages, "first")).await;
    stage.until(|messages| gone_out(messages, "first")).await;
}

/// An edit the server refuses (403): the message says so and a message sent after it
/// is held up. `Retry`, once the server takes it: the edit goes out, then the message.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_edit_is_sent_by_retry_and_what_waited_goes_out() {
    let mut stage = Stage::new(Refused::Edit, 403).await;
    stage.edit_mine("changed").await;
    let messages = stage
        .until(|messages| message_of(messages, MINE).is_some_and(|m| m.edit_error.is_some()))
        .await;
    assert_eq!(
        message_of(&messages, MINE).unwrap().edit_error.as_deref(),
        Some("not allowed in this room")
    );
    assert!(
        stage.queue_on(),
        "a refusal leaves the queue on, and blocked"
    );
    stage.send("after").await;
    stage.until(|messages| held(messages, "after")).await;

    stage.accept();
    stage.retry(ItemRef::Event(MINE.to_owned())).await;
    let messages = stage
        .until(|messages| {
            message_of(messages, MINE).is_some_and(|m| m.edit_error.is_none())
                && gone_out(messages, "after")
        })
        .await;
    let mine = message_of(&messages, MINE).unwrap();
    assert_eq!(mine.body, "changed");
    assert!(mine.edited);
    assert_eq!(stage.sent(Refused::Edit), 2, "refused once, then taken");
}

/// The same, discarded: the old text is back, it is not edited, and the message held
/// up goes out.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_edit_is_discarded_and_what_waited_goes_out() {
    let mut stage = Stage::new(Refused::Edit, 403).await;
    stage.edit_mine("changed").await;
    stage
        .until(|messages| message_of(messages, MINE).is_some_and(|m| m.edit_error.is_some()))
        .await;
    stage.send("after").await;
    stage.until(|messages| held(messages, "after")).await;

    stage.discard(ItemRef::Event(MINE.to_owned())).await;
    let messages = stage
        .until(|messages| {
            message_of(messages, MINE).is_some_and(|m| m.edit_error.is_none())
                && gone_out(messages, "after")
        })
        .await;
    let mine = message_of(&messages, MINE).unwrap();
    assert_eq!(mine.body, "original");
    assert!(!mine.edited);
    assert_eq!(stage.sent(Refused::Edit), 1, "never sent again");
}

/// A deletion the server refuses (here of bob's message, as a moderator): the message
/// shows as deleted, and says the deletion was not sent. Discarding it brings the text
/// back; a deletion refused again and then tried again deletes it for good.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_deletion_is_kept_back_or_sent_by_retry() {
    let mut stage = Stage::new(Refused::Deletion, 403).await;
    let delete = || MatrixCmd::Redact {
        room_id: ROOM.to_owned(),
        item: ItemRef::Event(BOBS.to_owned()),
        reason: None,
    };
    stage.handle(delete()).await;
    let messages = stage
        .until(|messages| message_of(messages, BOBS).is_some_and(|m| m.delete_error.is_some()))
        .await;
    let bobs = message_of(&messages, BOBS).unwrap();
    assert_eq!(bobs.kind, MsgKind::Redacted);
    assert_eq!(bobs.body, "Message deleted");
    assert!(!bobs.own && bobs.failed_any());

    stage.discard(ItemRef::Event(BOBS.to_owned())).await;
    let messages = stage
        .until(|messages| message_of(messages, BOBS).is_some_and(|m| m.delete_error.is_none()))
        .await;
    let bobs = message_of(&messages, BOBS).unwrap();
    assert_eq!(bobs.kind, MsgKind::Text);
    assert_eq!(bobs.body, "from bob");
    assert_eq!(stage.sent(Refused::Deletion), 1);

    stage.handle(delete()).await;
    stage
        .until(|messages| message_of(messages, BOBS).is_some_and(|m| m.delete_error.is_some()))
        .await;
    stage.accept();
    stage.retry(ItemRef::Event(BOBS.to_owned())).await;
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while stage.sent(Refused::Deletion) < 3 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the deletion is sent again");
    let messages = stage
        .until(|messages| message_of(messages, BOBS).is_some_and(|m| m.delete_error.is_none()))
        .await;
    assert_eq!(message_of(&messages, BOBS).unwrap().kind, MsgKind::Redacted);
}

/// A reaction the server refuses (on bob's message): the chip says so. Toggling it
/// takes it away, and nothing more is sent.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_reaction_is_taken_back_by_toggling_it() {
    let mut stage = Stage::new(Refused::Reaction, 403).await;
    let toggle = || MatrixCmd::ToggleReaction {
        room_id: ROOM.to_owned(),
        item: ItemRef::Event(BOBS.to_owned()),
        key: "👍".to_owned(),
        shortcode: None,
    };
    stage.handle(toggle()).await;
    let messages = stage
        .until(|messages| {
            message_of(messages, BOBS).is_some_and(|m| m.reactions.iter().any(|r| r.failed))
        })
        .await;
    let bobs = message_of(&messages, BOBS).unwrap();
    assert!(bobs.reactions[0].own);
    assert!(bobs.failed_any());
    assert_eq!(stage.sent(Refused::Reaction), 1);

    stage.handle(toggle()).await;
    stage
        .until(|messages| message_of(messages, BOBS).is_some_and(|m| m.reactions.is_empty()))
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(stage.sent(Refused::Reaction), 1, "nothing sent");
    assert!(
        !stage
            .server
            .requests()
            .iter()
            .any(|request| request.path.contains("/redact/")),
        "a reaction that never went out is not deleted on the server"
    );
}

/// `Retry` and `Discard` on a message with nothing waiting say so.
#[tokio::test(flavor = "multi_thread")]
async fn nothing_to_retry_or_discard_is_said() {
    let mut stage = Stage::new(Refused::Edit, 0).await;
    stage.retry(ItemRef::Event(MINE.to_owned())).await;
    stage.discard(ItemRef::Event(MINE.to_owned())).await;
    let mut toasts = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while toasts.len() < 2 {
            if let Some(MatrixEvent::ActionFailed(text)) = stage.events.recv().await {
                toasts.push(text);
            }
        }
    })
    .await
    .expect("both answered");
    assert_eq!(
        toasts,
        [
            "Could not retry: nothing waits to be sent here any more",
            "Could not discard: nothing waits to be sent here any more",
        ]
    );
}

/// Back online: once the sync is through again after it was offline (the sync arm
/// calls `back_online` then), every room's queue is on again, and a message that failed
/// for now goes out by itself, with the one held up behind it. Called directly: the
/// stand-in server does not speak sliding sync, so the sync's state cannot come from
/// it.
#[tokio::test(flavor = "multi_thread")]
async fn back_online_sends_what_failed_for_now() {
    let mut stage = Stage::new(Refused::Message("first"), 503).await;
    stage.send("first").await;
    stage
        .until(|messages| echo(messages, "first").is_some_and(|m| m.state == SendState::Failed))
        .await;
    stage.send("after").await;
    stage.until(|messages| held(messages, "after")).await;
    assert!(!stage.queue_on());
    stage.accept();

    // Search history switched off: only the send queues are left to it.
    let (events, _received) = tokio::sync::mpsc::unbounded_channel();
    let mut history = crate::matrix::search_history::SearchHistory::new(
        stage.client.clone(),
        events,
        false,
        None,
    );
    crate::matrix::back_online(&stage.client, &mut history).await;
    assert!(stage.queue_on());
    stage
        .until(|messages| gone_out(messages, "first") && gone_out(messages, "after"))
        .await;
}

/// On the dev homeserver, a send fails for real the same way every time: by permission.
/// Bob makes a room, invites alice (the worker) and raises the level needed to send
/// anything (`events_default`) to 50 whenever alice is to fail.
/// 1. Alice's edit is refused and her next message waits; with the level back down,
///    `Retry` sends both, and bob sees the edit.
/// 2. Her reaction is refused; `Discard` takes it back, and bob never sees it.
/// 3. Her deletion of her message is refused; `Discard` keeps the message, with its
///    text (`abort_send` of a deletion of an event on the server puts the content
///    back), and bob still sees it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local homeserver (scripts/dev-homeserver.sh)"]
async fn failed_changes_can_be_retried_or_discarded() {
    use std::time::Duration;

    use matrix_sdk::config::SyncSettings;
    use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoom;
    use matrix_sdk::ruma::events::TimelineEventType;
    use matrix_sdk::ruma::events::room::power_levels::RoomPowerLevelsEventContent;
    use serde_json::Value;

    use crate::matrix::spawn;
    use crate::paths::Paths;

    let _ = tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
        .with_test_writer()
        .try_init();
    let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
        .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
    let pid = std::process::id();

    let bob = matrix_sdk::Client::builder()
        .homeserver_url(&homeserver)
        .build()
        .await
        .unwrap();
    bob.matrix_auth()
        .login_username("bob", "bobpass")
        .initial_device_display_name("matrix-rat failed changes test")
        .send()
        .await
        .unwrap();
    bob.sync_once(SyncSettings::default()).await.unwrap();
    let mut create = CreateRoom::new();
    create.invite = vec!["@alice:localhost".try_into().unwrap()];
    create.name = Some(format!("failed changes {pid}"));
    let bob_room = bob.create_room(create).await.expect("create room");
    let room_id = bob_room.room_id().to_string();
    // Sending anything at all takes this level.
    let set_level = |level: i64| {
        let room = bob_room.clone();
        async move {
            let mut levels = room.power_levels().await.unwrap();
            levels.events_default = level.try_into().unwrap();
            for kind in [
                TimelineEventType::RoomMessage,
                TimelineEventType::Reaction,
                TimelineEventType::RoomRedaction,
            ] {
                levels.events.remove(&kind);
            }
            // Room v12+ forbids listing privileged creators in `users`.
            if let Some(creators) = &levels.rules.privileged_creators {
                for creator in creators.clone() {
                    levels.users.remove(&creator);
                }
            }
            let content = RoomPowerLevelsEventContent::try_from(levels).unwrap();
            room.send_state_event(content).await.unwrap();
        }
    };
    // What bob's server gives him of the room's latest events, as JSON.
    let bob_sees = |room: matrix_sdk::Room| async move {
        let page = room
            .messages(matrix_sdk::room::MessagesOptions::backward())
            .await
            .unwrap();
        page.chunk
            .iter()
            .filter_map(|event| serde_json::from_str::<Value>(event.raw().json().get()).ok())
            .collect::<Vec<Value>>()
    };

    let root = std::env::temp_dir().join(format!("matrix-rat-failed-{pid}"));
    let _ = std::fs::remove_dir_all(&root);
    let paths = Paths {
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        log_dir: root.join("log"),
        runtime_dir: root.join("run"),
        profile: "it".into(),
    };
    paths.create_dirs().unwrap();
    let (handle, mut events) = spawn(paths);
    handle.send(MatrixCmd::LoginPassword {
        homeserver: homeserver.clone(),
        username: "alice".to_owned(),
        password: "alicepass".to_owned().into(),
    });
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match events.recv().await.expect("worker stopped") {
                MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                MatrixEvent::Rooms(rooms) if rooms.iter().any(|r| r.id == room_id) => return,
                _ => {}
            }
        }
    })
    .await
    .expect("the invitation arrives");
    handle.send(MatrixCmd::Join {
        room_id: room_id.clone(),
    });
    handle.send(MatrixCmd::OpenRoom {
        room_id: room_id.clone(),
        generation: 1,
    });
    // The worker's timeline once `ready` accepts it.
    macro_rules! until {
        ($what:expr, $ready:expr) => {
            tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    match events.recv().await.expect("worker stopped") {
                        MatrixEvent::ActionFailed(err) | MatrixEvent::Error(err) => {
                            panic!("{err}")
                        }
                        MatrixEvent::Timeline {
                            room_id: shown,
                            messages,
                            ..
                        } if shown == room_id && ($ready)(&messages[..]) => break messages,
                        _ => {}
                    }
                }
            })
            .await
            .expect($what)
        };
    }
    let text = |body: &str| Outgoing::Text {
        plain: body.to_owned(),
        html: None,
        mentions: None,
    };
    let send = |body: &str| MatrixCmd::Send {
        room_id: room_id.clone(),
        message: text(body),
        reply_to: None,
    };
    let on_server = |messages: &[Message], body: &str| {
        messages
            .iter()
            .find(|m| m.body == body && matches!(m.item, Some(ItemRef::Event(_))))
            .and_then(|m| m.item.clone())
    };

    // Alice's message, on the server.
    let original = format!("original {pid}");
    handle.send(send(&original));
    let messages = until!("the message is sent", |messages: &[Message]| on_server(
        messages, &original
    )
    .is_some());
    let mine = on_server(&messages, &original).unwrap();
    let ItemRef::Event(mine_id) = mine.clone() else {
        unreachable!()
    };
    let is_mine = |m: &&Message| m.item.as_ref() == Some(&mine);

    // 1. An edit refused, a message held up, both sent by `Retry`.
    set_level(50).await;
    let edited = format!("edited {pid}");
    handle.send(MatrixCmd::Edit {
        room_id: room_id.clone(),
        item: mine.clone(),
        message: text(&edited),
    });
    until!("the edit fails", |messages: &[Message]| messages
        .iter()
        .find(is_mine)
        .is_some_and(
            |m| m.edit_error.as_deref() == Some("not allowed in this room")
        ));
    let after_edit = format!("after the edit {pid}");
    handle.send(send(&after_edit));
    until!("the message waits", |messages: &[Message]| messages
        .iter()
        .any(|m| m.body == after_edit
            && m.held
            && m.state == SendState::Sending));
    set_level(0).await;
    handle.send(MatrixCmd::Retry {
        room_id: room_id.clone(),
        item: mine.clone(),
    });
    until!("both go out", |messages: &[Message]| messages
        .iter()
        .find(is_mine)
        .is_some_and(|m| m.edit_error.is_none() && m.body == edited)
        && messages.iter().any(|m| m.body == after_edit
            && m.state == SendState::Sent
            && !m.held));
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let seen = bob_sees(bob_room.clone()).await;
            let edit = seen
                .iter()
                .any(|event| event["content"]["m.new_content"]["body"] == edited.as_str());
            let after = seen
                .iter()
                .any(|event| event["content"]["body"] == after_edit.as_str());
            if edit && after {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("bob sees the edit and the message after it");

    // 2. A reaction refused, discarded: bob never sees it.
    set_level(50).await;
    handle.send(MatrixCmd::ToggleReaction {
        room_id: room_id.clone(),
        item: mine.clone(),
        key: "👍".to_owned(),
        shortcode: None,
    });
    until!("the reaction fails", |messages: &[Message]| messages
        .iter()
        .find(is_mine)
        .is_some_and(|m| m.reactions.iter().any(|r| r.failed)));
    let after_reaction = format!("after the reaction {pid}");
    handle.send(send(&after_reaction));
    until!("the message waits", |messages: &[Message]| messages
        .iter()
        .any(|m| m.body == after_reaction && m.held));
    set_level(0).await;
    handle.send(MatrixCmd::Discard {
        room_id: room_id.clone(),
        item: mine.clone(),
    });
    until!(
        "the reaction is gone, the message out",
        |messages: &[Message]| messages
            .iter()
            .find(is_mine)
            .is_some_and(|m| m.reactions.is_empty())
            && on_server(messages, &after_reaction).is_some()
    );
    let seen = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let seen = bob_sees(bob_room.clone()).await;
            if seen
                .iter()
                .any(|event| event["content"]["body"] == after_reaction.as_str())
            {
                return seen;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("bob sees the message after the reaction");
    assert!(
        !seen.iter().any(|event| event["type"] == "m.reaction"),
        "bob never sees the reaction"
    );

    // 3. A deletion refused, discarded: the message stays, here and for bob.
    set_level(50).await;
    handle.send(MatrixCmd::Redact {
        room_id: room_id.clone(),
        item: mine.clone(),
        reason: None,
    });
    until!("the deletion fails", |messages: &[Message]| messages
        .iter()
        .find(is_mine)
        .is_some_and(
            |m| m.delete_error.is_some() && m.kind == MsgKind::Redacted
        ));
    let after_deletion = format!("after the deletion {pid}");
    handle.send(send(&after_deletion));
    until!("the message waits", |messages: &[Message]| messages
        .iter()
        .any(|m| m.body == after_deletion && m.held));
    set_level(0).await;
    handle.send(MatrixCmd::Discard {
        room_id: room_id.clone(),
        item: mine.clone(),
    });
    let messages = until!(
        "the message is back, the next one out",
        |messages: &[Message]| {
            messages
                .iter()
                .find(is_mine)
                .is_some_and(|m| m.delete_error.is_none() && m.kind == MsgKind::Text)
                && on_server(messages, &after_deletion).is_some()
        }
    );
    let back = messages.iter().find(is_mine).unwrap();
    assert_eq!(back.body, edited, "its text is back, edit and all");
    assert!(back.deleted.is_none());
    let seen = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let seen = bob_sees(bob_room.clone()).await;
            if seen
                .iter()
                .any(|event| event["content"]["body"] == after_deletion.as_str())
            {
                return seen;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("bob sees the message after the deletion");
    assert!(
        !seen.iter().any(|event| event["type"] == "m.room.redaction"),
        "no deletion reached the server"
    );
    let original_event = seen
        .iter()
        .find(|event| event["event_id"] == mine_id.as_str())
        .expect("bob has the message");
    assert_eq!(
        original_event["content"]["body"],
        Value::from(original.as_str())
    );

    handle.send(MatrixCmd::Shutdown);
    while events.recv().await.is_some() {}
    let _ = bob_room.leave().await;
    let _ = bob.matrix_auth().logout().await;
    let _ = std::fs::remove_dir_all(root);
}
