//! State of the popups that take over input: login, recovery key, emoji picker.

use emojis::Emoji;

use crate::emoji;
use crate::lineedit::LineEdit;
use crate::matrix::{ReauthKind, ReauthTarget};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReauthInfo {
    pub user_id: String,
    pub device_id: String,
    pub kind: ReauthKind,
}

#[derive(Clone)]
pub struct LoginForm {
    pub homeserver: LineEdit,
    pub username: LineEdit,
    pub password: LineEdit,
    /// 0–2 are the text fields, 3 and 4 the two buttons.
    pub focus: usize,
    /// A progress message while a login is running; input is locked meanwhile.
    pub busy: Option<String>,
    pub error: Option<String>,
    /// Non-error informational notice shown when entering re-authenticate mode.
    pub note: Option<String>,
    /// Shown during a browser login, in case the browser did not open by itself.
    pub browser_url: Option<String>,
    /// Present after a soft logout: homeserver and user are fixed so logging in again
    /// preserves the device and local encrypted store.
    pub reauth: Option<ReauthInfo>,
}

impl Default for LoginForm {
    fn default() -> Self {
        Self {
            homeserver: LineEdit::new(),
            username: LineEdit::new(),
            password: LineEdit::secret(),
            focus: 0,
            busy: None,
            error: None,
            note: None,
            browser_url: None,
            reauth: None,
        }
    }
}

impl LoginForm {
    pub const FIELDS: usize = 3;
    pub const PASSWORD_BUTTON: usize = 3;
    pub const BROWSER_BUTTON: usize = 4;
    const SLOTS: usize = 5;

    pub fn new(error: Option<String>) -> Self {
        Self {
            homeserver: LineEdit::from("matrix.org"),
            error,
            ..Self::default()
        }
    }

    pub fn reauth(target: ReauthTarget, note: String) -> Self {
        let focus = match target.kind {
            ReauthKind::Password => 2,
            ReauthKind::Browser => Self::BROWSER_BUTTON,
        };
        Self {
            homeserver: LineEdit::from(target.homeserver),
            username: LineEdit::from(target.user_id.clone()),
            password: LineEdit::secret(),
            focus,
            busy: None,
            error: None,
            note: Some(note),
            browser_url: None,
            reauth: Some(ReauthInfo {
                user_id: target.user_id,
                device_id: target.device_id,
                kind: target.kind,
            }),
        }
    }

    pub fn is_reauth(&self) -> bool {
        self.reauth.is_some()
    }

    pub fn allows_password(&self) -> bool {
        matches!(
            &self.reauth,
            None | Some(ReauthInfo {
                kind: ReauthKind::Password,
                ..
            })
        )
    }

    pub fn allows_browser(&self) -> bool {
        matches!(
            &self.reauth,
            None | Some(ReauthInfo {
                kind: ReauthKind::Browser,
                ..
            })
        )
    }

    pub fn can_focus(&self, slot: usize) -> bool {
        match &self.reauth {
            None => slot < Self::SLOTS,
            Some(info) => match info.kind {
                ReauthKind::Password => slot == 2 || slot == Self::PASSWORD_BUTTON,
                ReauthKind::Browser => slot == Self::BROWSER_BUTTON,
            },
        }
    }

    pub fn focus_slot(&mut self, slot: usize) {
        if self.can_focus(slot) {
            self.focus = slot;
        }
    }

    pub fn field_mut(&mut self) -> Option<&mut LineEdit> {
        if !self.can_focus(self.focus) {
            return None;
        }
        match self.focus {
            0 => Some(&mut self.homeserver),
            1 => Some(&mut self.username),
            2 => Some(&mut self.password),
            _ => None,
        }
    }

    pub fn field_at_mut(&mut self, slot: usize) -> Option<&mut LineEdit> {
        if !self.can_focus(slot) {
            return None;
        }
        match slot {
            0 => Some(&mut self.homeserver),
            1 => Some(&mut self.username),
            2 => Some(&mut self.password),
            _ => None,
        }
    }

    pub fn next(&mut self) {
        for _ in 0..Self::SLOTS {
            self.focus = (self.focus + 1) % Self::SLOTS;
            if self.can_focus(self.focus) {
                break;
            }
        }
    }

    pub fn previous(&mut self) {
        for _ in 0..Self::SLOTS {
            self.focus = (self.focus + Self::SLOTS - 1) % Self::SLOTS;
            if self.can_focus(self.focus) {
                break;
            }
        }
    }
}

#[derive(Clone)]
pub struct RecoveryForm {
    pub key: LineEdit,
    pub busy: bool,
    pub error: Option<String>,
}

impl Default for RecoveryForm {
    fn default() -> Self {
        Self {
            key: LineEdit::secret(),
            busy: false,
            error: None,
        }
    }
}

// Hand-written so the password and the recovery key can never end up in a log.
impl std::fmt::Debug for LoginForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginForm")
            .field("homeserver", &self.homeserver)
            .field("username", &self.username)
            .field("focus", &self.focus)
            .field("busy", &self.busy)
            .field("reauth", &self.reauth.as_ref().map(|r| r.kind))
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for RecoveryForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryForm")
            .field("busy", &self.busy)
            .finish_non_exhaustive()
    }
}

