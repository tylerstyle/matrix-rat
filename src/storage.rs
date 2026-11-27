//! Where the app keeps what lasts from one run to the next: the configuration, the
//! user's themes, the kept notes and the saved messages. `App` reads and writes them
//! through the [`Storage`] the shell gave it, by [`Key`], and touches no file itself.
//!
//! On this system [`FsStorage`] keeps them in the files they have always been in, and
//! writes each the way it always was: the configuration and the themes in the shared
//! configuration folder (readable by others, as the user's umask has it), the notes
//! and the saved messages in the profile's data folder, readable by the user only;
//! each replaced in one step (a new file next to it, renamed into its place).
//! A shell without files (the browser's) keeps them in its own storage, which can
//! refuse (full, private mode): every operation can fail.

use std::io;
#[cfg(not(target_family = "wasm"))]
use std::io::{Read, Write};
#[cfg(not(target_family = "wasm"))]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
#[cfg(not(target_family = "wasm"))]
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[cfg(not(target_family = "wasm"))]
use crate::paths::Paths;

/// What is kept.
#[derive(Clone, PartialEq, Eq)]
pub enum Key {
    /// The settings (`config.toml`), shared by all profiles.
    Config,
    /// A theme of the user's, by name (`themes/<name>.toml`), shared by all profiles.
    Theme(String),
    /// The transcripts and translations `keep_notes` keeps (`notes.json`): message
    /// text, the profile's own.
    Notes,
    /// The saved messages and their reminders (`saved.json`), the profile's own.
    Saved,
}

/// Hand-written: a theme's name is the user's own choice.
impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Config => "Config",
            Self::Theme(_) => "Theme(..)",
            Self::Notes => "Notes",
            Self::Saved => "Saved",
        })
    }
}

/// What [`Storage::remove`] takes away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    /// What is kept under the key.
    Value,
    /// That, and whatever was set aside or left half written for it: nothing of it
    /// stays (an explicit logout).
    All,
}

/// How [`Storage::set_aside`] keeps what it sets aside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aside {
    /// Moved out of the way: nothing is kept under the key afterwards.
    Move,
    /// Copied, readable by the user only: the key keeps what it has.
    Copy,
}

/// What [`Storage::list`] lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listing {
    /// The names [`Key::Theme`] finds something under, in no order.
    Themes,
}

/// The app's persistence. Synchronous: what is kept is small, and read or written
/// only on an action of the user's or once at a login.
pub trait Storage {
    /// What is kept under `key`; `None` where nothing is.
    fn read(&self, key: &Key) -> Result<Option<Vec<u8>>>;
    /// Whether something is kept under `key`.
    fn exists(&self, key: &Key) -> bool;
    /// Keeps `bytes` under `key` in place of what was there, in one step: a failed
    /// write leaves the old value.
    fn write(&self, key: &Key, bytes: &[u8]) -> Result<()>;
    /// Takes away what is kept under `key` (see [`Removal`]). Nothing there is fine.
    fn remove(&self, key: &Key, what: Removal) -> Result<()>;
    /// The names kept of one kind.
    fn list(&self, of: Listing) -> Result<Vec<String>>;
    /// Sets what is kept under `key` aside as `label`, next to it, never over
    /// something set aside before (`label`, `label.1`, `label.2`…, up to a bound).
    fn set_aside(&self, key: &Key, label: &str, how: Aside) -> Result<()>;
    /// Where `key` is kept, for a message to the user (never for the log): the
    /// file's path on this system.
    fn describe(&self, key: &Key) -> String;
    /// For tests: the files, to name the one a test uses.
    #[cfg(test)]
    fn files(&mut self) -> Option<&mut FsStorage> {
        None
    }
}

/// What is kept under `key` as text. Not UTF-8 is an error, as reading a file to a
/// string has it.
pub fn read_text(storage: &dyn Storage, key: &Key) -> Result<Option<String>> {
    let Some(bytes) = storage.read(key)? else {
        return Ok(None);
    };
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "stream did not contain valid UTF-8",
            )
        })
        .with_context(|| format!("could not read {}", storage.describe(key)))
}

/// What is kept under `key`, which was just found there: gone since is an error, as
/// it was when the file was read by its path.
pub fn read_found(storage: &dyn Storage, key: &Key) -> Result<String> {
    read_text(storage, key)?
        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        .with_context(|| format!("could not read {}", storage.describe(key)))
}

