//! Key bindings, presets, config loading/validation, and the key-bindings editor state.

use std::borrow::Cow;
use std::collections::BTreeSet;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::{Action, Dir, Pane};
use crate::config::{Config, KeyOverrideEdit, RawKeyConfig, RawKeyValue};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Context {
    Global,
    Sidebar,
    Timeline,
    Composer,
}

impl Context {
    pub const ALL: [Context; 4] = [
        Context::Global,
        Context::Sidebar,
        Context::Timeline,
        Context::Composer,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Context::Global => "Everywhere",
            Context::Sidebar => "Room list",
            Context::Timeline => "Messages",
            Context::Composer => "Composer",
        }
    }

    pub fn config_name(self) -> &'static str {
        match self {
            Context::Global => "global",
            Context::Sidebar => "sidebar",
            Context::Timeline => "timeline",
            Context::Composer => "composer",
        }
    }

    pub fn from_config_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "global" => Some(Context::Global),
            "sidebar" => Some(Context::Sidebar),
            "timeline" => Some(Context::Timeline),
            "composer" => Some(Context::Composer),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    #[default]
    Default,
    Vim,
}

impl Preset {
    pub const ALL: [Preset; 2] = [Preset::Default, Preset::Vim];

    pub fn name(self) -> &'static str {
        match self {
            Preset::Default => "default",
            Preset::Vim => "vim",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "default" => Some(Preset::Default),
            "vim" => Some(Preset::Vim),
            _ => None,
        }
    }

    pub fn step(self, delta: isize) -> Self {
        let len = Self::ALL.len() as isize;
        let idx = Self::ALL.iter().position(|p| *p == self).unwrap_or(0) as isize;
        Self::ALL[(idx + delta).rem_euclid(len) as usize]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionSpec {
    pub action: Action,
    pub name: &'static str,
    pub default_context: Context,
    pub description: &'static str,
}

macro_rules! action_table {
    ($( Action::$variant:ident $( ( $arg:path ) )? => ($name:literal, $ctx:expr, $desc:literal) ),* $(,)?) => {
        /// Every user-bindable action, derived once from `Action` so config names, help text,
        /// editor rows, and documentation share a single source of truth.
        pub const ACTIONS: &[ActionSpec] = &[
            $(
                ActionSpec {
                    action: Action::$variant $( ( $arg ) )?,
                    name: $name,
                    default_context: $ctx,
                    description: $desc,
                },
            )*
        ];

        /// Returns metadata for any key-bindable `Action`, or `None` for modal/click-only actions.
        pub fn action_spec(action: Action) -> Option<ActionSpec> {
            match action {
                $(
                    Action::$variant $( ( $arg ) )? => Some(ActionSpec {
                        action: Action::$variant $( ( $arg ) )?,
                        name: $name,
                        default_context: $ctx,
                        description: $desc,
                    }),
                )*
                Action::LoginSubmit
                | Action::LoginBrowser
                | Action::RecoverySubmit
                | Action::CloseModal
                | Action::Verify(_)
                | Action::Manage(_)
                | Action::OpenRoom(_)
                | Action::SelectTheme(_)
                | Action::Setting(_) => None,
            }
        }
    };
}

action_table! {
    Action::Quit => ("quit", Context::Global, "Quit"),
    Action::Suspend => ("suspend", Context::Global, "Suspend to the shell"),
    Action::FocusNext => ("focus_next", Context::Global, "Next pane"),
    Action::FocusPrev => ("focus_prev", Context::Global, "Previous pane"),
    Action::Focus(Pane::Sidebar) => ("focus_sidebar", Context::Global, "Room list"),
    Action::Focus(Pane::Timeline) => ("focus_timeline", Context::Global, "Messages"),
    Action::Focus(Pane::Composer) => ("focus_composer", Context::Global, "Composer"),
    Action::FocusDir(Dir::Left) => ("focus_left", Context::Global, "Go to the room list"),
    Action::FocusDir(Dir::Right) => ("focus_right", Context::Global, "Go back to the chat"),
    Action::FocusDir(Dir::Up) => ("focus_up", Context::Global, "Go to the messages"),
    Action::FocusDir(Dir::Down) => ("focus_down", Context::Global, "Go to the composer"),
    Action::FocusRoomFilter => ("focus_room_filter", Context::Global, "Find a room"),
    Action::ToggleHelp => ("help", Context::Global, "This help"),
    Action::CycleTheme => ("cycle_theme", Context::Global, "Next theme"),
    Action::Back => ("back", Context::Global, "Close, cancel or go back"),
    Action::PageUp => ("page_up", Context::Global, "Scroll messages up"),
    Action::PageDown => ("page_down", Context::Global, "Scroll messages down"),
    Action::OpenEmoji => ("emoji", Context::Global, "Emoji picker"),
    Action::Attach => ("attach", Context::Global, "Attach a file"),
    Action::Search => ("search", Context::Global, "Find in this room"),
    Action::SearchEverywhere => ("search_all", Context::Global, "Search all rooms"),
    Action::SpaceNext => ("space_next", Context::Global, "Next space (then all rooms)"),
    Action::SpacePrev => ("space_prev", Context::Global, "Previous space"),
    Action::RoomPrev => ("room_prev", Context::Global, "Previous room"),
    Action::RoomNext => ("room_next", Context::Global, "Next room"),
    Action::OpenSettings => ("settings", Context::Global, "Settings"),
    Action::OpenPalette => ("palette", Context::Global, "Command palette"),
    Action::NewRoom => ("new_room", Context::Global, "New chat or room…"),
    Action::RoomInfo => ("room_info", Context::Global, "Room info and members"),
    Action::LeaveRoom => ("leave_room", Context::Global, "Leave this room"),
    Action::InviteUser => ("invite_user", Context::Global, "Invite someone to this room"),
    Action::AcceptInvite => ("accept_invite", Context::Global, "Accept invitation"),
    Action::DeclineInvite => ("decline_invite", Context::Global, "Decline invitation"),
    Action::StartVerification => ("start_verification", Context::Global, "Encryption / Verify device"),
    Action::CloseThread => ("close_thread", Context::Global, "Close thread"),
    Action::ContextMenu => ("context_menu", Context::Global, "Menu for what has the focus"),
    Action::SidebarUp => ("sidebar_up", Context::Sidebar, "Previous room"),
    Action::SidebarDown => ("sidebar_down", Context::Sidebar, "Next room"),
    Action::SidebarTop => ("sidebar_top", Context::Sidebar, "First room"),
    Action::SidebarBottom => ("sidebar_bottom", Context::Sidebar, "Last room"),
    Action::SidebarOpen => ("sidebar_open", Context::Sidebar, "Open room and start typing"),
    Action::SelectOlder => ("select_older", Context::Timeline, "Select older message"),
    Action::SelectNewer => ("select_newer", Context::Timeline, "Newer message, then composer"),
    Action::Reply => ("reply", Context::Timeline, "Reply to selected message"),
    Action::Edit => ("edit", Context::Timeline, "Edit (own messages)"),
    Action::React => ("react", Context::Timeline, "React with an emoji"),
    Action::Delete => ("delete", Context::Timeline, "Delete (own messages)"),
    Action::Copy => ("copy", Context::Timeline, "Copy text"),
    Action::Save => ("save", Context::Timeline, "Save file or picture"),
    Action::Open => ("open", Context::Timeline, "Open link, picture or file"),
    Action::OpenThread => ("open_thread", Context::Timeline, "Open the message's thread"),
    Action::JumpToNewest => ("jump_to_newest", Context::Timeline, "Jump to newest"),
    Action::JumpToOldest => ("jump_to_oldest", Context::Timeline, "Jump to oldest"),
    Action::Send => ("send", Context::Composer, "Send"),
    Action::Newline => ("newline", Context::Composer, "Line break"),
    Action::CancelCompose => ("cancel_compose", Context::Composer, "Cancel reply or edit"),
}

pub fn action_by_name(name: &str) -> Option<ActionSpec> {
    let key = name.trim().to_ascii_lowercase();
    ACTIONS.iter().copied().find(|spec| spec.name == key)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub context: Context,
    pub code: KeyCode,
    pub mods: KeyModifiers,
    pub action: Action,
    pub label: Cow<'static, str>,
    pub description: &'static str,
}

const NONE: KeyModifiers = KeyModifiers::NONE;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;
const ALT: KeyModifiers = KeyModifiers::ALT;
const SHIFT: KeyModifiers = KeyModifiers::SHIFT;

const fn bind(
    context: Context,
    code: KeyCode,
    mods: KeyModifiers,
    action: Action,
    label: &'static str,
    description: &'static str,
) -> Binding {
    Binding {
        context,
        code,
        mods,
        action,
        label: Cow::Borrowed(label),
        description,
    }
}

use Context::{Composer, Global, Sidebar, Timeline};
use KeyCode::Char;

/// The default keymap. Order is the order shown in the help overlay.
pub const DEFAULT: &[Binding] = &[
    bind(
        Global,
        KeyCode::Tab,
        NONE,
        Action::FocusNext,
        "Tab",
        "Next pane",
    ),
    bind(
        Global,
        KeyCode::BackTab,
        NONE,
        Action::FocusPrev,
        "Shift-Tab",
        "Previous pane",
    ),
    bind(
        Global,
        KeyCode::Left,
        ALT,
        Action::FocusDir(Dir::Left),
        "Alt-←",
        "Go to the room list",
    ),
    bind(
        Global,
        KeyCode::Right,
        ALT,
        Action::FocusDir(Dir::Right),
        "Alt-→",
        "Go back to the chat",
    ),
    bind(
        Global,
        KeyCode::Up,
        ALT,
        Action::FocusDir(Dir::Up),
        "Alt-↑",
        "Go to the messages",
    ),
    bind(
        Global,
        KeyCode::Down,
        ALT,
        Action::FocusDir(Dir::Down),
        "Alt-↓",
        "Go to the composer",
    ),
    bind(
        Global,
        Char('1'),
        ALT,
        Action::Focus(Pane::Sidebar),
        "Alt-1",
        "Room list",
    ),
    bind(
        Global,
        Char('2'),
        ALT,
        Action::Focus(Pane::Timeline),
        "Alt-2",
        "Messages",
    ),
    bind(
        Global,
        Char('3'),
        ALT,
        Action::Focus(Pane::Composer),
        "Alt-3",
        "Composer",
    ),
    bind(
        Global,
        Char('r'),
        ALT,
        Action::FocusRoomFilter,
        "Alt-R",
        "Filter rooms",
    ),
    bind(
        Global,
        KeyCode::PageUp,
        NONE,
        Action::PageUp,
        "PgUp",
        "Scroll messages up",
    ),
    bind(
        Global,
        KeyCode::PageDown,
        NONE,
        Action::PageDown,
        "PgDn",
        "Scroll messages down",
    ),
    bind(
        Global,
        KeyCode::Esc,
        NONE,
        Action::Back,
        "Esc",
        "Close, cancel or go back",
    ),
    bind(
        Global,
        Char('e'),
        ALT,
        Action::OpenEmoji,
        "Alt-E",
        "Emoji picker",
    ),
    bind(
        Global,
        Char('a'),
        ALT,
        Action::Attach,
        "Alt-A",
        "Attach a file",
    ),
    bind(
        Global,
        KeyCode::F(1),
        NONE,
        Action::ToggleHelp,
        "F1",
        "This help",
    ),
    bind(
        Global,
        KeyCode::Up,
        CTRL,
        Action::RoomPrev,
        "Ctrl-↑",
        "Previous room",
    ),
    bind(
        Global,
        KeyCode::Down,
        CTRL,
        Action::RoomNext,
        "Ctrl-↓",
        "Next room",
    ),
    bind(
        Global,
        Char('f'),
        CTRL,
        Action::Search,
        "Ctrl-F",
        "Find in this room",
    ),
    bind(
        Global,
        Char('g'),
        CTRL,
        Action::SearchEverywhere,
        "Ctrl-G",
        "Search all rooms",
    ),
    bind(
        Global,
        Char('s'),
        ALT,
        Action::SpaceNext,
        "Alt-S",
        "Next space (then all rooms)",
    ),
    bind(
        Global,
        Char('k'),
        CTRL,
        Action::FocusRoomFilter,
        "Ctrl-K",
        "Find a room",
    ),
    bind(
        Global,
        KeyCode::F(2),
        NONE,
        Action::OpenSettings,
        "F2",
        "Settings",
    ),
    bind(
        Global,
        KeyCode::F(3),
        NONE,
        Action::CycleTheme,
        "F3",
        "Next theme",
    ),
    bind(
        Global,
        Char('p'),
        CTRL,
        Action::OpenPalette,
        "Ctrl-P",
        "Command palette",
    ),
    bind(
        Global,
        Char('n'),
        CTRL,
        Action::NewRoom,
        "Ctrl-N",
        "New chat or room…",
    ),
    bind(
        Global,
        KeyCode::F(4),
        NONE,
        Action::RoomInfo,
        "F4",
        "Room info and members",
    ),
    bind(
        Global,
        KeyCode::Menu,
        NONE,
        Action::ContextMenu,
        "Menu",
        "Menu for what has the focus",
    ),
    bind(
        Global,
        Char('z'),
        CTRL,
        Action::Suspend,
        "Ctrl-Z",
        "Suspend to the shell",
    ),
    bind(Global, Char('q'), CTRL, Action::Quit, "Ctrl-Q", "Quit"),
    bind(
        Sidebar,
        KeyCode::Up,
        NONE,
        Action::SidebarUp,
        "↑",
        "Previous room",
    ),
    bind(
        Sidebar,
        KeyCode::Down,
        NONE,
        Action::SidebarDown,
        "↓",
        "Next room",
    ),
    bind(
        Sidebar,
        KeyCode::Enter,
        NONE,
        Action::SidebarOpen,
        "Enter",
        "Open room and start typing",
    ),
    bind(
        Sidebar,
        Char('?'),
        NONE,
        Action::ToggleHelp,
        "?",
        "This help",
    ),
    bind(
        Timeline,
        KeyCode::Up,
        NONE,
        Action::SelectOlder,
        "↑",
        "Select older message",
    ),
    bind(
        Timeline,
        KeyCode::Down,
        NONE,
        Action::SelectNewer,
        "↓",
        "Newer message, then composer",
    ),
    bind(
        Timeline,
        KeyCode::Enter,
        NONE,
        Action::Reply,
        "Enter",
        "Reply to selected message",
    ),
    bind(
        Timeline,
        Char('e'),
        NONE,
        Action::Edit,
        "e",
        "Edit (own messages)",
    ),
    bind(
        Timeline,
        Char('r'),
        NONE,
        Action::React,
        "r",
        "React with an emoji",
    ),
    bind(
        Timeline,
        KeyCode::Delete,
        NONE,
        Action::Delete,
        "Del",
        "Delete (own messages)",
    ),
    bind(Timeline, Char('c'), NONE, Action::Copy, "c", "Copy text"),
    bind(
        Timeline,
        Char('s'),
        NONE,
        Action::Save,
        "s",
        "Save file or picture",
    ),
    bind(Timeline, Char('v'), NONE, Action::Open, "v", "View picture"),
    bind(
        Timeline,
        Char('o'),
        NONE,
        Action::Open,
        "o",
        "Open link, picture or file",
    ),
    bind(
        Timeline,
        Char('t'),
        NONE,
        Action::OpenThread,
        "t",
        "Open the message's thread",
    ),
    bind(
        Timeline,
        Char('m'),
        NONE,
        Action::ContextMenu,
        "m",
        "Menu for the message",
    ),
    bind(
        Timeline,
        KeyCode::End,
        NONE,
        Action::JumpToNewest,
        "End",
        "Jump to newest",
    ),
    bind(
        Timeline,
        Char('?'),
        NONE,
        Action::ToggleHelp,
        "?",
        "This help",
    ),
    bind(
        Composer,
        KeyCode::Enter,
        NONE,
        Action::Send,
        "Enter",
        "Send",
    ),
    bind(
        Composer,
        KeyCode::Enter,
        SHIFT,
        Action::Newline,
        "Shift-Enter",
        "Line break",
    ),
    bind(
        Composer,
        KeyCode::Enter,
        ALT,
        Action::Newline,
        "Alt-Enter",
        "Line break (fallback)",
    ),
    bind(
        Composer,
        KeyCode::Up,
        NONE,
        Action::SelectOlder,
        "↑",
        "First line: select a message",
    ),
];

/// Extra bindings added on top of `DEFAULT` when `preset = "vim"`.
pub const VIM_EXTRA: &[Binding] = &[
    bind(
        Sidebar,
        Char('j'),
        NONE,
        Action::SidebarDown,
        "j",
        "Next room",
    ),
    bind(
        Sidebar,
        Char('k'),
        NONE,
        Action::SidebarUp,
        "k",
        "Previous room",
    ),
    bind(
        Sidebar,
        Char('g'),
        NONE,
        Action::SidebarTop,
        "g",
        "First room",
    ),
    bind(
        Sidebar,
        Char('G'),
        NONE,
        Action::SidebarBottom,
        "G",
        "Last room",
    ),
    bind(
        Sidebar,
        Char('l'),
        NONE,
        Action::Focus(Pane::Timeline),
        "l",
        "Messages",
    ),
    bind(
        Sidebar,
        Char('i'),
        NONE,
        Action::Focus(Pane::Composer),
        "i",
        "Composer",
    ),
    bind(
        Sidebar,
        Char('/'),
        NONE,
        Action::Search,
        "/",
        "Find in this room",
    ),
    bind(
        Timeline,
        Char('j'),
        NONE,
        Action::SelectNewer,
        "j",
        "Newer message, then composer",
    ),
    bind(
        Timeline,
        Char('k'),
        NONE,
        Action::SelectOlder,
        "k",
        "Select older message",
    ),
    bind(
        Timeline,
        Char('g'),
        NONE,
        Action::JumpToOldest,
        "g",
        "Jump to oldest",
    ),
    bind(
        Timeline,
        Char('G'),
        NONE,
        Action::JumpToNewest,
        "G",
        "Jump to newest",
    ),
    bind(
        Timeline,
        Char('h'),
        NONE,
        Action::Focus(Pane::Sidebar),
        "h",
        "Room list",
    ),
    bind(
        Timeline,
        Char('l'),
        NONE,
        Action::Focus(Pane::Composer),
        "l",
        "Composer",
    ),
    bind(
        Timeline,
        Char('i'),
        NONE,
        Action::Focus(Pane::Composer),
        "i",
        "Composer",
    ),
    bind(
        Timeline,
        Char('/'),
        NONE,
        Action::Search,
        "/",
        "Find in this room",
    ),
    bind(
        Composer,
        KeyCode::Esc,
        NONE,
        Action::Focus(Pane::Timeline),
        "Esc",
        "Messages",
    ),
];

/// Makes equivalent key events compare equal: letters with Ctrl/Alt ignore case and
/// Shift, symbols typed with Shift (like `?`) ignore Shift, Shift-Tab is BackTab.
pub fn normalize(key: KeyEvent) -> (KeyCode, KeyModifiers) {
    let mut mods = key.modifiers & (CTRL | ALT | SHIFT);
    let code = match key.code {
        Char(c) => {
            mods -= SHIFT;
            if mods.is_empty() {
                Char(c)
            } else {
                Char(c.to_ascii_lowercase())
            }
        }
        KeyCode::BackTab => {
            mods -= SHIFT;
            KeyCode::BackTab
        }
        KeyCode::Tab if mods.contains(SHIFT) => {
            mods -= SHIFT;
            KeyCode::BackTab
        }
        other => other,
    };
    (code, mods)
}

/// Validates that `(code, mods)` is representable cleanly in terminal input and allowed in `context`.
pub fn validate_binding(context: Context, code: KeyCode, mods: KeyModifiers) -> Result<(), String> {
    if mods == CTRL {
        match code {
            Char('i') => return Err("Ctrl-I collides with Tab in terminal input".to_owned()),
            Char('m') => return Err("Ctrl-M collides with Enter in terminal input".to_owned()),
            Char('[') => return Err("Ctrl-[ collides with Esc in terminal input".to_owned()),
            Char('c') => return Err("Ctrl-C is reserved to clear the draft".to_owned()),
            _ => {}
        }
    }
    if context == Context::Composer && mods.is_empty() && matches!(code, Char(_)) {
        return Err("plain characters always type in the Composer".to_owned());
    }
    Ok(())
}

/// Parses a key spec such as `"ctrl-q"`, `"alt-left"`, `"shift-enter"`, `"f5"`, or `"R"`.
pub fn parse_key(context: Context, spec: &str) -> Result<(KeyCode, KeyModifiers), String> {
    let mut rest = spec.trim();
    if rest.is_empty() {
        return Err("empty key binding".to_owned());
    }
    let mut mods = KeyModifiers::NONE;
    loop {
        let matched = if rest.len() > 5 && rest[..5].eq_ignore_ascii_case("ctrl-") {
            Some((CTRL, "ctrl", 5))
        } else if rest.len() > 4 && rest[..4].eq_ignore_ascii_case("alt-") {
            Some((ALT, "alt", 4))
        } else if rest.len() > 6 && rest[..6].eq_ignore_ascii_case("shift-") {
            Some((SHIFT, "shift", 6))
        } else {
            None
        };
        let Some((flag, name, len)) = matched else {
            break;
        };
        if mods.contains(flag) {
            return Err(format!("duplicate modifier \"{name}\" in \"{spec}\""));
        }
        mods |= flag;
        rest = &rest[len..];
    }
    if rest.is_empty() {
        return Err(format!("missing key name in \"{spec}\""));
    }

    let raw_code = match rest.to_ascii_lowercase().as_str() {
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "space" => Char(' '),
        "backspace" | "bs" => KeyCode::Backspace,
        "del" | "delete" => KeyCode::Delete,
        "ins" | "insert" => KeyCode::Insert,
        "menu" => KeyCode::Menu,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pgup" | "pageup" | "page_up" | "page-up" => KeyCode::PageUp,
        "pgdn" | "pagedown" | "page_down" | "page-down" => KeyCode::PageDown,
        "up" | "↑" => KeyCode::Up,
        "down" | "↓" => KeyCode::Down,
        "left" | "←" => KeyCode::Left,
        "right" | "→" => KeyCode::Right,
        other
            if other.starts_with('f')
                && other.len() >= 2
                && other[1..].bytes().all(|b| b.is_ascii_digit()) =>
        {
            let n: u8 = other[1..]
                .parse()
                .map_err(|_| format!("invalid function key \"{rest}\""))?;
            if !(1..=12).contains(&n) {
                return Err(format!("function key F{n} out of supported range F1..F12"));
            }
            KeyCode::F(n)
        }
        _ => {
            let mut chars = rest.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if !c.is_control() => Char(c),
                _ => {
                    return Err(format!(
                        "unknown key \"{rest}\" (supported modifiers: ctrl-, alt-, shift-)"
                    ));
                }
            }
        }
    };

    if matches!(raw_code, Char(_)) && mods.contains(SHIFT) {
        if mods.intersects(CTRL | ALT) {
            return Err(format!(
                "\"{spec}\" combines Shift with Ctrl/Alt on a character, which terminals fold together; omit shift-"
            ));
        }
        return Err(format!(
            "\"{spec}\" uses shift- on a character; write the shifted character directly (for example \"R\")"
        ));
    }

    let (code, norm_mods) = normalize(KeyEvent::new(raw_code, mods));
    validate_binding(context, code, norm_mods)?;
    Ok((code, norm_mods))
}

