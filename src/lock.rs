//! One running instance per profile.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::paths::Paths;

/// Exclusive lock on the profile plus a pid file; both released on drop.
#[derive(Debug)]
pub struct ProfileLock {
    _file: File,
    pid_file: PathBuf,
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

        let pid_file = paths.pid_file();
        let mut pid = File::create(&pid_file)
            .with_context(|| format!("could not write {}", pid_file.display()))?;
        writeln!(pid, "{}", std::process::id())?;

        Ok(Self {
            _file: file,
            pid_file,
        })
    }
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
        assert!(ProfileLock::acquire(&paths).is_err());
        drop(first);
        assert!(!paths.pid_file().exists());
        let again = ProfileLock::acquire(&paths);
        assert!(again.is_ok());
        drop(again);
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }
}
