//! The user's folders on this machine: the home folder from `HOME`, the download
//! folder from the desktop's `user-dirs.dirs`, and paths shown with `~`.

use std::path::{Path, PathBuf};

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

/// A path for showing, with the home folder shortened to `~`.
pub fn display(path: &Path) -> String {
    match path.strip_prefix(home()) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders() {
        assert_eq!(resolve_dir("/srv/files"), PathBuf::from("/srv/files"));
        assert_eq!(resolve_dir("~/inbox"), home().join("inbox"));
        assert_eq!(resolve_dir("inbox"), home().join("inbox"));
        assert!(resolve_dir("-x").is_absolute());
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
