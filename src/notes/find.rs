//! Finding the programs on this machine: `whisper-cli`, `ffmpeg` and the model file
//! for `"auto"`, or the user's own command lines, looked up on `PATH` and on disk
//! without running anything.

use std::ffi::OsStr;

use super::{NoTranscriber, NoTranslator, Transcriber, Translator};
use crate::textcmd::{self, BadCommand};

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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::notes::{MAX_INPUT, TranscribeFound, language};
    use crate::textcmd::Step;
    use crate::textcmd::tests::Dir;

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
}
