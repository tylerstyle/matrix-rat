//! Command palette state, entry collection, and fuzzy ranking.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::action::{Action, Pane};
use crate::app::ComposeMode;
use crate::keymap;
use crate::lineedit::LineEdit;
use crate::modal::Setting;

/// Broad grouping shown alongside each command palette entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Action,
    Room,
    Settings,
    Theme,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Self::Action => "Action",
            Self::Room => "Room",
            Self::Settings => "Setting",
            Self::Theme => "Theme",
        }
    }
}

/// What running a command palette entry does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Action(Action),
    /// Matrix room id, resolved back to `App::rooms` when executed so background sync
    /// updates cannot redirect to a shifted index.
    Room(String),
    Theme(String),
}

/// One row in the command palette.
#[derive(Debug, Clone)]
pub struct Entry {
    pub target: Target,
    pub category: Category,
    pub title: String,
    pub hint: Option<String>,
    pub enabled: bool,
    subject: String,
    title_lower: String,
    hint_lower: String,
    aliases: Vec<String>,
    empty_group: u8,
    base_order: usize,
}

/// Room metadata needed to populate the room-switching entries.
#[derive(Debug, Clone)]
pub struct RoomItem {
    pub id: String,
    pub name: String,
    pub unread: u32,
    pub active: bool,
}

/// Availability of timeline actions for the currently selected message.
#[derive(Debug, Clone, Copy, Default)]
pub struct MessageCaps {
    pub has_selection: bool,
    pub own: bool,
    pub can_edit: bool,
    pub can_copy: bool,
    pub can_save: bool,
    pub can_open: bool,
    pub can_open_thread: bool,
    pub failed_retry: bool,
}

/// Snapshot of application state used to construct palette entries and availability.
#[derive(Debug, Clone)]
pub struct PaletteContext {
    pub focus: Pane,
    pub compose_mode: ComposeMode,
    pub in_invite: bool,
    pub thread_open: bool,
    pub has_room: bool,
    pub has_messages: bool,
    pub connected: bool,
    pub user_id: Option<String>,
    pub mouse: bool,
    pub typing_notices: bool,
    pub read_receipts: bool,
    pub read_markers: bool,
    pub room_previews: bool,
    pub notifications: bool,
    pub download_dir: String,
    pub verify_status: &'static str,
    pub wanted_theme: String,
    pub theme_dark: Option<String>,
    pub theme_light: Option<String>,
    pub themes: Vec<String>,
    pub rooms: Vec<RoomItem>,
    pub selection: MessageCaps,
    /// The room that room-level commands (room info, invite, favourite, leave) would
    /// act on, by the rule they follow themselves: its name, and whether it is an
    /// invitation. Not always the open room; `None` if there is none.
    pub target: Option<(String, bool)>,
    /// Effective keymap for displaying dynamic shortcuts.
    pub keymap: keymap::Keymap,
}

/// State for the command palette popup (`Ctrl-P`).
#[derive(Debug, Clone)]
pub struct Palette {
    pub query: LineEdit,
    pub entries: Vec<Entry>,
    /// Indices into `entries`, ordered best match first.
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub scroll: usize,
    /// List rows drawn on the last frame, used by PageUp/PageDown.
    pub rows: usize,
    /// Keeps keyboard selection in view on the next draw.
    pub reveal: bool,
}

impl Palette {
    /// Builds a command palette populated from the current application context.
    pub fn open(ctx: PaletteContext) -> Self {
        let mut entries = Vec::new();
        collect_actions(&ctx, &mut entries);
        collect_room_management(&ctx, &mut entries);
        collect_rooms(&ctx, &mut entries);
        collect_settings(&ctx, &mut entries);
        collect_themes(&ctx, &mut entries);

        let mut palette = Self {
            query: LineEdit::new(),
            entries,
            filtered: Vec::new(),
            selected: 0,
            scroll: 0,
            rows: 10,
            reveal: false,
        };
        palette.refilter();
        palette
    }

    #[cfg(test)]
    pub fn set_query(&mut self, query: String) {
        if self.query.as_str() == query {
            return;
        }
        self.query.set(query);
        self.on_query_changed();
    }

    pub fn on_query_changed(&mut self) {
        self.selected = 0;
        self.scroll = 0;
        self.reveal = true;
        self.refilter();
    }

