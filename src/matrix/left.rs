//! Rooms the user left in this session, so that they stay gone.
//!
//! Leaving and forgetting a room removes it here at once. A sync answer that was on its
//! way (or a server that mentions the room once more) can put it back, and since it
//! then comes without a membership, it comes back as a room the user is in: a room
//! that no later news corrects, because for the server there is nothing to tell.
//! Seen against a real server: a room left 30 seconds before, still listed, while a
//! fresh sync no longer knew it.
//!
//! So a room that was left is kept out of the list until something says otherwise:
//! the user joins it again here, an invitation or a knock for it arrives, or the
//! server says that the account is in it (joined again from another client). The server
//! is asked a while after leaving, and again, less and less often, for as long as the
//! room goes on looking joined.
//!
//! The list is not the only thing that would see such a room: whatever in the worker
//! wants the rooms the account is in leaves out those that are [gone](LeftRooms::gone)
//! (or, where only an id is at hand, [left](LeftRooms::is_left)).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use matrix_sdk::ruma::api::client::membership::joined_rooms;
use matrix_sdk::ruma::{OwnedRoomId, RoomId};
use matrix_sdk::{Client, Room, RoomState};
use tokio::sync::Notify;

/// A room that shows up as joined this long after it was left is asked about.
const ASK_AFTER: Duration = Duration::from_secs(10);
/// More rooms than this are not remembered: the oldest go.
const MOST: usize = 500;

/// How long to wait before asking about a room that goes on looking joined, after
/// `answers` answers that the account is not in it: less and less often, but never
/// less often than every few minutes, so that joining again elsewhere shows.
fn pause(answers: u8) -> Duration {
    match answers {
        0 => ASK_AFTER,
        1 => Duration::from_secs(30),
        2 => Duration::from_secs(60),
        _ => Duration::from_secs(5 * 60),
    }
}

struct Entry {
    /// When the room was left. Leaving it again starts a new entry, which an answer to
    /// a question about the old one says nothing about.
    since: Instant,
    /// When the server may be asked (again) whether the account is in it.
    ask_at: Instant,
    /// The answers so far that it is not, which make the pause grow.
    answers: u8,
    /// A question is on its way.
    asking: bool,
    /// The room list shows it as joined: the list is to be looked at again at `ask_at`.
    waiting: bool,
}

#[derive(Clone, Default)]
pub struct LeftRooms {
    rooms: Arc<Mutex<HashMap<OwnedRoomId, Entry>>>,
    /// The room list is to be made again: a room turned out to be joined after all, or
    /// an answer changed when to ask next.
    wake: Arc<Notify>,
}

impl std::fmt::Debug for LeftRooms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.rooms.lock().map_or(0, |rooms| rooms.len());
        write!(f, "LeftRooms({count})")
    }
}

/// What the room list does with a room (see [`LeftRooms::hides`]).
#[derive(Debug, PartialEq, Eq)]
enum Look {
    Show,
    Hide,
    /// Hides it, and asks the server about it (left at this time).
    Ask(Instant),
}

impl LeftRooms {
    /// The user left this room.
    pub fn mark(&self, room_id: &RoomId) {
        self.mark_at(room_id, Instant::now());
    }

    fn mark_at(&self, room_id: &RoomId, now: Instant) {
        let Ok(mut rooms) = self.rooms.lock() else {
            return;
        };
        // Full: the oldest goes, unless the room is one of them already.
        if rooms.len() >= MOST
            && !rooms.contains_key(room_id)
            && let Some(oldest) = rooms
                .iter()
                .min_by_key(|(_, entry)| entry.since)
                .map(|(id, _)| id.clone())
        {
            rooms.remove(&oldest);
        }
        rooms.insert(
            room_id.to_owned(),
            Entry {
                since: now,
                ask_at: now + pause(0),
                answers: 0,
                asking: false,
                waiting: false,
            },
        );
    }

    /// The user is joining this room (again): it is a room like any other.
    pub fn clear(&self, room_id: &RoomId) {
        let removed = self
            .rooms
            .lock()
            .is_ok_and(|mut rooms| rooms.remove(room_id).is_some());
        // The list may have been made while the join was on its way, without the room.
        if removed {
            self.wake.notify_one();
        }
    }

