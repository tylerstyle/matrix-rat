//! Store passphrase management: optional D-Bus Secret Service (`org.freedesktop.secrets`)
//! via `zbus`, plus safe two-way migration between `session.json` and the keyring.

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use serde::Deserialize;
use zbus::proxy::{Builder, CacheProperties, Proxy};
use zbus::{
    Connection, Error as ZbusError, ObjectPath, OwnedObjectPath, OwnedVariant, Variant, fdo,
};

const SERVICE_DEST: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE_IFACE: &str = "org.freedesktop.Secret.Service";
const COLLECTION_IFACE: &str = "org.freedesktop.Secret.Collection";
const ITEM_IFACE: &str = "org.freedesktop.Secret.Item";
const SESSION_IFACE: &str = "org.freedesktop.Secret.Session";
const PROMPT_IFACE: &str = "org.freedesktop.Secret.Prompt";

const ATTR_APP: &str = "application";
const ATTR_APP_VAL: &str = "matrix-rat";
const ATTR_PROFILE: &str = "profile";
const ATTR_USER_ID: &str = "user_id";
const ATTR_DEVICE_ID: &str = "device_id";

const SECRET_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretStoreMode {
    /// Use the OS secret service when it answers quickly (not over SSH); fall back to the
    /// session file otherwise.
    #[default]
    Auto,
    /// Require the OS secret service; refuse to fall back to storing the passphrase in
    /// `session.json`.
    Keyring,
    /// Store the passphrase directly in `session.json` (`0600`).
    File,
}

/// Identifies one account + device entry in the keyring without carrying secrets.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyringKey {
    pub profile: String,
    pub user_id: String,
    pub device_id: String,
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

    fn owned_attributes(&self) -> HashMap<String, String> {
        HashMap::from([
            (ATTR_APP.to_owned(), ATTR_APP_VAL.to_owned()),
            (ATTR_PROFILE.to_owned(), self.profile.clone()),
            (ATTR_USER_ID.to_owned(), self.user_id.clone()),
            (ATTR_DEVICE_ID.to_owned(), self.device_id.clone()),
        ])
    }

    fn ref_attributes(&self) -> HashMap<&'static str, &str> {
        HashMap::from([
            (ATTR_APP, ATTR_APP_VAL),
            (ATTR_PROFILE, self.profile.as_str()),
            (ATTR_USER_ID, self.user_id.as_str()),
            (ATTR_DEVICE_ID, self.device_id.as_str()),
        ])
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPassphrase {
    pub passphrase: String,
    pub in_keyring: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvePassphraseError {
    KeyringLocked(String),
    KeyringUnavailable(String),
    KeyringFailed(String),
    MissingFromKeyring,
    FileStore(String),
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
    if let Err(err) = file_store.write_passphrase(None) {
        tracing::warn!(%err, "migrated passphrase to keyring, but could not strip session file");
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
        tracing::warn!(%err, "migrated passphrase to session file, but could not remove keyring entry");
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
                            tracing::debug!(%err, "skipping auto migration to keyring; keeping file copy");
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
                        tracing::debug!(%err, "auto keyring save unavailable; using session file");
                    }
                }
            }
            Ok(Some(passphrase.to_owned()))
        }
    }
}

/// Real D-Bus `org.freedesktop.secrets` client backed by `zbus`.
#[derive(Debug, Clone)]
pub struct SecretServiceKeyring {
    timeout: Duration,
    address: Option<String>,
}

impl Default for SecretServiceKeyring {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretServiceKeyring {
    pub fn new() -> Self {
        Self {
            timeout: SECRET_TIMEOUT,
            address: None,
        }
    }

    #[cfg(test)]
    pub fn with_address(address: impl Into<String>) -> Self {
        Self {
            timeout: Duration::from_secs(2),
            address: Some(address.into()),
        }
    }

