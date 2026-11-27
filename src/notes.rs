//! Text under a message that only the user sees: the transcript of a voice or audio
//! message, and the translation of a message. Both are what a program of the user's
//! wrote when the user asked for it (see `crate::textcmd`); nothing is sent to the room.
//!
//! This file holds the pure parts: the notes and where they are kept, which program
//! with which arguments, and how its output becomes text for the screen. The app's side
//! is `app::notes`, the drawing `ui::timeline::notes`.
//!
//! Transcribing is local: `"auto"` means `whisper-cli` of whisper.cpp with a model
//! file, and where `ffmpeg` is found the sound is first made a 16 kHz mono WAV, which
//! every build of whisper.cpp reads (Matrix voice messages are Ogg/Opus, which only a
//! build with FFmpeg support reads by itself). Translating has no program of its own:
//! the user names one, and whether that is a local model or a service is theirs to
//! decide.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::storage::{Key, Removal, Storage};
use crate::textcmd::{self, BadCommand, Command, Failure, MAX_OUTPUT, Step};

/// The most notes kept; the oldest go first.
pub const MAX_NOTES: usize = 200;
/// The most characters of a note: a long voice message's worth. More is cut.
pub const MAX_TEXT: usize = 8000;
/// The most characters of a message handed to the translator.
pub const MAX_INPUT: usize = 16_000;
/// Programs running at once, over all messages.
pub const MAX_RUNNING: usize = 2;
/// The language translated into where the setting names none.
pub const DEFAULT_LANGUAGE: &str = "en";
/// The setting that keeps notes across restarts, as it is unless the config says
/// otherwise. Notes hold message text in plain form: off keeps them out of any file.
pub const KEEP_BY_DEFAULT: bool = false;

/// How long the programs get. Whisper on a slow machine with a large model takes its
/// time; `Esc` gives it up before.
const TRANSCRIBE_TIME: Duration = Duration::from_secs(15 * 60);
const CONVERT_TIME: Duration = Duration::from_secs(2 * 60);
const TRANSLATE_TIME: Duration = Duration::from_secs(2 * 60);
/// How much of a sound is converted for whisper, in seconds: the WAV lies in the
/// runtime folder (memory, on most systems) and takes 32 kB a second.
const CONVERT_SECONDS: &str = "1800";

/// The target language for a translator, in its environment.
pub const LANGUAGE_VARIABLE: &str = "MATRIX_RAT_TRANSLATE_TO";
/// Where the target language goes in a translator's arguments.
pub const LANGUAGE_PLACEHOLDER: &str = "{lang}";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteKind {
    Transcript,
    Translation,
}

impl NoteKind {
    pub const ALL: [NoteKind; 2] = [NoteKind::Transcript, NoteKind::Translation];

    /// The small label in front of the note.
    pub fn label(self) -> &'static str {
        match self {
            NoteKind::Transcript => "transcript",
            NoteKind::Translation => "translation",
        }
    }

    /// What the note says while its program runs.
    pub fn running(self) -> &'static str {
        match self {
            NoteKind::Transcript => "transcribing…",
            NoteKind::Translation => "translating…",
        }
    }
}

/// How far a note got.
#[derive(Clone, PartialEq, Eq)]
pub enum NoteState {
    /// Its program runs (or the sound for it is on its way).
    Running,
    /// What the program wrote, made safe for the screen and bounded.
    Done(String),
    /// A short reason in our own words.
    Failed(String),
}

/// Hand-written: the text is a message's, or says what one said.
impl fmt::Debug for NoteState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NoteState::Running => "Running",
            NoteState::Done(_) => "Done",
            NoteState::Failed(_) => "Failed",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub state: NoteState,
    /// Put away by the user: not drawn, and shown again without running anything.
    pub hidden: bool,
    /// What it was made from (see [`source`]): the note of something else is not
    /// shown as this message's. 0: not known, and taken as it is.
    pub source: u64,
    /// Notes in the order they were made, for the oldest to go first.
    seq: u64,
}

impl Note {
    /// Whether the note was made from what `source` stands for.
    pub fn is_of(&self, source: u64) -> bool {
        self.source == 0 || self.source == source
    }
}

/// A number for what a note is made from, the same at every start (it is kept with
/// the note): the sound's address for a transcript; the text and the language for a
/// translation. Never 0. It tells an edited message from the one that was translated;
/// it is no secret and guards nothing (FNV-1a).
pub fn source(parts: &[&str]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |byte: u8| {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for part in parts {
        part.bytes().for_each(&mut eat);
        // Between the parts, a byte no text holds.
        eat(0xff);
    }
    hash.max(1)
}

/// The notes of the session, by the event id of their message and their kind. A thread
/// shows its first message under the same event id as the room does, so a note is under
/// it in both.
#[derive(Default)]
pub struct Notes {
    map: BTreeMap<(String, NoteKind), Note>,
    next_seq: u64,
    /// Changes with every change: the timeline's rows are laid out again.
    pub rev: u64,
}

/// Hand-written: the keys are event ids and the notes hold message text.
impl fmt::Debug for Notes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notes")
            .field("notes", &self.map.len())
            .field("rev", &self.rev)
            .finish()
    }
}

