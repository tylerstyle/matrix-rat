//! matrix-rat — a mouse-friendly Matrix client for the terminal.

mod app;
mod lock;
mod logging;
mod paths;
mod terminal;

use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// Profile to use; each profile has its own account, config and store.
    #[arg(long, default_value = "default")]
    profile: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let paths = paths::Paths::new(&args.profile)?;
    paths.create_dirs()?;

    // Held for the whole run: two instances on one profile would corrupt the SDK stores.
    let _lock = lock::ProfileLock::acquire(&paths)?;
    let _log_guard = logging::init(&paths)?;
    tracing::info!(profile = %args.profile, version = env!("CARGO_PKG_VERSION"), "starting");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(app::run());
    if let Err(err) = &result {
        tracing::error!(?err, "exited with error");
    }
    result
}
