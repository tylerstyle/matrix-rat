//! The browser's tasks and timers, written for any executor that runs its tasks on one
//! thread, so that their logic is tested on the terminal too (the tests below run it on
//! tokio's `LocalSet` with a clock of their own).
//!
//! In the browser, [`Platform`] is `wasm_bindgen_futures::spawn_local` and gloo's timers
//! (`rt.rs`). Two things are ours because the obvious substitutes are wrong for the worker
//! (docs/WEB-PHASE-1.md, step 3):
//!
//! - [`JoinHandle::is_finished`] reads a flag the task sets as it ends, however it ends:
//!   with its output, aborted, panicked, or dropped by its executor. matrix-sdk-common's
//!   browser handle answers `is_aborted()`, so a task that ended normally never reads as
//!   finished (`matrix/paging.rs` and `matrix/media.rs` rely on it).
//! - [`JoinSet`] runs each task as soon as it is spawned, as tokio's does: the worker's sets
//!   only call `spawn`, `try_join_next` and `abort_all`, and a `FuturesUnordered`, which
//!   runs its futures only while `join_next` polls it, would leave theirs unpolled.

use std::fmt;
use std::future::{Future, IntoFuture, poll_fn};
use std::marker::PhantomData;
use std::panic::AssertUnwindSafe;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use futures::FutureExt;
use futures::future::{AbortHandle as Abort, Abortable, Aborted};
use futures::task::AtomicWaker;

use crate::time::Instant;

/// What runs the tasks and keeps the time: the browser's, or a test's.
pub trait Platform: 'static {
    /// What [`Platform::sleep`] makes; it counts from when it is made.
    type Sleep: Future<Output = ()> + Unpin + 'static;

    /// Runs `task` on this thread, from the next time the caller yields.
    fn spawn_local(task: impl Future<Output = ()> + 'static);

    /// Ends `duration` from now.
    fn sleep(duration: Duration) -> Self::Sleep;

    /// The time now, on the clock `sleep` keeps.
    fn now() -> Instant;
}

/// The longest timer the browser keeps: `setTimeout` holds its delay as an `i32`, and one
/// that does not fit ends at once.
const MAX_TIMER_MILLIS: u32 = i32::MAX.unsigned_abs();

/// `duration` in whole milliseconds for a browser timer: rounded up, so that a timer never
/// ends before the deadline it was made for, and at most [`MAX_TIMER_MILLIS`] (about 24.8
/// days; a longer wait ends then).
pub fn timer_millis(duration: Duration) -> u32 {
    let millis = duration.as_nanos().div_ceil(1_000_000);
    u32::try_from(millis).map_or(MAX_TIMER_MILLIS, |millis| millis.min(MAX_TIMER_MILLIS))
}

// --- Tasks ---------------------------------------------------------------------------

/// Why a task gave no output.
#[derive(Debug)]
pub struct JoinError {
    panicked: bool,
}

impl JoinError {
    const CANCELLED: Self = Self { panicked: false };
    const PANICKED: Self = Self { panicked: true };

    /// It was aborted, or dropped before it ended.
    pub fn is_cancelled(&self) -> bool {
        !self.panicked
    }

    /// It panicked.
    pub fn is_panic(&self) -> bool {
        self.panicked
    }
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.panicked {
            "task panicked"
        } else {
            "task was cancelled"
        })
    }
}

impl std::error::Error for JoinError {}

/// What a task shares with its handles, whatever its output.
struct Status {
    /// Set as the task ends, however it ends (see [`Ending`]).
    finished: AtomicBool,
    /// The [`JoinHandle`] waiting for the output.
    waker: AtomicWaker,
    /// The set the task is in, woken as it ends too (for [`JoinSet::join_next`]).
    set: Option<Arc<AtomicWaker>>,
    abort: Abort,
}