/// Validates a live key event from the interactive capture mode.
pub fn from_key_event(context: Context, key: KeyEvent) -> Result<(KeyCode, KeyModifiers), String> {
    let raw_mods = key.modifiers & (CTRL | ALT | SHIFT);
    if matches!(key.code, Char(_)) && raw_mods.contains(SHIFT) && raw_mods.intersects(CTRL | ALT) {
        return Err(
            "Ctrl/Alt+Shift with a letter cannot be distinguished reliably from Ctrl/Alt in terminal input"
                .to_owned(),
        );
    }
    if matches!(key.code, Char(c) if c.is_control()) {
        return Err("control character cannot be bound".to_owned());
    }
    let (code, mods) = normalize(key);
    match code {
        Char(_)
        | KeyCode::Enter
        | KeyCode::Esc
        | KeyCode::Tab
        | KeyCode::BackTab
        | KeyCode::Backspace
        | KeyCode::Delete
        | KeyCode::Insert
        | KeyCode::Home
        | KeyCode::End
        | KeyCode::PageUp
        | KeyCode::PageDown
        | KeyCode::Up
        | KeyCode::Down
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Menu
        | KeyCode::F(1..=12) => {}
        _ => return Err("unsupported key for key bindings".to_owned()),
    }
    validate_binding(context, code, mods)?;
    Ok((code, mods))
}