/// What the reaction picker offers before anything is typed.
const QUICK_REACTIONS: [&str; 10] = ["👍", "❤️", "😂", "🎉", "😮", "😢", "🙏", "👀", "✅", "🔥"];

#[derive(Debug, Clone)]
pub struct EmojiPicker {
    /// Index and id of the message to react to; `None` inserts into the draft.
    pub react_to: Option<(usize, String)>,
    pub query: LineEdit,
    pub results: Vec<&'static Emoji>,
    pub selected: usize,
    /// First visible row of the grid.
    pub scroll: usize,
    /// Grid size as of the last draw; arrow keys need it.
    pub columns: usize,
    pub rows: usize,
    /// Ask the next draw to bring the selection into view.
    pub reveal: bool,
}

impl Default for EmojiPicker {
    fn default() -> Self {
        Self {
            react_to: None,
            query: LineEdit::new(),
            results: emoji::search(""),
            selected: 0,
            scroll: 0,
            columns: 12,
            rows: 8,
            reveal: true,
        }
    }
}

impl EmojiPicker {
    pub fn for_reaction(index: usize, message_id: String) -> Self {
        let mut picker = Self {
            react_to: Some((index, message_id)),
            ..Self::default()
        };
        picker.refilter();
        picker
    }

    #[cfg(test)]
    pub fn set_query(&mut self, query: String) {
        self.query.set(query);
        self.refilter();
    }

    pub fn refilter(&mut self) {
        self.results = emoji::search(&self.query);
        if self.query.is_empty() && self.react_to.is_some() {
            let quick: Vec<_> = QUICK_REACTIONS
                .iter()
                .filter_map(|e| emojis::get(e))
                .collect();
            self.results.retain(|e| !quick.contains(e));
            self.results.splice(0..0, quick);
        }
        self.selected = 0;
        self.scroll = 0;
    }

    pub fn selected(&self) -> Option<&'static Emoji> {
        self.results.get(self.selected).copied()
    }

    /// Moves the selection, staying inside the results.
    pub fn step(&mut self, delta: isize) {
        if self.results.is_empty() {
            return;
        }
        let last = self.results.len() as isize - 1;
        let target = self.selected as isize + delta;
        // Moving a whole row past either end stays put instead of jumping to the corner.
        if (0..=last).contains(&target) {
            self.selected = target as usize;
        } else if delta.unsigned_abs() == 1 {
            self.selected = target.clamp(0, last) as usize;
        }
        self.reveal = true;
    }

    pub fn scroll_by(&mut self, rows: isize) {
        let total = self.results.len().div_ceil(self.columns.max(1));
        let max = total.saturating_sub(self.rows) as isize;
        self.scroll = (self.scroll as isize + rows).clamp(0, max.max(0)) as usize;
    }
}

/// The rows of the settings menu, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Theme,
    SidebarWidth,
    RoomPreviews,
    Mouse,
    TypingNotices,
    ReadReceipts,
    Notifications,
    DownloadDir,
    Keys,
    Encryption,
    Profile,
    Logout,
    /// Whether the messages say who has read them (shown next to `ReadReceipts`).
    ReadMarkers,
    /// Theme used when the terminal is dark.
    ThemeDark,
    /// Theme used when the terminal is light.
    ThemeLight,
    /// Whether separate dark and light theme slots are enabled and shown.
    ThemePairing,
    /// Manage users whose messages, previews and invitations are hidden.
    IgnoredUsers,
    /// The account's sessions (devices): rename, verify, sign out.
    Sessions,
    /// Key backup and recovery, and message keys to and from a file.
    KeyBackup,
}

impl Setting {
    pub const ALL: [Setting; 19] = [
        Setting::Theme,
        Setting::ThemePairing,
        Setting::ThemeDark,
        Setting::ThemeLight,
        Setting::SidebarWidth,
        Setting::RoomPreviews,
        Setting::Mouse,
        Setting::TypingNotices,
        Setting::ReadReceipts,
        Setting::ReadMarkers,
        Setting::Notifications,
        Setting::DownloadDir,
        Setting::Keys,
        Setting::Profile,
        Setting::Encryption,
        Setting::Sessions,
        Setting::KeyBackup,
        Setting::IgnoredUsers,
        Setting::Logout,
    ];