impl Status {
    fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

/// Where a task leaves its output (or why there is none).
type Slot<T> = Arc<Mutex<Option<Result<T, JoinError>>>>;

fn lock<T>(slot: &Mutex<T>) -> MutexGuard<'_, T> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Held by the task itself: dropped as the task ends, however it ends, it marks the task
/// finished and wakes who waits for it.
struct Ending<T> {
    status: Arc<Status>,
    output: Slot<T>,
}

impl<T> Drop for Ending<T> {
    fn drop(&mut self) {
        // Without an output the task did not get to its end: its executor dropped it.
        lock(&self.output).get_or_insert(Err(JoinError::CANCELLED));
        self.status.finished.store(true, Ordering::Release);
        self.status.waker.wake();
        if let Some(set) = &self.status.set {
            set.wake();
        }
    }
}

/// A task's handle: its output, awaited, and a way to stop it. Dropped, it lets the task
/// run on (as tokio's).
pub struct JoinHandle<T> {
    status: Arc<Status>,
    output: Slot<T>,
}

impl<T> JoinHandle<T> {
    /// Stops the task where it next waits (or before it starts). It reads as finished once
    /// it has stopped.
    pub fn abort(&self) {
        self.status.abort.abort();
    }

    /// Whether the task has ended: with its output, aborted, panicked or dropped.
    pub fn is_finished(&self) -> bool {
        self.status.is_finished()
    }

    /// A handle that can stop the task, and tell whether it has ended, without its output.
    pub fn abort_handle(&self) -> AbortHandle {
        AbortHandle {
            status: self.status.clone(),
        }
    }

    /// The output of a finished task, taken out.
    fn take(&self) -> Result<T, JoinError> {
        lock(&self.output)
            .take()
            .unwrap_or(Err(JoinError::CANCELLED))
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.is_finished() {
            self.status.waker.register(cx.waker());
            // It may have ended in between.
            if !self.is_finished() {
                return Poll::Pending;
            }
        }
        Poll::Ready(self.take())
    }
}

impl<T> fmt::Debug for JoinHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinHandle")
            .field("finished", &self.is_finished())
            .finish_non_exhaustive()
    }
}

/// Stops a task, and tells whether it has ended.
#[derive(Clone)]
pub struct AbortHandle {
    status: Arc<Status>,
}

impl AbortHandle {
    /// As [`JoinHandle::abort`].
    pub fn abort(&self) {
        self.status.abort.abort();
    }

    /// As [`JoinHandle::is_finished`].
    pub fn is_finished(&self) -> bool {
        self.status.is_finished()
    }
}

impl fmt::Debug for AbortHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AbortHandle")
            .field("finished", &self.is_finished())
            .finish_non_exhaustive()
    }
}

/// Runs `future` as a task of its own on `P`'s thread.
pub fn spawn<P: Platform, F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    spawn_in::<P, F>(future, None)
}

/// [`spawn`], waking `set` as well as the handle as the task ends.
fn spawn_in<P: Platform, F>(future: F, set: Option<Arc<AtomicWaker>>) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let (abort, registration) = Abort::new_pair();
    let status = Arc::new(Status {
        finished: AtomicBool::new(false),
        waker: AtomicWaker::new(),
        set,
        abort,
    });
    let output = Arc::new(Mutex::new(None));
    let ending = Ending {
        status: status.clone(),
        output: output.clone(),
    };
    P::spawn_local(async move {
        let ended = Abortable::new(AssertUnwindSafe(future).catch_unwind(), registration).await;
        let result = match ended {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_panic)) => Err(JoinError::PANICKED),
            Err(Aborted) => Err(JoinError::CANCELLED),
        };
        *lock(&ending.output) = Some(result);
        // `ending` is dropped here: the task reads as finished.
    });
    JoinHandle { status, output }
}

/// Tasks run together, each from the moment it is spawned, whose outputs are taken as they
/// end. Dropped, it aborts the tasks still in it (as tokio's).
pub struct JoinSet<T, P> {
    tasks: Vec<JoinHandle<T>>,
    /// Woken by each task of the set as it ends.
    waker: Arc<AtomicWaker>,
    platform: PhantomData<fn() -> P>,
}

