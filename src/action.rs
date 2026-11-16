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
    /// Hide the room list, or show it again: one pane fills the window, as it does
    /// by itself in a narrow one.
    ToggleSidebar,
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
    /// Copy the content of a fenced code block of the selected message (asks which
    /// when there are several).
    CopyCode,
    Open,
    /// Save the selected picture or file and hand it to the desktop's program for it
    /// (for a picture: the system's viewer instead of the one in here).
    OpenExternal,
    /// Jump to the message the selected one replies to.
    JumpToReply,

    Send,
    Newline,
    CancelCompose,
    /// In an empty composer: edit the user's own latest message. With text in it (or
    /// no such message) it does what `SelectOlder` does, so both can share a key.
    EditLast,
    /// Write the draft in `$VISUAL` / `$EDITOR`, and take the result back into the
    /// composer (it is not sent).
    ComposeInEditor,

    OpenEmoji,
    AcceptInvite,
    DeclineInvite,
    WithdrawKnock,
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
    /// Find text in the open room.
    Search,
    /// Search messages across all joined rooms (local index).
    SearchEverywhere,
    /// Narrow the room list to the next or previous space ("all rooms" in between).
    SpaceNext,
    SpacePrev,
    /// Show the info of the space the room list is narrowed to, with its rooms.
    SpaceInfo,
    /// Toggle the "unread only" quick filter in the room list.
    FilterUnread,
    /// Toggle the "people / direct chats only" quick filter in the room list.
    FilterPeople,
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
    /// Attach the picture the desktop's clipboard holds (a terminal pastes only text).
    PastePicture,
    /// Open the command palette.
    OpenPalette,
    /// Open a room by its index in `App::rooms`.
    OpenRoom(usize),
    /// Select a theme by its index in `App::theme_names`.
    SelectTheme(usize),
    /// Trigger a setting toggle or action from outside the settings popup.
    Setting(crate::modal::Setting),
    /// Open the chooser: join a room, start a direct chat, or create a room.
    NewRoom,
    /// Show the details and members of the open room (in the room list: the one
    /// under the cursor).
    RoomInfo,
    /// Ask whether to leave that room (shown in its room info).
    LeaveRoom,
    /// Invite someone to that room.
    InviteUser,
    /// A control of the room popups.
    Manage(crate::manage::Click),
    /// Open the menu of what has the focus: the selected message, the room under the
    /// cursor, or the composer.
    ContextMenu,
    /// Jump to the oldest loaded message in the timeline.
    JumpToOldest,
    /// Move the room-list cursor to the first visible room.
    SidebarTop,
    /// Move the room-list cursor to the last visible room.
    SidebarBottom,
    /// Go to the "new" line of the open room: the first message that was unread
    /// when the room was opened.
    JumpToUnread,
    /// Mark the open room as read if it shows as unread, else as unread.
    ToggleUnread,
    /// Mark every room as read.
    MarkAllRead,
    /// Pin or unpin the selected message.
    PinMessage,
    /// Open the room's pinned messages popup.
    OpenPinned,
    /// List the threads of the room (in the room list: the one under the cursor).
    OpenThreads,
    /// Forward the selected message to another room.
    ForwardMessage,
    /// A control of the sessions popup or the key backup popup. `Export` and
    /// `Import` also open the key backup popup at that form.
    Devices(crate::devices::Click),
    /// Look once more for the key of the selected message, which could not be
    /// decrypted.
    RetryDecryption,
    /// Open profile details for the selected sender or target user.
    UserInfo,
    /// Show the selected message's details: who sent it and when exactly, its event
    /// ID, encryption, delivery, who read it, and its source.
    MessageDetails,
    /// Open the poll creation form for the active room or thread.
    CreatePoll,
    /// Open the sticker picker for the active room or thread.
    OpenStickers,
}

impl Action {
    /// Whether the action is about the open chat: its messages, its composer, or the
    /// room as the one that is open. With no chat open (see `App::close_chat`) there
    /// is nothing for it to act on, so it is not performed and the command palette
    /// does not offer it. Actions about the room under the room list's cursor, and
    /// everything that opens a room, are not among them.
    ///
    /// Every action is named, so that a new one has to be placed.
    pub fn needs_open_chat(self) -> bool {
        match self {
            // Going to the messages or the composer, and moving in the messages.
            Action::FocusNext
            | Action::FocusPrev
            | Action::Focus(Pane::Timeline | Pane::Composer)
            | Action::FocusDir(Dir::Right | Dir::Up | Dir::Down)
            | Action::PageUp
            | Action::PageDown
            | Action::JumpToNewest
            | Action::JumpToOldest
            | Action::JumpToUnread
            | Action::SelectOlder
            | Action::SelectNewer
            // The selected message.
            | Action::Reply
            | Action::Edit
            | Action::React
            | Action::Delete
            | Action::Copy
            | Action::CopyCode
            | Action::Open
            | Action::OpenExternal
            | Action::JumpToReply
            | Action::Save
            | Action::OpenThread
            | Action::CloseThread
            | Action::PinMessage
            | Action::ForwardMessage
            | Action::RetryDecryption
            | Action::UserInfo
            | Action::MessageDetails
            // The composer.
            | Action::Send
            | Action::Newline
            | Action::CancelCompose
            | Action::EditLast
            | Action::ComposeInEditor
            | Action::OpenEmoji
            | Action::Attach
            | Action::PastePicture
            | Action::CreatePoll
            | Action::OpenStickers
            // The room as the open one.
            | Action::AcceptInvite
            | Action::DeclineInvite
            | Action::WithdrawKnock
            | Action::Search
            | Action::ToggleUnread
            | Action::OpenPinned => true,

            Action::Quit
            | Action::Suspend
            | Action::Focus(Pane::Sidebar)
            | Action::FocusDir(Dir::Left)
            | Action::FocusRoomFilter
            | Action::ToggleHelp
            | Action::CycleTheme
            | Action::ToggleSidebar
            | Action::Back
            | Action::SidebarUp
            | Action::SidebarDown
            | Action::SidebarOpen
            | Action::SidebarTop
            | Action::SidebarBottom
            | Action::LoginSubmit
            | Action::LoginBrowser
            | Action::RecoverySubmit
            | Action::CloseModal
            | Action::OpenSettings
            | Action::StartVerification
            | Action::RoomPrev
            | Action::RoomNext
            | Action::SearchEverywhere
            | Action::SpaceNext
            | Action::SpacePrev
            | Action::SpaceInfo
            | Action::FilterUnread
            | Action::FilterPeople
            | Action::Verify(_)
            | Action::OpenPalette
            | Action::OpenRoom(_)
            | Action::SelectTheme(_)
            | Action::Setting(_)
            | Action::NewRoom
            | Action::RoomInfo
            | Action::LeaveRoom
            | Action::InviteUser
            | Action::Manage(_)
            | Action::ContextMenu
            | Action::MarkAllRead
            | Action::OpenThreads
            | Action::Devices(_) => false,
        }
    }
}
