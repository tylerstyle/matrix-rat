//! A page loading in the background for the UI, and when another request for one counts as
//! already answered; and a page of older messages loaded so that a panic in the SDK is a
//! failed load, not the end of the task that asked for it.

use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::{FutureExt, pin_mut};
use matrix_sdk::SendOutsideWasm;
use matrix_sdk_ui::timeline::{Error as TimelineError, Timeline};
use tokio::sync::mpsc::UnboundedSender;

use super::MatrixEvent;
use crate::logsafe;
use crate::rt::{self, JoinHandle};

/// A page (older messages, newer ones of a detached slice, more rooms of the directory)
/// loading in the background, which answers the UI once it is loaded. A request for a page
/// made while one is loading is answered by that one's answer, and dropped. A page counts
/// as loading until it has answered, not until its task has ended: the UI asks for the
/// next page as soon as it hears the answer, which can be before the task has ended, and
/// that request would be dropped and never answered (the UI waits for an answer to every
/// request, so it would ask for no more pages).
pub(super) struct Paging {
    /// Aborted, the page does not answer if it has not yet.
    pub(super) task: JoinHandle<()>,
    answered: Arc<AtomicBool>,
}

impl Paging {
    /// Loads the page with `load`, and sends the UI the answer it gives.
    pub(super) fn spawn(
        events: UnboundedSender<MatrixEvent>,
        load: impl Future<Output = MatrixEvent> + SendOutsideWasm + 'static,
    ) -> Self {
        let answered = Arc::new(AtomicBool::new(false));
        let done = answered.clone();
        let task = rt::spawn(async move {
            let answer = load.await;
            done.store(true, Ordering::Release);
            let _ = events.send(answer);
        });
        Self { task, answered }
    }

    /// Whether the page has yet to answer. One whose task ended without answering (it
    /// panicked) never will, and the next request is not left to wait for it.
    pub(super) fn loading(&self) -> bool {
        !self.answered.load(Ordering::Acquire) && !self.task.is_finished()
    }
}

/// How often a page that is loading is looked at again, for a panic that another task
/// took (see [`backwards`]).
const LOOK_AGAIN: Duration = Duration::from_secs(1);

/// Loads up to `count` older events into `timeline` (the SDK's `paginate_backwards`, which
/// nothing else calls): whether the room's start was reached.
///
/// matrix-sdk 0.19.1 can panic in there (matrix-rust-sdk #5416, still open): when a page
/// from the server holds events the room's event cache has already stored, it drops the
/// stored copies, and fails with "The chunk is not found" (its order tracker does not know
/// where they are) or "failed to remove an event". The panic ended the task that asked,
/// and with it what waited for the answer: the UI's "loading older messages" stayed for
/// good, an agent's listener never became ready. Here it is a failed load like any other,
/// which the caller handles as it handles an error.
///
/// The SDK shares one page between all who ask for it and a task of its own, and the one
/// that happens to be working on it takes the panic. If that is not this one, this one may
/// never be woken (futures' `Shared` wakes those who wait when the page is done, not when
/// it panics), so the page is looked at again every [`LOOK_AGAIN`]: one that panicked
/// elsewhere then panics here too.
///
/// What the panic leaves of the event cache is the SDK's affair. Its locks are tokio's,
/// which a panic releases and does not poison, and its pagination status goes back to
/// what it was (the guard for that is dropped as the panic unwinds), so the next page
/// starts afresh. But the panic says that the room's events in memory and in the store
/// disagree, and that can last: on the dev homeserver (its test account has an ignore
/// list, for which the SDK clears all its caches at the start of each session), every
/// later page of the room panicked too, and the room's new events were refused by the
/// store, until the caches were cleared again.
pub(super) async fn backwards(timeline: &Timeline, count: u16) -> Result<bool, PageError> {
    #[expect(
        clippy::disallowed_methods,
        reason = "the one place it is called, with its panic caught"
    )]
    let page = timeline.paginate_backwards(count);
    // One process holds one account at a time; a logout and a login within the bound
    // share it, which only delays a clear.
    static LAST_CLEAR: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    match unpanicked(page, LOOK_AGAIN).await {
        Err(PageError::Panicked { .. }) => {
            let cleared = clear_due(&LAST_CLEAR, std::time::Instant::now())
                && clear_caches(timeline.room().client()).await;
            Err(PageError::Panicked { cleared })
        }
        answer => answer,
    }
}

