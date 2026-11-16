//! Entering and leaving the terminal's TUI state, in one place.
//!
//! Startup order matters (see docs/PLAN.md §4): terminal queries that read replies from
//! stdin (image protocol, background colour) must run after raw mode is on, before mouse
//! and focus reports are switched on (they would arrive among the replies) and before
//! the crossterm event stream exists. `TerminalGuard::enter` turns on raw mode;
//! `alternate_screen` may follow so that a terminal that prints a query instead of
//! answering it does so where it does no harm; callers run their queries, then call
//! `activate`.
//!
//! The window title: with the `window_title` setting on, the title that was there is
//! saved on the terminal's title stack when the app takes the terminal and put back
//! whenever it hands it back (on exit, around Ctrl-Z and the editor, and from the
//! panic hook). With the setting off the title is never touched, not even saved.

use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use crossterm::{execute, queue};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

static RAW: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);
/// The keyboard protocol's flags were pushed (and so are to be popped): only by
/// `activate` and `resume`, never by the queries before them.
static KEYBOARD_PUSHED: AtomicBool = AtomicBool::new(false);
static MOUSE: AtomicBool = AtomicBool::new(true);
/// The `window_title` setting.
static TITLE: AtomicBool = AtomicBool::new(false);
/// The title that was there is on the terminal's title stack, to be put back.
static TITLE_PUSHED: AtomicBool = AtomicBool::new(false);

/// Saves the window and icon titles on the terminal's title stack (xterm's
/// `CSI 22 ; 0 t`); terminals without one ignore it.
const PUSH_TITLE: &str = "\x1b[22;0t";
/// Puts the saved titles back (`CSI 23 ; 0 t`).
const POP_TITLE: &str = "\x1b[23;0t";

/// Restores the terminal when dropped. Also restored by the panic hook.
#[derive(Debug)]
pub struct TerminalGuard {
    _private: (),
}

impl TerminalGuard {
    /// Step 1: raw mode only. Run stdin-reading terminal queries after this.
    /// `title`: the `window_title` setting (see the module's documentation).
    pub fn enter(mouse: bool, title: bool) -> Result<Self> {
        install_panic_hook();
        MOUSE.store(mouse, Ordering::SeqCst);
        TITLE.store(title, Ordering::SeqCst);
        enable_raw_mode()?;
        RAW.store(true, Ordering::SeqCst);
        // Queried here because it also reads a reply from stdin.
        let enhanced = supports_keyboard_enhancement().unwrap_or(false);
        KEYBOARD_ENHANCED.store(enhanced, Ordering::SeqCst);
        Ok(Self { _private: () })
    }

