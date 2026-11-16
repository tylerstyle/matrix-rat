//! For the tests of the agent channel: a folder of their own, and a `serve` whose
//! worker is the test itself (a pair of channels in place of the SDK).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::proto::Msg;
use super::serve::{Limits, Served};
use crate::matrix::{
    AgentCmd, AgentEvent, MatrixCmd, MatrixEvent, MatrixHandle, RoomCategory, RoomSummary,
};

/// A fresh folder for one test, removed at its end. Its path is short: a socket's
/// address has to fit into about a hundred bytes.
pub struct Dir(PathBuf);

impl Dir {
    pub fn new(name: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "mr-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Waits that let a test fail fast.
pub fn quick() -> Limits {
    Limits {
        idle: Duration::from_secs(20),
        line: Duration::from_secs(5),
        write: Duration::from_secs(5),
        answer: Duration::from_secs(10),
        ready: Duration::from_secs(10),
        sync: Duration::from_secs(10),
        stop: Duration::from_millis(50),
        backlog: 64,
        connections: 32,
    }
}

/// A joined room as the worker tells it.
pub fn room(id: &str, name: &str) -> RoomSummary {
    RoomSummary {
        id: id.to_owned(),
        name: name.to_owned(),
        category: RoomCategory::Room,
        notifications: 0,
        preview: None,
        server_notifications: 0,
        mention: false,
        unread: false,
        marked_unread: false,
        avatar: None,
        alias: None,
        direct: false,
        muted: false,
        encrypted: true,
    }
}

/// A text message of someone else.
pub fn message(event_id: &str, sender: &str, body: &str) -> Msg {
    Msg {
        event_id: event_id.to_owned(),
        sender: sender.to_owned(),
        sender_name: sender
            .trim_start_matches('@')
            .split(':')
            .next()
            .unwrap_or_default()
            .to_owned(),
        timestamp_ms: 1_700_000_000_000,
        kind: "text".to_owned(),
        body: body.to_owned(),
        edited: false,
        own: false,
        truncated: false,
        warning: None,
    }
}

/// A message of the account itself.
pub fn own(event_id: &str, body: &str) -> Msg {
    Msg {
        own: true,
        ..message(event_id, BOT, body)
    }
}

/// The account `serve` runs as.
pub const BOT: &str = "@bot:example.org";
/// The person the agents ask.
pub const ANN: &str = "@ann:example.org";
/// Someone else in the room.
pub const EVE: &str = "@eve:example.org";
/// The room the tests talk in, and its name.
pub const ROOM: &str = "!agents:example.org";
pub const ROOM_NAME: &str = "Agents";

/// A running `serve` on a socket in a folder of its own, with the test in the
/// worker's place: it sees what `serve` asks for and says what the worker would.
pub struct Fake {
    pub served: Option<Served>,
    pub commands: UnboundedReceiver<MatrixCmd>,
    pub events: UnboundedSender<MatrixEvent>,
    pub socket: PathBuf,
    _dir: Dir,
}

impl Fake {
    /// Starts `serve` on a session that is logged in; nothing else is known yet.
    pub async fn start(name: &str, limits: Limits) -> Self {
        let dir = Dir::new(name);
        let socket = dir.path().join("run").join("t.sock");
        let (matrix, commands) = MatrixHandle::for_test();
        let (events, received) = unbounded_channel();
        events
            .send(MatrixEvent::LoggedIn {
                user_id: BOT.to_owned(),
            })
            .unwrap();
        let served = Served::begin(matrix, received, &socket, limits)
            .await
            .expect("serve starts");
        Self {
            served: Some(served),
            commands,
            events,
            socket,
            _dir: dir,
        }
    }

    /// Like [`Fake::start`], with the one room known and the first sync through.
    pub async fn ready(name: &str, limits: Limits) -> Self {
        let fake = Self::start(name, limits).await;
        fake.tell(MatrixEvent::Rooms(vec![room(ROOM, ROOM_NAME)]));
        fake.tell(MatrixEvent::CaughtUp);
        fake
    }

    /// The worker says something.
    pub fn tell(&self, event: MatrixEvent) {
        self.events.send(event).expect("serve is running");
    }

    /// The worker tells the newest messages of the room.
    pub fn watched(&self, messages: Vec<Msg>) {
        self.tell(MatrixEvent::Agent(AgentEvent::Watched {
            room_id: ROOM.to_owned(),
            messages,
        }));
    }

    /// The next thing `serve` asks of the worker. It may ask for nothing but what the
    /// agent channel is: anything else fails the test.
    pub async fn asked(&mut self) -> AgentCmd {
        let command = tokio::time::timeout(Duration::from_secs(10), self.commands.recv())
            .await
            .expect("serve asks the worker in time")
            .expect("serve is running");
        match command {
            MatrixCmd::Agent(command) => command,
            other => panic!(
                "serve asked the worker for something that is not its business: {}",
                other.describe()
            ),
        }
    }

    /// Whether `serve` has asked the worker for anything that was not looked at yet.
    pub fn asked_nothing(&mut self) -> bool {
        self.commands.try_recv().is_err()
    }

    /// Stops `serve` the way a signal does.
    pub async fn stop(&mut self) {
        if let Some(served) = self.served.take() {
            served.stop().await;
        }
    }
}

/// Another room the account is in, which nothing may ever be sent to or read from.
pub const OTHER_ROOM: &str = "!other:example.org";
pub const OTHER_ROOM_NAME: &str = "Other";

#[derive(Default)]
struct State {
    /// The messages of [`ROOM`], oldest first.
    messages: Vec<Msg>,
    watched: bool,
    /// The texts that were sent, as they went out.
    sent: Vec<String>,
    /// The rooms the worker was asked about.
    rooms: Vec<String>,
    /// How many reads were answered.
    reads: usize,
    /// What the worker was asked that is not the agent channel's business.
    odd: Vec<String>,
}

/// A `serve` whose worker is played by a task: one room with messages that can be
/// read, watched and sent to, as the real one would answer. For tests of what is
/// built on the socket.
pub struct World {
    pub fake: Fake,
    state: Arc<Mutex<State>>,
    player: tokio::task::JoinHandle<()>,
}

impl World {
    /// Two rooms are known ([`ROOM`] and [`OTHER_ROOM`]), the first sync is through,
    /// and [`ROOM`] holds `messages`.
    pub async fn start(name: &str, messages: Vec<Msg>) -> Self {
        let mut fake = Fake::start(name, quick()).await;
        fake.tell(MatrixEvent::Rooms(vec![
            room(ROOM, ROOM_NAME),
            room(OTHER_ROOM, OTHER_ROOM_NAME),
        ]));
        fake.tell(MatrixEvent::CaughtUp);
        let state = Arc::new(Mutex::new(State {
            messages,
            ..State::default()
        }));
        let commands = std::mem::replace(&mut fake.commands, unbounded_channel().1);
        let player = tokio::spawn(play(commands, fake.events.clone(), state.clone()));
        Self {
            fake,
            state,
            player,
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.fake.socket.clone()
    }

    /// Someone writes in the room.
    pub fn post(&self, message: Msg) {
        let mut state = self.state.lock().unwrap();
        state.messages.push(message);
        tell_watched(&state, &self.fake.events);
    }

    /// A message changes (edited, deleted, decrypted at last).
    pub fn change(&self, event_id: &str, change: impl FnOnce(&mut Msg)) {
        let mut state = self.state.lock().unwrap();
        let message = state
            .messages
            .iter_mut()
            .find(|message| message.event_id == event_id)
            .expect("the message is in the room");
        change(message);
        tell_watched(&state, &self.fake.events);
    }

    /// The texts sent to the room so far.
    pub fn sent(&self) -> Vec<String> {
        self.state.lock().unwrap().sent.clone()
    }

    /// The rooms the worker was asked about, each once.
    pub fn rooms_asked(&self) -> Vec<String> {
        let mut rooms = self.state.lock().unwrap().rooms.clone();
        rooms.sort();
        rooms.dedup();
        rooms
    }

    /// What `serve` asked of the worker that it never may.
    pub fn odd(&self) -> Vec<String> {
        self.state.lock().unwrap().odd.clone()
    }

    /// How many reads the worker has answered.
    pub fn reads(&self) -> usize {
        self.state.lock().unwrap().reads
    }

    /// Waits until the worker has answered `reads` reads in all. (A wait listens
    /// first and reads then: once its read is answered, what is posted is news.)
    pub async fn until_reads(&self, reads: usize) {
        let reached = async {
            while self.reads() < reads {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), reached)
            .await
            .unwrap_or_else(|_| panic!("the room was not read {reads} times"));
    }

    /// Waits until the room is watched (or no longer is).
    pub async fn until_watched(&self, watched: bool) {
        let reached = async {
            while self.state.lock().unwrap().watched != watched {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), reached)
            .await
            .unwrap_or_else(|_| panic!("the room never was watched: {watched}"));
    }

    pub async fn stop(mut self) {
        self.fake.stop().await;
        self.player.abort();
    }
}

fn tell_watched(state: &State, events: &UnboundedSender<MatrixEvent>) {
    if state.watched {
        let _ = events.send(MatrixEvent::Agent(AgentEvent::Watched {
            room_id: ROOM.to_owned(),
            messages: state.messages.clone(),
        }));
    }
}

/// The worker's part: answers what `serve` asks about [`ROOM`], and notes the rest.
async fn play(
    mut commands: UnboundedReceiver<MatrixCmd>,
    events: UnboundedSender<MatrixEvent>,
    state: Arc<Mutex<State>>,
) {
    let tell = |event: AgentEvent| {
        let _ = events.send(MatrixEvent::Agent(event));
    };
    while let Some(command) = commands.recv().await {
        let mut state = state.lock().unwrap();
        let command = match command {
            MatrixCmd::Agent(command) => command,
            MatrixCmd::Shutdown => continue,
            other => {
                state.odd.push(other.describe());
                continue;
            }
        };
        let (AgentCmd::Send { room_id, .. }
        | AgentCmd::Read { room_id, .. }
        | AgentCmd::Watch { room_id }
        | AgentCmd::Unwatch { room_id }) = &command;
        state.rooms.push(room_id.clone());
        let known = room_id == ROOM;
        match command {
            AgentCmd::Send {
                request, message, ..
            } => {
                let crate::format::Outgoing::Text { plain, .. } = message else {
                    state.odd.push("a send that is not a text".to_owned());
                    continue;
                };
                if !known {
                    tell(AgentEvent::Sent {
                        request,
                        result: Err("the account is not in this room".to_owned()),
                    });
                    continue;
                }
                state.sent.push(plain.clone());
                let event_id = format!("$sent-{}", state.sent.len());
                state.messages.push(own(&event_id, &plain));
                tell(AgentEvent::Sent {
                    request,
                    result: Ok(event_id),
                });
                tell_watched(&state, &events);
            }
            AgentCmd::Read { request, limit, .. } => {
                state.reads += 1;
                let skip = state.messages.len().saturating_sub(limit);
                tell(AgentEvent::Read {
                    request,
                    result: if known {
                        Ok(state.messages[skip..].to_vec())
                    } else {
                        Err("the account is not in this room".to_owned())
                    },
                });
            }
            AgentCmd::Watch { room_id } if !known => tell(AgentEvent::WatchEnded {
                room_id,
                reason: "the account is not in this room".to_owned(),
            }),
            AgentCmd::Watch { .. } => {
                state.watched = true;
                tell_watched(&state, &events);
            }
            AgentCmd::Unwatch { .. } => {
                if known {
                    state.watched = false;
                }
            }
        }
    }
}
