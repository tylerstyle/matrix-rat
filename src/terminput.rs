//! Async Unix terminal input reader handling keys, mouse, focus, bracketed paste,
//! and live DEC mode 2031 / OSC 11 colour-scheme notifications.
//!
//! `crossterm 0.29` routes unknown `CSI ? ...` sequences (such as kitty's `CSI ?997;1n`
//! and `CSI ?997;2n` colour-scheme reports) through a branch that never resets its input
//! buffer, stalling subsequent key presses. Owning the tty input parser here avoids that
//! bug while still producing `crossterm::event::Event` values for the rest of the app.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::thread::JoinHandle;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MediaKeyCode,
    ModifierKeyCode, MouseButton, MouseEvent, MouseEventKind,
};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::termquery;
use crate::theme::ColorScheme;

const READ_CHUNK: usize = 4096;
const ESC_TIMEOUT_MS: i32 = 15;
const INCOMPLETE_SEQ_TIMEOUT_MS: i32 = 100;
const MAX_CSI_LEN: usize = 256;
const MAX_CONTROL_LEN: usize = 4096;
const MAX_PASTE_LEN: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermInput {
    Event(Event),
    ColorScheme(ColorScheme),
}

enum InputFd {
    Borrowed(RawFd),
    Owned(OwnedFd),
}

impl InputFd {
    fn open_tty() -> io::Result<Self> {
        // SAFETY: `isatty` only inspects fd 0.
        if unsafe { libc::isatty(libc::STDIN_FILENO) == 1 } {
            Ok(Self::Borrowed(libc::STDIN_FILENO))
        } else {
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")?;
            Ok(Self::Owned(file.into()))
        }
    }

    fn as_raw_fd(&self) -> RawFd {
        match self {
            Self::Borrowed(fd) => *fd,
            Self::Owned(fd) => fd.as_raw_fd(),
        }
    }
}

/// Async stream of terminal events and live colour-scheme updates. Dropping stops the
/// background reader thread promptly without closing stdin.
pub struct TermEvents {
    rx: UnboundedReceiver<TermInput>,
    wake_tx: Option<OwnedFd>,
    thread: Option<JoinHandle<()>>,
}

impl TermEvents {
    pub fn spawn() -> io::Result<Self> {
        let tty = InputFd::open_tty()?;
        Self::from_input_fd(tty)
    }

    #[cfg(test)]
    pub(crate) fn from_fd(tty: OwnedFd) -> io::Result<Self> {
        Self::from_input_fd(InputFd::Owned(tty))
    }

    fn from_input_fd(tty: InputFd) -> io::Result<Self> {
        let (wake_rx, wake_tx) = make_wake_pipe()?;
        let (tx, rx) = unbounded_channel();
        let thread = std::thread::Builder::new()
            .name("term-input".to_owned())
            .spawn(move || reader_loop(tty, wake_rx, tx))?;
        Ok(Self {
            rx,
            wake_tx: Some(wake_tx),
            thread: Some(thread),
        })
    }

    pub async fn next(&mut self) -> Option<TermInput> {
        self.rx.recv().await
    }
}

