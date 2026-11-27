//! What the UI shows: rooms and messages. Filled with demo data until the Matrix
//! worker arrives (milestone M2).

use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomKind {
    Invite,
    Knock,
    Favourite,
    Person,
    Room,
    LowPriority,
}

impl RoomKind {
    pub const SECTIONS: [RoomKind; 6] = [
        RoomKind::Invite,
        RoomKind::Knock,
        RoomKind::Favourite,
        RoomKind::Person,
        RoomKind::Room,
        RoomKind::LowPriority,
    ];

    pub fn title(self) -> &'static str {
        match self {
            RoomKind::Invite => "Invites",
            RoomKind::Knock => "Pending knocks",
            RoomKind::Favourite => "Favourites",
            RoomKind::Person => "People",
            RoomKind::Room => "Rooms",
            RoomKind::LowPriority => "Low priority",
        }
    }

    pub fn is_pending(self) -> bool {
        matches!(self, Self::Invite | Self::Knock)
    }

    /// Toast message shown when trying a member-only action on an unresolved invite or knock.
    pub fn pending_hint(self) -> &'static str {
        match self {
            Self::Knock => "Wait for your join request to be accepted first",
            Self::Invite | Self::Favourite | Self::Person | Self::Room | Self::LowPriority => {
                "Accept the invitation first"
            }
        }
    }
}

/// Aggregated unread state for a space (or "All rooms").
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SpaceUnread {
    pub unread: bool,
    pub notifications: u64,
    pub mention: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Reaction {
    pub key: String,
    pub count: u32,
    pub own: bool,
    /// Ours, and it could not be sent: it waits to be tried again or removed.
    pub failed: bool,
}

/// Hand-written: the key of a reaction can be any text.
impl fmt::Debug for Reaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Reaction(count: {}, own: {}, failed: {})",
            self.count, self.own, self.failed
        )
    }
}

/// What of a message that is on the server could not be sent (see
/// [`Message::failed_change`]): what Del discards, and what the status line names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedChange {
    Edit,
    Deletion,
    Reaction,
    /// More than one of them.
    Several,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MsgKind {
    #[default]
    Text,
    /// A `/me` action, shown as "* alice waves".
    Emote,
    /// Joins, leaves, renames and the like: one muted line, no header.
    System,
    Redacted,
    Undecryptable,
    /// The "new messages below" line.
    ReadMarker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SendState {
    #[default]
    Sent,
    Sending,
    Failed,
}

/// How the Matrix side addresses a message, for replies, edits, reactions and deletion.
#[derive(Clone, PartialEq, Eq)]
pub enum ItemRef {
    Event(String),
    /// A message of ours that the server has not confirmed yet.
    Transaction(String),
}

/// Hand-written: which event it is stays out of logs.
impl fmt::Debug for ItemRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Event(_) => "ItemRef::Event",
            Self::Transaction(_) => "ItemRef::Transaction",
        })
    }
}

#[derive(Clone, Default)]
pub struct Message {
    /// Stable identity within the room's timeline; empty for demo data.
    pub id: String,
    /// `None` for demo data and for lines that are not events.
    pub item: Option<ItemRef>,
    pub kind: MsgKind,
    pub state: SendState,
    pub sender: String,
    /// Full Matrix user ID (`@user:server`), when known (real events from the SDK).
    pub sender_id: Option<String>,
    /// Someone else in the room goes by the same name, as the SDK finds: the name is
    /// shown with the user ID (see [`Message::sender_name`]).
    pub sender_ambiguous: bool,
    pub time: String,
    pub day: String,
    /// When it was sent, in milliseconds since 1970: as the sender's server says, or
    /// for a message of ours on its way, when it was queued. `None` for demo data.
    pub sent_at: Option<u64>,
    /// It came encrypted, whether or not it could be decrypted.
    pub encrypted: bool,
    pub body: String,
    pub own: bool,
    pub edited: bool,
    pub pinned: bool,
    /// The user saved it for later (see `crate::saved`); set by the app, not the worker.
    pub saved: bool,
    /// The formatted (HTML) version of the body, when the sender provided one.
    pub html: Option<String>,
    /// Sender and text of the message this one replies to.
    pub reply_to: Option<(String, String)>,
    /// The user ID of whoever wrote the message replied to, when known: who is quoted
    /// is told apart from a namesake as a sender is (see [`mark_namesakes`]).
    pub reply_sender: Option<String>,
    /// For a line about someone (a membership change, a new profile): their name as
    /// the line begins with it, and their user ID (see [`mark_namesakes`]).
    pub subject: Option<(String, String)>,
    /// Event id of the message this one replies to.
    pub reply_event: Option<String>,
    pub reactions: Vec<Reaction>,
    /// Set for picture messages; `body` then holds the text shown when pictures are off.
    pub image: Option<ImageInfo>,
    /// Set for file, audio and video messages.
    pub file: Option<Attachment>,
    /// Set for a message that started a thread.
    pub thread: Option<ThreadSummary>,
    /// Set for Matrix poll start events (`org.matrix.msc3381.poll.start` / `m.poll.*`).
    pub poll: Option<PollInfo>,
    /// Bytes sent and bytes in all, while a file of ours is on its way to the server.
    pub upload: Option<(u64, u64)>,
    /// Why sending failed, in a few words.
    pub send_error: Option<String>,
    /// Our edit of it could not be sent: why, in a few words.
    pub edit_error: Option<String>,
    /// Our deletion of it could not be sent: why, in a few words.
    pub delete_error: Option<String>,
    /// A message of ours on its way, held up behind a change that could not be sent
    /// in the same room (see `prepare_messages`).
    pub held: bool,
    /// A doubt about who really sent this (encrypted rooms only).
    pub shield: Option<Shield>,
    /// Mentions (`m.mentions`) carried by the message, used to preserve them on edit.
    pub mentions: Option<matrix_sdk::ruma::events::Mentions>,
    /// Who has read up to this message (and no further), as far as they tell.
    pub read_by: ReadBy,
    /// Why the message could not be decrypted (`kind` is `Undecryptable`). `None`
    /// for demo data.
    pub utd: Option<UtdWhy>,
    /// Who deleted it and why (`kind` is `Redacted`). `None` for demo data; `body`
    /// still says "Message deleted", for previews, replies, notifications and agents.
    pub deleted: Option<Deletion>,
}

/// Who deleted a message, and why.
#[derive(Clone, PartialEq, Eq)]
pub struct Deletion {
    /// Made safe for one line and at most [`Deletion::REASON_MAX`] columns; `None`
    /// where none was given. Someone else's text: never logged, never handed to agents.
    pub reason: Option<String>,
    pub by: DeletedBy,
}

/// Who deleted a message.
#[derive(Clone, PartialEq, Eq)]
pub enum DeletedBy {
    /// Its sender, or someone not known: the server did not say.
    Sender,
    /// We did, someone else's message.
    Us,
    /// Someone else did: a moderator. `name` as the timeline shows names (with the
    /// user ID where someone else goes by it too, see [`mark_namesakes`]); the user ID
    /// until the name is known.
    Other { user_id: String, name: String },
}

impl Deletion {
    /// The longest reason kept, in columns.
    pub const REASON_MAX: usize = 500;

    /// The line a deleted message shows: "Message deleted: why", "Deleted by carol:
    /// why", "Deleted by you".
    pub fn line(&self) -> String {
        let who = match &self.by {
            DeletedBy::Sender => "Message deleted",
            DeletedBy::Us => "Deleted by you",
            DeletedBy::Other { name, .. } => &format!("Deleted by {name}"),
        };
        match &self.reason {
            Some(reason) => format!("{who}: {reason}"),
            None => who.to_owned(),
        }
    }
}

/// Hand-written: the reason and who deleted it stay out of logs.
impl fmt::Debug for Deletion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deletion")
            .field("reason", &self.reason.is_some())
            .field("by", &self.by)
            .finish()
    }
}

/// Hand-written: who it was stays out of logs.
impl fmt::Debug for DeletedBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Sender => "Sender",
            Self::Us => "Us",
            Self::Other { .. } => "Other",
        })
    }
}

