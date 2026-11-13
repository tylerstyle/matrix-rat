//! Sending files, the part that needs no Matrix: recognising dropped paths, checking a
//! chosen file, completing a path being typed, and getting a file ready for upload
//! (its type, and for a picture its size and a small version).

use std::io::{Cursor, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use image::{DynamicImage, ImageDecoder, ImageReader, Limits};

use crate::images::{MAX_DECODE_ALLOC, MAX_DECODE_SIDE, human_size};
use crate::text::sanitize_line;

/// A paste longer than this is text, whatever it looks like.
const MAX_DROP_TEXT: usize = 16 * 1024;
/// More files than this in one paste is more likely a listing than a drop.
const MAX_DROP_FILES: usize = 20;
/// What is accepted when the server's own limit could not be asked for: the whole file
/// passes through memory.
pub const FALLBACK_LIMIT: u64 = crate::files::LARGE_FILE;
/// The box a picture's small version fits into, as other Matrix apps make them.
const THUMBNAIL: (u32, u32) = (800, 600);
/// Folder entries looked at for a completion.
const MAX_ENTRIES: usize = 20_000;

/// A file the user picked, as shown in the composer before it is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    pub path: PathBuf,
    /// Safe to show.
    pub name: String,
    pub size: u64,
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `~` and `~/…` name the home folder, as in a shell.
fn expand(path: &str) -> PathBuf {
    if path == "~" {
        home()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(path)
    }
}

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

/// Opens for reading without ever waiting (a pipe would otherwise block until someone
/// writes to it). Links are followed: the user chose the path.
fn open(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

fn unreadable(name: &str, err: &std::io::Error) -> String {
    use std::io::ErrorKind::{NotFound, PermissionDenied};
    match err.kind() {
        NotFound => format!("{name} does not exist"),
        PermissionDenied => format!("{name} cannot be read (permission denied)"),
        _ => format!("{name} cannot be read"),
    }
}

/// Whether `path` is something that can be sent. The error is a sentence for the user.
pub fn check(path: &Path) -> Result<Chosen, String> {
    let name = display_name(path);
    let meta = std::fs::metadata(path).map_err(|err| unreadable(&name, &err))?;
    if meta.is_dir() {
        return Err(format!("{name} is a folder; only files can be sent"));
    }
    if !meta.is_file() {
        return Err(format!("{name} is not a regular file"));
    }
    open(path).map_err(|err| unreadable(&name, &err))?;
    Ok(Chosen {
        path: path.to_owned(),
        name,
        size: meta.len(),
    })
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

/// One pasted word as a path: `file://` addresses, `~/…` and absolute paths. A bare
/// word is never a path, or pasting "notes.txt" into a message would attach a file.
fn dropped_path(word: &str) -> Option<PathBuf> {
    if word.starts_with("file:") {
        let url = url::Url::parse(word).ok()?;
        // A file has no query and no fragment; with either, this is something else.
        return (url.scheme() == "file" && url.query().is_none() && url.fragment().is_none())
            .then(|| url.to_file_path().ok())
            .flatten();
    }
    let path = expand(word);
    (path.is_absolute() && (word.starts_with('/') || word.starts_with("~/"))).then_some(path)
}

/// A path as typed after `/upload`: like a shell would take it, and `file://` addresses.
fn typed_path(word: &str) -> PathBuf {
    match dropped_path(word) {
        Some(path) if word.starts_with("file:") => path,
        _ => expand(word),
    }
}

fn existing_files<'a>(words: impl Iterator<Item = &'a str>) -> Option<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for word in words {
        let path = dropped_path(word)?;
        if paths.contains(&path) {
            continue;
        }
        if paths.len() == MAX_DROP_FILES
            || !std::fs::metadata(&path).is_ok_and(|meta| meta.is_file())
        {
            return None;
        }
        paths.push(path);
    }
    (!paths.is_empty()).then_some(paths)
}

/// The files a paste stands for, if it consists of nothing but the paths of existing
/// files: what a terminal types when files are dropped on it (one per line or
/// separated by spaces, plain, quoted, backslash-escaped or as `file://` addresses).
/// Anything else is text.
pub fn dropped_files(text: &str) -> Option<Vec<PathBuf>> {
    let text = text.trim();
    if text.is_empty() || text.len() > MAX_DROP_TEXT || text.contains('\0') {
        return None;
    }
    // Whole lines first: a plain path may contain spaces.
    let lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    if let Some(paths) = existing_files(lines) {
        return Some(paths);
    }
    let words = shell_words(text)?;
    existing_files(words.iter().map(String::as_str))
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

/// The files an `/upload` argument names: one path (which may contain spaces), or
/// several written as a shell would take them. Relative paths count from the folder the
/// app was started in.
pub fn command_files(argument: &str) -> Result<Vec<Chosen>, String> {
    let argument = argument.trim();
    if argument.is_empty() {
        return Err("/upload needs the path of a file (Tab completes it)".to_owned());
    }
    let whole = check(&typed_path(argument));
    if whole.is_ok() {
        return whole.map(|file| vec![file]);
    }
    match shell_words(argument) {
        Some(words) if words.len() > 1 || words.first().is_some_and(|w| w != argument) => {
            words.iter().map(|word| check(&typed_path(word))).collect()
        }
        _ => whole.map(|file| vec![file]),
    }
}

/// What Tab makes of a path being typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The path, extended as far as it is certain.
    pub text: String,
    /// What could follow, when there is more than one possibility.
    pub options: Vec<String>,
}

/// Completes the last component of `typed` from the folder it is in. `None` when
/// nothing matches.
pub fn complete(typed: &str) -> Option<Completion> {
    if typed == "~" {
        return Some(Completion {
            text: "~/".to_owned(),
            options: Vec::new(),
        });
    }
    let (folder, prefix) = match typed.rfind('/') {
        Some(slash) => typed.split_at(slash + 1),
        None => ("", typed),
    };
    let dir = if folder.is_empty() {
        PathBuf::from(".")
    } else {
        expand(folder)
    };
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .ok()?
        .take(MAX_ENTRIES)
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().into_string().ok()?;
            // Hidden files only when asked for; names that cannot be typed, never.
            if !name.starts_with(prefix)
                || (name.starts_with('.') && !prefix.starts_with('.'))
                || name.contains(char::is_control)
            {
                return None;
            }
            // The folder listing says what most entries are; only a link has to be
            // followed, so that a linked folder completes like a folder.
            let is_dir = match entry.file_type() {
                Ok(kind) if kind.is_symlink() => {
                    std::fs::metadata(entry.path()).is_ok_and(|meta| meta.is_dir())
                }
                Ok(kind) => kind.is_dir(),
                Err(_) => false,
            };
            Some(if is_dir { format!("{name}/") } else { name })
        })
        .collect();
    names.sort();
    let mut common = names.first()?.as_str();
    for name in &names[1..] {
        let same = common
            .char_indices()
            .zip(name.chars())
            .find(|((_, a), b)| a != b)
            .map_or(common.len().min(name.len()), |((at, _), _)| at);
        common = &common[..same];
    }
    Some(Completion {
        text: format!("{folder}{common}"),
        options: if names.len() > 1 { names } else { Vec::new() },
    })
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

