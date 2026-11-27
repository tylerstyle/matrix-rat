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

pub mod external_editor;
pub mod termquery;

use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use crossterm::{execute, queue};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Rect;

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

/// Restores the terminal when dropped. Also restored by the panic hook, for a panic that
/// ends the app.
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
        clear(&mut terminal)?;
        Ok(terminal)
    }
}

/// Enables mouse button, wheel, and drag reporting (`?1000`, `?1002`, `?1015`, `?1006`)
/// without any-event pointer-motion reporting (`?1003`).
struct EnableMouseButtons;

impl crossterm::Command for EnableMouseButtons {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str("\x1b[?1000h\x1b[?1002h\x1b[?1015h\x1b[?1006h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Ok(())
    }
}

/// Clears the whole screen and has the next draw write every cell again.
///
/// Not ratatui's `Terminal::clear` (which `clippy.toml` refuses): that one first asks the
/// terminal where its cursor is and waits up to two seconds for the answer. While the
/// event stream's reader thread waits for input it holds crossterm's reader, so the
/// answer cannot be read until a key, a click or a focus change comes: a repaint that no
/// input had asked for (a member list arriving for the open room) ended the app after
/// two seconds with "The cursor position could not be read within a normal duration".
/// A full-screen app has no cursor position to keep: every frame sets its own. A resize
/// to the size the screen has clears the same way and asks the terminal nothing.
pub fn clear<B: Backend>(terminal: &mut Terminal<B>) -> Result<(), B::Error> {
    let size = terminal.size()?;
    terminal.resize(Rect::new(0, 0, size.width, size.height))
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
        queue!(out, EnableMouseButtons)?;
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
        queue!(out, EnableMouseButtons)?;
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
/// `Terminal::clear` asked for was then "not read within a normal duration", and the app
/// ended after the first use of an editor. [`clear`] asks nothing now; the wake-up is
/// still taken, so that no later read with a time limit is cut short by it. A read that
/// waits for nothing takes it.
pub fn absorb_reader_wake() {
    let _ = crossterm::event::poll(std::time::Duration::ZERO);
}

/// Takes the terminal again after [`release`] (or a stop): raw mode, alternate screen,
/// mouse, paste, focus reports and the keyboard protocol, on a cleared screen.
pub fn resume(terminal: &mut Tui) -> Result<()> {
    enable_raw_mode()?;
    RAW.store(true, Ordering::SeqCst);
    write_activation()?;
    clear(terminal)?;
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

/// Installed by the first [`TerminalGuard::enter`], on the thread that runs the interface:
/// the main thread, whose `block_on` runs the app's loop (see `main`). Before it, and in
/// `serve`, `mcp` and the other commands, which never take the terminal, the standard hook
/// is all there is.
fn install_panic_hook() {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let interface = std::thread::current().id();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Where, not what: a panic's message can quote the text it failed on. The
        // whole message still goes to the terminal, below, for a panic that ends the app.
        match info.location() {
            Some(at) => tracing::error!(file = place(at.file()), line = at.line(), "panic"),
            None => tracing::error!("panic"),
        }
        let on_interface = std::thread::current().id() == interface;
        match on_panic(on_interface, on_screen()) {
            OnPanic::HandBack => {
                restore();
                previous(info);
            }
            OnPanic::LogOnly => {}
        }
    }));
}

/// What the panic hook does besides the log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OnPanic {
    /// Hands the terminal back and prints the panic, as the standard hook does.
    HandBack,
    /// Nothing: the interface goes on, on a screen that is left as it is.
    LogOnly,
}

/// What a panic is to the screen: `on_interface`, it is on the thread that runs the
/// interface; `on_screen`, the app has the terminal ([`on_screen`]).
///
/// A panic on the interface's thread ends the app: the terminal is handed back so that
/// the shell after it works, and the panic is printed there. One on another thread ends
/// only its own task or thread, or is caught (tokio takes a panicking task as a failed
/// one; `matrix::paging` catches matrix-sdk's of matrix-rust-sdk #5416, but the hook runs
/// before any catch), and the app goes on: handing the terminal back under it, and
/// printing over its screen, left it running on a broken terminal. When nothing of the
/// app is on the terminal, there is nothing to break.
fn on_panic(on_interface: bool, on_screen: bool) -> OnPanic {
    if on_interface || !on_screen {
        OnPanic::HandBack
    } else {
        OnPanic::LogOnly
    }
}