impl Notes {
    pub fn get(&self, event: &str, kind: NoteKind) -> Option<&Note> {
        self.map.get(&(event.to_owned(), kind))
    }

    /// The notes of one message, the transcript first.
    pub fn of<'a>(&'a self, event: &'a str) -> impl Iterator<Item = (NoteKind, &'a Note)> + 'a {
        NoteKind::ALL.into_iter().filter_map(move |kind| {
            self.map
                .get(&(event.to_owned(), kind))
                .map(|note| (kind, note))
        })
    }

    /// The event ids of the messages that have notes (one with two is named twice).
    pub fn events(&self) -> impl Iterator<Item = &str> {
        self.map.keys().map(|(event, _)| event.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// How many programs run.
    #[cfg(test)]
    pub fn running(&self) -> usize {
        self.map
            .values()
            .filter(|note| note.state == NoteState::Running)
            .count()
    }

    /// Puts a note under a message in place of the one of its kind there, shown. It
    /// is made from what the one before was made from (a run's end follows its start).
    pub fn set(&mut self, event: &str, kind: NoteKind, state: NoteState) {
        let source = self.get(event, kind).map_or(0, |note| note.source);
        self.set_from(event, kind, state, source);
    }

    /// [`Self::set`], for a note made from `source`.
    pub fn set_from(&mut self, event: &str, kind: NoteKind, state: NoteState, source: u64) {
        self.next_seq += 1;
        let note = Note {
            state,
            hidden: false,
            source,
            seq: self.next_seq,
        };
        self.map.insert((event.to_owned(), kind), note);
        self.rev += 1;
        self.forget_oldest();
    }

    /// Forgets every note (the account goes).
    pub fn clear(&mut self) {
        self.map.clear();
        self.rev += 1;
    }

    pub fn remove(&mut self, event: &str, kind: NoteKind) {
        if self.map.remove(&(event.to_owned(), kind)).is_some() {
            self.rev += 1;
        }
    }

    /// Hides a shown note or shows a hidden one; whether it is hidden now.
    pub fn toggle_hidden(&mut self, event: &str, kind: NoteKind) -> Option<bool> {
        let note = self.map.get_mut(&(event.to_owned(), kind))?;
        note.hidden = !note.hidden;
        self.rev += 1;
        Some(note.hidden)
    }

    /// Beyond [`MAX_NOTES`] the oldest that are not being made go.
    fn forget_oldest(&mut self) {
        while self.map.len() > MAX_NOTES {
            let oldest = self
                .map
                .iter()
                .filter(|(_, note)| note.state != NoteState::Running)
                .min_by_key(|(_, note)| note.seq)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(key) => self.map.remove(&key),
                None => break,
            };
        }
    }

    /// The finished notes, oldest first, as they are kept in a file.
    fn kept(&self) -> Vec<Kept> {
        let mut done: Vec<(&(String, NoteKind), &Note)> = self
            .map
            .iter()
            .filter(|(_, note)| matches!(note.state, NoteState::Done(_)))
            .collect();
        done.sort_by_key(|(_, note)| note.seq);
        done.into_iter()
            .filter_map(|((event, kind), note)| match &note.state {
                NoteState::Done(text) => Some(Kept {
                    event: event.clone(),
                    kind: *kind,
                    text: text.clone(),
                    hidden: note.hidden,
                    source: note.source,
                }),
                NoteState::Running | NoteState::Failed(_) => None,
            })
            .collect()
    }

    /// Writes the finished notes of `owner` to `storage` (on this system the file,
    /// owner-only, in one step). They are message text in plain form: only done where
    /// the user turned `keep_notes` on.
    pub fn save(&self, storage: &dyn Storage, owner: &str) -> anyhow::Result<()> {
        let kept = FileOut {
            version: FILE_VERSION,
            owner,
            notes: self.kept(),
        };
        let json = serde_json::to_vec(&kept).map_err(io::Error::other)?;
        storage.write(&Key::Notes, &json)
    }

    /// Takes in the notes of `owner` from `storage`, as far as they are sound: bounded
    /// in number and size, and made safe for the screen again (the file may have been
    /// written by anything). A file that is not there, not a plain file (a link is not
    /// followed), too large (see `FsStorage`), of another version or not readable
    /// gives none.
    ///
    /// A file without an owner (from before notes named their account) or belonging to
    /// another account is ignored and removed from disk, so its plain message text is
    /// never shown to the wrong person.
    pub fn load(&mut self, storage: &dyn Storage, owner: &str) {
        if owner.is_empty() {
            return;
        }
        let Ok(Some(bytes)) = storage.read(&Key::Notes) else {
            return;
        };
        let kept = match serde_json::from_slice::<FileIn>(&bytes) {
            Ok(kept) if kept.version == FILE_VERSION => kept,
            _ => {
                tracing::debug!("the file of kept notes could not be read");
                return;
            }
        };
        if kept.owner.as_deref() != Some(owner) {
            tracing::debug!("dropping kept notes file without a matching owner");
            let _ = storage.remove(&Key::Notes, Removal::Value);
            return;
        }
        // Only what is sound counts toward the bound, and the newest of that stay.
        let sound: Vec<(Kept, String)> = kept
            .notes
            .into_iter()
            .filter_map(|note| {
                let text = clean(&note.text, false);
                let fine = !note.event.is_empty() && note.event.len() <= 255 && !text.is_empty();
                fine.then_some((note, text))
            })
            .collect();
        let skip = sound.len().saturating_sub(MAX_NOTES);
        for (note, text) in sound.into_iter().skip(skip) {
            self.set_from(&note.event, note.kind, NoteState::Done(text), note.source);
            if note.hidden {
                self.toggle_hidden(&note.event, note.kind);
            }
        }
    }
}