/// Reads `path` and works out its type; for pictures also the size and a small
/// version. Slow (disk, decoding): runs on a blocking thread. The error is a sentence
/// for the user.
pub fn prepare(path: &Path, limit: Option<u64>) -> Result<Prepared, String> {
    let name = display_name(path);
    let file = open(path).map_err(|err| unreadable(&name, &err))?;
    // Asked of the open file, so it is the very thing that gets read.
    let meta = file.metadata().map_err(|err| unreadable(&name, &err))?;
    if meta.is_dir() {
        return Err(format!("{name} is a folder; only files can be sent"));
    }
    if !meta.is_file() {
        return Err(format!("{name} is not a regular file"));
    }
    if let Some(reason) = too_large(&name, meta.len(), limit) {
        return Err(reason);
    }
    // The file may have grown since: never read past the limit.
    let most = limit.unwrap_or(FALLBACK_LIMIT);
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    file.take(most.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|err| unreadable(&name, &err))?;
    if let Some(reason) = too_large(&name, bytes.len() as u64, limit) {
        return Err(reason);
    }
    let format = image::guess_format(&bytes).ok();
    let picture = format.and_then(|format| picture(&bytes, format));
    let mime = match (&picture, format) {
        (Some(_), Some(format)) => format.to_mime_type().to_owned(),
        _ => mime_type(&name, &bytes).to_owned(),
    };
    Ok(Prepared {
        name,
        bytes,
        mime,
        picture,
    })
}

