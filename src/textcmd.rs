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

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::audio::TempAudio;

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

/// Runs the programs of `request` and tells `events` how it went. Run it as a task of
/// its own.
pub async fn run(request: Request, events: UnboundedSender<Done>) {
    let Request {
        id,
        steps,
        files,
        mut cancel,
    } = request;
    let mut result = Err(Failure::Start);
    for step in steps {
        result = run_step(step, &mut cancel).await;
        if result.is_err() {
            break;
        }
    }
    tracing::debug!(ok = result.is_ok(), "a program run for a message ended");
    // The files are gone before the app hears of the end.
    drop(files);
    let _ = events.send(Done { id, result });
}

async fn run_step(step: Step, cancel: &mut oneshot::Receiver<()>) -> Result<Output, Failure> {
    let Step {
        mut command,
        time,
        max_output,
    } = step;
    let input = command.stdin.take();
    let mut running = Running::start(&command, input.is_some()).map_err(|err| {
        tracing::debug!(why = %crate::logsafe::kind(&err), "could not start a program for a message");
        Failure::Start
    })?;
    let result = tokio::select! {
        result = running.talk(input, max_output) => result,
        () = tokio::time::sleep(time) => Err(Failure::TimedOut),
        // A word, or the app gone: both mean stop.
        _ = cancel => Err(Failure::Cancelled),
    };
    running.end().await;
    result
}

/// A started program.
struct Running {
    child: tokio::process::Child,
    /// Its process group, numbered as itself.
    group: libc::pid_t,
    /// It was waited for: its number may be another's from now on.
    reaped: bool,
}

