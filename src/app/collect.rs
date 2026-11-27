//! Archiving a room as Markdown and catching up on what is unread (or on the last
//! messages, `/catchup N`), on `App`: the popup of each, what is asked of the worker,
//! the summary command that is started, and what comes back.
//!
//! Both are about the room that room-level commands are about (the open one, or in
//! the room list the one under the cursor). The worker collects the messages
//! (`crate::matrix::collect`); for an archive it also writes the file, for a summary
//! the text comes here and is handed to the user's command (`crate::catchup`).

use std::path::PathBuf;

use tokio::sync::oneshot;

use super::App;
use crate::catchup::{self, NoCommand, Outcome};
use crate::collect::{Cut, SUMMARY_MESSAGES, Span};
use crate::files;
use crate::input::{KeyCode, KeyModifiers};
use crate::matrix::{ARCHIVE_EVENTS, Archived, CollectCmd, CollectEvent, MatrixCmd, UnreadText};
use crate::modal::Modal;
use crate::model::MsgKind;
use crate::probes::Probes;

/// What a click in the popup is on (see `HitId::Collect`): a row of the "how far
/// back" list by its place, or one of these.
pub const CLICK_GO: u8 = 10;
pub const CLICK_CLOSE: u8 = 11;
pub const CLICK_COPY: u8 = 12;

/// How many of the newest messages are offered for a summary where it is not known
/// what is unread, and the room does not count any as unread either.
const NEWEST: usize = 50;

/// What the app keeps for both features between events.
pub struct CollectState {
    /// Counts what is asked of the worker and of the summary command.
    requests: u64,
    /// The `summary_command` setting as written.
    summary_setting: String,
    /// What that setting comes to here. Looked up when it is set and when a summary
    /// is asked for, not on every draw: it means looking through `PATH`.
    command: Result<catchup::Command, NoCommand>,
    /// A summary command to start, for the event loop to take.
    start: Option<catchup::Request>,
    /// The way to stop the summary command that runs, and its number.
    running: Option<(u64, oneshot::Sender<()>)>,
    /// The archive the worker is at, by its request: also when its popup is gone.
    archive: Option<u64>,
}

impl Default for CollectState {
    fn default() -> Self {
        Self {
            requests: 0,
            summary_setting: String::new(),
            command: Err(NoCommand::NotSet),
            start: None,
            running: None,
            archive: None,
        }
    }
}

/// Hand-written: the setting is a command line of the user's.
impl std::fmt::Debug for CollectState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollectState")
            .field("requests", &self.requests)
            .field("summary_set", &!self.summary_setting.is_empty())
            .field("command", &self.command.is_ok())
            .field("start", &self.start.is_some())
            .field("running", &self.running.is_some())
            .field("archive", &self.archive.is_some())
            .finish()
    }
}

impl CollectState {
    /// The setting as written, for the settings to edit.
    pub(super) fn summary_setting(&self) -> &str {
        &self.summary_setting
    }

    /// Takes the setting and looks its program up.
    pub(super) fn set_summary_setting(&mut self, setting: &str, probes: &dyn Probes) {
        self.summary_setting = setting.trim().to_owned();
        self.look_up(probes);
    }

    fn look_up(&mut self, probes: &dyn Probes) {
        self.command = probes.summary(&self.summary_setting);
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
    /// How many messages the app holds of the room, if it is the open one: only
    /// then is "what is loaded now" something the user sees, and offered.
    pub held: Option<usize>,
    /// The chosen row of [`ArchivePopup::spans`].
    pub selected: usize,
    /// Where the file goes.
    pub dir: PathBuf,
    /// The name it gets there if that is free.
    pub file_name: String,
    pub stage: ArchiveStage,
}

impl ArchivePopup {
    /// The rows of "how far back".
    pub fn spans(&self) -> &'static [Span] {
        match self.held {
            Some(_) => &Span::ALL,
            None => &Span::ALL[1..],
        }
    }

    pub fn span(&self) -> Span {
        let spans = self.spans();
        spans[self.selected.min(spans.len() - 1)]
    }
}

/// What the messages given to the summary command are, for the popup to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Of {
    /// Unread ones.
    Unread,
    /// The newest, read or not: where the user stopped reading was not found.
    Newest,
    /// The last ones, read or not, as many as were `asked` for (`/catchup N`) or, if
    /// fewer, as there were: `stopped` says whether the room has no more
    /// (`None`) or loading ended before.
    Last { asked: usize, stopped: Option<Cut> },
}