/// Clears all rooms' event caches after a panic: the room's events in memory and in the
/// store disagree, and stay so (later pages panic too, the room's new events are refused
/// by the store), so they are cleared and loaded anew, as the SDK itself does when the
/// ignore list changes (0.19.1 has no public way to clear one room). Open rooms then page
/// again. Whether they were cleared.
///
/// In a task of its own, which the caller waits for: the SDK empties the store first and
/// then reloads what it holds in memory, and a caller's timeout or abort between the two
/// would leave them apart for all rooms. A task that is no longer waited for runs to its
/// end.
async fn clear_caches(client: matrix_sdk::Client) -> bool {
    let clear = rt::spawn(async move {
        match client.event_cache().clear_all_rooms().await {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(why = %logsafe::kind(&err), "could not clear the event cache after a panic");
                false
            }
        }
    });
    clear.await.unwrap_or(false)
}

/// The least time between two clears of the event cache after a panic: a page that
/// panics again after one does not clear the caches over and over (each clear makes the
/// open rooms page again).
const CLEAR_EVERY: Duration = Duration::from_secs(5 * 60);

/// Whether the caches may be cleared after a panic at `now`, the last clear being kept in
/// `last`; if so, `now` is kept, and the next time is [`CLEAR_EVERY`] later.
fn clear_due(last: &std::sync::Mutex<Option<std::time::Instant>>, now: std::time::Instant) -> bool {
    let mut last = last
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last.is_some_and(|at| now.saturating_duration_since(at) < CLEAR_EVERY) {
        return false;
    }
    *last = Some(now);
    true
}

/// `page`'s answer, or [`PageError::Panicked`] if it panicked, looked at again every
/// `look_again` while it loads. Nothing of the panic is kept: its message can quote what
/// it failed on.
async fn unpanicked(
    page: impl Future<Output = Result<bool, TimelineError>>,
    look_again: Duration,
) -> Result<bool, PageError> {
    let page = AssertUnwindSafe(page).catch_unwind();
    pin_mut!(page);
    let answer = loop {
        tokio::select! {
            biased;
            answer = &mut page => break answer,
            // Only so that `page` is polled again.
            () = rt::sleep(look_again) => {}
        }
    };
    match answer {
        Ok(answer) => answer.map_err(PageError::Timeline),
        Err(_) => {
            let err = PageError::Panicked { cleared: false };
            tracing::warn!(
                why = %logsafe::kind(&err),
                "could not load older messages: taken as a failed load"
            );
            Err(err)
        }
    }
}

/// Why a page of older messages was not loaded.
pub(crate) enum PageError {
    /// The timeline said so.
    Timeline(TimelineError),
    /// The SDK panicked while loading it (see [`backwards`]). `cleared`: all rooms' event
    /// caches were cleared after it, so the timeline starts afresh (its reset reaches its
    /// subscribers on its own) and a page asked for now is of the cleared caches.
    Panicked { cleared: bool },
}

impl fmt::Display for PageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeline(err) => write!(f, "{err}"),
            Self::Panicked { .. } => {
                f.write_str("the Matrix library failed while loading the page")
            }
        }
    }
}

/// Hand-written: the timeline's error can name rooms and events; only its kind is printed.
impl fmt::Debug for PageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeline(err) => write!(f, "Timeline({})", logsafe::kind(err)),
            Self::Panicked { cleared } => write!(f, "Panicked {{ cleared: {cleared} }}"),
        }
    }
}

