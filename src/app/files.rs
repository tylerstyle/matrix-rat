//! Saving attachments (files, and the originals of pictures) to the download folder.

use std::path::PathBuf;

use super::App;
use crate::files;
use crate::images::human_size;
use crate::matrix::MatrixCmd;
use crate::model::Attachment;

impl App {
    /// What `s` would save right now: the picture in the viewer, else the selected
    /// message's file or picture.
    fn current_attachment(&self) -> Option<Attachment> {
        if let Some(info) = self.images.viewer_info() {
            return info.attachment();
        }
        let index = self.timeline.selected?;
        self.messages().get(index)?.attachment()
    }

    /// Whether the status line should offer saving.
    pub fn can_save(&self) -> bool {
        self.current_attachment().is_some()
    }

    pub fn download_dir(&self) -> PathBuf {
        files::resolve_dir(&self.download_dir)
    }

    /// Saves the current attachment; with `open` it is then handed to the desktop's
    /// program for it. False if there is nothing to save.
    pub(super) fn save_attachment(&mut self, open: bool) -> bool {
        let Some(file) = self.current_attachment() else {
            return false;
        };
        if self.matrix.is_none() {
            self.show_toast("Not connected to an account");
            return true;
        }
        if open
            && let Some(path) = self.saved.get(&file.key).cloned()
            && path.exists()
        {
            self.open_saved(&path);
            return true;
        }
        if self.saving.contains(&file.key) {
            self.show_toast(format!("Still saving {}…", file.name));
            return true;
        }
        // The whole file passes through memory, and the size is only the sender's
        // word: ask once for large files and for those that do not state a size.
        let again = if open {
            self.keymap
                .last_label_in(
                    crate::keymap::Context::Timeline,
                    crate::action::Action::Open,
                )
                .or_else(|| self.keymap.label_for(crate::action::Action::Open))
                .unwrap_or("this action")
        } else {
            self.keymap
                .label_in(
                    crate::keymap::Context::Timeline,
                    crate::action::Action::Save,
                )
                .or_else(|| self.keymap.label_for(crate::action::Action::Save))
                .unwrap_or("this action")
        };
        let question = match file.size {
            Some(size) if size > files::LARGE_FILE => Some(format!(
                "{} is {}. Press {again} again to download it",
                file.name,
                human_size(size)
            )),
            Some(_) => None,
            None => Some(format!(
                "{} does not say how large it is. Press {again} again to download it",
                file.name
            )),
        };
        if let Some(question) = question
            && self.confirm_large.as_deref() != Some(&file.key)
        {
            self.show_toast(question);
            self.confirm_large = Some(file.key);
            return true;
        }
        self.confirm_large = None;
        self.saving.insert(file.key.clone());
        self.show_toast(format!("Saving {}…", file.name));
        self.send_matrix(MatrixCmd::SaveFile {
            file,
            dir: self.download_dir(),
            open,
        });
        true
    }

    pub(super) fn on_file_saved(
        &mut self,
        key: String,
        open: bool,
        result: Result<PathBuf, String>,
    ) {
        // Not ours any more (logged out since): the file stays, nothing is shown or opened.
        if !self.saving.remove(&key) {
            return;
        }
        match result {
            Ok(path) => {
                self.show_toast(format!("Saved to {}", files::display(&path)));
                if open {
                    self.open_saved(&path);
                }
                self.saved.insert(key, path);
            }
            Err(err) => self.show_toast(format!("Could not save: {err}")),
        }
    }

    /// Hands a saved file to the desktop's program for it, if its type is one that only
    /// gets shown or played. Anything else stays saved and is named, not launched.
    fn open_saved(&mut self, path: &std::path::Path) {
        if files::opens_safely(path) {
            super::open_in_browser(&path.to_string_lossy());
        } else {
            self.show_toast(format!(
                "Saved to {} (this file type is not opened automatically)",
                files::display(path)
            ));
        }
    }

    /// Enter on the download folder row finishes typing: the folder is remembered.
    pub(super) fn set_download_dir(&mut self, text: &str) {
        self.download_dir = text.trim().to_owned();
        self.saved.clear();
        self.save_setting("download_dir", self.download_dir.clone());
        self.show_toast(format!(
            "Files are saved to {}",
            files::display(&self.download_dir())
        ));
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use matrix_sdk::ruma::OwnedMxcUri;
    use matrix_sdk::ruma::events::room::MediaSource;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::action::{Action, Pane};
    use crate::matrix::{MatrixEvent, MatrixHandle};
    use crate::modal::Modal;
    use crate::model::{ItemRef, Message, Room, RoomKind};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn file_message(name: &str, size: u64) -> Message {
        let key = format!("mxc://example.org/{name}");
        Message {
            id: name.to_owned(),
            item: Some(ItemRef::Event(format!("${name}"))),
            sender: "ann".to_owned(),
            body: format!("📎 {name}"),
            file: Some(Attachment {
                key: key.clone(),
                name: name.to_owned(),
                size: Some(size),
                source: Box::new(MediaSource::Plain(OwnedMxcUri::from(key))),
            }),
            ..Message::default()
        }
    }

    fn app_with(messages: Vec<Message>) -> (App, UnboundedReceiver<MatrixCmd>) {
        let mut app = App::demo();
        let (handle, commands) = MatrixHandle::for_test();
        app.matrix = Some(handle);
        let mut room = Room::new("!a", "Alpha", RoomKind::Room);
        room.messages = messages;
        app.rooms = vec![room];
        app.active = 0;
        app.focus = Pane::Timeline;
        (app, commands)
    }

    fn saves(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<(String, PathBuf, bool)> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            if let MatrixCmd::SaveFile { file, dir, open } = command {
                seen.push((file.name, dir, open));
            }
        }
        seen
    }

