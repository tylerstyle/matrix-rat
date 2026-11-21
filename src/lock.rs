//! One running instance per profile.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::paths::Paths;

/// Retries when locking the pid file races with `--reload-theme` cleaning up a stale one.
const PID_LOCK_ATTEMPTS: usize = 10;
const PID_LOCK_RETRY: Duration = Duration::from_millis(10);

/// Exclusive lock on the profile plus a pid file; both released on drop.
pub struct ProfileLock {
    _file: File,
    _pid: File,
    pid_file: PathBuf,
}

/// Hand-written: the pid file's path names the user's home folder and the profile.
impl std::fmt::Debug for ProfileLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProfileLock")
    }
}

impl ProfileLock {
    pub fn acquire(paths: &Paths) -> Result<Self> {
        let path = paths.lock_file();
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("could not open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => bail!(
                "matrix-rat is already running with profile {:?}; \
                 use --profile <name> to start a second account",
                paths.profile
            ),
            Err(TryLockError::Error(err)) => {
                return Err(err).with_context(|| format!("could not lock {}", path.display()));
            }
        }

        // The pid file stays open and locked for as long as we run. The lock, which the
        // system drops even if we are killed, is what tells `--reload-theme` that the
        // number in the file still belongs to a matrix-rat. Retry if `--reload-theme`
        // briefly locks or removes a stale pid file while we start up.
        let pid_file = paths.pid_file();
        let mut pid = open_and_lock_pid(&pid_file, || {})?;
        pid.set_len(0)?;
        writeln!(pid, "{}", std::process::id())?;
        pid.flush()?;

        Ok(Self {
            _file: file,
            _pid: pid,
            pid_file,
        })
    }
}

fn open_and_lock_pid(pid_file: &Path, mut between: impl FnMut()) -> Result<File> {
    let mut blocked = false;
    for attempt in 0..PID_LOCK_ATTEMPTS {
        let pid = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(pid_file)
            .with_context(|| format!("could not write {}", pid_file.display()))?;
        if attempt == 0 {
            between();
        }
        match pid.try_lock() {
            Ok(()) => {
                blocked = false;
                if same_file(&pid, pid_file)? {
                    return Ok(pid);
                }
            }
            Err(TryLockError::WouldBlock) => {
                blocked = true;
                // `--reload-theme` may hold the lock on a stale pid file just long
                // enough to remove it. Wait briefly only while that same file is still
                // on disk; if it was already removed, reopen immediately.
                if attempt + 1 < PID_LOCK_ATTEMPTS && same_file(&pid, pid_file)? {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "program startup before any interface exists; bounded wait for --reload-theme"
                    )]
                    std::thread::sleep(PID_LOCK_RETRY);
                }
            }
            Err(TryLockError::Error(err)) => {
                return Err(err).with_context(|| format!("could not lock {}", pid_file.display()));
            }
        }
    }
    if blocked {
        bail!("could not lock {}", pid_file.display());
    }
    bail!(
        "{} was replaced before it could be locked",
        pid_file.display()
    )
}

fn same_file(file: &File, path: &Path) -> Result<bool> {
    let locked = file
        .metadata()
        .with_context(|| format!("could not stat locked {}", path.display()))?;
    let on_disk = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(err).with_context(|| format!("could not stat {}", path.display()));
        }
    };
    Ok(locked.dev() == on_disk.dev() && locked.ino() == on_disk.ino())
}

impl Drop for ProfileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.pid_file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(name: &str) -> Paths {
        let root =
            std::env::temp_dir().join(format!("matrix-rat-test-{name}-{}", std::process::id()));
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "t".into(),
        };
        paths.create_dirs().unwrap();
        paths
    }

    #[test]
    fn second_lock_is_refused_and_pid_file_cleaned() {
        let paths = temp_paths("lock");
        let first = ProfileLock::acquire(&paths).unwrap();
        assert!(paths.pid_file().exists());
        // Where the pid file is (the user's folders) stays out of `Debug`.
        assert_eq!(format!("{first:?}"), "ProfileLock");
        assert!(ProfileLock::acquire(&paths).is_err());
        drop(first);
        assert!(!paths.pid_file().exists());
        let again = ProfileLock::acquire(&paths);
        assert!(again.is_ok());
        drop(again);
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn pid_lock_retries_when_unlinked_or_replaced_before_lock() {
        let paths = temp_paths("lock-race");
        let pid_file = paths.pid_file();

        // Simulate `--reload-theme` unlinking the unlocked pid file between open and try_lock.
        let pid = open_and_lock_pid(&pid_file, || {
            std::fs::remove_file(&pid_file).unwrap();
        })
        .unwrap();
        assert!(same_file(&pid, &pid_file).unwrap());
        // The file on disk must actually be locked by `pid`.
        let probe = OpenOptions::new().write(true).open(&pid_file).unwrap();
        assert!(matches!(probe.try_lock(), Err(TryLockError::WouldBlock)));
        drop(probe);
        drop(pid);
        std::fs::remove_file(&pid_file).unwrap();

        // Simulate replacing the pid file with a new inode between open and try_lock.
        let pid = open_and_lock_pid(&pid_file, || {
            let replacement = paths.runtime_dir.join("replacement.pid");
            std::fs::write(&replacement, b"0\n").unwrap();
            std::fs::rename(&replacement, &pid_file).unwrap();
        })
        .unwrap();
        assert!(same_file(&pid, &pid_file).unwrap());
        drop(pid);

        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn pid_lock_retries_on_would_block_during_reload_unlink() {
        let paths = temp_paths("lock-wouldblock");
        let pid_file = paths.pid_file();

        // 1. `--reload-theme` locks the stale pid file and unlinks it while still holding
        //    the lock on the unlinked descriptor: try_lock returns WouldBlock, and the
        //    retry opens and locks the fresh path without failing.
        let mut held: Option<File> = None;
        let pid = open_and_lock_pid(&pid_file, || {
            let reload = OpenOptions::new().write(true).open(&pid_file).unwrap();
            reload.try_lock().unwrap();
            std::fs::remove_file(&pid_file).unwrap();
            held = Some(reload);
        })
        .unwrap();
        assert!(same_file(&pid, &pid_file).unwrap());
        drop(held);
        drop(pid);
        std::fs::remove_file(&pid_file).unwrap();

        // 2. `--reload-theme` locks the stale pid file and has not yet unlinked it when
        //    try_lock runs; it unlinks and drops the lock shortly after on another thread.
        let mut releaser = None;
        let pid = open_and_lock_pid(&pid_file, || {
            let reload = OpenOptions::new().write(true).open(&pid_file).unwrap();
            reload.try_lock().unwrap();
            let unlink_path = pid_file.clone();
            releaser = Some(std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(20));
                let _ = std::fs::remove_file(&unlink_path);
                drop(reload);
            }));
        })
        .unwrap();
        if let Some(handle) = releaser {
            handle.join().unwrap();
        }
        assert!(same_file(&pid, &pid_file).unwrap());
        drop(pid);

        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }
}
