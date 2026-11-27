//! Which programs record a voice message: one that captures the microphone and one
//! that encodes what it hears.
//!
//! As with the player, no audio library is linked. A capture program writes raw sound
//! (signed 16-bit little-endian, mono, 48 kHz) to its standard output until it is told
//! to end (SIGTERM); an encoder reads exactly that on its standard input and writes
//! Ogg/Opus to its standard output. This file only chooses the two and says with which
//! arguments they are started. Nothing is run: programs are looked up on `PATH`, and a
//! sound server counts as there when its socket is (a `stat`, no more). Looking at
//! this machine is in [`find`]; what is found, and why nothing was, is here.

#[cfg(not(target_family = "wasm"))]
mod find;

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

#[cfg(not(target_family = "wasm"))]
pub use find::{choose, choose_here};

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
    #[cfg(any(not(target_family = "wasm"), test))]
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
#[cfg(any(not(target_family = "wasm"), test))]
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
    #[cfg(any(not(target_family = "wasm"), test))]
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

    #[cfg(any(not(target_family = "wasm"), test))]
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
    #[cfg(any(not(target_family = "wasm"), test))]
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
#[cfg(any(not(target_family = "wasm"), test))]
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

#[cfg(any(not(target_family = "wasm"), test))]
impl Seen {
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

/// Whether `/proc/asound/pcm` lists a device that records. Its lines read
/// `00-00: ALC256 Analog : ALC256 Analog : playback 1 : capture 1`: the device, its
/// name twice, then how many streams it has each way.
#[cfg(any(not(target_family = "wasm"), test))]
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
#[cfg(any(not(target_family = "wasm"), test))]
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
#[cfg(any(not(target_family = "wasm"), test))]
impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Env")
            .field("seen", &self.seen)
            .field("ssh", &self.ssh)
            .finish_non_exhaustive()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_missing_is_said_apart() {
        // What is said tells them apart, and says what the user can do.
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
