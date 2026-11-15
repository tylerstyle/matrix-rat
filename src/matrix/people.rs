//! User profiles, verification status lookup, and own-profile updates.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use matrix_sdk::ruma::UserId;
use matrix_sdk::ruma::api::client::profile::{AvatarUrl, DisplayName, get_profile};
use matrix_sdk::ruma::api::client::state::get_state_event_for_key;
use matrix_sdk::ruma::api::error::ErrorKind;
use matrix_sdk::ruma::events::StateEventType;
use matrix_sdk::ruma::events::room::member::{
    MembershipState, RoomMemberEventContent, SyncRoomMemberEvent,
};
use matrix_sdk::ruma::events::room::power_levels::UserPowerLevel;
use matrix_sdk::{Client, Room, RoomMemberships, RoomState};
use tokio::sync::{Notify, mpsc::UnboundedSender, watch};
use tokio::task::JoinSet;

use super::MatrixEvent;
use super::manage::{
    MemberRole, MemberStatus, RoomMember, build_member, failed, numeric_level, role as member_role,
    room as resolve_room,
};
use super::safety;
use crate::text::{is_blank, sanitize_line};
use crate::upload;

/// Bound profile and cross-signing queries so an unreachable peer homeserver cannot stall the UI.
const PROFILE_TIMEOUT: Duration = Duration::from_secs(15);
const KEYS_TIMEOUT: Duration = Duration::from_secs(5);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// Policy bounds for falling back to server queries in [`collect_shared_rooms`] when a room's
/// local member store is unsynced or stale:
/// - Rooms with at most `SHARED_ROOM_MAX_MEMBERS` members trigger `room.get_member` (`/members`)
///   at most once per unsynced room so future popups hit the SDK cache with zero network calls.
/// - Larger unsynced rooms and stale-synced rooms query only `m.room.member/{target}` via
///   `GET /state/...` to avoid multi-megabyte payloads and Megolm key rotation.
/// - At most `SHARED_ROOM_MAX_SYNCS` rooms are queried per popup, prioritized DMs-first then
///   smallest member count first, up to `SHARED_ROOM_CONCURRENCY` in flight and a shared
///   `SHARED_ROOM_SYNC_TIMEOUT` deadline; errored/timed-out rooms enter `SHARED_ROOM_COOLDOWN`.
const SHARED_ROOM_MAX_MEMBERS: u64 = 250;
const SHARED_ROOM_MAX_SYNCS: usize = 12;
const SHARED_ROOM_CONCURRENCY: usize = 4;
const SHARED_ROOM_SYNC_TIMEOUT: Duration = Duration::from_secs(5);
const SHARED_ROOM_COOLDOWN: Duration = Duration::from_secs(300);
const SHARED_ROOM_COOLDOWN_CAP: usize = 512;

/// Verification state of a Matrix user's cross-signing identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserVerifyStatus {
    Verified,
    Unverified,
    Violation,
    NoKeys,
    #[default]
    Unknown,
}

/// Which own-profile mutation completed or failed.
#[derive(Clone, PartialEq, Eq)]
pub enum OwnProfileAction {
    DisplayName,
    Avatar,
    RemoveAvatar,
}

/// Omits any unnecessary fields while preserving which action completed.
impl fmt::Debug for OwnProfileAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DisplayName => f.write_str("DisplayName"),
            Self::Avatar => f.write_str("Avatar"),
            Self::RemoveAvatar => f.write_str("RemoveAvatar"),
        }
    }
}

/// Current synced profile state for the logged-in account.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct OwnProfile {
    pub display_name: Option<String>,
    /// Validated `mxc://` avatar URL from the account profile, if any.
    pub avatar: Option<String>,
    /// Synced room-member display names by room ID (`Some(None)` if explicitly cleared).
    pub room_names: BTreeMap<String, Option<String>>,
}

/// Never prints display names, avatar addresses, or room IDs to logs.
impl fmt::Debug for OwnProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnProfile")
            .field("has_display_name", &self.display_name.is_some())
            .field("has_avatar", &self.avatar.is_some())
            .field("rooms", &self.room_names.len())
            .finish()
    }
}

impl OwnProfile {
    /// Effective display name for `room_id`, falling back to global profile name or None.
    pub fn effective_display_name(&self, room_id: Option<&str>) -> Option<&str> {
        if let Some(rid) = room_id
            && let Some(entry) = self.room_names.get(rid)
        {
            return entry.as_deref();
        }
        self.display_name.as_deref()
    }

    /// Name shown for self in `room_id`: room member name, else global name, else localpart.
    pub fn display_or_localpart<'a>(
        &'a self,
        room_id: Option<&str>,
        user_id: Option<&'a str>,
    ) -> Option<String> {
        if let Some(name) = self.effective_display_name(room_id) {
            return Some(name.to_owned());
        }
        user_id.map(fallback_localpart)
    }
}

/// Extracts a human-readable fallback name from a Matrix user ID (`@alice:hs` -> `alice`).
pub fn fallback_localpart(user_id: &str) -> String {
    let trimmed = sanitize_line(user_id);
    if let Some(rest) = trimmed.strip_prefix('@')
        && let Some((local, _)) = rest.split_once(':')
        && !local.is_empty()
    {
        return local.to_owned();
    }
    trimmed
}

/// Summary of one room shared with a user shown in User Info.
#[derive(Clone, PartialEq, Eq)]
pub struct UserSharedRoom {
    pub room_id: String,
    pub name: String,
    pub direct: bool,
    pub invited: bool,
}

/// Never prints room IDs or room names to logs.
impl fmt::Debug for UserSharedRoom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserSharedRoom")
            .field("direct", &self.direct)
            .field("invited", &self.invited)
            .finish_non_exhaustive()
    }
}

/// Full profile snapshot gathered for the User Info popup.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct UserProfile {
    pub user_id: String,
    pub display_name: Option<String>,
    /// Validated `mxc://` avatar URI, if any.
    pub avatar: Option<String>,
    pub verify: UserVerifyStatus,
    pub shared_rooms: Vec<UserSharedRoom>,
    /// True when fallback room member checks left candidates unqueried or errored/timed out.
    pub shared_rooms_incomplete: bool,
    /// Member details & moderation permissions in the context room, when applicable.
    pub room_member: Option<RoomMember>,
    pub room_name: Option<String>,
    pub own_is_creator: bool,
    pub own_power_level: i64,
    pub admin_count: usize,
    pub own: bool,
    /// Optional non-fatal note if remote profile lookup failed but local data was available.
    pub note: Option<String>,
}

/// Never prints user IDs, display names, avatars, or room names to logs.
impl fmt::Debug for UserProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserProfile")
            .field("has_name", &self.display_name.is_some())
            .field("has_avatar", &self.avatar.is_some())
            .field("verify", &self.verify)
            .field("shared_rooms", &self.shared_rooms.len())
            .field("shared_rooms_incomplete", &self.shared_rooms_incomplete)
            .field("has_room_member", &self.room_member.is_some())
            .field("own", &self.own)
            .finish()
    }
}

/// Tracks the latest synced or saved per-room display name for the signed-in user.
///
/// Sliding sync updates room member stores from `required_state` rather than `timeline`, so
/// reading only `room.get_member_no_sync(own)` misses live per-room display name edits until reconnect.
#[derive(Clone, Default)]
pub struct OwnRoomNames(Arc<Mutex<BTreeMap<String, Option<String>>>>);

impl OwnRoomNames {
    pub fn set(&self, room_id: &str, name: Option<String>) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(room_id.to_owned(), clean_name(name.as_deref()));
    }

    pub fn remove(&self, room_id: &str) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(room_id);
    }

    pub fn get(&self, room_id: &str) -> Option<Option<String>> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(room_id)
            .cloned()
    }
}

