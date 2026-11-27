//! Catching up: the unread messages of a room, summarised by a program of the user's
//! own (the `summary_command` setting), for the user alone.
//!
//! No service is built in and nothing is sent anywhere by this app: the command gets
//! the text on its standard input (one message a line: time, sender, text) and
//! answers with the summary on its standard output, ending with exit code 0. Where
//! the text goes from there is the command's business, which is why there is none
//! until the user names one.
//!
//! The command is started as the audio player is (see `crate::audio::play`): without
//! a shell, in a process group of its own, killed with everything it started when the
//! app says stop, when its time is up, when it says too much, and when the app ends.
//! What it answers is somebody else's text: it is bounded and cleaned before it is
//! shown.
//!
//! The runner is small and private to this feature on purpose; it is in [`process`],
//! with finding the command on `PATH`. The data and the cleaning are here.

mod process;

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::text::sanitize;

#[cfg(test)]
pub use process::choose;
pub use process::{choose_here, run};

/// How long the command may take.
pub const TIME_LIMIT: Duration = Duration::from_secs(180);
/// The most the command may say, in bytes; beyond that it is stopped.
pub const OUTPUT_MAX: usize = 64 * 1024;
/// The most of a summary that is shown, in characters and in lines.
pub const SHOWN_CHARS: usize = 8000;
pub const SHOWN_LINES: usize = 200;

/// The user's command for summaries, ready to start.
#[derive(Clone, PartialEq, Eq)]
pub struct Command {
    program: PathBuf,
    args: Vec<String>,
}

/// Hand-written: a command of the user's can name their own files, and a key.
impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Command({} arguments)", self.args.len())
    }
}

impl Command {
    /// The name of its program, for the settings and the popup.
    pub fn name(&self) -> String {
        self.program
            .file_name()
            .map(|name| crate::text::sanitize_line(&name.to_string_lossy()))
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn for_test(program: PathBuf, args: Vec<String>) -> Self {
        Self { program, args }
    }
}

/// Why there is no command to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoCommand {
    /// The setting is empty: summaries are not set up.
    NotSet,
    /// The setting could not be split into words (a quote left open).
    BadCommand,
    /// The program the setting names is not there, or cannot be run.
    Missing,
}

impl NoCommand {
    /// For a toast, and (shortened by the caller) the reason next to a greyed entry.
    pub fn message(self) -> &'static str {
        match self {
            NoCommand::NotSet => {
                "Catching up needs a summary command: set summary_command (Settings, Summary command)"
            }
            NoCommand::BadCommand => {
                "The summary_command setting could not be read: is a quote left open?"
            }
            NoCommand::Missing => "The program named in summary_command was not found",
        }
    }

    /// In a few words, for the command palette.
    pub fn short(self) -> &'static str {
        match self {
            NoCommand::NotSet => "set summary_command first",
            NoCommand::BadCommand => "summary_command cannot be read",
            NoCommand::Missing => "the summary command was not found",
        }
    }
}

/// A text to summarise.
pub struct Request {
    /// The app's number for it, carried by the answer.
    pub id: u64,
    pub command: Command,
    /// What the command reads.
    pub input: String,
    /// Closed by the app (dropped), or sent to: stop.
    pub stop: oneshot::Receiver<()>,
}

/// Hand-written: the text is what people wrote in a room.
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("id", &self.id)
            .field("input_bytes", &self.input.len())
            .finish_non_exhaustive()
    }
}

/// Why there is no summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The command could not be started.
    Start,
    /// It ended with an error (an exit code other than 0, or a signal).
    Exit,
    /// It took longer than [`TIME_LIMIT`].
    TimedOut,
    /// It said more than [`OUTPUT_MAX`].
    TooMuch,
    /// It said nothing.
    Empty,
}

