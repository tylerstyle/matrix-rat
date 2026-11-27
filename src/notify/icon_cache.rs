//! The icons of desktop notifications as files: the desktop's notification service
//! takes a picture by its path, so a room's picture or letter tile is written as a PNG
//! to an owner-only folder of the app's runtime folder, a few at a time.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::{NotifyIcon, cache_filename, encode_icon_png, icon_cache_key};

const DEFAULT_CACHE_LIMIT: usize = 64;

/// Thread-safe, bounded, symlink-safe cache of `0o600` PNG notification icons in `runtime_dir`.
pub struct IconCache {
    state: Mutex<CacheState>,
}

impl std::fmt::Debug for IconCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IconCache").finish_non_exhaustive()
    }
}

struct CacheState {
    dir: PathBuf,
    limit: usize,
    epoch: u64,
    next_tmp: u64,
    /// Ordered from least recently used (`front`) to most recently used (`back`).
    entries: VecDeque<(Vec<u8>, PathBuf)>,
}

impl IconCache {
    /// Creates an icon cache rooted at `dir`, clearing any leftover files from a prior run.
    pub fn new(dir: PathBuf) -> Self {
        Self::with_limit(dir, DEFAULT_CACHE_LIMIT)
    }

    pub fn with_limit(dir: PathBuf, limit: usize) -> Self {
        let cache = Self {
            state: Mutex::new(CacheState {
                dir,
                limit: limit.max(1),
                epoch: 0,
                next_tmp: 0,
                entries: VecDeque::new(),
            }),
        };
        cache.clear();
        cache
    }

    /// Monotonic session epoch, incremented on [`Self::clear`] so stale queued jobs do not
    /// recreate cache files after logout or shutdown.
    pub fn epoch(&self) -> u64 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).epoch
    }

    /// Bumps the cache epoch and removes all cached PNGs and the cache directory (without
    /// following symlinks).
    pub fn clear(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.epoch = state.epoch.wrapping_add(1);
        state.entries.clear();
        let Ok(meta) = std::fs::symlink_metadata(&state.dir) else {
            return;
        };
        if !meta.file_type().is_dir() {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(&state.dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let _ = std::fs::remove_dir(&state.dir);
    }

    /// Ensures a PNG file exists for `icon` when `expected_epoch` still matches the active
    /// session, enforcing `0o700` dir and `0o600` files without following symlinks. Returns
    /// `None` for [`NotifyIcon::Generic`], stale epochs, or any I/O failure.
    pub fn ensure_file(&self, icon: &NotifyIcon, expected_epoch: u64) -> Option<PathBuf> {
        let key = icon_cache_key(icon)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.epoch != expected_epoch {
            return None;
        }
        if let Some(pos) = state.entries.iter().position(|(k, _)| k == &key) {
            let path = state.entries[pos].1.clone();
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file()) {
                let entry = state.entries.remove(pos)?;
                state.entries.push_back(entry);
                return Some(path);
            }
            state.entries.remove(pos);
        }
        if let Err(err) = ensure_safe_dir(&state.dir) {
            tracing::debug!(why = %crate::logsafe::kind(&err), "notification icon directory unavailable");
            return None;
        }
        let filename = cache_filename(&key);
        let path = state.dir.join(&filename);
        if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file()) {
            let png = match encode_icon_png(icon) {
                Ok(bytes) => bytes,
                Err(err) => {
                    tracing::debug!(why = %crate::logsafe::kind(&err), "could not encode notification icon");
                    return None;
                }
            };
            state.next_tmp = state.next_tmp.wrapping_add(1);
            let tmp = state.dir.join(format!(
                ".{filename}.{}.{}.tmp",
                std::process::id(),
                state.next_tmp
            ));
            if let Err(err) =
                write_private_file(&tmp, &png).and_then(|()| std::fs::rename(&tmp, &path))
            {
                let _ = std::fs::remove_file(&tmp);
                tracing::debug!(why = %crate::logsafe::kind(&err), "could not write notification icon file");
                return None;
            }
        }
        state.entries.push_back((key, path.clone()));
        while state.entries.len() > state.limit {
            if let Some((_, old_path)) = state.entries.pop_front() {
                let _ = std::fs::remove_file(old_path);
            }
        }
        Some(path)
    }
}

fn ensure_safe_dir(dir: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => {
            if !meta.file_type().is_dir() {
                return Err(std::io::Error::other("icon cache path is not a directory"));
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir)?;
            let meta = std::fs::symlink_metadata(dir)?;
            if !meta.file_type().is_dir() {
                return Err(std::io::Error::other("icon cache path is not a directory"));
            }
        }
        Err(err) => return Err(err),
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::TILE_SIZE;
    use crate::notify::tests::TempDir;

    #[test]
    fn icon_cache_permissions_lru_eviction_and_cleanup() {
        let root = TempDir::new("icon-cache");
        let dir = root.0.join("cache");
        let cache = IconCache::with_limit(dir.clone(), 2);
        let epoch = cache.epoch();

        let tile_a = NotifyIcon::Tile {
            ch: 'A',
            bg: [10, 20, 30],
            fg: [240, 240, 240],
        };
        let tile_b = NotifyIcon::Tile {
            ch: 'B',
            bg: [20, 30, 40],
            fg: [240, 240, 240],
        };
        let tile_c = NotifyIcon::Tile {
            ch: 'C',
            bg: [30, 40, 50],
            fg: [240, 240, 240],
        };

        let path_a = cache.ensure_file(&tile_a, epoch).unwrap();
        let path_b = cache.ensure_file(&tile_b, epoch).unwrap();
        assert_ne!(path_a, path_b);
        assert_eq!(
            std::fs::symlink_metadata(&dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::symlink_metadata(&path_a)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(image::open(&path_a).unwrap().width(), TILE_SIZE);

        // Touch A so B becomes least recently used, then insert C: B must be evicted.
        assert_eq!(cache.ensure_file(&tile_a, epoch), Some(path_a.clone()));
        let path_c = cache.ensure_file(&tile_c, epoch).unwrap();
        assert!(path_a.exists());
        assert!(!path_b.exists());
        assert!(path_c.exists());

        // Clear wipes files and dir and bumps epoch so stale queued jobs do not rewrite.
        cache.clear();
        assert!(!dir.exists());
        assert_ne!(cache.epoch(), epoch);
        assert_eq!(cache.ensure_file(&tile_a, epoch), None);
        assert!(!dir.exists());
    }
}
