//! The terminal's shell: the event loop ([`run`]), which carries out what the app asks
//! of the terminal and the machine and hands it what happens there, and entering and
//! leaving the terminal's TUI state, in one place.
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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use crossterm::{execute, queue};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Rect;
use tokio::signal::unix::{SignalKind, signal};

use crate::app::{App, OsRequest};
use crate::config::Config;
use crate::matrix::{self, MatrixEvent};
use crate::paths::Paths;
use crate::rt::{self, MissedTickBehavior};
use crate::time::Instant;
use crate::ui;

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

/// Hands a link or a file to the desktop's program for it. Best effort; the login
/// form also shows the address to open by hand.
fn open_in_browser(url: &str) {
    use std::process::{Command, Stdio};
    let _ = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Does what the app asked of the desktop.
fn carry_out(requests: Vec<OsRequest>) {
    for request in requests {
        match request {
            OsRequest::Open(target) => open_in_browser(&target),
            OsRequest::RaiseWindow(command) => crate::focus::raise_window(&command),
        }
    }
}

/// Tells the terminal to free the pictures that are no longer shown (`all`: every
/// picture, before quitting). Written between frames, never during a draw.
fn flush_images(app: &mut App, all: bool) {
    use std::io::Write;
    let sequence = app.image_cleanup(all);
    if !sequence.is_empty() {
        let mut out = std::io::stdout();
        let _ = out.write_all(sequence.as_bytes());
        let _ = out.flush();
    }
}

/// Waits up to `grace` for `tasks` to end on their own. Returns `false` if any task
/// outlived the grace and had to be aborted (after aborting, drains briefly so drop
/// handlers — such as killing child process groups — run before returning).
async fn drain_or_abort(tasks: &mut rt::JoinSet<()>, grace: Duration) -> bool {
    if tasks.is_empty() {
        return true;
    }
    let done = async { while tasks.join_next().await.is_some() {} };
    if rt::timeout(grace, done).await.is_ok() {
        return true;
    }
    tasks.abort_all();
    let aborted = async { while tasks.join_next().await.is_some() {} };
    let _ = rt::timeout(Duration::from_millis(500), aborted).await;
    false
}

pub async fn run(paths: &Paths, config: &Config, demo: bool, target: Option<String>) -> Result<()> {
    let (portal_scheme, mut scheme_watch) = crate::portal::SchemeWatch::start().await;
    let guard = TerminalGuard::enter(config.mouse, config.window_title)?;
    // Terminal queries (image protocol, background colour) go here: on the alternate
    // screen, where a terminal that prints a question instead of answering does no
    // harm, but before mouse and focus reports are on and before the event stream.
    guard.alternate_screen()?;
    let (graphics, term_reply) =
        crate::images::Graphics::detect_with_term(config.images, portal_scheme.is_none());
    let initial_scheme = crate::portal::resolve_initial_scheme(portal_scheme, term_reply);
    let mut terminal = guard.activate()?;
    let mut events = EventStream::new();
    let mut tick = rt::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Without these, a kill or a closed terminal window would skip all cleanup.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;
    // Before the worker starts, as the signals above: an error from here on is returned
    // only after the worker was shut down (see `outcome` below).
    let mut theme_watch = crate::watch::ThemeWatch::start(paths)?;
    let (matrix, mut matrix_events) = if demo {
        (None, None)
    } else {
        let (handle, events) = matrix::spawn_with(paths.clone(), config.secret_store);
        (Some(handle), Some(events))
    };
    let mut app = App::new_in_scheme(
        config,
        Box::new(crate::storage::FsStorage::new(paths)),
        Box::new(crate::probes::NativeProbes),
        matrix.clone(),
        initial_scheme,
    );
    // As the terminal answered before the event stream existed: never asked again.
    app.found.keyboard_enhanced = Some(guard.keyboard_enhanced());
    // The demo is for looking around: what is changed there is not written into the
    // real configuration.
    if !demo {
        app.config_stored = true;
        // Read when a session is up: whose list it is must be known first.
        app.bookmarks.stored = true;
    }
    app.startup_target = target;
    // Copies of sounds and mpv's sockets; what an earlier run left there goes now.
    crate::audio::remove_leftovers(&paths.audio_dir());
    app.set_audio_dir(paths.audio_dir());
    // The demo keeps no notes in a file, as it writes no settings.
    if !demo {
        app.keep_notes_stored(paths.audio_dir());
    }
    if demo {
        app.add_demo_space();
        app.maybe_open_startup_target();
    }
    let notify_icons = Arc::new(crate::notify::IconCache::new(paths.notify_icons_dir()));
    struct NotifyCacheGuard(Arc<crate::notify::IconCache>);
    impl Drop for NotifyCacheGuard {
        fn drop(&mut self) {
            self.0.clear();
        }
    }
    let _notify_cache_guard = NotifyCacheGuard(Arc::clone(&notify_icons));
    app.set_notify_icons(notify_icons);
    let mut encoded_images = app.start_images(graphics);
    // Clicks on desktop notifications come back here, as the room to open.
    let (notice_clicks, mut clicked) =
        tokio::sync::mpsc::unbounded_channel::<crate::notify::Click>();
    // Pictures read from the clipboard come back here.
    let (pasted, mut pasted_back) = tokio::sync::mpsc::unbounded_channel();
    // What the audio players say comes back here; their tasks end with them.
    let (player_news, mut player_back) = tokio::sync::mpsc::unbounded_channel();
    let mut players = rt::JoinSet::new();
    // And what a recording says; kept separate so releasing the terminal for `$EDITOR`
    // can drain recordings without disturbing playback.
    let (record_news, mut record_back) = tokio::sync::mpsc::unbounded_channel();
    let mut recorders = rt::JoinSet::new();
    // So do the programs that transcribe and translate, and what they wrote.
    let (note_news, mut note_back) = tokio::sync::mpsc::unbounded_channel();
    let mut note_programs = rt::JoinSet::new();
    // The same for the summary command of "catch up".
    let (summary_news, mut summary_back) = tokio::sync::mpsc::unbounded_channel();
    let mut summaries = rt::JoinSet::new();
    // What the worker said while the external editor had the terminal.
    let mut held_events = external_editor::HeldEvents::default();

    // An error ends the loop, not the function: what runs after it (players stopped,
    // the worker shut down cleanly) runs for an error too, and the error is returned
    // then. Returned at once, it dropped the runtime under the worker, whose store
    // write in flight was cancelled; matrix-sdk-sqlite panics over that.
    let outcome = async {
        while !app.should_quit {
            // What the app asked of the desktop, as soon as it asked.
            carry_out(app.take_os_requests());
            if app.suspend_requested {
                app.suspend_requested = false;
                // What played or recorded was told to stop: wait for them to end, and abort
                // any that outlive the grace so no child process group stays alive across
                // SIGTSTP (they are not stopped with the terminal).
                let _ = drain_or_abort(&mut players, Duration::from_secs(1)).await;
                if !drain_or_abort(&mut recorders, Duration::from_secs(2)).await {
                    app.cancel_recording();
                }
                // The terminal frees its copies of the pictures now, in case we are
                // never resumed; they are sent again when drawn.
                flush_images(&mut app, true);
                suspend(&mut terminal)?;
                // The shell's title was put back meanwhile.
                app.window_title_lost();
                app.dirty = true;
            }
            if let Some(request) = app.take_editor_request() {
                // A running recording was told to stop when the editor was requested; wait
                // for it to finish (or kill it) before giving the terminal to the editor.
                if !drain_or_abort(&mut recorders, Duration::from_secs(2)).await {
                    app.cancel_recording();
                }
                // As for Ctrl-Z: the terminal frees its copies of the pictures now.
                flush_images(&mut app, true);
                // Its reader thread would take the keys meant for the editor: the stream
                // is dropped, and a new one is read from once we are back.
                drop(std::mem::replace(&mut events, EventStream::new()));
                rt::sleep(Duration::from_millis(50)).await;
                absorb_reader_wake();
                release();
                let dir = paths.runtime_dir.clone();
                // A blocking child: the worker, a task of its own, goes on syncing; what it
                // says meanwhile is held, each snapshot once, and handled when we are back.
                let editor = tokio::task::spawn_blocking(move || {
                    crate::editor::edit(&request, &dir, crate::editor::run_command)
                });
                let outcome = external_editor::hold_events_while(
                    editor,
                    matrix_events.as_mut(),
                    &mut held_events,
                )
                .await
                .unwrap_or(crate::editor::Outcome::Failed(
                    crate::editor::Failure::Launch,
                ));
                absorb_reader_wake();
                let resumed = resume(&mut terminal);
                // The screen was cleared: pictures are sent again when drawn. The title
                // from before was put back for the editor: ours is written again too.
                app.images.forget_encoded();
                app.window_title_lost();
                app.finish_editor(outcome);
                resumed?;
            }
            if let Some(request) = app.take_paste_request() {
                // Off the UI thread: the tool may take its time (up to its limit), and the
                // picture is decoded to check it. Messages and keys are handled meanwhile.
                let dir = paths.runtime_dir.clone();
                let back = pasted.clone();
                rt::spawn(async move {
                    let id = request.id;
                    let result = tokio::task::spawn_blocking(move || {
                        crate::clipboard::paste(&request, &dir, crate::clipboard::run_tool)
                    })
                    .await
                    .unwrap_or(Err(crate::clipboard::Failure::Failed));
                    // Nobody to take it only once the app has ended: then the picture's
                    // file is removed as the answer is dropped.
                    let _ = back.send(crate::clipboard::Done { id, result });
                });
            }
            if let Some(request) = app.take_audio_start() {
                // A task on the async threads, which live as long as the app: the player is
                // started from there (see `audio::play`).
                players.spawn(crate::audio::run(request, player_news.clone()));
            }
            if let Some(request) = app.take_record_start() {
                // As the players: the programs are tied to the thread that starts them.
                recorders.spawn(crate::audio::record::run(request, record_news.clone()));
            }
            while players.try_join_next().is_some() {}
            while recorders.try_join_next().is_some() {}
            for request in app.take_note_starts() {
                note_programs.spawn(crate::textcmd::run(request, note_news.clone()));
            }
            while note_programs.try_join_next().is_some() {}
            if let Some(request) = app.take_summary_start() {
                summaries.spawn(crate::catchup::run(request, summary_news.clone()));
            }
            while summaries.try_join_next().is_some() {}
            set_mouse(app.mouse)?;
            set_window_title(app.window_title)?;
            if std::mem::take(&mut app.repaint) {
                // As after a resize: pictures are sent again with the fresh screen.
                app.images.forget_encoded();
                clear(&mut terminal)?;
                app.dirty = true;
            }
            if let Some(text) = app.pending_copy.take() {
                copy_to_clipboard(&text)?;
            }
            let ready = app.take_notices_with_icons();
            for (notice, icon) in &ready.items {
                crate::notify::show(
                    ready.mode,
                    notice,
                    ready.preview,
                    icon,
                    ready.icon_cache.clone(),
                    Some(&notice_clicks),
                );
            }
            for (notice, icon) in &ready.reminders {
                crate::notify::show_reminder(
                    ready.mode,
                    notice,
                    ready.preview,
                    icon,
                    ready.icon_cache.clone(),
                    Some(&notice_clicks),
                );
            }
            if app.dirty {
                app.dirty = false;
                terminal.draw(|frame| ui::draw(frame, &mut app))?;
                app.after_draw();
                flush_images(&mut app, false);
                // Only when the count changed, and never with the setting off.
                if let Some(title) = app.take_window_title() {
                    write_window_title(&title)?;
                }
            }
            tokio::select! {
                event = events.next() => match event {
                    Some(Ok(event)) => app.on_event(event.into()),
                    Some(Err(err)) => return Err(err.into()),
                    None => break,
                },
                Some(event) = external_editor::next_matrix_event(
                    &mut held_events,
                    matrix_events.as_mut(),
                ) => app.on_matrix(event),
                Some(click) = clicked.recv() => app.on_matrix(MatrixEvent::NoticeClicked {
                    room_id: click.room_id,
                    event: click.event,
                }),
                Some(done) = pasted_back.recv() => app.finish_paste(done),
                Some(update) = player_back.recv() => app.on_audio(update),
                Some(done) = note_back.recv() => app.on_note_done(done),
                Some(done) = summary_back.recv() => app.on_summary(done),
                Some(update) = record_back.recv() => app.on_recording(update),
                _ = theme_watch.changed() => app.reload_theme(),
                scheme = scheme_watch.changed() => app.on_color_scheme(scheme),
                Some(done) = encoded_images.recv() => app.on_image_encoded(done),
                _ = wait_deadline(app.search_debounce_deadline()) => app.tick_global_search(),
                _ = wait_deadline(app.user_search_debounce_deadline()) => app.tick_user_search(),
                _ = wait_deadline(app.dir_search_debounce_deadline()) => app.tick_dir_search(),
                _ = tick.tick() => app.on_tick(),
                _ = tokio::signal::ctrl_c() => app.should_quit = true,
                _ = sigterm.recv() => app.should_quit = true,
                _ = sighup.recv() => app.should_quit = true,
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    flush_images(&mut app, true);
    // What plays stops with the app, and its file goes; a player that does not end in
    // time is killed as its task is dropped.
    app.stop_audio();
    // So do the programs that transcribe and translate, and a recording.
    app.stop_notes();
    app.cancel_recording();
    let stopped = async {
        while players.join_next().await.is_some() {}
        while recorders.join_next().await.is_some() {}
        while note_programs.join_next().await.is_some() {}
    };
    let _ = rt::timeout(Duration::from_secs(2), stopped).await;
    drop(players);
    drop(recorders);
    drop(note_programs);
    // A summary command that still runs is killed as its task is dropped.
    drop(summaries);
    // Let the worker finish handing any queued files to the send queue and stop
    // syncing cleanly, while `app` stays alive so temporary voice and picture files
    // stay on disk until the worker has read them.
    if let (Some(matrix), Some(mut events)) = (matrix, matrix_events) {
        let wait = if app.attach.sends_out() {
            Duration::from_secs(7)
        } else {
            Duration::from_secs(3)
        };
        // The worker takes commands in order: the draft is written before it stops.
        app.shut_down_worker();
        // Close every command sender so the worker also sees the channel end.
        drop(matrix);
        let drained = async {
            while let Some(event) = events.recv().await {
                // A logout that ends while the app closes still takes along what the
                // account kept here.
                if matches!(event, MatrixEvent::LoggedOut) {
                    app.on_matrix(event);
                }
            }
        };
        let _ = rt::timeout(wait, drained).await;
    }
    drop(app);
    crate::audio::remove_leftovers(&paths.audio_dir());
    outcome
}

async fn wait_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(at) => rt::sleep_until(at).await,
        None => std::future::pending().await,
    }
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

    #[tokio::test]
    async fn drain_or_abort_waits_for_quick_tasks_and_aborts_hung_ones() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct OnDrop(Arc<AtomicBool>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let mut quick = tokio::task::JoinSet::new();
        let finished = Arc::new(AtomicBool::new(false));
        let mark = Arc::clone(&finished);
        quick.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            mark.store(true, Ordering::SeqCst);
        });
        assert!(super::drain_or_abort(&mut quick, std::time::Duration::from_secs(1)).await);
        assert!(finished.load(Ordering::SeqCst));

        let mut hung = tokio::task::JoinSet::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = OnDrop(Arc::clone(&dropped));
        hung.spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        // Let the spawned task start so the drop guard lives inside its future.
        tokio::task::yield_now().await;
        assert!(!super::drain_or_abort(&mut hung, std::time::Duration::from_millis(30)).await);
        assert!(dropped.load(Ordering::SeqCst));
        assert!(hung.is_empty());
    }
}