#[cfg(not(target_family = "wasm"))]
/// The files the app has always kept, each where and how it always was.
#[derive(Clone, Default)]
pub struct FsStorage {
    config_file: PathBuf,
    themes_dir: PathBuf,
    notes_file: PathBuf,
    saved_file: PathBuf,
}

#[cfg(not(target_family = "wasm"))]
/// Hand-written: the paths name the user's home folder.
impl std::fmt::Debug for FsStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FsStorage")
    }
}

#[cfg(not(target_family = "wasm"))]
/// Files put aside under one label; beyond that nothing is put aside.
pub const ASIDE_MOST: usize = 20;

#[cfg(not(target_family = "wasm"))]
impl FsStorage {
    /// The profile's files: the configuration and the themes in the shared
    /// configuration folder, the notes and the saved messages in its data folder.
    pub fn new(paths: &Paths) -> Self {
        Self {
            config_file: paths.config_file(),
            themes_dir: paths.themes_dir(),
            notes_file: paths.notes_file(),
            saved_file: paths.saved_file(),
        }
    }

    /// The configuration in `file` (for reading a configuration by its path).
    pub fn with_config(self, file: impl Into<PathBuf>) -> Self {
        Self {
            config_file: file.into(),
            ..self
        }
    }

    fn path(&self, key: &Key) -> PathBuf {
        match key {
            Key::Config => self.config_file.clone(),
            Key::Theme(name) => self.themes_dir.join(format!("{name}.toml")),
            Key::Notes => self.notes_file.clone(),
            Key::Saved => self.saved_file.clone(),
        }
    }
}

#[cfg(test)]
impl FsStorage {
    pub fn with_themes(self, dir: impl Into<PathBuf>) -> Self {
        Self {
            themes_dir: dir.into(),
            ..self
        }
    }

    pub fn with_notes(self, file: impl Into<PathBuf>) -> Self {
        Self {
            notes_file: file.into(),
            ..self
        }
    }

    pub fn with_saved(self, file: impl Into<PathBuf>) -> Self {
        Self {
            saved_file: file.into(),
            ..self
        }
    }
}

#[cfg(not(target_family = "wasm"))]
impl Storage for FsStorage {
    fn read(&self, key: &Key) -> Result<Option<Vec<u8>>> {
        let path = self.path(key);
        match key {
            Key::Config | Key::Theme(_) => match std::fs::read_to_string(&path) {
                Ok(text) => Ok(Some(text.into_bytes())),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(err) => Err(err).with_context(|| format!("could not read {}", path.display())),
            },
            Key::Notes => {
                // Only a plain file (a link is not followed) of a sound size is read.
                let plain = std::fs::symlink_metadata(&path).is_ok_and(|meta| {
                    meta.file_type().is_file() && meta.len() <= crate::notes::MAX_FILE
                });
                if !plain {
                    return Ok(None);
                }
                Ok(Some(std::fs::read(&path)?))
            }
            Key::Saved => {
                Ok(read_bounded(&path, crate::saved::MAX_FILE_BYTES)?.map(String::into_bytes))
            }
        }
    }

    fn exists(&self, key: &Key) -> bool {
        self.path(key).exists()
    }

