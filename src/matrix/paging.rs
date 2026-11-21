//! A page loading in the background for the UI, and when another request for one counts as
//! already answered.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use super::MatrixEvent;

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
        load: impl Future<Output = MatrixEvent> + Send + 'static,
    ) -> Self {
        let answered = Arc::new(AtomicBool::new(false));
        let done = answered.clone();
        let task = tokio::spawn(async move {
            let answer = load.await;
            done.store(true, Ordering::Release);
            let _ = events.send(answer);
        });
        Self { task, answered }
    }

    /// Whether the page has yet to answer.
    pub(super) fn loading(&self) -> bool {
        !self.answered.load(Ordering::Acquire)
    }
}