    async fn connect(&self) -> Result<Connection, KeyringError> {
        if self.address.is_none() && !has_session_bus_hint() {
            return Err(KeyringError::Unavailable("no D-Bus session bus".into()));
        }
        let fut = async {
            match &self.address {
                Some(addr) => zbus::connection::Builder::address(addr.as_str())
                    .build()
                    .await
                    .map_err(|err| KeyringError::Unavailable(err.to_string())),
                None => Connection::session()
                    .await
                    .map_err(|err| KeyringError::Unavailable(err.to_string())),
            }
        };
        tokio::time::timeout(self.timeout, fut).await.map_err(|_| {
            KeyringError::Unavailable("timed out connecting to session D-Bus".into())
        })?
    }

    async fn run_with_session<T, F, Fut>(&self, op: F) -> Result<T, KeyringError>
    where
        F: FnOnce(Connection, Proxy<'static>, OwnedObjectPath) -> Fut,
        Fut: Future<Output = Result<T, KeyringError>>,
    {
        let conn = self.connect().await?;
        let timeout = self.timeout;
        tokio::time::timeout(timeout, async move {
            let service = make_proxy(&conn, SERVICE_PATH, SERVICE_IFACE).await?;
            let reply = service
                .call_method("OpenSession", &("plain", Variant::from("")))
                .await
                .map_err(map_zbus_error)?;
            let (_output, session_path): (OwnedVariant, OwnedObjectPath) =
                reply.body().deserialize().map_err(map_zbus_error)?;

            let result = op(conn.clone(), service, session_path.clone()).await;
            close_session(&conn, &session_path).await;
            result
        })
        .await
        .map_err(|_| KeyringError::Unavailable("timed out waiting for system keyring".into()))?
    }
}

impl KeyringBackend for SecretServiceKeyring {
    fn is_auto_allowed(&self) -> bool {
        if self.address.is_some() {
            return true;
        }
        !crate::portal::in_ssh_session() && has_session_bus_hint()
    }

    async fn read(&self, key: &KeyringKey) -> Result<Option<String>, KeyringError> {
        self.run_with_session(|conn, service, session_path| async move {
            let reply = service
                .call_method("SearchItems", &(key.ref_attributes(),))
                .await
                .map_err(map_zbus_error)?;
            let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) =
                reply.body().deserialize().map_err(map_zbus_error)?;

            if let Some(item_path) = unlocked.into_iter().next() {
                let items = [ObjectPath::from(&item_path)];
                let reply = service
                    .call_method("GetSecrets", &(&items[..], ObjectPath::from(&session_path)))
                    .await
                    .map_err(map_zbus_error)?;
                let mut secrets: HashMap<
                    OwnedObjectPath,
                    (OwnedObjectPath, Vec<u8>, Vec<u8>, String),
                > = reply.body().deserialize().map_err(map_zbus_error)?;
                let Some((_sess, _params, value_bytes, _ctype)) = secrets.remove(&item_path) else {
                    return Ok(None);
                };
                let secret = String::from_utf8(value_bytes)
                    .map_err(|_| KeyringError::Failed("keyring entry is not valid UTF-8".into()))?;
                return Ok(Some(secret));
            }
            if !locked.is_empty() {
                return Err(KeyringError::Locked("default keyring is locked".into()));
            }
            // KeePassXC and strict Secret Service daemons encrypt attributes while locked and
            // return empty SearchItems results rather than locked object paths.
            if let Ok(col) = default_collection(&conn, &service).await
                && is_collection_locked(&conn, &col).await.unwrap_or(false)
            {
                return Err(KeyringError::Locked("default keyring is locked".into()));
            }
            Ok(None)
        })
        .await
    }

