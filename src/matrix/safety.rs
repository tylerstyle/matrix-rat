//! People & safety helpers: ignored users, content reporting, and user directory search.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use matrix_sdk::Client;
use matrix_sdk::ruma::events::ignored_user_list::{IgnoredUser, IgnoredUserListEventContent};
use matrix_sdk::ruma::{EventId, MxcUri, OwnedUserId, RoomId, UserId};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::{MatrixEvent, UserMatch};
use crate::text::{is_blank, sanitize_line};

pub(super) type IgnoredSet = Arc<HashSet<OwnedUserId>>;

const USER_SEARCH_LIMIT: u64 = 10;
const LOCAL_IGNORE_GRACE: Duration = Duration::from_secs(10);

/// Accepts only well-formed `mxc://server/id` URIs without whitespace or control chars.
pub(super) fn safe_mxc(uri: &MxcUri) -> Option<String> {
    let valid = uri
        .parts()
        .is_ok_and(|(server, id)| !matches!(server.as_str(), "" | "." | "..") && !id.is_empty());
    if !valid {
        return None;
    }
    let text = uri.to_string();
    (!text.contains(|c: char| c.is_control() || c.is_whitespace())).then_some(text)
}

pub(super) fn is_ignored_in(ignored: &watch::Receiver<IgnoredSet>, user: &UserId) -> bool {
    ignored.borrow().contains(user)
}

enum IgnoreOp {
    Set { user_id: String, ignore: bool },
}

/// Keeps the session's `m.ignored_user_list` cached for all worker subsystems and
/// serializes local ignore/unignore writes so rapid edits do not clobber one another
/// before `/sync` reflects them back.
pub(super) struct IgnoreTracker {
    rx: watch::Receiver<IgnoredSet>,
    ops: UnboundedSender<IgnoreOp>,
    task: JoinHandle<()>,
}

async fn read_ignored_content(client: &Client) -> matrix_sdk::Result<IgnoredUserListEventContent> {
    let Some(raw) = client
        .account()
        .account_data::<IgnoredUserListEventContent>()
        .await?
    else {
        return Ok(IgnoredUserListEventContent::default());
    };
    Ok(raw.deserialize().unwrap_or_else(|err| {
        tracing::debug!(?err, "could not deserialize ignored user list");
        IgnoredUserListEventContent::default()
    }))
}

impl IgnoreTracker {
    pub(super) async fn start(client: Client, events: UnboundedSender<MatrixEvent>) -> Self {
        let initial = match read_ignored_content(&client).await {
            Ok(content) => content,
            Err(err) => {
                tracing::debug!(?err, "could not read initial ignored user list");
                IgnoredUserListEventContent::default()
            }
        };
        let initial_set: HashSet<OwnedUserId> = initial.ignored_users.keys().cloned().collect();
        let _ = events.send(MatrixEvent::IgnoredUsers(sorted_ids(&initial_set)));
        let (tx, rx) = watch::channel(Arc::new(initial_set));
        let (ops, mut op_rx) = unbounded_channel::<IgnoreOp>();

        let task = tokio::spawn(async move {
            let mut stream = client.subscribe_to_ignore_user_list_changes();
            let mut local_content: Option<(Instant, IgnoredUserListEventContent)> = None;

            loop {
                tokio::select! {
                    op = op_rx.recv() => {
                        let Some(op) = op else { break };
                        let IgnoreOp::Set { user_id, ignore } = op;
                        handle_ignore_op(
                            &client,
                            &events,
                            &tx,
                            &mut local_content,
                            user_id,
                            ignore,
                        )
                        .await;
                    }
                    sync_list = stream.next() => {
                        let Some(raw_ids) = sync_list else { break };
                        let synced: HashSet<OwnedUserId> = raw_ids
                            .into_iter()
                            .filter_map(|id| OwnedUserId::try_from(id.as_str()).ok())
                            .collect();
                        if let Some((at, ref pending)) = local_content {
                            let pending_set: HashSet<OwnedUserId> =
                                pending.ignored_users.keys().cloned().collect();
                            if synced != pending_set && at.elapsed() < LOCAL_IGNORE_GRACE {
                                // An in-flight `/sync` from before our PUT: wait for the echo.
                                continue;
                            }
                            local_content = None;
                        }
                        if **tx.borrow() != synced {
                            let ids = sorted_ids(&synced);
                            let _ = tx.send(Arc::new(synced));
                            let _ = events.send(MatrixEvent::IgnoredUsers(ids));
                        }
                    }
                }
            }
        });

        Self { rx, ops, task }
    }

