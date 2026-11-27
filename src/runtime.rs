//! The tokio runtime the program runs on: the interactive app's and `matrix-rat serve`'s,
//! each with its worker and blocking pools bounded. (The tasks and timers of the core
//! are [`crate::rt`]'s.)

/// Maximum tokio async worker threads for the interactive TUI (the UI loop itself also
/// runs on the main thread via `block_on`). Capping at 4 avoids spawning one idle worker
/// per logical CPU on high-core workstations (e.g. 20 threads on a 20-core host, 2 MiB
/// virtual stack each) while keeping ample headroom for sync, decryption, timeline updates
/// and parallel media downloads.
const INTERACTIVE_WORKERS: usize = 4;
/// Maximum tokio async worker threads for `matrix-rat serve`: a headless daemon driving
/// one Matrix session and local Unix sockets needs at most 2 workers alongside `block_on`.
const SERVE_WORKERS: usize = 2;
/// Maximum tokio blocking-pool threads (`spawn_blocking`: SQLite queries, image decoding,
/// file I/O, DNS, `$EDITOR`). Tokio defaults to 512, which allows a sync burst to spawn
/// dozens of threads (2 MiB virtual stack each); 16 covers full parallel media decoding
/// (4 pictures + 2 avatars + 2 emotes) plus concurrent SQLite and I/O without stalls.
const INTERACTIVE_BLOCKING: usize = 16;
/// Maximum tokio blocking-pool threads for `matrix-rat serve` (no image decoding or editor).
const SERVE_BLOCKING: usize = 8;

