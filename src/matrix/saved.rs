//! Saved messages: what the list shows of them. The app keeps only where a saved
//! message is; who wrote it and what it says is loaded here when the list is shown
//! (or a reminder is due), the way the pinned messages are: from the event cache where
//! it is there, else from the server.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use matrix_sdk::Room;

use super::moderation::{self, PinnedCache};
use crate::saved::{SavedDetails, SavedText};

/// The most messages looked up for one request; the list asks for what it shows.
pub const MAX_FETCH: usize = 40;
/// Lookups of messages that are not in the event cache, at a time.
const CONCURRENCY: usize = 4;
/// How long one message may take before it counts as not to be had.
const LOAD_TIMEOUT: Duration = Duration::from_secs(20);

pub enum SavedCmd {
    /// Look up these messages, each `(room, event)`. Answered with
    /// [`SavedEvent::Details`] for `request`.
    Fetch {
        request: u64,
        items: Vec<(String, String)>,
    },
}

/// Never prints a room or a message.
impl std::fmt::Debug for SavedCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fetch { request, items } => {
                write!(f, "Fetch(request {request}, {} items)", items.len())
            }
        }
    }
}

pub enum SavedEvent {
    /// What was found about the messages of a [`SavedCmd::Fetch`].
    Details {
        request: u64,
        items: Vec<SavedDetails>,
    },
}

/// Never prints a room, a message, a name or a text.
impl std::fmt::Debug for SavedEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Details { request, items } => {
                write!(f, "Details(request {request}, {} items)", items.len())
            }
        }
    }
}

/// Looks the messages up, in their order. `room` is `None` where the account is not
/// in the room (any more).
pub(super) async fn fetch(
    wanted: Vec<(String, String, Option<Room>)>,
    cache: Arc<Mutex<PinnedCache>>,
) -> Vec<SavedDetails> {
    futures::stream::iter(wanted)
        .map(|(room_id, event_id, room)| {
            let cache = cache.clone();
            async move { one(room_id, event_id, room, &cache).await }
        })
        .buffered(CONCURRENCY)
        .collect()
        .await
}

async fn one(
    room_id: String,
    event_id: String,
    room: Option<Room>,
    cache: &Arc<Mutex<PinnedCache>>,
) -> SavedDetails {
    let mut details = SavedDetails {
        room_id,
        event_id,
        sender: String::new(),
        sender_id: None,
        snippet: String::new(),
        text: SavedText::Unavailable,
    };
    let Some(room) = room else {
        details.text = SavedText::RoomGone;
        return details;
    };
    let loading = moderation::summarize_event(&room, &details.event_id, cache);
    let Ok(Some(found)) = tokio::time::timeout(LOAD_TIMEOUT, loading).await else {
        return details;
    };
    details.sender = found.sender;
    details.sender_id = found.sender_id;
    details.text = if found.redacted {
        SavedText::Redacted
    } else if found.undecryptable {
        SavedText::Undecryptable
    } else {
        // Something without text (a kind of event the list has no words for).
        details.snippet = if found.snippet == moderation::UNSUPPORTED {
            "[message]".to_owned()
        } else {
            found.snippet
        };
        SavedText::Text
    };
    details
}
