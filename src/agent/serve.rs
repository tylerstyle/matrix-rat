//! `matrix-rat serve`: the Matrix worker without the screen, behind the agent socket.
//!
//! Three parts:
//!
//! - The **hub** ([`Hub`]) is the only one that talks to the worker. It keeps what the
//!   worker tells (the joined rooms, whether the first sync is through), resolves room
//!   names, hands requests on as [`AgentCmd`]s and their answers back, and passes the
//!   news of watched rooms to the listeners. It asks the worker for exactly four
//!   things: send a text, read a room, watch a room, stop watching it.
//! - A **connection** ([`connection`]) reads request lines and writes answers; it
//!   knows the hub only through [`HubHandle`].
//! - The **accept loop** ([`accept`]) lets in this user's programs, a bounded number
//!   at a time.
//!
//! Everything waits with a limit and keeps a bounded amount: a line, a queue of
//! requests, the messages held for a listener (one that does not read fast enough is
//! dropped, not buffered for).

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixListener;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;

use super::proto::{self, Code, LineReader, Msg, Read, Refusal, Request, RoomInfo};
use super::socket::{self, BindError, SocketFile};
use crate::logsafe;
use crate::matrix::{
    AgentCmd, AgentEvent, MatrixCmd, MatrixEvent, MatrixHandle, RoomCategory, RoomSummary,
    SecretStoreMode,
};
use crate::paths::Paths;

/// Requests held until the account's rooms are known.
const MAX_WAITING: usize = 64;
/// Sends and reads the worker is busy with at once.
const MAX_PENDING: usize = 64;
/// Rooms watched at once (the worker's own limit).
const MAX_WATCHED: usize = crate::matrix::AGENT_MAX_WATCHED;
/// Messages of a watched room that are remembered as told.
const SEEN_MAX: usize = 1000;
/// How often the hub looks for what has waited too long or has gone away.
const TIDY: Duration = Duration::from_secs(1);

/// The waits and sizes of a running `serve`. Tests make them small.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// How long a connection may say nothing between two requests.
    pub idle: Duration,
    /// How long a request line may take from its first byte to its end.
    pub line: Duration,
    /// How long one line of an answer may wait for the client to read.
    pub write: Duration,
    /// How long a request waits for the Matrix side to answer.
    pub answer: Duration,
    /// How long a request waits for the account's rooms to be known.
    pub ready: Duration,
    /// How long a `listen` waits for the first sync to be through.
    pub sync: Duration,
    /// How long the worker gets to stop.
    pub stop: Duration,
    /// Messages held for a listener that is not reading; one more and it is dropped.
    pub backlog: usize,
    /// Connections served at once.
    pub connections: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(120),
            line: Duration::from_secs(10),
            write: Duration::from_secs(10),
            answer: Duration::from_secs(120),
            ready: Duration::from_secs(20),
            sync: Duration::from_secs(30),
            stop: Duration::from_secs(5),
            backlog: 64,
            connections: 32,
        }
    }
}

type Reply<T> = oneshot::Sender<Result<T, Refusal>>;
/// Where the answer to a read goes: the room's id, and its messages.
type ReadReply = Reply<(String, Vec<Msg>)>;

/// What a connection asks of the hub.
pub enum HubRequest {
    Rooms {
        reply: Reply<Vec<RoomInfo>>,
    },
    Send {
        room: String,
        text: String,
        reply: Reply<String>,
    },
    Read {
        room: String,
        limit: usize,
        reply: ReadReply,
    },
    Listen {
        room: String,
        reply: Reply<Listening>,
    },
    /// Whether the account has something unread in a room, as the worker told last.
    #[cfg(test)]
    Unread {
        room_id: String,
        reply: oneshot::Sender<Option<bool>>,
    },
}

impl HubRequest {
    fn refuse(self, refusal: Refusal) {
        match self {
            Self::Rooms { reply } => {
                let _ = reply.send(Err(refusal));
            }
            Self::Send { reply, .. } => {
                let _ = reply.send(Err(refusal));
            }
            Self::Read { reply, .. } => {
                let _ = reply.send(Err(refusal));
            }
            Self::Listen { reply, .. } => {
                let _ = reply.send(Err(refusal));
            }
            #[cfg(test)]
            Self::Unread { reply, .. } => {
                let _ = reply.send(None);
            }
        }
    }
}

/// A room being listened to: its id, and what happens in it from now on.
pub struct Listening {
    pub room_id: String,
    pub news: mpsc::Receiver<News>,
}

/// One thing a listener is told. The channel closing without an [`News::End`] means
/// the listener did not keep up and was dropped.
pub enum News {
    Message(Box<Msg>),
    End(Refusal),
}

