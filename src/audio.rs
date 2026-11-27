//! Voice messages and audio, played by a program the system already has.
//!
//! No audio library is linked, which keeps the binary and the Nix package free of ALSA
//! and PipeWire: `mpv`, `ffplay`, `pw-play`, `paplay` or `aplay`, or a command the user
//! names in `audio_player`, is started as a child process on a private copy of the
//! file. With mpv its control socket gives the real position, pause and seek; with the
//! others the position is a clock started with the player. The end is always the
//! player's exit.
//!
//! This file holds the pure parts: the players and their arguments, what the app and a
//! running player tell each other, and how a length, a waveform and a position are
//! shown. [`find`] looks for the player on this machine, [`temp`] keeps the files and
//! sockets, [`ipc`] holds what is said to mpv and read from it, [`play`] runs the
//! player. [`recorder`] chooses the programs a voice message is recorded with.

mod child;
mod find;
mod ipc;
mod play;
pub mod record;
pub mod recorder;
mod temp;

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;

use ipc::Said;

#[cfg(test)]
pub use find::choose;
pub use find::choose_here;
pub(crate) use find::{find_on_path, is_executable};
pub use play::run;
pub use temp::{private_dir, remove_leftovers, socket_path};

/// The largest file played here: the size at which saving asks first. A larger one is
/// saved (`s`) and played elsewhere.
pub const MAX_FILE: u64 = crate::files::LARGE_FILE;
/// A stated length beyond this is not believed.
pub const MAX_DURATION: Duration = Duration::from_secs(24 * 60 * 60);
/// The most loudness values kept of a sender's waveform.
pub const MAX_WAVEFORM: usize = 256;
/// The loudest value of a waveform (MSC3246: 0 to 1024).
pub const MAX_AMPLITUDE: u16 = 1024;
/// The most bars a waveform is drawn with.
pub const WAVE_BARS: usize = 24;
/// How far the seek keys jump, in seconds.
pub const SEEK_SECONDS: i32 = 5;

/// The players `"auto"` knows, in the order it looks for them; `Other` is any other
/// program the user named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Mpv,
    Ffplay,
    PwPlay,
    Paplay,
    Aplay,
    Other,
}

impl Kind {
    pub const KNOWN: [Kind; 5] = [
        Kind::Mpv,
        Kind::Ffplay,
        Kind::PwPlay,
        Kind::Paplay,
        Kind::Aplay,
    ];

    /// The name of its program; empty for `Other`.
    pub fn program(self) -> &'static str {
        match self {
            Kind::Mpv => "mpv",
            Kind::Ffplay => "ffplay",
            Kind::PwPlay => "pw-play",
            Kind::Paplay => "paplay",
            Kind::Aplay => "aplay",
            Kind::Other => "",
        }
    }

    /// What a player found by `"auto"` is started with: no window, nothing on the
    /// terminal, nothing remembered about the file, and an end when the sound ends.
    fn quiet_args(self) -> &'static [&'static str] {
        match self {
            Kind::Mpv => &[
                "--no-video",
                "--force-window=no",
                "--audio-display=no",
                "--no-terminal",
                "--really-quiet",
                "--idle=no",
                "--keep-open=no",
                "--save-position-on-quit=no",
                "--resume-playback=no",
            ],
            Kind::Ffplay => &["-nodisp", "-autoexit", "-loglevel", "quiet"],
            Kind::Aplay => &["-q"],
            Kind::PwPlay | Kind::Paplay | Kind::Other => &[],
        }
    }
}

/// A program that plays a file given as its last argument.
#[derive(Clone, PartialEq, Eq)]
pub struct Player {
    kind: Kind,
    program: PathBuf,
    /// The user's own arguments (from the setting); empty for one `"auto"` found.
    args: Vec<String>,
    /// Named in the setting, not found by `"auto"`: started with the user's arguments
    /// only.
    custom: bool,
}

/// Hand-written: a command of the user's can name their own files.
impl fmt::Debug for Player {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Player({:?}, custom: {})", self.kind, self.custom)
    }
}

