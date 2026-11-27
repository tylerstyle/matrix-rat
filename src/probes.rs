//! What `App` asks of the machine it runs on: whether colours, programs and a desktop
//! are there. `App` does not look itself: it asks the [`Probes`] the shell gave it, at
//! start and again when a setting changes what is to be found (or when the user asks
//! for something that needs it). On this system [`NativeProbes`] looks as the app
//! always has: the same variables, the same `PATH` look-ups, the same files. A shell
//! without programs to run (the browser's) answers that nothing is there:
//! [`NothingHere`].

use crate::audio::recorder::{NoRecorder, Recorder};
use crate::audio::{NoPlayer, Player, is_auto};
use crate::catchup::{self, NoCommand};
use crate::notes::{NoTranscriber, NoTranslator, Transcriber, Translator};
use crate::notify::NotifyMode;
#[cfg(not(target_family = "wasm"))]
use crate::{audio, audio::recorder, notes};

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
    /// What the `notifications` setting's `auto` comes to here: the desktop's
    /// notification service where a session bus is announced and the session is not
    /// over SSH, else the terminal's.
    fn notify_auto(&self) -> NotifyMode;
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
#[cfg(not(target_family = "wasm"))]
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeProbes;

#[cfg(not(target_family = "wasm"))]
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

    fn notify_auto(&self) -> NotifyMode {
        NotifyMode::Auto.resolved()
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

    fn notify_auto(&self) -> NotifyMode {
        NativeProbes.notify_auto()
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

/// A machine where nothing is found, as a shell without programs answers (the
/// browser's): no player, no recorder, no programs for notes or summaries, no editor,
/// no clipboard tools, no desktop to notify on, and not over SSH; colours are drawn as
/// the theme has them. What a setting names is not found here, and `auto` finds none.
/// Nothing is looked at, so the answers are the same everywhere (the tests use it too).
#[derive(Debug, Clone, Copy, Default)]
pub struct NothingHere;

impl Probes for NothingHere {
    fn truecolor(&self) -> bool {
        true
    }

    fn no_color(&self) -> bool {
        false
    }

    fn ssh(&self) -> bool {
        false
    }

    fn notify_auto(&self) -> NotifyMode {
        NotifyMode::Terminal
    }

    fn player(&self, setting: &str) -> Result<Player, NoPlayer> {
        Err(if is_auto(setting) {
            NoPlayer::NoneFound
        } else {
            NoPlayer::Missing
        })
    }

    fn recorder(&self, setting: &str, ssh: bool) -> Result<Recorder, NoRecorder> {
        Err(match (is_auto(setting), ssh) {
            (true, true) => NoRecorder::OverSsh,
            (true, false) => NoRecorder::NoMicrophone,
            (false, _) => NoRecorder::Missing,
        })
    }

    fn transcriber(&self, command: &str, _model: &str) -> Result<Transcriber, NoTranscriber> {
        Err(if is_auto(command) {
            NoTranscriber::NoProgram
        } else {
            NoTranscriber::Missing
        })
    }

    fn translator(&self, command: &str) -> Result<Translator, NoTranslator> {
        Err(if command.trim().is_empty() {
            NoTranslator::NotSet
        } else {
            NoTranslator::Missing
        })
    }

    fn summary(&self, setting: &str) -> Result<catchup::Command, NoCommand> {
        Err(if setting.trim().is_empty() {
            NoCommand::NotSet
        } else {
            NoCommand::Missing
        })
    }

    fn editor(&self) -> Result<Vec<String>, crate::editor::Failure> {
        Err(crate::editor::Failure::NotSet)
    }

    fn clipboard_tools(&self) -> Vec<crate::clipboard::Tool> {
        Vec::new()
    }

    fn environment(&self) -> crate::doctor::Facts {
        crate::doctor::Facts::nothing_here()
    }
}

/// [`NothingHere`], noting what it was asked, for the tests that look at when the app
/// asks.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct Asked {
    pub asked: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
}

#[cfg(test)]
impl Asked {
    fn ask(&self, what: &'static str) -> NothingHere {
        self.asked.borrow_mut().push(what);
        NothingHere
    }
}

#[cfg(test)]
impl Probes for Asked {
    fn truecolor(&self) -> bool {
        self.ask("truecolor").truecolor()
    }

    fn no_color(&self) -> bool {
        self.ask("no_color").no_color()
    }

    fn ssh(&self) -> bool {
        self.ask("ssh").ssh()
    }

    fn notify_auto(&self) -> NotifyMode {
        self.ask("notify_auto").notify_auto()
    }

    fn player(&self, setting: &str) -> Result<Player, NoPlayer> {
        self.ask("player").player(setting)
    }

    fn recorder(&self, setting: &str, ssh: bool) -> Result<Recorder, NoRecorder> {
        self.ask("recorder").recorder(setting, ssh)
    }

    fn transcriber(&self, command: &str, model: &str) -> Result<Transcriber, NoTranscriber> {
        self.ask("transcriber").transcriber(command, model)
    }

    fn translator(&self, command: &str) -> Result<Translator, NoTranslator> {
        self.ask("translator").translator(command)
    }

    fn summary(&self, setting: &str) -> Result<catchup::Command, NoCommand> {
        self.ask("summary").summary(setting)
    }

    fn editor(&self) -> Result<Vec<String>, crate::editor::Failure> {
        self.ask("editor").editor()
    }

    fn clipboard_tools(&self) -> Vec<crate::clipboard::Tool> {
        self.ask("clipboard_tools").clipboard_tools()
    }

    fn environment(&self) -> crate::doctor::Facts {
        self.ask("environment").environment()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where nothing is there, every program is missing and `auto` finds none, whatever
    /// the setting says; nothing of the machine the test runs on counts.
    #[test]
    fn nothing_here_finds_nothing_whatever_the_settings_say() {
        let here = NothingHere;
        assert!(here.truecolor() && !here.no_color() && !here.ssh());
        assert_eq!(here.notify_auto(), NotifyMode::Terminal);
        assert_eq!(here.player("auto").err(), Some(NoPlayer::NoneFound));
        assert_eq!(here.player(" ").err(), Some(NoPlayer::NoneFound));
        assert_eq!(here.player("sh").err(), Some(NoPlayer::Missing));
        assert_eq!(
            here.recorder("auto", false).err(),
            Some(NoRecorder::NoMicrophone)
        );
        assert_eq!(here.recorder("", true).err(), Some(NoRecorder::OverSsh));
        assert_eq!(
            here.recorder("sh -c true", true).err(),
            Some(NoRecorder::Missing)
        );
        assert_eq!(
            here.transcriber("auto", "/").err(),
            Some(NoTranscriber::NoProgram)
        );
        assert_eq!(
            here.transcriber("sh", "").err(),
            Some(NoTranscriber::Missing)
        );
        assert_eq!(here.translator(" ").err(), Some(NoTranslator::NotSet));
        assert_eq!(here.translator("sh").err(), Some(NoTranslator::Missing));
        assert_eq!(here.summary("").err(), Some(NoCommand::NotSet));
        assert_eq!(here.summary("sh").err(), Some(NoCommand::Missing));
        assert_eq!(here.editor(), Err(crate::editor::Failure::NotSet));
        assert!(here.clipboard_tools().is_empty());
        let facts = here.environment();
        assert!(facts.term.is_empty() && facts.size.is_none() && !facts.ssh);
        assert!(!facts.session_bus && !facts.wayland && !facts.x11);
        assert!(!facts.wl_paste && !facts.xclip && !facts.opener && !facts.editor_set);
        assert_eq!(facts.version, env!("CARGO_PKG_VERSION"));
    }
}