impl std::error::Error for PageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Timeline(err) => Some(err),
            Self::Panicked { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::panic::AssertUnwindSafe;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Poll;
    use std::time::Duration;

    use futures::FutureExt;
    use matrix_sdk_ui::timeline::{Error as TimelineError, PaginationError};

    use super::{PageError, unpanicked};
    use crate::logsafe;

    const SOON: Duration = Duration::from_millis(10);

    #[tokio::test]
    async fn a_panic_while_paging_is_a_failed_load() {
        let page = std::future::poll_fn(|_| -> Poll<Result<bool, TimelineError>> {
            panic!("The chunk is not found (in !secret:example.org)")
        });
        let err = unpanicked(page, SOON)
            .await
            .expect_err("the panic is an error");
        assert!(
            matches!(err, PageError::Panicked { cleared: false }),
            "{err:?}"
        );
        assert_eq!(logsafe::kind(&err).to_string(), "SDK panicked");
        // Nothing of the panic's own words.
        let shown = format!("{err} {err:?} {}", logsafe::kind(&err));
        assert!(
            !shown.contains("secret") && !shown.contains("chunk"),
            "{shown}"
        );
    }

    #[tokio::test]
    async fn a_panic_while_paging_leaves_the_task_running() {
        let task = tokio::spawn(async {
            let page = std::future::poll_fn(|_| -> Poll<Result<bool, TimelineError>> {
                panic!("The chunk is not found")
            });
            let first = unpanicked(page, SOON).await;
            // The task goes on to the next page, as a caller does after a failed one.
            let second = unpanicked(async { Ok(true) }, SOON).await;
            (first.is_err(), second.ok())
        });
        assert_eq!(
            task.await.expect("the task did not panic"),
            (true, Some(true))
        );
    }

    /// As in the SDK: the page is a shared future, which the caller and a task of the SDK
    /// both wait for. Here the SDK's task works on it when it panics, after the caller
    /// has polled it last, and nothing wakes the caller.
    #[tokio::test]
    async fn a_panic_that_another_task_took_is_a_failed_load_too() {
        let polls = Arc::new(AtomicUsize::new(0));
        let counted = polls.clone();
        let shared = std::future::poll_fn(move |_| -> Poll<bool> {
            // Waits (without asking to be woken) the first time, panics the second.
            if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                Poll::Pending
            } else {
                panic!("The chunk is not found")
            }
        })
        .boxed()
        .shared();
        let ours = shared.clone();
        let caller = tokio::spawn(unpanicked(async move { Ok(ours.await) }, SOON));
        while polls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        // The SDK's task polls it next, and takes the panic.
        let theirs = AssertUnwindSafe(shared).catch_unwind().await;
        assert!(theirs.is_err());
        let answer = tokio::time::timeout(Duration::from_secs(10), caller)
            .await
            .expect("the caller is not left waiting")
            .expect("the caller did not panic");
        assert!(
            matches!(answer, Err(PageError::Panicked { cleared: false })),
            "{answer:?}"
        );
        assert_eq!(
            polls.load(Ordering::SeqCst),
            2,
            "the page was not loaded again"
        );
    }

    #[tokio::test]
    async fn an_answer_or_an_error_of_the_timeline_comes_through() {
        assert!(!unpanicked(async { Ok(false) }, SOON).await.unwrap());
        assert!(unpanicked(async { Ok(true) }, SOON).await.unwrap());
        // One that takes a while is looked at again meanwhile, and still answers.
        let slow = async {
            tokio::time::sleep(SOON * 5).await;
            Ok(true)
        };
        assert!(unpanicked(slow, SOON).await.unwrap());
        let err = unpanicked(
            async {
                Err(TimelineError::PaginationError(
                    PaginationError::NotSupported,
                ))
            },
            SOON,
        )
        .await
        .expect_err("an error");
        assert!(
            matches!(
                err,
                PageError::Timeline(TimelineError::PaginationError(
                    PaginationError::NotSupported
                ))
            ),
            "{err:?}"
        );
        // The caller logs it as it logged the timeline's own error.
        assert_eq!(logsafe::kind(&err).to_string(), "pagination");
        assert_eq!(format!("{err:?}"), "Timeline(pagination)");
    }

    /// The caches are cleared after a panic at most once in [`super::CLEAR_EVERY`].
    #[test]
    fn the_caches_are_cleared_at_most_once_in_a_while() {
        let last = std::sync::Mutex::new(None);
        let start = std::time::Instant::now();
        assert!(super::clear_due(&last, start));
        assert!(!super::clear_due(&last, start));
        assert!(!super::clear_due(
            &last,
            start + super::CLEAR_EVERY - Duration::from_secs(1)
        ));
        assert!(super::clear_due(&last, start + super::CLEAR_EVERY));
        assert!(!super::clear_due(&last, start + super::CLEAR_EVERY));
    }
}
