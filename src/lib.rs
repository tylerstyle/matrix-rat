#![recursion_limit = "256"]
// A test may wait for what it tests; everywhere else `clippy.toml` refuses calls that
// block their thread unless the place says why it may.
#![cfg_attr(test, allow(clippy::disallowed_methods))]
// Every match on one of our own enums names its variants, so adding one flags every
// place that must decide about it. Foreign enums (and tests) are exempt via `#[expect]`
// or the test-only allow below.
#![warn(clippy::wildcard_enum_match_arm)]
#![cfg_attr(test, allow(clippy::wildcard_enum_match_arm))]
// Public are only the names the command line uses (`cli`); the documentation is read with
// `cargo doc --document-private-items`, where their links to the crate's own items lead
// somewhere, as they did when the binary was the crate.
#![allow(rustdoc::private_intra_doc_links)]
//! matrix-rat — a mouse-friendly Matrix client for the terminal.
//!
//! The client itself: the app, the Matrix worker, the terminal shell, `serve` and the
//! clients of its socket. The `matrix-rat` command (`src/main.rs`) reads the command line
//! and calls what [`cli`] makes public; every module stays the crate's own.

mod action;
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
mod format;
mod highlight;
mod images;
mod input;
mod keymap;
mod keywords;
mod lineedit;
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
mod previews;
mod probes;
mod rich;
mod rt;
mod saved;
mod shell;
mod storage;
#[cfg(test)]
mod testbus;
mod text;
mod textcmd;
mod theme;
mod time;
mod ui;
mod upload;
mod verify;

// The terminal's and the OS's alone: `serve` and its clients, the desktop's portal and
// window focus, the theme watch and the profile lock, the log file, the program's
// runtime. The browser build (`cargo check --lib --target wasm32-unknown-unknown`)
// leaves them out, together with the command line's [`cli`].
#[cfg(not(target_family = "wasm"))]
mod agent;
#[cfg(not(target_family = "wasm"))]
mod focus;
#[cfg(not(target_family = "wasm"))]
mod lock;
#[cfg(not(target_family = "wasm"))]
mod logging;
#[cfg(not(target_family = "wasm"))]
mod portal;
#[cfg(not(target_family = "wasm"))]
mod runtime;
#[cfg(not(target_family = "wasm"))]
mod watch;

/// What the command line (`src/main.rs`) uses, under the paths it has inside the crate,
/// and nothing else of those modules.
#[cfg(not(target_family = "wasm"))]
pub mod cli {
    pub mod agent {
        pub mod client {
            pub use crate::agent::client::{
                ClientError, Sender, check_from_args, check_room_arg, exit, listen, read, rooms,
                send, text_arg,
            };
        }
        pub mod mcp {
            pub use crate::agent::mcp::{Config, MAX_AGENT_NAME, NAME_ENV, agent_name, run_stdio};
        }
        pub mod proto {
            pub use crate::agent::proto::Trust;
        }
        pub mod serve {
            pub use crate::agent::serve::{EXIT_NEED_LOGIN, NeedLogin, run};
        }
    }
    pub mod config {
        pub use crate::config::Config;
    }
    pub mod doctor {
        pub use crate::doctor::command_line;
    }
    pub mod lock {
        pub use crate::lock::ProfileLock;
    }
    pub mod logging {
        pub use crate::logging::init;
    }
    pub mod logsafe {
        pub use crate::logsafe::kind_any;
    }
    pub mod notify {
        pub use crate::notify::set_app_icon;
    }
    pub mod paths {
        pub use crate::paths::Paths;
    }
    pub mod runtime {
        pub use crate::runtime::build_runtime;
    }
    pub mod shell {
        pub mod terminal {
            pub use crate::shell::terminal::run;
        }
    }
    pub mod theme {
        pub use crate::theme::TEMPLATES;
    }
    pub mod watch {
        pub use crate::watch::{ignore_reload_signal, signal_running_instances};
    }
}
