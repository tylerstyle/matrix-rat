//! Which programs record a voice message: one that captures the microphone and one
//! that encodes what it hears.
//!
//! As with the player, no audio library is linked. A capture program writes raw sound
//! (signed 16-bit little-endian, mono, 48 kHz) to its standard output until it is told
//! to end (SIGTERM); an encoder reads exactly that on its standard input and writes
//! Ogg/Opus to its standard output. This file only chooses the two and says with which
//! arguments they are started. Nothing is run here: programs are looked up on `PATH`,
//! and a sound server counts as there when its socket is (a `stat`, no more).

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

use super::find::named_program;
use super::{find_on_path, is_auto};

/// A program with its arguments, started without a shell.
#[derive(Clone, PartialEq, Eq)]
pub struct Tool {
    program: PathBuf,
    args: Vec<OsString>,
}

impl Tool {
    pub fn new(program: PathBuf, args: Vec<OsString>) -> Self {
        Self { program, args }
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    /// The name of its program, for the settings.
    fn name(&self) -> String {
        self.program()
            .file_name()
            .map(|name| crate::text::sanitize_line(&name.to_string_lossy()))
            .unwrap_or_default()
    }

    /// A program found by `"auto"`, with our own fixed arguments.
    fn known(program: PathBuf, args: &[&str]) -> Self {
        Self::new(program, args.iter().map(OsString::from).collect())
    }
}

/// Hand-written: a command of the user's can name their files.
impl fmt::Debug for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Tool")
    }
}

/// The capture programs `"auto"` knows, in the order it looks for them; `Own` is a
/// command the user named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    Parec,
    /// The same program as `parec` under the name some systems only have.
    Parecord,
    PwRecord,
    /// ffmpeg reading from a PulseAudio server.
    FfmpegPulse,
    Arecord,
    /// ffmpeg reading from ALSA.
    FfmpegAlsa,
    Own,
}

/// The sound server a capture program records from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Server {
    PipeWire,
    Pulse,
    Alsa,
}

impl Capture {
    /// The PulseAudio programs come first: their options are old and the same
    /// everywhere, and PipeWire answers on PulseAudio's socket on practically every
    /// desktop that runs it, while `pw-record` writes raw sound to its output only
    /// since PipeWire 1.4 or so and ends with a usage error before that.
    pub const KNOWN: [Capture; 6] = [
        Capture::Parec,
        Capture::Parecord,
        Capture::PwRecord,
        Capture::FfmpegPulse,
        Capture::Arecord,
        Capture::FfmpegAlsa,
    ];

    /// The name of its program; empty for `Own`.
    pub fn program(self) -> &'static str {
        match self {
            Capture::PwRecord => "pw-record",
            Capture::Parec => "parec",
            Capture::Parecord => "parecord",
            Capture::FfmpegPulse | Capture::FfmpegAlsa => "ffmpeg",
            Capture::Arecord => "arecord",
            Capture::Own => "",
        }
    }

    /// What a capture program found by `"auto"` is started with: raw signed 16-bit
    /// mono sound at 48 kHz on its standard output, from the default microphone, and
    /// nothing said on the terminal.
    fn args(self) -> &'static [&'static str] {
        match self {
            // `s16` is the machine's byte order, which is little-endian wherever this
            // is built for; pw-record has no name for one byte order alone.
            Capture::PwRecord => &["--raw", "--format=s16", "--rate=48000", "--channels=1", "-"],
            Capture::Parec => &[
                "--raw",
                "--format=s16le",
                "--rate=48000",
                "--channels=1",
                "--latency-msec=50",
            ],
            // Under this name the program would write a file with a header: it is
            // told both to record and to write raw sound.
            Capture::Parecord => &[
                "--record",
                "--raw",
                "--format=s16le",
                "--rate=48000",
                "--channels=1",
                "--latency-msec=50",
            ],
            Capture::FfmpegPulse => &[
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "pulse",
                "-i",
                "default",
                "-ac",
                "1",
                "-ar",
                "48000",
                "-f",
                "s16le",
                "pipe:1",
            ],
            Capture::Arecord => &["-q", "-t", "raw", "-f", "S16_LE", "-r", "48000", "-c", "1"],
            Capture::FfmpegAlsa => &[
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "alsa",
                "-i",
                "default",
                "-ac",
                "1",
                "-ar",
                "48000",
                "-f",
                "s16le",
                "pipe:1",
            ],
            Capture::Own => &[],
        }
    }

    fn server(self) -> Option<Server> {
        match self {
            Capture::PwRecord => Some(Server::PipeWire),
            Capture::Parec | Capture::Parecord | Capture::FfmpegPulse => Some(Server::Pulse),
            Capture::Arecord | Capture::FfmpegAlsa => Some(Server::Alsa),
            Capture::Own => None,
        }
    }
}