/// Formats `(code, mods)` for display in the help overlay, status bar, palette, and editor.
pub fn format_label(code: KeyCode, mods: KeyModifiers) -> String {
    let mut out = String::new();
    if mods.contains(CTRL) {
        out.push_str("Ctrl-");
    }
    if mods.contains(ALT) {
        out.push_str("Alt-");
    }
    if mods.contains(SHIFT) && code != KeyCode::BackTab {
        out.push_str("Shift-");
    }
    match code {
        KeyCode::Enter => out.push_str("Enter"),
        KeyCode::Esc => out.push_str("Esc"),
        KeyCode::Tab => out.push_str("Tab"),
        KeyCode::BackTab => out.push_str("Shift-Tab"),
        KeyCode::Backspace => out.push_str("Backspace"),
        KeyCode::Delete => out.push_str("Del"),
        KeyCode::Insert => out.push_str("Ins"),
        KeyCode::Menu => out.push_str("Menu"),
        KeyCode::Home => out.push_str("Home"),
        KeyCode::End => out.push_str("End"),
        KeyCode::PageUp => out.push_str("PgUp"),
        KeyCode::PageDown => out.push_str("PgDn"),
        KeyCode::Up => out.push('↑'),
        KeyCode::Down => out.push('↓'),
        KeyCode::Left => out.push('←'),
        KeyCode::Right => out.push('→'),
        KeyCode::F(n) => out.push_str(&format!("F{n}")),
        Char(' ') => out.push_str("Space"),
        Char(c) => {
            if mods.is_empty() {
                out.push(c);
            } else {
                out.push(c.to_ascii_uppercase());
            }
        }
        other => out.push_str(&format!("{other:?}")),
    }
    out
}