/// The file's format; a file of another is not read.
const FILE_VERSION: u32 = 1;
/// The largest file read (by `FsStorage`): far more than `save` ever writes.
pub(crate) const MAX_FILE: u64 = 16 * 1024 * 1024;

#[derive(Serialize)]
struct FileOut<'a> {
    version: u32,
    owner: &'a str,
    notes: Vec<Kept>,
}

#[derive(Deserialize)]
struct FileIn {
    version: u32,
    #[serde(default)]
    owner: Option<String>,
    notes: Vec<Kept>,
}

#[derive(Serialize, Deserialize)]
struct Kept {
    event: String,
    kind: NoteKind,
    text: String,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    source: u64,
}

/// What a program wrote, as a note: safe for the terminal (no control characters),
/// without empty lines and spaces around lines, and at most [`MAX_TEXT`] characters
/// (cut with `…`). `one_paragraph`: the lines are joined with spaces (whisper writes
/// one line per stretch of speech).
pub fn clean(output: &str, one_paragraph: bool) -> String {
    let safe = crate::text::sanitize(output);
    let lines: Vec<&str> = safe
        .lines()
        .map(str::trim)
        .filter(|line| !crate::text::is_blank(line))
        .collect();
    let text = lines.join(if one_paragraph { " " } else { "\n" });
    if text.chars().count() <= MAX_TEXT {
        return text;
    }
    let mut cut: String = text.chars().take(MAX_TEXT).collect();
    cut.push('…');
    cut
}

/// What a note says of a run that brought no text; `None` where it was given up (the
/// note goes).
pub fn failure_text(failure: Failure) -> Option<String> {
    match failure {
        Failure::Start => Some("the program could not be started".to_owned()),
        Failure::Exit(Some(code)) => Some(format!("the program failed (exit code {code})")),
        Failure::Exit(None) => Some("the program was killed".to_owned()),
        Failure::TimedOut => Some("the program took too long".to_owned()),
        Failure::Cancelled => None,
    }
}

/// What transcribes.
#[derive(Clone, PartialEq, Eq)]
pub enum Transcriber {
    /// `whisper-cli` as `"auto"` found it, with the model the settings name. Where
    /// `ffmpeg` is there too, it makes the sound a WAV first.
    Whisper {
        program: PathBuf,
        model: PathBuf,
        ffmpeg: Option<PathBuf>,
    },
    /// A command of the user's: the sound's file is added as its last argument.
    Own { program: PathBuf, args: Vec<String> },
}

/// Hand-written: the paths and the command are the user's own.
impl fmt::Debug for Transcriber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Transcriber::Whisper { ffmpeg, .. } => {
                write!(f, "Whisper(ffmpeg: {})", ffmpeg.is_some())
            }
            Transcriber::Own { .. } => f.write_str("Own"),
        }
    }
}

/// Why nothing transcribes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoTranscriber {
    /// `"auto"` did not find `whisper-cli`.
    NoProgram,
    /// `whisper-cli` is there, but the settings name no model file.
    NoModel,
    /// The model file the settings name is not there.
    ModelMissing,
    /// The command of the user's could not be read as one.
    BadCommand,
    /// The program it names is not there.
    Missing,
}

impl NoTranscriber {
    /// For a toast, the palette and the settings.
    pub fn message(self) -> &'static str {
        match self {
            NoTranscriber::NoProgram => "whisper-cli was not found",
            NoTranscriber::NoModel => "set transcribe_model to a whisper model file",
            NoTranscriber::ModelMissing => "the file named by transcribe_model was not found",
            NoTranscriber::BadCommand => "transcribe_command could not be read",
            NoTranscriber::Missing => "the program named by transcribe_command was not found",
        }
    }
}