    pub(super) fn subscribe(&self) -> watch::Receiver<IgnoredSet> {
        self.rx.clone()
    }

    pub(super) fn set(&self, user_id: String, ignore: bool) {
        let _ = self.ops.send(IgnoreOp::Set { user_id, ignore });
    }

    pub(super) fn stop(self) {
        drop(self);
    }
}

impl Drop for IgnoreTracker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle_ignore_op(
    client: &Client,
    events: &UnboundedSender<MatrixEvent>,
    tx: &watch::Sender<IgnoredSet>,
    local_content: &mut Option<(Instant, IgnoredUserListEventContent)>,
    user_id: String,
    ignore: bool,
) {
    let Ok(parsed) = OwnedUserId::try_from(user_id.as_str()) else {
        let _ = events.send(MatrixEvent::IgnoreResult {
            user_id,
            ignored: ignore,
            result: Err("Invalid Matrix user ID".to_owned()),
        });
        return;
    };
    if ignore && client.user_id() == Some(parsed.as_ref()) {
        let _ = events.send(MatrixEvent::IgnoreResult {
            user_id,
            ignored: true,
            result: Err("You cannot ignore your own account".to_owned()),
        });
        return;
    }

    let mut content = if let Some((_, pending)) = local_content.as_ref() {
        pending.clone()
    } else {
        match read_ignored_content(client).await {
            Ok(existing) => existing,
            Err(err) => {
                let _ = events.send(MatrixEvent::IgnoreResult {
                    user_id,
                    ignored: ignore,
                    result: Err(format!("{err:#}")),
                });
                return;
            }
        }
    };

    if ignore {
        content
            .ignored_users
            .insert(parsed.clone(), IgnoredUser::new());
    } else {
        content.ignored_users.remove(&parsed);
    }

    match client.account().set_account_data(content.clone()).await {
        Ok(_) => {
            let next: HashSet<OwnedUserId> = content.ignored_users.keys().cloned().collect();
            let ids = sorted_ids(&next);
            *local_content = Some((Instant::now(), content));
            let _ = tx.send(Arc::new(next));
            let _ = events.send(MatrixEvent::IgnoredUsers(ids));
            let _ = events.send(MatrixEvent::IgnoreResult {
                user_id,
                ignored: ignore,
                result: Ok(()),
            });
        }
        Err(err) => {
            let _ = events.send(MatrixEvent::IgnoreResult {
                user_id,
                ignored: ignore,
                result: Err(format!("{err:#}")),
            });
        }
    }
}

fn sorted_ids(users: &HashSet<OwnedUserId>) -> Vec<String> {
    let mut ids: Vec<String> = users.iter().map(|id| id.to_string()).collect();
    ids.sort();
    ids
}

pub(super) async fn report_content(
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    room_id: String,
    event_id: String,
    reason: Option<String>,
) {
    let outcome = async {
        let room_id = RoomId::parse(&room_id).map_err(|err| format!("{err:#}"))?;
        let event_id = EventId::parse(&event_id).map_err(|err| format!("{err:#}"))?;
        let room = client
            .get_room(&room_id)
            .ok_or_else(|| "Room is no longer available".to_owned())?;
        let clean_reason = reason.and_then(|text| {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        });
        room.report_content(event_id, clean_reason)
            .await
            .map(|_| ())
            .map_err(|err| format!("{err:#}"))
    }
    .await;
    let _ = events.send(MatrixEvent::ReportResult(outcome));
}

pub(super) struct UserSearcher {
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    ignored: watch::Receiver<IgnoredSet>,
    task: Option<JoinHandle<()>>,
}

impl UserSearcher {
    pub(super) fn new(
        client: Client,
        events: UnboundedSender<MatrixEvent>,
        ignored: watch::Receiver<IgnoredSet>,
    ) -> Self {
        Self {
            client,
            events,
            ignored,
            task: None,
        }
    }