impl Player {
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The position, pause and seek come from the player itself (mpv's socket).
    pub fn controllable(&self) -> bool {
        self.kind == Kind::Mpv
    }

    /// The name of its program, for the settings.
    pub fn name(&self) -> String {
        self.program
            .file_name()
            .map(|name| crate::text::sanitize_line(&name.to_string_lossy()))
            .unwrap_or_default()
    }

    /// The program and its arguments, the file last. `socket`: where mpv opens its
    /// control socket (left out for other players). No shell is involved.
    pub fn argv(&self, file: &Path, socket: Option<&Path>) -> (PathBuf, Vec<OsString>) {
        let mut args: Vec<OsString> = if self.custom {
            self.args.iter().map(OsString::from).collect()
        } else {
            self.kind.quiet_args().iter().map(OsString::from).collect()
        };
        if self.kind == Kind::Mpv
            && let Some(socket) = socket
        {
            let mut arg = OsString::from("--input-ipc-server=");
            arg.push(socket);
            args.push(arg);
        }
        args.push(file.as_os_str().to_owned());
        (self.program.clone(), args)
    }

    /// A player made of any program, for tests: started as a custom command would be.
    #[cfg(test)]
    pub fn for_test(program: PathBuf, kind: Kind) -> Self {
        Self {
            kind,
            program,
            args: Vec::new(),
            custom: true,
        }
    }
}

/// Why there is no player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoPlayer {
    /// `"auto"` found none of the players it knows.
    NoneFound,
    /// The setting could not be split into words (a quote left open), or is no
    /// command at all.
    BadCommand,
    /// The program the setting names is not there, or cannot be run.
    Missing,
}

impl NoPlayer {
    /// For a toast.
    pub fn message(self) -> &'static str {
        match self {
            NoPlayer::NoneFound => "No audio player found (mpv, ffplay, pw-play, paplay or aplay)",
            NoPlayer::BadCommand => {
                "The audio_player setting could not be read: is a quote left open?"
            }
            NoPlayer::Missing => "The audio player named in the settings was not found",
        }
    }
}

/// Whether a setting means "look for one": empty, or `auto`.
pub fn is_auto(setting: &str) -> bool {
    let setting = setting.trim();
    setting.is_empty() || setting.eq_ignore_ascii_case("auto")
}

/// A position: `m:ss`, from an hour `h:mm:ss`; whole seconds, cut.
pub fn clock(time: Duration) -> String {
    let secs = time.as_secs();
    let (hours, mins, rest) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if hours > 0 {
        format!("{hours}:{mins:02}:{rest:02}")
    } else {
        format!("{mins}:{rest:02}")
    }
}

/// A length: as [`clock`], rounded to the nearest second.
pub fn length(time: Duration) -> String {
    clock(Duration::from_secs(
        time.as_secs() + u64::from(time.subsec_millis() >= 500),
    ))
}

/// A sender's stated length, if it is believable.
pub fn bounded_duration(time: Duration) -> Option<Duration> {
    (time <= MAX_DURATION).then_some(time)
}

/// The loudest of each of `count` equal stretches of `values` (`values` is longer).
fn peaks<T: Copy + Ord + Default>(values: &[T], count: usize) -> Vec<T> {
    (0..count)
        .map(|bucket| {
            let start = bucket * values.len() / count;
            let end = ((bucket + 1) * values.len() / count).max(start + 1);
            values[start..end].iter().copied().max().unwrap_or_default()
        })
        .collect()
}

/// A sender's waveform, bounded: at most [`MAX_WAVEFORM`] values (the loudest of each
/// equal stretch of a longer one), each at most [`MAX_AMPLITUDE`].
pub fn bounded_waveform(values: &[u64]) -> Vec<u16> {
    let kept = if values.len() > MAX_WAVEFORM {
        peaks(values, MAX_WAVEFORM)
    } else {
        values.to_vec()
    };
    kept.into_iter()
        .map(|value| value.min(u64::from(MAX_AMPLITUDE)) as u16)
        .collect()
}

/// The bars a waveform is drawn with: one per value up to [`WAVE_BARS`], else the
/// loudest of each of that many stretches.
pub fn bars(waveform: &[u16]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let bar =
        |value: u16| BARS[usize::from(value.min(MAX_AMPLITUDE)) * 7 / usize::from(MAX_AMPLITUDE)];
    if waveform.len() <= WAVE_BARS {
        waveform.iter().map(|value| bar(*value)).collect()
    } else {
        peaks(waveform, WAVE_BARS).into_iter().map(bar).collect()
    }
}

/// How many of `cells` stand for the part up to `position` of a sound `length` long:
/// none at the start, all at the end. A cell counts once the sound has passed it.
pub fn played(position: Duration, length: Duration, cells: usize) -> usize {
    if length.is_zero() {
        return if position.is_zero() { 0 } else { cells };
    }
    let part = position.as_secs_f64() / length.as_secs_f64();
    ((part * cells as f64).floor() as usize).min(cells)
}

/// A downloaded sound in a file of its own while it plays: owner-only, gone when this
/// is dropped.
pub struct TempAudio {
    path: PathBuf,
}

/// Hand-written: the path names the user's runtime folder.
impl fmt::Debug for TempAudio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TempAudio")
    }
}

