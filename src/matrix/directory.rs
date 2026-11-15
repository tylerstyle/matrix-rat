//! Public room directory search, room preview lookup, and knock requests.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use matrix_sdk::room_directory_search::{RoomDescription, RoomDirectorySearch};
use matrix_sdk::ruma::api::client::room::get_summary;
use matrix_sdk::ruma::api::error::ErrorKind;
use matrix_sdk::ruma::room::{JoinRuleKind, JoinRuleSummary};
use matrix_sdk::ruma::{OwnedRoomOrAliasId, OwnedServerName, RoomOrAliasId, ServerName};
use matrix_sdk::{Client, EncryptionState, RoomState};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use super::MatrixEvent;
use super::left::LeftRooms;
use super::manage::{failed, failed_saying};
use super::safety::safe_mxc;
use crate::text::{is_blank, sanitize, sanitize_line};

const DIRECTORY_TIMEOUT: Duration = Duration::from_secs(15);
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(12);
const KNOCK_TIMEOUT: Duration = Duration::from_secs(20);
const DIRECTORY_PAGE_SIZE: u32 = 25;
const TOPIC_MAX_LEN: usize = 240;

/// One entry returned from a public room directory search.
#[derive(Clone, PartialEq, Eq)]
pub struct PublicRoom {
    pub room_id: String,
    pub name: Option<String>,
    pub alias: Option<String>,
    pub topic: Option<String>,
    pub avatar: Option<String>,
    pub joined_members: u64,
    pub world_readable: bool,
    pub knock: bool,
    pub joined: bool,
}

impl fmt::Debug for PublicRoom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublicRoom")
            .field("has_name", &self.name.is_some())
            .field("has_alias", &self.alias.is_some())
            .field("has_topic", &self.topic.is_some())
            .field("has_avatar", &self.avatar.is_some())
            .field("joined_members", &self.joined_members)
            .field("world_readable", &self.world_readable)
            .field("knock", &self.knock)
            .field("joined", &self.joined)
            .finish_non_exhaustive()
    }
}

/// One search or pagination batch from the public room directory.
#[derive(Clone, PartialEq, Eq)]
pub struct DirectoryPage {
    pub generation: u64,
    pub append: bool,
    pub rooms: Vec<PublicRoom>,
    pub has_more: bool,
}

impl fmt::Debug for DirectoryPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectoryPage")
            .field("generation", &self.generation)
            .field("append", &self.append)
            .field("count", &self.rooms.len())
            .field("has_more", &self.has_more)
            .finish()
    }
}

/// Access rule summarised on an unjoined room preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreviewJoinRule {
    Public,
    Knock,
    Invite,
    Restricted,
    KnockRestricted,
    Private,
    #[default]
    Unknown,
}

impl PreviewJoinRule {
    pub fn label(self) -> &'static str {
        match self {
            Self::Public => "anyone can join",
            Self::Knock => "ask to join (approval required)",
            Self::Invite => "invite only",
            Self::Restricted => "restricted to space members",
            Self::KnockRestricted => "space member or ask to join",
            Self::Private => "private",
            Self::Unknown => "unknown",
        }
    }

    pub fn can_knock(self) -> bool {
        matches!(self, Self::Knock | Self::KnockRestricted | Self::Unknown)
    }

    pub fn can_join(self) -> bool {
        matches!(
            self,
            Self::Public | Self::Restricted | Self::KnockRestricted | Self::Unknown
        )
    }
}

/// Local account membership in a previewed room, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreviewMembership {
    #[default]
    None,
    Joined,
    Invited,
    Knocked,
    Banned,
}

/// Summary details fetched before joining or knocking a room.
#[derive(Clone, PartialEq, Eq)]
pub struct RoomPreview {
    pub room_id: String,
    pub canonical_alias: Option<String>,
    pub name: Option<String>,
    pub topic: Option<String>,
    pub avatar: Option<String>,
    pub joined_members: u64,
    pub join_rule: PreviewJoinRule,
    pub encrypted: Option<bool>,
    pub world_readable: bool,
    pub membership: PreviewMembership,
    pub via: Vec<String>,
}

