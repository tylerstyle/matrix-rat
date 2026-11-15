//! Room management: joining, creating, leaving, inviting, and the details of a room
//! (members, roles, what we may do there).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use matrix_sdk::notification_settings::RoomNotificationMode;
use matrix_sdk::ruma::api::client::membership::get_member_events;
use matrix_sdk::ruma::api::client::room::aliases::v3::Request as AliasesRequest;
use matrix_sdk::ruma::api::client::room::create_room::v3::{Request as CreateRequest, RoomPreset};
use matrix_sdk::ruma::api::error::{ErrorBody, ErrorKind};
use matrix_sdk::ruma::events::InitialStateEvent;
use matrix_sdk::ruma::events::direct::DirectUserIdentifier;
use matrix_sdk::ruma::events::room::encryption::RoomEncryptionEventContent;
use matrix_sdk::ruma::events::room::history_visibility::HistoryVisibility;
use matrix_sdk::ruma::events::room::join_rules::JoinRule;
use matrix_sdk::ruma::events::room::member::{MembershipState, RoomMemberEvent};
use matrix_sdk::ruma::events::room::power_levels::{
    RoomPowerLevels, RoomPowerLevelsEventContent, UserPowerLevel,
};
use matrix_sdk::ruma::events::{MessageLikeEventType, StateEventType, TimelineEventType};
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::{
    Int, OwnedRoomAliasId, OwnedRoomOrAliasId, OwnedServerName, OwnedUserId, RoomAliasId, RoomId,
    RoomOrAliasId, ServerName, UInt, UserId,
};
use matrix_sdk::{Client, NotificationSettingsError, Room, RoomMemberships, RoomState};
use tokio::sync::{Notify, mpsc::UnboundedSender, watch};
use tokio::task::JoinHandle;

use super::MatrixEvent;
use crate::model::RoomPower;
use crate::text::{sanitize, sanitize_line, truncate};

/// Above this many members, the list is asked for again at most this often while the
/// room info is open: the request is a big one.
const LARGE_ROOM: u64 = 1000;
const LARGE_ROOM_REFETCH: Duration = Duration::from_secs(60);
/// Rooms with more members than this are not asked for their complete list at all;
/// the view shows the members known here and says so.
const MAX_MEMBERS: u64 = 20_000;
/// How long the server gets to send a member list.
const MEMBERS_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest piece of a server's own words that is passed on to the user.
const SERVER_TEXT: usize = 160;
/// Servers named in a join request; more only make it longer.
const VIA_MAX: usize = 5;

pub enum ManageCmd {
    /// Join by `#alias:server` or `!id:server`. Answered with [`ManageEvent::Opened`].
    Join {
        /// Comes back with the answer, so that it reaches who asked and nobody else.
        request: u64,
        target: String,
        via: Vec<String>,
    },
    /// Open the direct chat with this user. Answered with [`ManageEvent::Opened`], or
    /// with [`ManageEvent::NoDirectChat`] if there is none and `create` is off.
    DirectChat {
        request: u64,
        user_id: String,
        /// Make a new chat (a room and an invitation) if there is none.
        create: bool,
    },
    /// Create a room. Answered with [`ManageEvent::Opened`].
    Create {
        request: u64,
        name: String,
        topic: Option<String>,
        /// The part of the address before the colon; makes the room public.
        alias: Option<String>,
        /// Only for private rooms.
        encrypted: bool,
    },
    /// Leave (or decline the invitation) and forget. Answered with [`ManageEvent::Left`].
    Leave {
        room_id: String,
    },
    /// Send the room's details now and whenever they change, until [`Self::StopInfo`]
    /// or the next request.
    Info {
        room_id: String,
    },
    StopInfo,
    /// The rest is answered with [`ManageEvent::Done`].
    Invite {
        room_id: String,
        user_id: String,
    },
    /// Remove someone from the room, with an optional reason.
    Kick {
        room_id: String,
        user_id: String,
        reason: Option<String>,
    },
    /// Ban someone from the room, with an optional reason.
    Ban {
        room_id: String,
        user_id: String,
        reason: Option<String>,
    },
    /// Unban a previously banned member from the room, with an optional reason.
    Unban {
        room_id: String,
        user_id: String,
        reason: Option<String>,
    },
    /// Change a member's power level in the room (`0`, `50`, `100`).
    SetRole {
        room_id: String,
        user_id: String,
        level: i64,
    },
    SetName {
        room_id: String,
        name: String,
    },
    SetOwnName {
        room_id: String,
        name: Option<String>,
    },
    SetTopic {
        room_id: String,
        topic: String,
    },
    SetFavourite {
        room_id: String,
        on: bool,
    },
    SetLowPriority {
        room_id: String,
        on: bool,
    },
    SetNotify {
        room_id: String,
        level: NotifyLevel,
    },
    /// Fetches a preview summary of a room before joining or knocking.
    Preview {
        request: u64,
        target: String,
        via: Vec<String>,
    },
    /// Asks to join a knock-enabled room with an optional reason.
    Knock {
        request: u64,
        target: String,
        via: Vec<String>,
        reason: Option<String>,
    },
    /// Accept an incoming knock request by inviting the user.
    AcceptKnock {
        room_id: String,
        user_id: String,
    },
    /// Decline an incoming knock request.
    DeclineKnock {
        room_id: String,
        user_id: String,
    },
    /// Decline an incoming knock request and ban the user, with an optional reason.
    BanKnock {
        room_id: String,
        user_id: String,
        reason: Option<String>,
    },
    /// Change `m.room.join_rules`.
    SetJoinRule {
        room_id: String,
        rule: RoomJoinRule,
    },
    /// Change `m.room.history_visibility`.
    SetHistoryVisibility {
        room_id: String,
        visibility: RoomHistory,
    },
    /// Turn room encryption on permanently (`m.room.encryption`).
    EnableEncryption {
        room_id: String,
    },
    /// Upload and set the room's avatar picture (`m.room.avatar`).
    SetRoomAvatar {
        room_id: String,
        path: PathBuf,
    },
    /// Remove the room's avatar picture.
    RemoveRoomAvatar {
        room_id: String,
    },
    /// Update `m.room.canonical_alias` (`alias` + `alt_aliases`).
    SetAddresses {
        room_id: String,
        canonical: Option<String>,
        alt_aliases: Vec<String>,
    },
    /// Create a local directory alias (`#name:our-homeserver`) pointing to this room.
    CreateLocalAlias {
        room_id: String,
        alias: String,
    },
    /// Delete a local directory alias from our homeserver.
    DeleteLocalAlias {
        room_id: String,
        alias: String,
    },
    /// Mark or unmark the room as a direct chat in `m.direct`.
    SetDirect {
        room_id: String,
        direct: bool,
    },
    /// Update `m.room.power_levels` thresholds while protecting creator / self-lockout rules.
    SetPermissions {
        room_id: String,
        permissions: RoomPermissions,
    },
}

/// Prints which room, never names, topics or who is being written to.
impl std::fmt::Debug for ManageCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Join { via, .. } => write!(f, "Join(via {} servers)", via.len()),
            Self::DirectChat { create, .. } => write!(f, "DirectChat(create: {create})"),
            Self::Create { alias, .. } => write!(f, "Create(public: {})", alias.is_some()),
            Self::Leave { room_id } => write!(f, "Leave({room_id})"),
            Self::Info { room_id } => write!(f, "Info({room_id})"),
            Self::StopInfo => f.write_str("StopInfo"),
            Self::Invite { room_id, .. } => write!(f, "Invite({room_id})"),
            Self::Kick {
                room_id, reason, ..
            } => write!(f, "Kick({room_id}, reason: {})", reason.is_some()),
            Self::Ban {
                room_id, reason, ..
            } => write!(f, "Ban({room_id}, reason: {})", reason.is_some()),
            Self::Unban {
                room_id, reason, ..
            } => write!(f, "Unban({room_id}, reason: {})", reason.is_some()),
            Self::SetRole { room_id, level, .. } => write!(f, "SetRole({room_id}, {level})"),
            Self::SetName { room_id, .. } => write!(f, "SetName({room_id})"),
            Self::SetOwnName { room_id, .. } => write!(f, "SetOwnName({room_id})"),
            Self::SetTopic { room_id, .. } => write!(f, "SetTopic({room_id})"),
            Self::SetFavourite { room_id, on } => write!(f, "SetFavourite({room_id}, {on})"),
            Self::SetLowPriority { room_id, on } => write!(f, "SetLowPriority({room_id}, {on})"),
            Self::SetNotify { room_id, level } => write!(f, "SetNotify({room_id}, {level:?})"),
            Self::Preview { request, via, .. } => {
                write!(f, "Preview(request {request}, via {} servers)", via.len())
            }
            Self::Knock {
                request,
                via,
                reason,
                ..
            } => write!(
                f,
                "Knock(request {request}, via {} servers, reason: {})",
                via.len(),
                reason.is_some()
            ),
            Self::AcceptKnock { room_id, .. } => write!(f, "AcceptKnock({room_id})"),
            Self::DeclineKnock { room_id, .. } => write!(f, "DeclineKnock({room_id})"),
            Self::BanKnock {
                room_id, reason, ..
            } => write!(f, "BanKnock({room_id}, reason: {})", reason.is_some()),
            Self::SetJoinRule { room_id, rule } => write!(f, "SetJoinRule({room_id}, {rule:?})"),
            Self::SetHistoryVisibility {
                room_id,
                visibility,
            } => write!(f, "SetHistoryVisibility({room_id}, {visibility:?})"),
            Self::EnableEncryption { room_id } => write!(f, "EnableEncryption({room_id})"),
            Self::SetRoomAvatar { room_id, .. } => write!(f, "SetRoomAvatar({room_id})"),
            Self::RemoveRoomAvatar { room_id } => write!(f, "RemoveRoomAvatar({room_id})"),
            Self::SetAddresses {
                room_id,
                canonical,
                alt_aliases,
            } => write!(
                f,
                "SetAddresses({room_id}, main: {}, alt: {})",
                canonical.is_some(),
                alt_aliases.len()
            ),
            Self::CreateLocalAlias { room_id, .. } => write!(f, "CreateLocalAlias({room_id})"),
            Self::DeleteLocalAlias { room_id, .. } => write!(f, "DeleteLocalAlias({room_id})"),
            Self::SetDirect { room_id, direct } => write!(f, "SetDirect({room_id}, {direct})"),
            Self::SetPermissions { room_id, .. } => write!(f, "SetPermissions({room_id})"),
        }
    }
}

/// Which change a [`ManageEvent::Done`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Invite,
    Kick,
    Ban,
    Unban,
    SetRole,
    AcceptKnock,
    DeclineKnock,
    BanKnock,
    Name,
    OwnName,
    Topic,
    Favourite(bool),
    LowPriority(bool),
    Notify,
    JoinRule,
    HistoryVisibility,
    EnableEncryption,
    RoomAvatar,
    RemoveRoomAvatar,
    Addresses,
    CreateLocalAlias,
    DeleteLocalAlias,
    Direct(bool),
    Permissions,
}

pub enum ManageEvent {
    /// A join, a direct chat or a new room: the id of the room to show, or what went
    /// wrong, in words for the user.
    Opened {
        request: u64,
        result: Result<String, String>,
    },
    /// There is no direct chat with this user, and none was to be made.
    NoDirectChat { request: u64, user_id: String },
    Left {
        room_id: String,
        result: Result<(), String>,
    },
    Done {
        room_id: String,
        change: Change,
        result: Result<(), String>,
    },
    Info {
        room_id: String,
        result: Result<Box<RoomDetails>, String>,
    },
    Preview {
        request: u64,
        result: Result<Box<super::directory::RoomPreview>, String>,
    },
    Knocked {
        request: u64,
        result: Result<String, String>,
    },
}

impl std::fmt::Debug for ManageEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Opened { request, result } => {
                write!(f, "Opened(request {request}, ok: {})", result.is_ok())
            }
            Self::NoDirectChat { request, .. } => write!(f, "NoDirectChat(request {request})"),
            Self::Left { room_id, result } => write!(f, "Left({room_id}, ok: {})", result.is_ok()),
            Self::Done {
                room_id,
                change,
                result,
            } => write!(f, "Done({room_id}, {change:?}, ok: {})", result.is_ok()),
            Self::Info { room_id, result } => match result {
                Ok(details) => write!(f, "Info({room_id}, {details:?})"),
                Err(_) => write!(f, "Info({room_id}, failed)"),
            },
            Self::Preview { request, result } => {
                write!(f, "Preview(request {request}, ok: {})", result.is_ok())
            }
            Self::Knocked { request, result } => {
                write!(f, "Knocked(request {request}, ok: {})", result.is_ok())
            }
        }
    }
}

/// When a room notifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyLevel {
    All,
    Mentions,
    Mute,
}

impl NotifyLevel {
    pub const ALL: [NotifyLevel; 3] = [NotifyLevel::All, NotifyLevel::Mentions, NotifyLevel::Mute];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum MemberRole {
    #[default]
    User,
    Moderator,
    Admin,
    /// A creator of the room, in rooms where that cannot be taken away.
    Owner,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct RoomMember {
    pub user_id: String,
    /// Display name, or the first part of the user id.
    pub name: String,
    pub role: MemberRole,
    /// Raw power level from `m.room.power_levels` (`0` when default, `i64::MAX` for creator).
    pub power_level: i64,
    /// Invited, not (yet) joined.
    pub invited: bool,
    /// Banned from the room.
    pub banned: bool,
    pub own: bool,
    /// We are allowed to remove this member from the room.
    pub removable: bool,
    /// We are allowed to ban this member from the room.
    pub bannable: bool,
    /// We are allowed to unban this member from the room.
    pub unbannable: bool,
    /// We are allowed to change this member's power level.
    pub role_editable: bool,
}

/// An incoming request to join this room (`m.room.member` with `knock` membership).
/// All text is sanitized for display.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RoomKnockRequest {
    pub user_id: String,
    /// Display name, or the first part of the user id.
    pub name: String,
    pub reason: Option<String>,
    /// Whether we may invite this user (accept the request).
    pub acceptable: bool,
    /// Whether we may kick this user (decline the request).
    pub declinable: bool,
    /// Whether we may ban this user (decline and ban).
    pub bannable: bool,
}

impl std::fmt::Debug for RoomKnockRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoomKnockRequest")
            .field("has_reason", &self.reason.is_some())
            .field("acceptable", &self.acceptable)
            .field("declinable", &self.declinable)
            .field("bannable", &self.bannable)
            .finish_non_exhaustive()
    }
}

/// Who can join the room, normalized from `m.room.join_rules`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RoomJoinRule {
    #[default]
    Invite,
    Knock,
    Public,
    Restricted,
    KnockRestricted,
    Private,
    Custom,
}

impl RoomJoinRule {
    pub const EDITABLE: [Self; 3] = [Self::Invite, Self::Knock, Self::Public];

    pub fn label(self) -> &'static str {
        match self {
            Self::Invite => "Invite only",
            Self::Knock => "Knock to join",
            Self::Public => "Anyone (public)",
            Self::Restricted => "Restricted",
            Self::KnockRestricted => "Knock (restricted)",
            Self::Private => "Private",
            Self::Custom => "Custom",
        }
    }

    pub fn to_ruma(self) -> Option<JoinRule> {
        match self {
            Self::Invite => Some(JoinRule::Invite),
            Self::Knock => Some(JoinRule::Knock),
            Self::Public => Some(JoinRule::Public),
            Self::Restricted | Self::KnockRestricted | Self::Private | Self::Custom => None,
        }
    }
}

impl From<JoinRule> for RoomJoinRule {
    fn from(rule: JoinRule) -> Self {
        match rule {
            JoinRule::Invite => Self::Invite,
            JoinRule::Knock => Self::Knock,
            JoinRule::Public => Self::Public,
            JoinRule::Restricted(_) => Self::Restricted,
            JoinRule::KnockRestricted(_) => Self::KnockRestricted,
            JoinRule::Private => Self::Private,
            _ => Self::Custom,
        }
    }
}

/// Who can read message history (`m.room.history_visibility`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RoomHistory {
    #[default]
    Shared,
    Invited,
    Joined,
    WorldReadable,
    Custom,
}

