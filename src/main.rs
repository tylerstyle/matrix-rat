#![recursion_limit = "256"]
//! matrix-rat — a mouse-friendly Matrix client for the terminal.

mod action;
mod app;
mod clipboard;
mod complete;
mod composer;
mod config;
mod details;
mod devices;
mod editor;
mod emoji;
mod files;
mod focus;
mod format;
mod highlight;
mod images;
mod keymap;
mod keywords;
mod lineedit;
mod lock;
mod logging;
mod logsafe;
mod manage;
mod matrix;
mod menu;
mod modal;
mod model;
mod notify;
mod palette;
mod paths;
mod people;
mod portal;
mod previews;
mod rich;
mod terminal;
mod termquery;
mod text;
mod theme;
mod ui;
mod upload;
mod verify;
mod watch;

use std::fmt;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Profile to use; each profile has its own account, message store and log. The
    /// configuration and themes are shared between profiles.
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

    /// Matrix link or address to open after connecting (matrix.to, matrix:, room,
    /// alias, or user).
    #[arg(value_name = "LINK")]
    target: Option<String>,
}

/// Redacts the target link/address from Debug output.
impl fmt::Debug for Args {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Args")
            .field("profile", &self.profile)
            .field("demo", &self.demo)
            .field("reload_theme", &self.reload_theme)
            .field("print_template", &self.print_template)
            .field("has_target", &self.target.is_some())
            .finish()
    }
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
    notify::set_app_icon(config.notification_icon);

    // Held for the whole run: two instances on one profile would corrupt the SDK stores.
    let _lock = lock::ProfileLock::acquire(&paths)?;
    let _log_guard = logging::init(&paths)?;
    // Not the profile's name: it is the name of the log's own directory already.
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(app::run(&paths, &config, args.demo, args.target));
    if let Err(err) = &result {
        tracing::error!(why = %logsafe::kind_any(err), "exited with error");
    }
    result
}