impl fmt::Debug for RoomPreview {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoomPreview")
            .field("has_name", &self.name.is_some())
            .field("has_alias", &self.canonical_alias.is_some())
            .field("has_topic", &self.topic.is_some())
            .field("has_avatar", &self.avatar.is_some())
            .field("joined_members", &self.joined_members)
            .field("join_rule", &self.join_rule)
            .field("encrypted", &self.encrypted)
            .field("world_readable", &self.world_readable)
            .field("membership", &self.membership)
            .field("via_count", &self.via.len())
            .finish_non_exhaustive()
    }
}

/// Single-flight debounced/cancellable search runner over [`RoomDirectorySearch`].
pub(super) struct DirectorySearcher {
    client: Client,
    events: mpsc::UnboundedSender<MatrixEvent>,
    left: LeftRooms,
    current: Option<(u64, Arc<Mutex<RoomDirectorySearch>>)>,
    server_label: Option<String>,
    task: Option<JoinHandle<()>>,
}

impl DirectorySearcher {
    pub(super) fn new(
        client: Client,
        events: mpsc::UnboundedSender<MatrixEvent>,
        left: LeftRooms,
    ) -> Self {
        Self {
            client,
            events,
            left,
            current: None,
            server_label: None,
            task: None,
        }
    }

    pub(super) fn start(&mut self, generation: u64, query: String, server: Option<String>) {
        self.cancel();

        let via_server = match validate_server(server.as_deref()) {
            Ok(server) => server,
            Err(message) => {
                let _ = self.events.send(MatrixEvent::DirectoryError {
                    generation,
                    message,
                });
                return;
            }
        };
        self.server_label = via_server.as_ref().map(|s| s.to_string());

        let search = Arc::new(Mutex::new(RoomDirectorySearch::new(self.client.clone())));
        self.current = Some((generation, Arc::clone(&search)));
        let server_label = self.server_label.clone();
        let client = self.client.clone();
        let events = self.events.clone();
        let left = self.left.clone();
        let filter = clean_query(&query);

        self.task = Some(tokio::spawn(async move {
            let outcome = tokio::time::timeout(DIRECTORY_TIMEOUT, async move {
                let mut guard = search.lock().await;
                guard
                    .search(filter, DIRECTORY_PAGE_SIZE, via_server)
                    .await?;
                let (items, _) = guard.results();
                let rooms: Vec<PublicRoom> = items
                    .iter()
                    .map(|d| map_public_room(&client, &left, d))
                    .collect();
                Ok::<_, matrix_sdk::Error>(DirectoryPage {
                    generation,
                    append: false,
                    rooms,
                    has_more: !guard.is_at_last_page(),
                })
            })
            .await;

            send_directory_outcome(&events, generation, server_label.as_deref(), outcome);
        }));
    }

    pub(super) fn next_page(&mut self, generation: u64) {
        if self.task.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        let Some((cur_gen, search)) = &self.current else {
            return;
        };
        if *cur_gen != generation {
            return;
        }
        let search = Arc::clone(search);
        let server_label = self.server_label.clone();
        let client = self.client.clone();
        let events = self.events.clone();
        let left = self.left.clone();

        self.task = Some(tokio::spawn(async move {
            let outcome = tokio::time::timeout(DIRECTORY_TIMEOUT, async move {
                let mut guard = search.lock().await;
                if guard.is_at_last_page() {
                    return Ok::<_, matrix_sdk::Error>(DirectoryPage {
                        generation,
                        append: true,
                        rooms: Vec::new(),
                        has_more: false,
                    });
                }
                let before = guard.results().0.len();
                guard.next_page().await?;
                let (items, _) = guard.results();
                let rooms: Vec<PublicRoom> = items
                    .iter()
                    .skip(before)
                    .map(|d| map_public_room(&client, &left, d))
                    .collect();
                Ok(DirectoryPage {
                    generation,
                    append: true,
                    rooms,
                    has_more: !guard.is_at_last_page(),
                })
            })
            .await;

            send_directory_outcome(&events, generation, server_label.as_deref(), outcome);
        }));
    }

    pub(super) fn cancel(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.current = None;
        self.server_label = None;
    }

    pub(super) fn stop(&mut self) {
        self.cancel();
    }
}

impl Drop for DirectorySearcher {
    fn drop(&mut self) {
        self.stop();
    }
}