    /// Moves the selection by `delta`, clamping to the filtered results.
    pub fn step(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let max = self.filtered.len() - 1;
        let next = (self.selected as isize + delta).clamp(0, max as isize) as usize;
        if next != self.selected {
            self.selected = next;
            self.reveal = true;
        }
    }

    /// Scrolls the viewport without changing selection (mouse wheel).
    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.filtered.len().saturating_sub(self.rows.max(1));
        self.scroll = (self.scroll as isize + delta).clamp(0, max as isize) as usize;
        self.reveal = false;
    }

    #[cfg(test)]
    pub fn selected_entry(&self) -> Option<&Entry> {
        self.entry_at(self.selected)
    }

    pub fn entry_at(&self, filtered_index: usize) -> Option<&Entry> {
        let idx = *self.filtered.get(filtered_index)?;
        self.entries.get(idx)
    }

    fn refilter(&mut self) {
        let raw = self.query.trim();
        if raw.is_empty() {
            let mut indices: Vec<usize> = (0..self.entries.len()).collect();
            indices.sort_by_key(|&i| {
                let e = &self.entries[i];
                (e.empty_group, e.base_order)
            });
            self.filtered = indices;
            return;
        }

        let q = raw.to_lowercase();
        let pattern = Pattern::parse(&q, CaseMatching::Ignore, Normalization::Smart);
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf = Vec::new();
        let mut scored: Vec<(usize, u32)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| {
                score_entry(entry, &q, &pattern, &mut matcher, &mut buf).map(|s| (idx, s))
            })
            .collect();

        scored.sort_by(|&(ai, sa), &(bi, sb)| {
            sb.cmp(&sa)
                .then_with(|| {
                    self.entries[ai]
                        .subject
                        .chars()
                        .count()
                        .cmp(&self.entries[bi].subject.chars().count())
                })
                .then_with(|| {
                    self.entries[ai]
                        .base_order
                        .cmp(&self.entries[bi].base_order)
                })
        });

        self.filtered = scored.into_iter().map(|(idx, _)| idx).collect();
    }
}

#[expect(clippy::too_many_arguments)]
fn push_entry(
    entries: &mut Vec<Entry>,
    target: Target,
    category: Category,
    title: impl Into<String>,
    hint: Option<String>,
    subject: impl Into<String>,
    aliases: &[&str],
    enabled: bool,
    empty_group: u8,
) {
    let title = title.into();
    let title_lower = title.to_lowercase();
    let subject = subject.into().to_lowercase();
    let hint_lower = hint.as_deref().unwrap_or("").to_lowercase();
    let aliases = aliases.iter().map(|a| a.to_lowercase()).collect();
    let order = entries.len();
    entries.push(Entry {
        target,
        category,
        title,
        hint,
        enabled,
        subject,
        title_lower,
        hint_lower,
        aliases,
        empty_group: if enabled { empty_group } else { 5 },
        base_order: order,
    });
}

