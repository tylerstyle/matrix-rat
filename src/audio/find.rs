//! Looking for the player on this machine: the known ones on `PATH`, or the program a
//! command of the user's names. Nothing is run; a program counts when it is an
//! executable file (a `stat`, no more).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::{Kind, NoPlayer, Player, is_auto};

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
    let program = named_program(first, path, home).ok_or(NoPlayer::Missing)?;
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

/// The program a command of the user's starts with: looked up on `path`, unless it
/// names a file (`~/bin/play`, with `home` for `~`). `None`: not there, or not to be
/// run.
pub(super) fn named_program(
    first: &str,
    path: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<PathBuf> {
    if first.contains('/') {
        let named = match (first.strip_prefix("~/"), home) {
            (Some(rest), Some(home)) => Path::new(home).join(rest),
            _ => PathBuf::from(first),
        };
        is_executable(&named).then_some(named)
    } else {
        find_on_path(first, path)
    }
}

/// The first executable file called `name` in a folder of `path`. Relative folders
/// (and the empty entry, which means the current folder) are skipped.
pub(crate) fn find_on_path(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

pub(crate) fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::tests::Dir;

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
}