/// The encoders, in the order they are looked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoder {
    Opusenc,
    Ffmpeg,
}

impl Encoder {
    pub const KNOWN: [Encoder; 2] = [Encoder::Opusenc, Encoder::Ffmpeg];

    pub fn program(self) -> &'static str {
        match self {
            Encoder::Opusenc => "opusenc",
            Encoder::Ffmpeg => "ffmpeg",
        }
    }

    /// Raw sound as the capture programs write it on the standard input, Ogg/Opus at
    /// a bitrate fit for speech on the standard output.
    fn args(self) -> &'static [&'static str] {
        match self {
            Encoder::Opusenc => &[
                "--quiet",
                "--raw",
                "--raw-bits",
                "16",
                "--raw-rate",
                "48000",
                "--raw-chan",
                "1",
                "--bitrate",
                "24",
                "-",
                "-",
            ],
            // Without the bitexact flags the file would name the version of ffmpeg
            // and of its libraries to the room.
            Encoder::Ffmpeg => &[
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "s16le",
                "-ar",
                "48000",
                "-ac",
                "1",
                "-i",
                "pipe:0",
                "-map_metadata",
                "-1",
                "-fflags",
                "+bitexact",
                "-flags:a",
                "+bitexact",
                "-c:a",
                "libopus",
                "-application",
                "voip",
                "-b:a",
                "24k",
                "-f",
                "ogg",
                "pipe:1",
            ],
        }
    }
}

/// What of the machine's sound was seen, without running or opening anything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Seen {
    /// PipeWire's socket is in the runtime folder.
    pub pipewire: bool,
    /// A PulseAudio server (or PipeWire in its place): its socket is in the runtime
    /// folder, or `PULSE_SERVER` names one.
    pub pulse: bool,
    /// `PULSE_SERVER` names a server: the sound is not this machine's own (WSLg, a
    /// remote server), so its devices cannot be looked for here.
    pub pulse_named: bool,
    /// ALSA lists a device that records.
    pub alsa_capture: bool,
}

impl Seen {
    /// From what the caller read: `runtime_dir` is `XDG_RUNTIME_DIR`, `pulse_server`
    /// is `PULSE_SERVER`, `asound_pcm` the text of `/proc/asound/pcm`. The sockets
    /// are only looked at (`stat`).
    pub fn look(
        runtime_dir: Option<&OsStr>,
        pulse_server: Option<&OsStr>,
        asound_pcm: Option<&str>,
    ) -> Self {
        let runtime = runtime_dir.map(Path::new).filter(|dir| dir.is_absolute());
        let there = |rest: &str| runtime.is_some_and(|dir| exists(&dir.join(rest)));
        let pulse_named = pulse_server.is_some_and(|server| !server.is_empty());
        Self {
            pipewire: there("pipewire-0"),
            pulse: pulse_named || there("pulse/native"),
            pulse_named,
            alsa_capture: asound_pcm.is_some_and(lists_capture),
        }
    }

    /// [`Seen::look`] at this process's environment and this machine.
    pub fn here() -> Self {
        Self::look(
            std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
            std::env::var_os("PULSE_SERVER").as_deref(),
            // A few short lines the kernel makes up; no device is opened by reading it.
            std::fs::read_to_string("/proc/asound/pcm").ok().as_deref(),
        )
    }

    /// A microphone is plausible: a device that records, or a sound server elsewhere
    /// whose devices cannot be seen from here. Whether it really hears something
    /// shows only when recording.
    pub fn microphone(&self) -> bool {
        self.alsa_capture || self.pulse_named
    }

    /// Whether a program that records from `server` would record what the user
    /// means. With a server named in `PULSE_SERVER` that is this server alone.
    fn offers(&self, server: Server) -> bool {
        match server {
            Server::Pulse => self.pulse,
            _ if self.pulse_named => false,
            Server::PipeWire => self.pipewire,
            Server::Alsa => self.alsa_capture,
        }
    }
}

