//! The app's side of device verification: actions, popup keys and worker events.

use crossterm::event::{KeyCode, KeyModifiers};

use super::App;
use crate::matrix::{
    Answer, DeviceTrust, Identity, MatrixCmd, RecoveryStatus, Security, SetUpOutcome, VerifyCmd,
    VerifyStage, VerifyUpdate,
};
use crate::modal::{Modal, RecoveryForm};
use crate::verify::{Button, PopupKind, VerifyPopup, stuck};

impl App {
    /// The verification popup is showing the running verification.
    fn flow_popup_open(&self) -> bool {
        matches!(&self.modal, Some(Modal::Verify(popup)) if popup.kind == PopupKind::Flow)
    }

    /// Whether a verification may take the screen now. It makes way only for the login
    /// form and for popups the user is busy with.
    fn may_show_verification(&self) -> bool {
        match &self.modal {
            None | Some(Modal::Emoji(_)) | Some(Modal::Verify(_)) => true,
            Some(Modal::Recovery(form)) => !form.busy,
            _ => false,
        }
    }

    /// News from the worker belongs to the session that is logged in. Between a logout
    /// (or an expired session) and the next login, whatever the old session still had
    /// queued is dropped.
    fn session_live(&self) -> bool {
        self.user_id.is_some()
    }

    /// "Verify this device": asks the user's other devices, or offers what fits better.
    pub(super) fn start_verification(&mut self) {
        if self.matrix.is_none() {
            return self.show_toast("Not connected to an account");
        }
        if self.verify.active().is_some() {
            if self.may_show_verification() {
                self.verify.deferred = false;
                self.open_modal(Modal::Verify(VerifyPopup::flow()));
            }
            return;
        }
        let security = self.verify.security.unwrap_or_default();
        match (security.identity, security.device) {
            (Identity::Missing, _) => self.open_modal(Modal::Verify(VerifyPopup::set_up())),
            (_, DeviceTrust::Verified) => self.open_modal(Modal::Verify(VerifyPopup::status())),
            // Nothing can vouch for this device: explain instead of asking nobody.
            _ if stuck(&security) => self.open_modal(Modal::Verify(VerifyPopup::status())),
            _ if !security.other_devices && security.recovery == RecoveryStatus::NeedsKey => {
                // No device to verify with, but the recovery key does the same job.
                self.open_modal(Modal::Recovery(RecoveryForm::default()));
            }
            _ => {
                self.verify.dismissed = None;
                self.send_matrix(MatrixCmd::Verification(VerifyCmd::Request));
            }
        }
    }

    /// A button of the verification popup, clicked or chosen with Enter. It only counts
    /// if the popup shows it right now, and (but for Close) not in the first moment
    /// after the buttons appeared or changed.
    pub(super) fn verify_button(&mut self, button: Button) {
        let allowed = match (&self.modal, button) {
            // From the status line.
            (_, Button::Show) => true,
            (Some(Modal::Verify(popup)), _) => {
                popup.buttons(&self.verify).contains(&button)
                    && (button == Button::Close || popup.armed())
            }
            _ => false,
        };
        if allowed {
            self.press(button);
        }
    }

    fn press(&mut self, button: Button) {
        let flow = self.verify.active().map(|f| f.flow);
        let answer =
            |answer| flow.map(|flow| MatrixCmd::Verification(VerifyCmd::Answer { flow, answer }));
        let send = match button {
            Button::Accept => answer(Answer::Accept),
            Button::Start => answer(Answer::Start),
            Button::Match => answer(Answer::Match),
            // The other side hears about it; the popup explains what it means.
            Button::Mismatch => answer(Answer::Mismatch),
            // Withdraws the unanswered request and sends a new one; same popup.
            Button::Retry => answer(Answer::Retry),
            // The user is done with it: tell the other side and get out of the way.
            Button::Decline | Button::Cancel => {
                let command = answer(Answer::Cancel);
                self.verify.dismissed = flow;
                self.verify.flow = None;
                self.close_verify_popup();
                command
            }
            Button::Close => {
                if self.verify.active().is_none() {
                    self.verify.flow = None;
                }
                self.close_verify_popup();
                None
            }
            Button::Show => {
                if self.verify.flow.is_some() && self.may_show_verification() {
                    self.verify.deferred = false;
                    self.open_modal(Modal::Verify(VerifyPopup::flow()));
                }
                None
            }
            Button::SetUp => self.submit_set_up(),
        };
        if let Some(command) = send {
            self.send_matrix(command);
        }
    }