/// Made, written and removed in [`temp`].
impl TempAudio {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The ending of a file of the stated type: one of a fixed few, `audio` for anything
/// else. The players look at what is in the file, not at its name.
fn extension(mime: Option<&str>) -> &'static str {
    let essence = mime
        .and_then(|mime| mime.split(';').next())
        .map(|mime| mime.trim().to_ascii_lowercase());
    match essence.as_deref() {
        Some("audio/ogg" | "audio/x-ogg" | "application/ogg" | "audio/vorbis") => "ogg",
        Some("audio/opus") => "opus",
        Some("audio/mpeg" | "audio/mp3" | "audio/x-mp3" | "audio/mpeg3") => "mp3",
        Some("audio/mp4" | "audio/x-m4a" | "audio/m4a") => "m4a",
        Some("audio/aac" | "audio/x-aac" | "audio/aacp") => "aac",
        Some("audio/flac" | "audio/x-flac") => "flac",
        Some("audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave") => "wav",
        Some("audio/webm") => "webm",
        Some("audio/3gpp") => "3gp",
        Some("audio/amr") => "amr",
        _ => "audio",
    }
}

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
    /// Removed when the last owner drops it (on player exit for downloaded messages;
    /// kept alive by a waiting voice note while it remains in the composer).
    pub file: std::sync::Arc<TempAudio>,
    /// Where mpv opens its control socket: named by the shell that starts the player
    /// (`App` leaves it out). `None` for other players, or when no path would do.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fresh empty folder for one test, removed at its end.
    pub(super) struct Dir(pub PathBuf);

    impl Dir {
        pub fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "matrix-rat-audio-{name}-{}-{}",
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
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn endings_come_from_a_fixed_list() {
        for (mime, ending) in [
            (Some("audio/ogg"), "ogg"),
            (Some("AUDIO/OGG; codecs=opus"), "ogg"),
            (Some("audio/opus"), "opus"),
            (Some("audio/mpeg"), "mp3"),
            (Some("audio/mp4"), "m4a"),
            (Some("audio/x-m4a"), "m4a"),
            (Some("audio/aac"), "aac"),
            (Some("audio/flac"), "flac"),
            (Some("audio/x-wav"), "wav"),
            (Some("audio/webm"), "webm"),
            (Some("audio/ogg/../../x"), "audio"),
            (Some("text/x-shellscript"), "audio"),
            (Some(""), "audio"),
            (None, "audio"),
        ] {
            assert_eq!(extension(mime), ending, "{mime:?}");
        }
    }

    #[test]
    fn lengths_positions_and_waveforms() {
        assert_eq!(clock(Duration::from_millis(5_900)), "0:05");
        assert_eq!(clock(Duration::from_secs(754)), "12:34");
        assert_eq!(clock(Duration::from_secs(3_661)), "1:01:01");
        assert_eq!(length(Duration::from_millis(11_600)), "0:12");
        assert_eq!(length(Duration::from_millis(11_400)), "0:11");
        assert_eq!(
            bounded_duration(Duration::from_secs(90)),
            Some(Duration::from_secs(90))
        );
        assert_eq!(bounded_duration(Duration::from_secs(400_000)), None);

        // Clamped, and bounded in number by the loudest of each stretch.
        assert_eq!(bounded_waveform(&[0, 512, 5000]), [0, 512, 1024]);
        // Every stretch of a long one has a loud value: each kept value is one.
        let long: Vec<u64> = (0..10_000)
            .map(|n| if n % 10 == 0 { 1500 } else { 3 })
            .collect();
        let bounded = bounded_waveform(&long);
        assert_eq!(bounded.len(), MAX_WAVEFORM);
        assert!(bounded.iter().all(|value| *value == MAX_AMPLITUDE));
        assert!(bounded_waveform(&[]).is_empty());

        assert_eq!(bars(&[0, 512, 1024]), "▁▄█");
        assert_eq!(bars(&[]), "");
        let many: Vec<u16> = (0..48).map(|n| (n * 21) % 1024).collect();
        assert_eq!(bars(&many).chars().count(), WAVE_BARS);

        let secs = Duration::from_secs;
        assert_eq!(played(secs(0), secs(12), 24), 0);
        assert_eq!(played(secs(6), secs(12), 24), 12);
        assert_eq!(played(Duration::from_millis(5_999), secs(12), 24), 11);
        assert_eq!(played(secs(12), secs(12), 24), 24);
        assert_eq!(played(secs(30), secs(12), 24), 24);
        assert_eq!(played(secs(1), secs(12), 0), 0);
        assert_eq!(played(secs(0), Duration::ZERO, 10), 0);
        assert_eq!(played(secs(1), Duration::ZERO, 10), 10);
    }
}
