//! Room management: joining, creating, leaving, inviting, and the details of a room
//! (members, roles, what we may do there).

use std::time::{Duration, Instant};

use matrix_sdk::notification_settings::RoomNotificationMode;
use matrix_sdk::ruma::api::client::room::create_room::v3::{Request as CreateRequest, RoomPreset};
use matrix_sdk::ruma::api::error::{ErrorBody, ErrorKind};
use matrix_sdk::ruma::events::InitialStateEvent;
use matrix_sdk::ruma::events::StateEventType;
use matrix_sdk::ruma::events::room::encryption::RoomEncryptionEventContent;
use matrix_sdk::ruma::events::room::member::MembershipState;
use matrix_sdk::ruma::events::room::power_levels::UserPowerLevel;
use matrix_sdk::ruma::{Int, OwnedServerName, RoomId, RoomOrAliasId, ServerName, UserId};
use matrix_sdk::{Client, Room, RoomMemberships, RoomState};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use super::MatrixEvent;
use crate::text::{sanitize, sanitize_line, truncate};

/// Above this many members, the list is fetched again at most this often while the room
/// info is open: the request is a big one.
const LARGE_ROOM: u64 = 1000;
const LARGE_ROOM_REFETCH: Duration = Duration::from_secs(60);
/// The longest piece of a server's own words that is passed on to the user.
const SERVER_TEXT: usize = 160;

