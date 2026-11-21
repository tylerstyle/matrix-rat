//! The Matrix side of the app. Everything that talks to the SDK lives in this module and
//! runs on its own task; the UI only sees [`MatrixCmd`] and [`MatrixEvent`].

mod agent;
mod collect;
mod convert;
mod devices;
mod directory;
mod drafts;
mod emotes;
mod keyring;
mod keywords;
mod left;
mod manage;
mod media;
mod moderation;
mod notify;
mod people;
mod previews;
mod receipts;
mod rooms;
mod safety;
mod saved;
pub(crate) mod search;
mod session;
#[cfg(test)]
pub(crate) use rooms::measure::sample_messages;
#[cfg(test)]
pub(crate) use session::sqlite_store_config;
mod source;
mod spaces;
mod threads;
mod upload;
mod verify;

pub use agent::{AgentCmd, AgentEvent, MAX_WATCHED as AGENT_MAX_WATCHED};
pub use collect::{ARCHIVE_EVENTS, Archived, CollectCmd, CollectEvent, UnreadText};
pub use devices::{
    BackupUpload, DevicesCmd, DevicesEvent, ServerBackup, Session, SessionList, SessionTrust,
    SignOutOutcome,
};
#[cfg(test)]
pub use directory::PreviewJoinRule;
pub use directory::{DirectoryPage, PreviewMembership, PublicRoom, RoomPreview};
#[cfg(test)]
pub use emotes::EmotePack;
pub use emotes::{CustomEmoji, EmoteCatalog, MAX_SHORTCODE_LEN, StickerItem, is_valid_mxc};
pub use keyring::SecretStoreMode;
// For `crate::logsafe`, which turns it into a word for the log.
pub(crate) use keyring::KeyringError;
pub use keywords::{KeywordsCmd, KeywordsEvent, MAX_KEYWORDS, shown_keywords};
#[cfg(test)]
pub use manage::RoomSettingsPower;
pub use manage::{
    Change, ManageCmd, ManageEvent, MemberRole, NotifyLevel, RoomDetails, RoomHistory,
    RoomJoinRule, RoomKnockRequest, RoomMember, RoomPermissions,
};
pub use notify::Notice;
pub use people::{
    OwnProfile, OwnProfileAction, UserProfile, UserSharedRoom, UserVerifyStatus, fallback_localpart,
};
pub use saved::{MAX_FETCH as SAVED_MAX_FETCH, SavedCmd, SavedEvent};
pub use search::SearchHit;
pub use session::{ReauthKind, ReauthTarget};
pub use source::{EventSource, SOURCE_MAX, SourceKind};
pub use spaces::{SpaceChild, SpaceDetails, SpaceEdit, SpaceSummary};
pub use threads::ThreadPage;
pub use upload::OutgoingFile;
// For the app, which does not record yet: so far only tests make one.
pub use upload::VoiceMeta;
pub use verify::{
    Answer, BackupStatus, DeviceTrust, Identity, SasEmoji, Security, SetUpOutcome, VerifyCmd,
    VerifyStage, VerifyUpdate,
};

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::{StreamExt, pin_mut};
use matrix_sdk::encryption::recovery::RecoveryState;
use matrix_sdk::notification_settings::RoomNotificationMode;
use matrix_sdk::{Client, RoomState, SessionChange};
use matrix_sdk_ui::eyeball_im::Vector;
use matrix_sdk_ui::room_list_service::RoomListItem;
use matrix_sdk_ui::room_list_service::filters::new_filter_non_left;
use matrix_sdk_ui::sync_service::{State, SyncService};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::format::Outgoing;
use crate::model::{ItemRef, Message, PinnedMessage, RoomPower};
use crate::paths::Paths;

/// Rooms shown before the user scrolls for more. Paging arrives with the timeline work.
const ROOM_PAGE: usize = 10_000;
/// Rooms (from the top of the list) that get a latest-message preview.
const LATEST_ROOMS: usize = 500;

/// A password, a passphrase or a recovery key on its way to the worker: wiped from
/// memory when dropped. See [`crate::lineedit::LineEdit`] for what that does not
/// cover.
pub type Secret = zeroize::Zeroizing<String>;

pub enum MatrixCmd {
    LoginPassword {
        homeserver: String,
        username: String,
        password: Secret,
    },
    /// Log in through the homeserver's web page (OAuth 2.0).
    LoginBrowser {
        homeserver: String,
    },
    CancelLogin,
    Recover {
        key: Secret,
    },
    /// Start delivering this room's timeline. `generation` comes back with every
    /// update so the UI can drop ones that belong to an earlier open.
    OpenRoom {
        room_id: String,
        generation: u64,
    },
    /// Stop delivering the timeline that was asked for last (see
    /// [`MatrixCmd::OpenRoom`]): the UI shows no room any more.
    CloseRoom,
    /// Open a room (or thread) focused around a specific event id via `/context` when
    /// it is not already in a live timeline. Answered with [`MatrixEvent::Timeline`] for
    /// `generation`, or [`MatrixEvent::JumpFailed`] if unavailable.
    OpenEvent {
        room_id: String,
        event_id: String,
        thread_root: Option<String>,
        generation: u64,
    },
    /// Abort an [`MatrixCmd::OpenEvent`] for `generation`. If it is still pending, the
    /// active timeline is left untouched; if the worker already switched to it, the view
    /// that was streaming before the jump is restored under its original generation.
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
    SendPoll {
        room_id: String,
        question: String,
        answers: Vec<String>,
        disclosed: bool,
        max_selections: u32,
    },
    EditPoll {
        room_id: String,
        item: ItemRef,
        question: String,
        answers: Vec<String>,
        disclosed: bool,
        max_selections: u32,
    },
    VotePoll {
        room_id: String,
        poll_event_id: String,
        answers: Vec<String>,
    },
    EndPoll {
        room_id: String,
        poll_event_id: String,
    },
    /// Lazily loads MSC2545 / Matrix 1.19 image packs for the session and `room_id`.
    EnsureEmotePacks {
        room_id: Option<String>,
    },
    /// Sends an `m.sticker` event to `room_id` (or the active thread when wrapped in `InThread`).
    SendSticker {
        room_id: String,
        name: String,
        url: String,
        dimensions: Option<(u32, u32)>,
        size: Option<u64>,
        mime: Option<String>,
    },
    ToggleReaction {
        room_id: String,
        item: ItemRef,
        key: String,
        /// For a custom emoji (`key` is its `mxc://` address): its shortcode, which goes
        /// along with a new reaction (MSC4027).
        shortcode: Option<String>,
    },
    Redact {
        room_id: String,
        item: ItemRef,
        reason: Option<String>,
    },
    /// Pin or unpin an event in a room (`m.room.pinned_events`).
    Pin {
        room_id: String,
        event_id: String,
        pinned: bool,
    },
    /// Fetch summaries for all currently pinned events in a room.
    FetchPinned {
        room_id: String,
    },
    /// Forward a message from one room into another as a standalone copy.
    Forward {
        from_room_id: String,
        event_id: String,
        to_room_id: String,
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
    /// Download a voice or audio message into a private file in `dir`, to be played,
    /// in place of one that is still on its way; answered with
    /// [`MatrixEvent::AudioFetched`].
    FetchAudio {
        /// The app's number for the sound.
        id: u64,
        file: crate::model::Attachment,
        dir: std::path::PathBuf,
    },
    /// Give up the download of [`MatrixCmd::FetchAudio`]; it is not answered.
    CancelAudio,
    /// Download a voice or audio message into a private file in `dir`, to be
    /// transcribed, under the limits of [`MatrixCmd::FetchAudio`] but beside it (what
    /// plays is left alone, and several may be on their way); answered with
    /// [`MatrixEvent::SpeechFetched`].
    FetchSpeech {
        /// The app's number for the run.
        id: u64,
        file: crate::model::Attachment,
        dir: std::path::PathBuf,
    },
    /// Give up the download of [`MatrixCmd::FetchSpeech`] with this number; it is not
    /// answered.
    CancelSpeech {
        id: u64,
    },
    /// Ask the homeserver what a web address is about; answered with
    /// [`MatrixEvent::LinkPreview`]. Only sent when the user turned link previews on.
    LinkPreview {
        url: String,
    },
    /// Fetch the small picture of a room; answered with [`MatrixEvent::Avatar`].
    FetchAvatar {
        /// Its `mxc://` address.
        url: String,
    },
    /// Fetch the picture of a custom emoji; answered with [`MatrixEvent::EmotePicture`].
    FetchEmotePicture {
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
        /// The app's number for this send; the answer carries it.
        send: u64,
        room_id: String,
        files: Vec<OutgoingFile>,
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
        password: Secret,
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
    /// Adds a user to `m.ignored_user_list`.
    IgnoreUser {
        user_id: String,
    },
    /// Removes a user from `m.ignored_user_list`.
    UnignoreUser {
        user_id: String,
    },
    /// Reports an offending message to the homeserver.
    ReportContent {
        room_id: String,
        event_id: String,
        reason: Option<String>,
    },
    /// Searches the homeserver's user directory by name or MXID.
    SearchUsers {
        generation: u64,
        query: String,
    },
    /// Cancels any running user directory search.
    CancelUserSearch,
    /// The account's sessions, the key backup, and message keys to and from a file.
    Devices(DevicesCmd),
    /// The account's notification keywords: list, add, remove.
    Keywords(KeywordsCmd),
    /// Saved messages: who wrote them and what they say, for their list.
    Saved(SavedCmd),
    /// Try once more to read a message that could not be decrypted: look for its key
    /// in the key backup and decrypt again. Answered with
    /// [`MatrixEvent::DecryptionRetried`].
    RetryDecryption {
        room_id: String,
        event_id: String,
    },
    /// The JSON of a message as the timeline holds it, for the message details popup.
    /// Answered with [`MatrixEvent::Source`].
    FetchSource {
        request: u64,
        room_id: String,
        item: ItemRef,
    },
    /// Fetches profile, verification state, and shared rooms for User Info.
    FetchUserProfile {
        request: u64,
        user_id: String,
        room_id: Option<String>,
    },
    /// Changes the account's global display name.
    SetOwnDisplayName {
        name: Option<String>,
    },
    /// Uploads and sets a new account avatar picture from disk.
    SetOwnAvatar {
        path: std::path::PathBuf,
    },
    /// Clears the account's avatar picture.
    RemoveOwnAvatar,
    /// Searches the public room directory on this or another server.
    SearchDirectory {
        generation: u64,
        query: String,
        server: Option<String>,
    },
    /// Loads the next page for the active room directory search.
    DirectoryNextPage {
        generation: u64,
    },
    /// Aborts any in-flight public room directory search.
    CancelDirectorySearch,
    /// Asks for a page of the room's threads, newest activity first (the homeserver's
    /// thread list). Answered with [`MatrixEvent::Threads`] or
    /// [`MatrixEvent::ThreadsFailed`] for `generation`; a newer request replaces one
    /// that is still running.
    ListThreads {
        room_id: String,
        /// Where the previous page ended; `None` for the first page.
        from: Option<String>,
        /// Only threads the user started or answered in.
        mine: bool,
        generation: u64,
    },
    /// What `matrix-rat serve` asks for: send a text, read a room, watch a room.
    /// Never sent by the app.
    Agent(AgentCmd),
    /// A stretch of a room's messages as text: an archive of the room written to a
    /// file, or what is unread for a summary.
    Collect(CollectCmd),
}

/// Never prints the password or the recovery key, nor a message, a path or an id of a
/// room, an event or a user, so commands are safe to log.
impl std::fmt::Debug for MatrixCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.shown(false))
    }
}

