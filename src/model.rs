//! What the UI shows: rooms and messages. Filled with demo data until the Matrix
//! worker arrives (milestone M2).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomKind {
    Invite,
    Favourite,
    Person,
    Room,
}

impl RoomKind {
    pub const SECTIONS: [RoomKind; 4] = [
        RoomKind::Invite,
        RoomKind::Favourite,
        RoomKind::Person,
        RoomKind::Room,
    ];

    pub fn title(self) -> &'static str {
        match self {
            RoomKind::Invite => "Invites",
            RoomKind::Favourite => "Favourites",
            RoomKind::Person => "People",
            RoomKind::Room => "Rooms",
        }
    }
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
    pub time: String,
    pub day: String,
    pub body: String,
    pub own: bool,
    pub edited: bool,
    /// The formatted (HTML) version of the body, when the sender provided one.
    pub html: Option<String>,
    /// Sender and text of the message this one replies to.
    pub reply_to: Option<(String, String)>,
    pub reactions: Vec<Reaction>,
    /// Set for picture messages; `body` then holds the text shown when pictures are off.
    pub image: Option<ImageInfo>,
    /// Set for file, audio and video messages.
    pub file: Option<Attachment>,
    /// Set for a message that started a thread.
    pub thread: Option<ThreadSummary>,
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

/// The one line under a message that started a thread.
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

/// Something that can be saved to disk: a file, or the original of a picture.
#[derive(Clone)]
pub struct Attachment {
    /// The file's `mxc://` address.
    pub key: String,
    /// File name as the sender gave it, safe to show (not yet safe as a path).
    pub name: String,
    /// Size in bytes, as stated by the sender.
    pub size: Option<u64>,
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
                source: Box::new(media.full.clone()),
            }),
        }
    }
}

impl Message {
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

    /// Whether the user can change this message: their own text or `/me` action, not
    /// a file, a picture or something deleted.
    pub fn editable(&self) -> bool {
        self.own
            && self.image.is_none()
            && self.file.is_none()
            && matches!(self.kind, MsgKind::Text | MsgKind::Emote)
    }

    /// What saving this message would write to disk, if anything.
    pub fn attachment(&self) -> Option<Attachment> {
        if let Some(file) = &self.file {
            return Some(file.clone());
        }
        self.image.as_ref()?.attachment()
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
