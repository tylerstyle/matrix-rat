//! Default key bindings, scoped by where the focus is.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::{Action, Dir, Pane};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    Global,
    Sidebar,
    Timeline,
    Composer,
}

impl Context {
    pub fn title(self) -> &'static str {
        match self {
            Context::Global => "Everywhere",
            Context::Sidebar => "Room list",
            Context::Timeline => "Messages",
            Context::Composer => "Composer",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Binding {
    pub context: Context,
    pub code: KeyCode,
    pub mods: KeyModifiers,
    pub action: Action,
    pub label: &'static str,
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
        label,
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
        KeyCode::F(1),
        NONE,
        Action::ToggleHelp,
        "F1",
        "This help",
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

/// The action bound to `key` in `context`, falling back to global bindings.
pub fn lookup(context: Context, key: KeyEvent) -> Option<Action> {
    let (code, mods) = normalize(key);
    let find = |ctx: Context| {
        DEFAULT
            .iter()
            .find(|b| b.context == ctx && b.code == code && b.mods == mods)
            .map(|b| b.action)
    };
    find(context).or_else(|| find(Global))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn context_wins_over_global_and_falls_back() {
        assert_eq!(
            lookup(Composer, key(KeyCode::Enter, NONE)),
            Some(Action::Send)
        );
        assert_eq!(
            lookup(Timeline, key(KeyCode::Enter, NONE)),
            Some(Action::Reply)
        );
        assert_eq!(
            lookup(Composer, key(KeyCode::Enter, SHIFT)),
            Some(Action::Newline)
        );
        assert_eq!(
            lookup(Composer, key(KeyCode::Tab, NONE)),
            Some(Action::FocusNext)
        );
        assert_eq!(lookup(Composer, key(Char('e'), NONE)), None);
        assert_eq!(lookup(Composer, key(Char('?'), SHIFT)), None);
    }

    #[test]
    fn normalization() {
        assert_eq!(lookup(Global, key(Char('Q'), CTRL)), Some(Action::Quit));
        assert_eq!(
            lookup(Global, key(Char('R'), ALT | SHIFT)),
            Some(Action::FocusRoomFilter)
        );
        assert_eq!(
            lookup(Sidebar, key(Char('?'), SHIFT)),
            Some(Action::ToggleHelp)
        );
        assert_eq!(
            lookup(Global, key(KeyCode::BackTab, SHIFT)),
            Some(Action::FocusPrev)
        );
        assert_eq!(
            lookup(Global, key(KeyCode::Tab, SHIFT)),
            Some(Action::FocusPrev)
        );
    }

    #[test]
    fn no_duplicate_bindings_within_a_context() {
        for (i, a) in DEFAULT.iter().enumerate() {
            for b in &DEFAULT[i + 1..] {
                assert!(
                    !(a.context == b.context && a.code == b.code && a.mods == b.mods),
                    "{} bound twice in {:?}",
                    a.label,
                    a.context
                );
            }
        }
    }
}