/// Formats `(code, mods)` in canonical `config.toml` syntax that round-trips through `parse_key`.
pub fn format_config_key(code: KeyCode, mods: KeyModifiers) -> String {
    let mut out = String::new();
    if mods.contains(CTRL) {
        out.push_str("ctrl-");
    }
    if mods.contains(ALT) {
        out.push_str("alt-");
    }
    if mods.contains(SHIFT) && code != KeyCode::BackTab {
        out.push_str("shift-");
    }
    match code {
        KeyCode::Enter => out.push_str("enter"),
        KeyCode::Esc => out.push_str("esc"),
        KeyCode::Tab => out.push_str("tab"),
        KeyCode::BackTab => out.push_str("backtab"),
        KeyCode::Backspace => out.push_str("backspace"),
        KeyCode::Delete => out.push_str("del"),
        KeyCode::Insert => out.push_str("ins"),
        KeyCode::Menu => out.push_str("menu"),
        KeyCode::Home => out.push_str("home"),
        KeyCode::End => out.push_str("end"),
        KeyCode::PageUp => out.push_str("pgup"),
        KeyCode::PageDown => out.push_str("pgdn"),
        KeyCode::Up => out.push_str("up"),
        KeyCode::Down => out.push_str("down"),
        KeyCode::Left => out.push_str("left"),
        KeyCode::Right => out.push_str("right"),
        KeyCode::F(n) => out.push_str(&format!("f{n}")),
        Char(' ') => out.push_str("space"),
        Char(c) => {
            if mods.is_empty() {
                out.push(c);
            } else {
                out.push(c.to_ascii_lowercase());
            }
        }
        other => out.push_str(&format!("{other:?}").to_ascii_lowercase()),
    }
    out
}

/// Effective keymap built from a preset plus any valid user overrides.
type ResolvedRawEntry = (String, Context, ActionSpec, Vec<(KeyCode, KeyModifiers)>);

struct ConflictLog<'a> {
    source: &'a str,
    warnings: &'a mut Vec<String>,
    deferred: &'a [(Context, Action)],
}

#[derive(Debug, Clone)]
pub struct Keymap {
    preset: Preset,
    bindings: Vec<Binding>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self::from_preset(Preset::Default)
    }
}

impl Keymap {
    pub fn preset_bindings(preset: Preset) -> Vec<Binding> {
        let mut bindings = DEFAULT.to_vec();
        if preset == Preset::Vim {
            for extra in VIM_EXTRA {
                // Remove any duplicate in the same (context, code, mods) slot before appending.
                bindings.retain(|b| {
                    !(b.context == extra.context && b.code == extra.code && b.mods == extra.mods)
                });
                bindings.push(extra.clone());
            }
        }
        bindings
    }

    pub fn from_preset(preset: Preset) -> Self {
        Self {
            preset,
            bindings: Self::preset_bindings(preset),
        }
    }

    /// Loads `preset` and `[keys]` from `Config`. Never fails: invalid entries record warnings and
    /// leave `Action::Quit` reachable.
    pub fn from_config(config: &Config) -> (Self, Vec<String>) {
        Self::from_raw(&config.raw_key_config())
    }

    pub fn from_raw(raw: &RawKeyConfig) -> (Self, Vec<String>) {
        let mut warnings = raw.syntax_warnings.clone();
        let chosen_preset = match (&raw.keys_preset, &raw.top_preset) {
            (Some(res), _) => Some(res),
            (None, Some(res)) => Some(res),
            (None, None) => None,
        };
        let preset = match chosen_preset {
            Some(Ok(name)) => match Preset::from_name(name) {
                Some(p) => p,
                None => {
                    warnings.push(format!(
                        "unknown key preset \"{name}\" (expected \"default\" or \"vim\")"
                    ));
                    Preset::Default
                }
            },
            Some(Err(err)) => {
                warnings.push(err.clone());
                Preset::Default
            }
            None => Preset::Default,
        };

        let mut map = Self::from_preset(preset);
        let mut resolved: Vec<ResolvedRawEntry> = Vec::new();

        for (action_name, val) in &raw.root_entries {
            if let Some(entry) =
                Self::parse_raw_entry("[keys]", None, action_name, val, &mut warnings)
            {
                resolved.push(entry);
            }
        }
        for (section_name, entries) in &raw.sections {
            let Some(ctx) = Context::from_config_name(section_name) else {
                warnings.push(format!(
                    "unknown key context \"[keys.{section_name}]\" (expected global, sidebar, timeline, composer)"
                ));
                continue;
            };
            let source = format!("[keys.{section_name}]");
            for (action_name, val) in entries {
                if let Some(entry) =
                    Self::parse_raw_entry(&source, Some(ctx), action_name, val, &mut warnings)
                {
                    resolved.push(entry);
                }
            }
        }

        // Actions explicitly set later in the same config will clear/replace their own preset keys,
        // so moving an action's old key to another action in the same file stays warning-free.
        let remaining_actions: Vec<(Context, Action)> = resolved
            .iter()
            .map(|(_, ctx, spec, _)| (*ctx, spec.action))
            .collect();
        for (idx, (source, ctx, spec, keys)) in resolved.iter().enumerate() {
            map.replace_action_bindings(
                *ctx,
                *spec,
                keys,
                Some(ConflictLog {
                    source: source.as_str(),
                    warnings: &mut warnings,
                    deferred: &remaining_actions[idx + 1..],
                }),
            );
        }

        if !map.bindings.iter().any(|b| b.action == Action::Quit) {
            map.enforce_quit_rail(&mut warnings);
        }
        (map, warnings)
    }

