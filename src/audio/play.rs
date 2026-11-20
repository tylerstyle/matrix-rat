//! Running a player: the child process, mpv's control socket, and what the app is
//! told. One task per sound, on the async threads; it ends with the player.
//!
//! The player never outlives the app. It is killed, with everything it started (it is
//! the leader of a process group of its own, see [`Group`]), when the app says stop,
//! when the app's end of the channel goes (quitting, a panic that unwinds), and when
//! this task is dropped (the runtime shutting down); the kernel kills it when the app
//! dies any other way. It is always waited for, so it does not stay a zombie.

use std::fmt;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::{Instant, MissedTickBehavior};

use super::child::Group;
use super::ipc::{self, Lines, Said};
use super::{Player, TempAudio};

/// How long mpv gets to open its control socket, and how often it is tried.
const CONNECT_TIME: Duration = Duration::from_secs(5);
const CONNECT_STEP: Duration = Duration::from_millis(50);
/// The app hears what mpv says at most this often, however much it says.
const REPORT_STEP: Duration = Duration::from_millis(250);
/// A command mpv does not take within this ends the talk with it.
const WRITE_TIME: Duration = Duration::from_secs(1);

/// What the app asks of a running player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Pause, or go on (mpv).
    TogglePause,
    /// Jump by this many seconds, back where negative (mpv).
    Seek(i32),
    Stop,
}

/// How a player ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// By itself, after playing.
    Finished,
    /// With an error, or it could not be started at all.
    Failed,
    /// It was told to stop.
    Stopped,
}

/// What mpv said last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reported {
    pub position: Option<Duration>,
    pub paused: bool,
    /// The real length, which the sender may not have stated.
    pub length: Option<Duration>,
}

impl Reported {
    fn take(&mut self, said: Said) {
        match said {
            Said::Position(at) => self.position = Some(at),
            Said::Length(length) => self.length = Some(length),
            Said::Paused(paused) => self.paused = paused,
            Said::Ended => {
                if self.length.is_some() {
                    self.position = self.length;
                }
            }
        }
    }
}

/// What the app hears of a player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerEvent {
    /// mpv answers on its control socket: the position is its own, pause and seek work.
    Controllable,
    State(Reported),
    /// The last word: the player is gone and waited for, its file removed.
    Ended(End),
}

/// A [`PlayerEvent`] for the sound with this number. Nothing in it names a room, a
/// message or a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerUpdate {
    pub id: u64,
    pub event: PlayerEvent,
}

/// A sound to play.
pub struct Request {
    /// The app's number for it, carried by every answer.
    pub id: u64,
    pub player: Player,
    /// Removed when the player has ended.
    pub file: TempAudio,
    /// Where mpv opens its control socket; `None` for other players, or when no path
    /// would do.
    pub socket: Option<PathBuf>,
    /// Closed by the app (dropped): stop.
    pub control: UnboundedReceiver<Control>,
}

/// Hand-written: the paths name the user's runtime folder.
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("id", &self.id)
            .field("player", &self.player)
            .field("socket", &self.socket.is_some())
            .finish_non_exhaustive()
    }
}

/// Plays the sound of `request` and reports on it to `events` until the player has
/// ended. Run it as a task of its own.
pub async fn run(request: Request, events: UnboundedSender<PlayerUpdate>) {
    let Request {
        id,
        player,
        file,
        socket,
        mut control,
    } = request;
    let send = |event| {
        let _ = events.send(PlayerUpdate { id, event });
    };
    let socket = socket.filter(|_| player.controllable());
    let end = match Running::start(&player, file.path(), socket.as_deref()) {
        Ok(mut running) => running.watch(socket.as_deref(), &mut control, &send).await,
        Err(err) => {
            tracing::debug!(why = %crate::logsafe::kind(&err), "could not start the audio player");
            End::Failed
        }
    };
    tracing::debug!(player = ?player.kind(), ?end, "audio player ended");
    if let Some(socket) = &socket {
        let _ = std::fs::remove_file(socket);
    }
    // The file is gone before the app hears of the end.
    drop(file);
    send(PlayerEvent::Ended(end));
}

/// A started player.
struct Running {
    program: Group,
}

impl Running {
    /// Starts the player on `file`: nothing to read, nowhere to write, and as
    /// [`Group::spawn`] starts a program.
    fn start(player: &Player, file: &Path, socket: Option<&Path>) -> io::Result<Self> {
        let (program, args) = player.argv(file, socket);
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Ok(Self {
            program: Group::spawn(command)?,
        })
    }

