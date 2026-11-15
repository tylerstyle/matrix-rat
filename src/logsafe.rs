//! What the log may say about an error.
//!
//! The text of an error can hold anything: the SDK and the server put room, event and
//! user ids, addresses and their own messages in it, and a system error the path of a
//! file. None of that may reach the log. [`kind`] reduces an error to what helps to find
//! the cause and holds nothing private: an HTTP status and Matrix error code, the kind of
//! a system error, or a fixed word. It goes into a log line like this:
//!
//! `tracing::warn!(why = %logsafe::kind(&err), "could not send typing notice")`
//!
//! ([`kind_any`] for an `anyhow::Error`.)

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use matrix_sdk::ruma::api::error::Error as ServerError;

use crate::matrix::KeyringError;

/// The reduced form of an error, safe to log. Small and `Copy`, so it can be made in
/// the log line itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// The server answered with an error: its HTTP status, and the Matrix error code
    /// if it sent one.
    Http { status: u16, code: Option<Code> },
    /// A system call failed: only its kind, never the file it was about.
    Io(std::io::ErrorKind),
    /// No answer came: the connection could not be made, broke, or took too long.
    Network(&'static str),
    /// A fixed word for the kind of failure, when nothing more precise is known.
    Word(&'static str),
}

/// For a timeout of our own, which is not an error value.
pub const TIMED_OUT: Why = Why::Word("timed out");

/// When nothing in the error is known.
const OTHER: Why = Why::Word("other");

/// Stops the walk along a chain of causes that never ends.
const MAX_DEPTH: usize = 32;

impl Why {
    /// A word only names a family of failures; the others are the cause itself.
    fn is_precise(self) -> bool {
        !matches!(self, Self::Word(_))
    }
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http {
                status,
                code: Some(code),
            } => write!(f, "HTTP {status} {code}"),
            Self::Http { status, code: None } => write!(f, "HTTP {status}"),
            Self::Io(kind) => write!(f, "io {kind:?}"),
            Self::Network(what) => write!(f, "network: {what}"),
            Self::Word(word) => f.write_str(word),
        }
    }
}

/// A Matrix error code (`M_FORBIDDEN`, or a namespaced one like `ORG.EXAMPLE.CODE`),
/// copied so that [`Why`] can stay `Copy`. The server chooses it, so anything that is
/// not shaped like a code is not kept.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Code {
    len: u8,
    bytes: [u8; Code::MAX],
}

impl Code {
    const MAX: usize = 40;

    fn new(code: &str) -> Self {
        let shaped = !code.is_empty()
            && code.len() <= Self::MAX
            && code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.');
        let code = if shaped { code } else { "unusual code" };
        let mut bytes = [0; Self::MAX];
        bytes[..code.len()].copy_from_slice(code.as_bytes());
        Self {
            len: code.len() as u8,
            bytes,
        }
    }

