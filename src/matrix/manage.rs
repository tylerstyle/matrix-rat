//! Room management: joining, creating, leaving, inviting, and the details of a room
//! (members, roles, what we may do there).

use std::time::{Duration, Instant};

use matrix_sdk::notification_settings::RoomNotificationMode;
use matrix_sdk::ruma::api::client::membership::get_member_events;
use matrix_sdk::ruma::api::client::room::create_room::v3::{Request as CreateRequest, RoomPreset};
use matrix_sdk::ruma::api::error::{ErrorBody, ErrorKind};
use matrix_sdk::ruma::events::InitialStateEvent;
use matrix_sdk::ruma::events::StateEventType;
use matrix_sdk::ruma::events::direct::DirectUserIdentifier;
use matrix_sdk::ruma::events::room::encryption::RoomEncryptionEventContent;
use matrix_sdk::ruma::events::room::member::MembershipState;
use matrix_sdk::ruma::events::room::power_levels::{RoomPowerLevels, UserPowerLevel};
use matrix_sdk::ruma::{
    Int, OwnedRoomOrAliasId, OwnedServerName, OwnedUserId, RoomId, RoomOrAliasId, ServerName,
    UserId,
};
use matrix_sdk::{Client, NotificationSettingsError, Room, RoomMemberships, RoomState};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use super::MatrixEvent;
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
    /// Remove someone from the room.
    Kick {
        room_id: String,
        user_id: String,
    },
    SetName {
        room_id: String,
        name: String,
    },
    SetTopic {
        room_id: String,
        topic: String,
    },
    SetFavourite {
        room_id: String,
        on: bool,
    },
    SetNotify {
        room_id: String,
        level: NotifyLevel,
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
            Self::Kick { room_id, .. } => write!(f, "Kick({room_id})"),
            Self::SetName { room_id, .. } => write!(f, "SetName({room_id})"),
            Self::SetTopic { room_id, .. } => write!(f, "SetTopic({room_id})"),
            Self::SetFavourite { room_id, on } => write!(f, "SetFavourite({room_id}, {on})"),
            Self::SetNotify { room_id, level } => write!(f, "SetNotify({room_id}, {level:?})"),
        }
    }
}

/// Which change a [`ManageEvent::Done`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Invite,
    Kick,
    Name,
    Topic,
    Favourite(bool),
    Notify,
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

#[derive(Clone, PartialEq, Eq)]
pub struct RoomMember {
    pub user_id: String,
    /// Display name, or the first part of the user id.
    pub name: String,
    pub role: MemberRole,
    /// Invited, not (yet) joined.
    pub invited: bool,
    pub own: bool,
    /// We are allowed to remove this member from the room.
    pub removable: bool,
}

/// Everything the room info view shows. All text is safe to put on the screen.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RoomDetails {
    pub name: String,
    pub topic: String,
    pub alias: Option<String>,
    /// `None`: not known (yet).
    pub encrypted: Option<bool>,
    pub direct: bool,
    pub joined: u64,
    pub invited: u64,
    pub favourite: bool,
    /// `None` for rooms we are not in.
    pub notify: Option<NotifyLevel>,
    pub can_rename: bool,
    pub can_set_topic: bool,
    pub can_invite: bool,
    /// Joined members first, then invited ones; highest role first.
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
            "RoomDetails({} members, loading: {})",
            self.members.len(),
            self.members_loading
        )
    }
}

pub struct Manager {
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    /// Keeps the open room info view current.
    watcher: Option<JoinHandle<()>>,
}

impl Manager {
    pub fn new(client: Client, events: UnboundedSender<MatrixEvent>) -> Self {
        Self {
            client,
            events,
            watcher: None,
        }
    }

