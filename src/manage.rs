//! Room management on the UI side: the addresses people type or paste, and the state of
//! the two popups ("New chat or room" and "Room info").

use matrix_sdk::ruma::matrix_uri::MatrixId;
use matrix_sdk::ruma::{MatrixToUri, MatrixUri, RoomAliasId, RoomId, ServerName, UserId};

use crate::matrix::{MemberRole, NotifyLevel, RoomDetails, RoomMember};
use crate::model::RoomKind;

// ----- addresses ---------------------------------------------------------------------

/// What a typed or pasted address points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    Room {
        /// `#alias:server` or `!id:server`.
        target: String,
        /// Servers that know the room, for joining by id.
        via: Vec<String>,
    },
    User(String),
}

const NOT_AN_ADDRESS: &str =
    "That is not a room address. Use #room:server, !id:server or a matrix.to link.";

/// Understands `#alias:server`, `!id:server` (optionally followed by `?via=server` or
/// by server names), `@user:server`, and `matrix.to` / `matrix:` links pasted as they
/// are. The error is a sentence for the user.
pub fn parse_address(input: &str) -> Result<Address, String> {
    let input = input.trim().trim_start_matches('<').trim_end_matches('>');
    if input.is_empty() {
        return Err("Enter an address first.".to_owned());
    }
    if input.chars().any(char::is_control) {
        return Err(NOT_AN_ADDRESS.to_owned());
    }
    let mut words = input
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|w| !w.is_empty());
    let first = words.next().unwrap_or_default();
    // Server names after the address: where to ask for the room.
    let mut via: Vec<String> = Vec::new();
    for word in words {
        let name = word.strip_prefix("via=").unwrap_or(word);
        match ServerName::parse(name) {
            Ok(server) => via.push(server.to_string()),
            Err(_) => return Err(format!("“{name}” is not a server name.")),
        }
    }

    let lower = first.to_ascii_lowercase();
    let from_link = |id: &MatrixId, link_via: Vec<String>| match id {
        MatrixId::Room(id) => Ok((id.to_string(), link_via, false)),
        MatrixId::RoomAlias(alias) => Ok((alias.to_string(), link_via, false)),
        // A link to a message: the room it is in.
        MatrixId::Event(room, _) => Ok((room.to_string(), link_via, false)),
        MatrixId::User(user) => Ok((user.to_string(), Vec::new(), true)),
        _ => Err(NOT_AN_ADDRESS.to_owned()),
    };
    let names = |servers: &[matrix_sdk::ruma::OwnedServerName]| {
        servers.iter().map(ToString::to_string).collect::<Vec<_>>()
    };
    let (target, link_via, user) =
        if lower.starts_with("https://matrix.to/") || lower.starts_with("http://matrix.to/") {
            let link = MatrixToUri::parse(first)
                .map_err(|_| "That matrix.to link could not be read.".to_owned())?;
            from_link(link.id(), names(link.via()))?
        } else if lower.starts_with("matrix:") {
            let link = MatrixUri::parse(first)
                .map_err(|_| "That matrix: link could not be read.".to_owned())?;
            from_link(link.id(), names(link.via()))?
        } else if first.starts_with('@') {
            let user = UserId::parse(first).map_err(|_| NOT_A_USER.to_owned())?;
            (user.to_string(), Vec::new(), true)
        } else if first.starts_with('#') {
            let alias = RoomAliasId::parse(first).map_err(|_| NOT_AN_ADDRESS.to_owned())?;
            (alias.to_string(), Vec::new(), false)
        } else if first.starts_with('!') {
            let (id, query) = first.split_once('?').unwrap_or((first, ""));
            let id = RoomId::parse(id).map_err(|_| NOT_AN_ADDRESS.to_owned())?;
            let mut servers = Vec::new();
            for pair in query.split('&').filter(|p| !p.is_empty()) {
                if let Some(name) = pair.strip_prefix("via=") {
                    let server = ServerName::parse(name)
                        .map_err(|_| format!("“{name}” is not a server name."))?;
                    servers.push(server.to_string());
                }
            }
            (id.to_string(), servers, false)
        } else {
            return Err(NOT_AN_ADDRESS.to_owned());
        };
    if user {
        return Ok(Address::User(target));
    }
    for server in link_via {
        if !via.contains(&server) {
            via.push(server);
        }
    }
    Ok(Address::Room { target, via })
}