fn exists(path: &Path) -> bool {
    std::fs::metadata(path).is_ok()
}

/// Whether `/proc/asound/pcm` lists a device that records. Its lines read
/// `00-00: ALC256 Analog : ALC256 Analog : playback 1 : capture 1`: the device, its
/// name twice, then how many streams it has each way.
fn lists_capture(pcm: &str) -> bool {
    pcm.lines().any(|line| {
        line.split(" : ").skip(2).any(|field| {
            field
                .trim()
                .strip_prefix("capture ")
                .is_some_and(|count| count.trim().parse::<u32>().is_ok_and(|count| count > 0))
        })
    })
}

/// The two programs a recording is made with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorder {
    pub capture: Tool,
    pub encoder: Tool,
    capture_kind: Capture,
    encoder_kind: Encoder,
}

impl Recorder {
    /// Which capture program; never the user's command itself.
    pub fn capture_kind(&self) -> Capture {
        self.capture_kind
    }

    pub fn encoder_kind(&self) -> Encoder {
        self.encoder_kind
    }

    /// The names of the two programs, for the settings: `pw-record + opusenc`.
    pub fn names(&self) -> String {
        format!("{} + {}", self.capture.name(), self.encoder.name())
    }

    /// A recorder made of any two programs, for tests: started as they are given.
    #[cfg(test)]
    pub fn for_test(capture: Tool, encoder: Tool) -> Self {
        Self {
            capture,
            encoder,
            capture_kind: Capture::Own,
            encoder_kind: Encoder::Opusenc,
        }
    }
}

/// Where the programs and the sound are looked for: plain values, so that tests name
/// their own. The default is a machine with nothing on it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Env {
    /// The `PATH` variable.
    pub path: Option<OsString>,
    /// The `HOME` variable, for a `~/` in a command of the user's.
    pub home: Option<OsString>,
    pub seen: Seen,
    /// A session over SSH: the microphone would be the far machine's.
    pub ssh: bool,
}

/// Hand-written: the two variables name the user's folders.
impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Env")
            .field("seen", &self.seen)
            .field("ssh", &self.ssh)
            .finish_non_exhaustive()
    }
}

impl Env {
    /// This process's environment and this machine.
    pub fn here() -> Self {
        Self {
            path: std::env::var_os("PATH"),
            home: std::env::var_os("HOME"),
            seen: Seen::here(),
            ssh: crate::portal::in_ssh_session(),
        }
    }
}

/// Why there is no recorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoRecorder {
    /// `"auto"` in a session over SSH: what would record is the far machine's
    /// microphone, not the one in front of the user.
    OverSsh,
    /// `"auto"` saw no microphone: no ALSA device that records, and no sound server
    /// named in `PULSE_SERVER`.
    NoMicrophone,
    /// `"auto"` found none of the capture programs it knows with its sound server.
    NoCapture,
    /// Neither `opusenc` nor `ffmpeg` is there.
    NoEncoder,
    /// The setting could not be split into words (a quote left open), or is no
    /// command at all.
    BadCommand,
    /// The program the setting names is not there, or cannot be run.
    Missing,
}

impl NoRecorder {
    /// For a toast.
    pub fn message(self) -> &'static str {
        match self {
            NoRecorder::OverSsh => {
                "Over SSH the far machine's microphone would record: not offered (audio_recorder can name a recorder)"
            }
            NoRecorder::NoMicrophone => {
                "No microphone was found; if there is one, name a recorder in the audio_recorder setting"
            }
            NoRecorder::NoCapture => {
                "No recorder found (parec, parecord, pw-record, ffmpeg or arecord)"
            }
            NoRecorder::NoEncoder => "No Opus encoder found (opusenc or ffmpeg)",
            NoRecorder::BadCommand => {
                "The audio_recorder setting could not be read: is a quote left open?"
            }
            NoRecorder::Missing => "The recorder named in the settings was not found",
        }
    }

    /// In a few words, for a list: why recording is greyed there.
    pub fn short(self) -> &'static str {
        match self {
            NoRecorder::OverSsh => "not over SSH",
            NoRecorder::NoMicrophone => "no microphone found",
            NoRecorder::NoCapture => "no recorder found",
            NoRecorder::NoEncoder => "no Opus encoder found",
            NoRecorder::BadCommand => "audio_recorder could not be read",
            NoRecorder::Missing => "audio_recorder not found",
        }
    }
}

