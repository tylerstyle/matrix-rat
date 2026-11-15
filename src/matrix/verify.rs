//! Device verification (emoji comparison) and the encryption status of this device.
//!
//! The worker owns every SDK object here. The UI sees plain data: [`Security`] whenever
//! the status changes, and a [`VerifyUpdate`] whenever a running verification moves on.
//! One verification runs at a time; each gets a number (`flow`) that the UI quotes back.

use std::collections::{BTreeMap, VecDeque};
use std::future::{Future, pending};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::{Stream, StreamExt, pin_mut};
use matrix_sdk::Client;
use matrix_sdk::config::RequestConfig;
use matrix_sdk::encryption::VerificationState;
use matrix_sdk::encryption::backups::BackupState;
use matrix_sdk::encryption::verification::{
    CancelInfo, SasState, SasVerification, Verification, VerificationRequest,
    VerificationRequestState,
};
use matrix_sdk::event_handler::EventHandlerHandle;
use matrix_sdk::ruma::api::client::to_device::send_event_to_device::v3::Request as SendToDevice;
use matrix_sdk::ruma::api::client::uiaa::{
    AuthData, MatrixUserIdentifier, Password, UserIdentifier,
};
use matrix_sdk::ruma::events::key::verification::VerificationMethod;
use matrix_sdk::ruma::events::key::verification::cancel::CancelCode;
use matrix_sdk::ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEvent;
use matrix_sdk::ruma::events::room::encrypted::{
    EncryptedEventScheme, OriginalSyncRoomEncryptedEvent, RoomEncryptedEventContent,
};
use matrix_sdk::ruma::events::room::message::{MessageType, OriginalSyncRoomMessageEvent};
use matrix_sdk::ruma::events::secret::send::ToDeviceSecretSendEvent;
use matrix_sdk::ruma::events::{
    AnySyncMessageLikeEvent, AnyToDeviceEventContent, MessageLikeEventType, ToDeviceEventType,
};
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::to_device::DeviceIdOrAllDevices;
use matrix_sdk::ruma::{
    DeviceId, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedUserId, TransactionId, UserId,
};
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::{JoinHandle, JoinSet};

use super::{MatrixEvent, RecoveryStatus, Secret, recovery_status};
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
    /// Ask one particular device of the user for a verification: to verify it, if
    /// this device is verified, or else to get this one verified by it.
    RequestDevice {
        device_id: String,
    },
    /// Ask another Matrix user to verify identities in a direct chat.
    RequestUser {
        user_id: String,
    },
    Answer {
        flow: u64,
        answer: Answer,
    },
    /// Create the account's cross-signing keys if it has none. Tried without a
    /// password first; some servers then ask for the account password.
    SetUp {
        password: Option<Secret>,
    },
}

/// Never prints the password or target user id.
impl std::fmt::Debug for VerifyCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request => f.write_str("Request"),
            Self::RequestDevice { .. } => f.write_str("RequestDevice"),
            Self::RequestUser { .. } => f.write_str("RequestUser"),
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

/// A request or step further in the past or future than this is ignored (the SDK
/// allows ten minutes past and five minutes ahead; a few minutes of clock
/// difference are allowed for).
const FRESH_MS: u64 = 5 * 60 * 1000;

fn is_fresh_at(now: MilliSecondsSinceUnixEpoch, sent: MilliSecondsSinceUnixEpoch) -> bool {
    within(now, sent, FRESH_MS)
}

fn within(now: MilliSecondsSinceUnixEpoch, sent: MilliSecondsSinceUnixEpoch, ms: u64) -> bool {
    u64::from(now.get()).abs_diff(sent.get().into()) < ms
}

// ----- hurrying key requests ----------------------------------------------------------

/// A message that cannot be decrypted counts as new if it was sent this recently.
/// Anything older is history (a room being loaded, a backlog after a pause): its key
/// request may wait the 30 seconds. The time is the sender's server's, so the window
/// reaches as far ahead as back ([`within`]): a message stamped up to two minutes
/// into the future by a clock that runs fast still counts as new.
const UTD_FRESH_MS: u64 = 2 * 60 * 1000;
/// Nothing is hurried until the first sync of the session has been through for this
/// long: what arrives until then is the backlog.
const WAKE_SETTLE: Duration = Duration::from_secs(20);
/// At most this many wakes at once, and one more every [`WAKE_EVERY`] after that.
const WAKE_BURST: u32 = 2;
const WAKE_EVERY: Duration = Duration::from_secs(10);
/// How many megolm sessions are remembered as "seen undecryptable".
const WAKE_SESSIONS: usize = 512;

