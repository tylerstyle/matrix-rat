//! Archiving a room as Markdown and catching up on what is unread, on `App`: the
//! popup of each, what is asked of the worker, the summary command that is started,
//! and what comes back.
//!
//! Both are about the room that room-level commands are about (the open one, or in
//! the room list the one under the cursor). The worker collects the messages
//! (`crate::matrix::collect`); for an archive it also writes the file, for a summary
//! the text comes here and is handed to the user's command (`crate::catchup`).

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyModifiers};
use tokio::sync::oneshot;

use super::App;
use crate::catchup::{self, Outcome};
use crate::collect::{Cut, Span};
use crate::files;
use crate::matrix::{Archived, CollectCmd, CollectEvent, MatrixCmd, UnreadText};
use crate::modal::Modal;

/// What a click in the popup is on (see `HitId::Collect`): a row of the "how far
/// back" list by its place, or one of these.
pub const CLICK_GO: u8 = 10;
pub const CLICK_CLOSE: u8 = 11;
pub const CLICK_COPY: u8 = 12;

/// What the app keeps for both features between events.
#[derive(Default)]
pub struct CollectState {
    /// Counts what is asked of the worker and of the summary command.
    requests: u64,
    /// The `summary_command` setting as written.
    pub(super) summary_setting: String,
    /// A summary command to start, for the event loop to take.
    start: Option<catchup::Request>,
    /// The way to stop the summary command that runs, and its number.
    running: Option<(u64, oneshot::Sender<()>)>,
}

/// Hand-written: the setting is a command line of the user's.
impl std::fmt::Debug for CollectState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollectState")
            .field("requests", &self.requests)
            .field("summary_set", &!self.summary_setting.trim().is_empty())
            .field("start", &self.start.is_some())
            .field("running", &self.running.is_some())
            .finish()
    }
}

/// How far an archive is.
#[derive(Clone, PartialEq, Eq)]
pub enum ArchiveStage {
    /// The question: how far back, with the warning and where the file goes.
    Asking,
    /// The worker loads and writes; so many events are loaded.
    Running {
        request: u64,
        loaded: usize,
    },
    Done(Archived),
    Failed(String),
}

/// The popup of an archive.
#[derive(Clone)]
pub struct ArchivePopup {
    pub room_id: String,
    pub room_name: String,
    /// The chosen row of [`Span::ALL`].
    pub selected: usize,
    /// Where the file goes.
    pub dir: PathBuf,
    /// The name it gets there if that is free.
    pub file_name: String,
    pub stage: ArchiveStage,
}

impl ArchivePopup {
    pub fn span(&self) -> Span {
        Span::ALL[self.selected.min(Span::ALL.len() - 1)]
    }
}

/// How far catching up is.
#[derive(Clone, PartialEq, Eq)]
pub enum CatchUpStage {
    /// The worker collects what is unread.
    Collecting {
        request: u64,
        loaded: usize,
    },
    /// The user's command has the text.
    Summarising {
        run: u64,
        messages: usize,
        cut: bool,
    },
    /// The summary, for the user alone.
    Done {
        summary: String,
        messages: usize,
        cut: bool,
    },
    /// Nothing is unread.
    Nothing,
    Failed(String),
}

/// The popup of a summary.
#[derive(Clone)]
pub struct CatchUpPopup {
    pub room_name: String,
    /// The name of the program that summarises, for the user to see where the text
    /// goes.
    pub program: String,
    pub stage: CatchUpStage,
    /// Lines the summary is scrolled down by, and how many fit (set when drawn).
    pub scroll: usize,
    pub page: usize,
}

/// The popup of either feature.
#[derive(Clone)]
pub enum Popup {
    Archive(ArchivePopup),
    CatchUp(CatchUpPopup),
}

/// Hand-written: the popups hold a room's name, a folder and a summary of what was
/// said. Only what they are at is shown.
impl std::fmt::Debug for Popup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (name, stage) = match self {
            Popup::Archive(popup) => (
                "Archive",
                match popup.stage {
                    ArchiveStage::Asking => "Asking",
                    ArchiveStage::Running { .. } => "Running",
                    ArchiveStage::Done(_) => "Done",
                    ArchiveStage::Failed(_) => "Failed",
                },
            ),
            Popup::CatchUp(popup) => (
                "CatchUp",
                match popup.stage {
                    CatchUpStage::Collecting { .. } => "Collecting",
                    CatchUpStage::Summarising { .. } => "Summarising",
                    CatchUpStage::Done { .. } => "Done",
                    CatchUpStage::Nothing => "Nothing",
                    CatchUpStage::Failed(_) => "Failed",
                },
            ),
        };
        write!(f, "{name}({stage})")
    }
}