fn collect_actions(ctx: &PaletteContext, out: &mut Vec<Entry>) {
    // Contextual actions that are only meaningful when active and not bound in DEFAULT.
    if ctx.in_invite {
        push_entry(
            out,
            Target::Action(Action::AcceptInvite),
            Category::Action,
            "Accept invite",
            None,
            "Accept invite",
            &["join room"],
            true,
            0,
        );
        push_entry(
            out,
            Target::Action(Action::DeclineInvite),
            Category::Action,
            "Decline invite",
            None,
            "Decline invite",
            &["reject invite", "leave room"],
            true,
            0,
        );
    }
    let cancel_hint = ctx
        .keymap
        .label_in(keymap::Context::Composer, Action::CancelCompose)
        .or_else(|| ctx.keymap.label_in(keymap::Context::Composer, Action::Back))
        .map(str::to_owned);
    match ctx.compose_mode {
        ComposeMode::Reply(_) => push_entry(
            out,
            Target::Action(Action::CancelCompose),
            Category::Action,
            "Cancel reply",
            cancel_hint.clone(),
            "Cancel reply",
            &[],
            true,
            0,
        ),
        ComposeMode::Edit(_) => push_entry(
            out,
            Target::Action(Action::CancelCompose),
            Category::Action,
            "Cancel edit",
            cancel_hint,
            "Cancel edit",
            &[],
            true,
            0,
        ),
        ComposeMode::Normal => {}
    }
    // Thread closing is bound via Esc/back when a thread is open rather than its own entry in
    // DEFAULT, so expose it here with state-based availability.
    let close_thread_hint = ctx
        .keymap
        .label_for(Action::CloseThread)
        .or_else(|| ctx.keymap.label_in(keymap::Context::Timeline, Action::Back))
        .map(str::to_owned);
    push_entry(
        out,
        Target::Action(Action::CloseThread),
        Category::Action,
        "Close thread",
        close_thread_hint,
        "Close thread",
        &["back to room", "leave thread", "exit thread"],
        ctx.thread_open,
        0,
    );

    let active_bindings = ctx.keymap.bindings();
    let multi_filter = active_bindings
        .iter()
        .filter(|b| b.action == Action::FocusRoomFilter)
        .count()
        > 1;
    let multi_open = active_bindings
        .iter()
        .filter(|b| b.action == Action::Open)
        .count()
        > 1;

    let mut seen: Vec<Action> = Vec::new();
    for binding in active_bindings {
        if include_keymap_action(binding.action) == ActionInclusion::Skip {
            continue;
        }
        // Prefer the clearer binding when two default keys share an action: Ctrl-K over Alt-R
        // for finding/filtering rooms, and `o` over `v` for opening links/pictures/files.
        if binding.action == Action::FocusRoomFilter && binding.label == "Alt-R" && multi_filter {
            continue;
        }
        if binding.action == Action::Open && binding.label == "v" && multi_open {
            continue;
        }
        if seen.contains(&binding.action) {
            continue;
        }
        seen.push(binding.action);

        let (title, aliases): (&str, &[&str]) = match binding.action {
            Action::Reply if ctx.selection.failed_retry => {
                ("Try sending again", &["retry failed message", "reply"])
            }
            Action::SearchEverywhere => (
                binding.description,
                &["search messages", "global search", "find across rooms"],
            ),
            Action::FocusRoomFilter => (binding.description, &["filter rooms", "alt-r"]),
            Action::Open => (binding.description, &["view picture"]),
            Action::OpenThread => (binding.description, &["thread", "replies"]),
            Action::Attach => (binding.description, &["upload", "send file", "attach"]),
            Action::ToggleHelp => (binding.description, &["key bindings", "shortcuts", "help"]),
            _ => (binding.description, &[]),
        };

        let enabled = action_enabled(binding.action, ctx);
        let group = if action_contextual(binding.action, ctx) {
            0
        } else {
            1
        };

        push_entry(
            out,
            Target::Action(binding.action),
            Category::Action,
            title,
            Some(binding.label.to_string()),
            title,
            aliases,
            enabled,
            group,
        );
    }

    // Keep core actions reachable in the palette even if their keys were unbound.
    for binding in keymap::DEFAULT {
        if include_keymap_action(binding.action) == ActionInclusion::Skip
            || seen.contains(&binding.action)
        {
            continue;
        }
        seen.push(binding.action);
        let enabled = action_enabled(binding.action, ctx);
        let group = if action_contextual(binding.action, ctx) {
            0
        } else {
            1
        };
        push_entry(
            out,
            Target::Action(binding.action),
            Category::Action,
            binding.description,
            None,
            binding.description,
            &[],
            enabled,
            group,
        );
    }
}

#[derive(PartialEq, Eq)]
enum ActionInclusion {
    Include,
    Skip,
}

fn include_keymap_action(action: Action) -> ActionInclusion {
    match action {
        // Movement, line-by-line cursor stepping, raw compose keys, room-list stepping,
        // and the palette itself do not make sense to trigger from inside the command palette.
        Action::OpenPalette
        | Action::FocusNext
        | Action::FocusPrev
        | Action::FocusDir(_)
        | Action::PageUp
        | Action::PageDown
        | Action::Back
        | Action::SidebarUp
        | Action::SidebarDown
        | Action::SidebarTop
        | Action::SidebarBottom
        | Action::SidebarOpen
        | Action::SelectOlder
        | Action::SelectNewer
        | Action::RoomPrev
        | Action::RoomNext
        | Action::Send
        | Action::Newline
        // A composer key that depends on what is typed; "Edit" covers it by name.
        | Action::EditLast => ActionInclusion::Skip,
        _ => ActionInclusion::Include,
    }
}

