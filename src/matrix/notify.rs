//! Turns the SDK's notifications (messages that the account's push rules say deserve
//! one) into plain notices for the UI, which decides whether to show them.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use matrix_sdk::deserialized_responses::RawAnySyncOrStrippedTimelineEvent;
use matrix_sdk::ruma::events::room::message::MessageType;
use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent};
use matrix_sdk::sync::Notification;
use matrix_sdk::{Client, Room};
use tokio::sync::mpsc::UnboundedSender;

use super::MatrixEvent;
use crate::text::{sanitize_line, truncate};

/// Messages older than this when they arrive are history catching up, not news.
const FRESH: Duration = Duration::from_secs(120);
const BODY_COLUMNS: usize = 240;

/// Something worth telling the user about while they are looking elsewhere.
#[derive(Clone, PartialEq, Eq)]
pub struct Notice {
    pub room_id: String,
    /// Room name; for a direct chat this is the other person.
    pub room: String,
    pub sender: String,
    pub body: String,
    /// A mention or keyword hit, as opposed to an ordinary message.
    pub highlight: bool,
    /// Event id of the thread the message belongs to, if any.
    pub thread: Option<String>,
}

/// Hand-written: the text of a message does not belong in a log.
impl std::fmt::Debug for Notice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Notice({}, highlight: {})", self.room_id, self.highlight)
    }
}

/// Invitations carry no time, so old ones cannot be told from new ones. The first
/// syncs of a session deliver every pending invitation again; those are not news.
const SETTLE: Duration = Duration::from_secs(20);

#[derive(Default)]
struct WatchState {
    events: Option<UnboundedSender<MatrixEvent>>,
    /// When the first room list arrived: the session is up from then on.
    armed: Option<std::time::Instant>,
    /// Rooms whose invitation has been announced.
    invited: std::collections::HashSet<String>,
}

/// Keeps notices flowing while it lives. The handler itself cannot be removed from
/// the client, and the client may outlive the session in background tasks; dropping
/// this takes the channel away from the handler, so the worker's event channel can
/// close.
pub struct Watch(std::sync::Arc<std::sync::Mutex<WatchState>>);

impl Watch {
    /// The first room list is in. Invitations count as news once the session has
    /// settled after this, however long the start took.
    pub fn arm(&self) {
        let mut state = self.0.lock().expect("not poisoned");
        state.armed.get_or_insert_with(std::time::Instant::now);
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.0.lock().expect("not poisoned").events.take();
    }
}

/// Registers the handler for this session.
pub async fn watch(client: &Client, events: UnboundedSender<MatrixEvent>) -> Watch {
    let state = std::sync::Arc::new(std::sync::Mutex::new(WatchState {
        events: Some(events),
        ..WatchState::default()
    }));
    let watch = Watch(state.clone());
    client
        .register_notification_handler(move |notification, room, client| {
            let invitation = matches!(
                notification.event,
                RawAnySyncOrStrippedTimelineEvent::Stripped(_)
            );
            let events = {
                let mut state = state.lock().expect("not poisoned");
                let settled = state.armed.is_some_and(|at| at.elapsed() >= SETTLE);
                // An invitation is announced once per room, and only as news.
                if invitation && !(settled && state.invited.insert(room.room_id().to_string())) {
                    // Seen during the start: remember it, so it is not news later.
                    state.invited.insert(room.room_id().to_string());
                    None
                } else {
                    state.events.clone()
                }
            };
            async move {
                let Some(events) = events else {
                    return;
                };
                if let Some(notice) = notice(notification, &room, &client).await {
                    let _ = events.send(MatrixEvent::Notice(notice));
                }
            }
        })
        .await;
    watch
}

