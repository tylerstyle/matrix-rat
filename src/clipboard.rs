//! Pasting a picture from the desktop's clipboard (`Action::PastePicture`).
//!
//! A terminal pastes only text, so the picture is asked of the desktop's own tool, run
//! without a shell: `wl-paste` on Wayland, `xclip` on X11. What it writes is untrusted:
//! it is read with a size and a time limit (the tool is killed past either), decoded with
//! the same guards as the pictures in the timeline to be sure it is a picture of the kind
//! that was asked for, and only then written to a private temporary file, which the app
//! attaches like a file picked by hand (`app::clipboard`).
//!
//! The pure parts live here, with the running of the tool behind a function (see
//! [`run_tool`]) so that they can be tried without a clipboard. Nothing the clipboard
//! holds, nothing a tool writes and no file name is ever logged or shown in `Debug`
//! output.

use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use image::ImageFormat;

use crate::images::human_size;
use crate::upload::FALLBACK_LIMIT;

/// How long the whole reading may take, both runs of the tool together. A clipboard
/// answers at once; one that does not belongs to a program that hangs.
const TIME_LIMIT: Duration = Duration::from_secs(5);
/// The list of what the clipboard offers is a few lines; more is not such a list.
const MAX_LIST: u64 = 64 * 1024;

/// The desktop's clipboard tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// `wl-paste`, from wl-clipboard, for Wayland.
    WlPaste,
    /// `xclip`, for X11 (and programs under XWayland).
    Xclip,
}

impl Tool {
    /// The command that lists the kinds of content on offer, one per line.
    fn list(self) -> &'static [&'static str] {
        match self {
            Tool::WlPaste => &["wl-paste", "--list-types"],
            Tool::Xclip => &["xclip", "-selection", "clipboard", "-t", "TARGETS", "-o"],
        }
    }

    /// The command that writes the content of one kind. `wl-paste` adds a line break to
    /// what it takes for text unless told not to.
    fn read(self, kind: Kind) -> Vec<&'static str> {
        match self {
            Tool::WlPaste => vec!["wl-paste", "--no-newline", "--type", kind.mime()],
            Tool::Xclip => vec!["xclip", "-selection", "clipboard", "-t", kind.mime(), "-o"],
        }
    }

    /// What to install to have it.
    fn package(self) -> &'static str {
        match self {
            Tool::WlPaste => "wl-clipboard",
            Tool::Xclip => "xclip",
        }
    }
}

/// The tools to try, in order, from what the environment says about the desktop.
/// Wayland's first: under XWayland an X11 tool may not see what Wayland programs copied.
pub fn tools(wayland: Option<&OsStr>, x11: Option<&OsStr>) -> Vec<Tool> {
    let set = |value: Option<&OsStr>| value.is_some_and(|value| !value.is_empty());
    let mut tools = Vec::new();
    if set(wayland) {
        tools.push(Tool::WlPaste);
    }
    if set(x11) {
        tools.push(Tool::Xclip);
    }
    tools
}

/// [`tools`] for the process's own environment.
pub fn tools_from_env() -> Vec<Tool> {
    tools(
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        std::env::var_os("DISPLAY").as_deref(),
    )
}

/// The kinds of picture that are asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Png,
    Jpeg,
    Webp,
    Gif,
}

impl Kind {
    /// In order of preference: a PNG is every pixel as it was copied.
    const PREFERRED: [Kind; 4] = [Kind::Png, Kind::Jpeg, Kind::Webp, Kind::Gif];

    fn mime(self) -> &'static str {
        match self {
            Kind::Png => "image/png",
            Kind::Jpeg => "image/jpeg",
            Kind::Webp => "image/webp",
            Kind::Gif => "image/gif",
        }
    }

    fn format(self) -> ImageFormat {
        match self {
            Kind::Png => ImageFormat::Png,
            Kind::Jpeg => ImageFormat::Jpeg,
            Kind::Webp => ImageFormat::WebP,
            Kind::Gif => ImageFormat::Gif,
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Kind::Png => "png",
            Kind::Jpeg => "jpg",
            Kind::Webp => "webp",
            Kind::Gif => "gif",
        }
    }
}

