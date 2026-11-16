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
mod doctor;
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
#[cfg(test)]
mod testbus;
mod text;
mod theme;
mod ui;
mod upload;
mod verify;
mod watch;

use std::fmt;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Profile to use; each profile has its own account, message store and log. The
    /// configuration and themes are shared between profiles.
    #[arg(long, global = true, default_value = "default")]
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

// A word in the place of the LINK: a Matrix link or address is never one of these
// (they start with `https:`, `matrix:`, `#`, `!` or `@`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
enum Command {
    /// Print what matrix-rat finds here (terminal, pictures, clipboard, notifications,
    /// keys, config) and exit. Nothing private is in it: paste it into a bug report.
    /// Takes no lock and logs nobody in, so it also runs beside a running matrix-rat.
    Doctor,
}

/// Redacts the target link/address from Debug output.
impl fmt::Debug for Args {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Args")
            .field("command", &self.command)
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
    if args.command == Some(Command::Doctor) {
        // Before anything is made or locked: it only reads the config and asks the
        // terminal.
        print!("{}", doctor::command_line(&paths.config_file()));
        return Ok(());
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(words: &[&str]) -> Args {
        Args::try_parse_from(std::iter::once("matrix-rat").chain(words.iter().copied()))
            .unwrap_or_else(|err| panic!("{words:?}: {err}"))
    }

    #[test]
    fn doctor_is_a_word_of_its_own_and_links_still_open() {
        Args::command().debug_assert();
        let args = parse(&["doctor"]);
        assert_eq!(args.command, Some(Command::Doctor));
        assert_eq!(args.target, None);
        let args = parse(&["--profile", "work", "doctor"]);
        assert_eq!(args.command, Some(Command::Doctor));
        assert_eq!(args.profile, "work");
        for link in [
            "#rust:matrix.org",
            "!room:example.org",
            "@bob:example.org",
            "https://matrix.to/#/#rust:matrix.org",
            "matrix:r/rust:matrix.org",
        ] {
            let args = parse(&[link]);
            assert_eq!(args.command, None, "{link}");
            assert_eq!(args.target.as_deref(), Some(link));
        }
        let args = parse(&["--demo"]);
        assert_eq!((args.command, args.demo), (None, true));
        // The link stays out of debug output; the command is no secret.
        let shown = format!("{:?}", parse(&["@bob:example.org"]));
        assert!(!shown.contains("bob"), "{shown}");
        assert!(format!("{:?}", parse(&["doctor"])).contains("Doctor"));
    }
}
