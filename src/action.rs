//! Everything the user can ask the app to do. Keys, mouse clicks and (later) the command
//! palette all resolve to an `Action`, so they can never drift apart.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pane {
    Sidebar,
    Timeline,
    Composer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    Suspend,
    FocusNext,
    FocusPrev,
    Focus(Pane),
    FocusDir(Dir),
    FocusRoomFilter,
    ToggleHelp,
    CycleTheme,
    /// Esc: close, cancel or step back, whichever applies.
    Back,
    PageUp,
    PageDown,
    JumpToNewest,

    SidebarUp,
    SidebarDown,
    SidebarOpen,

    SelectOlder,
    SelectNewer,
    Reply,
    Edit,
    React,
    Delete,
    Copy,
    Open,

    Send,
    Newline,
    CancelCompose,

    OpenEmoji,
    AcceptInvite,
    DeclineInvite,
    LoginSubmit,
    LoginBrowser,
    RecoverySubmit,
    /// Esc on a popup.
    CloseModal,
    OpenSettings,
    /// Verify this device with another of the user's devices (or offer what fits:
    /// the recovery key, or setting up cross-signing).
    StartVerification,
    /// Open the room above or below the open one in the list, from anywhere.
    RoomPrev,
    RoomNext,
    /// Save the selected message's file or picture to the download folder.
    Save,
    /// A control of the device verification popup.
    Verify(crate::verify::Button),
    /// Show the selected message's thread (or start one) in the messages pane.
    OpenThread,
    /// Leave the thread and return to the room.
    CloseThread,
    /// Start attaching a file: puts `/upload ` into the composer.
    Attach,
}