impl<T, P> JoinSet<T, P> {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            waker: Arc::new(AtomicWaker::new()),
            platform: PhantomData,
        }
    }

    /// The tasks in the set, ended or not, whose output has not been taken.
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// The output of a task that has ended, taken out of the set, without waiting; `None`
    /// when none has.
    pub fn try_join_next(&mut self) -> Option<Result<T, JoinError>> {
        let at = self.tasks.iter().position(JoinHandle::is_finished)?;
        Some(self.tasks.swap_remove(at).take())
    }

    /// The output of the next task to end; `None` once the set is empty.
    pub async fn join_next(&mut self) -> Option<Result<T, JoinError>> {
        poll_fn(|cx| self.poll_join_next(cx)).await
    }

    fn poll_join_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<T, JoinError>>> {
        if self.tasks.is_empty() {
            return Poll::Ready(None);
        }
        if let Some(output) = self.try_join_next() {
            return Poll::Ready(Some(output));
        }
        self.waker.register(cx.waker());
        // One may have ended in between.
        match self.try_join_next() {
            Some(output) => Poll::Ready(Some(output)),
            None => Poll::Pending,
        }
    }

    /// Stops every task in the set. They stay in it until their outputs are taken.
    pub fn abort_all(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }

    /// Stops every task in the set and waits until they have all ended, emptying it.
    pub async fn shutdown(&mut self) {
        self.abort_all();
        while self.join_next().await.is_some() {}
    }
}

impl<T: 'static, P: Platform> JoinSet<T, P> {
    /// Runs `task` in the set, at once.
    pub fn spawn<F>(&mut self, task: F) -> AbortHandle
    where
        F: Future<Output = T> + 'static,
    {
        let handle = spawn_in::<P, F>(task, Some(self.waker.clone()));
        let abort = handle.abort_handle();
        self.tasks.push(handle);
        abort
    }
}

impl<T, P> Default for JoinSet<T, P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, P> Drop for JoinSet<T, P> {
    fn drop(&mut self) {
        self.abort_all();
    }
}

impl<T, P> fmt::Debug for JoinSet<T, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinSet")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

// --- Timers --------------------------------------------------------------------------

/// Ends after the time it was made for, counted from when it was made (as tokio's).
pub struct Sleep<P: Platform> {
    timer: P::Sleep,
}

impl<P: Platform> Future for Sleep<P> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        Pin::new(&mut self.timer).poll(cx)
    }
}

impl<P: Platform> fmt::Debug for Sleep<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sleep").finish_non_exhaustive()
    }
}

/// Ends `duration` from now.
pub fn sleep<P: Platform>(duration: Duration) -> Sleep<P> {
    Sleep {
        timer: P::sleep(duration),
    }
}

/// Ends at `deadline` (at once if it has passed).
pub fn sleep_until<P: Platform>(deadline: Instant) -> Sleep<P> {
    sleep(deadline.saturating_duration_since(P::now()))
}

/// What a timeout gives when the time ran out first.
#[derive(Debug, PartialEq, Eq)]
pub struct Elapsed(());

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("deadline has elapsed")
    }
}

impl std::error::Error for Elapsed {}

impl From<Elapsed> for std::io::Error {
    fn from(_: Elapsed) -> Self {
        std::io::ErrorKind::TimedOut.into()
    }
}

/// `future`'s output, or [`Elapsed`] if it is not ready within `duration` (counted from
/// now, not from the first poll, as tokio's).
pub fn timeout<P: Platform, F: IntoFuture>(
    duration: Duration,
    future: F,
) -> impl Future<Output = Result<F::Output, Elapsed>> {
    race(future.into_future(), sleep::<P>(duration))
}

/// `future`'s output, or [`Elapsed`] if it is not ready by `deadline`.
pub fn timeout_at<P: Platform, F: IntoFuture>(
    deadline: Instant,
    future: F,
) -> impl Future<Output = Result<F::Output, Elapsed>> {
    race(future.into_future(), sleep_until::<P>(deadline))
}

/// `future`'s output if it is ready before `timer` ends (looked at first, as tokio does).
async fn race<F: Future, P: Platform>(
    future: F,
    mut timer: Sleep<P>,
) -> Result<F::Output, Elapsed> {
    let mut future = pin!(future);
    poll_fn(move |cx| {
        if let Poll::Ready(output) = future.as_mut().poll(cx) {
            return Poll::Ready(Ok(output));
        }
        Pin::new(&mut timer).poll(cx).map(|()| Err(Elapsed(())))
    })
    .await
}

/// What an [`Interval`] does about the ticks it missed (it was not asked in time), as
/// tokio's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MissedTickBehavior {
    /// The missed ticks come at once, one after the other, then the beat goes on.
    #[default]
    Burst,
    /// The next tick is a period after the late one.
    Delay,
    /// The missed ticks are dropped: one tick now, the next on the beat.
    Skip,
}

