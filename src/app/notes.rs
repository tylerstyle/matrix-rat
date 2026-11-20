//! Transcripts and translations on `App` (see `crate::notes`): the two actions, what the
//! worker (the sound's file) and the programs (their text) answer, and the settings.
//!
//! Both act on the selected message and only when asked: the key, the button on the
//! message, the message's menu or the palette. The same again on a message whose
//! program runs stops it, on a finished note hides it, and on a hidden one shows it
//! (nothing is run again). `Esc` in the messages stops what runs for the selected
//! message. One program runs per message, and [`notes::MAX_RUNNING`] at once.
//!
//! A note stays with its message while the app runs, whatever room is open; with
//! `keep_notes` on, the finished ones are also written to a file in the profile's data
//! folder and read again at the next start.

use std::fmt;
use std::path::PathBuf;

use tokio::sync::oneshot;

use super::App;
use crate::audio::TempAudio;
use crate::images::human_size;
use crate::matrix::MatrixCmd;
use crate::model::{Message, MsgKind, SendState};
use crate::notes::{
    self, NoTranscriber, NoTranslator, NoteKind, NoteState, Notes, Transcriber, Translator,
};
use crate::palette::{NoteCaps, NoteOffer};
use crate::textcmd;

/// The notes, what makes them, and what is on its way.
pub struct NotesState {
    pub notes: Notes,
    /// The settings as written, and what they come to on this system; looked up
    /// again when one changes.
    pub transcribe_command: String,
    pub transcribe_model: String,
    transcriber: Result<Transcriber, NoTranscriber>,
    pub translate_command: String,
    translator: Result<Translator, NoTranslator>,
    pub translate_language: String,
    /// The `keep_notes` setting.
    pub keep: bool,
    /// Where kept notes go; `None` in the demo and in tests: nothing is written.
    file: Option<PathBuf>,
    /// The private folder for the copies of sounds (the player's); set by the event
    /// loop.
    dir: Option<PathBuf>,
    next_id: u64,
    runs: Vec<Run>,
    /// Programs to start; the event loop takes them (see [`App::take_note_starts`]).
    starts: Vec<textcmd::Request>,
}

/// Before the app knows its system and its settings: nothing transcribes, nothing
/// translates (what the demo and tests start with).
impl Default for NotesState {
    fn default() -> Self {
        Self {
            notes: Notes::default(),
            transcribe_command: "auto".to_owned(),
            transcribe_model: String::new(),
            transcriber: Err(NoTranscriber::NoProgram),
            translate_command: String::new(),
            translator: Err(NoTranslator::NotSet),
            translate_language: notes::DEFAULT_LANGUAGE.to_owned(),
            keep: notes::KEEP_BY_DEFAULT,
            file: None,
            dir: None,
            next_id: 0,
            runs: Vec::new(),
            starts: Vec::new(),
        }
    }
}

/// Hand-written: the settings name the user's files and commands, the runs messages.
impl fmt::Debug for NotesState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NotesState")
            .field("notes", &self.notes)
            .field("transcriber", &self.transcriber)
            .field("translator", &self.translator)
            .field("keep", &self.keep)
            .field("file", &self.file.is_some())
            .field("dir", &self.dir.is_some())
            .field("runs", &self.runs)
            .field("starts", &self.starts)
            .finish_non_exhaustive()
    }
}

/// A program on its way to a note.
struct Run {
    /// Answers about another are dropped.
    id: u64,
    /// The message's event.
    event: String,
    kind: NoteKind,
    phase: Phase,
    /// The program's lines are joined into one paragraph (see `notes::clean`).
    one_paragraph: bool,
}

impl fmt::Debug for Run {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Run")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
enum Phase {
    /// The sound's file is on its way from the worker.
    Fetching,
    /// The program runs; a word on this, or dropping it, stops it.
    Running(oneshot::Sender<()>),
}

impl NotesState {
    /// For the app on this system, from the settings.
    pub fn new(config: &crate::config::Config) -> Self {
        Self {
            transcriber: notes::transcriber_here(
                &config.transcribe_command,
                &config.transcribe_model,
            ),
            transcribe_command: config.transcribe_command.clone(),
            transcribe_model: config.transcribe_model.clone(),
            translator: notes::translator_here(&config.translate_command),
            translate_command: config.translate_command.clone(),
            translate_language: notes::language(&config.translate_language),
            keep: config.keep_notes,
            ..Self::default()
        }
    }

    /// What the settings show for the transcribe command.
    pub fn transcribe_text(&self) -> String {
        let auto = crate::audio::is_auto(&self.transcribe_command);
        match (&self.transcriber, auto) {
            (
                Ok(Transcriber::Whisper {
                    ffmpeg: Some(_), ..
                }),
                _,
            ) => "auto: whisper-cli, with ffmpeg".to_owned(),
            (Ok(Transcriber::Whisper { ffmpeg: None, .. }), _) => "auto: whisper-cli".to_owned(),
            (Err(why), true) => format!("auto: {}", why.message()),
            (Ok(_), _) => crate::text::sanitize_line(self.transcribe_command.trim()),
            (Err(_), false) => format!(
                "{} (not found)",
                crate::text::sanitize_line(self.transcribe_command.trim())
            ),
        }
    }

    /// What the settings show for the translate command.
    pub fn translate_text(&self) -> String {
        let command = crate::text::sanitize_line(self.translate_command.trim());
        match &self.translator {
            Ok(_) => command,
            Err(NoTranslator::NotSet) => "(not set)".to_owned(),
            Err(_) => format!("{command} (not found)"),
        }
    }

    fn run_for(&self, event: &str) -> Option<&Run> {
        self.runs.iter().find(|run| run.event == event)
    }

    fn save(&self) -> std::io::Result<()> {
        match (&self.file, self.keep) {
            (Some(file), true) => self.notes.save(file),
            _ => Ok(()),
        }
    }
}

/// The text of `message` that can be translated: what it says now (an edit's new
/// text), or the caption of a picture or file. `None` for a deleted message, one that
/// could not be decrypted, and anything without text of its own.
fn own_text(message: &Message) -> Option<String> {
    if !matches!(message.kind, MsgKind::Text | MsgKind::Emote) || message.poll.is_some() {
        return None;
    }
    let text = if let Some(file) = &message.file {
        // As `matrix::convert` writes it: the file's line, its waveform's where it
        // has one, then the caption.
        let bars = file
            .audio
            .as_ref()
            .map(|meta| crate::audio::bars(&meta.waveform))
            .filter(|bars| !bars.is_empty());
        let mut lines = message.body.lines().skip(1).peekable();
        if bars.is_some() && lines.peek().copied() == bars.as_deref() {
            lines.next();
        }
        lines.collect::<Vec<_>>().join("\n")
    } else if let Some(image) = &message.image {
        image.caption.clone().unwrap_or_default()
    } else {
        message.body.clone()
    };
    (!crate::text::is_blank(&text)).then_some(text)
}

impl App {
    /// Where the copies of sounds go, and where notes are kept across restarts; the
    /// kept ones are read now if the setting says so, and a file left from when it did
    /// is removed if it does not.
    pub(super) fn set_note_paths(&mut self, dir: PathBuf, file: PathBuf) {
        self.notes.dir = Some(dir);
        if self.notes.keep {
            self.notes.notes.load(&file);
        } else {
            let _ = notes::remove_file(&file);
        }
        self.notes.file = Some(file);
    }

