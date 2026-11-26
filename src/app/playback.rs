//! Voice messages and audio played in here (see `crate::audio`): the one sound there
//! may be, the actions that start, stop, pause and move it, and what the worker (the
//! file) and the player (where it is, how it ended) answer.
//!
//! A sound belongs to the room it was started in. It stops on `x` on its message again
//! (or a click on its `■`), when another one starts, when another room is opened or the
//! chat is closed, when the room is left, on logging out, on `Ctrl-Z` and on quitting.
//! It goes on while scrolling, selecting other messages, in popups, and in a thread of
//! its room, also one where its message is not shown: there `x` with no voice or audio
//! message selected (or "Stop audio" in the palette) stops it. `Esc` gives up a
//! download that is on its way.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use super::App;
use super::attach::Scope;
use crate::action::Action;
use crate::audio::recorder::{NoRecorder, Recorder};
use crate::audio::{
    self, Control, End, NoPlayer, Player, PlayerEvent, PlayerUpdate, Reported, TempAudio,
};
use crate::images::human_size;
use crate::keymap::Context;
use crate::matrix::MatrixCmd;
use crate::model::{AudioMeta, Message, Room};
use crate::palette::AudioCaps;
use crate::time::Instant;

/// The sound there is, and what plays it.
pub struct AudioState {
    /// The `audio_player` setting as written.
    pub setting: String,
    /// What the setting comes to on this system; looked up again when it changes.
    player: Result<Player, NoPlayer>,
    /// The `audio_recorder` setting as written.
    pub recorder_setting: String,
    /// What records a voice message here, or why nothing does; looked up again when
    /// the setting changes (see `voice.rs`).
    pub(super) recorder: Result<Recorder, NoRecorder>,
    /// The recording that runs, if one does (see `voice.rs`).
    pub(super) take: Option<super::voice::Take>,
    /// A recording to start; the event loop takes it (see [`App::take_record_start`]).
    pub(super) record_start: Option<audio::record::RecordRequest>,
    /// Where tests look for the recorder: nothing of the machine they run on.
    #[cfg(test)]
    pub(super) recorder_env: audio::recorder::Env,
    /// Over SSH a sound would play on the far machine, and its microphone would
    /// record: neither is offered.
    pub(super) ssh: bool,
    /// The private folder for the copies and mpv's sockets; set by the event loop.
    pub(super) dir: Option<PathBuf>,
    /// Sounds and recordings are numbered from the same count.
    pub(super) next_id: u64,
    current: Option<Sound>,
    /// A player to start; the event loop takes it (see [`App::take_audio_start`]).
    start: Option<audio::Request>,
    /// What was last shown of the position, to redraw only when that changes.
    shown: Option<(u64, bool)>,
}

/// Before the app knows its system: no player (what the demo and tests start with).
impl Default for AudioState {
    fn default() -> Self {
        Self {
            setting: "auto".to_owned(),
            player: Err(NoPlayer::NoneFound),
            recorder_setting: "auto".to_owned(),
            recorder: Err(NoRecorder::NoMicrophone),
            take: None,
            record_start: None,
            #[cfg(test)]
            recorder_env: audio::recorder::Env::default(),
            ssh: false,
            dir: None,
            next_id: 0,
            current: None,
            start: None,
            shown: None,
        }
    }
}

/// Hand-written: the setting can name the user's files, and the sound's room and
/// message are ids.
impl fmt::Debug for AudioState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioState")
            .field("auto", &audio::is_auto(&self.setting))
            .field("player", &self.player)
            .field("recorder_auto", &audio::is_auto(&self.recorder_setting))
            .field("recorder", &self.recorder)
            .field("recording", &self.take)
            .field("record_start", &self.record_start.is_some())
            .field("ssh", &self.ssh)
            .field("dir", &self.dir.is_some())
            .field("current", &self.current)
            .field("start", &self.start)
            .finish_non_exhaustive()
    }
}

/// What plays: a voice or audio message in the timeline, or a recorded voice note
/// waiting in the composer.
enum SoundSource {
    Message {
        message_id: String,
        /// The message's event, where it has one. A thread shows its first message under
        /// another id than the room does: the event says it is the same message.
        event: Option<String>,
        /// The file's address: with the message's id, which message this is.
        key: String,
    },
    Draft {
        scope: Scope,
        path: PathBuf,
        duration: Duration,
    },
}

/// The sound: for which message or waiting voice note, and how far it got.
struct Sound {
    /// Answers about another are dropped.
    id: u64,
    room_id: String,
    source: SoundSource,
    phase: Phase,
}

impl Sound {
    /// Whether `message` is the one this sound belongs to.
    fn belongs_to(&self, message: &Message) -> bool {
        let SoundSource::Message {
            message_id,
            event,
            key,
        } = &self.source
        else {
            return false;
        };
        let same = *message_id == message.id
            || event
                .as_deref()
                .is_some_and(|event| message.event_id() == Some(event));
        same && message.file.as_ref().is_some_and(|file| file.key == *key)
    }

    /// Whether this sound is a preview of the waiting voice note at `path`.
    fn is_draft(&self, path: &Path) -> bool {
        matches!(&self.source, SoundSource::Draft { path: p, .. } if p == path)
    }

    fn draft_scope(&self) -> Option<&Scope> {
        match &self.source {
            SoundSource::Draft { scope, .. } => Some(scope),
            SoundSource::Message { .. } => None,
        }
    }
}

impl fmt::Debug for Sound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sound")
            .field("id", &self.id)
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
enum Phase {
    /// The file is on its way.
    Loading,
    Playing(Playing),
}

#[derive(Debug)]
struct Playing {
    control: UnboundedSender<Control>,
    started: Instant,
    /// mpv answers on its socket: the position is its own, pause and seek work.
    controllable: bool,
    reported: Reported,
}

/// What the timeline shows of the sound, on its message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioView {
    Loading,
    Playing {
        position: Duration,
        length: Option<Duration>,
        paused: bool,
    },
}

impl AudioState {
    /// For the app on this system: the settings, the player and the recorder they
    /// name, and whether this is a session over SSH.
    pub fn new(setting: &str, recorder_setting: &str) -> Self {
        Self {
            setting: setting.to_owned(),
            player: audio::choose_here(setting),
            recorder_setting: recorder_setting.to_owned(),
            recorder: audio::recorder::choose_here(recorder_setting),
            ssh: crate::portal::in_ssh_session(),
            ..Self::default()
        }
    }