impl RoomHistory {
    pub const ALL: [Self; 4] = [
        Self::Shared,
        Self::Invited,
        Self::Joined,
        Self::WorldReadable,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Shared => "Shared (all history)",
            Self::Invited => "From invite",
            Self::Joined => "From join",
            Self::WorldReadable => "World readable",
            Self::Custom => "Custom",
        }
    }

    pub fn to_ruma(self) -> Option<HistoryVisibility> {
        match self {
            Self::Shared => Some(HistoryVisibility::Shared),
            Self::Invited => Some(HistoryVisibility::Invited),
            Self::Joined => Some(HistoryVisibility::Joined),
            Self::WorldReadable => Some(HistoryVisibility::WorldReadable),
            Self::Custom => None,
        }
    }
}

impl From<HistoryVisibility> for RoomHistory {
    fn from(vis: HistoryVisibility) -> Self {
        match vis {
            HistoryVisibility::Shared => Self::Shared,
            HistoryVisibility::Invited => Self::Invited,
            HistoryVisibility::Joined => Self::Joined,
            HistoryVisibility::WorldReadable => Self::WorldReadable,
            _ => Self::Custom,
        }
    }
}

/// Power-level gates for the Room Settings panel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RoomSettingsPower {
    pub can_join_rule: bool,
    pub can_history: bool,
    pub can_encrypt: bool,
    pub can_avatar: bool,
    pub can_aliases: bool,
    pub can_permissions: bool,
}

impl RoomSettingsPower {
    fn from_levels(levels: Option<&RoomPowerLevels>, own: &UserId) -> Self {
        let can = |ev: StateEventType| levels.is_some_and(|l| l.user_can_send_state(own, ev));
        Self {
            can_join_rule: can(StateEventType::RoomJoinRules),
            can_history: can(StateEventType::RoomHistoryVisibility),
            can_encrypt: can(StateEventType::RoomEncryption),
            can_avatar: can(StateEventType::RoomAvatar),
            can_aliases: can(StateEventType::RoomCanonicalAlias),
            can_permissions: can(StateEventType::RoomPowerLevels),
        }
    }
}

/// Room-wide action power-level thresholds exposed in the Permissions subview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomPermissions {
    pub send_messages: i64,
    pub invite: i64,
    pub kick: i64,
    pub ban: i64,
    pub redact: i64,
    pub change_settings: i64,
    /// Effective level required to edit `m.room.power_levels` (`state_default` unless overridden).
    pub power_levels_event_level: i64,
    /// Whether `m.room.power_levels` has an explicit override in `events`.
    pub explicit_power_levels_rule: bool,
}

impl Default for RoomPermissions {
    fn default() -> Self {
        Self {
            send_messages: 0,
            invite: 0,
            kick: 50,
            ban: 50,
            redact: 50,
            change_settings: 50,
            power_levels_event_level: 50,
            explicit_power_levels_rule: false,
        }
    }
}

impl RoomPermissions {
    pub fn from_levels(levels: &RoomPowerLevels) -> Self {
        Self {
            send_messages: i64::from(levels.for_message(MessageLikeEventType::RoomMessage)),
            invite: i64::from(levels.invite),
            kick: i64::from(levels.kick),
            ban: i64::from(levels.ban),
            redact: i64::from(levels.redact),
            change_settings: i64::from(levels.state_default),
            power_levels_event_level: i64::from(levels.for_state(StateEventType::RoomPowerLevels)),
            explicit_power_levels_rule: levels
                .events
                .contains_key(&TimelineEventType::RoomPowerLevels),
        }
    }
}

/// Everything the room info view shows. All text is safe to put on the screen.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RoomDetails {
    pub name: String,
    pub topic: String,
    pub alias: Option<String>,
    /// Safe `mxc://` room avatar URL if set.
    pub avatar_url: Option<String>,
    /// Own display name overridden/synced in this room, if set.
    pub own_display_name: Option<String>,
    /// `None`: not known (yet).
    pub encrypted: Option<bool>,
    pub direct: bool,
    pub joined: u64,
    pub invited: u64,
    pub favourite: bool,
    pub low_priority: bool,
    /// `None` for rooms we are not in.
    pub notify: Option<NotifyLevel>,
    pub join_rule: Option<RoomJoinRule>,
    pub history_visibility: Option<RoomHistory>,
    pub canonical_alias: Option<String>,
    pub alt_aliases: Vec<String>,
    pub local_aliases: Vec<String>,
    pub local_aliases_error: Option<String>,
    pub own_homeserver: Option<String>,
    pub settings_power: RoomSettingsPower,
    pub permissions: RoomPermissions,
    pub can_rename: bool,
    pub can_set_topic: bool,
    pub can_invite: bool,
    pub own_power_level: i64,
    pub own_is_creator: bool,
    /// Joined admins or creators, used to guard last-admin self-demotion.
    pub admin_count: usize,
    /// Pending incoming requests to join (filtered to those the viewer can act on).
    pub knock_requests: Vec<RoomKnockRequest>,
    /// Joined members first, then invited, then banned; highest role first.
    pub members: Vec<RoomMember>,
    /// The member list is still being fetched.
    pub members_loading: bool,
    /// The list is known to be incomplete and will stay so (the room is too large to
    /// ask for all of it).
    pub members_partial: bool,
    /// Why the list could not be brought up to date; what is shown is the last known.
    pub members_error: Option<String>,
}

/// Counts only: names and topics do not belong in a log.
impl std::fmt::Debug for RoomDetails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RoomDetails({} members, {} knocks, {} local_aliases, avatar: {}, loading: {})",
            self.members.len(),
            self.knock_requests.len(),
            self.local_aliases.len(),
            self.avatar_url.is_some(),
            self.members_loading
        )
    }
}

pub struct Manager {
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    /// Keeps the open room info view current.
    watcher: Option<JoinHandle<()>>,
    /// Wakes the open room info watcher after local moderation or membership changes.
    refresh: Arc<Refresh>,
    own_room_names: super::people::OwnRoomNames,
    people_poke: Arc<Notify>,
    own_refresh: Arc<watch::Sender<u64>>,
}

/// Wakes the open room info watcher when the watched room changes.
pub struct Refresh {
    tx: watch::Sender<u64>,
    watched: std::sync::Mutex<Option<String>>,
}

impl Refresh {
    fn new() -> Self {
        let (tx, _) = watch::channel(0u64);
        Self {
            tx,
            watched: std::sync::Mutex::new(None),
        }
    }

    fn watch(&self, room_id: &str) -> watch::Receiver<u64> {
        let mut watched = self.watched.lock().unwrap_or_else(|e| e.into_inner());
        *watched = Some(room_id.to_owned());
        self.tx.subscribe()
    }

    fn clear(&self) {
        *self.watched.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn bump(&self, room_id: &str) {
        let is_watched = self
            .watched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_deref()
            == Some(room_id);
        if is_watched {
            self.tx.send_modify(|n| *n = n.wrapping_add(1));
        }
    }
}

pub(super) fn bump_refresh(refresh: &Refresh, room_id: &str) {
    refresh.bump(room_id);
}

impl Manager {
    pub fn new(
        client: Client,
        events: UnboundedSender<MatrixEvent>,
        own_room_names: super::people::OwnRoomNames,
        people_poke: Arc<Notify>,
        own_refresh: Arc<watch::Sender<u64>>,
    ) -> Self {
        Self {
            client,
            events,
            watcher: None,
            refresh: Arc::new(Refresh::new()),
            own_room_names,
            people_poke,
            own_refresh,
        }
    }

    pub fn refresh_signal(&self) -> Arc<Refresh> {
        self.refresh.clone()
    }

    pub fn stop(&mut self) {
        self.refresh.clear();
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
        }
    }

    /// Every command runs in a task of its own: none of them may hold up the session.
    pub fn handle(&mut self, command: ManageCmd) {
        tracing::debug!(?command, "room management");
        let client = self.client.clone();
        let events = self.events.clone();
        let emit = move |event: ManageEvent| {
            let _ = events.send(MatrixEvent::Manage(event));
        };
        match command {
            ManageCmd::Join {
                request,
                target,
                via,
            } => {
                tokio::spawn(async move {
                    let result = join(&client, &target, &via).await;
                    emit(ManageEvent::Opened { request, result });
                });
            }
            ManageCmd::DirectChat {
                request,
                user_id,
                create,
            } => {
                tokio::spawn(async move {
                    emit(match direct_chat(&client, &user_id, create).await {
                        Ok(Some(room_id)) => ManageEvent::Opened {
                            request,
                            result: Ok(room_id),
                        },
                        Ok(None) => ManageEvent::NoDirectChat { request, user_id },
                        Err(error) => ManageEvent::Opened {
                            request,
                            result: Err(error),
                        },
                    });
                });
            }
            ManageCmd::Create {
                request,
                name,
                topic,
                alias,
                encrypted,
            } => {
                tokio::spawn(async move {
                    let result = create(&client, name, topic, alias, encrypted).await;
                    emit(ManageEvent::Opened { request, result });
                });
            }
            ManageCmd::Leave { room_id } => {
                tokio::spawn(async move {
                    let result = leave(&client, &room_id).await;
                    emit(ManageEvent::Left { room_id, result });
                });
            }
            ManageCmd::Preview {
                request,
                target,
                via,
            } => {
                tokio::spawn(async move {
                    let result = super::directory::fetch_preview(&client, &target, via)
                        .await
                        .map(Box::new);
                    emit(ManageEvent::Preview { request, result });
                });
            }
            ManageCmd::Knock {
                request,
                target,
                via,
                reason,
            } => {
                tokio::spawn(async move {
                    let result = super::directory::knock(&client, &target, via, reason).await;
                    emit(ManageEvent::Knocked { request, result });
                });
            }
            ManageCmd::Info { room_id } => {
                self.stop();
                // Subscribe before spawning so changes arriving during the initial
                // store read still mark this watcher's receiver dirty.
                let refresh = self.refresh.watch(&room_id);
                let own_room_names = self.own_room_names.clone();
                self.watcher = Some(tokio::spawn(watch(
                    client,
                    room_id,
                    self.events.clone(),
                    refresh,
                    own_room_names,
                )));
            }
            ManageCmd::StopInfo => self.stop(),
            ManageCmd::Invite { room_id, user_id } => {
                tokio::spawn(async move {
                    let result =
                        with_member(&client, &room_id, &user_id, "invite", async |room, user| {
                            room.invite_user_by_id(user).await
                        })
                        .await;
                    emit(ManageEvent::Done {
                        room_id,
                        change: Change::Invite,
                        result,
                    });
                });
            }
            ManageCmd::Kick {
                room_id,
                user_id,
                reason,
            } => {
                let clean = clean_reason(reason.as_deref());
                self.spawn_member_action(
                    room_id,
                    user_id,
                    "remove",
                    Change::Kick,
                    move |r, u| async move { r.kick_user(&u, clean.as_deref()).await },
                );
            }
            ManageCmd::Ban {
                room_id,
                user_id,
                reason,
            } => {
                let clean = clean_reason(reason.as_deref());
                self.spawn_member_action(
                    room_id,
                    user_id,
                    "ban",
                    Change::Ban,
                    move |r, u| async move { r.ban_user(&u, clean.as_deref()).await },
                );
            }
            ManageCmd::Unban {
                room_id,
                user_id,
                reason,
            } => {
                let clean = clean_reason(reason.as_deref());
                self.spawn_member_action(
                    room_id,
                    user_id,
                    "unban",
                    Change::Unban,
                    move |r, u| async move { r.unban_user(&u, clean.as_deref()).await },
                );
            }
            ManageCmd::AcceptKnock { room_id, user_id } => {
                self.spawn_member_action(
                    room_id,
                    user_id,
                    "accept that join request",
                    Change::AcceptKnock,
                    |r, u| async move { r.invite_user_by_id(&u).await },
                );
            }
            ManageCmd::DeclineKnock { room_id, user_id } => {
                self.spawn_member_action(
                    room_id,
                    user_id,
                    "decline that join request",
                    Change::DeclineKnock,
                    |r, u| async move { r.kick_user(&u, None).await },
                );
            }
            ManageCmd::BanKnock {
                room_id,
                user_id,
                reason,
            } => {
                let clean = clean_reason(reason.as_deref());
                self.spawn_member_action(
                    room_id,
                    user_id,
                    "ban that user",
                    Change::BanKnock,
                    move |r, u| async move { r.ban_user(&u, clean.as_deref()).await },
                );
            }
            ManageCmd::SetRole {
                room_id,
                user_id,
                level,
            } => {
                let refresh = self.refresh.clone();
                let events_tx = self.events.clone();
                tokio::spawn(async move {
                    let result = match Int::new(level) {
                        None => Err("Could not change role: power level out of range.".to_owned()),
                        Some(lvl) => {
                            let ok_room = room(&client, &room_id);
                            let res = with_member(
                                &client,
                                &room_id,
                                &user_id,
                                "change that member's role",
                                async |room, user| {
                                    room.update_power_levels(vec![(user, lvl)])
                                        .await
                                        .map(|_| ())
                                },
                            )
                            .await;
                            if res.is_ok()
                                && let Ok(r) = ok_room
                            {
                                emit_room_power(&events_tx, &r).await;
                                bump_refresh(&refresh, &room_id);
                            }
                            res
                        }
                    };
                    emit(ManageEvent::Done {
                        room_id,
                        change: Change::SetRole,
                        result,
                    });
                });
            }
            ManageCmd::SetName { room_id, name } => {
                tokio::spawn(async move {
                    let result = match room(&client, &room_id) {
                        Ok(room) => room
                            .set_name(sanitize_line(&name))
                            .await
                            .map(|_| ())
                            .map_err(|err| failed("rename the room", &err)),
                        Err(err) => Err(err),
                    };
                    emit(ManageEvent::Done {
                        room_id,
                        change: Change::Name,
                        result,
                    });
                });
            }
            ManageCmd::SetOwnName { room_id, name } => {
                let refresh = self.refresh.clone();
                let own_room_names = self.own_room_names.clone();
                let people_poke = self.people_poke.clone();
                let own_refresh = self.own_refresh.clone();
                tokio::spawn(async move {
                    let result =
                        match super::people::set_room_display_name(&client, &room_id, name).await {
                            Ok(applied) => {
                                own_room_names.set(&room_id, applied);
                                bump_refresh(&refresh, &room_id);
                                own_refresh.send_modify(|n| *n = n.wrapping_add(1));
                                people_poke.notify_one();
                                Ok(())
                            }
                            Err(err) => Err(err),
                        };
                    emit(ManageEvent::Done {
                        room_id,
                        change: Change::OwnName,
                        result,
                    });
                });
            }
            ManageCmd::SetTopic { room_id, topic } => {
                tokio::spawn(async move {
                    let result = match room(&client, &room_id) {
                        Ok(room) => room
                            .set_room_topic(&sanitize(&topic))
                            .await
                            .map(|_| ())
                            .map_err(|err| failed("change the topic", &err)),
                        Err(err) => Err(err),
                    };
                    emit(ManageEvent::Done {
                        room_id,
                        change: Change::Topic,
                        result,
                    });
                });
            }
            ManageCmd::SetFavourite { room_id, on } => {
                self.spawn_tag(
                    room_id,
                    "change the favourites",
                    Change::Favourite(on),
                    move |room| async move { room.set_is_favourite(on, None).await },
                );
            }
            ManageCmd::SetLowPriority { room_id, on } => {
                self.spawn_tag(
                    room_id,
                    "change low priority",
                    Change::LowPriority(on),
                    move |room| async move { room.set_is_low_priority(on, None).await },
                );
            }
            ManageCmd::SetNotify { room_id, level } => {
                tokio::spawn(async move {
                    let result = set_notify(&client, &room_id, level).await;
                    emit(ManageEvent::Done {
                        room_id,
                        change: Change::Notify,
                        result,
                    });
                });
            }
            ManageCmd::SetJoinRule { room_id, rule } => {
                self.spawn_setting(room_id, Change::JoinRule, move |_, room| async move {
                    let target = rule
                        .to_ruma()
                        .ok_or_else(|| "That join rule cannot be edited here.".to_owned())?;
                    room.privacy_settings()
                        .update_join_rule(target)
                        .await
                        .map_err(|err| failed("update who can join", &err))
                });
            }
            ManageCmd::SetHistoryVisibility {
                room_id,
                visibility,
            } => {
                self.spawn_setting(
                    room_id,
                    Change::HistoryVisibility,
                    move |_, room| async move {
                        let target = visibility.to_ruma().ok_or_else(|| {
                            "That history visibility cannot be selected.".to_owned()
                        })?;
                        room.privacy_settings()
                            .update_room_history_visibility(target)
                            .await
                            .map_err(|err| failed("update history visibility", &err))
                    },
                );
            }
            ManageCmd::EnableEncryption { room_id } => {
                self.spawn_setting(
                    room_id,
                    Change::EnableEncryption,
                    move |_, room| async move {
                        if room
                            .latest_encryption_state()
                            .await
                            .is_ok_and(|s| s.is_encrypted())
                        {
                            return Err("Encryption is already enabled in this room.".to_owned());
                        }
                        room.enable_encryption()
                            .await
                            .map_err(|err| failed("enable encryption", &err))
                    },
                );
            }
            ManageCmd::SetRoomAvatar { room_id, path } => {
                self.spawn_setting(
                    room_id,
                    Change::RoomAvatar,
                    move |client, room| async move {
                        let (mime, bytes) =
                            super::people::prepare_avatar_upload(&client, &path).await?;
                        tokio::time::timeout(
                            Duration::from_secs(60),
                            room.upload_avatar(&mime, bytes, None),
                        )
                        .await
                        .map_err(|_| "Uploading the room picture took too long.".to_owned())?
                        .map(|_| ())
                        .map_err(|err| failed("update the room picture", &err))
                    },
                );
            }
            ManageCmd::RemoveRoomAvatar { room_id } => {
                self.spawn_setting(
                    room_id,
                    Change::RemoveRoomAvatar,
                    move |_, room| async move {
                        room.remove_avatar()
                            .await
                            .map(|_| ())
                            .map_err(|err| failed("remove the room picture", &err))
                    },
                );
            }
            ManageCmd::SetAddresses {
                room_id,
                canonical,
                alt_aliases,
            } => {
                self.spawn_setting(room_id, Change::Addresses, move |_, room| async move {
                    let (main, alts) = parse_room_aliases(canonical, alt_aliases)?;
                    room.privacy_settings()
                        .update_canonical_alias(main, alts)
                        .await
                        .map_err(|err| failed("update the room addresses", &err))
                });
            }
            ManageCmd::CreateLocalAlias { room_id, alias } => {
                self.spawn_setting(
                    room_id,
                    Change::CreateLocalAlias,
                    move |client, room| async move {
                        let own = client
                            .user_id()
                            .ok_or_else(|| "Not signed in.".to_owned())?;
                        let alias_id = parse_local_alias(&alias, own.server_name())?;
                        client
                            .create_room_alias(&alias_id, room.room_id())
                            .await
                            .map_err(|err| failed("create that local address", &err.into()))
                    },
                );
            }
            ManageCmd::DeleteLocalAlias { room_id, alias } => {
                self.spawn_setting(
                    room_id,
                    Change::DeleteLocalAlias,
                    move |client, _| async move {
                        let own = client
                            .user_id()
                            .ok_or_else(|| "Not signed in.".to_owned())?;
                        let alias_id = parse_local_alias(&alias, own.server_name())?;
                        client
                            .remove_room_alias(&alias_id)
                            .await
                            .map_err(|err| failed("delete that local address", &err.into()))
                    },
                );
            }
            ManageCmd::SetDirect { room_id, direct } => {
                self.spawn_setting(room_id, Change::Direct(direct), move |_, room| async move {
                    room.set_is_direct(direct)
                        .await
                        .map_err(|err| failed("update the direct-chat setting", &err))
                });
            }
            ManageCmd::SetPermissions {
                room_id,
                permissions,
            } => {
                let events = self.events.clone();
                let refresh = self.refresh.clone();
                let client = self.client.clone();
                tokio::spawn(async move {
                    let result = match room(&client, &room_id) {
                        Ok(room) => {
                            let res = set_room_permissions(&room, &permissions).await;
                            if res.is_ok() {
                                emit_room_power(&events, &room).await;
                                bump_refresh(&refresh, &room_id);
                            }
                            res
                        }
                        Err(err) => Err(err),
                    };
                    let _ = events.send(MatrixEvent::Manage(ManageEvent::Done {
                        room_id,
                        change: Change::Permissions,
                        result,
                    }));
                });
            }
        }
    }

