//! Voice messages and audio, played by a program the system already has.
//!
//! No audio library is linked, which keeps the binary and the Nix package free of ALSA
//! and PipeWire: `mpv`, `ffplay`, `pw-play`, `paplay` or `aplay`, or a command the user
//! names in `audio_player`, is started as a child process on a private copy of the
//! file. With mpv its control socket gives the real position, pause and seek; with the
//! others the position is a clock started with the player. The end is always the
//! player's exit.
//!
//! This file holds the pure parts: which player, with which arguments, and how a
//! length, a waveform and a position are shown. [`temp`] keeps the files and sockets,
//! [`ipc`] speaks to mpv, [`play`] runs the player.

mod child;
mod ipc;
mod play;
pub mod record;
pub mod recorder;
mod temp;

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use play::{Control, End, PlayerEvent, PlayerUpdate, Reported, Request, run};
pub use temp::{TempAudio, private_dir, remove_leftovers, socket_path};

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

/// The player for an `audio_player` setting. `"auto"` looks for the known players on
/// `path` (the `PATH` variable), in order, without running anything; anything else is
/// a command line, split into words as a shell would (`mpv --volume=60`,
/// `'/opt/my player/play' -q`), to which the file is added. Its program is looked up
/// on `path` too, unless it names a file (`~/bin/play`, with `home` for `~`).
pub fn choose(
    setting: &str,
    path: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<Player, NoPlayer> {
    if is_auto(setting) {
        return Kind::KNOWN
            .into_iter()
            .find_map(|kind| {
                find_on_path(kind.program(), path).map(|program| Player {
                    kind,
                    program,
                    args: Vec::new(),
                    custom: false,
                })
            })
            .ok_or(NoPlayer::NoneFound);
    }
    let words = shlex::split(setting.trim()).ok_or(NoPlayer::BadCommand)?;
    let (first, args) = words.split_first().ok_or(NoPlayer::BadCommand)?;
    let program = if first.contains('/') {
        let named = match (first.strip_prefix("~/"), home) {
            (Some(rest), Some(home)) => Path::new(home).join(rest),
            _ => PathBuf::from(first),
        };
        is_executable(&named).then_some(named)
    } else {
        find_on_path(first, path)
    }
    .ok_or(NoPlayer::Missing)?;
    // A known player under the user's own arguments is still that player: mpv gets
    // its control socket.
    let kind = Kind::KNOWN
        .into_iter()
        .find(|kind| program.file_name() == Some(OsStr::new(kind.program())))
        .unwrap_or(Kind::Other);
    Ok(Player {
        kind,
        program,
        args: args.to_vec(),
        custom: true,
    })
}

/// [`choose`] with this process's `PATH` and `HOME`.
pub fn choose_here(setting: &str) -> Result<Player, NoPlayer> {
    choose(
        setting,
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// The first executable file called `name` in a folder of `path`. Relative folders
/// (and the empty entry, which means the current folder) are skipped.
fn find_on_path(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
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
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
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

    fn os(text: &str) -> Option<&OsStr> {
        Some(OsStr::new(text))
    }

    #[test]
    fn auto_takes_the_first_known_player_on_the_path_without_running_it() {
        let a = Dir::new("path-a");
        let b = Dir::new("path-b");
        // Never run: a player that would leave a mark if it were.
        let mark = a.0.join("ran");
        let script = format!("#!/bin/sh\ntouch {}\n", mark.display());
        a.file("paplay", &script, 0o755);
        b.file("ffplay", &script, 0o755);
        // Not executable, a folder, and a relative entry: none of them count.
        a.file("mpv", &script, 0o644);
        std::fs::create_dir(b.0.join("pw-play")).unwrap();
        let path = format!("relative:{}::{}", a.0.display(), b.0.display());

        let player = choose("auto", os(&path), None).unwrap();
        assert_eq!(player.kind(), Kind::Ffplay);
        assert_eq!(player.name(), "ffplay");
        assert!(!player.controllable());
        // The order of the players counts, not the order of the folders.
        let player = choose("", os(&format!("{}", a.0.display())), None).unwrap();
        assert_eq!(player.kind(), Kind::Paplay);
        a.file("mpv", &script, 0o755);
        let player = choose(" AUTO ", os(&path), None).unwrap();
        assert_eq!(player.kind(), Kind::Mpv);
        assert!(player.controllable());
        assert!(!mark.exists(), "nothing was run");

        assert_eq!(
            choose("auto", os("/nonexistent"), None),
            Err(NoPlayer::NoneFound)
        );
        assert_eq!(choose("auto", None, None), Err(NoPlayer::NoneFound));
        assert_eq!(choose("auto", os(""), None), Err(NoPlayer::NoneFound));
    }

    #[test]
    fn known_players_get_fixed_quiet_arguments_and_the_file_last() {
        let dir = Dir::new("args");
        let file = Path::new("/run/user/1000/matrix-rat/default-audio/play-0123.ogg");
        let socket = Path::new("/run/user/1000/matrix-rat/default-audio/ipc-0123.sock");
        let argv = |name: &str, socket: Option<&Path>| {
            let found = dir.file(name, "", 0o755);
            let player = choose("auto", Some(dir.0.as_os_str()), None).unwrap();
            assert_eq!(player.argv(file, socket).0, found);
            let args: Vec<String> = player
                .argv(file, socket)
                .1
                .into_iter()
                .map(|arg| arg.into_string().unwrap())
                .collect();
            std::fs::remove_file(found).unwrap();
            args
        };
        let mpv = argv("mpv", Some(socket));
        assert_eq!(
            mpv,
            [
                "--no-video",
                "--force-window=no",
                "--audio-display=no",
                "--no-terminal",
                "--really-quiet",
                "--idle=no",
                "--keep-open=no",
                "--save-position-on-quit=no",
                "--resume-playback=no",
                "--input-ipc-server=/run/user/1000/matrix-rat/default-audio/ipc-0123.sock",
                "/run/user/1000/matrix-rat/default-audio/play-0123.ogg",
            ]
        );
        // Without a socket (its path would be too long), mpv plays all the same.
        assert!(!argv("mpv", None).iter().any(|arg| arg.contains("ipc")));
        assert_eq!(
            argv("ffplay", Some(socket)),
            [
                "-nodisp",
                "-autoexit",
                "-loglevel",
                "quiet",
                "/run/user/1000/matrix-rat/default-audio/play-0123.ogg"
            ]
        );
        assert_eq!(
            argv("pw-play", Some(socket)),
            ["/run/user/1000/matrix-rat/default-audio/play-0123.ogg"]
        );
        assert_eq!(
            argv("paplay", None),
            ["/run/user/1000/matrix-rat/default-audio/play-0123.ogg"]
        );
        assert_eq!(
            argv("aplay", None),
            [
                "-q",
                "/run/user/1000/matrix-rat/default-audio/play-0123.ogg"
            ]
        );
    }

    #[test]
    fn a_command_of_the_users_is_split_like_a_shell_would() {
        let bin = Dir::new("custom-bin");
        let home = Dir::new("custom-home");
        let file = Path::new("/tmp/x/play-1.ogg");
        bin.file("cvlc", "", 0o755);
        bin.file("mpv", "", 0o755);
        std::fs::create_dir(home.0.join("bin")).unwrap();
        home.file("bin/my player", "", 0o755);
        let path = bin.0.display().to_string();
        let words = |player: &Player| -> Vec<String> {
            let (program, args) = player.argv(file, Some(Path::new("/s.sock")));
            std::iter::once(program.into_os_string())
                .chain(args)
                .map(|word| word.into_string().unwrap())
                .collect()
        };

        let player = choose(
            "cvlc --play-and-exit 'a b' c\\ d",
            os(&path),
            Some(home.0.as_os_str()),
        )
        .unwrap();
        assert_eq!(player.kind(), Kind::Other);
        assert!(!player.controllable());
        let cvlc = bin.0.join("cvlc").display().to_string();
        assert_eq!(
            words(&player),
            [
                cvlc.as_str(),
                "--play-and-exit",
                "a b",
                "c d",
                "/tmp/x/play-1.ogg"
            ]
        );
        // Under the user's own words mpv is still mpv: no fixed arguments of ours,
        // but its control socket.
        let player = choose("mpv --volume=60", os(&path), None).unwrap();
        assert!(player.controllable());
        assert_eq!(
            words(&player)[1..],
            [
                "--volume=60",
                "--input-ipc-server=/s.sock",
                "/tmp/x/play-1.ogg"
            ]
        );
        // A path, with `~` for the home folder.
        let player = choose("'~/bin/my player' -q", os(""), Some(home.0.as_os_str())).unwrap();
        assert_eq!(player.name(), "my player");
        assert_eq!(words(&player)[1..], ["-q", "/tmp/x/play-1.ogg"]);

        assert_eq!(
            choose("'unclosed", os(&path), None),
            Err(NoPlayer::BadCommand)
        );
        assert_eq!(
            choose("\"\"", os(&path), None).map(|_| ()),
            Err(NoPlayer::Missing)
        );
        assert_eq!(
            choose("nosuchplayer -x", os(&path), None),
            Err(NoPlayer::Missing)
        );
        assert_eq!(
            choose("/no/such/player", os(&path), None),
            Err(NoPlayer::Missing)
        );
        // A shell is never what runs it: what looks like shell syntax is a word.
        let player = choose("cvlc ; rm -rf ~", os(&path), None).unwrap();
        assert_eq!(words(&player)[1..4], [";", "rm", "-rf"]);
        // The user's command line stays out of debug output.
        assert_eq!(format!("{player:?}"), "Player(Other, custom: true)");
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