impl Failure {
    pub fn message(self) -> &'static str {
        match self {
            Failure::Start => "The summary command could not be started",
            Failure::Exit => "The summary command ended with an error",
            Failure::TimedOut => "The summary command took too long and was stopped",
            Failure::TooMuch => "The summary command said too much and was stopped",
            Failure::Empty => "The summary command gave no summary",
        }
    }
}

/// How a run ended.
#[derive(Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The summary, cleaned and bounded.
    Summary(String),
    Failed(Failure),
    /// The app said stop.
    Stopped,
}

/// Hand-written: a summary says what a room was about.
impl fmt::Debug for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Summary(text) => write!(f, "Summary({} bytes)", text.len()),
            Outcome::Failed(failure) => write!(f, "Failed({failure:?})"),
            Outcome::Stopped => f.write_str("Stopped"),
        }
    }
}

/// The last word of a run: the command is gone and waited for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    pub id: u64,
    pub outcome: Outcome,
}

/// What a command said, as text that is safe to show: no control characters, no
/// more than [`SHOWN_LINES`] lines and [`SHOWN_CHARS`] characters (a last line says
/// when it was cut). `None` if nothing is left.
pub fn clean(output: &[u8]) -> Option<String> {
    let text = sanitize(&String::from_utf8_lossy(output));
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut out = String::new();
    let mut chars = 0;
    let mut cut = false;
    for (at, line) in text.lines().enumerate() {
        if at >= SHOWN_LINES {
            cut = true;
            break;
        }
        let line = line.trim_end();
        let room = SHOWN_CHARS - chars;
        let length = line.chars().count() + 1;
        if length > room {
            // Shown as far as it goes.
            out.extend(line.chars().take(room));
            out.push('\n');
            cut = true;
            break;
        }
        chars += length;
        out.push_str(line);
        out.push('\n');
    }
    if cut {
        out.push_str("[The summary was longer; the rest is not shown.]\n");
    }
    Some(out.trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_command_says_is_cleaned_and_bounded() {
        assert_eq!(
            clean(b"  \x1b[31mred\x1b[0m line\r\n\x07second\tline  \n\n").as_deref(),
            Some("[31mred[0m line\nsecond    line")
        );
        assert_eq!(clean(b" \n\t\n"), None);
        assert_eq!(clean(b""), None);
        // Not UTF-8: shown as far as it can be.
        assert!(clean(b"caf\xff ok").unwrap().ends_with(" ok"));

        let many = "line\n".repeat(SHOWN_LINES + 50);
        let shown = clean(many.as_bytes()).unwrap();
        assert_eq!(shown.lines().count(), SHOWN_LINES + 1);
        assert!(shown.ends_with("the rest is not shown.]"));

        let long = "x".repeat(SHOWN_CHARS * 3);
        let shown = clean(long.as_bytes()).unwrap();
        assert!(shown.chars().count() < SHOWN_CHARS + 100);
        assert!(shown.starts_with("xxx") && shown.ends_with("the rest is not shown.]"));

        let wide = format!("{}\n", "y".repeat(100)).repeat(SHOWN_CHARS / 50);
        let shown = clean(wide.as_bytes()).unwrap();
        assert!(shown.chars().count() <= SHOWN_CHARS + 100);
        assert!(shown.ends_with("the rest is not shown.]"));
    }

    #[test]
    fn the_debug_output_names_no_command_and_no_text() {
        let command = Command::for_test(
            "/home/alice/bin/secret-tool".into(),
            vec!["--key=hunter2".into()],
        );
        let (_stop, stopped) = oneshot::channel();
        let request = Request {
            id: 1,
            command: command.clone(),
            input: "the secret plan".to_owned(),
            stop: stopped,
        };
        let shown = format!(
            "{command:?} {request:?} {:?}",
            Outcome::Summary("the secret summary".to_owned())
        );
        for private in ["alice", "secret", "hunter2", "plan"] {
            assert!(!shown.contains(private), "{shown}");
        }
    }
}