    /// Programs for the event loop to start.
    pub(super) fn take_note_starts(&mut self) -> Vec<textcmd::Request> {
        std::mem::take(&mut self.notes.starts)
    }

    /// For the doctor: what the settings come to, without the commands themselves.
    pub(super) fn notes_found(&self) -> (notes::TranscribeFound, notes::TranslateFound) {
        (
            notes::TranscribeFound::of(&self.notes.transcriber),
            notes::TranslateFound::of(&self.notes.translator),
        )
    }

    /// The notes, for drawing.
    pub fn notes(&self) -> &Notes {
        &self.notes.notes
    }

    /// The text a translation of `message` would be made from: its own, or, for a
    /// voice or audio message without a caption, its transcript.
    fn translatable(&self, message: &Message) -> Option<String> {
        own_text(message).or_else(|| {
            let event = message.event_id().filter(|_| message.is_sound())?;
            match &self.notes.notes.get(event, NoteKind::Transcript)?.state {
                NoteState::Done(text) => Some(text.clone()),
                _ => None,
            }
        })
    }

    /// What a note of `kind` under `message` is made from (see `notes::source`): the
    /// sound, or the text and the language. `None`: there is nothing to make one from.
    fn note_source(&self, message: &Message, kind: NoteKind) -> Option<u64> {
        match kind {
            NoteKind::Transcript => message
                .file
                .as_ref()
                .filter(|_| message.is_sound())
                .map(|file| notes::source(&[&file.key])),
            NoteKind::Translation => {
                let text = self.translatable(message)?;
                let language = notes::language(&self.notes.translate_language);
                Some(notes::source(&[&text, &language]))
            }
        }
    }

    /// Whether `note` is what its message says now: not the translation of a text
    /// that was edited since or into another language than the one set, and nothing
    /// under a deleted message. A stale note is as good as none: it is not drawn, and
    /// the action makes it anew.
    pub fn note_current(&self, message: &Message, kind: NoteKind, note: &notes::Note) -> bool {
        if message.kind == MsgKind::Redacted {
            return false;
        }
        match note.state {
            NoteState::Running => true,
            _ => self
                .note_source(message, kind)
                .is_some_and(|source| note.is_of(source)),
        }
    }

    /// What the action of `kind` would do with `message` now.
    pub fn note_offer(&self, message: &Message, kind: NoteKind) -> NoteOffer {
        let Some(event) = message.event_id() else {
            return NoteOffer::NotThis;
        };
        if message.state != SendState::Sent {
            return NoteOffer::NotThis;
        }
        let note = self
            .notes
            .notes
            .get(event, kind)
            .filter(|note| self.note_current(message, kind, note));
        match note.map(|note| (&note.state, note.hidden)) {
            Some((NoteState::Running, _)) => NoteOffer::Stop,
            Some((NoteState::Done(_), true)) => NoteOffer::Show,
            Some((NoteState::Done(_), false)) => NoteOffer::Hide,
            Some((NoteState::Failed(_), _)) | None => self.note_run(message, kind),
        }
    }

    /// Whether a note of `kind` could be made for `message` now: `Run`, or why not.
    fn note_run(&self, message: &Message, kind: NoteKind) -> NoteOffer {
        if message.kind == MsgKind::Redacted {
            return NoteOffer::NotThis;
        }
        match kind {
            NoteKind::Transcript if !message.is_sound() => NoteOffer::NotThis,
            NoteKind::Transcript => match &self.notes.transcriber {
                Ok(_) if self.matrix.is_some() => NoteOffer::Run,
                Ok(_) => NoteOffer::NotThis,
                Err(why) => NoteOffer::Unavailable(why.message()),
            },
            NoteKind::Translation if self.translatable(message).is_none() => NoteOffer::NotThis,
            NoteKind::Translation => match &self.notes.translator {
                Ok(_) => NoteOffer::Run,
                Err(why) => NoteOffer::Unavailable(why.message()),
            },
        }
    }

    /// What the two actions would do with the selected message, for the palette, the
    /// menu and the hints. With no message for it selected: why it is not set up, if
    /// it is not.
    pub fn note_caps(&self) -> NoteCaps {
        let selected = self
            .timeline
            .selected
            .and_then(|index| self.messages().get(index));
        let offer = |kind: NoteKind, why: Option<&'static str>| match selected
            .map(|message| self.note_offer(message, kind))
        {
            Some(NoteOffer::NotThis) | None => {
                why.map_or(NoteOffer::NotThis, NoteOffer::Unavailable)
            }
            Some(offer) => offer,
        };
        // A finished note can be made anew where one could be made at all.
        let again = |kind: NoteKind| {
            selected.is_some_and(|message| {
                matches!(
                    self.note_offer(message, kind),
                    NoteOffer::Hide | NoteOffer::Show
                ) && self.note_run(message, kind) == NoteOffer::Run
            })
        };
        NoteCaps {
            transcribe_again: again(NoteKind::Transcript),
            translate_again: again(NoteKind::Translation),
            transcribe: offer(
                NoteKind::Transcript,
                self.notes
                    .transcriber
                    .as_ref()
                    .err()
                    .map(|why| why.message()),
            ),
            translate: offer(
                NoteKind::Translation,
                self.notes
                    .translator
                    .as_ref()
                    .err()
                    .map(|why| why.message()),
            ),
        }
    }

    /// The key, the menu and the palette: transcribes or translates the selected
    /// message, or stops, hides or shows what is there (see the top of this file).
    pub(super) fn toggle_note(&mut self, kind: NoteKind) {
        let Some(message) = self
            .timeline
            .selected
            .and_then(|index| self.messages().get(index))
            .cloned()
        else {
            self.show_toast(match kind {
                NoteKind::Transcript => "Select a voice or audio message to transcribe it",
                NoteKind::Translation => "Select a message to translate it",
            });
            return;
        };
        let event = message.event_id().map(str::to_owned);
        match (self.note_offer(&message, kind), event) {
            (NoteOffer::Stop, Some(event)) => self.stop_note(&event, kind),
            (NoteOffer::Hide | NoteOffer::Show, Some(event)) => {
                self.notes.notes.toggle_hidden(&event, kind);
                self.note_kept();
            }
            (NoteOffer::Run, Some(event)) => self.start_note(event, &message, kind),
            (NoteOffer::Unavailable(why), _) => {
                let what = match kind {
                    NoteKind::Transcript => "Transcribing",
                    NoteKind::Translation => "Translating",
                };
                self.show_toast(format!("{what} is not set up: {why}"));
            }
            _ => self.show_toast(match kind {
                NoteKind::Transcript if message.is_sound() => {
                    "The demo has no account to fetch the audio with"
                }
                NoteKind::Transcript => "Only voice and audio messages can be transcribed",
                NoteKind::Translation => "This message has no text to translate",
            }),
        }
        self.dirty = true;
    }

