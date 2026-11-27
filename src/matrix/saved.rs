//! Saved messages: what the list shows of them. The app keeps only where a saved
//! message is; who wrote it and what it says is loaded here when the list is shown
//! (or a reminder is due), the way the pinned messages are: from the event cache where
//! it is there, else from the server.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use matrix_sdk::Room;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::UnboundedSender;

use super::MatrixEvent;
use super::moderation::{self, EventSummary, PinnedCache, SummaryKind};
use crate::rt;
use crate::saved::{SavedDetails, SavedText};

/// The most messages looked up for one request; the list asks for what it shows.
pub const MAX_FETCH: usize = 40;
/// Lookups at a time, over all requests together: the list asks again as it is
/// scrolled, and none of that may add up.
pub(super) const CONCURRENCY: usize = 4;
/// How long one message may take before it counts as not to be had.
const LOAD_TIMEOUT: Duration = Duration::from_secs(20);

pub enum SavedCmd {
    /// Look up these messages, each `(room, event)`. Answered with
    /// [`SavedEvent::Details`] for `request`, message by message as they are found.
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
    /// What was found about some of the messages of a [`SavedCmd::Fetch`]; each
    /// message is answered for once.
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

/// Where the room of a saved message stands for the account.
pub(super) enum Place<R> {
    /// The account is in it: `R` reads its messages.
    Joined(R),
    /// The account left it, or was banned from it.
    Left,
    /// Not known (yet), or only invited to: nothing can be said about the message.
    Unknown,
}

/// Looks the messages up and tells of each as soon as it is found, so that those
/// that are in the event cache show at once. `limit` is shared by all requests.
pub(super) async fn fetch(
    request: u64,
    wanted: Vec<(String, String, Place<Room>)>,
    cache: Arc<Mutex<PinnedCache>>,
    limit: Arc<Semaphore>,
    events: UnboundedSender<MatrixEvent>,
) {
    futures::stream::iter(wanted)
        .for_each_concurrent(None, |(room_id, event_id, place)| {
            let (cache, limit, events) = (cache.clone(), limit.clone(), events.clone());
            async move {
                // Waiting for a turn is not part of the time a message may take.
                let _turn = match &place {
                    Place::Joined(_) => limit.acquire().await.ok(),
                    Place::Left | Place::Unknown => None,
                };
                let load = |room: Room, event_id: String| async move {
                    moderation::summarize_event(&room, &event_id, &cache).await
                };
                let details = one(room_id, event_id, place, LOAD_TIMEOUT, load).await;
                let _ = events.send(MatrixEvent::Saved(SavedEvent::Details {
                    request,
                    items: vec![details],
                }));
            }
        })
        .await;
}

/// What the list shows of one message. `load` reads it from its room, and is given
/// `wait` to do so.
async fn one<R, F, Fut>(
    room_id: String,
    event_id: String,
    place: Place<R>,
    wait: Duration,
    load: F,
) -> SavedDetails
where
    F: FnOnce(R, String) -> Fut,
    Fut: Future<Output = EventSummary>,
{
    let mut details = SavedDetails {
        room_id,
        event_id,
        sender: String::new(),
        sender_id: None,
        snippet: String::new(),
        text: SavedText::Unavailable,
    };
    let room = match place {
        Place::Joined(room) => room,
        Place::Left => {
            details.text = SavedText::RoomGone;
            return details;
        }
        Place::Unknown => return details,
    };
    let loading = load(room, details.event_id.clone());
    let Ok(EventSummary::Found {
        sender,
        sender_id,
        kind,
    }) = rt::timeout(wait, loading).await
    else {
        return details;
    };
    details.sender = sender;
    details.sender_id = sender_id;
    details.text = match kind {
        SummaryKind::Redacted => SavedText::Redacted,
        SummaryKind::Undecryptable => SavedText::Undecryptable,
        SummaryKind::Text(text) => {
            details.snippet = text;
            SavedText::Text
        }
        // A kind of event the list has no words for.
        SummaryKind::NoText => {
            details.snippet = "[message]".to_owned();
            SavedText::Text
        }
    };
    details
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    fn found(kind: SummaryKind) -> EventSummary {
        EventSummary::Found {
            sender: "Ann".into(),
            sender_id: Some("@ann:example.org".into()),
            kind,
        }
    }

    /// What `one` says of a message of a joined room that loads as `summary`.
    async fn loaded(summary: EventSummary) -> SavedDetails {
        let load = |(), event_id: String| async move {
            assert_eq!(event_id, "$e");
            summary
        };
        one("!r".into(), "$e".into(), Place::Joined(()), WAIT, load).await
    }

    #[tokio::test]
    async fn a_message_is_told_by_what_it_is() {
        let text = loaded(found(SummaryKind::Text("hello".into()))).await;
        assert_eq!(
            (text.text, text.snippet.as_str()),
            (SavedText::Text, "hello")
        );
        assert_eq!(text.sender, "Ann");
        assert_eq!(text.sender_id.as_deref(), Some("@ann:example.org"));
        assert_eq!(
            (text.room_id.as_str(), text.event_id.as_str()),
            ("!r", "$e")
        );

        let deleted = loaded(found(SummaryKind::Redacted)).await;
        assert_eq!(deleted.text, SavedText::Redacted);
        assert!(deleted.snippet.is_empty());
        assert_eq!(deleted.sender, "Ann");

        let locked = loaded(found(SummaryKind::Undecryptable)).await;
        assert_eq!(locked.text, SavedText::Undecryptable);
        assert!(locked.snippet.is_empty());

        // A kind of event without text: no words of the pinned list's.
        let other = loaded(found(SummaryKind::NoText)).await;
        assert_eq!(
            (other.text, other.snippet.as_str()),
            (SavedText::Text, "[message]")
        );

        let missing = loaded(EventSummary::Unavailable).await;
        assert_eq!(missing.text, SavedText::Unavailable);
        assert!(missing.sender.is_empty() && missing.sender_id.is_none());
    }

    #[tokio::test]
    async fn a_message_that_takes_too_long_is_not_to_be_had() {
        let never = |(), _: String| std::future::pending::<EventSummary>();
        let wait = Duration::from_millis(20);
        let late = one("!r".into(), "$e".into(), Place::Joined(()), wait, never).await;
        assert_eq!(late.text, SavedText::Unavailable);
    }

    #[tokio::test]
    async fn without_a_room_nothing_is_loaded_and_only_a_room_that_was_left_is_gone() {
        fn not_there() -> EventSummary {
            panic!("there is nothing to load from");
        }
        let never_asked = |(), _: String| async { not_there() };
        let left = one("!r".into(), "$e".into(), Place::Left, WAIT, never_asked).await;
        assert_eq!(left.text, SavedText::RoomGone);
        // Not known (yet), or only invited to: that may change.
        let unknown = one("!r".into(), "$e".into(), Place::Unknown, WAIT, never_asked).await;
        assert_eq!(unknown.text, SavedText::Unavailable);
    }

    #[tokio::test]
    async fn every_message_is_answered_for_on_its_own_and_cached_ones_do_not_wait() {
        let (events, mut answers) = tokio::sync::mpsc::unbounded_channel();
        let wanted = vec![
            ("!left".to_owned(), "$a".to_owned(), Place::Left),
            ("!unknown".to_owned(), "$b".to_owned(), Place::Unknown),
        ];
        let limit = Arc::new(Semaphore::new(CONCURRENCY));
        // All turns are taken by other requests: what needs none is answered still.
        let _busy = limit
            .clone()
            .acquire_many_owned(CONCURRENCY as u32)
            .await
            .unwrap();
        fetch(7, wanted, Arc::default(), limit, events).await;
        let mut told = Vec::new();
        while let Ok(MatrixEvent::Saved(SavedEvent::Details { request, items })) =
            answers.try_recv()
        {
            assert_eq!(request, 7);
            assert_eq!(items.len(), 1);
            told.push((items[0].event_id.clone(), items[0].text));
        }
        told.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            told,
            [
                ("$a".to_owned(), SavedText::RoomGone),
                ("$b".to_owned(), SavedText::Unavailable)
            ]
        );
    }
}