pub enum ManageCmd {
    /// Join by `#alias:server` or `!id:server`. Answered with [`ManageEvent::Opened`].
    Join {
        target: String,
        via: Vec<String>,
    },
    /// Open the direct chat with this user, creating it if there is none.
    /// Answered with [`ManageEvent::Opened`].
    DirectChat {
        user_id: String,
    },
    /// Create a room. Answered with [`ManageEvent::Opened`].
    Create {
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
            Self::DirectChat { .. } => f.write_str("DirectChat"),
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
    Opened(Result<String, String>),
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
            Self::Opened(result) => write!(f, "Opened(ok: {})", result.is_ok()),
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
            ManageCmd::Join { target, via } => {
                tokio::spawn(async move {
                    emit(ManageEvent::Opened(join(&client, &target, &via).await));
                });
            }
            ManageCmd::DirectChat { user_id } => {
                tokio::spawn(async move {
                    emit(ManageEvent::Opened(direct_chat(&client, &user_id).await));
                });
            }
            ManageCmd::Create {
                name,
                topic,
                alias,
                encrypted,
            } => {
                tokio::spawn(async move {
                    let result = create(&client, name, topic, alias, encrypted).await;
                    emit(ManageEvent::Opened(result));
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
                            .set_name(name)
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
                            .set_room_topic(&topic)
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
    tracing::warn!(%err, "could not {what}");
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
    let said = server_text(err);
    let with = |ours: &str| match &said {
        Some(said) => format!("{ours} ({said})"),
        None => ours.to_owned(),
    };
    match err.client_api_error_kind() {
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
        None => match err {
            matrix_sdk::Error::Http(http) if http.as_ruma_api_error().is_none() => {
                "the server could not be reached".to_owned()
            }
            other => truncate(&sanitize_line(&other.to_string()), SERVER_TEXT),
        },
    }
}

async fn join(client: &Client, target: &str, via: &[String]) -> Result<String, String> {
    let not_an_address = || "Could not join: that is not a room address.".to_owned();
    let target = RoomOrAliasId::parse(target).map_err(|_| not_an_address())?;
    let mut via: Vec<OwnedServerName> = via
        .iter()
        .filter_map(|name| ServerName::parse(name).ok())
        .collect();
    // Where does it lead? A room we are in already is simply shown.
    let room_id = match <&RoomId>::try_from(&*target) {
        Ok(room_id) => room_id.to_owned(),
        Err(alias) => {
            let found = client
                .resolve_room_alias(alias)
                .await
                .map_err(|err| failed("join", &err.into()))?;
            for server in found.servers {
                if !via.contains(&server) {
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
    // More than a few servers only make the request longer.
    via.truncate(5);
    let joined = client
        .join_room_by_id_or_alias(&target, &via)
        .await
        .map_err(|err| failed("join", &err))?;
    Ok(joined.room_id().to_string())
}

async fn direct_chat(client: &Client, user_id: &str) -> Result<String, String> {
    let user = UserId::parse(user_id)
        .map_err(|_| "Could not start the chat: that is not a user ID.".to_owned())?;
    if client.user_id() == Some(&*user) {
        return Err("That is your own user ID.".to_owned());
    }
    if let Some(existing) = client.get_dm_room(&user) {
        return Ok(existing.room_id().to_string());
    }
    // Encrypted, marked as direct, and the other person is invited.
    let created = client
        .create_dm(&user)
        .await
        .map_err(|err| failed("start the chat", &err))?;
    Ok(created.room_id().to_string())
}

async fn create(
    client: &Client,
    name: String,
    topic: Option<String>,
    alias: Option<String>,
    encrypted: bool,
) -> Result<String, String> {
    let mut request = CreateRequest::new();
    request.name = Some(name);
    request.topic = topic;
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
    // So it does not linger on the account as a room once left. Leaving worked, which
    // is what was asked for; a server that cannot forget is not worth an alarm.
    if matches!(room.state(), RoomState::Left | RoomState::Banned)
        && let Err(err) = room.forget().await
    {
        tracing::warn!(%err, "could not forget the room after leaving");
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
            tracing::warn!(%err, "could not change the room's notifications");
            format!(
                "Could not change the notifications: {}",
                truncate(&sanitize_line(&err.to_string()), SERVER_TEXT)
            )
        })
}

/// Sends the room's details: at once from what is known, again with the complete
/// member list, and then whenever the room changes.
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
    let mut changes = room.subscribe_info();
    let joined = room.state() == RoomState::Joined;
    let mut current = details(&room, false, joined && !room.are_members_synced()).await;
    let mut sent: Option<RoomDetails> = None;
    // Counts that asking the server could not bring in line with the list: no use
    // asking again until they change.
    let mut gave_up = None;
    let mut fetched: Option<Instant> = None;
    loop {
        // The sync only brings the members it thinks are needed, so the list here can
        // be behind (someone joined, left or was removed). The room's own counts say so.
        let behind = joined
            && (current.members_loading || !counts_agree(&current))
            && gave_up != Some((current.joined, current.invited))
            && fetched.is_none_or(|at| {
                current.joined <= LARGE_ROOM || at.elapsed() >= LARGE_ROOM_REFETCH
            });
        current.members_loading = behind;
        if sent.as_ref() != Some(&current) {
            if !emit(Ok(current.clone())) {
                return;
            }
            sent = Some(current.clone());
        }
        if behind {
            // Asks the server for the list; large rooms take a moment.
            room.mark_members_missing();
            current = details(&room, true, false).await;
            fetched = Some(Instant::now());
            if !counts_agree(&current) {
                gave_up = Some((current.joined, current.invited));
            }
            if !emit(Ok(current.clone())) {
                return;
            }
            sent = Some(current);
        }
        if changes.next().await.is_none() {
            return;
        }
        // Changes come in bursts (a sync brings several); one look afterwards is enough.
        tokio::time::sleep(Duration::from_millis(300)).await;
        while futures::FutureExt::now_or_never(changes.next())
            .flatten()
            .is_some()
        {}
        if room.state() == RoomState::Left {
            return;
        }
        current = details(&room, false, false).await;
    }
}

/// Whether the member list has as many people as the room says it has.
fn counts_agree(details: &RoomDetails) -> bool {
    let invited = details.members.iter().filter(|m| m.invited).count() as u64;
    let joined = details.members.len() as u64 - invited;
    joined == details.joined && invited == details.invited
}

fn role(level: UserPowerLevel) -> MemberRole {
    match level {
        UserPowerLevel::Infinite => MemberRole::Owner,
        UserPowerLevel::Int(level) if level >= Int::from(100) => MemberRole::Admin,
        UserPowerLevel::Int(level) if level >= Int::from(50) => MemberRole::Moderator,
        _ => MemberRole::User,
    }
}

/// `fetch`: ask the server for the member list if it is not complete here.
async fn details(room: &Room, fetch: bool, members_loading: bool) -> RoomDetails {
    let joined = room.state() == RoomState::Joined;
    let own = room.own_user_id().to_owned();
    let wanted = RoomMemberships::JOIN | RoomMemberships::INVITE;
    let listed = if !joined {
        // An invitation: who is in there is not ours to ask yet.
        Ok(Vec::new())
    } else if fetch {
        room.members(wanted).await
    } else {
        room.members_no_sync(wanted).await
    };
    let listed = listed.unwrap_or_else(|err| {
        tracing::warn!(%err, "could not list the room's members");
        Vec::new()
    });
    let levels = if joined {
        room.power_levels().await.ok()
    } else {
        None
    };
    let can =
        |check: &dyn Fn(&matrix_sdk::ruma::events::room::power_levels::RoomPowerLevels) -> bool| {
            levels.as_ref().is_some_and(check)
        };
    let mut members: Vec<RoomMember> = listed
        .iter()
        .map(|member| {
            let user = member.user_id();
            RoomMember {
                user_id: sanitize_line(user.as_str()),
                name: sanitize_line(member.name()),
                role: role(member.power_level()),
                invited: *member.membership() == MembershipState::Invite,
                own: user == own,
                removable: user != own && can(&|levels| levels.user_can_kick_user(&own, user)),
            }
        })
        .collect();
    members.sort_by(|a, b| {
        (
            a.invited,
            std::cmp::Reverse(a.role),
            a.name.to_lowercase(),
            &a.user_id,
        )
            .cmp(&(
                b.invited,
                std::cmp::Reverse(b.role),
                b.name.to_lowercase(),
                &b.user_id,
            ))
    });
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
        can_rename: can(&|levels| levels.user_can_send_state(&own, StateEventType::RoomName)),
        can_set_topic: can(&|levels| levels.user_can_send_state(&own, StateEventType::RoomTopic)),
        can_invite: can(&|levels| levels.user_can_invite(&own)),
        members,
        members_loading,
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
        let event = ManageEvent::Opened(Err("Could not join #secret:example.org".into()));
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
            if let ManageEvent::Opened(result) = next_manage(events).await {
                return result;
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
            target: format!("#matrix-rat-no-such-room-{}:{server}", std::process::id()),
            via: Vec::new(),
        });
        let err = opened(&mut events).await.unwrap_err();
        assert!(err.starts_with("Could not join: not found"), "{err}");
        manage(ManageCmd::DirectChat {
            user_id: own.clone(),
        });
        assert!(opened(&mut events).await.unwrap_err().contains("your own"));

        // A private room: encrypted, and we are its admin (or owner).
        let tag = std::process::id();
        manage(ManageCmd::Create {
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
            target: room_id.clone(),
            via: Vec::new(),
        });
        assert_eq!(opened(&mut events).await.unwrap(), room_id);

        // A direct chat with bob: created once (encrypted, direct), reused afterwards.
        manage(ManageCmd::DirectChat {
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
            user_id: bob_id.clone(),
        });
        assert_eq!(opened(&mut events).await.unwrap(), chat);

        // A public room is found and joined by its address.
        let alias = format!("matrix-rat-manage-{tag}");
        manage(ManageCmd::Create {
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
}