    pub(super) fn start(&mut self, generation: u64, query: String) {
        self.cancel();
        let trimmed = query.trim().to_owned();
        if trimmed.is_empty() {
            let _ = self.events.send(MatrixEvent::UserSearchResults {
                generation,
                results: Vec::new(),
            });
            return;
        }
        let client = self.client.clone();
        let events = self.events.clone();
        let ignored = self.ignored.clone();
        self.task = Some(tokio::spawn(async move {
            let results = match client.search_users(&trimmed, USER_SEARCH_LIMIT).await {
                Ok(response) => response
                    .results
                    .into_iter()
                    .filter(|user| !is_ignored_in(&ignored, &user.user_id))
                    .map(|user| UserMatch {
                        user_id: user.user_id.to_string(),
                        display_name: user
                            .display_name
                            .map(|name| sanitize_line(&name))
                            .filter(|name| !is_blank(name)),
                        avatar: user.avatar_url.as_deref().and_then(safe_mxc),
                    })
                    .collect(),
                Err(err) => {
                    // User directory may be disabled on the homeserver; degrade silently.
                    tracing::debug!(?err, generation, "user directory search failed");
                    Vec::new()
                }
            };
            let _ = events.send(MatrixEvent::UserSearchResults {
                generation,
                results,
            });
        }));
    }

    pub(super) fn cancel(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    pub(super) fn stop(&mut self) {
        self.cancel();
    }
}

impl Drop for UserSearcher {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::config::SyncSettings;
    use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
    use matrix_sdk::ruma::{MxcUri, RoomId};

    use super::*;
    use crate::matrix::{ManageCmd, ManageEvent, MatrixCmd, RoomCategory, spawn};
    use crate::paths::Paths;

    #[test]
    fn safe_mxc_validates_scheme_authority_media_and_control_chars() {
        fn uri(s: &str) -> &MxcUri {
            s.into()
        }
        assert_eq!(
            safe_mxc(uri("mxc://example.org/abc123")),
            Some("mxc://example.org/abc123".to_owned())
        );
        assert_eq!(safe_mxc(uri("mxc://example.org/")), None);
        assert_eq!(safe_mxc(uri("https://example.org/abc")), None);
        assert_eq!(safe_mxc(uri("mxc://../abc")), None);
        assert_eq!(safe_mxc(uri("mxc://example.org/abc\ndef")), None);
    }