/// The kind of picture to ask for, from a tool's list of what is on offer: only whole
/// lines that name one of ours count.
fn choose(list: &[u8]) -> Option<Kind> {
    let offered: Vec<&[u8]> = list
        .split(|byte| *byte == b'\n')
        .map(<[u8]>::trim_ascii)
        .collect();
    Kind::PREFERRED.into_iter().find(|kind| {
        offered
            .iter()
            .any(|line| line.eq_ignore_ascii_case(kind.mime().as_bytes()))
    })
}

/// Why nothing was attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Neither Wayland nor X11: a console, or SSH without a forwarded display.
    NoDesktop,
    /// The tool is not installed (the first one that would have been used).
    Missing(Tool),
    NoPicture,
    /// More than files may be; the server's limit, if it is known.
    TooLarge(Option<u64>),
    TimedOut,
    /// The tool could not be started or ended with an error.
    Failed,
    /// What came is not a picture of the kind asked for, or a damaged one, or one too
    /// large to decode.
    NotAPicture,
    TempFile,
}

impl Failure {
    /// For a toast.
    pub fn message(self) -> String {
        match self {
            Failure::NoDesktop => {
                "There is no desktop clipboard here to paste a picture from".to_owned()
            }
            Failure::Missing(tool) => format!("Install {} to paste pictures", tool.package()),
            Failure::NoPicture => "No picture in the clipboard".to_owned(),
            Failure::TooLarge(Some(limit)) => format!(
                "The picture in the clipboard is too large: the server takes up to {}",
                human_size(limit)
            ),
            Failure::TooLarge(None) => format!(
                "The picture in the clipboard is too large: up to {} is tried, and the \
                 server's limit is not known",
                human_size(FALLBACK_LIMIT)
            ),
            Failure::TimedOut => "The clipboard did not answer in time".to_owned(),
            Failure::Failed => "Could not read the clipboard".to_owned(),
            Failure::NotAPicture => "What the clipboard holds is not a picture".to_owned(),
            Failure::TempFile => "Could not make a temporary file for the picture".to_owned(),
        }
    }
}

/// A reading of the clipboard, for the run loop to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Tells this reading's answer from that of an earlier one.
    pub id: u64,
    /// The tools to try, in order (see [`tools`]); never empty.
    pub tools: Vec<Tool>,
    /// The largest file the server accepts, if it is known.
    pub limit: Option<u64>,
}

/// What a reading came to, for the app.
#[derive(Debug)]
pub struct Done {
    pub id: u64,
    pub result: Result<TempPicture, Failure>,
}

/// What became of running a tool.
#[derive(Clone, PartialEq, Eq)]
pub enum Ran {
    /// It ended well, having written this (no more than it was allowed).
    Output(Vec<u8>),
    /// It is not installed.
    Missing,
    /// It could not be started, or it ended with an error.
    Failed,
    /// It wrote more than it was allowed, and was killed.
    TooLarge,
    /// It did not end in time, and was killed.
    TimedOut,
}

impl fmt::Debug for Ran {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ran::Output(bytes) => write!(f, "Output({} bytes)", bytes.len()),
            Ran::Missing => f.write_str("Missing"),
            Ran::Failed => f.write_str("Failed"),
            Ran::TooLarge => f.write_str("TooLarge"),
            Ran::TimedOut => f.write_str("TimedOut"),
        }
    }
}

/// A pasted picture in a file of its own: owner-only, in the app's runtime folder, and
/// gone when this is dropped.
pub struct TempPicture {
    path: PathBuf,
}

