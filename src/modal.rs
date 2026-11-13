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
    Logout,
    /// Whether the messages say who has read them (shown next to `ReadReceipts`).
    ReadMarkers,
    /// Theme used when the terminal is dark.
    ThemeDark,
    /// Theme used when the terminal is light.
    ThemeLight,
    /// Whether separate dark and light theme slots are enabled and shown.
    ThemePairing,
}

impl Setting {
    pub const ALL: [Setting; 15] = [
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
        Setting::Encryption,
        Setting::Logout,
    ];

    /// The heading shown above this row, if it starts a group.
    pub fn heading(self) -> Option<&'static str> {
        match self {
            Setting::Theme => Some("Appearance"),
            Setting::Mouse => Some("Behaviour"),
            Setting::Keys => Some("Help"),
            Setting::Encryption => Some("Account"),
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
            Setting::Encryption => "Encryption",
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
            Setting::Encryption => {
                "Verify this device with another of your devices, or see its status."
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

#[derive(Debug, Clone)]
pub enum Modal {
    Login(LoginForm),
    Recovery(RecoveryForm),
    Emoji(EmojiPicker),
    Settings(SettingsMenu),
    Verify(crate::verify::VerifyPopup),
    Search(crate::app::search::SearchBar),
    Palette(crate::palette::Palette),
    /// Join a room, start a direct chat, create a room, or invite someone.
    New(crate::manage::NewPopup),
    RoomInfo(crate::manage::InfoPopup),
    /// The context menu of a message, a room, a link or the composer.
    Menu(crate::menu::Menu),
    /// Configure key bindings and preset.
    Keybinds(crate::keymap::Editor),
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
            password: "hunter2".into(),
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
}