impl MissedTickBehavior {
    /// When the tick after one due at `due` but taken at `now` is.
    fn next_tick(self, due: Instant, now: Instant, period: Duration) -> Instant {
        match self {
            Self::Burst => due + period,
            Self::Delay => now + period,
            Self::Skip => {
                let into_beat = (now - due).as_nanos() % period.as_nanos();
                now + period - u64::try_from(into_beat).map_or(Duration::ZERO, Duration::from_nanos)
            }
        }
    }
}

/// How late a tick may be taken and still count as on time (as tokio's).
const ON_TIME: Duration = Duration::from_millis(5);

/// Ticks every period, the first one at once.
pub struct Interval<P: Platform> {
    period: Duration,
    missed: MissedTickBehavior,
    /// When the next tick is due.
    next: Instant,
    /// The timer for `next`, made when it is first waited for.
    timer: Option<P::Sleep>,
}

/// An [`Interval`] of `period`, which must not be zero.
pub fn interval<P: Platform>(period: Duration) -> Interval<P> {
    assert!(!period.is_zero(), "`period` must be non-zero.");
    Interval {
        period,
        missed: MissedTickBehavior::default(),
        next: P::now(),
        timer: None,
    }
}

impl<P: Platform> Interval<P> {
    pub fn set_missed_tick_behavior(&mut self, missed: MissedTickBehavior) {
        self.missed = missed;
    }

    /// Ends at the next tick, with the time it was due. Dropped before, it loses nothing.
    pub async fn tick(&mut self) -> Instant {
        poll_fn(|cx| self.poll_tick(cx)).await
    }

    fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<Instant> {
        if P::now() < self.next {
            let next = self.next;
            let timer = self
                .timer
                .get_or_insert_with(|| P::sleep(next.saturating_duration_since(P::now())));
            ready!(Pin::new(timer).poll(cx));
        }
        self.timer = None;
        let due = self.next;
        let now = P::now();
        self.next = if now > due + ON_TIME {
            self.missed.next_tick(due, now, self.period)
        } else {
            due + self.period
        };
        Poll::Ready(due)
    }
}

