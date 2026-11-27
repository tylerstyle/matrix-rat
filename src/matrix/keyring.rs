//! Store passphrase management: optional D-Bus Secret Service (`org.freedesktop.secrets`)
//! via `zbus`, plus safe two-way migration between `session.json` and the keyring.
//!
//! The modes, the errors and the migration over any [`KeyringBackend`] are here; the
//! Secret Service client itself is in [`secret_service`].

mod secret_service;

use std::future::Future;

use serde::Deserialize;

pub use secret_service::SecretServiceKeyring;

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

/// Identifies one account + device entry in the keyring without carrying secrets.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct KeyringKey {
    pub profile: String,
    pub user_id: String,
    pub device_id: String,
}

/// Hand-written: the account and the device stay out of logs.
impl std::fmt::Debug for KeyringKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyringKey").finish_non_exhaustive()
    }
}

impl KeyringKey {
    pub fn new(
        profile: impl Into<String>,
        user_id: impl Into<String>,
        device_id: impl Into<String>,
    ) -> Self {
        Self {
            profile: profile.into(),
            user_id: user_id.into(),
            device_id: device_id.into(),
        }
    }

    pub fn label(&self) -> String {
        format!("matrix-rat ({}) {}", self.profile, self.user_id)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum KeyringError {
    /// No session bus, SSH session (for auto), service absent, or timed out.
    Unavailable(String),
    /// Secret Service responded, but the default collection or matching entry is locked.
    Locked(String),
    /// Protocol or verification failure (never contains secret material).
    Failed(String),
}

impl std::fmt::Display for KeyringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(msg) | Self::Locked(msg) | Self::Failed(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for KeyringError {}

/// Hand-written: the message is the keyring's or the bus's own words, which can name
/// the bus address; only the variant is printed.
impl std::fmt::Debug for KeyringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unavailable(_) => "KeyringError::Unavailable",
            Self::Locked(_) => "KeyringError::Locked",
            Self::Failed(_) => "KeyringError::Failed",
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedPassphrase {
    pub passphrase: String,
    pub in_keyring: bool,
}

/// Hand-written: never prints the passphrase.
impl std::fmt::Debug for ResolvedPassphrase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedPassphrase")
            .field("in_keyring", &self.in_keyring)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum ResolvePassphraseError {
    KeyringLocked(String),
    KeyringUnavailable(String),
    KeyringFailed(String),
    MissingFromKeyring,
    FileStore(String),
}

/// Hand-written: the messages are error text from the keyring or the file system.
impl std::fmt::Debug for ResolvePassphraseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::KeyringLocked(_) => "KeyringLocked",
            Self::KeyringUnavailable(_) => "KeyringUnavailable",
            Self::KeyringFailed(_) => "KeyringFailed",
            Self::MissingFromKeyring => "MissingFromKeyring",
            Self::FileStore(_) => "FileStore",
        })
    }
}

impl ResolvePassphraseError {
    /// User-facing explanation suitable for the login screen; reassures when the encrypted
    /// store on disk has not been touched.
    pub fn user_message(&self, has_store: bool) -> String {
        let store_note = if has_store {
            " Your local encrypted store has been kept intact."
        } else {
            ""
        };
        let ssh_hint = if crate::portal::in_ssh_session() {
            " (SSH session)"
        } else {
            ""
        };
        match self {
            Self::KeyringLocked(detail) => format!(
                "System keyring is locked{ssh_hint}: {detail}.{store_note} Unlock it and restart, or log in as a new session."
            ),
            Self::KeyringUnavailable(detail) => format!(
                "System keyring is unavailable{ssh_hint}: {detail}.{store_note} Restore keyring access and restart, set secret_store = \"file\", or log in as a new session."
            ),
            Self::KeyringFailed(detail) => {
                format!("Could not read passphrase from system keyring: {detail}.{store_note}")
            }
            Self::MissingFromKeyring => format!(
                "Saved session requires its passphrase from the system keyring, but no entry was found.{store_note} Restore the keyring entry or log in as a new session."
            ),
            Self::FileStore(detail) => {
                format!("Could not update session file: {detail}.{store_note}")
            }
        }
    }
}