/// How far catching up is.
#[derive(Clone, PartialEq, Eq)]
pub enum CatchUpStage {
    /// The worker collects what is unread, or the last messages.
    Collecting {
        request: u64,
        loaded: usize,
    },
    /// Where the user stopped reading was not found: nothing is given to the
    /// command unless the user says so here. Holds the newest messages as text.
    Asking {
        text: String,
        messages: usize,
    },
    /// The user's command has the text. `cut`: there were more than a summary
    /// holds, and these are the newest.
    Summarising {
        run: u64,
        messages: usize,
        cut: bool,
        of: Of,
    },
    /// The summary, for the user alone.
    Done {
        summary: String,
        messages: usize,
        cut: bool,
        of: Of,
    },
    /// Nothing is unread (or, of the last messages, none was found, back to the
    /// room's first message).
    Nothing,
    /// Of the last messages, none was found in the newest
    /// [`LAST_EVENTS`](crate::matrix::LAST_EVENTS) events: loading ended there,
    /// before it saw the room's first message.
    NothingInNewest,
    Failed(String),
}

/// The popup of a summary.
#[derive(Clone)]
pub struct CatchUpPopup {
    pub room_name: String,
    /// The name of the program that summarises, for the user to see where the text
    /// goes.
    pub program: String,
    /// How many of the last messages were asked for (`/catchup N`); without it,
    /// what is unread.
    pub last: Option<usize>,
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

/// Hand-written: the popups hold a room's name, a folder, what was said in the room
/// and a summary of it. Only what they are at is shown.
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
                    CatchUpStage::Asking { .. } => "Asking",
                    CatchUpStage::Summarising { .. } => "Summarising",
                    CatchUpStage::Done { .. } => "Done",
                    CatchUpStage::Nothing => "Nothing",
                    CatchUpStage::NothingInNewest => "NothingInNewest",
                    CatchUpStage::Failed(_) => "Failed",
                },
            ),
        };
        write!(f, "{name}({stage})")
    }
}

/// "1 message", "3 messages", "5,000 messages"; with `what` between ("3 unread
/// messages").
pub fn count(messages: usize, what: &str) -> String {
    let number = thousands(messages);
    let gap = if what.is_empty() { "" } else { " " };
    if messages == 1 {
        format!("1{gap}{what} message")
    } else {
        format!("{number}{gap}{what} messages")
    }
}

