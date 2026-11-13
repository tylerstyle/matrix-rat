//! Logging in, and keeping the session on disk between runs.

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::authentication::oauth::registration::{
    ApplicationType, ClientMetadata, Localized, OAuthGrantType,
};
use matrix_sdk::authentication::oauth::{
    ClientId, ClientRegistrationData, OAuthSession, UserSession,
};
use matrix_sdk::encryption::{BackupDownloadStrategy, EncryptionSettings};
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::{OwnedDeviceId, UserId};
use matrix_sdk::utils::local_server::LocalServerBuilder;
use matrix_sdk::{AuthSession, Client};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use url::Url;

use super::MatrixEvent;
use super::keyring::{
    KeyringBackend, KeyringKey, PassphraseFileStore, SecretServiceKeyring, SecretStoreMode,
    prepare_passphrase_for_save, resolve_passphrase,
};
use crate::paths::Paths;

const DEVICE_NAME: &str = "matrix-rat";
/// Shown to the user by the homeserver during a browser login.
const CLIENT_URI: &str = "https://github.com/tylerstyle/matrix-rat";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReauthKind {
    Password,
    Browser,
}

/// Identity and auth kind preserved across a soft logout so logging in again keeps the same
/// device and local encrypted store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReauthTarget {
    pub homeserver: String,
    pub user_id: String,
    pub device_id: String,
    pub kind: ReauthKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReauthDecision {
    KeepStore,
    ReplaceStore { reason: String },
}

/// Pure identity verification for re-authentication: only reuse the existing store when
/// both `user_id` and `device_id` match exactly and `<data>/store` still exists.
pub fn verify_reauth_identity(
    expected_user_id: &str,
    expected_device_id: &str,
    actual_user_id: Option<&str>,
    actual_device_id: Option<&str>,
    store_exists: bool,
) -> ReauthDecision {
    if actual_user_id != Some(expected_user_id) {
        return ReauthDecision::ReplaceStore {
            reason: format!(
                "signed in as a different user ({})",
                actual_user_id.unwrap_or("unknown")
            ),
        };
    }
    if actual_device_id != Some(expected_device_id) {
        return ReauthDecision::ReplaceStore {
            reason: format!(
                "server issued a new device ID ({})",
                actual_device_id.unwrap_or("unknown")
            ),
        };
    }
    if !store_exists {
        return ReauthDecision::ReplaceStore {
            reason: "local store no longer exists".into(),
        };
    }
    ReauthDecision::KeepStore
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// What is written to `session.json` (mode 0600).
/// Intentionally omits `#[derive(Debug)]` so secrets cannot be printed by debug logs.
#[derive(Serialize, Deserialize)]
struct Saved {
    homeserver: String,
    /// Encrypts the local SDK stores when stored on disk; `None` when held in the OS keyring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    passphrase: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    soft_logout: bool,
    auth: SavedAuth,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SavedAuth {
    Password {
        session: MatrixSession,
    },
    Browser {
        client_id: String,
        session: UserSession,
    },
    /// Soft-logged-out state: expired tokens are dropped while keeping identity metadata and
    /// passphrase placement for re-authentication.
    Reauth {
        target: ReauthTarget,
    },
}

impl SavedAuth {
    fn target(&self, homeserver: &str) -> ReauthTarget {
        match self {
            Self::Password { session } => ReauthTarget {
                homeserver: homeserver.to_owned(),
                user_id: session.meta.user_id.to_string(),
                device_id: session.meta.device_id.to_string(),
                kind: ReauthKind::Password,
                client_id: None,
            },
            Self::Browser { client_id, session } => ReauthTarget {
                homeserver: homeserver.to_owned(),
                user_id: session.meta.user_id.to_string(),
                device_id: session.meta.device_id.to_string(),
                kind: ReauthKind::Browser,
                client_id: Some(client_id.clone()),
            },
            Self::Reauth { target } => target.clone(),
        }
    }
}

struct DiskSessionFile<'a>(&'a Path);

impl PassphraseFileStore for DiskSessionFile<'_> {
    fn read_passphrase(&self) -> std::result::Result<Option<String>, String> {
        let json = std::fs::read(self.0).map_err(|err| err.to_string())?;
        let saved: Saved = serde_json::from_slice(&json).map_err(|err| err.to_string())?;
        Ok(saved.passphrase)
    }

    fn write_passphrase(&mut self, passphrase: Option<&str>) -> std::result::Result<(), String> {
        let json = std::fs::read(self.0).map_err(|err| err.to_string())?;
        let mut saved: Saved = serde_json::from_slice(&json).map_err(|err| err.to_string())?;
        saved.passphrase = passphrase.map(str::to_owned);
        write_saved(self.0, &saved).map_err(|err| err.to_string())
    }
}

