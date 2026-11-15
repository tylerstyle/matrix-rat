//! What the UI knows about the account's sessions (devices) and its message keys: the
//! two popups ("Sessions", "Key backup and recovery") and the words they use.

use std::path::{Path, PathBuf};

use crate::lineedit::LineEdit;
use crate::matrix::{
    BackupStatus, BackupUpload, DeviceTrust, RecoveryStatus, Security, ServerBackup, Session,
    SessionList, SessionTrust,
};
use crate::model::UtdWhy;
use crate::text::sanitize_line;

/// The controls of the two popups. Keys and clicks both resolve to these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Click {
    /// Select a session in the list.
    Row(usize),
    Rename,
    /// Verify the selected session (or get verified by it).
    Verify,
    SignOut,
    /// Show or hide the addresses the sessions were last seen at.
    Addresses,
    /// Yes to what the popup asks: sign out, send the password, rename, export, import.
    Confirm,
    /// Step back to the list (or status), or close the popup from there.
    Cancel,
    /// Open the account's web page for ending a session.
    OpenWeb,
    /// Enter the recovery key.
    RecoveryKey,
    Export,
    Import,
}

/// What the app keeps for both popups. The list and the backup's state are only kept
/// current while one of the popups is open.
#[derive(Debug, Default)]
pub struct DevicesState {
    /// `None` until the worker has answered.
    pub list: Option<Result<SessionList, String>>,
    pub backup: ServerBackup,
    pub upload: BackupUpload,
    /// The number of the watch the worker runs for the open popup. News that quotes
    /// another number is from a popup that was closed, and dropped.
    pub watch: Option<u64>,
    pub next_watch: u64,
    /// Numbers the requests; answers quote theirs back.
    pub next_request: u64,
    /// Messages "try again" was asked for and has not been answered: by event id,
    /// why each could not be decrypted.
    pub retried: std::collections::HashMap<String, UtdWhy>,
}

impl DevicesState {
    /// The most retries that are remembered at a time.
    pub const RETRIES: usize = 32;

    pub fn sessions(&self) -> &[Session] {
        match &self.list {
            Some(Ok(list)) => &list.sessions,
            _ => &[],
        }
    }
}

/// A name for a session: what it calls itself (if anything) and its id.
pub fn label(session: &Session) -> String {
    match session
        .name
        .as_deref()
        .map(sanitize_line)
        .filter(|name| !name.trim().is_empty())
    {
        Some(name) => format!("{name} ({})", sanitize_line(&session.device_id)),
        None => sanitize_line(&session.device_id),
    }
}

/// What a row says about a session's keys. With `vouched` this device checked the
/// signature itself; without, it can only repeat the server.
pub fn trust_text(trust: SessionTrust, vouched: bool) -> &'static str {
    match (trust, vouched) {
        (SessionTrust::Verified, true) => "verified",
        (SessionTrust::Unverified, true) => "not verified",
        (SessionTrust::Verified, false) => "says verified",
        (SessionTrust::Unverified, false) => "says not verified",
        (SessionTrust::NoKeys, _) => "no encryption keys",
    }
}

/// How a row's trust is to be read: only what this device checked is good news.
pub fn trust_tone(trust: SessionTrust, vouched: bool) -> Tone {
    match (trust, vouched) {
        (SessionTrust::Verified, true) => Tone::Good,
        (_, true) => Tone::Warn,
        (_, false) => Tone::Plain,
    }
}

fn unverified(list: &SessionList) -> usize {
    list.sessions
        .iter()
        .filter(|session| session.trust != SessionTrust::Verified)
        .count()
}

/// The line above the list: how many sessions the account's identity does not vouch
/// for. `None` while there is nothing to say.
pub fn warning(list: &SessionList) -> Option<String> {
    match unverified(list) {
        0 => None,
        1 => Some(
            "1 session is not verified. If you do not recognise it, sign it out and change \
             your password."
                .to_owned(),
        ),
        n => Some(format!(
            "{n} sessions are not verified. Sign out any you do not recognise, and change \
             your password."
        )),
    }
}