    fn spawn_setting<F, Fut>(&self, room_id: String, change: Change, work: F)
    where
        F: FnOnce(Client, Room) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), String>> + Send,
    {
        let client = self.client.clone();
        let events = self.events.clone();
        let refresh = self.refresh.clone();
        tokio::spawn(async move {
            let result = match room(&client, &room_id) {
                Ok(room) => work(client, room).await,
                Err(err) => Err(err),
            };
            if result.is_ok() {
                bump_refresh(&refresh, &room_id);
            }
            let _ = events.send(MatrixEvent::Manage(ManageEvent::Done {
                room_id,
                change,
                result,
            }));
        });
    }

    fn spawn_tag<F, Fut>(&self, room_id: String, what: &'static str, change: Change, work: F)
    where
        F: FnOnce(Room) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = matrix_sdk::Result<()>> + Send,
    {
        let client = self.client.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let result = match room(&client, &room_id) {
                Ok(room) => work(room).await.map_err(|err| failed(what, &err)),
                Err(err) => Err(err),
            };
            let _ = events.send(MatrixEvent::Manage(ManageEvent::Done {
                room_id,
                change,
                result,
            }));
        });
    }

    fn spawn_member_action<F, Fut>(
        &self,
        room_id: String,
        user_id: String,
        what: &'static str,
        change: Change,
        work: F,
    ) where
        F: FnOnce(Room, OwnedUserId) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = matrix_sdk::Result<()>> + Send,
    {
        let client = self.client.clone();
        let events = self.events.clone();
        let refresh = self.refresh.clone();
        tokio::spawn(async move {
            let result = with_member(&client, &room_id, &user_id, what, async |room, user| {
                work(room.clone(), user.to_owned()).await
            })
            .await;
            if result.is_ok() {
                bump_refresh(&refresh, &room_id);
            }
            let _ = events.send(MatrixEvent::Manage(ManageEvent::Done {
                room_id,
                change,
                result,
            }));
        });
    }
}

fn clean_reason(reason: Option<&str>) -> Option<String> {
    reason.map(sanitize).filter(|s| !crate::text::is_blank(s))
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(super) fn room(client: &Client, room_id: &str) -> Result<Room, String> {
    let id = RoomId::parse(room_id).map_err(|_| "That is not a room id.".to_owned())?;
    client
        .get_room(&id)
        .ok_or_else(|| "This room is not known yet.".to_owned())
}

/// "Could not join: …", with the reason in words a person can act on.
pub(super) fn failed(what: &str, err: &matrix_sdk::Error) -> String {
    // The error's own text can name people, rooms and servers; its kind says enough.
    tracing::warn!(kind = ?err.client_api_error_kind(), "could not {what}");
    format!("Could not {what}: {}", explain(err))
}

/// The server's own words, made safe and short.
fn server_text(err: &matrix_sdk::Error) -> Option<String> {
    let ErrorBody::Standard(body) = &err.as_client_api_error()?.body else {
        return None;
    };
    let text = sanitize_line(body.message.trim());
    (!text.is_empty()).then(|| truncate(&text, SERVER_TEXT))
}

pub(super) fn explain(err: &matrix_sdk::Error) -> String {
    explain_kind(
        err.client_api_error_kind(),
        server_text(err),
        matches!(err, matrix_sdk::Error::Http(http) if http.as_ruma_api_error().is_none()),
        || truncate(&sanitize_line(&err.to_string()), SERVER_TEXT),
    )
}

/// `said`: the server's words; `unreachable`: no answer came from a server at all;
/// `other`: the last resort for errors that are neither.
fn explain_kind(
    kind: Option<&ErrorKind>,
    said: Option<String>,
    unreachable: bool,
    other: impl FnOnce() -> String,
) -> String {
    let with = |ours: &str| match &said {
        Some(said) => format!("{ours} ({said})"),
        None => ours.to_owned(),
    };
    match kind {
        Some(ErrorKind::NotFound) => with("not found"),
        Some(
            ErrorKind::Forbidden
            | ErrorKind::UnableToAuthorizeJoin
            | ErrorKind::UnableToGrantJoin
            | ErrorKind::GuestAccessForbidden
            | ErrorKind::InviteBlocked,
        ) => with("not allowed"),
        Some(ErrorKind::LimitExceeded(_)) => {
            "the server asks to slow down; try again in a moment".to_owned()
        }
        Some(ErrorKind::RoomInUse) => "that address is already taken".to_owned(),
        Some(ErrorKind::BadAlias | ErrorKind::InvalidParam | ErrorKind::BadJson) => {
            with("the server did not accept it")
        }
        Some(_) => said.unwrap_or_else(|| "the server refused".to_owned()),
        None if unreachable => "the server could not be reached".to_owned(),
        None => other(),
    }
}

/// Notification settings fail without saying why; none of it is worth quoting.
fn explain_notify(err: &NotificationSettingsError) -> &'static str {
    match err {
        NotificationSettingsError::UnableToAddPushRule
        | NotificationSettingsError::UnableToRemovePushRule
        | NotificationSettingsError::UnableToUpdatePushRule
        | NotificationSettingsError::UnableToSavePushRules => {
            "the server did not take the change; try again in a moment"
        }
        _ => "this room's notification rules could not be read",
    }
}

async fn join(client: &Client, target: &str, via: &[String]) -> Result<String, String> {
    let not_an_address = || "Could not join: that is not a room address.".to_owned();
    let target = RoomOrAliasId::parse(target).map_err(|_| not_an_address())?;
    let names = via_names(via);
    let mut via: Vec<OwnedServerName> = Vec::new();
    for name in names {
        if let Ok(server) = ServerName::parse(name) {
            via.push(server);
        }
    }
    // Where does it lead? A room we are in already is simply shown.
    let room_id = match <&RoomId>::try_from(&*target) {
        Ok(room_id) => room_id.to_owned(),
        Err(alias) => {
            let found = client
                .resolve_room_alias(alias)
                .await
                .map_err(|err| failed("join", &err.into()))?;
            for server in found.servers {
                if via.len() < VIA_MAX && !via.contains(&server) {
                    via.push(server);
                }
            }
            found.room_id
        }
    };
    if client
        .get_room(&room_id)
        .is_some_and(|room| room.state() == RoomState::Joined)
    {
        return Ok(room_id.to_string());
    }
    // By its id: the address has been looked up once already.
    let target: OwnedRoomOrAliasId = room_id.into();
    let joined = client
        .join_room_by_id_or_alias(&target, &via)
        .await
        .map_err(|err| failed("join", &err))?;
    Ok(joined.room_id().to_string())
}

/// The first few distinct names.
fn via_names(via: &[String]) -> Vec<&str> {
    let mut names: Vec<&str> = Vec::new();
    for name in via {
        if names.len() < VIA_MAX && !names.contains(&name.as_str()) {
            names.push(name);
        }
    }
    names
}

/// The room of the direct chat with `user_id`; `None` if there is none and none was
/// to be made.
async fn direct_chat(
    client: &Client,
    user_id: &str,
    create: bool,
) -> Result<Option<String>, String> {
    let user = UserId::parse(user_id)
        .map_err(|_| "Could not start the chat: that is not a user ID.".to_owned())?;
    if client.user_id() == Some(&*user) {
        return Err("That is your own user ID.".to_owned());
    }
    if let Some(existing) = client.get_dm_room(&user) {
        return Ok(Some(existing.room_id().to_string()));
    }
    // A chat they have invited us to is the chat with them as well: opening it (where
    // it can be accepted) beats making a second one.
    for invited in client.invited_rooms() {
        let marked = invited
            .direct_targets()
            .contains(<&DirectUserIdentifier>::from(&*user));
        let from_them = invited.is_direct().await.unwrap_or(false)
            && invited
                .invite_details()
                .await
                .is_ok_and(|invite| invite.inviter_id == user);
        if marked || from_them {
            return Ok(Some(invited.room_id().to_string()));
        }
    }
    if !create {
        return Ok(None);
    }
    // Encrypted, marked as direct, and the other person is invited.
    let created = client
        .create_dm(&user)
        .await
        .map_err(|err| failed("start the chat", &err))?;
    Ok(Some(created.room_id().to_string()))
}

async fn create(
    client: &Client,
    name: String,
    topic: Option<String>,
    alias: Option<String>,
    encrypted: bool,
) -> Result<String, String> {
    let mut request = CreateRequest::new();
    request.name = Some(sanitize_line(&name));
    request.topic = topic.map(|topic| sanitize(&topic));
    match alias {
        // Anyone may join by the address. Not listed in the server's directory:
        // publishing a room there is a decision of its own.
        Some(alias) => {
            request.preset = Some(RoomPreset::PublicChat);
            request.room_alias_name = Some(alias);
        }
        None => {
            request.preset = Some(RoomPreset::PrivateChat);
            if encrypted {
                request.initial_state = vec![
                    InitialStateEvent::with_empty_state_key(
                        RoomEncryptionEventContent::with_recommended_defaults(),
                    )
                    .to_raw_any(),
                ];
            }
        }
    }
    let created = client
        .create_room(request)
        .await
        .map_err(|err| failed("create the room", &err))?;
    Ok(created.room_id().to_string())
}

async fn leave(client: &Client, room_id: &str) -> Result<(), String> {
    let room = room(client, room_id)?;
    if matches!(
        room.state(),
        RoomState::Joined | RoomState::Invited | RoomState::Knocked
    ) {
        room.leave().await.map_err(|err| failed("leave", &err))?;
    }
    cleanup_left_room(client, &room).await;
    Ok(())
}

pub(super) async fn cleanup_left_room(client: &Client, room: &Room) {
    // Forgetting does not remove the room's unsent text from the store. The UI asks
    // for that (with the threads it knows) when the room leaves its list; the room's
    // own draft goes here already, while there still is a room to ask.
    if matches!(room.state(), RoomState::Left | RoomState::Banned)
        && let Err(err) = room.clear_composer_draft(None).await
    {
        tracing::warn!(?err, "could not remove the draft of a room after leaving");
    }
    // Clear the room's event cache while it still exists in `base_client`: calling
    // `room.forget()` first drops it from `base_client`, causing `event_cache().forget_room()`
    // inside `room.forget()` to fail with `ClientDropped` and leave a stale cache entry
    // that breaks later `clear_all_rooms()` calls on ignore/unignore.
    if matches!(room.state(), RoomState::Left | RoomState::Banned) {
        let _ = client.event_cache().forget_room(room.room_id()).await;
        if let Err(err) = room.forget().await {
            tracing::warn!(
                kind = ?err.client_api_error_kind(),
                "could not forget the room after leaving"
            );
        }
    }
}

async fn with_member<F>(
    client: &Client,
    room_id: &str,
    user_id: &str,
    what: &str,
    work: F,
) -> Result<(), String>
where
    F: AsyncFnOnce(&Room, &UserId) -> matrix_sdk::Result<()>,
{
    let room = room(client, room_id)?;
    let user =
        UserId::parse(user_id).map_err(|_| format!("Could not {what}: that is not a user ID."))?;
    work(&room, &user).await.map_err(|err| failed(what, &err))
}

async fn set_notify(client: &Client, room_id: &str, level: NotifyLevel) -> Result<(), String> {
    let room = room(client, room_id)?;
    let mode = match level {
        NotifyLevel::All => RoomNotificationMode::AllMessages,
        NotifyLevel::Mentions => RoomNotificationMode::MentionsAndKeywordsOnly,
        NotifyLevel::Mute => RoomNotificationMode::Mute,
    };
    client
        .notification_settings()
        .await
        .set_room_notification_mode(room.room_id(), mode)
        .await
        .map_err(|err| {
            tracing::warn!("could not change the room's notifications");
            format!(
                "Could not change the notifications: {}",
                explain_notify(&err)
            )
        })
}