/// "1 message", "3 messages".
pub fn count(messages: usize) -> String {
    if messages == 1 {
        "1 message".to_owned()
    } else {
        format!("{messages} messages")
    }
}

/// What to say about an archive that is not complete.
pub fn cut_note(cut: Option<Cut>) -> Option<String> {
    match cut? {
        Cut::Limit => Some(format!(
            "The room has more: one archive takes the newest {} events.",
            crate::matrix::ARCHIVE_EVENTS
        )),
        Cut::LoadFailed => Some("Not complete: older messages could not be loaded.".to_owned()),
    }
}

impl App {
    fn next_collect_request(&mut self) -> u64 {
        self.collect.requests = self.collect.requests.wrapping_add(1);
        self.collect.requests
    }

    /// The room both features would act on now, if it is one whose messages can be
    /// read: its place, or what to say instead.
    fn collect_target(&self) -> Result<usize, String> {
        let index = self.target_room().ok_or("No room is open")?;
        let room = &self.rooms[index];
        if room.is_pending() {
            return Err(room.kind.pending_hint().to_owned());
        }
        if self.matrix.is_none() {
            return Err("Not connected to an account".to_owned());
        }
        Ok(index)
    }

    // ----- the archive -------------------------------------------------------------

    /// Opens the question for an archive of the room: how far back, with the warning
    /// that it holds only text, and where the file goes. Nothing is loaded or
    /// written before the user says so there.
    pub(super) fn open_archive(&mut self) {
        let index = match self.collect_target() {
            Ok(index) => index,
            Err(why) => return self.show_toast(why),
        };
        let room = &self.rooms[index];
        let popup = ArchivePopup {
            room_id: room.id.clone(),
            room_name: room.name.clone(),
            selected: 0,
            dir: self.download_dir(),
            file_name: crate::archive::file_name(&room.name, chrono::Local::now().date_naive()),
            stage: ArchiveStage::Asking,
        };
        self.open_modal(Modal::Collect(Popup::Archive(popup)));
    }

    /// Enter on the question: the worker starts.
    fn start_archive(&mut self) {
        let request = self.next_collect_request();
        let Some(Modal::Collect(Popup::Archive(popup))) = &mut self.modal else {
            return;
        };
        if popup.stage != ArchiveStage::Asking {
            return;
        }
        popup.stage = ArchiveStage::Running { request, loaded: 0 };
        let command = CollectCmd::Archive {
            request,
            room_id: popup.room_id.clone(),
            span: popup.span(),
            dir: popup.dir.clone(),
            room_name: popup.room_name.clone(),
        };
        self.send_matrix(MatrixCmd::Collect(command));
    }

    fn on_archived(&mut self, request: u64, result: Result<Archived, String>) {
        if let Some(Modal::Collect(Popup::Archive(popup))) = &mut self.modal
            && matches!(popup.stage, ArchiveStage::Running { request: asked, .. } if asked == request)
        {
            popup.stage = match result {
                Ok(archived) => ArchiveStage::Done(archived),
                Err(why) => ArchiveStage::Failed(why),
            };
            return;
        }
        // Its popup is gone (another one took its place): the end is still told.
        if request != self.collect.requests {
            return;
        }
        match result {
            Ok(archived) => self.show_toast(format!(
                "Archived {} to {}",
                count(archived.messages),
                files::display(&archived.path)
            )),
            Err(why) => self.show_toast(format!("Could not archive the room: {why}")),
        }
    }

    // ----- catching up -------------------------------------------------------------

    /// The `summary_command` setting as a command, or why there is none.
    pub(super) fn summary_command(&self) -> Result<catchup::Command, catchup::NoCommand> {
        catchup::choose_here(&self.collect.summary_setting)
    }

    /// What the settings show for the summary command.
    pub(super) fn summary_setting_text(&self) -> String {
        let setting = self.collect.summary_setting.trim();
        if setting.is_empty() {
            return "not set".to_owned();
        }
        match self.summary_command() {
            Ok(_) => setting.to_owned(),
            Err(_) => format!("{setting} (not found)"),
        }
    }