fn action_enabled(action: Action, ctx: &PaletteContext) -> bool {
    let sel = ctx.selection;
    match action {
        Action::Reply => sel.has_selection,
        Action::Edit => sel.can_edit,
        Action::React => sel.has_selection,
        Action::Delete => sel.has_selection && sel.own,
        Action::Copy => sel.can_copy,
        Action::Save => sel.can_save,
        Action::Open => sel.can_open,
        Action::OpenThread => sel.can_open_thread,
        Action::CloseThread => ctx.thread_open,
        Action::Attach => {
            ctx.has_room && !ctx.in_invite && !matches!(ctx.compose_mode, ComposeMode::Edit(_))
        }
        Action::JumpToNewest | Action::JumpToOldest => ctx.has_room && ctx.has_messages,
        Action::AcceptInvite | Action::DeclineInvite => ctx.in_invite,
        Action::RoomInfo => ctx.target.is_some(),
        Action::Search => ctx.has_room && !ctx.in_invite,
        _ => true,
    }
}

fn action_contextual(action: Action, ctx: &PaletteContext) -> bool {
    match ctx.focus {
        Pane::Timeline => matches!(
            action,
            Action::Reply
                | Action::Edit
                | Action::React
                | Action::Delete
                | Action::Copy
                | Action::Save
                | Action::Open
                | Action::OpenThread
                | Action::JumpToNewest
                | Action::JumpToOldest
        ),
        Pane::Sidebar => action == Action::FocusRoomFilter,
        Pane::Composer => matches!(
            action,
            Action::OpenEmoji | Action::Attach | Action::FocusRoomFilter
        ),
    }
}

fn collect_rooms(ctx: &PaletteContext, out: &mut Vec<Entry>) {
    for room in &ctx.rooms {
        let hint = if room.unread > 0 {
            Some(format!("{} unread", room.unread))
        } else if room.active {
            Some("current".to_owned())
        } else {
            None
        };
        let room_alias = format!("room {}", room.name);
        push_entry(
            out,
            Target::Room(room.id.clone()),
            Category::Room,
            format!("Go to {}", room.name),
            hint,
            room.name.clone(),
            &[&room_alias],
            true,
            2,
        );
    }
}

/// Joining, creating, inviting and leaving: the ways in that have no key of their own.
/// ("New chat or room…" and "Room info" come from the keymap.)
fn collect_room_management(ctx: &PaletteContext, out: &mut Vec<Entry>) {
    use crate::manage::{Click, Stage};
    // Named, so that it is plain which room is meant: with the room list focused it
    // is the one under the cursor, not the open one.
    let (name, invite) = match &ctx.target {
        Some((name, invite)) => (crate::text::truncate(name, 30), *invite),
        None => ("this room".to_owned(), false),
    };
    let in_room = ctx.target.is_some() && !invite;
    let invite_title = format!("Invite someone to {name}…");
    let favourite_title = format!("Favourite on/off for {name}");
    let leave_title = if invite {
        format!("Decline the invitation to {name}…")
    } else {
        format!("Leave {name}…")
    };
    let entries: [(Action, &str, &[&str], bool); 6] = [
        (
            Action::Manage(Click::Stage(Stage::Join)),
            "Join a room…",
            &["join by address", "matrix.to link", "alias"],
            true,
        ),
        (
            Action::Manage(Click::Stage(Stage::Direct)),
            "Start a direct chat…",
            &["message a person", "dm", "private message", "user"],
            true,
        ),
        (
            Action::Manage(Click::Stage(Stage::Create)),
            "Create a room…",
            &["new room", "make a room"],
            true,
        ),
        (
            Action::InviteUser,
            &invite_title,
            &["add a person", "invite user", "invite to this room"],
            in_room,
        ),
        (
            Action::Manage(Click::Favourite),
            &favourite_title,
            &["favorite", "star", "pin room"],
            in_room,
        ),
        (
            Action::LeaveRoom,
            &leave_title,
            &[
                "leave room",
                "leave this room",
                "leave chat",
                "part",
                "forget room",
            ],
            ctx.target.is_some(),
        ),
    ];
    for (action, title, aliases, enabled) in entries {
        push_entry(
            out,
            Target::Action(action),
            Category::Action,
            title,
            None,
            title,
            aliases,
            enabled,
            1,
        );
    }
}

