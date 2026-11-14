//! What the UI knows about the account's sessions (devices) and its message keys: the
//! two popups ("Sessions", "Key backup and recovery") and the words they use.

use std::path::{Path, PathBuf};

use crate::lineedit::LineEdit;
use crate::matrix::{
    BackupStatus, DeviceTrust, RecoveryStatus, Security, ServerBackup, Session, SessionList,
    SessionTrust,
};
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
    /// The worker has been told that a popup is open.
    pub watching: bool,
    /// Numbers the requests; answers quote theirs back.
    pub next_request: u64,
    /// The room and the reason of the message "try again" was last asked for.
    pub retried: Option<(String, crate::model::UtdWhy)>,
}

impl DevicesState {
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

pub fn trust_text(trust: SessionTrust) -> &'static str {
    match trust {
        SessionTrust::Verified => "verified",
        SessionTrust::Unverified => "not verified",
        SessionTrust::NoKeys => "no encryption keys",
    }
}

/// The line above the list: how many sessions the account's identity does not vouch
/// for. `None` while there is nothing to say.
pub fn warning(list: &SessionList) -> Option<String> {
    let unverified = list
        .sessions
        .iter()
        .filter(|session| session.trust != SessionTrust::Verified)
        .count();
    match unverified {
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

/// What the settings menu shows on its key backup row.
pub fn backup_setting_text(security: Option<Security>) -> &'static str {
    let Some(security) = security else {
        return "unknown";
    };
    match (security.backup, security.recovery) {
        (_, RecoveryStatus::NeedsKey) => "this device needs the recovery key",
        (BackupStatus::Enabled, RecoveryStatus::NotSetUp) => "on, but recovery is not set up",
        (BackupStatus::Enabled, _) => "on",
        (BackupStatus::Enabling, _) => "turning on…",
        (BackupStatus::Downloading, _) => "downloading keys…",
        (BackupStatus::Disabling, _) => "turning off…",
        (BackupStatus::Inactive, _) => "not in use on this device",
    }
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
pub fn backup_lines(security: Option<Security>, server: ServerBackup) -> Vec<(Tone, String)> {
    let Some(security) = security else {
        return vec![(Tone::Plain, "Checking…".to_owned())];
    };
    let mut lines = Vec::new();
    let using = security.backup == BackupStatus::Enabled;
    // What the server has.
    match server {
        ServerBackup::Present { keys } if using => lines.push((
            Tone::Good,
            format!("Your message keys are backed up on the server ({keys} so far)."),
        )),
        ServerBackup::Present { keys } => lines.push((
            Tone::Plain,
            format!(
                "There is a key backup on the server, holding {}.",
                keys_count(keys)
            ),
        )),
        ServerBackup::Missing => lines.push((
            Tone::Warn,
            "There is no key backup on the server: message keys exist only on your devices."
                .to_owned(),
        )),
        ServerBackup::Unknown if using => lines.push((
            Tone::Good,
            "Your message keys are backed up on the server.".to_owned(),
        )),
        ServerBackup::Unknown => lines.push((
            Tone::Plain,
            "The server could not be asked about a key backup just now.".to_owned(),
        )),
    }
    // What this device does with it.
    let exists = !matches!(server, ServerBackup::Missing);
    match security.backup {
        BackupStatus::Enabled => lines.push((
            Tone::Good,
            "This device has the backup key: it saves new keys there and can read old ones."
                .to_owned(),
        )),
        BackupStatus::Enabling => lines.push((
            Tone::Plain,
            "This device is connecting to the backup…".to_owned(),
        )),
        BackupStatus::Downloading => lines.push((
            Tone::Plain,
            "This device is downloading keys from the backup…".to_owned(),
        )),
        BackupStatus::Disabling => {
            lines.push((Tone::Plain, "The backup is being turned off…".to_owned()))
        }
        BackupStatus::Inactive if !exists => lines.push((
            Tone::Plain,
            "matrix-rat cannot create a backup yet: use Element (Settings → Encryption)."
                .to_owned(),
        )),
        BackupStatus::Inactive if security.recovery == RecoveryStatus::NeedsKey => lines.push((
            Tone::Warn,
            "This device cannot read the backup: enter your recovery key.".to_owned(),
        )),
        BackupStatus::Inactive if security.device != DeviceTrust::Verified => lines.push((
            Tone::Warn,
            "This device cannot read the backup: verify it with another of your devices."
                .to_owned(),
        )),
        BackupStatus::Inactive => lines.push((
            Tone::Warn,
            "This device is not using the backup. Check the key storage in Element \
             (Settings → Encryption)."
                .to_owned(),
        )),
    }
    // Recovery: getting at all this from a new device.
    match security.recovery {
        RecoveryStatus::Ready => lines.push((
            Tone::Good,
            "Recovery is set up: your recovery key unlocks all this on a new device.".to_owned(),
        )),
        // Said above already, where the backup is what is missing.
        RecoveryStatus::NeedsKey if security.backup == BackupStatus::Inactive && exists => {}
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

    #[test]
    fn backup_status_in_plain_words() {
        use BackupStatus as B;
        use DeviceTrust as D;
        use RecoveryStatus as R;
        // All is well.
        let all = backup_lines(
            Some(security(B::Enabled, R::Ready, D::Verified)),
            ServerBackup::Present { keys: 1234 },
        );
        assert_eq!(
            texts(&all),
            "Good: Your message keys are backed up on the server (1234 so far).\n\
             Good: This device has the backup key: it saves new keys there and can read old ones.\n\
             Good: Recovery is set up: your recovery key unlocks all this on a new device."
        );
        assert_eq!(
            backup_setting_text(Some(security(B::Enabled, R::Ready, D::Verified))),
            "on"
        );
        // A new device that has not been given the keys.
        let new = security(B::Inactive, R::NeedsKey, D::Unverified);
        assert_eq!(
            texts(&backup_lines(Some(new), ServerBackup::Present { keys: 1 })),
            "Plain: There is a key backup on the server, holding 1 message key.\n\
             Warn: This device cannot read the backup: enter your recovery key."
        );
        assert!(wants_recovery_key(Some(new)));
        assert_eq!(
            backup_setting_text(Some(new)),
            "this device needs the recovery key"
        );
        // Backup in use, but one of the other keys is missing here.
        let partial = security(B::Enabled, R::NeedsKey, D::Verified);
        let lines = backup_lines(Some(partial), ServerBackup::Present { keys: 5 });
        assert!(
            texts(&lines).ends_with(
                "Warn: This device does not have all of your account's keys: enter your \
                 recovery key."
            ),
            "{lines:?}"
        );
        // An account without backup and recovery: say where to get them, offer nothing.
        let bare = security(B::Inactive, R::NotSetUp, D::Verified);
        let lines = texts(&backup_lines(Some(bare), ServerBackup::Missing));
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
        assert_eq!(backup_setting_text(Some(bare)), "not in use on this device");
        // A backup exists, there is no recovery, and this device is not verified.
        let unverified = security(B::Inactive, R::NotSetUp, D::Unverified);
        let lines = texts(&backup_lines(
            Some(unverified),
            ServerBackup::Present { keys: 9 },
        ));
        assert!(
            lines.contains("This device cannot read the backup: verify it"),
            "{lines}"
        );
        // Offline: only what this device knows by itself.
        let lines = texts(&backup_lines(
            Some(security(B::Enabled, R::Ready, D::Verified)),
            ServerBackup::Unknown,
        ));
        assert!(
            lines.starts_with("Good: Your message keys are backed up on the server.\n"),
            "{lines}"
        );
        let lines = texts(&backup_lines(
            Some(security(B::Inactive, R::Unknown, D::Unknown)),
            ServerBackup::Unknown,
        ));
        assert!(
            lines.starts_with("Plain: The server could not be asked"),
            "{lines}"
        );
        assert!(lines.ends_with("Plain: Checking recovery…"), "{lines}");
        // Nothing known at all.
        assert_eq!(
            texts(&backup_lines(None, ServerBackup::Unknown)),
            "Plain: Checking…"
        );
        assert_eq!(backup_setting_text(None), "unknown");
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
