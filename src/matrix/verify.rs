//! Device verification (emoji comparison) and the encryption status of this device.
//!
//! The worker owns every SDK object here. The UI sees plain data: [`Security`] whenever
//! the status changes, and a [`VerifyUpdate`] whenever a running verification moves on.
//! One verification runs at a time; each gets a number (`flow`) that the UI quotes back.

use std::future::pending;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::{Stream, StreamExt, pin_mut};
use matrix_sdk::Client;
use matrix_sdk::encryption::VerificationState;
use matrix_sdk::encryption::backups::BackupState;
use matrix_sdk::encryption::verification::{
    CancelInfo, SasState, SasVerification, Verification, VerificationRequest,
    VerificationRequestState,
};
use matrix_sdk::event_handler::EventHandlerHandle;
use matrix_sdk::ruma::api::client::uiaa::{
    AuthData, MatrixUserIdentifier, Password, UserIdentifier,
};
use matrix_sdk::ruma::events::key::verification::VerificationMethod;
use matrix_sdk::ruma::events::key::verification::cancel::CancelCode;
use matrix_sdk::ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEvent;
use matrix_sdk::ruma::events::room::message::{MessageType, OriginalSyncRoomMessageEvent};
use matrix_sdk::ruma::events::secret::send::ToDeviceSecretSendEvent;
use matrix_sdk::ruma::{DeviceId, MilliSecondsSinceUnixEpoch, OwnedUserId};
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use super::{MatrixEvent, RecoveryStatus, recovery_status};
use crate::text::sanitize_line;

// ----- what the UI sees ---------------------------------------------------------------

/// Whether this device is trusted by the account's own cross-signing identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeviceTrust {
    #[default]
    Unknown,
    Verified,
    Unverified,
}

/// The account's cross-signing identity, which verified devices are signed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Identity {
    #[default]
    Unknown,
    /// The account has never set up cross-signing.
    Missing,
    Present,
}

/// The server-side backup of message keys, as far as this device takes part in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackupStatus {
    /// Not in use on this device (there may or may not be one on the server).
    #[default]
    Inactive,
    /// Being created, resumed or switched on.
    Enabling,
    /// Keys are being downloaded from the backup.
    Downloading,
    Enabled,
    Disabling,
}

/// The encryption status of this device. Sent whenever any part of it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Security {
    pub device: DeviceTrust,
    pub identity: Identity,
    /// This device holds the private cross-signing keys, so it can verify others.
    pub signing_keys: bool,
    /// Another of the user's devices is cross-signed, so there is something to verify
    /// this one with.
    pub other_devices: bool,
    /// The account's devices that take part in encryption, this one included.
    pub devices: usize,
    pub recovery: RecoveryStatus,
    pub backup: BackupStatus,
}

impl Default for Security {
    fn default() -> Self {
        Self {
            device: DeviceTrust::Unknown,
            identity: Identity::Unknown,
            signing_keys: false,
            other_devices: false,
            devices: 0,
            recovery: RecoveryStatus::Unknown,
            backup: BackupStatus::Inactive,
        }
    }
}

/// One of the seven pictures of an emoji comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SasEmoji {
    pub symbol: String,
    pub name: String,
}

/// Where a verification stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyStage {
    /// The other side asks to verify; accept or decline.
    Requested,
    /// We asked; nobody has answered yet.
    Waiting,
    /// Both sides agreed; the other side is expected to begin the comparison.
    Ready,
    /// The comparison is being set up.
    Starting,
    /// Compare these with what the other side shows.
    Compare {
        /// Empty if the other side cannot show emoji; then the numbers are compared.
        emoji: Vec<SasEmoji>,
        decimals: (u16, u16, u16),
    },
    /// We said they match; the other side has not yet.
    Confirmed,
    Done,
    /// Ended without success; `reason` is ready to show.
    Cancelled {
        reason: String,
    },
}

impl VerifyStage {
    pub fn is_final(&self) -> bool {
        matches!(self, Self::Done | Self::Cancelled { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyUpdate {
    pub flow: u64,
    /// The other side, ready to show: a device name or a user id.
    pub peer: String,
    /// The other side is another device of this account.
    pub own: bool,
    /// This device is the one that gets verified: it was not verified yet when the
    /// verification began. Otherwise it vouches for the other side.
    pub this_device: bool,
    pub stage: VerifyStage,
}

/// What the user decided, inside a running verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Accept,
    /// Begin the emoji comparison ourselves instead of waiting for the other side.
    Start,
    Match,
    Mismatch,
    /// Decline a request or abort a running verification; the other side is told.
    Cancel,
}

pub enum VerifyCmd {
    /// Ask the user's other devices to verify this one.
    Request,
    Answer {
        flow: u64,
        answer: Answer,
    },
    /// Create the account's cross-signing keys if it has none. Some servers want the
    /// account password for that.
    SetUp {
        password: Option<String>,
    },
}

/// Never prints the password.
impl std::fmt::Debug for VerifyCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request => f.write_str("Request"),
            Self::Answer { flow, answer } => write!(f, "Answer({flow}, {answer:?})"),
            Self::SetUp { password } => write!(f, "SetUp(password: {})", password.is_some()),
        }
    }
}

// ----- the worker side ----------------------------------------------------------------

