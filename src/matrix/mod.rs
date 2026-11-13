//! The Matrix side of the app. Everything that talks to the SDK lives in this module and
//! runs on its own task; the UI only sees [`MatrixCmd`] and [`MatrixEvent`].

mod convert;
mod media;
mod notify;
mod rooms;
mod session;
mod threads;
mod upload;
mod verify;

pub use notify::Notice;
pub use verify::{
    Answer, BackupStatus, DeviceTrust, Identity, SasEmoji, Security, SetUpOutcome, VerifyCmd,
    VerifyStage, VerifyUpdate,
};

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::{StreamExt, pin_mut};
use matrix_sdk::encryption::recovery::RecoveryState;
use matrix_sdk::{Client, RoomState, SessionChange};
use matrix_sdk_ui::eyeball_im::Vector;
use matrix_sdk_ui::room_list_service::RoomListItem;
use matrix_sdk_ui::room_list_service::filters::new_filter_non_left;
use matrix_sdk_ui::sync_service::{State, SyncService};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::format::Outgoing;
use crate::model::{ItemRef, Message};
use crate::paths::Paths;

/// Rooms shown before the user scrolls for more. Paging arrives with the timeline work.
const ROOM_PAGE: usize = 10_000;

pub enum MatrixCmd {
    LoginPassword {
        homeserver: String,
        username: String,
        password: String,
    },
    /// Log in through the homeserver's web page (OAuth 2.0).
    LoginBrowser {
        homeserver: String,
    },
    CancelLogin,
    Recover {
        key: String,
    },
    /// Start delivering this room's timeline. `generation` comes back with every
    /// update so the UI can drop ones that belong to an earlier open.
    OpenRoom {
        room_id: String,
        generation: u64,
    },
    /// Load older messages.
    Paginate {
        room_id: String,
        generation: u64,
    },
    Send {
        room_id: String,
        message: Outgoing,
        /// Event id of the message being replied to.
        reply_to: Option<String>,
    },
    Edit {
        room_id: String,
        item: ItemRef,
        message: Outgoing,
    },
    ToggleReaction {
        room_id: String,
        item: ItemRef,
        key: String,
    },
    Redact {
        room_id: String,
        item: ItemRef,
    },
    /// Try again to send a message that failed.
    Retry {
        room_id: String,
        item: ItemRef,
    },
    /// Tell the room whether we are typing.
    Typing {
        room_id: String,
        typing: bool,
    },
    MarkRead {
        room_id: String,
        /// Whether other people may see the receipt.
        public: bool,
    },
    Join {
        room_id: String,
    },
    Leave {
        room_id: String,
    },
    /// End the session on the server and go back to the login form.
    Logout,
    Shutdown,
    /// Fetch and decode a picture; answered with [`MatrixEvent::Image`].
    FetchImage {
        key: String,
        media: Box<crate::model::MediaRef>,
        /// File size as stated by the sender.
        size: Option<u64>,
        /// The whole file, not a small version.
        full: bool,
    },
    /// Device verification and cross-signing.
    Verification(VerifyCmd),
    /// Download a file and write it into `dir`; answered with [`MatrixEvent::FileSaved`].
    SaveFile {
        file: crate::model::Attachment,
        dir: std::path::PathBuf,
        /// Open it with the desktop's program for it afterwards.
        open: bool,
    },
    /// Fetch the small picture of a room; answered with [`MatrixEvent::Avatar`].
    FetchAvatar {
        /// Its `mxc://` address.
        url: String,
    },
    /// Like [`MatrixCmd::OpenRoom`], for the thread that starts at the event `root`
    /// (which may have no replies yet).
    OpenThread {
        room_id: String,
        root: String,
        generation: u64,
    },
    /// A command about messages (send, edit, react, delete, retry, load older, mark
    /// read) that is meant for the thread starting at `root`, not for the room.
    InThread {
        root: String,
        command: Box<MatrixCmd>,
    },
    /// Send files from disk, in order. They show up in the timeline at once and upload
    /// in the background.
    SendFiles {
        room_id: String,
        files: Vec<std::path::PathBuf>,
        /// Text that goes with the first file.
        caption: Option<Outgoing>,
        /// Event id of the message being replied to (by the first file).
        reply_to: Option<String>,
    },
    /// Load joined members (once per room open) for `@` completions.
    FetchMembers {
        room_id: String,
    },
}