/// Hand-written: what it says, who sent it and which event it is stay out of logs.
impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("kind", &self.kind)
            .field("state", &self.state)
            .field("own", &self.own)
            .field("encrypted", &self.encrypted)
            .field("edited", &self.edited)
            .field("body_bytes", &self.body.len())
            .field("reply", &self.reply_to.is_some())
            .field("reactions", &self.reactions.len())
            .field("image", &self.image.is_some())
            .field("file", &self.file.is_some())
            .field("thread", &self.thread)
            .field("poll", &self.poll)
            .field("upload", &self.upload)
            .field("send_error", &self.send_error.is_some())
            .field("edit_error", &self.edit_error.is_some())
            .field("delete_error", &self.delete_error.is_some())
            .field("held", &self.held)
            .field("shield", &self.shield)
            .field("utd", &self.utd)
            .field("deleted", &self.deleted)
            .finish_non_exhaustive()
    }
}

/// Why a message could not be decrypted, as far as the SDK can tell (its `UtdCause`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UtdWhy {
    /// The key is missing and nothing explains it: it may still arrive.
    NoKey,
    /// Sent while this account was not in the room.
    SentBeforeJoin,
    /// Sent before this device existed, and the account has no key backup.
    HistoryNoBackup,
    /// Sent before this device existed; there is a backup, but this device is not
    /// verified and so cannot use it.
    HistoryNeedsVerification,
    /// The sender refuses to give the key to a device like this one (not verified).
    WithheldUnverified,
    /// The sender's device did not give this device the key.
    Withheld,
    /// Refused here: the sender was verified once and their identity has changed.
    SenderIdentityChanged,
    /// Refused here: the sending device is not signed by its owner.
    SenderDeviceUnsigned,
    /// Refused here: the sending device is not known (deleted, or not fetched yet).
    SenderDeviceUnknown,
    /// Not encrypted in a way this program understands.
    Unsupported,
    /// Damaged or not built as it should be (the SDK's `UtdCause::Malformed`): no key
    /// will help.
    Malformed,
}

impl UtdWhy {
    /// What the message shows in place of its text.
    pub fn text(self) -> &'static str {
        match self {
            Self::NoKey => "Unable to decrypt: the key for this message has not arrived",
            Self::SentBeforeJoin => "Unable to decrypt: sent before you joined the room",
            Self::HistoryNoBackup => {
                "Unable to decrypt: sent before this device was logged in, and your account \
                 has no key backup"
            }
            Self::HistoryNeedsVerification => {
                "Unable to decrypt: sent before this device was logged in. Verify this \
                 device to read it"
            }
            Self::WithheldUnverified => {
                "Unable to decrypt: the sender shares keys with verified devices only. \
                 Verify this device"
            }
            Self::Withheld => {
                "Unable to decrypt: the sender's device did not share the key with this device"
            }
            Self::SenderIdentityChanged => {
                "Not decrypted: the sender's verified identity has changed"
            }
            Self::SenderDeviceUnsigned => {
                "Not decrypted: sent from a device its owner has not verified"
            }
            Self::SenderDeviceUnknown => {
                "Not decrypted: sent from a device that is unknown or was removed"
            }
            Self::Unsupported => "Unable to decrypt: unknown kind of encryption",
            Self::Malformed => "Unable to decrypt: the message is malformed",
        }
    }

    /// What a "Try again" that changed nothing says: what would help, if anything.
    pub fn advice(self) -> &'static str {
        match self {
            Self::NoKey | Self::Withheld => {
                "Still no key. It may come from another of your devices or from the sender."
            }
            Self::SentBeforeJoin => {
                "Still no key: messages from before you joined stay unreadable unless \
                 their keys are shared with you."
            }
            Self::HistoryNoBackup => {
                "Still no key. Without a key backup, only a key file exported from another \
                 of your devices can bring it."
            }
            Self::HistoryNeedsVerification | Self::WithheldUnverified => {
                "Still no key. Verify this device (or enter your recovery key) first."
            }
            Self::SenderIdentityChanged
            | Self::SenderDeviceUnsigned
            | Self::SenderDeviceUnknown => {
                "Still not decrypted: the sender's device does not pass this device's checks."
            }
            Self::Unsupported => "This kind of encryption is not supported.",
            Self::Malformed => "The message is malformed and cannot be decrypted.",
        }
    }
}

/// The people whose read receipt sits on a message: never ourselves, never its sender.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadBy {
    /// Newest first; at most `ReadBy::KEPT` of them.
    pub readers: Vec<Reader>,
    /// How many there are in all.
    pub total: u32,
}

impl ReadBy {
    /// Names kept per message; the rest are only counted.
    pub const KEPT: usize = 20;

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Reader {
    /// Display name, safe to show.
    pub name: String,
    /// Their user ID, which their colour comes from as a sender's does; `None` for
    /// demo data.
    pub id: Option<String>,
    /// Someone else in the room goes by the same name (see [`Reader::shown_name`]).
    pub ambiguous: bool,
    /// When they read it ("14:05", or with the day when it was not today); empty if
    /// the receipt does not say.
    pub time: String,
}

impl Reader {
    /// What tells them from anyone else (see [`Message::sender_key`]).
    pub fn key(&self) -> &str {
        self.id.as_deref().unwrap_or(&self.name)
    }

    /// Their name as a list of readers shows it: with their user ID where someone else
    /// in the room goes by the same name. (A read marker has no room for the ID: its
    /// initials are in the colour of the ID instead.)
    pub fn shown_name(&self) -> std::borrow::Cow<'_, str> {
        match &self.id {
            Some(id) if self.ambiguous => disambiguated(&self.name, id).into(),
            _ => self.name.as_str().into(),
        }
    }
}

/// Hand-written: who read it stays out of logs.
impl fmt::Debug for Reader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reader").finish_non_exhaustive()
    }
}

/// Why a message in an encrypted room cannot be fully trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shield {
    /// Sent from a device its owner has not verified (or that is unknown).
    UnverifiedDevice,
    /// The sender's identity changed after having been verified.
    IdentityChanged,
    /// The sender named in the message is not the one who encrypted it.
    SenderMismatch,
    /// Arrived unencrypted although the room is encrypted.
    NotEncrypted,
    /// The key for this message did not come straight from the sender's device (it
    /// was restored from the backup or passed on by another device), or the device
    /// could not be tied to the sender. Usual for older history on a new device.
    Unconfirmed,
}

impl Shield {
    pub fn label(self) -> &'static str {
        match self {
            Shield::UnverifiedDevice => "⚠ from an unverified device",
            Shield::IdentityChanged => "⚠ sender's identity has changed",
            Shield::SenderMismatch => "sent on the sender's behalf (bridge or relay)",
            Shield::NotEncrypted => "not encrypted",
            Shield::Unconfirmed => "sender not confirmed",
        }
    }

    /// A warning, as opposed to a remark. A sender mismatch is what every bridged
    /// message looks like (the bridge encrypts on behalf of the people on the other
    /// side), so it is stated but not painted as an alarm.
    pub fn serious(self) -> bool {
        matches!(self, Shield::UnverifiedDevice | Shield::IdentityChanged)
    }
}

/// One line under a message that started a thread.
#[derive(Clone, Default)]
pub struct ThreadSummary {
    pub replies: u32,
    /// Sender and start of the newest reply, when known.
    pub latest: Option<(String, String)>,
    /// The user ID of the newest reply's sender, when known (see [`mark_namesakes`]).
    pub latest_sender: Option<String>,
    /// There are replies the user has not read.
    pub unread: bool,
    /// The replies themselves, for the demo only; with an account they are loaded
    /// when the thread is opened.
    pub demo_replies: Vec<Message>,
}

/// Hand-written: the newest reply's sender and text stay out of logs.
impl fmt::Debug for ThreadSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadSummary")
            .field("replies", &self.replies)
            .field("latest", &self.latest.is_some())
            .field("unread", &self.unread)
            .field("demo_replies", &self.demo_replies.len())
            .finish()
    }
}