/// Encryption choices shared by every client. Message keys missing for a message are
/// fetched from the server-side backup when this device can use it (after recovery or
/// verification). Cross-signing is never set up behind the user's back: creating an
/// account's identity is an explicit action.
fn encryption_settings() -> EncryptionSettings {
    EncryptionSettings {
        auto_enable_cross_signing: false,
        backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
        auto_enable_backups: false,
    }
}

/// For authentication before committing to a local store: probes homeserver / runs login in
/// memory so failed attempts or mismatched re-auths never touch `<data>/store`.
async fn discover_memory_client(homeserver: &str) -> Result<Client> {
    Client::builder()
        .server_name_or_homeserver_url(homeserver)
        .with_encryption_settings(encryption_settings())
        .with_threading_support(super::threads::SUPPORT)
        .handle_refresh_tokens()
        .build()
        .await
        .with_context(|| format!("could not reach homeserver {homeserver:?}"))
}

/// For a saved session: uses the known URL and makes no network request, so the app
/// starts from its local stores even when offline.
async fn offline_client(paths: &Paths, homeserver: &str, passphrase: &str) -> Result<Client> {
    Client::builder()
        .homeserver_url(homeserver)
        .sqlite_store(paths.store_dir(), Some(passphrase))
        .with_encryption_settings(encryption_settings())
        .with_threading_support(super::threads::SUPPORT)
        .handle_refresh_tokens()
        .build()
        .await
        .context("could not open the local stores")
}

fn generate_passphrase() -> Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("could not read random bytes")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// A new login is a new device, so it starts from empty stores. The previous stores hold
/// encryption keys that may exist nowhere else, so they are set aside, not deleted.
fn move_aside_store(paths: &Paths) -> Result<()> {
    let store = paths.store_dir();
    if store.exists() {
        let backup = paths.data_dir.join("store.previous");
        let _ = std::fs::remove_dir_all(&backup);
        std::fs::rename(&store, &backup).context("could not set the old stores aside")?;
    }
    let _ = std::fs::remove_file(paths.session_file());
    Ok(())
}

fn read_saved_key(paths: &Paths) -> Option<KeyringKey> {
    let json = std::fs::read(paths.session_file()).ok()?;
    let saved: Saved = serde_json::from_slice(&json).ok()?;
    let target = saved.auth.target(&saved.homeserver);
    Some(target.keyring_key(&paths.profile))
}

/// Removes the saved session, for when the server no longer accepts it. The stores stay
/// until the next login sets them aside.
pub async fn forget(paths: &Paths, target: Option<&ReauthTarget>) {
    forget_with(paths, target, &SecretServiceKeyring::new()).await;
}

pub async fn forget_with<K: KeyringBackend>(
    paths: &Paths,
    target: Option<&ReauthTarget>,
    keyring: &K,
) {
    let key = target
        .map(|t| t.keyring_key(&paths.profile))
        .or_else(|| read_saved_key(paths));
    let _ = std::fs::remove_file(paths.session_file());
    if let Some(key) = key
        && let Err(err) = keyring.delete(&key).await
    {
        tracing::debug!(%err, "could not delete keyring entry while forgetting session");
    }
}

/// Removes everything a session left behind, for an explicit logout.
pub async fn clear(paths: &Paths, target: Option<&ReauthTarget>) {
    clear_with(paths, target, &SecretServiceKeyring::new()).await;
}

pub async fn clear_with<K: KeyringBackend>(
    paths: &Paths,
    target: Option<&ReauthTarget>,
    keyring: &K,
) {
    forget_with(paths, target, keyring).await;
    let _ = std::fs::remove_dir_all(paths.store_dir());
    let _ = std::fs::remove_dir_all(paths.data_dir.join("store.previous"));
}

