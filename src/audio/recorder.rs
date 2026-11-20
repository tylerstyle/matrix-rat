//! Which programs record a voice message. (The choice among those the system has is
//! added with the composer's microphone; this is what the recording itself needs.)
// Until the composer's microphone uses it (the next piece of the voice messages).
#![cfg_attr(not(test), allow(dead_code))]

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

/// A program with its arguments, started without a shell.
#[derive(Clone, PartialEq, Eq)]
pub struct Tool {
    program: PathBuf,
    args: Vec<OsString>,
}

impl Tool {
    pub fn new(program: PathBuf, args: Vec<OsString>) -> Self {
        Self { program, args }
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn args(&self) -> &[OsString] {
        &self.args
    }
}

/// Hand-written: a command of the user's can name their own files.
impl fmt::Debug for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Tool")
    }
}