/// What stands above the list of sessions. "Verified" is only said (and only then
/// in a good tone) when this device checked it; otherwise the server's account is
/// given as that, with the reason.
pub fn session_lines(list: &SessionList) -> Vec<(Tone, String)> {
    if list.sessions.is_empty() {
        return vec![(Tone::Plain, "The server lists no sessions.".to_owned())];
    }
    if !list.vouched {
        let total = list.sessions.len();
        let claim = match unverified(list) {
            0 => "The server shows all of them as verified.".to_owned(),
            1 => "The server shows 1 of them as not verified.".to_owned(),
            n => format!("The server shows {n} of them as not verified."),
        };
        return vec![
            (
                Tone::Warn,
                "This device is not verified, so it cannot check your other sessions. \
                 Verify it first; until then nothing below is confirmed."
                    .to_owned(),
            ),
            (
                Tone::Plain,
                format!(
                    "{total} {} listed. {claim}",
                    if total == 1 {
                        "session is"
                    } else {
                        "sessions are"
                    }
                ),
            ),
        ];
    }
    match warning(list) {
        Some(text) => vec![(Tone::Warn, text)],
        None => vec![(
            Tone::Good,
            match list.sessions.len() {
                1 => "This is the only session, and it is verified.".to_owned(),
                n => format!("All {n} sessions are verified."),
            },
        )],
    }
}

/// When a session was last seen, as a date in local time.
pub fn last_seen(session: &Session) -> String {
    use chrono::TimeZone;
    session
        .last_seen
        .and_then(|ms| i64::try_from(ms).ok())
        .and_then(|ms| chrono::Local.timestamp_millis_opt(ms).single())
        .map(|at| at.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "never seen".to_owned())
}

// ----- the sessions popup -------------------------------------------------------------

/// Signing out another session, from the question to the server's last word.
#[derive(Clone)]
pub struct SignOut {
    pub device_id: String,
    pub label: String,
    /// The request the worker is busy with.
    pub request: Option<u64>,
    /// The account password, once the server has asked for it.
    pub password: Option<LineEdit>,
    /// The server takes no password: its web page does this (if it names one).
    pub web: Option<Option<String>>,
    pub error: Option<String>,
    /// The button with the keyboard focus (when the field does not have it).
    pub on_confirm: bool,
}

/// Never prints the password, the name or the address.
impl std::fmt::Debug for SignOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignOut")
            .field("busy", &self.request.is_some())
            .field("password_field", &self.password.is_some())
            .field("web", &self.web.is_some())
            .finish_non_exhaustive()
    }
}

impl SignOut {
    /// The password field is shown and takes what is typed.
    pub fn typing(&self) -> bool {
        self.password.is_some() && self.request.is_none() && self.web.is_none()
    }
}

#[derive(Clone)]
pub struct Rename {
    pub device_id: String,
    pub name: LineEdit,
    pub request: Option<u64>,
    pub error: Option<String>,
}

impl std::fmt::Debug for Rename {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rename")
            .field("busy", &self.request.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Default)]
pub enum SessionStage {
    #[default]
    List,
    Rename(Rename),
    SignOut(SignOut),
}

#[derive(Debug, Clone, Default)]
pub struct SessionsPopup {
    pub stage: SessionStage,
    /// The device id of the selected session; kept across updates of the list.
    pub selected: Option<String>,
    pub scroll: usize,
    /// Rows of the list as of the last draw.
    pub page: usize,
    pub reveal: bool,
    pub show_addresses: bool,
}

impl SessionsPopup {
    /// Bytes a session name may have.
    pub const NAME_MAX_BYTES: usize = 100;

    /// The position of the selected session (the first one if it is gone).
    pub fn selected_index(&self, sessions: &[Session]) -> usize {
        self.selected
            .as_deref()
            .and_then(|id| sessions.iter().position(|s| s.device_id == id))
            .unwrap_or(0)
    }

    pub fn selected_session<'a>(&self, sessions: &'a [Session]) -> Option<&'a Session> {
        sessions.get(self.selected_index(sessions))
    }

    pub fn select(&mut self, sessions: &[Session], index: usize) {
        if let Some(session) = sessions.get(index) {
            self.selected = Some(session.device_id.clone());
            self.reveal = true;
        }
    }

    pub fn step(&mut self, sessions: &[Session], delta: isize) {
        if sessions.is_empty() {
            return;
        }
        let at = self.selected_index(sessions) as isize;
        let to = (at + delta).clamp(0, sessions.len() as isize - 1);
        self.select(sessions, to as usize);
    }
}

// ----- the key backup popup -----------------------------------------------------------

/// The fewest characters a passphrase for a key file may have.
pub const PASSPHRASE_MIN: usize = 8;

