//! Asking the terminal what it can draw. The question is written to the terminal and
//! the answer read back from its input, which is also where the keyboard arrives. So
//! the reading is done right here with a deadline, on the calling thread: when the time
//! is up nothing is left behind that could take input away from the event loop.
//!
//! Must run in raw mode, before mouse and focus reporting are on (their reports would
//! arrive in between) and before anything else reads the terminal's input.

use std::os::fd::RawFd;
use std::time::{Duration, Instant};

/// Three questions in one write:
/// - the kitty graphics protocol, by asking it to check a one-pixel picture (`i=31`);
/// - the same with a compressed pixel (`i=32`), for compressed transmissions;
/// - the size of a cell in pixels (`CSI 16 t`).
///
/// The device attributes request (`CSI c`) comes last because every terminal answers
/// it: its reply means the others have been answered or ignored.
const QUERY: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\
\x1b_Gi=32,s=1,v=1,a=q,t=d,f=24,o=z;eJxjYGAAAAADAAE=\x1b\\\
\x1b[16t\x1b[c";

/// More than any terminal sends in reply; reading stops there.
const MAX_REPLY: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reply {
    /// The terminal speaks the kitty graphics protocol.
    pub kitty: bool,
    /// It also accepts compressed transmissions.
    pub compression: bool,
    /// Width and height of a cell in pixels.
    pub cell: Option<(u16, u16)>,
    /// The terminal answered the last question, so nothing more is coming.
    pub complete: bool,
}

/// What the bytes read so far say. Anything else in between (a key pressed during the
/// wait, for one) is skipped.
pub fn parse(bytes: &[u8]) -> Reply {
    let text = String::from_utf8_lossy(bytes);
    let mut reply = Reply {
        kitty: text.contains("\x1b_Gi=31;OK"),
        compression: text.contains("\x1b_Gi=32;OK"),
        ..Reply::default()
    };
    for (at, _) in text.match_indices("\x1b[") {
        let body = &text[at + 2..];
        let end = body
            .find(|c: char| !(c.is_ascii_digit() || c == ';' || c == '?'))
            .unwrap_or(body.len());
        let (parameters, rest) = body.split_at(end);
        match rest.chars().next() {
            // Cell size: CSI 6 ; height ; width t
            Some('t') => {
                let mut numbers = parameters.split(';').map(str::parse::<u16>);
                if let (Some(Ok(6)), Some(Ok(height)), Some(Ok(width))) =
                    (numbers.next(), numbers.next(), numbers.next())
                    && width > 0
                    && height > 0
                {
                    reply.cell = Some((width, height));
                }
            }
            // Device attributes: CSI ? ... c
            Some('c') if parameters.starts_with('?') => reply.complete = true,
            _ => {}
        }
    }
    reply
}

/// Asks the terminal on stdin/stdout. `None` if they are not a terminal.
pub fn ask(timeout: Duration) -> Option<Reply> {
    // SAFETY: isatty only inspects the descriptor.
    let terminal =
        unsafe { libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDOUT_FILENO) == 1 };
    terminal.then(|| query(libc::STDIN_FILENO, libc::STDOUT_FILENO, timeout))
}

/// Writes the question to `output` and reads the answer from `input` until it is
/// complete or `timeout` has passed. Blocks for at most that long.
pub fn query(input: RawFd, output: RawFd, timeout: Duration) -> Reply {
    let mut written = 0;
    while written < QUERY.len() {
        let rest = &QUERY[written..];
        // SAFETY: the pointer and length describe `rest`.
        let n = unsafe { libc::write(output, rest.as_ptr().cast(), rest.len()) };
        if n > 0 {
            written += n as usize;
        } else if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
        {
            continue;
        } else {
            return Reply::default();
        }
    }

    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut reply = Reply::default();
    while !reply.complete && bytes.len() < MAX_REPLY {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut fd = libc::pollfd {
            fd: input,
            events: libc::POLLIN,
            revents: 0,
        };
        let wait = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        // SAFETY: one valid pollfd is passed with a count of one.
        let ready = unsafe { libc::poll(&mut fd, 1, wait) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if ready == 0 {
            continue;
        }
        let mut chunk = [0u8; 256];
        // Readable now, so this does not block.
        // SAFETY: the pointer and length describe `chunk`.
        let n = unsafe { libc::read(input, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n as usize]);
        reply = parse(&bytes);
    }
    reply
}

/// The cell size worked out from the window size in pixels, where the system knows it.
pub fn cell_from_window() -> Option<(u16, u16)> {
    // SAFETY: winsize is plain data and the ioctl fills it in.
    let size = unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        (libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) == 0).then_some(size)
    }?;
    if size.ws_col == 0 || size.ws_row == 0 {
        return None;
    }
    let cell = (size.ws_xpixel / size.ws_col, size.ws_ypixel / size.ws_row);
    (cell.0 > 0 && cell.1 > 0).then_some(cell)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_recognised_among_other_input() {
        assert_eq!(parse(b""), Reply::default());
        // kitty: both graphics checks pass, cell size, device attributes.
        let reply = parse(b"\x1b_Gi=31;OK\x1b\\\x1b_Gi=32;OK\x1b\\\x1b[6;20;10t\x1b[?62;c");
        assert_eq!(
            reply,
            Reply {
                kitty: true,
                compression: true,
                cell: Some((10, 20)),
                complete: true
            }
        );
        // A terminal without graphics, and a key typed while waiting.
        let reply = parse(b"x\x1b[6;18;9t\x1b[A\x1b[?1;2c");
        assert_eq!(
            reply,
            Reply {
                kitty: false,
                compression: false,
                cell: Some((9, 18)),
                complete: true
            }
        );
        // An error reply is not support; half an answer is not complete.
        let reply = parse(b"\x1b_Gi=31;ENOTSUPPORTED\x1b\\\x1b[6;20");
        assert_eq!(reply, Reply::default());
        // Other reports are not mistaken for ours.
        assert_eq!(
            parse(b"\x1b[?1u\x1b[4;600;800t\x1b[6;0;0t\x1b[I"),
            Reply::default()
        );
    }

    /// A pseudo terminal: (our side, the terminal's side).
    fn pty() -> (RawFd, RawFd) {
        let (mut ours, mut theirs) = (0, 0);
        // SAFETY: openpty fills in the two descriptors; the termios calls get a
        // descriptor that was just opened and a struct they initialise.
        unsafe {
            let (name, modes, size) = (std::ptr::null_mut(), std::ptr::null(), std::ptr::null());
            assert_eq!(libc::openpty(&mut theirs, &mut ours, name, modes, size), 0);
            let mut modes: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(ours, &mut modes), 0);
            libc::cfmakeraw(&mut modes);
            assert_eq!(libc::tcsetattr(ours, libc::TCSANOW, &modes), 0);
        }
        (ours, theirs)
    }

    fn read_some(fd: RawFd, wait: Duration) -> Vec<u8> {
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let mut chunk = [0u8; 1024];
        // SAFETY: valid pollfd; the buffer is ours.
        unsafe {
            if libc::poll(&mut pollfd, 1, wait.as_millis() as i32) <= 0 {
                return Vec::new();
            }
            let n = libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len());
            chunk[..n.max(0) as usize].to_vec()
        }
    }

    fn write_all(fd: RawFd, bytes: &[u8]) {
        // SAFETY: the pointer and length describe `bytes`.
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        assert_eq!(n, bytes.len() as isize);
    }

    #[test]
    fn a_silent_terminal_costs_the_timeout_and_no_keypress() {
        let (ours, theirs) = pty();
        let started = Instant::now();
        let reply = query(ours, ours, Duration::from_millis(300));
        let took = started.elapsed();
        assert_eq!(reply, Reply::default());
        assert!(
            took >= Duration::from_millis(300) && took < Duration::from_secs(2),
            "{took:?}"
        );
        // The terminal did get the question.
        let asked = read_some(theirs, Duration::from_millis(500));
        assert!(asked.starts_with(b"\x1b_Gi=31,"));
        assert!(asked.ends_with(b"\x1b[c"));

        // What is typed afterwards reaches whoever reads next: nothing of the query is
        // still waiting on the input.
        std::thread::sleep(Duration::from_millis(100));
        write_all(theirs, b"hello");
        assert_eq!(read_some(ours, Duration::from_millis(500)), b"hello");
        // SAFETY: both were opened by `pty`.
        unsafe {
            libc::close(ours);
            libc::close(theirs);
        }
    }

    #[test]
    fn an_answering_terminal_is_understood_without_waiting() {
        let (ours, theirs) = pty();
        let terminal = std::thread::spawn(move || {
            let mut asked = Vec::new();
            while !asked.ends_with(b"\x1b[c") {
                let more = read_some(theirs, Duration::from_secs(5));
                assert!(!more.is_empty(), "no question arrived");
                asked.extend(more);
            }
            // In pieces, as it may arrive over a slow line.
            write_all(theirs, b"\x1b_Gi=31;OK\x1b\\\x1b_Gi=32;ENOTSUPP");
            std::thread::sleep(Duration::from_millis(50));
            write_all(theirs, b"ORTED\x1b\\\x1b[6;34;17t\x1b[?62;4c");
            theirs
        });
        let started = Instant::now();
        let reply = query(ours, ours, Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(
            reply,
            Reply {
                kitty: true,
                compression: false,
                cell: Some((17, 34)),
                complete: true
            }
        );
        let theirs = terminal.join().unwrap();
        // SAFETY: both were opened by `pty`.
        unsafe {
            libc::close(ours);
            libc::close(theirs);
        }
    }
}