const NOT_A_USER: &str = "A user ID looks like @name:server.";

/// A complete user id, from what was typed (`@name:server`, `name:server`, or a link
/// to a user).
pub fn parse_user(input: &str) -> Result<String, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Enter a user ID first.".to_owned());
    }
    let lower = input.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("matrix:")
    {
        return match parse_address(input) {
            Ok(Address::User(user)) => Ok(user),
            _ => Err(NOT_A_USER.to_owned()),
        };
    }
    if input.starts_with(['#', '!', '$', '+']) {
        return Err(NOT_A_USER.to_owned());
    }
    let with_sigil = if input.starts_with('@') {
        input.to_owned()
    } else {
        format!("@{input}")
    };
    if with_sigil
        .chars()
        .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(NOT_A_USER.to_owned());
    }
    UserId::parse(&with_sigil)
        .map(|user| user.to_string())
        .map_err(|_| NOT_A_USER.to_owned())
}

/// The part of a new room's address before the colon, from what was typed.
pub fn parse_alias_local(input: &str) -> Result<String, String> {
    let local = input.trim().trim_start_matches('#');
    if local.is_empty() {
        return Err("A public room needs an address.".to_owned());
    }
    if local.contains(':') {
        return Err("Only the part before the colon; your server adds its own name.".to_owned());
    }
    if local
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '#')
    {
        return Err("An address cannot contain spaces.".to_owned());
    }
    // The same rules the complete address has to pass.
    RoomAliasId::parse(format!("#{local}:example.org"))
        .map(|_| local.to_owned())
        .map_err(|_| "That address is not valid.".to_owned())
}

// ----- "New chat or room" ------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Pick one of the three below.
    Choose,
    Join,
    Direct,
    Create,
    /// Invite someone to the room the popup was opened for.
    Invite,
}

impl Stage {
    pub const CHOICES: [Stage; 3] = [Stage::Join, Stage::Direct, Stage::Create];

    pub fn title(self) -> &'static str {
        match self {
            Stage::Choose => "New chat or room",
            Stage::Join => "Join a room",
            Stage::Direct => "Start a direct chat",
            Stage::Create => "Create a room",
            Stage::Invite => "Invite to this room",
        }
    }

    /// What the chooser says under the title.
    pub fn summary(self) -> &'static str {
        match self {
            Stage::Join => "By its address or a matrix.to link",
            Stage::Direct => "With one person, by user ID",
            Stage::Create => "Private or public, with a name of your choice",
            Stage::Choose | Stage::Invite => "",
        }
    }
}

/// Something in the room popups that can be clicked. Keys do the same through these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Click {
    /// Show this stage of the "new" popup, opening it if need be.
    Stage(Stage),
    /// Put the cursor into this slot of the form.
    Slot(usize),
    /// Flip the switch in this slot of the form.
    Flip(usize),
    /// The form's button.
    Submit,
    /// A row of the room info view: select it and do what it does.
    Row(usize),
    /// A row of the room info view: change its value by this step.
    Step(usize, i8),
    /// Select a row of the room info view without doing anything.
    Select(usize),
    /// Direct chat with the selected member.
    MemberChat,
    /// Remove the selected member from the room (asks first).
    MemberRemove,
    /// Favourite on or off for the room in question, without a popup.
    Favourite,
}

/// The form behind Ctrl-N. One popup for all stages, so going back keeps what was typed.
#[derive(Clone)]
pub struct NewPopup {
    pub stage: Stage,
    /// Cursor in the chooser.
    pub choice: usize,
    /// Opened at a form directly: Esc closes instead of going back to the chooser.
    pub single: bool,
    pub address: String,
    pub user: String,
    pub name: String,
    pub topic: String,
    pub public: bool,
    pub alias: String,
    pub encrypted: bool,
    /// The slot with the cursor (see `Self::slots`).
    pub focus: usize,
    /// What is being waited for; input is locked meanwhile.
    pub busy: Option<&'static str>,
    pub error: Option<String>,
    /// Id and name of the room to invite to.
    pub room: Option<(String, String)>,
}

