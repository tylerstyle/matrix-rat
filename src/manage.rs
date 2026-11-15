//! Room management on the UI side: the addresses people type or paste, and the state of
//! the two popups ("New chat or room" and "Room info").

use matrix_sdk::ruma::matrix_uri::MatrixId;
use matrix_sdk::ruma::{EventId, MatrixToUri, MatrixUri, RoomAliasId, RoomId, ServerName, UserId};

use crate::lineedit::{EditOutcome, LineEdit};
use crate::matrix::{
    MemberRole, NotifyLevel, RoomDetails, RoomKnockRequest, RoomMember, RoomPermissions, SpaceChild,
};
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
    /// Pick one of the ways below.
    Choose,
    Join,
    Direct,
    Create,
    /// A space: a room that gathers other rooms.
    CreateSpace,
    /// Search the public room directory.
    Browse,
    /// Inspect an unjoined room's summary before joining or asking to join.
    Preview,
    /// Invite someone to the room the popup was opened for.
    Invite,
}

impl Stage {
    pub const CHOICES: [Stage; 5] = [
        Stage::Join,
        Stage::Direct,
        Stage::Create,
        Stage::CreateSpace,
        Stage::Browse,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Stage::Choose => "New chat or room",
            Stage::Join => "Join a room",
            Stage::Direct => "Start a direct chat",
            Stage::Create => "Create a room",
            Stage::CreateSpace => "Create a space",
            Stage::Browse => "Browse public rooms",
            Stage::Preview => "Room preview",
            Stage::Invite => "Invite to this room",
        }
    }

    /// What the chooser says under the title.
    pub fn summary(self) -> &'static str {
        match self {
            Stage::Join => "By its address or a matrix.to link",
            Stage::Direct => "With one person, by user ID",
            Stage::Create => "Private or public, with a name of your choice",
            Stage::CreateSpace => "A place that gathers rooms, to share or to tidy up",
            Stage::Browse => "Discover rooms on this or another server",
            Stage::Choose | Stage::Preview | Stage::Invite => "",
        }
    }
}

/// Action button inside the Room Preview screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewButton {
    Open,
    Accept,
    Withdraw,
    Join,
    Knock,
    Cancel,
}

impl PreviewButton {
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open room",
            Self::Accept => "Accept invite",
            Self::Withdraw => "Withdraw request",
            Self::Join => "Join room",
            Self::Knock => "Ask to join",
            Self::Cancel => "Cancel",
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
    /// Open the ban prompt for the selected member.
    MemberBan,
    /// Unban the selected banned member (asks first).
    MemberUnban,
    /// Open the role chooser for the selected member.
    MemberRole,
    /// Pick an option inside the open role chooser.
    RoleOption(usize),
    /// Ignore or stop ignoring the selected member (asks first).
    MemberIgnore,
    /// Accept the selected incoming knock request.
    KnockAccept,
    /// Decline the selected incoming knock request (asks first).
    KnockDecline,
    /// Open the ban prompt for the selected incoming knock request.
    KnockBan,
    /// Favourite on or off for the room in question, without a popup.
    Favourite,
    /// Low priority on or off for the room in question, without a popup.
    LowPriority,
    /// Pick a room directory search result.
    BrowseRow(usize),
    /// Click a button in the room preview screen.
    PreviewBtn(usize),
    /// Open the room info for the open room, whatever has the focus (its name was clicked).
    OpenInfo,
    /// Pick an option inside a setting choice subview (JoinRule or History).
    SettingChoice(usize),
    /// Focus or activate a slot/button inside a Room Settings subview.
    SettingSlot(usize),
    /// Highlight an address entry inside the Addresses subview.
    AddressRow(usize),
    /// Promote an address entry to main canonical alias.
    AddressMakeMain(usize),
    /// Publish a local-only address as an alternate address.
    AddressPublishAlt(usize),
    /// Unpublish a canonical or alternate address.
    AddressUnpublish(usize),
    /// Delete a local address on our own homeserver.
    AddressDeleteLocal(usize),
    /// Set a permission threshold `(action_index, power_level)`.
    PermSet(usize, i64),
    /// Pick a space to put the room in question into.
    AddToSpace,
    /// Take the room in question out of the space the room list is narrowed to
    /// (shown in its room info, which asks first).
    RemoveFromSpace,
    /// Pick a room to put into the space the room list is narrowed to.
    AddRoomToSpace,
    /// Open the selected room of a space, or preview it if we are not in it.
    ChildOpen,
    /// Suggested on or off for the selected room of a space.
    ChildSuggest,
    /// Take the selected room out of the space (asks first).
    ChildRemove,
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
    /// That room is a space.
    pub into_space: bool,
    /// Number of the request whose answer is awaited; answers to others are not ours.
    pub request: Option<u64>,
    /// Type-ahead user search matches shown under the user ID field.
    pub user_results: Vec<crate::matrix::UserMatch>,
    /// Highlighted row inside `user_results`, if the user stepped into the dropdown.
    pub user_selected: Option<usize>,
    /// True while a debounced/worker user directory lookup is pending for this input.
    pub user_searching: bool,
    /// Generation of the most recent user search started from this popup.
    pub user_search_gen: Option<u64>,
    /// Public room directory search filter.
    pub dir_query: LineEdit,
    /// Optional server name for public room directory (`None` / blank = own homeserver).
    pub dir_server: LineEdit,
    /// Placeholder label for the own homeserver in the directory server field.
    pub dir_default_server: Option<String>,
    /// Public rooms returned so far for the active directory search.
    pub dir_results: Vec<crate::matrix::PublicRoom>,
    /// Highlighted row inside `dir_results`.
    pub dir_selected: usize,
    /// First visible result row in the scrollable directory list.
    pub dir_scroll: usize,
    /// Height of the directory list in rows (set by the UI renderer).
    pub dir_page: usize,
    /// Keeps `dir_selected` scrolled into view on the next draw.
    pub dir_reveal: bool,
    /// Whether another page of directory results exists on the server.
    pub dir_has_more: bool,
    /// True while an initial directory search is pending.
    pub dir_searching: bool,
    /// True while a pagination batch is pending.
    pub dir_loading_more: bool,
    /// Generation of the active directory search.
    pub dir_search_gen: Option<u64>,
    /// Directory lookup error shown cleanly instead of hanging.
    pub dir_error: Option<String>,
    /// Room ID or alias for the room preview screen.
    pub preview_target: String,
    /// Homeservers passed via `via=` for previewing/joining/knocking.
    pub preview_via: Vec<String>,
    /// True while `ManageCmd::Preview` is in flight.
    pub preview_loading: bool,
    /// Request number matching the pending `ManageEvent::Preview`.
    pub preview_request: Option<u64>,
    /// Fetched (or directory-seeded) preview summary.
    pub preview: Option<crate::matrix::RoomPreview>,
    /// Optional knock reason entered on the preview screen.
    pub preview_reason: LineEdit,
    /// True when the preview was opened from `Browse` so Cancel/Esc returns there.
    pub preview_from_browse: bool,
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