/// One answer option in a Matrix poll (`m.poll.*`).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct PollAnswer {
    pub id: String,
    pub text: String,
    pub votes: u32,
    /// Whether the signed-in account currently votes for this answer.
    pub own: bool,
}

impl std::fmt::Debug for PollAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PollAnswer").finish_non_exhaustive()
    }
}

/// A Matrix poll (MSC3381 / `m.poll.*`) attached to a timeline message.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct PollInfo {
    pub question: String,
    pub disclosed: bool,
    /// Maximum answers a voter may pick (`1` = single choice, `> 1` = multiple choice).
    pub max_selections: u64,
    pub answers: Vec<PollAnswer>,
    /// Distinct non-ignored voters across all answers (masked while undisclosed & open).
    pub total_voters: u32,
    pub ended: bool,
    /// Whether the SDK considers this poll editable (own, no votes recorded, not ended).
    pub editable: bool,
}

impl std::fmt::Debug for PollInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PollInfo")
            .field("disclosed", &self.disclosed)
            .field("max_selections", &self.max_selections)
            .field("answers", &self.answers.len())
            .field("ended", &self.ended)
            .field("editable", &self.editable)
            .finish_non_exhaustive()
    }
}

impl PollInfo {
    /// Whether vote counts, bars and voter totals may be shown yet.
    ///
    /// Undisclosed polls hide counts and totals until an `m.poll.end` event arrives;
    /// while open, only the signed-in user's own selection mark is visible.
    pub fn shows_counts(&self) -> bool {
        self.disclosed || self.ended
    }

    /// Whether voters may select more than one answer.
    pub fn is_multi(&self) -> bool {
        self.max_selections > 1
    }

    /// Effective selection limit for multi-choice polls, clamped to existing answers.
    pub fn effective_max(&self) -> usize {
        if self.answers.is_empty() {
            return 0;
        }
        usize::try_from(self.max_selections)
            .unwrap_or(self.answers.len())
            .clamp(1, self.answers.len())
    }

    /// Answer IDs the signed-in account currently votes for, in display order.
    pub fn own_selection_ids(&self) -> Vec<String> {
        self.answers
            .iter()
            .filter(|a| a.own)
            .map(|a| a.id.clone())
            .collect()
    }

    /// Highest vote count among answers once ended and revealed, or `0` when open or empty.
    pub fn winner_max(&self) -> u32 {
        if !(self.ended && self.shows_counts()) {
            return 0;
        }
        self.answers.iter().map(|a| a.votes).max().unwrap_or(0)
    }

    /// Next answer ID list when toggling answer `idx` directly from the timeline:
    /// single-choice switches immediately (or retracts if clicking the already-selected
    /// answer), while multi-choice toggles `idx` up to [`Self::effective_max`]. Returns
    /// `Err(max)` if adding `idx` would exceed `max`.
    pub fn toggled_selection(&self, idx: usize) -> Result<Vec<String>, usize> {
        let Some(target) = self.answers.get(idx) else {
            return Ok(self.own_selection_ids());
        };
        if !self.is_multi() {
            return Ok(if target.own {
                Vec::new()
            } else {
                vec![target.id.clone()]
            });
        }
        let max = self.effective_max();
        let mut next = self.own_selection_ids();
        if let Some(pos) = next.iter().position(|id| id == &target.id) {
            next.remove(pos);
            return Ok(next);
        }
        if next.len() >= max {
            return Err(max);
        }
        next.push(target.id.clone());
        Ok(next)
    }
}

impl ThreadSummary {
    /// The summary of a thread whose replies are all at hand (the demo).
    pub fn of(replies: Vec<Message>) -> Option<Self> {
        let latest = replies.last()?;
        Some(Self {
            replies: replies.len() as u32,
            latest: Some((latest.sender_name().into_owned(), latest.body.clone())),
            latest_sender: latest.sender_id.clone(),
            unread: false,
            demo_replies: replies,
        })
    }
}

/// What kind of attachment this is, for typed labels and playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MediaKind {
    #[default]
    File,
    Audio,
    Voice,
    Video,
    Picture,
}

/// Something that can be saved to disk: a file, or the original of a picture.
#[derive(Clone)]
pub struct Attachment {
    /// The file's `mxc://` address.
    pub key: String,
    /// File name as the sender gave it, safe to show (not yet safe as a path).
    pub name: String,
    /// Size in bytes, as stated by the sender.
    pub size: Option<u64>,
    pub kind: MediaKind,
    pub source: Box<matrix_sdk::ruma::events::room::MediaSource>,
    /// For voice and audio: what the sender said about the sound, to draw how far it
    /// has played.
    pub audio: Option<std::sync::Arc<AudioMeta>>,
}

/// Hand-written: an encrypted source carries the key of the file, and the name can
/// say what the file is about.
impl std::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Attachment({:?}, size: {:?}, audio: {:?})",
            self.kind, self.size, self.audio
        )
    }
}

/// What a voice or audio message says about its sound. From the sender, so bounded
/// (see `crate::audio`): the length is believable, the waveform short.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct AudioMeta {
    pub duration: Option<std::time::Duration>,
    /// Loudness along the sound, from 0 to 1024; at most a few hundred values.
    pub waveform: Vec<u16>,
    /// The stated type (`audio/ogg`), only to give the played copy a file ending.
    pub mime: Option<String>,
}

/// Hand-written: only the shape (the type is the sender's text).
impl fmt::Debug for AudioMeta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioMeta")
            .field("duration", &self.duration)
            .field("waveform", &self.waveform.len())
            .field("mime", &self.mime.is_some())
            .finish()
    }
}

/// A picture attached to a message.
#[derive(Clone)]
pub struct ImageInfo {
    /// Identifies the picture in the image cache (its `mxc://` address).
    pub key: String,
    pub source: ImageSource,
    /// File name, safe to show.
    pub name: String,
    /// Text the sender wrote under the picture.
    pub caption: Option<String>,
    /// Pixel size as stated by the sender; not to be trusted beyond layout.
    pub dimensions: Option<(u32, u32)>,
    /// File size in bytes, as stated by the sender.
    pub size: Option<u64>,
    /// Whether this picture is an `m.sticker` (small size, no caption line).
    pub sticker: bool,
}

/// Hand-written: its address, its name and the caption stay out of logs.
impl fmt::Debug for ImageInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageInfo")
            .field("source", &self.source)
            .field("caption", &self.caption.is_some())
            .field("dimensions", &self.dimensions)
            .field("size", &self.size)
            .field("sticker", &self.sticker)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub enum ImageSource {
    /// A picture made up by the app itself, for the demo.
    Demo,
    Matrix(Box<MediaRef>),
}

/// Where the Matrix worker finds a picture.
#[derive(Clone)]
pub struct MediaRef {
    pub full: matrix_sdk::ruma::events::room::MediaSource,
    /// A smaller version provided by the sender, with its size in bytes if stated.
    pub thumbnail: Option<(matrix_sdk::ruma::events::room::MediaSource, Option<u64>)>,
}

/// Hand-written: an encrypted source carries the key of the file.
impl std::fmt::Debug for ImageSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Demo => "Demo",
            Self::Matrix(_) => "Matrix",
        })
    }
}

impl ImageInfo {
    /// The original file of the picture, for saving. The demo picture has none.
    pub fn attachment(&self) -> Option<Attachment> {
        match &self.source {
            ImageSource::Demo => None,
            ImageSource::Matrix(media) => Some(Attachment {
                key: self.key.clone(),
                name: self.name.clone(),
                size: self.size,
                kind: MediaKind::Picture,
                source: Box::new(media.full.clone()),
                audio: None,
            }),
        }
    }
}

impl Message {
    /// Returns the Matrix event ID if this message corresponds to a confirmed timeline event.
    pub fn event_id(&self) -> Option<&str> {
        match &self.item {
            Some(ItemRef::Event(id)) => Some(id.as_str()),
            _ => None,
        }
    }