impl MatrixCmd {
    /// Like `Debug`, with the ids of rooms, events and users (and the address and name
    /// a login is for), for tests to tell commands apart.
    #[cfg(test)]
    pub fn describe(&self) -> String {
        self.shown(true)
    }

    /// `ids`: with ids, which only tests may ask for.
    fn shown(&self, ids: bool) -> String {
        let id = |id: &str| ids.then(|| id.to_owned());
        let (name, parts): (&str, Vec<Option<String>>) = match self {
            Self::LoginPassword {
                homeserver,
                username,
                ..
            } => (
                "LoginPassword",
                vec![ids.then(|| format!("{username} at {homeserver}"))],
            ),
            Self::LoginBrowser { homeserver } => ("LoginBrowser", vec![id(homeserver)]),
            Self::CancelLogin => ("CancelLogin", vec![]),
            Self::Recover { .. } => ("Recover", vec![]),
            Self::OpenRoom { room_id, .. } => ("OpenRoom", vec![id(room_id)]),
            Self::CloseRoom => ("CloseRoom", vec![]),
            Self::OpenEvent {
                room_id,
                generation,
                ..
            } => (
                "OpenEvent",
                vec![id(room_id), Some(format!("generation {generation}"))],
            ),
            Self::CancelOpenEvent { generation } => (
                "CancelOpenEvent",
                vec![Some(format!("generation {generation}"))],
            ),
            Self::Paginate { room_id, .. } => ("Paginate", vec![id(room_id)]),
            Self::PaginateForwards { room_id, .. } => ("PaginateForwards", vec![id(room_id)]),
            // Message text is private too.
            Self::Send { room_id, .. } => ("Send", vec![id(room_id)]),
            Self::Edit { room_id, .. } => ("Edit", vec![id(room_id)]),
            Self::SendPoll { room_id, .. } => ("SendPoll", vec![id(room_id)]),
            Self::EditPoll { room_id, .. } => ("EditPoll", vec![id(room_id)]),
            Self::VotePoll { room_id, .. } => ("VotePoll", vec![id(room_id)]),
            Self::EndPoll { room_id, .. } => ("EndPoll", vec![id(room_id)]),
            Self::EnsureEmotePacks { room_id } => (
                "EnsureEmotePacks",
                vec![Some(format!("has_room: {}", room_id.is_some()))],
            ),
            Self::SendSticker { dimensions, .. } => (
                "SendSticker",
                vec![Some(format!("has_dims: {}", dimensions.is_some()))],
            ),
            Self::ToggleReaction { room_id, .. } => ("ToggleReaction", vec![id(room_id)]),
            Self::Redact { room_id, .. } => ("Redact", vec![id(room_id)]),
            Self::Pin {
                room_id, pinned, ..
            } => ("Pin", vec![id(room_id), Some(pinned.to_string())]),
            Self::FetchPinned { room_id } => ("FetchPinned", vec![id(room_id)]),
            Self::Forward {
                from_room_id,
                to_room_id,
                ..
            } => (
                "Forward",
                vec![ids.then(|| format!("{from_room_id} -> {to_room_id}"))],
            ),
            Self::Retry { room_id, .. } => ("Retry", vec![id(room_id)]),
            Self::Typing { room_id, typing } => {
                ("Typing", vec![id(room_id), Some(typing.to_string())])
            }
            Self::MarkRead { room_id, .. } => ("MarkRead", vec![id(room_id)]),
            Self::SetUnread { room_id, unread } => {
                ("SetUnread", vec![id(room_id), Some(unread.to_string())])
            }
            Self::MarkRoomsRead { room_ids, .. } => (
                "MarkRoomsRead",
                vec![Some(format!("{} rooms", room_ids.len()))],
            ),
            Self::Join { room_id } => ("Join", vec![id(room_id)]),
            Self::Leave { room_id } => ("Leave", vec![id(room_id)]),
            Self::Logout => ("Logout", vec![]),
            Self::Previews(wanted) => ("Previews", vec![Some(wanted.to_string())]),
            Self::Shutdown => ("Shutdown", vec![]),
            // The source of an encrypted file carries its key.
            Self::FetchImage { full, .. } => ("FetchImage", vec![Some(format!("full: {full}"))]),
            Self::FetchAvatar { .. } => ("FetchAvatar", vec![]),
            Self::FetchEmotePicture { .. } => ("FetchEmotePicture", vec![]),
            // The address is from a message.
            Self::LinkPreview { .. } => ("LinkPreview", vec![]),
            Self::Verification(command) => ("Verification", vec![Some(format!("{command:?}"))]),
            Self::SaveFile { open, .. } => ("SaveFile", vec![Some(format!("open: {open}"))]),
            // The source of an encrypted file carries its key; the folder is a path.
            Self::FetchAudio { id, .. } => ("FetchAudio", vec![Some(format!("sound {id}"))]),
            Self::CancelAudio => ("CancelAudio", vec![]),
            Self::FetchSpeech { id, .. } => ("FetchSpeech", vec![Some(format!("run {id}"))]),
            Self::CancelSpeech { id } => ("CancelSpeech", vec![Some(format!("run {id}"))]),
            Self::OpenThread { room_id, root, .. } => ("OpenThread", vec![id(room_id), id(root)]),
            Self::InThread { root, command } => {
                ("InThread", vec![id(root), Some(command.shown(ids))])
            }
            // Paths can say a lot about the user; the caption is message text.
            Self::SendFiles { room_id, files, .. } => {
                let voices = files.iter().filter(|file| file.voice.is_some()).count();
                (
                    "SendFiles",
                    vec![
                        id(room_id),
                        Some(format!("{} files", files.len())),
                        (voices > 0).then(|| format!("{voices} voice")),
                    ],
                )
            }
            Self::FetchMembers { room_id } => ("FetchMembers", vec![id(room_id)]),
            Self::Manage(command) => ("Manage", vec![Some(command.shown(ids))]),
            Self::ReauthPassword { .. } => ("ReauthPassword", vec![]),
            Self::ReauthBrowser => ("ReauthBrowser", vec![]),
            // Query text is private too.
            Self::SearchMessages { generation, .. } => (
                "SearchMessages",
                vec![Some(format!("generation {generation}"))],
            ),
            Self::CancelSearch => ("CancelSearch", vec![]),
            // A draft is message text.
            Self::SaveDraft { room_id, text } => (
                "SaveDraft",
                vec![id(room_id), Some(format!("{} bytes", text.len()))],
            ),
            Self::ForgetDrafts { room_id, threads } => (
                "ForgetDrafts",
                vec![id(room_id), Some(format!("{} threads", threads.len()))],
            ),
            Self::IgnoreUser { .. } => ("IgnoreUser", vec![]),
            Self::UnignoreUser { .. } => ("UnignoreUser", vec![]),
            Self::ReportContent { room_id, .. } => ("ReportContent", vec![id(room_id)]),
            Self::SearchUsers { generation, .. } => (
                "SearchUsers",
                vec![Some(format!("generation {generation}"))],
            ),
            Self::CancelUserSearch => ("CancelUserSearch", vec![]),
            Self::Devices(command) => ("Devices", vec![Some(format!("{command:?}"))]),
            // The inner command prints no keyword.
            Self::Keywords(command) => ("Keywords", vec![Some(format!("{command:?}"))]),
            // The inner command prints no room and no message.
            Self::Saved(command) => ("Saved", vec![Some(format!("{command:?}"))]),
            Self::RetryDecryption { room_id, .. } => ("RetryDecryption", vec![id(room_id)]),
            // Which message is private too.
            Self::FetchSource { request, .. } => {
                ("FetchSource", vec![Some(format!("request {request}"))])
            }
            Self::FetchUserProfile {
                request, room_id, ..
            } => (
                "FetchUserProfile",
                vec![
                    Some(format!("request {request}")),
                    Some(format!("has_room: {}", room_id.is_some())),
                ],
            ),
            Self::SetOwnDisplayName { name } => (
                "SetOwnDisplayName",
                vec![Some(format!("clear: {}", name.is_none()))],
            ),
            Self::SetOwnAvatar { .. } => ("SetOwnAvatar", vec![]),
            Self::RemoveOwnAvatar => ("RemoveOwnAvatar", vec![]),
            Self::SearchDirectory {
                generation, server, ..
            } => (
                "SearchDirectory",
                vec![
                    Some(format!("generation {generation}")),
                    Some(format!(
                        "federated: {}",
                        server.as_deref().is_some_and(|s| !s.trim().is_empty())
                    )),
                ],
            ),
            Self::DirectoryNextPage { generation } => (
                "DirectoryNextPage",
                vec![Some(format!("generation {generation}"))],
            ),
            Self::CancelDirectorySearch => ("CancelDirectorySearch", vec![]),
            // Which room is not said; the token is the server's.
            Self::ListThreads {
                from,
                mine,
                generation,
                ..
            } => (
                "ListThreads",
                vec![
                    Some(format!("generation {generation}")),
                    Some(format!("more: {}", from.is_some())),
                    Some(format!("mine: {mine}")),
                ],
            ),
            // The inner command prints no room and no text.
            Self::Agent(command) => ("Agent", vec![Some(command.shown(ids))]),
            Self::Collect(command) => ("Collect", vec![Some(command.shown(ids))]),
        };
        crate::logsafe::shape(name, parts)
    }
}