/// A verification request that arrived.
pub struct Incoming {
    sender: OwnedUserId,
    flow_id: String,
    /// Sent to this device directly (the user's own devices do that), not in a room,
    /// and in the last few minutes.
    fresh_to_device: bool,
}

struct Running {
    flow: u64,
    /// We asked (rather than answered someone).
    outgoing: bool,
    answers: UnboundedSender<Answer>,
    finished: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

/// Lives as long as a session: watches for requests and runs verifications.
pub struct Verifier {
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    incoming: UnboundedReceiver<Incoming>,
    handlers: Vec<EventHandlerHandle>,
    next_flow: u64,
    running: Option<Running>,
    security: JoinHandle<()>,
    /// Asks the status watcher to look again.
    refresh: Arc<Notify>,
}

impl Verifier {
    pub fn start(client: &Client, events: UnboundedSender<MatrixEvent>) -> Self {
        let (tx, incoming) = unbounded_channel();
        let to_device = tx.clone();
        let from_device =
            client.add_event_handler(move |ev: ToDeviceKeyVerificationRequestEvent| {
                let tx = to_device.clone();
                async move {
                    let age = u64::from(MilliSecondsSinceUnixEpoch::now().get())
                        .saturating_sub(ev.content.timestamp.get().into());
                    let _ = tx.send(Incoming {
                        sender: ev.sender,
                        flow_id: ev.content.transaction_id.to_string(),
                        fresh_to_device: age < 5 * 60 * 1000,
                    });
                }
            });
        // Requests from other people arrive as messages in a direct chat.
        let in_room = client.add_event_handler(move |ev: OriginalSyncRoomMessageEvent| {
            let tx = tx.clone();
            async move {
                if matches!(ev.content.msgtype, MessageType::VerificationRequest(_)) {
                    let _ = tx.send(Incoming {
                        sender: ev.sender,
                        flow_id: ev.event_id.to_string(),
                        fresh_to_device: false,
                    });
                }
            }
        });
        let refresh = Arc::new(Notify::new());
        // After a verification, the other device sends the keys this one lacks. The SDK
        // has stored them by the time handlers run.
        let poke = refresh.clone();
        let secrets = client.add_event_handler(move |_: ToDeviceSecretSendEvent| {
            let poke = poke.clone();
            async move { poke.notify_one() }
        });
        let security = tokio::spawn(watch_security(
            client.clone(),
            events.clone(),
            refresh.clone(),
        ));
        Self {
            client: client.clone(),
            events,
            incoming,
            handlers: vec![from_device, in_room, secrets],
            next_flow: 0,
            running: None,
            security,
            refresh,
        }
    }

    /// The next verification request that arrived. Never ends while the session lasts.
    pub async fn next_incoming(&mut self) -> Incoming {
        match self.incoming.recv().await {
            Some(incoming) => incoming,
            // Cannot happen while the handlers hold senders; wait forever if it does.
            None => pending().await,
        }
    }

    fn busy(&self) -> bool {
        self.running
            .as_ref()
            .is_some_and(|r| !r.finished.load(Ordering::SeqCst))
    }

    pub async fn on_incoming(&mut self, incoming: Incoming) {
        let user_id = incoming.sender;
        let Some(request) = self
            .client
            .encryption()
            .get_verification_request(&user_id, &incoming.flow_id)
            .await
        else {
            // Too old, not for us, or ignored by the SDK. The usual case for one of our
            // own devices: it is so new that this one has not heard of it yet (the
            // server reports new devices with the next sync, which can take a while).
            // Fetch the device list now, so that asking again works.
            if incoming.fresh_to_device && self.client.user_id() == Some(&*user_id) {
                tracing::info!("verification request from an unknown own device");
                let client = self.client.clone();
                tokio::spawn(async move { refresh_own_keys(&client).await });
                let _ = self.events.send(MatrixEvent::ActionFailed(
                    "A device of yours that this one did not know yet asked for verification. \
                     Ask again from that device."
                        .to_owned(),
                ));
            }
            return;
        };
        if request.we_started()
            || !matches!(request.state(), VerificationRequestState::Requested { .. })
        {
            return;
        }
        if self.busy() {
            tracing::info!(%user_id, "ignoring a verification request while another one runs");
            let _ = self.events.send(MatrixEvent::ActionFailed(format!(
                "Ignored a verification request from {}: another verification is running",
                sanitize_line(user_id.as_str())
            )));
            return;
        }
        self.launch(Some(request));
    }

    pub async fn handle(&mut self, command: VerifyCmd) {
        match command {
            VerifyCmd::Request => {
                // Asking again replaces our own unanswered request.
                if self.busy() && !self.running.as_ref().is_some_and(|r| r.outgoing) {
                    let _ = self.events.send(MatrixEvent::ActionFailed(
                        "A verification is already running".to_owned(),
                    ));
                } else {
                    self.launch(None);
                }
            }
            VerifyCmd::Answer { flow, answer } => match &self.running {
                Some(running) if running.flow == flow => {
                    let _ = running.answers.send(answer);
                }
                // A late click on a verification that is over.
                _ => {}
            },
            VerifyCmd::SetUp { password } => {
                let client = self.client.clone();
                let events = self.events.clone();
                let refresh = self.refresh.clone();
                tokio::spawn(async move {
                    let result = set_up_cross_signing(&client, password).await;
                    if let Err(err) = &result {
                        tracing::warn!(err, "could not set up cross-signing");
                    }
                    let _ = events.send(MatrixEvent::CrossSigningSetUp(result));
                    refresh.notify_one();
                });
            }
        }
    }