    pub const DIR_SEARCH: usize = 0;
    pub const DIR_SERVER: usize = 1;

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
            into_space: false,
            request: None,
            user_results: Vec::new(),
            user_selected: None,
            user_searching: false,
            user_search_gen: None,
            dir_query: LineEdit::new().with_max_bytes(FIELD_MAX),
            dir_server: LineEdit::new().with_max_bytes(FIELD_MAX),
            dir_default_server: None,
            dir_results: Vec::new(),
            dir_selected: 0,
            dir_scroll: 0,
            dir_page: 8,
            dir_reveal: false,
            dir_has_more: false,
            dir_searching: false,
            dir_loading_more: false,
            dir_search_gen: None,
            dir_error: None,
            preview_target: String::new(),
            preview_via: Vec::new(),
            preview_loading: false,
            preview_request: None,
            preview: None,
            preview_reason: LineEdit::new().with_max_bytes(FIELD_MAX),
            preview_from_browse: false,
        }
    }

    pub fn slots(&self) -> usize {
        match self.stage {
            Stage::Choose => Stage::CHOICES.len(),
            Stage::Create | Stage::CreateSpace => Self::BUTTON + 1,
            Stage::Browse => 2,
            Stage::Preview => usize::from(self.preview_has_reason()) + self.preview_buttons().len(),
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
            (Stage::Browse, Self::DIR_SEARCH) => Some(&mut self.dir_query),
            (Stage::Browse, Self::DIR_SERVER) => Some(&mut self.dir_server),
            (Stage::Preview, 0) if self.preview_has_reason() => Some(&mut self.preview_reason),
            (Stage::Create | Stage::CreateSpace, Self::NAME) => Some(&mut self.name),
            (Stage::Create | Stage::CreateSpace, Self::TOPIC) => Some(&mut self.topic),
            (Stage::Create | Stage::CreateSpace, Self::EXTRA) if self.public => {
                Some(&mut self.alias)
            }
            _ => None,
        }
    }

    /// Flips the switch in `slot`; false if there is none.
    pub fn flip(&mut self, slot: usize) -> bool {
        match (self.stage, slot) {
            (Stage::Create | Stage::CreateSpace, Self::ACCESS) => self.public = !self.public,
            (Stage::Create, Self::EXTRA) if !self.public => self.encrypted = !self.encrypted,
            _ => return false,
        }
        true
    }

    /// Whether the slot can take the cursor: a private space has nothing to say in
    /// the slot where a room has its encryption switch (spaces are not encrypted).
    pub fn live(&self, slot: usize) -> bool {
        !(self.stage == Stage::CreateSpace && slot == Self::EXTRA && !self.public)
    }

    pub fn step(&mut self, delta: isize) {
        let count = self.slots() as isize;
        if self.stage == Stage::Choose {
            self.choice = (self.choice as isize + delta).rem_euclid(count) as usize;
        } else {
            self.focus = (self.focus as isize + delta).rem_euclid(count) as usize;
            if !self.live(self.focus) {
                let past = if delta < 0 { -1 } else { 1 };
                self.focus = (self.focus as isize + past).rem_euclid(count) as usize;
            }
        }
    }

    pub fn go(&mut self, stage: Stage) {
        self.stage = stage;
        self.focus = 0;
        self.error = None;
        self.clear_user_search();
        if stage != Stage::Preview {
            self.preview_loading = false;
            self.preview_request = None;
            self.preview_from_browse = false;
        }
    }

    pub fn clear_user_search(&mut self) {
        self.user_results.clear();
        self.user_selected = None;
        self.user_searching = false;
        self.user_search_gen = None;
    }

    pub fn step_user_search(&mut self, delta: isize) {
        let len = self.user_results.len();
        if len == 0 {
            self.user_selected = None;
            return;
        }
        self.user_selected = match self.user_selected {
            None if delta > 0 => Some(0),
            None if delta < 0 => Some(len - 1),
            Some(i) if delta > 0 => Some((i + 1).min(len - 1)),
            Some(0) if delta < 0 => None,
            Some(i) if delta < 0 => Some(i - 1),
            other => other,
        };
    }

    pub fn clear_dir_search(&mut self) {
        self.dir_results.clear();
        self.dir_selected = 0;
        self.dir_scroll = 0;
        self.dir_has_more = false;
        self.dir_searching = false;
        self.dir_loading_more = false;
        self.dir_search_gen = None;
        self.dir_error = None;
    }

    pub fn step_dir(&mut self, delta: isize) {
        let len = self.dir_results.len();
        if len == 0 {
            self.dir_selected = 0;
            return;
        }
        let next = (self.dir_selected as isize + delta).clamp(0, len as isize - 1) as usize;
        if next != self.dir_selected {
            self.dir_selected = next;
            self.dir_reveal = true;
        }
    }

    pub fn scroll_dir(&mut self, lines: isize) {
        let max = self.dir_results.len().saturating_sub(self.dir_page.max(1));
        self.dir_scroll = (self.dir_scroll as isize + lines).clamp(0, max as isize) as usize;
        self.dir_reveal = false;
    }

    pub fn near_dir_end(&self) -> bool {
        self.dir_has_more
            && !self.dir_searching
            && !self.dir_loading_more
            && !self.dir_results.is_empty()
            && self.dir_selected.saturating_add(3) >= self.dir_results.len()
    }

    pub fn preview_buttons(&self) -> Vec<PreviewButton> {
        use crate::matrix::PreviewMembership;
        if let Some(preview) = &self.preview {
            match preview.membership {
                PreviewMembership::Joined => {
                    return vec![PreviewButton::Open, PreviewButton::Cancel];
                }
                PreviewMembership::Invited => {
                    return vec![PreviewButton::Accept, PreviewButton::Cancel];
                }
                PreviewMembership::Knocked => {
                    return vec![PreviewButton::Withdraw, PreviewButton::Cancel];
                }
                PreviewMembership::Banned => return vec![PreviewButton::Cancel],
                PreviewMembership::None => {}
            }
            let mut out = Vec::new();
            if preview.join_rule.can_join() {
                out.push(PreviewButton::Join);
            }
            if preview.join_rule.can_knock() {
                out.push(PreviewButton::Knock);
            }
            out.push(PreviewButton::Cancel);
            out
        } else if self.preview_loading {
            vec![PreviewButton::Cancel]
        } else {
            // Preview failed or offline: still let the user attempt a join/knock or cancel.
            vec![
                PreviewButton::Join,
                PreviewButton::Knock,
                PreviewButton::Cancel,
            ]
        }
    }

    pub fn preview_has_reason(&self) -> bool {
        self.preview_buttons().contains(&PreviewButton::Knock)
    }

    pub fn preview_button_at(&self, slot: usize) -> Option<PreviewButton> {
        let base = usize::from(self.preview_has_reason());
        slot.checked_sub(base)
            .and_then(|i| self.preview_buttons().get(i).copied())
    }

    pub fn default_preview_focus(&self) -> usize {
        if let Some(pos) = self
            .preview_buttons()
            .iter()
            .position(|b| *b != PreviewButton::Cancel)
        {
            usize::from(self.preview_has_reason()) + pos
        } else {
            usize::from(self.preview_has_reason())
        }
    }
}

// ----- "Room info" -------------------------------------------------------------------

/// Admin-editable or inspectable room settings inside the collapsible Room settings section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomSetting {
    JoinRule,
    History,
    Encryption,
    Picture,
    Addresses,
    Direct,
    Permissions,
}

impl RoomSetting {
    pub const ALL: [Self; 7] = [
        Self::JoinRule,
        Self::History,
        Self::Encryption,
        Self::Picture,
        Self::Addresses,
        Self::Direct,
        Self::Permissions,
    ];
}