impl TempPicture {
    /// Writes `bytes` to a new file in `dir` (made if need be, owner-only), named after
    /// the time, such as `pasted-20261115-143012.png`: the name is what the room sees.
    fn create(dir: &Path, kind: Kind, bytes: &[u8]) -> io::Result<Self> {
        if !dir.is_dir() {
            fs::create_dir_all(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        // In UTC: a local time would tell the room the sender's time zone.
        let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let mut last = io::Error::from(io::ErrorKind::AlreadyExists);
        for n in 1..=8 {
            let name = match n {
                1 => format!("pasted-{stamp}.{}", kind.extension()),
                n => format!("pasted-{stamp}-{n}.{}", kind.extension()),
            };
            let path = dir.join(name);
            // `create_new` never follows a link someone else put there.
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(mut file) => {
                    // Removed by `Drop` also if writing fails.
                    let picture = Self { path };
                    file.write_all(bytes)?;
                    return Ok(picture);
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => last = err,
                Err(err) => return Err(err),
            }
        }
        Err(last)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempPicture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl fmt::Debug for TempPicture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TempPicture")
    }
}

/// Runs `argv` (no shell) until it ends, it has written more than `max` bytes or
/// `deadline` has passed, whichever is first; in the last two cases it is killed.
/// Nothing is passed to it, and what it says about errors goes nowhere.
#[expect(
    clippy::disallowed_methods,
    reason = "the tool was killed the line before, so reaping it does not wait; and this runs inside spawn_blocking"
)]
pub fn run_tool(argv: &[&str], max: u64, deadline: Instant) -> Ran {
    let Some((program, args)) = argv.split_first() else {
        return Ran::Failed;
    };
    let spawned = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ran::Missing,
        Err(_) => return Ran::Failed,
    };
    let ran = collect(&mut child, max, deadline);
    // Whatever happened, the tool does not outlive this: killed if it still runs (a
    // tool that has ended is not signalled again), and reaped.
    let _ = child.kill();
    let _ = child.wait();
    ran
}

/// Reads what the tool writes and waits for its end, within the limits.
#[expect(
    clippy::disallowed_methods,
    reason = "short naps up to the caller's deadline, never longer"
)]
fn collect(child: &mut Child, max: u64, deadline: Instant) -> Ran {
    let Some(mut out) = child.stdout.take() else {
        return Ran::Failed;
    };
    let mut bytes = Vec::new();
    let mut chunk = vec![0; 64 * 1024];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ran::TimedOut;
        }
        let mut ready = libc::pollfd {
            fd: out.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let wait = left.as_millis().clamp(1, i32::MAX as u128) as i32;
        // SAFETY: one valid pollfd is passed with a count of one.
        match unsafe { libc::poll(&mut ready, 1, wait) } {
            0 => continue,
            n if n < 0 => {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Ran::Failed;
            }
            _ => {}
        }
        // Readable, or closed: either way this does not block.
        match out.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if (bytes.len() + n) as u64 > max {
                    return Ran::TooLarge;
                }
                bytes.extend_from_slice(&chunk[..n]);
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Ran::Failed,
        }
    }
    // All of it was read; the tool has to end too, in the time that is left.
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ran::Output(bytes),
            Ok(Some(_)) | Err(_) => return Ran::Failed,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) => return Ran::TimedOut,
        }
    }
}

/// Reads a picture from the clipboard with the tools of `request`, run by `runner` (see
/// [`run_tool`]), and puts it into a file in `dir`. Slow (a tool, decoding): runs on a
/// blocking thread.
pub fn paste(
    request: &Request,
    dir: &Path,
    mut runner: impl FnMut(&[&str], u64, Instant) -> Ran,
) -> Result<TempPicture, Failure> {
    let deadline = Instant::now() + TIME_LIMIT;
    // The first tool that is installed is the one asked; a missing one is told about
    // only if no other is there.
    let mut missing = None;
    let mut listed = None;
    for &tool in &request.tools {
        match runner(tool.list(), MAX_LIST, deadline) {
            Ran::Missing => {
                missing.get_or_insert(tool);
            }
            ran => {
                listed = Some((tool, ran));
                break;
            }
        }
    }
    let Some((tool, list)) = listed else {
        return Err(missing.map_or(Failure::NoDesktop, Failure::Missing));
    };
    let kind = match list {
        Ran::Output(list) => choose(&list).ok_or(Failure::NoPicture)?,
        Ran::TimedOut => return Err(Failure::TimedOut),
        // The tools end with an error when nothing was copied; and no list of kinds is
        // that long.
        Ran::Failed | Ran::TooLarge | Ran::Missing => return Err(Failure::NoPicture),
    };
    let most = request.limit.unwrap_or(FALLBACK_LIMIT);
    let bytes = match runner(&tool.read(kind), most, deadline) {
        // A runner is not trusted to have kept to the limit either.
        Ran::Output(bytes) if bytes.len() as u64 > most => {
            return Err(Failure::TooLarge(request.limit));
        }
        Ran::Output(bytes) if bytes.is_empty() => return Err(Failure::NoPicture),
        Ran::Output(bytes) => bytes,
        Ran::TooLarge => return Err(Failure::TooLarge(request.limit)),
        Ran::TimedOut => return Err(Failure::TimedOut),
        Ran::Missing | Ran::Failed => return Err(Failure::Failed),
    };
    check(&bytes, kind)?;
    TempPicture::create(dir, kind, &bytes).map_err(|_| Failure::TempFile)
}