/// Records an incoming `m.room.member` event for the signed-in user into `own_room_names`.
pub(super) fn record_own_member_event(
    own_room_names: &OwnRoomNames,
    room_id: &str,
    ev: &SyncRoomMemberEvent,
) {
    match ev.membership() {
        MembershipState::Join | MembershipState::Knock => {
            let name = ev
                .as_original()
                .and_then(|original| clean_name(original.content.displayname.as_deref()));
            own_room_names.set(room_id, name);
        }
        MembershipState::Leave | MembershipState::Ban => {
            own_room_names.remove(room_id);
        }
        _ => {}
    }
}

/// Per-room cooldown preventing broken or timed-out rooms from stalling every subsequent popup.
#[derive(Clone, Default)]
struct MemberSyncCooldown(Arc<Mutex<HashMap<String, Instant>>>);

impl MemberSyncCooldown {
    fn is_cooling_down(&self, room_id: &str, now: Instant) -> bool {
        let Ok(guard) = self.0.lock() else {
            return false;
        };
        guard
            .get(room_id)
            .is_some_and(|at| now.saturating_duration_since(*at) < SHARED_ROOM_COOLDOWN)
    }

    fn record_failure(&self, room_id: String, now: Instant) {
        let Ok(mut guard) = self.0.lock() else {
            return;
        };
        guard.retain(|_, at| now.saturating_duration_since(*at) < SHARED_ROOM_COOLDOWN);
        if guard.len() >= SHARED_ROOM_COOLDOWN_CAP
            && !guard.contains_key(&room_id)
            && let Some(oldest) = guard
                .iter()
                .min_by_key(|(_, at)| *at)
                .map(|(k, _)| k.clone())
        {
            guard.remove(&oldest);
        }
        guard.insert(room_id, now);
    }

    fn clear(&self, room_id: &str) {
        if let Ok(mut guard) = self.0.lock() {
            guard.remove(room_id);
        }
    }
}

/// Watches own-profile sync updates and serializes own-profile edits / user lookups.
pub struct People {
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    sync_poke: Arc<Notify>,
    own_room_names: OwnRoomNames,
    shared_cooldown: MemberSyncCooldown,
    sync_task: tokio::task::JoinHandle<()>,
    tasks: JoinSet<()>,
}

impl People {
    pub fn start(
        client: Client,
        events: UnboundedSender<MatrixEvent>,
        own_refresh: Arc<watch::Sender<u64>>,
        own_room_names: OwnRoomNames,
    ) -> Self {
        let sync_poke = Arc::new(Notify::new());
        let task_client = client.clone();
        let task_events = events.clone();
        let task_poke = sync_poke.clone();
        let task_own_room_names = own_room_names.clone();
        let sync_task = tokio::spawn(async move {
            loop {
                let profile = load_own_profile(&task_client, &task_own_room_names).await;
                let _ = task_events.send(MatrixEvent::OwnProfile(profile));
                own_refresh.send_modify(|n| *n = n.wrapping_add(1));
                task_poke.notified().await;
            }
        });
        Self {
            client,
            events,
            sync_poke,
            own_room_names,
            shared_cooldown: MemberSyncCooldown::default(),
            sync_task,
            tasks: JoinSet::new(),
        }
    }

    pub fn sync_poke(&self) -> Arc<Notify> {
        self.sync_poke.clone()
    }

    pub fn fetch_user_profile(&mut self, request: u64, user_id: String, room_id: Option<String>) {
        self.reap();
        let client = self.client.clone();
        let events = self.events.clone();
        let own_room_names = self.own_room_names.clone();
        let shared_cooldown = self.shared_cooldown.clone();
        self.tasks.spawn(async move {
            let result = fetch_user_profile(
                &client,
                &user_id,
                room_id.as_deref(),
                &own_room_names,
                &shared_cooldown,
            )
            .await
            .map(Box::new);
            let _ = events.send(MatrixEvent::UserProfile { request, result });
        });
    }

    pub fn set_display_name(&mut self, name: Option<String>) {
        self.reap();
        let client = self.client.clone();
        let events = self.events.clone();
        let poke = self.sync_poke.clone();
        self.tasks.spawn(async move {
            let result = set_display_name(&client, name).await;
            if result.is_ok() {
                poke.notify_one();
            }
            let _ = events.send(MatrixEvent::OwnProfileResult {
                what: OwnProfileAction::DisplayName,
                result,
            });
        });
    }

    pub fn set_avatar(&mut self, path: PathBuf) {
        self.reap();
        let client = self.client.clone();
        let events = self.events.clone();
        let poke = self.sync_poke.clone();
        self.tasks.spawn(async move {
            let result = set_avatar(&client, &path).await;
            if result.is_ok() {
                poke.notify_one();
            }
            let _ = events.send(MatrixEvent::OwnProfileResult {
                what: OwnProfileAction::Avatar,
                result,
            });
        });
    }

    pub fn remove_avatar(&mut self) {
        self.reap();
        let client = self.client.clone();
        let events = self.events.clone();
        let poke = self.sync_poke.clone();
        self.tasks.spawn(async move {
            let result = remove_avatar(&client).await;
            if result.is_ok() {
                poke.notify_one();
            }
            let _ = events.send(MatrixEvent::OwnProfileResult {
                what: OwnProfileAction::RemoveAvatar,
                result,
            });
        });
    }

    pub fn stop(mut self) {
        self.sync_task.abort();
        self.tasks.abort_all();
    }

    fn reap(&mut self) {
        while self.tasks.try_join_next().is_some() {}
    }
}

/// Reads the live global account profile and current synced per-room display names.
pub(super) async fn load_own_profile(client: &Client, own_room_names: &OwnRoomNames) -> OwnProfile {
    let mut display_name = None;
    let mut avatar = None;
    match tokio::time::timeout(PROFILE_TIMEOUT, client.account().fetch_user_profile()).await {
        Ok(Ok(profile)) => {
            (display_name, avatar) = extract_profile(&profile);
        }
        Ok(Err(_)) => tracing::debug!("could not fetch own profile"),
        Err(_) => tracing::debug!("timed out fetching own profile"),
    }

    if display_name.is_none()
        && avatar.is_none()
        && let Ok(cached_name) = client.account().get_display_name().await
    {
        display_name = clean_name(cached_name.as_deref());
        if let Ok(Some(cached_avatar)) = client.account().get_avatar_url().await {
            avatar = safety::safe_mxc(&cached_avatar);
        }
    }

    let mut room_names = BTreeMap::new();
    if let Some(own) = client.user_id() {
        for room in client.rooms() {
            if room.state() != RoomState::Joined {
                continue;
            }
            let rid = room.room_id().to_string();
            if let Some(cached) = own_room_names.get(&rid) {
                room_names.insert(rid, cached);
            } else if let Ok(Some(member)) = room.get_member_no_sync(own).await {
                room_names.insert(rid, clean_name(member.display_name()));
            }
        }
    }

    OwnProfile {
        display_name,
        avatar,
        room_names,
    }
}

