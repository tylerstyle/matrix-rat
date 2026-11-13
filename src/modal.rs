//! State of the popups that take over input: login, recovery key, emoji picker.

use emojis::Emoji;

use crate::emoji;

#[derive(Clone, Default)]
pub struct LoginForm {
    pub homeserver: String,
    pub username: String,
    pub password: String,
    /// 0–2 are the text fields, 3 and 4 the two buttons.
    pub focus: usize,
    /// A progress message while a login is running; input is locked meanwhile.
    pub busy: Option<String>,
    pub error: Option<String>,
    /// Shown during a browser login, in case the browser did not open by itself.
    pub browser_url: Option<String>,
}

impl LoginForm {
    pub const FIELDS: usize = 3;
    pub const PASSWORD_BUTTON: usize = 3;
    pub const BROWSER_BUTTON: usize = 4;
    const SLOTS: usize = 5;

    pub fn new(error: Option<String>) -> Self {
        Self {
            homeserver: "matrix.org".to_owned(),
            error,
            ..Self::default()
        }
    }

    pub fn field_mut(&mut self) -> Option<&mut String> {
        match self.focus {
            0 => Some(&mut self.homeserver),
            1 => Some(&mut self.username),
            2 => Some(&mut self.password),
            _ => None,
        }
    }

    pub fn next(&mut self) {
        self.focus = (self.focus + 1) % Self::SLOTS;
    }

    pub fn previous(&mut self) {
        self.focus = (self.focus + Self::SLOTS - 1) % Self::SLOTS;
    }
}

#[derive(Clone, Default)]
pub struct RecoveryForm {
    pub key: String,
    pub busy: bool,
    pub error: Option<String>,
}

// Hand-written so the password and the recovery key can never end up in a log.
impl std::fmt::Debug for LoginForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginForm")
            .field("homeserver", &self.homeserver)
            .field("username", &self.username)
            .field("focus", &self.focus)
            .field("busy", &self.busy)
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
    pub query: String,
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
            query: String::new(),
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
        picker.set_query(String::new());
        picker
    }

    pub fn set_query(&mut self, query: String) {
        self.results = emoji::search(&query);
        if query.is_empty() && self.react_to.is_some() {
            let quick: Vec<_> = QUICK_REACTIONS
                .iter()
                .filter_map(|e| emojis::get(e))
                .collect();
            self.results.retain(|e| !quick.contains(e));
            self.results.splice(0..0, quick);
        }
        self.query = query;
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
    Mouse,
    TypingNotices,
    ReadReceipts,
    DownloadDir,
    Keys,
    Encryption,
    Logout,
}

impl Setting {
    pub const ALL: [Setting; 9] = [
        Setting::Theme,
        Setting::SidebarWidth,
        Setting::Mouse,
        Setting::TypingNotices,
        Setting::ReadReceipts,
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
            Setting::SidebarWidth => "Room list width",
            Setting::Mouse => "Mouse",
            Setting::TypingNotices => "Typing notices",
            Setting::ReadReceipts => "Read receipts",
            Setting::DownloadDir => "Download folder",
            Setting::Keys => "Key bindings",
            Setting::Encryption => "Encryption",
            Setting::Logout => "Log out",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Setting::Theme => {
                "Themes in the themes folder are listed too and reload when they change."
            }
            Setting::SidebarWidth => "You can also drag the line next to the room list.",
            Setting::Mouse => "Off: the terminal's own text selection works instead of clicking.",
            Setting::TypingNotices => "Show others when you are typing.",
            Setting::ReadReceipts => "Show others how far you have read.",
            Setting::DownloadDir => {
                "Where s saves files and pictures. Enter to change; empty for the default."
            }
            Setting::Keys => "Show all key bindings (F1).",
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
    pub editing: Option<String>,
}

impl SettingsMenu {
    pub fn current(&self) -> Setting {
        Setting::ALL[self.selected.min(Setting::ALL.len() - 1)]
    }

    pub fn step(&mut self, delta: isize) {
        let count = Setting::ALL.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(count) as usize;
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
    Palette(crate::palette::Palette),
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
        form.field_mut().unwrap().push('a');
        assert_eq!(form.username, "a");
    }

    #[test]
    fn debug_output_hides_secrets() {
        let mut form = LoginForm::new(None);
        form.password = "hunter2".into();
        assert!(!format!("{form:?}").contains("hunter2"));
        let recovery = RecoveryForm {
            key: "EsTc secret".into(),
            ..RecoveryForm::default()
        };
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