/// Why `serve` cannot go on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// There is no session, or it needs a new login.
    NeedLogin,
    /// The worker has stopped.
    WorkerGone,
}

impl End {
    /// What the user is told; `profile` is the user's own word for the account.
    pub fn message(self, profile: &str) -> String {
        match self {
            Self::NeedLogin => format!(
                "this profile has no session that can be used. Log in once with \
                 `matrix-rat --profile {profile}` (the app shows what is wrong, if \
                 something is), then start `matrix-rat --profile {profile} serve` again"
            ),
            Self::WorkerGone => "the Matrix side has stopped".to_owned(),
        }
    }
}

/// A joined room as the hub knows it.
struct Known {
    info: RoomInfo,
    /// Its main address (`#name:server`), if it has one.
    alias: Option<String>,
    /// Something in it is unread for the account, by its own count or the server's.
    #[cfg_attr(not(test), allow(dead_code))]
    unread: bool,
}

/// The messages of a watched room that were told (or were there when the watch
/// began), with a mark of what each said.
#[derive(Default)]
struct Seen {
    marks: HashMap<String, u64>,
    order: VecDeque<String>,
}

impl Seen {
    /// Whether `message` is new, or says something else than when it was noted last
    /// (edited, deleted, decrypted at last).
    fn note(&mut self, message: &Msg) -> bool {
        let mark = mark(message);
        match self.marks.get_mut(&message.event_id) {
            Some(known) if *known == mark => false,
            Some(known) => {
                *known = mark;
                true
            }
            None => {
                self.marks.insert(message.event_id.clone(), mark);
                self.order.push_back(message.event_id.clone());
                while self.order.len() > SEEN_MAX {
                    if let Some(oldest) = self.order.pop_front() {
                        self.marks.remove(&oldest);
                    }
                }
                true
            }
        }
    }
}

/// What a message says, as one number.
fn mark(message: &Msg) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (
        &message.kind,
        &message.body,
        message.edited,
        message.truncated,
        &message.warning,
    )
        .hash(&mut hasher);
    hasher.finish()
}

/// A room the worker was asked to watch.
#[derive(Default)]
struct Watch {
    seen: Seen,
    /// The worker has told the room's messages at least once.
    told: bool,
    /// What was there before is noted, so what comes now is news. Not before the
    /// first sync of the session is through: what that brings is what was missed
    /// while `serve` was not running, not what is said now.
    baseline: bool,
    /// Listeners waiting for the baseline, since when.
    pending: Vec<(Instant, Reply<Listening>)>,
    listeners: Vec<mpsc::Sender<News>>,
}

impl Watch {
    fn attach(&mut self, room_id: &str, reply: Reply<Listening>, backlog: usize) {
        let (listener, news) = mpsc::channel(backlog.max(1));
        let listening = Listening {
            room_id: room_id.to_owned(),
            news,
        };
        // Gone meanwhile: nobody to listen.
        if reply.send(Ok(listening)).is_ok() {
            self.listeners.push(listener);
        }
    }

    /// From now on what the room's messages show is news, and those who waited for
    /// that are listening.
    fn begin(&mut self, room_id: &str, backlog: usize) {
        self.baseline = true;
        for (_, reply) in std::mem::take(&mut self.pending) {
            self.attach(room_id, reply, backlog);
        }
    }

    /// Hands `message` to every listener that has room for it. One that has none has
    /// not been reading for [`Limits::backlog`] messages and is dropped: its
    /// connection sees the channel close.
    fn tell(&mut self, message: &Msg) {
        self.listeners.retain(|listener| {
            match listener.try_send(News::Message(Box::new(message.clone()))) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!("agent: a listener that did not keep up was dropped");
                    false
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            }
        });
    }

    fn end(self, refusal: &Refusal) {
        for (_, reply) in self.pending {
            let _ = reply.send(Err(refusal.clone()));
        }
        for listener in self.listeners {
            let _ = listener.try_send(News::End(refusal.clone()));
        }
    }

    fn idle(&self) -> bool {
        self.pending.is_empty() && self.listeners.is_empty()
    }
}

/// The one place that talks to the worker.
pub struct Hub {
    matrix: MatrixHandle,
    events: UnboundedReceiver<MatrixEvent>,
    requests: mpsc::Receiver<HubRequest>,
    limits: Limits,
    logged_in: bool,
    /// The first sync of the session is through.
    caught_up: bool,
    /// The joined rooms; `None` until the worker has told them once.
    rooms: Option<Vec<Known>>,
    /// Requests that came before the rooms were known, since when.
    waiting: Vec<(Instant, HubRequest)>,
    next_request: u64,
    sends: HashMap<u64, Reply<String>>,
    /// By request: the room's id, and where the answer goes.
    reads: HashMap<u64, (String, ReadReply)>,
    /// By room id.
    watched: HashMap<String, Watch>,
}

