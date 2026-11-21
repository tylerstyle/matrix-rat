//! Application state, input handling and the event loop.

mod attach;
mod code;
pub mod collect;
mod commands;
mod complete;
mod details;
mod devices;
mod doctor;
mod drafts;
mod emotes;
mod external_editor;
mod files;
pub mod global_search;
mod keybinds;
mod keywords;
#[cfg(test)]
mod measure;
mod media;
mod menu;
mod moderation;
mod notes;
mod palette;
mod paste_picture;
mod people;
mod playback;
mod poll;
mod previews;
mod rooms;
mod rooms_bar;
mod safety;
mod saved;
pub mod search;
mod spaces;
mod thread;
mod thread_list;
mod title;
mod unread;
mod verify;
mod voice;

pub use attach::AttachState;
pub use playback::{AudioState, AudioView};
pub use voice::{RecordingView, TakePhase};

use std::path::PathBuf;
use std::sync::Arc;
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
use crate::format::Outgoing;
use crate::keymap::{self, Context, Keymap};
use crate::lineedit::{EditKeys, EditOutcome, LineEdit};
use crate::matrix::{
    self, MatrixCmd, MatrixEvent, MatrixHandle, RecoveryStatus, RoomCategory, RoomSummary,
    SyncStatus,
};
use crate::modal::{
    EmojiPicker, LoginForm, Modal, Pick, RecoveryForm, ReportForm, Setting, SettingsMenu,
};
use crate::model::{
    ItemRef, Message, MsgKind, OWN_NAME, Reaction, Room, RoomKind, SendState, demo_rooms,
};
use crate::paths::Paths;
use crate::terminal::{self, TerminalGuard};
use crate::theme::{ColorScheme, DEFAULT_THEME, Theme, truecolor_supported};
use crate::ui::hit::{Button, HitId, HitMap, InputTarget};
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
    pub filter: LineEdit,
    /// Session-only quick filter: only show unread rooms (plus the open room).
    pub unread_only: bool,
    /// Session-only quick filter: only show direct chats (plus the open room).
    pub people_only: bool,
    pub width: u16,
    /// Put away by the user: one pane fills the window, the list when it has the focus.
    pub hidden: bool,
    pub scroll: usize,
    pub reveal_cursor: bool,
}

#[derive(Default)]
pub struct TimelineState {
    pub selected: Option<usize>,
    /// Rows scrolled up from the newest message.
    pub scroll: usize,
    /// Ask the next draw to bring the selected message into view.
    pub reveal_selected: bool,
    /// Ask the next draw to put the "new" line at the top of the view, if what is
    /// below it does not fit (else the newest message stays at the bottom).
    pub reveal_unread: bool,
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
    /// Server unread state last marked as read for the open room, to avoid resending
    /// `MarkRead` every frame when unread messages are all ignored/hidden.
    pub hidden_read_state: Option<(String, u64, u32, bool, bool)>,
    /// Set when viewing an event-focused `/context` slice instead of the live tail.
    pub detached: bool,
    /// Set by the draw when the newest loaded row of a detached slice is on screen.
    pub want_newer: bool,
    pub loading_newer: bool,
    /// The forward end of the detached slice has reached the live tail.
    pub reached_end: bool,
    /// The user scrolled towards newer messages since the last forward request.
    pub newer_armed: bool,
    /// Message count when the running forward-pagination request was made.
    pub forward_count_at_request: usize,
    /// Consecutive forward-pagination requests that added nothing visible.
    pub empty_forward_batches: u8,
    /// New remote messages arrived on the room/thread's live timeline while detached.
    pub detached_new: u32,
}

/// Hand-written: what was marked read names an event and a room; only whether there is
/// one is shown.
impl std::fmt::Debug for TimelineState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimelineState")
            .field("selected", &self.selected)
            .field("scroll", &self.scroll)
            .field("reveal_selected", &self.reveal_selected)
            .field("reveal_unread", &self.reveal_unread)
            .field("page", &self.page)
            .field("want_older", &self.want_older)
            .field("loading_older", &self.loading_older)
            .field("reached_start", &self.reached_start)
            .field("older_armed", &self.older_armed)
            .field("view_full", &self.view_full)
            .field("count_at_request", &self.count_at_request)
            .field("empty_batches", &self.empty_batches)
            .field("drawn", &self.drawn)
            .field("marked_read", &self.marked_read.is_some())
            .field("hidden_read_state", &self.hidden_read_state.is_some())
            .field("detached", &self.detached)
            .field("want_newer", &self.want_newer)
            .field("loading_newer", &self.loading_newer)
            .field("reached_end", &self.reached_end)
            .field("newer_armed", &self.newer_armed)
            .field("forward_count_at_request", &self.forward_count_at_request)
            .field("empty_forward_batches", &self.empty_forward_batches)
            .field("detached_new", &self.detached_new)
            .finish()
    }
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
    /// Reminders that came due: shown under a title and text of their own.
    pub reminders: Vec<(crate::notify::DueReminder, crate::notify::NotifyIcon)>,
}

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
    pub own_profile: Option<crate::matrix::OwnProfile>,
    user_info_requests: u64,
    recovery_prompted: bool,
    /// Counts timeline requests; see `request_timeline`.
    generation: u64,
    /// Monotonic allocator shared by live timeline opens and speculative `/context` jumps.
    next_gen: u64,
    /// The open room was picked by the app, not by the user.
    auto_opened: bool,
    /// No chat is open: the user closed it (see `close_chat`). `active` then only
    /// remembers the room that was left, for the room list's cursor; nothing treats
    /// it as open (see `open_index`).
    chat_closed: bool,
    /// The `esc_closes_chat` setting: `Esc` in the composer, with nothing left to
    /// cancel, closes the chat.
    pub esc_closes_chat: bool,
    /// The `rooms_bar` setting: with the room list out of sight, a line above the
    /// chat that leads back to it and shows the rooms with new notifications.
    pub rooms_bar: bool,
    /// The room the "newest room with notifications" key opened last, while it is
    /// still the open one: the key goes on from there (see `open_active_room`).
    active_from: Option<String>,
    /// False while the terminal window is in the background.
    window_focused: bool,
    /// The draft that was in the composer when an edit started.
    edit_stash: Option<String>,
    /// When we last told the room we are typing, if we did.
    typing_sent: Option<Instant>,
    pub typing_notices: bool,
    pub read_receipts: bool,
    /// No notifications for now, whatever the mode says.
    pub do_not_disturb: bool,
    /// The `window_title` setting: the unread count in the terminal's title (see
    /// `title`); the event loop applies changes to the terminal.
    pub window_title: bool,
    /// The title as last written; `None` before the first and after the terminal was
    /// handed back.
    title_written: Option<String>,
    /// What the app learned about where it runs, for the doctor's report.
    found: doctor::Found,
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
    /// For which rooms the homeserver is asked to preview links (off by default).
    pub link_previews: crate::config::LinkPreviews,
    /// What the homeserver said about the links on screen.
    pub previews: crate::previews::PreviewStore,
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
    /// The `notification_click_command` setting (see `focus`). `None` where no
    /// window is to be raised at all: the demo, and tests, which may well run
    /// inside a desktop that would obey.
    notification_click_command: Option<String>,
    /// Attachments being downloaded, and where finished ones went (by `mxc://` address).
    saving: std::collections::HashSet<String>,
    saved: std::collections::HashMap<String, PathBuf>,
    /// A large attachment the user was asked about once.
    confirm_large: Option<String>,
    /// Current colour scheme (`Dark` or `Light`), resolved at startup (portal, terminal
    /// query, `COLORFGBG`) and updated live via the desktop portal.
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
    /// The user asked for no colours (`NO_COLOR`): every theme is shown without them.
    no_color: bool,
    pub hits: HitMap,
    pub hover: Option<HitId>,
    dragging_splitter: bool,
    pub toast: Option<(String, Instant)>,
    pub toast_room: Option<(String, Instant)>,
    pub screen: Rect,
    pub should_quit: bool,
    pub suspend_requested: bool,
    /// Set by `Action::ComposeInEditor`: the run loop hands the terminal to the editor.
    pub editor_request: Option<crate::editor::Request>,
    pub dirty: bool,
    /// Pictures: the cache and the viewer.
    pub images: crate::images::ImageStore,
    /// The thread shown in place of the room's messages, and thread drafts.
    pub threads: thread::Threads,
    /// Files waiting in the composer to be sent.
    pub attach: attach::AttachState,
    /// What of the unsent text is in the store, to have it back after a restart.
    drafts: drafts::Drafts,
    /// The "new" line of the open room, and rooms marked as unread by hand.
    unread: unread::Unread,
    /// Composer completions and tracked mentions/room links.
    pub complete: complete::CompleteState,
    /// Joining, creating and leaving rooms.
    manage: rooms::ManageState,
    /// Active key bindings and preset.
    pub keymap: Keymap,
    /// Monotonic counter for active/debounced global search requests.
    search_gen: u64,
    /// Cross-room / event jump waiting for worker `/context` load.
    pending_jump: Option<global_search::PendingJump>,
    /// Matrix link or address passed on the command line, opened once after login/rooms arrive.
    startup_target: Option<String>,
    /// Since when the target's room has been missing from a list that counts as synced.
    startup_missing_since: Option<Instant>,
    /// How long such a room is waited for: right after a login the first lists are not
    /// complete, and a room the account is in would be taken for one it is not in.
    startup_room_wait: Duration,
    /// True once at least one `MatrixEvent::Rooms` list was received in this session.
    rooms_received: bool,
    /// Matrix user IDs currently ignored by this account (`m.ignored_user_list`).
    pub ignored_users: std::collections::BTreeSet<String>,
    /// Monotonic counter for active/debounced user directory searches.
    pub(super) user_search_gen: u64,
    /// Deadline timer start for debounced user directory lookups in `NewPopup`.
    pub(super) user_search_debounce: Option<std::time::Instant>,
    /// Monotonic counter for active/debounced public room directory searches.
    pub(super) dir_search_gen: u64,
    /// Deadline timer start for debounced public room directory searches in `NewPopup`.
    pub(super) dir_search_debounce: Option<std::time::Instant>,
    /// The account's sessions and the key backup, for their popups.
    pub devices: crate::devices::DevicesState,
    /// The notification keywords popup's watch and requests (and the demo's list).
    pub keywords: crate::keywords::KeywordsState,
    /// The messages saved for later and their reminders.
    pub bookmarks: crate::saved::SavedState,
    /// Custom emoji and stickers from image packs.
    pub emotes: emotes::EmoteState,
    /// Voice and audio played in here: the player, and the sound there is.
    pub(crate) audio: playback::AudioState,
    /// Transcripts and translations under messages, and the programs that make them.
    notes: notes::NotesState,
    /// Archiving a room and catching up on it: the requests, and the summary command.
    collect: collect::CollectState,
}

/// Hand-written: the app holds everything private there is (messages, drafts, ids,
/// names, paths, the clipboard's text). Only the shape of its state is shown; the
/// parts it shows have redacted `Debug`s of their own.
impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("rooms", &self.rooms.len())
            .field("active", &self.active)
            .field("chat_closed", &self.chat_closed)
            .field("focus", &self.focus)
            .field("sidebar", &self.sidebar)
            .field("timeline", &self.timeline)
            .field("composer", &self.composer)
            .field("compose_mode", &self.compose_mode)
            .field("help_open", &self.help_open)
            .field("modal", &self.modal)
            .field("connected", &self.matrix.is_some())
            .field("sync", &self.sync)
            .field("logged_in", &self.user_id.is_some())
            .field("own_profile", &self.own_profile)
            .field("generation", &self.generation)
            .field("window_focused", &self.window_focused)
            .field("edit_stash", &self.edit_stash.is_some())
            .field("pending_copy", &self.pending_copy.is_some())
            .field("window_title", &self.window_title)
            .field("rooms_bar", &self.rooms_bar)
            .field("active_from", &self.active_from.is_some())
            .field("title_written", &self.title_written)
            .field("found", &self.found)
            .field("notify_mode", &self.notify_mode)
            .field("pending_notices", &self.pending_notices.len())
            .field("link_previews", &self.link_previews)
            .field("previews", &self.previews)
            .field("spaces", &self.spaces.len())
            .field("space", &self.space_at)
            .field("logged_out", &self.logged_out)
            .field("saving", &self.saving.len())
            .field("saved", &self.saved.len())
            .field("color_scheme", &self.color_scheme)
            .field("theme_rev", &self.theme_rev)
            .field("timeline_rev", &self.timeline_rev)
            .field("row_cache", &self.row_cache)
            .field("verify", &self.verify)
            .field("toast", &self.toast.is_some())
            .field("screen", &self.screen)
            .field("editor_request", &self.editor_request.is_some())
            .field("images", &self.images)
            .field("threads", &self.threads)
            .field("attach", &self.attach)
            .field("drafts", &self.drafts)
            .field("unread", &self.unread)
            .field("complete", &self.complete)
            .field("manage", &self.manage)
            .field("pending_jump", &self.pending_jump)
            .field("startup_target", &self.startup_target.is_some())
            .field("ignored_users", &self.ignored_users.len())
            .field("devices", &self.devices)
            .field("keywords", &self.keywords)
            .field("bookmarks", &self.bookmarks)
            .field("emotes", &self.emotes)
            .field("audio", &self.audio)
            .field("notes", &self.notes)
            .field("collect", &self.collect)
            .finish_non_exhaustive()
    }
}

/// Shared 250 ms search debounce window used by directory & user lookups.
pub(super) const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);

/// Advances a search generation counter and clears any pending debounce timer,
/// returning `(next_generation, had_pending_debounce)`.
pub(super) fn bump_search_gen(counter: &mut u64, debounce: &mut Option<Instant>) -> (u64, bool) {
    *counter = counter.wrapping_add(1);
    (*counter, debounce.take().is_some())
}

/// Converts a search debounce start timestamp into a `tokio::time::Instant` deadline.
pub(super) fn search_debounce_deadline(debounce: Option<Instant>) -> Option<tokio::time::Instant> {
    debounce.map(|at| (at + SEARCH_DEBOUNCE).into())
}

/// Clears and returns `true` when a search debounce deadline has elapsed.
pub(super) fn take_due_search_debounce(debounce: &mut Option<Instant>) -> bool {
    if debounce.is_some_and(|at| at.elapsed() >= SEARCH_DEBOUNCE) {
        *debounce = None;
        true
    } else {
        false
    }
}

async fn wait_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

impl App {
    /// With `matrix` the app shows the account's rooms; without, built-in demo data.
    #[cfg(test)]
    pub fn new(config: &Config, themes_dir: PathBuf, matrix: Option<MatrixHandle>) -> Self {
        let mut app = Self::new_in_scheme(config, themes_dir, matrix, ColorScheme::Dark);
        // Tests may well run inside a desktop that would obey: no window is raised.
        app.notification_click_command = None;
        // Nor do they depend on the players and recorders the machine has, or on
        // running over SSH.
        app.audio = playback::AudioState::default();
        app
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
        app.no_color = crate::theme::no_color_wanted();
        if app.no_color {
            app.theme.strip_colors();
        }
        app.theme_names = Theme::available(&themes_dir);
        app.themes_dir = themes_dir;
        app.sidebar.width = config.sidebar_width;
        app.sidebar.hidden = config.sidebar_hidden;
        app.typing_notices = config.typing_notices;
        app.read_receipts = config.read_receipts;
        app.do_not_disturb = config.do_not_disturb;
        app.window_title = config.window_title;
        app.read_markers = config.read_markers;
        app.esc_closes_chat = config.esc_closes_chat;
        app.rooms_bar = config.rooms_bar;
        app.mouse = config.mouse;
        app.found.image_mode = config.images;
        app.found.secret_store = config.secret_store;
        app.found.ignored_settings = config.ignored.clone();
        app.download_dir = config.download_dir.clone();
        app.audio = playback::AudioState::new(&config.audio_player, &config.audio_recorder);
        app.notes = notes::NotesState::new(config);
        app.collect.set_summary_setting(&config.summary_command);
        app.notification_click_command = Some(config.notification_click_command.clone());
        app.room_previews = config.room_previews;
        app.link_previews = config.link_previews;
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
        // Our own setting names, nothing of the user's: fit for the log and the screen.
        let ignored = config.ignored_notice();
        if ignored.is_some() {
            tracing::warn!(settings = ?config.ignored, "values in the config not understood");
        }
        let (keymap, warnings) = Keymap::from_config(config);
        app.keymap = keymap;
        app.found.key_problems = warnings.len();
        for warning in &warnings {
            tracing::warn!(%warning, "invalid key binding in config");
        }
        // One line for both: there is room for one notice only.
        let keys = (!warnings.is_empty())
            .then(|| format!("Key bindings: {} problem(s), see the log", warnings.len()));
        let notices: Vec<String> = ignored.into_iter().chain(keys).collect();
        if !notices.is_empty() {
            app.show_toast(notices.join(" · "));
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
            own_profile: None,
            user_info_requests: 0,
            recovery_prompted: false,
            generation: 0,
            next_gen: 0,
            auto_opened: true,
            chat_closed: false,
            esc_closes_chat: true,
            rooms_bar: true,
            active_from: None,
            window_focused: true,
            edit_stash: None,
            typing_sent: None,
            typing_notices: true,
            read_receipts: true,
            do_not_disturb: false,
            window_title: true,
            title_written: None,
            found: Default::default(),
            read_markers: true,
            mouse: true,
            pending_copy: None,
            notify_mode: crate::notify::NotifyMode::Off,
            notify_mode_on: crate::notify::NotifyMode::Auto,
            notification_preview: true,
            count_notices: Vec::new(),
            detailed_notices: Default::default(),
            room_previews: false,
            link_previews: crate::config::LinkPreviews::Off,
            previews: crate::previews::PreviewStore::default(),
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
            notification_click_command: None,
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
            no_color: false,
            hits: HitMap::default(),
            hover: None,
            dragging_splitter: false,
            toast: None,
            toast_room: None,
            screen: Rect::default(),
            should_quit: false,
            suspend_requested: false,
            editor_request: None,
            dirty: true,
            images: crate::images::ImageStore::new(crate::images::Graphics::halfblocks()),
            threads: Default::default(),
            attach: Default::default(),
            drafts: Default::default(),
            unread: Default::default(),
            complete: Default::default(),
            manage: Default::default(),
            keymap: Keymap::default(),
            search_gen: 0,
            pending_jump: None,
            startup_target: None,
            startup_missing_since: None,
            startup_room_wait: if cfg!(test) {
                Duration::ZERO
            } else {
                Duration::from_secs(4)
            },
            rooms_received: false,
            ignored_users: std::collections::BTreeSet::new(),
            user_search_gen: 0,
            user_search_debounce: None,
            dir_search_gen: 0,
            dir_search_debounce: None,
            devices: Default::default(),
            keywords: Default::default(),
            bookmarks: Default::default(),
            emotes: Default::default(),
            audio: Default::default(),
            notes: Default::default(),
            collect: Default::default(),
        }
    }

    // ----- derived views -------------------------------------------------------------

