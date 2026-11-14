//! Room management on the UI side: the addresses people type or paste, and the state of
//! the two popups ("New chat or room" and "Room info").

use matrix_sdk::ruma::matrix_uri::MatrixId;
use matrix_sdk::ruma::{EventId, MatrixToUri, MatrixUri, RoomAliasId, RoomId, ServerName, UserId};

use crate::lineedit::{EditOutcome, LineEdit};
use crate::matrix::{MemberRole, NotifyLevel, RoomDetails, RoomMember};
use crate::model::RoomKind;

// ----- addresses ---------------------------------------------------------------------

/// What a typed or pasted address points at.
#[derive(Clone, PartialEq, Eq)]
pub enum Address {
    Room {
        /// `#alias:server` or `!id:server`.
        target: String,
        /// Servers that know the room, for joining by id.
        via: Vec<String>,
    },
    /// A message inside a room (`!id:server` or `#alias:server`, `$event`).
    Event {
        room: String,
        event: String,
        via: Vec<String>,
    },
    User(String),
}

/// Hand-written: who or what someone wants to reach stays out of logs.
impl std::fmt::Debug for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Room { via, .. } => write!(f, "Address::Room(via {} servers)", via.len()),
            Self::Event { via, .. } => write!(f, "Address::Event(via {} servers)", via.len()),
            Self::User(_) => f.write_str("Address::User"),
        }
    }
}

/// Formats a room address (`#alias:server` or `!id:server`) and `via` servers as
/// accepted by the Join input.
pub fn format_join_target(target: &str, via: &[String]) -> String {
    if via.is_empty() {
        target.to_owned()
    } else {
        format!("{target} {}", via.join(" "))
    }
}

const NOT_AN_ADDRESS: &str =
    "That is not a room address. Use #room:server, !id:server or a matrix.to link.";
const NOT_A_USER: &str = "A user ID looks like @name:server.";
const NOT_A_SERVER: &str = "What follows the address is not a server name.";
const BAD_MATRIX_TO: &str = "That matrix.to link could not be read.";
const BAD_MATRIX_URI: &str = "That matrix: link could not be read.";
/// The longest address or link that is looked at.
const INPUT_MAX: usize = 2048;
/// What the Matrix specification allows for an identifier, in bytes.
const ID_MAX: usize = 255;
/// More servers than this only make the request longer.
pub const VIA_MAX: usize = 5;
/// The longest text any of the one-line inputs of the room popups takes.
pub const FIELD_MAX: usize = 1024;

/// Characters with no place in an identifier: whitespace, everything `text::sanitize`
/// would take out, and the invisible ones that make two different ids look the same.
fn bad_id_char(c: char) -> bool {
    c.is_control()
        || c.is_whitespace()
        || matches!(
            c,
            '\u{00ad}' | '\u{034f}' | '\u{061c}' | '\u{180e}' | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}'
        )
}

fn clean_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= ID_MAX && !id.chars().any(bad_id_char)
}

/// What can go into a one-line input: `text` without control characters and
/// direction overrides, on one line.
pub fn clean_input(text: &str) -> String {
    crate::text::sanitize_line(text)
}

/// A room id or alias as the server wants it, or nothing. Checked after links were
/// decoded, so nothing can hide behind percent signs.
fn checked_room(id: &str) -> Option<String> {
    if !clean_id(id) {
        return None;
    }
    let body = &id[1..];
    if id.starts_with('#') {
        let (local, server) = body.split_once(':')?;
        (!local.is_empty() && !server.is_empty()).then_some(())?;
        return RoomAliasId::parse(id).ok().map(|alias| alias.to_string());
    }
    if !id.starts_with('!') || body.contains(['#', '?', '/', '!', '@', '$']) {
        return None;
    }
    // Newer rooms have ids without a server part; where there is one, both parts count.
    if let Some((opaque, server)) = body.split_once(':') {
        (!opaque.is_empty() && ServerName::parse(server).is_ok()).then_some(())?;
    } else if body.is_empty() {
        return None;
    }
    RoomId::parse(id).ok().map(|id| id.to_string())
}

fn checked_event(id: &str) -> Option<String> {
    if !clean_id(id) || !id.starts_with('$') {
        return None;
    }
    EventId::parse(id).ok().map(|event| event.to_string())
}