fn collect_settings(ctx: &PaletteContext, out: &mut Vec<Entry>) {
    struct SettingItem<'a> {
        target: Target,
        title: &'a str,
        hint: Option<String>,
        aliases: &'a [&'a str],
        enabled: bool,
    }

    let switch = |on: bool| if on { "on" } else { "off" }.to_owned();
    let items = [
        SettingItem {
            target: Target::Action(Action::Setting(Setting::Mouse)),
            title: Setting::Mouse.label(),
            hint: Some(switch(ctx.mouse)),
            aliases: &["toggle mouse", "setting"],
            enabled: true,
        },
        SettingItem {
            target: Target::Action(Action::Setting(Setting::TypingNotices)),
            title: Setting::TypingNotices.label(),
            hint: Some(switch(ctx.typing_notices)),
            aliases: &["toggle typing notices", "setting"],
            enabled: true,
        },
        SettingItem {
            target: Target::Action(Action::Setting(Setting::ReadReceipts)),
            title: Setting::ReadReceipts.label(),
            hint: Some(switch(ctx.read_receipts)),
            aliases: &["toggle read receipts", "setting"],
            enabled: true,
        },
        SettingItem {
            target: Target::Action(Action::Setting(Setting::ReadMarkers)),
            title: Setting::ReadMarkers.label(),
            hint: Some(switch(ctx.read_markers)),
            aliases: &["toggle read markers", "seen by", "who has read", "setting"],
            enabled: true,
        },
        SettingItem {
            target: Target::Action(Action::Setting(Setting::RoomPreviews)),
            title: Setting::RoomPreviews.label(),
            hint: Some(switch(ctx.room_previews)),
            aliases: &["toggle room previews", "latest message", "setting"],
            enabled: true,
        },
        SettingItem {
            target: Target::Action(Action::Setting(Setting::Notifications)),
            title: Setting::Notifications.label(),
            hint: Some(switch(ctx.notifications)),
            aliases: &[
                "toggle notifications",
                "alerts",
                "desktop notifications",
                "setting",
            ],
            enabled: true,
        },
        SettingItem {
            target: Target::Action(Action::Setting(Setting::DownloadDir)),
            title: Setting::DownloadDir.label(),
            hint: Some(ctx.download_dir.clone()),
            aliases: &["download directory", "save folder", "setting"],
            enabled: true,
        },
        SettingItem {
            target: Target::Action(Action::Setting(Setting::Keys)),
            title: Setting::Keys.label(),
            hint: Some(ctx.keymap.preset_summary()),
            aliases: &["shortcuts", "key bindings", "vim", "preset", "setting"],
            enabled: true,
        },
        SettingItem {
            target: Target::Action(Action::StartVerification),
            title: "Encryption / Verify device",
            hint: Some(ctx.verify_status.to_owned()),
            aliases: &["cross-signing", "verification", "security", "setting"],
            enabled: true,
        },
        SettingItem {
            target: Target::Action(Action::Setting(Setting::Logout)),
            title: Setting::Logout.label(),
            hint: ctx.user_id.clone(),
            aliases: &["logout", "sign out", "account", "setting"],
            enabled: ctx.connected,
        },
    ];

    for item in items {
        push_entry(
            out,
            item.target,
            Category::Settings,
            item.title,
            item.hint,
            item.title,
            item.aliases,
            item.enabled,
            3,
        );
    }
}

fn collect_themes(ctx: &PaletteContext, out: &mut Vec<Entry>) {
    for name in &ctx.themes {
        let active = name == &ctx.wanted_theme;
        let is_dark = ctx.theme_dark.as_deref() == Some(name.as_str());
        let is_light = ctx.theme_light.as_deref() == Some(name.as_str());
        let hint = match (active, is_dark, is_light) {
            (true, true, false) => Some("active · dark".to_owned()),
            (true, false, true) => Some("active · light".to_owned()),
            (true, _, _) => Some("active".to_owned()),
            (false, true, true) => Some("dark · light".to_owned()),
            (false, true, false) => Some("dark".to_owned()),
            (false, false, true) => Some("light".to_owned()),
            (false, false, false) => None,
        };
        push_entry(
            out,
            Target::Theme(name.clone()),
            Category::Theme,
            format!("Theme: {name}"),
            hint,
            name.clone(),
            &[],
            true,
            4,
        );
    }
}

