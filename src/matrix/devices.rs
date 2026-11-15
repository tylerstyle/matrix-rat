//! This account's sessions (devices), the key backup as the server has it, and room
//! keys to and from a file.
//!
//! The worker owns every SDK object here. The UI sees plain data: the list of sessions
//! while its popup is open, and the outcome of what it asked for.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::future::pending;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::StreamExt;
use matrix_sdk::config::RequestConfig;
use matrix_sdk::encryption::backups::UploadState;
use matrix_sdk::encryption::{KeyExportError, RoomKeyImportError};
use matrix_sdk::ruma::api::client::backup::get_latest_backup_info;
use matrix_sdk::ruma::api::client::device::delete_devices;
use matrix_sdk::ruma::api::client::discovery::get_authorization_server_metadata::v1::{
    AccountManagementActionData, DeviceDeleteData,
};
use matrix_sdk::ruma::api::client::uiaa::{
    AuthData, AuthType, MatrixUserIdentifier, Password, UiaaInfo, UserIdentifier,
};
use matrix_sdk::ruma::api::error::ErrorKind;
use matrix_sdk::ruma::{DeviceId, OwnedDeviceId};
use matrix_sdk::{AuthApi, Client, HttpError};
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::{JoinHandle, JoinSet};
use zeroize::Zeroizing;

use super::MatrixEvent;

// ----- what the UI sees ---------------------------------------------------------------

/// Whether the account's cross-signing identity vouches for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTrust {
    /// Signed by the account's cross-signing identity.
    Verified,
    /// Has encryption keys, but they are not signed.
    Unverified,
    /// The server lists the session, but it has published no encryption keys: it
    /// cannot be verified (and cannot read encrypted messages).
    NoKeys,
}

/// One login of this account, as the server lists it.
#[derive(Clone, PartialEq, Eq)]
pub struct Session {
    pub device_id: String,
    /// The name its owner (or its program) gave it. Anyone with access to the
    /// account can set it: it proves nothing.
    pub name: Option<String>,
    /// When the server last heard from it, in milliseconds since 1970.
    pub last_seen: Option<u64>,
    pub last_ip: Option<String>,
    /// The session this program runs in.
    pub current: bool,
    pub trust: SessionTrust,
}

/// Never prints the name or the address.
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Session(current: {}, {:?})", self.current, self.trust)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SessionList {
    /// This session first, then the others, most recently seen first.
    pub sessions: Vec<Session>,
    /// This device trusts the account's cross-signing identity, so "verified" above
    /// was checked here. Without, it only repeats what the server says.
    pub vouched: bool,
}

impl std::fmt::Debug for SessionList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SessionList({} sessions, vouched: {})",
            self.sessions.len(),
            self.vouched
        )
    }
}

/// The backup of message keys, as the server has it (whether or not this device can
/// use it).
#[derive(Clone, PartialEq, Eq, Default)]
pub enum ServerBackup {
    /// Not asked yet, or the server could not be reached: nothing is known.
    #[default]
    Unknown,
    Missing,
    Present {
        /// How many message keys it holds.
        keys: u64,
        /// Which backup it is. (Which one this device uploads to cannot be compared
        /// with it: matrix-sdk 0.19.1 keeps its backup version to itself.)
        version: String,
    },
}

/// Which backup it is and how much it holds says something about the account: not
/// for a log.
impl std::fmt::Debug for ServerBackup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unknown => "Unknown",
            Self::Missing => "Missing",
            Self::Present { .. } => "Present",
        })
    }
}

/// What this device's uploads to the backup are doing, as far as was seen while the
/// popup was open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackupUpload {
    /// Nothing was reported yet.
    #[default]
    Unknown,
    /// Nothing waits to be uploaded.
    Idle,
    Uploading {
        done: usize,
        total: usize,
    },
    /// The last upload failed; the SDK tries again later.
    Failed,
}

pub enum DevicesCmd {
    /// The sessions popup (or the key backup popup) opened (with a number, which the
    /// news for it quotes) or closed. While open, [`DevicesEvent::List`] and
    /// [`DevicesEvent::Backup`] are sent, at once and on every change.
    Watch(Option<u64>),
    Rename {
        request: u64,
        device_id: String,
        name: String,
    },
    /// Sign out another session. Tried without a password first; servers then say
    /// what they want.
    SignOut {
        request: u64,
        device_id: String,
        password: Option<Zeroizing<String>>,
    },
    /// Write all message keys of this device to a new file, encrypted with the
    /// passphrase.
    ExportKeys {
        request: u64,
        path: PathBuf,
        passphrase: Zeroizing<String>,
    },
    ImportKeys {
        request: u64,
        path: PathBuf,
        passphrase: Zeroizing<String>,
    },
}

/// Never prints a password, a passphrase, a path or a name.
impl std::fmt::Debug for DevicesCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Watch(watch) => write!(f, "Watch({watch:?})"),
            Self::Rename { request, .. } => write!(f, "Rename({request})"),
            Self::SignOut {
                request, password, ..
            } => write!(f, "SignOut({request}, password: {})", password.is_some()),
            Self::ExportKeys { request, .. } => write!(f, "ExportKeys({request})"),
            Self::ImportKeys { request, .. } => write!(f, "ImportKeys({request})"),
        }
    }
}

/// How signing out another session went.
#[derive(Clone, PartialEq, Eq)]
pub enum SignOutOutcome {
    Done,
    /// The server wants the account password.
    NeedsPassword,
    /// The password given was not accepted. Nothing was tried again.
    WrongPassword,
    /// The server takes no password for this (single sign-on, or an account managed
    /// by an authentication service). `url` is its page for ending that session, if
    /// it names one.
    Web {
        url: Option<String>,
    },
    Failed(String),
}

impl std::fmt::Debug for SignOutOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Done => f.write_str("Done"),
            Self::NeedsPassword => f.write_str("NeedsPassword"),
            Self::WrongPassword => f.write_str("WrongPassword"),
            Self::Web { url } => write!(f, "Web(url: {})", url.is_some()),
            // The reason can name a session or a server.
            Self::Failed(_) => f.write_str("Failed"),
        }
    }
}

pub enum DevicesEvent {
    List {
        /// The [`DevicesCmd::Watch`] this is news for.
        watch: u64,
        list: Result<SessionList, String>,
    },
    Backup {
        watch: u64,
        server: ServerBackup,
        upload: BackupUpload,
    },
    Renamed {
        request: u64,
        result: Result<(), String>,
    },
    SignedOut {
        request: u64,
        outcome: SignOutOutcome,
    },
    /// How many keys were written, and where.
    Exported {
        request: u64,
        result: Result<(usize, PathBuf), String>,
    },
    /// How many keys were new to this device, and how many the file held.
    Imported {
        request: u64,
        result: Result<(usize, usize), String>,
    },
}

/// Never prints a path or a name.
impl std::fmt::Debug for DevicesEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::List {
                watch,
                list: Ok(list),
            } => write!(f, "List({watch}, {list:?})"),
            Self::List { watch, .. } => write!(f, "List({watch}, failed)"),
            Self::Backup {
                watch,
                server,
                upload,
            } => {
                let server = match server {
                    ServerBackup::Unknown => "unknown",
                    ServerBackup::Missing => "missing",
                    ServerBackup::Present { .. } => "present",
                };
                write!(f, "Backup({watch}, {server}, {upload:?})")
            }
            Self::Renamed { request, result } => {
                write!(f, "Renamed({request}, ok: {})", result.is_ok())
            }
            Self::SignedOut { request, outcome } => write!(f, "SignedOut({request}, {outcome:?})"),
            Self::Exported { request, result } => {
                write!(f, "Exported({request}, ok: {})", result.is_ok())
            }
            Self::Imported { request, result } => {
                write!(f, "Imported({request}, ok: {})", result.is_ok())
            }
        }
    }
}