    /// The heading shown above this row, if it starts a group.
    pub fn heading(self) -> Option<&'static str> {
        match self {
            Setting::Theme => Some("Appearance"),
            Setting::Mouse => Some("Behaviour"),
            Setting::Keys => Some("Help"),
            Setting::Profile => Some("Account"),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Setting::Theme => "Theme",
            Setting::ThemePairing => "Follow dark/light",
            Setting::ThemeDark => "Dark theme",
            Setting::ThemeLight => "Light theme",
            Setting::SidebarWidth => "Room list width",
            Setting::RoomPreviews => "Room previews",
            Setting::Mouse => "Mouse",
            Setting::TypingNotices => "Typing notices",
            Setting::ReadReceipts => "Read receipts",
            Setting::ReadMarkers => "Read markers",
            Setting::Notifications => "Notifications",
            Setting::DownloadDir => "Download folder",
            Setting::Keys => "Key bindings",
            Setting::Profile => "Profile",
            Setting::Encryption => "Encryption",
            Setting::Sessions => "Sessions",
            Setting::KeyBackup => "Key backup",
            Setting::IgnoredUsers => "Ignored users",
            Setting::Logout => "Log out",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Setting::Theme => {
                "F3 cycles this, or the active dark/light slot when paired; themes/ files reload live."
            }
            Setting::ThemePairing => "Use separate themes for dark and light desktop appearance.",
            Setting::ThemeDark => "Used when dark mode is active; (unset) follows Theme.",
            Setting::ThemeLight => "Used when light mode is active; (unset) follows Theme.",
            Setting::SidebarWidth => "You can also drag the line next to the room list.",
            Setting::RoomPreviews => {
                "Show the latest message under each room (and larger room pictures)."
            }
            Setting::Mouse => "Off: the terminal's own text selection works instead of clicking.",
            Setting::TypingNotices => "Show others when you are typing.",
            Setting::ReadReceipts => "Show others how far you have read.",
            Setting::ReadMarkers => "Show under messages who has read up to there.",
            Setting::Notifications => {
                "Tell you about new messages while you are in another room or window."
            }
            Setting::DownloadDir => {
                "Where s saves files and pictures. Enter to change; empty for the default."
            }
            Setting::Keys => {
                "Edit key bindings or switch presets (F1 still opens the key overview)."
            }
            Setting::Profile => "Change your display name and profile picture.",
            Setting::Encryption => {
                "Verify this device with another of your devices, or see its status."
            }
            Setting::Sessions => {
                "Where your account is logged in: rename, verify or sign out a session."
            }
            Setting::KeyBackup => {
                "Whether your message keys are backed up; recovery key; keys to or from a file."
            }
            Setting::IgnoredUsers => {
                "View or unignore users whose messages and invitations are hidden."
            }
            Setting::Logout => {
                "Ends this session. Encrypted history needs your recovery key again."
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SettingsMenu {
    /// Index into `Setting::ALL`.
    pub selected: usize,
    /// Log out was chosen once; choosing it again does it.
    pub confirm_logout: bool,
    /// The download folder while it is being typed.
    pub editing: Option<LineEdit>,
    /// Whether `ThemeDark` and `ThemeLight` slot rows are shown.
    pub show_theme_pair: bool,
}

impl SettingsMenu {
    pub fn for_pairing(paired: bool) -> Self {
        Self {
            show_theme_pair: paired,
            ..Self::default()
        }
    }

    pub fn select(setting: Setting, show_theme_pair: bool) -> Self {
        let selected = Setting::ALL.iter().position(|s| *s == setting).unwrap_or(0);
        Self {
            selected,
            show_theme_pair,
            ..Self::default()
        }
    }

    pub fn shows(&self, setting: Setting) -> bool {
        self.show_theme_pair || !matches!(setting, Setting::ThemeDark | Setting::ThemeLight)
    }

    pub fn current(&self) -> Setting {
        let current = Setting::ALL[self.selected.min(Setting::ALL.len() - 1)];
        if self.shows(current) {
            current
        } else {
            Setting::Theme
        }
    }

    pub fn step(&mut self, delta: isize) {
        if delta != 0 {
            let count = Setting::ALL.len() as isize;
            let mut idx = self.selected.min(Setting::ALL.len() - 1) as isize;
            for _ in 0..Setting::ALL.len() {
                idx = (idx + delta).rem_euclid(count);
                if self.shows(Setting::ALL[idx as usize]) {
                    self.selected = idx as usize;
                    break;
                }
            }
        }
        self.confirm_logout = false;
        self.editing = None;
    }
}

/// Pop-up showing the account's `m.ignored_user_list` with search and unignore.
#[derive(Clone)]
pub struct IgnoredUsersPopup {
    pub filter: LineEdit,
    pub all: Vec<String>,
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub scroll: usize,
    pub page: usize,
    pub reveal: bool,
}

impl std::fmt::Debug for IgnoredUsersPopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IgnoredUsersPopup")
            .field("count", &self.all.len())
            .field("shown", &self.filtered.len())
            .field("selected", &self.selected)
            .finish()
    }
}

impl IgnoredUsersPopup {
    pub fn new<'a>(users: impl IntoIterator<Item = &'a String>) -> Self {
        let mut all: Vec<String> = users.into_iter().cloned().collect();
        all.sort();
        all.dedup();
        let filtered: Vec<usize> = (0..all.len()).collect();
        Self {
            filter: LineEdit::new().with_max_bytes(256),
            all,
            filtered,
            selected: 0,
            scroll: 0,
            page: 0,
            reveal: false,
        }
    }