fn checked_user(id: &str) -> Option<String> {
    if !clean_id(id) {
        return None;
    }
    let (local, server) = id.strip_prefix('@')?.split_once(':')?;
    (!local.is_empty() && !server.is_empty()).then_some(())?;
    UserId::parse(id).ok().map(|user| user.to_string())
}

/// Adds a server to ask, once, up to `VIA_MAX`.
fn push_via(via: &mut Vec<String>, name: &str) -> Result<(), String> {
    if !clean_id(name) {
        return Err(NOT_A_SERVER.to_owned());
    }
    let server = ServerName::parse(name).map_err(|_| NOT_A_SERVER.to_owned())?;
    let server = server.to_string();
    if via.len() < VIA_MAX && !via.contains(&server) {
        via.push(server);
    }
    Ok(())
}

/// What follows `https://matrix.to/#/` if `link` is such a link (in any spelling of
/// the scheme and host); `Some("")` for a matrix.to link of another shape.
fn matrix_to_rest(link: &str) -> Option<&str> {
    for base in ["https://matrix.to/#/", "http://matrix.to/#/"] {
        if let Some(head) = link.get(..base.len())
            && head.eq_ignore_ascii_case(base)
        {
            return Some(&link[base.len()..]);
        }
    }
    let lower = link.to_ascii_lowercase();
    (lower.starts_with("https://matrix.to") || lower.starts_with("http://matrix.to")).then_some("")
}

/// True if `url` is a Matrix link (`matrix:` or `matrix.to/#`) that should be handled in-app
/// rather than opened in an external browser.
pub fn is_matrix_link(url: &str) -> bool {
    let trimmed = url.trim();
    let trimmed = trimmed.strip_prefix('<').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix('>').unwrap_or(trimmed).trim();
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("matrix:")
        || lower.starts_with("https://matrix.to/#")
        || lower.starts_with("http://matrix.to/#")
}

/// Checks the identifier parts of a link's path: as many as allowed, none empty. The
/// library that reads the links looks at the first byte of each part without checking
/// that there is one. Returns the path without slashes around it.
fn path_parts<'a>(path: &'a str, allowed: &[usize]) -> Option<&'a str> {
    let path = path.trim_matches('/');
    let mut count = 0;
    for part in path.split('/') {
        if part.is_empty() {
            return None;
        }
        count += 1;
    }
    allowed.contains(&count).then_some(path)
}