async fn fetch_user_profile(
    client: &Client,
    user_id: &str,
    room_id: Option<&str>,
    own_room_names: &OwnRoomNames,
    cooldown: &MemberSyncCooldown,
) -> Result<UserProfile, String> {
    let parsed =
        UserId::parse(user_id).map_err(|_| "That is not a valid Matrix user ID.".to_owned())?;
    let own_user = client.user_id().map(ToOwned::to_owned);
    let own = own_user.as_deref() == Some(&*parsed);

    let account = client.account();
    let profile_fut = tokio::time::timeout(PROFILE_TIMEOUT, account.fetch_user_profile_of(&parsed));
    let verify_fut = lookup_verify_status(client, &parsed);
    let (profile_res, verify) = tokio::join!(profile_fut, verify_fut);

    let mut display_name: Option<String> = None;
    let mut avatar: Option<String> = None;
    let mut remote_err: Option<String> = None;

    match profile_res {
        Ok(Ok(profile)) => {
            (display_name, avatar) = extract_profile(&profile);
        }
        Ok(Err(err)) => {
            tracing::debug!("user profile lookup failed");
            remote_err = Some(profile_error(&err));
        }
        Err(_) => {
            remote_err = Some("Profile lookup timed out.".to_owned());
        }
    }

    let ctx = match (room_id, own_user.as_deref()) {
        (Some(rid), Some(own_id)) => {
            context_room_info(client, rid, &parsed, own_id, own_room_names).await
        }
        _ => ContextRoomInfo::default(),
    };

    let (mut shared_rooms, mut shared_rooms_incomplete) =
        collect_shared_rooms(client, &parsed, room_id, cooldown).await;
    shared_rooms_incomplete |= ctx.shared_check_failed;
    if let Some(ctx_shared) = ctx.shared_room
        && !shared_rooms.iter().any(|r| r.room_id == ctx_shared.room_id)
    {
        shared_rooms.push(ctx_shared);
        sort_shared_rooms(&mut shared_rooms);
    }

    // If remote profile lacked name/avatar, fall back to member profile in context room or shared room.
    if display_name.is_none()
        && let Some(member) = &ctx.member
        && !is_blank(&member.name)
    {
        display_name = Some(member.name.clone());
    }
    if avatar.is_none() {
        avatar = match ctx.avatar_fallback {
            some @ Some(_) => some,
            None => find_member_avatar(client, &parsed, room_id).await,
        };
    }

    if let Some(err) = &remote_err
        && display_name.is_none()
        && avatar.is_none()
        && ctx.member.is_none()
        && shared_rooms.is_empty()
        && matches!(verify, UserVerifyStatus::NoKeys | UserVerifyStatus::Unknown)
    {
        return Err(err.clone());
    }

    Ok(UserProfile {
        user_id: sanitize_line(parsed.as_str()),
        display_name,
        avatar,
        verify,
        shared_rooms,
        shared_rooms_incomplete,
        room_member: ctx.member,
        room_name: ctx.room_name,
        own_is_creator: ctx.own_is_creator,
        own_power_level: ctx.own_power_level,
        admin_count: ctx.admin_count,
        own,
        note: remote_err,
    })
}

#[derive(Default)]
struct ContextRoomInfo {
    member: Option<RoomMember>,
    room_name: Option<String>,
    own_is_creator: bool,
    own_power_level: i64,
    admin_count: usize,
    shared_room: Option<UserSharedRoom>,
    shared_check_failed: bool,
    avatar_fallback: Option<String>,
}

struct ResolvedMember {
    membership: MembershipState,
    display_name: String,
    avatar_url: Option<String>,
    from_fallback: bool,
}

enum SingleMemberFetch {
    Found(ResolvedMember),
    NotFound,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FallbackMode {
    SyncMembers,
    SingleStateEvent,
}

#[derive(Clone, PartialEq, Eq)]
struct FallbackCandidate {
    room_id: String,
    direct: bool,
    members_count: u64,
    mode: FallbackMode,
}

/// Hand-written: which room stays out of logs.
impl fmt::Debug for FallbackCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FallbackCandidate")
            .field("direct", &self.direct)
            .field("members_count", &self.members_count)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

enum FallbackCheck {
    Matched(UserSharedRoom),
    NotMember,
    Failed,
}

fn member_from_local(member: &matrix_sdk::room::RoomMember, from_fallback: bool) -> ResolvedMember {
    ResolvedMember {
        membership: member.membership().clone(),
        display_name: member.name().to_owned(),
        avatar_url: member.avatar_url().and_then(safety::safe_mxc),
        from_fallback,
    }
}

async fn fetch_single_member_state(
    client: &Client,
    room: &Room,
    target: &UserId,
) -> SingleMemberFetch {
    let req = get_state_event_for_key::v3::Request::new(
        room.room_id().to_owned(),
        StateEventType::RoomMember,
        target.to_string(),
    );
    let resp = match tokio::time::timeout(KEYS_TIMEOUT, client.send(req)).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(err)) if matches!(err.client_api_error_kind(), Some(ErrorKind::NotFound)) => {
            return SingleMemberFetch::NotFound;
        }
        Ok(Err(_)) | Err(_) => return SingleMemberFetch::Failed,
    };
    let Ok(content) = resp
        .into_content()
        .deserialize_as_unchecked::<RoomMemberEventContent>()
    else {
        return SingleMemberFetch::Failed;
    };
    let display_name =
        clean_name(content.displayname.as_deref()).unwrap_or_else(|| target.localpart().to_owned());
    let avatar_url = content.avatar_url.as_deref().and_then(safety::safe_mxc);
    SingleMemberFetch::Found(ResolvedMember {
        membership: content.membership,
        display_name,
        avatar_url,
        from_fallback: true,
    })
}

async fn resolve_context_member(
    client: &Client,
    room: &Room,
    target: &UserId,
) -> (Option<ResolvedMember>, bool) {
    if let Ok(Some(member)) = room.get_member_no_sync(target).await {
        return (Some(member_from_local(&member, false)), false);
    }
    if room.state() != RoomState::Joined {
        return (None, false);
    }
    if !room.are_members_synced() {
        let count = room
            .joined_members_count()
            .saturating_add(room.invited_members_count());
        if count <= SHARED_ROOM_MAX_MEMBERS {
            return match tokio::time::timeout(KEYS_TIMEOUT, room.get_member(target)).await {
                Ok(Ok(Some(member))) => (Some(member_from_local(&member, false)), false),
                Ok(Ok(None)) => (None, false),
                Ok(Err(_)) | Err(_) => (None, true),
            };
        }
    }
    // Either `are_members_synced()` was already set before `target` joined (for example via an
    // early `OwnMembers::ensure` sync) or the room exceeds `SHARED_ROOM_MAX_MEMBERS`: fetch only
    // `m.room.member/{target}` rather than trusting stale cache state or downloading all members.
    match fetch_single_member_state(client, room, target).await {
        SingleMemberFetch::Found(member) => (Some(member), false),
        SingleMemberFetch::NotFound => (None, false),
        SingleMemberFetch::Failed => (None, true),
    }
}

async fn build_shared_room(room: &Room, state: RoomState) -> UserSharedRoom {
    let name = room_display_name(room).await;
    let direct = room.is_direct().await.unwrap_or(false);
    UserSharedRoom {
        room_id: room.room_id().to_string(),
        name,
        direct,
        invited: state == RoomState::Invited,
    }
}

fn sort_shared_rooms(rooms: &mut [UserSharedRoom]) {
    rooms.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.room_id.cmp(&b.room_id))
    });
}

fn classify_fallback(
    members_synced: bool,
    stored_joined: u64,
    joined_count: u64,
    invited_count: u64,
) -> Option<FallbackMode> {
    if !members_synced {
        let total = joined_count.saturating_add(invited_count);
        if total <= SHARED_ROOM_MAX_MEMBERS {
            Some(FallbackMode::SyncMembers)
        } else {
            Some(FallbackMode::SingleStateEvent)
        }
    } else if stored_joined < joined_count {
        Some(FallbackMode::SingleStateEvent)
    } else {
        None
    }
}

/// Returns selected fallback rooms plus whether any candidates were skipped due to cooldown or cap.
fn select_fallback_candidates(
    mut candidates: Vec<FallbackCandidate>,
    cooldown: &MemberSyncCooldown,
    now: Instant,
) -> (Vec<FallbackCandidate>, bool) {
    let total = candidates.len();
    candidates.retain(|c| !cooldown.is_cooling_down(&c.room_id, now));
    candidates.sort_by(|a, b| {
        b.direct
            .cmp(&a.direct)
            .then_with(|| a.members_count.cmp(&b.members_count))
            .then_with(|| a.room_id.cmp(&b.room_id))
    });
    candidates.truncate(SHARED_ROOM_MAX_SYNCS);
    let left_unqueried = candidates.len() < total;
    (candidates, left_unqueried)
}

