//! The context menu: a small list of what can be done with a message, a room, a link
//! or the draft, opened by a right click or by key. This module decides what is on
//! offer and where the menu sits; `app::menu` opens it and runs the choice.
//!
//! Every entry ends in an `Action` or a flow that exists outside the menu too, so
//! the menu can never do something its key would not. What does not apply is left
//! out rather than greyed.

use ratatui::layout::Rect;

use crate::action::Action;
use crate::keymap::{Context, Keymap};
use crate::manage::Click;
use crate::model::{ItemRef, Message, MsgKind, ReadBy, Room, RoomKind, SendState};
use crate::text::{sanitize_line, truncate, width};

/// What a menu is about. Held by identity, never by position: the thing is looked
/// up again when an entry is run, and the menu closes when it is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Message(MessageRef),
    /// A link inside a message.
    Link {
        message: MessageRef,
        url: String,
    },
    /// A room in the room list, by its id, and the section it was in when the menu
    /// was made (what the menu offers depends on it).
    Room {
        id: String,
        kind: RoomKind,
    },
    /// The draft of this room.
    Composer {
        room_id: String,
    },
}

/// A message of the open room or thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRef {
    pub room_id: String,
    /// The thread it was shown in (its first message's event id), if any.
    pub thread: Option<String>,
    /// The message's stable id. Empty for demo data, which is found by `index`.
    pub id: String,
    pub index: usize,
    /// What the menu's entries were chosen for. A message that is sent, fails or
    /// is deleted keeps its id, but its menu is no longer the right one.
    pub state: SendState,
    pub kind: MsgKind,
    /// The server has it (it has an event id).
    pub on_server: bool,
}