/// A selectable row of the room info view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoRow {
    Favourite,
    LowPriority,
    Pinned,
    Notify,
    Name,
    Topic,
    Invite,
    OwnName,
    /// Pick a space to put this room into.
    AddToSpace,
    /// Take this room out of the space the room list is narrowed to.
    RemoveFromSpace,
    Leave,
    /// Index into the details' incoming knock request list.
    KnockRequest(usize),
    /// In a space: pick a room to put into it.
    AddChild,
    /// In a space: index into the details' list of its rooms.
    Child(usize),
    /// Collapsible `▸ Room settings` / `▾ Room settings` row right above `Members`.
    SettingsToggle,
    /// One setting row inside the expanded Room settings section.
    Setting(RoomSetting),
    /// Index into the details' member list.
    Member(usize),
}

/// One combined row in the Room Addresses subview.
#[derive(Clone, PartialEq, Eq)]
pub struct AddressEntry {
    pub alias: String,
    pub is_canonical: bool,
    pub is_alt: bool,
    pub is_local: bool,
}

impl std::fmt::Debug for AddressEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddressEntry")
            .field("is_canonical", &self.is_canonical)
            .field("is_alt", &self.is_alt)
            .field("is_local", &self.is_local)
            .finish_non_exhaustive()
    }
}

/// Combines canonical, alternate, and local homeserver aliases into a stable list.
pub fn address_entries(details: &RoomDetails) -> Vec<AddressEntry> {
    let mut entries = Vec::<AddressEntry>::new();
    let push =
        |entries: &mut Vec<AddressEntry>, alias: &str, canonical: bool, alt: bool, local: bool| {
            if let Some(existing) = entries.iter_mut().find(|e| e.alias == alias) {
                existing.is_canonical |= canonical;
                existing.is_alt |= alt;
                existing.is_local |= local;
            } else {
                entries.push(AddressEntry {
                    alias: alias.to_owned(),
                    is_canonical: canonical,
                    is_alt: alt,
                    is_local: local,
                });
            }
        };
    if let Some(main) = &details.canonical_alias {
        push(&mut entries, main, true, false, false);
    }
    for alt in &details.alt_aliases {
        push(&mut entries, alt, false, true, false);
    }
    for local in &details.local_aliases {
        push(&mut entries, local, false, false, true);
    }
    entries
}

/// Validates and expands `#name` or `#name:server` typed in the Addresses subview.
pub fn normalize_alias_input(raw: &str, own_homeserver: Option<&str>) -> Result<String, String> {
    let mut text = crate::text::sanitize_line(raw).trim().to_owned();
    if text.starts_with('<') && text.ends_with('>') && text.len() > 2 {
        text = text[1..text.len() - 1].trim().to_owned();
    }
    if text.is_empty() {
        return Err("Enter a room address like #name:server.".to_owned());
    }
    if text.chars().any(char::is_whitespace) {
        return Err("A room address cannot contain spaces.".to_owned());
    }
    if !text.starts_with('#') {
        text.insert(0, '#');
    }
    if !text.contains(':') {
        let Some(hs) = own_homeserver.filter(|s| !s.is_empty()) else {
            return Err("Include the homeserver, like #name:example.org.".to_owned());
        };
        text.push(':');
        text.push_str(hs);
    }
    RoomAliasId::parse(&text)
        .map(|a| a.to_string())
        .map_err(|_| "That is not a valid room address (use #name:server).".to_owned())
}

/// Actions exposed in the Room Permissions subview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermAction {
    SendMessages,
    Invite,
    Kick,
    Ban,
    Redact,
    ChangeSettings,
}

impl PermAction {
    pub const ALL: [Self; 6] = [
        Self::SendMessages,
        Self::Invite,
        Self::Kick,
        Self::Ban,
        Self::Redact,
        Self::ChangeSettings,
    ];

    pub const LEVELS: [(i64, &'static str); 3] = [
        (0, "Default (0)"),
        (50, "Moderator (50)"),
        (100, "Admin (100)"),
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::SendMessages => "Send messages",
            Self::Invite => "Invite members",
            Self::Kick => "Kick members",
            Self::Ban => "Ban members",
            Self::Redact => "Delete messages",
            Self::ChangeSettings => "Change settings",
        }
    }

    pub fn get(self, p: &RoomPermissions) -> i64 {
        match self {
            Self::SendMessages => p.send_messages,
            Self::Invite => p.invite,
            Self::Kick => p.kick,
            Self::Ban => p.ban,
            Self::Redact => p.redact,
            Self::ChangeSettings => p.change_settings,
        }
    }

    pub fn set(self, p: &mut RoomPermissions, value: i64) {
        match self {
            Self::SendMessages => p.send_messages = value,
            Self::Invite => p.invite = value,
            Self::Kick => p.kick = value,
            Self::Ban => p.ban = value,
            Self::Redact => p.redact = value,
            Self::ChangeSettings => p.change_settings = value,
        }
    }
}

/// Checks power-level cap and non-creator self-lockout before sending permission updates.
pub fn validate_permissions_draft(
    details: &RoomDetails,
    draft: &RoomPermissions,
) -> Result<(), String> {
    if !details.settings_power.can_permissions {
        return Err("You do not have permission to edit room permissions.".to_owned());
    }
    for action in PermAction::ALL {
        let value = action.get(draft);
        if value != action.get(&details.permissions)
            && !details.own_is_creator
            && value > details.own_power_level
        {
            return Err(format!(
                "You cannot set a permission above your own level ({}).",
                details.own_power_level
            ));
        }
    }
    let effective_pl_req = if details.permissions.explicit_power_levels_rule {
        details.permissions.power_levels_event_level
    } else {
        draft.change_settings
    };
    if !details.own_is_creator && effective_pl_req > details.own_power_level {
        return Err("That change would lock you out of editing room permissions.".to_owned());
    }
    Ok(())
}

/// Interactive Room Settings subview opened from an [`InfoRow::Setting`].
#[derive(Clone)]
pub enum SettingSubview {
    JoinRule {
        selected: usize,
    },
    History {
        selected: usize,
    },
    EncryptConfirm {
        confirm: LineEdit,
        focus: usize,
        error: Option<String>,
    },
    Picture {
        path: LineEdit,
        focus: usize,
        error: Option<String>,
        notice: Option<String>,
    },
    Addresses {
        input: LineEdit,
        focus: usize,
        selected: usize,
        confirm_delete: Option<String>,
        error: Option<String>,
        notice: Option<String>,
    },
    Permissions {
        draft: RoomPermissions,
        selected: usize,
        error: Option<String>,
    },
}

impl std::fmt::Debug for SettingSubview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::JoinRule { .. } => "SettingSubview::JoinRule",
            Self::History { .. } => "SettingSubview::History",
            Self::EncryptConfirm { .. } => "SettingSubview::EncryptConfirm",
            Self::Picture { .. } => "SettingSubview::Picture",
            Self::Addresses { .. } => "SettingSubview::Addresses",
            Self::Permissions { .. } => "SettingSubview::Permissions",
        })
    }
}

/// What the user was asked to confirm by doing it a second time.
#[derive(Clone, PartialEq, Eq)]
pub enum Confirm {
    Leave,
    /// Remove the member with this user id.
    Remove(String),
    /// Decline the incoming knock request from this user id.
    DeclineKnock(String),
    /// Unban the member with this user id.
    Unban(String),
    /// Ignore the member with this user id.
    Ignore(String),
    /// Stop ignoring the member with this user id.
    Unignore(String),
    /// There is no direct chat with this user yet: create one and invite them.
    NewChat(String),
    /// Take this room out of the space the room list is narrowed to.
    RemoveFromSpace,
    /// Take the room with this id out of this space.
    RemoveChild(String),
}