    /// The sender's name as shown: with their user ID after it where someone else in
    /// the room goes by the same name.
    pub fn sender_name(&self) -> std::borrow::Cow<'_, str> {
        match &self.sender_id {
            Some(id) if self.sender_ambiguous => disambiguated(&self.sender, id).into(),
            _ => self.sender.as_str().into(),
        }
    }

    /// What tells the sender from anyone else (their colour comes from it): their user
    /// ID, or their name where that is not known.
    pub fn sender_key(&self) -> &str {
        self.sender_id.as_deref().unwrap_or(&self.sender)
    }

    /// Whether `other` is by the same person: the same user ID where both have one,
    /// else the same name.
    pub fn same_sender(&self, other: &Message) -> bool {
        match (&self.sender_id, &other.sender_id) {
            (Some(one), Some(two)) => one == two,
            _ => self.sender == other.sender,
        }
    }

    /// What a deleted message shows in place of its text: who deleted it and why, where
    /// that is known (see [`Deletion::line`]).
    pub fn deleted_line(&self) -> std::borrow::Cow<'_, str> {
        match &self.deleted {
            Some(deletion) => deletion.line().into(),
            None => self.body.as_str().into(),
        }
    }

    /// The people the message names, each by their name as given (not told apart)
    /// and user ID: its sender, whom its line is about, whoever wrote what it quotes,
    /// the latest reply of its thread, its readers, and whoever deleted it.
    fn people(&self) -> impl Iterator<Item = (&str, &str)> {
        let sender = self
            .sender_id
            .as_deref()
            .filter(|_| self.kind != MsgKind::ReadMarker)
            .map(|id| (self.sender.as_str(), id));
        let subject = self
            .subject
            .as_ref()
            .map(|(name, id)| (name.as_str(), id.as_str()));
        let quoted = self
            .reply_to
            .as_ref()
            .zip(self.reply_sender.as_deref())
            .map(|((name, _), id)| (plain_name(name, id), id));
        let latest = self.thread.as_ref().and_then(|thread| {
            let (name, _) = thread.latest.as_ref()?;
            let id = thread.latest_sender.as_deref()?;
            Some((plain_name(name, id), id))
        });
        let readers = self.read_by.readers.iter().filter_map(|reader| {
            let id = reader.id.as_deref()?;
            Some((reader.name.as_str(), id))
        });
        let deleter = match &self.deleted {
            Some(Deletion {
                by: DeletedBy::Other { user_id, name },
                ..
            }) => Some((plain_name(name, user_id), user_id.as_str())),
            _ => None,
        };
        sender
            .into_iter()
            .chain(subject)
            .chain(quoted)
            .chain(latest)
            .chain(readers)
            .chain(deleter)
    }

    /// True for non-interactive status or divider rows (`System` or `ReadMarker`).
    pub fn is_system_or_marker(&self) -> bool {
        matches!(self.kind, MsgKind::System | MsgKind::ReadMarker)
    }

    /// What is shown after a message of ours that is on its way.
    pub fn sending_label(&self) -> String {
        match self.upload {
            Some((sent, total)) if total > 0 => {
                format!("◌ uploading {}%", sent.min(total) * 100 / total)
            }
            _ if self.file.is_some() || self.image.is_some() => "◌ uploading…".to_owned(),
            _ => "◌ sending".to_owned(),
        }
    }

    /// What of this message could not be sent, if anything: the message itself, or a
    /// change of ours to it (an edit, a deletion, a reaction).
    pub fn failed_any(&self) -> bool {
        self.state == SendState::Failed || self.failed_change().is_some()
    }

    /// Which change of ours to this message could not be sent, if any. The message
    /// may be someone else's: our reaction to it, or our deletion as a moderator.
    pub fn failed_change(&self) -> Option<FailedChange> {
        let reactions = self.reactions.iter().filter(|r| r.failed).count();
        let edit = usize::from(self.edit_error.is_some());
        let deletion = usize::from(self.delete_error.is_some());
        match (edit, deletion, reactions) {
            (0, 0, 0) => None,
            (1, 0, 0) => Some(FailedChange::Edit),
            (0, 1, 0) => Some(FailedChange::Deletion),
            (0, 0, 1) => Some(FailedChange::Reaction),
            _ => Some(FailedChange::Several),
        }
    }

    /// What is shown after a message of ours that could not be sent.
    pub fn failed_label(&self) -> String {
        match &self.send_error {
            Some(reason) => format!("✕ not sent: {reason}"),
            None => "✕ not sent".to_owned(),
        }
    }

    /// Whether the user can change this message: their own text, `/me` action, or an
    /// open unvoted poll, not a file, a picture, or something deleted.
    pub fn editable(&self) -> bool {
        if let Some(poll) = &self.poll {
            return self.own && !poll.ended && poll.editable;
        }
        self.own
            && self.image.is_none()
            && self.file.is_none()
            && matches!(self.kind, MsgKind::Text | MsgKind::Emote)
    }

    /// Whether `file` is audio, voice, or video (played rather than opened as a document).
    pub fn is_playable(&self) -> bool {
        self.file.as_ref().is_some_and(|file| {
            matches!(
                file.kind,
                MediaKind::Audio | MediaKind::Voice | MediaKind::Video
            )
        })
    }

    /// Whether this is a voice or audio message that can be played in the app: one
    /// that was sent (and so is on the server) and not deleted.
    pub fn is_sound(&self) -> bool {
        self.state == SendState::Sent
            && self.kind != MsgKind::Redacted
            && self
                .file
                .as_ref()
                .is_some_and(|file| matches!(file.kind, MediaKind::Audio | MediaKind::Voice))
    }

    /// What saving this message would write to disk, if anything.
    pub fn attachment(&self) -> Option<Attachment> {
        if let Some(file) = &self.file {
            return Some(file.clone());
        }
        self.image.as_ref()?.attachment()
    }

    /// The code blocks of the message, in order (empty ones included: their index is
    /// what the display refers to). Only formatted messages have any: a plain body
    /// with backticks is shown as it was written.
    pub fn code_blocks(&self) -> Vec<crate::rich::CodeBlock> {
        const MARK: &[u8] = b"<pre";
        let Some(html) = self.html.as_deref() else {
            return Vec::new();
        };
        // Parsing is for messages that have one.
        if !html
            .as_bytes()
            .windows(MARK.len())
            .any(|window| window.eq_ignore_ascii_case(MARK))
        {
            return Vec::new();
        }
        crate::rich::from_html(html).code_blocks
    }

    pub fn has_code(&self) -> bool {
        self.code_blocks()
            .iter()
            .any(|block| !block.text.is_empty())
    }

    pub fn has_spoiler(&self) -> bool {
        const MARK: &[u8] = b"data-mx-spoiler";
        // Attribute names are case-insensitive, like in the parser.
        self.html.as_deref().is_some_and(|html| {
            html.as_bytes()
                .windows(MARK.len())
                .any(|window| window.eq_ignore_ascii_case(MARK))
        })
    }
}

/// A name that someone else in the room goes by too, told apart as Element does:
/// `name (@user:server)`. The ID is made safe for the terminal.
pub fn disambiguated(name: &str, user_id: &str) -> String {
    format!("{name} ({})", crate::text::sanitize_line(user_id))
}

/// `shown` without the ID [`disambiguated`] put after it for `user_id`, if it has it.
fn plain_name<'a>(shown: &'a str, user_id: &str) -> &'a str {
    told_apart(shown, user_id).unwrap_or(shown)
}

/// The name in `shown` if [`disambiguated`] told it apart for `user_id`.
fn told_apart<'a>(shown: &'a str, user_id: &str) -> Option<&'a str> {
    shown
        .strip_suffix(')')?
        .strip_suffix(user_id)?
        .strip_suffix(" (")
}