    /// Whether the room was left and nothing has said otherwise. Where the room itself
    /// is at hand, [`Self::gone`] knows better.
    pub fn is_left(&self, room_id: &RoomId) -> bool {
        self.rooms
            .lock()
            .is_ok_and(|rooms| rooms.contains_key(room_id))
    }

    /// Whether `room` was left in this session and nothing has said otherwise since: it
    /// may look joined (a sync answer that was on its way brings it back that way), but
    /// the account is not in it. An invitation or a knock since is news, so such a room
    /// is not gone.
    pub fn gone(&self, room: &Room) -> bool {
        self.gone_as(room.room_id(), room.state())
    }

    fn gone_as(&self, room_id: &RoomId, state: RoomState) -> bool {
        !matches!(state, RoomState::Invited | RoomState::Knocked) && self.is_left(room_id)
    }

    /// For the room list: whether `room` is kept out of it (it is [gone](Self::gone)).
    /// An invitation or a knock is news and ends the remembering; a room that looks
    /// joined is asked about, and asked again for as long as it goes on looking so.
    pub fn hides(&self, room: &Room) -> bool {
        match self.look(room.room_id(), room.state(), Instant::now()) {
            Look::Show => false,
            Look::Hide => true,
            Look::Ask(since) => {
                let (client, room_id, left) =
                    (room.client(), room.room_id().to_owned(), self.clone());
                tokio::spawn(async move {
                    let joined = server_says_joined(&client, &room_id).await;
                    left.answered(&room_id, since, joined, Instant::now());
                    // Back in the list, or a new time to ask: either way the list is to
                    // be looked at again.
                    left.wake.notify_one();
                });
                true
            }
        }
    }

    fn look(&self, room_id: &RoomId, state: RoomState, now: Instant) -> Look {
        let Ok(mut rooms) = self.rooms.lock() else {
            return Look::Show;
        };
        let Some(entry) = rooms.get_mut(room_id) else {
            return Look::Show;
        };
        match state {
            RoomState::Invited | RoomState::Knocked => {
                rooms.remove(room_id);
                Look::Show
            }
            RoomState::Left | RoomState::Banned => {
                entry.waiting = false;
                Look::Hide
            }
            RoomState::Joined if entry.asking => Look::Hide,
            RoomState::Joined if now >= entry.ask_at => {
                entry.asking = true;
                entry.waiting = false;
                Look::Ask(entry.since)
            }
            RoomState::Joined => {
                entry.waiting = true;
                Look::Hide
            }
        }
    }

    /// Takes the server's answer about a room asked about because it looked joined after
    /// it was left at `since` (`None`: no answer came). Whether it is back.
    fn answered(
        &self,
        room_id: &RoomId,
        since: Instant,
        joined: Option<bool>,
        now: Instant,
    ) -> bool {
        let Ok(mut rooms) = self.rooms.lock() else {
            return false;
        };
        let Some(entry) = rooms.get_mut(room_id) else {
            return false;
        };
        // Joined and left again while the question was on its way: the answer can tell of
        // the time in between, and must not undo the newer leaving.
        if entry.since != since {
            return false;
        }
        entry.asking = false;
        match joined {
            Some(true) => {
                rooms.remove(room_id);
                true
            }
            Some(false) => {
                entry.answers = entry.answers.saturating_add(1);
                entry.ask_at = now + pause(entry.answers);
                false
            }
            // No answer is not an answer: asked again after the same pause.
            None => {
                entry.ask_at = now + pause(entry.answers);
                false
            }
        }
    }

    /// When the list is next to be looked at, to ask about a room that it shows joined.
    fn next_ask(&self) -> Option<Instant> {
        let rooms = self.rooms.lock().ok()?;
        rooms
            .values()
            .filter(|entry| entry.waiting && !entry.asking)
            .map(|entry| entry.ask_at)
            .min()
    }

    /// The time to ask came: the list is looked at again, and the rooms it still shows
    /// joined are asked about then. One no longer in it is not woken for again.
    fn woke(&self, now: Instant) {
        if let Ok(mut rooms) = self.rooms.lock() {
            for entry in rooms.values_mut().filter(|entry| entry.ask_at <= now) {
                entry.waiting = false;
            }
        }
    }