/// What became of a message after [`MatrixCmd::RetryDecryption`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retried {
    Decrypted,
    /// Still no key.
    StillLocked,
    /// No longer in the timeline as a message of its own: decrypted into an edit or
    /// a reaction (which show on the message they belong to), or no longer loaded.
    /// Which of the two cannot be told.
    Gone,
}

#[derive(Clone, PartialEq, Eq)]
pub enum SyncStatus {
    Connecting,
    Synced,
    Offline,
    Error(String),
    Stopped,
}

/// Hand-written: an error is the SDK's own text.
impl std::fmt::Debug for SyncStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Connecting => "Connecting",
            Self::Synced => "Synced",
            Self::Offline => "Offline",
            Self::Error(_) => "Error",
            Self::Stopped => "Stopped",
        })
    }
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
    Knock,
    Favourite,
    Person,
    Room,
    LowPriority,
}

impl RoomCategory {
    #[cfg(test)]
    pub fn is_pending(self) -> bool {
        matches!(self, Self::Invite | Self::Knock)
    }
}

#[derive(Clone, PartialEq, Eq)]
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
    /// True for direct chats (`m.direct`), even when moved to Favourites or Low priority.
    pub direct: bool,
    /// True when the room's notification mode is `Mute`.
    pub muted: bool,
    /// Encrypted, or not known to be unencrypted yet.
    pub encrypted: bool,
}

/// Hand-written: which room, its name and its latest message stay out of logs.
impl std::fmt::Debug for RoomSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoomSummary")
            .field("category", &self.category)
            .field("notifications", &self.notifications)
            .field("mention", &self.mention)
            .field("unread", &self.unread)
            .field("marked_unread", &self.marked_unread)
            .field("direct", &self.direct)
            .field("muted", &self.muted)
            .field("encrypted", &self.encrypted)
            .finish_non_exhaustive()
    }
}

impl RoomSummary {
    #[cfg(test)]
    pub fn is_pending(&self) -> bool {
        self.category.is_pending()
    }
}

/// A user returned by the homeserver's user directory search.
#[derive(Clone, PartialEq, Eq)]
pub struct UserMatch {
    pub user_id: String,
    pub display_name: Option<String>,
    /// Safe `mxc://` address of the user's avatar, if any.
    pub avatar: Option<String>,
}

impl std::fmt::Debug for UserMatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserMatch")
            .field("has_name", &self.display_name.is_some())
            .field("has_avatar", &self.avatar.is_some())
            .finish()
    }
}

/// Metadata attached to [`MatrixEvent::Timeline`] when it shows a detached
/// slice centered around an older event rather than the live tip.
#[derive(Clone, PartialEq, Eq)]
pub struct DetachedTimeline {
    /// Event id the detached slice was centered on.
    pub target_event: String,
    /// Thread root event id when `/context` resolved to a thread timeline.
    pub thread_root: Option<String>,
    /// Whether forward pagination has already reached the live end.
    pub reached_end: bool,
}

/// Hand-written: which event and thread stay out of logs.
impl std::fmt::Debug for DetachedTimeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DetachedTimeline")
            .field("thread", &self.thread_root.is_some())
            .field("reached_end", &self.reached_end)
            .finish_non_exhaustive()
    }
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
    /// The open room's timeline lost what it had loaded and starts again from its
    /// latest messages: the SDK cleared it (a sync that missed messages keeps only
    /// those after the gap; an ignore-list change empties it). Sent right before the
    /// timeline that follows, which says afresh whether the start is loaded: a
    /// `Timeline` alone only ever adds to that, and the start loaded before is not.
    TimelineReset {
        room_id: String,
        generation: u64,
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
    /// While viewing a detached `/context` slice, how many new messages have
    /// arrived on the live timeline meanwhile.
    DetachedNew {
        generation: u64,
        count: u32,
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
    /// A notifying message or invite arrived for this room, but was suppressed because
    /// its sender or inviter is on the ignored list. Keeps count-based notification
    /// fallbacks from firing a generic "New message" toast for it.
    SuppressedNotice {
        room_id: String,
    },
    /// The user clicked the desktop notification shown for this room. Comes from the
    /// notification service, not from the worker.
    NoticeClicked {
        room_id: String,
        /// For a reminder's notification: the message it was for, and its thread.
        event: Option<(String, Option<String>)>,
    },
    /// Outcome of [`MatrixCmd::SaveFile`]: where the file is now, or what went wrong.
    FileSaved {
        key: String,
        open: bool,
        result: Result<std::path::PathBuf, String>,
    },
    /// Outcome of [`MatrixCmd::FetchAudio`]: the private copy to play (removed when
    /// dropped), or a short reason for the user.
    AudioFetched {
        id: u64,
        result: Result<crate::audio::TempAudio, String>,
    },
    /// Outcome of [`MatrixCmd::FetchSpeech`]: the private copy to transcribe (removed
    /// when dropped), or a short reason for the user.
    SpeechFetched {
        id: u64,
        result: Result<crate::audio::TempAudio, String>,
    },
    /// Outcome of [`MatrixCmd::LinkPreview`]: what the server said about the address
    /// (`None`: nothing, for whatever reason).
    LinkPreview {
        url: String,
        result: Option<crate::previews::Raw>,
    },
    /// Outcome of [`MatrixCmd::FetchAvatar`].
    Avatar {
        url: String,
        result: Result<crate::images::Decoded, String>,
    },
    /// Outcome of [`MatrixCmd::FetchEmotePicture`].
    EmotePicture {
        url: String,
        result: Result<crate::images::Decoded, String>,
    },
    /// The largest file the server accepts, in bytes.
    UploadLimit(u64),
    /// The first file of a [`MatrixCmd::SendFiles`] could not be sent, so none was:
    /// the caption and the reply belong to that one. Carries the reason.
    FilesNotSent {
        send: u64,
        reason: String,
    },
    /// The files of a [`MatrixCmd::SendFiles`] were read and handed to the send queue,
    /// which keeps its own copy (a later file that could not be was reported on its
    /// own): none of them is needed on disk any more. Every `SendFiles` is answered by
    /// this or by [`MatrixEvent::FilesNotSent`].
    FilesSent(u64),
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
    /// Current sorted list of ignored Matrix user IDs from `m.ignored_user_list`.
    IgnoredUsers(Vec<String>),
    /// Outcome of [`MatrixCmd::IgnoreUser`] or [`MatrixCmd::UnignoreUser`].
    IgnoreResult {
        user_id: String,
        ignored: bool,
        result: Result<(), String>,
    },
    /// Outcome of [`MatrixCmd::ReportContent`].
    ReportResult(Result<(), String>),
    /// Outcome of [`MatrixCmd::SearchUsers`].
    UserSearchResults {
        generation: u64,
        results: Vec<UserMatch>,
    },
    /// Moderation permissions for a room (sent on open and whenever power levels change).
    RoomPower {
        room_id: String,
        power: RoomPower,
    },
    /// Current set of pinned event IDs (`m.room.pinned_events`) in a room.
    PinnedEvents {
        room_id: String,
        ids: Vec<String>,
    },
    /// Loaded summaries for [`MatrixCmd::FetchPinned`].
    PinnedList {
        room_id: String,
        items: Vec<PinnedMessage>,
        error: Option<String>,
    },
    /// Outcome of [`MatrixCmd::Forward`].
    ForwardResult {
        to_room_id: String,
        to_room_name: String,
        result: Result<(), String>,
    },
    /// An answer to a [`MatrixCmd::Devices`], or news for the sessions popup.
    Devices(DevicesEvent),
    /// An answer to a [`MatrixCmd::Keywords`], or the list for the keywords popup.
    Keywords(KeywordsEvent),
    /// An answer to a [`MatrixCmd::Saved`].
    Saved(SavedEvent),
    /// Outcome of [`MatrixCmd::RetryDecryption`]: whether the message can be read
    /// now, or what went wrong.
    DecryptionRetried {
        room_id: String,
        event_id: String,
        result: Result<Retried, String>,
    },
    /// Synced profile of the logged-in account.
    OwnProfile(OwnProfile),
    /// Outcome of an own-profile mutation command.
    OwnProfileResult {
        what: OwnProfileAction,
        result: Result<(), String>,
    },
    /// Outcome of [`MatrixCmd::FetchSource`].
    Source {
        request: u64,
        result: Result<Box<EventSource>, String>,
    },
    /// Outcome of [`MatrixCmd::FetchUserProfile`].
    UserProfile {
        request: u64,
        result: Result<Box<UserProfile>, String>,
    },
    /// Outcome of [`MatrixCmd::SearchDirectory`] or [`MatrixCmd::DirectoryNextPage`].
    DirectoryResults(DirectoryPage),
    /// Error from [`MatrixCmd::SearchDirectory`] or [`MatrixCmd::DirectoryNextPage`].
    DirectoryError {
        generation: u64,
        message: String,
    },
    /// Session-cached MSC2545 / Matrix 1.19 custom emoji and sticker packs: the
    /// account's, and those of `room_id` (the room they were loaded for, which the UI
    /// may have left by the time they arrive).
    EmotePacks {
        room_id: Option<String>,
        catalog: EmoteCatalog,
    },
    /// A page of [`MatrixCmd::ListThreads`].
    Threads(ThreadPage),
    /// [`MatrixCmd::ListThreads`] got no answer: the server has no thread list, or
    /// could not give it.
    ThreadsFailed {
        room_id: String,
        generation: u64,
    },
    /// An answer to a [`MatrixCmd::Agent`], or news of a room that is watched.
    Agent(AgentEvent),
    /// An answer to a [`MatrixCmd::Collect`], or how far it is.
    Collect(CollectEvent),
}

/// Hand-written so that no message text, no error text (it can name rooms, people
/// and servers) and no id of a room, an event or a user ends up in a log.
impl std::fmt::Debug for MatrixEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.shown(false))
    }
}

