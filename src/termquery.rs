//! Asking the terminal what it can draw. The question is written to the terminal and
//! the answer read back from its input, which is also where the keyboard arrives. So
//! the reading is done right here with a deadline, on the calling thread: when the time
//! is up nothing is left behind that could take input away from the event loop.
//!
//! Must run in raw mode, before mouse and focus reporting are on (their reports would
//! arrive in between) and before anything else reads the terminal's input.

use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use crate::theme::ColorScheme;

/// Five questions in one write:
/// - the kitty graphics protocol, by asking it to check a one-pixel picture (`i=31`);
/// - the same with a compressed pixel (`i=32`), for compressed transmissions;
/// - the size of a cell in pixels (`CSI 16 t`);
/// - the desktop colour-scheme report (`CSI ? 996 n`);
/// - the terminal's background colour (`OSC 11`).
///
/// The device attributes request (`CSI c`) comes last because every terminal answers
/// it: its reply means the others have been answered or ignored.
const QUERY: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\
\x1b_Gi=32,s=1,v=1,a=q,t=d,f=24,o=z;eJxjYGAAAAADAAE=\x1b\\\
\x1b[16t\x1b[?996n\x1b]11;?\x1b\\\x1b[c";

/// Same graphics + cell probe without appearance queries, when the desktop portal
/// already supplied an initial dark/light preference.
const GRAPHICS_QUERY: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\
\x1b_Gi=32,s=1,v=1,a=q,t=d,f=24,o=z;eJxjYGAAAAADAAE=\x1b\\\
\x1b[16t\x1b[c";

/// Appearance-only probe when graphics detection is skipped (disabled, half blocks, or
/// inside a multiplexer where kitty APC probes are not used).
const APPEARANCE_QUERY: &[u8] = b"\x1b[?996n\x1b]11;?\x1b\\\x1b[c";

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
    /// Colour scheme reported by `CSI ? 997 ; 1 n` (dark) or `2 n` (light).
    pub reported_scheme: Option<ColorScheme>,
    /// Colour scheme inferred from `OSC 11` background luminance.
    pub background_scheme: Option<ColorScheme>,
    /// The terminal answered the last question, so nothing more is coming.
    pub complete: bool,
}

impl Reply {
    /// Explicit colour-scheme report wins over perceived background luminance.
    pub fn scheme(&self) -> Option<ColorScheme> {
        self.reported_scheme.or(self.background_scheme)
    }
}

/// Parses the parameter portion (without trailing `n`) of a private CSI `n` report,
/// matching `CSI ? 997 ; 1 n` (dark) and `CSI ? 997 ; 2 n` (light).
pub fn parse_color_scheme_csi(params: &str) -> Option<ColorScheme> {
    let mode = params.strip_prefix("?997;")?.split(';').next()?;
    match mode {
        "1" => Some(ColorScheme::Dark),
        "2" => Some(ColorScheme::Light),
        _ => None,
    }
}

/// Parses an OSC payload (between `ESC ]` and BEL/ST) for `11;<color>` background
/// luminance, returning [`ColorScheme::Dark`] or [`ColorScheme::Light`].
pub fn parse_osc11_payload(payload: &[u8]) -> Option<ColorScheme> {
    let text = std::str::from_utf8(payload).ok()?;
    let color = text.strip_prefix("11;")?.trim();
    let (r, g, b) = parse_osc_rgb(color)?;
    Some(scheme_from_rgb(r, g, b))
}

fn parse_osc_rgb(color: &str) -> Option<(f64, f64, f64)> {
    let lower = color.to_ascii_lowercase();
    if let Some(rest) = lower
        .strip_prefix("rgb:")
        .or_else(|| lower.strip_prefix("rgba:"))
    {
        let mut parts = rest.split('/');
        let r = parse_hex_channel(parts.next()?)?;
        let g = parse_hex_channel(parts.next()?)?;
        let b = parse_hex_channel(parts.next()?)?;
        return Some((r, g, b));
    }
    if let Some(hex) = lower.strip_prefix('#')
        && hex.len() == 6
    {
        let r = parse_hex_channel(&hex[0..2])?;
        let g = parse_hex_channel(&hex[2..4])?;
        let b = parse_hex_channel(&hex[4..6])?;
        return Some((r, g, b));
    }
    None
}

fn parse_hex_channel(hex: &str) -> Option<f64> {
    if !(1..=4).contains(&hex.len()) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let val = u32::from_str_radix(hex, 16).ok()? as f64;
    let max = ((1u32 << (4 * hex.len())) - 1) as f64;
    Some(val / max)
}

fn scheme_from_rgb(r: f64, g: f64, b: f64) -> ColorScheme {
    let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    if luma >= 0.5 {
        ColorScheme::Light
    } else {
        ColorScheme::Dark
    }
}

/// Parses a `COLORFGBG` environment value (`fg;bg` or `fg;misc;bg`) into a colour scheme.
pub fn scheme_from_colorfgbg(value: &str) -> Option<ColorScheme> {
    let bg = value.rsplit(';').next()?.trim().parse::<u8>().ok()?;
    match bg {
        0..=6 | 8 => Some(ColorScheme::Dark),
        7 | 9..=15 => Some(ColorScheme::Light),
        16..=231 => {
            const CUBE: [f64; 6] = [
                0.0,
                95.0 / 255.0,
                135.0 / 255.0,
                175.0 / 255.0,
                215.0 / 255.0,
                1.0,
            ];
            let idx = (bg - 16) as usize;
            let r = CUBE[idx / 36];
            let g = CUBE[(idx / 6) % 6];
            let b = CUBE[idx % 6];
            Some(scheme_from_rgb(r, g, b))
        }
        232..=255 => {
            let level = (8.0 + 10.0 * f64::from(bg - 232)) / 255.0;
            Some(scheme_from_rgb(level, level, level))
        }
    }
}

/// Fallback colour-scheme detection from environment variables (`COLORFGBG`).
pub fn env_color_scheme() -> Option<ColorScheme> {
    let val = std::env::var("COLORFGBG").ok()?;
    scheme_from_colorfgbg(&val)
}

/// Resolves the initial colour scheme from a terminal reply, falling back to `COLORFGBG`
/// and finally [`ColorScheme::Dark`].
pub fn resolve_color_scheme(reply: Option<Reply>) -> ColorScheme {
    reply
        .and_then(|r| r.scheme())
        .or_else(env_color_scheme)
        .unwrap_or(ColorScheme::Dark)
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
            // Colour scheme notification: CSI ? 997 ; 1/2 n
            Some('n') if parameters.starts_with('?') => {
                if let Some(scheme) = parse_color_scheme_csi(parameters) {
                    reply.reported_scheme = Some(scheme);
                }
            }
            // Device attributes: CSI ? ... c
            Some('c') if parameters.starts_with('?') => reply.complete = true,
            _ => {}
        }
    }
    for (at, _) in text.match_indices("\x1b]") {
        let body = &text[at + 2..];
        let st = body.find("\x1b\\");
        let bel = body.find('\x07');
        let end = match (st, bel) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if let Some(end) = end
            && let Some(scheme) = parse_osc11_payload(&body.as_bytes()[..end])
        {
            reply.background_scheme = Some(scheme);
        }
    }
    reply
}

/// Asks the terminal on stdin/stdout. `None` if they are not a terminal.
pub fn ask(timeout: Duration) -> Option<Reply> {
    ask_bytes(QUERY, timeout)
}

/// Asks only about kitty graphics and cell size (used when the portal already
/// supplied the initial dark/light scheme). `None` if not a terminal.
pub fn ask_graphics(timeout: Duration) -> Option<Reply> {
    ask_bytes(GRAPHICS_QUERY, timeout)
}

/// Asks only for colour scheme / background (`CSI ? 996 n` + `OSC 11`). `None` if not a tty.
pub fn ask_appearance(timeout: Duration) -> Option<Reply> {
    ask_bytes(APPEARANCE_QUERY, timeout)
}

fn ask_bytes(request: &[u8], timeout: Duration) -> Option<Reply> {
    // SAFETY: isatty only inspects the descriptor.
    let terminal =
        unsafe { libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDOUT_FILENO) == 1 };
    terminal.then(|| query_bytes(libc::STDIN_FILENO, libc::STDOUT_FILENO, request, timeout))
}