// ----- the details of a room ---------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MemberStatus {
    Joined,
    Invited,
    Banned,
}

type EntryStatus = MemberStatus;

/// One person in the room, before roles are worked out.
#[derive(Clone, PartialEq, Eq)]
struct Entry {
    user: OwnedUserId,
    name: String,
    status: EntryStatus,
}

pub(super) async fn power_for_room(room: &Room) -> RoomPower {
    if room.state() != RoomState::Joined {
        return RoomPower::default();
    }
    let pl = room.power_levels_or_default().await;
    let own = room.own_user_id();
    let own_pl = pl.for_user(own);
    let own_is_creator = matches!(own_pl, UserPowerLevel::Infinite);
    let own_level = match own_pl {
        UserPowerLevel::Infinite => i64::MAX,
        UserPowerLevel::Int(v) => i64::from(v),
        _ => 0,
    };
    let creators = pl
        .rules
        .privileged_creators
        .as_ref()
        .map(|list| list.iter().map(|u| u.to_string()).collect())
        .unwrap_or_default();
    let member_levels = pl
        .users
        .iter()
        .map(|(u, lvl)| (u.to_string(), i64::from(*lvl)))
        .collect();
    RoomPower {
        own_level,
        own_is_creator,
        can_redact_other: pl.user_can_redact_event_of_other(own),
        can_ban: pl.user_can_ban(own),
        can_unban: pl.user_can_unban(own),
        can_kick: pl.user_can_kick(own),
        can_pin: pl.user_can_send_state(own, StateEventType::RoomPinnedEvents),
        can_change_roles: pl.user_can_send_state(own, StateEventType::RoomPowerLevels),
        can_rename: pl.user_can_send_state(own, StateEventType::RoomName),
        can_set_topic: pl.user_can_send_state(own, StateEventType::RoomTopic),
        users_default: i64::from(pl.users_default),
        creators,
        member_levels,
    }
}

pub(super) async fn emit_room_power(events: &UnboundedSender<MatrixEvent>, room: &Room) {
    let power = power_for_room(room).await;
    let _ = events.send(MatrixEvent::RoomPower {
        room_id: room.room_id().to_string(),
        power,
    });
}

/// The member list as the watcher holds it between looks at the room.
#[derive(Default)]
struct MemberList {
    entries: Vec<Entry>,
    /// `entries` is a complete snapshot of the room (not lazy-loaded, waiting,
    /// partial, or stale after an error).
    complete: bool,
    /// The room's counts when `entries` was last read from the store.
    counts: Option<(u64, u64)>,
    /// Counts the server's own list did not match either (or could not be had for):
    /// no use asking again until they change.
    gave_up: Option<(u64, u64)>,
    asked: Option<Instant>,
    error: Option<String>,
    /// `entries` with roles, sorted, and what it was made from.
    shown: Vec<RoomMember>,
    shown_from: Option<(Vec<Entry>, String)>,
}

impl MemberList {
    fn agrees(&self, (joined, invited): (u64, u64)) -> bool {
        entries_agree(&self.entries, (joined, invited))
    }

    /// Fills the details' member list, admin count, error, and (once the list is
    /// complete) counts so the header never disagrees with the people shown.
    fn apply(
        &mut self,
        current: &mut RoomDetails,
        levels: Option<&RoomPowerLevels>,
        own: &UserId,
        own_name: Option<Option<String>>,
    ) {
        if let Some(cached) = own_name {
            let synced_name = cached.unwrap_or_else(|| own.localpart().to_owned());
            for entry in &mut self.entries {
                if entry.user == own {
                    entry.name.clone_from(&synced_name);
                }
            }
        }
        current.members = self.present(levels, own);
        current.admin_count = current
            .members
            .iter()
            .filter(|m| !m.invited && !m.banned && m.role >= MemberRole::Admin)
            .count();
        current.members_error.clone_from(&self.error);
        if self.complete && !current.members_loading && !current.members_partial {
            (current.joined, current.invited) = entry_counts(&self.entries);
        }
    }

    /// The list for the screen. Roles and sorting are only redone when the people or
    /// the power levels changed.
    fn present(&mut self, levels: Option<&RoomPowerLevels>, own: &UserId) -> Vec<RoomMember> {
        let key = format!("{levels:?}");
        let fresh =
            matches!(&self.shown_from, Some((entries, k)) if *entries == self.entries && *k == key);
        if !fresh {
            self.shown = present(&self.entries, levels, own);
            self.shown_from = Some((self.entries.clone(), key));
        }
        self.shown.clone()
    }
}

fn entry_counts(entries: &[Entry]) -> (u64, u64) {
    let mut joined = 0;
    let mut invited = 0;
    for entry in entries {
        match entry.status {
            EntryStatus::Joined => joined += 1,
            EntryStatus::Invited => invited += 1,
            EntryStatus::Banned => {}
        }
    }
    (joined, invited)
}

/// Whether a list has as many joined and invited people as the room says it has.
fn entries_agree(entries: &[Entry], counts: (u64, u64)) -> bool {
    entry_counts(entries) == counts
}

pub(super) fn role(level: UserPowerLevel) -> MemberRole {
    match level {
        UserPowerLevel::Infinite => MemberRole::Owner,
        UserPowerLevel::Int(level) if level >= Int::from(100) => MemberRole::Admin,
        UserPowerLevel::Int(level) if level >= Int::from(50) => MemberRole::Moderator,
        _ => MemberRole::User,
    }
}

pub(super) fn numeric_level(level: UserPowerLevel) -> i64 {
    match level {
        UserPowerLevel::Infinite => i64::MAX,
        UserPowerLevel::Int(level) => i64::from(level),
        _ => 0,
    }
}

pub(super) fn build_member(
    user: &UserId,
    name: &str,
    status: MemberStatus,
    levels: Option<&RoomPowerLevels>,
    own: &UserId,
) -> RoomMember {
    let level = levels.map_or(UserPowerLevel::Int(0.into()), |l| l.for_user(user));
    let invited = status == EntryStatus::Invited;
    let banned = status == EntryStatus::Banned;
    let not_self = user != own;
    RoomMember {
        user_id: sanitize_line(user.as_str()),
        name: sanitize_line(name),
        role: role(level),
        power_level: numeric_level(level),
        invited,
        banned,
        own: user == own,
        removable: !banned && not_self && levels.is_some_and(|l| l.user_can_kick_user(own, user)),
        bannable: !banned && not_self && levels.is_some_and(|l| l.user_can_ban_user(own, user)),
        unbannable: banned && not_self && levels.is_some_and(|l| l.user_can_unban_user(own, user)),
        role_editable: !banned
            && !invited
            && levels.is_some_and(|l| l.user_can_change_user_power_level(own, user)),
    }
}

/// Roles and order: joined, invited, then banned; highest role first, then by name.
fn present(entries: &[Entry], levels: Option<&RoomPowerLevels>, own: &UserId) -> Vec<RoomMember> {
    let mut members: Vec<RoomMember> = entries
        .iter()
        .map(|entry| build_member(&entry.user, &entry.name, entry.status, levels, own))
        .collect();
    members.sort_by_cached_key(|member| {
        let bucket = if member.banned {
            2u8
        } else if member.invited {
            1
        } else {
            0
        };
        (
            bucket,
            std::cmp::Reverse(member.role),
            std::cmp::Reverse(member.power_level),
            member.name.to_lowercase(),
            member.user_id.clone(),
        )
    });
    members
}

/// The members the store knows. The sync only brings the ones it thinks are needed,
/// so this can be behind the room's own counts.
async fn stored_members(room: &Room) -> Result<Vec<Entry>, String> {
    let wanted = RoomMemberships::JOIN | RoomMemberships::INVITE | RoomMemberships::BAN;
    let members = room.members_no_sync(wanted).await.map_err(|err| {
        tracing::warn!(%err, "could not read the room's members");
        "The member list could not be read.".to_owned()
    })?;
    Ok(members
        .iter()
        .filter_map(|member| {
            let status = match *member.membership() {
                MembershipState::Join => EntryStatus::Joined,
                MembershipState::Invite => EntryStatus::Invited,
                MembershipState::Ban => EntryStatus::Banned,
                _ => return None,
            };
            Some(Entry {
                user: member.user_id().to_owned(),
                name: member.name().to_owned(),
                status,
            })
        })
        .collect())
}

/// The member list as the server has it now. Asked for directly and kept out of the
/// SDK's store on purpose: a full reload there makes the SDK throw away the room's
/// outgoing encryption session, which is too high a price for looking at a list.
struct ServerMembers {
    entries: Vec<Entry>,
    knocks: Vec<RawKnock>,
}

fn parse_member_events(chunk: &[Raw<RoomMemberEvent>]) -> ServerMembers {
    let mut entries = Vec::new();
    let mut knocks = Vec::new();
    for raw in chunk {
        // One event the server got wrong does not cost the rest of the list.
        let Ok(event) = raw.deserialize() else {
            continue;
        };
        let user = event.state_key().to_owned();
        let status = match event.membership() {
            MembershipState::Join => EntryStatus::Joined,
            MembershipState::Invite => EntryStatus::Invited,
            MembershipState::Ban => EntryStatus::Banned,
            MembershipState::Knock => {
                let (ts, display_name, reason) = match event.as_original() {
                    Some(orig) => (
                        Some(orig.origin_server_ts.get()),
                        orig.content.displayname.as_deref(),
                        orig.content.reason.as_deref(),
                    ),
                    None => (None, None, None),
                };
                knocks.push(raw_knock(ts, user, display_name, reason));
                continue;
            }
            _ => continue,
        };
        let name = event
            .as_original()
            .and_then(|event| event.content.displayname.clone())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| user.localpart().to_owned());
        entries.push(Entry { user, name, status });
    }
    sort_raw_knocks(&mut knocks);
    ServerMembers { entries, knocks }
}

async fn fetch_member_events(
    client: &Client,
    room: &Room,
    membership: Option<MembershipState>,
) -> Result<ServerMembers, String> {
    let mut request = get_member_events::v3::Request::new(room.room_id().to_owned());
    request.membership = membership;
    let response = tokio::time::timeout(MEMBERS_TIMEOUT, client.send(request))
        .await
        .map_err(|_| "The member list took too long to arrive.".to_owned())?
        .map_err(|err| failed("load the member list", &err.into()))?;
    Ok(parse_member_events(&response.chunk))
}

async fn server_members(client: &Client, room: &Room) -> Result<ServerMembers, String> {
    fetch_member_events(client, room, None).await
}

async fn server_knocks(client: &Client, room: &Room) -> Result<Vec<RawKnock>, String> {
    fetch_member_events(client, room, Some(MembershipState::Knock))
        .await
        .map(|s| s.knocks)
}

#[derive(Clone, PartialEq, Eq)]
struct RawKnock {
    timestamp: u64,
    user: OwnedUserId,
    name: String,
    reason: Option<String>,
}

fn raw_knock(
    timestamp: Option<UInt>,
    user: OwnedUserId,
    display_name: Option<&str>,
    reason: Option<&str>,
) -> RawKnock {
    let name = display_name
        .map(sanitize_line)
        .filter(|s| !crate::text::is_blank(s))
        .unwrap_or_else(|| sanitize_line(user.localpart()));
    let reason = reason
        .map(|r| sanitize_line(r).trim().to_owned())
        .filter(|s| !s.is_empty());
    RawKnock {
        timestamp: timestamp.map_or(0, u64::from),
        user,
        name,
        reason,
    }
}

fn sort_raw_knocks(knocks: &mut [RawKnock]) {
    knocks.sort_by(|a, b| {
        a.timestamp
            .cmp(&b.timestamp)
            .then_with(|| a.user.cmp(&b.user))
    });
}

/// Everything about the room but its members.
async fn stored_knocks(room: &Room) -> Vec<RawKnock> {
    let Ok(members) = room.members_no_sync(RoomMemberships::KNOCK).await else {
        return Vec::new();
    };
    let mut out: Vec<RawKnock> = members
        .into_iter()
        .map(|m| {
            raw_knock(
                m.event().timestamp(),
                m.user_id().to_owned(),
                m.display_name(),
                m.event().reason(),
            )
        })
        .collect();
    sort_raw_knocks(&mut out);
    out
}

fn build_knock_requests(
    raw: &[RawKnock],
    levels: Option<&RoomPowerLevels>,
    own: &UserId,
) -> Vec<RoomKnockRequest> {
    raw.iter()
        .filter_map(|k| {
            let not_self = k.user != own;
            let acceptable = not_self && levels.is_some_and(|l| l.user_can_invite(own));
            let declinable = not_self && levels.is_some_and(|l| l.user_can_kick_user(own, &k.user));
            let bannable = not_self && levels.is_some_and(|l| l.user_can_ban_user(own, &k.user));
            (acceptable || declinable || bannable).then(|| RoomKnockRequest {
                user_id: sanitize_line(k.user.as_str()),
                name: k.name.clone(),
                reason: k.reason.clone(),
                acceptable,
                declinable,
                bannable,
            })
        })
        .collect()
}

async fn head(
    room: &Room,
    levels: Option<&RoomPowerLevels>,
    own_room_names: &super::people::OwnRoomNames,
    raw_knocks: &[RawKnock],
) -> RoomDetails {
    let joined = room.state() == RoomState::Joined;
    let own = room.own_user_id();
    let can = |check: &dyn Fn(&RoomPowerLevels) -> bool| levels.is_some_and(check);
    let name = match room.cached_display_name() {
        Some(name) => name.to_string(),
        None => room
            .display_name()
            .await
            .map(|name| name.to_string())
            .unwrap_or_else(|_| room.room_id().to_string()),
    };
    let encrypted = if joined {
        room.latest_encryption_state()
            .await
            .ok()
            .map(|state| state.is_encrypted())
    } else {
        None
    };
    let notify = match room.notification_mode().await {
        Some(RoomNotificationMode::AllMessages) => Some(NotifyLevel::All),
        Some(RoomNotificationMode::MentionsAndKeywordsOnly) => Some(NotifyLevel::Mentions),
        Some(RoomNotificationMode::Mute) => Some(NotifyLevel::Mute),
        None => None,
    };
    let own_pl = levels.map(|l| l.for_user(own));
    let own_is_creator = matches!(own_pl, Some(UserPowerLevel::Infinite));
    let own_power_level = own_pl.map_or(0, numeric_level);
    let own_display_name = if joined {
        if let Some(cached) = own_room_names.get(room.room_id().as_str()) {
            cached
        } else {
            room.get_member_no_sync(own)
                .await
                .ok()
                .flatten()
                .and_then(|member| member.display_name().map(sanitize_line))
                .filter(|name| !crate::text::is_blank(name))
        }
    } else {
        None
    };
    let canonical_alias = room.canonical_alias().map(|a| sanitize_line(a.as_str()));
    let alt_aliases: Vec<String> = room
        .alt_aliases()
        .into_iter()
        .map(|a| sanitize_line(a.as_str()))
        .collect();
    RoomDetails {
        name: sanitize_line(&name),
        topic: sanitize(room.topic().unwrap_or_default().trim()),
        alias: canonical_alias
            .clone()
            .or_else(|| alt_aliases.first().cloned()),
        avatar_url: room.avatar_url().and_then(|u| super::safety::safe_mxc(&u)),
        own_display_name,
        encrypted,
        direct: room.is_direct().await.unwrap_or(false),
        joined: room.joined_members_count(),
        invited: room.invited_members_count(),
        favourite: room.is_favourite(),
        low_priority: room.is_low_priority(),
        notify,
        join_rule: room.join_rule().map(RoomJoinRule::from),
        history_visibility: joined.then(|| RoomHistory::from(room.history_visibility_or_default())),
        canonical_alias,
        alt_aliases,
        own_homeserver: Some(own.server_name().to_string()),
        settings_power: RoomSettingsPower::from_levels(levels, own),
        permissions: levels.map(RoomPermissions::from_levels).unwrap_or_default(),
        can_rename: can(&|levels| levels.user_can_send_state(own, StateEventType::RoomName)),
        can_set_topic: can(&|levels| levels.user_can_send_state(own, StateEventType::RoomTopic)),
        can_invite: can(&|levels| levels.user_can_invite(own)),
        own_power_level,
        own_is_creator,
        knock_requests: if joined {
            build_knock_requests(raw_knocks, levels, own)
        } else {
            Vec::new()
        },
        ..RoomDetails::default()
    }
}