    async fn write(&self, key: &KeyringKey, secret: &str) -> Result<(), KeyringError> {
        self.run_with_session(|conn, service, session_path| async move {
            let col_path = default_collection(&conn, &service).await?;
            if is_collection_locked(&conn, &col_path).await? {
                return Err(KeyringError::Locked("default keyring is locked".into()));
            }
            let col_proxy = make_proxy(&conn, col_path.as_str(), COLLECTION_IFACE).await?;
            let attrs = key.owned_attributes();
            let mut props: HashMap<&str, Variant<'_>> = HashMap::new();
            props.insert(
                "org.freedesktop.Secret.Item.Label",
                Variant::from(key.label()),
            );
            props.insert(
                "org.freedesktop.Secret.Item.Attributes",
                Variant::from(attrs),
            );

            let secret_arg = (
                ObjectPath::from(&session_path),
                &[] as &[u8],
                secret.as_bytes(),
                "text/plain",
            );
            let reply = col_proxy
                .call_method("CreateItem", &(props, secret_arg, true))
                .await
                .map_err(map_zbus_error)?;
            let (item, prompt): (OwnedObjectPath, OwnedObjectPath) =
                reply.body().deserialize().map_err(map_zbus_error)?;
            if prompt.as_str() != "/" {
                dismiss_prompt(&conn, &prompt).await;
                return Err(KeyringError::Locked(
                    "keyring requires interactive unlock or confirmation".into(),
                ));
            }
            if item.as_str() == "/" {
                return Err(KeyringError::Failed(
                    "keyring did not return an item path".into(),
                ));
            }
            Ok(())
        })
        .await
    }

    async fn delete(&self, key: &KeyringKey) -> Result<(), KeyringError> {
        self.run_with_session(|conn, service, _session_path| async move {
            let reply = service
                .call_method("SearchItems", &(key.ref_attributes(),))
                .await
                .map_err(map_zbus_error)?;
            let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) =
                reply.body().deserialize().map_err(map_zbus_error)?;
            if unlocked.is_empty() && !locked.is_empty() {
                return Err(KeyringError::Locked("default keyring is locked".into()));
            }
            for item_path in unlocked {
                let item_proxy = make_proxy(&conn, item_path.as_str(), ITEM_IFACE).await?;
                let reply = item_proxy
                    .call_method("Delete", &())
                    .await
                    .map_err(map_zbus_error)?;
                let prompt: OwnedObjectPath = reply.body().deserialize().map_err(map_zbus_error)?;
                if prompt.as_str() != "/" {
                    dismiss_prompt(&conn, &prompt).await;
                    return Err(KeyringError::Locked(
                        "keyring requires unlock to delete entry".into(),
                    ));
                }
            }
            if !locked.is_empty() {
                return Err(KeyringError::Locked(
                    "remaining keyring entry is locked".into(),
                ));
            }
            Ok(())
        })
        .await
    }
}

fn has_session_bus_hint() -> bool {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|v| !v.is_empty()) {
        return true;
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .is_some_and(|dir| std::path::Path::new(&dir).join("bus").exists())
}

async fn make_proxy(
    conn: &Connection,
    path: &str,
    iface: &str,
) -> Result<Proxy<'static>, KeyringError> {
    Builder::<Proxy<'static>>::new(conn)
        .destination(SERVICE_DEST)
        .path(path.to_owned())
        .interface(iface.to_owned())
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(map_zbus_error)
}

async fn default_collection(
    conn: &Connection,
    service: &Proxy<'static>,
) -> Result<OwnedObjectPath, KeyringError> {
    let reply = service
        .call_method("ReadAlias", &("default",))
        .await
        .map_err(map_zbus_error)?;
    let path: OwnedObjectPath = reply.body().deserialize().map_err(map_zbus_error)?;
    if path.as_str() != "/" {
        return Ok(path);
    }
    let collections: Vec<OwnedObjectPath> = service
        .get_property("Collections")
        .await
        .map_err(map_zbus_error)?;
    for col in collections {
        if col.as_str().ends_with("/session") {
            continue;
        }
        if let Ok(col_proxy) = make_proxy(conn, col.as_str(), COLLECTION_IFACE).await
            && col_proxy
                .get_property::<String>("Label")
                .await
                .is_ok_and(|label| label.eq_ignore_ascii_case("session"))
        {
            continue;
        }
        return Ok(col);
    }
    Err(KeyringError::Unavailable(
        "no persistent default collection in system keyring".into(),
    ))
}

async fn is_collection_locked(
    conn: &Connection,
    col_path: &OwnedObjectPath,
) -> Result<bool, KeyringError> {
    let proxy = make_proxy(conn, col_path.as_str(), COLLECTION_IFACE).await?;
    proxy.get_property("Locked").await.map_err(map_zbus_error)
}

