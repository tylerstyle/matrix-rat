//! The Matrix side of the app. Everything that talks to the SDK lives in this module and
//! runs on its own task; the UI only sees [`MatrixCmd`] and [`MatrixEvent`].

mod convert;
mod drafts;
mod keyring;
mod manage;
mod media;
mod notify;
mod receipts;
mod rooms;
pub(crate) mod search;
mod session;
mod spaces;
mod threads;
mod upload;
mod verify;

pub use keyring::SecretStoreMode;
pub use manage::{
    Change, ManageCmd, ManageEvent, MemberRole, NotifyLevel, RoomDetails, RoomMember,
};
pub use notify::Notice;
pub use search::SearchHit;
pub use session::{ReauthKind, ReauthTarget};
pub use spaces::SpaceSummary;
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
/// Rooms (from the top of the list) that get a latest-message preview.
const LATEST_ROOMS: usize = 500;

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
    /// Open a room (or thread) focused around a specific event id via `/context` when
    /// it is not already in a live timeline. Answered with [`MatrixEvent::Timeline`] for
    /// `generation`, or [`MatrixEvent::JumpFailed`] if unavailable.
    OpenEvent {
        room_id: String,
        event_id: String,
        thread_root: Option<String>,
        generation: u64,
    },
    /// Abort an in-flight [`MatrixCmd::OpenEvent`] if its `generation` is still pending,
    /// leaving whatever timeline is currently active untouched.
    CancelOpenEvent {
        generation: u64,
    },
    /// Load older messages.
    Paginate {
        room_id: String,
        generation: u64,
    },
    /// Load newer messages on an event-focused detached timeline.
    PaginateForwards {
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
    /// Everything in the room (or, inside [`MatrixCmd::InThread`], the thread) has been
    /// read: sends the receipt and, for a room, moves the read marker (`m.fully_read`)
    /// and takes back a "marked as unread".
    MarkRead {
        room_id: String,
        /// Whether other people may see the receipt.
        public: bool,
    },
    /// Sets or takes back the "marked as unread" flag of a room (`m.marked_unread`
    /// in its account data, shared with the user's other clients).
    SetUnread {
        room_id: String,
        unread: bool,
    },
    /// [`MatrixCmd::MarkRead`] for many rooms, one after the other in the background,
    /// without keeping their timelines open.
    MarkRoomsRead {
        room_ids: Vec<String>,
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
    /// Whether the room list shows latest messages; without, they are not worked out.
    Previews(bool),
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
    /// Joining, creating, leaving and looking at rooms.
    Manage(ManageCmd),
    /// Re-authenticate a soft-logged-out session with a password, keeping the existing device id.
    ReauthPassword {
        password: String,
    },
    /// Re-authenticate a soft-logged-out session via browser, keeping the existing device id.
    ReauthBrowser,
    /// Searches indexed messages across joined rooms.
    SearchMessages {
        generation: u64,
        query: String,
    },
    /// Cancels any running global message search.
    CancelSearch,
    /// Write the unsent text of a room to the store, to have it back after a restart
    /// (see [`MatrixEvent::Draft`]). Empty text removes what is stored. Inside
    /// [`MatrixCmd::InThread`] it is that thread's draft.
    SaveDraft {
        room_id: String,
        text: String,
    },
    /// The room is gone from the room list: remove its stored draft and those of
    /// these threads (by the event they start at), unless the account is still in
    /// the room after all.
    ForgetDrafts {
        room_id: String,
        threads: Vec<String>,
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
            Self::OpenEvent {
                room_id,
                generation,
                ..
            } => write!(f, "OpenEvent({room_id}, generation {generation})"),
            Self::CancelOpenEvent { generation } => {
                write!(f, "CancelOpenEvent(generation {generation})")
            }
            Self::Paginate { room_id, .. } => write!(f, "Paginate({room_id})"),
            Self::PaginateForwards { room_id, .. } => write!(f, "PaginateForwards({room_id})"),
            // Message text is private too.
            Self::Send { room_id, .. } => write!(f, "Send({room_id})"),
            Self::Edit { room_id, .. } => write!(f, "Edit({room_id})"),
            Self::ToggleReaction { room_id, .. } => write!(f, "ToggleReaction({room_id})"),
            Self::Redact { room_id, .. } => write!(f, "Redact({room_id})"),
            Self::Retry { room_id, .. } => write!(f, "Retry({room_id})"),
            Self::Typing { room_id, typing } => write!(f, "Typing({room_id}, {typing})"),
            Self::MarkRead { room_id, .. } => write!(f, "MarkRead({room_id})"),
            Self::SetUnread { room_id, unread } => write!(f, "SetUnread({room_id}, {unread})"),
            Self::MarkRoomsRead { room_ids, .. } => {
                write!(f, "MarkRoomsRead({} rooms)", room_ids.len())
            }
            Self::Join { room_id } => write!(f, "Join({room_id})"),
            Self::Leave { room_id } => write!(f, "Leave({room_id})"),
            Self::Logout => f.write_str("Logout"),
            Self::Previews(wanted) => write!(f, "Previews({wanted})"),
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
            Self::Manage(command) => write!(f, "Manage({command:?})"),
            Self::ReauthPassword { .. } => f.write_str("ReauthPassword"),
            Self::ReauthBrowser => f.write_str("ReauthBrowser"),
            // Query text is private too.
            Self::SearchMessages { generation, .. } => {
                write!(f, "SearchMessages(generation {generation})")
            }
            Self::CancelSearch => f.write_str("CancelSearch"),
            // A draft is message text.
            Self::SaveDraft { room_id, text } => {
                write!(f, "SaveDraft({room_id}, {} bytes)", text.len())
            }
            Self::ForgetDrafts { room_id, threads } => {
                write!(f, "ForgetDrafts({room_id}, {} threads)", threads.len())
            }
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
    /// The latest message in one line ("ann: see you then"), if known.
    pub preview: Option<String>,
    /// The count as the server reports it; unlike ours it needs nothing from us.
    pub server_notifications: u64,
    pub mention: bool,
    /// Has unread messages, even if they do not notify.
    pub unread: bool,
    /// The user marked the room as unread by hand, here or in another client.
    pub marked_unread: bool,
    /// `mxc://` address of the room's picture (for a chat with one person without
    /// a picture of its own: that person's).
    pub avatar: Option<String>,
    /// Canonical alias (e.g. `#rust:matrix.org`), when set.
    pub alias: Option<String>,
}

/// Metadata attached to [`MatrixEvent::Timeline`] when it shows a detached
/// slice centered around an older event rather than the live tip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedTimeline {
    /// Event id the detached slice was centered on.
    pub target_event: String,
    /// Thread root event id when `/context` resolved to a thread timeline.
    pub thread_root: Option<String>,
    /// Whether forward pagination has already reached the live end.
    pub reached_end: bool,
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
    /// The first sync of the session is through: what was missed while the app was
    /// not running has arrived (or is arriving in the open room right now). Sent
    /// once per session.
    CaughtUp,
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
        /// `Some` when this timeline is a detached `/context` view rather than live.
        detached: Option<DetachedTimeline>,
    },
    /// A request for older messages finished.
    Paginated {
        generation: u64,
        reached_start: bool,
    },
    /// A request for newer messages on a detached timeline finished.
    PaginatedForwards {
        generation: u64,
        reached_end: bool,
    },
    /// Opening an event-focused timeline failed; the previous view was kept.
    JumpFailed {
        generation: u64,
        reason: String,
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
    /// The spaces the user is in, with the rooms in each. Replaces the previous list.
    Spaces(Vec<SpaceSummary>),
    /// A message the account's notification rules consider worth a notification.
    Notice(Notice),
    /// The user clicked the desktop notification shown for this room. Comes from the
    /// notification service, not from the worker.
    NoticeClicked {
        room_id: String,
    },
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
    /// An answer to a [`MatrixCmd::Manage`].
    Manage(ManageEvent),
    /// The current session suffered a soft logout; ask the user to re-authenticate to keep
    /// the existing device and local encrypted store.
    NeedReauth {
        target: ReauthTarget,
        reason: String,
    },
    /// Results (partial or complete) from [`MatrixCmd::SearchMessages`].
    SearchResults {
        generation: u64,
        hits: Vec<SearchHit>,
        done: bool,
        note: Option<String>,
    },
    /// The draft the store holds for a room, or for the thread starting at `thread`;
    /// empty if there is none. Sent once per session, when the room or thread is
    /// opened for the first time.
    Draft {
        room_id: String,
        thread: Option<String>,
        text: String,
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
            Self::CaughtUp => f.write_str("CaughtUp"),
            Self::Rooms(rooms) => write!(f, "Rooms({})", rooms.len()),
            Self::Recovery(status) => write!(f, "Recovery({status:?})"),
            Self::RecoverResult(result) => write!(f, "RecoverResult(ok: {})", result.is_ok()),
            Self::Timeline {
                room_id,
                generation,
                messages,
                reached_start,
                detached,
            } => write!(
                f,
                "Timeline({room_id}, generation {generation}, {} messages, start: {reached_start}, detached: {})",
                messages.len(),
                detached.is_some()
            ),
            Self::Paginated {
                generation,
                reached_start,
            } => write!(
                f,
                "Paginated(generation {generation}, start: {reached_start})"
            ),
            Self::PaginatedForwards {
                generation,
                reached_end,
            } => write!(
                f,
                "PaginatedForwards(generation {generation}, end: {reached_end})"
            ),
            Self::JumpFailed { generation, reason } => {
                write!(f, "JumpFailed(generation {generation}, {reason})")
            }
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
            Self::Spaces(spaces) => write!(f, "Spaces({})", spaces.len()),
            Self::Notice(notice) => write!(f, "{notice:?}"),
            Self::NoticeClicked { room_id } => write!(f, "NoticeClicked({room_id})"),
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
            Self::Manage(event) => write!(f, "Manage({event:?})"),
            Self::NeedReauth { target, .. } => {
                write!(f, "NeedReauth({} dev={})", target.user_id, target.device_id)
            }
            Self::SearchResults {
                generation,
                hits,
                done,
                note,
            } => write!(
                f,
                "SearchResults(generation {generation}, {} hits, done: {done}, note: {})",
                hits.len(),
                note.is_some()
            ),
            // A draft is message text.
            Self::Draft {
                room_id,
                thread,
                text,
            } => match thread {
                Some(root) => write!(f, "Draft({room_id}, {root}, {} bytes)", text.len()),
                None => write!(f, "Draft({room_id}, {} bytes)", text.len()),
            },
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

/// Starts the worker on the current tokio runtime using `SecretStoreMode::Auto`.
#[allow(dead_code)]
pub fn spawn(paths: Paths) -> (MatrixHandle, UnboundedReceiver<MatrixEvent>) {
    spawn_with(paths, SecretStoreMode::Auto)
}

/// Starts the worker on the current tokio runtime with the configured `secret_store` policy.
pub fn spawn_with(
    paths: Paths,
    secret_store: SecretStoreMode,
) -> (MatrixHandle, UnboundedReceiver<MatrixEvent>) {
    let (commands, command_rx) = unbounded_channel();
    let (events, event_rx) = unbounded_channel();
    tokio::spawn(async move {
        Worker {
            paths,
            secret_store,
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
    /// The server rejected the token with `soft_logout = true`; keep device & store.
    SoftLogout,
    /// The user asked to log out.
    LoggedOut,
}

enum PendingAuth {
    Fresh {
        reason: Option<String>,
    },
    Reauth {
        target: ReauthTarget,
        passphrase: String,
        in_keyring: bool,
        reason: String,
    },
}

struct Worker {
    paths: Paths,
    secret_store: SecretStoreMode,
    commands: UnboundedReceiver<MatrixCmd>,
    events: UnboundedSender<MatrixEvent>,
}

impl Worker {
    fn emit(&self, event: MatrixEvent) {
        let _ = self.events.send(event);
    }

    async fn run(mut self) {
        let mut pending: Option<PendingAuth> = None;
        loop {
            let auth = match pending.take() {
                Some(step) => Some(step),
                None => match session::restore(&self.paths, self.secret_store).await {
                    session::RestoreOutcome::Restored(active) => {
                        let res = self.run_active_session(active, None, &mut pending).await;
                        if !res {
                            return;
                        }
                        continue;
                    }
                    session::RestoreOutcome::NoSession => Some(PendingAuth::Fresh { reason: None }),
                    session::RestoreOutcome::NeedReauth {
                        target,
                        passphrase,
                        in_keyring,
                    } => Some(PendingAuth::Reauth {
                        target,
                        passphrase,
                        in_keyring,
                        reason: "Your previous session expired. Log in again to keep this device and its verification.".to_owned(),
                    }),
                    session::RestoreOutcome::Blocked(reason) => {
                        Some(PendingAuth::Fresh { reason: Some(reason) })
                    }
                },
            };

            let (active, post_notice) = match auth {
                Some(PendingAuth::Fresh { reason }) => {
                    self.emit(MatrixEvent::NeedLogin { reason });
                    let Some(pair) = self.wait_for_login(None).await else {
                        return;
                    };
                    pair
                }
                Some(PendingAuth::Reauth {
                    target,
                    passphrase,
                    in_keyring,
                    reason,
                }) => {
                    self.emit(MatrixEvent::NeedReauth {
                        target: target.clone(),
                        reason,
                    });
                    let Some(pair) = self
                        .wait_for_login(Some((target, passphrase, in_keyring)))
                        .await
                    else {
                        return;
                    };
                    pair
                }
                None => unreachable!(),
            };
            if !self
                .run_active_session(active, post_notice, &mut pending)
                .await
            {
                return;
            }
        }
    }

    /// Runs one authenticated session to completion. Returns `false` if the worker should stop.
    async fn run_active_session(
        &mut self,
        active: session::ActiveSession,
        post_notice: Option<String>,
        pending: &mut Option<PendingAuth>,
    ) -> bool {
        let outcome = self.run_session(&active, post_notice).await;
        match outcome {
            Ok(SessionEnd::Shutdown) => {
                let joined = search::joined_room_ids(&active.client);
                drop(active);
                search::prune_search_dir(&self.paths.search_dir(), &joined);
                return false;
            }
            Ok(SessionEnd::SoftLogout) => {
                let target = active.target.clone();
                let passphrase = active.passphrase.clone();
                let in_keyring = active.in_keyring;
                if let Err(err) = session::mark_soft_logout(&self.paths, &active) {
                    tracing::warn!(?err, "could not record soft logout state");
                }
                let joined = search::joined_room_ids(&active.client);
                drop(active);
                search::prune_search_dir(&self.paths.search_dir(), &joined);
                *pending = Some(PendingAuth::Reauth {
                    target,
                    passphrase,
                    in_keyring,
                    reason: "Your session expired (soft logout). Log in again to keep this device and its verification.".to_owned(),
                });
            }
            Ok(SessionEnd::Expired) => {
                let target = active.target.clone();
                drop(active);
                session::forget(&self.paths, Some(&target)).await;
                *pending = Some(PendingAuth::Fresh {
                    reason: Some("Your session has expired. Please log in again.".to_owned()),
                });
            }
            Ok(SessionEnd::LoggedOut) => {
                let target = active.target.clone();
                let mut reason = None;
                // Tell the server, but log out here even if it cannot be reached.
                match tokio::time::timeout(Duration::from_secs(10), active.client.logout()).await {
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
                drop(active);
                session::clear(&self.paths, Some(&target)).await;
                *pending = Some(PendingAuth::Fresh { reason });
            }
            Err(err) => {
                drop(active);
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
                            Some(MatrixCmd::Shutdown) | None => return false,
                            Some(_) => {}
                        },
                    }
                }
            }
        }
        true
    }

    /// Handles login (and soft-logout re-authentication) commands until one succeeds.
    /// Returns the active session and any non-fatal warning notice (`None` means shut down).
    async fn wait_for_login(
        &mut self,
        reauth: Option<(ReauthTarget, String, bool)>,
    ) -> Option<(session::ActiveSession, Option<String>)> {
        let Worker {
            paths,
            secret_store,
            commands,
            events,
        } = self;
        let secret_store = *secret_store;
        loop {
            let request = match commands.recv().await? {
                MatrixCmd::Shutdown => return None,
                request @ (MatrixCmd::LoginPassword { .. }
                | MatrixCmd::LoginBrowser { .. }
                | MatrixCmd::ReauthPassword { .. }
                | MatrixCmd::ReauthBrowser) => request,
                // Anything else was meant for a session that has ended.
                _ => continue,
            };
            let login = async {
                match &request {
                    MatrixCmd::LoginPassword {
                        homeserver,
                        username,
                        password,
                    } => {
                        let s = session::login_password(
                            paths,
                            homeserver,
                            username,
                            password,
                            secret_store,
                        )
                        .await?;
                        Ok((s, None))
                    }
                    MatrixCmd::LoginBrowser { homeserver } => {
                        let s =
                            session::login_browser(paths, homeserver, events, secret_store).await?;
                        Ok((s, None))
                    }
                    MatrixCmd::ReauthPassword { password } => {
                        let Some((target, passphrase, in_keyring)) = reauth.as_ref() else {
                            anyhow::bail!(
                                "session is not awaiting re-authentication; please log in again"
                            );
                        };
                        let outcome = session::reauth_password(
                            paths,
                            target,
                            password,
                            passphrase,
                            *in_keyring,
                            secret_store,
                        )
                        .await?;
                        let notice = (!outcome.kept_device).then(|| {
                            "Server issued a new device ID; started a fresh local store.".to_owned()
                        });
                        Ok((outcome.session, notice))
                    }
                    MatrixCmd::ReauthBrowser => {
                        let Some((target, passphrase, in_keyring)) = reauth.as_ref() else {
                            anyhow::bail!(
                                "session is not awaiting re-authentication; please log in again"
                            );
                        };
                        let outcome = session::reauth_browser(
                            paths,
                            target,
                            events,
                            passphrase,
                            *in_keyring,
                            secret_store,
                        )
                        .await?;
                        let notice = (!outcome.kept_device).then(|| {
                            "Server issued a new device ID; started a fresh local store.".to_owned()
                        });
                        Ok((outcome.session, notice))
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
                Some(Ok(ok)) => return Some(ok),
                Some(Err(err)) => {
                    tracing::warn!(?err, "login failed");
                    let _ = events.send(MatrixEvent::LoginFailed(format!("{err:#}")));
                }
                // Cancelled by the user: nothing to report.
                None => {}
            }
        }
    }

    async fn run_session(
        &mut self,
        active: &session::ActiveSession,
        post_notice: Option<String>,
    ) -> Result<SessionEnd> {
        let client = &active.client;
        let passphrase = active.passphrase.as_str();
        let in_keyring = active.in_keyring;
        let user_id = client
            .user_id()
            .ok_or_else(|| anyhow!("logged in without a user id"))?
            .to_string();
        self.emit(MatrixEvent::LoggedIn { user_id });
        if let Some(notice) = post_notice {
            self.emit(MatrixEvent::ActionFailed(notice));
        }
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
                            if let Err(err) =
                                session::save_tokens(&paths, &client, &passphrase, in_keyring)
                            {
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
            .with_room_list_timeline_limit(20)
            .build()
            .await
            .context("could not set up sync")?;
        // Start listening for redecryption reports before sync begins so any
        // newly decrypted UTDs are forwarded into the search index.
        let mut searcher = search::Searcher::new(client.clone(), self.events.clone());
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
        // The room list leaves its first state when the first answer of the server
        // has been worked in.
        let mut list_state = sync.room_list_service().state();
        let mut caught_up = false;
        if caught_up_in(&list_state.get()) {
            caught_up = true;
            self.emit(MatrixEvent::CaughtUp);
        }
        let recovery = client.encryption().recovery();
        let recovery_states = recovery.state_stream();
        pin_mut!(recovery_states);
        self.emit(MatrixEvent::Recovery(recovery_status(recovery.state())));

        let mut rooms: Vector<RoomListItem> = Vector::new();
        let mut timelines = rooms::Timelines::new(client.clone(), self.events.clone());
        let mut own_members = notify::OwnMembers::default();
        let mut previews = Previews::default();
        let mut verifier = verify::Verifier::start(client, self.events.clone());
        // Kept alive for the session: it follows the space hierarchy by itself.
        let space_service = matrix_sdk_ui::spaces::SpaceService::new(client.clone()).await;
        let (mut space_filters, space_changes) = space_service.subscribe_to_space_filters().await;
        pin_mut!(space_changes);
        self.emit(MatrixEvent::Spaces(spaces::summarize(&space_filters)));

        let mut manager = manage::Manager::new(client.clone(), self.events.clone());
        let end = loop {
            tokio::select! {
                Some(diffs) = space_changes.next() => {
                    for diff in diffs {
                        diff.apply(&mut space_filters);
                    }
                    self.emit(MatrixEvent::Spaces(spaces::summarize(&space_filters)));
                }
                incoming = verifier.next_incoming() => verifier.on_incoming(incoming).await,
                Some(diffs) = entries.next() => {
                    for diff in diffs {
                        diff.apply(&mut rooms);
                    }
                    own_members.ensure(&rooms).await;
                    // The SDK works out a room's latest message only for rooms it was
                    // asked to; ask once for each (it needs the event cache running).
                    previews.listen(client, &rooms);
                    _notices.arm();
                    self.emit(MatrixEvent::Rooms(summarize(&rooms, &mut previews).await));
                }
                () = own_members.finished() => own_members.ensure(&rooms).await,
                Some(opened) = timelines.next_opened() => timelines.on_opened(opened).await,
                Some(state) = sync_state.next() => {
                    self.emit(MatrixEvent::Sync(sync_status(&state)));
                }
                Some(state) = list_state.next(), if !caught_up => {
                    if caught_up_in(&state) {
                        caught_up = true;
                        self.emit(MatrixEvent::CaughtUp);
                    }
                }
                Some(state) = recovery_states.next() => {
                    self.emit(MatrixEvent::Recovery(recovery_status(state)));
                }
                change = session_changes.recv() => match change {
                    Ok(SessionChange::UnknownToken(info)) => {
                        tracing::warn!(
                            soft_logout = info.soft_logout,
                            "the server no longer accepts this session"
                        );
                        if info.soft_logout {
                            break SessionEnd::SoftLogout;
                        }
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
                    Some(MatrixCmd::Previews(wanted)) => {
                        if previews.enabled != wanted {
                            previews.enabled = wanted;
                            previews.listen(client, &rooms);
                            self.emit(MatrixEvent::Rooms(summarize(&rooms, &mut previews).await));
                        }
                    }
                    Some(MatrixCmd::Verification(command)) => verifier.handle(command).await,
                    Some(MatrixCmd::Manage(command)) => manager.handle(command),
                    Some(MatrixCmd::SearchMessages { generation, query }) => {
                        searcher.start(generation, query);
                    }
                    Some(MatrixCmd::CancelSearch) => searcher.cancel(),
                    Some(command) => timelines.handle(command).await,
                },
            }
        };
        verifier.stop().await;
        manager.stop();
        searcher.stop();
        timelines.close();
        // Do not hang on exit if the server is unreachable.
        let _ = tokio::time::timeout(Duration::from_secs(2), sync.stop()).await;
        token_saver.abort();
        // Once more on the way out, in case a refresh happened during the shutdown.
        if matches!(end, SessionEnd::Shutdown)
            && let Err(err) = session::save_tokens(&self.paths, client, passphrase, in_keyring)
        {
            tracing::error!(?err, "could not save the session");
        }
        Ok(end)
    }
}

/// What the room list shows under a room's name: who wrote last, and what.
async fn latest_message(room: &RoomListItem, direct: bool) -> Option<String> {
    use matrix_sdk_ui::timeline::{LatestEventValue, RoomExt};
    let (own, sender, profile, content) = match room.latest_event().await {
        LatestEventValue::Remote {
            is_own,
            sender,
            profile,
            content,
            ..
        } => (is_own, sender, profile, content),
        LatestEventValue::Local {
            sender,
            profile,
            content,
            ..
        } => (true, sender, profile, content),
        LatestEventValue::RemoteInvite { .. } | LatestEventValue::None => return None,
    };
    let text = convert::preview(&content);
    if text.is_empty() {
        return None;
    }
    Some(if own {
        format!("You: {text}")
    } else if direct {
        // In a chat with one person it is clear who wrote.
        text
    } else {
        format!("{}: {text}", convert::display_name(&profile, &sender))
    })
}

/// Whether the room list has had an answer from the server in this state.
fn caught_up_in(state: &matrix_sdk_ui::room_list_service::State) -> bool {
    use matrix_sdk_ui::room_list_service::State;
    matches!(state, State::SettingUp | State::Recovering | State::Running)
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

/// The latest-message lines of the room list. Working one out means loading a profile
/// and reading (maybe decrypting) an event, so each is kept until that room's latest
/// message changes, and nothing is done at all while the list does not show them.
#[derive(Default)]
struct Previews {
    enabled: bool,
    /// By room id: the time of the message the line was made from, and the line.
    lines: std::collections::HashMap<
        String,
        (
            Option<matrix_sdk::ruma::MilliSecondsSinceUnixEpoch>,
            Option<String>,
        ),
    >,
    /// Rooms the SDK has been asked to keep a latest message for.
    listening: std::collections::HashSet<matrix_sdk::ruma::OwnedRoomId>,
    tasks: tokio::task::JoinSet<()>,
}

impl Previews {
    /// Asks the SDK, in the background, to track the latest message of the rooms at
    /// the top of the list that it does not track yet.
    fn listen(&mut self, client: &Client, rooms: &Vector<RoomListItem>) {
        while self.tasks.try_join_next().is_some() {}
        if !self.enabled {
            return;
        }
        let new: Vec<_> = rooms
            .iter()
            .take(LATEST_ROOMS)
            .map(|room| room.room_id().to_owned())
            .filter(|id| self.listening.insert(id.clone()))
            .collect();
        if new.is_empty() {
            return;
        }
        let client = client.clone();
        self.tasks.spawn(async move {
            // The latest messages come from the event cache, which must be running.
            if client.event_cache().subscribe().is_err() {
                return;
            }
            let latest = client.latest_events().await;
            for id in new {
                if let Err(err) = latest.listen_to_room(&id).await {
                    tracing::debug!(%err, "no latest message for a room");
                }
            }
        });
    }

    async fn line(&mut self, room: &RoomListItem, direct: bool) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let stamp = room.latest_event_timestamp();
        let id = room.room_id().as_str();
        if let Some((seen, line)) = self.lines.get(id)
            && *seen == stamp
        {
            return line.clone();
        }
        let line = latest_message(room, direct).await;
        self.lines.insert(id.to_owned(), (stamp, line.clone()));
        line
    }
}

async fn summarize(rooms: &Vector<RoomListItem>, previews: &mut Previews) -> Vec<RoomSummary> {
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
            preview: previews.line(room, direct).await,
            server_notifications: room.unread_notification_counts().notification_count,
            mention: room.num_unread_mentions() > 0,
            unread: notifications > 0 || room.num_unread_messages() > 0,
            marked_unread: room.is_marked_unread(),
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

    /// The room list reports each room's latest message.
    /// `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=... cargo test -- --ignored`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn rooms_come_with_their_latest_message() {
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let username = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let root = std::env::temp_dir().join(format!("matrix-rat-preview-{}", std::process::id()));
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
            password,
        });
        let text = format!("preview {}", std::process::id());
        let mut sent = false;
        let wait = async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    MatrixEvent::Rooms(rooms) => {
                        let joined = rooms.iter().find(|r| r.category != RoomCategory::Invite);
                        if let Some(room) = joined {
                            if !sent {
                                sent = true;
                                // As the app does once it is logged in.
                                handle.send(MatrixCmd::Previews(true));
                                handle.send(MatrixCmd::Send {
                                    room_id: room.id.clone(),
                                    message: crate::format::parse(&text),
                                    reply_to: None,
                                });
                            }
                            if let Some(preview) = &room.preview
                                && preview.contains(&text)
                            {
                                return preview.clone();
                            }
                        }
                    }
                    _ => {}
                }
            }
        };
        let preview = tokio::time::timeout(Duration::from_secs(45), wait)
            .await
            .expect("timed out waiting for the latest message of a room");
        assert_eq!(preview, format!("You: {text}"));
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = std::fs::remove_dir_all(root);
    }

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
                                detached: None,
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

    struct MockMatrixServer {
        base_url: String,
        expire_soft: std::sync::Arc<std::sync::atomic::AtomicBool>,
        expire_hard: std::sync::Arc<std::sync::atomic::AtomicBool>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for MockMatrixServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl MockMatrixServer {
        async fn start(different_reauth_device: bool) -> Self {
            use std::sync::Arc;
            use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let base_url = format!("http://{addr}");
            let expire_soft = Arc::new(AtomicBool::new(false));
            let expire_hard = Arc::new(AtomicBool::new(false));
            let logins = Arc::new(AtomicUsize::new(0));

            let es = expire_soft.clone();
            let eh = expire_hard.clone();
            let task = tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        break;
                    };
                    let es = es.clone();
                    let eh = eh.clone();
                    let logins = logins.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 8192];
                        let n = stream.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let mut lines = req.lines();
                        let first = lines.next().unwrap_or("");
                        let mut parts = first.split_whitespace();
                        let method = parts.next().unwrap_or("");
                        let raw_path = parts.next().unwrap_or("/");
                        let path = raw_path.split('?').next().unwrap_or("/");
                        let auth_tok = lines.clone().find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("authorization").then(|| {
                                v.trim()
                                    .strip_prefix("Bearer ")
                                    .unwrap_or(v.trim())
                                    .to_owned()
                            })
                        });

                        let (status, body) = if path == "/_matrix/client/versions" {
                            (
                                "200 OK",
                                r#"{"versions":["v1.11"],"unstable_features":{"org.matrix.simplified_msc3575":true}}"#.to_owned(),
                            )
                        } else if path == "/_matrix/client/v3/login" && method == "GET" {
                            (
                                "200 OK",
                                r#"{"flows":[{"type":"m.login.password"}]}"#.to_owned(),
                            )
                        } else if path == "/_matrix/client/v3/login" && method == "POST" {
                            let count = logins.fetch_add(1, Ordering::SeqCst) + 1;
                            let dev = if count > 1 && different_reauth_device {
                                "OTHERDEV"
                            } else {
                                "SOFTDEV1"
                            };
                            (
                                "200 OK",
                                format!(
                                    r#"{{"user_id":"@alice:localhost","device_id":"{dev}","access_token":"tok_{count}"}}"#
                                ),
                            )
                        } else if path.contains("sync") {
                            let is_tok1 = auth_tok.as_deref() == Some("tok_1");
                            if is_tok1 && eh.load(Ordering::SeqCst) {
                                (
                                    "401 Unauthorized",
                                    r#"{"errcode":"M_UNKNOWN_TOKEN","error":"hard expired","soft_logout":false}"#.to_owned(),
                                )
                            } else if is_tok1 && es.load(Ordering::SeqCst) {
                                (
                                    "401 Unauthorized",
                                    r#"{"errcode":"M_UNKNOWN_TOKEN","error":"soft expired","soft_logout":true}"#.to_owned(),
                                )
                            } else {
                                ("200 OK", r#"{"pos":"1","lists":{},"rooms":{}}"#.to_owned())
                            }
                        } else if path.starts_with("/_matrix/client/") {
                            ("200 OK", "{}".to_owned())
                        } else {
                            ("404 Not Found", "{}".to_owned())
                        };

                        let resp = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                    });
                }
            });

            Self {
                base_url,
                expire_soft,
                expire_hard,
                task,
            }
        }
    }

    async fn store_ed25519_key(paths: &Paths, homeserver: &str, passphrase: &str) -> String {
        let client = matrix_sdk::Client::builder()
            .homeserver_url(homeserver)
            .sqlite_store(paths.store_dir(), Some(passphrase))
            .build()
            .await
            .unwrap();
        let meta = matrix_sdk::SessionMeta {
            user_id: "@alice:localhost".try_into().unwrap(),
            device_id: "SOFTDEV1".into(),
        };
        let session =
            matrix_sdk::AuthSession::Matrix(matrix_sdk::authentication::matrix::MatrixSession {
                meta,
                tokens: matrix_sdk::authentication::SessionTokens {
                    access_token: "inspect".into(),
                    refresh_token: None,
                },
            });
        client.restore_session(session).await.unwrap();
        client.encryption().ed25519_key().await.unwrap_or_default()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn worker_soft_logout_keeps_device_and_hard_logout_forgets() {
        let server = MockMatrixServer::start(false).await;
        let root = std::env::temp_dir().join(format!("matrix-rat-soft-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "softtest".into(),
        };
        paths.create_dirs().unwrap();

        let (handle, mut events) = spawn_with(paths.clone(), SecretStoreMode::File);
        assert!(matches!(
            events.recv().await.unwrap(),
            MatrixEvent::NeedLogin { .. }
        ));
        handle.send(MatrixCmd::LoginPassword {
            homeserver: server.base_url.clone(),
            username: "alice".into(),
            password: "pw".into(),
        });

        // Wait until first sync completes cleanly.
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(e) | MatrixEvent::Error(e) => panic!("{e}"),
                    MatrixEvent::Sync(SyncStatus::Synced) => return,
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for initial sync");

        let pass_before: String = {
            let v: serde_json::Value =
                serde_json::from_slice(&std::fs::read(paths.session_file()).unwrap()).unwrap();
            v["passphrase"].as_str().unwrap().to_owned()
        };

        // Trigger soft_logout = true from the server.
        server
            .expire_soft
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let reauth_target = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::NeedReauth { target, .. } => return target,
                    MatrixEvent::NeedLogin { reason } => {
                        panic!("soft logout must not drop to NeedLogin: {reason:?}");
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for NeedReauth");

        assert_eq!(reauth_target.user_id, "@alice:localhost");
        assert_eq!(reauth_target.device_id, "SOFTDEV1");
        assert!(paths.store_dir().exists());
        assert!(!paths.data_dir.join("store.previous").exists());

        // Shutdown before inspecting the encrypted store directly so SQLite locks close,
        // then start a fresh worker to also verify soft_logout survives restart.
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}

        let key_before = store_ed25519_key(&paths, &server.base_url, &pass_before).await;
        assert!(!key_before.is_empty());

        let (handle, mut events) = spawn_with(paths.clone(), SecretStoreMode::File);
        match tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap()
        {
            MatrixEvent::NeedReauth { target, .. } => {
                assert_eq!(target.device_id, "SOFTDEV1");
            }
            other => panic!("expected NeedReauth on restart, got {other:?}"),
        }

        handle.send(MatrixCmd::ReauthPassword {
            password: "pw".into(),
        });
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(e) | MatrixEvent::Error(e) => panic!("{e}"),
                    MatrixEvent::ActionFailed(e) => panic!("unexpected ActionFailed: {e}"),
                    MatrixEvent::Sync(SyncStatus::Synced) => return,
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for re-auth sync");
        assert!(!paths.data_dir.join("store.previous").exists());

        // Shutdown and verify the encrypted Olm identity key survived intact.
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let key_after = store_ed25519_key(&paths, &server.base_url, &pass_before).await;
        assert_eq!(key_before, key_after);

        // Hard logout (`soft_logout: false`): restore old expired tok_1 behavior? Simply run a new
        // worker with expire_hard on tok_1 -> verify NeedLogin & deleted session.json.
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn worker_hard_logout_deletes_session_and_device_mismatch_starts_fresh() {
        // Part 1: hard logout (soft_logout = false) forgets session.json and emits NeedLogin.
        let server = MockMatrixServer::start(false).await;
        let root = std::env::temp_dir().join(format!("matrix-rat-hard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "hardtest".into(),
        };
        paths.create_dirs().unwrap();

        let (handle, mut events) = spawn_with(paths.clone(), SecretStoreMode::File);
        handle.send(MatrixCmd::LoginPassword {
            homeserver: server.base_url.clone(),
            username: "alice".into(),
            password: "pw".into(),
        });
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if matches!(
                    events.recv().await,
                    Some(MatrixEvent::Sync(SyncStatus::Synced))
                ) {
                    return;
                }
            }
        })
        .await
        .unwrap();
        assert!(paths.session_file().exists());

        server
            .expire_hard
            .store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::NeedLogin { reason } => {
                        assert!(reason.unwrap_or_default().contains("expired"));
                        return;
                    }
                    MatrixEvent::NeedReauth { .. } => {
                        panic!("hard logout must not emit NeedReauth");
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert!(!paths.session_file().exists());
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = std::fs::remove_dir_all(&root);

        // Part 2: soft logout where server returns a DIFFERENT device_id on re-auth sets old store aside.
        let server2 = MockMatrixServer::start(true).await;
        let root2 =
            std::env::temp_dir().join(format!("matrix-rat-mismatch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root2);
        let paths2 = Paths {
            config_dir: root2.join("config"),
            data_dir: root2.join("data"),
            log_dir: root2.join("log"),
            runtime_dir: root2.join("run"),
            profile: "mismatchtest".into(),
        };
        paths2.create_dirs().unwrap();

        let (handle2, mut events2) = spawn_with(paths2.clone(), SecretStoreMode::File);
        handle2.send(MatrixCmd::LoginPassword {
            homeserver: server2.base_url.clone(),
            username: "alice".into(),
            password: "pw".into(),
        });
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if matches!(
                    events2.recv().await,
                    Some(MatrixEvent::Sync(SyncStatus::Synced))
                ) {
                    return;
                }
            }
        })
        .await
        .unwrap();

        server2
            .expire_soft
            .store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if matches!(events2.recv().await, Some(MatrixEvent::NeedReauth { .. })) {
                    return;
                }
            }
        })
        .await
        .unwrap();

        handle2.send(MatrixCmd::ReauthPassword {
            password: "pw".into(),
        });
        let mut saw_notice = false;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match events2.recv().await.expect("worker stopped") {
                    MatrixEvent::ActionFailed(msg) if msg.contains("new device ID") => {
                        saw_notice = true;
                    }
                    MatrixEvent::Sync(SyncStatus::Synced) if saw_notice => return,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert!(paths2.data_dir.join("store.previous").exists());
        handle2.send(MatrixCmd::Shutdown);
        while events2.recv().await.is_some() {}
        let _ = std::fs::remove_dir_all(&root2);
    }

    /// Verifies against a real local homeserver (`scripts/dev-homeserver.sh` / continuwuity)
    /// that re-authenticating with the saved `device_id` keeps both the device ID and the
    /// encrypted SQLite Olm store's identity key intact. (Note: continuwuity itself does not emit
    /// `soft_logout: true` on `/logout`, so soft logout detection is covered by `worker_soft_logout_*`).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn soft_logout_reauth_preserves_device_id_and_crypto_keys() {
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let reg_token =
            std::env::var("MATRIX_RAT_REG_TOKEN").unwrap_or_else(|_| "devtoken".to_owned());
        let root = std::env::temp_dir().join(format!("matrix-rat-e2e-soft-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "e2e-soft".into(),
        };
        paths.create_dirs().unwrap();

        // Register a disposable user for this test.
        let username = format!("sess_{}", std::process::id());
        let password = "e2e_session_pass".to_owned();
        {
            use matrix_sdk::ruma::api::client::account::register::v3::Request as RegisterRequest;
            use matrix_sdk::ruma::api::client::uiaa::{AuthData, RegistrationToken};

            let reg_client = matrix_sdk::Client::builder()
                .homeserver_url(&homeserver)
                .build()
                .await
                .unwrap();
            let mut request = RegisterRequest::new();
            request.username = Some(username.clone());
            request.password = Some(password.clone());
            request.inhibit_login = true;
            match reg_client.matrix_auth().register(request.clone()).await {
                Ok(_) => {}
                Err(err) => {
                    let session = err
                        .as_uiaa_response()
                        .expect("registration UIAA response")
                        .session
                        .clone();
                    let mut auth = RegistrationToken::new(reg_token);
                    auth.session = session;
                    request.auth = Some(AuthData::RegistrationToken(auth));
                    reg_client.matrix_auth().register(request).await.unwrap();
                }
            }
        }

        let active = session::login_password(
            &paths,
            &homeserver,
            &username,
            &password,
            SecretStoreMode::File,
        )
        .await
        .unwrap();
        let orig_device = active.target.device_id.clone();
        let orig_key = active.client.encryption().ed25519_key().await.unwrap();
        assert!(active.target.user_id.starts_with(&format!("@{username}:")));

        // Server invalidates the old access token via /logout and we record soft-logout state.
        active.client.logout().await.unwrap();
        session::mark_soft_logout(&paths, &active).unwrap();
        drop(active);

        let session::RestoreOutcome::NeedReauth {
            target,
            passphrase,
            in_keyring,
        } = session::restore(&paths, SecretStoreMode::File).await
        else {
            panic!("expected NeedReauth");
        };
        assert_eq!(target.device_id, orig_device);

        let reauth = session::reauth_password(
            &paths,
            &target,
            &password,
            &passphrase,
            in_keyring,
            SecretStoreMode::File,
        )
        .await
        .unwrap();
        assert!(reauth.kept_device);
        assert_eq!(reauth.session.target.device_id, orig_device);
        assert!(!paths.data_dir.join("store.previous").exists());
        let reauth_key = reauth
            .session
            .client
            .encryption()
            .ed25519_key()
            .await
            .unwrap();
        assert_eq!(orig_key, reauth_key);

        reauth.session.client.logout().await.unwrap();
        drop(reauth);
        session::clear(&paths, Some(&target)).await;
        let _ = std::fs::remove_dir_all(&root);
    }
}