    fn parse_raw_entry(
        source: &str,
        explicit_ctx: Option<Context>,
        action_name: &str,
        value: &RawKeyValue,
        warnings: &mut Vec<String>,
    ) -> Option<ResolvedRawEntry> {
        let Some(spec) = action_by_name(action_name) else {
            warnings.push(format!(
                "{source}.{action_name}: unknown action \"{action_name}\""
            ));
            return None;
        };
        let ctx = explicit_ctx.unwrap_or(spec.default_context);
        let keys = match value {
            RawKeyValue::Invalid(err) => {
                warnings.push(format!("{source}.{action_name}: {err}"));
                return None;
            }
            RawKeyValue::Str(s) if s.trim().eq_ignore_ascii_case("none") => Some(Vec::new()),
            RawKeyValue::Str(s) => match parse_key(ctx, s) {
                Ok(key) => Some(vec![key]),
                Err(err) => {
                    warnings.push(format!("{source}.{action_name}: {err}"));
                    None
                }
            },
            RawKeyValue::List(items) if items.is_empty() => Some(Vec::new()),
            RawKeyValue::List(items) => {
                let mut valid = Vec::new();
                for item in items {
                    match item {
                        Ok(s) => match parse_key(ctx, s) {
                            Ok(key) => {
                                if !valid.contains(&key) {
                                    valid.push(key);
                                }
                            }
                            Err(err) => warnings.push(format!("{source}.{action_name}: {err}")),
                        },
                        Err(err) => warnings.push(format!("{source}.{action_name}: {err}")),
                    }
                }
                (!valid.is_empty()).then_some(valid)
            }
        }?;
        Some((source.to_owned(), ctx, spec, keys))
    }

    fn replace_action_bindings(
        &mut self,
        ctx: Context,
        spec: ActionSpec,
        keys: &[(KeyCode, KeyModifiers)],
        mut conflict_log: Option<ConflictLog<'_>>,
    ) {
        let mut preserved_desc: Option<&'static str> = None;
        let mut insert_at: Option<usize> = None;
        let mut i = 0;
        while i < self.bindings.len() {
            if self.bindings[i].context == ctx && self.bindings[i].action == spec.action {
                insert_at.get_or_insert(i);
                preserved_desc.get_or_insert(self.bindings[i].description);
                self.bindings.remove(i);
            } else {
                i += 1;
            }
        }
        let desc = preserved_desc.unwrap_or(spec.description);

        // Dedup conflicts in reverse or forward order: last one wins cleanly.
        let mut to_insert = Vec::with_capacity(keys.len());
        for &(code, mods) in keys {
            let mut j = 0;
            while j < self.bindings.len() {
                if self.bindings[j].context == ctx
                    && self.bindings[j].code == code
                    && self.bindings[j].mods == mods
                {
                    let prev = self.bindings.remove(j);
                    if let Some(ref mut log) = conflict_log
                        && !log.deferred.contains(&(prev.context, prev.action))
                    {
                        let prev_name = action_spec(prev.action).map_or("?", |s| s.name);
                        log.warnings.push(format!(
                            "{}.{}: '{}' overrides '{}' in {}",
                            log.source,
                            spec.name,
                            format_label(code, mods),
                            prev_name,
                            ctx.title()
                        ));
                    }
                    if let Some(pos) = insert_at
                        && j < pos
                    {
                        insert_at = Some(pos - 1);
                    }
                } else {
                    j += 1;
                }
            }
            to_insert.push(Binding {
                context: ctx,
                code,
                mods,
                action: spec.action,
                label: Cow::Owned(format_label(code, mods)),
                description: desc,
            });
        }

        let at = insert_at
            .unwrap_or_else(|| self.insert_position_for(ctx))
            .min(self.bindings.len());
        self.bindings.splice(at..at, to_insert);
    }

    fn insert_position_for(&self, ctx: Context) -> usize {
        if let Some(last) = self.bindings.iter().rposition(|b| b.context == ctx) {
            return last + 1;
        }
        let rank = Context::ALL.iter().position(|c| *c == ctx).unwrap_or(3);
        self.bindings
            .iter()
            .position(|b| {
                Context::ALL
                    .iter()
                    .position(|c| *c == b.context)
                    .unwrap_or(3)
                    > rank
            })
            .unwrap_or(self.bindings.len())
    }

    fn enforce_quit_rail(&mut self, warnings: &mut Vec<String>) {
        let code = Char('q');
        let mods = CTRL;
        self.bindings
            .retain(|b| !(b.context == Context::Global && b.code == code && b.mods == mods));
        let at = self.insert_position_for(Context::Global);
        self.bindings.insert(
            at,
            bind(Context::Global, code, mods, Action::Quit, "Ctrl-Q", "Quit"),
        );
        warnings.push("Quit cannot be unbound; restored Ctrl-Q in Everywhere".to_owned());
    }

    pub fn preset(&self) -> Preset {
        self.preset
    }

    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    /// True if any binding differs from `self.preset`'s clean defaults.
    pub fn has_overrides(&self) -> bool {
        let base = Self::preset_bindings(self.preset);
        for ctx in Context::ALL {
            for spec in ACTIONS {
                let cur: Vec<_> = self
                    .bindings
                    .iter()
                    .filter(|b| b.context == ctx && b.action == spec.action)
                    .map(|b| (b.code, b.mods))
                    .collect();
                let def: Vec<_> = base
                    .iter()
                    .filter(|b| b.context == ctx && b.action == spec.action)
                    .map(|b| (b.code, b.mods))
                    .collect();
                if cur != def {
                    return true;
                }
            }
        }
        false
    }

    /// Short description shown in Settings and Command Palette (e.g. `"default"`, `"vim"`, `"vim (edited)"`).
    pub fn preset_summary(&self) -> String {
        if self.has_overrides() {
            format!("{} (edited)", self.preset.name())
        } else {
            self.preset.name().to_owned()
        }
    }

    /// The action bound to `key` in `context`, falling back to global bindings.
    /// Plain printable characters in `Composer` never resolve to an action so typing always works.
    pub fn lookup(&self, context: Context, key: KeyEvent) -> Option<Action> {
        let (code, mods) = normalize(key);
        if context == Context::Composer && mods.is_empty() && matches!(code, Char(_)) {
            return None;
        }
        self.lookup_in(context, code, mods)
            .or_else(|| self.lookup_in(Context::Global, code, mods))
    }

    fn lookup_in(&self, context: Context, code: KeyCode, mods: KeyModifiers) -> Option<Action> {
        self.bindings
            .iter()
            .find(|b| b.context == context && b.code == code && b.mods == mods)
            .map(|b| b.action)
    }

    /// Matches global `Quit` or `Suspend` shortcuts behind popups/viewer without intercepting
    /// plain characters that belong in popup text inputs.
    pub fn lookup_popup_global(&self, code: KeyCode, mods: KeyModifiers) -> Option<Action> {
        if !(mods.intersects(CTRL | ALT) || matches!(code, KeyCode::F(_))) {
            return None;
        }
        self.lookup_in(Context::Global, code, mods)
            .filter(|a| matches!(a, Action::Quit | Action::Suspend))
    }

    /// First key label for `(context, action)` without falling back to other contexts.
    pub fn label_in(&self, context: Context, action: Action) -> Option<&str> {
        self.bindings
            .iter()
            .find(|b| b.context == context && b.action == action)
            .map(|b| b.label.as_ref())
    }

    /// Last key label for `(context, action)` (useful where `DEFAULT` lists a secondary key first,
    /// e.g. `Open` has `v` before `o` in `Timeline`).
    pub fn last_label_in(&self, context: Context, action: Action) -> Option<&str> {
        self.bindings
            .iter()
            .rfind(|b| b.context == context && b.action == action)
            .map(|b| b.label.as_ref())
    }

