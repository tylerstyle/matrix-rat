//! The app's side of the sessions popup, the key backup popup and "try again" on a
//! message that could not be decrypted: actions, keys and worker events.

use super::App;
use crate::devices::{
    Click, ExportForm, ImportForm, KeysStage, Rename, SessionStage, SessionsPopup, SignOut,
    check_passphrase, default_export_path, key_file_path, label, no_recovery_key_reason,
    wants_recovery_key,
};
use crate::lineedit::{EditKeys, LineEdit};
use crate::matrix::{
    DeviceTrust, DevicesCmd, DevicesEvent, MatrixCmd, Retried, SessionTrust, SignOutOutcome,
    VerifyCmd,
};
use crate::modal::{Modal, RecoveryForm};
use crate::model::{ItemRef, MsgKind};
use crate::verify::VerifyPopup;
use crossterm::event::{KeyCode, KeyModifiers};

impl App {
    /// There is an account to ask; says so if not.
    fn devices_usable(&mut self) -> bool {
        if self.matrix.is_none() {
            self.show_toast("The demo has no account to do this with.");
            return false;
        }
        if self.logged_out || self.user_id.is_none() {
            self.show_toast("Log in first");
            return false;
        }
        true
    }

    pub(super) fn open_sessions(&mut self) {
        if !self.devices_usable() {
            return;
        }
        self.open_modal(Modal::Sessions(Box::default()));
        self.devices_watch_begin();
    }

    pub(super) fn open_keys(&mut self) {
        if !self.devices_usable() {
            return;
        }
        self.open_modal(Modal::Keys(Box::default()));
        self.devices_watch_begin();
    }

    fn open_keys_at(&mut self, click: Click) {
        self.open_keys();
        if matches!(self.modal, Some(Modal::Keys(_))) {
            self.devices_click(click);
        }
    }

    pub(super) fn open_key_export(&mut self) {
        self.open_keys_at(Click::Export);
    }

    pub(super) fn open_key_import(&mut self) {
        self.open_keys_at(Click::Import);
    }

    /// One of the two popups opened: has the worker keep the list of sessions and
    /// the state of the backup current, under a new number. Whatever was known from
    /// an earlier opening is dropped, and so is news that still quotes its number
    /// (also when the popup was closed and opened again before the worker heard).
    fn devices_watch_begin(&mut self) {
        self.devices.next_watch += 1;
        let watch = self.devices.next_watch;
        self.devices.watch = Some(watch);
        self.devices.list = None;
        self.devices.backup = Default::default();
        self.devices.upload = Default::default();
        self.send_matrix(MatrixCmd::Devices(DevicesCmd::Watch(Some(watch))));
    }

    /// Called a few times a second: tells the worker to stop watching once neither
    /// popup is open (popups close in many places).
    pub(super) fn devices_watch(&mut self) {
        let open = matches!(self.modal, Some(Modal::Sessions(_) | Modal::Keys(_)));
        if !open && self.devices.watch.take().is_some() {
            self.send_matrix(MatrixCmd::Devices(DevicesCmd::Watch(None)));
        }
    }

    /// A control of one of the two popups, clicked or chosen by key.
    pub(super) fn devices_click(&mut self, click: Click) {
        match &self.modal {
            Some(Modal::Sessions(_)) => self.sessions_click(click),
            Some(Modal::Keys(_)) => self.keys_click(click),
            _ => {}
        }
    }

    // ----- sessions -------------------------------------------------------------------

    fn sessions_click(&mut self, click: Click) {
        let Some(Modal::Sessions(popup)) = &mut self.modal else {
            return;
        };
        let sessions = self.devices.sessions();
        let selected = popup.selected_session(sessions).cloned();
        match (&mut popup.stage, click) {
            (SessionStage::List, Click::Row(index)) => popup.select(sessions, index),
            (SessionStage::List, Click::Addresses) => {
                popup.show_addresses = !popup.show_addresses;
            }
            (SessionStage::List, Click::Cancel) => self.modal = None,
            (SessionStage::List, Click::Rename) => {
                if let Some(session) = selected {
                    let mut name = LineEdit::new().with_max_bytes(SessionsPopup::NAME_MAX_BYTES);
                    name.set(
                        session
                            .name
                            .as_deref()
                            .map(crate::text::sanitize_line)
                            .unwrap_or_default(),
                    );
                    popup.stage = SessionStage::Rename(Rename {
                        device_id: session.device_id,
                        name,
                        request: None,
                        error: None,
                    });
                }
            }
            (SessionStage::List, Click::SignOut) => match selected {
                // Ending this session is logging out, with all that follows from it.
                Some(session) if session.current => self.show_toast(
                    "This is the session you are using. Use Log out (in the settings) for it.",
                ),
                Some(session) => {
                    popup.stage = SessionStage::SignOut(SignOut {
                        label: label(&session),
                        device_id: session.device_id,
                        request: None,
                        password: None,
                        web: None,
                        error: None,
                        on_confirm: false,
                    });
                }
                None => {}
            },
            (SessionStage::List, Click::Verify) => {
                if let Some(session) = selected {
                    self.verify_session(session.device_id, session.current, session.trust);
                }
            }
            (SessionStage::Rename(_) | SessionStage::SignOut(_), Click::Cancel) => {
                // A request that is under way goes on; its outcome is told in a toast.
                popup.stage = SessionStage::List;
            }
            (SessionStage::Rename(_), Click::Confirm) => self.submit_rename(),
            (SessionStage::SignOut(_), Click::Confirm) => self.submit_sign_out(),
            (SessionStage::SignOut(sign_out), Click::OpenWeb) => {
                if let Some(Some(url)) = &sign_out.web {
                    super::open_in_browser(url);
                    self.show_toast("Opened in your browser");
                }
            }
            _ => {}
        }
    }

    /// Starts a verification with one other session of the account, if that makes
    /// sense: one of the two has to be verified already.
    fn verify_session(&mut self, device_id: String, current: bool, trust: SessionTrust) {
        if current {
            self.modal = None;
            return self.start_verification();
        }
        let own_verified = self
            .verify
            .security
            .is_some_and(|security| security.device == DeviceTrust::Verified);
        match (own_verified, trust) {
            (_, SessionTrust::NoKeys) => self
                .show_toast("That session has no encryption keys, so there is nothing to verify."),
            (true, SessionTrust::Verified) => self.show_toast("That session is verified already."),
            (false, SessionTrust::Unverified) => self.show_toast(
                "This device is not verified itself, so it cannot vouch for another. Verify \
                 this device first.",
            ),
            _ if self.verify.active().is_some() => {
                self.verify.deferred = false;
                self.open_modal(Modal::Verify(VerifyPopup::flow()));
            }
            _ => {
                self.modal = None;
                self.verify.dismissed = None;
                self.send_matrix(MatrixCmd::Verification(VerifyCmd::RequestDevice {
                    device_id,
                }));
            }
        }
    }

    fn submit_rename(&mut self) {
        let request = self.devices.next_request + 1;
        let Some(Modal::Sessions(popup)) = &mut self.modal else {
            return;
        };
        let SessionStage::Rename(rename) = &mut popup.stage else {
            return;
        };
        if rename.request.is_some() {
            return;
        }
        let name = rename.name.as_str().trim().to_owned();
        if name.is_empty() {
            rename.error = Some("Enter a name.".to_owned());
            return;
        }
        rename.request = Some(request);
        rename.error = None;
        let device_id = rename.device_id.clone();
        self.devices.next_request = request;
        self.send_matrix(MatrixCmd::Devices(DevicesCmd::Rename {
            request,
            device_id,
            name,
        }));
    }

    /// Sends the request to sign a session out: the first time bare (the server then
    /// says what it wants), after that with the password that was typed. The password
    /// leaves the popup as it is sent.
    fn submit_sign_out(&mut self) {
        let request = self.devices.next_request + 1;
        let Some(Modal::Sessions(popup)) = &mut self.modal else {
            return;
        };
        let SessionStage::SignOut(sign_out) = &mut popup.stage else {
            return;
        };
        if sign_out.request.is_some() || sign_out.web.is_some() {
            return;
        }
        let password = match &mut sign_out.password {
            Some(typed) if typed.is_empty() => {
                sign_out.error = Some("Enter your account password.".to_owned());
                return;
            }
            Some(typed) => Some(typed.take_secret()),
            None => None,
        };
        sign_out.request = Some(request);
        sign_out.error = None;
        let device_id = sign_out.device_id.clone();
        self.devices.next_request = request;
        self.send_matrix(MatrixCmd::Devices(DevicesCmd::SignOut {
            request,
            device_id,
            password,
        }));
    }