fn parse_local_alias(raw: &str, own_server: &ServerName) -> Result<OwnedRoomAliasId, String> {
    let cleaned = sanitize_line(raw).trim().to_owned();
    let alias = RoomAliasId::parse(&cleaned)
        .map_err(|_| "Enter a room address like #name:server.".to_owned())?;
    if alias.server_name() != own_server {
        return Err(format!(
            "Local addresses must use your homeserver (:{own_server})."
        ));
    }
    Ok(alias)
}

fn parse_room_aliases(
    canonical: Option<String>,
    alt_aliases: Vec<String>,
) -> Result<(Option<OwnedRoomAliasId>, Vec<OwnedRoomAliasId>), String> {
    let main = match canonical.map(|s| sanitize_line(&s).trim().to_owned()) {
        Some(s) if !s.is_empty() => Some(
            RoomAliasId::parse(&s)
                .map_err(|_| "That main address is not a valid room address.".to_owned())?,
        ),
        _ => None,
    };
    let mut alts = Vec::new();
    for raw in alt_aliases {
        let cleaned = sanitize_line(&raw).trim().to_owned();
        if cleaned.is_empty() {
            continue;
        }
        let parsed = RoomAliasId::parse(&cleaned)
            .map_err(|_| "An alternate address is not a valid room address.".to_owned())?;
        if main.as_ref() != Some(&parsed) && !alts.contains(&parsed) {
            alts.push(parsed);
        }
    }
    Ok((main, alts))
}

async fn fetch_local_aliases(client: &Client, room: &Room) -> Result<Vec<String>, String> {
    let req = AliasesRequest::new(room.room_id().to_owned());
    let resp = tokio::time::timeout(Duration::from_secs(5), client.send(req))
        .await
        .map_err(|_| "Asking for local room addresses took too long.".to_owned())?
        .map_err(|err| failed("list local addresses", &err.into()))?;
    let mut aliases: Vec<String> = resp
        .aliases
        .into_iter()
        .map(|a| sanitize_line(a.as_str()))
        .collect();
    aliases.sort();
    aliases.dedup();
    Ok(aliases)
}

pub(super) fn prepare_permissions_update(
    pl: &RoomPowerLevels,
    own: &UserId,
    next: &RoomPermissions,
) -> Result<RoomPowerLevelsEventContent, String> {
    if !pl.user_can_send_state(own, StateEventType::RoomPowerLevels) {
        return Err("You do not have permission to change room permissions.".to_owned());
    }
    let current = RoomPermissions::from_levels(pl);
    let own_pl = pl.for_user(own);
    let own_is_creator = matches!(own_pl, UserPowerLevel::Infinite);
    let own_level = numeric_level(own_pl);
    let mut updated = pl.clone();
    // Room v12+ forbids listing privileged creators in `m.room.power_levels`'s `users` map.
    if let Some(creators) = &updated.rules.privileged_creators {
        for creator in creators {
            updated.users.remove(creator);
        }
    }
    let convert = |value: i64| -> Result<Int, String> {
        if !own_is_creator && value > own_level {
            return Err(format!(
                "You cannot set a permission above your own level ({own_level})."
            ));
        }
        Int::try_from(value).map_err(|_| "That permission level is out of range.".to_owned())
    };
    if next.send_messages != current.send_messages {
        updated.events_default = convert(next.send_messages)?;
        updated.events.remove(&TimelineEventType::RoomMessage);
    }
    if next.invite != current.invite {
        updated.invite = convert(next.invite)?;
    }
    if next.kick != current.kick {
        updated.kick = convert(next.kick)?;
    }
    if next.ban != current.ban {
        updated.ban = convert(next.ban)?;
    }
    if next.redact != current.redact {
        updated.redact = convert(next.redact)?;
    }
    if next.change_settings != current.change_settings {
        updated.state_default = convert(next.change_settings)?;
    }
    if !own_is_creator && !updated.user_can_send_state(own, StateEventType::RoomPowerLevels) {
        return Err("That change would lock you out of editing room permissions.".to_owned());
    }
    RoomPowerLevelsEventContent::try_from(updated)
        .map_err(|err| format!("Could not build room permissions: {err}"))
}

async fn set_room_permissions(room: &Room, next: &RoomPermissions) -> Result<(), String> {
    let pl = room
        .power_levels()
        .await
        .map_err(|err| failed("read room permissions", &err.into()))?;
    let content = prepare_permissions_update(&pl, room.own_user_id(), next)?;
    room.send_state_event(content)
        .await
        .map(|_| ())
        .map_err(|err| failed("update room permissions", &err))
}

/// What to do about the member list, given the room's counts.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// The list is as long as the room says.
    Fine,
    /// Ask the server now.
    Ask,
    /// Ask the server, but not before this much time has passed.
    Wait(Duration),
    /// Too many to ask for; what is known here has to do.
    TooLarge,
    /// Asking did not help for these counts.
    GaveUp,
}

fn plan(
    agrees: bool,
    counts: (u64, u64),
    gave_up: Option<(u64, u64)>,
    since_asked: Option<Duration>,
) -> Plan {
    let total = counts.0 + counts.1;
    if agrees {
        Plan::Fine
    } else if total > MAX_MEMBERS {
        Plan::TooLarge
    } else if gave_up == Some(counts) {
        Plan::GaveUp
    } else {
        match since_asked {
            Some(since) if total > LARGE_ROOM && since < LARGE_ROOM_REFETCH => {
                Plan::Wait(LARGE_ROOM_REFETCH - since)
            }
            _ => Plan::Ask,
        }
    }
}