    /// Waits for the player's end, and meanwhile for the app's commands, mpv's socket
    /// and what it says.
    async fn watch(
        &mut self,
        socket: Option<&Path>,
        control: &mut UnboundedReceiver<Control>,
        send: &impl Fn(PlayerEvent),
    ) -> End {
        let mut mpv: Option<Mpv> = None;
        // Done with trying to reach the socket (reached, given up, or none to reach).
        let mut tried = socket.is_none();
        let give_up = Instant::now() + CONNECT_TIME;
        let mut connect = tokio::time::interval(CONNECT_STEP);
        connect.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut report = tokio::time::interval(REPORT_STEP);
        report.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut said = Reported::default();
        let mut told = said;
        loop {
            tokio::select! {
                status = self.program.wait() => {
                    self.program.sweep();
                    return match status {
                        Ok(status) if status.success() => End::Finished,
                        _ => End::Failed,
                    };
                }
                message = control.recv() => match message {
                    None | Some(Control::Stop) => {
                        self.program.put_down().await;
                        return End::Stopped;
                    }
                    Some(command) => {
                        let taken = match (&mut mpv, ipc::command(command)) {
                            (Some(mpv), Some(line)) => mpv.write(&line).await,
                            _ => true,
                        };
                        if !taken {
                            mpv = None;
                        }
                    }
                },
                _ = connect.tick(), if !tried => {
                    match socket {
                        Some(path) => {
                            if let Ok(stream) = UnixStream::connect(path).await {
                                tried = true;
                                let _ = std::fs::set_permissions(
                                    path,
                                    std::fs::Permissions::from_mode(0o600),
                                );
                                let mut talk = Mpv::new(stream);
                                if talk.write(ipc::OBSERVE).await {
                                    send(PlayerEvent::Controllable);
                                    mpv = Some(talk);
                                }
                            } else if Instant::now() >= give_up {
                                tried = true;
                            }
                        }
                        None => tried = true,
                    }
                }
                read = read_some(&mut mpv) => match read {
                    // mpv closed it, as it does when it ends.
                    Ok(0) | Err(_) => mpv = None,
                    Ok(count) => {
                        if let Some(talk) = &mut mpv {
                            talk.take(count, &mut said);
                        }
                    }
                },
                _ = report.tick() => {
                    if said != told {
                        send(PlayerEvent::State(said));
                        told = said;
                    }
                }
            }
        }
    }
}

/// The talk with mpv over its socket.
struct Mpv {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    buffer: Vec<u8>,
    lines: Lines,
}

impl Mpv {
    fn new(stream: UnixStream) -> Self {
        let (reader, writer) = stream.into_split();
        Self {
            reader,
            writer,
            buffer: vec![0; ipc::MAX_LINE],
            lines: Lines::default(),
        }
    }

    /// Sends `bytes`; false if mpv did not take them in time.
    async fn write(&mut self, bytes: &[u8]) -> bool {
        matches!(
            tokio::time::timeout(WRITE_TIME, self.writer.write_all(bytes)).await,
            Ok(Ok(()))
        )
    }

    /// Takes in what the last read brought.
    fn take(&mut self, count: usize, said: &mut Reported) {
        let Self { buffer, lines, .. } = self;
        lines.push(&buffer[..count.min(buffer.len())], |line| {
            if let Some(what) = ipc::parse(line) {
                said.take(what);
            }
        });
    }
}

