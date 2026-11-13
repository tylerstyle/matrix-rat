//! Application state, input handling and the event loop.

mod files;
mod media;
mod verify;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures::StreamExt;
use ratatui::layout::Rect;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::MissedTickBehavior;

use crate::action::{Action, Dir, Pane};
use crate::composer::Composer;
use crate::config::Config;
use crate::format::{self, Outgoing};
use crate::keymap::{self, Context};
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
use crate::theme::{DEFAULT_THEME, Theme, truecolor_supported};
use crate::ui::hit::{Button, HitId, HitMap};
use crate::ui::{self, SIDEBAR_MIN, max_sidebar};

const TOAST_TIME: Duration = Duration::from_secs(3);
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
    /// Whether clicks go to the app; the event loop applies changes to the terminal.
    pub mouse: bool,
    /// Text on its way to the system clipboard; the event loop sends it.
    pub pending_copy: Option<String>,
    /// The `download_dir` setting; empty means the desktop's download folder.
    download_dir: String,
    /// Attachments being downloaded, and where finished ones went (by `mxc://` address).
    saving: std::collections::HashSet<String>,
    saved: std::collections::HashMap<String, PathBuf>,
    /// A large attachment the user was asked about once.
    confirm_large: Option<String>,
    /// The theme the user asked for, which may not exist yet (a generator may still
    /// have to write it).
    wanted_theme: String,
    /// Where the configuration lives, to notice a changed `theme` setting.
    config_file: Option<PathBuf>,
    /// The `theme` setting as last read from the configuration.
    config_theme: String,
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
}

