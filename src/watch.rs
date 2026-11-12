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
        })
    }

    /// Waits for a change, then for the burst of changes around it to end.
    pub async fn changed(&mut self) {
        if self.changes.recv().await.is_none() {
            // Nothing can ever arrive; never resolve.
            std::future::pending::<()>().await;
        }
        loop {
            tokio::time::sleep(SETTLE).await;
            let mut more = false;
            while self.changes.try_recv().is_ok() {
                more = true;
            }
            if !more {
                return;
            }
        }
    }
}

fn watch(watcher: &mut RecommendedWatcher, dir: &Path) -> Result<()> {
    watcher
        .watch(dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("could not watch {}", dir.display()))
}

/// `matrix-rat --reload-theme`: asks every running instance to reload its theme.
/// Returns how many were told.
pub fn signal_running_instances(paths: &Paths) -> usize {
    let Ok(entries) = std::fs::read_dir(&paths.runtime_dir) else {
        return 0;
    };
    let mut told = 0;
    for path in entries.flatten().map(|entry| entry.path()) {
        if path.extension().is_none_or(|ext| ext != "pid") {
            continue;
        }
        // Only ever a plain process number, never anything else from the file.
        let pid = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
            .filter(|pid| *pid > 1);
        let Some(pid) = pid else { continue };
        let sent = std::process::Command::new("kill")
            .args(["-USR1", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if sent {
            told += 1;
        } else {
            // A leftover from an instance that did not exit cleanly.
            let _ = std::fs::remove_file(&path);
        }
    }
    told
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

    #[test]
    fn stale_and_bogus_pid_files_are_ignored() {
        let root = std::env::temp_dir().join(format!("matrix-rat-pids-{}", std::process::id()));
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "t".into(),
        };
        paths.create_dirs().unwrap();
        std::fs::write(paths.runtime_dir.join("a.pid"), "-1; rm -rf /\n").unwrap();
        std::fs::write(paths.runtime_dir.join("b.pid"), "1\n").unwrap();
        std::fs::write(paths.runtime_dir.join("c.pid"), "4194000\n").unwrap();
        std::fs::write(paths.runtime_dir.join("d.txt"), "123\n").unwrap();
        assert_eq!(signal_running_instances(&paths), 0);
        // The dead instance's file was cleaned up; the malformed ones were not touched.
        assert!(!paths.runtime_dir.join("c.pid").exists());
        assert!(paths.runtime_dir.join("a.pid").exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