/// Reads from mpv's socket where there is one; else waits for ever.
async fn read_some(mpv: &mut Option<Mpv>) -> io::Result<usize> {
    match mpv {
        Some(talk) => talk.reader.read(&mut talk.buffer).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::Kind;
    use crate::audio::tests::Dir;
    use tokio::io::AsyncBufReadExt;
    use tokio::sync::mpsc::unbounded_channel;

    const WAIT: Duration = Duration::from_secs(10);

    /// A player that is a shell script, read by `sh` (never executed itself, so no
    /// test can find it busy being written): it notes its arguments and then its
    /// process number in `dir`, then does `body`. It plays nothing.
    fn fake(dir: &Dir, body: &str, kind: Kind) -> Player {
        let at = dir.0.display();
        let script = format!(
            "printf '%s\\n' \"$@\" > '{at}/args'\n\
             echo $$ > '{at}/pid.new' && mv '{at}/pid.new' '{at}/pid'\n{body}\n"
        );
        let file = dir.file("player.sh", &script, 0o644);
        let mut player = Player::for_test(PathBuf::from("/bin/sh"), kind);
        player.args = vec![file.display().to_string()];
        player
    }

    fn sound(dir: &Dir) -> TempAudio {
        TempAudio::create(&dir.0.join("audio"), Some("audio/ogg"), b"OggS").unwrap()
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
            .expect("the player started")
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

    async fn next(updates: &mut UnboundedReceiver<PlayerUpdate>) -> PlayerEvent {
        let update = tokio::time::timeout(WAIT, updates.recv())
            .await
            .expect("an answer in time")
            .expect("the task answers");
        assert_eq!(update.id, 7);
        update.event
    }

    /// Starts `player` on a sound of its own as a task; its handle, the app's two ends
    /// and the sound file's path.
    fn start(
        player: Player,
        file: TempAudio,
        socket: Option<PathBuf>,
    ) -> (
        tokio::task::JoinHandle<()>,
        UnboundedSender<Control>,
        UnboundedReceiver<PlayerUpdate>,
        PathBuf,
    ) {
        let path = file.path().to_owned();
        let (control, receiver) = unbounded_channel();
        let (events, updates) = unbounded_channel();
        let request = Request {
            id: 7,
            player,
            file,
            socket,
            control: receiver,
        };
        (tokio::spawn(run(request, events)), control, updates, path)
    }

    #[tokio::test]
    async fn stopping_kills_the_player_and_what_it_started_and_waits_for_it() {
        let dir = Dir::new("play-stop");
        let child = dir.0.join("child");
        let body = format!("sleep 60 &\necho $! > '{}'\nwait", child.display());
        let (task, control, mut updates, path) =
            start(fake(&dir, &body, Kind::Other), sound(&dir), None);
        let pid = number(&dir.0.join("pid")).await;
        let started = number(&child).await;
        assert!(alive(pid) && alive(started));
        assert!(path.exists());

        control.send(Control::Stop).unwrap();
        assert_eq!(next(&mut updates).await, PlayerEvent::Ended(End::Stopped));
        task.await.unwrap();
        assert!(reaped(pid), "the player was waited for");
        assert!(stops_running(started).await, "what it started is gone too");
        assert!(!path.exists(), "the sound file is removed");
    }

    #[tokio::test]
    async fn the_app_going_away_stops_it_too() {
        let dir = Dir::new("play-drop");
        let (task, control, mut updates, path) =
            start(fake(&dir, "exec sleep 60", Kind::Other), sound(&dir), None);
        let pid = number(&dir.0.join("pid")).await;
        assert!(alive(pid));
        drop(control);
        assert_eq!(next(&mut updates).await, PlayerEvent::Ended(End::Stopped));
        task.await.unwrap();
        assert!(reaped(pid));
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn the_end_is_the_players_exit() {
        let dir = Dir::new("play-end");
        // Played to the end.
        let (task, _control, mut updates, path) =
            start(fake(&dir, "exit 0", Kind::Other), sound(&dir), None);
        assert_eq!(next(&mut updates).await, PlayerEvent::Ended(End::Finished));
        task.await.unwrap();
        assert!(!path.exists());
        let pid = number(&dir.0.join("pid")).await;
        assert!(reaped(pid));

        // Could not play it: an error, at once.
        let (task, _control, mut updates, path) =
            start(fake(&dir, "exit 2", Kind::Other), sound(&dir), None);
        assert_eq!(next(&mut updates).await, PlayerEvent::Ended(End::Failed));
        task.await.unwrap();
        assert!(!path.exists());

        // Not there at all.
        let missing = Player::for_test(dir.0.join("no-such-player"), Kind::Other);
        let (task, _control, mut updates, path) = start(missing, sound(&dir), None);
        assert_eq!(next(&mut updates).await, PlayerEvent::Ended(End::Failed));
        task.await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_dropped_task_leaves_no_player() {
        let dir = Dir::new("play-abort");
        let (task, _control, _updates, path) =
            start(fake(&dir, "exec sleep 60", Kind::Other), sound(&dir), None);
        let pid = number(&dir.0.join("pid")).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(stops_running(pid).await);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn the_kernel_kills_the_player_when_the_thread_that_started_it_ends() {
        let dir = Dir::new("play-parent");
        let player = fake(&dir, "exec sleep 60", Kind::Other);
        let file = dir.file("sound.ogg", "OggS", 0o600);
        let runtime = tokio::runtime::Handle::current();
        let pid = std::thread::spawn(move || {
            let _inside = runtime.enter();
            let running = Running::start(&player, &file, None).unwrap();
            let pid = running.program.id();
            // Neither killed nor waited for here: only the kernel can end it now.
            std::mem::forget(running);
            pid
        })
        .join()
        .unwrap();
        let wait = async {
            loop {
                let mut status = 0;
                // SAFETY: a plain system call with a valid pointer.
                match unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } {
                    0 => tokio::time::sleep(Duration::from_millis(10)).await,
                    found => return (found, status),
                }
            }
        };
        let (found, status) = tokio::time::timeout(WAIT, wait).await.expect("it ended");
        assert_eq!(found, pid);
        assert!(libc::WIFSIGNALED(status));
        assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
    }

    /// Line by line, what the app sent to "mpv".
    async fn heard(reader: &mut tokio::io::BufReader<OwnedReadHalf>) -> String {
        let mut line = String::new();
        tokio::time::timeout(WAIT, reader.read_line(&mut line))
            .await
            .expect("a line in time")
            .unwrap();
        line
    }

    /// The next report that is `wanted`, skipping those on the way.
    async fn reported(updates: &mut UnboundedReceiver<PlayerUpdate>, wanted: Reported) {
        loop {
            match next(updates).await {
                PlayerEvent::State(state) if state == wanted => return,
                PlayerEvent::State(_) => {}
                other => panic!("{other:?} instead of {wanted:?}"),
            }
        }
    }

    #[tokio::test]
    async fn mpv_is_heard_and_asked_through_its_socket() {
        let dir = Dir::new("play-ipc");
        let socket = dir.0.join("ipc.sock");
        // This test is "mpv" on the socket; the program is a stand-in that waits.
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (task, control, mut updates, path) = start(
            fake(&dir, "exec sleep 60", Kind::Mpv),
            sound(&dir),
            Some(socket.clone()),
        );
        let pid = number(&dir.0.join("pid")).await;
        // Its control socket, and the file last.
        let args = std::fs::read_to_string(dir.0.join("args")).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert_eq!(
            args,
            [
                format!("--input-ipc-server={}", socket.display()),
                path.display().to_string()
            ]
        );

        let (stream, _) = tokio::time::timeout(WAIT, listener.accept())
            .await
            .unwrap()
            .unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(reader);
        for name in ["time-pos", "pause", "duration", "eof-reached"] {
            let line = heard(&mut reader).await;
            assert!(
                line.contains("observe_property") && line.contains(name),
                "{line}"
            );
        }
        assert_eq!(next(&mut updates).await, PlayerEvent::Controllable);

        let say = |name: &str, data: &str| {
            format!("{{\"event\":\"property-change\",\"name\":\"{name}\",\"data\":{data}}}\n")
        };
        let lines = [
            say("duration", "12.0"),
            say("time-pos", "3.5"),
            say("pause", "false"),
            "{\"request_id\":0,\"error\":\"success\"}\n".to_owned(),
        ]
        .concat();
        writer.write_all(lines.as_bytes()).await.unwrap();
        let playing = Reported {
            position: Some(Duration::from_secs_f64(3.5)),
            paused: false,
            length: Some(Duration::from_secs(12)),
        };
        reported(&mut updates, playing).await;

        // A megabyte without a line's end, and some nonsense, cost nothing; what
        // comes after is heard again.
        writer.write_all(&vec![b'x'; 1 << 20]).await.unwrap();
        writer
            .write_all(format!("\nnot json\n{}", say("pause", "true")).as_bytes())
            .await
            .unwrap();
        let paused = Reported {
            paused: true,
            ..playing
        };
        reported(&mut updates, paused).await;

        // The app's keys become mpv's commands.
        control.send(Control::TogglePause).unwrap();
        assert_eq!(
            heard(&mut reader).await,
            "{\"command\":[\"cycle\",\"pause\"]}\n"
        );
        control.send(Control::Seek(-5)).unwrap();
        assert_eq!(
            heard(&mut reader).await,
            "{\"command\":[\"seek\",-5,\"relative\"]}\n"
        );
        // The end of the file: the bar is full.
        writer
            .write_all(say("eof-reached", "true").as_bytes())
            .await
            .unwrap();
        reported(
            &mut updates,
            Reported {
                position: Some(Duration::from_secs(12)),
                ..paused
            },
        )
        .await;

        control.send(Control::Stop).unwrap();
        assert_eq!(next(&mut updates).await, PlayerEvent::Ended(End::Stopped));
        task.await.unwrap();
        assert!(reaped(pid));
        assert!(!socket.exists(), "the socket is removed");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn without_mpv_there_is_no_socket_to_wait_for() {
        let dir = Dir::new("play-plain");
        let socket = dir.0.join("ipc.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (task, control, mut updates, _path) = start(
            fake(&dir, "exec sleep 60", Kind::PwPlay),
            sound(&dir),
            Some(socket.clone()),
        );
        let pid = number(&dir.0.join("pid")).await;
        let args = std::fs::read_to_string(dir.0.join("args")).unwrap();
        assert!(!args.contains("ipc"), "{args}");
        let accepted = tokio::time::timeout(Duration::from_millis(300), listener.accept()).await;
        assert!(accepted.is_err(), "nothing connects");
        control.send(Control::TogglePause).unwrap();
        control.send(Control::Stop).unwrap();
        assert_eq!(next(&mut updates).await, PlayerEvent::Ended(End::Stopped));
        task.await.unwrap();
        assert!(reaped(pid));
    }
}