    pub fn sync_users<'a>(&mut self, users: impl IntoIterator<Item = &'a String>) {
        let keep = self.selected_user().map(str::to_owned);
        let mut all: Vec<String> = users.into_iter().cloned().collect();
        all.sort();
        all.dedup();
        self.all = all;
        self.refilter_preserving(keep.as_deref());
    }

    pub fn refilter(&mut self) {
        let keep = self.selected_user().map(str::to_owned);
        self.refilter_preserving(keep.as_deref());
    }

    fn refilter_preserving(&mut self, keep: Option<&str>) {
        let query = self.filter.as_str().trim().to_lowercase();
        self.filtered = if query.is_empty() {
            (0..self.all.len()).collect()
        } else {
            self.all
                .iter()
                .enumerate()
                .filter(|(_, user)| user.to_lowercase().contains(&query))
                .map(|(index, _)| index)
                .collect()
        };
        self.selected = keep
            .and_then(|wanted| {
                self.filtered
                    .iter()
                    .position(|&idx| self.all.get(idx).map(String::as_str) == Some(wanted))
            })
            .unwrap_or_else(|| self.selected.min(self.filtered.len().saturating_sub(1)));
        self.reveal = true;
    }

    pub fn selected_user(&self) -> Option<&str> {
        self.filtered
            .get(self.selected)
            .and_then(|&idx| self.all.get(idx))
            .map(String::as_str)
    }

    pub fn step(&mut self, delta: isize) {
        let len = self.filtered.len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let max = (len - 1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, max) as usize;
        self.reveal = true;
    }

    pub fn scroll_by(&mut self, lines: isize) {
        self.scroll = (self.scroll as isize + lines).max(0) as usize;
        self.reveal = false;
    }
}

/// Pop-up for reporting an offending message to the homeserver.
#[derive(Clone)]
pub struct ReportForm {
    pub room_id: String,
    pub event_id: String,
    pub sender_id: Option<String>,
    pub sender_name: String,
    pub reason: LineEdit,
    pub can_ignore: bool,
    pub also_ignore: bool,
    pub focus: usize,
    pub error: Option<String>,
}

/// Hand-written: event IDs, user IDs, names, and reasons stay out of logs.
impl std::fmt::Debug for ReportForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReportForm")
            .field("room_id", &self.room_id)
            .field("can_ignore", &self.can_ignore)
            .field("also_ignore", &self.also_ignore)
            .field("focus", &self.focus)
            .field("reason_bytes", &self.reason.as_str().len())
            .finish()
    }
}

impl ReportForm {
    pub const REASON: usize = 0;
    pub const REASON_MAX_BYTES: usize = 512;

    pub fn new(
        room_id: String,
        event_id: String,
        sender_id: Option<String>,
        sender_name: String,
        already_ignored: bool,
    ) -> Self {
        let can_ignore = sender_id.is_some() && !already_ignored;
        Self {
            room_id,
            event_id,
            sender_id,
            sender_name,
            reason: LineEdit::new().with_max_bytes(Self::REASON_MAX_BYTES),
            can_ignore,
            also_ignore: false,
            focus: 0,
            error: None,
        }
    }

    pub fn slots(&self) -> usize {
        if self.can_ignore { 3 } else { 2 }
    }

    pub fn toggle_slot(&self) -> Option<usize> {
        self.can_ignore.then_some(1)
    }

    pub fn submit_slot(&self) -> usize {
        if self.can_ignore { 2 } else { 1 }
    }

    pub fn step(&mut self, delta: isize) {
        let count = self.slots() as isize;
        self.focus = (self.focus as isize + delta).rem_euclid(count) as usize;
    }

    pub fn flip_ignore(&mut self) {
        if self.can_ignore {
            self.also_ignore = !self.also_ignore;
        }
    }
}

/// Pop-up listing a room's pinned messages (`m.room.pinned_events`).
#[derive(Clone)]
pub struct PinnedPopup {
    pub room_id: String,
    pub room_name: String,
    pub items: Vec<crate::model::PinnedMessage>,
    pub loading: bool,
    pub error: Option<String>,
    pub can_pin: bool,
    pub selected: usize,
    pub scroll: usize,
    pub page: usize,
    pub reveal: bool,
}

impl std::fmt::Debug for PinnedPopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedPopup")
            .field("room_id", &self.room_id)
            .field("items", &self.items.len())
            .field("loading", &self.loading)
            .field("can_pin", &self.can_pin)
            .finish()
    }
}

impl PinnedPopup {
    pub fn open(room_id: String, room_name: String, can_pin: bool) -> Self {
        Self {
            room_id,
            room_name,
            items: Vec::new(),
            loading: true,
            error: None,
            can_pin,
            selected: 0,
            scroll: 0,
            page: 10,
            reveal: true,
        }
    }

    pub fn set_items(&mut self, items: Vec<crate::model::PinnedMessage>, error: Option<String>) {
        let prev_id = self.selected_item().map(|m| m.event_id.clone());
        self.items = items;
        self.loading = false;
        self.error = error;
        self.selected = prev_id
            .and_then(|id| self.items.iter().position(|m| m.event_id == id))
            .unwrap_or_else(|| self.selected.min(self.items.len().saturating_sub(1)));
        self.reveal = true;
    }

    pub fn selected_item(&self) -> Option<&crate::model::PinnedMessage> {
        self.items.get(self.selected)
    }

    pub fn remove_event(&mut self, event_id: &str) {
        self.items.retain(|m| m.event_id != event_id);
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
        self.reveal = true;
    }

    pub fn step(&mut self, delta: isize) {
        if self.items.is_empty() {
            self.selected = 0;
            return;
        }
        let max = (self.items.len() - 1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, max) as usize;
        self.reveal = true;
    }

    pub fn scroll_by(&mut self, lines: isize) {
        self.scroll = (self.scroll as isize + lines).max(0) as usize;
        self.reveal = false;
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
        self.reveal = true;
    }

    pub fn select_last(&mut self) {
        self.selected = self.items.len().saturating_sub(1);
        self.reveal = true;
    }

