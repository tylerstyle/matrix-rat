//! Pasting a picture from the desktop's clipboard (`Action::PastePicture`).
//!
//! The action only records what is to be read (`attach.paste_request`): the run loop
//! reads the clipboard on a blocking thread (`clipboard::paste`) and hands the answer
//! back to [`App::finish_paste`], which a test can call with any outcome. The picture,
//! in a temporary file of ours, then goes the way of any attached file: a chip in the
//! composer, sent with Enter, or taken off again. The file stays as long as a chip or a
//! send needs it (`AttachState::sweep`) and is removed when it is dropped, at the
//! latest when the app ends.

use super::App;
use crate::action::Pane;
#[cfg(any(not(target_family = "wasm"), test))]
use crate::clipboard::Done;
use crate::clipboard::{Failure, Request, Tool};
#[cfg(any(not(target_family = "wasm"), test))]
use crate::upload;

impl App {
    /// `Action::PastePicture`.
    pub(super) fn request_paste_picture(&mut self) {
        self.request_paste_with(self.probes.clipboard_tools());
    }

    /// With the tools already worked out (a test passes its own).
    pub(super) fn request_paste_with(&mut self, tools: Vec<Tool>) {
        if !self.can_attach() {
            return;
        }
        let Some(scope) = self.attach_scope() else {
            return;
        };
        if self.attach.reading.is_some() {
            self.show_toast("Still reading the clipboard");
            return;
        }
        if tools.is_empty() {
            self.show_toast(Failure::NoDesktop.message());
            return;
        }
        self.set_focus(Pane::Composer);
        self.attach.readings += 1;
        let id = self.attach.readings;
        self.attach.reading = Some((id, scope));
        self.attach.paste_request = Some(Request {
            id,
            tools,
            limit: self.attach.limit,
        });
    }

    /// What the run loop is to read, once.
    #[cfg(any(not(target_family = "wasm"), test))]
    pub(crate) fn take_paste_request(&mut self) -> Option<Request> {
        self.attach.paste_request.take()
    }