impl std::fmt::Display for ResolvePassphraseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.user_message(false))
    }
}

impl std::error::Error for ResolvePassphraseError {}

pub trait KeyringBackend: Send + Sync {
    fn is_auto_allowed(&self) -> bool;
    fn read(
        &self,
        key: &KeyringKey,
    ) -> impl Future<Output = Result<Option<String>, KeyringError>> + Send;
    fn write(
        &self,
        key: &KeyringKey,
        secret: &str,
    ) -> impl Future<Output = Result<(), KeyringError>> + Send;
    fn delete(&self, key: &KeyringKey) -> impl Future<Output = Result<(), KeyringError>> + Send;
}

pub trait PassphraseFileStore {
    fn read_passphrase(&self) -> Result<Option<String>, String>;
    fn write_passphrase(&mut self, passphrase: Option<&str>) -> Result<(), String>;
}

/// Writes `secret` to `keyring` and verifies by reading it back before returning `Ok(())`.
pub async fn write_and_verify_keyring<K: KeyringBackend>(
    keyring: &K,
    key: &KeyringKey,
    secret: &str,
) -> Result<(), KeyringError> {
    keyring.write(key, secret).await?;
    let read_back = keyring.read(key).await?;
    if read_back.as_deref() != Some(secret) {
        return Err(KeyringError::Failed(
            "keyring read-back verification did not match".into(),
        ));
    }
    Ok(())
}

/// Safe `file -> keyring` migration: write to keyring, read back and verify exact match,
/// only then strip the passphrase from `session.json`.
async fn migrate_file_to_keyring<K: KeyringBackend, F: PassphraseFileStore>(
    keyring: &K,
    file_store: &mut F,
    key: &KeyringKey,
    passphrase: &str,
) -> Result<bool, KeyringError> {
    write_and_verify_keyring(keyring, key, passphrase).await?;
    // The reason is text, which can quote the session file: only the fact is logged.
    if file_store.write_passphrase(None).is_err() {
        tracing::warn!("migrated passphrase to keyring, but could not strip session file");
        return Ok(false);
    }
    Ok(true)
}

/// Safe `keyring -> file` migration: write passphrase into `session.json`, read back and
/// verify exact match, only then delete the keyring entry.
async fn migrate_keyring_to_file<K: KeyringBackend, F: PassphraseFileStore>(
    keyring: &K,
    file_store: &mut F,
    key: &KeyringKey,
    passphrase: &str,
) -> Result<(), String> {
    file_store.write_passphrase(Some(passphrase))?;
    let read_back = file_store.read_passphrase()?;
    if read_back.as_deref() != Some(passphrase) {
        return Err("session file read-back verification did not match".into());
    }
    if let Err(err) = keyring.delete(key).await {
        tracing::warn!(
            why = %crate::logsafe::kind(&err),
            "migrated passphrase to session file, but could not remove keyring entry"
        );
    }
    Ok(())
}