impl<P: Platform> fmt::Debug for Interval<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Interval")
            .field("period", &self.period)
            .field("missed_tick_behavior", &self.missed)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::task::Waker;

    use futures::channel::oneshot;
    use tokio::task::LocalSet;

    use super::*;

    /// Tasks on tokio's `LocalSet`, and a clock that moves only when a test moves it.
    enum Virtual {}

    thread_local! {
        static START: Instant = Instant::now();
        static PASSED: Cell<Duration> = const { Cell::new(Duration::ZERO) };
        static SLEEPERS: RefCell<Vec<Waker>> = const { RefCell::new(Vec::new()) };
    }

    impl Platform for Virtual {
        type Sleep = VirtualSleep;

        fn spawn_local(task: impl Future<Output = ()> + 'static) {
            drop(tokio::task::spawn_local(task));
        }

        fn sleep(duration: Duration) -> VirtualSleep {
            VirtualSleep {
                at: Self::now() + duration,
            }
        }

        fn now() -> Instant {
            START.with(|start| *start) + PASSED.get()
        }
    }

    struct VirtualSleep {
        at: Instant,
    }

    impl Future for VirtualSleep {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if Virtual::now() >= self.at {
                return Poll::Ready(());
            }
            SLEEPERS.with_borrow_mut(|sleepers| sleepers.push(cx.waker().clone()));
            Poll::Pending
        }
    }

    /// Moves the clock on by `by`, and wakes the sleepers to look at it.
    fn advance(by: Duration) {
        PASSED.set(PASSED.get() + by);
        for sleeper in SLEEPERS.take() {
            sleeper.wake();
        }
    }

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    /// Lets the `LocalSet`'s tasks run as far as they can now.
    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    /// `test` on a `LocalSet`, given up (failing) if it hangs.
    async fn on_local_set<F: Future<Output = ()>>(test: F) {
        let executor = LocalSet::new();
        tokio::time::timeout(Duration::from_secs(10), executor.run_until(test))
            .await
            .expect("the test hung");
    }

    /// Notes when it is dropped, as a task's future is when the task ends.
    struct Dropped(Rc<Cell<bool>>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    /// A task that waits for ever, holding `dropped`'s note.
    fn waits_for_ever(dropped: &Rc<Cell<bool>>) -> impl Future<Output = ()> + 'static {
        let note = Dropped(dropped.clone());
        async move {
            let _note = note;
            std::future::pending::<()>().await;
        }
    }

    #[tokio::test]
    async fn a_task_reads_finished_once_it_has_ended_and_gives_its_output() {
        on_local_set(async {
            let (tell, told) = oneshot::channel::<u32>();
            let handle = spawn::<Virtual, _>(async move { told.await.unwrap_or(0) + 1 });
            settle().await;
            assert!(!handle.is_finished());
            let abort = handle.abort_handle();
            assert!(!abort.is_finished());
            tell.send(41).unwrap();
            settle().await;
            assert!(handle.is_finished(), "a normal end is finished too");
            assert!(abort.is_finished());
            assert_eq!(handle.await.unwrap(), 42);
        })
        .await;
    }

    #[tokio::test]
    async fn an_aborted_task_is_dropped_and_reads_finished_and_cancelled() {
        on_local_set(async {
            let dropped = Rc::new(Cell::new(false));
            let handle = spawn::<Virtual, _>(waits_for_ever(&dropped));
            settle().await;
            assert!(!handle.is_finished());
            handle.abort();
            settle().await;
            assert!(dropped.get(), "the task's future was dropped");
            assert!(handle.is_finished());
            let err = handle.await.unwrap_err();
            assert!(err.is_cancelled() && !err.is_panic());
            assert_eq!(err.to_string(), "task was cancelled");

            // The same through an AbortHandle.
            let dropped = Rc::new(Cell::new(false));
            let handle = spawn::<Virtual, _>(waits_for_ever(&dropped));
            settle().await;
            handle.abort_handle().abort();
            settle().await;
            assert!(dropped.get() && handle.is_finished());
        })
        .await;
    }

    #[tokio::test]
    async fn a_task_aborted_before_it_ran_never_runs() {
        on_local_set(async {
            let ran = Rc::new(Cell::new(false));
            let runs = ran.clone();
            let handle = spawn::<Virtual, _>(async move { runs.set(true) });
            handle.abort();
            settle().await;
            assert!(!ran.get());
            assert!(handle.is_finished());
            assert!(handle.await.unwrap_err().is_cancelled());
        })
        .await;
    }

    #[tokio::test]
    async fn a_task_that_panicked_reads_finished_and_panicked() {
        on_local_set(async {
            let handle: JoinHandle<()> = spawn::<Virtual, _>(async { panic!("on purpose") });
            settle().await;
            assert!(handle.is_finished(), "a panic is an end too");
            let err = handle.await.unwrap_err();
            assert!(err.is_panic() && !err.is_cancelled());
            assert_eq!(err.to_string(), "task panicked");
        })
        .await;
    }

    #[tokio::test]
    async fn a_task_its_executor_dropped_reads_finished_and_cancelled() {
        let executor = LocalSet::new();
        let entered = executor.enter();
        let handle = spawn::<Virtual, _>(std::future::pending::<()>());
        drop(entered);
        assert!(!handle.is_finished());
        drop(executor);
        assert!(
            handle.is_finished(),
            "the guard in the task marked it as it was dropped"
        );
        assert!(handle.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn a_set_runs_its_tasks_at_once_without_join_next() {
        on_local_set(async {
            let ran = Rc::new(Cell::new(0));
            let mut set = JoinSet::<u32, Virtual>::new();
            for n in 1..=3 {
                let ran = ran.clone();
                set.spawn(async move {
                    ran.set(ran.get() + 1);
                    n
                });
            }
            assert_eq!(set.len(), 3);
            settle().await;
            assert_eq!(ran.get(), 3, "each task ran, and nobody called join_next");
            let mut outputs: Vec<u32> = std::iter::from_fn(|| set.try_join_next())
                .map(Result::unwrap)
                .collect();
            outputs.sort_unstable();
            assert_eq!(outputs, [1, 2, 3]);
            assert!(set.is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn try_join_next_takes_only_what_has_ended_and_never_waits() {
        on_local_set(async {
            let (tell, told) = oneshot::channel::<()>();
            let mut set = JoinSet::<&str, Virtual>::new();
            let late = set.spawn(async move {
                let _ = told.await;
                "late"
            });
            set.spawn(async { "early" });
            assert!(set.try_join_next().is_none(), "nothing has run yet");
            settle().await;
            assert_eq!(set.try_join_next().unwrap().unwrap(), "early");
            assert!(set.try_join_next().is_none(), "the other one still runs");
            assert_eq!(set.len(), 1);
            assert!(!late.is_finished());
            tell.send(()).unwrap();
            settle().await;
            assert!(late.is_finished());
            assert_eq!(set.try_join_next().unwrap().unwrap(), "late");
            assert!(set.is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn join_next_waits_for_the_next_task_to_end() {
        on_local_set(async {
            let mut set = JoinSet::<u32, Virtual>::new();
            assert!(
                set.join_next().await.is_none(),
                "an empty set has none to wait for"
            );
            let (tell, told) = oneshot::channel::<u32>();
            set.spawn(async move { told.await.unwrap_or(0) });
            assert!(set.join_next().now_or_never().is_none());
            // Told by another task while join_next waits: the set's task ending wakes it.
            spawn::<Virtual, _>(async move { tell.send(7).unwrap() });
            assert_eq!(set.join_next().await.unwrap().unwrap(), 7);
            assert!(set.join_next().await.is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn abort_all_and_shutdown_stop_every_task() {
        on_local_set(async {
            let mut set = JoinSet::<(), Virtual>::new();
            let first = Rc::new(Cell::new(false));
            let second = Rc::new(Cell::new(false));
            set.spawn(waits_for_ever(&first));
            set.spawn(waits_for_ever(&second));
            settle().await;
            set.abort_all();
            assert_eq!(set.len(), 2, "aborted tasks stay until taken");
            while let Some(ended) = set.join_next().await {
                assert!(ended.unwrap_err().is_cancelled());
            }
            assert!(first.get() && second.get());

            let third = Rc::new(Cell::new(false));
            set.spawn(waits_for_ever(&third));
            settle().await;
            set.shutdown().await;
            assert!(set.is_empty() && third.get());
        })
        .await;
    }

    #[tokio::test]
    async fn dropping_a_set_aborts_its_tasks() {
        on_local_set(async {
            let dropped = Rc::new(Cell::new(false));
            let mut set = JoinSet::<(), Virtual>::new();
            let task = set.spawn(waits_for_ever(&dropped));
            settle().await;
            assert!(!task.is_finished());
            drop(set);
            settle().await;
            assert!(dropped.get());
            assert!(task.is_finished());
        })
        .await;
    }

    #[test]
    fn a_sleep_counts_from_when_it_was_made() {
        let mut nap = sleep::<Virtual>(ms(10));
        advance(ms(9));
        assert!((&mut nap).now_or_never().is_none());
        advance(ms(1));
        assert!((&mut nap).now_or_never().is_some());
        let mut passed = sleep_until::<Virtual>(Virtual::now() - ms(1));
        assert!(
            (&mut passed).now_or_never().is_some(),
            "a passed deadline ends at once"
        );
        let mut until = sleep_until::<Virtual>(Virtual::now() + ms(5));
        assert!((&mut until).now_or_never().is_none());
        advance(ms(5));
        assert!((&mut until).now_or_never().is_some());
    }

    #[test]
    fn a_timeout_gives_the_output_or_elapsed_at_its_deadline() {
        let quick = pin!(timeout::<Virtual, _>(ms(10), std::future::ready(5)));
        assert_eq!(quick.now_or_never(), Some(Ok(5)));

        let mut slow = pin!(timeout::<Virtual, _>(ms(10), std::future::pending::<()>()));
        assert_eq!(slow.as_mut().now_or_never(), None);
        advance(ms(9));
        assert_eq!(slow.as_mut().now_or_never(), None);
        advance(ms(1));
        assert_eq!(slow.as_mut().now_or_never(), Some(Err(Elapsed(()))));

        // Counted from when it was made, not from its first poll.
        let made = timeout::<Virtual, _>(ms(10), std::future::pending::<()>());
        advance(ms(10));
        assert_eq!(pin!(made).now_or_never(), Some(Err(Elapsed(()))));

        // The output is looked at first, even once the deadline has passed.
        let ready = pin!(timeout_at::<Virtual, _>(
            Virtual::now(),
            std::future::ready(1)
        ));
        assert_eq!(ready.now_or_never(), Some(Ok(1)));
        let mut at = pin!(timeout_at::<Virtual, _>(
            Virtual::now() + ms(3),
            std::future::pending::<()>()
        ));
        assert_eq!(at.as_mut().now_or_never(), None);
        advance(ms(3));
        let elapsed = at.as_mut().now_or_never().unwrap().unwrap_err();
        assert_eq!(elapsed.to_string(), "deadline has elapsed");
        assert_eq!(
            std::io::Error::from(elapsed).kind(),
            std::io::ErrorKind::TimedOut
        );
    }

    fn tick_now(interval: &mut Interval<Virtual>) -> Option<Instant> {
        pin!(interval.tick()).now_or_never()
    }

    #[test]
    fn an_interval_ticks_at_once_then_every_period() {
        let start = Virtual::now();
        let mut every = interval::<Virtual>(ms(10));
        assert_eq!(tick_now(&mut every), Some(start));
        assert_eq!(tick_now(&mut every), None);
        advance(ms(9));
        assert_eq!(tick_now(&mut every), None);
        advance(ms(1));
        assert_eq!(tick_now(&mut every), Some(start + ms(10)));
        // A little late is on time: the beat stays.
        advance(ms(13));
        assert_eq!(tick_now(&mut every), Some(start + ms(20)));
        advance(ms(7));
        assert_eq!(tick_now(&mut every), Some(start + ms(30)));
    }

    #[test]
    fn an_interval_that_skips_takes_one_missed_tick_then_keeps_to_the_beat() {
        let start = Virtual::now();
        let mut every = interval::<Virtual>(ms(10));
        every.set_missed_tick_behavior(MissedTickBehavior::Skip);
        assert_eq!(tick_now(&mut every), Some(start));
        advance(ms(35)); // The ticks at 10, 20 and 30 were missed.
        assert_eq!(
            tick_now(&mut every),
            Some(start + ms(10)),
            "one of them, at once"
        );
        assert_eq!(tick_now(&mut every), None, "and not the others");
        advance(ms(4));
        assert_eq!(tick_now(&mut every), None);
        advance(ms(1));
        assert_eq!(
            tick_now(&mut every),
            Some(start + ms(40)),
            "back on the beat"
        );
    }

    #[test]
    fn an_interval_that_delays_starts_a_new_beat_from_the_late_tick() {
        let start = Virtual::now();
        let mut every = interval::<Virtual>(ms(10));
        every.set_missed_tick_behavior(MissedTickBehavior::Delay);
        assert_eq!(tick_now(&mut every), Some(start));
        advance(ms(35));
        assert_eq!(tick_now(&mut every), Some(start + ms(10)));
        advance(ms(9));
        assert_eq!(tick_now(&mut every), None);
        advance(ms(1));
        assert_eq!(tick_now(&mut every), Some(start + ms(45)));
    }

    #[test]
    fn an_interval_that_bursts_takes_every_missed_tick_at_once() {
        let start = Virtual::now();
        let mut every = interval::<Virtual>(ms(10));
        assert_eq!(tick_now(&mut every), Some(start));
        advance(ms(35));
        assert_eq!(tick_now(&mut every), Some(start + ms(10)));
        assert_eq!(tick_now(&mut every), Some(start + ms(20)));
        assert_eq!(tick_now(&mut every), Some(start + ms(30)));
        assert_eq!(tick_now(&mut every), None);
    }

    #[test]
    fn a_browser_timer_is_whole_milliseconds_rounded_up_with_a_ceiling() {
        assert_eq!(timer_millis(Duration::ZERO), 0);
        assert_eq!(timer_millis(Duration::from_nanos(1)), 1);
        assert_eq!(timer_millis(Duration::from_micros(1500)), 2);
        assert_eq!(timer_millis(ms(250)), 250);
        // setTimeout keeps the delay as an i32: what does not fit would end at once.
        assert_eq!(
            timer_millis(Duration::from_secs(30 * 24 * 3600)),
            2_147_483_647
        );
        assert_eq!(timer_millis(Duration::MAX), 2_147_483_647);
    }
}