// ----- the worker side ----------------------------------------------------------------

/// While the popup is open the list is also fetched this often: nothing tells when a
/// session was last seen, and a session that was removed elsewhere is only noticed
/// at once when the SDK reports it by itself (see [`watch`]).
const POLL: Duration = Duration::from_secs(30);

/// Lives as long as a session.
pub struct Devices {
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    watcher: Option<JoinHandle<()>>,
    /// Asks the watcher to look again.
    refresh: Arc<Notify>,
    tasks: JoinSet<()>,
    imported_tx: UnboundedSender<()>,
    imported_rx: UnboundedReceiver<()>,
}

impl Devices {
    pub fn new(client: Client, events: UnboundedSender<MatrixEvent>) -> Self {
        let (imported_tx, imported_rx) = unbounded_channel();
        Self {
            client,
            events,
            watcher: None,
            refresh: Arc::new(Notify::new()),
            tasks: JoinSet::new(),
            imported_tx,
            imported_rx,
        }
    }

    /// Resolves each time keys were imported from a file: what is on screen may be
    /// readable now.
    pub async fn next_imported(&mut self) {
        if self.imported_rx.recv().await.is_none() {
            pending::<()>().await;
        }
    }

    pub fn handle(&mut self, command: DevicesCmd) {
        while self.tasks.try_join_next().is_some() {}
        let client = self.client.clone();
        let events = self.events.clone();
        let refresh = self.refresh.clone();
        let send = move |event| {
            let _ = events.send(MatrixEvent::Devices(event));
        };
        match command {
            DevicesCmd::Watch(watch_id) => {
                if let Some(old) = self.watcher.take() {
                    old.abort();
                }
                if let Some(id) = watch_id {
                    self.watcher = Some(tokio::spawn(watch(
                        client,
                        self.events.clone(),
                        refresh,
                        id,
                    )));
                }
            }
            DevicesCmd::Rename {
                request,
                device_id,
                name,
            } => {
                self.tasks.spawn(async move {
                    let device: OwnedDeviceId = device_id.into();
                    let result = client
                        .rename_device(&device, name.trim())
                        .await
                        .map(|_| ())
                        .map_err(|err| format!("Could not rename the session: {err}"));
                    // (What went wrong is told to the user; its text can name
                    // sessions and servers and stays out of the log.)
                    if result.is_err() {
                        tracing::warn!("a session could not be renamed");
                    }
                    send(DevicesEvent::Renamed { request, result });
                    refresh.notify_one();
                });
            }
            DevicesCmd::SignOut {
                request,
                device_id,
                password,
            } => {
                self.tasks.spawn(async move {
                    let outcome = sign_out(&client, &device_id, password).await;
                    if matches!(outcome, SignOutOutcome::Failed(_)) {
                        tracing::warn!("a session could not be signed out");
                    }
                    if outcome == SignOutOutcome::Done {
                        // The crypto store forgets the device when the keys are
                        // fetched again.
                        if let Some(user) = client.user_id() {
                            let _ = client.encryption().request_user_identity(user).await;
                        }
                    }
                    send(DevicesEvent::SignedOut { request, outcome });
                    refresh.notify_one();
                });
            }
            DevicesCmd::ExportKeys {
                request,
                path,
                passphrase,
            } => {
                self.tasks.spawn(async move {
                    let result = export_keys(&client, path, passphrase).await;
                    // (The reason names the file; it is shown, not logged.)
                    if result.is_err() {
                        tracing::warn!("the message keys could not be exported");
                    }
                    send(DevicesEvent::Exported { request, result });
                });
            }
            DevicesCmd::ImportKeys {
                request,
                path,
                passphrase,
            } => {
                let imported = self.imported_tx.clone();
                self.tasks.spawn(async move {
                    let result = import_keys(&client, path, passphrase).await;
                    match &result {
                        Ok(_) => {
                            let _ = imported.send(());
                        }
                        Err(_) => tracing::warn!("the message keys could not be imported"),
                    }
                    send(DevicesEvent::Imported { request, result });
                    // A backup takes the new keys up.
                    refresh.notify_one();
                });
            }
        }
    }

    pub fn stop(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
        }
        self.tasks.abort_all();
    }
}