    pub fn stop(&mut self) {
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
            ManageCmd::Info { room_id } => {
                self.stop();
                self.watcher = Some(tokio::spawn(watch(client, room_id, self.events.clone())));
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
            ManageCmd::Kick { room_id, user_id } => {
                tokio::spawn(async move {
                    let result =
                        with_member(&client, &room_id, &user_id, "remove", async |room, user| {
                            room.kick_user(user, None).await
                        })
                        .await;
                    emit(ManageEvent::Done {
                        room_id,
                        change: Change::Kick,
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
                tokio::spawn(async move {
                    let result = match room(&client, &room_id) {
                        Ok(room) => room
                            .set_is_favourite(on, None)
                            .await
                            .map_err(|err| failed("change the favourites", &err)),
                        Err(err) => Err(err),
                    };
                    emit(ManageEvent::Done {
                        room_id,
                        change: Change::Favourite(on),
                        result,
                    });
                });
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
        }
    }
}

fn room(client: &Client, room_id: &str) -> Result<Room, String> {
    let id = RoomId::parse(room_id).map_err(|_| "That is not a room id.".to_owned())?;
    client
        .get_room(&id)
        .ok_or_else(|| "This room is not known yet.".to_owned())
}

/// "Could not join: …", with the reason in words a person can act on.
fn failed(what: &str, err: &matrix_sdk::Error) -> String {
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

fn explain(err: &matrix_sdk::Error) -> String {
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
    // Forgetting does not remove the room's unsent text from the store. The UI asks
    // for that (with the threads it knows) when the room leaves its list; the room's
    // own draft goes here already, while there still is a room to ask.
    if matches!(room.state(), RoomState::Left | RoomState::Banned)
        && let Err(err) = room.clear_composer_draft(None).await
    {
        tracing::warn!(?err, "could not remove the draft of a room after leaving");
    }
    // So it does not linger on the account as a room once left. Leaving worked, which
    // is what was asked for; a server that cannot forget is not worth an alarm.
    if matches!(room.state(), RoomState::Left | RoomState::Banned)
        && let Err(err) = room.forget().await
    {
        tracing::warn!(
            kind = ?err.client_api_error_kind(),
            "could not forget the room after leaving"
        );
    }
    Ok(())
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

/// One person in the room, before roles are worked out.
#[derive(Clone, PartialEq, Eq)]
struct Entry {
    user: OwnedUserId,
    name: String,
    invited: bool,
}

/// The member list as the watcher holds it between looks at the room.
#[derive(Default)]
struct MemberList {
    entries: Vec<Entry>,
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

/// Whether a list has as many people as the room says it has.
fn entries_agree(entries: &[Entry], (joined, invited): (u64, u64)) -> bool {
    let listed_invited = entries.iter().filter(|entry| entry.invited).count() as u64;
    entries.len() as u64 - listed_invited == joined && listed_invited == invited
}

fn role(level: UserPowerLevel) -> MemberRole {
    match level {
        UserPowerLevel::Infinite => MemberRole::Owner,
        UserPowerLevel::Int(level) if level >= Int::from(100) => MemberRole::Admin,
        UserPowerLevel::Int(level) if level >= Int::from(50) => MemberRole::Moderator,
        _ => MemberRole::User,
    }
}

/// Roles and order: joined before invited, the highest role first, then by name.
fn present(entries: &[Entry], levels: Option<&RoomPowerLevels>, own: &UserId) -> Vec<RoomMember> {
    let mut members: Vec<RoomMember> = entries
        .iter()
        .map(|entry| RoomMember {
            user_id: sanitize_line(entry.user.as_str()),
            name: sanitize_line(&entry.name),
            role: levels.map_or(MemberRole::User, |levels| {
                role(levels.for_user(&entry.user))
            }),
            invited: entry.invited,
            own: entry.user == own,
            removable: entry.user != own
                && levels.is_some_and(|levels| levels.user_can_kick_user(own, &entry.user)),
        })
        .collect();
    members.sort_by_cached_key(|member| {
        (
            member.invited,
            std::cmp::Reverse(member.role),
            member.name.to_lowercase(),
            member.user_id.clone(),
        )
    });
    members
}

/// The members the store knows. The sync only brings the ones it thinks are needed,
/// so this can be behind the room's own counts.
async fn stored_members(room: &Room) -> Result<Vec<Entry>, String> {
    let wanted = RoomMemberships::JOIN | RoomMemberships::INVITE;
    let members = room.members_no_sync(wanted).await.map_err(|err| {
        tracing::warn!(%err, "could not read the room's members");
        "The member list could not be read.".to_owned()
    })?;
    Ok(members
        .iter()
        .map(|member| Entry {
            user: member.user_id().to_owned(),
            name: member.name().to_owned(),
            invited: *member.membership() == MembershipState::Invite,
        })
        .collect())
}

/// The member list as the server has it now. Asked for directly and kept out of the
/// SDK's store on purpose: a full reload there makes the SDK throw away the room's
/// outgoing encryption session, which is too high a price for looking at a list.
async fn server_members(client: &Client, room: &Room) -> Result<Vec<Entry>, String> {
    let request = get_member_events::v3::Request::new(room.room_id().to_owned());
    let response = tokio::time::timeout(MEMBERS_TIMEOUT, client.send(request))
        .await
        .map_err(|_| "The member list took too long to arrive.".to_owned())?
        .map_err(|err| failed("load the member list", &err.into()))?;
    let mut entries = Vec::new();
    for raw in response.chunk {
        // One event the server got wrong does not cost the rest of the list.
        let Ok(event) = raw.deserialize() else {
            continue;
        };
        let invited = match event.membership() {
            MembershipState::Join => false,
            MembershipState::Invite => true,
            _ => continue,
        };
        let user = event.state_key().to_owned();
        let name = event
            .as_original()
            .and_then(|event| event.content.displayname.clone())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| user.localpart().to_owned());
        entries.push(Entry {
            user,
            name,
            invited,
        });
    }
    Ok(entries)
}

/// Everything about the room but its members.
async fn head(room: &Room, levels: Option<&RoomPowerLevels>) -> RoomDetails {
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
    RoomDetails {
        name: sanitize_line(&name),
        topic: sanitize(room.topic().unwrap_or_default().trim()),
        alias: room
            .canonical_alias()
            .or_else(|| room.alt_aliases().into_iter().next())
            .map(|alias| sanitize_line(alias.as_str())),
        encrypted,
        direct: room.is_direct().await.unwrap_or(false),
        joined: room.joined_members_count(),
        invited: room.invited_members_count(),
        favourite: room.is_favourite(),
        notify,
        can_rename: can(&|levels| levels.user_can_send_state(own, StateEventType::RoomName)),
        can_set_topic: can(&|levels| levels.user_can_send_state(own, StateEventType::RoomTopic)),
        can_invite: can(&|levels| levels.user_can_invite(own)),
        ..RoomDetails::default()
    }
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
async fn watch(client: Client, room_id: String, events: UnboundedSender<MatrixEvent>) {
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
    let mut sent: Option<RoomDetails> = None;
    loop {
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
        let mut current = head(&room, levels.as_ref()).await;
        let counts = (current.joined, current.invited);
        let mut wait = None;
        if joined {
            // The room's info changes with every message. The members are only read
            // again when their number did, and never in rooms too large to list.
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
                    Err(error) => list.error = Some(error),
                }
            }
            let since = list.asked.map(|at| at.elapsed());
            match plan(list.agrees(counts), counts, list.gave_up, since) {
                Plan::Fine => list.error = None,
                Plan::GaveUp => {}
                Plan::TooLarge => current.members_partial = true,
                Plan::Wait(time) => wait = Some(time),
                Plan::Ask => current.members_loading = true,
            }
        } else {
            list = MemberList::default();
        }
        current.members = list.present(levels.as_ref(), &own);
        current.members_error.clone_from(&list.error);
        let ask = current.members_loading;
        if sent.as_ref() != Some(&current) {
            if !emit(Ok(current.clone())) {
                return;
            }
            sent = Some(current.clone());
        }
        if ask {
            list.asked = Some(Instant::now());
            match server_members(&client, &room).await {
                Ok(entries) => {
                    list.entries = entries;
                    list.error = None;
                    if !list.agrees(counts) {
                        list.gave_up = Some(counts);
                    }
                }
                // What is shown stays; the reason goes next to it.
                Err(error) => {
                    list.error = Some(error);
                    list.gave_up = Some(counts);
                }
            }
            current.members_loading = false;
            current.members = list.present(levels.as_ref(), &own);
            current.members_error.clone_from(&list.error);
            if !emit(Ok(current.clone())) {
                return;
            }
            sent = Some(current);
        }
        // Until the room changes, or until a postponed look at the members is due.
        match wait {
            Some(time) => {
                tokio::select! {
                    change = changes.next() => if change.is_none() { return },
                    () = tokio::time::sleep(time) => {}
                }
            }
            None => {
                if changes.next().await.is_none() {
                    return;
                }
            }
        }
        // Changes come in bursts (a sync brings several); one look afterwards is enough.
        tokio::time::sleep(Duration::from_millis(300)).await;
        while futures::FutureExt::now_or_never(changes.next())
            .flatten()
            .is_some()
        {}
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
        let details = RoomDetails {
            name: "Secret plans".into(),
            topic: "the heist".into(),
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
            password,
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
        assert_eq!(details.joined, 2);
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
        let details = info(&mut events, &public, |d| d.joined == 2).await;
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
            invited,
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
        let entries = [entry("a", false), entry("b", false), entry("c", true)];
        assert!(entries_agree(&entries, (2, 1)));
        assert!(!entries_agree(&entries, (3, 0)));
        assert!(!entries_agree(&entries, (2, 0)));
        assert!(entries_agree(&[], (0, 0)));
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
}
