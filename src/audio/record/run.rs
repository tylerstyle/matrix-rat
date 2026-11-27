//! Running a recording: both programs, the sound between them, and what the app is
//! told until the end.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::MissedTickBehavior;

use super::{
    Failure, Limits, Meter, RecordControl, RecordEvent, RecordRequest, RecordUpdate, Recording,
    is_ogg_opus,
};
use crate::audio::TempAudio;
use crate::audio::child::Group;
use crate::audio::recorder::Tool;

/// The app hears of the running time and the level this often.
const REPORT_STEP: Duration = Duration::from_millis(250);
/// After it was told to end, the capture has this long to hand over what it still
/// holds; then it is killed.
const DRAIN_TIME: Duration = Duration::from_millis(500);
/// The encoder has this long to take the rest of the sound and write its last page,
/// and then this long to end.
const ENCODE_TIME: Duration = Duration::from_secs(5);
const EXIT_TIME: Duration = Duration::from_secs(1);
/// The most sound that may wait for the encoder: one that takes none is given up on.
const MAX_WAITING: usize = 1 << 20;

/// Makes the recording of `request` and reports on it to `events` until it has ended.
/// Run it as a task of its own.
///
/// While it finishes (after [`RecordControl::Stop`], a few seconds at most) it does
/// not stop for a command, but one to give it up that came meanwhile is honoured
/// before anything is written. Dropping the task kills both programs at any time.
pub async fn run(request: RecordRequest, events: UnboundedSender<RecordUpdate>) {
    let RecordRequest {
        id,
        capture,
        encoder,
        dir,
        limits,
        mut control,
    } = request;
    let send = |event| {
        let _ = events.send(RecordUpdate { id, event });
    };
    let event = match record(&capture, &encoder, limits, &mut control, &send).await {
        // Given up while it finished: nothing is written.
        Ok(_) if given_up(&mut control) => RecordEvent::Cancelled,
        Ok(taken) => keep(taken, limits, dir).await,
        Err(end) => end,
    };
    match &event {
        RecordEvent::Failed(why) => tracing::debug!(?why, "a recording failed"),
        RecordEvent::Cancelled => tracing::debug!("a recording was given up"),
        RecordEvent::Started | RecordEvent::Level { .. } | RecordEvent::Done(_) => {
            tracing::debug!("a recording ended")
        }
    }
    send(event);
}

/// Whether the app has said, or shown by going, that it wants no recording.
fn given_up(control: &mut UnboundedReceiver<RecordControl>) -> bool {
    use tokio::sync::mpsc::error::TryRecvError;
    loop {
        match control.try_recv() {
            Ok(RecordControl::Cancel) | Err(TryRecvError::Disconnected) => return true,
            Ok(RecordControl::Stop) => {}
            Err(TryRecvError::Empty) => return false,
        }
    }
}

/// What a recording brought, before it is checked and written.
struct Taken {
    ogg: Vec<u8>,
    meter: Meter,
    full: bool,
}

/// Why the recording left its running state.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Leave {
    /// The app said stop.
    Stop,
    Cancel,
    /// The longest length was reached.
    Full,
    CaptureEnded,
    EncoderEnded,
    NoSound,
    TooLarge,
}

