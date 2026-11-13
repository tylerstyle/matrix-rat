//! Saving attachments to disk: where they go and under which name.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::text::sanitize_line;

/// Asked about once before a download this large starts.
pub const LARGE_FILE: u64 = 100 * 1000 * 1000;

/// A file name that is safe to create inside the download folder. The name comes from
/// whoever sent the file, so it may carry a path, control characters or a leading dot.
pub fn safe_name(name: &str) -> String {
    let cleaned = sanitize_line(name);
    let last = cleaned
        .trim_end_matches(['/', '\\', ' '])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    let mut name: String = last
        .chars()
        .map(|c| if matches!(c, ':' | '\0') { '_' } else { c })
        .collect();
    // File systems limit names to 255 bytes; keep the end, where the extension is.
    while name.len() > 200 {
        name.remove(0);
    }
    // After cutting, so that no cut can bring a dot to the front: never a hidden file.
    let name = name.trim().trim_start_matches(['.', ' ']).to_owned();
    if name.is_empty() {
        "attachment".to_owned()
    } else {
        name
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The desktop's download folder (from `user-dirs.dirs`), or `~/Downloads`.
pub fn default_dir() -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home().join(".config"));
    std::fs::read_to_string(config.join("user-dirs.dirs"))
        .ok()
        .and_then(|text| user_dirs_download(&text))
        .unwrap_or_else(|| home().join("Downloads"))
}

fn user_dirs_download(text: &str) -> Option<PathBuf> {
    let value = text
        .lines()
        .filter_map(|line| line.trim().strip_prefix("XDG_DOWNLOAD_DIR="))
        .next_back()?
        .trim()
        .trim_matches('"');
    let path = match value.strip_prefix("$HOME") {
        Some(rest) => home().join(rest.trim_start_matches('/')),
        None => PathBuf::from(value),
    };
    // The format allows nothing but absolute paths; `$HOME` alone means "not set".
    (path.is_absolute() && path != home()).then_some(path)
}

/// The folder for a `download_dir` setting; an empty setting means the default. Always
/// absolute: a relative setting is taken from the home folder, not from wherever the
/// program happened to be started.
pub fn resolve_dir(setting: &str) -> PathBuf {
    let setting = setting.trim();
    if setting.is_empty() {
        default_dir()
    } else if setting == "~" {
        home()
    } else if let Some(rest) = setting.strip_prefix("~/") {
        home().join(rest)
    } else if Path::new(setting).is_absolute() {
        PathBuf::from(setting)
    } else {
        home().join(setting)
    }
}

/// Whether a saved file may be handed to the desktop's default program right away.
/// The sender chooses the name, so only types that open in a viewer or player qualify;
/// scripts, launchers, web pages, documents with macros and everything unknown do not.
pub fn opens_safely(path: &Path) -> bool {
    const VIEWABLE: [&str; 24] = [
        "png", "jpg", "jpeg", "gif", "webp", "bmp", "avif", "mp3", "ogg", "oga", "opus", "flac",
        "wav", "m4a", "mp4", "mkv", "webm", "mov", "avi", "txt", "log", "md", "csv", "pdf",
    ];
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| VIEWABLE.contains(&extension.to_ascii_lowercase().as_str()))
}