/// 5000 as "5,000".
pub fn thousands(number: usize) -> String {
    let digits = number.to_string();
    let mut out = String::new();
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// What to say about an archive that is not complete.
pub fn cut_note(cut: Option<Cut>) -> Option<String> {
    match cut? {
        Cut::Limit => Some(format!(
            "Not complete: the room has more. One archive takes the newest {}.",
            count(ARCHIVE_EVENTS, "")
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
        // The worker does one collection at a time, and an archive that runs (its
        // popup may have made way) must not be ended by the next one.
        if self.collect.archive.is_some() {
            return Err("An archive is still being written: wait for it to end".to_owned());
        }
        Ok(index)
    }

    /// Whether `index` is the room whose messages are on screen (not one of its
    /// threads).
    fn shows_room(&self, index: usize) -> bool {
        self.open_index() == Some(index) && !self.thread_open()
    }

    /// The session ends: nothing of it is waited for any more.
    pub(super) fn forget_collections(&mut self) {
        self.stop_summary();
        self.collect.archive = None;
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
        let held = self.shows_room(index).then(|| {
            self.messages()
                .iter()
                .filter(|message| message.kind != MsgKind::ReadMarker)
                .count()
        });
        let room = &self.rooms[index];
        let popup = ArchivePopup {
            room_id: room.id.clone(),
            room_name: room.name.clone(),
            held,
            selected: 0,
            dir: self.download_dir(),
            file_name: crate::archive::file_name(&room.name, chrono::Local::now().date_naive()),
            stage: ArchiveStage::Asking,
        };
        self.open_modal(Modal::Collect(Popup::Archive(popup)));
    }

    /// Enter on the question: the worker starts.
    fn start_archive(&mut self) {
        if !matches!(
            &self.modal,
            Some(Modal::Collect(Popup::Archive(ArchivePopup {
                stage: ArchiveStage::Asking,
                ..
            })))
        ) {
            return;
        }
        let request = self.next_collect_request();
        let Some(Modal::Collect(Popup::Archive(popup))) = &mut self.modal else {
            return;
        };
        popup.stage = ArchiveStage::Running { request, loaded: 0 };
        let command = CollectCmd::Archive {
            request,
            room_id: popup.room_id.clone(),
            span: popup.span(),
            held: popup.held.unwrap_or(0),
            dir: popup.dir.clone(),
            room_name: popup.room_name.clone(),
        };
        self.collect.archive = Some(request);
        self.send_matrix(MatrixCmd::Collect(command));
    }

    fn on_archived(&mut self, request: u64, result: Result<Archived, String>) {
        // Not ours: one that was given up, whose answer was on its way already.
        let ours = self.collect.archive == Some(request);
        if ours {
            self.collect.archive = None;
        }
        if let Some(Modal::Collect(Popup::Archive(popup))) = &mut self.modal
            && matches!(popup.stage, ArchiveStage::Running { request: asked, .. } if asked == request)
        {
            popup.stage = match result {
                Ok(archived) => ArchiveStage::Done(archived),
                Err(why) => ArchiveStage::Failed(why),
            };
            return;
        }
        // Its popup is gone. A file that was written is there, given up or not, and
        // the user is told where; a failure only of an archive that is still wanted.
        match result {
            Ok(archived) => self.show_toast(format!(
                "Archived {} to {}",
                count(archived.messages, ""),
                files::display(&archived.path)
            )),
            Err(why) if ours => self.show_toast(format!("Could not archive the room: {why}")),
            Err(_) => {}
        }
    }

    // ----- catching up -------------------------------------------------------------

    /// The `summary_command` setting as a command, or why there is none: as it was
    /// found when last looked up.
    pub(super) fn summary_command(&self) -> Result<catchup::Command, NoCommand> {
        self.collect.command.clone()
    }

    /// What the settings show for the summary command.
    pub(super) fn summary_setting_text(&self) -> String {
        let setting = self.collect.summary_setting();
        match &self.collect.command {
            Err(NoCommand::NotSet) => "not set".to_owned(),
            Ok(_) => setting.to_owned(),
            Err(_) => format!("{setting} (not found)"),
        }
    }

    /// Enter on the summary command row: the setting is kept, and looked up at once.
    pub(super) fn set_summary_command(&mut self, text: &str) {
        self.collect.set_summary_setting(text, self.probes.as_ref());
        self.save_setting("summary_command", self.collect.summary_setting().to_owned());
        let said = match self.summary_command() {
            Ok(command) => format!(
                "Catching up gives the unread text, or the last N (/catchup N), to {}",
                command.name()
            ),
            Err(NoCommand::NotSet) => "Catching up is off: no summary command".to_owned(),
            Err(why) => why.message().to_owned(),
        };
        self.show_toast(said);
    }

    /// Collects what is unread in the room and has the user's command summarise it,
    /// in a popup. Not without a command; nothing is asked for where nothing is
    /// unread.
    pub(super) fn open_catch_up(&mut self) {
        let Some((command, index)) = self.catch_up_target() else {
            return;
        };
        // The open room counts as read once it is looked at; its "new" line still
        // says from where it was unread.
        let from = self
            .shows_room(index)
            .then(|| self.first_unread_event().map(str::to_owned))
            .flatten();
        let room = &self.rooms[index];
        if from.is_none() && !room.shows_unread() {
            return self.show_toast(format!("Nothing is unread in {}", room.name));
        }
        let room_id = room.id.clone();
        // Should where reading stopped not be found: as many as the room counts as
        // unread, or a handful.
        let newest = match usize::try_from(room.unread).unwrap_or(0) {
            0 => NEWEST,
            unread => unread.min(SUMMARY_MESSAGES),
        };
        let request = self.next_collect_request();
        self.show_collecting(&command, index, None, request);
        self.send_matrix(MatrixCmd::Collect(CollectCmd::Unread {
            request,
            room_id,
            from,
            newest,
        }));
    }

    /// `/catchup N`: has the user's command summarise the last `count` messages of
    /// the room, read or not, in a popup; also where nothing is unread. Not without
    /// a command.
    pub(super) fn open_catch_up_last(&mut self, count: usize) {
        let Some((command, index)) = self.catch_up_target() else {
            return;
        };
        let room_id = self.rooms[index].id.clone();
        let request = self.next_collect_request();
        self.show_collecting(&command, index, Some(count), request);
        self.send_matrix(MatrixCmd::Collect(CollectCmd::Last {
            request,
            room_id,
            count,
        }));
    }

    /// The summary command and the room to catch up on; `None` once the user has
    /// been told why there is none.
    fn catch_up_target(&mut self) -> Option<(catchup::Command, usize)> {
        // The program may have come or gone since the setting was made.
        self.collect.look_up(self.probes.as_ref());
        let command = match self.summary_command() {
            Ok(command) => command,
            Err(why) => {
                self.show_toast(why.message());
                return None;
            }
        };
        match self.collect_target() {
            Ok(index) => Some((command, index)),
            Err(why) => {
                self.show_toast(why);
                None
            }
        }
    }

    /// The popup of catching up on the room at `index`, while the worker collects
    /// for `request`; `last` as in [`CatchUpPopup::last`].
    fn show_collecting(
        &mut self,
        command: &catchup::Command,
        index: usize,
        last: Option<usize>,
        request: u64,
    ) {
        self.stop_summary();
        let room_name = self.rooms[index].name.clone();
        self.open_modal(Modal::Collect(Popup::CatchUp(CatchUpPopup {
            room_name,
            program: command.name(),
            last,
            stage: CatchUpStage::Collecting { request, loaded: 0 },
            scroll: 0,
            page: 1,
        })));
    }

    fn on_unread_text(&mut self, request: u64, result: Result<UnreadText, String>) {
        let Some(Modal::Collect(Popup::CatchUp(popup))) = &mut self.modal else {
            return;
        };
        if !matches!(popup.stage, CatchUpStage::Collecting { request: asked, .. } if asked == request)
        {
            return;
        }
        match result {
            Err(why) => {
                popup.stage = CatchUpStage::Failed(format!("Could not load the messages: {why}"));
            }
            Ok(text) if text.messages == 0 => {
                popup.stage = match (popup.last, text.stopped) {
                    // Of the last messages, none is no answer where loading ended
                    // before the room's first message.
                    (Some(_), Some(Cut::LoadFailed)) => {
                        CatchUpStage::Failed("Could not load the messages.".to_owned())
                    }
                    (Some(_), Some(Cut::Limit)) => CatchUpStage::NothingInNewest,
                    (Some(_), None) | (None, _) => CatchUpStage::Nothing,
                }
            }
            Ok(text) => match popup.last {
                // The last messages, read or not, are what was asked for.
                Some(asked) => {
                    let of = Of::Last {
                        asked,
                        stopped: text.stopped,
                    };
                    self.start_summary(text.text, text.messages, text.cut, of);
                }
                // Not known what is unread: the text stays here until the user has
                // said that the newest messages may go to the command.
                None if !text.known => {
                    popup.stage = CatchUpStage::Asking {
                        text: text.text,
                        messages: text.messages,
                    };
                }
                None => self.start_summary(text.text, text.messages, text.cut, Of::Unread),
            },
        }
    }

    /// Enter on the question whether the newest messages are to be summarised.
    fn confirm_catch_up(&mut self) {
        let Some(Modal::Collect(Popup::CatchUp(popup))) = &mut self.modal else {
            return;
        };
        let CatchUpStage::Asking { text, messages } = &mut popup.stage else {
            return;
        };
        let (text, messages) = (std::mem::take(text), *messages);
        self.start_summary(text, messages, false, Of::Newest);
    }

    /// Hands `text` to the summary command, for the popup that is open.
    fn start_summary(&mut self, text: String, messages: usize, cut: bool, of: Of) {
        let command = self.summary_command();
        let Some(Modal::Collect(Popup::CatchUp(popup))) = &mut self.modal else {
            return;
        };
        let command = match command {
            Ok(command) => command,
            Err(why) => {
                popup.stage = CatchUpStage::Failed(why.message().to_owned());
                return;
            }
        };
        // Counted only now that a command really starts.
        self.collect.requests = self.collect.requests.wrapping_add(1);
        let run = self.collect.requests;
        popup.stage = CatchUpStage::Summarising {
            run,
            messages,
            cut,
            of,
        };
        let (stop, stopped) = oneshot::channel();
        self.collect.running = Some((run, stop));
        self.collect.start = Some(catchup::Request {
            id: run,
            command,
            input: text,
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
        let CatchUpStage::Summarising {
            run,
            messages,
            cut,
            of,
        } = popup.stage
        else {
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
                of,
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
                // Its answer, should it be on its way already, is of no archive
                // that is waited for (a file that was written is still told).
                self.collect.archive = None;
                self.show_toast("The archive was given up");
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
                    popup.selected = (popup.selected + 1).min(popup.spans().len() - 1);
                }
                (ArchiveStage::Asking, KeyCode::Enter) => self.start_archive(),
                (ArchiveStage::Done(_) | ArchiveStage::Failed(_), KeyCode::Enter) => {
                    self.close_collect();
                }
                _ => {}
            },
            Popup::CatchUp(popup) => {
                let page = popup.page.saturating_sub(1).max(1) as isize;
                #[expect(
                    clippy::wildcard_enum_match_arm,
                    reason = "input events stand in for the terminal's (crate::input)"
                )]
                match code {
                    KeyCode::Esc => self.close_collect(),
                    KeyCode::Enter =>
                    {
                        #[warn(clippy::wildcard_enum_match_arm)]
                        match popup.stage {
                            CatchUpStage::Asking { .. } => self.confirm_catch_up(),
                            CatchUpStage::Collecting { .. } | CatchUpStage::Summarising { .. } => {}
                            CatchUpStage::Done { .. }
                            | CatchUpStage::Nothing
                            | CatchUpStage::NothingInNewest
                            | CatchUpStage::Failed(_) => self.close_collect(),
                        }
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
                ArchiveStage::Running { .. } | ArchiveStage::Done(_) | ArchiveStage::Failed(_) => {
                    self.close_collect()
                }
            },
            (Popup::Archive(popup), row)
                if popup.stage == ArchiveStage::Asking
                    && usize::from(row) < popup.spans().len() =>
            {
                popup.selected = usize::from(row);
            }
            (Popup::CatchUp(_), CLICK_GO) => self.confirm_catch_up(),
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
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::action::{Action, Pane};
    use crate::input::{InputEvent, KeyEvent};
    use crate::matrix::{LAST_EVENTS, MatrixHandle};
    use crate::model::{Room, RoomKind};
    use crate::ui;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> InputEvent {
        InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
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
        app.collect
            .set_summary_setting("/bin/sh -c 'never run'", app.probes.as_ref());
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
        assert!(*archive_stage(&app) == ArchiveStage::Asking);
        // Nothing is asked of the worker before the user says so.
        assert!(asked(&mut commands).is_empty());
        let shown = flat(&screen(&mut app));
        assert!(shown.contains("Archive Alpha Plans"), "{shown}");
        for row in [
            "What is loaded now",
            "The last 7 days",
            "The last 30 days",
            "As far back as it goes, at most 5,000 messages",
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
        assert!(shown.contains("the room has more"), "{shown}");
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
    fn a_file_that_was_written_before_the_archive_was_given_up_is_still_told() {
        let (mut app, _commands) = app();
        app.perform(Action::ArchiveRoom);
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.toast.clone().unwrap().0, "The archive was given up");
        // The worker had published the file already: it is there, so its place is said.
        app.on_matrix(crate::matrix::MatrixEvent::Collect(
            CollectEvent::Archived {
                request: 1,
                result: Ok(Archived {
                    path: "/tmp/matrix-rat-never-written/Alpha.md".into(),
                    messages: 4,
                    cut: None,
                }),
            },
        ));
        assert_eq!(
            app.toast.clone().unwrap().0,
            "Archived 4 messages to /tmp/matrix-rat-never-written/Alpha.md"
        );
        assert!(app.modal.is_none());
    }

    #[test]
    fn what_is_loaded_is_offered_for_the_open_room_only_and_says_how_much_that_is() {
        let (mut app, mut commands) = app();
        app.rooms[0].messages = vec![
            crate::model::Message::default(),
            crate::model::Message {
                kind: MsgKind::ReadMarker,
                ..Default::default()
            },
            crate::model::Message::default(),
        ];
        app.perform(Action::ArchiveRoom);
        app.on_event(key(KeyCode::Enter));
        let sent = commands.try_recv().unwrap();
        assert!(
            matches!(
                sent,
                MatrixCmd::Collect(CollectCmd::Archive {
                    span: Span::Loaded,
                    held: 2,
                    ..
                })
            ),
            "{}",
            sent.describe()
        );
        app.on_event(key(KeyCode::Esc));

        // A room that is not on screen has nothing "loaded now" to see.
        to_beta(&mut app);
        app.perform(Action::ArchiveRoom);
        let shown = flat(&screen(&mut app));
        assert!(!shown.contains("What is loaded now"), "{shown}");
        assert!(shown.contains("● The last 7 days"), "{shown}");
        for _ in 0..5 {
            app.on_event(key(KeyCode::Down));
        }
        let Some(Modal::Collect(Popup::Archive(popup))) = &app.modal else {
            panic!("no archive popup");
        };
        assert!(popup.span() == Span::Everything);
    }

    #[test]
    fn while_an_archive_runs_without_its_popup_nothing_else_is_collected() {
        let (mut app, mut commands) = app();
        with_command(&mut app);
        app.perform(Action::ArchiveRoom);
        app.on_event(key(KeyCode::Enter));
        asked(&mut commands);
        // Another popup took its place; the worker is still at it.
        app.modal = None;
        to_beta(&mut app);
        for action in [Action::CatchUp, Action::ArchiveRoom] {
            app.toast = None;
            app.perform(action);
            assert!(app.modal.is_none());
            assert!(app.toast.clone().unwrap().0.contains("still being written"));
        }
        // Nothing reached the worker, which would have ended the archive for it.
        assert!(asked(&mut commands).is_empty());
        // No request was counted for what did not start.
        app.on_matrix(crate::matrix::MatrixEvent::Collect(
            CollectEvent::Archived {
                request: 1,
                result: Err("no space".into()),
            },
        ));
        assert!(app.toast.clone().unwrap().0.contains("no space"));
        app.perform(Action::ArchiveRoom);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(
            asked(&mut commands),
            ["Collect(Archive(request 2, !b:example.org, Days(7)))"]
        );
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
        to_beta(&mut app);
        let target = app.rooms[app.cursor_room().unwrap()].id.clone();
        app.perform(Action::ArchiveRoom);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(
            asked(&mut commands),
            [format!("Collect(Archive(request 1, {target}, Days(7)))")]
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
        to_beta(&mut app);
        app.perform(Action::CatchUp);
        assert!(app.modal.is_none());
        assert!(asked(&mut commands).is_empty());
        assert!(app.toast.clone().unwrap().0.contains("summary_command"));

        app.collect
            .set_summary_setting("no-such-summary-program-anywhere", app.probes.as_ref());
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

    /// The cursor of the room list on the room that is not open.
    fn to_beta(app: &mut App) {
        app.focus = Pane::Sidebar;
        let at = app
            .visible_rooms()
            .iter()
            .position(|index| app.rooms[*index].id == "!b:example.org")
            .unwrap();
        app.sidebar.cursor = at;
    }

    /// Opens the catch-up of the unread room under the cursor.
    fn catching_up() -> (App, UnboundedReceiver<MatrixCmd>) {
        let (mut app, mut commands) = app();
        with_command(&mut app);
        to_beta(&mut app);
        app.perform(Action::CatchUp);
        assert_eq!(
            asked(&mut commands),
            ["Collect(Unread(request 1, !b:example.org, from: false))"]
        );
        (app, commands)
    }

    fn unread(app: &mut App, request: u64, messages: usize, cut: bool) {
        unread_of(app, request, messages, cut, true);
    }

    fn unread_of(app: &mut App, request: u64, messages: usize, cut: bool, known: bool) {
        app.on_matrix(crate::matrix::MatrixEvent::Collect(CollectEvent::Unread {
            request,
            result: Ok(UnreadText {
                text: "[2026-11-20 14:03] Ann: the secret plan\n".repeat(messages),
                messages,
                cut,
                known,
                stopped: None,
            }),
        }));
    }

    /// Types `text` into the composer and presses Enter.
    fn run(app: &mut App, text: &str) {
        app.perform(Action::Focus(Pane::Composer));
        app.composer.set_text(text);
        app.perform(Action::Send);
    }

    /// The answer of the worker to `/catchup N`: `messages` of the last ones.
    fn last_of(app: &mut App, request: u64, messages: usize, cut: bool, stopped: Option<Cut>) {
        app.on_matrix(crate::matrix::MatrixEvent::Collect(CollectEvent::Unread {
            request,
            result: Ok(UnreadText {
                text: "[2026-11-20 14:03] Ann: the secret plan\n".repeat(messages),
                messages,
                cut,
                known: false,
                stopped,
            }),
        }));
    }

    /// `/catchup 5` in the open room, where nothing is unread, through to its summary.
    #[test]
    fn catchup_with_a_number_summarises_the_last_messages_read_or_not() {
        let (mut app, mut commands) = app();
        with_command(&mut app);
        // Nothing is unread in the open room: `/catchup` alone says so, as before.
        run(&mut app, "/catchup");
        assert!(app.modal.is_none());
        assert_eq!(
            app.toast.clone().unwrap().0,
            "Nothing is unread in Alpha Plans"
        );
        assert!(asked(&mut commands).is_empty());

        run(&mut app, "/catchup 5");
        assert!(app.composer.is_empty());
        assert_eq!(
            asked(&mut commands),
            ["Collect(Last(request 1, !a:example.org, 5 messages))"]
        );
        assert!(matches!(
            &app.modal,
            Some(Modal::Collect(Popup::CatchUp(CatchUpPopup {
                last: Some(5),
                stage: CatchUpStage::Collecting { request: 1, .. },
                ..
            })))
        ));
        let shown = flat(&screen(&mut app));
        assert!(
            shown.contains("Loading the last 5 messages… 0 messages looked at."),
            "{shown}"
        );
        assert!(shown.contains("Their text is then given"), "{shown}");

        // The answer goes to the command at once: no question, as they were asked for.
        last_of(&mut app, 1, 5, false, None);
        let request = app.take_summary_start().expect("started at once");
        assert_eq!(request.input.lines().count(), 5);
        assert!(matches!(
            catch_up_stage(&app),
            CatchUpStage::Summarising {
                messages: 5,
                of: Of::Last { asked: 5, .. },
                ..
            }
        ));
        app.on_summary(catchup::Done {
            id: request.id,
            outcome: Outcome::Summary("Ann has a plan.".to_owned()),
        });
        let shown = flat(&screen(&mut app));
        assert!(
            shown.contains("Summary of the last 5 messages, by sh."),
            "{shown}"
        );
        assert!(shown.contains("Ann has a plan."), "{shown}");
        assert!(shown.contains("Only you see this"), "{shown}");
        // Nothing was sent to the room, and nothing marked as read.
        assert!(asked(&mut commands).is_empty());
    }

    /// The summary of the last messages says when it holds fewer than were asked for,
    /// and why.
    #[test]
    fn fewer_last_messages_than_asked_for_are_summarised_and_it_says_why() {
        let done = |messages: usize, cut: bool, stopped: Option<Cut>| {
            let (mut app, mut commands) = app();
            with_command(&mut app);
            run(&mut app, "/catchup 50");
            assert_eq!(asked(&mut commands).len(), 1);
            last_of(&mut app, 1, messages, cut, stopped);
            let request = app.take_summary_start().expect("started");
            assert_eq!(request.input.lines().count(), messages);
            app.on_summary(catchup::Done {
                id: request.id,
                outcome: Outcome::Summary("Ann has a plan.".to_owned()),
            });
            flat(&screen(&mut app))
        };
        let shown = done(12, false, None);
        assert!(
            shown.contains("Summary of the last 12 messages (the room has no more), by sh."),
            "{shown}"
        );
        let shown = done(12, false, Some(Cut::LoadFailed));
        assert!(
            shown.contains("Summary of the last 12 messages (older ones could not be loaded)"),
            "{shown}"
        );
        // Loading stopped at its bound: nothing failed, and whether the room has more
        // is not known.
        let shown = done(12, false, Some(Cut::Limit));
        assert!(
            shown.contains(&format!(
                "Summary of the last 12 messages (catching up looks no further back than \
                 the room's newest {} events), by sh.",
                thousands(LAST_EVENTS)
            )),
            "{shown}"
        );
        assert!(!shown.contains("could not be loaded"), "{shown}");
        assert!(!shown.contains("has more"), "{shown}");
        // More text than a summary takes: the newest of them, and it says so.
        let shown = done(40, true, None);
        assert!(
            shown.contains(
                "Summary of the newest 40 messages (the text of more is too long for one \
                 summary), by sh."
            ),
            "{shown}"
        );
        assert!(!shown.contains("the room has no more"), "{shown}");
    }

    /// None of the last messages at all: nothing is started, and it says whether the
    /// room has none, or loading failed or stopped at its bound before the room's
    /// first message.
    #[test]
    fn no_last_messages_say_whether_the_room_has_none_or_loading_ended_first() {
        let nothing = |stopped: Option<Cut>| {
            let (mut app, mut commands) = app();
            with_command(&mut app);
            run(&mut app, "/catchup 50");
            assert_eq!(asked(&mut commands).len(), 1);
            last_of(&mut app, 1, 0, false, stopped);
            assert!(app.take_summary_start().is_none());
            let stage = catch_up_stage(&app).clone();
            let shown = flat(&screen(&mut app));
            // Enter closes the popup.
            app.on_event(key(KeyCode::Enter));
            assert!(app.modal.is_none());
            (stage, shown)
        };
        let (stage, shown) = nothing(None);
        assert!(stage == CatchUpStage::Nothing);
        assert!(
            shown.contains("No messages were found in this room."),
            "{shown}"
        );

        let (stage, shown) = nothing(Some(Cut::LoadFailed));
        assert!(stage == CatchUpStage::Failed("Could not load the messages.".to_owned()));
        assert!(shown.contains("Could not load the messages."), "{shown}");
        assert!(!shown.contains("No messages were found"), "{shown}");

        let (stage, shown) = nothing(Some(Cut::Limit));
        assert!(stage == CatchUpStage::NothingInNewest);
        assert!(
            shown.contains(&format!(
                "No messages were found in the newest {} events of this room; catching up \
                 looks no further back.",
                thousands(LAST_EVENTS)
            )),
            "{shown}"
        );
        assert!(!shown.contains("in this room."), "{shown}");

        // What is unread is as before: nothing unread is nothing unread.
        for stopped in [Some(Cut::LoadFailed), Some(Cut::Limit)] {
            let (mut app, _commands) = catching_up();
            app.on_matrix(crate::matrix::MatrixEvent::Collect(CollectEvent::Unread {
                request: 1,
                result: Ok(UnreadText {
                    text: String::new(),
                    messages: 0,
                    cut: false,
                    known: true,
                    stopped,
                }),
            }));
            assert!(*catch_up_stage(&app) == CatchUpStage::Nothing);
            assert!(flat(&screen(&mut app)).contains("Nothing is unread"));
        }
    }

    /// Anything after `/catchup` but a whole number from 1 to 300 is refused with one
    /// line that says what it takes; the draft stays to be fixed, nothing is asked.
    #[test]
    fn catchup_refuses_anything_but_a_number_of_last_messages_from_1_to_300() {
        let (mut app, mut commands) = app();
        with_command(&mut app);
        let usage =
            "/catchup takes nothing (what is unread) or a number of last messages from 1 to 300";
        for text in [
            "/catchup 0",
            "/catchup 301",
            "/catchup 99999999999999999999999",
            "/catchup word",
            "/catchup -3",
            "/catchup +5",
            "/catchup 1.5",
            "/catchup 5 more",
            "/catchup ５",
        ] {
            app.toast = None;
            run(&mut app, text);
            assert_eq!(app.toast.clone().unwrap().0, usage, "{text}");
            assert_eq!(app.composer.text(), text);
            assert!(app.modal.is_none(), "{text}");
        }
        assert!(asked(&mut commands).is_empty());
        // The ends of the range are taken, spaces around the number too.
        for (text, count) in [("/catchup 1", 1), ("/catchup  300 ", 300)] {
            app.modal = None;
            run(&mut app, text);
            let sent = asked(&mut commands);
            assert_eq!(sent.len(), 1, "{text}");
            assert!(
                sent[0].ends_with(&format!(", {count} messages))")),
                "{sent:?}"
            );
            // One message is "it".
            let shown = flat(&screen(&mut app));
            let (what, its) = match count {
                1 => ("the last message", "Its text"),
                _ => ("the last 300 messages", "Their text"),
            };
            assert!(shown.contains(&format!("Loading {what}…")), "{shown}");
            assert!(shown.contains(its), "{shown}");
        }
    }

    #[test]
    fn catchup_with_a_number_needs_a_summary_command_and_a_room_too() {
        let (mut app, mut commands) = app();
        run(&mut app, "/catchup 5");
        assert!(app.modal.is_none());
        assert!(app.toast.clone().unwrap().0.contains("summary_command"));
        assert!(asked(&mut commands).is_empty());

        with_command(&mut app);
        app.rooms[0].kind = RoomKind::Invite;
        app.toast = None;
        run(&mut app, "/catchup 5");
        assert!(app.modal.is_none());
        assert!(app.toast.is_some());
        assert!(asked(&mut commands).is_empty());
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
        assert!(
            shown.contains("newest 2 unread messages (there are more)"),
            "{shown}"
        );
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
    fn where_reading_stopped_is_not_found_nothing_is_started_until_the_user_says_so() {
        let (mut app, mut commands) = catching_up();
        unread_of(&mut app, 1, 3, false, false);
        assert!(matches!(
            catch_up_stage(&app),
            CatchUpStage::Asking { messages: 3, .. }
        ));
        // The text has gone nowhere.
        assert!(app.take_summary_start().is_none());
        let shown = flat(&screen(&mut app));
        assert!(shown.contains("was not found"), "{shown}");
        assert!(
            shown.contains("Summarise the newest 3 messages instead?"),
            "{shown}"
        );
        assert!(shown.contains("Enter summarise"), "{shown}");

        // Esc: it never does.
        let (mut left, _other) = catching_up();
        unread_of(&mut left, 1, 3, false, false);
        left.on_event(key(KeyCode::Esc));
        assert!(left.modal.is_none());
        assert!(left.take_summary_start().is_none());

        // Enter: the newest go to the command, and the result says what they were.
        app.on_event(key(KeyCode::Enter));
        let request = app.take_summary_start().expect("started now");
        assert_eq!(request.input.lines().count(), 3);
        // A second Enter starts nothing more.
        app.on_event(key(KeyCode::Enter));
        assert!(app.take_summary_start().is_none());
        app.on_summary(catchup::Done {
            id: request.id,
            outcome: Outcome::Summary("Ann has a plan.".to_owned()),
        });
        let shown = flat(&screen(&mut app));
        assert!(
            shown.contains("Summary of the newest 3 messages, read or not"),
            "{shown}"
        );
        assert!(!shown.contains("unread messages"), "{shown}");
        assert!(asked(&mut commands).is_empty());
    }

    #[test]
    fn the_worker_is_told_how_many_of_the_newest_to_take_if_need_be() {
        // The room counts 3 as unread.
        let (mut app, mut commands) = app();
        with_command(&mut app);
        to_beta(&mut app);
        app.perform(Action::CatchUp);
        let sent = commands.try_recv().unwrap();
        assert!(
            matches!(
                sent,
                MatrixCmd::Collect(CollectCmd::Unread { newest: 3, .. })
            ),
            "{}",
            sent.describe()
        );
        // Unread without a count (marked so by hand): a handful.
        app.modal = None;
        let other = app.cursor_room().unwrap();
        app.rooms[other].unread = 0;
        app.rooms[other].marked_unread = true;
        app.perform(Action::CatchUp);
        let sent = commands.try_recv().unwrap();
        assert!(
            matches!(
                sent,
                MatrixCmd::Collect(CollectCmd::Unread { newest: 50, .. })
            ),
            "{}",
            sent.describe()
        );
        // More than a summary takes: no more than that.
        app.modal = None;
        app.rooms[other].unread = 5000;
        app.perform(Action::CatchUp);
        let sent = commands.try_recv().unwrap();
        assert!(
            matches!(
                sent,
                MatrixCmd::Collect(CollectCmd::Unread { newest: 300, .. })
            ),
            "{}",
            sent.describe()
        );
    }

    #[test]
    fn a_summary_of_all_that_is_unread_says_so() {
        let (mut app, _commands) = catching_up();
        unread(&mut app, 1, 2, false);
        let request = app.take_summary_start().unwrap();
        // The collection was request 1; the command is counted when it starts.
        assert_eq!(request.id, 2);
        app.on_summary(catchup::Done {
            id: request.id,
            outcome: Outcome::Summary("Ann has a plan.".to_owned()),
        });
        let shown = flat(&screen(&mut app));
        assert!(
            shown.contains("Summary of 2 unread messages, by sh."),
            "{shown}"
        );
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
        assert!(*catch_up_stage(&app) == CatchUpStage::Nothing);
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
        assert_eq!(app.collect.summary_setting(), "/bin/sh -c cat");
        assert_eq!(app.summary_setting_text(), "/bin/sh -c cat");
        assert!(app.toast.clone().unwrap().0.contains("to sh"));
        assert!(app.toast.clone().unwrap().0.contains("(/catchup N)"));
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
        app.collect.command = Ok(catchup::Command::for_test(
            "/home/alice/bin/summarise".into(),
            vec!["--key=hunter2".to_owned()],
        ));
        unread(&mut app, 1, 1, false);
        let mut shown = vec![format!("{app:?}")];
        let request = app.take_summary_start();
        if let Some(request) = &request {
            shown.push(format!("{request:?}"));
            app.on_summary(catchup::Done {
                id: request.id,
                outcome: Outcome::Summary("the hushed summary".to_owned()),
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
            "hushed",
            "secret plan",
            "Beta",
            "Alpha",
            "never-written",
            "example.org",
        ] {
            assert!(!all.contains(private), "{private} in {all}");
        }
    }
}