async fn close_session(conn: &Connection, session_path: &OwnedObjectPath) {
    if let Ok(proxy) = make_proxy(conn, session_path.as_str(), SESSION_IFACE).await {
        let _ = proxy.call_method("Close", &()).await;
    }
}

async fn dismiss_prompt(conn: &Connection, prompt_path: &OwnedObjectPath) {
    if let Ok(proxy) = make_proxy(conn, prompt_path.as_str(), PROMPT_IFACE).await {
        let _ = proxy.call_method("Dismiss", &()).await;
    }
}

fn map_zbus_error(err: ZbusError) -> KeyringError {
    match &err {
        ZbusError::FDO(fdo_err)
            if matches!(
                &**fdo_err,
                fdo::Error::ServiceUnknown(_)
                    | fdo::Error::NameHasNoOwner(_)
                    | fdo::Error::UnknownObject(_)
                    | fdo::Error::UnknownInterface(_)
                    | fdo::Error::AccessDenied(_)
                    | fdo::Error::SpawnExecFailed(_)
                    | fdo::Error::SpawnChildExited(_)
                    | fdo::Error::SpawnFailed(_)
            ) =>
        {
            KeyringError::Unavailable(fdo_err.to_string())
        }
        ZbusError::MethodError(name, msg, _)
            if name.as_str() == "org.freedesktop.Secret.Error.IsLocked" =>
        {
            KeyringError::Locked(
                msg.clone()
                    .unwrap_or_else(|| "keyring object is locked".into()),
            )
        }
        ZbusError::InputOutput(_) => KeyringError::Unavailable(err.to_string()),
        _ => KeyringError::Failed(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};

    use futures::StreamExt;
    use zbus::names::UniqueName;

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

    struct DbusDaemonGuard {
        child: Child,
        address: String,
    }

    impl Drop for DbusDaemonGuard {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn start_private_dbus_daemon() -> DbusDaemonGuard {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--print-address=1", "--nofork"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("dbus-daemon must be installed to run secret service test");
        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut address = String::new();
        reader.read_line(&mut address).unwrap();
        let address = address.trim().to_owned();
        assert!(!address.is_empty(), "dbus-daemon did not print address");
        DbusDaemonGuard { child, address }
    }

    type StoredSecretItem = (HashMap<String, String>, String, Vec<u8>);
    type WireSecretTuple = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);
    type CreateItemArgs = (HashMap<String, OwnedVariant>, WireSecretTuple, bool);

    #[derive(Default)]
    struct FakeSecretDaemonState {
        locked: bool,
        next_id: usize,
        items: HashMap<String, StoredSecretItem>,
    }

    async fn spawn_fake_secret_service(
        address: &str,
        state: Arc<Mutex<FakeSecretDaemonState>>,
    ) -> tokio::task::JoinHandle<()> {
        let conn = zbus::connection::Builder::address(address)
            .build()
            .await
            .unwrap();
        conn.request_name("org.freedesktop.secrets").await.unwrap();

        tokio::spawn(async move {
            let mut stream = zbus::MessageStream::from(&conn);
            while let Some(Ok(msg)) = stream.next().await {
                if msg.header().message_type() != zbus::message::Type::MethodCall {
                    continue;
                }
                let hdr = msg.header();
                let path = hdr.path().map(|p| p.as_str()).unwrap_or("");
                let iface = hdr.interface().map(|i| i.as_str()).unwrap_or("");
                let member = hdr.member().map(|m| m.as_str()).unwrap_or("");
                let sender_name: Option<UniqueName<'_>> = hdr.sender().cloned();
                let _ = sender_name;

                match (path, iface, member) {
                    ("/org/freedesktop/secrets", SERVICE_IFACE, "OpenSession") => {
                        let (_alg, _input): (String, OwnedVariant) =
                            msg.body().deserialize().unwrap();
                        let sess = OwnedObjectPath::try_from("/org/freedesktop/secrets/session/s1")
                            .unwrap();
                        let _ = conn.reply(&hdr, &(Variant::from(""), sess)).await;
                    }
                    ("/org/freedesktop/secrets", SERVICE_IFACE, "ReadAlias") => {
                        let alias: String = msg.body().deserialize().unwrap();
                        let target = if alias == "default" {
                            "/org/freedesktop/secrets/collection/default"
                        } else {
                            "/"
                        };
                        let path = OwnedObjectPath::try_from(target).unwrap();
                        let _ = conn.reply(&hdr, &path).await;
                    }
                    ("/org/freedesktop/secrets", SERVICE_IFACE, "SearchItems") => {
                        let attrs: HashMap<String, String> = msg.body().deserialize().unwrap();
                        let (unlocked, locked) = {
                            let st = state.lock().unwrap();
                            let mut unlocked = Vec::new();
                            let mut locked = Vec::new();
                            for (item_path, (item_attrs, _, _)) in &st.items {
                                if attrs.iter().all(|(k, v)| item_attrs.get(k) == Some(v)) {
                                    let p = OwnedObjectPath::try_from(item_path.as_str()).unwrap();
                                    if st.locked {
                                        locked.push(p);
                                    } else {
                                        unlocked.push(p);
                                    }
                                }
                            }
                            (unlocked, locked)
                        };
                        let _ = conn.reply(&hdr, &(unlocked, locked)).await;
                    }
                    ("/org/freedesktop/secrets", SERVICE_IFACE, "GetSecrets") => {
                        let (items, session): (Vec<OwnedObjectPath>, OwnedObjectPath) =
                            msg.body().deserialize().unwrap();
                        let res = {
                            let st = state.lock().unwrap();
                            if st.locked {
                                Err(())
                            } else {
                                let mut out: HashMap<
                                    OwnedObjectPath,
                                    (OwnedObjectPath, Vec<u8>, Vec<u8>, String),
                                > = HashMap::new();
                                for item in items {
                                    if let Some((_, ctype, val)) = st.items.get(item.as_str()) {
                                        out.insert(
                                            item,
                                            (
                                                session.clone(),
                                                Vec::new(),
                                                val.clone(),
                                                ctype.clone(),
                                            ),
                                        );
                                    }
                                }
                                Ok(out)
                            }
                        };
                        match res {
                            Err(()) => {
                                let _ = conn
                                    .reply_error(
                                        &hdr,
                                        "org.freedesktop.Secret.Error.IsLocked",
                                        &"collection is locked",
                                    )
                                    .await;
                            }
                            Ok(out) => {
                                let _ = conn.reply(&hdr, &out).await;
                            }
                        }
                    }
                    (
                        "/org/freedesktop/secrets/collection/default",
                        "org.freedesktop.DBus.Properties",
                        "Get",
                    ) => {
                        let (_if, prop): (String, String) = msg.body().deserialize().unwrap();
                        if prop == "Locked" {
                            let locked = state.lock().unwrap().locked;
                            let _ = conn.reply(&hdr, &Variant::from(locked)).await;
                        }
                    }
                    (
                        "/org/freedesktop/secrets/collection/default",
                        COLLECTION_IFACE,
                        "CreateItem",
                    ) => {
                        let (props, secret, replace): CreateItemArgs =
                            msg.body().deserialize().unwrap();
                        let res = {
                            let mut st = state.lock().unwrap();
                            if st.locked {
                                Err(())
                            } else {
                                let attrs = props
                                    .get("org.freedesktop.Secret.Item.Attributes")
                                    .and_then(|v| {
                                        HashMap::<String, String>::try_from(v.clone()).ok()
                                    })
                                    .unwrap_or_default();
                                let existing = st
                                    .items
                                    .iter()
                                    .find(|(_, (a, _, _))| a == &attrs)
                                    .map(|(p, _)| p.clone());
                                let item_path = match (existing, replace) {
                                    (Some(p), true) => p,
                                    _ => {
                                        st.next_id += 1;
                                        format!(
                                            "/org/freedesktop/secrets/collection/default/i{}",
                                            st.next_id
                                        )
                                    }
                                };
                                st.items
                                    .insert(item_path.clone(), (attrs, secret.3, secret.2));
                                Ok((
                                    OwnedObjectPath::try_from(item_path.as_str()).unwrap(),
                                    OwnedObjectPath::try_from("/").unwrap(),
                                ))
                            }
                        };
                        match res {
                            Err(()) => {
                                let _ = conn
                                    .reply_error(
                                        &hdr,
                                        "org.freedesktop.Secret.Error.IsLocked",
                                        &"collection is locked",
                                    )
                                    .await;
                            }
                            Ok(reply) => {
                                let _ = conn.reply(&hdr, &reply).await;
                            }
                        }
                    }
                    (p, ITEM_IFACE, "Delete") if p.starts_with("/org/freedesktop/secrets/") => {
                        let locked = {
                            let mut st = state.lock().unwrap();
                            if st.locked {
                                true
                            } else {
                                st.items.remove(p);
                                false
                            }
                        };
                        if locked {
                            let _ = conn
                                .reply_error(
                                    &hdr,
                                    "org.freedesktop.Secret.Error.IsLocked",
                                    &"item is locked",
                                )
                                .await;
                        } else {
                            let no_prompt = OwnedObjectPath::try_from("/").unwrap();
                            let _ = conn.reply(&hdr, &no_prompt).await;
                        }
                    }
                    (s, SESSION_IFACE, "Close")
                        if s.starts_with("/org/freedesktop/secrets/session/") =>
                    {
                        let _ = conn.reply(&hdr, &()).await;
                    }
                    _ => {
                        let _ = conn
                            .reply_error(
                                &hdr,
                                "org.freedesktop.DBus.Error.UnknownMethod",
                                &format!("unhandled {path} {iface}.{member}"),
                            )
                            .await;
                    }
                }
            }
        })
    }

    #[tokio::test]
    async fn dbus_secret_service_roundtrip_over_private_bus() {
        let daemon = start_private_dbus_daemon();
        let state = Arc::new(Mutex::new(FakeSecretDaemonState::default()));
        let service_task = spawn_fake_secret_service(&daemon.address, state.clone()).await;

        // Wait briefly until well-known name is owned.
        let client = SecretServiceKeyring::with_address(&daemon.address);
        let key = sample_key();

        // Missing initially.
        assert_eq!(client.read(&key).await.unwrap(), None);

        // Write + read back + overwrite.
        write_and_verify_keyring(&client, &key, "pass-v1")
            .await
            .unwrap();
        assert_eq!(client.read(&key).await.unwrap().as_deref(), Some("pass-v1"));

        write_and_verify_keyring(&client, &key, "pass-v2")
            .await
            .unwrap();
        assert_eq!(client.read(&key).await.unwrap().as_deref(), Some("pass-v2"));
        assert_eq!(state.lock().unwrap().items.len(), 1);

        // Different device_id or profile gets distinct entry.
        let other = KeyringKey::new("work", "@alice:example.org", "DEV999");
        assert_eq!(client.read(&other).await.unwrap(), None);

        // Locked keyring returns KeyringError::Locked cleanly on read and write.
        state.lock().unwrap().locked = true;
        assert!(matches!(
            client.read(&key).await,
            Err(KeyringError::Locked(_))
        ));
        assert!(matches!(
            client.write(&key, "pass-v3").await,
            Err(KeyringError::Locked(_))
        ));

        // Unlock and delete cleanly removes the entry.
        state.lock().unwrap().locked = false;
        client.delete(&key).await.unwrap();
        assert_eq!(client.read(&key).await.unwrap(), None);

        service_task.abort();
    }

    #[tokio::test]
    async fn dbus_missing_service_returns_unavailable() {
        let daemon = start_private_dbus_daemon();
        let client = SecretServiceKeyring::with_address(&daemon.address);
        let err = client.read(&sample_key()).await.unwrap_err();
        assert!(matches!(err, KeyringError::Unavailable(_)), "{err:?}");
    }
}