    /// "Transcribe again" and "Translate again": the finished note under the selected
    /// message is made anew and replaced.
    pub(super) fn redo_note(&mut self, kind: NoteKind) {
        let message = self
            .timeline
            .selected
            .and_then(|index| self.messages().get(index))
            .cloned();
        let event = message
            .as_ref()
            .and_then(Message::event_id)
            .map(str::to_owned);
        let (Some(message), Some(event)) = (message, event) else {
            return;
        };
        let finished = matches!(
            self.note_offer(&message, kind),
            NoteOffer::Hide | NoteOffer::Show
        );
        if finished && self.note_run(&message, kind) == NoteOffer::Run {
            self.start_note(event, &message, kind);
        } else {
            self.show_toast(match kind {
                NoteKind::Transcript => "There is no transcript here that could be made again",
                NoteKind::Translation => "There is no translation here that could be made again",
            });
        }
        self.dirty = true;
    }

    /// A click on a message's button: as the key on that message.
    pub(super) fn click_note(&mut self, index: usize, kind: NoteKind) {
        self.cancel_pending_jump();
        self.set_focus(crate::action::Pane::Timeline);
        if !self.selectable(index) {
            return;
        }
        self.timeline.selected = Some(index);
        self.perform(match kind {
            NoteKind::Transcript => crate::action::Action::Transcribe,
            NoteKind::Translation => crate::action::Action::Translate,
        });
    }

    fn start_note(&mut self, event: String, message: &Message, kind: NoteKind) {
        if self.notes.run_for(&event).is_some() {
            self.show_toast("A program already runs for this message: wait, or stop it");
            return;
        }
        if self.notes.runs.len() >= notes::MAX_RUNNING {
            self.show_toast(format!(
                "{} programs run already: wait for one, or stop it",
                notes::MAX_RUNNING
            ));
            return;
        }
        self.notes.next_id += 1;
        let id = self.notes.next_id;
        let source = self.note_source(message, kind).unwrap_or(0);
        let (phase, one_paragraph) = match kind {
            NoteKind::Transcript => {
                let (Some(file), Some(dir)) = (message.file.clone(), self.notes.dir.clone()) else {
                    self.show_toast("Audio cannot be transcribed here");
                    return;
                };
                // The player's limit: a larger file is not fetched into memory.
                if let Some(size) = file.size.filter(|size| *size > crate::audio::MAX_FILE) {
                    self.show_toast(format!(
                        "{} is {}, too large to transcribe here",
                        file.name,
                        human_size(size)
                    ));
                    return;
                }
                let one = self
                    .notes
                    .transcriber
                    .as_ref()
                    .is_ok_and(Transcriber::one_paragraph);
                self.send_matrix(MatrixCmd::FetchSpeech { id, file, dir });
                (Phase::Fetching, one)
            }
            NoteKind::Translation => {
                let (Some(text), Ok(translator)) =
                    (self.translatable(message), self.notes.translator.clone())
                else {
                    return;
                };
                let language = notes::language(&self.notes.translate_language);
                let (stop, cancel) = oneshot::channel();
                self.notes.starts.push(textcmd::Request {
                    id,
                    steps: vec![translator.step(&text, &language)],
                    files: Vec::new(),
                    cancel,
                });
                (Phase::Running(stop), false)
            }
        };
        self.notes.runs.push(Run {
            id,
            event: event.clone(),
            kind,
            phase,
            one_paragraph,
        });
        self.notes
            .notes
            .set_from(&event, kind, NoteState::Running, source);
    }

    /// Stops what runs for this note; the note goes.
    fn stop_note(&mut self, event: &str, kind: NoteKind) {
        let Some(at) = self
            .notes
            .runs
            .iter()
            .position(|run| run.event == event && run.kind == kind)
        else {
            return;
        };
        let run = self.notes.runs.remove(at);
        // A program not started yet goes with its request (and its files).
        self.notes.starts.retain(|request| request.id != run.id);
        match run.phase {
            Phase::Fetching => self.send_matrix(MatrixCmd::CancelSpeech { id: run.id }),
            // The program's task kills it (also if it has gone already).
            Phase::Running(stop) => {
                let _ = stop.send(());
            }
        }
        self.notes.notes.remove(event, kind);
        self.dirty = true;
    }

    /// `Esc` in the messages: stops what runs for the selected message. False if
    /// nothing does.
    pub(super) fn stop_selected_note(&mut self) -> bool {
        let running = self
            .timeline
            .selected
            .and_then(|index| self.messages().get(index))
            .and_then(Message::event_id)
            .and_then(|event| self.notes.run_for(event))
            .map(|run| (run.event.clone(), run.kind));
        match running {
            Some((event, kind)) => {
                self.stop_note(&event, kind);
                true
            }
            None => false,
        }
    }

    /// The account goes (logout, or its session is reset): nothing of its messages
    /// stays. What runs is stopped (a sound still on its way from the worker would
    /// never come), the notes are forgotten, and the file of kept ones is removed.
    pub(super) fn forget_notes(&mut self) {
        self.stop_notes();
        self.notes.notes.clear();
        if let Some(file) = &self.notes.file
            && let Err(err) = notes::remove_file(file)
        {
            tracing::warn!(why = %crate::logsafe::kind(&err), "could not remove the notes");
        }
    }

    /// A deleted message loses its notes, here and in the file of kept ones: what it
    /// said is to be gone. Called when the open room's messages change.
    pub(super) fn forget_notes_of_deleted(&mut self) {
        if self.notes.notes.is_empty() {
            return;
        }
        let gone: Vec<String> = self
            .messages()
            .iter()
            .filter(|message| message.kind == MsgKind::Redacted)
            .filter_map(Message::event_id)
            .filter(|event| self.notes.notes.of(event).next().is_some())
            .map(str::to_owned)
            .collect();
        if gone.is_empty() {
            return;
        }
        for event in &gone {
            for kind in NoteKind::ALL {
                self.stop_note(event, kind);
                self.notes.notes.remove(event, kind);
            }
        }
        self.note_kept();
    }

    /// Stops every program: each is told to end (or its download given up) and its
    /// "…ing" note goes. Finished notes stay. The app ends, or the account goes.
    pub(super) fn stop_notes(&mut self) {
        let running: Vec<(String, NoteKind)> = self
            .notes
            .runs
            .iter()
            .map(|run| (run.event.clone(), run.kind))
            .collect();
        for (event, kind) in running {
            self.stop_note(&event, kind);
        }
    }

