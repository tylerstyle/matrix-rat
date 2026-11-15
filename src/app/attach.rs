//! Attaching files to a message: dropped or pasted paths, the `/upload` command with
//! Tab completion, and the files waiting in the composer until Enter sends them.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyModifiers};

use super::{App, ComposeMode};
use crate::action::Pane;
use crate::clipboard::TempPicture;
use crate::format::Outgoing;
use crate::images::human_size;
use crate::matrix::MatrixCmd;
#[cfg(test)]
use crate::model::RoomKind;
use crate::model::{ItemRef, Message, OWN_NAME};
use crate::upload::{self, Chosen};

/// Where files wait: a room, or one thread of it (room id and what names the thread).
/// Like drafts, they belong to the conversation they were attached in.
pub type Scope = (String, Option<String>);

/// What a send handed to the worker, to give it back if none of it could be sent.
struct Handed {
    scope: Scope,
    files: Vec<Chosen>,
    text: String,
}

/// Hand-written: which room, which files and what text stay out of logs.
impl std::fmt::Debug for Handed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Handed({} files, {} bytes of text)",
            self.files.len(),
            self.text.len()
        )
    }
}

#[derive(Default)]
pub struct AttachState {
    /// Files waiting to be sent.
    pub(super) pending: HashMap<Scope, Vec<Chosen>>,
    /// The draft put aside while an `/upload` command is typed.
    stash: HashMap<Scope, String>,
    /// The sends the worker has not answered yet, by their number. It reads the files
    /// only some time after it was handed them, so no picture of ours goes while one
    /// is out; and each answer finds its own files, whichever comes first.
    handed: HashMap<u64, Handed>,
    /// Sends numbered so far.
    sends: u64,
    /// The largest file the server accepts, once it has said so.
    pub limit: Option<u64>,
    /// The last draw had no room to show the waiting files.
    pub hidden: bool,
    /// Pictures pasted from the clipboard: files of ours, each removed (by being
    /// dropped) once no chip and no send needs it any more.
    pub(super) pasted: Vec<TempPicture>,
    /// The clipboard is being read for this conversation; the number tells the answer
    /// from that of a reading given up before.
    pub(super) reading: Option<(u64, Scope)>,
    /// Readings asked for so far; kept by `clear`, so that an answer from before a
    /// logout is never taken for that of a new reading.
    pub(super) readings: u64,
    /// A reading for the run loop to start.
    pub(super) paste_request: Option<crate::clipboard::Request>,
}

/// Hand-written: rooms, file paths and drafts stay out of logs; counts say enough.
impl std::fmt::Debug for AttachState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachState")
            .field(
                "pending",
                &self.pending.values().map(Vec::len).sum::<usize>(),
            )
            .field("stash", &self.stash.len())
            .field("handed", &self.handed)
            .field("sends", &self.sends)
            .field("limit", &self.limit)
            .field("hidden", &self.hidden)
            .field("pasted", &self.pasted.len())
            .field(
                "reading",
                &self.reading.as_ref().map(|(reading, _)| reading),
            )
            .field("readings", &self.readings)
            .field("paste_request", &self.paste_request.is_some())
            .finish()
    }
}

impl AttachState {
    /// The number of the send handed to the worker last.
    #[cfg(test)]
    pub(super) fn last_send(&self) -> u64 {
        self.sends
    }

    /// The files waiting in `scope`.
    pub fn files(&self, scope: Option<&Scope>) -> &[Chosen] {
        scope
            .and_then(|scope| self.pending.get(scope))
            .map_or(&[], Vec::as_slice)
    }

    /// The draft put aside in `scope` for an `/upload` command; empty if none was.
    pub fn stashed(&self, scope: &Scope) -> &str {
        self.stash.get(scope).map_or("", String::as_str)
    }

    /// Puts `draft` aside in `scope` as if an `/upload` command had been begun over
    /// it, unless one is there already.
    pub fn stash_draft(&mut self, scope: Scope, draft: String) {
        self.stash.entry(scope).or_insert(draft);
    }

    /// Drops what waits in a room and its threads: the room is gone.
    pub fn forget_room(&mut self, room_id: &str) {
        self.pending.retain(|(room, _), _| room != room_id);
        self.stash.retain(|(room, _), _| room != room_id);
        self.sweep();
    }

    pub fn clear(&mut self) {
        let readings = self.readings;
        *self = Self::default();
        self.readings = readings;
    }

    /// Removes the pasted pictures that nothing needs any more: no chip shows them, and
    /// the worker has answered every send (it may still be about to read one that went).
    /// Called wherever a chip goes or a send is answered.
    fn sweep(&mut self) {
        if !self.handed.is_empty() {
            return;
        }
        let pending = &self.pending;
        self.pasted.retain(|picture| {
            pending
                .values()
                .flatten()
                .any(|file| file.path == picture.path())
        });
    }
}

impl App {
    /// The files waiting to be sent to the open room.
    pub fn attachments(&self) -> &[Chosen] {
        self.attach.files(self.attach_scope().as_ref())
    }

    /// The conversation files are attached to right now: the open thread, else the room.
    pub fn attach_scope(&self) -> Option<Scope> {
        let room = self.room()?;
        Some((room.id.clone(), self.thread_key().map(str::to_owned)))
    }

    /// Whether files can be attached here; says why not.
    pub(super) fn can_attach(&mut self) -> bool {
        if matches!(self.compose_mode, ComposeMode::Edit(_)) {
            self.show_toast("Finish editing before attaching a file");
            false
        } else if let Some(hint) = self
            .room()
            .filter(|room| room.is_pending())
            .map(|room| room.kind.pending_hint())
        {
            self.show_toast(hint);
            false
        } else {
            self.room().is_some()
        }
    }

