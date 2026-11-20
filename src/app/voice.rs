//! Recording a voice message: whether it is offered here, and the setting that says
//! with what. Nothing records yet: the action only says so.

use super::App;
use crate::audio::{self, recorder};
use crate::ui::Mic;

impl super::playback::AudioState {
    /// What the settings show for the recorder: `auto: parec + opusenc`,
    /// `auto: no microphone found`, or the command.
    pub fn recorder_setting_text(&self) -> String {
        use recorder::NoRecorder;
        if audio::is_auto(&self.recorder_setting) {
            return match &self.recorder {
                Ok(recorder) => format!("auto: {}", recorder.names()),
                Err(why) => format!("auto: {}", why.short()),
            };
        }
        let command = crate::text::sanitize_line(self.recorder_setting.trim());
        match &self.recorder {
            Ok(_) => command,
            Err(NoRecorder::NoEncoder) => format!("{command} (no Opus encoder found)"),
            Err(NoRecorder::BadCommand) => format!("{command} (could not be read)"),
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

    /// What the microphone button of the composer shows. A recording that runs keeps
    /// its stop button whatever became of the recorder since.
    pub(crate) fn mic_button(&self) -> Mic {
        if self.audio.recording {
            Mic::Recording
        } else if self.recording_refused().is_some() {
            Mic::Hidden
        } else {
            Mic::Idle
        }
    }

    /// For the doctor: what the setting comes to, without the command itself.
    pub(super) fn recording_found(&self) -> crate::doctor::Recording {
        crate::doctor::Recording::from_choice(&self.audio.recorder)
    }

    /// What `setting` comes to on this machine as it is now. Nothing is run. Over
    /// SSH is what the player goes by too.
    fn look_for_recorder(&self, setting: &str) -> Result<recorder::Recorder, recorder::NoRecorder> {
        #[cfg(not(test))]
        let env = recorder::Env::here();
        // Tests name their own machine: an empty one unless they say otherwise.
        #[cfg(test)]
        let env = self.audio.recorder_env.clone();
        recorder::choose(
            setting,
            &recorder::Env {
                ssh: self.audio.ssh,
                ..env
            },
        )
    }

    /// Whether a voice message can be recorded into this chat; says why not. Where a
    /// file could not be attached either: while editing, in a room not joined yet.
    fn can_record_here(&mut self) -> bool {
        if matches!(self.compose_mode, super::ComposeMode::Edit(_)) {
            self.show_toast("Finish editing before recording a voice message");
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

    /// Alt-M and the microphone button.
    pub(super) fn record_voice(&mut self) {
        if !self.can_record_here() {
            return;
        }
        if self.recording_refused().is_some() {
            // The machine may have changed since it was looked at: a headset plugged
            // in, the sound server started. One more look before saying no.
            self.audio.recorder = self.look_for_recorder(&self.audio.recorder_setting);
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
        self.audio.recorder = self.look_for_recorder(&setting);
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
    pub(crate) fn set_test_recorder(&mut self, capture: recorder::Tool, encoder: recorder::Tool) {
        self.audio.recorder = Ok(recorder::Recorder::for_test(capture, encoder));
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    use super::*;
    use crate::action::{Action, Pane};
    use crate::app::ComposeMode;
    use crate::audio::recorder::{Env, NoRecorder, Seen, Tool};
    use crate::modal::Modal;
    use crate::ui::hit::HitId;

    const NOT_YET: &str = "Recording voice messages is not available in this build yet";

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

    fn alt_m() -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT))
    }

    /// A folder of programs that are never run, removed at the end.
    struct Bin(PathBuf);

    impl Bin {
        fn new(name: &str, programs: &[&str]) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "matrix-rat-voice-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let bin = Self(dir);
            for program in programs {
                bin.add(program);
            }
            bin
        }

        fn add(&self, program: &str) {
            use std::os::unix::fs::PermissionsExt;
            let path = self.0.join(program);
            std::fs::write(&path, "").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        /// A machine with these programs, a device that records and nothing else.
        fn env(&self) -> Env {
            Env {
                path: Some(self.0.clone().into_os_string()),
                home: None,
                seen: Seen {
                    alsa_capture: true,
                    ..Seen::default()
                },
                ssh: false,
            }
        }
    }

    impl Drop for Bin {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_test_app_offers_no_recording_whatever_the_machine_has() {
        let config = crate::config::Config::default();
        let mut app = App::new(&config, PathBuf::from("/nonexistent/themes"), None);
        assert_eq!(app.recording_refused(), Some(NoRecorder::NoMicrophone));
        assert_eq!(app.mic_button(), Mic::Hidden);
        // Looking again finds the empty machine of the tests, not the real one.
        app.set_audio_recorder("auto");
        assert_eq!(app.recording_refused(), Some(NoRecorder::NoMicrophone));
        assert_eq!(App::demo().mic_button(), Mic::Hidden);
        assert_eq!(offered().mic_button(), Mic::Idle);
    }

    #[test]
    fn the_microphone_is_drawn_and_clickable_only_where_recording_is_offered() {
        let symbols = &crate::ui::SYMBOLS;
        let mut app = App::demo();
        let text = screen(&mut app, 100);
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
        // And it stays, should the recorder be gone meanwhile: it is what stops it.
        app.audio.recorder = Err(NoRecorder::Missing);
        assert_eq!(app.mic_button(), Mic::Recording);
        app.audio.recording = false;
        assert_eq!(app.mic_button(), Mic::Hidden);
    }

    #[test]
    fn the_key_and_a_click_on_the_microphone_say_that_nothing_records_yet() {
        let mut app = offered();
        app.perform(Action::RecordVoice);
        assert_eq!(toast(&app), NOT_YET);
        app.toast = None;
        let _ = screen(&mut app, 100);
        let mic = app.hits.rect_of(HitId::Do(Action::RecordVoice)).unwrap();
        app.on_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: mic.x,
            row: mic.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(toast(&app), NOT_YET);
        // Alt-M is its key, in every pane.
        for pane in [Pane::Sidebar, Pane::Timeline, Pane::Composer] {
            app.toast = None;
            app.set_focus(pane);
            app.on_event(alt_m());
            assert_eq!(toast(&app), NOT_YET, "{pane:?}");
        }
    }

    #[test]
    fn with_a_popup_open_the_key_does_nothing() {
        let mut app = offered();
        app.perform(Action::OpenSettings);
        assert!(matches!(app.modal, Some(Modal::Settings(_))));
        app.on_event(alt_m());
        app.perform(Action::RecordVoice);
        assert_eq!(toast(&app), "");
        assert!(matches!(app.modal, Some(Modal::Settings(_))));
    }

    #[test]
    fn the_palette_entry_runs_the_action_and_is_refused_where_nothing_records() {
        let find = |app: &App| -> (usize, bool) {
            let Some(Modal::Palette(palette)) = &app.modal else {
                panic!("the palette is not open");
            };
            (0..)
                .map_while(|index| palette.entry_at(index).map(|entry| (index, entry)))
                .find(|(_, entry)| {
                    entry.target == crate::palette::Target::Action(Action::RecordVoice)
                })
                .map(|(index, entry)| (index, entry.enabled))
                .expect("the entry is listed")
        };
        let mut app = offered();
        app.perform(Action::OpenPalette);
        let (index, enabled) = find(&app);
        assert!(enabled);
        app.run_palette_at(index);
        assert!(app.modal.is_none());
        assert_eq!(toast(&app), NOT_YET);

        // Greyed where the machine cannot record: choosing it does nothing.
        let mut app = App::demo();
        app.perform(Action::OpenPalette);
        let (index, enabled) = find(&app);
        assert!(!enabled);
        app.run_palette_at(index);
        assert_eq!(toast(&app), "");
    }

    #[test]
    fn recording_is_refused_with_its_own_reason() {
        let mut app = App::demo();
        app.perform(Action::RecordVoice);
        assert_eq!(toast(&app), NoRecorder::NoMicrophone.message());
        assert!(toast(&app).contains("audio_recorder"));

        // While editing: said in words about recording, not about files.
        let mut app = offered();
        app.compose_mode = ComposeMode::Edit(0);
        app.perform(Action::RecordVoice);
        assert_eq!(
            toast(&app),
            "Finish editing before recording a voice message"
        );
        // That comes first, also where nothing records.
        let mut app = App::demo();
        app.compose_mode = ComposeMode::Edit(0);
        app.perform(Action::RecordVoice);
        assert!(toast(&app).starts_with("Finish editing"));
    }

    #[test]
    fn a_refused_recording_looks_at_the_machine_again() {
        let bin = Bin::new("again", &["arecord"]);
        let mut app = App::demo();
        app.audio.recorder_env = bin.env();
        app.perform(Action::RecordVoice);
        assert_eq!(toast(&app), "No Opus encoder found (opusenc or ffmpeg)");
        assert_eq!(app.mic_button(), Mic::Hidden);
        // The encoder is installed meanwhile: the next try finds it, and from then
        // on the button is there.
        bin.add("opusenc");
        assert_eq!(app.mic_button(), Mic::Hidden);
        app.perform(Action::RecordVoice);
        assert_eq!(toast(&app), NOT_YET);
        assert_eq!(app.mic_button(), Mic::Idle);
        assert_eq!(app.audio.recorder_setting_text(), "auto: arecord + opusenc");
        // What is offered is not looked up again on every key.
        std::fs::remove_file(bin.0.join("arecord")).unwrap();
        app.perform(Action::RecordVoice);
        assert_eq!(toast(&app), NOT_YET);
    }

    #[test]
    fn over_ssh_recording_is_refused_as_playing_is_unless_the_user_names_a_recorder() {
        let bin = Bin::new("ssh", &["arecord", "ffmpeg"]);
        let mut app = App::demo();
        app.audio.recorder_env = bin.env();
        app.set_audio_recorder("auto");
        assert_eq!(app.mic_button(), Mic::Idle);

        // The far machine has a microphone and all the programs.
        app.audio.ssh = true;
        app.set_audio_recorder("auto");
        assert_eq!(app.recording_refused(), Some(NoRecorder::OverSsh));
        assert_eq!(toast(&app), NoRecorder::OverSsh.message());
        assert_eq!(app.audio.recorder_setting_text(), "auto: not over SSH");
        assert_eq!(app.mic_button(), Mic::Hidden);
        assert_eq!(app.recording_found(), crate::doctor::Recording::OverSsh);
        // Asking again changes nothing.
        app.perform(Action::RecordVoice);
        assert_eq!(toast(&app), NoRecorder::OverSsh.message());
        // In the palette it is greyed with that reason.
        assert_eq!(app.palette_context().record_refused, Some("not over SSH"));

        // A command of the user's own says they mean it.
        app.set_audio_recorder("arecord -q -t raw -f S16_LE -r 48000 -c 1");
        assert_eq!(app.recording_refused(), None);
        assert_eq!(app.mic_button(), Mic::Idle);
        assert_eq!(
            app.recording_found(),
            crate::doctor::Recording::Own(crate::audio::recorder::Encoder::Ffmpeg)
        );
    }

    #[test]
    fn the_recorder_setting_names_a_command_of_the_users() {
        let bin = Bin::new("setting", &["my-recorder", "ffmpeg"]);
        let mut app = App::demo();
        assert_eq!(
            app.audio.recorder_setting_text(),
            "auto: no microphone found"
        );
        // No microphone is seen on this machine: with a command that is not asked.
        app.audio.recorder_env = Env {
            seen: Seen::default(),
            ..bin.env()
        };
        let command = format!("{} --raw", bin.0.join("my-recorder").display());
        app.set_audio_recorder(&command);
        assert_eq!(app.audio.recorder_setting_text(), command);
        assert_eq!(
            toast(&app),
            "Voice messages are recorded with my-recorder + ffmpeg"
        );
        assert_eq!(app.mic_button(), Mic::Idle);
        // Found on the path as well.
        app.set_audio_recorder("  my-recorder  ");
        assert_eq!(app.audio.recorder_setting, "my-recorder");
        assert_eq!(app.recording_refused(), None);

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
        // A quote left open is not "not found".
        app.set_audio_recorder("my-recorder 'open");
        assert_eq!(
            app.audio.recorder_setting_text(),
            "my-recorder 'open (could not be read)"
        );
        assert!(toast(&app).contains("quote"));
        // Without an encoder the command is there, and that is what is said.
        std::fs::remove_file(bin.0.join("ffmpeg")).unwrap();
        app.set_audio_recorder("my-recorder");
        assert_eq!(
            app.audio.recorder_setting_text(),
            "my-recorder (no Opus encoder found)"
        );
        app.set_audio_recorder("  ");
        assert_eq!(app.audio.recorder_setting, "auto");
        assert_eq!(app.recording_refused(), Some(NoRecorder::NoMicrophone));

        let mut app = offered();
        assert_eq!(app.audio.recorder_setting_text(), "auto: capture + encoder");
        // Debug output names neither the setting nor the programs nor the folders.
        app.audio.recorder_setting = "/home/alice/bin/secret-recorder".to_owned();
        app.audio.recorder_env.path = Some("/home/alice/secret-bin".into());
        app.set_test_recorder(tool("secret-capture"), tool("secret-encoder"));
        let shown = format!("{app:?}");
        assert!(shown.contains("recorder_auto: false"), "{shown}");
        for private in ["secret-", "/home/alice"] {
            assert!(!shown.contains(private), "{private} in {shown}");
        }
    }
}
