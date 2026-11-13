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
            && let Some(path) = self.saved.get(&file.key)
            && path.exists()
        {
            super::open_in_browser(&path.to_string_lossy());
            return true;
        }
        if self.saving.contains(&file.key) {
            self.show_toast(format!("Still saving {}…", file.name));
            return true;
        }
        let large = file.size.filter(|size| *size > files::LARGE_FILE);
        if let Some(size) = large
            && self.confirm_large.as_deref() != Some(&file.key)
        {
            self.show_toast(format!(
                "{} is {}. Press s again to download it",
                file.name,
                human_size(size)
            ));
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
        self.saving.remove(&key);
        match result {
            Ok(path) => {
                self.show_toast(format!("Saved to {}", files::display(&path)));
                if open {
                    super::open_in_browser(&path.to_string_lossy());
                }
                self.saved.insert(key, path);
            }
            Err(err) => self.show_toast(format!("Could not save: {err}")),
        }
    }

    /// Enter on the download folder row finishes typing: the folder is remembered.
    pub(super) fn set_download_dir(&mut self, text: &str) {
        self.download_dir = text.trim().to_owned();
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

        // `o` on a file saves it and asks for it to be opened.
        let (mut app, mut commands) = app_with(vec![file_message("notes.txt", 10)]);
        app.timeline.selected = Some(0);
        app.on_event(key(KeyCode::Char('o')));
        let sent = saves(&mut commands);
        assert_eq!(sent.len(), 1);
        assert!(sent[0].2);
    }

    #[test]
    fn download_folder_is_typed_in_the_settings() {
        let (mut app, _commands) = app_with(Vec::new());
        app.perform(Action::OpenSettings);
        let row = crate::modal::Setting::ALL
            .iter()
            .position(|s| *s == crate::modal::Setting::DownloadDir)
            .unwrap();
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