/// Writing the message keys to a file.
#[derive(Clone)]
pub struct ExportForm {
    pub path: LineEdit,
    pub passphrase: LineEdit,
    pub again: LineEdit,
    /// 0–2 are the fields, 3 and 4 the buttons.
    pub focus: usize,
    pub request: Option<u64>,
    pub error: Option<String>,
}

impl ExportForm {
    pub const FIELDS: usize = 3;
    pub const CONFIRM: usize = 3;
    pub const CANCEL: usize = 4;
    pub const SLOTS: usize = 5;

    pub fn new(path: &Path) -> Self {
        Self {
            path: LineEdit::from(crate::files::display(path)),
            passphrase: LineEdit::secret(),
            again: LineEdit::secret(),
            focus: 1,
            request: None,
            error: None,
        }
    }

    pub fn field_mut(&mut self, slot: usize) -> Option<&mut LineEdit> {
        match slot {
            0 => Some(&mut self.path),
            1 => Some(&mut self.passphrase),
            2 => Some(&mut self.again),
            _ => None,
        }
    }
}

/// Reading message keys from a file.
#[derive(Clone)]
pub struct ImportForm {
    pub path: LineEdit,
    pub passphrase: LineEdit,
    /// 0 and 1 are the fields, 2 and 3 the buttons.
    pub focus: usize,
    pub request: Option<u64>,
    pub error: Option<String>,
}

impl ImportForm {
    pub const FIELDS: usize = 2;
    pub const CONFIRM: usize = 2;
    pub const CANCEL: usize = 3;
    pub const SLOTS: usize = 4;

    pub fn new() -> Self {
        Self {
            path: LineEdit::new(),
            passphrase: LineEdit::secret(),
            focus: 0,
            request: None,
            error: None,
        }
    }

    pub fn field_mut(&mut self, slot: usize) -> Option<&mut LineEdit> {
        match slot {
            0 => Some(&mut self.path),
            1 => Some(&mut self.passphrase),
            _ => None,
        }
    }
}

// Hand-written: neither a passphrase nor the path of a key file belongs in a log.
impl std::fmt::Debug for ExportForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportForm")
            .field("focus", &self.focus)
            .field("busy", &self.request.is_some())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ImportForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportForm")
            .field("focus", &self.focus)
            .field("busy", &self.request.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Default)]
pub enum KeysStage {
    #[default]
    Status,
    Export(ExportForm),
    Import(ImportForm),
}

/// The buttons of the status page, in order.
pub const STATUS_BUTTONS: [Click; 4] = [
    Click::RecoveryKey,
    Click::Export,
    Click::Import,
    Click::Cancel,
];

#[derive(Debug, Clone, Default)]
pub struct KeysPopup {
    pub stage: KeysStage,
    /// Index into [`STATUS_BUTTONS`]; `None` means "Close".
    pub focus: Option<usize>,
}

impl KeysPopup {
    pub fn focused(&self) -> Click {
        self.focus
            .and_then(|at| STATUS_BUTTONS.get(at).copied())
            .unwrap_or(Click::Cancel)
    }

    pub fn step(&mut self, delta: isize) {
        let count = STATUS_BUTTONS.len() as isize;
        let at = self.focus.unwrap_or(STATUS_BUTTONS.len() - 1) as isize;
        self.focus = Some((at + delta).rem_euclid(count) as usize);
    }
}

/// What is wrong with the passphrases typed for an export, if anything.
pub fn check_passphrase(passphrase: &str, again: &str) -> Result<(), &'static str> {
    if passphrase.is_empty() {
        return Err(
            "Enter a passphrase: the file holds the keys to your messages, and the \
             passphrase is all that protects it.",
        );
    }
    if passphrase.chars().count() < PASSPHRASE_MIN {
        return Err("That passphrase is too short: use at least 8 characters.");
    }
    if passphrase != again {
        return Err("The two passphrases differ. Type them again.");
    }
    Ok(())
}

/// Where a key file typed by the user is: `~` is the home folder, and a bare name
/// goes to (or comes from) the download folder.
pub fn key_file_path(typed: &str, download_dir: &Path) -> Option<PathBuf> {
    let typed = typed.trim();
    if typed.is_empty() || typed.contains('\0') {
        return None;
    }
    let path = if typed == "~" || typed.starts_with("~/") || Path::new(typed).is_absolute() {
        crate::files::resolve_dir(typed)
    } else {
        download_dir.join(typed)
    };
    // A folder is not a file name.
    (!typed.ends_with('/') && path.file_name().is_some()).then_some(path)
}

