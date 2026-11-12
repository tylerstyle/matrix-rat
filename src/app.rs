//! Application state, input handling and the event loop.

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
use crate::keymap::{self, Context};
use crate::model::{Message, OWN_NAME, Reaction, Room, RoomKind, demo_rooms};
use crate::paths::Paths;
use crate::terminal::{self, TerminalGuard};
use crate::theme::{DEFAULT_THEME, Theme, truecolor_supported};
use crate::ui::hit::{Button, HitId, HitMap};
use crate::ui::{self, SIDEBAR_MIN, max_sidebar};

const TOAST_TIME: Duration = Duration::from_secs(3);
const WHEEL_LINES: usize = 3;
const THUMBS_UP: &str = "👍";

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
}

impl App {
    pub fn new(config: &Config, themes_dir: PathBuf) -> Self {
        let truecolor = truecolor_supported();
        let mut app = Self::with_theme(Theme::builtin(DEFAULT_THEME).expect("default theme"));
        app.truecolor = truecolor;
        app.theme_names = Theme::available(&themes_dir);
        app.themes_dir = themes_dir;
        app.sidebar.width = config.sidebar_width;
        app.set_theme(&config.theme);
        app
    }

    /// Demo data with fixed settings; also what the tests render.
    #[cfg(test)]
    pub fn demo() -> Self {
        Self::with_theme(Theme::builtin(DEFAULT_THEME).expect("default theme"))
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
        self.rooms
            .get_mut(self.active)
            .map(|room| &mut room.messages)
    }

    // ----- state changes -------------------------------------------------------------