    /// Enter on the summary command row: the setting is kept, and looked up at once.
    pub(super) fn set_summary_command(&mut self, text: &str) {
        let setting = text.trim().to_owned();
        self.collect.summary_setting = setting.clone();
        self.save_setting("summary_command", setting);
        let said = match self.summary_command() {
            Ok(command) => format!(
                "Catching up gives the unread text of a room to {}",
                command.name()
            ),
            Err(catchup::NoCommand::NotSet) => "Catching up is off: no summary command".to_owned(),
            Err(why) => why.message().to_owned(),
        };
        self.show_toast(said);
    }

    /// Collects what is unread in the room and has the user's command summarise it,
    /// in a popup. Not without a command; nothing is asked for where nothing is
    /// unread.
    pub(super) fn open_catch_up(&mut self) {
        let command = match self.summary_command() {
            Ok(command) => command,
            Err(why) => return self.show_toast(why.message()),
        };
        let index = match self.collect_target() {
            Ok(index) => index,
            Err(why) => return self.show_toast(why),
        };
        // The open room counts as read once it is looked at; its "new" line still
        // says from where it was unread.
        let from = (self.open_index() == Some(index) && !self.thread_open())
            .then(|| self.first_unread_event().map(str::to_owned))
            .flatten();
        let room = &self.rooms[index];
        if from.is_none() && !room.shows_unread() {
            return self.show_toast(format!("Nothing is unread in {}", room.name));
        }
        let room_id = room.id.clone();
        let room_name = room.name.clone();
        let request = self.next_collect_request();
        self.stop_summary();
        self.open_modal(Modal::Collect(Popup::CatchUp(CatchUpPopup {
            room_name,
            program: command.name(),
            stage: CatchUpStage::Collecting { request, loaded: 0 },
            scroll: 0,
            page: 1,
        })));
        self.send_matrix(MatrixCmd::Collect(CollectCmd::Unread {
            request,
            room_id,
            from,
        }));
    }

    fn on_unread_text(&mut self, request: u64, result: Result<UnreadText, String>) {
        // The command is looked up again: the setting may have changed meanwhile.
        let command = self.summary_command();
        let run = self.next_collect_request();
        let Some(Modal::Collect(Popup::CatchUp(popup))) = &mut self.modal else {
            return;
        };
        if !matches!(popup.stage, CatchUpStage::Collecting { request: asked, .. } if asked == request)
        {
            return;
        }
        let unread = match result {
            Ok(unread) if unread.messages == 0 => {
                popup.stage = CatchUpStage::Nothing;
                return;
            }
            Ok(unread) => unread,
            Err(why) => {
                popup.stage = CatchUpStage::Failed(format!("Could not load the messages: {why}"));
                return;
            }
        };
        let command = match command {
            Ok(command) => command,
            Err(why) => {
                popup.stage = CatchUpStage::Failed(why.message().to_owned());
                return;
            }
        };
        popup.stage = CatchUpStage::Summarising {
            run,
            messages: unread.messages,
            cut: unread.cut,
        };
        let (stop, stopped) = oneshot::channel();
        self.collect.running = Some((run, stop));
        self.collect.start = Some(catchup::Request {
            id: run,
            command,
            input: unread.text,
            stop: stopped,
        });
    }

    /// A summary command to start, once. Also the place where a command whose popup
    /// is gone (closed, or another one took its place) is stopped: the event loop
    /// asks here on every turn.
    pub fn take_summary_start(&mut self) -> Option<catchup::Request> {
        let wanted = match (&self.modal, &self.collect.running) {
            (Some(Modal::Collect(Popup::CatchUp(popup))), Some((running, _))) => {
                matches!(popup.stage, CatchUpStage::Summarising { run, .. } if run == *running)
            }
            (_, None) => true,
            _ => false,
        };
        if !wanted {
            self.stop_summary();
        }
        self.collect.start.take()
    }

    /// Stops the summary command, if one runs or is about to.
    pub(super) fn stop_summary(&mut self) {
        self.collect.start = None;
        // Dropped: the task sees its end of it close, and kills the command.
        self.collect.running = None;
    }

    /// The summary command has ended.
    pub fn on_summary(&mut self, done: catchup::Done) {
        self.dirty = true;
        if !matches!(&self.collect.running, Some((running, _)) if *running == done.id) {
            return;
        }
        self.collect.running = None;
        let Some(Modal::Collect(Popup::CatchUp(popup))) = &mut self.modal else {
            return;
        };
        let CatchUpStage::Summarising { run, messages, cut } = popup.stage else {
            return;
        };
        if run != done.id {
            return;
        }
        popup.stage = match done.outcome {
            Outcome::Summary(summary) => CatchUpStage::Done {
                summary,
                messages,
                cut,
            },
            Outcome::Failed(failure) => CatchUpStage::Failed(failure.message().to_owned()),
            Outcome::Stopped => CatchUpStage::Failed("Stopped".to_owned()),
        };
    }