fn send_directory_outcome(
    events: &mpsc::UnboundedSender<MatrixEvent>,
    generation: u64,
    server: Option<&str>,
    outcome: Result<Result<DirectoryPage, matrix_sdk::Error>, tokio::time::error::Elapsed>,
) {
    let event = match outcome {
        Ok(Ok(page)) => MatrixEvent::DirectoryResults(page),
        Ok(Err(err)) => MatrixEvent::DirectoryError {
            generation,
            message: friendly_directory_error(&err, server),
        },
        Err(_) => MatrixEvent::DirectoryError {
            generation,
            message: match server {
                Some(s) => format!("{s} took too long to answer."),
                None => "The homeserver took too long to answer.".to_owned(),
            },
        },
    };
    let _ = events.send(event);
}

fn clean_query(query: &str) -> Option<String> {
    let cleaned = sanitize_line(query).trim().to_owned();
    (!cleaned.is_empty()).then_some(cleaned)
}

pub(crate) fn validate_server(raw: Option<&str>) -> Result<Option<OwnedServerName>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().any(char::is_whitespace)
        || trimmed.chars().any(char::is_control)
        || trimmed.contains('/')
        || trimmed.contains('?')
        || trimmed.contains('#')
    {
        return Err("Enter a server name like matrix.org, without a scheme or path.".to_owned());
    }
    ServerName::parse(trimmed)
        .map(Some)
        .map_err(|_| "Enter a valid server name (for example, matrix.org).".to_owned())
}

fn clean_text_opt(raw: Option<&str>, limit: usize) -> Option<String> {
    let raw = raw?;
    let clean = sanitize_line(raw).trim().to_owned();
    if is_blank(&clean) {
        return None;
    }
    let truncated: String = clean.chars().take(limit).collect();
    Some(truncated)
}

fn clean_multiline_opt(raw: Option<&str>, limit: usize) -> Option<String> {
    let raw = raw?;
    let clean = sanitize(raw).trim().to_owned();
    if is_blank(&clean) {
        return None;
    }
    let truncated: String = clean.chars().take(limit).collect();
    Some(truncated)
}

fn map_public_room(client: &Client, left: &LeftRooms, desc: &RoomDescription) -> PublicRoom {
    // A room left here can look joined while it is not (see `super::left`).
    let joined = client
        .get_room(&desc.room_id)
        .is_some_and(|r| r.state() == RoomState::Joined && !left.gone(&r));
    PublicRoom {
        room_id: desc.room_id.to_string(),
        name: clean_text_opt(desc.name.as_deref(), 120),
        alias: desc.alias.as_ref().map(|a| sanitize_line(a.as_str())),
        topic: clean_text_opt(desc.topic.as_deref(), TOPIC_MAX_LEN),
        avatar: desc.avatar_url.as_deref().and_then(safe_mxc),
        joined_members: desc.joined_members,
        world_readable: desc.is_world_readable,
        knock: matches!(
            desc.join_rule,
            JoinRuleKind::Knock | JoinRuleKind::KnockRestricted
        ),
        joined,
    }
}

fn map_join_rule_summary(summary: Option<&JoinRuleSummary>) -> PreviewJoinRule {
    match summary {
        Some(JoinRuleSummary::Public) => PreviewJoinRule::Public,
        Some(JoinRuleSummary::Knock) => PreviewJoinRule::Knock,
        Some(JoinRuleSummary::Invite) => PreviewJoinRule::Invite,
        Some(JoinRuleSummary::Restricted(_)) => PreviewJoinRule::Restricted,
        Some(JoinRuleSummary::KnockRestricted(_)) => PreviewJoinRule::KnockRestricted,
        Some(JoinRuleSummary::Private) => PreviewJoinRule::Private,
        Some(_) | None => PreviewJoinRule::Unknown,
    }
}

fn map_room_state(state: Option<RoomState>) -> PreviewMembership {
    match state {
        Some(RoomState::Joined) => PreviewMembership::Joined,
        Some(RoomState::Invited) => PreviewMembership::Invited,
        Some(RoomState::Knocked) => PreviewMembership::Knocked,
        Some(RoomState::Banned) => PreviewMembership::Banned,
        Some(RoomState::Left) | None => PreviewMembership::None,
    }
}

fn parse_via(via: &[String]) -> Vec<OwnedServerName> {
    via.iter()
        .filter_map(|s| validate_server(Some(s)).ok().flatten())
        .collect()
}

