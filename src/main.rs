#![recursion_limit = "256"]
// A test may wait for what it tests; everywhere else `clippy.toml` refuses calls that
// block their thread unless the place says why it may.
#![cfg_attr(test, allow(clippy::disallowed_methods))]
// Every match on one of our own enums names its variants, so adding one flags every
// place that must decide about it. Foreign enums (and tests) are exempt via `#[expect]`
// or the test-only allow below.
#![warn(clippy::wildcard_enum_match_arm)]
#![cfg_attr(test, allow(clippy::wildcard_enum_match_arm))]
//! matrix-rat — a mouse-friendly Matrix client for the terminal.

mod action;
mod agent;
mod app;
mod archive;
mod audio;
mod catchup;
mod clipboard;
mod collect;
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
#[cfg(test)]
mod measure;
mod menu;
mod modal;
mod model;
mod notes;
mod notify;
mod palette;
mod paths;
mod people;
mod portal;
mod previews;
mod rich;
mod saved;
mod terminal;
mod termquery;
#[cfg(test)]
mod testbus;
mod text;
mod textcmd;
mod theme;
mod time;
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
#[derive(Clone, PartialEq, Eq, Subcommand)]
enum Command {
    /// Print what matrix-rat finds here (terminal, pictures, clipboard, notifications,
    /// keys, config) and exit. Nothing private is in it: paste it into a bug report.
    /// Takes no lock and logs nobody in, so it also runs beside a running matrix-rat.
    Doctor,
    /// Run without the screen, for agents and scripts: stay in sync on a profile
    /// that is already logged in, and serve `rooms`, `send`, `read`, `listen` and
    /// `mcp` over a private socket. Accepts no invitation and never asks for a login.
    /// Ends on SIGTERM, SIGHUP or Ctrl-C.
    Serve,
    /// Print the rooms the account is in (id, name, encrypted, direct), one JSON
    /// object per line, through a running `matrix-rat serve`. Exit codes as for
    /// `send`.
    Rooms,
    /// Send a message through a running `matrix-rat serve`. Exit codes: 0 sent,
    /// 2 wrong usage, 3 no `serve` is running for the profile, 4 `serve` refused.
    Send {
        /// The room: its id, its exact name, or its address.
        #[arg(long)]
        room: String,
        /// The text (Markdown), after the options; a single `-` reads it from
        /// standard input.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        text: Vec<String>,
    },
    /// Print the latest messages of a room, oldest first, one JSON object per line,
    /// through a running `matrix-rat serve`. Exit codes as for `send`.
    Read {
        /// The room: its id, its exact name, or its address.
        #[arg(long)]
        room: String,
        /// How many messages (1 to 100).
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=100))]
        limit: u32,
        /// Only messages that count as this user's (`@user:server`), among the
        /// latest 100. Give it again for each other account of the same person: a
        /// message from any of them counts, each held to the same rule on its own.
        #[arg(long, value_name = "USER")]
        from: Vec<String>,
        /// With `--from`: also take messages whose sender is not verified (see the
        /// README), from each account alike. Without this, a message counts as that
        /// user's only if it came end-to-end encrypted from a device and an identity
        /// that are verified (each account's own).
        #[arg(long, requires = "from")]
        allow_unverified: bool,
    },
    /// Stay connected to a running `matrix-rat serve` and print messages of a room
    /// as they come (and again when they change), one JSON object per line.
    Listen {
        /// The room: its id, its exact name, or its address.
        #[arg(long)]
        room: String,
        /// Only messages that count as this user's (`@user:server`). Give it again
        /// for each other account of the same person: a message from any of them
        /// counts, each held to the same rule on its own.
        #[arg(long, value_name = "USER")]
        from: Vec<String>,
        /// With `--from`: also take messages whose sender is not verified (see the
        /// README), from each account alike. Without this, a message counts as that
        /// user's only if it came end-to-end encrypted from a device and an identity
        /// that are verified (each account's own).
        #[arg(long, requires = "from")]
        allow_unverified: bool,
    },
    /// An MCP server on standard input and output for one agent, bound to one room,
    /// through a running `matrix-rat serve`: tools to send, to read, and to wait for
    /// a message. No tool can reach another room.
    Mcp {
        /// The one room: its id (best), its exact name, or its address.
        #[arg(long)]
        room: String,
        /// The agent's name; its messages are signed with it: `[NAME] text`. Without
        /// this it is read from the environment variable MATRIX_RAT_AGENT_NAME, so
        /// that agents sharing one command line sign with their own names.
        #[arg(long)]
        name: Option<String>,
        /// The user whose messages count as answers (`@user:server`). Give it again
        /// for each other account of the same person: a message from any of them
        /// counts, each held to the same rule on its own. Everything else in the
        /// room is marked as not from the user.
        #[arg(long, value_name = "USER")]
        from: Vec<String>,
        /// With `--from`: also take messages whose sender is not verified (see the
        /// README), from each account alike. Without this, a message counts as that
        /// user's only if it came end-to-end encrypted from a device and an identity
        /// that are verified (each account's own).
        #[arg(long, requires = "from")]
        allow_unverified: bool,
    },
}

