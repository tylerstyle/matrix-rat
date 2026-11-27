//! Pasting a picture from the desktop's clipboard (`Action::PastePicture`).
//!
//! A terminal pastes only text, so the picture is asked of the desktop's own tool, run
//! without a shell: `wl-paste` on Wayland, `xclip` on X11. What it writes is untrusted:
//! it is read with a size and a time limit (the tool is killed past either), decoded with
//! the same guards as the pictures in the timeline to be sure it is a picture of the kind
//! that was asked for, and only then written to a private temporary file, which the app
//! attaches like a file picked by hand (`app::clipboard`).
//!
//! The pure parts live here. Running the tool, the environment that picks it and the
//! temporary file are in [`system`], with the running of the tool behind a function
//! (see [`run_tool`]) so that the rest can be tried without a clipboard. Nothing the
//! clipboard holds, nothing a tool writes and no file name is ever logged or shown in
//! `Debug` output.

mod system;

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};

use image::ImageFormat;

use crate::images::human_size;
use crate::upload::FALLBACK_LIMIT;

pub use system::{paste, run_tool, tools_from_env};

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
/// gone when this is dropped (made and removed in [`system`]).
pub struct TempPicture {
    path: PathBuf,
}

impl TempPicture {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Debug for TempPicture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TempPicture")
    }
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
    use std::ffi::OsStr;

    use super::*;

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
}