async fn context_room_info(
    client: &Client,
    room_id: &str,
    target: &UserId,
    own: &UserId,
    own_room_names: &OwnRoomNames,
) -> ContextRoomInfo {
    let Ok(room) = resolve_room(client, room_id) else {
        return ContextRoomInfo::default();
    };
    let rname = room_display_name(&room).await;
    let state = room.state();
    if !matches!(state, RoomState::Joined | RoomState::Invited) {
        return ContextRoomInfo {
            room_name: Some(rname),
            ..ContextRoomInfo::default()
        };
    }
    let (resolved, failed) = resolve_context_member(client, &room, target).await;
    let Some(resolved) = resolved else {
        return ContextRoomInfo {
            room_name: Some(rname),
            shared_check_failed: failed && state == RoomState::Joined && !room.is_space(),
            ..ContextRoomInfo::default()
        };
    };
    let Some(status) = member_status(resolved.membership.clone()) else {
        return ContextRoomInfo {
            room_name: Some(rname),
            ..ContextRoomInfo::default()
        };
    };
    let levels = if state == RoomState::Joined {
        Some(room.power_levels_or_default().await)
    } else {
        None
    };
    let own_pl = levels.as_ref().map(|l| l.for_user(own));
    let own_is_creator = matches!(own_pl, Some(UserPowerLevel::Infinite));
    let own_power_level = own_pl.map_or(0, numeric_level);
    let mut admin_count = if let Some(ref lvls) = levels {
        room.members_no_sync(RoomMemberships::JOIN)
            .await
            .map(|joined| {
                joined
                    .iter()
                    .filter(|m| member_role(lvls.for_user(m.user_id())) >= MemberRole::Admin)
                    .count()
            })
            .unwrap_or(0)
    } else {
        0
    };
    let mut info = build_member(target, &resolved.display_name, status, levels.as_ref(), own);
    if resolved.from_fallback
        && matches!(resolved.membership, MembershipState::Join)
        && info.role >= MemberRole::Admin
    {
        admin_count = admin_count.saturating_add(1);
    }
    if target == own
        && let Some(cached) = own_room_names.get(room_id)
    {
        info.name = cached.unwrap_or_else(|| own.localpart().to_owned());
    }
    let shared_room = if matches!(
        resolved.membership,
        MembershipState::Join | MembershipState::Invite
    ) {
        Some(build_shared_room(&room, state).await)
    } else {
        None
    };
    ContextRoomInfo {
        member: Some(info),
        room_name: Some(rname),
        own_is_creator,
        own_power_level,
        admin_count,
        shared_room,
        shared_check_failed: false,
        avatar_fallback: resolved.avatar_url,
    }
}

async fn fallback_check_room(
    client: &Client,
    room: &Room,
    target: &UserId,
    mode: FallbackMode,
) -> FallbackCheck {
    let matched = match mode {
        FallbackMode::SyncMembers => {
            match tokio::time::timeout(KEYS_TIMEOUT, room.get_member(target)).await {
                Ok(Ok(Some(member))) => matches!(
                    *member.membership(),
                    MembershipState::Join | MembershipState::Invite
                ),
                Ok(Ok(None)) => false,
                Ok(Err(_)) | Err(_) => return FallbackCheck::Failed,
            }
        }
        FallbackMode::SingleStateEvent => {
            match fetch_single_member_state(client, room, target).await {
                SingleMemberFetch::Found(m) => {
                    matches!(
                        m.membership,
                        MembershipState::Join | MembershipState::Invite
                    )
                }
                SingleMemberFetch::NotFound => false,
                SingleMemberFetch::Failed => return FallbackCheck::Failed,
            }
        }
    };
    if matched {
        FallbackCheck::Matched(build_shared_room(room, RoomState::Joined).await)
    } else {
        FallbackCheck::NotMember
    }
}

/// Discovers shared rooms (`Join` / `Invite`) with `target`, falling back boundedly when a
/// joined non-space room's member store is unsynced or stale.
///
/// Sliding sync loads members lazily (`$LAZY` + `$ME`), so rooms not opened in the current
/// session may still have `are_members_synced() == false`, or `are_members_synced() == true`
/// with a stale cached count if an early `OwnMembers` sync ran before `target` joined:
/// 1. Pass 1 reads only the local SDK member store (zero HTTP requests).
/// 2. Pass 2 considers joined non-space rooms not resolved in Pass 1 (skipping any room already
///    checked via `context_room_info` or inside [`SHARED_ROOM_COOLDOWN`]). Candidates are ordered
///    DMs-first then smallest member count first, truncated to [`SHARED_ROOM_MAX_SYNCS`], and
///    queried with at most [`SHARED_ROOM_CONCURRENCY`] in flight under a shared
///    [`SHARED_ROOM_SYNC_TIMEOUT`] deadline:
///    - Unsynced rooms with at most [`SHARED_ROOM_MAX_MEMBERS`] members call `room.get_member`,
///      filling the SDK store once so subsequent popups hit Pass 1 with no network calls.
///    - Larger unsynced rooms and stale-synced rooms (`stored_joined < joined_members_count()`)
///      query only `m.room.member/{target}` so huge `/members` responses and full-member Megolm
///      key rotations are avoided.
///
/// Rooms whose fallback errors or misses the deadline enter [`SHARED_ROOM_COOLDOWN`].
/// Also returns `true` when fallback checks left candidates unqueried or failed/timed out.
async fn collect_shared_rooms(
    client: &Client,
    target: &UserId,
    skip_room_id: Option<&str>,
    cooldown: &MemberSyncCooldown,
) -> (Vec<UserSharedRoom>, bool) {
    let mut rooms_out = Vec::new();
    let mut raw_candidates = Vec::new();
    let mut candidate_rooms: HashMap<String, Room> = HashMap::new();

    for room in client.rooms() {
        let state = room.state();
        if !matches!(state, RoomState::Joined | RoomState::Invited) {
            continue;
        }
        if let Ok(Some(member)) = room.get_member_no_sync(target).await {
            if matches!(
                *member.membership(),
                MembershipState::Join | MembershipState::Invite
            ) {
                rooms_out.push(build_shared_room(&room, state).await);
            }
            continue;
        }

        let rid = room.room_id().to_string();
        if state != RoomState::Joined
            || room.is_space()
            || skip_room_id.is_some_and(|skip| skip == rid)
        {
            continue;
        }

        let joined_count = room.joined_members_count();
        let invited_count = room.invited_members_count();
        let members_synced = room.are_members_synced();
        let stored_joined = if members_synced {
            room.members_no_sync(RoomMemberships::JOIN)
                .await
                .map(|m| m.len() as u64)
                .unwrap_or(0)
        } else {
            0
        };
        let Some(mode) =
            classify_fallback(members_synced, stored_joined, joined_count, invited_count)
        else {
            continue;
        };

        let direct = room.is_direct().await.unwrap_or(false);
        raw_candidates.push(FallbackCandidate {
            room_id: rid.clone(),
            direct,
            members_count: joined_count.saturating_add(invited_count),
            mode,
        });
        candidate_rooms.insert(rid, room);
    }

    let (chosen, mut incomplete) =
        select_fallback_candidates(raw_candidates, cooldown, Instant::now());
    if !chosen.is_empty() {
        let chosen_ids: Vec<String> = chosen.iter().map(|c| c.room_id.clone()).collect();
        let mut completed: HashMap<String, bool> = HashMap::new();
        let mut stream = futures::stream::iter(chosen.into_iter().filter_map(|cand| {
            let room = candidate_rooms.get(&cand.room_id)?.clone();
            let client = client.clone();
            Some(async move {
                let outcome = fallback_check_room(&client, &room, target, cand.mode).await;
                (cand.room_id, outcome)
            })
        }))
        .buffer_unordered(SHARED_ROOM_CONCURRENCY);

        let deadline = tokio::time::Instant::now() + SHARED_ROOM_SYNC_TIMEOUT;
        while let Ok(Some((rid, outcome))) = tokio::time::timeout_at(deadline, stream.next()).await
        {
            match outcome {
                FallbackCheck::Matched(shared) => {
                    completed.insert(rid, true);
                    rooms_out.push(shared);
                }
                FallbackCheck::NotMember => {
                    completed.insert(rid, true);
                }
                FallbackCheck::Failed => {
                    completed.insert(rid, false);
                }
            }
        }
        drop(stream);

        let done_at = Instant::now();
        for rid in chosen_ids {
            match completed.get(&rid).copied() {
                Some(true) => cooldown.clear(&rid),
                Some(false) | None => {
                    incomplete = true;
                    cooldown.record_failure(rid, done_at);
                }
            }
        }
    }

    sort_shared_rooms(&mut rooms_out);
    (rooms_out, incomplete)
}

