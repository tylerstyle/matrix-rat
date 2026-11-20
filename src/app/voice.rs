//! Recording a voice message: whether it is offered here, and the setting that says
//! with what. Nothing records yet: the action only says so.

use super::App;
use crate::audio::{self, recorder};
use crate::ui::Mic;

impl super::playback::AudioState {
    /// What the settings show for the recorder: `auto: pw-record + opusenc`,
    /// `auto: no microphone found`, or the command.
    pub fn recorder_setting_text(&self) -> String {
        if audio::is_auto(&self.recorder_setting) {
            return match &self.recorder {
                Ok(recorder) => format!("auto: {}", recorder.names()),
                Err(why) => format!("auto: {}", why.short()),
            };
        }
        let command = crate::text::sanitize_line(self.recorder_setting.trim());
        match &self.recorder {
            Ok(_) => command,
            Err(recorder::NoRecorder::NoEncoder) => format!("{command} (no Opus encoder found)"),
            Err(_) => format!("{command} (not found)"),
        }
    }
}

impl App {
    /// Why recording a voice message is not offered on this machine; `None` where it
    /// is: a capture program, an encoder and a plausible microphone are there.
    pub(crate) fn recording_refused(&self) -> Option<recorder::NoRecorder> {
        self.audio.recorder.as_ref().err().copied()
    }

    /// What the microphone button of the composer shows.
    pub(crate) fn mic_button(&self) -> Mic {
        if self.recording_refused().is_some() {
            Mic::Hidden
        } else if self.audio.recording {
            Mic::Recording
        } else {
            Mic::Idle
        }
    }

    /// For the doctor: what the setting comes to, without the command itself.
    pub(super) fn recording_found(&self) -> crate::doctor::Recording {
        crate::doctor::Recording::from_choice(&self.audio.recorder)
    }

    /// Alt-M and the microphone button. Refused where a file could not be attached
    /// either (while editing, in a room not joined yet), and where nothing records.
    pub(super) fn record_voice(&mut self) {
        if !self.can_attach() {
            return;
        }
        if let Some(why) = self.recording_refused() {
            self.show_toast(why.message());
            return;
        }
        self.show_toast("Recording voice messages is not available in this build yet");
    }

    /// Enter on the voice recorder row: the setting is kept, and looked up at once.
    pub(super) fn set_audio_recorder(&mut self, text: &str) {
        let setting = if audio::is_auto(text) {
            "auto".to_owned()
        } else {
            text.trim().to_owned()
        };
        self.audio.recorder = recorder::choose_here(&setting);
        self.audio.recorder_setting = setting.clone();
        self.save_setting("audio_recorder", setting);
        let said = match &self.audio.recorder {
            Ok(recorder) => format!("Voice messages are recorded with {}", recorder.names()),
            Err(why) => why.message().to_owned(),
        };
        self.show_toast(said);
    }