async fn notice(notification: Notification, room: &Room, client: &Client) -> Option<Notice> {
    if !notification
        .actions
        .iter()
        .any(|action| action.should_notify())
    {
        return None;
    }
    let highlight = notification
        .actions
        .iter()
        .any(|action| action.is_highlight());
    let room_name = match room.cached_display_name() {
        Some(name) => name.to_string(),
        None => room.room_id().to_string(),
    };
    let mut thread = None;
    let (sender, body) = match &notification.event {
        RawAnySyncOrStrippedTimelineEvent::Sync(raw) => {
            thread = thread_of(raw.json().get());
            let event = raw.deserialize().ok()?;
            if Some(event.sender()) == client.user_id() {
                return None;
            }
            let sent = UNIX_EPOCH + Duration::from_millis(event.origin_server_ts().0.into());
            let age = SystemTime::now().duration_since(sent).unwrap_or_default();
            if age > FRESH {
                return None;
            }
            let sender = match room.get_member_no_sync(event.sender()).await {
                Ok(Some(member)) => member.name().to_owned(),
                _ => event.sender().localpart().to_owned(),
            };
            (sender, describe(&event))
        }
        // An invitation: only the bare facts are known.
        RawAnySyncOrStrippedTimelineEvent::Stripped(_) => {
            (String::new(), "You are invited to this room".to_owned())
        }
    };
    Some(Notice {
        room_id: room.room_id().to_string(),
        room: sanitize_line(&room_name),
        sender: sanitize_line(&sender),
        body: truncate(sanitize_line(&body).trim(), BODY_COLUMNS),
        highlight,
        thread,
    })
}

/// The thread an event belongs to, read from its `m.thread` relation.
fn thread_of(event_json: &str) -> Option<String> {
    let event: serde_json::Value = serde_json::from_str(event_json).ok()?;
    let relation = event.get("content")?.get("m.relates_to")?;
    (relation.get("rel_type")?.as_str()? == "m.thread")
        .then(|| relation.get("event_id")?.as_str().map(sanitize_line))
        .flatten()
}

/// Notification rules can only be applied in a room where our own membership is
/// known locally. Servers normally send it along with the room; some do not, and then
/// neither notifications nor notification counts work there. This fetches the member
/// list of such rooms, once per room, a few at a time. Dropping it stops the fetches.
#[derive(Default)]
pub struct OwnMembers {
    tried: std::collections::HashSet<matrix_sdk::ruma::OwnedRoomId>,
    /// Rooms where our membership is known; nothing to do there.
    fine: std::collections::HashSet<matrix_sdk::ruma::OwnedRoomId>,
    tasks: tokio::task::JoinSet<()>,
}

impl OwnMembers {
    const AT_ONCE: usize = 2;
    /// A safety net for odd servers, not a crawler: after this many rooms it stops.
    const MOST: usize = 500;

    /// Resolves when one of the running fetches ends, so the next rooms can start
    /// without waiting for the room list to change. Never resolves while idle.
    pub async fn finished(&mut self) {
        if self.tasks.is_empty() {
            std::future::pending::<()>().await;
        }
        let _ = self.tasks.join_next().await;
    }

    pub async fn ensure(
        &mut self,
        rooms: &matrix_sdk_ui::eyeball_im::Vector<matrix_sdk_ui::room_list_service::RoomListItem>,
    ) {
        while self.tasks.try_join_next().is_some() {}
        for room in rooms {
            if self.tasks.len() >= Self::AT_ONCE || self.tried.len() >= Self::MOST {
                return;
            }
            if room.state() != matrix_sdk::RoomState::Joined
                || room.is_space()
                || self.tried.contains(room.room_id())
                || self.fine.contains(room.room_id())
            {
                continue;
            }
            // Only rooms that really need a fetch count against the limit.
            let known = room.get_member_no_sync(room.own_user_id()).await;
            if matches!(known, Ok(Some(_))) {
                self.fine.insert(room.room_id().to_owned());
                continue;
            }
            self.tried.insert(room.room_id().to_owned());
            let room = room.clone();
            self.tasks.spawn(async move {
                if let Err(err) = room.sync_members().await {
                    tracing::debug!(%err, "could not fetch members for notifications");
                }
            });
        }
    }
}