    /// Runs a verification: an incoming `request`, or (with `None`) a new request to
    /// the user's other devices.
    fn launch(&mut self, request: Option<VerificationRequest>) {
        if let Some(old) = self.running.take() {
            if old.finished.load(Ordering::SeqCst) {
                old.task.abort();
            } else {
                // Let it tell the other side; it ends by itself after that.
                let _ = old.answers.send(Answer::Cancel);
            }
        }
        let outgoing = request.is_none();
        self.next_flow += 1;
        let (answers, answer_rx) = unbounded_channel();
        let finished = Arc::new(AtomicBool::new(false));
        let flow = Flow {
            id: self.next_flow,
            events: self.events.clone(),
            refresh: self.refresh.clone(),
            finished: finished.clone(),
            peer: "your other devices".to_owned(),
            own: true,
            this_device: true,
            last: None,
        };
        let client = self.client.clone();
        let task = tokio::spawn(flow.run(client, request, answer_rx));
        self.running = Some(Running {
            flow: self.next_flow,
            outgoing,
            answers,
            finished,
            task,
        });
    }

    /// Ends the session's verification work. A running verification is abandoned; the
    /// other side sees it time out.
    pub fn stop(self) {
        for handler in self.handlers {
            self.client.remove_event_handler(handler);
        }
        if let Some(running) = self.running {
            running.task.abort();
        }
        self.security.abort();
    }
}

/// Creates the account's cross-signing identity unless it has one. Servers that follow
/// MSC3967 allow the first upload without a password.
async fn set_up_cross_signing(client: &Client, password: Option<String>) -> Result<(), String> {
    let encryption = client.encryption();
    let err = match encryption.bootstrap_cross_signing_if_needed(None).await {
        Ok(()) => {
            refresh_own_keys(client).await;
            return Ok(());
        }
        Err(err) => err,
    };
    let Some(info) = err.as_uiaa_response() else {
        return Err(format!("Could not set up encryption: {err}"));
    };
    let (Some(password), Some(user_id)) = (password, client.user_id()) else {
        return Err("Your server asks for your password to set up encryption".to_owned());
    };
    let mut auth = Password::new(
        UserIdentifier::Matrix(MatrixUserIdentifier::new(user_id.to_string())),
        password,
    );
    auth.session = info.session.clone();
    encryption
        .bootstrap_cross_signing(Some(AuthData::Password(auth)))
        .await
        .map_err(|err| format!("Could not set up encryption: {err}"))?;
    refresh_own_keys(client).await;
    Ok(())
}

/// Fetches this account's devices and identity from the server now. New signatures on
/// our own device otherwise only show up when the server next mentions a change, which
/// can take a minute while nothing else happens.
async fn refresh_own_keys(client: &Client) {
    if let Some(user_id) = client.user_id()
        && let Err(err) = client.encryption().request_user_identity(user_id).await
    {
        tracing::warn!(%err, "could not fetch our own keys");
    }
}

/// After this device was verified: the other device uploads its signature for this
/// one a moment after the verification ends. Look until it is there.
async fn await_own_signature(client: Client, refresh: Arc<Notify>) {
    for delay in [0, 1, 2, 4, 8] {
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        if matches!(
            client.encryption().get_own_device().await,
            Ok(Some(device)) if device.is_cross_signed_by_owner()
        ) {
            break;
        }
        refresh_own_keys(&client).await;
        refresh.notify_one();
    }
}

type SasStream = Pin<Box<dyn Stream<Item = SasState> + Send>>;

/// One verification, from request to result. Runs on its own task.
struct Flow {
    id: u64,
    events: UnboundedSender<MatrixEvent>,
    refresh: Arc<Notify>,
    finished: Arc<AtomicBool>,
    peer: String,
    own: bool,
    this_device: bool,
    /// The stage last sent to the UI.
    last: Option<VerifyStage>,
}

impl Flow {
    fn show(&mut self, stage: VerifyStage) {
        if self.last.as_ref() == Some(&stage) {
            return;
        }
        if stage.is_final() {
            self.finished.store(true, Ordering::SeqCst);
            // Signatures and secrets change the status; look again.
            self.refresh.notify_one();
        }
        let _ = self.events.send(MatrixEvent::Verification(VerifyUpdate {
            flow: self.id,
            peer: self.peer.clone(),
            own: self.own,
            this_device: self.this_device,
            stage: stage.clone(),
        }));
        self.last = Some(stage);
    }

    fn done(&self) -> bool {
        self.last.as_ref().is_some_and(VerifyStage::is_final)
    }

    fn fail(&mut self, what: &str, err: impl std::fmt::Display) {
        tracing::warn!(flow = self.id, %err, "verification: could not {what}");
        let _ = self.events.send(MatrixEvent::ActionFailed(format!(
            "Could not {what}: {err}"
        )));
    }