    pub fn as_str(&self) -> &str {
        // Only ASCII is ever copied in.
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or_default()
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

/// Reduces an error to what may be logged. Walks the chain of causes and keeps the
/// most precise thing it recognises; an error of no known type is "other".
pub fn kind(err: &(dyn Error + 'static)) -> Why {
    let mut found = None;
    let mut next = Some(err);
    for _ in 0..MAX_DEPTH {
        let Some(err) = next else {
            break;
        };
        if let Some(why) = recognise(err) {
            if why.is_precise() {
                return why;
            }
            found.get_or_insert(why);
        }
        next = err.source();
    }
    found.unwrap_or(OTHER)
}

/// [`kind`] for an [`anyhow::Error`], whose context strings are left out like any
/// other text.
pub fn kind_any(err: &anyhow::Error) -> Why {
    kind(err.as_ref())
}

/// The error itself, or the same type in a `Box` or an `Arc` (as error types keep
/// their causes).
fn find<'a, T: Error + 'static>(err: &'a (dyn Error + 'static)) -> Option<&'a T> {
    err.downcast_ref::<T>()
        .or_else(|| err.downcast_ref::<Box<T>>().map(|err| &**err))
        .or_else(|| err.downcast_ref::<Arc<T>>().map(|err| &**err))
}

fn recognise(err: &(dyn Error + 'static)) -> Option<Why> {
    if let Some(err) = find::<matrix_sdk::Error>(err) {
        return Some(sdk(err));
    }
    if let Some(err) = find::<matrix_sdk::HttpError>(err) {
        return Some(http(err));
    }
    if let Some(err) = find::<matrix_sdk::reqwest::Error>(err) {
        return Some(network(err));
    }
    if let Some(err) = find::<matrix_sdk_ui::timeline::Error>(err) {
        return Some(timeline(err));
    }
    if let Some(err) = find::<std::io::Error>(err) {
        return Some(Why::Io(err.kind()));
    }
    if find::<serde_json::Error>(err).is_some() {
        return Some(Why::Word("unreadable data"));
    }
    if let Some(err) = find::<tokio::task::JoinError>(err) {
        return Some(Why::Word(if err.is_panic() {
            "task panicked"
        } else {
            "task cancelled"
        }));
    }
    if find::<tokio::time::error::Elapsed>(err).is_some() {
        return Some(TIMED_OUT);
    }
    if let Some(err) = find::<zbus::Error>(err) {
        return Some(dbus(err));
    }
    if let Some(err) = find::<notify::Error>(err) {
        return Some(match &err.kind {
            notify::ErrorKind::Io(err) => Why::Io(err.kind()),
            notify::ErrorKind::PathNotFound => Why::Io(std::io::ErrorKind::NotFound),
            notify::ErrorKind::MaxFilesWatch => Why::Word("too many watched files"),
            _ => Why::Word("file watch"),
        });
    }
    if let Some(err) = find::<KeyringError>(err) {
        return Some(Why::Word(match err {
            KeyringError::Unavailable(_) => "keyring unavailable",
            KeyringError::Locked(_) => "keyring locked",
            KeyringError::Failed(_) => "keyring failed",
        }));
    }
    if let Some(err) = find::<image::ImageError>(err) {
        return Some(match err {
            image::ImageError::IoError(err) => Why::Io(err.kind()),
            _ => Why::Word("image"),
        });
    }
    if find::<toml::de::Error>(err).is_some() || find::<toml_edit::TomlError>(err).is_some() {
        return Some(Why::Word("invalid TOML"));
    }
    None
}

fn sdk(err: &matrix_sdk::Error) -> Why {
    use matrix_sdk::Error as E;
    match err {
        E::Http(err) => http(err),
        E::Io(err) => Why::Io(err.kind()),
        E::Timeout => TIMED_OUT,
        E::SerdeJson(_) => Why::Word("unreadable data"),
        E::AuthenticationRequired => Why::Word("not logged in"),
        E::InsufficientData => Why::Word("not enough local data"),
        E::StateStore(_)
        | E::EventCacheStore(_)
        | E::MediaStore(_)
        | E::CryptoStoreError(_)
        | E::CrossProcessLockError(_) => Why::Word("store"),
        E::OlmError(_)
        | E::MegolmError(_)
        | E::DecryptorError(_)
        | E::SignatureError(_)
        | E::BadCryptoStoreState
        | E::NoOlmMachine => Why::Word("encryption"),
        E::BackupNotEnabled => Why::Word("no key backup"),
        E::Identifier(_) | E::Url(_) | E::UserTagName(_) => Why::Word("invalid input"),
        E::WrongRoomState(_) => Why::Word("wrong room state"),
        E::EventCache(_) => Why::Word("event cache"),
        E::SendQueueWedgeError(_) => Why::Word("send queue"),
        E::Media(_) => Why::Word("media"),
        E::OAuth(_) => Why::Word("OAuth"),
        _ => Why::Word("sdk"),
    }
}

fn http(err: &matrix_sdk::HttpError) -> Why {
    use matrix_sdk::HttpError as H;
    if let Some(err) = err.as_client_api_error() {
        return answer(err);
    }
    if err.as_uiaa_response().is_some() {
        return Why::Word("authentication needed");
    }
    match err {
        H::Reqwest(err) => network(err),
        H::Api(_) => Why::Word("unreadable answer"),
        H::IntoHttp(_) => Why::Word("request not built"),
        H::RefreshToken(_) => Why::Word("token refresh"),
        H::Cached(err) => http(err),
    }
}

/// The server's own words stay out; its status and code say what kind of no it was.
fn answer(err: &ServerError) -> Why {
    Why::Http {
        status: err.status_code.as_u16(),
        code: err
            .error_kind()
            .map(|kind| Code::new(kind.errcode().as_str())),
    }
}

/// The text of a connection error names the address it was for.
fn network(err: &matrix_sdk::reqwest::Error) -> Why {
    if let Some(status) = err.status() {
        return Why::Http {
            status: status.as_u16(),
            code: None,
        };
    }
    Why::Network(if err.is_timeout() {
        "timed out"
    } else if err.is_connect() {
        "no connection"
    } else if err.is_body() || err.is_decode() {
        "broken answer"
    } else {
        "request failed"
    })
}

fn timeline(err: &matrix_sdk_ui::timeline::Error) -> Why {
    use matrix_sdk_ui::timeline::Error as T;
    match err {
        T::Sdk(err) => sdk(err),
        T::EventCacheError(_) => Why::Word("event cache"),
        T::PaginationError(_) => Why::Word("pagination"),
        T::SendQueueError(_) => Why::Word("send queue"),
        _ => Why::Word("timeline"),
    }
}

/// D-Bus errors can carry the bus address and the other side's own words.
fn dbus(err: &zbus::Error) -> Why {
    match err {
        zbus::Error::InputOutput(err) => Why::Io(err.kind()),
        zbus::Error::MethodError(..) | zbus::Error::FDO(_) => Why::Word("D-Bus error reply"),
        zbus::Error::Address(_) | zbus::Error::Handshake(_) => Why::Word("D-Bus connection"),
        _ => Why::Word("D-Bus"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use matrix_sdk::reqwest::StatusCode;
    use matrix_sdk::ruma::api::client::uiaa::UiaaResponse;
    use matrix_sdk::ruma::api::error::{
        ErrorBody, ErrorKind, FromHttpResponseError, StandardErrorBody,
    };

    /// What a log line would show of it, both ways it can be written.
    fn shown(why: Why) -> String {
        format!("{why} {why:?}")
    }

    fn refused(status: StatusCode, kind: ErrorKind, said: &str) -> matrix_sdk::Error {
        let answer =
            ErrorBody::Standard(StandardErrorBody::new(kind, said.to_owned())).into_error(status);
        let http = matrix_sdk::HttpError::from(FromHttpResponseError::Server(
            UiaaResponse::MatrixError(answer),
        ));
        matrix_sdk::Error::from(http)
    }

    #[test]
    fn a_system_error_keeps_its_kind_and_loses_its_path() {
        let err = std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "/home/alice/notes/plans.txt: no such file",
        );
        let why = kind(&err);
        assert_eq!(why, Why::Io(std::io::ErrorKind::NotFound));
        assert_eq!(why.to_string(), "io NotFound");
        let text = shown(why);
        assert!(!text.contains("alice") && !text.contains("plans"), "{text}");
    }

    #[test]
    fn a_chain_keeps_what_it_recognises_and_loses_its_context() {
        let err = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "/home/alice/.local/share/matrix-rat/session.json",
        );
        let err = anyhow::Error::from(err)
            .context("could not save the draft of !secret:example.org")
            .context("for @alice:example.org");
        let why = kind_any(&err);
        assert_eq!(why, Why::Io(std::io::ErrorKind::PermissionDenied));
        let text = shown(why);
        for private in ["alice", "secret", "example.org", "session.json", "draft"] {
            assert!(!text.contains(private), "{private} in {text}");
        }
    }

    #[test]
    fn a_chain_of_nothing_known_is_other() {
        let err: Result<(), _> = Err(anyhow::anyhow!("no room !secret:example.org"));
        let err = err.context("$event:example.org").unwrap_err();
        let why = kind_any(&err);
        assert_eq!(why, OTHER);
        assert_eq!(shown(why), "other Word(\"other\")");
    }

    #[test]
    fn a_refusal_keeps_its_status_and_code_and_loses_the_servers_words() {
        let err = refused(
            StatusCode::FORBIDDEN,
            ErrorKind::Forbidden,
            "@alice:example.org may not see !secret:example.org",
        );
        let why = kind(&err);
        assert_eq!(why.to_string(), "HTTP 403 M_FORBIDDEN");
        let text = shown(why);
        assert!(
            !text.contains("alice") && !text.contains("secret"),
            "{text}"
        );
        // Wrapped by us, the same.
        let err = anyhow::Error::from(err).context("could not join #secret:example.org");
        assert_eq!(kind_any(&err).to_string(), "HTTP 403 M_FORBIDDEN");
    }

    #[test]
    fn an_answer_without_a_code_keeps_its_status() {
        let answer = ErrorBody::Json(serde_json::json!({ "said": "!secret:example.org" }))
            .into_error(StatusCode::BAD_GATEWAY);
        let err = matrix_sdk::HttpError::from(FromHttpResponseError::Server(
            UiaaResponse::MatrixError(answer),
        ));
        let why = kind(&err);
        assert_eq!(why.to_string(), "HTTP 502");
        assert!(!shown(why).contains("secret"));
    }

    #[test]
    fn a_code_of_odd_shape_is_not_kept() {
        assert_eq!(Code::new("M_NOT_FOUND").as_str(), "M_NOT_FOUND");
        assert_eq!(
            Code::new("ORG.EXAMPLE.MSC1234.CODE").as_str(),
            "ORG.EXAMPLE.MSC1234.CODE"
        );
        for odd in [
            "@alice:example.org",
            "!room:example.org",
            "",
            &"M".repeat(41),
        ] {
            assert_eq!(Code::new(odd).as_str(), "unusual code", "{odd}");
        }
    }

    #[test]
    fn an_sdk_error_without_an_answer_is_named_by_a_word() {
        assert_eq!(kind(&matrix_sdk::Error::Timeout), TIMED_OUT);
        assert_eq!(
            kind(&matrix_sdk::Error::BackupNotEnabled).to_string(),
            "no key backup"
        );
        let err = KeyringError::Unavailable("unix:path=/run/user/1000/bus".into());
        let text = shown(kind(&err));
        assert!(
            text.contains("keyring unavailable") && !text.contains("/run"),
            "{text}"
        );
    }
}