/// The recorder for an `audio_recorder` setting.
///
/// `"auto"` takes the first known capture program that is on `PATH` and whose sound
/// server was seen, and wants a microphone to be plausible; over SSH it takes none,
/// as nothing is played there either. Where `PULSE_SERVER` names a server, only the
/// programs that speak to it are taken: the sound is that server's, not this
/// machine's. Anything else is a capture command of the user's own, split into words
/// as a shell would and started as it is: it must write raw signed 16-bit
/// little-endian mono sound at 48 kHz to its standard output and end on SIGTERM. Its
/// program is looked up as the player's is; whether a microphone is there, and
/// whose it is, is then the user's business. Either way an encoder must be on `PATH`.
pub fn choose(setting: &str, env: &Env) -> Result<Recorder, NoRecorder> {
    let (path, seen) = (env.path.as_deref(), &env.seen);
    let (capture_kind, capture) = if is_auto(setting) {
        if env.ssh {
            return Err(NoRecorder::OverSsh);
        }
        if !seen.microphone() {
            return Err(NoRecorder::NoMicrophone);
        }
        Capture::KNOWN
            .into_iter()
            .filter(|kind| kind.server().is_some_and(|server| seen.offers(server)))
            .find_map(|kind| {
                find_on_path(kind.program(), path)
                    .map(|program| (kind, Tool::known(program, kind.args())))
            })
            .ok_or(NoRecorder::NoCapture)?
    } else {
        let words = shlex::split(setting.trim()).ok_or(NoRecorder::BadCommand)?;
        let (first, args) = words.split_first().ok_or(NoRecorder::BadCommand)?;
        let program = named_program(first, path, env.home.as_deref()).ok_or(NoRecorder::Missing)?;
        let args = args.iter().map(OsString::from).collect();
        (Capture::Own, Tool::new(program, args))
    };
    let (encoder_kind, encoder) = Encoder::KNOWN
        .into_iter()
        .find_map(|kind| {
            find_on_path(kind.program(), path)
                .map(|program| (kind, Tool::known(program, kind.args())))
        })
        .ok_or(NoRecorder::NoEncoder)?;
    Ok(Recorder {
        capture,
        encoder,
        capture_kind,
        encoder_kind,
    })
}

/// [`choose`] with this process's environment and what is seen of this machine.
pub fn choose_here(setting: &str) -> Result<Recorder, NoRecorder> {
    choose(setting, &Env::here())
}

#[cfg(test)]
mod tests {
    use super::super::tests::Dir;
    use super::*;

    const PCM: &str = "00-00: ALC256 Analog : ALC256 Analog : playback 1 : capture 1\n\
                       00-03: HDMI 0 : HDMI 0 : playback 1\n";
    const PCM_NO_CAPTURE: &str = "00-03: HDMI 0 : HDMI 0 : playback 1\n\
                                  00-07: capture 1 : capture 2 : playback 1\n";

    fn os(text: &str) -> Option<&OsStr> {
        Some(OsStr::new(text))
    }

    /// Everything there: both sound servers and a device that records.
    fn all() -> Seen {
        Seen {
            pipewire: true,
            pulse: true,
            pulse_named: false,
            alsa_capture: true,
        }
    }

    /// A machine with the programs of `bin` and what was `seen`.
    fn env(bin: &Dir, seen: Seen) -> Env {
        Env {
            path: Some(bin.0.clone().into_os_string()),
            home: None,
            seen,
            ssh: false,
        }
    }

    fn words(tool: &Tool) -> Vec<String> {
        tool.args()
            .iter()
            .map(|arg| arg.clone().into_string().unwrap())
            .collect()
    }

