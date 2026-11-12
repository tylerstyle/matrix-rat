//! Live theme reload: watches the config and themes directories, and listens for
//! SIGUSR1 (sent by `matrix-rat --reload-theme`, for theme generators' hooks).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::paths::Paths;

/// Tools write a theme in several steps (temp file, rename); wait for them to finish.
const SETTLE: Duration = Duration::from_millis(120);

/// Yields `()` whenever the theme should be loaded again. Keep the watcher alive.
pub struct ThemeWatch {
    _watcher: Option<RecommendedWatcher>,
    changes: UnboundedReceiver<()>,
    /// When the current burst of changes is considered over. Kept here, not in the
    /// future, so that a change is never lost when `changed()` is cancelled.
    settle_at: Option<tokio::time::Instant>,
}

impl ThemeWatch {
    pub fn start(paths: &Paths) -> Result<Self> {
        let (sender, changes) = unbounded_channel();

        // Directories are watched rather than files: generators and editors replace
        // files by renaming, which a watch on the old file would never see.
        std::fs::create_dir_all(paths.themes_dir()).ok();
        let file_sender = sender.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let relevant = event.is_ok_and(|event| {
                    !event.kind.is_access()
                        && event
                            .paths
                            .iter()
                            .any(|path| path.extension().is_some_and(|ext| ext == "toml"))
                });
                if relevant {
                    let _ = file_sender.send(());
                }
            })
            .ok();
        if let Some(watcher) = watcher.as_mut() {
            for dir in [paths.config_dir.as_path(), paths.themes_dir().as_path()] {
                if let Err(err) = watch(watcher, dir) {
                    tracing::warn!(?err, "themes will not reload by themselves");
                }
            }
        }

        let mut usr1 =
            signal(SignalKind::user_defined1()).context("could not listen for SIGUSR1")?;
        tokio::spawn(async move {
            while usr1.recv().await.is_some() {
                if sender.send(()).is_err() {
                    return;
                }
            }
        });

        Ok(Self {
            _watcher: watcher,
            changes,
            settle_at: None,
        })
    }

    /// Waits for a change, then briefly for the burst of changes around it to end.
    ///
    /// Safe to cancel and call again (it is one branch of the event loop's `select!`):
    /// a change that was already received is remembered in `settle_at`.
    pub async fn changed(&mut self) {
        let settle_at = match self.settle_at {
            Some(at) => at,
            None => {
                if self.changes.recv().await.is_none() {
                    // Nothing can ever arrive; never resolve.
                    std::future::pending::<()>().await;
                }
                let at = tokio::time::Instant::now() + SETTLE;
                self.settle_at = Some(at);
                at
            }
        };
        tokio::time::sleep_until(settle_at).await;
        self.settle_at = None;
        // Whatever arrived meanwhile is covered by the reload that follows.
        while self.changes.try_recv().is_ok() {}
    }
}

fn watch(watcher: &mut RecommendedWatcher, dir: &Path) -> Result<()> {
    watcher
        .watch(dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("could not watch {}", dir.display()))
}

