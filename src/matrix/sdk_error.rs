//! matrix-sdk's errors in an [`anyhow::Error`].
//!
//! `anyhow::Error` takes only errors that are `Send` and `Sync`. On this system all of
//! matrix-sdk's are, and [`SdkResult`] is what `?` and `anyhow::Context` were before.
//! In the browser the SDK's catch-all (`matrix_sdk::Error::UnknownError`) holds a
//! `Box<dyn Error>` that is neither, and so is every error that can hold it (the
//! timeline's, the sync service's): there [`SdkResult`] first copies the error into an
//! `CopiedError` (its text, its causes', and what [`crate::logsafe`] makes of it), and that
//! goes into the `anyhow::Error`. What the user is shown and what the log says stay
//! the same.

use std::fmt::Display;

/// `?` and `.context(..)` for a `Result` with one of matrix-sdk's errors.
pub(crate) trait SdkResult<T> {
    /// The error as an [`anyhow::Error`], as `?` makes it.
    fn sdk(self) -> anyhow::Result<T>;
    /// [`anyhow::Context::context`].
    fn sdk_context<C>(self, context: C) -> anyhow::Result<T>
    where
        C: Display + Send + Sync + 'static;
}

#[cfg(not(target_family = "wasm"))]
impl<T, E> SdkResult<T> for Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn sdk(self) -> anyhow::Result<T> {
        self.map_err(anyhow::Error::from)
    }

    fn sdk_context<C>(self, context: C) -> anyhow::Result<T>
    where
        C: Display + Send + Sync + 'static,
    {
        anyhow::Context::context(self, context)
    }
}

#[cfg(target_family = "wasm")]
impl<T, E> SdkResult<T> for Result<T, E>
where
    E: std::error::Error + 'static,
{
    fn sdk(self) -> anyhow::Result<T> {
        self.map_err(|err| anyhow::Error::new(CopiedError::new(&err, 0)))
    }

    fn sdk_context<C>(self, context: C) -> anyhow::Result<T>
    where
        C: Display + Send + Sync + 'static,
    {
        anyhow::Context::context(self.sdk(), context)
    }
}

/// An error copied so that it can be sent: its text, the copies of its causes, and
/// what the log may say of it.
#[cfg(target_family = "wasm")]
pub(crate) struct CopiedError {
    text: String,
    why: crate::logsafe::Why,
    source: Option<Box<CopiedError>>,
}

#[cfg(target_family = "wasm")]
impl CopiedError {
    /// Causes past this many are left out, as [`crate::logsafe`] stops there too.
    const MAX_DEPTH: usize = 32;

    fn new(err: &(dyn std::error::Error + 'static), depth: usize) -> Self {
        Self {
            text: err.to_string(),
            why: crate::logsafe::kind(err),
            source: err
                .source()
                .filter(|_| depth + 1 < Self::MAX_DEPTH)
                .map(|source| Box::new(Self::new(source, depth + 1))),
        }
    }

    /// What [`crate::logsafe::kind`] made of the error it was copied from.
    pub(crate) fn why(&self) -> crate::logsafe::Why {
        self.why
    }
}

#[cfg(target_family = "wasm")]
impl Display for CopiedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// Hand-written: the text can hold ids, addresses and the server's own words.
#[cfg(target_family = "wasm")]
impl std::fmt::Debug for CopiedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CopiedError")
            .field("why", &self.why)
            .finish_non_exhaustive()
    }
}

#[cfg(target_family = "wasm")]
impl std::error::Error for CopiedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}