    /// What the settings show: `auto: mpv`, `auto: none found`, or the command.
    pub fn setting_text(&self) -> String {
        if audio::is_auto(&self.setting) {
            return match &self.player {
                Ok(player) => format!("auto: {}", player.name()),
                Err(_) => "auto: none found".to_owned(),
            };
        }
        let command = crate::text::sanitize_line(self.setting.trim());
        match &self.player {
            Ok(_) => command,
            Err(_) => format!("{command} (not found)"),
        }
    }

    /// Playing is possible here (an account aside).
    pub(crate) fn offered(&self) -> bool {
        !self.ssh && self.player.is_ok()
    }

    /// What the composer chip shows for a waiting voice note that is playing.
    pub(crate) fn playing_draft(&self, path: &Path) -> Option<AudioView> {
        self.current
            .as_ref()
            .filter(|sound| sound.is_draft(path))
            .map(|sound| sound.view(None))
    }

    pub(super) fn is_draft_playing(&self, path: &Path) -> bool {
        self.current
            .as_ref()
            .is_some_and(|sound| sound.is_draft(path))
    }

    pub(crate) fn is_draft_playing_in(&self, scope: Option<&Scope>) -> bool {
        let Some(scope) = scope else {
            return false;
        };
        self.current
            .as_ref()
            .is_some_and(|sound| sound.draft_scope() == Some(scope))
    }

    /// Whether `message` of the room `room_id` is the sound.
    fn is(&self, room_id: &str, message: &Message) -> bool {
        self.current
            .as_ref()
            .is_some_and(|sound| sound.room_id == room_id && sound.belongs_to(message))
    }

    fn playing(&self) -> Option<&Playing> {
        match &self.current.as_ref()?.phase {
            Phase::Playing(playing) => Some(playing),
            Phase::Loading => None,
        }
    }

    /// The whole seconds and the pause shown, while it plays.
    fn shown_now(&self) -> Option<(u64, bool)> {
        match self.current.as_ref()?.view(None) {
            AudioView::Playing {
                position, paused, ..
            } => Some((position.as_secs(), paused)),
            AudioView::Loading => None,
        }
    }
}

impl Sound {
    /// Where it is: mpv's word where mpv answers, else the time since the player
    /// started. `meta`: what the message says about the sound.
    fn view(&self, meta: Option<&AudioMeta>) -> AudioView {
        let Phase::Playing(playing) = &self.phase else {
            return AudioView::Loading;
        };
        let fallback_len = match &self.source {
            SoundSource::Message { .. } => meta.and_then(|meta| meta.duration),
            SoundSource::Draft { duration, .. } => Some(*duration),
        };
        let length = playing.reported.length.or(fallback_len);
        let position = match playing.reported.position {
            Some(at) if playing.controllable => at,
            _ => playing.started.elapsed(),
        };
        AudioView::Playing {
            position: length.map_or(position, |length| position.min(length)),
            length,
            paused: playing.reported.paused,
        }
    }
}

impl App {
    /// Where the copies of sounds go; what an earlier run left there is swept away.
    pub(super) fn set_audio_dir(&mut self, dir: PathBuf) {
        audio::remove_leftovers(&dir);
        self.audio.dir = Some(dir);
    }

    /// A player for the event loop to start.
    pub(super) fn take_audio_start(&mut self) -> Option<audio::Request> {
        self.audio.start.take()
    }

    /// What `x` and `Space` would do now, for the palette, the menu and the hints.
    /// For the doctor: what the setting comes to, without the command itself.
    pub(super) fn audio_found(&self) -> crate::doctor::Audio {
        crate::doctor::Audio::from_choice(&self.audio.player)
    }

    pub fn audio_caps(&self) -> AudioCaps {
        let room_id = self.room_id();
        let selected = self
            .timeline
            .selected
            .and_then(|index| self.messages().get(index))
            .filter(|message| message.is_sound());
        let is_it = selected.is_some_and(|message| {
            room_id
                .as_deref()
                .is_some_and(|room_id| self.audio.is(room_id, message))
        });
        let playing = self.audio.playing();
        AudioCaps {
            active: self.audio.current.is_some(),
            can_play: selected.is_some() && self.matrix.is_some() && self.audio.offered(),
            selected: is_it,
            can_pause: is_it && playing.is_some_and(|playing| playing.controllable),
            paused: playing.is_some_and(|playing| playing.reported.paused),
        }
    }

    /// The message of `room` that the sound belongs to, and what it shows there.
    pub fn audio_in(&self, room: &Room) -> Option<(usize, AudioView)> {
        let sound = self
            .audio
            .current
            .as_ref()
            .filter(|sound| sound.room_id == room.id)?;
        let at = room
            .messages
            .iter()
            .rposition(|message| sound.belongs_to(message))?;
        let meta = room.messages[at].file.as_ref()?.audio.as_deref();
        Some((at, sound.view(meta)))
    }

    /// `x`: plays the selected voice or audio message, or stops it if it is what plays
    /// (or loads). With no such message selected, it stops what plays.
    pub(super) fn play_audio(&mut self) {
        let Some(room_id) = self.room_id() else {
            return;
        };
        let selected = self
            .timeline
            .selected
            .and_then(|index| self.messages().get(index))
            .filter(|message| message.is_sound())
            .map(|message| (self.audio.is(&room_id, message), message.clone()));
        match selected {
            Some((true, _)) => self.stop_audio(),
            Some((false, message)) => self.start_audio(room_id, message),
            None if self.audio.current.is_some() => self.stop_audio(),
            None => self.show_toast("Select a voice or audio message to play it"),
        }
    }