/// The file an export is offered to go to: named after the day, and not there yet.
pub fn default_export_path(
    download_dir: &Path,
    date: &str,
    exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    let first = download_dir.join(format!("matrix-rat-keys-{date}.txt"));
    if !exists(&first) {
        return first;
    }
    (2..100)
        .map(|n| download_dir.join(format!("matrix-rat-keys-{date}-{n}.txt")))
        .find(|path| !exists(path))
        .unwrap_or(first)
}

// ----- the words for the key backup ---------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Warn,
    Plain,
}

/// What the settings menu shows on its key backup row, and how it is to be read:
/// a warning only for what needs doing, plain for what is in progress or unknown.
pub fn backup_setting(security: Option<Security>) -> (&'static str, Tone) {
    let Some(security) = security else {
        return ("unknown", Tone::Plain);
    };
    match (security.backup, security.recovery) {
        (_, RecoveryStatus::NeedsKey) => ("this device needs the recovery key", Tone::Warn),
        (BackupStatus::Enabled, RecoveryStatus::NotSetUp) => {
            ("on, but recovery is not set up", Tone::Warn)
        }
        (BackupStatus::Enabled, _) => ("on", Tone::Good),
        (BackupStatus::Enabling, _) => ("turning on…", Tone::Plain),
        (BackupStatus::Downloading, _) => ("downloading keys…", Tone::Plain),
        (BackupStatus::Disabling, _) => ("turning off…", Tone::Plain),
        (BackupStatus::Inactive, _) => ("not in use on this device", Tone::Plain),
    }
}

pub fn backup_setting_text(security: Option<Security>) -> &'static str {
    backup_setting(security).0
}

/// The recovery key can be entered: the account has one and this device still lacks
/// what it unlocks.
pub fn wants_recovery_key(security: Option<Security>) -> bool {
    security.is_some_and(|security| security.recovery == RecoveryStatus::NeedsKey)
}

/// Why the recovery key cannot be entered right now.
pub fn no_recovery_key_reason(security: Option<Security>) -> &'static str {
    match security.map(|security| security.recovery) {
        Some(RecoveryStatus::Ready) => {
            "Nothing to unlock: this device already has everything the recovery key gives."
        }
        Some(RecoveryStatus::NotSetUp) => {
            "Your account has no recovery key. Set up recovery in Element (Settings → \
             Encryption)."
        }
        _ => "The state of recovery is not known yet. Try again in a moment.",
    }
}

fn keys_count(keys: u64) -> String {
    match keys {
        1 => "1 message key".to_owned(),
        n => format!("{n} message keys"),
    }
}