/// Resolves the passphrase for a saved session on startup and performs any configured safe
/// two-way migration without ever leaving `<data>/store` without a readable passphrase.
pub async fn resolve_passphrase<K: KeyringBackend, F: PassphraseFileStore>(
    mode: SecretStoreMode,
    file_passphrase: Option<&str>,
    key: &KeyringKey,
    keyring: &K,
    file_store: &mut F,
) -> Result<ResolvedPassphrase, ResolvePassphraseError> {
    match file_passphrase {
        Some(passphrase) => match mode {
            SecretStoreMode::File => Ok(ResolvedPassphrase {
                passphrase: passphrase.to_owned(),
                in_keyring: false,
            }),
            SecretStoreMode::Auto => {
                if keyring.is_auto_allowed() {
                    match migrate_file_to_keyring(keyring, file_store, key, passphrase).await {
                        Ok(stripped) => {
                            return Ok(ResolvedPassphrase {
                                passphrase: passphrase.to_owned(),
                                in_keyring: stripped,
                            });
                        }
                        Err(err) => {
                            tracing::debug!(
                                why = %crate::logsafe::kind(&err),
                                "skipping auto migration to keyring; keeping file copy"
                            );
                        }
                    }
                }
                Ok(ResolvedPassphrase {
                    passphrase: passphrase.to_owned(),
                    in_keyring: false,
                })
            }
            SecretStoreMode::Keyring => {
                match migrate_file_to_keyring(keyring, file_store, key, passphrase).await {
                    Ok(stripped) => Ok(ResolvedPassphrase {
                        passphrase: passphrase.to_owned(),
                        in_keyring: stripped,
                    }),
                    Err(KeyringError::Locked(msg)) => {
                        Err(ResolvePassphraseError::KeyringLocked(msg))
                    }
                    Err(KeyringError::Unavailable(msg)) => {
                        Err(ResolvePassphraseError::KeyringUnavailable(msg))
                    }
                    Err(KeyringError::Failed(msg)) => {
                        Err(ResolvePassphraseError::KeyringFailed(msg))
                    }
                }
            }
        },
        None => {
            let passphrase = match keyring.read(key).await {
                Ok(Some(secret)) => secret,
                Ok(None) => return Err(ResolvePassphraseError::MissingFromKeyring),
                Err(KeyringError::Locked(msg)) => {
                    return Err(ResolvePassphraseError::KeyringLocked(msg));
                }
                Err(KeyringError::Unavailable(msg)) => {
                    return Err(ResolvePassphraseError::KeyringUnavailable(msg));
                }
                Err(KeyringError::Failed(msg)) => {
                    return Err(ResolvePassphraseError::KeyringFailed(msg));
                }
            };
            if mode == SecretStoreMode::File {
                migrate_keyring_to_file(keyring, file_store, key, &passphrase)
                    .await
                    .map_err(ResolvePassphraseError::FileStore)?;
                Ok(ResolvedPassphrase {
                    passphrase,
                    in_keyring: false,
                })
            } else {
                Ok(ResolvedPassphrase {
                    passphrase,
                    in_keyring: true,
                })
            }
        }
    }
}