fn write_saved(path: &Path, saved: &Saved) -> Result<()> {
    let json = serde_json::to_vec_pretty(saved)?;
    // Write a private temp file and rename, so a crash never leaves half a session.
    let temp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("could not write {}", temp.display()))?;
    file.write_all(&json)?;
    file.sync_all()?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

fn client_auth(client: &Client) -> Result<(SavedAuth, AuthSession, ReauthTarget)> {
    let homeserver = client.homeserver().to_string();
    if let Some(session) = client.matrix_auth().session() {
        let auth = SavedAuth::Password {
            session: session.clone(),
        };
        let target = auth.target(&homeserver);
        Ok((auth, AuthSession::Matrix(session), target))
    } else if let Some(session) = client.oauth().full_session() {
        let auth = SavedAuth::Browser {
            client_id: session.client_id.as_str().to_owned(),
            session: session.user.clone(),
        };
        let target = auth.target(&homeserver);
        Ok((auth, AuthSession::OAuth(Box::new(session)), target))
    } else {
        bail!("no session to save");
    }
}

/// Saves refreshed access/refresh tokens during a running session without contacting D-Bus.
pub fn save_tokens(
    paths: &Paths,
    client: &Client,
    passphrase: &str,
    in_keyring: bool,
) -> Result<()> {
    let (auth, _, _) = client_auth(client)?;
    let saved = Saved {
        homeserver: client.homeserver().to_string(),
        passphrase: (!in_keyring).then(|| passphrase.to_owned()),
        soft_logout: false,
        auth,
    };
    write_saved(&paths.session_file(), &saved)
}

/// Records a soft logout on disk: drops expired tokens while preserving the target device,
/// passphrase placement, and `<data>/store`.
pub fn mark_soft_logout(paths: &Paths, active: &ActiveSession) -> Result<()> {
    let saved = Saved {
        homeserver: active.target.homeserver.clone(),
        passphrase: (!active.in_keyring).then(|| active.passphrase.clone()),
        soft_logout: true,
        auth: SavedAuth::Reauth {
            target: active.target.clone(),
        },
    };
    write_saved(&paths.session_file(), &saved)
}

/// Restores the saved session, if there is one.
pub async fn restore(paths: &Paths, secret_store: SecretStoreMode) -> RestoreOutcome {
    restore_with(paths, secret_store, &SecretServiceKeyring::new()).await
}

pub async fn restore_with<K: KeyringBackend>(
    paths: &Paths,
    secret_store: SecretStoreMode,
    keyring: &K,
) -> RestoreOutcome {
    let path = paths.session_file();
    if !path.exists() {
        return RestoreOutcome::NoSession;
    }
    let json = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            return RestoreOutcome::Blocked(format!(
                "Could not read saved session ({err}); your local store was kept intact."
            ));
        }
    };
    let saved: Saved = match serde_json::from_slice(&json) {
        Ok(s) => s,
        Err(_) => {
            return RestoreOutcome::Blocked(
                "The saved session file is damaged; your local store was kept intact. Log in to replace it."
                    .into(),
            );
        }
    };

    let target = saved.auth.target(&saved.homeserver);
    let key = target.keyring_key(&paths.profile);
    let has_store = paths.store_dir().exists();
    let mut disk_file = DiskSessionFile(&path);

    let resolved = match resolve_passphrase(
        secret_store,
        saved.passphrase.as_deref(),
        &key,
        keyring,
        &mut disk_file,
    )
    .await
    {
        Ok(ok) => ok,
        Err(err) => return RestoreOutcome::Blocked(err.user_message(has_store)),
    };

    if !has_store {
        return RestoreOutcome::Blocked(
            "Saved session exists, but the local encrypted store is missing. Please log in again."
                .into(),
        );
    }

    if saved.soft_logout || matches!(saved.auth, SavedAuth::Reauth { .. }) {
        return RestoreOutcome::NeedReauth {
            target,
            passphrase: resolved.passphrase,
            in_keyring: resolved.in_keyring,
        };
    }

    let client = match offline_client(paths, &saved.homeserver, &resolved.passphrase).await {
        Ok(client) => client,
        Err(err) => {
            return RestoreOutcome::Blocked(format!(
                "Could not open local encrypted store: {err:#}"
            ));
        }
    };
    let session = match saved.auth {
        SavedAuth::Password { session } => AuthSession::Matrix(session),
        SavedAuth::Browser { client_id, session } => AuthSession::OAuth(Box::new(OAuthSession {
            client_id: ClientId::new(client_id),
            user: session,
        })),
        SavedAuth::Reauth { .. } => unreachable!("handled above"),
    };
    if let Err(err) = client.restore_session(session).await {
        return RestoreOutcome::Blocked(format!(
            "Could not restore your session: {err:#}. Your local store was kept intact."
        ));
    }

    RestoreOutcome::Restored(ActiveSession {
        client,
        passphrase: resolved.passphrase,
        in_keyring: resolved.in_keyring,
        target,
    })
}