/// Hand-written: who it is about stays out of logs.
impl std::fmt::Debug for Confirm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Leave => "Confirm::Leave",
            Self::Remove(_) => "Confirm::Remove",
            Self::DeclineKnock(_) => "Confirm::DeclineKnock",
            Self::Unban(_) => "Confirm::Unban",
            Self::Ignore(_) => "Confirm::Ignore",
            Self::Unignore(_) => "Confirm::Unignore",
            Self::NewChat(_) => "Confirm::NewChat",
            Self::RemoveFromSpace => "Confirm::RemoveFromSpace",
            Self::RemoveChild(_) => "Confirm::RemoveChild",
        })
    }
}

/// Inline reason prompt for banning the selected member or knocker inside room info.
#[derive(Clone)]
pub struct BanPrompt {
    pub user_id: String,
    pub name: String,
    pub reason: LineEdit,
    pub knock: bool,
}

impl BanPrompt {
    pub fn new(user_id: String, name: String) -> Self {
        Self {
            user_id,
            name,
            reason: LineEdit::new().with_max_bytes(256),
            knock: false,
        }
    }

    pub fn for_knock(user_id: String, name: String) -> Self {
        Self {
            knock: true,
            ..Self::new(user_id, name)
        }
    }
}

impl std::fmt::Debug for BanPrompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BanPrompt({} reason bytes)", self.reason.as_str().len())
    }
}

/// Inline role picker for a member row (`Admin` / `Moderator` / `Default`).
#[derive(Clone)]
pub struct RoleChooser {
    pub user_id: String,
    pub name: String,
    pub current_level: i64,
    pub is_self: bool,
    pub admin_count: usize,
    pub options: Vec<(i64, &'static str)>,
    pub selected: usize,
    pub confirm_last_admin: bool,
}

impl std::fmt::Debug for RoleChooser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RoleChooser(current: {}, self: {}, admins: {}, confirm_last: {})",
            self.current_level, self.is_self, self.admin_count, self.confirm_last_admin
        )
    }
}

impl RoleChooser {
    pub fn new(
        member: &RoomMember,
        own_is_creator: bool,
        own_level: i64,
        admin_count: usize,
    ) -> Self {
        let options: Vec<(i64, &'static str)> = [(100, "Admin"), (50, "Moderator"), (0, "Default")]
            .into_iter()
            .filter(|(lvl, _)| own_is_creator || own_level >= *lvl)
            .collect();
        let selected = options
            .iter()
            .position(|(lvl, _)| *lvl == member.power_level)
            .unwrap_or(0);
        Self {
            user_id: member.user_id.clone(),
            name: member.name.clone(),
            current_level: member.power_level,
            is_self: member.own,
            admin_count,
            options,
            selected,
            confirm_last_admin: false,
        }
    }

    pub fn step(&mut self, delta: isize) {
        if self.options.is_empty() {
            return;
        }
        let max = (self.options.len() - 1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, max) as usize;
        self.confirm_last_admin = false;
    }

    pub fn chosen(&self) -> Option<(i64, &'static str)> {
        self.options.get(self.selected).copied()
    }

    pub fn select(&mut self, index: usize) -> bool {
        if index < self.options.len() {
            self.selected = index;
            self.confirm_last_admin = false;
            true
        } else {
            false
        }
    }

    pub fn needs_last_admin_confirm(&self, new_level: i64) -> bool {
        self.is_self && self.current_level >= 100 && new_level < 100 && self.admin_count <= 1
    }
}

/// The answer the room info view is waiting for. Any other answer leaves it alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// A direct chat to open, by the number of the request.
    Chat(u64),
    Change(crate::matrix::Change),
    Leave,
    /// A change of a space, by the number of the request.
    Space(u64),
}

/// A space as the room info of a room in it needs it: the one the room list is
/// narrowed to.
#[derive(Clone, PartialEq, Eq)]
pub struct SpaceRef {
    pub id: String,
    pub name: String,
    /// We may take rooms out of it.
    pub can_edit: bool,
}

impl std::fmt::Debug for SpaceRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SpaceRef(can_edit: {})", self.can_edit)
    }
}

#[derive(Clone)]
pub struct InfoPopup {
    pub room_id: String,
    /// Name and kind as the room list has them, until the details arrive.
    pub name: String,
    pub kind: RoomKind,
    /// The room is a space: its rooms are listed, and what only fits a chat is left out.
    pub space: bool,
    /// The space the room list is narrowed to, if this room is directly in it.
    pub in_space: Option<SpaceRef>,
    /// There is a space this room could be put into.
    pub can_add_to_space: bool,
    pub details: Option<RoomDetails>,
    pub error: Option<String>,
    pub notice: Option<String>,
    /// Narrows the member list.
    pub filter: LineEdit,
    /// The filter was typed last and matches nobody, so the selection is just where
    /// it was left: until a row is chosen, the filter's line is what is in view and
    /// the keys for a row go to the filter.
    pub blind: bool,
    /// Index into `Self::rows`.
    pub selected: usize,
    /// First line shown, and how many fit (as of the last draw).
    pub scroll: usize,
    pub page: usize,
    /// Ask the next draw to bring the selected row into view.
    pub reveal: bool,
    /// Whether the collapsible Room settings section above Members is expanded.
    pub settings_open: bool,
    /// Active Room Settings sub-dialog (JoinRule / History / EncryptConfirm / Picture / Addresses / Permissions).
    pub setting_view: Option<SettingSubview>,
    /// The name or topic while it is being typed.
    pub editing: Option<(InfoRow, LineEdit)>,
    /// Inline reason prompt for banning a member.
    pub ban_prompt: Option<BanPrompt>,
    /// Inline role chooser on a member row.
    pub role_chooser: Option<RoleChooser>,
    pub confirm: Option<Confirm>,
    /// A change is on its way to the server.
    pub busy: Option<&'static str>,
    /// Which answer ends `busy`.
    pub pending: Option<Pending>,
    /// A row to select once it is there: some only exist when the details are in.
    pub wanted: Option<InfoRow>,
}