impl App {
    /// With `matrix` the app shows the account's rooms; without, built-in demo data.
    pub fn new(config: &Config, themes_dir: PathBuf, matrix: Option<MatrixHandle>) -> Self {
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
        app.mouse = config.mouse;
        app.download_dir = config.download_dir.clone();
        app.config_theme = config.theme.clone();
        app.set_theme(&config.theme);
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
            mouse: true,
            pending_copy: None,
            download_dir: String::new(),
            saving: Default::default(),
            saved: Default::default(),
            confirm_large: None,
            wanted_theme: DEFAULT_THEME.to_owned(),
            config_file: None,
            config_theme: String::new(),
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
        self.toast = Some((text.into(), Instant::now()));
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
            && config.theme != self.config_theme
        {
            self.config_theme = config.theme.clone();
            self.wanted_theme = config.theme;
        }
        let name = self.wanted_theme.clone();
        let exists = Theme::builtin(&name).is_some()
            || self.themes_dir.join(format!("{name}.toml")).exists();
        if exists {
            self.set_theme(&name);
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
        self.set_theme(&name);
        self.config_theme = name.clone();
        self.save_setting("theme", name);
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

    /// What the settings menu shows next to a row.
    pub fn setting_value(&self, setting: Setting) -> String {
        let switch = |on: bool| if on { "on" } else { "off" }.to_owned();
        match setting {
            Setting::Theme => self.wanted_theme.clone(),
            Setting::SidebarWidth => self.sidebar.width.to_string(),
            Setting::Mouse => switch(self.mouse),
            Setting::TypingNotices => switch(self.typing_notices),
            Setting::ReadReceipts => switch(self.read_receipts),
            Setting::DownloadDir => crate::files::display(&self.download_dir()),
            Setting::Keys => String::new(),
            Setting::Encryption => match self.verify.security.map(|s| s.device) {
                Some(crate::matrix::DeviceTrust::Verified) => "this device is verified",
                Some(crate::matrix::DeviceTrust::Unverified) => "not verified, Enter to verify",
                _ => "unknown",
            }
            .to_owned(),
            Setting::Logout => self.user_id.clone().unwrap_or_default(),
        }
    }

    /// Changes a setting from the menu: `delta` is -1 or 1 for ◂ ▸, 0 for Enter or a
    /// click on the row.
    fn change_setting(&mut self, setting: Setting, delta: i8) {
        match setting {
            Setting::Theme => self.step_theme(if delta < 0 { -1 } else { 1 }),
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
            Setting::DownloadDir => {
                if delta == 0
                    && let Some(Modal::Settings(menu)) = &mut self.modal
                {
                    menu.editing = Some(self.download_dir.clone());
                }
            }
            Setting::Keys => {
                if delta == 0 {
                    self.modal = None;
                    self.help_open = true;
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

    /// Nothing of a previous session may linger behind the login form.
    pub(crate) fn reset_session(&mut self) {
        self.rooms.clear();
        self.active = 0;
        self.timeline = TimelineState::default();
        self.composer.take();
        self.compose_mode = ComposeMode::Normal;
        self.edit_stash = None;
        self.typing_sent = None;
        self.user_id = None;
        self.recovery_prompted = false;
        self.verify = Default::default();
        self.saving.clear();
        self.saved.clear();
        self.confirm_large = None;
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
    }

    fn open_room(&mut self, index: usize) {
        if index >= self.rooms.len() {
            return;
        }
        self.auto_opened = false;
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
                room.draft = draft;
            }
            let draft = std::mem::take(&mut self.rooms[index].draft);
            self.composer.set_text(&draft);
        }
        self.active = index;
        self.rooms[index].unread = 0;
        self.rooms[index].bold = false;
        self.rooms[index].mention = false;
        self.timeline = TimelineState::default();
        self.sidebar.filter.clear();
        self.request_timeline();
        self.set_focus(Pane::Composer);
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
        let text = self.composer.text().trim().to_owned();
        let Some(room_id) = self.room().map(|room| room.id.clone()) else {
            return;
        };
        if text.is_empty() {
            return;
        }
        if self
            .room()
            .is_some_and(|room| room.kind == RoomKind::Invite)
        {
            self.show_toast("Accept the invitation first");
            return;
        }
        let outgoing = match format::parse(&text) {
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
            if matches!(self.compose_mode, ComposeMode::Edit(_)) {
                self.end_edit();
            }
            self.compose_mode = ComposeMode::Normal;
            self.timeline.scroll = 0;
            return;
        }

        // Demo mode: everything happens locally.
        self.composer.take();
        let mode = self.compose_mode;
        if matches!(mode, ComposeMode::Edit(_)) {
            self.end_edit();
        }
        self.compose_mode = ComposeMode::Normal;
        let (body, kind) = match outgoing {
            Outgoing::Emote { plain, .. } => (plain, MsgKind::Emote),
            Outgoing::Text { plain, .. } => (plain, MsgKind::Text),
            Outgoing::Rejected(_) => return,
        };
        let mut message = Message {
            sender: OWN_NAME.to_owned(),
            time: "now".to_owned(),
            day: "Today".to_owned(),
            body,
            kind,
            own: true,
            ..Message::default()
        };
        match mode {
            ComposeMode::Edit(i) if i < self.messages().len() => {
                if let Some(target) = self.messages_mut().and_then(|m| m.get_mut(i)) {
                    target.body = message.body;
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
        if !seen || self.matrix.is_none() {
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
        if !matches!(self.compose_mode, ComposeMode::Edit(_)) {
            self.edit_stash = Some(self.composer.take());
        }
        self.composer.set_text(text);
        self.compose_mode = ComposeMode::Edit(index);
        self.set_focus(Pane::Composer);
    }

    /// Ends an edit (sent, cancelled or impossible) and brings the draft back.
    fn end_edit(&mut self) {
        let draft = self.edit_stash.take().unwrap_or_default();
        self.composer.set_text(&draft);
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
            Action::ToggleHelp => self.help_open = !self.help_open,
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

            Action::SidebarUp => self.move_sidebar_cursor(-1),
            Action::SidebarDown => self.move_sidebar_cursor(1),
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
            Action::Newline => self.composer.insert("\n"),
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
            Action::Save => {
                if !self.save_attachment(false) {
                    self.show_toast("Nothing to save in this message");
                }
            }
            Action::StartVerification => self.start_verification(),
            Action::Verify(button) => self.verify_button(button),
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
            matrix.send(command);
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
            Some(Modal::Emoji(_) | Modal::Settings(_)) => self.modal = None,
            Some(Modal::Verify(_)) => self.verify_escape(),
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
            self.composer.insert(emoji);
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

    fn on_modal_key(&mut self, code: KeyCode, mods: KeyModifiers) {
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
                Some(Modal::Settings(menu)) => {
                    if let Some(text) = &mut menu.editing {
                        text.clear();
                    }
                }
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
            Some(Modal::Verify(_)) => self.on_verify_key(code),
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
            }
            MatrixEvent::Sync(status) => self.sync = Some(status),
            MatrixEvent::Rooms(rooms) => self.apply_rooms(rooms),
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
                    self.apply_timeline(messages);
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
                    let grew = self.rooms.get(self.active).map_or(0, |r| r.messages.len())
                        > view.count_at_request;
                    view.empty_batches = if grew { 0 } else { view.empty_batches + 1 };
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
            MatrixEvent::FileSaved { key, open, result } => self.on_file_saved(key, open, result),
            MatrixEvent::Security(security) => self.on_security(security),
            MatrixEvent::Verification(update) => self.on_verification(update),
            MatrixEvent::CrossSigningSetUp(result) => self.on_cross_signing_set_up(result),
        }
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
                let mut room = old
                    .remove(&summary.id)
                    .unwrap_or_else(|| Room::new(&summary.id, &summary.name, kind));
                room.name = summary.name;
                room.kind = kind;
                room.unread = summary.notifications.min(u32::MAX as u64) as u32;
                room.bold = summary.unread;
                room.mention = summary.mention;
                room
            })
            .collect();

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
                }
            }
            None => {
                // The open room is gone (or this is the first list): show the first one
                // that is a real conversation, with its own draft. A draft for the
                // vanished room has nowhere to go.
                self.auto_opened = true;
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
                    let draft = self
                        .rooms
                        .get_mut(self.active)
                        .map(|room| std::mem::take(&mut room.draft))
                        .unwrap_or_default();
                    self.composer.set_text(&draft);
                }
                self.request_timeline();
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
        let (code, mods) = keymap::normalize(key);

        // Quit and suspend work everywhere, including behind popups (handy for fetching
        // a password from another program).
        if mods == KeyModifiers::CONTROL && (self.modal.is_some() || self.help_open) {
            match code {
                KeyCode::Char('q') => {
                    self.should_quit = true;
                    return;
                }
                KeyCode::Char('z') => {
                    self.suspend_requested = true;
                    return;
                }
                _ => {}
            }
        }
        if self.modal.is_some() {
            self.on_modal_key(code, mods);
            return;
        }
        if self.images.viewer_open() {
            self.on_viewer_key(code, mods);
            return;
        }

        if self.help_open {
            match (code, mods) {
                (KeyCode::Esc | KeyCode::F(1) | KeyCode::Enter, _)
                | (KeyCode::Char('?' | 'q'), KeyModifiers::NONE) => self.help_open = false,
                _ => {}
            }
            return;
        }

        // Ctrl-C never quits (too easy to hit); it clears the draft instead.
        if (code, mods) == (KeyCode::Char('c'), KeyModifiers::CONTROL) {
            if self.composer.is_empty() {
                self.show_toast("Press Ctrl-Q to quit");
            } else {
                self.composer.take();
                self.cancel_compose();
            }
            return;
        }

        let context = match self.focus {
            Pane::Sidebar => Context::Sidebar,
            Pane::Timeline => Context::Timeline,
            Pane::Composer => Context::Composer,
        };
        let width = self.composer_view.width;
        if self.focus == Pane::Composer
            && (code, mods) == (KeyCode::Up, KeyModifiers::NONE)
            && !self.composer.on_first_row(width)
        {
            self.composer.up(width);
            return;
        }
        if let Some(action) = keymap::lookup(context, key) {
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
            Some(Modal::Verify(_)) => return,
            None => {}
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
                self.composer.insert(&text);
            }
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        let hit = self.hits.at(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.dirty = true;
                // An open popup covers everything else with its backdrop, so only its
                // own controls can be hit.
                match hit {
                    Some(HitId::Backdrop) => {
                        self.images.close_viewer();
                        self.help_open = false;
                        if matches!(self.modal, Some(Modal::Emoji(_) | Modal::Settings(_))) {
                            self.modal = None;
                        }
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
                    }
                    Some(HitId::Button(button)) => self.perform(match button {
                        Button::Help => Action::ToggleHelp,
                        Button::Settings => Action::OpenSettings,
                        Button::Theme => Action::CycleTheme,
                        Button::CancelCompose => Action::CancelCompose,
                        Button::JumpToNewest => Action::JumpToNewest,
                    }),
                    Some(HitId::Do(action)) => self.perform(action),
                    Some(HitId::Pane(pane)) => self.set_focus(pane),
                    Some(HitId::Overlay) | None => {}
                    Some(HitId::Image(index)) => self.click_image(index),
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
                if matches!(self.modal, Some(Modal::Emoji(_))) =>
            {
                if let Some(Modal::Emoji(picker)) = &mut self.modal {
                    picker.scroll_by(if mouse.kind == MouseEventKind::ScrollUp {
                        -2
                    } else {
                        2
                    });
                    self.dirty = true;
                }
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
                        self.sidebar.scroll = if up {
                            self.sidebar.scroll.saturating_sub(1)
                        } else {
                            self.sidebar.scroll + 1
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
    // Terminal queries (image protocol, background colour) go here, before the event stream.
    let mut terminal = guard.activate()?;
    // What can this terminal draw? Asked on the alternate screen (where a terminal that
    // prints the question instead of answering does no harm) and before anything else
    // reads stdin.
    let graphics = crate::images::Graphics::detect(config.images);
    terminal.clear()?;
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Without these, a kill or a closed terminal window would skip all cleanup.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let (matrix, mut matrix_events) = if demo {
        (None, None)
    } else {
        let (handle, events) = matrix::spawn(paths.clone());
        (Some(handle), Some(events))
    };
    let mut app = App::new(config, paths.themes_dir(), matrix.clone());
    app.config_file = Some(paths.config_file());
    let mut encoded_images = app.start_images(graphics);
    let mut theme_watch = crate::watch::ThemeWatch::start(paths)?;

    while !app.should_quit {
        if app.suspend_requested {
            app.suspend_requested = false;
            terminal::suspend(&mut terminal)?;
            app.images.forget_encoded();
            app.dirty = true;
        }
        terminal::set_mouse(app.mouse)?;
        if let Some(text) = app.pending_copy.take() {
            terminal::copy_to_clipboard(&text)?;
        }
        if app.dirty {
            app.dirty = false;
            terminal.draw(|frame| ui::draw(frame, &mut app))?;
            app.after_draw();
            app.flush_images(false);
        }
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(event)) => app.on_event(event),
                Some(Err(err)) => return Err(err.into()),
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
            mention: false,
            unread: notifications > 0,
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

        // Width, then the switches.
        app.on_event(key(KeyCode::Down));
        let width = app.sidebar.width;
        app.on_event(key(KeyCode::Right));
        assert_eq!(app.sidebar.width, width + 2);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.sidebar.width, width + 2);
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

        // "Key bindings" leads to the help; the backdrop closes the menu.
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none() && app.help_open);
        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::F(2)));
        redraw(&mut app, &mut terminal);
        app.on_event(click(0, 0));
        assert!(app.modal.is_none());
        let _ = std::fs::remove_dir_all(&dir);
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
}
