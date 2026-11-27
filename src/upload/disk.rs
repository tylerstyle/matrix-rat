//! The files on this machine's disk: whether a chosen one can be sent, which files a
//! paste or an `/upload` names, completing a path from its folder, and reading a file
//! to send. `~` is the `HOME` variable's folder.

use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::{
    Chosen, Completion, FALLBACK_LIMIT, Prepared, display_name, mime_type, picture, shell_words,
    too_large, unreadable,
};

/// A paste longer than this is text, whatever it looks like.
const MAX_DROP_TEXT: usize = 16 * 1024;
/// More files than this in one paste is more likely a listing than a drop.
const MAX_DROP_FILES: usize = 20;
/// Folder entries looked at for a completion.
const MAX_ENTRIES: usize = 20_000;

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

/// Opens for reading without ever waiting (a pipe would otherwise block until someone
/// writes to it). Links are followed: the user chose the path.
fn open(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
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

// ----- dropped and pasted paths ------------------------------------------------------

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

/// Reads `path` and works out its type; for pictures also the size and a small
/// version. Slow (disk, decoding): runs on a blocking thread. The error is a sentence
/// for the user.
pub fn prepare(path: &Path, limit: Option<u64>) -> Result<Prepared, String> {
    prepare_as(path, display_name(path), limit)
}

/// [`prepare`] for a file that is called `name` whatever its path says: what cannot
/// be sent is then reported under that name.
pub fn prepare_as(path: &Path, name: String, limit: Option<u64>) -> Result<Prepared, String> {
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

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::images::MAX_DECODE_SIDE;
    use crate::upload::BY_EXTENSION;
    use crate::upload::tests::Scratch;

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
    fn size_limits_when_a_file_is_read() {
        let scratch = Scratch::new("limit");
        let file = scratch.file("big.bin", &[0; 2000]);
        assert!(prepare(&file, Some(2000)).is_ok());
        assert_eq!(
            prepare(&file, Some(1999)).err().unwrap(),
            "big.bin is too large: 2.0 kB, the server takes up to 2.0 kB"
        );
    }

    #[test]
    fn the_files_an_upload_command_names() {
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