/// Hand-written: a room, a user and a text are private; which command it is, is not.
impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Doctor => "Doctor",
            Self::Serve => "Serve",
            Self::Rooms => "Rooms",
            Self::Send { .. } => "Send",
            Self::Read { .. } => "Read",
            Self::Listen { .. } => "Listen",
            Self::Mcp { .. } => "Mcp",
        })
    }
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
fn build_runtime(serve: bool) -> std::io::Result<tokio::runtime::Runtime> {
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

#[expect(
    clippy::disallowed_methods,
    reason = "the entry point: the main thread is the runtime's"
)]
fn main() -> Result<std::process::ExitCode> {
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
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let paths = paths::Paths::new(&args.profile)?;
    let serve = match args.command {
        Some(Command::Doctor) => {
            // Before anything is made or locked: it only reads the config and asks the
            // terminal.
            print!("{}", doctor::command_line(&paths.config_file()));
            return Ok(std::process::ExitCode::SUCCESS);
        }
        Some(Command::Serve) => true,
        // The clients of a running `serve`: they make no folder, take no lock, open
        // no store and write no log. They end with their own exit codes.
        Some(command) => {
            let env_name = std::env::var_os(agent::mcp::NAME_ENV);
            std::process::exit(i32::from(client_command(
                command,
                &paths,
                env_name.as_deref(),
            )))
        }
        None => false,
    };
    if args.reload_theme {
        // Works without the profile lock: the running instances hold it.
        watch::signal_running_instances(&paths);
        return Ok(std::process::ExitCode::SUCCESS);
    }
    paths.create_dirs()?;
    let config = config::Config::load(&paths.config_file())?;
    notify::set_app_icon(config.notification_icon);

    // From the lock on, `matrix-rat --reload-theme` takes this process for a running
    // instance and sends it a signal, which ends a program that does not handle it.
    // The app listens for it only once its screen is set up, and `serve` never does:
    // until something listens, the signal is ignored.
    watch::ignore_reload_signal();
    // Held for the whole run: two instances on one profile would corrupt the SDK stores.
    let _lock = lock::ProfileLock::acquire(&paths)?;
    let _log_guard = logging::init(&paths)?;
    // Not the profile's name: it is the name of the log's own directory already.
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");

    let runtime = build_runtime(serve)?;
    let result = if serve {
        runtime.block_on(agent::serve::run(&paths, config.secret_store))
    } else {
        runtime.block_on(app::run(&paths, &config, args.demo, args.target))
    };
    if let Err(err) = &result {
        tracing::error!(why = %logsafe::kind_any(err), "exited with error");
        // A missing login has an exit code of its own: a restart does not mend it.
        if err.is::<agent::serve::NeedLogin>() {
            eprintln!("Error: {err}");
            return Ok(agent::serve::EXIT_NEED_LOGIN.into());
        }
    }
    result.map(|()| std::process::ExitCode::SUCCESS)
}