fn command(tool: &Tool, input: Stdio) -> std::process::Command {
    let mut command = std::process::Command::new(tool.program());
    command
        .args(tool.args())
        .stdin(input)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

/// Runs both programs until the recording ends; `Err` is the last word where there is
/// nothing to keep. Both programs are gone and waited for when this returns.
async fn record(
    capture: &Tool,
    encoder: &Tool,
    limits: Limits,
    control: &mut UnboundedReceiver<RecordControl>,
    send: &impl Fn(RecordEvent),
) -> Result<Taken, RecordEvent> {
    let failed = |why| Err(RecordEvent::Failed(why));
    let mut capture = match Group::spawn(command(capture, Stdio::null())) {
        Ok(capture) => capture,
        Err(err) => {
            tracing::debug!(why = %crate::logsafe::kind(&err), "could not start the capture");
            return failed(Failure::Start);
        }
    };
    let mut encoder = match Group::spawn(command(encoder, Stdio::piped())) {
        Ok(encoder) => encoder,
        Err(err) => {
            tracing::debug!(why = %crate::logsafe::kind(&err), "could not start the encoder");
            capture.put_down().await;
            return failed(Failure::Start);
        }
    };
    let (Some(mut sound), Some(mut feed), Some(mut coded)) = (
        capture.take_stdout(),
        encoder.take_stdin(),
        encoder.take_stdout(),
    ) else {
        capture.put_down().await;
        encoder.put_down().await;
        return failed(Failure::Start);
    };

    let mut meter = Meter::new(limits.longest);
    // Sound on its way to the encoder, and what the encoder made of it so far.
    let mut waiting: Vec<u8> = Vec::new();
    let mut ogg: Vec<u8> = Vec::new();
    let mut heard = vec![0u8; 16 * 1024];
    let mut packed = vec![0u8; 16 * 1024];
    let no_sound = tokio::time::sleep(limits.start);
    tokio::pin!(no_sound);
    let mut report = tokio::time::interval(REPORT_STEP);
    report.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let leave = loop {
        tokio::select! {
            message = control.recv() => match message {
                Some(RecordControl::Stop) => break Leave::Stop,
                Some(RecordControl::Cancel) | None => break Leave::Cancel,
            },
            read = sound.read(&mut heard) => match read {
                Ok(0) | Err(_) => break Leave::CaptureEnded,
                Ok(count) => {
                    let silent_so_far = meter.samples == 0;
                    meter.take(&heard[..count], &mut waiting);
                    if silent_so_far && meter.samples > 0 {
                        send(RecordEvent::Started);
                    }
                    if meter.full() {
                        break Leave::Full;
                    }
                    if waiting.len() > MAX_WAITING {
                        break Leave::EncoderEnded;
                    }
                }
            },
            wrote = feed.write(&waiting), if !waiting.is_empty() => match wrote {
                Ok(count) if count > 0 => {
                    waiting.drain(..count.min(waiting.len()));
                }
                _ => break Leave::EncoderEnded,
            },
            read = coded.read(&mut packed) => match read {
                // It ends only when its input does, which is not yet.
                Ok(0) | Err(_) => break Leave::EncoderEnded,
                Ok(count) => {
                    ogg.extend_from_slice(&packed[..count]);
                    if ogg.len() > limits.bytes {
                        break Leave::TooLarge;
                    }
                }
            },
            () = &mut no_sound, if meter.samples == 0 => break Leave::NoSound,
            _ = report.tick() => {
                if meter.samples > 0 {
                    send(RecordEvent::Level {
                        elapsed: meter.elapsed(),
                        level: meter.take_level(),
                    });
                }
            }
        }
    };

    let nothing = match leave {
        Leave::Cancel => Some(RecordEvent::Cancelled),
        Leave::NoSound => Some(RecordEvent::Failed(Failure::NoSound)),
        Leave::EncoderEnded => Some(RecordEvent::Failed(Failure::Encoder)),
        Leave::TooLarge => Some(RecordEvent::Failed(Failure::TooLarge)),
        Leave::CaptureEnded if meter.samples == 0 => Some(RecordEvent::Failed(Failure::Capture)),
        Leave::Stop | Leave::Full | Leave::CaptureEnded => None,
    };
    if let Some(end) = nothing {
        // Both are killed before either is waited for.
        capture.kill();
        encoder.kill();
        capture.put_down().await;
        encoder.put_down().await;
        return Err(end);
    }

    // The capture ends first. What it still holds is taken, within the bounds that
    // held so far: one that does not end when told, and keeps giving sound, is not
    // listened to for long. A full recording takes no more at all.
    if leave == Leave::Stop {
        capture.signal(libc::SIGTERM);
        let rest = async {
            while !meter.full() && waiting.len() <= MAX_WAITING {
                match sound.read(&mut heard).await {
                    Ok(0) | Err(_) => break,
                    Ok(count) => meter.take(&heard[..count], &mut waiting),
                }
            }
        };
        let _ = tokio::time::timeout(DRAIN_TIME, rest).await;
    }
    drop(sound);
    capture.put_down().await;

    // Too little to keep: the encoder is not asked to make something of it (some
    // fail on next to nothing, which would hide the real reason).
    if meter.elapsed() < limits.shortest {
        encoder.put_down().await;
        return failed(Failure::TooShort);
    }

    // Then the encoder gets the rest and the end of its input, while what it writes
    // is read: it must never wait for us with a full pipe while we wait for it.
    // Either side failing ends both: the other would wait for it in vain.
    let feeding = async {
        if feed.write_all(&waiting).await.is_err() {
            return Err(Failure::Encoder);
        }
        let _ = feed.shutdown().await;
        drop(feed);
        Ok::<(), Failure>(())
    };
    let reading = async {
        loop {
            match coded.read(&mut packed).await {
                Ok(0) => break Ok::<(), Failure>(()),
                Err(_) => break Err(Failure::Encoder),
                Ok(count) if ogg.len() + count > limits.bytes => break Err(Failure::TooLarge),
                Ok(count) => ogg.extend_from_slice(&packed[..count]),
            }
        }
    };
    let finished =
        tokio::time::timeout(ENCODE_TIME, async { tokio::try_join!(feeding, reading) }).await;
    let failure = match finished {
        Ok(Ok(_)) => match tokio::time::timeout(EXIT_TIME, encoder.wait()).await {
            Ok(Ok(status)) if status.success() => None,
            _ => Some(Failure::Encoder),
        },
        Ok(Err(why)) => Some(why),
        Err(_) => Some(Failure::Encoder),
    };
    if let Some(why) = failure {
        encoder.put_down().await;
        return failed(why);
    }
    Ok(Taken {
        ogg,
        meter,
        full: leave == Leave::Full,
    })
}

/// Checks what was recorded and writes it to its file: the last word.
async fn keep(taken: Taken, limits: Limits, dir: PathBuf) -> RecordEvent {
    let Taken { ogg, meter, full } = taken;
    let duration = meter.elapsed();
    if duration < limits.shortest {
        return RecordEvent::Failed(Failure::TooShort);
    }
    if !is_ogg_opus(&ogg) {
        return RecordEvent::Failed(Failure::NotOpus);
    }
    let size = ogg.len() as u64;
    let written = tokio::task::spawn_blocking(move || TempAudio::voice(&dir, &ogg)).await;
    match written {
        Ok(Ok(file)) => RecordEvent::Done(Recording {
            file,
            duration,
            waveform: meter.waveform(),
            size,
            silent: meter.loudest == 0,
            full,
        }),
        Ok(Err(err)) => {
            tracing::debug!(why = %crate::logsafe::kind(&err), "could not write a recording");
            RecordEvent::Failed(Failure::NotKept)
        }
        Err(_) => RecordEvent::Failed(Failure::NotKept),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::record::tests::HEAD;
    use crate::audio::tests::Dir;
    use std::ffi::OsString;
    use std::path::Path;
    use tokio::sync::mpsc::unbounded_channel;

    const WAIT: Duration = Duration::from_secs(10);
    /// One second of the stand-in capture's sound: `y` and a line's end, which as a
    /// sample is 2681 of 32767, 48000 times.
    const SECOND: usize = 96_000;

    /// A stand-in program that is a shell script, read by `sh`: it notes its process
    /// number in `dir` as `<name>.pid`, then does `body`.
    fn tool(dir: &Dir, name: &str, body: &str) -> Tool {
        let at = dir.0.display();
        let script = format!(
            "echo $$ > '{at}/{name}.new' && mv '{at}/{name}.new' '{at}/{name}.pid'\n{body}\n"
        );
        let file = dir.file(&format!("{name}.sh"), &script, 0o644);
        Tool::new(PathBuf::from("/bin/sh"), vec![OsString::from(file)])
    }

    /// A capture that gives `seconds` of sound and then does `after`.
    fn capture(dir: &Dir, seconds: usize, after: &str) -> Tool {
        let bytes = seconds * SECOND;
        tool(dir, "capture", &format!("yes | head -c {bytes}\n{after}"))
    }

    /// An encoder that writes [`HEAD`] and then what it is given, unchanged.
    fn encoder(dir: &Dir) -> Tool {
        tool(dir, "encoder", &format!("printf '{HEAD}'\nexec cat"))
    }

    struct Session {
        task: tokio::task::JoinHandle<()>,
        control: UnboundedSender<RecordControl>,
        updates: UnboundedReceiver<RecordUpdate>,
        folder: PathBuf,
    }

    fn start(dir: &Dir, capture: Tool, encoder: Tool, limits: Limits) -> Session {
        let folder = dir.0.join("audio");
        let (control, receiver) = unbounded_channel();
        let (events, updates) = unbounded_channel();
        let request = RecordRequest {
            id: 7,
            capture,
            encoder,
            dir: folder.clone(),
            limits,
            control: receiver,
        };
        Session {
            task: tokio::spawn(run(request, events)),
            control,
            updates,
            folder,
        }
    }

    impl Session {
        async fn next(&mut self) -> RecordEvent {
            let update = tokio::time::timeout(WAIT, self.updates.recv())
                .await
                .expect("an answer in time")
                .expect("the task answers");
            assert_eq!(update.id, 7);
            update.event
        }

        /// The level of the next report.
        async fn level(&mut self) -> u16 {
            loop {
                match self.next().await {
                    RecordEvent::Level { level, .. } => return level,
                    RecordEvent::Started => {}
                    other => panic!("{other:?} while it should run"),
                }
            }
        }

        /// Waits until the recording says it runs at least this long.
        async fn runs_for(&mut self, wanted: Duration) {
            loop {
                match self.next().await {
                    RecordEvent::Level { elapsed, .. } if elapsed >= wanted => return,
                    RecordEvent::Level { .. } | RecordEvent::Started => {}
                    other => panic!("{other:?} while it should run"),
                }
            }
        }

        /// The last word, skipping the reports before it; the task has ended.
        async fn end(mut self) -> (RecordEvent, PathBuf) {
            let last = loop {
                match self.next().await {
                    RecordEvent::Level { .. } | RecordEvent::Started => {}
                    last => break last,
                }
            };
            self.task.await.unwrap();
            (last, self.folder)
        }
    }

    /// The number in the file at `path`, if it is there now.
    fn noted(path: &Path) -> Option<libc::pid_t> {
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    /// The number in the file at `path`, once it is there.
    async fn number(path: &Path) -> libc::pid_t {
        let wait = async {
            loop {
                if let Some(number) = noted(path) {
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

    async fn both(dir: &Dir) -> (libc::pid_t, libc::pid_t) {
        (
            number(&dir.0.join("capture.pid")).await,
            number(&dir.0.join("encoder.pid")).await,
        )
    }

    /// The files in the folder of finished recordings (none if it is not there).
    fn kept(folder: &Path) -> Vec<String> {
        std::fs::read_dir(folder)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn done(event: RecordEvent) -> Recording {
        match event {
            RecordEvent::Done(recording) => recording,
            other => panic!("{other:?} instead of a recording"),
        }
    }

    fn failure(event: RecordEvent) -> Failure {
        match event {
            RecordEvent::Failed(why) => why,
            other => panic!("{other:?} instead of a failure"),
        }
    }

    #[tokio::test]
    async fn stopping_keeps_the_sound_with_its_length_and_waveform() {
        let dir = Dir::new("record-stop");
        let mut session = start(
            &dir,
            capture(&dir, 1, "exec sleep 60"),
            encoder(&dir),
            Limits::default(),
        );
        let (first, second) = both(&dir).await;
        assert!(matches!(session.next().await, RecordEvent::Started));
        // The stand-in's sound is 2681 of 32767 loud, all the time.
        assert_eq!(session.level().await, 83);
        session.runs_for(Duration::from_secs(1)).await;
        assert!(
            kept(&session.folder).is_empty(),
            "nothing is on disk while it runs"
        );

        session.control.send(RecordControl::Stop).unwrap();
        let (last, folder) = session.end().await;
        let recording = done(last);
        assert!(reaped(first) && reaped(second), "both were waited for");
        assert_eq!(recording.duration, Duration::from_secs(1));
        assert_eq!(recording.waveform, vec![1024; 20]);
        assert!(!recording.silent && !recording.full);
        // The file holds what the encoder wrote, which here is all the sound.
        let bytes = std::fs::read(recording.file.path()).unwrap();
        assert_eq!(bytes.len(), HEAD.len() + SECOND);
        assert_eq!(recording.size, bytes.len() as u64);
        assert!(bytes.starts_with(HEAD.as_bytes()));
        assert!(bytes[HEAD.len()..].chunks(2).all(|sample| sample == b"y\n"));
        let names = kept(&folder);
        assert_eq!(names.len(), 1);
        assert!(
            names[0].starts_with("voice-") && names[0].ends_with(".ogg"),
            "{names:?}"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(recording.file.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // It goes with the recording.
        drop(recording);
        assert!(kept(&folder).is_empty());
    }

    #[tokio::test]
    async fn giving_it_up_leaves_no_program_and_no_file() {
        let dir = Dir::new("record-cancel");
        let mut session = start(
            &dir,
            capture(&dir, 1, "exec sleep 60"),
            encoder(&dir),
            Limits::default(),
        );
        let (first, second) = both(&dir).await;
        session.runs_for(Duration::from_secs(1)).await;
        session.control.send(RecordControl::Cancel).unwrap();
        let (last, folder) = session.end().await;
        assert!(matches!(last, RecordEvent::Cancelled));
        assert!(reaped(first) && reaped(second));
        assert!(kept(&folder).is_empty());
    }

    #[tokio::test]
    async fn the_app_going_away_gives_it_up_too() {
        let dir = Dir::new("record-drop");
        let session = start(
            &dir,
            capture(&dir, 1, "exec sleep 60"),
            encoder(&dir),
            Limits::default(),
        );
        let (first, second) = both(&dir).await;
        let Session {
            task,
            control,
            mut updates,
            folder,
        } = session;
        drop(control);
        let last = loop {
            let update = tokio::time::timeout(WAIT, updates.recv())
                .await
                .unwrap()
                .unwrap();
            match update.event {
                RecordEvent::Level { .. } | RecordEvent::Started => {}
                last => break last,
            }
        };
        assert!(matches!(last, RecordEvent::Cancelled));
        task.await.unwrap();
        assert!(reaped(first) && reaped(second));
        assert!(kept(&folder).is_empty());
    }

    #[tokio::test]
    async fn a_dropped_task_leaves_no_program() {
        let dir = Dir::new("record-abort");
        let session = start(
            &dir,
            capture(&dir, 1, "exec sleep 60"),
            encoder(&dir),
            Limits::default(),
        );
        let (first, second) = both(&dir).await;
        session.task.abort();
        assert!(session.task.await.unwrap_err().is_cancelled());
        let gone = async {
            while alive(first) || alive(second) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(WAIT, gone)
            .await
            .expect("both are gone");
        assert!(kept(&session.folder).is_empty());
    }

    #[tokio::test]
    async fn a_capture_that_ends_by_itself_after_some_sound_is_kept() {
        let dir = Dir::new("record-ended");
        let session = start(
            &dir,
            capture(&dir, 2, "exit 0"),
            encoder(&dir),
            Limits::default(),
        );
        let (last, _folder) = session.end().await;
        let recording = done(last);
        assert_eq!(recording.duration, Duration::from_secs(2));
        assert_eq!(recording.size, (HEAD.len() + 2 * SECOND) as u64);
    }

    #[tokio::test]
    async fn a_capture_that_gives_no_sound_fails() {
        // It ends at once: no microphone, or arguments it does not take.
        let dir = Dir::new("record-none");
        let session = start(
            &dir,
            tool(&dir, "capture", "exit 1"),
            encoder(&dir),
            Limits::default(),
        );
        let (last, folder) = session.end().await;
        assert_eq!(failure(last), Failure::Capture);
        assert!(kept(&folder).is_empty());

        // It runs and stays silent: the wait for sound has an end.
        let dir = Dir::new("record-mute");
        let limits = Limits {
            start: Duration::from_millis(200),
            ..Limits::default()
        };
        let session = start(
            &dir,
            tool(&dir, "capture", "exec sleep 60"),
            encoder(&dir),
            limits,
        );
        // Not waited for before the end: the wait for sound can be over (and both put
        // down) before a shell has noted its number, on a busy machine. One that was
        // noted is gone; one that was not was put down before its script ran.
        let (last, _folder) = session.end().await;
        assert_eq!(failure(last), Failure::NoSound);
        for name in ["capture.pid", "encoder.pid"] {
            if let Some(pid) = noted(&dir.0.join(name)) {
                assert!(reaped(pid), "{name}: the program does not stay behind");
            }
        }

        // It is not there at all.
        let dir = Dir::new("record-missing");
        let missing = Tool::new(dir.0.join("no-such-program"), Vec::new());
        let session = start(&dir, missing, encoder(&dir), Limits::default());
        let (last, _folder) = session.end().await;
        assert_eq!(failure(last), Failure::Start);
    }

    #[tokio::test]
    async fn an_encoder_that_ends_early_or_badly_fails_the_recording() {
        // It ends before the recording does. It waits for the capture to note its number
        // first: ended at once, it gets the capture put down before the capture's shell
        // has written it (on a busy machine), and the test would wait for it in vain.
        let dir = Dir::new("record-encoder-gone");
        let capture_pid = dir.0.join("capture.pid");
        let body = format!(
            "while [ ! -s '{}' ]; do sleep 0.01; done\nexit 3",
            capture_pid.display()
        );
        let session = start(
            &dir,
            capture(&dir, 1, "exec sleep 60"),
            tool(&dir, "encoder", &body),
            Limits::default(),
        );
        let (started, _) = both(&dir).await;
        let (last, folder) = session.end().await;
        assert_eq!(failure(last), Failure::Encoder);
        assert!(reaped(started), "the capture does not stay behind");
        assert!(kept(&folder).is_empty());

        // It takes everything and then says it failed.
        let dir = Dir::new("record-encoder-error");
        let body = format!("printf '{HEAD}'\ncat\nexit 4");
        let session = start(
            &dir,
            capture(&dir, 1, "exit 0"),
            tool(&dir, "encoder", &body),
            Limits::default(),
        );
        let (last, folder) = session.end().await;
        assert_eq!(failure(last), Failure::Encoder);
        assert!(kept(&folder).is_empty());

        // It is not there at all: the capture that was started is put down again.
        let dir = Dir::new("record-encoder-missing");
        let missing = Tool::new(dir.0.join("no-such-program"), Vec::new());
        let session = start(
            &dir,
            capture(&dir, 1, "exec sleep 60"),
            missing,
            Limits::default(),
        );
        let (last, folder) = session.end().await;
        assert_eq!(failure(last), Failure::Start);
        assert!(kept(&folder).is_empty());
    }

    /// Waits until the process whose number is in `path` runs no more.
    async fn gone(path: &Path) {
        let pid = number(path).await;
        let wait = async {
            while alive(pid) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(WAIT, wait).await.expect("it is gone");
    }

    #[tokio::test]
    async fn what_a_failing_encoder_started_does_not_stay_behind() {
        let dir = Dir::new("record-encoder-child");
        let at = dir.0.display();
        // It lets go of everything, starts something that stays, and fails.
        let body = format!(
            "printf '{HEAD}'\ncat\nsleep 60 >/dev/null 2>&1 &\necho $! > '{at}/left.pid'\nexit 4"
        );
        let session = start(
            &dir,
            capture(&dir, 1, "exit 0"),
            tool(&dir, "encoder", &body),
            Limits::default(),
        );
        let (last, _folder) = session.end().await;
        assert_eq!(failure(last), Failure::Encoder);
        gone(&dir.0.join("left.pid")).await;
    }

    #[tokio::test]
    async fn the_rest_goes_to_a_slow_encoder_while_its_answer_is_read() {
        // Two seconds of sound wait, far more than a pipe holds, for an encoder that
        // starts late and then answers as much.
        let slow = |dir: &Dir| {
            tool(
                dir,
                "encoder",
                &format!("printf '{HEAD}'\nsleep 1\nexec cat"),
            )
        };
        let dir = Dir::new("record-slow");
        let session = start(
            &dir,
            capture(&dir, 2, "exit 0"),
            slow(&dir),
            Limits::default(),
        );
        let (last, _folder) = session.end().await;
        assert_eq!(done(last).size, (HEAD.len() + 2 * SECOND) as u64);

        // Too much of an answer ends it at once, not after the encoder's time is up.
        let dir = Dir::new("record-slow-large");
        let limits = Limits {
            bytes: 100_000,
            ..Limits::default()
        };
        let session = start(&dir, capture(&dir, 2, "exit 0"), slow(&dir), limits);
        let began = crate::time::Instant::now();
        let (last, folder) = session.end().await;
        assert_eq!(failure(last), Failure::TooLarge);
        assert!(
            began.elapsed() < Duration::from_secs(4),
            "{:?}",
            began.elapsed()
        );
        assert!(kept(&folder).is_empty());
    }

    #[tokio::test]
    async fn stopping_too_early_says_so_whatever_the_encoder_makes_of_it() {
        let dir = Dir::new("record-early");
        let limits = Limits {
            shortest: Duration::from_secs(5),
            ..Limits::default()
        };
        let mut session = start(
            &dir,
            capture(&dir, 1, "exec sleep 60"),
            tool(&dir, "encoder", "cat > /dev/null\nexit 4"),
            limits,
        );
        let (first, second) = both(&dir).await;
        session.runs_for(Duration::from_secs(1)).await;
        session.control.send(RecordControl::Stop).unwrap();
        let (last, folder) = session.end().await;
        assert_eq!(failure(last), Failure::TooShort);
        assert!(reaped(first) && reaped(second));
        assert!(kept(&folder).is_empty());
    }

    #[tokio::test]
    async fn giving_it_up_while_it_finishes_writes_nothing() {
        let dir = Dir::new("record-late-cancel");
        let mut session = start(
            &dir,
            capture(&dir, 1, "exec sleep 60"),
            encoder(&dir),
            Limits::default(),
        );
        session.runs_for(Duration::from_secs(1)).await;
        // Both are waiting before the task looks: the first is taken, the second is
        // found when the recording is ready to be written.
        session.control.send(RecordControl::Stop).unwrap();
        session.control.send(RecordControl::Cancel).unwrap();
        let (last, folder) = session.end().await;
        assert!(matches!(last, RecordEvent::Cancelled));
        assert!(kept(&folder).is_empty());
    }

    #[tokio::test]
    async fn a_recording_that_cannot_be_written_fails() {
        let dir = Dir::new("record-not-kept");
        // Where the folder for recordings should be, there is a file.
        dir.file("audio", "in the way", 0o600);
        let session = start(
            &dir,
            capture(&dir, 1, "exit 0"),
            encoder(&dir),
            Limits::default(),
        );
        let (last, _folder) = session.end().await;
        assert_eq!(failure(last), Failure::NotKept);
    }

    #[tokio::test]
    async fn what_is_not_ogg_opus_is_not_kept() {
        let dir = Dir::new("record-not-opus");
        // It hands back the raw sound.
        let session = start(
            &dir,
            capture(&dir, 1, "exit 0"),
            tool(&dir, "encoder", "exec cat"),
            Limits::default(),
        );
        let (last, folder) = session.end().await;
        assert_eq!(failure(last), Failure::NotOpus);
        assert!(kept(&folder).is_empty());
    }

    #[tokio::test]
    async fn the_longest_length_stops_a_recording_and_keeps_it() {
        let dir = Dir::new("record-full");
        let limits = Limits {
            longest: Duration::from_millis(500),
            ..Limits::default()
        };
        // A capture that never ends, not even when told, and gives sound far faster
        // than a microphone.
        let body = "trap '' TERM\nexec yes";
        let session = start(&dir, tool(&dir, "capture", body), encoder(&dir), limits);
        let (first, second) = both(&dir).await;
        let (last, _folder) = session.end().await;
        let recording = done(last);
        assert!(recording.full);
        assert_eq!(recording.duration, Duration::from_millis(500));
        assert_eq!(recording.size, (HEAD.len() + SECOND / 2) as u64);
        assert_eq!(recording.waveform, vec![1024; 10]);
        assert!(reaped(first) && reaped(second));
    }

    #[tokio::test]
    async fn a_recording_too_short_or_too_large_is_not_kept() {
        let dir = Dir::new("record-short");
        let limits = Limits {
            shortest: Duration::from_secs(5),
            ..Limits::default()
        };
        let session = start(&dir, capture(&dir, 1, "exit 0"), encoder(&dir), limits);
        let (last, folder) = session.end().await;
        assert_eq!(failure(last), Failure::TooShort);
        assert!(kept(&folder).is_empty());

        let dir = Dir::new("record-large");
        let limits = Limits {
            bytes: 1000,
            ..Limits::default()
        };
        let session = start(
            &dir,
            capture(&dir, 1, "exec sleep 60"),
            encoder(&dir),
            limits,
        );
        let (first, second) = both(&dir).await;
        let (last, folder) = session.end().await;
        assert_eq!(failure(last), Failure::TooLarge);
        assert!(reaped(first) && reaped(second));
        assert!(kept(&folder).is_empty());
    }

    #[tokio::test]
    async fn a_capture_that_does_not_end_when_told_is_killed() {
        let dir = Dir::new("record-stubborn");
        // It ignores the polite signal, and so does what it starts.
        let body = "trap '' TERM\nyes | head -c 96000\nsleep 60 &\nwait";
        let mut session = start(
            &dir,
            tool(&dir, "capture", body),
            encoder(&dir),
            Limits::default(),
        );
        let (first, second) = both(&dir).await;
        session.runs_for(Duration::from_secs(1)).await;
        session.control.send(RecordControl::Stop).unwrap();
        let (last, _folder) = session.end().await;
        let recording = done(last);
        assert_eq!(recording.duration, Duration::from_secs(1));
        assert!(reaped(first) && reaped(second));
    }

    #[tokio::test]
    async fn silence_is_kept_and_said_to_be_silence() {
        let dir = Dir::new("record-silent");
        let body = "head -c 96000 /dev/zero\nexit 0";
        let session = start(
            &dir,
            tool(&dir, "capture", body),
            encoder(&dir),
            Limits::default(),
        );
        let (last, _folder) = session.end().await;
        let recording = done(last);
        assert!(recording.silent);
        assert_eq!(recording.waveform, vec![0; 20]);
    }

    #[test]
    fn what_is_printed_for_debugging_names_no_file_and_no_shape() {
        let dir = Dir::new("record-debug");
        let folder = dir.0.join("audio");
        let recording = Recording {
            file: TempAudio::voice(&folder, HEAD.as_bytes()).unwrap(),
            duration: Duration::from_secs(3),
            waveform: vec![777; 5],
            size: 36,
            silent: false,
            full: false,
        };
        let (_control, receiver) = unbounded_channel();
        let request = RecordRequest {
            id: 7,
            capture: Tool::new(dir.0.join("secret-capture"), vec!["secret-arg".into()]),
            encoder: Tool::new(dir.0.join("secret-encoder"), Vec::new()),
            dir: folder,
            limits: Limits::default(),
            control: receiver,
        };
        let shown = format!(
            "{:?} {request:?} {:?}",
            RecordEvent::Done(recording),
            request.capture
        );
        let folder = dir.0.to_string_lossy();
        for private in [folder.as_ref(), "secret", "777", "voice-"] {
            assert!(!shown.contains(private), "{shown}");
        }
    }
}