/// Tells apart people of one name among `messages` where the SDK does not: it knows
/// only the members it has loaded, those without a display name not among each other,
/// and not those who left. Two user IDs seen under one name (as senders, as whom a
/// line is about, as the writers of what is quoted or of a thread's latest reply, as
/// readers) are each named with their ID; and whoever the SDK found ambiguous on one
/// of their messages is named with the ID in the quotes, thread lines, readers and
/// system lines about them too, which the SDK leaves as they were made.
///
/// Only adds what is missing: what was told apart stays as it is, and the pass may
/// be made again on what it made. Costs a lookup per name given, and nothing more
/// where nobody is to be told apart.
pub fn mark_namesakes(messages: &mut [Message]) {
    mark_namesakes_among(messages, None);
}

/// Says whether someone other than an ID (the second) goes by a name (the first)
/// beyond the messages: among the room's members, as far as they are known.
pub type Elsewhere<'a> = &'a dyn Fn(&str, &str) -> bool;

/// [`mark_namesakes`], where `elsewhere` also says who else goes by a name.
pub fn mark_namesakes_among(messages: &mut [Message], elsewhere: Option<Elsewhere<'_>>) {
    let mut namesakes = Namesakes::default();
    for message in messages.iter() {
        namesakes.learn(message);
    }
    if elsewhere.is_none() && namesakes.nobody() {
        return;
    }
    for message in messages.iter_mut() {
        namesakes.mark_among(message, elsewhere);
    }
}

/// Who shares a name with someone else among the messages it was shown, for
/// [`mark_namesakes`]; what it knows can be put on other messages too (an archive's,
/// written one by one after it has seen them all).
#[derive(Default)]
pub struct Namesakes {
    /// Each name with the one ID seen under it, `None` once there is a second.
    seen: HashMap<String, Option<String>>,
    /// Those the SDK found ambiguous on one of their messages.
    flagged: std::collections::HashSet<String>,
}

/// Hand-written: names and IDs stay out of logs.
impl fmt::Debug for Namesakes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Namesakes")
            .field("seen", &self.seen.len())
            .field("flagged", &self.flagged.len())
            .finish_non_exhaustive()
    }
}

impl Namesakes {
    /// Takes in the people of `message`.
    pub fn learn(&mut self, message: &Message) {
        for (name, id) in message.people() {
            match self.seen.get_mut(name) {
                Some(only) => {
                    if only.as_deref() != Some(id) {
                        *only = None;
                    }
                }
                None => {
                    self.seen.insert(name.to_owned(), Some(id.to_owned()));
                }
            }
        }
        if message.sender_ambiguous
            && let Some(id) = &message.sender_id
            && !self.flagged.contains(id)
        {
            self.flagged.insert(id.clone());
        }
    }

    /// Takes in the room's members, each a name and a user ID, after the messages:
    /// those who go by a name seen among them, as only they can change what is told
    /// apart there. A ban line about someone who wrote nothing then names them with
    /// the ID where a member goes by the same name.
    pub fn learn_members<'a>(&mut self, members: impl IntoIterator<Item = (&'a str, &'a str)>) {
        for (name, id) in members {
            if let Some(only) = self.seen.get_mut(name)
                && only.as_deref() != Some(id)
            {
                *only = None;
            }
        }
    }

    /// Nobody seen is to be told apart.
    fn nobody(&self) -> bool {
        self.flagged.is_empty() && self.seen.values().all(Option::is_some)
    }

    /// Someone else goes by `name`.
    fn shared(&self, name: &str) -> bool {
        matches!(self.seen.get(name), Some(None))
    }

    /// Whether [`Namesakes::mark`] names anyone of `message` with the ID.
    pub fn touches(&self, message: &Message) -> bool {
        message
            .people()
            .any(|(name, id)| self.shared(name) || self.flagged.contains(id))
    }

    /// Names with their IDs the people of `message` who are to be told apart.
    pub fn mark(&self, message: &mut Message) {
        self.mark_among(message, None);
    }

    /// [`Namesakes::mark`], with `elsewhere` as for [`mark_namesakes_among`].
    pub fn mark_among(&self, message: &mut Message, elsewhere: Option<Elsewhere<'_>>) {
        let shared = |name: &str| self.shared(name);
        let elsewhere = |name: &str, id: &str| elsewhere.is_some_and(|other| other(name, id));
        let apart =
            |name: &str, id: &str| shared(name) || self.flagged.contains(id) || elsewhere(name, id);
        let tell = |shown: &mut String, id: &str| {
            if told_apart(shown.as_str(), id).is_none() && apart(shown.as_str(), id) {
                *shown = disambiguated(shown.as_str(), id);
            }
        };
        if let Some(id) = &message.sender_id
            && (shared(&message.sender) || elsewhere(&message.sender, id))
        {
            message.sender_ambiguous = true;
        }
        if let (Some((name, _)), Some(id)) = (&mut message.reply_to, &message.reply_sender) {
            tell(name, id);
        }
        if let Some(thread) = &mut message.thread
            && let (Some((name, _)), Some(id)) = (&mut thread.latest, &thread.latest_sender)
        {
            tell(name, id);
        }
        for reader in &mut message.read_by.readers {
            if let Some(id) = &reader.id
                && apart(&reader.name, id)
            {
                reader.ambiguous = true;
            }
        }
        // Whoever deleted it, unless the name shown is the user ID itself.
        if let Some(Deletion {
            by: DeletedBy::Other { user_id, name },
            ..
        }) = &mut message.deleted
            && name != user_id
        {
            tell(name, user_id);
        }
        // A line that begins with whom it is about ("bob joined").
        if let Some((name, id)) = &message.subject
            && apart(name, id)
        {
            let whole = disambiguated(name, id);
            if !message.body.starts_with(&whole)
                && let Some(rest) = message.body.strip_prefix(name.as_str())
                && let Some(rest) = rest.strip_prefix(' ')
            {
                message.body = format!("{whole} {rest}");
            }
        }
        // And one that ends with who did it ("bob was banned by carol").
        if message.kind == MsgKind::System
            && let Some(id) = &message.sender_id
            && message
                .subject
                .as_ref()
                .is_some_and(|(_, about)| about != id)
            && apart(&message.sender, id)
            && let Some(rest) = message.body.strip_suffix(message.sender.as_str())
            && rest.ends_with(" by ")
        {
            message.body = format!("{rest}{}", disambiguated(&message.sender, id));
        }
    }
}

/// `shown`, a name as it is shown (told apart by [`disambiguated`] or not), in at most
/// `columns` columns. Too wide, a told-apart name is shortened before its user ID,
/// which is what tells the two apart and stays whole; where not even one letter of the
/// name fits beside it, the ID alone is shown; where that does not fit either, the
/// whole is cut from its end, as any other name is.
pub fn fit_name(shown: &str, columns: usize) -> String {
    use crate::text::{truncate, width};
    if width(shown) <= columns {
        return shown.to_owned();
    }
    if let Some(at) = shown.rfind(" (@")
        && shown.ends_with(')')
    {
        let (name, id) = shown.split_at(at);
        let id_width = width(id);
        // A letter and "…" at least.
        if columns >= id_width + 2 {
            return truncate(name, columns - id_width) + id;
        }
        let bare = &id[2..id.len() - 1];
        if width(bare) <= columns {
            return bare.to_owned();
        }
    }
    truncate(shown, columns)
}

#[derive(Clone)]
pub struct Room {
    /// Stable identity (the Matrix room id); indices change as the list reorders.
    pub id: String,
    pub name: String,
    pub kind: RoomKind,
    /// Notification count shown as a badge.
    pub unread: u32,
    /// Has unread messages, even without a notification count.
    pub bold: bool,
    /// Marked as unread by hand (here or in another client), until it is opened,
    /// marked as read or written in. Looks like a room with unread messages.
    pub marked_unread: bool,
    pub mention: bool,
    pub online: bool,
    pub typing: Option<String>,
    pub messages: Vec<Message>,
    /// Unsent text, kept while another room is open.
    pub draft: String,
    /// `mxc://` address of the room's picture.
    pub avatar: Option<String>,
    /// The latest message in one line, for the room list.
    pub preview: Option<String>,
    /// The server's notification count as last seen, to notice when it rises.
    pub server_count: u64,
    /// Canonical room alias (e.g. `#rust:matrix.org`), if any.
    pub alias: Option<String>,
    /// True for direct chats (`m.direct`), even when moved to Favourites or Low priority.
    pub direct: bool,
    /// True when the room's notification mode is `Mute` (excluded from space unread counts).
    pub muted: bool,
    /// The room is encrypted, or it is not known yet that it is not (a link in it must
    /// not be given to the homeserver on a guess).
    pub encrypted: bool,
    /// Power levels and moderation rights for the current user in this room.
    pub power: RoomPower,
    /// Event IDs listed in `m.room.pinned_events` (server order).
    pub pinned_ids: Vec<String>,
}