    fn start_audio(&mut self, room_id: String, message: Message) {
        let event = message.event_id().map(str::to_owned);
        let Some(file) = message.file else {
            return;
        };
        if self.matrix.is_none() {
            self.show_toast("The demo has no account to play audio with");
            return;
        }
        if self.audio.take.is_some() {
            self.show_toast("Stop the recording before playing a sound");
            return;
        }
        let save = self
            .keymap
            .label_in(Context::Timeline, Action::Save)
            .or_else(|| self.keymap.label_for(Action::Save))
            .unwrap_or("Saving")
            .to_owned();
        if self.audio.ssh {
            self.show_toast(format!(
                "Audio plays on the machine matrix-rat runs on: {save} saves the file instead"
            ));
            return;
        }
        if let Some(why) = self.audio.player.as_ref().err().copied() {
            self.show_toast(why.message());
            return;
        }
        if let Some(size) = file.size.filter(|size| *size > audio::MAX_FILE) {
            self.show_toast(format!(
                "{} is {}, too large to play here: {save} saves it",
                file.name,
                human_size(size)
            ));
            return;
        }
        let Some(dir) = self.audio.dir.clone() else {
            self.show_toast("Audio cannot be played here");
            return;
        };
        // One sound at a time.
        self.stop_audio();
        self.audio.next_id += 1;
        let id = self.audio.next_id;
        self.audio.current = Some(Sound {
            id,
            room_id,
            source: SoundSource::Message {
                event,
                message_id: message.id,
                key: file.key.clone(),
            },
            phase: Phase::Loading,
        });
        self.send_matrix(MatrixCmd::FetchAudio { id, file, dir });
        self.dirty = true;
    }

    /// Whether the open conversation has a recorded voice message waiting in the composer.
    pub(crate) fn has_waiting_voice_note(&self) -> bool {
        self.attachments()
            .iter()
            .any(|file| self.attach.voice(&file.path).is_some())
    }

    /// Whether a waiting voice message in the open conversation can be played here.
    pub(crate) fn can_play_voice_note(&self) -> bool {
        self.has_waiting_voice_note() && self.audio.offered()
    }

    /// Whether a waiting voice message in the open conversation is currently playing.
    pub(crate) fn playing_voice_note_here(&self) -> bool {
        self.audio.is_draft_playing_in(self.attach_scope().as_ref())
    }

    /// `Alt-L` and the palette: plays the newest waiting voice message in the open
    /// conversation, or stops the waiting voice message that plays here.
    pub(super) fn play_voice_note(&mut self) {
        let Some(scope) = self.attach_scope() else {
            return;
        };
        if self.audio.is_draft_playing_in(Some(&scope)) {
            self.stop_audio();
            return;
        }
        let chosen = self
            .attach
            .files(Some(&scope))
            .iter()
            .rev()
            .find_map(|file| {
                self.attach
                    .voice(&file.path)
                    .map(|voice| (voice.file_arc(), voice.duration))
            });
        match chosen {
            Some((file, duration)) => self.start_draft_audio(scope, file, duration),
            None => self.show_toast("No voice message waiting in the composer"),
        }
    }

    /// Click on `▶` / `■` on waiting attachment `index`: plays that waiting voice
    /// message, or stops it if it is what plays.
    pub(super) fn play_voice_note_at(&mut self, index: usize) {
        if self.modal.is_some() || self.help_open || self.images.viewer_open() {
            return;
        }
        let Some(scope) = self.attach_scope() else {
            return;
        };
        let Some(path) = self
            .attach
            .files(Some(&scope))
            .get(index)
            .map(|file| file.path.clone())
        else {
            return;
        };
        if self.audio.is_draft_playing(&path) {
            self.stop_audio();
            return;
        }
        let Some((file, duration)) = self
            .attach
            .voice(&path)
            .map(|voice| (voice.file_arc(), voice.duration))
        else {
            return;
        };
        self.start_draft_audio(scope, file, duration);
    }

    fn start_draft_audio(&mut self, scope: Scope, file: Arc<TempAudio>, duration: Duration) {
        if self.audio.take.is_some() {
            self.show_toast("Stop the recording before playing a sound");
            return;
        }
        if self.audio.ssh {
            self.show_toast("Audio plays on the machine matrix-rat runs on");
            return;
        }
        let player = match self.audio.player.clone() {
            Ok(player) => player,
            Err(why) => {
                self.show_toast(why.message());
                return;
            }
        };
        let socket = if player.controllable() {
            self.audio.dir.as_deref().and_then(audio::socket_path)
        } else {
            None
        };
        // One sound at a time.
        self.stop_audio();
        self.audio.next_id += 1;
        let id = self.audio.next_id;
        let (control, receiver) = unbounded_channel();
        let path = file.path().to_owned();
        let room_id = scope.0.clone();
        self.audio.current = Some(Sound {
            id,
            room_id,
            source: SoundSource::Draft {
                scope,
                path,
                duration,
            },
            phase: Phase::Playing(Playing {
                control,
                started: Instant::now(),
                controllable: false,
                reported: Reported::default(),
            }),
        });
        self.audio.start = Some(audio::Request {
            id,
            player,
            file,
            socket,
            control: receiver,
        });
        self.dirty = true;
    }

    /// Stops playback if a waiting voice note (not a timeline message) is playing.
    pub(super) fn stop_draft_audio(&mut self) {
        if self
            .audio
            .current
            .as_ref()
            .is_some_and(|sound| matches!(sound.source, SoundSource::Draft { .. }))
        {
            self.stop_audio();
        }
    }

    /// Stops playback if the waiting voice note at `path` is what plays.
    pub(super) fn stop_draft_audio_for(&mut self, path: &Path) {
        if self.audio.is_draft_playing(path) {
            self.stop_audio();
        }
    }

    /// Stops what plays, or gives up its download; nothing if nothing plays.
    pub(super) fn stop_audio(&mut self) {
        // A player not started yet: its file goes with it.
        self.audio.start = None;
        let Some(sound) = self.audio.current.take() else {
            return;
        };
        match sound.phase {
            Phase::Loading => self.send_matrix(MatrixCmd::CancelAudio),
            // The player's task kills it (also if it has gone already).
            Phase::Playing(playing) => {
                let _ = playing.control.send(Control::Stop);
            }
        }
        self.audio.shown = None;
        self.dirty = true;
    }

    /// Whether a sound's file is on its way (`Esc` gives it up).
    pub(super) fn audio_loading(&self) -> bool {
        self.audio
            .current
            .as_ref()
            .is_some_and(|sound| matches!(sound.phase, Phase::Loading))
    }

    /// `Space` and the arrows on the message that plays: pause, go on, move (mpv). On
    /// anything else these keys do nothing, as they did before.
    pub(super) fn control_audio(&mut self, control: Control) {
        if !self.audio_caps().selected {
            return;
        }
        let Some(playing) = self.audio.playing() else {
            return;
        };
        if playing.controllable {
            let _ = playing.control.send(control);
            return;
        }
        // mpv may not have opened its socket yet; any other player cannot do it.
        let mpv = self.audio.player.as_ref().is_ok_and(Player::controllable);
        if !mpv {
            self.show_toast("Pause and seek need mpv as the audio player");
        }
    }