    // ----- both ---------------------------------------------------------------------

    pub(super) fn on_collect(&mut self, event: CollectEvent) {
        match event {
            CollectEvent::Progress { request, loaded } => match &mut self.modal {
                Some(Modal::Collect(Popup::Archive(ArchivePopup {
                    stage:
                        ArchiveStage::Running {
                            request: asked,
                            loaded: shown,
                        },
                    ..
                })))
                | Some(Modal::Collect(Popup::CatchUp(CatchUpPopup {
                    stage:
                        CatchUpStage::Collecting {
                            request: asked,
                            loaded: shown,
                        },
                    ..
                }))) if *asked == request => *shown = loaded,
                _ => {}
            },
            CollectEvent::Archived { request, result } => self.on_archived(request, result),
            CollectEvent::Unread { request, result } => self.on_unread_text(request, result),
        }
    }

    /// Esc, or a click on Cancel or Close: what runs for the popup is given up, and
    /// the popup goes.
    pub(super) fn close_collect(&mut self) {
        let Some(Modal::Collect(popup)) = self.modal.take() else {
            return;
        };
        match popup {
            Popup::Archive(ArchivePopup {
                stage: ArchiveStage::Running { .. },
                ..
            }) => {
                self.send_matrix(MatrixCmd::Collect(CollectCmd::Cancel));
                // Its answer, should it be on its way already, is nobody's.
                self.next_collect_request();
                self.show_toast("The archive was given up; no file was written");
            }
            Popup::CatchUp(CatchUpPopup {
                stage: CatchUpStage::Collecting { .. },
                ..
            }) => self.send_matrix(MatrixCmd::Collect(CollectCmd::Cancel)),
            Popup::CatchUp(CatchUpPopup {
                stage: CatchUpStage::Summarising { .. },
                ..
            }) => self.stop_summary(),
            Popup::Archive(_) | Popup::CatchUp(_) => {}
        }
    }