    /// Whether the terminal speaks the kitty keyboard protocol, as it answered in
    /// [`TerminalGuard::enter`]: the app keeps this rather than asking again (once the
    /// event stream exists, nothing else may read the terminal's input).
    pub fn keyboard_enhanced(&self) -> bool {
        KEYBOARD_ENHANCED.load(Ordering::SeqCst)
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
    if KEYBOARD_ENHANCED.load(Ordering::SeqCst) && !KEYBOARD_PUSHED.swap(true, Ordering::SeqCst) {
        queue!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    if TITLE.load(Ordering::SeqCst) && !TITLE_PUSHED.swap(true, Ordering::SeqCst) {
        write!(out, "{PUSH_TITLE}")?;
    }
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

/// Turns the `window_title` setting on or off while running: on, the title that is
/// there is saved first; off, it is put back and not touched again.
pub fn set_window_title(on: bool) -> Result<()> {
    if TITLE.swap(on, Ordering::SeqCst) == on || !ACTIVE.load(Ordering::SeqCst) {
        return Ok(());
    }
    let mut out = io::stdout();
    if on {
        if !TITLE_PUSHED.swap(true, Ordering::SeqCst) {
            write!(out, "{PUSH_TITLE}")?;
        }
    } else if TITLE_PUSHED.swap(false, Ordering::SeqCst) {
        write!(out, "{POP_TITLE}")?;
    }
    out.flush()?;
    Ok(())
}

/// Sets the window title, where the `window_title` setting allows it and the title
/// that was there has been saved; else nothing is written.
pub fn write_window_title(title: &str) -> Result<()> {
    if !TITLE.load(Ordering::SeqCst) || !TITLE_PUSHED.load(Ordering::SeqCst) {
        return Ok(());
    }
    execute!(io::stdout(), SetTitle(title))?;
    Ok(())
}

/// For `matrix-rat doctor`: asks the terminal what the app asks it at start, with the
/// same code and time limits: the keyboard protocol in raw mode, then the pictures on
/// the alternate screen (where a terminal that prints a question instead of answering
/// does no harm). The terminal is handed back as it was, also if a question fails.
/// Returns whether the keyboard protocol is there, and how pictures would be shown.
pub fn ask_for_doctor(images: crate::config::ImageMode) -> Result<(bool, crate::images::Graphics)> {
    // No mouse and no title: nothing is activated, only asked.
    let guard = TerminalGuard::enter(false, false)?;
    guard.alternate_screen()?;
    let (graphics, _) = crate::images::Graphics::detect_with_term(images, false);
    let keyboard = guard.keyboard_enhanced();
    drop(guard);
    Ok((keyboard, graphics))
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
    resume(terminal)
}

/// Hands the terminal back as it was before the TUI (cooked mode, main screen, no mouse
/// or paste reports, no keyboard protocol), for a child process that wants it. Undo
/// with [`resume`].
pub fn release() {
    restore();
}

/// Takes a wake-up that a dropped `EventStream` left in crossterm's reader.
///
/// Dropping the stream wakes the reader so that its thread can end. If that thread was
/// not reading just then (it rests between events), nobody takes the wake-up, and the
/// next read with a time limit returns at once with nothing: the cursor position that
/// `Terminal::clear` asks for was then "not read within a normal duration", and the app
/// ended after the first use of an editor. A read that waits for nothing takes it.
pub fn absorb_reader_wake() {
    let _ = crossterm::event::poll(std::time::Duration::ZERO);
}

/// Takes the terminal again after [`release`] (or a stop): raw mode, alternate screen,
/// mouse, paste, focus reports and the keyboard protocol, on a cleared screen.
pub fn resume(terminal: &mut Tui) -> Result<()> {
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
        // Only what was pushed is popped: one pop more would take an entry of the
        // shell's (or of whatever runs us).
        if KEYBOARD_PUSHED.swap(false, Ordering::SeqCst) {
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
    if TITLE_PUSHED.swap(false, Ordering::SeqCst) {
        let _ = write!(out, "{POP_TITLE}");
        let _ = out.flush();
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
        // Where, not what: a panic's message can quote the text it failed on. The
        // whole message still goes to the terminal, below.
        match info.location() {
            Some(at) => tracing::error!(file = place(at.file()), line = at.line(), "panic"),
            None => tracing::error!("panic"),
        }
        previous(info);
    }));
}

/// The file a panic happened in, without the directories before it: the path of a
/// dependency's file starts with the home directory, and so with the user's name.
fn place(file: &str) -> &str {
    if !file.starts_with('/') {
        return file;
    }
    // `…/registry/src/<index>/<crate>-<version>/src/…`: from the crate on.
    file.split_once("/registry/src/")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map_or_else(|| file.rsplit('/').next().unwrap_or(file), |(_, rest)| rest)
}

#[cfg(test)]
mod tests {
    use super::{POP_TITLE, PUSH_TITLE, base64, place};

    #[test]
    fn the_title_is_saved_and_put_back_on_the_terminals_title_stack() {
        // xterm's window manipulation: 22 saves, 23 restores; 0 is both the window
        // and the icon title, which the title we set (OSC 0) changes together.
        assert_eq!(PUSH_TITLE, "\x1b[22;0t");
        assert_eq!(POP_TITLE, "\x1b[23;0t");
    }

    #[test]
    fn a_panics_place_leaves_out_the_home_directory() {
        assert_eq!(place("src/app.rs"), "src/app.rs");
        assert_eq!(
            place(
                "/home/alice/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/ratatui-0.30.2/src/buffer.rs"
            ),
            "ratatui-0.30.2/src/buffer.rs"
        );
        assert_eq!(place("/home/alice/elsewhere/lib.rs"), "lib.rs");
    }

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
