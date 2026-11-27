//! Looking at this machine for a recorder: the sound servers' sockets and the list of
//! devices that record, the capture programs and encoders on `PATH`, or the program a
//! command of the user's names. Nothing is run or opened.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use super::{Capture, Encoder, Env, NoRecorder, Recorder, Seen, Tool, lists_capture};
use crate::audio::find::named_program;
use crate::audio::{find_on_path, is_auto};

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
}

fn exists(path: &Path) -> bool {
    std::fs::metadata(path).is_ok()
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
    use super::*;
    use crate::audio::tests::Dir;

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
    }

    #[test]
    fn what_is_chosen_carries_the_fixed_arguments() {
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
}