    /// Adds files to the open room's waiting ones and returns how many were taken.
    /// A refusal is explained.
    pub(super) fn attach_files(&mut self, files: Vec<Chosen>) -> usize {
        let Some(scope) = self.attach_scope() else {
            return 0;
        };
        let mut taken = 0;
        let mut refusal = None;
        for file in files {
            let waiting = self.attach.pending.entry(scope.clone()).or_default();
            if waiting.iter().any(|other| other.path == file.path) {
                refusal = Some(format!("{} is already attached", file.name));
            } else if let Some(reason) = upload::too_large(&file.name, file.size, self.attach.limit)
            {
                refusal = Some(reason);
            } else {
                waiting.push(file);
                taken += 1;
            }
        }
        if let Some(reason) = refusal {
            self.show_toast(reason);
        }
        if taken > 0 {
            self.set_focus(Pane::Composer);
        }
        taken
    }

    /// A paste that is nothing but the paths of existing files (what a terminal types
    /// when files are dropped on it) attaches them. False: it is text.
    pub(super) fn attach_pasted(&mut self, text: &str) -> bool {
        // Where no file can be attached (an edit, an invitation or pending knock), a path is text.
        let pending = self.room().is_none_or(|room| room.is_pending());
        if pending || matches!(self.compose_mode, ComposeMode::Edit(_)) {
            return false;
        }
        let Some(paths) = upload::dropped_files(text) else {
            return false;
        };
        let mut files = Vec::new();
        for path in paths {
            match upload::check(&path) {
                Ok(file) => files.push(file),
                // It is a file, so it is not typed out as text either.
                Err(reason) => self.show_toast(reason),
            }
        }
        // A drop answers an `/upload` command that was being typed.
        if self.attach_files(files) > 0 && upload::command_argument(self.composer.text()).is_some()
        {
            self.restore_draft();
        }
        true
    }

    pub(super) fn remove_attachment(&mut self, index: usize) {
        if let Some(scope) = self.attach_scope()
            && let Some(waiting) = self.attach.pending.get_mut(&scope)
            && index < waiting.len()
        {
            waiting.remove(index);
            self.attach.sweep();
        }
    }

    /// Forgets the waiting files of the open conversation, and a draft put aside for
    /// an `/upload` command there. False if there was neither.
    pub(super) fn clear_attachments(&mut self) -> bool {
        let Some(scope) = self.attach_scope() else {
            return false;
        };
        let had_draft = self.attach.stash.remove(&scope).is_some();
        let had_files = self
            .attach
            .pending
            .remove(&scope)
            .is_some_and(|waiting| !waiting.is_empty());
        self.attach.sweep();
        had_draft || had_files
    }

    /// Alt-A and the paperclip: starts an `/upload` command. A draft that is being
    /// written comes back when the file is chosen, as its caption.
    pub(super) fn begin_attach(&mut self) {
        if !self.can_attach() {
            return;
        }
        let Some(scope) = self.attach_scope() else {
            return;
        };
        self.set_focus(Pane::Composer);
        if upload::command_argument(self.composer.text()).is_some() {
            return;
        }
        let draft = self.composer.take();
        if !draft.trim().is_empty() {
            self.attach.stash.insert(scope, draft);
        }
        self.composer.set_text("/upload ");
    }

    /// The draft that was put aside for an `/upload` command comes back.
    pub(super) fn restore_draft(&mut self) {
        let draft = self
            .attach_scope()
            .and_then(|scope| self.attach.stash.remove(&scope))
            .unwrap_or_default();
        self.composer.set_text(&draft);
    }

    /// Enter on an `/upload` command: the file joins the waiting ones (nothing is sent
    /// yet) and the draft comes back. False if the composer holds no such command.
    pub(super) fn upload_command(&mut self) -> bool {
        let text = self.composer.text().to_owned();
        let Some(argument) = upload::command_argument(&text) else {
            return false;
        };
        if !self.can_attach() {
            return true;
        }
        match upload::command_files(argument) {
            Ok(files) => {
                let wanted = files.len();
                // The command stays for fixing if not all of it could be attached.
                if self.attach_files(files) == wanted {
                    self.restore_draft();
                }
            }
            Err(reason) => self.show_toast(reason),
        }
        true
    }