    /// Resolves when the room list is to be made again: a room kept out of it turned out
    /// to be joined, an answer changed when to ask next, or it is time to ask about one
    /// (which is done while the list is made). The rooms themselves may not change
    /// meanwhile, and then nothing else would make the list again.
    pub async fn due(&self) {
        let next = self.next_ask();
        let time_to_ask = async {
            match next {
                Some(at) => tokio::time::sleep_until(at.into()).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = self.wake.notified() => {}
            () = time_to_ask => self.woke(Instant::now()),
        }
    }
}

/// Whether the server lists the room among those the account is in; `None` if it could
/// not be asked.
async fn server_says_joined(client: &Client, room_id: &RoomId) -> Option<bool> {
    let request = joined_rooms::v3::Request::new();
    let answer = tokio::time::timeout(Duration::from_secs(20), client.send(request))
        .await
        .ok()?
        .ok()?;
    Some(answer.joined_rooms.iter().any(|joined| joined == room_id))
}

/// A stand-in homeserver, for tests of what the worker does with the rooms it knows:
/// a client that has had one sync answer from it knows the rooms in that answer, and
/// what it asks later is answered from the same few things.
#[cfg(test)]
pub(super) mod testing {
    use std::sync::{Arc, Mutex};

    use matrix_sdk::authentication::matrix::MatrixSession;
    use matrix_sdk::config::SyncSettings;
    use matrix_sdk::{AuthSession, Client, SessionMeta, SessionTokens};
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The account the client is logged in as.
    pub const ME: &str = "@alice:localhost";
    /// The one other person in the rooms.
    pub const BOB: &str = "@bob:localhost";
    /// Bob's picture.
    pub const BOB_PICTURE: &str = "mxc://localhost/bob";

    #[derive(Default)]
    pub struct Server {
        /// Rooms the account is in, with Bob.
        pub joined: Vec<&'static str>,
        /// Rooms the account is in whose members did not come along (as some servers
        /// do it).
        pub without_members: Vec<&'static str>,
        /// Rooms Bob invited the account to, each as a chat with him.
        pub invited: Vec<&'static str>,
        /// Rooms the account data names as the chat with Bob.
        pub direct: Vec<&'static str>,
        /// What `/joined_rooms` lists.
        pub server_joined: Vec<&'static str>,
        /// What a request for a single event gets.
        pub event: Option<Value>,
        /// The paths asked for, in order.
        pub seen: Arc<Mutex<Vec<String>>>,
    }