/// Writes the full question to `output` and reads the answer from `input` until it is
/// complete or `timeout` has passed. Blocks for at most that long.
#[cfg(test)]
pub fn query(input: RawFd, output: RawFd, timeout: Duration) -> Reply {
    query_bytes(input, output, QUERY, timeout)
}

/// Writes only the appearance question (`CSI ?996n` + `OSC 11` + `CSI c`) and reads the answer.
#[cfg(test)]
pub fn query_appearance(input: RawFd, output: RawFd, timeout: Duration) -> Reply {
    query_bytes(input, output, APPEARANCE_QUERY, timeout)
}

fn query_bytes(input: RawFd, output: RawFd, request: &[u8], timeout: Duration) -> Reply {
    let mut written = 0;
    while written < request.len() {
        let rest = &request[written..];
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
                complete: true,
                ..Reply::default()
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
                complete: true,
                ..Reply::default()
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
                reported_scheme: None,
                background_scheme: None,
                complete: true,
            }
        );
        let theirs = terminal.join().unwrap();
        // SAFETY: both were opened by `pty`.
        unsafe {
            libc::close(ours);
            libc::close(theirs);
        }
    }

    #[test]
    fn csi_996_and_osc_11_pty_replies_resolve_scheme() {
        // Explicit CSI ?997;2n (light) wins even when OSC 11 arrived too, and finishes
        // as soon as DA1 arrives.
        let (ours, theirs) = pty();
        let terminal = std::thread::spawn(move || {
            let mut asked = Vec::new();
            while !asked.ends_with(b"\x1b[c") {
                let more = read_some(theirs, Duration::from_secs(5));
                assert!(!more.is_empty(), "no question arrived");
                asked.extend(more);
            }
            assert!(
                asked
                    .windows(b"\x1b[?996n".len())
                    .any(|w| w == b"\x1b[?996n")
            );
            assert!(
                asked
                    .windows(b"\x1b]11;?\x1b\\".len())
                    .any(|w| w == b"\x1b]11;?\x1b\\")
            );
            write_all(
                theirs,
                b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\\x1b[?997;2n\x1b[?62;c",
            );
            theirs
        });
        let reply = query_appearance(ours, ours, Duration::from_secs(5));
        assert!(reply.complete);
        assert_eq!(reply.reported_scheme, Some(ColorScheme::Light));
        assert_eq!(reply.background_scheme, Some(ColorScheme::Dark));
        assert_eq!(reply.scheme(), Some(ColorScheme::Light));
        let theirs = terminal.join().unwrap();
        // SAFETY: both were opened by `pty`.
        unsafe {
            libc::close(ours);
            libc::close(theirs);
        }
    }

    #[test]
    fn osc_11_alone_detects_dark_and_light_backgrounds() {
        let light = parse(b"\x1b]11;rgb:fbfb/f1f1/c7c7\x07\x1b[?62;c");
        assert!(light.complete);
        assert_eq!(light.reported_scheme, None);
        assert_eq!(light.background_scheme, Some(ColorScheme::Light));
        assert_eq!(light.scheme(), Some(ColorScheme::Light));

        let dark = parse(b"\x1b]11;rgb:2828/2828/2828\x1b\\\x1b[?62;c");
        assert!(dark.complete);
        assert_eq!(dark.scheme(), Some(ColorScheme::Dark));

        // 1-digit, 2-digit, and #rrggbb forms also parse.
        assert_eq!(
            parse_osc11_payload(b"11;rgb:f/f/f"),
            Some(ColorScheme::Light)
        );
        assert_eq!(
            parse_osc11_payload(b"11;rgb:10/10/18"),
            Some(ColorScheme::Dark)
        );
        assert_eq!(parse_osc11_payload(b"11;#eff1f5"), Some(ColorScheme::Light));
        assert_eq!(
            parse_osc11_payload(b"11;rgba:1e/1e/2e/ff"),
            Some(ColorScheme::Dark)
        );
        assert_eq!(parse_osc11_payload(b"11;garbage"), None);
    }

    #[test]
    fn colorfgbg_fallback_maps_background_indices() {
        assert_eq!(scheme_from_colorfgbg("15;0"), Some(ColorScheme::Dark));
        assert_eq!(
            scheme_from_colorfgbg("15;default;8"),
            Some(ColorScheme::Dark)
        );
        assert_eq!(scheme_from_colorfgbg("0;15"), Some(ColorScheme::Light));
        assert_eq!(scheme_from_colorfgbg("0;7"), Some(ColorScheme::Light));
        assert_eq!(
            scheme_from_colorfgbg("0;default;231"),
            Some(ColorScheme::Light)
        );
        assert_eq!(scheme_from_colorfgbg("15;232"), Some(ColorScheme::Dark));
        assert_eq!(scheme_from_colorfgbg("15;254"), Some(ColorScheme::Light));
        assert_eq!(scheme_from_colorfgbg("default;default"), None);
        assert_eq!(scheme_from_colorfgbg(""), None);

        // Unanswered terminal falls back to Dark when no COLORFGBG matches.
        assert_eq!(resolve_color_scheme(None), ColorScheme::Dark);
    }
}