    /// Primary label for `action`, preferring `Global` then any context.
    pub fn label_for(&self, action: Action) -> Option<&str> {
        self.label_in(Context::Global, action).or_else(|| {
            self.bindings
                .iter()
                .find(|b| b.action == action)
                .map(|b| b.label.as_ref())
        })
    }

    /// All labels for `(context, action)` in declaration order.
    pub fn labels_in(&self, context: Context, action: Action) -> Vec<&str> {
        self.bindings
            .iter()
            .filter(|b| b.context == context && b.action == action)
            .map(|b| b.label.as_ref())
            .collect()
    }

    /// All keys bound to `(context, action)` formatted for config persistence, or `None` when the
    /// current keys equal the active preset's default for `(context, action)`.
    pub fn config_override_for(&self, context: Context, action: Action) -> Option<Vec<String>> {
        let current: Vec<_> = self
            .bindings
            .iter()
            .filter(|b| b.context == context && b.action == action)
            .map(|b| (b.code, b.mods))
            .collect();
        let base: Vec<_> = Self::preset_bindings(self.preset)
            .into_iter()
            .filter(|b| b.context == context && b.action == action)
            .map(|b| (b.code, b.mods))
            .collect();
        if current == base {
            None
        } else {
            Some(
                current
                    .into_iter()
                    .map(|(code, mods)| format_config_key(code, mods))
                    .collect(),
            )
        }
    }

    /// Checks whether binding `(code, mods)` to `target` in `context` would displace another
    /// action, and whether doing so is blocked by the `Quit` safety rail.
    pub fn check_rebind(
        &self,
        context: Context,
        target: Action,
        code: KeyCode,
        mods: KeyModifiers,
    ) -> Result<Option<Action>, String> {
        let displaced = self
            .bindings
            .iter()
            .find(|b| {
                b.context == context && b.code == code && b.mods == mods && b.action != target
            })
            .map(|b| b.action);
        if displaced == Some(Action::Quit)
            && self.quit_count_excluding(context, Action::Quit, Some((code, mods))) == 0
        {
            return Err(format!(
                "{} is the only binding for Quit — bind another key to Quit first",
                format_label(code, mods)
            ));
        }
        Ok(displaced)
    }

    fn quit_count_excluding(
        &self,
        context: Context,
        action: Action,
        only_key: Option<(KeyCode, KeyModifiers)>,
    ) -> usize {
        self.bindings
            .iter()
            .filter(|b| {
                if b.action != Action::Quit {
                    return false;
                }
                if b.context != context {
                    return true;
                }
                match only_key {
                    Some((c, m)) => !(b.code == c && b.mods == m),
                    None => action != Action::Quit,
                }
            })
            .count()
    }

    /// Binds `(context, target)` to the single key `(code, mods)`, removing any displaced binding
    /// in `context`. Returns the displaced `Action` (if any) so persistence can update both.
    pub fn apply_rebind(
        &mut self,
        context: Context,
        target: Action,
        code: KeyCode,
        mods: KeyModifiers,
    ) -> Option<Action> {
        let displaced = self
            .bindings
            .iter()
            .find(|b| {
                b.context == context && b.code == code && b.mods == mods && b.action != target
            })
            .map(|b| b.action);
        let spec = action_spec(target).expect("only bindable actions appear in editor");
        self.replace_action_bindings(context, spec, &[(code, mods)], None);
        displaced
    }

    /// Removes all bindings for `(context, action)`, refusing if that removes the last `Quit`.
    pub fn unbind_action(&mut self, context: Context, action: Action) -> Result<(), String> {
        if action == Action::Quit && self.quit_count_excluding(context, action, None) == 0 {
            return Err("Quit must keep at least one key binding".to_owned());
        }
        let spec = action_spec(action).expect("only bindable actions appear in editor");
        self.replace_action_bindings(context, spec, &[], None);
        Ok(())
    }

    /// Resets `(context, action)` back to `self.preset`'s default, reclaiming displaced keys in
    /// `context` unless that would remove the last `Quit`. Returns every action whose config entry
    /// should be re-evaluated (`target` plus any displaced actions).
    pub fn reset_action(
        &mut self,
        context: Context,
        action: Action,
    ) -> Result<Vec<Action>, String> {
        let base_keys: Vec<_> = Self::preset_bindings(self.preset)
            .into_iter()
            .filter(|b| b.context == context && b.action == action)
            .map(|b| (b.code, b.mods))
            .collect();
        let mut displaced = Vec::new();
        for &(code, mods) in &base_keys {
            if let Some(other) = self.check_rebind(context, action, code, mods)?
                && !displaced.contains(&other)
            {
                displaced.push(other);
            }
        }
        let spec = action_spec(action).expect("only bindable actions appear in editor");
        self.replace_action_bindings(context, spec, &base_keys, None);
        let mut touched = vec![action];
        touched.extend(displaced);
        Ok(touched)
    }

    /// Switches preset while keeping any non-default overrides that still differ from the previous preset.
    pub fn switch_preset(&mut self, next: Preset) -> Vec<String> {
        type PreservedOverride = (Context, ActionSpec, Vec<(KeyCode, KeyModifiers)>);
        let mut preserved: Vec<PreservedOverride> = Vec::new();
        for ctx in Context::ALL {
            for spec in ACTIONS {
                let current: Vec<_> = self
                    .bindings
                    .iter()
                    .filter(|b| b.context == ctx && b.action == spec.action)
                    .map(|b| (b.code, b.mods))
                    .collect();
                let old_base: Vec<_> = Self::preset_bindings(self.preset)
                    .into_iter()
                    .filter(|b| b.context == ctx && b.action == spec.action)
                    .map(|b| (b.code, b.mods))
                    .collect();
                if current != old_base {
                    preserved.push((ctx, *spec, current));
                }
            }
        }
        *self = Self::from_preset(next);
        let mut warnings = Vec::new();
        let remaining_actions: Vec<(Context, Action)> = preserved
            .iter()
            .map(|(ctx, spec, _)| (*ctx, spec.action))
            .collect();
        for (idx, (ctx, spec, keys)) in preserved.iter().enumerate() {
            self.replace_action_bindings(
                *ctx,
                *spec,
                keys,
                Some(ConflictLog {
                    source: "custom",
                    warnings: &mut warnings,
                    deferred: &remaining_actions[idx + 1..],
                }),
            );
        }
        if !self.bindings.iter().any(|b| b.action == Action::Quit) {
            self.enforce_quit_rail(&mut warnings);
        }
        warnings
    }
}

/// Interactive button IDs inside the key-bindings editor modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorButton {
    PresetPrev,
    PresetNext,
    Rebind,
    Unbind,
    Reset,
    ResetAll,
    Close,
    ConfirmReplace,
    CancelPrompt,
}

/// Current stage of the key-bindings editor modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorStage {
    Browse,
    Capture,
    Conflict {
        code: KeyCode,
        mods: KeyModifiers,
        displaced: Action,
    },
}

/// One row shown in the key-bindings editor, ordered by context and `ACTIONS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorRow {
    pub context: Context,
    pub action: Action,
    pub description: &'static str,
}

/// UI state for the Settings -> Key bindings editor modal.
#[derive(Debug, Clone)]
pub struct Editor {
    pub selected: usize,
    pub scroll: usize,
    pub follow_selection: bool,
    pub stage: EditorStage,
    /// `(message, is_error)` feedback shown inside the modal.
    pub feedback: Option<(String, bool)>,
    rows: Vec<EditorRow>,
}

impl Editor {
    pub fn new(keymap: &Keymap) -> Self {
        let mut editor = Self {
            selected: 0,
            scroll: 0,
            follow_selection: true,
            stage: EditorStage::Browse,
            feedback: None,
            rows: Vec::new(),
        };
        editor.rebuild_rows(keymap);
        editor
    }

