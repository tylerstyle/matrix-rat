//! What the UI knows about encryption: this device's status, a running device
//! verification, and the state of the popup that shows it.

use std::time::Duration;

use crate::lineedit::LineEdit;
use crate::matrix::{
    BackupStatus, DeviceTrust, Identity, RecoveryStatus, Security, VerifyStage, VerifyUpdate,
};
use crate::time::Instant;

/// After the popup's buttons appear or change, clicks and Enter are ignored this long,
/// so that a key or click meant for what was there before cannot answer what is there
/// now (a "Cancel" click landing on "They match" as the comparison appears).
pub const GUARD: Duration = Duration::from_millis(700);

/// A verification request that arrives while the user types waits until they have
/// paused this long, instead of taking over the keyboard mid-sentence.
pub const TYPING_PAUSE: Duration = Duration::from_secs(2);

/// After this device's verification ends, its other device sends the signing keys it
/// lacks; that normally takes seconds. Only when they have not come this long after
/// does the app say they are missing.
pub const KEYS_GRACE: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub struct VerifyState {
    /// `None` until the worker reports (and always in demo mode).
    pub security: Option<Security>,
    /// The latest news of the running (or just finished) verification.
    pub flow: Option<VerifyUpdate>,
    /// A verification the user cancelled; its remaining updates are not shown.
    pub dismissed: Option<u64>,
    /// The newest verification seen. Updates of older ones are late and dropped.
    pub newest: u64,
    /// Waiting for the outcome of setting up cross-signing.
    pub setting_up: bool,
    /// When the user last pressed a key anywhere in the app.
    pub last_key: Option<Instant>,
    /// A request came in while the user was typing; its popup opens at the next pause.
    pub deferred: bool,
    /// When a verification of this device last reached Done. A new verification clears
    /// it. Until [`KEYS_GRACE`] after it, missing signing keys are not mentioned.
    pub done_at: Option<Instant>,
    /// The screen was redrawn once for the end of that grace time.
    pub keys_warned: bool,
}

/// What the status line suggests, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hint {
    /// A verification is waiting for the user, behind another popup.
    Pending {
        requested: bool,
    },
    Unverified,
    /// This device is verified but lacks the signing keys.
    KeysMissing,
    /// The account has no cross-signing identity yet.
    NotSetUp,
}

impl VerifyState {
    /// The running verification, if it is not over yet.
    pub fn active(&self) -> Option<&VerifyUpdate> {
        self.flow.as_ref().filter(|f| !f.stage.is_final())
    }

    pub fn hint(&self, popup_open: bool, now: Instant) -> Option<Hint> {
        if let Some(flow) = self.active()
            && !popup_open
        {
            return Some(Hint::Pending {
                requested: flow.stage == VerifyStage::Requested,
            });
        }
        let security = self.security?;
        match (security.identity, security.device) {
            (Identity::Missing, _) => Some(Hint::NotSetUp),
            (Identity::Present, DeviceTrust::Unverified) => Some(Hint::Unverified),
            _ if self.keys_missing(now) => Some(Hint::KeysMissing),
            _ => None,
        }
    }

    /// This device lacks the signing keys, and it is time to say so: no verification
    /// runs, and the last one of this device ended more than [`KEYS_GRACE`] ago.
    pub fn keys_missing(&self, now: Instant) -> bool {
        self.security.as_ref().is_some_and(keys_missing)
            && self.active().is_none()
            && !self
                .done_at
                .is_some_and(|at| now.saturating_duration_since(at) < KEYS_GRACE)
    }

    /// The finished verification of this device is on screen (or would be), and the
    /// signing keys have not come after all: its Done screen says so and offers the
    /// ways out. Only from the tick that noticed ([`keys_warned`](Self::keys_warned)),
    /// which also starts the buttons' guard time again.
    pub fn keys_warning(&self, now: Instant) -> bool {
        self.flow
            .as_ref()
            .is_some_and(|f| f.stage == VerifyStage::Done && f.own && f.this_device)
            && self.keys_warned
            && self.keys_missing(now)
    }

