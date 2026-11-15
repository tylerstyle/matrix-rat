//! Writing a message in an external editor (`$VISUAL`, then `$EDITOR`).
//!
//! The pure parts live here: which command to run, the temporary file and what to make
//! of what the editor left in it. Handing the terminal over and putting the text back
//! into the composer is the app's business (`app::editor`, `terminal`).
//!
//! Nothing about the draft is ever logged or shown in `Debug` output, and neither is the
//! file's name: it is a message the user has not sent yet.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// What to run and what to start the file with.
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    /// The editor and its own arguments; the file is added as the last one.
    pub argv: Vec<String>,
    /// The composer's text.
    pub text: String,
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("words", &self.argv.len())
            .field("text_len", &self.text.len())
            .finish()
    }
}

/// Why the composer was left as it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Neither `$VISUAL` nor `$EDITOR` names an editor.
    NotSet,
    /// The variable could not be split into words (a quote left open).
    BadCommand,
    /// No temporary file could be made.
    TempFile,
    /// The editor could not be started.
    Launch,
    /// The editor ended with an error (or was killed).
    Exit,
    /// What the editor left is not readable text.
    Unreadable,
}

impl Failure {
    /// For a toast.
    pub fn message(self) -> &'static str {
        match self {
            Failure::NotSet => "Set $VISUAL or $EDITOR to the editor you want, then try again",
            Failure::BadCommand => "$VISUAL or $EDITOR could not be read: is a quote left open?",
            Failure::TempFile => "Could not make a temporary file for the editor",
            Failure::Launch => "Could not start the editor; the draft is as it was",
            Failure::Exit => "The editor did not end well; the draft is as it was",
            Failure::Unreadable => "Could not read what the editor left; the draft is as it was",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The text to put into the composer instead of what is there. May be empty:
    /// the user deleted it all.
    Text(String),
    /// The editor left the same text.
    Unchanged,
    Failed(Failure),
}

impl fmt::Debug for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Text(text) => write!(f, "Text({} bytes)", text.len()),
            Outcome::Unchanged => f.write_str("Unchanged"),
            Outcome::Failed(failure) => write!(f, "Failed({failure:?})"),
        }
    }
}

/// The editor command from the two variables: `$VISUAL` if it is set to something,
/// else `$EDITOR`, split into words like a shell would (`code --wait`,
/// `emacsclient -t`, `'my editor' -f`). No guess when neither is set.
pub fn command(visual: Option<&str>, editor: Option<&str>) -> Result<Vec<String>, Failure> {
    let chosen = [visual, editor]
        .into_iter()
        .flatten()
        .find(|value| !value.trim().is_empty())
        .ok_or(Failure::NotSet)?;
    match shlex::split(chosen) {
        Some(words) if !words.is_empty() => Ok(words),
        Some(_) => Err(Failure::NotSet),
        None => Err(Failure::BadCommand),
    }
}

/// [`command`] for the process's own environment.
pub fn command_from_env() -> Result<Vec<String>, Failure> {
    let var = |name| std::env::var_os(name).and_then(|value| value.into_string().ok());
    command(var("VISUAL").as_deref(), var("EDITOR").as_deref())
}

/// A file holding the draft while the editor has it: owner-only, ending in `.md` (so
/// that editors pick Markdown highlighting), and gone when this is dropped.
#[derive(Debug)]
pub struct TempFile {
    path: PathBuf,
    /// A directory made only for this file, removed with it.
    own_dir: Option<PathBuf>,
}

impl TempFile {
    /// Creates a new file in `dir` (made if need be, owner-only) holding `text`.
    /// A directory that cannot be used is replaced by a new owner-only one inside the
    /// system's temporary directory: never that shared directory itself, because editors
    /// put swap and backup files next to the file they edit.
    pub fn create(dir: &Path, text: &str) -> io::Result<Self> {
        Self::create_in(dir, text).or_else(|_| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.subsec_nanos());
            let own = std::env::temp_dir().join(format!(
                "matrix-rat-compose-{}-{nanos:x}",
                std::process::id()
            ));
            // Fails if anything is already there, so it is ours and private from the start.
            fs::DirBuilder::new().mode(0o700).create(&own)?;
            match Self::create_in(&own, text) {
                Ok(mut file) => {
                    file.own_dir = Some(own);
                    Ok(file)
                }
                Err(err) => {
                    let _ = fs::remove_dir(&own);
                    Err(err)
                }
            }
        })
    }

    fn create_in(dir: &Path, text: &str) -> io::Result<Self> {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        if !dir.is_dir() {
            fs::create_dir_all(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.subsec_nanos());
        let mut last = io::Error::from(io::ErrorKind::AlreadyExists);
        for _ in 0..8 {
            let name = format!(
                "compose-{}-{}-{nanos:x}.md",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            );
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
                    let temp = Self {
                        path,
                        own_dir: None,
                    };
                    file.write_all(text.as_bytes())?;
                    return Ok(temp);
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

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        if let Some(dir) = &self.own_dir {
            // Only if the editor left nothing else in it.
            let _ = fs::remove_dir(dir);
        }
    }
}

/// Runs the editor on the file and waits. True if it ended successfully.
pub fn run_command(argv: &[String], file: &Path) -> io::Result<bool> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    Ok(Command::new(program)
        .args(args)
        .arg(file)
        .status()?
        .success())
}

