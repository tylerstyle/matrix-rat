//! State of the popups that take over input: login, recovery key, emoji picker.

use emojis::Emoji;

use crate::emoji;
use crate::lineedit::LineEdit;
use crate::matrix::{CustomEmoji, EmoteCatalog, ReauthKind, ReauthTarget, StickerItem};

#[derive(Clone, PartialEq, Eq)]
pub struct ReauthInfo {
    pub user_id: String,
    pub device_id: String,
    pub kind: ReauthKind,
}

/// Hand-written: whose account and which device stay out of logs.
impl std::fmt::Debug for ReauthInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReauthInfo")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
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

/// Columns an emoji takes in the picker: two for the glyph, one of spacing.
pub const EMOJI_CELL: u16 = 3;
/// Columns a custom emoji takes in the picker: its picture (where there is one) and its
/// name, by which it is known in every terminal.
pub const CUSTOM_CELL: u16 = 14;

/// One result of the emoji picker.
// Tests compare it with `assert_eq!`: there it shows everything, for a failure to be read.
#[cfg_attr(test, derive(Debug))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Unicode(&'static Emoji),
    /// Index into [`EmojiPicker::custom`].
    Custom(usize),
}

/// Outside tests: which emoji was picked stays out of logs, like a reaction's key.
#[cfg(not(test))]
impl std::fmt::Debug for Pick {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unicode(_) => f.write_str("Pick::Unicode"),
            Self::Custom(index) => write!(f, "Pick::Custom({index})"),
        }
    }
}

/// A result of the emoji picker as what it is rather than where it is among the
/// results, which change when packs arrive.
#[derive(Clone, PartialEq, Eq)]
enum Chosen {
    Unicode(&'static Emoji),
    Custom { shortcode: String, url: String },
}

/// Results of one kind in a row of their own: the picker's grid is made of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    /// Index of its first result, and how many it has.
    start: usize,
    len: usize,
    /// Cells per row, and their width.
    columns: usize,
    cell: u16,
    first_row: usize,
}

#[derive(Clone)]
pub struct EmojiPicker {
    /// Index and id of the message to react to; `None` inserts into the draft.
    pub react_to: Option<(usize, String)>,
    pub query: LineEdit,
    /// The custom emoji of the image packs, as they were when it was opened or since.
    pub custom: Vec<CustomEmoji>,
    pub results: Vec<Pick>,
    pub selected: usize,
    /// First visible row of the grid.
    pub scroll: usize,
    /// Grid size as of the last draw; arrow keys need it. `columns` counts emoji,
    /// `custom_columns` custom emoji, which take wider cells.
    pub columns: usize,
    pub custom_columns: usize,
    pub rows: usize,
    /// Ask the next draw to bring the selection into view.
    pub reveal: bool,
    /// The grid column up and down aim for: where they started from, kept across
    /// shorter rows and rows of wider cells as a text editor keeps the cursor's column
    /// across shorter lines. Forgotten when the selection moves any other way.
    pub goal: Option<u16>,
}

/// Hand-written: the id of the message reacted to, what was typed and the custom
/// emoji's names and addresses stay out of logs.
impl std::fmt::Debug for EmojiPicker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmojiPicker")
            .field("reaction", &self.react_to.is_some())
            .field("query_bytes", &self.query.len())
            .field("custom", &self.custom.len())
            .field("results", &self.results.len())
            .field("selected", &self.selected)
            .field("scroll", &self.scroll)
            .finish_non_exhaustive()
    }
}