    fn set_peer(
        &mut self,
        user_id: &matrix_sdk::ruma::UserId,
        own_user: &matrix_sdk::ruma::UserId,
        name: Option<&str>,
        device: &DeviceId,
    ) {
        self.own = user_id == own_user;
        self.peer = if self.own {
            device_label(name, device)
        } else {
            sanitize_line(user_id.as_str())
        };
    }

    async fn run(
        mut self,
        client: Client,
        request: Option<VerificationRequest>,
        mut answers: UnboundedReceiver<Answer>,
    ) {
        let request = match request {
            Some(request) => request,
            None => {
                self.show(VerifyStage::Waiting);
                match request_own(&client).await {
                    Ok(request) => request,
                    Err(reason) => return self.show(VerifyStage::Cancelled { reason }),
                }
            }
        };
        if !request.is_self_verification() {
            self.own = false;
            self.peer = sanitize_line(request.other_user_id().as_str());
        }
        self.this_device = request.is_self_verification()
            && !matches!(
                client.encryption().get_own_device().await,
                Ok(Some(device)) if device.is_cross_signed_by_owner()
            );
        // Subscribe before reading the current state, so no change is missed.
        let changes = request.changes();
        pin_mut!(changes);
        let mut sas: Option<SasVerification> = None;
        let mut sas_changes: Option<SasStream> = None;
        self.on_request(&request, request.state(), &mut sas, &mut sas_changes)
            .await;

        while !self.done() {
            tokio::select! {
                state = changes.next() => match state {
                    Some(state) => self.on_request(&request, state, &mut sas, &mut sas_changes).await,
                    None => break,
                },
                Some(state) = async {
                    match sas_changes.as_mut() {
                        Some(stream) => stream.next().await,
                        None => pending().await,
                    }
                } => {
                    if let Some(current) = sas.clone() {
                        self.on_sas(&current, state).await;
                    }
                }
                answer = answers.recv() => match answer {
                    Some(answer) => self.answer(&request, sas.as_ref(), answer).await,
                    // The worker let go of this verification.
                    None => break,
                },
            }
        }
        if self.this_device && self.last == Some(VerifyStage::Done) {
            tokio::spawn(await_own_signature(client, self.refresh.clone()));
        }
    }

    async fn on_request(
        &mut self,
        request: &VerificationRequest,
        state: VerificationRequestState,
        sas: &mut Option<SasVerification>,
        sas_changes: &mut Option<SasStream>,
    ) {
        let own_user = request.own_user_id().to_owned();
        match state {
            VerificationRequestState::Created { .. } => self.show(VerifyStage::Waiting),
            VerificationRequestState::Requested {
                other_device_data, ..
            } => {
                self.set_peer(
                    other_device_data.user_id(),
                    &own_user,
                    other_device_data.display_name(),
                    other_device_data.device_id(),
                );
                self.show(VerifyStage::Requested);
            }
            VerificationRequestState::Ready {
                other_device_data, ..
            } => {
                self.set_peer(
                    other_device_data.user_id(),
                    &own_user,
                    other_device_data.display_name(),
                    other_device_data.device_id(),
                );
                if request.we_started() {
                    // Whoever asked begins the comparison, like Element X does.
                    self.show(VerifyStage::Starting);
                    if let Err(err) = request.start_sas().await {
                        self.fail("start the emoji comparison", err);
                    }
                } else {
                    self.show(VerifyStage::Ready);
                }
            }
            VerificationRequestState::Transitioned { verification } => match verification {
                Verification::SasV1(new) => {
                    // Subscribe first, then handle the current state.
                    *sas_changes = Some(Box::pin(new.changes()));
                    *sas = Some(new.clone());
                    let state = new.state();
                    self.on_sas(&new, state).await;
                }
                // Only emoji comparison is offered, so this does not happen.
                _ => {
                    let _ = request.cancel().await;
                    self.show(VerifyStage::Cancelled {
                        reason: "The other device chose a method matrix-rat does not support"
                            .to_owned(),
                    });
                }
            },
            VerificationRequestState::Done => self.show(VerifyStage::Done),
            VerificationRequestState::Cancelled(info) => self.show(VerifyStage::Cancelled {
                reason: cancel_reason(&info),
            }),
        }
    }