async fn commit_fresh_session<K: KeyringBackend>(
    paths: &Paths,
    mem_client: Client,
    secret_store: SecretStoreMode,
    keyring: &K,
) -> Result<ActiveSession> {
    let homeserver = mem_client.homeserver().to_string();
    let (auth, auth_session, target) = client_auth(&mem_client)?;
    drop(mem_client);

    let new_passphrase = generate_passphrase()?;
    let new_key = target.keyring_key(&paths.profile);
    // Prepare the passphrase before touching any existing store so keyring failures never
    // lose or rotate `<data>/store`.
    let file_passphrase =
        prepare_passphrase_for_save(secret_store, keyring, &new_key, &new_passphrase)
            .await
            .map_err(|err| anyhow!("could not store passphrase in keyring: {err}"))?;
    let in_keyring = file_passphrase.is_none();

    let old_key = read_saved_key(paths);
    if let Err(err) = move_aside_store(paths) {
        if in_keyring {
            let _ = keyring.delete(&new_key).await;
        }
        return Err(err);
    }
    if let Some(old_key) = old_key
        && old_key != new_key
    {
        let _ = keyring.delete(&old_key).await;
    }

    let result: Result<ActiveSession> = async {
        let client = offline_client(paths, &homeserver, &new_passphrase).await?;
        client.restore_session(auth_session).await?;
        let saved = Saved {
            homeserver,
            passphrase: file_passphrase,
            soft_logout: false,
            auth,
        };
        write_saved(&paths.session_file(), &saved)?;
        Ok(ActiveSession {
            client,
            passphrase: new_passphrase,
            in_keyring,
            target,
        })
    }
    .await;

    if result.is_err() && in_keyring {
        let _ = keyring.delete(&new_key).await;
    }
    result
}

async fn commit_preserved_reauth<K: KeyringBackend>(
    paths: &Paths,
    mem_client: Client,
    existing_passphrase: &str,
    was_in_keyring: bool,
    secret_store: SecretStoreMode,
    keyring: &K,
) -> Result<ActiveSession> {
    let homeserver = mem_client.homeserver().to_string();
    let (auth, auth_session, target) = client_auth(&mem_client)?;
    drop(mem_client);

    let key = target.keyring_key(&paths.profile);
    let file_passphrase =
        prepare_passphrase_for_save(secret_store, keyring, &key, existing_passphrase)
            .await
            .map_err(|err| anyhow!("could not store passphrase in keyring: {err}"))?;
    let in_keyring = file_passphrase.is_none();

    let client = offline_client(paths, &homeserver, existing_passphrase).await?;
    client.restore_session(auth_session).await?;
    let saved = Saved {
        homeserver,
        passphrase: file_passphrase,
        soft_logout: false,
        auth,
    };
    write_saved(&paths.session_file(), &saved)?;

    if was_in_keyring && !in_keyring {
        let _ = keyring.delete(&key).await;
    }

    Ok(ActiveSession {
        client,
        passphrase: existing_passphrase.to_owned(),
        in_keyring,
        target,
    })
}

async fn finish_reauth<K: KeyringBackend>(
    paths: &Paths,
    mem_client: Client,
    expected: &ReauthTarget,
    existing_passphrase: &str,
    was_in_keyring: bool,
    secret_store: SecretStoreMode,
    keyring: &K,
) -> Result<ReauthOutcome> {
    let actual_user = mem_client.user_id().map(|u| u.to_string());
    let actual_device = mem_client.device_id().map(|d| d.to_string());
    match verify_reauth_identity(
        &expected.user_id,
        &expected.device_id,
        actual_user.as_deref(),
        actual_device.as_deref(),
        paths.store_dir().exists(),
    ) {
        ReauthDecision::KeepStore => {
            let session = commit_preserved_reauth(
                paths,
                mem_client,
                existing_passphrase,
                was_in_keyring,
                secret_store,
                keyring,
            )
            .await?;
            Ok(ReauthOutcome {
                session,
                kept_device: true,
            })
        }
        ReauthDecision::ReplaceStore { reason } => {
            tracing::warn!(%reason, "re-authentication cannot reuse the existing store; starting fresh");
            let session = commit_fresh_session(paths, mem_client, secret_store, keyring).await?;
            Ok(ReauthOutcome {
                session,
                kept_device: false,
            })
        }
    }
}