    #[test]
    fn the_sound_servers_are_seen_by_their_sockets_and_the_microphone_in_the_list_of_devices() {
        let runtime = Dir::new("seen");
        let dir = Some(runtime.0.as_os_str());
        assert_eq!(Seen::look(dir, None, None), Seen::default());
        assert_eq!(Seen::look(None, None, None), Seen::default());

        // A plain file stands for the socket: it is looked at, never opened.
        runtime.file("pipewire-0", "", 0o600);
        let seen = Seen::look(dir, None, Some(PCM));
        assert!(seen.pipewire && !seen.pulse && seen.alsa_capture);
        assert!(seen.microphone());

        std::fs::create_dir(runtime.0.join("pulse")).unwrap();
        runtime.file("pulse/native", "", 0o600);
        let seen = Seen::look(dir, os(""), Some(PCM_NO_CAPTURE));
        assert!(seen.pulse && !seen.pulse_named);
        // Only playback devices, and "capture" in a name is no device that records.
        assert!(!seen.alsa_capture && !seen.microphone());

        // A server named in the environment: its devices are not this machine's.
        let seen = Seen::look(None, os("unix:/mnt/wslg/PulseServer"), None);
        assert!(seen.pulse && seen.pulse_named && !seen.pipewire);
        assert!(seen.microphone());
        // A relative runtime folder is not believed.
        assert_eq!(Seen::look(os("."), None, None), Seen::default());
        assert!(!lists_capture(""));
    }

    #[test]
    fn auto_takes_the_first_capture_program_whose_server_is_seen_without_running_it() {
        let bin = Dir::new("rec-auto");
        // Never run: a program that would leave a mark if it were.
        let mark = bin.0.join("ran");
        let script = format!("#!/bin/sh\ntouch {}\n", mark.display());
        for name in [
            "pw-record",
            "parec",
            "parecord",
            "ffmpeg",
            "arecord",
            "opusenc",
        ] {
            bin.file(name, &script, 0o755);
        }
        let kind = |seen: Seen| choose("auto", &env(&bin, seen)).unwrap().capture_kind();

        // PulseAudio's programs before PipeWire's own, which older systems have in a
        // form that cannot write to its output.
        assert_eq!(kind(all()), Capture::Parec);
        // Without PulseAudio's socket its programs do not count, though they are there.
        let no_pulse = Seen {
            pulse: false,
            ..all()
        };
        assert_eq!(kind(no_pulse), Capture::PwRecord);
        std::fs::remove_file(bin.0.join("parec")).unwrap();
        assert_eq!(kind(all()), Capture::Parecord);
        std::fs::remove_file(bin.0.join("parecord")).unwrap();
        assert_eq!(kind(all()), Capture::PwRecord);
        let no_pipewire = Seen {
            pipewire: false,
            ..all()
        };
        assert_eq!(kind(no_pipewire), Capture::FfmpegPulse);
        let alsa_only = Seen {
            alsa_capture: true,
            ..Seen::default()
        };
        assert_eq!(kind(alsa_only), Capture::Arecord);
        std::fs::remove_file(bin.0.join("arecord")).unwrap();
        assert_eq!(kind(alsa_only), Capture::FfmpegAlsa);
        // The encoder: opusenc before ffmpeg.
        let recorder = choose("", &env(&bin, all())).unwrap();
        assert_eq!(recorder.encoder_kind(), Encoder::Opusenc);
        assert_eq!(recorder.names(), "pw-record + opusenc");
        assert_eq!(recorder.capture.program(), bin.0.join("pw-record"));
        std::fs::remove_file(bin.0.join("opusenc")).unwrap();
        let recorder = choose(" AUTO ", &env(&bin, all())).unwrap();
        assert_eq!(recorder.encoder_kind(), Encoder::Ffmpeg);
        assert_eq!(recorder.encoder.program(), bin.0.join("ffmpeg"));
        assert!(!mark.exists(), "nothing was run");
    }

