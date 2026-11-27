//! "Give a program bytes or a file, get bounded text back": what transcribing an audio
//! message and translating a message are made of (see `crate::notes`).
//!
//! A run is one or more programs, one after the other (a converter, then the program
//! that reads its result); the text is what the last one wrote to its standard output.
//! Each is started without a shell, in a process group of its own, with nothing of the
//! terminal: what it reads is what we feed it (or nothing), its errors go nowhere. It
//! has a time limit and its output a size limit, and it never outlives the app: it is
//! killed, with everything it started, when the app cancels the run, when the app's end
//! of the channel goes, when the task is dropped, and by the kernel when the app dies
//! any other way (`PR_SET_PDEATHSIG`). This is the manner of `audio::play`.
//!
//! What is fed and what comes back is a message's text: none of it is ever logged, and
//! the types that hold it print none of it.
//!
//! The types of a run are here; starting and watching the programs, and finding a
//! setting's program on `PATH`, are in [`process`].

#[cfg(not(target_family = "wasm"))]
mod process;

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::oneshot;

use crate::audio::TempAudio;

#[cfg(not(target_family = "wasm"))]
pub use process::{parse, run};

/// The most that is kept of what a program writes: far more than a message's text.
pub const MAX_OUTPUT: usize = 64 * 1024;

/// A program with its arguments, as it is started: no shell reads any of it.
pub struct Command {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Added to the environment the app runs in.
    pub env: Vec<(&'static str, String)>,
    /// What it reads on its standard input; `None`: nothing (`/dev/null`).
    pub stdin: Option<Vec<u8>>,
}

/// Hand-written: the program and its arguments can name the user's files, and what it
/// reads is a message's text.
impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Command")
            .field("args", &self.args.len())
            .field("env", &self.env.len())
            .field("stdin", &self.stdin.as_ref().map(Vec::len))
            .finish_non_exhaustive()
    }
}

/// One program of a run, and its limits.
#[derive(Debug)]
pub struct Step {
    pub command: Command,
    /// Killed when it has not ended after this long.
    pub time: Duration,
    /// Killed when it has written more than this; what it wrote up to there counts.
    pub max_output: usize,
}

/// Programs to run one after the other.
pub struct Request {
    /// The app's number for the run, carried by the answer.
    pub id: u64,
    /// Each must end well (exit code 0) for the next to start.
    pub steps: Vec<Step>,
    /// Files the programs read or write: removed when the run has ended, however.
    pub files: Vec<TempAudio>,
    /// A word on it, or its other end dropped: the run is given up.
    pub cancel: oneshot::Receiver<()>,
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("id", &self.id)
            .field("steps", &self.steps.len())
            .field("files", &self.files.len())
            .finish_non_exhaustive()
    }
}

/// What the last program wrote to its standard output.
#[derive(Clone, PartialEq, Eq)]
pub struct Output {
    /// Not yet made safe for the screen: it is whatever the program wrote.
    pub text: String,
    /// It wrote more than was kept.
    pub truncated: bool,
}

/// Hand-written: the text is a message's, or says what one said.
impl fmt::Debug for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Output")
            .field("len", &self.text.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// Why a run brought no text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// A program could not be started at all.
    Start,
    /// A program ended with this exit code (`None`: by a signal).
    Exit(Option<i32>),
    TimedOut,
    /// The app gave it up.
    Cancelled,
}

/// The last word of a run: its programs are gone and waited for, its files removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    pub id: u64,
    pub result: Result<Output, Failure>,
}

/// Why a setting is no command to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadCommand {
    /// It could not be split into words (a quote left open), or is no command at all.
    Unreadable,
    /// The program it names is not there, or cannot be run.
    Missing,
}

/// `~/x` as a path in `home`; anything else as it is.
pub fn expand_home(text: &str, home: Option<&OsStr>) -> PathBuf {
    match (text.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => Path::new(home).join(rest),
        _ => PathBuf::from(text),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    pub(crate) const LONG: Duration = Duration::from_secs(60);

    /// A fresh empty folder for one test, removed at its end.
    pub(crate) struct Dir(pub PathBuf);

    impl Dir {
        pub fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "matrix-rat-textcmd-{name}-{}-{}",
                std::process::id(),
                crate::time::SystemTime::now()
                    .duration_since(crate::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        /// A file `name` here, with `mode`.
        pub fn file(&self, name: &str, text: &str, mode: u32) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        }

        /// A program that is a shell script, read by `sh` (never executed itself, so
        /// no test can find it busy being written): it notes its process number in
        /// this folder, then does `body`.
        pub fn script(&self, name: &str, body: &str) -> Command {
            let at = self.0.display();
            let script = format!(
                "echo $$ > '{at}/{name}.new' && mv '{at}/{name}.new' '{at}/{name}.pid'\n{body}\n"
            );
            let file = self.file(&format!("{name}.sh"), &script, 0o644);
            Command {
                program: PathBuf::from("/bin/sh"),
                args: vec![file.into_os_string()],
                env: Vec::new(),
                stdin: None,
            }
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn step(command: Command) -> Step {
        Step {
            command,
            time: LONG,
            max_output: MAX_OUTPUT,
        }
    }

    #[test]
    fn what_is_fed_and_what_comes_back_stay_out_of_debug_output() {
        let command = Command {
            program: PathBuf::from("/home/alice/bin/translate"),
            args: vec!["--key=secret".into()],
            env: vec![("MATRIX_RAT_TRANSLATE_TO", "Klingon".to_owned())],
            stdin: Some(b"the launch codes".to_vec()),
        };
        let (_cancel, cancelled) = oneshot::channel();
        let request = Request {
            id: 3,
            steps: vec![step(command)],
            files: Vec::new(),
            cancel: cancelled,
        };
        let done = Done {
            id: 3,
            result: Ok(Output {
                text: "the launch codes".to_owned(),
                truncated: false,
            }),
        };
        let shown = format!("{request:?} {:?} {done:?}", request.steps);
        for private in ["alice", "secret", "Klingon", "launch"] {
            assert!(!shown.contains(private), "{private} in {shown}");
        }
    }
}