pub async fn login_password(
    paths: &Paths,
    homeserver: &str,
    username: &str,
    password: &str,
    secret_store: SecretStoreMode,
) -> Result<ActiveSession> {
    if homeserver.trim().is_empty() || username.trim().is_empty() || password.is_empty() {
        bail!("homeserver, username and password are all needed");
    }
    let client = discover_memory_client(homeserver.trim()).await?;
    client
        .matrix_auth()
        .login_username(username.trim(), password)
        .initial_device_display_name(DEVICE_NAME)
        .request_refresh_token()
        .send()
        .await
        .map_err(|err| anyhow!("login was refused: {err}"))?;
    commit_fresh_session(paths, client, secret_store, &SecretServiceKeyring::new()).await
}

pub async fn reauth_password(
    paths: &Paths,
    target: &ReauthTarget,
    password: &str,
    existing_passphrase: &str,
    was_in_keyring: bool,
    secret_store: SecretStoreMode,
) -> Result<ReauthOutcome> {
    if password.is_empty() {
        bail!("a password is needed");
    }
    let client = discover_memory_client(&target.homeserver).await?;
    client
        .matrix_auth()
        .login_username(&target.user_id, password)
        .device_id(&target.device_id)
        .initial_device_display_name(DEVICE_NAME)
        .request_refresh_token()
        .send()
        .await
        .map_err(|err| anyhow!("login was refused: {err}"))?;
    finish_reauth(
        paths,
        client,
        target,
        existing_passphrase,
        was_in_keyring,
        secret_store,
        &SecretServiceKeyring::new(),
    )
    .await
}

async fn run_oauth_login(
    client: &Client,
    events: &UnboundedSender<MatrixEvent>,
    reauth: Option<&ReauthTarget>,
) -> Result<()> {
    client
        .oauth()
        .server_metadata()
        .await
        .map_err(|_| anyhow!("this homeserver does not offer browser login; use a password"))?;

    let (redirect_uri, redirect) = LocalServerBuilder::new()
        .spawn()
        .await
        .context("could not start the local redirect server")?;
    let mut metadata = ClientMetadata::new(
        ApplicationType::Native,
        vec![OAuthGrantType::AuthorizationCode {
            redirect_uris: vec![redirect_uri.clone()],
        }],
        Localized::new(Url::parse(CLIENT_URI)?, []),
    );
    metadata.client_name = Some(Localized::new(DEVICE_NAME.to_owned(), []));
    let registration = ClientRegistrationData::new(Raw::new(&metadata)?);

    let oauth = client.oauth();
    if let Some(target) = reauth
        && let Some(client_id) = &target.client_id
    {
        oauth.restore_registered_client(ClientId::new(client_id.clone()));
    }
    let device_id = reauth.map(|target| OwnedDeviceId::from(target.device_id.as_str()));
    let mut builder = oauth.login(redirect_uri, device_id, Some(registration), None);
    if let Some(target) = reauth
        && let Ok(uid) = UserId::parse(&target.user_id)
    {
        builder = builder.user_id_hint(&uid);
    }
    let authorization = builder
        .build()
        .await
        .map_err(|err| anyhow!("could not start browser login: {err}"))?;
    let _ = events.send(MatrixEvent::BrowserLoginUrl(authorization.url.to_string()));

    let query = redirect.await;
    let query = query.ok_or_else(|| anyhow!("the browser did not complete the login"))?;
    oauth
        .finish_login(query.into())
        .await
        .map_err(|err| anyhow!("login was refused: {err}"))?;
    Ok(())
}