/// One line saying what happened.
fn describe(event: &AnySyncTimelineEvent) -> String {
    let AnySyncTimelineEvent::MessageLike(event) = event else {
        return "New activity".to_owned();
    };
    match event {
        AnySyncMessageLikeEvent::RoomMessage(message) => match message.as_original() {
            Some(message) => match &message.content.msgtype {
                MessageType::Image(_) => "sent a picture".to_owned(),
                MessageType::Video(_) => "sent a video".to_owned(),
                MessageType::Audio(_) => "sent an audio message".to_owned(),
                MessageType::File(file) => format!("sent a file: {}", file.filename()),
                other => other.body().to_owned(),
            },
            None => "New activity".to_owned(),
        },
        AnySyncMessageLikeEvent::RoomEncrypted(_) => "Encrypted message".to_owned(),
        AnySyncMessageLikeEvent::Sticker(_) => "sent a sticker".to_owned(),
        _ => "New activity".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::config::SyncSettings;
    use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;

    use super::*;
    use crate::matrix::{MatrixCmd, spawn};
    use crate::paths::Paths;

    /// A message from someone else in a joined room produces one notice with the
    /// sender's name and the text; our own messages produce none.
    /// `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=... cargo test -- --ignored`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn a_message_from_someone_else_is_announced() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
            .with_test_writer()
            .try_init();
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let username = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");

        let root = std::env::temp_dir().join(format!("matrix-rat-notify-{}", std::process::id()));
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
            username,
            password,
        });
        // Wait until the room list is there, so the sync is running.
        let synced = async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    // The list arrives in batches; wait for a room we have joined.
                    MatrixEvent::Rooms(rooms)
                        if rooms
                            .iter()
                            .any(|room| room.category != crate::matrix::RoomCategory::Invite) =>
                    {
                        return rooms;
                    }
                    _ => {}
                }
            }
        };
        let listed = tokio::time::timeout(Duration::from_secs(60), synced)
            .await
            .expect("timed out waiting for rooms");
        let shared_counts: std::collections::HashMap<String, u64> = listed
            .iter()
            .map(|room| (room.id.clone(), room.notifications))
            .collect();
        // Not the invitations: nothing arrives in a room we have not joined.
        let shared: Vec<String> = listed
            .into_iter()
            .filter(|room| room.category != crate::matrix::RoomCategory::Invite)
            .map(|room| room.id)
            .collect();

        // Bob writes into a room both are in.
        let bob = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .unwrap();
        bob.matrix_auth()
            .login_username("bob", "bobpass")
            .initial_device_display_name("matrix-rat notify test")
            .send()
            .await
            .unwrap();
        bob.sync_once(SyncSettings::default()).await.unwrap();
        // A room both are in: Bob joins (or is already in) the first one that lets him.
        let mut room = None;
        for id in &shared {
            let id = <&matrix_sdk::ruma::RoomId>::try_from(id.as_str()).unwrap();
            if let Ok(joined) = bob.join_room_by_id(id).await {
                room = Some(joined);
                break;
            }
        }
        let room = room.expect("bob can join a room of the test user");
        // Give the worker a moment to learn its own membership where the server did
        // not send it.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let text = format!("ping {}", std::process::id());
        room.send(RoomMessageEventContent::text_plain(&text))
            .await
            .unwrap();

        // Either the SDK announces the message itself (it can when it knows our own
        // membership of the room), or the server's notification count for the room
        // goes up, which the UI turns into a notification.
        let before = shared_counts
            .get(room.room_id().as_str())
            .copied()
            .unwrap_or(0);
        let wait = async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::Notice(notice) if notice.body == text => {
                        assert_eq!(notice.room_id, room.room_id().as_str());
                        assert!(!format!("{notice:?}").contains("ping"));
                        return "detailed";
                    }
                    MatrixEvent::Rooms(rooms) => {
                        let now = rooms
                            .iter()
                            .find(|listed| listed.id == room.room_id().as_str())
                            .map_or(0, |listed| listed.notifications);
                        if now > before {
                            return "count";
                        }
                    }
                    _ => {}
                }
            }
        };
        let how = tokio::time::timeout(Duration::from_secs(30), wait)
            .await
            .expect("timed out waiting for any sign of the new message");
        eprintln!("announced by: {how}");
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = bob.matrix_auth().logout().await;
        let _ = std::fs::remove_dir_all(root);
    }
}