    /// Keys that mean something else while files are being attached. True if used.
    pub(super) fn attach_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        // The text of a message being edited is never a command.
        if matches!(self.compose_mode, ComposeMode::Edit(_)) {
            return false;
        }
        match (code, mods) {
            (KeyCode::Tab, KeyModifiers::NONE) => self.complete_upload_path(),
            // Backspace in an empty message takes the last file off again.
            (KeyCode::Backspace, _) if self.composer.is_empty() => {
                let last = self.attachments().len().checked_sub(1);
                last.is_some_and(|last| {
                    self.remove_attachment(last);
                    true
                })
            }
            // Esc gives up an `/upload` command and brings the draft back.
            (KeyCode::Esc, KeyModifiers::NONE)
                if upload::command_argument(self.composer.text()).is_some() =>
            {
                self.restore_draft();
                true
            }
            _ => false,
        }
    }

    /// Tab in an `/upload` command completes the path. False anywhere else.
    fn complete_upload_path(&mut self) -> bool {
        let text = self.composer.text().to_owned();
        let Some(argument) = upload::command_argument(&text) else {
            return false;
        };
        let command = &text[..text.len() - argument.len()];
        if !command.ends_with(char::is_whitespace) {
            // Only the command's name so far.
            self.composer.set_text(&format!("{command} "));
            return true;
        }
        match upload::complete(argument) {
            Some(completion) => {
                if let Some(choices) = super::format_completion_options(&completion.options) {
                    self.show_toast(choices);
                }
                self.composer
                    .set_text(&format!("{command}{}", completion.text));
            }
            None => self.show_toast("No file or folder matches"),
        }
        true
    }

    /// Enter with files waiting: sends them, the text as the caption of the first.
    /// False if no files are waiting.
    pub(super) fn send_attachments(&mut self) -> bool {
        let files = self.attachments().to_vec();
        let Some(room) = self.room() else {
            return false;
        };
        if files.is_empty() {
            return false;
        }
        let room_id = room.id.clone();
        if room.is_pending() {
            let hint = room.kind.pending_hint();
            self.show_toast(hint);
            return true;
        }
        // Never send what is not on the screen.
        if self.attach.hidden {
            self.show_toast("Make the window taller to see the attached files first");
            return true;
        }
        let Some(scope) = self.attach_scope() else {
            return false;
        };
        let text = self.composer.text().trim().to_owned();
        let caption = if text.is_empty() {
            None
        } else {
            match self.format_outgoing(self.composer.text()) {
                Outgoing::Rejected(reason) => {
                    self.show_toast(reason);
                    return true;
                }
                Outgoing::Emote { .. } => {
                    self.show_toast("A caption cannot be a /me action");
                    return true;
                }
                caption @ Outgoing::Text { .. } => Some(caption),
            }
        };
        let reply_to = match self.compose_mode {
            ComposeMode::Normal => None,
            ComposeMode::Edit(_) => {
                self.show_toast("Finish editing before sending files");
                return true;
            }
            ComposeMode::Reply(i) => match self.messages().get(i).and_then(|m| m.item.clone()) {
                Some(ItemRef::Event(event_id)) => Some(event_id),
                _ if self.matrix.is_none() => None,
                _ => {
                    self.show_toast("That message has not been sent yet");
                    return true;
                }
            },
        };
        // Looked at again: time has passed since the files were attached. Nothing is
        // sent unless all of it can be; the file in question keeps its place, for the
        // user to take off.
        for file in &files {
            let problem = match upload::check(&file.path) {
                Ok(now) => upload::too_large(&now.name, now.size, self.attach.limit),
                Err(reason) => Some(reason),
            };
            if let Some(problem) = problem {
                self.show_toast(problem);
                return true;
            }
        }

        if self.matrix.is_some() {
            self.clear_marked_unread(self.active);
            self.attach.sends += 1;
            self.send_matrix(MatrixCmd::SendFiles {
                send: self.attach.sends,
                room_id: room_id.clone(),
                files: files.iter().map(|file| file.path.clone()).collect(),
                caption,
                reply_to,
            });
        } else {
            // Demo mode: everything happens locally.
            let replied = match self.compose_mode {
                ComposeMode::Reply(i) => self
                    .messages()
                    .get(i)
                    .map(|m| (m.sender.clone(), m.body.clone())),
                _ => None,
            };
            for (n, file) in files.iter().enumerate() {
                let mut body = format!("📎 {} · {}", file.name, human_size(file.size));
                if n == 0 && !text.is_empty() {
                    body.push('\n');
                    body.push_str(&text);
                }
                self.push_message(Message {
                    sender: OWN_NAME.to_owned(),
                    time: "now".to_owned(),
                    day: "Today".to_owned(),
                    body,
                    own: true,
                    reply_to: replied.clone().filter(|_| n == 0),
                    ..Message::default()
                });
            }
        }
        self.attach.pending.remove(&scope);
        if self.matrix.is_some() {
            let handed = Handed {
                scope,
                files,
                text: self.composer.text().to_owned(),
            };
            self.attach.handed.insert(self.attach.sends, handed);
        }
        // In the demo a pasted picture has been "sent" now; else it waits for the answer.
        self.attach.sweep();
        self.composer.take();
        self.clear_completion_marks();
        self.compose_mode = ComposeMode::Normal;
        // The caption was the draft.
        self.forget_sent_draft();
        if self.timeline.detached {
            self.return_to_live();
        } else {
            self.timeline.scroll = 0;
        }
        true
    }

    /// The worker could not send the first of a send's files, the one the caption (and a
    /// reply) belonged to, and so sent none: they wait again, and the text is back if
    /// nothing was written in the meantime.
    pub(super) fn on_files_not_sent(&mut self, send: u64, reason: String) {
        // A room that was left meanwhile has no composer for them to wait in.
        if let Some(handed) = self.attach.handed.remove(&send)
            && !self.logged_out
            && self.rooms.iter().any(|room| room.id == handed.scope.0)
        {
            let here = self.attach_scope().as_ref() == Some(&handed.scope);
            let waiting = self.attach.pending.entry(handed.scope).or_default();
            for file in handed.files {
                if !waiting.iter().any(|other| other.path == file.path) {
                    waiting.push(file);
                }
            }
            if here && self.composer.is_empty() && self.compose_mode == ComposeMode::Normal {
                self.composer.set_text(&handed.text);
            }
        }
        // After the files are back: a pasted picture among them stays.
        self.attach.sweep();
        self.show_toast(format!("{reason}. Nothing was sent"));
    }

    /// The worker has read the files of a send and needs them no more: pasted pictures
    /// that no chip shows go, once no other send is out.
    pub(super) fn on_files_sent(&mut self, send: u64) {
        self.attach.handed.remove(&send);
        self.attach.sweep();
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crossterm::event::{Event, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::action::Action;
    use crate::matrix::{MatrixEvent, MatrixHandle};
    use crate::model::Room;
    use crate::upload::tests::Scratch;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
    }

    fn remote(id: &str, body: &str) -> Message {
        Message {
            id: id.to_owned(),
            item: Some(ItemRef::Event(format!("${id}"))),
            sender: "ann".to_owned(),
            body: body.to_owned(),
            ..Message::default()
        }
    }

    /// Connected, with two rooms, the first one open.
    fn connected() -> (App, UnboundedReceiver<MatrixCmd>) {
        let mut app = App::demo();
        let (handle, commands) = MatrixHandle::for_test();
        app.matrix = Some(handle);
        let mut alpha = Room::new("!a", "Alpha", RoomKind::Room);
        alpha.messages = vec![remote("one", "first"), remote("two", "second")];
        app.rooms = vec![alpha, Room::new("!b", "Beta", RoomKind::Room)];
        app.active = 0;
        (app, commands)
    }

    type Sent = (String, Vec<PathBuf>, Option<Outgoing>, Option<String>);

    /// The file commands sent so far; anything else sent is forgotten.
    fn sent(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<Sent> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            // From a thread the command arrives wrapped; one test looks at that itself.
            let command = match command {
                MatrixCmd::InThread { command, .. } => *command,
                command => command,
            };
            if let MatrixCmd::SendFiles {
                room_id,
                files,
                caption,
                reply_to,
                ..
            } = command
            {
                seen.push((room_id, files, caption, reply_to));
            }
        }
        seen
    }

    fn toast(app: &App) -> &str {
        app.toast.as_ref().map_or("", |(text, _)| text.as_str())
    }

    fn paths(app: &App) -> Vec<PathBuf> {
        app.attachments()
            .iter()
            .map(|file| file.path.clone())
            .collect()
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        format!("{}", terminal.backend())
    }

    fn find(screen: &str, needle: &str) -> (u16, u16) {
        for (row, line) in screen.lines().enumerate() {
            // Each row is printed in quotes; columns are counted in cells.
            if let Some(at) = line.find(needle) {
                let column = crate::text::width(&line[..at]) - 1;
                return (column as u16, row as u16);
            }
        }
        panic!("{needle:?} is not on the screen:\n{screen}");
    }

    fn click(column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn a_pasted_path_becomes_a_chip_and_enter_sends_it_with_the_caption() {
        let scratch = Scratch::new("app-paste");
        let notes = scratch.file("notes.txt", b"hello");
        let photo = scratch.file("my photo.png", &[0; 1500]);
        let (mut app, mut commands) = connected();
        type_text(&mut app, "look at **this**");

        // Dropped from a file manager while the room list has the focus.
        app.perform(Action::Focus(Pane::Sidebar));
        app.on_event(Event::Paste(format!(
            "{}\n'{}'\n",
            notes.display(),
            photo.display()
        )));
        assert_eq!(paths(&app), [notes.clone(), photo.clone()]);
        assert_eq!(app.focus, Pane::Composer);
        // Not typed out, not put into the room filter, and nothing sent by itself.
        assert_eq!(app.composer.text(), "look at **this**");
        assert_eq!(app.sidebar.filter, "");
        assert!(sent(&mut commands).is_empty());
        let shown = screen(&mut app, 80, 24);
        assert!(shown.contains("📎 notes.txt · 5 B"), "{shown}");
        assert!(shown.contains("📎 my photo.png · 1.5 kB"), "{shown}");

        app.on_event(key(KeyCode::Enter));
        let commands = sent(&mut commands);
        let [(room, files, caption, reply_to)] = commands.as_slice() else {
            panic!("expected one command, got {}", commands.len());
        };
        assert_eq!(room, "!a");
        assert_eq!(files, &[notes, photo]);
        assert_eq!(
            caption,
            &Some(Outgoing::Text {
                plain: "look at **this**".into(),
                html: Some("look at <strong>this</strong>".into()),
                mentions: None,
            })
        );
        assert_eq!(reply_to, &None);
        // The composer is free again.
        assert!(app.attachments().is_empty());
        assert!(app.composer.is_empty());
    }

    #[test]
    fn a_paste_that_is_not_only_files_is_text() {
        let scratch = Scratch::new("app-text");
        let notes = scratch.file("notes.txt", b"hello");
        let (mut app, mut commands) = connected();
        for paste in [
            "just words".to_owned(),
            format!("see {}", notes.display()),
            format!("{}/missing.txt", scratch.0.display()),
            scratch.0.display().to_string(),
        ] {
            app.composer.take();
            app.on_event(Event::Paste(paste.clone()));
            assert_eq!(app.composer.text(), paste);
            assert!(app.attachments().is_empty());
        }
        // Without files waiting, Enter sends the text as a message, as ever.
        app.composer.set_text("just words");
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());
        assert!(app.composer.is_empty());

        // A path pasted into a popup's field is text for that field.
        app.perform(Action::OpenSettings);
        app.on_event(Event::Paste(notes.display().to_string()));
        assert!(app.attachments().is_empty());
    }

    #[test]
    fn files_without_a_caption_and_as_a_reply() {
        let scratch = Scratch::new("app-reply");
        let notes = scratch.file("notes.txt", b"hello");
        let (mut app, mut commands) = connected();
        app.on_event(Event::Paste(notes.display().to_string()));
        app.on_event(key(KeyCode::Enter));
        let first = sent(&mut commands);
        assert_eq!(first.len(), 1);
        assert_eq!((&first[0].2, &first[0].3), (&None, &None));

        // Replying to "second": the file goes out as the reply.
        app.timeline.selected = Some(1);
        app.perform(Action::Reply);
        assert_eq!(app.compose_mode, ComposeMode::Reply(1));
        app.on_event(Event::Paste(notes.display().to_string()));
        type_text(&mut app, "here");
        app.on_event(key(KeyCode::Enter));
        let second = sent(&mut commands);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].3.as_deref(), Some("$two"));
        assert_eq!(app.compose_mode, ComposeMode::Normal);

        // A /me action or an unknown command is no caption; everything stays.
        app.on_event(Event::Paste(notes.display().to_string()));
        for (text, reason) in [
            ("/me waves", "A caption cannot be a /me action"),
            ("/nosuch thing", "Unknown command /nosuch"),
        ] {
            app.composer.set_text(text);
            app.on_event(key(KeyCode::Enter));
            assert!(toast(&app).starts_with(reason), "{}", toast(&app));
            assert_eq!(app.composer.text(), text);
            assert_eq!(app.attachments().len(), 1);
        }
        assert!(sent(&mut commands).is_empty());
    }

    #[test]
    fn chips_come_off_by_click_backspace_and_ctrl_c() {
        let scratch = Scratch::new("app-remove");
        let files: Vec<PathBuf> = ["a.txt", "b.txt", "c.txt"]
            .iter()
            .map(|name| scratch.file(name, b"x"))
            .collect();
        let (mut app, mut commands) = connected();
        let all = files
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        app.on_event(Event::Paste(all.clone()));
        assert_eq!(paths(&app), files);
        // The same file twice is one chip.
        app.on_event(Event::Paste(files[0].display().to_string()));
        assert_eq!(app.attachments().len(), 3);
        assert_eq!(toast(&app), "a.txt is already attached");

        // The ✕ of the middle one.
        let shown = screen(&mut app, 80, 24);
        let (name, row) = find(&shown, "b.txt");
        let line = shown.lines().nth(row as usize).unwrap();
        let close = crate::text::width(&line[..line.find('✕').unwrap()]) as u16 - 1;
        assert!(close > name);
        app.on_event(click(close, row));
        assert_eq!(paths(&app), [files[0].clone(), files[2].clone()]);

        // Backspace edits the text first; in an empty message it takes the last file.
        type_text(&mut app, "ab");
        app.on_event(key(KeyCode::Backspace));
        assert_eq!(app.composer.text(), "a");
        assert_eq!(app.attachments().len(), 2);
        app.on_event(key(KeyCode::Backspace));
        app.on_event(key(KeyCode::Backspace));
        assert_eq!(paths(&app), [files[0].clone()]);

        // Ctrl-C clears the draft and what is attached to it.
        app.on_event(Event::Paste(all));
        type_text(&mut app, "caption");
        app.on_event(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert!(app.composer.is_empty());
        assert!(app.attachments().is_empty());
        assert!(sent(&mut commands).is_empty());
        // With nothing left, Enter does nothing.
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());
    }

    #[test]
    fn files_larger_than_the_server_accepts_are_refused() {
        let scratch = Scratch::new("app-limit");
        let small = scratch.file("small.bin", &[0; 900]);
        let big = scratch.file("big.bin", &[0; 2500]);
        let (mut app, mut commands) = connected();
        app.on_matrix(MatrixEvent::UploadLimit(1000));

        app.on_event(Event::Paste(format!(
            "{} {}",
            small.display(),
            big.display()
        )));
        assert_eq!(paths(&app), std::slice::from_ref(&small));
        assert_eq!(
            toast(&app),
            "big.bin is too large: 2.5 kB, the server takes up to 1.0 kB"
        );
        app.composer.set_text(&format!("/upload {}", big.display()));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.attachments().len(), 1);
        assert!(toast(&app).starts_with("big.bin is too large: 2.5 kB"));
        // The command stays, to be corrected.
        assert!(app.composer.text().starts_with("/upload "));
        app.composer.take();

        // The file grew after it was attached: refused when sending, and kept.
        std::fs::write(&small, [0; 1200]).unwrap();
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());
        assert!(toast(&app).starts_with("small.bin is too large: 1.2 kB"));
        assert_eq!(app.attachments().len(), 1);
        // Gone altogether: the same.
        std::fs::remove_file(&small).unwrap();
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());
        assert_eq!(toast(&app), "small.bin does not exist");
        assert_eq!(app.attachments().len(), 1);
    }

    #[test]
    fn the_upload_command_with_tab_completion_keeps_the_draft() {
        let scratch = Scratch::new("app-command");
        let cat = scratch.file("photos/cat picture.png", b"12345");
        scratch.file("photos/car.png", b"1");
        let dir = scratch.0.display().to_string();
        let (mut app, mut commands) = connected();
        type_text(&mut app, "my caption");

        // Alt-A puts the draft aside for the command.
        app.on_event(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::ALT,
        )));
        assert_eq!(app.composer.text(), "/upload ");
        type_text(&mut app, &format!("{dir}/ph"));
        app.on_event(key(KeyCode::Tab));
        assert_eq!(app.composer.text(), format!("/upload {dir}/photos/"));
        assert_eq!(app.focus, Pane::Composer);
        // Two candidates: completed as far as they agree, and both named.
        app.on_event(key(KeyCode::Tab));
        assert_eq!(app.composer.text(), format!("/upload {dir}/photos/ca"));
        assert_eq!(toast(&app), "car.png  cat picture.png");
        type_text(&mut app, "t");
        app.on_event(key(KeyCode::Tab));
        assert_eq!(
            app.composer.text(),
            format!("/upload {dir}/photos/cat picture.png")
        );
        type_text(&mut app, "x");
        app.on_event(key(KeyCode::Tab));
        assert_eq!(toast(&app), "No file or folder matches");
        app.on_event(key(KeyCode::Backspace));

        // Enter attaches (it does not send) and the draft is back as the caption.
        app.on_event(key(KeyCode::Enter));
        assert_eq!(paths(&app), std::slice::from_ref(&cat));
        assert_eq!(app.composer.text(), "my caption");
        assert!(sent(&mut commands).is_empty());
        app.on_event(key(KeyCode::Enter));
        let commands_sent = sent(&mut commands);
        assert_eq!(commands_sent.len(), 1);
        assert_eq!(commands_sent[0].1, [cat]);
        assert!(matches!(
            &commands_sent[0].2,
            Some(Outgoing::Text { plain, .. }) if plain == "my caption"
        ));

        // Mistakes are explained and the command stays; Esc gives it up.
        type_text(&mut app, "draft");
        app.perform(Action::Attach);
        app.on_event(key(KeyCode::Enter));
        assert!(toast(&app).contains("needs the path"));
        type_text(&mut app, &dir);
        app.on_event(key(KeyCode::Enter));
        assert!(toast(&app).contains("is a folder"));
        assert!(app.composer.text().starts_with("/upload "));
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.composer.text(), "draft");
        assert!(app.attachments().is_empty());
        // `/file` is the same command, and Tab after the bare name adds the space.
        app.composer.set_text("/file");
        app.on_event(key(KeyCode::Tab));
        assert_eq!(app.composer.text(), "/file ");
        // Anywhere else Tab still moves between the panes.
        app.composer.set_text("words");
        app.on_event(key(KeyCode::Tab));
        assert_eq!(app.focus, Pane::Sidebar);
        assert!(sent(&mut commands).is_empty());
    }

    #[test]
    fn chips_stay_with_their_room_and_editing_takes_no_files() {
        let scratch = Scratch::new("app-rooms");
        let notes = scratch.file("notes.txt", b"hello");
        let (mut app, mut commands) = connected();
        app.on_event(Event::Paste(notes.display().to_string()));
        assert_eq!(app.attachments().len(), 1);

        app.open_room(1);
        assert!(app.attachments().is_empty());
        type_text(&mut app, "hi");
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());
        app.open_room(0);
        assert_eq!(paths(&app), std::slice::from_ref(&notes));

        // While a message is being edited, a path is text and Alt-A is refused.
        app.clear_attachments();
        let mut own = remote("mine", "typo");
        own.own = true;
        app.rooms[0].messages.push(own);
        app.timeline.selected = Some(2);
        app.perform(Action::Edit);
        assert!(matches!(app.compose_mode, ComposeMode::Edit(2)));
        app.on_event(Event::Paste(notes.display().to_string()));
        assert!(app.attachments().is_empty());
        assert!(app.composer.text().ends_with("notes.txt"));
        app.perform(Action::Attach);
        assert_eq!(toast(&app), "Finish editing before attaching a file");

        // Logging out forgets files that were waiting.
        app.perform(Action::CancelCompose);
        app.on_event(Event::Paste(notes.display().to_string()));
        assert_eq!(app.attachments().len(), 1);
        app.on_matrix(MatrixEvent::NeedLogin { reason: None });
        app.modal = None;
        app.rooms = vec![Room::new("!a", "Alpha", RoomKind::Room)];
        assert!(app.attachments().is_empty());
        assert_eq!(app.attach.limit, None);
    }

    #[test]
    fn the_paperclip_and_the_demo() {
        let scratch = Scratch::new("app-demo");
        let notes = scratch.file("notes.txt", b"hello");
        let mut app = App::demo();
        let shown = screen(&mut app, 100, 30);
        let (column, row) = find(&shown, "📎");
        app.on_event(click(column, row));
        assert_eq!(app.composer.text(), "/upload ");
        type_text(&mut app, &notes.display().to_string());
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.attachments().len(), 1);
        type_text(&mut app, "for you");
        let before = app.rooms[0].messages.len();
        app.on_event(key(KeyCode::Enter));
        let messages = &app.rooms[0].messages;
        assert_eq!(messages.len(), before + 1);
        assert_eq!(messages[before].body, "📎 notes.txt · 5 B\nfor you");
        assert!(app.attachments().is_empty());
    }

    #[test]
    fn chips_on_screen() {
        let mut app = App::demo();
        let room = app.rooms[0].id.clone();
        let file = |name: &str, size: u64| Chosen {
            path: PathBuf::from(format!("/nowhere/{name}")),
            name: name.to_owned(),
            size,
        };
        let room = (room, None);
        app.attach.pending.insert(
            room.clone(),
            vec![
                file("holiday.jpg", 1_234_567),
                file(
                    "a rather long file name that will not fit into a narrow window.pdf",
                    88,
                ),
            ],
        );
        insta::assert_snapshot!("chips", screen(&mut app, 70, 14));
        app.composer.insert("With a caption");
        app.timeline.selected = Some(1);
        app.compose_mode = ComposeMode::Reply(1);
        app.attach.pending.get_mut(&room).unwrap().extend([
            file("three.txt", 3),
            file("four.txt", 4),
            file("five.txt", 5),
        ]);
        insta::assert_snapshot!("chips_many", screen(&mut app, 70, 16));
        // Nothing breaks when there is no room for them.
        for (width, height) in [(1, 1), (10, 4), (20, 6), (30, 3), (61, 5)] {
            screen(&mut app, width, height);
        }
    }

    #[test]
    fn nothing_is_pasted_behind_the_help_or_the_picture_viewer() {
        let scratch = Scratch::new("app-covered");
        let notes = scratch.file("notes.txt", b"hello");
        let mut app = App::demo();
        app.perform(Action::ToggleHelp);
        for paste in [notes.display().to_string(), "words".to_owned()] {
            app.on_event(Event::Paste(paste));
            assert!(app.attachments().is_empty());
            assert!(app.composer.is_empty());
        }
        app.perform(Action::ToggleHelp);

        // The demo's picture, opened in the viewer.
        app.open_room(4);
        assert!(app.open_image(1));
        assert!(app.images.viewer_open());
        let focus = app.focus;
        for paste in [notes.display().to_string(), "words".to_owned()] {
            app.on_event(Event::Paste(paste));
            assert!(app.attachments().is_empty());
            assert!(app.composer.is_empty());
            assert_eq!(app.focus, focus);
        }
        // Closed again, a drop attaches as ever.
        app.images.close_viewer();
        app.on_event(Event::Paste(notes.display().to_string()));
        assert_eq!(app.attachments().len(), 1);
    }

    #[test]
    fn files_and_upload_drafts_stay_with_their_room_or_thread() {
        let scratch = Scratch::new("app-thread");
        let in_room = scratch.file("room.txt", b"r");
        let in_thread = scratch.file("thread.txt", b"t");
        let (mut app, mut commands) = connected();

        // Attached in the room, with an `/upload` command begun over a draft.
        app.on_event(Event::Paste(in_room.display().to_string()));
        type_text(&mut app, "room draft");
        app.perform(Action::Attach);
        assert_eq!(app.composer.text(), "/upload ");

        // The thread shows none of it, and Enter there sends no file.
        app.open_thread(0);
        assert!(app.thread_open());
        assert!(app.attachments().is_empty());
        assert!(!screen(&mut app, 80, 24).contains("room.txt"));
        type_text(&mut app, "thread words");
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());

        // Attached in the thread; its own `/upload` draft is its own.
        app.on_event(Event::Paste(in_thread.display().to_string()));
        assert_eq!(paths(&app), std::slice::from_ref(&in_thread));
        type_text(&mut app, "thread draft");
        app.perform(Action::Attach);
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.composer.text(), "thread draft");

        // Back in the room: its file, its command, and its draft behind that.
        app.close_thread();
        assert_eq!(paths(&app), std::slice::from_ref(&in_room));
        assert!(!screen(&mut app, 80, 24).contains("thread.txt"));
        assert_eq!(app.composer.text(), "/upload ");
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.composer.text(), "room draft");
        while commands.try_recv().is_ok() {}
        app.on_event(key(KeyCode::Enter));
        // Not wrapped: it goes to the room.
        let Ok(MatrixCmd::SendFiles { files, .. }) = commands.try_recv() else {
            panic!("the room's file was not sent to the room");
        };
        assert_eq!(files, std::slice::from_ref(&in_room));
        assert!(app.attachments().is_empty());

        // The thread still has its file, and sends it into the thread.
        app.open_thread(0);
        assert_eq!(paths(&app), std::slice::from_ref(&in_thread));
        while commands.try_recv().is_ok() {}
        app.composer.take();
        app.on_event(key(KeyCode::Enter));
        let Ok(MatrixCmd::InThread { root, command }) = commands.try_recv() else {
            panic!("the files were not sent into the thread");
        };
        assert_eq!(root, "$one");
        let MatrixCmd::SendFiles { files, .. } = *command else {
            panic!("not a file command");
        };
        assert_eq!(files, std::slice::from_ref(&in_thread));
        app.close_thread();
        assert!(app.attachments().is_empty());
    }

    #[test]
    fn files_that_are_not_on_screen_are_not_sent() {
        let scratch = Scratch::new("app-short");
        let files: Vec<PathBuf> = ["a.txt", "b.txt", "c.txt"]
            .iter()
            .map(|name| scratch.file(name, b"x"))
            .collect();
        let (mut app, mut commands) = connected();
        for file in &files {
            app.on_event(Event::Paste(file.display().to_string()));
        }

        // One row for three files: the first by name, the others counted.
        let shown = screen(&mut app, 80, 5);
        assert!(shown.contains("📎 a.txt  +2 more"), "{shown}");
        assert!(!app.attach.hidden);

        // No row at all: Enter refuses and says why.
        let shown = screen(&mut app, 80, 4);
        assert!(!shown.contains("a.txt"), "{shown}");
        assert!(app.attach.hidden);
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());
        assert!(toast(&app).starts_with("Make the window taller"));
        assert_eq!(app.attachments().len(), 3);

        // Tall enough again, it goes.
        screen(&mut app, 80, 24);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(sent(&mut commands).len(), 1);
    }

    #[test]
    fn ctrl_c_drops_and_edits_around_the_upload_command() {
        let scratch = Scratch::new("app-edges");
        let notes = scratch.file("notes.txt", b"hello");
        let (mut app, mut commands) = connected();
        let ctrl_c = || Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

        // Ctrl-C on an `/upload` command forgets the draft that was put aside for it.
        type_text(&mut app, "old draft");
        app.perform(Action::Attach);
        app.on_event(ctrl_c());
        assert!(app.composer.is_empty());
        app.perform(Action::Attach);
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.composer.text(), "");

        // A file dropped while the command is being typed answers it.
        type_text(&mut app, "caption");
        app.perform(Action::Attach);
        type_text(&mut app, "/some/where");
        app.on_event(Event::Paste(notes.display().to_string()));
        assert_eq!(app.attachments().len(), 1);
        assert_eq!(app.composer.text(), "caption");
        app.composer.take();

        // An edit is sent as an edit although a file is waiting, and Tab and Esc
        // mean what they always mean while editing.
        let mut own = remote("mine", "typo");
        own.own = true;
        app.rooms[0].messages.push(own);
        app.timeline.selected = Some(2);
        app.perform(Action::Edit);
        app.composer.set_text("fixed");
        while commands.try_recv().is_ok() {}
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(commands.try_recv(), Ok(MatrixCmd::Edit { .. })));
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert_eq!(app.attachments().len(), 1);
        app.timeline.selected = Some(2);
        app.perform(Action::Edit);
        app.composer.set_text("/upload ");
        app.on_event(key(KeyCode::Tab));
        assert_eq!(app.composer.text(), "/upload ");
        app.set_focus(Pane::Composer);
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert!(sent(&mut commands).is_empty());

        // A message that is a file cannot be edited (its text is not the message).
        let mut file = remote("file", "📎 report.pdf · 1 kB");
        file.own = true;
        file.file = Some(crate::model::Attachment {
            key: "mxc://example.org/report".to_owned(),
            name: "report.pdf".to_owned(),
            size: Some(1000),
            kind: crate::model::MediaKind::File,
            source: Box::new(matrix_sdk::ruma::events::room::MediaSource::Plain(
                matrix_sdk::ruma::OwnedMxcUri::from("mxc://example.org/report"),
            )),
        });
        app.rooms[0].messages.push(file);
        app.timeline.selected = Some(3);
        app.perform(Action::Edit);
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert_eq!(toast(&app), "This message cannot be edited");
    }

    #[test]
    fn an_invitation_takes_no_files() {
        let scratch = Scratch::new("app-invite");
        let notes = scratch.file("notes.txt", b"hello");
        let (mut app, mut commands) = connected();
        app.rooms[0].kind = RoomKind::Invite;
        // A path is text there, and the command and the key say why not.
        app.on_event(Event::Paste(notes.display().to_string()));
        assert!(app.attachments().is_empty());
        assert_eq!(app.composer.text(), notes.display().to_string());
        app.composer.take();
        app.perform(Action::Attach);
        assert!(app.composer.is_empty());
        assert_eq!(toast(&app), "Accept the invitation first");
        app.composer
            .set_text(&format!("/upload {}", notes.display()));
        app.on_event(key(KeyCode::Enter));
        assert!(app.attachments().is_empty());
        assert!(sent(&mut commands).is_empty());
    }

    #[test]
    fn files_the_worker_could_not_send_come_back() {
        let scratch = Scratch::new("app-back");
        let notes = scratch.file("notes.txt", b"hello");
        let other = scratch.file("other.txt", b"hello");
        let (mut app, mut commands) = connected();
        app.on_event(Event::Paste(format!(
            "{}\n{}",
            notes.display(),
            other.display()
        )));
        type_text(&mut app, "the caption");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(sent(&mut commands).len(), 1);
        assert!(app.attachments().is_empty() && app.composer.is_empty());

        app.on_matrix(MatrixEvent::FilesNotSent {
            send: app.attach.last_send(),
            reason: "notes.txt does not exist".to_owned(),
        });
        assert_eq!(toast(&app), "notes.txt does not exist. Nothing was sent");
        assert_eq!(paths(&app), [notes.clone(), other.clone()]);
        assert_eq!(app.composer.text(), "the caption");
        // Only once, and never over something written since.
        app.composer.set_text("new words");
        app.on_matrix(MatrixEvent::FilesNotSent {
            send: app.attach.last_send(),
            reason: "again".to_owned(),
        });
        assert_eq!(app.composer.text(), "new words");
        assert_eq!(app.attachments().len(), 2);

        // Reported while another room is open: the files wait where they were.
        app.composer.take();
        app.on_event(key(KeyCode::Enter));
        assert_eq!(sent(&mut commands).len(), 1);
        app.open_room(1);
        app.on_matrix(MatrixEvent::FilesNotSent {
            send: app.attach.last_send(),
            reason: "no".to_owned(),
        });
        assert!(app.attachments().is_empty() && app.composer.is_empty());
        app.open_room(0);
        assert_eq!(app.attachments().len(), 2);
    }

    #[test]
    fn each_answer_gives_back_the_files_of_its_own_send() {
        let scratch = Scratch::new("app-two-sends");
        let first = scratch.file("first.txt", b"hello");
        let second = scratch.file("second.txt", b"hello");
        let (mut app, mut commands) = connected();
        app.on_event(Event::Paste(first.display().to_string()));
        app.on_event(key(KeyCode::Enter));
        let first_send = app.attach.last_send();
        app.open_room(1);
        app.focus = Pane::Composer;
        app.on_event(Event::Paste(second.display().to_string()));
        app.on_event(key(KeyCode::Enter));
        let second_send = app.attach.last_send();
        assert_ne!(first_send, second_send);
        assert_eq!(sent(&mut commands).len(), 2);

        // The second is done, then the first fails: the first's file comes back, to
        // the room it was sent in, and the second's does not.
        app.on_matrix(MatrixEvent::FilesSent(second_send));
        app.on_matrix(MatrixEvent::FilesNotSent {
            send: first_send,
            reason: "no".to_owned(),
        });
        assert!(app.attachments().is_empty());
        app.open_room(0);
        assert_eq!(paths(&app), std::slice::from_ref(&first));

        // An answer for a send that is not out (answered twice) changes nothing.
        app.on_matrix(MatrixEvent::FilesNotSent {
            send: second_send,
            reason: "late".to_owned(),
        });
        assert_eq!(paths(&app), std::slice::from_ref(&first));
        app.open_room(1);
        assert!(app.attachments().is_empty());
    }

    #[test]
    fn files_the_worker_has_taken_are_forgotten() {
        let scratch = Scratch::new("app-taken");
        let notes = scratch.file("notes.txt", b"hello");
        let (mut app, mut commands) = connected();
        app.on_event(Event::Paste(notes.display().to_string()));
        type_text(&mut app, "the caption");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(sent(&mut commands).len(), 1);
        app.on_matrix(MatrixEvent::FilesSent(app.attach.last_send()));
        // Every send is answered: nothing is kept to be given back.
        app.on_matrix(MatrixEvent::FilesNotSent {
            send: app.attach.last_send(),
            reason: "late".to_owned(),
        });
        assert!(app.attachments().is_empty());
        assert!(app.composer.is_empty());
    }
}
