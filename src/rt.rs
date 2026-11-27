//! The tasks and timers of the core: `spawn`, `JoinHandle`, `JoinSet`, sleeps, timeouts
//! and intervals.
//!
//! On the terminal these are tokio's own items, re-exported or wrapped without a change of
//! behaviour, so nothing changes there. Deadlines are `crate::time::Instant`, the crate's one
//! clock: there is no second `Instant` to convert to and from (on the terminal it becomes
//! tokio's with `From`, which is the same clock).
//!
//! `spawn` asks for `SendOutsideWasm`, which is `Send` on the terminal (so tokio's bounds
//! stay) and nothing in the browser, whose futures are not `Send`.
//!
//! `spawn_blocking` is not here: it stays native-only, at its places
//! (`tokio::task::spawn_blocking`), as do the programs, sockets and signals of the
//! terminal's edges (audio, `textcmd`, `catchup`, `portal`, `watch`, `agent`).

use std::future::{Future, IntoFuture};

use matrix_sdk::SendOutsideWasm;

use crate::time::Instant;

pub use tokio::task::{AbortHandle, JoinHandle, JoinSet};
pub use tokio::time::error::Elapsed;
pub use tokio::time::{MissedTickBehavior, interval, sleep, timeout};

/// Runs `future` as a task of its own, at once.
#[track_caller]
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + SendOutsideWasm + 'static,
    F::Output: SendOutsideWasm + 'static,
{
    tokio::spawn(future)
}

/// Ends at `deadline` (at once if it has passed).
pub fn sleep_until(deadline: Instant) -> tokio::time::Sleep {
    tokio::time::sleep_until(deadline.into())
}

/// `future`'s output, or `Elapsed` if it is not ready by `deadline`.
pub fn timeout_at<F: IntoFuture>(
    deadline: Instant,
    future: F,
) -> tokio::time::Timeout<F::IntoFuture> {
    tokio::time::timeout_at(deadline.into(), future)
}