impl Drop for TermEvents {
    fn drop(&mut self) {
        if let Some(wake_tx) = self.wake_tx.take() {
            let byte = [1u8; 1];
            // SAFETY: valid 1-byte buffer and owned pipe fd.
            unsafe {
                libc::write(wake_tx.as_raw_fd(), byte.as_ptr().cast(), byte.len());
            }
            drop(wake_tx);
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

fn make_wake_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1 as libc::c_int; 2];
    // SAFETY: `fds` points to 2 ints as required by `pipe2`.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe2` returned two newly opened descriptors.
    let rx = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: same.
    let tx = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((rx, tx))
}

fn reader_loop(tty: InputFd, wake_rx: OwnedFd, tx: tokio::sync::mpsc::UnboundedSender<TermInput>) {
    let mut parser = TermParser::default();
    let mut chunk = [0u8; READ_CHUNK];
    let mut fds = [
        libc::pollfd {
            fd: tty.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake_rx.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];

    loop {
        for item in &mut fds {
            item.revents = 0;
        }
        let timeout = parser.poll_timeout_ms();
        // SAFETY: `fds` holds 2 valid pollfd entries.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if rc < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if fds[1].revents != 0 {
            break;
        }
        if rc == 0 {
            parser.flush_timeout();
            if !flush_outputs(&mut parser, &tx) {
                break;
            }
            continue;
        }
        if fds[0].revents & libc::POLLIN != 0 {
            // SAFETY: `chunk` is valid for `chunk.len()` bytes.
            let n = unsafe { libc::read(tty.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
            if n < 0 {
                let kind = io::Error::last_os_error().kind();
                if matches!(kind, io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) {
                    continue;
                }
                break;
            }
            if n == 0 {
                parser.flush_timeout();
                let _ = flush_outputs(&mut parser, &tx);
                break;
            }
            parser.feed(&chunk[..n as usize]);
            if !flush_outputs(&mut parser, &tx) {
                break;
            }
        } else if fds[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            parser.flush_timeout();
            let _ = flush_outputs(&mut parser, &tx);
            break;
        }
    }
}

fn flush_outputs(
    parser: &mut TermParser,
    tx: &tokio::sync::mpsc::UnboundedSender<TermInput>,
) -> bool {
    while let Some(item) = parser.next_output() {
        if tx.send(item).is_err() {
            return false;
        }
    }
    true
}

#[derive(Debug, Default)]
pub struct TermParser {
    buf: Vec<u8>,
    out: VecDeque<TermInput>,
}

impl TermParser {
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        self.advance();
    }

    /// Resolves an ambiguous pending prefix (such as a lone `ESC` keypress or `Alt+]`
    /// / `Alt+[`) when no trailing bytes arrive within the escape wait.
    pub fn flush_timeout(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        if self.buf == b"\x1b" {
            self.buf.clear();
            self.out
                .push_back(key_input(KeyCode::Esc, KeyModifiers::NONE));
            return;
        }
        if self.buf.len() == 2 && self.buf[0] == b'\x1b' {
            let second = self.buf[1];
            self.buf.clear();
            if second.is_ascii_graphic() {
                let c = second as char;
                let mut mods = KeyModifiers::ALT;
                if c.is_uppercase() {
                    mods |= KeyModifiers::SHIFT;
                }
                self.out.push_back(key_input(KeyCode::Char(c), mods));
            }
            return;
        }
        // Incomplete non-paste escape/UTF-8 fragment timed out: drop leading byte so
        // the parser cannot wedge on malformed terminal output, then resume parsing.
        if !self.buf.starts_with(b"\x1b[200~") {
            self.buf.drain(..1);
            self.advance();
        }
    }

    pub fn next_output(&mut self) -> Option<TermInput> {
        self.out.pop_front()
    }

    fn poll_timeout_ms(&self) -> i32 {
        if self.buf.is_empty() || self.buf.starts_with(b"\x1b[200~") {
            -1
        } else if self.buf.len() <= 2 && self.buf[0] == b'\x1b' {
            ESC_TIMEOUT_MS
        } else {
            INCOMPLETE_SEQ_TIMEOUT_MS
        }
    }

    fn advance(&mut self) {
        while !self.buf.is_empty() {
            match self.step() {
                Step::Consumed(n) => {
                    self.buf.drain(..n);
                }
                Step::Incomplete => break,
            }
        }
    }

    fn step(&mut self) -> Step {
        if self.buf[0] != b'\x1b' {
            return self.step_plain(0, KeyModifiers::NONE);
        }
        if self.buf.len() == 1 {
            return Step::Incomplete;
        }
        match self.buf[1] {
            b'\x1b' => {
                self.out
                    .push_back(key_input(KeyCode::Esc, KeyModifiers::NONE));
                Step::Consumed(2)
            }
            b'[' => self.step_csi(),
            b'O' => self.step_ss3(),
            kind @ (b']' | b'_' | b'P' | b'^' | b'X') => self.step_string(kind),
            _ => self.step_plain(1, KeyModifiers::ALT),
        }
    }

    fn step_plain(&mut self, offset: usize, extra_mods: KeyModifiers) -> Step {
        let slice = &self.buf[offset..];
        let Some(&b) = slice.first() else {
            return Step::Incomplete;
        };
        let (code, mods, used) = match b {
            b'\r' => (KeyCode::Enter, KeyModifiers::NONE, 1),
            b'\t' => (KeyCode::Tab, KeyModifiers::NONE, 1),
            b'\x7F' => (KeyCode::Backspace, KeyModifiers::NONE, 1),
            b'\0' => (KeyCode::Char(' '), KeyModifiers::CONTROL, 1),
            0x01..=0x1A => (
                KeyCode::Char((b - 1 + b'a') as char),
                KeyModifiers::CONTROL,
                1,
            ),
            0x1C..=0x1F => (
                KeyCode::Char((b - 0x1C + b'4') as char),
                KeyModifiers::CONTROL,
                1,
            ),
            0x20..=0x7E => {
                let c = b as char;
                let mods = if c.is_uppercase() {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                };
                (KeyCode::Char(c), mods, 1)
            }
            _ => match utf8_len(b) {
                None => return Step::Consumed(offset + 1),
                Some(len) => {
                    if slice.len() < len {
                        // Bail early if any existing continuation byte is already invalid.
                        if slice[1..].iter().any(|&c| c & 0b1100_0000 != 0b1000_0000) {
                            return Step::Consumed(offset + 1);
                        }
                        return Step::Incomplete;
                    }
                    let Ok(s) = std::str::from_utf8(&slice[..len]) else {
                        return Step::Consumed(offset + 1);
                    };
                    let Some(c) = s.chars().next() else {
                        return Step::Consumed(offset + 1);
                    };
                    let mods = if c.is_uppercase() {
                        KeyModifiers::SHIFT
                    } else {
                        KeyModifiers::NONE
                    };
                    (KeyCode::Char(c), mods, len)
                }
            },
        };
        self.out.push_back(key_input(code, mods | extra_mods));
        Step::Consumed(offset + used)
    }

    fn step_ss3(&mut self) -> Step {
        if self.buf.len() < 3 {
            return Step::Incomplete;
        }
        let code = match self.buf[2] {
            b'D' => Some(KeyCode::Left),
            b'C' => Some(KeyCode::Right),
            b'A' => Some(KeyCode::Up),
            b'B' => Some(KeyCode::Down),
            b'H' => Some(KeyCode::Home),
            b'F' => Some(KeyCode::End),
            v @ b'P'..=b'S' => Some(KeyCode::F(1 + v - b'P')),
            _ => None,
        };
        if let Some(code) = code {
            self.out.push_back(key_input(code, KeyModifiers::NONE));
        }
        Step::Consumed(3)
    }

    fn step_string(&mut self, kind: u8) -> Step {
        if self.buf.len() == 2 {
            return Step::Incomplete;
        }
        let mut i = 2;
        while i < self.buf.len() {
            if kind == b']' && self.buf[i] == b'\x07' {
                if let Some(scheme) = termquery::parse_osc11_payload(&self.buf[2..i]) {
                    self.out.push_back(TermInput::ColorScheme(scheme));
                }
                return Step::Consumed(i + 1);
            }
            if self.buf[i] == b'\x1b' {
                let Some(&next) = self.buf.get(i + 1) else {
                    return Step::Incomplete;
                };
                if next == b'\\' {
                    if kind == b']'
                        && let Some(scheme) = termquery::parse_osc11_payload(&self.buf[2..i])
                    {
                        self.out.push_back(TermInput::ColorScheme(scheme));
                    }
                    return Step::Consumed(i + 2);
                }
            }
            i += 1;
            if i > MAX_CONTROL_LEN {
                return Step::Consumed(2);
            }
        }
        Step::Incomplete
    }

    fn step_csi(&mut self) -> Step {
        if self.buf.len() == 2 {
            return Step::Incomplete;
        }
        if self.buf[2] == b'M' {
            if self.buf.len() < 6 {
                return Step::Incomplete;
            }
            if let Some(ev) = parse_normal_mouse(&self.buf[..6]) {
                self.out.push_back(TermInput::Event(ev));
            }
            return Step::Consumed(6);
        }
        if self.buf[2] == b'[' {
            if self.buf.len() < 4 {
                return Step::Incomplete;
            }
            if let v @ b'A'..=b'E' = self.buf[3] {
                self.out
                    .push_back(key_input(KeyCode::F(1 + v - b'A'), KeyModifiers::NONE));
            }
            return Step::Consumed(4);
        }
        if self.buf.starts_with(b"\x1b[200~")
            || (self.buf.len() < 6 && b"\x1b[200~".starts_with(&self.buf))
        {
            if self.buf.len() < 6 {
                return Step::Incomplete;
            }
            if let Some(pos) = find_subslice(&self.buf[6..], b"\x1b[201~") {
                let paste = String::from_utf8_lossy(&self.buf[6..6 + pos]).into_owned();
                self.out.push_back(TermInput::Event(Event::Paste(paste)));
                return Step::Consumed(6 + pos + 6);
            }
            if self.buf.len() > MAX_PASTE_LEN {
                return Step::Consumed(self.buf.len());
            }
            return Step::Incomplete;
        }

        let mut i = 2;
        while i < self.buf.len() {
            let b = self.buf[i];
            if (0x40..=0x7E).contains(&b) {
                if let Some(item) = parse_csi_complete(&self.buf[..=i]) {
                    self.out.push_back(item);
                }
                return Step::Consumed(i + 1);
            }
            if !(0x20..=0x3F).contains(&b) {
                // Unexpected control/non-CSI byte interrupts the sequence; drop only
                // up to here so the interrupting byte (e.g. a new ESC) still parses.
                return Step::Consumed(i);
            }
            i += 1;
            if i > MAX_CSI_LEN {
                return Step::Consumed(2);
            }
        }
        Step::Incomplete
    }
}

enum Step {
    Consumed(usize),
    Incomplete,
}

fn key_input(code: KeyCode, modifiers: KeyModifiers) -> TermInput {
    TermInput::Event(Event::Key(KeyEvent::new(code, modifiers)))
}

fn utf8_len(lead: u8) -> Option<usize> {
    match lead {
        0xC2..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF4 => Some(4),
        _ => None,
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_csi_complete(seq: &[u8]) -> Option<TermInput> {
    let last = *seq.last()?;
    let inner = std::str::from_utf8(&seq[2..seq.len() - 1]).ok()?;

    if inner.starts_with('?') {
        if last == b'n'
            && let Some(scheme) = termquery::parse_color_scheme_csi(inner)
        {
            return Some(TermInput::ColorScheme(scheme));
        }
        return None;
    }

    if inner.is_empty() {
        return match last {
            b'A' => Some(key_input(KeyCode::Up, KeyModifiers::NONE)),
            b'B' => Some(key_input(KeyCode::Down, KeyModifiers::NONE)),
            b'C' => Some(key_input(KeyCode::Right, KeyModifiers::NONE)),
            b'D' => Some(key_input(KeyCode::Left, KeyModifiers::NONE)),
            b'F' => Some(key_input(KeyCode::End, KeyModifiers::NONE)),
            b'H' => Some(key_input(KeyCode::Home, KeyModifiers::NONE)),
            b'Z' => Some(TermInput::Event(Event::Key(KeyEvent::new_with_kind(
                KeyCode::BackTab,
                KeyModifiers::SHIFT,
                KeyEventKind::Press,
            )))),
            b'I' => Some(TermInput::Event(Event::FocusGained)),
            b'O' => Some(TermInput::Event(Event::FocusLost)),
            b'P' => Some(key_input(KeyCode::F(1), KeyModifiers::NONE)),
            b'Q' => Some(key_input(KeyCode::F(2), KeyModifiers::NONE)),
            b'S' => Some(key_input(KeyCode::F(4), KeyModifiers::NONE)),
            _ => None,
        };
    }

    if let Some(sgr) = inner.strip_prefix('<')
        && matches!(last, b'M' | b'm')
    {
        return parse_sgr_mouse(sgr, last == b'm').map(TermInput::Event);
    }

    match last {
        b'~' => parse_special_key(inner).map(TermInput::Event),
        b'u' => parse_csi_u_key(inner).map(TermInput::Event),
        b'M' => parse_rxvt_mouse(inner).map(TermInput::Event),
        b'R' => {
            if inner.starts_with(';') || inner.starts_with("1;") {
                parse_modifier_key(inner, last).map(TermInput::Event)
            } else {
                None
            }
        }
        b'A' | b'B' | b'C' | b'D' | b'F' | b'H' | b'P' | b'Q' | b'S' => {
            parse_modifier_key(inner, last).map(TermInput::Event)
        }
        _ => None,
    }
}

fn parse_modifier_and_kind(part: &str) -> Option<(u8, u8)> {
    let mut sub = part.split(':');
    let mask = sub.next()?.parse::<u8>().ok()?;
    let kind = sub.next().and_then(|k| k.parse::<u8>().ok()).unwrap_or(1);
    Some((mask, kind))
}

fn decode_modifiers(mask: u8) -> KeyModifiers {
    let bits = mask.saturating_sub(1);
    let mut mods = KeyModifiers::empty();
    if bits & 1 != 0 {
        mods |= KeyModifiers::SHIFT;
    }
    if bits & 2 != 0 {
        mods |= KeyModifiers::ALT;
    }
    if bits & 4 != 0 {
        mods |= KeyModifiers::CONTROL;
    }
    if bits & 8 != 0 {
        mods |= KeyModifiers::SUPER;
    }
    if bits & 16 != 0 {
        mods |= KeyModifiers::HYPER;
    }
    if bits & 32 != 0 {
        mods |= KeyModifiers::META;
    }
    mods
}

fn decode_state(mask: u8) -> KeyEventState {
    let bits = mask.saturating_sub(1);
    let mut state = KeyEventState::empty();
    if bits & 64 != 0 {
        state |= KeyEventState::CAPS_LOCK;
    }
    if bits & 128 != 0 {
        state |= KeyEventState::NUM_LOCK;
    }
    state
}

fn decode_kind(kind: u8) -> KeyEventKind {
    match kind {
        2 => KeyEventKind::Repeat,
        3 => KeyEventKind::Release,
        _ => KeyEventKind::Press,
    }
}

fn parse_modifier_key(inner: &str, final_byte: u8) -> Option<Event> {
    let mut split = inner.split(';');
    let first = split.next()?;
    let (mods, kind) = if let Some(part) = split.next()
        && let Some((mask, kind_code)) = parse_modifier_and_kind(part)
    {
        (decode_modifiers(mask), decode_kind(kind_code))
    } else if let Some(digit) = first.chars().last().and_then(|c| c.to_digit(10)) {
        (decode_modifiers(digit as u8), KeyEventKind::Press)
    } else {
        (KeyModifiers::NONE, KeyEventKind::Press)
    };
    let code = match final_byte {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'F' => KeyCode::End,
        b'H' => KeyCode::Home,
        b'P' => KeyCode::F(1),
        b'Q' => KeyCode::F(2),
        b'R' => KeyCode::F(3),
        b'S' => KeyCode::F(4),
        _ => return None,
    };
    Some(Event::Key(KeyEvent::new_with_kind(code, mods, kind)))
}

fn parse_special_key(inner: &str) -> Option<Event> {
    let mut split = inner.split(';');
    let first = split.next()?.parse::<u8>().ok()?;
    let (mods, kind, state) = if let Some(part) = split.next()
        && let Some((mask, kind_code)) = parse_modifier_and_kind(part)
    {
        (
            decode_modifiers(mask),
            decode_kind(kind_code),
            decode_state(mask),
        )
    } else {
        (KeyModifiers::NONE, KeyEventKind::Press, KeyEventState::NONE)
    };
    let code = match first {
        1 | 7 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        v @ 11..=15 => KeyCode::F(v - 10),
        v @ 17..=21 => KeyCode::F(v - 11),
        v @ 23..=26 => KeyCode::F(v - 12),
        v @ 28..=29 => KeyCode::F(v - 15),
        v @ 31..=34 => KeyCode::F(v - 17),
        _ => return None,
    };
    Some(Event::Key(KeyEvent::new_with_kind_and_state(
        code, mods, kind, state,
    )))
}

fn parse_csi_u_key(inner: &str) -> Option<Event> {
    let mut split = inner.split(';');
    let mut codepoints = split.next()?.split(':');
    let codepoint = codepoints.next()?.parse::<u32>().ok()?;
    let (mut mods, kind, state_mods) = if let Some(part) = split.next()
        && let Some((mask, kind_code)) = parse_modifier_and_kind(part)
    {
        (
            decode_modifiers(mask),
            decode_kind(kind_code),
            decode_state(mask),
        )
    } else {
        (KeyModifiers::NONE, KeyEventKind::Press, KeyEventState::NONE)
    };

    let (mut code, state_code) = if let Some(pair) = translate_functional_key(codepoint) {
        pair
    } else {
        let c = char::from_u32(codepoint)?;
        let code = match c {
            '\x1B' => KeyCode::Esc,
            '\r' => KeyCode::Enter,
            '\t' => {
                if mods.contains(KeyModifiers::SHIFT) {
                    KeyCode::BackTab
                } else {
                    KeyCode::Tab
                }
            }
            '\x7F' => KeyCode::Backspace,
            _ => KeyCode::Char(c),
        };
        (code, KeyEventState::empty())
    };

    if let KeyCode::Modifier(modifier) = code {
        match modifier {
            ModifierKeyCode::LeftAlt | ModifierKeyCode::RightAlt => mods |= KeyModifiers::ALT,
            ModifierKeyCode::LeftControl | ModifierKeyCode::RightControl => {
                mods |= KeyModifiers::CONTROL
            }
            ModifierKeyCode::LeftShift | ModifierKeyCode::RightShift => mods |= KeyModifiers::SHIFT,
            ModifierKeyCode::LeftSuper | ModifierKeyCode::RightSuper => mods |= KeyModifiers::SUPER,
            ModifierKeyCode::LeftHyper | ModifierKeyCode::RightHyper => mods |= KeyModifiers::HYPER,
            ModifierKeyCode::LeftMeta | ModifierKeyCode::RightMeta => mods |= KeyModifiers::META,
            _ => {}
        }
    }

    if mods.contains(KeyModifiers::SHIFT)
        && let Some(shifted) = codepoints
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .and_then(char::from_u32)
    {
        code = KeyCode::Char(shifted);
        mods.remove(KeyModifiers::SHIFT);
    }

    Some(Event::Key(KeyEvent::new_with_kind_and_state(
        code,
        mods,
        kind,
        state_code | state_mods,
    )))
}

fn translate_functional_key(codepoint: u32) -> Option<(KeyCode, KeyEventState)> {
    let keypad = match codepoint {
        57399 => Some(KeyCode::Char('0')),
        57400 => Some(KeyCode::Char('1')),
        57401 => Some(KeyCode::Char('2')),
        57402 => Some(KeyCode::Char('3')),
        57403 => Some(KeyCode::Char('4')),
        57404 => Some(KeyCode::Char('5')),
        57405 => Some(KeyCode::Char('6')),
        57406 => Some(KeyCode::Char('7')),
        57407 => Some(KeyCode::Char('8')),
        57408 => Some(KeyCode::Char('9')),
        57409 => Some(KeyCode::Char('.')),
        57410 => Some(KeyCode::Char('/')),
        57411 => Some(KeyCode::Char('*')),
        57412 => Some(KeyCode::Char('-')),
        57413 => Some(KeyCode::Char('+')),
        57414 => Some(KeyCode::Enter),
        57415 => Some(KeyCode::Char('=')),
        57416 => Some(KeyCode::Char(',')),
        57417 => Some(KeyCode::Left),
        57418 => Some(KeyCode::Right),
        57419 => Some(KeyCode::Up),
        57420 => Some(KeyCode::Down),
        57421 => Some(KeyCode::PageUp),
        57422 => Some(KeyCode::PageDown),
        57423 => Some(KeyCode::Home),
        57424 => Some(KeyCode::End),
        57425 => Some(KeyCode::Insert),
        57426 => Some(KeyCode::Delete),
        57427 => Some(KeyCode::KeypadBegin),
        _ => None,
    };
    if let Some(code) = keypad {
        return Some((code, KeyEventState::KEYPAD));
    }
    let other = match codepoint {
        57358 => KeyCode::CapsLock,
        57359 => KeyCode::ScrollLock,
        57360 => KeyCode::NumLock,
        57361 => KeyCode::PrintScreen,
        57362 => KeyCode::Pause,
        57363 => KeyCode::Menu,
        v @ 57376..=57398 => KeyCode::F((v - 57376 + 13) as u8),
        57428 => KeyCode::Media(MediaKeyCode::Play),
        57429 => KeyCode::Media(MediaKeyCode::Pause),
        57430 => KeyCode::Media(MediaKeyCode::PlayPause),
        57431 => KeyCode::Media(MediaKeyCode::Reverse),
        57432 => KeyCode::Media(MediaKeyCode::Stop),
        57433 => KeyCode::Media(MediaKeyCode::FastForward),
        57434 => KeyCode::Media(MediaKeyCode::Rewind),
        57435 => KeyCode::Media(MediaKeyCode::TrackNext),
        57436 => KeyCode::Media(MediaKeyCode::TrackPrevious),
        57437 => KeyCode::Media(MediaKeyCode::Record),
        57438 => KeyCode::Media(MediaKeyCode::LowerVolume),
        57439 => KeyCode::Media(MediaKeyCode::RaiseVolume),
        57440 => KeyCode::Media(MediaKeyCode::MuteVolume),
        57441 => KeyCode::Modifier(ModifierKeyCode::LeftShift),
        57442 => KeyCode::Modifier(ModifierKeyCode::LeftControl),
        57443 => KeyCode::Modifier(ModifierKeyCode::LeftAlt),
        57444 => KeyCode::Modifier(ModifierKeyCode::LeftSuper),
        57445 => KeyCode::Modifier(ModifierKeyCode::LeftHyper),
        57446 => KeyCode::Modifier(ModifierKeyCode::LeftMeta),
        57447 => KeyCode::Modifier(ModifierKeyCode::RightShift),
        57448 => KeyCode::Modifier(ModifierKeyCode::RightControl),
        57449 => KeyCode::Modifier(ModifierKeyCode::RightAlt),
        57450 => KeyCode::Modifier(ModifierKeyCode::RightSuper),
        57451 => KeyCode::Modifier(ModifierKeyCode::RightHyper),
        57452 => KeyCode::Modifier(ModifierKeyCode::RightMeta),
        57453 => KeyCode::Modifier(ModifierKeyCode::IsoLevel3Shift),
        57454 => KeyCode::Modifier(ModifierKeyCode::IsoLevel5Shift),
        _ => return None,
    };
    Some((other, KeyEventState::empty()))
}

fn parse_cb(cb: u8) -> Option<(MouseEventKind, KeyModifiers)> {
    let button_number = (cb & 0b0000_0011) | ((cb & 0b1100_0000) >> 4);
    let dragging = cb & 0b0010_0000 != 0;
    let kind = match (button_number, dragging) {
        (0, false) => MouseEventKind::Down(MouseButton::Left),
        (1, false) => MouseEventKind::Down(MouseButton::Middle),
        (2, false) => MouseEventKind::Down(MouseButton::Right),
        (0, true) => MouseEventKind::Drag(MouseButton::Left),
        (1, true) => MouseEventKind::Drag(MouseButton::Middle),
        (2, true) => MouseEventKind::Drag(MouseButton::Right),
        (3, false) => MouseEventKind::Up(MouseButton::Left),
        (3, true) | (4, true) | (5, true) => MouseEventKind::Moved,
        (4, false) => MouseEventKind::ScrollUp,
        (5, false) => MouseEventKind::ScrollDown,
        (6, false) => MouseEventKind::ScrollLeft,
        (7, false) => MouseEventKind::ScrollRight,
        _ => return None,
    };
    let mut mods = KeyModifiers::empty();
    if cb & 0b0000_0100 != 0 {
        mods |= KeyModifiers::SHIFT;
    }
    if cb & 0b0000_1000 != 0 {
        mods |= KeyModifiers::ALT;
    }
    if cb & 0b0001_0000 != 0 {
        mods |= KeyModifiers::CONTROL;
    }
    Some((kind, mods))
}

fn parse_normal_mouse(buf: &[u8]) -> Option<Event> {
    let cb = buf[3].checked_sub(32)?;
    let (kind, modifiers) = parse_cb(cb)?;
    let column = u16::from(buf[4].saturating_sub(32)).saturating_sub(1);
    let row = u16::from(buf[5].saturating_sub(32)).saturating_sub(1);
    Some(Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    }))
}

fn parse_rxvt_mouse(inner: &str) -> Option<Event> {
    let mut split = inner.split(';');
    let cb = split.next()?.parse::<u8>().ok()?.checked_sub(32)?;
    let (kind, modifiers) = parse_cb(cb)?;
    let column = split.next()?.parse::<u16>().ok()?.saturating_sub(1);
    let row = split.next()?.parse::<u16>().ok()?.saturating_sub(1);
    Some(Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    }))
}

