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
use super::manage::failed;
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
    current: Option<(u64, Arc<Mutex<RoomDirectorySearch>>)>,
    server_label: Option<String>,
    task: Option<JoinHandle<()>>,
}

impl DirectorySearcher {
    pub(super) fn new(client: Client, events: mpsc::UnboundedSender<MatrixEvent>) -> Self {
        Self {
            client,
            events,
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
        let filter = clean_query(&query);

        self.task = Some(tokio::spawn(async move {
            let outcome = tokio::time::timeout(DIRECTORY_TIMEOUT, async move {
                let mut guard = search.lock().await;
                guard
                    .search(filter, DIRECTORY_PAGE_SIZE, via_server)
                    .await?;
                let (items, _) = guard.results();
                let rooms: Vec<PublicRoom> =
                    items.iter().map(|d| map_public_room(&client, d)).collect();
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
                    .map(|d| map_public_room(&client, d))
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

fn map_public_room(client: &Client, desc: &RoomDescription) -> PublicRoom {
    let joined = client
        .get_room(&desc.room_id)
        .is_some_and(|r| r.state() == RoomState::Joined);
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

    let local_room = client.get_room(&preview.room_id);
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
            Some(s) => failed(&format!("search {s}'s room directory"), err),
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
}
