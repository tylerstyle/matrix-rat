//! Sending files, the part that needs no Matrix: recognising dropped paths, checking a
//! chosen file, completing a path being typed, and getting a file ready for upload
//! (its type, and for a picture its size and a small version).
//!
//! The types, the limits, the words of a command or a paste and what a file's bytes
//! are come from here; everything that looks at the disk or the home folder is in
//! [`disk`].

#[cfg(not(target_family = "wasm"))]
mod disk;
/// The browser has no disk to look at: no path names a file there, a paste is text,
/// and nothing is read (phase 2 sends what the browser's file picker gives).
#[cfg(target_family = "wasm")]
mod disk {
    use std::path::{Path, PathBuf};

    use super::{Chosen, Completion, Prepared};

    const NO_DISK: &str = "There are no files on a disk here.";

    pub fn check(_path: &Path) -> Result<Chosen, String> {
        Err(NO_DISK.to_owned())
    }

    pub fn dropped_files(_text: &str) -> Option<Vec<PathBuf>> {
        None
    }

    pub fn command_files(_argument: &str) -> Result<Vec<Chosen>, String> {
        Err(NO_DISK.to_owned())
    }

    pub fn complete(_typed: &str) -> Option<Completion> {
        None
    }

    pub fn prepare(_path: &Path, _limit: Option<u64>) -> Result<Prepared, String> {
        Err(NO_DISK.to_owned())
    }

    pub fn prepare_as(
        _path: &Path,
        _name: String,
        _limit: Option<u64>,
    ) -> Result<Prepared, String> {
        Err(NO_DISK.to_owned())
    }
}

#[cfg(any(not(target_family = "wasm"), test))]
use std::io::Cursor;
#[cfg(any(not(target_family = "wasm"), test))]
use std::path::Path;
use std::path::PathBuf;

#[cfg(any(not(target_family = "wasm"), test))]
use image::{DynamicImage, ImageDecoder, ImageReader, Limits};

use crate::images::human_size;
#[cfg(any(not(target_family = "wasm"), test))]
use crate::images::{MAX_DECODE_ALLOC, MAX_DECODE_SIDE};
#[cfg(any(not(target_family = "wasm"), test))]
use crate::text::sanitize_line;

pub use disk::{check, command_files, complete, dropped_files, prepare, prepare_as};

/// What is accepted when the server's own limit could not be asked for: the whole file
/// passes through memory.
pub const FALLBACK_LIMIT: u64 = crate::files::LARGE_FILE;
/// The box a picture's small version fits into, as other Matrix apps make them.
#[cfg(any(not(target_family = "wasm"), test))]
const THUMBNAIL: (u32, u32) = (800, 600);

/// A file the user picked, as shown in the composer before it is sent.
#[derive(Clone, PartialEq, Eq)]
pub struct Chosen {
    pub path: PathBuf,
    /// Safe to show.
    pub name: String,
    pub size: u64,
}

/// Hand-written: the path and the name of a file can say a lot about the user.
impl std::fmt::Debug for Chosen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Chosen({} bytes)", self.size)
    }
}

#[cfg(any(not(target_family = "wasm"), test))]
fn display_name(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|name| sanitize_line(&name.to_string_lossy()))
        .unwrap_or_default();
    if name.trim().is_empty() {
        "file".to_owned()
    } else {
        name
    }
}

#[cfg(any(not(target_family = "wasm"), test))]
fn unreadable(name: &str, err: &std::io::Error) -> String {
    use std::io::ErrorKind::{NotFound, PermissionDenied};
    #[expect(clippy::wildcard_enum_match_arm, reason = "foreign std::io::ErrorKind")]
    match err.kind() {
        NotFound => format!("{name} does not exist"),
        PermissionDenied => format!("{name} cannot be read (permission denied)"),
        _ => format!("{name} cannot be read"),
    }
}

/// Why a file of `size` bytes is refused, if it is. `limit` is the server's, if known.
pub fn too_large(name: &str, size: u64, limit: Option<u64>) -> Option<String> {
    match limit {
        Some(limit) if size > limit => Some(format!(
            "{name} is too large: {}, the server takes up to {}",
            human_size(size),
            human_size(limit)
        )),
        None if size > FALLBACK_LIMIT => Some(format!(
            "{name} is too large: {}, and the server's limit is not known (up to {} is tried)",
            human_size(size),
            human_size(FALLBACK_LIMIT)
        )),
        _ => None,
    }
}

// ----- dropped and pasted paths ------------------------------------------------------