/// Decides which undecryptable messages are worth a wake of [`send_queued`], so that a
/// key request for a message that just arrived leaves at once, and nothing else costs
/// a request. Every limit holds whatever arrives:
///
/// - a megolm session gets one wake at most (the SDK asks for a session's key once,
///   so a second message of the same session has nothing new to send), and none if
///   it was first seen as history: its key was asked for when that was loaded. A
///   session that was refused only for the reasons below is not counted as seen, so
///   a later message of it (still new by its own time) can get the wake;
/// - only messages sent in the last two minutes count, and none before the session's
///   first sync has been through for a while;
/// - at most [`WAKE_BURST`] wakes in a row and one per [`WAKE_EVERY`] in the long run
///   (never more than eight in any minute), however many rooms and sessions there
///   are;
/// - none while no key request can be waiting: the SDK only asks when another of
///   this account's devices is verified.
///
/// Only session ids are kept, in memory, and nothing here is logged.
struct KeyRequestWakes {
    /// When the first sync of this session was through.
    live_since: Option<Instant>,
    /// Sessions a message of which could not be decrypted, oldest first.
    seen: VecDeque<String>,
    /// Wakes that may be made right now.
    ration: u32,
    /// When the ration was last counted up.
    counted: Instant,
}

impl KeyRequestWakes {
    fn new(now: Instant) -> Self {
        Self {
            live_since: None,
            seen: VecDeque::new(),
            ration: WAKE_BURST,
            counted: now,
        }
    }

    fn caught_up(&mut self, now: Instant) {
        self.live_since.get_or_insert(now);
    }

    /// A message of megolm session `session` could not be decrypted; `fresh` if it
    /// was sent just now, `askable` if the SDK can have queued a request for its key.
    /// Whether to wake for it.
    fn allow(&mut self, now: Instant, session: &str, fresh: bool, askable: bool) -> bool {
        if self.seen.iter().any(|seen| seen == session) {
            return false;
        }
        if !fresh {
            // History: its key was asked for when it was loaded. A later message of
            // the session has nothing new to send.
            self.remember(session);
            return false;
        }
        let settled = self
            .live_since
            .is_some_and(|since| now.saturating_duration_since(since) >= WAKE_SETTLE);
        if !settled || !askable {
            return false;
        }
        let earned = now.saturating_duration_since(self.counted).as_secs() / WAKE_EVERY.as_secs();
        if earned > 0 {
            let earned = u32::try_from(earned).unwrap_or(u32::MAX);
            self.ration = self.ration.saturating_add(earned).min(WAKE_BURST);
            self.counted = if self.ration == WAKE_BURST {
                now
            } else {
                self.counted + WAKE_EVERY * earned
            };
        }
        if self.ration == 0 {
            return false;
        }
        if self.ration == WAKE_BURST {
            // The wait for the next one starts when the full ration is first used.
            self.counted = now;
        }
        self.ration -= 1;
        self.remember(session);
        true
    }

    fn remember(&mut self, session: &str) {
        self.seen.push_back(session.to_owned());
        if self.seen.len() > WAKE_SESSIONS {
            self.seen.pop_front();
        }
    }
}

/// The megolm session of an encrypted event, if it has one.
fn megolm_session(content: &RoomEncryptedEventContent) -> Option<&str> {
    match &content.scheme {
        EncryptedEventScheme::MegolmV1AesSha2(megolm) => Some(&megolm.session_id),
        _ => None,
    }
}

fn is_fresh(sent: MilliSecondsSinceUnixEpoch) -> bool {
    is_fresh_at(MilliSecondsSinceUnixEpoch::now(), sent)
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
    /// Rations the wakes made for messages that cannot be decrypted.
    key_wakes: Arc<Mutex<KeyRequestWakes>>,
}

/// An in-room verification request is for us only if it names us: in a room with
/// several people, everyone sees requests meant for someone else.
fn addressed_to(msgtype: &MessageType, own: Option<&UserId>) -> bool {
    match msgtype {
        MessageType::VerificationRequest(request) => Some(&*request.to) == own,
        _ => false,
    }
}

/// Whether this room event is a step of a verification taken by someone else just now
/// (anything after the request itself: ready, start, accept, key, mac, done, cancel).
/// The SDK may answer such a step by itself; see [`send_queued`].
fn in_room_step(event: &AnySyncMessageLikeEvent, own: Option<&UserId>) -> bool {
    use MessageLikeEventType as T;
    matches!(
        event.event_type(),
        T::KeyVerificationReady
            | T::KeyVerificationStart
            | T::KeyVerificationAccept
            | T::KeyVerificationKey
            | T::KeyVerificationMac
            | T::KeyVerificationDone
            | T::KeyVerificationCancel
    ) && Some(event.sender()) != own
        // Not history that comes in when a room is first loaded, nor skewed far into the future.
        && is_fresh(event.origin_server_ts())
}