impl Default for EmojiPicker {
    fn default() -> Self {
        Self {
            react_to: None,
            query: LineEdit::new(),
            custom: Vec::new(),
            results: emoji::search("").into_iter().map(Pick::Unicode).collect(),
            selected: 0,
            scroll: 0,
            columns: 12,
            custom_columns: 3,
            rows: 8,
            reveal: true,
            goal: None,
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

    /// Offers the custom emoji of `catalog` too. Only filters again when they changed.
    /// Packs that arrive while the picker is open leave the selection on the emoji it
    /// was on where that one is still offered, and the view where it was.
    pub fn set_custom(&mut self, catalog: &EmoteCatalog) {
        let custom: Vec<CustomEmoji> = catalog.emojis().cloned().collect();
        if custom == self.custom {
            return;
        }
        let chosen = self.selected().and_then(|pick| self.chosen(pick));
        let (selected, scroll, goal) = (self.selected, self.scroll, self.goal);
        self.custom = custom;
        self.refilter();
        let found = chosen.and_then(|chosen| {
            self.results
                .iter()
                .position(|&pick| self.chosen(pick).as_ref() == Some(&chosen))
        });
        // One that is gone leaves the selection where it was, as far as the results go.
        self.selected = found.unwrap_or(selected.min(self.results.len().saturating_sub(1)));
        self.scroll = scroll;
        self.goal = goal;
        self.reveal = true;
    }

    /// A result as what it is rather than where: for finding it again among new
    /// results.
    fn chosen(&self, pick: Pick) -> Option<Chosen> {
        match pick {
            Pick::Unicode(emoji) => Some(Chosen::Unicode(emoji)),
            Pick::Custom(at) => self.custom.get(at).map(|custom| Chosen::Custom {
                shortcode: custom.shortcode.clone(),
                url: custom.url.clone(),
            }),
        }
    }

    #[cfg(test)]
    pub fn set_query(&mut self, query: String) {
        self.query.set(query);
        self.refilter();
    }

    /// The results for the query: custom emoji first (their names match what was typed
    /// best), then the others. Before anything is typed for a reaction, the quick
    /// reactions come first of all.
    pub fn refilter(&mut self) {
        let unicode = emoji::search(&self.query);
        let custom = emoji::search_custom(&self.query, &self.custom);
        let mut results = Vec::with_capacity(unicode.len() + custom.len());
        let mut rest = unicode;
        if self.query.is_empty() && self.react_to.is_some() {
            let quick: Vec<_> = QUICK_REACTIONS
                .iter()
                .filter_map(|e| emojis::get(e))
                .collect();
            rest.retain(|e| !quick.contains(e));
            results.extend(quick.into_iter().map(Pick::Unicode));
        }
        results.extend(custom.into_iter().map(Pick::Custom));
        results.extend(rest.into_iter().map(Pick::Unicode));
        self.results = results;
        self.selected = 0;
        self.scroll = 0;
        self.goal = None;
    }

    pub fn selected(&self) -> Option<Pick> {
        self.results.get(self.selected).copied()
    }

    /// The custom emoji a result stands for, if it is one.
    pub fn custom_at(&self, index: usize) -> Option<&CustomEmoji> {
        match self.results.get(index)? {
            Pick::Custom(at) => self.custom.get(*at),
            Pick::Unicode(_) => None,
        }
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
        self.goal = None;
    }

    /// Moves the selection `delta` rows up or down, to what is under the column it
    /// started from (see [`EmojiPicker::goal`]), or to the last one of a shorter row.
    /// It stops at the first and the last row.
    pub fn step_rows(&mut self, delta: isize) {
        let runs = self.runs();
        let Some((row, x, _)) = place(&runs, self.selected) else {
            return;
        };
        let target = (row as isize + delta).clamp(0, row_count(&runs) as isize - 1);
        let goal = *self.goal.get_or_insert(x);
        let items = row_items(&runs, target as usize);
        let under = items
            .clone()
            .find(|&index| place(&runs, index).is_some_and(|(_, left, cell)| goal < left + cell));
        if let Some(index) = under.or(items.end.checked_sub(1)) {
            self.selected = index;
            self.reveal = true;
        }
    }

    pub fn scroll_by(&mut self, rows: isize) {
        let total = row_count(&self.runs());
        let max = total.saturating_sub(self.rows) as isize;
        self.scroll = (self.scroll as isize + rows).clamp(0, max.max(0)) as usize;
    }

    /// How many rows the grid has.
    pub fn grid_rows(&self) -> usize {
        row_count(&self.runs())
    }

    /// Row, first column and width of a result's cell in the grid.
    pub fn cell_of(&self, index: usize) -> Option<(usize, u16, u16)> {
        place(&self.runs(), index)
    }

    /// The results on rows `rows` of the grid, with their row, column and width.
    pub fn cells(&self, rows: std::ops::Range<usize>) -> Vec<(usize, usize, u16, u16)> {
        let runs = self.runs();
        let mut cells = Vec::new();
        for row in rows.start..rows.end.min(row_count(&runs)) {
            for index in row_items(&runs, row) {
                if let Some((row, x, width)) = place(&runs, index) {
                    cells.push((index, row, x, width));
                }
            }
        }
        cells
    }

    fn runs(&self) -> Vec<Run> {
        let mut runs: Vec<Run> = Vec::new();
        let mut row = 0;
        for (index, pick) in self.results.iter().enumerate() {
            let custom = matches!(pick, Pick::Custom(_));
            let same = runs
                .last()
                .is_some_and(|run| (run.cell == CUSTOM_CELL) == custom);
            if let Some(run) = runs.last_mut().filter(|_| same) {
                run.len += 1;
                continue;
            }
            if let Some(run) = runs.last() {
                row += run.len.div_ceil(run.columns);
            }
            let (columns, cell) = if custom {
                (self.custom_columns.max(1), CUSTOM_CELL)
            } else {
                (self.columns.max(1), EMOJI_CELL)
            };
            runs.push(Run {
                start: index,
                len: 1,
                columns,
                cell,
                first_row: row,
            });
        }
        runs
    }
}

fn row_count(runs: &[Run]) -> usize {
    runs.last()
        .map_or(0, |run| run.first_row + run.len.div_ceil(run.columns))
}

fn place(runs: &[Run], index: usize) -> Option<(usize, u16, u16)> {
    let run = runs
        .iter()
        .find(|run| (run.start..run.start + run.len).contains(&index))?;
    let at = index - run.start;
    let column = (at % run.columns) as u16;
    Some((
        run.first_row + at / run.columns,
        column * run.cell,
        run.cell,
    ))
}

fn row_items(runs: &[Run], row: usize) -> std::ops::Range<usize> {
    let Some(run) = runs
        .iter()
        .rfind(|run| run.first_row <= row && row < run.first_row + run.len.div_ceil(run.columns))
    else {
        return 0..0;
    };
    let start = run.start + (row - run.first_row) * run.columns;
    start..(start + run.columns).min(run.start + run.len)
}

/// The rows of the settings menu, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Theme,
    SidebarWidth,
    RoomPreviews,
    /// Previews of the first link of a message, asked of the homeserver.
    LinkPreviews,
    Mouse,
    TypingNotices,
    ReadReceipts,
    /// No notifications for now.
    DoNotDisturb,
    Notifications,
    /// Words that make a message notify like a mention (push rules on the server).
    Keywords,
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
    pub const ALL: [Setting; 22] = [
        Setting::Theme,
        Setting::ThemePairing,
        Setting::ThemeDark,
        Setting::ThemeLight,
        Setting::SidebarWidth,
        Setting::RoomPreviews,
        Setting::LinkPreviews,
        Setting::Mouse,
        Setting::TypingNotices,
        Setting::ReadReceipts,
        Setting::ReadMarkers,
        Setting::Notifications,
        Setting::DoNotDisturb,
        Setting::Keywords,
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
            Setting::LinkPreviews => "Link previews",
            Setting::Mouse => "Mouse",
            Setting::TypingNotices => "Typing notices",
            Setting::ReadReceipts => "Read receipts",
            Setting::ReadMarkers => "Read markers",
            Setting::Notifications => "Notifications",
            Setting::DoNotDisturb => "Do not disturb",
            Setting::Keywords => "Notification keywords",
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
            Setting::LinkPreviews => {
                "Under a message, what its first link is about. Your homeserver fetches the link, so it learns it; in encrypted rooms only with \"all rooms\"."
            }
            Setting::Mouse => "Off: the terminal's own text selection works instead of clicking.",
            Setting::TypingNotices => "Show others when you are typing.",
            Setting::ReadReceipts => "Show others how far you have read.",
            Setting::ReadMarkers => "Show under messages who has read up to there.",
            Setting::Notifications => {
                "Tell you about new messages while you are in another room or window."
            }
            Setting::DoNotDisturb => {
                "No notifications for now. Unread marks and counts go on; the status line shows that it is on."
            }
            Setting::Keywords => {
                "Words that make a message notify you like a mention. Your server keeps them, so your other clients use them too."
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

/// Hand-written: room, event and user ids, names, and reasons stay out of logs.
impl std::fmt::Debug for ReportForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReportForm")
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

/// Hand-written: which room, its name and the pinned messages stay out of logs.
impl std::fmt::Debug for PinnedPopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedPopup")
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

/// How far the server's list of a room's threads has come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadsLoad {
    /// A page is on its way.
    Loading,
    /// There is more; `next` says where it continues.
    More,
    /// All of it is here.
    Done,
    /// The server has no such list or could not give it: only the threads in the
    /// room's loaded messages are known.
    Failed,
}

/// Pop-up listing the threads of a room, newest activity first.
#[derive(Clone)]
pub struct ThreadsPopup {
    pub room_id: String,
    pub room_name: String,
    /// Tags the requests of this listing; answers to an earlier one are dropped.
    pub generation: u64,
    pub filter: LineEdit,
    /// Only threads the user started or answered in.
    pub mine: bool,
    /// From the server, in its order, each thread once.
    pub listed: Vec<crate::model::ThreadInfo>,
    /// Known from the room's loaded messages, newest first.
    pub local: Vec<crate::model::ThreadInfo>,
    /// Where the server's list continues.
    pub next: Option<String>,
    /// Where the page on its way was asked from: a server that answers with the same
    /// place again has no more.
    pub asked: Option<String>,
    pub load: ThreadsLoad,
    /// What the list shows: what passes the filter.
    pub shown: Vec<crate::model::ThreadInfo>,
    /// The selected row; the row after the threads is the one that loads more.
    pub selected: usize,
    /// First thread on screen.
    pub scroll: usize,
    /// Threads that fit on screen as of the last draw; PageUp/PageDown move by it.
    pub page: usize,
    pub reveal: bool,
}

/// Hand-written: the room, names, text and the server's token stay out of logs.
impl std::fmt::Debug for ThreadsPopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadsPopup")
            .field("generation", &self.generation)
            .field("mine", &self.mine)
            .field("listed", &self.listed.len())
            .field("local", &self.local.len())
            .field("shown", &self.shown.len())
            .field("load", &self.load)
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

impl ThreadsPopup {
    /// The most threads one listing holds, however many pages the server offers.
    pub const MAX_THREADS: usize = 500;
    pub const FILTER_MAX_BYTES: usize = 256;

    /// `loading`: the server's list is asked for; without, the threads of the
    /// loaded messages are all there is (the demo).
    pub fn new(
        room_id: String,
        room_name: String,
        generation: u64,
        local: Vec<crate::model::ThreadInfo>,
        loading: bool,
    ) -> Self {
        let mut popup = Self {
            room_id,
            room_name,
            generation,
            filter: LineEdit::new().with_max_bytes(Self::FILTER_MAX_BYTES),
            mine: false,
            listed: Vec::new(),
            local,
            next: None,
            asked: None,
            load: if loading {
                ThreadsLoad::Loading
            } else {
                ThreadsLoad::Done
            },
            shown: Vec::new(),
            selected: 0,
            scroll: 0,
            page: 0,
            reveal: true,
        };
        popup.refresh();
        popup
    }

    /// Starts the listing over (for "only mine" switched on or off), as `generation`.
    pub fn restart(&mut self, generation: u64, loading: bool) {
        self.generation = generation;
        self.listed.clear();
        self.next = None;
        self.asked = None;
        self.load = if loading {
            ThreadsLoad::Loading
        } else {
            ThreadsLoad::Done
        };
        self.selected = 0;
        self.scroll = 0;
        self.refresh();
    }

    /// Whether a row after the threads loads more of them (or says that more is
    /// on its way).
    pub fn more_row(&self) -> bool {
        match self.load {
            ThreadsLoad::More => true,
            ThreadsLoad::Loading => !self.listed.is_empty(),
            ThreadsLoad::Done | ThreadsLoad::Failed => false,
        }
    }

    pub fn rows(&self) -> usize {
        self.shown.len() + usize::from(self.more_row())
    }

    pub fn thread_at(&self, row: usize) -> Option<&crate::model::ThreadInfo> {
        self.shown.get(row)
    }

    /// Takes the next page of the server's list. Threads it has listed already are
    /// not listed twice.
    pub fn add_page(&mut self, threads: Vec<crate::model::ThreadInfo>, next: Option<String>) {
        for mut thread in threads {
            if self.listed.len() >= Self::MAX_THREADS {
                break;
            }
            if self.listed.iter().any(|known| known.root == thread.root) {
                continue;
            }
            // The room's loaded messages follow the thread as it changes.
            if let Some(known) = self.local.iter().find(|known| known.root == thread.root) {
                thread.unread = known.unread;
            }
            self.listed.push(thread);
        }
        let again = next.is_some() && next == self.asked;
        self.next = next.filter(|_| !again && self.listed.len() < Self::MAX_THREADS);
        self.asked = None;
        self.load = if self.next.is_some() {
            ThreadsLoad::More
        } else {
            ThreadsLoad::Done
        };
        self.refresh();
    }

    pub fn failed(&mut self) {
        self.next = None;
        self.asked = None;
        self.load = ThreadsLoad::Failed;
        self.refresh();
    }

    /// Works out what the list shows, keeping the selection on its thread. Threads
    /// of the loaded messages that the server did not list are added once its list
    /// is complete, or when it has none.
    pub fn refresh(&mut self) {
        let keep = self.thread_at(self.selected).map(|t| t.root.clone());
        let words: Vec<String> = self
            .filter
            .as_str()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let matches = |thread: &crate::model::ThreadInfo| {
            let hay = format!("{} {}", thread.sender, thread.text).to_lowercase();
            words.iter().all(|word| hay.contains(word.as_str()))
        };
        let complete = matches!(self.load, ThreadsLoad::Done | ThreadsLoad::Failed);
        let extra = self.local.iter().filter(|thread| {
            complete
                && (!self.mine || thread.mine)
                && !self.listed.iter().any(|known| known.root == thread.root)
        });
        self.shown = self
            .listed
            .iter()
            .chain(extra)
            .filter(|thread| matches(thread))
            .cloned()
            .collect();
        // On the row that loads more, the selection lands on the first new thread.
        self.selected = keep
            .and_then(|root| self.shown.iter().position(|t| t.root == root))
            .unwrap_or_else(|| self.selected.min(self.rows().saturating_sub(1)));
        self.reveal = true;
    }

    pub fn step(&mut self, delta: isize) {
        let rows = self.rows();
        if rows == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, rows as isize - 1) as usize;
        self.reveal = true;
    }

    pub fn scroll_by(&mut self, lines: isize) {
        self.scroll = (self.scroll as isize + lines).max(0) as usize;
        self.reveal = false;
    }

    /// The selection has come near the end of what is loaded, and the server has more.
    pub fn wants_more(&self) -> bool {
        self.load == ThreadsLoad::More && self.selected + 3 >= self.rows()
    }

    /// The end of what is loaded is on screen, and the server has more.
    pub fn shows_end(&self) -> bool {
        self.load == ThreadsLoad::More && self.scroll + self.page.max(1) >= self.shown.len()
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
    /// Remove a member from the room (the `/kick` command).
    Kick {
        room_id: String,
        user_id: String,
        user_name: String,
    },
    /// Let a banned user back in (the `/unban` command).
    Unban {
        room_id: String,
        user_id: String,
        user_name: String,
    },
}

/// Hand-written: which room, which message and whom stay out of logs.
impl std::fmt::Debug for ReasonConfirmKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Redact { .. } => "ReasonConfirmKind::Redact",
            Self::Ban { .. } => "ReasonConfirmKind::Ban",
            Self::Kick { .. } => "ReasonConfirmKind::Kick",
            Self::Unban { .. } => "ReasonConfirmKind::Unban",
        })
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