/// A path for showing, with the home folder shortened to `~`.
pub fn display(path: &Path) -> String {
    match path.strip_prefix(home()) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Writes `bytes` into `dir` under `name`, or `name (1)`, `name (2)`… if that exists:
/// nothing is ever overwritten, no link is followed, and the file appears under its
/// name only when it is complete.
pub fn write_new(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::ErrorKind::AlreadyExists;
    std::fs::create_dir_all(dir)?;
    let name = safe_name(name);
    // The data goes into a new hidden file first (never an existing one).
    let mut partial = None;
    for attempt in 0..100u32 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let path = dir.join(format!(
            ".matrix-rat-{}-{nanos}-{attempt}.part",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                partial = Some((path, file));
                break;
            }
            Err(err) if err.kind() == AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    let Some((partial, mut file)) = partial else {
        return Err(std::io::Error::other("could not create a temporary file"));
    };
    let result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| publish(dir, &name, &partial));
    let _ = std::fs::remove_file(&partial);
    result
}

/// Gives the finished file its name. A hard link fails if the name is taken (by
/// anything, links included) and never shows a half-written file.
fn publish(dir: &Path, name: &str, partial: &Path) -> std::io::Result<PathBuf> {
    use std::io::ErrorKind::AlreadyExists;
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    for n in 0..10_000 {
        let candidate = if n == 0 {
            dir.join(name)
        } else {
            dir.join(format!("{stem} ({n}){extension}"))
        };
        match std::fs::hard_link(partial, &candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == AlreadyExists => continue,
            Err(_) => {}
        }
        // File systems without hard links (FAT, some network shares): reserve the
        // name with a new empty file, then move the data onto it.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => {}
            Err(err) if err.kind() == AlreadyExists => continue,
            Err(err) => return Err(err),
        }
        return match std::fs::rename(partial, &candidate) {
            Ok(()) => Ok(candidate),
            Err(err) => {
                let _ = std::fs::remove_file(&candidate);
                Err(err)
            }
        };
    }
    Err(std::io::Error::other("too many files with this name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_from_strangers_are_made_safe() {
        assert_eq!(safe_name("report.pdf"), "report.pdf");
        assert_eq!(safe_name("../../.ssh/authorized_keys"), "authorized_keys");
        assert_eq!(safe_name("..\\..\\evil.exe"), "evil.exe");
        assert_eq!(safe_name("/etc/passwd"), "passwd");
        assert_eq!(safe_name(".bashrc"), "bashrc");
        assert_eq!(safe_name("a\u{1b}[31m\nb.txt"), "a[31m b.txt");
        assert_eq!(safe_name(""), "attachment");
        assert_eq!(safe_name("..."), "attachment");
        assert_eq!(safe_name("dir/"), "dir");
        assert_eq!(safe_name("/"), "attachment");
        assert_eq!(safe_name("dir/file.txt/"), "file.txt");
        // Cutting a long name must not bring a dot or a space to the front.
        let hidden = format!("{}.bashrc{}", "x".repeat(20), "a".repeat(193));
        assert!(
            !safe_name(&hidden).starts_with('.'),
            "{}",
            safe_name(&hidden)
        );
        assert_eq!(safe_name(&hidden).len(), 199);
        let long = format!("{}.tar.gz", "x".repeat(400));
        let name = safe_name(&long);
        assert!(name.len() <= 200 && name.ends_with(".tar.gz"));
    }

    #[test]
    fn nothing_is_overwritten() {
        let dir = std::env::temp_dir().join(format!("matrix-rat-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = write_new(&dir, "note.txt", b"one").unwrap();
        let second = write_new(&dir, "note.txt", b"two").unwrap();
        let third = write_new(&dir, "../note.txt", b"three").unwrap();
        assert_eq!(first, dir.join("note.txt"));
        assert_eq!(second, dir.join("note (1).txt"));
        assert_eq!(third, dir.join("note (2).txt"));
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        assert_eq!(std::fs::read(&third).unwrap(), b"three");
        assert_eq!(write_new(&dir, "README", b"x").unwrap(), dir.join("README"));
        assert_eq!(
            write_new(&dir, "README", b"x").unwrap(),
            dir.join("README (1)")
        );
        // A link in the folder is neither followed nor replaced.
        let target = dir.join("target");
        std::fs::write(&target, b"precious").unwrap();
        std::os::unix::fs::symlink(&target, dir.join("link.txt")).unwrap();
        let beside = write_new(&dir, "link.txt", b"new").unwrap();
        assert_eq!(beside, dir.join("link (1).txt"));
        assert_eq!(std::fs::read(&target).unwrap(), b"precious");
        // No partial files are left behind.
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".part")
            })
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folders() {
        assert_eq!(resolve_dir("/srv/files"), PathBuf::from("/srv/files"));
        assert_eq!(resolve_dir("~/inbox"), home().join("inbox"));
        assert_eq!(resolve_dir("inbox"), home().join("inbox"));
        assert!(resolve_dir("-x").is_absolute());
        for (name, safe) in [
            ("a.PNG", true),
            ("b.pdf", true),
            ("c.txt", true),
            ("d.desktop", false),
            ("e.sh", false),
            ("f.html", false),
            ("g.svg", false),
            ("h", false),
            ("i.png.exe", false),
        ] {
            assert_eq!(opens_safely(Path::new(name)), safe, "{name}");
        }
        assert_eq!(display(&home().join("Downloads")), "~/Downloads");
        assert_eq!(display(Path::new("/srv/files")), "/srv/files");
        assert_eq!(
            user_dirs_download(
                "# c\nXDG_DESKTOP_DIR=\"$HOME/D\"\nXDG_DOWNLOAD_DIR=\"$HOME/Laden\"\n"
            ),
            Some(home().join("Laden"))
        );
        assert_eq!(
            user_dirs_download("XDG_DOWNLOAD_DIR=\"/data/dl\""),
            Some(PathBuf::from("/data/dl"))
        );
        assert_eq!(user_dirs_download("XDG_DOWNLOAD_DIR=\"$HOME/\""), None);
        assert_eq!(user_dirs_download("XDG_DOWNLOAD_DIR=\"relative\""), None);
    }
}