/// Hand-written: which room, its name, its messages and the draft stay out of logs.
impl fmt::Debug for Room {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Room")
            .field("kind", &self.kind)
            .field("unread", &self.unread)
            .field("bold", &self.bold)
            .field("marked_unread", &self.marked_unread)
            .field("mention", &self.mention)
            .field("messages", &self.messages.len())
            .field("draft_bytes", &self.draft.len())
            .field("direct", &self.direct)
            .field("muted", &self.muted)
            .field("encrypted", &self.encrypted)
            .field("power", &self.power)
            .field("pinned", &self.pinned_ids.len())
            .finish_non_exhaustive()
    }
}

/// Effective moderation and state permissions derived from the room's power levels.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RoomPower {
    pub own_level: i64,
    pub own_is_creator: bool,
    pub can_redact_other: bool,
    pub can_ban: bool,
    pub can_unban: bool,
    pub can_kick: bool,
    pub can_pin: bool,
    pub can_change_roles: bool,
    /// May change the room's name (`m.room.name`).
    pub can_rename: bool,
    /// May change the room's topic (`m.room.topic`).
    pub can_set_topic: bool,
    pub users_default: i64,
    pub creators: Vec<String>,
    pub member_levels: HashMap<String, i64>,
}

impl RoomPower {
    pub fn is_creator(&self, user_id: &str) -> bool {
        self.creators.iter().any(|c| c == user_id)
    }

    pub fn user_level(&self, user_id: &str) -> i64 {
        self.member_levels
            .get(user_id)
            .copied()
            .unwrap_or(self.users_default)
    }

    /// Whether the current user strictly outranks `target_user_id`.
    pub fn outranks(&self, target_user_id: &str) -> bool {
        if self.is_creator(target_user_id) {
            return false;
        }
        if self.own_is_creator {
            return true;
        }
        self.own_level > self.user_level(target_user_id)
    }

    /// Whether the current user can ban `target_user_id` right now.
    pub fn can_ban_user(&self, own_user_id: Option<&str>, target_user_id: &str) -> bool {
        self.can_ban && own_user_id != Some(target_user_id) && self.outranks(target_user_id)
    }

    /// Whether the current user can remove `target_user_id` from the room right now.
    pub fn can_kick_user(&self, own_user_id: Option<&str>, target_user_id: &str) -> bool {
        self.can_kick && own_user_id != Some(target_user_id) && self.outranks(target_user_id)
    }

    /// Whether the current user can lift the ban on `target_user_id` right now.
    pub fn can_unban_user(&self, own_user_id: Option<&str>, target_user_id: &str) -> bool {
        self.can_unban && own_user_id != Some(target_user_id) && self.outranks(target_user_id)
    }
}

/// Counts and flags only: never puts user IDs in logs.
impl fmt::Debug for RoomPower {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoomPower")
            .field("own_level", &self.own_level)
            .field("own_is_creator", &self.own_is_creator)
            .field("can_redact_other", &self.can_redact_other)
            .field("can_ban", &self.can_ban)
            .field("can_unban", &self.can_unban)
            .field("can_kick", &self.can_kick)
            .field("can_pin", &self.can_pin)
            .field("can_change_roles", &self.can_change_roles)
            .field("can_rename", &self.can_rename)
            .field("can_set_topic", &self.can_set_topic)
            .field("users_default", &self.users_default)
            .field("creators", &self.creators.len())
            .field("member_levels", &self.member_levels.len())
            .finish()
    }
}

/// Summary of one pinned event for the Pinned Messages popup.
#[derive(Clone, PartialEq, Eq)]
pub struct PinnedMessage {
    pub event_id: String,
    pub sender: String,
    pub sender_id: Option<String>,
    pub day: String,
    pub time: String,
    pub snippet: String,
    pub redacted: bool,
    pub undecryptable: bool,
    pub ignored: bool,
}

/// Redacts event IDs, sender IDs, and message content.
impl fmt::Debug for PinnedMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "PinnedMessage({} chars, redacted: {}, undecryptable: {}, ignored: {})",
            self.snippet.len(),
            self.redacted,
            self.undecryptable,
            self.ignored
        )
    }
}

/// One thread of a room, as the threads popup lists it. All text is made safe for the
/// terminal before it gets here.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ThreadInfo {
    /// Event id of the message the thread starts at; in the demo `demo:` and its
    /// position in the room.
    pub root: String,
    /// Who started the thread, and the first line of what they wrote.
    pub sender: String,
    pub text: String,
    pub replies: u32,
    /// Who wrote the newest reply, and when, as far as known.
    pub latest_sender: Option<String>,
    pub latest_when: Option<String>,
    /// There are replies the user has not read.
    pub unread: bool,
    /// The user started the thread or answered in it, as far as known.
    pub mine: bool,
}

/// Counts and flags only: names, text and ids stay out of logs.
impl fmt::Debug for ThreadInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ThreadInfo({} replies, unread: {}, mine: {})",
            self.replies, self.unread, self.mine
        )
    }
}

pub const OWN_NAME: &str = "you";
/// Cache key of the picture in the demo data.
pub const DEMO_IMAGE: &str = "demo:palette";

fn msg(sender: &str, day: &str, time: &str, body: &str) -> Message {
    Message {
        sender: sender.into(),
        time: time.into(),
        day: day.into(),
        body: body.into(),
        own: sender == OWN_NAME,
        ..Message::default()
    }
}

impl Room {
    /// Whether this room is an unresolved invitation or outgoing join request.
    pub fn is_pending(&self) -> bool {
        self.kind.is_pending()
    }

    /// Whether the room has news that notifies: a number on its badge that counts.
    /// The window title adds these numbers up and the rooms bar shows these rooms.
    ///
    /// - A muted room does not count (its badge is not news either).
    /// - A room marked unread by hand, or bold without a number, has nothing to count.
    /// - Invitations and join requests are not messages; the room list shows them apart.
    pub fn notifies(&self) -> bool {
        self.unread > 0 && !self.muted && !self.is_pending()
    }

    /// Whether the room list shows the room as unread, for whatever reason.
    pub fn shows_unread(&self) -> bool {
        self.bold || self.unread > 0 || self.mention || self.marked_unread
    }

    pub fn new(id: &str, name: &str, kind: RoomKind) -> Room {
        Room {
            id: id.into(),
            ..room(name, kind, 0, Vec::new())
        }
    }
}

/// A demo room; its name doubles as its id.
fn room(name: &str, kind: RoomKind, unread: u32, messages: Vec<Message>) -> Room {
    Room {
        id: name.into(),
        name: name.into(),
        kind,
        unread,
        bold: unread > 0,
        marked_unread: false,
        mention: false,
        online: false,
        typing: None,
        messages,
        draft: String::new(),
        avatar: None,
        preview: None,
        server_count: 0,
        alias: None,
        direct: kind == RoomKind::Person,
        muted: false,
        encrypted: true,
        power: RoomPower::default(),
        pinned_ids: Vec::new(),
    }
}