    async fn login_worker(
        prefix: &str,
        homeserver: &str,
        username: &str,
        password: &str,
    ) -> (
        std::path::PathBuf,
        crate::matrix::MatrixHandle,
        tokio::sync::mpsc::UnboundedReceiver<MatrixEvent>,
        String,
    ) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
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
            homeserver: homeserver.to_owned(),
            username: username.to_owned(),
            password: password.to_owned(),
        });
        let user_id = loop {
            match tokio::time::timeout(Duration::from_secs(60), events.recv())
                .await
                .expect("timed out logging in")
                .expect("worker stopped")
            {
                MatrixEvent::LoggedIn { user_id } => break user_id,
                MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                _ => {}
            }
        };
        (root, handle, events, user_id)
    }

    async fn wait_for_ignored(
        events: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixEvent>,
        target: &str,
        present: bool,
    ) {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::IgnoredUsers(list)
                        if list.iter().any(|u| u == target) == present =>
                    {
                        return;
                    }
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for IgnoredUsers sync");
    }

    async fn wait_for_room_joined(
        events: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixEvent>,
        room_id: &str,
    ) {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::Rooms(rooms)
                        if rooms
                            .iter()
                            .any(|r| r.id == room_id && r.category != RoomCategory::Invite) =>
                    {
                        return;
                    }
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for room in Rooms list");
    }

    async fn wait_for_timeline_contains(
        events: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixEvent>,
        room_id: &str,
        needle: &str,
        present: bool,
    ) -> Option<String> {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::Timeline {
                        room_id: rid,
                        messages,
                        ..
                    } if rid == room_id => {
                        let found = messages.iter().find(|m| m.body.contains(needle)).map(|m| {
                            match &m.item {
                                Some(crate::model::ItemRef::Event(id)) => id.clone(),
                                _ => String::new(),
                            }
                        });
                        if found.is_some() == present {
                            return found;
                        }
                    }
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for timeline snapshot state")
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn ignore_syncs_across_clients_hides_and_restores_timeline_search_and_report() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
            .with_test_writer()
            .try_init();
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let username = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");

        let (_root1, h1, mut ev1, _alice_id) =
            login_worker("matrix-rat-safety1", &homeserver, &username, &password).await;
        let (_root2, h2, mut ev2, _) =
            login_worker("matrix-rat-safety2", &homeserver, &username, &password).await;
        h2.send(MatrixCmd::Previews(true));

        let bob = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .unwrap();
        bob.matrix_auth()
            .login_username("bob", "bobpass")
            .initial_device_display_name("matrix-rat safety test")
            .send()
            .await
            .unwrap();
        let bob_id = bob.user_id().unwrap().to_string();

        // Ensure clean baseline in case a previous run aborted mid-test.
        h1.send(MatrixCmd::UnignoreUser {
            user_id: bob_id.clone(),
        });
        wait_for_ignored(&mut ev1, &bob_id, false).await;
        wait_for_ignored(&mut ev2, &bob_id, false).await;

        // Clear any leftover ignore state cleanly even if the test panics mid-run.
        let h1_guard = h1.clone();
        let bob_guard = bob_id.clone();
        let outcome = tokio::spawn(async move {
        // Create a public room (so continuwuity indexes members in `/user_directory/search`)
        // and invite bob.
        let tag = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        h1.send(MatrixCmd::Manage(ManageCmd::Create {
            request: 42,
            name: format!("Safety Test {tag}"),
            topic: None,
            encrypted: false,
            alias: Some(format!("safety-test-{tag}")),
        }));
        let room_id = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match ev1.recv().await.expect("worker stopped") {
                    MatrixEvent::Manage(ManageEvent::Opened {
                        request: 42,
                        result,
                    }) => break result.expect("create room"),
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out creating room");

        wait_for_room_joined(&mut ev1, &room_id).await;
        wait_for_room_joined(&mut ev2, &room_id).await;

        h1.send(MatrixCmd::Manage(ManageCmd::Invite {
            room_id: room_id.clone(),
            user_id: bob_id.clone(),
        }));
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match ev1.recv().await.expect("worker stopped") {
                    MatrixEvent::Manage(ManageEvent::Done {
                        change: crate::matrix::Change::Invite,
                        result,
                        ..
                    }) => {
                        result.expect("invite bob");
                        break;
                    }
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out inviting bob");

        let parsed_room = RoomId::parse(&room_id).unwrap();
        let bob_room = bob.join_room_by_id(&parsed_room).await.unwrap();

        // Second room never opened on h1: set up before ignoring bob so the invite/join
        // succeeds on homeservers with MSC4406 ignore rules; used later for `/context`
        // OpenEvent rejection, SuppressedNotice, and MarkRoomsRead.
        h1.send(MatrixCmd::Manage(ManageCmd::Create {
            request: 43,
            name: format!("Safety Unread {tag}"),
            topic: None,
            encrypted: false,
            alias: None,
        }));
        let room2_id = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match ev1.recv().await.expect("worker stopped") {
                    MatrixEvent::Manage(ManageEvent::Opened {
                        request: 43,
                        result,
                    }) => break result.expect("create room2"),
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out creating room2");
        wait_for_room_joined(&mut ev1, &room2_id).await;
        h1.send(MatrixCmd::Manage(ManageCmd::Invite {
            room_id: room2_id.clone(),
            user_id: bob_id.clone(),
        }));
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match ev1.recv().await.expect("worker stopped") {
                    MatrixEvent::Manage(ManageEvent::Done {
                        change: crate::matrix::Change::Invite,
                        result,
                        ..
                    }) => {
                        result.expect("invite bob to room2");
                        break;
                    }
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out inviting bob to room2");
        let parsed_room2 = RoomId::parse(&room2_id).unwrap();
        let bob_room2 = bob.join_room_by_id(&parsed_room2).await.unwrap();
        bob.sync_once(SyncSettings::default()).await.unwrap();

        // Verify UserDirectory search finds bob once both users share a room.
        h1.send(MatrixCmd::SearchUsers {
            generation: 1,
            query: "bob".into(),
        });
        let found_bob = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match ev1.recv().await.expect("worker stopped") {
                    MatrixEvent::UserSearchResults {
                        generation: 1,
                        results,
                    } => break results.iter().any(|u| u.user_id == bob_id),
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for UserSearchResults");
        assert!(found_bob, "expected {bob_id} in directory search");

        h1.send(MatrixCmd::OpenRoom {
            room_id: room_id.clone(),
            generation: 1,
        });
        h2.send(MatrixCmd::OpenRoom {
            room_id: room_id.clone(),
            generation: 1,
        });

        let msg_text = format!("safety ping {}", std::process::id());
        bob_room
            .send(RoomMessageEventContent::text_plain(&msg_text))
            .await
            .unwrap();

        let event_id = wait_for_timeline_contains(&mut ev1, &room_id, &msg_text, true)
            .await
            .expect("expected bob event id");
        wait_for_timeline_contains(&mut ev2, &room_id, &msg_text, true).await;

        // Report bob's message to the homeserver.
        h1.send(MatrixCmd::ReportContent {
            room_id: room_id.clone(),
            event_id: event_id.clone(),
            reason: Some("safety integration test".into()),
        });
        let report_res = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match ev1.recv().await.expect("worker stopped") {
                    MatrixEvent::ReportResult(res) => break res,
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for ReportResult");
        report_res.expect("homeserver should accept content report");

        // Ignoring bob on client 1 syncs to client 2, hides bob's message on both
        // timelines, hides bob from room previews, and omits bob from user search.
        h1.send(MatrixCmd::IgnoreUser {
            user_id: bob_id.clone(),
        });
        wait_for_ignored(&mut ev1, &bob_id, true).await;
        wait_for_ignored(&mut ev2, &bob_id, true).await;
        wait_for_timeline_contains(&mut ev1, &room_id, &msg_text, false).await;
        wait_for_timeline_contains(&mut ev2, &room_id, &msg_text, false).await;

        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match ev2.recv().await.expect("worker stopped") {
                    MatrixEvent::Rooms(rooms)
                        if rooms.iter().find(|r| r.id == room_id).is_some_and(|r| {
                            !r.preview.as_deref().is_some_and(|p| p.contains(&msg_text))
                        }) =>
                    {
                        break;
                    }
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for preview suppression");

        h1.send(MatrixCmd::SearchUsers {
            generation: 2,
            query: "bob".into(),
        });
        let still_shown = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match ev1.recv().await.expect("worker stopped") {
                    MatrixEvent::UserSearchResults {
                        generation: 2,
                        results,
                    } => break results.iter().any(|u| u.user_id == bob_id),
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for filtered UserSearchResults");
        assert!(
            !still_shown,
            "ignored user should be omitted from directory search"
        );

        // Jumping to an ignored user's message rejects cleanly via JumpFailed (warm cache).
        h1.send(MatrixCmd::OpenEvent {
            room_id: room_id.clone(),
            event_id: event_id.clone(),
            thread_root: None,
            generation: 99,
        });
        let jump_reason = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match ev1.recv().await.expect("worker stopped") {
                    MatrixEvent::JumpFailed {
                        generation: 99,
                        reason,
                    } => break reason,
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for warm-cache JumpFailed");
        assert!(jump_reason.contains("ignored"), "{jump_reason}");

        // Ignored bob speaks in unopened room2: Continuwuity's MSC4406 may withhold the
        // event or notification from sync altogether, or emit `SuppressedNotice` — in
        // either case no `MatrixEvent::Notice` may be emitted for room2.
        let bob_sent = bob_room2
            .send(RoomMessageEventContent::text_plain("unread while ignored"))
            .await
            .unwrap();
        let bob_ev2 = bob_sent.response.event_id.to_string();

        let _ = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(ev) = ev1.recv().await {
                match ev {
                    MatrixEvent::Notice(n) if n.room_id == room2_id => {
                        panic!("must not emit Notice for ignored sender: {n:?}");
                    }
                    MatrixEvent::SuppressedNotice { room_id: rid } if rid == room2_id => {
                        break;
                    }
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await;

        // `/context` OpenEvent on unopened room2 rejects with "ignored" without opening.
        h1.send(MatrixCmd::OpenEvent {
            room_id: room2_id.clone(),
            event_id: bob_ev2,
            thread_root: None,
            generation: 100,
        });
        let ctx_reason = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match ev1.recv().await.expect("worker stopped") {
                    MatrixEvent::Notice(n) if n.room_id == room2_id => {
                        panic!("must not emit Notice for ignored sender: {n:?}");
                    }
                    MatrixEvent::JumpFailed {
                        generation: 100,
                        reason,
                    } => break reason,
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for /context JumpFailed");
        assert!(ctx_reason.contains("ignored"), "{ctx_reason}");

        // Mark room2 unread then run MarkRoomsRead: exercises fallback read marking even
        // when room2 holds no visible non-ignored messages in its timeline.
        h1.send(MatrixCmd::SetUnread {
            room_id: room2_id.clone(),
            unread: true,
        });
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match ev1.recv().await.expect("worker stopped") {
                    MatrixEvent::Rooms(list)
                        if list
                            .iter()
                            .find(|r| r.id == room2_id)
                            .is_some_and(|r| r.marked_unread) =>
                    {
                        break;
                    }
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for room2 marked_unread");
        h1.send(MatrixCmd::MarkRoomsRead {
            room_ids: vec![room2_id.clone()],
            public: false,
        });
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match ev1.recv().await.expect("worker stopped") {
                    MatrixEvent::ActionFailed(err) => panic!("MarkRoomsRead failed: {err}"),
                    MatrixEvent::Rooms(list)
                        if list
                            .iter()
                            .find(|r| r.id == room2_id)
                            .is_some_and(|r| !r.marked_unread && !r.unread && r.notifications == 0) =>
                    {
                        break;
                    }
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for room2 unread count to clear");

        let _ = bob_room2.leave().await;
        h1.send(MatrixCmd::Manage(ManageCmd::Leave {
            room_id: room2_id.clone(),
        }));
        let _ = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(ev) = ev1.recv().await {
                if matches!(ev, MatrixEvent::Manage(ManageEvent::Left { room_id: rid, .. }) if rid == room2_id) {
                    break;
                }
            }
        })
        .await;

        // Unignoring bob syncs and restores bob's message in the live timeline.
        h1.send(MatrixCmd::UnignoreUser {
            user_id: bob_id.clone(),
        });
        wait_for_ignored(&mut ev1, &bob_id, false).await;
        wait_for_ignored(&mut ev2, &bob_id, false).await;
        wait_for_timeline_contains(&mut ev1, &room_id, &msg_text, true).await;

        // Clean up test room and workers so alice's account remains clean.
        let _ = bob_room.leave().await;
        let _ = bob.matrix_auth().logout().await;
        h1.send(MatrixCmd::Manage(ManageCmd::Leave {
            room_id: room_id.clone(),
        }));
        let _ = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(ev) = ev1.recv().await {
                if matches!(ev, MatrixEvent::Manage(ManageEvent::Left { room_id: rid, .. }) if rid == room_id) {
                    break;
                }
            }
        })
        .await;
        h1.send(MatrixCmd::Shutdown);
        h2.send(MatrixCmd::Shutdown);
        drop(h1);
        drop(h2);
        tokio::time::timeout(Duration::from_secs(15), async {
            while ev1.recv().await.is_some() {}
        })
        .await
        .expect("timed out shutting down worker 1");
        tokio::time::timeout(Duration::from_secs(15), async {
            while ev2.recv().await.is_some() {}
        })
        .await
        .expect("timed out shutting down worker 2");
        let _ = std::fs::remove_dir_all(_root1);
        let _ = std::fs::remove_dir_all(_root2);
        })
        .await;
        if let Err(err) = outcome {
            h1_guard.send(MatrixCmd::UnignoreUser { user_id: bob_guard });
            tokio::time::sleep(Duration::from_secs(1)).await;
            h1_guard.send(MatrixCmd::Shutdown);
            std::panic::resume_unwind(err.into_panic());
        }
    }
}
