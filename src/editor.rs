//! Writing a message in an external editor (`$VISUAL`, then `$EDITOR`).
//!
//! The pure parts live here: the request and what to make of what the editor left.
//! Running it is in [`run`]: which command, from the environment, the temporary file
//! and the editor itself. Handing the terminal over and putting the text back into the
//! composer is the app's business (`app::editor`, `terminal`).
//!
//! Nothing about the draft is ever logged or shown in `Debug` output, and neither is the
//! file's name: it is a message the user has not sent yet.

#[cfg(not(target_family = "wasm"))]
mod run;

use std::fmt;
#[cfg(any(not(target_family = "wasm"), test))]
use std::io;

#[cfg(test)]
pub use run::command;
#[cfg(not(target_family = "wasm"))]
pub use run::{command_from_env, edit, run_command};

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

/// What to do with the editor's result. A failure leaves the draft alone; so does a
/// file that says what the draft said. A file that ended up empty replaces a draft
/// that had text: deleting everything is a thing to want.
#[cfg(any(not(target_family = "wasm"), test))]
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
#[cfg(any(not(target_family = "wasm"), test))]
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