    async fn on_sas(&mut self, sas: &SasVerification, state: SasState) {
        let other = sas.other_device();
        self.set_peer(
            sas.other_user_id(),
            sas.own_user_id(),
            other.display_name(),
            other.device_id(),
        );
        match state {
            SasState::Created { .. } | SasState::Accepted { .. } => {
                self.show(VerifyStage::Starting)
            }
            SasState::Started { .. } => {
                // The other side began the comparison: the user already agreed to verify.
                self.show(VerifyStage::Starting);
                if !sas.we_started()
                    && let Err(err) = sas.accept().await
                {
                    self.fail("accept the emoji comparison", err);
                }
            }
            SasState::KeysExchanged { emojis, decimals } => self.show(VerifyStage::Compare {
                emoji: emojis
                    .map(|sas| {
                        sas.emojis
                            .iter()
                            .map(|e| SasEmoji {
                                symbol: e.symbol.to_owned(),
                                name: e.description.to_owned(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                decimals,
            }),
            SasState::Confirmed => self.show(VerifyStage::Confirmed),
            SasState::Done { .. } => self.show(VerifyStage::Done),
            SasState::Cancelled(info) => self.show(VerifyStage::Cancelled {
                reason: cancel_reason(&info),
            }),
        }
    }

    async fn answer(
        &mut self,
        request: &VerificationRequest,
        sas: Option<&SasVerification>,
        answer: Answer,
    ) {
        let result = match (answer, sas) {
            (Answer::Accept, _) => request
                .accept_with_methods(vec![VerificationMethod::SasV1])
                .await
                .map_err(|e| ("accept the request", e)),
            (Answer::Start, None) => request
                .start_sas()
                .await
                .map(|_| ())
                .map_err(|e| ("start the emoji comparison", e)),
            (Answer::Start, Some(_)) => Ok(()),
            (Answer::Match, Some(sas)) => sas.confirm().await.map_err(|e| ("confirm", e)),
            (Answer::Mismatch, Some(sas)) => sas.mismatch().await.map_err(|e| ("cancel", e)),
            (Answer::Match | Answer::Mismatch, None) => Ok(()),
            (Answer::Cancel, Some(sas)) if !sas.is_done() => {
                sas.cancel().await.map_err(|e| ("cancel", e))
            }
            (Answer::Cancel, _) => request.cancel().await.map_err(|e| ("cancel", e)),
        };
        if let Err((what, err)) = result {
            self.fail(what, err);
        }
    }
}

/// Sends a verification request to all of the user's other devices.
async fn request_own(client: &Client) -> Result<VerificationRequest, String> {
    let encryption = client.encryption();
    let user_id = client
        .user_id()
        .ok_or_else(|| "Not logged in".to_owned())?
        .to_owned();
    let identity = encryption
        .get_user_identity(&user_id)
        .await
        .map_err(|err| format!("Could not read the encryption store: {err}"))?
        .ok_or_else(|| {
            "Your account has no cross-signing keys yet, so there is nothing to verify against"
                .to_owned()
        })?;
    if !own_devices(client).await.1 {
        return Err(
            "None of your other devices can verify this one. Use your recovery key instead."
                .to_owned(),
        );
    }
    identity
        .request_verification_with_methods(vec![VerificationMethod::SasV1])
        .await
        .map_err(|err| format!("Could not send the request: {err}"))
}

/// How many devices this account has, and whether another one is cross-signed, so it
/// can vouch for this one.
async fn own_devices(client: &Client) -> (usize, bool) {
    let (Some(user_id), Some(own)) = (client.user_id(), client.device_id()) else {
        return (0, false);
    };
    match client.encryption().get_user_devices(user_id).await {
        Ok(devices) => {
            let devices: Vec<_> = devices.devices().filter(|d| !d.is_dehydrated()).collect();
            let other = devices
                .iter()
                .any(|d| d.device_id() != own && d.is_cross_signed_by_owner());
            (devices.len(), other)
        }
        Err(_) => (0, false),
    }
}

fn device_label(name: Option<&str>, device: &DeviceId) -> String {
    match name.map(sanitize_line).filter(|n| !n.trim().is_empty()) {
        Some(name) => format!("{name} ({device})"),
        None => device.to_string(),
    }
}

fn cancel_reason(info: &CancelInfo) -> String {
    let by = if info.cancelled_by_us() {
        "You"
    } else {
        "The other side"
    };
    match info.cancel_code() {
        CancelCode::Timeout => "Timed out: nobody answered in time".to_owned(),
        CancelCode::User => format!("{by} cancelled the verification"),
        CancelCode::MismatchedSas => {
            "The emoji did not match. Nothing was verified; if this keeps happening, \
             someone may be interfering."
                .to_owned()
        }
        CancelCode::Accepted => "Another of your devices answered the request".to_owned(),
        _ => format!("Cancelled: {}", info.reason()),
    }
}

// ----- the status watcher -------------------------------------------------------------

/// Sends [`Security`] at the start and whenever it changes, for the whole session.
async fn watch_security(
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    refresh: Arc<Notify>,
) {
    let Some(user_id) = client.user_id().map(ToOwned::to_owned) else {
        return;
    };
    let encryption = client.encryption();
    let verification = encryption.verification_state();
    let recovery = encryption.recovery().state_stream();
    let backup = encryption.backups().state_stream();
    let devices = encryption.devices_stream().await.ok();
    let identities = encryption.user_identities_stream().await.ok();
    pin_mut!(verification, recovery, backup);
    let mut devices = devices.map(Box::pin);
    let mut identities = identities.map(Box::pin);

    // The server has confirmed the account has no cross-signing identity.
    let mut confirmed_missing = false;
    let mut last = None;
    loop {
        let now = security(&client, &user_id, &mut confirmed_missing).await;
        if last != Some(now) {
            tracing::info!(?now, "encryption status");
            let _ = events.send(MatrixEvent::Security(now));
            last = Some(now);
        }
        // Wait for anything that may have changed it.
        tokio::select! {
            Some(_) = verification.next() => {}
            Some(_) = recovery.next() => {}
            Some(_) = backup.next() => {}
            Some(updates) = async {
                match devices.as_mut() {
                    Some(stream) => stream.next().await,
                    None => pending().await,
                }
            } => {
                if !updates.new.contains_key(&user_id) && !updates.changed.contains_key(&user_id) {
                    continue;
                }
            }
            Some(updates) = async {
                match identities.as_mut() {
                    Some(stream) => stream.next().await,
                    None => pending().await,
                }
            } => {
                if !updates.new.contains_key(&user_id) && !updates.changed.contains_key(&user_id) {
                    continue;
                }
            }
            _ = refresh.notified() => {}
        }
    }
}

async fn security(
    client: &Client,
    user_id: &matrix_sdk::ruma::UserId,
    confirmed_missing: &mut bool,
) -> Security {
    let encryption = client.encryption();
    let device = match encryption.get_own_device().await {
        Ok(Some(device)) if device.is_cross_signed_by_owner() => DeviceTrust::Verified,
        Ok(Some(_)) => DeviceTrust::Unverified,
        _ => match encryption.verification_state().get() {
            VerificationState::Verified => DeviceTrust::Verified,
            VerificationState::Unverified => DeviceTrust::Unverified,
            VerificationState::Unknown => DeviceTrust::Unknown,
        },
    };
    let identity = match encryption.get_user_identity(user_id).await {
        Ok(Some(_)) => {
            *confirmed_missing = false;
            Identity::Present
        }
        Ok(None) if *confirmed_missing => Identity::Missing,
        // Not known locally (yet): ask the server.
        Ok(None) => match encryption.request_user_identity(user_id).await {
            Ok(Some(_)) => Identity::Present,
            Ok(None) => {
                *confirmed_missing = true;
                Identity::Missing
            }
            Err(_) => Identity::Unknown,
        },
        Err(_) => Identity::Unknown,
    };
    let signing_keys = encryption
        .cross_signing_status()
        .await
        .is_some_and(|status| status.is_complete());
    let backup = match encryption.backups().state() {
        BackupState::Unknown => BackupStatus::Inactive,
        BackupState::Creating | BackupState::Enabling | BackupState::Resuming => {
            BackupStatus::Enabling
        }
        BackupState::Downloading => BackupStatus::Downloading,
        BackupState::Enabled => BackupStatus::Enabled,
        BackupState::Disabling => BackupStatus::Disabling,
    };
    let (devices, other_devices) = own_devices(client).await;
    Security {
        device,
        identity,
        signing_keys,
        other_devices,
        devices,
        recovery: recovery_status(encryption.recovery().state()),
        backup,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use matrix_sdk::ruma::api::client::account::register::v3::Request as RegisterRequest;
    use matrix_sdk::ruma::api::client::uiaa::RegistrationToken;

    use super::*;
    use crate::matrix::{MatrixCmd, MatrixHandle, spawn};
    use crate::paths::Paths;

    #[test]
    fn debug_output_hides_the_password() {
        let command = MatrixCmd::Verification(VerifyCmd::SetUp {
            password: Some("hunter2".into()),
        });
        assert!(!format!("{command:?}").contains("hunter2"));
    }

    /// Creates a new account, so the test never touches the cross-signing keys of an
    /// account someone else uses. Needs the server's registration token.
    async fn register(homeserver: &str, name: &str, password: &str, token: &str) {
        let client = Client::builder()
            .homeserver_url(homeserver)
            .build()
            .await
            .expect("client");
        let mut request = RegisterRequest::new();
        request.username = Some(name.to_owned());
        request.password = Some(password.to_owned());
        // Only the account is needed; the devices come from the logins below.
        request.inhibit_login = true;
        let session = match client.matrix_auth().register(request.clone()).await {
            Ok(_) => return,
            Err(err) => err
                .as_uiaa_response()
                .unwrap_or_else(|| panic!("registration failed: {err}"))
                .session
                .clone(),
        };
        let mut auth = RegistrationToken::new(token.to_owned());
        auth.session = session;
        request.auth = Some(AuthData::RegistrationToken(auth));
        client
            .matrix_auth()
            .register(request)
            .await
            .expect("registration with token");
    }

    /// Waits for an event the check accepts, failing on errors.
    async fn wait_for(
        events: &mut UnboundedReceiver<MatrixEvent>,
        what: &str,
        mut check: impl FnMut(&MatrixEvent) -> bool,
    ) {
        let wait = async {
            loop {
                let event = events.recv().await.expect("worker stopped");
                match &event {
                    MatrixEvent::LoginFailed(err)
                    | MatrixEvent::Error(err)
                    | MatrixEvent::ActionFailed(err) => panic!("while waiting for {what}: {err}"),
                    MatrixEvent::CrossSigningSetUp(Err(err)) => panic!("set-up failed: {err}"),
                    _ => {}
                }
                if check(&event) {
                    return;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(60), wait)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    /// One side of a verification, driven the way the UI does: accept what comes in,
    /// confirm the emoji (and remember them).
    fn step(handle: &MatrixHandle, event: MatrixEvent, emoji: &mut Vec<SasEmoji>, done: &mut bool) {
        let update = match event {
            MatrixEvent::Verification(update) => update,
            MatrixEvent::Error(err) | MatrixEvent::ActionFailed(err) => panic!("{err}"),
            _ => return,
        };
        let answer = |answer| {
            handle.send(MatrixCmd::Verification(VerifyCmd::Answer {
                flow: update.flow,
                answer,
            }))
        };
        eprintln!(
            "flow {} with {}: {:?}",
            update.flow, update.peer, update.stage
        );
        assert!(update.own, "a verification with someone else");
        match update.stage {
            VerifyStage::Requested => answer(Answer::Accept),
            VerifyStage::Compare { emoji: shown, .. } => {
                *emoji = shown;
                answer(Answer::Match);
            }
            VerifyStage::Done => *done = true,
            VerifyStage::Cancelled { reason } => panic!("verification cancelled: {reason}"),
            _ => {}
        }
    }

    /// Two devices of one new account: the first sets up cross-signing, the second asks
    /// it for verification, both confirm the emoji, and the second ends up verified,
    /// with the signing keys passed over. Start the homeserver with
    /// `scripts/dev-homeserver.sh`; then
    /// `cargo test two_devices_verify -- --ignored --nocapture`.
    /// `MATRIX_RAT_TEST_HOMESERVER` and `MATRIX_RAT_TEST_REGISTRATION_TOKEN` override the
    /// defaults `http://127.0.0.1:6167` and `devtoken`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn two_devices_verify_each_other() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
            .with_test_writer()
            .try_init();
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let token = std::env::var("MATRIX_RAT_TEST_REGISTRATION_TOKEN")
            .unwrap_or_else(|_| "devtoken".to_owned());
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let name = format!("ratverify{stamp}");
        let password = format!("pw-{stamp}");
        register(&homeserver, &name, &password, &token).await;

        let root = std::env::temp_dir().join(format!("matrix-rat-verify-{stamp}"));
        let paths = |device: &str| {
            let dir = root.join(device);
            let paths = Paths {
                config_dir: dir.join("config"),
                data_dir: dir.join("data"),
                log_dir: dir.join("log"),
                runtime_dir: dir.join("run"),
                profile: device.into(),
            };
            paths.create_dirs().unwrap();
            paths
        };
        let login = || MatrixCmd::LoginPassword {
            homeserver: homeserver.clone(),
            username: name.clone(),
            password: password.clone(),
        };

        // First device: a new account has no cross-signing until asked for it.
        let (a, mut a_events) = spawn(paths("a"));
        a.send(login());
        wait_for(
            &mut a_events,
            "no identity on A",
            |e| matches!(e, MatrixEvent::Security(s) if s.identity == Identity::Missing),
        )
        .await;
        a.send(MatrixCmd::Verification(VerifyCmd::SetUp {
            password: Some(password.clone()),
        }));
        // The two pieces of news may come in either order.
        let (mut set_up, mut verified) = (false, false);
        wait_for(&mut a_events, "set-up and A verified", |e| {
            match e {
                MatrixEvent::CrossSigningSetUp(Ok(())) => set_up = true,
                MatrixEvent::Security(s) => {
                    verified = s.device == DeviceTrust::Verified && s.signing_keys;
                }
                _ => {}
            }
            set_up && verified
        })
        .await;

        // Second device: unverified, with A to verify against.
        let (b, mut b_events) = spawn(paths("b"));
        b.send(login());
        wait_for(&mut b_events, "B unverified with A present", |e| {
            matches!(e, MatrixEvent::Security(s)
                if s.identity == Identity::Present
                    && s.device == DeviceTrust::Unverified
                    && s.other_devices
                    && !s.signing_keys)
        })
        .await;
        // A ignores requests from devices it has not heard of yet.
        wait_for(
            &mut a_events,
            "A to learn about B",
            |e| matches!(e, MatrixEvent::Security(s) if s.devices == 2),
        )
        .await;
        b.send(MatrixCmd::Verification(VerifyCmd::Request));

        let (mut a_emoji, mut b_emoji) = (Vec::new(), Vec::new());
        let (mut a_done, mut b_done) = (false, false);
        let run = async {
            while !(a_done && b_done) {
                tokio::select! {
                    Some(event) = a_events.recv() => step(&a, event, &mut a_emoji, &mut a_done),
                    Some(event) = b_events.recv() => step(&b, event, &mut b_emoji, &mut b_done),
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(90), run)
            .await
            .expect("timed out during the verification");
        assert_eq!(a_emoji.len(), 7);
        assert_eq!(a_emoji, b_emoji, "both devices must show the same emoji");

        // B gets signed by A and receives the private keys from it, without a restart.
        wait_for(&mut b_events, "B verified with the keys", |e| {
            matches!(e, MatrixEvent::Security(s)
                if s.device == DeviceTrust::Verified && s.signing_keys)
        })
        .await;

        for (handle, mut events) in [(a, a_events), (b, b_events)] {
            handle.send(MatrixCmd::Shutdown);
            while events.recv().await.is_some() {}
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// The other device as a plain SDK client (like Element would be): answers one
    /// verification request and confirms the emoji.
    async fn answer_one_request(client: Client) {
        let (tx, mut rx) = unbounded_channel();
        client.add_event_handler(move |ev: ToDeviceKeyVerificationRequestEvent| {
            let tx = tx.clone();
            async move {
                let _ = tx.send((ev.sender, ev.content.transaction_id.to_string()));
            }
        });
        let (user_id, flow_id) = rx.recv().await.expect("a request");
        let request = client
            .encryption()
            .get_verification_request(&user_id, &flow_id)
            .await
            .expect("known request");
        let changes = request.changes();
        pin_mut!(changes);
        request.accept().await.expect("accept");
        let sas = loop {
            if let VerificationRequestState::Transitioned { verification } = request.state()
                && let Some(sas) = verification.sas()
            {
                break sas;
            }
            changes.next().await.expect("request changes");
        };
        let sas_changes = sas.changes();
        pin_mut!(sas_changes);
        sas.accept().await.expect("accept the comparison");
        loop {
            match sas.state() {
                SasState::KeysExchanged { .. } => sas.confirm().await.expect("confirm"),
                SasState::Done { .. } => return,
                SasState::Cancelled(info) => panic!("cancelled: {}", info.reason()),
                _ => {}
            }
            sas_changes.next().await.expect("comparison changes");
        }
    }

    /// Encrypted history written before this device existed becomes readable once it is
    /// verified: the other device sends the backup key, and the timeline decrypts the
    /// old message by itself. The first device is a plain SDK client, because only it
    /// can create the key backup that matrix-rat cannot set up yet. Same setup and
    /// variables as `two_devices_verify_each_other`;
    /// `cargo test history_unlocks -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn history_unlocks_after_verification() {
        use matrix_sdk::config::SyncSettings;
        use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoom;
        use matrix_sdk::ruma::events::InitialStateEvent;
        use matrix_sdk::ruma::events::room::encryption::RoomEncryptionEventContent;
        use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;

        use crate::model::MsgKind;

        let _ = tracing_subscriber::fmt()
            .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
            .with_test_writer()
            .try_init();
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let token = std::env::var("MATRIX_RAT_TEST_REGISTRATION_TOKEN")
            .unwrap_or_else(|_| "devtoken".to_owned());
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let name = format!("rathistory{stamp}");
        let password = format!("pw-{stamp}");
        register(&homeserver, &name, &password, &token).await;

        // Device A: cross-signing, recovery with a key backup, one encrypted message.
        let a = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .expect("client A");
        a.matrix_auth()
            .login_username(&name, &password)
            .send()
            .await
            .expect("login A");
        a.sync_once(SyncSettings::default()).await.expect("sync A");
        set_up_cross_signing(&a, Some(password.clone()))
            .await
            .expect("cross-signing on A");
        a.encryption()
            .recovery()
            .enable()
            .await
            .expect("recovery on A");
        let mut create = CreateRoom::new();
        create.initial_state = vec![
            InitialStateEvent::with_empty_state_key(
                RoomEncryptionEventContent::with_recommended_defaults(),
            )
            .to_raw_any(),
        ];
        let room = a.create_room(create).await.expect("room");
        let secret = format!("written before B existed {stamp}");
        room.send(RoomMessageEventContent::text_plain(&secret))
            .await
            .expect("send");
        a.encryption()
            .backups()
            .wait_for_steady_state()
            .await
            .expect("room key backed up");
        let syncing = tokio::spawn({
            let a = a.clone();
            async move { a.sync(SyncSettings::default()).await }
        });
        let answering = tokio::spawn(answer_one_request(a.clone()));

        // Device B: matrix-rat. The message is there but cannot be read.
        let root = std::env::temp_dir().join(format!("matrix-rat-history-{stamp}"));
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "b".into(),
        };
        paths.create_dirs().unwrap();
        let (b, mut events) = spawn(paths);
        b.send(MatrixCmd::LoginPassword {
            homeserver: homeserver.clone(),
            username: name.clone(),
            password: password.clone(),
        });
        let room_id = room.room_id().to_string();
        let mut opened = false;
        let mut locked = false;
        let mut ready = false;
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
                    locked |= messages.iter().any(|m| m.kind == MsgKind::Undecryptable);
                }
                MatrixEvent::Security(s) => ready = s.other_devices,
                _ => {}
            }
            locked && ready
        })
        .await;
        // A must have heard of B before B asks.
        let wait_for_a = async {
            let user_id = a.user_id().unwrap().to_owned();
            while a
                .encryption()
                .get_user_devices(&user_id)
                .await
                .unwrap()
                .devices()
                .count()
                < 2
            {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(30), wait_for_a)
            .await
            .expect("A never saw B");

        b.send(MatrixCmd::Verification(VerifyCmd::Request));
        let (mut emoji, mut done) = (Vec::new(), false);
        let run = async {
            while !done {
                let event = events.recv().await.expect("worker stopped");
                step(&b, event, &mut emoji, &mut done);
            }
        };
        tokio::time::timeout(Duration::from_secs(60), run)
            .await
            .expect("timed out during the verification");
        tokio::time::timeout(Duration::from_secs(10), answering)
            .await
            .expect("A did not finish")
            .expect("A failed");

        // No restart: the backup key arrives, and the old message decrypts.
        let (mut verified, mut backup, mut readable) = (false, false, false);
        wait_for(
            &mut events,
            "B verified, with backup, reading history",
            |e| {
                match e {
                    MatrixEvent::Security(s) => {
                        verified = s.device == DeviceTrust::Verified;
                        backup = s.backup == BackupStatus::Enabled;
                    }
                    MatrixEvent::Timeline { messages, .. } => {
                        readable = messages
                            .iter()
                            .any(|m| m.body == secret && m.kind != MsgKind::Undecryptable);
                    }
                    _ => {}
                }
                verified && backup && readable
            },
        )
        .await;

        b.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        syncing.abort();
        let _ = std::fs::remove_dir_all(root);
    }
}