    pub(super) fn on_audio_fetched(&mut self, id: u64, result: Result<TempAudio, String>) {
        let wanted = self
            .audio
            .current
            .as_ref()
            .is_some_and(|sound| sound.id == id && matches!(sound.phase, Phase::Loading));
        if !wanted {
            // Given up meanwhile: the file goes with the answer.
            return;
        }
        self.dirty = true;
        let file = match result {
            Ok(file) => file,
            Err(why) => {
                self.audio.current = None;
                self.show_toast(format!("Could not play it: {why}"));
                return;
            }
        };
        let Ok(player) = self.audio.player.clone() else {
            self.audio.current = None;
            return;
        };
        let socket = if player.controllable() {
            self.audio.dir.as_deref().and_then(audio::socket_path)
        } else {
            None
        };
        let (control, receiver) = unbounded_channel();
        if let Some(sound) = &mut self.audio.current {
            sound.phase = Phase::Playing(Playing {
                control,
                started: Instant::now(),
                controllable: false,
                reported: Reported::default(),
            });
        }
        self.audio.start = Some(audio::Request {
            id,
            player,
            file: Arc::new(file),
            socket,
            control: receiver,
        });
    }

    pub(super) fn on_audio(&mut self, update: PlayerUpdate) {
        let Some(Sound {
            phase: Phase::Playing(playing),
            ..
        }) = self
            .audio
            .current
            .as_mut()
            .filter(|sound| sound.id == update.id)
        else {
            return;
        };
        match update.event {
            PlayerEvent::Controllable => {
                playing.controllable = true;
                self.dirty = true;
            }
            PlayerEvent::State(reported) => {
                playing.reported = reported;
                self.note_audio_shown();
            }
            PlayerEvent::Ended(end) => {
                self.audio.current = None;
                self.audio.shown = None;
                self.dirty = true;
                if end == End::Failed {
                    self.show_toast("The player could not play this file");
                }
            }
        }
    }

    /// A redraw when what is shown of the position changed.
    fn note_audio_shown(&mut self) {
        let shown = self.audio.shown_now();
        if shown != self.audio.shown {
            self.audio.shown = shown;
            self.dirty = true;
        }
    }

    /// On every tick: a sound whose room (or waiting voice note) is not open any more
    /// stops; one that plays is redrawn when its second changes.
    pub(super) fn audio_tick(&mut self) {
        let Some(sound) = &self.audio.current else {
            return;
        };
        let still_here = match &sound.source {
            SoundSource::Message { .. } => {
                self.room_id().as_deref() == Some(sound.room_id.as_str())
            }
            SoundSource::Draft { scope, path, .. } => {
                self.attach_scope().as_ref() == Some(scope)
                    && self
                        .attach
                        .files(Some(scope))
                        .iter()
                        .any(|file| file.path == *path)
            }
        };
        if !still_here {
            self.stop_audio();
            return;
        }
        self.note_audio_shown();
    }

    /// Enter on the audio player row: the setting is kept, and looked up at once.
    pub(super) fn set_audio_player(&mut self, text: &str) {
        let setting = if audio::is_auto(text) {
            "auto".to_owned()
        } else {
            text.trim().to_owned()
        };
        self.audio.player = audio::choose_here(&setting);
        self.audio.setting = setting.clone();
        self.save_setting("audio_player", setting);
        // What plays goes on with the player it has.
        let said = match &self.audio.player {
            Ok(player) => format!("Voice and audio play with {}", player.name()),
            Err(why) => why.message().to_owned(),
        };
        self.show_toast(said);
    }

