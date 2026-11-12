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

#[derive(Debug, Clone, Default)]
pub struct Message {
    pub sender: String,
    pub time: String,
    pub day: String,
    pub body: String,
    pub own: bool,
    pub edited: bool,
    /// Sender and text of the message this one replies to.
    pub reply_to: Option<(String, String)>,
    pub reactions: Vec<Reaction>,
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
    pub mention: bool,
    pub online: bool,
    pub typing: Option<String>,
    pub messages: Vec<Message>,
    /// Unsent text, kept while another room is open.
    pub draft: String,
}

pub const OWN_NAME: &str = "you";

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
        mention: false,
        online: false,
        typing: None,
        messages,
        draft: String::new(),
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
                "07:56",
                "@you can you check the contrast of the unread badge?",
            ),
        ],
    );
    design.mention = true;

    vec![
        rust,
        room(
            "matrix-rat",
            RoomKind::Favourite,
            0,
            vec![msg(
                "alice",
                "Today",
                "10:30",
                "Welcome to the demo. Click a room, type, press Enter.",
            )],
        ),
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