    pub(super) fn on_speech_fetched(&mut self, id: u64, result: Result<TempAudio, String>) {
        let Some(at) = self
            .notes
            .runs
            .iter()
            .position(|run| run.id == id && matches!(run.phase, Phase::Fetching))
        else {
            // Given up meanwhile: the file goes with the answer.
            return;
        };
        self.dirty = true;
        let started = result.and_then(|sound| {
            let transcriber = self
                .notes
                .transcriber
                .clone()
                .map_err(|why| why.message().to_owned())?;
            // The converted sound lies next to the fetched one, under the same kind
            // of name, and goes with it.
            let wav = match (&self.notes.dir, transcriber.converts()) {
                (Some(dir), true) => Some(
                    TempAudio::create(dir, Some("audio/wav"), &[])
                        .map_err(|_| "it could not be put into a file".to_owned())?,
                ),
                _ => None,
            };
            let steps = transcriber.steps(sound.path(), wav.as_ref().map(TempAudio::path));
            let (stop, cancel) = oneshot::channel();
            let request = textcmd::Request {
                id,
                steps,
                files: std::iter::once(sound).chain(wav).collect(),
                cancel,
            };
            Ok((request, stop))
        });
        match started {
            Ok((request, stop)) => {
                self.notes.starts.push(request);
                self.notes.runs[at].phase = Phase::Running(stop);
            }
            Err(why) => {
                let run = self.notes.runs.remove(at);
                self.notes
                    .notes
                    .set(&run.event, run.kind, NoteState::Failed(why));
            }
        }
    }

    /// A program's last word: its text becomes the note.
    pub(super) fn on_note_done(&mut self, done: textcmd::Done) {
        let Some(at) = self.notes.runs.iter().position(|run| run.id == done.id) else {
            // Stopped meanwhile.
            return;
        };
        let run = self.notes.runs.remove(at);
        self.dirty = true;
        let state = match done.result {
            Ok(output) => {
                let text = notes::clean(&output.text, run.one_paragraph);
                if text.is_empty() {
                    NoteState::Failed("the program wrote no text".to_owned())
                } else {
                    NoteState::Done(text)
                }
            }
            Err(failure) => match notes::failure_text(failure) {
                Some(why) => NoteState::Failed(why),
                None => {
                    self.notes.notes.remove(&run.event, run.kind);
                    return;
                }
            },
        };
        self.notes.notes.set(&run.event, run.kind, state);
        self.note_kept();
    }

    /// The file of kept notes follows a change, where they are kept.
    fn note_kept(&mut self) {
        if let Err(err) = self.notes.save() {
            tracing::warn!(why = %crate::logsafe::kind(&err), "could not keep the notes");
            self.show_toast("The notes could not be written to their file");
        }
    }

    /// A typed setting as it is edited: the audio player's and the four of this file.
    pub(super) fn setting_text(&self, setting: crate::modal::Setting) -> String {
        use crate::modal::Setting;
        match setting {
            Setting::AudioPlayer => self.audio.setting.clone(),
            Setting::TranscribeCommand => self.notes.transcribe_command.clone(),
            Setting::TranscribeModel => self.notes.transcribe_model.clone(),
            Setting::TranslateCommand => self.notes.translate_command.clone(),
            Setting::TranslateLanguage => notes::language(&self.notes.translate_language),
            _ => String::new(),
        }
    }

    /// Enter on one of the text rows of the settings: kept, and looked up at once.
    pub(super) fn set_transcribe_command(&mut self, text: &str) {
        let setting = if crate::audio::is_auto(text) {
            "auto".to_owned()
        } else {
            text.trim().to_owned()
        };
        self.notes.transcribe_command = setting.clone();
        self.save_setting("transcribe_command", setting);
        self.look_up_transcriber();
    }

    pub(super) fn set_transcribe_model(&mut self, text: &str) {
        let setting = text.trim().to_owned();
        self.notes.transcribe_model = setting.clone();
        self.save_setting("transcribe_model", setting);
        self.look_up_transcriber();
    }

    fn look_up_transcriber(&mut self) {
        self.notes.transcriber =
            notes::transcriber_here(&self.notes.transcribe_command, &self.notes.transcribe_model);
        let said = match &self.notes.transcriber {
            Ok(Transcriber::Whisper {
                ffmpeg: Some(_), ..
            }) => "Voice and audio are transcribed with whisper-cli (and ffmpeg)".to_owned(),
            Ok(Transcriber::Whisper { ffmpeg: None, .. }) => {
                "Voice and audio are transcribed with whisper-cli".to_owned()
            }
            Ok(Transcriber::Own { .. }) => {
                "Voice and audio are transcribed with your command".to_owned()
            }
            Err(why) => format!("Transcribing is not offered: {}", why.message()),
        };
        self.show_toast(said);
    }

    pub(super) fn set_translate_command(&mut self, text: &str) {
        let setting = text.trim().to_owned();
        self.notes.translator = notes::translator_here(&setting);
        self.notes.translate_command = setting.clone();
        self.save_setting("translate_command", setting);
        let said = match &self.notes.translator {
            Ok(_) => format!(
                "Messages are translated into {} with your command",
                notes::language(&self.notes.translate_language)
            ),
            Err(why) => format!("Translating is not offered: {}", why.message()),
        };
        self.show_toast(said);
    }

    pub(super) fn set_translate_language(&mut self, text: &str) {
        let setting = notes::language(text);
        if text.trim().starts_with('-') {
            self.show_toast("A language cannot start with \"-\": it would read as an option");
        }
        // Translations into the language before are not shown as the current ones.
        self.notes.notes.rev += 1;
        self.notes.translate_language = setting.clone();
        self.save_setting("translate_language", setting);
    }

    /// The `keep_notes` switch: on writes what there is, off removes the file.
    pub(super) fn toggle_keep_notes(&mut self) {
        self.notes.keep = !self.notes.keep;
        self.save_setting("keep_notes", self.notes.keep);
        let done = match (&self.notes.file, self.notes.keep) {
            (Some(_), true) => self.notes.save(),
            (Some(file), false) => notes::remove_file(file),
            (None, _) => Ok(()),
        };
        if let Err(err) = done {
            tracing::warn!(why = %crate::logsafe::kind(&err), "could not write or remove the notes");
            self.show_toast("The file of notes could not be written or removed");
        }
    }