/// Hand-written: rooms, their names and messages stay out of logs; counts say enough.
impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub")
            .field("logged_in", &self.logged_in)
            .field("caught_up", &self.caught_up)
            .field("rooms", &self.rooms.as_ref().map(Vec::len))
            .field("waiting", &self.waiting.len())
            .field("sends", &self.sends.len())
            .field("reads", &self.reads.len())
            .field("watched", &self.watched.len())
            .field(
                "listeners",
                &self
                    .watched
                    .values()
                    .map(|watch| watch.listeners.len())
                    .sum::<usize>(),
            )
            .finish()
    }
}

impl Hub {
    pub fn new(
        matrix: MatrixHandle,
        events: UnboundedReceiver<MatrixEvent>,
        limits: Limits,
    ) -> (Self, HubHandle) {
        let (sender, requests) = mpsc::channel(MAX_WAITING);
        let hub = Self {
            matrix,
            events,
            requests,
            limits,
            logged_in: false,
            caught_up: false,
            rooms: None,
            waiting: Vec::new(),
            next_request: 0,
            sends: HashMap::new(),
            reads: HashMap::new(),
            watched: HashMap::new(),
        };
        (
            hub,
            HubHandle {
                requests: sender,
                limits,
            },
        )
    }

    /// Waits for the worker to say that the saved session is in use. It is never
    /// given a login to try: without a session that works, this is the end.
    pub async fn logged_in(&mut self) -> Result<(), End> {
        loop {
            match self.events.recv().await {
                None => return Err(End::WorkerGone),
                Some(event) => {
                    if let Some(end) = self.on_event(event) {
                        return Err(end);
                    }
                    if self.logged_in {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Serves until the session is over or the worker is gone.
    pub async fn run(&mut self) -> End {
        let mut tidy = tokio::time::interval(TIDY);
        tidy.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                event = self.events.recv() => match event {
                    None => return End::WorkerGone,
                    Some(event) => {
                        if let Some(end) = self.on_event(event) {
                            return end;
                        }
                    }
                },
                Some(request) = self.requests.recv() => self.on_request(request),
                _ = tidy.tick() => self.tidy(Instant::now()),
            }
        }
    }

    fn on_event(&mut self, event: MatrixEvent) -> Option<End> {
        match event {
            MatrixEvent::NeedLogin { .. } | MatrixEvent::NeedReauth { .. } => {
                self.session_over(&Refusal::new(
                    Code::Stopping,
                    "the account's session has ended",
                ));
                return Some(End::NeedLogin);
            }
            MatrixEvent::LoggedIn { .. } => {
                if self.logged_in {
                    self.session_restarted();
                }
                self.logged_in = true;
            }
            MatrixEvent::CaughtUp => {
                self.caught_up = true;
                let backlog = self.limits.backlog;
                for (room_id, watch) in &mut self.watched {
                    if watch.told && !watch.baseline {
                        watch.begin(room_id, backlog);
                    }
                }
            }
            MatrixEvent::Rooms(rooms) => self.on_rooms(rooms),
            MatrixEvent::Agent(event) => self.on_agent(event),
            // Everything else is for a screen: there is none.
            _ => {}
        }
        None
    }

    /// The worker began a new session by itself (after an error): what it was asked
    /// is lost with the old one, and what was watched is asked for again.
    fn session_restarted(&mut self) {
        tracing::info!("agent: the session was started again");
        self.caught_up = false;
        let lost = Refusal::new(
            Code::Failed,
            "the connection to Matrix was started again; for a send, the message \
             may or may not have been sent",
        );
        for (_, reply) in self.sends.drain() {
            let _ = reply.send(Err(lost.clone()));
        }
        for (_, (_, reply)) in self.reads.drain() {
            let _ = reply.send(Err(lost.clone()));
        }
        for room_id in self.watched.keys() {
            self.matrix.send(MatrixCmd::Agent(AgentCmd::Watch {
                room_id: room_id.clone(),
            }));
        }
    }

    fn session_over(&mut self, refusal: &Refusal) {
        for (_, request) in self.waiting.drain(..) {
            request.refuse(refusal.clone());
        }
        for (_, reply) in self.sends.drain() {
            let _ = reply.send(Err(refusal.clone()));
        }
        for (_, (_, reply)) in self.reads.drain() {
            let _ = reply.send(Err(refusal.clone()));
        }
        for (_, watch) in self.watched.drain() {
            watch.end(refusal);
        }
    }

    fn on_rooms(&mut self, rooms: Vec<RoomSummary>) {
        let joined: Vec<Known> = rooms
            .into_iter()
            // An invitation is not a room of ours, and nothing here accepts one.
            .filter(|room| !matches!(room.category, RoomCategory::Invite | RoomCategory::Knock))
            .map(|room| Known {
                info: RoomInfo {
                    id: proto::clean_line(&room.id, proto::MAX_NAME),
                    name: proto::clean_line(&room.name, proto::MAX_NAME),
                    encrypted: room.encrypted,
                    direct: room.direct,
                },
                alias: room.alias,
                unread: room.unread || room.server_notifications > 0,
            })
            .collect();
        self.rooms = Some(joined);
        // A watched room the account is no longer in has nothing more to say. (Not
        // judged before the first sync: the list may still be filling.)
        if self.caught_up {
            let gone: Vec<String> = self
                .watched
                .keys()
                .filter(|room_id| !self.is_joined(room_id))
                .cloned()
                .collect();
            for room_id in gone {
                if let Some(watch) = self.watched.remove(&room_id) {
                    watch.end(&Refusal::new(
                        Code::NotFound,
                        "the account is no longer in this room",
                    ));
                }
                self.matrix
                    .send(MatrixCmd::Agent(AgentCmd::Unwatch { room_id }));
            }
        }
        for (_, request) in std::mem::take(&mut self.waiting) {
            self.answer(request);
        }
    }

    fn is_joined(&self, room_id: &str) -> bool {
        self.rooms
            .iter()
            .flatten()
            .any(|known| known.info.id == room_id)
    }

    fn on_agent(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Sent { request, result } => {
                if let Some(reply) = self.sends.remove(&request) {
                    let _ = reply.send(
                        result
                            .map(|event_id| proto::clean_line(&event_id, proto::MAX_NAME))
                            .map_err(|reason| Refusal::failed(&reason)),
                    );
                }
            }
            AgentEvent::Read { request, result } => {
                if let Some((room_id, reply)) = self.reads.remove(&request) {
                    let _ = reply.send(
                        result
                            .map(|messages| (room_id, messages))
                            .map_err(|reason| Refusal::failed(&reason)),
                    );
                }
            }
            AgentEvent::Watched { room_id, messages } => {
                let (caught_up, backlog) = (self.caught_up, self.limits.backlog);
                // Not watched any more: an answer that was on its way.
                let Some(watch) = self.watched.get_mut(&room_id) else {
                    return;
                };
                watch.told = true;
                let news: Vec<Msg> = messages
                    .into_iter()
                    .filter(|message| watch.seen.note(message))
                    .collect();
                if !watch.baseline {
                    // What was there before anyone listened is not news.
                    if caught_up {
                        watch.begin(&room_id, backlog);
                    }
                    return;
                }
                for message in &news {
                    watch.tell(message);
                }
            }
            AgentEvent::WatchEnded { room_id, reason } => {
                if let Some(watch) = self.watched.remove(&room_id) {
                    watch.end(&Refusal::failed(&reason));
                }
            }
        }
    }

    fn on_request(&mut self, request: HubRequest) {
        if self.rooms.is_some() {
            return self.answer(request);
        }
        // Right after the start: the answer waits for the rooms, for a while.
        if self.waiting.len() >= MAX_WAITING {
            request.refuse(busy());
        } else {
            self.waiting.push((Instant::now(), request));
        }
    }

    fn answer(&mut self, request: HubRequest) {
        match request {
            HubRequest::Rooms { reply } => {
                let rooms = self
                    .rooms
                    .iter()
                    .flatten()
                    .take(proto::MAX_ROOMS)
                    .map(|known| known.info.clone())
                    .collect();
                let _ = reply.send(Ok(rooms));
            }
            HubRequest::Send { room, text, reply } => {
                let checked = proto::check_text(&text)
                    .and_then(|()| self.resolve(&room))
                    .and_then(|room_id| Ok((room_id, self.slot()?)));
                match checked {
                    Ok((room_id, request)) => {
                        // The app's own Markdown, without its slash commands: a text
                        // that starts with a slash is a text.
                        let message = crate::format::message(&proto::clean_outgoing(&text));
                        self.matrix.send(MatrixCmd::Agent(AgentCmd::Send {
                            request,
                            room_id,
                            message,
                        }));
                        self.sends.insert(request, reply);
                    }
                    Err(refusal) => {
                        let _ = reply.send(Err(refusal));
                    }
                }
            }
            HubRequest::Read { room, limit, reply } => {
                let checked = self
                    .resolve(&room)
                    .and_then(|room_id| Ok((room_id, self.slot()?)));
                match checked {
                    Ok((room_id, request)) => {
                        self.matrix.send(MatrixCmd::Agent(AgentCmd::Read {
                            request,
                            room_id: room_id.clone(),
                            limit: limit.clamp(1, proto::READ_MAX),
                        }));
                        self.reads.insert(request, (room_id, reply));
                    }
                    Err(refusal) => {
                        let _ = reply.send(Err(refusal));
                    }
                }
            }
            HubRequest::Listen { room, reply } => self.listen(&room, reply),
            #[cfg(test)]
            HubRequest::Unread { room_id, reply } => {
                let unread = self
                    .rooms
                    .iter()
                    .flatten()
                    .find(|known| known.info.id == room_id)
                    .map(|known| known.unread);
                let _ = reply.send(unread);
            }
        }
    }

    fn listen(&mut self, room: &str, reply: Reply<Listening>) {
        let room_id = match self.resolve(room) {
            Ok(room_id) => room_id,
            Err(refusal) => {
                let _ = reply.send(Err(refusal));
                return;
            }
        };
        if !self.watched.contains_key(&room_id) {
            if self.watched.len() >= MAX_WATCHED {
                let _ = reply.send(Err(busy()));
                return;
            }
            self.matrix.send(MatrixCmd::Agent(AgentCmd::Watch {
                room_id: room_id.clone(),
            }));
        }
        let backlog = self.limits.backlog;
        let watch = self.watched.entry(room_id.clone()).or_default();
        if watch.baseline {
            watch.attach(&room_id, reply, backlog);
        } else {
            watch.pending.push((Instant::now(), reply));
        }
    }

    /// A number for a request to the worker, unless it is busy with too many.
    fn slot(&mut self) -> Result<u64, Refusal> {
        if self.sends.len() + self.reads.len() >= MAX_PENDING {
            return Err(busy());
        }
        self.next_request += 1;
        Ok(self.next_request)
    }

    /// The id of the joined room that `room` names: an id, or the exact name, or the
    /// main address. A name that several rooms have is refused, and the refusal says
    /// nothing about them.
    fn resolve(&self, room: &str) -> Result<String, Refusal> {
        let wanted = room.trim();
        let not_found = || {
            Refusal::new(
                Code::NotFound,
                "no joined room has that id, name or address",
            )
        };
        let rooms = || self.rooms.iter().flatten();
        if wanted.starts_with('!') {
            return rooms()
                .find(|known| known.info.id == wanted)
                .map(|known| known.info.id.clone())
                .ok_or_else(not_found);
        }
        let mut named = rooms().filter(|known| known.info.name == wanted);
        match (named.next(), named.next()) {
            (Some(known), None) => return Ok(known.info.id.clone()),
            (Some(_), Some(_)) => {
                return Err(Refusal::new(
                    Code::Ambiguous,
                    "ambiguous: several joined rooms have that name; use the room's id",
                ));
            }
            _ => {}
        }
        if wanted.starts_with('#') {
            let mut addressed = rooms().filter(|known| known.alias.as_deref() == Some(wanted));
            if let (Some(known), None) = (addressed.next(), addressed.next()) {
                return Ok(known.info.id.clone());
            }
        }
        Err(not_found())
    }

    /// Refuses what has waited too long, forgets who has gone away, and stops
    /// watching rooms nobody listens to.
    fn tidy(&mut self, now: Instant) {
        let ready = self.limits.ready;
        let late: Vec<(Instant, HubRequest)> = self
            .waiting
            .extract_if(.., |(since, _)| now.duration_since(*since) >= ready)
            .collect();
        for (_, request) in late {
            request.refuse(Refusal::new(
                Code::NotReady,
                "the account's rooms have not arrived yet; try again in a moment",
            ));
        }
        self.sends.retain(|_, reply| !reply.is_closed());
        self.reads.retain(|_, (_, reply)| !reply.is_closed());
        let sync = self.limits.sync;
        let mut idle = Vec::new();
        for (room_id, watch) in &mut self.watched {
            let late: Vec<(Instant, Reply<Listening>)> = watch
                .pending
                .extract_if(.., |(since, reply)| {
                    reply.is_closed() || now.duration_since(*since) >= sync
                })
                .collect();
            for (_, reply) in late {
                let _ = reply.send(Err(Refusal::new(
                    Code::NotReady,
                    "the first sync with the server is not through yet; try again in a \
                     moment",
                )));
            }
            watch.listeners.retain(|listener| !listener.is_closed());
            if watch.idle() {
                idle.push(room_id.clone());
            }
        }
        for room_id in idle {
            self.watched.remove(&room_id);
            self.matrix
                .send(MatrixCmd::Agent(AgentCmd::Unwatch { room_id }));
        }
    }
}

fn busy() -> Refusal {
    Refusal::new(
        Code::Busy,
        "too much is going on at once; try again in a moment",
    )
}

/// A connection's way to the hub.
#[derive(Clone)]
pub struct HubHandle {
    requests: mpsc::Sender<HubRequest>,
    limits: Limits,
}

impl HubHandle {
    /// Asks the hub and waits for its answer, for `limit` at most.
    async fn ask<T>(
        &self,
        limit: Duration,
        request: impl FnOnce(Reply<T>) -> HubRequest,
    ) -> Result<T, Refusal> {
        let (reply, answer) = oneshot::channel();
        let asked = async {
            self.requests
                .send(request(reply))
                .await
                .map_err(|_| Refusal::stopping())?;
            answer.await.map_err(|_| Refusal::stopping())?
        };
        match tokio::time::timeout(limit, asked).await {
            Ok(answer) => answer,
            Err(_) => Err(Refusal::new(Code::Timeout, "no answer in time")),
        }
    }

    pub async fn rooms(&self) -> Result<Vec<RoomInfo>, Refusal> {
        let limit = self.limits.ready + self.limits.write;
        self.ask(limit, |reply| HubRequest::Rooms { reply }).await
    }

    pub async fn send(&self, room: String, text: String) -> Result<String, Refusal> {
        let limit = self.limits.ready + self.limits.answer;
        self.ask(limit, |reply| HubRequest::Send { room, text, reply })
            .await
    }

    pub async fn read(&self, room: String, limit: usize) -> Result<(String, Vec<Msg>), Refusal> {
        let wait = self.limits.ready + self.limits.answer;
        self.ask(wait, |reply| HubRequest::Read { room, limit, reply })
            .await
    }

    pub async fn listen(&self, room: String) -> Result<Listening, Refusal> {
        let limit = self.limits.ready + self.limits.sync + self.limits.write;
        self.ask(limit, |reply| HubRequest::Listen { room, reply })
            .await
    }

    #[cfg(test)]
    pub async fn unread(&self, room_id: &str) -> Option<bool> {
        let (reply, answer) = oneshot::channel();
        let request = HubRequest::Unread {
            room_id: room_id.to_owned(),
            reply,
        };
        self.requests.send(request).await.ok()?;
        answer.await.ok()?
    }
}

// ----- a connection ---------------------------------------------------------------------

/// Serves one connection: request lines in, answers out, until the client goes away,
/// says nothing for too long, or sends what cannot be read.
pub async fn connection<R, W>(reader: R, mut writer: W, hub: HubHandle)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let limits = hub.limits;
    let mut lines = LineReader::new(reader, proto::MAX_REQUEST_LINE);
    loop {
        let line = match lines.next(Some(limits.idle), Some(limits.line)).await {
            Ok(Read::Line(line)) => line,
            Ok(Read::Invalid) => {
                let refusal = Refusal::bad("the line is not UTF-8");
                if say(&mut writer, &refusal.line(), &limits).await {
                    continue;
                }
                return;
            }
            Ok(Read::TooLong) => {
                // Where the next line starts is unknown: said, and closed.
                let refusal = Refusal::new(
                    Code::TooLong,
                    format!(
                        "a request line may be {} bytes at most",
                        proto::MAX_REQUEST_LINE
                    ),
                );
                say(&mut writer, &refusal.line(), &limits).await;
                return;
            }
            Ok(Read::Stalled) => {
                let refusal = Refusal::new(Code::Timeout, "the line was not finished in time");
                say(&mut writer, &refusal.line(), &limits).await;
                return;
            }
            Ok(Read::Eof | Read::Idle) | Err(_) => return,
        };
        if line.trim().is_empty() {
            continue;
        }
        let going = match Request::parse(&line) {
            Err(refusal) => say(&mut writer, &refusal.line(), &limits).await,
            Ok(Request::Rooms) => match hub.rooms().await {
                Ok(rooms) => {
                    let mut going =
                        say(&mut writer, &proto::count_line(rooms.len()), &limits).await;
                    for room in &rooms {
                        going = going && say(&mut writer, &proto::room_line(room), &limits).await;
                    }
                    going
                }
                Err(refusal) => say(&mut writer, &refusal.line(), &limits).await,
            },
            Ok(Request::Send { room, text }) => {
                let line = match hub.send(room, text).await {
                    Ok(event_id) => proto::sent_line(&event_id),
                    Err(refusal) => refusal.line(),
                };
                say(&mut writer, &line, &limits).await
            }
            Ok(Request::Read { room, limit }) => match hub.read(room, limit).await {
                Ok((room_id, messages)) => {
                    let header = proto::read_line(&room_id, messages.len());
                    let mut going = say(&mut writer, &header, &limits).await;
                    for message in &messages {
                        going =
                            going && say(&mut writer, &proto::message_line(message), &limits).await;
                    }
                    going
                }
                Err(refusal) => say(&mut writer, &refusal.line(), &limits).await,
            },
            Ok(Request::Listen { room }) => match hub.listen(room).await {
                Ok(listening) => {
                    stream(&mut lines, &mut writer, listening, &limits).await;
                    // A stream is the last thing a connection does.
                    false
                }
                Err(refusal) => say(&mut writer, &refusal.line(), &limits).await,
            },
        };
        if !going {
            return;
        }
    }
}

/// Writes one line; `false` if the client is gone or does not read.
async fn say<W: AsyncWrite + Unpin>(writer: &mut W, line: &str, limits: &Limits) -> bool {
    proto::write_line(writer, line, limits.write).await.is_ok()
}

/// Passes a room's news to a listener until it goes away (or says anything: while
/// listening there is nothing to ask), does not read, or the stream has to end.
async fn stream<R, W>(
    lines: &mut LineReader<R>,
    writer: &mut W,
    mut listening: Listening,
    limits: &Limits,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if !say(writer, &proto::listening_line(&listening.room_id), limits).await {
        return;
    }
    loop {
        tokio::select! {
            news = listening.news.recv() => match news {
                Some(News::Message(message)) => {
                    if !say(writer, &proto::message_line(&message), limits).await {
                        return;
                    }
                }
                Some(News::End(refusal)) => {
                    say(writer, &refusal.line(), limits).await;
                    return;
                }
                None => {
                    let refusal = Refusal::new(
                        Code::TooSlow,
                        "dropped: messages came faster than they were read",
                    );
                    say(writer, &refusal.line(), limits).await;
                    return;
                }
            },
            _ = lines.next(None, None) => return,
        }
    }
}

// ----- the socket -----------------------------------------------------------------------

/// Lets in this user's programs, [`Limits::connections`] at a time; one more is told
/// that it is too many, anyone else is dropped without a word.
pub async fn accept(listener: UnixListener, hub: HubHandle) {
    let slots = Arc::new(Semaphore::new(hub.limits.connections));
    let mut connections = JoinSet::new();
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(err) => {
                tracing::warn!(why = %logsafe::Why::Io(err.kind()), "agent socket: could not accept");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        while connections.try_join_next().is_some() {}
        if !socket::peer_is_us(&stream) {
            tracing::warn!("agent socket: a connection of another user was dropped");
            continue;
        }
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            // Said in one write, without waiting for it to be read.
            let line = format!("{}\n", busy().line());
            let refuse = async {
                if stream.writable().await.is_ok() {
                    let _ = stream.try_write(line.as_bytes());
                }
            };
            let _ = tokio::time::timeout(Duration::from_millis(200), refuse).await;
            continue;
        };
        let hub = hub.clone();
        connections.spawn(async move {
            let (reader, writer) = stream.into_split();
            connection(reader, writer, hub).await;
            drop(slot);
        });
    }
}