/// Understands `#alias:server`, `!id:server` (optionally followed by `?via=server` or
/// by server names), `@user:server`, and `matrix.to` / `matrix:` links pasted as they
/// are. The error is a sentence for the user.
pub fn parse_address(input: &str) -> Result<Address, String> {
    let input = input.trim();
    let input = input.strip_prefix('<').unwrap_or(input);
    let input = input.strip_suffix('>').unwrap_or(input).trim();
    if input.is_empty() {
        return Err("Enter an address first.".to_owned());
    }
    if input.len() > INPUT_MAX {
        return Err("That is too long to be an address.".to_owned());
    }
    // The address itself, then perhaps the names of servers to ask.
    let mut words = input.split_whitespace();
    let first = words.next().unwrap_or_default().trim_end_matches(',');
    let mut via: Vec<String> = Vec::new();
    for word in words.flat_map(|word| word.split(',')) {
        if !word.is_empty() {
            push_via(&mut via, word.strip_prefix("via=").unwrap_or(word))?;
        }
    }
    if first.chars().any(bad_id_char) {
        return Err(NOT_AN_ADDRESS.to_owned());
    }

    let (id, link_via) = if let Some(rest) = matrix_to_rest(first) {
        let (path, query) = match rest.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (rest, None),
        };
        let path = path_parts(path, &[1, 2]).ok_or_else(|| BAD_MATRIX_TO.to_owned())?;
        let mut link = format!("https://matrix.to/#/{path}");
        if let Some(query) = query {
            link.push('?');
            link.push_str(query);
        }
        let link = MatrixToUri::parse(&link).map_err(|_| BAD_MATRIX_TO.to_owned())?;
        (link.id().clone(), link.via().to_vec())
    } else if first
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("matrix:"))
    {
        let rest = &first[7..];
        // No server belongs in front of the path, and nothing behind the query.
        if rest.starts_with("//") || rest.contains('#') {
            return Err(BAD_MATRIX_URI.to_owned());
        }
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        let path = path_parts(path, &[2, 4]).ok_or_else(|| BAD_MATRIX_URI.to_owned())?;
        let link = MatrixUri::parse(&format!("matrix:{path}?{query}"))
            .map_err(|_| BAD_MATRIX_URI.to_owned())?;
        (link.id().clone(), link.via().to_vec())
    } else if first.starts_with('@') {
        return checked_user(first)
            .map(Address::User)
            .ok_or_else(|| NOT_A_USER.to_owned());
    } else if first.starts_with('#') {
        let target = checked_room(first).ok_or_else(|| NOT_AN_ADDRESS.to_owned())?;
        return Ok(Address::Room { target, via });
    } else if first.starts_with('!') {
        let (id, query) = first.split_once('?').unwrap_or((first, ""));
        let target = checked_room(id).ok_or_else(|| NOT_AN_ADDRESS.to_owned())?;
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            if let Some(name) = pair.strip_prefix("via=") {
                push_via(&mut via, name)?;
            }
        }
        return Ok(Address::Room { target, via });
    } else {
        return Err(NOT_AN_ADDRESS.to_owned());
    };

    // From a link: what it decodes to has to pass the same checks as what is typed.
    let (room, event) = match &id {
        MatrixId::Room(id) => (id.to_string(), None),
        MatrixId::RoomAlias(alias) => (alias.to_string(), None),
        MatrixId::Event(room, event) => (room.to_string(), Some(event.to_string())),
        MatrixId::User(user) => {
            return checked_user(user.as_str())
                .map(Address::User)
                .ok_or_else(|| NOT_A_USER.to_owned());
        }
        _ => return Err(NOT_AN_ADDRESS.to_owned()),
    };
    let target = checked_room(&room).ok_or_else(|| NOT_AN_ADDRESS.to_owned())?;
    for server in link_via {
        push_via(&mut via, server.as_str())?;
    }
    if let Some(event) = event {
        let event = checked_event(&event).ok_or_else(|| NOT_AN_ADDRESS.to_owned())?;
        return Ok(Address::Event {
            room: target,
            event,
            via,
        });
    }
    Ok(Address::Room { target, via })
}

/// A complete user id, from what was typed (`@name:server`, `name:server`, or a link
/// to a user).
pub fn parse_user(input: &str) -> Result<String, String> {
    let input = input.trim();
    let input = input.strip_prefix('<').unwrap_or(input);
    let input = input.strip_suffix('>').unwrap_or(input).trim();
    if input.is_empty() {
        return Err("Enter a user ID first.".to_owned());
    }
    if input.len() > INPUT_MAX {
        return Err(NOT_A_USER.to_owned());
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
    checked_user(&with_sigil).ok_or_else(|| NOT_A_USER.to_owned())
}

/// The part of a new room's address before the colon, from what was typed.
pub fn parse_alias_local(input: &str) -> Result<String, String> {
    let input = input.trim();
    let local = input.strip_prefix('#').unwrap_or(input);
    if local.is_empty() {
        return Err("A public room needs an address.".to_owned());
    }
    if local.contains(':') {
        return Err("Only the part before the colon; your server adds its own name.".to_owned());
    }
    if local.chars().any(char::is_whitespace) {
        return Err("An address cannot contain spaces.".to_owned());
    }
    // The same rules the complete address has to pass. The server's name is not
    // known here; the server has the last word on the length.
    if local.contains('#') || checked_room(&format!("#{local}:example.org")).is_none() {
        return Err("That address is not valid.".to_owned());
    }
    Ok(local.to_owned())
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
    /// Room info for the open room, whatever has the focus (its name was clicked).
    OpenInfo,
}

/// The form behind Ctrl-N. One popup for all stages, so going back keeps what was typed.
#[derive(Clone)]
pub struct NewPopup {
    pub stage: Stage,
    /// Cursor in the chooser.
    pub choice: usize,
    /// Opened at a form directly: Esc closes instead of going back to the chooser.
    pub single: bool,
    pub address: LineEdit,
    pub user: LineEdit,
    pub name: LineEdit,
    pub topic: LineEdit,
    pub public: bool,
    pub alias: LineEdit,
    pub encrypted: bool,
    /// The slot with the cursor (see `Self::slots`).
    pub focus: usize,
    /// What is being waited for; input is locked meanwhile.
    pub busy: Option<&'static str>,
    pub error: Option<String>,
    /// Id and name of the room to invite to.
    pub room: Option<(String, String)>,
    /// Number of the request whose answer is awaited; answers to others are not ours.
    pub request: Option<u64>,
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
            address: LineEdit::new().with_max_bytes(FIELD_MAX),
            user: LineEdit::new().with_max_bytes(FIELD_MAX),
            name: LineEdit::new().with_max_bytes(FIELD_MAX),
            topic: LineEdit::new().with_max_bytes(FIELD_MAX),
            public: false,
            alias: LineEdit::new().with_max_bytes(FIELD_MAX),
            encrypted: true,
            focus: 0,
            busy: None,
            error: None,
            room: None,
            request: None,
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
    pub fn field_mut(&mut self) -> Option<&mut LineEdit> {
        self.field_at_mut(self.focus)
    }

    pub fn field_at_mut(&mut self, slot: usize) -> Option<&mut LineEdit> {
        match (self.stage, slot) {
            (Stage::Join, 0) => Some(&mut self.address),
            (Stage::Direct | Stage::Invite, 0) => Some(&mut self.user),
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
#[derive(Clone, PartialEq, Eq)]
pub enum Confirm {
    Leave,
    /// Remove the member with this user id.
    Remove(String),
    /// There is no direct chat with this user yet: create one and invite them.
    NewChat(String),
}

/// Hand-written: who it is about stays out of logs.
impl std::fmt::Debug for Confirm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Leave => "Confirm::Leave",
            Self::Remove(_) => "Confirm::Remove",
            Self::NewChat(_) => "Confirm::NewChat",
        })
    }
}