    pub(super) fn on_collect_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let plain = !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let Some(Modal::Collect(popup)) = &mut self.modal else {
            return;
        };
        match popup {
            Popup::Archive(popup) => match (&popup.stage, code) {
                (_, KeyCode::Esc) => self.close_collect(),
                (ArchiveStage::Asking, KeyCode::Up | KeyCode::BackTab) => {
                    popup.selected = popup.selected.saturating_sub(1);
                }
                (ArchiveStage::Asking, KeyCode::Down | KeyCode::Tab) => {
                    popup.selected = (popup.selected + 1).min(Span::ALL.len() - 1);
                }
                (ArchiveStage::Asking, KeyCode::Enter) => self.start_archive(),
                (ArchiveStage::Done(_) | ArchiveStage::Failed(_), KeyCode::Enter) => {
                    self.close_collect();
                }
                _ => {}
            },
            Popup::CatchUp(popup) => {
                let page = popup.page.saturating_sub(1).max(1) as isize;
                match code {
                    KeyCode::Esc => self.close_collect(),
                    KeyCode::Enter
                        if !matches!(
                            popup.stage,
                            CatchUpStage::Collecting { .. } | CatchUpStage::Summarising { .. }
                        ) =>
                    {
                        self.close_collect();
                    }
                    KeyCode::Up => self.scroll_collect(-1),
                    KeyCode::Char('k') if plain => self.scroll_collect(-1),
                    KeyCode::Down => self.scroll_collect(1),
                    KeyCode::Char('j') if plain => self.scroll_collect(1),
                    KeyCode::PageUp => self.scroll_collect(-page),
                    KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_collect(page),
                    KeyCode::Home => popup.scroll = 0,
                    // Brought back to the last page when drawn.
                    KeyCode::End => popup.scroll = usize::MAX,
                    KeyCode::Char('c') if plain => self.copy_summary(),
                    _ => {}
                }
            }
        }
    }

    pub(super) fn scroll_collect(&mut self, lines: isize) {
        if let Some(Modal::Collect(Popup::CatchUp(popup))) = &mut self.modal {
            popup.scroll = popup.scroll.saturating_add_signed(lines);
        }
    }

    /// A click in the popup: on a row of the "how far back" list, or on a button.
    pub(super) fn collect_click(&mut self, what: u8) {
        let Some(Modal::Collect(popup)) = &mut self.modal else {
            return;
        };
        match (popup, what) {
            (_, CLICK_CLOSE) => self.close_collect(),
            (Popup::Archive(popup), CLICK_GO) => match popup.stage {
                ArchiveStage::Asking => self.start_archive(),
                _ => self.close_collect(),
            },
            (Popup::Archive(popup), row)
                if popup.stage == ArchiveStage::Asking && usize::from(row) < Span::ALL.len() =>
            {
                popup.selected = usize::from(row);
            }
            (Popup::CatchUp(_), CLICK_COPY) => self.copy_summary(),
            _ => {}
        }
    }

    /// The summary to the clipboard, the way every copy goes (through the terminal).
    fn copy_summary(&mut self) {
        let Some(Modal::Collect(Popup::CatchUp(popup))) = &self.modal else {
            return;
        };
        let CatchUpStage::Done { summary, .. } = &popup.stage else {
            return;
        };
        self.pending_copy = Some(summary.clone());
        self.show_toast("Copied the summary");
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyEvent};
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::action::{Action, Pane};
    use crate::app::ui;
    use crate::matrix::MatrixHandle;
    use crate::model::{Room, RoomKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn app() -> (App, UnboundedReceiver<MatrixCmd>) {
        let mut app = App::demo();
        let (handle, commands) = MatrixHandle::for_test();
        app.matrix = Some(handle);
        let mut other = Room::new("!b:example.org", "Beta", RoomKind::Room);
        other.unread = 3;
        app.rooms = vec![
            Room::new("!a:example.org", "Alpha Plans", RoomKind::Room),
            other,
        ];
        app.active = 0;
        app.focus = Pane::Timeline;
        app.download_dir = "/tmp/matrix-rat-never-written".to_owned();
        (app, commands)
    }

    fn asked(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(command) = commands.try_recv() {
            out.push(command.describe());
        }
        out
    }

    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        format!("{}", terminal.backend())
    }

    /// The screen without the line breaks a narrow popup puts into its sentences.
    fn flat(screen: &str) -> String {
        screen
            .lines()
            .map(|line| line.trim_matches(|c: char| c == '"' || c == '│' || c.is_whitespace()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// A summary command that is never started: `sh` is on every machine the tests
    /// run on, and only looked up.
    fn with_command(app: &mut App) {
        app.collect.summary_setting = "/bin/sh -c 'never run'".to_owned();
        assert!(app.summary_command().is_ok());
    }

    fn archive_stage(app: &App) -> &ArchiveStage {
        match &app.modal {
            Some(Modal::Collect(Popup::Archive(popup))) => &popup.stage,
            other => panic!("no archive popup: {other:?}"),
        }
    }

    fn catch_up_stage(app: &App) -> &CatchUpStage {
        match &app.modal {
            Some(Modal::Collect(Popup::CatchUp(popup))) => &popup.stage,
            other => panic!("no catch-up popup: {other:?}"),
        }
    }

    #[test]
    fn the_archive_asks_first_and_warns_that_it_is_text_only() {
        let (mut app, mut commands) = app();
        app.perform(Action::ArchiveRoom);
        assert_eq!(*archive_stage(&app), ArchiveStage::Asking);
        // Nothing is asked of the worker before the user says so.
        assert!(asked(&mut commands).is_empty());
        let shown = flat(&screen(&mut app));
        assert!(shown.contains("Archive Alpha Plans"), "{shown}");
        for row in [
            "What is loaded now",
            "The last 7 days",
            "The last 30 days",
            "Everything",
        ] {
            assert!(shown.contains(row), "{row} in {shown}");
        }
        assert!(shown.contains("Text only"), "{shown}");
        assert!(shown.contains("not included"), "{shown}");
        assert!(shown.contains("/tmp/matrix-rat-never-written"), "{shown}");
        assert!(shown.contains("never overwritten"), "{shown}");

        // Esc: nothing happened.
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert!(asked(&mut commands).is_empty());
    }

    #[test]
    fn enter_starts_the_archive_of_the_chosen_stretch_and_shows_how_far_it_is() {
        let (mut app, mut commands) = app();
        app.perform(Action::ArchiveRoom);
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(
            asked(&mut commands),
            ["Collect(Archive(request 1, !a:example.org, Days(7)))"]
        );
        // A second Enter starts nothing more.
        app.on_event(key(KeyCode::Enter));
        assert!(asked(&mut commands).is_empty());

        app.on_matrix(crate::matrix::MatrixEvent::Collect(
            CollectEvent::Progress {
                request: 1,
                loaded: 300,
            },
        ));
        let shown = flat(&screen(&mut app));
        assert!(shown.contains("300"), "{shown}");
        assert!(shown.contains("Esc"), "{shown}");

        app.on_matrix(crate::matrix::MatrixEvent::Collect(
            CollectEvent::Archived {
                request: 1,
                result: Ok(Archived {
                    path: "/tmp/matrix-rat-never-written/Alpha-Plans-2026-11-20.md".into(),
                    messages: 280,
                    cut: Some(Cut::Limit),
                }),
            },
        ));
        let shown = flat(&screen(&mut app));
        assert!(shown.contains("280 messages"), "{shown}");
        assert!(shown.contains("Alpha-Plans-2026-11-20.md"), "{shown}");
        assert!(shown.contains("The room has more"), "{shown}");
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
    }

    #[test]
    fn esc_gives_a_running_archive_up_and_a_late_answer_is_dropped() {
        let (mut app, mut commands) = app();
        app.perform(Action::ArchiveRoom);
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert_eq!(
            asked(&mut commands),
            [
                "Collect(Archive(request 1, !a:example.org, Loaded))",
                "Collect(Cancel)"
            ]
        );
        app.toast = None;
        app.on_matrix(crate::matrix::MatrixEvent::Collect(
            CollectEvent::Archived {
                request: 1,
                result: Err("late".into()),
            },
        ));
        assert!(app.toast.is_none());
    }

    #[test]
    fn an_archive_whose_popup_made_way_still_tells_its_end_and_a_failure_is_shown() {
        let (mut app, _commands) = app();
        app.perform(Action::ArchiveRoom);
        app.on_event(key(KeyCode::Enter));
        app.modal = None;
        app.on_matrix(crate::matrix::MatrixEvent::Collect(
            CollectEvent::Archived {
                request: 1,
                result: Ok(Archived {
                    path: "/tmp/matrix-rat-never-written/Alpha.md".into(),
                    messages: 1,
                    cut: None,
                }),
            },
        ));
        let toast = app.toast.clone().unwrap().0;
        assert_eq!(
            toast,
            "Archived 1 message to /tmp/matrix-rat-never-written/Alpha.md"
        );

        app.perform(Action::ArchiveRoom);
        app.on_event(key(KeyCode::Enter));
        app.on_matrix(crate::matrix::MatrixEvent::Collect(
            CollectEvent::Archived {
                request: 2,
                result: Err("could not write the file: no space".into()),
            },
        ));
        assert!(matches!(archive_stage(&app), ArchiveStage::Failed(_)));
        assert!(flat(&screen(&mut app)).contains("no space"));
    }

    #[test]
    fn the_archive_is_of_the_room_under_the_cursor_in_the_room_list() {
        let (mut app, mut commands) = app();
        app.focus = Pane::Sidebar;
        app.sidebar.cursor = 1;
        let target = app.rooms[app.cursor_room().unwrap()].id.clone();
        app.perform(Action::ArchiveRoom);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(
            asked(&mut commands),
            [format!("Collect(Archive(request 1, {target}, Loaded))")]
        );
    }

    #[test]
    fn an_invitation_and_the_demo_have_nothing_to_archive_or_catch_up_on() {
        let (mut app, mut commands) = app();
        with_command(&mut app);
        app.rooms[0].kind = RoomKind::Invite;
        app.perform(Action::ArchiveRoom);
        app.perform(Action::CatchUp);
        assert!(app.modal.is_none());
        assert!(asked(&mut commands).is_empty());

        let mut demo = App::demo();
        with_command(&mut demo);
        demo.perform(Action::ArchiveRoom);
        assert!(demo.modal.is_none());
        assert_eq!(demo.toast.clone().unwrap().0, "Not connected to an account");
    }

    #[test]
    fn catching_up_is_not_offered_without_a_summary_command() {
        let (mut app, mut commands) = app();
        app.focus = Pane::Sidebar;
        app.sidebar.cursor = 1;
        app.perform(Action::CatchUp);
        assert!(app.modal.is_none());
        assert!(asked(&mut commands).is_empty());
        assert!(app.toast.clone().unwrap().0.contains("summary_command"));

        app.collect.summary_setting = "no-such-summary-program-anywhere".to_owned();
        app.perform(Action::CatchUp);
        assert!(app.modal.is_none());
        assert!(app.toast.clone().unwrap().0.contains("was not found"));
        assert!(asked(&mut commands).is_empty());
    }

    #[test]
    fn nothing_is_collected_where_nothing_is_unread() {
        let (mut app, mut commands) = app();
        with_command(&mut app);
        app.perform(Action::CatchUp);
        assert!(app.modal.is_none());
        assert_eq!(
            app.toast.clone().unwrap().0,
            "Nothing is unread in Alpha Plans"
        );
        assert!(asked(&mut commands).is_empty());
    }

    /// Opens the catch-up of the unread room under the cursor.
    fn catching_up() -> (App, UnboundedReceiver<MatrixCmd>) {
        let (mut app, mut commands) = app();
        with_command(&mut app);
        app.focus = Pane::Sidebar;
        let at = app
            .visible_rooms()
            .iter()
            .position(|index| app.rooms[*index].id == "!b:example.org")
            .unwrap();
        app.sidebar.cursor = at;
        app.perform(Action::CatchUp);
        assert_eq!(
            asked(&mut commands),
            ["Collect(Unread(request 1, !b:example.org, from: false))"]
        );
        (app, commands)
    }

    fn unread(app: &mut App, request: u64, messages: usize, cut: bool) {
        app.on_matrix(crate::matrix::MatrixEvent::Collect(CollectEvent::Unread {
            request,
            result: Ok(UnreadText {
                text: "[2026-11-20 14:03] Ann: the secret plan\n".repeat(messages),
                messages,
                cut,
            }),
        }));
    }

    #[test]
    fn the_unread_text_goes_to_the_command_and_the_summary_is_shown_to_the_user_only() {
        let (mut app, mut commands) = catching_up();
        assert!(matches!(
            catch_up_stage(&app),
            CatchUpStage::Collecting { .. }
        ));
        // Nothing is started before the text is there.
        assert!(app.take_summary_start().is_none());
        let shown = flat(&screen(&mut app));
        assert!(shown.contains("Catch up on Beta"), "{shown}");
        assert!(shown.contains("sh"), "{shown}");

        unread(&mut app, 1, 2, true);
        let request = app
            .take_summary_start()
            .expect("the command is to be started");
        assert_eq!(request.input.lines().count(), 2);
        assert!(app.take_summary_start().is_none(), "once");
        assert!(flat(&screen(&mut app)).contains("2 messages"));

        app.on_summary(catchup::Done {
            id: request.id,
            outcome: Outcome::Summary("Ann has a plan.\nBob agrees.".to_owned()),
        });
        let shown = flat(&screen(&mut app));
        assert!(shown.contains("Ann has a plan."), "{shown}");
        assert!(shown.contains("Bob agrees."), "{shown}");
        assert!(shown.contains("newest 2 messages"), "{shown}");
        assert!(shown.contains("Only you see this"), "{shown}");
        // Nothing was sent to the room, or anywhere.
        assert!(asked(&mut commands).is_empty());

        app.on_event(key(KeyCode::Char('c')));
        assert_eq!(
            app.pending_copy.as_deref(),
            Some("Ann has a plan.\nBob agrees.")
        );
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
    }

    #[test]
    fn closing_the_popup_stops_the_command_and_its_answer_is_dropped() {
        let (mut app, mut commands) = catching_up();
        unread(&mut app, 1, 1, false);
        let mut request = app.take_summary_start().unwrap();
        assert!(request.stop.try_recv().is_err(), "still wanted");
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        // Its end of the channel is closed: the task kills the command.
        assert_eq!(
            request.stop.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        );
        app.on_summary(catchup::Done {
            id: request.id,
            outcome: Outcome::Summary("late".to_owned()),
        });
        assert!(app.modal.is_none());
        assert!(asked(&mut commands).is_empty());
    }

    #[test]
    fn a_command_whose_popup_made_way_is_stopped_too() {
        let (mut app, _commands) = catching_up();
        unread(&mut app, 1, 1, false);
        let mut request = app.take_summary_start().unwrap();
        // Another popup takes the place.
        app.modal = None;
        app.perform(Action::OpenSettings);
        assert!(app.take_summary_start().is_none());
        assert_eq!(
            request.stop.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        );
    }

    #[test]
    fn giving_up_while_the_messages_load_cancels_the_collection() {
        let (mut app, mut commands) = catching_up();
        app.on_event(key(KeyCode::Esc));
        assert_eq!(asked(&mut commands), ["Collect(Cancel)"]);
        unread(&mut app, 1, 1, false);
        assert!(app.take_summary_start().is_none());
    }

    #[test]
    fn nothing_unread_a_failed_load_and_a_failed_command_say_so() {
        let (mut app, _commands) = catching_up();
        unread(&mut app, 1, 0, false);
        assert_eq!(*catch_up_stage(&app), CatchUpStage::Nothing);
        assert!(app.take_summary_start().is_none());
        assert!(flat(&screen(&mut app)).contains("Nothing is unread"));

        let (mut app, _commands) = catching_up();
        app.on_matrix(crate::matrix::MatrixEvent::Collect(CollectEvent::Unread {
            request: 1,
            result: Err("the account is not in this room".into()),
        }));
        assert!(flat(&screen(&mut app)).contains("not in this room"));
        assert!(app.take_summary_start().is_none());

        let (mut app, _commands) = catching_up();
        unread(&mut app, 1, 1, false);
        let request = app.take_summary_start().unwrap();
        app.on_summary(catchup::Done {
            id: request.id,
            outcome: Outcome::Failed(catchup::Failure::TimedOut),
        });
        assert!(flat(&screen(&mut app)).contains("took too long"));
    }

    #[test]
    fn a_long_summary_scrolls() {
        let (mut app, _commands) = catching_up();
        unread(&mut app, 1, 1, false);
        let request = app.take_summary_start().unwrap();
        let summary: Vec<String> = (1..=60).map(|n| format!("point {n}")).collect();
        app.on_summary(catchup::Done {
            id: request.id,
            outcome: Outcome::Summary(summary.join("\n")),
        });
        let shown = screen(&mut app);
        assert!(
            shown.contains("point 1") && !shown.contains("point 60"),
            "{shown}"
        );
        app.on_event(key(KeyCode::End));
        let shown = screen(&mut app);
        assert!(
            shown.contains("point 60") && !shown.contains("point 1 "),
            "{shown}"
        );
        app.on_event(key(KeyCode::Home));
        app.on_event(key(KeyCode::PageDown));
        assert!(!screen(&mut app).contains("point 1 "));
    }

    #[test]
    fn the_open_room_is_caught_up_on_from_its_new_line() {
        let (mut app, mut commands) = app();
        with_command(&mut app);
        // As after opening an unread room: the line stands above this message.
        app.set_unread_line_for_test("$first-unread:example.org");
        app.perform(Action::CatchUp);
        assert_eq!(
            asked(&mut commands),
            ["Collect(Unread(request 1, !a:example.org, from: true))"]
        );
    }

    #[test]
    fn the_setting_is_kept_and_says_what_it_found() {
        let (mut app, _commands) = app();
        assert_eq!(app.summary_setting_text(), "not set");
        app.set_summary_command("  /bin/sh -c cat ");
        assert_eq!(app.collect.summary_setting, "/bin/sh -c cat");
        assert_eq!(app.summary_setting_text(), "/bin/sh -c cat");
        assert!(app.toast.clone().unwrap().0.contains("to sh"));
        app.set_summary_command("no-such-summary-program-anywhere");
        assert!(app.summary_setting_text().ends_with("(not found)"));
        app.set_summary_command("");
        assert_eq!(app.summary_setting_text(), "not set");
        assert!(app.toast.clone().unwrap().0.contains("off"));
    }

    #[test]
    fn the_debug_output_of_the_popups_and_the_state_holds_nothing_private() {
        let (mut app, _commands) = catching_up();
        app.collect.summary_setting = "/home/alice/bin/summarise --key=hunter2".to_owned();
        unread(&mut app, 1, 1, false);
        let mut shown = vec![format!("{app:?}")];
        let request = app.take_summary_start();
        if let Some(request) = &request {
            shown.push(format!("{request:?}"));
            app.on_summary(catchup::Done {
                id: request.id,
                outcome: Outcome::Summary("the secret summary".to_owned()),
            });
        }
        shown.push(format!("{app:?}"));
        app.modal = None;
        app.focus = Pane::Timeline;
        app.perform(Action::ArchiveRoom);
        shown.push(format!("{app:?}"));
        let all = shown.join("\n");
        assert!(
            all.contains("Collect(CatchUp(") && all.contains("Collect(Archive("),
            "{all}"
        );
        for private in [
            "alice",
            "hunter2",
            "secret",
            "Beta",
            "Alpha",
            "never-written",
            "example.org",
        ] {
            assert!(!all.contains(private), "{private} in {all}");
        }
    }
}