/// The whole round trip: the draft goes into a file in `dir`, `runner` runs the editor
/// on it (see [`run_command`]), and what is left in the file is judged. The file is
/// removed on every path.
pub fn edit(
    request: &Request,
    dir: &Path,
    runner: impl FnOnce(&[String], &Path) -> io::Result<bool>,
) -> Outcome {
    let Ok(file) = TempFile::create(dir, &request.text) else {
        return Outcome::Failed(Failure::TempFile);
    };
    let ran = runner(&request.argv, file.path());
    let left = fs::read(file.path());
    drop(file);
    decide(&request.text, ran, left)
}

/// What to do with the editor's result. A failure leaves the draft alone; so does a
/// file that says what the draft said. A file that ended up empty replaces a draft
/// that had text: deleting everything is a thing to want.
fn decide(before: &str, ran: io::Result<bool>, left: io::Result<Vec<u8>>) -> Outcome {
    match ran {
        Err(_) => return Outcome::Failed(Failure::Launch),
        Ok(false) => return Outcome::Failed(Failure::Exit),
        Ok(true) => {}
    }
    let Some(text) = left.ok().and_then(|bytes| String::from_utf8(bytes).ok()) else {
        return Outcome::Failed(Failure::Unreadable);
    };
    let text = normalise(&text);
    if text == normalise(before) {
        Outcome::Unchanged
    } else {
        Outcome::Text(text)
    }
}