impl MatrixEvent {
    /// Like `Debug`, with the ids of rooms, events and users and the texts of errors,
    /// for tests to tell events apart.
    #[cfg(test)]
    pub fn describe(&self) -> String {
        self.shown(true)
    }

    /// `full`: with ids and error texts, which only tests may ask for.
    fn shown(&self, full: bool) -> String {
        let id = |id: &str| full.then(|| id.to_owned());
        let text = |text: &str| full.then(|| text.to_owned());
        let ok = |ok: bool| Some(format!("ok: {ok}"));
        let generation = |generation: &u64| Some(format!("generation {generation}"));
        let (name, parts): (&str, Vec<Option<String>>) = match self {
            Self::NeedLogin { reason } => (
                "NeedLogin",
                vec![Some(if full {
                    format!("{reason:?}")
                } else {
                    format!("reason: {}", reason.is_some())
                })],
            ),
            Self::LoginFailed(error) => ("LoginFailed", vec![text(error)]),
            Self::BrowserLoginUrl(_) => ("BrowserLoginUrl", vec![]),
            Self::LoggedIn { user_id } => ("LoggedIn", vec![id(user_id)]),
            Self::Sync(status) => ("Sync", vec![Some(format!("{status:?}"))]),
            Self::CaughtUp => ("CaughtUp", vec![]),
            Self::Rooms(rooms) => ("Rooms", vec![Some(rooms.len().to_string())]),
            Self::Recovery(status) => ("Recovery", vec![Some(format!("{status:?}"))]),
            Self::RecoverResult(result) => ("RecoverResult", vec![ok(result.is_ok())]),
            Self::Timeline {
                room_id,
                generation: g,
                messages,
                reached_start,
                detached,
            } => (
                "Timeline",
                vec![
                    id(room_id),
                    generation(g),
                    Some(format!("{} messages", messages.len())),
                    Some(format!("start: {reached_start}")),
                    Some(format!("detached: {}", detached.is_some())),
                ],
            ),
            Self::TimelineReset {
                room_id,
                generation: g,
            } => ("TimelineReset", vec![id(room_id), generation(g)]),
            Self::Paginated {
                generation: g,
                reached_start,
            } => (
                "Paginated",
                vec![generation(g), Some(format!("start: {reached_start}"))],
            ),
            Self::PaginatedForwards {
                generation: g,
                reached_end,
            } => (
                "PaginatedForwards",
                vec![generation(g), Some(format!("end: {reached_end}"))],
            ),
            Self::DetachedNew {
                generation: g,
                count,
            } => (
                "DetachedNew",
                vec![generation(g), Some(format!("count: {count}"))],
            ),
            Self::JumpFailed {
                generation: g,
                reason,
            } => ("JumpFailed", vec![generation(g), text(reason)]),
            Self::Typing {
                generation: g,
                names,
            } => ("Typing", vec![generation(g), Some(names.len().to_string())]),
            Self::ActionFailed(error) => ("ActionFailed", vec![text(error)]),
            Self::Error(error) => ("Error", vec![text(error)]),
            Self::Image {
                full: whole,
                result,
                ..
            } => (
                "Image",
                vec![Some(format!("full: {whole}")), ok(result.is_ok())],
            ),
            Self::Avatar { result, .. } => ("Avatar", vec![ok(result.is_ok())]),
            Self::EmotePicture { result, .. } => ("EmotePicture", vec![ok(result.is_ok())]),
            Self::LinkPreview { result, .. } => (
                "LinkPreview",
                vec![Some(format!("found: {}", result.is_some()))],
            ),
            Self::Security(security) => ("Security", vec![Some(format!("{security:?}"))]),
            Self::Verification(update) => (
                "Verification",
                vec![
                    Some(update.flow.to_string()),
                    Some(format!("{:?}", update.stage)),
                ],
            ),
            Self::CrossSigningSetUp(result) => {
                ("CrossSigningSetUp", vec![Some(format!("{result:?}"))])
            }
            Self::Spaces(spaces) => ("Spaces", vec![Some(spaces.len().to_string())]),
            Self::Notice(notice) => return format!("{notice:?}"),
            Self::SuppressedNotice { room_id } => ("SuppressedNotice", vec![id(room_id)]),
            Self::NoticeClicked { room_id, .. } => ("NoticeClicked", vec![id(room_id)]),
            Self::FileSaved { result, .. } => ("FileSaved", vec![ok(result.is_ok())]),
            Self::AudioFetched { id, result } => (
                "AudioFetched",
                vec![Some(format!("sound {id}")), ok(result.is_ok())],
            ),
            Self::SpeechFetched { id, result } => (
                "SpeechFetched",
                vec![Some(format!("run {id}")), ok(result.is_ok())],
            ),
            Self::UploadLimit(bytes) => ("UploadLimit", vec![Some(bytes.to_string())]),
            // The reason names a file.
            Self::FilesNotSent { .. } => ("FilesNotSent", vec![]),
            Self::FilesSent(_) => ("FilesSent", vec![]),
            // Member display names and user ids are omitted from the log.
            Self::RoomMembers {
                room_id,
                members,
                can_mention_room,
            } => (
                "RoomMembers",
                vec![
                    id(room_id),
                    Some(format!("{} members", members.len())),
                    Some(format!("room_mention: {can_mention_room}")),
                ],
            ),
            Self::Manage(event) => ("Manage", vec![Some(format!("{event:?}"))]),
            Self::NeedReauth { target, .. } => (
                "NeedReauth",
                vec![full.then(|| format!("{} dev={}", target.user_id, target.device_id))],
            ),
            Self::SearchResults {
                generation: g,
                hits,
                done,
                note,
            } => (
                "SearchResults",
                vec![
                    generation(g),
                    Some(format!("{} hits", hits.len())),
                    Some(format!("done: {done}")),
                    Some(format!("note: {}", note.is_some())),
                ],
            ),
            // A draft is message text.
            Self::Draft {
                room_id,
                thread,
                text,
            } => (
                "Draft",
                vec![
                    id(room_id),
                    match thread {
                        Some(root) => id(root).or_else(|| Some("in a thread".to_owned())),
                        None => None,
                    },
                    Some(format!("{} bytes", text.len())),
                ],
            ),
            Self::IgnoredUsers(users) => ("IgnoredUsers", vec![Some(users.len().to_string())]),
            Self::IgnoreResult {
                ignored, result, ..
            } => (
                "IgnoreResult",
                vec![Some(format!("ignored: {ignored}")), ok(result.is_ok())],
            ),
            Self::ReportResult(result) => ("ReportResult", vec![ok(result.is_ok())]),
            Self::UserSearchResults {
                generation: g,
                results,
            } => (
                "UserSearchResults",
                vec![generation(g), Some(format!("{} results", results.len()))],
            ),
            Self::RoomPower { room_id, power } => {
                ("RoomPower", vec![id(room_id), Some(format!("{power:?}"))])
            }
            Self::PinnedEvents { room_id, ids } => (
                "PinnedEvents",
                vec![id(room_id), Some(format!("{} pinned", ids.len()))],
            ),
            Self::PinnedList {
                room_id,
                items,
                error,
            } => (
                "PinnedList",
                vec![
                    id(room_id),
                    Some(format!("{} items", items.len())),
                    Some(format!("err: {}", error.is_some())),
                ],
            ),
            Self::ForwardResult {
                to_room_id, result, ..
            } => ("ForwardResult", vec![id(to_room_id), ok(result.is_ok())]),
            Self::Devices(event) => ("Devices", vec![Some(format!("{event:?}"))]),
            Self::Keywords(event) => ("Keywords", vec![Some(format!("{event:?}"))]),
            Self::Saved(event) => ("Saved", vec![Some(format!("{event:?}"))]),
            Self::DecryptionRetried {
                room_id, result, ..
            } => (
                "DecryptionRetried",
                vec![id(room_id), Some(format!("{:?}", result.as_ref().ok()))],
            ),
            Self::OwnProfile(profile) => ("OwnProfile", vec![Some(format!("{profile:?}"))]),
            Self::OwnProfileResult { what, result } => (
                "OwnProfileResult",
                vec![Some(format!("{what:?}")), ok(result.is_ok())],
            ),
            Self::Source { request, result } => (
                "Source",
                vec![Some(format!("request {request}")), ok(result.is_ok())],
            ),
            Self::UserProfile { request, result } => (
                "UserProfile",
                vec![Some(format!("request {request}")), ok(result.is_ok())],
            ),
            Self::DirectoryResults(page) => ("DirectoryResults", vec![Some(format!("{page:?}"))]),
            Self::DirectoryError { generation: g, .. } => ("DirectoryError", vec![generation(g)]),
            Self::EmotePacks { room_id, catalog } => (
                "EmotePacks",
                vec![
                    match room_id {
                        Some(room_id) => id(room_id),
                        None => Some("no room".to_owned()),
                    },
                    Some(format!("{catalog:?}")),
                ],
            ),
            Self::Threads(page) => ("Threads", vec![Some(format!("{page:?}"))]),
            Self::ThreadsFailed { generation: g, .. } => ("ThreadsFailed", vec![generation(g)]),
            // The inner event prints no room, no message and no reason.
            Self::Agent(event) => ("Agent", vec![Some(event.shown(full))]),
            Self::Collect(event) => ("Collect", vec![Some(event.shown(full))]),
        };
        crate::logsafe::shape(name, parts)
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

    /// Waits until the worker has ended: it takes no more commands, and has let go
    /// of the store. For a program that ends when the worker does.
    pub async fn stopped(&self) {
        self.commands.closed().await;
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
        // Rooms left in this session, which a late sync answer must not bring back.
        let left = left::LeftRooms::default();
        let outcome = self.run_session(&active, post_notice, &left).await;
        match outcome {
            Ok(SessionEnd::Shutdown) => {
                let joined = search::joined_room_ids(&active.client, &left);
                drop(active);
                search::prune_search_dir_off_loop(&self.paths.search_dir(), joined).await;
                return false;
            }
            Ok(SessionEnd::SoftLogout) => {
                let target = active.target.clone();
                let passphrase = active.passphrase.clone();
                let in_keyring = active.in_keyring;
                if let Err(err) = session::mark_soft_logout(&self.paths, &active) {
                    tracing::warn!(why = %crate::logsafe::kind_any(&err), "could not record soft logout state");
                }
                let joined = search::joined_room_ids(&active.client, &left);
                drop(active);
                search::prune_search_dir_off_loop(&self.paths.search_dir(), joined).await;
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
                        let why = match &outcome {
                            Ok(Err(err)) => crate::logsafe::kind(err),
                            _ => crate::logsafe::TIMED_OUT,
                        };
                        tracing::warn!(%why, "could not log out on the server");
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
                tracing::error!(why = %crate::logsafe::kind_any(&err), "session failed");
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
                MatrixCmd::CancelLogin
                | MatrixCmd::Recover { .. }
                | MatrixCmd::OpenRoom { .. }
                | MatrixCmd::CloseRoom
                | MatrixCmd::OpenEvent { .. }
                | MatrixCmd::CancelOpenEvent { .. }
                | MatrixCmd::Paginate { .. }
                | MatrixCmd::PaginateForwards { .. }
                | MatrixCmd::Send { .. }
                | MatrixCmd::Edit { .. }
                | MatrixCmd::SendPoll { .. }
                | MatrixCmd::EditPoll { .. }
                | MatrixCmd::VotePoll { .. }
                | MatrixCmd::EndPoll { .. }
                | MatrixCmd::EnsureEmotePacks { .. }
                | MatrixCmd::SendSticker { .. }
                | MatrixCmd::ToggleReaction { .. }
                | MatrixCmd::Redact { .. }
                | MatrixCmd::Pin { .. }
                | MatrixCmd::FetchPinned { .. }
                | MatrixCmd::Forward { .. }
                | MatrixCmd::Retry { .. }
                | MatrixCmd::Typing { .. }
                | MatrixCmd::MarkRead { .. }
                | MatrixCmd::SetUnread { .. }
                | MatrixCmd::MarkRoomsRead { .. }
                | MatrixCmd::Join { .. }
                | MatrixCmd::Leave { .. }
                | MatrixCmd::Logout
                | MatrixCmd::Previews(_)
                | MatrixCmd::FetchImage { .. }
                | MatrixCmd::Verification(_)
                | MatrixCmd::SaveFile { .. }
                | MatrixCmd::FetchAudio { .. }
                | MatrixCmd::CancelAudio
                | MatrixCmd::FetchSpeech { .. }
                | MatrixCmd::CancelSpeech { .. }
                | MatrixCmd::LinkPreview { .. }
                | MatrixCmd::FetchAvatar { .. }
                | MatrixCmd::FetchEmotePicture { .. }
                | MatrixCmd::OpenThread { .. }
                | MatrixCmd::InThread { .. }
                | MatrixCmd::SendFiles { .. }
                | MatrixCmd::FetchMembers { .. }
                | MatrixCmd::Manage(_)
                | MatrixCmd::SearchMessages { .. }
                | MatrixCmd::CancelSearch
                | MatrixCmd::SaveDraft { .. }
                | MatrixCmd::ForgetDrafts { .. }
                | MatrixCmd::IgnoreUser { .. }
                | MatrixCmd::UnignoreUser { .. }
                | MatrixCmd::ReportContent { .. }
                | MatrixCmd::SearchUsers { .. }
                | MatrixCmd::CancelUserSearch
                | MatrixCmd::Devices(_)
                | MatrixCmd::Keywords(_)
                | MatrixCmd::Saved(_)
                | MatrixCmd::RetryDecryption { .. }
                | MatrixCmd::FetchSource { .. }
                | MatrixCmd::FetchUserProfile { .. }
                | MatrixCmd::SetOwnDisplayName { .. }
                | MatrixCmd::SetOwnAvatar { .. }
                | MatrixCmd::RemoveOwnAvatar
                | MatrixCmd::SearchDirectory { .. }
                | MatrixCmd::DirectoryNextPage { .. }
                | MatrixCmd::CancelDirectorySearch
                | MatrixCmd::ListThreads { .. }
                | MatrixCmd::Agent(_)
                | MatrixCmd::Collect(_) => continue,
            };
            let login = async {
                #[expect(
                    clippy::wildcard_enum_match_arm,
                    reason = "filtered to login and reauth commands immediately above"
                )]
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
                    tracing::warn!(why = %crate::logsafe::kind_any(&err), "login failed");
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
        left: &left::LeftRooms,
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
        let ignore_tracker =
            safety::IgnoreTracker::start(client.clone(), self.events.clone()).await;
        let mut ignored = ignore_tracker.subscribe();
        let _notices =
            notify::watch(client, self.events.clone(), ignored.clone(), left.clone()).await;
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
                                tracing::error!(
                                    why = %crate::logsafe::kind_any(&err),
                                    "could not save refreshed tokens"
                                );
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
        let mut searcher = search::Searcher::new(
            client.clone(),
            self.events.clone(),
            ignored.clone(),
            left.clone(),
        );
        let mut user_searcher =
            safety::UserSearcher::new(client.clone(), self.events.clone(), ignored.clone());
        let mut dir_searcher =
            directory::DirectorySearcher::new(client.clone(), self.events.clone(), left.clone());
        // The rooms the store has as joined before the sync brings any, to be checked
        // against the server's list once the first sync is in (a room left in an earlier
        // session can look joined, see `left`).
        let mut start_check = left::StartCheck::new(client);
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
        let caught_up_at_start = caught_up_in(&list_state.get());
        if caught_up_at_start {
            caught_up = true;
            start_check.caught_up(client, left);
            self.emit(MatrixEvent::CaughtUp);
        }
        let recovery = client.encryption().recovery();
        let recovery_states = recovery.state_stream();
        pin_mut!(recovery_states);
        self.emit(MatrixEvent::Recovery(recovery_status(recovery.state())));

        let mut rooms: Vector<RoomListItem> = Vector::new();
        let own_refresh = std::sync::Arc::new(tokio::sync::watch::Sender::new(0u64));
        let own_room_names = people::OwnRoomNames::default();
        let mut timelines = rooms::Timelines::new(
            client.clone(),
            self.events.clone(),
            ignored.clone(),
            own_refresh.subscribe(),
            own_room_names.clone(),
            left.clone(),
        );
        let mut people_mgr = people::People::start(
            client.clone(),
            self.events.clone(),
            own_refresh.clone(),
            own_room_names.clone(),
            left.clone(),
        );
        let mut own_members = notify::OwnMembers::default();
        let mut previews = Previews::default();
        let mut verifier = verify::Verifier::start(client, self.events.clone());
        if caught_up_at_start {
            verifier.caught_up();
        }
        // Kept alive for the session: it follows the space hierarchy by itself.
        let space_service = matrix_sdk_ui::spaces::SpaceService::new(client.clone()).await;
        let (mut space_filters, space_changes) = space_service.subscribe_to_space_filters().await;
        pin_mut!(space_changes);
        self.emit(MatrixEvent::Spaces(
            spaces::summarize(client, &space_filters, left).await,
        ));
        // New rights in a space, or a change of its rooms that leaves the SDK's
        // filters as they were, is not reported by the SDK: the handlers below ask
        // for the spaces to be told again.
        let spaces_poke = std::sync::Arc::new(tokio::sync::Notify::new());

        // One instance for the whole session: it hears of changes to the push rules
        // only while it lives (dropping it removes its listener, and `notify_changes`
        // would close at once).
        let notification_settings = client.notification_settings().await;
        let mut notify_changes = notification_settings.subscribe_to_changes();
        let mut notify_open = true;
        let mut keywords = keywords::Keywords::new(
            client.clone(),
            notification_settings.clone(),
            self.events.clone(),
        );
        // Room management asks for the room list to be made again (a room muted here).
        let relist = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut manager = manage::Manager::new(
            client.clone(),
            self.events.clone(),
            own_room_names.clone(),
            people_mgr.sync_poke(),
            own_refresh.clone(),
            left.clone(),
            notification_settings.clone(),
            relist.clone(),
        );
        let mut devices = devices::Devices::new(client.clone(), self.events.clone());
        let mut agent = agent::Agent::new(
            client.clone(),
            self.events.clone(),
            ignored.clone(),
            left.clone(),
        );
        let mut collector = collect::Collector::new(
            client.clone(),
            self.events.clone(),
            ignored.clone(),
            left.clone(),
        );
        let mut emote_mgr =
            emotes::EmoteManager::new(client.clone(), self.events.clone(), left.clone());
        let emote_handlers = emote_mgr.register_handlers(client);
        let power_events = self.events.clone();
        let manage_refresh = manager.refresh_signal();
        let power_spaces_poke = spaces_poke.clone();
        let power_handler = client.add_event_handler(
            move |_ev: matrix_sdk::ruma::events::room::power_levels::SyncRoomPowerLevelsEvent,
                  room: matrix_sdk::Room| {
                let events = power_events.clone();
                let refresh = manage_refresh.clone();
                let spaces_poke = power_spaces_poke.clone();
                async move {
                    manage::emit_room_power(&events, &room).await;
                    manage::bump_refresh(&refresh, room.room_id().as_str());
                    if room.is_space() {
                        spaces_poke.notify_one();
                    }
                }
            },
        );
        let child_refresh = manager.refresh_signal();
        let child_spaces_poke = spaces_poke.clone();
        // Raw: a room taken out of a space leaves an event with empty content, which
        // the typed event cannot read, and the handler would not be called for it.
        let space_child_handler = client.add_event_handler(
            move |_ev: matrix_sdk::ruma::serde::Raw<
                matrix_sdk::ruma::events::space::child::SyncSpaceChildEvent,
            >,
                  room: matrix_sdk::Room| {
                let refresh = child_refresh.clone();
                let spaces_poke = child_spaces_poke.clone();
                async move {
                    manage::bump_refresh(&refresh, room.room_id().as_str());
                    spaces_poke.notify_one();
                }
            },
        );
        let member_refresh = manager.refresh_signal();
        let own_user_for_member = client.user_id().map(ToOwned::to_owned);
        let people_poke = people_mgr.sync_poke();
        let own_timeline_refresh = own_refresh.clone();
        let member_own_room_names = own_room_names;
        let member_handler = client.add_event_handler(
            move |ev: matrix_sdk::ruma::events::room::member::SyncRoomMemberEvent,
                  room: matrix_sdk::Room| {
                let refresh = member_refresh.clone();
                let own_id = own_user_for_member.clone();
                let people_poke = people_poke.clone();
                let own_timeline_refresh = own_timeline_refresh.clone();
                let own_room_names = member_own_room_names.clone();
                async move {
                    manage::bump_refresh(&refresh, room.room_id().as_str());
                    // Only a change of the user's own name or picture in the room is
                    // worth showing their messages anew and looking at the profile.
                    if own_id
                        .as_ref()
                        .is_some_and(|own| ev.state_key() == own.as_str())
                        && people::record_own_member_event(
                            &own_room_names,
                            room.room_id().as_str(),
                            &ev,
                        )
                    {
                        own_timeline_refresh.send_modify(|n| *n = n.wrapping_add(1));
                        people_poke.notify_one();
                    }
                }
            },
        );
        let room_state_handlers = [
            watch_room_state::<matrix_sdk::ruma::events::room::join_rules::SyncRoomJoinRulesEvent>(
                client,
                &manager.refresh_signal(),
            ),
            watch_room_state::<
                matrix_sdk::ruma::events::room::history_visibility::SyncRoomHistoryVisibilityEvent,
            >(client, &manager.refresh_signal()),
            watch_room_state::<matrix_sdk::ruma::events::room::encryption::SyncRoomEncryptionEvent>(
                client,
                &manager.refresh_signal(),
            ),
            watch_room_state::<matrix_sdk::ruma::events::room::avatar::SyncRoomAvatarEvent>(
                client,
                &manager.refresh_signal(),
            ),
            watch_room_state::<
                matrix_sdk::ruma::events::room::canonical_alias::SyncRoomCanonicalAliasEvent,
            >(client, &manager.refresh_signal()),
            watch_room_state::<matrix_sdk::ruma::events::room::name::SyncRoomNameEvent>(
                client,
                &manager.refresh_signal(),
            ),
            watch_room_state::<matrix_sdk::ruma::events::room::topic::SyncRoomTopicEvent>(
                client,
                &manager.refresh_signal(),
            ),
        ];
        let end = loop {
            // A room's timeline left unused for long goes (see `rooms::IDLE_TIMELINE`).
            let timelines_idle = timelines.idle_due();
            tokio::select! {
                () = timelines_idle => timelines.drop_idle(std::time::Instant::now()),
                Some(diffs) = space_changes.next() => {
                    for diff in diffs {
                        diff.apply(&mut space_filters);
                    }
                    self.emit(MatrixEvent::Spaces(
                        spaces::summarize(client, &space_filters, left).await,
                    ));
                }
                () = spaces_poke.notified() => {
                    self.emit(MatrixEvent::Spaces(
                        spaces::summarize(client, &space_filters, left).await,
                    ));
                }
                change = notify_changes.recv(), if notify_open => {
                    match change {
                        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            self.emit(MatrixEvent::Rooms(
                                summarize(&rooms, &mut previews, &ignored, left, &notification_settings).await,
                            ));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            notify_open = false;
                        }
                    }
                }
                // A room kept out of the list is one the account is in after all, an
                // invitation or a knock for one came (making the list showed it), or it
                // is time to ask the server about one. (A space can be such a room, and
                // the spaces leave out the rooms that were left: they are told again too.)
                () = left.due() => {
                    // One that is back has its latest message followed from now on.
                    previews.listen(client, &rooms, left);
                    self.emit(MatrixEvent::Rooms(
                        summarize(&rooms, &mut previews, &ignored, left, &notification_settings).await,
                    ));
                    self.emit(MatrixEvent::Spaces(
                        spaces::summarize(client, &space_filters, left).await,
                    ));
                }
                () = relist.notified() => {
                    self.emit(MatrixEvent::Rooms(
                        summarize(&rooms, &mut previews, &ignored, left, &notification_settings).await,
                    ));
                }
                incoming = verifier.next_incoming() => verifier.on_incoming(incoming).await,
                // Keys came in from a file: what is on screen may be readable now.
                () = devices.next_imported() => timelines.retry_decryption().await,
                Some(diffs) = entries.next() => {
                    for diff in diffs {
                        diff.apply(&mut rooms);
                    }
                    own_members.ensure(&rooms, left).await;
                    // The SDK works out a room's latest message only for rooms it was
                    // asked to; ask once for each (it needs the event cache running).
                    previews.listen(client, &rooms, left);
                    _notices.arm();
                    self.emit(MatrixEvent::Rooms(
                        summarize(&rooms, &mut previews, &ignored, left, &notification_settings).await,
                    ));
                }
                // The timelines stay: the SDK clears every room's event cache in place
                // when the list changes (each timeline, shown or kept, starts over from
                // the same empty cache a new one would), and their forwarders, and any
                // opened later, leave out who is ignored now.
                Ok(()) = ignored.changed() => {
                    previews.lines.clear();
                    self.emit(MatrixEvent::Rooms(
                        summarize(&rooms, &mut previews, &ignored, left, &notification_settings).await,
                    ));
                }
                () = own_members.finished() => own_members.ensure(&rooms, left).await,
                Some(opened) = timelines.next_opened() => timelines.on_opened(opened).await,
                Some(state) = sync_state.next() => {
                    self.emit(MatrixEvent::Sync(sync_status(&state)));
                }
                Some(state) = list_state.next(), if !caught_up => {
                    if caught_up_in(&state) {
                        caught_up = true;
                        start_check.caught_up(client, left);
                        verifier.caught_up();
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
                            previews.listen(client, &rooms, left);
                            self.emit(MatrixEvent::Rooms(
                                summarize(&rooms, &mut previews, &ignored, left, &notification_settings).await,
                            ));
                        }
                    }
                    Some(MatrixCmd::Verification(command)) => verifier.handle(command).await,
                    Some(MatrixCmd::Manage(command)) => manager.handle(command),
                    Some(MatrixCmd::Devices(command)) => devices.handle(command),
                    Some(MatrixCmd::Keywords(command)) => keywords.handle(command),
                    Some(MatrixCmd::Agent(command)) => agent.handle(command),
                    Some(MatrixCmd::Collect(command)) => collector.handle(command),
                    Some(MatrixCmd::SearchMessages { generation, query }) => {
                        searcher.start(generation, query);
                    }
                    Some(MatrixCmd::CancelSearch) => searcher.cancel(),
                    Some(MatrixCmd::IgnoreUser { user_id }) => ignore_tracker.set(user_id, true),
                    Some(MatrixCmd::UnignoreUser { user_id }) => {
                        ignore_tracker.set(user_id, false);
                    }
                    Some(MatrixCmd::ReportContent {
                        room_id,
                        event_id,
                        reason,
                    }) => {
                        let client = client.clone();
                        let events = self.events.clone();
                        tokio::spawn(async move {
                            safety::report_content(client, events, room_id, event_id, reason)
                                .await;
                        });
                    }
                    Some(MatrixCmd::SearchUsers { generation, query }) => {
                        user_searcher.start(generation, query);
                    }
                    Some(MatrixCmd::CancelUserSearch) => user_searcher.cancel(),
                    Some(MatrixCmd::FetchUserProfile {
                        request,
                        user_id,
                        room_id,
                    }) => people_mgr.fetch_user_profile(request, user_id, room_id),
                    Some(MatrixCmd::SetOwnDisplayName { name }) => {
                        people_mgr.set_display_name(name);
                    }
                    Some(MatrixCmd::SetOwnAvatar { path }) => {
                        people_mgr.set_avatar(path);
                    }
                    Some(MatrixCmd::RemoveOwnAvatar) => {
                        people_mgr.remove_avatar();
                    }
                    Some(MatrixCmd::SearchDirectory {
                        generation,
                        query,
                        server,
                    }) => dir_searcher.start(generation, query, server),
                    Some(MatrixCmd::DirectoryNextPage { generation }) => {
                        dir_searcher.next_page(generation);
                    }
                    Some(MatrixCmd::CancelDirectorySearch) => dir_searcher.cancel(),
                    Some(MatrixCmd::EnsureEmotePacks { room_id }) => emote_mgr.ensure(room_id),
                    Some(command) => timelines.handle(command).await,
                },
            }
        };
        client.remove_event_handler(power_handler);
        client.remove_event_handler(space_child_handler);
        client.remove_event_handler(member_handler);
        for handler in emote_handlers {
            client.remove_event_handler(handler);
        }
        for handler in room_state_handlers {
            client.remove_event_handler(handler);
        }
        verifier.stop().await;
        people_mgr.stop();
        manager.stop();
        devices.stop();
        keywords.stop();
        agent.stop();
        collector.stop();
        emote_mgr.stop();
        searcher.stop();
        user_searcher.stop();
        dir_searcher.stop();
        ignore_tracker.stop();
        if matches!(end, SessionEnd::Shutdown) {
            // Give files sent right before quitting a moment to reach the send queue's
            // persistent store before tearing timelines down.
            timelines.finish_uploads(upload::SHUTDOWN_WAIT).await;
        }
        timelines.stop();
        // Do not hang on exit if the server is unreachable.
        let _ = tokio::time::timeout(Duration::from_secs(2), sync.stop()).await;
        token_saver.abort();
        // Waited for: a save that is running must not write the session file again
        // after the session was forgotten.
        let _ = token_saver.await;
        // Once more on the way out, in case a refresh happened during the shutdown.
        if matches!(end, SessionEnd::Shutdown)
            && let Err(err) = session::save_tokens(&self.paths, client, passphrase, in_keyring)
        {
            tracing::error!(why = %crate::logsafe::kind_any(&err), "could not save the session");
        }
        Ok(end)
    }
}