/// `None` for anything this app cannot read as a picture; it is then sent as a file
/// of its type like any other.
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

    fn text(path: &Path) -> &str {
        path.to_str().unwrap()
    }

    #[test]
    fn dropped_paths_in_the_forms_terminals_paste_them() {
        let scratch = Scratch::new("drop");
        let plain = scratch.file("report.pdf", b"x");
        let spaced = scratch.file("my holiday (1).jpg", b"x");
        let odd = scratch.file("it's 100%.txt", b"x");
        let dir = text(&scratch.0).to_owned();

        // One plain path, with the trailing space or newline some terminals add.
        assert_eq!(dropped_files(text(&plain)), Some(vec![plain.clone()]));
        assert_eq!(
            dropped_files(&format!("{} \n", text(&plain))),
            Some(vec![plain.clone()])
        );
        // Spaces: unquoted on a line of its own, quoted, or escaped.
        assert_eq!(dropped_files(text(&spaced)), Some(vec![spaced.clone()]));
        assert_eq!(
            dropped_files(&format!("'{dir}/my holiday (1).jpg'")),
            Some(vec![spaced.clone()])
        );
        assert_eq!(
            dropped_files(&format!("\"{dir}/my holiday (1).jpg\"")),
            Some(vec![spaced.clone()])
        );
        assert_eq!(
            dropped_files(&format!("{dir}/my\\ holiday\\ \\(1\\).jpg")),
            Some(vec![spaced.clone()])
        );
        // A quote inside single quotes, the way shells write it.
        assert_eq!(
            dropped_files(&format!("'{dir}/it'\\''s 100%.txt'")),
            Some(vec![odd.clone()])
        );
        // Addresses, with percent-encoding and with the host some programs add.
        assert_eq!(
            dropped_files(&format!("file://{dir}/my%20holiday%20(1).jpg")),
            Some(vec![spaced.clone()])
        );
        assert_eq!(
            dropped_files(&format!("file://localhost{dir}/it's%20100%25.txt")),
            Some(vec![odd.clone()])
        );
        // Several: by line, by space, mixed forms; a repeated one counts once.
        assert_eq!(
            dropped_files(&format!("{}\n{}\n", text(&plain), text(&spaced))),
            Some(vec![plain.clone(), spaced.clone()])
        );
        assert_eq!(
            dropped_files(&format!(
                "{} '{dir}/my holiday (1).jpg' file://{dir}/report.pdf",
                text(&plain)
            )),
            Some(vec![plain.clone(), spaced.clone()])
        );
    }

    #[test]
    fn everything_else_stays_text() {
        let scratch = Scratch::new("text");
        let file = scratch.file("a.txt", b"x");
        let dir = text(&scratch.0).to_owned();
        for paste in [
            "",
            "   ",
            "hello world",
            // A bare or relative name is a word, not a file.
            "a.txt",
            "./a.txt",
            "Cargo.toml",
            // Does not exist, or is a folder.
            &format!("{dir}/missing.txt"),
            &dir,
            "/",
            // A path inside a sentence, or next to one that does not exist.
            &format!("see {}", text(&file)),
            &format!("{} {dir}/missing.txt", text(&file)),
            &format!("{}\nand some words", text(&file)),
            // An unfinished quote, another kind of address, another host.
            &format!("'{}", text(&file)),
            "https://example.org/a.txt",
            &format!("file://{}?download=1", text(&file)),
            &format!("file://{}#top", text(&file)),
            &format!("file://elsewhere.example{}", text(&file)),
        ] {
            assert_eq!(dropped_files(paste), None, "{paste:?}");
        }
        // A device is not a regular file.
        assert_eq!(dropped_files("/dev/null"), None);
        // Far too many at once.
        let many: Vec<String> = (0..=MAX_DROP_FILES)
            .map(|n| text(&scratch.file(&format!("{n}.bin"), b"x")).to_owned())
            .collect();
        assert_eq!(dropped_files(&many.join("\n")), None);
        // The same few files named many times over are still those few.
        let repeated = [many[0].as_str(); MAX_DROP_FILES + 5].join("\n");
        assert_eq!(dropped_files(&repeated).map(|files| files.len()), Some(1));
        assert!(dropped_files(&many[1..].join("\n")).is_some());
    }

    #[test]
    fn home_is_expanded_in_dropped_paths() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        // Any regular file directly in the home folder will do.
        let Some(name) = std::fs::read_dir(&home).ok().and_then(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.metadata().is_ok_and(|meta| meta.is_file()))
                .filter_map(|entry| entry.file_name().into_string().ok())
                .find(|name| !name.contains(char::is_whitespace))
        }) else {
            return;
        };
        assert_eq!(
            dropped_files(&format!("~/{name}")),
            Some(vec![home.join(&name)])
        );
    }

    #[test]
    fn folders_devices_and_missing_files_are_refused_with_a_reason() {
        let scratch = Scratch::new("check");
        let file = scratch.file("notes.txt", b"hello");
        let chosen = check(&file).unwrap();
        assert_eq!((chosen.name.as_str(), chosen.size), ("notes.txt", 5));

        assert_eq!(
            check(&scratch.0.join("nope.txt")).unwrap_err(),
            "nope.txt does not exist"
        );
        assert!(check(&scratch.0).unwrap_err().contains("is a folder"));
        assert_eq!(
            check(Path::new("/dev/null")).unwrap_err(),
            "null is not a regular file"
        );
        assert!(prepare(Path::new("/dev/zero"), None).is_err());
        assert!(prepare(&scratch.0, None).is_err());

        // A link is followed to the file it names.
        let link = scratch.0.join("link.txt");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert_eq!(check(&link).unwrap().size, 5);
        assert_eq!(prepare(&link, None).unwrap().name, "link.txt");

        // No permission (root reads anything, so only checked when it fails to).
        use std::os::unix::fs::PermissionsExt;
        let locked = scratch.file("locked.txt", b"secret");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&locked).is_err() {
            assert_eq!(
                check(&locked).unwrap_err(),
                "locked.txt cannot be read (permission denied)"
            );
            assert!(prepare(&locked, None).is_err());
        }

        // Control characters in a name never reach the screen.
        let odd = scratch.file("a\u{1b}[31mb.txt", b"x");
        assert_eq!(check(&odd).unwrap().name, "a[31mb.txt");
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

        let scratch = Scratch::new("limit");
        let file = scratch.file("big.bin", &[0; 2000]);
        assert!(prepare(&file, Some(2000)).is_ok());
        assert_eq!(
            prepare(&file, Some(1999)).err().unwrap(),
            "big.bin is too large: 2.0 kB, the server takes up to 2.0 kB"
        );
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

        let scratch = Scratch::new("command");
        let spaced = scratch.file("a b.txt", b"12");
        let other = scratch.file("c.txt", b"345");
        let dir = text(&scratch.0).to_owned();
        let names = |argument: &str| -> Result<Vec<String>, String> {
            Ok(command_files(argument)?
                .into_iter()
                .map(|file| file.name)
                .collect())
        };
        // One path, spaces and all; or several, written as for a shell.
        assert_eq!(names(text(&spaced)).unwrap(), ["a b.txt"]);
        assert_eq!(names(&format!("'{dir}/a b.txt'")).unwrap(), ["a b.txt"]);
        assert_eq!(
            names(&format!("{dir}/a\\ b.txt {}", text(&other))).unwrap(),
            ["a b.txt", "c.txt"]
        );
        // An address, as a file manager copies it.
        assert_eq!(
            names(&format!("file://{dir}/a%20b.txt")).unwrap(),
            ["a b.txt"]
        );
        assert_eq!(
            names(&format!("file://{dir}/c.txt {dir}/a\\ b.txt")).unwrap(),
            ["c.txt", "a b.txt"]
        );
        assert!(names("").unwrap_err().contains("needs the path"));
        assert_eq!(
            names(&format!("{dir}/missing.txt")).unwrap_err(),
            "missing.txt does not exist"
        );
        assert!(names(&dir).unwrap_err().contains("is a folder"));
        // One bad path among several refuses them all.
        assert_eq!(
            names(&format!("{} {dir}/missing.txt", text(&other))).unwrap_err(),
            "missing.txt does not exist"
        );
    }

    #[test]
    fn tab_completes_folders_and_files() {
        let scratch = Scratch::new("complete");
        scratch.file("photos/cat.png", b"x");
        scratch.file("photos/car.png", b"x");
        scratch.file("photos/dog one.png", b"x");
        scratch.file("photos/.hidden", b"x");
        scratch.file("plan.md", b"x");
        let dir = text(&scratch.0).to_owned();
        let complete_in = |typed: &str| complete(&format!("{dir}/{typed}"));
        let done = |typed: &str| Completion {
            text: format!("{dir}/{typed}"),
            options: Vec::new(),
        };

        // A single match is completed whole; a folder gets its slash.
        assert_eq!(complete_in("pho"), Some(done("photos/")));
        assert_eq!(complete_in("photos/d"), Some(done("photos/dog one.png")));
        // Several: as far as they agree, and the choices.
        assert_eq!(
            complete_in("p"),
            Some(Completion {
                text: format!("{dir}/p"),
                options: vec!["photos/".into(), "plan.md".into()],
            })
        );
        assert_eq!(
            complete_in("photos/c"),
            Some(Completion {
                text: format!("{dir}/photos/ca"),
                options: vec!["car.png".into(), "cat.png".into()],
            })
        );
        // Hidden files only when the dot is typed.
        assert_eq!(
            complete_in("photos/").unwrap().options,
            ["car.png", "cat.png", "dog one.png"]
        );
        assert_eq!(complete_in("photos/."), Some(done("photos/.hidden")));
        // Nothing matches; no such folder.
        assert_eq!(complete_in("zzz"), None);
        assert_eq!(complete_in("nowhere/x"), None);
        assert_eq!(complete("~").unwrap().text, "~/");
        // The home folder is looked into, and the text keeps its short form.
        if std::env::var_os("HOME").is_some()
            && let Some(completion) = complete("~/")
        {
            assert!(completion.text.starts_with("~/"));
        }
    }

    #[test]
    fn pictures_get_their_size_and_a_small_version() {
        let scratch = Scratch::new("picture");
        let mut png = Vec::new();
        crate::images::demo_picture(1600, 900)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        // The name does not matter: the content says what it is.
        let prepared = prepare(&scratch.file("large.dat", &png), None).unwrap();
        assert_eq!(prepared.mime, "image/png");
        assert_eq!(prepared.bytes, png);
        let picture = prepared.picture.unwrap();
        assert_eq!((picture.width, picture.height), (1600, 900));
        let thumbnail = picture.thumbnail.unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (800, 450));
        assert!(thumbnail.bytes.len() < png.len());
        let small = image::load_from_memory(&thumbnail.bytes).unwrap();
        assert_eq!((small.width(), small.height()), (800, 450));

        // A small picture needs no small version.
        let mut png = Vec::new();
        crate::images::demo_picture(320, 200)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let picture = prepare(&scratch.file("small.png", &png), None)
            .unwrap()
            .picture
            .unwrap();
        assert_eq!((picture.width, picture.height), (320, 200));
        assert!(picture.thumbnail.is_none());

        // A picture cut off after its header is still sent as what it says it is.
        let prepared = prepare(&scratch.file("cut.png", &png[..60]), None).unwrap();
        assert_eq!(prepared.mime, "image/png");
    }

    #[test]
    fn oversized_pictures_are_sent_as_plain_files_of_their_type() {
        let scratch = Scratch::new("bounds");
        // Wider than any picture this app decodes: no pixel size, no small version,
        // and no attempt to load it.
        let mut png = Vec::new();
        crate::images::demo_picture(MAX_DECODE_SIDE + 1, 2)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let prepared = prepare(&scratch.file("wide.png", &png), None).unwrap();
        assert!(prepared.picture.is_none());
        assert_eq!(prepared.mime, "image/png");
        // A header that claims an absurd size is refused the same way, without the
        // arithmetic on it going wrong.
        let mut header = png[..33].to_vec();
        header[16..24].copy_from_slice(&[0x7f, 0xff, 0xff, 0xff, 0x7f, 0xff, 0xff, 0xff]);
        let prepared = prepare(&scratch.file("absurd.png", &header), None).unwrap();
        assert!(prepared.picture.is_none());
        assert_eq!(prepared.mime, "image/png");
    }

    #[test]
    fn other_files_get_a_type() {
        let scratch = Scratch::new("mime");
        let mime = |name: &str, content: &[u8]| {
            let prepared = prepare(&scratch.file(name, content), None).unwrap();
            assert!(prepared.picture.is_none());
            prepared.mime
        };
        assert_eq!(mime("a.PDF", b"%PDF-1.7"), "application/pdf");
        assert_eq!(mime("song.mp3", &[0xff, 0xfb, 0]), "audio/mpeg");
        assert_eq!(mime("clip.mkv", &[0x1a, 0x45]), "video/x-matroska");
        assert_eq!(
            mime("notes", "plain words\nand ümlauts".as_bytes()),
            "text/plain"
        );
        assert_eq!(mime("main.rs", b"fn main() {}\n"), "text/plain");
        assert_eq!(
            mime("blob", &[0, 159, 146, 150]),
            "application/octet-stream"
        );
        assert_eq!(mime("empty", b""), "text/plain");
        // Called a picture, but not one: the name decides, nothing is decoded.
        assert_eq!(mime("fake.png", b"not a png"), "image/png");
        assert_eq!(mime("x.zst", &[0x28, 0xb5]), "application/zstd");
        assert!(BY_EXTENSION.is_sorted_by(|a, b| a.0 < b.0));
    }
}
