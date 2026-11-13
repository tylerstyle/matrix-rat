//! What the UI knows about encryption: this device's status, a running device
//! verification, and the state of the popup that shows it.

use crate::matrix::{
    BackupStatus, DeviceTrust, Identity, RecoveryStatus, Security, VerifyStage, VerifyUpdate,
};

#[derive(Debug, Default)]
pub struct VerifyState {
    /// `None` until the worker reports (and always in demo mode).
    pub security: Option<Security>,
    /// The latest news of the running (or just finished) verification.
    pub flow: Option<VerifyUpdate>,
    /// A verification the user cancelled; its remaining updates are not shown.
    pub dismissed: Option<u64>,
    /// Waiting for the outcome of setting up cross-signing.
    pub setting_up: bool,
}

/// What the status line suggests, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hint {
    /// A verification is waiting for the user, behind another popup.
    Pending {
        requested: bool,
    },
    Unverified,
    /// The account has no cross-signing identity yet.
    NotSetUp,
}

impl VerifyState {
    /// The running verification, if it is not over yet.
    pub fn active(&self) -> Option<&VerifyUpdate> {
        self.flow.as_ref().filter(|f| !f.stage.is_final())
    }

    pub fn hint(&self, popup_open: bool) -> Option<Hint> {
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
            _ => None,
        }
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
        }
    }
}

/// Which of two things the popup shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupKind {
    /// The running verification (`VerifyState::flow`).
    Flow,
    /// An offer to create the account's cross-signing keys.
    SetUp,
    /// The encryption status of a device with nothing left to do.
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyPopup {
    pub kind: PopupKind,
    /// Index into [`VerifyPopup::buttons`].
    pub focus: usize,
}

impl VerifyPopup {
    pub fn flow() -> Self {
        Self {
            kind: PopupKind::Flow,
            focus: 0,
        }
    }

    pub fn set_up() -> Self {
        Self {
            kind: PopupKind::SetUp,
            focus: 0,
        }
    }

    pub fn status() -> Self {
        Self {
            kind: PopupKind::Status,
            focus: 0,
        }
    }

    /// The buttons shown right now, in order.
    pub fn buttons(&self, state: &VerifyState) -> &'static [Button] {
        use Button as B;
        match self.kind {
            PopupKind::SetUp if state.setting_up => &[],
            PopupKind::SetUp => &[B::SetUp, B::Close],
            PopupKind::Status => &[B::Close],
            PopupKind::Flow => match state.flow.as_ref().map(|f| &f.stage) {
                None => &[B::Close],
                Some(VerifyStage::Requested) => &[B::Accept, B::Decline],
                Some(VerifyStage::Ready) => &[B::Start, B::Cancel],
                Some(VerifyStage::Compare { .. }) => &[B::Match, B::Mismatch],
                Some(VerifyStage::Waiting | VerifyStage::Starting | VerifyStage::Confirmed) => {
                    &[B::Cancel]
                }
                Some(VerifyStage::Done | VerifyStage::Cancelled { .. }) => &[B::Close],
            },
        }
    }

    /// What Esc does: decline, cancel or close, whichever fits.
    pub fn escape(&self, state: &VerifyState) -> Button {
        match (self.kind, state.active().map(|f| &f.stage)) {
            (PopupKind::Flow, Some(VerifyStage::Requested)) => Button::Decline,
            (PopupKind::Flow, Some(_)) => Button::Cancel,
            _ => Button::Close,
        }
    }

    pub fn focused(&self, state: &VerifyState) -> Option<Button> {
        self.buttons(state).get(self.focus).copied()
    }

    /// Moves the keyboard focus, wrapping around.
    pub fn step(&mut self, state: &VerifyState, delta: isize) {
        let count = self.buttons(state).len() as isize;
        if count > 0 {
            self.focus = (self.focus as isize + delta).rem_euclid(count) as usize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::RecoveryStatus;

    fn flow(stage: VerifyStage) -> VerifyState {
        VerifyState {
            flow: Some(VerifyUpdate {
                flow: 1,
                peer: "phone".into(),
                own: true,
                this_device: true,
                stage,
            }),
            ..VerifyState::default()
        }
    }

    #[test]
    fn buttons_follow_the_stage() {
        let popup = VerifyPopup::flow();
        let state = flow(VerifyStage::Requested);
        assert_eq!(popup.buttons(&state), [Button::Accept, Button::Decline]);
        assert_eq!(popup.escape(&state), Button::Decline);
        let state = flow(VerifyStage::Compare {
            emoji: Vec::new(),
            decimals: (1, 2, 3),
        });
        assert_eq!(popup.buttons(&state), [Button::Match, Button::Mismatch]);
        assert_eq!(popup.escape(&state), Button::Cancel);
        let state = flow(VerifyStage::Cancelled {
            reason: "Timed out".into(),
        });
        assert_eq!(popup.buttons(&state), [Button::Close]);
        assert_eq!(popup.escape(&state), Button::Close);

        let mut offer = VerifyPopup::set_up();
        let mut state = VerifyState::default();
        assert_eq!(offer.focused(&state), Some(Button::SetUp));
        offer.step(&state, -1);
        assert_eq!(offer.focused(&state), Some(Button::Close));
        offer.step(&state, 1);
        assert_eq!(offer.focused(&state), Some(Button::SetUp));
        state.setting_up = true;
        assert!(offer.buttons(&state).is_empty());
        offer.step(&state, 1);
        assert_eq!(offer.focused(&state), None);
    }

    #[test]
    fn hints() {
        let mut state = VerifyState::default();
        // Nothing known yet (or demo mode): no hint.
        assert_eq!(state.hint(false), None);
        let mut security = Security {
            identity: Identity::Present,
            device: DeviceTrust::Unverified,
            recovery: RecoveryStatus::NeedsKey,
            ..Security::default()
        };
        state.security = Some(security);
        assert_eq!(state.hint(false), Some(Hint::Unverified));
        security.device = DeviceTrust::Verified;
        state.security = Some(security);
        assert_eq!(state.hint(false), None);
        security.identity = Identity::Missing;
        state.security = Some(security);
        assert_eq!(state.hint(false), Some(Hint::NotSetUp));
        // A request waiting behind another popup comes first, unless its popup is open.
        state.flow = flow(VerifyStage::Requested).flow;
        assert_eq!(state.hint(false), Some(Hint::Pending { requested: true }));
        assert_eq!(state.hint(true), Some(Hint::NotSetUp));
    }
}