    #[test]
    fn a_server_named_in_pulse_server_is_the_only_one_recorded_from() {
        let runtime = Dir::new("rec-named-runtime");
        let bin = Dir::new("rec-named-bin");
        runtime.file("pipewire-0", "", 0o600);
        for name in ["pw-record", "arecord", "opusenc"] {
            bin.file(name, "", 0o755);
        }
        // PipeWire's socket and a device that records are here, but the user's sound
        // is the named server's.
        let seen = Seen::look(
            Some(runtime.0.as_os_str()),
            os("tcp:desk.example:4713"),
            Some(PCM),
        );
        assert!(seen.pipewire && seen.pulse_named && seen.alsa_capture);
        assert_eq!(choose("auto", &env(&bin, seen)), Err(NoRecorder::NoCapture));
        bin.file("ffmpeg", "", 0o755);
        let recorder = choose("auto", &env(&bin, seen)).unwrap();
        assert_eq!(recorder.capture_kind(), Capture::FfmpegPulse);
        bin.file("parecord", "", 0o755);
        let recorder = choose("auto", &env(&bin, seen)).unwrap();
        assert_eq!(recorder.capture_kind(), Capture::Parecord);
        // Without the variable the same machine records with its own PipeWire.
        let seen = Seen::look(Some(runtime.0.as_os_str()), None, Some(PCM));
        let recorder = choose("auto", &env(&bin, seen)).unwrap();
        assert_eq!(recorder.capture_kind(), Capture::PwRecord);
    }

    #[test]
    fn over_ssh_auto_records_nothing_and_a_command_of_the_users_still_does() {
        let bin = Dir::new("rec-ssh");
        for name in ["parec", "arecord", "opusenc"] {
            bin.file(name, "", 0o755);
        }
        let mut far = env(&bin, all());
        assert!(choose("auto", &far).is_ok());
        // The far machine has everything, a microphone among it: all the more reason.
        far.ssh = true;
        assert_eq!(choose("auto", &far), Err(NoRecorder::OverSsh));
        assert_eq!(choose("", &far), Err(NoRecorder::OverSsh));
        let recorder = choose("arecord -q -t raw -f S16_LE -r 48000 -c 1", &far).unwrap();
        assert_eq!(recorder.capture_kind(), Capture::Own);
        assert!(NoRecorder::OverSsh.message().contains("audio_recorder"));
        assert_eq!(NoRecorder::OverSsh.short(), "not over SSH");
    }

    #[test]
    fn what_is_missing_is_told_apart() {
        let bin = Dir::new("rec-missing");
        let why = |setting: &str, seen: Seen| choose(setting, &env(&bin, seen)).unwrap_err();

        // No microphone comes first: on a machine without one nothing else matters.
        assert_eq!(why("auto", Seen::default()), NoRecorder::NoMicrophone);
        let servers_only = Seen {
            alsa_capture: false,
            ..all()
        };
        bin.file("pw-record", "", 0o755);
        bin.file("opusenc", "", 0o755);
        assert_eq!(why("auto", servers_only), NoRecorder::NoMicrophone);
        std::fs::remove_file(bin.0.join("pw-record")).unwrap();
        assert_eq!(why("auto", all()), NoRecorder::NoCapture);
        // Not executable does not count.
        bin.file("arecord", "", 0o644);
        assert_eq!(why("auto", all()), NoRecorder::NoCapture);
        // A program whose server is not seen is no recorder.
        bin.file("pw-record", "", 0o755);
        let alsa_only = Seen {
            alsa_capture: true,
            ..Seen::default()
        };
        assert_eq!(why("auto", alsa_only), NoRecorder::NoCapture);
        std::fs::remove_file(bin.0.join("opusenc")).unwrap();
        assert_eq!(why("auto", all()), NoRecorder::NoEncoder);
        let nowhere = Env {
            seen: all(),
            ..Env::default()
        };
        assert_eq!(choose("auto", &nowhere), Err(NoRecorder::NoCapture));
        assert_eq!(
            choose("auto", &Env::default()),
            Err(NoRecorder::NoMicrophone)
        );

        assert_eq!(why("'unclosed", all()), NoRecorder::BadCommand);
        assert_eq!(why("\"\"", all()), NoRecorder::Missing);
        assert_eq!(why("nosuchrecorder -x", all()), NoRecorder::Missing);
        assert_eq!(why("/no/such/recorder", all()), NoRecorder::Missing);
        // A command of the user's still needs an encoder.
        assert_eq!(why("pw-record --raw -", all()), NoRecorder::NoEncoder);

        // What is said tells them apart too, and says what the user can do.
        assert!(
            NoRecorder::NoMicrophone
                .message()
                .contains("audio_recorder")
        );
        assert!(NoRecorder::NoCapture.message().contains("parec"));
        assert!(NoRecorder::NoEncoder.message().contains("opusenc"));
        assert!(NoRecorder::BadCommand.message().contains("quote"));
        assert!(NoRecorder::Missing.message().contains("not found"));
        // A command that cannot be read is not said to be missing.
        assert_eq!(
            NoRecorder::BadCommand.short(),
            "audio_recorder could not be read"
        );
        assert_eq!(NoRecorder::Missing.short(), "audio_recorder not found");
        let all_reasons = [
            NoRecorder::OverSsh,
            NoRecorder::NoMicrophone,
            NoRecorder::NoCapture,
            NoRecorder::NoEncoder,
            NoRecorder::BadCommand,
            NoRecorder::Missing,
        ];
        for (n, one) in all_reasons.iter().enumerate() {
            for other in &all_reasons[n + 1..] {
                assert_ne!(one.message(), other.message());
                assert_ne!(one.short(), other.short());
            }
        }
    }

