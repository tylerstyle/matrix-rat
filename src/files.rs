//! Saving attachments to disk: where they go and under which name.
//!
//! The names and the rules are here. The folders the user's environment points to
//! (`HOME`, `user-dirs.dirs`) are found in [`home`]; the files are written in
//! [`write`].

#[cfg(not(target_family = "wasm"))]
mod home;
/// The browser has no home folder, no `HOME` and no `user-dirs.dirs`: a folder is
/// taken and shown as it was given.
#[cfg(target_family = "wasm")]
mod home {
    use std::path::{Path, PathBuf};

    pub fn resolve_dir(setting: &str) -> PathBuf {
        PathBuf::from(setting.trim())
    }

    pub fn display(path: &Path) -> String {
        path.display().to_string()
    }
}
#[cfg(not(target_family = "wasm"))]
mod write;
/// The browser has no folder to write into: every file is refused (phase 2 gives it
/// downloads).
#[cfg(target_family = "wasm")]
mod write {
    use std::io;
    use std::path::{Path, PathBuf};

    fn refused() -> io::Error {
        io::Error::from(io::ErrorKind::Unsupported)
    }

    pub fn write_new(_dir: &Path, _name: &str, _bytes: &[u8]) -> io::Result<PathBuf> {
        Err(refused())
    }

    /// Never made: [`NewFile::create`] refuses.
    pub enum NewFile {}

    impl NewFile {
        pub fn create(_dir: &Path, _name: &str) -> io::Result<Self> {
            Err(refused())
        }

        pub fn finish(self) -> io::Result<PathBuf> {
            match self {}
        }
    }

    impl io::Write for NewFile {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            match *self {}
        }

        fn flush(&mut self) -> io::Result<()> {
            match *self {}
        }
    }
}

use std::path::Path;

pub use home::{display, resolve_dir};
pub use write::{NewFile, write_new};

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
    fn what_opens_at_once() {
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
    }
}
