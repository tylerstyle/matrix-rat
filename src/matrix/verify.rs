//! Device verification (emoji comparison) and the encryption status of this device.
//!
//! The worker owns every SDK object here. The UI sees plain data: [`Security`] whenever
//! the status changes, and a [`VerifyUpdate`] whenever a running verification moves on.
//! One verification runs at a time; each gets a number (`flow`) that the UI quotes back.

use std::future::{Future, pending};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
use matrix_sdk::ruma::{DeviceId, MilliSecondsSinceUnixEpoch, OwnedUserId, UserId};
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::{JoinHandle, JoinSet};

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
    /// Our request is unanswered: withdraw it and send a new one.
    Retry,
}

pub enum VerifyCmd {
    /// Ask the user's other devices to verify this one.
    Request,
    Answer {
        flow: u64,
        answer: Answer,
    },
    /// Create the account's cross-signing keys if it has none. Tried without a
    /// password first; some servers then ask for the account password.
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

/// How setting up cross-signing went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetUpOutcome {
    Done,
    /// The server wants the account password; `rejected` if the one given was wrong.
    NeedsPassword {
        rejected: bool,
    },
    Failed(String),
}

// ----- the worker side ----------------------------------------------------------------

/// A verification request that arrived.
pub struct Incoming {
    sender: OwnedUserId,
    flow_id: String,
    /// Sent in the last few minutes.
    fresh: bool,
}

/// A request older than this has expired (the SDK gives them ten minutes; a few
/// minutes of clock difference are allowed for).
const FRESH_MS: u64 = 5 * 60 * 1000;

fn age_ms(sent: MilliSecondsSinceUnixEpoch) -> u64 {
    u64::from(MilliSecondsSinceUnixEpoch::now().get()).saturating_sub(sent.get().into())
}

struct Running {
    flow: u64,
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
    /// Everything else this session started (status watcher, set-up, key fetches,
    /// declining requests); all of it ends with the session.
    tasks: JoinSet<()>,
    /// Asks the status watcher to look again.
    refresh: Arc<Notify>,
}

/// An in-room verification request is for us only if it names us: in a room with
/// several people, everyone sees requests meant for someone else.
fn addressed_to(msgtype: &MessageType, own: Option<&UserId>) -> bool {
    match msgtype {
        MessageType::VerificationRequest(request) => Some(&*request.to) == own,
        _ => false,
    }
}

