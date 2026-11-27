//! What `App` asks of the machine it runs on: whether colours, programs and a desktop
//! are there. `App` does not look itself: it asks the [`Probes`] the shell gave it, at
//! start and again when a setting changes what is to be found (or when the user asks
//! for something that needs it). On this system [`NativeProbes`] looks as the app
//! always has: the same variables, the same `PATH` look-ups, the same files. A shell
//! without programs to run (the browser's) answers that nothing is there.

use crate::audio::recorder::{self, NoRecorder, Recorder};
use crate::audio::{self, NoPlayer, Player};
use crate::catchup::{self, NoCommand};
use crate::notes::{self, NoTranscriber, NoTranslator, Transcriber, Translator};

/// The questions. Each answer is what the app would find on its own: nothing here
/// starts a program, and the answers say nothing of the user's that the app did not
/// already hold.
pub trait Probes {
    /// The terminal shows 24-bit colours (`COLORTERM`).
    fn truecolor(&self) -> bool;
    /// The user asked for no colours (`NO_COLOR`).
    fn no_color(&self) -> bool;
    /// A session over SSH: what plays or records would be the far machine's.
    fn ssh(&self) -> bool;
    /// The player the `audio_player` setting comes to here.
    fn player(&self, setting: &str) -> Result<Player, NoPlayer>;
    /// The recorder the `audio_recorder` setting comes to here, `ssh` as the app
    /// knows it.
    fn recorder(&self, setting: &str, ssh: bool) -> Result<Recorder, NoRecorder>;
    /// What transcribes, from the `transcribe_command` and `transcribe_model`
    /// settings.
    fn transcriber(&self, command: &str, model: &str) -> Result<Transcriber, NoTranscriber>;
    /// What translates, from the `translate_command` setting.
    fn translator(&self, command: &str) -> Result<Translator, NoTranslator>;
    /// The `summary_command` setting's program.
    fn summary(&self, setting: &str) -> Result<catchup::Command, NoCommand>;
    /// The user's editor (`VISUAL`, `EDITOR`), as a command line.
    fn editor(&self) -> Result<Vec<String>, crate::editor::Failure>;
    /// The programs that could read a picture from the clipboard here.
    fn clipboard_tools(&self) -> Vec<crate::clipboard::Tool>;
    /// What the environment tells the doctor, with the rest left for the app to fill
    /// in from what it knows.
    fn environment(&self) -> crate::doctor::Facts;
}

/// This process's environment and this machine: what the app has always looked at.
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeProbes;

impl Probes for NativeProbes {
    fn truecolor(&self) -> bool {
        crate::theme::truecolor_supported()
    }

    fn no_color(&self) -> bool {
        crate::theme::no_color_wanted()
    }

    fn ssh(&self) -> bool {
        crate::portal::in_ssh_session()
    }

    fn player(&self, setting: &str) -> Result<Player, NoPlayer> {
        audio::choose_here(setting)
    }

    fn recorder(&self, setting: &str, ssh: bool) -> Result<Recorder, NoRecorder> {
        recorder::choose(
            setting,
            &recorder::Env {
                ssh,
                ..recorder::Env::here()
            },
        )
    }

    fn transcriber(&self, command: &str, model: &str) -> Result<Transcriber, NoTranscriber> {
        notes::transcriber_here(command, model)
    }

    fn translator(&self, command: &str) -> Result<Translator, NoTranslator> {
        notes::translator_here(command)
    }

    fn summary(&self, setting: &str) -> Result<catchup::Command, NoCommand> {
        catchup::choose_here(setting)
    }

    fn editor(&self) -> Result<Vec<String>, crate::editor::Failure> {
        crate::editor::command_from_env()
    }

    fn clipboard_tools(&self) -> Vec<crate::clipboard::Tool> {
        crate::clipboard::tools_from_env()
    }

    fn environment(&self) -> crate::doctor::Facts {
        crate::doctor::Facts::from_environment()
    }
}

/// What the tests' apps ask: this machine, as before, except where a test must not
/// depend on it. The recorder is looked for on the machine the test names (an empty
/// one unless it says otherwise), and there is no SSH.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct TestProbes {
    pub recorder_env: recorder::Env,
}

#[cfg(test)]
impl Probes for TestProbes {
    fn truecolor(&self) -> bool {
        NativeProbes.truecolor()
    }

    fn no_color(&self) -> bool {
        NativeProbes.no_color()
    }

    fn ssh(&self) -> bool {
        false
    }

    fn player(&self, setting: &str) -> Result<Player, NoPlayer> {
        NativeProbes.player(setting)
    }

    fn recorder(&self, setting: &str, ssh: bool) -> Result<Recorder, NoRecorder> {
        recorder::choose(
            setting,
            &recorder::Env {
                ssh,
                ..self.recorder_env.clone()
            },
        )
    }

    fn transcriber(&self, command: &str, model: &str) -> Result<Transcriber, NoTranscriber> {
        NativeProbes.transcriber(command, model)
    }

    fn translator(&self, command: &str) -> Result<Translator, NoTranslator> {
        NativeProbes.translator(command)
    }

    fn summary(&self, setting: &str) -> Result<catchup::Command, NoCommand> {
        NativeProbes.summary(setting)
    }

    fn editor(&self) -> Result<Vec<String>, crate::editor::Failure> {
        NativeProbes.editor()
    }

    fn clipboard_tools(&self) -> Vec<crate::clipboard::Tool> {
        NativeProbes.clipboard_tools()
    }

    fn environment(&self) -> crate::doctor::Facts {
        NativeProbes.environment()
    }
}

/// A machine where nothing is found, as a shell without programs answers: no player,
/// no recorder, no programs for notes or summaries, no editor, no clipboard tools.
/// Records what it was asked, for the tests that look at when the app asks.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct NothingHere {
    pub asked: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
}

#[cfg(test)]
impl NothingHere {
    fn ask(&self, what: &'static str) {
        self.asked.borrow_mut().push(what);
    }
}

#[cfg(test)]
impl Probes for NothingHere {
    fn truecolor(&self) -> bool {
        self.ask("truecolor");
        true
    }

    fn no_color(&self) -> bool {
        self.ask("no_color");
        false
    }

    fn ssh(&self) -> bool {
        self.ask("ssh");
        false
    }

    fn player(&self, _: &str) -> Result<Player, NoPlayer> {
        self.ask("player");
        Err(NoPlayer::NoneFound)
    }

    fn recorder(&self, setting: &str, ssh: bool) -> Result<Recorder, NoRecorder> {
        self.ask("recorder");
        recorder::choose(
            setting,
            &recorder::Env {
                ssh,
                ..recorder::Env::default()
            },
        )
    }

    fn transcriber(&self, command: &str, model: &str) -> Result<Transcriber, NoTranscriber> {
        self.ask("transcriber");
        notes::transcriber(command, model, None, None)
    }

    fn translator(&self, command: &str) -> Result<Translator, NoTranslator> {
        self.ask("translator");
        notes::translator(command, None, None)
    }

    fn summary(&self, setting: &str) -> Result<catchup::Command, NoCommand> {
        self.ask("summary");
        catchup::choose(setting, None, None)
    }

    fn editor(&self) -> Result<Vec<String>, crate::editor::Failure> {
        self.ask("editor");
        crate::editor::command(None, None)
    }

    fn clipboard_tools(&self) -> Vec<crate::clipboard::Tool> {
        self.ask("clipboard_tools");
        crate::clipboard::tools(None, None)
    }

    fn environment(&self) -> crate::doctor::Facts {
        self.ask("environment");
        crate::doctor::sample()
    }
}