/// The transcriber for the `transcribe_command` and `transcribe_model` settings.
/// `"auto"` looks for `whisper-cli` (and `ffmpeg`) on `path` without running anything,
/// and wants the model file to be there; anything else is a command line of the
/// user's (see [`textcmd::parse`]).
pub fn transcriber(
    command: &str,
    model: &str,
    path: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<Transcriber, NoTranscriber> {
    if !crate::audio::is_auto(command) {
        return match textcmd::parse(command, path, home) {
            Ok((program, args)) => Ok(Transcriber::Own { program, args }),
            Err(BadCommand::Unreadable) => Err(NoTranscriber::BadCommand),
            Err(BadCommand::Missing) => Err(NoTranscriber::Missing),
        };
    }
    let program =
        crate::audio::find_on_path("whisper-cli", path).ok_or(NoTranscriber::NoProgram)?;
    let model = model.trim();
    if model.is_empty() {
        return Err(NoTranscriber::NoModel);
    }
    let model = textcmd::expand_home(model, home);
    if !model.is_absolute() || !model.is_file() {
        return Err(NoTranscriber::ModelMissing);
    }
    Ok(Transcriber::Whisper {
        program,
        model,
        ffmpeg: crate::audio::find_on_path("ffmpeg", path),
    })
}

/// [`transcriber`] with this process's `PATH` and `HOME`.
pub fn transcriber_here(command: &str, model: &str) -> Result<Transcriber, NoTranscriber> {
    transcriber(
        command,
        model,
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

impl Transcriber {
    /// Whether the sound is converted first, into a file the caller makes.
    pub fn converts(&self) -> bool {
        matches!(
            self,
            Transcriber::Whisper {
                ffmpeg: Some(_),
                ..
            }
        )
    }

    /// Whether the program's lines are stretches of one speech (whisper's are).
    pub fn one_paragraph(&self) -> bool {
        matches!(self, Transcriber::Whisper { .. })
    }

    /// The programs that turn the file `sound` into text. `wav`: an empty file for
    /// the converted sound, where [`Self::converts`].
    ///
    /// whisper-cli: the model, the file, the language found by itself (`-l auto`), no
    /// timestamps (`-nt`) and nothing but the text on its output (`-np`). ffmpeg:
    /// 16 kHz, one channel, 16-bit, which is what whisper.cpp wants of a WAV.
    pub fn steps(&self, sound: &Path, wav: Option<&Path>) -> Vec<Step> {
        let os = |words: &[&str]| words.iter().map(OsString::from).collect::<Vec<_>>();
        let step = |program: &Path, args: Vec<OsString>, time| Step {
            command: Command {
                program: program.to_owned(),
                args,
                env: Vec::new(),
                stdin: None,
            },
            time,
            max_output: MAX_OUTPUT,
        };
        match self {
            Transcriber::Whisper {
                program,
                model,
                ffmpeg,
            } => {
                let mut steps = Vec::new();
                let mut heard = sound;
                if let (Some(ffmpeg), Some(wav)) = (ffmpeg, wav) {
                    let mut args = os(&["-nostdin", "-hide_banner", "-loglevel", "error", "-y"]);
                    // Only the file itself is read, whatever it claims to be (a
                    // playlist would name other files or addresses).
                    args.extend(os(&["-protocol_whitelist", "file", "-i"]));
                    args.push(sound.into());
                    args.extend(os(&[
                        "-vn",
                        "-t",
                        CONVERT_SECONDS,
                        "-ar",
                        "16000",
                        "-ac",
                        "1",
                        "-c:a",
                        "pcm_s16le",
                        "-f",
                        "wav",
                    ]));
                    args.push(wav.into());
                    steps.push(step(ffmpeg, args, CONVERT_TIME));
                    heard = wav;
                }
                let mut args: Vec<OsString> = vec!["-m".into(), model.into()];
                args.extend(os(&["-l", "auto", "-nt", "-np", "-f"]));
                args.push(heard.into());
                steps.push(step(program, args, TRANSCRIBE_TIME));
                steps
            }
            Transcriber::Own { program, args } => {
                let mut args: Vec<OsString> = args.iter().map(OsString::from).collect();
                args.push(sound.into());
                vec![step(program, args, TRANSCRIBE_TIME)]
            }
        }
    }
}

/// What translates: a command of the user's.
#[derive(Clone, PartialEq, Eq)]
pub struct Translator {
    program: PathBuf,
    args: Vec<String>,
}

/// Hand-written: the command is the user's own, and can hold a key to a service.
impl fmt::Debug for Translator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Translator")
    }
}

/// Why nothing translates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoTranslator {
    /// No command is set: translating is not offered.
    NotSet,
    BadCommand,
    Missing,
}

impl NoTranslator {
    /// For a toast, the palette and the settings.
    pub fn message(self) -> &'static str {
        match self {
            NoTranslator::NotSet => "set translate_command to a program that translates",
            NoTranslator::BadCommand => "translate_command could not be read",
            NoTranslator::Missing => "the program named by translate_command was not found",
        }
    }
}

