//! Entering and leaving the terminal's TUI state, in one place.
//!
//! Startup order matters (see docs/PLAN.md §4): terminal queries that read replies from
//! stdin (image protocol, background colour) must run after raw mode is on, before mouse
//! and focus reports are switched on (they would arrive among the replies) and before
//! the crossterm event stream exists. `TerminalGuard::enter` turns on raw mode;
//! `alternate_screen` may follow so that a terminal that prints a query instead of
//! answering it does so where it does no harm; callers run their queries, then call
//! `activate`.

use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use crossterm::{execute, queue};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

static RAW: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);
static MOUSE: AtomicBool = AtomicBool::new(true);

/// Restores the terminal when dropped. Also restored by the panic hook.
#[derive(Debug)]
pub struct TerminalGuard {
    _private: (),
}

impl TerminalGuard {
    /// Step 1: raw mode only. Run stdin-reading terminal queries after this.
    pub fn enter(mouse: bool) -> Result<Self> {
        install_panic_hook();
        MOUSE.store(mouse, Ordering::SeqCst);
        enable_raw_mode()?;
        RAW.store(true, Ordering::SeqCst);
        // Queried here because it also reads a reply from stdin.
        let enhanced = supports_keyboard_enhancement().unwrap_or(false);
        KEYBOARD_ENHANCED.store(enhanced, Ordering::SeqCst);
        Ok(Self { _private: () })
    }

    /// Optional step between the two: only the alternate screen, nothing that makes
    /// the terminal send reports. `activate` clears whatever a query left on it.
    pub fn alternate_screen(&self) -> Result<()> {
        // Set first, so that `restore` leaves the screen even if the write fails halfway.
        ACTIVE.store(true, Ordering::SeqCst);
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(())
    }

    /// Step 2: alternate screen, mouse, paste, focus events and (where supported) the
    /// kitty keyboard protocol so Shift-Enter can be told apart from Enter.
    pub fn activate(&self) -> Result<Tui> {
        write_activation()?;
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        terminal.clear()?;
        Ok(terminal)
    }
}

fn write_activation() -> Result<()> {
    let mut out = io::stdout();
    // Set first: if a write below fails halfway, `restore` must still undo it.
    ACTIVE.store(true, Ordering::SeqCst);
    queue!(
        out,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableFocusChange
    )?;
    if MOUSE.load(Ordering::SeqCst) {
        queue!(out, EnableMouseCapture)?;
    }
    if KEYBOARD_ENHANCED.load(Ordering::SeqCst) {
        queue!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    // Ask terminals that support DEC mode 2031 (kitty, contour, foot, ghostty) to
    // report subsequent desktop dark/light colour-scheme changes live.
    io::Write::write_all(&mut out, b"\x1b[?2031h")?;
    out.flush()?;
    Ok(())
}

/// Turns mouse capture on or off while running (the settings menu does this).
pub fn set_mouse(on: bool) -> Result<()> {
    if MOUSE.swap(on, Ordering::SeqCst) == on || !ACTIVE.load(Ordering::SeqCst) {
        return Ok(());
    }
    let mut out = io::stdout();
    if on {
        queue!(out, EnableMouseCapture)?;
    } else {
        queue!(out, DisableMouseCapture)?;
    }
    out.flush()?;
    Ok(())
}

/// Puts text on the system clipboard through the terminal (OSC 52), which also works
/// over SSH. Terminals that do not support it ignore it.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    let mut out = io::stdout();
    write!(out, "\x1b]52;c;{}\x1b\\", base64(text.as_bytes()))?;
    out.flush()?;
    Ok(())
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Ctrl-Z: hand the terminal back, stop the process, and set everything up again when
/// the shell resumes it with `fg`.
pub fn suspend(terminal: &mut Tui) -> Result<()> {
    restore();
    signal_hook::low_level::emulate_default_handler(signal_hook::consts::SIGTSTP)?;
    enable_raw_mode()?;
    RAW.store(true, Ordering::SeqCst);
    write_activation()?;
    terminal.clear()?;
    Ok(())
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Asks the terminal for its current colour scheme (`CSI ? 996 n`), for instance
/// after resuming from `Ctrl-Z` suspension.
pub fn query_color_scheme() {
    let mut out = io::stdout();
    let _ = io::Write::write_all(&mut out, b"\x1b[?996n");
    let _ = io::Write::flush(&mut out);
}

/// Idempotent; safe to call from the panic hook and from `Drop`.
pub fn restore() {
    let mut out = io::stdout();
    if ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = io::Write::write_all(&mut out, b"\x1b[?2031l");
        if KEYBOARD_ENHANCED.load(Ordering::SeqCst) {
            let _ = execute!(out, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            out,
            DisableFocusChange,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
    if RAW.swap(false, Ordering::SeqCst) {
        let _ = disable_raw_mode();
    }
}

fn install_panic_hook() {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        tracing::error!(%info, "panic");
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_pads_correctly() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("ünï", "w7xuw68="),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded);
        }
    }
}
