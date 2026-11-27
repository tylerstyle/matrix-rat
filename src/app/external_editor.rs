//! Writing the draft in `$VISUAL` / `$EDITOR` (`Action::ComposeInEditor`).
//!
//! The action only records what is to be done (`editor_request`): the run loop owns the
//! terminal, and hands it over for as long as the editor runs (`terminal::release`, then
//! `terminal::resume`, as for Ctrl-Z). The app does not read the keyboard meanwhile; the
//! Matrix worker is a task of its own and keeps syncing, its events wait in the shell
//! (`shell::terminal::external_editor`) and are handled when the editor is done.
//! The result comes back through [`App::finish_editor`], which a test can call with any
//! outcome.

use super::App;
use crate::action::Pane;
#[cfg(any(not(target_family = "wasm"), test))]
use crate::editor::Outcome;
use crate::editor::{Failure, Request};

impl App {
    /// `Action::ComposeInEditor`.
    pub(super) fn request_editor(&mut self) {
        self.request_editor_with(self.probes.editor());
    }

    /// With the editor command already worked out (a test passes its own).
    pub(super) fn request_editor_with(&mut self, command: Result<Vec<String>, Failure>) {
        if self.room().is_none_or(|room| room.is_pending()) {
            self.show_toast("There is no chat to write in");
            return;
        }
        match command {
            Ok(argv) => {
                // No microphone stays open behind an editor occupying the terminal.
                self.stop_recording();
                self.set_focus(Pane::Composer);
                self.editor_request = Some(Request {
                    argv,
                    text: self.composer.text().to_owned(),
                });
            }
            Err(failure) => self.show_toast(failure.message()),
        }
    }

    /// What the run loop is to start, once.
    #[cfg(any(not(target_family = "wasm"), test))]
    pub(crate) fn take_editor_request(&mut self) -> Option<Request> {
        self.editor_request.take()
    }