    fn write(&self, key: &Key, bytes: &[u8]) -> Result<()> {
        let path = self.path(key);
        match key {
            Key::Config | Key::Theme(_) => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                let temporary = path.with_extension("toml.new");
                std::fs::write(&temporary, bytes)
                    .and_then(|()| std::fs::rename(&temporary, &path))
                    .with_context(|| format!("could not write {}", path.display()))
            }
            Key::Notes => Ok(write_owner_only(&path, bytes)?),
            Key::Saved => Ok(write_private(&path, bytes)?),
        }
    }

    fn remove(&self, key: &Key, what: Removal) -> Result<()> {
        let path = self.path(key);
        match (key, what) {
            (_, Removal::Value) => Ok(remove_if_there(&path)?),
            (Key::Saved, Removal::All) => {
                remove_with_siblings(&path);
                Ok(())
            }
            (Key::Notes, Removal::All) => {
                let value = remove_if_there(&path);
                let pending = remove_if_there(&path.with_extension("json.new"));
                Ok(value.and(pending)?)
            }
            (Key::Config | Key::Theme(_), Removal::All) => {
                let value = remove_if_there(&path);
                let pending = remove_if_there(&path.with_extension("toml.new"));
                Ok(value.and(pending)?)
            }
        }
    }

    fn list(&self, of: Listing) -> Result<Vec<String>> {
        match of {
            Listing::Themes => Ok(std::fs::read_dir(&self.themes_dir)?
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
                .filter_map(|path| path.file_stem().and_then(|s| s.to_str()).map(str::to_owned))
                .collect()),
        }
    }

    fn set_aside(&self, key: &Key, label: &str, how: Aside) -> Result<()> {
        let path = self.path(key);
        let name = path
            .file_name()
            .map(|name| format!("{}.{label}", name.to_string_lossy()))
            .ok_or_else(|| io::Error::other("nothing to set aside"))?;
        match how {
            Aside::Move => {
                let aside =
                    free_name(&path, &name).ok_or_else(|| io::Error::other("no name left"))?;
                std::fs::rename(&path, aside)?;
            }
            Aside::Copy => {
                let copy = free_name(&path, &name)
                    .ok_or_else(|| io::Error::other("no name left for the copy"))?;
                copy_private(&path, &copy)?;
            }
        }
        Ok(())
    }

    fn describe(&self, key: &Key) -> String {
        self.path(key).display().to_string()
    }

    #[cfg(test)]
    fn files(&mut self) -> Option<&mut FsStorage> {
        Some(self)
    }
}