    /// Sends the set-up request, with the password if the field is shown. The password
    /// leaves the popup as it is sent.
    fn submit_set_up(&mut self) -> Option<MatrixCmd> {
        if self.verify.setting_up {
            return None;
        }
        let Some(Modal::Verify(popup)) = &mut self.modal else {
            return None;
        };
        let password = match &mut popup.password {
            Some(typed) if typed.is_empty() => return None,
            Some(typed) => Some(std::mem::take(typed)),
            None => None,
        };
        popup.password_error = None;
        self.verify.setting_up = true;
        Some(MatrixCmd::Verification(VerifyCmd::SetUp { password }))
    }

    fn close_verify_popup(&mut self) {
        if matches!(self.modal, Some(Modal::Verify(_))) {
            self.modal = None;
        }
    }

    pub(super) fn on_verify_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Some(Modal::Verify(popup)) = &mut self.modal else {
            return;
        };
        let state = &self.verify;
        let plain = !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let typing = popup.field_focused && popup.has_field(state);
        let pressed = match code {
            KeyCode::Esc => {
                let button = popup.escape(state);
                return self.press(button);
            }
            // Space never chooses a button: it is too easy to type by accident.
            KeyCode::Enter if typing => Some(Button::SetUp),
            KeyCode::Enter => popup.focused(state),
            KeyCode::Tab | KeyCode::Down => {
                popup.step(state, 1);
                None
            }
            KeyCode::BackTab | KeyCode::Up => {
                popup.step(state, -1);
                None
            }
            KeyCode::Right if !typing => {
                popup.step(state, 1);
                None
            }
            KeyCode::Left if !typing => {
                popup.step(state, -1);
                None
            }
            KeyCode::Backspace if typing => {
                if let Some(password) = &mut popup.password {
                    password.pop();
                }
                None
            }
            KeyCode::Char(c) if typing && plain => {
                if let Some(password) = &mut popup.password {
                    password.push(c);
                }
                None
            }
            _ => None,
        };
        if let Some(button) = pressed {
            self.verify_button(button);
        }
    }

    pub(super) fn on_verify_paste(&mut self, text: &str) {
        if let Some(Modal::Verify(popup)) = &mut self.modal
            && popup.field_focused
            && popup.has_field(&self.verify)
            && let Some(password) = &mut popup.password
        {
            password.push_str(text);
        }
    }

    /// Esc on the popup, from wherever it comes.
    pub(super) fn verify_escape(&mut self) {
        if let Some(Modal::Verify(popup)) = &self.modal {
            let button = popup.escape(&self.verify);
            self.press(button);
        }
    }

    /// Called a few times a second: opens a request that waited for the user to stop
    /// typing.
    pub(super) fn verify_tick(&mut self) {
        if !self.verify.deferred {
            return;
        }
        if self.verify.active().is_none() {
            self.verify.deferred = false;
        } else if !self.verify.typing() && self.may_show_verification() {
            self.verify.deferred = false;
            self.open_modal(Modal::Verify(VerifyPopup::flow()));
            self.dirty = true;
        }
    }

    pub(super) fn on_security(&mut self, security: Security) {
        if !self.session_live() {
            return;
        }
        let was = self.verify.security;
        self.verify.security = Some(security);
        // Say so when this device becomes verified after it was not (not at startup).
        if was.is_some_and(|was| was.device == DeviceTrust::Unverified)
            && security.device == DeviceTrust::Verified
            && !self.flow_popup_open()
        {
            self.show_toast("This device is now verified");
        }
        // Verifying brought the keys; the recovery prompt has nothing left to do.
        if security.recovery == RecoveryStatus::Ready
            && matches!(&self.modal, Some(Modal::Recovery(form)) if !form.busy)
        {
            self.modal = None;
        }
        if security.identity == Identity::Present
            && !self.verify.setting_up
            && matches!(&self.modal, Some(Modal::Verify(popup)) if popup.kind == PopupKind::SetUp)
        {
            self.modal = None;
        }
    }

    pub(super) fn on_verification(&mut self, update: VerifyUpdate) {
        if !self.session_live()
            || self.verify.dismissed == Some(update.flow)
            || update.flow < self.verify.newest
        {
            return;
        }
        self.verify.newest = update.flow;
        let same_flow = self.verify.flow.as_ref().map(|f| f.flow) == Some(update.flow);
        let stage_changed = !same_flow
            || self.verify.flow.as_ref().is_some_and(|f| {
                std::mem::discriminant(&f.stage) != std::mem::discriminant(&update.stage)
            });
        let final_stage = update.stage.is_final();
        let summary = match &update.stage {
            VerifyStage::Done if update.this_device => {
                Some("Verified: this device is now trusted".to_owned())
            }
            VerifyStage::Done if update.own => {
                Some(format!("Verified your device “{}”", update.peer))
            }
            VerifyStage::Done => Some(format!("Verified {}", update.peer)),
            VerifyStage::Cancelled { reason } => Some(format!("Verification ended: {reason}")),
            _ => None,
        };
        let incoming = update.stage == VerifyStage::Requested;
        self.verify.flow = Some(update);

        if let Some(Modal::Verify(popup)) = &mut self.modal
            && popup.kind == PopupKind::Flow
        {
            if stage_changed {
                // New buttons: no focus carried over, and a moment before they count.
                popup.rearm();
                popup.compare_shown = false;
            }
            return;
        }
        if final_stage {
            // Nobody is looking at it: a toast is enough.
            self.verify.flow = None;
            self.verify.deferred = false;
            if let Some(summary) = summary {
                self.show_toast(summary);
            }
        } else if !same_flow {
            if incoming && self.verify.typing() {
                // Not in the middle of someone's sentence: the status line shows it,
                // and the popup opens once they pause.
                self.verify.deferred = true;
            } else if self.may_show_verification() {
                self.open_modal(Modal::Verify(VerifyPopup::flow()));
            }
        }
    }

    pub(super) fn on_cross_signing_set_up(&mut self, outcome: SetUpOutcome) {
        if !self.session_live() {
            return;
        }
        self.verify.setting_up = false;
        match outcome {
            SetUpOutcome::Done => {
                self.close_verify_popup();
                self.show_toast("Encryption is set up for your account");
            }
            SetUpOutcome::NeedsPassword { rejected } => {
                match &mut self.modal {
                    Some(Modal::Verify(popup)) if popup.kind == PopupKind::SetUp => {
                        popup.password = Some(String::new());
                        popup.field_focused = true;
                        popup.password_error =
                            rejected.then_some("That password was not accepted.");
                        popup.rearm();
                    }
                    // The popup was closed meanwhile.
                    _ => self.show_toast("Setting up encryption needs your password"),
                }
            }
            SetUpOutcome::Failed(error) => {
                self.close_verify_popup();
                self.show_toast(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crossterm::event::{Event, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::matrix::{MatrixEvent, MatrixHandle, SasEmoji};
    use crate::ui;
    use crate::verify::{GUARD, TYPING_PAUSE};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn click(column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn connected() -> (
        App,
        Terminal<TestBackend>,
        tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>,
    ) {
        let mut app = App::demo();
        let (handle, commands) = MatrixHandle::for_test();
        app.matrix = Some(handle);
        app.user_id = Some("@me:example.org".into());
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
        (app, terminal, commands)
    }

    fn redraw(app: &mut App, terminal: &mut Terminal<TestBackend>) -> String {
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        format!("{}", terminal.backend())
    }

    /// Lets the guard time of the popup's buttons pass.
    fn wait_guard(app: &mut App) {
        if let Some(Modal::Verify(popup)) = &mut app.modal {
            popup.armed_at = Instant::now().checked_sub(GUARD).unwrap();
        }
    }

    /// The user last typed long enough ago.
    fn stop_typing(app: &mut App) {
        app.verify.last_key = Instant::now().checked_sub(TYPING_PAUSE);
    }

    fn drain(commands: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixCmd>) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            seen.push(format!("{command:?}"));
        }
        seen
    }

    fn find(screen: &str, needle: &str) -> (u16, u16) {
        for (row, line) in screen.lines().enumerate() {
            if let Some(byte) = line.find(needle) {
                let col = crate::text::width(&line[..byte]) - 1;
                return (col as u16, row as u16);
            }
        }
        panic!("{needle:?} not on screen:\n{screen}");
    }

    fn update(flow: u64, stage: VerifyStage) -> MatrixEvent {
        MatrixEvent::Verification(VerifyUpdate {
            flow,
            peer: "Phone (ABCDEFGH)".into(),
            own: true,
            this_device: true,
            stage,
        })
    }

    fn compare() -> VerifyStage {
        let names = ["Dog", "Cat", "Lion", "Horse", "Unicorn", "Pig", "Elephant"];
        let symbols = ["🐶", "🐱", "🦁", "🐎", "🦄", "🐷", "🐘"];
        VerifyStage::Compare {
            emoji: symbols
                .iter()
                .zip(names)
                .map(|(symbol, name)| SasEmoji {
                    symbol: (*symbol).into(),
                    name: name.into(),
                })
                .collect(),
            decimals: (1234, 5678, 9012),
        }
    }

    fn unverified() -> Security {
        Security {
            device: DeviceTrust::Unverified,
            identity: Identity::Present,
            other_devices: true,
            recovery: RecoveryStatus::NeedsKey,
            ..Security::default()
        }
    }

    fn set_up_popup(app: &App) -> &VerifyPopup {
        match &app.modal {
            Some(Modal::Verify(popup)) if popup.kind == PopupKind::SetUp => popup,
            other => panic!("not the set-up popup: {other:?}"),
        }
    }

    #[test]
    fn incoming_request_full_flow_by_keyboard() {
        let (mut app, mut terminal, mut commands) = connected();
        stop_typing(&mut app);
        app.on_matrix(update(1, VerifyStage::Requested));
        let screen = redraw(&mut app, &mut terminal);
        find(&screen, "Phone (ABCDEFGH)");
        find(&screen, "Accept");
        // Right after it appeared, Enter counts for nothing; later it presses the
        // focused button, which is Decline unless the user picks Accept.
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).is_empty());
        assert!(matches!(app.modal, Some(Modal::Verify(_))));
        wait_guard(&mut app);
        app.on_event(key(KeyCode::Left));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(drain(&mut commands), ["Verification(Answer(1, Accept))"]);
        app.on_matrix(update(1, VerifyStage::Starting));
        app.on_matrix(update(1, compare()));
        let screen = redraw(&mut app, &mut terminal);
        for name in [
            "Dog",
            "Unicorn",
            "Elephant",
            "They match",
            "They don't match",
        ] {
            find(&screen, name);
        }
        // The emoji columns line up: every name starts in the same column as its emoji.
        let (dog, dog_row) = find(&screen, "Dog");
        let (unicorn, _) = find(&screen, "Unicorn");
        let (dog_emoji, emoji_row) = find(&screen, "🐶");
        assert_eq!(emoji_row + 1, dog_row);
        assert!(dog.abs_diff(dog_emoji) <= 2);
        assert!(unicorn > dog);
        // The answers have just appeared: a quick Enter does nothing.
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).is_empty());
        wait_guard(&mut app);
        // The focus starts on "They don't match"; Space never answers.
        app.on_event(key(KeyCode::BackTab));
        app.on_event(key(KeyCode::Char(' ')));
        assert!(drain(&mut commands).is_empty());
        app.on_event(key(KeyCode::Enter));
        assert_eq!(drain(&mut commands), ["Verification(Answer(1, Match))"]);
        app.on_matrix(update(1, VerifyStage::Confirmed));
        let screen = redraw(&mut app, &mut terminal);
        find(&screen, "Waiting for");
        app.on_matrix(update(1, VerifyStage::Done));
        let screen = redraw(&mut app, &mut terminal);
        find(&screen, "verified");
        // Enter closes a finished verification at once, without sending anything.
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert!(app.verify.flow.is_none());
        assert!(drain(&mut commands).is_empty());
    }

    #[test]
    fn a_request_does_not_interrupt_typing() {
        let (mut app, mut terminal, mut commands) = connected();
        // The user is typing when the request arrives: no popup takes the keys.
        for c in "hello".chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        app.on_matrix(update(1, VerifyStage::Requested));
        assert!(app.modal.is_none());
        app.on_event(key(KeyCode::Char(' ')));
        app.on_event(key(KeyCode::Enter));
        assert!(
            drain(&mut commands)
                .iter()
                .all(|c| !c.contains("Verification"))
        );
        let screen = redraw(&mut app, &mut terminal);
        find(&screen, "verification request");
        // Still typing: it keeps waiting.
        app.on_tick();
        assert!(app.modal.is_none());
        // A pause: now it opens, and even so the next keys need the guard time to pass.
        stop_typing(&mut app);
        app.on_tick();
        assert!(matches!(app.modal, Some(Modal::Verify(_))));
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).is_empty());
    }

    #[test]
    fn they_match_needs_every_emoji_on_screen() {
        let (mut app, _terminal, mut commands) = connected();
        app.on_matrix(update(1, compare()));
        // Too short a window: the comparison cannot be shown in full.
        let mut short = Terminal::new(TestBackend::new(100, 14)).unwrap();
        let screen = redraw(&mut app, &mut short);
        find(&screen, "too small");
        assert!(!screen.contains("They match"), "{screen}");
        assert!(!screen.contains("Elephant"), "{screen}");
        wait_guard(&mut app);
        app.perform(crate::action::Action::Verify(Button::Match));
        assert!(drain(&mut commands).is_empty());
        // Too narrow for the popup at all: the same.
        let mut narrow = Terminal::new(TestBackend::new(20, 40)).unwrap();
        let screen = redraw(&mut app, &mut narrow);
        assert!(!screen.contains("They match"), "{screen}");
        wait_guard(&mut app);
        app.perform(crate::action::Action::Verify(Button::Match));
        assert!(drain(&mut commands).is_empty());
        // Big enough: the answers appear, after the guard time.
        let mut big = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let screen = redraw(&mut app, &mut big);
        find(&screen, "They match");
        app.perform(crate::action::Action::Verify(Button::Match));
        assert!(drain(&mut commands).is_empty());
        wait_guard(&mut app);
        app.perform(crate::action::Action::Verify(Button::Match));
        assert_eq!(drain(&mut commands), ["Verification(Answer(1, Match))"]);
        // Shrunk again: gone again.
        let screen = redraw(&mut app, &mut short);
        assert!(!screen.contains("They match"), "{screen}");
    }

    #[test]
    fn esc_declines_and_later_updates_are_ignored() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(update(4, VerifyStage::Requested));
        redraw(&mut app, &mut terminal);
        // Esc needs no guard: it only ever says no.
        app.on_event(key(KeyCode::Esc));
        assert_eq!(drain(&mut commands), ["Verification(Answer(4, Cancel))"]);
        assert!(app.modal.is_none());
        app.on_matrix(update(
            4,
            VerifyStage::Cancelled {
                reason: "You cancelled the verification".into(),
            },
        ));
        assert!(app.modal.is_none());
        assert!(app.toast.is_none());
        // Esc during the comparison cancels it on the other side as well.
        app.on_matrix(update(5, compare()));
        app.on_event(key(KeyCode::Esc));
        assert_eq!(drain(&mut commands), ["Verification(Answer(5, Cancel))"]);
    }

    #[test]
    fn buttons_work_by_mouse_after_the_guard_time() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(update(2, compare()));
        let screen = redraw(&mut app, &mut terminal);
        let (col, row) = find(&screen, "They don't match");
        // A click meant for what was there a moment ago does not count.
        app.on_event(click(col + 2, row));
        assert!(drain(&mut commands).is_empty());
        wait_guard(&mut app);
        app.on_event(click(col + 2, row));
        assert_eq!(drain(&mut commands), ["Verification(Answer(2, Mismatch))"]);
        app.on_matrix(update(
            2,
            VerifyStage::Cancelled {
                reason: "The emoji did not match".into(),
            },
        ));
        let screen = redraw(&mut app, &mut terminal);
        find(&screen, "The emoji did not match");
        let (col, row) = find(&screen, "Close");
        app.on_event(click(col + 1, row));
        assert!(app.modal.is_none());
        // Clicks beside the popup do not dismiss a running verification.
        app.on_matrix(update(3, VerifyStage::Waiting));
        redraw(&mut app, &mut terminal);
        app.on_event(click(1, 1));
        assert!(matches!(app.modal, Some(Modal::Verify(_))));
    }

    #[test]
    fn status_hint_starts_verification() {
        let (mut app, mut terminal, mut commands) = connected();
        let screen = redraw(&mut app, &mut terminal);
        assert!(!screen.contains("unverified"));
        app.on_matrix(MatrixEvent::Security(unverified()));
        let screen = redraw(&mut app, &mut terminal);
        let (col, row) = find(&screen, "unverified device");
        app.on_event(click(col + 3, row));
        assert_eq!(drain(&mut commands), ["Verification(Request)"]);
        app.on_matrix(update(1, VerifyStage::Waiting));
        let screen = redraw(&mut app, &mut terminal);
        find(&screen, "Waiting for");
        // The other side answers and verification succeeds: the hint goes away.
        app.on_matrix(update(1, compare()));
        app.on_matrix(update(1, VerifyStage::Done));
        app.on_matrix(MatrixEvent::Security(Security {
            device: DeviceTrust::Verified,
            recovery: RecoveryStatus::Ready,
            ..unverified()
        }));
        app.on_event(key(KeyCode::Enter));
        let screen = redraw(&mut app, &mut terminal);
        assert!(!screen.contains("unverified"), "{screen}");
        assert!(app.modal.is_none());
    }

    #[test]
    fn start_verification_picks_what_fits() {
        let (mut app, mut terminal, mut commands) = connected();
        // Without another device to ask, the recovery key is the way.
        app.on_matrix(MatrixEvent::Security(Security {
            other_devices: false,
            ..unverified()
        }));
        app.perform(crate::action::Action::StartVerification);
        assert!(matches!(app.modal, Some(Modal::Recovery(_))));
        assert!(drain(&mut commands).is_empty());
        app.modal = None;
        // Neither a device nor a recovery key: explain, do not ask nobody.
        app.on_matrix(MatrixEvent::Security(Security {
            other_devices: false,
            recovery: RecoveryStatus::NotSetUp,
            ..unverified()
        }));
        assert_eq!(
            app.setting_value(crate::modal::Setting::Encryption),
            "not verified, Enter for details"
        );
        app.perform(crate::action::Action::StartVerification);
        let screen = redraw(&mut app, &mut terminal);
        find(&screen, "No other device of yours can vouch");
        assert!(drain(&mut commands).is_empty());
        app.on_event(key(KeyCode::Esc));
        // Verified already: nothing to do but show the status.
        app.on_matrix(MatrixEvent::Security(Security {
            device: DeviceTrust::Verified,
            signing_keys: true,
            backup: crate::matrix::BackupStatus::Enabled,
            ..unverified()
        }));
        app.perform(crate::action::Action::StartVerification);
        let screen = redraw(&mut app, &mut terminal);
        find(&screen, "this device can verify others");
        find(&screen, "Key backup");
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert!(drain(&mut commands).is_empty());
    }

    #[test]
    fn set_up_asks_for_the_password_only_when_the_server_wants_it() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::Security(Security {
            identity: Identity::Missing,
            ..unverified()
        }));
        assert_eq!(
            app.setting_value(crate::modal::Setting::Encryption),
            "not set up, Enter to set up"
        );
        app.perform(crate::action::Action::StartVerification);
        assert_eq!(set_up_popup(&app).kind, PopupKind::SetUp);
        assert!(drain(&mut commands).is_empty());
        // Enter alone closes (the focus is on Close); Set up must be chosen.
        wait_guard(&mut app);
        app.on_event(key(KeyCode::Left));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(
            drain(&mut commands),
            ["Verification(SetUp(password: false))"]
        );
        // Pressing again while it runs sends nothing more.
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).is_empty());

        // The server wants the password: a masked field appears, with the focus.
        app.on_matrix(MatrixEvent::CrossSigningSetUp(
            SetUpOutcome::NeedsPassword { rejected: false },
        ));
        for c in "hunter".chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        app.on_paste("2");
        let screen = redraw(&mut app, &mut terminal);
        find(&screen, "asks for your account password");
        find(&screen, "•••••••");
        assert!(!screen.contains("hunter"));
        assert!(!format!("{:?}", app.modal).contains("hunter"));
        wait_guard(&mut app);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(
            drain(&mut commands),
            ["Verification(SetUp(password: true))"]
        );
        // Gone from the popup once sent.
        assert_eq!(set_up_popup(&app).password.as_deref(), Some(""));

        // Wrong: say so, let the user try again; Ctrl-U clears what was typed.
        app.on_matrix(MatrixEvent::CrossSigningSetUp(
            SetUpOutcome::NeedsPassword { rejected: true },
        ));
        let screen = redraw(&mut app, &mut terminal);
        find(&screen, "not accepted");
        app.on_event(key(KeyCode::Char('x')));
        app.on_event(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(set_up_popup(&app).password.as_deref(), Some(""));
        // An empty password is not sent.
        wait_guard(&mut app);
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).is_empty());
        app.on_event(key(KeyCode::Char('y')));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(
            drain(&mut commands),
            ["Verification(SetUp(password: true))"]
        );
        app.on_matrix(MatrixEvent::CrossSigningSetUp(SetUpOutcome::Done));
        assert!(app.modal.is_none());
        assert!(!app.verify.setting_up);
    }

    #[test]
    fn asking_again_keeps_the_same_verification() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::Security(unverified()));
        app.perform(crate::action::Action::StartVerification);
        assert_eq!(drain(&mut commands), ["Verification(Request)"]);
        app.on_matrix(update(1, VerifyStage::Waiting));
        let screen = redraw(&mut app, &mut terminal);
        let (col, row) = find(&screen, "Ask again");
        wait_guard(&mut app);
        app.on_event(click(col + 2, row));
        assert_eq!(drain(&mut commands), ["Verification(Answer(1, Retry))"]);
        assert!(matches!(app.modal, Some(Modal::Verify(_))));
        app.on_matrix(update(1, compare()));
        // Anything from an older verification that arrives late changes nothing.
        app.on_matrix(update(2, VerifyStage::Waiting));
        app.on_matrix(update(1, VerifyStage::Done));
        let flow = app.verify.flow.as_ref().unwrap();
        assert_eq!(flow.flow, 2);
    }

    #[test]
    fn news_from_an_ended_session_is_dropped() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::Security(unverified()));
        // Logging out: what the old session still had queued must not come back.
        app.reset_session();
        app.on_matrix(update(1, VerifyStage::Requested));
        app.on_matrix(MatrixEvent::Security(unverified()));
        app.on_matrix(MatrixEvent::CrossSigningSetUp(SetUpOutcome::Done));
        assert!(app.modal.is_none());
        assert!(app.verify.flow.is_none());
        assert!(app.verify.security.is_none());
        assert!(app.toast.is_none());
        let screen = redraw(&mut app, &mut terminal);
        assert!(!screen.contains("verification"), "{screen}");
        // The next login brings news again.
        app.on_matrix(MatrixEvent::LoggedIn {
            user_id: "@me:example.org".into(),
        });
        app.on_matrix(update(1, VerifyStage::Requested));
        assert!(app.verify.flow.is_some());
        drain(&mut commands);
    }

    #[test]
    fn requests_wait_behind_the_login_form() {
        let (mut app, mut terminal, _commands) = connected();
        app.modal = Some(Modal::Login(crate::modal::LoginForm::new(None)));
        app.on_matrix(update(7, VerifyStage::Requested));
        assert!(matches!(app.modal, Some(Modal::Login(_))));
        // Once the form is gone, the status line offers to answer.
        app.modal = None;
        let screen = redraw(&mut app, &mut terminal);
        let (col, row) = find(&screen, "verification request");
        app.on_event(click(col + 2, row));
        assert!(matches!(app.modal, Some(Modal::Verify(_))));
        // It ends while not shown: just a toast.
        app.modal = None;
        app.on_matrix(update(
            7,
            VerifyStage::Cancelled {
                reason: "Timed out".into(),
            },
        ));
        assert!(app.verify.flow.is_none());
        assert!(app.toast.as_ref().unwrap().0.contains("Timed out"));
    }

    #[test]
    fn new_request_replaces_the_recovery_prompt_and_tiny_screens_survive() {
        let (mut app, mut terminal, _commands) = connected();
        app.on_matrix(MatrixEvent::Recovery(RecoveryStatus::NeedsKey));
        assert!(matches!(app.modal, Some(Modal::Recovery(_))));
        app.on_matrix(update(1, VerifyStage::Requested));
        assert!(matches!(app.modal, Some(Modal::Verify(_))));
        for stage in [VerifyStage::Requested, compare(), VerifyStage::Done] {
            app.on_matrix(update(1, stage));
            for (w, h) in [(1, 1), (10, 4), (25, 7), (40, 12), (61, 5), (200, 3)] {
                let mut small = Terminal::new(TestBackend::new(w, h)).unwrap();
                small.draw(|f| ui::draw(f, &mut app)).unwrap();
            }
        }
        redraw(&mut app, &mut terminal);
        // Recovery done by verifying: a reopened recovery prompt closes by itself.
        app.modal = Some(Modal::Recovery(RecoveryForm::default()));
        app.on_matrix(MatrixEvent::Security(Security {
            device: DeviceTrust::Verified,
            recovery: RecoveryStatus::Ready,
            ..unverified()
        }));
        assert!(app.modal.is_none());
    }
}