/// What the room list shows under a room's name: who wrote last, and what.
async fn latest_message(
    room: &RoomListItem,
    direct: bool,
    ignored: &tokio::sync::watch::Receiver<safety::IgnoredSet>,
) -> Option<String> {
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
    if !own && safety::is_ignored_in(ignored, &sender) {
        return None;
    }
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

/// Registers a sync event handler that marks `room_id` dirty in [`manage::Manager`] on arrival.
fn watch_room_state<Ev>(
    client: &Client,
    refresh: &std::sync::Arc<manage::Refresh>,
) -> matrix_sdk::event_handler::EventHandlerHandle
where
    Ev: matrix_sdk::event_handler::SyncEvent + serde::de::DeserializeOwned + Send + Sync + 'static,
{
    let refresh = refresh.clone();
    client.add_event_handler(move |_ev: Ev, room: matrix_sdk::Room| {
        let refresh = refresh.clone();
        async move {
            manage::bump_refresh(&refresh, room.room_id().as_str());
        }
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
    fn listen(&mut self, client: &Client, rooms: &Vector<RoomListItem>, left: &left::LeftRooms) {
        while self.tasks.try_join_next().is_some() {}
        if !self.enabled {
            return;
        }
        let new: Vec<_> = rooms
            .iter()
            // Not shown, and not ours any more (see `left`): no slot for those. One that
            // comes back is listened to when it does.
            .filter(|room| !left.gone(room))
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
                    tracing::debug!(why = %crate::logsafe::kind(&err), "no latest message for a room");
                }
            }
        });
    }

    async fn line(
        &mut self,
        room: &RoomListItem,
        direct: bool,
        ignored: &tokio::sync::watch::Receiver<safety::IgnoredSet>,
    ) -> Option<String> {
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
        let line = latest_message(room, direct, ignored).await;
        self.lines.insert(id.to_owned(), (stamp, line.clone()));
        line
    }
}