    fn toast(app: &App) -> &str {
        app.toast.as_ref().map_or("", |(text, _)| text.as_str())
    }

    #[test]
    fn s_saves_the_selected_file_once() {
        let mut text = file_message("plain", 0);
        text.file = None;
        let (mut app, mut commands) = app_with(vec![text, file_message("report.pdf", 5000)]);
        app.download_dir = "/srv/inbox".to_owned();

        app.timeline.selected = Some(0);
        assert!(!app.can_save());
        app.on_event(key(KeyCode::Char('s')));
        assert!(saves(&mut commands).is_empty());
        assert!(toast(&app).starts_with("Nothing to save"));

        app.timeline.selected = Some(1);
        assert!(app.can_save());
        app.on_event(key(KeyCode::Char('s')));
        assert_eq!(
            saves(&mut commands),
            [("report.pdf".to_owned(), PathBuf::from("/srv/inbox"), false)]
        );
        // Pressing again while it is on its way does not download it twice.
        app.on_event(key(KeyCode::Char('s')));
        assert!(saves(&mut commands).is_empty());

        app.on_matrix(MatrixEvent::FileSaved {
            key: "mxc://example.org/report.pdf".to_owned(),
            open: false,
            result: Ok(PathBuf::from("/srv/inbox/report.pdf")),
        });
        assert_eq!(toast(&app), "Saved to /srv/inbox/report.pdf");
        // Saving again is allowed and makes a second copy.
        app.on_event(key(KeyCode::Char('s')));
        assert_eq!(saves(&mut commands).len(), 1);
        app.on_matrix(MatrixEvent::FileSaved {
            key: "mxc://example.org/report.pdf".to_owned(),
            open: false,
            result: Err("the download failed".to_owned()),
        });
        assert_eq!(toast(&app), "Could not save: the download failed");
    }

    #[test]
    fn large_files_ask_first_and_o_opens_after_saving() {
        let (mut app, mut commands) = app_with(vec![file_message("film.mkv", 3_000_000_000)]);
        app.timeline.selected = Some(0);
        app.perform(Action::Save);
        assert!(saves(&mut commands).is_empty());
        assert!(toast(&app).contains("3.0 GB"), "{}", toast(&app));
        app.perform(Action::Save);
        assert_eq!(saves(&mut commands).len(), 1);
        // A file that does not state its size is asked about too.
        let mut unknown = file_message("blob.bin", 0);
        unknown.file.as_mut().unwrap().size = None;
        let (mut app, mut commands) = app_with(vec![unknown]);
        app.timeline.selected = Some(0);
        app.perform(Action::Save);
        assert!(saves(&mut commands).is_empty());
        app.perform(Action::Save);
        assert_eq!(saves(&mut commands).len(), 1);
        // A result that arrives after logging out opens nothing and says nothing.
        app.reset_session();
        app.toast = None;
        app.on_matrix(MatrixEvent::FileSaved {
            key: "mxc://example.org/blob.bin".to_owned(),
            open: true,
            result: Ok(PathBuf::from("/tmp/blob.bin")),
        });
        assert!(app.toast.is_none());
        assert!(app.saved.is_empty());

        // `o` on a file saves it and asks for it to be opened.
        let (mut app, mut commands) = app_with(vec![file_message("notes.txt", 10)]);
        app.timeline.selected = Some(0);
        app.on_event(key(KeyCode::Char('o')));
        let sent = saves(&mut commands);
        assert_eq!(sent.len(), 1);
        assert!(sent[0].2);
        // Opening a file wins over a link in its caption.
        let mut with_link = file_message("notes.txt", 10);
        with_link.body.push_str("\nsee https://example.org");
        let (mut app, mut commands) = app_with(vec![with_link]);
        app.timeline.selected = Some(0);
        app.on_event(key(KeyCode::Char('o')));
        assert_eq!(saves(&mut commands).len(), 1);
        // A launcher is saved but not started.
        app.on_matrix(MatrixEvent::FileSaved {
            key: "mxc://example.org/notes.txt".to_owned(),
            open: true,
            result: Ok(PathBuf::from("/tmp/evil.desktop")),
        });
        assert!(
            toast(&app).contains("not opened automatically"),
            "{}",
            toast(&app)
        );
    }

    #[test]
    fn download_folder_is_typed_in_the_settings() {
        let (mut app, _commands) = app_with(Vec::new());
        app.perform(Action::OpenSettings);
        let row = match &app.modal {
            Some(Modal::Settings(m)) => crate::modal::Setting::ALL
                .iter()
                .filter(|s| m.shows(**s))
                .position(|s| *s == crate::modal::Setting::DownloadDir)
                .unwrap(),
            _ => unreachable!(),
        };
        for _ in 0..row {
            app.on_event(key(KeyCode::Down));
        }
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(&app.modal, Some(Modal::Settings(m)) if m.editing.is_some()));
        app.on_event(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        for c in "~/inbox".chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        // Esc leaves the old folder; typing again and Enter sets the new one.
        app.on_event(key(KeyCode::Esc));
        assert!(matches!(&app.modal, Some(Modal::Settings(m)) if m.editing.is_none()));
        assert_eq!(app.download_dir, "~/Downloads");
        app.on_event(key(KeyCode::Enter));
        app.on_event(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        app.on_event(Event::Paste("~/inbox\nsecond line".to_owned()));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.download_dir, "~/inbox");
        assert!(app.download_dir().ends_with("inbox"));
        assert!(app.download_dir().is_absolute());
    }
}