    /// Rooms that pass the filter, in the order the sidebar shows them.
    pub fn visible_rooms(&self) -> Vec<usize> {
        let needle = self.sidebar.filter.to_lowercase();
        let open = self.open_index();
        RoomKind::SECTIONS
            .iter()
            .flat_map(|kind| {
                self.rooms
                    .iter()
                    .enumerate()
                    .filter(|(_, room)| room.kind == *kind)
                    // Invitations and pending knocks are shown in every space: they belong to none yet.
                    .filter(|(_, room)| {
                        self.space.is_none()
                            || room.is_pending()
                            || self.space_rooms.contains(&room.id)
                    })
                    // The open room stays in the list whatever the quick filters say.
                    .filter(|(i, room)| {
                        Some(*i) == open
                            || ((!self.sidebar.unread_only || room.shows_unread())
                                && (!self.sidebar.people_only || room.direct))
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

    /// Where the open room is in `rooms`. `None` when there are no rooms (before the
    /// first sync) and while no chat is open. Everything that means "the open room"
    /// asks here (or `room`), never `active` itself.
    pub fn open_index(&self) -> Option<usize> {
        (!self.chat_closed && self.active < self.rooms.len()).then_some(self.active)
    }

    /// The open room. `None` when there are no rooms (before the first sync) and
    /// while no chat is open.
    pub fn room(&self) -> Option<&Room> {
        self.open_index().and_then(|index| self.rooms.get(index))
    }

    /// The user closed the chat and has not opened one since: the chat area shows no
    /// room, and the room list has the focus.
    pub fn chat_closed(&self) -> bool {
        self.chat_closed
    }

    fn messages(&self) -> &[Message] {
        self.room().map_or(&[], |room| room.messages.as_slice())
    }

    fn messages_mut(&mut self) -> Option<&mut Vec<Message>> {
        self.timeline_rev += 1;
        let open = self.open_index()?;
        self.rooms.get_mut(open).map(|room| &mut room.messages)
    }

    // ----- state changes -------------------------------------------------------------

    pub fn show_toast(&mut self, text: impl Into<String>) {
        // Toasts often quote what a server or another person said.
        let text = crate::text::sanitize_line(&text.into());
        self.toast = Some((text, Instant::now()));
        self.toast_room = None;
    }

    /// Called when something in the config or themes directory changed (or on
    /// request): follows a changed `theme` setting and loads the wanted theme again.
    /// Quiet unless a theme file is broken: a desktop recolouring itself should not
    /// pop up messages, and a theme that a generator has yet to write is just awaited.
    pub fn reload_theme(&mut self) {
        self.dirty = true;
        self.theme_names = Theme::available(&self.themes_dir);
        let loaded = self.config_file.as_deref().map(Config::load);
        // What the doctor says about the file follows it; the settings themselves
        // (but the theme) are read at start only.
        match &loaded {
            Some(Ok(config)) => {
                self.found.config_broken = false;
                self.found.ignored_settings = config.ignored.clone();
                let problems = Keymap::from_config(config).1.len();
                if problems != self.found.key_problems {
                    tracing::info!(problems, "key bindings in the config read again");
                    self.found.key_problems = problems;
                }
            }
            Some(Err(_)) => self.found.config_broken = true,
            None => {}
        }
        if let Some(Ok(config)) = loaded {
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
        if self.has_theme_pair()
            && let Some(Modal::Settings(menu)) = &mut self.modal
        {
            menu.show_theme_pair = true;
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
                if self.no_color {
                    theme.strip_colors();
                }
                self.theme = theme;
                self.theme_rev += 1;
            }
            Err(err) => {
                tracing::warn!(theme = name, why = %crate::logsafe::kind_any(&err), "could not load theme");
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

    fn toggle_theme_pairing(&mut self) {
        let on = self.has_theme_pair()
            || matches!(self.modal, Some(Modal::Settings(ref menu)) if menu.show_theme_pair);
        if on {
            self.theme_dark = None;
            self.theme_light = None;
            self.config_theme_dark = None;
            self.config_theme_light = None;
            self.remove_setting("theme_dark");
            self.remove_setting("theme_light");
            let desired = self.desired_theme();
            if desired != self.wanted_theme {
                self.set_theme(&desired);
            }
            if let Some(Modal::Settings(menu)) = &mut self.modal {
                menu.show_theme_pair = false;
            }
        } else if let Some(Modal::Settings(menu)) = &mut self.modal {
            menu.show_theme_pair = true;
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
            tracing::warn!(why = %crate::logsafe::kind_any(&err), key, "could not save setting");
            self.show_toast(format!("Could not save the setting: {err:#}"));
        }
    }

    /// Removes one setting from the configuration file (there is none in demo mode).
    fn remove_setting(&mut self, key: &str) {
        let Some(file) = &self.config_file else {
            return;
        };
        if let Err(err) = crate::config::remove_setting(file, key) {
            tracing::warn!(why = %crate::logsafe::kind_any(&err), key, "could not remove setting");
            self.show_toast(format!("Could not save the setting: {err:#}"));
        }
    }

    /// What the settings menu shows next to a row.
    pub fn setting_value(&self, setting: Setting) -> String {
        let switch = |on: bool| if on { "on" } else { "off" }.to_owned();
        match setting {
            Setting::Theme => self.wanted_theme.clone(),
            Setting::ThemePairing => switch(
                self.has_theme_pair()
                    || matches!(self.modal, Some(Modal::Settings(ref menu)) if menu.show_theme_pair),
            ),
            Setting::ThemeDark => self.theme_dark.as_deref().unwrap_or("(unset)").to_owned(),
            Setting::ThemeLight => self.theme_light.as_deref().unwrap_or("(unset)").to_owned(),
            Setting::SidebarWidth => self.sidebar.width.to_string(),
            Setting::RoomPreviews => switch(self.room_previews),
            Setting::LinkPreviews => self.link_previews.label().to_owned(),
            Setting::Mouse => switch(self.mouse),
            Setting::EscClosesChat => switch(self.esc_closes_chat),
            Setting::RoomsBar => switch(self.rooms_bar),
            Setting::TypingNotices => switch(self.typing_notices),
            Setting::ReadReceipts => switch(self.read_receipts),
            Setting::ReadMarkers => switch(self.read_markers),
            Setting::Notifications => switch(self.notify_mode != crate::notify::NotifyMode::Off),
            Setting::DoNotDisturb => switch(self.do_not_disturb),
            Setting::WindowTitle => switch(self.window_title),
            Setting::DownloadDir => crate::files::display(&self.download_dir()),
            Setting::AudioPlayer => self.audio.setting_text(),
            Setting::AudioRecorder => self.audio.recorder_setting_text(),
            Setting::TranscribeCommand => self.notes.transcribe_text(),
            Setting::TranscribeModel => match self.notes.transcribe_model.trim() {
                "" => "(not set)".to_owned(),
                model => crate::text::sanitize_line(model),
            },
            Setting::TranslateCommand => self.notes.translate_text(),
            Setting::TranslateLanguage => crate::notes::language(&self.notes.translate_language),
            Setting::KeepNotes => switch(self.notes.keep),
            Setting::SummaryCommand => self.summary_setting_text(),
            Setting::Keys => self.keymap.preset_summary(),
            Setting::Profile => self.own_profile_summary(),
            Setting::Encryption => crate::verify::setting_text(self.verify.security).to_owned(),
            Setting::Sessions | Setting::Keywords | Setting::Doctor => "Enter to view".to_owned(),
            Setting::KeyBackup => {
                crate::devices::backup_setting_text(self.verify.security).to_owned()
            }
            Setting::IgnoredUsers => match self.ignored_users.len() {
                0 => "None".to_owned(),
                1 => "1 user".to_owned(),
                n => format!("{n} users"),
            },
            Setting::Logout => self.user_id.clone().unwrap_or_default(),
        }
    }

    /// Changes a setting from the menu: `delta` is -1 or 1 for ◂ ▸, 0 for Enter or a
    /// click on the row.
    fn change_setting(&mut self, setting: Setting, delta: i8) {
        match setting {
            Setting::Theme => self.step_theme(if delta < 0 { -1 } else { 1 }),
            Setting::ThemePairing => self.toggle_theme_pairing(),
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
            Setting::EscClosesChat => {
                self.esc_closes_chat = !self.esc_closes_chat;
                self.save_setting("esc_closes_chat", self.esc_closes_chat);
                // Switched off, there is no such thing as no chat open: the room
                // that was left is back (with no rooms at all, the first one that
                // comes opens as at the start).
                if !self.esc_closes_chat && self.chat_closed {
                    if self.active < self.rooms.len() {
                        // `show_room`: the list follows, out of a space the room is
                        // not in.
                        self.show_room(self.active);
                    } else {
                        self.chat_closed = false;
                    }
                }
            }
            Setting::RoomsBar => {
                self.rooms_bar = !self.rooms_bar;
                self.save_setting("rooms_bar", self.rooms_bar);
                // Pictures are placed by row: the chat moves up or down a line.
                self.repaint = true;
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
            Setting::DoNotDisturb => {
                self.do_not_disturb = !self.do_not_disturb;
                self.save_setting("do_not_disturb", self.do_not_disturb);
                // What was waiting to be shown is not shown later either.
                self.pending_notices.clear();
                self.count_notices.clear();
                self.show_toast(if self.do_not_disturb {
                    "Do not disturb: no notifications"
                } else {
                    "Notifications are back on"
                });
            }
            Setting::WindowTitle => {
                self.window_title = !self.window_title;
                self.save_setting("window_title", self.window_title);
                // Off, the event loop puts the old title back; on again, ours is
                // written at once.
                self.title_written = None;
            }
            Setting::ReadMarkers => {
                self.read_markers = !self.read_markers;
                self.save_setting("read_markers", self.read_markers);
                // The rows are laid out afresh.
                self.timeline_rev += 1;
            }
            Setting::LinkPreviews => {
                self.link_previews = self.link_previews.step(if delta < 0 { -1 } else { 1 });
                self.save_setting("link_previews", self.link_previews.name());
                // The rows are laid out afresh (the previews appear or go).
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
                    menu.editing = Some(LineEdit::from(self.download_dir.as_str()));
                }
            }
            Setting::AudioPlayer
            | Setting::TranscribeCommand
            | Setting::TranscribeModel
            | Setting::TranslateCommand
            | Setting::TranslateLanguage => {
                let text = self.setting_text(setting);
                if delta == 0
                    && let Some(Modal::Settings(menu)) = &mut self.modal
                {
                    menu.editing = Some(LineEdit::from(text.as_str()));
                }
            }
            Setting::AudioRecorder => {
                if delta == 0
                    && let Some(Modal::Settings(menu)) = &mut self.modal
                {
                    menu.editing = Some(LineEdit::from(self.audio.recorder_setting.as_str()));
                }
            }
            Setting::KeepNotes => self.toggle_keep_notes(),
            Setting::SummaryCommand => {
                if delta == 0
                    && let Some(Modal::Settings(menu)) = &mut self.modal
                {
                    menu.editing = Some(LineEdit::from(self.collect.summary_setting()));
                }
            }
            Setting::Keys => {
                if delta == 0 {
                    self.open_keybinds_editor();
                }
            }
            Setting::Doctor => {
                if delta == 0 {
                    // In the settings' place; any other popup stays.
                    if matches!(self.modal, Some(Modal::Settings(_))) {
                        self.modal = None;
                    }
                    self.open_doctor();
                }
            }
            Setting::Profile => {
                if delta == 0 {
                    self.modal = None;
                    self.open_own_profile();
                }
            }
            Setting::Encryption => {
                if delta == 0 {
                    self.modal = None;
                    self.start_verification();
                }
            }
            Setting::IgnoredUsers => {
                if delta == 0 {
                    self.open_ignored_users();
                }
            }
            Setting::Sessions => {
                if delta == 0 {
                    self.open_sessions();
                }
            }
            Setting::Keywords => {
                if delta == 0 {
                    self.open_keywords();
                }
            }
            Setting::KeyBackup => {
                if delta == 0 {
                    self.open_keys();
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
            && !self.timeline.detached
            && self.room().is_some_and(|room| room.id == notice.room_id)
            && self.threads.open_root() == notice.thread.as_deref();
        if self.notify_mode == crate::notify::NotifyMode::Off
            || self.do_not_disturb
            || watching
            || self.logged_out
        {
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

    /// The user clicked the desktop notification of a room: that room is opened, as
    /// if picked in the room list. Not while a question is open that has to be
    /// answered first (logging in, the recovery key, a device verification), and not
    /// for a room that is gone; what was merely being looked at (a menu, the command
    /// palette, the settings, a picture) makes way.
    ///
    /// A reminder's notification carries its message (`event`): that is where to go,
    /// if it is saved still. The notification that stood for several reminders
    /// names no room, and opens the list of saved messages.
    fn on_notice_click(&mut self, room_id: &str, event: Option<(String, Option<String>)>) {
        if self.logged_out
            || matches!(
                self.modal,
                Some(Modal::Login(_) | Modal::Recovery(_) | Modal::Verify(_))
            )
        {
            return;
        }
        let several_reminders = room_id.is_empty();
        let index = (!several_reminders)
            .then(|| self.rooms.iter().position(|room| room.id == room_id))
            .flatten();
        if index.is_none() && !several_reminders {
            return;
        }
        // The click was on the desktop, maybe with another window in front: ask for
        // ours to be shown.
        if let Some(command) = &self.notification_click_command {
            crate::focus::raise_window(command);
        }
        // A jump to a message that is still on its way would take the view
        // somewhere else afterwards.
        self.cancel_pending_jump();
        match self.modal {
            Some(Modal::Search(_)) => self.close_search(),
            Some(Modal::GlobalSearch(_)) => self.close_global_search(),
            Some(Modal::New(_) | Modal::RoomInfo(_)) => self.close_manage(),
            _ => self.modal = None,
        }
        self.help_open = false;
        self.images.close_viewer();
        if let Some((event_id, _)) = &event
            && self.reminder_clicked(room_id, event_id)
        {
            return;
        }
        let Some(index) = index else {
            self.open_saved_list();
            return;
        };
        // Already there: nothing moves, a thread stays open, the place in the
        // messages is kept. Only a view of some older part of the room (after a
        // jump to a message) goes back to the newest messages, where the news is.
        if Some(index) == self.open_index() {
            if self.timeline.detached {
                self.return_to_live();
            }
            return;
        }
        // The room list shows where we went, also when a space hid that room.
        if self.space.is_some() && !self.space_rooms.contains(room_id) {
            self.select_space(None);
        }
        self.open_room(index);
        if let Some(at) = self.visible_rooms().iter().position(|i| *i == index) {
            self.sidebar.cursor = at;
            self.sidebar.reveal_cursor = true;
        }
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
        // Never download media for unaccepted invitations or pending knocks.
        let is_pending = room.is_some_and(|r| r.is_pending());
        let avatar = if is_pending {
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
        let due = std::mem::take(&mut self.bookmarks.notices);
        let mut reminders = Vec::with_capacity(due.len());
        for reminder in due {
            // The one that stands for several is of no room.
            let icon = if reminder.count > 1 {
                crate::notify::NotifyIcon::Generic
            } else {
                self.resolve_notice_icon(&reminder.notice)
            };
            reminders.push((reminder, icon));
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
            reminders,
        }
    }

    /// Nothing of a previous session may linger behind the login form.
    pub(crate) fn reset_session(&mut self) {
        // The sound belongs to a room of the session, and so does a recording.
        self.stop_audio();
        // So do the transcripts and translations: they are the account's messages.
        self.forget_notes();
        // So do a summary that is being made and an archive that is being written.
        self.forget_collections();
        self.cancel_recording();
        self.rooms.clear();
        self.active = 0;
        // The next session starts as every one does: its first room opens by itself.
        self.chat_closed = false;
        self.active_from = None;
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
        self.sidebar.unread_only = false;
        self.sidebar.people_only = false;
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
        self.previews.clear();
        self.attach.clear();
        self.drafts = Default::default();
        self.unread = Default::default();
        self.complete.reset_session();
        self.manage = Default::default();
        self.own_profile = None;
        self.pending_jump = None;
        self.rooms_received = false;
        self.search_gen += 1;
        self.ignored_users.clear();
        bump_search_gen(&mut self.user_search_gen, &mut self.user_search_debounce);
        bump_search_gen(&mut self.dir_search_gen, &mut self.dir_search_debounce);
        self.devices = Default::default();
        // The worker's watch ended with the session.
        self.keywords.watch = None;
        // No reminder comes due over the login form, and what is saved there belongs
        // to the account that left: the list is read again when a session is up.
        self.bookmarks.store = Default::default();
        self.bookmarks.waiting.clear();
        self.bookmarks.notices.clear();
        self.bookmarks.confirm = None;
        self.reset_emotes();
        if let Some(Modal::New(popup)) = &mut self.modal {
            popup.clear_user_search();
            popup.clear_dir_search();
        }
        if matches!(
            self.modal,
            Some(
                Modal::GlobalSearch(_)
                    | Modal::IgnoredUsers(_)
                    | Modal::Report(_)
                    | Modal::Pinned(_)
                    | Modal::ReasonConfirm(_)
                    | Modal::Sessions(_)
                    | Modal::Keys(_)
                    | Modal::UserInfo(_)
                    | Modal::OwnProfile(_)
                    | Modal::PollForm(_)
                    | Modal::PollVote(_)
                    | Modal::Stickers(_)
                    | Modal::Emoji(_)
                    | Modal::Details(_)
                    | Modal::Threads(_)
                    | Modal::Keywords(_)
                    | Modal::Saved(_)
            )
        ) {
            self.modal = None;
        }
        self.toast = None;
        self.toast_room = None;
        self.sync = Some(SyncStatus::Stopped);
    }

    fn set_focus(&mut self, pane: Pane) {
        if self.chat_closed {
            // No chat is open: there are no messages and no composer to go to, and
            // the list's cursor stays where the user has it.
            self.focus = Pane::Sidebar;
            self.timeline.selected = None;
            self.sidebar.reveal_cursor = true;
            self.sync_completion();
            return;
        }
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
                let open = self.open_index();
                if let Some(at) = self.visible_rooms().iter().position(|i| Some(*i) == open) {
                    self.sidebar.cursor = at;
                }
                self.sidebar.reveal_cursor = true;
            }
            Pane::Composer => self.timeline.selected = None,
        }
        self.sync_completion();
    }

    /// Makes the room at `index` the open one. `opened`: the user opens the room,
    /// which starts its "new" line afresh also if it is the open one already; a
    /// jump to a message leaves the line of the room it stays in alone.
    pub(super) fn switch_room_state(&mut self, index: usize, opened: bool) {
        if index >= self.rooms.len() {
            return;
        }
        self.auto_opened = false;
        // Whatever opens a room: the key for the newest active room starts afresh
        // (it notes its own room after this).
        self.active_from = None;
        // With no chat open every room is another one, the one that was left too.
        let other = Some(index) != self.open_index();
        let fresh = opened || other;
        if other {
            self.leave_open_room();
            let new_id = self.rooms[index].id.clone();
            let draft = std::mem::take(&mut self.rooms[index].draft);
            self.composer.set_text(&draft);
            self.restore_room_marks(&new_id);
        }
        self.active = index;
        self.chat_closed = false;
        // Before the count is cleared: it helps to place the "new" line.
        if fresh {
            self.begin_unread_line();
        }
        self.rooms[index].unread = 0;
        self.rooms[index].bold = false;
        self.rooms[index].mention = false;
        // Opening a room that was marked as unread by hand is reading it.
        self.clear_marked_unread(index);
        self.sidebar.filter.clear();
        self.request_members_for_open_room();
    }

    /// What is being written in the open room stays with it when the room is left,
    /// for another room or for none: the typing notice is taken back, the unsent text
    /// goes to the store and waits with the room, an edit is given up.
    fn leave_open_room(&mut self) {
        // What plays belongs to the room. A recording ends here and waits in the
        // conversation it was begun in.
        self.stop_audio();
        self.stop_recording();
        if self.typing_sent.take().is_some()
            && let Some(room_id) = self.room_id()
        {
            self.send_matrix(MatrixCmd::Typing {
                room_id,
                typing: false,
            });
        }
        // What is unsent here goes to the store, in case we do not come back.
        self.save_draft();
        // An edit belongs to its room; a plain draft waits there until we come back.
        self.cancel_compose();
        let draft = self.composer.take();
        if let Some(room) = self.open_index().and_then(|open| self.rooms.get_mut(open)) {
            let old_id = room.id.clone();
            room.draft = draft;
            self.save_room_marks(&old_id);
        }
    }

    /// Closes the chat: no room is open afterwards, and the room list has the focus
    /// with its cursor on the room that was left. What was being written there waits
    /// for the return as after a room switch. Nothing of the room is on screen from
    /// here on, so nothing in it is read (no receipt goes out, and a message there
    /// notifies like one anywhere else) until a room is opened again.
    fn close_chat(&mut self) {
        let Some(left) = self.open_index() else {
            return;
        };
        self.cancel_pending_jump();
        // A thread belongs to its room.
        self.leave_thread();
        self.leave_open_room();
        self.chat_closed = true;
        self.auto_opened = false;
        self.active_from = None;
        self.timeline = TimelineState::default();
        self.row_cache = None;
        self.timeline_rev += 1;
        self.no_unread_line();
        // A new generation: late updates for the room are dropped from here on. The
        // worker stops making them.
        self.generation = self.allocate_gen();
        self.send_matrix(MatrixCmd::CloseRoom);
        // The room list is shown also where it was put away or the window is narrow:
        // with the focus, it is the one pane there is.
        self.set_focus(Pane::Sidebar);
        if let Some(at) = self.visible_rooms().iter().position(|i| *i == left) {
            self.sidebar.cursor = at;
        }
        // Everything moves: the chat makes way.
        self.repaint = true;
    }

    fn open_room(&mut self, index: usize) {
        if index >= self.rooms.len() {
            return;
        }
        self.cancel_pending_jump();
        // A thread belongs to its room; picking a room (even the same one) leaves it.
        self.leave_thread();
        self.switch_room_state(index, true);
        self.timeline = TimelineState::default();
        self.row_cache = None;
        self.timeline_rev += 1;
        self.request_timeline();
        self.set_focus(Pane::Composer);
    }

    /// The display label of the space at `at` ("Parent › Child" for sub-spaces).
    pub(crate) fn space_display_name(&self, at: usize) -> Option<String> {
        let space = self.spaces.get(at)?;
        if space.level == 0 {
            return Some(space.name.clone());
        }
        match self.spaces[..at].iter().rev().find(|s| s.level == 0) {
            Some(parent) => Some(format!("{} › {}", parent.name, space.name)),
            None => Some(space.name.clone()),
        }
    }

    /// The name of the selected space, for the room list's header. A space inside
    /// another is shown with its parent, so it is clear where it sits.
    pub fn space_name(&self) -> Option<String> {
        self.space_display_name(self.space_at?)
    }

    fn space_room_ids(&self, at: usize) -> std::collections::HashSet<&str> {
        let mut ids = std::collections::HashSet::new();
        let Some(space) = self.spaces.get(at) else {
            return ids;
        };
        ids.extend(space.rooms.iter().map(String::as_str));
        // A top-level space lists only what is directly in it; the rooms of the
        // spaces inside it follow in the list and belong to the view as well,
        // deduplicated so shared rooms inside the tree count once.
        if space.level == 0 {
            for inner in self.spaces[at + 1..].iter().take_while(|s| s.level > 0) {
                ids.extend(inner.rooms.iter().map(String::as_str));
            }
        }
        ids
    }

    fn aggregate_space_unread(&self, include: impl Fn(&Room) -> bool) -> crate::model::SpaceUnread {
        let mut out = crate::model::SpaceUnread::default();
        for room in &self.rooms {
            if room.muted || !include(room) {
                continue;
            }
            if room.shows_unread() {
                out.unread = true;
            }
            out.notifications = out.notifications.saturating_add(u64::from(room.unread));
            out.mention |= room.mention;
        }
        out
    }

    /// Aggregated unread state across all non-muted rooms.
    pub fn all_spaces_unread(&self) -> crate::model::SpaceUnread {
        self.aggregate_space_unread(|_| true)
    }

    /// Unread state for the space at `at` (sub-spaces count into their level-0 parent
    /// without internal double-counting; muted rooms excluded).
    pub fn space_unread(&self, at: usize) -> crate::model::SpaceUnread {
        let ids = self.space_room_ids(at);
        self.aggregate_space_unread(|room| ids.contains(room.id.as_str()))
    }

    /// Aggregated unread state for the currently selected space, if any.
    pub fn selected_space_unread(&self) -> Option<crate::model::SpaceUnread> {
        self.space_at.map(|at| self.space_unread(at))
    }

    /// Unread state for rooms in other spaces outside the currently selected space
    /// (muted rooms excluded). Empty when "All rooms" is active.
    pub fn other_spaces_unread(&self) -> crate::model::SpaceUnread {
        if self.space_at.is_none() {
            return crate::model::SpaceUnread::default();
        }
        let mut other_ids = std::collections::HashSet::new();
        for space in &self.spaces {
            for id in &space.rooms {
                if !self.space_rooms.contains(id.as_str()) {
                    other_ids.insert(id.as_str());
                }
            }
        }
        self.aggregate_space_unread(|room| other_ids.contains(room.id.as_str()))
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
        self.space_rooms = self
            .space_room_ids(at)
            .into_iter()
            .map(str::to_owned)
            .collect();
        self.space = Some(self.spaces[at].id.clone());
        self.space_at = Some(at);
    }

    fn toggle_sidebar_filter(&mut self, unread: bool) {
        let cursor_id = self.cursor_room().map(|r| self.rooms[r].id.clone());
        if unread {
            self.sidebar.unread_only = !self.sidebar.unread_only;
        } else {
            self.sidebar.people_only = !self.sidebar.people_only;
        }
        let visible = self.visible_rooms();
        if let Some(id) = cursor_id
            && let Some(pos) = visible.iter().position(|&i| self.rooms[i].id == id)
        {
            self.sidebar.cursor = pos;
        } else {
            self.sidebar.cursor = self.sidebar.cursor.min(visible.len().saturating_sub(1));
        }
        self.sidebar.reveal_cursor = true;
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
        let shown_listed = match &self.modal {
            Some(Modal::RoomInfo(info)) => self.spaces.iter().any(|s| s.id == info.room_id),
            _ => false,
        };
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
        // First the new selection, which an open room info then follows.
        self.select_pending_space();
        self.follow_spaces(shown_listed);
    }

    /// Ctrl-↑/↓: opens the neighbouring room in the list, wrapping at the ends, and
    /// leaves the focus where it was.
    fn step_room(&mut self, delta: isize) {
        let rooms = self.visible_rooms();
        if rooms.is_empty() {
            return;
        }
        // With no chat open the list's cursor is what there is to step from.
        let from = match self.open_index() {
            Some(open) => rooms.iter().position(|index| *index == open),
            None => Some(self.sidebar.cursor.min(rooms.len() - 1)),
        };
        let next = match from {
            Some(at) => (at as isize + delta).rem_euclid(rooms.len() as isize) as usize,
            None => 0,
        };
        // The focus the user chose stays; the one the closed chat left in the room
        // list was not chosen, and the room that opens is to be seen.
        let focus = (!self.chat_closed).then_some(self.focus);
        let opened = rooms[next];
        self.open_room(opened);
        // The list follows, so the open room is always in view.
        self.sidebar.cursor = self
            .visible_rooms()
            .iter()
            .position(|index| *index == opened)
            .unwrap_or(0);
        self.sidebar.reveal_cursor = true;
        if let Some(focus) = focus
            && focus != Pane::Composer
        {
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
        // Nothing is sent from under a recording: it is ended first, on purpose.
        if self.recording_refuses_send() {
            return;
        }
        // A room command does what it says (and waits while a message is edited).
        if self.room_command() {
            return;
        }
        // An `/upload` command attaches; with files waiting, Enter sends those. An
        // edit comes first: its text is a message, and files wait until it is done.
        if !matches!(self.compose_mode, ComposeMode::Edit(_))
            && (self.upload_command()
                || self.poll_command()
                || self.sticker_command()
                || self.send_attachments())
        {
            return;
        }
        let Some(room_id) = self.room().map(|room| room.id.clone()) else {
            return;
        };
        if self.composer.text().trim().is_empty() {
            return;
        }
        if let Some(hint) = self
            .room()
            .filter(|room| room.is_pending())
            .map(|room| room.kind.pending_hint())
        {
            self.show_toast(hint);
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
            // Whoever writes in a room has read it.
            if let Some(open) = self.open_index() {
                self.clear_marked_unread(open);
            }
            self.composer.take();
            self.clear_completion_marks();
            if matches!(self.compose_mode, ComposeMode::Edit(_)) {
                // The draft that waited behind the edit is back, and still unsent.
                self.end_edit();
            } else {
                self.forget_sent_draft();
            }
            self.compose_mode = ComposeMode::Normal;
            if self.timeline.detached {
                self.return_to_live();
            } else {
                self.timeline.scroll = 0;
            }
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
                .is_some_and(|room| !room.is_pending());
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
    /// Opens room `index` of the list, leaving a space that does not hold it, with the
    /// list's cursor on it.
    pub(super) fn show_room(&mut self, index: usize) {
        let Some(room_id) = self.rooms.get(index).map(|room| room.id.clone()) else {
            return;
        };
        if self.space.is_some() && !self.space_rooms.contains(&room_id) {
            self.select_space(None);
        }
        self.open_room(index);
        if let Some(at) = self.visible_rooms().iter().position(|i| *i == index) {
            self.sidebar.cursor = at;
            self.sidebar.reveal_cursor = true;
        }
    }

    pub(super) fn find_joined_room(&self, target: &str) -> Option<usize> {
        self.rooms.iter().position(|room| {
            !room.is_pending() && (room.id == target || room.alias.as_deref() == Some(target))
        })
    }

    fn open_link(&mut self, message: usize, n: usize) -> bool {
        let link = self.messages().get(message).and_then(|m| {
            crate::rich::Rich::of(m.html.as_deref(), &m.body)
                .links
                .into_iter()
                .nth(n)
        });
        let Some(url) = link else {
            return false;
        };
        if !self.try_open_matrix_link(&url) {
            if self.matrix.is_some() {
                open_in_browser(&url);
            }
            self.show_toast(format!("Opening {url}"));
        }
        true
    }

    fn try_open_matrix_link(&mut self, url: &str) -> bool {
        if !crate::manage::is_matrix_link(url) {
            return false;
        }
        if matches!(self.modal, Some(Modal::Search(_))) {
            self.close_search();
        }
        match crate::manage::parse_address(url) {
            Ok(crate::manage::Address::Event { room, event, .. }) => {
                if let Some(index) = self.find_joined_room(&room) {
                    let room_id = self.rooms[index].id.clone();
                    self.jump_to_event(room_id, event, None);
                } else {
                    self.show_toast("Join this room before jumping to a message in it");
                }
            }
            Ok(crate::manage::Address::Room { target, via }) => {
                if let Some(index) = self.find_joined_room(&target) {
                    self.show_room(index);
                } else {
                    self.open_room_preview(&target, via);
                }
            }
            Ok(crate::manage::Address::User(user_id)) => {
                let room_id = self.room_id();
                self.open_user_info(user_id, room_id, None);
            }
            Err(reason) => self.show_toast(reason),
        }
        true
    }

    fn jump_to_reply(&mut self, index: usize) {
        let Some(message) = self.messages().get(index) else {
            return;
        };
        let Some(event_id) = message.reply_event.clone() else {
            let reason = if message.reply_to.is_some() {
                "Original message is not available"
            } else {
                "That message is not a reply"
            };
            self.show_toast(reason);
            return;
        };
        let Some(room_id) = self.room_id() else {
            return;
        };
        let thread_root = if self.thread_open()
            && global_search::find_target_message(
                self.messages(),
                &event_id,
                self.matrix.is_none(),
                true,
            )
            .is_some()
        {
            self.thread_key().map(str::to_owned)
        } else {
            None
        };
        self.jump_to_event(room_id, event_id, thread_root);
    }

    /// Leaves a detached `/context` slice and reloads the live timeline for the active room
    /// (or open thread).
    pub(super) fn return_to_live(&mut self) {
        self.cancel_pending_jump();
        let keep_selection = (self.focus == Pane::Timeline)
            .then_some(self.timeline.selected)
            .flatten();
        self.timeline = TimelineState::default();
        self.timeline.selected = keep_selection;
        self.row_cache = None;
        self.timeline_rev += 1;
        self.repaint = true;
        if let Some(root) = self.threads.open_root().map(str::to_owned) {
            self.generation = self.allocate_gen();
            if let Some(room_id) = self.room_id() {
                self.send_matrix(MatrixCmd::OpenThread {
                    room_id,
                    root,
                    generation: self.generation,
                });
            }
        } else {
            self.request_timeline();
        }
    }

    /// Whether the room of the start-up target can be taken as one the account is not
    /// in: at once without a server, else when it stayed missing for a while after the
    /// lists counted as synced (the tick asks again).
    fn startup_room_is_missing(&mut self) -> bool {
        if self.matrix.is_none() {
            return true;
        }
        if self.sync != Some(SyncStatus::Synced) {
            self.startup_missing_since = None;
            return false;
        }
        let since = *self.startup_missing_since.get_or_insert_with(Instant::now);
        since.elapsed() >= self.startup_room_wait
    }

    /// Opens `startup_target` once login/rooms/sync conditions allow it.
    pub(super) fn maybe_open_startup_target(&mut self) {
        let Some(raw) = self.startup_target.as_deref() else {
            return;
        };
        if self.matrix.is_some()
            && (self.logged_out
                || self.user_id.is_none()
                || !self.rooms_received
                || self.modal.is_some())
        {
            // The wait for a missing room does not run behind a login or another
            // popup: it starts again when the way is clear.
            self.startup_missing_since = None;
            return;
        }
        let parsed = crate::manage::parse_address(raw);
        match parsed {
            Err(reason) => {
                self.startup_target = None;
                self.show_toast(reason);
            }
            Ok(crate::manage::Address::Event { room, event, .. }) => {
                if let Some(index) = self.find_joined_room(&room) {
                    self.startup_target = None;
                    let room_id = self.rooms[index].id.clone();
                    self.jump_to_event(room_id, event, None);
                } else if self.startup_room_is_missing() {
                    self.startup_target = None;
                    self.show_toast("Join this room before jumping to a message in it");
                }
            }
            Ok(crate::manage::Address::Room { target, via }) => {
                if let Some(index) = self.find_joined_room(&target) {
                    self.startup_target = None;
                    self.show_room(index);
                } else if self.startup_room_is_missing() {
                    self.startup_target = None;
                    self.open_room_preview(&target, via);
                }
            }
            Ok(crate::manage::Address::User(user_id)) => {
                self.startup_target = None;
                self.open_user_info(user_id, None, None);
            }
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

    pub(crate) fn room_id(&self) -> Option<String> {
        self.room().map(|room| room.id.clone())
    }

    /// Allocates a strictly increasing timeline/jump generation id so late worker
    /// events for a superseded view or cancelled jump are dropped.
    pub(super) fn allocate_gen(&mut self) -> u64 {
        self.next_gen = self.next_gen.max(self.generation).wrapping_add(1).max(1);
        self.next_gen
    }

    /// Asks the worker for the open room's timeline. Every call starts a new
    /// generation, so late updates for an earlier room are recognised and dropped.
    fn request_timeline(&mut self) {
        self.generation = self.allocate_gen();
        if let Some(room_id) = self.room_id() {
            self.send_matrix(MatrixCmd::OpenRoom {
                room_id,
                generation: self.generation,
            });
        }
        self.follow_room_emotes();
    }

    /// Called after each draw: loads older messages when the view shows the oldest
    /// loaded one, and newer ones when at the bottom of a detached slice.
    pub fn after_draw(&mut self) {
        // A custom emoji's picture travelled in a cell that was painted over: drawn
        // again, with another copy carrying it (see `ImageStore::verify`).
        if self.images.take_redraw() {
            self.dirty = true;
        }
        self.maybe_mark_read();
        self.maybe_load_older();
        self.maybe_load_newer();
        self.send_image_requests();
        self.send_preview_requests();
    }

    /// Sends a read receipt, but only for what the user can actually be looking at:
    /// the messages are on screen, at the newest live message, nothing covers them and the
    /// terminal window has the focus. Once per newest message.
    fn maybe_mark_read(&mut self) {
        // No chat is open: nothing is on screen to have been read.
        if self.chat_closed {
            self.timeline.drawn = false;
            return;
        }
        let seen = std::mem::take(&mut self.timeline.drawn)
            && !self.timeline.detached
            && self.timeline.scroll == 0
            && self.modal.is_none()
            && !self.help_open
            && !self.images.viewer_open()
            && self.window_focused;
        // A thread whose replies have not arrived yet shows nothing to have read.
        if !seen || self.matrix.is_none() || self.thread_loading() {
            return;
        }
        // Where the "new" line goes is read from the room's first messages after
        // opening it; a receipt sent before that would move what it is read from.
        if self.unread_line_pending() {
            return;
        }
        let newest = self.messages().iter().rev().find_map(|m| match &m.item {
            Some(ItemRef::Event(id)) => Some(id.clone()),
            _ => None,
        });
        let hidden_unread = self.room().is_some_and(|r| {
            !r.is_pending() && !r.marked_unread && (r.unread > 0 || r.bold || r.mention)
        });
        let unread_state = self
            .room()
            .map(|r| (r.id.clone(), r.server_count, r.unread, r.bold, r.mention));
        if newest.is_some() && newest != self.timeline.marked_read {
            self.note_read(newest.as_deref());
            self.timeline.marked_read = newest;
            self.timeline.hidden_read_state = unread_state;
        } else if self.has_pending_unread_mark()
            || (hidden_unread && unread_state != self.timeline.hidden_read_state)
        {
            // The bottom of the room is on screen with no new visible messages, yet the
            // room opened unread, settled at a trailing marker, or the server still counts
            // unread messages (e.g. from ignored senders): send a read receipt to clear them.
            self.note_read(newest.as_deref());
            self.timeline.hidden_read_state = unread_state;
        } else {
            return;
        }
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
        if self.matrix.is_none() || self.room().is_none_or(|r| r.is_pending()) {
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

    /// Loads newer messages when the newest loaded message of a detached `/context` slice
    /// is on screen and either the view is not full yet or the user scrolled downward.
    fn maybe_load_newer(&mut self) {
        const MAX_EMPTY: u8 = 5;
        let count = self.messages().len();
        let view = &mut self.timeline;
        if !view.detached {
            view.want_newer = false;
            return;
        }
        if !std::mem::take(&mut view.want_newer) || view.loading_newer || view.reached_end {
            return;
        }
        let filling = !view.view_full && view.empty_forward_batches < MAX_EMPTY;
        if !(view.newer_armed || filling) {
            return;
        }
        if self.matrix.is_none() || self.room().is_none_or(|r| r.is_pending()) {
            return;
        }
        self.timeline.newer_armed = false;
        self.timeline.loading_newer = true;
        self.timeline.forward_count_at_request = count;
        if let Some(room_id) = self.room_id() {
            self.send_matrix(MatrixCmd::PaginateForwards {
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
        // A reaction with a custom emoji can only be named once the packs are known.
        let custom_reaction = messages.iter().any(|m| {
            m.reactions.iter().any(|r| {
                r.key
                    .get(..6)
                    .is_some_and(|scheme| scheme.eq_ignore_ascii_case("mxc://"))
            })
        });
        if let Some(target) = self.messages_mut() {
            *target = messages;
        }
        if custom_reaction {
            self.want_emotes();
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

    /// The user's own latest message that can be edited, if the composer is free for
    /// it: empty, and neither answering nor editing. This is what `Action::EditLast`
    /// edits; it has no key unless the user gave it one.
    pub fn last_editable(&self) -> Option<usize> {
        if !self.composer.is_empty() || self.compose_mode != ComposeMode::Normal {
            return None;
        }
        let last = self.messages().iter().rposition(Message::editable)?;
        // One that could not be sent wants sending again or discarding first, which
        // is offered on the message itself; an older one is not "the last".
        (self.messages()[last].state != SendState::Failed).then_some(last)
    }

    /// Starts editing the message at `index` with its text in the composer.
    fn edit_message(&mut self, index: usize) {
        let Some(message) = self.messages().get(index) else {
            return;
        };
        if message.poll.is_some() {
            self.open_poll_edit(index);
            return;
        }
        // An action stays an action when edited.
        let text = if message.kind == MsgKind::Emote {
            format!("/me {}", message.body)
        } else {
            message.body.clone()
        };
        self.begin_edit(index, &text);
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
        // A custom emoji goes by its address; its name goes along for other clients to
        // show where they do not know the picture. The key itself is sent as it is: an
        // own reaction is taken back by exactly its key.
        let address = key.trim();
        let shortcode = crate::matrix::is_valid_mxc(address)
            .then(|| self.emotes.catalog.shortcode_for_url(address))
            .flatten()
            .map(str::to_owned);
        self.toggle_reaction_named(msg, key, shortcode);
    }

    /// [`App::toggle_reaction`], with the `shortcode` a custom emoji was chosen by: two
    /// packs may name the same picture differently.
    fn toggle_reaction_named(&mut self, msg: usize, key: &str, shortcode: Option<String>) {
        if self.matrix.is_some() {
            let item = self.messages().get(msg).and_then(|m| m.item.clone());
            if let (Some(item), Some(room_id)) = (item, self.room_id()) {
                self.send_matrix(MatrixCmd::ToggleReaction {
                    room_id,
                    item,
                    key: key.to_owned(),
                    shortcode,
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
                if self.room().is_some_and(|r| r.power.can_redact_other)
                    && message.state == SendState::Sent
                    && message.kind != MsgKind::Redacted
                    && !message.is_system_or_marker()
                {
                    self.open_redact_confirm(index);
                } else {
                    self.show_toast("You can only delete your own messages");
                }
                return;
            }
            Some(_) => {}
        }
        if self.matrix.is_some() {
            // The timeline update that follows shows it as deleted.
            let item = self.messages().get(index).and_then(|m| m.item.clone());
            if let (Some(item), Some(room_id)) = (item, self.room_id()) {
                self.send_matrix(MatrixCmd::Redact {
                    room_id,
                    item,
                    reason: None,
                });
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
        if !matches!(
            action,
            Action::Quit | Action::Suspend | Action::Open | Action::JumpToReply
        ) {
            self.cancel_pending_jump();
        }
        if self.chat_closed && action.needs_open_chat() {
            // No chat is open, so there is nothing for it to act on. Moving about
            // just does not happen; a command says why nothing did.
            if !matches!(
                action,
                Action::FocusNext
                    | Action::FocusPrev
                    | Action::Focus(_)
                    | Action::FocusDir(_)
                    | Action::PageUp
                    | Action::PageDown
                    | Action::JumpToNewest
                    | Action::JumpToOldest
                    | Action::SelectOlder
                    | Action::SelectNewer
            ) {
                self.show_toast("No chat is open");
            }
            return;
        }
        match action {
            Action::Quit => self.should_quit = true,
            Action::Suspend => {
                // Nothing could stop it while the app is away, and no microphone
                // stays open behind a stopped app.
                self.stop_audio();
                // The programs that transcribe and translate go on: they use neither
                // the terminal nor the sound device and are in process groups of their
                // own, and a long transcription is not thrown away for a look at the
                // shell. Their time limits run on meanwhile and are applied when the
                // app is back; if the app is killed while away, the system kills them.
                self.stop_recording();
                self.suspend_requested = true;
            }
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
            Action::ToggleSidebar => {
                self.sidebar.hidden = !self.sidebar.hidden;
                self.save_setting("sidebar_hidden", self.sidebar.hidden);
                // The list would fill the window if it kept the focus.
                self.dragging_splitter = false;
                if self.sidebar.hidden && self.focus == Pane::Sidebar {
                    self.set_focus(Pane::Composer);
                }
                // Pictures are placed by column: everything moves.
                self.repaint = true;
            }
            Action::CycleTheme => {
                self.step_theme(1);
            }
            Action::Back => self.back(),
            Action::CloseChat => self.leave_for_room_list(),
            Action::PageUp => {
                self.timeline.scroll += self.timeline.page.saturating_sub(1).max(1);
                self.timeline.older_armed = true;
            }
            Action::PageDown => {
                let step = self.timeline.page.saturating_sub(1).max(1);
                self.timeline.scroll = self.timeline.scroll.saturating_sub(step);
                self.timeline.newer_armed = true;
            }
            Action::JumpToNewest => {
                if self.timeline.detached {
                    self.return_to_live();
                } else {
                    self.timeline.scroll = 0;
                    if self.focus == Pane::Timeline {
                        self.timeline.selected = self.find_selectable(usize::MAX, false);
                    }
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
                        if self.timeline.detached {
                            self.timeline.newer_armed = true;
                        }
                    }
                    None if self.timeline.detached && !self.timeline.reached_end => {
                        self.timeline.scroll = 0;
                        self.timeline.newer_armed = true;
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
                if self.recording_refuses_edit() {
                    return;
                }
                let selected = self.timeline.selected;
                match selected.and_then(|i| self.messages().get(i).map(|m| (i, m))) {
                    Some((i, message)) if message.editable() => self.edit_message(i),
                    Some((_, message)) if message.own => {
                        self.show_toast("This message cannot be edited");
                    }
                    Some(_) => self.show_toast("You can only edit your own messages"),
                    None => {}
                }
            }
            Action::EditLast => {
                if self.recording_refuses_edit() {
                    return;
                }
                match self.last_editable() {
                    Some(i) => self.edit_message(i),
                    None if !self.composer.is_empty()
                        || self.compose_mode != ComposeMode::Normal =>
                    {
                        self.show_toast("Finish what you are writing first")
                    }
                    // The last one could not be sent: there it is, to be sent again or
                    // discarded, which is offered on the message itself.
                    None if self
                        .messages()
                        .iter()
                        .rfind(|message| message.editable())
                        .is_some_and(|message| message.state == SendState::Failed) =>
                    {
                        self.perform(Action::SelectOlder)
                    }
                    None => self.show_toast("There is no message of yours to edit here"),
                }
            }
            Action::React => {
                if let Some(i) = self.timeline.selected
                    && self.modal.is_none()
                    && let Some(message) = self.messages().get(i)
                {
                    let mut picker = EmojiPicker::for_reaction(i, message.id.clone());
                    // Custom emoji too; the packs are loaded now if they were not yet.
                    self.want_emotes();
                    picker.set_custom(&self.emotes.catalog);
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
            Action::CopyCode => self.copy_code_of_selected(),
            Action::OpenExternal => {
                self.cancel_pending_jump();
                if !self.save_attachment_for(true, Action::OpenExternal) {
                    self.show_toast("Nothing here to open in another program");
                }
            }
            Action::ComposeInEditor => self.request_editor(),
            Action::Open => {
                self.cancel_pending_jump();
                let opened = match self.timeline.selected {
                    Some(i) => {
                        // A file or playable media message is about its file, even with
                        // a video thumbnail or a link in the caption.
                        let has_file = self.messages().get(i).is_some_and(|m| m.file.is_some());
                        let votable = self.messages().get(i).is_some_and(|m| {
                            m.poll.is_some()
                                && m.kind != MsgKind::Redacted
                                && m.state == SendState::Sent
                        });
                        if votable {
                            // Opening a poll is voting in it.
                            self.open_poll_vote(i);
                            true
                        } else if has_file {
                            self.save_attachment(true)
                        } else {
                            self.open_image(i) || self.open_link(i, 0)
                        }
                    }
                    None => false,
                };
                if !opened {
                    self.show_toast("Nothing to open in this message");
                }
            }
            Action::JumpToReply => {
                if let Some(i) = self.timeline.selected {
                    self.jump_to_reply(i);
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
            Action::WithdrawKnock => {
                let knocked = self.room().filter(|room| room.kind == RoomKind::Knock);
                if let Some(room_id) = knocked.map(|room| room.id.clone()) {
                    self.send_matrix(MatrixCmd::Leave { room_id });
                }
            }
            Action::OpenEmoji => {
                if self.modal.is_none() {
                    self.set_focus(Pane::Composer);
                    self.want_emotes();
                    let mut picker = EmojiPicker::default();
                    picker.set_custom(&self.emotes.catalog);
                    self.open_modal(Modal::Emoji(picker));
                }
            }
            Action::LoginSubmit => self.submit_login(false),
            Action::LoginBrowser => self.submit_login(true),
            Action::RecoverySubmit => self.submit_recovery(),
            Action::CloseModal => self.close_modal(),
            Action::OpenSettings => {
                if self.modal.is_none() {
                    let paired = self.has_theme_pair();
                    self.open_modal(Modal::Settings(SettingsMenu::for_pairing(paired)));
                }
            }
            Action::Search => self.open_search(),
            Action::SearchEverywhere => self.open_global_search(),
            Action::SpaceNext => self.step_space(1),
            Action::SpacePrev => self.step_space(-1),
            Action::FilterUnread => self.toggle_sidebar_filter(true),
            Action::FilterPeople => self.toggle_sidebar_filter(false),
            Action::RoomPrev => self.step_room(-1),
            Action::RoomNext => self.step_room(1),
            Action::RoomActive => self.open_active_room(),
            Action::Save => {
                if !self.save_attachment(false) {
                    self.show_toast("Nothing to save in this message");
                }
            }
            Action::PlayAudio => self.play_audio(),
            Action::Transcribe => self.toggle_note(crate::notes::NoteKind::Transcript),
            Action::TranscribeAgain => self.redo_note(crate::notes::NoteKind::Transcript),
            Action::TranslateAgain => self.redo_note(crate::notes::NoteKind::Translation),
            Action::Translate => self.toggle_note(crate::notes::NoteKind::Translation),
            Action::PauseAudio => self.control_audio(crate::audio::Control::TogglePause),
            Action::SeekAudio(dir) => {
                let seconds = match dir {
                    Dir::Left => -crate::audio::SEEK_SECONDS,
                    Dir::Right => crate::audio::SEEK_SECONDS,
                    Dir::Up | Dir::Down => return,
                };
                self.control_audio(crate::audio::Control::Seek(seconds));
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
            Action::RecordVoice => {
                if self.modal.is_none() {
                    self.record_voice();
                }
            }
            Action::PlayVoiceNote => {
                if self.modal.is_none() {
                    self.play_voice_note();
                }
            }
            Action::PastePicture => {
                if self.modal.is_none() {
                    self.request_paste_picture();
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
            Action::JumpToUnread => self.jump_to_unread(),
            Action::ToggleUnread => self.toggle_unread(),
            Action::MarkAllRead => self.mark_all_read(),
            Action::PinMessage => self.toggle_pin_message(None),
            Action::OpenPinned => self.open_pinned(),
            Action::SaveForLater => self.toggle_saved(),
            Action::RemindMe => self.open_reminder_for_selected(),
            Action::OpenSaved => self.open_saved_list(),
            Action::OpenThreads => self.open_threads(),
            Action::ForwardMessage => self.open_forward_picker(None),
            Action::RetryDecryption => self.retry_decryption(),
            Action::Devices(click) => match (&self.modal, click) {
                (Some(Modal::Sessions(_) | Modal::Keys(_)), _) => self.devices_click(click),
                (_, crate::devices::Click::Export) => self.open_key_export(),
                (_, crate::devices::Click::Import) => self.open_key_import(),
                _ => {}
            },
            Action::UserInfo => self.open_user_info_from_selection(),
            Action::MessageDetails => self.open_message_details(),
            Action::Doctor => self.open_doctor(),
            Action::ArchiveRoom => self.open_archive(),
            Action::CatchUp => self.open_catch_up(),
            Action::CreatePoll => self.open_poll_create(""),
            Action::OpenStickers => {
                self.open_stickers("");
            }
            Action::NewRoom
            | Action::RoomInfo
            | Action::SpaceInfo
            | Action::LeaveRoom
            | Action::InviteUser
            | Action::Manage(_) => self.perform_manage(action),
        }
    }

    // ----- popups --------------------------------------------------------------------

    /// Only one popup is shown at a time; the help overlay makes way too.
    fn open_modal(&mut self, modal: Modal) {
        if !matches!(modal, Modal::Recovery(_)) {
            self.cancel_pending_jump();
        }
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
        if browser && !form.allows_browser() {
            return;
        }
        if !browser && !form.allows_password() {
            return;
        }
        form.error = None;
        form.browser_url = None;
        let command = if form.is_reauth() {
            if browser {
                form.busy = Some("Waiting for your browser…".to_owned());
                MatrixCmd::ReauthBrowser
            } else {
                form.busy = Some("Re-authenticating…".to_owned());
                MatrixCmd::ReauthPassword {
                    password: form.password.take_secret(),
                }
            }
        } else if browser {
            form.busy = Some("Waiting for your browser…".to_owned());
            MatrixCmd::LoginBrowser {
                homeserver: form.homeserver.as_str().to_owned(),
            }
        } else {
            form.busy = Some("Logging in…".to_owned());
            MatrixCmd::LoginPassword {
                homeserver: form.homeserver.as_str().to_owned(),
                username: form.username.as_str().to_owned(),
                password: form.password.take_secret(),
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
        let key = form.key.take_secret();
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
                self.maybe_open_startup_target();
            }
            Some(
                Modal::Emoji(_)
                | Modal::Settings(_)
                | Modal::Keybinds(_)
                | Modal::IgnoredUsers(_)
                | Modal::Report(_)
                | Modal::Pinned(_)
                | Modal::ReasonConfirm(_)
                | Modal::PollForm(_)
                | Modal::PollVote(_)
                | Modal::Stickers(_)
                | Modal::Threads(_)
                | Modal::Doctor(_),
            ) => {
                self.modal = None;
            }
            Some(Modal::Collect(_)) => self.close_collect(),
            Some(Modal::Palette(_)) => self.close_palette(),
            Some(Modal::Search(_)) => self.close_search(),
            Some(Modal::GlobalSearch(_)) => self.close_global_search(),
            Some(Modal::Verify(_)) => self.verify_escape(),
            Some(Modal::New(_) | Modal::RoomInfo(_)) => self.manage_escape(),
            Some(Modal::UserInfo(_)) => self.escape_user_info(),
            Some(Modal::OwnProfile(_)) => self.escape_own_profile(),
            Some(Modal::Details(_)) => self.escape_details(),
            Some(Modal::Menu(_)) => self.modal = None,
            Some(Modal::Sessions(_) | Modal::Keys(_)) => {
                self.devices_click(crate::devices::Click::Cancel);
            }
            Some(Modal::Keywords(_)) => self.keywords_escape(),
            Some(Modal::Saved(_)) => self.saved_escape(),
            None => {}
        }
    }

    /// Result `at` of the picker was chosen: react with it, or put it in the draft.
    fn pick_emoji(&mut self, at: usize) {
        let Some(Modal::Emoji(picker)) = &self.modal else {
            return;
        };
        let Some(pick) = picker.results.get(at).copied() else {
            return;
        };
        let custom = picker.custom_at(at).cloned();
        let Some(Modal::Emoji(picker)) = self.modal.take() else {
            return;
        };
        let Some((index, id)) = picker.react_to else {
            self.set_focus(Pane::Composer);
            match (pick, custom) {
                (Pick::Custom(_), Some(custom)) => self.insert_custom_emoji(&custom),
                (Pick::Unicode(emoji), _) => self.insert_composer_text(emoji.as_str()),
                (Pick::Custom(_), None) => {}
            }
            return;
        };
        // A custom emoji reacts by its address, with the name it was chosen by (see
        // `toggle_reaction`).
        let (key, shortcode) = match (pick, custom) {
            (Pick::Custom(_), Some(custom)) => (custom.url, Some(custom.shortcode)),
            (Pick::Unicode(emoji), _) => (emoji.as_str().to_owned(), None),
            (Pick::Custom(_), None) => return,
        };
        // The message may have moved while the picker was open.
        let index = if id.is_empty() {
            Some(index)
        } else {
            self.messages().iter().position(|m| m.id == id)
        };
        if let Some(index) = index {
            self.toggle_reaction_named(index, &key, shortcode);
        }
    }

    /// Puts a custom emoji into the draft as an accepted `:` completion would, so that
    /// it is sent as its picture. It is set apart from a word before it: a shortcode
    /// in the middle of a word stays text.
    fn insert_custom_emoji(&mut self, emoji: &crate::matrix::CustomEmoji) {
        let cursor = self.composer.cursor();
        let after_word = self.composer.text()[..cursor]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace());
        if after_word {
            self.insert_composer_text(" ");
        }
        let at = self.composer.cursor();
        let target = crate::complete::Target::CustomEmoji {
            shortcode: emoji.shortcode.clone(),
            url: emoji.url.clone(),
        };
        let (text, mark) = crate::complete::replacement(self.composer.text(), &(at..at), &target);
        self.composer.replace_range(at..at, &text);
        self.adjust_marks_for_splice(at, at, self.composer.cursor());
        self.complete.marks.extend(mark);
        self.sync_completion();
    }

    fn on_modal_key(&mut self, key: KeyEvent, code: KeyCode, mods: KeyModifiers) {
        let plain = !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
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
                        LoginForm::PASSWORD_BUTTON => self.submit_login(false),
                        focus if !form.is_reauth() && focus + 1 < LoginForm::FIELDS => form.next(),
                        _ => {
                            let browser = !form.allows_password();
                            self.submit_login(browser);
                        }
                    },
                    _ => {
                        if let Some(field) = form.field_mut() {
                            field.handle_key(code, mods, EditKeys::FULL);
                        }
                    }
                }
            }
            Some(Modal::Recovery(form)) => match code {
                KeyCode::Esc => self.close_modal(),
                _ if form.busy => {}
                KeyCode::Enter => self.submit_recovery(),
                _ => {
                    form.key.handle_key(code, mods, EditKeys::FULL);
                }
            },
            Some(Modal::Emoji(picker)) => {
                let page = picker.rows.max(1) as isize;
                match code {
                    KeyCode::Esc => self.modal = None,
                    KeyCode::Enter => {
                        let selected = picker.selected;
                        self.pick_emoji(selected);
                    }
                    KeyCode::Left if plain => picker.step(-1),
                    KeyCode::Right if plain => picker.step(1),
                    KeyCode::Up => picker.step_rows(-1),
                    KeyCode::Down => picker.step_rows(1),
                    KeyCode::PageUp => picker.step_rows(-page),
                    KeyCode::PageDown => picker.step_rows(page),
                    _ => {
                        // Plain Left/Right step the emoji grid; word motion and
                        // line-edit shortcuts still edit the search query.
                        let keys = EditKeys {
                            arrows: false,
                            ..EditKeys::FULL
                        };
                        if picker.query.handle_key(code, mods, keys).changed() {
                            picker.refilter();
                        }
                    }
                }
            }
            Some(Modal::Settings(menu)) if menu.editing.is_some() => match code {
                KeyCode::Esc => menu.editing = None,
                KeyCode::Enter => {
                    let text = menu.editing.take().unwrap_or_default();
                    match menu.current() {
                        Setting::AudioPlayer => self.set_audio_player(&text),
                        Setting::AudioRecorder => self.set_audio_recorder(&text),
                        Setting::TranscribeCommand => self.set_transcribe_command(&text),
                        Setting::TranscribeModel => self.set_transcribe_model(&text),
                        Setting::TranslateCommand => self.set_translate_command(&text),
                        Setting::TranslateLanguage => self.set_translate_language(&text),
                        Setting::SummaryCommand => self.set_summary_command(&text),
                        _ => self.set_download_dir(&text),
                    }
                }
                _ => {
                    if let Some(text) = &mut menu.editing {
                        text.handle_key(code, mods, EditKeys::FULL);
                    }
                }
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
            Some(Modal::GlobalSearch(_)) => self.on_global_search_key(code, mods),
            Some(Modal::New(_) | Modal::RoomInfo(_)) => self.on_manage_key(code, mods),
            Some(Modal::Menu(_)) => self.on_menu_key(key, code, mods),
            Some(Modal::Keybinds(_)) => self.on_keybinds_key(key, code, mods),
            Some(Modal::IgnoredUsers(_)) => self.on_ignored_users_key(code, mods),
            Some(Modal::Report(_)) => self.on_report_key(code, mods),
            Some(Modal::Pinned(_)) => self.on_pinned_key(code, mods),
            Some(Modal::ReasonConfirm(_)) => self.on_reason_confirm_key(code, mods),
            Some(Modal::Sessions(_)) => self.on_sessions_key(code, mods),
            Some(Modal::Keys(_)) => self.on_keys_key(code, mods),
            Some(Modal::UserInfo(_)) => self.on_user_info_key(code, mods),
            Some(Modal::OwnProfile(_)) => self.on_own_profile_key(code, mods),
            Some(Modal::PollForm(_)) => self.on_poll_form_key(code, mods),
            Some(Modal::PollVote(_)) => self.on_poll_vote_key(code, mods),
            Some(Modal::Stickers(_)) => self.on_stickers_key(code, mods),
            Some(Modal::Details(_)) => self.on_details_key(code, mods),
            Some(Modal::Threads(_)) => self.on_threads_key(code, mods),
            Some(Modal::Keywords(_)) => self.on_keywords_key(code, mods),
            Some(Modal::Saved(_)) => self.on_saved_key(code, mods),
            Some(Modal::Doctor(_)) => self.on_doctor_key(code, mods),
            Some(Modal::Collect(_)) => self.on_collect_key(code, mods),
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
            MatrixEvent::NeedReauth { target, reason } => {
                self.reset_session();
                self.open_modal(Modal::Login(LoginForm::reauth(target, reason)));
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
                if let Some(file) = self.bookmarks.file.clone() {
                    self.load_saved(file, &user_id);
                }
                self.user_id = Some(user_id);
                self.send_matrix(MatrixCmd::Previews(self.room_previews));
                self.logged_out = false;
                self.session_started = Some(Instant::now());
                self.unread.session_begins();
                self.maybe_open_startup_target();
            }
            MatrixEvent::CaughtUp => self.unread.caught_up(),
            MatrixEvent::Sync(status) => {
                self.sync = Some(status);
                if self.sync == Some(SyncStatus::Synced) {
                    self.maybe_open_startup_target();
                }
            }
            // News of a session that has ended must not bring its rooms back.
            MatrixEvent::Rooms(_) if self.logged_out => {}
            MatrixEvent::Rooms(rooms) => {
                self.rooms_received = true;
                self.apply_rooms(rooms);
                self.saved_rooms_arrived();
                // A room that was just joined or created is opened when it is listed.
                self.open_pending_room();
                self.search_check_room();
                self.maybe_open_startup_target();
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
                self.maybe_open_startup_target();
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
                detached,
            } => {
                if self.pending_jump.as_ref().is_some_and(|pending| {
                    pending.generation == generation && pending.room_id == room_id
                }) {
                    self.on_jump_arrived(generation, room_id, messages, reached_start, detached);
                } else if generation == self.generation
                    && self.room_id().as_deref() == Some(&room_id)
                {
                    let empty_reset = messages.is_empty() && !reached_start;
                    let messages = self.place_unread_line(messages, detached.is_some());
                    let messages = self.thread_snapshot(messages, reached_start);
                    self.apply_timeline(messages);
                    self.forget_notes_of_deleted();
                    self.sync_pinned_flags(&room_id);
                    self.sync_saved_flags(&room_id);
                    self.search_sync();
                    if empty_reset {
                        self.timeline.reached_start = false;
                        self.timeline.empty_batches = 0;
                    }
                    self.timeline.reached_start |= reached_start;
                    match detached {
                        Some(info) => {
                            self.timeline.detached = true;
                            self.timeline.reached_end |= info.reached_end;
                        }
                        None => {
                            self.timeline.detached = false;
                            self.timeline.detached_new = 0;
                            self.timeline.reached_end = false;
                            self.timeline.loading_newer = false;
                            self.timeline.want_newer = false;
                        }
                    }
                }
            }
            MatrixEvent::DetachedNew { generation, count } => {
                if generation == self.generation && self.timeline.detached {
                    self.timeline.detached_new = count;
                }
            }
            MatrixEvent::JumpFailed { generation, reason } => {
                self.on_jump_failed(generation, reason);
            }
            MatrixEvent::Paginated {
                generation,
                reached_start,
            } => {
                if generation == self.generation {
                    // Did this request bring anything to show?
                    // (The messages on screen: a thread's while one is open.)
                    let shown = self.messages().len();
                    let view = &mut self.timeline;
                    view.loading_older = false;
                    view.reached_start |= reached_start;
                    let grew = shown > view.count_at_request;
                    view.empty_batches = if grew { 0 } else { view.empty_batches + 1 };
                    self.search_after_load();
                }
            }
            MatrixEvent::PaginatedForwards {
                generation,
                reached_end,
            } => {
                if generation == self.generation {
                    let shown = self.messages().len();
                    let view = &mut self.timeline;
                    view.loading_newer = false;
                    view.reached_end |= reached_end;
                    let grew = shown > view.forward_count_at_request;
                    view.empty_forward_batches = if grew {
                        0
                    } else {
                        view.empty_forward_batches + 1
                    };
                }
            }
            MatrixEvent::Typing { generation, names } => {
                if generation == self.generation
                    && let Some(room) = self.open_index().and_then(|open| self.rooms.get_mut(open))
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
            MatrixEvent::EmotePicture { url, result } => self.images.on_emote(&url, result),
            MatrixEvent::LinkPreview { url, result } => self.on_link_preview(&url, result),
            MatrixEvent::Spaces(_) if self.logged_out => {}
            MatrixEvent::Spaces(spaces) => self.apply_spaces(spaces),
            MatrixEvent::Notice(notice) => {
                self.detailed_notices
                    .insert(notice.room_id.clone(), Instant::now());
                self.count_notices
                    .retain(|(_, waiting)| waiting.room_id != notice.room_id);
                self.on_notice(notice);
            }
            MatrixEvent::SuppressedNotice { room_id } => {
                self.detailed_notices
                    .insert(room_id.clone(), Instant::now());
                self.count_notices
                    .retain(|(_, waiting)| waiting.room_id != room_id);
            }
            MatrixEvent::NoticeClicked { room_id, event } => {
                self.on_notice_click(&room_id, event);
            }
            MatrixEvent::FileSaved { key, open, result } => self.on_file_saved(key, open, result),
            MatrixEvent::AudioFetched { id, result } => self.on_audio_fetched(id, result),
            MatrixEvent::SpeechFetched { id, result } => self.on_speech_fetched(id, result),
            MatrixEvent::Security(security) => self.on_security(security),
            MatrixEvent::Verification(update) => self.on_verification(update),
            MatrixEvent::CrossSigningSetUp(result) => self.on_cross_signing_set_up(result),
            MatrixEvent::UploadLimit(bytes) => self.attach.limit = Some(bytes),
            MatrixEvent::FilesNotSent { send, reason } => self.on_files_not_sent(send, reason),
            MatrixEvent::FilesSent(send) => self.on_files_sent(send),
            MatrixEvent::RoomMembers {
                room_id,
                members,
                can_mention_room,
            } => {
                // Not one of a session that has ended.
                if !self.logged_out {
                    self.on_room_members(room_id, members, can_mention_room);
                }
            }
            MatrixEvent::Draft {
                room_id,
                thread,
                text,
            } => {
                // Not one of a session that has ended.
                if !self.logged_out {
                    self.on_stored_draft(room_id, thread, text);
                }
            }
            // Answers to requests of a session that has ended.
            MatrixEvent::Manage(_) if self.logged_out => {}
            MatrixEvent::Manage(event) => self.on_manage(event),
            MatrixEvent::SearchResults {
                generation,
                hits,
                done,
                note,
            } => self.on_search_results(generation, hits, done, note),
            MatrixEvent::IgnoredUsers(users) => self.on_ignored_users_list(users),
            MatrixEvent::EmotePacks { .. } if self.logged_out => {}
            MatrixEvent::EmotePacks { room_id, catalog } => self.on_emote_packs(room_id, catalog),
            MatrixEvent::Threads(page) => self.on_threads(page),
            MatrixEvent::ThreadsFailed {
                room_id,
                generation,
            } => self.on_threads_failed(&room_id, generation),
            MatrixEvent::IgnoreResult {
                user_id,
                ignored,
                result,
            } => self.on_ignore_result(user_id, ignored, result),
            MatrixEvent::ReportResult(result) => self.on_report_result(result),
            MatrixEvent::UserSearchResults {
                generation,
                results,
            } => self.on_user_search_results(generation, results),
            MatrixEvent::DirectoryResults(page) => self.on_directory_results(page),
            MatrixEvent::DirectoryError {
                generation,
                message,
            } => self.on_directory_error(generation, message),
            MatrixEvent::Source { request, result } => self.on_event_source(request, result),
            MatrixEvent::RoomPower { room_id, power } => {
                if let Some(room) = self.rooms.iter_mut().find(|r| r.id == room_id) {
                    room.power = power;
                }
            }
            MatrixEvent::PinnedEvents { room_id, ids } => self.on_pinned_events(room_id, ids),
            MatrixEvent::PinnedList {
                room_id,
                items,
                error,
            } => self.on_pinned_list(room_id, items, error),
            MatrixEvent::ForwardResult {
                to_room_id,
                to_room_name,
                result,
            } => {
                self.on_forward_result(to_room_id, to_room_name, result);
            }
            MatrixEvent::Devices(event) => self.on_devices_event(event),
            MatrixEvent::Keywords(event) => self.on_keywords_event(event),
            MatrixEvent::Saved(event) => self.on_saved_event(event),
            // Answers for `matrix-rat serve`; the app never asks for them.
            MatrixEvent::Agent(_) => {}
            MatrixEvent::Collect(event) => self.on_collect(event),
            MatrixEvent::DecryptionRetried {
                event_id, result, ..
            } => self.on_decryption_retried(&event_id, result),
            MatrixEvent::OwnProfile(profile) => self.on_own_profile(profile),
            MatrixEvent::OwnProfileResult { what, result } => {
                self.on_own_profile_result(what, result);
            }
            MatrixEvent::UserProfile { request, result } => {
                self.on_user_profile(request, result.map(|p| *p));
            }
        }
        // A menu about something that is gone closes.
        self.menu_follow();
    }

    /// Replaces the room list with the server's, keeping what belongs to the UI (drafts,
    /// loaded messages) and keeping the same room open even if its position changed.
    fn apply_rooms(&mut self, summaries: Vec<RoomSummary>) {
        let mut open_id = self.room().map(|room| room.id.clone());
        // With no chat open, `active` is the room that was left: followed to its new
        // place like the open one, but never opened.
        let left_id = self
            .chat_closed
            .then(|| self.rooms.get(self.active))
            .flatten()
            .map(|room| room.id.clone());
        // Rooms arrive in batches. If all we could show so far was a pending invite or
        // knock, look again: a real conversation is the better default.
        if self.auto_opened && self.room().is_some_and(|room| room.is_pending()) {
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
                    RoomCategory::Knock => RoomKind::Knock,
                    RoomCategory::Favourite => RoomKind::Favourite,
                    RoomCategory::Person => RoomKind::Person,
                    RoomCategory::Room => RoomKind::Room,
                    RoomCategory::LowPriority => RoomKind::LowPriority,
                };
                let known = old.remove(&summary.id);
                // The server counts what the account's rules say deserves a
                // notification. A count that went up is news, on any server.
                if let Some(room) = &known
                    && had_rooms
                    && !kind.is_pending()
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
                room.marked_unread = self.unread.settle(&summary.id, summary.marked_unread);
                room.bold = summary.unread;
                room.mention = summary.mention;
                room.avatar = summary.avatar;
                room.preview = summary.preview;
                room.alias = summary.alias;
                room.direct = summary.direct;
                room.muted = summary.muted;
                room.encrypted = summary.encrypted;
                room
            })
            .collect();

        // What is left over is gone from the list, and its drafts with it.
        for room_id in old.keys() {
            self.forget_room_drafts(room_id);
            self.unread.forget(room_id);
            if self.cancel_recording_in(room_id) {
                self.show_toast("That room is gone: the recording was discarded");
            }
        }

        // Held back for a moment: if the full notification (with sender and text)
        // arrives for the same room, it is shown instead.
        let now = Instant::now();
        self.count_notices
            .extend(risen.into_iter().map(|notice| (now, notice)));

        let position = |rooms: &[Room], id: &Option<String>| {
            id.as_ref()
                .and_then(|id| rooms.iter().position(|room| room.id == *id))
        };
        let was_pending = open_id
            .as_ref()
            .and_then(|id| old_kinds.get(id))
            .is_some_and(|kind| kind.is_pending());
        match position(&self.rooms, &open_id) {
            Some(index) => {
                self.active = index;
                if self
                    .rooms
                    .get(index)
                    .is_some_and(|r| r.unread == 0 && !r.bold && !r.mention)
                {
                    self.timeline.hidden_read_state = None;
                }
                // An accepted invitation or approved knock becomes a room with messages to load.
                if was_pending && !self.rooms[index].is_pending() {
                    self.timeline = TimelineState::default();
                    self.begin_unread_line();
                    self.request_timeline();
                    self.request_members_for_open_room();
                }
            }
            // The user closed the chat: a new list opens none, whatever became of the
            // room that was left (its place is found below, with the cursor's).
            None if self.chat_closed => {}
            None => {
                // The open room is gone (or this is the first list): show the first one
                // that is a real conversation, with its own draft. A draft for the
                // vanished room has nowhere to go.
                self.auto_opened = true;
                self.threads.open = None;
                self.active = self
                    .rooms
                    .iter()
                    .position(|room| !room.is_pending())
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
                self.begin_unread_line();
                self.request_timeline();
                self.request_members_for_open_room();
            }
        }
        if let Some(index) = position(&self.rooms, &cursor_id)
            && let Some(at) = self.visible_rooms().iter().position(|i| *i == index)
        {
            self.sidebar.cursor = at;
        }
        if self.chat_closed {
            // The room that was left, where it is now; if it is gone, the one under
            // the cursor is as good a place to remember.
            self.active = position(&self.rooms, &left_id)
                .or_else(|| self.cursor_room())
                .unwrap_or(0);
        }
    }

    /// Esc: undo the most recent "mode" first, then head back to the composer; from
    /// there, with nothing left to cancel and no thread open, the chat closes (the
    /// `esc_closes_chat` setting).
    fn back(&mut self) {
        if self.help_open {
            self.help_open = false;
        } else if self.audio_loading() {
            // A sound's download is given up first: it is what is under way.
            self.stop_audio();
        } else if self.focus == Pane::Timeline && self.stop_selected_note() {
            // So is the program that runs for the selected message.
        } else if self.focus == Pane::Sidebar
            && (!self.sidebar.filter.is_empty()
                || self.sidebar.unread_only
                || self.sidebar.people_only)
        {
            self.sidebar.filter.clear();
            self.sidebar.unread_only = false;
            self.sidebar.people_only = false;
            self.sidebar.cursor = 0;
            self.sidebar.reveal_cursor = true;
        } else if self.chat_closed {
            // Nothing is left to step back from: the room list is all there is.
        } else if self.focus != Pane::Composer {
            self.set_focus(Pane::Composer);
        } else if self.compose_mode != ComposeMode::Normal {
            self.cancel_compose();
        } else if self.thread_open() {
            self.close_thread();
        } else if self.esc_closes_chat {
            self.close_chat();
        }
    }

    // ----- input ---------------------------------------------------------------------

    pub fn on_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.on_key(key);
                self.update_typing();
                self.note_draft();
            }
            Event::Mouse(mouse) => {
                self.on_mouse(mouse);
                self.update_typing();
                self.note_draft();
            }
            Event::Paste(mut text) => {
                // A secret is taken as it is and our copy of it wiped; everything
                // else goes the usual way.
                if self.paste_secret(&text) {
                    zeroize::Zeroize::zeroize(&mut text);
                } else {
                    self.on_paste(&text);
                }
                self.update_typing();
                self.note_draft();
            }
            Event::Resize(..) => {
                // A resized terminal may have dropped the pictures it was sent.
                self.images.forget_encoded();
                self.dirty = true;
            }
            Event::FocusGained => {
                self.window_focused = true;
                self.found.focus_reports = true;
                self.dirty = true;
            }
            Event::FocusLost => {
                // Nobody watches the microphone from another window.
                self.stop_recording();
                self.window_focused = false;
                self.found.focus_reports = true;
            }
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

        // With no popup open, Esc discards an active recording of the open chat
        // before doing anything else. A take that is already Saving is left to finish.
        if code == KeyCode::Esc && mods == KeyModifiers::NONE && self.discard_active_recording() {
            self.show_toast("The recording was discarded");
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
            } else {
                self.cancel_pending_jump();
            }
            return;
        }

        if self.focus == Pane::Composer && self.complete_key(code, mods) {
            self.cancel_pending_jump();
            return;
        }
        let width = self.composer_view.width;
        if self.focus == Pane::Composer
            && (code, mods) == (KeyCode::Up, KeyModifiers::NONE)
            && !self.composer.on_first_row(width)
        {
            self.cancel_pending_jump();
            self.composer.up(width);
            self.sync_completion();
            return;
        }
        if self.focus == Pane::Composer && self.attach_key(code, mods) {
            self.cancel_pending_jump();
            return;
        }
        if let Some(action) = self.keymap.lookup(context, key) {
            self.perform(action);
            return;
        }
        match self.focus {
            Pane::Composer => self.edit_composer(code, mods),
            Pane::Sidebar => {
                let outcome = self.sidebar.filter.handle_key(code, mods, EditKeys::FULL);
                if !matches!(outcome, EditOutcome::Ignored) {
                    self.cancel_pending_jump();
                }
                if outcome.changed() {
                    self.sidebar.cursor = 0;
                    self.sidebar.reveal_cursor = true;
                }
            }
            // Unbound keys do nothing here; they are never typed into the draft.
            Pane::Timeline => {}
        }
    }

    /// Empties the draft, with what is attached to it. False if there was nothing.
    fn clear_draft(&mut self) -> bool {
        // What is attached goes with the draft.
        let had_files = self.clear_attachments();
        // Also with nothing typed: a reply or an edit that was begun is given up (an
        // edit emptied by hand would otherwise have no way out by this key).
        let composing = self.compose_mode != ComposeMode::Normal;
        if !self.composer.is_empty() || composing {
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
                self.cancel_pending_jump();
                let mut buf = [0u8; 4];
                if self
                    .sidebar
                    .filter
                    .insert(c.encode_utf8(&mut buf))
                    .changed()
                {
                    self.sidebar.cursor = 0;
                    self.sidebar.reveal_cursor = true;
                }
            }
            Pane::Composer => {
                self.cancel_pending_jump();
                self.composer.insert(c.encode_utf8(&mut [0; 4]));
            }
            Pane::Timeline => {}
        }
    }

    fn edit_composer(&mut self, code: KeyCode, mods: KeyModifiers) {
        let width = self.composer_view.width;
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let alt = mods.contains(KeyModifiers::ALT);
        let before = self.composer.text().to_owned();
        let before_cursor = self.composer.cursor();
        let mut handled = true;
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
            _ => handled = false,
        }
        if handled {
            self.cancel_pending_jump();
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
                    field.paste(first_line);
                }
                return;
            }
            Some(Modal::Recovery(form)) => {
                if !form.busy {
                    form.key.paste(first_line);
                }
                return;
            }
            Some(Modal::Emoji(picker)) => {
                if picker.query.paste(first_line).changed() {
                    picker.refilter();
                }
                return;
            }
            Some(Modal::Settings(menu)) => {
                if let Some(typed) = &mut menu.editing {
                    typed.paste(first_line);
                }
                return;
            }
            Some(Modal::Verify(_)) => return self.on_verify_paste(first_line),
            Some(Modal::Palette(palette)) => {
                if palette.query.paste(first_line).changed() {
                    palette.on_query_changed();
                }
                return;
            }
            Some(Modal::Search(_)) => return self.search_paste(first_line),
            Some(Modal::GlobalSearch(_)) => return self.global_search_paste(first_line),
            Some(Modal::New(_) | Modal::RoomInfo(_)) => {
                return self.on_manage_paste(&text, first_line);
            }
            // A paste is not an answer to a menu.
            Some(Modal::Menu(_)) => return,
            Some(Modal::Keybinds(_)) => return,
            Some(Modal::IgnoredUsers(_)) => return self.on_ignored_users_paste(first_line),
            Some(Modal::Report(_)) => return self.on_report_paste(first_line),
            Some(Modal::ReasonConfirm(_)) => return self.on_reason_confirm_paste(first_line),
            Some(Modal::PollForm(_)) => return self.on_poll_form_paste(first_line),
            Some(Modal::PollVote(_)) => return,
            Some(Modal::Stickers(_)) => return self.on_stickers_paste(first_line),
            Some(Modal::Threads(_)) => return self.on_threads_paste(first_line),
            Some(Modal::Keywords(_)) => return self.on_keywords_paste(first_line),
            Some(Modal::Saved(_)) => return self.on_saved_paste(first_line),
            Some(Modal::Sessions(_) | Modal::Keys(_)) => {
                return self.on_devices_paste(&text, first_line);
            }
            Some(Modal::Pinned(_) | Modal::Details(_) | Modal::Doctor(_) | Modal::Collect(_)) => {
                return;
            }
            Some(Modal::UserInfo(_)) => return self.on_user_info_paste(first_line),
            Some(Modal::OwnProfile(_)) => return self.on_own_profile_paste(&text, first_line),
            None => {}
        }
        // The picture viewer and the help cover the composer: nothing is pasted
        // (or attached) where it cannot be seen.
        if self.help_open || self.images.viewer_open() {
            return;
        }
        self.cancel_pending_jump();
        // Files dropped on the terminal arrive as a paste of their paths.
        if self.attach_pasted(&text) {
            return;
        }
        match self.focus {
            Pane::Sidebar => {
                if self
                    .sidebar
                    .filter
                    .paste(text.lines().next().unwrap_or(""))
                    .changed()
                {
                    self.sidebar.cursor = 0;
                    self.sidebar.reveal_cursor = true;
                }
            }
            _ => {
                self.set_focus(Pane::Composer);
                self.insert_composer_text(&text);
                self.allow_text_edit_reopen_completion();
            }
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        let hit_rect = self.hits.at_rect(mouse.column, mouse.row);
        let hit = hit_rect.map(|(_, id)| id);
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && hit != Some(HitId::Do(Action::RecordVoice))
        {
            self.clear_held_mic();
        }
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
                            &self.modal,
                            Some(
                                Modal::Emoji(_)
                                    | Modal::Settings(_)
                                    | Modal::Keybinds(_)
                                    | Modal::IgnoredUsers(_)
                                    | Modal::Report(_)
                                    | Modal::Pinned(_)
                                    | Modal::ReasonConfirm(_)
                                    | Modal::PollForm(_)
                                    | Modal::PollVote(_)
                                    | Modal::Stickers(_)
                                    | Modal::Details(_)
                                    | Modal::Threads(_)
                                    | Modal::Doctor(_)
                            )
                        ) || matches!(
                            &self.modal,
                            Some(Modal::UserInfo(p)) if p.busy.is_none()
                        ) || matches!(
                            &self.modal,
                            Some(Modal::OwnProfile(p)) if p.busy.is_none()
                        ) {
                            self.modal = None;
                        }
                        // Like Esc: what runs for it is given up.
                        if matches!(self.modal, Some(Modal::Collect(_))) {
                            self.close_collect();
                        }
                        if matches!(self.modal, Some(Modal::Sessions(_) | Modal::Keys(_))) {
                            // Like Esc: one step back, and out from the first page.
                            self.devices_click(crate::devices::Click::Cancel);
                        }
                        if matches!(self.modal, Some(Modal::Keywords(_))) {
                            // Like Esc: away from the question, else closed.
                            self.keywords_escape();
                        }
                        if matches!(self.modal, Some(Modal::Saved(_))) {
                            // Like Esc: from the form back to the list, else closed.
                            self.saved_escape();
                        }
                        if matches!(self.modal, Some(Modal::Search(_))) {
                            self.close_search();
                        }
                        if matches!(self.modal, Some(Modal::GlobalSearch(_))) {
                            self.close_global_search();
                        }
                        self.close_manage();
                        // Like Esc: a picker goes back to the room info it came from.
                        if matches!(self.modal, Some(Modal::Palette(_))) {
                            self.close_palette();
                        }
                    }
                    Some(HitId::Field(slot)) => match &mut self.modal {
                        Some(Modal::Login(form)) if form.busy.is_none() => form.focus_slot(slot),
                        Some(Modal::Verify(popup)) => {
                            popup.field_focused = true;
                            popup.focus = None;
                        }
                        Some(Modal::Report(form)) if slot < form.slots() => {
                            form.focus = slot;
                        }
                        Some(Modal::ReasonConfirm(popup))
                            if slot == crate::modal::ReasonConfirmPopup::REASON =>
                        {
                            popup.focus_reason();
                        }
                        _ => {}
                    },
                    Some(HitId::Input(target)) => {
                        let rel_col = hit_rect
                            .map_or(0, |(rect, _)| mouse.column.saturating_sub(rect.x) as usize);
                        self.click_input(target, rel_col);
                    }
                    Some(HitId::Emoji(index)) => self.pick_emoji(index),
                    Some(HitId::Setting(index, delta)) => {
                        if let Some(Modal::Settings(menu)) = &mut self.modal
                            && let Some(setting) = Setting::ALL.get(index).copied()
                            && menu.shows(setting)
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
                    // The room list is out of sight: it follows to the room, out of
                    // a space the room is not in.
                    Some(HitId::BarRoom(index)) => self.show_room(index),
                    Some(HitId::RoomFilter) => {
                        self.cancel_pending_jump();
                        self.set_focus(Pane::Sidebar);
                        if let Some((rect, _)) = hit_rect {
                            let rel_col =
                                mouse.column.saturating_sub(rect.x.saturating_add(3)) as usize;
                            self.sidebar.filter.place(rel_col, false);
                        }
                    }
                    Some(HitId::Message(index)) => {
                        self.cancel_pending_jump();
                        self.set_focus(Pane::Timeline);
                        if self.selectable(index) {
                            self.timeline.selected = Some(index);
                        }
                    }
                    Some(HitId::Reaction(msg, reaction)) => {
                        self.cancel_pending_jump();
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
                        self.cancel_pending_jump();
                        self.set_focus(Pane::Timeline);
                        if self.selectable(message) {
                            self.timeline.selected = Some(message);
                        }
                        self.open_link(message, n);
                    }
                    Some(HitId::ComposerText) => {
                        self.cancel_pending_jump();
                        self.set_focus(Pane::Composer);
                        let view = self.composer_view;
                        let row = (mouse.row - view.area.y) as usize + view.first_row;
                        let col = (mouse.column - view.area.x) as usize;
                        self.composer.place(view.width, row, col);
                        self.sync_completion();
                    }
                    Some(HitId::Button(Button::OwnProfile)) => self.open_own_profile(),
                    Some(HitId::Button(button)) => self.perform(match button {
                        Button::Help => Action::ToggleHelp,
                        Button::Settings => Action::OpenSettings,
                        Button::Theme => Action::CycleTheme,
                        Button::CancelCompose => Action::CancelCompose,
                        Button::JumpToNewest => Action::JumpToNewest,
                        Button::Palette => Action::OpenPalette,
                        Button::OwnProfile => unreachable!(),
                    }),
                    // The microphone is held, not clicked: its own way in.
                    Some(HitId::Do(Action::RecordVoice)) => self.press_mic(),
                    Some(HitId::Do(action)) => self.perform(action),
                    Some(HitId::Pane(pane)) => {
                        self.cancel_pending_jump();
                        self.set_focus(pane);
                    }
                    Some(HitId::Overlay) | None => {}
                    Some(HitId::Image(index) | HitId::PlayMedia(index)) => self.click_image(index),
                    Some(HitId::NoteButton(index, kind)) => self.click_note(index, kind),
                    Some(HitId::ReplyQuote(index)) => {
                        self.cancel_pending_jump();
                        self.set_focus(Pane::Timeline);
                        if self.selectable(index) {
                            self.timeline.selected = Some(index);
                        }
                        self.jump_to_reply(index);
                    }
                    Some(HitId::Thread(index)) => self.open_thread(index),
                    Some(HitId::CopyCode(message, block)) => {
                        self.cancel_pending_jump();
                        self.set_focus(Pane::Timeline);
                        if self.selectable(message) {
                            self.timeline.selected = Some(message);
                        }
                        self.copy_code_block(message, block);
                    }
                    Some(HitId::Attachment(index)) => {
                        self.cancel_pending_jump();
                        self.remove_attachment(index);
                    }
                    Some(HitId::AttachmentPlay(index)) => {
                        self.cancel_pending_jump();
                        self.play_voice_note_at(index);
                    }
                    Some(HitId::Palette(index)) => self.run_palette_at(index),
                    Some(HitId::GlobalSearch(index)) => self.activate_global_search(index),
                    Some(HitId::Complete(index)) => {
                        self.cancel_pending_jump();
                        self.accept_completion(Some(index));
                    }
                    // Handled by `menu_mouse` above.
                    Some(HitId::Menu(_)) => {}
                    Some(HitId::KeybindRow(index)) => self.click_keybind_row(index),
                    Some(HitId::KeybindBtn(button)) => self.click_keybind_button(button),
                    Some(HitId::IgnoredRow(index)) => {
                        if let Some(Modal::IgnoredUsers(popup)) = &mut self.modal
                            && index < popup.filtered.len()
                        {
                            popup.selected = index;
                        }
                    }
                    Some(HitId::IgnoredUnignore(index)) => self.unignore_at(index),
                    Some(HitId::ReportIgnoreToggle) => {
                        if let Some(Modal::Report(form)) = &mut self.modal {
                            if let Some(slot) = form.toggle_slot() {
                                form.focus = slot;
                            }
                            form.flip_ignore();
                        }
                    }
                    Some(HitId::ReportSubmit) => self.submit_report(),
                    Some(HitId::UserSearchRow(index)) => self.pick_user_search(index),
                    Some(HitId::UserSearchInfo(index)) => self.open_user_info_from_search(index),
                    Some(HitId::Sender(index)) => self.open_user_info_from_sender(index),
                    Some(HitId::UserInfoRow(index)) => self.click_user_info_row(index),
                    Some(HitId::UserInfoCopy) => self.copy_user_info_id(),
                    Some(HitId::UserInfoBanSubmit) => self.submit_user_info_ban(),
                    Some(HitId::UserInfoRoleOption(index)) => {
                        self.pick_user_info_role_option(index)
                    }
                    Some(HitId::OwnProfileSlot(slot)) => self.click_own_profile_slot(slot),
                    Some(HitId::PinnedRow(index)) => self.activate_pinned_at(index),
                    Some(HitId::PinnedUnpin(index)) => self.unpin_pinned_at(index),
                    Some(HitId::ReasonConfirmBtn(true)) => self.submit_reason_confirm(),
                    Some(HitId::ReasonConfirmBtn(false)) => self.modal = None,
                    Some(HitId::Toast) => self.click_forward_toast(),
                    Some(HitId::PollAnswer(msg, ans)) => self.click_timeline_poll_answer(msg, ans),
                    Some(HitId::PollFormSlot(slot)) => self.click_poll_form_slot(slot),
                    Some(HitId::PollFormRemove(idx)) => self.remove_poll_answer(idx),
                    Some(HitId::PollFormMaxStep(delta)) => self.step_poll_max(delta),
                    Some(HitId::PollVoteAnswer(idx)) => self.click_poll_vote_answer(idx),
                    Some(HitId::PollVoteBtn(true)) => self.submit_poll_vote(),
                    Some(HitId::PollVoteBtn(false)) => self.modal = None,
                    Some(HitId::StickerRow(index)) => self.send_sticker_at(index),
                    Some(HitId::DetailsFlip) => self.flip_details(),
                    Some(HitId::DetailsCopy) => self.copy_details(),
                    Some(HitId::DoctorCopy) => self.copy_doctor(),
                    Some(HitId::Collect(what)) => self.collect_click(what),
                    Some(HitId::ThreadRow(index)) => self.activate_thread_row(index),
                    Some(HitId::ThreadsMore) => self.click_threads_more(),
                    Some(HitId::ThreadsMine) => self.toggle_threads_mine(),
                    Some(HitId::KeywordRow(index)) => self.click_keyword_row(index),
                    Some(HitId::KeywordRemove(index)) => self.remove_keyword(Some(index)),
                    Some(HitId::SavedRow(index)) => self.activate_saved(Some(index)),
                    Some(HitId::SavedRemind(index)) => self.open_saved_form(Some(index)),
                    Some(HitId::SavedRemove(index)) => self.remove_saved(Some(index)),
                    Some(HitId::ReminderChoice(row)) => self.click_reminder_choice(row),
                    Some(HitId::ReminderDue) => self.click_due_reminder(),
                    // Between two marks: neither of them, and not the row.
                    Some(HitId::SavedGap) => {}
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_splitter => {
                let width = (mouse.column + 1).saturating_sub(self.screen.x);
                self.sidebar.width = width.clamp(SIDEBAR_MIN, max_sidebar(self.screen.width));
                self.dirty = true;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.release_mic();
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
                            | Modal::GlobalSearch(_)
                            | Modal::RoomInfo(_)
                            | Modal::Keybinds(_)
                            | Modal::IgnoredUsers(_)
                            | Modal::Pinned(_)
                            | Modal::Sessions(_)
                            | Modal::UserInfo(_)
                            | Modal::Settings(_)
                            | Modal::PollForm(_)
                            | Modal::PollVote(_)
                            | Modal::Stickers(_)
                            | Modal::Details(_)
                            | Modal::Threads(_)
                            | Modal::Keywords(_)
                            | Modal::Saved(_)
                            | Modal::Doctor(_)
                            | Modal::Collect(_)
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
                    Some(Modal::GlobalSearch(search)) => search.scroll_by(delta),
                    Some(Modal::RoomInfo(_)) => self.manage_scroll(delta),
                    Some(Modal::Keybinds(editor)) => editor.scroll_by(delta),
                    Some(Modal::IgnoredUsers(_)) => self.scroll_ignored_users(delta),
                    Some(Modal::Pinned(_)) => self.scroll_pinned(delta),
                    Some(Modal::Sessions(popup)) => {
                        if matches!(popup.stage, crate::devices::SessionStage::List) {
                            popup.step(self.devices.sessions(), delta.signum());
                        }
                    }
                    Some(Modal::UserInfo(_)) => self.scroll_user_info(delta),
                    Some(Modal::PollForm(_) | Modal::PollVote(_)) => {
                        self.scroll_poll_modal(delta);
                    }
                    Some(Modal::Stickers(_)) => self.scroll_stickers(delta),
                    Some(Modal::Details(_)) => self.scroll_details(delta),
                    Some(Modal::Threads(_)) => self.scroll_threads(delta),
                    Some(Modal::Keywords(_)) => self.scroll_keywords(delta),
                    Some(Modal::Saved(_)) => self.scroll_saved(delta),
                    Some(Modal::Doctor(_)) => self.scroll_doctor(delta),
                    Some(Modal::Collect(_)) => self.scroll_collect(delta),
                    // Row by row: the list follows the selected row. Not while a
                    // value is being typed.
                    Some(Modal::Settings(menu)) if menu.editing.is_none() => {
                        menu.step(delta.signum());
                    }
                    _ => {}
                }
                self.dirty = true;
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if matches!(
                    self.modal,
                    Some(Modal::New(ref popup)) if popup.stage == crate::manage::Stage::Browse
                ) =>
            {
                let delta = if mouse.kind == MouseEventKind::ScrollUp {
                    -1
                } else {
                    1
                };
                self.scroll_dir_browse(delta);
                self.dirty = true;
            }
            // In the find bar the wheel goes from match to match, wherever the pointer
            // is: the bar owns the keyboard in the same way.
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if matches!(self.modal, Some(Modal::Search(_))) =>
            {
                self.search_step(mouse.kind == MouseEventKind::ScrollUp);
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
                        self.cancel_pending_jump();
                        self.timeline.older_armed |= up;
                        self.timeline.newer_armed |= !up;
                        self.timeline.scroll = if up {
                            self.timeline.scroll + WHEEL_LINES
                        } else {
                            self.timeline.scroll.saturating_sub(WHEEL_LINES)
                        };
                    }
                    Some(Pane::Sidebar) => {
                        self.cancel_pending_jump();
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

    fn click_input(&mut self, target: InputTarget, rel_col: usize) {
        match target {
            InputTarget::Devices(slot) => self.click_devices_field(slot, rel_col),
            InputTarget::Keyword => self.click_keywords_input(rel_col),
            InputTarget::Reminder => self.click_saved_input(rel_col),
            InputTarget::Login(slot) => {
                if let Some(Modal::Login(form)) = &mut self.modal
                    && form.busy.is_none()
                {
                    form.focus_slot(slot);
                    if let Some(field) = form.field_at_mut(slot) {
                        field.place(rel_col, slot == 2);
                    }
                }
            }
            InputTarget::Recovery => {
                if let Some(Modal::Recovery(form)) = &mut self.modal
                    && !form.busy
                {
                    form.key.place(rel_col, false);
                }
            }
            InputTarget::DownloadDir => {
                if let Some(Modal::Settings(menu)) = &mut self.modal
                    && let Some(text) = &mut menu.editing
                {
                    text.place(rel_col, false);
                }
            }
            InputTarget::EmojiQuery => {
                if let Some(Modal::Emoji(picker)) = &mut self.modal {
                    picker.query.place(rel_col, false);
                    picker.goal = None;
                }
            }
            InputTarget::PaletteQuery => {
                if let Some(Modal::Palette(palette)) = &mut self.modal {
                    palette.query.place(rel_col, false);
                }
            }
            InputTarget::SearchQuery => {
                if let Some(Modal::Search(bar)) = &mut self.modal {
                    bar.query.place(rel_col, false);
                }
            }
            InputTarget::GlobalSearchQuery => {
                if let Some(Modal::GlobalSearch(search)) = &mut self.modal {
                    search.query.place(rel_col, false);
                }
            }
            InputTarget::NewField(slot) => {
                if let Some(Modal::New(popup)) = &mut self.modal
                    && popup.busy.is_none()
                {
                    if slot < popup.slots() {
                        popup.focus = slot;
                    }
                    if let Some(field) = popup.field_at_mut(slot) {
                        field.place(rel_col, false);
                    }
                }
            }
            InputTarget::InfoEdit(row) => {
                if let Some(Modal::RoomInfo(popup)) = &mut self.modal
                    && popup.busy.is_none()
                    && let Some((editing_row, text)) = &mut popup.editing
                    && *editing_row == row
                {
                    text.place(rel_col, false);
                }
            }
            InputTarget::InfoFilter => {
                if let Some(Modal::RoomInfo(popup)) = &mut self.modal
                    && popup.busy.is_none()
                    && popup.editing.is_none()
                {
                    popup.filter.place(rel_col, false);
                }
            }
            InputTarget::BanReason => match &mut self.modal {
                Some(Modal::RoomInfo(popup)) => {
                    if let Some(prompt) = &mut popup.ban_prompt {
                        prompt.reason.place(rel_col, false);
                    }
                }
                Some(Modal::UserInfo(popup)) => {
                    if let Some(prompt) = &mut popup.ban_prompt {
                        prompt.reason.place(rel_col, false);
                    }
                }
                _ => {}
            },
            InputTarget::VerifyPassword => {
                if let Some(Modal::Verify(popup)) = &mut self.modal
                    && popup.has_field(&self.verify)
                    && let Some(password) = &mut popup.password
                {
                    popup.field_focused = true;
                    popup.focus = None;
                    password.place(rel_col, true);
                }
            }
            InputTarget::IgnoredFilter => {
                if let Some(Modal::IgnoredUsers(popup)) = &mut self.modal {
                    popup.filter.place(rel_col, false);
                }
            }
            InputTarget::StickerFilter => {
                if let Some(Modal::Stickers(picker)) = &mut self.modal {
                    picker.filter.place(rel_col, false);
                }
            }
            InputTarget::ThreadsFilter => self.click_threads_filter(rel_col),
            InputTarget::ReportReason => {
                if let Some(Modal::Report(form)) = &mut self.modal {
                    form.focus = ReportForm::REASON;
                    form.reason.place(rel_col, false);
                }
            }
            InputTarget::PollQuestion => {
                if let Some(Modal::PollForm(form)) = &mut self.modal {
                    form.focus = 0;
                    form.question.place(rel_col, false);
                }
            }
            InputTarget::PollAnswer(i) => {
                if let Some(Modal::PollForm(form)) = &mut self.modal
                    && i < form.answers.len()
                {
                    form.focus = 1 + i;
                    form.answers[i].place(rel_col, false);
                }
            }
            InputTarget::ReasonConfirm => {
                if let Some(Modal::ReasonConfirm(popup)) = &mut self.modal {
                    popup.focus_reason();
                    popup.reason.place(rel_col, false);
                }
            }
            InputTarget::OwnProfileName => {
                if let Some(Modal::OwnProfile(popup)) = &mut self.modal
                    && popup.busy.is_none()
                {
                    popup.set_focus(crate::people::OwnProfilePopup::NAME_INPUT);
                    popup.display_name.place(rel_col, false);
                }
            }
            InputTarget::OwnProfileAvatar => {
                if let Some(Modal::OwnProfile(popup)) = &mut self.modal
                    && popup.busy.is_none()
                {
                    popup.set_focus(crate::people::OwnProfilePopup::AVATAR_PATH);
                    popup.avatar_path.place(rel_col, false);
                }
            }
            InputTarget::RoomSettingEncrypt => {
                if let Some(Modal::RoomInfo(info)) = &mut self.modal
                    && info.busy.is_none()
                    && let Some(crate::manage::SettingSubview::EncryptConfirm {
                        confirm,
                        focus,
                        ..
                    }) = &mut info.setting_view
                {
                    *focus = 0;
                    confirm.place(rel_col, false);
                }
            }
            InputTarget::RoomSettingAvatar => {
                if let Some(Modal::RoomInfo(info)) = &mut self.modal
                    && info.busy.is_none()
                    && let Some(crate::manage::SettingSubview::Picture { path, focus, .. }) =
                        &mut info.setting_view
                {
                    *focus = 0;
                    path.place(rel_col, false);
                }
            }
            InputTarget::RoomSettingAlias => {
                if let Some(Modal::RoomInfo(info)) = &mut self.modal
                    && info.busy.is_none()
                    && let Some(crate::manage::SettingSubview::Addresses { input, focus, .. }) =
                        &mut info.setting_view
                {
                    *focus = 0;
                    input.place(rel_col, false);
                }
            }
        }
    }

    fn on_tick(&mut self) {
        self.verify_tick();
        self.maybe_open_startup_target();
        self.devices_watch();
        self.keywords_watch();
        self.saved_tick(saved::TEXT_WAIT);
        self.tick_global_search();
        self.release_count_notices(COUNT_NOTICE_WAIT);
        self.save_idle_draft(drafts::IDLE);
        self.settle_unread_line();
        if self
            .toast
            .as_ref()
            .is_some_and(|(_, since)| since.elapsed() >= TOAST_TIME)
        {
            self.toast = None;
            self.toast_room = None;
            self.dirty = true;
        }
        if self.images.tick() {
            self.dirty = true;
        }
        self.audio_tick();
        self.recording_tick();
    }
}

const PATH_OPTIONS_SHOWN: usize = 6;

/// Formats up to [`PATH_OPTIONS_SHOWN`] file completion candidates for a notice or toast.
pub(super) fn format_completion_options(options: &[String]) -> Option<String> {
    if options.is_empty() {
        return None;
    }
    let shown = options.len().min(PATH_OPTIONS_SHOWN);
    let mut choices = options[..shown].join("  ");
    if options.len() > shown {
        choices.push_str(&format!("  … and {} more", options.len() - shown));
    }
    Some(choices)
}

/// Outcome of Tab-completing a single-file [`LineEdit`] path field.
pub(super) enum PathCompleteResult {
    /// Empty input: caller falls through to normal Tab navigation.
    Empty,
    /// Matched one or more candidates (updates `field` when `changed` is true).
    Handled {
        notice: Option<String>,
        changed: bool,
        has_options: bool,
    },
    /// Nothing on disk matched.
    NoMatch,
}

/// Tab-completes a single-file path input via [`crate::upload::complete`].
pub(super) fn complete_path_field(field: &mut LineEdit) -> PathCompleteResult {
    let current = field.as_str().to_owned();
    if current.is_empty() {
        return PathCompleteResult::Empty;
    }
    match crate::upload::complete(&current) {
        Some(completion) => {
            let changed = completion.text != current;
            let has_options = !completion.options.is_empty();
            if changed {
                field.set(&completion.text);
            }
            PathCompleteResult::Handled {
                notice: format_completion_options(&completion.options),
                changed,
                has_options,
            }
        }
        None => PathCompleteResult::NoMatch,
    }
}

/// Extracts a single dropped image file path from a terminal paste, if any.
pub(super) fn dropped_single_image_path(full_text: &str) -> Result<Option<String>, &'static str> {
    let Some(paths) = crate::upload::dropped_files(full_text) else {
        return Ok(None);
    };
    if paths.len() > 1 {
        return Err("Drop one image file.");
    }
    Ok(paths.into_iter().next().map(|p| p.display().to_string()))
}

/// Validates a typed or dropped path as a single supported image within the server limit.
pub(super) fn validate_avatar_path(raw: &str, limit: Option<u64>) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("Enter an image file path first.".to_owned());
    }
    let files = crate::upload::command_files(raw)?;
    if files.len() != 1 {
        return Err("Choose a single image file.".to_owned());
    }
    let path = &files[0].path;
    let prepared = crate::upload::prepare(path, limit)?;
    if prepared.picture.is_none() || !prepared.mime.starts_with("image/") {
        return Err(format!("{} is not a supported image", prepared.name));
    }
    Ok(path.clone())
}

/// Best effort; the login form also shows the address to open by hand.
#[cfg(not(test))]
fn open_in_browser(url: &str) {
    use std::process::{Command, Stdio};
    let _ = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
thread_local! {
    static TEST_OPENED_URLS: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn open_in_browser(url: &str) {
    TEST_OPENED_URLS.with(|urls| urls.borrow_mut().push(url.to_owned()));
}

/// Waits up to `grace` for `tasks` to end on their own. Returns `false` if any task
/// outlived the grace and had to be aborted (after aborting, drains briefly so drop
/// handlers — such as killing child process groups — run before returning).
async fn drain_or_abort(tasks: &mut tokio::task::JoinSet<()>, grace: Duration) -> bool {
    if tasks.is_empty() {
        return true;
    }
    let done = async { while tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(grace, done).await.is_ok() {
        return true;
    }
    tasks.abort_all();
    let aborted = async { while tasks.join_next().await.is_some() {} };
    let _ = tokio::time::timeout(Duration::from_millis(500), aborted).await;
    false
}

pub async fn run(paths: &Paths, config: &Config, demo: bool, target: Option<String>) -> Result<()> {
    let (portal_scheme, mut scheme_watch) = crate::portal::SchemeWatch::start().await;
    let guard = TerminalGuard::enter(config.mouse, config.window_title)?;
    // Terminal queries (image protocol, background colour) go here: on the alternate
    // screen, where a terminal that prints a question instead of answering does no
    // harm, but before mouse and focus reports are on and before the event stream.
    guard.alternate_screen()?;
    let (graphics, term_reply) =
        crate::images::Graphics::detect_with_term(config.images, portal_scheme.is_none());
    let initial_scheme = crate::portal::resolve_initial_scheme(portal_scheme, term_reply);
    let mut terminal = guard.activate()?;
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Without these, a kill or a closed terminal window would skip all cleanup.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let (matrix, mut matrix_events) = if demo {
        (None, None)
    } else {
        let (handle, events) = matrix::spawn_with(paths.clone(), config.secret_store);
        (Some(handle), Some(events))
    };
    let mut app = App::new_in_scheme(config, paths.themes_dir(), matrix.clone(), initial_scheme);
    // As the terminal answered before the event stream existed: never asked again.
    app.found.keyboard_enhanced = Some(guard.keyboard_enhanced());
    // The demo is for looking around: what is changed there is not written into the
    // real configuration.
    if !demo {
        app.config_file = Some(paths.config_file());
        // Read when a session is up: whose list it is must be known first.
        app.bookmarks.file = Some(paths.saved_file());
    }
    app.startup_target = target;
    // Copies of sounds and mpv's sockets; what an earlier run left there goes now.
    app.set_audio_dir(paths.audio_dir());
    // The demo keeps no notes in a file, as it writes no settings.
    if !demo {
        app.set_note_paths(paths.audio_dir(), paths.notes_file());
    }
    if demo {
        app.add_demo_space();
        app.maybe_open_startup_target();
    }
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
    // Clicks on desktop notifications come back here, as the room to open.
    let (notice_clicks, mut clicked) =
        tokio::sync::mpsc::unbounded_channel::<crate::notify::Click>();
    // Pictures read from the clipboard come back here.
    let (pasted, mut pasted_back) = tokio::sync::mpsc::unbounded_channel();
    // What the audio players say comes back here; their tasks end with them.
    let (player_news, mut player_back) = tokio::sync::mpsc::unbounded_channel();
    let mut players = tokio::task::JoinSet::new();
    // And what a recording says; kept separate so releasing the terminal for `$EDITOR`
    // can drain recordings without disturbing playback.
    let (record_news, mut record_back) = tokio::sync::mpsc::unbounded_channel();
    let mut recorders = tokio::task::JoinSet::new();
    // So do the programs that transcribe and translate, and what they wrote.
    let (note_news, mut note_back) = tokio::sync::mpsc::unbounded_channel();
    let mut note_programs = tokio::task::JoinSet::new();
    // The same for the summary command of "catch up".
    let (summary_news, mut summary_back) = tokio::sync::mpsc::unbounded_channel();
    let mut summaries = tokio::task::JoinSet::new();

    while !app.should_quit {
        if app.suspend_requested {
            app.suspend_requested = false;
            // What played or recorded was told to stop: wait for them to end, and abort
            // any that outlive the grace so no child process group stays alive across
            // SIGTSTP (they are not stopped with the terminal).
            let _ = drain_or_abort(&mut players, Duration::from_secs(1)).await;
            if !drain_or_abort(&mut recorders, Duration::from_secs(2)).await {
                app.cancel_recording();
            }
            // The terminal frees its copies of the pictures now, in case we are
            // never resumed; they are sent again when drawn.
            app.flush_images(true);
            terminal::suspend(&mut terminal)?;
            // The shell's title was put back meanwhile.
            app.window_title_lost();
            app.dirty = true;
        }
        if let Some(request) = app.take_editor_request() {
            // A running recording was told to stop when the editor was requested; wait
            // for it to finish (or kill it) before giving the terminal to the editor.
            if !drain_or_abort(&mut recorders, Duration::from_secs(2)).await {
                app.cancel_recording();
            }
            // As for Ctrl-Z: the terminal frees its copies of the pictures now.
            app.flush_images(true);
            // Its reader thread would take the keys meant for the editor: the stream
            // is dropped, and a new one is read from once we are back.
            drop(std::mem::replace(&mut events, EventStream::new()));
            tokio::time::sleep(Duration::from_millis(50)).await;
            terminal::absorb_reader_wake();
            terminal::release();
            let dir = paths.runtime_dir.clone();
            // A blocking child: the worker, a task of its own, goes on syncing and its
            // events wait in their channel.
            let outcome = tokio::task::spawn_blocking(move || {
                crate::editor::edit(&request, &dir, crate::editor::run_command)
            })
            .await
            .unwrap_or(crate::editor::Outcome::Failed(
                crate::editor::Failure::Launch,
            ));
            terminal::absorb_reader_wake();
            let resumed = terminal::resume(&mut terminal);
            // The screen was cleared: pictures are sent again when drawn. The title
            // from before was put back for the editor: ours is written again too.
            app.images.forget_encoded();
            app.window_title_lost();
            app.finish_editor(outcome);
            resumed?;
        }
        if let Some(request) = app.take_paste_request() {
            // Off the UI thread: the tool may take its time (up to its limit), and the
            // picture is decoded to check it. Messages and keys are handled meanwhile.
            let dir = paths.runtime_dir.clone();
            let back = pasted.clone();
            tokio::spawn(async move {
                let id = request.id;
                let result = tokio::task::spawn_blocking(move || {
                    crate::clipboard::paste(&request, &dir, crate::clipboard::run_tool)
                })
                .await
                .unwrap_or(Err(crate::clipboard::Failure::Failed));
                // Nobody to take it only once the app has ended: then the picture's
                // file is removed as the answer is dropped.
                let _ = back.send(crate::clipboard::Done { id, result });
            });
        }
        if let Some(request) = app.take_audio_start() {
            // A task on the async threads, which live as long as the app: the player is
            // started from there (see `audio::play`).
            players.spawn(crate::audio::run(request, player_news.clone()));
        }
        if let Some(request) = app.take_record_start() {
            // As the players: the programs are tied to the thread that starts them.
            recorders.spawn(crate::audio::record::run(request, record_news.clone()));
        }
        while players.try_join_next().is_some() {}
        while recorders.try_join_next().is_some() {}
        for request in app.take_note_starts() {
            note_programs.spawn(crate::textcmd::run(request, note_news.clone()));
        }
        while note_programs.try_join_next().is_some() {}
        if let Some(request) = app.take_summary_start() {
            summaries.spawn(crate::catchup::run(request, summary_news.clone()));
        }
        while summaries.try_join_next().is_some() {}
        terminal::set_mouse(app.mouse)?;
        terminal::set_window_title(app.window_title)?;
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
                Some(&notice_clicks),
            );
        }
        for (notice, icon) in &ready.reminders {
            crate::notify::show_reminder(
                ready.mode,
                notice,
                ready.preview,
                icon,
                ready.icon_cache.clone(),
                Some(&notice_clicks),
            );
        }
        if app.dirty {
            app.dirty = false;
            terminal.draw(|frame| ui::draw(frame, &mut app))?;
            app.after_draw();
            app.flush_images(false);
            // Only when the count changed, and never with the setting off.
            if let Some(title) = app.take_window_title() {
                terminal::write_window_title(&title)?;
            }
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
            Some(click) = clicked.recv() => app.on_matrix(MatrixEvent::NoticeClicked {
                room_id: click.room_id,
                event: click.event,
            }),
            Some(done) = pasted_back.recv() => app.finish_paste(done),
            Some(update) = player_back.recv() => app.on_audio(update),
            Some(done) = note_back.recv() => app.on_note_done(done),
            Some(done) = summary_back.recv() => app.on_summary(done),
            Some(update) = record_back.recv() => app.on_recording(update),
            _ = theme_watch.changed() => app.reload_theme(),
            scheme = scheme_watch.changed() => app.on_color_scheme(scheme),
            Some(done) = encoded_images.recv() => app.on_image_encoded(done),
            _ = wait_deadline(app.search_debounce_deadline()) => app.tick_global_search(),
            _ = wait_deadline(app.user_search_debounce_deadline()) => app.tick_user_search(),
            _ = wait_deadline(app.dir_search_debounce_deadline()) => app.tick_dir_search(),
            _ = tick.tick() => app.on_tick(),
            _ = tokio::signal::ctrl_c() => app.should_quit = true,
            _ = sigterm.recv() => app.should_quit = true,
            _ = sighup.recv() => app.should_quit = true,
        }
    }
    app.flush_images(true);
    // What plays stops with the app, and its file goes; a player that does not end in
    // time is killed as its task is dropped.
    app.stop_audio();
    // So do the programs that transcribe and translate, and a recording.
    app.stop_notes();
    app.cancel_recording();
    let stopped = async {
        while players.join_next().await.is_some() {}
        while recorders.join_next().await.is_some() {}
        while note_programs.join_next().await.is_some() {}
    };
    let _ = tokio::time::timeout(Duration::from_secs(2), stopped).await;
    drop(players);
    drop(recorders);
    drop(note_programs);
    // A summary command that still runs is killed as its task is dropped.
    drop(summaries);
    // Let the worker finish handing any queued files to the send queue and stop
    // syncing cleanly, while `app` stays alive so temporary voice and picture files
    // stay on disk until the worker has read them.
    if let (Some(matrix), Some(mut events)) = (matrix, matrix_events) {
        let wait = if app.attach.sends_out() {
            Duration::from_secs(7)
        } else {
            Duration::from_secs(3)
        };
        // The worker takes commands in order: the draft is written before it stops.
        app.shut_down_worker();
        // Close every command sender so the worker also sees the channel end.
        drop(matrix);
        let drained = async { while events.recv().await.is_some() {} };
        let _ = tokio::time::timeout(wait, drained).await;
    }
    drop(app);
    crate::audio::remove_leftovers(&paths.audio_dir());
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

    /// Alt-↑: from the composer to the newest message, whatever the composer holds.
    fn alt_up() -> Event {
        key_mod(KeyCode::Up, KeyModifiers::ALT)
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
    fn alt_up_from_composer_walks_messages_and_down_returns() {
        let (mut app, _) = drawn();
        let last = app.rooms[0].messages.len() - 1;
        app.on_event(alt_up());
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

    /// The demo's first room with these messages instead: `(own, text)`.
    fn with_messages(messages: &[(bool, &str)]) -> App {
        let (mut app, _) = drawn();
        app.rooms[0].messages = messages
            .iter()
            .enumerate()
            .map(|(n, (own, body))| Message {
                id: n.to_string(),
                sender: if *own { OWN_NAME } else { "ann" }.to_owned(),
                body: (*body).to_owned(),
                own: *own,
                ..Message::default()
            })
            .collect();
        app
    }

    #[test]
    fn up_in_an_empty_composer_walks_into_the_messages() {
        let mut app = with_messages(&[(false, "hello"), (true, "mine"), (false, "bye")]);
        app.on_event(key(KeyCode::Up));
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert!(app.composer.is_empty());
        assert_eq!(app.focus, Pane::Timeline);
        assert_eq!(app.timeline.selected, Some(2));
    }

    #[test]
    fn edit_last_edits_the_own_latest_message() {
        let mut app = with_messages(&[
            (false, "hello"),
            (true, "first"),
            (true, "second"),
            (true, "a file"),
            (true, "gone"),
            (false, "bye"),
        ]);
        // Not everything of ours can be edited: the newest that can is taken.
        app.rooms[0].messages[3].file = Some(crate::model::Attachment {
            key: "k".into(),
            name: "a.pdf".into(),
            size: None,
            kind: crate::model::MediaKind::File,
            source: Box::new(matrix_sdk::ruma::events::room::MediaSource::Plain(
                "mxc://example.org/a".into(),
            )),
            audio: None,
        });
        app.rooms[0].messages[4].kind = MsgKind::Redacted;
        app.perform(Action::EditLast);
        assert_eq!(app.compose_mode, ComposeMode::Edit(2));
        assert_eq!(app.composer.text(), "second");
        assert_eq!(app.focus, Pane::Composer);
        assert_eq!(app.timeline.selected, None);

        // It is an edit like any other: Enter changes the message, nothing is added.
        type_text(&mut app, "!");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.rooms[0].messages.len(), 6);
        let edited = &app.rooms[0].messages[2];
        assert_eq!((edited.body.as_str(), edited.edited), ("second!", true));
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert!(app.composer.is_empty());

        // Esc gives it up and leaves the composer empty again.
        app.perform(Action::EditLast);
        assert_eq!(app.compose_mode, ComposeMode::Edit(2));
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert!(app.composer.is_empty());

        // While editing, Up goes to the messages and leaves the edit as it is.
        app.perform(Action::EditLast);
        app.on_event(key(KeyCode::Up));
        assert_eq!(app.compose_mode, ComposeMode::Edit(2));
        assert_eq!(app.focus, Pane::Timeline);
    }

    #[test]
    fn edit_last_edits_an_action_as_an_action() {
        let mut app = with_messages(&[(true, "waves")]);
        app.rooms[0].messages[0].kind = MsgKind::Emote;
        app.perform(Action::EditLast);
        assert_eq!(app.compose_mode, ComposeMode::Edit(0));
        assert_eq!(app.composer.text(), "/me waves");
    }

    #[test]
    fn up_with_text_in_the_composer_selects_a_message_as_before() {
        let mut app = with_messages(&[(true, "mine"), (false, "theirs")]);
        type_text(&mut app, "draft");
        app.on_event(key(KeyCode::Up));
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert_eq!(app.composer.text(), "draft");
        assert_eq!(app.focus, Pane::Timeline);
        assert_eq!(app.timeline.selected, Some(1));

        // Nor does it start an edit in the middle of a reply.
        let mut app = with_messages(&[(true, "mine"), (false, "theirs")]);
        app.timeline.selected = Some(1);
        app.perform(Action::Reply);
        app.on_event(key(KeyCode::Up));
        assert_eq!(app.compose_mode, ComposeMode::Reply(1));
        assert_eq!(app.focus, Pane::Timeline);
    }

    #[test]
    fn edit_last_leaves_a_message_that_was_not_sent_to_the_messages_pane() {
        let mut app = with_messages(&[(true, "sent"), (true, "not sent")]);
        app.rooms[0].messages[1].state = SendState::Failed;
        app.perform(Action::EditLast);
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert_eq!(app.focus, Pane::Timeline);
        assert_eq!(app.timeline.selected, Some(1));

        // One that is still on its way can be corrected.
        app.rooms[0].messages[1].state = SendState::Sending;
        app.perform(Action::Focus(Pane::Composer));
        app.perform(Action::EditLast);
        assert_eq!(app.compose_mode, ComposeMode::Edit(1));
    }

    #[test]
    fn up_without_an_own_message_selects_a_message_as_before() {
        let mut app = with_messages(&[(false, "one"), (false, "two")]);
        app.on_event(key(KeyCode::Up));
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert!(app.composer.is_empty());
        assert_eq!(app.focus, Pane::Timeline);
        assert_eq!(app.timeline.selected, Some(1));
    }

    #[test]
    fn edit_last_follows_its_key() {
        let mut app = with_messages(&[(true, "mine"), (false, "theirs")]);
        app.keymap.apply_rebind(
            Context::Composer,
            Action::EditLast,
            KeyCode::F(8),
            KeyModifiers::NONE,
        );
        app.on_event(key(KeyCode::F(8)));
        assert_eq!(app.compose_mode, ComposeMode::Edit(0));
        assert_eq!(app.composer.text(), "mine");
    }

    #[test]
    fn edit_last_is_in_the_palette_once_with_or_without_a_key() {
        let listed = |app: &mut App| {
            app.perform(Action::OpenPalette);
            let count = match &app.modal {
                Some(Modal::Palette(palette)) => (0..)
                    .map_while(|at| palette.entry_at(at))
                    .filter(|entry| entry.title == "Edit your last message")
                    .count(),
                _ => panic!("no palette"),
            };
            app.modal = None;
            count
        };
        let mut app = with_messages(&[(true, "mine"), (false, "theirs")]);
        assert_eq!(listed(&mut app), 1);
        app.keymap.apply_rebind(
            Context::Composer,
            Action::EditLast,
            KeyCode::F(8),
            KeyModifiers::NONE,
        );
        assert_eq!(listed(&mut app), 1);
    }

    #[test]
    fn edit_last_says_why_when_there_is_nothing_to_edit() {
        let toast = |app: &App| app.toast.as_ref().map(|(text, _)| text.clone());
        // Nothing of ours in the room.
        let mut app = with_messages(&[(false, "theirs")]);
        app.perform(Action::EditLast);
        assert_eq!(
            toast(&app).as_deref(),
            Some("There is no message of yours to edit here")
        );
        assert_eq!(app.focus, Pane::Composer);
        assert_eq!(app.timeline.selected, None);
        // Something is being written.
        let mut app = with_messages(&[(true, "mine")]);
        for c in "draft".chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        app.perform(Action::EditLast);
        assert_eq!(
            toast(&app).as_deref(),
            Some("Finish what you are writing first")
        );
        assert_eq!(app.composer.text(), "draft");
        assert_eq!(app.compose_mode, ComposeMode::Normal);
    }

    #[test]
    fn edit_last_works_in_the_vim_preset_too() {
        let mut app = with_messages(&[(true, "mine"), (false, "theirs")]);
        app.keymap = Keymap::from_preset(keymap::Preset::Vim);
        app.perform(Action::EditLast);
        assert_eq!(app.compose_mode, ComposeMode::Edit(0));
        // Esc is "to the messages" there and leaves the edit as it is, like one begun
        // with `e`; Ctrl-C gives it up.
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.focus, Pane::Timeline);
        assert_eq!(app.compose_mode, ComposeMode::Edit(0));
        app.on_event(key(KeyCode::Char('i')));
        assert_eq!(app.focus, Pane::Composer);
        app.on_event(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert!(app.composer.is_empty());
    }

    #[test]
    fn ctrl_c_gives_up_an_edit_or_a_reply_even_with_nothing_typed() {
        // An edit whose text was erased by hand.
        let mut app = with_messages(&[(true, "ab"), (false, "theirs")]);
        app.perform(Action::EditLast);
        app.on_event(key(KeyCode::Backspace));
        app.on_event(key(KeyCode::Backspace));
        assert!(app.composer.is_empty());
        assert_eq!(app.compose_mode, ComposeMode::Edit(0));
        app.on_event(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert!(!app.should_quit);
        assert_eq!(app.rooms[0].messages[0].body, "ab");

        // The draft that waited behind the edit is back, not cleared with it.
        let mut app = with_messages(&[(true, "ab")]);
        type_text(&mut app, "draft");
        app.timeline.selected = Some(0);
        app.perform(Action::Edit);
        app.on_event(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert_eq!(app.composer.text(), "draft");

        // A reply that was begun and not written.
        let mut app = with_messages(&[(false, "theirs")]);
        app.timeline.selected = Some(0);
        app.perform(Action::Reply);
        app.on_event(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        // With nothing to give up it still only says how to quit.
        app.on_event(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.toast.is_some());
        assert!(!app.should_quit);
    }

    #[test]
    fn the_edit_banner_names_the_key_that_cancels() {
        let mut app = with_messages(&[(true, "mine")]);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        app.perform(Action::EditLast);
        redraw(&mut app, &mut terminal);
        find(&terminal, "✎ Editing your message · Esc cancel");

        // In the vim preset Esc goes to the messages; Ctrl-C is what cancels.
        app.keymap = Keymap::from_preset(keymap::Preset::Vim);
        redraw(&mut app, &mut terminal);
        find(&terminal, "✎ Editing your message · Ctrl-C cancel");

        // A key of its own for it is named first.
        app.keymap.apply_rebind(
            Context::Composer,
            Action::CancelCompose,
            KeyCode::F(9),
            KeyModifiers::NONE,
        );
        redraw(&mut app, &mut terminal);
        find(&terminal, "✎ Editing your message · F9 cancel");
        app.on_event(key(KeyCode::F(9)));
        assert_eq!(app.compose_mode, ComposeMode::Normal);
    }

    #[test]
    fn reply_edit_delete_react() {
        let (mut app, _) = drawn();
        // Reply to the newest message.
        app.on_event(alt_up());
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.focus, Pane::Composer);
        assert!(matches!(app.compose_mode, ComposeMode::Reply(_)));
        type_text(&mut app, "congrats");
        app.on_event(key(KeyCode::Enter));
        let sent = app.rooms[0].messages.last().unwrap();
        assert_eq!(sent.reply_to.as_ref().unwrap().0, "dave");
        assert_eq!(app.compose_mode, ComposeMode::Normal);

        // Edit it.
        app.on_event(alt_up());
        app.on_event(key(KeyCode::Char('e')));
        assert_eq!(app.composer.text(), "congrats");
        type_text(&mut app, "!");
        let count = app.rooms[0].messages.len();
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.rooms[0].messages.len(), count);
        let edited = app.rooms[0].messages.last().unwrap();
        assert_eq!((edited.body.as_str(), edited.edited), ("congrats!", true));

        // React, then un-react.
        app.on_event(alt_up());
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

        app.on_event(alt_up());
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
    fn escape_closes_one_layer_at_a_time_down_to_no_chat() {
        let (mut app, mut terminal) = drawn();
        let left = app.active;
        assert!(
            screen_text(&terminal).contains('▌'),
            "the open room is marked"
        );

        // The help first.
        app.on_event(key(KeyCode::F(1)));
        app.on_event(key(KeyCode::Esc));
        assert!(!app.help_open && !app.chat_closed());
        // From the messages back to the composer.
        app.on_event(alt_up());
        assert_eq!(app.focus, Pane::Timeline);
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.focus, Pane::Composer);
        assert!(!app.chat_closed());
        // A reply that was begun is given up.
        app.on_event(alt_up());
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(app.compose_mode, ComposeMode::Reply(_)));
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert!(!app.chat_closed());
        // A thread is left for its room, where the focus was.
        app.on_event(alt_up());
        app.perform(Action::OpenThread);
        assert!(app.thread_open());
        assert_eq!(app.focus, Pane::Composer);
        app.on_event(key(KeyCode::Esc));
        assert!(!app.thread_open() && !app.chat_closed());
        assert_eq!(app.focus, Pane::Timeline);
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.focus, Pane::Composer);
        assert!(!app.chat_closed());

        // Nothing is left to cancel: the chat closes.
        app.on_event(key(KeyCode::Esc));
        assert!(app.chat_closed());
        assert!(app.room().is_none() && app.room_id().is_none());
        assert_eq!(app.focus, Pane::Sidebar);
        // The cursor is on the room that was left, and no room is the open one.
        assert_eq!(app.cursor_room(), Some(left));
        assert_eq!(app.open_index(), None);
        redraw(&mut app, &mut terminal);
        find(&terminal, "No chat open");
        let screen = screen_text(&terminal);
        assert!(!screen.contains('▌'), "{screen}");
        assert!(!screen.contains("Type a message"), "{screen}");

        // Once more changes nothing...
        app.on_event(key(KeyCode::Esc));
        assert!(app.chat_closed());
        assert_eq!((app.focus, app.cursor_room()), (Pane::Sidebar, Some(left)));
        // ...but for a filter of the room list, which it clears as ever.
        type_text(&mut app, "bo");
        assert_eq!(app.visible_rooms().len(), 1);
        app.on_event(key(KeyCode::Esc));
        assert!(app.sidebar.filter.is_empty());
        assert!(app.chat_closed());
        assert_eq!(app.focus, Pane::Sidebar);
    }

    #[test]
    fn with_the_vim_preset_stepping_back_from_the_composer_needs_a_key_of_its_own() {
        let (mut app, _terminal) = drawn();
        let config =
            Config::parse("preset = \"vim\"\n[keys.composer]\nback = \"alt-q\"\n").unwrap();
        app.keymap = Keymap::from_config(&config).0;
        // Esc is the way to the messages there, and from the messages back.
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.focus, Pane::Timeline);
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.focus, Pane::Composer);
        assert!(!app.chat_closed());
        // The key given to `back` in the composer closes the chat.
        app.on_event(alt('q'));
        assert!(app.chat_closed());
        assert_eq!(app.focus, Pane::Sidebar);
    }

    #[test]
    fn the_room_that_was_left_is_filtered_like_any_other() {
        let (mut app, _terminal) = drawn();
        let left = app.active;
        assert!(!app.rooms[left].shows_unread());
        // Open, it stays in the list whatever the quick filters say...
        app.perform(Action::FilterUnread);
        assert!(app.visible_rooms().contains(&left));
        app.perform(Action::FilterUnread);
        // ...left, it is a room like the others.
        app.on_event(key(KeyCode::Esc));
        assert!(app.chat_closed());
        assert_eq!(app.cursor_room(), Some(left));
        app.perform(Action::FilterUnread);
        assert!(!app.visible_rooms().contains(&left));
        assert!(app.cursor_room().is_some_and(|room| room != left));
        // Closed with the filter on, the cursor is on a room that is listed.
        let (mut app, _terminal) = drawn();
        app.perform(Action::FilterUnread);
        app.on_event(key(KeyCode::Esc));
        assert!(app.chat_closed());
        assert!(app.cursor_room().is_some_and(|room| room != left));
    }

    /// The rooms of `connected`, with no chat open and nothing waiting to be read
    /// from the worker's side.
    fn closed() -> (
        App,
        Terminal<TestBackend>,
        tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>,
    ) {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_event(key(KeyCode::Esc));
        assert!(app.chat_closed());
        redraw(&mut app, &mut terminal);
        drain(&mut commands);
        (app, terminal, commands)
    }

    fn open_id(app: &App) -> Option<&str> {
        app.room().map(|room| room.id.as_str())
    }

    #[test]
    fn a_closed_chat_keeps_its_draft_and_gives_it_back() {
        let (mut app, mut terminal, mut commands) = connected();
        drain(&mut commands);
        type_text(&mut app, "half");
        assert_eq!(drain(&mut commands), ["Typing(!a, true)"]);
        app.on_event(key(KeyCode::Esc));
        assert!(app.chat_closed());
        // As a room switch does it: the typing notice is taken back, the draft goes
        // to the store and waits with its room. The worker stops the room's timeline.
        assert_eq!(
            drain(&mut commands),
            ["Typing(!a, false)", "SaveDraft(!a, 4 bytes)", "CloseRoom"]
        );
        assert!(app.composer.is_empty());
        assert_eq!(app.rooms[0].draft, "half");
        assert_eq!(app.compose_mode, ComposeMode::Normal);

        // Enter on the room that was left (the cursor is on it): the draft is back,
        // with the cursor at its end.
        app.on_event(key(KeyCode::Enter));
        assert!(!app.chat_closed());
        assert_eq!(open_id(&app), Some("!a"));
        assert_eq!(app.focus, Pane::Composer);
        assert_eq!(app.composer.text(), "half");
        assert_eq!(app.composer.cursor(), 4);
        assert!(app.rooms[0].draft.is_empty());
        let seen = drain(&mut commands);
        assert!(seen.contains(&"OpenRoom(!a)".to_owned()), "{seen:?}");
        // Nothing was written meanwhile: nothing is saved again.
        assert!(!seen.iter().any(|c| c.starts_with("SaveDraft")), "{seen:?}");

        // The same by a click on its row.
        app.on_event(key(KeyCode::Esc));
        assert!(app.chat_closed());
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "Alpha");
        app.on_event(click(col, row));
        assert_eq!(open_id(&app), Some("!a"));
        assert_eq!(app.composer.text(), "half");
        assert_eq!(app.focus, Pane::Composer);

        // An edit that was begun is given up before the chat can close, and the
        // draft behind it is what waits.
        app.on_matrix(timeline(&app, vec![remote("1", "me", "mine")]));
        app.on_event(alt_up());
        app.on_event(key(KeyCode::Char('e')));
        assert!(matches!(app.compose_mode, ComposeMode::Edit(_)));
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert!(!app.chat_closed());
        app.on_event(key(KeyCode::Esc));
        assert!(app.chat_closed());
        assert_eq!(app.rooms[0].draft, "half");
    }

    #[test]
    fn no_chat_opens_by_itself_when_the_room_list_changes() {
        let (mut app, mut terminal, mut commands) = closed();
        let invited = || summary("!i", "Invited", RoomCategory::Invite, 0);
        // The list in another order, with news in the room that was left: its
        // count grows like any other room's.
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!b", "Beta", RoomCategory::Room, 0),
            summary("!a", "Alpha", RoomCategory::Room, 3),
            invited(),
        ]));
        assert!(app.chat_closed());
        assert_eq!(app.rooms[app.active].id, "!a");
        assert_eq!(app.rooms[app.active].unread, 3);
        assert_eq!(app.cursor_room(), Some(app.active));
        assert_eq!(app.focus, Pane::Sidebar);

        // The room that was left is gone (left elsewhere, or removed from it).
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!b", "Beta", RoomCategory::Room, 0),
            invited(),
        ]));
        assert!(app.chat_closed());
        assert!(app.room().is_none());
        assert!(app.active < app.rooms.len());
        assert_eq!(app.cursor_room(), Some(app.active));
        assert!(app.composer.is_empty());
        redraw(&mut app, &mut terminal);
        app.after_draw();
        find(&terminal, "No chat open");

        // Not even the first list after an empty one opens a room.
        app.on_matrix(MatrixEvent::Rooms(Vec::new()));
        assert!(app.chat_closed());
        redraw(&mut app, &mut terminal);
        app.after_draw();
        find(&terminal, "no rooms yet");
        find(&terminal, "No chat open");
        app.on_matrix(MatrixEvent::Rooms(vec![summary(
            "!b",
            "Beta",
            RoomCategory::Room,
            0,
        )]));
        assert!(app.chat_closed());
        let seen = drain(&mut commands);
        assert!(
            !seen
                .iter()
                .any(|c| c.starts_with("OpenRoom") || c.starts_with("MarkRead")),
            "{seen:?}"
        );

        // The user opens one: the room under the cursor.
        app.on_event(key(KeyCode::Enter));
        assert_eq!(open_id(&app), Some("!b"));
        assert_eq!(drain(&mut commands), ["FetchMembers(!b)", "OpenRoom(!b)"]);
    }

    #[test]
    fn nothing_is_read_while_no_chat_is_open() {
        let (mut app, mut terminal, mut commands) = connected();
        let many = |n: usize| -> Vec<Message> {
            (0..n)
                .map(|i| remote(&i.to_string(), "ann", &format!("message {i}")))
                .collect()
        };
        app.timeline.reached_start = true;
        drain(&mut commands);
        app.on_matrix(timeline(&app, many(5)));
        frame(&mut app, &mut terminal);
        frame(&mut app, &mut terminal);
        assert_eq!(marked_read(&mut commands), 1);

        // More arrives for the room after it was closed: updates that were on
        // their way, and one that claims to be current.
        let late = timeline(&app, many(6));
        app.on_event(key(KeyCode::Esc));
        assert!(app.chat_closed());
        assert_eq!(drain(&mut commands), ["CloseRoom"]);
        app.on_matrix(late);
        app.on_matrix(MatrixEvent::Timeline {
            room_id: "!a".into(),
            generation: app.generation,
            messages: many(7),
            reached_start: false,
            detached: None,
        });
        app.on_matrix(MatrixEvent::Paginated {
            generation: app.generation,
            reached_start: false,
        });
        assert_eq!(
            app.rooms[0].messages.len(),
            5,
            "nothing is put into the room"
        );
        // The server counts what is unread there, and the list shows it.
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!a", "Alpha", RoomCategory::Room, 2),
            summary("!b", "Beta", RoomCategory::Room, 0),
            summary("!i", "Invited", RoomCategory::Invite, 0),
        ]));
        for _ in 0..3 {
            frame(&mut app, &mut terminal);
        }
        assert_eq!(app.rooms[0].unread, 2);
        assert!(app.rooms[0].bold);
        // No receipt, no request for history, nothing at all.
        assert_eq!(drain(&mut commands), Vec::<String>::new());

        // Opened again and seen, it is read.
        app.on_event(key(KeyCode::Enter));
        assert_eq!(open_id(&app), Some("!a"));
        app.timeline.reached_start = true;
        app.on_matrix(timeline(&app, many(7)));
        frame(&mut app, &mut terminal);
        frame(&mut app, &mut terminal);
        assert!(marked_read(&mut commands) >= 1);
    }

    #[test]
    fn a_message_in_the_room_that_was_left_notifies_like_any_other() {
        use crate::matrix::Notice;
        use crate::notify::NotifyMode;
        let (mut app, _terminal, _commands) = connected();
        app.notify_mode = NotifyMode::Terminal;
        let notice = |room: &str| {
            MatrixEvent::Notice(Notice {
                room_id: room.into(),
                room: "Alpha".into(),
                sender: "ann".into(),
                body: "hello".into(),
                highlight: false,
                thread: None,
            })
        };
        // Open and on screen: the message is being watched.
        app.on_matrix(notice("!a"));
        assert!(app.pending_notices.is_empty());
        // Closed: nobody is looking at it.
        app.on_event(key(KeyCode::Esc));
        assert!(app.chat_closed());
        app.on_matrix(notice("!a"));
        let (notices, ..) = app.take_notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].room_id, "!a");

        // A count that rose there is news as well.
        app.detailed_notices.clear();
        app.shown_notices.clear();
        app.session_started = Instant::now().checked_sub(SESSION_SETTLE);
        let mut alpha = summary("!a", "Alpha", RoomCategory::Room, 2);
        alpha.server_notifications = 2;
        app.on_matrix(MatrixEvent::Rooms(vec![
            alpha,
            summary("!b", "Beta", RoomCategory::Room, 0),
            summary("!i", "Invited", RoomCategory::Invite, 0),
        ]));
        app.release_count_notices(Duration::ZERO);
        let (notices, ..) = app.take_notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].body, "2 new messages");
    }

    #[test]
    fn the_room_list_keeps_the_focus_while_no_chat_is_open() {
        let (mut app, mut terminal) = drawn();
        app.on_event(key(KeyCode::Esc));
        assert!(app.chat_closed());
        redraw(&mut app, &mut terminal);
        let cursor = app.sidebar.cursor;
        for event in [
            key(KeyCode::Tab),
            key(KeyCode::BackTab),
            key_mod(KeyCode::Left, KeyModifiers::ALT),
            key_mod(KeyCode::Right, KeyModifiers::ALT),
            key_mod(KeyCode::Up, KeyModifiers::ALT),
            key_mod(KeyCode::Down, KeyModifiers::ALT),
            alt('1'),
            alt('2'),
            alt('3'),
            key(KeyCode::PageUp),
            key(KeyCode::PageDown),
        ] {
            app.on_event(event.clone());
            assert_eq!(app.focus, Pane::Sidebar, "{event:?}");
            assert!(app.chat_closed(), "{event:?}");
            // Going nowhere does not move the list's cursor either.
            assert_eq!(app.sidebar.cursor, cursor, "{event:?}");
            assert_eq!(app.timeline.selected, None, "{event:?}");
        }
        // Moving about says nothing.
        assert!(app.toast.is_none());

        // A click where the chat was hits nothing.
        let (col, row) = find(&terminal, "No chat open");
        for (col, row) in [(col, row), (col + 20, row + 2), (col + 20, 27)] {
            app.on_event(click(col, row));
            app.on_event(mouse(MouseEventKind::ScrollUp, col, row));
            app.on_event(mouse(MouseEventKind::Down(MouseButton::Right), col, row));
            assert_eq!(app.focus, Pane::Sidebar);
            assert!(app.chat_closed() && app.modal.is_none());
        }
        assert_eq!(app.timeline.scroll, 0);

        // Letters go where they go in the room list, its filter; a paste too.
        // Nothing lands in a draft nobody sees.
        type_text(&mut app, "bo");
        assert_eq!(app.sidebar.filter.as_str(), "bo");
        app.on_event(Event::Paste("b".into()));
        assert_eq!(app.sidebar.filter.as_str(), "bob");
        assert!(app.composer.is_empty());
        assert!(app.attachments().is_empty());
        // Ctrl-C has no draft to clear.
        app.on_event(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.chat_closed());
        assert!(app.rooms.iter().all(|room| room.draft.is_empty()));
    }

    #[test]
    fn what_needs_an_open_chat_does_nothing_without_one() {
        // Every action that has a name, on the demo: none panics, and those about
        // the open chat leave everything as it is.
        for spec in keymap::ACTIONS {
            let (mut app, mut terminal) = drawn();
            app.on_event(key(KeyCode::Esc));
            assert!(app.chat_closed());
            app.perform(spec.action);
            redraw(&mut app, &mut terminal);
            app.after_draw();
            if spec.action.needs_open_chat() {
                assert!(app.chat_closed(), "{}", spec.name);
                assert!(app.modal.is_none(), "{}", spec.name);
                assert!(app.composer.is_empty(), "{}", spec.name);
                assert_eq!(app.focus, Pane::Sidebar, "{}", spec.name);
                assert_eq!(app.timeline.selected, None, "{}", spec.name);
                assert!(app.attachments().is_empty(), "{}", spec.name);
                assert!(app.editor_request.is_none(), "{}", spec.name);
                assert!(app.pending_copy.is_none(), "{}", spec.name);
            }
        }
        // With an account: none of them sends anything.
        for spec in keymap::ACTIONS
            .iter()
            .filter(|spec| spec.action.needs_open_chat())
        {
            let (mut app, mut terminal, mut commands) = closed();
            app.perform(spec.action);
            frame(&mut app, &mut terminal);
            assert_eq!(drain(&mut commands), Vec::<String>::new(), "{}", spec.name);
            assert!(app.chat_closed(), "{}", spec.name);
        }
        // A command says why nothing happened.
        let (mut app, _terminal, _commands) = closed();
        app.perform(Action::Search);
        assert_eq!(toast_text(&app), Some("No chat is open"));

        // The room-level ones are about the room under the cursor, as always in
        // the room list.
        let (mut app, _terminal, _commands) = closed();
        app.perform(Action::RoomInfo);
        match &app.modal {
            Some(Modal::RoomInfo(info)) => assert_eq!(info.room_id, "!a"),
            other => panic!("no room info: {other:?}"),
        }
        assert!(app.chat_closed());
    }

    #[test]
    fn the_palette_offers_nothing_about_a_chat_that_is_not_open() {
        use crate::palette::Target;
        let (mut app, _terminal) = drawn();
        app.on_event(key(KeyCode::Esc));
        app.perform(Action::OpenPalette);
        let Some(Modal::Palette(palette)) = &app.modal else {
            panic!("no palette");
        };
        let entries: Vec<_> = (0..).map_while(|at| palette.entry_at(at)).collect();
        for entry in &entries {
            if let Target::Action(action) = entry.target
                && action.needs_open_chat()
            {
                assert!(!entry.enabled, "{}", entry.title);
            }
        }
        let enabled = |action: Action| {
            entries
                .iter()
                .find(|entry| matches!(entry.target, Target::Action(a) if a == action))
                .unwrap_or_else(|| panic!("no entry for {action:?}"))
                .enabled
        };
        assert!(!enabled(Action::Attach));
        assert!(!enabled(Action::OpenEmoji));
        assert!(!enabled(Action::Search));
        assert!(!enabled(Action::Focus(Pane::Composer)));
        assert!(!enabled(Action::ToggleUnread));
        assert!(enabled(Action::SearchEverywhere));
        assert!(enabled(Action::FocusRoomFilter));
        assert!(enabled(Action::RoomInfo));
        assert!(enabled(Action::NewRoom));
        // No room is "the current one".
        for entry in &entries {
            if matches!(entry.target, Target::Room(_)) {
                assert_ne!(entry.hint.as_deref(), Some("current"), "{}", entry.title);
            }
        }
    }

    #[test]
    fn everything_that_opens_a_room_opens_one_again() {
        // The notification of another room, and of the room that was left.
        for room in ["!b", "!a"] {
            let (mut app, _terminal, mut commands) = closed();
            app.on_matrix(MatrixEvent::NoticeClicked {
                room_id: room.into(),
                event: None,
            });
            assert_eq!(open_id(&app), Some(room));
            assert_eq!(app.focus, Pane::Composer);
            let seen = drain(&mut commands);
            assert!(seen.contains(&format!("OpenRoom({room})")), "{seen:?}");
        }

        // A link to a room.
        let (mut app, _terminal, mut commands) = closed();
        app.rooms[1].id = "!b:example.org".into();
        assert!(app.try_open_matrix_link("https://matrix.to/#/%21b:example.org"));
        assert_eq!(open_id(&app), Some("!b:example.org"));
        assert!(drain(&mut commands).contains(&"OpenRoom(!b:example.org)".to_owned()));

        // The command palette's list of rooms.
        let (mut app, _terminal, mut commands) = closed();
        app.on_event(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL));
        type_text(&mut app, "Beta");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(open_id(&app), Some("!b"));
        assert_eq!(app.focus, Pane::Composer);
        assert!(drain(&mut commands).contains(&"OpenRoom(!b)".to_owned()));

        // The room list's filter, then Enter.
        let (mut app, _terminal, _commands) = closed();
        app.on_event(key_mod(KeyCode::Char('k'), KeyModifiers::CONTROL));
        type_text(&mut app, "bet");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(open_id(&app), Some("!b"));

        // Ctrl-↓ and Ctrl-↑ step from the list's cursor, which is on the room that
        // was left, and the room that opens is the one to write in.
        let (mut app, _terminal, mut commands) = closed();
        app.on_event(key_mod(KeyCode::Down, KeyModifiers::CONTROL));
        assert_eq!(open_id(&app), Some("!b"));
        assert_eq!(app.focus, Pane::Composer);
        assert_eq!(app.cursor_room(), app.open_index());
        assert!(drain(&mut commands).contains(&"OpenRoom(!b)".to_owned()));
        let (mut app, _terminal, _commands) = closed();
        app.on_event(key_mod(KeyCode::Up, KeyModifiers::CONTROL));
        assert_eq!(open_id(&app), Some("!i"));
        assert_eq!(app.cursor_room(), app.open_index());

        // A jump to a message (a search hit, a link to a message): in another room,
        // and in the room that was left, whose messages are not on screen.
        for room in ["!b", "!a"] {
            let (mut app, _terminal, mut commands) = closed();
            app.rooms[0].messages = vec![remote("1", "ann", "already loaded")];
            app.jump_to_event(room.into(), "$ev1".into(), None);
            let generation = app.next_gen;
            assert_eq!(
                drain(&mut commands),
                [format!("OpenEvent({room}, generation {generation})")]
            );
            // Until it arrives nothing changes.
            assert!(app.chat_closed());
            assert_eq!(app.timeline.selected, None);
            app.on_matrix(MatrixEvent::Timeline {
                room_id: room.into(),
                generation,
                messages: vec![remote("1", "ann", "the one")],
                reached_start: true,
                detached: None,
            });
            assert_eq!(open_id(&app), Some(room));
            assert_eq!(app.focus, Pane::Timeline);
            assert_eq!(app.timeline.selected, Some(0));
            assert_eq!(app.messages().len(), 1);
        }

        // An invitation: opened from the list, then accepted.
        let (mut app, _terminal, mut commands) = closed();
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(open_id(&app), Some("!i"));
        drain(&mut commands);
        app.perform(Action::AcceptInvite);
        assert_eq!(drain(&mut commands), ["Join(!i)"]);
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!a", "Alpha", RoomCategory::Room, 0),
            summary("!b", "Beta", RoomCategory::Room, 0),
            summary("!i", "Invited", RoomCategory::Room, 0),
        ]));
        assert_eq!(open_id(&app), Some("!i"));
        assert!(drain(&mut commands).contains(&"OpenRoom(!i)".to_owned()));

        // A room that was asked for (joined by a command, created, a chat begun)
        // opens when the list has it.
        let (mut app, _terminal, mut commands) = closed();
        app.manage.record_pending_open("!n".into());
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!a", "Alpha", RoomCategory::Room, 0),
            summary("!b", "Beta", RoomCategory::Room, 0),
            summary("!n", "New", RoomCategory::Room, 0),
        ]));
        assert_eq!(open_id(&app), Some("!n"));
        assert_eq!(app.focus, Pane::Composer);
        assert!(drain(&mut commands).contains(&"OpenRoom(!n)".to_owned()));

        // In the demo, a thread from the list of a room's threads.
        let (mut app, _terminal) = drawn();
        app.on_event(key(KeyCode::Esc));
        assert!(app.chat_closed());
        let left = app.active;
        app.jump_to_event(app.rooms[left].id.clone(), "demo:0".into(), None);
        assert_eq!(app.open_index(), Some(left));
        assert_eq!(app.timeline.selected, Some(0));
    }

    #[test]
    fn with_the_setting_off_escape_stops_at_the_composer() {
        use crate::modal::Setting;
        let config = Config {
            esc_closes_chat: false,
            ..Config::default()
        };
        assert!(!App::new(&config, std::path::PathBuf::new(), None).esc_closes_chat);
        assert!(App::new(&Config::default(), std::path::PathBuf::new(), None).esc_closes_chat);

        let (mut app, mut terminal, mut commands) = connected();
        let dir = std::env::temp_dir().join(format!("matrix-rat-esc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("config.toml");
        app.config_file = Some(file.clone());
        // The settings row (and the palette's entry) writes it to the configuration.
        app.perform(Action::Setting(Setting::EscClosesChat));
        assert!(!app.esc_closes_chat);
        assert!(!Config::load(&file).unwrap().esc_closes_chat);
        assert_eq!(app.setting_value(Setting::EscClosesChat), "off");

        drain(&mut commands);
        type_text(&mut app, "x");
        drain(&mut commands);
        for _ in 0..3 {
            app.on_event(key(KeyCode::Esc));
            assert!(!app.chat_closed());
            assert_eq!(app.focus, Pane::Composer);
            assert_eq!(open_id(&app), Some("!a"));
            assert_eq!(app.composer.text(), "x");
        }
        assert_eq!(drain(&mut commands), Vec::<String>::new());
        // The steps before it are as they are with it on.
        app.on_event(alt('1'));
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.focus, Pane::Composer);
        redraw(&mut app, &mut terminal);
        find(&terminal, "Alpha");

        // On again, the chat closes; switched off with no chat open, the room that
        // was left is back: there is no such state without the setting.
        app.perform(Action::Setting(Setting::EscClosesChat));
        assert!(Config::load(&file).unwrap().esc_closes_chat);
        app.on_event(key(KeyCode::Esc));
        assert!(app.chat_closed());
        app.perform(Action::Setting(Setting::EscClosesChat));
        assert!(!app.chat_closed());
        assert_eq!(open_id(&app), Some("!a"));
        assert_eq!(app.composer.text(), "x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn closing_the_chat_shows_a_room_list_that_was_put_away() {
        let (mut app, mut terminal) = drawn();
        app.perform(Action::ToggleSidebar);
        assert!(app.sidebar.hidden);
        redraw(&mut app, &mut terminal);
        assert!(!screen_text(&terminal).contains("1 Rooms"));

        app.on_event(key(KeyCode::Esc));
        assert!(app.chat_closed());
        assert_eq!(app.focus, Pane::Sidebar);
        redraw(&mut app, &mut terminal);
        let screen = screen_text(&terminal);
        // The list is the one pane there is; the setting is as the user left it.
        assert!(screen.contains("1 Rooms"), "{screen}");
        assert!(!screen.contains("Type a message"), "{screen}");
        assert!(app.sidebar.hidden);

        // Shown again by its key, the list keeps the focus, next to the empty chat
        // area; put away again, it stays: it is all there is to see.
        app.perform(Action::ToggleSidebar);
        redraw(&mut app, &mut terminal);
        let screen = screen_text(&terminal);
        assert!(screen.contains("1 Rooms") && screen.contains("No chat open"));
        app.perform(Action::ToggleSidebar);
        assert_eq!(app.focus, Pane::Sidebar);
        redraw(&mut app, &mut terminal);
        assert!(screen_text(&terminal).contains("1 Rooms"));

        // A room is opened: the list is put away again.
        app.on_event(key(KeyCode::Enter));
        assert!(!app.chat_closed());
        redraw(&mut app, &mut terminal);
        let screen = screen_text(&terminal);
        assert!(!screen.contains("1 Rooms"), "{screen}");
        assert!(screen.contains("Type a message"), "{screen}");

        // In a narrow window the list takes the chat's place in the same way.
        let (mut app, _) = drawn();
        let mut narrow = Terminal::new(TestBackend::new(50, 20)).unwrap();
        redraw(&mut app, &mut narrow);
        assert!(!screen_text(&narrow).contains("1 Rooms"));
        app.on_event(key(KeyCode::Esc));
        redraw(&mut app, &mut narrow);
        let screen = screen_text(&narrow);
        assert!(screen.contains("1 Rooms"), "{screen}");
        assert!(!screen.contains("No chat open"), "{screen}");
    }

    #[test]
    fn a_new_session_starts_with_a_room_open_again() {
        let (mut app, _terminal, mut commands) = closed();
        app.on_matrix(MatrixEvent::NeedLogin { reason: None });
        assert!(!app.chat_closed());
        app.on_matrix(MatrixEvent::LoggedIn {
            user_id: "@me:example.org".into(),
        });
        drain(&mut commands);
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!a", "Alpha", RoomCategory::Room, 0),
            summary("!b", "Beta", RoomCategory::Room, 0),
        ]));
        assert_eq!(open_id(&app), Some("!a"));
        assert!(drain(&mut commands).contains(&"OpenRoom(!a)".to_owned()));
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
        app.on_event(alt_up());
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
        let (col, row) = find(&terminal, "🙂");
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
            encrypted: false,
            id: id.into(),
            name: name.into(),
            category,
            notifications,
            preview: None,
            server_notifications: notifications,
            mention: false,
            unread: notifications > 0,
            marked_unread: false,
            avatar: None,
            alias: None,
            direct: category == RoomCategory::Person,
            muted: false,
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
    fn reauth_flow_keeps_identity_fixed_and_submits_reauth() {
        use crate::matrix::{MatrixEvent, ReauthKind, ReauthTarget};
        let (mut app, _terminal, mut commands) = connected();
        while commands.try_recv().is_ok() {}

        let pw_target = ReauthTarget {
            homeserver: "https://example.org".into(),
            user_id: "@alice:example.org".into(),
            device_id: "DEV1".into(),
            kind: ReauthKind::Password,
            client_id: None,
        };
        app.on_matrix(MatrixEvent::NeedReauth {
            target: pw_target,
            reason: "Your session expired.".into(),
        });
        assert!(matches!(app.modal, Some(Modal::Login(_))));

        // Typing goes straight to password; fixed homeserver & account stay untouched even on Tab.
        type_text(&mut app, "secretpw");
        app.on_event(key(KeyCode::Tab));
        {
            let Some(Modal::Login(form)) = &app.modal else {
                panic!("expected Login modal");
            };
            assert!(form.is_reauth());
            assert_eq!(form.homeserver, "https://example.org");
            assert_eq!(form.username, "@alice:example.org");
            assert_eq!(form.password, "secretpw");
            assert_eq!(form.focus, crate::modal::LoginForm::PASSWORD_BUTTON);
        }
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            commands.try_recv().unwrap(),
            MatrixCmd::ReauthPassword { password } if password.as_str() == "secretpw"
        ));