/// `matrix-rat --reload-theme`: asks every running instance to reload its theme.
/// Returns how many were told.
///
/// A pid file alone proves nothing: after a crash the number may belong to another
/// program, and SIGUSR1 would end it. A running instance keeps its pid file locked, so
/// only files that are locked right now are trusted, and the process is checked to be
/// this program before it is signalled.
pub fn signal_running_instances(paths: &Paths) -> usize {
    let Ok(entries) = std::fs::read_dir(&paths.runtime_dir) else {
        return 0;
    };
    let mut told = 0;
    for path in entries.flatten().map(|entry| entry.path()) {
        if path.extension().is_none_or(|ext| ext != "pid") {
            continue;
        }
        let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path) else {
            continue;
        };
        match file.try_lock() {
            // Nobody holds it: left behind by an instance that is gone.
            Ok(()) => {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(_)) => continue,
        }
        // Only ever a plain process number, never anything else from the file.
        let pid = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
            .filter(|pid| *pid > 1);
        let Some(pid) = pid else { continue };
        if !is_this_program(pid) {
            continue;
        }
        let sent = std::process::Command::new("kill")
            .args(["-USR1", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if sent {
            told += 1;
        }
    }
    told
}

/// Whether process `pid` is a matrix-rat. Where the system cannot tell us (no /proc),
/// the lock on the pid file has to be proof enough.
fn is_this_program(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        Ok(name) => name.trim() == env!("CARGO_PKG_NAME"),
        Err(_) => !std::path::Path::new("/proc/self").exists(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_changes_and_renames_are_noticed() {
        let root = std::env::temp_dir().join(format!("matrix-rat-watch-{}", std::process::id()));
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "t".into(),
        };
        paths.create_dirs().unwrap();
        let mut watch = ThemeWatch::start(&paths).unwrap();
        let theme = paths.themes_dir().join("x.toml");

        // A plain write.
        std::fs::write(&theme, "[palette]\n").unwrap();
        tokio::time::timeout(Duration::from_secs(5), watch.changed())
            .await
            .expect("write was not noticed");

        // Replace by rename, as generators and editors do; twice, to prove the watch
        // survives the first replacement.
        for n in 0..2 {
            let temp = paths.themes_dir().join("x.toml.tmp");
            std::fs::write(&temp, format!("# {n}\n[palette]\n")).unwrap();
            std::fs::rename(&temp, &theme).unwrap();
            tokio::time::timeout(Duration::from_secs(5), watch.changed())
                .await
                .expect("rename was not noticed");
        }

        // Unrelated files do not trigger a reload.
        std::fs::write(paths.themes_dir().join("notes.txt"), "x").unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(600), watch.changed())
                .await
                .is_err()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_change_survives_cancellation() {
        let (sender, changes) = unbounded_channel();
        let mut watch = ThemeWatch {
            _watcher: None,
            changes,
            settle_at: None,
        };
        sender.send(()).unwrap();
        // Cancelled during the settle time, as happens when another event arrives in
        // the event loop...
        assert!(
            tokio::time::timeout(Duration::from_millis(20), watch.changed())
                .await
                .is_err()
        );
        // ...the change is still reported by the next call.
        tokio::time::timeout(Duration::from_secs(2), watch.changed())
            .await
            .expect("the change was lost");
        // And only once.
        assert!(
            tokio::time::timeout(Duration::from_millis(300), watch.changed())
                .await
                .is_err()
        );
    }

    #[test]
    fn only_locked_pid_files_of_this_program_are_signalled() {
        let root = std::env::temp_dir().join(format!("matrix-rat-pids-{}", std::process::id()));
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "t".into(),
        };
        paths.create_dirs().unwrap();
        // Leftovers: a live but unrelated process (this test), nonsense, a dead pid.
        std::fs::write(
            paths.runtime_dir.join("a.pid"),
            format!("{}\n", std::process::id()),
        )
        .unwrap();
        std::fs::write(paths.runtime_dir.join("b.pid"), "-1; rm -rf /\n").unwrap();
        std::fs::write(paths.runtime_dir.join("c.pid"), "4194000\n").unwrap();
        std::fs::write(paths.runtime_dir.join("d.txt"), "123\n").unwrap();
        // Nothing is signalled (we are still alive to assert it), leftovers are removed.
        assert_eq!(signal_running_instances(&paths), 0);
        for name in ["a.pid", "b.pid", "c.pid"] {
            assert!(!paths.runtime_dir.join(name).exists(), "{name}");
        }
        assert!(paths.runtime_dir.join("d.txt").exists());

        // A held lock marks a live instance: its file is never removed. It is not
        // signalled here either, because this test binary is not matrix-rat.
        let lock = crate::lock::ProfileLock::acquire(&paths).unwrap();
        assert_eq!(signal_running_instances(&paths), 0);
        assert!(paths.pid_file().exists());
        drop(lock);
        let _ = std::fs::remove_dir_all(root);
    }
}