impl Drop for Devices {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Sends the list of sessions and the state of the backup, now and whenever they may
/// have changed, until it is aborted. Everything it sends carries `id`.
///
/// What makes it look again: news about this account's devices from the SDK; a
/// change of the backup's state; a request from this module after it changed
/// something; and the clock ([`POLL`]). The SDK's device stream has no entry for a
/// device that was removed: a removal shows there, if at all, as an update that
/// names no device (`DeviceChanges::deleted` is not passed on), so such an update
/// makes it look too. A removal that arrives together with other people's device
/// news is not recognisable and waits for the clock.
async fn watch(
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    refresh: Arc<Notify>,
    id: u64,
) {
    let Some(user_id) = client.user_id().map(ToOwned::to_owned) else {
        return;
    };
    let encryption = client.encryption();
    let backups = encryption.backups();
    let mut devices = encryption.devices_stream().await.ok().map(Box::pin);
    let mut backup_states = Box::pin(backups.state_stream());
    let mut uploads = Box::pin(backups.wait_for_steady_state().subscribe_to_progress());
    // Devices without keys whose keys were asked for: once is enough per opening.
    let mut asked = HashSet::new();
    let mut last_list = None;
    let mut last_backup = None;
    let mut server = ServerBackup::Unknown;
    let mut upload = BackupUpload::Unknown;
    let mut fetch = true;
    loop {
        if fetch {
            let now = list(&client, &mut asked).await;
            if last_list.as_ref() != Some(&now) {
                let _ = events.send(MatrixEvent::Devices(DevicesEvent::List {
                    watch: id,
                    list: now.clone(),
                }));
                last_list = Some(now);
            }
            server = server_backup(&client).await;
        }
        let now = (server.clone(), upload);
        if last_backup.as_ref() != Some(&now) {
            let _ = events.send(MatrixEvent::Devices(DevicesEvent::Backup {
                watch: id,
                server: server.clone(),
                upload,
            }));
            last_backup = Some(now);
        }
        let poll = tokio::time::sleep(POLL);
        tokio::pin!(poll);
        fetch = loop {
            tokio::select! {
                Some(updates) = async {
                    match devices.as_mut() {
                        Some(stream) => stream.next().await,
                        None => pending().await,
                    }
                } => {
                    let ours = updates.new.contains_key(&user_id)
                        || updates.changed.contains_key(&user_id);
                    let maybe_removal = updates.new.is_empty() && updates.changed.is_empty();
                    if ours || maybe_removal {
                        break true;
                    }
                }
                Some(_) = backup_states.next() => break true,
                Some(state) = uploads.next() => {
                    let now = match state {
                        Ok(UploadState::Idle | UploadState::Done) => BackupUpload::Idle,
                        Ok(UploadState::Uploading(counts)) => BackupUpload::Uploading {
                            done: counts.backed_up,
                            total: counts.total,
                        },
                        Ok(UploadState::Error) => BackupUpload::Failed,
                        // Reports were missed; the next one tells.
                        Err(_) => upload,
                    };
                    if now != upload {
                        // Finished uploads change the server's count; the steps on
                        // the way there are only shown.
                        let finished = now == BackupUpload::Idle;
                        upload = now;
                        break finished;
                    }
                }
                () = refresh.notified() => break true,
                () = &mut poll => break true,
            }
        };
        if fetch {
            // Changes tend to come in groups.
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

/// The account's sessions as the server lists them, each with what the crypto store
/// knows about its keys. `asked` remembers the devices whose keys were asked for
/// because the store had none: each is asked about once, so that a session that
/// simply has no keys (a bot, a bridge, an old web login) costs one request, not one
/// per look.
async fn list(client: &Client, asked: &mut HashSet<OwnedDeviceId>) -> Result<SessionList, String> {
    let (Some(user_id), Some(own)) = (client.user_id(), client.device_id()) else {
        return Err("Not logged in".to_owned());
    };
    let response = client
        .devices()
        .await
        .map_err(|err| format!("Could not load the sessions: {err}"))?;
    let encryption = client.encryption();
    let mut known = encryption.get_user_devices(user_id).await.ok();
    let keyless: Vec<OwnedDeviceId> = response
        .devices
        .iter()
        .filter(|device| {
            known
                .as_ref()
                .is_none_or(|known| known.get(&device.device_id).is_none())
        })
        .map(|device| device.device_id.clone())
        .collect();
    if keyless.iter().any(|device| !asked.contains(device)) {
        // The store hears of a new device with a later sync; ask for the keys now, so
        // that a session is not shown as having none while they are on their way.
        if let Err(err) = encryption.request_user_identity(user_id).await {
            tracing::debug!(kind = ?err.client_api_error_kind(), "could not fetch our own keys");
        }
        known = encryption.get_user_devices(user_id).await.ok();
    }
    asked.extend(keyless);
    let vouched = matches!(
        encryption.get_user_identity(user_id).await,
        Ok(Some(identity)) if identity.is_verified()
    );
    let mut sessions: Vec<Session> = response
        .devices
        .into_iter()
        .filter_map(|device| {
            let keys = known
                .as_ref()
                .and_then(|known| known.get(&device.device_id));
            // Not a login: a place where messages wait for a device to come.
            if keys.as_ref().is_some_and(|keys| keys.is_dehydrated()) {
                return None;
            }
            let trust = match &keys {
                Some(keys) if keys.is_cross_signed_by_owner() => SessionTrust::Verified,
                Some(_) => SessionTrust::Unverified,
                None => SessionTrust::NoKeys,
            };
            Some(Session {
                current: device.device_id == own,
                device_id: device.device_id.to_string(),
                name: device.display_name.filter(|name| !name.trim().is_empty()),
                last_seen: device.last_seen_ts.map(|at| u64::from(at.get())),
                last_ip: device.last_seen_ip.filter(|ip| !ip.trim().is_empty()),
                trust,
            })
        })
        .collect();
    sessions.sort_by(|a, b| {
        b.current
            .cmp(&a.current)
            .then(b.last_seen.cmp(&a.last_seen))
            .then(a.device_id.cmp(&b.device_id))
    });
    Ok(SessionList { sessions, vouched })
}

async fn server_backup(client: &Client) -> ServerBackup {
    match client
        .send(get_latest_backup_info::v3::Request::new())
        .await
    {
        Ok(info) => ServerBackup::Present {
            keys: info.count.into(),
            version: info.version,
        },
        Err(err) if matches!(err.client_api_error_kind(), Some(ErrorKind::NotFound)) => {
            ServerBackup::Missing
        }
        Err(err) => {
            tracing::debug!(kind = ?err.client_api_error_kind(), "could not ask for the key backup");
            ServerBackup::Unknown
        }
    }
}

// ----- signing out another session ----------------------------------------------------

/// Whether the server lets the request through for the account password alone: one of
/// the ways it offers consists (after what is already done) of that single step.
fn password_offered(info: &UiaaInfo) -> bool {
    info.flows.iter().any(|flow| {
        let mut left = flow
            .stages
            .iter()
            .filter(|stage| !info.completed.contains(stage));
        left.next() == Some(&AuthType::Password) && left.next().is_none()
    })
}

async fn delete_device(
    client: &Client,
    device: &DeviceId,
    auth: Option<AuthData>,
) -> Result<(), HttpError> {
    let mut request = delete_devices::v3::Request::new(vec![device.to_owned()]);
    request.auth = auth;
    // Never sent twice: the second request carries the password.
    let config = RequestConfig::new().disable_retry();
    client
        .send(request)
        .with_request_config(config)
        .await
        .map(|_| ())
}

/// The server's own page for ending that session, if it advertises one.
async fn web_page(client: &Client, device: &DeviceId) -> Option<String> {
    let metadata = tokio::time::timeout(
        Duration::from_secs(10),
        client.oauth().cached_server_metadata(),
    )
    .await
    .ok()?
    .ok()?;
    let url = metadata.account_management_url_with_action(
        AccountManagementActionData::DeviceDelete(DeviceDeleteData::new(device)),
    )?;
    matches!(url.scheme(), "https" | "http").then(|| url.to_string())
}

/// Signs another session out (`POST /delete_devices`). The server answers the first,
/// bare request with the ways it accepts as proof that this is the account's owner.
/// Only the account password is something we can give; with it the request is made
/// once more, and whatever the server says then is final.
async fn sign_out(
    client: &Client,
    device_id: &str,
    password: Option<Zeroizing<String>>,
) -> SignOutOutcome {
    let Some(user_id) = client.user_id() else {
        return SignOutOutcome::Failed("Not logged in".to_owned());
    };
    let device: OwnedDeviceId = device_id.into();
    if client.device_id() == Some(&*device) {
        return SignOutOutcome::Failed(
            "This is the session you are using. Use Log out for it.".to_owned(),
        );
    }
    // An account that logged in through its authentication service has no password
    // the homeserver would know; sessions are ended on that service's pages.
    if matches!(client.auth_api(), Some(AuthApi::OAuth(_))) {
        return SignOutOutcome::Web {
            url: web_page(client, &device).await,
        };
    }
    let failed =
        |err: HttpError| SignOutOutcome::Failed(format!("Could not sign the session out: {err}"));
    let info = match delete_device(client, &device, None).await {
        Ok(()) => return SignOutOutcome::Done,
        Err(err) => match err.as_uiaa_response() {
            Some(info) => info.clone(),
            None => return failed(err),
        },
    };
    if !password_offered(&info) {
        return SignOutOutcome::Web {
            url: web_page(client, &device).await,
        };
    }
    let Some(password) = password else {
        return SignOutOutcome::NeedsPassword;
    };
    let mut auth = Password::new(
        UserIdentifier::Matrix(MatrixUserIdentifier::new(user_id.to_string())),
        password.as_str().to_owned(),
    );
    drop(password);
    auth.session = info.session.clone();
    match delete_device(client, &device, Some(AuthData::Password(auth))).await {
        Ok(()) => SignOutOutcome::Done,
        Err(err) => match err.as_uiaa_response() {
            // Accepted, but the server wants more than it said.
            Some(again) if again.completed.contains(&AuthType::Password) => SignOutOutcome::Failed(
                "The server wants more than the password to sign a session out. \
                     Use your account's web page or another client."
                    .to_owned(),
            ),
            Some(_) => SignOutOutcome::WrongPassword,
            None if matches!(err.client_api_error_kind(), Some(ErrorKind::Forbidden)) => {
                SignOutOutcome::WrongPassword
            }
            None => failed(err),
        },
    }
}

// ----- room keys to and from a file ---------------------------------------------------

/// A folder of our own, next to where a key file is to appear, in which the file is
/// written first. Nobody else can look into it or put anything there (mode 0700), so
/// the SDK, which is given a path and opens it with `File::create`, cannot be made to
/// write through a link or into a file of someone else's making. Removed, with what
/// is in it, when dropped.
struct Staging {
    dir: PathBuf,
    /// The folder as we made it, to see that it was not swapped for another.
    made: std::fs::File,
    file: Option<PathBuf>,
}

impl Staging {
    fn create(parent: &Path) -> std::io::Result<Self> {
        let mut last = None;
        for attempt in 0..16u32 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.subsec_nanos());
            let dir = parent.join(format!(
                ".matrix-rat-keys-{}-{nanos:09}-{attempt}",
                std::process::id()
            ));
            // Fails if anything is there under that name, a link included.
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => {
                    return match std::fs::File::open(&dir) {
                        Ok(made) => Ok(Self {
                            dir,
                            made,
                            file: None,
                        }),
                        Err(err) => {
                            let _ = std::fs::remove_dir(&dir);
                            Err(err)
                        }
                    };
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => last = Some(err),
                Err(err) => return Err(err),
            }
        }
        Err(last.unwrap_or_else(|| std::io::ErrorKind::AlreadyExists.into()))
    }

    /// Where the file is written before it is published.
    fn path(&mut self, name: &OsStr) -> PathBuf {
        let path = self.dir.join(name);
        self.file = Some(path.clone());
        path
    }

    /// The folder is still the one we made: ours, private, and a folder.
    fn intact(&self) -> bool {
        let (Ok(made), Ok(there)) = (self.made.metadata(), std::fs::symlink_metadata(&self.dir))
        else {
            return false;
        };
        there.is_dir()
            && there.dev() == made.dev()
            && there.ino() == made.ino()
            && there.permissions().mode() & 0o077 == 0
    }

    /// Makes the staged file private and durable and gives it its name at `target`
    /// with a hard link: that never follows a link at `target`, fails if anything is
    /// there, and leaves no moment in which `target` holds anything but the finished
    /// file. Where the file system has no hard links nothing is published.
    fn publish(&self, target: &Path) -> Result<(), String> {
        let staged = self
            .file
            .as_deref()
            .ok_or_else(|| "nothing was written".to_owned())?;
        if !self.intact() {
            return Err(
                "The folder the file was being written in was replaced meanwhile. Nothing \
                 was published; export again."
                    .to_owned(),
            );
        }
        let write = |err: std::io::Error| format!("Could not write the file: {err}");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(staged)
            .map_err(write)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(write)?;
        file.sync_all().map_err(write)?;
        match std::fs::hard_link(staged, target) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(
                    "A file with that name exists already. Nothing was written; choose \
                     another name."
                        .to_owned(),
                );
            }
            Err(err)
                if matches!(
                    err.raw_os_error(),
                    Some(libc::EPERM | libc::EOPNOTSUPP | libc::ENOSYS | libc::EMLINK)
                ) =>
            {
                return Err(format!(
                    "That folder's file system cannot give a finished file its name \
                     safely (no hard links: {err}). Nothing was written; choose a folder \
                     on another drive."
                ));
            }
            Err(err) => return Err(write(err)),
        }
        // The new name is in the folder's own data.
        if let Some(parent) = target.parent()
            && let Ok(folder) = std::fs::File::open(folder_of(parent))
        {
            let _ = folder.sync_all();
        }
        Ok(())
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        // Only what is ours: a folder that was swapped is left alone.
        if !self.intact() {
            return;
        }
        if let Some(file) = &self.file {
            let _ = std::fs::remove_file(file);
        }
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// The folder a path lies in; for a bare file name, the current one.
fn folder_of(parent: &Path) -> &Path {
    if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    }
}

/// Writes every message key this device has into a new file at `path`, in the format
/// Element reads and writes, encrypted with `passphrase`. Returns how many.
///
/// `Encryption::export_room_keys` is the only way to the keys that matrix-sdk 0.19.1
/// offers, and it takes a path that it opens itself, later, with `File::create`. So
/// it is given a path inside a [`Staging`] folder, and the result is published under
/// the wanted name only when it is complete.
async fn export_keys(
    client: &Client,
    path: PathBuf,
    passphrase: Zeroizing<String>,
) -> Result<(usize, PathBuf), String> {
    if passphrase.is_empty() {
        return Err("A passphrase is needed".to_owned());
    }
    let Some(name) = path.file_name().map(ToOwned::to_owned) else {
        return Err("That is not a file name".to_owned());
    };
    let parent = folder_of(path.parent().unwrap_or(Path::new(""))).to_owned();
    std::fs::create_dir_all(&parent)
        .map_err(|err| format!("Could not create the folder: {err}"))?;
    // Said again, for certain, when the file is published.
    if std::fs::symlink_metadata(&path).is_ok() {
        return Err(
            "A file with that name exists already. Nothing was written; choose another name."
                .to_owned(),
        );
    }
    let mut staging =
        Staging::create(&parent).map_err(|err| format!("Could not write in that folder: {err}"))?;
    let count = Arc::new(AtomicUsize::new(0));
    let counted = count.clone();
    let result = client
        .encryption()
        .export_room_keys(staging.path(&name), &passphrase, move |_| {
            counted.fetch_add(1, Ordering::Relaxed);
            true
        })
        .await;
    drop(passphrase);
    result.map_err(|err| format!("Could not export the keys: {err}"))?;
    let count = count.load(Ordering::Relaxed);
    if count == 0 {
        return Err("This device has no message keys to export yet".to_owned());
    }
    staging.publish(&path)?;
    Ok((count, path))
}

fn import_error(err: &RoomKeyImportError) -> String {
    match err {
        RoomKeyImportError::Export(KeyExportError::InvalidMac) => {
            "Wrong passphrase (or the file is damaged). Nothing was imported.".to_owned()
        }
        RoomKeyImportError::Export(
            KeyExportError::InvalidHeaders
            | KeyExportError::UnsupportedVersion
            | KeyExportError::Decode(_),
        ) => "This is not a key export file. Nothing was imported.".to_owned(),
        RoomKeyImportError::Io(err) | RoomKeyImportError::Export(KeyExportError::Io(err)) => {
            format!("Could not read the file: {err}")
        }
        other => format!("Could not import the keys: {other}"),
    }
}

/// Reads message keys from a file as Element (or [`export_keys`]) writes it. Returns
/// how many were new to this device, and how many the file held.
async fn import_keys(
    client: &Client,
    path: PathBuf,
    passphrase: Zeroizing<String>,
) -> Result<(usize, usize), String> {
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err("That is not a file".to_owned()),
        Err(err) => return Err(format!("Could not read the file: {err}")),
    }
    let result = client
        .encryption()
        .import_room_keys(path, &passphrase)
        .await;
    drop(passphrase);
    match result {
        Ok(result) => Ok((result.imported_count, result.total_count)),
        Err(err) => Err(import_error(&err)),
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::ruma::api::client::uiaa::AuthFlow;

    use super::*;
    use crate::matrix::MatrixCmd;

    fn flows(flows: &[&[AuthType]]) -> UiaaInfo {
        UiaaInfo::new(
            flows
                .iter()
                .map(|stages| AuthFlow::new(stages.to_vec()))
                .collect(),
        )
    }

    #[test]
    fn only_a_password_alone_counts_as_offered() {
        assert!(password_offered(&flows(&[&[AuthType::Password]])));
        // One way among several is enough.
        assert!(password_offered(&flows(&[
            &[AuthType::Sso],
            &[AuthType::Password]
        ])));
        // Single sign-on only, an authentication service, or nothing at all.
        assert!(!password_offered(&flows(&[&[AuthType::Sso]])));
        assert!(!password_offered(&flows(&[&[AuthType::OAuth]])));
        assert!(!password_offered(&flows(&[])));
        assert!(!password_offered(&flows(&[&[]])));
        // A password and something we cannot give.
        assert!(!password_offered(&flows(&[&[
            AuthType::Password,
            AuthType::EmailIdentity
        ]])));
        // ...unless that part is done already.
        let mut info = flows(&[&[AuthType::EmailIdentity, AuthType::Password]]);
        info.completed = vec![AuthType::EmailIdentity];
        assert!(password_offered(&info));
        // A password that was taken already is not asked for again.
        let mut info = flows(&[&[AuthType::Password]]);
        info.completed = vec![AuthType::Password];
        assert!(!password_offered(&info));
    }

    #[test]
    fn debug_output_hides_secrets_names_and_paths() {
        let commands = [
            MatrixCmd::Devices(DevicesCmd::SignOut {
                request: 1,
                device_id: "PHONEDEVICE".into(),
                password: Some(Zeroizing::new("hunter2".to_owned())),
            }),
            MatrixCmd::Devices(DevicesCmd::Rename {
                request: 2,
                device_id: "PHONEDEVICE".into(),
                name: "secret name".into(),
            }),
            MatrixCmd::Devices(DevicesCmd::ExportKeys {
                request: 3,
                path: "/home/someone/keys.txt".into(),
                passphrase: Zeroizing::new("hunter2".to_owned()),
            }),
            MatrixCmd::Devices(DevicesCmd::ImportKeys {
                request: 4,
                path: "/home/someone/keys.txt".into(),
                passphrase: Zeroizing::new("hunter2".to_owned()),
            }),
        ];
        let session = Session {
            device_id: "PHONEDEVICE".into(),
            name: Some("secret name".into()),
            last_seen: Some(1),
            last_ip: Some("192.0.2.7".into()),
            current: false,
            trust: SessionTrust::Unverified,
        };
        let events = [
            MatrixEvent::Devices(DevicesEvent::List {
                watch: 1,
                list: Ok(SessionList {
                    sessions: vec![session.clone()],
                    vouched: true,
                }),
            }),
            MatrixEvent::Devices(DevicesEvent::Backup {
                watch: 1,
                server: ServerBackup::Present {
                    keys: 3,
                    version: "backup-version-9".into(),
                },
                upload: BackupUpload::Idle,
            }),
            MatrixEvent::Devices(DevicesEvent::Exported {
                request: 3,
                result: Ok((7, "/home/someone/keys.txt".into())),
            }),
            MatrixEvent::Devices(DevicesEvent::SignedOut {
                request: 1,
                outcome: SignOutOutcome::Web {
                    url: Some("https://account.example.org/?device_id=PHONEDEVICE".into()),
                },
            }),
        ];
        let mut text = format!("{session:?}");
        for command in &commands {
            text.push_str(&format!("{command:?}"));
        }
        for event in &events {
            text.push_str(&format!("{event:?}"));
        }
        for secret in [
            "hunter2",
            "secret name",
            "someone",
            "keys.txt",
            "192.0.2.7",
            "account.example.org",
            "backup-version-9",
            "PHONEDEVICE",
        ] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        // What a failure was about, and which backup the server has.
        let failed = SignOutOutcome::Failed("Could not sign out PHONEDEVICE at example.org".into());
        let backup = ServerBackup::Present {
            keys: 7,
            version: "backup-version-9".into(),
        };
        let text = format!("{failed:?}{backup:?}");
        for secret in ["PHONEDEVICE", "example.org", "backup-version-9", "7"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("matrix-rat-keyfile-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// What the SDK does with the path it is given.
    fn sdk_writes(staging: &mut Staging, name: &str, bytes: &[u8]) {
        let path = staging.path(OsStr::new(name));
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn a_key_file_is_staged_in_private_and_published_complete() {
        let dir = scratch("publish");
        let target = dir.join("keys.txt");
        let mut staging = Staging::create(&dir).unwrap();
        let mode = |path: &Path| {
            std::fs::symlink_metadata(path)
                .unwrap()
                .permissions()
                .mode()
        };
        assert_eq!(mode(&staging.dir) & 0o777, 0o700);
        sdk_writes(&mut staging, "keys.txt", b"keys");
        // Nothing under the wanted name until it is finished.
        assert!(!target.exists());
        staging.publish(&target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"keys");
        assert_eq!(mode(&target) & 0o777, 0o600);
        drop(staging);
        // The staging folder is gone, and the file has one name.
        assert_eq!(entries(&dir), ["keys.txt"]);
        assert_eq!(std::fs::metadata(&target).unwrap().nlink(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_key_file_never_replaces_anything_and_failures_leave_nothing() {
        let dir = scratch("refuse");
        // An existing file stays as it is.
        let target = dir.join("keys.txt");
        std::fs::write(&target, b"mine").unwrap();
        let mut staging = Staging::create(&dir).unwrap();
        sdk_writes(&mut staging, "keys.txt", b"keys");
        let refused = staging.publish(&target).unwrap_err();
        assert!(refused.contains("exists already"), "{refused}");
        drop(staging);
        assert_eq!(std::fs::read(&target).unwrap(), b"mine");
        assert_eq!(entries(&dir), ["keys.txt"]);

        // A link that points nowhere is not followed, and nothing appears behind it.
        let link = dir.join("link.txt");
        std::os::unix::fs::symlink(dir.join("elsewhere.txt"), &link).unwrap();
        let mut staging = Staging::create(&dir).unwrap();
        sdk_writes(&mut staging, "link.txt", b"keys");
        let refused = staging.publish(&link).unwrap_err();
        assert!(refused.contains("exists already"), "{refused}");
        drop(staging);
        assert!(!dir.join("elsewhere.txt").exists());
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(entries(&dir), ["keys.txt", "link.txt"]);

        // Given up before publishing (the export failed): nothing is left.
        let mut staging = Staging::create(&dir).unwrap();
        sdk_writes(&mut staging, "other.txt", b"half");
        drop(staging);
        assert_eq!(entries(&dir), ["keys.txt", "link.txt"]);
        // Nothing written at all.
        let staging = Staging::create(&dir).unwrap();
        assert!(staging.publish(&dir.join("none.txt")).is_err());
        drop(staging);
        assert_eq!(entries(&dir), ["keys.txt", "link.txt"]);

        // The staging folder swapped for another one: not published, not cleaned out.
        let mut staging = Staging::create(&dir).unwrap();
        sdk_writes(&mut staging, "swapped.txt", b"keys");
        let moved = dir.join("moved-away");
        std::fs::rename(&staging.dir, &moved).unwrap();
        std::fs::create_dir(&staging.dir).unwrap();
        std::fs::write(staging.dir.join("swapped.txt"), b"theirs").unwrap();
        let refused = staging.publish(&dir.join("swapped.txt")).unwrap_err();
        assert!(refused.contains("replaced"), "{refused}");
        let theirs = staging.dir.clone();
        drop(staging);
        assert!(!dir.join("swapped.txt").exists());
        assert_eq!(
            std::fs::read(theirs.join("swapped.txt")).unwrap(),
            b"theirs"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_bare_file_name_means_the_current_folder() {
        assert_eq!(
            folder_of(Path::new("keys.txt").parent().unwrap()),
            Path::new(".")
        );
        assert_eq!(
            folder_of(Path::new("/tmp/keys.txt").parent().unwrap()),
            Path::new("/tmp")
        );
        // And works: staged and published next to where the tests run, then removed.
        let name = format!("matrix-rat-test-keys-{}.txt", std::process::id());
        let target = Path::new(&name);
        let parent = folder_of(target.parent().unwrap());
        let mut staging = Staging::create(parent).unwrap();
        sdk_writes(&mut staging, &name, b"keys");
        staging.publish(target).unwrap();
        drop(staging);
        assert_eq!(std::fs::read(target).unwrap(), b"keys");
        std::fs::remove_file(target).unwrap();
    }

    #[test]
    fn import_errors_are_told_in_plain_words() {
        let wrong = import_error(&RoomKeyImportError::Export(KeyExportError::InvalidMac));
        assert!(wrong.starts_with("Wrong passphrase"), "{wrong}");
        let other = import_error(&RoomKeyImportError::Export(KeyExportError::InvalidHeaders));
        assert!(
            other.starts_with("This is not a key export file"),
            "{other}"
        );
        let missing = import_error(&RoomKeyImportError::Io(std::io::Error::from(
            std::io::ErrorKind::NotFound,
        )));
        assert!(missing.starts_with("Could not read the file"), "{missing}");
    }

    // ----- against a homeserver -------------------------------------------------------
    //
    // Start one with `scripts/dev-homeserver.sh`; then
    // `cargo test devices::tests -- --ignored --nocapture --test-threads=1`. Every test
    // makes its own account (the server's registration token is needed, see
    // `verify::tests::new_account`) and ends every session it began.

    use matrix_sdk::config::SyncSettings;
    use tokio::sync::mpsc::UnboundedReceiver;

    use crate::matrix::verify::tests::{
        first_device, homeserver, new_account, trace, wait_for, worker_paths,
    };
    use crate::matrix::{MatrixHandle, Security, spawn};

    /// A plain SDK client (like Element would be) logged in as a new device.
    async fn plain_login(name: &str, password: &str, device: &str) -> Client {
        let client = Client::builder()
            .homeserver_url(homeserver())
            .build()
            .await
            .expect("client");
        client
            .matrix_auth()
            .login_username(name, password)
            .initial_device_display_name(device)
            .send()
            .await
            .expect("login");
        client
            .sync_once(SyncSettings::default())
            .await
            .expect("first sync");
        client
    }

    /// An encrypted room of `client` with one message in it.
    async fn encrypted_room_with(client: &Client, text: &str) -> matrix_sdk::Room {
        use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoom;
        use matrix_sdk::ruma::events::InitialStateEvent;
        use matrix_sdk::ruma::events::room::encryption::RoomEncryptionEventContent;
        use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
        let mut create = CreateRoom::new();
        create.initial_state = vec![
            InitialStateEvent::with_empty_state_key(
                RoomEncryptionEventContent::with_recommended_defaults(),
            )
            .to_raw_any(),
        ];
        let room = client.create_room(create).await.expect("room");
        room.send(RoomMessageEventContent::text_plain(text))
            .await
            .expect("send");
        room
    }

    /// Ends a worker's session on the server and stops the worker.
    async fn log_out(handle: MatrixHandle, mut events: UnboundedReceiver<MatrixEvent>) {
        handle.send(MatrixCmd::Logout);
        wait_for(&mut events, "the logout", |e| {
            matches!(e, MatrixEvent::NeedLogin { .. })
        })
        .await;
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
    }

    /// Leaves and forgets a room and ends a plain client's session.
    async fn tidy(client: Client, room: Option<matrix_sdk::Room>) {
        if let Some(room) = room {
            room.leave().await.expect("leave");
            room.forget().await.expect("forget");
        }
        client.logout().await.expect("logout");
    }

    fn devices(command: DevicesCmd) -> MatrixCmd {
        MatrixCmd::Devices(command)
    }

    /// Two logins of one account: both are listed (this one marked and verified, the
    /// other not verified), renaming shows on the server and in the list, and the
    /// other one is signed out with the account password, after a wrong password was
    /// reported and changed nothing.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn sessions_are_listed_renamed_and_signed_out() {
        trace();
        let (name, password) = new_account("ratsessions").await;
        let root = std::env::temp_dir().join(format!("matrix-rat-{name}"));
        let (a, mut events) = first_device(&root, &name, &password).await;
        let b = plain_login(&name, &password, "second login").await;
        let b_id = b.device_id().expect("device id").to_string();

        a.send(devices(DevicesCmd::Watch(Some(1))));
        let mut list = None;
        wait_for(&mut events, "both sessions, with keys", |e| {
            if let MatrixEvent::Devices(DevicesEvent::List { list: Ok(now), .. }) = e
                && now.sessions.len() == 2
                && now
                    .sessions
                    .iter()
                    .any(|s| s.device_id == b_id && s.trust == SessionTrust::Unverified)
            {
                list = Some(now.clone());
            }
            list.is_some()
        })
        .await;
        let list = list.unwrap();
        assert!(list.vouched, "A trusts its own identity");
        let (own, other) = (&list.sessions[0], &list.sessions[1]);
        assert!(
            own.current && own.trust == SessionTrust::Verified,
            "{own:?}"
        );
        assert!(!other.current);
        assert_eq!(other.name.as_deref(), Some("second login"));
        let own_id = own.device_id.clone();

        // Rename the other session, then this one: the server has the names, and the
        // list follows without being asked.
        for (request, device_id, new) in [
            (1, &b_id, "  renamed phone "),
            (2, &own_id, "this one, renamed"),
        ] {
            a.send(devices(DevicesCmd::Rename {
                request,
                device_id: device_id.clone(),
                name: new.to_owned(),
            }));
            let (mut answered, mut listed) = (false, false);
            wait_for(&mut events, "the rename", |e| {
                match e {
                    MatrixEvent::Devices(DevicesEvent::Renamed { request: r, result })
                        if *r == request =>
                    {
                        assert_eq!(*result, Ok(()));
                        answered = true;
                    }
                    MatrixEvent::Devices(DevicesEvent::List { list: Ok(now), .. }) => {
                        listed = now.sessions.iter().any(|s| {
                            s.device_id == *device_id && s.name.as_deref() == Some(new.trim())
                        });
                    }
                    _ => {}
                }
                answered && listed
            })
            .await;
        }
        let on_server = b.devices().await.expect("devices").devices;
        let name_of = |id: &str| {
            on_server
                .iter()
                .find(|d| d.device_id == id)
                .and_then(|d| d.display_name.clone())
        };
        assert_eq!(name_of(&b_id).as_deref(), Some("renamed phone"));
        assert_eq!(name_of(&own_id).as_deref(), Some("this one, renamed"));

        // Signing out: the server asks for the password...
        let sign_out = |request, device_id: &str, password: Option<&str>| {
            a.send(devices(DevicesCmd::SignOut {
                request,
                device_id: device_id.to_owned(),
                password: password.map(|p| Zeroizing::new(p.to_owned())),
            }));
        };
        let outcome_of = |e: &MatrixEvent, wanted: u64| match e {
            MatrixEvent::Devices(DevicesEvent::SignedOut { request, outcome })
                if *request == wanted =>
            {
                Some(outcome.clone())
            }
            _ => None,
        };
        sign_out(3, &b_id, None);
        let mut outcome = None;
        wait_for(&mut events, "the server to ask for the password", |e| {
            outcome = outcome.take().or(outcome_of(e, 3));
            outcome.is_some()
        })
        .await;
        assert_eq!(outcome, Some(SignOutOutcome::NeedsPassword));
        // ...a wrong one is reported, and the session is still there...
        sign_out(4, &b_id, Some("not the password"));
        let mut outcome = None;
        wait_for(&mut events, "the wrong password to be refused", |e| {
            outcome = outcome.take().or(outcome_of(e, 4));
            outcome.is_some()
        })
        .await;
        assert_eq!(outcome, Some(SignOutOutcome::WrongPassword));
        b.whoami().await.expect("B is still logged in");
        // ...and the right one ends it: gone from the list, its token refused.
        sign_out(5, &b_id, Some(&password));
        let (mut outcome, mut alone) = (None, false);
        wait_for(&mut events, "the session to be signed out", |e| {
            outcome = outcome.take().or(outcome_of(e, 5));
            if let MatrixEvent::Devices(DevicesEvent::List { list: Ok(now), .. }) = e {
                alone = now.sessions.len() == 1 && now.sessions[0].device_id == own_id;
            }
            outcome.is_some() && alone
        })
        .await;
        assert_eq!(outcome, Some(SignOutOutcome::Done));
        b.whoami().await.expect_err("B was signed out");
        // This device is never signed out from here, password or not.
        sign_out(6, &own_id, Some(&password));
        let mut outcome = None;
        wait_for(&mut events, "the refusal", |e| {
            outcome = outcome.take().or(outcome_of(e, 6));
            outcome.is_some()
        })
        .await;
        assert!(
            matches!(&outcome, Some(SignOutOutcome::Failed(why)) if why.contains("Log out")),
            "{outcome:?}"
        );

        log_out(a, events).await;
        let _ = std::fs::remove_dir_all(root);
    }

    /// Keys written to a file by one device let another device of the account read a
    /// message from before it existed: the file is new, private and never overwritten;
    /// a wrong passphrase imports nothing; after the import the open timeline shows
    /// the message without being opened again.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn exported_keys_unlock_history_on_a_new_device() {
        use crate::model::{ItemRef, MsgKind};
        trace();
        let (name, password) = new_account("ratkeys").await;
        let a = plain_login(&name, &password, "first").await;
        let secret = format!("written before B existed, by {name}");
        let room = encrypted_room_with(&a, &secret).await;
        let room_id = room.room_id().to_string();

        let dir = std::env::temp_dir().join(format!("matrix-rat-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("exported").join("keys.txt");
        let pass = |text: &str| Zeroizing::new(text.to_owned());
        // No passphrase, no file.
        export_keys(&a, file.clone(), pass(""))
            .await
            .expect_err("an empty passphrase");
        assert!(!file.exists());
        let (count, written) = export_keys(&a, file.clone(), pass("correct horse"))
            .await
            .expect("export");
        assert!(count >= 1, "{count} keys");
        assert_eq!(written, file);
        let meta = std::fs::metadata(&file).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let exported = std::fs::read_to_string(&file).unwrap();
        assert!(
            exported.starts_with("-----BEGIN MEGOLM SESSION DATA-----"),
            "not the format Element reads"
        );
        assert!(!exported.contains(&secret) && !exported.contains(&room_id));
        // Never overwritten.
        let again = export_keys(&a, file.clone(), pass("another passphrase"))
            .await
            .expect_err("the file exists");
        assert!(again.contains("exists already"), "{again}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), exported);

        // Device B: matrix-rat, logged in after the message was sent. No backup, and A
        // is not syncing, so nobody gives it the key.
        let (b, mut events) = spawn(worker_paths(&dir, "b"));
        b.send(MatrixCmd::LoginPassword {
            homeserver: homeserver(),
            username: name.clone(),
            password: password.clone().into(),
        });
        let mut opened = false;
        let mut locked = None;
        wait_for(&mut events, "B to see the locked message", |e| {
            match e {
                MatrixEvent::Rooms(rooms) if !opened && rooms.iter().any(|r| r.id == room_id) => {
                    opened = true;
                    b.send(MatrixCmd::OpenRoom {
                        room_id: room_id.clone(),
                        generation: 1,
                    });
                }
                MatrixEvent::Timeline { messages, .. } => {
                    if let Some(message) =
                        messages.iter().find(|m| m.kind == MsgKind::Undecryptable)
                    {
                        locked = Some((message.item.clone(), message.utd, message.body.clone()));
                    }
                }
                _ => {}
            }
            locked.is_some()
        })
        .await;
        let (item, why, body) = locked.unwrap();
        eprintln!("shown as: {body} ({why:?})");
        assert!(why.is_some(), "a reason is given");
        assert!(body.starts_with("Unable to decrypt"), "{body}");
        let Some(ItemRef::Event(event_id)) = item else {
            panic!("the message has no event id");
        };

        // Trying again finds nothing: there is no backup and nobody to ask.
        b.send(MatrixCmd::RetryDecryption {
            room_id: room_id.clone(),
            event_id: event_id.clone(),
        });
        let mut retried = None;
        wait_for(&mut events, "the retry", |e| {
            if let MatrixEvent::DecryptionRetried {
                result,
                event_id: answered,
                ..
            } = e
            {
                assert_eq!(*answered, event_id);
                retried = Some(result.clone());
            }
            retried.is_some()
        })
        .await;
        assert_eq!(retried, Some(Ok(crate::matrix::Retried::StillLocked)));

        let import = |request, passphrase: &str| {
            b.send(devices(DevicesCmd::ImportKeys {
                request,
                path: file.clone(),
                passphrase: pass(passphrase),
            }));
        };
        let imported_of = |e: &MatrixEvent, wanted: u64| match e {
            MatrixEvent::Devices(DevicesEvent::Imported { request, result })
                if *request == wanted =>
            {
                Some(result.clone())
            }
            _ => None,
        };
        // A wrong passphrase imports nothing.
        import(1, "correct horse?");
        let mut result = None;
        wait_for(&mut events, "the wrong passphrase", |e| {
            result = result.take().or(imported_of(e, 1));
            result.is_some()
        })
        .await;
        let refused = result.unwrap().expect_err("a wrong passphrase");
        assert!(refused.starts_with("Wrong passphrase"), "{refused}");
        // The right one: the keys come in, and the open timeline shows the message.
        import(2, "correct horse");
        let (mut result, mut readable) = (None, false);
        wait_for(&mut events, "the import and the message", |e| {
            result = result.take().or(imported_of(e, 2));
            if let MatrixEvent::Timeline { messages, .. } = e {
                readable = messages
                    .iter()
                    .any(|m| m.body == secret && m.kind != MsgKind::Undecryptable);
            }
            result.is_some() && readable
        })
        .await;
        let (imported, total) = result.unwrap().expect("import");
        assert!(imported >= 1, "{imported} of {total}");
        assert_eq!(total, count);
        // Once more: nothing is new.
        import(3, "correct horse");
        let mut result = None;
        wait_for(&mut events, "the second import", |e| {
            result = result.take().or(imported_of(e, 3));
            result.is_some()
        })
        .await;
        assert_eq!(result, Some(Ok((0, count))));

        log_out(b, events).await;
        tidy(a, Some(room)).await;
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A real export, published the safe way: nothing appears under the wanted name
    /// until the file is complete; it is private; an existing file, a dangling link
    /// and a folder at that name are left as they are; and nothing of the staging is
    /// left behind, whether the export worked or not.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn a_real_export_is_published_complete_private_and_only_once() {
        trace();
        let (name, password) = new_account("ratexport").await;
        let a = plain_login(&name, &password, "first").await;
        let room = encrypted_room_with(&a, "something worth a key").await;
        let dir = std::env::temp_dir().join(format!("matrix-rat-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let pass = || Zeroizing::new("correct horse".to_owned());
        let names = |dir: &Path| {
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };

        let file = dir.join("keys.txt");
        let (count, written) = export_keys(&a, file.clone(), pass()).await.expect("export");
        assert!(count >= 1);
        assert_eq!(written, file);
        let meta = std::fs::symlink_metadata(&file).unwrap();
        assert!(meta.is_file());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(meta.nlink(), 1, "the staged name is gone");
        assert_eq!(names(&dir), ["keys.txt"]);
        let exported = std::fs::read_to_string(&file).unwrap();
        assert!(exported.starts_with("-----BEGIN MEGOLM SESSION DATA-----"));
        assert!(
            exported
                .trim_end()
                .ends_with("-----END MEGOLM SESSION DATA-----")
        );

        // Something is there already: a file, a link to nowhere, a folder.
        let link = dir.join("link.txt");
        std::os::unix::fs::symlink(dir.join("elsewhere.txt"), &link).unwrap();
        let folder = dir.join("folder");
        std::fs::create_dir(&folder).unwrap();
        for taken in [&file, &link, &folder] {
            let refused = export_keys(&a, taken.clone(), pass())
                .await
                .expect_err("the name is taken");
            assert!(refused.contains("exists already"), "{refused}");
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), exported);
        assert!(!dir.join("elsewhere.txt").exists());
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(names(&dir), ["folder", "keys.txt", "link.txt"]);
        assert!(names(&folder).is_empty());
        // Into a folder that does not exist yet, and a second time under a new name.
        let deep = dir.join("new").join("keys-2.txt");
        export_keys(&a, deep.clone(), pass()).await.expect("export");
        assert_eq!(names(deep.parent().unwrap()), ["keys-2.txt"]);
        // A folder that cannot be written in: said, and nothing left anywhere.
        let locked = dir.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        let refused = export_keys(&a, locked.join("keys.txt"), pass())
            .await
            .expect_err("a folder without write access");
        assert!(
            refused.starts_with("Could not write in that folder"),
            "{refused}"
        );
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(names(&locked).is_empty());

        tidy(a, Some(room)).await;
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The state of the key backup as the popup words it: for an account that has
    /// none, and for one that has a backup and recovery, first on a device that
    /// cannot read it and then, the recovery key entered, on one that can.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn backup_status_with_and_without_a_backup() {
        use crate::devices::backup_lines;
        use crate::matrix::verify::set_up_cross_signing;
        use crate::matrix::{
            BackupStatus, DeviceTrust, Identity, RecoveryStatus, SetUpOutcome, VerifyCmd,
        };
        trace();
        let words = |security: Security, (server, upload): &(ServerBackup, BackupUpload)| {
            backup_lines(Some(security), server, *upload)
                .into_iter()
                .map(|(_, text)| text)
                .collect::<Vec<_>>()
                .join("\n")
        };

        // Without: a new account whose only device has set up cross-signing.
        let (name, password) = new_account("ratnobackup").await;
        let root = std::env::temp_dir().join(format!("matrix-rat-{name}"));
        let (w, mut events) = spawn(worker_paths(&root, "w"));
        w.send(MatrixCmd::LoginPassword {
            homeserver: homeserver(),
            username: name.clone(),
            password: password.clone().into(),
        });
        wait_for(
            &mut events,
            "no identity yet",
            |e| matches!(e, MatrixEvent::Security(s) if s.identity == Identity::Missing),
        )
        .await;
        w.send(MatrixCmd::Verification(VerifyCmd::SetUp {
            password: Some(password.clone().into()),
        }));
        w.send(devices(DevicesCmd::Watch(Some(1))));
        let (mut security, mut server) = (None, None);
        wait_for(&mut events, "a verified device without backup", |e| {
            match e {
                MatrixEvent::Security(s) => security = Some(*s),
                MatrixEvent::Devices(DevicesEvent::Backup {
                    server: now,
                    upload,
                    ..
                }) => {
                    server = Some((now.clone(), *upload));
                }
                _ => {}
            }
            security.is_some_and(|s| {
                s.device == DeviceTrust::Verified && s.recovery == RecoveryStatus::NotSetUp
            }) && matches!(server, Some((ServerBackup::Missing, _)))
        })
        .await;
        let security = security.unwrap();
        assert_eq!(security.backup, BackupStatus::Inactive);
        let text = words(security, &server.unwrap());
        eprintln!("without a backup:\n{text}\n");
        assert!(
            text.contains("There is no key backup on the server"),
            "{text}"
        );
        assert!(text.contains("Recovery is not set up"), "{text}");
        log_out(w, events).await;
        let _ = std::fs::remove_dir_all(root);

        // With: another account. A plain client sets up cross-signing and recovery
        // (which creates the backup) and writes a message, whose key is backed up.
        let (name, password) = new_account("ratbackup").await;
        let a = plain_login(&name, &password, "first").await;
        assert_eq!(
            set_up_cross_signing(&a, Some(password.clone().into())).await,
            SetUpOutcome::Done
        );
        let recovery_key = a
            .encryption()
            .recovery()
            .enable()
            .await
            .expect("recovery on A");
        let room = encrypted_room_with(&a, "something to back up").await;
        a.encryption()
            .backups()
            .wait_for_steady_state()
            .await
            .expect("room key backed up");

        // A new device of that account: the backup is there, but not for it.
        let root = std::env::temp_dir().join(format!("matrix-rat-{name}"));
        let (b, mut events) = spawn(worker_paths(&root, "b"));
        b.send(MatrixCmd::LoginPassword {
            homeserver: homeserver(),
            username: name.clone(),
            password: password.clone().into(),
        });
        b.send(devices(DevicesCmd::Watch(Some(1))));
        let (mut security, mut server) = (None::<Security>, None);
        let mut watching = false;
        wait_for(&mut events, "a backup this device cannot read", |e| {
            match e {
                // The command sent before the login was dropped with it.
                MatrixEvent::LoggedIn { .. } if !watching => {
                    watching = true;
                    b.send(devices(DevicesCmd::Watch(Some(1))));
                }
                MatrixEvent::Security(s) => security = Some(*s),
                MatrixEvent::Devices(DevicesEvent::Backup {
                    server: now,
                    upload,
                    ..
                }) => {
                    server = Some((now.clone(), *upload));
                }
                _ => {}
            }
            security.is_some_and(|s| s.recovery == RecoveryStatus::NeedsKey)
                && matches!(&server, Some((ServerBackup::Present { keys, .. }, _)) if *keys >= 1)
        })
        .await;
        let locked = security.unwrap();
        assert_eq!(locked.backup, BackupStatus::Inactive);
        let text = words(locked, server.as_ref().unwrap());
        assert!(
            text.starts_with("The server has a key backup (version "),
            "{text}"
        );
        eprintln!("with a backup, before the recovery key:\n{text}\n");
        assert!(
            text.contains("This device cannot read the backup: enter your recovery key."),
            "{text}"
        );

        // The recovery key entered (from here: at any time, not only at the start).
        b.send(MatrixCmd::Recover {
            key: recovery_key.into(),
        });
        let mut recovered = false;
        wait_for(&mut events, "the backup to be in use", |e| {
            match e {
                MatrixEvent::RecoverResult(result) => {
                    assert_eq!(*result, Ok(()));
                    recovered = true;
                }
                MatrixEvent::Security(s) => security = Some(*s),
                MatrixEvent::Devices(DevicesEvent::Backup {
                    server: now,
                    upload,
                    ..
                }) => {
                    server = Some((now.clone(), *upload));
                }
                _ => {}
            }
            recovered
                && security.is_some_and(|s| {
                    s.backup == BackupStatus::Enabled && s.recovery == RecoveryStatus::Ready
                })
        })
        .await;
        let text = words(security.unwrap(), server.as_ref().unwrap());
        eprintln!("after the recovery key:\n{text}\n");
        assert!(
            text.starts_with("The server has a key backup (version "),
            "{text}"
        );
        assert!(text.contains("This device has a backup key"), "{text}");
        assert!(text.contains("Recovery is set up"), "{text}");

        log_out(b, events).await;
        tidy(a, Some(room)).await;
        let _ = std::fs::remove_dir_all(root);
    }
}