/// Never prints the password or the recovery key, so commands are safe to log.
impl std::fmt::Debug for MatrixCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LoginPassword {
                homeserver,
                username,
                ..
            } => write!(f, "LoginPassword({username} at {homeserver})"),
            Self::LoginBrowser { homeserver } => write!(f, "LoginBrowser({homeserver})"),
            Self::CancelLogin => f.write_str("CancelLogin"),
            Self::Recover { .. } => f.write_str("Recover"),
            Self::OpenRoom { room_id, .. } => write!(f, "OpenRoom({room_id})"),
            Self::Paginate { room_id, .. } => write!(f, "Paginate({room_id})"),
            // Message text is private too.
            Self::Send { room_id, .. } => write!(f, "Send({room_id})"),
            Self::Edit { room_id, .. } => write!(f, "Edit({room_id})"),
            Self::ToggleReaction { room_id, .. } => write!(f, "ToggleReaction({room_id})"),
            Self::Redact { room_id, .. } => write!(f, "Redact({room_id})"),
            Self::Retry { room_id, .. } => write!(f, "Retry({room_id})"),
            Self::Typing { room_id, typing } => write!(f, "Typing({room_id}, {typing})"),
            Self::MarkRead { room_id, .. } => write!(f, "MarkRead({room_id})"),
            Self::Join { room_id } => write!(f, "Join({room_id})"),
            Self::Leave { room_id } => write!(f, "Leave({room_id})"),
            Self::Logout => f.write_str("Logout"),
            Self::Shutdown => f.write_str("Shutdown"),
            // The source of an encrypted file carries its key.
            Self::FetchImage { full, .. } => write!(f, "FetchImage(full: {full})"),
            Self::FetchAvatar { .. } => f.write_str("FetchAvatar"),
            Self::Verification(command) => write!(f, "Verification({command:?})"),
            Self::SaveFile { open, .. } => write!(f, "SaveFile(open: {open})"),
            Self::OpenThread { room_id, root, .. } => write!(f, "OpenThread({room_id}, {root})"),
            // The inner command prints no content either.
            Self::InThread { root, command } => write!(f, "InThread({root}, {command:?})"),
            // Paths can say a lot about the user; the caption is message text.
            Self::SendFiles { room_id, files, .. } => {
                write!(f, "SendFiles({room_id}, {} files)", files.len())
            }
            Self::FetchMembers { room_id } => write!(f, "FetchMembers({room_id})"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStatus {
    Connecting,
    Synced,
    Offline,
    Error(String),
    Stopped,
}

/// Whether this device can read encrypted history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStatus {
    Unknown,
    /// Nothing to do: recovery is set up and this device has the keys.
    Ready,
    /// The account has no recovery set up.
    NotSetUp,
    /// The account has recovery, but this device still needs the recovery key.
    NeedsKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomCategory {
    Invite,
    Favourite,
    Person,
    Room,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSummary {
    pub id: String,
    pub name: String,
    pub category: RoomCategory,
    /// Notification count, shown as a badge.
    pub notifications: u64,
    /// The count as the server reports it; unlike ours it needs nothing from us.
    pub server_notifications: u64,
    pub mention: bool,
    /// Has unread messages, even if they do not notify.
    pub unread: bool,
    /// `mxc://` address of the room's picture (for a chat with one person without
    /// a picture of its own: that person's).
    pub avatar: Option<String>,
    /// Canonical alias (e.g. `#rust:matrix.org`), when set.
    pub alias: Option<String>,
}

pub enum MatrixEvent {
    /// No usable session; show the login form. `reason` explains a failed restore.
    NeedLogin {
        reason: Option<String>,
    },
    LoginFailed(String),
    /// The page the user has to open to continue a browser login.
    BrowserLoginUrl(String),
    LoggedIn {
        user_id: String,
    },
    Sync(SyncStatus),
    Rooms(Vec<RoomSummary>),
    Recovery(RecoveryStatus),
    /// Outcome of [`MatrixCmd::Recover`]: `Err` carries a message for the user.
    RecoverResult(Result<(), String>),
    /// The complete, current content of the open room's timeline.
    Timeline {
        room_id: String,
        generation: u64,
        messages: Vec<Message>,
        /// The oldest message of the room is loaded; there is no more history.
        reached_start: bool,
    },
    /// A request for older messages finished.
    Paginated {
        generation: u64,
        reached_start: bool,
    },
    /// Who else is typing in the open room.
    Typing {
        generation: u64,
        names: Vec<String>,
    },
    /// Something the user did could not be carried out.
    ActionFailed(String),
    Error(String),
    /// Outcome of [`MatrixCmd::FetchImage`]: the pixels, or a short reason for the user.
    Image {
        key: String,
        full: bool,
        result: Result<crate::images::Decoded, String>,
    },
    /// The encryption status of this device changed.
    Security(Security),
    /// A device verification moved on.
    Verification(VerifyUpdate),
    /// Outcome of [`VerifyCmd::SetUp`].
    CrossSigningSetUp(SetUpOutcome),
    /// A message the account's notification rules consider worth a notification.
    Notice(Notice),
    /// Outcome of [`MatrixCmd::SaveFile`]: where the file is now, or what went wrong.
    FileSaved {
        key: String,
        open: bool,
        result: Result<std::path::PathBuf, String>,
    },
    /// Outcome of [`MatrixCmd::FetchAvatar`].
    Avatar {
        url: String,
        result: Result<crate::images::Decoded, String>,
    },
    /// The largest file the server accepts, in bytes.
    UploadLimit(u64),
    /// The first file of a [`MatrixCmd::SendFiles`] could not be sent, so none was:
    /// the caption and the reply belong to that one. Carries the reason.
    FilesNotSent(String),
    /// Joined members and `@room` permission for a room, used for `@` completions.
    RoomMembers {
        room_id: String,
        members: Vec<crate::complete::Member>,
        can_mention_room: bool,
    },
}

/// Hand-written so that message text never ends up in a log.
impl std::fmt::Debug for MatrixEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NeedLogin { reason } => write!(f, "NeedLogin({reason:?})"),
            Self::LoginFailed(error) => write!(f, "LoginFailed({error})"),
            Self::BrowserLoginUrl(_) => f.write_str("BrowserLoginUrl"),
            Self::LoggedIn { user_id } => write!(f, "LoggedIn({user_id})"),
            Self::Sync(status) => write!(f, "Sync({status:?})"),
            Self::Rooms(rooms) => write!(f, "Rooms({})", rooms.len()),
            Self::Recovery(status) => write!(f, "Recovery({status:?})"),
            Self::RecoverResult(result) => write!(f, "RecoverResult(ok: {})", result.is_ok()),
            Self::Timeline {
                room_id,
                generation,
                messages,
                reached_start,
            } => write!(
                f,
                "Timeline({room_id}, generation {generation}, {} messages, start: {reached_start})",
                messages.len()
            ),
            Self::Paginated {
                generation,
                reached_start,
            } => write!(
                f,
                "Paginated(generation {generation}, start: {reached_start})"
            ),
            Self::Typing { generation, names } => {
                write!(f, "Typing(generation {generation}, {})", names.len())
            }
            Self::ActionFailed(error) => write!(f, "ActionFailed({error})"),
            Self::Error(error) => write!(f, "Error({error})"),
            Self::Image { full, result, .. } => {
                write!(f, "Image(full: {full}, ok: {})", result.is_ok())
            }
            Self::Avatar { result, .. } => write!(f, "Avatar(ok: {})", result.is_ok()),
            Self::Security(security) => write!(f, "Security({security:?})"),
            Self::Verification(update) => {
                write!(f, "Verification({}, {:?})", update.flow, update.stage)
            }
            Self::CrossSigningSetUp(result) => {
                write!(f, "CrossSigningSetUp({result:?})")
            }
            Self::Notice(notice) => write!(f, "{notice:?}"),
            Self::FileSaved { result, .. } => write!(f, "FileSaved(ok: {})", result.is_ok()),
            Self::UploadLimit(bytes) => write!(f, "UploadLimit({bytes})"),
            // The reason names a file.
            Self::FilesNotSent(_) => f.write_str("FilesNotSent"),
            // Member display names and user ids are omitted from the log.
            Self::RoomMembers {
                room_id,
                members,
                can_mention_room,
            } => write!(
                f,
                "RoomMembers({room_id}, {} members, room_mention: {can_mention_room})",
                members.len()
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MatrixHandle {
    commands: UnboundedSender<MatrixCmd>,
}

impl MatrixHandle {
    /// A handle whose commands can be inspected, for tests.
    #[cfg(test)]
    pub fn for_test() -> (Self, UnboundedReceiver<MatrixCmd>) {
        let (commands, receiver) = unbounded_channel();
        (Self { commands }, receiver)
    }

    pub fn send(&self, command: MatrixCmd) {
        // A closed channel means the worker is gone, which only happens at shutdown.
        let _ = self.commands.send(command);
    }
}

/// Starts the worker on the current tokio runtime.
pub fn spawn(paths: Paths) -> (MatrixHandle, UnboundedReceiver<MatrixEvent>) {
    let (commands, command_rx) = unbounded_channel();
    let (events, event_rx) = unbounded_channel();
    tokio::spawn(async move {
        Worker {
            paths,
            commands: command_rx,
            events,
        }
        .run()
        .await;
    });
    (MatrixHandle { commands }, event_rx)
}

enum SessionEnd {
    Shutdown,
    /// The server no longer accepts our token.
    Expired,
    /// The user asked to log out.
    LoggedOut,
}

struct Worker {
    paths: Paths,
    commands: UnboundedReceiver<MatrixCmd>,
    events: UnboundedSender<MatrixEvent>,
}

impl Worker {
    fn emit(&self, event: MatrixEvent) {
        let _ = self.events.send(event);
    }

    async fn run(mut self) {
        let mut reason = None;
        loop {
            let restored = match session::restore(&self.paths).await {
                Ok(found) => found,
                Err(err) => {
                    // Restoring needs no network, so this is a local problem (damaged
                    // session file or stores) that waiting will not fix.
                    tracing::warn!(?err, "could not restore session");
                    reason = Some(format!("Could not restore your session: {err:#}"));
                    None
                }
            };
            let (client, passphrase) = match restored {
                Some(pair) => pair,
                None => {
                    self.emit(MatrixEvent::NeedLogin {
                        reason: reason.take(),
                    });
                    match self.wait_for_login().await {
                        Some(pair) => pair,
                        None => return,
                    }
                }
            };
            match self.run_session(&client, &passphrase).await {
                Ok(SessionEnd::Shutdown) => return,
                Ok(SessionEnd::Expired) => {
                    session::forget(&self.paths);
                    reason = Some("Your session has expired. Please log in again.".to_owned());
                }
                Ok(SessionEnd::LoggedOut) => {
                    // Tell the server, but log out here even if it cannot be reached.
                    match tokio::time::timeout(Duration::from_secs(10), client.logout()).await {
                        Ok(Ok(())) => {}
                        outcome => {
                            tracing::warn!(?outcome, "could not log out on the server");
                            reason = Some(
                                "Logged out here. The server could not be told, so the session \
                                 may still be listed on your account."
                                    .to_owned(),
                            );
                        }
                    }
                    drop(client);
                    session::clear(&self.paths);
                }
                Err(err) => {
                    tracing::error!(?err, "session failed");
                    self.emit(MatrixEvent::Error(format!("{err:#}")));
                    // Keep the session on disk: this may be a temporary problem.
                    // Try again shortly, unless told to stop first.
                    let retry = tokio::time::sleep(Duration::from_secs(5));
                    tokio::pin!(retry);
                    loop {
                        tokio::select! {
                            _ = &mut retry => break,
                            command = self.commands.recv() => match command {
                                Some(MatrixCmd::Shutdown) | None => return,
                                Some(_) => {}
                            },
                        }
                    }
                }
            }
        }
    }

    /// Handles login commands until one succeeds. `None` means shut down.
    async fn wait_for_login(&mut self) -> Option<(Client, String)> {
        let Worker {
            paths,
            commands,
            events,
        } = self;
        loop {
            let request = match commands.recv().await? {
                MatrixCmd::Shutdown => return None,
                request @ (MatrixCmd::LoginPassword { .. } | MatrixCmd::LoginBrowser { .. }) => {
                    request
                }
                // Anything else was meant for a session that has ended.
                _ => continue,
            };
            let login = async {
                match &request {
                    MatrixCmd::LoginPassword {
                        homeserver,
                        username,
                        password,
                    } => session::login_password(paths, homeserver, username, password).await,
                    MatrixCmd::LoginBrowser { homeserver } => {
                        session::login_browser(paths, homeserver, events).await
                    }
                    _ => unreachable!("filtered above"),
                }
            };
            tokio::pin!(login);
            // The login runs alongside the command channel, so Esc and quitting work
            // at every stage. Dropping the future abandons the attempt.
            let result = loop {
                tokio::select! {
                    result = &mut login => break Some(result),
                    command = commands.recv() => match command {
                        Some(MatrixCmd::Shutdown) | None => return None,
                        Some(MatrixCmd::CancelLogin) => break None,
                        Some(_) => {}
                    },
                }
            };
            match result {
                Some(Ok(pair)) => return Some(pair),
                Some(Err(err)) => {
                    tracing::warn!(?err, "login failed");
                    let _ = events.send(MatrixEvent::LoginFailed(format!("{err:#}")));
                }
                // Cancelled by the user: nothing to report.
                None => {}
            }
        }
    }

    async fn run_session(&mut self, client: &Client, passphrase: &str) -> Result<SessionEnd> {
        let user_id = client
            .user_id()
            .ok_or_else(|| anyhow!("logged in without a user id"))?
            .to_string();
        self.emit(MatrixEvent::LoggedIn { user_id });
        self.emit(MatrixEvent::Sync(SyncStatus::Connecting));

        // Refreshed tokens replace the old ones on the server at once, so they must
        // reach the disk at once too: a session file with a used-up refresh token logs
        // the user out at the next start. Listening begins before the first request
        // (the very first sync usually refreshes), in a task of its own so that no slow
        // step of the main loop can delay a save.
        let _notices = notify::watch(client, self.events.clone()).await;
        let mut session_changes = client.subscribe_to_session_changes();
        let token_saver = {
            let mut changes = client.subscribe_to_session_changes();
            let (paths, client, passphrase) =
                (self.paths.clone(), client.clone(), passphrase.to_owned());
            tokio::spawn(async move {
                use tokio::sync::broadcast::error::RecvError;
                loop {
                    match changes.recv().await {
                        // Having missed news means tokens may have changed: save.
                        Ok(SessionChange::TokensRefreshed) | Err(RecvError::Lagged(_)) => {
                            if let Err(err) = session::save(&paths, &client, &passphrase) {
                                tracing::error!(?err, "could not save refreshed tokens");
                            }
                        }
                        Ok(SessionChange::UnknownToken { .. }) => {}
                        Err(RecvError::Closed) => break,
                    }
                }
            })
        };

        // Offline mode: the service notices a lost connection, reports `Offline` and
        // resumes by itself when the server is reachable again.
        let sync = SyncService::builder(client.clone())
            .with_offline_mode()
            .build()
            .await
            .context("could not set up sync")?;
        sync.start().await;

        let room_list = sync
            .room_list_service()
            .all_rooms()
            .await
            .context("could not open the room list")?;
        let (entries, controller) = room_list.entries_with_dynamic_adapters(ROOM_PAGE);
        // Nothing is emitted until a filter is set.
        controller.set_filter(Box::new(new_filter_non_left()));
        pin_mut!(entries);

        let mut sync_state = sync.state();
        // The subscriber only reports changes; the service may already be running.
        self.emit(MatrixEvent::Sync(sync_status(&sync_state.get())));
        let recovery = client.encryption().recovery();
        let recovery_states = recovery.state_stream();
        pin_mut!(recovery_states);
        self.emit(MatrixEvent::Recovery(recovery_status(recovery.state())));

        let mut rooms: Vector<RoomListItem> = Vector::new();
        let mut timelines = rooms::Timelines::new(client.clone(), self.events.clone());
        let mut own_members = notify::OwnMembers::default();
        let mut verifier = verify::Verifier::start(client, self.events.clone());
        let end = loop {
            tokio::select! {
                incoming = verifier.next_incoming() => verifier.on_incoming(incoming).await,
                Some(diffs) = entries.next() => {
                    for diff in diffs {
                        diff.apply(&mut rooms);
                    }
                    own_members.ensure(&rooms).await;
                    _notices.arm();
                    self.emit(MatrixEvent::Rooms(summarize(&rooms).await));
                }
                () = own_members.finished() => own_members.ensure(&rooms).await,
                Some(state) = sync_state.next() => {
                    self.emit(MatrixEvent::Sync(sync_status(&state)));
                }
                Some(state) = recovery_states.next() => {
                    self.emit(MatrixEvent::Recovery(recovery_status(state)));
                }
                change = session_changes.recv() => match change {
                    Ok(SessionChange::UnknownToken(info)) => {
                        tracing::warn!(?info, "the server no longer accepts this session");
                        break SessionEnd::Expired;
                    }
                    // Saving is the token saver's job.
                    Ok(SessionChange::TokensRefreshed) | Err(_) => {}
                },
                command = self.commands.recv() => match command {
                    Some(MatrixCmd::Recover { key }) => {
                        // Can take a while (downloads keys); must not block the loop.
                        let recovery = client.encryption().recovery();
                        let events = self.events.clone();
                        tokio::spawn(async move {
                            let result = recovery
                                .recover(key.trim())
                                .await
                                .map_err(|err| err.to_string());
                            let _ = events.send(MatrixEvent::RecoverResult(result));
                        });
                    }
                    Some(MatrixCmd::Shutdown) | None => break SessionEnd::Shutdown,
                    Some(MatrixCmd::Logout) => break SessionEnd::LoggedOut,
                    Some(MatrixCmd::Verification(command)) => verifier.handle(command).await,
                    Some(command) => timelines.handle(command).await,
                },
            }
        };
        verifier.stop().await;
        timelines.close();
        // Do not hang on exit if the server is unreachable.
        let _ = tokio::time::timeout(Duration::from_secs(2), sync.stop()).await;
        token_saver.abort();
        // Once more on the way out, in case a refresh happened during the shutdown.
        if matches!(end, SessionEnd::Shutdown)
            && let Err(err) = session::save(&self.paths, client, passphrase)
        {
            tracing::error!(?err, "could not save the session");
        }
        Ok(end)
    }
}

fn sync_status(state: &State) -> SyncStatus {
    match state {
        State::Idle => SyncStatus::Connecting,
        State::Running => SyncStatus::Synced,
        State::Offline => SyncStatus::Offline,
        State::Terminated => SyncStatus::Stopped,
        State::Error(err) => SyncStatus::Error(err.to_string()),
    }
}

fn recovery_status(state: RecoveryState) -> RecoveryStatus {
    match state {
        RecoveryState::Unknown => RecoveryStatus::Unknown,
        RecoveryState::Enabled => RecoveryStatus::Ready,
        RecoveryState::Disabled => RecoveryStatus::NotSetUp,
        RecoveryState::Incomplete => RecoveryStatus::NeedsKey,
    }
}

async fn summarize(rooms: &Vector<RoomListItem>) -> Vec<RoomSummary> {
    let mut summaries = Vec::with_capacity(rooms.len());
    for room in rooms {
        // Spaces get their own navigation later; they are not chats.
        if room.is_space() {
            continue;
        }
        let invited = room.state() == RoomState::Invited;
        let direct = room.is_direct().await.unwrap_or(false);
        let category = if invited {
            RoomCategory::Invite
        } else if room.is_favourite() {
            RoomCategory::Favourite
        } else if direct {
            RoomCategory::Person
        } else {
            RoomCategory::Room
        };
        let name = match room.cached_display_name() {
            Some(name) => name.to_string(),
            None => room
                .display_name()
                .await
                .map(|name| name.to_string())
                .unwrap_or_else(|_| room.room_id().to_string()),
        };
        // No picture for invitations: fetching one would tell a stranger's server
        // that the invitation was seen, before the user decided anything.
        let avatar = if invited {
            None
        } else if let Some(avatar) = room.avatar_url() {
            Some(avatar)
        } else if direct {
            // The other person's picture, known from the room's summary: no members
            // have to be loaded for it.
            match room.heroes().await.as_slice() {
                [other] => other.avatar_url.clone(),
                _ => None,
            }
        } else {
            None
        };
        let notifications = room.num_unread_notifications();
        summaries.push(RoomSummary {
            id: room.room_id().to_string(),
            name: crate::text::sanitize_line(&name),
            category,
            notifications,
            server_notifications: room.unread_notification_counts().notification_count,
            mention: room.num_unread_mentions() > 0,
            unread: notifications > 0 || room.num_unread_messages() > 0 || room.is_marked_unread(),
            avatar: avatar
                .filter(|uri| {
                    uri.parts().is_ok_and(|(server, id)| {
                        !matches!(server.as_str(), "" | "." | "..") && !id.is_empty()
                    })
                })
                .map(|uri| uri.to_string())
                .filter(|uri| !uri.contains(|c: char| c.is_control() || c.is_whitespace())),
            alias: room
                .canonical_alias()
                .map(|alias| crate::text::sanitize_line(alias.as_str())),
        });
    }
    summaries
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end check against a local homeserver. Start one with
    /// `scripts/dev-homeserver.sh`, create the user, then run:
    /// `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=... cargo test -- --ignored`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn password_login_sync_and_restore() {
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let username = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");

        let root = std::env::temp_dir().join(format!("matrix-rat-it-{}", std::process::id()));
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "it".into(),
        };
        paths.create_dirs().unwrap();

        async fn until_rooms(
            events: &mut UnboundedReceiver<MatrixEvent>,
        ) -> (bool, Vec<RoomSummary>) {
            let mut needed_login = false;
            let wait = async {
                loop {
                    match events.recv().await.expect("worker stopped") {
                        MatrixEvent::NeedLogin { .. } => needed_login = true,
                        MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                        MatrixEvent::Rooms(rooms) if !rooms.is_empty() => return rooms,
                        _ => {}
                    }
                    if needed_login {
                        return Vec::new();
                    }
                }
            };
            let rooms = tokio::time::timeout(Duration::from_secs(30), wait)
                .await
                .expect("timed out waiting for rooms");
            (needed_login, rooms)
        }

        // First run: no session, so the worker asks for a login.
        let (handle, mut events) = spawn(paths.clone());
        let (needed_login, _) = until_rooms(&mut events).await;
        assert!(needed_login);
        handle.send(MatrixCmd::LoginPassword {
            homeserver,
            username,
            password,
        });
        let (_, rooms) = until_rooms(&mut events).await;
        assert!(!rooms.is_empty());
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}

        // Second run: the saved session is restored without logging in again.
        let (handle, mut events) = spawn(paths.clone());
        let (needed_login, rooms) = until_rooms(&mut events).await;
        assert!(!needed_login, "session was not restored");
        assert!(!rooms.is_empty());
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = std::fs::remove_dir_all(root);
    }

    /// Sends a message into the first joined room and waits for the server to confirm
    /// it. Same setup as above; the user must have joined at least one room.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn send_and_see_it_confirmed() {
        use crate::model::SendState;
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let username = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let root = std::env::temp_dir().join(format!("matrix-rat-send-{}", std::process::id()));
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
            password,
        });

        let body = format!("integration test {}", std::process::id());
        let wait = async {
            let mut room_id = None;
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err)
                    | MatrixEvent::Error(err)
                    | MatrixEvent::ActionFailed(err) => panic!("{err}"),
                    MatrixEvent::Rooms(rooms) if room_id.is_none() => {
                        let joined = rooms.iter().find(|r| r.category != RoomCategory::Invite);
                        if let Some(room) = joined {
                            room_id = Some(room.id.clone());
                            handle.send(MatrixCmd::OpenRoom {
                                room_id: room.id.clone(),
                                generation: 1,
                            });
                            handle.send(MatrixCmd::Send {
                                room_id: room.id.clone(),
                                message: crate::format::parse(&format!("**{body}**")),
                                reply_to: None,
                            });
                        }
                    }
                    MatrixEvent::Timeline {
                        generation: 1,
                        messages,
                        ..
                    } => {
                        // The debug output must not contain message text.
                        let debug = format!(
                            "{:?}",
                            MatrixEvent::Timeline {
                                room_id: String::new(),
                                generation: 1,
                                messages: messages.clone(),
                                reached_start: false,
                            }
                        );
                        assert!(!debug.contains(&body));
                        let confirmed = messages.iter().any(|m| {
                            m.body.contains(&body)
                                && m.state == SendState::Sent
                                && m.own
                                && m.html.as_deref().is_some_and(|h| h.contains("<strong>"))
                                && matches!(m.item, Some(ItemRef::Event(_)))
                        });
                        if confirmed {
                            return;
                        }
                    }
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(30), wait)
            .await
            .expect("timed out waiting for the sent message");
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = std::fs::remove_dir_all(root);
    }
}
