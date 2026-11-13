//! The app's side of device verification: actions, popup keys and worker events.

use crossterm::event::KeyCode;

use super::App;
use crate::matrix::{
    Answer, DeviceTrust, Identity, MatrixCmd, RecoveryStatus, Security, VerifyCmd, VerifyStage,
    VerifyUpdate,
};
use crate::modal::{Modal, RecoveryForm};
use crate::verify::{Button, PopupKind, VerifyPopup};

impl App {
    /// The verification popup is showing the running verification.
    fn flow_popup_open(&self) -> bool {
        matches!(
            self.modal,
            Some(Modal::Verify(VerifyPopup {
                kind: PopupKind::Flow,
                ..
            }))
        )
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

    /// "Verify this device": asks the user's other devices, or offers what fits better.
    pub(super) fn start_verification(&mut self) {
        if self.matrix.is_none() {
            return self.show_toast("Not connected to an account");
        }
        if self.verify.active().is_some() {
            if self.may_show_verification() {
                self.open_modal(Modal::Verify(VerifyPopup::flow()));
            }
            return;
        }
        let security = self.verify.security.unwrap_or_default();
        match (security.identity, security.device) {
            (_, DeviceTrust::Verified) => self.open_modal(Modal::Verify(VerifyPopup::status())),
            (Identity::Missing, _) => self.open_modal(Modal::Verify(VerifyPopup::set_up())),
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

    /// A button of the verification popup, clicked or pressed.
    pub(super) fn verify_button(&mut self, button: Button) {
        let flow = self.verify.active().map(|f| f.flow);
        let answer =
            |answer| flow.map(|flow| MatrixCmd::Verification(VerifyCmd::Answer { flow, answer }));
        match button {
            Button::Accept => {
                if let Some(command) = answer(Answer::Accept) {
                    self.send_matrix(command);
                }
            }
            Button::Start => {
                if let Some(command) = answer(Answer::Start) {
                    self.send_matrix(command);
                }
            }
            Button::Match => {
                if let Some(command) = answer(Answer::Match) {
                    self.send_matrix(command);
                }
            }
            // The other side hears about it; the popup explains what it means.
            Button::Mismatch => {
                if let Some(command) = answer(Answer::Mismatch) {
                    self.send_matrix(command);
                }
            }
            // The user is done with it: tell the other side and get out of the way.
            Button::Decline | Button::Cancel => {
                if let Some(command) = answer(Answer::Cancel) {
                    self.send_matrix(command);
                }
                self.verify.dismissed = flow;
                self.verify.flow = None;
                self.close_verify_popup();
            }
            Button::Close => {
                if self.verify.active().is_none() {
                    self.verify.flow = None;
                }
                self.close_verify_popup();
            }
            Button::Show => {
                if self.verify.flow.is_some() && self.may_show_verification() {
                    self.open_modal(Modal::Verify(VerifyPopup::flow()));
                }
            }
            Button::Retry => {
                // The old request's last words are not shown; the new one takes its place.
                self.verify.dismissed = flow;
                self.send_matrix(MatrixCmd::Verification(VerifyCmd::Request));
            }
            Button::SetUp => {
                if !self.verify.setting_up {
                    self.verify.setting_up = true;
                    self.send_matrix(MatrixCmd::Verification(VerifyCmd::SetUp { password: None }));
                }
            }
        }
    }

    fn close_verify_popup(&mut self) {
        if matches!(self.modal, Some(Modal::Verify(_))) {
            self.modal = None;
        }
    }

    pub(super) fn on_verify_key(&mut self, code: KeyCode) {
        let Some(Modal::Verify(popup)) = &mut self.modal else {
            return;
        };
        let state = &self.verify;
        let pressed = match code {
            KeyCode::Esc => Some(popup.escape(state)),
            KeyCode::Enter | KeyCode::Char(' ') => popup.focused(state),
            KeyCode::Tab | KeyCode::Right | KeyCode::Down => {
                popup.step(state, 1);
                None
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Up => {
                popup.step(state, -1);
                None
            }
            _ => None,
        };
        if let Some(button) = pressed {
            self.verify_button(button);
        }
    }

    /// Esc on the popup, from wherever it comes.
    pub(super) fn verify_escape(&mut self) {
        if let Some(Modal::Verify(popup)) = &self.modal {
            let button = popup.escape(&self.verify);
            self.verify_button(button);
        }
    }

    pub(super) fn on_security(&mut self, security: Security) {
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
            && matches!(
                self.modal,
                Some(Modal::Verify(VerifyPopup {
                    kind: PopupKind::SetUp,
                    ..
                }))
            )
            && !self.verify.setting_up
        {
            self.modal = None;
        }
    }

    pub(super) fn on_verification(&mut self, update: VerifyUpdate) {
        if self.verify.dismissed == Some(update.flow) || update.flow < self.verify.newest {
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
        self.verify.flow = Some(update);

        if let Some(Modal::Verify(popup)) = &mut self.modal
            && popup.kind == PopupKind::Flow
        {
            if stage_changed {
                popup.focus = 0;
            }
            return;
        }
        if final_stage {
            // Nobody is looking at it: a toast is enough.
            self.verify.flow = None;
            if let Some(summary) = summary {
                self.show_toast(summary);
            }
        } else if !same_flow && self.may_show_verification() {
            self.open_modal(Modal::Verify(VerifyPopup::flow()));
        }
    }

    pub(super) fn on_cross_signing_set_up(&mut self, result: Result<(), String>) {
        self.verify.setting_up = false;
        match result {
            Ok(()) => {
                self.close_verify_popup();
                self.show_toast("Encryption is set up for your account");
            }
            Err(error) => {
                self.close_verify_popup();
                self.show_toast(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{
        Event, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::matrix::{MatrixEvent, MatrixHandle, SasEmoji};
    use crate::ui;

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
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
        (app, terminal, commands)
    }

    fn redraw(app: &mut App, terminal: &mut Terminal<TestBackend>) -> String {
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        format!("{}", terminal.backend())
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

    #[test]
    fn incoming_request_full_flow_by_keyboard() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(update(1, VerifyStage::Requested));
        let screen = redraw(&mut app, &mut terminal);
        find(&screen, "Phone (ABCDEFGH)");
        find(&screen, "Accept");
        // Enter presses the focused button: Accept.
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
        // Tab moves to "They don't match", Shift-Tab back; Enter confirms.
        app.on_event(key(KeyCode::Tab));
        app.on_event(key(KeyCode::BackTab));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(drain(&mut commands), ["Verification(Answer(1, Match))"]);
        app.on_matrix(update(1, VerifyStage::Confirmed));
        let screen = redraw(&mut app, &mut terminal);
        find(&screen, "Waiting for");
        app.on_matrix(update(1, VerifyStage::Done));
        let screen = redraw(&mut app, &mut terminal);
        find(&screen, "verified");
        // Esc closes a finished verification without sending anything.
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert!(app.verify.flow.is_none());
        assert!(drain(&mut commands).is_empty());
    }

    #[test]
    fn esc_declines_and_later_updates_are_ignored() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(update(4, VerifyStage::Requested));
        redraw(&mut app, &mut terminal);
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
    fn buttons_work_by_mouse() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(update(2, compare()));
        let screen = redraw(&mut app, &mut terminal);
        let (col, row) = find(&screen, "They don't match");
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
        // No cross-signing yet: offer to set it up, and do so only when asked.
        app.on_matrix(MatrixEvent::Security(Security {
            identity: Identity::Missing,
            ..unverified()
        }));
        app.perform(crate::action::Action::StartVerification);
        assert!(matches!(
            app.modal,
            Some(Modal::Verify(VerifyPopup {
                kind: PopupKind::SetUp,
                ..
            }))
        ));
        assert!(drain(&mut commands).is_empty());
        app.on_event(key(KeyCode::Enter));
        assert_eq!(
            drain(&mut commands),
            ["Verification(SetUp(password: false))"]
        );
        // Pressing again while it runs sends nothing more.
        app.on_event(key(KeyCode::Enter));
        assert!(drain(&mut commands).is_empty());
        app.on_matrix(MatrixEvent::CrossSigningSetUp(Ok(())));
        assert!(app.modal.is_none());
        assert!(!app.verify.setting_up);
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
    fn asking_again_replaces_the_request_and_late_news_is_dropped() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::Security(unverified()));
        app.perform(crate::action::Action::StartVerification);
        assert_eq!(drain(&mut commands), ["Verification(Request)"]);
        app.on_matrix(update(1, VerifyStage::Waiting));
        let screen = redraw(&mut app, &mut terminal);
        find(&screen, "Ask again");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(drain(&mut commands), ["Verification(Request)"]);
        // The old request ends (cancelled by us): not shown.
        app.on_matrix(update(
            1,
            VerifyStage::Cancelled {
                reason: "You cancelled the verification".into(),
            },
        ));
        assert_eq!(
            app.verify.flow.as_ref().unwrap().stage,
            VerifyStage::Waiting
        );
        app.on_matrix(update(2, VerifyStage::Waiting));
        app.on_matrix(update(2, compare()));
        // Anything from the first request that arrives late changes nothing.
        app.on_matrix(update(1, VerifyStage::Done));
        let flow = app.verify.flow.as_ref().unwrap();
        assert_eq!(flow.flow, 2);
        assert!(matches!(flow.stage, VerifyStage::Compare { .. }));
        assert!(matches!(app.modal, Some(Modal::Verify(_))));
    }

    #[test]
    fn requests_wait_behind_the_login_form() {
        let (mut app, mut terminal, _commands) = connected();
        app.on_matrix(MatrixEvent::NeedLogin { reason: None });
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