/// Stores the passphrase for a login (or updates storage when preserving a store) according to
/// `mode`. Returns `Some(passphrase)` if it should be stored inside `session.json`, or `None`
/// if it has been verified inside the keyring.
pub async fn prepare_passphrase_for_save<K: KeyringBackend>(
    mode: SecretStoreMode,
    keyring: &K,
    key: &KeyringKey,
    passphrase: &str,
) -> Result<Option<String>, KeyringError> {
    match mode {
        SecretStoreMode::File => Ok(Some(passphrase.to_owned())),
        SecretStoreMode::Keyring => {
            write_and_verify_keyring(keyring, key, passphrase).await?;
            Ok(None)
        }
        SecretStoreMode::Auto => {
            if keyring.is_auto_allowed() {
                match write_and_verify_keyring(keyring, key, passphrase).await {
                    Ok(()) => return Ok(None),
                    Err(err) => {
                        tracing::debug!(
                            why = %crate::logsafe::kind(&err),
                            "auto keyring save unavailable; using session file"
                        );
                    }
                }
            }
            Ok(Some(passphrase.to_owned()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct MockKeyring {
        auto_allowed: bool,
        entries: Mutex<HashMap<KeyringKey, String>>,
        fail_read: Option<KeyringError>,
        fail_write: Option<KeyringError>,
        fail_delete: Option<KeyringError>,
        corrupt_readback: Option<String>,
    }

    impl MockKeyring {
        fn new(auto_allowed: bool) -> Self {
            Self {
                auto_allowed,
                ..Self::default()
            }
        }
    }

    impl KeyringBackend for MockKeyring {
        fn is_auto_allowed(&self) -> bool {
            self.auto_allowed
        }

        async fn read(&self, key: &KeyringKey) -> Result<Option<String>, KeyringError> {
            if let Some(err) = &self.fail_read {
                return Err(err.clone());
            }
            if let Some(corrupt) = &self.corrupt_readback {
                return Ok(Some(corrupt.clone()));
            }
            Ok(self.entries.lock().unwrap().get(key).cloned())
        }

        async fn write(&self, key: &KeyringKey, secret: &str) -> Result<(), KeyringError> {
            if let Some(err) = &self.fail_write {
                return Err(err.clone());
            }
            self.entries
                .lock()
                .unwrap()
                .insert(key.clone(), secret.to_owned());
            Ok(())
        }

        async fn delete(&self, key: &KeyringKey) -> Result<(), KeyringError> {
            if let Some(err) = &self.fail_delete {
                return Err(err.clone());
            }
            self.entries.lock().unwrap().remove(key);
            Ok(())
        }
    }

    #[derive(Default)]
    struct MockFileStore {
        passphrase: Option<String>,
        fail_read: bool,
        fail_write: bool,
        corrupt_on_write: Option<String>,
    }

    impl PassphraseFileStore for MockFileStore {
        fn read_passphrase(&self) -> Result<Option<String>, String> {
            if self.fail_read {
                return Err("disk read failed".into());
            }
            Ok(self.passphrase.clone())
        }

        fn write_passphrase(&mut self, passphrase: Option<&str>) -> Result<(), String> {
            if self.fail_write {
                return Err("disk write failed".into());
            }
            self.passphrase = self
                .corrupt_on_write
                .clone()
                .or_else(|| passphrase.map(str::to_owned));
            Ok(())
        }
    }

    fn sample_key() -> KeyringKey {
        KeyringKey::new("default", "@alice:example.org", "DEV123")
    }

    #[tokio::test]
    async fn migrate_file_to_keyring_and_back_succeeds() {
        let key = sample_key();
        let keyring = MockKeyring::new(true);
        let mut file = MockFileStore {
            passphrase: Some("super-secret-pass".into()),
            ..MockFileStore::default()
        };

        // Auto migrates file -> keyring and strips file copy after verification.
        let resolved = resolve_passphrase(
            SecretStoreMode::Auto,
            Some("super-secret-pass"),
            &key,
            &keyring,
            &mut file,
        )
        .await
        .unwrap();
        assert_eq!(resolved.passphrase, "super-secret-pass");
        assert!(resolved.in_keyring);
        assert_eq!(file.passphrase, None);
        assert_eq!(
            keyring
                .entries
                .lock()
                .unwrap()
                .get(&key)
                .map(String::as_str),
            Some("super-secret-pass")
        );

        // Switching to File migrates keyring -> file, verifies, and deletes keyring entry.
        let resolved2 = resolve_passphrase(SecretStoreMode::File, None, &key, &keyring, &mut file)
            .await
            .unwrap();
        assert_eq!(resolved2.passphrase, "super-secret-pass");
        assert!(!resolved2.in_keyring);
        assert_eq!(file.passphrase.as_deref(), Some("super-secret-pass"));
        assert!(!keyring.entries.lock().unwrap().contains_key(&key));
    }

    #[tokio::test]
    async fn migrate_file_to_keyring_crash_points_never_lose_passphrase() {
        let key = sample_key();

        // Write fails: file copy kept intact in Auto and Keyring.
        let mut keyring = MockKeyring::new(true);
        keyring.fail_write = Some(KeyringError::Unavailable("bus down".into()));
        let mut file = MockFileStore {
            passphrase: Some("secret-1".into()),
            ..MockFileStore::default()
        };
        let auto_ok = resolve_passphrase(
            SecretStoreMode::Auto,
            Some("secret-1"),
            &key,
            &keyring,
            &mut file,
        )
        .await
        .unwrap();
        assert!(!auto_ok.in_keyring);
        assert_eq!(file.passphrase.as_deref(), Some("secret-1"));
        let req_err = resolve_passphrase(
            SecretStoreMode::Keyring,
            Some("secret-1"),
            &key,
            &keyring,
            &mut file,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            req_err,
            ResolvePassphraseError::KeyringUnavailable(_)
        ));
        assert_eq!(file.passphrase.as_deref(), Some("secret-1"));

        // Read-back mismatch after write: file copy MUST NOT be stripped.
        let mut keyring = MockKeyring::new(true);
        keyring.corrupt_readback = Some("wrong-value".into());
        let mut file = MockFileStore {
            passphrase: Some("secret-1".into()),
            ..MockFileStore::default()
        };
        let err = resolve_passphrase(
            SecretStoreMode::Keyring,
            Some("secret-1"),
            &key,
            &keyring,
            &mut file,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ResolvePassphraseError::KeyringFailed(_)));
        assert_eq!(file.passphrase.as_deref(), Some("secret-1"));

        // Crash right before file strip (simulated: next run sees file still has passphrase while
        // keyring already holds it): idempotent second run finishes cleanly.
        let keyring = MockKeyring::new(true);
        keyring
            .entries
            .lock()
            .unwrap()
            .insert(key.clone(), "secret-1".into());
        let mut file = MockFileStore {
            passphrase: Some("secret-1".into()),
            ..MockFileStore::default()
        };
        let recovered = resolve_passphrase(
            SecretStoreMode::Keyring,
            Some("secret-1"),
            &key,
            &keyring,
            &mut file,
        )
        .await
        .unwrap();
        assert!(recovered.in_keyring);
        assert_eq!(file.passphrase, None);
    }

    #[tokio::test]
    async fn migrate_keyring_to_file_crash_points_never_delete_keyring_early() {
        let key = sample_key();

        // Disk write failure: keyring entry MUST remain untouched.
        let keyring = MockKeyring::new(true);
        keyring
            .entries
            .lock()
            .unwrap()
            .insert(key.clone(), "secret-2".into());
        let mut file = MockFileStore {
            fail_write: true,
            ..MockFileStore::default()
        };
        let err = resolve_passphrase(SecretStoreMode::File, None, &key, &keyring, &mut file)
            .await
            .unwrap_err();
        assert!(matches!(err, ResolvePassphraseError::FileStore(_)));
        assert_eq!(
            keyring
                .entries
                .lock()
                .unwrap()
                .get(&key)
                .map(String::as_str),
            Some("secret-2")
        );

        // Disk read-back mismatch: keyring entry MUST NOT be deleted.
        let mut file = MockFileStore {
            corrupt_on_write: Some("corrupt".into()),
            ..MockFileStore::default()
        };
        let err = resolve_passphrase(SecretStoreMode::File, None, &key, &keyring, &mut file)
            .await
            .unwrap_err();
        assert!(matches!(err, ResolvePassphraseError::FileStore(_)));
        assert_eq!(
            keyring
                .entries
                .lock()
                .unwrap()
                .get(&key)
                .map(String::as_str),
            Some("secret-2")
        );

        // Keyring delete failure after verified disk write: succeeds using verified file copy.
        let mut keyring_del_fail = MockKeyring::new(true);
        keyring_del_fail
            .entries
            .lock()
            .unwrap()
            .insert(key.clone(), "secret-2".into());
        keyring_del_fail.fail_delete = Some(KeyringError::Unavailable("vanished".into()));
        let mut file = MockFileStore::default();
        let resolved = resolve_passphrase(
            SecretStoreMode::File,
            None,
            &key,
            &keyring_del_fail,
            &mut file,
        )
        .await
        .unwrap();
        assert_eq!(resolved.passphrase, "secret-2");
        assert!(!resolved.in_keyring);
        assert_eq!(file.passphrase.as_deref(), Some("secret-2"));
    }

    #[tokio::test]
    async fn missing_or_locked_keyring_reports_error_when_session_needs_keyring() {
        let key = sample_key();
        let mut keyring = MockKeyring::new(false); // even when auto is disallowed (e.g. SSH)
        keyring.fail_read = Some(KeyringError::Unavailable("no bus".into()));
        let mut file = MockFileStore::default();
        let err = resolve_passphrase(SecretStoreMode::Auto, None, &key, &keyring, &mut file)
            .await
            .unwrap_err();
        assert!(matches!(err, ResolvePassphraseError::KeyringUnavailable(_)));
        assert!(
            err.user_message(true)
                .contains("encrypted store has been kept intact")
        );

        keyring.fail_read = Some(KeyringError::Locked("locked".into()));
        let err = resolve_passphrase(SecretStoreMode::Auto, None, &key, &keyring, &mut file)
            .await
            .unwrap_err();
        assert!(matches!(err, ResolvePassphraseError::KeyringLocked(_)));

        let keyring_empty = MockKeyring::new(true);
        let err = resolve_passphrase(SecretStoreMode::Auto, None, &key, &keyring_empty, &mut file)
            .await
            .unwrap_err();
        assert!(matches!(err, ResolvePassphraseError::MissingFromKeyring));
    }
}