    /// The clipboard was read: the picture is attached to the conversation it was asked
    /// for, as a file picked by hand would be, or a toast says why not. An answer to a
    /// reading that was given up (by a logout) is dropped, and its file with it.
    #[cfg(any(not(target_family = "wasm"), test))]
    pub(crate) fn finish_paste(&mut self, done: Done) {
        self.dirty = true;
        let Some((_, scope)) = self.attach.reading.take_if(|(id, _)| *id == done.id) else {
            return;
        };
        let picture = match done.result {
            Ok(picture) => picture,
            Err(failure) => return self.show_toast(failure.message()),
        };
        let file = match upload::check(picture.path()) {
            Ok(file) => file,
            Err(reason) => return self.show_toast(reason),
        };
        if self.attach_scope().as_ref() == Some(&scope) {
            if self.attach_files(vec![file]) == 0 {
                return;
            }
            // Like a dropped file, it answers an `/upload` command being typed.
            self.restore_draft_for(&scope);
        } else if self.rooms.iter().any(|room| room.id == scope.0) {
            // The user went elsewhere meanwhile: it waits where it was asked for.
            if let Some(reason) = upload::too_large(&file.name, file.size, self.attach.limit) {
                return self.show_toast(reason);
            }
            self.attach
                .pending
                .entry(scope.clone())
                .or_default()
                .push(file);
            self.restore_draft_for(&scope);
            self.show_toast("The picture waits in the chat you pasted it in");
        } else {
            // That room is gone.
            return;
        }
        self.attach.pasted.push(picture);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::{Path, PathBuf};

    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::action::Action;
    use crate::app::ComposeMode;
    use crate::clipboard::{self, Ran};
    use crate::input::{InputEvent, KeyCode, KeyEvent, KeyModifiers};
    use crate::matrix::{MatrixCmd, MatrixEvent, MatrixHandle};
    use crate::model::{ItemRef, Message, Room, RoomKind};
    use crate::upload::tests::Scratch;

    fn key(code: KeyCode) -> InputEvent {
        InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
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

    /// Connected to a fake worker, with two rooms, the first one open.
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

    fn png() -> Vec<u8> {
        let mut bytes = Vec::new();
        crate::images::demo_picture(64, 48)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    /// A Wayland clipboard holding `content` as a PNG.
    fn clipboard(content: Vec<u8>) -> impl FnMut(&[&str], u64, crate::time::Instant) -> Ran {
        move |argv, _, _| {
            if argv.contains(&"--list-types") {
                Ran::Output(b"text/html\nimage/png\n".to_vec())
            } else {
                Ran::Output(content.clone())
            }
        }
    }

    /// The whole flow with a clipboard that is a closure: what the run loop does, minus
    /// the thread. Returns the request, if one was made.
    fn paste_with(
        app: &mut App,
        dir: &Path,
        runner: impl FnMut(&[&str], u64, crate::time::Instant) -> Ran,
    ) -> Option<Request> {
        app.request_paste_with(vec![Tool::WlPaste]);
        let request = app.take_paste_request()?;
        let result = clipboard::paste(&request, dir, runner);
        app.finish_paste(Done {
            id: request.id,
            result,
        });
        Some(request)
    }

    fn paste(app: &mut App, dir: &Path) {
        paste_with(app, dir, clipboard(png())).expect("a request");
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

    /// The one file attached in the open conversation.
    fn attached(app: &App) -> PathBuf {
        let [path] = paths(app).try_into().expect("one attached file");
        path
    }

    /// Paths of the files the commands sent so far ask to send, in a room or a thread.
    fn sent_files(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<PathBuf> {
        let mut files = Vec::new();
        while let Ok(command) = commands.try_recv() {
            let command = match command {
                MatrixCmd::InThread { command, .. } => *command,
                command => command,
            };
            if let MatrixCmd::SendFiles { files: sent, .. } = command {
                files.extend(sent.into_iter().map(|file| file.path));
            }
        }
        files
    }

    #[test]
    fn a_pasted_picture_is_attached_and_its_file_goes_once_the_worker_has_it() {
        let dir = Scratch::new("paste-sent");
        let (mut app, mut commands) = connected();
        app.composer.insert("look");
        app.perform(Action::Focus(Pane::Sidebar));
        paste(&mut app, &dir.0);
        // A chip like any attached file, in the composer, with the draft as it was.
        let path = attached(&app);
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        assert!(
            name.starts_with("pasted-") && name.ends_with(".png"),
            "{name}"
        );
        assert!(path.starts_with(&dir.0));
        assert_eq!(std::fs::read(&path).unwrap(), png());
        assert_eq!(app.focus, Pane::Composer);
        assert_eq!(app.composer.text(), "look");
        assert!(sent_files(&mut commands).is_empty());

        // Enter sends it with the caption; the worker reads it later, so it stays.
        app.on_event(key(KeyCode::Enter));
        assert_eq!(sent_files(&mut commands), std::slice::from_ref(&path));
        assert!(app.attachments().is_empty());
        assert!(path.exists());
        app.on_matrix(MatrixEvent::FilesSent(app.attach.last_send()));
        assert!(!path.exists());
    }

    #[test]
    fn a_picture_answers_an_upload_command_being_typed() {
        let dir = Scratch::new("paste-command");
        let (mut app, _commands) = connected();
        app.composer.insert("my caption");
        app.perform(Action::Attach);
        assert_eq!(app.composer.text(), "/upload ");
        paste(&mut app, &dir.0);
        assert!(attached(&app).exists());
        assert_eq!(app.composer.text(), "my caption");
    }

    #[test]
    fn a_picture_taken_off_again_is_removed() {
        let dir = Scratch::new("paste-cancel");
        let (mut app, mut commands) = connected();
        // Backspace in an empty message takes it off.
        paste(&mut app, &dir.0);
        let path = attached(&app);
        app.on_event(key(KeyCode::Backspace));
        assert!(app.attachments().is_empty());
        assert!(!path.exists());
        // Ctrl-C clears the draft and what is attached to it.
        paste(&mut app, &dir.0);
        let path = attached(&app);
        app.composer.insert("caption");
        app.on_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert!(!path.exists());
        // Leaving the room forgets what waited there.
        paste(&mut app, &dir.0);
        let path = attached(&app);
        app.attach.forget_room("!a");
        assert!(!path.exists());
        assert!(sent_files(&mut commands).is_empty());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
    }

    #[test]
    fn a_picture_the_worker_could_not_send_comes_back_with_its_file() {
        let dir = Scratch::new("paste-back");
        let (mut app, mut commands) = connected();
        paste(&mut app, &dir.0);
        let path = attached(&app);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(sent_files(&mut commands), std::slice::from_ref(&path));
        app.on_matrix(MatrixEvent::FilesNotSent {
            send: app.attach.last_send(),
            reason: "no".to_owned(),
        });
        assert_eq!(attached(&app), path);
        assert!(path.exists());

        // Taken off while another send is out: it goes when that one is answered.
        let notes = dir.file("notes.txt", b"hello");
        app.open_room(1);
        app.on_event(InputEvent::Paste(notes.display().to_string()));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(sent_files(&mut commands), std::slice::from_ref(&notes));
        app.open_room(0);
        app.perform(Action::Focus(Pane::Composer));
        app.on_event(key(KeyCode::Backspace));
        assert!(app.attachments().is_empty());
        assert!(path.exists());
        app.on_matrix(MatrixEvent::FilesSent(app.attach.last_send()));
        assert!(!path.exists());
        // A file of the user's own is never removed.
        assert!(notes.exists());
    }

    #[test]
    fn several_sends_out_keep_their_pictures_until_all_are_answered() {
        let dir = Scratch::new("paste-several");
        let (mut app, mut commands) = connected();
        paste(&mut app, &dir.0);
        let first = attached(&app);
        app.on_event(key(KeyCode::Enter));
        paste(&mut app, &dir.0);
        let second = attached(&app);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(sent_files(&mut commands), [first.clone(), second.clone()]);
        // While one send is out, no picture goes: the later answer may come first.
        let last = app.attach.last_send();
        app.on_matrix(MatrixEvent::FilesSent(last));
        assert!(first.exists() && second.exists());
        app.on_matrix(MatrixEvent::FilesSent(last - 1));
        assert!(!first.exists() && !second.exists());
    }

    #[test]
    fn in_a_thread_the_picture_goes_into_the_thread() {
        let dir = Scratch::new("paste-thread");
        let (mut app, mut commands) = connected();
        app.open_thread(0);
        assert!(app.thread_open());
        paste(&mut app, &dir.0);
        let path = attached(&app);
        // Not shown in the room.
        app.close_thread();
        assert!(app.attachments().is_empty());
        app.open_thread(0);
        assert_eq!(attached(&app), path);
        while commands.try_recv().is_ok() {}
        app.on_event(key(KeyCode::Enter));
        let Ok(MatrixCmd::InThread { root, command }) = commands.try_recv() else {
            panic!("the picture was not sent into the thread");
        };
        assert_eq!(root, "$one");
        let MatrixCmd::SendFiles { files, .. } = *command else {
            panic!("not a file command");
        };
        assert_eq!(files, std::slice::from_ref(&path));
        app.on_matrix(MatrixEvent::FilesSent(app.attach.last_send()));
        assert!(!path.exists());
    }

    #[test]
    fn the_demo_shows_the_picture_as_sent_and_removes_its_file() {
        let dir = Scratch::new("paste-demo");
        let mut app = App::demo();
        paste(&mut app, &dir.0);
        let path = attached(&app);
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        app.composer.insert("from the clipboard");
        let before = app.rooms[0].messages.len();
        app.on_event(key(KeyCode::Enter));
        let messages = &app.rooms[0].messages;
        assert_eq!(messages.len(), before + 1);
        assert!(
            messages[before].body.starts_with(&format!("📎 {name} · ")),
            "{}",
            messages[before].body
        );
        assert!(messages[before].body.ends_with("\nfrom the clipboard"));
        assert!(!path.exists());
    }

    #[test]
    fn its_file_is_removed_when_the_app_ends() {
        let dir = Scratch::new("paste-exit");
        let (mut app, _commands) = connected();
        paste(&mut app, &dir.0);
        let waiting = attached(&app);
        // Also one that is out with the worker.
        app.open_room(1);
        paste(&mut app, &dir.0);
        let out = attached(&app);
        app.on_event(key(KeyCode::Enter));
        assert!(waiting.exists() && out.exists());
        drop(app);
        assert!(!waiting.exists() && !out.exists());
    }

    #[test]
    fn what_goes_wrong_is_said_and_nothing_is_attached() {
        let dir = Scratch::new("paste-wrong");
        let (mut app, _commands) = connected();
        for (failure, said) in [
            (Failure::NoPicture, "No picture in the clipboard"),
            (
                Failure::Missing(Tool::WlPaste),
                "Install wl-clipboard to paste pictures",
            ),
            (Failure::TimedOut, "The clipboard did not answer in time"),
        ] {
            app.request_paste_with(vec![Tool::WlPaste]);
            let request = app.take_paste_request().unwrap();
            app.finish_paste(Done {
                id: request.id,
                result: Err(failure),
            });
            assert_eq!(toast(&app), said);
            assert!(app.attachments().is_empty());
        }
        // A real reading that finds no picture says the same.
        paste_with(&mut app, &dir.0, |argv: &[&str], _, _| {
            if argv.contains(&"--list-types") {
                Ran::Output(b"text/plain\n".to_vec())
            } else {
                Ran::Output(b"words".to_vec())
            }
        });
        assert_eq!(toast(&app), "No picture in the clipboard");
        // So does one that is not really a picture.
        paste_with(
            &mut app,
            &dir.0,
            clipboard(b"\x89PNG\r\n\x1a\nnot really".to_vec()),
        );
        assert_eq!(toast(&app), "What the clipboard holds is not a picture");
        assert!(app.attachments().is_empty());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);

        // No desktop: said at once, and nothing is run.
        app.request_paste_with(Vec::new());
        assert!(app.take_paste_request().is_none());
        assert!(toast(&app).starts_with("There is no desktop clipboard"));
        // One reading at a time.
        app.request_paste_with(vec![Tool::Xclip]);
        app.request_paste_with(vec![Tool::Xclip]);
        assert_eq!(toast(&app), "Still reading the clipboard");
        assert!(app.take_paste_request().is_some());
        assert!(app.take_paste_request().is_none());
    }

    #[test]
    fn it_is_refused_where_no_file_can_be_attached() {
        let (mut app, _commands) = connected();
        // While a message is being edited.
        let mut own = remote("mine", "typo");
        own.own = true;
        app.rooms[0].messages.push(own);
        app.timeline.selected = Some(2);
        app.perform(Action::Edit);
        assert!(matches!(app.compose_mode, ComposeMode::Edit(2)));
        app.request_paste_with(vec![Tool::WlPaste]);
        assert!(app.take_paste_request().is_none());
        assert_eq!(toast(&app), "Finish editing before attaching a file");
        app.perform(Action::CancelCompose);
        // In an invitation.
        app.rooms[0].kind = RoomKind::Invite;
        app.request_paste_with(vec![Tool::WlPaste]);
        assert!(app.take_paste_request().is_none());
        assert_eq!(toast(&app), "Accept the invitation first");
    }

    #[test]
    fn a_picture_asked_for_in_one_chat_waits_there() {
        let dir = Scratch::new("paste-elsewhere");
        let (mut app, _commands) = connected();
        app.request_paste_with(vec![Tool::WlPaste]);
        let request = app.take_paste_request().unwrap();
        // The user goes to another room while the clipboard is read.
        app.open_room(1);
        let result = clipboard::paste(&request, &dir.0, clipboard(png()));
        app.finish_paste(Done {
            id: request.id,
            result,
        });
        assert!(app.attachments().is_empty());
        assert_eq!(
            toast(&app),
            "The picture waits in the chat you pasted it in"
        );
        app.open_room(0);
        assert!(attached(&app).exists());

        // A room left meanwhile takes nothing, and the file goes.
        app.open_room(1);
        app.request_paste_with(vec![Tool::WlPaste]);
        let request = app.take_paste_request().unwrap();
        app.open_room(0);
        app.rooms.truncate(1);
        let result = clipboard::paste(&request, &dir.0, clipboard(png()));
        app.finish_paste(Done {
            id: request.id,
            result,
        });
        assert_eq!(app.attachments().len(), 1);
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    #[test]
    fn an_answer_from_before_a_logout_is_dropped_with_its_file() {
        let dir = Scratch::new("paste-logout");
        let (mut app, _commands) = connected();
        app.request_paste_with(vec![Tool::WlPaste]);
        let request = app.take_paste_request().unwrap();
        app.on_matrix(MatrixEvent::NeedLogin { reason: None });
        app.modal = None;
        app.rooms = vec![Room::new("!a", "Alpha", RoomKind::Room)];
        // A new reading begins before the old one answers.
        app.request_paste_with(vec![Tool::WlPaste]);
        let new = app.take_paste_request().unwrap();
        assert_ne!(new.id, request.id);
        let result = clipboard::paste(&request, &dir.0, clipboard(png()));
        app.finish_paste(Done {
            id: request.id,
            result,
        });
        assert!(app.attachments().is_empty());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
        // The new one's answer is taken.
        let result = clipboard::paste(&new, &dir.0, clipboard(png()));
        app.finish_paste(Done { id: new.id, result });
        assert_eq!(app.attachments().len(), 1);
    }

    #[test]
    fn the_limit_the_server_said_is_the_one_the_clipboard_is_held_to() {
        let (mut app, _commands) = connected();
        app.on_matrix(MatrixEvent::UploadLimit(5000));
        app.request_paste_with(vec![Tool::WlPaste]);
        assert_eq!(app.take_paste_request().unwrap().limit, Some(5000));
    }

    #[test]
    fn the_key_works_in_the_composer_only() {
        let alt_i = || InputEvent::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::ALT));
        // What it does depends on the desktop of the machine running the test: a
        // request, or the toast that says there is no clipboard. Nothing is run.
        let (mut app, _commands) = connected();
        app.perform(Action::Focus(Pane::Composer));
        app.on_event(alt_i());
        assert!(
            app.attach.paste_request.is_some()
                || toast(&app).starts_with("There is no desktop clipboard")
        );
        let (mut app, _commands) = connected();
        app.perform(Action::Focus(Pane::Sidebar));
        app.on_event(alt_i());
        assert!(app.attach.paste_request.is_none() && app.toast.is_none());
    }

    #[test]
    fn paste_finishing_after_room_or_thread_switch_restores_stashed_draft() {
        let dir = Scratch::new("paste-switch-stash");
        let (mut app, _commands) = connected();
        app.rooms[0].messages.push(remote("m1", "root"));

        // Stash a draft via /upload, start a paste, then switch to another room.
        app.composer.set_text("my caption");
        app.perform(Action::Attach);
        assert_eq!(app.composer.text(), "/upload ");
        app.request_paste_with(vec![Tool::WlPaste]);
        let request = app.take_paste_request().unwrap();
        app.open_room(1);
        assert_eq!(app.rooms[0].draft, "/upload ");

        let result = clipboard::paste(&request, &dir.0, clipboard(png()));
        app.finish_paste(Done {
            id: request.id,
            result,
        });
        assert_eq!(app.rooms[0].draft, "my caption");
        app.open_room(0);
        assert_eq!(app.composer.text(), "my caption");

        // Same when the user opened a thread in the same room before paste finished.
        app.perform(Action::Attach);
        assert_eq!(app.composer.text(), "/upload ");
        app.request_paste_with(vec![Tool::WlPaste]);
        let request = app.take_paste_request().unwrap();
        app.open_thread(0);
        let result = clipboard::paste(&request, &dir.0, clipboard(png()));
        app.finish_paste(Done {
            id: request.id,
            result,
        });
        app.close_thread();
        assert_eq!(app.composer.text(), "my caption");
    }
}
