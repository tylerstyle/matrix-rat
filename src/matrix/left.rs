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
//! server, asked once after a while, says that the account is in it (joined again from
//! another client).

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

#[derive(Default)]
struct Entry {
    since: Option<Instant>,
    /// The server has been asked (or is being asked) whether the account is in it.
    asked: bool,
}

#[derive(Clone, Default)]
pub struct LeftRooms {
    rooms: Arc<Mutex<HashMap<OwnedRoomId, Entry>>>,
    /// A room turned out to be joined after all: the list is to be made again.
    back: Arc<Notify>,
}

impl std::fmt::Debug for LeftRooms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.rooms.lock().map_or(0, |rooms| rooms.len());
        write!(f, "LeftRooms({count})")
    }
}

impl LeftRooms {
    /// The user left this room.
    pub fn mark(&self, room_id: &RoomId) {
        let Ok(mut rooms) = self.rooms.lock() else {
            return;
        };
        if rooms.len() >= MOST
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
                since: Some(Instant::now()),
                asked: false,
            },
        );
    }

    /// The user is joining this room (again): it is a room like any other.
    pub fn clear(&self, room_id: &RoomId) {
        if let Ok(mut rooms) = self.rooms.lock() {
            rooms.remove(room_id);
        }
    }

    /// Whether the room was left and nothing has said otherwise.
    pub fn is_left(&self, room_id: &RoomId) -> bool {
        self.rooms
            .lock()
            .is_ok_and(|rooms| rooms.contains_key(room_id))
    }

    /// For the room list: whether `room` is kept out of it. An invitation or a knock is
    /// news and ends the remembering; a room that looks joined is asked about, once.
    pub fn hides(&self, room: &Room) -> bool {
        let room_id = room.room_id();
        let ask = {
            let Ok(mut rooms) = self.rooms.lock() else {
                return false;
            };
            let Some(entry) = rooms.get_mut(room_id) else {
                return false;
            };
            match room.state() {
                RoomState::Invited | RoomState::Knocked => {
                    rooms.remove(room_id);
                    return false;
                }
                RoomState::Left | RoomState::Banned => return true,
                RoomState::Joined => {
                    let due = entry.since.is_none_or(|since| since.elapsed() >= ASK_AFTER);
                    let ask = due && !entry.asked;
                    entry.asked |= ask;
                    ask
                }
            }
        };
        if ask {
            let (client, room_id, left) = (room.client(), room_id.to_owned(), self.clone());
            tokio::spawn(async move {
                if server_says_joined(&client, &room_id).await == Some(true) {
                    left.clear(&room_id);
                    left.back.notify_one();
                }
            });
        }
        true
    }

    /// Resolves when a room that was kept out turned out to be joined.
    pub async fn back(&self) {
        self.back.notified().await;
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
}
