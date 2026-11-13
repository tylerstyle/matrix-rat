//! Application state, input handling and the event loop.

mod attach;
mod complete;
mod files;
mod keybinds;
mod media;
mod menu;
mod palette;
mod rooms;
pub mod search;
mod thread;
mod verify;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::MissedTickBehavior;

use crate::action::{Action, Dir, Pane};
use crate::composer::Composer;
use crate::config::Config;
use crate::format::Outgoing;
use crate::keymap::{self, Context, Keymap};
use crate::matrix::{
    self, MatrixCmd, MatrixEvent, MatrixHandle, RecoveryStatus, RoomCategory, RoomSummary,
    SyncStatus,
};
use crate::modal::{EmojiPicker, LoginForm, Modal, RecoveryForm, Setting, SettingsMenu};
use crate::model::{
    ItemRef, Message, MsgKind, OWN_NAME, Reaction, Room, RoomKind, SendState, demo_rooms,
};
use crate::paths::Paths;
use crate::terminal::{self, TerminalGuard};
use crate::terminput::{TermEvents, TermInput};
use crate::theme::{ColorScheme, DEFAULT_THEME, Theme, truecolor_supported};
use crate::ui::hit::{Button, HitId, HitMap};
use crate::ui::{self, SIDEBAR_MIN, max_sidebar};