    pub(super) fn on_sessions_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Some(Modal::Sessions(popup)) = &mut self.modal else {
            return;
        };
        let sessions = self.devices.sessions();
        let plain = !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let click = match &mut popup.stage {
            SessionStage::List => {
                let page = popup.page.max(1) as isize;
                match code {
                    KeyCode::Esc => Some(Click::Cancel),
                    KeyCode::Up => {
                        popup.step(sessions, -1);
                        None
                    }
                    KeyCode::Down => {
                        popup.step(sessions, 1);
                        None
                    }
                    KeyCode::PageUp => {
                        popup.step(sessions, -page);
                        None
                    }
                    KeyCode::PageDown => {
                        popup.step(sessions, page);
                        None
                    }
                    KeyCode::Home => {
                        popup.select(sessions, 0);
                        None
                    }
                    KeyCode::End => {
                        popup.select(sessions, sessions.len().saturating_sub(1));
                        None
                    }
                    KeyCode::Char('r') if plain => Some(Click::Rename),
                    KeyCode::Char('v') if plain => Some(Click::Verify),
                    KeyCode::Char('a') if plain => Some(Click::Addresses),
                    KeyCode::Delete => Some(Click::SignOut),
                    _ => None,
                }
            }
            SessionStage::Rename(rename) => match code {
                KeyCode::Esc => Some(Click::Cancel),
                _ if rename.request.is_some() => None,
                KeyCode::Enter => Some(Click::Confirm),
                _ => {
                    if rename.name.handle_key(code, mods, EditKeys::FULL).changed() {
                        rename.error = None;
                    }
                    None
                }
            },
            SessionStage::SignOut(sign_out) => match code {
                KeyCode::Esc => Some(Click::Cancel),
                _ if sign_out.request.is_some() => None,
                KeyCode::Enter if sign_out.typing() => Some(Click::Confirm),
                _ if sign_out.typing() => {
                    if let Some(password) = &mut sign_out.password
                        && password.handle_key(code, mods, EditKeys::FULL).changed()
                    {
                        sign_out.error = None;
                    }
                    None
                }
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                    sign_out.on_confirm = !sign_out.on_confirm;
                    None
                }
                // Enter says yes only after the focus was moved there.
                KeyCode::Enter if sign_out.on_confirm => match &sign_out.web {
                    None => Some(Click::Confirm),
                    Some(Some(_)) => Some(Click::OpenWeb),
                    Some(None) => Some(Click::Cancel),
                },
                KeyCode::Enter => Some(Click::Cancel),
                _ => None,
            },
        };
        if let Some(click) = click {
            self.devices_click(click);
        }
    }

    // ----- key backup, export and import ---------------------------------------------

    fn keys_click(&mut self, click: Click) {
        let download_dir = (click == Click::Export).then(|| self.download_dir());
        let Some(Modal::Keys(popup)) = &mut self.modal else {
            return;
        };
        match (&mut popup.stage, click) {
            (KeysStage::Status, Click::Cancel) => self.modal = None,
            (KeysStage::Status, Click::RecoveryKey) => {
                if wants_recovery_key(self.verify.security) {
                    self.open_modal(Modal::Recovery(RecoveryForm::default()));
                } else {
                    self.show_toast(no_recovery_key_reason(self.verify.security));
                }
            }
            (KeysStage::Status, Click::Export) => {
                let download_dir = download_dir.unwrap_or_default();
                let date = chrono::Local::now().format("%Y-%m-%d").to_string();
                let path = default_export_path(&download_dir, &date, |path| {
                    std::fs::symlink_metadata(path).is_ok()
                });
                popup.stage = KeysStage::Export(ExportForm::new(&path));
            }
            (KeysStage::Status, Click::Import) => {
                popup.stage = KeysStage::Import(ImportForm::new());
            }
            (KeysStage::Export(_) | KeysStage::Import(_), Click::Cancel) => {
                // What was typed goes with the form. A request that is under way goes
                // on; its outcome is told in a toast.
                popup.stage = KeysStage::Status;
            }
            (KeysStage::Export(_), Click::Confirm) => self.submit_export(),
            (KeysStage::Import(_), Click::Confirm) => self.submit_import(),
            _ => {}
        }
    }

    /// Checks what was typed and has the worker write the key file. The passphrase
    /// leaves the form as it is sent.
    fn submit_export(&mut self) {
        let download_dir = self.download_dir();
        let request = self.devices.next_request + 1;
        let Some(Modal::Keys(popup)) = &mut self.modal else {
            return;
        };
        let KeysStage::Export(form) = &mut popup.stage else {
            return;
        };
        if form.request.is_some() {
            return;
        }
        let Some(path) = key_file_path(form.path.as_str(), &download_dir) else {
            form.error = Some("Enter the name of the file to write.".to_owned());
            form.focus = 0;
            return;
        };
        if path.is_dir() {
            form.error = Some("That is a folder. Add the name of the file to write.".to_owned());
            form.focus = 0;
            return;
        }
        if let Err(problem) = check_passphrase(form.passphrase.as_str(), form.again.as_str()) {
            form.error = Some(problem.to_owned());
            // Back to the field that is wrong: the first one, unless only the
            // repetition differs.
            form.focus =
                if check_passphrase(form.passphrase.as_str(), form.passphrase.as_str()).is_err() {
                    1
                } else {
                    2
                };
            return;
        }
        // The worker refuses too; this saves typing the passphrase for nothing.
        if std::fs::symlink_metadata(&path).is_ok() {
            form.error = Some(
                "A file with that name exists already. Nothing is overwritten: choose \
                 another name."
                    .to_owned(),
            );
            form.focus = 0;
            return;
        }
        let passphrase = form.passphrase.take_secret();
        drop(form.again.take_secret());
        form.request = Some(request);
        form.error = None;
        self.devices.next_request = request;
        self.send_matrix(MatrixCmd::Devices(DevicesCmd::ExportKeys {
            request,
            path,
            passphrase,
        }));
    }

    fn submit_import(&mut self) {
        let download_dir = self.download_dir();
        let request = self.devices.next_request + 1;
        let Some(Modal::Keys(popup)) = &mut self.modal else {
            return;
        };
        let KeysStage::Import(form) = &mut popup.stage else {
            return;
        };
        if form.request.is_some() {
            return;
        }
        let Some(path) = key_file_path(form.path.as_str(), &download_dir) else {
            form.error = Some("Enter the key file to read (or drop it here).".to_owned());
            form.focus = 0;
            return;
        };
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => {}
            Ok(meta) => {
                form.error = Some(
                    if meta.is_dir() {
                        "That is a folder. Name the key file in it."
                    } else {
                        "That is not a file."
                    }
                    .to_owned(),
                );
                form.focus = 0;
                return;
            }
            Err(_) => {
                form.error = Some("There is no such file.".to_owned());
                form.focus = 0;
                return;
            }
        }
        if form.passphrase.is_empty() {
            form.error = Some("Enter the passphrase the file was exported with.".to_owned());
            form.focus = 1;
            return;
        }
        let passphrase = form.passphrase.take_secret();
        form.request = Some(request);
        form.error = None;
        self.devices.next_request = request;
        self.send_matrix(MatrixCmd::Devices(DevicesCmd::ImportKeys {
            request,
            path,
            passphrase,
        }));
    }

    pub(super) fn on_keys_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Some(Modal::Keys(popup)) = &mut self.modal else {
            return;
        };
        let plain = !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        // A form: where the focus is, how many places it has, and whether it is busy.
        let (focus, slots, fields, busy) = match &mut popup.stage {
            KeysStage::Status => {
                let click = match code {
                    KeyCode::Esc => Some(Click::Cancel),
                    KeyCode::Enter => Some(popup.focused()),
                    KeyCode::Tab | KeyCode::Right | KeyCode::Down => {
                        popup.step(1);
                        None
                    }
                    KeyCode::BackTab | KeyCode::Left | KeyCode::Up => {
                        popup.step(-1);
                        None
                    }
                    KeyCode::Char('k') if plain => Some(Click::RecoveryKey),
                    KeyCode::Char('e') if plain => Some(Click::Export),
                    KeyCode::Char('i') if plain => Some(Click::Import),
                    _ => None,
                };
                if let Some(click) = click {
                    self.devices_click(click);
                }
                return;
            }
            KeysStage::Export(form) => (
                &mut form.focus,
                ExportForm::SLOTS,
                ExportForm::FIELDS,
                form.request.is_some(),
            ),
            KeysStage::Import(form) => (
                &mut form.focus,
                ImportForm::SLOTS,
                ImportForm::FIELDS,
                form.request.is_some(),
            ),
        };
        let step = |focus: &mut usize, delta: isize| {
            *focus = (*focus as isize + delta).rem_euclid(slots as isize) as usize;
        };
        let click = match code {
            KeyCode::Esc => Some(Click::Cancel),
            _ if busy => None,
            KeyCode::Tab | KeyCode::Down => {
                step(focus, 1);
                None
            }
            KeyCode::BackTab | KeyCode::Up => {
                step(focus, -1);
                None
            }
            KeyCode::Left | KeyCode::Right if *focus >= fields => {
                *focus = if *focus == fields { fields + 1 } else { fields };
                None
            }
            // Enter moves on through the fields; from the last one it sends.
            KeyCode::Enter if *focus + 1 < fields => {
                step(focus, 1);
                None
            }
            KeyCode::Enter if *focus == fields + 1 => Some(Click::Cancel),
            KeyCode::Enter => Some(Click::Confirm),
            _ => {
                let slot = *focus;
                let typed = |field: Option<&mut LineEdit>| {
                    field
                        .is_some_and(|field| field.handle_key(code, mods, EditKeys::FULL).changed())
                };
                match &mut popup.stage {
                    KeysStage::Export(form) => {
                        if typed(form.field_mut(slot)) {
                            form.error = None;
                        }
                    }
                    KeysStage::Import(form) => {
                        if typed(form.field_mut(slot)) {
                            form.error = None;
                        }
                    }
                    KeysStage::Status => {}
                }
                None
            }
        };
        if let Some(click) = click {
            self.devices_click(click);
        }
    }

    /// A click into one of the text fields of the two popups.
    pub(super) fn click_devices_field(&mut self, slot: usize, rel_col: usize) {
        match &mut self.modal {
            Some(Modal::Sessions(popup)) => match &mut popup.stage {
                SessionStage::Rename(rename) if rename.request.is_none() => {
                    rename.name.place(rel_col, false);
                }
                SessionStage::SignOut(sign_out) if sign_out.typing() => {
                    if let Some(password) = &mut sign_out.password {
                        password.place(rel_col, true);
                    }
                }
                _ => {}
            },
            Some(Modal::Keys(popup)) => match &mut popup.stage {
                KeysStage::Export(form) if form.request.is_none() => {
                    if let Some(field) = form.field_mut(slot) {
                        field.place(rel_col, slot > 0);
                        form.focus = slot;
                    }
                }
                KeysStage::Import(form) if form.request.is_none() => {
                    if let Some(field) = form.field_mut(slot) {
                        field.place(rel_col, slot > 0);
                        form.focus = slot;
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// A paste into the secret field that has the focus, if one has: a password, a
    /// passphrase, the recovery key, in whichever popup. It goes in as it came (its
    /// first line, with every space), not by the way of other pastes, which trim and
    /// copy. True if it was taken; the caller then wipes its copy of the text.
    pub(super) fn paste_secret(&mut self, raw: &str) -> bool {
        let field = match &mut self.modal {
            Some(Modal::Login(form)) if form.busy.is_none() => {
                form.field_mut().filter(|field| field.is_secret())
            }
            Some(Modal::Recovery(form)) if !form.busy => Some(&mut form.key),
            Some(Modal::Verify(popup)) if popup.field_focused && popup.has_field(&self.verify) => {
                popup.password.as_mut()
            }
            Some(Modal::Sessions(popup)) => match &mut popup.stage {
                SessionStage::SignOut(sign_out) if sign_out.typing() => {
                    sign_out.error = None;
                    sign_out.password.as_mut()
                }
                _ => None,
            },
            Some(Modal::Keys(popup)) => match &mut popup.stage {
                KeysStage::Export(form) if form.request.is_none() && form.focus > 0 => {
                    form.error = None;
                    let slot = form.focus;
                    form.field_mut(slot)
                }
                KeysStage::Import(form) if form.request.is_none() && form.focus == 1 => {
                    form.error = None;
                    Some(&mut form.passphrase)
                }
                _ => None,
            },
            _ => None,
        };
        let Some(field) = field.filter(|field| field.is_secret()) else {
            return false;
        };
        field.paste(raw);
        self.dirty = true;
        true
    }

    /// A paste into one of the two popups that is not a secret (those go through
    /// [`App::paste_secret`]). A file dropped on the terminal arrives as a paste of
    /// its path: in the import form's file field that names the key file.
    pub(super) fn on_devices_paste(&mut self, text: &str, first_line: &str) {
        match &mut self.modal {
            Some(Modal::Sessions(popup)) => match &mut popup.stage {
                SessionStage::Rename(rename) if rename.request.is_none() => {
                    if rename.name.paste(first_line).changed() {
                        rename.error = None;
                    }
                }
                _ => {}
            },
            Some(Modal::Keys(popup)) => match &mut popup.stage {
                KeysStage::Export(form) if form.request.is_none() && form.focus == 0 => {
                    form.path.paste(first_line);
                    form.error = None;
                }
                KeysStage::Import(form) if form.request.is_none() && form.focus == 0 => {
                    let dropped = crate::upload::dropped_files(text)
                        .filter(|files| files.len() == 1)
                        .and_then(|mut files| files.pop());
                    if let Some(file) = dropped {
                        form.path.set(crate::files::display(&file));
                        form.focus = 1;
                    } else {
                        form.path.paste(first_line);
                    }
                    form.error = None;
                }
                _ => {}
            },
            _ => {}
        }
    }

    // ----- worker events --------------------------------------------------------------

    pub(super) fn on_devices_event(&mut self, event: DevicesEvent) {
        // News of a session that has ended.
        if self.user_id.is_none() || self.logged_out {
            return;
        }
        match event {
            // News for a popup that was closed (and maybe opened again) since.
            DevicesEvent::List { watch, .. } | DevicesEvent::Backup { watch, .. }
                if self.devices.watch != Some(watch) => {}
            DevicesEvent::List { list, .. } => self.devices.list = Some(list),
            DevicesEvent::Backup { server, upload, .. } => {
                self.devices.backup = server;
                self.devices.upload = upload;
            }
            DevicesEvent::Renamed { request, result } => {
                let shown = match &mut self.modal {
                    Some(Modal::Sessions(popup)) => match &mut popup.stage {
                        SessionStage::Rename(rename) if rename.request == Some(request) => {
                            match &result {
                                Ok(()) => popup.stage = SessionStage::List,
                                Err(err) => {
                                    rename.request = None;
                                    rename.error = Some(err.clone());
                                }
                            }
                            true
                        }
                        _ => false,
                    },
                    _ => false,
                };
                match result {
                    Ok(()) => self.show_toast("Session renamed"),
                    Err(err) if !shown => self.show_toast(err),
                    Err(_) => {}
                }
            }
            DevicesEvent::SignedOut { request, outcome } => self.on_signed_out(request, outcome),
            DevicesEvent::Exported { request, result } => {
                let shown = match &mut self.modal {
                    Some(Modal::Keys(popup)) => match &mut popup.stage {
                        KeysStage::Export(form) if form.request == Some(request) => {
                            match &result {
                                Ok(_) => popup.stage = KeysStage::Status,
                                Err(err) => {
                                    form.request = None;
                                    form.error = Some(err.clone());
                                    form.focus = 0;
                                }
                            }
                            true
                        }
                        _ => false,
                    },
                    _ => false,
                };
                match result {
                    Ok((count, path)) => self.show_toast(format!(
                        "Exported {count} {} to {}. Keep the file and its passphrase safe.",
                        if count == 1 { "key" } else { "keys" },
                        crate::files::display(&path)
                    )),
                    Err(err) if !shown => self.show_toast(format!("Key export failed: {err}")),
                    Err(_) => {}
                }
            }
            DevicesEvent::Imported { request, result } => {
                let shown = match &mut self.modal {
                    Some(Modal::Keys(popup)) => match &mut popup.stage {
                        KeysStage::Import(form) if form.request == Some(request) => {
                            match &result {
                                Ok(_) => popup.stage = KeysStage::Status,
                                Err(err) => {
                                    form.request = None;
                                    form.error = Some(err.clone());
                                    form.focus = 1;
                                }
                            }
                            true
                        }
                        _ => false,
                    },
                    _ => false,
                };
                match result {
                    Ok((_, 0)) => self.show_toast("That file holds no keys"),
                    Ok((0, total)) => self.show_toast(format!(
                        "Nothing new: this device already had all {total} keys of that file"
                    )),
                    Ok((imported, total)) => self.show_toast(format!(
                        "Imported {imported} of {total} keys. Messages they unlock show in \
                         a moment."
                    )),
                    Err(err) if !shown => self.show_toast(format!("Key import failed: {err}")),
                    Err(_) => {}
                }
            }
        }
    }

    fn on_signed_out(&mut self, request: u64, outcome: SignOutOutcome) {
        let waiting = match &mut self.modal {
            Some(Modal::Sessions(popup)) => match &popup.stage {
                SessionStage::SignOut(sign_out) if sign_out.request == Some(request) => {
                    let name = sign_out.label.clone();
                    Some((popup, name))
                }
                _ => None,
            },
            _ => None,
        };
        let Some((popup, name)) = waiting else {
            // The popup was closed (or moved on) meanwhile: only what happened counts.
            match outcome {
                SignOutOutcome::Done => self.show_toast("The session was signed out"),
                SignOutOutcome::Failed(err) => self.show_toast(err),
                SignOutOutcome::WrongPassword => {
                    self.show_toast("Not signed out: the password was not accepted");
                }
                SignOutOutcome::NeedsPassword | SignOutOutcome::Web { .. } => {}
            }
            return;
        };
        let SessionStage::SignOut(sign_out) = &mut popup.stage else {
            return;
        };
        sign_out.request = None;
        sign_out.error = None;
        sign_out.on_confirm = false;
        match outcome {
            SignOutOutcome::Done => {
                popup.stage = SessionStage::List;
                self.show_toast(format!("Signed out {name}"));
            }
            SignOutOutcome::NeedsPassword => sign_out.password = Some(LineEdit::secret()),
            SignOutOutcome::WrongPassword => {
                sign_out.password = Some(LineEdit::secret());
                sign_out.error = Some(
                    "The server did not accept that password. The session is still signed in."
                        .to_owned(),
                );
            }
            SignOutOutcome::Web { url } => {
                sign_out.password = None;
                sign_out.web = Some(url);
            }
            SignOutOutcome::Failed(err) => sign_out.error = Some(err),
        }
    }

    // ----- messages that could not be decrypted ---------------------------------------

    /// "Try again" on the selected message: looks for its key once more.
    pub(super) fn retry_decryption(&mut self) {
        let Some(message) = self
            .timeline
            .selected
            .and_then(|index| self.messages().get(index))
        else {
            return;
        };
        if message.kind != MsgKind::Undecryptable {
            return self.show_toast("This message is not waiting for a key");
        }
        let (Some(ItemRef::Event(event_id)), Some(why)) = (&message.item, message.utd) else {
            return self.show_toast("There is nothing to try again here");
        };
        let event_id = event_id.clone();
        let Some(room_id) = self.room_id() else {
            return;
        };
        if self.matrix.is_none() || self.logged_out {
            return self.show_toast("Not connected to an account");
        }
        // Answers name their message; a few may be under way at once.
        if self.devices.retried.len() >= crate::devices::DevicesState::RETRIES {
            self.devices.retried.clear();
        }
        self.devices.retried.insert(event_id.clone(), why);
        self.send_matrix(MatrixCmd::RetryDecryption { room_id, event_id });
        self.show_toast("Looking for the key again…");
    }

    pub(super) fn on_decryption_retried(
        &mut self,
        event_id: &str,
        result: Result<Retried, String>,
    ) {
        if self.user_id.is_none() || self.logged_out {
            return;
        }
        let why = self.devices.retried.remove(event_id);
        match (result, why) {
            (Ok(Retried::Decrypted), _) => self.show_toast("Decrypted"),
            (Ok(Retried::Gone), _) => self.show_toast(
                "That message is no longer shown here: it was decrypted into an edit or a \
                 reaction, or it scrolled out of what is loaded.",
            ),
            (Ok(Retried::StillLocked), Some(why)) => self.show_toast(why.advice()),
            (Ok(Retried::StillLocked), None) => self.show_toast("Still no key for that message"),
            (Err(err), _) => self.show_toast(format!("Could not try again: {err}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc::UnboundedReceiver;

    use crate::action::Action;
    use crate::app::App;
    use crate::config::Config;
    use crate::devices::{Click, KeysStage, SessionStage};
    use crate::matrix::{
        BackupStatus, BackupUpload, DeviceTrust, DevicesCmd, DevicesEvent, Identity, MatrixCmd,
        MatrixEvent, MatrixHandle, RecoveryStatus, Retried, RoomCategory, RoomSummary, Security,
        ServerBackup, Session, SessionList, SessionTrust, SignOutOutcome, VerifyCmd,
    };
    use crate::modal::{Modal, Setting};
    use crate::model::{ItemRef, Message, MsgKind, UtdWhy};
    use crate::ui;
    use crate::verify::VerifyPopup;

    const ROOM: &str = "!room:example.org";
    /// The message that could not be decrypted.
    const EVENT: &str = "$ev2:example.org";
    /// Midday, so that the date is the same in (nearly) every time zone.
    const SEEN: u64 = 1_699_963_200_000;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn click(col: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
    }

    fn screen(app: &mut App, terminal: &mut Terminal<TestBackend>) -> String {
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        format!("{}", terminal.backend())
    }

    fn find(text: &str, needle: &str) -> (u16, u16) {
        for (row, line) in text.lines().enumerate() {
            if let Some(byte) = line.find(needle) {
                let col = crate::text::width(&line[..byte]);
                return (col as u16, row as u16);
            }
        }
        panic!("{needle:?} not on screen:\n{text}");
    }

    fn toast(app: &App) -> &str {
        app.toast
            .as_ref()
            .map(|(text, _)| text.as_str())
            .unwrap_or("")
    }

    fn security(device: DeviceTrust, backup: BackupStatus, recovery: RecoveryStatus) -> Security {
        Security {
            device,
            identity: Identity::Present,
            signing_keys: device == DeviceTrust::Verified,
            other_devices: true,
            devices: 3,
            recovery,
            backup,
        }
    }

    fn connected() -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let (handle, commands) = MatrixHandle::for_test();
        let config = Config::default();
        let mut app = App::new(&config, std::path::PathBuf::new(), Some(handle));
        app.on_matrix(MatrixEvent::LoggedIn {
            user_id: "@alice:example.org".into(),
        });
        app.on_matrix(MatrixEvent::Security(security(
            DeviceTrust::Verified,
            BackupStatus::Enabled,
            RecoveryStatus::Ready,
        )));
        app.on_matrix(MatrixEvent::Rooms(vec![RoomSummary {
            encrypted: false,
            id: ROOM.into(),
            name: "Rats".into(),
            category: RoomCategory::Room,
            notifications: 0,
            server_notifications: 0,
            mention: false,
            unread: false,
            marked_unread: false,
            preview: None,
            avatar: None,
            alias: None,
            direct: false,
            muted: false,
        }]));
        app.on_matrix(MatrixEvent::Timeline {
            room_id: ROOM.into(),
            generation: app.generation,
            messages: vec![
                Message {
                    id: "e1".into(),
                    item: Some(ItemRef::Event("$ev1:example.org".into())),
                    sender: "bob".into(),
                    sender_id: Some("@bob:example.org".into()),
                    body: "hello from bob".into(),
                    day: "Today".into(),
                    time: "12:00".into(),
                    ..Message::default()
                },
                Message {
                    id: "e2".into(),
                    item: Some(ItemRef::Event("$ev2:example.org".into())),
                    sender: "bob".into(),
                    sender_id: Some("@bob:example.org".into()),
                    kind: MsgKind::Undecryptable,
                    body: UtdWhy::HistoryNeedsVerification.text().into(),
                    utd: Some(UtdWhy::HistoryNeedsVerification),
                    day: "Today".into(),
                    time: "12:01".into(),
                    ..Message::default()
                },
            ],
            reached_start: false,
            detached: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
        (app, terminal, commands)
    }

    fn drain(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<MatrixCmd> {
        let mut out = Vec::new();
        while let Ok(cmd) = commands.try_recv() {
            out.push(cmd);
        }
        out
    }

    fn session(id: &str, name: Option<&str>, current: bool, trust: SessionTrust) -> Session {
        Session {
            device_id: id.into(),
            name: name.map(str::to_owned),
            last_seen: Some(SEEN),
            last_ip: Some("192.0.2.7".into()),
            current,
            trust,
        }
    }

    fn sessions() -> SessionList {
        let mut old = session("OLDDEVICE", None, false, SessionTrust::NoKeys);
        old.last_seen = None;
        old.last_ip = None;
        SessionList {
            sessions: vec![
                session(
                    "THISDEVICE",
                    Some("matrix-rat on laptop"),
                    true,
                    SessionTrust::Verified,
                ),
                session(
                    "PHONEDEVICE",
                    Some("Element Android"),
                    false,
                    SessionTrust::Unverified,
                ),
                old,
            ],
            vouched: true,
        }
    }

    fn devices(event: DevicesEvent) -> MatrixEvent {
        MatrixEvent::Devices(event)
    }

    /// News for the popup that is open.
    fn listed(app: &App, list: Result<SessionList, String>) -> MatrixEvent {
        devices(DevicesEvent::List {
            watch: app.devices.watch.unwrap_or(0),
            list,
        })
    }

    fn backed(app: &App, server: ServerBackup, upload: BackupUpload) -> MatrixEvent {
        devices(DevicesEvent::Backup {
            watch: app.devices.watch.unwrap_or(0),
            server,
            upload,
        })
    }

    fn present(keys: u64) -> ServerBackup {
        ServerBackup::Present {
            keys,
            version: "7".into(),
        }
    }

    fn retried(event_id: &str, result: Result<Retried, String>) -> MatrixEvent {
        MatrixEvent::DecryptionRetried {
            room_id: ROOM.into(),
            event_id: event_id.into(),
            result,
        }
    }

    /// The sessions popup, open and filled.
    fn with_sessions() -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let (mut app, terminal, mut commands) = connected();
        drain(&mut commands);
        app.perform(Action::Setting(Setting::Sessions));
        assert!(matches!(
            drain(&mut commands).as_slice(),
            [MatrixCmd::Devices(DevicesCmd::Watch(Some(_)))]
        ));
        app.on_matrix(listed(&app, Ok(sessions())));
        (app, terminal, commands)
    }

    fn sign_out_stage(app: &App) -> &crate::devices::SignOut {
        match &app.modal {
            Some(Modal::Sessions(popup)) => match &popup.stage {
                SessionStage::SignOut(sign_out) => sign_out,
                other => panic!("not signing out: {other:?}"),
            },
            other => panic!("no sessions popup: {other:?}"),
        }
    }

    fn in_list(app: &App) -> bool {
        matches!(&app.modal, Some(Modal::Sessions(popup)) if matches!(popup.stage, SessionStage::List))
    }

    fn on_status(app: &App) -> bool {
        matches!(&app.modal, Some(Modal::Keys(popup)) if matches!(popup.stage, KeysStage::Status))
    }

    #[test]
    fn sessions_open_from_the_settings_list_live_and_stop_watching_when_closed() {
        let (mut app, mut terminal, mut commands) = connected();
        drain(&mut commands);
        app.on_event(key(KeyCode::F(2)));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Enter to view");
        let (col, row) = find(&text, "Sessions");
        app.on_event(click(col, row));
        assert!(matches!(app.modal, Some(Modal::Sessions(_))));
        assert!(matches!(
            drain(&mut commands).as_slice(),
            [MatrixCmd::Devices(DevicesCmd::Watch(Some(_)))]
        ));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Loading…");

        app.on_matrix(listed(&app, Ok(sessions())));
        let text = screen(&mut app, &mut terminal);
        insta::assert_snapshot!("sessions_popup", text);
        find(&text, "2 sessions are not verified.");
        find(&text, "· this device");
        find(&text, "no encryption keys");
        // Addresses only when asked for.
        assert!(!text.contains("192.0.2.7"));
        app.on_event(key(KeyCode::Char('a')));
        let text = screen(&mut app, &mut terminal);
        insta::assert_snapshot!("sessions_popup_addresses", text);
        find(&text, "last seen from 192.0.2.7");
        find(&text, "the server gives no address");
        let (col, row) = find(&text, " Hide addresses ");
        app.on_event(click(col + 1, row));
        assert!(!screen(&mut app, &mut terminal).contains("192.0.2.7"));

        // The list changes while it is open: the selection stays on its session.
        app.on_event(key(KeyCode::Down));
        let mut fewer = sessions();
        fewer.sessions.remove(2);
        fewer.sessions[1].trust = SessionTrust::Verified;
        fewer.vouched = false;
        app.on_matrix(listed(&app, Ok(fewer)));
        let text = screen(&mut app, &mut terminal);
        find(&text, "The server shows all of them as verified.");
        find(&text, "This device is not verified, so it cannot check");
        assert!(!text.contains("sessions are verified."), "{text}");
        find(&text, "› Element Android (PHONEDEVICE)");
        // A click selects.
        let (col, row) = find(&text, "matrix-rat on laptop");
        app.on_event(click(col, row));
        find(&screen(&mut app, &mut terminal), "› matrix-rat on laptop");

        // An error instead of a list is shown as such.
        app.on_matrix(listed(
            &app,
            Err("Could not load the sessions: offline".into()),
        ));
        find(
            &screen(&mut app, &mut terminal),
            "Could not load the sessions: offline",
        );

        assert!(drain(&mut commands).is_empty());
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        app.on_tick();
        app.on_tick();
        assert!(matches!(
            drain(&mut commands).as_slice(),
            [MatrixCmd::Devices(DevicesCmd::Watch(None))]
        ));
        // Opened again, nothing old is shown.
        app.perform(Action::Setting(Setting::Sessions));
        find(&screen(&mut app, &mut terminal), "Loading…");
    }

    #[test]
    fn a_session_is_renamed_and_failures_and_late_answers_are_handled() {
        let (mut app, mut terminal, mut commands) = with_sessions();
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Char('r')));
        let text = screen(&mut app, &mut terminal);
        insta::assert_snapshot!("sessions_rename", text);
        find(&text, "Element Android");
        // An empty name is not sent.
        for _ in 0.."Element Android".len() {
            app.on_event(key(KeyCode::Backspace));
        }
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).is_empty());
        find(&screen(&mut app, &mut terminal), "Enter a name.");
        type_text(&mut app, "Phone");
        app.on_event(key(KeyCode::Enter));
        let sent = drain(&mut commands);
        let [
            MatrixCmd::Devices(DevicesCmd::Rename {
                request,
                device_id,
                name,
            }),
        ] = sent.as_slice()
        else {
            panic!("{sent:?}");
        };
        assert_eq!(
            (device_id.as_str(), name.as_str()),
            ("PHONEDEVICE", "Phone")
        );
        find(&screen(&mut app, &mut terminal), "Renaming…");
        // Nothing is sent twice while it runs.
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).is_empty());
        // An answer to some other request changes nothing here.
        app.on_matrix(devices(DevicesEvent::Renamed {
            request: request + 7,
            result: Err("late".into()),
        }));
        find(&screen(&mut app, &mut terminal), "Renaming…");
        app.on_matrix(devices(DevicesEvent::Renamed {
            request: *request,
            result: Err("Could not rename the session: no".into()),
        }));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Could not rename the session: no");
        // Try again: this time it works.
        app.on_event(key(KeyCode::Enter));
        let sent = drain(&mut commands);
        let [MatrixCmd::Devices(DevicesCmd::Rename { request, .. })] = sent.as_slice() else {
            panic!("{sent:?}");
        };
        app.on_matrix(devices(DevicesEvent::Renamed {
            request: *request,
            result: Ok(()),
        }));
        assert!(in_list(&app));
        assert_eq!(toast(&app), "Session renamed");
        // This device can be renamed too.
        app.on_event(key(KeyCode::Home));
        app.on_event(key(KeyCode::Char('r')));
        find(&screen(&mut app, &mut terminal), "matrix-rat on laptop");
        app.on_event(key(KeyCode::Esc));
        assert!(in_list(&app));
    }

    #[test]
    fn signing_out_asks_first_then_for_the_password_and_never_resends_it() {
        let (mut app, mut terminal, mut commands) = with_sessions();
        // Never this device.
        app.on_event(key(KeyCode::Delete));
        assert!(in_list(&app));
        assert!(toast(&app).contains("Use Log out"), "{}", toast(&app));
        assert!(drain(&mut commands).is_empty());

        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Delete));
        let text = screen(&mut app, &mut terminal);
        insta::assert_snapshot!("sessions_sign_out_confirm", text);
        find(&text, "Sign out Element Android (PHONEDEVICE)?");
        find(&text, "can no longer be read by anyone.");
        // Enter alone says no.
        app.on_event(key(KeyCode::Enter));
        assert!(in_list(&app));
        assert!(drain(&mut commands).is_empty());

        app.on_event(key(KeyCode::Delete));
        app.on_event(key(KeyCode::Tab));
        app.on_event(key(KeyCode::Enter));
        let sent = drain(&mut commands);
        let [
            MatrixCmd::Devices(DevicesCmd::SignOut {
                request,
                device_id,
                password: None,
            }),
        ] = sent.as_slice()
        else {
            panic!("{sent:?}");
        };
        assert_eq!(device_id, "PHONEDEVICE");
        find(&screen(&mut app, &mut terminal), "Asking the server…");
        // Nothing more is sent while the server is asked.
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Tab));
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).is_empty());

        app.on_matrix(devices(DevicesEvent::SignedOut {
            request: *request,
            outcome: SignOutOutcome::NeedsPassword,
        }));
        let text = screen(&mut app, &mut terminal);
        insta::assert_snapshot!("sessions_sign_out_password", text);
        find(&text, "asks for your account password");
        // No password, nothing sent.
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).is_empty());
        type_text(&mut app, "hunter2");
        let text = screen(&mut app, &mut terminal);
        assert!(!text.contains("hunter2"));
        assert!(!format!("{:?}", app.modal).contains("hunter2"));
        app.on_event(key(KeyCode::Enter));
        let sent = drain(&mut commands);
        let [
            MatrixCmd::Devices(DevicesCmd::SignOut {
                request,
                device_id,
                password: Some(password),
            }),
        ] = sent.as_slice()
        else {
            panic!("{sent:?}");
        };
        assert_eq!(
            (device_id.as_str(), password.as_str()),
            ("PHONEDEVICE", "hunter2")
        );
        assert!(!format!("{sent:?}").contains("hunter2"));
        // The popup no longer has it.
        assert!(sign_out_stage(&app).password.as_ref().unwrap().is_empty());

        // Wrong: said so, the field is back and empty, and nothing was tried again.
        app.on_matrix(devices(DevicesEvent::SignedOut {
            request: *request,
            outcome: SignOutOutcome::WrongPassword,
        }));
        assert!(drain(&mut commands).is_empty());
        let text = screen(&mut app, &mut terminal);
        find(&text, "The server did not accept that password.");
        assert!(sign_out_stage(&app).password.as_ref().unwrap().is_empty());
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).is_empty());

        type_text(&mut app, "right");
        app.on_event(key(KeyCode::Enter));
        let sent = drain(&mut commands);
        let [MatrixCmd::Devices(DevicesCmd::SignOut { request, .. })] = sent.as_slice() else {
            panic!("{sent:?}");
        };
        app.on_matrix(devices(DevicesEvent::SignedOut {
            request: *request,
            outcome: SignOutOutcome::Done,
        }));
        assert!(in_list(&app));
        assert_eq!(toast(&app), "Signed out Element Android (PHONEDEVICE)");
    }

    /// Selects the phone and confirms signing it out; returns the request's number.
    fn ask_to_sign_out(app: &mut App, commands: &mut UnboundedReceiver<MatrixCmd>) -> u64 {
        app.on_event(key(KeyCode::Home));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Delete));
        app.on_event(key(KeyCode::Tab));
        app.on_event(key(KeyCode::Enter));
        match drain(commands).as_slice() {
            [MatrixCmd::Devices(DevicesCmd::SignOut { request, .. })] => *request,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn signing_out_without_a_password_stage_points_to_the_web_and_fakes_nothing() {
        let (mut app, mut terminal, mut commands) = with_sessions();
        let request = ask_to_sign_out(&mut app, &mut commands);
        let url = "https://account.example.org/account/?action=org.matrix.device_delete&device_id=PHONEDEVICE";
        app.on_matrix(devices(DevicesEvent::SignedOut {
            request,
            outcome: SignOutOutcome::Web {
                url: Some(url.into()),
            },
        }));
        let text = screen(&mut app, &mut terminal);
        insta::assert_snapshot!("sessions_sign_out_web", text);
        find(&text, "This server takes no password for this");
        find(&text, "https://account.example.org/account/");
        // The page opens only when chosen; Enter alone goes back.
        crate::app::TEST_OPENED_URLS.with(|urls| urls.borrow_mut().clear());
        let (col, row) = find(&text, " Open in browser ");
        app.on_event(click(col + 1, row));
        crate::app::TEST_OPENED_URLS.with(|urls| assert_eq!(*urls.borrow(), [url.to_owned()]));
        assert!(drain(&mut commands).is_empty());
        app.on_event(key(KeyCode::Enter));
        assert!(in_list(&app));
        crate::app::TEST_OPENED_URLS.with(|urls| assert_eq!(urls.borrow().len(), 1));

        // No page either: a plain explanation, and nothing to press but Back.
        let request = ask_to_sign_out(&mut app, &mut commands);
        app.on_matrix(devices(DevicesEvent::SignedOut {
            request,
            outcome: SignOutOutcome::Web { url: None },
        }));
        let text = screen(&mut app, &mut terminal);
        insta::assert_snapshot!("sessions_sign_out_no_web", text);
        find(&text, "Nothing was changed.");
        assert!(!text.contains("Open in browser"));
        assert!(!text.contains("Password"));
        app.on_event(key(KeyCode::Tab));
        app.on_event(key(KeyCode::Enter));
        assert!(in_list(&app));
        assert!(drain(&mut commands).is_empty());

        // A failure is shown, and asking again is possible.
        let request = ask_to_sign_out(&mut app, &mut commands);
        app.on_matrix(devices(DevicesEvent::SignedOut {
            request,
            outcome: SignOutOutcome::Failed("Could not sign the session out: offline".into()),
        }));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Could not sign the session out: offline");
        find(&text, " Sign out ");
    }

    #[test]
    fn answers_for_a_sign_out_that_was_left_are_told_but_change_nothing() {
        let (mut app, _terminal, mut commands) = with_sessions();
        let request = ask_to_sign_out(&mut app, &mut commands);
        // Left while the server is asked.
        app.on_event(key(KeyCode::Esc));
        assert!(in_list(&app));
        // The server asking for a password opens nothing.
        app.on_matrix(devices(DevicesEvent::SignedOut {
            request,
            outcome: SignOutOutcome::NeedsPassword,
        }));
        assert!(in_list(&app));
        assert_eq!(toast(&app), "");
        // Another sign-out is being confirmed: the old answer does not answer it.
        app.on_event(key(KeyCode::Delete));
        app.on_matrix(devices(DevicesEvent::SignedOut {
            request,
            outcome: SignOutOutcome::WrongPassword,
        }));
        assert!(sign_out_stage(&app).password.is_none());
        assert!(toast(&app).contains("password was not accepted"));
        app.on_matrix(devices(DevicesEvent::SignedOut {
            request,
            outcome: SignOutOutcome::Done,
        }));
        assert!(sign_out_stage(&app).request.is_none());
        assert_eq!(toast(&app), "The session was signed out");
        // With the popup closed altogether.
        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        app.on_matrix(devices(DevicesEvent::SignedOut {
            request,
            outcome: SignOutOutcome::Failed("Could not sign the session out: no".into()),
        }));
        assert_eq!(toast(&app), "Could not sign the session out: no");
        assert!(app.modal.is_none());
        assert!(drain(&mut commands).is_empty());
    }

    #[test]
    fn another_session_is_verified_only_where_that_can_work() {
        let (mut app, _terminal, mut commands) = with_sessions();
        // Without keys there is nothing to verify.
        app.on_event(key(KeyCode::End));
        app.on_event(key(KeyCode::Char('v')));
        assert!(toast(&app).contains("no encryption keys"));
        assert!(in_list(&app));
        // This device is verified, that one is not: ask it.
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Char('v')));
        let sent = drain(&mut commands);
        assert!(
            matches!(
                sent.as_slice(),
                [MatrixCmd::Verification(VerifyCmd::RequestDevice { device_id })]
                    if device_id == "PHONEDEVICE"
            ),
            "{sent:?}"
        );
        assert!(app.modal.is_none());

        // Both verified: nothing to do.
        let (mut app, _terminal, mut commands) = with_sessions();
        let mut list = sessions();
        list.sessions[1].trust = SessionTrust::Verified;
        app.on_matrix(listed(&app, Ok(list)));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Char('v')));
        assert_eq!(toast(&app), "That session is verified already.");
        assert!(drain(&mut commands).is_empty());
        // This device is not verified: a verified one can vouch for it...
        app.on_matrix(MatrixEvent::Security(security(
            DeviceTrust::Unverified,
            BackupStatus::Inactive,
            RecoveryStatus::NotSetUp,
        )));
        app.on_event(key(KeyCode::Char('v')));
        assert!(matches!(
            drain(&mut commands).as_slice(),
            [MatrixCmd::Verification(VerifyCmd::RequestDevice { .. })]
        ));
        // ...an unverified one cannot.
        app.perform(Action::Setting(Setting::Sessions));
        drain(&mut commands);
        app.on_matrix(listed(&app, Ok(sessions())));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Char('v')));
        assert!(
            toast(&app).contains("not verified itself"),
            "{}",
            toast(&app)
        );
        assert!(drain(&mut commands).is_empty());
        // On this device's own row: the usual way to verify this device.
        app.on_event(key(KeyCode::Home));
        app.on_event(key(KeyCode::Char('v')));
        assert!(matches!(
            drain(&mut commands).as_slice(),
            [MatrixCmd::Verification(VerifyCmd::Request)]
        ));
    }

    #[test]
    fn key_backup_status_and_the_recovery_key() {
        let (mut app, mut terminal, mut commands) = connected();
        drain(&mut commands);
        app.on_event(key(KeyCode::F(2)));
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, "Key backup");
        app.on_event(click(col, row));
        assert!(matches!(app.modal, Some(Modal::Keys(_))));
        assert!(matches!(
            drain(&mut commands).as_slice(),
            [MatrixCmd::Devices(DevicesCmd::Watch(Some(_)))]
        ));
        app.on_matrix(backed(&app, present(1234), BackupUpload::Idle));
        let text = screen(&mut app, &mut terminal);
        insta::assert_snapshot!("keys_popup_ok", text);
        find(
            &text,
            "The server has a key backup (version 7): 1234 message keys.",
        );
        find(&text, "No keys are waiting to be uploaded.");
        // Nothing to unlock: the key is not asked for.
        app.on_event(key(KeyCode::Char('k')));
        assert!(on_status(&app));
        assert!(
            toast(&app).starts_with("Nothing to unlock"),
            "{}",
            toast(&app)
        );

        // A device that still needs the key: say so, and take it.
        app.on_matrix(MatrixEvent::Security(security(
            DeviceTrust::Unverified,
            BackupStatus::Inactive,
            RecoveryStatus::NeedsKey,
        )));
        let text = screen(&mut app, &mut terminal);
        insta::assert_snapshot!("keys_popup_needs_key", text);
        find(
            &text,
            "This device cannot read the backup: enter your recovery",
        );
        let (col, row) = find(&text, " Enter recovery key ");
        app.on_event(click(col + 1, row));
        assert!(matches!(app.modal, Some(Modal::Recovery(_))));
        type_text(&mut app, "EsTj 1234");
        app.on_event(key(KeyCode::Enter));
        let sent = drain(&mut commands);
        assert!(
            matches!(sent.as_slice(), [MatrixCmd::Recover { key }] if key.as_str() == "EsTj 1234"),
            "{sent:?}"
        );
        app.on_tick();
        assert!(matches!(
            drain(&mut commands).as_slice(),
            [MatrixCmd::Devices(DevicesCmd::Watch(None))]
        ));

        // No backup and no recovery: what to do is said, nothing is offered.
        app.on_matrix(MatrixEvent::RecoverResult(Err("no".into())));
        app.on_event(key(KeyCode::Esc));
        app.on_matrix(MatrixEvent::Security(security(
            DeviceTrust::Verified,
            BackupStatus::Inactive,
            RecoveryStatus::NotSetUp,
        )));
        app.perform(Action::Setting(Setting::KeyBackup));
        app.on_matrix(backed(&app, ServerBackup::Missing, BackupUpload::Unknown));
        let text = screen(&mut app, &mut terminal);
        insta::assert_snapshot!("keys_popup_nothing", text);
        find(&text, "There is no key backup on the server");
        // Enter is "Close" until the focus is moved.
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        app.perform(Action::Setting(Setting::KeyBackup));
        app.on_event(key(KeyCode::Tab));
        app.on_event(key(KeyCode::Enter));
        assert!(on_status(&app));
        assert!(toast(&app).contains("no recovery key"), "{}", toast(&app));
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "matrix-rat-app-devices-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn export_form(app: &mut App) -> &mut crate::devices::ExportForm {
        match &mut app.modal {
            Some(Modal::Keys(popup)) => match &mut popup.stage {
                KeysStage::Export(form) => form,
                other => panic!("not exporting: {other:?}"),
            },
            other => panic!("no keys popup: {other:?}"),
        }
    }

    fn import_form(app: &mut App) -> &mut crate::devices::ImportForm {
        match &mut app.modal {
            Some(Modal::Keys(popup)) => match &mut popup.stage {
                KeysStage::Import(form) => form,
                other => panic!("not importing: {other:?}"),
            },
            other => panic!("no keys popup: {other:?}"),
        }
    }

    /// Fills both passphrase fields and presses Enter in the last one.
    fn retype(app: &mut App, first: &str, second: &str) {
        let form = export_form(app);
        form.passphrase.set(first);
        form.again.set(second);
        form.focus = 2;
        app.on_event(key(KeyCode::Enter));
    }

    #[test]
    fn keys_are_exported_only_with_a_good_passphrase_to_a_new_file() {
        let dir = scratch("export");
        let (mut app, mut terminal, mut commands) = connected();
        drain(&mut commands);
        // From the palette's entry, straight to the form.
        app.perform(Action::Devices(Click::Export));
        drain(&mut commands);
        // The name offered carries the day.
        let offered = export_form(&mut app).path.as_str().to_owned();
        assert!(
            offered.contains("matrix-rat-keys-20") && offered.ends_with(".txt"),
            "{offered}"
        );
        export_form(&mut app)
            .path
            .set("~/Downloads/matrix-rat-keys-2026-11-14.txt");
        insta::assert_snapshot!("keys_export_form", screen(&mut app, &mut terminal));

        let target = dir.join("keys.txt");
        export_form(&mut app).path.set(target.to_str().unwrap());
        // No passphrase, a short one, two different ones: nothing is sent.
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Enter));
        find(&screen(&mut app, &mut terminal), "Enter a passphrase");
        assert!(drain(&mut commands).is_empty());
        export_form(&mut app).focus = 1;
        type_text(&mut app, "short");
        app.on_event(key(KeyCode::Enter));
        type_text(&mut app, "short");
        app.on_event(key(KeyCode::Enter));
        find(&screen(&mut app, &mut terminal), "too short");
        assert!(drain(&mut commands).is_empty());
        retype(&mut app, "correct horse", "correct hors");
        let text = screen(&mut app, &mut terminal);
        find(&text, "The two passphrases differ.");
        assert!(!text.contains("correct hors"));
        assert!(!format!("{:?}", app.modal).contains("correct hors"));
        assert!(drain(&mut commands).is_empty());
        // A file that is there is never overwritten.
        std::fs::write(&target, b"mine").unwrap();
        retype(&mut app, "correct horse", "correct horse");
        find(&screen(&mut app, &mut terminal), "exists already");
        assert!(drain(&mut commands).is_empty());
        assert_eq!(std::fs::read(&target).unwrap(), b"mine");
        std::fs::remove_file(&target).unwrap();
        // No file name.
        export_form(&mut app).path.set("  ");
        retype(&mut app, "correct horse", "correct horse");
        find(
            &screen(&mut app, &mut terminal),
            "Enter the name of the file",
        );
        assert!(drain(&mut commands).is_empty());

        export_form(&mut app).path.set(target.to_str().unwrap());
        retype(&mut app, "correct horse", "correct horse");
        let sent = drain(&mut commands);
        let [
            MatrixCmd::Devices(DevicesCmd::ExportKeys {
                request,
                path,
                passphrase,
            }),
        ] = sent.as_slice()
        else {
            panic!("{sent:?}");
        };
        assert_eq!((path, passphrase.as_str()), (&target, "correct horse"));
        assert!(!format!("{sent:?}").contains("correct horse"));
        assert!(!format!("{sent:?}").contains("keys.txt"));
        // The form has let go of both copies.
        let form = export_form(&mut app);
        assert!(form.passphrase.is_empty() && form.again.is_empty());
        find(&screen(&mut app, &mut terminal), "Exporting…");
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).is_empty());

        // A failure comes back into the form.
        app.on_matrix(devices(DevicesEvent::Exported {
            request: *request,
            result: Err("Could not create the file: read-only".into()),
        }));
        find(
            &screen(&mut app, &mut terminal),
            "Could not create the file: read-only",
        );
        retype(&mut app, "correct horse", "correct horse");
        let sent = drain(&mut commands);
        let [MatrixCmd::Devices(DevicesCmd::ExportKeys { request, .. })] = sent.as_slice() else {
            panic!("{sent:?}");
        };
        app.on_matrix(devices(DevicesEvent::Exported {
            request: *request,
            result: Ok((42, target.clone())),
        }));
        assert!(on_status(&app));
        assert!(
            toast(&app).starts_with("Exported 42 keys to ") && toast(&app).contains("keys.txt"),
            "{}",
            toast(&app)
        );

        // Left while it runs: the outcome is still told.
        app.on_event(key(KeyCode::Char('e')));
        export_form(&mut app).path.set(target.to_str().unwrap());
        retype(&mut app, "correct horse", "correct horse");
        let request = match drain(&mut commands).as_slice() {
            [MatrixCmd::Devices(DevicesCmd::ExportKeys { request, .. })] => *request,
            other => panic!("{other:?}"),
        };
        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        app.on_matrix(devices(DevicesEvent::Exported {
            request,
            result: Err("This device has no message keys to export yet".into()),
        }));
        assert_eq!(
            toast(&app),
            "Key export failed: This device has no message keys to export yet"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn keys_are_imported_from_a_typed_or_dropped_file() {
        let dir = scratch("import");
        let file = dir.join("element-keys.txt");
        std::fs::write(&file, b"-----BEGIN MEGOLM SESSION DATA-----").unwrap();
        let (mut app, mut terminal, mut commands) = connected();
        drain(&mut commands);
        app.perform(Action::Setting(Setting::KeyBackup));
        app.on_event(key(KeyCode::Char('i')));
        drain(&mut commands);
        insta::assert_snapshot!("keys_import_form", screen(&mut app, &mut terminal));
        // Nothing typed; a file that is not there; no passphrase.
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Enter));
        find(
            &screen(&mut app, &mut terminal),
            "Enter the key file to read",
        );
        import_form(&mut app)
            .path
            .set(dir.join("nothing.txt").to_str().unwrap());
        import_form(&mut app).focus = 1;
        app.on_event(key(KeyCode::Enter));
        find(&screen(&mut app, &mut terminal), "There is no such file.");
        // Dropped on the window: the path arrives as a paste.
        app.on_event(Event::Paste(format!("'{}'\n", file.display())));
        assert!(
            import_form(&mut app)
                .path
                .as_str()
                .ends_with("element-keys.txt")
        );
        assert_eq!(import_form(&mut app).focus, 1);
        app.on_event(key(KeyCode::Enter));
        find(&screen(&mut app, &mut terminal), "Enter the passphrase");
        assert!(drain(&mut commands).is_empty());
        // A paste that is no file is text for the field it lands in.
        app.on_event(Event::Paste("open sesame".into()));
        let text = screen(&mut app, &mut terminal);
        assert!(!text.contains("open sesame"));
        app.on_event(key(KeyCode::Enter));
        let sent = drain(&mut commands);
        let [
            MatrixCmd::Devices(DevicesCmd::ImportKeys {
                request,
                path,
                passphrase,
            }),
        ] = sent.as_slice()
        else {
            panic!("{sent:?}");
        };
        assert_eq!((path, passphrase.as_str()), (&file, "open sesame"));
        assert!(import_form(&mut app).passphrase.is_empty());
        find(&screen(&mut app, &mut terminal), "Importing…");

        app.on_matrix(devices(DevicesEvent::Imported {
            request: *request,
            result: Err("Wrong passphrase (or the file is damaged). Nothing was imported.".into()),
        }));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Wrong passphrase");
        assert_eq!(import_form(&mut app).focus, 1);
        type_text(&mut app, "open sesame!");
        app.on_event(key(KeyCode::Enter));
        let request = match drain(&mut commands).as_slice() {
            [MatrixCmd::Devices(DevicesCmd::ImportKeys { request, .. })] => *request,
            other => panic!("{other:?}"),
        };
        // An answer to an earlier request is not this one's.
        app.on_matrix(devices(DevicesEvent::Imported {
            request: request - 1,
            result: Ok((0, 0)),
        }));
        assert_eq!(toast(&app), "That file holds no keys");
        assert!(import_form(&mut app).request.is_some());
        app.on_matrix(devices(DevicesEvent::Imported {
            request,
            result: Ok((3, 10)),
        }));
        assert!(on_status(&app));
        assert!(
            toast(&app).starts_with("Imported 3 of 10 keys."),
            "{}",
            toast(&app)
        );
        app.on_matrix(devices(DevicesEvent::Imported {
            request,
            result: Ok((0, 10)),
        }));
        assert!(toast(&app).starts_with("Nothing new"), "{}", toast(&app));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_undecryptable_message_says_why_and_can_be_tried_again() {
        let (mut app, mut terminal, mut commands) = connected();
        drain(&mut commands);
        let text = screen(&mut app, &mut terminal);
        find(
            &text,
            "Unable to decrypt: sent before this device was logged in.",
        );
        // The newest message is the one that could not be decrypted.
        app.perform(Action::SelectOlder);
        app.perform(Action::ContextMenu);
        let text = screen(&mut app, &mut terminal);
        insta::assert_snapshot!("undecryptable_menu", text);
        let (col, row) = find(&text, "Try decrypting again");
        app.on_event(click(col, row));
        let sent = drain(&mut commands);
        assert!(
            matches!(
                sent.as_slice(),
                [MatrixCmd::RetryDecryption { room_id, event_id }]
                    if room_id == ROOM && event_id == EVENT
            ),
            "{sent:?}"
        );
        assert_eq!(toast(&app), "Looking for the key again…");
        // An answer about some other message is not this one's: no reason is named,
        // and this one stays waited for.
        app.on_matrix(retried("$other:example.org", Ok(Retried::StillLocked)));
        assert_eq!(toast(&app), "Still no key for that message");
        // Still nothing: what would help is said.
        app.on_matrix(retried(EVENT, Ok(Retried::StillLocked)));
        assert!(
            toast(&app).contains("Verify this device"),
            "{}",
            toast(&app)
        );
        assert!(app.devices.retried.is_empty());
        // By key; this time it works.
        app.on_event(key(KeyCode::Char('d')));
        assert!(matches!(
            drain(&mut commands).as_slice(),
            [MatrixCmd::RetryDecryption { .. }]
        ));
        app.on_matrix(retried(EVENT, Ok(Retried::Decrypted)));
        assert_eq!(toast(&app), "Decrypted");
        app.on_matrix(retried(
            EVENT,
            Err("this message is no longer loaded".into()),
        ));
        assert_eq!(
            toast(&app),
            "Could not try again: this message is no longer loaded"
        );
        // Gone from the timeline is not called a success.
        app.on_matrix(retried(EVENT, Ok(Retried::Gone)));
        assert!(toast(&app).starts_with("That message is no longer shown here"));
        // What is remembered of retries stays small.
        for n in 0..100 {
            app.devices.retried.insert(format!("$e{n}"), UtdWhy::NoKey);
            app.on_event(key(KeyCode::Char('d')));
        }
        assert!(app.devices.retried.len() <= crate::devices::DevicesState::RETRIES + 1);
        drain(&mut commands);

        // A readable message has nothing to try, in the menu or by key.
        app.perform(Action::SelectOlder);
        app.perform(Action::ContextMenu);
        assert!(!screen(&mut app, &mut terminal).contains("Try decrypting again"));
        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::Char('d')));
        assert!(drain(&mut commands).is_empty());
        assert_eq!(toast(&app), "This message is not waiting for a key");
    }

    #[test]
    fn news_for_a_popup_that_was_closed_is_dropped_also_when_it_was_reopened_at_once() {
        let (mut app, mut terminal, mut commands) = with_sessions();
        let first = app.devices.watch.unwrap();
        app.on_matrix(backed(&app, present(5), BackupUpload::Idle));
        // Closed and opened again before a tick could tell the worker.
        app.on_event(key(KeyCode::Esc));
        app.perform(Action::Setting(Setting::KeyBackup));
        let second = app.devices.watch.unwrap();
        assert_ne!(first, second);
        let sent = drain(&mut commands);
        assert!(
            matches!(
                sent.as_slice(),
                [MatrixCmd::Devices(DevicesCmd::Watch(Some(id)))] if *id == second
            ),
            "{sent:?}"
        );
        // Nothing of the first opening is left: neither the list nor the backup.
        assert!(app.devices.list.is_none());
        assert_eq!(app.devices.backup, ServerBackup::Unknown);
        assert_eq!(app.devices.upload, BackupUpload::Unknown);
        // The old watcher's last words arrive: not for this popup.
        app.on_matrix(devices(DevicesEvent::List {
            watch: first,
            list: Ok(sessions()),
        }));
        app.on_matrix(devices(DevicesEvent::Backup {
            watch: first,
            server: present(5),
            upload: BackupUpload::Idle,
        }));
        assert!(app.devices.list.is_none());
        assert_eq!(app.devices.backup, ServerBackup::Unknown);
        let text = screen(&mut app, &mut terminal);
        find(&text, "Could not ask the server");
        assert!(!text.contains("The server has a key backup"));
        // The new one's are.
        app.on_matrix(backed(&app, present(6), BackupUpload::Idle));
        find(
            &screen(&mut app, &mut terminal),
            "(version 7): 6 message keys.",
        );
        // A tick while it is open sends nothing; closed, one stop.
        app.on_tick();
        assert!(drain(&mut commands).is_empty());
        app.on_event(key(KeyCode::Esc));
        app.on_tick();
        app.on_tick();
        assert!(matches!(
            drain(&mut commands).as_slice(),
            [MatrixCmd::Devices(DevicesCmd::Watch(None))]
        ));
        // News after closing changes nothing either.
        app.on_matrix(devices(DevicesEvent::Backup {
            watch: second,
            server: ServerBackup::Missing,
            upload: BackupUpload::Unknown,
        }));
        assert_eq!(app.devices.backup, present(6));
    }

    #[test]
    fn nothing_is_called_verified_that_this_device_did_not_check() {
        let (mut app, mut terminal, _commands) = with_sessions();
        // Not verified itself: the server's word, as that, and no good news.
        let mut list = sessions();
        list.vouched = false;
        list.sessions[1].trust = SessionTrust::Verified;
        list.sessions.remove(2);
        app.on_matrix(listed(&app, Ok(list)));
        let text = screen(&mut app, &mut terminal);
        insta::assert_snapshot!("sessions_popup_unchecked", text);
        find(&text, "This device is not verified, so it cannot check");
        find(
            &text,
            "2 sessions are listed. The server shows all of them as verified.",
        );
        find(&text, "says verified");
        assert!(!text.contains("sessions are verified."), "{text}");
        // Each row gives the server's word as that.
        assert!(
            !text.replace("says verified", "").contains(" verified  "),
            "{text}"
        );
        // The list empty (whatever went wrong): no "all 0".
        for vouched in [true, false] {
            app.on_matrix(listed(
                &app,
                Ok(SessionList {
                    sessions: Vec::new(),
                    vouched,
                }),
            ));
            let text = screen(&mut app, &mut terminal);
            find(&text, "The server lists no sessions.");
            assert!(
                !text.contains("All 0") && !text.contains("are verified"),
                "{text}"
            );
        }
    }

    #[test]
    fn the_backup_is_only_called_backed_up_as_far_as_that_was_seen() {
        let (mut app, mut terminal, _commands) = connected();
        app.perform(Action::Setting(Setting::KeyBackup));
        // The server has not answered (yet, or at all): nothing good about its side.
        let text = screen(&mut app, &mut terminal);
        insta::assert_snapshot!("keys_popup_unchecked", text);
        find(&text, "Could not ask the server");
        find(&text, "whether the");
        assert!(!text.contains("The server has"), "{text}");
        // It has one, and uploads are still running, then fail, then are through.
        app.on_matrix(backed(
            &app,
            present(10),
            BackupUpload::Uploading { done: 3, total: 40 },
        ));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Uploading keys: 3 of 40 are in the backup so far.");
        assert!(!text.contains("No keys are waiting"), "{text}");
        app.on_matrix(backed(&app, present(10), BackupUpload::Failed));
        find(
            &screen(&mut app, &mut terminal),
            "The last upload of keys failed.",
        );
        app.on_matrix(backed(&app, present(40), BackupUpload::Idle));
        find(
            &screen(&mut app, &mut terminal),
            "No keys are waiting to be uploaded.",
        );
        // Gone from the server while this device still counts on it.
        app.on_matrix(backed(&app, ServerBackup::Missing, BackupUpload::Idle));
        find(
            &screen(&mut app, &mut terminal),
            "This device still expects a backup",
        );
        // The settings row: red for what needs doing, not for what is in progress.
        app.on_event(key(KeyCode::Esc));
        let row_style = |app: &mut App, terminal: &mut Terminal<TestBackend>, value: &str| {
            app.on_event(key(KeyCode::F(2)));
            let text = screen(app, terminal);
            let (col, row) = find(&text, value);
            let style = terminal.backend().buffer()[(col, row)].style();
            app.on_event(key(KeyCode::Esc));
            style
        };
        let error = app.theme.style(crate::theme::Role::Error);
        for (backup, recovery, value, red) in [
            (
                BackupStatus::Enabling,
                RecoveryStatus::Ready,
                "turning on…",
                false,
            ),
            (
                BackupStatus::Downloading,
                RecoveryStatus::Ready,
                "downloading keys…",
                false,
            ),
            (
                BackupStatus::Inactive,
                RecoveryStatus::Ready,
                "not in use on this device",
                false,
            ),
            (BackupStatus::Enabled, RecoveryStatus::Ready, "on  ", false),
            (
                BackupStatus::Inactive,
                RecoveryStatus::NeedsKey,
                "this device needs the recovery key",
                true,
            ),
        ] {
            app.on_matrix(MatrixEvent::Security(security(
                DeviceTrust::Verified,
                backup,
                recovery,
            )));
            // The recovery prompt that opens by itself is not what is looked at here.
            if matches!(app.modal, Some(Modal::Recovery(_))) {
                app.on_event(key(KeyCode::Esc));
            }
            let style = row_style(&mut app, &mut terminal, value);
            assert_eq!(style.fg == error.fg, red, "{value}");
        }
    }

    #[test]
    fn secrets_are_pasted_as_they_are_and_only_into_their_fields() {
        let dir = scratch("paste");
        let file = dir.join("element-keys.txt");
        std::fs::write(&file, b"-----BEGIN MEGOLM SESSION DATA-----").unwrap();
        let (mut app, mut terminal, mut commands) = connected();
        drain(&mut commands);
        // A passphrase with spaces at both ends, pasted: every space counts, as when
        // it is typed. Only the first line is taken.
        app.perform(Action::Devices(Click::Export));
        drain(&mut commands);
        let target = dir.join("keys.txt");
        export_form(&mut app).path.set(target.to_str().unwrap());
        export_form(&mut app).focus = 1;
        app.on_event(Event::Paste("  correct horse  \nnot this".into()));
        assert_eq!(
            export_form(&mut app).passphrase.as_str(),
            "  correct horse  "
        );
        app.on_event(key(KeyCode::Enter));
        // The repetition without the spaces is a different passphrase.
        app.on_event(Event::Paste("correct horse".into()));
        app.on_event(key(KeyCode::Enter));
        find(
            &screen(&mut app, &mut terminal),
            "The two passphrases differ.",
        );
        assert_eq!(export_form(&mut app).focus, 2);
        assert!(drain(&mut commands).is_empty());
        // A paste clears the error it answers.
        export_form(&mut app).again.set("");
        app.on_event(Event::Paste("  correct horse  ".into()));
        assert!(export_form(&mut app).error.is_none());
        app.on_event(key(KeyCode::Enter));
        let sent = drain(&mut commands);
        assert!(
            matches!(
                sent.as_slice(),
                [MatrixCmd::Devices(DevicesCmd::ExportKeys { passphrase, .. })]
                    if passphrase.as_str() == "  correct horse  "
            ),
            "{sent:?}"
        );
        // The file field keeps the usual ways: trimmed, one line.
        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::Char('e')));
        export_form(&mut app).path.set("");
        export_form(&mut app).focus = 0;
        app.on_event(Event::Paste("  /tmp/x.txt  \n".into()));
        assert_eq!(export_form(&mut app).path.as_str(), "/tmp/x.txt");
        // Too short: back to the passphrase itself, not to its repetition.
        export_form(&mut app).path.set(target.to_str().unwrap());
        retype(&mut app, "short", "short");
        assert_eq!(export_form(&mut app).focus, 1);
        // A folder is not a file to write.
        export_form(&mut app).path.set(dir.to_str().unwrap());
        retype(&mut app, "correct horse", "correct horse");
        find(&screen(&mut app, &mut terminal), "That is a folder.");
        assert_eq!(export_form(&mut app).focus, 0);
        assert!(drain(&mut commands).is_empty());

        // Import: a file's path pasted into the passphrase field is a passphrase,
        // not a dropped file.
        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::Char('i')));
        import_form(&mut app).focus = 1;
        let dropped = format!("{}", file.display());
        app.on_event(Event::Paste(dropped.clone()));
        assert!(import_form(&mut app).path.is_empty());
        assert_eq!(import_form(&mut app).passphrase.as_str(), dropped);
        // A folder is not a key file.
        import_form(&mut app).path.set(dir.to_str().unwrap());
        app.on_event(key(KeyCode::Enter));
        find(&screen(&mut app, &mut terminal), "That is a folder.");
        assert!(drain(&mut commands).is_empty());

        // The account password, when signing a session out.
        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::Esc));
        app.perform(Action::Setting(Setting::Sessions));
        app.on_matrix(listed(&app, Ok(sessions())));
        drain(&mut commands);
        let request = ask_to_sign_out(&mut app, &mut commands);
        app.on_matrix(devices(DevicesEvent::SignedOut {
            request,
            outcome: SignOutOutcome::NeedsPassword,
        }));
        // Nothing typed: said, not passed over.
        app.on_event(key(KeyCode::Enter));
        find(
            &screen(&mut app, &mut terminal),
            "Enter your account password.",
        );
        assert!(drain(&mut commands).is_empty());
        app.on_event(Event::Paste(" pass word \r\n".into()));
        assert!(sign_out_stage(&app).error.is_none());
        app.on_event(key(KeyCode::Enter));
        let sent = drain(&mut commands);
        assert!(
            matches!(
                sent.as_slice(),
                [MatrixCmd::Devices(DevicesCmd::SignOut { password: Some(password), .. })]
                    if password.as_str() == " pass word "
            ),
            "{sent:?}"
        );
        // The other secrets go the same way: the login password, the recovery key,
        // the password for setting up cross-signing.
        app.modal = Some(Modal::Recovery(crate::modal::RecoveryForm::default()));
        app.on_event(Event::Paste(" EsTj 1234 ".into()));
        app.on_event(key(KeyCode::Enter));
        let sent = drain(&mut commands);
        assert!(
            matches!(sent.as_slice(), [MatrixCmd::Recover { key }] if key.as_str() == " EsTj 1234 "),
            "{sent:?}"
        );
        let mut set_up = VerifyPopup::set_up();
        set_up.password = Some(crate::lineedit::LineEdit::secret());
        set_up.field_focused = true;
        app.modal = Some(Modal::Verify(set_up));
        app.on_event(Event::Paste(" pw ".into()));
        assert!(matches!(&app.modal, Some(Modal::Verify(popup))
                if popup.password.as_ref().is_some_and(|typed| typed.as_str() == " pw ")));
        let mut login = crate::modal::LoginForm::new(None);
        login.focus = 2;
        app.modal = Some(Modal::Login(login));
        app.on_event(Event::Paste(" pw ".into()));
        // The user name next to it is not a secret and is trimmed as before.
        if let Some(Modal::Login(form)) = &mut app.modal {
            assert_eq!(form.password.as_str(), " pw ");
            form.focus = 1;
        }
        app.on_event(Event::Paste(" alice ".into()));
        assert!(
            matches!(&app.modal, Some(Modal::Login(form)) if form.username.as_str() == "alice")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn logging_out_closes_the_popups_and_drops_late_news() {
        let (mut app, _terminal, mut commands) = with_sessions();
        assert_eq!(app.devices.sessions().len(), 3);
        app.on_matrix(MatrixEvent::NeedLogin { reason: None });
        assert!(matches!(app.modal, Some(Modal::Login(_))));
        assert!(app.devices.list.is_none());
        assert!(app.devices.watch.is_none());
        app.on_matrix(listed(&app, Ok(sessions())));
        app.on_matrix(backed(&app, ServerBackup::Missing, BackupUpload::Unknown));
        app.on_matrix(devices(DevicesEvent::SignedOut {
            request: 1,
            outcome: SignOutOutcome::Done,
        }));
        app.on_matrix(retried(EVENT, Ok(Retried::Decrypted)));
        assert!(app.devices.list.is_none());
        assert_eq!(app.devices.backup, ServerBackup::Unknown);
        assert_eq!(toast(&app), "");
        drain(&mut commands);
        // Without an account neither popup opens.
        app.perform(Action::Setting(Setting::Sessions));
        app.perform(Action::Devices(Click::Export));
        assert!(matches!(app.modal, Some(Modal::Login(_))));
        assert!(drain(&mut commands).is_empty());
        // The demo has none either.
        let mut demo = App::new(&Config::default(), std::path::PathBuf::new(), None);
        demo.perform(Action::Setting(Setting::KeyBackup));
        assert!(demo.modal.is_none());
        assert!(toast(&demo).contains("demo"));
    }
}