/// Hand-written: which room, names and topics stay out of logs.
impl std::fmt::Debug for InfoPopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InfoPopup")
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
            space: false,
            in_space: None,
            can_add_to_space: false,
            details: None,
            error: None,
            notice: None,
            filter: LineEdit::new().with_max_bytes(FIELD_MAX),
            blind: false,
            selected: 0,
            scroll: 0,
            page: 10,
            reveal: true,
            settings_open: false,
            setting_view: None,
            editing: None,
            ban_prompt: None,
            role_chooser: None,
            confirm: None,
            busy: None,
            pending: None,
            wanted: None,
        }
    }

    /// The info of a space.
    pub fn for_space(space_id: &str, name: &str) -> Self {
        Self {
            space: true,
            ..Self::new(space_id, name, RoomKind::Room)
        }
    }

    /// What the room list says about this room and the spaces. A question about a
    /// space the room is no longer in, or that we may no longer change, is withdrawn;
    /// the selection stays on its row, or goes to the one above where the row is gone.
    pub fn set_space_context(&mut self, in_space: Option<SpaceRef>, can_add: bool) {
        if self.in_space == in_space && self.can_add_to_space == can_add {
            return;
        }
        let row = self.current();
        let still_asked = matches!(
            (&in_space, &self.in_space),
            (Some(new), Some(old)) if new.id == old.id && new.can_edit
        );
        if !still_asked && self.confirm == Some(Confirm::RemoveFromSpace) {
            self.confirm = None;
        }
        self.in_space = in_space;
        self.can_add_to_space = can_add;
        let rows = self.rows();
        self.selected = match row.and_then(|row| rows.iter().position(|r| *r == row)) {
            Some(at) => at,
            None => self
                .selected
                .saturating_sub(1)
                .min(rows.len().saturating_sub(1)),
        };
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

    pub fn knocked(&self) -> bool {
        self.kind == RoomKind::Knock
    }

    pub fn pending(&self) -> bool {
        self.kind.is_pending()
    }

    pub fn direct(&self) -> bool {
        !self.space
            && self
                .details
                .as_ref()
                .map_or(self.kind == RoomKind::Person, |details| details.direct)
    }

    /// The settings that apply: a space has no messages to encrypt and is no chat.
    pub fn settings(&self) -> Vec<RoomSetting> {
        RoomSetting::ALL
            .into_iter()
            .filter(|setting| {
                !self.space || !matches!(setting, RoomSetting::Encryption | RoomSetting::Direct)
            })
            .collect()
    }

    /// The rooms of the space, if this is one and they are known.
    pub fn children(&self) -> &[SpaceChild] {
        self.details
            .as_ref()
            .and_then(|details| details.space.as_ref())
            .map_or(&[], |space| space.children.as_slice())
    }

    /// We may put rooms into this space and take them out.
    pub fn can_edit_children(&self) -> bool {
        self.details
            .as_ref()
            .and_then(|details| details.space.as_ref())
            .is_some_and(|space| space.can_edit)
    }

    pub fn child(&self, row: InfoRow) -> Option<&SpaceChild> {
        match row {
            InfoRow::Child(index) => self.children().get(index),
            _ => None,
        }
    }

    pub fn selected_child(&self) -> Option<&SpaceChild> {
        self.child(self.current()?)
    }

    /// The selectable rows, top to bottom: what can be done, then the members that
    /// pass the filter.
    pub fn rows(&self) -> Vec<InfoRow> {
        let mut rows = Vec::new();
        if !self.pending() {
            // A space is not in the room list and has no messages of its own.
            if !self.space {
                rows.push(InfoRow::Favourite);
                rows.push(InfoRow::LowPriority);
                rows.push(InfoRow::Pinned);
            }
            if let Some(details) = &self.details {
                if details.notify.is_some() && !self.space {
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
                if !self.space {
                    rows.push(InfoRow::OwnName);
                }
            }
            if self.can_add_to_space {
                rows.push(InfoRow::AddToSpace);
            }
            if self.in_space.is_some() {
                rows.push(InfoRow::RemoveFromSpace);
            }
        }
        rows.push(InfoRow::Leave);
        if !self.pending()
            && let Some(details) = &self.details
        {
            rows.extend((0..details.knock_requests.len()).map(InfoRow::KnockRequest));
            if let Some(space) = &details.space {
                rows.push(InfoRow::AddChild);
                rows.extend((0..space.children.len()).map(InfoRow::Child));
            }
            rows.push(InfoRow::SettingsToggle);
            if self.settings_open {
                rows.extend(self.settings().into_iter().map(InfoRow::Setting));
            }
        }
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

    pub fn knock_request(&self, row: InfoRow) -> Option<&RoomKnockRequest> {
        match row {
            InfoRow::KnockRequest(index) => self.details.as_ref()?.knock_requests.get(index),
            _ => None,
        }
    }

    pub fn selected_knock_request(&self) -> Option<&RoomKnockRequest> {
        self.knock_request(self.current()?)
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
                self.ban_prompt = None;
                self.role_chooser = None;
            }
            self.selected = at;
            self.blind = false;
            self.reveal = true;
        } else if self.details.is_none() {
            self.wanted = Some(row);
        }
    }

    pub fn step(&mut self, delta: isize) {
        self.confirm = None;
        self.notice = None;
        self.ban_prompt = None;
        self.role_chooser = None;
        // The user has chosen for themselves.
        self.wanted = None;
        self.blind = false;
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
        self.ban_prompt = None;
        self.role_chooser = None;
        let rows = self.rows();
        let wanted = if self.filter.is_empty() {
            // Emptied: the selection stays where it was.
            rows.iter().position(|r| Some(*r) == before)
        } else {
            rows.iter().position(|r| matches!(r, InfoRow::Member(_)))
        };
        self.blind = !self.filter.is_empty() && wanted.is_none();
        self.selected = wanted.unwrap_or(self.selected.min(rows.len().saturating_sub(1)));
        self.reveal = true;
    }

    /// Whether the filter's line is in view rather than the selected row: see `blind`.
    pub fn typing_blind(&self) -> bool {
        self.blind
            && !self.filter.is_empty()
            && self.editing.is_none()
            && !matches!(self.current(), Some(InfoRow::Member(_)))
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
        let knock = row
            .and_then(|row| self.knock_request(row))
            .map(|req| req.user_id.clone());
        let child = row
            .and_then(|row| self.child(row))
            .map(|child| child.room_id.clone());
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
            Some(Confirm::DeclineKnock(user_id)) => details
                .knock_requests
                .iter()
                .any(|k| k.user_id == *user_id && k.declinable),
            Some(Confirm::Unban(user_id)) => details
                .members
                .iter()
                .any(|m| m.user_id == *user_id && m.unbannable),
            Some(
                Confirm::Ignore(user_id) | Confirm::Unignore(user_id) | Confirm::NewChat(user_id),
            ) => still(user_id, false),
            // Only while it is in the space and we may still take it out.
            Some(Confirm::RemoveChild(room_id)) => details.space.as_ref().is_some_and(|space| {
                space.can_edit && space.children.iter().any(|c| c.room_id == *room_id)
            }),
            Some(Confirm::Leave | Confirm::RemoveFromSpace) | None => true,
        };
        if !keep {
            self.confirm = None;
        }
        if let Some(prompt) = &self.ban_prompt {
            let is_knock = prompt.knock;
            let valid = if is_knock {
                details
                    .knock_requests
                    .iter()
                    .any(|k| k.user_id == prompt.user_id && k.bannable)
            } else {
                details
                    .members
                    .iter()
                    .any(|m| m.user_id == prompt.user_id && m.bannable)
            };
            if !valid {
                self.ban_prompt = None;
                self.error = Some(if is_knock {
                    "You are no longer allowed to ban this user.".to_owned()
                } else {
                    "You are no longer allowed to ban this member.".to_owned()
                });
            }
        }
        if let Some(chooser) = &mut self.role_chooser {
            if details
                .members
                .iter()
                .any(|m| m.user_id == chooser.user_id && m.role_editable)
            {
                chooser.admin_count = details.admin_count;
            } else {
                self.role_chooser = None;
                self.error = Some("You are no longer allowed to change this role.".to_owned());
            }
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
        match &mut self.setting_view {
            Some(SettingSubview::JoinRule { .. }) if !details.settings_power.can_join_rule => {
                self.setting_view = None;
                self.error = Some("You are no longer allowed to change who can join.".to_owned());
            }
            Some(SettingSubview::History { .. }) if !details.settings_power.can_history => {
                self.setting_view = None;
                self.error =
                    Some("You are no longer allowed to change history visibility.".to_owned());
            }
            Some(SettingSubview::EncryptConfirm { .. }) if details.encrypted == Some(true) => {
                self.setting_view = None;
                self.notice = Some("Encryption is already enabled in this room.".to_owned());
            }
            Some(SettingSubview::EncryptConfirm { .. }) if !details.settings_power.can_encrypt => {
                self.setting_view = None;
                self.error = Some("You are no longer allowed to turn on encryption.".to_owned());
            }
            Some(SettingSubview::Picture { .. }) if !details.settings_power.can_avatar => {
                self.setting_view = None;
                self.error =
                    Some("You are no longer allowed to change the room picture.".to_owned());
            }
            Some(SettingSubview::Addresses {
                selected,
                confirm_delete,
                ..
            }) => {
                let entries = address_entries(details);
                *selected = (*selected).min(entries.len().saturating_sub(1));
                if let Some(target) = confirm_delete.as_ref()
                    && !entries.iter().any(|e| e.alias == *target && e.is_local)
                {
                    *confirm_delete = None;
                }
            }
            Some(SettingSubview::Permissions { draft, .. })
                if !details.settings_power.can_permissions =>
            {
                *draft = details.permissions;
            }
            _ => {}
        }
        let rows = self.rows();
        let same = match (&member, &knock, &child, row) {
            (Some(user_id), _, _, _) => rows.iter().position(
                |r| matches!(self.member(*r), Some(member) if member.user_id == *user_id),
            ),
            (_, Some(user_id), _, _) => rows.iter().position(
                |r| matches!(self.knock_request(*r), Some(req) if req.user_id == *user_id),
            ),
            (_, _, Some(room_id), _) => rows
                .iter()
                .position(|r| matches!(self.child(*r), Some(child) if child.room_id == *room_id)),
            (None, None, None, Some(row)) => rows.iter().position(|r| *r == row),
            (None, None, None, None) => None,
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
        } else if self.knocked() {
            "Withdraw request"
        } else if self.direct() {
            "Leave this chat"
        } else if self.space {
            "Leave space"
        } else {
            "Leave room"
        }
    }

    /// The question before leaving: the keys first, so that a narrow window cuts the
    /// explanation and not them. `Self::confirm_keys` is the part that must fit.
    pub fn leave_confirm(&self) -> &'static str {
        if self.invited() {
            "Enter again to decline, Esc to keep it"
        } else if self.knocked() {
            "Enter again to withdraw, Esc to keep waiting"
        } else if self.space {
            "Enter again to leave, Esc to stay · the rooms in it are not left"
        } else {
            "Enter again to leave, Esc to stay · this device forgets the room"
        }
    }

    /// The room list's view of the room changed (an invitation was accepted, say).
    /// Returns whether the details have to be asked for again.
    pub fn set_kind(&mut self, kind: RoomKind) -> bool {
        let was_pending = self.pending();
        self.kind = kind;
        if was_pending != self.pending() {
            self.confirm = None;
            self.editing = None;
            self.ban_prompt = None;
            self.role_chooser = None;
            self.setting_view = None;
            self.settings_open = false;
            self.notice = None;
            self.selected = 0;
            self.reveal = true;
            return true;
        }
        false
    }

    /// Whether the viewer can edit `setting` given cached details and power levels.
    pub fn setting_editable(&self, setting: RoomSetting) -> bool {
        let Some(details) = &self.details else {
            return false;
        };
        match setting {
            RoomSetting::JoinRule => {
                details.settings_power.can_join_rule
                    && details
                        .join_rule
                        .is_none_or(|rule| rule.to_ruma().is_some())
            }
            RoomSetting::History => details.settings_power.can_history,
            RoomSetting::Encryption => {
                details.encrypted == Some(false) && details.settings_power.can_encrypt
            }
            RoomSetting::Picture => details.settings_power.can_avatar,
            RoomSetting::Addresses => details.settings_power.can_aliases,
            RoomSetting::Direct => true,
            RoomSetting::Permissions => details.settings_power.can_permissions,
        }
    }
}

