//! File logging. A TUI must never write logs to stdout or stderr.

use anyhow::Result;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::paths::Paths;

/// What is logged unless `MATRIX_RAT_LOG` says otherwise: warnings, and this program's
/// own notes. The SDK's request layer is left out: it writes the server's answer to a
/// failed request into the log as it came, room ids and all, and a lookup that finds
/// nothing (a room without pinned messages) is such a failure. This program logs its
/// own failures, without those. So for the SDK's notification settings: when a push rule
/// cannot be changed, they log its id, and the id of a keyword's rule is the keyword. And
/// for the search index's builder (matrix-sdk 0.20): when it cannot open a room's index
/// and makes it anew, once for every room after the upgrade to 0.20 and after a key
/// change, it logs the room's id and the error as it came; it logs nothing else.
const DEFAULT_FILTER: &str = "warn,matrix_rat=info,matrix_sdk::http_client=off,\
    matrix_sdk::notification_settings=off,matrix_sdk_search::index::builder=off";

/// Starts logging to `<log_dir>/matrix-rat.log` (rotated daily).
/// Level comes from `MATRIX_RAT_LOG` (e.g. `debug`, `matrix_sdk=trace`); default `info`.
/// Keep the returned guard alive: dropping it flushes and stops the writer.
pub fn init(paths: &Paths) -> Result<WorkerGuard> {
    let appender = tracing_appender::rolling::daily(&paths.log_dir, "matrix-rat.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_env("MATRIX_RAT_LOG")
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init()
        .map_err(|err| anyhow::anyhow!("could not start logging: {err}"))?;
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::EnvFilter;

    use super::DEFAULT_FILTER;

    /// What was written to the log, kept in memory.
    #[derive(Clone, Default)]
    struct Written(Arc<Mutex<Vec<u8>>>);

    impl Write for Written {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("not poisoned").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The default filter parses, and keeps out the SDK's lines that carry ids or the
    /// error as it came; the SDK's other warnings and this program's notes go in.
    #[test]
    fn the_default_filter_keeps_out_the_sdk_lines_with_ids() {
        let written = Written::default();
        let writer = written.clone();
        let filter = EnvFilter::try_new(DEFAULT_FILTER).expect("the default filter parses");
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_env_filter(filter)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: "matrix_sdk_search::index::builder", "index made anew");
            tracing::error!(target: "matrix_sdk_search::index::builder", "index error");
            tracing::warn!(target: "matrix_sdk::http_client", "server answer");
            tracing::warn!(target: "matrix_sdk::notification_settings", "push rule");
            tracing::warn!(target: "matrix_sdk::latest_events", "sdk warning");
            tracing::info!(target: "matrix_sdk::latest_events", "sdk note");
            tracing::info!(target: "matrix_rat::logging", "own note");
        });
        let log = String::from_utf8(written.0.lock().expect("not poisoned").clone())
            .expect("the log is text");
        for gone in [
            "index made anew",
            "index error",
            "server answer",
            "push rule",
            "sdk note",
        ] {
            assert!(!log.contains(gone), "{gone} was logged");
        }
        for kept in ["sdk warning", "own note"] {
            assert!(log.contains(kept), "{kept} was not logged");
        }
    }
}