    impl Server {
        /// A client of this server (logged in as [`ME`]) after its first sync.
        pub async fn client(self) -> Client {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = Arc::new(self);
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(server.clone().answer(stream));
                }
            });
            let client = Client::builder()
                .homeserver_url(format!("http://{address}"))
                .build()
                .await
                .unwrap();
            let session = MatrixSession {
                meta: SessionMeta {
                    user_id: ME.try_into().unwrap(),
                    device_id: "STANDIN".into(),
                },
                tokens: SessionTokens {
                    access_token: "token".into(),
                    refresh_token: None,
                },
            };
            client
                .restore_session(AuthSession::Matrix(session))
                .await
                .unwrap();
            client.sync_once(SyncSettings::default()).await.unwrap();
            client
        }

        async fn answer(self: Arc<Self>, mut stream: tokio::net::TcpStream) {
            // The whole request is read (the head, and the body it announces), so that
            // closing the connection does not cut the answer off.
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            let head_end = loop {
                let Ok(read) = stream.read(&mut buffer).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..read]);
                if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at + 4;
                }
            };
            let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
            let length = head
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while request.len() < head_end + length {
                match stream.read(&mut buffer).await {
                    Ok(read) if read > 0 => request.extend_from_slice(&buffer[..read]),
                    _ => break,
                }
            }
            let path = head.split_whitespace().nth(1).unwrap_or("/");
            let path = path.split('?').next().unwrap_or(path);
            if let Ok(mut seen) = self.seen.lock() {
                seen.push(path.to_owned());
            }
            let body = self.body_for(path).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }

        fn body_for(&self, path: &str) -> Value {
            if path == "/_matrix/client/versions" {
                json!({ "versions": ["v1.11"] })
            } else if path.ends_with("/sync") {
                self.sync()
            } else if path.ends_with("/joined_rooms") {
                json!({ "joined_rooms": self.server_joined })
            } else if path.contains("/event/") {
                self.event.clone().unwrap_or_else(|| json!({}))
            } else if let Some(target) = path.split("/join/").nth(1) {
                // Any room can be joined.
                json!({ "room_id": target.replace("%21", "!").replace("%3A", ":") })
            } else {
                json!({})
            }
        }

        fn sync(&self) -> Value {
            let joined: serde_json::Map<String, Value> = self
                .joined
                .iter()
                .map(|room| (room.to_string(), joined_room(room, true)))
                .chain(
                    self.without_members
                        .iter()
                        .map(|room| (room.to_string(), joined_room(room, false))),
                )
                .collect();
            let invited: serde_json::Map<String, Value> = self
                .invited
                .iter()
                .map(|room| (room.to_string(), invited_room()))
                .collect();
            json!({
                "next_batch": "s1",
                "rooms": { "join": joined, "invite": invited },
                "account_data": {
                    "events": [{ "type": "m.direct", "content": { BOB: self.direct } }]
                }
            })
        }
    }

    fn member(room: &str, user: &str, picture: Option<&str>) -> Value {
        let name = user
            .trim_start_matches('@')
            .split(':')
            .next()
            .unwrap_or(user);
        let mut content = json!({ "membership": "join", "displayname": name });
        if let Some(picture) = picture {
            content["avatar_url"] = json!(picture);
        }
        json!({
            "type": "m.room.member",
            "state_key": user,
            "sender": user,
            "event_id": format!("$member-{name}-{}", room.trim_start_matches('!')),
            "origin_server_ts": 1_000,
            "content": content
        })
    }

    fn joined_room(room: &str, members: bool) -> Value {
        let mut state = vec![json!({
            "type": "m.room.create",
            "state_key": "",
            "sender": ME,
            "event_id": format!("$create-{}", room.trim_start_matches('!')),
            "origin_server_ts": 500,
            "content": { "creator": ME, "room_version": "10" }
        })];
        if members {
            state.push(member(room, ME, None));
            state.push(member(room, BOB, Some(BOB_PICTURE)));
        }
        json!({
            "state": { "events": state },
            "timeline": { "events": [], "limited": false },
            "summary": { "m.joined_member_count": 2, "m.heroes": [BOB] }
        })
    }

    fn invited_room() -> Value {
        json!({
            "invite_state": { "events": [
                {
                    "type": "m.room.member",
                    "state_key": ME,
                    "sender": BOB,
                    "content": { "membership": "invite", "is_direct": true }
                },
                {
                    "type": "m.room.member",
                    "state_key": BOB,
                    "sender": BOB,
                    "content": { "membership": "join" }
                }
            ] }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::room_id;

    #[test]
    fn a_left_room_is_remembered_until_it_is_joined_again() {
        let left = LeftRooms::default();
        let room = room_id!("!gone:example.org");
        assert!(!left.is_left(room));
        left.mark(room);
        assert!(left.is_left(room));
        assert!(!left.is_left(room_id!("!other:example.org")));
        left.clear(room);
        assert!(!left.is_left(room));
        // Nothing of a room is in the debug output.
        left.mark(room);
        assert_eq!(format!("{left:?}"), "LeftRooms(1)");
    }

    #[tokio::test]
    async fn joining_a_room_left_here_again_makes_the_list_again() {
        let left = LeftRooms::default();
        let room = room_id!("!back:example.org");
        left.clear(room);
        let woken = tokio::time::timeout(Duration::from_millis(50), left.due()).await;
        assert!(woken.is_err(), "nothing to make the list again for");
        left.mark(room);
        left.clear(room);
        tokio::time::timeout(Duration::from_secs(5), left.due())
            .await
            .expect("the list is made again with the room in it");
    }

    #[test]
    fn only_so_many_are_remembered() {
        let left = LeftRooms::default();
        for n in 0..MOST + 20 {
            let id = OwnedRoomId::try_from(format!("!r{n}:example.org")).unwrap();
            left.mark(&id);
        }
        assert_eq!(left.rooms.lock().unwrap().len(), MOST);
        // The newest are the ones kept.
        let newest = OwnedRoomId::try_from(format!("!r{}:example.org", MOST + 19)).unwrap();
        assert!(left.is_left(&newest));
    }

    #[test]
    fn leaving_a_remembered_room_again_does_not_push_another_out() {
        let left = LeftRooms::default();
        let start = Instant::now();
        let ids: Vec<OwnedRoomId> = (0..MOST)
            .map(|n| OwnedRoomId::try_from(format!("!r{n}:example.org")).unwrap())
            .collect();
        for (n, id) in ids.iter().enumerate() {
            left.mark_at(id, start + Duration::from_millis(n as u64));
        }
        left.mark_at(&ids[MOST - 1], start + Duration::from_secs(10));
        assert_eq!(left.rooms.lock().unwrap().len(), MOST);
        assert!(left.is_left(&ids[0]), "the oldest stays");
    }

    #[test]
    fn a_room_that_goes_on_looking_joined_is_asked_about_less_and_less_often() {
        let left = LeftRooms::default();
        let room = room_id!("!gone:example.org");
        let start = Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        left.mark_at(room, start);
        // Brought back by a late answer: hidden, not asked about yet.
        assert_eq!(left.look(room, RoomState::Joined, at(2)), Look::Hide);
        assert_eq!(left.look(room, RoomState::Joined, at(10)), Look::Ask(start));
        // One question at a time.
        assert_eq!(left.look(room, RoomState::Joined, at(11)), Look::Hide);
        // Each "no" makes the next pause longer, up to a few minutes.
        let mut now = 12;
        for pause in [30, 60, 300, 300, 300] {
            assert!(!left.answered(room, start, Some(false), at(now)));
            assert_eq!(
                left.look(room, RoomState::Joined, at(now + pause - 1)),
                Look::Hide
            );
            assert_eq!(
                left.look(room, RoomState::Joined, at(now + pause)),
                Look::Ask(start)
            );
            now += pause + 1;
        }
        assert!(left.is_left(room));
    }

    #[test]
    fn a_question_that_fails_is_not_an_answer() {
        let left = LeftRooms::default();
        let room = room_id!("!gone:example.org");
        let start = Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        left.mark_at(room, start);
        assert_eq!(left.look(room, RoomState::Joined, at(10)), Look::Ask(start));
        // No answer: asked again, after the pause before the first question.
        assert!(!left.answered(room, start, None, at(15)));
        assert_eq!(left.look(room, RoomState::Joined, at(24)), Look::Hide);
        assert_eq!(left.look(room, RoomState::Joined, at(25)), Look::Ask(start));
        // The first real answer is still the first: the pause after it is the shortest.
        assert!(!left.answered(room, start, Some(false), at(26)));
        assert_eq!(left.look(room, RoomState::Joined, at(55)), Look::Hide);
        assert_eq!(left.look(room, RoomState::Joined, at(56)), Look::Ask(start));
    }

    #[test]
    fn the_server_saying_the_account_is_in_brings_the_room_back() {
        let left = LeftRooms::default();
        let room = room_id!("!back:example.org");
        let start = Instant::now();
        left.mark_at(room, start);
        let asked = start + Duration::from_secs(10);
        assert_eq!(left.look(room, RoomState::Joined, asked), Look::Ask(start));
        assert!(left.answered(room, start, Some(true), asked));
        assert!(!left.is_left(room));
        assert_eq!(left.look(room, RoomState::Joined, asked), Look::Show);
    }

    #[test]
    fn an_answer_about_an_earlier_leaving_does_not_undo_a_later_one() {
        let left = LeftRooms::default();
        let room = room_id!("!again:example.org");
        let start = Instant::now();
        left.mark_at(room, start);
        let asked = start + Duration::from_secs(10);
        assert_eq!(left.look(room, RoomState::Joined, asked), Look::Ask(start));
        // Meanwhile joined and left again; then the old question is answered "joined".
        left.clear(room);
        let again = start + Duration::from_secs(11);
        left.mark_at(room, again);
        assert!(!left.answered(room, start, Some(true), again));
        assert!(left.is_left(room));
        // The newer leaving has its own question, in time.
        assert_eq!(
            left.look(room, RoomState::Joined, again + Duration::from_secs(10)),
            Look::Ask(again)
        );
    }

    #[test]
    fn the_list_is_woken_to_ask_about_a_room_it_shows_joined() {
        let left = LeftRooms::default();
        let room = room_id!("!gone:example.org");
        let start = Instant::now();
        left.mark_at(room, start);
        // Nothing shows it: nothing to wake for.
        assert_eq!(left.next_ask(), None);
        assert_eq!(left.look(room, RoomState::Joined, start), Look::Hide);
        assert_eq!(left.next_ask(), Some(start + ASK_AFTER));
        // Woken, the list looks again; a room it no longer shows is not woken for again.
        left.woke(start + ASK_AFTER);
        assert_eq!(left.next_ask(), None);
        // While a question is on its way, the answer is what wakes the list.
        assert_eq!(
            left.look(room, RoomState::Joined, start + ASK_AFTER),
            Look::Ask(start)
        );
        assert_eq!(left.next_ask(), None);
        left.answered(room, start, Some(false), start + ASK_AFTER);
        assert_eq!(
            left.look(room, RoomState::Joined, start + ASK_AFTER),
            Look::Hide
        );
        assert_eq!(left.next_ask(), Some(start + ASK_AFTER + pause(1)));
        // The server says it is gone: nothing to ask about.
        assert_eq!(
            left.look(room, RoomState::Left, start + ASK_AFTER),
            Look::Hide
        );
        assert_eq!(left.next_ask(), None);
    }

    #[tokio::test]
    async fn the_list_is_made_again_when_a_question_is_due_or_answered() {
        let left = LeftRooms::default();
        let room = room_id!("!gone:example.org");
        // Left long enough ago that the question is due now.
        let now = Instant::now();
        left.mark_at(room, now);
        if let Some(entry) = left.rooms.lock().unwrap().get_mut(room) {
            entry.ask_at = now;
            entry.waiting = true;
        }
        tokio::time::timeout(Duration::from_secs(5), left.due())
            .await
            .expect("woken when the question is due");
        assert_eq!(left.next_ask(), None, "woken once");
        // An answer wakes it too, even when it came while nobody was waiting.
        left.wake.notify_one();
        tokio::time::timeout(Duration::from_secs(5), left.due())
            .await
            .expect("woken by the answer");
    }

    #[test]
    fn an_invitation_or_a_knock_after_leaving_is_news() {
        let left = LeftRooms::default();
        let start = Instant::now();
        for state in [RoomState::Invited, RoomState::Knocked] {
            // A declined invitation (or a withdrawn knock) that a late answer brings
            // back looking joined stays hidden...
            let room = room_id!("!declined:example.org");
            left.mark_at(room, start);
            assert!(left.gone_as(room, RoomState::Joined));
            assert_eq!(left.look(room, RoomState::Joined, start), Look::Hide);
            // ...but an invitation or a knock that comes later is shown.
            assert!(!left.gone_as(room, state));
            assert_eq!(left.look(room, state, start), Look::Show);
            assert!(!left.is_left(room));
            assert!(!left.gone_as(room, RoomState::Joined));
        }
    }

    #[test]
    fn a_room_left_here_is_gone_whatever_it_looks_like() {
        let left = LeftRooms::default();
        let room = room_id!("!gone:example.org");
        assert!(!left.gone_as(room, RoomState::Joined));
        left.mark(room);
        for state in [RoomState::Joined, RoomState::Left, RoomState::Banned] {
            assert!(left.gone_as(room, state));
        }
    }

    #[tokio::test]
    async fn the_server_is_asked_which_rooms_the_account_is_in() {
        let client = testing::Server {
            server_joined: vec!["!in:localhost"],
            ..testing::Server::default()
        }
        .client()
        .await;
        assert_eq!(
            server_says_joined(&client, room_id!("!in:localhost")).await,
            Some(true)
        );
        assert_eq!(
            server_says_joined(&client, room_id!("!out:localhost")).await,
            Some(false)
        );
    }
}