    /// The editor is done: its text goes into the composer (not sent), or the draft stays
    /// as it was and a toast says why.
    #[cfg(any(not(target_family = "wasm"), test))]
    pub(crate) fn finish_editor(&mut self, outcome: Outcome) {
        self.dirty = true;
        match outcome {
            Outcome::Failed(failure) => self.show_toast(failure.message()),
            Outcome::Unchanged => {}
            Outcome::Text(text) => {
                self.set_focus(Pane::Composer);
                // Through `insert`: tabs and control characters are dealt with as for
                // anything typed or pasted.
                self.composer.take();
                self.composer.insert(&text);
                // Where a mention stood is not known any more: a mention completed
                // before is plain text now, and is sent as such unless it is completed
                // again (as with a draft restored after a restart). An edit that had
                // mentions still clears them on the server.
                let edit_had_mentions = self.complete.edit_had_mentions;
                self.clear_completion_marks();
                self.complete.edit_had_mentions = edit_had_mentions;
                self.update_typing();
                // Counts as a change of the draft, like anything typed.
                self.note_draft();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::action::Action;
    use crate::app::ComposeMode;
    use crate::complete::MentionMark;
    use crate::input::{InputEvent, KeyCode, KeyEvent, KeyModifiers};
    use crate::matrix::{MatrixCmd, MatrixHandle};
    use crate::model::{ItemRef, Message, Room, RoomKind};

    fn message(n: usize, own: bool) -> Message {
        Message {
            id: n.to_string(),
            item: Some(ItemRef::Event(format!("$e{n}"))),
            sender: if own { "me" } else { "ann" }.to_owned(),
            body: format!("line {n}"),
            own,
            ..Message::default()
        }
    }

    /// Connected to a fake worker, the composer has the focus.
    fn connected() -> (App, UnboundedReceiver<MatrixCmd>) {
        let mut app = App::demo();
        let (handle, commands) = MatrixHandle::for_test();
        app.matrix = Some(handle);
        let mut alpha = Room::new("!a", "Alpha", RoomKind::Room);
        alpha.messages = vec![message(0, false), message(1, true)];
        app.rooms = vec![alpha, Room::new("!b", "Beta", RoomKind::Room)];
        app.active = 0;
        app.screen = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.perform(Action::Focus(Pane::Composer));
        (app, commands)
    }

    fn argv() -> Result<Vec<String>, Failure> {
        Ok(vec!["fake-editor".to_owned()])
    }

    /// The whole flow with an editor that is a closure: what the run loop does, minus
    /// the terminal. Returns what the editor was given; `None` from the closure is an
    /// editor that fails.
    fn write_in_editor(app: &mut App, editor: impl FnOnce(&str) -> Option<String>) -> String {
        app.request_editor_with(argv());
        let request = app.take_editor_request().expect("a request");
        let dir = std::env::temp_dir().join(format!("matrix-rat-app-{}", std::process::id()));
        let given = request.text.clone();
        let outcome = crate::editor::edit(&request, &dir, |_, path| {
            let before = std::fs::read_to_string(path).unwrap();
            match editor(&before) {
                Some(text) => std::fs::write(path, text).map(|()| true),
                None => Ok(false),
            }
        });
        app.finish_editor(outcome);
        let _ = std::fs::remove_dir(&dir);
        given
    }

    fn saves(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            if let MatrixCmd::SaveDraft { room_id, text } = command {
                seen.push(format!("{room_id}={text}"));
            }
        }
        seen
    }

    #[test]
    fn the_draft_goes_out_and_the_result_comes_back_unsent() {
        let (mut app, mut commands) = connected();
        app.composer.insert("first line\nsecond");
        let given = write_in_editor(&mut app, |seen| Some(format!("{seen} and more\n")));
        assert_eq!(given, "first line\nsecond");
        assert_eq!(app.composer.text(), "first line\nsecond and more");
        // At the end, ready to go on; nothing was sent.
        assert_eq!(app.composer.cursor(), app.composer.text().len());
        assert_eq!(app.focus, Pane::Composer);
        assert!(app.editor_request.is_none());
        while let Ok(command) = commands.try_recv() {
            assert!(!format!("{command:?}").starts_with("Send"), "{command:?}");
        }
    }

    #[test]
    fn from_anywhere_it_writes_in_the_composer() {
        let (mut app, _commands) = connected();
        app.perform(Action::Focus(Pane::Timeline));
        write_in_editor(&mut app, |_| Some("hello".to_owned()));
        assert_eq!(app.focus, Pane::Composer);
        assert_eq!(app.composer.text(), "hello");
    }

    #[test]
    fn the_key_works_in_the_composer_only() {
        let alt_v = || InputEvent::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT));
        // What it does depends on $VISUAL / $EDITOR of the machine running the test: a
        // request, or the toast that says how to set one. Either way something happens.
        let (mut app, _commands) = connected();
        app.on_event(alt_v());
        assert!(
            app.editor_request.is_some()
                || app
                    .toast
                    .as_ref()
                    .is_some_and(|(text, _)| text.contains("$EDITOR"))
        );
        let (mut app, _commands) = connected();
        app.perform(Action::Focus(Pane::Timeline));
        app.on_event(alt_v());
        assert!(app.editor_request.is_none() && app.toast.is_none());
    }

    #[test]
    fn no_editor_means_a_hint_and_no_guess() {
        let (mut app, _commands) = connected();
        app.composer.insert("keep");
        app.request_editor_with(crate::editor::command(None, None));
        assert!(app.editor_request.is_none());
        let toast = &app.toast.as_ref().unwrap().0;
        assert!(
            toast.contains("$VISUAL") && toast.contains("$EDITOR"),
            "{toast}"
        );
        assert_eq!(app.composer.text(), "keep");
    }

    #[test]
    fn a_failed_editor_leaves_the_draft_and_says_so() {
        let (mut app, _commands) = connected();
        app.composer.insert("keep me");
        write_in_editor(&mut app, |_| None);
        assert_eq!(app.composer.text(), "keep me");
        assert!(app.toast.as_ref().unwrap().0.contains("draft is as it was"));
    }