    pub fn clamp_scroll(&mut self, visible: usize) {
        if self.items.is_empty() {
            self.selected = 0;
            self.scroll = 0;
            return;
        }
        self.selected = self.selected.min(self.items.len() - 1);
        if self.reveal {
            self.reveal = false;
            if self.selected < self.scroll {
                self.scroll = self.selected;
            } else if self.selected >= self.scroll + visible {
                self.scroll = self.selected + 1 - visible;
            }
        }
        self.scroll = self.scroll.min(self.items.len().saturating_sub(visible));
    }
}

/// Moderation confirmation prompt with an optional reason field.
#[derive(Clone, PartialEq, Eq)]
pub enum ReasonConfirmKind {
    Redact {
        room_id: String,
        item: crate::model::ItemRef,
        sender_name: String,
        preview: String,
    },
    Ban {
        room_id: String,
        user_id: String,
        user_name: String,
    },
}

impl std::fmt::Debug for ReasonConfirmKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Redact { room_id, .. } => write!(f, "ReasonConfirmKind::Redact({room_id})"),
            Self::Ban { room_id, .. } => write!(f, "ReasonConfirmKind::Ban({room_id})"),
        }
    }
}

#[derive(Clone)]
pub struct ReasonConfirmPopup {
    pub kind: ReasonConfirmKind,
    pub reason: LineEdit,
    /// Focus slot: `0` = reason input, `1` = confirm button, `2` = cancel button.
    pub focus: usize,
}

impl std::fmt::Debug for ReasonConfirmPopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReasonConfirmPopup")
            .field("kind", &self.kind)
            .field("reason_bytes", &self.reason.as_str().len())
            .field("focus", &self.focus)
            .finish()
    }
}

impl ReasonConfirmPopup {
    pub const REASON: usize = 0;
    pub const CONFIRM: usize = 1;
    pub const CANCEL: usize = 2;
    pub const SLOTS: usize = 3;
    pub const REASON_MAX_BYTES: usize = 256;

    pub fn redact(
        room_id: String,
        item: crate::model::ItemRef,
        sender_name: String,
        preview: String,
    ) -> Self {
        Self {
            kind: ReasonConfirmKind::Redact {
                room_id,
                item,
                sender_name,
                preview,
            },
            reason: LineEdit::new().with_max_bytes(Self::REASON_MAX_BYTES),
            focus: Self::REASON,
        }
    }

    pub fn ban(room_id: String, user_id: String, user_name: String) -> Self {
        Self {
            kind: ReasonConfirmKind::Ban {
                room_id,
                user_id,
                user_name,
            },
            reason: LineEdit::new().with_max_bytes(Self::REASON_MAX_BYTES),
            focus: Self::REASON,
        }
    }

    pub fn step(&mut self, delta: isize) {
        self.focus = (self.focus as isize + delta).rem_euclid(Self::SLOTS as isize) as usize;
    }

    pub fn on_reason(&self) -> bool {
        self.focus == Self::REASON
    }

    pub fn on_submit(&self) -> bool {
        self.focus == Self::CONFIRM
    }

    pub fn on_cancel(&self) -> bool {
        self.focus == Self::CANCEL
    }

    pub fn focus_reason(&mut self) {
        self.focus = Self::REASON;
    }

    pub fn flip_button(&mut self) {
        self.focus = if self.focus == Self::CONFIRM {
            Self::CANCEL
        } else {
            Self::CONFIRM
        };
    }
}

pub const MAX_POLL_ANSWERS: usize = 20;
pub const MIN_POLL_ANSWERS: usize = 2;

/// Logical slots inside [`PollFormPopup`], matching Tab order top-to-bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollFormSlot {
    Question,
    Answer(usize),
    AddAnswer,
    Kind,
    MaxSelections,
    Submit,
    Cancel,
}

/// Modal form for creating a new Matrix poll or editing an unvoted poll of ours.
#[derive(Clone)]
pub struct PollFormPopup {
    pub room_id: String,
    pub editing: Option<crate::model::ItemRef>,
    pub question: LineEdit,
    pub answers: Vec<LineEdit>,
    pub disclosed: bool,
    pub max_selections: usize,
    pub focus: usize,
    pub scroll: usize,
    pub error: Option<String>,
}

impl std::fmt::Debug for PollFormPopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PollFormPopup")
            .field("editing", &self.editing.is_some())
            .field("answers", &self.answers.len())
            .field("disclosed", &self.disclosed)
            .field("max_selections", &self.max_selections)
            .field("focus", &self.focus)
            .finish_non_exhaustive()
    }
}

impl PollFormPopup {
    pub fn create(room_id: String, initial_question: &str) -> Self {
        let mut question = LineEdit::new();
        let initial = crate::text::sanitize_line(initial_question.trim());
        let focus = if initial.is_empty() { 0 } else { 1 };
        question.set(initial);
        Self {
            room_id,
            editing: None,
            question,
            answers: vec![LineEdit::new(), LineEdit::new()],
            disclosed: true,
            max_selections: 1,
            focus,
            scroll: 0,
            error: None,
        }
    }

