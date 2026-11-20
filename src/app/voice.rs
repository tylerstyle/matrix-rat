//! Recording a voice message: whether it is offered here, the setting that says with
//! what, and the recording itself from the first press to the note that waits in the
//! composer.
//!
//! One recording at a time. The mouse holds the microphone (letting go ends it; a
//! short click leaves it running until the next click), the key starts and the same
//! key ends it. However it ends, nothing is sent: the note waits with the files of the
//! conversation it was begun in, and goes when the user sends it.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use super::App;
use super::attach::Scope;
use crate::action::{Action, Pane};
use crate::audio::record::{
    Failure, Limits, RecordControl, RecordEvent, RecordRequest, RecordUpdate, Recording,
};
use crate::audio::{self, TempAudio, recorder};
use crate::keymap::Context;
use crate::matrix::VoiceMeta;
use crate::ui::Mic;

/// A press on the microphone shorter than this is a click: the recording runs on until
/// the next click. A longer one is a hold: letting go ends it. It also covers a
/// terminal that never says the button was let go.
const HOLD: Duration = Duration::from_millis(400);
/// What a recording is called among the waiting files; the room sees another, fixed
/// name (the worker's).
pub const VOICE_NAME: &str = "Voice message";

/// The recording that runs.
pub(in crate::app) struct Take {
    /// Answers about another are dropped.
    id: u64,
    /// The conversation it was begun in, where its note will wait.
    scope: Scope,
    control: UnboundedSender<RecordControl>,
    phase: TakePhase,
    /// The mouse button went down on the microphone then and has not come up.
    held: Option<Instant>,
    elapsed: Duration,
    level: u16,
}

/// Hand-written: the conversation is ids.
impl fmt::Debug for Take {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Take")
            .field("id", &self.id)
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakePhase {
    /// The programs were started; no sound has arrived.
    Starting,
    Running,
    /// Told to end: the encoder finishes, the file is written.
    Saving,
}

/// What is shown of the recording that runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordingView {
    pub phase: TakePhase,
    pub elapsed: Duration,
    /// The loudest of the last moment, 0 to [`audio::MAX_AMPLITUDE`].
    pub level: u16,
    /// The mouse holds the microphone: letting go ends it.
    pub held: bool,
}

/// A voice message recorded here, waiting to be sent. Its file goes with it (and can
/// be shared with the player while previewed without moving or removing it).
pub struct VoiceNote {
    file: Arc<TempAudio>,
    pub duration: Duration,
    waveform: Vec<u16>,
}

/// Hand-written: the path names the user's folder, the waveform is the shape of what
/// was said.
impl fmt::Debug for VoiceNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VoiceNote").finish_non_exhaustive()
    }
}

impl VoiceNote {
    pub fn path(&self) -> &Path {
        self.file.path()
    }

    /// A shared handle to the waiting note's file, for preview playback.
    pub(super) fn file_arc(&self) -> Arc<TempAudio> {
        Arc::clone(&self.file)
    }

    /// What makes the file a voice message when it is sent.
    pub(super) fn meta(&self) -> VoiceMeta {
        VoiceMeta {
            duration: self.duration,
            waveform: self.waveform.clone(),
        }
    }
}