/// OAuth 2.0 login: the user approves in a browser, which then redirects to a
/// temporary web server on this machine. Dropping the future cancels the login and
/// closes that server.
pub async fn login_browser(
    paths: &Paths,
    homeserver: &str,
    events: &UnboundedSender<MatrixEvent>,
    secret_store: SecretStoreMode,
) -> Result<ActiveSession> {
    if homeserver.trim().is_empty() {
        bail!("a homeserver is needed");
    }
    let client = discover_memory_client(homeserver.trim()).await?;
    run_oauth_login(&client, events, None).await?;
    commit_fresh_session(paths, client, secret_store, &SecretServiceKeyring::new()).await
}

pub async fn reauth_browser(
    paths: &Paths,
    target: &ReauthTarget,
    events: &UnboundedSender<MatrixEvent>,
    existing_passphrase: &str,
    was_in_keyring: bool,
    secret_store: SecretStoreMode,
) -> Result<ReauthOutcome> {
    let client = discover_memory_client(&target.homeserver).await?;
    run_oauth_login(&client, events, Some(target)).await?;
    finish_reauth(
        paths,
        client,
        target,
        existing_passphrase,
        was_in_keyring,
        secret_store,
        &SecretServiceKeyring::new(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::matrix::keyring::KeyringError;

    #[derive(Default)]
    struct TestKeyring {
        auto_allowed: bool,
        locked: bool,
        unavailable: bool,
        entries: Mutex<HashMap<KeyringKey, String>>,
    }

    impl KeyringBackend for TestKeyring {
        fn is_auto_allowed(&self) -> bool {
            self.auto_allowed
        }

        async fn read(
            &self,
            key: &KeyringKey,
        ) -> std::result::Result<Option<String>, KeyringError> {
            if self.unavailable {
                return Err(KeyringError::Unavailable("bus unavailable".into()));
            }
            if self.locked {
                return Err(KeyringError::Locked("keyring locked".into()));
            }
            Ok(self.entries.lock().unwrap().get(key).cloned())
        }

        async fn write(
            &self,
            key: &KeyringKey,
            secret: &str,
        ) -> std::result::Result<(), KeyringError> {
            if self.unavailable {
                return Err(KeyringError::Unavailable("bus unavailable".into()));
            }
            if self.locked {
                return Err(KeyringError::Locked("keyring locked".into()));
            }
            self.entries
                .lock()
                .unwrap()
                .insert(key.clone(), secret.to_owned());
            Ok(())
        }

        async fn delete(&self, key: &KeyringKey) -> std::result::Result<(), KeyringError> {
            if self.unavailable {
                return Err(KeyringError::Unavailable("bus unavailable".into()));
            }
            if self.locked {
                return Err(KeyringError::Locked("keyring locked".into()));
            }
            self.entries.lock().unwrap().remove(key);
            Ok(())
        }
    }

    fn temp_paths(tag: &str) -> Paths {
        let root =
            std::env::temp_dir().join(format!("matrix-rat-sess-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "test".into(),
        };
        paths.create_dirs().unwrap();
        paths
    }

    #[test]
    fn reauth_identity_decisions() {
        assert_eq!(
            verify_reauth_identity("@alice:hs", "DEV1", Some("@alice:hs"), Some("DEV1"), true),
            ReauthDecision::KeepStore
        );
        assert!(matches!(
            verify_reauth_identity("@alice:hs", "DEV1", Some("@bob:hs"), Some("DEV1"), true),
            ReauthDecision::ReplaceStore { .. }
        ));
        assert!(matches!(
            verify_reauth_identity("@alice:hs", "DEV1", Some("@alice:hs"), Some("DEV2"), true),
            ReauthDecision::ReplaceStore { .. }
        ));
        assert!(matches!(
            verify_reauth_identity("@alice:hs", "DEV1", Some("@alice:hs"), Some("DEV1"), false),
            ReauthDecision::ReplaceStore { .. }
        ));
        assert!(matches!(
            verify_reauth_identity("@alice:hs", "DEV1", Some("@alice:hs"), None, true),
            ReauthDecision::ReplaceStore { .. }
        ));
    }

    #[test]
    fn legacy_and_reauth_session_json_roundtrip() {
        use matrix_sdk::SessionMeta;
        use matrix_sdk::authentication::SessionTokens;

        let legacy_auth = SavedAuth::Password {
            session: MatrixSession {
                meta: SessionMeta {
                    user_id: "@alice:example.org".try_into().unwrap(),
                    device_id: "DEV1".into(),
                },
                tokens: SessionTokens {
                    access_token: "tok_secret".into(),
                    refresh_token: None,
                },
            },
        };
        let legacy_full = Saved {
            homeserver: "https://example.org".into(),
            passphrase: Some("legacy-secret-passphrase".into()),
            soft_logout: false,
            auth: legacy_auth,
        };
        // Legacy files on disk had no `soft_logout` key at all: verify deserialization without it.
        let mut legacy_val = serde_json::to_value(&legacy_full).unwrap();
        legacy_val.as_object_mut().unwrap().remove("soft_logout");
        let parsed: Saved = serde_json::from_value(legacy_val).unwrap();
        assert!(!parsed.soft_logout);
        assert_eq!(
            parsed.passphrase.as_deref(),
            Some("legacy-secret-passphrase")
        );
        let target = parsed.auth.target(&parsed.homeserver);
        assert_eq!(target.user_id, "@alice:example.org");
        assert_eq!(target.device_id, "DEV1");
        assert_eq!(target.kind, ReauthKind::Password);

        // Soft-logout state drops tokens and omits passphrase when in keyring.
        let soft = Saved {
            homeserver: "https://example.org".into(),
            passphrase: None,
            soft_logout: true,
            auth: SavedAuth::Reauth { target },
        };
        let encoded = serde_json::to_string(&soft).unwrap();
        assert!(!encoded.contains("passphrase"));
        assert!(!encoded.contains("tok_secret"));
        let decoded: Saved = serde_json::from_str(&encoded).unwrap();
        assert!(decoded.soft_logout);
        assert_eq!(decoded.passphrase, None);
    }

    #[tokio::test]
    async fn restore_migrates_safely_and_preserves_store_when_keyring_locked() {
        let paths = temp_paths("restore-soft-locked");
        std::fs::create_dir_all(paths.store_dir()).unwrap();
        let store_marker = paths.store_dir().join("marker");
        std::fs::write(&store_marker, b"keep-me").unwrap();

        let target = ReauthTarget {
            homeserver: "http://127.0.0.1:6167".into(),
            user_id: "@alice:localhost".into(),
            device_id: "DEV1".into(),
            kind: ReauthKind::Password,
            client_id: None,
        };
        let saved = Saved {
            homeserver: target.homeserver.clone(),
            passphrase: Some("pass-123".into()),
            soft_logout: true,
            auth: SavedAuth::Reauth {
                target: target.clone(),
            },
        };
        write_saved(&paths.session_file(), &saved).unwrap();

        // 1. Restore with Auto migrates passphrase from session.json -> keyring, and returns NeedReauth.
        let mut keyring = TestKeyring {
            auto_allowed: true,
            ..TestKeyring::default()
        };
        match restore_with(&paths, SecretStoreMode::Auto, &keyring).await {
            RestoreOutcome::NeedReauth {
                target: got,
                passphrase,
                in_keyring,
            } => {
                assert_eq!(got, target);
                assert_eq!(passphrase, "pass-123");
                assert!(in_keyring);
            }
            _ => panic!("expected NeedReauth"),
        }
        // Passphrase is no longer on disk, now lives in keyring.
        let on_disk: Saved =
            serde_json::from_slice(&std::fs::read(paths.session_file()).unwrap()).unwrap();
        assert_eq!(on_disk.passphrase, None);

        // 2. Next start when keyring is locked MUST NOT wipe session.json or store!
        keyring.locked = true;
        match restore_with(&paths, SecretStoreMode::Auto, &keyring).await {
            RestoreOutcome::Blocked(msg) => {
                assert!(msg.contains("locked"), "{msg}");
                assert!(msg.contains("kept intact"), "{msg}");
            }
            _ => panic!("expected Blocked when keyring locked"),
        }
        assert!(paths.session_file().exists());
        assert_eq!(std::fs::read(&store_marker).unwrap(), b"keep-me");

        // 3. Explicit forget deletes session.json and keyring entry cleanly after unlocking.
        keyring.locked = false;
        forget_with(&paths, Some(&target), &keyring).await;
        assert!(!paths.session_file().exists());
        assert!(
            !keyring
                .entries
                .lock()
                .unwrap()
                .contains_key(&target.keyring_key(&paths.profile))
        );

        let _ = std::fs::remove_dir_all(paths.data_dir.parent().unwrap());
    }
}
