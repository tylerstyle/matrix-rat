//! File logging. A TUI must never write logs to stdout or stderr.

use anyhow::Result;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::paths::Paths;

/// What is logged unless `MATRIX_RAT_LOG` says otherwise: this program's own notes, and
/// nothing from the libraries. Most of the SDK's warnings and errors carry room, user,
/// device or event ids, or an error as it came (in matrix-sdk 0.20 about three in four),
/// and the spans around them add more ids to every line; so do new crates of the SDK, and
/// a filter by target cannot name them in advance. This program logs every failure it
/// handles itself, by kind and without those.
const DEFAULT_FILTER: &str = "off,matrix_rat=info";

/// Starts logging to `<log_dir>/matrix-rat.log` (rotated daily).
/// Level comes from `MATRIX_RAT_LOG` (e.g. `matrix_rat=debug`, `matrix_rat=info,matrix_sdk=warn`);
/// default [`DEFAULT_FILTER`]. What a library writes then goes in as it came, ids and all.
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

    /// The default filter parses, keeps out every line of the libraries, whatever its
    /// level and crate (also ones the SDK adds later), and lets this program's notes in.
    #[test]
    fn the_default_filter_keeps_out_the_libraries() {
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
            tracing::warn!(target: "matrix_sdk_crypto::identities::manager", "own device");
            tracing::error!(target: "matrix_sdk_ui::sync_service", "sync error");
            tracing::error!(target: "matrix_sdk_future_crate", "new crate");
            tracing::warn!(target: "hyper_util::client", "connection");
            tracing::info!(target: "matrix_sdk::latest_events", "sdk note");
            tracing::info!(target: "matrix_rat::logging", "own note");
            tracing::warn!(target: "matrix_rat::matrix", "own warning");
            tracing::debug!(target: "matrix_rat::matrix", "own detail");
        });
        let log = String::from_utf8(written.0.lock().expect("not poisoned").clone())
            .expect("the log is text");
        for gone in [
            "index made anew",
            "index error",
            "server answer",
            "push rule",
            "own device",
            "sync error",
            "new crate",
            "connection",
            "sdk note",
            "own detail",
        ] {
            assert!(!log.contains(gone), "{gone} was logged");
        }
        for kept in ["own note", "own warning"] {
            assert!(log.contains(kept), "{kept} was not logged");
        }
    }
}
