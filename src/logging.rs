//! File logging. A TUI must never write logs to stdout or stderr.

use anyhow::Result;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::paths::Paths;

/// Starts logging to `<log_dir>/matrix-rat.log` (rotated daily).
/// Level comes from `MATRIX_RAT_LOG` (e.g. `debug`, `matrix_sdk=trace`); default `info`.
/// Keep the returned guard alive: dropping it flushes and stops the writer.
pub fn init(paths: &Paths) -> Result<WorkerGuard> {
    let appender = tracing_appender::rolling::daily(&paths.log_dir, "matrix-rat.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_env("MATRIX_RAT_LOG")
        .unwrap_or_else(|_| EnvFilter::new("warn,matrix_rat=info"));
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init()
        .map_err(|err| anyhow::anyhow!("could not start logging: {err}"))?;
    Ok(guard)
}