/// Splits like a shell would: quotes group, a backslash takes the next character
/// literally. `None` for an unfinished quote.
#[cfg(any(not(target_family = "wasm"), test))]
fn shell_words(text: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => word.push(c),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            c @ ('"' | '\\' | '$' | '`') => word.push(c),
                            c => {
                                word.push('\\');
                                word.push(c);
                            }
                        },
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.push(chars.next()?);
            }
            c if c.is_whitespace() => {
                if std::mem::take(&mut in_word) {
                    words.push(std::mem::take(&mut word));
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Some(words)
}

// ----- the /upload command -----------------------------------------------------------

/// The argument of `/upload` (or `/file`) if `input` is that command.
pub fn command_argument(input: &str) -> Option<&str> {
    let rest = input.trim_start().strip_prefix('/')?;
    let rest = ["upload", "file"]
        .iter()
        .find_map(|name| rest.strip_prefix(name))?;
    if rest.is_empty() {
        Some("")
    } else {
        rest.starts_with(char::is_whitespace)
            .then(|| rest.trim_start())
    }
}

/// What Tab makes of a path being typed.
// Tests compare it with `assert_eq!`: there it shows everything, for a failure to be read.
#[cfg_attr(test, derive(Debug))]
#[derive(Clone, PartialEq, Eq)]
pub struct Completion {
    /// The path, extended as far as it is certain.
    pub text: String,
    /// What could follow, when there is more than one possibility.
    pub options: Vec<String>,
}

/// Outside tests: the paths stay out of logs.
#[cfg(not(test))]
impl std::fmt::Debug for Completion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Completion")
            .field("options", &self.options.len())
            .finish_non_exhaustive()
    }
}

// ----- getting a file ready ----------------------------------------------------------

/// A file read into memory, with what the message about it will say.
pub struct Prepared {
    pub name: String,
    pub bytes: Vec<u8>,
    pub mime: String,
    pub picture: Option<Picture>,
}

pub struct Picture {
    /// Pixel size as it is meant to be seen (after turning a camera's picture upright).
    pub width: u32,
    pub height: u32,
    pub thumbnail: Option<Thumbnail>,
}

pub struct Thumbnail {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
}

/// `None` for anything this app cannot read as a picture; it is then sent as a file
/// of its type like any other.
#[cfg(any(not(target_family = "wasm"), test))]
fn picture(bytes: &[u8], format: image::ImageFormat) -> Option<Picture> {
    use image::metadata::Orientation::{Rotate90, Rotate90FlipH, Rotate270, Rotate270FlipH};
    // The same bounds as for pictures that are shown: beyond them a picture is sent
    // as the file it is, without a size in pixels or a small version.
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODE_SIDE);
    limits.max_image_height = Some(MAX_DECODE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().ok()?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let sideways = matches!(
        orientation,
        Rotate90 | Rotate270 | Rotate90FlipH | Rotate270FlipH
    );
    let (shown_width, shown_height) = if sideways {
        (height, width)
    } else {
        (width, height)
    };
    let mut picture = Picture {
        width: shown_width,
        height: shown_height,
        thumbnail: None,
    };
    // A picture that is small already is its own small version.
    if shown_width <= THUMBNAIL.0 && shown_height <= THUMBNAIL.1 {
        return Some(picture);
    }
    // Whatever the file's own format, turning and scaling may need four bytes a pixel.
    let as_rgba = u64::from(width)
        .saturating_mul(u64::from(height))
        .saturating_mul(4);
    if decoder.total_bytes().max(as_rgba) > MAX_DECODE_ALLOC {
        return Some(picture);
    }
    let Ok(mut image) = DynamicImage::from_decoder(decoder) else {
        // Damaged somewhere past its header: still the user's file to send.
        return Some(picture);
    };
    image.apply_orientation(orientation);
    let small = image.thumbnail(THUMBNAIL.0, THUMBNAIL.1);
    let mut out = Vec::new();
    let (encoded, mime) = if small.color().has_alpha() {
        (
            small.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png),
            "image/png",
        )
    } else {
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80);
        (small.to_rgb8().write_with_encoder(encoder), "image/jpeg")
    };
    // Only worth sending along if it saves anything.
    if encoded.is_ok() && out.len() < bytes.len() {
        picture.thumbnail = Some(Thumbnail {
            width: small.width(),
            height: small.height(),
            bytes: out,
            mime,
        });
    }
    Some(picture)
}