/// Whether the app has the terminal: raw mode, or its screen (between [`release`] and
/// [`resume`], around Ctrl-Z and the external editor, it has neither).
fn on_screen() -> bool {
    RAW.load(Ordering::SeqCst) || ACTIVE.load(Ordering::SeqCst)
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
    use crossterm::Command;
    use ratatui::Terminal;
    use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
    use ratatui::buffer::Cell;
    use ratatui::layout::{Position, Size};
    use ratatui::widgets::Paragraph;

    use super::{EnableMouseButtons, OnPanic, POP_TITLE, PUSH_TITLE, base64, on_panic, place};

    /// A terminal that cannot say where its cursor is, as while the event stream's reader
    /// thread waits: asking returns an error instead of the two-second wait.
    struct NoCursorAnswer(TestBackend);

    impl Backend for NoCursorAnswer {
        type Error = std::io::Error;

        fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.0.draw(content).map_err(|never| match never {})
        }
        fn hide_cursor(&mut self) -> std::io::Result<()> {
            self.0.hide_cursor().map_err(|never| match never {})
        }
        fn show_cursor(&mut self) -> std::io::Result<()> {
            self.0.show_cursor().map_err(|never| match never {})
        }
        fn get_cursor_position(&mut self) -> std::io::Result<Position> {
            Err(std::io::Error::other("not read within a normal duration"))
        }
        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> std::io::Result<()> {
            self.0
                .set_cursor_position(position)
                .map_err(|never| match never {})
        }
        fn clear(&mut self) -> std::io::Result<()> {
            self.0.clear().map_err(|never| match never {})
        }
        fn clear_region(&mut self, clear_type: ClearType) -> std::io::Result<()> {
            self.0
                .clear_region(clear_type)
                .map_err(|never| match never {})
        }
        fn size(&self) -> std::io::Result<Size> {
            self.0.size().map_err(|never| match never {})
        }
        fn window_size(&mut self) -> std::io::Result<WindowSize> {
            self.0.window_size().map_err(|never| match never {})
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush().map_err(|never| match never {})
        }
    }

    fn screen(terminal: &Terminal<NoCursorAnswer>) -> String {
        let buffer = terminal.backend().0.buffer();
        buffer.content().iter().map(Cell::symbol).collect()
    }

    #[test]
    fn clearing_asks_the_terminal_nothing_and_the_next_draw_writes_every_cell() {
        let mut terminal = Terminal::new(NoCursorAnswer(TestBackend::new(12, 2))).unwrap();
        let draw = |terminal: &mut Terminal<NoCursorAnswer>| {
            terminal
                .draw(|frame| frame.render_widget(Paragraph::new("hello"), frame.area()))
                .unwrap();
        };
        draw(&mut terminal);
        assert!(screen(&terminal).starts_with("hello"));
        // ratatui's own clear would fail here: it asks where the cursor is first.
        super::clear(&mut terminal).expect("clearing must not ask for the cursor");
        assert_eq!(screen(&terminal).trim(), "", "the screen is cleared");
        // The same frame again: written in full, not left out as unchanged.
        draw(&mut terminal);
        assert!(screen(&terminal).starts_with("hello"));
    }

    #[test]
    fn mouse_capture_enables_buttons_and_drag_without_any_motion() {
        let mut ansi = String::new();
        EnableMouseButtons.write_ansi(&mut ansi).unwrap();
        assert!(ansi.contains("\x1b[?1000h"));
        assert!(ansi.contains("\x1b[?1002h"));
        assert!(ansi.contains("\x1b[?1015h"));
        assert!(ansi.contains("\x1b[?1006h"));
        assert!(!ansi.contains("1003"));
    }

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
    fn only_a_panic_that_ends_the_app_hands_the_terminal_back() {
        // On the interface's thread: the app ends, and the shell gets the terminal back
        // with the panic printed on it.
        assert_eq!(on_panic(true, true), OnPanic::HandBack);
        assert_eq!(on_panic(true, false), OnPanic::HandBack);
        // On another thread (a task of the Matrix worker, a picture being decoded): the
        // app goes on, on its screen, which is left alone.
        assert_eq!(on_panic(false, true), OnPanic::LogOnly);
        // ...unless the app does not have the terminal (the editor has it, or the app
        // has ended): then it is as before.
        assert_eq!(on_panic(false, false), OnPanic::HandBack);
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