    pub fn edit(
        room_id: String,
        target: crate::model::ItemRef,
        poll: &crate::model::PollInfo,
    ) -> Self {
        let mut question = LineEdit::new();
        question.set(poll.question.clone());
        let mut answers: Vec<LineEdit> = poll
            .answers
            .iter()
            .take(MAX_POLL_ANSWERS)
            .map(|a| {
                let mut edit = LineEdit::new();
                edit.set(a.text.clone());
                edit
            })
            .collect();
        while answers.len() < MIN_POLL_ANSWERS {
            answers.push(LineEdit::new());
        }
        let max_selections = usize::try_from(poll.max_selections)
            .unwrap_or(1)
            .clamp(1, answers.len());
        Self {
            room_id,
            editing: Some(target),
            question,
            answers,
            disclosed: poll.disclosed,
            max_selections,
            focus: 0,
            scroll: 0,
            error: None,
        }
    }

    pub fn slots(&self) -> Vec<PollFormSlot> {
        let mut out = Vec::with_capacity(self.answers.len() + 6);
        out.push(PollFormSlot::Question);
        for i in 0..self.answers.len() {
            out.push(PollFormSlot::Answer(i));
        }
        if self.answers.len() < MAX_POLL_ANSWERS {
            out.push(PollFormSlot::AddAnswer);
        }
        out.push(PollFormSlot::Kind);
        out.push(PollFormSlot::MaxSelections);
        out.push(PollFormSlot::Submit);
        out.push(PollFormSlot::Cancel);
        out
    }

    pub fn current_slot(&self) -> PollFormSlot {
        let slots = self.slots();
        slots
            .get(self.focus.min(slots.len().saturating_sub(1)))
            .copied()
            .unwrap_or(PollFormSlot::Question)
    }

    pub fn step(&mut self, delta: isize) {
        let len = self.slots().len();
        if len == 0 {
            return;
        }
        self.focus = (self.focus as isize + delta).rem_euclid(len as isize) as usize;
    }

    pub fn add_answer(&mut self) -> bool {
        if self.answers.len() >= MAX_POLL_ANSWERS {
            return false;
        }
        let new_idx = self.answers.len();
        self.answers.push(LineEdit::new());
        self.focus = 1 + new_idx;
        self.error = None;
        true
    }

    pub fn remove_answer(&mut self, index: usize) -> bool {
        if self.answers.len() <= MIN_POLL_ANSWERS || index >= self.answers.len() {
            return false;
        }
        self.answers.remove(index);
        self.max_selections = self.max_selections.clamp(1, self.answers.len());
        let max_slot = self.slots().len().saturating_sub(1);
        self.focus = self.focus.min(max_slot);
        self.error = None;
        true
    }

    pub fn flip_kind(&mut self) {
        self.disclosed = !self.disclosed;
        self.error = None;
    }

    pub fn step_max(&mut self, delta: i8) {
        let limit = self.answers.len().max(1);
        if delta < 0 {
            self.max_selections = self.max_selections.saturating_sub(1).max(1);
        } else if delta > 0 {
            self.max_selections = (self.max_selections + 1).min(limit);
        }
        self.error = None;
    }

    pub fn active_input_mut(&mut self) -> Option<&mut LineEdit> {
        match self.current_slot() {
            PollFormSlot::Question => Some(&mut self.question),
            PollFormSlot::Answer(i) => self.answers.get_mut(i),
            _ => None,
        }
    }

    pub fn validated(&self) -> Result<(String, Vec<String>, bool, u32), (usize, String)> {
        let question = crate::text::sanitize_line(self.question.trim());
        if question.is_empty() {
            return Err((0, "Enter a poll question".to_owned()));
        }
        let mut answers = Vec::new();
        let mut first_empty = None;
        for (i, field) in self.answers.iter().enumerate() {
            let clean = crate::text::sanitize_line(field.trim());
            if clean.is_empty() {
                first_empty.get_or_insert(i);
            } else {
                answers.push(clean);
            }
        }
        if answers.len() < MIN_POLL_ANSWERS {
            let slot = first_empty.map_or(1, |i| 1 + i);
            return Err((slot, "Enter at least two answers".to_owned()));
        }
        let max_sel = (self.max_selections.clamp(1, answers.len())) as u32;
        Ok((question, answers, self.disclosed, max_sel))
    }
}

/// Modal chooser for voting or changing your vote in a Matrix poll.
#[derive(Clone)]
pub struct PollVotePopup {
    pub room_id: String,
    pub poll_event_id: String,
    pub question: String,
    pub disclosed: bool,
    pub max_selections: u64,
    pub answers: Vec<crate::model::PollAnswer>,
    pub selected_ids: Vec<String>,
    pub cursor: usize,
    pub total_voters: u32,
    pub ended: bool,
    pub scroll: usize,
    pub error: Option<String>,
}

impl std::fmt::Debug for PollVotePopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PollVotePopup")
            .field("disclosed", &self.disclosed)
            .field("max_selections", &self.max_selections)
            .field("answers", &self.answers.len())
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

impl PollVotePopup {
    pub fn from_poll(
        room_id: String,
        poll_event_id: String,
        poll: &crate::model::PollInfo,
    ) -> Self {
        let selected_ids = poll.own_selection_ids();
        let cursor = poll.answers.iter().position(|a| a.own).unwrap_or(0);
        Self {
            room_id,
            poll_event_id,
            question: poll.question.clone(),
            disclosed: poll.disclosed,
            max_selections: poll.max_selections,
            answers: poll.answers.clone(),
            selected_ids,
            cursor,
            total_voters: poll.total_voters,
            ended: poll.ended,
            scroll: 0,
            error: None,
        }
    }