/// The state of the key backup and of recovery, in plain words: what there is, what
/// this device can do with it, and what to do about what is missing.
///
/// Three things are known, from three places, and none is taken for another:
/// `server` is what `GET /room_keys/version` answered just now (`Unknown` when it
/// did not answer: then nothing is said about the server's side); the backup state
/// in `security` is this device's own, kept without asking the server; `upload` is
/// what the SDK reported of its uploads while the popup was open. Which backup this
/// device uploads to cannot be compared with the server's (matrix-sdk 0.19.1 does
/// not tell its version), and that is said where it matters.
pub fn backup_lines(
    security: Option<Security>,
    server: &ServerBackup,
    upload: BackupUpload,
) -> Vec<(Tone, String)> {
    let Some(security) = security else {
        return vec![(Tone::Plain, "Checking…".to_owned())];
    };
    let mut lines = Vec::new();
    // What the server has.
    match server {
        ServerBackup::Present { keys, version } => lines.push((
            Tone::Plain,
            format!(
                "The server has a key backup (version {}): {}.",
                crate::text::truncate(&sanitize_line(version), 16),
                keys_count(*keys)
            ),
        )),
        ServerBackup::Missing => lines.push((
            Tone::Warn,
            "There is no key backup on the server: message keys exist only on your devices."
                .to_owned(),
        )),
        ServerBackup::Unknown => lines.push((
            Tone::Plain,
            "Could not ask the server whether a key backup is there.".to_owned(),
        )),
    }
    // What this device does with it.
    match (security.backup, server) {
        (BackupStatus::Enabled, ServerBackup::Present { .. }) => {
            lines.push((
                Tone::Good,
                "This device has a backup key: it saves new message keys to its backup and \
                 can read old ones from it."
                    .to_owned(),
            ));
            match upload {
                BackupUpload::Idle => {
                    lines.push((Tone::Good, "No keys are waiting to be uploaded.".to_owned()))
                }
                BackupUpload::Uploading { done, total } => lines.push((
                    Tone::Plain,
                    format!("Uploading keys: {done} of {total} are in the backup so far."),
                )),
                BackupUpload::Failed => lines.push((
                    Tone::Warn,
                    "The last upload of keys failed. It is tried again by itself.".to_owned(),
                )),
                BackupUpload::Unknown => lines.push((
                    Tone::Plain,
                    "Whether all keys have been uploaded has not been reported yet.".to_owned(),
                )),
            }
        }
        (BackupStatus::Enabled, ServerBackup::Unknown) => lines.push((
            Tone::Plain,
            "This device is set up to save its message keys to a backup; whether the \
             server still has that backup could not be checked."
                .to_owned(),
        )),
        (BackupStatus::Enabled, ServerBackup::Missing) => lines.push((
            Tone::Warn,
            "This device still expects a backup to save its keys to. It was probably \
             deleted from another device; check the key storage in Element."
                .to_owned(),
        )),
        (BackupStatus::Enabling, _) => lines.push((
            Tone::Plain,
            "This device is connecting to the backup…".to_owned(),
        )),
        (BackupStatus::Downloading, _) => lines.push((
            Tone::Plain,
            "This device is downloading keys from the backup…".to_owned(),
        )),
        (BackupStatus::Disabling, _) => {
            lines.push((Tone::Plain, "The backup is being turned off…".to_owned()))
        }
        (BackupStatus::Inactive, ServerBackup::Missing) => lines.push((
            Tone::Plain,
            "matrix-rat cannot create a backup yet: use Element (Settings → Encryption)."
                .to_owned(),
        )),
        (BackupStatus::Inactive, ServerBackup::Unknown) => lines.push((
            Tone::Plain,
            "This device is not using a key backup.".to_owned(),
        )),
        (BackupStatus::Inactive, ServerBackup::Present { .. }) => lines.push((
            Tone::Warn,
            if security.recovery == RecoveryStatus::NeedsKey {
                "This device cannot read the backup: enter your recovery key."
            } else if security.device != DeviceTrust::Verified {
                "This device cannot read the backup: verify it with another of your devices."
            } else {
                "This device is not using the backup. Check the key storage in Element \
                 (Settings → Encryption)."
            }
            .to_owned(),
        )),
    }
    // Recovery: getting at all this from a new device.
    let said_above =
        security.backup == BackupStatus::Inactive && matches!(server, ServerBackup::Present { .. });
    match security.recovery {
        RecoveryStatus::Ready => lines.push((
            Tone::Good,
            "Recovery is set up: your recovery key unlocks all this on a new device.".to_owned(),
        )),
        RecoveryStatus::NeedsKey if said_above => {}
        RecoveryStatus::NeedsKey => lines.push((
            Tone::Warn,
            "This device does not have all of your account's keys: enter your recovery key."
                .to_owned(),
        )),
        RecoveryStatus::NotSetUp => lines.push((
            Tone::Warn,
            "Recovery is not set up: there is no recovery key that would unlock your keys \
             on a new device. Set it up in Element (Settings → Encryption)."
                .to_owned(),
        )),
        RecoveryStatus::Unknown => {
            lines.push((Tone::Plain, "Checking recovery…".to_owned()));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::Identity;

    fn security(backup: BackupStatus, recovery: RecoveryStatus, device: DeviceTrust) -> Security {
        Security {
            device,
            identity: Identity::Present,
            backup,
            recovery,
            ..Security::default()
        }
    }

    fn texts(lines: &[(Tone, String)]) -> String {
        lines
            .iter()
            .map(|(tone, text)| format!("{tone:?}: {text}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn present(keys: u64) -> ServerBackup {
        ServerBackup::Present {
            keys,
            version: "7".into(),
        }
    }

    #[test]
    fn backup_status_in_plain_words() {
        use BackupStatus as B;
        use BackupUpload as U;
        use DeviceTrust as D;
        use RecoveryStatus as R;
        let well = security(B::Enabled, R::Ready, D::Verified);
        // All is well, and seen to be: the server has a backup, this device uses one,
        // and its uploads are through.
        assert_eq!(
            texts(&backup_lines(Some(well), &present(1234), U::Idle)),
            "Plain: The server has a key backup (version 7): 1234 message keys.\n\
             Good: This device has a backup key: it saves new message keys to its backup and \
             can read old ones from it.\n\
             Good: No keys are waiting to be uploaded.\n\
             Good: Recovery is set up: your recovery key unlocks all this on a new device."
        );
        assert_eq!(backup_setting(Some(well)), ("on", Tone::Good));
        // Uploads not finished, failed, or not heard of: said, never passed over.
        let lines = texts(&backup_lines(
            Some(well),
            &present(10),
            U::Uploading { done: 3, total: 40 },
        ));
        assert!(
            lines.contains("Plain: Uploading keys: 3 of 40 are in the backup so far."),
            "{lines}"
        );
        assert!(!lines.contains("No keys are waiting"), "{lines}");
        let lines = texts(&backup_lines(Some(well), &present(10), U::Failed));
        assert!(
            lines.contains("Warn: The last upload of keys failed."),
            "{lines}"
        );
        let lines = texts(&backup_lines(Some(well), &present(10), U::Unknown));
        assert!(
            lines.contains("Plain: Whether all keys have been uploaded"),
            "{lines}"
        );
        // The server did not answer: nothing good is said about its side, and nothing
        // is taken to exist.
        let lines = backup_lines(Some(well), &ServerBackup::Unknown, U::Idle);
        let text = texts(&lines);
        assert!(
            text.starts_with("Plain: Could not ask the server"),
            "{text}"
        );
        assert!(
            text.contains(
                "Plain: This device is set up to save its message keys to a backup; whether"
            ),
            "{text}"
        );
        assert_eq!(
            lines.iter().filter(|(tone, _)| *tone == Tone::Good).count(),
            1,
            "only recovery, which is known locally: {text}"
        );
        // Enabled here, gone there.
        let text = texts(&backup_lines(Some(well), &ServerBackup::Missing, U::Idle));
        assert!(
            text.starts_with("Warn: There is no key backup on the server"),
            "{text}"
        );
        assert!(
            text.contains("Warn: This device still expects a backup"),
            "{text}"
        );

        // A new device that has not been given the keys.
        let new = security(B::Inactive, R::NeedsKey, D::Unverified);
        assert_eq!(
            texts(&backup_lines(Some(new), &present(1), U::Unknown)),
            "Plain: The server has a key backup (version 7): 1 message key.\n\
             Warn: This device cannot read the backup: enter your recovery key."
        );
        assert!(wants_recovery_key(Some(new)));
        assert_eq!(
            backup_setting(Some(new)),
            ("this device needs the recovery key", Tone::Warn)
        );
        // The same device while the server cannot be asked: no backup is assumed, and
        // the recovery key is asked for on its own grounds.
        let text = texts(&backup_lines(Some(new), &ServerBackup::Unknown, U::Unknown));
        assert!(!text.contains("cannot read the backup"), "{text}");
        assert!(
            text.contains("Plain: This device is not using a key backup."),
            "{text}"
        );
        assert!(
            text.ends_with("Warn: This device does not have all of your account's keys: enter your recovery key."),
            "{text}"
        );
        // Backup in use, but one of the other keys is missing here.
        let partial = security(B::Enabled, R::NeedsKey, D::Verified);
        let text = texts(&backup_lines(Some(partial), &present(5), U::Idle));
        assert!(
            text.ends_with(
                "Warn: This device does not have all of your account's keys: enter your \
                 recovery key."
            ),
            "{text}"
        );
        // An account without backup and recovery: say where to get them, offer nothing.
        let bare = security(B::Inactive, R::NotSetUp, D::Verified);
        let lines = texts(&backup_lines(
            Some(bare),
            &ServerBackup::Missing,
            U::Unknown,
        ));
        assert!(
            lines.starts_with("Warn: There is no key backup on the server"),
            "{lines}"
        );
        assert!(
            lines.contains("matrix-rat cannot create a backup yet: use Element"),
            "{lines}"
        );
        assert!(
            lines.contains("Recovery is not set up") && lines.contains("Set it up in Element"),
            "{lines}"
        );
        assert!(!wants_recovery_key(Some(bare)));
        assert!(no_recovery_key_reason(Some(bare)).contains("no recovery key"));
        // Not in use, in progress, unknown: nothing to be alarmed about by itself.
        assert_eq!(
            backup_setting(Some(bare)),
            ("not in use on this device", Tone::Plain)
        );
        for backup in [B::Enabling, B::Downloading, B::Disabling] {
            let (_, tone) = backup_setting(Some(security(backup, R::Ready, D::Verified)));
            assert_eq!(tone, Tone::Plain, "{backup:?}");
        }
        assert_eq!(backup_setting(None), ("unknown", Tone::Plain));
        assert_eq!(
            backup_setting(Some(security(B::Enabled, R::NotSetUp, D::Verified))).1,
            Tone::Warn
        );
        // A backup exists, there is no recovery, and this device is not verified.
        let unverified = security(B::Inactive, R::NotSetUp, D::Unverified);
        let lines = texts(&backup_lines(Some(unverified), &present(9), U::Unknown));
        assert!(
            lines.contains("This device cannot read the backup: verify it"),
            "{lines}"
        );
        let lines = texts(&backup_lines(
            Some(security(B::Inactive, R::Unknown, D::Unknown)),
            &ServerBackup::Unknown,
            U::Unknown,
        ));
        assert!(lines.ends_with("Plain: Checking recovery…"), "{lines}");
        // A version from the server is shown harmless and short.
        let odd = ServerBackup::Present {
            keys: 2,
            version: format!("9\u{1b}[31m{}", "9".repeat(60)),
        };
        let text = texts(&backup_lines(Some(well), &odd, U::Idle));
        assert!(
            !text.contains('\u{1b}') && text.lines().next().unwrap().len() < 90,
            "{text}"
        );
        // Nothing known at all.
        assert_eq!(
            texts(&backup_lines(None, &ServerBackup::Unknown, U::Unknown)),
            "Plain: Checking…"
        );
        assert!(!wants_recovery_key(None));
    }

    #[test]
    fn passphrases_are_checked() {
        assert!(check_passphrase("", "").unwrap_err().contains("Enter a"));
        assert!(
            check_passphrase("short", "short")
                .unwrap_err()
                .contains("too short")
        );
        assert!(
            check_passphrase("long enough", "long enoug")
                .unwrap_err()
                .contains("differ")
        );
        assert_eq!(check_passphrase("long enough", "long enough"), Ok(()));
        // Characters, not bytes.
        assert_eq!(check_passphrase("äöüäöüäö", "äöüäöüäö"), Ok(()));
    }

    #[test]
    fn key_file_names() {
        let downloads = Path::new("/home/me/Downloads");
        assert_eq!(
            key_file_path("keys.txt", downloads),
            Some(downloads.join("keys.txt"))
        );
        assert_eq!(
            key_file_path(" /tmp/keys.txt ", downloads),
            Some(PathBuf::from("/tmp/keys.txt"))
        );
        assert_eq!(key_file_path("", downloads), None);
        assert_eq!(key_file_path("  ", downloads), None);
        assert_eq!(key_file_path("/tmp/", downloads), None);
        assert_eq!(key_file_path("/", downloads), None);
        let home = key_file_path("~/keys.txt", downloads).unwrap();
        assert!(home.is_absolute() && home.ends_with("keys.txt"), "{home:?}");
        // The default name carries the day and never names a file that is there.
        assert_eq!(
            default_export_path(downloads, "2026-11-14", |_| false),
            downloads.join("matrix-rat-keys-2026-11-14.txt")
        );
        let taken = downloads.join("matrix-rat-keys-2026-11-14.txt");
        assert_eq!(
            default_export_path(downloads, "2026-11-14", |path| path == taken),
            downloads.join("matrix-rat-keys-2026-11-14-2.txt")
        );
    }

    fn session(id: &str, trust: SessionTrust) -> Session {
        Session {
            device_id: id.into(),
            name: None,
            last_seen: None,
            last_ip: None,
            current: false,
            trust,
        }
    }

    #[test]
    fn unverified_sessions_are_counted_and_named() {
        let mut list = SessionList {
            sessions: vec![session("A", SessionTrust::Verified)],
            vouched: true,
        };
        assert_eq!(warning(&list), None);
        list.sessions.push(session("B", SessionTrust::Unverified));
        assert!(warning(&list).unwrap().starts_with("1 session is not"));
        // A session without keys cannot be verified, so it counts too.
        list.sessions.push(session("C", SessionTrust::NoKeys));
        assert!(warning(&list).unwrap().starts_with("2 sessions are not"));

        // Checked here: good news may be called that.
        let all = |sessions: Vec<Session>, vouched| SessionList { sessions, vouched };
        let verified = || session("A", SessionTrust::Verified);
        assert_eq!(
            session_lines(&all(vec![verified(), verified()], true)),
            [(Tone::Good, "All 2 sessions are verified.".to_owned())]
        );
        assert_eq!(session_lines(&list)[0].0, Tone::Warn);
        assert_eq!(trust_text(SessionTrust::Verified, true), "verified");
        assert_eq!(trust_tone(SessionTrust::Verified, true), Tone::Good);
        assert_eq!(trust_tone(SessionTrust::NoKeys, true), Tone::Warn);
        // Not checked here: the server's word, as that, and nothing in a good tone.
        for sessions in [
            vec![verified(), verified()],
            vec![verified(), session("B", SessionTrust::Unverified)],
            vec![verified()],
        ] {
            let lines = session_lines(&all(sessions, false));
            assert!(
                lines.iter().all(|(tone, _)| *tone != Tone::Good),
                "{lines:?}"
            );
            assert!(
                lines[0].1.starts_with("This device is not verified"),
                "{lines:?}"
            );
            assert!(lines[1].1.contains("The server shows"), "{lines:?}");
            assert!(
                !lines.iter().any(|(_, text)| text.contains("are verified.")),
                "{lines:?}"
            );
        }
        assert_eq!(trust_text(SessionTrust::Verified, false), "says verified");
        assert_eq!(
            trust_text(SessionTrust::Unverified, false),
            "says not verified"
        );
        for trust in [
            SessionTrust::Verified,
            SessionTrust::Unverified,
            SessionTrust::NoKeys,
        ] {
            assert_eq!(trust_tone(trust, false), Tone::Plain);
        }
        // No sessions at all (whatever went wrong): never "all 0 are verified".
        for vouched in [true, false] {
            assert_eq!(
                session_lines(&all(Vec::new(), vouched)),
                [(Tone::Plain, "The server lists no sessions.".to_owned())]
            );
        }

        let mut named = session("PHONE", SessionTrust::Verified);
        assert_eq!(label(&named), "PHONE");
        named.name = Some("My\u{1b}[31m phone\n".into());
        let shown = label(&named);
        assert!(shown.ends_with("(PHONE)") && !shown.contains('\u{1b}') && !shown.contains('\n'));
        named.name = Some("  ".into());
        assert_eq!(label(&named), "PHONE");
        assert_eq!(last_seen(&named), "never seen");
        named.last_seen = Some(1_700_000_000_000);
        assert!(
            last_seen(&named).starts_with("2023-11-1"),
            "{}",
            last_seen(&named)
        );
    }

    #[test]
    fn selection_follows_the_session_not_the_row() {
        let mut sessions = vec![
            session("A", SessionTrust::Verified),
            session("B", SessionTrust::Unverified),
            session("C", SessionTrust::Unverified),
        ];
        let mut popup = SessionsPopup::default();
        assert_eq!(popup.selected_index(&sessions), 0);
        popup.step(&sessions, 5);
        assert_eq!(popup.selected.as_deref(), Some("C"));
        popup.step(&sessions, -1);
        assert_eq!(popup.selected.as_deref(), Some("B"));
        // The list changes under it: still B.
        sessions.remove(0);
        assert_eq!(popup.selected_index(&sessions), 0);
        assert_eq!(popup.selected_session(&sessions).unwrap().device_id, "B");
        // B is gone: the first one.
        sessions.remove(0);
        assert_eq!(popup.selected_session(&sessions).unwrap().device_id, "C");
        popup.step(&[], 1);
    }

    #[test]
    fn debug_output_hides_what_was_typed() {
        let mut export = ExportForm::new(Path::new("/home/someone/keys.txt"));
        export.passphrase.set("hunter2");
        export.again.set("hunter2");
        let mut import = ImportForm::new();
        import.path.set("/home/someone/keys.txt");
        import.passphrase.set("hunter2");
        let mut password = LineEdit::secret();
        password.set("hunter2");
        let sign_out = SignOut {
            device_id: "PHONE".into(),
            label: "secret name".into(),
            request: None,
            password: Some(password),
            web: Some(Some("https://account.example.org".into())),
            error: None,
            on_confirm: false,
        };
        let rename = Rename {
            device_id: "PHONE".into(),
            name: LineEdit::from("secret name"),
            request: None,
            error: None,
        };
        let text = format!("{export:?}{import:?}{sign_out:?}{rename:?}");
        for secret in ["hunter2", "someone", "secret name", "example.org"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
    }
}
