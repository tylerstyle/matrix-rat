//! Key, mouse, paste, resize and focus events, in the app's own types.
//!
//! Named and shaped like crossterm's, the terminal library's, so that everything past
//! the edge that reads the input (`App::on_event`, the key map, the line editor, every
//! popup) is the same whatever delivers it: the terminal today, a browser later. Only
//! that edge converts (the `From` impls at the end, for crossterm's events).
//!
//! The terminal's limits on what a key can be (Ctrl-I arrives as Tab, Ctrl-M as Enter,
//! Ctrl-[ as Esc) are not this module's business: `keymap::validate_binding` keeps them.
//!
//! The lint policy: `input::KeyCode` and `input::MouseEventKind` stand in for the
//! terminal's enums, so a match that ignores the keys or mouse actions it has no use for
//! may take a wildcard with `#[expect(clippy::wildcard_enum_match_arm, reason = "input
//! events stand in for the terminal's (crate::input)")]`, as it did for crossterm's.
//! The others (`InputEvent`, `KeyEventKind`, `MouseButton`) are matched in full.

use std::fmt;
use std::ops::{BitAnd, BitOr, BitOrAssign, Sub, SubAssign};

/// One input event, as `crossterm::event::Event`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum InputEvent {
    FocusGained,
    FocusLost,
    Key(KeyEvent),
    Mouse(MouseEvent),
    /// Text pasted at once (bracketed paste).
    Paste(String),
    /// The new size: columns, rows.
    Resize(u16, u16),
}

/// Never prints what was pasted: it may be a password or a recovery key.
impl fmt::Debug for InputEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InputEvent::FocusGained => f.write_str("FocusGained"),
            InputEvent::FocusLost => f.write_str("FocusLost"),
            InputEvent::Key(key) => f.debug_tuple("Key").field(key).finish(),
            InputEvent::Mouse(mouse) => f.debug_tuple("Mouse").field(mouse).finish(),
            InputEvent::Paste(text) => write!(f, "Paste({} bytes)", text.len()),
            InputEvent::Resize(columns, rows) => {
                f.debug_tuple("Resize").field(columns).field(rows).finish()
            }
        }
    }
}

/// A key, as `crossterm::event::KeyEvent` (without its `state`, which nothing reads).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
    pub kind: KeyEventKind,
}

impl KeyEvent {
    /// A key press, as crossterm's `KeyEvent::new`.
    pub const fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self {
            code,
            modifiers,
            kind: KeyEventKind::Press,
        }
    }
}

/// Whether a key went down, repeats or came up, as `crossterm::event::KeyEventKind`.
/// Terminals without the kitty keyboard protocol report presses only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyEventKind {
    Press,
    Repeat,
    Release,
}

/// Which key, as `crossterm::event::KeyCode`, every variant of it: any of them can
/// arrive from the terminal, and one the app has no use for still counts as a key
/// (the key-bindings editor answers it with "unsupported key").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    Backspace,
    Enter,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    /// Shift-Tab.
    BackTab,
    Delete,
    Insert,
    /// A function key: `F(1)` is F1.
    F(u8),
    Char(char),
    /// What a terminal sends for Ctrl-Space (and Ctrl-@).
    Null,
    Esc,
    CapsLock,
    ScrollLock,
    NumLock,
    PrintScreen,
    Pause,
    Menu,
    /// The keypad's 5 with Num Lock off.
    KeypadBegin,
    Media(MediaKeyCode),
    /// A modifier key on its own (only with the kitty protocol's "all keys" flag).
    Modifier(ModifierKeyCode),
}

/// A media key, as `crossterm::event::MediaKeyCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKeyCode {
    Play,
    Pause,
    PlayPause,
    Reverse,
    Stop,
    FastForward,
    Rewind,
    TrackNext,
    TrackPrevious,
    Record,
    LowerVolume,
    RaiseVolume,
    MuteVolume,
}

/// A modifier key, as `crossterm::event::ModifierKeyCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModifierKeyCode {
    LeftShift,
    LeftControl,
    LeftAlt,
    LeftSuper,
    LeftHyper,
    LeftMeta,
    RightShift,
    RightControl,
    RightAlt,
    RightSuper,
    RightHyper,
    RightMeta,
    IsoLevel3Shift,
    IsoLevel5Shift,
}