    pub fn shows_counts(&self) -> bool {
        self.disclosed || self.ended
    }

    pub fn is_multi(&self) -> bool {
        self.max_selections > 1
    }

    pub fn effective_max(&self) -> usize {
        if self.answers.is_empty() {
            return 0;
        }
        usize::try_from(self.max_selections)
            .unwrap_or(self.answers.len())
            .clamp(1, self.answers.len())
    }

    pub fn is_selected(&self, id: &str) -> bool {
        self.selected_ids.iter().any(|s| s == id)
    }

    pub fn step(&mut self, delta: isize) {
        if self.answers.is_empty() {
            return;
        }
        let max = self.answers.len() - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, max as isize) as usize;
        self.error = None;
    }

    pub fn toggle_at(&mut self, index: usize) {
        let Some(answer) = self.answers.get(index) else {
            return;
        };
        let id = answer.id.clone();
        self.error = None;
        if !self.is_multi() {
            if self.is_selected(&id) {
                self.selected_ids.clear();
            } else {
                self.selected_ids = vec![id];
            }
            return;
        }
        if let Some(pos) = self.selected_ids.iter().position(|s| s == &id) {
            self.selected_ids.remove(pos);
            return;
        }
        let cap = self.effective_max();
        if self.selected_ids.len() >= cap {
            self.error = Some(format!("Choose at most {cap} answers"));
            return;
        }
        self.selected_ids.push(id);
    }

    pub fn ordered_selection(&self) -> Vec<String> {
        self.answers
            .iter()
            .filter(|a| self.is_selected(&a.id))
            .map(|a| a.id.clone())
            .collect()
    }
}

#[derive(Debug, Clone)]
pub enum Modal {
    Login(LoginForm),
    Recovery(RecoveryForm),
    Emoji(EmojiPicker),
    Settings(SettingsMenu),
    Verify(crate::verify::VerifyPopup),
    Search(crate::app::search::SearchBar),
    Palette(crate::palette::Palette),
    /// Global full-text search across loaded rooms.
    GlobalSearch(crate::app::global_search::GlobalSearch),
    /// Join a room, start a direct chat, create a room, or invite someone.
    New(Box<crate::manage::NewPopup>),
    RoomInfo(Box<crate::manage::InfoPopup>),
    /// Profile details, shared rooms, and actions for a user.
    UserInfo(Box<crate::people::UserInfoPopup>),
    /// Account display name and avatar editor.
    OwnProfile(crate::people::OwnProfilePopup),
    /// The context menu of a message, a room, a link or the composer.
    Menu(crate::menu::Menu),
    /// Configure key bindings and preset.
    Keybinds(crate::keymap::Editor),
    /// Manage the account's ignored users list.
    IgnoredUsers(IgnoredUsersPopup),
    /// Report an offending message to the homeserver.
    Report(ReportForm),
    /// Pinned messages of the open room.
    Pinned(PinnedPopup),
    /// Moderation action confirmation with optional reason (redact other's message or ban user).
    ReasonConfirm(ReasonConfirmPopup),
    /// The account's sessions (devices).
    Sessions(Box<crate::devices::SessionsPopup>),
    /// Key backup and recovery; message keys to and from a file.
    Keys(Box<crate::devices::KeysPopup>),
    /// Create or edit a poll (`org.matrix.msc3381.poll.start`).
    PollForm(PollFormPopup),
    /// Vote or change vote in a poll (`org.matrix.msc3381.poll.response`).
    PollVote(PollVotePopup),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_focus_cycles() {
        let mut form = LoginForm::new(None);
        assert_eq!(form.homeserver, "matrix.org");
        form.previous();
        assert_eq!(form.focus, LoginForm::BROWSER_BUTTON);
        assert!(form.field_mut().is_none());
        form.next();
        form.next();
        form.field_mut().unwrap().insert("a");
        assert_eq!(form.username, "a");
    }

    #[test]
    fn reauth_form_keeps_identity_fixed_and_redacts_secrets() {
        let pw_target = ReauthTarget {
            homeserver: "https://example.org".into(),
            user_id: "@alice:example.org".into(),
            device_id: "DEV1".into(),
            kind: ReauthKind::Password,
            client_id: None,
        };
        let mut pw = LoginForm::reauth(pw_target, "Session expired".into());
        assert!(pw.is_reauth());
        assert!(pw.allows_password());
        assert!(!pw.allows_browser());
        assert_eq!(pw.focus, 2);
        // Fixed slots (Homeserver = 0, Account = 1) and Browser button = 4 cannot be focused.
        pw.focus_slot(0);
        assert_eq!(pw.focus, 2);
        pw.focus_slot(1);
        assert_eq!(pw.focus, 2);
        pw.focus_slot(LoginForm::BROWSER_BUTTON);
        assert_eq!(pw.focus, 2);
        // Typing edits only password, Tab cycles only Password <-> Log in button.
        pw.field_mut().unwrap().insert("secret_pw");
        assert!(!format!("{pw:?}").contains("secret_pw"));
        pw.next();
        assert_eq!(pw.focus, LoginForm::PASSWORD_BUTTON);
        assert!(pw.field_mut().is_none());
        pw.previous();
        assert_eq!(pw.focus, 2);
        assert_eq!(pw.homeserver, "https://example.org");
        assert_eq!(pw.username, "@alice:example.org");

        let browser_target = ReauthTarget {
            homeserver: "https://sso.example.org".into(),
            user_id: "@alice:sso.example.org".into(),
            device_id: "DEV2".into(),
            kind: ReauthKind::Browser,
            client_id: Some("cid-1".into()),
        };
        let mut br = LoginForm::reauth(browser_target, "Session expired".into());
        assert!(br.is_reauth());
        assert!(br.allows_browser());
        assert!(!br.allows_password());
        assert_eq!(br.focus, LoginForm::BROWSER_BUTTON);
        assert!(br.field_mut().is_none());
        br.focus_slot(2);
        br.next();
        assert_eq!(br.focus, LoginForm::BROWSER_BUTTON);
        br.previous();
        assert_eq!(br.focus, LoginForm::BROWSER_BUTTON);
    }