/// Why `serve` did not start.
#[derive(Debug)]
pub enum StartError {
    Session(End),
    Socket(BindError),
}

/// A running `serve`: the hub, the socket and its connections.
pub struct Served {
    matrix: MatrixHandle,
    #[cfg_attr(not(test), allow(dead_code))]
    handle: HubHandle,
    limits: Limits,
    socket: SocketFile,
    /// The hub at work; `None` once it has ended.
    hub: Option<JoinHandle<End>>,
    /// Why it ended.
    end: Option<End>,
    accept: JoinHandle<()>,
}

impl Served {
    /// Starts the worker on the profile's saved session and serves it; for tests
    /// against a real homeserver.
    #[cfg(test)]
    pub async fn start(
        paths: &Paths,
        secret_store: SecretStoreMode,
        limits: Limits,
    ) -> Result<Self, StartError> {
        let (matrix, events) = crate::matrix::spawn_with(paths.clone(), secret_store);
        Self::begin(matrix, events, &paths.agent_socket(), limits).await
    }

    /// Waits for the worker to be logged in, then opens the socket at `socket` and
    /// serves. On failure the worker has been told to stop.
    pub async fn begin(
        matrix: MatrixHandle,
        events: UnboundedReceiver<MatrixEvent>,
        socket: &Path,
        limits: Limits,
    ) -> Result<Self, StartError> {
        let (mut hub, handle) = Hub::new(matrix.clone(), events, limits);
        if let Err(end) = hub.logged_in().await {
            stop_worker(&matrix, limits.stop).await;
            return Err(StartError::Session(end));
        }
        let (listener, socket) = match socket::bind(socket) {
            Ok(bound) => bound,
            Err(err) => {
                stop_worker(&matrix, limits.stop).await;
                return Err(StartError::Socket(err));
            }
        };
        Ok(Self {
            matrix,
            limits,
            socket,
            accept: tokio::spawn(accept(listener, handle.clone())),
            hub: Some(tokio::spawn(async move { hub.run().await })),
            end: None,
            handle,
        })
    }