    pub fn rebuild_rows(&mut self, keymap: &Keymap) {
        let preset_bindings = Keymap::preset_bindings(keymap.preset());
        let mut rows = Vec::new();
        for ctx in Context::ALL {
            let mut included: BTreeSet<&'static str> = BTreeSet::new();
            for spec in ACTIONS {
                let in_default = spec.default_context == ctx;
                let in_preset = preset_bindings
                    .iter()
                    .any(|b| b.context == ctx && b.action == spec.action);
                let in_current = keymap
                    .bindings()
                    .iter()
                    .any(|b| b.context == ctx && b.action == spec.action);
                if (in_default || in_preset || in_current) && included.insert(spec.name) {
                    let description = keymap
                        .bindings()
                        .iter()
                        .find(|b| b.context == ctx && b.action == spec.action)
                        .or_else(|| {
                            preset_bindings
                                .iter()
                                .find(|b| b.context == ctx && b.action == spec.action)
                        })
                        .map_or(spec.description, |b| b.description);
                    rows.push(EditorRow {
                        context: ctx,
                        action: spec.action,
                        description,
                    });
                }
            }
        }
        self.rows = rows;
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    pub fn rows(&self) -> &[EditorRow] {
        &self.rows
    }

    pub fn current_row(&self) -> Option<EditorRow> {
        self.rows.get(self.selected).copied()
    }

    pub fn step(&mut self, delta: isize) {
        if self.rows.is_empty() {
            self.selected = 0;
            return;
        }
        let max = self.rows.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, max) as usize;
        self.follow_selection = true;
        self.feedback = None;
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max_line = self.rows.len() + Context::ALL.len() * 2;
        let next = self.scroll as isize + delta;
        self.scroll = next.clamp(0, max_line as isize) as usize;
        self.follow_selection = false;
    }
}