    #[test]
    fn debug_output_hides_secrets() {
        let mut form = LoginForm::new(None);
        form.password.set("hunter2");
        assert!(!format!("{form:?}").contains("hunter2"));
        assert!(!format!("{:?}", form.password).contains("hunter2"));
        let mut recovery = RecoveryForm::default();
        recovery.key.set("EsTc secret");
        assert!(!format!("{:?}", recovery.key).contains("secret"));
        assert!(!format!("{:?}", Modal::Recovery(recovery)).contains("secret"));
        let command = crate::matrix::MatrixCmd::LoginPassword {
            homeserver: "example.org".into(),
            username: "alice".into(),
            password: String::from("hunter2").into(),
        };
        assert!(!format!("{command:?}").contains("hunter2"));
    }

    #[test]
    fn picker_navigation_stays_in_bounds() {
        let mut picker = EmojiPicker::default();
        picker.step(-1);
        assert_eq!(picker.selected, 0);
        picker.step(-(picker.columns as isize));
        assert_eq!(picker.selected, 0);
        picker.step(picker.columns as isize);
        assert_eq!(picker.selected, picker.columns);
        picker.set_query("rat".into());
        assert_eq!(picker.selected, 0);
        assert_eq!(picker.selected().unwrap().as_str(), "🐀");
        picker.step(1_000_000);
        assert_eq!(picker.selected, 0);
        picker.set_query("zzzzqqqq".into());
        picker.step(1);
        assert!(picker.selected().is_none());
        picker.scroll_by(5);
        assert_eq!(picker.scroll, 0);
    }

    #[test]
    fn moderation_modals_redact_debug_and_navigate_cleanly() {
        let mut pinned =
            PinnedPopup::open("!room123:example.org".into(), "Secret Room".into(), true);
        pinned.set_items(
            vec![
                crate::model::PinnedMessage {
                    event_id: "$ev1:example.org".into(),
                    sender: "Alice".into(),
                    sender_id: Some("@alice:example.org".into()),
                    day: "Today".into(),
                    time: "12:00".into(),
                    snippet: "classified launch codes".into(),
                    redacted: false,
                    undecryptable: false,
                    ignored: false,
                },
                crate::model::PinnedMessage {
                    event_id: "$ev2:example.org".into(),
                    sender: "Bob".into(),
                    sender_id: Some("@bob:example.org".into()),
                    day: "Today".into(),
                    time: "12:05".into(),
                    snippet: "secret follow-up".into(),
                    redacted: true,
                    undecryptable: false,
                    ignored: false,
                },
            ],
            None,
        );
        assert_eq!(pinned.selected_item().unwrap().event_id, "$ev1:example.org");
        pinned.select_last();
        assert_eq!(pinned.selected_item().unwrap().event_id, "$ev2:example.org");
        pinned.clamp_scroll(1);
        assert!(pinned.scroll <= 1);
        pinned.select_first();
        assert_eq!(pinned.selected, 0);
        pinned.remove_event("$ev1:example.org");
        assert_eq!(pinned.items.len(), 1);
        assert_eq!(pinned.selected_item().unwrap().event_id, "$ev2:example.org");

        let mut redact = ReasonConfirmPopup::redact(
            "!room123:example.org".into(),
            crate::model::ItemRef::Event("$ev1:example.org".into()),
            "Alice".into(),
            "classified launch codes".into(),
        );
        redact.reason.set("top secret reason");
        assert!(redact.on_reason());
        redact.step(1);
        assert!(redact.on_submit());
        redact.flip_button();
        assert!(redact.on_cancel());
        redact.focus_reason();
        assert!(redact.on_reason());

        let mut ban = ReasonConfirmPopup::ban(
            "!room123:example.org".into(),
            "@bob:example.org".into(),
            "Bob".into(),
        );
        ban.reason.set("private ban reason");

        let debug = format!("{pinned:?} {:?} {redact:?} {ban:?}", pinned.items[0]);
        for secret in [
            "Secret",
            "$ev1",
            "$ev2",
            "Alice",
            "alice",
            "Bob",
            "bob",
            "classified",
            "launch",
            "follow-up",
            "top secret",
            "private ban",
        ] {
            assert!(!debug.contains(secret), "{secret} leaked in {debug}");
        }
    }
}
