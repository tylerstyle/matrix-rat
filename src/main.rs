#![recursion_limit = "256"]
//! matrix-rat — a mouse-friendly Matrix client for the terminal.

mod action;
mod agent;
mod app;
mod audio;
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
#[derive(Clone, PartialEq, Eq, Subcommand)]
enum Command {
    /// Print what matrix-rat finds here (terminal, pictures, clipboard, notifications,
    /// keys, config) and exit. Nothing private is in it: paste it into a bug report.
    /// Takes no lock and logs nobody in, so it also runs beside a running matrix-rat.
    Doctor,
    /// Run without the screen, for agents and scripts: stay in sync on a profile
    /// that is already logged in, and serve `send`, `read`, `listen` and `mcp` over a
    /// private socket. Accepts no invitation and never asks for a login. Ends on
    /// SIGTERM or Ctrl-C.
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
        /// Only messages of this user (`@user:server`), among the latest 100.
        #[arg(long, value_name = "USER")]
        from: Option<String>,
    },
    /// Stay connected to a running `matrix-rat serve` and print messages of a room
    /// as they come (and again when they change), one JSON object per line.
    Listen {
        /// The room: its id, its exact name, or its address.
        #[arg(long)]
        room: String,
        /// Only messages of this user (`@user:server`).
        #[arg(long, value_name = "USER")]
        from: Option<String>,
    },
    /// An MCP server on standard input and output for one agent, bound to one room,
    /// through a running `matrix-rat serve`: tools to send, to read, and to wait for
    /// a message. No tool can reach another room.
    Mcp {
        /// The one room: its id (best), its exact name, or its address.
        #[arg(long)]
        room: String,
        /// The agent's name; its messages are signed with it: `[NAME] text`.
        #[arg(long)]
        name: String,
        /// The user whose messages count as answers (`@user:server`). Everything
        /// else in the room is marked as not from the user.
        #[arg(long, value_name = "USER")]
        from: Option<String>,
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
    let serve = match args.command {
        Some(Command::Doctor) => {
            // Before anything is made or locked: it only reads the config and asks the
            // terminal.
            print!("{}", doctor::command_line(&paths.config_file()));
            return Ok(());
        }
        Some(Command::Serve) => true,
        // The clients of a running `serve`: they make no folder, take no lock, open
        // no store and write no log. They end with their own exit codes.
        Some(command) => std::process::exit(i32::from(client_command(command, &paths))),
        None => false,
    };
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
    let result = if serve {
        runtime.block_on(agent::serve::run(&paths, config.secret_store))
    } else {
        runtime.block_on(app::run(&paths, &config, args.demo, args.target))
    };
    if let Err(err) = &result {
        tracing::error!(why = %logsafe::kind_any(err), "exited with error");
    }
    result
}

/// Runs `send`, `read`, `listen` or `mcp` against the profile's `serve` and returns
/// the exit code. What is wrong is said on standard error, where it is the user's (or
/// the agent's) to read; nothing is logged.
fn client_command(command: Command, paths: &paths::Paths) -> u8 {
    use agent::client::{self, exit};
    let socket = paths.agent_socket();
    let profile = paths.profile.as_str();
    let usage = |what: String| {
        eprintln!("matrix-rat: {what}");
        exit::USAGE
    };
    // The arguments are checked before anything is connected.
    let (room, from) = match &command {
        Command::Rooms => (None, None),
        Command::Send { room, .. } => (Some(room), None),
        Command::Read { room, from, .. }
        | Command::Listen { room, from }
        | Command::Mcp { room, from, .. } => (Some(room), from.as_ref()),
        Command::Doctor | Command::Serve => return exit::USAGE,
    };
    if let Some(room) = room
        && let Err(what) = client::check_room_arg(room)
    {
        return usage(what);
    }
    if let Some(from) = from
        && let Err(what) = client::check_from_arg(from)
    {
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
    let outcome = match command {
        Command::Rooms => runtime.block_on(client::rooms(&socket, &mut out)),
        Command::Send { room, text } => {
            let text = match client::text_arg(&text, &mut std::io::stdin()) {
                Ok(text) => text,
                Err(what) => return usage(what),
            };
            runtime.block_on(client::send(&socket, &room, &text, &mut out))
        }
        Command::Read { room, limit, from } => runtime.block_on(client::read(
            &socket,
            &room,
            limit as usize,
            from.as_deref(),
            &mut out,
        )),
        Command::Listen { room, from } => {
            runtime.block_on(client::listen(&socket, &room, from.as_deref(), &mut out))
        }
        Command::Mcp { room, name, from } => {
            let name = match agent::mcp::check_name(&name) {
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
                from: None,
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
                from: Some("@ann:example.org".into()),
            })
        );
        assert_eq!(
            parse(&["listen", "--room", "Agents", "--from", "@ann:example.org"]).command,
            Some(Command::Listen {
                room: "Agents".into(),
                from: Some("@ann:example.org".into()),
            })
        );
        assert_eq!(
            parse(&["mcp", "--room", "Agents", "--name", "builder"]).command,
            Some(Command::Mcp {
                room: "Agents".into(),
                name: "builder".into(),
                from: None,
            })
        );

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
            &["mcp", "--room", "Agents"],
            &["mcp", "--name", "builder"],
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
            assert_eq!(client_command(command, &paths), 2, "{words:?}");
        }
        // With good arguments and no `serve`: said so, with its own code.
        for words in [
            &["rooms"][..],
            &["read", "--room", "Agents"],
            &["send", "--room", "Agents", "hello"],
            &["listen", "--room", "Agents", "--from", "@ann:example.org"],
        ] {
            let command = parse(words).command.unwrap();
            assert_eq!(client_command(command, &paths), 3, "{words:?}");
        }
        assert!(!root.exists(), "a client made a folder");
    }
}
