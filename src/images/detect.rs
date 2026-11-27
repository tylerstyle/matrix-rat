//! Asking the terminal what it can draw, and what the environment says about it.

use std::time::Duration;

use super::{Graphics, Protocol};
use crate::config::ImageMode;
use crate::shell::terminal::termquery;

/// How long the terminal gets to answer the question about what it can draw.
const QUERY_TIMEOUT: Duration = Duration::from_millis(1000);

impl Graphics {
    /// Asks the terminal what it can do (see [`termquery`] for when this may
    /// run). Takes at most [`QUERY_TIMEOUT`]; without an answer it falls back to
    /// half blocks for graphics. When `probe_appearance` is true, colour-scheme and
    /// background probes are batched into the same round-trip (or run alone when
    /// graphics probing is disabled).
    ///
    /// Only kitty's protocol with Unicode placeholders is used for real graphics: its
    /// pictures live in ordinary cells, so they cannot be left behind or painted over
    /// popups. Terminals that only do sixel or the iTerm2 protocol get half blocks, and
    /// so does everything inside a multiplexer.
    pub fn detect_with_term(
        mode: ImageMode,
        probe_appearance: bool,
    ) -> (Self, Option<termquery::Reply>) {
        let probe_graphics =
            matches!(mode, ImageMode::Auto) && !in_multiplexer() && !placeholders_broken();
        if !probe_graphics {
            let graphics = match mode {
                ImageMode::Off => Self::off(),
                ImageMode::Halfblocks => Self::halfblocks(),
                ImageMode::Auto if in_multiplexer() => {
                    tracing::info!("images: inside a terminal multiplexer, using half blocks");
                    Self::halfblocks()
                }
                ImageMode::Auto => {
                    tracing::info!(
                        "images: this terminal's kitty placeholders are unreliable, using half blocks"
                    );
                    Self::halfblocks()
                }
            };
            let reply = if probe_appearance {
                let reply = termquery::ask_appearance(QUERY_TIMEOUT);
                if let Some(reply) = reply {
                    tracing::info!(?reply, "appearance: terminal queried");
                }
                reply
            } else {
                None
            };
            return (graphics, reply);
        }
        let reply = if probe_appearance {
            termquery::ask(QUERY_TIMEOUT)
        } else {
            termquery::ask_graphics(QUERY_TIMEOUT)
        };
        let Some(reply) = reply else {
            return (Self::halfblocks(), None);
        };
        let graphics = Self::from_reply(reply, termquery::cell_from_window());
        tracing::info!(?reply, ?graphics, "images: terminal queried");
        (graphics, Some(reply))
    }

    /// `window_cell`: the cell size the system reports, for terminals that do not
    /// answer that part.
    fn from_reply(reply: termquery::Reply, window_cell: Option<(u16, u16)>) -> Self {
        match (reply.kitty, reply.cell.or(window_cell)) {
            (true, Some(font)) => Self {
                protocol: Some(Protocol::Kitty {
                    compress: reply.compression,
                }),
                font,
            },
            // No graphics, no answer in time, or no way to size a picture.
            _ => Self::halfblocks(),
        }
    }
}

/// WezTerm and Konsole answer the kitty graphics check but do not place pictures
/// through Unicode placeholders correctly.
pub(crate) fn placeholders_broken() -> bool {
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    set("WEZTERM_EXECUTABLE") || set("KONSOLE_VERSION")
}

pub(crate) fn in_multiplexer() -> bool {
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    let term = std::env::var("TERM").unwrap_or_default();
    set("TMUX")
        || set("ZELLIJ")
        || set("STY")
        || term.starts_with("tmux")
        || term.starts_with("screen")
        || std::env::var("TERM_PROGRAM").is_ok_and(|program| program == "tmux")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_answers_decide_how_pictures_are_drawn() {
        use crate::shell::terminal::termquery::Reply;
        let kitty = Reply {
            kitty: true,
            compression: true,
            cell: Some((9, 18)),
            complete: true,
            ..Reply::default()
        };
        assert_eq!(
            Graphics::from_reply(kitty, Some((7, 14))),
            Graphics {
                protocol: Some(Protocol::Kitty { compress: true }),
                font: (9, 18)
            }
        );
        // The cell size may come from the system instead.
        let no_cell = Reply {
            cell: None,
            ..kitty
        };
        assert_eq!(Graphics::from_reply(no_cell, Some((7, 14))).font, (7, 14));
        // Without any size a picture cannot be laid out; without kitty or without an
        // answer (the timeout) it is half blocks.
        assert_eq!(Graphics::from_reply(no_cell, None), Graphics::halfblocks());
        let plain = Reply {
            kitty: false,
            ..kitty
        };
        assert_eq!(Graphics::from_reply(plain, None), Graphics::halfblocks());
        assert_eq!(
            Graphics::from_reply(Reply::default(), Some((7, 14))),
            Graphics::halfblocks()
        );
    }
}