/// Sends the room's details: at once from what is known here, again when the member
/// list had to be asked for, and then whenever something that is shown changes.
async fn watch(
    client: Client,
    room_id: String,
    events: UnboundedSender<MatrixEvent>,
    mut refresh: watch::Receiver<u64>,
    own_room_names: super::people::OwnRoomNames,
) {
    let emit = |result: Result<RoomDetails, String>| {
        events
            .send(MatrixEvent::Manage(ManageEvent::Info {
                room_id: room_id.clone(),
                result: result.map(Box::new),
            }))
            .is_ok()
    };
    let room = match room(&client, &room_id) {
        Ok(room) => room,
        Err(err) => {
            emit(Err(err));
            return;
        }
    };
    let own = room.own_user_id().to_owned();
    let mut changes = room.subscribe_info();
    let mut list = MemberList::default();
    let mut knocks_loaded = false;
    let mut force_knocks = false;
    let mut raw_knocks: Vec<RawKnock> = Vec::new();
    let mut sent: Option<RoomDetails> = None;
    let mut force_refresh = false;
    let mut local_aliases: Vec<String> = Vec::new();
    let mut local_aliases_error: Option<String> = None;
    let mut local_aliases_loaded = false;
    let mut force_aliases = false;
    loop {
        if refresh.has_changed().unwrap_or(false) {
            force_refresh = true;
            force_aliases = true;
            force_knocks = true;
        }
        refresh.borrow_and_update();
        // Looked at afresh every time: an invitation may have been accepted meanwhile.
        let joined = match room.state() {
            RoomState::Left | RoomState::Banned => return,
            state => state == RoomState::Joined,
        };
        let levels = if joined {
            Some(room.power_levels_or_default().await)
        } else {
            None
        };
        if joined && !knocks_loaded && raw_knocks.is_empty() {
            raw_knocks = stored_knocks(&room).await;
        } else if !joined {
            knocks_loaded = false;
            force_knocks = false;
            raw_knocks.clear();
            local_aliases.clear();
            local_aliases_error = None;
            local_aliases_loaded = false;
            force_aliases = false;
        }
        let mut current = head(&room, levels.as_ref(), &own_room_names, &raw_knocks).await;
        current.local_aliases = local_aliases.clone();
        current.local_aliases_error = local_aliases_error.clone();
        let counts = (current.joined, current.invited);
        let mut wait = None;
        if joined {
            // The room's info changes with every message. The members are only read
            // again when their number changes, and never in rooms too large to list.
            let unread = list.counts.is_none();
            let too_large = counts.0 + counts.1 > MAX_MEMBERS;
            if unread || (list.counts != Some(counts) && !too_large) {
                list.counts = Some(counts);
                match stored_members(&room).await {
                    // A list that is behind does not replace one that was right.
                    Ok(entries) if unread || entries_agree(&entries, counts) => {
                        list.entries = entries;
                        list.error = None;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        list.error = Some(error);
                        list.complete = false;
                    }
                }
            }
            let since = list.asked.map(|at| at.elapsed());
            if force_refresh && !too_large {
                force_refresh = false;
                current.members_loading = true;
                list.complete = false;
            } else {
                force_refresh = false;
                match plan(list.agrees(counts), counts, list.gave_up, since) {
                    Plan::Fine => {
                        list.error = None;
                        list.complete = true;
                    }
                    Plan::GaveUp => {}
                    Plan::TooLarge => {
                        current.members_partial = true;
                        list.complete = false;
                    }
                    Plan::Wait(time) => {
                        wait = Some(time);
                        list.complete = false;
                    }
                    Plan::Ask => {
                        current.members_loading = true;
                        list.complete = false;
                    }
                }
            }
        } else {
            force_refresh = false;
            list = MemberList::default();
        }
        list.apply(
            &mut current,
            levels.as_ref(),
            &own,
            own_room_names.get(&room_id),
        );
        let ask = current.members_loading;
        let ask_knocks = joined && !ask && (!knocks_loaded || force_knocks);
        let ask_aliases = joined && (!local_aliases_loaded || force_aliases);
        force_knocks = false;
        force_aliases = false;
        if sent.as_ref() != Some(&current) {
            if !emit(Ok(current.clone())) {
                return;
            }
            sent = Some(current.clone());
        }
        if ask || ask_knocks || ask_aliases {
            if ask {
                list.asked = Some(Instant::now());
                match server_members(&client, &room).await {
                    Ok(server) => {
                        list.entries = server.entries;
                        raw_knocks = server.knocks;
                        knocks_loaded = true;
                        list.error = None;
                        list.complete = true;
                        if !list.agrees(counts) {
                            list.gave_up = Some(counts);
                        }
                    }
                    // What is shown stays; the reason goes next to it.
                    Err(error) => {
                        list.error = Some(error);
                        list.complete = false;
                        list.gave_up = Some(counts);
                    }
                }
                current.members_loading = false;
                list.apply(
                    &mut current,
                    levels.as_ref(),
                    &own,
                    own_room_names.get(&room_id),
                );
            } else if ask_knocks {
                knocks_loaded = true;
                if let Ok(knocks) = server_knocks(&client, &room).await {
                    raw_knocks = knocks;
                }
            }
            if joined {
                current.knock_requests = build_knock_requests(&raw_knocks, levels.as_ref(), &own);
            }
            if ask_aliases {
                local_aliases_loaded = true;
                match fetch_local_aliases(&client, &room).await {
                    Ok(aliases) => {
                        local_aliases = aliases;
                        local_aliases_error = None;
                    }
                    Err(err) => {
                        local_aliases_error = Some(err);
                    }
                }
                current.local_aliases = local_aliases.clone();
                current.local_aliases_error = local_aliases_error.clone();
            }
            if !emit(Ok(current.clone())) {
                return;
            }
            sent = Some(current);
        }
        // Until the room changes, a moderation event or knock arrives, or a postponed fetch is due.
        let deadline = wait.map(|time| tokio::time::Instant::now() + time);
        let burst = tokio::select! {
            change = changes.next() => {
                if change.is_none() {
                    return;
                }
                true
            }
            changed = refresh.changed() => {
                if changed.is_err() {
                    return;
                }
                force_refresh = true;
                force_aliases = true;
                force_knocks = true;
                false
            }
            () = async {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => false,
        };
        if burst {
            // Changes come in bursts (a sync brings several); one look afterwards is enough.
            tokio::time::sleep(Duration::from_millis(300)).await;
            while futures::FutureExt::now_or_never(changes.next())
                .flatten()
                .is_some()
            {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::{MatrixCmd, spawn};
    use crate::paths::Paths;
    use matrix_sdk::config::SyncSettings;
    use tokio::sync::mpsc::UnboundedReceiver;

    #[test]
    fn roles_come_from_power_levels() {
        assert_eq!(role(UserPowerLevel::Infinite), MemberRole::Owner);
        assert_eq!(role(UserPowerLevel::Int(100.into())), MemberRole::Admin);
        assert_eq!(role(UserPowerLevel::Int(50.into())), MemberRole::Moderator);
        assert_eq!(role(UserPowerLevel::Int(49.into())), MemberRole::User);
        assert_eq!(role(UserPowerLevel::Int((-1).into())), MemberRole::User);
        assert!(MemberRole::Owner > MemberRole::Admin && MemberRole::Admin > MemberRole::User);
    }

    #[test]
    fn debug_output_names_nobody() {
        let command = ManageCmd::Create {
            request: 0,
            name: "Secret plans".into(),
            topic: Some("the heist".into()),
            alias: None,
            encrypted: true,
        };
        let text = format!("{command:?}");
        assert!(!text.contains("Secret") && !text.contains("heist"));
        let command = ManageCmd::Invite {
            room_id: "!r:example.org".into(),
            user_id: "@bob:example.org".into(),
        };
        assert!(!format!("{command:?}").contains("bob"));
        let command = ManageCmd::Join {
            request: 0,
            target: "#secret:example.org".into(),
            via: Vec::new(),
        };
        assert!(!format!("{command:?}").contains("secret"));
        let command = ManageCmd::AcceptKnock {
            room_id: "!r:example.org".into(),
            user_id: "@bob:example.org".into(),
        };
        assert!(!format!("{command:?}").contains("bob"));
        let command = ManageCmd::DeclineKnock {
            room_id: "!r:example.org".into(),
            user_id: "@bob:example.org".into(),
        };
        assert!(!format!("{command:?}").contains("bob"));
        let command = ManageCmd::BanKnock {
            room_id: "!r:example.org".into(),
            user_id: "@bob:example.org".into(),
            reason: Some("secret reason".into()),
        };
        let text = format!("{command:?}");
        assert!(!text.contains("bob") && !text.contains("secret"));
        let req = RoomKnockRequest {
            user_id: "@bob:example.org".into(),
            name: "Secret Bob".into(),
            reason: Some("the heist".into()),
            acceptable: true,
            declinable: true,
            bannable: false,
        };
        let text = format!("{req:?}");
        assert!(!text.contains("bob") && !text.contains("Secret") && !text.contains("heist"));
        let details = RoomDetails {
            name: "Secret plans".into(),
            topic: "the heist".into(),
            knock_requests: vec![req],
            ..RoomDetails::default()
        };
        let event = ManageEvent::Info {
            room_id: "!r:example.org".into(),
            result: Ok(Box::new(details)),
        };
        let text = format!("{event:?}");
        assert!(!text.contains("Secret") && !text.contains("heist"));
        let event = ManageEvent::Opened {
            request: 1,
            result: Err("Could not join #secret:example.org".into()),
        };
        assert!(!format!("{event:?}").contains("secret"));
    }

    async fn next_manage(events: &mut UnboundedReceiver<MatrixEvent>) -> ManageEvent {
        let wait = async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::Manage(event) => return event,
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(60), wait)
            .await
            .expect("timed out waiting for the answer")
    }

    async fn opened(events: &mut UnboundedReceiver<MatrixEvent>) -> Result<String, String> {
        loop {
            if let ManageEvent::Opened { result, .. } = next_manage(events).await {
                return result;
            }
        }
    }

    /// The answer to a request for a direct chat that was not to be made: the room
    /// of the chat there is, or `None`.
    async fn looked_for_chat(
        events: &mut UnboundedReceiver<MatrixEvent>,
        wanted: u64,
    ) -> Option<String> {
        loop {
            match next_manage(events).await {
                ManageEvent::NoDirectChat { request, .. } if request == wanted => return None,
                ManageEvent::Opened { request, result } if request == wanted => {
                    return Some(result.unwrap());
                }
                _ => {}
            }
        }
    }

    async fn left(events: &mut UnboundedReceiver<MatrixEvent>, wanted: &str) {
        loop {
            if let ManageEvent::Left { room_id, result } = next_manage(events).await
                && room_id == wanted
            {
                result.unwrap();
                return;
            }
        }
    }

    async fn done(events: &mut UnboundedReceiver<MatrixEvent>, wanted: Change) {
        loop {
            if let ManageEvent::Done { change, result, .. } = next_manage(events).await
                && change == wanted
            {
                result.unwrap();
                return;
            }
        }
    }

    /// Details of `room_id`, once they satisfy `ready`.
    async fn info(
        events: &mut UnboundedReceiver<MatrixEvent>,
        room_id: &str,
        ready: impl Fn(&RoomDetails) -> bool,
    ) -> RoomDetails {
        loop {
            if let ManageEvent::Info {
                room_id: about,
                result,
            } = next_manage(events).await
                && about == room_id
            {
                let details = result.unwrap();
                if ready(&details) {
                    return *details;
                }
            }
        }
    }

    /// The whole life of a room against a local homeserver: create, invite bob, bob
    /// joins, he shows up with the right role, rename, favourite, notifications, a
    /// direct chat (reused the second time), joining by address, readable errors,
    /// removing bob, then leave and forget.
    /// `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=... cargo test -- --ignored`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn a_room_from_creation_to_forgetting() {
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let username = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let root = std::env::temp_dir().join(format!("matrix-rat-manage-{}", std::process::id()));
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
            password: password.into(),
        });
        let manage = |command: ManageCmd| handle.send(MatrixCmd::Manage(command));
        let own = loop {
            match events.recv().await.expect("worker stopped") {
                MatrixEvent::LoggedIn { user_id } => break user_id,
                MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                _ => {}
            }
        };

        let bob = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .unwrap();
        bob.matrix_auth()
            .login_username("bob", "bobpass")
            .initial_device_display_name("matrix-rat manage test")
            .send()
            .await
            .unwrap();
        let bob_id = bob.user_id().unwrap().to_string();

        // Readable errors: a room that does not exist, a user id that is our own.
        let server = own.split_once(':').unwrap().1.to_owned();
        manage(ManageCmd::Join {
            request: 0,
            target: format!("#matrix-rat-no-such-room-{}:{server}", std::process::id()),
            via: Vec::new(),
        });
        let err = opened(&mut events).await.unwrap_err();
        assert!(err.starts_with("Could not join: not found"), "{err}");
        manage(ManageCmd::DirectChat {
            request: 0,
            create: true,
            user_id: own.clone(),
        });
        assert!(opened(&mut events).await.unwrap_err().contains("your own"));

        // A private room: encrypted, and we are its admin (or owner).
        let tag = std::process::id();
        manage(ManageCmd::Create {
            request: 0,
            name: format!("matrix-rat manage test {tag}"),
            topic: Some("made by a test\u{1b}[31m".into()),
            alias: None,
            encrypted: true,
        });
        let room_id = opened(&mut events).await.unwrap();
        manage(ManageCmd::Info {
            room_id: room_id.clone(),
        });
        // The first answers may come before the sync has brought the room's state.
        let details = info(&mut events, &room_id, |d| {
            d.name.contains("manage test") && d.members.len() == 1 && d.can_invite
        })
        .await;
        assert_eq!(details.name, format!("matrix-rat manage test {tag}"));
        // What the server stored is shown without its escape character.
        assert_eq!(details.topic, "made by a test[31m");
        assert_eq!(details.encrypted, Some(true));
        assert!(!details.direct);
        assert!(details.can_rename && details.can_set_topic && details.can_invite);
        let me = &details.members[0];
        assert!(me.own && !me.removable && me.user_id == own);
        assert!(me.role >= MemberRole::Admin, "{:?}", me.role);

        // Invite bob: he is listed as invited, after us.
        manage(ManageCmd::Invite {
            room_id: room_id.clone(),
            user_id: bob_id.clone(),
        });
        done(&mut events, Change::Invite).await;
        let details = info(&mut events, &room_id, |d| d.members.len() == 2).await;
        let listed = &details.members[1];
        assert!(listed.user_id == bob_id && listed.invited && listed.removable);
        assert_eq!(listed.role, MemberRole::User);
        assert_eq!((details.joined, details.invited), (1, 1));

        // Bob joins with a plain client: joined now, still an ordinary member.
        bob.sync_once(SyncSettings::default()).await.unwrap();
        bob.join_room_by_id(<&RoomId>::try_from(room_id.as_str()).unwrap())
            .await
            .unwrap();
        let details = info(&mut events, &room_id, |d| {
            d.members.len() == 2 && d.members.iter().all(|m| !m.invited)
        })
        .await;
        assert_eq!((details.joined, details.invited), (2, 0));
        assert_eq!(details.members[1].role, MemberRole::User);
        assert!(details.members[0].own);

        // Name, topic, favourite, notifications.
        manage(ManageCmd::SetName {
            room_id: room_id.clone(),
            name: format!("matrix-rat renamed {tag}"),
        });
        done(&mut events, Change::Name).await;
        manage(ManageCmd::SetTopic {
            room_id: room_id.clone(),
            topic: "a new topic".into(),
        });
        done(&mut events, Change::Topic).await;
        manage(ManageCmd::SetFavourite {
            room_id: room_id.clone(),
            on: true,
        });
        done(&mut events, Change::Favourite(true)).await;
        manage(ManageCmd::SetNotify {
            room_id: room_id.clone(),
            level: NotifyLevel::Mute,
        });
        done(&mut events, Change::Notify).await;
        manage(ManageCmd::Info {
            room_id: room_id.clone(),
        });
        let details = info(&mut events, &room_id, |d| {
            d.favourite
                && d.topic == "a new topic"
                && d.name.contains("renamed")
                && d.notify == Some(NotifyLevel::Mute)
                && !d.members_loading
        })
        .await;
        assert_eq!(details.members.len(), 2);
        manage(ManageCmd::SetNotify {
            room_id: room_id.clone(),
            level: NotifyLevel::Mentions,
        });
        done(&mut events, Change::Notify).await;
        manage(ManageCmd::Info {
            room_id: room_id.clone(),
        });
        info(&mut events, &room_id, |d| {
            d.notify == Some(NotifyLevel::Mentions)
        })
        .await;

        // Joining a room we are in already just shows it.
        manage(ManageCmd::Join {
            request: 0,
            target: room_id.clone(),
            via: Vec::new(),
        });
        assert_eq!(opened(&mut events).await.unwrap(), room_id);

        // Looking for a chat makes none: nothing with a stranger, and with bob only
        // what earlier runs may have left behind (invitations of his are declined
        // here, so that the rest starts from nothing).
        manage(ManageCmd::DirectChat {
            request: 41,
            create: false,
            user_id: format!("@matrix-rat-nobody-{tag}:{server}"),
        });
        assert!(looked_for_chat(&mut events, 41).await.is_none());
        for _ in 0..20 {
            manage(ManageCmd::DirectChat {
                request: 42,
                create: false,
                user_id: bob_id.clone(),
            });
            let Some(stale) = looked_for_chat(&mut events, 42).await else {
                break;
            };
            manage(ManageCmd::Leave {
                room_id: stale.clone(),
            });
            left(&mut events, &stale).await;
        }
        // Bob invites us to a chat: that is the chat with him, not a reason for a
        // second one. It is found without being asked to make anything.
        let own_id = UserId::parse(&own).unwrap();
        let from_bob = bob.create_dm(&own_id).await.unwrap();
        let wait = async {
            loop {
                manage(ManageCmd::DirectChat {
                    request: 43,
                    create: false,
                    user_id: bob_id.clone(),
                });
                if let Some(found) = looked_for_chat(&mut events, 43).await {
                    return found;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };
        let found = tokio::time::timeout(Duration::from_secs(30), wait)
            .await
            .expect("the invitation from bob was not found");
        assert_eq!(found, from_bob.room_id().as_str());
        // Declined; then there is no chat again.
        manage(ManageCmd::Leave {
            room_id: found.clone(),
        });
        left(&mut events, &found).await;
        let _ = from_bob.leave().await;
        let _ = from_bob.forget().await;
        manage(ManageCmd::DirectChat {
            request: 44,
            create: false,
            user_id: bob_id.clone(),
        });
        assert!(looked_for_chat(&mut events, 44).await.is_none());

        // A direct chat with bob: created once (encrypted, direct), reused afterwards.
        manage(ManageCmd::DirectChat {
            request: 0,
            create: true,
            user_id: bob_id.clone(),
        });
        let chat = opened(&mut events).await.unwrap();
        assert_ne!(chat, room_id);
        manage(ManageCmd::Info {
            room_id: chat.clone(),
        });
        let details = info(&mut events, &chat, |d| d.direct).await;
        assert_eq!(details.encrypted, Some(true));
        manage(ManageCmd::DirectChat {
            request: 45,
            create: false,
            user_id: bob_id.clone(),
        });
        assert_eq!(looked_for_chat(&mut events, 45).await, Some(chat.clone()));

        // A public room is found and joined by its address.
        let alias = format!("matrix-rat-manage-{tag}");
        manage(ManageCmd::Create {
            request: 0,
            name: format!("matrix-rat public test {tag}"),
            topic: None,
            alias: Some(alias.clone()),
            encrypted: false,
        });
        let public = opened(&mut events).await.unwrap();
        bob.join_room_by_id_or_alias(
            <&RoomOrAliasId>::try_from(format!("#{alias}:{server}").as_str()).unwrap(),
            &[],
        )
        .await
        .unwrap();
        manage(ManageCmd::Info {
            room_id: public.clone(),
        });
        // `server_members` can report both members before Sliding Sync delivers
        // the room's alias state event, so wait for synced state too.
        let details = info(&mut events, &public, |d| d.joined == 2 && d.alias.is_some()).await;
        assert_eq!(details.alias, Some(format!("#{alias}:{server}")));
        assert_eq!(details.encrypted, Some(false));
        // The address is taken now.
        manage(ManageCmd::Create {
            request: 0,
            name: "matrix-rat duplicate".into(),
            topic: None,
            alias: Some(alias.clone()),
            encrypted: false,
        });
        let err = opened(&mut events).await.unwrap_err();
        assert!(err.contains("already taken"), "{err}");

        // Remove bob from the first room.
        manage(ManageCmd::Info {
            room_id: room_id.clone(),
        });
        manage(ManageCmd::Kick {
            room_id: room_id.clone(),
            user_id: bob_id.clone(),
            reason: None,
        });
        done(&mut events, Change::Kick).await;
        let details = info(&mut events, &room_id, |d| d.members.len() == 1).await;
        assert!(details.members[0].own);

        // Leave and forget all three; they are gone from the client afterwards.
        manage(ManageCmd::StopInfo);
        for id in [&room_id, &chat, &public] {
            manage(ManageCmd::Leave {
                room_id: id.clone(),
            });
            loop {
                if let ManageEvent::Left { room_id, result } = next_manage(&mut events).await {
                    assert_eq!(&room_id, id);
                    result.unwrap();
                    break;
                }
            }
        }
        // Forgotten rooms cannot be looked at any more.
        manage(ManageCmd::Info {
            room_id: room_id.clone(),
        });
        loop {
            if let ManageEvent::Info { result, .. } = next_manage(&mut events).await {
                assert!(result.is_err());
                break;
            }
        }

        // Bob tidies up too.
        bob.sync_once(SyncSettings::default()).await.unwrap();
        for id in [&room_id, &chat, &public] {
            if let Some(room) = bob.get_room(<&RoomId>::try_from(id.as_str()).unwrap()) {
                let _ = room.leave().await;
                let _ = room.forget().await;
            }
        }
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = bob.matrix_auth().logout().await;
        let _ = std::fs::remove_dir_all(root);
    }

    fn entry(name: &str, invited: bool) -> Entry {
        Entry {
            user: UserId::parse(format!("@{}:example.org", name.to_lowercase())).unwrap(),
            name: name.to_owned(),
            status: if invited {
                EntryStatus::Invited
            } else {
                EntryStatus::Joined
            },
        }
    }

    #[test]
    fn the_member_list_is_only_asked_for_when_it_helps() {
        let minute = Some(Duration::from_secs(1));
        // As long as the room says: nothing to do, whatever happened before.
        assert_eq!(plan(true, (5, 1), Some((5, 1)), minute), Plan::Fine);
        // Behind: ask, at once in a small room, also right after having asked.
        assert_eq!(plan(false, (5, 1), None, None), Plan::Ask);
        assert_eq!(plan(false, (5, 1), None, minute), Plan::Ask);
        // Asking did not help for these counts: not again until they change.
        assert_eq!(plan(false, (5, 1), Some((5, 1)), None), Plan::GaveUp);
        assert_eq!(plan(false, (6, 1), Some((5, 1)), None), Plan::Ask);
        // A large room: the first time at once, then spaced out, with the rest of
        // the wait handed back so that the look is not forgotten.
        assert_eq!(plan(false, (LARGE_ROOM + 1, 0), None, None), Plan::Ask);
        assert_eq!(
            plan(
                false,
                (LARGE_ROOM + 1, 0),
                None,
                Some(Duration::from_secs(20))
            ),
            Plan::Wait(LARGE_ROOM_REFETCH - Duration::from_secs(20))
        );
        assert_eq!(
            plan(false, (LARGE_ROOM + 1, 0), None, Some(LARGE_ROOM_REFETCH)),
            Plan::Ask
        );
        // Too many to list: never asked for.
        assert_eq!(plan(false, (MAX_MEMBERS, 1), None, None), Plan::TooLarge);
        assert_eq!(plan(false, (MAX_MEMBERS, 0), None, None), Plan::Ask);
    }

    #[test]
    fn lists_are_compared_with_the_rooms_counts() {
        let mut banned = entry("d", false);
        banned.status = EntryStatus::Banned;
        let entries = [
            entry("a", false),
            entry("b", false),
            entry("c", true),
            banned,
        ];
        assert!(entries_agree(&entries, (2, 1)));
        assert!(!entries_agree(&entries, (3, 0)));
        assert!(!entries_agree(&entries, (2, 0)));
        assert!(entries_agree(&[], (0, 0)));
    }

    #[test]
    fn refresh_signal_only_wakes_the_watched_room() {
        let refresh = Refresh::new();
        let mut rx = refresh.watch("!watched:example.org");
        assert!(!rx.has_changed().unwrap());

        bump_refresh(&refresh, "!other:example.org");
        assert!(!rx.has_changed().unwrap());

        bump_refresh(&refresh, "!watched:example.org");
        assert!(rx.has_changed().unwrap());
        let _ = rx.borrow_and_update();

        refresh.clear();
        bump_refresh(&refresh, "!watched:example.org");
        assert!(!rx.has_changed().unwrap());
    }

    #[test]
    fn complete_member_list_overrides_stale_summary_counts() {
        let own = UserId::parse("@me:example.org").unwrap();
        let mut banned = entry("mallory", false);
        banned.status = EntryStatus::Banned;
        let mut list = MemberList {
            entries: vec![entry("alice", false), entry("bob", false), banned],
            complete: true,
            ..MemberList::default()
        };

        // Complete list: header counts come from the list, excluding banned users.
        let mut details = RoomDetails {
            joined: 1,
            invited: 1,
            ..RoomDetails::default()
        };
        list.apply(&mut details, None, &own, None);
        assert_eq!((details.joined, details.invited), (2, 0));

        // Still loading, partial, or incomplete: keep the room summary's counts.
        let mut loading = RoomDetails {
            joined: 1,
            invited: 1,
            members_loading: true,
            ..RoomDetails::default()
        };
        list.apply(&mut loading, None, &own, None);
        assert_eq!((loading.joined, loading.invited), (1, 1));

        let mut partial = RoomDetails {
            joined: 25_000,
            invited: 3,
            members_partial: true,
            ..RoomDetails::default()
        };
        list.apply(&mut partial, None, &own, None);
        assert_eq!((partial.joined, partial.invited), (25_000, 3));

        list.complete = false;
        let mut incomplete = RoomDetails {
            joined: 1,
            invited: 1,
            ..RoomDetails::default()
        };
        list.apply(&mut incomplete, None, &own, None);
        assert_eq!((incomplete.joined, incomplete.invited), (1, 1));
    }

    #[test]
    fn members_are_ordered_and_made_safe() {
        let own = UserId::parse("@me:example.org").unwrap();
        let mut entries = vec![
            entry("zed", true),
            entry("Bob", false),
            entry("alice", false),
            entry("Me", false),
        ];
        entries[1].name = "Bob\u{1b}[31m".into();
        // No power levels known: everyone is an ordinary member, nobody removable.
        let shown = present(&entries, None, &own);
        let names: Vec<&str> = shown.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["alice", "Bob[31m", "Me", "zed"]);
        assert!(shown[3].invited && shown[2].own);
        assert!(
            shown
                .iter()
                .all(|m| !m.removable && m.role == MemberRole::User)
        );

        // The list for the screen is only rebuilt when the people changed.
        let mut list = MemberList {
            entries,
            ..MemberList::default()
        };
        assert!(list.present(None, &own) == shown);
        list.shown[0].name = "kept".into();
        assert_eq!(list.present(None, &own)[0].name, "kept");
        list.entries.pop();
        assert_eq!(list.present(None, &own).len(), 3);
        assert!(list.agrees((2, 1)));
    }

    #[test]
    fn errors_in_words() {
        let other = || "something else".to_owned();
        let said = || Some("You are not invited".to_owned());
        assert_eq!(
            explain_kind(Some(&ErrorKind::NotFound), None, false, other),
            "not found"
        );
        assert_eq!(
            explain_kind(Some(&ErrorKind::Forbidden), said(), false, other),
            "not allowed (You are not invited)"
        );
        assert_eq!(
            explain_kind(Some(&ErrorKind::RoomInUse), said(), false, other),
            "that address is already taken"
        );
        assert_eq!(
            explain_kind(Some(&ErrorKind::Unknown), None, false, other),
            "the server refused"
        );
        assert_eq!(
            explain_kind(Some(&ErrorKind::Unknown), said(), false, other),
            "You are not invited"
        );
        assert_eq!(
            explain_kind(None, None, true, other),
            "the server could not be reached"
        );
        assert_eq!(explain_kind(None, None, false, other), "something else");
        // Notification settings: words of our own, nothing quoted.
        for err in [
            NotificationSettingsError::UnableToSavePushRules,
            NotificationSettingsError::UnableToUpdatePushRule,
            NotificationSettingsError::RuleNotFound("!secret:example.org".into()),
            NotificationSettingsError::InvalidParameter("!secret:example.org".into()),
        ] {
            let text = explain_notify(&err);
            assert!(!text.contains("secret") && !text.contains('`'), "{text}");
        }
    }

    #[test]
    fn servers_in_a_join_request() {
        let via: Vec<String> = [
            "a.org", "a.org", "b.org", "c.org", "d.org", "e.org", "f.org",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        assert_eq!(
            via_names(&via),
            ["a.org", "b.org", "c.org", "d.org", "e.org"]
        );
        assert_eq!(VIA_MAX, crate::manage::VIA_MAX);
    }

    #[tokio::test]
    #[ignore = "needs a local homeserver (scripts/dev-homeserver.sh)"]
    async fn low_priority_tag_syncs_and_clears_favourite() {
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let username = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let root = std::env::temp_dir().join(format!("matrix-rat-lowprio-{}", std::process::id()));
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
            homeserver,
            username,
            password: password.into(),
        });
        loop {
            match events.recv().await.expect("worker stopped") {
                MatrixEvent::LoggedIn { .. } => break,
                MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                _ => {}
            }
        }

        handle.send(MatrixCmd::Manage(ManageCmd::Create {
            request: 0,
            name: format!("matrix-rat lowprio {}", std::process::id()),
            topic: None,
            alias: None,
            encrypted: false,
        }));
        let room_id = opened(&mut events).await.expect("created room");

        async fn wait_category(
            events: &mut UnboundedReceiver<MatrixEvent>,
            room_id: &str,
            wanted: super::super::RoomCategory,
        ) {
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match events.recv().await.expect("worker event") {
                        MatrixEvent::Rooms(list) => {
                            if list.iter().any(|r| r.id == room_id && r.category == wanted) {
                                return;
                            }
                        }
                        MatrixEvent::Error(err) => panic!("{err}"),
                        _ => {}
                    }
                }
            })
            .await
            .expect("timed out waiting for room category sync");
        }

        // 1. Favourite on -> RoomCategory::Favourite & details.favourite.
        handle.send(MatrixCmd::Manage(ManageCmd::SetFavourite {
            room_id: room_id.clone(),
            on: true,
        }));
        done(&mut events, Change::Favourite(true)).await;
        wait_category(&mut events, &room_id, super::super::RoomCategory::Favourite).await;

        // 2. Low priority on -> replaces Favourite (mutually exclusive).
        handle.send(MatrixCmd::Manage(ManageCmd::SetLowPriority {
            room_id: room_id.clone(),
            on: true,
        }));
        done(&mut events, Change::LowPriority(true)).await;
        wait_category(
            &mut events,
            &room_id,
            super::super::RoomCategory::LowPriority,
        )
        .await;
        handle.send(MatrixCmd::Manage(ManageCmd::Info {
            room_id: room_id.clone(),
        }));
        let d = info(&mut events, &room_id, |d| d.low_priority && !d.favourite).await;
        assert!(d.low_priority && !d.favourite);

        // 3. Favourite on -> clears LowPriority.
        handle.send(MatrixCmd::Manage(ManageCmd::SetFavourite {
            room_id: room_id.clone(),
            on: true,
        }));
        done(&mut events, Change::Favourite(true)).await;
        wait_category(&mut events, &room_id, super::super::RoomCategory::Favourite).await;
        handle.send(MatrixCmd::Manage(ManageCmd::Info {
            room_id: room_id.clone(),
        }));
        let d = info(&mut events, &room_id, |d| d.favourite && !d.low_priority).await;
        assert!(d.favourite && !d.low_priority);

        // Clean up.
        handle.send(MatrixCmd::Manage(ManageCmd::Leave { room_id }));
        let _ = next_manage(&mut events).await;
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn knock_requests_sort_and_power_gates_per_action() {
        use matrix_sdk::ruma::events::room::power_levels::{
            RoomPowerLevelsEventContent, RoomPowerLevelsSource,
        };
        use matrix_sdk::ruma::room_version_rules::AuthorizationRules;
        use std::collections::BTreeMap;

        let alice = UserId::parse("@alice:example.org").unwrap();
        let bob = UserId::parse("@bob:example.org").unwrap();
        let carol = UserId::parse("@carol:example.org").unwrap();
        let dave = UserId::parse("@dave:example.org").unwrap();
        let regular = UserId::parse("@user:example.org").unwrap();

        let mut content = RoomPowerLevelsEventContent::new(&AuthorizationRules::V11);
        content.invite = Int::from(50);
        content.kick = Int::from(50);
        content.ban = Int::from(75);
        content.users = BTreeMap::from([
            (alice.clone(), Int::from(50)),
            (carol.clone(), Int::from(60)),
        ]);
        let levels = RoomPowerLevels::new(
            RoomPowerLevelsSource::Original(content),
            &AuthorizationRules::V11,
            [],
        );

        let mut raw = vec![
            raw_knock(
                Some(UInt::from(200u32)),
                dave.clone(),
                None,
                Some("\tplease\n\x07add me "),
            ),
            raw_knock(
                Some(UInt::from(100u32)),
                carol.clone(),
                Some("Carol"),
                Some("   "),
            ),
            raw_knock(
                Some(UInt::from(100u32)),
                bob.clone(),
                Some("Bob\x07\u{202e}"),
                Some("hello"),
            ),
        ];
        sort_raw_knocks(&mut raw);
        // Tie on timestamp 100 breaks by user_id (@bob < @carol), then @dave at 200.
        assert_eq!(
            raw.iter().map(|k| k.user.as_str()).collect::<Vec<_>>(),
            [
                "@bob:example.org",
                "@carol:example.org",
                "@dave:example.org"
            ]
        );

        let mod_view = build_knock_requests(&raw, Some(&levels), &alice);
        assert_eq!(mod_view.len(), 3);
        assert_eq!(mod_view[0].name, "Bob");
        assert_eq!(mod_view[0].reason.as_deref(), Some("hello"));
        assert!(mod_view[0].acceptable && mod_view[0].declinable && !mod_view[0].bannable);

        // Target with PL 60 >= Alice's 50 cannot be declined by Alice, though invite is still allowed.
        assert_eq!(mod_view[1].user_id, "@carol:example.org");
        assert_eq!(mod_view[1].reason, None);
        assert!(mod_view[1].acceptable && !mod_view[1].declinable && !mod_view[1].bannable);

        // Localpart fallback & control-char sanitization on dave.
        assert_eq!(mod_view[2].name, "dave");
        assert_eq!(mod_view[2].reason.as_deref(), Some("please add me"));

        // Plain member with PL 0 below invite(50)/kick(50)/ban(75) sees no actionable requests.
        assert!(build_knock_requests(&raw, Some(&levels), &regular).is_empty());
    }

    #[test]
    fn permissions_update_guards_v12_creators_lockout_and_level_cap() {
        use matrix_sdk::ruma::events::room::power_levels::{
            RoomPowerLevelsEventContent, RoomPowerLevelsSource,
        };
        use matrix_sdk::ruma::room_version_rules::AuthorizationRules;
        use std::collections::BTreeMap;

        let creator = UserId::parse("@creator:example.org").unwrap();
        let admin = UserId::parse("@admin:example.org").unwrap();
        let mod_user = UserId::parse("@mod:example.org").unwrap();

        let mut content = RoomPowerLevelsEventContent::new(&AuthorizationRules::V11);
        content.state_default = Int::from(50);
        content.events = BTreeMap::from([
            (TimelineEventType::RoomMessage, Int::from(25)),
            (StateEventType::RoomTombstone.into(), Int::from(100)),
        ]);
        content.users = BTreeMap::from([
            (admin.clone(), Int::from(100)),
            (mod_user.clone(), Int::from(50)),
        ]);
        let v11_levels = RoomPowerLevels::new(
            RoomPowerLevelsSource::Original(content.clone()),
            &AuthorizationRules::V11,
            [],
        );

        let cur = RoomPermissions::from_levels(&v11_levels);
        assert_eq!(cur.send_messages, 25);
        assert_eq!(cur.change_settings, 50);

        // Moderator (50) cannot raise a permission above 50.
        let mut too_high = cur;
        too_high.ban = 100;
        let err = prepare_permissions_update(&v11_levels, &mod_user, &too_high).unwrap_err();
        assert!(err.contains("above your own level (50)"), "{err}");

        // Moderator (50) raising change_settings (`state_default`) to 100 would lock themselves out.
        let mut lockout_content = content.clone();
        lockout_content.state_default = Int::from(0);
        let mod_pl = RoomPowerLevels::new(
            RoomPowerLevelsSource::Original(lockout_content),
            &AuthorizationRules::V11,
            [],
        );
        let mut lockout = RoomPermissions::from_levels(&mod_pl);
        lockout.change_settings = 50;
        let _ = prepare_permissions_update(&mod_pl, &mod_user, &lockout).unwrap();
        let mut mod_drop = mod_pl.clone();
        mod_drop.users.insert(mod_user.clone(), Int::from(40));
        mod_drop.state_default = Int::from(40);
        let mut mod_lock = RoomPermissions::from_levels(&mod_drop);
        mod_lock.change_settings = 50;
        // Trying to set 50 as 40 fails both cap and lockout; lowering events_default only clears override.
        assert!(prepare_permissions_update(&mod_drop, &mod_user, &mod_lock).is_err());

        // Admin updates send_messages -> clears explicit RoomMessage override and preserves RoomTombstone.
        let mut lowered = cur;
        lowered.send_messages = 0;
        let out = prepare_permissions_update(&v11_levels, &admin, &lowered).unwrap();
        assert_eq!(out.events_default, Int::from(0));
        assert!(!out.events.contains_key(&TimelineEventType::RoomMessage));
        assert_eq!(
            out.events.get(&StateEventType::RoomTombstone.into()),
            Some(&Int::from(100))
        );

        // Self-lockout when explicit admin sets state_default > own level? Allowed up to 100, fails > 100 if capped, or
        // explicitly: non-creator user with explicit override cannot raise power_levels req above own level; wait:
        // test non-creator with state_default == 50 and own == 50 cannot raise power_levels without self-lockout
        // if explicit rule isn't lower (here own == 50 raising to 60 is capped; simulate own == 50 with state_default 50).
        // Simulate admin (PL 100) where `events[RoomPowerLevels]` wasn't set and own level is artificially 50 via users check:
        // Instead verify when `events` doesn't allow after state_default change:
        let mut strict_v11 = content;
        strict_v11.state_default = Int::from(50);
        let strict_pl = RoomPowerLevels::new(
            RoomPowerLevelsSource::Original(strict_v11),
            &AuthorizationRules::V11,
            [],
        );
        let mut raise_default = RoomPermissions::from_levels(&strict_pl);
        raise_default.change_settings = 100;
        assert!(prepare_permissions_update(&strict_pl, &mod_user, &raise_default).is_err());

        // Room v12+: creator has infinite power, can set 100, never placed in `users`.
        let mut v12_content = RoomPowerLevelsEventContent::new(&AuthorizationRules::V12);
        v12_content
            .events
            .insert(StateEventType::RoomTombstone.into(), Int::from(100));
        let mut v12_levels = RoomPowerLevels::new(
            RoomPowerLevelsSource::Original(v12_content),
            &AuthorizationRules::V12,
            [creator.clone()],
        );
        v12_levels.users.insert(creator.clone(), Int::from(100));
        let mut v12_next = RoomPermissions::from_levels(&v12_levels);
        v12_next.change_settings = 100;
        let v12_out = prepare_permissions_update(&v12_levels, &creator, &v12_next).unwrap();
        assert_eq!(v12_out.state_default, Int::from(100));
        assert!(!v12_out.users.contains_key(&creator));
        assert_eq!(
            v12_out.events.get(&StateEventType::RoomTombstone.into()),
            Some(&Int::from(100))
        );
    }

    #[test]
    fn room_join_rule_history_and_alias_parsers() {
        use matrix_sdk::ruma::ServerName;
        use matrix_sdk::ruma::events::room::join_rules::AllowRule;

        assert_eq!(RoomJoinRule::from(JoinRule::Invite), RoomJoinRule::Invite);
        assert_eq!(RoomJoinRule::from(JoinRule::Knock), RoomJoinRule::Knock);
        assert_eq!(RoomJoinRule::from(JoinRule::Public), RoomJoinRule::Public);
        assert!(RoomJoinRule::Restricted.to_ruma().is_none());
        assert!(RoomJoinRule::KnockRestricted.to_ruma().is_none());
        assert!(RoomJoinRule::Private.to_ruma().is_none());
        let restricted =
            JoinRule::Restricted(matrix_sdk::ruma::events::room::join_rules::Restricted::new(
                vec![AllowRule::room_membership(
                    matrix_sdk::ruma::RoomId::parse("!space:example.org").unwrap(),
                )],
            ));
        assert_eq!(RoomJoinRule::from(restricted), RoomJoinRule::Restricted);

        assert_eq!(
            RoomHistory::from(HistoryVisibility::WorldReadable).to_ruma(),
            Some(HistoryVisibility::WorldReadable)
        );
        assert_eq!(
            RoomHistory::from(HistoryVisibility::Joined),
            RoomHistory::Joined
        );

        let hs = ServerName::parse("example.org").unwrap();
        assert_eq!(
            parse_local_alias("#chat:example.org", &hs)
                .unwrap()
                .as_str(),
            "#chat:example.org"
        );
        let wrong_hs = parse_local_alias("#chat:other.org", &hs).unwrap_err();
        assert!(wrong_hs.contains("homeserver"), "{wrong_hs}");
        assert!(parse_local_alias("bad alias", &hs).is_err());

        let (main, alts) = parse_room_aliases(
            Some("#main:example.org".to_owned()),
            vec![
                "#alt1:example.org".to_owned(),
                "#main:example.org".to_owned(),
                "#alt1:example.org".to_owned(),
                "#alt2:other.org".to_owned(),
            ],
        )
        .unwrap();
        assert_eq!(main.as_ref().map(|a| a.as_str()), Some("#main:example.org"));
        assert_eq!(
            alts.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
            ["#alt1:example.org", "#alt2:other.org"]
        );
    }

    #[test]
    fn room_settings_debug_redacts_private_fields() {
        let cmds = [
            ManageCmd::SetJoinRule {
                room_id: "!secret_r:example.org".into(),
                rule: RoomJoinRule::Knock,
            },
            ManageCmd::SetHistoryVisibility {
                room_id: "!secret_r:example.org".into(),
                visibility: RoomHistory::Joined,
            },
            ManageCmd::EnableEncryption {
                room_id: "!secret_r:example.org".into(),
            },
            ManageCmd::SetRoomAvatar {
                room_id: "!secret_r:example.org".into(),
                path: PathBuf::from("/tmp/secret-room-avatar.png"),
            },
            ManageCmd::RemoveRoomAvatar {
                room_id: "!secret_r:example.org".into(),
            },
            ManageCmd::SetAddresses {
                room_id: "!secret_r:example.org".into(),
                canonical: Some("#secret-main:example.org".into()),
                alt_aliases: vec!["#secret-alt:example.org".into()],
            },
            ManageCmd::CreateLocalAlias {
                room_id: "!secret_r:example.org".into(),
                alias: "#secret-local:example.org".into(),
            },
            ManageCmd::DeleteLocalAlias {
                room_id: "!secret_r:example.org".into(),
                alias: "#secret-local:example.org".into(),
            },
            ManageCmd::SetDirect {
                room_id: "!secret_r:example.org".into(),
                direct: true,
            },
            ManageCmd::SetPermissions {
                room_id: "!secret_r:example.org".into(),
                permissions: RoomPermissions::default(),
            },
        ];
        let details = RoomDetails {
            name: "SecretRoom".into(),
            topic: "SecretTopic".into(),
            avatar_url: Some("mxc://example.org/secret-mxc".into()),
            canonical_alias: Some("#secret-main:example.org".into()),
            alt_aliases: vec!["#secret-alt:example.org".into()],
            local_aliases: vec!["#secret-local:example.org".into()],
            own_homeserver: Some("secret-hs.org".into()),
            ..RoomDetails::default()
        };
        let cmds_dbg = format!("{cmds:?}");
        for secret in [
            "secret-room-avatar",
            "secret-main",
            "secret-alt",
            "secret-local",
        ] {
            assert!(!cmds_dbg.contains(secret), "{secret} leaked in {cmds_dbg}");
        }
        let details_dbg = format!("{details:?}");
        for secret in [
            "secret-main",
            "secret-alt",
            "secret-local",
            "SecretRoom",
            "SecretTopic",
            "secret-mxc",
            "secret-hs",
        ] {
            assert!(
                !details_dbg.contains(secret),
                "{secret} leaked in {details_dbg}"
            );
        }
    }

    #[test]
    fn parse_member_events_separates_members_and_knocks() {
        let raw_events: Vec<Raw<RoomMemberEvent>> = [
            r#"{"type":"m.room.member","state_key":"@alice:example.org","sender":"@alice:example.org","event_id":"$e1","origin_server_ts":100,"room_id":"!r:example.org","content":{"membership":"join","displayname":"Alice"}}"#,
            r#"{"type":"m.room.member","state_key":"@bob:example.org","sender":"@bob:example.org","event_id":"$e2","origin_server_ts":250,"room_id":"!r:example.org","content":{"membership":"knock","displayname":"Bob\u0007","reason":"  let me in  "}}"#,
            r#"{"type":"m.room.member","state_key":"@carol:example.org","sender":"@carol:example.org","event_id":"$e3","origin_server_ts":150,"room_id":"!r:example.org","content":{"membership":"knock"}}"#,
            r#"{"type":"m.room.member","state_key":"@dave:example.org","sender":"@alice:example.org","event_id":"$e4","origin_server_ts":300,"room_id":"!r:example.org","content":{"membership":"ban"}}"#,
            r#"{"type":"m.room.member","state_key":"@eve:example.org","sender":"@eve:example.org","event_id":"$e5","origin_server_ts":350,"room_id":"!r:example.org","content":{"membership":"leave"}}"#,
            r#"{"not":"a valid member event"}"#,
        ]
        .into_iter()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();

        let parsed = parse_member_events(&raw_events);
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].user, "@alice:example.org");
        assert!(matches!(parsed.entries[0].status, EntryStatus::Joined));
        assert_eq!(parsed.entries[1].user, "@dave:example.org");
        assert!(matches!(parsed.entries[1].status, EntryStatus::Banned));

        assert_eq!(parsed.knocks.len(), 2);
        // Sorted oldest timestamp first (@carol at 150, then @bob at 250).
        assert_eq!(parsed.knocks[0].user, "@carol:example.org");
        assert_eq!(parsed.knocks[0].name, "carol");
        assert_eq!(parsed.knocks[0].reason, None);
        assert_eq!(parsed.knocks[1].user, "@bob:example.org");
        assert_eq!(parsed.knocks[1].name, "Bob");
        assert_eq!(parsed.knocks[1].reason.as_deref(), Some("let me in"));
    }

    struct ServerWorker {
        root: std::path::PathBuf,
        handle: crate::matrix::MatrixHandle,
        events: UnboundedReceiver<MatrixEvent>,
        user_id: String,
    }

    impl ServerWorker {
        async fn login(tag: &str, homeserver: &str, username: &str, password: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("matrix-rat-srv-{tag}-{}", std::process::id()));
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
                password: password.to_owned().into(),
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
            Self {
                root,
                handle,
                events,
                user_id,
            }
        }

        fn manage(&self, cmd: ManageCmd) {
            self.handle.send(MatrixCmd::Manage(cmd));
        }

        async fn cleanup(mut self, room_ids: &[String]) {
            self.manage(ManageCmd::StopInfo);
            for room_id in room_ids {
                self.manage(ManageCmd::Leave {
                    room_id: room_id.clone(),
                });
                let _ = tokio::time::timeout(Duration::from_secs(15), async {
                    while let Some(ev) = self.events.recv().await {
                        if matches!(
                            ev,
                            MatrixEvent::Manage(ManageEvent::Left { room_id: id, .. })
                                if id == *room_id
                        ) {
                            break;
                        }
                    }
                })
                .await;
            }
            self.handle.send(MatrixCmd::Logout);
            let _ = tokio::time::timeout(Duration::from_secs(10), async {
                while let Some(ev) = self.events.recv().await {
                    if matches!(ev, MatrixEvent::NeedLogin { .. }) {
                        break;
                    }
                }
            })
            .await;
            self.handle.send(MatrixCmd::Shutdown);
            while self.events.recv().await.is_some() {}
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn test_server_and_accounts() -> (String, String, String, String, String) {
        let hs = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let alice_user = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let alice_pass =
            std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let bob_user = std::env::var("MATRIX_RAT_TEST_USER2").unwrap_or_else(|_| "bob".to_owned());
        let bob_pass =
            std::env::var("MATRIX_RAT_TEST_PASSWORD2").unwrap_or_else(|_| "bobpass".to_owned());
        (hs, alice_user, alice_pass, bob_user, bob_pass)
    }

    async fn wait_knocked(events: &mut UnboundedReceiver<MatrixEvent>, wanted_req: u64) {
        loop {
            if let ManageEvent::Knocked { request, result } = next_manage(events).await
                && request == wanted_req
            {
                result.expect("knock failed");
                return;
            }
        }
    }

    /// End-to-end check against a local homeserver (`MATRIX_RAT_TEST_USER` +
    /// `MATRIX_RAT_TEST_PASSWORD`): Alice sets knock join rule, Bob previews
    /// and knocks with a reason, Alice sees the request and accepts it, and
    /// Bob joins the room.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn knock_room_bob_knocks_alice_sees_and_accepts() {
        use std::panic::AssertUnwindSafe;

        use futures::FutureExt;

        let (hs, alice_user, alice_pass, bob_user, bob_pass) = test_server_and_accounts();
        let mut alice = ServerWorker::login("knock-alice", &hs, &alice_user, &alice_pass).await;
        let mut bob = ServerWorker::login("knock-bob", &hs, &bob_user, &bob_pass).await;
        let mut cleanup_rooms = Vec::new();
        let bob_id = bob.user_id.clone();

        let outcome = AssertUnwindSafe(async {
            let tag = std::process::id();
            alice.manage(ManageCmd::Create {
                request: 1,
                name: format!("matrix-rat knock {tag}"),
                topic: None,
                alias: None,
                encrypted: false,
            });
            let room_id = opened(&mut alice.events).await.unwrap();
            cleanup_rooms.push(room_id.clone());

            alice.manage(ManageCmd::SetJoinRule {
                room_id: room_id.clone(),
                rule: RoomJoinRule::Knock,
            });
            done(&mut alice.events, Change::JoinRule).await;

            alice.manage(ManageCmd::Info {
                room_id: room_id.clone(),
            });
            info(&mut alice.events, &room_id, |d| {
                d.join_rule == Some(RoomJoinRule::Knock)
            })
            .await;

            let server = alice
                .user_id
                .split_once(':')
                .map(|(_, s)| s.to_owned())
                .unwrap();
            bob.manage(ManageCmd::Preview {
                request: 10,
                target: room_id.clone(),
                via: vec![server.clone()],
            });
            let preview = loop {
                if let ManageEvent::Preview {
                    request: 10,
                    result,
                } = next_manage(&mut bob.events).await
                {
                    break result.expect("preview knock room");
                }
            };
            assert_eq!(
                preview.join_rule,
                super::super::directory::PreviewJoinRule::Knock
            );

            bob.manage(ManageCmd::Knock {
                request: 11,
                target: room_id.clone(),
                via: vec![server.clone()],
                reason: Some("let me in please".into()),
            });
            wait_knocked(&mut bob.events, 11).await;

            let details = info(&mut alice.events, &room_id, |d| {
                d.knock_requests.iter().any(|k| k.user_id == bob_id)
            })
            .await;
            let knock = details
                .knock_requests
                .iter()
                .find(|k| k.user_id == bob_id)
                .unwrap();
            assert_eq!(knock.reason.as_deref(), Some("let me in please"));

            alice.manage(ManageCmd::AcceptKnock {
                room_id: room_id.clone(),
                user_id: bob_id.clone(),
            });
            done(&mut alice.events, Change::AcceptKnock).await;

            bob.manage(ManageCmd::Join {
                request: 12,
                target: room_id.clone(),
                via: vec![server],
            });
            let joined = opened(&mut bob.events).await.unwrap();
            assert_eq!(joined, room_id);

            info(&mut alice.events, &room_id, |d| {
                d.joined == 2 && d.knock_requests.is_empty()
            })
            .await;
        })
        .catch_unwind()
        .await;

        bob.cleanup(&cleanup_rooms).await;
        alice.cleanup(&cleanup_rooms).await;
        if let Err(payload) = outcome {
            std::panic::resume_unwind(payload);
        }
    }

    /// End-to-end check against a local homeserver (`MATRIX_RAT_TEST_USER` +
    /// `MATRIX_RAT_TEST_PASSWORD`): join-rule and history-visibility changes
    /// push live `ManageEvent::Info` updates to another joined member.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn room_settings_join_rule_and_history_sync_to_member() {
        use std::panic::AssertUnwindSafe;

        use futures::FutureExt;

        let (hs, alice_user, alice_pass, bob_user, bob_pass) = test_server_and_accounts();
        let mut alice = ServerWorker::login("sync-alice", &hs, &alice_user, &alice_pass).await;
        let mut bob = ServerWorker::login("sync-bob", &hs, &bob_user, &bob_pass).await;
        let mut cleanup_rooms = Vec::new();
        let bob_id = bob.user_id.clone();

        let outcome = AssertUnwindSafe(async {
            let tag = std::process::id();
            alice.manage(ManageCmd::Create {
                request: 1,
                name: format!("matrix-rat settings sync {tag}"),
                topic: None,
                alias: None,
                encrypted: false,
            });
            let room_id = opened(&mut alice.events).await.unwrap();
            cleanup_rooms.push(room_id.clone());

            alice.manage(ManageCmd::Invite {
                room_id: room_id.clone(),
                user_id: bob_id,
            });
            done(&mut alice.events, Change::Invite).await;

            bob.manage(ManageCmd::Join {
                request: 2,
                target: room_id.clone(),
                via: Vec::new(),
            });
            assert_eq!(opened(&mut bob.events).await.unwrap(), room_id);

            bob.manage(ManageCmd::Info {
                room_id: room_id.clone(),
            });
            info(&mut bob.events, &room_id, |d| d.joined == 2).await;

            alice.manage(ManageCmd::SetJoinRule {
                room_id: room_id.clone(),
                rule: RoomJoinRule::Knock,
            });
            done(&mut alice.events, Change::JoinRule).await;
            info(&mut bob.events, &room_id, |d| {
                d.join_rule == Some(RoomJoinRule::Knock)
            })
            .await;

            alice.manage(ManageCmd::SetJoinRule {
                room_id: room_id.clone(),
                rule: RoomJoinRule::Public,
            });
            done(&mut alice.events, Change::JoinRule).await;
            info(&mut bob.events, &room_id, |d| {
                d.join_rule == Some(RoomJoinRule::Public)
            })
            .await;

            alice.manage(ManageCmd::SetHistoryVisibility {
                room_id: room_id.clone(),
                visibility: RoomHistory::Invited,
            });
            done(&mut alice.events, Change::HistoryVisibility).await;
            info(&mut bob.events, &room_id, |d| {
                d.history_visibility == Some(RoomHistory::Invited)
            })
            .await;
        })
        .catch_unwind()
        .await;

        bob.cleanup(&cleanup_rooms).await;
        alice.cleanup(&cleanup_rooms).await;
        if let Err(payload) = outcome {
            std::panic::resume_unwind(payload);
        }
    }

    /// End-to-end check against a local homeserver (`MATRIX_RAT_TEST_USER` +
    /// `MATRIX_RAT_TEST_PASSWORD`): enabling encryption on a fresh unencrypted
    /// room succeeds and updates `RoomDetails.encrypted` to `Some(true)`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn enabling_encryption_in_fresh_room() {
        use std::panic::AssertUnwindSafe;

        use futures::FutureExt;

        let (hs, alice_user, alice_pass, _, _) = test_server_and_accounts();
        let mut alice = ServerWorker::login("enc-alice", &hs, &alice_user, &alice_pass).await;
        let mut cleanup_rooms = Vec::new();

        let outcome = AssertUnwindSafe(async {
            let tag = std::process::id();
            alice.manage(ManageCmd::Create {
                request: 1,
                name: format!("matrix-rat enc {tag}"),
                topic: None,
                alias: None,
                encrypted: false,
            });
            let room_id = opened(&mut alice.events).await.unwrap();
            cleanup_rooms.push(room_id.clone());

            alice.manage(ManageCmd::Info {
                room_id: room_id.clone(),
            });
            let before = info(&mut alice.events, &room_id, |d| {
                d.encrypted == Some(false) && d.settings_power.can_encrypt
            })
            .await;
            assert_eq!(before.encrypted, Some(false));

            alice.manage(ManageCmd::EnableEncryption {
                room_id: room_id.clone(),
            });
            done(&mut alice.events, Change::EnableEncryption).await;

            let after = info(&mut alice.events, &room_id, |d| d.encrypted == Some(true)).await;
            assert_eq!(after.encrypted, Some(true));
        })
        .catch_unwind()
        .await;

        alice.cleanup(&cleanup_rooms).await;
        if let Err(payload) = outcome {
            std::panic::resume_unwind(payload);
        }
    }
}