impl Verifier {
    pub fn start(client: &Client, events: UnboundedSender<MatrixEvent>) -> Self {
        let (tx, incoming) = unbounded_channel();
        let to_device = tx.clone();
        let from_device =
            client.add_event_handler(move |ev: ToDeviceKeyVerificationRequestEvent| {
                let tx = to_device.clone();
                async move {
                    let _ = tx.send(Incoming {
                        sender: ev.sender,
                        flow_id: ev.content.transaction_id.to_string(),
                        fresh: age_ms(ev.content.timestamp) < FRESH_MS,
                    });
                }
            });
        // Requests from other people arrive as messages in a direct chat.
        let own = client.user_id().map(ToOwned::to_owned);
        let in_room = client.add_event_handler(move |ev: OriginalSyncRoomMessageEvent| {
            let tx = tx.clone();
            let addressed = addressed_to(&ev.content.msgtype, own.as_deref());
            async move {
                if addressed {
                    let _ = tx.send(Incoming {
                        sender: ev.sender,
                        flow_id: ev.event_id.to_string(),
                        fresh: age_ms(ev.origin_server_ts) < FRESH_MS,
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
        let mut tasks = JoinSet::new();
        tasks.spawn(watch_security(
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
            tasks,
            refresh,
        }
    }

    /// Runs `work` for this session only.
    fn spawn(&mut self, work: impl Future<Output = ()> + Send + 'static) {
        // Forget the ones that are done.
        while self.tasks.try_join_next().is_some() {}
        self.tasks.spawn(work);
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
            // Too old, or ignored by the SDK because this device does not know the
            // sender's device (yet): one of our own devices that just logged in (the
            // server reports new devices with the next sync, which can take a while), or
            // someone whose devices were never fetched (nothing was sent to them yet).
            // Fetch them now, so that asking again works.
            if incoming.fresh {
                let own = self.client.user_id() == Some(&*user_id);
                tracing::info!(own, "verification request from an unknown device");
                let client = self.client.clone();
                let sender = user_id.clone();
                self.spawn(async move {
                    if let Err(err) = client.encryption().request_user_identity(&sender).await {
                        tracing::warn!(%err, "could not fetch the keys of {sender}");
                    }
                });
                let _ = self.events.send(MatrixEvent::ActionFailed(if own {
                    "A device of yours that this one did not know yet asked for verification. \
                     Ask again from that device."
                        .to_owned()
                } else {
                    format!(
                        "{} asked to verify you before this device knew their keys. \
                         Ask them to try again.",
                        sanitize_line(user_id.as_str())
                    )
                }));
            }
            return;
        };
        if request.we_started()
            || !matches!(request.state(), VerificationRequestState::Requested { .. })
        {
            return;
        }
        if self.busy() {
            // One at a time. Say no rather than leave the sender waiting.
            tracing::info!(%user_id, "declining a verification request while another one runs");
            let _ = self.events.send(MatrixEvent::ActionFailed(format!(
                "Declined a verification request from {}: another verification is running",
                sanitize_line(user_id.as_str())
            )));
            self.spawn(async move {
                if let Err(err) = request.cancel().await {
                    tracing::warn!(%err, "could not decline a verification request");
                }
            });
            return;
        }
        self.launch(Some(request));
    }

    pub async fn handle(&mut self, command: VerifyCmd) {
        match command {
            VerifyCmd::Request => {
                if self.busy() {
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
                self.spawn(async move {
                    let outcome = set_up_cross_signing(&client, password).await;
                    if let SetUpOutcome::Failed(err) = &outcome {
                        tracing::warn!(err, "could not set up cross-signing");
                    }
                    let _ = events.send(MatrixEvent::CrossSigningSetUp(outcome));
                    refresh.notify_one();
                });
            }
        }
    }

    /// Runs a verification: an incoming `request`, or (with `None`) a new request to
    /// the user's other devices. Only called when no other one runs.
    fn launch(&mut self, request: Option<VerificationRequest>) {
        if let Some(old) = self.running.take() {
            // Over already; at most still waiting for its signature to show up.
            old.task.abort();
        }
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
            answers,
            finished,
            task,
        });
    }

    /// Ends the session's verification work. A running verification is cancelled
    /// (briefly waiting for the other side to be told); everything else stops.
    pub async fn stop(mut self) {
        for handler in self.handlers.drain(..) {
            self.client.remove_event_handler(handler);
        }
        if let Some(running) = self.running.take() {
            if !running.finished.load(Ordering::SeqCst) {
                let _ = running.answers.send(Answer::Cancel);
                let mut task = running.task;
                if tokio::time::timeout(Duration::from_secs(2), &mut task)
                    .await
                    .is_err()
                {
                    task.abort();
                }
            } else {
                running.task.abort();
            }
        }
        self.tasks.shutdown().await;
    }
}

/// Creates the account's cross-signing identity unless it has one. Servers that follow
/// MSC3967 allow the first upload without a password; others ask for it.
async fn set_up_cross_signing(client: &Client, password: Option<String>) -> SetUpOutcome {
    let encryption = client.encryption();
    let failed = |err: matrix_sdk::Error| {
        SetUpOutcome::Failed(format!("Could not set up encryption: {err}"))
    };
    let first = match &password {
        None => encryption.bootstrap_cross_signing_if_needed(None).await,
        // An attempt before created the keys here already; upload those, and get the
        // server's session for the password along the way.
        Some(_) => encryption.bootstrap_cross_signing(None).await,
    };
    let err = match first {
        Ok(()) => {
            refresh_own_keys(client).await;
            return SetUpOutcome::Done;
        }
        Err(err) => err,
    };
    let Some(info) = err.as_uiaa_response() else {
        return failed(err);
    };
    let (Some(password), Some(user_id)) = (password, client.user_id()) else {
        return SetUpOutcome::NeedsPassword { rejected: false };
    };
    let mut auth = Password::new(
        UserIdentifier::Matrix(MatrixUserIdentifier::new(user_id.to_string())),
        password,
    );
    auth.session = info.session.clone();
    match encryption
        .bootstrap_cross_signing(Some(AuthData::Password(auth)))
        .await
    {
        Ok(()) => {
            refresh_own_keys(client).await;
            SetUpOutcome::Done
        }
        Err(err) if err.as_uiaa_response().is_some() => {
            SetUpOutcome::NeedsPassword { rejected: true }
        }
        Err(err) => failed(err),
    }
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
async fn await_own_signature(client: &Client, refresh: &Notify) {
    for delay in [0, 1, 2, 4, 8] {
        tokio::time::sleep(Duration::from_secs(delay)).await;
        if matches!(
            client.encryption().get_own_device().await,
            Ok(Some(device)) if device.is_cross_signed_by_owner()
        ) {
            break;
        }
        refresh_own_keys(client).await;
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

/// What became of a verification request.
enum Ended {
    /// Done, cancelled, or abandoned by the worker.
    Over,
    /// The user asked again: here is the new request.
    Replaced(VerificationRequest),
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
        user_id: &UserId,
        own_user: &UserId,
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
        let mut request = match request {
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
        while let Ended::Replaced(new) = self.follow(&client, &request, &mut answers).await {
            request = new;
        }
        if self.this_device && self.last == Some(VerifyStage::Done) {
            await_own_signature(&client, &self.refresh).await;
        }
    }

    /// Follows one request until it ends, or the user replaces it.
    async fn follow(
        &mut self,
        client: &Client,
        request: &VerificationRequest,
        answers: &mut UnboundedReceiver<Answer>,
    ) -> Ended {
        // Subscribe before reading the current state, so no change is missed.
        let changes = request.changes();
        pin_mut!(changes);
        let mut sas: Option<SasVerification> = None;
        let mut sas_changes: Option<SasStream> = None;
        self.on_request(request, request.state(), &mut sas, &mut sas_changes)
            .await;

        while !self.done() {
            tokio::select! {
                state = changes.next() => match state {
                    Some(state) => self.on_request(request, state, &mut sas, &mut sas_changes).await,
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
                    Some(Answer::Retry) => {
                        if let Some(new) = self.retry(client, request).await {
                            return Ended::Replaced(new);
                        }
                    }
                    Some(answer) => self.answer(request, sas.as_ref(), answer).await,
                    // The worker let go of this verification.
                    None => break,
                },
            }
        }
        Ended::Over
    }

    /// Withdraws our unanswered request and sends a new one. The SDK cancels both
    /// requests if a new one starts while the old one is still open, so the old one is
    /// cancelled (which the SDK records at once, before sending) before asking again.
    async fn retry(
        &mut self,
        client: &Client,
        request: &VerificationRequest,
    ) -> Option<VerificationRequest> {
        let unanswered = request.we_started()
            && matches!(request.state(), VerificationRequestState::Created { .. });
        if !unanswered {
            return None;
        }
        if let Err(err) = request.cancel().await {
            // The SDK has marked it cancelled all the same; only the other devices may
            // keep showing it until it times out.
            tracing::warn!(%err, "could not withdraw a verification request");
        }
        debug_assert!(request.is_cancelled());
        match request_own(client).await {
            Ok(new) => Some(new),
            Err(reason) => {
                self.show(VerifyStage::Cancelled { reason });
                None
            }
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
            // Handled by `follow`.
            (Answer::Retry, _) => Ok(()),
            (Answer::Cancel, sas) => {
                let result = match sas {
                    Some(sas) if !sas.is_done() => sas.cancel().await,
                    _ => request.cancel().await,
                };
                // The user has moved on: a failure to tell the other side is only
                // logged, not shown.
                if let Err(err) = result {
                    tracing::warn!(flow = self.id, %err, "could not send the cancellation");
                }
                Ok(())
            }
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
        let recovery = recovery_status(encryption.recovery().state());
        return Err(if recovery == RecoveryStatus::NeedsKey {
            "None of your other devices can verify this one. Use your recovery key instead."
        } else {
            "None of your other devices can verify this one, and there is no recovery key \
             to enter."
        }
        .to_owned());
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
        // Worded by the SDK, but it ends up on screen: keep it harmless anyway.
        _ => format!("Cancelled: {}", sanitize_line(info.reason())),
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

    let mut checks = IdentityChecks::default();
    let mut last = None;
    loop {
        let now = security(&client, &user_id, &mut checks).await;
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

/// What the server said about the account's cross-signing identity.
#[derive(Default)]
struct IdentityChecks {
    /// The account has none.
    confirmed_missing: bool,
    /// The identity this device created is on the server.
    published: bool,
}

/// Whether the server has the account's cross-signing identity. Asked directly, because
/// the SDK keeps an identity it created locally even when the upload failed (a server
/// that wants a password, and the user gave none).
async fn identity_published(client: &Client, user_id: &UserId) -> matrix_sdk::Result<bool> {
    use matrix_sdk::ruma::api::client::keys::get_keys;
    let mut request = get_keys::v3::Request::new();
    request.device_keys.insert(user_id.to_owned(), Vec::new());
    let response = client.send(request).await?;
    Ok(response.master_keys.contains_key(user_id))
}

async fn security(client: &Client, user_id: &UserId, checks: &mut IdentityChecks) -> Security {
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
    let signing_keys = encryption
        .cross_signing_status()
        .await
        .is_some_and(|status| status.is_complete());
    let identity = match encryption.get_user_identity(user_id).await {
        // Made here but maybe never uploaded: only the server can tell. (Once this
        // device is signed by it, it is on the server.)
        Ok(Some(_)) if signing_keys && device != DeviceTrust::Verified && !checks.published => {
            match identity_published(client, user_id).await {
                Ok(true) => {
                    checks.published = true;
                    Identity::Present
                }
                Ok(false) => Identity::Missing,
                // Offline: believe the store.
                Err(_) => Identity::Present,
            }
        }
        Ok(Some(_)) => {
            checks.confirmed_missing = false;
            Identity::Present
        }
        Ok(None) if checks.confirmed_missing => Identity::Missing,
        // Not known locally (yet): ask the server.
        Ok(None) => match encryption.request_user_identity(user_id).await {
            Ok(Some(_)) => Identity::Present,
            Ok(None) => {
                checks.confirmed_missing = true;
                Identity::Missing
            }
            Err(_) => Identity::Unknown,
        },
        Err(_) => Identity::Unknown,
    };
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
    fn only_requests_addressed_to_us_count() {
        use matrix_sdk::ruma::events::room::message::KeyVerificationRequestEventContent;
        use matrix_sdk::ruma::{owned_device_id, owned_user_id};
        let me = owned_user_id!("@me:example.org");
        let request = |to| {
            MessageType::VerificationRequest(KeyVerificationRequestEventContent::new(
                "verify".into(),
                vec![VerificationMethod::SasV1],
                owned_device_id!("DEVICE"),
                to,
            ))
        };
        assert!(addressed_to(&request(me.clone()), Some(&me)));
        assert!(!addressed_to(
            &request(owned_user_id!("@bob:example.org")),
            Some(&me)
        ));
        assert!(!addressed_to(&request(me.clone()), None));
        assert!(!addressed_to(&MessageType::text_plain("hi"), Some(&me)));
    }

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
                    MatrixEvent::CrossSigningSetUp(SetUpOutcome::Failed(err)) => {
                        panic!("set-up failed: {err}")
                    }
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

    fn trace() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
            .with_test_writer()
            .try_init();
    }

    fn homeserver() -> String {
        std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned())
    }

    /// A new account, named after `prefix` and the time. Returns name and password.
    async fn new_account(prefix: &str) -> (String, String) {
        let token = std::env::var("MATRIX_RAT_TEST_REGISTRATION_TOKEN")
            .unwrap_or_else(|_| "devtoken".to_owned());
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let name = format!("{prefix}{stamp}");
        let password = format!("pw-{stamp}");
        register(&homeserver(), &name, &password, &token).await;
        (name, password)
    }

    /// matrix-rat workers on one new account: `a` with cross-signing set up (and
    /// verified), `b` freshly logged in and unverified, and `a` aware of `b`.
    struct Pair {
        root: std::path::PathBuf,
        a: MatrixHandle,
        a_events: UnboundedReceiver<MatrixEvent>,
        b: MatrixHandle,
        b_events: UnboundedReceiver<MatrixEvent>,
    }

    fn worker_paths(root: &std::path::Path, device: &str) -> Paths {
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
    }

    /// Logs a worker in and sets up cross-signing on the new account.
    async fn first_device(
        root: &std::path::Path,
        name: &str,
        password: &str,
    ) -> (MatrixHandle, UnboundedReceiver<MatrixEvent>) {
        let (a, mut a_events) = spawn(worker_paths(root, "a"));
        a.send(MatrixCmd::LoginPassword {
            homeserver: homeserver(),
            username: name.to_owned(),
            password: password.to_owned(),
        });
        // A new account has no cross-signing until asked for it.
        wait_for(
            &mut a_events,
            "no identity on A",
            |e| matches!(e, MatrixEvent::Security(s) if s.identity == Identity::Missing),
        )
        .await;
        a.send(MatrixCmd::Verification(VerifyCmd::SetUp {
            password: Some(password.to_owned()),
        }));
        // The two pieces of news may come in either order.
        let (mut set_up, mut verified) = (false, false);
        wait_for(&mut a_events, "set-up and A verified", |e| {
            match e {
                MatrixEvent::CrossSigningSetUp(SetUpOutcome::Done) => set_up = true,
                MatrixEvent::Security(s) => {
                    verified = s.device == DeviceTrust::Verified && s.signing_keys;
                }
                _ => {}
            }
            set_up && verified
        })
        .await;
        (a, a_events)
    }

    async fn pair(prefix: &str) -> Pair {
        let (name, password) = new_account(prefix).await;
        let root = std::env::temp_dir().join(format!("matrix-rat-{name}"));
        let (a, mut a_events) = first_device(&root, &name, &password).await;
        // Second device: unverified, with A to verify against.
        let (b, mut b_events) = spawn(worker_paths(&root, "b"));
        b.send(MatrixCmd::LoginPassword {
            homeserver: homeserver(),
            username: name,
            password,
        });
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
        Pair {
            root,
            a,
            a_events,
            b,
            b_events,
        }
    }

    impl Pair {
        /// Both sides answer and confirm until both are done; B must end up verified
        /// with the keys passed over. Returns the emoji both showed.
        async fn finish(&mut self) -> Vec<SasEmoji> {
            let (mut a_emoji, mut b_emoji) = (Vec::new(), Vec::new());
            let (mut a_done, mut b_done) = (false, false);
            let run = async {
                while !(a_done && b_done) {
                    tokio::select! {
                        Some(event) = self.a_events.recv() => step(&self.a, event, &mut a_emoji, &mut a_done),
                        Some(event) = self.b_events.recv() => step(&self.b, event, &mut b_emoji, &mut b_done),
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(90), run)
                .await
                .expect("timed out during the verification");
            assert_eq!(a_emoji.len(), 7);
            assert_eq!(a_emoji, b_emoji, "both devices must show the same emoji");
            // B gets signed by A and receives the private keys, without a restart.
            wait_for(&mut self.b_events, "B verified with the keys", |e| {
                matches!(e, MatrixEvent::Security(s)
                    if s.device == DeviceTrust::Verified && s.signing_keys)
            })
            .await;
            a_emoji
        }

        async fn shut_down(self) {
            for (handle, mut events) in [(self.a, self.a_events), (self.b, self.b_events)] {
                handle.send(MatrixCmd::Shutdown);
                while events.recv().await.is_some() {}
            }
            let _ = std::fs::remove_dir_all(self.root);
        }
    }

    /// The next verification update, skipping other news.
    async fn next_update(events: &mut UnboundedReceiver<MatrixEvent>, what: &str) -> VerifyUpdate {
        let wait = async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::Verification(update) => {
                        eprintln!("{what}: flow {} {:?}", update.flow, update.stage);
                        return update;
                    }
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(30), wait)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    /// Two devices of one new account: the first sets up cross-signing, the second asks
    /// it for verification, both confirm the emoji, and the second ends up verified,
    /// with the signing keys passed over. Start the homeserver with
    /// `scripts/dev-homeserver.sh`; then
    /// `cargo test verify::tests -- --ignored --nocapture --test-threads=1`.
    /// `MATRIX_RAT_TEST_HOMESERVER` and `MATRIX_RAT_TEST_REGISTRATION_TOKEN` override the
    /// defaults `http://127.0.0.1:6167` and `devtoken`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn two_devices_verify_each_other() {
        trace();
        let mut pair = pair("ratverify").await;
        pair.b.send(MatrixCmd::Verification(VerifyCmd::Request));
        pair.finish().await;
        pair.shut_down().await;
    }

    /// "Ask again" against the real SDK: the first request is withdrawn before the
    /// second is made (otherwise the SDK cancels both), the other device sees the first
    /// one cancelled and the second one arrive, and the second one works.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn asking_again_works() {
        trace();
        let mut pair = pair("ratretry").await;
        pair.b.send(MatrixCmd::Verification(VerifyCmd::Request));
        let b_flow = next_update(&mut pair.b_events, "B").await;
        assert_eq!(b_flow.stage, VerifyStage::Waiting);
        let first = next_update(&mut pair.a_events, "A").await;
        assert_eq!(first.stage, VerifyStage::Requested);
        // A leaves it unanswered; B asks again.
        pair.b.send(MatrixCmd::Verification(VerifyCmd::Answer {
            flow: b_flow.flow,
            answer: Answer::Retry,
        }));
        let gone = next_update(&mut pair.a_events, "A").await;
        assert_eq!(gone.flow, first.flow);
        assert!(
            matches!(&gone.stage, VerifyStage::Cancelled { reason } if reason.contains("other side")),
            "{gone:?}"
        );
        let second = next_update(&mut pair.a_events, "A").await;
        assert!(second.flow > first.flow);
        assert_eq!(second.stage, VerifyStage::Requested);
        pair.a.send(MatrixCmd::Verification(VerifyCmd::Answer {
            flow: second.flow,
            answer: Answer::Accept,
        }));
        // From here on as usual; B must never see its request cancelled.
        pair.finish().await;
        pair.shut_down().await;
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
        assert_eq!(
            set_up_cross_signing(&a, Some(password.clone())).await,
            SetUpOutcome::Done
        );
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

    /// A plain SDK client (like Element) on a new account, with cross-signing, syncing
    /// in the background.
    async fn plain_client(prefix: &str) -> (Client, JoinHandle<()>) {
        use matrix_sdk::config::SyncSettings;
        let (name, password) = new_account(prefix).await;
        let client = Client::builder()
            .homeserver_url(homeserver())
            .build()
            .await
            .expect("client");
        client
            .matrix_auth()
            .login_username(&name, &password)
            .send()
            .await
            .expect("login");
        client
            .sync_once(SyncSettings::default())
            .await
            .expect("sync");
        assert_eq!(
            set_up_cross_signing(&client, Some(password)).await,
            SetUpOutcome::Done
        );
        let syncing = tokio::spawn({
            let client = client.clone();
            async move {
                let _ = client.sync(SyncSettings::default()).await;
            }
        });
        (client, syncing)
    }

    /// An encrypted direct chat from `client` with `user`.
    async fn direct_chat(client: &Client, user: &UserId) -> String {
        use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoom;
        use matrix_sdk::ruma::events::InitialStateEvent;
        use matrix_sdk::ruma::events::room::encryption::RoomEncryptionEventContent;
        let mut create = CreateRoom::new();
        create.invite = vec![user.to_owned()];
        create.is_direct = true;
        create.initial_state = vec![
            InitialStateEvent::with_empty_state_key(
                RoomEncryptionEventContent::with_recommended_defaults(),
            )
            .to_raw_any(),
        ];
        let room = client.create_room(create).await.expect("room");
        room.room_id().to_string()
    }

    /// Asks `user` for verification (in the direct chat), as Element would.
    async fn request_user(client: &Client, user: &UserId) -> VerificationRequest {
        let identity = loop {
            if let Some(identity) = client
                .encryption()
                .request_user_identity(user)
                .await
                .expect("keys")
            {
                break identity;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        };
        identity
            .request_verification_with_methods(vec![VerificationMethod::SasV1])
            .await
            .expect("request")
    }

    /// The asking side, as Element plays it: begins the comparison once the other side
    /// is ready, and confirms the emoji.
    async fn run_requester(request: VerificationRequest) {
        let changes = request.changes();
        pin_mut!(changes);
        let sas = loop {
            match request.state() {
                VerificationRequestState::Ready { .. } => {
                    break request.start_sas().await.expect("start").expect("sas");
                }
                VerificationRequestState::Cancelled(info) => panic!("cancelled: {}", info.reason()),
                _ => {}
            }
            changes.next().await.expect("request changes");
        };
        let sas_changes = sas.changes();
        pin_mut!(sas_changes);
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

    /// Other people verify a matrix-rat user in a direct chat: the request names us and
    /// is accepted; a second person asking meanwhile is declined (not left waiting);
    /// the comparison ends with each side trusting the other.
    /// `cargo test other_people -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn other_people_verify_in_a_direct_chat() {
        use std::collections::HashSet;

        use crate::matrix::RoomCategory;

        trace();
        let (name, password) = new_account("ratpeer").await;
        let root = std::env::temp_dir().join(format!("matrix-rat-{name}"));
        let (u, mut u_events) = first_device(&root, &name, &password).await;
        let (v, v_sync) = plain_client("ratpeerv").await;
        let (w, w_sync) = plain_client("ratpeerw").await;
        let server = v.user_id().unwrap().server_name().to_owned();
        let u_id = UserId::parse(format!("@{name}:{server}")).unwrap();
        let chats: HashSet<String> =
            [direct_chat(&v, &u_id).await, direct_chat(&w, &u_id).await].into();

        // matrix-rat accepts both invitations.
        let mut asked = HashSet::new();
        let mut joined = HashSet::new();
        wait_for(&mut u_events, "U in both chats", |e| {
            if let MatrixEvent::Rooms(rooms) = e {
                for room in rooms.iter().filter(|r| chats.contains(&r.id)) {
                    if room.category == RoomCategory::Invite {
                        if asked.insert(room.id.clone()) {
                            u.send(MatrixCmd::Join {
                                room_id: room.id.clone(),
                            });
                        }
                    } else {
                        joined.insert(room.id.clone());
                    }
                }
            }
            joined.len() == 2
        })
        .await;

        // V asks. U may not know V's device yet and then ignores it: ask again.
        let mut v_request = None;
        let mut u_flow = None;
        for _ in 0..4 {
            let request = request_user(&v, &u_id).await;
            let seen = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match u_events.recv().await.expect("worker stopped") {
                        MatrixEvent::Verification(update) => return update,
                        MatrixEvent::Error(err) => panic!("{err}"),
                        _ => {}
                    }
                }
            })
            .await;
            match seen {
                Ok(update) => {
                    assert_eq!(update.stage, VerifyStage::Requested);
                    assert!(!update.own);
                    assert_eq!(update.peer, v.user_id().unwrap().as_str());
                    u_flow = Some(update.flow);
                    v_request = Some(request);
                    break;
                }
                Err(_) => {
                    let _ = request.cancel().await;
                }
            }
        }
        let (v_request, u_flow) = (v_request.expect("U never saw V's request"), u_flow.unwrap());

        // W asks meanwhile: U is busy and says no, rather than leave W waiting. (Again
        // U may not know W's device at first.)
        let mut declined = false;
        for _ in 0..4 {
            let w_request = request_user(&w, &u_id).await;
            let w_changes = w_request.changes();
            pin_mut!(w_changes);
            let cancelled = tokio::time::timeout(Duration::from_secs(10), async {
                while !w_request.is_cancelled() {
                    w_changes.next().await;
                }
            })
            .await;
            if cancelled.is_ok() {
                declined = true;
                break;
            }
            let _ = w_request.cancel().await;
        }
        assert!(declined, "W's request was not declined");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match u_events.recv().await.expect("worker stopped") {
                    MatrixEvent::ActionFailed(text) if text.starts_with("Declined") => return,
                    MatrixEvent::Verification(update) => panic!("unexpected: {update:?}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("U did not say it declined");

        // U accepts V; V begins the comparison; both confirm.
        u.send(MatrixCmd::Verification(VerifyCmd::Answer {
            flow: u_flow,
            answer: Answer::Accept,
        }));
        let requester = tokio::spawn(run_requester(v_request));
        let run = async {
            loop {
                let update = next_update(&mut u_events, "U").await;
                assert_eq!(update.flow, u_flow);
                assert!(!update.own);
                match update.stage {
                    VerifyStage::Compare { .. } => {
                        u.send(MatrixCmd::Verification(VerifyCmd::Answer {
                            flow: u_flow,
                            answer: Answer::Match,
                        }));
                    }
                    VerifyStage::Done => return,
                    VerifyStage::Cancelled { reason } => panic!("cancelled: {reason}"),
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(60), run)
            .await
            .expect("timed out during the verification");
        tokio::time::timeout(Duration::from_secs(10), requester)
            .await
            .expect("V did not finish")
            .expect("V failed");
        // V signed U's identity; it shows once V fetches it back from the server.
        let trusted = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let identity = v.encryption().request_user_identity(&u_id).await;
                if identity.ok().flatten().is_some_and(|i| i.is_verified()) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
        .await;
        assert!(trusted.is_ok(), "V does not trust U after the verification");

        u.send(MatrixCmd::Shutdown);
        while u_events.recv().await.is_some() {}
        v_sync.abort();
        w_sync.abort();
        let _ = std::fs::remove_dir_all(root);
    }
}