/// Hand-written: what is typed here (names, topics, who is being written to) is private.
impl std::fmt::Debug for NewPopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewPopup")
            .field("stage", &self.stage)
            .field("busy", &self.busy)
            .finish_non_exhaustive()
    }
}

impl NewPopup {
    pub const NAME: usize = 0;
    pub const TOPIC: usize = 1;
    pub const ACCESS: usize = 2;
    /// The address of a public room, or the encryption switch of a private one.
    pub const EXTRA: usize = 3;
    pub const BUTTON: usize = 4;

    pub fn new(stage: Stage) -> Self {
        Self {
            stage,
            choice: 0,
            single: stage != Stage::Choose,
            address: String::new(),
            user: String::new(),
            name: String::new(),
            topic: String::new(),
            public: false,
            alias: String::new(),
            encrypted: true,
            focus: 0,
            busy: None,
            error: None,
            room: None,
        }
    }

    pub fn slots(&self) -> usize {
        match self.stage {
            Stage::Choose => Stage::CHOICES.len(),
            Stage::Create => Self::BUTTON + 1,
            Stage::Join | Stage::Direct | Stage::Invite => 1,
        }
    }

    /// The text being typed into, if the cursor is in a text field.
    pub fn field_mut(&mut self) -> Option<&mut String> {
        match (self.stage, self.focus) {
            (Stage::Join, _) => Some(&mut self.address),
            (Stage::Direct | Stage::Invite, _) => Some(&mut self.user),
            (Stage::Create, Self::NAME) => Some(&mut self.name),
            (Stage::Create, Self::TOPIC) => Some(&mut self.topic),
            (Stage::Create, Self::EXTRA) if self.public => Some(&mut self.alias),
            _ => None,
        }
    }

    /// Flips the switch in `slot`; false if there is none.
    pub fn flip(&mut self, slot: usize) -> bool {
        match (self.stage, slot) {
            (Stage::Create, Self::ACCESS) => self.public = !self.public,
            (Stage::Create, Self::EXTRA) if !self.public => self.encrypted = !self.encrypted,
            _ => return false,
        }
        true
    }

    pub fn step(&mut self, delta: isize) {
        let count = self.slots() as isize;
        if self.stage == Stage::Choose {
            self.choice = (self.choice as isize + delta).rem_euclid(count) as usize;
        } else {
            self.focus = (self.focus as isize + delta).rem_euclid(count) as usize;
        }
    }

    pub fn go(&mut self, stage: Stage) {
        self.stage = stage;
        self.focus = 0;
        self.error = None;
    }
}

// ----- "Room info" -------------------------------------------------------------------

/// A row of the room info view that can be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoRow {
    Favourite,
    Notify,
    Name,
    Topic,
    Invite,
    Leave,
    /// Index into the details' member list.
    Member(usize),
}

/// What the user was asked to confirm by doing it a second time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    Leave,
    /// Remove the member with this user id.
    Remove(String),
}

#[derive(Clone)]
pub struct InfoPopup {
    pub room_id: String,
    /// Name and kind as the room list has them, until the details arrive.
    pub name: String,
    pub kind: RoomKind,
    pub details: Option<RoomDetails>,
    pub error: Option<String>,
    /// Narrows the member list.
    pub filter: String,
    /// Index into `Self::rows`.
    pub selected: usize,
    /// First line shown, and how many fit (as of the last draw).
    pub scroll: usize,
    pub page: usize,
    /// Ask the next draw to bring the selected row into view.
    pub reveal: bool,
    /// The name or topic while it is being typed.
    pub editing: Option<(InfoRow, String)>,
    pub confirm: Option<Confirm>,
    /// A change is on its way to the server.
    pub busy: Option<&'static str>,
}

/// Hand-written: names and topics stay out of logs.
impl std::fmt::Debug for InfoPopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InfoPopup")
            .field("room_id", &self.room_id)
            .field("busy", &self.busy)
            .finish_non_exhaustive()
    }
}

