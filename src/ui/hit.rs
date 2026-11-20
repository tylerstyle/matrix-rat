//! Mouse hit-testing. While drawing, every clickable thing registers its rectangle;
//! mouse events are then resolved against what was actually on screen.

use ratatui::layout::{Position, Rect};

use crate::action::{Action, Pane};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Help,
    Settings,
    Theme,
    CancelCompose,
    JumpToNewest,
    Palette,
    OwnProfile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputTarget {
    Login(usize),
    Recovery,
    /// The text setting being typed in the settings: the download folder, or the audio
    /// player.
    DownloadDir,
    EmojiQuery,
    PaletteQuery,
    SearchQuery,
    GlobalSearchQuery,
    NewField(usize),
    InfoEdit(crate::manage::InfoRow),
    InfoFilter,
    VerifyPassword,
    IgnoredFilter,
    ReportReason,
    /// Reason input inside `ReasonConfirmPopup`.
    ReasonConfirm,
    /// Inline ban reason input inside `RoomInfo`.
    BanReason,
    /// A text field of the sessions popup or the key backup popup, by its slot.
    Devices(usize),
    /// Display name input inside `OwnProfilePopup`.
    OwnProfileName,
    /// Avatar file path input inside `OwnProfilePopup`.
    OwnProfileAvatar,
    /// Confirmation input inside `SettingSubview::EncryptConfirm`.
    RoomSettingEncrypt,
    /// Image path input inside `SettingSubview::Picture`.
    RoomSettingAvatar,
    /// Alias input inside `SettingSubview::Addresses`.
    RoomSettingAlias,
    /// Question input inside `PollFormPopup`.
    PollQuestion,
    /// Answer input inside `PollFormPopup`, by 0-based index.
    PollAnswer(usize),
    /// Filter input of the sticker picker.
    StickerFilter,
    /// Filter input of the threads popup.
    ThreadsFilter,
    /// The field of the notification keywords popup.
    Keyword,
    /// The typed time of the reminder form.
    Reminder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitId {
    Pane(Pane),
    /// Index into `App::rooms`.
    Room(usize),
    /// A room in the rooms bar above the chat (index into `App::rooms`).
    BarRoom(usize),
    RoomFilter,
    /// Index into the active room's messages.
    Message(usize),
    /// Message index and reaction index.
    Reaction(usize, usize),
    /// Message index and link index within it.
    Link(usize, usize),
    ComposerText,
    /// A text field or button slot in a form.
    Field(usize),
    /// Clickable value region of a one-line text input (`rect.x` is column 0 of the viewport).
    Input(InputTarget),
    /// Index into the emoji picker's results.
    Emoji(usize),
    /// A row of the settings menu, and the step to change it by (0: just activate).
    Setting(usize, i8),
    Splitter,
    Button(Button),
    /// A clickable hint that performs an action, like the ones in the status line.
    Do(Action),
    /// The dimmed area around an overlay; clicking it closes the overlay.
    Backdrop,
    Overlay,
    /// The picture of the message at this index.
    Image(usize),
    /// The play badge / summary line of an audio, voice, or video message.
    PlayMedia(usize),
    /// The button on a message that transcribes or translates it (or stops that,
    /// hides or shows the text): message index and which of the two.
    NoteButton(usize, crate::notes::NoteKind),
    /// The "copy" label on the first line of a code block: message index and the
    /// block's index among that message's code blocks.
    CopyCode(usize, usize),
    /// The thread line under the message at this index.
    Thread(usize),
    /// The quoted-reply row above the message at this index.
    ReplyQuote(usize),
    /// The ✕ of the waiting file at this index in the composer.
    Attachment(usize),
    /// A row in the command palette (index into its filtered entries).
    Palette(usize),
    /// A result row in the global search popup.
    GlobalSearch(usize),
    /// A row in the composer completion popup (index into `completion.items`).
    Complete(usize),
    /// A row of the context menu.
    Menu(usize),
    /// A row in the key-bindings editor (index into `editor.rows()`).
    KeybindRow(usize),
    /// A button in the key-bindings editor.
    KeybindBtn(crate::keymap::EditorButton),
    /// A row in the ignored-users popup.
    IgnoredRow(usize),
    /// The stop-ignoring button on a row of the ignored-users popup.
    IgnoredUnignore(usize),
    /// Toggle for "Also ignore this user" in the report popup.
    ReportIgnoreToggle,
    /// Send report button in the report popup.
    ReportSubmit,
    /// A match in the user directory dropdown inside the direct-chat / invite popup.
    UserSearchRow(usize),
    /// Info action button on a highlighted user directory search row.
    UserSearchInfo(usize),
    /// Clicking a message sender header in the timeline opens User Info.
    Sender(usize),
    /// Selectable row inside `UserInfoPopup`.
    UserInfoRow(usize),
    /// Copy user ID button inside `UserInfoPopup`.
    UserInfoCopy,
    /// Confirm button (`true`) or Cancel (`false`) inside `UserInfoPopup` inline confirm? Or inline Ban submit.
    UserInfoBanSubmit,
    /// Inline role option inside `UserInfoPopup`.
    UserInfoRoleOption(usize),
    /// Focusable slot/button inside `OwnProfilePopup`.
    OwnProfileSlot(usize),
    /// Clickable row in the Pinned messages popup (jump to message).
    PinnedRow(usize),
    /// Unpin action button on a row in the Pinned messages popup.
    PinnedUnpin(usize),
    /// Submit (`true`) or Cancel (`false`) button inside `ReasonConfirmPopup`.
    ReasonConfirmBtn(bool),
    /// Clickable "Open ->" affordance on a forward-success toast.
    Toast,
    /// Clickable poll answer row in the timeline (message index, answer index).
    PollAnswer(usize, usize),
    /// Focusable/clickable slot inside `PollFormPopup`.
    PollFormSlot(usize),
    /// Remove-answer button (`×`) on an answer row in `PollFormPopup`.
    PollFormRemove(usize),
    /// Stepper `-1`/`+1` on max selections in `PollFormPopup`.
    PollFormMaxStep(i8),
    /// Answer row inside `PollVotePopup`.
    PollVoteAnswer(usize),
    /// Submit (`true`) or Cancel (`false`) button inside `PollVotePopup`.
    PollVoteBtn(bool),
    /// A row of the sticker picker (index into its filtered list); clicking sends it.
    StickerRow(usize),
    /// "View source" / "Back to details" in the message details popup.
    DetailsFlip,
    /// "Copy event ID" / "Copy JSON" in the message details popup.
    DetailsCopy,
    /// "Copy as text" in the doctor's popup.
    DoctorCopy,
    /// A thread in the threads popup (index into what it shows); clicking opens it.
    ThreadRow(usize),
    /// The row after the threads in the threads popup that loads more of them.
    ThreadsMore,
    /// The "only mine" switch of the threads popup.
    ThreadsMine,
    /// A row of the notification keywords popup.
    KeywordRow(usize),
    /// The remove mark on a row of the notification keywords popup.
    KeywordRemove(usize),
    /// An entry of the saved messages popup; clicking goes to the message.
    SavedRow(usize),
    /// The "remind" mark on an entry of the saved messages popup.
    SavedRemind(usize),
    /// The "remove" mark on an entry of the saved messages popup.
    SavedRemove(usize),
    /// The space between the marks of an entry: a click that missed a mark must not
    /// go to the message instead.
    SavedGap,
    /// A row of the reminder form: a fixed choice, the typed time, or "no reminder".
    ReminderChoice(usize),
    /// "n reminders due" in the status line.
    ReminderDue,
}

#[derive(Debug, Default)]
pub struct HitMap {
    entries: Vec<(Rect, HitId)>,
}

impl HitMap {
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Later registrations lie on top of earlier ones.
    pub fn add(&mut self, rect: Rect, id: HitId) {
        if rect.width > 0 && rect.height > 0 {
            self.entries.push((rect, id));
        }
    }

    #[cfg(test)]
    pub fn at(&self, column: u16, row: u16) -> Option<HitId> {
        self.at_rect(column, row).map(|(_, id)| id)
    }

    pub fn at_rect(&self, column: u16, row: u16) -> Option<(Rect, HitId)> {
        let position = Position::new(column, row);
        self.entries
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(position))
            .copied()
    }

    /// Where something was drawn in the last frame: the smallest rectangle around
    /// everything registered for it (a message is registered row by row).
    pub fn rect_of(&self, id: HitId) -> Option<Rect> {
        self.entries
            .iter()
            .filter(|(_, entry)| *entry == id)
            .map(|(rect, _)| *rect)
            .reduce(|all, rect| all.union(rect))
    }

    /// The pane under the pointer, looking through whatever lies on top of it.
    pub fn pane_at(&self, column: u16, row: u16) -> Option<Pane> {
        let position = Position::new(column, row);
        self.entries.iter().rev().find_map(|(rect, id)| match id {
            HitId::Pane(pane) if rect.contains(position) => Some(*pane),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topmost_wins_and_empty_rects_are_ignored() {
        let mut map = HitMap::default();
        map.add(Rect::new(0, 0, 10, 10), HitId::Pane(Pane::Timeline));
        map.add(Rect::new(2, 2, 3, 1), HitId::Message(4));
        map.add(Rect::new(0, 0, 0, 5), HitId::Splitter);
        assert_eq!(map.at(3, 2), Some(HitId::Message(4)));
        assert_eq!(map.at(5, 2), Some(HitId::Pane(Pane::Timeline)));
        assert_eq!(map.at(0, 0), Some(HitId::Pane(Pane::Timeline)));
        assert_eq!(map.at(10, 10), None);
        assert_eq!(map.pane_at(3, 2), Some(Pane::Timeline));
        assert_eq!(map.rect_of(HitId::Message(4)), Some(Rect::new(2, 2, 3, 1)));
        map.add(Rect::new(2, 3, 5, 1), HitId::Message(4));
        assert_eq!(map.rect_of(HitId::Message(4)), Some(Rect::new(2, 2, 5, 2)));
        assert_eq!(map.rect_of(HitId::Message(5)), None);
        map.clear();
        assert_eq!(map.at(3, 2), None);
    }
}