    pub fn kick(room_id: String, user_id: String, user_name: String) -> Self {
        Self {
            kind: ReasonConfirmKind::Kick {
                room_id,
                user_id,
                user_name,
            },
            reason: LineEdit::new().with_max_bytes(Self::REASON_MAX_BYTES),
            focus: Self::REASON,
        }
    }

    pub fn unban(room_id: String, user_id: String, user_name: String) -> Self {
        Self {
            kind: ReasonConfirmKind::Unban {
                room_id,
                user_id,
                user_name,
            },
            reason: LineEdit::new().with_max_bytes(Self::REASON_MAX_BYTES),
            focus: Self::REASON,
        }
    }

    /// The same question with `reason` already filled in (typed after a command).
    pub fn with_reason(mut self, reason: &str) -> Self {
        self.reason.set(reason);
        self
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
/// A question or an answer may be this long; a paste is cut there.
pub const POLL_TEXT_MAX_BYTES: usize = 500;

fn poll_field() -> LineEdit {
    LineEdit::new().with_max_bytes(POLL_TEXT_MAX_BYTES)
}

/// A poll form's question, answers, whether votes are shown while it is open, and
/// how many answers each voter may pick.
pub type ValidPoll = (String, Vec<String>, bool, u32);

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
        let mut question = poll_field();
        let initial = crate::text::sanitize_line(initial_question.trim());
        let focus = if initial.is_empty() { 0 } else { 1 };
        question.set(initial);
        Self {
            room_id,
            editing: None,
            question,
            answers: vec![poll_field(), poll_field()],
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
        let mut question = poll_field();
        question.set(poll.question.clone());
        let mut answers: Vec<LineEdit> = poll
            .answers
            .iter()
            .take(MAX_POLL_ANSWERS)
            .map(|a| {
                let mut edit = poll_field();
                edit.set(a.text.clone());
                edit
            })
            .collect();
        while answers.len() < MIN_POLL_ANSWERS {
            answers.push(poll_field());
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
        self.answers.push(poll_field());
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

    pub fn validated(&self) -> Result<ValidPoll, (usize, String)> {
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

/// Pop-up listing the stickers of the account's and the room's image packs by name.
#[derive(Clone)]
pub struct StickerPicker {
    pub room_id: String,
    pub filter: LineEdit,
    pub all: Vec<StickerItem>,
    /// Indices into `all` that match the filter.
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub scroll: usize,
    /// List rows as of the last draw; PageUp/PageDown move by it.
    pub page: usize,
    pub reveal: bool,
    /// The packs have not arrived yet.
    pub loading: bool,
}

/// Hand-written: the room id, sticker names and addresses stay out of logs.
impl std::fmt::Debug for StickerPicker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StickerPicker")
            .field("count", &self.all.len())
            .field("shown", &self.filtered.len())
            .field("selected", &self.selected)
            .field("loading", &self.loading)
            .finish_non_exhaustive()
    }
}

impl StickerPicker {
    pub fn new(room_id: String, filter: &str, catalog: &EmoteCatalog, loading: bool) -> Self {
        let mut picker = Self {
            room_id,
            filter: LineEdit::new().with_max_bytes(256),
            all: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            scroll: 0,
            page: 0,
            reveal: true,
            loading,
        };
        picker.filter.set(filter.to_owned());
        picker.set_catalog(catalog, loading);
        picker
    }

    /// Takes newly arrived packs, keeping the selected sticker where it still exists.
    pub fn set_catalog(&mut self, catalog: &EmoteCatalog, loading: bool) {
        let keep = self.selected_sticker().map(|s| s.url.clone());
        self.all = catalog.stickers().into_iter().cloned().collect();
        self.loading = loading;
        self.refilter_keeping(keep.as_deref());
    }

    pub fn refilter(&mut self) {
        self.refilter_keeping(None);
        self.scroll = 0;
    }

    /// A sticker matches when every word of the filter is in its name, shortcode or
    /// pack name.
    fn refilter_keeping(&mut self, keep: Option<&str>) {
        let words: Vec<String> = self
            .filter
            .as_str()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        self.filtered = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, sticker)| {
                let hay = format!(
                    "{} {} {}",
                    sticker.name.to_lowercase(),
                    sticker.shortcode.to_lowercase(),
                    sticker.pack_name.to_lowercase()
                );
                words.iter().all(|w| hay.contains(w.as_str()))
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = keep
            .and_then(|url| {
                self.filtered
                    .iter()
                    .position(|&i| self.all.get(i).is_some_and(|s| s.url == url))
            })
            .unwrap_or(0);
        self.reveal = true;
    }

    pub fn selected_sticker(&self) -> Option<&StickerItem> {
        self.sticker_at(self.selected)
    }

    /// The sticker on row `index` of the filtered list.
    pub fn sticker_at(&self, index: usize) -> Option<&StickerItem> {
        self.filtered.get(index).and_then(|&i| self.all.get(i))
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

#[derive(Clone)]
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
    /// Pick a sticker from the image packs and send it.
    Stickers(StickerPicker),
    /// Who sent a message and when, its event ID, encryption, delivery, readers and
    /// source.
    Details(Box<crate::details::DetailsPopup>),
    /// The threads of a room.
    Threads(ThreadsPopup),
    /// The account's notification keywords.
    Keywords(crate::keywords::KeywordsPopup),
}

/// Hand-written, so that a new kind of popup is added here by hand: each one holds
/// something private (what was typed, names, ids, messages), and its own `Debug` must
/// keep that out of logs before it can be shown here.
impl std::fmt::Debug for Modal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (name, popup): (&str, &dyn std::fmt::Debug) = match self {
            Self::Login(popup) => ("Login", popup),
            Self::Recovery(popup) => ("Recovery", popup),
            Self::Emoji(popup) => ("Emoji", popup),
            Self::Settings(popup) => ("Settings", popup),
            Self::Verify(popup) => ("Verify", popup),
            Self::Search(popup) => ("Search", popup),
            Self::Palette(popup) => ("Palette", popup),
            Self::GlobalSearch(popup) => ("GlobalSearch", popup),
            Self::New(popup) => ("New", popup),
            Self::RoomInfo(popup) => ("RoomInfo", popup),
            Self::UserInfo(popup) => ("UserInfo", popup),
            Self::OwnProfile(popup) => ("OwnProfile", popup),
            Self::Menu(popup) => ("Menu", popup),
            Self::Keybinds(popup) => ("Keybinds", popup),
            Self::IgnoredUsers(popup) => ("IgnoredUsers", popup),
            Self::Report(popup) => ("Report", popup),
            Self::Pinned(popup) => ("Pinned", popup),
            Self::ReasonConfirm(popup) => ("ReasonConfirm", popup),
            Self::Sessions(popup) => ("Sessions", popup),
            Self::Keys(popup) => ("Keys", popup),
            Self::PollForm(popup) => ("PollForm", popup),
            Self::PollVote(popup) => ("PollVote", popup),
            Self::Stickers(popup) => ("Stickers", popup),
            Self::Details(popup) => ("Details", popup),
            Self::Threads(popup) => ("Threads", popup),
            Self::Keywords(popup) => ("Keywords", popup),
        };
        f.debug_tuple(name).field(popup).finish()
    }
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

    /// Every kind of popup, each holding recognisable private things (typed text, ids,
    /// names, paths, messages): none of them shows in `Debug`.
    #[test]
    fn no_popup_shows_what_it_holds_in_debug() {
        use std::path::Path;

        use crate::model::{Message, PollAnswer, PollInfo, RoomKind, ThreadInfo};

        const PRIVATE: &[&str] = &[
            "secret", "Secret", "alice", "Alice", "hunter2", "DEVHUSH", "/home/", "mxc:",
        ];
        let typed = |text: &str| LineEdit::from(text);
        let secret = |text: &str| {
            let mut field = LineEdit::secret();
            field.set(text);
            field
        };

        let mut login = LoginForm::reauth(
            ReauthTarget {
                homeserver: "https://secret.example.org".into(),
                user_id: "@alice:secret.example.org".into(),
                device_id: "DEVHUSH".into(),
                kind: ReauthKind::Password,
                client_id: None,
            },
            "The session of @alice:secret.example.org expired".into(),
        );
        login.password = secret("hunter2");
        login.error = Some("secret.example.org said no to @alice".into());
        login.browser_url = Some("https://secret.example.org/sso?alice".into());

        let recovery = RecoveryForm {
            key: secret("EsTc secret key"),
            error: Some("the key of alice is wrong".into()),
            ..RecoveryForm::default()
        };

        let mut emoji = EmojiPicker::for_reaction(0, "$secret-event:example.org".into());
        emoji.set_query("secret query".into());

        let settings = SettingsMenu {
            editing: Some(typed("/home/alice/secret-downloads")),
            ..SettingsMenu::default()
        };

        let mut verify = crate::verify::VerifyPopup::set_up();
        verify.password = Some(secret("hunter2"));

        let mut search = crate::app::search::SearchBar::default();
        search.query = typed("secret words");

        let mut palette = crate::palette::Palette::open_pick(
            &[crate::palette::PickItem {
                id: "!secret-room:example.org".into(),
                name: "Secret Room".into(),
                hint: Some("alice's".into()),
                category: crate::palette::Category::Room,
            }],
            crate::palette::Pick::IntoSpace {
                space_id: "!secret-space:example.org".into(),
                space_name: "Secret Space".into(),
                back: None,
            },
        );
        palette.query = typed("secret");

        let mut global = crate::app::global_search::GlobalSearch::new(1);
        global.query = typed("secret words");
        global.active_query = "secret words".into();
        global.note = Some("nothing for secret words".into());

        let mut new = crate::manage::NewPopup::new(crate::manage::Stage::Invite);
        new.user = typed("@alice:secret.example.org");
        new.address = typed("#secret:example.org");
        new.room = Some(("!secret-room:example.org".into(), "Secret Room".into()));
        new.error = Some("secret.example.org refused".into());

        let info = crate::manage::InfoPopup::new(
            "!secret-room:example.org",
            "Secret Room",
            RoomKind::Room,
        );

        let user = crate::people::UserInfoPopup::new(
            "@alice:secret.example.org".into(),
            Some("!secret-room:example.org".into()),
            Some("Alice Secret".into()),
            None,
            false,
            0,
            1,
            1,
            true,
        );

        let profile = crate::matrix::OwnProfile {
            display_name: Some("Alice Secret".into()),
            avatar: Some("mxc://secret.example.org/face".into()),
            room_names: [("!secret-room:example.org".into(), Some("Alice".into()))].into(),
        };
        let own = crate::people::OwnProfilePopup::new(
            Some("@alice:secret.example.org".into()),
            Some(&profile),
        );

        let menu = crate::menu::Menu::new(
            crate::menu::Target::Room {
                id: "!secret-room:example.org".into(),
                kind: RoomKind::Room,
            },
            "Secret Room",
            vec![crate::menu::Item {
                label: "Alice Secret".into(),
                hint: "12:00".into(),
                run: None,
                confirm: None,
            }],
            crate::menu::Anchor::at(0, 0),
        )
        .unwrap();

        let mut keybinds = crate::keymap::Editor::new(&crate::keymap::Keymap::default());
        keybinds.feedback = Some((
            "Could not save /home/alice-secret/.config/matrix-rat/config.toml".into(),
            true,
        ));

        let ignored = IgnoredUsersPopup::new(&["@alice:secret.example.org".to_owned()]);

        let mut report = ReportForm::new(
            "!secret-room:example.org".into(),
            "$secret-event:example.org".into(),
            Some("@alice:secret.example.org".into()),
            "Alice Secret".into(),
            false,
        );
        report.reason.set("a secret reason");

        let pinned = PinnedPopup::open(
            "!secret-room:example.org".into(),
            "Secret Room".into(),
            true,
        );

        let mut ban = ReasonConfirmPopup::ban(
            "!secret-room:example.org".into(),
            "@alice:secret.example.org".into(),
            "Alice Secret".into(),
        );
        ban.reason.set("a secret reason");

        let sessions = crate::devices::SessionsPopup {
            stage: crate::devices::SessionStage::Rename(crate::devices::Rename {
                device_id: "DEVHUSH".into(),
                name: typed("alice's secret laptop"),
                request: None,
                error: Some("secret.example.org said no".into()),
            }),
            selected: Some("DEVHUSH".into()),
            ..crate::devices::SessionsPopup::default()
        };

        let mut export = crate::devices::ExportForm::new(Path::new("/home/alice/secret-keys.txt"));
        export.passphrase = secret("hunter2");
        let keys = crate::devices::KeysPopup {
            stage: crate::devices::KeysStage::Export(export),
            focus: None,
        };

        let poll_form =
            PollFormPopup::create("!secret-room:example.org".into(), "Secret question?");

        let poll = PollInfo {
            question: "Secret question?".into(),
            max_selections: 1,
            answers: vec![PollAnswer {
                id: "secret-answer".into(),
                text: "Secret answer".into(),
                votes: 1,
                own: true,
            }],
            ..PollInfo::default()
        };
        let poll_vote = PollVotePopup::from_poll(
            "!secret-room:example.org".into(),
            "$secret-poll:example.org".into(),
            &poll,
        );

        let catalog = EmoteCatalog {
            packs: vec![crate::matrix::EmotePack {
                id: "!secret-room:example.org".into(),
                name: "Secret Pack".into(),
                emojis: Vec::new(),
                stickers: vec![StickerItem {
                    shortcode: "secretcat".into(),
                    name: "Secret Cat".into(),
                    url: "mxc://secret.example.org/cat".into(),
                    pack_name: "Secret Pack".into(),
                    dimensions: None,
                    size: None,
                    mime: None,
                }],
            }],
        };
        let stickers =
            StickerPicker::new("!secret-room:example.org".into(), "secret", &catalog, false);

        let message = Message {
            id: "secret-id".into(),
            item: Some(crate::model::ItemRef::Event(
                "$secret-event:example.org".into(),
            )),
            sender: "Alice Secret".into(),
            sender_id: Some("@alice:secret.example.org".into()),
            body: "the secret plans".into(),
            ..Message::default()
        };
        let details = crate::details::DetailsPopup::new(&message, true, true, 1);

        let threads = ThreadsPopup::new(
            "!secret-room:example.org".into(),
            "Secret Room".into(),
            1,
            vec![ThreadInfo {
                root: "$secret-root:example.org".into(),
                sender: "Alice Secret".into(),
                text: "a secret thread".into(),
                replies: 2,
                latest_sender: Some("Alice".into()),
                latest_when: Some("12:00".into()),
                unread: true,
                mine: false,
            }],
            false,
        );

        let mut keywords =
            crate::keywords::KeywordsPopup::new(Some((vec!["secretword".into()], 1)));
        keywords.input.set("another secret");

        let popups = [
            Modal::Login(login),
            Modal::Recovery(recovery),
            Modal::Emoji(emoji),
            Modal::Settings(settings),
            Modal::Verify(verify),
            Modal::Search(search),
            Modal::Palette(palette),
            Modal::GlobalSearch(global),
            Modal::New(Box::new(new)),
            Modal::RoomInfo(Box::new(info)),
            Modal::UserInfo(Box::new(user)),
            Modal::OwnProfile(own),
            Modal::Menu(menu),
            Modal::Keybinds(keybinds),
            Modal::IgnoredUsers(ignored),
            Modal::Report(report),
            Modal::Pinned(pinned),
            Modal::ReasonConfirm(ban),
            Modal::Sessions(Box::new(sessions)),
            Modal::Keys(Box::new(keys)),
            Modal::PollForm(poll_form),
            Modal::PollVote(poll_vote),
            Modal::Stickers(stickers),
            Modal::Details(Box::new(details)),
            Modal::Threads(threads),
            Modal::Keywords(keywords),
        ];
        for popup in &popups {
            let shown = format!("{popup:?}");
            for private in PRIVATE {
                assert!(!shown.contains(private), "{private} in {shown}");
            }
        }
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
        assert_eq!(
            picker.selected(),
            Some(Pick::Unicode(emojis::get("🐀").unwrap()))
        );
        picker.step(1_000_000);
        assert_eq!(picker.selected, 0);
        picker.set_query("zzzzqqqq".into());
        picker.step(1);
        assert!(picker.selected().is_none());
        picker.scroll_by(5);
        assert_eq!(picker.scroll, 0);
    }

    #[test]
    fn the_picker_offers_custom_emoji_by_name_in_rows_of_their_own() {
        let emoji = |code: &str| CustomEmoji {
            shortcode: code.into(),
            url: format!("mxc://example.org/{code}"),
            body: None,
            pack_name: "Blobs".into(),
        };
        let catalog = EmoteCatalog {
            packs: vec![crate::matrix::EmotePack {
                id: "user".into(),
                name: "Blobs".into(),
                emojis: ["blob", "blobcat", "partyblob", "blobwave"]
                    .map(emoji)
                    .to_vec(),
                stickers: Vec::new(),
            }],
        };
        let mut picker = EmojiPicker::for_reaction(0, "$m".into());
        picker.set_custom(&catalog);
        // The quick reactions, the custom emoji, then everything else.
        assert!(matches!(picker.results[9], Pick::Unicode(_)));
        let custom: Vec<&str> = (10..14)
            .map(|at| picker.custom_at(at).unwrap().shortcode.as_str())
            .collect();
        assert_eq!(custom, ["blob", "blobcat", "partyblob", "blobwave"]);
        assert!(matches!(picker.results[14], Pick::Unicode(_)));

        // Twelve emoji or three custom ones to a row; each kind starts a row.
        assert_eq!(picker.cell_of(9), Some((0, 27, EMOJI_CELL)));
        assert_eq!(picker.cell_of(10), Some((1, 0, CUSTOM_CELL)));
        assert_eq!(picker.cell_of(12), Some((1, 28, CUSTOM_CELL)));
        assert_eq!(picker.cell_of(13), Some((2, 0, CUSTOM_CELL)));
        assert_eq!(picker.cell_of(14), Some((3, 0, EMOJI_CELL)));
        let cells = picker.cells(1..3);
        assert_eq!(
            cells.iter().map(|c| c.0).collect::<Vec<_>>(),
            [10, 11, 12, 13]
        );

        // Up and down go to what is under the selection, or to the last of a
        // shorter row; past the top it stays.
        picker.selected = 9;
        picker.step_rows(1);
        assert_eq!(picker.selected, 11);
        picker.step_rows(1);
        assert_eq!(picker.selected, 13);
        picker.step_rows(-1);
        assert_eq!(picker.selected, 11);
        picker.step_rows(-1);
        assert_eq!(picker.selected, 9);
        picker.step_rows(-1);
        assert_eq!(picker.selected, 9);

        // Typed: the custom emoji named so come first, best match first.
        picker.set_query("blob".into());
        let found: Vec<&str> = (0..4)
            .map(|at| picker.custom_at(at).unwrap().shortcode.as_str())
            .collect();
        assert_eq!(found, ["blob", "blobcat", "blobwave", "partyblob"]);
        // The same packs again change nothing; new ones are offered at once.
        picker.selected = 2;
        picker.set_custom(&catalog);
        assert_eq!(picker.selected, 2);
        picker.set_custom(&EmoteCatalog::default());
        assert!(picker.custom.is_empty());
        assert!(picker.results.iter().all(|p| matches!(p, Pick::Unicode(_))));
        // Nothing of them in a log, nor the message reacted to, nor what was typed.
        picker.set_custom(&catalog);
        picker.set_query("partyb".into());
        let debug = format!("{picker:?} {:?}", Modal::Emoji(picker.clone()));
        for secret in ["example.org", "blob", "Blobs", "$m", "partyb"] {
            assert!(!debug.contains(secret), "{secret} leaked in {debug}");
        }
    }

    #[test]
    fn packs_arriving_while_the_picker_is_open_leave_the_selection_on_its_emoji() {
        let emoji = |code: &str| CustomEmoji {
            shortcode: code.into(),
            url: format!("mxc://example.org/{code}"),
            body: None,
            pack_name: "Blobs".into(),
        };
        let pack = |codes: &[&str]| crate::matrix::EmotePack {
            id: "user".into(),
            name: "Blobs".into(),
            emojis: codes.iter().map(|code| emoji(code)).collect(),
            stickers: Vec::new(),
        };
        // For the draft, where custom emoji come first: scrolled down to an emoji.
        let mut picker = EmojiPicker::default();
        picker.step_rows(5);
        picker.step(3);
        picker.scroll = 4;
        let before = picker.selected();
        picker.set_custom(&EmoteCatalog {
            packs: vec![pack(&["blob", "blobcat", "partyblob", "blobwave"])],
        });
        assert_eq!(picker.selected(), before);
        assert_eq!(picker.scroll, 4);

        // A custom emoji stays selected as another pack comes in before it.
        picker.selected = 1;
        let blobcat = picker.custom_at(1).unwrap().clone();
        picker.set_custom(&EmoteCatalog {
            packs: vec![
                pack(&["ratwave", "ratparty"]),
                pack(&["blob", "blobcat", "partyblob", "blobwave"]),
            ],
        });
        assert_eq!(picker.custom_at(picker.selected), Some(&blobcat));
        assert_eq!(picker.scroll, 4);
        // One that is gone leaves the selection where it was.
        picker.set_custom(&EmoteCatalog {
            packs: vec![pack(&["ratwave", "ratparty", "blob"])],
        });
        assert_eq!(picker.selected, 3);
    }

    #[test]
    fn up_and_down_keep_their_column_across_short_and_wide_rows_and_pages_stop_at_the_ends() {
        let emoji = |code: &str| CustomEmoji {
            shortcode: code.into(),
            url: format!("mxc://example.org/{code}"),
            body: None,
            pack_name: "Blobs".into(),
        };
        let catalog = EmoteCatalog {
            packs: vec![crate::matrix::EmotePack {
                id: "user".into(),
                name: "Blobs".into(),
                emojis: ["blob", "blobcat", "partyblob", "blobwave"]
                    .map(emoji)
                    .to_vec(),
                stickers: Vec::new(),
            }],
        };
        // Row 0: ten quick reactions; rows 1 and 2: three custom emoji and one; then
        // twelve emoji to a row from result 14.
        let mut picker = EmojiPicker::for_reaction(0, "$m".into());
        picker.set_custom(&catalog);
        let row_of = |picker: &EmojiPicker| picker.cell_of(picker.selected).unwrap().0;

        // From the tenth column down through the short row and back under it.
        picker.selected = 9;
        for expected in [11, 13, 14 + 9, 14 + 12 + 9] {
            picker.step_rows(1);
            assert_eq!(picker.selected, expected);
        }
        for expected in [14 + 9, 13, 11, 9] {
            picker.step_rows(-1);
            assert_eq!(picker.selected, expected);
        }
        // From a narrow cell into a wide one and back: the same narrow cell, not the
        // one at the wide cell's left edge.
        picker.selected = 14 + 2;
        picker.goal = None;
        picker.step_rows(-1);
        assert_eq!(picker.selected, 13);
        picker.step_rows(1);
        assert_eq!(picker.selected, 14 + 2);

        // After a step sideways, the column is where the selection is now.
        picker.step(1);
        picker.step_rows(-1);
        assert_eq!(picker.selected, 13);
        picker.step_rows(1);
        assert_eq!(picker.selected, 14 + 3);
        // After typing too.
        picker.set_query(String::new());
        picker.selected = 14 + 2;
        picker.step_rows(-1);
        picker.step_rows(1);
        assert_eq!(picker.selected, 14 + 2);

        // A page that would go past either end stops at the first or the last row.
        picker.selected = 14 + 12 + 3;
        picker.goal = None;
        picker.step_rows(-8);
        assert_eq!(picker.selected, 3);
        picker.step_rows(8);
        assert_eq!(row_of(&picker), 8);
        picker.step_rows(1_000);
        assert_eq!(row_of(&picker), picker.grid_rows() - 1);
        picker.step_rows(-1_000);
        assert_eq!(picker.selected, 3);
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
        let kick = ReasonConfirmPopup::kick(
            "!room123:example.org".into(),
            "@bob:example.org".into(),
            "Bob".into(),
        )
        .with_reason("private kick reason");
        let unban = ReasonConfirmPopup::unban(
            "!room123:example.org".into(),
            "@bob:example.org".into(),
            "Bob".into(),
        );

        let debug = format!(
            "{pinned:?} {:?} {redact:?} {ban:?} {kick:?} {unban:?}",
            pinned.items[0]
        );
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
            "private kick",
        ] {
            assert!(!debug.contains(secret), "{secret} leaked in {debug}");
        }
    }
}