/// The part of a confirmation that names the keys: `prompt` up to its first `·`.
pub fn confirm_keys(prompt: &str) -> &str {
    prompt.split('·').next().unwrap_or(prompt).trim_end()
}

/// Formats a member's power level for display: `owner`/`admin`/`moderator` for standard
/// levels, empty for `0` (default), and the exact integer string for custom levels.
pub fn member_role_display(role: MemberRole, power_level: i64) -> std::borrow::Cow<'static, str> {
    use std::borrow::Cow;
    match role {
        MemberRole::Owner => Cow::Borrowed("owner"),
        _ if power_level == 100 => Cow::Borrowed("admin"),
        _ if power_level == 50 => Cow::Borrowed("moderator"),
        _ if power_level == 0 => Cow::Borrowed(""),
        _ => Cow::Owned(power_level.to_string()),
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
                    ..RoomMember::default()
                })
                .collect(),
            ..RoomDetails::default()
        }
    }

    #[test]
    fn info_rows_follow_permissions_and_the_filter() {
        let mut popup = InfoPopup::new("!r:example.org", "Rats", RoomKind::Room);
        assert_eq!(
            popup.rows(),
            [
                InfoRow::Favourite,
                InfoRow::LowPriority,
                InfoRow::Pinned,
                InfoRow::Leave
            ]
        );
        popup.set_details(details(&[
            ("alice", false),
            ("bob", false),
            ("carol", true),
        ]));
        assert_eq!(popup.rows().len(), 10 + 3);
        popup.set_filter("BO".into());
        assert_eq!(popup.rows().len(), 10 + 1);
        assert_eq!(popup.selected_member().unwrap().name, "bob");
        // Emptying the filter keeps the member selected, wherever it is now.
        popup.set_filter(String::new());
        assert_eq!(popup.selected_member().unwrap().name, "bob");
        popup.set_filter("zzz".into());
        assert_eq!(popup.current(), Some(InfoRow::SettingsToggle));
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
                InfoRow::LowPriority,
                InfoRow::Pinned,
                InfoRow::Notify,
                InfoRow::OwnName,
                InfoRow::Leave,
                InfoRow::SettingsToggle,
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

    fn child(id: &str) -> SpaceChild {
        SpaceChild {
            room_id: id.to_owned(),
            name: id.trim_start_matches('!').to_owned(),
            joined: true,
            ..SpaceChild::default()
        }
    }

    fn space_details(children: &[&str], can_edit: bool) -> RoomDetails {
        RoomDetails {
            space: Some(crate::matrix::SpaceDetails {
                children: children.iter().map(|id| child(id)).collect(),
                more: false,
                can_edit,
            }),
            ..details(&[("alice", false)])
        }
    }

    #[test]
    fn a_space_keeps_the_selection_on_its_room_and_drops_questions_that_no_longer_apply() {
        let mut popup = InfoPopup::for_space("!s:example.org", "Space");
        assert_eq!(popup.leave_label(), "Leave space");
        popup.set_details(space_details(&["!a", "!b"], true));
        popup.select(InfoRow::Child(1));
        popup.confirm = Some(Confirm::RemoveChild("!b".into()));
        // A room added in front: the selection and the question stay with "!b".
        popup.set_details(space_details(&["!new", "!a", "!b"], true));
        assert_eq!(popup.selected_child().unwrap().room_id, "!b");
        assert_eq!(popup.confirm, Some(Confirm::RemoveChild("!b".into())));
        // The right is gone: so is the question.
        popup.set_details(space_details(&["!new", "!a", "!b"], false));
        assert!(popup.confirm.is_none());
        popup.confirm = Some(Confirm::RemoveChild("!b".into()));
        popup.set_details(space_details(&["!new", "!a"], true));
        assert!(popup.confirm.is_none());

        // A room's info: the question about its space goes when the space does.
        let mut room = InfoPopup::new("!r:example.org", "Rats", RoomKind::Room);
        let work = SpaceRef {
            id: "!work:example.org".into(),
            name: "Work".into(),
            can_edit: true,
        };
        room.set_space_context(Some(work.clone()), true);
        room.select(InfoRow::RemoveFromSpace);
        room.confirm = Some(Confirm::RemoveFromSpace);
        room.set_space_context(Some(work.clone()), false);
        assert_eq!(room.confirm, Some(Confirm::RemoveFromSpace));
        assert_eq!(room.current(), Some(InfoRow::RemoveFromSpace));
        // Nor while we may no longer change the space.
        let read_only = SpaceRef {
            can_edit: false,
            ..work
        };
        room.set_space_context(Some(read_only), false);
        assert!(room.confirm.is_none());
        assert_eq!(room.current(), Some(InfoRow::RemoveFromSpace));
        room.confirm = Some(Confirm::RemoveFromSpace);
        room.set_space_context(None, false);
        assert!(room.confirm.is_none());
        // Its row is gone; the one above it is selected, not Leave.
        assert_ne!(room.current(), Some(InfoRow::Leave));
        assert!(
            !format!(
                "{:?}",
                SpaceRef {
                    id: "!secret:example.org".into(),
                    name: "Secret".into(),
                    can_edit: true
                }
            )
            .contains("ecret")
        );
    }

    #[test]
    fn a_private_space_has_no_slot_for_encryption() {
        let mut form = NewPopup::new(Stage::CreateSpace);
        form.focus = NewPopup::ACCESS;
        form.step(1);
        assert_eq!(form.focus, NewPopup::BUTTON);
        form.step(-1);
        assert_eq!(form.focus, NewPopup::ACCESS);
        assert!(!form.flip(NewPopup::EXTRA));
        assert!(form.flip(NewPopup::ACCESS));
        form.step(1);
        assert_eq!(form.focus, NewPopup::EXTRA);
        assert!(form.field_mut().is_some());
    }

    #[test]
    fn form_state() {
        let mut form = NewPopup::new(Stage::Choose);
        assert!(!form.single && form.field_mut().is_none());
        form.step(-1);
        assert_eq!(form.choice, Stage::CHOICES.len() - 1);
        assert_eq!(Stage::CHOICES[form.choice], Stage::Browse);
        form.step(-2);
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
        assert_eq!(
            popup.rows(),
            [
                InfoRow::Favourite,
                InfoRow::LowPriority,
                InfoRow::Pinned,
                InfoRow::Leave
            ]
        );
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

    #[test]
    fn role_display_and_moderation_prompts_cover_custom_levels_guard_and_redacted_debug() {
        assert_eq!(member_role_display(MemberRole::Owner, i64::MAX), "owner");
        assert_eq!(member_role_display(MemberRole::Admin, 100), "admin");
        assert_eq!(member_role_display(MemberRole::Moderator, 50), "moderator");
        assert_eq!(member_role_display(MemberRole::User, 0), "");
        assert_eq!(member_role_display(MemberRole::Moderator, 75), "75");
        assert_eq!(member_role_display(MemberRole::User, -5), "-5");

        let custom_mod = RoomMember {
            user_id: "@alice:example.org".into(),
            name: "Alice".into(),
            role: MemberRole::Moderator,
            power_level: 75,
            own: true,
            role_editable: true,
            ..RoomMember::default()
        };
        let mut chooser = RoleChooser::new(&custom_mod, false, 80, 1);
        assert_eq!(chooser.options, [(50, "Moderator"), (0, "Default")]);
        assert!(!chooser.needs_last_admin_confirm(50));
        assert!(chooser.select(1));
        assert_eq!(chooser.chosen(), Some((0, "Default")));

        let sole_admin = RoomMember {
            user_id: "@alice:example.org".into(),
            name: "Alice".into(),
            role: MemberRole::Admin,
            power_level: 100,
            own: true,
            role_editable: true,
            ..RoomMember::default()
        };
        let self_admin_chooser = RoleChooser::new(&sole_admin, false, 100, 1);
        assert!(self_admin_chooser.needs_last_admin_confirm(50));
        assert!(!self_admin_chooser.needs_last_admin_confirm(100));

        let other = RoomMember {
            user_id: "@bob:example.org".into(),
            name: "Bob".into(),
            role: MemberRole::Admin,
            power_level: 100,
            own: false,
            role_editable: true,
            ..RoomMember::default()
        };
        let chooser_other = RoleChooser::new(&other, true, 100, 1);
        assert!(!chooser_other.needs_last_admin_confirm(50));

        let mut ban = BanPrompt::new("@bob:example.org".into(), "Bob Secret".into());
        ban.reason.set("spamming links");
        let debug = format!(
            "{:?} {:?} {:?}",
            ban,
            chooser_other,
            Confirm::Unban("@bob:example.org".into())
        );
        for secret in ["bob", "Bob", "Secret", "spamming", "alice"] {
            assert!(!debug.contains(secret), "{debug}");
        }
    }

    #[test]
    fn knock_requests_rows_selection_and_permission_guards() {
        let mut popup = InfoPopup::new("!r:example.org", "Rats", RoomKind::Room);
        let alice = RoomKnockRequest {
            user_id: "@alice:example.org".into(),
            name: "Alice".into(),
            reason: Some("secret reason".into()),
            acceptable: true,
            declinable: true,
            bannable: true,
        };
        let bob = RoomKnockRequest {
            user_id: "@bob:example.org".into(),
            name: "Bob".into(),
            reason: None,
            acceptable: true,
            declinable: true,
            bannable: true,
        };
        let mut d = details(&[("mod", false)]);
        d.knock_requests = vec![alice.clone(), bob.clone()];
        popup.set_details(d);

        // Knock requests appear above Members and remain visible when filtering members.
        popup.set_filter("zzz".to_owned());
        let rows = popup.rows();
        assert!(rows.contains(&InfoRow::KnockRequest(0)));
        assert!(rows.contains(&InfoRow::KnockRequest(1)));
        assert!(!rows.iter().any(|r| matches!(r, InfoRow::Member(_))));

        // Select Bob (index 1); removing Alice shifts Bob to index 0, keeping Bob selected.
        popup.select(InfoRow::KnockRequest(1));
        assert_eq!(
            popup.selected_knock_request().map(|k| k.user_id.as_str()),
            Some("@bob:example.org")
        );
        popup.confirm = Some(Confirm::DeclineKnock("@bob:example.org".into()));
        popup.ban_prompt = Some(BanPrompt::for_knock(
            "@bob:example.org".into(),
            "Bob".into(),
        ));
        let dbg = format!(
            "{:?} {:?}",
            popup.confirm.as_ref().unwrap(),
            popup.ban_prompt.as_ref().unwrap()
        );
        assert!(!dbg.contains("bob") && !dbg.contains("Bob"), "{dbg}");

        let mut d2 = details(&[("mod", false)]);
        let mut bob_no_ban = bob;
        bob_no_ban.declinable = false;
        bob_no_ban.bannable = false;
        d2.knock_requests = vec![bob_no_ban];
        popup.set_details(d2);
        assert_eq!(popup.current(), Some(InfoRow::KnockRequest(0)));
        assert_eq!(popup.confirm, None);
        assert!(popup.ban_prompt.is_none());
        assert!(
            popup
                .error
                .as_deref()
                .is_some_and(|e| e.contains("no longer allowed to ban"))
        );
    }

    #[test]
    fn address_entries_normalize_and_permissions_validation() {
        let mut d = details(&[("admin", false)]);
        d.canonical_alias = Some("#main:example.org".into());
        d.alt_aliases = vec!["#alt:example.org".into(), "#main:example.org".into()];
        d.local_aliases = vec!["#main:example.org".into(), "#local-only:example.org".into()];
        let entries = address_entries(&d);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].alias, "#main:example.org");
        assert!(entries[0].is_canonical && entries[0].is_alt && entries[0].is_local);
        assert_eq!(entries[1].alias, "#alt:example.org");
        assert!(!entries[1].is_canonical && entries[1].is_alt && !entries[1].is_local);
        assert_eq!(entries[2].alias, "#local-only:example.org");
        assert!(!entries[2].is_canonical && !entries[2].is_alt && entries[2].is_local);

        assert_eq!(
            normalize_alias_input("chat", Some("example.org")).as_deref(),
            Ok("#chat:example.org")
        );
        assert_eq!(
            normalize_alias_input(" #other:matrix.org ", Some("example.org")).as_deref(),
            Ok("#other:matrix.org")
        );
        assert!(normalize_alias_input("chat", None).is_err());
        assert!(normalize_alias_input("   ", Some("example.org")).is_err());
        assert!(normalize_alias_input("bad alias", Some("example.org")).is_err());

        // Permissions draft validation: non-creator level cap & self-lockout guard.
        let mut perm_details = details(&[("mod", false)]);
        perm_details.own_power_level = 50;
        perm_details.settings_power.can_permissions = true;
        perm_details.permissions = RoomPermissions {
            change_settings: 50,
            power_levels_event_level: 50,
            explicit_power_levels_rule: false,
            ..RoomPermissions::default()
        };
        let mut too_high = perm_details.permissions;
        too_high.ban = 100;
        assert!(validate_permissions_draft(&perm_details, &too_high).is_err());

        // Without permission, validation refuses.
        perm_details.settings_power.can_permissions = false;
        assert!(validate_permissions_draft(&perm_details, &perm_details.permissions).is_err());

        // Self-lockout when raising `change_settings` above own level even with lower PL cap check.
        perm_details.settings_power.can_permissions = true;
        perm_details.own_power_level = 75;
        let mut ok_mod = perm_details.permissions;
        ok_mod.ban = 50;
        assert!(validate_permissions_draft(&perm_details, &ok_mod).is_ok());

        // Creator bypasses numeric limits.
        perm_details.own_is_creator = true;
        perm_details.own_power_level = i64::MAX;
        assert!(validate_permissions_draft(&perm_details, &too_high).is_ok());
    }

    #[test]
    fn room_settings_rows_debug_redaction_and_permission_withdrawal() {
        use crate::matrix::{RoomJoinRule, RoomSettingsPower};

        let mut popup = InfoPopup::new("!r:example.org", "Rats", RoomKind::Room);
        let mut d = details(&[("admin", false)]);
        d.encrypted = Some(false);
        d.join_rule = Some(RoomJoinRule::Invite);
        d.settings_power = RoomSettingsPower {
            can_join_rule: true,
            can_history: true,
            can_encrypt: true,
            can_avatar: true,
            can_aliases: true,
            can_permissions: true,
        };
        d.local_aliases = vec!["#secret-alias:example.org".into()];
        popup.set_details(d.clone());

        let collapsed = popup.rows();
        let st_idx = collapsed
            .iter()
            .position(|r| *r == InfoRow::SettingsToggle)
            .unwrap();
        let mem_idx = collapsed
            .iter()
            .position(|r| *r == InfoRow::Member(0))
            .unwrap();
        assert_eq!(st_idx + 1, mem_idx);
        assert!(!collapsed.iter().any(|r| matches!(r, InfoRow::Setting(_))));

        popup.settings_open = true;
        let expanded = popup.rows();
        let exp_st = expanded
            .iter()
            .position(|r| *r == InfoRow::SettingsToggle)
            .unwrap();
        for (i, s) in RoomSetting::ALL.iter().enumerate() {
            assert_eq!(expanded[exp_st + 1 + i], InfoRow::Setting(*s));
        }
        assert_eq!(
            expanded[exp_st + 1 + RoomSetting::ALL.len()],
            InfoRow::Member(0)
        );

        assert!(popup.setting_editable(RoomSetting::JoinRule));
        assert!(popup.setting_editable(RoomSetting::Encryption));
        assert!(popup.setting_editable(RoomSetting::Direct));

        // Restricted join rule is clearly marked non-editable even if power permits state events.
        let mut restricted = d.clone();
        restricted.join_rule = Some(RoomJoinRule::Restricted);
        restricted.encrypted = Some(true);
        popup.set_details(restricted);
        assert!(!popup.setting_editable(RoomSetting::JoinRule));
        assert!(!popup.setting_editable(RoomSetting::Encryption));
        assert!(popup.setting_editable(RoomSetting::Direct));

        // Debug redaction across all SettingSubview variants.
        let views = [
            SettingSubview::JoinRule { selected: 0 },
            SettingSubview::History { selected: 1 },
            SettingSubview::EncryptConfirm {
                confirm: LineEdit::with_text("SECRET_CONFIRM"),
                focus: 0,
                error: Some("SECRET_ERR".into()),
            },
            SettingSubview::Picture {
                path: LineEdit::with_text("/tmp/secret-pic.png"),
                focus: 0,
                error: Some("SECRET_ERR".into()),
                notice: Some("SECRET_NOTE".into()),
            },
            SettingSubview::Addresses {
                input: LineEdit::with_text("#secret-typed:example.org"),
                focus: 0,
                selected: 0,
                confirm_delete: Some("#secret-del:example.org".into()),
                error: Some("SECRET_ERR".into()),
                notice: Some("SECRET_NOTE".into()),
            },
            SettingSubview::Permissions {
                draft: RoomPermissions::default(),
                selected: 0,
                error: Some("SECRET_ERR".into()),
            },
        ];
        let dbg = format!("{views:?}");
        for secret in [
            "SECRET_CONFIRM",
            "SECRET_ERR",
            "SECRET_NOTE",
            "secret-pic",
            "secret-typed",
            "secret-del",
        ] {
            assert!(!dbg.contains(secret), "{secret} leaked in {dbg}");
        }

        // Permission loss withdraws active editing subviews in set_details.
        popup.set_details(d.clone());
        popup.setting_view = Some(SettingSubview::Picture {
            path: LineEdit::new(),
            focus: 0,
            error: None,
            notice: None,
        });
        let mut revoked = d.clone();
        revoked.settings_power.can_avatar = false;
        popup.set_details(revoked);
        assert!(popup.setting_view.is_none());
        assert!(popup.error.is_some());

        // Encryption becoming true withdraws EncryptConfirm with notice.
        popup.set_details(d.clone());
        popup.setting_view = Some(SettingSubview::EncryptConfirm {
            confirm: LineEdit::new(),
            focus: 0,
            error: None,
        });
        let mut now_enc = d;
        now_enc.encrypted = Some(true);
        popup.set_details(now_enc);
        assert!(popup.setting_view.is_none());
        assert!(popup.notice.is_some());
    }
}