fn parse_sgr_mouse(inner: &str, release: bool) -> Option<Event> {
    let mut split = inner.split(';');
    let cb = split.next()?.parse::<u8>().ok()?;
    let (mut kind, modifiers) = parse_cb(cb)?;
    let column = split.next()?.parse::<u16>().ok()?.saturating_sub(1);
    let row = split.next()?.parse::<u16>().ok()?.saturating_sub(1);
    if release && let MouseEventKind::Down(btn) = kind {
        kind = MouseEventKind::Up(btn);
    }
    Some(Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn drain_all(bytes: &[u8]) -> Vec<TermInput> {
        let mut p = TermParser::default();
        p.feed(bytes);
        p.flush_timeout();
        let mut out = Vec::new();
        while let Some(item) = p.next_output() {
            out.push(item);
        }
        out
    }

    #[test]
    fn parses_live_color_scheme_notifications_without_wedging_subsequent_keys() {
        let items = drain_all(b"\x1b[?997;2nab\x1b[?997;1n\x1b[?999;4z\r");
        assert_eq!(
            items,
            vec![
                TermInput::ColorScheme(ColorScheme::Light),
                key_input(KeyCode::Char('a'), KeyModifiers::NONE),
                key_input(KeyCode::Char('b'), KeyModifiers::NONE),
                TermInput::ColorScheme(ColorScheme::Dark),
                key_input(KeyCode::Enter, KeyModifiers::NONE),
            ]
        );
    }

    #[test]
    fn parses_split_color_scheme_and_osc11_reports() {
        let mut p = TermParser::default();
        p.feed(b"\x1b[?99");
        assert!(p.next_output().is_none());
        p.feed(b"7;2n\x1b]11;rgb:1010/1010/1818\x07x");
        let mut out = Vec::new();
        while let Some(item) = p.next_output() {
            out.push(item);
        }
        assert_eq!(
            out,
            vec![
                TermInput::ColorScheme(ColorScheme::Light),
                TermInput::ColorScheme(ColorScheme::Dark),
                key_input(KeyCode::Char('x'), KeyModifiers::NONE),
            ]
        );
    }

    #[test]
    fn parses_keys_kitty_csi_u_mouse_and_paste() {
        let items = drain_all(
            b"\x01\x1b[1;5C\x1b[13~\x1b[97;5u\x1b[<0;12;6M\x1b[<0;12;6m\x1b[200~hi\x1b[2A\x1b[201~",
        );
        assert_eq!(
            items,
            vec![
                key_input(KeyCode::Char('a'), KeyModifiers::CONTROL),
                key_input(KeyCode::Right, KeyModifiers::CONTROL),
                key_input(KeyCode::F(3), KeyModifiers::NONE),
                key_input(KeyCode::Char('a'), KeyModifiers::CONTROL),
                TermInput::Event(Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 11,
                    row: 5,
                    modifiers: KeyModifiers::NONE,
                })),
                TermInput::Event(Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    column: 11,
                    row: 5,
                    modifiers: KeyModifiers::NONE,
                })),
                TermInput::Event(Event::Paste("hi\x1b[2A".to_owned())),
            ]
        );
    }

    #[test]
    fn lone_escape_and_alt_brackets_resolve_on_timeout() {
        assert_eq!(
            drain_all(b"\x1b"),
            vec![key_input(KeyCode::Esc, KeyModifiers::NONE)]
        );
        assert_eq!(
            drain_all(b"\x1b["),
            vec![key_input(KeyCode::Char('['), KeyModifiers::ALT)]
        );
        assert_eq!(
            drain_all(b"\x1b]"),
            vec![key_input(KeyCode::Char(']'), KeyModifiers::ALT)]
        );
        assert_eq!(
            drain_all("ä\x1bä".as_bytes()),
            vec![
                key_input(KeyCode::Char('ä'), KeyModifiers::NONE),
                key_input(KeyCode::Char('ä'), KeyModifiers::ALT),
            ]
        );
    }

    #[test]
    fn ignores_unknown_sequences_and_cursor_position_reports() {
        let items = drain_all(b"\x1b[20;10R\x1b[?62;4c\x1b]0;title\x07\x1b_Gi=31;OK\x1b\\z");
        assert_eq!(
            items,
            vec![key_input(KeyCode::Char('z'), KeyModifiers::NONE)]
        );
    }

    #[tokio::test]
    async fn pty_live_color_scheme_stream_receives_changes_and_keys() {
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        // SAFETY: valid pointers for a pty pair.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // Configure raw mode on slave just as `TerminalGuard::enter` does.
        // SAFETY: `slave` is an open pty descriptor.
        unsafe {
            let mut tio: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(slave, &mut tio), 0);
            libc::cfmakeraw(&mut tio);
            assert_eq!(libc::tcsetattr(slave, libc::TCSANOW, &tio), 0);
        }
        // SAFETY: `slave` is newly opened and owned here.
        let slave_owned = unsafe { OwnedFd::from_raw_fd(slave) };
        let mut stream = TermEvents::from_fd(slave_owned).unwrap();

        let payload = b"\x1b[?997;2nq\x1b[?997;1n";
        // SAFETY: `master` is open and `payload` is valid.
        let n = unsafe { libc::write(master, payload.as_ptr().cast(), payload.len()) };
        assert_eq!(n, payload.len() as isize);

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await
                .unwrap(),
            Some(TermInput::ColorScheme(ColorScheme::Light))
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await
                .unwrap(),
            Some(key_input(KeyCode::Char('q'), KeyModifiers::NONE))
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await
                .unwrap(),
            Some(TermInput::ColorScheme(ColorScheme::Dark))
        );

        drop(stream);
        // SAFETY: `master` was opened by `openpty`.
        unsafe { libc::close(master) };
    }
}