/// Unix line endings, no byte order mark, and no blank space at the end (editors end a
/// file with a newline, which is not part of the message). Control characters are left
/// to the composer, which drops them as it takes the text in.
fn normalise(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim_end()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh empty directory for one test, removed at its end.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "matrix-rat-test-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn files(&self) -> usize {
            fs::read_dir(&self.0).unwrap().count()
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// An "editor": `sh -c script`, which gets the file as `$1`.
    fn sh(script: &str) -> Vec<String> {
        ["sh", "-c", script, "sh"].map(str::to_owned).to_vec()
    }

    fn run(text: &str, argv: Vec<String>, dir: &Dir) -> Outcome {
        let request = Request {
            argv,
            text: text.to_owned(),
        };
        let outcome = edit(&request, &dir.0, run_command);
        // Whatever happened, nothing is left behind.
        assert_eq!(dir.files(), 0);
        outcome
    }

    #[test]
    fn the_command_is_split_like_a_shell_would() {
        let words = |visual, editor| command(visual, editor);
        assert_eq!(words(None, Some("vim")), Ok(vec!["vim".to_owned()]));
        assert_eq!(
            words(None, Some("code --wait")),
            Ok(vec!["code".to_owned(), "--wait".to_owned()])
        );
        assert_eq!(
            words(Some("emacsclient -t"), Some("nano")),
            Ok(vec!["emacsclient".to_owned(), "-t".to_owned()])
        );
        assert_eq!(
            words(Some("'my editor' --opt \"a b\" c\\ d"), None),
            Ok(["my editor", "--opt", "a b", "c d"]
                .map(str::to_owned)
                .to_vec())
        );
        // An empty $VISUAL does not hide $EDITOR.
        assert_eq!(words(Some("  "), Some("nano")), Ok(vec!["nano".to_owned()]));
        assert_eq!(words(Some(""), Some("")), Err(Failure::NotSet));
        assert_eq!(words(None, None), Err(Failure::NotSet));
        assert_eq!(words(None, Some("vim 'oops")), Err(Failure::BadCommand));
    }

    #[test]
    fn the_file_is_private_markdown_and_goes_away() {
        let dir = Dir::new("file");
        let file = TempFile::create(&dir.0, "hello\nworld").unwrap();
        let path = file.path().to_owned();
        assert_eq!(path.parent(), Some(dir.0.as_path()));
        assert_eq!(path.extension().unwrap(), "md");
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\nworld");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        // Another file is another file.
        let other = TempFile::create(&dir.0, "").unwrap();
        assert_ne!(other.path(), path);
        drop(file);
        assert!(!path.exists());
        assert!(other.path().exists());
    }

    #[test]
    fn a_missing_directory_is_made_owner_only() {
        let dir = Dir::new("make");
        let inner = dir.0.join("matrix-rat");
        let file = TempFile::create(&inner, "x").unwrap();
        assert!(file.path().starts_with(&inner));
        let mode = fs::metadata(&inner).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
    }

    #[test]
    fn an_unusable_directory_falls_back_to_the_system_one() {
        // A file where the directory should be.
        let dir = Dir::new("fallback");
        let blocker = dir.0.join("blocker");
        fs::write(&blocker, "").unwrap();
        let file = TempFile::create(&blocker.join("sub"), "x").unwrap();
        assert!(file.path().starts_with(std::env::temp_dir()));
        let path = file.path().to_owned();
        let own = path.parent().unwrap().to_owned();
        assert_ne!(
            own,
            std::env::temp_dir(),
            "not in the shared directory itself"
        );
        assert_eq!(
            fs::metadata(&own).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(file);
        assert!(!path.exists());
        assert!(!own.exists());
    }

    #[test]
    fn what_the_editor_adds_comes_back() {
        let dir = Dir::new("append");
        let outcome = run("draft", sh("printf '\\nmore\\n' >> \"$1\""), &dir);
        assert_eq!(outcome, Outcome::Text("draft\nmore".to_owned()));
    }

    #[test]
    fn the_editor_sees_the_draft() {
        let dir = Dir::new("sees");
        // It copies the file it was given over its own: the text is changed to what it saw
        // plus a marker, so the draft must have been in the file.
        let outcome = run(
            "one\ntwo",
            sh("{ cat \"$1\"; printf ' seen'; } > \"$1.new\" && mv \"$1.new\" \"$1\""),
            &dir,
        );
        assert_eq!(outcome, Outcome::Text("one\ntwo seen".to_owned()));
    }

    #[test]
    fn a_failing_editor_changes_nothing() {
        let dir = Dir::new("fail");
        // It wrote something, but it says it failed: that is not taken.
        let outcome = run("draft", sh("printf 'junk' > \"$1\"; exit 1"), &dir);
        assert_eq!(outcome, Outcome::Failed(Failure::Exit));
        let outcome = run("draft", vec!["/nonexistent/editor".to_owned()], &dir);
        assert_eq!(outcome, Outcome::Failed(Failure::Launch));
        let outcome = run("draft", Vec::new(), &dir);
        assert_eq!(outcome, Outcome::Failed(Failure::Launch));
    }

    #[test]
    fn an_emptied_file_deletes_the_draft_but_only_one_that_had_text() {
        let dir = Dir::new("empty");
        let empty = sh(": > \"$1\"");
        assert_eq!(
            run("draft", empty.clone(), &dir),
            Outcome::Text(String::new())
        );
        // Blank space only is the same as nothing.
        assert_eq!(
            run("draft", sh("printf ' \\n\\n' > \"$1\""), &dir),
            Outcome::Text(String::new())
        );
        // Nothing before and nothing after: nothing happened.
        assert_eq!(run("", empty, &dir), Outcome::Unchanged);
    }

    #[test]
    fn the_same_text_is_no_change_whatever_the_editor_does_to_the_end() {
        let dir = Dir::new("same");
        assert_eq!(run("a\nb", sh("true"), &dir), Outcome::Unchanged);
        assert_eq!(
            run("a\nb", sh("printf 'a\\r\\nb\\r\\n\\r\\n' > \"$1\""), &dir),
            Outcome::Unchanged
        );
    }

    #[test]
    fn line_endings_are_unix_and_the_file_must_be_text() {
        let dir = Dir::new("ends");
        assert_eq!(
            run(
                "x",
                sh("printf '\\357\\273\\277a\\r\\nb\\rc\\n' > \"$1\""),
                &dir
            ),
            Outcome::Text("a\nb\nc".to_owned())
        );
        assert_eq!(
            run("x", sh("printf '\\377\\376bad' > \"$1\""), &dir),
            Outcome::Failed(Failure::Unreadable)
        );
        // Removing the file is a way of leaving nothing readable.
        assert_eq!(
            run("x", sh("rm \"$1\""), &dir),
            Outcome::Failed(Failure::Unreadable)
        );
    }

    #[test]
    fn nothing_of_the_draft_is_in_debug_output() {
        let request = Request {
            argv: vec!["vim".into()],
            text: "secret draft".into(),
        };
        assert!(!format!("{request:?}").contains("secret"));
        let outcome = Outcome::Text("secret draft".into());
        assert!(!format!("{outcome:?}").contains("secret"));
    }
}
