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

#[derive(Debug, Clone)]
pub struct EmojiPicker {
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
    pub fn set_query(&mut self, query: String) {
        self.results = emoji::search(&query);
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

#[derive(Debug, Clone)]
pub enum Modal {
    Login(LoginForm),
    Recovery(RecoveryForm),
    Emoji(EmojiPicker),
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