    #[test]
    fn every_capture_line_asks_for_the_sound_the_encoder_is_told_to_expect() {
        for kind in Capture::KNOWN {
            let args = kind.args();
            let line = args.join(" ");
            let has = |word: &str| args.contains(&word);
            // 48 kHz.
            assert!(
                has("--rate=48000") || line.contains("-r 48000") || line.contains("-ar 48000"),
                "{kind:?}: {line}"
            );
            // One channel.
            assert!(
                has("--channels=1") || line.contains("-c 1") || line.contains("-ac 1"),
                "{kind:?}: {line}"
            );
            // Signed 16-bit little-endian, raw: no header before the sound. Only
            // pw-record has no name for the byte order (it is the machine's).
            let format = has("--format=s16le")
                || line.contains("-f S16_LE")
                || line.contains("-f s16le pipe:1")
                || (kind == Capture::PwRecord && has("--format=s16"));
            assert!(format, "{kind:?}: {line}");
            let raw = has("--raw") || line.contains("-t raw") || line.contains("-f s16le pipe:1");
            assert!(raw, "{kind:?}: {line}");
            // To its standard output: no file is named. The one name there is, is
            // the default device ffmpeg wants.
            assert!(!line.contains('/') && !line.contains(".wav"), "{line}");
            // The server it is offered for is the one it records from.
            match kind.server().unwrap() {
                Server::Pulse if kind.program() == "ffmpeg" => assert!(line.contains("-f pulse")),
                Server::Alsa if kind.program() == "ffmpeg" => assert!(line.contains("-f alsa")),
                Server::Pulse => assert!(kind.program().starts_with("parec")),
                Server::PipeWire => assert_eq!(kind.program(), "pw-record"),
                Server::Alsa => assert_eq!(kind.program(), "arecord"),
            }
        }
        for kind in Encoder::KNOWN {
            let line = kind.args().join(" ");
            assert!(line.contains("48000"), "{kind:?}: {line}");
            assert!(
                line.contains("--raw-chan 1") || line.contains("-ac 1"),
                "{line}"
            );
            assert!(
                line.contains("--raw-bits 16") || line.contains("-f s16le"),
                "{line}"
            );
            // Speech at 24 kbit/s, read from the input and written to the output.
            assert!(
                line.contains("--bitrate 24") || line.contains("-b:a 24k"),
                "{line}"
            );
            assert!(
                line.ends_with(" - -") || (line.contains("pipe:0") && line.ends_with("pipe:1")),
                "{line}"
            );
        }
        // No shell is involved, and nothing in our own words would need one.
        let ours = Capture::KNOWN
            .into_iter()
            .flat_map(|kind| kind.args())
            .chain(Encoder::KNOWN.into_iter().flat_map(|kind| kind.args()));
        for word in ours {
            assert!(
                !word.is_empty()
                    && !word.contains(|c: char| " \t\n;|&$`<>(){}*?~'\"\\".contains(c)),
                "{word:?}"
            );
        }
    }