    /// For tests: a player that is never started (the test takes its request).
    #[cfg(test)]
    pub(super) fn set_test_player(&mut self, kind: audio::Kind, dir: PathBuf) {
        self.audio.player = Ok(Player::for_test(PathBuf::from("/nonexistent/player"), kind));
        self.audio.dir = Some(dir);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use matrix_sdk::ruma::events::room::MediaSource;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::action::Pane;
    use crate::audio::Kind;
    use crate::input::{
        InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use crate::matrix::{MatrixEvent, MatrixHandle, SyncStatus};
    use crate::model::{Attachment, ItemRef, MediaKind, RoomKind};

    /// A fresh empty folder for one test, removed at its end.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            // Short: a socket's path in it must fit a socket's address.
            let path = std::env::temp_dir().join(format!(
                "mr-{name}-{}",
                crate::time::SystemTime::now()
                    .duration_since(crate::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
                    % 1_000_000_000
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn key(code: KeyCode) -> InputEvent {
        InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// A voice message `id`, twelve seconds long, with a waveform if `waveform`.
    fn voice(id: &str, waveform: bool) -> Message {
        let address = format!("mxc://example.org/{id}");
        let meta = AudioMeta {
            duration: Some(Duration::from_secs(12)),
            waveform: if waveform {
                vec![
                    120, 340, 610, 900, 1024, 700, 450, 260, 520, 830, 990, 640, 300, 150, 380,
                    720, 940, 560, 240, 100, 330, 680, 870, 410,
                ]
            } else {
                Vec::new()
            },
            mime: Some("audio/ogg".to_owned()),
        };
        let bars = if waveform {
            format!("\n{}", audio::bars(&meta.waveform))
        } else {
            String::new()
        };
        Message {
            id: id.to_owned(),
            item: Some(ItemRef::Event(format!("${id}"))),
            sender: "alice".to_owned(),
            time: "10:00".to_owned(),
            day: "Today".to_owned(),
            body: format!("🎤 {id}.ogg · 0:12 · 2.0 kB{bars}"),
            file: Some(Attachment {
                key: address.clone(),
                name: format!("{id}.ogg"),
                size: Some(2000),
                kind: MediaKind::Voice,
                source: Box::new(MediaSource::Plain(address.as_str().into())),
                audio: Some(Arc::new(meta)),
            }),
            ..Message::default()
        }
    }

    fn text(id: &str, body: &str) -> Message {
        Message {
            id: id.to_owned(),
            item: Some(ItemRef::Event(format!("${id}"))),
            sender: "bob".to_owned(),
            time: "10:01".to_owned(),
            day: "Today".to_owned(),
            body: body.to_owned(),
            ..Message::default()
        }
    }

    /// An app with an account (a fake worker), room Alpha showing `messages` and an
    /// empty room Beta, and an mpv that is never started.
    fn app_with(messages: Vec<Message>, dir: &Dir) -> (App, UnboundedReceiver<MatrixCmd>) {
        let mut app = App::demo();
        let (handle, commands) = MatrixHandle::for_test();
        app.matrix = Some(handle);
        app.sync = Some(SyncStatus::Synced);
        let mut alpha = Room::new("!alpha", "Alpha", RoomKind::Room);
        alpha.messages = messages;
        app.rooms = vec![alpha, Room::new("!beta", "Beta", RoomKind::Room)];
        app.active = 0;
        app.focus = Pane::Timeline;
        app.timeline.reached_start = true;
        app.set_test_player(Kind::Mpv, dir.0.clone());
        (app, commands)
    }

    /// What was asked of the worker about sounds.
    fn asked(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            if matches!(
                command,
                MatrixCmd::FetchAudio { .. } | MatrixCmd::CancelAudio
            ) {
                seen.push(command.describe());
            }
        }
        seen
    }

    fn toast(app: &App) -> &str {
        app.toast.as_ref().map_or("", |(text, _)| text.as_str())
    }

    /// The file arrives for sound `id`: the player is to be started. Its request, as
    /// the event loop would take it.
    fn fetched(app: &mut App, id: u64, dir: &Dir) -> audio::Request {
        let file = TempAudio::create(&dir.0, Some("audio/ogg"), b"OggS").unwrap();
        app.on_matrix(MatrixEvent::AudioFetched {
            id,
            result: Ok(file),
        });
        app.take_audio_start().expect("a player to start")
    }

    fn select(app: &mut App, index: usize) {
        app.focus = Pane::Timeline;
        app.timeline.selected = Some(index);
    }

    fn update(app: &mut App, id: u64, event: PlayerEvent) {
        app.on_audio(PlayerUpdate { id, event });
    }

    #[test]
    fn x_plays_and_x_again_stops() {
        let dir = Dir::new("toggle");
        let (mut app, mut commands) = app_with(vec![text("t", "hi"), voice("v1", true)], &dir);
        select(&mut app, 1);
        assert!(app.audio_caps().can_play);
        app.on_event(key(KeyCode::Char('x')));
        assert_eq!(asked(&mut commands), ["FetchAudio(sound 1)"]);
        assert!(app.audio_loading());
        assert!(app.audio_caps().selected);
        let room = app.rooms[0].clone();
        assert_eq!(app.audio_in(&room), Some((1, AudioView::Loading)));

        let mut request = fetched(&mut app, 1, &dir);
        assert_eq!(request.id, 1);
        assert_eq!(request.player.kind(), Kind::Mpv);
        // mpv gets a control socket in the private folder, where a path that long can
        // be a socket's address (the test's folder may be too deep for one).
        assert_eq!(
            request.socket.is_some(),
            audio::socket_path(&dir.0).is_some()
        );
        assert!(
            request
                .socket
                .as_ref()
                .is_none_or(|socket| socket.starts_with(&dir.0))
        );
        assert!(!app.audio_loading());
        assert!(matches!(
            app.audio_in(&room),
            Some((1, AudioView::Playing { length: Some(length), paused: false, .. }))
                if length == Duration::from_secs(12)
        ));

        app.on_event(key(KeyCode::Char('x')));
        assert_eq!(request.control.try_recv(), Ok(Control::Stop));
        assert!(app.audio_in(&room).is_none());
        assert!(!app.audio_caps().active);
        // The player's last word about it changes nothing.
        update(&mut app, 1, PlayerEvent::Ended(End::Stopped));
        assert!(app.toast.is_none());
        assert!(asked(&mut commands).is_empty());
    }

    #[test]
    fn a_thread_shows_its_first_message_playing() {
        let dir = Dir::new("thread-root");
        let (mut app, mut commands) = app_with(vec![text("t", "hi"), voice("v1", true)], &dir);
        select(&mut app, 1);
        app.on_event(key(KeyCode::Char('x')));
        fetched(&mut app, 1, &dir);
        // The thread shows the message as its first, under another id.
        app.open_thread(1);
        assert!(app.thread_open());
        let room = app.rooms[0].clone();
        assert_ne!(room.messages[0].id, "v1");
        assert!(matches!(
            app.audio_in(&room),
            Some((0, AudioView::Playing { .. }))
        ));
        // `x` on it stops it instead of starting it again.
        select(&mut app, 0);
        assert!(app.audio_caps().selected);
        app.on_event(key(KeyCode::Char('x')));
        assert!(!app.audio_caps().active);
        assert_eq!(asked(&mut commands), ["FetchAudio(sound 1)"]);

        // The other way round: started in the thread, shown in the room.
        app.on_event(key(KeyCode::Char('x')));
        fetched(&mut app, 2, &dir);
        app.close_thread();
        assert!(!app.thread_open());
        let room = app.rooms[0].clone();
        assert!(matches!(
            app.audio_in(&room),
            Some((1, AudioView::Playing { .. }))
        ));
    }

    #[test]
    fn playing_another_stops_the_first() {
        let dir = Dir::new("another");
        let (mut app, mut commands) = app_with(vec![voice("v1", true), voice("v2", false)], &dir);
        select(&mut app, 0);
        app.perform(Action::PlayAudio);
        let mut first = fetched(&mut app, 1, &dir);
        select(&mut app, 1);
        app.perform(Action::PlayAudio);
        assert_eq!(first.control.try_recv(), Ok(Control::Stop));
        assert_eq!(
            asked(&mut commands),
            ["FetchAudio(sound 1)", "FetchAudio(sound 2)"]
        );
        // A download that is replaced is given up by the worker itself; a late answer
        // for the first is dropped, with its file.
        let late = TempAudio::create(&dir.0, None, b"x").unwrap();
        let late_path = late.path().to_owned();
        app.on_matrix(MatrixEvent::AudioFetched {
            id: 1,
            result: Ok(late),
        });
        assert!(app.take_audio_start().is_none());
        assert!(!late_path.exists());
        let room = app.rooms[0].clone();
        assert_eq!(app.audio_in(&room), Some((1, AudioView::Loading)));
        // And news of the first player is not about the second.
        update(&mut app, 1, PlayerEvent::Ended(End::Failed));
        assert!(app.toast.is_none());
        assert!(app.audio_loading());
    }

    #[test]
    fn a_download_is_given_up_by_x_esc_and_a_late_answer_is_dropped() {
        let dir = Dir::new("cancel");
        let (mut app, mut commands) = app_with(vec![voice("v1", true)], &dir);
        select(&mut app, 0);
        app.on_event(key(KeyCode::Char('x')));
        app.on_event(key(KeyCode::Char('x')));
        assert_eq!(asked(&mut commands), ["FetchAudio(sound 1)", "CancelAudio"]);
        let late = TempAudio::create(&dir.0, None, b"x").unwrap();
        let path = late.path().to_owned();
        app.on_matrix(MatrixEvent::AudioFetched {
            id: 1,
            result: Ok(late),
        });
        assert!(app.take_audio_start().is_none());
        assert!(!path.exists(), "the file of a sound not wanted is removed");

        // Esc: the download is the first thing it steps back from; the message stays
        // selected.
        app.on_event(key(KeyCode::Char('x')));
        app.on_event(key(KeyCode::Esc));
        assert_eq!(asked(&mut commands), ["FetchAudio(sound 2)", "CancelAudio"]);
        assert_eq!(app.timeline.selected, Some(0));
        assert!(!app.audio_caps().active);

        // A download that fails says why, in our words.
        app.on_event(key(KeyCode::Char('x')));
        app.on_matrix(MatrixEvent::AudioFetched {
            id: 3,
            result: Err("the download failed".to_owned()),
        });
        assert_eq!(toast(&app), "Could not play it: the download failed");
        assert!(!app.audio_caps().active);
    }

    #[test]
    fn the_end_is_the_players_and_a_failure_is_said_in_our_words() {
        let dir = Dir::new("end");
        let (mut app, _commands) = app_with(vec![voice("v1", true)], &dir);
        select(&mut app, 0);
        app.perform(Action::PlayAudio);
        fetched(&mut app, 1, &dir);
        update(&mut app, 1, PlayerEvent::Ended(End::Finished));
        assert!(!app.audio_caps().active);
        assert!(app.toast.is_none());

        app.perform(Action::PlayAudio);
        fetched(&mut app, 2, &dir);
        update(&mut app, 2, PlayerEvent::Ended(End::Failed));
        assert!(!app.audio_caps().active);
        assert_eq!(toast(&app), "The player could not play this file");
    }

    #[test]
    fn leaving_the_room_or_closing_the_chat_stops_it_and_scrolling_does_not() {
        let dir = Dir::new("rooms");
        let (mut app, mut commands) = app_with(vec![voice("v1", true), text("t", "hi")], &dir);
        select(&mut app, 0);
        app.perform(Action::PlayAudio);
        let mut request = fetched(&mut app, 1, &dir);
        // Selecting another message, scrolling, a popup, the tick: it goes on.
        select(&mut app, 1);
        app.perform(Action::PageUp);
        app.perform(Action::ToggleHelp);
        app.perform(Action::ToggleHelp);
        app.on_tick();
        assert!(request.control.try_recv().is_err());
        assert!(app.audio_caps().active);
        // `x` on a message that is not voice or audio stops it.
        app.on_event(key(KeyCode::Char('x')));
        assert_eq!(request.control.try_recv(), Ok(Control::Stop));

        // Another room.
        select(&mut app, 0);
        app.perform(Action::PlayAudio);
        let mut request = fetched(&mut app, 2, &dir);
        app.perform(Action::OpenRoom(1));
        assert_eq!(request.control.try_recv(), Ok(Control::Stop));
        assert!(!app.audio_caps().active);

        // The chat closed while it loads: the download is given up.
        app.perform(Action::OpenRoom(0));
        select(&mut app, 0);
        asked(&mut commands);
        app.perform(Action::PlayAudio);
        app.close_chat();
        assert_eq!(asked(&mut commands), ["FetchAudio(sound 3)", "CancelAudio"]);

        // The room gone from the list (left, here or elsewhere): stopped on the tick.
        app.perform(Action::OpenRoom(0));
        select(&mut app, 0);
        app.perform(Action::PlayAudio);
        let mut request = fetched(&mut app, 4, &dir);
        app.rooms.remove(0);
        app.active = 0;
        app.on_tick();
        assert_eq!(request.control.try_recv(), Ok(Control::Stop));
        assert!(!app.audio_caps().active);
    }

    #[test]
    fn quitting_logging_out_and_suspending_stop_it() {
        let dir = Dir::new("quit");
        let (mut app, _commands) = app_with(vec![voice("v1", true)], &dir);
        for stop in [Action::Suspend, Action::Quit] {
            select(&mut app, 0);
            app.perform(Action::PlayAudio);
            let id = app.audio.next_id;
            let mut request = fetched(&mut app, id, &dir);
            app.perform(stop);
            // Quitting: the event loop stops it on the way out; the app going away
            // closes the channel, which stops it too.
            if stop == Action::Quit {
                assert!(app.should_quit);
                app.stop_audio();
            }
            assert_eq!(request.control.try_recv(), Ok(Control::Stop), "{stop:?}");
        }
        select(&mut app, 0);
        app.perform(Action::PlayAudio);
        let id = app.audio.next_id;
        let request = fetched(&mut app, id, &dir);
        app.reset_session();
        assert!(!app.audio_caps().active);
        drop(app);
        let mut control = request.control;
        assert!(matches!(control.try_recv(), Ok(Control::Stop) | Err(_)));
    }

    #[test]
    fn suspending_from_the_picture_viewer_stops_it_too() {
        let dir = Dir::new("viewer");
        let (mut app, _commands) = app_with(vec![voice("v1", true)], &dir);
        select(&mut app, 0);
        app.perform(Action::PlayAudio);
        let id = app.audio.next_id;
        let mut request = fetched(&mut app, id, &dir);
        // The viewer handles its keys itself.
        app.on_viewer_key(
            crate::input::KeyCode::Char('z'),
            crate::input::KeyModifiers::CONTROL,
        );
        assert!(app.suspend_requested);
        assert_eq!(request.control.try_recv(), Ok(Control::Stop));
    }

    #[test]
    fn pause_and_seek_on_the_message_that_plays_with_mpv() {
        let dir = Dir::new("pause");
        let (mut app, _commands) = app_with(vec![voice("v1", true), text("t", "hi")], &dir);
        select(&mut app, 0);
        app.perform(Action::PlayAudio);
        let mut request = fetched(&mut app, 1, &dir);
        // Before mpv answers on its socket nothing is sent, and nothing said.
        app.on_event(key(KeyCode::Char(' ')));
        assert!(request.control.try_recv().is_err());
        assert!(app.toast.is_none());
        update(&mut app, 1, PlayerEvent::Controllable);
        assert!(app.audio_caps().can_pause);
        app.on_event(key(KeyCode::Char(' ')));
        app.on_event(key(KeyCode::Left));
        app.on_event(key(KeyCode::Right));
        let sent: Vec<Control> = std::iter::from_fn(|| request.control.try_recv().ok()).collect();
        assert_eq!(
            sent,
            [Control::TogglePause, Control::Seek(-5), Control::Seek(5)]
        );
        // What mpv says is what is shown.
        update(
            &mut app,
            1,
            PlayerEvent::State(Reported {
                position: Some(Duration::from_secs(5)),
                paused: true,
                length: Some(Duration::from_secs(13)),
            }),
        );
        assert!(app.audio_caps().paused);
        let room = app.rooms[0].clone();
        assert_eq!(
            app.audio_in(&room),
            Some((
                0,
                AudioView::Playing {
                    position: Duration::from_secs(5),
                    length: Some(Duration::from_secs(13)),
                    paused: true
                }
            ))
        );
        // On another message these keys do nothing, as before.
        select(&mut app, 1);
        app.on_event(key(KeyCode::Char(' ')));
        app.on_event(key(KeyCode::Left));
        assert!(request.control.try_recv().is_err());
        assert!(!app.audio_caps().can_pause);
    }

    #[test]
    fn other_players_have_no_pause_and_a_clock_for_the_position() {
        let dir = Dir::new("clock");
        let (mut app, _commands) = app_with(vec![voice("v1", true)], &dir);
        app.set_test_player(Kind::PwPlay, dir.0.clone());
        select(&mut app, 0);
        app.perform(Action::PlayAudio);
        let mut request = fetched(&mut app, 1, &dir);
        assert!(request.socket.is_none());
        app.on_event(key(KeyCode::Char(' ')));
        assert!(request.control.try_recv().is_err());
        assert_eq!(toast(&app), "Pause and seek need mpv as the audio player");
        assert!(!app.audio_caps().can_pause);
        let room = app.rooms[0].clone();
        let Some((
            0,
            AudioView::Playing {
                position,
                length,
                paused: false,
            },
        )) = app.audio_in(&room)
        else {
            panic!("not playing");
        };
        assert!(position < Duration::from_secs(5));
        assert_eq!(length, Some(Duration::from_secs(12)));
    }

    #[test]
    fn what_cannot_be_played_here_is_not_offered_and_says_why() {
        let dir = Dir::new("refusals");
        let large = || {
            let mut message = voice("big", false);
            message.file.as_mut().unwrap().size = Some(audio::MAX_FILE + 1);
            message
        };
        let (mut app, mut commands) = app_with(vec![voice("v1", true), large()], &dir);
        let menu_labels = |app: &mut App| -> Vec<String> {
            app.perform(Action::ContextMenu);
            let labels = match &app.modal {
                Some(crate::modal::Modal::Menu(menu)) => {
                    menu.items.iter().map(|item| item.label.clone()).collect()
                }
                _ => Vec::new(),
            };
            app.modal = None;
            labels
        };
        select(&mut app, 0);
        let labels = menu_labels(&mut app);
        assert!(labels.contains(&"Play audio".to_owned()), "{labels:?}");
        assert!(
            labels.contains(&"Play in system player".to_owned()),
            "{labels:?}"
        );

        // Over SSH.
        app.audio.ssh = true;
        select(&mut app, 0);
        assert!(!app.audio_caps().can_play);
        let labels = menu_labels(&mut app);
        assert!(
            !labels.contains(&"Play in system player".to_owned()),
            "{labels:?}"
        );
        assert_eq!(
            labels.iter().filter(|l| l.as_str() == "Play audio").count(),
            1
        );
        app.perform(Action::PlayAudio);
        assert_eq!(
            toast(&app),
            "Audio plays on the machine matrix-rat runs on: s saves the file instead"
        );
        app.audio.ssh = false;

        // No player.
        app.audio.player = Err(NoPlayer::NoneFound);
        assert!(!app.audio_caps().can_play);
        app.perform(Action::PlayAudio);
        assert_eq!(
            toast(&app),
            "No audio player found (mpv, ffplay, pw-play, paplay or aplay)"
        );
        app.set_test_player(Kind::Mpv, dir.0.clone());

        // Too large.
        select(&mut app, 1);
        app.perform(Action::PlayAudio);
        assert!(
            toast(&app).starts_with("big.ogg is 100 MB, too large to play here"),
            "{}",
            toast(&app)
        );
        assert!(asked(&mut commands).is_empty());

        // The demo has no account.
        app.matrix = None;
        select(&mut app, 0);
        assert!(!app.audio_caps().can_play);
        app.perform(Action::PlayAudio);
        assert_eq!(toast(&app), "The demo has no account to play audio with");

        // Nothing of the kind selected, nothing playing.
        app.timeline.selected = None;
        app.perform(Action::PlayAudio);
        assert_eq!(toast(&app), "Select a voice or audio message to play it");
        assert!(asked(&mut commands).is_empty());
    }

    #[test]
    fn a_click_on_the_mark_plays_and_stops() {
        let dir = Dir::new("click");
        let (mut app, mut commands) = app_with(vec![voice("v1", true)], &dir);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let screen = format!("{}", terminal.backend());
        let row = screen
            .lines()
            .position(|line| line.contains("▶ 🎤"))
            .unwrap() as u16;
        let buffer = terminal.backend().buffer();
        let column = (0..buffer.area.width)
            .find(|x| buffer[(*x, row)].symbol() == "▶")
            .unwrap();
        let click = InputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
        app.on_event(click.clone());
        assert_eq!(asked(&mut commands), ["FetchAudio(sound 1)"]);
        assert_eq!(app.timeline.selected, Some(0));
        let mut request = fetched(&mut app, 1, &dir);
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let shown = format!("{}", terminal.backend());
        // The time since the player started; a slow machine may be past the first second.
        assert!(
            shown.contains("■ 0:0") && shown.contains("/ 0:12 · 🎤 v1.ogg"),
            "{shown}"
        );
        app.on_event(click);
        assert_eq!(request.control.try_recv(), Ok(Control::Stop));
    }

    /// Room Alpha with a voice message with a waveform, one without, and text; the first
    /// playing at 0:05 (as mpv says) and selected.
    fn playing_app(dir: &Dir) -> App {
        let (mut app, _commands) = app_with(
            vec![
                text("t1", "listen to this"),
                voice("waves", true),
                voice("plain", false),
                text("t2", "and this one has no waveform"),
            ],
            dir,
        );
        select(&mut app, 1);
        app.perform(Action::PlayAudio);
        // The request is dropped: nothing plays in a test.
        fetched(&mut app, 1, dir);
        update(&mut app, 1, PlayerEvent::Controllable);
        update(
            &mut app,
            1,
            PlayerEvent::State(Reported {
                position: Some(Duration::from_millis(5_400)),
                paused: false,
                length: None,
            }),
        );
        app
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        format!("{}", terminal.backend())
    }

    #[test]
    fn a_playing_voice_message_100x30() {
        let dir = Dir::new("snap-wide");
        let mut app = playing_app(&dir);
        let shown = screen(&mut app, 100, 30);
        assert!(shown.contains("■ 0:05 / 0:12 · 🎤 waves.ogg"), "{shown}");
        assert!(shown.contains("▶ 🎤 plain.ogg · 0:12 · 2.0 kB"), "{shown}");
        // The hints: stop, pause and the arrows.
        assert!(shown.contains("x stop"), "{shown}");
        assert!(shown.contains("Space pause"), "{shown}");
        assert!(shown.contains("←→ seek"), "{shown}");
        insta::assert_snapshot!(shown);
    }

    #[test]
    fn a_playing_voice_message_80x24() {
        let dir = Dir::new("snap-narrow");
        let mut app = playing_app(&dir);
        // The one without a waveform plays: its bar is in its first row.
        select(&mut app, 2);
        app.perform(Action::PlayAudio);
        fetched(&mut app, 2, &dir);
        update(&mut app, 2, PlayerEvent::Controllable);
        update(
            &mut app,
            2,
            PlayerEvent::State(Reported {
                position: Some(Duration::from_secs(3)),
                paused: true,
                length: None,
            }),
        );
        let shown = screen(&mut app, 80, 24);
        assert!(shown.contains("■ 0:03 / 0:12 paused ━━━━━━──"), "{shown}");
        assert!(shown.contains("Space resume"), "{shown}");
        insta::assert_snapshot!(shown);
    }

    #[test]
    fn the_hint_offers_play_and_the_palette_stop() {
        let dir = Dir::new("hints");
        let (mut app, _commands) = app_with(vec![voice("v1", true), text("t", "hi")], &dir);
        select(&mut app, 0);
        let shown = screen(&mut app, 100, 30);
        assert!(shown.contains("x play"), "{shown}");
        // Over SSH, the open key (`v`, then `o`) plays it in the system's player as
        // before.
        app.audio.ssh = true;
        let shown = screen(&mut app, 100, 30);
        assert!(
            !shown.contains("x play") && shown.contains("v play"),
            "{shown}"
        );
        app.audio.ssh = false;

        app.perform(Action::PlayAudio);
        fetched(&mut app, 1, &dir);
        select(&mut app, 1);
        let context = app.palette_context();
        assert!(context.audio.active && !context.audio.can_play);
        let palette = crate::palette::Palette::open(context);
        let titles: Vec<&str> = palette
            .entries
            .iter()
            .filter(|entry| entry.target == crate::palette::Target::Action(Action::PlayAudio))
            .map(|entry| entry.title.as_str())
            .collect();
        assert_eq!(titles, ["Stop audio"]);
    }

    #[test]
    fn a_read_marker_stays_where_it_is_while_it_plays() {
        let dir = Dir::new("marker");
        let mut message = voice("v1", true);
        message.read_by = crate::model::ReadBy {
            readers: vec![crate::model::Reader {
                name: "Bob".to_owned(),
                time: "10:05".to_owned(),
                ..Default::default()
            }],
            total: 1,
        };
        let bars = audio::bars(
            &message
                .file
                .as_ref()
                .unwrap()
                .audio
                .as_ref()
                .unwrap()
                .waveform,
        );
        let (mut app, _commands) = app_with(vec![message], &dir);
        let marked = |shown: &str| {
            shown
                .lines()
                .find(|line| line.contains("✓ Bob"))
                .unwrap_or_else(|| panic!("no marker:\n{shown}"))
                .to_owned()
        };
        let idle = marked(&screen(&mut app, 100, 30));
        assert!(idle.contains(&bars), "{idle}");
        select(&mut app, 0);
        app.perform(Action::PlayAudio);
        fetched(&mut app, 1, &dir);
        update(&mut app, 1, PlayerEvent::Controllable);
        update(
            &mut app,
            1,
            PlayerEvent::State(Reported {
                position: Some(Duration::from_secs(6)),
                ..Reported::default()
            }),
        );
        let shown = screen(&mut app, 100, 30);
        assert!(shown.contains("■ 0:06 / 0:12"), "{shown}");
        // The waveform's row, drawn as it plays, with the marker where it was.
        let playing = marked(&shown);
        assert!(playing.contains(&bars), "{playing}");
        assert_eq!(playing.find("✓ Bob"), idle.find("✓ Bob"));
    }

    #[test]
    fn the_setting_names_a_command_of_the_users() {
        let dir = Dir::new("setting");
        let (mut app, _commands) = app_with(Vec::new(), &dir);
        let player = dir.0.join("my-player");
        std::fs::write(&player, "").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&player, std::fs::Permissions::from_mode(0o755)).unwrap();
        app.set_audio_player(&format!("{} --quiet", player.display()));
        assert_eq!(
            app.audio.setting_text(),
            format!("{} --quiet", player.display())
        );
        assert_eq!(toast(&app), "Voice and audio play with my-player");
        app.set_audio_player("no-such-player-anywhere-really");
        assert_eq!(
            app.audio.setting_text(),
            "no-such-player-anywhere-really (not found)"
        );
        assert_eq!(
            toast(&app),
            "The audio player named in the settings was not found"
        );
        app.set_audio_player("  ");
        assert_eq!(app.audio.setting, "auto");
    }

    #[test]
    fn debug_output_names_no_room_message_file_or_folder() {
        let dir = Dir::new("debug");
        let mut app = playing_app(&dir);
        app.audio.setting = "/home/alice/bin/secret-player".to_owned();
        let shown = format!("{app:?}");
        assert!(shown.contains("AudioState"), "{shown}");
        for private in [
            "!alpha",
            "waves",
            "mxc://",
            "secret-player",
            "matrix-rat-playback",
        ] {
            assert!(!shown.contains(private), "{private} in {shown}");
        }
    }
}