/// Types by the end of the file name, in alphabetical order (it is searched by halving).
#[cfg(any(not(target_family = "wasm"), test))]
const BY_EXTENSION: &[(&str, &str)] = &[
    ("7z", "application/x-7z-compressed"),
    ("aac", "audio/aac"),
    ("avi", "video/x-msvideo"),
    ("avif", "image/avif"),
    ("bmp", "image/bmp"),
    ("bz2", "application/x-bzip2"),
    ("css", "text/css"),
    ("csv", "text/csv"),
    ("doc", "application/msword"),
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    ("epub", "application/epub+zip"),
    ("flac", "audio/flac"),
    ("gif", "image/gif"),
    ("gz", "application/gzip"),
    ("heic", "image/heic"),
    ("htm", "text/html"),
    ("html", "text/html"),
    ("ics", "text/calendar"),
    ("jpeg", "image/jpeg"),
    ("jpg", "image/jpeg"),
    ("json", "application/json"),
    ("m4a", "audio/mp4"),
    ("md", "text/markdown"),
    ("mkv", "video/x-matroska"),
    ("mov", "video/quicktime"),
    ("mp3", "audio/mpeg"),
    ("mp4", "video/mp4"),
    ("odp", "application/vnd.oasis.opendocument.presentation"),
    ("ods", "application/vnd.oasis.opendocument.spreadsheet"),
    ("odt", "application/vnd.oasis.opendocument.text"),
    ("oga", "audio/ogg"),
    ("ogg", "audio/ogg"),
    ("ogv", "video/ogg"),
    ("opus", "audio/ogg"),
    ("pdf", "application/pdf"),
    ("png", "image/png"),
    ("ppt", "application/vnd.ms-powerpoint"),
    (
        "pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    ("rtf", "application/rtf"),
    ("svg", "image/svg+xml"),
    ("tar", "application/x-tar"),
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
    ("txt", "text/plain"),
    ("wav", "audio/wav"),
    ("webm", "video/webm"),
    ("webp", "image/webp"),
    ("xls", "application/vnd.ms-excel"),
    (
        "xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    ("xml", "application/xml"),
    ("xz", "application/x-xz"),
    ("zip", "application/zip"),
    ("zst", "application/zstd"),
];

/// The type of a file that is not a picture this app can read: by its name, else by
/// whether it looks like text.
#[cfg(any(not(target_family = "wasm"), test))]
fn mime_type(name: &str, bytes: &[u8]) -> &'static str {
    let extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    if let Some(extension) = extension
        && let Ok(at) = BY_EXTENSION.binary_search_by(|(known, _)| (*known).cmp(extension.as_str()))
    {
        return BY_EXTENSION[at].1;
    }
    // Text if the beginning reads as text. A cut through a character at the end of
    // the sample does not count against it.
    let sample = &bytes[..bytes.len().min(8192)];
    let text = match std::str::from_utf8(sample) {
        Ok(text) => Some(text),
        Err(err) if err.error_len().is_none() => {
            std::str::from_utf8(&sample[..err.valid_up_to()]).ok()
        }
        Err(_) => None,
    };
    match text {
        Some(text) if !text.contains(|c: char| c.is_control() && !c.is_whitespace()) => {
            "text/plain"
        }
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A folder of its own for one test, removed when dropped.
    pub(crate) struct Scratch(pub PathBuf);

    impl Scratch {
        pub(crate) fn new(test: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("matrix-rat-upload-{test}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir.canonicalize().unwrap())
        }

        pub(crate) fn file(&self, name: &str, content: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, content).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn size_limits() {
        assert_eq!(too_large("a", 10, Some(10)), None);
        assert_eq!(
            too_large("a.zip", 25_000_000, Some(20_000_000)).unwrap(),
            "a.zip is too large: 25 MB, the server takes up to 20 MB"
        );
        assert_eq!(too_large("a", FALLBACK_LIMIT, None), None);
        assert!(too_large("a", FALLBACK_LIMIT + 1, None).is_some());
    }

    #[test]
    fn the_upload_command() {
        assert_eq!(command_argument("/upload ~/a b.txt"), Some("~/a b.txt"));
        assert_eq!(command_argument("/file  /tmp/x"), Some("/tmp/x"));
        assert_eq!(command_argument("/upload"), Some(""));
        assert_eq!(command_argument("/upload "), Some(""));
        assert_eq!(command_argument("/uploads x"), None);
        assert_eq!(command_argument("/files"), None);
        assert_eq!(command_argument("//upload x"), None);
        assert_eq!(command_argument("upload x"), None);
        assert_eq!(command_argument("/me uploads"), None);
    }
}