    /// The user is typing (pressed a key a moment ago).
    pub fn typing(&self) -> bool {
        self.last_key.is_some_and(|at| at.elapsed() < TYPING_PAUSE)
    }
}

/// This device is unverified, and nothing can change that from here: no other device
/// can vouch for it and there is no recovery key to enter.
pub fn stuck(security: &Security) -> bool {
    security.identity == Identity::Present
        && security.device == DeviceTrust::Unverified
        && !security.other_devices
        && security.recovery != RecoveryStatus::NeedsKey
}

/// This device is verified, but it did not get the signing keys (matrix-sdk 0.20 can
/// lose them when this device confirms first, and the other device may never send
/// them), so it cannot verify others.
pub fn keys_missing(security: &Security) -> bool {
    security.identity == Identity::Present
        && security.device == DeviceTrust::Verified
        && !security.signing_keys
}

/// The ways out of [`keys_missing`] that this device offers: verifying again with
/// another device, and the recovery key. Close comes last.
fn fix_buttons(security: &Security) -> &'static [Button] {
    use Button as B;
    match (
        security.other_devices,
        security.recovery == RecoveryStatus::NeedsKey,
    ) {
        (true, true) => &[B::VerifyAgain, B::RecoveryKey, B::Close],
        (true, false) => &[B::VerifyAgain, B::Close],
        (false, true) => &[B::RecoveryKey, B::Close],
        (false, false) => &[B::Close],
    }
}

/// The value the settings menu shows on its encryption row.
pub fn setting_text(security: Option<Security>) -> &'static str {
    let Some(security) = security else {
        return "unknown";
    };
    match (security.identity, security.device) {
        (Identity::Missing, _) => "not set up, Enter to set up",
        _ if keys_missing(&security) => "verified, but without the signing keys, Enter to fix",
        (_, DeviceTrust::Verified) => "this device is verified",
        (Identity::Present, DeviceTrust::Unverified) if stuck(&security) => {
            "not verified, Enter for details"
        }
        (Identity::Present, DeviceTrust::Unverified) => "not verified, Enter to verify",
        _ => "checking…",
    }
}

/// The encryption status as label and value rows, for the status popup (and settings).
pub fn describe(security: &Security) -> [(&'static str, &'static str); 4] {
    let device = match security.device {
        DeviceTrust::Verified => "verified",
        DeviceTrust::Unverified => "not verified",
        DeviceTrust::Unknown => "checking…",
    };
    let identity = match security.identity {
        Identity::Present if security.signing_keys => "set up, and this device can verify others",
        Identity::Present if keys_missing(security) => {
            "set up, but this device lacks the signing keys"
        }
        Identity::Present => "set up",
        Identity::Missing => "not set up",
        Identity::Unknown => "checking…",
    };
    let recovery = match security.recovery {
        RecoveryStatus::Ready => "set up",
        RecoveryStatus::NeedsKey => "set up, but this device needs the recovery key",
        RecoveryStatus::NotSetUp => "not set up",
        RecoveryStatus::Unknown => "checking…",
    };
    let backup = match security.backup {
        BackupStatus::Enabled => "on",
        BackupStatus::Enabling => "turning on…",
        BackupStatus::Downloading => "downloading keys…",
        BackupStatus::Disabling => "turning off…",
        BackupStatus::Inactive => "not in use on this device",
    };
    [
        ("This device", device),
        ("Cross-signing", identity),
        ("Recovery", recovery),
        ("Key backup", backup),
    ]
}

/// The controls of the verification popup. Keys and clicks both resolve to these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Accept,
    Decline,
    /// Begin the emoji comparison instead of waiting for the other side.
    Start,
    Match,
    Mismatch,
    Cancel,
    Close,
    /// Bring the popup back for a verification that waits behind another popup.
    Show,
    /// Create the account's cross-signing keys.
    SetUp,
    /// Send our unanswered request again.
    Retry,
    /// Verify this device once more, to get the signing keys it lacks.
    VerifyAgain,
    /// Enter the recovery key, for the same.
    RecoveryKey,
}