/// Persists changes to the affected actions (plus optional preset change) in `config.toml`.
pub fn persist_overrides(
    path: Option<&std::path::Path>,
    keymap: &Keymap,
    preset: Option<Preset>,
    clear_all: bool,
    touched: &[(Context, Action)],
) -> std::io::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let mut computed: Vec<(Context, ActionSpec, Option<Vec<String>>)> = Vec::new();
    for &(ctx, action) in touched {
        if let Some(spec) = action_spec(action)
            && !computed
                .iter()
                .any(|(c, s, _)| *c == ctx && s.name == spec.name)
        {
            computed.push((ctx, spec, keymap.config_override_for(ctx, action)));
        }
    }
    let edits: Vec<KeyOverrideEdit<'_>> = computed
        .iter()
        .map(|(ctx, spec, keys)| KeyOverrideEdit {
            context: ctx.config_name(),
            default_context: spec.default_context.config_name(),
            action: spec.name,
            keys: keys.as_deref(),
        })
        .collect();
    crate::config::save_key_changes(path, preset.map(Preset::name), clear_all, &edits)
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn action_table_has_unique_names_and_complete_mapping() {
        let mut names = HashSet::new();
        for spec in ACTIONS {
            assert!(
                names.insert(spec.name),
                "duplicate action name {}",
                spec.name
            );
            let round = action_spec(spec.action).expect("spec exists");
            assert_eq!(round.name, spec.name);
            assert_eq!(
                action_by_name(spec.name).map(|s| s.action),
                Some(spec.action)
            );
        }
        assert_eq!(
            action_by_name("SETTINGS").map(|s| s.action),
            Some(Action::OpenSettings)
        );
        assert_eq!(action_by_name("unknown_action"), None);
    }

    #[test]
    fn every_default_binding_uses_standard_label() {
        for b in DEFAULT {
            assert_eq!(format_label(b.code, b.mods), b.label.as_ref());
        }
        for b in VIM_EXTRA {
            assert_eq!(format_label(b.code, b.mods), b.label.as_ref());
        }
    }

    #[test]
    fn parse_and_format_config_key_round_trip() {
        for binding in DEFAULT.iter().chain(VIM_EXTRA.iter()) {
            let text = format_config_key(binding.code, binding.mods);
            let parsed = parse_key(binding.context, &text)
                .unwrap_or_else(|e| panic!("failed on {text} ({:?}): {e}", binding.context));
            assert_eq!(parsed, (binding.code, binding.mods), "{text}");
        }
        assert_eq!(parse_key(Global, "ctrl--").unwrap(), (Char('-'), CTRL));
        assert_eq!(parse_key(Global, "-").unwrap(), (Char('-'), NONE));
        assert_eq!(parse_key(Global, "Ctrl-Q").unwrap(), (Char('q'), CTRL));
        assert_eq!(parse_key(Timeline, "R").unwrap(), (Char('R'), NONE));
        assert_eq!(parse_key(Timeline, "space").unwrap(), (Char(' '), NONE));
        assert_eq!(parse_key(Global, "F12").unwrap(), (KeyCode::F(12), NONE));
        assert_eq!(parse_key(Global, "pgup").unwrap(), (KeyCode::PageUp, NONE));
        assert_eq!(
            parse_key(Global, "page-down").unwrap(),
            (KeyCode::PageDown, NONE)
        );
        assert_eq!(
            parse_key(Global, "shift-enter").unwrap(),
            (KeyCode::Enter, SHIFT)
        );
        assert_eq!(
            parse_key(Global, "shift-tab").unwrap(),
            (KeyCode::BackTab, NONE)
        );
    }

    #[test]
    fn unrepresentable_and_disallowed_keys_are_rejected() {
        for bad in [
            "",
            "ctrl-",
            "ctrl-ctrl-q",
            "meta-q",
            "f0",
            "f13",
            "ctrl-i",
            "ctrl-m",
            "ctrl-[",
            "ctrl-c",
            "shift-r",
            "ctrl-shift-r",
            "alt-shift-q",
        ] {
            assert!(
                parse_key(Global, bad).is_err(),
                "{bad} should have been rejected"
            );
        }
        // Plain chars cannot be bound in Composer.
        assert!(parse_key(Composer, "e").is_err());
        assert!(parse_key(Composer, "R").is_err());
        assert!(parse_key(Composer, "space").is_err());
        assert!(parse_key(Composer, "shift-enter").is_ok());

        // Interactive Shift+R is fine, but Ctrl+Shift+R is rejected.
        assert_eq!(
            from_key_event(Timeline, key(Char('R'), SHIFT)).unwrap(),
            (Char('R'), NONE)
        );
        assert!(from_key_event(Global, key(Char('R'), CTRL | SHIFT)).is_err());
        assert!(from_key_event(Composer, key(Char('a'), NONE)).is_err());
        assert!(from_key_event(Global, key(Char('c'), CTRL)).is_err());
    }

    #[test]
    fn context_wins_over_global_and_falls_back() {
        let km = Keymap::default();
        assert_eq!(
            km.lookup(Composer, key(KeyCode::Enter, NONE)),
            Some(Action::Send)
        );
        assert_eq!(
            km.lookup(Timeline, key(KeyCode::Enter, NONE)),
            Some(Action::Reply)
        );
        assert_eq!(
            km.lookup(Composer, key(KeyCode::Enter, SHIFT)),
            Some(Action::Newline)
        );
        assert_eq!(
            km.lookup(Composer, key(KeyCode::Tab, NONE)),
            Some(Action::FocusNext)
        );
        assert_eq!(km.lookup(Composer, key(Char('e'), NONE)), None);
        assert_eq!(km.lookup(Composer, key(Char('?'), SHIFT)), None);
    }

    #[test]
    fn composer_plain_chars_always_type_even_with_global_plain_binding() {
        let cfg = Config::parse("[keys.global]\nsettings = \"j\"\n").unwrap();
        let (km, warnings) = Keymap::from_config(&cfg);
        assert!(warnings.is_empty(), "{warnings:?}");
        // Triggering works in Timeline/Sidebar...
        assert_eq!(
            km.lookup(Timeline, key(Char('j'), NONE)),
            Some(Action::OpenSettings)
        );
        // ...but Composer MUST still let the user type 'j'.
        assert_eq!(km.lookup(Composer, key(Char('j'), NONE)), None);
    }

    #[test]
    fn normalization() {
        let km = Keymap::default();
        assert_eq!(km.lookup(Global, key(Char('Q'), CTRL)), Some(Action::Quit));
        assert_eq!(
            km.lookup(Global, key(Char('R'), ALT | SHIFT)),
            Some(Action::FocusRoomFilter)
        );
        assert_eq!(
            km.lookup(Sidebar, key(Char('?'), SHIFT)),
            Some(Action::ToggleHelp)
        );
        assert_eq!(
            km.lookup(Global, key(KeyCode::BackTab, SHIFT)),
            Some(Action::FocusPrev)
        );
        assert_eq!(
            km.lookup(Global, key(KeyCode::Tab, SHIFT)),
            Some(Action::FocusPrev)
        );
    }

    #[test]
    fn no_duplicate_bindings_within_a_context() {
        for base in [
            Keymap::from_preset(Preset::Default),
            Keymap::from_preset(Preset::Vim),
        ] {
            for (i, a) in base.bindings().iter().enumerate() {
                for b in &base.bindings()[i + 1..] {
                    assert!(
                        !(a.context == b.context && a.code == b.code && a.mods == b.mods),
                        "{} bound twice in {:?} ({})",
                        a.label,
                        a.context,
                        base.preset().name()
                    );
                }
            }
        }
    }

    #[test]
    fn room_management_keys() {
        let km = Keymap::default();
        for context in [Global, Sidebar, Timeline, Composer] {
            assert_eq!(
                km.lookup(context, key(Char('n'), CTRL)),
                Some(Action::NewRoom)
            );
            assert_eq!(
                km.lookup(context, key(KeyCode::F(4), NONE)),
                Some(Action::RoomInfo)
            );
        }
        assert_eq!(
            km.lookup(Global, key(Char('N'), CTRL | SHIFT)),
            Some(Action::NewRoom)
        );
        assert_eq!(km.lookup(Composer, key(Char('n'), NONE)), None);
        assert_eq!(km.lookup(Global, key(KeyCode::F(4), CTRL)), None);
    }

    #[test]
    fn config_overrides_conflicts_order_and_quit_rail() {
        let cfg = Config::parse(
            r#"
            [keys]
            help = "f6"

            [keys.global]
            settings = "f5"
            cycle_theme = []
            search = "none"
            quit = []

            [keys.timeline]
            zebra_typo = 123
            edit = "Q"
            reply = ["enter", "R"]
            open = "R"
            "#,
        )
        .unwrap();
        let (km, warnings) = Keymap::from_config(&cfg);
        assert_eq!(km.label_for(Action::ToggleHelp), Some("F6"));
        assert_eq!(
            km.lookup(Global, key(KeyCode::F(5), NONE)),
            Some(Action::OpenSettings)
        );
        assert_eq!(km.lookup(Global, key(KeyCode::F(2), NONE)), None);
        assert_eq!(km.label_for(Action::CycleTheme), None);
        assert_eq!(km.lookup(Global, key(Char('f'), CTRL)), None);
        // Quit rail restored Ctrl-Q after `quit = []` left Quit with zero bindings.
        assert_eq!(km.lookup(Global, key(Char('q'), CTRL)), Some(Action::Quit));
        // Within Timeline: `open = "R"` declared AFTER `reply = ["enter", "R"]` wins `R`,
        // while `reply` keeps `Enter`.
        assert_eq!(
            km.lookup(Timeline, key(KeyCode::Enter, NONE)),
            Some(Action::Reply)
        );
        assert_eq!(
            km.lookup(Timeline, key(Char('R'), NONE)),
            Some(Action::Open)
        );
        assert_eq!(
            km.lookup(Timeline, key(Char('Q'), NONE)),
            Some(Action::Edit)
        );
        assert!(
            warnings.iter().any(|w| w.contains("zebra_typo")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("Quit cannot be unbound")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("overrides 'reply'")),
            "{warnings:?}"
        );
    }

    #[test]
    fn invalid_config_values_do_not_drop_valid_settings_or_keys() {
        let cfg = Config::parse(
            r#"
            theme = "nord"
            preset = "emacs"

            [keys]
            global = 42
            bad_action = "f5"
            settings = ["ctrl-c", "f8"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.theme, "nord");
        let (km, warnings) = Keymap::from_config(&cfg);
        assert_eq!(km.preset(), Preset::Default);
        assert_eq!(
            km.lookup(Global, key(KeyCode::F(8), NONE)),
            Some(Action::OpenSettings)
        );
        assert!(warnings.iter().any(|w| w.contains("emacs")), "{warnings:?}");
        assert!(
            warnings.iter().any(|w| w.contains("bad_action")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("Ctrl-C")),
            "{warnings:?}"
        );
    }

    #[test]
    fn vim_preset_bindings_and_composer_precedence() {
        let cfg = Config::parse("[keys]\npreset = \"vim\"\n").unwrap();
        let (km, warnings) = Keymap::from_config(&cfg);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(km.preset(), Preset::Vim);

        assert_eq!(
            km.lookup(Sidebar, key(Char('j'), NONE)),
            Some(Action::SidebarDown)
        );
        assert_eq!(
            km.lookup(Sidebar, key(Char('k'), NONE)),
            Some(Action::SidebarUp)
        );
        assert_eq!(
            km.lookup(Sidebar, key(Char('g'), NONE)),
            Some(Action::SidebarTop)
        );
        assert_eq!(
            km.lookup(Sidebar, key(Char('G'), NONE)),
            Some(Action::SidebarBottom)
        );
        assert_eq!(
            km.lookup(Sidebar, key(Char('l'), NONE)),
            Some(Action::Focus(Pane::Timeline))
        );
        assert_eq!(
            km.lookup(Sidebar, key(Char('i'), NONE)),
            Some(Action::Focus(Pane::Composer))
        );
        assert_eq!(
            km.lookup(Sidebar, key(Char('/'), NONE)),
            Some(Action::Search)
        );

        assert_eq!(
            km.lookup(Timeline, key(Char('j'), NONE)),
            Some(Action::SelectNewer)
        );
        assert_eq!(
            km.lookup(Timeline, key(Char('k'), NONE)),
            Some(Action::SelectOlder)
        );
        assert_eq!(
            km.lookup(Timeline, key(Char('g'), NONE)),
            Some(Action::JumpToOldest)
        );
        assert_eq!(
            km.lookup(Timeline, key(Char('G'), NONE)),
            Some(Action::JumpToNewest)
        );
        assert_eq!(
            km.lookup(Timeline, key(Char('h'), NONE)),
            Some(Action::Focus(Pane::Sidebar))
        );
        assert_eq!(
            km.lookup(Timeline, key(Char('l'), NONE)),
            Some(Action::Focus(Pane::Composer))
        );
        assert_eq!(
            km.lookup(Timeline, key(Char('i'), NONE)),
            Some(Action::Focus(Pane::Composer))
        );
        assert_eq!(
            km.lookup(Timeline, key(Char('/'), NONE)),
            Some(Action::Search)
        );

        assert_eq!(
            km.lookup(Composer, key(KeyCode::Esc, NONE)),
            Some(Action::Focus(Pane::Timeline))
        );
        assert_eq!(km.lookup(Composer, key(Char('j'), NONE)), None);
        assert_eq!(km.lookup(Composer, key(Char('i'), NONE)), None);
        assert_eq!(km.lookup(Composer, key(Char('/'), NONE)), None);
        assert_eq!(km.lookup(Composer, key(Char('G'), NONE)), None);
    }

    #[test]
    fn quit_override_in_another_context_keeps_quit_without_restoring_ctrl_q() {
        let cfg = Config::parse(
            r#"
            [keys.global]
            quit = []
            [keys.timeline]
            quit = "Q"
            "#,
        )
        .unwrap();
        let (km, warnings) = Keymap::from_config(&cfg);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(km.lookup(Global, key(Char('q'), CTRL)), None);
        assert_eq!(
            km.lookup(Timeline, key(Char('Q'), NONE)),
            Some(Action::Quit)
        );
    }
}