const TOAST_TIME: Duration = Duration::from_secs(3);
/// How long a count-based notification waits for a detailed one to take its place,
/// and for how long a detailed one stands for its room.
const COUNT_NOTICE_WAIT: Duration = Duration::from_millis(1500);
const DETAILED_COVERS: Duration = Duration::from_secs(10);
/// Notification limits: per room, and in all within a window.
const NOTICE_PER_ROOM: Duration = Duration::from_secs(5);
const NOTICE_WINDOW: Duration = Duration::from_secs(30);
const NOTICE_MOST: usize = 6;
/// Unread counts that rise right after login are the backlog arriving, not news.
const SESSION_SETTLE: Duration = Duration::from_secs(20);
const WHEEL_LINES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeMode {
    Normal,
    /// Replying to the message at this index.
    Reply(usize),
    /// Editing the message at this index.
    Edit(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideRow {
    Section(RoomKind),
    /// Index into `App::rooms`.
    Room(usize),
    /// The second line of a room: its latest message.
    Preview(usize),
}

#[derive(Debug, Default)]
pub struct SidebarState {
    /// Position of the keyboard cursor among the visible rooms.
    pub cursor: usize,
    pub filter: String,
    pub width: u16,
    pub scroll: usize,
    pub reveal_cursor: bool,
}

#[derive(Debug, Default)]
pub struct TimelineState {
    pub selected: Option<usize>,
    /// Rows scrolled up from the newest message.
    pub scroll: usize,
    /// Ask the next draw to bring the selected message into view.
    pub reveal_selected: bool,
    /// Visible rows, as of the last draw.
    pub page: usize,
    /// Set by the draw when the oldest loaded message is on screen.
    pub want_older: bool,
    pub loading_older: bool,
    /// There is no older history to load.
    pub reached_start: bool,
    /// The user scrolled towards older messages since the last request.
    pub older_armed: bool,
    /// The rows do not fill the view (as of the last draw).
    pub view_full: bool,
    /// Message count when the running request for history was made.
    pub count_at_request: usize,
    /// Consecutive history requests that added nothing visible.
    pub empty_batches: u8,
    /// The messages were drawn in the last frame.
    pub drawn: bool,
    /// Event id of the newest message a read receipt was sent for.
    pub marked_read: Option<String>,
}

/// Where the composer's text was drawn, for mapping clicks to a cursor position.
#[derive(Debug, Default, Clone, Copy)]
pub struct ComposerView {
    pub area: Rect,
    pub width: usize,
    pub first_row: usize,
    pub cursor: Option<(u16, u16)>,
}

pub struct ReadyNotices {
    pub items: Vec<(crate::matrix::Notice, crate::notify::NotifyIcon)>,
    pub mode: crate::notify::NotifyMode,
    pub preview: bool,
    pub icon_cache: Option<(Arc<crate::notify::IconCache>, u64)>,
}

#[derive(Debug)]
pub struct App {
    pub rooms: Vec<Room>,
    pub active: usize,
    pub focus: Pane,
    pub sidebar: SidebarState,
    pub timeline: TimelineState,
    pub composer: Composer,
    pub compose_mode: ComposeMode,
    pub composer_view: ComposerView,
    pub help_open: bool,
    /// Lines the help is scrolled down by, where it does not fit the window.
    pub help_scroll: usize,
    /// A popup that takes all input while it is open.
    pub modal: Option<Modal>,
    /// `None` in demo mode.
    matrix: Option<MatrixHandle>,
    pub sync: Option<SyncStatus>,
    pub user_id: Option<String>,
    recovery_prompted: bool,
    /// Counts timeline requests; see `request_timeline`.
    generation: u64,
    /// The open room was picked by the app, not by the user.
    auto_opened: bool,
    /// False while the terminal window is in the background.
    window_focused: bool,
    /// The draft that was in the composer when an edit started.
    edit_stash: Option<String>,
    /// When we last told the room we are typing, if we did.
    typing_sent: Option<Instant>,
    pub typing_notices: bool,
    pub read_receipts: bool,
    /// Show who has read up to where (the `read_markers` setting).
    pub read_markers: bool,
    /// Whether clicks go to the app; the event loop applies changes to the terminal.
    pub mouse: bool,
    /// Text on its way to the system clipboard; the event loop sends it.
    pub pending_copy: Option<String>,
    /// How notifications are shown, and the mode to return to when switched back on.
    pub notify_mode: crate::notify::NotifyMode,
    notify_mode_on: crate::notify::NotifyMode,
    notification_preview: bool,
    /// Notifications made from a risen unread count, waiting to see whether a
    /// detailed one arrives for the same room; and when each room last got one.
    count_notices: Vec<(Instant, crate::matrix::Notice)>,
    detailed_notices: std::collections::HashMap<String, Instant>,
    /// Two lines per room in the room list, the second with the latest message.
    pub room_previews: bool,
    /// The user's spaces, and the one the room list is narrowed to (by id).
    pub spaces: Vec<crate::matrix::SpaceSummary>,
    pub space: Option<String>,
    /// Its position in `spaces`.
    space_at: Option<usize>,
    /// Rooms of the selected space, sub-spaces included.
    space_rooms: std::collections::HashSet<String>,
    /// When and for which room notifications were shown lately, to limit bursts.
    shown_notices: Vec<(Instant, String)>,
    /// Ask the event loop to draw the whole screen afresh instead of only what
    /// changed, after the content of a pane was swapped wholesale.
    pub repaint: bool,
    /// Between a logout (or expiry) and the next login.
    logged_out: bool,
    /// When this session's login was confirmed.
    session_started: Option<Instant>,
    /// Notifications on their way out; the event loop shows them.
    pub pending_notices: Vec<crate::matrix::Notice>,
    /// Ephemeral PNG cache for desktop notification avatars, wiped on logout/exit.
    notify_icons: Option<Arc<crate::notify::IconCache>>,
    /// The `download_dir` setting; empty means the desktop's download folder.
    download_dir: String,
    /// Attachments being downloaded, and where finished ones went (by `mxc://` address).
    saving: std::collections::HashSet<String>,
    saved: std::collections::HashMap<String, PathBuf>,
    /// A large attachment the user was asked about once.
    confirm_large: Option<String>,
    /// Current terminal colour scheme (`Dark` or `Light`), detected at startup and
    /// updated live via DEC mode 2031 / OSC 11.
    pub color_scheme: ColorScheme,
    /// Base `theme` setting from config (used when unpaired, or as fallback for an
    /// unset slot).
    base_theme: String,
    /// Paired theme overrides for dark and light modes, if configured.
    theme_dark: Option<String>,
    theme_light: Option<String>,
    /// The theme the user asked for, which may not exist yet (a generator may still
    /// have to write it).
    wanted_theme: String,
    /// Where the configuration lives, to notice a changed `theme` setting.
    config_file: Option<PathBuf>,
    /// The `theme`, `theme_dark`, and `theme_light` settings as last read from the
    /// configuration.
    config_theme: String,
    config_theme_dark: Option<String>,
    config_theme_light: Option<String>,
    /// Bumped whenever a theme is loaded.
    pub theme_rev: u64,
    /// Bumped whenever the open room's messages change, to invalidate `row_cache`.
    pub timeline_rev: u64,
    pub row_cache: Option<crate::ui::RowCache>,
    /// Encryption status and device verification.
    pub verify: crate::verify::VerifyState,
    pub theme: Theme,
    theme_names: Vec<String>,
    themes_dir: PathBuf,
    truecolor: bool,
    pub hits: HitMap,
    pub hover: Option<HitId>,
    dragging_splitter: bool,
    pub toast: Option<(String, Instant)>,
    pub screen: Rect,
    pub should_quit: bool,
    pub suspend_requested: bool,
    pub dirty: bool,
    /// Pictures: the cache and the viewer.
    pub images: crate::images::ImageStore,
    /// The thread shown in place of the room's messages, and thread drafts.
    pub threads: thread::Threads,
    /// Files waiting in the composer to be sent.
    pub attach: attach::AttachState,
    /// Composer completions and tracked mentions/room links.
    pub complete: complete::CompleteState,
    /// Joining, creating and leaving rooms.
    manage: rooms::ManageState,
    /// Active key bindings and preset.
    pub keymap: Keymap,
}

impl App {
    /// With `matrix` the app shows the account's rooms; without, built-in demo data.
    #[cfg(test)]
    pub fn new(config: &Config, themes_dir: PathBuf, matrix: Option<MatrixHandle>) -> Self {
        Self::new_in_scheme(config, themes_dir, matrix, ColorScheme::Dark)
    }

    pub(crate) fn new_in_scheme(
        config: &Config,
        themes_dir: PathBuf,
        matrix: Option<MatrixHandle>,
        color_scheme: ColorScheme,
    ) -> Self {
        let truecolor = truecolor_supported();
        let mut app = Self::with_theme(Theme::builtin(DEFAULT_THEME).expect("default theme"));
        if matrix.is_some() {
            app.rooms.clear();
            app.sync = Some(SyncStatus::Connecting);
        }
        app.matrix = matrix;
        app.truecolor = truecolor;
        app.theme_names = Theme::available(&themes_dir);
        app.themes_dir = themes_dir;
        app.sidebar.width = config.sidebar_width;
        app.typing_notices = config.typing_notices;
        app.read_receipts = config.read_receipts;
        app.read_markers = config.read_markers;
        app.mouse = config.mouse;
        app.download_dir = config.download_dir.clone();
        app.room_previews = config.room_previews;
        app.notify_mode = config.notifications;
        if config.notifications != crate::notify::NotifyMode::Off {
            app.notify_mode_on = config.notifications;
        }
        app.notification_preview = config.notification_preview;
        app.color_scheme = color_scheme;
        app.base_theme = config.theme.clone();
        let (dark, light) = config.paired_themes();
        app.theme_dark = dark.clone();
        app.theme_light = light.clone();
        app.config_theme = config.theme.clone();
        app.config_theme_dark = dark;
        app.config_theme_light = light;
        let initial = app.desired_theme();
        app.set_theme(&initial);
        let (keymap, warnings) = Keymap::from_config(config);
        app.keymap = keymap;
        for warning in &warnings {
            tracing::warn!(%warning, "invalid key binding in config");
        }
        if !warnings.is_empty() {
            app.show_toast(format!(
                "Key bindings: {} problem(s), see the log",
                warnings.len()
            ));
        }
        app
    }

    /// Demo data with fixed settings; also what the tests render.
    #[cfg(test)]
    pub fn demo() -> Self {
        let mut app = Self::with_theme(Theme::builtin(DEFAULT_THEME).expect("default theme"));
        // Not the machine's own folder, so screens look the same everywhere.
        app.download_dir = "~/Downloads".to_owned();
        app
    }

    fn with_theme(theme: Theme) -> Self {
        Self {
            rooms: demo_rooms(),
            active: 0,
            focus: Pane::Composer,
            sidebar: SidebarState {
                width: Config::default().sidebar_width,
                ..SidebarState::default()
            },
            timeline: TimelineState::default(),
            composer: Composer::default(),
            compose_mode: ComposeMode::Normal,
            composer_view: ComposerView {
                width: 80,
                ..ComposerView::default()
            },
            help_open: false,
            help_scroll: 0,
            modal: None,
            matrix: None,
            sync: None,
            user_id: None,
            recovery_prompted: false,
            generation: 0,
            auto_opened: true,
            window_focused: true,
            edit_stash: None,
            typing_sent: None,
            typing_notices: true,
            read_receipts: true,
            read_markers: true,
            mouse: true,
            pending_copy: None,
            notify_mode: crate::notify::NotifyMode::Off,
            notify_mode_on: crate::notify::NotifyMode::Auto,
            notification_preview: true,
            count_notices: Vec::new(),
            detailed_notices: Default::default(),
            room_previews: false,
            spaces: Vec::new(),
            space: None,
            space_at: None,
            space_rooms: Default::default(),
            shown_notices: Vec::new(),
            repaint: false,
            logged_out: false,
            session_started: None,
            pending_notices: Vec::new(),
            notify_icons: None,
            download_dir: String::new(),
            saving: Default::default(),
            saved: Default::default(),
            confirm_large: None,
            color_scheme: ColorScheme::Dark,
            base_theme: DEFAULT_THEME.to_owned(),
            theme_dark: None,
            theme_light: None,
            wanted_theme: DEFAULT_THEME.to_owned(),
            config_file: None,
            config_theme: String::new(),
            config_theme_dark: None,
            config_theme_light: None,
            theme_rev: 0,
            timeline_rev: 0,
            row_cache: None,
            verify: Default::default(),
            theme,
            theme_names: Theme::builtin_names(),
            themes_dir: PathBuf::new(),
            truecolor: true,
            hits: HitMap::default(),
            hover: None,
            dragging_splitter: false,
            toast: None,
            screen: Rect::default(),
            should_quit: false,
            suspend_requested: false,
            dirty: true,
            images: crate::images::ImageStore::new(crate::images::Graphics::halfblocks()),
            threads: Default::default(),
            attach: Default::default(),
            complete: Default::default(),
            manage: Default::default(),
            keymap: Keymap::default(),
        }
    }

    // ----- derived views -------------------------------------------------------------

    /// Rooms that pass the filter, in the order the sidebar shows them.
    pub fn visible_rooms(&self) -> Vec<usize> {
        let needle = self.sidebar.filter.to_lowercase();
        RoomKind::SECTIONS
            .iter()
            .flat_map(|kind| {
                self.rooms
                    .iter()
                    .enumerate()
                    .filter(|(_, room)| room.kind == *kind)
                    // Invitations are shown in every space: they belong to none yet.
                    .filter(|(_, room)| {
                        self.space.is_none()
                            || room.kind == RoomKind::Invite
                            || self.space_rooms.contains(&room.id)
                    })
                    .filter(|(_, room)| room.name.to_lowercase().contains(&needle))
                    .map(|(i, _)| i)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub fn sidebar_rows(&self) -> Vec<SideRow> {
        let mut rows = Vec::new();
        let mut section = None;
        for index in self.visible_rooms() {
            let kind = self.rooms[index].kind;
            if section != Some(kind) {
                section = Some(kind);
                rows.push(SideRow::Section(kind));
            }
            rows.push(SideRow::Room(index));
            if self.room_previews {
                rows.push(SideRow::Preview(index));
            }
        }
        rows
    }

    /// The room under the sidebar's keyboard cursor.
    pub fn cursor_room(&self) -> Option<usize> {
        let visible = self.visible_rooms();
        visible
            .get(self.sidebar.cursor.min(visible.len().saturating_sub(1)))
            .copied()
    }

    /// The open room. `None` when there are no rooms (before the first sync).
    pub fn room(&self) -> Option<&Room> {
        self.rooms.get(self.active)
    }

    fn messages(&self) -> &[Message] {
        self.room().map_or(&[], |room| room.messages.as_slice())
    }

    fn messages_mut(&mut self) -> Option<&mut Vec<Message>> {
        self.timeline_rev += 1;
        self.rooms
            .get_mut(self.active)
            .map(|room| &mut room.messages)
    }

    // ----- state changes -------------------------------------------------------------

    pub fn show_toast(&mut self, text: impl Into<String>) {
        // Toasts often quote what a server or another person said.
        let text = crate::text::sanitize_line(&text.into());
        self.toast = Some((text, Instant::now()));
    }

    /// Called when something in the config or themes directory changed (or on
    /// request): follows a changed `theme` setting and loads the wanted theme again.
    /// Quiet unless a theme file is broken: a desktop recolouring itself should not
    /// pop up messages, and a theme that a generator has yet to write is just awaited.
    pub fn reload_theme(&mut self) {
        self.dirty = true;
        self.theme_names = Theme::available(&self.themes_dir);
        if let Some(file) = &self.config_file
            && let Ok(config) = Config::load(file)
        {
            let (cfg_dark, cfg_light) = config.paired_themes();
            if config.theme != self.config_theme
                || cfg_dark != self.config_theme_dark
                || cfg_light != self.config_theme_light
            {
                self.base_theme = config.theme.clone();
                self.theme_dark = cfg_dark.clone();
                self.theme_light = cfg_light.clone();
                self.config_theme = config.theme;
                self.config_theme_dark = cfg_dark;
                self.config_theme_light = cfg_light;
                self.wanted_theme = self.desired_theme();
            }
        }
        let name = self.wanted_theme.clone();
        let exists = Theme::builtin(&name).is_some()
            || self.themes_dir.join(format!("{name}.toml")).exists();
        if exists {
            self.set_theme(&name);
        }
    }

    fn has_theme_pair(&self) -> bool {
        self.theme_dark.is_some() || self.theme_light.is_some()
    }

    fn desired_theme(&self) -> String {
        let paired = match self.color_scheme {
            ColorScheme::Dark => &self.theme_dark,
            ColorScheme::Light => &self.theme_light,
        };
        paired
            .as_deref()
            .unwrap_or(self.base_theme.as_str())
            .to_owned()
    }

    /// Switches to the theme paired with `scheme` if a dark/light pair is configured.
    pub fn on_color_scheme(&mut self, scheme: ColorScheme) {
        if self.color_scheme == scheme {
            return;
        }
        self.color_scheme = scheme;
        let next = self.desired_theme();
        if next != self.wanted_theme {
            self.set_theme(&next);
            self.dirty = true;
        }
    }

    fn set_theme(&mut self, name: &str) {
        self.wanted_theme = name.to_owned();
        match Theme::load(name, &self.themes_dir) {
            Ok(mut theme) => {
                for warning in &theme.warnings {
                    tracing::warn!(theme = name, "{warning}");
                }
                if !theme.warnings.is_empty() {
                    self.show_toast(format!(
                        "Theme {name}: {} problem(s), see the log",
                        theme.warnings.len()
                    ));
                }
                if !self.truecolor {
                    theme.downgrade_to_256();
                }
                self.theme = theme;
                self.theme_rev += 1;
            }
            Err(err) => {
                tracing::warn!(theme = name, ?err, "could not load theme");
                self.show_toast(format!("Could not load theme {name}: {err:#}"));
            }
        }
    }

    /// Moves to the next or previous theme and remembers the choice.
    fn step_theme(&mut self, delta: isize) {
        // Look again: a generator may have written a new theme since.
        self.theme_names = Theme::available(&self.themes_dir);
        let count = self.theme_names.len() as isize;
        let at = self
            .theme_names
            .iter()
            .position(|n| *n == self.wanted_theme)
            .map_or(0, |i| (i as isize + delta).rem_euclid(count) as usize);
        let name = self.theme_names[at].clone();
        self.apply_theme(name);
    }

    /// Cycles `theme_dark` or `theme_light` through `(unset)` and available themes.
    fn step_slot_theme(&mut self, slot: ColorScheme, delta: isize) {
        self.theme_names = Theme::available(&self.themes_dir);
        let current = match slot {
            ColorScheme::Dark => self.theme_dark.as_deref(),
            ColorScheme::Light => self.theme_light.as_deref(),
        };
        let cur = match current {
            None => 0usize,
            Some(name) => self
                .theme_names
                .iter()
                .position(|n| n == name)
                .map_or(0, |i| i + 1),
        };
        let count = (self.theme_names.len() + 1) as isize;
        let at = (cur as isize + delta).rem_euclid(count) as usize;
        let next = if at == 0 {
            None
        } else {
            Some(self.theme_names[at - 1].clone())
        };
        let key = match slot {
            ColorScheme::Dark => {
                self.theme_dark = next.clone();
                self.config_theme_dark = next.clone();
                "theme_dark"
            }
            ColorScheme::Light => {
                self.theme_light = next.clone();
                self.config_theme_light = next.clone();
                "theme_light"
            }
        };
        match next {
            Some(name) => self.save_setting(key, name),
            None => self.remove_setting(key),
        }
        let desired = self.desired_theme();
        if desired != self.wanted_theme {
            self.set_theme(&desired);
        }
    }

    /// Loads `name`, remembers it, and saves it to the configuration (to the active
    /// dark/light slot when paired, otherwise to `theme`).
    fn apply_theme(&mut self, name: String) {
        self.set_theme(&name);
        if self.has_theme_pair() {
            match self.color_scheme {
                ColorScheme::Dark => {
                    self.theme_dark = Some(name.clone());
                    self.config_theme_dark = Some(name.clone());
                    self.save_setting("theme_dark", name);
                }
                ColorScheme::Light => {
                    self.theme_light = Some(name.clone());
                    self.config_theme_light = Some(name.clone());
                    self.save_setting("theme_light", name);
                }
            }
        } else {
            self.base_theme = name.clone();
            self.config_theme = name.clone();
            self.save_setting("theme", name);
        }
    }

    /// Writes one setting to the configuration file (there is none in demo mode).
    fn save_setting(&mut self, key: &str, value: impl Into<toml_edit::Value>) {
        let Some(file) = &self.config_file else {
            return;
        };
        if let Err(err) = crate::config::save_setting(file, key, value) {
            tracing::warn!(?err, key, "could not save setting");
            self.show_toast(format!("Could not save the setting: {err:#}"));
        }
    }

    /// Removes one setting from the configuration file (there is none in demo mode).
    fn remove_setting(&mut self, key: &str) {
        let Some(file) = &self.config_file else {
            return;
        };
        if let Err(err) = crate::config::remove_setting(file, key) {
            tracing::warn!(?err, key, "could not remove setting");
            self.show_toast(format!("Could not save the setting: {err:#}"));
        }
    }

    /// What the settings menu shows next to a row.
    pub fn setting_value(&self, setting: Setting) -> String {
        let switch = |on: bool| if on { "on" } else { "off" }.to_owned();
        match setting {
            Setting::Theme => self.wanted_theme.clone(),
            Setting::ThemeDark => self.theme_dark.as_deref().unwrap_or("(unset)").to_owned(),
            Setting::ThemeLight => self.theme_light.as_deref().unwrap_or("(unset)").to_owned(),
            Setting::SidebarWidth => self.sidebar.width.to_string(),
            Setting::RoomPreviews => switch(self.room_previews),
            Setting::Mouse => switch(self.mouse),
            Setting::TypingNotices => switch(self.typing_notices),
            Setting::ReadReceipts => switch(self.read_receipts),
            Setting::ReadMarkers => switch(self.read_markers),
            Setting::Notifications => switch(self.notify_mode != crate::notify::NotifyMode::Off),
            Setting::DownloadDir => crate::files::display(&self.download_dir()),
            Setting::Keys => self.keymap.preset_summary(),
            Setting::Encryption => crate::verify::setting_text(self.verify.security).to_owned(),
            Setting::Logout => self.user_id.clone().unwrap_or_default(),
        }
    }

    /// Changes a setting from the menu: `delta` is -1 or 1 for ◂ ▸, 0 for Enter or a
    /// click on the row.
    fn change_setting(&mut self, setting: Setting, delta: i8) {
        match setting {
            Setting::Theme => self.step_theme(if delta < 0 { -1 } else { 1 }),
            Setting::ThemeDark => {
                self.step_slot_theme(ColorScheme::Dark, if delta < 0 { -1 } else { 1 });
            }
            Setting::ThemeLight => {
                self.step_slot_theme(ColorScheme::Light, if delta < 0 { -1 } else { 1 });
            }
            Setting::SidebarWidth => {
                if delta != 0 {
                    let width = self.sidebar.width.saturating_add_signed(2 * delta as i16);
                    self.sidebar.width = width.clamp(SIDEBAR_MIN, max_sidebar(self.screen.width));
                    self.save_setting("sidebar_width", self.sidebar.width as i64);
                }
            }
            Setting::Mouse => {
                self.mouse = !self.mouse;
                if !self.mouse {
                    // No more pointer events will come to clear these.
                    self.hover = None;
                    self.dragging_splitter = false;
                }
                self.save_setting("mouse", self.mouse);
            }
            Setting::TypingNotices => {
                self.typing_notices = !self.typing_notices;
                self.save_setting("typing_notices", self.typing_notices);
                self.update_typing();
            }
            Setting::ReadReceipts => {
                self.read_receipts = !self.read_receipts;
                self.save_setting("read_receipts", self.read_receipts);
            }
            Setting::ReadMarkers => {
                self.read_markers = !self.read_markers;
                self.save_setting("read_markers", self.read_markers);
                // The rows are laid out afresh.
                self.timeline_rev += 1;
            }
            Setting::RoomPreviews => {
                self.room_previews = !self.room_previews;
                self.send_matrix(MatrixCmd::Previews(self.room_previews));
                self.sidebar.reveal_cursor = true;
                self.save_setting("room_previews", self.room_previews);
            }
            Setting::Notifications => {
                use crate::notify::NotifyMode;
                self.notify_mode = if self.notify_mode == NotifyMode::Off {
                    self.notify_mode_on
                } else {
                    NotifyMode::Off
                };
                self.save_setting("notifications", self.notify_mode.name());
            }
            Setting::DownloadDir => {
                if delta == 0
                    && let Some(Modal::Settings(menu)) = &mut self.modal
                {
                    menu.editing = Some(self.download_dir.clone());
                }
            }
            Setting::Keys => {
                if delta == 0 {
                    self.open_keybinds_editor();
                }
            }
            Setting::Encryption => {
                if delta == 0 {
                    self.modal = None;
                    self.start_verification();
                }
            }
            Setting::Logout => {
                if delta != 0 {
                    return;
                }
                if self.matrix.is_none() {
                    self.show_toast("The demo has no account to log out of");
                    return;
                }
                let Some(Modal::Settings(menu)) = &mut self.modal else {
                    return;
                };
                if menu.confirm_logout {
                    self.modal = None;
                    self.send_matrix(MatrixCmd::Logout);
                    // Nothing may be sent for the old session from here on.
                    self.reset_session();
                    self.show_toast("Logging out…");
                } else {
                    menu.confirm_logout = true;
                }
            }
        }
    }

    /// A message that deserves a notification arrived. It is shown unless the user is
    /// looking right at it: the window has the focus and that room is open.
    fn on_notice(&mut self, notice: crate::matrix::Notice) {
        // Watching means the message is on screen right now: this room and this
        // thread (or the room itself), at the newest message, nothing covering it.
        let watching = self.window_focused
            && self.modal.is_none()
            && !self.help_open
            && !self.images.viewer_open()
            && self.timeline.scroll == 0
            && self.room().is_some_and(|room| room.id == notice.room_id)
            && self.threads.open_root() == notice.thread.as_deref();
        if self.notify_mode == crate::notify::NotifyMode::Off || watching || self.logged_out {
            return;
        }
        // A burst (a busy room, catching up after sleep) must not flood the desktop:
        // one notification per room every few seconds, and a few per half minute in all.
        let now = Instant::now();
        self.shown_notices
            .retain(|(at, _)| now.duration_since(*at) < NOTICE_WINDOW);
        let same_room = self
            .shown_notices
            .iter()
            .any(|(at, room)| *room == notice.room_id && now.duration_since(*at) < NOTICE_PER_ROOM);
        if same_room || self.shown_notices.len() >= NOTICE_MOST {
            return;
        }
        self.shown_notices.push((now, notice.room_id.clone()));
        self.pending_notices.push(notice);
    }

    /// Shows the count-based notifications that have waited long enough, unless a
    /// detailed one covered the room in the meantime.
    fn release_count_notices(&mut self, wait: Duration) {
        if self.count_notices.is_empty() {
            return;
        }
        let (due, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.count_notices)
            .into_iter()
            .partition(|(since, _)| since.elapsed() >= wait);
        self.count_notices = waiting;
        self.detailed_notices
            .retain(|_, at| at.elapsed() < DETAILED_COVERS);
        for (_, notice) in due {
            let covered = self
                .detailed_notices
                .get(&notice.room_id)
                .is_some_and(|at| at.elapsed() < DETAILED_COVERS);
            if !covered {
                self.on_notice(notice);
            }
        }
    }

    /// Attaches the ephemeral desktop notification icon cache managed by the event loop.
    pub fn set_notify_icons(&mut self, cache: Arc<crate::notify::IconCache>) {
        self.notify_icons = Some(cache);
    }

    /// Hands the waiting notifications to the event loop (test helper).
    #[cfg(test)]
    pub fn take_notices(
        &mut self,
    ) -> (Vec<crate::matrix::Notice>, crate::notify::NotifyMode, bool) {
        (
            std::mem::take(&mut self.pending_notices),
            self.notify_mode,
            self.notification_preview,
        )
    }

    fn resolve_notice_icon(&mut self, notice: &crate::matrix::Notice) -> crate::notify::NotifyIcon {
        if !self.notification_preview
            || self.notify_mode.resolved() != crate::notify::NotifyMode::Desktop
        {
            return crate::notify::NotifyIcon::Generic;
        }
        let room = self.rooms.iter().find(|r| r.id == notice.room_id);
        // Never download media for unaccepted invitations.
        let is_invite = room.is_some_and(|r| r.kind == RoomKind::Invite);
        let avatar = if is_invite {
            None
        } else {
            room.and_then(|r| r.avatar.clone())
        };
        if let Some(url) = avatar
            && let Some(pixels) = self.images.request_avatar(&url)
        {
            return crate::notify::NotifyIcon::Picture { key: url, pixels };
        }
        let tile_name = room
            .map(|r| r.name.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&notice.room);
        crate::notify::tile_icon(&self.theme, tile_name)
    }

    /// Notifications ready for delivery together with their resolved desktop icons and
    /// icon cache epoch.
    pub fn take_notices_with_icons(&mut self) -> ReadyNotices {
        let waiting = std::mem::take(&mut self.pending_notices);
        let mut items = Vec::with_capacity(waiting.len());
        for notice in waiting {
            let icon = self.resolve_notice_icon(&notice);
            items.push((notice, icon));
        }
        self.send_avatar_requests();
        let icon_cache = self
            .notify_icons
            .as_ref()
            .map(|c| (Arc::clone(c), c.epoch()));
        ReadyNotices {
            items,
            mode: self.notify_mode,
            preview: self.notification_preview,
            icon_cache,
        }
    }

    /// Nothing of a previous session may linger behind the login form.
    pub(crate) fn reset_session(&mut self) {
        self.rooms.clear();
        self.active = 0;
        self.timeline = TimelineState::default();
        self.composer.take();
        self.compose_mode = ComposeMode::Normal;
        self.edit_stash = None;
        self.typing_sent = None;
        self.threads = Default::default();
        self.user_id = None;
        self.recovery_prompted = false;
        self.verify = Default::default();
        self.logged_out = true;
        self.spaces.clear();
        self.space = None;
        self.space_at = None;
        self.space_rooms.clear();
        self.session_started = None;
        self.pending_notices.clear();
        if let Some(cache) = &self.notify_icons {
            cache.clear();
        }
        self.count_notices.clear();
        self.detailed_notices.clear();
        self.shown_notices.clear();
        self.row_cache = None;
        self.saving.clear();
        self.saved.clear();
        self.confirm_large = None;
        self.images.clear();
        self.attach.clear();
        self.complete.reset_session();
        self.manage = Default::default();
        self.sync = Some(SyncStatus::Stopped);
    }

    fn set_focus(&mut self, pane: Pane) {
        self.focus = pane;
        match pane {
            Pane::Timeline => {
                // While reading older messages, focusing the pane (a click on empty
                // space, Tab) must not pull the view back to the newest message.
                if self.timeline.scroll == 0 {
                    if self.timeline.selected.is_none() {
                        self.timeline.selected = self.find_selectable(usize::MAX, false);
                    }
                    self.timeline.reveal_selected = true;
                }
            }
            Pane::Sidebar => {
                self.timeline.selected = None;
                if let Some(at) = self.visible_rooms().iter().position(|i| *i == self.active) {
                    self.sidebar.cursor = at;
                }
                self.sidebar.reveal_cursor = true;
            }
            Pane::Composer => self.timeline.selected = None,
        }
        self.sync_completion();
    }

    fn open_room(&mut self, index: usize) {
        if index >= self.rooms.len() {
            return;
        }
        self.auto_opened = false;
        // A thread belongs to its room; picking a room (even the same one) leaves it.
        self.leave_thread();
        if index != self.active {
            if self.typing_sent.take().is_some()
                && let Some(room_id) = self.room_id()
            {
                self.send_matrix(MatrixCmd::Typing {
                    room_id,
                    typing: false,
                });
            }
            // An edit belongs to its room; a plain draft waits there until we come back.
            self.cancel_compose();
            let draft = self.composer.take();
            if let Some(room) = self.rooms.get_mut(self.active) {
                let old_id = room.id.clone();
                room.draft = draft;
                self.save_room_marks(&old_id);
            }
            let new_id = self.rooms[index].id.clone();
            let draft = std::mem::take(&mut self.rooms[index].draft);
            self.composer.set_text(&draft);
            self.restore_room_marks(&new_id);
        }
        self.active = index;
        self.rooms[index].unread = 0;
        self.rooms[index].bold = false;
        self.rooms[index].mention = false;
        self.timeline = TimelineState::default();
        self.sidebar.filter.clear();
        self.request_timeline();
        self.request_members_for_open_room();
        self.set_focus(Pane::Composer);
    }

    /// The name of the selected space, for the room list's header. A space inside
    /// another is shown with its parent, so it is clear where it sits.
    pub fn space_name(&self) -> Option<String> {
        let at = self.space_at?;
        let space = self.spaces.get(at)?;
        if space.level == 0 {
            return Some(space.name.clone());
        }
        match self.spaces[..at].iter().rev().find(|s| s.level == 0) {
            Some(parent) => Some(format!("{} › {}", parent.name, space.name)),
            None => Some(space.name.clone()),
        }
    }

    /// Narrows the room list to the space at this position in `spaces` (`None`: all
    /// rooms). By position, not id: a space inside two others is listed twice.
    pub fn select_space(&mut self, at: Option<usize>) {
        self.set_space(at);
        self.sidebar.cursor = 0;
        self.sidebar.scroll = 0;
        self.sidebar.reveal_cursor = true;
    }

    fn set_space(&mut self, at: Option<usize>) {
        self.space_rooms.clear();
        self.space = None;
        self.space_at = None;
        let Some(at) = at.filter(|at| *at < self.spaces.len()) else {
            return;
        };
        let space = &self.spaces[at];
        self.space_rooms.extend(space.rooms.iter().cloned());
        // A top-level space lists only what is directly in it; the rooms of the
        // spaces inside it follow in the list and belong to the view as well.
        if space.level == 0 {
            for inner in self.spaces[at + 1..].iter().take_while(|s| s.level > 0) {
                self.space_rooms.extend(inner.rooms.iter().cloned());
            }
        }
        self.space = Some(space.id.clone());
        self.space_at = Some(at);
    }

    /// Alt-S and the arrows in the room list: all rooms, then each space in turn.
    fn step_space(&mut self, delta: isize) {
        if self.spaces.is_empty() {
            self.show_toast("You are not in any space");
            return;
        }
        // Position 0 is "all rooms".
        let count = self.spaces.len() as isize + 1;
        let at = self.space_at.map_or(0, |i| i as isize + 1);
        let next = (at + delta).rem_euclid(count);
        self.select_space((next > 0).then(|| next as usize - 1));
    }

    fn apply_spaces(&mut self, spaces: Vec<crate::matrix::SpaceSummary>) {
        // The selected space again, where it is now: at the same place if it still
        // is that space, else wherever it went. Gone means all rooms.
        let selected = self.space.take();
        let was_at = self.space_at.take();
        self.spaces = spaces;
        let at = selected.and_then(|id| {
            was_at
                .filter(|at| self.spaces.get(*at).is_some_and(|s| s.id == id))
                .or_else(|| self.spaces.iter().position(|s| s.id == id))
        });
        // A background update does not move the list under the user.
        self.set_space(at);
        let last = self.visible_rooms().len().saturating_sub(1);
        self.sidebar.cursor = self.sidebar.cursor.min(last);
    }

    /// Ctrl-↑/↓: opens the neighbouring room in the list, wrapping at the ends, and
    /// leaves the focus where it was.
    fn step_room(&mut self, delta: isize) {
        let rooms = self.visible_rooms();
        if rooms.is_empty() {
            return;
        }
        let next = match rooms.iter().position(|index| *index == self.active) {
            Some(at) => (at as isize + delta).rem_euclid(rooms.len() as isize) as usize,
            None => 0,
        };
        let focus = self.focus;
        self.open_room(rooms[next]);
        // The list follows, so the open room is always in view.
        self.sidebar.cursor = self
            .visible_rooms()
            .iter()
            .position(|index| *index == self.active)
            .unwrap_or(0);
        self.sidebar.reveal_cursor = true;
        if focus != Pane::Composer {
            self.set_focus(focus);
        }
    }

    fn move_sidebar_cursor(&mut self, delta: isize) {
        let count = self.visible_rooms().len();
        if count == 0 {
            return;
        }
        let current = self.sidebar.cursor.min(count - 1) as isize;
        self.sidebar.cursor = (current + delta).clamp(0, count as isize - 1) as usize;
        self.sidebar.reveal_cursor = true;
    }

    fn send(&mut self) {
        // An `/upload` command attaches; with files waiting, Enter sends those. An
        // edit comes first: its text is a message, and files wait until it is done.
        if !matches!(self.compose_mode, ComposeMode::Edit(_))
            && (self.upload_command() || self.send_attachments())
        {
            return;
        }
        let Some(room_id) = self.room().map(|room| room.id.clone()) else {
            return;
        };
        if self.composer.text().trim().is_empty() {
            return;
        }
        if self
            .room()
            .is_some_and(|room| room.kind == RoomKind::Invite)
        {
            self.show_toast("Accept the invitation first");
            return;
        }
        let outgoing = match self.format_outgoing(self.composer.text()) {
            // The draft stays, so a typo in a command is easy to fix.
            Outgoing::Rejected(reason) => return self.show_toast(reason),
            outgoing => outgoing,
        };

        if self.matrix.is_some() {
            let command = match self.compose_mode {
                ComposeMode::Normal => MatrixCmd::Send {
                    room_id,
                    message: outgoing,
                    reply_to: None,
                },
                ComposeMode::Reply(i) => {
                    match self.messages().get(i).and_then(|m| m.item.clone()) {
                        Some(ItemRef::Event(event_id)) => MatrixCmd::Send {
                            room_id,
                            message: outgoing,
                            reply_to: Some(event_id),
                        },
                        _ => return self.show_toast("That message has not been sent yet"),
                    }
                }
                ComposeMode::Edit(i) => match self.messages().get(i).and_then(|m| m.item.clone()) {
                    Some(item) => MatrixCmd::Edit {
                        room_id,
                        item,
                        message: outgoing,
                    },
                    None => return self.show_toast("That message cannot be edited"),
                },
            };
            self.send_matrix(command);
            self.composer.take();
            self.clear_completion_marks();
            if matches!(self.compose_mode, ComposeMode::Edit(_)) {
                self.end_edit();
            }
            self.compose_mode = ComposeMode::Normal;
            self.timeline.scroll = 0;
            return;
        }

        // Demo mode: everything happens locally.
        self.composer.take();
        self.clear_completion_marks();
        let mode = self.compose_mode;
        if matches!(mode, ComposeMode::Edit(_)) {
            self.end_edit();
        }
        self.compose_mode = ComposeMode::Normal;
        let (body, html, mentions, kind) = match outgoing {
            Outgoing::Emote {
                plain,
                html,
                mentions,
            } => (plain, html, mentions, MsgKind::Emote),
            Outgoing::Text {
                plain,
                html,
                mentions,
            } => (plain, html, mentions, MsgKind::Text),
            Outgoing::Rejected(_) => return,
        };
        let mut message = Message {
            sender: OWN_NAME.to_owned(),
            time: "now".to_owned(),
            day: "Today".to_owned(),
            body,
            html,
            mentions,
            kind,
            own: true,
            ..Message::default()
        };
        match mode {
            ComposeMode::Edit(i) if i < self.messages().len() => {
                if let Some(target) = self.messages_mut().and_then(|m| m.get_mut(i)) {
                    target.body = message.body;
                    target.html = message.html;
                    target.mentions = message.mentions;
                    target.kind = message.kind;
                    target.edited = true;
                }
            }
            ComposeMode::Reply(i) if i < self.messages().len() => {
                let target = &self.messages()[i];
                message.reply_to = Some((target.sender.clone(), target.body.clone()));
                self.push_message(message);
            }
            _ => self.push_message(message),
        }
        self.timeline.scroll = 0;
    }

    /// Keeps the room informed about whether we are typing. Called after every change
    /// to the draft; repeats the notice every few seconds while typing continues,
    /// because servers let it expire.
    fn update_typing(&mut self) {
        const REPEAT: Duration = Duration::from_secs(4);
        let text = self.composer.text();
        // Commands are not messages, and nothing is typed "to" an invitation.
        let typing = self.typing_notices
            && self.matrix.is_some()
            && !text.trim().is_empty()
            // Correcting an old message is not "typing a message".
            && !matches!(self.compose_mode, ComposeMode::Edit(_))
            && (!text.starts_with('/') || text.starts_with("//") || text.starts_with("/me "))
            && self
                .room()
                .is_some_and(|room| room.kind != RoomKind::Invite);
        let due = self.typing_sent.is_none_or(|sent| sent.elapsed() >= REPEAT);
        let Some(room_id) = self.room_id() else {
            return;
        };
        if typing && due {
            self.typing_sent = Some(Instant::now());
            self.send_matrix(MatrixCmd::Typing {
                room_id,
                typing: true,
            });
        } else if !typing && self.typing_sent.take().is_some() {
            self.send_matrix(MatrixCmd::Typing {
                room_id,
                typing: false,
            });
        }
    }

    /// Opens the `n`th link of a message in the system browser.
    fn open_link(&mut self, message: usize, n: usize) -> bool {
        let link = self.messages().get(message).and_then(|m| {
            crate::rich::Rich::of(m.html.as_deref(), &m.body)
                .links
                .into_iter()
                .nth(n)
        });
        match link {
            Some(url) => {
                if self.matrix.is_some() {
                    open_in_browser(&url);
                }
                self.show_toast(format!("Opening {url}"));
                true
            }
            None => false,
        }
    }

    /// Whether the selected message is one of ours that could not be sent.
    pub fn selected_failed(&self) -> bool {
        self.timeline
            .selected
            .and_then(|i| self.messages().get(i))
            .is_some_and(|m| m.state == SendState::Failed)
    }

    /// System lines and the "new" marker cannot be selected.
    fn selectable(&self, index: usize) -> bool {
        self.messages()
            .get(index)
            .is_some_and(|m| !matches!(m.kind, MsgKind::System | MsgKind::ReadMarker))
    }

    /// The nearest selectable message from `from` in the given direction, inclusive.
    fn find_selectable(&self, from: usize, forward: bool) -> Option<usize> {
        if forward {
            (from..self.messages().len()).find(|i| self.selectable(*i))
        } else {
            (0..=from.min(self.messages().len().checked_sub(1)?))
                .rev()
                .find(|i| self.selectable(*i))
        }
    }

    fn room_id(&self) -> Option<String> {
        self.room().map(|room| room.id.clone())
    }

    /// Asks the worker for the open room's timeline. Every call starts a new
    /// generation, so late updates for an earlier room are recognised and dropped.
    fn request_timeline(&mut self) {
        self.generation += 1;
        if let Some(room_id) = self.room_id() {
            self.send_matrix(MatrixCmd::OpenRoom {
                room_id,
                generation: self.generation,
            });
        }
    }

    /// Called after each draw: loads older messages when the view shows the oldest
    /// loaded one.
    pub fn after_draw(&mut self) {
        self.maybe_mark_read();
        self.maybe_load_older();
        self.send_image_requests();
    }

    /// Sends a read receipt, but only for what the user can actually be looking at:
    /// the messages are on screen, at the newest one, nothing covers them and the
    /// terminal window has the focus. Once per newest message.
    fn maybe_mark_read(&mut self) {
        let seen = std::mem::take(&mut self.timeline.drawn)
            && self.timeline.scroll == 0
            && self.modal.is_none()
            && !self.help_open
            && !self.images.viewer_open()
            && self.window_focused;
        // A thread whose replies have not arrived yet shows nothing to have read.
        if !seen || self.matrix.is_none() || self.thread_loading() {
            return;
        }
        let newest = self.messages().iter().rev().find_map(|m| match &m.item {
            Some(ItemRef::Event(id)) => Some(id.clone()),
            _ => None,
        });
        if newest.is_none() || newest == self.timeline.marked_read {
            return;
        }
        self.timeline.marked_read = newest;
        if let Some(room_id) = self.room_id() {
            self.send_matrix(MatrixCmd::MarkRead {
                room_id,
                public: self.read_receipts,
            });
        }
    }

    /// Loads older messages when the oldest loaded one is on screen. A request is only
    /// made to fill a view that is not full yet, or after the user scrolled up; a
    /// finished request alone never triggers the next one once the view is full.
    fn maybe_load_older(&mut self) {
        /// Requests that brought nothing to show (only hidden events) before giving
        /// up on filling the view by ourselves.
        const MAX_EMPTY: u8 = 5;
        let count = self.messages().len();
        let view = &mut self.timeline;
        if !std::mem::take(&mut view.want_older) || view.loading_older || view.reached_start {
            return;
        }
        let filling = !view.view_full && view.empty_batches < MAX_EMPTY;
        if !(view.older_armed || filling) {
            return;
        }
        if self.matrix.is_none() || self.room().is_none_or(|r| r.kind == RoomKind::Invite) {
            return;
        }
        self.timeline.older_armed = false;
        self.timeline.loading_older = true;
        self.timeline.count_at_request = count;
        if let Some(room_id) = self.room_id() {
            self.send_matrix(MatrixCmd::Paginate {
                room_id,
                generation: self.generation,
            });
        }
    }

    /// Replaces the open room's messages with the worker's current list, keeping the
    /// selection and a pending reply or edit on the same messages.
    fn apply_timeline(&mut self, messages: Vec<Message>) {
        let id_at = |i: usize| self.messages().get(i).map(|m| m.id.clone());
        let selected_id = self.timeline.selected.and_then(id_at);
        let compose_id = match self.compose_mode {
            ComposeMode::Reply(i) | ComposeMode::Edit(i) => id_at(i),
            ComposeMode::Normal => None,
        };
        if let Some(target) = self.messages_mut() {
            *target = messages;
        }
        let find = |messages: &[Message], id: Option<String>| {
            id.and_then(|id| messages.iter().position(|m| m.id == id))
        };
        let selected_at = find(self.messages(), selected_id);
        let compose_at = find(self.messages(), compose_id);

        if self.timeline.selected.is_some() {
            self.timeline.selected =
                selected_at.or_else(|| self.find_selectable(usize::MAX, false));
            if self.timeline.selected.is_none() && self.focus == Pane::Timeline {
                self.set_focus(Pane::Composer);
            }
        }
        self.compose_mode = match (self.compose_mode, compose_at) {
            (ComposeMode::Reply(_), Some(i)) => ComposeMode::Reply(i),
            (ComposeMode::Edit(_), Some(i)) => ComposeMode::Edit(i),
            (ComposeMode::Edit(_), None) => {
                // The message being edited is gone; its text must not be sent as new.
                self.end_edit();
                ComposeMode::Normal
            }
            _ => ComposeMode::Normal,
        };
    }

    /// Starts editing: the unsent draft is put aside and comes back afterwards.
    fn begin_edit(&mut self, index: usize, text: &str) {
        let already_editing = matches!(self.compose_mode, ComposeMode::Edit(_));
        if !already_editing {
            self.edit_stash = Some(self.composer.take());
        }
        let (marks, had_mentions) = self
            .messages()
            .get(index)
            .map(|msg| {
                let marks =
                    crate::complete::marks_from_message(msg.html.as_deref(), msg.mentions.as_ref());
                let had = msg
                    .mentions
                    .as_ref()
                    .is_some_and(|m| !m.user_ids.is_empty() || m.room);
                (marks, had)
            })
            .unwrap_or_default();
        self.stash_marks_for_edit(marks, had_mentions, already_editing);
        self.composer.set_text(text);
        self.compose_mode = ComposeMode::Edit(index);
        self.set_focus(Pane::Composer);
    }

    /// Ends an edit (sent, cancelled or impossible) and brings the draft back.
    fn end_edit(&mut self) {
        let draft = self.edit_stash.take().unwrap_or_default();
        self.composer.set_text(&draft);
        self.restore_marks_after_edit();
        self.compose_mode = ComposeMode::Normal;
    }

    fn push_message(&mut self, message: Message) {
        if let Some(messages) = self.messages_mut() {
            messages.push(message);
        }
    }

    fn toggle_reaction(&mut self, msg: usize, key: &str) {
        if self.matrix.is_some() {
            let item = self.messages().get(msg).and_then(|m| m.item.clone());
            if let (Some(item), Some(room_id)) = (item, self.room_id()) {
                self.send_matrix(MatrixCmd::ToggleReaction {
                    room_id,
                    item,
                    key: key.to_owned(),
                });
            }
            return;
        }
        let Some(message) = self.messages_mut().and_then(|m| m.get_mut(msg)) else {
            return;
        };
        match message.reactions.iter().position(|r| r.key == key) {
            Some(at) => {
                let reaction = &mut message.reactions[at];
                if reaction.own {
                    reaction.own = false;
                    reaction.count -= 1;
                    if reaction.count == 0 {
                        message.reactions.remove(at);
                    }
                } else {
                    reaction.own = true;
                    reaction.count += 1;
                }
            }
            None => message.reactions.push(Reaction {
                key: key.to_owned(),
                count: 1,
                own: true,
            }),
        }
    }

    fn delete_message(&mut self, index: usize) {
        match self.messages().get(index) {
            None => return,
            Some(message) if !message.own => {
                self.show_toast("You can only delete your own messages");
                return;
            }
            Some(_) => {}
        }
        if self.matrix.is_some() {
            // The timeline update that follows shows it as deleted.
            let item = self.messages().get(index).and_then(|m| m.item.clone());
            if let (Some(item), Some(room_id)) = (item, self.room_id()) {
                self.send_matrix(MatrixCmd::Redact { room_id, item });
            }
            return;
        }
        if let Some(messages) = self.messages_mut() {
            messages.remove(index);
        }
        // Keep a pending reply or edit pointing at the right message.
        self.compose_mode = match self.compose_mode {
            ComposeMode::Reply(i) | ComposeMode::Edit(i) if i == index => {
                if matches!(self.compose_mode, ComposeMode::Edit(_)) {
                    self.end_edit();
                }
                ComposeMode::Normal
            }
            ComposeMode::Reply(i) if i > index => ComposeMode::Reply(i - 1),
            ComposeMode::Edit(i) if i > index => ComposeMode::Edit(i - 1),
            other => other,
        };
        if self.messages().is_empty() {
            self.set_focus(Pane::Composer);
        } else {
            self.timeline.selected = self
                .find_selectable(index, false)
                .or_else(|| self.find_selectable(index, true));
            self.timeline.reveal_selected = true;
            if self.timeline.selected.is_none() {
                self.set_focus(Pane::Composer);
            }
        }
    }

    fn cancel_compose(&mut self) {
        if matches!(self.compose_mode, ComposeMode::Edit(_)) {
            self.end_edit();
        }
        self.compose_mode = ComposeMode::Normal;
    }

    pub fn perform(&mut self, action: Action) {
        self.dirty = true;
        match action {
            Action::Quit => self.should_quit = true,
            Action::Suspend => self.suspend_requested = true,
            Action::FocusNext => self.set_focus(match self.focus {
                Pane::Sidebar => Pane::Timeline,
                Pane::Timeline => Pane::Composer,
                Pane::Composer => Pane::Sidebar,
            }),
            Action::FocusPrev => self.set_focus(match self.focus {
                Pane::Sidebar => Pane::Composer,
                Pane::Timeline => Pane::Sidebar,
                Pane::Composer => Pane::Timeline,
            }),
            Action::Focus(pane) => self.set_focus(pane),
            Action::FocusDir(dir) => match (dir, self.focus) {
                (Dir::Left, _) => self.set_focus(Pane::Sidebar),
                (Dir::Right, Pane::Sidebar) => self.set_focus(Pane::Composer),
                (Dir::Right, _) => {}
                (Dir::Up, _) => self.set_focus(Pane::Timeline),
                (Dir::Down, _) => self.set_focus(Pane::Composer),
            },
            Action::FocusRoomFilter => {
                self.set_focus(Pane::Sidebar);
                self.sidebar.filter.clear();
                self.sidebar.cursor = 0;
            }
            Action::ToggleHelp => {
                self.help_open = !self.help_open;
                self.help_scroll = 0;
            }
            Action::CycleTheme => {
                self.step_theme(1);
            }
            Action::Back => self.back(),
            Action::PageUp => {
                self.timeline.scroll += self.timeline.page.saturating_sub(1).max(1);
                self.timeline.older_armed = true;
            }
            Action::PageDown => {
                let step = self.timeline.page.saturating_sub(1).max(1);
                self.timeline.scroll = self.timeline.scroll.saturating_sub(step);
            }
            Action::JumpToNewest => {
                self.timeline.scroll = 0;
                if self.focus == Pane::Timeline {
                    self.timeline.selected = self.find_selectable(usize::MAX, false);
                }
            }
            Action::JumpToOldest => {
                self.timeline.scroll = usize::MAX;
                self.timeline.older_armed = true;
                if self.focus == Pane::Timeline {
                    self.timeline.selected = self.find_selectable(0, true);
                    self.timeline.reveal_selected = true;
                }
            }

            Action::SidebarUp => self.move_sidebar_cursor(-1),
            Action::SidebarDown => self.move_sidebar_cursor(1),
            Action::SidebarTop => {
                if !self.visible_rooms().is_empty() {
                    self.sidebar.cursor = 0;
                    self.sidebar.reveal_cursor = true;
                }
            }
            Action::SidebarBottom => {
                let count = self.visible_rooms().len();
                if count > 0 {
                    self.sidebar.cursor = count - 1;
                    self.sidebar.reveal_cursor = true;
                }
            }
            Action::SidebarOpen => {
                if let Some(index) = self.cursor_room() {
                    self.open_room(index);
                }
            }

            Action::SelectOlder => match self.timeline.selected {
                Some(i) if self.focus == Pane::Timeline => {
                    match i
                        .checked_sub(1)
                        .and_then(|j| self.find_selectable(j, false))
                    {
                        Some(older) => self.timeline.selected = Some(older),
                        // At the oldest loaded message: ask for more history.
                        None => self.timeline.older_armed = true,
                    }
                    self.timeline.reveal_selected = true;
                }
                _ => {
                    self.set_focus(Pane::Timeline);
                    // Asked for by key: select from the newest and show it, even if
                    // the view was scrolled away.
                    if self.timeline.selected.is_none() {
                        self.timeline.selected = self.find_selectable(usize::MAX, false);
                    }
                    self.timeline.reveal_selected = true;
                }
            },
            Action::SelectNewer => {
                let newer = self
                    .timeline
                    .selected
                    .and_then(|i| self.find_selectable(i + 1, true));
                match newer {
                    Some(newer) => {
                        self.timeline.selected = Some(newer);
                        self.timeline.reveal_selected = true;
                    }
                    None => {
                        self.timeline.scroll = 0;
                        self.set_focus(Pane::Composer);
                    }
                }
            }
            Action::Reply
                if self.matrix.is_some()
                    && self
                        .timeline
                        .selected
                        .and_then(|i| self.messages().get(i))
                        .is_some_and(|m| m.state == SendState::Failed) =>
            {
                let item = self
                    .timeline
                    .selected
                    .and_then(|i| self.messages().get(i))
                    .and_then(|m| m.item.clone());
                if let (Some(item), Some(room_id)) = (item, self.room_id()) {
                    self.send_matrix(MatrixCmd::Retry { room_id, item });
                }
            }
            Action::Reply => {
                if let Some(i) = self.timeline.selected {
                    if matches!(self.compose_mode, ComposeMode::Edit(_)) {
                        self.end_edit();
                    }
                    self.compose_mode = ComposeMode::Reply(i);
                    self.set_focus(Pane::Composer);
                }
            }
            Action::Edit => {
                let selected = self.timeline.selected;
                match selected.and_then(|i| self.messages().get(i).map(|m| (i, m))) {
                    Some((_, message))
                        if message.own
                            && (message.image.is_some()
                                || message.file.is_some()
                                || !matches!(message.kind, MsgKind::Text | MsgKind::Emote)) =>
                    {
                        self.show_toast("This message cannot be edited");
                    }
                    Some((i, message)) if message.own => {
                        // An action stays an action when edited.
                        let text = if message.kind == MsgKind::Emote {
                            format!("/me {}", message.body)
                        } else {
                            message.body.clone()
                        };
                        self.begin_edit(i, &text);
                    }
                    Some(_) => self.show_toast("You can only edit your own messages"),
                    None => {}
                }
            }
            Action::React => {
                if let Some(i) = self.timeline.selected
                    && self.modal.is_none()
                    && let Some(message) = self.messages().get(i)
                {
                    let picker = EmojiPicker::for_reaction(i, message.id.clone());
                    self.open_modal(Modal::Emoji(picker));
                }
            }
            Action::Delete => {
                if let Some(i) = self.timeline.selected {
                    self.delete_message(i);
                }
            }
            Action::Copy => {
                let text = self
                    .timeline
                    .selected
                    .and_then(|i| self.messages().get(i))
                    .map(|message| message.body.clone())
                    .filter(|body| !body.is_empty());
                if let Some(text) = text {
                    self.pending_copy = Some(text);
                    self.show_toast("Copied");
                } else {
                    self.show_toast("Nothing to copy");
                }
            }
            Action::Open => {
                let opened = match self.timeline.selected {
                    Some(i) => {
                        // A file message is about its file, even with a link in the caption.
                        let has_file = self.messages().get(i).is_some_and(|m| m.file.is_some());
                        self.open_image(i)
                            || (has_file && self.save_attachment(true))
                            || self.open_link(i, 0)
                    }
                    None => false,
                };
                if !opened {
                    self.show_toast("Nothing to open in this message");
                }
            }

            Action::Send => self.send(),
            Action::Newline => self.insert_composer_text("\n"),
            Action::CancelCompose => self.cancel_compose(),

            Action::AcceptInvite | Action::DeclineInvite => {
                let invited = self.room().filter(|room| room.kind == RoomKind::Invite);
                if let Some(room_id) = invited.map(|room| room.id.clone()) {
                    self.send_matrix(if action == Action::AcceptInvite {
                        MatrixCmd::Join { room_id }
                    } else {
                        MatrixCmd::Leave { room_id }
                    });
                }
            }
            Action::OpenEmoji => {
                if self.modal.is_none() {
                    self.set_focus(Pane::Composer);
                    self.open_modal(Modal::Emoji(EmojiPicker::default()));
                }
            }
            Action::LoginSubmit => self.submit_login(false),
            Action::LoginBrowser => self.submit_login(true),
            Action::RecoverySubmit => self.submit_recovery(),
            Action::CloseModal => self.close_modal(),
            Action::OpenSettings => {
                if self.modal.is_none() {
                    self.open_modal(Modal::Settings(SettingsMenu::default()));
                }
            }
            Action::Search => self.open_search(),
            Action::SpaceNext => self.step_space(1),
            Action::SpacePrev => self.step_space(-1),
            Action::RoomPrev => self.step_room(-1),
            Action::RoomNext => self.step_room(1),
            Action::Save => {
                if !self.save_attachment(false) {
                    self.show_toast("Nothing to save in this message");
                }
            }
            Action::StartVerification => self.start_verification(),
            Action::Verify(button) => self.verify_button(button),
            Action::OpenThread => {
                if let Some(i) = self.timeline.selected {
                    self.open_thread(i);
                }
            }
            Action::CloseThread => self.close_thread(),
            Action::Attach => {
                if self.modal.is_none() {
                    self.begin_attach();
                }
            }
            Action::OpenPalette => self.open_palette(),
            Action::OpenRoom(index) => self.open_room(index),
            Action::SelectTheme(index) => {
                if let Some(name) = self.theme_names.get(index).cloned() {
                    self.apply_theme(name.clone());
                    self.show_toast(format!("Theme: {name}"));
                }
            }
            Action::Setting(setting) => self.perform_setting(setting),
            Action::ContextMenu => self.open_menu_by_key(),
            Action::NewRoom
            | Action::RoomInfo
            | Action::LeaveRoom
            | Action::InviteUser
            | Action::Manage(_) => self.perform_manage(action),
        }
    }

    // ----- popups --------------------------------------------------------------------

    /// Only one popup is shown at a time; the help overlay makes way too.
    fn open_modal(&mut self, modal: Modal) {
        self.help_open = false;
        self.modal = Some(modal);
    }

    fn send_matrix(&self, command: MatrixCmd) {
        if let Some(matrix) = &self.matrix {
            matrix.send(self.in_thread(command));
        }
    }

    fn submit_login(&mut self, browser: bool) {
        let Some(Modal::Login(form)) = &mut self.modal else {
            return;
        };
        if form.busy.is_some() {
            return;
        }
        form.error = None;
        form.browser_url = None;
        let command = if browser {
            form.busy = Some("Waiting for your browser…".to_owned());
            MatrixCmd::LoginBrowser {
                homeserver: form.homeserver.clone(),
            }
        } else {
            form.busy = Some("Logging in…".to_owned());
            MatrixCmd::LoginPassword {
                homeserver: form.homeserver.clone(),
                username: form.username.clone(),
                password: std::mem::take(&mut form.password),
            }
        };
        self.send_matrix(command);
    }

    fn submit_recovery(&mut self) {
        let Some(Modal::Recovery(form)) = &mut self.modal else {
            return;
        };
        if form.busy || form.key.trim().is_empty() {
            return;
        }
        form.busy = true;
        form.error = None;
        // Not kept in the form once it is on its way.
        let key = std::mem::take(&mut form.key);
        self.send_matrix(MatrixCmd::Recover { key });
    }

    /// Esc on a popup. The login form cannot be dismissed, only a running login.
    fn close_modal(&mut self) {
        match &mut self.modal {
            Some(Modal::Login(form)) => {
                if form.busy.take().is_some() {
                    form.browser_url = None;
                    self.send_matrix(MatrixCmd::CancelLogin);
                }
            }
            Some(Modal::Recovery(_)) => {
                self.modal = None;
                self.show_toast("Encrypted history stays locked until you enter the key");
            }
            Some(Modal::Emoji(_) | Modal::Settings(_) | Modal::Palette(_) | Modal::Keybinds(_)) => {
                self.modal = None;
            }
            Some(Modal::Search(_)) => self.close_search(),
            Some(Modal::Verify(_)) => self.verify_escape(),
            Some(Modal::New(_) | Modal::RoomInfo(_)) => self.manage_escape(),
            Some(Modal::Menu(_)) => self.modal = None,
            None => {}
        }
    }

    /// An emoji was chosen in the picker: react with it, or put it in the draft.
    fn pick_emoji(&mut self, emoji: &str) {
        let Some(Modal::Emoji(picker)) = self.modal.take() else {
            return;
        };
        let Some((index, id)) = picker.react_to else {
            self.set_focus(Pane::Composer);
            self.insert_composer_text(emoji);
            return;
        };
        // The message may have moved while the picker was open.
        let index = if id.is_empty() {
            Some(index)
        } else {
            self.messages().iter().position(|m| m.id == id)
        };
        if let Some(index) = index {
            self.toggle_reaction(index, emoji);
        }
    }

    fn on_modal_key(&mut self, key: KeyEvent, code: KeyCode, mods: KeyModifiers) {
        let plain = !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        // Ctrl-U clears the input being typed into.
        if (code, mods) == (KeyCode::Char('u'), KeyModifiers::CONTROL) {
            match self.modal.as_mut() {
                Some(Modal::Login(form)) if form.busy.is_none() => {
                    if let Some(field) = form.field_mut() {
                        field.clear();
                    }
                }
                Some(Modal::Recovery(form)) if !form.busy => form.key.clear(),
                Some(Modal::Emoji(picker)) => picker.set_query(String::new()),
                Some(Modal::Palette(palette)) => palette.set_query(String::new()),
                Some(Modal::Search(_)) => self.search_clear(),
                Some(Modal::Settings(menu)) => {
                    if let Some(text) = &mut menu.editing {
                        text.clear();
                    }
                }
                Some(Modal::Verify(popup)) => {
                    if let Some(password) = &mut popup.password {
                        password.clear();
                    }
                }
                Some(Modal::New(_) | Modal::RoomInfo(_)) => self.manage_clear_input(),
                _ => {}
            }
            return;
        }
        match self.modal.as_mut() {
            Some(Modal::Login(form)) => {
                if form.busy.is_some() {
                    if code == KeyCode::Esc {
                        self.close_modal();
                    }
                    return;
                }
                match code {
                    KeyCode::Tab | KeyCode::Down => form.next(),
                    KeyCode::BackTab | KeyCode::Up => form.previous(),
                    KeyCode::Enter => match form.focus {
                        LoginForm::BROWSER_BUTTON => self.submit_login(true),
                        focus if focus + 1 < LoginForm::FIELDS => form.next(),
                        _ => self.submit_login(false),
                    },
                    KeyCode::Backspace => {
                        if let Some(field) = form.field_mut() {
                            field.pop();
                        }
                    }
                    KeyCode::Char(c) if plain => {
                        if let Some(field) = form.field_mut() {
                            field.push(c);
                        }
                    }
                    _ => {}
                }
            }
            Some(Modal::Recovery(form)) => match code {
                KeyCode::Esc => self.close_modal(),
                _ if form.busy => {}
                KeyCode::Enter => self.submit_recovery(),
                KeyCode::Backspace => {
                    form.key.pop();
                }
                KeyCode::Char(c) if plain => form.key.push(c),
                _ => {}
            },
            Some(Modal::Emoji(picker)) => {
                let columns = picker.columns.max(1) as isize;
                let page = columns * picker.rows.max(1) as isize;
                match code {
                    KeyCode::Esc => self.modal = None,
                    KeyCode::Enter => {
                        if let Some(emoji) = picker.selected() {
                            self.pick_emoji(emoji.as_str());
                        }
                    }
                    KeyCode::Left => picker.step(-1),
                    KeyCode::Right => picker.step(1),
                    KeyCode::Up => picker.step(-columns),
                    KeyCode::Down => picker.step(columns),
                    KeyCode::PageUp => picker.step(-page),
                    KeyCode::PageDown => picker.step(page),
                    KeyCode::Backspace => {
                        let mut query = std::mem::take(&mut picker.query);
                        query.pop();
                        picker.set_query(query);
                    }
                    KeyCode::Char(c) if plain => {
                        let mut query = std::mem::take(&mut picker.query);
                        query.push(c);
                        picker.set_query(query);
                    }
                    _ => {}
                }
            }
            Some(Modal::Settings(menu)) if menu.editing.is_some() => match code {
                KeyCode::Esc => menu.editing = None,
                KeyCode::Enter => {
                    let text = menu.editing.take().unwrap_or_default();
                    self.set_download_dir(&text);
                }
                KeyCode::Backspace => {
                    menu.editing.as_mut().map(String::pop);
                }
                KeyCode::Char(c) if plain => {
                    if let Some(text) = &mut menu.editing {
                        text.push(c);
                    }
                }
                _ => {}
            },
            Some(Modal::Settings(menu)) => {
                let setting = menu.current();
                match code {
                    KeyCode::Esc | KeyCode::F(2) => self.modal = None,
                    KeyCode::Up | KeyCode::BackTab => menu.step(-1),
                    KeyCode::Down | KeyCode::Tab => menu.step(1),
                    KeyCode::Left => self.change_setting(setting, -1),
                    KeyCode::Right => self.change_setting(setting, 1),
                    KeyCode::Enter | KeyCode::Char(' ') => self.change_setting(setting, 0),
                    _ => {}
                }
            }
            Some(Modal::Verify(_)) => self.on_verify_key(code, mods),
            Some(Modal::Palette(_)) => self.on_palette_key(code, mods),
            Some(Modal::Search(_)) => self.on_search_key(code, mods),
            Some(Modal::New(_) | Modal::RoomInfo(_)) => self.on_manage_key(code, mods),
            Some(Modal::Menu(_)) => self.on_menu_key(code, mods),
            Some(Modal::Keybinds(_)) => self.on_keybinds_key(key, code, mods),
            None => {}
        }
    }

    // ----- Matrix events -------------------------------------------------------------

    pub fn on_matrix(&mut self, event: MatrixEvent) {
        self.dirty = true;
        match event {
            MatrixEvent::NeedLogin { reason } => {
                self.reset_session();
                self.open_modal(Modal::Login(LoginForm::new(reason)));
            }
            MatrixEvent::LoginFailed(error) => {
                if let Some(Modal::Login(form)) = &mut self.modal {
                    form.busy = None;
                    form.browser_url = None;
                    form.error = Some(error);
                }
            }
            MatrixEvent::BrowserLoginUrl(url) => {
                open_in_browser(&url);
                if let Some(Modal::Login(form)) = &mut self.modal {
                    form.browser_url = Some(url);
                }
            }
            MatrixEvent::LoggedIn { user_id } => {
                if matches!(self.modal, Some(Modal::Login(_))) {
                    self.modal = None;
                }
                self.user_id = Some(user_id);
                self.send_matrix(MatrixCmd::Previews(self.room_previews));
                self.logged_out = false;
                self.session_started = Some(Instant::now());
            }
            MatrixEvent::Sync(status) => self.sync = Some(status),
            // News of a session that has ended must not bring its rooms back.
            MatrixEvent::Rooms(_) if self.logged_out => {}
            MatrixEvent::Rooms(rooms) => {
                self.apply_rooms(rooms);
                // A room that was just joined or created is opened when it is listed.
                self.open_pending_room();
                self.search_check_room();
            }
            MatrixEvent::Recovery(status) => {
                // Takes precedence over the emoji picker, never over the login form.
                let free = matches!(self.modal, None | Some(Modal::Emoji(_)));
                if status == RecoveryStatus::NeedsKey && !self.recovery_prompted && free {
                    self.recovery_prompted = true;
                    self.open_modal(Modal::Recovery(RecoveryForm::default()));
                }
            }
            MatrixEvent::RecoverResult(Ok(())) => {
                if matches!(self.modal, Some(Modal::Recovery(_))) {
                    self.modal = None;
                }
                self.show_toast("Encrypted history unlocked");
            }
            MatrixEvent::RecoverResult(Err(error)) => match &mut self.modal {
                Some(Modal::Recovery(form)) => {
                    form.busy = false;
                    form.error = Some(error);
                }
                _ => self.show_toast(format!("Recovery failed: {error}")),
            },
            MatrixEvent::Timeline {
                room_id,
                generation,
                messages,
                reached_start,
            } => {
                if generation == self.generation && self.room_id().as_deref() == Some(&room_id) {
                    let messages = self.thread_snapshot(messages);
                    self.apply_timeline(messages);
                    self.search_sync();
                    self.timeline.reached_start |= reached_start;
                }
            }
            MatrixEvent::Paginated {
                generation,
                reached_start,
            } => {
                if generation == self.generation {
                    let view = &mut self.timeline;
                    view.loading_older = false;
                    view.reached_start |= reached_start;
                    // Did this request bring anything to show?
                    // (The messages on screen: a thread's while one is open.)
                    let grew = self.rooms.get(self.active).map_or(0, |r| r.messages.len())
                        > view.count_at_request;
                    view.empty_batches = if grew { 0 } else { view.empty_batches + 1 };
                    self.search_after_load();
                }
            }
            MatrixEvent::Typing { generation, names } => {
                if generation == self.generation
                    && let Some(room) = self.rooms.get_mut(self.active)
                {
                    room.typing = (!names.is_empty()).then(|| names.join(", "));
                }
            }
            MatrixEvent::ActionFailed(error) => self.show_toast(error),
            MatrixEvent::Error(error) => {
                self.sync = Some(SyncStatus::Error(error.clone()));
                self.show_toast(error);
            }
            MatrixEvent::Image { key, full, result } => {
                self.images.on_loaded(&key, full, result);
            }
            MatrixEvent::Avatar { url, result } => self.images.on_avatar(&url, result),
            MatrixEvent::Spaces(_) if self.logged_out => {}
            MatrixEvent::Spaces(spaces) => self.apply_spaces(spaces),
            MatrixEvent::Notice(notice) => {
                self.detailed_notices
                    .insert(notice.room_id.clone(), Instant::now());
                self.count_notices
                    .retain(|(_, waiting)| waiting.room_id != notice.room_id);
                self.on_notice(notice);
            }
            MatrixEvent::FileSaved { key, open, result } => self.on_file_saved(key, open, result),
            MatrixEvent::Security(security) => self.on_security(security),
            MatrixEvent::Verification(update) => self.on_verification(update),
            MatrixEvent::CrossSigningSetUp(result) => self.on_cross_signing_set_up(result),
            MatrixEvent::UploadLimit(bytes) => self.attach.limit = Some(bytes),
            MatrixEvent::FilesNotSent(reason) => self.on_files_not_sent(reason),
            MatrixEvent::RoomMembers {
                room_id,
                members,
                can_mention_room,
            } => self.on_room_members(room_id, members, can_mention_room),
            // Answers to requests of a session that has ended.
            MatrixEvent::Manage(_) if self.logged_out => {}
            MatrixEvent::Manage(event) => self.on_manage(event),
        }
        // A menu about something that is gone closes.
        self.menu_follow();
    }

    /// Replaces the room list with the server's, keeping what belongs to the UI (drafts,
    /// loaded messages) and keeping the same room open even if its position changed.
    fn apply_rooms(&mut self, summaries: Vec<RoomSummary>) {
        let mut open_id = self.room().map(|room| room.id.clone());
        // Rooms arrive in batches. If all we could show so far was an invitation,
        // look again: a real conversation is the better default.
        if self.auto_opened
            && self
                .room()
                .is_some_and(|room| room.kind == RoomKind::Invite)
        {
            open_id = None;
        }
        let cursor_id = self
            .cursor_room()
            .and_then(|i| self.rooms.get(i))
            .map(|room| room.id.clone());
        let old_kinds: std::collections::HashMap<String, RoomKind> = self
            .rooms
            .iter()
            .map(|room| (room.id.clone(), room.kind))
            .collect();
        // Before the first list there is nothing to compare with: no news then.
        let had_rooms = !self.rooms.is_empty();
        let mut risen = Vec::new();
        let mut old: std::collections::HashMap<String, Room> = self
            .rooms
            .drain(..)
            .map(|room| (room.id.clone(), room))
            .collect();
        self.rooms = summaries
            .into_iter()
            .map(|summary| {
                let kind = match summary.category {
                    RoomCategory::Invite => RoomKind::Invite,
                    RoomCategory::Favourite => RoomKind::Favourite,
                    RoomCategory::Person => RoomKind::Person,
                    RoomCategory::Room => RoomKind::Room,
                };
                let known = old.remove(&summary.id);
                // The server counts what the account's rules say deserves a
                // notification. A count that went up is news, on any server.
                if let Some(room) = &known
                    && had_rooms
                    && kind != RoomKind::Invite
                    && summary.server_notifications > room.server_count
                    && self
                        .session_started
                        .is_some_and(|at| at.elapsed() >= SESSION_SETTLE)
                {
                    risen.push(crate::matrix::Notice {
                        room_id: summary.id.clone(),
                        room: summary.name.clone(),
                        sender: String::new(),
                        thread: None,
                        body: match summary.server_notifications {
                            1 => "New message".to_owned(),
                            n => format!("{n} new messages"),
                        },
                        highlight: summary.mention,
                    });
                }
                let mut room = known.unwrap_or_else(|| Room::new(&summary.id, &summary.name, kind));
                room.server_count = summary.server_notifications;
                room.name = summary.name;
                room.kind = kind;
                room.unread = summary.notifications.min(u32::MAX as u64) as u32;
                room.bold = summary.unread;
                room.mention = summary.mention;
                room.avatar = summary.avatar;
                room.preview = summary.preview;
                room.alias = summary.alias;
                room
            })
            .collect();

        // Held back for a moment: if the full notification (with sender and text)
        // arrives for the same room, it is shown instead.
        let now = Instant::now();
        self.count_notices
            .extend(risen.into_iter().map(|notice| (now, notice)));

        let position = |rooms: &[Room], id: &Option<String>| {
            id.as_ref()
                .and_then(|id| rooms.iter().position(|room| room.id == *id))
        };
        let was_invite = open_id
            .as_ref()
            .and_then(|id| old_kinds.get(id))
            .is_some_and(|kind| *kind == RoomKind::Invite);
        match position(&self.rooms, &open_id) {
            Some(index) => {
                self.active = index;
                // An accepted invitation becomes a room with messages to load.
                if was_invite && self.rooms[index].kind != RoomKind::Invite {
                    self.timeline = TimelineState::default();
                    self.request_timeline();
                    self.request_members_for_open_room();
                }
            }
            None => {
                // The open room is gone (or this is the first list): show the first one
                // that is a real conversation, with its own draft. A draft for the
                // vanished room has nowhere to go.
                self.auto_opened = true;
                self.threads.open = None;
                self.active = self
                    .rooms
                    .iter()
                    .position(|room| room.kind != RoomKind::Invite)
                    .unwrap_or(0);
                self.timeline = TimelineState::default();
                if matches!(self.compose_mode, ComposeMode::Edit(_)) {
                    self.end_edit();
                }
                self.compose_mode = ComposeMode::Normal;
                self.typing_sent = None;
                if open_id.is_some() {
                    let (draft, new_id) = self
                        .rooms
                        .get_mut(self.active)
                        .map(|room| (std::mem::take(&mut room.draft), Some(room.id.clone())))
                        .unwrap_or((String::new(), None));
                    self.composer.set_text(&draft);
                    if let Some(id) = new_id {
                        self.restore_room_marks(&id);
                    } else {
                        self.clear_completion_marks();
                    }
                }
                self.request_timeline();
                self.request_members_for_open_room();
            }
        }
        if let Some(index) = position(&self.rooms, &cursor_id)
            && let Some(at) = self.visible_rooms().iter().position(|i| *i == index)
        {
            self.sidebar.cursor = at;
        }
    }

    /// Esc: undo the most recent "mode" first, then head back to the composer.
    fn back(&mut self) {
        if self.help_open {
            self.help_open = false;
        } else if self.focus == Pane::Sidebar && !self.sidebar.filter.is_empty() {
            self.sidebar.filter.clear();
            self.sidebar.cursor = 0;
        } else if self.focus != Pane::Composer {
            self.set_focus(Pane::Composer);
        } else if self.compose_mode != ComposeMode::Normal {
            self.cancel_compose();
        } else {
            self.close_thread();
        }
    }

    // ----- input ---------------------------------------------------------------------

    pub fn on_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.on_key(key);
                self.update_typing();
            }
            Event::Mouse(mouse) => {
                self.on_mouse(mouse);
                self.update_typing();
            }
            Event::Paste(text) => {
                self.on_paste(&text);
                self.update_typing();
            }
            Event::Resize(..) => {
                // A resized terminal may have dropped the pictures it was sent.
                self.images.forget_encoded();
                self.dirty = true;
            }
            Event::FocusGained => {
                self.window_focused = true;
                self.dirty = true;
            }
            Event::FocusLost => self.window_focused = false,
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        self.dirty = true;
        self.verify.last_key = Some(Instant::now());
        let (code, mods) = keymap::normalize(key);

        if self.is_keybinds_capturing() {
            self.on_keybinds_key(key, code, mods);
            return;
        }

        // Quit and suspend work everywhere, including behind popups (handy for fetching
        // a password from another program).
        if (self.modal.is_some() || self.help_open)
            && let Some(action) = self.keymap.lookup_popup_global(code, mods)
        {
            self.perform(action);
            return;
        }
        if self.modal.is_some() {
            self.on_modal_key(key, code, mods);
            return;
        }
        if self.images.viewer_open() {
            self.on_viewer_key(code, mods);
            return;
        }

        let context = match self.focus {
            Pane::Sidebar => Context::Sidebar,
            Pane::Timeline => Context::Timeline,
            Pane::Composer => Context::Composer,
        };
        if self.help_open {
            if self.keymap.lookup(context, key) == Some(Action::ToggleHelp) {
                self.help_open = false;
                return;
            }
            match (code, mods) {
                (KeyCode::Esc | KeyCode::F(1) | KeyCode::Enter, _)
                | (KeyCode::Char('?' | 'q'), KeyModifiers::NONE) => self.help_open = false,
                // The drawing keeps this within what there is to scroll.
                (KeyCode::Up, _) => self.help_scroll = self.help_scroll.saturating_sub(1),
                (KeyCode::Down, _) => self.help_scroll += 1,
                (KeyCode::PageUp | KeyCode::Home, _) => self.help_scroll = 0,
                (KeyCode::PageDown | KeyCode::End, _) => self.help_scroll += 10,
                _ => {}
            }
            return;
        }

        // Ctrl-C never quits (too easy to hit); it clears the draft instead.
        if (code, mods) == (KeyCode::Char('c'), KeyModifiers::CONTROL) {
            if !self.clear_draft() {
                let quit = self
                    .keymap
                    .label_in(Context::Global, Action::Quit)
                    .or_else(|| self.keymap.label_for(Action::Quit))
                    .unwrap_or("Ctrl-Q");
                self.show_toast(format!("Press {quit} to quit"));
            }
            return;
        }

        if self.focus == Pane::Composer && self.complete_key(code, mods) {
            return;
        }
        let width = self.composer_view.width;
        if self.focus == Pane::Composer
            && (code, mods) == (KeyCode::Up, KeyModifiers::NONE)
            && !self.composer.on_first_row(width)
        {
            self.composer.up(width);
            self.sync_completion();
            return;
        }
        if self.focus == Pane::Composer && self.attach_key(code, mods) {
            return;
        }
        if let Some(action) = self.keymap.lookup(context, key) {
            self.perform(action);
            return;
        }
        match self.focus {
            Pane::Composer => self.edit_composer(code, mods),
            Pane::Sidebar => match (code, mods) {
                (KeyCode::Char(c), KeyModifiers::NONE) => self.type_char(c),
                (KeyCode::Backspace, _) => {
                    self.sidebar.filter.pop();
                    self.sidebar.cursor = 0;
                    self.sidebar.reveal_cursor = true;
                }
                _ => {}
            },
            // Unbound keys do nothing here; they are never typed into the draft.
            Pane::Timeline => {}
        }
    }

    /// Empties the draft, with what is attached to it. False if there was nothing.
    fn clear_draft(&mut self) -> bool {
        // What is attached goes with the draft.
        let had_files = self.clear_attachments();
        if !self.composer.is_empty() {
            self.composer.take();
            self.clear_completion_marks();
            self.cancel_compose();
            return true;
        }
        had_files
    }

    /// Types into whatever has the focus: the room filter or the composer.
    pub fn type_char(&mut self, c: char) {
        match self.focus {
            Pane::Sidebar => {
                self.sidebar.filter.push(c);
                self.sidebar.cursor = 0;
                self.sidebar.reveal_cursor = true;
            }
            Pane::Composer => self.composer.insert(c.encode_utf8(&mut [0; 4])),
            Pane::Timeline => {}
        }
    }

    fn edit_composer(&mut self, code: KeyCode, mods: KeyModifiers) {
        let width = self.composer_view.width;
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let alt = mods.contains(KeyModifiers::ALT);
        let before = self.composer.text().to_owned();
        let before_cursor = self.composer.cursor();
        match code {
            KeyCode::Char(c) if !ctrl && !alt => self.type_char(c),
            KeyCode::Char('b') if alt => self.composer.word_left(),
            KeyCode::Char('f') if alt => self.composer.word_right(),
            KeyCode::Char('w') if ctrl => self.composer.delete_word_left(),
            KeyCode::Char('a') if ctrl => self.composer.home(width),
            KeyCode::Char('e') if ctrl => self.composer.end(width),
            KeyCode::Backspace if ctrl || alt => self.composer.delete_word_left(),
            KeyCode::Backspace => self.composer.backspace(),
            KeyCode::Delete => self.composer.delete(),
            KeyCode::Left if ctrl => self.composer.word_left(),
            KeyCode::Right if ctrl => self.composer.word_right(),
            KeyCode::Left => self.composer.left(),
            KeyCode::Right => self.composer.right(),
            KeyCode::Down => self.composer.down(width),
            KeyCode::Home => self.composer.home(width),
            KeyCode::End => self.composer.end(width),
            _ => {}
        }
        if self.composer.text() != before {
            self.adjust_marks_for_cursor_edit(before_cursor, before.len());
            self.allow_text_edit_reopen_completion();
        } else {
            self.sync_completion();
        }
    }

    fn on_paste(&mut self, text: &str) {
        self.dirty = true;
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let first_line = text.lines().next().unwrap_or("").trim();
        match &mut self.modal {
            Some(Modal::Login(form)) => {
                if form.busy.is_none()
                    && let Some(field) = form.field_mut()
                {
                    field.push_str(first_line);
                }
                return;
            }
            Some(Modal::Recovery(form)) => {
                if !form.busy {
                    form.key.push_str(first_line);
                }
                return;
            }
            Some(Modal::Emoji(picker)) => {
                let query = format!("{}{first_line}", picker.query);
                picker.set_query(query);
                return;
            }
            Some(Modal::Settings(menu)) => {
                if let Some(typed) = &mut menu.editing {
                    typed.push_str(first_line);
                }
                return;
            }
            Some(Modal::Verify(_)) => return self.on_verify_paste(first_line),
            Some(Modal::Palette(palette)) => {
                palette.push_str(first_line);
                return;
            }
            Some(Modal::Search(_)) => return self.search_paste(first_line),
            Some(Modal::New(_) | Modal::RoomInfo(_)) => return self.on_manage_paste(first_line),
            // A paste is not an answer to a menu.
            Some(Modal::Menu(_)) => return,
            Some(Modal::Keybinds(_)) => return,
            None => {}
        }
        // The picture viewer and the help cover the composer: nothing is pasted
        // (or attached) where it cannot be seen.
        if self.help_open || self.images.viewer_open() {
            return;
        }
        // Files dropped on the terminal arrive as a paste of their paths.
        if self.attach_pasted(&text) {
            return;
        }
        match self.focus {
            Pane::Sidebar => {
                self.sidebar
                    .filter
                    .push_str(text.lines().next().unwrap_or(""));
                self.sidebar.cursor = 0;
            }
            _ => {
                self.set_focus(Pane::Composer);
                self.insert_composer_text(&text);
                self.allow_text_edit_reopen_completion();
            }
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        let hit = self.hits.at(mouse.column, mouse.row);
        if self.menu_mouse(&mouse, hit) {
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.dirty = true;
                // An open popup covers everything else with its backdrop, so only its
                // own controls can be hit.
                match hit {
                    Some(HitId::Backdrop) => {
                        self.images.close_viewer();
                        self.help_open = false;
                        if matches!(
                            self.modal,
                            Some(
                                Modal::Emoji(_)
                                    | Modal::Settings(_)
                                    | Modal::Palette(_)
                                    | Modal::Keybinds(_)
                            )
                        ) {
                            self.modal = None;
                        }
                        if matches!(self.modal, Some(Modal::Search(_))) {
                            self.close_search();
                        }
                        self.close_manage();
                    }
                    Some(HitId::Field(slot)) => {
                        if let Some(Modal::Login(form)) = &mut self.modal
                            && form.busy.is_none()
                        {
                            form.focus = slot;
                        }
                    }
                    Some(HitId::Emoji(index)) => {
                        let emoji = match &self.modal {
                            Some(Modal::Emoji(picker)) => picker.results.get(index).copied(),
                            _ => None,
                        };
                        if let Some(emoji) = emoji {
                            self.pick_emoji(emoji.as_str());
                        }
                    }
                    Some(HitId::Setting(index, delta)) => {
                        if let Some(Modal::Settings(menu)) = &mut self.modal
                            && let Some(setting) = Setting::ALL.get(index).copied()
                        {
                            if menu.selected != index {
                                menu.selected = index;
                                menu.confirm_logout = false;
                                menu.editing = None;
                            }
                            if menu.editing.is_none() {
                                self.change_setting(setting, delta);
                            }
                        }
                    }
                    Some(HitId::Splitter) => self.dragging_splitter = true,
                    Some(HitId::Room(index)) => self.open_room(index),
                    Some(HitId::RoomFilter) => self.set_focus(Pane::Sidebar),
                    Some(HitId::Message(index)) => {
                        self.set_focus(Pane::Timeline);
                        if self.selectable(index) {
                            self.timeline.selected = Some(index);
                        }
                    }
                    Some(HitId::Reaction(msg, reaction)) => {
                        let key = self
                            .messages()
                            .get(msg)
                            .and_then(|m| m.reactions.get(reaction))
                            .map(|r| r.key.clone());
                        if let Some(key) = key {
                            self.toggle_reaction(msg, &key);
                        }
                    }
                    Some(HitId::Link(message, n)) => {
                        self.set_focus(Pane::Timeline);
                        if self.selectable(message) {
                            self.timeline.selected = Some(message);
                        }
                        self.open_link(message, n);
                    }
                    Some(HitId::ComposerText) => {
                        self.set_focus(Pane::Composer);
                        let view = self.composer_view;
                        let row = (mouse.row - view.area.y) as usize + view.first_row;
                        let col = (mouse.column - view.area.x) as usize;
                        self.composer.place(view.width, row, col);
                        self.sync_completion();
                    }
                    Some(HitId::Button(button)) => self.perform(match button {
                        Button::Help => Action::ToggleHelp,
                        Button::Settings => Action::OpenSettings,
                        Button::Theme => Action::CycleTheme,
                        Button::CancelCompose => Action::CancelCompose,
                        Button::JumpToNewest => Action::JumpToNewest,
                        Button::Palette => Action::OpenPalette,
                    }),
                    Some(HitId::Do(action)) => self.perform(action),
                    Some(HitId::Pane(pane)) => self.set_focus(pane),
                    Some(HitId::Overlay) | None => {}
                    Some(HitId::Image(index)) => self.click_image(index),
                    Some(HitId::Thread(index)) => self.open_thread(index),
                    Some(HitId::Attachment(index)) => self.remove_attachment(index),
                    Some(HitId::Palette(index)) => self.run_palette_at(index),
                    Some(HitId::Complete(index)) => {
                        self.accept_completion(Some(index));
                    }
                    // Handled by `menu_mouse` above.
                    Some(HitId::Menu(_)) => {}
                    Some(HitId::KeybindRow(index)) => self.click_keybind_row(index),
                    Some(HitId::KeybindBtn(button)) => self.click_keybind_button(button),
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_splitter => {
                let width = (mouse.column + 1).saturating_sub(self.screen.x);
                self.sidebar.width = width.clamp(SIDEBAR_MIN, max_sidebar(self.screen.width));
                self.dirty = true;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if std::mem::take(&mut self.dragging_splitter) {
                    self.save_setting("sidebar_width", self.sidebar.width as i64);
                }
            }
            MouseEventKind::Moved => {
                if self.hover != hit {
                    self.hover = hit;
                    self.dirty = true;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if matches!(
                    self.modal,
                    Some(
                        Modal::Emoji(_)
                            | Modal::Palette(_)
                            | Modal::RoomInfo(_)
                            | Modal::Keybinds(_)
                    )
                ) =>
            {
                let delta = if mouse.kind == MouseEventKind::ScrollUp {
                    -2
                } else {
                    2
                };
                match &mut self.modal {
                    Some(Modal::Emoji(picker)) => picker.scroll_by(delta),
                    Some(Modal::Palette(palette)) => palette.scroll_by(delta),
                    Some(Modal::RoomInfo(_)) => self.manage_scroll(delta),
                    Some(Modal::Keybinds(editor)) => editor.scroll_by(delta),
                    _ => {}
                }
                self.dirty = true;
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if self.help_open && self.modal.is_none() =>
            {
                self.help_scroll = if mouse.kind == MouseEventKind::ScrollUp {
                    self.help_scroll.saturating_sub(2)
                } else {
                    self.help_scroll + 2
                };
                self.dirty = true;
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if !self.help_open && self.modal.is_none() && !self.images.viewer_open() =>
            {
                let up = mouse.kind == MouseEventKind::ScrollUp;
                // The wheel scrolls what is under the pointer, wherever the focus is.
                match self.hits.pane_at(mouse.column, mouse.row) {
                    Some(Pane::Timeline) => {
                        self.timeline.older_armed |= up;
                        self.timeline.scroll = if up {
                            self.timeline.scroll + WHEEL_LINES
                        } else {
                            self.timeline.scroll.saturating_sub(WHEEL_LINES)
                        };
                    }
                    Some(Pane::Sidebar) => {
                        // A room at a time, also when rooms take two lines.
                        let step = if self.room_previews { 2 } else { 1 };
                        self.sidebar.scroll = if up {
                            self.sidebar.scroll.saturating_sub(step)
                        } else {
                            self.sidebar.scroll + step
                        };
                    }
                    _ => {}
                }
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn on_tick(&mut self) {
        self.verify_tick();
        self.release_count_notices(COUNT_NOTICE_WAIT);
        if self
            .toast
            .as_ref()
            .is_some_and(|(_, since)| since.elapsed() >= TOAST_TIME)
        {
            self.toast = None;
            self.dirty = true;
        }
        if self.images.tick() {
            self.dirty = true;
        }
    }
}

/// Best effort; the login form also shows the address to open by hand.
fn open_in_browser(url: &str) {
    use std::process::{Command, Stdio};
    let _ = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

pub async fn run(paths: &Paths, config: &Config, demo: bool) -> Result<()> {
    let guard = TerminalGuard::enter(config.mouse)?;
    // Terminal queries (image protocol, background colour) go here: on the alternate
    // screen, where a terminal that prints a question instead of answering does no
    // harm, but before mouse and focus reports are on and before the event stream.
    guard.alternate_screen()?;
    let (graphics, term_reply) = crate::images::Graphics::detect_with_term(config.images);
    let initial_scheme = crate::termquery::resolve_color_scheme(term_reply);
    let mut terminal = guard.activate()?;
    let mut events = TermEvents::spawn()?;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Without these, a kill or a closed terminal window would skip all cleanup.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let mut sigwinch = signal(SignalKind::window_change())?;
    let (matrix, mut matrix_events) = if demo {
        (None, None)
    } else {
        let (handle, events) = matrix::spawn(paths.clone());
        (Some(handle), Some(events))
    };
    let mut app = App::new_in_scheme(config, paths.themes_dir(), matrix.clone(), initial_scheme);
    app.config_file = Some(paths.config_file());
    let notify_icons = Arc::new(crate::notify::IconCache::new(paths.notify_icons_dir()));
    struct NotifyCacheGuard(Arc<crate::notify::IconCache>);
    impl Drop for NotifyCacheGuard {
        fn drop(&mut self) {
            self.0.clear();
        }
    }
    let _notify_cache_guard = NotifyCacheGuard(Arc::clone(&notify_icons));
    app.set_notify_icons(notify_icons);
    let mut encoded_images = app.start_images(graphics);
    let mut theme_watch = crate::watch::ThemeWatch::start(paths)?;

    while !app.should_quit {
        if app.suspend_requested {
            app.suspend_requested = false;
            // The terminal frees its copies of the pictures now, in case we are
            // never resumed; they are sent again when drawn.
            app.flush_images(true);
            drop(events);
            terminal::suspend(&mut terminal)?;
            events = TermEvents::spawn()?;
            terminal::query_color_scheme();
            app.dirty = true;
        }
        terminal::set_mouse(app.mouse)?;
        if std::mem::take(&mut app.repaint) {
            // As after a resize: pictures are sent again with the fresh screen.
            app.images.forget_encoded();
            terminal.clear()?;
            app.dirty = true;
        }
        if let Some(text) = app.pending_copy.take() {
            terminal::copy_to_clipboard(&text)?;
        }
        let ready = app.take_notices_with_icons();
        for (notice, icon) in &ready.items {
            crate::notify::show(
                ready.mode,
                notice,
                ready.preview,
                icon,
                ready.icon_cache.clone(),
            );
        }
        if app.dirty {
            app.dirty = false;
            terminal.draw(|frame| ui::draw(frame, &mut app))?;
            app.after_draw();
            app.flush_images(false);
        }
        tokio::select! {
            event = events.next() => match event {
                Some(TermInput::Event(event)) => app.on_event(event),
                Some(TermInput::ColorScheme(scheme)) => app.on_color_scheme(scheme),
                None => break,
            },
            Some(event) = async {
                match matrix_events.as_mut() {
                    Some(events) => events.recv().await,
                    None => std::future::pending().await,
                }
            } => app.on_matrix(event),
            _ = theme_watch.changed() => app.reload_theme(),
            Some(done) = encoded_images.recv() => app.on_image_encoded(done),
            _ = tick.tick() => app.on_tick(),
            _ = tokio::signal::ctrl_c() => app.should_quit = true,
            _ = sigwinch.recv() => {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    app.on_event(Event::Resize(cols, rows));
                }
            }
            _ = sigterm.recv() => app.should_quit = true,
            _ = sighup.recv() => app.should_quit = true,
        }
    }
    app.flush_images(true);
    // Let the worker stop syncing cleanly, but never hang on the way out.
    if let (Some(matrix), Some(mut events)) = (matrix, matrix_events) {
        matrix.send(MatrixCmd::Shutdown);
        // Close every command sender so the worker also sees the channel end.
        drop(matrix);
        drop(app);
        let drained = async { while events.recv().await.is_some() {} };
        let _ = tokio::time::timeout(Duration::from_secs(3), drained).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn key_mod(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn click(column: u16, row: u16) -> Event {
        mouse(MouseEventKind::Down(MouseButton::Left), column, row)
    }

    /// An app that has been drawn once at 100x30, so the hit map is filled.
    fn drawn() -> (App, Terminal<TestBackend>) {
        let mut app = App::demo();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
        (app, terminal)
    }

    fn redraw(app: &mut App, terminal: &mut Terminal<TestBackend>) {
        terminal.draw(|f| ui::draw(f, app)).unwrap();
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
    }

    /// Finds the screen position of `needle` in the last frame.
    fn find(terminal: &Terminal<TestBackend>, needle: &str) -> (u16, u16) {
        let screen = format!("{}", terminal.backend());
        for (row, line) in screen.lines().enumerate() {
            if let Some(byte) = line.find(needle) {
                // Line starts with a quote character from the Display impl.
                let col = crate::text::width(&line[..byte]) - 1;
                return (col as u16, row as u16);
            }
        }
        panic!("{needle:?} not on screen:\n{screen}");
    }

    #[test]
    fn typing_and_sending() {
        let (mut app, _) = drawn();
        let before = app.rooms[0].messages.len();
        type_text(&mut app, "hello");
        app.on_event(key_mod(KeyCode::Enter, KeyModifiers::SHIFT));
        type_text(&mut app, "world");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.rooms[0].messages.len(), before + 1);
        assert_eq!(app.rooms[0].messages.last().unwrap().body, "hello\nworld");
        assert!(app.composer.is_empty());
        // Whitespace only is not sent.
        type_text(&mut app, "   ");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.rooms[0].messages.len(), before + 1);
    }

    #[test]
    fn clicking_a_room_opens_it_and_focuses_the_composer() {
        let (mut app, terminal) = drawn();
        let (col, row) = find(&terminal, "design");
        assert_eq!(app.rooms[4].unread, 5);
        app.on_event(click(col, row));
        assert_eq!(app.rooms[app.active].name, "design");
        assert_eq!(app.focus, Pane::Composer);
        assert_eq!(app.rooms[4].unread, 0);
    }

    #[test]
    fn clicking_panes_moves_focus() {
        let (mut app, terminal) = drawn();
        let (col, row) = find(&terminal, "Favourites");
        app.on_event(click(col, row));
        assert_eq!(app.focus, Pane::Sidebar);
        let (col, row) = find(&terminal, "Has anyone tried");
        app.on_event(click(col, row));
        assert_eq!(app.focus, Pane::Timeline);
        assert_eq!(app.timeline.selected, Some(0));
        let (col, row) = find(&terminal, "Type a message");
        app.on_event(click(col, row));
        assert_eq!(app.focus, Pane::Composer);
        assert_eq!(app.timeline.selected, None);
    }

    #[test]
    fn up_from_composer_walks_messages_and_down_returns() {
        let (mut app, _) = drawn();
        let last = app.rooms[0].messages.len() - 1;
        app.on_event(key(KeyCode::Up));
        assert_eq!(app.focus, Pane::Timeline);
        assert_eq!(app.timeline.selected, Some(last));
        app.on_event(key(KeyCode::Up));
        assert_eq!(app.timeline.selected, Some(last - 1));
        // Letters without a binding do nothing and are not typed into the draft.
        app.on_event(key(KeyCode::Char('x')));
        assert!(app.composer.is_empty());
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Down));
        assert_eq!(app.focus, Pane::Composer);
        assert_eq!(app.timeline.selected, None);
    }

    #[test]
    fn up_inside_multiline_draft_moves_the_cursor_first() {
        let (mut app, _) = drawn();
        type_text(&mut app, "one");
        app.on_event(key_mod(KeyCode::Enter, KeyModifiers::ALT));
        type_text(&mut app, "two");
        app.on_event(key(KeyCode::Up));
        assert_eq!(app.focus, Pane::Composer);
        app.on_event(key(KeyCode::Up));
        assert_eq!(app.focus, Pane::Timeline);
    }

    #[test]
    fn reply_edit_delete_react() {
        let (mut app, _) = drawn();
        // Reply to the newest message.
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.focus, Pane::Composer);
        assert!(matches!(app.compose_mode, ComposeMode::Reply(_)));
        type_text(&mut app, "congrats");
        app.on_event(key(KeyCode::Enter));
        let sent = app.rooms[0].messages.last().unwrap();
        assert_eq!(sent.reply_to.as_ref().unwrap().0, "dave");
        assert_eq!(app.compose_mode, ComposeMode::Normal);

        // Edit it.
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Char('e')));
        assert_eq!(app.composer.text(), "congrats");
        type_text(&mut app, "!");
        let count = app.rooms[0].messages.len();
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.rooms[0].messages.len(), count);
        let edited = app.rooms[0].messages.last().unwrap();
        assert_eq!((edited.body.as_str(), edited.edited), ("congrats!", true));

        // React, then un-react.
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Char('r')));
        assert!(matches!(&app.modal, Some(Modal::Emoji(p)) if p.react_to.is_some()));
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.focus, Pane::Timeline);
        let reactions = &app.rooms[0].messages.last().unwrap().reactions;
        assert_eq!((reactions.len(), reactions[0].key.as_str()), (1, "👍"));
        assert!(app.composer.is_empty());
        app.on_event(key(KeyCode::Char('r')));
        app.on_event(key(KeyCode::Enter));
        assert!(app.rooms[0].messages.last().unwrap().reactions.is_empty());
        // Any emoji can be searched for; Esc reacts with nothing.
        app.on_event(key(KeyCode::Char('r')));
        type_text(&mut app, "rat");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.rooms[0].messages.last().unwrap().reactions[0].key, "🐀");
        app.on_event(key(KeyCode::Char('r')));
        type_text(&mut app, "rat");
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Char('r')));
        app.on_event(key(KeyCode::Esc));
        assert!(app.rooms[0].messages.last().unwrap().reactions.is_empty());

        // Copying hands the text to the event loop.
        app.on_event(key(KeyCode::Char('c')));
        assert_eq!(app.pending_copy.as_deref(), Some("congrats!"));

        // Delete own, refuse to delete or edit someone else's.
        app.on_event(key(KeyCode::Delete));
        assert_eq!(app.rooms[0].messages.len(), count - 1);
        assert_eq!(app.rooms[0].messages.last().unwrap().sender, "dave");
        app.on_event(key(KeyCode::Delete));
        app.on_event(key(KeyCode::Char('e')));
        assert_eq!(app.rooms[0].messages.len(), count - 1);
        assert_eq!(app.focus, Pane::Timeline);
        assert!(app.toast.is_some());
    }

    #[test]
    fn escape_steps_back() {
        let (mut app, _) = drawn();
        app.on_event(key(KeyCode::F(1)));
        assert!(app.help_open);
        // Keys do not leak through the overlay.
        app.on_event(key(KeyCode::Char('x')));
        assert!(app.composer.is_empty());
        app.on_event(key(KeyCode::Esc));
        assert!(!app.help_open);

        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(app.compose_mode, ComposeMode::Reply(_)));
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.compose_mode, ComposeMode::Normal);

        app.on_event(key_mod(KeyCode::Char('r'), KeyModifiers::ALT));
        type_text(&mut app, "bo");
        assert_eq!(app.visible_rooms().len(), 1);
        app.on_event(key(KeyCode::Esc));
        assert!(app.sidebar.filter.is_empty());
        assert_eq!(app.focus, Pane::Sidebar);
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.focus, Pane::Composer);
    }

    #[test]
    fn filter_then_enter_opens_the_match() {
        let (mut app, _) = drawn();
        app.on_event(key_mod(KeyCode::Char('r'), KeyModifiers::ALT));
        type_text(&mut app, "nix");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.rooms[app.active].name, "nixos");
        assert_eq!(app.focus, Pane::Composer);
        assert!(app.sidebar.filter.is_empty());
    }

    #[test]
    fn drafts_stay_with_their_room_and_edits_do_not_leak() {
        let (mut app, _) = drawn();
        type_text(&mut app, "draft for rust");
        app.open_room(2);
        assert!(app.composer.is_empty());
        type_text(&mut app, "draft for bob");
        app.open_room(0);
        assert_eq!(app.composer.text(), "draft for rust");
        app.composer.take();

        // Start editing in room 0, then switch: the edit text must not follow.
        app.timeline.selected = Some(5);
        app.focus = Pane::Timeline;
        app.on_event(key(KeyCode::Char('e')));
        assert!(matches!(app.compose_mode, ComposeMode::Edit(5)));
        app.open_room(2);
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert_eq!(app.composer.text(), "draft for bob");
        app.open_room(0);
        assert!(app.composer.is_empty());
    }

    #[test]
    fn no_rooms_is_a_calm_empty_state() {
        let (mut app, mut terminal) = drawn();
        app.rooms.clear();
        redraw(&mut app, &mut terminal);
        type_text(&mut app, "hello");
        for code in [
            KeyCode::Enter,
            KeyCode::Up,
            KeyCode::Enter,
            KeyCode::Delete,
            KeyCode::Tab,
        ] {
            app.on_event(key(code));
            app.on_event(key(KeyCode::Char('e')));
            app.on_event(key(KeyCode::Char('r')));
            redraw(&mut app, &mut terminal);
        }
        app.on_event(click(40, 10));
        app.on_event(click(5, 5));
        redraw(&mut app, &mut terminal);
    }

    #[test]
    fn status_hints_are_clickable() {
        let (mut app, mut terminal) = drawn();
        app.on_event(key(KeyCode::Up));
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "reply");
        app.on_event(click(col, row));
        assert!(matches!(app.compose_mode, ComposeMode::Reply(_)));
        assert_eq!(app.focus, Pane::Composer);
    }

    #[test]
    fn narrow_mode_has_a_rooms_button() {
        let mut app = App::demo();
        let mut terminal = Terminal::new(TestBackend::new(50, 20)).unwrap();
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "rooms");
        app.on_event(click(col, row));
        assert_eq!(app.focus, Pane::Sidebar);
    }

    fn alt(c: char) -> Event {
        key_mod(KeyCode::Char(c), KeyModifiers::ALT)
    }

    #[test]
    fn emoji_picker_inserts_at_the_cursor() {
        let (mut app, mut terminal) = drawn();
        type_text(&mut app, "ab");
        app.on_event(key(KeyCode::Left));
        app.on_event(alt('e'));
        assert!(matches!(app.modal, Some(Modal::Emoji(_))));
        // Typing searches; Enter takes the first match and closes.
        type_text(&mut app, "thumbsup");
        assert_eq!(app.composer.text(), "ab");
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.composer.text(), "a👍b");
        assert_eq!(app.focus, Pane::Composer);

        // Arrow keys move through the grid.
        app.on_event(alt('e'));
        redraw(&mut app, &mut terminal);
        let columns = match &app.modal {
            Some(Modal::Emoji(picker)) => picker.columns,
            _ => unreachable!(),
        };
        app.on_event(key(KeyCode::Right));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Left));
        app.on_event(key(KeyCode::Enter));
        let expected = crate::emoji::search("")[columns].as_str();
        assert_eq!(app.composer.text(), format!("a👍{expected}b"));

        // Esc closes without inserting.
        app.on_event(alt('e'));
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert_eq!(app.composer.text(), format!("a👍{expected}b"));
    }

    #[test]
    fn emoji_picker_by_mouse() {
        let (mut app, mut terminal) = drawn();
        let (col, row) = find(&terminal, "☺");
        app.on_event(click(col, row));
        assert!(matches!(app.modal, Some(Modal::Emoji(_))));
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "😃");
        app.on_event(click(col, row));
        assert!(app.modal.is_none());
        assert_eq!(app.composer.text(), "😃");

        // Clicking outside closes it; clicks do not reach what is underneath.
        app.on_event(alt('e'));
        redraw(&mut app, &mut terminal);
        app.on_event(click(5, 5));
        assert!(app.modal.is_none());
        assert_eq!(app.focus, Pane::Composer);
        assert_eq!(app.active, 0);
    }

    fn summary(id: &str, name: &str, category: RoomCategory, notifications: u64) -> RoomSummary {
        RoomSummary {
            id: id.into(),
            name: name.into(),
            category,
            notifications,
            preview: None,
            server_notifications: notifications,
            mention: false,
            unread: notifications > 0,
            avatar: None,
            alias: None,
        }
    }

    #[test]
    fn login_flow() {
        let (mut app, mut terminal) = drawn();
        app.rooms.clear();
        app.on_matrix(MatrixEvent::NeedLogin { reason: None });
        redraw(&mut app, &mut terminal);
        // Homeserver is prefilled; replace it, then fill in the rest.
        for _ in 0.."matrix.org".len() {
            app.on_event(key(KeyCode::Backspace));
        }
        type_text(&mut app, "example.org");
        app.on_event(key(KeyCode::Enter));
        type_text(&mut app, "alice");
        app.on_event(key(KeyCode::Tab));
        type_text(&mut app, "secret");
        app.on_event(key(KeyCode::Enter));
        let Some(Modal::Login(form)) = &app.modal else {
            panic!("login form closed early");
        };
        assert_eq!(
            (form.homeserver.as_str(), form.username.as_str()),
            ("example.org", "alice")
        );
        assert!(form.busy.is_some());
        // The password is not kept around after submitting.
        assert!(form.password.is_empty());
        // Input is locked while busy.
        type_text(&mut app, "x");
        redraw(&mut app, &mut terminal);

        app.on_matrix(MatrixEvent::LoginFailed("wrong password".into()));
        let Some(Modal::Login(form)) = &app.modal else {
            panic!("login form closed on failure");
        };
        assert!(form.busy.is_none());
        assert_eq!(form.error.as_deref(), Some("wrong password"));
        redraw(&mut app, &mut terminal);
        find(&terminal, "wrong password");

        app.on_matrix(MatrixEvent::LoggedIn {
            user_id: "@alice:example.org".into(),
        });
        assert!(app.modal.is_none());
        app.on_matrix(MatrixEvent::Recovery(RecoveryStatus::NeedsKey));
        assert!(matches!(app.modal, Some(Modal::Recovery(_))));
        type_text(&mut app, "EsTc 1234");
        app.on_event(key(KeyCode::Enter));
        app.on_matrix(MatrixEvent::RecoverResult(Err("bad key".into())));
        redraw(&mut app, &mut terminal);
        find(&terminal, "bad key");
        app.on_matrix(MatrixEvent::RecoverResult(Ok(())));
        assert!(app.modal.is_none());
        // Asked once per session only.
        app.on_matrix(MatrixEvent::Recovery(RecoveryStatus::NeedsKey));
        assert!(app.modal.is_none());

        // A new login starts clean and asks again; the prompt replaces the emoji picker
        // and the help overlay, and Ctrl-Z still works behind a popup.
        type_text(&mut app, "left over");
        app.on_matrix(MatrixEvent::NeedLogin {
            reason: Some("expired".into()),
        });
        assert!(app.composer.is_empty() && app.rooms.is_empty() && app.user_id.is_none());
        app.on_event(key_mod(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert!(app.suspend_requested);
        app.on_matrix(MatrixEvent::LoggedIn {
            user_id: "@alice:example.org".into(),
        });
        app.on_event(alt('e'));
        app.help_open = true;
        app.on_matrix(MatrixEvent::Recovery(RecoveryStatus::NeedsKey));
        assert!(matches!(app.modal, Some(Modal::Recovery(_))));
        assert!(!app.help_open);
        type_text(&mut app, "abc");
        app.on_event(key_mod(KeyCode::Char('u'), KeyModifiers::CONTROL));
        let Some(Modal::Recovery(form)) = &app.modal else {
            unreachable!()
        };
        assert!(form.key.is_empty());
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "Later");
        app.on_event(click(col, row));
        assert!(app.modal.is_none());
    }

    #[test]
    fn room_updates_keep_the_open_room_and_its_draft() {
        let (mut app, mut terminal) = drawn();
        app.rooms.clear();
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!a", "Alpha", RoomCategory::Room, 0),
            summary("!b", "Beta", RoomCategory::Person, 2),
            summary("!c", "Gamma", RoomCategory::Invite, 0),
        ]));
        redraw(&mut app, &mut terminal);
        assert_eq!(app.rooms[app.active].id, "!a");
        app.open_room(1);
        type_text(&mut app, "draft");
        // The server reorders and renames; Beta moves to the front, Alpha disappears.
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!b", "Beta renamed", RoomCategory::Favourite, 0),
            summary("!c", "Gamma", RoomCategory::Invite, 0),
            summary("!d", "Delta", RoomCategory::Room, 7),
        ]));
        assert_eq!(app.rooms[app.active].id, "!b");
        assert_eq!(app.rooms[app.active].name, "Beta renamed");
        assert_eq!(app.composer.text(), "draft");
        assert_eq!(app.rooms[2].unread, 7);
        redraw(&mut app, &mut terminal);
        find(&terminal, "Invites");

        // The open room vanishes: fall back to the first one without panicking.
        app.on_matrix(MatrixEvent::Rooms(vec![summary(
            "!d",
            "Delta",
            RoomCategory::Room,
            0,
        )]));
        assert_eq!(app.rooms[app.active].id, "!d");
        app.on_matrix(MatrixEvent::Rooms(Vec::new()));
        redraw(&mut app, &mut terminal);
    }

    /// An app connected to a fake worker, with two joined rooms and one invitation.
    fn connected() -> (
        App,
        Terminal<TestBackend>,
        tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>,
    ) {
        let (mut app, mut terminal) = drawn();
        let (handle, commands) = MatrixHandle::for_test();
        app.rooms.clear();
        app.matrix = Some(handle);
        app.sync = Some(SyncStatus::Synced);
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!a", "Alpha", RoomCategory::Room, 0),
            summary("!b", "Beta", RoomCategory::Room, 0),
            summary("!i", "Invited", RoomCategory::Invite, 0),
        ]));
        redraw(&mut app, &mut terminal);
        (app, terminal, commands)
    }

    fn drain(commands: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            seen.push(format!("{command:?}"));
        }
        seen
    }

    fn remote(id: &str, sender: &str, body: &str) -> Message {
        Message {
            id: id.into(),
            item: Some(ItemRef::Event(format!("$ev{id}"))),
            sender: sender.into(),
            time: "10:00".into(),
            day: "Today".into(),
            body: body.into(),
            own: sender == "me",
            ..Message::default()
        }
    }

    fn timeline(app: &App, messages: Vec<Message>) -> MatrixEvent {
        MatrixEvent::Timeline {
            room_id: app.rooms[app.active].id.clone(),
            generation: app.generation,
            messages,
            reached_start: false,
        }
    }

    #[test]
    fn timeline_requests_and_stale_updates() {
        let (mut app, mut terminal, mut commands) = connected();
        // The first room list opens the first room and asks for its history.
        let seen = drain(&mut commands);
        assert!(seen.contains(&"OpenRoom(!a)".to_owned()), "{seen:?}");

        app.on_matrix(timeline(&app, vec![remote("1", "ann", "hello")]));
        assert_eq!(app.rooms[0].messages.len(), 1);

        // An update from before the room switch must not land in the new room.
        let stale = timeline(&app, vec![remote("9", "ann", "late")]);
        app.open_room(1);
        assert!(drain(&mut commands).contains(&"OpenRoom(!b)".to_owned()));
        app.on_matrix(stale);
        assert!(app.rooms[1].messages.is_empty());
        // Same room id but an old generation is dropped too.
        app.on_matrix(MatrixEvent::Timeline {
            room_id: "!b".into(),
            generation: app.generation - 1,
            messages: vec![remote("9", "ann", "late")],
            reached_start: false,
        });
        assert!(app.rooms[1].messages.is_empty());
        redraw(&mut app, &mut terminal);
        find(&terminal, "Loading messages");
    }

    #[test]
    fn history_is_requested_once_until_it_arrives() {
        let (mut app, mut terminal, mut commands) = connected();
        drain(&mut commands);
        app.on_matrix(timeline(&app, vec![remote("1", "ann", "hello")]));
        redraw(&mut app, &mut terminal);
        app.after_draw();
        redraw(&mut app, &mut terminal);
        app.after_draw();
        let seen = drain(&mut commands);
        assert_eq!(
            seen.iter().filter(|c| *c == "Paginate(!a)").count(),
            1,
            "{seen:?}"
        );
        // Done, more exists: the next draw at the top asks again.
        app.on_matrix(MatrixEvent::Paginated {
            generation: app.generation,
            reached_start: false,
        });
        redraw(&mut app, &mut terminal);
        app.after_draw();
        assert!(drain(&mut commands).contains(&"Paginate(!a)".to_owned()));
        // The start of the room was reached: no more requests.
        app.on_matrix(MatrixEvent::Paginated {
            generation: app.generation,
            reached_start: true,
        });
        redraw(&mut app, &mut terminal);
        app.after_draw();
        assert!(!drain(&mut commands).contains(&"Paginate(!a)".to_owned()));
    }

    #[test]
    fn selection_and_reply_follow_their_message_when_history_loads() {
        let (mut app, _terminal, mut commands) = connected();
        app.on_matrix(timeline(
            &app,
            vec![remote("1", "ann", "first"), remote("2", "me", "second")],
        ));
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Up));
        assert_eq!(app.timeline.selected, Some(0));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.compose_mode, ComposeMode::Reply(0));
        app.on_event(key(KeyCode::Up));
        assert_eq!(app.timeline.selected, Some(1));

        // Older history arrives in front, with a system line and a marker mixed in.
        let system = Message {
            id: "s".into(),
            kind: MsgKind::System,
            body: "ann joined".into(),
            ..Message::default()
        };
        let marker = Message {
            id: "m".into(),
            kind: MsgKind::ReadMarker,
            ..Message::default()
        };
        app.on_matrix(timeline(
            &app,
            vec![
                remote("0", "ann", "older"),
                system,
                remote("1", "ann", "first"),
                marker,
                remote("2", "me", "second"),
            ],
        ));
        assert_eq!(app.timeline.selected, Some(4));
        assert_eq!(app.compose_mode, ComposeMode::Reply(2));
        // Moving up skips the marker and the system line.
        app.on_event(key(KeyCode::Up));
        assert_eq!(app.timeline.selected, Some(2));
        app.on_event(key(KeyCode::Up));
        assert_eq!(app.timeline.selected, Some(0));

        // Send the reply: it names the right event and clears the draft.
        app.on_event(key(KeyCode::Esc));
        drain(&mut commands);
        type_text(&mut app, "**hi**");
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).contains(&"Send(!a)".to_owned()));
        assert!(app.composer.is_empty());
        assert_eq!(app.compose_mode, ComposeMode::Normal);

        // If the message being edited disappears, its text is not left as a draft.
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Char('e')));
        assert_eq!(app.composer.text(), "second");
        app.on_matrix(timeline(&app, vec![remote("0", "ann", "older")]));
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert!(app.composer.is_empty());
    }

    #[test]
    fn actions_become_commands_when_connected() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(timeline(
            &app,
            vec![remote("1", "ann", "theirs"), remote("2", "me", "mine")],
        ));
        drain(&mut commands);
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Char('r')));
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Delete));
        assert_eq!(drain(&mut commands), ["ToggleReaction(!a)", "Redact(!a)"]);
        // Nothing changes locally; the worker's next update shows the result.
        assert_eq!(app.rooms[0].messages.len(), 2);

        // Unknown commands are refused and the draft is kept.
        app.on_event(key(KeyCode::Esc));
        type_text(&mut app, "/nosuchcommand x");
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).is_empty());
        assert_eq!(app.composer.text(), "/nosuchcommand x");
        assert!(app.toast.is_some());
        app.composer.take();

        // Typing notices and failures reach the screen.
        app.on_matrix(MatrixEvent::Typing {
            generation: app.generation,
            names: vec!["ann".into()],
        });
        app.on_matrix(MatrixEvent::ActionFailed("Could not send: offline".into()));
        redraw(&mut app, &mut terminal);
        find(&terminal, "ann is typing");
        find(&terminal, "Could not send: offline");

        // Invitations: no sending, but accept and decline work by click.
        app.open_room(2);
        drain(&mut commands);
        type_text(&mut app, "hello");
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).is_empty());
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "Accept");
        app.on_event(click(col, row));
        assert_eq!(drain(&mut commands), ["Join(!i)"]);
        // Once joined, its history is requested.
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!a", "Alpha", RoomCategory::Room, 0),
            summary("!b", "Beta", RoomCategory::Room, 0),
            summary("!i", "Invited", RoomCategory::Room, 0),
        ]));
        assert!(drain(&mut commands).contains(&"OpenRoom(!i)".to_owned()));
    }

    #[test]
    fn links_open_by_click_and_key() {
        let (mut app, mut terminal) = drawn();
        let last = app.rooms[0].messages.len() - 1;
        app.rooms[0].messages[last].html =
            Some("see <a href=\"https://example.org/x\">the page</a> now".into());
        app.timeline_rev += 1;
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "the page");
        app.on_event(click(col + 2, row));
        assert_eq!(
            app.toast.as_ref().unwrap().0,
            "Opening https://example.org/x"
        );
        assert_eq!(app.timeline.selected, Some(last));
        // Clicking beside the link selects the message without opening anything.
        app.on_event(key(KeyCode::Esc));
        app.toast = None;
        app.on_event(click(col - 3, row));
        assert!(app.toast.is_none());
        assert_eq!(app.timeline.selected, Some(last));
        app.on_event(key(KeyCode::Char('o')));
        assert_eq!(
            app.toast.as_ref().unwrap().0,
            "Opening https://example.org/x"
        );
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Char('o')));
        assert_eq!(
            app.toast.as_ref().unwrap().0,
            "Nothing to open in this message"
        );
    }

    #[test]
    fn view_stays_put_when_messages_arrive_while_reading_history() {
        let (mut app, mut terminal, _commands) = connected();
        let many = |n: usize| -> Vec<Message> {
            (0..n)
                .map(|i| remote(&i.to_string(), "ann", &format!("message {i}")))
                .collect()
        };
        app.on_matrix(timeline(&app, many(60)));
        redraw(&mut app, &mut terminal);
        // Scroll up, note what is at the top of the view.
        app.timeline.scroll = 20;
        redraw(&mut app, &mut terminal);
        let before = format!("{}", terminal.backend());
        app.on_matrix(timeline(&app, many(63)));
        redraw(&mut app, &mut terminal);
        assert_eq!(format!("{}", terminal.backend()), before);
        // At the bottom, new messages are shown as they arrive.
        app.timeline.scroll = 0;
        app.on_matrix(timeline(&app, many(64)));
        redraw(&mut app, &mut terminal);
        find(&terminal, "message 63");
    }

    #[test]
    fn typing_notices_and_retry() {
        let (mut app, mut terminal, mut commands) = connected();
        drain(&mut commands);
        type_text(&mut app, "he");
        // One notice for a burst of typing, not one per key.
        assert_eq!(drain(&mut commands), ["Typing(!a, true)"]);
        // Commands are not announced as typing.
        app.composer.take();
        app.on_event(key(KeyCode::Char('/')));
        assert_eq!(drain(&mut commands), ["Typing(!a, false)"]);
        app.composer.take();
        type_text(&mut app, "hello");
        drain(&mut commands);
        // Switching rooms stops the notice in the room we leave.
        app.open_room(1);
        assert!(drain(&mut commands).contains(&"Typing(!a, false)".to_owned()));
        // Sending stops it too.
        type_text(&mut app, "x");
        app.on_event(key(KeyCode::Enter));
        let seen = drain(&mut commands);
        assert_eq!(
            seen[seen.len() - 2..],
            ["Send(!b)", "Typing(!b, false)"],
            "{seen:?}"
        );

        // A failed message offers retry on Enter and discard on Delete.
        let mut failed = remote("9", "me", "did not go");
        failed.state = SendState::Failed;
        failed.item = Some(ItemRef::Transaction("txn1".into()));
        app.on_matrix(timeline(&app, vec![failed]));
        drain(&mut commands);
        app.on_event(key(KeyCode::Up));
        redraw(&mut app, &mut terminal);
        find(&terminal, "try again");
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Delete));
        assert_eq!(drain(&mut commands), ["Retry(!b)", "Redact(!b)"]);
        assert_eq!(app.compose_mode, ComposeMode::Normal);

        // Switched off in the config: no notices at all.
        app.typing_notices = false;
        app.on_event(key(KeyCode::Esc));
        type_text(&mut app, "quiet");
        assert!(drain(&mut commands).is_empty());
    }

    /// Draws a frame and runs what follows a draw, like the event loop does.
    fn frame(app: &mut App, terminal: &mut Terminal<TestBackend>) {
        redraw(app, terminal);
        app.after_draw();
    }

    fn marked_read(commands: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>) -> usize {
        drain(commands)
            .iter()
            .filter(|c| c.starts_with("MarkRead"))
            .count()
    }

    #[test]
    fn read_receipts_only_for_what_was_really_seen() {
        let (mut app, mut terminal, mut commands) = connected();
        let many = |n: usize| -> Vec<Message> {
            (0..n)
                .map(|i| remote(&i.to_string(), "ann", &format!("message {i}")))
                .collect()
        };
        app.timeline.reached_start = true;
        drain(&mut commands);

        // Arriving alone marks nothing; being drawn at the newest message does, once.
        app.on_matrix(timeline(&app, many(50)));
        assert_eq!(marked_read(&mut commands), 0);
        frame(&mut app, &mut terminal);
        frame(&mut app, &mut terminal);
        assert_eq!(marked_read(&mut commands), 1);
        // The same messages again (a profile loaded, a reaction): nothing new to mark.
        app.on_matrix(timeline(&app, many(50)));
        frame(&mut app, &mut terminal);
        assert_eq!(marked_read(&mut commands), 0);

        // Not while a popup covers the messages...
        app.on_event(key(KeyCode::F(1)));
        app.on_matrix(timeline(&app, many(51)));
        frame(&mut app, &mut terminal);
        assert_eq!(marked_read(&mut commands), 0);
        // ...but as soon as it is closed.
        app.on_event(key(KeyCode::Esc));
        frame(&mut app, &mut terminal);
        assert_eq!(marked_read(&mut commands), 1);

        // Not while the terminal window is in the background.
        app.on_event(Event::FocusLost);
        app.on_matrix(timeline(&app, many(52)));
        frame(&mut app, &mut terminal);
        assert_eq!(marked_read(&mut commands), 0);
        app.on_event(Event::FocusGained);
        frame(&mut app, &mut terminal);
        assert_eq!(marked_read(&mut commands), 1);

        // Not while reading older messages; yes after scrolling back down.
        app.on_event(key(KeyCode::PageUp));
        frame(&mut app, &mut terminal);
        app.on_matrix(timeline(&app, many(53)));
        frame(&mut app, &mut terminal);
        assert_eq!(marked_read(&mut commands), 0);
        app.on_event(key(KeyCode::PageDown));
        app.on_event(key(KeyCode::PageDown));
        frame(&mut app, &mut terminal);
        assert_eq!(marked_read(&mut commands), 1);

        // Not when the room list hides the messages in a narrow window.
        let mut narrow = Terminal::new(TestBackend::new(50, 20)).unwrap();
        app.on_event(alt('1'));
        app.on_matrix(timeline(&app, many(54)));
        frame(&mut app, &mut narrow);
        assert_eq!(marked_read(&mut commands), 0);
    }

    #[test]
    fn history_loading_cannot_spin() {
        let (mut app, mut terminal, mut commands) = connected();
        let paginations = |commands: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>| {
            drain(commands)
                .iter()
                .filter(|c| c.starts_with("Paginate"))
                .count()
        };
        let generation = app.generation;
        app.on_matrix(timeline(&app, vec![remote("1", "ann", "hello")]));
        drain(&mut commands);
        // A view that is not full keeps asking, but gives up when requests bring
        // nothing to show.
        let mut asked = 0;
        for _ in 0..20 {
            frame(&mut app, &mut terminal);
            asked += paginations(&mut commands);
            app.on_matrix(MatrixEvent::Paginated {
                generation,
                reached_start: false,
            });
        }
        assert_eq!(asked, 5);
        // The user scrolling up asks once more.
        app.on_event(key(KeyCode::PageUp));
        frame(&mut app, &mut terminal);
        assert_eq!(paginations(&mut commands), 1);
        app.on_matrix(MatrixEvent::Paginated {
            generation,
            reached_start: false,
        });

        // A full view asks only when the user scrolls to the top.
        let many: Vec<Message> = (0..80)
            .map(|i| remote(&i.to_string(), "ann", &format!("message {i}")))
            .collect();
        app.on_matrix(timeline(&app, many.clone()));
        for _ in 0..5 {
            frame(&mut app, &mut terminal);
        }
        assert_eq!(paginations(&mut commands), 0);
        app.timeline.scroll = usize::MAX / 2;
        app.timeline.older_armed = true;
        frame(&mut app, &mut terminal);
        assert_eq!(paginations(&mut commands), 1);
        // Finishing does not start the next one by itself, even at the top.
        app.on_matrix(MatrixEvent::Paginated {
            generation,
            reached_start: false,
        });
        for _ in 0..5 {
            frame(&mut app, &mut terminal);
        }
        assert_eq!(paginations(&mut commands), 0);

        // A snapshot that contains the start of the room ends all loading.
        app.on_matrix(MatrixEvent::Timeline {
            room_id: "!a".into(),
            generation,
            messages: many,
            reached_start: true,
        });
        app.on_event(key(KeyCode::PageUp));
        frame(&mut app, &mut terminal);
        assert_eq!(paginations(&mut commands), 0);
    }

    #[test]
    fn editing_keeps_the_draft_and_the_message_kind() {
        let (mut app, _terminal, mut commands) = connected();
        let mut emote = remote("2", "me", "waves");
        emote.kind = MsgKind::Emote;
        app.on_matrix(timeline(&app, vec![remote("1", "ann", "hi"), emote]));
        type_text(&mut app, "my draft");
        app.on_event(key(KeyCode::Up));
        drain(&mut commands);
        app.on_event(key(KeyCode::Char('e')));
        // An action is edited as an action, and editing is not announced as typing.
        assert_eq!(app.composer.text(), "/me waves");
        type_text(&mut app, "!");
        assert!(!drain(&mut commands).iter().any(|c| c.contains("true")));
        app.on_event(key(KeyCode::Backspace));
        // Cancelling brings the draft back.
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.composer.text(), "my draft");
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        // So does sending the edit.
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Char('e')));
        type_text(&mut app, " again");
        drain(&mut commands);
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).contains(&"Edit(!a)".to_owned()));
        assert_eq!(app.composer.text(), "my draft");
    }

    #[test]
    fn selection_never_lands_on_system_lines_and_clicks_do_not_yank_the_view() {
        let (mut app, mut terminal, _commands) = connected();
        let system = |id: &str| Message {
            id: id.into(),
            kind: MsgKind::System,
            body: "bob joined".into(),
            ..Message::default()
        };
        let mut messages: Vec<Message> = (0..60)
            .map(|i| remote(&i.to_string(), "ann", &format!("message {i}")))
            .collect();
        messages.push(system("s1"));
        app.on_matrix(timeline(&app, messages));
        redraw(&mut app, &mut terminal);

        // End / jump to newest picks the newest real message, not the system line.
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::End));
        assert_eq!(app.timeline.selected, Some(59));

        // Reading older messages, a click on the pane's empty area or a system line
        // focuses it without scrolling back to the bottom.
        app.on_event(key(KeyCode::Esc));
        app.timeline.scroll = 30;
        redraw(&mut app, &mut terminal);
        let before = app.timeline.scroll;
        app.on_event(click(app.screen.width - 3, 0));
        assert_eq!(app.focus, Pane::Timeline);
        redraw(&mut app, &mut terminal);
        assert_eq!(app.timeline.scroll, before);
    }

    #[test]
    fn a_real_room_is_preferred_over_an_invitation_at_startup() {
        let (mut app, _terminal) = drawn();
        let (handle, mut commands) = MatrixHandle::for_test();
        app.rooms.clear();
        app.matrix = Some(handle);
        // The invitation arrives first, the joined room a moment later.
        app.on_matrix(MatrixEvent::Rooms(vec![summary(
            "!i",
            "Invited",
            RoomCategory::Invite,
            0,
        )]));
        assert_eq!(app.rooms[app.active].id, "!i");
        type_text(&mut app, "typed early");
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!i", "Invited", RoomCategory::Invite, 0),
            summary("!a", "Alpha", RoomCategory::Room, 0),
        ]));
        assert_eq!(app.rooms[app.active].id, "!a");
        assert!(drain(&mut commands).contains(&"OpenRoom(!a)".to_owned()));
        // What was typed meanwhile is not lost.
        assert_eq!(app.composer.text(), "typed early");
        // An invitation the user opened themselves stays open.
        app.open_room(0);
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!i", "Invited", RoomCategory::Invite, 0),
            summary("!a", "Alpha", RoomCategory::Room, 0),
            summary("!b", "Beta", RoomCategory::Room, 0),
        ]));
        assert_eq!(app.rooms[app.active].id, "!i");
    }

    #[test]
    fn theme_file_changes_are_picked_up() {
        let dir = std::env::temp_dir().join(format!("matrix-rat-themes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config {
            theme: "generated".into(),
            ..Config::default()
        };
        // The generator has not run yet: the app starts on the default theme.
        let mut app = App::new(&config, dir.clone(), None);
        assert_eq!(app.theme.name, DEFAULT_THEME);
        let rev = app.theme_rev;

        std::fs::write(
            dir.join("generated.toml"),
            "[palette]\nprimary = \"#010203\"\n",
        )
        .unwrap();
        app.reload_theme();
        assert_eq!(app.theme.name, "generated");
        assert!(app.theme_rev > rev);
        assert_eq!(
            app.theme.style(crate::theme::Role::BorderFocused).fg,
            Some(app.theme.color(1, 2, 3))
        );

        // New colours under the same name are applied; a broken file keeps the old
        // theme and says so.
        std::fs::write(
            dir.join("generated.toml"),
            "[palette]\nprimary = \"#040506\"\n",
        )
        .unwrap();
        app.reload_theme();
        assert_eq!(
            app.theme.style(crate::theme::Role::BorderFocused).fg,
            Some(app.theme.color(4, 5, 6))
        );
        // The theme vanishing for a moment (a generator rewriting it) is not an error.
        app.toast = None;
        std::fs::remove_file(dir.join("generated.toml")).unwrap();
        app.reload_theme();
        assert!(app.toast.is_none());
        assert_eq!(app.theme.name, "generated");

        // A changed `theme` setting in the config file is followed.
        // Lives next to the themes directory, not inside it.
        let config_file = dir.with_extension("config.toml");
        std::fs::write(&config_file, "theme = \"terminal\"\n").unwrap();
        app.config_file = Some(config_file.clone());
        app.reload_theme();
        assert_eq!(app.theme.name, "terminal");
        // F3 moves on from the theme in use, and finds newly written themes.
        std::fs::write(dir.join("zz-new.toml"), "[palette]\n").unwrap();
        app.perform(Action::CycleTheme);
        assert_eq!(app.theme.name, "zz-new");
        std::fs::write(&config_file, "theme = \"generated\"\n").unwrap();
        app.reload_theme();
        std::fs::write(
            dir.join("generated.toml"),
            "[palette]\nprimary = \"#040506\"\n",
        )
        .unwrap();
        app.reload_theme();
        assert_eq!(app.theme.name, "generated");

        app.toast = None;
        std::fs::write(dir.join("generated.toml"), "[palette\n").unwrap();
        app.reload_theme();
        assert_eq!(
            app.theme.style(crate::theme::Role::BorderFocused).fg,
            Some(app.theme.color(4, 5, 6))
        );
        assert!(app.toast.is_some());
        let _ = std::fs::remove_file(config_file);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn paired_themes_follow_scheme_changes_and_f3_updates_active_slot() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "matrix-rat-theme-pair-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let themes_dir = dir.join("themes");
        std::fs::create_dir_all(&themes_dir).unwrap();
        let names = Theme::available(&themes_dir);
        let dark_name = names
            .iter()
            .find(|n| *n != "terminal")
            .cloned()
            .expect("built-in non-terminal theme");
        let light_name = names
            .iter()
            .find(|n| *n != "terminal" && *n != &dark_name)
            .cloned()
            .expect("second built-in theme");

        let file = dir.join("config.toml");
        std::fs::write(
            &file,
            format!("theme = \"terminal\"\ntheme_dark = \"{dark_name}\"\ntheme_light = \"{light_name}\"\n"),
        )
        .unwrap();
        let cfg = Config::load(&file).unwrap();

        let mut app = App::new_in_scheme(&cfg, themes_dir, None, ColorScheme::Dark);
        app.config_file = Some(file.clone());
        assert_eq!(app.color_scheme, ColorScheme::Dark);
        assert_eq!(app.theme.name, dark_name);
        assert_eq!(app.wanted_theme, dark_name);

        // Live switch to Light picks the paired light theme.
        app.dirty = false;
        app.on_color_scheme(ColorScheme::Light);
        assert_eq!(app.color_scheme, ColorScheme::Light);
        assert_eq!(app.theme.name, light_name);
        assert!(app.dirty);

        // F3 / Setting::Theme updates only the active Light slot, preserving Dark and base.
        app.step_theme(1);
        let updated_light = app.theme.name.clone();
        assert_ne!(updated_light, light_name);
        let saved = Config::load(&file).unwrap();
        assert_eq!(saved.theme, "terminal");
        assert_eq!(saved.theme_dark.as_deref(), Some(dark_name.as_str()));
        assert_eq!(saved.theme_light.as_deref(), Some(updated_light.as_str()));

        // Live switches bounce cleanly between dark_name and updated_light.
        app.on_color_scheme(ColorScheme::Dark);
        assert_eq!(app.theme.name, dark_name);
        app.on_color_scheme(ColorScheme::Light);
        assert_eq!(app.theme.name, updated_light);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unpaired_theme_ignores_live_scheme_changes() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "matrix-rat-theme-unpaired-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let themes_dir = dir.join("themes");
        std::fs::create_dir_all(&themes_dir).unwrap();
        let file = dir.join("config.toml");

        std::fs::write(&file, "theme = \"terminal\"\n").unwrap();
        let cfg = Config::load(&file).unwrap();
        let mut term_app = App::new_in_scheme(&cfg, themes_dir.clone(), None, ColorScheme::Dark);
        term_app.config_file = Some(file.clone());
        assert_eq!(term_app.theme.name, "terminal");
        term_app.on_color_scheme(ColorScheme::Light);
        assert_eq!(term_app.color_scheme, ColorScheme::Light);
        assert_eq!(term_app.theme.name, "terminal");
        assert_eq!(term_app.wanted_theme, "terminal");

        // A generator theme name that has not been written yet keeps waiting across scheme switches
        // and still writes to `theme` when cycled while unpaired.
        let gen_file = dir.join("gen.toml");
        std::fs::write(&gen_file, "theme = \"generated\"\n").unwrap();
        let gen_cfg = Config::load(&gen_file).unwrap();
        let mut gen_app = App::new_in_scheme(&gen_cfg, themes_dir, None, ColorScheme::Dark);
        gen_app.config_file = Some(gen_file.clone());
        assert_eq!(gen_app.wanted_theme, "generated");
        gen_app.on_color_scheme(ColorScheme::Light);
        assert_eq!(gen_app.wanted_theme, "generated");
        gen_app.step_theme(1);
        let reloaded = Config::load(&gen_file).unwrap();
        assert_eq!(reloaded.theme, gen_app.wanted_theme);
        assert!(reloaded.theme_dark.is_none());
        assert!(reloaded.theme_light.is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pane_jump_keys() {
        let (mut app, _) = drawn();
        app.on_event(key_mod(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(app.focus, Pane::Sidebar);
        app.on_event(key_mod(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(app.focus, Pane::Composer);
        app.on_event(key_mod(KeyCode::Up, KeyModifiers::ALT));
        assert_eq!(app.focus, Pane::Timeline);
        app.on_event(key_mod(KeyCode::Char('1'), KeyModifiers::ALT));
        assert_eq!(app.focus, Pane::Sidebar);
        app.on_event(key(KeyCode::Tab));
        assert_eq!(app.focus, Pane::Timeline);
        app.on_event(key(KeyCode::BackTab));
        assert_eq!(app.focus, Pane::Sidebar);
    }

    #[test]
    fn wheel_scrolls_the_pane_under_the_pointer_without_focusing_it() {
        let (mut app, mut terminal) = drawn();
        let (col, row) = find(&terminal, "Has anyone tried");
        app.on_event(mouse(MouseEventKind::ScrollUp, col, row));
        assert_eq!(app.timeline.scroll, WHEEL_LINES);
        assert_eq!(app.focus, Pane::Composer);
        // Drawing clamps the scroll to what exists.
        redraw(&mut app, &mut terminal);
        assert_eq!(app.timeline.scroll, 0);
    }

    #[test]
    fn dragging_the_splitter_resizes_the_sidebar() {
        let (mut app, mut terminal) = drawn();
        let edge = app.sidebar.width - 1;
        app.on_event(click(edge, 5));
        app.on_event(mouse(MouseEventKind::Drag(MouseButton::Left), 34, 5));
        assert_eq!(app.sidebar.width, 35);
        app.on_event(mouse(MouseEventKind::Drag(MouseButton::Left), 2, 5));
        assert_eq!(app.sidebar.width, SIDEBAR_MIN);
        app.on_event(mouse(MouseEventKind::Drag(MouseButton::Left), 99, 5));
        assert_eq!(app.sidebar.width, 50);
        app.on_event(mouse(MouseEventKind::Up(MouseButton::Left), 99, 5));
        app.on_event(mouse(MouseEventKind::Drag(MouseButton::Left), 20, 5));
        assert_eq!(app.sidebar.width, 50);
        redraw(&mut app, &mut terminal);
    }

    #[test]
    fn clicking_a_reaction_toggles_it() {
        let (mut app, terminal) = drawn();
        let (col, row) = find(&terminal, "👍 2");
        app.on_event(click(col, row));
        let reaction = &app.rooms[0].messages[1].reactions[0];
        assert_eq!((reaction.count, reaction.own), (3, true));
    }

    #[test]
    fn settings_menu_by_keyboard_and_mouse() {
        let dir = std::env::temp_dir().join(format!("matrix-rat-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        std::fs::write(&file, "# mine\nsidebar_width = 30\n").unwrap();
        let (mut app, mut terminal) = drawn();
        app.config_file = Some(file.clone());

        app.on_event(key(KeyCode::F(2)));
        assert!(matches!(app.modal, Some(Modal::Settings(_))));
        redraw(&mut app, &mut terminal);
        find(&terminal, "Appearance");
        // Theme is selected first: → picks the next one and saves it.
        let before = app.theme.name.clone();
        app.on_event(key(KeyCode::Right));
        assert_ne!(app.theme.name, before);
        assert_eq!(Config::load(&file).unwrap().theme, app.theme.name);
        app.on_event(key(KeyCode::Left));
        assert_eq!(app.theme.name, before);

        // Dark theme and Light theme slots cycle through (unset) + themes and clean
        // up config keys when returned to (unset).
        app.on_event(key(KeyCode::Down));
        assert_eq!(app.setting_value(Setting::ThemeDark), "(unset)");
        app.on_event(key(KeyCode::Right));
        assert_ne!(app.setting_value(Setting::ThemeDark), "(unset)");
        assert!(Config::load(&file).unwrap().theme_dark.is_some());
        app.on_event(key(KeyCode::Left));
        assert_eq!(app.setting_value(Setting::ThemeDark), "(unset)");
        assert!(Config::load(&file).unwrap().theme_dark.is_none());

        app.on_event(key(KeyCode::Down));
        assert_eq!(app.setting_value(Setting::ThemeLight), "(unset)");
        app.on_event(key(KeyCode::Right));
        assert_ne!(app.setting_value(Setting::ThemeLight), "(unset)");
        assert!(Config::load(&file).unwrap().theme_light.is_some());
        app.on_event(key(KeyCode::Left));
        assert_eq!(app.setting_value(Setting::ThemeLight), "(unset)");
        assert!(Config::load(&file).unwrap().theme_light.is_none());

        // Width, then the switches.
        app.on_event(key(KeyCode::Down));
        let width = app.sidebar.width;
        app.on_event(key(KeyCode::Right));
        assert_eq!(app.sidebar.width, width + 2);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.sidebar.width, width + 2);
        // Room previews: a second line per room in the list.
        app.on_event(key(KeyCode::Down));
        let rows = app.sidebar_rows().len();
        app.on_event(key(KeyCode::Enter));
        assert!(app.room_previews);
        assert!(app.sidebar_rows().len() > rows);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.sidebar_rows().len(), rows);
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        assert!(!app.mouse);
        app.on_event(key(KeyCode::Char(' ')));
        assert!(app.mouse);

        // By mouse: a click on a row flips its switch.
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "Read receipts");
        app.on_event(click(col, row));
        assert!(!app.read_receipts);
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "▸");
        let theme = app.theme.name.clone();
        app.on_event(click(col, row));
        assert_ne!(app.theme.name, theme);

        let saved = Config::load(&file).unwrap();
        assert_eq!(saved.sidebar_width, width + 2);
        assert!(saved.mouse && !saved.read_receipts);
        assert!(std::fs::read_to_string(&file).unwrap().contains("# mine"));

        // Logging out asks twice; the demo has no account, so it only says so.
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "Log out");
        app.on_event(click(col, row));
        assert!(matches!(app.modal, Some(Modal::Settings(_))));
        assert!(app.toast.is_some());

        // "Key bindings" opens the key-bindings editor; the backdrop closes popups.
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Keybinds(_))) && !app.help_open);
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        app.on_event(key(KeyCode::F(2)));
        redraw(&mut app, &mut terminal);
        app.on_event(click(0, 0));
        assert!(app.modal.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ctrl_arrows_walk_through_the_rooms_from_anywhere() {
        let (mut app, mut terminal) = drawn();
        let order = app.visible_rooms();
        assert_eq!(app.active, order[0]);
        assert_eq!(app.focus, Pane::Composer);
        type_text(&mut app, "draft");
        app.on_event(key_mod(KeyCode::Down, KeyModifiers::CONTROL));
        assert_eq!(app.active, order[1]);
        assert_eq!(app.focus, Pane::Composer);
        assert!(app.composer.is_empty());
        // Back again: the draft waited in its room; the top wraps to the bottom.
        app.on_event(key_mod(KeyCode::Up, KeyModifiers::CONTROL));
        assert_eq!(app.composer.text(), "draft");
        app.on_event(key_mod(KeyCode::Up, KeyModifiers::CONTROL));
        assert_eq!(app.active, *order.last().unwrap());
        assert_eq!(app.cursor_room(), Some(app.active));
        // From the room list the focus stays in the list.
        app.on_event(key_mod(KeyCode::Char('1'), KeyModifiers::ALT));
        app.on_event(key_mod(KeyCode::Down, KeyModifiers::CONTROL));
        assert_eq!((app.active, app.focus), (order[0], Pane::Sidebar));
        redraw(&mut app, &mut terminal);
    }

    #[test]
    fn notifications_only_for_what_is_not_being_watched() {
        use crate::matrix::Notice;
        use crate::notify::NotifyMode;
        let (mut app, _terminal, _commands) = connected();
        app.notify_mode = NotifyMode::Terminal;
        let notice = |room: &str| {
            MatrixEvent::Notice(Notice {
                room_id: room.into(),
                room: "Beta".into(),
                sender: "ann".into(),
                body: "hello".into(),
                highlight: false,
                thread: None,
            })
        };
        // The open room, window focused: the message is on screen already.
        app.on_matrix(notice("!a"));
        assert!(app.pending_notices.is_empty());
        // Another room, or the same room with the window in the background.
        app.on_matrix(notice("!b"));
        app.on_event(Event::FocusLost);
        app.on_matrix(notice("!a"));
        let (notices, mode, preview) = app.take_notices();
        assert_eq!(
            (notices.len(), mode, preview),
            (2, NotifyMode::Terminal, true)
        );
        assert!(app.pending_notices.is_empty());
        // A risen unread count is news too, shown after a short wait, once the
        // session has settled (right after login it is only the backlog arriving)...
        app.detailed_notices.clear();
        app.shown_notices.clear();
        app.session_started = Some(Instant::now());
        let mut early = summary("!b", "Beta", RoomCategory::Room, 1);
        early.server_notifications = 1;
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!a", "Alpha", RoomCategory::Room, 0),
            early,
            summary("!i", "Invited", RoomCategory::Invite, 3),
        ]));
        app.release_count_notices(Duration::ZERO);
        assert!(app.pending_notices.is_empty());
        app.session_started = Instant::now().checked_sub(SESSION_SETTLE);
        let mut beta = summary("!b", "Beta", RoomCategory::Room, 0);
        let rooms = |beta: &RoomSummary| {
            MatrixEvent::Rooms(vec![
                summary("!a", "Alpha", RoomCategory::Room, 0),
                beta.clone(),
                summary("!i", "Invited", RoomCategory::Invite, 3),
            ])
        };
        beta.notifications = 2;
        beta.server_notifications = 2;
        app.on_matrix(rooms(&beta));
        assert!(app.pending_notices.is_empty());
        app.release_count_notices(Duration::ZERO);
        let (notices, ..) = app.take_notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(
            (notices[0].room.as_str(), notices[0].body.as_str()),
            ("Beta", "2 new messages")
        );
        // ...the same count again is not, and a detailed notice replaces the plain one.
        app.on_matrix(rooms(&beta));
        beta.server_notifications = 3;
        app.on_matrix(rooms(&beta));
        // (The per-room pause is over by then.)
        app.shown_notices.clear();
        app.on_matrix(notice("!b"));
        app.release_count_notices(Duration::ZERO);
        let (notices, ..) = app.take_notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].body, "hello");
        // A busy room gets one notification per pause, a flood from many rooms is cut
        // short, and neither limit is reset by the event loop taking them.
        app.shown_notices.clear();
        for _ in 0..20 {
            app.on_matrix(notice("!b"));
            app.take_notices();
        }
        assert_eq!(app.shown_notices.len(), 1);
        let mut shown = 1;
        for n in 0..20 {
            app.on_matrix(notice(&format!("!room{n}")));
            shown += app.take_notices().0.len();
        }
        assert_eq!(shown, NOTICE_MOST);
        // Not while scrolled back or behind the help: then it is not on screen.
        app.shown_notices.clear();
        app.on_event(Event::FocusGained);
        app.help_open = true;
        app.on_matrix(notice("!a"));
        assert_eq!(app.take_notices().0.len(), 1);
        app.help_open = false;
        // Logging out forgets what was waiting.
        app.shown_notices.clear();
        app.on_matrix(notice("!b"));
        app.reset_session();
        assert!(app.pending_notices.is_empty());
        app.on_matrix(rooms(&beta));
        assert!(app.rooms.is_empty());
        app.notify_mode = NotifyMode::Off;
        app.on_matrix(notice("!b"));
        assert!(app.pending_notices.is_empty());
    }

    #[test]
    fn spaces_narrow_the_room_list() {
        use crate::matrix::SpaceSummary;
        let (mut app, mut terminal, _commands) = connected();
        let space = |id: &str, name: &str, level: u8, rooms: &[&str]| SpaceSummary {
            id: id.into(),
            name: name.into(),
            level,
            rooms: rooms.iter().map(|r| (*r).to_owned()).collect(),
        };
        // No spaces: no header line, and the key says so.
        app.on_event(key_mod(KeyCode::Char('s'), KeyModifiers::ALT));
        assert!(app.space.is_none() && app.toast.is_some());
        app.on_matrix(MatrixEvent::Spaces(vec![
            space("!work", "Work", 0, &["!team"]),
            space("!team", "Team", 1, &["!a"]),
            space("!fun", "Fun", 0, &["!b"]),
        ]));
        let names = |app: &App| -> Vec<String> {
            app.visible_rooms()
                .into_iter()
                .map(|i| app.rooms[i].name.clone())
                .collect()
        };
        assert_eq!(names(&app), ["Invited", "Alpha", "Beta"]);
        // A top-level space shows the rooms of the spaces inside it too; invitations
        // stay visible everywhere.
        app.on_event(key_mod(KeyCode::Char('s'), KeyModifiers::ALT));
        assert_eq!(app.space_name().as_deref(), Some("Work"));
        assert_eq!(names(&app), ["Invited", "Alpha"]);
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "▸");
        // The arrow in the list steps on: Team, then Fun, then all rooms again.
        app.on_event(click(col, row));
        assert_eq!(app.space_name().as_deref(), Some("Work › Team"));
        app.on_event(key_mod(KeyCode::Char('s'), KeyModifiers::ALT));
        assert_eq!(app.space_name().as_deref(), Some("Fun"));
        assert_eq!(names(&app), ["Invited", "Beta"]);
        app.on_event(key_mod(KeyCode::Char('s'), KeyModifiers::ALT));
        assert!(app.space.is_none());
        // A space that disappears while selected gives the whole list back.
        app.select_space(Some(2));
        app.on_matrix(MatrixEvent::Spaces(vec![space(
            "!work",
            "Work",
            0,
            &["!a"],
        )]));
        assert!(app.space.is_none());
        assert_eq!(names(&app).len(), 3);

        // A space inside two others is listed twice; stepping still visits every
        // entry and comes back to all rooms, forwards and backwards.
        app.on_matrix(MatrixEvent::Spaces(vec![
            space("!one", "One", 0, &[]),
            space("!shared", "Shared", 1, &["!a"]),
            space("!two", "Two", 0, &[]),
            space("!shared", "Shared", 1, &["!a"]),
            space("!three", "Three", 0, &["!b"]),
        ]));
        let mut seen = Vec::new();
        for _ in 0..6 {
            app.perform(Action::SpaceNext);
            seen.push(app.space_name().unwrap_or_else(|| "All".into()));
        }
        assert_eq!(
            seen,
            ["One", "One › Shared", "Two", "Two › Shared", "Three", "All"]
        );
        app.perform(Action::SpacePrev);
        assert_eq!(app.space_name().as_deref(), Some("Three"));
        app.perform(Action::SpacePrev);
        assert_eq!(app.space_name().as_deref(), Some("Two › Shared"));
        // An update keeps that very entry selected, and an empty space says so.
        app.on_matrix(MatrixEvent::Spaces(vec![
            space("!one", "One", 0, &[]),
            space("!shared", "Shared", 1, &["!a"]),
            space("!two", "Two", 0, &[]),
            space("!shared", "Shared", 1, &["!a"]),
            space("!empty", "Empty", 0, &[]),
        ]));
        assert_eq!(app.space_name().as_deref(), Some("Two › Shared"));
        app.select_space(Some(4));
        app.rooms.retain(|room| room.kind != RoomKind::Invite);
        redraw(&mut app, &mut terminal);
        find(&terminal, "no rooms here");

        // While all rooms are shown, an update leaves the cursor where it is.
        app.select_space(None);
        app.sidebar.cursor = 1;
        app.on_matrix(MatrixEvent::Spaces(vec![space("!one", "One", 0, &[])]));
        assert_eq!(app.sidebar.cursor, 1);
    }

    #[test]
    fn logging_out_needs_a_second_enter() {
        let (mut app, mut terminal, mut commands) = connected();
        drain(&mut commands);
        app.on_event(key(KeyCode::F(2)));
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).is_empty());
        // Moving away withdraws the question.
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).is_empty());
        app.on_event(key(KeyCode::Enter));
        assert_eq!(drain(&mut commands), ["Logout"]);
        assert!(app.modal.is_none());
        // The old session's rooms are gone at once, so nothing more is sent for them.
        assert!(app.rooms.is_empty());
        redraw(&mut app, &mut terminal);
        app.after_draw();
        assert!(drain(&mut commands).is_empty());
    }

    #[test]
    fn clicking_in_the_draft_places_the_cursor() {
        let (mut app, mut terminal) = drawn();
        type_text(&mut app, "hello world");
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "hello world");
        app.on_event(click(col + 5, row));
        type_text(&mut app, ",");
        assert_eq!(app.composer.text(), "hello, world");
    }

    #[test]
    fn help_closes_on_backdrop_click_only() {
        let (mut app, mut terminal) = drawn();
        app.on_event(key(KeyCode::F(1)));
        redraw(&mut app, &mut terminal);
        app.on_event(click(50, 15));
        assert!(app.help_open);
        app.on_event(click(0, 0));
        assert!(!app.help_open);
    }

    #[test]
    fn themes_cycle_and_paste_goes_to_the_composer() {
        let (mut app, _) = drawn();
        let first = app.theme.name.clone();
        app.on_event(key(KeyCode::F(3)));
        assert_ne!(app.theme.name, first);
        for _ in 1..Theme::builtin_names().len() {
            app.on_event(key(KeyCode::F(3)));
        }
        assert_eq!(app.theme.name, first);

        app.on_event(key(KeyCode::Up));
        app.on_event(Event::Paste("a\r\nb".into()));
        assert_eq!(app.focus, Pane::Composer);
        assert_eq!(app.composer.text(), "a\nb");
    }

    #[test]
    fn ctrl_c_clears_the_draft_and_ctrl_q_quits() {
        let (mut app, _) = drawn();
        type_text(&mut app, "draft");
        app.on_event(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.composer.is_empty() && !app.should_quit);
        app.on_event(key_mod(KeyCode::Char('Q'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn desktop_notification_icons_tile_fetch_upgrade_invite_and_logout_wipe() {
        use crate::images::Decoded;
        use crate::notify::{IconCache, NotifyIcon, NotifyMode};

        let (mut app, _terminal, mut commands) = connected();
        let _ = drain(&mut commands);
        let temp_dir = std::env::temp_dir().join(format!(
            "matrix-rat-app-notify-icons-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp_dir);
        let cache = Arc::new(IconCache::new(temp_dir.clone()));
        app.set_notify_icons(Arc::clone(&cache));
        app.notify_mode = NotifyMode::Desktop;

        let mut beta = summary("!b", "#beta", RoomCategory::Room, 0);
        beta.avatar = Some("mxc://example.org/beta".into());
        let mut invite = summary("!i", "InviteRoom", RoomCategory::Invite, 0);
        invite.avatar = Some("mxc://example.org/invite".into());
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!a", "Alpha", RoomCategory::Room, 0),
            beta,
            invite,
        ]));
        let _ = drain(&mut commands);

        let make_notice = |room_id: &str, room: &str| {
            MatrixEvent::Notice(crate::matrix::Notice {
                room_id: room_id.into(),
                room: room.into(),
                sender: "ann".into(),
                body: "hello".into(),
                highlight: false,
                thread: None,
            })
        };

        // 1. Joined room without cached avatar: immediate Tile + background FetchAvatar.
        app.on_matrix(make_notice("!b", "#beta"));
        let ready = app.take_notices_with_icons();
        assert_eq!(
            (ready.items.len(), ready.mode, ready.preview),
            (1, NotifyMode::Desktop, true)
        );
        assert!(matches!(ready.items[0].1, NotifyIcon::Tile { ch: 'B', .. }));
        assert!(matches!(
            commands.try_recv(),
            Ok(MatrixCmd::FetchAvatar { url }) if url == "mxc://example.org/beta"
        ));
        assert!(drain(&mut commands).is_empty());
        let (icon_cache, epoch) = ready.icon_cache.unwrap();
        let tile_path = icon_cache.ensure_file(&ready.items[0].1, epoch).unwrap();
        assert!(tile_path.exists());

        // 2. Deliver avatar -> next notice uses the real Picture and writes PNG to cache.
        app.on_matrix(MatrixEvent::Avatar {
            url: "mxc://example.org/beta".into(),
            result: Ok(Decoded {
                pixels: Arc::new(image::DynamicImage::new_rgba8(96, 96)),
                original: (96, 96),
            }),
        });
        app.shown_notices.clear();
        app.on_matrix(make_notice("!b", "#beta"));
        let ready = app.take_notices_with_icons();
        assert_eq!(ready.items.len(), 1);
        assert!(matches!(
            &ready.items[0].1,
            NotifyIcon::Picture { key, .. } if key == "mxc://example.org/beta"
        ));
        let pic_path = cache.ensure_file(&ready.items[0].1, cache.epoch()).unwrap();
        assert!(pic_path.exists());

        // 3. Invitation room: never fetches avatar, uses letter tile only.
        app.shown_notices.clear();
        app.on_matrix(make_notice("!i", "InviteRoom"));
        let ready = app.take_notices_with_icons();
        assert_eq!(ready.items.len(), 1);
        assert!(matches!(ready.items[0].1, NotifyIcon::Tile { ch: 'I', .. }));
        assert!(drain(&mut commands).is_empty());

        // 4. Logout clears the cache directory and invalidates the old epoch.
        let pre_logout_epoch = cache.epoch();
        app.reset_session();
        assert!(!temp_dir.exists());
        assert_eq!(cache.ensure_file(&ready.items[0].1, pre_logout_epoch), None);
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn hidden_preview_and_terminal_mode_skip_icon_fetch_and_cache() {
        use crate::notify::{IconCache, NotifyIcon, NotifyMode};

        let (mut app, _terminal, mut commands) = connected();
        let _ = drain(&mut commands);
        let temp_dir =
            std::env::temp_dir().join(format!("matrix-rat-app-notify-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        let cache = Arc::new(IconCache::new(temp_dir.clone()));
        app.set_notify_icons(Arc::clone(&cache));

        let mut beta = summary("!b", "Beta", RoomCategory::Room, 0);
        beta.avatar = Some("mxc://example.org/never-fetch".into());
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!a", "Alpha", RoomCategory::Room, 0),
            beta,
        ]));
        let _ = drain(&mut commands);

        let make_notice = || {
            MatrixEvent::Notice(crate::matrix::Notice {
                room_id: "!b".into(),
                room: "Beta".into(),
                sender: "ann".into(),
                body: "secret".into(),
                highlight: false,
                thread: None,
            })
        };

        // Preview off in Desktop mode: Generic icon, no FetchAvatar, no cache files.
        app.notify_mode = NotifyMode::Desktop;
        app.notification_preview = false;
        app.on_matrix(make_notice());
        let ready = app.take_notices_with_icons();
        assert_eq!(ready.items.len(), 1);
        assert!(matches!(ready.items[0].1, NotifyIcon::Generic));
        assert!(drain(&mut commands).is_empty());
        assert!(!temp_dir.exists());

        // Terminal mode even with preview on: Generic icon, no FetchAvatar, no cache files.
        app.shown_notices.clear();
        app.notification_preview = true;
        app.notify_mode = NotifyMode::Terminal;
        app.on_matrix(make_notice());
        let ready = app.take_notices_with_icons();
        assert_eq!(ready.items.len(), 1);
        assert!(matches!(ready.items[0].1, NotifyIcon::Generic));
        assert!(drain(&mut commands).is_empty());
        assert!(!temp_dir.exists());
    }
}
