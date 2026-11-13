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
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .trim_start_matches('.')
        .trim();
    let mut name: String = last
        .chars()
        .map(|c| if matches!(c, ':' | '\0') { '_' } else { c })
        .collect();
    // File systems limit names to 255 bytes; keep the end, where the extension is.
    while name.len() > 200 {
        name.remove(0);
    }
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

/// The folder for a `download_dir` setting; an empty setting means the default.
pub fn resolve_dir(setting: &str) -> PathBuf {
    let setting = setting.trim();
    if setting.is_empty() {
        default_dir()
    } else if setting == "~" {
        home()
    } else if let Some(rest) = setting.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(setting)
    }
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
/// nothing is ever overwritten. The file appears under its name only when complete.
pub fn write_new(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let name = safe_name(name);
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name.as_str(), ""),
    };
    for n in 0..10_000 {
        let candidate = if n == 0 {
            dir.join(&name)
        } else {
            dir.join(format!("{stem} ({n}){extension}"))
        };
        // Creating the file reserves the name; an existing one is left alone.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
        let partial = dir.join(format!(".{}.{}.part", std::process::id(), n));
        let written = std::fs::File::create(&partial)
            .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()))
            .and_then(|()| std::fs::rename(&partial, &candidate));
        return match written {
            Ok(()) => Ok(candidate),
            Err(err) => {
                let _ = std::fs::remove_file(&partial);
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
        assert_eq!(safe_name("dir/"), "attachment");
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