/// The answer the room info view is waiting for. Any other answer leaves it alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// A direct chat to open, by the number of the request.
    Chat(u64),
    Change(crate::matrix::Change),
    Leave,
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
    pub filter: LineEdit,
    /// Index into `Self::rows`.
    pub selected: usize,
    /// First line shown, and how many fit (as of the last draw).
    pub scroll: usize,
    pub page: usize,
    /// Ask the next draw to bring the selected row into view.
    pub reveal: bool,
    /// The name or topic while it is being typed.
    pub editing: Option<(InfoRow, LineEdit)>,
    pub confirm: Option<Confirm>,
    /// A change is on its way to the server.
    pub busy: Option<&'static str>,
    /// Which answer ends `busy`.
    pub pending: Option<Pending>,
    /// A row to select once it is there: some only exist when the details are in.
    pub wanted: Option<InfoRow>,
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
            filter: LineEdit::new().with_max_bytes(FIELD_MAX),
            selected: 0,
            scroll: 0,
            page: 10,
            reveal: true,
            editing: None,
            confirm: None,
            busy: None,
            pending: None,
            wanted: None,
        }
    }

    /// Locks the view until the answer `pending` is there.
    pub fn wait_for(&mut self, pending: Pending, label: &'static str) {
        self.busy = Some(label);
        self.pending = Some(pending);
    }

    /// Ends the wait if `answer` is the one waited for.
    pub fn answered(&mut self, answer: Pending) -> bool {
        if self.pending != Some(answer) {
            return false;
        }
        self.pending = None;
        self.busy = None;
        true
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

    /// Selects `row` if it is shown; before the details are in, as soon as it is.
    pub fn select(&mut self, row: InfoRow) {
        if let Some(at) = self.rows().iter().position(|r| *r == row) {
            if at != self.selected {
                self.confirm = None;
            }
            self.selected = at;
            self.reveal = true;
        } else if self.details.is_none() {
            self.wanted = Some(row);
        }
    }

    pub fn step(&mut self, delta: isize) {
        self.confirm = None;
        // The user has chosen for themselves.
        self.wanted = None;
        // Up from the first row: what stands above it (name, topic, facts) comes
        // back into view; in a low window the rows alone can fill it.
        if delta < 0 && self.selected == 0 {
            self.scroll = self.scroll.saturating_sub(delta.unsigned_abs());
            self.reveal = false;
            return;
        }
        let last = self.rows().len().saturating_sub(1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
        self.reveal = true;
    }

    /// Mutates the member filter in place; when the text changes, the first match gets the selection.
    pub fn apply_filter(&mut self, edit: impl FnOnce(&mut LineEdit) -> EditOutcome) -> EditOutcome {
        let before = self.current();
        let outcome = edit(&mut self.filter);
        if outcome.changed() {
            self.after_filter_change(before);
        }
        outcome
    }

    fn after_filter_change(&mut self, before: Option<InfoRow>) {
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

    /// Changes the member filter; the first match gets the selection.
    pub fn set_filter(&mut self, filter: String) {
        self.apply_filter(|edit| {
            if edit.as_str() == filter {
                EditOutcome::Ignored
            } else {
                edit.set(filter);
                EditOutcome::Changed
            }
        });
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
        // A question about someone who is gone, or whom we may no longer remove, is
        // withdrawn; so is an edit we no longer have the right to make.
        let details = self.details.as_ref().expect("just set");
        let still = |user_id: &String, removable: bool| {
            details
                .members
                .iter()
                .any(|m| m.user_id == *user_id && (m.removable || !removable))
        };
        let keep = match &self.confirm {
            Some(Confirm::Remove(user_id)) => still(user_id, true),
            Some(Confirm::NewChat(user_id)) => still(user_id, false),
            Some(Confirm::Leave) | None => true,
        };
        if !keep {
            self.confirm = None;
        }
        let allowed = match self.editing.as_ref().map(|(row, _)| *row) {
            Some(InfoRow::Name) => details.can_rename,
            Some(InfoRow::Topic) => details.can_set_topic,
            _ => true,
        };
        if !allowed {
            self.editing = None;
            self.error = Some("You are no longer allowed to change this.".to_owned());
        }
        let rows = self.rows();
        let same = match (&member, row) {
            (Some(user_id), _) => rows.iter().position(
                |r| matches!(self.member(*r), Some(member) if member.user_id == *user_id),
            ),
            (None, Some(row)) => rows.iter().position(|r| *r == row),
            (None, None) => None,
        };
        self.selected = same.unwrap_or(self.selected.min(rows.len().saturating_sub(1)));
        if let Some(row) = self.wanted.take() {
            self.select(row);
        }
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

    /// The question before leaving: the keys first, so that a narrow window cuts the
    /// explanation and not them. `Self::confirm_keys` is the part that must fit.
    pub fn leave_confirm(&self) -> &'static str {
        if self.invited() {
            "Enter again to decline, Esc to keep it"
        } else {
            "Enter again to leave, Esc to stay · this device forgets the room"
        }
    }

    /// The room list's view of the room changed (an invitation was accepted, say).
    /// Returns whether the details have to be asked for again.
    pub fn set_kind(&mut self, kind: RoomKind) -> bool {
        let was_invite = self.invited();
        self.kind = kind;
        if was_invite != self.invited() {
            self.confirm = None;
            self.editing = None;
            self.selected = 0;
            self.reveal = true;
            return true;
        }
        false
    }
}

/// The part of a confirmation that names the keys: `prompt` up to its first `·`.
pub fn confirm_keys(prompt: &str) -> &str {
    prompt.split('·').next().unwrap_or(prompt).trim_end()
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
        // A link to a message identifies both its room and event.
        assert_eq!(
            parse_address("https://matrix.to/#/!abc:example.org/$event?via=example.org"),
            Ok(Address::Event {
                room: "!abc:example.org".to_owned(),
                event: "$event".to_owned(),
                via: vec!["example.org".to_owned()],
            })
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
            parse_address("matrix:roomid/abc:example.org/e/event?via=example.org"),
            Ok(Address::Event {
                room: "!abc:example.org".to_owned(),
                event: "$event".to_owned(),
                via: vec!["example.org".to_owned()],
            })
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

    #[test]
    fn links_with_empty_parts_are_refused_not_fatal() {
        // The library behind the links indexes into each part of the path; these
        // used to end the program.
        for input in [
            "https://matrix.to/#/!room:host///",
            "https://matrix.to/#///!room:host",
            "https://matrix.to/#////",
            "https://matrix.to/#//",
            "https://matrix.to/#/!room:host//$event",
            "https://matrix.to/#/!room:host/",
            "https://matrix.to/#/%",
            "https://matrix.to/#/?via=example.org",
            "matrix:r//",
            "matrix:r/",
            "matrix:roomid//e/",
            "matrix:///",
            "matrix:",
        ] {
            let _ = parse_address(input);
            let _ = parse_user(input);
        }
        // Nothing but slashes, or an empty part in the middle: not a link to anything.
        for input in [
            "https://matrix.to/#////",
            "https://matrix.to/#/!room:host//$event",
            "matrix:r//",
            "matrix:roomid//e/",
        ] {
            assert!(parse_address(input).is_err(), "{input}");
        }
        // Slashes too many around the address are only untidy.
        for input in [
            "https://matrix.to/#/!room:host/",
            "https://matrix.to/#/!room:host///",
            "https://matrix.to/#///!room:host",
        ] {
            assert_eq!(parse_address(input), room("!room:host", &[]), "{input}");
        }
    }

    #[test]
    fn no_link_can_panic() {
        // Every arrangement of the characters that matter, behind each kind of link.
        let pieces = [
            "/", "%", "?", "#", "!", "@", "$", ":", "a", "%2F", "&via=", "=", "r", "u",
        ];
        let mut tried = 0;
        for base in [
            "https://matrix.to/#/",
            "https://matrix.to/",
            "matrix:",
            "matrix:r/",
            "!",
            "#",
            "@",
        ] {
            for a in pieces {
                for b in pieces {
                    for c in pieces {
                        for d in pieces {
                            let input = format!("{base}{a}{b}{c}{d}");
                            let _ = parse_address(&input);
                            let _ = parse_user(&input);
                            tried += 1;
                        }
                    }
                }
            }
        }
        assert!(tried > 200_000);
    }

    #[test]
    fn nothing_hides_behind_percent_signs() {
        for input in [
            // Escape, newline, tab and space, encoded.
            "https://matrix.to/#/%23room%1b[31m:example.org",
            "https://matrix.to/#/%23room%0a:example.org",
            "https://matrix.to/#/%23ro%20om:example.org",
            "https://matrix.to/#/!room%09:example.org",
            "https://matrix.to/#/@bob%0a:example.org",
            "matrix:u/bob%0a:example.org",
            "matrix:r/room%1b:example.org",
            "matrix:roomid/room%20x:example.org",
            // Direction overrides and characters that cannot be seen, as they are
            // and encoded.
            "#room\u{202e}moor:example.org",
            "#room\u{200b}:example.org",
            "@bob\u{200b}:example.org",
            "@bob\u{202e}evil:example.org",
            "@bob\u{feff}:example.org",
            "https://matrix.to/#/%40bob%E2%80%8B:example.org",
            "https://matrix.to/#/%23room%E2%80%AE:example.org",
            "!room:example.org?via=exa\u{200b}mple.org",
            "https://matrix.to/#/!room:example.org?via=exa%E2%80%8Bmple.org",
        ] {
            assert!(parse_address(input).is_err(), "{input:?}");
            assert!(parse_user(input).is_err(), "{input:?}");
        }
        // The complaint itself carries nothing of the kind to the screen.
        let err = parse_address("!room:example.org exa\u{202e}mple").unwrap_err();
        assert!(!err.contains('\u{202e}'));
        assert!(parse_alias_local("ra\u{200b}ts").is_err());
        assert!(parse_alias_local("ra\u{202e}ts").is_err());
    }

    #[test]
    fn spellings_of_matrix_to() {
        for input in [
            "http://matrix.to/#/#rust:matrix.org",
            "HTTPS://MATRIX.TO/#/#rust:matrix.org",
            "Https://Matrix.To/#/%23rust:matrix.org",
        ] {
            assert_eq!(
                parse_address(input),
                room("#rust:matrix.org", &[]),
                "{input}"
            );
        }
        assert_eq!(
            parse_user("HTTP://matrix.to/#/@bob:example.org"),
            Ok("@bob:example.org".into())
        );
        // matrix.to, but not a link to anything.
        for input in [
            "https://matrix.to",
            "https://matrix.to/",
            "https://matrix.to/docs",
        ] {
            assert_eq!(parse_address(input).unwrap_err(), BAD_MATRIX_TO, "{input}");
        }
    }

    #[test]
    fn empty_and_malformed_parts() {
        for input in [
            "#:matrix.org",
            "#room:",
            "@:example.org",
            "@bob:",
            "!",
            "!:example.org",
            "!r:host#extra",
            "!r:host/extra",
            "!r:not_a_host!",
            "matrix:r/:example.org",
            "matrix:u/:example.org",
            "matrix://evil.example:999/r/rust:matrix.org",
            "matrix:r/rust:matrix.org#frag",
            "MATRIX://x/r/rust:matrix.org",
        ] {
            assert!(parse_address(input).is_err(), "{input:?}");
        }
        assert!(parse_user(":example.org").is_err());
        assert!(parse_user("@:example.org").is_err());
        // Rooms of the newest version have ids without a server part.
        assert_eq!(
            parse_address("!9xcIvO0s67btxlOiSEwIkDmmmso1C8694gNflknIm1k"),
            room("!9xcIvO0s67btxlOiSEwIkDmmmso1C8694gNflknIm1k", &[])
        );
        // The scheme in capitals is still the scheme.
        assert_eq!(
            parse_address("MATRIX:r/rust:matrix.org"),
            room("#rust:matrix.org", &[])
        );
        // Too long to be anything.
        assert!(parse_address(&format!("#{}:example.org", "a".repeat(300))).is_err());
        assert!(parse_address(&"#".repeat(5000)).is_err());
        assert!(parse_user(&format!("@{}:example.org", "a".repeat(300))).is_err());
    }

    #[test]
    fn servers_to_ask_once_each_and_not_too_many() {
        // Repeats do not crowd out the others, wherever they come from.
        assert_eq!(
            parse_address("!r:a.org a.org a.org, a.org b.org"),
            room("!r:a.org", &["a.org", "b.org"])
        );
        assert_eq!(
            parse_address("!r:a.org?via=a.org&via=a.org&via=b.org a.org c.org"),
            room("!r:a.org", &["a.org", "c.org", "b.org"])
        );
        assert_eq!(
            parse_address("https://matrix.to/#/!r:a.org?via=a.org&via=b.org&via=a.org b.org"),
            room("!r:a.org", &["b.org", "a.org"])
        );
        let many: Vec<String> = (0..40).map(|n| format!("s{n}.org")).collect();
        let Ok(Address::Room { via, .. }) = parse_address(&format!("!r:a.org {}", many.join(" ")))
        else {
            panic!("not a room");
        };
        assert_eq!(via.len(), VIA_MAX);
        assert_eq!(via[0], "s0.org");
        // A comma belongs to the list, not to the address before it.
        assert_eq!(
            parse_address("#rust:matrix.org, matrix.org"),
            room("#rust:matrix.org", &["matrix.org"])
        );
    }

    #[test]
    fn user_ids_in_brackets() {
        assert_eq!(
            parse_user("<@bob:example.org>"),
            Ok("@bob:example.org".into())
        );
        assert_eq!(
            parse_address("<@bob:example.org>"),
            Ok(Address::User("@bob:example.org".into()))
        );
    }

    #[test]
    fn what_is_typed_into_a_field_is_text() {
        assert_eq!(
            clean_input("a\u{1b}[31mb\u{202e}c\td\ne\u{7}"),
            "a[31mbc    d e"
        );
        // Names may hold characters that join others into one picture.
        assert_eq!(
            clean_input("👨\u{200d}👩\u{200d}👧"),
            "👨\u{200d}👩\u{200d}👧"
        );
    }

    #[test]
    fn form_state() {
        let mut form = NewPopup::new(Stage::Choose);
        assert!(!form.single && form.field_mut().is_none());
        form.step(-1);
        assert_eq!(form.choice, 2);
        form.go(Stage::CHOICES[form.choice]);
        assert_eq!(form.stage, Stage::Create);
        // Name, topic, a switch, the encryption switch, the button.
        form.field_mut().unwrap().insert("n");
        form.step(2);
        assert!(form.field_mut().is_none() && form.flip(NewPopup::ACCESS));
        assert!(form.public);
        form.step(1);
        form.field_mut().unwrap().insert("a");
        assert_eq!((form.name.as_str(), form.alias.as_str()), ("n", "a"));
        assert!(!form.flip(NewPopup::EXTRA) && form.encrypted);
        form.flip(NewPopup::ACCESS);
        assert!(form.field_mut().is_none() && form.flip(NewPopup::EXTRA) && !form.encrypted);
        form.step(1);
        form.step(1);
        assert_eq!(form.focus, NewPopup::NAME);
        // Going elsewhere drops the complaint, not what was typed.
        form.error = Some("no".into());
        form.go(Stage::Join);
        assert!(form.error.is_none());
        form.go(Stage::Create);
        assert_eq!(form.name, "n");
    }

    #[test]
    fn questions_and_edits_end_when_their_reason_does() {
        let mut list = details(&[("alice", false), ("bob", false)]);
        list.members[1].removable = true;
        let mut popup = InfoPopup::new("!r:example.org", "Rats", RoomKind::Room);
        popup.set_details(list.clone());
        // The member is still there and still removable: the question stands.
        popup.confirm = Some(Confirm::Remove("@bob:example.org".into()));
        popup.set_details(list.clone());
        assert!(popup.confirm.is_some());
        // No longer removable, or gone.
        let mut demoted = list.clone();
        demoted.members[1].removable = false;
        popup.set_details(demoted);
        assert_eq!(popup.confirm, None);
        popup.confirm = Some(Confirm::NewChat("@bob:example.org".into()));
        popup.set_details(details(&[("alice", false)]));
        assert_eq!(popup.confirm, None);
        // Selecting something else withdraws a question; selecting the same does not.
        popup.confirm = Some(Confirm::Leave);
        popup.select(popup.current().unwrap());
        assert!(popup.confirm.is_some());
        popup.select(InfoRow::Leave);
        popup.select(InfoRow::Favourite);
        assert_eq!(popup.confirm, None);
        // An edit we lost the right to make is over, and says why.
        popup.editing = Some((InfoRow::Topic, "new".into()));
        let mut plain = details(&[("alice", false)]);
        plain.can_set_topic = false;
        popup.set_details(plain);
        assert!(popup.editing.is_none() && popup.error.is_some());
    }

    #[test]
    fn an_accepted_invitation_becomes_a_room() {
        let mut popup = InfoPopup::new("!r:example.org", "Rats", RoomKind::Invite);
        popup.confirm = Some(Confirm::Leave);
        assert!(!popup.set_kind(RoomKind::Invite));
        assert!(popup.confirm.is_some());
        assert!(popup.set_kind(RoomKind::Room));
        assert_eq!(popup.confirm, None);
        assert_eq!(popup.rows(), [InfoRow::Favourite, InfoRow::Leave]);
        assert_eq!(popup.leave_label(), "Leave room");
    }

    #[test]
    fn up_from_the_first_row_scrolls_to_the_top() {
        let mut popup = InfoPopup::new("!r:example.org", "Rats", RoomKind::Room);
        popup.set_details(details(&[("alice", false)]));
        popup.scroll = 5;
        popup.step(-1);
        assert_eq!((popup.selected, popup.scroll), (0, 4));
        popup.step(-10);
        assert_eq!(popup.scroll, 0);
        assert!(!popup.reveal);
        popup.step(1);
        assert!(popup.selected == 1 && popup.reveal);
    }

    #[test]
    fn only_the_awaited_answer_ends_the_wait() {
        use crate::matrix::Change;
        let mut popup = InfoPopup::new("!r:example.org", "Rats", RoomKind::Room);
        popup.wait_for(Pending::Change(Change::Kick), "Removing…");
        assert!(!popup.answered(Pending::Change(Change::Name)));
        assert!(!popup.answered(Pending::Chat(1)));
        assert!(popup.busy.is_some());
        assert!(popup.answered(Pending::Change(Change::Kick)));
        assert!(popup.busy.is_none() && popup.pending.is_none());
        assert!(!popup.answered(Pending::Change(Change::Kick)));
    }

    #[test]
    fn debug_output_of_addresses_and_questions() {
        let text = format!(
            "{:?} {:?} {:?} {:?}",
            parse_address("#secret:example.org").unwrap(),
            parse_address("@bob:example.org").unwrap(),
            Confirm::Remove("@bob:example.org".into()),
            Confirm::NewChat("@bob:example.org".into()),
        );
        assert!(!text.contains("secret") && !text.contains("bob"), "{text}");
        assert_eq!(
            confirm_keys("Enter again, Esc to stay · and more"),
            "Enter again, Esc to stay"
        );
        assert_eq!(confirm_keys("no more"), "no more");
    }
}
