//! Entering and leaving the terminal's TUI state, in one place.
//!
//! Startup order matters (see docs/PLAN.md §4): terminal queries that read replies from
//! stdin (image protocol, background colour) must run after raw mode is on and before the
//! crossterm event stream exists. `TerminalGuard::enter` does the first half; callers run
//! their queries, then call `activate`.

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
    out.flush()?;
    Ok(())
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

/// Idempotent; safe to call from the panic hook and from `Drop`.
pub fn restore() {
    let mut out = io::stdout();
    if ACTIVE.swap(false, Ordering::SeqCst) {
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