#[cfg(not(target_family = "wasm"))]
/// The file's text, of at most `most` bytes; `None` if there is no file. A larger
/// file is an error, read no further than one byte beyond.
fn read_bounded(file: &Path, most: u64) -> io::Result<Option<String>> {
    let opened = match std::fs::File::open(file) {
        Ok(opened) => opened,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut text = String::new();
    // One byte more than allowed tells a file that is too large from one that fits.
    opened.take(most + 1).read_to_string(&mut text)?;
    if text.len() as u64 > most {
        return Err(io::Error::other("too large"));
    }
    Ok(Some(text))
}

#[cfg(not(target_family = "wasm"))]
/// Removes `file`; not there is fine.
fn remove_if_there(file: &Path) -> io::Result<()> {
    match std::fs::remove_file(file) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

#[cfg(not(target_family = "wasm"))]
/// Removes `file` and everything next to it whose name starts with its name and a
/// dot: what was put aside, and a write's leftover. Best effort.
fn remove_with_siblings(file: &Path) {
    let (Some(dir), Some(name)) = (file.parent(), file.file_name().and_then(|n| n.to_str())) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let ours = entry
            .file_name()
            .to_str()
            .is_some_and(|found| found == name || found.starts_with(&format!("{name}.")));
        if ours {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(not(target_family = "wasm"))]
/// `name` next to `file`, or `name.1`, `name.2`…: the first that is not taken, so
/// that what was put aside earlier is not written over.
fn free_name(file: &Path, name: &str) -> Option<PathBuf> {
    (0..ASIDE_MOST)
        .map(|n| match n {
            0 => file.with_file_name(name),
            n => file.with_file_name(format!("{name}.{n}")),
        })
        .find(|candidate| {
            matches!(
                std::fs::symlink_metadata(candidate),
                Err(err) if err.kind() == io::ErrorKind::NotFound
            )
        })
}

#[cfg(not(target_family = "wasm"))]
/// Copies `source` into a new file at `target`, readable by the user only, and
/// waits for it to reach the disk.
fn copy_private(source: &Path, target: &Path) -> io::Result<()> {
    let bytes = std::fs::read(source)?;
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(target)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(&bytes)?;
        file.sync_all()
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(target);
    }
    written
}

#[cfg(not(target_family = "wasm"))]
/// Writes `bytes` to a new file next to `path`, for the user only, and puts it in
/// `path`'s place in one step, once it is on the disk.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension("json.new");
    // What an interrupted write left is not built upon: its mode is not known.
    let _ = std::fs::remove_file(&temporary);
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        // On the disk before it takes the old file's place: a crash leaves one of
        // the two whole.
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

#[cfg(not(target_family = "wasm"))]
/// Writes `bytes` to a new file next to `path`, created for the owner only, and
/// renames it into `path`'s place.
fn write_owner_only(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension("json.new");
    // A leftover of an earlier try, or anything else lying there, is not written
    // through: the file is made anew, for the owner only.
    let _ = std::fs::remove_file(&temporary);
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let written = out
        .write_all(bytes)
        .and_then(|()| out.flush())
        .and_then(|()| std::fs::rename(&temporary, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// In memory, for the tests of what the app keeps: a handle that shares its
/// contents, so a test can look at what the app it was given wrote.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct MemStorage {
    kept: std::rc::Rc<std::cell::RefCell<MemKept>>,
}

/// What a [`MemStorage`] keeps: the values by name, and apart from them what was set
/// aside, by the name it was set aside from and its label (`label`, `label.1`…), so
/// no value's name (a theme `a.b`) is taken for something set aside.
#[cfg(test)]
#[derive(Default)]
struct MemKept {
    entries: std::collections::BTreeMap<String, Vec<u8>>,
    asides: std::collections::BTreeMap<(String, String), Vec<u8>>,
}

#[cfg(test)]
impl MemStorage {
    fn name(key: &Key) -> String {
        match key {
            Key::Config => "config".to_owned(),
            Key::Theme(name) => format!("theme/{name}"),
            Key::Notes => "notes".to_owned(),
            Key::Saved => "saved".to_owned(),
        }
    }

    /// Every name kept (asides as `<key>.<label>`), in order.
    pub fn names(&self) -> Vec<String> {
        let kept = self.kept.borrow();
        let mut names: Vec<String> = kept.entries.keys().cloned().collect();
        names.extend(
            kept.asides
                .keys()
                .map(|(name, label)| format!("{name}.{label}")),
        );
        names.sort();
        names
    }

    pub fn put(&self, key: &Key, bytes: &[u8]) {
        self.kept
            .borrow_mut()
            .entries
            .insert(Self::name(key), bytes.to_vec());
    }
}

#[cfg(test)]
impl Storage for MemStorage {
    fn read(&self, key: &Key) -> Result<Option<Vec<u8>>> {
        Ok(self.kept.borrow().entries.get(&Self::name(key)).cloned())
    }

    fn exists(&self, key: &Key) -> bool {
        self.kept.borrow().entries.contains_key(&Self::name(key))
    }

    fn write(&self, key: &Key, bytes: &[u8]) -> Result<()> {
        self.put(key, bytes);
        Ok(())
    }

    fn remove(&self, key: &Key, what: Removal) -> Result<()> {
        let name = Self::name(key);
        let mut kept = self.kept.borrow_mut();
        kept.entries.remove(&name);
        if what == Removal::All {
            kept.asides.retain(|(from, _), _| *from != name);
        }
        Ok(())
    }

    fn list(&self, of: Listing) -> Result<Vec<String>> {
        match of {
            Listing::Themes => Ok(self
                .kept
                .borrow()
                .entries
                .keys()
                .filter_map(|name| name.strip_prefix("theme/"))
                .map(str::to_owned)
                .collect()),
        }
    }

    fn set_aside(&self, key: &Key, label: &str, how: Aside) -> Result<()> {
        let name = Self::name(key);
        let mut kept = self.kept.borrow_mut();
        let value = kept
            .entries
            .get(&name)
            .cloned()
            .ok_or_else(|| io::Error::other("nothing"))?;
        let aside = (0..ASIDE_MOST)
            .map(|n| match n {
                0 => (name.clone(), label.to_owned()),
                n => (name.clone(), format!("{label}.{n}")),
            })
            .find(|candidate| !kept.asides.contains_key(candidate))
            .ok_or_else(|| io::Error::other("no name left"))?;
        kept.asides.insert(aside, value);
        if how == Aside::Move {
            kept.entries.remove(&name);
        }
        Ok(())
    }

    fn describe(&self, key: &Key) -> String {
        format!("memory:{}", Self::name(key))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::textcmd::tests::Dir;

    /// The profile's files as `Paths` lays them out: the configuration and themes in
    /// one folder, the notes and the saved messages in another.
    fn files_in(dir: &Path) -> FsStorage {
        FsStorage {
            config_file: dir.join("config").join("config.toml"),
            themes_dir: dir.join("config").join("themes"),
            notes_file: dir.join("data").join("notes.json"),
            saved_file: dir.join("data").join("saved.json"),
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// What every storage does, the files and one without them alike.
    fn keeps_by_key_and_sets_aside(storage: &dyn Storage) {
        let keys = [
            Key::Config,
            Key::Theme("mine".into()),
            Key::Notes,
            Key::Saved,
        ];
        for key in &keys {
            assert_eq!(storage.read(key).unwrap(), None, "{key:?}");
            assert!(!storage.exists(key));
            // Nothing there to remove is fine.
            storage.remove(key, Removal::Value).unwrap();
            storage.write(key, b"first").unwrap();
            assert!(storage.exists(key));
            storage.write(key, b"second").unwrap();
            assert_eq!(storage.read(key).unwrap().as_deref(), Some(&b"second"[..]));
        }
        assert_eq!(storage.list(Listing::Themes).unwrap(), ["mine"]);
        assert_eq!(read_found(storage, &Key::Config).unwrap(), "second");

        // A copy leaves the value; a move takes it; neither goes over an earlier one.
        storage
            .set_aside(&Key::Saved, "before-repair", Aside::Copy)
            .unwrap();
        assert_eq!(
            storage.read(&Key::Saved).unwrap().as_deref(),
            Some(&b"second"[..])
        );
        storage
            .set_aside(&Key::Saved, "damaged", Aside::Move)
            .unwrap();
        assert!(!storage.exists(&Key::Saved));
        assert!(
            storage
                .set_aside(&Key::Saved, "damaged", Aside::Move)
                .is_err()
        );
        for _ in 1..ASIDE_MOST {
            storage.write(&Key::Saved, b"again").unwrap();
            storage
                .set_aside(&Key::Saved, "damaged", Aside::Move)
                .unwrap();
        }
        // No name left: what could not be set aside stays where it is.
        storage.write(&Key::Saved, b"last").unwrap();
        assert!(
            storage
                .set_aside(&Key::Saved, "damaged", Aside::Move)
                .is_err()
        );
        assert!(storage.exists(&Key::Saved));

        // Removing a value takes that alone; removing all, what was set aside too.
        storage
            .remove(&Key::Theme("mine".into()), Removal::Value)
            .unwrap();
        assert!(storage.list(Listing::Themes).unwrap().is_empty());
        // A theme's name may hold a dot: it is listed, and removing all of a theme
        // whose name starts the same leaves it.
        storage.write(&Key::Theme("mine".into()), b"third").unwrap();
        storage
            .write(&Key::Theme("mine.v2".into()), b"other")
            .unwrap();
        let mut themes = storage.list(Listing::Themes).unwrap();
        themes.sort();
        assert_eq!(themes, ["mine", "mine.v2"]);
        storage
            .remove(&Key::Theme("mine".into()), Removal::All)
            .unwrap();
        assert_eq!(storage.list(Listing::Themes).unwrap(), ["mine.v2"]);
        storage
            .remove(&Key::Theme("mine.v2".into()), Removal::Value)
            .unwrap();
        storage.remove(&Key::Saved, Removal::All).unwrap();
        storage.remove(&Key::Notes, Removal::All).unwrap();
        assert!(!storage.exists(&Key::Saved) && !storage.exists(&Key::Notes));
        assert!(storage.exists(&Key::Config));
        assert_eq!(read_found(storage, &Key::Config).unwrap(), "second");
        assert!(read_found(storage, &Key::Notes).is_err());
    }

    #[test]
    fn the_storage_in_memory_keeps_by_key_and_sets_aside_as_the_files_do() {
        let storage = MemStorage::default();
        keeps_by_key_and_sets_aside(&storage);
        assert_eq!(storage.names(), ["config"]);
    }

    #[test]
    fn the_files_keep_by_key_and_set_aside_next_to_where_they_are() {
        let dir = Dir::new("storage-contract");
        let files = files_in(&dir.0);
        // The profile's data folder is made at start (`Paths::create_dirs`).
        std::fs::create_dir_all(dir.0.join("data")).unwrap();
        keeps_by_key_and_sets_aside(&files);
        assert_eq!(names(&dir.0.join("config")), ["config.toml", "themes"]);
        assert!(names(&dir.0.join("data")).is_empty());
    }

    /// The configuration and themes are shared, as readable as the user's umask makes
    /// a new file; the notes, the saved messages and their copies are the user's only.
    /// Each is replaced in one step, through a new file next to it that does not stay.
    #[test]
    fn the_files_are_written_where_and_as_they_always_were() {
        let dir = Dir::new("storage-files");
        let files = files_in(&dir.0);
        std::fs::write(dir.0.join("as-the-umask-has-it"), b"x").unwrap();
        let shared = mode(&dir.0.join("as-the-umask-has-it"));

        // The folder is made where there is none yet.
        files.write(&Key::Config, b"mouse = false\n").unwrap();
        files
            .write(&Key::Theme("mine".into()), b"[palette]\n")
            .unwrap();
        let config = dir.0.join("config");
        assert_eq!(names(&config), ["config.toml", "themes"]);
        assert_eq!(names(&config.join("themes")), ["mine.toml"]);
        assert_eq!(mode(&config.join("config.toml")), shared);
        assert_eq!(mode(&config.join("themes/mine.toml")), shared);
        assert_eq!(
            files.describe(&Key::Config),
            config.join("config.toml").display().to_string()
        );

        std::fs::create_dir_all(dir.0.join("data")).unwrap();
        // What an earlier write left is not written through.
        std::fs::write(dir.0.join("data/saved.json.new"), b"old").unwrap();
        std::fs::write(dir.0.join("data/notes.json.new"), b"old").unwrap();
        std::fs::set_permissions(
            dir.0.join("data/saved.json.new"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        files.write(&Key::Saved, b"{}").unwrap();
        files.write(&Key::Notes, b"{}").unwrap();
        files
            .set_aside(&Key::Saved, "before-repair", Aside::Copy)
            .unwrap();
        let data = dir.0.join("data");
        assert_eq!(
            names(&data),
            ["notes.json", "saved.json", "saved.json.before-repair"]
        );
        for name in ["notes.json", "saved.json", "saved.json.before-repair"] {
            assert_eq!(mode(&data.join(name)), 0o600, "{name}");
        }

        // Themes are the files ending in `.toml`, by name.
        std::fs::write(config.join("themes/notes.txt"), b"x").unwrap();
        std::fs::write(config.join("themes/other.toml"), b"[palette]\n").unwrap();
        let mut themes = files.list(Listing::Themes).unwrap();
        themes.sort();
        assert_eq!(themes, ["mine", "other"]);
        // No folder of themes: an error, which the app takes for none.
        assert!(FsStorage::default().list(Listing::Themes).is_err());

        // A logout's removal takes the notes' half-written file too, and leaves
        // other files of the profile.
        std::fs::write(data.join("notes.json.new"), b"half").unwrap();
        std::fs::write(data.join("session.json"), b"x").unwrap();
        files.remove(&Key::Notes, Removal::All).unwrap();
        files.remove(&Key::Saved, Removal::All).unwrap();
        assert_eq!(names(&data), ["session.json"]);
    }

    #[test]
    fn what_cannot_be_read_says_where_and_neither_debug_output_names_it() {
        let dir = Dir::new("storage-errors");
        let files = files_in(&dir.0);
        // A folder where the configuration should be.
        std::fs::create_dir_all(dir.0.join("config/config.toml")).unwrap();
        let err = files.read(&Key::Config).unwrap_err();
        let shown = format!("{err:#}");
        assert!(shown.starts_with("could not read "), "{shown}");
        assert!(shown.contains("config.toml"), "{shown}");
        // Text that is not UTF-8.
        std::fs::create_dir_all(dir.0.join("config/themes")).unwrap();
        std::fs::write(dir.0.join("config/themes/bad.toml"), [0xff, 0xfe]).unwrap();
        assert!(read_text(&files, &Key::Theme("bad".into())).is_err());
        let storage = MemStorage::default();
        storage.put(&Key::Config, &[0xff]);
        let err = read_text(&storage, &Key::Config).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "could not read memory:config: stream did not contain valid UTF-8"
        );

        let shown = format!("{files:?} {:?}", Key::Theme("secret-name".into()));
        assert_eq!(shown, "FsStorage Theme(..)");
    }
}
