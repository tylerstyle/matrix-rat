//! The files a sound plays from, and mpv's control sockets: in a private folder of the
//! app's runtime folder, under random names that say nothing about the file (only an
//! ending from its type), removed when done, and swept at start and exit.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// How the names of our own files start: nothing else in the folder is touched.
const SOUND: &str = "play-";
const SOCKET: &str = "ipc-";
/// A unix socket's address holds 108 bytes, the closing zero included; some room is
/// kept.
const MAX_SOCKET_PATH: usize = 100;

/// A downloaded sound in a file of its own while it plays: owner-only, gone when this
/// is dropped.
pub struct TempAudio {
    path: PathBuf,
}

/// Hand-written: the path names the user's runtime folder.
impl std::fmt::Debug for TempAudio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TempAudio")
    }
}

impl TempAudio {
    /// Writes `bytes` to a new file in `dir` (made if need be, owner-only), named
    /// `play-<random>.<ending>`, the ending from the sender's stated type (`mime`),
    /// never from the sender's file name.
    pub fn create(dir: &Path, mime: Option<&str>, bytes: &[u8]) -> io::Result<Self> {
        private_dir(dir)?;
        let mut last = io::Error::from(io::ErrorKind::AlreadyExists);
        for _ in 0..8 {
            let path = dir.join(format!("{SOUND}{}.{}", random_hex(), extension(mime)));
            // `create_new` never follows a link someone else put there.
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(mut file) => {
                    // Removed by `Drop` also if writing fails.
                    let sound = Self { path };
                    file.write_all(bytes)?;
                    file.flush()?;
                    return Ok(sound);
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => last = err,
                Err(err) => return Err(err),
            }
        }
        Err(last)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempAudio {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// The ending of a file of the stated type: one of a fixed few, `audio` for anything
/// else. The players look at what is in the file, not at its name.
pub fn extension(mime: Option<&str>) -> &'static str {
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

/// Makes `dir` if it is not there, owner-only, and checks that it is a folder of this
/// user's (not a link to one elsewhere). A folder that is not is refused with an error
/// of the kind `Other`. (The agent socket's folder is checked with this too.)
pub fn private_dir(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) => {
            // SAFETY: a plain system call without arguments.
            let me = unsafe { libc::geteuid() };
            if !meta.file_type().is_dir() || meta.uid() != me {
                return Err(io::Error::other("not a private folder"));
            }
            if meta.mode() & 0o077 != 0 {
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        }
        Err(err) => Err(err),
    }
}

/// Sixteen random hex digits, from the system's random source (or, should it not be
/// there, from the standard library's randomly seeded hasher).
fn random_hex() -> String {
    let mut bytes = [0u8; 8];
    let random = fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .map(|()| u64::from_ne_bytes(bytes))
        .unwrap_or_else(|_| {
            use std::hash::BuildHasher;
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos());
            std::hash::RandomState::new().hash_one((nanos, std::process::id()))
        });
    format!("{random:016x}")
}

/// A new name for mpv's control socket in `dir`, if a path that long can be a socket's
/// address.
pub fn socket_path(dir: &Path) -> Option<PathBuf> {
    let path = dir.join(format!("{SOCKET}{}.sock", random_hex()));
    (path.is_absolute() && path.as_os_str().len() <= MAX_SOCKET_PATH).then_some(path)
}

/// Removes what an earlier run left in `dir` (killed, or ended while a sound played):
/// sound files and sockets by our own names only. A folder that is not there is fine.
pub fn remove_leftovers(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let ours = name.starts_with(SOUND) || name.starts_with(SOCKET);
        if ours && entry.file_type().is_ok_and(|kind| !kind.is_dir()) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::tests::Dir;

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_sound_file_is_private_randomly_named_and_removed() {
        let root = Dir::new("temp");
        let dir = root.0.join("default-audio");
        let sound = TempAudio::create(&dir, Some("audio/ogg; codecs=opus"), b"OggS...").unwrap();
        // The folder and the file: the owner's only.
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(sound.path()), 0o600);
        assert_eq!(fs::read(sound.path()).unwrap(), b"OggS...");
        let name = sound
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            name.starts_with("play-") && name.ends_with(".ogg"),
            "{name}"
        );
        assert_eq!(name.len(), "play-".len() + 16 + ".ogg".len());
        assert!(name[5..21].chars().all(|c| c.is_ascii_hexdigit()));
        // Two are never the same file.
        let other = TempAudio::create(&dir, None, b"x").unwrap();
        assert_ne!(other.path(), sound.path());
        assert!(other.path().to_string_lossy().ends_with(".audio"));
        assert_eq!(format!("{sound:?}"), "TempAudio");
        drop(sound);
        drop(other);
        assert!(names(&dir).is_empty());

        // A folder that is open to others is closed; a link is not used.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let again = TempAudio::create(&dir, Some("audio/mpeg"), b"ID3").unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert!(again.path().to_string_lossy().ends_with(".mp3"));
        let link = root.0.join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(TempAudio::create(&link, None, b"x").is_err());
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
    fn leftovers_of_our_own_are_swept_and_nothing_else() {
        let root = Dir::new("leftovers");
        let dir = &root.0;
        fs::write(dir.join("play-00aa.ogg"), b"x").unwrap();
        fs::write(dir.join("ipc-00aa.sock"), b"").unwrap();
        fs::write(dir.join("notes.txt"), b"keep").unwrap();
        fs::create_dir(dir.join("play-dir")).unwrap();
        remove_leftovers(dir);
        assert_eq!(names(dir), ["notes.txt", "play-dir"]);
        // A folder that is not there yet is fine.
        remove_leftovers(&dir.join("missing"));

        let runtime = Path::new("/run/user/1000/matrix-rat/default-audio");
        let socket = socket_path(runtime).unwrap();
        assert_eq!(socket.parent(), Some(runtime));
        let name = socket.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("ipc-") && name.ends_with(".sock"),
            "{name}"
        );
        assert_ne!(socket_path(runtime), Some(socket));
        // Too long for a socket's address: none.
        assert_eq!(socket_path(&dir.join("x".repeat(100))), None);
        assert_eq!(socket_path(Path::new("relative")), None);
    }
}