/// The modifiers held with a key or a click, as `crossterm::event::KeyModifiers`
/// (a set of flags with the same names and bits).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct KeyModifiers(u8);

impl KeyModifiers {
    pub const NONE: Self = Self(0);
    pub const SHIFT: Self = Self(1);
    pub const CONTROL: Self = Self(1 << 1);
    pub const ALT: Self = Self(1 << 2);
    pub const SUPER: Self = Self(1 << 3);
    pub const HYPER: Self = Self(1 << 4);
    pub const META: Self = Self(1 << 5);

    const NAMES: [(Self, &'static str); 6] = [
        (Self::SHIFT, "SHIFT"),
        (Self::CONTROL, "CONTROL"),
        (Self::ALT, "ALT"),
        (Self::SUPER, "SUPER"),
        (Self::HYPER, "HYPER"),
        (Self::META, "META"),
    ];

    /// Whether every flag of `other` is held.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether any flag of `other` is held.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Whether no modifier is held.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// As the flags' own: `KeyModifiers(SHIFT | CONTROL)`, `KeyModifiers(0x0)` for none.
impl fmt::Debug for KeyModifiers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return write!(f, "KeyModifiers({:#x})", self.0);
        }
        f.write_str("KeyModifiers(")?;
        let mut first = true;
        for (flag, name) in Self::NAMES {
            if self.contains(flag) {
                if !first {
                    f.write_str(" | ")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        f.write_str(")")
    }
}

impl BitOr for KeyModifiers {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl BitOrAssign for KeyModifiers {
    fn bitor_assign(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

impl BitAnd for KeyModifiers {
    type Output = Self;

    fn bitand(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

/// The flags of `self` without those of `other`.
impl Sub for KeyModifiers {
    type Output = Self;

    fn sub(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

impl SubAssign for KeyModifiers {
    fn sub_assign(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

/// A mouse action, as `crossterm::event::MouseEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MouseEvent {
    pub kind: MouseEventKind,
    /// Zero-based column of the cell.
    pub column: u16,
    /// Zero-based row of the cell.
    pub row: u16,
    pub modifiers: KeyModifiers,
}

/// What the mouse did, as `crossterm::event::MouseEventKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseEventKind {
    Down(MouseButton),
    Up(MouseButton),
    Drag(MouseButton),
    /// Moved without a button held.
    Moved,
    ScrollDown,
    ScrollUp,
    ScrollLeft,
    ScrollRight,
}

/// A mouse button, as `crossterm::event::MouseButton`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

// ----- from the terminal ---------------------------------------------------------------

/// The terminal's events, read with crossterm, into ours: one to one, nothing dropped
/// or changed here (`App::on_event` drops key releases, as it always did).
#[cfg(not(target_family = "wasm"))]
mod from_crossterm {
    use crossterm::event as ct;

    use super::{
        InputEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MediaKeyCode, ModifierKeyCode,
        MouseButton, MouseEvent, MouseEventKind,
    };

    impl From<ct::Event> for InputEvent {
        fn from(event: ct::Event) -> Self {
            match event {
                ct::Event::FocusGained => InputEvent::FocusGained,
                ct::Event::FocusLost => InputEvent::FocusLost,
                ct::Event::Key(key) => InputEvent::Key(key.into()),
                ct::Event::Mouse(mouse) => InputEvent::Mouse(mouse.into()),
                ct::Event::Paste(text) => InputEvent::Paste(text),
                ct::Event::Resize(columns, rows) => InputEvent::Resize(columns, rows),
            }
        }
    }

    /// Leaves out `state` (keypad, Caps Lock, Num Lock), which nothing reads.
    impl From<ct::KeyEvent> for KeyEvent {
        fn from(key: ct::KeyEvent) -> Self {
            KeyEvent {
                code: key.code.into(),
                modifiers: key.modifiers.into(),
                kind: key.kind.into(),
            }
        }
    }

    impl From<ct::KeyEventKind> for KeyEventKind {
        fn from(kind: ct::KeyEventKind) -> Self {
            match kind {
                ct::KeyEventKind::Press => KeyEventKind::Press,
                ct::KeyEventKind::Repeat => KeyEventKind::Repeat,
                ct::KeyEventKind::Release => KeyEventKind::Release,
            }
        }
    }

    impl From<ct::KeyCode> for KeyCode {
        fn from(code: ct::KeyCode) -> Self {
            match code {
                ct::KeyCode::Backspace => KeyCode::Backspace,
                ct::KeyCode::Enter => KeyCode::Enter,
                ct::KeyCode::Left => KeyCode::Left,
                ct::KeyCode::Right => KeyCode::Right,
                ct::KeyCode::Up => KeyCode::Up,
                ct::KeyCode::Down => KeyCode::Down,
                ct::KeyCode::Home => KeyCode::Home,
                ct::KeyCode::End => KeyCode::End,
                ct::KeyCode::PageUp => KeyCode::PageUp,
                ct::KeyCode::PageDown => KeyCode::PageDown,
                ct::KeyCode::Tab => KeyCode::Tab,
                ct::KeyCode::BackTab => KeyCode::BackTab,
                ct::KeyCode::Delete => KeyCode::Delete,
                ct::KeyCode::Insert => KeyCode::Insert,
                ct::KeyCode::F(n) => KeyCode::F(n),
                ct::KeyCode::Char(c) => KeyCode::Char(c),
                ct::KeyCode::Null => KeyCode::Null,
                ct::KeyCode::Esc => KeyCode::Esc,
                ct::KeyCode::CapsLock => KeyCode::CapsLock,
                ct::KeyCode::ScrollLock => KeyCode::ScrollLock,
                ct::KeyCode::NumLock => KeyCode::NumLock,
                ct::KeyCode::PrintScreen => KeyCode::PrintScreen,
                ct::KeyCode::Pause => KeyCode::Pause,
                ct::KeyCode::Menu => KeyCode::Menu,
                ct::KeyCode::KeypadBegin => KeyCode::KeypadBegin,
                ct::KeyCode::Media(media) => KeyCode::Media(media.into()),
                ct::KeyCode::Modifier(modifier) => KeyCode::Modifier(modifier.into()),
            }
        }
    }

    impl From<ct::MediaKeyCode> for MediaKeyCode {
        fn from(media: ct::MediaKeyCode) -> Self {
            match media {
                ct::MediaKeyCode::Play => MediaKeyCode::Play,
                ct::MediaKeyCode::Pause => MediaKeyCode::Pause,
                ct::MediaKeyCode::PlayPause => MediaKeyCode::PlayPause,
                ct::MediaKeyCode::Reverse => MediaKeyCode::Reverse,
                ct::MediaKeyCode::Stop => MediaKeyCode::Stop,
                ct::MediaKeyCode::FastForward => MediaKeyCode::FastForward,
                ct::MediaKeyCode::Rewind => MediaKeyCode::Rewind,
                ct::MediaKeyCode::TrackNext => MediaKeyCode::TrackNext,
                ct::MediaKeyCode::TrackPrevious => MediaKeyCode::TrackPrevious,
                ct::MediaKeyCode::Record => MediaKeyCode::Record,
                ct::MediaKeyCode::LowerVolume => MediaKeyCode::LowerVolume,
                ct::MediaKeyCode::RaiseVolume => MediaKeyCode::RaiseVolume,
                ct::MediaKeyCode::MuteVolume => MediaKeyCode::MuteVolume,
            }
        }
    }

    impl From<ct::ModifierKeyCode> for ModifierKeyCode {
        fn from(modifier: ct::ModifierKeyCode) -> Self {
            match modifier {
                ct::ModifierKeyCode::LeftShift => ModifierKeyCode::LeftShift,
                ct::ModifierKeyCode::LeftControl => ModifierKeyCode::LeftControl,
                ct::ModifierKeyCode::LeftAlt => ModifierKeyCode::LeftAlt,
                ct::ModifierKeyCode::LeftSuper => ModifierKeyCode::LeftSuper,
                ct::ModifierKeyCode::LeftHyper => ModifierKeyCode::LeftHyper,
                ct::ModifierKeyCode::LeftMeta => ModifierKeyCode::LeftMeta,
                ct::ModifierKeyCode::RightShift => ModifierKeyCode::RightShift,
                ct::ModifierKeyCode::RightControl => ModifierKeyCode::RightControl,
                ct::ModifierKeyCode::RightAlt => ModifierKeyCode::RightAlt,
                ct::ModifierKeyCode::RightSuper => ModifierKeyCode::RightSuper,
                ct::ModifierKeyCode::RightHyper => ModifierKeyCode::RightHyper,
                ct::ModifierKeyCode::RightMeta => ModifierKeyCode::RightMeta,
                ct::ModifierKeyCode::IsoLevel3Shift => ModifierKeyCode::IsoLevel3Shift,
                ct::ModifierKeyCode::IsoLevel5Shift => ModifierKeyCode::IsoLevel5Shift,
            }
        }
    }

    /// Flag by flag, by name (not by bits).
    impl From<ct::KeyModifiers> for KeyModifiers {
        fn from(theirs: ct::KeyModifiers) -> Self {
            [
                (ct::KeyModifiers::SHIFT, KeyModifiers::SHIFT),
                (ct::KeyModifiers::CONTROL, KeyModifiers::CONTROL),
                (ct::KeyModifiers::ALT, KeyModifiers::ALT),
                (ct::KeyModifiers::SUPER, KeyModifiers::SUPER),
                (ct::KeyModifiers::HYPER, KeyModifiers::HYPER),
                (ct::KeyModifiers::META, KeyModifiers::META),
            ]
            .into_iter()
            .filter(|(flag, _)| theirs.contains(*flag))
            .fold(KeyModifiers::NONE, |ours, (_, flag)| ours | flag)
        }
    }

    impl From<ct::MouseEvent> for MouseEvent {
        fn from(mouse: ct::MouseEvent) -> Self {
            MouseEvent {
                kind: mouse.kind.into(),
                column: mouse.column,
                row: mouse.row,
                modifiers: mouse.modifiers.into(),
            }
        }
    }

    impl From<ct::MouseEventKind> for MouseEventKind {
        fn from(kind: ct::MouseEventKind) -> Self {
            match kind {
                ct::MouseEventKind::Down(button) => MouseEventKind::Down(button.into()),
                ct::MouseEventKind::Up(button) => MouseEventKind::Up(button.into()),
                ct::MouseEventKind::Drag(button) => MouseEventKind::Drag(button.into()),
                ct::MouseEventKind::Moved => MouseEventKind::Moved,
                ct::MouseEventKind::ScrollDown => MouseEventKind::ScrollDown,
                ct::MouseEventKind::ScrollUp => MouseEventKind::ScrollUp,
                ct::MouseEventKind::ScrollLeft => MouseEventKind::ScrollLeft,
                ct::MouseEventKind::ScrollRight => MouseEventKind::ScrollRight,
            }
        }
    }

    impl From<ct::MouseButton> for MouseButton {
        fn from(button: ct::MouseButton) -> Self {
            match button {
                ct::MouseButton::Left => MouseButton::Left,
                ct::MouseButton::Right => MouseButton::Right,
                ct::MouseButton::Middle => MouseButton::Middle,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event as ct;

    use super::*;

    #[test]
    fn modifier_flags_combine_and_test_like_crossterms() {
        let ctrl_alt = KeyModifiers::CONTROL | KeyModifiers::ALT;
        assert!(ctrl_alt.contains(KeyModifiers::CONTROL));
        assert!(ctrl_alt.contains(KeyModifiers::CONTROL | KeyModifiers::ALT));
        assert!(!ctrl_alt.contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT));
        assert!(ctrl_alt.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT));
        assert!(!ctrl_alt.intersects(KeyModifiers::SHIFT | KeyModifiers::SUPER));
        assert!(!ctrl_alt.is_empty());
        assert!(KeyModifiers::NONE.is_empty());
        assert_eq!(KeyModifiers::default(), KeyModifiers::NONE);
        // Every set contains the empty one; the empty one meets none.
        assert!(KeyModifiers::NONE.contains(KeyModifiers::NONE));
        assert!(!ctrl_alt.intersects(KeyModifiers::NONE));

        assert_eq!(ctrl_alt & KeyModifiers::ALT, KeyModifiers::ALT);
        assert_eq!(ctrl_alt & KeyModifiers::SHIFT, KeyModifiers::NONE);
        assert_eq!(ctrl_alt - KeyModifiers::ALT, KeyModifiers::CONTROL);
        assert_eq!(ctrl_alt - KeyModifiers::SHIFT, ctrl_alt);
        let mut mods = KeyModifiers::SHIFT;
        mods |= KeyModifiers::META;
        assert_eq!(mods, KeyModifiers::SHIFT | KeyModifiers::META);
        mods -= KeyModifiers::SHIFT;
        assert_eq!(mods, KeyModifiers::META);
    }

    #[test]
    fn modifiers_have_crossterms_bits_and_debug_text() {
        for (ours, theirs) in [
            (KeyModifiers::NONE, ct::KeyModifiers::NONE),
            (KeyModifiers::SHIFT, ct::KeyModifiers::SHIFT),
            (KeyModifiers::CONTROL, ct::KeyModifiers::CONTROL),
            (KeyModifiers::ALT, ct::KeyModifiers::ALT),
            (KeyModifiers::SUPER, ct::KeyModifiers::SUPER),
            (KeyModifiers::HYPER, ct::KeyModifiers::HYPER),
            (KeyModifiers::META, ct::KeyModifiers::META),
            (
                KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::META,
                ct::KeyModifiers::SHIFT | ct::KeyModifiers::CONTROL | ct::KeyModifiers::META,
            ),
        ] {
            assert_eq!(ours.0, theirs.bits());
            assert_eq!(format!("{ours:?}"), format!("{theirs:?}"));
        }
    }

    #[test]
    fn every_modifier_set_converts_flag_by_flag() {
        for bits in 0..=0b11_1111 {
            let theirs = ct::KeyModifiers::from_bits_truncate(bits);
            let ours = KeyModifiers::from(theirs);
            assert_eq!(ours.0, bits);
            for (flag, ours_flag) in [
                (ct::KeyModifiers::SHIFT, KeyModifiers::SHIFT),
                (ct::KeyModifiers::CONTROL, KeyModifiers::CONTROL),
                (ct::KeyModifiers::ALT, KeyModifiers::ALT),
                (ct::KeyModifiers::SUPER, KeyModifiers::SUPER),
                (ct::KeyModifiers::HYPER, KeyModifiers::HYPER),
                (ct::KeyModifiers::META, KeyModifiers::META),
            ] {
                assert_eq!(ours.contains(ours_flag), theirs.contains(flag));
            }
        }
    }

    #[test]
    fn every_key_code_converts_to_its_namesake() {
        let media = [
            (ct::MediaKeyCode::Play, MediaKeyCode::Play),
            (ct::MediaKeyCode::Pause, MediaKeyCode::Pause),
            (ct::MediaKeyCode::PlayPause, MediaKeyCode::PlayPause),
            (ct::MediaKeyCode::Reverse, MediaKeyCode::Reverse),
            (ct::MediaKeyCode::Stop, MediaKeyCode::Stop),
            (ct::MediaKeyCode::FastForward, MediaKeyCode::FastForward),
            (ct::MediaKeyCode::Rewind, MediaKeyCode::Rewind),
            (ct::MediaKeyCode::TrackNext, MediaKeyCode::TrackNext),
            (ct::MediaKeyCode::TrackPrevious, MediaKeyCode::TrackPrevious),
            (ct::MediaKeyCode::Record, MediaKeyCode::Record),
            (ct::MediaKeyCode::LowerVolume, MediaKeyCode::LowerVolume),
            (ct::MediaKeyCode::RaiseVolume, MediaKeyCode::RaiseVolume),
            (ct::MediaKeyCode::MuteVolume, MediaKeyCode::MuteVolume),
        ];
        let modifiers = [
            (ct::ModifierKeyCode::LeftShift, ModifierKeyCode::LeftShift),
            (
                ct::ModifierKeyCode::LeftControl,
                ModifierKeyCode::LeftControl,
            ),
            (ct::ModifierKeyCode::LeftAlt, ModifierKeyCode::LeftAlt),
            (ct::ModifierKeyCode::LeftSuper, ModifierKeyCode::LeftSuper),
            (ct::ModifierKeyCode::LeftHyper, ModifierKeyCode::LeftHyper),
            (ct::ModifierKeyCode::LeftMeta, ModifierKeyCode::LeftMeta),
            (ct::ModifierKeyCode::RightShift, ModifierKeyCode::RightShift),
            (
                ct::ModifierKeyCode::RightControl,
                ModifierKeyCode::RightControl,
            ),
            (ct::ModifierKeyCode::RightAlt, ModifierKeyCode::RightAlt),
            (ct::ModifierKeyCode::RightSuper, ModifierKeyCode::RightSuper),
            (ct::ModifierKeyCode::RightHyper, ModifierKeyCode::RightHyper),
            (ct::ModifierKeyCode::RightMeta, ModifierKeyCode::RightMeta),
            (
                ct::ModifierKeyCode::IsoLevel3Shift,
                ModifierKeyCode::IsoLevel3Shift,
            ),
            (
                ct::ModifierKeyCode::IsoLevel5Shift,
                ModifierKeyCode::IsoLevel5Shift,
            ),
        ];
        let mut codes = vec![
            (ct::KeyCode::Backspace, KeyCode::Backspace),
            (ct::KeyCode::Enter, KeyCode::Enter),
            (ct::KeyCode::Left, KeyCode::Left),
            (ct::KeyCode::Right, KeyCode::Right),
            (ct::KeyCode::Up, KeyCode::Up),
            (ct::KeyCode::Down, KeyCode::Down),
            (ct::KeyCode::Home, KeyCode::Home),
            (ct::KeyCode::End, KeyCode::End),
            (ct::KeyCode::PageUp, KeyCode::PageUp),
            (ct::KeyCode::PageDown, KeyCode::PageDown),
            (ct::KeyCode::Tab, KeyCode::Tab),
            (ct::KeyCode::BackTab, KeyCode::BackTab),
            (ct::KeyCode::Delete, KeyCode::Delete),
            (ct::KeyCode::Insert, KeyCode::Insert),
            (ct::KeyCode::F(1), KeyCode::F(1)),
            (ct::KeyCode::F(12), KeyCode::F(12)),
            (ct::KeyCode::F(24), KeyCode::F(24)),
            (ct::KeyCode::Char('a'), KeyCode::Char('a')),
            (ct::KeyCode::Char('A'), KeyCode::Char('A')),
            (ct::KeyCode::Char(' '), KeyCode::Char(' ')),
            (ct::KeyCode::Char('ß'), KeyCode::Char('ß')),
            (ct::KeyCode::Char('\u{1}'), KeyCode::Char('\u{1}')),
            (ct::KeyCode::Null, KeyCode::Null),
            (ct::KeyCode::Esc, KeyCode::Esc),
            (ct::KeyCode::CapsLock, KeyCode::CapsLock),
            (ct::KeyCode::ScrollLock, KeyCode::ScrollLock),
            (ct::KeyCode::NumLock, KeyCode::NumLock),
            (ct::KeyCode::PrintScreen, KeyCode::PrintScreen),
            (ct::KeyCode::Pause, KeyCode::Pause),
            (ct::KeyCode::Menu, KeyCode::Menu),
            (ct::KeyCode::KeypadBegin, KeyCode::KeypadBegin),
        ];
        codes.extend(
            media
                .iter()
                .map(|&(theirs, ours)| (ct::KeyCode::Media(theirs), KeyCode::Media(ours))),
        );
        codes.extend(
            modifiers
                .iter()
                .map(|&(theirs, ours)| (ct::KeyCode::Modifier(theirs), KeyCode::Modifier(ours))),
        );
        for (theirs, ours) in codes {
            assert_eq!(KeyCode::from(theirs), ours);
            // The same name, so the key map's fallback label (`{:?}`) reads the same.
            assert_eq!(format!("{ours:?}"), format!("{theirs:?}"));
        }
    }

    #[test]
    fn key_events_keep_code_modifiers_and_kind() {
        for (kind, ours) in [
            (ct::KeyEventKind::Press, KeyEventKind::Press),
            (ct::KeyEventKind::Repeat, KeyEventKind::Repeat),
            (ct::KeyEventKind::Release, KeyEventKind::Release),
        ] {
            let theirs = ct::KeyEvent::new_with_kind_and_state(
                ct::KeyCode::Char('x'),
                ct::KeyModifiers::CONTROL | ct::KeyModifiers::SHIFT,
                kind,
                ct::KeyEventState::CAPS_LOCK,
            );
            assert_eq!(
                KeyEvent::from(theirs),
                KeyEvent {
                    code: KeyCode::Char('x'),
                    modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                    kind: ours,
                }
            );
        }
        // `new` is a press, as crossterm's.
        assert_eq!(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            KeyEvent::from(ct::KeyEvent::new(ct::KeyCode::Enter, ct::KeyModifiers::ALT))
        );
        assert_eq!(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE).kind,
            KeyEventKind::Press
        );
    }

    #[test]
    fn mouse_events_keep_kind_cell_and_modifiers() {
        let buttons = [
            (ct::MouseButton::Left, MouseButton::Left),
            (ct::MouseButton::Right, MouseButton::Right),
            (ct::MouseButton::Middle, MouseButton::Middle),
        ];
        let mut kinds = vec![
            (ct::MouseEventKind::Moved, MouseEventKind::Moved),
            (ct::MouseEventKind::ScrollDown, MouseEventKind::ScrollDown),
            (ct::MouseEventKind::ScrollUp, MouseEventKind::ScrollUp),
            (ct::MouseEventKind::ScrollLeft, MouseEventKind::ScrollLeft),
            (ct::MouseEventKind::ScrollRight, MouseEventKind::ScrollRight),
        ];
        for (theirs, ours) in buttons {
            assert_eq!(MouseButton::from(theirs), ours);
            kinds.push((ct::MouseEventKind::Down(theirs), MouseEventKind::Down(ours)));
            kinds.push((ct::MouseEventKind::Up(theirs), MouseEventKind::Up(ours)));
            kinds.push((ct::MouseEventKind::Drag(theirs), MouseEventKind::Drag(ours)));
        }
        for (theirs, ours) in kinds {
            let mouse = ct::MouseEvent {
                kind: theirs,
                column: 17,
                row: 4,
                modifiers: ct::KeyModifiers::ALT,
            };
            assert_eq!(
                MouseEvent::from(mouse),
                MouseEvent {
                    kind: ours,
                    column: 17,
                    row: 4,
                    modifiers: KeyModifiers::ALT,
                }
            );
        }
    }

    #[test]
    fn every_event_converts_to_its_namesake() {
        let key = ct::KeyEvent::new_with_kind(
            ct::KeyCode::Up,
            ct::KeyModifiers::SHIFT,
            ct::KeyEventKind::Release,
        );
        let mouse = ct::MouseEvent {
            kind: ct::MouseEventKind::Down(ct::MouseButton::Right),
            column: 0,
            row: u16::MAX,
            modifiers: ct::KeyModifiers::NONE,
        };
        for (theirs, ours) in [
            (ct::Event::FocusGained, InputEvent::FocusGained),
            (ct::Event::FocusLost, InputEvent::FocusLost),
            (
                ct::Event::Key(key),
                InputEvent::Key(KeyEvent {
                    code: KeyCode::Up,
                    modifiers: KeyModifiers::SHIFT,
                    // Releases come through; `App::on_event` is what ignores them.
                    kind: KeyEventKind::Release,
                }),
            ),
            (
                ct::Event::Mouse(mouse),
                InputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Right),
                    column: 0,
                    row: u16::MAX,
                    modifiers: KeyModifiers::NONE,
                }),
            ),
            (
                ct::Event::Paste("two\nlines ".to_owned()),
                InputEvent::Paste("two\nlines ".to_owned()),
            ),
            (ct::Event::Resize(120, 40), InputEvent::Resize(120, 40)),
        ] {
            assert_eq!(InputEvent::from(theirs), ours);
        }
    }

    #[test]
    fn debug_never_shows_what_was_pasted() {
        let paste = InputEvent::Paste("hunter2 recovery words".to_owned());
        assert_eq!(format!("{paste:?}"), "Paste(22 bytes)");
        assert_eq!(
            format!("{:?}", InputEvent::Resize(80, 24)),
            "Resize(80, 24)"
        );
        assert_eq!(format!("{:?}", InputEvent::FocusLost), "FocusLost");
    }
}