impl InfoPopup {
    pub fn new(room_id: &str, name: &str, kind: RoomKind) -> Self {
        Self {
            room_id: room_id.to_owned(),
            name: name.to_owned(),
            kind,
            details: None,
            error: None,
            filter: String::new(),
            selected: 0,
            scroll: 0,
            page: 10,
            reveal: true,
            editing: None,
            confirm: None,
            busy: None,
        }
    }

    pub fn invited(&self) -> bool {
        self.kind == RoomKind::Invite
    }

    pub fn direct(&self) -> bool {
        self.details
            .as_ref()
            .map_or(self.kind == RoomKind::Person, |details| details.direct)
    }

    /// The selectable rows, top to bottom: what can be done, then the members that
    /// pass the filter.
    pub fn rows(&self) -> Vec<InfoRow> {
        let mut rows = Vec::new();
        if !self.invited() {
            rows.push(InfoRow::Favourite);
            if let Some(details) = &self.details {
                if details.notify.is_some() {
                    rows.push(InfoRow::Notify);
                }
                if details.can_rename {
                    rows.push(InfoRow::Name);
                }
                if details.can_set_topic {
                    rows.push(InfoRow::Topic);
                }
                if details.can_invite {
                    rows.push(InfoRow::Invite);
                }
            }
        }
        rows.push(InfoRow::Leave);
        if let Some(details) = &self.details {
            let needle = self.filter.to_lowercase();
            rows.extend(
                details
                    .members
                    .iter()
                    .enumerate()
                    .filter(|(_, member)| {
                        needle.is_empty()
                            || member.name.to_lowercase().contains(&needle)
                            || member.user_id.to_lowercase().contains(&needle)
                    })
                    .map(|(index, _)| InfoRow::Member(index)),
            );
        }
        rows
    }

    pub fn current(&self) -> Option<InfoRow> {
        let rows = self.rows();
        rows.get(self.selected.min(rows.len().saturating_sub(1)))
            .copied()
    }

    pub fn member(&self, row: InfoRow) -> Option<&RoomMember> {
        match row {
            InfoRow::Member(index) => self.details.as_ref()?.members.get(index),
            _ => None,
        }
    }

    pub fn selected_member(&self) -> Option<&RoomMember> {
        self.member(self.current()?)
    }

    /// Selects `row` if it is shown.
    pub fn select(&mut self, row: InfoRow) {
        if let Some(at) = self.rows().iter().position(|r| *r == row) {
            self.selected = at;
            self.reveal = true;
        }
    }

    pub fn step(&mut self, delta: isize) {
        let last = self.rows().len().saturating_sub(1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
        self.confirm = None;
        self.reveal = true;
    }

    /// Changes the member filter; the first match gets the selection.
    pub fn set_filter(&mut self, filter: String) {
        let before = self.current();
        self.filter = filter;
        self.confirm = None;
        let rows = self.rows();
        let wanted = if self.filter.is_empty() {
            // Emptied: the selection stays where it was.
            rows.iter().position(|r| Some(*r) == before)
        } else {
            rows.iter().position(|r| matches!(r, InfoRow::Member(_)))
        };
        self.selected = wanted.unwrap_or(self.selected.min(rows.len().saturating_sub(1)));
        self.reveal = true;
    }

    /// Takes new details, keeping the selection on the same row (members move when
    /// the list is reloaded).
    pub fn set_details(&mut self, details: RoomDetails) {
        let row = self.current();
        let member = row
            .and_then(|row| self.member(row))
            .map(|member| member.user_id.clone());
        self.name.clone_from(&details.name);
        self.details = Some(details);
        let rows = self.rows();
        let same = match (&member, row) {
            (Some(user_id), _) => rows.iter().position(
                |r| matches!(self.member(*r), Some(member) if member.user_id == *user_id),
            ),
            (None, Some(row)) => rows.iter().position(|r| *r == row),
            (None, None) => None,
        };
        self.selected = same.unwrap_or(self.selected.min(rows.len().saturating_sub(1)));
    }

    pub fn scroll_by(&mut self, lines: isize) {
        self.scroll = (self.scroll as isize + lines).max(0) as usize;
        self.reveal = false;
    }

    /// How leaving is called for this kind of room.
    pub fn leave_label(&self) -> &'static str {
        if self.invited() {
            "Decline invitation"
        } else if self.direct() {
            "Leave this chat"
        } else {
            "Leave room"
        }
    }