/// Makes the SDK send what its verification code has queued, each time `wake` is
/// notified.
///
/// Some steps of a verification are answered by the SDK itself: our key after the other
/// side's key, our "done" (and the upload of our signature) after their confirmation,
/// a cancellation after a step that does not fit. It does not send these answers; it
/// queues them (`VerificationMachine::queue_up_content` in matrix-sdk-crypto 0.19.1),
/// and the queue is only emptied by `Client::send_outgoing_requests`, which is private
/// and which, under the `SyncService`, runs only when the encryption sync starts a new
/// request (`SlidingSync::send_sync_request`, guarded by `is_e2ee_enabled`). The room
/// list sync, which is the one that delivers verification steps taken in a room, does
/// not run it. So the answer to a step taken in a direct chat stays queued until the
/// encryption sync's long poll returns for some other reason: up to 30 seconds, twice
/// per verification (the emoji show late, and the other side waits for our "done").
/// Verifications between one's own devices are not affected: their steps arrive by the
/// encryption sync itself, which sends the queue with its next request.
///
/// Seen with the server test `other_people_verify_in_a_direct_chat`, which failed about
/// every other run. The local homeserver answers both polls when a room event arrives,
/// so it was a race there (a server that leaves the encryption poll alone would give
/// the full wait every time). From a failing run, SDK logs of the device under test:
///
/// ```text
/// 18:45:44.933435 sync_once{conn_id="encryption" pos="429863"}: Received response
/// 18:45:44.942276 sync_once{conn_id="room-list" pos="429863"}: Received response
/// 18:45:44.943630 sync_once{conn_id="encryption" pos="429868"}: Sending request
/// 18:45:44.943957 sync_once{conn_id="encryption" ..}:send_outgoing_requests: ..
/// 18:45:44.944305 sync_once{conn_id="room-list" ..}:handle_response:receive_any_event:
///                 SAS received an event and changed its state .. new_state=KeyReceived
/// 18:45:44.944348 .. Storing the request info, waiting for the request to be marked
///                 as sent
/// 18:46:14.9      (the test gives up; the encryption sync sent no request in between)
/// ```
///
/// The queue was read 0.4 ms before our key was put in it.
///
/// The SDK has no public call that empties the queue or ends that poll (short of
/// `SyncService::expire_sessions`, which starts both syncs over), so we end the poll
/// from outside: an empty `m.dummy` message to this very device ("should be ignored"
/// says the spec, and the SDK does: `OlmMachine::handle_to_device_event`) makes the
/// server answer the poll, and the SDK sends the queue with the request that follows.
/// The message carries nothing and goes to nobody else.
///
/// We never send the queue ourselves, so nothing can go out twice, and the order of
/// things does not matter: whether the message reaches the server before or after the
/// encryption sync's current request, that request is answered because of it and the
/// next one is made after the answer was queued (the handler that wakes us runs after
/// the SDK has worked the step in). If the message cannot be sent, or a server does
/// not answer the poll for it, the answer goes out as late as before. One small
/// request per wake at most; wakes that come while it is under way count as one.
///
/// The same nudge gets room-key requests on their way: the SDK queues one for our other
/// devices when a message that came by the room list sync cannot be decrypted
/// (`OlmMachine::decrypt_room_event_inner` calls `create_outgoing_key_request`, once per
/// megolm session and only while another device of ours is verified), and it would wait
/// for the encryption sync's next request like the answers above. Those wakes are
/// rationed by [`KeyRequestWakes`].
async fn send_queued(client: Client, wake: Arc<Notify>) {
    loop {
        wake.notified().await;
        let (Some(user), Some(device)) = (client.user_id(), client.device_id()) else {
            continue;
        };
        let Ok(content) = Raw::<AnyToDeviceEventContent>::from_json_string("{}".to_owned()) else {
            continue;
        };
        let to = DeviceIdOrAllDevices::DeviceId(device.to_owned());
        let messages = BTreeMap::from([(user.to_owned(), BTreeMap::from([(to, content)]))]);
        let request =
            SendToDevice::new_raw(ToDeviceEventType::Dummy, TransactionId::new(), messages);
        // Once only, and not for long: a late nudge is worth nothing.
        let config = RequestConfig::new()
            .disable_retry()
            .timeout(Duration::from_secs(10));
        if let Err(err) = client.send(request).with_request_config(config).await {
            // The queue is sent with the encryption sync's next request all the same.
            tracing::warn!(%err, "could not hurry the answer to a verification step");
        }
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
                        fresh: is_fresh(ev.content.timestamp),
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
                        fresh: is_fresh(ev.origin_server_ts),
                    });
                }
            }
        });
        // Steps of a verification in a room: the SDK has worked them in by the time
        // handlers run, and queued its answer, which `send_queued` gets on its way.
        let wake = Arc::new(Notify::new());
        let own = client.user_id().map(ToOwned::to_owned);
        let poke = wake.clone();
        let steps = client.add_event_handler(move |ev: AnySyncMessageLikeEvent| {
            let poke = poke.clone();
            let step = in_room_step(&ev, own.as_deref());
            async move {
                if step {
                    poke.notify_one();
                }
            }
        });
        // A message that is still encrypted when handlers run could not be decrypted
        // (handlers get the decrypted event otherwise), and the SDK has queued its
        // request for the key. Only events from sync come here, never loaded history.
        let key_wakes = Arc::new(Mutex::new(KeyRequestWakes::new(Instant::now())));
        // Whether a key request can be waiting at all; kept by the status watcher.
        let askable = Arc::new(AtomicBool::new(false));
        let can_ask = askable.clone();
        let ration = key_wakes.clone();
        let poke = wake.clone();
        let undecrypted = client.add_event_handler(move |ev: OriginalSyncRoomEncryptedEvent| {
            let fresh = within(
                MilliSecondsSinceUnixEpoch::now(),
                ev.origin_server_ts,
                UTD_FRESH_MS,
            );
            let hurry = megolm_session(&ev.content).is_some_and(|session| {
                ration.lock().is_ok_and(|mut ration| {
                    ration.allow(
                        Instant::now(),
                        session,
                        fresh,
                        can_ask.load(Ordering::Relaxed),
                    )
                })
            });
            if hurry {
                poke.notify_one();
            }
            async {}
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
            askable,
        ));
        tasks.spawn(send_queued(client.clone(), wake));
        Self {
            client: client.clone(),
            events,
            incoming,
            handlers: vec![from_device, in_room, steps, secrets, undecrypted],
            next_flow: 0,
            running: None,
            tasks,
            refresh,
            key_wakes,
        }
    }

    /// The first sync of the session is through: from a little later on, new messages
    /// that cannot be decrypted get their key request sent at once.
    pub fn caught_up(&self) {
        if let Ok(mut wakes) = self.key_wakes.lock() {
            wakes.caught_up(Instant::now());
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
        self.launch(Some(request), Target::Own);
    }

    pub async fn handle(&mut self, command: VerifyCmd) {
        match command {
            VerifyCmd::Request => {
                if self.busy() {
                    let _ = self.events.send(MatrixEvent::ActionFailed(
                        "A verification is already running".to_owned(),
                    ));
                } else {
                    self.launch(None, Target::Own);
                }
            }
            VerifyCmd::RequestDevice { device_id } => {
                if self.busy() {
                    let _ = self.events.send(MatrixEvent::ActionFailed(
                        "A verification is already running".to_owned(),
                    ));
                } else {
                    self.launch(None, Target::Device(device_id.into()));
                }
            }
            VerifyCmd::RequestUser { user_id } => {
                if self.busy() {
                    let _ = self.events.send(MatrixEvent::ActionFailed(
                        "A verification is already running".to_owned(),
                    ));
                    return;
                }
                let Ok(parsed) = matrix_sdk::ruma::UserId::parse(&user_id) else {
                    let _ = self.events.send(MatrixEvent::ActionFailed(
                        "That is not a valid Matrix user ID.".to_owned(),
                    ));
                    return;
                };
                if self.client.user_id() == Some(&*parsed) {
                    let _ = self.events.send(MatrixEvent::ActionFailed(
                        "Use Settings → Encryption to verify your own device.".to_owned(),
                    ));
                    return;
                }
                self.launch(None, Target::User(parsed));
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

    /// Runs a verification: an incoming `request`, or a new request to whom `target`
    /// says (for an incoming one it says nothing). Only called when no other one runs.
    fn launch(&mut self, request: Option<VerificationRequest>, target: Target) {
        if let Some(old) = self.running.take() {
            // Over already; at most still waiting for its signature to show up.
            old.task.abort();
        }
        self.next_flow += 1;
        let (answers, answer_rx) = unbounded_channel();
        let finished = Arc::new(AtomicBool::new(false));
        let (peer, own, this_device) = match &target {
            Target::User(user_id) => (sanitize_line(user_id.as_str()), false, false),
            // Until the request is made and the device's name is known.
            Target::Device(device) => (sanitize_line(device.as_str()), true, true),
            Target::Own => ("your other devices".to_owned(), true, true),
        };
        let flow = Flow {
            id: self.next_flow,
            events: self.events.clone(),
            refresh: self.refresh.clone(),
            finished: finished.clone(),
            peer,
            target,
            own,
            this_device,
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

    fn remove_handlers(&mut self) {
        for handler in self.handlers.drain(..) {
            self.client.remove_event_handler(handler);
        }
    }

    /// Ends the session's verification work. A running verification is cancelled
    /// (briefly waiting for the other side to be told); everything else stops.
    pub async fn stop(mut self) {
        self.remove_handlers();
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

impl Drop for Verifier {
    fn drop(&mut self) {
        self.remove_handlers();
        if let Some(running) = self.running.take() {
            running.task.abort();
        }
    }
}

/// Creates the account's cross-signing identity unless it has one. Servers that follow
/// MSC3967 allow the first upload without a password; others ask for it.
pub(super) async fn set_up_cross_signing(
    client: &Client,
    password: Option<Secret>,
) -> SetUpOutcome {
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
        // The request needs a `String` of its own; ours is wiped as it is dropped.
        password.as_str().to_owned(),
    );
    drop(password);
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
    target: Target,
    own: bool,
    this_device: bool,
    /// The stage last sent to the UI.
    last: Option<VerifyStage>,
}

/// Whom a request of ours goes to.
enum Target {
    /// All of the user's other devices.
    Own,
    /// One of them.
    Device(OwnedDeviceId),
    /// Another person, in a direct chat.
    User(OwnedUserId),
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

    /// Sends our request: to the one device this verification is meant for, or to all
    /// of the user's other devices.
    async fn request(&mut self, client: &Client) -> Result<VerificationRequest, String> {
        match &self.target {
            Target::Device(device) => {
                let (request, label) = request_device(client, device).await?;
                self.peer = label;
                Ok(request)
            }
            Target::User(user_id) => request_user(client, user_id).await,
            Target::Own => request_own(client).await,
        }
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
                match self.request(&client).await {
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
        match self.request(client).await {
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

/// Sends a verification request to one of the user's devices. Returns it with a name
/// for that device.
async fn request_device(
    client: &Client,
    device_id: &DeviceId,
) -> Result<(VerificationRequest, String), String> {
    let user_id = client
        .user_id()
        .ok_or_else(|| "Not logged in".to_owned())?
        .to_owned();
    if client.device_id() == Some(device_id) {
        return Err("That is this device".to_owned());
    }
    let device = client
        .encryption()
        .get_device(&user_id, device_id)
        .await
        .map_err(|err| format!("Could not read the encryption store: {err}"))?
        .ok_or_else(|| {
            "That session has no encryption keys (or they are not known here yet), so it \
             cannot be verified"
                .to_owned()
        })?;
    let label = device_label(device.display_name(), device_id);
    let request = device
        .request_verification_with_methods(vec![VerificationMethod::SasV1])
        .await
        .map_err(|err| format!("Could not send the request: {err}"))?;
    Ok((request, label))
}

/// Sends a verification request to another Matrix user in a direct chat.
async fn request_user(client: &Client, user_id: &UserId) -> Result<VerificationRequest, String> {
    let encryption = client.encryption();
    let identity = match encryption
        .get_user_identity(user_id)
        .await
        .map_err(|err| format!("Could not read the encryption store: {err}"))?
    {
        Some(identity) => identity,
        None => tokio::time::timeout(
            Duration::from_secs(15),
            encryption.request_user_identity(user_id),
        )
        .await
        .map_err(|_| "Looking up that user's encryption keys timed out.".to_owned())?
        .map_err(|err| format!("Could not fetch that user's encryption keys: {err}"))?
        .ok_or_else(|| {
            "That user has no encryption keys yet, so there is nothing to verify.".to_owned()
        })?,
    };
    identity
        .request_verification_with_methods(vec![VerificationMethod::SasV1])
        .await
        .map_err(|err| format!("Could not send the verification request: {err}"))
}

/// Whether the SDK asks this account's other devices for message keys it lacks: it
/// does while one of them is verified from here (`should_request_key` in
/// matrix-sdk-crypto 0.19.1).
async fn can_ask_for_keys(client: &Client) -> bool {
    let (Some(user_id), Some(own)) = (client.user_id(), client.device_id()) else {
        return false;
    };
    client
        .encryption()
        .get_user_devices(user_id)
        .await
        .is_ok_and(|devices| {
            devices
                .devices()
                .any(|device| device.device_id() != own && device.is_verified())
        })
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
    askable: Arc<AtomicBool>,
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
        askable.store(can_ask_for_keys(&client).await, Ordering::Relaxed);
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
pub(super) mod tests {
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
    fn steps_other_people_take_in_a_room_hurry_the_answer() {
        use matrix_sdk::ruma::user_id;
        let me = user_id!("@me:example.org");
        let now = u64::from(MilliSecondsSinceUnixEpoch::now().get());
        let event = |kind: &str, sender: &str, sent: u64| -> AnySyncMessageLikeEvent {
            // The check looks at type, sender and time only; the SDK has judged the
            // rest by then.
            let relates = serde_json::json!({ "rel_type": "m.reference", "event_id": "$request" });
            let content = match kind {
                "m.key.verification.ready" => serde_json::json!({
                    "from_device": "DEVICE", "methods": ["m.sas.v1"], "m.relates_to": relates,
                }),
                "m.key.verification.start" => serde_json::json!({
                    "from_device": "DEVICE",
                    "method": "m.sas.v1",
                    "key_agreement_protocols": ["curve25519-hkdf-sha256"],
                    "hashes": ["sha256"],
                    "message_authentication_codes": ["hkdf-hmac-sha256.v2"],
                    "short_authentication_string": ["emoji"],
                    "m.relates_to": relates,
                }),
                "m.key.verification.accept" => serde_json::json!({
                    "method": "m.sas.v1",
                    "key_agreement_protocol": "curve25519-hkdf-sha256",
                    "hash": "sha256",
                    "message_authentication_code": "hkdf-hmac-sha256.v2",
                    "short_authentication_string": ["emoji"],
                    "commitment": "a2V5",
                    "m.relates_to": relates,
                }),
                "m.key.verification.key" => serde_json::json!({
                    "key": "a2V5", "m.relates_to": relates,
                }),
                "m.key.verification.mac" => serde_json::json!({
                    "mac": {}, "keys": "a2V5", "m.relates_to": relates,
                }),
                "m.key.verification.done" => serde_json::json!({ "m.relates_to": relates }),
                "m.key.verification.cancel" => serde_json::json!({
                    "code": "m.user", "reason": "no", "m.relates_to": relates,
                }),
                // A message of ours that this device cannot read (yet).
                "m.room.encrypted" => serde_json::json!({
                    "algorithm": "m.megolm.v1.aes-sha2",
                    "ciphertext": "a2V5",
                    "sender_key": "a2V5",
                    "device_id": "DEVICE",
                    "session_id": "session",
                }),
                // The request itself: nothing is answered before the user accepts.
                "m.room.message" => serde_json::json!({
                    "msgtype": "m.key.verification.request",
                    "body": "verify",
                    "from_device": "DEVICE",
                    "methods": ["m.sas.v1"],
                    "to": me,
                }),
                _ => panic!("no content for {kind}"),
            };
            serde_json::from_value(serde_json::json!({
                "type": kind,
                "event_id": "$step",
                "sender": sender,
                "origin_server_ts": sent,
                "content": content,
            }))
            .unwrap()
        };
        let them = "@them:example.org";
        for kind in [
            "m.key.verification.ready",
            "m.key.verification.start",
            "m.key.verification.accept",
            "m.key.verification.key",
            "m.key.verification.mac",
            "m.key.verification.done",
            "m.key.verification.cancel",
        ] {
            let live = event(kind, them, now);
            assert_eq!(live.event_type().to_string(), kind);
            assert!(in_room_step(&live, Some(me)), "{kind}");
            // Just loaded, but sent a while ago (or slightly ahead on an offset clock).
            assert!(in_room_step(
                &event(kind, them, now - FRESH_MS / 2),
                Some(me)
            ));
            assert!(in_room_step(
                &event(kind, them, now + FRESH_MS / 2),
                Some(me)
            ));
            // Our own steps (this device's, or another one's) need no answer from here.
            assert!(!in_room_step(&event(kind, me.as_str(), now), Some(me)));
            // History or far in the future.
            assert!(!in_room_step(
                &event(kind, them, now - 2 * FRESH_MS),
                Some(me)
            ));
            assert!(!in_room_step(
                &event(kind, them, now + 2 * FRESH_MS),
                Some(me)
            ));
        }
        for kind in ["m.room.message", "m.room.encrypted"] {
            let other = event(kind, them, now);
            assert_eq!(other.event_type().to_string(), kind);
            assert!(!in_room_step(&other, Some(me)), "{kind}");
        }
    }

    #[test]
    fn freshness_window_is_symmetric() {
        use matrix_sdk::ruma::UInt;

        let ts = |ms: u64| MilliSecondsSinceUnixEpoch(UInt::new_saturating(ms));
        let now = ts(1_700_000_000_000);
        let at = now.get();
        let at_ms = u64::from(at);

        assert!(is_fresh_at(now, now));
        assert!(is_fresh_at(now, ts(at_ms - (FRESH_MS - 1))));
        assert!(!is_fresh_at(now, ts(at_ms - FRESH_MS)));
        assert!(is_fresh_at(now, ts(at_ms + (FRESH_MS - 1))));
        assert!(!is_fresh_at(now, ts(at_ms + FRESH_MS)));
    }

    #[test]
    fn key_request_wakes_are_rationed() {
        let start = Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        let mut wakes = KeyRequestWakes::new(start);
        // Before the first sync is through, and for a while after: nothing. But a
        // session that was only refused for that keeps its chance.
        assert!(!wakes.allow(at(0), "backlog", true, true));
        wakes.caught_up(at(5));
        assert!(!wakes.allow(at(6), "early", true, true));
        let live = 5 + WAKE_SETTLE.as_secs();
        assert!(wakes.allow(at(live + 1), "early", true, true));
        assert!(!wakes.allow(at(live + 1), "early", true, true));
        // History (a room being loaded) never wakes, and marks its session as seen.
        assert!(!wakes.allow(at(live + 1), "history", false, true));
        assert!(!wakes.allow(at(live + 2), "history", true, true));
        // A new message of a new session: one wake, and none for the next of it.
        assert!(wakes.allow(at(live + 2), "a", true, true));
        assert!(!wakes.allow(at(live + 2), "a", true, true));
        assert!(!wakes.allow(at(live + 600), "a", true, true));
        // The ration is used up: refused, but not forgotten.
        assert!(!wakes.allow(at(live + 2), "b", true, true));
        assert!(wakes.allow(at(live + 2 + WAKE_EVERY.as_secs()), "b", true, true));
        // No other verified device, so no request can be waiting: nothing is sent,
        // and the session keeps its chance for when there is one.
        assert!(!wakes.allow(at(live + 900), "c", true, false));
        assert!(wakes.allow(at(live + 901), "c", true, true));

        // A flood of new sessions: the burst, then one per interval, whatever comes.
        let mut wakes = KeyRequestWakes::new(start);
        wakes.caught_up(start);
        let base = WAKE_SETTLE.as_secs();
        let mut allowed = 0;
        for n in 0..10_000u64 {
            // Ten new sessions a second, for 1000 seconds.
            if wakes.allow(at(base + n / 10), &format!("s{n}"), true, true) {
                allowed += 1;
            }
        }
        let most = u64::from(WAKE_BURST) + 1000 / WAKE_EVERY.as_secs();
        assert!(allowed <= most, "{allowed} wakes, at most {most} expected");
        assert!(allowed >= most - 1, "{allowed} wakes");
        // The same flood from one session that is refused again and again: no more.
        let mut wakes = KeyRequestWakes::new(start);
        wakes.caught_up(start);
        let mut allowed = 0;
        for n in 0..10_000u64 {
            let session = if n % 2 == 0 {
                "same".to_owned()
            } else {
                format!("s{n}")
            };
            if wakes.allow(at(base + n / 10), &session, true, true) {
                allowed += 1;
            }
        }
        assert!(allowed <= most, "{allowed} wakes, at most {most} expected");
        // Never more than the burst within one interval.
        let mut wakes = KeyRequestWakes::new(start);
        wakes.caught_up(start);
        let mut times = Vec::new();
        for n in 0..3_000u64 {
            let now = start + WAKE_SETTLE + Duration::from_millis(n * 100);
            if wakes.allow(now, &format!("t{n}"), true, true) {
                times.push(now);
            }
        }
        for window in times.windows(WAKE_BURST as usize + 1) {
            assert!(
                window[WAKE_BURST as usize].duration_since(window[0]) >= WAKE_EVERY,
                "{times:?}"
            );
        }
        // After a quiet time the full burst is back, and no more than that.
        let later = base + 400 + 3600;
        let again = (0..10)
            .filter(|n| wakes.allow(at(later), &format!("u{n}"), true, true))
            .count();
        assert_eq!(again, WAKE_BURST as usize);
        // The memory of sessions is bounded, whatever is seen.
        for n in 0..(2 * WAKE_SESSIONS) {
            wakes.allow(at(later), &format!("old{n}"), false, true);
        }
        assert_eq!(wakes.seen.len(), WAKE_SESSIONS);
        // The time of the first sync is set once.
        let mut wakes = KeyRequestWakes::new(start);
        wakes.caught_up(at(0));
        wakes.caught_up(at(1000));
        assert!(wakes.allow(at(WAKE_SETTLE.as_secs()), "x", true, true));
    }

    #[test]
    fn only_megolm_messages_have_a_session_to_ask_for() {
        let megolm: RoomEncryptedEventContent = serde_json::from_value(serde_json::json!({
            "algorithm": "m.megolm.v1.aes-sha2",
            "ciphertext": "a2V5",
            "sender_key": "a2V5",
            "device_id": "DEVICE",
            "session_id": "the-session",
        }))
        .unwrap();
        assert_eq!(megolm_session(&megolm), Some("the-session"));
        let olm: RoomEncryptedEventContent = serde_json::from_value(serde_json::json!({
            "algorithm": "m.olm.v1.curve25519-aes-sha2",
            "ciphertext": {},
            "sender_key": "a2V5",
        }))
        .unwrap();
        assert_eq!(megolm_session(&olm), None);
        // Sent two minutes ago or more is history.
        let now = MilliSecondsSinceUnixEpoch::now();
        let ago = |ms: u64| {
            MilliSecondsSinceUnixEpoch(matrix_sdk::ruma::UInt::new_saturating(
                u64::from(now.get()) - ms,
            ))
        };
        assert!(within(now, ago(1_000), UTD_FRESH_MS));
        assert!(!within(now, ago(UTD_FRESH_MS), UTD_FRESH_MS));
        // A clock that runs ahead is allowed for by the same amount.
        let ahead = |ms: u64| {
            MilliSecondsSinceUnixEpoch(matrix_sdk::ruma::UInt::new_saturating(
                u64::from(now.get()) + ms,
            ))
        };
        assert!(within(now, ahead(UTD_FRESH_MS - 1_000), UTD_FRESH_MS));
        assert!(!within(now, ahead(UTD_FRESH_MS), UTD_FRESH_MS));
    }

    #[test]
    fn debug_output_hides_the_password() {
        let command = MatrixCmd::Verification(VerifyCmd::SetUp {
            password: Some(String::from("hunter2").into()),
        });
        assert!(!format!("{command:?}").contains("hunter2"));
    }

    #[tokio::test]
    async fn dropping_verifier_removes_handlers_and_stops_tasks() {
        let client = Client::builder()
            .homeserver_url("http://127.0.0.1:1")
            .build()
            .await
            .expect("client");
        let (tx, mut rx) = unbounded_channel();
        let verifier = Verifier::start(&client, tx);
        let refresh = Arc::downgrade(&verifier.refresh);

        drop(verifier);

        // Handlers stored on `client` and background tasks on `verifier` hold
        // strong refs to `refresh` and clones of `tx`; removing and aborting
        // them on drop must release every reference while `client` still lives.
        tokio::time::timeout(Duration::from_secs(2), async {
            while rx.recv().await.is_some() {}
            while refresh.strong_count() > 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("verifier handlers and tasks should tear down on drop");
        drop(client);
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
    pub(in crate::matrix) async fn wait_for(
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

    pub(in crate::matrix) fn trace() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
            .with_test_writer()
            .try_init();
    }

    pub(in crate::matrix) fn homeserver() -> String {
        std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned())
    }

    /// A new account, named after `prefix` and the time. Returns name and password.
    pub(in crate::matrix) async fn new_account(prefix: &str) -> (String, String) {
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

    pub(in crate::matrix) fn worker_paths(root: &std::path::Path, device: &str) -> Paths {
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
    pub(in crate::matrix) async fn first_device(
        root: &std::path::Path,
        name: &str,
        password: &str,
    ) -> (MatrixHandle, UnboundedReceiver<MatrixEvent>) {
        let (a, mut a_events) = spawn(worker_paths(root, "a"));
        a.send(MatrixCmd::LoginPassword {
            homeserver: homeserver(),
            username: name.to_owned(),
            password: password.to_owned().into(),
        });
        // A new account has no cross-signing until asked for it.
        wait_for(
            &mut a_events,
            "no identity on A",
            |e| matches!(e, MatrixEvent::Security(s) if s.identity == Identity::Missing),
        )
        .await;
        a.send(MatrixCmd::Verification(VerifyCmd::SetUp {
            password: Some(password.to_owned().into()),
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
            password: password.into(),
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

    /// From the list of sessions: the verified device asks one particular session of the
    /// account, the two compare emoji, and that session ends up verified (and is shown
    /// so in the list). Both sessions are logged out at the end.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn a_chosen_session_is_verified() {
        use crate::matrix::{DevicesCmd, DevicesEvent, SessionTrust};
        trace();
        let mut pair = pair("ratchosen").await;
        pair.a.send(MatrixCmd::Devices(DevicesCmd::Watch(Some(1))));
        let mut other = None;
        wait_for(&mut pair.a_events, "the other session in the list", |e| {
            if let MatrixEvent::Devices(DevicesEvent::List { list: Ok(list), .. }) = e {
                other = list
                    .sessions
                    .iter()
                    .find(|s| !s.current && s.trust == SessionTrust::Unverified)
                    .map(|s| s.device_id.clone());
            }
            other.is_some()
        })
        .await;
        pair.a
            .send(MatrixCmd::Verification(VerifyCmd::RequestDevice {
                device_id: other.unwrap(),
            }));
        pair.finish().await;
        // Asked afresh (the updates so far went by during the verification).
        pair.a.send(MatrixCmd::Devices(DevicesCmd::Watch(Some(1))));
        wait_for(&mut pair.a_events, "both sessions verified", |e| {
            matches!(e, MatrixEvent::Devices(DevicesEvent::List { list: Ok(list), .. })
                if list.sessions.len() == 2
                    && list.sessions.iter().all(|s| s.trust == SessionTrust::Verified))
        })
        .await;
        for (handle, mut events) in [(pair.a, pair.a_events), (pair.b, pair.b_events)] {
            handle.send(MatrixCmd::Logout);
            wait_for(&mut events, "the logout", |e| {
                matches!(e, MatrixEvent::NeedLogin { .. })
            })
            .await;
            handle.send(MatrixCmd::Shutdown);
            while events.recv().await.is_some() {}
        }
        let _ = std::fs::remove_dir_all(pair.root);
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
            set_up_cross_signing(&a, Some(password.clone().into())).await,
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
            password: password.clone().into(),
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
            set_up_cross_signing(&client, Some(password.into())).await,
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
        let began = std::time::Instant::now();
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
        // Each step is answered at once (it takes two or three seconds here). An answer
        // that waits for the encryption sync's poll takes 30 seconds: see `send_queued`.
        tokio::time::timeout(Duration::from_secs(15), run)
            .await
            .expect("the verification took too long: are queued answers sent at once?");
        eprintln!("U done after {:?}", began.elapsed());
        tokio::time::timeout(Duration::from_secs(10), requester)
            .await
            .expect("V did not finish")
            .expect("V failed");
        eprintln!("V done after {:?}", began.elapsed());
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