/// The translator for the `translate_command` setting: empty means none.
pub fn translator(
    command: &str,
    path: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<Translator, NoTranslator> {
    if command.trim().is_empty() {
        return Err(NoTranslator::NotSet);
    }
    match textcmd::parse(command, path, home) {
        Ok((program, args)) => Ok(Translator { program, args }),
        Err(BadCommand::Unreadable) => Err(NoTranslator::BadCommand),
        Err(BadCommand::Missing) => Err(NoTranslator::Missing),
    }
}

/// [`translator`] with this process's `PATH` and `HOME`.
pub fn translator_here(command: &str) -> Result<Translator, NoTranslator> {
    translator(
        command,
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// The `translate_language` setting as it is handed on: one line, and the default
/// where it is empty.
pub fn language(setting: &str) -> String {
    let language = crate::text::sanitize_line(setting);
    match language.trim() {
        "" => DEFAULT_LANGUAGE.to_owned(),
        // It becomes an argument of the command: it must not read as an option.
        language if language.starts_with('-') => DEFAULT_LANGUAGE.to_owned(),
        language => language.to_owned(),
    }
}

impl Translator {
    /// The program that translates `text` into `language`: the text on its standard
    /// input (at most [`MAX_INPUT`] characters of it), the language in
    /// `MATRIX_RAT_TRANSLATE_TO` and wherever its arguments say `{lang}`.
    pub fn step(&self, text: &str, language: &str) -> Step {
        let text: String = text.chars().take(MAX_INPUT).collect();
        Step {
            command: Command {
                program: self.program.clone(),
                args: self
                    .args
                    .iter()
                    .map(|arg| OsString::from(arg.replace(LANGUAGE_PLACEHOLDER, language)))
                    .collect(),
                env: vec![(LANGUAGE_VARIABLE, language.to_owned())],
                stdin: Some(text.into_bytes()),
            },
            time: TRANSLATE_TIME,
            max_output: MAX_OUTPUT,
        }
    }
}

/// What the doctor says of transcribing. Never the command or a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscribeFound {
    Whisper { ffmpeg: bool },
    Own,
    Not(NoTranscriber),
}

impl TranscribeFound {
    pub fn of(found: &Result<Transcriber, NoTranscriber>) -> Self {
        match found {
            Ok(Transcriber::Whisper { ffmpeg, .. }) => TranscribeFound::Whisper {
                ffmpeg: ffmpeg.is_some(),
            },
            Ok(Transcriber::Own { .. }) => TranscribeFound::Own,
            Err(why) => TranscribeFound::Not(*why),
        }
    }
}

/// What the doctor says of translating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranslateFound {
    Own,
    Not(NoTranslator),
}

impl TranslateFound {
    pub fn of(found: &Result<Translator, NoTranslator>) -> Self {
        match found {
            Ok(_) => TranslateFound::Own,
            Err(why) => TranslateFound::Not(*why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::FsStorage;
    use crate::textcmd::tests::Dir;
    use std::os::unix::fs::PermissionsExt;

    fn words(step: &Step) -> Vec<String> {
        std::iter::once(step.command.program.clone().into_os_string())
            .chain(step.command.args.iter().cloned())
            .map(|word| word.into_string().unwrap())
            .collect()
    }

    #[test]
    fn auto_wants_whisper_cli_and_a_model_and_takes_ffmpeg_where_it_is() {
        let bin = Dir::new("notes-bin");
        let home = Dir::new("notes-home");
        // Never run: programs that would leave a mark if they were.
        let mark = bin.0.join("ran");
        let script = format!("#!/bin/sh\ntouch {}\n", mark.display());
        let path = Some(bin.0.as_os_str());
        let home_dir = Some(home.0.as_os_str());
        let model = home.file("ggml-base.bin", "model", 0o600);
        let model_text = model.display().to_string();

        assert_eq!(
            transcriber("auto", &model_text, path, home_dir),
            Err(NoTranscriber::NoProgram)
        );
        bin.file("whisper-cli", &script, 0o755);
        assert_eq!(
            transcriber("auto", "  ", path, home_dir),
            Err(NoTranscriber::NoModel)
        );
        assert_eq!(
            transcriber("", "~/no-such-model.bin", path, home_dir),
            Err(NoTranscriber::ModelMissing)
        );
        // A folder, or a path from wherever the app was started, is no model.
        assert_eq!(
            transcriber("auto", &home.0.display().to_string(), path, home_dir),
            Err(NoTranscriber::ModelMissing)
        );
        assert_eq!(
            transcriber("auto", "ggml-base.bin", path, home_dir),
            Err(NoTranscriber::ModelMissing)
        );

        // Without ffmpeg the file goes to whisper-cli as it is.
        let found = transcriber(" Auto ", "~/ggml-base.bin", path, home_dir).unwrap();
        assert!(!found.converts() && found.one_paragraph());
        let steps = found.steps(Path::new("/run/a/play-1.ogg"), None);
        assert_eq!(steps.len(), 1);
        let whisper = bin.0.join("whisper-cli").display().to_string();
        assert_eq!(
            words(&steps[0]),
            [
                whisper.as_str(),
                "-m",
                model_text.as_str(),
                "-l",
                "auto",
                "-nt",
                "-np",
                "-f",
                "/run/a/play-1.ogg"
            ]
        );
        assert!(steps[0].command.stdin.is_none());

        // With it, a WAV is made first and whisper-cli reads that.
        bin.file("ffmpeg", &script, 0o755);
        let found = transcriber("auto", &model_text, path, home_dir).unwrap();
        assert!(found.converts());
        let steps = found.steps(
            Path::new("/run/a/play-1.ogg"),
            Some(Path::new("/run/a/play-2.wav")),
        );
        assert_eq!(steps.len(), 2);
        let ffmpeg = bin.0.join("ffmpeg").display().to_string();
        assert_eq!(
            words(&steps[0]),
            [
                ffmpeg.as_str(),
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-protocol_whitelist",
                "file",
                "-i",
                "/run/a/play-1.ogg",
                "-vn",
                "-t",
                "1800",
                "-ar",
                "16000",
                "-ac",
                "1",
                "-c:a",
                "pcm_s16le",
                "-f",
                "wav",
                "/run/a/play-2.wav"
            ]
        );
        assert_eq!(words(&steps[1]).last().unwrap(), "/run/a/play-2.wav");
        assert!(!mark.exists(), "nothing was run");
        assert_eq!(format!("{found:?}"), "Whisper(ffmpeg: true)");
        assert_eq!(
            TranscribeFound::of(&Ok(found)),
            TranscribeFound::Whisper { ffmpeg: true }
        );
    }

    #[test]
    fn a_transcribe_command_of_the_users_gets_the_file_as_its_last_argument() {
        let bin = Dir::new("notes-own");
        bin.file("my-stt", "", 0o755);
        let path = Some(bin.0.as_os_str());
        // No model is asked for: the command is the user's business.
        let found = transcriber("my-stt --lang 'de at' ; x", "", path, None).unwrap();
        assert!(!found.converts() && !found.one_paragraph());
        let steps = found.steps(Path::new("/run/a/play-1.ogg"), None);
        assert_eq!(
            words(&steps[0])[1..],
            ["--lang", "de at", ";", "x", "/run/a/play-1.ogg"]
        );
        assert_eq!(format!("{found:?}"), "Own");
        assert_eq!(
            transcriber("'open", "", path, None),
            Err(NoTranscriber::BadCommand)
        );
        assert_eq!(
            transcriber("no-such-stt", "", path, None),
            Err(NoTranscriber::Missing)
        );
    }

    #[test]
    fn a_translator_gets_the_language_in_its_arguments_and_environment_and_the_text_to_read() {
        let bin = Dir::new("notes-trans");
        bin.file("trans", "", 0o755);
        let path = Some(bin.0.as_os_str());
        assert_eq!(translator("  ", path, None), Err(NoTranslator::NotSet));
        assert_eq!(
            translator("'open", path, None),
            Err(NoTranslator::BadCommand)
        );
        assert_eq!(
            translator("deepl-cli", path, None),
            Err(NoTranslator::Missing)
        );
        let found = translator("trans -b -t {lang} --note=to-{lang}", path, None).unwrap();
        let step = found.step("Guten Morgen", "English");
        assert_eq!(
            words(&step)[1..],
            ["-b", "-t", "English", "--note=to-English"]
        );
        assert_eq!(
            step.command.env,
            [("MATRIX_RAT_TRANSLATE_TO", "English".to_owned())]
        );
        assert_eq!(step.command.stdin.as_deref(), Some(&b"Guten Morgen"[..]));
        // A very long message is cut before it is handed over.
        let long = "ä".repeat(MAX_INPUT + 10);
        let step = found.step(&long, "en");
        assert_eq!(step.command.stdin.unwrap().len(), MAX_INPUT * 2);
        assert_eq!(format!("{found:?}"), "Translator");

        assert_eq!(language("  "), "en");
        assert_eq!(language(" German \n"), "German");
        assert_eq!(language("pt\u{1b}[31m-BR"), "pt[31m-BR");
    }

    #[test]
    fn program_output_is_made_safe_tidy_and_bounded() {
        assert_eq!(
            clean(
                "\n  Hello there. \n\n\t General\u{1b}[2J Kenobi.\r\n\n",
                true
            ),
            "Hello there. General[2J Kenobi."
        );
        assert_eq!(clean(" one \n\n two\n", false), "one\ntwo");
        assert_eq!(clean("\n \u{200b} \n", false), "");
        let long = "x".repeat(MAX_TEXT + 500);
        let cut = clean(&long, false);
        assert_eq!(cut.chars().count(), MAX_TEXT + 1);
        assert!(cut.ends_with('…'));

        assert_eq!(failure_text(Failure::Cancelled), None);
        assert_eq!(
            failure_text(Failure::Exit(Some(2))).unwrap(),
            "the program failed (exit code 2)"
        );
        assert!(failure_text(Failure::TimedOut).is_some());
        assert!(failure_text(Failure::Start).is_some());
        assert!(failure_text(Failure::Exit(None)).is_some());
    }

    #[test]
    fn notes_are_kept_by_message_and_kind_and_the_oldest_go_first() {
        let mut notes = Notes::default();
        assert!(notes.is_empty());
        notes.set("$a", NoteKind::Translation, NoteState::Running);
        notes.set("$a", NoteKind::Transcript, NoteState::Done("heard".into()));
        assert_eq!(notes.running(), 1);
        // The transcript first, whichever was made first.
        let kinds: Vec<NoteKind> = notes.of("$a").map(|(kind, _)| kind).collect();
        assert_eq!(kinds, [NoteKind::Transcript, NoteKind::Translation]);
        assert!(notes.get("$b", NoteKind::Transcript).is_none());

        let rev = notes.rev;
        assert_eq!(notes.toggle_hidden("$a", NoteKind::Transcript), Some(true));
        assert!(notes.get("$a", NoteKind::Transcript).unwrap().hidden);
        assert_eq!(notes.toggle_hidden("$a", NoteKind::Transcript), Some(false));
        assert_eq!(notes.toggle_hidden("$b", NoteKind::Transcript), None);
        assert!(notes.rev > rev);
        notes.remove("$a", NoteKind::Translation);
        assert_eq!(notes.running(), 0);

        // The bound: the oldest finished ones go, one that runs stays.
        notes.set("$run", NoteKind::Translation, NoteState::Running);
        for n in 0..MAX_NOTES + 5 {
            notes.set(
                &format!("${n}"),
                NoteKind::Translation,
                NoteState::Done("t".into()),
            );
        }
        assert_eq!(notes.map.len(), MAX_NOTES);
        assert!(notes.get("$a", NoteKind::Transcript).is_none());
        assert!(notes.get("$0", NoteKind::Translation).is_none());
        assert!(notes.get("$run", NoteKind::Translation).is_some());
        assert!(notes.get("$10", NoteKind::Translation).is_some());
        // Nothing of them in debug output.
        notes.set(
            "$secret",
            NoteKind::Transcript,
            NoteState::Done("codes".into()),
        );
        let shown = format!("{notes:?} {:?}", notes.get("$secret", NoteKind::Transcript));
        assert!(
            !shown.contains("secret") && !shown.contains("codes"),
            "{shown}"
        );
    }

    #[test]
    fn finished_notes_are_written_owner_only_and_read_back_bounded_and_safe() {
        let dir = Dir::new("notes-file");
        let file = dir.0.join("notes.json");
        let mut notes = Notes::default();
        notes.set(
            "$a",
            NoteKind::Transcript,
            NoteState::Done("heard this".into()),
        );
        notes.set(
            "$a",
            NoteKind::Translation,
            NoteState::Done("read\nthat".into()),
        );
        notes.toggle_hidden("$a", NoteKind::Translation);
        notes.set("$b", NoteKind::Translation, NoteState::Running);
        notes.set("$c", NoteKind::Translation, NoteState::Failed("no".into()));
        let owner = "@alice:example.org";
        let files = FsStorage::default().with_notes(&file);
        // Something lying where the new file is made first is not written through.
        std::fs::write(dir.0.join("notes.json.new"), b"old").unwrap();
        notes.save(&files, owner).unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!dir.0.join("notes.json.new").exists());

        let mut back = Notes::default();
        back.load(&files, owner);
        assert_eq!(back.map.len(), 2, "only what was finished is kept");
        assert_eq!(
            back.get("$a", NoteKind::Transcript).unwrap().state,
            NoteState::Done("heard this".into())
        );
        let translation = back.get("$a", NoteKind::Translation).unwrap();
        assert_eq!(translation.state, NoteState::Done("read\nthat".into()));
        assert!(translation.hidden);

        // Another account's file is dropped (not loaded, and removed).
        let mut wrong = Notes::default();
        wrong.load(&files, "@bob:example.org");
        assert!(wrong.is_empty());
        assert!(!file.exists());

        // An old v1 file without an owner cannot be attributed and is dropped.
        std::fs::write(
            &file,
            r#"{"version":1,"notes":[{"event":"$old","kind":"transcript","text":"legacy"}]}"#,
        )
        .unwrap();
        let mut legacy = Notes::default();
        legacy.load(&files, owner);
        assert!(legacy.is_empty());
        assert!(!file.exists());

        // A file written by anything: escapes go, nonsense is skipped, the newest stay.
        let mut many: Vec<String> = (0..MAX_NOTES + 3)
            .map(|n| format!(r#"{{"event":"${n}","kind":"translation","text":"t"}}"#))
            .collect();
        many.push(r#"{"event":"$esc","kind":"transcript","text":"a\u001b[2Jb"}"#.to_owned());
        many.push(r#"{"event":"","kind":"transcript","text":"x"}"#.to_owned());
        many.push(r#"{"event":"$blank","kind":"transcript","text":" \n "}"#.to_owned());
        let json = format!(
            r#"{{"version":1,"owner":"{owner}","notes":[{}]}}"#,
            many.join(",")
        );
        std::fs::write(&file, json).unwrap();
        let mut back = Notes::default();
        back.load(&files, owner);
        // 204 of them are sound: the two that are not take nobody's place.
        assert_eq!(back.map.len(), MAX_NOTES);
        assert_eq!(
            back.get("$esc", NoteKind::Transcript).unwrap().state,
            NoteState::Done("a[2Jb".into())
        );
        assert!(back.get("$blank", NoteKind::Transcript).is_none());
        assert!(back.get("$3", NoteKind::Translation).is_none());
        assert!(back.get("$4", NoteKind::Translation).is_some());

        // A file of a version this one does not know: no notes, left untouched.
        let json = format!(
            r#"{{"version":2,"owner":"{owner}","notes":[{{"event":"$a","kind":"transcript","text":"t"}}]}}"#
        );
        std::fs::write(&file, &json).unwrap();
        let mut back = Notes::default();
        back.load(&files, owner);
        assert!(back.is_empty());
        assert!(file.exists());

        // Not JSON, or not there: no notes, no error.
        std::fs::write(&file, b"not json").unwrap();
        let mut back = Notes::default();
        back.load(&files, owner);
        back.load(
            &FsStorage::default().with_notes(dir.0.join("missing.json")),
            owner,
        );
        assert!(back.is_empty());

        files.remove(&Key::Notes, Removal::Value).unwrap();
        assert!(!file.exists());
        files.remove(&Key::Notes, Removal::Value).unwrap();
    }

    #[test]
    fn a_link_a_folder_or_a_huge_file_in_place_of_the_notes_is_not_read_or_written_through() {
        let dir = Dir::new("notes-odd");
        let file = dir.0.join("notes.json");
        let owner = "@alice:example.org";
        let files = FsStorage::default().with_notes(&file);
        let good = format!(
            r#"{{"version":1,"owner":"{owner}","notes":[{{"event":"$a","kind":"transcript","text":"t"}}]}}"#
        );
        let mut notes = Notes::default();
        notes.set(
            "$mine",
            NoteKind::Transcript,
            NoteState::Done("mine".into()),
        );

        // A link to a file elsewhere: not followed when reading; writing puts a file
        // of our own in the link's place and leaves the other file as it was.
        let elsewhere = dir.0.join("elsewhere.json");
        std::fs::write(&elsewhere, &good).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &file).unwrap();
        let mut back = Notes::default();
        back.load(&files, owner);
        assert!(back.is_empty());
        notes.save(&files, owner).unwrap();
        assert!(
            std::fs::symlink_metadata(&file)
                .unwrap()
                .file_type()
                .is_file()
        );
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), good);
        std::fs::remove_file(&file).unwrap();

        // Larger than anything we write: not read into memory.
        std::fs::write(&file, &good).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .unwrap()
            .set_len(MAX_FILE + 1)
            .unwrap();
        let mut back = Notes::default();
        back.load(&files, owner);
        assert!(back.is_empty());
        std::fs::remove_file(&file).unwrap();

        // A folder: no notes, and writing fails without touching what is in it.
        std::fs::create_dir(&file).unwrap();
        std::fs::write(file.join("inside"), b"kept").unwrap();
        let mut back = Notes::default();
        back.load(&files, owner);
        assert!(back.is_empty());
        assert!(notes.save(&files, owner).is_err());
        assert_eq!(std::fs::read(file.join("inside")).unwrap(), b"kept");
        assert!(!dir.0.join("notes.json.new").exists());
        assert!(files.remove(&Key::Notes, Removal::Value).is_err());
    }

    #[test]
    fn a_note_knows_what_it_was_made_from() {
        // The same at every start, never 0, and the parts do not run into each other.
        assert_eq!(source(&["a", "b"]), source(&["a", "b"]));
        assert_ne!(source(&["ab", ""]), source(&["a", "b"]));
        assert_ne!(source(&["hallo", "en"]), source(&["hallo", "fr"]));
        assert_ne!(source(&[]), 0);
        assert_eq!(source(&["mxc://example.org/v"]), 0x2391_d9c4_71a7_8a17);

        let mut notes = Notes::default();
        notes.set_from("$a", NoteKind::Translation, NoteState::Running, 7);
        // The run's end is of what its start was of.
        notes.set("$a", NoteKind::Translation, NoteState::Done("t".into()));
        let note = notes.get("$a", NoteKind::Translation).unwrap();
        assert!(note.is_of(7) && !note.is_of(8));
        // Kept with the note.
        let dir = Dir::new("notes-source");
        let files = FsStorage::default().with_notes(dir.0.join("notes.json"));
        notes.save(&files, "@alice:example.org").unwrap();
        let mut back = Notes::default();
        back.load(&files, "@alice:example.org");
        assert_eq!(back.get("$a", NoteKind::Translation).unwrap().source, 7);
        // One of which nothing is known is taken as it is.
        notes.set_from("$b", NoteKind::Translation, NoteState::Done("t".into()), 0);
        assert!(notes.get("$b", NoteKind::Translation).unwrap().is_of(9));
        notes.clear();
        assert!(notes.is_empty());
    }

    #[test]
    fn a_language_that_reads_as_an_option_is_not_handed_on() {
        assert_eq!(language(" fr "), "fr");
        assert_eq!(language(""), DEFAULT_LANGUAGE);
        assert_eq!(language("--engine=evil"), DEFAULT_LANGUAGE);
        assert_eq!(language(" -x"), DEFAULT_LANGUAGE);
        assert_eq!(language("pt-BR"), "pt-BR");
    }
}
