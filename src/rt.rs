//! The tasks and timers of the core: `spawn`, `JoinHandle`, `JoinSet`, `AbortHandle`,
//! sleeps, timeouts and intervals.
//!
//! On the terminal these are tokio's own items, re-exported or wrapped without a change of
//! behaviour, so nothing changes there. The browser has no tokio runtime: there they are
//! [`local`]'s, on `wasm_bindgen_futures::spawn_local` and gloo-timers (the glue at the end
//! of this file), with a `JoinHandle` and a `JoinSet` of our own (`local` says why).
//!
//! Deadlines are `crate::time::Instant`, the crate's one clock: there is no second `Instant`
//! to convert to and from (on the terminal it becomes tokio's with `From`, which is the same
//! clock).
//!
//! `spawn` asks for `SendOutsideWasm`, which is `Send` on the terminal (so tokio's bounds
//! stay) and nothing in the browser, whose futures are not `Send`.
//!
//! `spawn_blocking` is not here: it stays native-only, at its places
//! (`tokio::task::spawn_blocking`), as do the programs, sockets and signals of the
//! terminal's edges (audio, `textcmd`, `catchup`, `portal`, `watch`, `agent`).

use std::future::Future;
#[cfg(not(target_family = "wasm"))]
use std::future::IntoFuture;

use matrix_sdk::SendOutsideWasm;

#[cfg(not(target_family = "wasm"))]
use crate::time::Instant;

#[cfg(any(test, target_family = "wasm"))]
mod local;

#[cfg(not(target_family = "wasm"))]
pub use tokio::task::{AbortHandle, JoinError, JoinHandle, JoinSet};
#[cfg(not(target_family = "wasm"))]
pub use tokio::time::error::Elapsed;
#[cfg(not(target_family = "wasm"))]
pub use tokio::time::{MissedTickBehavior, interval, sleep, timeout};

#[cfg(target_family = "wasm")]
pub use browser::{JoinSet, interval, sleep, sleep_until, timeout, timeout_at};
#[cfg(target_family = "wasm")]
pub use local::{AbortHandle, Elapsed, JoinError, JoinHandle, MissedTickBehavior};

/// Runs `future` as a task of its own, at once.
#[cfg(not(target_family = "wasm"))]
#[track_caller]
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + SendOutsideWasm + 'static,
    F::Output: SendOutsideWasm + 'static,
{
    tokio::spawn(future)
}

/// Runs `future` as a task of its own, at once.
#[cfg(target_family = "wasm")]
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + SendOutsideWasm + 'static,
    F::Output: SendOutsideWasm + 'static,
{
    local::spawn::<browser::Browser, F>(future)
}

/// Ends at `deadline` (at once if it has passed).
#[cfg(not(target_family = "wasm"))]
pub fn sleep_until(deadline: Instant) -> tokio::time::Sleep {
    tokio::time::sleep_until(deadline.into())
}

/// `future`'s output, or `Elapsed` if it is not ready by `deadline`.
#[cfg(not(target_family = "wasm"))]
pub fn timeout_at<F: IntoFuture>(
    deadline: Instant,
    future: F,
) -> tokio::time::Timeout<F::IntoFuture> {
    tokio::time::timeout_at(deadline.into(), future)
}

/// The browser's half: [`local`] on the page's one thread and gloo's timers.
#[cfg(target_family = "wasm")]
mod browser {
    use std::future::{Future, IntoFuture};
    use std::time::Duration;

    use super::local::{self, Elapsed, Platform};
    use crate::time::Instant;

    /// The page's thread, `setTimeout` and `performance.now()`.
    pub enum Browser {}

    impl Platform for Browser {
        type Sleep = gloo_timers::future::TimeoutFuture;

        fn spawn_local(task: impl Future<Output = ()> + 'static) {
            wasm_bindgen_futures::spawn_local(task);
        }

        fn sleep(duration: Duration) -> Self::Sleep {
            gloo_timers::future::TimeoutFuture::new(local::timer_millis(duration))
        }

        fn now() -> Instant {
            Instant::now()
        }
    }

    pub type JoinSet<T> = local::JoinSet<T, Browser>;

    pub fn sleep(duration: Duration) -> local::Sleep<Browser> {
        local::sleep(duration)
    }

    pub fn sleep_until(deadline: Instant) -> local::Sleep<Browser> {
        local::sleep_until(deadline)
    }

    pub fn timeout<F: IntoFuture>(
        duration: Duration,
        future: F,
    ) -> impl Future<Output = Result<F::Output, Elapsed>> {
        local::timeout::<Browser, F>(duration, future)
    }

    pub fn timeout_at<F: IntoFuture>(
        deadline: Instant,
        future: F,
    ) -> impl Future<Output = Result<F::Output, Elapsed>> {
        local::timeout_at::<Browser, F>(deadline, future)
    }

    pub fn interval(period: Duration) -> local::Interval<Browser> {
        local::interval(period)
    }
}