    /// For tests: a transcriber and a translator that are never started (the test
    /// takes their requests), and the folder for sounds.
    #[cfg(test)]
    pub(super) fn set_test_notes(&mut self, dir: PathBuf, ffmpeg: bool) {
        let bin = std::path::Path::new("/nonexistent");
        self.notes.transcriber = Ok(Transcriber::Whisper {
            program: bin.join("whisper-cli"),
            model: bin.join("model.bin"),
            ffmpeg: ffmpeg.then(|| bin.join("ffmpeg")),
        });
        self.notes.translator =
            notes::translator("sh -c 'cat' {lang}", Some("/bin".as_ref()), None);
        self.notes.dir = Some(dir);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use matrix_sdk::ruma::events::room::MediaSource;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::action::{Action, Pane};
    use crate::matrix::{MatrixEvent, MatrixHandle, SyncStatus};
    use crate::model::{Attachment, AudioMeta, ItemRef, MediaKind, Room, RoomKind};
    use crate::textcmd::tests::Dir;
    use crate::textcmd::{Done, Failure, Output};
    use crate::ui::hit::HitId;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn voice(id: &str) -> Message {
        let address = format!("mxc://example.org/{id}");
        Message {
            id: id.to_owned(),
            item: Some(ItemRef::Event(format!("${id}"))),
            sender: "alice".to_owned(),
            time: "10:00".to_owned(),
            day: "Today".to_owned(),
            body: format!("🎤 {id}.ogg · 0:12 · 2.0 kB"),
            file: Some(Attachment {
                key: address.clone(),
                name: format!("{id}.ogg"),
                size: Some(2000),
                kind: MediaKind::Voice,
                source: Box::new(MediaSource::Plain(address.as_str().into())),
                audio: Some(Arc::new(AudioMeta {
                    duration: Some(std::time::Duration::from_secs(12)),
                    waveform: Vec::new(),
                    mime: Some("audio/ogg".to_owned()),
                })),
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

    /// An app with an account (a fake worker) and a room showing `messages`; programs
    /// are "found" but never started: the tests take their requests.
    fn app_with(
        messages: Vec<Message>,
        dir: &Dir,
        ffmpeg: bool,
    ) -> (App, UnboundedReceiver<MatrixCmd>) {
        let (mut app, commands) = bare_app(messages);
        app.set_test_notes(dir.0.as_path().to_owned(), ffmpeg);
        (app, commands)
    }

    /// The same with nothing set up.
    fn bare_app(messages: Vec<Message>) -> (App, UnboundedReceiver<MatrixCmd>) {
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
        (app, commands)
    }

    /// What was asked of the worker about sounds to transcribe.
    fn asked(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            if matches!(
                command,
                MatrixCmd::FetchSpeech { .. } | MatrixCmd::CancelSpeech { .. }
            ) {
                seen.push(command.describe());
            }
        }
        seen
    }

    fn toast(app: &App) -> &str {
        app.toast.as_ref().map_or("", |(text, _)| text.as_str())
    }

    fn select(app: &mut App, index: usize) {
        app.focus = Pane::Timeline;
        app.timeline.selected = Some(index);
    }

    fn note(app: &App, event: &str, kind: NoteKind) -> Option<NoteState> {
        app.notes().get(event, kind).map(|note| note.state.clone())
    }

    /// The worker's answer with the sound's file; the program's request, as the event
    /// loop would take it.
    fn fetched(app: &mut App, id: u64, dir: &Dir) -> textcmd::Request {
        let file = TempAudio::create(dir.0.as_path(), Some("audio/ogg"), b"OggS").unwrap();
        app.on_matrix(MatrixEvent::SpeechFetched {
            id,
            result: Ok(file),
        });
        let mut starts = app.take_note_starts();
        assert_eq!(starts.len(), 1);
        starts.remove(0)
    }

    fn wrote(app: &mut App, id: u64, text: &str) {
        app.on_note_done(Done {
            id,
            result: Ok(Output {
                text: text.to_owned(),
                truncated: false,
            }),
        });
    }

    fn args(step: &textcmd::Step) -> Vec<String> {
        step.command
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        format!("{}", terminal.backend())
    }

    #[test]
    fn a_voice_message_is_fetched_converted_and_transcribed_under_it() {
        let dir = Dir::new("notes-transcribe");
        let (mut app, mut commands) = app_with(vec![text("t", "hi"), voice("v1")], &dir, true);
        select(&mut app, 1);
        assert_eq!(app.note_caps().transcribe, NoteOffer::Run);
        app.on_event(key(KeyCode::Char('W')));
        assert_eq!(asked(&mut commands), ["FetchSpeech(run 1)"]);
        assert_eq!(
            note(&app, "$v1", NoteKind::Transcript),
            Some(NoteState::Running)
        );
        assert_eq!(app.note_caps().transcribe, NoteOffer::Stop);
        assert!(screen(&mut app, 100, 30).contains("transcribing…"));
        // Nothing is started before the file is there.
        assert!(app.take_note_starts().is_empty());

        let request = fetched(&mut app, 1, &dir);
        assert_eq!(request.id, 1);
        // ffmpeg writes a WAV next to the fetched file, whisper-cli reads that; both
        // files go with the run.
        assert_eq!(request.steps.len(), 2);
        assert_eq!(request.files.len(), 2);
        let sound = request.files[0].path().to_string_lossy().into_owned();
        let wav = request.files[1].path().to_string_lossy().into_owned();
        assert!(request.files[1].path().starts_with(dir.0.as_path()));
        let convert = args(&request.steps[0]);
        assert!(request.steps[0].command.program.ends_with("ffmpeg"));
        assert!(convert.contains(&sound) && convert.last() == Some(&wav));
        assert!(convert.windows(2).any(|pair| pair == ["-ar", "16000"]));
        let whisper = args(&request.steps[1]);
        assert!(request.steps[1].command.program.ends_with("whisper-cli"));
        assert_eq!(whisper.last(), Some(&wav));
        assert!(
            whisper
                .windows(2)
                .any(|pair| pair == ["-m", "/nonexistent/model.bin"])
        );
        assert!(
            request
                .steps
                .iter()
                .all(|step| step.command.stdin.is_none())
        );

        // whisper-cli writes a line per stretch of speech: one paragraph of it.
        wrote(&mut app, 1, "\n Hello there.\n General Kenobi.\n");
        assert_eq!(
            note(&app, "$v1", NoteKind::Transcript),
            Some(NoteState::Done("Hello there. General Kenobi.".into()))
        );
        let shown = screen(&mut app, 100, 30);
        assert!(
            shown.contains("┊ transcript: Hello there. General Kenobi."),
            "{shown}"
        );
        assert!(shown.contains("W hide transcript"), "{shown}");
        // Nothing was sent to the room, and nothing more asked of the worker.
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn without_ffmpeg_the_file_goes_to_whisper_as_it_is() {
        let dir = Dir::new("notes-direct");
        let (mut app, _commands) = app_with(vec![voice("v1")], &dir, false);
        select(&mut app, 0);
        app.perform(Action::Transcribe);
        let request = fetched(&mut app, 1, &dir);
        assert_eq!(request.steps.len(), 1);
        assert_eq!(request.files.len(), 1);
        let sound = request.files[0].path().to_string_lossy().into_owned();
        assert_eq!(args(&request.steps[0]).last(), Some(&sound));
    }

    #[test]
    fn the_same_key_again_hides_and_shows_without_running_anything() {
        let dir = Dir::new("notes-hide");
        let (mut app, mut commands) = app_with(vec![voice("v1")], &dir, true);
        select(&mut app, 0);
        app.perform(Action::Transcribe);
        fetched(&mut app, 1, &dir);
        wrote(&mut app, 1, "spoken words");
        assert_eq!(asked(&mut commands), ["FetchSpeech(run 1)"]);

        app.on_event(key(KeyCode::Char('W')));
        assert_eq!(app.note_caps().transcribe, NoteOffer::Show);
        assert!(!screen(&mut app, 100, 30).contains("spoken words"));
        app.on_event(key(KeyCode::Char('W')));
        assert_eq!(app.note_caps().transcribe, NoteOffer::Hide);
        assert!(screen(&mut app, 100, 30).contains("spoken words"));
        assert!(asked(&mut commands).is_empty());
        assert!(app.take_note_starts().is_empty());

        // It stays with its message when another room is opened and this one again.
        app.active = 1;
        app.timeline.selected = None;
        assert!(!screen(&mut app, 100, 30).contains("spoken words"));
        app.active = 0;
        app.row_cache = None;
        assert!(screen(&mut app, 100, 30).contains("spoken words"));
    }

    #[test]
    fn a_message_is_translated_by_the_users_command_into_the_set_language() {
        let dir = Dir::new("notes-translate");
        let (mut app, mut commands) =
            app_with(vec![text("t", "Guten Morgen"), voice("v1")], &dir, true);
        app.notes.translate_language = " fr ".to_owned();
        select(&mut app, 0);
        assert_eq!(app.note_caps().translate, NoteOffer::Run);
        // A text message is not transcribed.
        assert_eq!(app.note_caps().transcribe, NoteOffer::NotThis);
        app.on_event(key(KeyCode::Char('L')));
        let mut starts = app.take_note_starts();
        assert_eq!(starts.len(), 1);
        let request = starts.remove(0);
        assert_eq!(request.steps.len(), 1);
        assert!(request.files.is_empty());
        let command = &request.steps[0].command;
        // The text on its standard input; the language as a variable and where the
        // command line says `{lang}`.
        assert_eq!(command.stdin.as_deref(), Some("Guten Morgen".as_bytes()));
        assert_eq!(command.env, [("MATRIX_RAT_TRANSLATE_TO", "fr".to_owned())]);
        assert_eq!(args(&request.steps[0]), ["-c", "cat", "fr"]);
        assert_eq!(
            note(&app, "$t", NoteKind::Translation),
            Some(NoteState::Running)
        );
        assert!(screen(&mut app, 100, 30).contains("translating…"));

        // A translation keeps its lines; what the program writes is made safe.
        wrote(&mut app, request.id, "Bonjour\u{1b}[2J\n\nà tous\n");
        assert_eq!(
            note(&app, "$t", NoteKind::Translation),
            Some(NoteState::Done("Bonjour[2J\nà tous".into()))
        );
        let shown = screen(&mut app, 100, 30);
        assert!(shown.contains("┊ translation: Bonjour[2J"), "{shown}");
        assert!(shown.contains("L hide translation"), "{shown}");
        // Nothing went to the worker: not a word of it is sent anywhere by the app.
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn what_is_translated_is_the_text_the_caption_or_the_transcript() {
        let dir = Dir::new("notes-what");
        let mut captioned = voice("v2");
        captioned.body.push_str("\nhör mal");
        let redacted = Message {
            kind: MsgKind::Redacted,
            ..text("r", "Message deleted")
        };
        let undecryptable = Message {
            kind: MsgKind::Undecryptable,
            ..text("u", "Unable to decrypt")
        };
        let unsent = Message {
            state: SendState::Sending,
            ..text("s", "not yet")
        };
        let (mut app, _commands) = app_with(
            vec![
                voice("v1"),
                captioned,
                redacted,
                undecryptable,
                unsent,
                text("e", "neuer Text"),
            ],
            &dir,
            true,
        );
        let offer = |app: &App, index: usize| {
            app.note_offer(&app.rooms[0].messages[index], NoteKind::Translation)
        };
        // A voice message without a caption: only once it has a transcript.
        assert_eq!(offer(&app, 0), NoteOffer::NotThis);
        assert_eq!(offer(&app, 1), NoteOffer::Run);
        assert_eq!(offer(&app, 2), NoteOffer::NotThis);
        assert_eq!(offer(&app, 3), NoteOffer::NotThis);
        assert_eq!(offer(&app, 4), NoteOffer::NotThis);
        assert_eq!(offer(&app, 5), NoteOffer::Run);
        select(&mut app, 2);
        app.perform(Action::Translate);
        assert_eq!(toast(&app), "This message has no text to translate");
        assert!(app.take_note_starts().is_empty());

        // The caption, without the file's line.
        select(&mut app, 1);
        app.perform(Action::Translate);
        let request = app.take_note_starts().remove(0);
        assert_eq!(
            request.steps[0].command.stdin.as_deref(),
            Some("hör mal".as_bytes())
        );

        // The transcript of the first, once it is there.
        select(&mut app, 0);
        app.perform(Action::Transcribe);
        fetched(&mut app, 2, &dir);
        wrote(&mut app, 2, "gesprochene Worte");
        assert_eq!(offer(&app, 0), NoteOffer::Run);
        app.perform(Action::Translate);
        let request = app.take_note_starts().remove(0);
        assert_eq!(
            request.steps[0].command.stdin.as_deref(),
            Some("gesprochene Worte".as_bytes())
        );
    }

    #[test]
    fn esc_and_the_key_again_stop_what_runs() {
        let dir = Dir::new("notes-stop");
        let (mut app, mut commands) = app_with(vec![voice("v1"), text("t", "hallo")], &dir, true);
        // While the file is fetched: the worker is told, a late answer is dropped.
        select(&mut app, 0);
        app.perform(Action::Transcribe);
        app.on_event(key(KeyCode::Esc));
        assert_eq!(
            asked(&mut commands),
            ["FetchSpeech(run 1)", "CancelSpeech(run 1)"]
        );
        assert_eq!(note(&app, "$v1", NoteKind::Transcript), None);
        assert_eq!(app.focus, Pane::Timeline);
        let late = TempAudio::create(dir.0.as_path(), None, b"x").unwrap();
        let late_path = late.path().to_owned();
        app.on_matrix(MatrixEvent::SpeechFetched {
            id: 1,
            result: Ok(late),
        });
        assert!(app.take_note_starts().is_empty());
        assert!(!late_path.exists());

        // While the program runs: it is told to stop, and its last word is dropped.
        app.perform(Action::Transcribe);
        let mut request = fetched(&mut app, 2, &dir);
        app.on_event(key(KeyCode::Char('W')));
        assert_eq!(request.cancel.try_recv(), Ok(()));
        assert_eq!(note(&app, "$v1", NoteKind::Transcript), None);
        wrote(&mut app, 2, "too late");
        assert_eq!(note(&app, "$v1", NoteKind::Transcript), None);

        // A program not started yet is not started at all.
        select(&mut app, 1);
        app.perform(Action::Translate);
        app.on_event(key(KeyCode::Esc));
        assert!(app.take_note_starts().is_empty());
        assert_eq!(note(&app, "$t", NoteKind::Translation), None);
        // With nothing running, Esc does what it always did.
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.focus, Pane::Composer);

        // The app ends: everything is stopped.
        select(&mut app, 1);
        app.perform(Action::Translate);
        let mut request = app.take_note_starts().remove(0);
        app.stop_notes();
        assert_eq!(request.cancel.try_recv(), Ok(()));
        assert_eq!(app.notes().running(), 0);
    }

    #[test]
    fn one_program_per_message_and_two_at_once() {
        let dir = Dir::new("notes-limit");
        let mut captioned = voice("v1");
        captioned.body.push_str("\nhör mal");
        let (mut app, mut commands) = app_with(
            vec![captioned, text("a", "eins"), text("b", "zwei")],
            &dir,
            true,
        );
        select(&mut app, 0);
        app.perform(Action::Transcribe);
        // The same message again, for the other kind: not while one runs for it.
        app.perform(Action::Translate);
        assert!(toast(&app).contains("already runs for this message"));
        assert_eq!(note(&app, "$v1", NoteKind::Translation), None);

        select(&mut app, 1);
        app.perform(Action::Translate);
        assert_eq!(app.take_note_starts().len(), 1);
        select(&mut app, 2);
        app.perform(Action::Translate);
        assert!(toast(&app).contains("2 programs run already"));
        assert!(app.take_note_starts().is_empty());
        assert_eq!(note(&app, "$b", NoteKind::Translation), None);
        assert_eq!(asked(&mut commands), ["FetchSpeech(run 1)"]);

        // One ends: there is room again.
        wrote(&mut app, 2, "one");
        app.perform(Action::Translate);
        assert_eq!(app.take_note_starts().len(), 1);
    }

    #[test]
    fn a_failure_is_said_under_the_message_and_can_be_tried_again() {
        let dir = Dir::new("notes-fail");
        let (mut app, mut commands) = app_with(vec![voice("v1"), text("t", "hallo")], &dir, true);
        let failed = |app: &App, event: &str, kind| match note(app, event, kind) {
            Some(NoteState::Failed(why)) => why,
            other => panic!("{other:?}"),
        };
        // The download fails: the worker's words are our own, and shown.
        select(&mut app, 0);
        app.perform(Action::Transcribe);
        app.on_matrix(MatrixEvent::SpeechFetched {
            id: 1,
            result: Err("it could not be fetched".into()),
        });
        assert_eq!(
            failed(&app, "$v1", NoteKind::Transcript),
            "it could not be fetched"
        );
        assert!(screen(&mut app, 100, 30).contains("┊ transcript: it could not be fetched"));
        // Again: it runs again.
        assert_eq!(app.note_caps().transcribe, NoteOffer::Run);
        app.perform(Action::Transcribe);
        assert_eq!(
            asked(&mut commands),
            ["FetchSpeech(run 1)", "FetchSpeech(run 2)"]
        );
        fetched(&mut app, 2, &dir);
        app.on_note_done(Done {
            id: 2,
            result: Err(Failure::Exit(Some(3))),
        });
        assert!(failed(&app, "$v1", NoteKind::Transcript).contains('3'));

        select(&mut app, 1);
        for (result, said) in [
            (Err(Failure::TimedOut), "long"),
            (Err(Failure::Start), "start"),
            (
                Ok(Output {
                    text: " \n".into(),
                    truncated: false,
                }),
                "no text",
            ),
        ] {
            app.perform(Action::Translate);
            let request = app.take_note_starts().remove(0);
            app.on_note_done(Done {
                id: request.id,
                result,
            });
            let why = failed(&app, "$t", NoteKind::Translation);
            assert!(why.contains(said), "{why}");
        }
        // Stopped by the user: nothing is left to say.
        app.perform(Action::Translate);
        let request = app.take_note_starts().remove(0);
        app.on_note_done(Done {
            id: request.id,
            result: Err(Failure::Cancelled),
        });
        assert_eq!(note(&app, "$t", NoteKind::Translation), None);
    }

    #[test]
    fn a_file_too_large_for_the_player_is_not_fetched() {
        let dir = Dir::new("notes-large");
        let mut large = voice("v1");
        if let Some(file) = &mut large.file {
            file.size = Some(crate::audio::MAX_FILE + 1);
        }
        let (mut app, mut commands) = app_with(vec![large], &dir, true);
        select(&mut app, 0);
        app.perform(Action::Transcribe);
        assert!(toast(&app).contains("too large to transcribe"));
        assert!(asked(&mut commands).is_empty());
        assert!(app.notes().is_empty());
    }

    #[test]
    fn nothing_is_offered_until_it_is_set_up() {
        let (mut app, mut commands) = bare_app(vec![voice("v1"), text("t", "hallo")]);
        select(&mut app, 0);
        assert!(matches!(
            app.note_caps().transcribe,
            NoteOffer::Unavailable(why) if why.contains("whisper-cli")
        ));
        select(&mut app, 1);
        assert!(matches!(
            app.note_caps().translate,
            NoteOffer::Unavailable(why) if why.contains("translate_command")
        ));
        // No hint, no button, no entry in the message's menu.
        let shown = screen(&mut app, 100, 30);
        assert!(
            !shown.contains("transcribe") && !shown.contains("translate"),
            "{shown}"
        );
        app.perform(Action::ContextMenu);
        let shown = screen(&mut app, 100, 30);
        assert!(
            !shown.contains("Translate") && !shown.contains("Transcribe"),
            "{shown}"
        );
        app.modal = None;
        // The key says why, and nothing runs.
        app.on_event(key(KeyCode::Char('L')));
        assert!(toast(&app).starts_with("Translating is not set up: "));
        select(&mut app, 0);
        app.on_event(key(KeyCode::Char('W')));
        assert!(toast(&app).starts_with("Transcribing is not set up: "));
        assert!(app.take_note_starts().is_empty());
        assert!(asked(&mut commands).is_empty());
        assert!(app.notes().is_empty());
        // The palette has both, greyed, with the reason.
        let context = app.palette_context();
        assert!(!context.notes.transcribe.possible());
        assert!(!context.notes.translate.possible());
        app.toast = None;
        app.perform(Action::OpenPalette);
        for letter in "Translate message".chars() {
            app.on_event(key(KeyCode::Char(letter)));
        }
        let shown = screen(&mut app, 120, 30);
        assert!(
            shown.contains("Translate message (set translate_command"),
            "{shown}"
        );
    }

    #[test]
    fn the_buttons_on_a_message_do_what_the_keys_do() {
        let dir = Dir::new("notes-buttons");
        let (mut app, mut commands) = app_with(
            vec![text("t", "Guten Morgen"), voice("v1"), text("u", "zwei")],
            &dir,
            true,
        );
        // Nothing selected: the voice message has its button, the texts have none.
        app.timeline.selected = None;
        app.focus = Pane::Composer;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let shown = format!("{}", terminal.backend());
        assert_eq!(shown.matches(" transcribe ").count(), 1, "{shown}");
        assert!(!shown.contains(" translate "), "{shown}");
        let spot = |app: &App, kind| {
            (0..30u16)
                .flat_map(|row| (0..100u16).map(move |column| (column, row)))
                .find(|(column, row)| {
                    matches!(app.hits.at(*column, *row), Some(HitId::NoteButton(_, k)) if k == kind)
                })
        };
        let click = |app: &mut App, (column, row): (u16, u16)| {
            app.on_event(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }));
        };
        let at = spot(&app, NoteKind::Transcript).expect("a button on the voice message");
        assert_eq!(spot(&app, NoteKind::Translation), None);
        click(&mut app, at);
        assert_eq!(asked(&mut commands), ["FetchSpeech(run 1)"]);
        assert_eq!(app.timeline.selected, Some(1));
        assert_eq!(app.focus, Pane::Timeline);

        // A selected text message has the button that translates.
        select(&mut app, 0);
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let shown = format!("{}", terminal.backend());
        assert!(shown.contains(" translate "), "{shown}");
        // The voice message's now stops.
        assert!(shown.contains(" stop "), "{shown}");
        let at = spot(&app, NoteKind::Translation).expect("a button on the selected text");
        click(&mut app, at);
        assert_eq!(app.take_note_starts().len(), 1);
        assert_eq!(
            note(&app, "$t", NoteKind::Translation),
            Some(NoteState::Running)
        );
    }

    #[test]
    fn the_menu_of_a_message_offers_both() {
        let dir = Dir::new("notes-menu");
        let mut captioned = voice("v1");
        captioned.body.push_str("\nhör mal");
        let (mut app, _commands) = app_with(vec![captioned], &dir, true);
        select(&mut app, 0);
        app.perform(Action::ContextMenu);
        let shown = screen(&mut app, 100, 40);
        assert!(
            shown.contains("Transcribe") && shown.contains("Translate"),
            "{shown}"
        );
        app.modal = None;
        app.perform(Action::Translate);
        let request = app.take_note_starts().remove(0);
        wrote(&mut app, request.id, "listen");
        app.perform(Action::ContextMenu);
        let shown = screen(&mut app, 100, 40);
        assert!(shown.contains("Hide translation"), "{shown}");
    }

    #[test]
    fn notes_are_kept_in_a_private_file_only_when_the_setting_says_so() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new("notes-keep");
        let file = dir.0.as_path().join("notes.json");
        let (mut app, _commands) = app_with(vec![text("t", "Guten Morgen")], &dir, true);
        assert!(!app.notes.keep, "session only unless asked");
        app.set_note_paths(dir.0.as_path().to_owned(), file.clone());
        select(&mut app, 0);
        app.perform(Action::Translate);
        let request = app.take_note_starts().remove(0);
        wrote(&mut app, request.id, "Good morning");
        assert!(!file.exists());

        // Switched on: what there is goes into the file, for the user only.
        app.toggle_keep_notes();
        assert!(app.notes.keep);
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("Good morning")
        );

        // The next start reads it; a run that never ended is not in it.
        let (mut next, _commands) = app_with(vec![text("t", "Guten Morgen")], &dir, true);
        next.notes.keep = true;
        next.set_note_paths(dir.0.as_path().to_owned(), file.clone());
        assert_eq!(
            note(&next, "$t", NoteKind::Translation),
            Some(NoteState::Done("Good morning".into()))
        );
        // Hiding is kept too.
        select(&mut next, 0);
        next.perform(Action::Translate);
        assert_eq!(next.note_caps().translate, NoteOffer::Show);
        let (mut third, _commands) = app_with(vec![text("t", "Guten Morgen")], &dir, true);
        third.notes.keep = true;
        third.set_note_paths(dir.0.as_path().to_owned(), file.clone());
        select(&mut third, 0);
        assert_eq!(third.note_caps().translate, NoteOffer::Show);

        // Switched off: the file goes, the notes of the session stay.
        third.toggle_keep_notes();
        assert!(!file.exists());
        assert!(!third.notes().is_empty());
        // And a file left behind is removed by a start with the setting off.
        std::fs::write(&file, "{}").unwrap();
        let (mut off, _commands) = app_with(Vec::new(), &dir, true);
        off.set_note_paths(dir.0.as_path().to_owned(), file.clone());
        assert!(!file.exists());
        assert!(off.notes().is_empty());
    }

    #[test]
    fn the_settings_show_what_is_found_and_never_run_anything() {
        let (mut app, _commands) = bare_app(Vec::new());
        assert_eq!(app.notes.translate_text(), "(not set)");
        assert!(app.notes.transcribe_text().starts_with("auto: "));
        app.set_translate_command("no-such-translator-x --to {lang}");
        assert!(app.notes.translate_text().ends_with("(not found)"));
        assert!(toast(&app).starts_with("Translating is not offered"));
        app.set_translate_command("sh -c cat");
        assert_eq!(app.notes.translate_text(), "sh -c cat");
        assert!(toast(&app).contains("translated into en"));
        app.set_translate_language("  Brazilian Portuguese ");
        assert_eq!(app.notes.translate_language, "Brazilian Portuguese");
        // Nothing typed: the default again.
        app.set_translate_language(" ");
        assert_eq!(app.notes.translate_language, "en");
        app.set_transcribe_command("no-such-transcriber-x");
        assert!(app.notes.transcribe_text().ends_with("(not found)"));
        app.set_transcribe_command("sh");
        assert_eq!(app.notes.transcribe_text(), "sh");
        assert!(toast(&app).contains("your command"));
        app.set_transcribe_command(" AUTO ");
        assert_eq!(app.notes.transcribe_command, "auto");
        // Debug output says nothing of the commands or the files.
        app.set_translate_command("sh -c 'secret-key' /home/alice");
        app.set_transcribe_model("/home/alice/model.bin");
        let shown = format!("{:?}", app.notes);
        assert!(
            !shown.contains("secret-key") && !shown.contains("alice"),
            "{shown}"
        );
    }
}
