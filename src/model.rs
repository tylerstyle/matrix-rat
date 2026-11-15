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
            _ => "Accept the invitation first",
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    pub key: String,
    pub count: u32,
    pub own: bool,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemRef {
    Event(String),
    /// A message of ours that the server has not confirmed yet.
    Transaction(String),
}

#[derive(Debug, Clone, Default)]
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
    /// The formatted (HTML) version of the body, when the sender provided one.
    pub html: Option<String>,
    /// Sender and text of the message this one replies to.
    pub reply_to: Option<(String, String)>,
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
    /// A doubt about who really sent this (encrypted rooms only).
    pub shield: Option<Shield>,
    /// Mentions (`m.mentions`) carried by the message, used to preserve them on edit.
    pub mentions: Option<matrix_sdk::ruma::events::Mentions>,
    /// Who has read up to this message (and no further), as far as they tell.
    pub read_by: ReadBy,
    /// Why the message could not be decrypted (`kind` is `Undecryptable`). `None`
    /// for demo data.
    pub utd: Option<UtdWhy>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reader {
    /// Display name, safe to show.
    pub name: String,
    /// When they read it ("14:05", or with the day when it was not today); empty if
    /// the receipt does not say.
    pub time: String,
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
#[derive(Debug, Clone, Default)]
pub struct ThreadSummary {
    pub replies: u32,
    /// Sender and start of the newest reply, when known.
    pub latest: Option<(String, String)>,
    /// There are replies the user has not read.
    pub unread: bool,
    /// The replies themselves, for the demo only; with an account they are loaded
    /// when the thread is opened.
    pub demo_replies: Vec<Message>,
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
            latest: Some((latest.sender.clone(), latest.body.clone())),
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
}

/// Hand-written: an encrypted source carries the key of the file.
impl std::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Attachment({})", self.name)
    }
}

/// A picture attached to a message.
#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
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
        },
        Reaction {
            key: "🎉".into(),
            count: 1,
            own: true,
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