async fn summarize(
    rooms: &Vector<RoomListItem>,
    previews: &mut Previews,
    ignored: &tokio::sync::watch::Receiver<safety::IgnoredSet>,
    left: &left::LeftRooms,
    notify_settings: &matrix_sdk::notification_settings::NotificationSettings,
) -> Vec<RoomSummary> {
    let mut summaries = Vec::with_capacity(rooms.len());
    for room in rooms {
        // Left in this session and brought back by a late answer: not a room of ours.
        if left.hides(room) {
            continue;
        }
        // Spaces we are in have their own navigation, they are not chats; an
        // invitation to one, or our request to join one, is answered here.
        if room.is_space() && !matches!(room.state(), RoomState::Invited | RoomState::Knocked) {
            continue;
        }
        let invited = room.state() == RoomState::Invited;
        let knocked = room.state() == RoomState::Knocked;
        if invited
            && let Ok(invite) = room.invite_details().await
            && safety::is_ignored_in(ignored, &invite.inviter_id)
        {
            continue;
        }
        let direct = room.is_direct().await.unwrap_or(false);
        let muted = manage::notification_mode(notify_settings, room).await
            == Some(RoomNotificationMode::Mute);
        let category = if invited {
            RoomCategory::Invite
        } else if knocked {
            RoomCategory::Knock
        } else if room.is_favourite() {
            RoomCategory::Favourite
        } else if room.is_low_priority() {
            RoomCategory::LowPriority
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
        // No picture for invitations or unaccepted knocks: fetching one would tell
        // a stranger's server that it was seen, before joining.
        let avatar = if invited || knocked {
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
        // Server push-rule counts intentionally remain faithful to `/sync` (so ignored
        // senders still count until marked read): subtracting them reliably would require
        // fetching unread history for every room and would otherwise mask real unread
        // messages sitting before an ignored latest message.
        let notifications = room.num_unread_notifications();
        summaries.push(RoomSummary {
            id: room.room_id().to_string(),
            name: crate::text::sanitize_line(&name),
            category,
            notifications,
            preview: previews.line(room, direct, ignored).await,
            server_notifications: room.unread_notification_counts().notification_count,
            mention: room.num_unread_mentions() > 0,
            unread: notifications > 0 || room.num_unread_messages() > 0,
            marked_unread: room.is_marked_unread(),
            avatar: avatar.as_deref().and_then(safety::safe_mxc),
            alias: room
                .canonical_alias()
                .map(|alias| crate::text::sanitize_line(alias.as_str())),
            direct,
            muted,
            encrypted: !matches!(
                room.encryption_state(),
                matrix_sdk::EncryptionState::NotEncrypted
            ),
        });
    }
    summaries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_latest_message_is_followed_for_a_room_left_here() {
        use matrix_sdk::ruma::room_id;

        let (kept, gone) = (room_id!("!kept:localhost"), room_id!("!gone:localhost"));
        let client = left::testing::Server {
            joined: vec![kept.as_str(), gone.as_str()],
            ..left::testing::Server::default()
        }
        .client()
        .await;
        let rooms = client.rooms().into_iter().map(RoomListItem::from).collect();
        let left = left::LeftRooms::default();
        left.mark(gone);
        let mut previews = Previews {
            enabled: true,
            ..Previews::default()
        };
        previews.listen(&client, &rooms, &left);
        assert!(previews.listening.contains(kept));
        assert!(!previews.listening.contains(gone));
    }

    #[test]
    fn commands_and_events_print_no_ids_and_no_error_text() {
        let command = MatrixCmd::InThread {
            root: "$root:example.org".into(),
            command: Box::new(MatrixCmd::Typing {
                room_id: "!room:example.org".into(),
                typing: true,
            }),
        };
        assert_eq!(format!("{command:?}"), "InThread(Typing(true))");
        assert_eq!(
            command.describe(),
            "InThread($root:example.org, Typing(!room:example.org, true))"
        );
        let command = MatrixCmd::Manage(ManageCmd::Leave {
            room_id: "!room:example.org".into(),
        });
        assert_eq!(format!("{command:?}"), "Manage(Leave)");
        // Files: how many, and how many of them are voice messages. No path, no
        // caption, and nothing of the sound.
        let mut command = MatrixCmd::SendFiles {
            send: 7,
            room_id: "!room:example.org".into(),
            files: vec![
                std::path::PathBuf::from("/home/someone/secret-plan.pdf").into(),
                std::path::PathBuf::from("/run/user/1000/default-audio/voice-0123456789abcdef.ogg")
                    .into(),
            ],
            caption: Some(crate::format::parse("the secret plan for example.org")),
            reply_to: Some("$event:example.org".into()),
        };
        assert_eq!(format!("{command:?}"), "SendFiles(2 files)");
        assert_eq!(command.describe(), "SendFiles(!room:example.org, 2 files)");
        if let MatrixCmd::SendFiles { files, .. } = &mut command {
            files[1].voice = Some(VoiceMeta {
                duration: Duration::from_millis(12_345),
                waveform: vec![77; 5],
            });
            // Nor does a file on its own say where it is.
            assert_eq!(format!("{files:?}"), "[OutgoingFile, OutgoingFile(voice)]");
        }
        assert_eq!(format!("{command:?}"), "SendFiles(2 files, 1 voice)");
        assert_eq!(
            command.describe(),
            "SendFiles(!room:example.org, 2 files, 1 voice)"
        );
        // A sound to transcribe: its number, and no reason as anything worded it.
        let event = MatrixEvent::SpeechFetched {
            id: 3,
            result: Err("the download from private.example.org failed".into()),
        };
        let shown = format!("{event:?} {}", event.describe());
        assert!(shown.contains("SpeechFetched(run 3"), "{shown}");
        assert!(!shown.contains("private"), "{shown}");
        assert_eq!(
            format!("{:?}", MatrixCmd::CancelSpeech { id: 3 }),
            "CancelSpeech(run 3)"
        );
        // Saved messages: neither which, nor what was found about them.
        let command = MatrixCmd::Saved(SavedCmd::Fetch {
            request: 3,
            items: vec![("!room:example.org".into(), "$event:example.org".into())],
        });
        assert_eq!(format!("{command:?}"), "Saved(Fetch(request 3, 1 items))");
        assert_eq!(command.describe(), "Saved(Fetch(request 3, 1 items))");
        let event = MatrixEvent::Saved(SavedEvent::Details {
            request: 3,
            items: vec![crate::saved::SavedDetails {
                room_id: "!room:example.org".into(),
                event_id: "$event:example.org".into(),
                sender: "Alice of example.org".into(),
                sender_id: Some("@alice:example.org".into()),
                snippet: "the secret plan for example.org".into(),
                text: crate::saved::SavedText::Text,
            }],
        });
        assert_eq!(format!("{event:?}"), "Saved(Details(request 3, 1 items))");
        let click = MatrixEvent::NoticeClicked {
            room_id: "!room:example.org".into(),
            event: Some((
                "$event:example.org".into(),
                Some("$root:example.org".into()),
            )),
        };
        assert_eq!(format!("{click:?}"), "NoticeClicked");
        // What `serve` asks for and is told: no room, no text, no event, no reason.
        let command = MatrixCmd::Agent(AgentCmd::Send {
            request: 4,
            room_id: "!room:example.org".into(),
            text: "the secret plan for example.org".into(),
        });
        assert_eq!(format!("{command:?}"), "Agent(Send(request 4))");
        assert_eq!(
            command.describe(),
            "Agent(Send(request 4, !room:example.org))"
        );
        let command = MatrixCmd::Agent(AgentCmd::Watch {
            room_id: "!room:example.org".into(),
        });
        assert_eq!(format!("{command:?}"), "Agent(Watch)");
        // Collecting a room's text: no room, no event, no name, no folder.
        let command = MatrixCmd::Collect(CollectCmd::Archive {
            request: 6,
            room_id: "!room:example.org".into(),
            span: crate::collect::Span::Days(7),
            held: 0,
            dir: "/home/alice/secret-folder".into(),
            room_name: "Plans of example.org".into(),
        });
        assert_eq!(
            format!("{command:?}"),
            "Collect(Archive(request 6, Days(7)))"
        );
        assert_eq!(
            command.describe(),
            "Collect(Archive(request 6, !room:example.org, Days(7)))"
        );
        let command = MatrixCmd::Collect(CollectCmd::Unread {
            request: 7,
            room_id: "!room:example.org".into(),
            from: Some("$event:example.org".into()),
            newest: 50,
        });
        assert_eq!(
            format!("{command:?}"),
            "Collect(Unread(request 7, from: true))"
        );
        for (event, shown) in [
            (
                CollectEvent::Progress {
                    request: 6,
                    loaded: 200,
                },
                "Collect(Progress(request 6, 200 loaded))",
            ),
            (
                CollectEvent::Unread {
                    request: 7,
                    result: Ok(UnreadText {
                        text: "the secret plan for example.org".into(),
                        messages: 1,
                        cut: false,
                        known: true,
                    }),
                },
                "Collect(Unread(request 7, 1 messages))",
            ),
            (
                CollectEvent::Archived {
                    request: 6,
                    result: Ok(Archived {
                        path: "/home/alice/secret-folder/Plans.md".into(),
                        messages: 3,
                        cut: None,
                    }),
                },
                "Collect(Archived(request 6, 3 messages))",
            ),
            (
                CollectEvent::Archived {
                    request: 6,
                    result: Err("could not write /home/alice/secret-folder".into()),
                },
                "Collect(Archived(request 6, ok: false))",
            ),
        ] {
            assert_eq!(format!("{:?}", MatrixEvent::Collect(event)), shown);
        }
        let told = crate::agent::proto::Msg {
            event_id: "$event:example.org".into(),
            sender: "@alice:example.org".into(),
            sender_name: "Alice of example.org".into(),
            timestamp_ms: 1,
            kind: "text".into(),
            body: "the secret plan for example.org".into(),
            edited: false,
            own: false,
            truncated: false,
            warning: None,
            verified: true,
            untrusted: None,
        };
        for (event, shown) in [
            (
                AgentEvent::Sent {
                    request: 4,
                    result: Ok("$event:example.org".into()),
                },
                "Agent(Sent(request 4, ok: true))",
            ),
            (
                AgentEvent::Sent {
                    request: 4,
                    result: Err("#secret:example.org refused it".into()),
                },
                "Agent(Sent(request 4, ok: false))",
            ),
            (
                AgentEvent::Read {
                    request: 5,
                    result: Ok(vec![told.clone()]),
                },
                "Agent(Read(request 5, 1 messages))",
            ),
            (
                AgentEvent::Watched {
                    room_id: "!room:example.org".into(),
                    messages: vec![told.clone(), told],
                },
                "Agent(Watched(2 messages))",
            ),
            (
                AgentEvent::WatchEnded {
                    room_id: "!room:example.org".into(),
                    reason: "not in #secret:example.org".into(),
                },
                "Agent(WatchEnded)",
            ),
        ] {
            assert_eq!(format!("{:?}", MatrixEvent::Agent(event)), shown);
        }
        let event = MatrixEvent::Agent(AgentEvent::WatchEnded {
            room_id: "!room:example.org".into(),
            reason: "gone".into(),
        });
        assert_eq!(
            event.describe(),
            "Agent(WatchEnded(!room:example.org, gone))"
        );
        let event = MatrixEvent::Timeline {
            room_id: "!room:example.org".into(),
            generation: 3,
            messages: Vec::new(),
            reached_start: true,
            detached: None,
        };
        assert_eq!(
            format!("{event:?}"),
            "Timeline(generation 3, 0 messages, start: true, detached: false)"
        );
        let failed = "Could not join #secret:example.org: not allowed";
        for event in [
            MatrixEvent::ActionFailed(failed.into()),
            MatrixEvent::Error(failed.into()),
            MatrixEvent::LoginFailed(failed.into()),
            MatrixEvent::Sync(SyncStatus::Error(failed.into())),
            MatrixEvent::LoggedIn {
                user_id: "@alice:example.org".into(),
            },
        ] {
            let text = format!("{event:?}");
            assert!(!text.contains("example.org"), "{text}");
        }
        assert_eq!(
            MatrixEvent::ActionFailed(failed.into()).describe(),
            format!("ActionFailed({failed})")
        );
        let packs = |room_id: Option<&str>| MatrixEvent::EmotePacks {
            room_id: room_id.map(str::to_owned),
            catalog: EmoteCatalog::default(),
        };
        let event = packs(Some("!room:example.org"));
        assert_eq!(
            format!("{event:?}"),
            "EmotePacks(EmoteCatalog { packs: 0 })"
        );
        assert_eq!(
            event.describe(),
            "EmotePacks(!room:example.org, EmoteCatalog { packs: 0 })"
        );
        assert_eq!(
            format!("{:?}", packs(None)),
            "EmotePacks(no room, EmoteCatalog { packs: 0 })"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_invitation_to_a_space_is_listed_with_the_rooms_and_a_space_we_are_in_is_not() {
        use super::spaces::fake;
        let server = fake::Server::start(serde_json::json!({
            "join": {
                "!work:localhost": fake::joined(true, vec![]),
                "!rats:localhost": fake::joined(false, vec![]),
            },
            "invite": { "!club:localhost": fake::invited(true) },
            "knock": { "!guild:localhost": fake::knocked(true) },
        }))
        .await;
        let client = fake::client(&server).await;
        let rooms: Vector<RoomListItem> =
            client.rooms().into_iter().map(RoomListItem::from).collect();
        // All but one are spaces, as far as the store knows, whatever our membership.
        let spaces = rooms.iter().filter(|room| room.is_space()).count();
        assert_eq!((rooms.len(), spaces), (4, 3));
        let (_ignore, ignored) = tokio::sync::watch::channel(safety::IgnoredSet::default());
        let settings = client.notification_settings().await;
        let mut listed: Vec<(String, RoomCategory)> = summarize(
            &rooms,
            &mut Previews::default(),
            &ignored,
            &left::LeftRooms::default(),
            &settings,
        )
        .await
        .into_iter()
        .map(|room| (room.id, room.category))
        .collect();
        listed.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            listed,
            [
                ("!club:localhost".to_owned(), RoomCategory::Invite),
                ("!guild:localhost".to_owned(), RoomCategory::Knock),
                ("!rats:localhost".to_owned(), RoomCategory::Room),
            ]
        );
    }

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
            password: password.into(),
        });
        let text = format!("preview {}", std::process::id());
        let mut sent = false;
        let wait = async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    MatrixEvent::Rooms(rooms) => {
                        let joined = rooms.iter().find(|r| !r.is_pending());
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
            password: password.into(),
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
            password: password.into(),
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
                        let joined = rooms.iter().find(|r| !r.is_pending());
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

    /// A poll from start to end, and a sticker, against a real server: what the worker
    /// sends must come back through the SDK's timeline as what the UI shows.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn a_poll_is_started_voted_in_and_ended_and_a_sticker_arrives() {
        use crate::model::SendState;
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let username = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let root = std::env::temp_dir().join(format!("matrix-rat-poll-{}", std::process::id()));
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

        let question = format!("poll test {}?", std::process::id());
        let sticker = format!("sticker test {}", std::process::id());
        let wait = async {
            let mut room_id: Option<String> = None;
            // What was asked of the worker so far: voted, ended.
            let (mut voted, mut ended) = (false, false);
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err)
                    | MatrixEvent::Error(err)
                    | MatrixEvent::ActionFailed(err) => panic!("{err}"),
                    MatrixEvent::Rooms(rooms) if room_id.is_none() => {
                        if let Some(room) = rooms.iter().find(|r| !r.is_pending()) {
                            room_id = Some(room.id.clone());
                            handle.send(MatrixCmd::OpenRoom {
                                room_id: room.id.clone(),
                                generation: 1,
                            });
                            handle.send(MatrixCmd::SendPoll {
                                room_id: room.id.clone(),
                                question: question.clone(),
                                answers: vec!["one".into(), "  ".into(), "two".into()],
                                disclosed: true,
                                max_selections: 1,
                            });
                            handle.send(MatrixCmd::SendSticker {
                                room_id: room.id.clone(),
                                name: sticker.clone(),
                                url: format!("mxc://localhost/sticker{}", std::process::id()),
                                dimensions: Some((64, 64)),
                                size: Some(1234),
                                mime: Some("image/png".into()),
                            });
                        }
                    }
                    MatrixEvent::Timeline {
                        generation: 1,
                        messages,
                        ..
                    } => {
                        let room_id = room_id.clone().expect("a room is open");
                        let sticker_seen = messages.iter().any(|m| {
                            m.own
                                && m.state == SendState::Sent
                                && m.image
                                    .as_ref()
                                    .is_some_and(|image| image.sticker && image.name == sticker)
                        });
                        let Some(message) = messages.iter().find(|m| {
                            m.state == SendState::Sent
                                && m.poll.as_ref().is_some_and(|p| p.question == question)
                        }) else {
                            continue;
                        };
                        // Voting needs the event's id, which the server has to give first.
                        let Some(ItemRef::Event(poll_event_id)) = message.item.clone() else {
                            continue;
                        };
                        let poll = message.poll.as_ref().unwrap();
                        // The blank answer was dropped on the way out.
                        let texts: Vec<&str> =
                            poll.answers.iter().map(|a| a.text.as_str()).collect();
                        assert_eq!(texts, ["one", "two"]);
                        assert!(poll.disclosed && !poll.is_multi());
                        // The question is not in what a log line of this event would hold.
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
                        assert!(!debug.contains(&question));
                        if !voted {
                            assert!(message.editable(), "an unvoted own poll can be edited");
                            voted = true;
                            handle.send(MatrixCmd::VotePoll {
                                room_id,
                                poll_event_id: poll_event_id.clone(),
                                answers: vec![poll.answers[1].id.clone()],
                            });
                        } else if !ended {
                            if poll.answers[1].own && poll.answers[1].votes == 1 {
                                assert_eq!(poll.total_voters, 1);
                                assert!(!poll.answers[0].own);
                                ended = true;
                                handle.send(MatrixCmd::EndPoll {
                                    room_id,
                                    poll_event_id: poll_event_id.clone(),
                                });
                            }
                        } else if poll.ended && sticker_seen {
                            assert_eq!(poll.winner_max(), 1);
                            assert!(!message.editable());
                            return;
                        }
                    }
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(40), wait)
            .await
            .expect("timed out waiting for the poll and the sticker");
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
            password: String::from("pw").into(),
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
            password: String::from("pw").into(),
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
            password: String::from("pw").into(),
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
            password: String::from("pw").into(),
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
            password: String::from("pw").into(),
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