/// Builds the tokio runtime with bounded worker and blocking thread pools.
pub fn build_runtime(serve: bool) -> std::io::Result<tokio::runtime::Runtime> {
    let available = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(2);
    let (max_workers, max_blocking) = if serve {
        (SERVE_WORKERS, SERVE_BLOCKING)
    } else {
        (INTERACTIVE_WORKERS, INTERACTIVE_BLOCKING)
    };
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(available.clamp(1, max_workers))
        .max_blocking_threads(max_blocking)
        .enable_all()
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{matrix, paths, watch};

    #[test]
    fn runtime_caps_workers_and_blocking_pool() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let available = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(2);

        let interactive = build_runtime(false).unwrap();
        assert_eq!(
            interactive.metrics().num_workers(),
            available.clamp(1, INTERACTIVE_WORKERS)
        );
        drop(interactive);

        let serve = build_runtime(true).unwrap();
        assert_eq!(
            serve.metrics().num_workers(),
            available.clamp(1, SERVE_WORKERS)
        );

        // Spawn more blocking tasks than the serve cap; peak concurrency must stay within
        // `SERVE_BLOCKING` while every queued task still runs to completion.
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        serve.block_on(async {
            let mut handles = Vec::with_capacity(SERVE_BLOCKING + 4);
            for _ in 0..(SERVE_BLOCKING + 4) {
                let active = active.clone();
                let peak = peak.clone();
                handles.push(tokio::task::spawn_blocking(move || {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(40));
                    active.fetch_sub(1, Ordering::SeqCst);
                }));
            }
            for handle in handles {
                handle.await.unwrap();
            }
        });
        assert!(peak.load(Ordering::SeqCst) <= SERVE_BLOCKING);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    /// Measures process thread names (`/proc/self/task/*/comm`) and open file descriptors
    /// (`/proc/self/fd`) without a homeserver, comparing uncapped defaults against the
    /// bounded runtime, SQLite pool cap, and `CrossProcessLockConfig::SingleProcess`.
    /// Run alone so parallel unit tests do not add threads:
    /// `cargo test measure_threads_and_fds_at_rest -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore = "measures process threads and FDs in isolation"]
    fn measure_threads_and_fds_at_rest() {
        use std::collections::BTreeMap;
        use std::time::Duration;

        use matrix_sdk::Client;
        use matrix_sdk_base::cross_process_lock::CrossProcessLockConfig;

        fn snapshot(label: &str) {
            let mut comms: BTreeMap<String, usize> = BTreeMap::new();
            let mut total = 0usize;
            if let Ok(entries) = std::fs::read_dir("/proc/self/task") {
                for entry in entries.flatten() {
                    let comm = std::fs::read_to_string(entry.path().join("comm"))
                        .unwrap_or_else(|_| "?".to_owned());
                    *comms.entry(comm.trim().to_owned()).or_default() += 1;
                    total += 1;
                }
            }
            let fds = std::fs::read_dir("/proc/self/fd")
                .map(|it| it.count())
                .unwrap_or(0);
            let rss_kb = std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.starts_with("VmRSS:"))
                        .map(|l| l.trim().to_owned())
                })
                .unwrap_or_default();
            println!("[{label}] threads={total} fds={fds} {rss_kb} comms={comms:?}");
        }

        let root = std::env::temp_dir().join(format!("mr-threads-measure-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let make_paths = |name: &str| {
            let base = root.join(name);
            let paths = paths::Paths {
                config_dir: base.join("config"),
                data_dir: base.join("data"),
                log_dir: base.join("log"),
                runtime_dir: base.join("run"),
                profile: name.into(),
            };
            paths.create_dirs().unwrap();
            paths
        };

        // Run a scenario inside a dedicated runtime, exercising concurrent SQLite reads +
        // the event-cache cross-process lock, then waiting past tokio's 10s keep-alive.
        let run_scenario = |name: &str,
                            runtime: tokio::runtime::Runtime,
                            capped_sqlite: bool,
                            single_process: bool,
                            interactive_helpers: bool| {
            let paths = make_paths(name);
            let (_log_writer, _log_guard) = tracing_appender::non_blocking(std::io::sink());
            runtime.block_on(async {
                let _theme = interactive_helpers
                    .then(|| watch::ThemeWatch::start(&paths).ok())
                    .flatten();
                let _events = (interactive_helpers
                    && std::io::IsTerminal::is_terminal(&std::io::stdin()))
                .then(crossterm::event::EventStream::new);

                let mut builder = Client::builder()
                    .homeserver_url("http://127.0.0.1:1")
                    .search_index_store(
                        matrix_sdk::search_index::SearchIndexStoreKind::EncryptedDirectory(
                            paths.search_dir(),
                            "measure-passphrase".to_owned(),
                        ),
                    );
                if capped_sqlite {
                    builder = builder.sqlite_store_with_config_and_cache_path(
                        matrix::sqlite_store_config(&paths, "measure-passphrase"),
                        None::<std::path::PathBuf>,
                    );
                } else {
                    builder = builder.sqlite_store(paths.store_dir(), Some("measure-passphrase"));
                }
                if single_process {
                    builder =
                        builder.cross_process_store_config(CrossProcessLockConfig::SingleProcess);
                }
                let client = builder.build().await.unwrap();
                snapshot(&format!("{name}:after-open"));

                // Hold the event-cache cross-process lock while fanning out concurrent
                // SQLite reads across state, event-cache, and media stores (mimicking
                // initial sync fanout + SyncService lease loop).
                let _lock_guard = client.event_cache_store().lock().await.unwrap();
                let mut futs = Vec::new();
                for i in 0..48usize {
                    let client = client.clone();
                    futs.push(tokio::spawn(async move {
                        let name = format!("k{i}");
                        let key = matrix_sdk_base::StateStoreDataKey::Filter(&name);
                        let _ = client.state_store().get_kv_data(key).await;
                    }));
                }
                for _ in 0..16usize {
                    let client = client.clone();
                    futs.push(tokio::spawn(async move {
                        let _ = client
                            .state_store()
                            .get_room_infos(&Default::default())
                            .await;
                    }));
                }
                for _ in 0..16usize {
                    let client = client.clone();
                    futs.push(tokio::spawn(async move {
                        if let Ok(store) = client.media_store().lock().await {
                            let _ = store.clean().await;
                        }
                    }));
                }
                for fut in futs {
                    let _ = fut.await;
                }
                snapshot(&format!("{name}:after-burst"));

                // Wait past tokio's 10s blocking-pool keep_alive while the lock stays held.
                tokio::time::sleep(Duration::from_secs(12)).await;
                snapshot(&format!("{name}:after-12s-rest"));
            });
            drop(runtime);
            std::thread::sleep(Duration::from_millis(200));
        };

        snapshot("baseline-before");

        // 1. Uncapped defaults (exact pre-Task-8 setup on this machine).
        let uncapped = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        run_scenario("uncapped-multiprocess", uncapped, false, false, true);

        // 2. Runtime caps + SQLite pool cap, still with SDK default MultiProcess lock.
        run_scenario(
            "capped-multiprocess",
            build_runtime(false).unwrap(),
            true,
            false,
            true,
        );

        // 3. Runtime caps + SQLite pool cap + CrossProcessLockConfig::SingleProcess.
        run_scenario(
            "capped-singleprocess",
            build_runtime(false).unwrap(),
            true,
            true,
            true,
        );

        // 4. `serve` runtime caps + SQLite pool cap + SingleProcess (no TUI helpers).
        run_scenario(
            "serve-capped-singleprocess",
            build_runtime(true).unwrap(),
            true,
            true,
            false,
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