/// Why there is no recording, in the user's words.
fn failure_text(failure: Failure) -> &'static str {
    match failure {
        Failure::Start => "The recorder could not be started",
        Failure::NoSound => "No sound arrived from the microphone",
        Failure::Capture => "The recorder ended before it recorded anything",
        Failure::Encoder => "The recording could not be encoded: nothing was kept",
        Failure::NotOpus => "The encoder did not write Ogg/Opus: nothing was kept",
        Failure::TooShort => "Too short to keep: hold the microphone while you speak",
        Failure::TooLarge => "The recording grew too large: nothing was kept",
        Failure::NotKept => "The recording could not be kept",
    }
}

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
        if self.audio.take.is_some() {
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

    /// Alt-M and the palette: starts a recording, or ends the one that runs and
    /// keeps it.
    pub(super) fn record_voice(&mut self) {
        if self.audio.take.is_some() {
            self.stop_recording();
        } else {
            self.start_recording(false);
        }
    }

    /// The mouse button went down on the microphone: a recording starts and is held,
    /// or the one that runs ends (its button is the stop sign).
    pub(super) fn press_mic(&mut self) {
        if self.modal.is_some() || self.help_open || self.images.viewer_open() {
            return;
        }
        if self.audio.take.is_some() {
            self.stop_recording();
        } else {
            self.start_recording(true);
        }
    }

    /// Any left click elsewhere drops a stale mouse hold (e.g., if a release was
    /// missed outside the terminal) so releasing that other click does not end the
    /// recording.
    pub(super) fn clear_held_mic(&mut self) {
        if let Some(take) = &mut self.audio.take
            && take.held.take().is_some()
        {
            self.dirty = true;
        }
    }

    /// The mouse button came up, anywhere: a held recording ends, unless the press was
    /// only a click, which leaves it running until the next one.
    pub(super) fn release_mic(&mut self) {
        let Some(take) = &mut self.audio.take else {
            return;
        };
        let Some(pressed) = take.held.take() else {
            return;
        };
        self.dirty = true;
        if pressed.elapsed() >= HOLD {
            self.stop_recording();
        }
    }

    /// A recording for the event loop to start.
    pub(super) fn take_record_start(&mut self) -> Option<RecordRequest> {
        self.audio.record_start.take()
    }

    /// What the composer shows of a recording begun in the open conversation.
    pub(crate) fn recording_here(&self) -> Option<RecordingView> {
        let scope = self.attach_scope()?;
        self.recording().filter(|_| {
            self.audio
                .take
                .as_ref()
                .is_some_and(|take| take.scope == scope)
        })
    }

    /// What the status line shows of the recording, wherever it was begun.
    pub(crate) fn recording(&self) -> Option<RecordingView> {
        self.audio.take.as_ref().map(|take| RecordingView {
            phase: take.phase,
            elapsed: take.elapsed,
            level: take.level,
            held: take.held.is_some(),
        })
    }

    /// The key that ends a recording, for the hints.
    pub(crate) fn record_key(&self) -> Option<&str> {
        self.keymap
            .label_in(Context::Composer, Action::RecordVoice)
            .or_else(|| self.keymap.label_for(Action::RecordVoice))
    }

    /// Enter under a recording sends nothing: says what to do first. False where no
    /// recording is in the way.
    pub(super) fn recording_refuses_send(&mut self) -> bool {
        let Some(view) = self.recording_here() else {
            return false;
        };
        let said = if view.phase == TakePhase::Saving {
            "Still saving the recording: wait a moment, then send it".to_owned()
        } else {
            match self.record_key() {
                Some(key) => {
                    format!("The recording still runs: {key} ends it, then it can be sent")
                }
                None => "The recording still runs: end it first, then it can be sent".to_owned(),
            }
        };
        self.show_toast(said);
        true
    }

    /// Entering Edit mode under a recording is refused: says what to do first. False
    /// where no recording is in the way.
    pub(super) fn recording_refuses_edit(&mut self) -> bool {
        let Some(view) = self.recording_here() else {
            return false;
        };
        let said = if view.phase == TakePhase::Saving {
            "Still saving the recording: wait a moment before editing".to_owned()
        } else {
            match self.record_key() {
                Some(key) => {
                    format!("The recording still runs: {key} ends it before editing a message")
                }
                None => "The recording still runs: end it before editing a message".to_owned(),
            }
        };
        self.show_toast(said);
        true
    }

    fn start_recording(&mut self, held: bool) {
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
        let (Ok(recorder), Some(dir), Some(scope)) = (
            self.audio.recorder.clone(),
            self.audio.dir.clone(),
            self.attach_scope(),
        ) else {
            self.show_toast("Voice messages cannot be recorded here");
            return;
        };
        // A sound that plays would be recorded with it.
        self.stop_audio();
        self.audio.next_id += 1;
        let id = self.audio.next_id;
        let (control, receiver) = unbounded_channel();
        self.audio.take = Some(Take {
            id,
            scope,
            control,
            phase: TakePhase::Starting,
            held: held.then(Instant::now),
            elapsed: Duration::ZERO,
            level: 0,
        });
        self.audio.record_start = Some(RecordRequest {
            id,
            capture: recorder.capture,
            encoder: recorder.encoder,
            dir,
            limits: Limits::default(),
            control: receiver,
        });
        self.dirty = true;
    }

    /// Ends the recording and keeps it: its note will wait where it was begun.
    /// Nothing if none runs, or it is ending already. If the event loop has not even
    /// started the recorder yet, it is cleared without spawning.
    pub(super) fn stop_recording(&mut self) {
        if self.audio.record_start.take().is_some() {
            self.audio.take = None;
            self.dirty = true;
            return;
        }
        let Some(take) = &mut self.audio.take else {
            return;
        };
        if take.phase == TakePhase::Saving {
            return;
        }
        take.phase = TakePhase::Saving;
        take.held = None;
        // Its task gone already: its last word is on the way, or it was dropped.
        let _ = take.control.send(RecordControl::Stop);
        self.dirty = true;
    }

    /// Discards a recording that is still open (`Starting` or `Running`) in the open
    /// conversation. A take that is already `Saving`, or belongs to another chat, is
    /// left untouched.
    pub(super) fn discard_active_recording(&mut self) -> bool {
        let active_here = self
            .recording_here()
            .is_some_and(|view| matches!(view.phase, TakePhase::Starting | TakePhase::Running));
        active_here && self.cancel_recording()
    }

    /// Cancels a recording that belongs to `room_id`, wherever the user is now.
    /// True if one was cancelled.
    pub(super) fn cancel_recording_in(&mut self, room_id: &str) -> bool {
        if self
            .audio
            .take
            .as_ref()
            .is_some_and(|take| take.scope.0 == room_id)
        {
            self.cancel_recording()
        } else {
            false
        }
    }

    /// Ends the recording and keeps nothing, at any moment of it. False if none ran.
    pub(super) fn cancel_recording(&mut self) -> bool {
        // Not started yet: it never is.
        self.audio.record_start = None;
        let Some(take) = self.audio.take.take() else {
            return false;
        };
        // The channel going with `take` says the same.
        let _ = take.control.send(RecordControl::Cancel);
        self.dirty = true;
        true
    }

    /// On every tick: a recording whose conversation is not the open one any more
    /// (another room, a thread opened or closed, the chat closed) ends and is kept.
    pub(super) fn recording_tick(&mut self) {
        let Some(take) = &self.audio.take else {
            return;
        };
        if self.attach_scope().as_ref() != Some(&take.scope) {
            self.stop_recording();
        }
    }

    pub(super) fn on_recording(&mut self, update: RecordUpdate) {
        // Of a recording given up meanwhile: a file that came with it goes with it.
        if self
            .audio
            .take
            .as_ref()
            .is_none_or(|take| take.id != update.id)
        {
            return;
        }
        self.dirty = true;
        match update.event {
            RecordEvent::Started => {
                if let Some(take) = &mut self.audio.take
                    && take.phase == TakePhase::Starting
                {
                    take.phase = TakePhase::Running;
                }
            }
            RecordEvent::Level { elapsed, level } => {
                if let Some(take) = &mut self.audio.take {
                    take.elapsed = elapsed;
                    take.level = level;
                }
            }
            RecordEvent::Done(recording) => {
                if let Some(take) = self.audio.take.take() {
                    self.keep_recording(take.scope, recording);
                }
            }
            RecordEvent::Failed(failure) => {
                self.audio.take = None;
                self.show_toast(failure_text(failure));
            }
            RecordEvent::Cancelled => self.audio.take = None,
        }
    }

    /// A finished recording joins the files waiting in the conversation it was begun
    /// in, also when that is not the open one any more. Where that conversation's room
    /// is gone, or the session, it is dropped, and its file with it.
    fn keep_recording(&mut self, scope: Scope, recording: Recording) {
        if self.logged_out || !self.rooms.iter().any(|room| room.id == scope.0) {
            return;
        }
        let Recording {
            file,
            duration,
            waveform,
            size,
            silent,
            full,
        } = recording;
        if let Some(reason) = crate::upload::too_large(VOICE_NAME, size, self.attach.limit) {
            self.show_toast(reason);
            return;
        }
        let here = self.attach_scope().as_ref() == Some(&scope);
        let voice = VoiceNote {
            file: Arc::new(file),
            duration,
            waveform,
        };
        self.attach.add_voice(scope, voice, size);
        // Like a pasted picture, it answers an `/upload` command being typed.
        if here && crate::upload::command_argument(self.composer.text()).is_some() {
            self.restore_draft();
        }
        let said = if silent {
            "Only silence was recorded: is the microphone the right one?"
        } else if full {
            "The longest voice message was reached: the recording waits to be sent"
        } else if here {
            "Recorded: add a caption, or send it"
        } else {
            "Recorded: it waits in the chat it was begun in"
        };
        self.show_toast(said);
        if here && self.modal.is_none() && !self.help_open && !self.images.viewer_open() {
            self.set_focus(Pane::Composer);
        }
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

    /// Whether a recording was begun; it is given up again.
    fn began(app: &mut App) -> bool {
        let began = app.take_record_start().is_some();
        app.cancel_recording();
        began
    }

    /// The task of the recording that runs says `event`.
    fn update(app: &mut App, event: RecordEvent) {
        let id = app.audio.next_id;
        app.on_recording(RecordUpdate { id, event });
    }

    /// The recording ends well: its file is in `dir`.
    fn done(app: &mut App, dir: &Bin, secs: u64) -> PathBuf {
        let file = TempAudio::voice(&dir.0, b"OggS and so on").unwrap();
        let path = file.path().to_owned();
        update(
            app,
            RecordEvent::Done(Recording {
                file,
                duration: Duration::from_secs(secs),
                waveform: vec![100, 1024, 3],
                size: 28_000,
                silent: false,
                full: false,
            }),
        );
        path
    }

    /// The recording is under way: its request, as the event loop would take it.
    fn recording(app: &mut App) -> RecordRequest {
        let request = app.take_record_start().expect("a recording to start");
        update(app, RecordEvent::Started);
        request
    }

    fn told(request: &mut RecordRequest) -> Option<RecordControl> {
        request.control.try_recv().ok()
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn tool(name: &str) -> Tool {
        Tool::new(PathBuf::from(format!("/nonexistent/{name}")), Vec::new())
    }

    fn offered() -> App {
        let mut app = App::demo();
        app.set_test_recorder(tool("capture"), tool("encoder"));
        // Never written to: the tests that keep a recording bring a folder.
        app.audio.dir = Some(PathBuf::from("/nonexistent/matrix-rat-voice"));
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
        app.perform(Action::RecordVoice);
        let text = screen(&mut app, 100);
        assert!(
            !text.contains(symbols.mic) && text.contains(symbols.stop),
            "{text}"
        );
        assert_eq!(app.hits.rect_of(HitId::Do(Action::RecordVoice)), Some(mic));
        // And it stays, should the recorder be gone meanwhile: it is what stops it.
        app.audio.recorder = Err(NoRecorder::Missing);
        assert_eq!(app.mic_button(), Mic::Recording);
        app.cancel_recording();
        assert_eq!(app.mic_button(), Mic::Hidden);
    }

    #[test]
    fn the_key_starts_a_recording_in_every_pane() {
        for pane in [Pane::Sidebar, Pane::Timeline, Pane::Composer] {
            let mut app = offered();
            app.set_focus(pane);
            app.on_event(alt_m());
            assert_eq!(app.mic_button(), Mic::Recording, "{pane:?}");
            assert!(began(&mut app), "{pane:?}");
            assert_eq!(app.mic_button(), Mic::Idle, "{pane:?}");
        }
    }

    #[test]
    fn the_key_starts_a_recording_and_the_same_key_ends_it_into_a_waiting_note() {
        let dir = Bin::new("key", &[]);
        let mut app = offered();
        app.on_event(alt_m());
        assert!(screen(&mut app, 100).contains("● starting…"));
        let mut request = recording(&mut app);
        update(
            &mut app,
            RecordEvent::Level {
                elapsed: Duration::from_secs(7),
                level: 1024,
            },
        );
        let stops = format!("{} stops", app.record_key().unwrap());
        let text = screen(&mut app, 100);
        let row = format!("● 0:07 █ · {stops} · Esc discards");
        assert!(text.contains(&row), "{text}");
        // The status line says it too, whatever covers the composer.
        assert_eq!(text.matches("● 0:07").count(), 2, "{text}");
        assert_eq!(told(&mut request), None);

        app.on_event(alt_m());
        assert_eq!(told(&mut request), Some(RecordControl::Stop));
        assert!(screen(&mut app, 100).contains("● 0:07 saving…"));
        // It is ending: the key has nothing more to say, and the stop sign stays.
        app.on_event(alt_m());
        assert_eq!(told(&mut request), None);
        assert_eq!(app.mic_button(), Mic::Recording);

        let path = done(&mut app, &dir, 12);
        assert_eq!(app.mic_button(), Mic::Idle);
        assert_eq!(app.focus, Pane::Composer);
        let text = screen(&mut app, 100);
        assert!(text.contains("🎤 Voice message · 0:12 · "), "{text}");
        assert!(!text.contains("● 0:07"), "{text}");
        assert_eq!(app.attachments().len(), 1);
        assert!(path.exists());
        // Nothing went anywhere by itself: Enter sends it (here, in the demo, as a
        // row of its own), and its file goes.
        let before = app.messages().len();
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.messages().len(), before + 1);
        let sent = &app.messages().last().unwrap().body;
        assert_eq!(sent, "🎤 Voice message · 0:12");
        assert!(app.attachments().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn holding_the_microphone_records_until_it_is_let_go_and_a_click_latches() {
        let mut app = offered();
        let _ = screen(&mut app, 100);
        let mic = app.hits.rect_of(HitId::Do(Action::RecordVoice)).unwrap();
        let down = MouseEventKind::Down(MouseButton::Left);
        let up = MouseEventKind::Up(MouseButton::Left);

        // Let go at once: that was a click, and the recording runs on.
        app.on_event(mouse(down, mic.x, mic.y));
        let mut request = recording(&mut app);
        assert!(screen(&mut app, 100).contains("let go to stop"));
        app.on_event(mouse(up, mic.x, mic.y));
        assert_eq!(told(&mut request), None);
        assert!(screen(&mut app, 100).contains(" stops · Esc discards"));
        // The next click, on what is the stop sign now, ends it; its letting go
        // says nothing more.
        app.on_event(mouse(down, mic.x, mic.y));
        assert_eq!(told(&mut request), Some(RecordControl::Stop));
        app.on_event(mouse(up, mic.x, mic.y));
        assert_eq!(told(&mut request), None);
        update(&mut app, RecordEvent::Failed(Failure::TooShort));
        assert!(toast(&app).starts_with("Too short to keep"));
        assert_eq!(app.mic_button(), Mic::Idle);

        // Held for real: letting go ends it, wherever the pointer is by then.
        app.on_event(mouse(down, mic.x, mic.y));
        let mut request = recording(&mut app);
        app.audio.take.as_mut().unwrap().held = Some(Instant::now() - HOLD);
        app.on_event(mouse(up, 0, 0));
        assert_eq!(told(&mut request), Some(RecordControl::Stop));
        // And the key ends a held one as well.
        app.cancel_recording();
        app.on_event(mouse(down, mic.x, mic.y));
        let mut request = recording(&mut app);
        app.on_event(alt_m());
        assert_eq!(told(&mut request), Some(RecordControl::Stop));
    }

    #[test]
    fn esc_discards_a_recording_wherever_the_keys_go_and_a_late_file_goes_with_it() {
        let dir = Bin::new("esc", &[]);
        let mut app = offered();
        app.perform(Action::RecordVoice);
        let mut request = recording(&mut app);
        app.set_focus(Pane::Timeline);
        app.on_event(key(KeyCode::Esc));
        assert_eq!(told(&mut request), Some(RecordControl::Cancel));
        assert_eq!(app.mic_button(), Mic::Idle);
        assert_eq!(toast(&app), "The recording was discarded");
        // That was all Esc did.
        assert_eq!(app.focus, Pane::Timeline);
        // The task had finished meanwhile: what it brings is not kept.
        let path = done(&mut app, &dir, 5);
        assert!(!path.exists());
        assert!(app.attachments().is_empty());
        // With nothing recording, Esc is what it always was.
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.focus, Pane::Composer);

        // One that was not even started yet never is.
        app.perform(Action::RecordVoice);
        app.on_event(key(KeyCode::Esc));
        assert!(app.take_record_start().is_none());
    }

    #[test]
    fn enter_sends_nothing_from_under_a_recording() {
        let mut app = offered();
        app.set_focus(Pane::Composer);
        for c in "hello".chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        app.perform(Action::RecordVoice);
        let _request = recording(&mut app);
        let before = app.messages().len();
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.messages().len(), before);
        assert_eq!(app.composer.text(), "hello");
        assert!(toast(&app).starts_with("The recording still runs"));
        assert_eq!(app.mic_button(), Mic::Recording);
    }

    #[test]
    fn leaving_the_chat_losing_the_window_and_suspending_end_it_and_keep_it() {
        let dir = Bin::new("leave", &[]);
        type Way = fn(&mut App);
        let ways: [(&str, Way); 4] = [
            ("another room", |app| app.open_room(1)),
            ("the window", |app| app.on_event(Event::FocusLost)),
            ("suspend", |app| app.perform(Action::Suspend)),
            ("a tick elsewhere", |app| {
                app.active = 1;
                app.on_tick();
            }),
        ];
        for (name, way) in ways {
            let mut app = offered();
            let origin = app.attach_scope().unwrap();
            app.perform(Action::RecordVoice);
            let mut request = recording(&mut app);
            way(&mut app);
            assert_eq!(told(&mut request), Some(RecordControl::Stop), "{name}");
            // The note waits where it was begun, also when that is not open now.
            let path = done(&mut app, &dir, 3);
            assert!(path.exists(), "{name}");
            assert_eq!(app.attach.files(Some(&origin)).len(), 1, "{name}");
            if app.attach_scope().as_ref() != Some(&origin) {
                assert!(app.attachments().is_empty(), "{name}");
                assert_eq!(
                    toast(&app),
                    "Recorded: it waits in the chat it was begun in",
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn logging_out_gives_a_recording_up_and_removes_the_notes_that_wait() {
        let dir = Bin::new("logout", &[]);
        let mut app = offered();
        app.perform(Action::RecordVoice);
        let _request = recording(&mut app);
        let path = done(&mut app, &dir, 3);
        app.perform(Action::RecordVoice);
        let mut request = recording(&mut app);
        app.reset_session();
        assert_eq!(told(&mut request), Some(RecordControl::Cancel));
        assert_eq!(app.mic_button(), Mic::Idle);
        assert!(!path.exists());
        // What its task still brings finds no session to wait in.
        let late = done(&mut app, &dir, 3);
        assert!(!late.exists());
    }

    #[test]
    fn a_failure_is_said_and_a_note_taken_off_again_loses_its_file() {
        let dir = Bin::new("fail", &[]);
        let mut app = offered();
        app.perform(Action::RecordVoice);
        let _request = app.take_record_start().unwrap();
        update(&mut app, RecordEvent::Failed(Failure::NoSound));
        assert_eq!(toast(&app), "No sound arrived from the microphone");
        assert_eq!(app.mic_button(), Mic::Idle);
        // An answer about another recording than the one that runs is dropped.
        app.perform(Action::RecordVoice);
        let _request = recording(&mut app);
        app.on_recording(RecordUpdate {
            id: app.audio.next_id - 1,
            event: RecordEvent::Failed(Failure::Encoder),
        });
        assert_eq!(app.mic_button(), Mic::Recording);

        let path = done(&mut app, &dir, 3);
        assert!(path.exists());
        // Backspace in the empty message takes the note off, as it does a file.
        app.on_event(key(KeyCode::Backspace));
        assert!(app.attachments().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn a_silent_recording_and_one_stopped_by_its_length_are_kept_and_said() {
        let dir = Bin::new("said", &[]);
        for (silent, full, said) in [
            (true, false, "Only silence was recorded"),
            (false, true, "The longest voice message was reached"),
        ] {
            let mut app = offered();
            app.perform(Action::RecordVoice);
            let _request = recording(&mut app);
            let file = TempAudio::voice(&dir.0, b"OggS").unwrap();
            update(
                &mut app,
                RecordEvent::Done(Recording {
                    file,
                    duration: Duration::from_secs(300),
                    waveform: Vec::new(),
                    size: 4,
                    silent,
                    full,
                }),
            );
            assert!(toast(&app).starts_with(said), "{}", toast(&app));
            assert_eq!(app.attachments().len(), 1);
        }
    }

    #[test]
    fn the_worker_is_handed_a_voice_message_with_its_length_and_waveform() {
        use crate::matrix::{MatrixCmd, MatrixHandle, OutgoingFile};
        use crate::model::{Room, RoomKind};
        let dir = Bin::new("worker", &[]);
        let mut app = offered();
        let (handle, mut commands) = MatrixHandle::for_test();
        app.matrix = Some(handle);
        app.rooms = vec![Room::new("!alpha", "Alpha", RoomKind::Room)];
        app.active = 0;
        app.set_focus(Pane::Composer);
        app.perform(Action::RecordVoice);
        let _request = recording(&mut app);
        let path = done(&mut app, &dir, 12);
        app.on_event(key(KeyCode::Enter));
        let mut sent = None;
        while let Ok(command) = commands.try_recv() {
            if let MatrixCmd::SendFiles { files, .. } = command {
                sent = Some(files);
            }
        }
        let files: Vec<OutgoingFile> = sent.expect("the files to be sent");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, path);
        assert_eq!(
            files[0].voice,
            Some(VoiceMeta {
                duration: Duration::from_secs(12),
                waveform: vec![100, 1024, 3],
            })
        );
        // The worker reads the file some time later: it stays until it has answered.
        assert!(app.attachments().is_empty());
        assert!(path.exists());
        assert!(app.attach.sends_out());

        // Shutting down closes the command channel while keeping `app` (and the voice
        // file) alive so the worker can finish reading it before `app` drops.
        app.shut_down_worker();
        assert!(matches!(commands.try_recv(), Ok(MatrixCmd::Shutdown)));
        assert!(matches!(
            commands.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
        assert!(path.exists());
        drop(app);
        assert!(!path.exists());
    }

    #[test]
    fn logging_out_during_a_send_suppresses_a_late_failure_toast() {
        use crate::matrix::{MatrixEvent, MatrixHandle};
        use crate::model::{Room, RoomKind};
        let dir = Bin::new("logout-send", &[]);
        let mut app = offered();
        let (handle, _commands) = MatrixHandle::for_test();
        app.matrix = Some(handle);
        app.rooms = vec![Room::new("!alpha", "Alpha", RoomKind::Room)];
        app.active = 0;
        app.set_focus(Pane::Composer);
        app.perform(Action::RecordVoice);
        let _request = recording(&mut app);
        let path = done(&mut app, &dir, 4);
        app.on_event(key(KeyCode::Enter));
        let send = app.attach.last_send();
        assert!(path.exists());

        app.reset_session();
        assert!(!path.exists());
        assert_eq!(toast(&app), "");
        app.on_matrix(MatrixEvent::FilesNotSent {
            send,
            reason: "The voice message does not exist".to_owned(),
        });
        assert_eq!(toast(&app), "");
    }

    #[test]
    fn debug_output_names_no_folder_file_or_conversation() {
        let dir = Bin::new("debug", &[]);
        let mut app = offered();
        app.perform(Action::RecordVoice);
        let _request = recording(&mut app);
        let _path = done(&mut app, &dir, 3);
        app.perform(Action::RecordVoice);
        let request = recording(&mut app);
        let shown = format!("{:?} {:?} {request:?}", app.audio, app.attach);
        let room = app.attach_scope().unwrap().0;
        for private in ["matrix-rat-voice", "voice-", room.as_str(), "nonexistent"] {
            assert!(!shown.contains(private), "{private} in {shown}");
        }
        assert!(shown.contains("voices: 1"), "{shown}");
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
        assert!(began(&mut app));

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
        // Set by the event loop: there is somewhere to keep a recording.
        app.audio.dir = Some(PathBuf::from("/nonexistent/matrix-rat-voice"));
        app.perform(Action::RecordVoice);
        assert_eq!(toast(&app), "No Opus encoder found (opusenc or ffmpeg)");
        assert_eq!(app.mic_button(), Mic::Hidden);
        // The encoder is installed meanwhile: the next try finds it, and from then
        // on the button is there.
        bin.add("opusenc");
        assert_eq!(app.mic_button(), Mic::Hidden);
        app.perform(Action::RecordVoice);
        assert!(began(&mut app));
        assert_eq!(app.mic_button(), Mic::Idle);
        assert_eq!(app.audio.recorder_setting_text(), "auto: arecord + opusenc");
        // What is offered is not looked up again on every key.
        std::fs::remove_file(bin.0.join("arecord")).unwrap();
        app.perform(Action::RecordVoice);
        assert!(began(&mut app));
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

    #[test]
    fn editor_stops_a_running_recording_and_clears_an_unspawned_start() {
        let dir = Bin::new("editor", &[]);
        let mut app = offered();
        app.perform(Action::RecordVoice);
        let mut request = recording(&mut app);
        app.request_editor_with(Ok(vec!["fake-editor".to_owned()]));
        assert_eq!(told(&mut request), Some(RecordControl::Stop));
        assert!(app.take_editor_request().is_some());
        let path = done(&mut app, &dir, 5);
        assert!(path.exists());
        assert_eq!(app.attachments().len(), 1);

        // If `stop_recording` (via `$EDITOR`, `Suspend`, room change, etc.) arrives
        // before the event loop takes `record_start`, nothing is ever spawned.
        let mut app = offered();
        app.perform(Action::RecordVoice);
        app.request_editor_with(Ok(vec!["fake-editor".to_owned()]));
        assert!(app.take_record_start().is_none());
        assert_eq!(app.mic_button(), Mic::Idle);
        assert!(app.recording().is_none());

        let mut app = offered();
        app.perform(Action::RecordVoice);
        app.perform(Action::Suspend);
        assert!(app.suspend_requested);
        assert!(app.take_record_start().is_none());
        assert_eq!(app.mic_button(), Mic::Idle);
    }

    #[test]
    fn suspending_from_the_picture_viewer_stops_a_recording() {
        let mut app = offered();
        app.perform(Action::RecordVoice);
        let mut request = recording(&mut app);
        app.on_viewer_key(KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert!(app.suspend_requested);
        assert_eq!(told(&mut request), Some(RecordControl::Stop));
    }

    #[test]
    fn esc_lets_popups_go_first_and_never_discards_a_saving_or_other_room_recording() {
        let dir = Bin::new("esc-popups", &[]);
        let mut app = offered();
        app.perform(Action::RecordVoice);
        let mut request = recording(&mut app);
        update(
            &mut app,
            RecordEvent::Level {
                elapsed: Duration::from_secs(7),
                level: 512,
            },
        );

        // Emoji popup: Esc closes it, recording keeps running and status keeps ● 0:07.
        app.perform(Action::OpenEmoji);
        assert!(matches!(app.modal, Some(Modal::Emoji(_))));
        assert!(screen(&mut app, 100).contains("● 0:07"));
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert_eq!(told(&mut request), None);
        assert_eq!(app.mic_button(), Mic::Recording);

        // Settings popup with an edited field: first Esc leaves field edit, second
        // closes settings, recording still runs.
        app.perform(Action::OpenSettings);
        if let Some(Modal::Settings(menu)) = &mut app.modal {
            menu.editing = Some(crate::lineedit::LineEdit::from("auto"));
        }
        app.on_event(key(KeyCode::Esc));
        assert!(matches!(
            &app.modal,
            Some(Modal::Settings(menu)) if menu.editing.is_none()
        ));
        assert_eq!(told(&mut request), None);
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert_eq!(told(&mut request), None);

        // Help overlay: Esc closes help, recording still runs.
        app.perform(Action::ToggleHelp);
        assert!(app.help_open);
        app.on_event(key(KeyCode::Esc));
        assert!(!app.help_open);
        assert_eq!(told(&mut request), None);

        // Image viewer: Esc closes viewer, recording still runs.
        app.images.open_viewer(&crate::model::ImageInfo {
            key: crate::model::DEMO_IMAGE.to_owned(),
            source: crate::model::ImageSource::Demo,
            name: "photo.png".to_owned(),
            caption: None,
            dimensions: Some((10, 10)),
            size: Some(10),
            sticker: false,
        });
        assert!(app.images.viewer_open());
        app.on_event(key(KeyCode::Esc));
        assert!(!app.images.viewer_open());
        assert_eq!(told(&mut request), None);

        // Saving in the open chat: Esc does NOT discard it (it does its normal job
        // instead, here returning focus from Timeline to Composer).
        app.set_focus(Pane::Timeline);
        app.on_event(alt_m());
        assert_eq!(told(&mut request), Some(RecordControl::Stop));
        app.on_event(key(KeyCode::Esc));
        assert_eq!(told(&mut request), None);
        assert_eq!(app.focus, Pane::Composer);
        assert_eq!(app.mic_button(), Mic::Recording);
        let path = done(&mut app, &dir, 7);
        assert!(path.exists());
        assert_eq!(app.attachments().len(), 1);

        // Saving in another room after switching rooms: Esc in the new room does NOT
        // discard the old room's recording.
        let origin = app.attach_scope().unwrap();
        app.perform(Action::RecordVoice);
        let mut request = recording(&mut app);
        app.open_room(1);
        assert_eq!(told(&mut request), Some(RecordControl::Stop));
        app.on_event(key(KeyCode::Esc));
        assert_eq!(told(&mut request), None);
        let other_path = done(&mut app, &dir, 4);
        assert!(other_path.exists());
        assert_eq!(app.attach.files(Some(&origin)).len(), 2);
    }

    #[test]
    fn enter_refuses_starting_and_saving_recordings_with_accurate_toasts() {
        let mut app = offered();
        app.set_focus(Pane::Composer);
        app.perform(Action::RecordVoice);
        let mut request = app.take_record_start().unwrap();
        let before = app.messages().len();

        // Starting: refuses and names the key that stops it.
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.messages().len(), before);
        assert!(toast(&app).starts_with("The recording still runs"));
        assert!(toast(&app).contains(app.record_key().unwrap()));

        // Saving: refuses with a wait message, NOT telling the user to press Alt-M.
        update(&mut app, RecordEvent::Started);
        app.on_event(alt_m());
        assert_eq!(told(&mut request), Some(RecordControl::Stop));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.messages().len(), before);
        assert_eq!(
            toast(&app),
            "Still saving the recording: wait a moment, then send it"
        );
    }

    #[test]
    fn thread_transitions_stop_a_recording_at_once() {
        let dir = Bin::new("thread", &[]);
        let mut app = offered();
        let room_scope = app.attach_scope().unwrap();
        app.perform(Action::RecordVoice);
        let mut request = recording(&mut app);

        // Opening a thread stops the room's recording immediately (before any tick).
        let msg_idx = app
            .messages()
            .iter()
            .position(|m| matches!(m.kind, crate::model::MsgKind::Text))
            .unwrap();
        app.open_thread(msg_idx);
        assert!(app.thread_open());
        assert_eq!(told(&mut request), Some(RecordControl::Stop));
        let path1 = done(&mut app, &dir, 3);
        assert!(path1.exists());
        assert_eq!(app.attach.files(Some(&room_scope)).len(), 1);
        assert!(app.attachments().is_empty());

        // Closing the thread stops a thread recording immediately.
        let thread_scope = app.attach_scope().unwrap();
        app.perform(Action::RecordVoice);
        let mut request = recording(&mut app);
        app.close_thread();
        assert!(!app.thread_open());
        assert_eq!(told(&mut request), Some(RecordControl::Stop));
        let path2 = done(&mut app, &dir, 4);
        assert!(path2.exists());
        assert_eq!(app.attach.files(Some(&thread_scope)).len(), 1);

        // Jumping into a thread stops the room's recording immediately.
        app.perform(Action::RecordVoice);
        let mut request = recording(&mut app);
        app.enter_thread_from_jump("$root".to_owned());
        assert!(app.thread_open());
        assert_eq!(told(&mut request), Some(RecordControl::Stop));
    }

    #[test]
    fn edit_and_edit_last_are_refused_while_recording() {
        let mut app = offered();
        let own_idx = app
            .messages()
            .iter()
            .rposition(|m| m.editable())
            .expect("demo room has an editable own message");
        app.perform(Action::RecordVoice);
        let mut request = recording(&mut app);

        app.timeline.selected = Some(own_idx);
        app.perform(Action::Edit);
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert!(toast(&app).contains(app.record_key().unwrap()));

        app.perform(Action::EditLast);
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert!(toast(&app).contains(app.record_key().unwrap()));

        // While Saving: distinct wait toast.
        app.on_event(alt_m());
        assert_eq!(told(&mut request), Some(RecordControl::Stop));
        app.perform(Action::EditLast);
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert_eq!(
            toast(&app),
            "Still saving the recording: wait a moment before editing"
        );
    }

    #[test]
    fn a_vanishing_room_cancels_its_recording_at_once() {
        use crate::matrix::{RoomCategory, RoomSummary};

        let dir = Bin::new("vanish", &[]);
        let mut app = offered();
        let kept_id = app.rooms[1].id.clone();
        let kept_name = app.rooms[1].name.clone();
        app.perform(Action::RecordVoice);
        let mut request = recording(&mut app);

        app.apply_rooms(vec![RoomSummary {
            id: kept_id,
            name: kept_name,
            category: RoomCategory::Room,
            notifications: 0,
            preview: None,
            server_notifications: 0,
            mention: false,
            unread: false,
            marked_unread: false,
            avatar: None,
            alias: None,
            direct: false,
            muted: false,
            encrypted: false,
        }]);
        assert_eq!(told(&mut request), Some(RecordControl::Cancel));
        assert_eq!(app.mic_button(), Mic::Idle);
        assert_eq!(
            toast(&app),
            "That room is gone: the recording was discarded"
        );
        let late = done(&mut app, &dir, 3);
        assert!(!late.exists());
    }

    #[test]
    fn clicking_elsewhere_clears_a_stale_mouse_hold_on_the_microphone() {
        let mut app = offered();
        let _ = screen(&mut app, 100);
        let mic = app.hits.rect_of(HitId::Do(Action::RecordVoice)).unwrap();
        let down = MouseEventKind::Down(MouseButton::Left);
        let up = MouseEventKind::Up(MouseButton::Left);

        app.on_event(mouse(down, mic.x, mic.y));
        let mut request = recording(&mut app);
        app.audio.take.as_mut().unwrap().held = Some(Instant::now() - HOLD);
        assert!(screen(&mut app, 100).contains("let go to stop"));

        // Left-clicking elsewhere drops the stale hold; its release must not stop it.
        app.on_event(mouse(down, 0, 0));
        assert!(app.audio.take.as_ref().unwrap().held.is_none());
        assert!(!screen(&mut app, 100).contains("let go to stop"));
        app.on_event(mouse(up, 0, 0));
        assert_eq!(told(&mut request), None);
    }

    #[test]
    fn keep_recording_checks_size_limit_restores_upload_draft_and_hides_temp_name_on_send_error() {
        let dir = Bin::new("limits", &[]);
        let mut app = offered();
        // Recording larger than the server's limit is dropped with a clear toast.
        app.attach.limit = Some(10_000);
        app.perform(Action::RecordVoice);
        let _request = recording(&mut app);
        let too_big = done(&mut app, &dir, 12);
        assert!(!too_big.exists());
        assert!(app.attachments().is_empty());
        assert!(
            toast(&app).starts_with("Voice message is too large"),
            "{}",
            toast(&app)
        );
        assert!(!toast(&app).contains("voice-"));

        // Finishing while `/upload ` is typed restores the stashed caption draft.
        app.attach.limit = None;
        app.set_focus(Pane::Composer);
        app.composer.set_text("my caption");
        app.perform(Action::Attach);
        assert_eq!(app.composer.text(), "/upload ");
        app.perform(Action::RecordVoice);
        let _request = recording(&mut app);
        let path = done(&mut app, &dir, 4);
        assert!(path.exists());
        assert_eq!(app.composer.text(), "my caption");

        // If the server limit becomes smaller before Enter, toast names "Voice message".
        app.attach.limit = Some(5);
        app.on_event(key(KeyCode::Enter));
        assert!(
            toast(&app).starts_with("Voice message is too large"),
            "{}",
            toast(&app)
        );
        assert!(!toast(&app).contains("voice-"));

        // If the file vanished from disk before Enter, toast does not expose temp name.
        app.attach.limit = None;
        std::fs::remove_file(&path).unwrap();
        app.on_event(key(KeyCode::Enter));
        assert_eq!(toast(&app), "The voice message is no longer available");
    }

    #[test]
    fn palette_menu_and_status_hint_offer_stop_recording_while_one_runs() {
        let mut app = offered();
        app.set_focus(Pane::Composer);
        app.perform(Action::RecordVoice);
        let _request = recording(&mut app);
        // Even if the recorder disappears mid-recording, stopping stays offered.
        app.audio.recorder = Err(NoRecorder::Missing);

        // Status line hint says "stop recording" (wide enough for the 5th composer hint).
        let text = screen(&mut app, 180);
        assert!(text.contains("stop recording"), "{text}");

        // Palette entry says "Stop recording" and is enabled.
        let ctx = app.palette_context();
        assert!(ctx.recording);
        let palette = crate::palette::Palette::open(ctx);
        let entry = palette
            .entries
            .iter()
            .find(|e| e.target == crate::palette::Target::Action(Action::RecordVoice))
            .unwrap();
        assert!(entry.enabled);
        assert_eq!(entry.title, "Stop recording");

        // Composer context menu says "Stop recording".
        app.open_menu_by_key();
        let Some(Modal::Menu(menu)) = &app.modal else {
            panic!("composer menu did not open");
        };
        assert!(menu.items.iter().any(|item| item.label == "Stop recording"));
    }

    #[test]
    fn clicking_play_on_a_waiting_voice_note_plays_and_stops_it_without_removing_the_file() {
        use crate::audio::{Control, End, Kind, NoPlayer, PlayerEvent, PlayerUpdate};

        let alt_l = || Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT));
        let down = MouseEventKind::Down(MouseButton::Left);

        let dir = Bin::new("preview", &[]);
        let mut app = offered();
        app.set_focus(Pane::Composer);

        // No waiting note: Alt-L says so.
        app.on_event(alt_l());
        assert_eq!(toast(&app), "No voice message waiting in the composer");

        // Record a waiting voice note in demo mode (no Matrix worker needed).
        app.perform(Action::RecordVoice);
        let _rec = recording(&mut app);
        let path = done(&mut app, &dir, 12);
        assert!(path.exists());

        // Without a player: clicking ▶ explains why.
        let text = screen(&mut app, 100);
        assert!(text.contains("▶"), "{text}");
        let play_hit = app.hits.rect_of(HitId::AttachmentPlay(0)).unwrap();
        app.on_event(mouse(down, play_hit.x, play_hit.y));
        assert_eq!(toast(&app), NoPlayer::NoneFound.message());
        assert!(app.take_audio_start().is_none());

        // Over SSH: explains why.
        app.set_test_player(Kind::Other, dir.0.clone());
        app.audio.ssh = true;
        app.on_event(mouse(down, play_hit.x, play_hit.y));
        assert_eq!(toast(&app), "Audio plays on the machine matrix-rat runs on");
        assert!(app.take_audio_start().is_none());
        app.audio.ssh = false;

        // Click ▶: starts playback on the note's file; chip flips to ■ and shows 0:00 / 0:12.
        app.toast = None;
        app.on_event(mouse(down, play_hit.x, play_hit.y));
        let req = app.take_audio_start().expect("player request");
        assert_eq!(req.file.path(), path.as_path());
        let play_id = req.id;
        assert!(app.playing_voice_note_here());
        let text = screen(&mut app, 100);
        assert!(text.contains("■"), "{text}");
        assert!(text.contains("0:00 / 0:12"), "{text}");
        assert!(text.contains("stop voice"), "{text}");

        // Player finishes and drops its Arc<TempAudio>: the note's file MUST stay intact!
        drop(req);
        app.on_audio(PlayerUpdate {
            id: play_id,
            event: PlayerEvent::Ended(End::Finished),
        });
        assert!(!app.playing_voice_note_here());
        assert!(
            path.exists(),
            "playing must not remove the waiting note's file"
        );

        // Alt-L plays it; clicking ■ stops it, file still intact.
        app.on_event(alt_l());
        let mut req = app.take_audio_start().expect("player request via Alt-L");
        let _ = screen(&mut app, 100);
        let stop_hit = app.hits.rect_of(HitId::AttachmentPlay(0)).unwrap();
        app.on_event(mouse(down, stop_hit.x, stop_hit.y));
        assert_eq!(req.control.try_recv(), Ok(Control::Stop));
        drop(req);
        assert!(path.exists());

        // Starting a recording stops a playing waiting note; playing while recording is refused.
        app.on_event(alt_l());
        let mut req = app.take_audio_start().expect("player request");
        app.perform(Action::RecordVoice);
        let _rec = recording(&mut app);
        assert_eq!(req.control.try_recv(), Ok(Control::Stop));
        drop(req);
        assert!(!app.playing_voice_note_here());
        app.on_event(alt_l());
        assert_eq!(toast(&app), "Stop the recording before playing a sound");
        app.cancel_recording();

        // Opening a thread stops composer draft playback immediately.
        app.on_event(alt_l());
        let mut req = app.take_audio_start().expect("player request");
        let msg_idx = app
            .messages()
            .iter()
            .position(|m| matches!(m.kind, crate::model::MsgKind::Text))
            .unwrap();
        app.open_thread(msg_idx);
        assert_eq!(req.control.try_recv(), Ok(Control::Stop));
        drop(req);
        app.close_thread();

        // Removing the waiting note (✕) stops its playback and unlinks once the player drops.
        app.on_event(alt_l());
        let mut req = app.take_audio_start().expect("player request");
        let _ = screen(&mut app, 100);
        let close_hit = app.hits.rect_of(HitId::Attachment(0)).unwrap();
        app.on_event(mouse(down, close_hit.x, close_hit.y));
        assert_eq!(req.control.try_recv(), Ok(Control::Stop));
        assert!(app.attachments().is_empty());
        drop(req);
        assert!(!path.exists());
    }

    #[test]
    fn multiple_waiting_voice_notes_can_be_played_removed_and_sent_independently() {
        use crate::audio::{Control, Kind};

        let alt_l = || Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT));
        let dir = Bin::new("multi", &[]);
        let mut app = offered();
        app.set_test_player(Kind::Other, dir.0.clone());
        app.set_focus(Pane::Composer);

        app.perform(Action::RecordVoice);
        let _r1 = recording(&mut app);
        let path1 = done(&mut app, &dir, 5);

        app.perform(Action::RecordVoice);
        let _r2 = recording(&mut app);
        let path2 = done(&mut app, &dir, 12);

        assert_eq!(app.attachments().len(), 2);
        let _ = screen(&mut app, 100);
        assert!(app.hits.rect_of(HitId::AttachmentPlay(0)).is_some());
        assert!(app.hits.rect_of(HitId::AttachmentPlay(1)).is_some());
        assert!(app.hits.rect_of(HitId::Attachment(0)).is_some());
        assert!(app.hits.rect_of(HitId::Attachment(1)).is_some());

        // Alt-L plays the newest waiting note (index 1, 0:12).
        app.on_event(alt_l());
        let mut req = app.take_audio_start().expect("newest note plays");
        assert_eq!(req.file.path(), path2.as_path());

        // Removing the OTHER note (index 0) leaves index 1's playback running.
        app.remove_attachment(0);
        assert!(!path1.exists());
        assert!(req.control.try_recv().is_err());
        assert!(app.playing_voice_note_here());

        // Record a third note, then send both: stops playback and sends both notes.
        app.stop_audio();
        drop(req);
        app.perform(Action::RecordVoice);
        let _r3 = recording(&mut app);
        let path3 = done(&mut app, &dir, 8);
        assert_eq!(app.attachments().len(), 2);

        app.on_event(alt_l());
        let mut req = app.take_audio_start().expect("plays newest before send");
        let before = app.messages().len();
        app.on_event(key(KeyCode::Enter));
        assert_eq!(req.control.try_recv(), Ok(Control::Stop));
        drop(req);
        assert_eq!(app.messages().len(), before + 2);
        assert_eq!(app.messages()[before].body, "🎤 Voice message · 0:12");
        assert_eq!(app.messages()[before + 1].body, "🎤 Voice message · 0:08");
        assert!(!path2.exists());
        assert!(!path3.exists());
    }

    #[test]
    fn voice_chip_and_recording_line_render_cleanly_at_80x24_and_narrow_widths() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;

        let dir = Bin::new("widths", &[]);
        let mut app = offered();
        app.set_focus(Pane::Composer);

        // 80x24 while recording: full recording row fits cleanly.
        app.perform(Action::RecordVoice);
        let _req = recording(&mut app);
        update(
            &mut app,
            RecordEvent::Level {
                elapsed: Duration::from_secs(7),
                level: 1024,
            },
        );
        let text_80 = screen(&mut app, 80);
        assert!(
            text_80.contains("● 0:07 █ · Alt-M stops · Esc discards"),
            "{text_80}"
        );

        // 80x24 with waiting voice note: full chip + ▶ and ✕ clickable.
        let _path1 = done(&mut app, &dir, 12);
        let text_80 = screen(&mut app, 80);
        assert!(text_80.contains("🎤 Voice message · 0:12 · "), "{text_80}");
        assert!(text_80.contains("▶"), "{text_80}");
        assert!(text_80.contains("✕"), "{text_80}");
        assert!(app.hits.rect_of(HitId::AttachmentPlay(0)).is_some());
        assert!(app.hits.rect_of(HitId::Attachment(0)).is_some());

        // Narrow screen (26 cols): drops size & label to `🎤 0:12 ▶  ✕`, both clickable!
        let text_26 = screen(&mut app, 26);
        assert!(text_26.contains("🎤 0:12"), "{text_26}");
        assert!(app.hits.rect_of(HitId::AttachmentPlay(0)).is_some());
        assert!(app.hits.rect_of(HitId::Attachment(0)).is_some());

        // Direct chip progressive-drop check across exact widths:
        // 30 cols -> drops size first ("🎤 Voice message · 0:12"), keeps ▶ and ✕.
        // 22 cols and 14 cols -> drops label next ("🎤 0:12"), keeps ▶ and ✕.
        let render_chip = |app: &App, w: u16, h: u16| -> (String, crate::ui::hit::HitMap) {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut hits = crate::ui::hit::HitMap::default();
            let files = app.attachments();
            terminal
                .draw(|frame| {
                    crate::ui::attach::draw(
                        frame,
                        files,
                        &app.attach,
                        &app.audio,
                        &app.theme,
                        None,
                        &mut hits,
                        Rect::new(0, 0, w, h),
                    );
                })
                .unwrap();
            (format!("{}", terminal.backend()), hits)
        };

        let (r30, h30) = render_chip(&app, 30, 1);
        assert!(r30.contains("🎤 Voice message · 0:12"), "{r30}");
        assert!(!r30.contains("kB"), "{r30}");
        assert!(h30.rect_of(HitId::AttachmentPlay(0)).is_some());
        assert!(h30.rect_of(HitId::Attachment(0)).is_some());

        for w in [22u16, 14u16] {
            let (row, hits) = render_chip(&app, w, 1);
            assert!(row.contains("🎤 0:12"), "width={w}: {row}");
            assert!(row.contains("▶"), "width={w}: {row}");
            assert!(row.contains("✕"), "width={w}: {row}");
            assert!(
                hits.rect_of(HitId::AttachmentPlay(0)).is_some(),
                "width={w}"
            );
            assert!(hits.rect_of(HitId::Attachment(0)).is_some(), "width={w}");
        }

        // Overflow single row with a voice note first uses 🎤, not 📎.
        app.perform(Action::RecordVoice);
        let _req2 = recording(&mut app);
        let _path2 = done(&mut app, &dir, 5);
        let (overflow, _) = render_chip(&app, 30, 1);
        assert!(overflow.contains("🎤 Voice message  +1 more"), "{overflow}");
        assert!(!overflow.contains("📎"), "{overflow}");
    }
}