fn score_entry(
    entry: &Entry,
    q: &str,
    pattern: &Pattern,
    matcher: &mut Matcher,
    buf: &mut Vec<char>,
) -> Option<u32> {
    let subject = &entry.subject;
    let subject_fuzzy = pattern.score(Utf32Str::new(subject, buf), matcher);
    let title_fuzzy = pattern.score(Utf32Str::new(&entry.title_lower, buf), matcher);
    let alias_fuzzy = entry
        .aliases
        .iter()
        .filter_map(|a| pattern.score(Utf32Str::new(a, buf), matcher))
        .max();

    let hint_match = !entry.hint_lower.is_empty() && entry.hint_lower.contains(q);
    let alias_contains = entry.aliases.iter().any(|a| a.contains(q));

    let tier = if subject == q {
        40_000
    } else if subject.starts_with(q) {
        30_000
    } else if subject
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| !word.is_empty() && word.starts_with(q))
    {
        24_000
    } else if entry.title_lower.starts_with(q) {
        22_000
    } else if subject.contains(q) {
        16_000
    } else if entry.title_lower.contains(q) {
        14_000
    } else if alias_contains || hint_match {
        10_000
    } else if subject_fuzzy.is_none() && title_fuzzy.is_none() && alias_fuzzy.is_none() {
        return None;
    } else {
        0
    };

    let fuzzy = subject_fuzzy
        .unwrap_or(0)
        .saturating_mul(2)
        .max(title_fuzzy.unwrap_or(0))
        .max(alias_fuzzy.unwrap_or(0));
    // Prefer rooms when the query matches the room name well, so typing e.g. "rust" or
    // "design" brings up the room before incidental command matches.
    let room_bonus = if entry.category == Category::Room
        && (subject == q
            || subject.starts_with(q)
            || subject.contains(q)
            || subject_fuzzy.is_some())
    {
        4_000
    } else {
        0
    };
    let enabled_bonus = if entry.enabled { 1_000 } else { 0 };

    Some(tier + room_bonus + enabled_bonus + fuzzy.min(3_500))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Dir;

    fn sample_context() -> PaletteContext {
        PaletteContext {
            focus: Pane::Composer,
            compose_mode: ComposeMode::Normal,
            in_invite: false,
            thread_open: false,
            has_room: true,
            has_messages: true,
            connected: false,
            user_id: None,
            mouse: true,
            typing_notices: true,
            read_receipts: true,
            read_markers: true,
            room_previews: true,
            notifications: true,
            download_dir: "~/Downloads".to_owned(),
            verify_status: "unknown",
            target: Some(("rust".to_owned(), false)),
            wanted_theme: "rat-dark".to_owned(),
            theme_dark: None,
            theme_light: None,
            themes: vec![
                "rat-dark".to_owned(),
                "gruvbox-dark".to_owned(),
                "nord".to_owned(),
            ],
            rooms: vec![
                RoomItem {
                    id: "!rust:x".to_owned(),
                    name: "rust".to_owned(),
                    unread: 0,
                    active: true,
                },
                RoomItem {
                    id: "!rat:x".to_owned(),
                    name: "matrix-rat".to_owned(),
                    unread: 0,
                    active: false,
                },
                RoomItem {
                    id: "!bob:x".to_owned(),
                    name: "bob".to_owned(),
                    unread: 2,
                    active: false,
                },
                RoomItem {
                    id: "!design:x".to_owned(),
                    name: "design".to_owned(),
                    unread: 5,
                    active: false,
                },
            ],
            selection: MessageCaps::default(),
            keymap: keymap::Keymap::default(),
        }
    }

    #[test]
    fn includes_all_entry_sources_and_excludes_cursor_noise() {
        let p = Palette::open(sample_context());
        let titles: Vec<&str> = p.entries.iter().map(|e| e.title.as_str()).collect();
        // Actions
        assert!(titles.contains(&"Settings"));
        assert!(titles.contains(&"Find a room"));
        assert!(titles.contains(&"Reply to selected message"));
        // Movement/cursor noise and room-stepping actions are excluded.
        assert!(!p.entries.iter().any(|e| matches!(
            e.target,
            Target::Action(
                Action::OpenPalette
                    | Action::FocusNext
                    | Action::FocusPrev
                    | Action::FocusDir(Dir::Left)
                    | Action::PageUp
                    | Action::SidebarUp
                    | Action::SelectOlder
                    | Action::RoomPrev
                    | Action::RoomNext
                    | Action::Send
                    | Action::Newline
            )
        )));
        // Deduplication: ToggleHelp once as Action, Open once, FocusRoomFilter once.
        assert_eq!(
            p.entries
                .iter()
                .filter(|e| e.target == Target::Action(Action::ToggleHelp))
                .count(),
            1
        );
        assert_eq!(
            p.entries
                .iter()
                .filter(|e| e.target == Target::Action(Action::Open))
                .count(),
            1
        );
        assert_eq!(
            p.entries
                .iter()
                .filter(|e| e.target == Target::Action(Action::FocusRoomFilter))
                .count(),
            1
        );
        // Thread, attach, rooms, settings, themes
        assert!(titles.contains(&"Open the message's thread"));
        assert!(titles.contains(&"Close thread"));
        assert!(titles.contains(&"Attach a file"));
        assert!(titles.contains(&"Go to bob"));
        assert!(titles.contains(&"Theme: nord"));
        assert!(titles.contains(&"Mouse"));
        assert!(titles.contains(&"Typing notices"));
        assert!(titles.contains(&"Read receipts"));
        assert!(titles.contains(&"Notifications"));
        assert!(titles.contains(&"Download folder"));
        assert!(titles.contains(&"Key bindings"));
        assert!(titles.contains(&"Encryption / Verify device"));
        assert!(titles.contains(&"Log out"));
    }

    #[test]
    fn empty_query_places_enabled_actions_and_rooms_ahead_of_disabled_entries() {
        let mut ctx = sample_context();
        ctx.focus = Pane::Timeline;
        ctx.selection = MessageCaps {
            has_selection: true,
            own: false,
            can_edit: false,
            can_copy: true,
            can_save: false,
            can_open: false,
            can_open_thread: true,
            failed_retry: false,
        };
        let p = Palette::open(ctx);
        let first_disabled = p
            .filtered
            .iter()
            .position(|&i| !p.entries[i].enabled)
            .expect("some actions are disabled");
        assert!(
            p.filtered[first_disabled..]
                .iter()
                .all(|&i| !p.entries[i].enabled)
        );
        // Contextual enabled Timeline actions come first.
        let top_titles: Vec<&str> = p.filtered[..5]
            .iter()
            .map(|&i| p.entries[i].title.as_str())
            .collect();
        assert!(top_titles.contains(&"Reply to selected message"));
        assert!(top_titles.contains(&"React with an emoji"));
        assert!(top_titles.contains(&"Copy text"));
        assert!(top_titles.contains(&"Open the message's thread"));
    }

    #[test]
    fn thread_attach_and_save_availability_follow_context() {
        let enabled = |p: &Palette, action: Action| -> bool {
            p.entries
                .iter()
                .find(|e| e.target == Target::Action(action))
                .expect("entry present")
                .enabled
        };

        let mut ctx = sample_context();
        let p = Palette::open(ctx.clone());
        assert!(!enabled(&p, Action::CloseThread));
        assert!(!enabled(&p, Action::OpenThread));
        assert!(!enabled(&p, Action::Save));
        assert!(enabled(&p, Action::Attach));

        // Inside a thread: CloseThread is enabled and ranked at top; OpenThread is disabled.
        ctx.thread_open = true;
        ctx.selection.has_selection = true;
        ctx.selection.can_save = true;
        ctx.selection.can_open_thread = false;
        let p = Palette::open(ctx.clone());
        assert!(enabled(&p, Action::CloseThread));
        assert!(!enabled(&p, Action::OpenThread));
        assert!(enabled(&p, Action::Save));
        assert_eq!(
            p.entry_at(0).map(|e| &e.target),
            Some(&Target::Action(Action::CloseThread))
        );

        // Editing disables Attach; invite rooms also disable Attach.
        ctx.thread_open = false;
        ctx.compose_mode = ComposeMode::Edit(0);
        let p = Palette::open(ctx.clone());
        assert!(!enabled(&p, Action::Attach));

        ctx.compose_mode = ComposeMode::Normal;
        ctx.in_invite = true;
        let p = Palette::open(ctx);
        assert!(!enabled(&p, Action::Attach));
    }

    #[test]
    fn room_queries_rank_matching_room_first() {
        let mut p = Palette::open(sample_context());
        p.set_query("design".into());
        assert_eq!(
            p.selected_entry().map(|e| e.title.as_str()),
            Some("Go to design")
        );

        p.set_query("matrix".into());
        assert_eq!(
            p.selected_entry().map(|e| e.title.as_str()),
            Some("Go to matrix-rat")
        );

        p.set_query("bob".into());
        assert_eq!(
            p.selected_entry().map(|e| e.title.as_str()),
            Some("Go to bob")
        );
    }

    #[test]
    fn fuzzy_query_matches_themes_settings_and_key_hints() {
        let mut p = Palette::open(sample_context());
        p.set_query("gruv".into());
        assert_eq!(
            p.selected_entry().map(|e| e.title.as_str()),
            Some("Theme: gruvbox-dark")
        );

        p.set_query("receipts".into());
        assert_eq!(
            p.selected_entry().map(|e| e.title.as_str()),
            Some("Read receipts")
        );

        p.set_query("logout".into());
        assert_eq!(
            p.selected_entry().map(|e| e.title.as_str()),
            Some("Log out")
        );
        assert!(!p.selected_entry().unwrap().enabled);

        p.set_query("ctrl-k".into());
        assert_eq!(
            p.selected_entry().map(|e| e.title.as_str()),
            Some("Find a room")
        );
    }

    #[test]
    fn step_and_scroll_clamp_to_bounds() {
        let mut p = Palette::open(sample_context());
        p.rows = 5;
        p.step(-10);
        assert_eq!(p.selected, 0);
        p.step(10_000);
        assert_eq!(p.selected, p.filtered.len() - 1);

        p.set_query("zzzz_unlikely".into());
        assert!(p.filtered.is_empty());
        p.step(1);
        p.scroll_by(5);
        assert!(p.selected_entry().is_none());
    }

    #[test]
    fn no_spurious_cross_field_fuzzy_matches() {
        let mut p = Palette::open(sample_context());
        p.set_query("rat".into());
        let titles: Vec<&str> = p
            .filtered
            .iter()
            .map(|&i| p.entries[i].title.as_str())
            .collect();
        assert!(titles.contains(&"Go to matrix-rat"));
        assert!(titles.contains(&"Theme: rat-dark"));
        assert!(!titles.contains(&"Go to rust"));
        assert!(!titles.contains(&"Room list"));
        assert!(!titles.contains(&"Emoji picker"));
    }

    #[test]
    fn room_entries_name_their_room_and_follow_what_it_is() {
        let title = |palette: &Palette, start: &str| {
            let entry = palette
                .entries
                .iter()
                .find(|e| e.title.starts_with(start))
                .unwrap_or_else(|| panic!("no entry starts with {start:?}"));
            (entry.title.clone(), entry.enabled)
        };
        // A room: everything is on, and says which room.
        let palette = Palette::open(sample_context());
        assert_eq!(title(&palette, "Leave"), ("Leave rust…".to_owned(), true));
        assert_eq!(
            title(&palette, "Invite someone"),
            ("Invite someone to rust…".to_owned(), true)
        );
        assert!(title(&palette, "Favourite on/off for rust").1);
        assert!(title(&palette, "Room info").1);
        // An invitation can be looked at and declined, nothing else.
        let mut ctx = sample_context();
        ctx.target = Some(("Party".to_owned(), true));
        let palette = Palette::open(ctx);
        assert_eq!(
            title(&palette, "Decline"),
            ("Decline the invitation to Party…".to_owned(), true)
        );
        assert!(!title(&palette, "Invite someone to Party").1);
        assert!(!title(&palette, "Favourite on/off for Party").1);
        assert!(title(&palette, "Room info").1);
        // No room to act on (even with one open behind a filtered list): all off.
        let mut ctx = sample_context();
        ctx.target = None;
        let palette = Palette::open(ctx);
        for start in ["Leave", "Invite someone", "Favourite on/off", "Room info"] {
            assert!(!title(&palette, start).1, "{start}");
        }
        // The ways to a new chat or room need no room at all.
        for start in [
            "Join a room",
            "Start a direct chat",
            "Create a room",
            "New chat or room",
        ] {
            assert!(title(&palette, start).1, "{start}");
        }
        // A long name does not push the rest of the row out.
        let mut ctx = sample_context();
        ctx.target = Some(("x".repeat(200), false));
        let palette = Palette::open(ctx);
        assert!(title(&palette, "Leave").0.chars().count() < 50);
    }
}