impl Button {
    pub fn label(self) -> &'static str {
        match self {
            Self::Accept => " Accept ",
            Self::Decline => " Decline ",
            Self::Start => " Compare emoji ",
            Self::Match => " They match ",
            Self::Mismatch => " They don't match ",
            Self::Cancel => " Cancel ",
            Self::Close => " Close ",
            Self::Show => " Show ",
            Self::SetUp => " Set up ",
            Self::Retry => " Ask again ",
            Self::VerifyAgain => " Verify again ",
            Self::RecoveryKey => " Enter recovery key ",
        }
    }

    /// Says yes to something. Such a button never has the focus by default, so a
    /// stray Enter cannot choose it.
    pub fn affirmative(self) -> bool {
        matches!(
            self,
            Self::Accept
                | Self::Start
                | Self::Match
                | Self::SetUp
                | Self::Retry
                | Self::VerifyAgain
                | Self::RecoveryKey
        )
    }
}

/// Which of four things the popup shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupKind {
    /// The running verification (`VerifyState::flow`).
    Flow,
    /// An offer to create the account's cross-signing keys.
    SetUp,
    /// The encryption status, when there is nothing to verify (or no way to).
    Status,
    /// This device is verified but lacks the signing keys: the ways to get them.
    KeysMissing,
}

#[derive(Clone)]
pub struct VerifyPopup {
    pub kind: PopupKind,
    /// The button with the keyboard focus; `None` (or one that is not shown) means
    /// the default, which is never an affirmative one.
    pub focus: Option<Button>,
    /// The password field has the focus (when it is shown).
    pub field_focused: bool,
    /// The account password, when the server asks for it to set up cross-signing.
    /// `Some` shows the (masked) field.
    pub password: Option<LineEdit>,
    /// Why the last password did not work.
    pub password_error: Option<&'static str>,
    /// Set by the last draw: every emoji (or digit) of the comparison is on screen.
    /// Until then "They match" is not offered.
    pub compare_shown: bool,
    /// When the buttons last appeared or changed; see [`GUARD`].
    pub armed_at: Instant,
}

/// Never prints the password.
impl std::fmt::Debug for VerifyPopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifyPopup")
            .field("kind", &self.kind)
            .field("focus", &self.focus)
            .field("password_field", &self.password.is_some())
            .field("compare_shown", &self.compare_shown)
            .finish_non_exhaustive()
    }
}

impl VerifyPopup {
    fn new(kind: PopupKind) -> Self {
        Self {
            kind,
            focus: None,
            field_focused: false,
            password: None,
            password_error: None,
            compare_shown: false,
            armed_at: Instant::now(),
        }
    }

    pub fn flow() -> Self {
        Self::new(PopupKind::Flow)
    }

    pub fn set_up() -> Self {
        Self::new(PopupKind::SetUp)
    }

    pub fn status() -> Self {
        Self::new(PopupKind::Status)
    }

    pub fn keys_missing() -> Self {
        Self::new(PopupKind::KeysMissing)
    }

    /// The buttons changed: start the guard time again and forget the focus.
    pub fn rearm(&mut self) {
        self.armed_at = Instant::now();
        self.focus = None;
    }

    /// Clicks and Enter count (the guard time is over).
    pub fn armed(&self) -> bool {
        self.armed_at.elapsed() >= GUARD
    }