    pub fn leave_confirm(&self) -> &'static str {
        if self.invited() {
            "Enter again to decline, Esc to keep it"
        } else {
            "Enter again to leave, Esc to stay"
        }
    }
}

pub fn role_label(role: MemberRole) -> &'static str {
    match role {
        MemberRole::Owner => "owner",
        MemberRole::Admin => "admin",
        MemberRole::Moderator => "moderator",
        MemberRole::User => "",
    }
}

pub fn notify_label(level: NotifyLevel) -> &'static str {
    match level {
        NotifyLevel::All => "All messages",
        NotifyLevel::Mentions => "Mentions only",
        NotifyLevel::Mute => "Muted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(target: &str, via: &[&str]) -> Result<Address, String> {
        Ok(Address::Room {
            target: target.to_owned(),
            via: via.iter().map(|s| (*s).to_owned()).collect(),
        })
    }

    #[test]
    fn aliases_and_ids() {
        assert_eq!(
            parse_address("#rust:matrix.org"),
            room("#rust:matrix.org", &[])
        );
        assert_eq!(
            parse_address("  <#rust:matrix.org>\n"),
            room("#rust:matrix.org", &[])
        );
        assert_eq!(
            parse_address("!abc123:example.org"),
            room("!abc123:example.org", &[])
        );
        assert_eq!(
            parse_address("!abc123:example.org?via=example.org&via=matrix.org"),
            room("!abc123:example.org", &["example.org", "matrix.org"])
        );
        assert_eq!(
            parse_address("!abc123:example.org example.org, via=matrix.org:8448"),
            room("!abc123:example.org", &["example.org", "matrix.org:8448"])
        );
        assert_eq!(
            parse_address("@alice:example.org"),
            Ok(Address::User("@alice:example.org".to_owned()))
        );
    }

    #[test]
    fn matrix_to_links() {
        assert_eq!(
            parse_address("https://matrix.to/#/#rust:matrix.org"),
            room("#rust:matrix.org", &[])
        );
        assert_eq!(
            parse_address("https://matrix.to/#/%23rust%3Amatrix.org"),
            room("#rust:matrix.org", &[])
        );
        assert_eq!(
            parse_address("https://matrix.to/#/!abc:example.org?via=example.org&via=matrix.org"),
            room("!abc:example.org", &["example.org", "matrix.org"])
        );
        // A link to a message leads to its room.
        assert_eq!(
            parse_address("https://matrix.to/#/!abc:example.org/$event?via=example.org"),
            room("!abc:example.org", &["example.org"])
        );
        assert_eq!(
            parse_address("https://matrix.to/#/@bob:example.org"),
            Ok(Address::User("@bob:example.org".to_owned()))
        );
        assert!(parse_address("https://matrix.to/#/").is_err());
        assert!(parse_address("https://matrix.to/#/nonsense").is_err());
    }

    #[test]
    fn matrix_scheme_links() {
        assert_eq!(
            parse_address("matrix:r/rust:matrix.org"),
            room("#rust:matrix.org", &[])
        );
        assert_eq!(
            parse_address("matrix:roomid/abc:example.org?via=example.org&action=join"),
            room("!abc:example.org", &["example.org"])
        );
        assert_eq!(
            parse_address("matrix:u/bob:example.org?action=chat"),
            Ok(Address::User("@bob:example.org".to_owned()))
        );
        assert!(parse_address("matrix:nothing").is_err());
    }

    #[test]
    fn garbage_is_refused_with_a_sentence() {
        for input in [
            "",
            "   ",
            "rust",
            "#rust",
            "#:matrix.org extra words here!",
            "https://example.org/#/#rust:matrix.org",
            "!abc:example.org not_a_server!",
            "#rust:matrix.org\u{1b}[31m",
            "$event:example.org",
            "javascript:alert(1)",
        ] {
            let err = parse_address(input).expect_err(input);
            assert!(err.ends_with('.'), "{input:?}: {err}");
            assert!(!err.contains('\u{1b}'));
        }
    }

    #[test]
    fn user_ids() {
        assert_eq!(
            parse_user(" @bob:example.org "),
            Ok("@bob:example.org".into())
        );
        assert_eq!(parse_user("bob:example.org"), Ok("@bob:example.org".into()));
        assert_eq!(
            parse_user("https://matrix.to/#/@bob:example.org"),
            Ok("@bob:example.org".into())
        );
        for input in [
            "",
            "bob",
            "@bob",
            "@bob:",
            "@bo b:example.org",
            "#room:example.org",
            "https://matrix.to/#/#room:example.org",
        ] {
            assert!(parse_user(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn alias_local_parts() {
        assert_eq!(parse_alias_local(" #rats "), Ok("rats".into()));
        assert_eq!(parse_alias_local("rat-room_2"), Ok("rat-room_2".into()));
        for input in ["", "#", "rats:example.org", "two words", "a#b"] {
            assert!(parse_alias_local(input).is_err(), "{input:?}");
        }
    }

    fn details(members: &[(&str, bool)]) -> RoomDetails {
        RoomDetails {
            name: "Rats".into(),
            can_rename: true,
            can_set_topic: true,
            can_invite: true,
            notify: Some(NotifyLevel::All),
            members: members
                .iter()
                .map(|(name, invited)| RoomMember {
                    user_id: format!("@{name}:example.org"),
                    name: (*name).to_owned(),
                    role: MemberRole::User,
                    invited: *invited,
                    own: false,
                    removable: false,
                })
                .collect(),
            ..RoomDetails::default()
        }
    }

    #[test]
    fn info_rows_follow_permissions_and_the_filter() {
        let mut popup = InfoPopup::new("!r:example.org", "Rats", RoomKind::Room);
        assert_eq!(popup.rows(), [InfoRow::Favourite, InfoRow::Leave]);
        popup.set_details(details(&[
            ("alice", false),
            ("bob", false),
            ("carol", true),
        ]));
        assert_eq!(popup.rows().len(), 6 + 3);
        popup.set_filter("BO".into());
        assert_eq!(popup.rows().len(), 6 + 1);
        assert_eq!(popup.selected_member().unwrap().name, "bob");
        // Emptying the filter keeps the member selected, wherever it is now.
        popup.set_filter(String::new());
        assert_eq!(popup.selected_member().unwrap().name, "bob");
        popup.set_filter("zzz".into());
        assert_eq!(popup.current(), Some(InfoRow::Leave));
        // The selection stays with the member when the list changes under it.
        popup.set_filter(String::new());
        popup.select(InfoRow::Member(1));
        popup.set_details(details(&[("zed", false), ("alice", false), ("bob", false)]));
        assert_eq!(popup.selected_member().unwrap().name, "bob");
        // Nothing to edit or invite to without the rights.
        let mut plain = details(&[("alice", false)]);
        plain.can_rename = false;
        plain.can_set_topic = false;
        plain.can_invite = false;
        popup.set_details(plain);
        assert_eq!(
            popup.rows(),
            [
                InfoRow::Favourite,
                InfoRow::Notify,
                InfoRow::Leave,
                InfoRow::Member(0)
            ]
        );
        popup.step(100);
        assert_eq!(popup.current(), Some(InfoRow::Member(0)));
        popup.step(-100);
        assert_eq!(popup.current(), Some(InfoRow::Favourite));
    }

    #[test]
    fn an_invitation_can_only_be_declined() {
        let popup = InfoPopup::new("!r:example.org", "Rats", RoomKind::Invite);
        assert_eq!(popup.rows(), [InfoRow::Leave]);
        assert_eq!(popup.leave_label(), "Decline invitation");
    }

    #[test]
    fn debug_output_names_nothing_private() {
        let mut popup = NewPopup::new(Stage::Create);
        popup.name = "Secret plans".into();
        popup.user = "@bob:example.org".into();
        let text = format!("{popup:?}");
        assert!(!text.contains("Secret") && !text.contains("bob"));
        let mut info = InfoPopup::new("!r:example.org", "Secret plans", RoomKind::Room);
        info.set_details(details(&[("alice", false)]));
        let text = format!("{info:?}");
        assert!(!text.contains("Secret") && !text.contains("alice"));
    }
}
