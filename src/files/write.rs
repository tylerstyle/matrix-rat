//! Writing files on this machine: private (0600), never over an existing file, and
//! under their name only when they are complete.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::safe_name;

/// Writes `bytes` into `dir` under `name`, or `name (1)`, `name (2)`… if that exists:
/// nothing is ever overwritten, no link is followed, and the file appears under its
/// name only when it is complete. Only the user may read it (mode 0600), like
/// [`NewFile`], since saved attachments may come from encrypted rooms.
pub fn write_new(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::ErrorKind::AlreadyExists;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(dir)?;
    let name = safe_name(name);
    // The data goes into a new hidden file first (never an existing one).
    let mut partial = None;
    for attempt in 0..100u32 {
        let nanos = crate::time::SystemTime::now()
            .duration_since(crate::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let path = dir.join(format!(
            ".matrix-rat-{}-{nanos}-{attempt}.part",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
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

/// A private file that is written piece by piece: it appears in `dir` under its name
/// (or `name (1)`, `name (2)`… where that is taken) only when [`NewFile::finish`] is
/// called, complete. Nothing is overwritten and no link is followed, as with
/// [`write_new`]. Only the user may read it (mode 0600): it holds the text of a room
/// that may be encrypted. Dropped before it is finished, nothing of it stays.
pub struct NewFile {
    dir: PathBuf,
    name: String,
    partial: PathBuf,
    file: std::io::BufWriter<std::fs::File>,
}

impl NewFile {
    pub fn create(dir: &Path, name: &str) -> std::io::Result<Self> {
        use std::io::ErrorKind::AlreadyExists;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::create_dir_all(dir)?;
        for attempt in 0..100u32 {
            let nanos = crate::time::SystemTime::now()
                .duration_since(crate::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            let partial = dir.join(format!(
                ".matrix-rat-{}-{nanos}-{attempt}.part",
                std::process::id()
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&partial)
            {
                Ok(file) => {
                    return Ok(Self {
                        dir: dir.to_owned(),
                        name: safe_name(name),
                        partial,
                        file: std::io::BufWriter::new(file),
                    });
                }
                Err(err) if err.kind() == AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }
        Err(std::io::Error::other("could not create a temporary file"))
    }

    /// Everything is written: the file gets its name, which is returned.
    pub fn finish(mut self) -> std::io::Result<PathBuf> {
        self.file.flush()?;
        self.file.get_ref().sync_all()?;
        // The partial file is removed when `self` is dropped, here as on an error.
        publish(&self.dir, &self.name, &self.partial)
    }
}

impl Write for NewFile {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.file.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl Drop for NewFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.partial);
    }
}

/// Gives the finished file its name. A hard link fails if the name is taken (by
/// anything, links included) and never shows a half-written file.
fn publish(dir: &Path, name: &str, partial: &Path) -> std::io::Result<PathBuf> {
    use std::io::ErrorKind::AlreadyExists;
    use std::os::unix::fs::OpenOptionsExt;
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
            .mode(0o600)
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
    fn nothing_is_overwritten() {
        use std::os::unix::fs::PermissionsExt;
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
        assert_eq!(
            std::fs::metadata(&first).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&second).unwrap().permissions().mode() & 0o777,
            0o600
        );
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
    fn a_file_written_piece_by_piece_is_private_whole_and_never_in_the_way() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("matrix-rat-newfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let names = |dir: &Path| {
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };

        let mut file = NewFile::create(&dir, "../room.md").unwrap();
        file.write_all(b"one\n").unwrap();
        // Not under its name before it is complete.
        assert!(!dir.join("room.md").exists());
        file.write_all(b"two\n").unwrap();
        let first = file.finish().unwrap();
        assert_eq!(first, dir.join("room.md"));
        assert_eq!(std::fs::read(&first).unwrap(), b"one\ntwo\n");
        let mode = std::fs::metadata(&first).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        // The name is taken: the next one goes beside it, as private.
        let mut file = NewFile::create(&dir, "room.md").unwrap();
        file.write_all(b"three").unwrap();
        let second = file.finish().unwrap();
        assert_eq!(second, dir.join("room (1).md"));
        assert_eq!(std::fs::read(&first).unwrap(), b"one\ntwo\n");
        let mode = std::fs::metadata(&second).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        // Given up: nothing of it stays.
        let mut file = NewFile::create(&dir, "gone.md").unwrap();
        file.write_all(b"half").unwrap();
        drop(file);
        assert_eq!(names(&dir), ["room (1).md", "room.md"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