impl MessageRef {
    /// Whether `message` is still what the menu was made for.
    pub fn fits(&self, message: &Message) -> bool {
        self.state == message.state
            && self.kind == message.kind
            && self.on_server == matches!(message.item, Some(ItemRef::Event(_)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Run {
    /// Done exactly as its key would do it, on the menu's target.
    Do(Action),
    /// Copy a `matrix.to` link to the message.
    CopyPermalink,
    /// List who has read up to the message.
    ReadBy,
    OpenLink,
    CopyLink,
    OpenRoom,
    MarkRead,
    MarkUnread,
    /// The room's notification setting, in its room info.
    Notifications,
    ClearDraft,
    /// Ignore the message's sender (`m.ignored_user_list`).
    IgnoreUser,
    /// Remove the message's sender from `m.ignored_user_list`.
    UnignoreUser,
    /// Report the message to the homeserver.
    Report,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    /// The key that does the same, or a detail (the time in "Read by").
    pub hint: String,
    /// `None`: a line that only says something.
    pub run: Option<Run>,
    /// Asks before it runs: the first Enter or click turns the label into this prompt.
    pub confirm: Option<&'static str>,
}

impl Item {
    fn new(label: &str, hint: &str, run: Run) -> Self {
        Self {
            label: label.to_owned(),
            hint: hint.to_owned(),
            run: Some(run),
            confirm: None,
        }
    }

    /// An entry that performs `action`, with the key bound to it in `context`.
    fn action(keys: &Keymap, label: &str, context: Context, action: Action) -> Self {
        Self::new(
            label,
            key_hint(keys, context, action, false),
            Run::Do(action),
        )
    }

    fn asking(self) -> Self {
        self.asking_label("Again to delete")
    }

    fn asking_label(mut self, prompt: &'static str) -> Self {
        self.confirm = Some(prompt);
        self
    }
}

/// The key that performs `action` where the focus is in `context`, as the keymap in
/// effect spells it (presets and the user's own bindings included); empty when no
/// key does. With two keys for one action, `first` picks the one listed first.
pub fn key_hint(keys: &Keymap, context: Context, action: Action, first: bool) -> &str {
    let in_context = |wanted: Context| {
        if first {
            keys.label_in(wanted, action)
        } else {
            keys.last_label_in(wanted, action)
        }
    };
    in_context(context)
        .or_else(|| in_context(Context::Global))
        .unwrap_or("")
}

#[derive(Debug, Clone)]
pub struct Menu {
    pub target: Target,
    /// Shown in the top border; may be empty.
    pub title: String,
    pub items: Vec<Item>,
    pub selected: usize,
    pub anchor: Anchor,
    /// The entry that was chosen once and asks to be chosen again.
    pub armed: Option<usize>,
}

/// What the menu hangs from: the pointer, or the rows of the thing it is about. The
/// menu goes below it where there is room, else above, and does not cover it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub column: u16,
    /// First and last row.
    pub top: u16,
    pub bottom: u16,
}

impl Anchor {
    /// One cell: the pointer.
    pub fn at(column: u16, row: u16) -> Self {
        Self {
            column,
            top: row,
            bottom: row,
        }
    }

    /// Something drawn in `rect`, with the menu `indent` columns in.
    pub fn below(rect: Rect, indent: u16) -> Self {
        Self {
            column: rect.x.saturating_add(indent),
            top: rect.y,
            bottom: rect.bottom().saturating_sub(1).max(rect.y),
        }
    }
}

impl Menu {
    /// `None` when there is nothing to offer.
    pub fn new(target: Target, title: &str, items: Vec<Item>, anchor: Anchor) -> Option<Self> {
        (!items.is_empty()).then(|| Self {
            target,
            title: sanitize_line(title),
            items,
            selected: 0,
            anchor,
            armed: None,
        })
    }

    /// Selects an entry. Moving away from an armed one takes its question back.
    pub fn select(&mut self, index: usize) {
        if index < self.items.len() && index != self.selected {
            self.selected = index;
            self.armed = None;
        }
    }

    /// What the entry at `index` shows: its label, or the question once armed.
    pub fn label(&self, index: usize) -> &str {
        match self.items.get(index) {
            Some(item) if self.armed == Some(index) => item.confirm.unwrap_or(&item.label),
            Some(item) => &item.label,
            None => "",
        }
    }

    /// Choosing the entry at `index`: true if it may run now. An entry that asks
    /// first is armed by the first choice and runs on the second.
    pub fn confirmed(&mut self, index: usize) -> bool {
        let asks = self
            .items
            .get(index)
            .is_some_and(|item| item.confirm.is_some());
        if asks && self.armed != Some(index) {
            self.selected = index;
            self.armed = Some(index);
            return false;
        }
        true
    }

    /// Moves the selection, going round at the ends.
    pub fn step(&mut self, delta: isize) {
        let count = self.items.len() as isize;
        if count > 0 {
            self.select((self.selected as isize + delta).rem_euclid(count) as usize);
        }
    }

    /// Selects the next entry that starts with `letter`. Nothing is run by a letter:
    /// one slip of a finger must not delete a message.
    pub fn jump(&mut self, letter: char) {
        let wanted: Vec<char> = letter.to_lowercase().collect();
        let count = self.items.len();
        let found = (1..=count)
            .map(|step| (self.selected + step) % count)
            .find(|at| {
                let first = self.items[*at].label.chars().next();
                first.is_some_and(|c| c.to_lowercase().collect::<Vec<_>>() == wanted)
            });
        if let Some(at) = found {
            self.select(at);
        }
    }

    pub fn run_at(&self, index: usize) -> Option<Run> {
        self.items.get(index)?.run
    }

    /// Where the menu is drawn on `screen`, and the first entry shown (it scrolls
    /// where the screen is too low for all of them). Next to its anchor: below it
    /// where there is room, else above, and pushed back inside at the edges.
    /// `None` on a screen too small for even one entry.
    pub fn place(&self, screen: Rect) -> Option<(Rect, usize)> {
        if screen.width < 8 || screen.height < 3 {
            return None;
        }
        // Border, one column of padding on either side, two between label and hint.
        let content = self
            .items
            .iter()
            .map(|item| {
                let label = match width(&item.hint) {
                    0 => width(&item.label),
                    hint => width(&item.label) + 2 + hint,
                };
                // The question takes the label's place and must fit as well.
                if let Some(prompt) = item.confirm {
                    label.max(width(prompt))
                } else {
                    label
                }
            })
            .max()
            .unwrap_or(0)
            .max(width(&self.title).min(30));
        let menu_width = (content + 4).min(screen.width as usize) as u16;
        let menu_height = (self.items.len() + 2).min(screen.height as usize) as u16;
        let last_row = screen.bottom() - 1;
        let column = self.anchor.column.clamp(screen.x, screen.right() - 1);
        let bottom = self.anchor.bottom.clamp(screen.y, last_row);
        let top = self.anchor.top.clamp(screen.y, bottom);
        let x = column.min(screen.right() - menu_width);
        let y = if bottom + menu_height <= last_row {
            bottom + 1
        } else if top >= screen.y + menu_height {
            top - menu_height
        } else {
            screen.bottom() - menu_height
        };
        let shown = (menu_height - 2) as usize;
        let first = (self.selected + 1)
            .saturating_sub(shown)
            .min(self.items.len() - shown);
        Some((Rect::new(x, y, menu_width, menu_height), first))
    }
}

/// What the app knows around a message, as far as the menu depends on it.
#[derive(Debug, Clone, Copy, Default)]
pub struct MessageFacts {
    /// There is an account (not the demo).
    pub connected: bool,
    /// The message is shown in a thread.
    pub in_thread: bool,
    /// A link to the message can be made.
    pub permalink: bool,
    /// The user wants to see who has read what (the `read_markers` setting).
    pub read_markers: bool,
    /// Whether the message's sender is currently in `m.ignored_user_list`.
    pub sender_ignored: bool,
}

/// The entries for a message. Empty for lines that are not messages.
pub fn message_items(message: &Message, facts: MessageFacts, keys: &Keymap) -> Vec<Item> {
    use Context::Timeline;
    if matches!(message.kind, MsgKind::System | MsgKind::ReadMarker) {
        return Vec::new();
    }
    let on_server = matches!(message.item, Some(ItemRef::Event(_)));
    // The demo's messages have no address and need none.
    let addressable = on_server || !facts.connected;
    let sent = message.state == SendState::Sent;
    let deleted = message.kind == MsgKind::Redacted;
    let readable = matches!(message.kind, MsgKind::Text | MsgKind::Emote);
    let mut items = Vec::new();

    if facts.connected && message.state == SendState::Failed {
        // The reply key retries a message that was not sent.
        items.push(Item::action(
            keys,
            "Try sending again",
            Timeline,
            Action::Reply,
        ));
    }
    if sent && addressable && !deleted {
        items.push(Item::action(keys, "Reply", Timeline, Action::Reply));
    }
    if sent && addressable && !facts.in_thread && (message.thread.is_some() || !deleted) {
        let label = if message.thread.is_some() {
            "Open thread"
        } else {
            "Reply in thread"
        };
        items.push(Item::action(keys, label, Timeline, Action::OpenThread));
    }
    if message.reply_event.is_some() {
        items.push(Item::action(
            keys,
            "Go to replied message",
            Timeline,
            Action::JumpToReply,
        ));
    }
    if sent && addressable && readable {
        items.push(Item::action(keys, "React…", Timeline, Action::React));
    }
    if message.own
        && sent
        && addressable
        && readable
        && message.image.is_none()
        && message.file.is_none()
    {
        items.push(Item::action(keys, "Edit", Timeline, Action::Edit));
    }
    if !deleted && !message.body.is_empty() {
        items.push(Item::action(keys, "Copy text", Timeline, Action::Copy));
    }
    if facts.permalink && on_server {
        items.push(Item::new("Copy link to message", "", Run::CopyPermalink));
    }
    // One entry for what the open key does with this message: the picture first,
    // then the file, then the first link.
    let has_link = || {
        !crate::rich::Rich::of(message.html.as_deref(), &message.body)
            .links
            .is_empty()
    };
    if !deleted {
        let open = if message.image.is_some() {
            Some(("View picture", true))
        } else if message.file.is_some() {
            Some(("Open file", false))
        } else if has_link() {
            Some(("Open link", false))
        } else {
            None
        };
        if let Some((label, first)) = open {
            let hint = key_hint(keys, Timeline, Action::Open, first);
            items.push(Item::new(label, hint, Run::Do(Action::Open)));
        }
    }
    if message.attachment().is_some() {
        let label = if message.image.is_some() {
            "Save picture"
        } else {
            "Save file"
        };
        items.push(Item::action(keys, label, Timeline, Action::Save));
    }
    if facts.read_markers && !message.read_by.is_empty() {
        items.push(Item::new("Read by…", "", Run::ReadBy));
    }
    if message.own && !sent {
        // Deleting what has not been sent takes it out of the queue.
        items.push(Item::action(keys, "Discard", Timeline, Action::Delete));
    } else if message.own && addressable && !deleted {
        // A click lands on the wrong row more easily than a finger on Del.
        items.push(Item::action(keys, "Delete", Timeline, Action::Delete).asking());
    }
    if facts.connected && !message.own {
        if message.sender_id.is_some() {
            let item = if facts.sender_ignored {
                Item::new("Stop ignoring", "", Run::UnignoreUser)
                    .asking_label("Again to stop ignoring")
            } else {
                Item::new("Ignore user", "", Run::IgnoreUser).asking_label("Again to ignore")
            };
            items.push(item);
        }
        if on_server && sent {
            items.push(Item::new("Report…", "", Run::Report));
        }
    }
    items
}

/// Who has read up to a message: one line each, with the time where it is known.
pub fn read_by_items(read_by: &ReadBy) -> Vec<Item> {
    let mut items: Vec<Item> = read_by
        .readers
        .iter()
        .map(|reader| Item {
            label: truncate(&sanitize_line(&reader.name), 30),
            hint: sanitize_line(&reader.time),
            run: None,
            confirm: None,
        })
        .collect();
    let more = (read_by.total as usize).saturating_sub(items.len());
    if more > 0 {
        items.push(Item {
            label: format!("and {more} more"),
            hint: String::new(),
            run: None,
            confirm: None,
        });
    }
    items
}

/// The entries for a link in a message.
pub fn link_items() -> Vec<Item> {
    vec![
        Item::new("Open link", "", Run::OpenLink),
        Item::new("Copy link", "", Run::CopyLink),
    ]
}

/// The entries for a room in the room list.
pub fn room_items(room: &Room, keys: &Keymap) -> Vec<Item> {
    use Context::Sidebar;
    let invite = room.kind == RoomKind::Invite;
    let mut items = vec![
        Item::new(
            "Open",
            key_hint(keys, Sidebar, Action::SidebarOpen, true),
            Run::OpenRoom,
        ),
        Item::action(keys, "Room info", Sidebar, Action::RoomInfo),
    ];
    if invite {
        items.push(Item::action(
            keys,
            "Decline invitation…",
            Sidebar,
            Action::LeaveRoom,
        ));
        return items;
    }
    if room.shows_unread() {
        items.push(Item::new("Mark as read", "", Run::MarkRead));
    } else {
        items.push(Item::new("Mark as unread", "", Run::MarkUnread));
    }
    let favourite = if room.kind == RoomKind::Favourite {
        "Remove from favourites"
    } else {
        "Add to favourites"
    };
    items.push(Item::new(
        favourite,
        "",
        Run::Do(Action::Manage(Click::Favourite)),
    ));
    items.push(Item::new("Notification mode…", "", Run::Notifications));
    items.push(Item::action(keys, "Leave…", Sidebar, Action::LeaveRoom));
    items
}

/// The entries for the composer. Pasting is the terminal's business.
pub fn composer_items(can_attach: bool, has_draft: bool, keys: &Keymap) -> Vec<Item> {
    use Context::Composer;
    let mut items = Vec::new();
    if can_attach {
        items.push(Item::action(keys, "Attach file…", Composer, Action::Attach));
    }
    items.push(Item::action(keys, "Emoji…", Composer, Action::OpenEmoji));
    if has_draft {
        items.push(Item::new("Clear draft", "", Run::ClearDraft));
    }
    items
}

/// A `matrix.to` link to an event, routed through our own server (which is in the
/// room, since we are). `None` for ids that are not real ones (the demo's).
pub fn permalink(room_id: &str, event_id: &str, own_user: Option<&str>) -> Option<String> {
    use matrix_sdk::ruma::{EventId, RoomId, UserId};
    let room = RoomId::parse(room_id).ok()?;
    let event = EventId::parse(event_id).ok()?;
    let via = own_user
        .and_then(|user| UserId::parse(user).ok())
        .map(|user| user.server_name().to_owned());
    Some(room.matrix_to_event_uri_via(event, via).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Attachment, ImageInfo, ImageSource, Reader, ThreadSummary};

    fn keys() -> Keymap {
        Keymap::from_preset(crate::keymap::Preset::Default)
    }

    fn room_target() -> Target {
        Target::Room {
            id: "r".into(),
            kind: RoomKind::Room,
        }
    }

    fn labels(items: &[Item]) -> Vec<&str> {
        items.iter().map(|item| item.label.as_str()).collect()
    }

    fn text(own: bool) -> Message {
        Message {
            id: "m1".into(),
            item: Some(ItemRef::Event("$event".into())),
            sender: if own { "you" } else { "bob" }.into(),
            body: "hello".into(),
            own,
            ..Message::default()
        }
    }

    const CONNECTED: MessageFacts = MessageFacts {
        connected: true,
        in_thread: false,
        permalink: true,
        read_markers: true,
        sender_ignored: false,
    };

    fn file() -> Attachment {
        use matrix_sdk::ruma::events::room::MediaSource;
        use matrix_sdk::ruma::mxc_uri;
        Attachment {
            key: "mxc://example.org/f".into(),
            name: "notes.pdf".into(),
            size: None,
            source: Box::new(MediaSource::Plain(
                mxc_uri!("mxc://example.org/f").to_owned(),
            )),
        }
    }

    #[test]
    fn a_message_of_someone_else() {
        let items = message_items(&text(false), CONNECTED, &keys());
        assert_eq!(
            labels(&items),
            [
                "Reply",
                "Reply in thread",
                "React…",
                "Copy text",
                "Copy link to message",
                "Report…",
            ]
        );
        // Nothing to edit or delete there, and nothing is listed as unavailable.
        assert!(items.iter().all(|item| item.run.is_some()));
    }

    #[test]
    fn a_message_of_ours() {
        let items = message_items(&text(true), CONNECTED, &keys());
        assert_eq!(
            labels(&items),
            [
                "Reply",
                "Reply in thread",
                "React…",
                "Edit",
                "Copy text",
                "Copy link to message",
                "Delete"
            ]
        );
        assert_eq!(items[3].run, Some(Run::Do(Action::Edit)));
        assert_eq!(items[6].run, Some(Run::Do(Action::Delete)));
    }

    #[test]
    fn key_hints_come_from_the_keymap() {
        let items = message_items(&text(true), CONNECTED, &keys());
        for item in &items {
            let Some(Run::Do(action)) = item.run else {
                assert_eq!(item.hint, "", "{}", item.label);
                continue;
            };
            let keys = keys();
            let bound = keys.bindings().iter().any(|binding| {
                binding.action == action
                    && binding.label == item.hint
                    && matches!(binding.context, Context::Timeline | Context::Global)
            });
            assert!(bound, "{}: {:?}", item.label, item.hint);
        }
        let keys = keys();
        // The context's own key wins over a global one, and a missing one is no hint.
        assert_eq!(
            key_hint(&keys, Context::Timeline, Action::Reply, false),
            "Enter"
        );
        assert_eq!(key_hint(&keys, Context::Composer, Action::Reply, false), "");
        assert_eq!(
            key_hint(&keys, Context::Timeline, Action::RoomInfo, false),
            "F4"
        );
        // Two keys open things: the first is the one for pictures.
        let first = key_hint(&keys, Context::Timeline, Action::Open, true);
        let last = key_hint(&keys, Context::Timeline, Action::Open, false);
        assert_ne!(first, last);
        assert!(!first.is_empty() && !last.is_empty());

        // The keys in effect, not the built-in ones: rebound, and unbound.
        let mut keys = keys;
        let (code, mods) = crate::keymap::parse_key(Context::Timeline, "x").unwrap();
        keys.apply_rebind(Context::Timeline, Action::Edit, code, mods);
        keys.unbind_action(Context::Timeline, Action::Copy).unwrap();
        let items = message_items(&text(true), CONNECTED, &keys);
        let hint = |label: &str| {
            let item = items.iter().find(|item| item.label == label).unwrap();
            item.hint.clone()
        };
        assert_eq!(hint("Edit"), "x");
        assert_eq!(hint("Copy text"), "");
        assert_eq!(hint("Reply"), "Enter");
        // The vim keys are added to the usual ones, and `m` stays the menu's.
        let vim = Keymap::from_preset(crate::keymap::Preset::Vim);
        assert_eq!(
            key_hint(&vim, Context::Timeline, Action::ContextMenu, true),
            "m"
        );
        assert_eq!(
            key_hint(&vim, Context::Composer, Action::ContextMenu, true),
            "Menu"
        );
    }

    #[test]
    fn threads_links_pictures_and_files() {
        let mut message = text(false);
        message.thread = Some(ThreadSummary::default());
        message.body = "see https://example.org".into();
        let items = message_items(&message, CONNECTED, &keys());
        assert!(labels(&items).contains(&"Open thread"));
        assert!(!labels(&items).contains(&"Reply in thread"));
        assert!(labels(&items).contains(&"Open link"));
        assert!(!labels(&items).contains(&"Save file"));

        // Inside a thread there is no thread to open.
        let in_thread = MessageFacts {
            in_thread: true,
            ..CONNECTED
        };
        let items = message_items(&message, in_thread, &keys());
        assert!(!labels(&items).iter().any(|label| label.contains("thread")));

        let mut picture = text(true);
        picture.image = Some(ImageInfo {
            key: "demo".into(),
            source: ImageSource::Demo,
            name: "cat.png".into(),
            caption: None,
            dimensions: None,
            size: None,
        });
        let items = message_items(&picture, CONNECTED, &keys());
        assert!(labels(&items).contains(&"View picture"));
        // Pictures are not edited; the demo picture has no file to save.
        assert!(!labels(&items).contains(&"Edit"));
        assert!(!labels(&items).contains(&"Save picture"));

        let mut with_file = text(false);
        with_file.file = Some(file());
        let items = message_items(&with_file, CONNECTED, &keys());
        assert!(labels(&items).contains(&"Open file"));
        assert!(labels(&items).contains(&"Save file"));
        assert!(!labels(&items).contains(&"Open link"));
    }

    #[test]
    fn unsent_messages_can_be_retried_or_discarded() {
        let mut failed = text(true);
        failed.item = Some(ItemRef::Transaction("txn".into()));
        failed.state = SendState::Failed;
        assert_eq!(
            labels(&message_items(&failed, CONNECTED, &keys())),
            ["Try sending again", "Copy text", "Discard"]
        );
        let mut sending = failed.clone();
        sending.state = SendState::Sending;
        assert_eq!(
            labels(&message_items(&sending, CONNECTED, &keys())),
            ["Copy text", "Discard"]
        );
    }

    #[test]
    fn deleted_unreadable_and_system_lines() {
        let mut deleted = text(true);
        deleted.kind = MsgKind::Redacted;
        deleted.body = "Message deleted".into();
        assert_eq!(
            labels(&message_items(&deleted, CONNECTED, &keys())),
            ["Copy link to message"]
        );
        let mut locked = text(false);
        locked.kind = MsgKind::Undecryptable;
        let items = message_items(&locked, CONNECTED, &keys());
        assert!(labels(&items).contains(&"Reply"));
        assert!(!labels(&items).contains(&"React…"));
        let mut system = text(false);
        system.kind = MsgKind::System;
        assert!(message_items(&system, CONNECTED, &keys()).is_empty());
    }

    #[test]
    fn the_demo_has_no_links_to_messages_but_everything_else() {
        let mut message = text(true);
        message.item = None;
        let items = message_items(&message, MessageFacts::default(), &keys());
        assert_eq!(
            labels(&items),
            [
                "Reply",
                "Reply in thread",
                "React…",
                "Edit",
                "Copy text",
                "Delete"
            ]
        );
    }

    #[test]
    fn readers_are_listed_with_their_times() {
        let mut message = text(true);
        assert!(!labels(&message_items(&message, CONNECTED, &keys())).contains(&"Read by…"));
        message.read_by = ReadBy {
            readers: vec![
                Reader {
                    name: "bob".into(),
                    time: "14:05".into(),
                },
                Reader {
                    name: "carol\u{1b}[2J".into(),
                    time: String::new(),
                },
            ],
            total: 5,
        };
        assert!(labels(&message_items(&message, CONNECTED, &keys())).contains(&"Read by…"));
        // Not wanted: not offered either.
        let quiet = MessageFacts {
            read_markers: false,
            ..CONNECTED
        };
        assert!(!labels(&message_items(&message, quiet, &keys())).contains(&"Read by…"));
        let items = read_by_items(&message.read_by);
        assert_eq!(labels(&items), ["bob", "carol[2J", "and 3 more"]);
        assert_eq!(items[0].hint, "14:05");
        assert!(items.iter().all(|item| item.run.is_none()));
    }

    #[test]
    fn rooms_invitations_links_and_the_composer() {
        let mut room = Room::new("!a:example.org", "rust", RoomKind::Room);
        assert_eq!(
            labels(&room_items(&room, &keys())),
            [
                "Open",
                "Room info",
                "Mark as unread",
                "Add to favourites",
                "Notification mode…",
                "Leave…"
            ]
        );
        room.kind = RoomKind::Favourite;
        room.bold = true;
        let items = room_items(&room, &keys());
        assert_eq!(
            labels(&items),
            [
                "Open",
                "Room info",
                "Mark as read",
                "Remove from favourites",
                "Notification mode…",
                "Leave…"
            ]
        );
        // Leaving goes through the question in the room info.
        assert_eq!(items[5].run, Some(Run::Do(Action::LeaveRoom)));
        assert_eq!(items[1].hint, "F4");
        room.kind = RoomKind::Invite;
        assert_eq!(
            labels(&room_items(&room, &keys())),
            ["Open", "Room info", "Decline invitation…"]
        );

        assert_eq!(labels(&link_items()), ["Open link", "Copy link"]);

        assert_eq!(
            labels(&composer_items(true, true, &keys())),
            ["Attach file…", "Emoji…", "Clear draft"]
        );
        assert_eq!(labels(&composer_items(false, false, &keys())), ["Emoji…"]);
        // No word about pasting: that is the terminal's.
        assert!(
            composer_items(true, true, &keys())
                .iter()
                .all(|item| !item.label.contains("Paste"))
        );
    }

    fn menu(count: usize, anchor: (u16, u16)) -> Menu {
        let items = (0..count)
            .map(|n| Item::new(&format!("Entry {n}"), "Ctrl-X", Run::OpenLink))
            .collect();
        let anchor = Anchor::at(anchor.0, anchor.1);
        Menu::new(room_target(), "", items, anchor).unwrap()
    }

    #[test]
    fn nothing_to_offer_is_no_menu() {
        let target = room_target();
        assert!(Menu::new(target, "", Vec::new(), Anchor::at(0, 0)).is_none());
    }

    #[test]
    fn the_menu_stays_on_screen() {
        let screen = Rect::new(0, 0, 80, 24);
        // In the open: right below the pointer.
        let (rect, first) = menu(5, (10, 5)).place(screen).unwrap();
        assert_eq!((rect.x, rect.y, rect.height, first), (10, 6, 7, 0));
        // Wherever it is asked for, and whatever the size of the screen, it is
        // inside, and the selected entry is among those shown.
        for (width, height) in [(80, 24), (30, 8), (12, 4), (8, 3), (200, 60)] {
            let screen = Rect::new(0, 0, width, height);
            for anchor in [
                (0, 0),
                (width - 1, 0),
                (0, height - 1),
                (width - 1, height - 1),
                (width / 2, height / 2),
                (u16::MAX, u16::MAX),
            ] {
                for selected in [0, 4, 8] {
                    let mut menu = menu(9, anchor);
                    menu.selected = selected;
                    let (rect, first) = menu.place(screen).unwrap();
                    let what = format!("{width}x{height} at {anchor:?}");
                    assert!(rect.width >= 3 && rect.height >= 3, "{what}");
                    assert_eq!(screen.intersection(rect), rect, "{what}");
                    let shown = (rect.height - 2) as usize;
                    assert!((first..first + shown).contains(&selected), "{what}");
                    assert!(first + shown <= 9, "{what}");
                }
            }
        }
        // Near the bottom it opens upwards, and does not cover its anchor row.
        let (rect, _) = menu(5, (10, 22)).place(screen).unwrap();
        assert_eq!(rect.bottom(), 22);
        // About something of several rows: below it, or above all of it.
        let mut tall = menu(5, (10, 0));
        tall.anchor = Anchor::below(Rect::new(4, 8, 30, 3), 2);
        let (rect, _) = tall.place(screen).unwrap();
        assert_eq!((rect.x, rect.y), (6, 11));
        tall.anchor = Anchor::below(Rect::new(4, 18, 30, 3), 2);
        let (rect, _) = tall.place(screen).unwrap();
        assert_eq!(rect.bottom(), 18);
        // Too small for anything.
        assert!(menu(5, (0, 0)).place(Rect::new(0, 0, 7, 10)).is_none());
        assert!(menu(5, (0, 0)).place(Rect::new(0, 0, 40, 2)).is_none());
        // Not at the origin either.
        let offset = Rect::new(5, 3, 20, 6);
        let (rect, _) = menu(9, (0, 0)).place(offset).unwrap();
        assert_eq!(offset.intersection(rect), rect);
    }

    #[test]
    fn moving_goes_round_and_letters_only_select() {
        let items = vec![
            Item::new("Reply", "", Run::Do(Action::Reply)),
            Item::new("React…", "", Run::Do(Action::React)),
            Item::new("Delete", "", Run::Do(Action::Delete)),
        ];
        let target = room_target();
        let mut menu = Menu::new(target, "", items, Anchor::at(0, 0)).unwrap();
        menu.step(-1);
        assert_eq!(menu.selected, 2);
        menu.step(1);
        assert_eq!(menu.selected, 0);
        menu.jump('r');
        assert_eq!(menu.selected, 1);
        menu.jump('R');
        assert_eq!(menu.selected, 0);
        menu.jump('d');
        assert_eq!(menu.selected, 2);
        menu.jump('x');
        assert_eq!(menu.selected, 2);
        assert_eq!(menu.run_at(2), Some(Run::Do(Action::Delete)));
        assert_eq!(menu.run_at(3), None);
    }

    #[test]
    fn delete_asks_once_before_it_runs() {
        let items = message_items(&text(true), CONNECTED, &keys());
        let delete = items.len() - 1;
        assert!(items[delete].confirm.is_some());
        // Nothing else asks; discarding what was never sent does not either.
        assert_eq!(
            items.iter().filter(|item| item.confirm.is_some()).count(),
            1
        );
        let mut failed = text(true);
        failed.state = SendState::Failed;
        let unsent = message_items(&failed, CONNECTED, &keys());
        assert!(unsent.iter().all(|item| item.confirm.is_none()));

        let target = room_target();
        let mut menu = Menu::new(target, "", items, Anchor::at(0, 0)).unwrap();
        assert!(menu.confirmed(0));
        assert!(!menu.confirmed(delete));
        assert_eq!(menu.selected, delete);
        assert_eq!(menu.label(delete), "Again to delete");
        // The question fits whatever else is in the menu, also without a key.
        let mut bare = keys();
        for action in [
            Action::Delete,
            Action::Reply,
            Action::Copy,
            Action::OpenThread,
        ] {
            bare.unbind_action(Context::Timeline, action).unwrap();
        }
        let mut short = text(true);
        short.item = None;
        let facts = MessageFacts {
            in_thread: true,
            ..MessageFacts::default()
        };
        let items = message_items(&short, facts, &bare);
        let narrow = Menu::new(room_target(), "", items, Anchor::at(0, 0)).unwrap();
        let (rect, _) = narrow.place(Rect::new(0, 0, 80, 24)).unwrap();
        assert_eq!(rect.width as usize, width("Again to delete") + 4);
        // A message that changed is not what its menu was made for.
        let mut wanted = MessageRef {
            room_id: "!r".into(),
            thread: None,
            id: "m1".into(),
            index: 0,
            state: SendState::Sending,
            kind: MsgKind::Text,
            on_server: false,
        };
        let mut message = text(true);
        message.item = Some(ItemRef::Transaction("t".into()));
        message.state = SendState::Sending;
        assert!(wanted.fits(&message));
        message.state = SendState::Sent;
        assert!(!wanted.fits(&message));
        wanted.state = SendState::Sent;
        message.item = Some(ItemRef::Event("$e".into()));
        assert!(!wanted.fits(&message));
        wanted.on_server = true;
        assert!(wanted.fits(&message));
        message.kind = MsgKind::Redacted;
        assert!(!wanted.fits(&message));
        assert!(menu.confirmed(delete));
        // Moving away takes the question back.
        menu.armed = None;
        assert!(!menu.confirmed(delete));
        menu.step(-1);
        assert_eq!(menu.armed, None);
        assert_eq!(menu.label(delete), "Delete");
        menu.step(1);
        assert!(!menu.confirmed(delete));
        menu.jump('r');
        assert_eq!(menu.armed, None);
    }

    #[test]
    fn links_to_messages() {
        assert_eq!(
            permalink(
                "!room:example.org",
                "$event:example.org",
                Some("@me:home.example")
            )
            .as_deref(),
            Some("https://matrix.to/#/!room:example.org/$event:example.org?via=home.example")
        );
        assert!(
            permalink("!room:example.org", "$abc", None)
                .unwrap()
                .starts_with("https://matrix.to/#/!room:example.org/$abc")
        );
        // The demo's rooms and messages have no address.
        assert_eq!(permalink("rust", "$event", None), None);
        assert_eq!(permalink("!room:example.org", "", None), None);
    }

    #[test]
    fn remote_message_offers_ignore_and_report_with_custom_confirm_labels() {
        let mut message = text(false);
        message.sender_id = Some("@bob:example.org".into());

        let items = message_items(&message, CONNECTED, &keys());
        assert!(
            items
                .iter()
                .any(|i| i.label == "Report…" && i.confirm.is_none())
        );
        let ignore_at = items
            .iter()
            .position(|i| i.run == Some(Run::IgnoreUser))
            .unwrap();
        assert_eq!(items[ignore_at].label, "Ignore user");
        let mut menu = Menu::new(room_target(), "", items, Anchor::at(0, 0)).unwrap();
        assert!(!menu.confirmed(ignore_at));
        assert_eq!(menu.label(ignore_at), "Again to ignore");
        assert!(menu.confirmed(ignore_at));

        let unignore_facts = MessageFacts {
            sender_ignored: true,
            ..CONNECTED
        };
        let items = message_items(&message, unignore_facts, &keys());
        let unignore_at = items
            .iter()
            .position(|i| i.run == Some(Run::UnignoreUser))
            .unwrap();
        assert_eq!(items[unignore_at].label, "Stop ignoring");
        let mut menu = Menu::new(room_target(), "", items, Anchor::at(0, 0)).unwrap();
        assert!(!menu.confirmed(unignore_at));
        assert_eq!(menu.label(unignore_at), "Again to stop ignoring");
        assert!(menu.confirmed(unignore_at));
    }
}