    /// Waits until `serve` cannot go on. May be dropped while waiting and called
    /// again; once it has ended, it says so every time.
    pub async fn ended(&mut self) -> End {
        if let Some(hub) = &mut self.hub {
            let end = hub.await.unwrap_or(End::WorkerGone);
            self.hub = None;
            self.end = Some(end);
        }
        self.end.unwrap_or(End::WorkerGone)
    }

    /// Closes the socket and its connections, removes the socket's file, and has the
    /// worker stop the way the app does on quitting.
    pub async fn stop(self) {
        self.accept.abort();
        let _ = self.accept.await;
        drop(self.socket);
        if let Some(hub) = self.hub {
            hub.abort();
            let _ = hub.await;
        }
        stop_worker(&self.matrix, self.limits.stop).await;
    }

    #[cfg(test)]
    pub async fn unread(&self, room_id: &str) -> bool {
        self.handle.unread(room_id).await == Some(true)
    }
}

/// Tells the worker to stop and gives it a moment to do so (it stops syncing and
/// saves the session), but never hangs on the way out.
async fn stop_worker(matrix: &MatrixHandle, limit: Duration) {
    matrix.send(MatrixCmd::Shutdown);
    let _ = tokio::time::timeout(limit, matrix.stopped()).await;
}

/// The signals that end `serve`, and the one that must not.
struct Signals {
    terminate: Signal,
    interrupt: Signal,
    hangup: Signal,
    /// `matrix-rat --reload-theme` sends this to every running instance of a
    /// profile, and unhandled it would end the program. There is no theme here.
    reload: Signal,
}