/// Whether `bytes` are a picture of `kind`: by its signature, then decoded whole with
/// the bounds the timeline's pictures have (a few bytes can claim gigabytes of pixels).
fn check(bytes: &[u8], kind: Kind) -> Result<(), Failure> {
    if image::guess_format(bytes).ok() != Some(kind.format()) {
        return Err(Failure::NotAPicture);
    }
    // Only whether it decodes matters: the smallest result is the cheapest.
    crate::images::decode(bytes, (1, 1))
        .map(drop)
        .map_err(|_| Failure::NotAPicture)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::upload::tests::Scratch;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        crate::images::demo_picture(width, height)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    fn request(tools: &[Tool], limit: Option<u64>) -> Request {
        Request {
            id: 1,
            tools: tools.to_vec(),
            limit,
        }
    }

    /// A clipboard that offers `list` and gives `content` for whatever is asked; what
    /// was run comes back in `ran`, with the bound it was given.
    struct Fake {
        list: Ran,
        content: Ran,
        ran: Vec<(Vec<String>, u64, Instant)>,
    }

    impl Fake {
        fn new(list: &str, content: Ran) -> Self {
            Self {
                list: Ran::Output(list.as_bytes().to_vec()),
                content,
                ran: Vec::new(),
            }
        }

        fn paste(&mut self, request: &Request, dir: &Path) -> Result<TempPicture, Failure> {
            paste(request, dir, |argv, max, deadline| {
                self.ran.push((
                    argv.iter().map(|word| (*word).to_owned()).collect(),
                    max,
                    deadline,
                ));
                if argv.contains(&"--list-types") || argv.contains(&"TARGETS") {
                    self.list.clone()
                } else {
                    self.content.clone()
                }
            })
        }

        fn commands(&self) -> Vec<String> {
            self.ran.iter().map(|(argv, _, _)| argv.join(" ")).collect()
        }
    }

    #[test]
    fn the_tool_is_chosen_from_the_desktop() {
        let on = Some(OsStr::new("wayland-1"));
        let x = Some(OsStr::new(":0"));
        let empty = Some(OsStr::new(""));
        assert_eq!(tools(on, x), [Tool::WlPaste, Tool::Xclip]);
        assert_eq!(tools(on, None), [Tool::WlPaste]);
        assert_eq!(tools(None, x), [Tool::Xclip]);
        assert_eq!(tools(empty, x), [Tool::Xclip]);
        assert!(tools(None, None).is_empty());
        assert!(tools(empty, empty).is_empty());
    }

    #[test]
    fn the_best_kind_of_picture_on_offer_is_asked_for() {
        assert_eq!(
            choose(b"text/plain\nimage/jpeg\nimage/png\n"),
            Some(Kind::Png)
        );
        assert_eq!(choose(b"image/gif\r\nimage/webp\r\n"), Some(Kind::Webp));
        assert_eq!(choose(b"TEXT/PLAIN\nIMAGE/JPEG"), Some(Kind::Jpeg));
        assert_eq!(choose(b"  image/gif  "), Some(Kind::Gif));
        // Only whole lines naming one of ours count.
        let lists: [&[u8]; 5] = [
            b"",
            b"text/plain\nUTF8_STRING\ntext/html",
            b"image/png-not\nx-image/png\nimage/pngimage/jpeg",
            b"image/bmp\nimage/tiff",
            b"\xff\xfe\x00image",
        ];
        for list in lists {
            assert_eq!(choose(list), None, "{list:?}");
        }
    }

    #[test]
    fn the_desktops_own_commands_are_run() {
        let dir = Scratch::new("clip-commands");
        let mut fake = Fake::new("image/png", Ran::Output(png(4, 3)));
        fake.paste(&request(&[Tool::WlPaste], None), &dir.0)
            .unwrap();
        assert_eq!(
            fake.commands(),
            [
                "wl-paste --list-types",
                "wl-paste --no-newline --type image/png"
            ]
        );
        let mut fake = Fake::new("TARGETS\nimage/png", Ran::Output(png(4, 3)));
        fake.paste(&request(&[Tool::Xclip], None), &dir.0).unwrap();
        assert_eq!(
            fake.commands(),
            [
                "xclip -selection clipboard -t TARGETS -o",
                "xclip -selection clipboard -t image/png -o"
            ]
        );
        // Both readings share one deadline, a few seconds ahead.
        let deadlines: Vec<Instant> = fake.ran.iter().map(|(_, _, at)| *at).collect();
        assert_eq!(deadlines[0], deadlines[1]);
        let ahead = deadlines[0].saturating_duration_since(Instant::now());
        assert!(
            ahead > Duration::from_secs(1) && ahead <= TIME_LIMIT,
            "{ahead:?}"
        );
    }

    #[test]
    fn a_picture_ends_in_a_private_file_that_goes_away() {
        let dir = Scratch::new("clip-file");
        let picture = png(40, 30);
        let mut fake = Fake::new("text/plain\nimage/png", Ran::Output(picture.clone()));
        let file = fake
            .paste(
                &request(&[Tool::WlPaste, Tool::Xclip], Some(1_000_000)),
                &dir.0,
            )
            .unwrap();
        let path = file.path().to_owned();
        assert_eq!(path.parent(), Some(dir.0.as_path()));
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("pasted-") && name.ends_with(".png"),
            "{name}"
        );
        assert_eq!(fs::read(&path).unwrap(), picture);
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        // The same second twice is two files.
        let other = fake
            .paste(&request(&[Tool::WlPaste], None), &dir.0)
            .unwrap();
        assert_ne!(other.path(), path);
        drop(file);
        assert!(!path.exists());
        assert!(other.path().exists());

        // A folder that is not there yet is made, for the owner only.
        let inner = dir.0.join("run");
        let file = fake
            .paste(&request(&[Tool::WlPaste], None), &inner)
            .unwrap();
        assert!(file.path().starts_with(&inner));
        let mode = fs::metadata(&inner).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
        // One that cannot be made is said so.
        let blocker = dir.0.join("blocker");
        fs::write(&blocker, "").unwrap();
        assert_eq!(
            fake.paste(&request(&[Tool::WlPaste], None), &blocker.join("x"))
                .unwrap_err(),
            Failure::TempFile
        );
    }

    #[test]
    fn a_clipboard_without_a_picture_says_so() {
        let dir = Scratch::new("clip-none");
        let tools = [Tool::WlPaste];
        // Only text on offer: the picture is not even asked for.
        let mut fake = Fake::new("text/plain\nUTF8_STRING", Ran::Output(png(4, 3)));
        assert_eq!(
            fake.paste(&request(&tools, None), &dir.0).unwrap_err(),
            Failure::NoPicture
        );
        assert_eq!(fake.ran.len(), 1);
        // Nothing copied at all: the tools end with an error.
        for list in [Ran::Failed, Ran::TooLarge] {
            let mut fake = Fake::new("", Ran::Output(png(4, 3)));
            fake.list = list;
            assert_eq!(
                fake.paste(&request(&tools, None), &dir.0).unwrap_err(),
                Failure::NoPicture
            );
        }
        // Offered, but nothing came.
        let mut fake = Fake::new("image/png", Ran::Output(Vec::new()));
        assert_eq!(
            fake.paste(&request(&tools, None), &dir.0).unwrap_err(),
            Failure::NoPicture
        );
        assert_eq!(Failure::NoPicture.message(), "No picture in the clipboard");
        // A tool that fails while giving what it offered.
        let mut fake = Fake::new("image/png", Ran::Failed);
        assert_eq!(
            fake.paste(&request(&tools, None), &dir.0).unwrap_err(),
            Failure::Failed
        );
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
    }

    #[test]
    fn a_missing_tool_names_what_to_install() {
        let dir = Scratch::new("clip-missing");
        let mut fake = Fake::new("image/png", Ran::Output(png(4, 3)));
        fake.list = Ran::Missing;
        let failure = fake
            .paste(&request(&[Tool::WlPaste, Tool::Xclip], None), &dir.0)
            .unwrap_err();
        assert_eq!(failure, Failure::Missing(Tool::WlPaste));
        assert_eq!(failure.message(), "Install wl-clipboard to paste pictures");
        assert_eq!(fake.ran.len(), 2);
        let failure = fake
            .paste(&request(&[Tool::Xclip], None), &dir.0)
            .unwrap_err();
        assert_eq!(failure.message(), "Install xclip to paste pictures");
        // No desktop at all.
        assert_eq!(
            fake.paste(&request(&[], None), &dir.0).unwrap_err(),
            Failure::NoDesktop
        );

        // Without wl-paste, xclip is asked (under XWayland it sees the clipboard too).
        let mut ran = Vec::new();
        let file = paste(
            &request(&[Tool::WlPaste, Tool::Xclip], None),
            &dir.0,
            |argv, _, _| {
                ran.push(argv[0].to_owned());
                match argv {
                    ["wl-paste", ..] => Ran::Missing,
                    [.., "TARGETS", "-o"] => Ran::Output(b"image/png".to_vec()),
                    _ => Ran::Output(png(4, 3)),
                }
            },
        );
        assert!(file.is_ok());
        assert_eq!(ran, ["wl-paste", "xclip", "xclip"]);
    }

    #[test]
    fn too_large_a_picture_is_refused_at_the_attachment_limit() {
        let dir = Scratch::new("clip-large");
        let tools = [Tool::WlPaste];
        // The tool is held to the server's limit, else to the one used for attaching.
        let mut fake = Fake::new("image/png", Ran::TooLarge);
        let failure = fake
            .paste(&request(&tools, Some(1000)), &dir.0)
            .unwrap_err();
        assert_eq!(failure, Failure::TooLarge(Some(1000)));
        assert_eq!(fake.ran[1].1, 1000);
        assert_eq!(
            failure.message(),
            "The picture in the clipboard is too large: the server takes up to 1.0 kB"
        );
        let failure = fake.paste(&request(&tools, None), &dir.0).unwrap_err();
        assert_eq!(fake.ran[3].1, FALLBACK_LIMIT);
        assert!(
            failure.message().contains("100 MB is tried"),
            "{}",
            failure.message()
        );
        // The list of kinds has a small bound of its own.
        assert_eq!(fake.ran[0].1, MAX_LIST);
        // A runner that gives more than it was allowed is not believed.
        let mut fake = Fake::new("image/png", Ran::Output(png(40, 30)));
        assert_eq!(
            fake.paste(&request(&tools, Some(100)), &dir.0).unwrap_err(),
            Failure::TooLarge(Some(100))
        );
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
    }

    #[test]
    fn a_clipboard_that_does_not_answer_in_time() {
        let dir = Scratch::new("clip-slow");
        let tools = [Tool::Xclip];
        let mut fake = Fake::new("image/png", Ran::TimedOut);
        let failure = fake.paste(&request(&tools, None), &dir.0).unwrap_err();
        assert_eq!(failure, Failure::TimedOut);
        assert_eq!(failure.message(), "The clipboard did not answer in time");
        let mut fake = Fake::new("", Ran::Output(png(4, 3)));
        fake.list = Ran::TimedOut;
        assert_eq!(
            fake.paste(&request(&tools, None), &dir.0).unwrap_err(),
            Failure::TimedOut
        );
        assert_eq!(fake.ran.len(), 1);
    }

    #[test]
    fn what_is_not_a_picture_of_its_kind_is_refused() {
        let dir = Scratch::new("clip-fake");
        let tools = [Tool::WlPaste];
        let picture = png(40, 30);
        // A header that claims a picture of absurd size: never decoded.
        let mut absurd = picture[..33].to_vec();
        absurd[16..24].copy_from_slice(&[0x00, 0x00, 0x7f, 0xff, 0x00, 0x00, 0x7f, 0xff]);
        for (list, content) in [
            ("image/png", b"<html>not a picture</html>".to_vec()),
            // Says JPEG, is a PNG.
            ("image/jpeg", picture.clone()),
            // Cut off after its header.
            ("image/png", picture[..60].to_vec()),
            ("image/png", absurd),
            ("image/gif", b"GIF89a".to_vec()),
        ] {
            let mut fake = Fake::new(list, Ran::Output(content));
            let failure = fake.paste(&request(&tools, None), &dir.0).unwrap_err();
            assert_eq!(failure, Failure::NotAPicture, "{list}");
        }
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
        // The real thing in another of the kinds.
        let mut gif = Vec::new();
        crate::images::demo_picture(8, 8)
            .write_to(&mut Cursor::new(&mut gif), ImageFormat::Gif)
            .unwrap();
        let mut fake = Fake::new("image/gif", Ran::Output(gif));
        let file = fake.paste(&request(&tools, None), &dir.0).unwrap();
        assert!(file.path().to_str().unwrap().ends_with(".gif"));
    }

    #[test]
    fn the_tool_is_run_without_a_shell_and_within_its_limits() {
        let soon = |ms| Instant::now() + Duration::from_millis(ms);
        assert_eq!(
            run_tool(&["printf", "a b"], 100, soon(5000)),
            Ran::Output(b"a b".to_vec())
        );
        // Words are not split again, and nothing is expanded.
        assert_eq!(
            run_tool(&["printf", "%s|", "$HOME", "a b"], 100, soon(5000)),
            Ran::Output(b"$HOME|a b|".to_vec())
        );
        assert_eq!(run_tool(&["false"], 100, soon(5000)), Ran::Failed);
        assert_eq!(
            run_tool(&["/nonexistent/wl-paste"], 100, soon(5000)),
            Ran::Missing
        );
        assert_eq!(run_tool(&[], 100, soon(5000)), Ran::Failed);
        // Exactly the bound is fine; one more byte is not.
        assert_eq!(
            run_tool(&["head", "-c", "100", "/dev/zero"], 100, soon(5000)),
            Ran::Output(vec![0; 100])
        );
        assert_eq!(
            run_tool(&["head", "-c", "101", "/dev/zero"], 100, soon(5000)),
            Ran::TooLarge
        );
        // Without end: stopped at the bound, at once.
        let started = Instant::now();
        assert_eq!(
            run_tool(&["cat", "/dev/zero"], 1_000_000, soon(5000)),
            Ran::TooLarge
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn a_tool_past_its_time_is_killed() {
        let dir = Scratch::new("clip-kill");
        let marker = dir.0.join("still-running");
        let soon = || Instant::now() + Duration::from_millis(200);
        let started = Instant::now();
        // Silent, writing and then hanging, closing its output and hanging: all the same.
        assert_eq!(run_tool(&["sleep", "10"], 100, soon()), Ran::TimedOut);
        assert_eq!(
            run_tool(&["sh", "-c", "printf abc; exec sleep 10"], 100, soon()),
            Ran::TimedOut
        );
        assert_eq!(
            run_tool(&["sh", "-c", "exec >&-; exec sleep 10"], 100, soon()),
            Ran::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        // Killed, not left behind: it never gets to leave its mark.
        let script = format!("sleep 1; : > '{}'", marker.display());
        assert_eq!(
            run_tool(&["sh", "-c", script.as_str()], 100, soon()),
            Ran::TimedOut
        );
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!marker.exists());
    }

    #[test]
    fn nothing_of_the_clipboard_or_the_file_is_in_debug_output() {
        let dir = Scratch::new("clip-debug");
        let ran = Ran::Output(b"secret words".to_vec());
        assert_eq!(format!("{ran:?}"), "Output(12 bytes)");
        let mut fake = Fake::new("image/png", Ran::Output(png(4, 3)));
        let done = Done {
            id: 7,
            result: fake.paste(&request(&[Tool::WlPaste], None), &dir.0),
        };
        let debug = format!("{done:?}");
        assert!(!debug.contains("pasted") && !debug.contains('/'), "{debug}");
    }
}