pub(super) async fn fetch_preview(
    client: &Client,
    left: &LeftRooms,
    target: &str,
    via: Vec<String>,
) -> Result<RoomPreview, String> {
    let target_id: OwnedRoomOrAliasId = <&RoomOrAliasId>::try_from(target.trim())
        .map(ToOwned::to_owned)
        .map_err(|_| "Enter a valid room address (#room:server or !id:server).".to_owned())?;

    let via_servers = parse_via(&via);
    let mut summary_via = via_servers.clone();
    if summary_via.is_empty()
        && let Some(server) = target_id.server_name()
        && client.session_meta().map(|s| s.user_id.server_name()) != Some(server)
    {
        summary_via.push(server.to_owned());
    }

    let summary_req = get_summary::v1::Request::new(target_id.clone(), summary_via);
    let preview_fut = client.get_room_preview(&target_id, via_servers.clone());
    let summary_fut = client.send(summary_req);

    let (preview_res, summary_res) = tokio::time::timeout(PREVIEW_TIMEOUT, async {
        tokio::join!(preview_fut, summary_fut)
    })
    .await
    .map_err(|_| "Timed out while loading the room preview.".to_owned())?;

    let preview = preview_res.map_err(|err| friendly_preview_error(&err))?;

    // A room left here can look joined while it is not (see `super::left`): what is
    // known of it here is stale, and the server's word counts.
    let local_room = client
        .get_room(&preview.room_id)
        .filter(|room| !left.gone(room));
    let local_state = local_room.as_ref().map(|r| r.state());
    let membership = match local_state {
        Some(RoomState::Joined | RoomState::Invited | RoomState::Knocked | RoomState::Banned) => {
            map_room_state(local_state)
        }
        _ => summary_res
            .as_ref()
            .ok()
            .and_then(|resp| resp.membership.as_ref())
            .map(|m| map_room_state(Some(RoomState::from(m))))
            .unwrap_or(PreviewMembership::None),
    };

    let encrypted = match local_room {
        Some(ref room) if room.state() == RoomState::Joined => {
            match room.latest_encryption_state().await {
                Ok(EncryptionState::Encrypted) => Some(true),
                Ok(EncryptionState::NotEncrypted) => Some(false),
                Ok(EncryptionState::Unknown) | Err(_) => None,
            }
        }
        _ => summary_res
            .as_ref()
            .ok()
            .map(|resp| resp.summary.encryption.is_some()),
    };

    Ok(RoomPreview {
        room_id: preview.room_id.to_string(),
        canonical_alias: preview.canonical_alias.map(|a| sanitize_line(a.as_str())),
        name: clean_text_opt(preview.name.as_deref(), 120),
        topic: clean_multiline_opt(preview.topic.as_deref(), 600),
        avatar: preview.avatar_url.as_deref().and_then(safe_mxc),
        joined_members: preview.num_joined_members,
        join_rule: map_join_rule_summary(preview.join_rule.as_ref()),
        encrypted,
        world_readable: preview.is_world_readable.unwrap_or(false),
        membership,
        via,
    })
}

pub(super) async fn knock(
    client: &Client,
    target: &str,
    via: Vec<String>,
    reason: Option<String>,
) -> Result<String, String> {
    let target_id: OwnedRoomOrAliasId = <&RoomOrAliasId>::try_from(target.trim())
        .map(ToOwned::to_owned)
        .map_err(|_| "Enter a valid room address (#room:server or !id:server).".to_owned())?;
    let via_servers = parse_via(&via);
    let reason = reason.and_then(|r| {
        let s = sanitize_line(&r).trim().to_owned();
        (!is_blank(&s)).then_some(s)
    });

    let room = tokio::time::timeout(KNOCK_TIMEOUT, client.knock(target_id, reason, via_servers))
        .await
        .map_err(|_| "Timed out while asking to join that room.".to_owned())?
        .map_err(|err| friendly_knock_error(&err))?;

    Ok(room.room_id().to_string())
}

fn friendly_directory_error(err: &matrix_sdk::Error, server: Option<&str>) -> String {
    match err.client_api_error_kind() {
        Some(ErrorKind::Forbidden | ErrorKind::NotFound | ErrorKind::Unrecognized) => {
            match server {
                Some(s) => format!("{s} does not share a public room directory."),
                None => "This homeserver does not share a public room directory.".to_owned(),
            }
        }
        _ => match server {
            Some(s) => failed_saying(
                "search another server's room directory",
                &format!("search {s}'s room directory"),
                err,
            ),
            None => failed("search public rooms", err),
        },
    }
}