/// Runs `send`, `read`, `listen` or `mcp` against the profile's `serve` and returns
/// the exit code. What is wrong is said on standard error, where it is the user's (or
/// the agent's) to read; nothing is logged. `env_name` is the agent's name from the
/// environment, which only `mcp` looks at.
#[expect(
    clippy::disallowed_methods,
    reason = "a command-line client: its one thread runs the request to its end"
)]
fn client_command(
    command: Command,
    paths: &paths::Paths,
    env_name: Option<&std::ffi::OsStr>,
) -> u8 {
    use agent::client::{self, exit};
    let socket = paths.agent_socket();
    let profile = paths.profile.as_str();
    let usage = |what: String| {
        eprintln!("matrix-rat: {what}");
        exit::USAGE
    };
    // The arguments are checked before anything is connected.
    let (room, from) = match &command {
        Command::Rooms => (None, &[][..]),
        Command::Send { room, .. } => (Some(room), &[][..]),
        Command::Read { room, from, .. }
        | Command::Listen { room, from, .. }
        | Command::Mcp { room, from, .. } => (Some(room), from.as_slice()),
        Command::Doctor | Command::Serve => return exit::USAGE,
    };
    if let Some(room) = room
        && let Err(what) = client::check_room_arg(room)
    {
        return usage(what);
    }
    if let Err(what) = client::check_from_args(from) {
        return usage(what);
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("matrix-rat: could not start: {err}");
            return exit::FAILED;
        }
    };
    let mut out = std::io::stdout();
    // Where the commands note what they left out; never what a message says.
    let mut notes = std::io::stderr();
    let trust = |allow_unverified: bool| {
        if allow_unverified {
            agent::proto::Trust::AllowUnverified
        } else {
            agent::proto::Trust::Verified
        }
    };
    let outcome = match command {
        Command::Rooms => runtime.block_on(client::rooms(&socket, &mut out)),
        Command::Send { room, text } => {
            let text = match client::text_arg(&text, &mut std::io::stdin()) {
                Ok(text) => text,
                Err(what) => return usage(what),
            };
            runtime.block_on(client::send(&socket, &room, &text, &mut out))
        }
        Command::Read {
            room,
            limit,
            from,
            allow_unverified,
        } => {
            let from = client::Sender::of(&from, trust(allow_unverified));
            runtime.block_on(client::read(
                &socket,
                &room,
                limit as usize,
                from,
                &mut out,
                &mut notes,
            ))
        }
        Command::Listen {
            room,
            from,
            allow_unverified,
        } => {
            let from = client::Sender::of(&from, trust(allow_unverified));
            runtime.block_on(client::listen(&socket, &room, from, &mut out, &mut notes))
        }
        Command::Mcp {
            room,
            name,
            from,
            allow_unverified,
        } => {
            let name = match agent::mcp::agent_name(name.as_deref(), env_name) {
                Ok(name) => name,
                Err(what) => return usage(what),
            };
            // Speaks until its input ends. What goes wrong with `serve` meanwhile is
            // told to the agent, call by call.
            runtime.block_on(agent::mcp::run_stdio(agent::mcp::Config {
                socket,
                profile: profile.to_owned(),
                room,
                name,
                from,
                trust: trust(allow_unverified),
            }));
            Ok(())
        }
        Command::Doctor | Command::Serve => return exit::USAGE,
    };
    match outcome {
        Ok(()) => exit::OK,
        Err(err) => {
            eprintln!("matrix-rat: {}", err.message(profile));
            err.exit_code()
        }
    }
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

    #[test]
    fn the_commands_for_agents_are_words_of_their_own_and_keep_their_arguments_private() {
        assert_eq!(parse(&["serve"]).command, Some(Command::Serve));
        let args = parse(&["--profile", "bot", "serve"]);
        assert_eq!(
            (args.command, args.profile.as_str()),
            (Some(Command::Serve), "bot")
        );
        // The profile may come after the command too.
        assert_eq!(parse(&["serve", "--profile", "bot"]).profile, "bot");
        assert_eq!(parse(&["rooms"]).command, Some(Command::Rooms));

        // Everything after the first word of the text is text, dashes and all.
        let args = parse(&["send", "--room", "Agents", "ls", "-la", "--room", "x"]);
        assert_eq!(
            args.command,
            Some(Command::Send {
                room: "Agents".into(),
                text: vec!["ls".into(), "-la".into(), "--room".into(), "x".into()],
            })
        );
        let args = parse(&["--profile", "bot", "send", "--room", "!r:example.org", "-"]);
        assert_eq!(
            (args.command, args.profile.as_str()),
            (
                Some(Command::Send {
                    room: "!r:example.org".into(),
                    text: vec!["-".into()],
                }),
                "bot"
            )
        );
        assert_eq!(
            parse(&["read", "--room", "Agents"]).command,
            Some(Command::Read {
                room: "Agents".into(),
                limit: 20,
                from: Vec::new(),
                allow_unverified: false,
            })
        );
        assert_eq!(
            parse(&[
                "read",
                "--room",
                "Agents",
                "--limit",
                "100",
                "--from",
                "@ann:example.org"
            ])
            .command,
            Some(Command::Read {
                room: "Agents".into(),
                limit: 100,
                from: vec!["@ann:example.org".into()],
                allow_unverified: false,
            })
        );
        assert_eq!(
            parse(&["listen", "--room", "Agents", "--from", "@ann:example.org"]).command,
            Some(Command::Listen {
                room: "Agents".into(),
                from: vec!["@ann:example.org".into()],
                allow_unverified: false,
            })
        );
        assert_eq!(
            parse(&["mcp", "--room", "Agents", "--name", "builder"]).command,
            Some(Command::Mcp {
                room: "Agents".into(),
                name: Some("builder".into()),
                from: Vec::new(),
                allow_unverified: false,
            })
        );
        // Taking what is not verified is said in so many words, and only makes
        // sense for a user.
        assert_eq!(
            parse(&[
                "mcp",
                "--room",
                "Agents",
                "--name",
                "builder",
                "--from",
                "@ann:example.org",
                "--allow-unverified"
            ])
            .command,
            Some(Command::Mcp {
                room: "Agents".into(),
                name: Some("builder".into()),
                from: vec!["@ann:example.org".into()],
                allow_unverified: true,
            })
        );
        assert!(matches!(
            parse(&[
                "read",
                "--room",
                "R",
                "--from",
                "@a:b.c",
                "--allow-unverified"
            ])
            .command,
            Some(Command::Read {
                allow_unverified: true,
                ..
            })
        ));
        assert!(matches!(
            parse(&[
                "listen",
                "--room",
                "R",
                "--allow-unverified",
                "--from",
                "@a:b.c"
            ])
            .command,
            Some(Command::Listen {
                allow_unverified: true,
                ..
            })
        ));

        // What is missing or out of range is a usage error (exit code 2).
        for words in [
            &["send"][..],
            &["send", "--room", "Agents"],
            &["send", "hello"],
            &["read"],
            &["read", "--room", "Agents", "--limit", "0"],
            &["read", "--room", "Agents", "--limit", "101"],
            &["read", "--room", "Agents", "--limit", "many"],
            &["listen"],
            &["mcp", "--name", "builder"],
            &[
                "mcp",
                "--room",
                "Agents",
                "--name",
                "builder",
                "--allow-unverified",
            ],
            &["read", "--room", "Agents", "--allow-unverified"],
            &["listen", "--room", "Agents", "--allow-unverified"],
            &["serve", "--room", "Agents"],
        ] {
            let err =
                Args::try_parse_from(std::iter::once("matrix-rat").chain(words.iter().copied()))
                    .expect_err(&format!("{words:?}"));
            assert_eq!(err.exit_code(), 2, "{words:?}");
        }

        // The room, the user, the name and the text stay out of debug output.
        for words in [
            &["send", "--room", "Secret Garden", "the", "launch", "codes"][..],
            &[
                "read",
                "--room",
                "Secret Garden",
                "--from",
                "@alice:example.org",
            ],
            &[
                "listen",
                "--room",
                "!hidden:example.org",
                "--from",
                "@alice:example.org",
            ],
            &[
                "mcp",
                "--room",
                "Secret Garden",
                "--name",
                "raise-alice",
                "--from",
                "@alice:example.org",
            ],
        ] {
            let shown = format!("{:?}", parse(words));
            for private in ["Garden", "launch", "alice", "hidden", "example.org"] {
                assert!(!shown.contains(private), "{private} in {shown}");
            }
        }
        assert!(format!("{:?}", parse(&["mcp", "--room", "R", "--name", "n"])).contains("Mcp"));

        // The name may be left to the environment; that there is one at all is
        // checked when the command runs.
        assert_eq!(
            parse(&["mcp", "--room", "Agents"]).command,
            Some(Command::Mcp {
                room: "Agents".into(),
                name: None,
                from: Vec::new(),
                allow_unverified: false,
            })
        );
    }

    /// `--from` once for each account of the user: none, one (as it always was) or
    /// several, on `read`, `listen` and `mcp` alike, in the order given.
    #[test]
    fn from_may_name_several_accounts_of_the_user() {
        let from = |words: &[&str]| -> (Vec<String>, bool) {
            match parse(words).command {
                Some(
                    Command::Read {
                        from,
                        allow_unverified,
                        ..
                    }
                    | Command::Listen {
                        from,
                        allow_unverified,
                        ..
                    }
                    | Command::Mcp {
                        from,
                        allow_unverified,
                        ..
                    },
                ) => (from, allow_unverified),
                other => panic!("{words:?}: {other:?}"),
            }
        };
        let (ann, beeper) = ("@ann:example.org", "@ann:beeper.example");
        for command in ["read", "listen", "mcp"] {
            let base = [command, "--room", "Agents"];
            let with = |extra: &[&'static str]| -> Vec<&str> {
                base.iter().chain(extra).copied().collect()
            };
            assert_eq!(from(&base), (Vec::new(), false), "{command}");
            assert_eq!(
                from(&with(&["--from", ann])),
                (vec![ann.to_owned()], false),
                "{command}"
            );
            assert_eq!(
                from(&with(&["--from", ann, "--from", beeper])),
                (vec![ann.to_owned(), beeper.to_owned()], false),
                "{command}"
            );
            // In the order given, also around other options.
            assert_eq!(
                from(&with(&[
                    "--from",
                    beeper,
                    "--allow-unverified",
                    "--from",
                    ann
                ])),
                (vec![beeper.to_owned(), ann.to_owned()], true),
                "{command}"
            );
            // One value each time: a second word is not taken for an account.
            let words = with(&["--from", ann, beeper]);
            let parsed =
                Args::try_parse_from(std::iter::once("matrix-rat").chain(words.iter().copied()));
            assert!(parsed.is_err(), "{words:?}");
            // A `--from` without a value is a usage error.
            let words = with(&["--from"]);
            let err =
                Args::try_parse_from(std::iter::once("matrix-rat").chain(words.iter().copied()))
                    .expect_err(&format!("{words:?}"));
            assert_eq!(err.exit_code(), 2, "{words:?}");
        }
        // Both accounts stay out of debug output.
        let shown = format!(
            "{:?}",
            parse(&[
                "mcp", "--room", "Agents", "--name", "n", "--from", ann, "--from", beeper
            ])
        );
        assert!(
            !shown.contains("ann") && !shown.contains("beeper") && !shown.contains("example"),
            "{shown}"
        );
    }

    /// Every `--from` is checked, and the same account twice is a usage error too,
    /// before anything is connected; two good ones go on to look for `serve`.
    #[test]
    fn every_from_is_checked_before_connecting() {
        let root = std::env::temp_dir().join(format!("mr-main-from-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = paths::Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "bot".into(),
        };
        let (ann, beeper) = ("@ann:example.org", "@ann:beeper.example");
        for command in ["read", "listen", "mcp"] {
            let base = [command, "--room", "Agents", "--name", "builder"];
            let base = if command == "mcp" {
                &base[..]
            } else {
                &base[..3]
            };
            for (from, code) in [
                (&[ann, beeper][..], 3),
                (&[ann, ""], 2),
                (&["", ann], 2),
                (&[ann, "beeper"], 2),
                (&[ann, ann], 2),
                (&[ann, beeper, ann], 2),
            ] {
                // (`mcp` with good arguments would go on to serve on standard input.)
                if command == "mcp" && code != 2 {
                    continue;
                }
                let mut words = base.to_vec();
                for user in from {
                    words.extend(["--from", *user]);
                }
                let command = parse(&words).command.unwrap();
                assert_eq!(client_command(command, &paths, None), code, "{words:?}");
            }
        }
        assert!(!root.exists(), "a client made a folder");
    }

    #[test]
    fn a_client_command_with_a_bad_argument_ends_with_the_usage_code_before_connecting() {
        // A profile whose runtime folder is not there: nothing is made of it.
        let root = std::env::temp_dir().join(format!("mr-main-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = paths::Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "bot".into(),
        };
        for words in [
            &["read", "--room", "  "][..],
            &["read", "--room", "Agents", "--from", "ann"],
            &["listen", "--room", "Agents", "--from", "#room:example.org"],
            &["mcp", "--room", "Agents", "--name", "a]b"],
            &[
                "mcp", "--room", "Agents", "--name", "builder", "--from", "@ann",
            ],
            &["send", "--room", "Agents", "  "],
        ] {
            let command = parse(words).command.unwrap();
            assert_eq!(client_command(command, &paths, None), 2, "{words:?}");
        }
        // No name at all, and a name from the environment that is not fine: the
        // same usage error as a bad `--name`.
        let too_long = "n".repeat(agent::mcp::MAX_AGENT_NAME + 1);
        let hostile: [Option<&str>; 7] = [
            None,
            Some(""),
            Some("a]b"),
            Some("two\nlines"),
            Some("esc\u{1b}[31m"),
            Some("flip\u{202e}"),
            Some(too_long.as_str()),
        ];
        for env in hostile {
            let command = parse(&["mcp", "--room", "Agents"]).command.unwrap();
            let env = env.map(std::ffi::OsStr::new);
            assert_eq!(client_command(command, &paths, env), 2, "{env:?}");
        }
        // A bad `--name` is not mended by a good name in the environment.
        let command = parse(&["mcp", "--room", "Agents", "--name", "a]b"])
            .command
            .unwrap();
        let env = Some(std::ffi::OsStr::new("builder"));
        assert_eq!(client_command(command, &paths, env), 2);
        // With good arguments and no `serve`: said so, with its own code.
        for words in [
            &["rooms"][..],
            &["read", "--room", "Agents"],
            &["send", "--room", "Agents", "hello"],
            &["listen", "--room", "Agents", "--from", "@ann:example.org"],
        ] {
            let command = parse(words).command.unwrap();
            assert_eq!(client_command(command, &paths, None), 3, "{words:?}");
        }
        assert!(!root.exists(), "a client made a folder");
    }

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