async fn find_member_avatar(
    client: &Client,
    target: &UserId,
    preferred_room: Option<&str>,
) -> Option<String> {
    if let Some(rid) = preferred_room
        && let Ok(room) = resolve_room(client, rid)
        && let Ok(Some(member)) = room.get_member_no_sync(target).await
        && let Some(uri) = member.avatar_url()
        && let Some(safe) = safety::safe_mxc(uri)
    {
        return Some(safe);
    }
    for room in client.rooms() {
        if !matches!(room.state(), RoomState::Joined | RoomState::Invited) {
            continue;
        }
        if let Ok(Some(member)) = room.get_member_no_sync(target).await
            && let Some(uri) = member.avatar_url()
            && let Some(safe) = safety::safe_mxc(uri)
        {
            return Some(safe);
        }
    }
    None
}

async fn lookup_verify_status(client: &Client, target: &UserId) -> UserVerifyStatus {
    let enc = client.encryption();
    let existing = match enc.get_user_identity(target).await {
        Ok(Some(identity)) => Some(identity),
        Ok(None) => tokio::time::timeout(KEYS_TIMEOUT, enc.request_user_identity(target))
            .await
            .ok()
            .and_then(Result::ok)
            .flatten(),
        Err(_) => return UserVerifyStatus::Unknown,
    };
    match existing {
        Some(identity) if identity.has_verification_violation() => UserVerifyStatus::Violation,
        Some(identity) if identity.is_verified() => UserVerifyStatus::Verified,
        Some(_) => UserVerifyStatus::Unverified,
        None => UserVerifyStatus::NoKeys,
    }
}

pub(super) async fn set_display_name(client: &Client, name: Option<String>) -> Result<(), String> {
    let cleaned = clean_name(name.as_deref());
    client
        .account()
        .set_display_name(cleaned.as_deref())
        .await
        .map_err(|err| failed("update your display name", &err))
}

