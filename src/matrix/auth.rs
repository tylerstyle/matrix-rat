//! What a login is to the worker, whatever keeps it: the session it holds while it
//! runs, how a restore and a renewed login end, and what a soft logout keeps for the
//! next login, and where the store's passphrase is kept. On this system
//! [`super::session`] logs in and keeps the session on disk, and [`super::keyring`]
//! keeps the passphrase; in the browser `browser` stands in for the session until
//! phase 2 logs in there.

use matrix_sdk::Client;
use serde::{Deserialize, Serialize};

#[cfg(not(target_family = "wasm"))]
use super::keyring::KeyringKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretStoreMode {
    /// Use the OS secret service when it answers quickly (not over SSH); fall back to the
    /// session file otherwise.
    Auto,
    /// Require the OS secret service; refuse to fall back to storing the passphrase in
    /// `session.json`.
    Keyring,
    /// Store the passphrase directly in `session.json` (`0600`). The default: a
    /// passphrase that has moved into the keyring is out of reach whenever the app is
    /// started without the desktop session (over SSH, on a console), and the account
    /// then cannot be opened there. Moving it is for the user to decide.
    #[default]
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReauthKind {
    Password,
    Browser,
}

/// Identity and auth kind preserved across a soft logout so logging in again keeps the same
/// device and local encrypted store.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReauthTarget {
    pub homeserver: String,
    pub user_id: String,
    pub device_id: String,
    pub kind: ReauthKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

/// Hand-written: the server, the account and the device stay out of logs.
impl std::fmt::Debug for ReauthTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReauthTarget")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

#[cfg(not(target_family = "wasm"))]
impl ReauthTarget {
    pub fn keyring_key(&self, profile: &str) -> KeyringKey {
        KeyringKey::new(profile, &self.user_id, &self.device_id)
    }
}

/// Live session state kept by the Matrix worker while running.
pub struct ActiveSession {
    pub client: Client,
    pub passphrase: String,
    pub in_keyring: bool,
    pub target: ReauthTarget,
}

pub enum RestoreOutcome {
    /// No `session.json` exists yet.
    NoSession,
    /// Saved session restored cleanly.
    Restored(ActiveSession),
    /// Server soft-logged-out (or saved `soft_logout` marker); prompt re-auth against the
    /// same device and store.
    NeedReauth {
        target: ReauthTarget,
        passphrase: String,
        in_keyring: bool,
    },
    /// Keyring/store cannot be opened safely right now; `session.json` and `<data>/store`
    /// have been kept untouched.
    Blocked(String),
}

pub struct ReauthOutcome {
    pub session: ActiveSession,
    pub kept_device: bool,
}

/// The browser's stand-in for `session`, with the same calls: phase 2 logs in by an
/// OAuth redirect and keeps the stores in IndexedDB. Until then there is no session to
/// restore, none to make and nothing on a disk to keep or remove.
#[cfg(target_family = "wasm")]
pub(super) mod browser {
    use anyhow::{Result, bail};
    use matrix_sdk::Client;
    use tokio::sync::mpsc::UnboundedSender;

    pub use super::{ActiveSession, ReauthKind, ReauthOutcome, ReauthTarget, RestoreOutcome};
    use crate::matrix::{MatrixEvent, SecretStoreMode};
    use crate::paths::Paths;

    const NO_LOGIN: &str = "logging in is not available in the browser yet";

    pub async fn restore(_paths: &Paths, _secret_store: SecretStoreMode) -> RestoreOutcome {
        RestoreOutcome::NoSession
    }

    pub fn mark_soft_logout(_paths: &Paths, _active: &ActiveSession) -> Result<()> {
        Ok(())
    }

    pub async fn forget(_paths: &Paths, _target: Option<&ReauthTarget>) {}

    pub async fn clear(_paths: &Paths, _target: Option<&ReauthTarget>) {}

    pub fn save_tokens(
        _paths: &Paths,
        _client: &Client,
        _passphrase: &str,
        _in_keyring: bool,
    ) -> Result<()> {
        Ok(())
    }

    pub async fn login_password(
        _paths: &Paths,
        _homeserver: &str,
        _username: &str,
        _password: &str,
        _secret_store: SecretStoreMode,
    ) -> Result<ActiveSession> {
        bail!(NO_LOGIN)
    }

    pub async fn reauth_password(
        _paths: &Paths,
        _target: &ReauthTarget,
        _password: &str,
        _existing_passphrase: &str,
        _was_in_keyring: bool,
        _secret_store: SecretStoreMode,
    ) -> Result<ReauthOutcome> {
        bail!(NO_LOGIN)
    }

    pub async fn login_browser(
        _paths: &Paths,
        _homeserver: &str,
        _events: &UnboundedSender<MatrixEvent>,
        _secret_store: SecretStoreMode,
    ) -> Result<ActiveSession> {
        bail!(NO_LOGIN)
    }

    pub async fn reauth_browser(
        _paths: &Paths,
        _target: &ReauthTarget,
        _events: &UnboundedSender<MatrixEvent>,
        _existing_passphrase: &str,
        _was_in_keyring: bool,
        _secret_store: SecretStoreMode,
    ) -> Result<ReauthOutcome> {
        bail!(NO_LOGIN)
    }
}