    /// A pin: the lines as they were checked against the programs' documentation.
    #[test]
    fn known_programs_get_fixed_arguments() {
        let args = |kind: Capture| kind.args().join(" ");
        assert_eq!(
            args(Capture::PwRecord),
            "--raw --format=s16 --rate=48000 --channels=1 -"
        );
        assert_eq!(
            args(Capture::Parec),
            "--raw --format=s16le --rate=48000 --channels=1 --latency-msec=50"
        );
        assert_eq!(
            args(Capture::Parecord),
            "--record --raw --format=s16le --rate=48000 --channels=1 --latency-msec=50"
        );
        assert_eq!(
            args(Capture::FfmpegPulse),
            "-hide_banner -loglevel error -nostdin -f pulse -i default -ac 1 -ar 48000 \
             -f s16le pipe:1"
        );
        assert_eq!(args(Capture::Arecord), "-q -t raw -f S16_LE -r 48000 -c 1");
        assert_eq!(
            args(Capture::FfmpegAlsa),
            "-hide_banner -loglevel error -nostdin -f alsa -i default -ac 1 -ar 48000 \
             -f s16le pipe:1"
        );
        assert!(args(Capture::Own).is_empty());
        assert_eq!(
            Encoder::Opusenc.args().join(" "),
            "--quiet --raw --raw-bits 16 --raw-rate 48000 --raw-chan 1 --bitrate 24 - -"
        );
        assert_eq!(
            Encoder::Ffmpeg.args().join(" "),
            "-hide_banner -loglevel error -nostdin -f s16le -ar 48000 -ac 1 -i pipe:0 \
             -map_metadata -1 -fflags +bitexact -flags:a +bitexact -c:a libopus \
             -application voip -b:a 24k -f ogg pipe:1"
        );

        // What is chosen carries exactly these.
        let bin = Dir::new("rec-args");
        bin.file("arecord", "", 0o755);
        bin.file("ffmpeg", "", 0o755);
        let seen = Seen {
            alsa_capture: true,
            ..Seen::default()
        };
        let recorder = choose("auto", &env(&bin, seen)).unwrap();
        assert_eq!(words(&recorder.capture), Capture::Arecord.args());
        assert_eq!(words(&recorder.encoder), Encoder::Ffmpeg.args());
    }

    #[test]
    fn a_command_of_the_users_is_split_like_a_shell_would_and_needs_no_microphone_seen() {
        let bin = Dir::new("rec-custom-bin");
        let home = Dir::new("rec-custom-home");
        bin.file("pw-record", "", 0o755);
        bin.file("ffmpeg", "", 0o755);
        std::fs::create_dir(home.0.join("bin")).unwrap();
        home.file("bin/my recorder", "", 0o755);
        // Nothing was seen: with a command of their own that is the user's business.
        let mut nothing = env(&bin, Seen::default());
        nothing.home = Some(home.0.clone().into_os_string());

        let recorder = choose(
            "pw-record --target='USB Mic' --raw --rate=48000 --channels=1 -",
            &nothing,
        )
        .unwrap();
        // Their words only, none of ours, also for a program "auto" knows.
        assert_eq!(recorder.capture_kind(), Capture::Own);
        assert_eq!(recorder.capture.program(), bin.0.join("pw-record"));
        assert_eq!(
            words(&recorder.capture),
            [
                "--target=USB Mic",
                "--raw",
                "--rate=48000",
                "--channels=1",
                "-"
            ]
        );
        assert_eq!(recorder.encoder_kind(), Encoder::Ffmpeg);

        // A path, with `~` for the home folder.
        let recorder = choose("'~/bin/my recorder' -q", &nothing).unwrap();
        assert_eq!(recorder.names(), "my recorder + ffmpeg");
        assert_eq!(words(&recorder.capture), ["-q"]);
        // A shell is never what runs it: what looks like shell syntax is a word.
        let recorder = choose("pw-record ; rm -rf ~", &nothing).unwrap();
        assert_eq!(words(&recorder.capture), [";", "rm", "-rf", "~"]);
    }

    #[test]
    fn debug_output_shows_no_program_no_argument_and_no_folder() {
        let tool = Tool::new(
            PathBuf::from("/home/alice/bin/secret-recorder"),
            vec![OsString::from("--out=/home/alice/voice")],
        );
        assert_eq!(format!("{tool:?}"), "Tool");
        let recorder = Recorder::for_test(tool.clone(), tool);
        let shown = format!("{recorder:?}");
        assert_eq!(
            shown,
            "Recorder { capture: Tool, encoder: Tool, capture_kind: Own, encoder_kind: Opusenc }"
        );
        let env = Env {
            path: Some(OsString::from("/home/alice/bin")),
            home: Some(OsString::from("/home/alice")),
            ..Env::default()
        };
        assert!(!format!("{env:?}").contains("alice"));
    }
}