pub fn demo_rooms() -> Vec<Room> {
    let mut rust = room(
        "rust",
        RoomKind::Favourite,
        0,
        vec![
            msg(
                "alice",
                "Yesterday",
                "21:40",
                "Has anyone tried the new event cache yet?",
            ),
            msg(
                "bob",
                "Yesterday",
                "21:44",
                "Yes — startup is instant now. Room previews come straight from disk.",
            ),
            msg(
                "bob",
                "Yesterday",
                "21:45",
                "Back-pagination also survives restarts, which is a big deal for large rooms like this one where scrolling up used to refetch everything from the server every single time.",
            ),
            msg(
                "carol",
                "Today",
                "09:12",
                "Morning! Is the sliding sync proxy still needed?",
            ),
            msg(
                "alice",
                "Today",
                "09:15",
                "No, it is native in the homeserver now.",
            ),
            msg(OWN_NAME, "Today", "09:20", "Good to know, thanks."),
            msg(
                "dave",
                "Today",
                "10:02",
                "Shipping a TUI client on top of it this week 🐀",
            ),
        ],
    );
    rust.messages[1].reactions = vec![
        Reaction {
            key: "👍".into(),
            count: 2,
            own: false,
            failed: false,
        },
        Reaction {
            key: "🎉".into(),
            count: 1,
            own: true,
            failed: false,
        },
    ];
    rust.messages[4].reply_to = Some((
        "carol".into(),
        "Morning! Is the sliding sync proxy still needed?".into(),
    ));
    rust.messages[5].edited = true;
    rust.typing = Some("carol".into());

    let mut bob = room(
        "bob",
        RoomKind::Person,
        2,
        vec![
            msg(OWN_NAME, "Today", "08:01", "Lunch at 12?"),
            msg("bob", "Today", "08:30", "Sure. The usual place?"),
            msg(
                "bob",
                "Today",
                "08:31",
                "I'll bring the laptop so we can look at the theme files.",
            ),
        ],
    );
    bob.online = true;

    let mut design = room(
        "design",
        RoomKind::Room,
        5,
        vec![
            msg("erin", "Today", "07:55", "New palette is up for review."),
            msg(
                "erin",
                "Today",
                "07:55",
                "🖼  palette.png\nWarm to cool, light at the top.",
            ),
            msg(
                "erin",
                "Today",
                "07:56",
                "@you can you check the contrast of the unread badge?",
            ),
        ],
    );
    design.mention = true;
    design.messages[1].image = Some(ImageInfo {
        key: DEMO_IMAGE.into(),
        source: ImageSource::Demo,
        name: "palette.png".into(),
        caption: Some("Warm to cool, light at the top.".into()),
        dimensions: Some((480, 270)),
        size: None,
        sticker: false,
    });

    let mut welcome = room(
        "matrix-rat",
        RoomKind::Favourite,
        0,
        vec![msg(
            "alice",
            "Today",
            "10:30",
            "Welcome to the demo. Click a room, type, press Enter.",
        )],
    );
    welcome.messages[0].thread = ThreadSummary::of(vec![
        msg("bob", "Today", "10:32", "Threads work too."),
        msg(
            "carol",
            "Today",
            "10:35",
            "Press t on a message to open its thread, or click the line under it.",
        ),
    ]);

    vec![
        rust,
        welcome,
        bob,
        room(
            "carol",
            RoomKind::Person,
            0,
            vec![msg("carol", "Yesterday", "18:03", "See you tomorrow!")],
        ),
        design,
        room(
            "nixos",
            RoomKind::Room,
            0,
            vec![msg(
                "frank",
                "Today",
                "06:10",
                "Unstable channel bumped the kernel again.",
            )],
        ),
        room(
            "off-topic",
            RoomKind::Room,
            12,
            vec![msg(
                "grace",
                "Today",
                "10:11",
                "Anyone else watching the launch tonight?",
            )],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Namesakes the SDK did not find (loaded senders without display names, someone
    /// who left) are told apart among what is loaded: senders, whom a line is about,
    /// who is quoted, who replied last in a thread, readers. Whoever the SDK found
    /// ambiguous is told apart there too. Once is enough: a second pass changes
    /// nothing.
    #[test]
    fn namesakes_among_the_loaded_messages_are_told_apart() {
        let said = |id: &str, name: &str, user: &str| Message {
            id: id.into(),
            sender: name.into(),
            sender_id: Some(user.into()),
            body: format!("said {id}"),
            ..Message::default()
        };
        let mut quoting = said("q", "carol", "@carol:example.org");
        quoting.reply_to = Some(("bob".into(), "what bob said".into()));
        quoting.reply_sender = Some("@bob:elsewhere.org".into());
        quoting.thread = Some(ThreadSummary {
            replies: 1,
            latest: Some(("bob".into(), "a reply".into())),
            latest_sender: Some("@bob:example.org".into()),
            ..ThreadSummary::default()
        });
        quoting.read_by = ReadBy {
            readers: vec![Reader {
                name: "bob".into(),
                id: Some("@bob:example.org".into()),
                ..Reader::default()
            }],
            total: 1,
        };
        let mut banned = said("x", "carol", "@carol:example.org");
        banned.kind = MsgKind::System;
        banned.body = "bob was banned by carol".into();
        banned.subject = Some(("bob".into(), "@bob:third.org".into()));
        // Carol is a namesake too, as the SDK found: her quotes say so.
        let mut dave = said("d", "dave", "@dave:example.org");
        dave.reply_to = Some(("carol".into(), "what carol said".into()));
        dave.reply_sender = Some("@carol:example.org".into());
        let mut other_carol = said("c", "carol", "@carol:example.org");
        other_carol.sender_ambiguous = true;
        let mut messages = vec![
            said("a", "bob", "@bob:example.org"),
            said("b", "bob", "@bob:elsewhere.org"),
            quoting,
            banned,
            dave,
            other_carol,
        ];
        mark_namesakes(&mut messages);
        let names: Vec<String> = messages.iter().map(|m| m.sender_name().into()).collect();
        assert_eq!(
            names,
            [
                "bob (@bob:example.org)",
                "bob (@bob:elsewhere.org)",
                "carol",
                "carol",
                "dave",
                "carol (@carol:example.org)"
            ]
        );
        let quoting = &messages[2];
        assert_eq!(
            quoting.reply_to.as_ref().unwrap().0,
            "bob (@bob:elsewhere.org)"
        );
        let latest = &quoting.thread.as_ref().unwrap().latest;
        assert_eq!(latest.as_ref().unwrap().0, "bob (@bob:example.org)");
        assert_eq!(
            quoting.read_by.readers[0].shown_name(),
            "bob (@bob:example.org)"
        );
        assert_eq!(
            messages[3].body,
            "bob (@bob:third.org) was banned by carol (@carol:example.org)"
        );
        assert_eq!(
            messages[4].reply_to.as_ref().unwrap().0,
            "carol (@carol:example.org)"
        );
        let once = messages.clone();
        mark_namesakes(&mut messages);
        for (again, before) in messages.iter().zip(&once) {
            assert_eq!(again.body, before.body);
            assert_eq!(again.reply_to, before.reply_to);
            assert_eq!(again.sender_name(), before.sender_name());
        }

        // Nobody shares a name: nothing changes.
        let mut alone = vec![
            said("a", "bob", "@bob:example.org"),
            said("e", "eve", "@eve:x.org"),
        ];
        mark_namesakes(&mut alone);
        assert!(alone.iter().all(|m| !m.sender_ambiguous));
    }

    /// A moderator who deleted a message and shares a name with a sender in view is
    /// named with their ID on the deleted line, and the sender too; a second pass
    /// changes nothing. Alone with their name, they keep it as it is.
    #[test]
    fn a_moderator_who_shares_a_name_is_told_apart() {
        let said = |id: &str, name: &str, user: &str| Message {
            id: id.into(),
            sender: name.into(),
            sender_id: Some(user.into()),
            body: format!("said {id}"),
            ..Message::default()
        };
        let deleted_by = |name: &str, user: &str| {
            let mut message = said("x", "bob", "@bob:example.org");
            message.kind = MsgKind::Redacted;
            message.body = "Message deleted".into();
            message.deleted = Some(Deletion {
                reason: Some("spam".into()),
                by: DeletedBy::Other {
                    user_id: user.into(),
                    name: name.into(),
                },
            });
            message
        };
        let mut messages = vec![
            said("a", "carol", "@carol:elsewhere.org"),
            deleted_by("carol", "@carol:example.org"),
        ];
        mark_namesakes(&mut messages);
        assert_eq!(messages[0].sender_name(), "carol (@carol:elsewhere.org)");
        assert_eq!(
            messages[1].deleted_line(),
            "Deleted by carol (@carol:example.org): spam"
        );
        let once = messages.clone();
        mark_namesakes(&mut messages);
        assert_eq!(messages[1].deleted, once[1].deleted);

        let mut alone = vec![
            said("a", "dave", "@dave:example.org"),
            deleted_by("carol", "@carol:example.org"),
        ];
        mark_namesakes(&mut alone);
        assert_eq!(alone[1].deleted_line(), "Deleted by carol: spam");
        // A name that is the user ID is not told apart again.
        let mut bare = [deleted_by("@carol:example.org", "@carol:example.org")];
        let mut namesakes = Namesakes::default();
        namesakes.learn(&said("a", "@carol:example.org", "@mallory:example.org"));
        namesakes.learn(&bare[0]);
        namesakes.mark(&mut bare[0]);
        assert_eq!(
            bare[0].deleted_line(),
            "Deleted by @carol:example.org: spam"
        );
    }

    /// What a deleted message shows, and that its Debug says nothing of who or why.
    #[test]
    fn a_deleted_line_names_who_and_why_and_debug_neither() {
        let line = |reason: Option<&str>, by: DeletedBy| {
            Deletion {
                reason: reason.map(str::to_owned),
                by,
            }
            .line()
        };
        assert_eq!(
            line(Some("sent to the wrong room"), DeletedBy::Sender),
            "Message deleted: sent to the wrong room"
        );
        assert_eq!(line(None, DeletedBy::Sender), "Message deleted");
        assert_eq!(line(Some("oops"), DeletedBy::Us), "Deleted by you: oops");
        let carol = || DeletedBy::Other {
            user_id: "@carol:example.org".into(),
            name: "carol".into(),
        };
        assert_eq!(
            line(Some("spam link"), carol()),
            "Deleted by carol: spam link"
        );
        assert_eq!(line(None, carol()), "Deleted by carol");
        // Demo data without a deletion shows its body.
        let demo = Message {
            kind: MsgKind::Redacted,
            body: "Message deleted".into(),
            ..Message::default()
        };
        assert_eq!(demo.deleted_line(), "Message deleted");

        let message = Message {
            kind: MsgKind::Redacted,
            deleted: Some(Deletion {
                reason: Some("secret reason".into()),
                by: carol(),
            }),
            ..Message::default()
        };
        let debug = format!("{message:?}");
        for secret in ["secret", "carol", "example.org"] {
            assert!(!debug.contains(secret), "{secret} in {debug}");
        }
        assert!(
            debug.contains("Deletion { reason: true, by: Other }"),
            "{debug}"
        );
    }

    #[test]
    fn a_name_told_apart_keeps_its_id_when_it_is_shortened() {
        let shown = disambiguated("Alexander Hamilton", "@alexander:example.org");
        // Wide enough: as it is.
        assert_eq!(fit_name(&shown, 60), shown);
        // The name gives way, down to a letter.
        assert_eq!(fit_name(&shown, 30), "Alex… (@alexander:example.org)");
        assert_eq!(fit_name(&shown, 27), "A… (@alexander:example.org)");
        // Then the ID alone, then the whole cut from its end.
        assert_eq!(fit_name(&shown, 26), "@alexander:example.org");
        assert_eq!(fit_name(&shown, 22), "@alexander:example.org");
        assert_eq!(fit_name(&shown, 12), "Alexander H…");
        // A name not told apart is cut as before.
        assert_eq!(fit_name("Alexander Hamilton", 10), "Alexander…");
        for columns in 0..50 {
            assert!(crate::text::width(&fit_name(&shown, columns)) <= columns);
        }
    }

    #[test]
    fn a_message_and_its_room_print_nothing_private() {
        let message = Message {
            id: "$event:example.org".into(),
            item: Some(ItemRef::Event("$event:example.org".into())),
            sender: "Alice".into(),
            sender_id: Some("@alice:example.org".into()),
            body: "the secret plan".into(),
            reply_to: Some(("Bob".into(), "earlier secret".into())),
            reactions: vec![Reaction {
                key: "secret".into(),
                count: 1,
                own: false,
                failed: false,
            }],
            thread: Some(ThreadSummary {
                replies: 2,
                latest: Some(("Bob".into(), "a secret reply".into())),
                ..ThreadSummary::default()
            }),
            ..Message::default()
        };
        let room = Room {
            id: "!room:example.org".into(),
            name: "Secret room".into(),
            kind: RoomKind::Room,
            unread: 0,
            bold: false,
            marked_unread: false,
            mention: false,
            online: false,
            typing: Some("Bob".into()),
            messages: vec![message.clone()],
            draft: "a secret draft".into(),
            avatar: None,
            preview: Some("Alice: the secret plan".into()),
            server_count: 0,
            alias: Some("#secret:example.org".into()),
            direct: false,
            muted: false,
            encrypted: true,
            power: RoomPower::default(),
            pinned_ids: vec!["$event:example.org".into()],
        };
        for text in [format!("{message:?}"), format!("{room:?}")] {
            for private in ["example.org", "secret", "Secret", "Alice", "Bob"] {
                assert!(!text.contains(private), "{private} in {text}");
            }
        }
    }

    #[test]
    fn poll_debug_is_redacted_and_helpers_behave() {
        let mut poll = PollInfo {
            question: "Secret lunch question?".into(),
            disclosed: false,
            max_selections: 2,
            answers: vec![
                PollAnswer {
                    id: "ans-1".into(),
                    text: "Private pizza".into(),
                    votes: 3,
                    own: true,
                },
                PollAnswer {
                    id: "ans-2".into(),
                    text: "Secret sushi".into(),
                    votes: 3,
                    own: false,
                },
                PollAnswer {
                    id: "ans-3".into(),
                    text: "Salad".into(),
                    votes: 1,
                    own: false,
                },
            ],
            total_voters: 6,
            ended: false,
            editable: true,
        };
        let dbg = format!("{poll:?}");
        assert!(!dbg.contains("Secret"));
        assert!(!dbg.contains("Private"));
        assert!(!dbg.contains("ans-1"));
        assert!(!dbg.contains('6'));
        let adbg = format!("{:?}", poll.answers[0]);
        assert!(!adbg.contains("Private"));
        assert!(!adbg.contains("ans-1"));
        assert!(!adbg.contains('3'));

        assert!(!poll.shows_counts());
        assert!(poll.is_multi());
        assert_eq!(poll.effective_max(), 2);
        assert_eq!(poll.winner_max(), 0);
        assert_eq!(poll.toggled_selection(1).unwrap(), vec!["ans-1", "ans-2"]);

        poll.answers[1].own = true;
        assert_eq!(poll.toggled_selection(2), Err(2));
        assert_eq!(poll.toggled_selection(0).unwrap(), vec!["ans-2"]);

        poll.ended = true;
        assert!(poll.shows_counts());
        assert_eq!(poll.winner_max(), 3);

        let single = PollInfo {
            question: "Yes or no?".into(),
            disclosed: true,
            max_selections: 1,
            answers: vec![
                PollAnswer {
                    id: "a1".into(),
                    text: "Yes".into(),
                    votes: 1,
                    own: true,
                },
                PollAnswer {
                    id: "a2".into(),
                    text: "No".into(),
                    votes: 0,
                    own: false,
                },
            ],
            total_voters: 1,
            ended: false,
            editable: false,
        };
        assert!(!single.is_multi());
        assert_eq!(single.toggled_selection(0).unwrap(), Vec::<String>::new());
        assert_eq!(single.toggled_selection(1).unwrap(), vec!["a2"]);

        let own_editable = Message {
            own: true,
            poll: Some(PollInfo {
                ended: false,
                editable: true,
                ..single.clone()
            }),
            ..Message::default()
        };
        assert!(own_editable.editable());
        let voted = Message {
            own: true,
            poll: Some(single),
            ..Message::default()
        };
        assert!(!voted.editable());
    }
}