impl Signals {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
            hangup: signal(SignalKind::hangup())?,
            reload: signal(SignalKind::user_defined1())?,
        })
    }

    /// Waits for a signal to stop.
    async fn stop(&mut self) {
        loop {
            tokio::select! {
                _ = self.terminate.recv() => return,
                _ = self.interrupt.recv() => return,
                _ = self.hangup.recv() => return,
                _ = self.reload.recv() => {}
            }
        }
    }
}

/// `matrix-rat serve`, from the start of the worker to its end. The profile's lock is
/// held and logging is running. Says one line on stderr when the socket is open.
pub async fn run(paths: &Paths, secret_store: SecretStoreMode) -> anyhow::Result<()> {
    let mut signals = Signals::new()?;
    let limits = Limits::default();
    let (matrix, events) = crate::matrix::spawn_with(paths.clone(), secret_store);
    let socket = paths.agent_socket();
    let starting = Served::begin(matrix.clone(), events, &socket, limits);
    let started = tokio::select! {
        started = starting => started,
        () = signals.stop() => {
            stop_worker(&matrix, limits.stop).await;
            return Ok(());
        }
    };
    let mut served = match started {
        Ok(served) => served,
        Err(StartError::Session(end)) => {
            tracing::error!(why = ?end, "serve: not started");
            anyhow::bail!("{}", end.message(&paths.profile));
        }
        Err(StartError::Socket(err)) => {
            tracing::error!("serve: the socket could not be opened");
            return Err(anyhow::Error::new(err).context(format!(
                "could not open the agent socket {}",
                socket.display()
            )));
        }
    };
    tracing::info!("serving");
    eprintln!("matrix-rat serve: ready");
    let end = tokio::select! {
        end = served.ended() => Some(end),
        () = signals.stop() => None,
    };
    served.stop().await;
    tracing::info!(why = ?end, "serve: stopped");
    match end {
        Some(end) => anyhow::bail!("{}", end.message(&paths.profile)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests;
