//! The client's event cache, which every client of this program keeps on.
//!
//! Since matrix-sdk 0.20 a client can be built without one (`Client::event_cache` and
//! `Client::latest_events` answer an `Option`), but no client here turns it off: the
//! timelines, the latest messages of the room list, search and paging all need it, the
//! agent's clients as much as the app's. A missing cache is therefore a failure, never a
//! panic: it is logged by its kind and handled as the caller handles a failed load.

use std::sync::Arc;

use matrix_sdk::event_cache::{EventCache, EventCacheDropHandles, RoomEventCache};
use matrix_sdk::{Client, Room};

use crate::logsafe::Why;

/// What the log says when the cache is not there.
const OFF: Why = Why::Word("event cache off");

/// The client's event cache; `None`, logged, if the client has none.
pub(super) fn of(client: &Client) -> Option<&EventCache> {
    let cache = client.event_cache();
    if cache.is_none() {
        tracing::warn!(why = %OFF, "the client has no event cache");
    }
    cache
}

/// The event cache of `room` (`Room::event_cache` before matrix-sdk 0.20), with the
/// handles that keep the cache's tasks running while it is held. `None` if the client
/// has no cache (logged) or the room's could not be had.
pub(super) async fn room(room: &Room) -> Option<(RoomEventCache, Arc<EventCacheDropHandles>)> {
    let client = room.client();
    of(&client)?.room(room.room_id()).await.ok()
}