impl Running {
    /// Starts the program: no shell, in a process group of its own (the terminal's keys
    /// do not reach it, and it is killed with all it starts), its errors to nowhere,
    /// and set to be killed by the kernel when this thread ends. The async threads
    /// live as long as the app does, so in effect: when the app does.
    fn start(command: &Command, fed: bool) -> io::Result<Self> {
        use std::os::unix::process::CommandExt;
        let mut start = std::process::Command::new(&command.program);
        start
            .args(&command.args)
            .envs(command.env.iter().map(|(name, value)| (name, value)))
            .stdin(if fed { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: this runs in the new process between fork and exec, where only
        // async-signal-safe calls may be made: these are three system calls, and the
        // error built from the last one does not allocate.
        unsafe {
            start.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) == -1 {
                    return Err(io::Error::last_os_error());
                }
                // The app may have ended before that was in place.
                if libc::getppid() != parent {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
        let child = tokio::process::Command::from(start)
            .kill_on_drop(true)
            .spawn()?;
        let group = child
            .id()
            .map(|id| id as libc::pid_t)
            .ok_or_else(|| io::Error::other("the program has no process number"))?;
        Ok(Self {
            child,
            group,
            reaped: false,
        })
    }

    /// Kills the program and all it started.
    fn kill(&self) {
        if !self.reaped {
            // SAFETY: a plain system call. While the group's leader is not waited for,
            // its number is not given to another process.
            unsafe { libc::kill(-self.group, libc::SIGKILL) };
        }
    }

    /// The program was waited for just now: whatever it started and left behind in
    /// its group goes too. Call it at once after the wait, with nothing awaited in
    /// between.
    fn was_reaped(&mut self) {
        self.reaped = true;
        // SAFETY: a plain system call. While anything is left in the group its number
        // stays taken; if nothing is, the number is free from here on, and that it is
        // given out again within this instant is as good as impossible.
        unsafe { libc::kill(-self.group, libc::SIGKILL) };
    }

    /// Feeds the program `input` and reads what it writes, both at once (a program
    /// that writes before it has read everything must not stop us, nor we it), and
    /// waits for its end. The end of the program counts, not the end of its output:
    /// something it started and left behind can hold the output open for as long as
    /// it likes, and what the program wrote is there all the same.
    async fn talk(&mut self, input: Option<Vec<u8>>, max: usize) -> Result<Output, Failure> {
        let pipe = self.child.stdin.take();
        let mut stdout = self.child.stdout.take().ok_or(Failure::Start)?;
        let mut kept = Vec::new();
        let mut status = None;
        let truncated = {
            let feed = async move {
                if let (Some(mut pipe), Some(bytes)) = (pipe, input) {
                    // A program that does not read all of it is no error by itself:
                    // its exit code says how it went.
                    let _ = pipe.write_all(&bytes).await;
                    let _ = pipe.shutdown().await;
                }
                // Dropped here: the program sees the end of its input.
            };
            let read = read_bounded(&mut stdout, &mut kept, max);
            tokio::pin!(feed);
            tokio::pin!(read);
            let mut fed = false;
            loop {
                tokio::select! {
                    more = &mut read => break more,
                    () = &mut feed, if !fed => fed = true,
                    ended = self.child.wait() => {
                        status = Some(ended);
                        // What it left behind is killed, so the output ends; what is
                        // still in the pipe is read. Something that left the group
                        // and holds the output is not waited for beyond this.
                        self.was_reaped();
                        break tokio::time::timeout(READ_GRACE, &mut read)
                            .await
                            .unwrap_or(false);
                    }
                }
            }
            // What was not fed by now never is: the pipe closes with this block.
        };
        let status = match status {
            Some(status) => status,
            None => {
                if truncated {
                    // It would go on writing to nobody.
                    self.kill();
                }
                let status = self.child.wait().await;
                self.was_reaped();
                status
            }
        };
        let output = Output {
            text: String::from_utf8_lossy(&kept).into_owned(),
            truncated,
        };
        match status {
            _ if truncated => Ok(output),
            Ok(status) if status.success() => Ok(output),
            Ok(status) => Err(Failure::Exit(status.code())),
            Err(_) => Err(Failure::Exit(None)),
        }
    }

    /// Makes sure nothing of the program stays: killed if it still runs, and waited
    /// for, so that it is no zombie.
    async fn end(&mut self) {
        if !self.reaped {
            self.kill();
            let _ = self.child.wait().await;
            self.reaped = true;
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // Dropped before its end (the task was): nothing of it stays.
        self.kill();
    }
}

/// How long the rest of the output is read for once the program has ended.
const READ_GRACE: Duration = Duration::from_millis(500);

/// Reads into `kept` until the end or until more than `max` bytes came: at most `max`
/// bytes are kept; whether there was more. What was read so far stays in `kept` if
/// this is given up.
async fn read_bounded(
    from: &mut tokio::process::ChildStdout,
    kept: &mut Vec<u8>,
    max: usize,
) -> bool {
    let mut buffer = [0u8; 8192];
    loop {
        match from.read(&mut buffer).await {
            Ok(0) | Err(_) => return false,
            Ok(count) => {
                let room = max - kept.len();
                if count > room {
                    kept.extend_from_slice(&buffer[..room]);
                    return true;
                }
                kept.extend_from_slice(&buffer[..count]);
            }
        }
    }
}

/// Why a setting is no command to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadCommand {
    /// It could not be split into words (a quote left open), or is no command at all.
    Unreadable,
    /// The program it names is not there, or cannot be run.
    Missing,
}

/// A command line of the user's own from a setting: split into words as a shell would
/// (no shell runs it), its program looked up on `path` (the `PATH` variable) without
/// running anything, unless it names a file (`~/bin/translate`, with `home` for `~`).
pub fn parse(
    setting: &str,
    path: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<(PathBuf, Vec<String>), BadCommand> {
    let words = shlex::split(setting.trim()).ok_or(BadCommand::Unreadable)?;
    let (first, args) = words.split_first().ok_or(BadCommand::Unreadable)?;
    let program = if first.contains('/') {
        let named = expand_home(first, home);
        crate::audio::is_executable(&named).then_some(named)
    } else {
        crate::audio::find_on_path(first, path)
    }
    .ok_or(BadCommand::Missing)?;
    Ok((program, args.to_vec()))
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
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    const WAIT: Duration = Duration::from_secs(10);
    const LONG: Duration = Duration::from_secs(60);

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

    fn step(command: Command) -> Step {
        Step {
            command,
            time: LONG,
            max_output: MAX_OUTPUT,
        }
    }

    /// The number in the file at `path`, once it is there.
    async fn number(path: &Path) -> libc::pid_t {
        let wait = async {
            loop {
                if let Ok(text) = std::fs::read_to_string(path)
                    && let Ok(number) = text.trim().parse()
                {
                    return number;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(WAIT, wait)
            .await
            .expect("the program started")
    }

    /// Whether a process runs: it is there and not a zombie waiting to be reaped.
    fn alive(pid: libc::pid_t) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.trim_start().chars().next())
                .is_some_and(|state| !matches!(state, 'Z' | 'X'))
        })
    }

    /// Whether no process has this number any more, not even a zombie.
    fn reaped(pid: libc::pid_t) -> bool {
        !Path::new(&format!("/proc/{pid}")).exists()
    }

    async fn stops_running(pid: libc::pid_t) -> bool {
        let wait = async {
            while alive(pid) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(WAIT, wait).await.is_ok()
    }

    /// Starts the steps as a task: its handle, the app's end to cancel with, and where
    /// the answer comes.
    fn start(
        steps: Vec<Step>,
        files: Vec<TempAudio>,
    ) -> (
        tokio::task::JoinHandle<()>,
        oneshot::Sender<()>,
        UnboundedReceiver<Done>,
    ) {
        let (cancel, cancelled) = oneshot::channel();
        let (events, done) = unbounded_channel();
        let request = Request {
            id: 7,
            steps,
            files,
            cancel: cancelled,
        };
        (tokio::spawn(run(request, events)), cancel, done)
    }

    async fn outcome(done: &mut UnboundedReceiver<Done>) -> Result<Output, Failure> {
        let done = tokio::time::timeout(WAIT, done.recv())
            .await
            .expect("an answer in time")
            .expect("the task answers");
        assert_eq!(done.id, 7);
        done.result
    }

    async fn text_of(command: Command) -> Result<String, Failure> {
        let (task, _cancel, mut done) = start(vec![step(command)], Vec::new());
        let result = outcome(&mut done).await;
        task.await.unwrap();
        result.map(|output| {
            assert!(!output.truncated);
            output.text
        })
    }

    #[tokio::test]
    async fn the_text_is_what_the_program_writes_given_its_arguments_input_and_environment() {
        let dir = Dir::new("text");
        let mut command = dir.script(
            "echo",
            "printf '%s|' \"$@\"\nprintf '%s|' \"$MATRIX_RAT_TEST\"\ncat\necho gone >&2",
        );
        command.args.push("a b".into());
        command.args.push("; rm -rf ~".into());
        command.env.push(("MATRIX_RAT_TEST", "to here".to_owned()));
        command.stdin = Some("fed text\n".as_bytes().to_vec());
        // No shell reads the arguments, and what goes to its errors is not text.
        assert_eq!(
            text_of(command).await.unwrap(),
            "a b|; rm -rf ~|to here|fed text\n"
        );
        let pid = number(&dir.0.join("echo.pid")).await;
        assert!(reaped(pid), "the program was waited for");

        // Nothing to read where nothing is fed: the end of input at once.
        let command = dir.script("empty", "cat\necho end");
        assert_eq!(text_of(command).await.unwrap(), "end\n");
    }

    #[tokio::test]
    async fn feeding_and_reading_do_not_wait_for_each_other() {
        let dir = Dir::new("pipes");
        let big = vec![b'x'; 4 << 20];
        // More than a pipe holds in both directions at once.
        let mut command = dir.script("cat", "cat");
        command.stdin = Some(big.clone());
        let (task, _cancel, mut done) = start(
            vec![Step {
                command,
                time: LONG,
                max_output: 8 << 20,
            }],
            Vec::new(),
        );
        let output = outcome(&mut done).await.unwrap();
        task.await.unwrap();
        assert_eq!(output.text.len(), big.len());
        assert!(!output.truncated);

        // A program that reads none of it and ends: its word counts.
        let mut command = dir.script("deaf", "echo done");
        command.stdin = Some(big.clone());
        assert_eq!(text_of(command).await.unwrap(), "done\n");

        // One that writes first and reads later is fed all the same.
        let mut command = dir.script("late", "echo first\nsleep 0.2\nwc -c");
        command.stdin = Some(big);
        let text = text_of(command).await.unwrap();
        assert!(text.starts_with("first\n") && text.trim_end().ends_with("4194304"));
    }

    #[tokio::test]
    async fn output_beyond_the_bound_is_cut_and_the_program_killed() {
        let dir = Dir::new("bound");
        let command = dir.script("yes", "exec yes 0123456789");
        let (task, _cancel, mut done) = start(
            vec![Step {
                command,
                time: LONG,
                max_output: 1000,
            }],
            Vec::new(),
        );
        let output = outcome(&mut done).await.unwrap();
        task.await.unwrap();
        assert!(output.truncated);
        assert_eq!(output.text.len(), 1000);
        assert!(output.text.starts_with("0123456789\n0123"));
        let pid = number(&dir.0.join("yes.pid")).await;
        assert!(reaped(pid));
        // Exactly the bound is not too much.
        let command = dir.script("fits", "printf 12345");
        let (task, _cancel, mut done) = start(
            vec![Step {
                command,
                time: LONG,
                max_output: 5,
            }],
            Vec::new(),
        );
        let output = outcome(&mut done).await.unwrap();
        task.await.unwrap();
        assert_eq!((output.text.as_str(), output.truncated), ("12345", false));
    }

    #[tokio::test]
    async fn a_program_that_fails_or_is_not_there_brings_no_text() {
        let dir = Dir::new("fail");
        let command = dir.script("three", "echo half a text\nexit 3");
        assert_eq!(text_of(command).await, Err(Failure::Exit(Some(3))));
        let command = dir.script("killed", "kill -9 $$");
        assert_eq!(text_of(command).await, Err(Failure::Exit(None)));
        let missing = Command {
            program: dir.0.join("no-such-program"),
            args: Vec::new(),
            env: Vec::new(),
            stdin: Some(b"text".to_vec()),
        };
        assert_eq!(text_of(missing).await, Err(Failure::Start));
        // Nothing to run at all.
        let (task, _cancel, mut done) = start(Vec::new(), Vec::new());
        assert_eq!(outcome(&mut done).await, Err(Failure::Start));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn a_program_that_leaves_something_holding_its_output_still_brings_its_text() {
        let dir = Dir::new("left-open");
        let child = dir.0.join("child");
        // What it starts inherits the output and outlives it by far.
        let body = format!("sleep 600 &\necho $! > '{}'\necho good", child.display());
        let command = dir.script("wrapper", &body);
        let (task, _cancel, mut done) = start(
            vec![Step {
                command,
                time: Duration::from_secs(8),
                max_output: MAX_OUTPUT,
            }],
            Vec::new(),
        );
        let started = number(&child).await;
        assert_eq!(
            outcome(&mut done).await,
            Ok(Output {
                text: "good\n".into(),
                truncated: false
            })
        );
        task.await.unwrap();
        assert!(stops_running(started).await, "what it left behind is gone");
    }

    #[tokio::test]
    async fn the_time_limit_kills_the_program_and_what_it_started() {
        let dir = Dir::new("time");
        let child = dir.0.join("child");
        let body = format!("sleep 60 &\necho $! > '{}'\nwait", child.display());
        let command = dir.script("slow", &body);
        let (task, _cancel, mut done) = start(
            vec![Step {
                command,
                time: Duration::from_millis(400),
                max_output: MAX_OUTPUT,
            }],
            Vec::new(),
        );
        let pid = number(&dir.0.join("slow.pid")).await;
        let started = number(&child).await;
        assert_eq!(outcome(&mut done).await, Err(Failure::TimedOut));
        task.await.unwrap();
        assert!(reaped(pid), "the program was waited for");
        assert!(stops_running(started).await, "what it started is gone too");
    }

    #[tokio::test]
    async fn cancelling_or_the_app_going_away_stops_it() {
        let dir = Dir::new("cancel");
        let sound = TempAudio::create(&dir.0.join("audio"), Some("audio/ogg"), b"OggS").unwrap();
        let path = sound.path().to_owned();
        let (task, cancel, mut done) =
            start(vec![step(dir.script("one", "exec sleep 60"))], vec![sound]);
        let pid = number(&dir.0.join("one.pid")).await;
        assert!(alive(pid) && path.exists());
        cancel.send(()).unwrap();
        assert_eq!(outcome(&mut done).await, Err(Failure::Cancelled));
        task.await.unwrap();
        assert!(reaped(pid));
        assert!(!path.exists(), "the file is removed");

        // The app's end dropped, not a word said.
        let (task, cancel, mut done) =
            start(vec![step(dir.script("two", "exec sleep 60"))], Vec::new());
        let pid = number(&dir.0.join("two.pid")).await;
        drop(cancel);
        assert_eq!(outcome(&mut done).await, Err(Failure::Cancelled));
        task.await.unwrap();
        assert!(reaped(pid));
    }

    #[tokio::test]
    async fn a_dropped_task_leaves_no_program() {
        let dir = Dir::new("abort");
        let sound = TempAudio::create(&dir.0.join("audio"), None, b"x").unwrap();
        let path = sound.path().to_owned();
        let (task, _cancel, _done) =
            start(vec![step(dir.script("left", "exec sleep 60"))], vec![sound]);
        let pid = number(&dir.0.join("left.pid")).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(stops_running(pid).await);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn steps_run_in_order_and_stop_at_the_first_that_fails() {
        let dir = Dir::new("steps");
        let made = dir.0.join("made");
        let first = dir.script(
            "first",
            &format!("echo converted > '{}'\necho not the text", made.display()),
        );
        let second = dir.script("second", &format!("cat '{}'", made.display()));
        let sound = TempAudio::create(&dir.0.join("audio"), None, b"x").unwrap();
        let path = sound.path().to_owned();
        let (task, _cancel, mut done) = start(vec![step(first), step(second)], vec![sound]);
        // The text is the last program's.
        assert_eq!(outcome(&mut done).await.unwrap().text, "converted\n");
        task.await.unwrap();
        assert!(!path.exists(), "the files go when the run has ended");

        let mark = dir.0.join("ran");
        let first = dir.script("bad", "exit 1");
        let second = dir.script("never", &format!("touch '{}'", mark.display()));
        let (task, _cancel, mut done) = start(vec![step(first), step(second)], Vec::new());
        assert_eq!(outcome(&mut done).await, Err(Failure::Exit(Some(1))));
        task.await.unwrap();
        assert!(!mark.exists(), "the second was not started");
    }

    #[test]
    fn a_setting_is_split_like_a_shell_would_and_its_program_looked_up() {
        let bin = Dir::new("parse-bin");
        let home = Dir::new("parse-home");
        // Never run: a program that would leave a mark if it were.
        let mark = bin.0.join("ran");
        bin.file(
            "trans",
            &format!("#!/bin/sh\ntouch {}\n", mark.display()),
            0o755,
        );
        bin.file("notrun", "", 0o644);
        std::fs::create_dir(home.0.join("bin")).unwrap();
        home.file("bin/my tool", "", 0o755);
        let path = Some(bin.0.as_os_str());
        let home_dir = Some(home.0.as_os_str());

        let (program, args) = parse(" trans -b ':{lang}' 'a b' c\\ d ", path, home_dir).unwrap();
        assert_eq!(program, bin.0.join("trans"));
        assert_eq!(args, ["-b", ":{lang}", "a b", "c d"]);
        let (program, args) = parse("'~/bin/my tool' -q", None, home_dir).unwrap();
        assert_eq!(program, home.0.join("bin/my tool"));
        assert_eq!(args, ["-q"]);
        // What looks like shell syntax is a word.
        let (_, args) = parse("trans ; rm -rf ~", path, home_dir).unwrap();
        assert_eq!(args, [";", "rm", "-rf", "~"]);

        assert_eq!(
            parse("'unclosed", path, home_dir),
            Err(BadCommand::Unreadable)
        );
        assert_eq!(parse("  ", path, home_dir), Err(BadCommand::Unreadable));
        assert_eq!(parse("nosuch -x", path, home_dir), Err(BadCommand::Missing));
        assert_eq!(parse("notrun", path, home_dir), Err(BadCommand::Missing));
        assert_eq!(
            parse("/no/such/tool", path, home_dir),
            Err(BadCommand::Missing)
        );
        assert_eq!(parse("trans", None, home_dir), Err(BadCommand::Missing));
        assert!(!mark.exists(), "nothing was run");
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