/// Reads and validates a local image file for either an account or room avatar upload.
pub(super) async fn prepare_avatar_upload(
    client: &Client,
    path: &Path,
) -> Result<(mime::Mime, Vec<u8>), String> {
    let limit = tokio::time::timeout(
        Duration::from_secs(5),
        client.load_or_fetch_max_upload_size(),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .map(Into::into);
    let owned_path = path.to_owned();
    let prepared = tokio::task::spawn_blocking(move || upload::prepare(&owned_path, limit))
        .await
        .map_err(|_| "Could not read that image.".to_owned())??;
    if prepared.picture.is_none() {
        return Err(format!("{} is not a readable image", prepared.name));
    }
    let mime: mime::Mime = prepared
        .mime
        .parse()
        .map_err(|_| format!("{} has an unsupported image type", prepared.name))?;
    if mime.type_() != mime::IMAGE {
        return Err(format!("{} is not a supported image", prepared.name));
    }
    Ok((mime, prepared.bytes))
}

pub(super) async fn set_avatar(client: &Client, path: &Path) -> Result<(), String> {
    let (mime, bytes) = prepare_avatar_upload(client, path).await?;
    let mxc = tokio::time::timeout(UPLOAD_TIMEOUT, client.account().upload_avatar(&mime, bytes))
        .await
        .map_err(|_| "Uploading the avatar took too long.".to_owned())?
        .map_err(|err| failed("upload your avatar", &err))?;
    client
        .account()
        .set_avatar_url(Some(&mxc))
        .await
        .map_err(|err| failed("update your avatar", &err))
}

pub(super) async fn remove_avatar(client: &Client) -> Result<(), String> {
    client
        .account()
        .set_avatar_url(None)
        .await
        .map_err(|err| failed("remove your avatar", &err))
}

pub(super) async fn set_room_display_name(
    client: &Client,
    room_id: &str,
    name: Option<String>,
) -> Result<Option<String>, String> {
    let room = resolve_room(client, room_id)?;
    let target_name = match clean_name(name.as_deref()) {
        Some(custom) => Some(custom),
        // Clearing a per-room name restores the account's global display name.
        None => client
            .account()
            .get_display_name()
            .await
            .ok()
            .flatten()
            .and_then(|global| clean_name(Some(&global))),
    };
    room.set_own_member_display_name(target_name.clone())
        .await
        .map(|_| target_name)
        .map_err(|err| failed("update your name in this room", &err))
}

/// Reads the room's current synced own display name so timeline items never stay stale.
pub(super) async fn room_own_display_name(
    room: &Room,
    own_room_names: &OwnRoomNames,
) -> Option<String> {
    let own = room.own_user_id();
    if let Some(cached) = own_room_names.get(room.room_id().as_str()) {
        return Some(cached.unwrap_or_else(|| own.localpart().to_owned()));
    }
    let member = room.get_member_no_sync(own).await.ok()??;
    Some(clean_name(member.display_name()).unwrap_or_else(|| own.localpart().to_owned()))
}

async fn room_display_name(room: &Room) -> String {
    let raw = match room.cached_display_name() {
        Some(name) => name.to_string(),
        None => room
            .display_name()
            .await
            .map(|name| name.to_string())
            .unwrap_or_else(|_| room.room_id().to_string()),
    };
    let clean = sanitize_line(&raw);
    if clean.is_empty() {
        room.room_id().to_string()
    } else {
        clean
    }
}

fn member_status(state: MembershipState) -> Option<MemberStatus> {
    match state {
        MembershipState::Join => Some(MemberStatus::Joined),
        MembershipState::Invite => Some(MemberStatus::Invited),
        MembershipState::Ban => Some(MemberStatus::Banned),
        _ => None,
    }
}

fn extract_profile(profile: &get_profile::v3::Response) -> (Option<String>, Option<String>) {
    let name = profile.get_static::<DisplayName>().ok().flatten();
    let avatar = profile.get_static::<AvatarUrl>().ok().flatten();
    (
        clean_name(name.as_deref()),
        avatar.as_deref().and_then(safety::safe_mxc),
    )
}

fn clean_name(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    let clean = sanitize_line(raw).trim().to_owned();
    (!is_blank(&clean)).then_some(clean)
}

fn profile_error(err: &matrix_sdk::Error) -> String {
    use matrix_sdk::ruma::api::error::ErrorKind;
    match err.client_api_error_kind() {
        Some(ErrorKind::NotFound) => "User not found.".to_owned(),
        Some(ErrorKind::Forbidden) => "This server does not share that profile.".to_owned(),
        _ => failed("look up that user", err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_impls_redact_private_profile_fields() {
        let secret_uid = "@secret_person:example.org";
        let secret_name = "Alice Secret Name";
        let secret_avatar = "mxc://example.org/secret-avatar-id";
        let secret_room = "Secret War Room";

        let own = OwnProfile {
            display_name: Some(secret_name.to_owned()),
            avatar: Some(secret_avatar.to_owned()),
            room_names: BTreeMap::from([(
                "!r1:example.org".to_owned(),
                Some(secret_name.to_owned()),
            )]),
        };
        let own_dbg = format!("{own:?}");
        assert!(!own_dbg.contains(secret_name));
        assert!(!own_dbg.contains(secret_avatar));
        assert!(!own_dbg.contains("!r1:example.org"));

        let shared = UserSharedRoom {
            room_id: "!r1:example.org".to_owned(),
            name: secret_room.to_owned(),
            direct: true,
            invited: false,
        };
        let shared_dbg = format!("{shared:?}");
        assert!(!shared_dbg.contains("!r1:example.org"));
        assert!(!shared_dbg.contains(secret_room));

        let user = UserProfile {
            user_id: secret_uid.to_owned(),
            display_name: Some(secret_name.to_owned()),
            avatar: Some(secret_avatar.to_owned()),
            verify: UserVerifyStatus::Verified,
            shared_rooms: vec![shared],
            shared_rooms_incomplete: false,
            room_member: None,
            room_name: Some(secret_room.to_owned()),
            own_is_creator: false,
            own_power_level: 0,
            admin_count: 0,
            own: false,
            note: Some("secret note".to_owned()),
        };
        let user_dbg = format!("{user:?}");
        assert!(!user_dbg.contains(secret_uid));
        assert!(!user_dbg.contains(secret_name));
        assert!(!user_dbg.contains(secret_avatar));
        assert!(!user_dbg.contains(secret_room));
        assert!(!user_dbg.contains("secret note"));
    }

    #[test]
    fn own_profile_effective_name_prefers_synced_room_entry_then_global_then_localpart() {
        let mut profile = OwnProfile {
            display_name: Some("Global Alice".to_owned()),
            avatar: None,
            room_names: BTreeMap::new(),
        };
        assert_eq!(
            profile.display_or_localpart(Some("!unseen:hs"), Some("@alice:hs")),
            Some("Global Alice".to_owned())
        );

        profile
            .room_names
            .insert("!work:hs".to_owned(), Some("Work Alice".to_owned()));
        assert_eq!(
            profile.display_or_localpart(Some("!work:hs"), Some("@alice:hs")),
            Some("Work Alice".to_owned())
        );

        profile.room_names.insert("!bare:hs".to_owned(), None);
        assert_eq!(
            profile.display_or_localpart(Some("!bare:hs"), Some("@alice:hs")),
            Some("alice".to_owned())
        );
    }

    #[test]
    fn clean_name_strips_controls_and_rejects_whitespace_only() {
        assert_eq!(clean_name(Some("  \n\t ")), None);
        assert_eq!(clean_name(Some("Alice\n\x07")), Some("Alice".to_owned()));
    }

    #[test]
    fn fallback_localpart_extracts_local_part_safely() {
        assert_eq!(fallback_localpart("@alice:example.org"), "alice");
        assert_eq!(fallback_localpart("plain"), "plain");
    }

    #[test]
    fn own_room_names_tracks_and_clears_synced_member_events() {
        let names = OwnRoomNames::default();
        assert_eq!(names.get("!r:hs"), None);

        let join_named: SyncRoomMemberEvent = serde_json::from_value(serde_json::json!({
            "type": "m.room.member",
            "event_id": "$ev1",
            "sender": "@alice:hs",
            "state_key": "@alice:hs",
            "origin_server_ts": 1000,
            "content": {
                "membership": "join",
                "displayname": "Room\n\x07 Alice  "
            }
        }))
        .expect("parse named join member event");
        record_own_member_event(&names, "!r:hs", &join_named);
        assert_eq!(names.get("!r:hs"), Some(Some("Room  Alice".to_owned())));

        let join_bare: SyncRoomMemberEvent = serde_json::from_value(serde_json::json!({
            "type": "m.room.member",
            "event_id": "$ev2",
            "sender": "@alice:hs",
            "state_key": "@alice:hs",
            "origin_server_ts": 2000,
            "content": {
                "membership": "join",
                "displayname": "   "
            }
        }))
        .expect("parse cleared join member event");
        record_own_member_event(&names, "!r:hs", &join_bare);
        assert_eq!(names.get("!r:hs"), Some(None));

        let leave: SyncRoomMemberEvent = serde_json::from_value(serde_json::json!({
            "type": "m.room.member",
            "event_id": "$ev3",
            "sender": "@alice:hs",
            "state_key": "@alice:hs",
            "origin_server_ts": 3000,
            "content": {
                "membership": "leave"
            }
        }))
        .expect("parse leave member event");
        record_own_member_event(&names, "!r:hs", &leave);
        assert_eq!(names.get("!r:hs"), None);
    }

    #[test]
    fn classify_fallback_picks_sync_or_state_or_none() {
        assert_eq!(
            classify_fallback(false, 0, 10, 0),
            Some(FallbackMode::SyncMembers)
        );
        assert_eq!(
            classify_fallback(false, 0, SHARED_ROOM_MAX_MEMBERS, 0),
            Some(FallbackMode::SyncMembers)
        );
        assert_eq!(
            classify_fallback(false, 0, SHARED_ROOM_MAX_MEMBERS + 1, 0),
            Some(FallbackMode::SingleStateEvent)
        );
        assert_eq!(
            classify_fallback(false, 0, u64::MAX, 1),
            Some(FallbackMode::SingleStateEvent)
        );
        assert_eq!(classify_fallback(true, 2, 2, 0), None);
        assert_eq!(
            classify_fallback(true, 1, 2, 0),
            Some(FallbackMode::SingleStateEvent)
        );
    }

    #[test]
    fn select_fallback_candidates_orders_dms_and_small_rooms_skips_cooldown_and_caps() {
        let now = Instant::now();
        let cooldown = MemberSyncCooldown::default();
        cooldown.record_failure("!cool:hs".to_owned(), now - Duration::from_secs(60));
        cooldown.record_failure(
            "!expired:hs".to_owned(),
            now - SHARED_ROOM_COOLDOWN - Duration::from_secs(1),
        );

        let mut raw = vec![
            FallbackCandidate {
                room_id: "!cool:hs".to_owned(),
                direct: true,
                members_count: 2,
                mode: FallbackMode::SyncMembers,
            },
            FallbackCandidate {
                room_id: "!expired:hs".to_owned(),
                direct: false,
                members_count: 3,
                mode: FallbackMode::SyncMembers,
            },
            FallbackCandidate {
                room_id: "!large-group:hs".to_owned(),
                direct: false,
                members_count: 400,
                mode: FallbackMode::SingleStateEvent,
            },
            FallbackCandidate {
                room_id: "!dm-b:hs".to_owned(),
                direct: true,
                members_count: 2,
                mode: FallbackMode::SyncMembers,
            },
            FallbackCandidate {
                room_id: "!dm-a:hs".to_owned(),
                direct: true,
                members_count: 2,
                mode: FallbackMode::SyncMembers,
            },
        ];
        for i in 0..20 {
            raw.push(FallbackCandidate {
                room_id: format!("!group-{i:02}:hs"),
                direct: false,
                members_count: 10 + i as u64,
                mode: FallbackMode::SyncMembers,
            });
        }

        let (picked, left_unqueried) = select_fallback_candidates(raw, &cooldown, now);
        assert!(left_unqueried);
        assert_eq!(picked.len(), SHARED_ROOM_MAX_SYNCS);
        assert!(!picked.iter().any(|c| c.room_id == "!cool:hs"));
        assert!(!picked.iter().any(|c| c.room_id == "!large-group:hs"));
        assert_eq!(picked[0].room_id, "!dm-a:hs");
        assert_eq!(picked[1].room_id, "!dm-b:hs");
        assert_eq!(picked[2].room_id, "!expired:hs");

        // Fewer than cap and none in cooldown -> nothing left unqueried.
        let small: Vec<FallbackCandidate> = (0..3)
            .map(|i| FallbackCandidate {
                room_id: format!("!ok-{i}:hs"),
                direct: false,
                members_count: 5,
                mode: FallbackMode::SyncMembers,
            })
            .collect();
        let (picked_small, incomplete_small) = select_fallback_candidates(small, &cooldown, now);
        assert_eq!(picked_small.len(), 3);
        assert!(!incomplete_small);

        // Fewer than cap, but one is in cooldown -> left_unqueried is true.
        let cooled = vec![
            FallbackCandidate {
                room_id: "!cool:hs".to_owned(),
                direct: false,
                members_count: 5,
                mode: FallbackMode::SyncMembers,
            },
            FallbackCandidate {
                room_id: "!ok:hs".to_owned(),
                direct: false,
                members_count: 5,
                mode: FallbackMode::SyncMembers,
            },
        ];
        let (picked_cooled, incomplete_cooled) = select_fallback_candidates(cooled, &cooldown, now);
        assert_eq!(picked_cooled.len(), 1);
        assert!(incomplete_cooled);
    }

    #[test]
    fn member_sync_cooldown_prunes_and_caps_entries() {
        let base = Instant::now();
        let cooldown = MemberSyncCooldown::default();
        cooldown.record_failure("!old:hs".to_owned(), base);
        assert!(cooldown.is_cooling_down("!old:hs", base + Duration::from_secs(10)));
        assert!(!cooldown.is_cooling_down("!old:hs", base + SHARED_ROOM_COOLDOWN));

        let later = base + SHARED_ROOM_COOLDOWN + Duration::from_secs(1);
        for i in 0..(SHARED_ROOM_COOLDOWN_CAP + 10) {
            cooldown.record_failure(format!("!r{i}:hs"), later + Duration::from_millis(i as u64));
        }
        let guard = cooldown.0.lock().unwrap();
        assert!(!guard.contains_key("!old:hs"));
        assert_eq!(guard.len(), SHARED_ROOM_COOLDOWN_CAP);
    }

    #[tokio::test]
    #[ignore = "needs a local homeserver"]
    async fn own_profile_room_name_avatar_and_user_info_sync() {
        use std::io::Cursor;
        use tokio::sync::mpsc::UnboundedReceiver;

        use crate::format::Outgoing;
        use crate::matrix::{
            Answer, Change, ManageCmd, ManageEvent, MatrixCmd, MatrixEvent, VerifyCmd, spawn,
        };
        use crate::paths::Paths;

        async fn recv_until<T>(
            events: &mut UnboundedReceiver<MatrixEvent>,
            step: &str,
            mut pick: impl FnMut(MatrixEvent) -> Option<T>,
        ) -> T {
            tokio::time::timeout(Duration::from_secs(45), async {
                loop {
                    match events.recv().await.expect("worker stopped") {
                        MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => {
                            panic!("worker error: {err}")
                        }
                        other => {
                            if let Some(found) = pick(other) {
                                return found;
                            }
                        }
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for server event: {step}"))
        }

        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let username = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let root =
            std::env::temp_dir().join(format!("matrix-rat-people-srv-{}", std::process::id()));
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
            username: username.clone(),
            password: password.clone().into(),
        });
        let own_id = recv_until(&mut events, "login", |e| match e {
            MatrixEvent::LoggedIn { user_id } => Some(user_id),
            _ => None,
        })
        .await;

        let bob = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .unwrap();
        bob.matrix_auth()
            .login_username("bob", "bobpass")
            .initial_device_display_name("matrix-rat people test")
            .send()
            .await
            .unwrap();
        let bob_id = bob.user_id().unwrap().to_string();

        // Wait for initial profile sync so our subsequent updates have a clean baseline.
        let _ = recv_until(&mut events, "initial own profile", |e| match e {
            MatrixEvent::OwnProfile(p) => Some(p),
            _ => None,
        })
        .await;

        let tag = std::process::id();
        let global_name = format!("Alice People {tag}");

        // 1. Set global display name and verify sync + plain-client lookup.
        handle.send(MatrixCmd::SetOwnDisplayName {
            name: Some(global_name.clone()),
        });
        recv_until(&mut events, "set display name result", |e| match e {
            MatrixEvent::OwnProfileResult {
                what: OwnProfileAction::DisplayName,
                result,
            } => {
                result.expect("set display name");
                Some(())
            }
            _ => None,
        })
        .await;
        recv_until(&mut events, "global display name synced", |e| match e {
            MatrixEvent::OwnProfile(p) if p.display_name.as_deref() == Some(&global_name) => {
                Some(())
            }
            _ => None,
        })
        .await;

        // 2. Upload avatar picture, observe it from another account, then remove it.
        let mut png_bytes = Vec::new();
        crate::images::demo_picture(32, 20)
            .write_to(&mut Cursor::new(&mut png_bytes), image::ImageFormat::Png)
            .unwrap();
        let avatar_path = root.join("avatar.png");
        std::fs::write(&avatar_path, &png_bytes).unwrap();
        handle.send(MatrixCmd::SetOwnAvatar { path: avatar_path });
        recv_until(&mut events, "set avatar result", |e| match e {
            MatrixEvent::OwnProfileResult {
                what: OwnProfileAction::Avatar,
                result,
            } => {
                result.expect("set avatar");
                Some(())
            }
            _ => None,
        })
        .await;
        let uploaded_avatar = recv_until(&mut events, "avatar mxc synced", |e| match e {
            MatrixEvent::OwnProfile(p) => p.avatar,
            _ => None,
        })
        .await;
        let own_uid = UserId::parse(&own_id).unwrap();
        let bob_seen = bob.account().fetch_user_profile_of(&own_uid).await.unwrap();
        let (seen_name, seen_avatar) = extract_profile(&bob_seen);
        assert_eq!(seen_name.as_deref(), Some(global_name.as_str()));
        assert_eq!(seen_avatar.as_deref(), Some(uploaded_avatar.as_str()));

        handle.send(MatrixCmd::RemoveOwnAvatar);
        recv_until(&mut events, "remove avatar result", |e| match e {
            MatrixEvent::OwnProfileResult {
                what: OwnProfileAction::RemoveAvatar,
                result,
            } => {
                result.expect("remove avatar");
                Some(())
            }
            _ => None,
        })
        .await;
        recv_until(&mut events, "avatar removal synced", |e| match e {
            MatrixEvent::OwnProfile(p) if p.avatar.is_none() => Some(()),
            _ => None,
        })
        .await;

        // 3. Create a shared room, join Bob, test per-room display name + timeline live updates.
        handle.send(MatrixCmd::Manage(ManageCmd::Create {
            request: 1,
            name: format!("matrix-rat people {tag}"),
            topic: None,
            alias: None,
            encrypted: false,
        }));
        let room_id = recv_until(&mut events, "create room", |e| match e {
            MatrixEvent::Manage(ManageEvent::Opened { request: 1, result }) => {
                Some(result.unwrap())
            }
            _ => None,
        })
        .await;
        handle.send(MatrixCmd::Manage(ManageCmd::Invite {
            room_id: room_id.clone(),
            user_id: bob_id.clone(),
        }));
        recv_until(&mut events, "invite bob", |e| match e {
            MatrixEvent::Manage(ManageEvent::Done {
                change: Change::Invite,
                result,
                ..
            }) => {
                result.expect("invite bob");
                Some(())
            }
            _ => None,
        })
        .await;
        let parsed_room = matrix_sdk::ruma::RoomId::parse(&room_id).unwrap();
        bob.join_room_by_id(&parsed_room).await.unwrap();

        // Wait until room details include Bob as joined.
        loop {
            handle.send(MatrixCmd::Manage(ManageCmd::Info {
                room_id: room_id.clone(),
            }));
            let details = recv_until(&mut events, "room info poll bob", |e| match e {
                MatrixEvent::Manage(ManageEvent::Info {
                    room_id: r,
                    result: Ok(d),
                }) if r == room_id => Some(*d),
                _ => None,
            })
            .await;
            if details
                .members
                .iter()
                .any(|m| m.user_id == bob_id && !m.invited)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }

        // Set per-room name, send message, open room timeline -> message shows per-room name.
        let room_nick = format!("Alice Room {tag}");
        handle.send(MatrixCmd::Manage(ManageCmd::SetOwnName {
            room_id: room_id.clone(),
            name: Some(room_nick.clone()),
        }));
        recv_until(&mut events, "set room own name", |e| match e {
            MatrixEvent::Manage(ManageEvent::Done {
                change: Change::OwnName,
                result,
                ..
            }) => {
                result.expect("set room own name");
                Some(())
            }
            _ => None,
        })
        .await;
        recv_until(&mut events, "room own name in OwnProfile", |e| match e {
            MatrixEvent::OwnProfile(p)
                if p.effective_display_name(Some(&room_id)) == Some(room_nick.as_str()) =>
            {
                Some(())
            }
            _ => None,
        })
        .await;

        handle.send(MatrixCmd::Manage(ManageCmd::Info {
            room_id: room_id.clone(),
        }));
        let info = recv_until(&mut events, "room info after SetOwnName", |e| match e {
            MatrixEvent::Manage(ManageEvent::Info {
                room_id: r,
                result: Ok(d),
            }) if r == room_id && !d.members_loading => Some(*d),
            _ => None,
        })
        .await;
        assert_eq!(info.own_display_name.as_deref(), Some(room_nick.as_str()));
        assert!(info.members.iter().any(|m| m.own && m.name == room_nick));

        handle.send(MatrixCmd::OpenRoom {
            room_id: room_id.clone(),
            generation: 1,
        });
        handle.send(MatrixCmd::Send {
            room_id: room_id.clone(),
            message: Outgoing::Text {
                plain: "hi under room nick".into(),
                html: None,
                mentions: None,
            },
            reply_to: None,
        });
        recv_until(
            &mut events,
            "timeline message under room nick",
            |e| match e {
                MatrixEvent::Timeline {
                    room_id: r,
                    messages,
                    ..
                } if r == room_id
                    && messages.iter().any(|m| {
                        m.own && m.body.contains("hi under room nick") && m.sender == room_nick
                    }) =>
                {
                    Some(())
                }
                _ => None,
            },
        )
        .await;

        // Clear per-room name -> timeline live-refreshes back to global display name.
        handle.send(MatrixCmd::Manage(ManageCmd::SetOwnName {
            room_id: room_id.clone(),
            name: None,
        }));
        recv_until(&mut events, "clear room own name", |e| match e {
            MatrixEvent::Manage(ManageEvent::Done {
                change: Change::OwnName,
                result,
                ..
            }) => {
                result.expect("clear room own name");
                Some(())
            }
            _ => None,
        })
        .await;
        recv_until(
            &mut events,
            "timeline message reverted to global name",
            |e| match e {
                MatrixEvent::Timeline {
                    room_id: r,
                    messages,
                    ..
                } if r == room_id
                    && messages.iter().any(|m| {
                        m.own && m.body.contains("hi under room nick") && m.sender == global_name
                    }) =>
                {
                    Some(())
                }
                _ => None,
            },
        )
        .await;

        // 4. FetchUserProfile returns Bob's shared room + room member details.
        handle.send(MatrixCmd::FetchUserProfile {
            request: 42,
            user_id: bob_id.clone(),
            room_id: Some(room_id.clone()),
        });
        let profile = recv_until(&mut events, "fetch bob profile", |e| match e {
            MatrixEvent::UserProfile {
                request: 42,
                result,
            } => Some(result.unwrap()),
            _ => None,
        })
        .await;
        assert_eq!(profile.user_id, bob_id);
        assert!(profile.shared_rooms.iter().any(|r| r.room_id == room_id));
        assert!(
            profile
                .room_member
                .as_ref()
                .is_some_and(|m| m.user_id == bob_id)
        );

        // 5. Verify action initiates a cross-user verification request cleanly.
        handle.send(MatrixCmd::Verification(VerifyCmd::RequestUser {
            user_id: bob_id.clone(),
        }));
        let flow = recv_until(&mut events, "verification event for bob", |e| match e {
            MatrixEvent::Verification(v) if !v.own && v.peer == bob_id => Some(v.flow),
            _ => None,
        })
        .await;
        handle.send(MatrixCmd::Verification(VerifyCmd::Answer {
            flow,
            answer: Answer::Cancel,
        }));

        // 6. Fresh session without opening the room still discovers shared_rooms via bounded fallback.
        handle.send(MatrixCmd::Logout);
        recv_until(&mut events, "logout session 1", |e| match e {
            MatrixEvent::NeedLogin { .. } => Some(()),
            _ => None,
        })
        .await;
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}

        let paths2 = Paths {
            config_dir: root.join("config2"),
            data_dir: root.join("data2"),
            log_dir: root.join("log2"),
            runtime_dir: root.join("run2"),
            profile: "it2".into(),
        };
        paths2.create_dirs().unwrap();
        let (handle2, mut events2) = spawn(paths2);
        handle2.send(MatrixCmd::LoginPassword {
            homeserver,
            username,
            password: password.into(),
        });
        recv_until(&mut events2, "login session 2", |e| match e {
            MatrixEvent::LoggedIn { .. } => Some(()),
            _ => None,
        })
        .await;
        recv_until(&mut events2, "room list sees shared room", |e| match e {
            MatrixEvent::Rooms(rooms) if rooms.iter().any(|r| r.id == room_id) => Some(()),
            _ => None,
        })
        .await;
        handle2.send(MatrixCmd::FetchUserProfile {
            request: 43,
            user_id: bob_id.clone(),
            room_id: None,
        });
        let profile2 = recv_until(
            &mut events2,
            "fetch bob profile in fresh session",
            |e| match e {
                MatrixEvent::UserProfile {
                    request: 43,
                    result,
                } => Some(result.unwrap()),
                _ => None,
            },
        )
        .await;
        assert!(profile2.shared_rooms.iter().any(|r| r.room_id == room_id));

        // Clean up.
        let _ = bob.get_room(&parsed_room).unwrap().leave().await;
        let _ = bob.logout().await;
        handle2.send(MatrixCmd::SetOwnDisplayName { name: None });
        let _ = recv_until(&mut events2, "cleanup clear display name", |e| match e {
            MatrixEvent::OwnProfileResult {
                what: OwnProfileAction::DisplayName,
                ..
            } => Some(()),
            _ => None,
        })
        .await;
        handle2.send(MatrixCmd::Manage(ManageCmd::Leave {
            room_id: room_id.clone(),
        }));
        let _ = recv_until(&mut events2, "cleanup leave room", |e| match e {
            MatrixEvent::Manage(_) => Some(()),
            _ => None,
        })
        .await;
        handle2.send(MatrixCmd::Logout);
        recv_until(&mut events2, "cleanup logout", |e| match e {
            MatrixEvent::NeedLogin { .. } => Some(()),
            _ => None,
        })
        .await;
        handle2.send(MatrixCmd::Shutdown);
        while events2.recv().await.is_some() {}
        let _ = std::fs::remove_dir_all(root);
    }
}