    /// The buttons shown right now, in order.
    pub fn buttons(&self, state: &VerifyState) -> &'static [Button] {
        use Button as B;
        match self.kind {
            PopupKind::SetUp if state.setting_up => &[],
            PopupKind::SetUp => &[B::SetUp, B::Close],
            PopupKind::Status => &[B::Close],
            PopupKind::KeysMissing => fix_buttons(&state.security.unwrap_or_default()),
            PopupKind::Flow if state.keys_warning(Instant::now()) => {
                fix_buttons(&state.security.unwrap_or_default())
            }
            PopupKind::Flow => match state.flow.as_ref().map(|f| &f.stage) {
                None => &[B::Close],
                Some(VerifyStage::Requested) => &[B::Accept, B::Decline],
                Some(VerifyStage::Ready) => &[B::Start, B::Cancel],
                // Only once the user can see everything there is to compare.
                Some(VerifyStage::Compare { .. }) if self.compare_shown => &[B::Match, B::Mismatch],
                Some(VerifyStage::Compare { .. }) => &[B::Cancel],
                Some(VerifyStage::Waiting) => &[B::Retry, B::Cancel],
                Some(VerifyStage::Starting | VerifyStage::Confirmed) => &[B::Cancel],
                Some(VerifyStage::Done | VerifyStage::Cancelled { .. }) => &[B::Close],
            },
        }
    }

    /// The password field is shown and can take the focus.
    pub fn has_field(&self, state: &VerifyState) -> bool {
        self.kind == PopupKind::SetUp && self.password.is_some() && !state.setting_up
    }

    /// What Esc does: decline, cancel or close, whichever fits.
    pub fn escape(&self, state: &VerifyState) -> Button {
        match (self.kind, state.active().map(|f| &f.stage)) {
            (PopupKind::Flow, Some(VerifyStage::Requested)) => Button::Decline,
            (PopupKind::Flow, Some(_)) => Button::Cancel,
            _ => Button::Close,
        }
    }

    /// The button Enter would press: the chosen one if it is shown, else the first
    /// one that does not say yes. `None` while the password field has the focus.
    pub fn focused(&self, state: &VerifyState) -> Option<Button> {
        if self.field_focused && self.has_field(state) {
            return None;
        }
        let buttons = self.buttons(state);
        match self.focus {
            Some(button) if buttons.contains(&button) => Some(button),
            _ => buttons.iter().copied().find(|b| !b.affirmative()),
        }
    }

    /// Moves the keyboard focus through the field (if shown) and the buttons,
    /// wrapping around.
    pub fn step(&mut self, state: &VerifyState, delta: isize) {
        let mut slots: Vec<Option<Button>> = Vec::new();
        if self.has_field(state) {
            slots.push(None);
        }
        slots.extend(self.buttons(state).iter().map(|b| Some(*b)));
        if slots.is_empty() {
            return;
        }
        let current = if self.field_focused && self.has_field(state) {
            None
        } else {
            self.focused(state)
        };
        let at = slots.iter().position(|s| *s == current).unwrap_or(0) as isize;
        let next = slots[(at + delta).rem_euclid(slots.len() as isize) as usize];
        self.field_focused = next.is_none();
        if next.is_some() {
            self.focus = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(stage: VerifyStage) -> VerifyState {
        VerifyState {
            flow: Some(VerifyUpdate {
                flow: 1,
                peer: "phone".into(),
                own: true,
                this_device: true,
                peer_verified: true,
                stage,
            }),
            ..VerifyState::default()
        }
    }

    #[test]
    fn buttons_follow_the_stage_and_default_to_no() {
        let mut popup = VerifyPopup::flow();
        let state = flow(VerifyStage::Requested);
        assert_eq!(popup.buttons(&state), [Button::Accept, Button::Decline]);
        assert_eq!(popup.focused(&state), Some(Button::Decline));
        assert_eq!(popup.escape(&state), Button::Decline);
        popup.step(&state, 1);
        assert_eq!(popup.focused(&state), Some(Button::Accept));

        let state = flow(VerifyStage::Compare {
            emoji: Vec::new(),
            decimals: (1, 2, 3),
        });
        // Nothing drawn yet: only cancelling is possible.
        let mut popup = VerifyPopup::flow();
        assert_eq!(popup.buttons(&state), [Button::Cancel]);
        popup.compare_shown = true;
        assert_eq!(popup.buttons(&state), [Button::Match, Button::Mismatch]);
        assert_eq!(popup.focused(&state), Some(Button::Mismatch));
        assert_eq!(popup.escape(&state), Button::Cancel);

        let state = flow(VerifyStage::Cancelled {
            reason: "Timed out".into(),
        });
        assert_eq!(popup.buttons(&state), [Button::Close]);
        assert_eq!(popup.focused(&state), Some(Button::Close));
        assert_eq!(popup.escape(&state), Button::Close);
    }

    #[test]
    fn set_up_focus_moves_through_the_password_field() {
        let mut offer = VerifyPopup::set_up();
        let mut state = VerifyState::default();
        assert_eq!(offer.focused(&state), Some(Button::Close));
        offer.step(&state, -1);
        assert_eq!(offer.focused(&state), Some(Button::SetUp));
        // The server wants the password: the field joins the cycle.
        offer.password = Some(LineEdit::secret());
        offer.field_focused = true;
        assert_eq!(offer.focused(&state), None);
        offer.step(&state, 1);
        assert_eq!(offer.focused(&state), Some(Button::SetUp));
        offer.step(&state, 1);
        assert_eq!(offer.focused(&state), Some(Button::Close));
        offer.step(&state, 1);
        assert!(offer.field_focused);
        state.setting_up = true;
        assert!(offer.buttons(&state).is_empty());
        assert!(!offer.has_field(&state));
        offer.step(&state, 1);
        assert_eq!(offer.focused(&state), None);
        offer.password = Some({
            let mut pw = LineEdit::secret();
            pw.set("hunter2");
            pw
        });
        assert!(!format!("{offer:?}").contains("hunter2"));
        assert!(!format!("{:?}", offer.password).contains("hunter2"));
    }

    #[test]
    fn hints_and_setting_text() {
        let now = Instant::now();
        let mut state = VerifyState::default();
        // Nothing known yet (or demo mode): no hint.
        assert_eq!(state.hint(false, now), None);
        assert_eq!(setting_text(None), "unknown");
        let mut security = Security {
            identity: Identity::Present,
            device: DeviceTrust::Unverified,
            other_devices: true,
            recovery: RecoveryStatus::NeedsKey,
            ..Security::default()
        };
        state.security = Some(security);
        assert_eq!(state.hint(false, now), Some(Hint::Unverified));
        assert_eq!(
            setting_text(Some(security)),
            "not verified, Enter to verify"
        );
        security.other_devices = false;
        security.recovery = RecoveryStatus::NotSetUp;
        assert!(stuck(&security));
        assert_eq!(
            setting_text(Some(security)),
            "not verified, Enter for details"
        );
        security.device = DeviceTrust::Verified;
        security.signing_keys = true;
        state.security = Some(security);
        assert_eq!(state.hint(false, now), None);
        assert_eq!(setting_text(Some(security)), "this device is verified");
        security.identity = Identity::Missing;
        security.device = DeviceTrust::Unverified;
        state.security = Some(security);
        assert_eq!(state.hint(false, now), Some(Hint::NotSetUp));
        assert_eq!(setting_text(Some(security)), "not set up, Enter to set up");
        // A request waiting behind another popup comes first, unless its popup is open.
        state.flow = flow(VerifyStage::Requested).flow;
        assert_eq!(
            state.hint(false, now),
            Some(Hint::Pending { requested: true })
        );
        assert_eq!(state.hint(true, now), Some(Hint::NotSetUp));
    }

    fn without_keys() -> Security {
        Security {
            identity: Identity::Present,
            device: DeviceTrust::Verified,
            signing_keys: false,
            other_devices: true,
            recovery: RecoveryStatus::NeedsKey,
            ..Security::default()
        }
    }

    #[test]
    fn missing_keys_are_mentioned_only_when_no_verification_can_still_bring_them() {
        let now = Instant::now();
        let mut state = VerifyState {
            security: Some(without_keys()),
            ..VerifyState::default()
        };
        assert!(keys_missing(&without_keys()));
        assert!(state.keys_missing(now));
        assert_eq!(state.hint(false, now), Some(Hint::KeysMissing));
        // Not while a verification runs: it may be the one that brings them.
        state.flow = flow(VerifyStage::Ready).flow;
        assert!(!state.keys_missing(now));
        assert_eq!(state.hint(true, now), None);
        assert_eq!(
            state.hint(false, now),
            Some(Hint::Pending { requested: false })
        );
        // Nor in the grace time after this device's verification ended.
        state.flow = flow(VerifyStage::Done).flow;
        state.done_at = Some(now);
        assert!(!state.keys_missing(now));
        assert_eq!(state.hint(false, now), None);
        assert!(!state.keys_missing(now + KEYS_GRACE / 2));
        assert!(state.keys_missing(now + KEYS_GRACE));
        assert_eq!(state.hint(false, now + KEYS_GRACE), Some(Hint::KeysMissing));
        // The Done screen says so only once the tick has noticed.
        assert!(!state.keys_warning(now + KEYS_GRACE));
        state.keys_warned = true;
        assert!(state.keys_warning(now + KEYS_GRACE));
        // Once they came, nothing to say.
        let mut security = without_keys();
        security.signing_keys = true;
        state.security = Some(security);
        assert!(!state.keys_missing(now + KEYS_GRACE));
        assert!(!state.keys_warning(now + KEYS_GRACE));
        assert_eq!(state.hint(false, now + KEYS_GRACE), None);
        // Only a verified device of an account with cross-signing can lack them.
        for security in [
            Security {
                device: DeviceTrust::Unverified,
                ..without_keys()
            },
            Security {
                identity: Identity::Missing,
                ..without_keys()
            },
            Security {
                identity: Identity::Unknown,
                ..without_keys()
            },
        ] {
            assert!(!keys_missing(&security), "{security:?}");
        }
    }

    #[test]
    fn missing_keys_in_the_settings_and_the_status() {
        let security = without_keys();
        assert_eq!(
            setting_text(Some(security)),
            "verified, but without the signing keys, Enter to fix"
        );
        let rows = describe(&security);
        assert_eq!(rows[0], ("This device", "verified"));
        assert_eq!(
            rows[1],
            (
                "Cross-signing",
                "set up, but this device lacks the signing keys"
            )
        );
        assert_eq!(
            rows[2],
            ("Recovery", "set up, but this device needs the recovery key")
        );
        // An unverified device without them is just unverified.
        let unverified = Security {
            device: DeviceTrust::Unverified,
            ..security
        };
        assert_eq!(describe(&unverified)[1], ("Cross-signing", "set up"));
    }

    #[test]
    fn the_ways_to_the_keys_follow_what_this_device_has() {
        use Button as B;
        let mut state = VerifyState {
            security: Some(without_keys()),
            ..VerifyState::default()
        };
        let popup = VerifyPopup::keys_missing();
        let cases = [
            (
                true,
                RecoveryStatus::NeedsKey,
                &[B::VerifyAgain, B::RecoveryKey, B::Close][..],
            ),
            (
                true,
                RecoveryStatus::NotSetUp,
                &[B::VerifyAgain, B::Close][..],
            ),
            (
                false,
                RecoveryStatus::NeedsKey,
                &[B::RecoveryKey, B::Close][..],
            ),
            (false, RecoveryStatus::NotSetUp, &[B::Close][..]),
        ];
        for (other_devices, recovery, buttons) in cases {
            state.security = Some(Security {
                other_devices,
                recovery,
                ..without_keys()
            });
            assert_eq!(popup.buttons(&state), buttons);
            // Saying yes takes a choice: the default is Close, and so is Esc.
            assert_eq!(popup.focused(&state), Some(B::Close));
            assert_eq!(popup.escape(&state), B::Close);
        }
        assert!(B::VerifyAgain.affirmative() && B::RecoveryKey.affirmative());

        // The Done screen of this device's verification, once the keys are overdue,
        // offers the same.
        state.security = Some(without_keys());
        state.flow = flow(VerifyStage::Done).flow;
        state.keys_warned = true;
        let done = VerifyPopup::flow();
        assert_eq!(
            done.buttons(&state),
            [B::VerifyAgain, B::RecoveryKey, B::Close]
        );
        state.keys_warned = false;
        state.done_at = Some(Instant::now());
        assert_eq!(done.buttons(&state), [B::Close]);
        // Not the Done screen of another device's verification.
        state.done_at = None;
        state.keys_warned = true;
        if let Some(flow) = &mut state.flow {
            flow.this_device = false;
        }
        assert_eq!(done.buttons(&state), [B::Close]);
    }
}
