#![recursion_limit = "256"]
//! matrix-rat — a mouse-friendly Matrix client for the terminal.

mod action;
mod app;
mod composer;
mod config;
mod emoji;
mod format;
mod images;
mod keymap;
mod lock;
mod logging;
mod matrix;
mod modal;
mod model;
mod paths;
mod rich;
mod terminal;
mod text;
mod theme;
mod ui;
mod verify;
mod watch;

use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// Profile to use; each profile has its own account, config and store.
    #[arg(long, default_value = "default")]
    profile: String,

    /// Run on built-in demo data without connecting to Matrix.
    #[arg(long)]
    demo: bool,

    /// Tell running instances to reload their theme, then exit. For the hooks of
    /// theme generators.
    #[arg(long)]
    reload_theme: bool,

    /// Print a theme template for a generator (noctalia, matugen, wallust, pywal)
    /// and exit.
    #[arg(long, value_name = "GENERATOR")]
    print_template: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(generator) = &args.print_template {
        let Some((_, template)) = theme::TEMPLATES.iter().find(|(name, _)| name == generator)
        else {
            let known: Vec<&str> = theme::TEMPLATES.iter().map(|(name, _)| *name).collect();
            anyhow::bail!(
                "no template for {generator:?}; available: {}",
                known.join(", ")
            );
        };
        print!("{template}");
        return Ok(());
    }
    let paths = paths::Paths::new(&args.profile)?;
    if args.reload_theme {
        // Works without the profile lock: the running instances hold it.
        watch::signal_running_instances(&paths);
        return Ok(());
    }
    paths.create_dirs()?;
    let config = config::Config::load(&paths.config_file())?;

    // Held for the whole run: two instances on one profile would corrupt the SDK stores.
    let _lock = lock::ProfileLock::acquire(&paths)?;
    let _log_guard = logging::init(&paths)?;
    tracing::info!(profile = %args.profile, version = env!("CARGO_PKG_VERSION"), "starting");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(app::run(&paths, &config, args.demo));
    if let Err(err) = &result {
        tracing::error!(?err, "exited with error");
    }
    result
}
