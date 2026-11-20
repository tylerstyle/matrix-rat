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
//! The runner here is small and private to this feature on purpose.

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::text::sanitize;

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

/// The command for a `summary_command` setting: a command line split into words as a
/// shell would (no shell runs it). Its program is looked up on `path` (the `PATH`
/// variable) without running anything, unless it names a file (`~/bin/summarise`,
/// with `home` for `~`).
pub fn choose(
    setting: &str,
    path: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<Command, NoCommand> {
    let setting = setting.trim();
    if setting.is_empty() {
        return Err(NoCommand::NotSet);
    }
    let words = shlex::split(setting).ok_or(NoCommand::BadCommand)?;
    let (first, args) = words.split_first().ok_or(NoCommand::BadCommand)?;
    let program = if first.contains('/') {
        let named = match (first.strip_prefix("~/"), home) {
            (Some(rest), Some(home)) => Path::new(home).join(rest),
            _ => PathBuf::from(first),
        };
        crate::audio::is_executable(&named).then_some(named)
    } else {
        crate::audio::find_on_path(first, path)
    }
    .ok_or(NoCommand::Missing)?;
    Ok(Command {
        program,
        args: args.to_vec(),
    })
}

/// [`choose`] with this process's `PATH` and `HOME`.
pub fn choose_here(setting: &str) -> Result<Command, NoCommand> {
    choose(
        setting,
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
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

/// Runs the command of `request` on its text and tells `done` how it ended. Run it as
/// a task of its own.
pub async fn run(request: Request, done: UnboundedSender<Done>) {
    let Request {
        id,
        command,
        input,
        mut stop,
    } = request;
    let outcome = match Running::start(&command) {
        Ok(mut running) => running.talk(input.as_bytes(), &mut stop).await,
        Err(err) => {
            tracing::debug!(why = %crate::logsafe::kind(&err), "could not start the summary command");
            Outcome::Failed(Failure::Start)
        }
    };
    tracing::debug!(?outcome, "summary command ended");
    let _ = done.send(Done { id, outcome });
}

/// A started command.
struct Running {
    child: tokio::process::Child,
    /// Its process group, numbered as itself.
    group: libc::pid_t,
    /// It was waited for: its number may be another's from now on.
    reaped: bool,
}

impl Running {
    /// Starts the command: no shell, its input and output as pipes to here, nowhere
    /// to write errors, in a process group of its own (the terminal's keys do not
    /// reach it, and it is killed with all it starts), and set to be killed by the
    /// kernel when the thread that started it ends (see `audio::play`).
    fn start(command: &Command) -> io::Result<Self> {
        use std::os::unix::process::CommandExt;
        let mut start = std::process::Command::new(&command.program);
        start
            .args(&command.args)
            .stdin(Stdio::piped())
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
            .ok_or_else(|| io::Error::other("the command has no process number"))?;
        Ok(Self {
            child,
            group,
            reaped: false,
        })
    }

    /// Kills the command and all it started.
    fn kill(&self) {
        // SAFETY: a plain system call. While the group's leader is not waited for,
        // its number is not given to another process; once it is, the group's number
        // stays taken while anything is in it, and is nobody's when it is empty.
        if !self.reaped {
            unsafe { libc::kill(-self.group, libc::SIGKILL) };
        }
    }

    /// Gives the command its text, takes what it says, and waits for its end; all of
    /// it within [`TIME_LIMIT`], and only until `stop` says so. Whatever happens, the
    /// command and what it started are gone and waited for afterwards.
    async fn talk(&mut self, input: &[u8], stop: &mut oneshot::Receiver<()>) -> Outcome {
        let stdin = self.child.stdin.take();
        let stdout = self.child.stdout.take();
        let child = &mut self.child;
        let work = async {
            let write = async {
                if let Some(mut stdin) = stdin {
                    // A command that does not read all of it is its own business.
                    let _ = stdin.write_all(input).await;
                    let _ = stdin.shutdown().await;
                }
            };
            let read = async {
                let mut said = Vec::new();
                if let Some(stdout) = stdout {
                    let _ = stdout
                        .take(OUTPUT_MAX as u64 + 1)
                        .read_to_end(&mut said)
                        .await;
                }
                said
            };
            // What it says decides: once that has ended (or is too much) the rest of
            // the text is not waited for. A command that echoes more than is taken
            // would otherwise stand still on its full pipe, and we on ours.
            tokio::pin!(write, read);
            let mut written = false;
            let said = loop {
                tokio::select! {
                    () = &mut write, if !written => written = true,
                    said = &mut read => break said,
                }
            };
            if said.len() > OUTPUT_MAX {
                return (Outcome::Failed(Failure::TooMuch), false);
            }
            match child.wait().await {
                Ok(status) if status.success() => (
                    clean(&said).map_or(Outcome::Failed(Failure::Empty), Outcome::Summary),
                    true,
                ),
                _ => (Outcome::Failed(Failure::Exit), true),
            }
        };
        let (outcome, reaped) = tokio::select! {
            ended = tokio::time::timeout(TIME_LIMIT, work) => {
                ended.unwrap_or((Outcome::Failed(Failure::TimedOut), false))
            }
            // Told to stop, or the app's end of it is gone.
            _ = stop => (Outcome::Stopped, false),
        };
        if !reaped {
            self.kill();
            let _ = self.child.wait().await;
        }
        self.reaped = true;
        // Whatever it started and left behind in its group, if anything.
        // SAFETY: a plain system call.
        unsafe { libc::kill(-self.group, libc::SIGKILL) };
        outcome
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // Dropped before its end (the task was): nothing of it stays.
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    const WAIT: Duration = Duration::from_secs(10);

    /// A fresh empty folder for one test, removed at its end.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "matrix-rat-catchup-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self, name: &str, text: &str, mode: u32) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let path = self.0.join(name);
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A summary command that is a shell script, read by `sh` (never executed
    /// itself): it notes its process number in `dir`, then does `body`.
    fn fake(dir: &Dir, body: &str) -> Command {
        let at = dir.0.display();
        let script = format!("echo $$ > '{at}/pid.new' && mv '{at}/pid.new' '{at}/pid'\n{body}\n");
        let file = dir.file("summarise.sh", &script, 0o644);
        Command::for_test(PathBuf::from("/bin/sh"), vec![file.display().to_string()])
    }

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
            .expect("the command started")
    }

    /// Whether a process runs: it is there and not a zombie waiting to be reaped.
    fn alive(pid: libc::pid_t) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.trim_start().chars().next())
                .is_some_and(|state| !matches!(state, 'Z' | 'X'))
        })
    }

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

    /// Runs `command` on `input` to its end.
    async fn outcome_of(command: Command, input: &str) -> Outcome {
        let (_stop, stopped) = oneshot::channel();
        let (done, mut answers) = unbounded_channel();
        let request = Request {
            id: 3,
            command,
            input: input.to_owned(),
            stop: stopped,
        };
        run(request, done).await;
        let answer = answers.recv().await.unwrap();
        assert_eq!(answer.id, 3);
        answer.outcome
    }

    #[tokio::test]
    async fn the_command_reads_the_text_and_its_answer_is_the_summary() {
        let dir = Dir::new("ok");
        // Counts what it was given, and says so.
        let command = fake(&dir, "lines=$(wc -l)\necho \"Summary of $lines lines\"");
        let outcome = outcome_of(command, "[10:00] Ann: one\n[10:01] Bob: two\n").await;
        assert_eq!(outcome, Outcome::Summary("Summary of 2 lines".to_owned()));
        let pid = number(&dir.0.join("pid")).await;
        assert!(reaped(pid), "the command was waited for");
    }

    #[tokio::test]
    async fn a_large_text_goes_through_without_a_deadlock() {
        let dir = Dir::new("large");
        // Echoes everything back while it reads: both pipes are busy at once.
        let input = "a line of text\n".repeat(2000);
        let outcome = tokio::time::timeout(WAIT, outcome_of(fake(&dir, "cat"), &input))
            .await
            .expect("no deadlock");
        let Outcome::Summary(text) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(text.starts_with("a line of text\n"));
        assert!(text.ends_with("[The summary was longer; the rest is not shown.]"));
    }

    #[tokio::test]
    async fn a_command_that_echoes_more_than_is_taken_is_stopped_at_once() {
        let dir = Dir::new("echo-flood");
        // Far more than both pipes hold and than a summary may be: `cat` stands
        // still on its output while we still write to it.
        let input = "a line of text that is echoed\n".repeat(10_000);
        assert!(input.len() > 4 * OUTPUT_MAX);
        let outcome = tokio::time::timeout(WAIT, outcome_of(fake(&dir, "cat"), &input))
            .await
            .expect("no deadlock");
        assert_eq!(outcome, Outcome::Failed(Failure::TooMuch));
        let pid = number(&dir.0.join("pid")).await;
        assert!(reaped(pid));
    }

    #[tokio::test]
    async fn an_error_an_empty_answer_and_a_missing_program_are_told_apart() {
        let dir = Dir::new("fail");
        let failed = outcome_of(fake(&dir, "echo 'half a summary'\nexit 3"), "text\n").await;
        assert_eq!(failed, Outcome::Failed(Failure::Exit));
        let silent = outcome_of(fake(&dir, "cat > /dev/null"), "text\n").await;
        assert_eq!(silent, Outcome::Failed(Failure::Empty));
        // It does not read what it is given: that is no failure of ours.
        let deaf = outcome_of(fake(&dir, "echo done"), &"x\n".repeat(100_000)).await;
        assert_eq!(deaf, Outcome::Summary("done".to_owned()));
        let missing = Command::for_test(dir.0.join("no-such-program"), Vec::new());
        assert_eq!(
            outcome_of(missing, "text\n").await,
            Outcome::Failed(Failure::Start)
        );
    }

    #[tokio::test]
    async fn a_command_that_says_too_much_is_stopped() {
        let dir = Dir::new("flood");
        let command = fake(
            &dir,
            "cat > /dev/null\nwhile :; do echo flooding the pipe; done",
        );
        let outcome = tokio::time::timeout(WAIT, outcome_of(command, "text\n"))
            .await
            .expect("it is stopped");
        assert_eq!(outcome, Outcome::Failed(Failure::TooMuch));
        let pid = number(&dir.0.join("pid")).await;
        assert!(reaped(pid));
    }

    #[tokio::test]
    async fn stopping_kills_the_command_and_what_it_started_and_waits_for_it() {
        let dir = Dir::new("stop");
        let child = dir.0.join("child");
        let body = format!("sleep 60 &\necho $! > '{}'\nwait", child.display());
        let (stop, stopped) = oneshot::channel();
        let (done, mut answers) = unbounded_channel();
        let request = Request {
            id: 4,
            command: fake(&dir, &body),
            input: "text\n".to_owned(),
            stop: stopped,
        };
        let task = tokio::spawn(run(request, done));
        let pid = number(&dir.0.join("pid")).await;
        let started = number(&child).await;
        assert!(alive(pid) && alive(started));
        stop.send(()).unwrap();
        let answer = tokio::time::timeout(WAIT, answers.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(answer.outcome, Outcome::Stopped);
        task.await.unwrap();
        assert!(reaped(pid), "the command was waited for");
        assert!(stops_running(started).await, "what it started is gone too");
    }

    #[tokio::test]
    async fn the_app_going_away_and_a_dropped_task_leave_no_command() {
        let dir = Dir::new("drop");
        let (stop, stopped) = oneshot::channel::<()>();
        let (done, mut answers) = unbounded_channel();
        let request = Request {
            id: 5,
            command: fake(&dir, "exec sleep 60"),
            input: String::new(),
            stop: stopped,
        };
        let task = tokio::spawn(run(request, done));
        let pid = number(&dir.0.join("pid")).await;
        assert!(alive(pid));
        drop(stop);
        let answer = tokio::time::timeout(WAIT, answers.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(answer.outcome, Outcome::Stopped);
        task.await.unwrap();
        assert!(reaped(pid));

        let other = Dir::new("abort");
        let (_stop, stopped) = oneshot::channel::<()>();
        let (done, _answers) = unbounded_channel();
        let request = Request {
            id: 6,
            command: fake(&other, "exec sleep 60"),
            input: String::new(),
            stop: stopped,
        };
        let task = tokio::spawn(run(request, done));
        let pid = number(&other.0.join("pid")).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(stops_running(pid).await);
    }

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
    fn the_setting_is_a_command_line_found_without_running_it() {
        let dir = Dir::new("choose");
        let mark = dir.0.join("ran");
        dir.file(
            "summarise",
            &format!("#!/bin/sh\ntouch '{}'\n", mark.display()),
            0o755,
        );
        dir.file("not-a-program", "text", 0o644);
        let path = Some(dir.0.as_os_str());
        let home = Some(dir.0.as_os_str());

        let command = choose("summarise --model 'my model' -q", path, home).unwrap();
        assert_eq!(command.program, dir.0.join("summarise"));
        assert_eq!(command.args, ["--model", "my model", "-q"]);
        assert_eq!(command.name(), "summarise");
        assert_eq!(
            choose("~/summarise", None, home).unwrap().program,
            dir.0.join("summarise")
        );

        assert_eq!(choose("", path, home), Err(NoCommand::NotSet));
        assert_eq!(choose("   ", path, home), Err(NoCommand::NotSet));
        assert_eq!(
            choose("summarise 'open", path, home),
            Err(NoCommand::BadCommand)
        );
        assert_eq!(choose("no-such-thing", path, home), Err(NoCommand::Missing));
        assert_eq!(choose("not-a-program", path, home), Err(NoCommand::Missing));
        assert_eq!(choose("summarise", None, home), Err(NoCommand::Missing));
        // A shell's ways mean nothing: they are words for the program.
        let piped = choose("summarise | tee /tmp/x; rm -rf ~", path, home).unwrap();
        assert_eq!(piped.args, ["|", "tee", "/tmp/x;", "rm", "-rf", "~"]);
        assert!(!mark.exists(), "nothing was run");
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