    #[test]
    fn emptying_the_file_empties_the_draft_but_nothing_from_nothing() {
        let (mut app, mut commands) = connected();
        app.composer.insert("gone");
        app.note_draft();
        write_in_editor(&mut app, |_| Some(" \n".to_owned()));
        assert_eq!(app.composer.text(), "");
        app.save_idle_draft(Duration::ZERO);
        assert_eq!(saves(&mut commands), ["!a="]);

        write_in_editor(&mut app, |_| Some(String::new()));
        assert_eq!(app.composer.text(), "");
        assert!(app.toast.is_none());
    }

    #[test]
    fn control_characters_are_dealt_with_as_for_anything_typed() {
        let (mut app, _commands) = connected();
        write_in_editor(&mut app, |_| Some("a\u{1b}[2Jb\tc\r\nd\u{7}".to_owned()));
        assert_eq!(app.composer.text(), "a[2Jb    c\nd");
    }

    #[test]
    fn mentions_become_plain_text_and_the_draft_counts_as_changed() {
        let (mut app, mut commands) = connected();
        app.composer.insert("hi @ann");
        app.complete
            .marks
            .push(MentionMark::user("ann", "@ann:example.org").with_at(Some(3)));
        app.note_draft();
        write_in_editor(&mut app, |seen| Some(format!("well, {seen}")));
        assert_eq!(app.composer.text(), "well, hi @ann");
        // Not valid any more: where the mention was is not known.
        assert!(app.complete.marks.is_empty());
        assert!(app.complete.active.is_none());
        // And it is saved like typed text.
        app.save_idle_draft(Duration::ZERO);
        assert_eq!(saves(&mut commands), ["!a=well, hi @ann"]);
    }

    #[test]
    fn an_edit_stays_an_edit() {
        let (mut app, mut commands) = connected();
        app.perform(Action::Focus(Pane::Timeline));
        app.timeline.selected = Some(1);
        app.perform(Action::Edit);
        assert!(matches!(app.compose_mode, ComposeMode::Edit(1)));
        assert_eq!(app.composer.text(), "line 1");
        app.complete.edit_had_mentions = true;
        write_in_editor(&mut app, |seen| Some(format!("{seen}!")));
        assert_eq!(app.composer.text(), "line 1!");
        assert!(matches!(app.compose_mode, ComposeMode::Edit(1)));
        // The edit still clears the mentions the message had.
        assert!(app.complete.edit_had_mentions);
        // An edit is not a draft.
        app.save_idle_draft(Duration::ZERO);
        assert_eq!(saves(&mut commands), Vec::<String>::new());
    }

    #[test]
    fn a_reply_stays_a_reply() {
        let (mut app, _commands) = connected();
        app.perform(Action::Focus(Pane::Timeline));
        app.timeline.selected = Some(0);
        app.perform(Action::Reply);
        assert!(matches!(app.compose_mode, ComposeMode::Reply(0)));
        write_in_editor(&mut app, |_| Some("answer".to_owned()));
        assert!(matches!(app.compose_mode, ComposeMode::Reply(0)));
        assert_eq!(app.composer.text(), "answer");
    }

    #[test]
    fn it_works_in_the_demo_and_not_without_a_chat() {
        let mut app = App::demo();
        app.perform(Action::Focus(Pane::Composer));
        write_in_editor(&mut app, |_| Some("from the demo".to_owned()));
        assert_eq!(app.composer.text(), "from the demo");

        app.rooms.clear();
        app.request_editor_with(argv());
        assert!(app.editor_request.is_none());
    }

    #[test]
    fn the_request_does_not_show_the_draft() {
        let (mut app, _commands) = connected();
        app.composer.insert("private words");
        app.request_editor_with(argv());
        assert!(!format!("{:?}", app.editor_request).contains("private"));
    }
}