    /// For tests: recording is offered, with two programs that are never started
    /// unless the test starts them.
    #[cfg(test)]
    pub(super) fn set_test_recorder(&mut self, capture: recorder::Tool, encoder: recorder::Tool) {
        self.audio.recorder = Ok(recorder::Recorder::for_test(capture, encoder));
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::action::{Action, Pane};
    use crate::app::ComposeMode;
    use crate::audio::recorder::{NoRecorder, Tool};
    use crate::ui::hit::HitId;

    fn tool(name: &str) -> Tool {
        Tool::new(PathBuf::from(format!("/nonexistent/{name}")), Vec::new())
    }

    fn offered() -> App {
        let mut app = App::demo();
        app.set_test_recorder(tool("capture"), tool("encoder"));
        app
    }

    fn toast(app: &App) -> &str {
        app.toast.as_ref().map_or("", |(text, _)| text.as_str())
    }

    fn screen(app: &mut App, width: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        format!("{}", terminal.backend())
    }

    #[test]
    fn a_test_app_offers_no_recording_whatever_the_machine_has() {
        let config = crate::config::Config::default();
        let app = App::new(&config, PathBuf::from("/nonexistent/themes"), None);
        assert_eq!(app.recording_refused(), Some(NoRecorder::NoMicrophone));
        assert_eq!(app.mic_button(), Mic::Hidden);
        assert_eq!(App::demo().mic_button(), Mic::Hidden);
        assert_eq!(offered().mic_button(), Mic::Idle);
    }

    #[test]
    fn the_microphone_is_drawn_and_clickable_only_where_recording_is_offered() {
        let mut app = App::demo();
        let text = screen(&mut app, 100);
        let symbols = &crate::ui::SYMBOLS;
        assert!(!text.contains(symbols.mic), "{text}");
        assert!(text.contains(symbols.clip), "{text}");
        assert!(app.hits.rect_of(HitId::Do(Action::RecordVoice)).is_none());

        let mut app = offered();
        let text = screen(&mut app, 100);
        let row = text
            .lines()
            .find(|line| line.contains(symbols.mic))
            .unwrap();
        // In the order microphone, paperclip, smiley.
        let at = |symbol: &str| row.find(symbol).unwrap();
        assert!(
            at(symbols.mic) < at(symbols.clip) && at(symbols.clip) < at(symbols.smiley),
            "{row}"
        );
        let mic = app.hits.rect_of(HitId::Do(Action::RecordVoice)).unwrap();
        let clip = app.hits.rect_of(HitId::Do(Action::Attach)).unwrap();
        let smiley = app.hits.rect_of(HitId::Do(Action::OpenEmoji)).unwrap();
        assert_eq!((mic.width, clip.width, smiley.width), (2, 2, 2));
        assert_eq!((clip.x, smiley.x), (mic.x + 3, mic.x + 6));

        // While a recording runs the button is a stop sign, in the same place.
        app.audio.recording = true;
        let text = screen(&mut app, 100);
        assert!(
            !text.contains(symbols.mic) && text.contains(symbols.stop),
            "{text}"
        );
        assert_eq!(app.hits.rect_of(HitId::Do(Action::RecordVoice)), Some(mic));

        // Too narrow for the buttons and some text: none of them, as before.
        let mut app = offered();
        app.sidebar.hidden = true;
        let text = screen(&mut app, 16);
        assert!(
            !text.contains(symbols.mic) && !text.contains(symbols.clip),
            "{text}"
        );
        assert!(app.hits.rect_of(HitId::Do(Action::RecordVoice)).is_none());
    }

    #[test]
    fn the_key_and_a_click_on_the_microphone_say_that_nothing_records_yet() {
        let mut app = offered();
        app.perform(Action::RecordVoice);
        assert_eq!(
            toast(&app),
            "Recording voice messages is not available in this build yet"
        );
        app.toast = None;
        let _ = screen(&mut app, 100);
        let mic = app.hits.rect_of(HitId::Do(Action::RecordVoice)).unwrap();
        app.on_event(crossterm::event::Event::Mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: mic.x,
                row: mic.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        ));
        assert_eq!(
            toast(&app),
            "Recording voice messages is not available in this build yet"
        );
        // Alt-M is its key, in every pane.
        for pane in [Pane::Sidebar, Pane::Timeline, Pane::Composer] {
            app.toast = None;
            app.set_focus(pane);
            app.on_event(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char('m'),
                    crossterm::event::KeyModifiers::ALT,
                ),
            ));
            assert!(
                toast(&app).starts_with("Recording voice messages"),
                "{pane:?}"
            );
        }
    }

    #[test]
    fn recording_is_refused_with_the_reason_where_it_is_not_offered_or_nothing_is_attached() {
        let mut app = App::demo();
        app.perform(Action::RecordVoice);
        assert_eq!(toast(&app), NoRecorder::NoMicrophone.message());
        app.audio.recorder = Err(NoRecorder::NoEncoder);
        app.perform(Action::RecordVoice);
        assert_eq!(toast(&app), "No Opus encoder found (opusenc or ffmpeg)");

        // Where a file cannot be attached, that reason comes first.
        let mut app = offered();
        app.compose_mode = ComposeMode::Edit(0);
        app.perform(Action::RecordVoice);
        assert_eq!(toast(&app), "Finish editing before attaching a file");
    }

    #[test]
    fn the_recorder_setting_names_a_command_of_the_users() {
        let dir =
            std::env::temp_dir().join(format!("matrix-rat-voice-setting-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("my-recorder");
        std::fs::write(&program, "").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut app = App::demo();
        assert_eq!(
            app.audio.recorder_setting_text(),
            "auto: no microphone found"
        );
        app.set_audio_recorder(&format!("{} --raw", program.display()));
        // Whether the machine this runs on has an encoder is not the test's business:
        // either way the command is kept and shown, and only a missing encoder is said.
        let shown = app.audio.recorder_setting_text();
        let command = format!("{} --raw", program.display());
        assert!(
            shown == command || shown == format!("{command} (no Opus encoder found)"),
            "{shown}"
        );
        app.set_audio_recorder("no-such-recorder-anywhere-really");
        assert_eq!(
            app.audio.recorder_setting_text(),
            "no-such-recorder-anywhere-really (not found)"
        );
        assert_eq!(
            toast(&app),
            "The recorder named in the settings was not found"
        );
        assert_eq!(app.mic_button(), Mic::Hidden);
        app.set_audio_recorder("  ");
        assert_eq!(app.audio.recorder_setting, "auto");
        std::fs::remove_dir_all(&dir).unwrap();

        let mut app = offered();
        assert_eq!(app.audio.recorder_setting_text(), "auto: capture + encoder");
        // Debug output names neither the setting nor the programs.
        app.audio.recorder_setting = "/home/alice/bin/secret-recorder".to_owned();
        app.set_test_recorder(tool("secret-capture"), tool("secret-encoder"));
        let shown = format!("{app:?}");
        assert!(shown.contains("recorder_auto: false"), "{shown}");
        for private in ["secret-", "/home/alice"] {
            assert!(!shown.contains(private), "{private} in {shown}");
        }
    }
}