        // Failure keeps re-authenticate mode open so the user can retry.
        app.on_matrix(MatrixEvent::LoginFailed("wrong password".into()));
        let Some(Modal::Login(form)) = &app.modal else {
            panic!("expected Login modal");
        };
        assert!(form.is_reauth());
        assert_eq!(form.error.as_deref(), Some("wrong password"));

        // Browser re-auth immediately focuses the browser button and dispatches ReauthBrowser.
        app.on_matrix(MatrixEvent::NeedReauth {
            target: ReauthTarget {
                homeserver: "https://sso.example.org".into(),
                user_id: "@alice:sso.example.org".into(),
                device_id: "DEV2".into(),
                kind: ReauthKind::Browser,
                client_id: Some("cid-1".into()),
            },
            reason: "Your session expired.".into(),
        });
        type_text(&mut app, "ignored");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            commands.try_recv().unwrap(),
            MatrixCmd::ReauthBrowser
        ));
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
            seen.push(command.describe());
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
            detached: None,
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
            detached: None,
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
        app.on_event(alt_up());
        app.on_event(key(KeyCode::Up));
        assert_eq!(app.timeline.selected, Some(0));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.compose_mode, ComposeMode::Reply(0));
        app.on_event(key(KeyCode::Up));
        assert_eq!(app.timeline.selected, Some(1));

        // Older history arrives in front, with a system line mixed in, and the SDK's
        // read marker, which is not shown: nothing was unread when the room was
        // opened (see `unread` for the line that is shown when something was).
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
        assert_eq!(app.timeline.selected, Some(3));
        assert_eq!(app.compose_mode, ComposeMode::Reply(2));
        // Moving up skips the system line.
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

        // (EditLast edits our latest message.)
        app.perform(Action::EditLast);
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
        app.on_event(alt_up());
        app.on_event(key(KeyCode::Char('r')));
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Delete));
        // The reaction picker asks for the image packs, for their custom emoji.
        assert_eq!(
            drain(&mut commands),
            [
                "EnsureEmotePacks(has_room: true)",
                "ToggleReaction(!a)",
                "Redact(!a)"
            ]
        );
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
            seen[seen.len() - 3..],
            ["Send(!b)", "SaveDraft(!b, 0 bytes)", "Typing(!b, false)"],
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
            detached: None,
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

        // Slot rows start hidden behind "Follow dark/light", so Down skips straight to width.
        find(&terminal, "Follow dark/light");
        assert!(!format!("{}", terminal.backend()).contains("Dark theme"));
        app.on_event(key(KeyCode::Down));
        assert_eq!(app.setting_value(Setting::ThemePairing), "off");
        app.on_event(key(KeyCode::Down));
        assert_eq!(
            app.sidebar.width,
            app.setting_value(Setting::SidebarWidth)
                .parse::<u16>()
                .unwrap()
        );
        app.on_event(key(KeyCode::Up));

        // Enabling pairing reveals Dark theme and Light theme slots.
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.setting_value(Setting::ThemePairing), "on");
        redraw(&mut app, &mut terminal);
        find(&terminal, "Dark theme");
        find(&terminal, "Light theme");

        // Dark theme and Light theme slots cycle through (unset) + themes and clean
        // up config keys when returned to (unset) or when Follow dark/light is turned off.
        app.on_event(key(KeyCode::Down));
        assert_eq!(app.setting_value(Setting::ThemeDark), "(unset)");
        app.on_event(key(KeyCode::Right));
        assert_ne!(app.setting_value(Setting::ThemeDark), "(unset)");
        assert!(Config::load(&file).unwrap().theme_dark.is_some());
        app.on_event(key(KeyCode::Left));
        assert_eq!(app.setting_value(Setting::ThemeDark), "(unset)");
        assert!(Config::load(&file).unwrap().theme_dark.is_none());
        app.on_event(key(KeyCode::Right));
        assert!(Config::load(&file).unwrap().theme_dark.is_some());

        app.on_event(key(KeyCode::Down));
        assert_eq!(app.setting_value(Setting::ThemeLight), "(unset)");
        app.on_event(key(KeyCode::Right));
        assert_ne!(app.setting_value(Setting::ThemeLight), "(unset)");
        assert!(Config::load(&file).unwrap().theme_light.is_some());
        app.on_event(key(KeyCode::Left));
        assert_eq!(app.setting_value(Setting::ThemeLight), "(unset)");
        assert!(Config::load(&file).unwrap().theme_light.is_none());

        // Turning Follow dark/light off removes configured slot keys, restores base Theme,
        // and hides the two slot rows again.
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Char(' ')));
        assert_eq!(app.setting_value(Setting::ThemePairing), "off");
        assert_eq!(app.theme.name, before);
        assert!(Config::load(&file).unwrap().theme_dark.is_none());
        redraw(&mut app, &mut terminal);
        assert!(!format!("{}", terminal.backend()).contains("Dark theme"));

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
        // The rooms bar: off, and on again.
        app.on_event(key(KeyCode::Down));
        assert!(app.rooms_bar);
        app.on_event(key(KeyCode::Enter));
        assert!(!app.rooms_bar);
        assert!(!Config::load(&file).unwrap().rooms_bar);
        app.on_event(key(KeyCode::Enter));
        assert!(app.rooms_bar);
        // Link previews: off, unencrypted rooms, all rooms, round; arrows go both ways.
        app.on_event(key(KeyCode::Down));
        assert_eq!(app.link_previews, crate::config::LinkPreviews::Off);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.link_previews, crate::config::LinkPreviews::Unencrypted);
        app.on_event(key(KeyCode::Right));
        assert_eq!(app.link_previews, crate::config::LinkPreviews::All);
        app.on_event(key(KeyCode::Right));
        assert_eq!(app.link_previews, crate::config::LinkPreviews::Off);
        app.on_event(key(KeyCode::Left));
        assert_eq!(app.link_previews, crate::config::LinkPreviews::All);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.link_previews, crate::config::LinkPreviews::Off);
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

        // Logging out asks twice; the demo has no account, so it only says so. The
        // last rows are below the edge of a window this low: the menu scrolls to them.
        while !matches!(&app.modal, Some(Modal::Settings(menu)) if menu.current() == Setting::Logout)
        {
            app.on_event(key(KeyCode::Up));
        }
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "Log out");
        app.on_event(click(col, row));
        assert!(matches!(app.modal, Some(Modal::Settings(_))));
        assert!(app.toast.is_some());

        // "Key bindings" opens the key-bindings editor; the backdrop closes popups.
        // (Up from "Log out": ignored users, key backup, sessions, encryption, profile,
        // what the app found, keys.)
        for _ in 0..7 {
            app.on_event(key(KeyCode::Up));
        }
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
    fn do_not_disturb_holds_every_notification_back_and_says_that_it_is_on() {
        use crate::matrix::Notice;
        use crate::notify::NotifyMode;
        let (mut app, mut terminal, _commands) = connected();
        let mut screen = |app: &mut App| {
            terminal.draw(|frame| ui::draw(frame, app)).unwrap();
            format!("{}", terminal.backend())
        };
        app.notify_mode = NotifyMode::Terminal;
        let notice = |highlight| {
            MatrixEvent::Notice(Notice {
                room_id: "!b".into(),
                room: "Beta".into(),
                sender: "ann".into(),
                body: "hello".into(),
                highlight,
                thread: None,
            })
        };
        // One is waiting when the switch is turned on: it is not shown later either.
        app.on_matrix(notice(false));
        assert_eq!(app.pending_notices.len(), 1);
        app.perform(Action::Setting(Setting::DoNotDisturb));
        assert!(app.do_not_disturb);
        assert!(app.pending_notices.is_empty());
        app.shown_notices.clear();
        app.on_matrix(notice(false));
        app.on_matrix(notice(true));
        assert!(app.pending_notices.is_empty());
        app.toast = None;
        assert!(screen(&mut app).contains("do not disturb"));

        // Off again: notifications come back, the mark goes.
        app.perform(Action::Setting(Setting::DoNotDisturb));
        app.shown_notices.clear();
        app.on_matrix(notice(false));
        assert_eq!(app.pending_notices.len(), 1);
        app.toast = None;
        assert!(!screen(&mut app).contains("do not disturb"));
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
            ..Default::default()
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
        app.rooms.retain(|room| !room.is_pending());
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

        app.on_event(alt_up());
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

    #[test]
    fn login_and_reauth_inputs_edit_mid_text_and_place_cursor_by_click() {
        let (mut app, mut terminal, mut commands) = connected();
        let ctrl = |c: char| key_mod(KeyCode::Char(c), KeyModifiers::CONTROL);
        let ctrl_key = |code: KeyCode| key_mod(code, KeyModifiers::CONTROL);

        app.open_modal(Modal::Login(LoginForm::new(None)));
        // Clear default "matrix.org", type multi-word text + grapheme, move & edit mid-string.
        app.on_event(ctrl('u'));
        type_text(&mut app, "one two👍🏽 three");
        app.on_event(ctrl_key(KeyCode::Left)); // before "three"
        app.on_event(key(KeyCode::Left)); // after 👍🏽
        app.on_event(key(KeyCode::Backspace)); // deletes whole 👍🏽
        app.on_event(ctrl('k')); // kills " three"
        app.on_event(key(KeyCode::Home));
        app.on_event(key(KeyCode::Delete)); // deletes 'o'
        app.on_event(key(KeyCode::Char('O')));
        app.on_event(alt('f')); // after "One"
        app.on_event(key(KeyCode::Char('!')));
        match &app.modal {
            Some(Modal::Login(form)) => assert_eq!(form.homeserver, "One! two"),
            other => panic!("unexpected modal: {other:?}"),
        }

        // Render and click inside homeserver value rect and password value rect.
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "One! two");
        app.on_event(click(col + 4, row));
        app.on_event(key(KeyCode::Char('?')));
        match &app.modal {
            Some(Modal::Login(form)) => assert_eq!(form.homeserver, "One!? two"),
            other => panic!("unexpected modal: {other:?}"),
        }

        // Click "Password" label (HitId::Field(2)) then type password with skin-tone emoji.
        let (pw_label_col, pw_row) = find(&terminal, "Password");
        app.on_event(click(pw_label_col, pw_row));
        type_text(&mut app, "pa👍🏽ss");
        redraw(&mut app, &mut terminal);
        // Click third bullet (column 2 -> right before 👍🏽 in 1-col-per-grapheme masking).
        let (dots_col, dots_row) = find(&terminal, "•••••");
        app.on_event(click(dots_col + 2, dots_row));
        app.on_event(key(KeyCode::Delete));
        match &app.modal {
            Some(Modal::Login(form)) => {
                assert_eq!(form.password, "pass");
                assert!(!format!("{form:?}").contains("pass"));
            }
            other => panic!("unexpected modal: {other:?}"),
        }

        // Reauth keeps fixed identity, edits password, and take_string() preserves secret redaction.
        app.on_event(key(KeyCode::Esc));
        app.open_modal(Modal::Login(LoginForm::reauth(
            crate::matrix::ReauthTarget {
                homeserver: "https://matrix.example.org".into(),
                user_id: "@me:example.org".into(),
                device_id: "DEV".into(),
                kind: crate::matrix::ReauthKind::Password,
                client_id: None,
            },
            "Session expired".into(),
        )));
        type_text(&mut app, "alpha beta");
        app.on_event(ctrl('w')); // removes "beta"
        app.on_event(key(KeyCode::Char('X')));
        app.on_event(key(KeyCode::Enter));
        let sent: Vec<_> = std::iter::from_fn(|| commands.try_recv().ok()).collect();
        assert!(sent.iter().any(|c| matches!(
            c,
            MatrixCmd::ReauthPassword { password } if password.as_str() == "alpha X"
        )));
        match &mut app.modal {
            Some(Modal::Login(form)) => {
                assert_eq!(form.password, "");
                form.busy = None;
            }
            other => panic!("unexpected modal: {other:?}"),
        }
        type_text(&mut app, "newsecret");
        match &app.modal {
            Some(Modal::Login(form)) => {
                assert_eq!(form.password, "newsecret");
                assert!(!format!("{form:?}").contains("newsecret"));
            }
            other => panic!("unexpected modal: {other:?}"),
        }
    }

    #[test]
    fn recovery_settings_download_dir_and_emoji_inputs_edit_and_click_to_place() {
        let (mut app, mut terminal, mut commands) = connected();
        let ctrl = |c: char| key_mod(KeyCode::Char(c), KeyModifiers::CONTROL);
        let ctrl_key = |code: KeyCode| key_mod(code, KeyModifiers::CONTROL);

        // 1. RecoveryForm: mid-text edits, click-to-place, and secret kept after take_string().
        app.on_matrix(MatrixEvent::Recovery(
            crate::matrix::RecoveryStatus::NeedsKey,
        ));
        type_text(&mut app, "EsTj abcd");
        app.on_event(ctrl_key(KeyCode::Left));
        type_text(&mut app, "MID ");
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "EsTj MID abcd");
        app.on_event(click(col + 6, row)); // right on 'I' in "MID"
        app.on_event(key(KeyCode::Delete)); // removes 'I' -> "MD"
        app.on_event(ctrl('e'));
        app.on_event(ctrl('w')); // removes "abcd"
        match &app.modal {
            Some(Modal::Recovery(form)) => assert_eq!(form.key.as_str(), "EsTj MD "),
            other => panic!("unexpected modal: {other:?}"),
        }
        app.on_event(key(KeyCode::Enter));
        let _ = drain(&mut commands);
        match &mut app.modal {
            Some(Modal::Recovery(form)) => {
                assert_eq!(form.key, "");
                form.busy = false;
            }
            other => panic!("unexpected modal: {other:?}"),
        }
        type_text(&mut app, "retrykey");
        match &app.modal {
            Some(Modal::Recovery(form)) => {
                assert_eq!(form.key, "retrykey");
                assert!(!format!("{form:?}").contains("retrykey"));
            }
            other => panic!("unexpected modal: {other:?}"),
        }
        app.on_event(key(KeyCode::Esc));

        // 2. SettingsMenu::DownloadDir: mid-text editing & click-to-place.
        app.perform(Action::OpenSettings);
        let row_idx = match &app.modal {
            Some(Modal::Settings(m)) => crate::modal::Setting::ALL
                .iter()
                .filter(|s| m.shows(**s))
                .position(|s| *s == crate::modal::Setting::DownloadDir)
                .unwrap(),
            other => panic!("unexpected modal: {other:?}"),
        };
        for _ in 0..row_idx {
            app.on_event(key(KeyCode::Down));
        }
        app.on_event(key(KeyCode::Enter));
        app.on_event(ctrl('u'));
        type_text(&mut app, "/tmp/inbox old");
        app.on_event(ctrl('w')); // -> "/tmp/inbox "
        app.on_event(alt('b'));
        app.on_event(key(KeyCode::Char('2'))); // "/tmp/2inbox "
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "/tmp/2inbox");
        app.on_event(click(col + 6, row)); // right after '2'
        app.on_event(ctrl('k')); // -> "/tmp/2"
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.download_dir, "/tmp/2");
        app.on_event(key(KeyCode::Esc));

        // 3. EmojiPicker: plain Left/Right step grid; word motion & shortcuts edit query; click places.
        app.on_event(alt('e'));
        type_text(&mut app, "thumbsup extra");
        app.on_event(ctrl('k')); // no-op at end
        app.on_event(ctrl_key(KeyCode::Left)); // before "extra"
        app.on_event(ctrl('k')); // deletes "extra" -> "thumbsup "
        app.on_event(key(KeyCode::Backspace)); // "thumbsup"
        let before_sel = match &app.modal {
            Some(Modal::Emoji(p)) => {
                assert_eq!(p.query, "thumbsup");
                p.selected
            }
            other => panic!("unexpected modal: {other:?}"),
        };
        // Wait: "thumbsup" might only have 1-2 results, use empty or broader query to verify Right steps grid.
        app.on_event(ctrl('u'));
        type_text(&mut app, "smile cat");
        redraw(&mut app, &mut terminal);
        let sel0 = match &app.modal {
            Some(Modal::Emoji(p)) => p.selected,
            _ => unreachable!(),
        };
        app.on_event(ctrl('u'));
        type_text(&mut app, "heart");
        redraw(&mut app, &mut terminal);
        app.on_event(key(KeyCode::Right));
        let sel1 = match &app.modal {
            Some(Modal::Emoji(p)) => p.selected,
            _ => unreachable!(),
        };
        assert_eq!(sel1, sel0 + 1);
        app.on_event(key(KeyCode::Left));
        // Now use Alt-B / Delete / click-to-place on "heart".
        let (col, row) = find(&terminal, "heart");
        app.on_event(click(col + 2, row)); // he|art
        app.on_event(key(KeyCode::Delete)); // he|rt
        match &app.modal {
            Some(Modal::Emoji(p)) => assert_eq!(p.query, "hert"),
            other => panic!("unexpected modal: {other:?}"),
        }
        let _ = before_sel;
    }

    #[test]
    fn sidebar_filter_edits_mid_text_handles_graphemes_and_clicks_to_place() {
        let (mut app, mut terminal, _commands) = connected();
        let ctrl = |c: char| key_mod(KeyCode::Char(c), KeyModifiers::CONTROL);
        let ctrl_key = |code: KeyCode| key_mod(code, KeyModifiers::CONTROL);

        app.set_focus(Pane::Sidebar);
        type_text(&mut app, "cafe");
        app.on_event(key(KeyCode::Char('\u{0301}'))); // combining acute -> "cafe\u{0301}"
        type_text(&mut app, " 👍🏽 alp");
        app.on_event(ctrl_key(KeyCode::Left));
        app.on_event(ctrl('a'));
        app.on_event(key(KeyCode::Delete)); // drops 'c'
        app.on_event(key(KeyCode::Char('C')));
        app.on_event(ctrl('e'));
        app.on_event(ctrl('w')); // drops "alp"
        assert_eq!(app.sidebar.filter, "Cafe\u{0301} 👍🏽 ");
        // One Backspace after space removes ' ', next single Backspace removes whole 👍🏽.
        app.on_event(key(KeyCode::Backspace));
        app.on_event(key(KeyCode::Backspace));
        assert_eq!(app.sidebar.filter, "Cafe\u{0301} ");

        // Redraw and click at column 2 of the filter text (between 'a' and 'f').
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, " / Cafe\u{0301}");
        app.on_event(click(col + 3 + 2, row));
        app.on_event(key(KeyCode::Char('X')));
        assert_eq!(app.sidebar.filter, "CaXfe\u{0301} ");
    }

    /// Connected to a fake worker: `!a` Alpha (open, with two messages), `!b` Beta
    /// and an invitation.
    fn with_worker() -> (App, tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>) {
        let (handle, mut commands) = MatrixHandle::for_test();
        let config = crate::config::Config::default();
        let mut app = App::new(&config, std::path::PathBuf::new(), Some(handle));
        app.on_matrix(MatrixEvent::LoggedIn {
            user_id: "@me:example.org".into(),
        });
        app.on_matrix(MatrixEvent::CaughtUp);
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!a", "Alpha", RoomCategory::Room, 0),
            summary("!b", "Beta", RoomCategory::Room, 0),
            summary("!i", "Invited", RoomCategory::Invite, 0),
        ]));
        let message = |n: usize| Message {
            id: n.to_string(),
            item: Some(ItemRef::Event(format!("$e{n}"))),
            sender: "ann".into(),
            body: format!("line {n}"),
            ..Message::default()
        };
        app.on_matrix(MatrixEvent::Timeline {
            room_id: "!a".into(),
            generation: app.generation,
            messages: vec![message(0), message(1)],
            reached_start: true,
            detached: None,
        });
        while commands.try_recv().is_ok() {}
        (app, commands)
    }

    fn commands_sent(
        commands: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>,
    ) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            seen.push(command.describe());
        }
        seen
    }

    fn clicked(room_id: &str) -> MatrixEvent {
        MatrixEvent::NoticeClicked {
            room_id: room_id.into(),
            event: None,
        }
    }

    #[test]
    fn a_clicked_notification_opens_its_room_like_the_room_list_does() {
        let (mut app, mut commands) = with_worker();
        assert_eq!(clicked("!b").describe(), "NoticeClicked(!b)");
        // Something half written, and a message selected, in the open room.
        type_text(&mut app, "half a thought");
        app.on_matrix(clicked("!b"));
        assert_eq!(app.active, 1);
        assert_eq!(app.focus, Pane::Composer);
        // The draft stays with its room, in the store as well.
        assert_eq!(app.composer.text(), "");
        assert_eq!(app.rooms[0].draft, "half a thought");
        assert_eq!(
            commands_sent(&mut commands),
            [
                "Typing(!a, true)",
                "Typing(!a, false)",
                "SaveDraft(!a, 14 bytes)",
                "FetchMembers(!b)",
                "OpenRoom(!b)"
            ]
        );

        // What was only being looked at makes way: the palette, a menu, the help.
        app.perform(Action::OpenPalette);
        assert!(app.modal.is_some());
        app.on_matrix(clicked("!a"));
        assert!(app.modal.is_none());
        assert_eq!(app.active, 0);
        assert_eq!(app.composer.text(), "half a thought");
        commands_sent(&mut commands);
        app.perform(Action::ToggleHelp);
        app.on_matrix(clicked("!b"));
        assert!(!app.help_open);
        assert_eq!(app.active, 1);
        // A search across rooms is ended properly.
        app.perform(Action::SearchEverywhere);
        commands_sent(&mut commands);
        app.on_matrix(clicked("!a"));
        assert!(app.modal.is_none());
        assert!(commands_sent(&mut commands).contains(&"CancelSearch".to_owned()));

        // A room that a space hides is shown in the list as well.
        app.on_matrix(MatrixEvent::Spaces(vec![crate::matrix::SpaceSummary {
            id: "!s".into(),
            name: "Space".into(),
            level: 0,
            rooms: vec!["!a".into()],
            ..Default::default()
        }]));
        app.select_space(Some(0));
        assert!(app.space.is_some());
        app.on_matrix(clicked("!b"));
        assert_eq!(app.active, 1);
        assert!(app.space.is_none());
        assert_eq!(app.cursor_room(), Some(1));
    }

    #[test]
    fn a_click_for_the_open_room_or_a_room_that_is_gone_changes_nothing() {
        let (mut app, mut commands) = with_worker();
        // The open room: the place in the messages, a selection and a thread stay.
        app.perform(Action::Focus(Pane::Timeline));
        app.timeline.scroll = 3;
        let selected = app.timeline.selected;
        assert!(selected.is_some());
        let generation = app.generation;
        app.on_matrix(clicked("!a"));
        assert_eq!(
            (app.active, app.focus, app.timeline.scroll),
            (0, Pane::Timeline, 3)
        );
        assert_eq!(app.timeline.selected, selected);
        assert_eq!(app.generation, generation);
        app.perform(Action::OpenThread);
        assert!(app.thread_open());
        commands_sent(&mut commands);
        app.on_matrix(clicked("!a"));
        assert!(app.thread_open());
        assert_eq!(commands_sent(&mut commands), Vec::<String>::new());

        // A room that has left the list since (or never was in it): nothing, and a
        // popup that is open stays.
        app.perform(Action::OpenPalette);
        app.on_matrix(clicked("!gone"));
        assert!(matches!(app.modal, Some(Modal::Palette(_))));
        assert_eq!(app.active, 0);
        assert!(app.thread_open());
        assert_eq!(commands_sent(&mut commands), Vec::<String>::new());
    }

    #[test]
    fn a_click_ends_a_jump_that_is_under_way_and_a_view_of_older_messages() {
        let slice = |generation: u64, room: &str| MatrixEvent::Timeline {
            room_id: room.into(),
            generation,
            messages: vec![Message {
                id: "old".into(),
                item: Some(ItemRef::Event("$old".into())),
                sender: "ann".into(),
                body: "long ago".into(),
                ..Message::default()
            }],
            reached_start: false,
            detached: Some(crate::matrix::DetachedTimeline {
                target_event: "$old".into(),
                thread_root: None,
                reached_end: false,
            }),
        };
        // The generation of the jump that was asked for last.
        let asked = |commands: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>| {
            let mut found = None;
            while let Ok(command) = commands.try_recv() {
                if let MatrixCmd::OpenEvent { generation, .. } = command {
                    found = Some(generation);
                }
            }
            found.expect("no jump was asked for")
        };

        // A jump into the other room is on its way; the notification of the open
        // room is clicked. The jump is called off, and its late answer changes
        // nothing.
        let (mut app, mut commands) = with_worker();
        app.jump_to_event("!b".into(), "$old".into(), None);
        let jump = asked(&mut commands);
        app.on_matrix(clicked("!a"));
        assert_eq!(
            commands_sent(&mut commands),
            [format!("CancelOpenEvent(generation {jump})")]
        );
        app.on_matrix(slice(jump, "!b"));
        assert_eq!(app.active, 0);
        assert!(!app.timeline.detached);

        // The same with a click for another room: that room is opened, as from the
        // room list, and shows its newest messages.
        app.jump_to_event("!b".into(), "$old".into(), None);
        let jump = asked(&mut commands);
        app.on_matrix(clicked("!b"));
        assert_eq!(
            commands_sent(&mut commands),
            [
                format!("CancelOpenEvent(generation {jump})"),
                "FetchMembers(!b)".to_owned(),
                "OpenRoom(!b)".to_owned()
            ]
        );
        app.on_matrix(slice(jump, "!b"));
        assert!(!app.timeline.detached);
        assert!(app.messages().is_empty());

        // The view shows an older part of a room (a jump arrived). A click for
        // another room opens that one as usual.
        app.jump_to_event("!a".into(), "$old".into(), None);
        let jump = asked(&mut commands);
        app.on_matrix(slice(jump, "!a"));
        assert!(app.timeline.detached);
        assert_eq!(app.active, 0);
        commands_sent(&mut commands);
        app.on_matrix(clicked("!b"));
        assert_eq!(app.active, 1);
        assert!(!app.timeline.detached);
        assert_eq!(
            commands_sent(&mut commands),
            ["FetchMembers(!b)", "OpenRoom(!b)"]
        );
        // A click for the room itself goes back to its newest messages.
        app.jump_to_event("!b".into(), "$old".into(), None);
        let jump = asked(&mut commands);
        app.on_matrix(slice(jump, "!b"));
        assert!(app.timeline.detached);
        commands_sent(&mut commands);
        app.on_matrix(clicked("!b"));
        assert!(!app.timeline.detached);
        assert_eq!(commands_sent(&mut commands), ["OpenRoom(!b)"]);
    }

    #[test]
    fn a_click_is_ignored_while_a_question_has_to_be_answered() {
        // The recovery key is asked for.
        let (mut app, mut commands) = with_worker();
        app.on_matrix(MatrixEvent::Recovery(RecoveryStatus::NeedsKey));
        assert!(matches!(app.modal, Some(Modal::Recovery(_))));
        app.on_matrix(clicked("!b"));
        assert!(matches!(app.modal, Some(Modal::Recovery(_))));
        assert_eq!(app.active, 0);
        // A device verification is under way.
        app.modal = Some(Modal::Verify(crate::verify::VerifyPopup::flow()));
        app.on_matrix(clicked("!b"));
        assert!(matches!(app.modal, Some(Modal::Verify(_))));
        assert_eq!(app.active, 0);
        assert_eq!(commands_sent(&mut commands), Vec::<String>::new());

        // The session ended and the login form is up; the rooms of the old session
        // are not gone into, and neither are those of the next one before it is in.
        let (mut app, mut commands) = with_worker();
        app.on_matrix(MatrixEvent::NeedLogin { reason: None });
        assert!(matches!(app.modal, Some(Modal::Login(_))));
        app.on_matrix(clicked("!b"));
        assert!(matches!(app.modal, Some(Modal::Login(_))));
        app.modal = None;
        app.on_matrix(clicked("!b"));
        assert_eq!(commands_sent(&mut commands), Vec::<String>::new());
    }

    fn take_opened_urls() -> Vec<String> {
        TEST_OPENED_URLS.with(|urls| std::mem::take(&mut *urls.borrow_mut()))
    }

    fn screen_text(terminal: &Terminal<TestBackend>) -> String {
        format!("{}", terminal.backend())
    }

    fn toast_text(app: &App) -> Option<&str> {
        app.toast.as_ref().map(|(text, _)| text.as_str())
    }

    fn detached_timeline(
        room_id: &str,
        generation: u64,
        target_event: &str,
        thread_root: Option<&str>,
        reached_end: bool,
        messages: Vec<Message>,
    ) -> MatrixEvent {
        MatrixEvent::Timeline {
            room_id: room_id.to_owned(),
            generation,
            messages,
            reached_start: false,
            detached: Some(crate::matrix::DetachedTimeline {
                target_event: target_event.to_owned(),
                thread_root: thread_root.map(str::to_owned),
                reached_end,
            }),
        }
    }

    #[test]
    fn unloaded_detached_jump_banner_and_return_to_live_paths() {
        let (mut app, mut terminal, mut commands) = connected();
        app.rooms[0].messages = (40..50)
            .map(|i| remote(&i.to_string(), "ann", &format!("live {i}")))
            .collect();
        drain(&mut commands);

        // Jump to an unloaded message in the active room.
        app.jump_to_event("!a".into(), "$ev5".into(), None);
        let generation = app.next_gen;
        assert_eq!(
            drain(&mut commands),
            [format!("OpenEvent(!a, generation {generation})")]
        );

        let slice = (0..10)
            .map(|i| remote(&i.to_string(), "bob", &format!("old {i}")))
            .collect();
        app.on_matrix(detached_timeline(
            "!a", generation, "$ev5", None, false, slice,
        ));
        assert!(app.timeline.detached);
        assert!(!app.timeline.reached_end);
        assert_eq!(app.timeline.selected, Some(5));
        assert_eq!(app.focus, Pane::Timeline);
        redraw(&mut app, &mut terminal);
        let text = screen_text(&terminal);
        assert!(text.contains("viewing older messages"), "{text}");

        // End (JumpToNewest) clears detached mode and asks for the live timeline.
        app.on_event(key(KeyCode::End));
        assert!(!app.timeline.detached);
        let cmds = drain(&mut commands);
        assert!(cmds.iter().any(|c| c == "OpenRoom(!a)"), "{cmds:?}");
        app.on_matrix(timeline(&app, vec![remote("50", "ann", "live 50")]));
        assert!(!app.timeline.detached);
        redraw(&mut app, &mut terminal);
        assert!(!screen_text(&terminal).contains("viewing older messages"));

        // Clicking the detached banner returns to live too.
        app.jump_to_event("!a".into(), "$ev5".into(), None);
        let generation = app.next_gen;
        drain(&mut commands);
        let slice = (0..10)
            .map(|i| remote(&i.to_string(), "bob", &format!("old {i}")))
            .collect();
        app.on_matrix(detached_timeline(
            "!a", generation, "$ev5", None, false, slice,
        ));
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "viewing older messages");
        app.on_event(click(col + 1, row));
        assert!(!app.timeline.detached);
        let cmds = drain(&mut commands);
        assert!(cmds.iter().any(|c| c == "OpenRoom(!a)"), "{cmds:?}");

        // Sending a text message while detached returns to the live timeline.
        app.jump_to_event("!a".into(), "$ev5".into(), None);
        let generation = app.next_gen;
        drain(&mut commands);
        let slice = (0..10)
            .map(|i| remote(&i.to_string(), "bob", &format!("old {i}")))
            .collect();
        app.on_matrix(detached_timeline(
            "!a", generation, "$ev5", None, false, slice,
        ));
        app.set_focus(Pane::Composer);
        type_text(&mut app, "back to present");
        app.on_event(key(KeyCode::Enter));
        assert!(!app.timeline.detached);
        let cmds = drain(&mut commands);
        assert!(cmds.iter().any(|c| c == "OpenRoom(!a)"), "{cmds:?}");
        assert!(cmds.iter().any(|c| c == "Send(!a)"), "{cmds:?}");

        // Sending attachments while detached returns to live too.
        let note =
            std::env::temp_dir().join(format!("matrix-rat-jump-note-{}.txt", std::process::id()));
        std::fs::write(&note, b"hello attachment").unwrap();
        app.jump_to_event("!a".into(), "$ev5".into(), None);
        let generation = app.next_gen;
        drain(&mut commands);
        let slice = (0..10)
            .map(|i| remote(&i.to_string(), "bob", &format!("old {i}")))
            .collect();
        app.on_matrix(detached_timeline(
            "!a", generation, "$ev5", None, false, slice,
        ));
        app.attach.pending.insert(
            ("!a".to_owned(), None),
            vec![crate::upload::Chosen {
                path: note.clone(),
                name: "note.txt".to_owned(),
                size: 16,
            }],
        );
        redraw(&mut app, &mut terminal);
        app.set_focus(Pane::Composer);
        app.on_event(key(KeyCode::Enter));
        let _ = std::fs::remove_file(&note);
        assert!(!app.timeline.detached);
        let cmds = drain(&mut commands);
        assert!(cmds.iter().any(|c| c == "OpenRoom(!a)"), "{cmds:?}");
        assert!(
            cmds.iter().any(|c| c == "SendFiles(!a, 1 files)"),
            "{cmds:?}"
        );

        // Returning to live inside a detached thread sends OpenThread instead of OpenRoom.
        app.jump_to_event("!a".into(), "$ev2".into(), Some("$root".into()));
        let generation = app.next_gen;
        drain(&mut commands);
        let thread_slice = vec![
            remote("1", "bob", "older reply 1"),
            remote("2", "bob", "older reply 2"),
        ];
        app.on_matrix(detached_timeline(
            "!a",
            generation,
            "$ev2",
            Some("$root"),
            false,
            thread_slice,
        ));
        assert!(app.thread_open());
        assert!(app.timeline.detached);
        app.on_event(key(KeyCode::End));
        assert!(app.thread_open());
        assert!(!app.timeline.detached);
        let cmds = drain(&mut commands);
        assert!(
            cmds.iter().any(|c| c == "OpenThread(!a, $root)"),
            "{cmds:?}"
        );
    }

    #[test]
    fn jump_failure_toasts_without_touching_active_view() {
        let (mut app, _terminal, mut commands) = connected();
        app.rooms[0].messages = vec![remote("1", "ann", "first"), remote("2", "bob", "second")];
        app.timeline.selected = Some(1);
        app.timeline.scroll = 4;
        app.composer.set_text("draft in progress");
        let saved_gen = app.generation;
        drain(&mut commands);

        app.jump_to_event("!b".into(), "$missing".into(), None);
        let jump_gen = app.next_gen;
        assert_eq!(
            drain(&mut commands),
            [format!("OpenEvent(!b, generation {jump_gen})")]
        );

        // Stale JumpFailed for another generation is ignored.
        app.on_matrix(MatrixEvent::JumpFailed {
            generation: jump_gen.saturating_sub(1),
            reason: "stale failure".into(),
        });
        assert!(app.toast.is_none());
        assert!(app.pending_jump.is_some());

        app.on_matrix(MatrixEvent::JumpFailed {
            generation: jump_gen,
            reason: "Message not found".into(),
        });
        assert!(app.pending_jump.is_none());
        assert_eq!(toast_text(&app), Some("Message not found"));
        assert_eq!(app.active, 0);
        assert_eq!(app.generation, saved_gen);
        assert_eq!(app.rooms[0].messages.len(), 2);
        assert_eq!(app.timeline.selected, Some(1));
        assert_eq!(app.timeline.scroll, 4);
        assert_eq!(app.composer.text(), "draft in progress");
    }

    #[test]
    fn detached_view_blocks_read_receipts_and_keeps_live_unreads_intact() {
        let (mut app, mut terminal, mut commands) = connected();
        app.rooms[0].messages = vec![remote("10", "ann", "recent")];
        redraw(&mut app, &mut terminal);
        drain(&mut commands);

        app.jump_to_event("!a".into(), "$ev2".into(), None);
        let generation = app.next_gen;
        drain(&mut commands);
        let slice = (0..5)
            .map(|i| remote(&i.to_string(), "bob", &format!("slice {i}")))
            .collect();
        app.on_matrix(detached_timeline(
            "!a", generation, "$ev2", None, false, slice,
        ));
        redraw(&mut app, &mut terminal);
        app.after_draw();

        // Reading a historical slice never marks the live room read.
        let cmds = drain(&mut commands);
        assert!(!cmds.iter().any(|c| c.contains("MarkRead")), "{cmds:?}");
        assert!(app.timeline.marked_read.is_none());

        // Sidebar unread counters continue updating while detached.
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!a", "Alpha", RoomCategory::Room, 3),
            summary("!b", "Beta", RoomCategory::Room, 2),
        ]));
        assert_eq!(app.rooms[0].unread, 3);
        assert_eq!(app.rooms[1].unread, 2);

        // Returning to live and redrawing resumes read receipts cleanly.
        app.on_event(key(KeyCode::End));
        drain(&mut commands);
        app.on_matrix(timeline(&app, vec![remote("11", "ann", "newest")]));
        redraw(&mut app, &mut terminal);
        app.after_draw();
        let cmds = drain(&mut commands);
        assert!(cmds.iter().any(|c| c == "MarkRead(!a)"), "{cmds:?}");
    }

    #[test]
    fn detached_forward_pagination_anchors_view_and_stops_at_end() {
        let (mut app, mut terminal, mut commands) = connected();
        drain(&mut commands);

        app.jump_to_event("!a".into(), "$ev5".into(), None);
        let generation = app.next_gen;
        drain(&mut commands);
        let slice: Vec<Message> = (0..25)
            .map(|i| remote(&i.to_string(), "bob", &format!("msg {i:02}")))
            .collect();
        app.on_matrix(detached_timeline(
            "!a", generation, "$ev5", None, false, slice,
        ));

        // Scroll down to the bottom of the slice (`scroll == 0`), triggering forward pagination.
        for _ in 0..8 {
            app.on_event(key(KeyCode::PageDown));
        }
        assert_eq!(app.timeline.scroll, 0);
        frame(&mut app, &mut terminal);
        assert_eq!(drain(&mut commands), ["PaginateForwards(!a)"]);
        assert!(app.timeline.loading_newer);
        redraw(&mut app, &mut terminal);
        assert!(screen_text(&terminal).contains("loading newer messages"));

        // Deliver newer messages; viewport anchor keeps bottom visible line above the new messages.
        let mut expanded: Vec<Message> = (0..32)
            .map(|i| remote(&i.to_string(), "bob", &format!("msg {i:02}")))
            .collect();
        app.on_matrix(MatrixEvent::Timeline {
            room_id: "!a".into(),
            generation: app.generation,
            messages: expanded.clone(),
            reached_start: false,
            detached: Some(crate::matrix::DetachedTimeline {
                target_event: "$ev5".into(),
                thread_root: None,
                reached_end: false,
            }),
        });
        app.on_matrix(MatrixEvent::PaginatedForwards {
            generation: app.generation,
            reached_end: false,
        });
        assert!(!app.timeline.loading_newer);
        redraw(&mut app, &mut terminal);
        let text = screen_text(&terminal);
        assert!(
            app.timeline.scroll > 0,
            "anchor should hold above newer items"
        );
        assert!(text.contains("msg 24"), "{text}");

        // Scroll down again to reach the bottom, then hit the end of the timeline.
        for _ in 0..8 {
            app.on_event(key(KeyCode::PageDown));
        }
        assert_eq!(app.timeline.scroll, 0);
        redraw(&mut app, &mut terminal);
        app.after_draw();
        assert_eq!(drain(&mut commands), ["PaginateForwards(!a)"]);

        expanded.push(remote("32", "bob", "msg 32"));
        app.on_matrix(MatrixEvent::Timeline {
            room_id: "!a".into(),
            generation: app.generation,
            messages: expanded,
            reached_start: false,
            detached: Some(crate::matrix::DetachedTimeline {
                target_event: "$ev5".into(),
                thread_root: None,
                reached_end: true,
            }),
        });
        app.on_matrix(MatrixEvent::PaginatedForwards {
            generation: app.generation,
            reached_end: true,
        });
        assert!(app.timeline.reached_end);
        redraw(&mut app, &mut terminal);
        app.after_draw();
        assert!(
            !drain(&mut commands)
                .iter()
                .any(|c| c.contains("PaginateForwards"))
        );
    }

    #[test]
    fn opening_links_handles_event_room_user_broken_and_http() {
        let _ = take_opened_urls();
        let (mut app, _terminal, mut commands) = connected();
        app.rooms[0].alias = Some("#alpha:example.org".into());
        app.rooms[0].messages = vec![remote("1", "ann", "already here")];
        app.rooms[1].id = "!b:example.org".into();
        drain(&mut commands);

        // 1. Joined event by room ID -> OpenEvent.
        assert!(app.try_open_matrix_link("https://matrix.to/#/%21b:example.org/%24old"));
        let jump1_gen = app.next_gen;
        assert_eq!(
            drain(&mut commands),
            [format!("OpenEvent(!b:example.org, generation {jump1_gen})")]
        );
        assert!(take_opened_urls().is_empty());

        // 2. Joined event by room alias ($ev1 loaded in room 0 -> selects locally and cancels prior jump).
        assert!(app.try_open_matrix_link("matrix:r/alpha:example.org/e/ev1"));
        assert_eq!(
            drain(&mut commands),
            [format!("CancelOpenEvent(generation {jump1_gen})")]
        );
        assert_eq!(app.timeline.selected, Some(0));

        // 3. Event in an unjoined room -> toast, never OpenEvent or browser.
        assert!(app.try_open_matrix_link("https://matrix.to/#/%21unknown:example.org/%24ev"));
        assert_eq!(
            toast_text(&app),
            Some("Join this room before jumping to a message in it")
        );
        assert!(drain(&mut commands).is_empty());
        assert!(take_opened_urls().is_empty());

        // 4. Joined room link -> switches room.
        assert!(app.try_open_matrix_link("https://matrix.to/#/%21b:example.org"));
        assert_eq!(app.active, 1);
        drain(&mut commands);

        // 5. Unjoined room with ?via= -> Stage::Preview (never joins sight unseen).
        assert!(app.try_open_matrix_link(
            "https://matrix.to/#/%23public:example.org?via=one.org&via=two.org"
        ));
        match &app.modal {
            Some(Modal::New(popup)) => {
                assert_eq!(popup.stage, crate::manage::Stage::Preview);
                assert_eq!(popup.preview_target, "#public:example.org");
                assert_eq!(
                    popup.preview_via,
                    vec!["one.org".to_owned(), "two.org".to_owned()]
                );
                assert!(popup.preview_loading);
            }
            other => panic!("expected Preview modal, got {other:?}"),
        }
        assert_eq!(
            drain(&mut commands),
            vec!["Manage(Preview(request 1, via 2 servers))".to_owned()]
        );
        app.modal = None;

        // 6. User link -> UserInfo modal for that user.
        assert!(app.try_open_matrix_link("matrix:u/carol:example.org"));
        match &app.modal {
            Some(Modal::UserInfo(popup)) => {
                assert_eq!(popup.user_id, "@carol:example.org");
            }
            other => panic!("expected UserInfo modal, got {other:?}"),
        }
        app.modal = None;

        // 7. Broken Matrix link -> toast, never launches browser.
        assert!(app.try_open_matrix_link("https://matrix.to/#/not-valid"));
        assert!(app.toast.is_some());
        assert!(take_opened_urls().is_empty());

        // 8. Normal HTTP link inside a selected message -> opens external browser.
        let web = remote("2", "ann", "see https://example.com/docs");
        app.rooms[app.active].messages.push(web);
        app.timeline.selected = Some(app.rooms[app.active].messages.len() - 1);
        app.set_focus(Pane::Timeline);
        app.perform(Action::Open);
        assert_eq!(take_opened_urls(), ["https://example.com/docs"]);
    }

    #[test]
    fn jump_to_reply_supports_loaded_unloaded_quote_click_menu_and_non_replies() {
        let (mut app, mut terminal, mut commands) = connected();
        let m0 = remote("0", "ann", "root question");
        let mut m1 = remote("1", "bob", "loaded reply");
        m1.reply_to = Some(("ann".into(), "root question".into()));
        m1.reply_event = Some("$ev0".into());
        let mut m2 = remote("2", "bob", "unloaded reply");
        m2.reply_to = Some(("ann".into(), "an earlier message".into()));
        m2.reply_event = Some("$ev_old".into());
        app.rooms[0].messages = vec![m0, m1, m2];
        drain(&mut commands);

        // Loaded reply via `p` key selects target locally.
        app.set_focus(Pane::Timeline);
        app.timeline.selected = Some(1);
        app.on_event(key(KeyCode::Char('p')));
        assert_eq!(app.timeline.selected, Some(0));
        assert!(drain(&mut commands).is_empty());

        // Clicking the quote line jumps to m0 too.
        app.timeline.selected = Some(1);
        redraw(&mut app, &mut terminal);
        let (col, quote_row) = find(&terminal, "┃ ann: root question");
        app.on_event(click(col, quote_row));
        assert_eq!(app.timeline.selected, Some(0));

        // Message context menu offers "Go to replied message" for replies.
        app.timeline.selected = Some(1);
        app.perform(Action::ContextMenu);
        match &app.modal {
            Some(Modal::Menu(menu)) => {
                assert!(
                    menu.items
                        .iter()
                        .any(|item| item.label == "Go to replied message"),
                    "{:?}",
                    menu.items
                        .iter()
                        .map(|item| &item.label)
                        .collect::<Vec<_>>()
                );
            }
            other => panic!("expected message menu, got {other:?}"),
        }
        app.modal = None;

        // Unloaded reply via `p` dispatches OpenEvent.
        app.timeline.selected = Some(2);
        app.on_event(key(KeyCode::Char('p')));
        assert_eq!(
            drain(&mut commands),
            [format!("OpenEvent(!a, generation {})", app.next_gen)]
        );

        // Non-reply message via `p` shows a clear toast.
        app.cancel_pending_jump();
        app.timeline.selected = Some(0);
        app.on_event(key(KeyCode::Char('p')));
        assert_eq!(toast_text(&app), Some("That message is not a reply"));
    }

    #[test]
    fn a_start_link_to_a_room_that_arrives_late_opens_the_room_not_a_preview() {
        let (mut app, _terminal) = drawn();
        let (handle, mut commands) = MatrixHandle::for_test();
        app.rooms.clear();
        app.matrix = Some(handle);
        app.user_id = Some("@me:example.org".into());
        app.startup_room_wait = Duration::from_secs(3600);
        app.startup_target = Some("!late:example.org".into());

        // Synced, with a first list that does not have the room yet: nothing is decided.
        app.on_matrix(MatrixEvent::Rooms(vec![summary(
            "!a:example.org",
            "Alpha",
            RoomCategory::Room,
            0,
        )]));
        app.on_matrix(MatrixEvent::Sync(SyncStatus::Synced));
        app.on_tick();
        assert!(app.startup_target.is_some());
        assert!(app.modal.is_none());

        // The room arrives: it is opened.
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!a:example.org", "Alpha", RoomCategory::Room, 0),
            summary("!late:example.org", "Late", RoomCategory::Room, 0),
        ]));
        assert!(app.startup_target.is_none());
        assert!(app.modal.is_none());
        assert_eq!(app.room_id().as_deref(), Some("!late:example.org"));
        drain(&mut commands);

        // A popup in the way stops the wait; it starts again afterwards.
        app.startup_target = Some("!behind:example.org".into());
        app.startup_missing_since = None;
        app.on_tick();
        assert!(app.startup_missing_since.is_some());
        app.perform(Action::OpenEmoji);
        assert!(app.modal.is_some());
        app.on_tick();
        assert!(app.startup_missing_since.is_none());
        app.modal = None;
        app.startup_target = None;

        // A room that never arrives is previewed once the wait is over.
        app.startup_target = Some("!never:example.org".into());
        app.startup_missing_since = None;
        app.on_tick();
        assert!(app.startup_target.is_some());
        app.startup_room_wait = Duration::ZERO;
        app.on_tick();
        assert!(app.startup_target.is_none());
        assert!(app.modal.is_some());
    }

    #[test]
    fn cli_startup_target_waits_for_sync_and_recovery_and_fires_once() {
        let (mut app, _terminal) = drawn();
        let (handle, mut commands) = MatrixHandle::for_test();
        app.rooms.clear();
        app.matrix = Some(handle);
        app.startup_target = Some("https://matrix.to/#/%21b:example.org/%24start".into());

        // NeedLogin must not drop startup_target.
        app.on_matrix(MatrixEvent::NeedLogin { reason: None });
        assert!(app.startup_target.is_some());
        assert!(drain(&mut commands).is_empty());

        // LoggedIn + Recovery(NeedsKey) opens recovery and keeps target deferred.
        app.modal = None;
        app.on_matrix(MatrixEvent::LoggedIn {
            user_id: "@alice:example.org".into(),
        });
        app.on_matrix(MatrixEvent::Recovery(
            crate::matrix::RecoveryStatus::NeedsKey,
        ));
        assert!(matches!(app.modal, Some(Modal::Recovery(_))));
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!a:example.org", "Alpha", RoomCategory::Room, 0),
            summary("!b:example.org", "Beta", RoomCategory::Room, 0),
        ]));
        app.on_matrix(MatrixEvent::Sync(SyncStatus::Synced));
        let pre_cmds = drain(&mut commands);
        assert!(
            !pre_cmds.iter().any(|c| c.starts_with("OpenEvent")),
            "{pre_cmds:?}"
        );

        // Unlocking recovery executes startup_target once and clears it.
        app.on_matrix(MatrixEvent::RecoverResult(Ok(())));
        assert!(app.modal.is_none());
        assert!(app.startup_target.is_none());
        let cmds = drain(&mut commands);
        assert_eq!(
            cmds,
            [format!(
                "OpenEvent(!b:example.org, generation {})",
                app.next_gen
            )]
        );

        // Further room updates never re-trigger the consumed startup_target.
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary("!a:example.org", "Alpha", RoomCategory::Room, 0),
            summary("!b:example.org", "Beta", RoomCategory::Room, 0),
        ]));
        assert!(drain(&mut commands).is_empty());

        // Startup link in demo mode works without a worker.
        let mut demo = App::demo();
        demo.startup_target = Some("https://matrix.to/#/@alice:example.org".into());
        demo.maybe_open_startup_target();
        assert!(demo.startup_target.is_none());
        assert!(matches!(
            &demo.modal,
            Some(Modal::UserInfo(popup)) if popup.user_id == "@alice:example.org"
        ));
    }

    #[test]
    fn detached_banner_counts_new_messages_and_ignores_notices_when_detached() {
        use crate::matrix::Notice;
        use crate::notify::NotifyMode;
        let (mut app, mut terminal, mut commands) = connected();
        app.notify_mode = NotifyMode::Terminal;
        app.rooms[0].messages = vec![remote("1", "ann", "live msg")];
        drain(&mut commands);

        let notice = |room: &str, thread: Option<&str>| {
            MatrixEvent::Notice(Notice {
                room_id: room.into(),
                room: "Alpha".into(),
                sender: "bob".into(),
                body: "ping".into(),
                highlight: false,
                thread: thread.map(str::to_owned),
            })
        };

        // Live view at bottom & focused: notice suppressed because message is on screen.
        app.on_matrix(notice("!a", None));
        assert!(app.take_notices().0.is_empty());

        // Jump to an unloaded message: detached slice opens at generation `g`.
        app.jump_to_event("!a".into(), "$old1".into(), None);
        let generation = app.next_gen;
        drain(&mut commands);
        app.on_matrix(detached_timeline(
            "!a",
            generation,
            "$old1",
            None,
            false,
            vec![
                remote("1", "bob", "old one"),
                Message {
                    item: Some(crate::model::ItemRef::Event("$old1".into())),
                    ..remote("2", "bob", "old target")
                },
            ],
        ));
        assert!(app.timeline.detached);
        assert_eq!(app.timeline.scroll, 0);

        // Even with scroll == 0 and window focused, a new live message triggers a notice
        // when viewing a detached slice (`!detached` is required for watching).
        app.on_matrix(notice("!a", None));
        assert_eq!(app.take_notices().0.len(), 1);

        // Stale generation DetachedNew is ignored; matching generation updates banner.
        app.on_matrix(MatrixEvent::DetachedNew {
            generation: generation - 1,
            count: 9,
        });
        assert_eq!(app.timeline.detached_new, 0);
        app.on_matrix(MatrixEvent::DetachedNew {
            generation,
            count: 3,
        });
        assert_eq!(app.timeline.detached_new, 3);
        redraw(&mut app, &mut terminal);
        find(&terminal, "viewing older messages · 3 new · End");

        // Pagination within the same detached slice preserves detached_new.
        app.on_matrix(detached_timeline(
            "!a",
            generation,
            "$old1",
            None,
            false,
            vec![remote("0", "bob", "older still")],
        ));
        assert_eq!(app.timeline.detached_new, 3);

        // Works identically inside a detached thread.
        app.jump_to_event("!a".into(), "$reply_old".into(), Some("$root".into()));
        let tgen = app.next_gen;
        app.on_matrix(detached_timeline(
            "!a",
            tgen,
            "$reply_old",
            Some("$root"),
            false,
            vec![Message {
                item: Some(crate::model::ItemRef::Event("$reply_old".into())),
                ..remote("3", "bob", "old reply")
            }],
        ));
        app.on_matrix(MatrixEvent::DetachedNew {
            generation: tgen,
            count: 2,
        });
        redraw(&mut app, &mut terminal);
        find(&terminal, "viewing older messages · 2 new · End");

        // Returning to live clears detached_new.
        app.perform(Action::JumpToNewest);
        assert_eq!(app.timeline.detached_new, 0);
    }

    #[test]
    fn room_list_quick_filters_combine_and_reset_on_esc_and_logout() {
        let (mut app, mut terminal, mut commands) = connected();
        let mut dm_fav = summary("!dmfav", "Alice", RoomCategory::Favourite, 1);
        dm_fav.direct = true;
        let mut dm_read = summary("!dm", "Dan", RoomCategory::Person, 0);
        dm_read.direct = true;
        let mut low_unread = summary("!low", "LowNews", RoomCategory::LowPriority, 4);
        low_unread.direct = false;
        let ch_unread = summary("!unread", "Rust", RoomCategory::Room, 2);
        let ch_open = summary("!open", "OpenChannel", RoomCategory::Room, 0);
        app.on_matrix(MatrixEvent::Rooms(vec![
            dm_fav, ch_open, dm_read, ch_unread, low_unread,
        ]));
        app.open_room(1); // OpenChannel (`!open`, read group room)
        drain(&mut commands);

        let names = |app: &App| -> Vec<String> {
            app.visible_rooms()
                .into_iter()
                .map(|i| app.rooms[i].name.clone())
                .collect()
        };
        assert_eq!(
            names(&app),
            ["Alice", "Dan", "OpenChannel", "Rust", "LowNews"]
        );

        // Alt-O ("Unread only"): keeps unread rooms + OpenChannel (active).
        app.on_event(key_mod(KeyCode::Char('o'), KeyModifiers::ALT));
        assert!(app.sidebar.unread_only);
        assert_eq!(names(&app), ["Alice", "OpenChannel", "Rust", "LowNews"]);

        // Alt-D ("People only"): combined AND filter -> direct unread (Alice) + OpenChannel (active).
        app.on_event(key_mod(KeyCode::Char('d'), KeyModifiers::ALT));
        assert!(app.sidebar.people_only);
        assert_eq!(names(&app), ["Alice", "OpenChannel"]);

        // Toggle Unread off via clicking chip -> People only (Alice in Favourites + Dan + OpenChannel).
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "● Unread");
        app.on_event(click(col + 2, row));
        assert!(!app.sidebar.unread_only);
        assert_eq!(names(&app), ["Alice", "Dan", "OpenChannel"]);

        // Combine with space + text filter.
        app.on_matrix(MatrixEvent::Spaces(vec![crate::matrix::SpaceSummary {
            id: "!sp".into(),
            name: "Friends".into(),
            level: 0,
            rooms: vec!["!dmfav".into(), "!open".into()],
            ..Default::default()
        }]));
        app.select_space(Some(0));
        assert_eq!(names(&app), ["Alice", "OpenChannel"]);
        app.sidebar.filter.set("ali");
        assert_eq!(names(&app), ["Alice"]);
        app.sidebar.filter.clear();

        // Command palette toggles People off cleanly.
        app.on_event(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL));
        type_text(&mut app, "Filter: People");
        app.on_event(key(KeyCode::Enter));
        assert!(!app.sidebar.people_only);

        // Narrow sidebar (inner.width < 18): compact indicator instead of chip strip.
        app.sidebar.width = 16;
        app.perform(Action::FilterUnread);
        app.perform(Action::FilterPeople);
        redraw(&mut app, &mut terminal);
        let screen = screen_text(&terminal);
        assert!(!screen.contains("● Unread"), "{screen}");
        find(&terminal, " UP");

        // Esc in sidebar resets quick filters and typed filter.
        app.sidebar.filter.set("rust");
        app.set_focus(Pane::Sidebar);
        app.on_event(key(KeyCode::Esc));
        assert!(app.sidebar.filter.is_empty());
        assert!(!app.sidebar.unread_only);
        assert!(!app.sidebar.people_only);

        // Session-only: reset_session clears quick filters, and Config ignores them.
        app.perform(Action::FilterUnread);
        app.perform(Action::FilterPeople);
        app.reset_session();
        assert!(!app.sidebar.unread_only);
        assert!(!app.sidebar.people_only);
        let cfg = Config::parse("unread_only = true\npeople_only = true\n").unwrap();
        let fresh = App::new(&cfg, PathBuf::new(), None);
        assert!(!fresh.sidebar.unread_only);
        assert!(!fresh.sidebar.people_only);
    }

    #[test]
    fn space_unread_counts_subspaces_muted_shared_other_indicator_and_palette() {
        use crate::matrix::SpaceSummary;
        let (mut app, mut terminal, _commands) = connected();
        let r_parent = summary("!rp", "ParentRoom", RoomCategory::Room, 2);
        let mut r_sub = summary("!rs", "SubRoom", RoomCategory::Room, 3);
        r_sub.mention = true;
        let mut r_muted = summary("!rm", "MutedRoom", RoomCategory::Room, 9);
        r_muted.muted = true;
        r_muted.mention = true;
        let r_shared = summary("!rsh", "SharedRoom", RoomCategory::Room, 4);
        let mut r_marked = summary("!rmk", "MarkedRoom", RoomCategory::LowPriority, 0);
        r_marked.marked_unread = true;
        let mut r_direct_low = summary("!dl", "Dana", RoomCategory::LowPriority, 0);
        r_direct_low.direct = true;
        r_direct_low.marked_unread = true;

        app.on_matrix(MatrixEvent::Rooms(vec![
            r_parent,
            r_sub,
            r_muted,
            r_shared,
            r_marked,
            r_direct_low,
        ]));
        app.on_matrix(MatrixEvent::Spaces(vec![
            // Parent Work lists !rp AND !rs directly; Team also lists !rs and !rm and !rsh -> !rs must not double-count, !rm is muted!
            SpaceSummary {
                id: "!work".into(),
                name: "Work".into(),
                level: 0,
                rooms: vec!["!rp".into(), "!rs".into()],
                ..Default::default()
            },
            SpaceSummary {
                id: "!team".into(),
                name: "Team".into(),
                level: 1,
                rooms: vec!["!rs".into(), "!rm".into(), "!rsh".into()],
                ..Default::default()
            },
            // Second top-level space Play shares !rsh and contains marked-unread !rmk.
            SpaceSummary {
                id: "!play".into(),
                name: "Play".into(),
                level: 0,
                rooms: vec!["!rsh".into(), "!rmk".into()],
                ..Default::default()
            },
        ]));

        // Work (level 0): !rp(2) + !rs(3,@) + !rsh(4) = 9, mention = true (!rm muted excluded, !rs counted once!).
        assert_eq!(
            app.space_unread(0),
            crate::model::SpaceUnread {
                unread: true,
                notifications: 9,
                mention: true,
            }
        );
        // Team (sub-space): !rs(3,@) + !rsh(4) = 7, mention = true.
        assert_eq!(
            app.space_unread(1),
            crate::model::SpaceUnread {
                unread: true,
                notifications: 7,
                mention: true,
            }
        );
        // Play (level 0): !rsh(4) + !rmk(marked, 0) = 4, unread = true, mention = false (!rsh counts in BOTH Work and Play!).
        assert_eq!(
            app.space_unread(2),
            crate::model::SpaceUnread {
                unread: true,
                notifications: 4,
                mention: false,
            }
        );

        // Selecting Work: other_spaces_unread sees !rmk (marked_unread, 0 notifications -> unread: true), ignores Team rooms already in Work!
        app.select_space(Some(0));
        assert_eq!(
            app.other_spaces_unread(),
            crate::model::SpaceUnread {
                unread: true,
                notifications: 0,
                mention: false,
            }
        );
        redraw(&mut app, &mut terminal);
        find(&terminal, "◂ Work 9 ▸ •");

        // Selecting Play: other spaces (!rp 2 + !rs 3,@) -> 5 unread + mention indicator "+5".
        app.select_space(Some(2));
        assert_eq!(
            app.other_spaces_unread(),
            crate::model::SpaceUnread {
                unread: true,
                notifications: 5,
                mention: true,
            }
        );
        redraw(&mut app, &mut terminal);
        find(&terminal, "◂ Play 4 ▸ +5");

        // All rooms: no "other" indicator.
        app.select_space(None);
        assert!(!app.other_spaces_unread().unread);

        // Command palette shows spaces with unread counts and switches to Work › Team.
        app.on_event(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL));
        redraw(&mut app, &mut terminal);
        type_text(&mut app, "Work Team");
        redraw(&mut app, &mut terminal);
        find(&terminal, "7 unread · @");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.space_name().as_deref(), Some("Work › Team"));
    }

    /// The net for a field added later: the whole app, filled with recognisable private
    /// things and with one popup after another open, says none of them in `Debug`.
    #[test]
    fn the_debug_output_of_the_app_holds_nothing_private() {
        const PRIVATE: &[&str] = &[
            "hidden-room",
            "Secret Garden",
            "#garden",
            "@alice",
            "Alice Hidden",
            "@bob",
            "Bob Hidden",
            "$launch",
            "launch codes",
            "private.example",
            "needle",
            "my private draft",
            "garden filter",
            "typed into",
            "copied secret",
            "toast about",
            "/home/alice",
            "raise-alice",
            "@self-secret",
            "@troll",
            "!space-secret",
            "Space Secret",
            "Hush",
        ];
        let mut app = App::demo();
        let (handle, _commands) = MatrixHandle::for_test();
        app.matrix = Some(handle);
        // A list of saved messages with all that goes with it: an entry in a thread
        // with a reminder, its file, a notification on its way, a question asked.
        let kept = crate::saved::SavedItem {
            room_id: "!hidden-room:private.example".into(),
            event_id: "$launch".into(),
            thread: Some("$launch-root".into()),
            saved_at: 1,
            remind_at: Some(2),
            done: false,
        };
        app.bookmarks.store.add(kept.clone()).unwrap();
        app.bookmarks.file = Some("/home/alice/saved.json".into());
        app.bookmarks.waiting.push(crate::saved::Waiting {
            request: 1,
            items: vec![kept],
            since: Instant::now(),
        });
        app.bookmarks.notices.push(crate::notify::DueReminder {
            notice: crate::matrix::Notice {
                room_id: "!hidden-room:private.example".into(),
                room: "Secret Garden".into(),
                sender: "Alice Hidden".into(),
                body: "launch codes".into(),
                highlight: false,
                thread: None,
            },
            event: Some(("$launch".into(), None)),
            count: 1,
        });
        app.bookmarks.confirm = Some((
            ("!hidden-room:private.example".into(), "$launch".into()),
            Instant::now(),
        ));
        app.user_id = Some("@self-secret:example.org".into());
        app.own_profile = Some(crate::matrix::OwnProfile {
            display_name: Some("Hush Self".into()),
            avatar: Some("mxc://private.example.org/me".into()),
            room_names: Default::default(),
        });
        let message = |id: &str, sender: &str, sender_id: &str, body: &str| Message {
            id: id.to_owned(),
            item: Some(ItemRef::Event(format!("${id}:example.org"))),
            sender: sender.to_owned(),
            sender_id: Some(sender_id.to_owned()),
            body: body.to_owned(),
            html: Some(format!("<b>{body}</b>")),
            ..Message::default()
        };
        let mut room = Room::new("!hidden-room:example.org", "Secret Garden", RoomKind::Room);
        room.alias = Some("#garden:example.org".into());
        room.messages = vec![
            message(
                "launch",
                "Alice Hidden",
                "@alice:example.org",
                "the launch codes are at https://private.example.org/plans",
            ),
            message(
                "needle",
                "Bob Hidden",
                "@bob:example.org",
                "needle in the haystack",
            ),
        ];
        app.rooms = vec![room];
        app.active = 0;
        app.spaces = vec![crate::matrix::SpaceSummary {
            id: "!space-secret:example.org".into(),
            name: "Space Secret".into(),
            level: 0,
            rooms: vec!["!hidden-room:example.org".into()],
            children: vec!["!hidden-room:example.org".into()],
            can_edit: true,
        }];
        app.link_previews = crate::config::LinkPreviews::All;
        app.download_dir = "/home/alice/Downloads".into();
        app.notification_click_command = Some("raise-alice-terminal".into());
        app.pending_copy = Some("copied secret".into());
        app.toast = Some(("toast about Secret Garden".into(), Instant::now()));
        app.ignored_users.insert("@troll:example.org".into());
        app.timeline.marked_read = Some("$needle:example.org".into());
        app.sidebar.filter.set("garden filter");
        // A translation under a message, and the commands and files that make notes.
        app.notes.notes.set(
            "$launch:example.org",
            crate::notes::NoteKind::Translation,
            crate::notes::NoteState::Done("the launch codes, translated".into()),
        );
        app.notes.transcribe_command = "/home/alice/bin/hear".into();
        app.notes.transcribe_model = "/home/alice/model.bin".into();
        app.notes.translate_command = "raise-alice-translate --key Hush".into();
        app.notes.translate_language = "Hush".into();
        // The window title as written, and what the doctor keeps.
        assert!(app.take_window_title().is_some());
        app.found.key_problems = 2;
        app.found.ignored_settings = vec!["mouse".into()];
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        // Lays out the messages: the row cache and the link previews fill.
        redraw(&mut app, &mut terminal);
        assert!(app.row_cache.is_some());

        let mut shown = Vec::new();
        // A draft, and a mention being completed in it.
        app.on_room_members(
            "!hidden-room:example.org".into(),
            vec![crate::complete::Member {
                user_id: "@alice:example.org".into(),
                display_name: "Alice Hidden".into(),
            }],
            true,
        );
        app.focus = Pane::Composer;
        type_text(&mut app, "my private draft @Ali");
        assert!(app.complete.active.is_some());
        shown.push(format!("{app:?}"));
        app.on_event(key(KeyCode::Esc));

        // One popup after another; where it takes text, something is typed into it.
        for (action, takes_text) in [
            (Action::Search, true),
            (Action::ContextMenu, false),
            (Action::OpenPalette, true),
            (Action::RoomInfo, false),
            (Action::UserInfo, false),
            (Action::MessageDetails, false),
            (Action::OpenPinned, false),
            (Action::SaveForLater, false),
            (Action::OpenSaved, false),
            (Action::RemindMe, true),
            (Action::OpenThreads, true),
            (Action::NewRoom, false),
            (Action::React, true),
            (Action::ForwardMessage, true),
            (Action::Doctor, false),
            (Action::ArchiveRoom, false),
        ] {
            app.modal = None;
            app.focus = Pane::Timeline;
            app.timeline.selected = Some(0);
            app.perform(action);
            if takes_text {
                type_text(&mut app, "needle");
            }
            shown.push(format!("{app:?}"));
        }
        app.modal = Some(Modal::Settings(SettingsMenu {
            editing: Some(LineEdit::from("/home/alice/elsewhere")),
            ..SettingsMenu::default()
        }));
        shown.push(format!("{app:?}"));
        app.modal = Some(Modal::Login(LoginForm::reauth(
            crate::matrix::ReauthTarget {
                homeserver: "https://private.example.org".into(),
                user_id: "@self-secret:example.org".into(),
                device_id: "Hush".into(),
                kind: crate::matrix::ReauthKind::Password,
                client_id: None,
            },
            "Session of @self-secret expired".into(),
        )));
        type_text(&mut app, "typed into the password");
        shown.push(format!("{app:?}"));

        // A waiting voice note, a draft playback, and a running recording with private
        // folders/commands: none of them leak into Debug.
        app.modal = None;
        let voice_dir =
            std::env::temp_dir().join(format!("matrix-rat-Hush-debug-{}", std::process::id()));
        std::fs::create_dir_all(&voice_dir).unwrap();
        app.audio.dir = Some(voice_dir.clone());
        app.audio.recorder_setting = "/home/alice/bin/Hush-rec".into();
        app.set_test_recorder(
            crate::audio::recorder::Tool::new(PathBuf::from("/home/alice/Hush-cap"), Vec::new()),
            crate::audio::recorder::Tool::new(PathBuf::from("/home/alice/Hush-enc"), Vec::new()),
        );
        app.set_test_player(crate::audio::Kind::Other, voice_dir.clone());
        app.perform(Action::RecordVoice);
        let _req = app.take_record_start();
        let temp_voice = crate::audio::TempAudio::voice(&voice_dir, b"OggS").unwrap();
        let voice_id = app.audio.next_id;
        app.on_recording(crate::audio::record::RecordUpdate {
            id: voice_id,
            event: crate::audio::record::RecordEvent::Done(crate::audio::record::Recording {
                file: temp_voice,
                duration: Duration::from_secs(7),
                waveform: vec![123, 456, 789],
                size: 4,
                silent: false,
                full: false,
            }),
        });
        assert_eq!(app.attachments().len(), 1);
        app.perform(Action::PlayVoiceNote);
        let _play_req = app.take_audio_start().expect("draft player request");
        shown.push(format!("{app:?}"));
        app.stop_audio();
        app.perform(Action::RecordVoice);
        let _rec_req = app.take_record_start().expect("recording request");
        shown.push(format!("{app:?}"));
        app.cancel_recording();
        let _ = app.clear_attachments();
        let _ = std::fs::remove_dir_all(&voice_dir);

        // A thread, with the room's draft put aside.
        app.modal = None;
        app.composer.set_text("my private draft");
        app.focus = Pane::Timeline;
        app.timeline.selected = Some(1);
        app.perform(Action::OpenThread);
        assert!(app.threads.open.is_some());
        shown.push(format!("{app:?}"));

        let all = shown.join("\n");
        for popup in [
            "Search(",
            "Menu(",
            "Palette(",
            "RoomInfo(",
            "UserInfo(",
            "Details(",
            "Pinned(",
            "Saved(",
            "Threads(",
            "New(",
            "Emoji(",
            "Settings(",
            "Login(",
            "Doctor(",
            "Collect(",
        ] {
            assert!(all.contains(popup), "{popup} never open in {all}");
        }
        // What the doctor and the window title keep is no more than counts, switches
        // and our own setting names.
        assert!(all.contains("title_written: Some(\"matrix-rat"), "{all}");
        assert!(all.contains("found: Found {"), "{all}");
        assert!(all.contains("bookmarks: SavedState {"), "{all}");
        for text in &shown {
            for private in PRIVATE {
                assert!(!text.contains(private), "{private} in {text}");
            }
        }
    }

    #[tokio::test]
    async fn drain_or_abort_waits_for_quick_tasks_and_aborts_hung_ones() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct OnDrop(Arc<AtomicBool>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let mut quick = tokio::task::JoinSet::new();
        let finished = Arc::new(AtomicBool::new(false));
        let mark = Arc::clone(&finished);
        quick.spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            mark.store(true, Ordering::SeqCst);
        });
        assert!(drain_or_abort(&mut quick, Duration::from_secs(1)).await);
        assert!(finished.load(Ordering::SeqCst));

        let mut hung = tokio::task::JoinSet::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = OnDrop(Arc::clone(&dropped));
        hung.spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        // Let the spawned task start so the drop guard lives inside its future.
        tokio::task::yield_now().await;
        assert!(!drain_or_abort(&mut hung, Duration::from_millis(30)).await);
        assert!(dropped.load(Ordering::SeqCst));
        assert!(hung.is_empty());
    }
}