fn friendly_preview_error(err: &matrix_sdk::Error) -> String {
    match err.client_api_error_kind() {
        Some(ErrorKind::NotFound) => "Room not found.".to_owned(),
        Some(ErrorKind::Forbidden) => "You cannot preview this private room.".to_owned(),
        Some(ErrorKind::Unrecognized) => "That server does not support room previews.".to_owned(),
        _ => failed("load room preview", err),
    }
}

fn friendly_knock_error(err: &matrix_sdk::Error) -> String {
    match err.client_api_error_kind() {
        Some(ErrorKind::Forbidden) => "You cannot ask to join this room.".to_owned(),
        Some(ErrorKind::NotFound) => "Room not found.".to_owned(),
        Some(ErrorKind::UnsupportedRoomVersion) => {
            "This room uses a version your server does not support.".to_owned()
        }
        _ => failed("ask to join", err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::left::testing::Server;
    use matrix_sdk::ruma::room_id;

    #[tokio::test]
    async fn a_room_left_here_is_not_shown_as_joined() {
        let room = room_id!("!public:localhost");
        let client = Server {
            joined: vec![room.as_str()],
            ..Server::default()
        }
        .client()
        .await;
        let found = RoomDescription {
            room_id: room.to_owned(),
            name: None,
            topic: None,
            alias: None,
            avatar_url: None,
            join_rule: JoinRuleKind::Public,
            is_world_readable: false,
            joined_members: 2,
        };
        let left = LeftRooms::default();
        assert!(map_public_room(&client, &left, &found).joined);
        let preview = fetch_preview(&client, &left, room.as_str(), Vec::new()).await;
        assert_eq!(preview.unwrap().membership, PreviewMembership::Joined);
        // Left, and looking joined still: the server's word counts (and this one does
        // not know the room).
        left.mark(room);
        assert!(!map_public_room(&client, &left, &found).joined);
        let preview = fetch_preview(&client, &left, room.as_str(), Vec::new()).await;
        assert_eq!(preview.unwrap().membership, PreviewMembership::None);
    }

    #[test]
    fn validate_server_accepts_hosts_and_ports_and_rejects_urls_or_whitespace() {
        assert_eq!(validate_server(None), Ok(None));
        assert_eq!(validate_server(Some("   ")), Ok(None));
        assert_eq!(
            validate_server(Some("matrix.org"))
                .unwrap()
                .unwrap()
                .as_str(),
            "matrix.org"
        );
        assert_eq!(
            validate_server(Some("example.org:8448"))
                .unwrap()
                .unwrap()
                .as_str(),
            "example.org:8448"
        );
        assert!(validate_server(Some("https://matrix.org")).is_err());
        assert!(validate_server(Some("matrix.org/room")).is_err());
        assert!(validate_server(Some("matrix.org extra")).is_err());
        assert!(validate_server(Some("not valid")).is_err());
    }

    #[test]
    fn debug_impls_redact_private_directory_and_preview_fields() {
        let secret_id = "!secret_room_123:example.org";
        let secret_name = "Secret Syndicate";
        let secret_alias = "#secret-club:example.org";
        let secret_topic = "Classified topic text";
        let secret_mxc = "mxc://example.org/secret-media";
        let secret_via = "federation.secret.org";

        let item = PublicRoom {
            room_id: secret_id.into(),
            name: Some(secret_name.into()),
            alias: Some(secret_alias.into()),
            topic: Some(secret_topic.into()),
            avatar: Some(secret_mxc.into()),
            joined_members: 42,
            world_readable: true,
            knock: true,
            joined: false,
        };
        let item_dbg = format!("{item:?}");
        assert!(!item_dbg.contains(secret_id));
        assert!(!item_dbg.contains(secret_name));
        assert!(!item_dbg.contains(secret_alias));
        assert!(!item_dbg.contains(secret_topic));
        assert!(!item_dbg.contains(secret_mxc));

        let page = DirectoryPage {
            generation: 7,
            append: false,
            rooms: vec![item],
            has_more: true,
        };
        let page_dbg = format!("{page:?}");
        assert!(!page_dbg.contains(secret_id));
        assert!(!page_dbg.contains(secret_name));

        let preview = RoomPreview {
            room_id: secret_id.into(),
            canonical_alias: Some(secret_alias.into()),
            name: Some(secret_name.into()),
            topic: Some(secret_topic.into()),
            avatar: Some(secret_mxc.into()),
            joined_members: 12,
            join_rule: PreviewJoinRule::Knock,
            encrypted: Some(true),
            world_readable: false,
            membership: PreviewMembership::None,
            via: vec![secret_via.into()],
        };
        let prev_dbg = format!("{preview:?}");
        assert!(!prev_dbg.contains(secret_id));
        assert!(!prev_dbg.contains(secret_name));
        assert!(!prev_dbg.contains(secret_alias));
        assert!(!prev_dbg.contains(secret_topic));
        assert!(!prev_dbg.contains(secret_mxc));
        assert!(!prev_dbg.contains(secret_via));
    }

    /// End-to-end check against a local homeserver (`MATRIX_RAT_TEST_USER` +
    /// `MATRIX_RAT_TEST_PASSWORD`, plus optional `MATRIX_RAT_TEST_USER2` / `bob`):
    /// searches the public directory, previews an unjoined room, joins it,
    /// confirms the directory marks it as joined, and leaves/cleans up afterwards.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn public_room_directory_search_preview_and_join() {
        use std::panic::AssertUnwindSafe;

        use futures::FutureExt;
        use matrix_sdk::ruma::api::client::directory::set_room_visibility::v3::Request as SetRoomVisibility;
        use matrix_sdk::ruma::api::client::room::{
            Visibility,
            create_room::v3::{Request as CreateRequest, RoomPreset},
        };

        use super::super::{ManageCmd, ManageEvent, MatrixCmd, spawn};
        use crate::paths::Paths;

        let hs = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let alice_user = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let alice_pass =
            std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let bob_user = std::env::var("MATRIX_RAT_TEST_USER2").unwrap_or_else(|_| "bob".to_owned());
        let bob_pass =
            std::env::var("MATRIX_RAT_TEST_PASSWORD2").unwrap_or_else(|_| "bobpass".to_owned());

        let alice = Client::builder().homeserver_url(&hs).build().await.unwrap();
        alice
            .matrix_auth()
            .login_username(&alice_user, &alice_pass)
            .initial_device_display_name("matrix-rat directory test")
            .send()
            .await
            .unwrap();

        let tag = format!("{}-dir", std::process::id());
        let room_name = format!("matrix-rat public dir {tag}");
        let topic = "public directory integration test".to_owned();
        let mut req = CreateRequest::new();
        req.visibility = Visibility::Public;
        req.preset = Some(RoomPreset::PublicChat);
        req.name = Some(room_name.clone());
        req.topic = Some(topic.clone());
        let alice_room = alice.create_room(req).await.unwrap();
        let room_id = alice_room.room_id().to_string();
        let _ = alice
            .send(SetRoomVisibility::new(
                alice_room.room_id().to_owned(),
                Visibility::Public,
            ))
            .await;

        let bob_root = std::env::temp_dir().join(format!("matrix-rat-dir-{tag}"));
        let _ = std::fs::remove_dir_all(&bob_root);
        let paths = Paths {
            config_dir: bob_root.join("config"),
            data_dir: bob_root.join("data"),
            log_dir: bob_root.join("log"),
            runtime_dir: bob_root.join("run"),
            profile: "it".into(),
        };
        paths.create_dirs().unwrap();
        let (bob_handle, mut bob_events) = spawn(paths);
        bob_handle.send(MatrixCmd::LoginPassword {
            homeserver: hs.clone(),
            username: bob_user,
            password: bob_pass.into(),
        });
        loop {
            match tokio::time::timeout(Duration::from_secs(60), bob_events.recv())
                .await
                .expect("timed out logging in bob")
                .expect("bob worker stopped")
            {
                MatrixEvent::LoggedIn { .. } => break,
                MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                _ => {}
            }
        }

        let outcome = AssertUnwindSafe(async {
            // 1. Search the public directory for the newly published room (`!joined`).
            let mut generation = 1_u64;
            let found = tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    bob_handle.send(MatrixCmd::SearchDirectory {
                        generation,
                        query: room_name.clone(),
                        server: None,
                    });
                    let page = loop {
                        match bob_events.recv().await.expect("worker stopped") {
                            MatrixEvent::DirectoryResults(page)
                                if page.generation == generation =>
                            {
                                break Ok(page);
                            }
                            MatrixEvent::DirectoryError {
                                generation: g,
                                message,
                            } if g == generation => break Err(message),
                            MatrixEvent::Error(err) => panic!("{err}"),
                            _ => {}
                        }
                    };
                    if let Ok(page) = page
                        && let Some(room) = page.rooms.into_iter().find(|r| r.room_id == room_id)
                    {
                        return room;
                    }
                    generation += 1;
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            })
            .await
            .expect("public room did not appear in directory");
            assert_eq!(found.name.as_deref(), Some(room_name.as_str()));
            assert_eq!(found.topic.as_deref(), Some(topic.as_str()));
            assert!(!found.joined);
            assert!(!found.knock);

            // 2. Preview it before joining.
            bob_handle.send(MatrixCmd::Manage(ManageCmd::Preview {
                request: 1,
                target: room_id.clone(),
                via: Vec::new(),
            }));
            let preview = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match bob_events.recv().await.expect("worker stopped") {
                        MatrixEvent::Manage(ManageEvent::Preview { request: 1, result }) => {
                            return result.expect("preview failed");
                        }
                        MatrixEvent::Error(err) => panic!("{err}"),
                        _ => {}
                    }
                }
            })
            .await
            .expect("timed out waiting for preview");
            assert_eq!(preview.room_id, room_id);
            assert_eq!(preview.name.as_deref(), Some(room_name.as_str()));
            assert_eq!(preview.topic.as_deref(), Some(topic.as_str()));
            assert_eq!(preview.join_rule, PreviewJoinRule::Public);
            assert_eq!(preview.membership, PreviewMembership::None);

            // 3. Join the room, then search again and verify `joined` flips to true.
            bob_handle.send(MatrixCmd::Manage(ManageCmd::Join {
                request: 2,
                target: room_id.clone(),
                via: Vec::new(),
            }));
            let joined_id = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match bob_events.recv().await.expect("worker stopped") {
                        MatrixEvent::Manage(ManageEvent::Opened { request: 2, result }) => {
                            return result.expect("join failed");
                        }
                        MatrixEvent::Error(err) => panic!("{err}"),
                        _ => {}
                    }
                }
            })
            .await
            .expect("timed out waiting for join");
            assert_eq!(joined_id, room_id);

            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    generation += 1;
                    bob_handle.send(MatrixCmd::SearchDirectory {
                        generation,
                        query: room_name.clone(),
                        server: None,
                    });
                    let page = loop {
                        match bob_events.recv().await.expect("worker stopped") {
                            MatrixEvent::DirectoryResults(page)
                                if page.generation == generation =>
                            {
                                break Some(page);
                            }
                            MatrixEvent::DirectoryError { generation: g, .. }
                                if g == generation =>
                            {
                                break None;
                            }
                            MatrixEvent::Error(err) => panic!("{err}"),
                            _ => {}
                        }
                    };
                    if let Some(page) = page
                        && page.rooms.iter().any(|r| r.room_id == room_id && r.joined)
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            })
            .await
            .expect("directory search did not mark newly joined room as joined");
        })
        .catch_unwind()
        .await;

        bob_handle.send(MatrixCmd::Manage(ManageCmd::Leave {
            room_id: room_id.clone(),
        }));
        let _ = tokio::time::timeout(Duration::from_secs(15), async {
            while let Some(ev) = bob_events.recv().await {
                if matches!(
                    ev,
                    MatrixEvent::Manage(ManageEvent::Left { room_id: id, .. }) if id == room_id
                ) {
                    break;
                }
            }
        })
        .await;
        bob_handle.send(MatrixCmd::Logout);
        let _ = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(ev) = bob_events.recv().await {
                if matches!(ev, MatrixEvent::NeedLogin { .. }) {
                    break;
                }
            }
        })
        .await;
        bob_handle.send(MatrixCmd::Shutdown);
        while bob_events.recv().await.is_some() {}
        let _ = std::fs::remove_dir_all(&bob_root);

        let _ = alice
            .send(SetRoomVisibility::new(
                alice_room.room_id().to_owned(),
                Visibility::Private,
            ))
            .await;
        let _ = alice_room.leave().await;
        let _ = alice_room.forget().await;
        let _ = alice.matrix_auth().logout().await;

        if let Err(payload) = outcome {
            std::panic::resume_unwind(payload);
        }
    }
}