    pub fn show_toast(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), Instant::now()));
    }

    fn set_theme(&mut self, name: &str) {
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
            }
            Err(err) => {
                tracing::warn!(theme = name, ?err, "could not load theme");
                self.show_toast(format!("Could not load theme {name}: {err:#}"));
            }
        }
    }

    fn set_focus(&mut self, pane: Pane) {
        self.focus = pane;
        match pane {
            Pane::Timeline => {
                if self.timeline.selected.is_none() && !self.messages().is_empty() {
                    self.timeline.selected = Some(self.messages().len() - 1);
                }
                self.timeline.reveal_selected = true;
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
        if index != self.active {
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
        self.rooms[index].mention = false;
        self.timeline = TimelineState::default();
        self.sidebar.filter.clear();
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
        if text.is_empty() || self.room().is_none() {
            return;
        }
        self.composer.take();
        let mode = std::mem::replace(&mut self.compose_mode, ComposeMode::Normal);
        let mut message = Message {
            sender: OWN_NAME.to_owned(),
            time: "now".to_owned(),
            day: "Today".to_owned(),
            body: text,
            own: true,
            ..Message::default()
        };
        match mode {
            ComposeMode::Edit(i) if i < self.messages().len() => {
                if let Some(target) = self.messages_mut().and_then(|m| m.get_mut(i)) {
                    target.body = message.body;
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

    fn push_message(&mut self, message: Message) {
        if let Some(messages) = self.messages_mut() {
            messages.push(message);
        }
    }

    fn toggle_reaction(&mut self, msg: usize, key: &str) {
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
        if let Some(messages) = self.messages_mut() {
            messages.remove(index);
        }
        // Keep a pending reply or edit pointing at the right message.
        self.compose_mode = match self.compose_mode {
            ComposeMode::Reply(i) | ComposeMode::Edit(i) if i == index => {
                if matches!(self.compose_mode, ComposeMode::Edit(_)) {
                    self.composer.take();
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
            self.timeline.selected = Some(index.min(self.messages().len() - 1));
            self.timeline.reveal_selected = true;
        }
    }

    fn cancel_compose(&mut self) {
        if matches!(self.compose_mode, ComposeMode::Edit(_)) {
            self.composer.take();
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
                let at = self
                    .theme_names
                    .iter()
                    .position(|n| *n == self.theme.name)
                    .map_or(0, |i| (i + 1) % self.theme_names.len());
                let name = self.theme_names[at].clone();
                self.set_theme(&name);
            }
            Action::Back => self.back(),
            Action::PageUp => self.timeline.scroll += self.timeline.page.saturating_sub(1).max(1),
            Action::PageDown => {
                let step = self.timeline.page.saturating_sub(1).max(1);
                self.timeline.scroll = self.timeline.scroll.saturating_sub(step);
            }
            Action::JumpToNewest => {
                self.timeline.scroll = 0;
                if self.focus == Pane::Timeline && !self.messages().is_empty() {
                    self.timeline.selected = Some(self.messages().len() - 1);
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
                    self.timeline.selected = Some(i.saturating_sub(1));
                    self.timeline.reveal_selected = true;
                }
                _ => self.set_focus(Pane::Timeline),
            },
            Action::SelectNewer => match self.timeline.selected {
                Some(i) if i + 1 < self.messages().len() => {
                    self.timeline.selected = Some(i + 1);
                    self.timeline.reveal_selected = true;
                }
                _ => {
                    self.timeline.scroll = 0;
                    self.set_focus(Pane::Composer);
                }
            },
            Action::Reply => {
                if let Some(i) = self.timeline.selected {
                    if matches!(self.compose_mode, ComposeMode::Edit(_)) {
                        self.composer.take();
                    }
                    self.compose_mode = ComposeMode::Reply(i);
                    self.set_focus(Pane::Composer);
                }
            }
            Action::Edit => {
                let selected = self.timeline.selected;
                match selected.and_then(|i| self.messages().get(i).map(|m| (i, m))) {
                    Some((i, message)) if message.own => {
                        let body = message.body.clone();
                        self.composer.set_text(&body);
                        self.compose_mode = ComposeMode::Edit(i);
                        self.set_focus(Pane::Composer);
                    }
                    Some(_) => self.show_toast("You can only edit your own messages"),
                    None => {}
                }
            }
            Action::React => {
                if let Some(i) = self.timeline.selected {
                    self.toggle_reaction(i, THUMBS_UP);
                }
            }
            Action::Delete => {
                if let Some(i) = self.timeline.selected {
                    self.delete_message(i);
                }
            }
            Action::Copy => self.show_toast("Copying is not available yet"),
            Action::Open => self.show_toast("Nothing to open in this message"),

            Action::Send => self.send(),
            Action::Newline => self.composer.insert("\n"),
            Action::CancelCompose => self.cancel_compose(),
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
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Mouse(mouse) => self.on_mouse(mouse),
            Event::Paste(text) => self.on_paste(&text),
            Event::Resize(..) | Event::FocusGained => self.dirty = true,
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        self.dirty = true;
        let (code, mods) = keymap::normalize(key);

        if self.help_open {
            match (code, mods) {
                (KeyCode::Char('q'), KeyModifiers::CONTROL) => self.should_quit = true,
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
                if self.help_open {
                    if hit == Some(HitId::Backdrop) {
                        self.help_open = false;
                    }
                    return;
                }
                match hit {
                    Some(HitId::Splitter) => self.dragging_splitter = true,
                    Some(HitId::Room(index)) => self.open_room(index),
                    Some(HitId::RoomFilter) => self.set_focus(Pane::Sidebar),
                    Some(HitId::Message(index)) => {
                        self.set_focus(Pane::Timeline);
                        self.timeline.selected = Some(index);
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
                    Some(HitId::ComposerText) => {
                        self.set_focus(Pane::Composer);
                        let view = self.composer_view;
                        let row = (mouse.row - view.area.y) as usize + view.first_row;
                        let col = (mouse.column - view.area.x) as usize;
                        self.composer.place(view.width, row, col);
                    }
                    Some(HitId::Button(button)) => self.perform(match button {
                        Button::Help => Action::ToggleHelp,
                        Button::Theme => Action::CycleTheme,
                        Button::CancelCompose => Action::CancelCompose,
                        Button::JumpToNewest => Action::JumpToNewest,
                    }),
                    Some(HitId::Do(action)) => self.perform(action),
                    Some(HitId::Pane(pane)) => self.set_focus(pane),
                    Some(HitId::Backdrop | HitId::Overlay) | None => {}
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_splitter => {
                let width = (mouse.column + 1).saturating_sub(self.screen.x);
                self.sidebar.width = width.clamp(SIDEBAR_MIN, max_sidebar(self.screen.width));
                self.dirty = true;
            }
            MouseEventKind::Up(MouseButton::Left) => self.dragging_splitter = false,
            MouseEventKind::Moved => {
                if self.hover != hit {
                    self.hover = hit;
                    self.dirty = true;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if !self.help_open => {
                let up = mouse.kind == MouseEventKind::ScrollUp;
                // The wheel scrolls what is under the pointer, wherever the focus is.
                match self.hits.pane_at(mouse.column, mouse.row) {
                    Some(Pane::Timeline) => {
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
    }
}

pub async fn run(paths: &Paths, config: &Config) -> Result<()> {
    let guard = TerminalGuard::enter(config.mouse)?;
    // Terminal queries (image protocol, background colour) go here, before the event stream.
    let mut terminal = guard.activate()?;
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Without these, a kill or a closed terminal window would skip all cleanup.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let mut app = App::new(config, paths.themes_dir());

    while !app.should_quit {
        if app.suspend_requested {
            app.suspend_requested = false;
            terminal::suspend(&mut terminal)?;
            app.dirty = true;
        }
        if app.dirty {
            app.dirty = false;
            terminal.draw(|frame| ui::draw(frame, &mut app))?;
        }
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(event)) => app.on_event(event),
                Some(Err(err)) => return Err(err.into()),
                None => break,
            },
            _ = tick.tick() => app.on_tick(),
            _ = tokio::signal::ctrl_c() => app.should_quit = true,
            _ = sigterm.recv() => app.should_quit = true,
            _ = sighup.recv() => app.should_quit = true,
        }
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
        assert_eq!(app.rooms[0].messages.last().unwrap().reactions.len(), 1);
        app.on_event(key(KeyCode::Char('r')));
        assert!(app.rooms[0].messages.last().unwrap().reactions.is_empty());

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
