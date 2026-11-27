//! Search reaches back three months: the SDK's search backfill
//! (`EventCache::run_search_backfill`), which indexes what is stored once, then pages every
//! joined room back through the client's back-pagination queue, a week at a time; then
//! our own walk of each room, which takes it the rest of the way back to 13 weeks (see
//! [`deepen`] for why the SDK's sweep alone does not).
//!
//! It starts once a session, [`START_DELAY`] after the room list first caught up, if the
//! `search_history` setting is on. An outage during a run leaves rooms short (a request
//! that fails marks no room done, but its week is passed), so one catch-up run follows it.
//! The rules of when a run starts are [`Rules`], a plain value. The run is cancelled when
//! the setting goes off and at the end of the session (quit, logout, expiry): dropping the
//! SDK's future drops its handles, which cancels its requests, and dropping our walk lets
//! go of its room; a batch on its way completes in the SDK's own task, and nothing more
//! is asked.
//!
//! What is fetched reaches the search index a moment later, and what has not reached it
//! when the session ends is lost to it for good; [`Marks`] has the profile's next start
//! index the stored events again then.

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::time::Duration;

use chrono::{Datelike, Days, NaiveDate};
use futures::FutureExt;
use matrix_sdk::event_cache::{
    BackPaginationOutcome, EventCache, EventsOrigin, RoomEventCacheUpdate, RoomPagination,
    SearchBackfillStrategy, Subscriber,
};
use matrix_sdk::ruma::{MilliSecondsSinceUnixEpoch, RoomId};
use matrix_sdk::{Client, RoomState};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::MatrixEvent;
use crate::rt;

/// From the room list's first catch-up to the start: the room list, the previews and the
/// open room's first page go first, and the latest-event and read-receipt backfills, which
/// share the queue, get a head start. The once-only reindex reads the whole store.
#[cfg(not(test))]
const START_DELAY: Duration = Duration::from_secs(10);
#[cfg(test)]
const START_DELAY: Duration = Duration::ZERO;

/// Our run gives up after this. A panic in one of the queue's runs never reports its end,
/// and the SDK's sweep would wait for it for ever.
const TIME_LIMIT: Duration = Duration::from_secs(6 * 60 * 60);

/// How far the SDK's sweep and our walk go back: 13 weeks.
const BACK_DAYS: u64 = 13 * 7;
const BACK: Days = Days::new(BACK_DAYS);

/// Our walk of one room: batches of 30 events, at most 130 of them (those read back
/// from the store included), what the SDK's sweep would take in 13 weeks.
const BATCH: u16 = 30;
const MAX_BATCHES: usize = 130;

/// Between the rooms of our walk, as the SDK's `Foreground` sweep waits between rooms.
#[cfg(not(test))]
const ROOM_PACING: Duration = Duration::from_secs(1);
#[cfg(test)]
const ROOM_PACING: Duration = Duration::ZERO;

/// After a run that fetched, how long the search index is given to take it all in before
/// [`Marks`] counts it as in. On the dev server it took 7 s after a run of 9 000 events.
const SETTLE: Duration = Duration::from_secs(30);

/// How often a batch that is loading is looked at again, for a panic another task took
/// (as in `paging::backwards`).
const LOOK_AGAIN: Duration = Duration::from_secs(1);

/// The most our walk rests after a batch: half as long as the batch took, up to this.
const MAX_REST: Duration = Duration::from_secs(1);

/// How far the fetch of older messages has got, for the search box.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum HistoryRun {
    /// The setting is off, or there is no session.
    #[default]
    Off,
    /// On, but not fetching yet: shortly after the start, or until the connection is
    /// back.
    Waiting,
    /// Fetching, back to this date ("28 Aug", see [`short_date`]).
    Running { back_to: String },
    /// Fetched, back to about this date.
    Done { back_to: String },
    /// Given up (the time limit): what came so far is found; the next start goes on.
    Stopped,
}

impl HistoryRun {
    /// Older messages are still on their way.
    pub fn fetching(&self) -> bool {
        matches!(self, Self::Waiting | Self::Running { .. })
    }
}

/// `when` for the search box, short: "28 Aug" within the year of `now`, "11 Nov 2026"
/// when it lies in another.
pub(crate) fn short_date(when: NaiveDate, now: NaiveDate) -> String {
    if when.year() == now.year() {
        when.format("%-d %b").to_string()
    } else {
        when.format("%-d %b %Y").to_string()
    }
}

/// The date a run that starts `today` reaches back to, as the box shows it.
fn back_to(today: NaiveDate) -> String {
    short_date(today.checked_sub_days(BACK).unwrap_or(today), today)
}

/// What changes the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Input {
    /// The setting, switched (or sent again).
    Setting(bool),
    /// The room list caught up for the first time in this session.
    CaughtUp,
    /// The sync says it is offline.
    Offline,
    /// The sync is through again after it was offline.
    Online,
    /// The run ended. `finished`: it went through (not stopped at the time limit, nor
    /// for want of an event cache or a queue).
    Ended { finished: bool },
}

/// What to do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Nothing,
    /// Start a run. `delay`: wait [`START_DELAY`] first (the first start after the room
    /// list caught up); `catch_up`: it follows a run that saw an outage.
    Start {
        delay: bool,
        catch_up: bool,
    },
    /// Stop the run that is going.
    Stop,
}

/// When a run starts, as a plain value of what the session knows: never two at once, the
/// first after the room list caught up, and one catch-up per outage at most.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Rules {
    /// The setting.
    enabled: bool,
    /// The room list has caught up in this session.
    caught_up: bool,
    /// The sync said it is offline, and has not been through since.
    offline: bool,
    /// A run is going.
    running: bool,
    /// The sync was offline while a run went; kept after it ended, for the catch-up.
    saw_offline: bool,
    /// A run went through in this session without an outage: no more runs.
    done: bool,
    /// A run stopped at the time limit: none again until the setting is switched.
    halted: bool,
}

impl Rules {
    fn on(&mut self, input: Input) -> Step {
        match input {
            Input::Setting(on) => {
                self.enabled = on;
                self.halted = false;
                if !on {
                    // Off clears it all; a run stopped so has not finished.
                    self.saw_offline = false;
                    if self.running {
                        self.running = false;
                        return Step::Stop;
                    }
                    return Step::Nothing;
                }
            }
            Input::CaughtUp => self.caught_up = true,
            Input::Offline => {
                self.offline = true;
                if self.running {
                    self.saw_offline = true;
                }
            }
            Input::Online => self.offline = false,
            Input::Ended { finished } => {
                self.running = false;
                if !finished {
                    // No catch-up after it: the next run is the user's, by the setting.
                    self.halted = true;
                    self.saw_offline = false;
                } else if !self.saw_offline {
                    self.done = true;
                }
            }
        }
        if self.enabled
            && self.caught_up
            && !self.offline
            && !self.running
            && !self.done
            && !self.halted
        {
            let catch_up = self.saw_offline;
            self.running = true;
            self.saw_offline = false;
            return Step::Start {
                delay: input == Input::CaughtUp,
                catch_up,
            };
        }
        Step::Nothing
    }
}

/// What a run tells the session loop.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Report {
    /// The run `run` sent its first request: back to this date.
    Begun { run: u64, back_to: String },
    /// The run `run` ended; see [`Input::Ended`].
    Ended { run: u64, finished: bool },
}

/// Sends the end of a run when dropped: also when it panicked, or was cancelled (the loop
/// has forgotten that run by then).
struct EndReport {
    reports: UnboundedSender<Report>,
    run: u64,
    finished: bool,
}

impl Drop for EndReport {
    fn drop(&mut self) {
        let _ = self.reports.send(Report::Ended {
            run: self.run,
            finished: self.finished,
        });
    }
}

/// The search history of one session: the rules, the run going, and what the search box
/// was told.
pub(super) struct SearchHistory {
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    rules: Rules,
    /// The number of the run going; a report of another is late and ignored.
    run: u64,
    task: Option<rt::JoinHandle<()>>,
    /// The date the run going (or the last one) reaches back to, once it has begun.
    back_to: Option<String>,
    reports: UnboundedSender<Report>,
    reports_rx: UnboundedReceiver<Report>,
    shown: Option<HistoryRun>,
    /// The search index's folder, for [`Marks`]; `None`: no marks.
    index_dir: Option<PathBuf>,
    /// A run of this session has looked for what an earlier session left unindexed.
    looked_for_leftovers: bool,
    /// See [`SETTLE`].
    settle: Duration,
}

impl SearchHistory {
    /// `enabled`: the setting. `index_dir`: the search index's folder. Nothing runs before
    /// [`SearchHistory::caught_up`].
    pub(super) fn new(
        client: Client,
        events: UnboundedSender<MatrixEvent>,
        enabled: bool,
        index_dir: Option<PathBuf>,
    ) -> Self {
        let (reports, reports_rx) = unbounded_channel();
        let mut history = Self {
            client,
            events,
            rules: Rules::default(),
            run: 0,
            task: None,
            back_to: None,
            reports,
            reports_rx,
            shown: None,
            index_dir,
            looked_for_leftovers: false,
            settle: SETTLE,
        };
        // The browser keeps no index and has no queue: nothing to fetch for.
        history.apply(Input::Setting(enabled && cfg!(not(target_family = "wasm"))));
        history
    }

    pub(super) fn set_enabled(&mut self, enabled: bool) {
        self.apply(Input::Setting(enabled && cfg!(not(target_family = "wasm"))));
    }

    /// The room list caught up for the first time in this session.
    pub(super) fn caught_up(&mut self) {
        self.apply(Input::CaughtUp);
    }

    pub(super) fn offline(&mut self) {
        self.apply(Input::Offline);
    }

    /// The sync is through again after it was offline.
    pub(super) fn online(&mut self) {
        self.apply(Input::Online);
    }

    /// The next report of a run, for the session loop to hand back to
    /// [`SearchHistory::on_report`]. Cancel-safe.
    pub(super) async fn next(&mut self) -> Report {
        match self.reports_rx.recv().await {
            Some(report) => report,
            // Never: `self.reports` is a sender.
            None => std::future::pending().await,
        }
    }

    pub(super) fn on_report(&mut self, report: Report) {
        match report {
            Report::Begun { run, back_to } if run == self.run && self.rules.running => {
                self.back_to = Some(back_to);
                self.show();
            }
            Report::Ended { run, finished } if run == self.run && self.rules.running => {
                // The task is kept: it may be waiting for the index to settle, which the
                // end of the session cancels (see `Marks`).
                self.apply(Input::Ended { finished });
            }
            // Of a run that was stopped.
            Report::Begun { .. } | Report::Ended { .. } => {}
        }
    }

    /// Cancels the run going, at the end of the session.
    pub(super) fn stop(&mut self) {
        // Its report, if one comes, is of no run any more.
        self.run += 1;
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    fn apply(&mut self, input: Input) {
        match self.rules.on(input) {
            Step::Nothing => {}
            Step::Start { delay, catch_up } => {
                if catch_up {
                    tracing::info!("search history catches up after an outage");
                }
                self.stop();
                self.back_to = None;
                let ended = EndReport {
                    reports: self.reports.clone(),
                    run: self.run,
                    finished: false,
                };
                let how = How {
                    delay,
                    leftovers: !std::mem::replace(&mut self.looked_for_leftovers, true),
                    settle: self.settle,
                };
                let marks = Marks::new(self.index_dir.clone());
                self.task = Some(rt::spawn(run(self.client.clone(), how, marks, ended)));
            }
            Step::Stop => self.stop(),
        }
        self.show();
    }

    /// Tells the app what the box should say, when that changed.
    fn show(&mut self) {
        let rules = &self.rules;
        let now = if !rules.enabled {
            HistoryRun::Off
        } else if rules.running {
            match &self.back_to {
                Some(back_to) => HistoryRun::Running {
                    back_to: back_to.clone(),
                },
                None => HistoryRun::Waiting,
            }
        } else if rules.done {
            HistoryRun::Done {
                back_to: self
                    .back_to
                    .clone()
                    .unwrap_or_else(|| back_to(chrono::Local::now().date_naive())),
            }
        } else if rules.halted {
            HistoryRun::Stopped
        } else {
            HistoryRun::Waiting
        };
        if self.shown.as_ref() != Some(&now) {
            self.shown = Some(now.clone());
            let _ = self.events.send(MatrixEvent::SearchHistory(now));
        }
    }
}

impl Drop for SearchHistory {
    fn drop(&mut self) {
        self.stop();
    }
}

/// How a run goes: `delay`: [`START_DELAY`] first; `leftovers`: look for what an earlier
/// session left unindexed (the session's first run does); `settle`: see [`SETTLE`].
struct How {
    delay: bool,
    leftovers: bool,
    settle: Duration,
}

/// Small files of ours in the search index's folder, beside the SDK's `reindexed-v1`
/// (written once the SDK has indexed the stored events, after which it never does again).
/// The folder goes at logout, and with it these; `search::prune_search_dir` leaves files
/// alone. Without a folder (tests, the browser) there are none.
///
/// - `history-indexing`: a run fetched, and the index may not have all of it yet. The SDK
///   indexes what is fetched in a task of its own, from a queue in memory, a batch at a
///   time; what is queued when the session ends (quit, logout, a crash) is lost to the
///   index, though it is stored. Written before the first fetch, removed [`SETTLE`] after
///   the run. Found at a start, it has the SDK index the stored events again (by removing
///   `reindexed-v1`): CPU only, and what is in the index already is skipped.
struct Marks {
    dir: Option<PathBuf>,
    /// This run wrote `history-indexing`.
    indexing: bool,
}

/// The SDK's own (`matrix_sdk::search_index`, `REINDEX_MARKER`; matrix-sdk is pinned).
const REINDEXED: &str = "reindexed-v1";
const INDEXING: &str = "history-indexing";

impl Marks {
    fn new(dir: Option<PathBuf>) -> Self {
        Self {
            dir,
            indexing: false,
        }
    }

    fn has(&self, name: &str) -> bool {
        #[cfg(not(target_family = "wasm"))]
        if let Some(dir) = &self.dir {
            return dir.join(name).exists();
        }
        let _ = (&self.dir, name);
        false
    }

    fn put(&self, name: &str) {
        #[cfg(not(target_family = "wasm"))]
        if let Some(dir) = &self.dir
            && let Err(err) =
                std::fs::create_dir_all(dir).and_then(|()| std::fs::write(dir.join(name), b""))
        {
            tracing::warn!(why = %crate::logsafe::kind(&err), "search history: could not write a mark");
        }
        let _ = (&self.dir, name);
    }

    fn remove(&self, name: &str) {
        #[cfg(not(target_family = "wasm"))]
        if let Some(dir) = &self.dir
            && let Err(err) = std::fs::remove_file(dir.join(name))
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(why = %crate::logsafe::kind(&err), "search history: could not remove a mark");
        }
        let _ = (&self.dir, name);
    }

    /// An earlier session ended before the index had all a run fetched: the SDK is to
    /// index the stored events again.
    fn catch_up_on_leftovers(&self) {
        if self.has(INDEXING) {
            tracing::info!("search history: the index missed some of the last run; it is redone");
            self.remove(REINDEXED);
        }
    }

    /// Before anything is fetched that the index has yet to take in.
    fn fetching(&mut self) {
        if !self.indexing {
            self.indexing = true;
            self.put(INDEXING);
        }
    }
}

/// One run: wait, then the SDK's sweep and our walk, under the time limit.
/// `ended` says how it ended when it is dropped, also when the task is cancelled or
/// panics; it is sent as soon as the run is through, before the index settles.
async fn run(client: Client, how: How, mut marks: Marks, mut ended: EndReport) {
    if how.delay {
        rt::sleep(START_DELAY).await;
    }
    let Some(cache) = super::cache::of(&client) else {
        return;
    };
    // Without it `run_search_backfill` returns at once, having fetched nothing.
    if cache.back_pagination_queue().is_none() {
        tracing::warn!("search history: the client has no back-pagination queue");
        return;
    }
    if how.leftovers {
        marks.catch_up_on_leftovers();
    }
    // A mark of this session's last run, stopped before the index settled: this run takes
    // it over, and removes it once it has settled in turn.
    marks.indexing = marks.has(INDEXING);
    let rooms = client.joined_rooms().len();
    tracing::info!(rooms, "search history started");
    let _ = ended.reports.send(Report::Begun {
        run: ended.run,
        back_to: back_to(chrono::Local::now().date_naive()),
    });
    let started = crate::time::Instant::now();
    let work = async {
        marks.fetching();
        cache
            .run_search_backfill(SearchBackfillStrategy::Foreground)
            .await;
        deepen(&client, cache, &mut marks).await
    };
    match rt::timeout(TIME_LIMIT, work).await {
        Ok(true) => {
            let elapsed_s = started.elapsed().as_secs();
            tracing::info!(elapsed_s, "search history done");
            ended.finished = true;
        }
        // Stopped, as at the time limit: the setting may try again.
        Ok(false) => tracing::warn!("search history stopped: the SDK failed"),
        Err(_) => tracing::warn!("search history stopped at the time limit"),
    }
    // The box says so now; the index may still be taking in the last batches. The client
    // goes, so a session that ends meanwhile is not held up (and this task is cancelled).
    drop(ended);
    drop(client);
    if marks.indexing {
        rt::sleep(how.settle).await;
        marks.remove(INDEXING);
    }
}

/// Takes every room the rest of the way back, one room at a time.
///
/// The SDK's sweep does not get there in a room with more than about 300 events (seen on
/// the dev homeserver with 0.20.0, see docs/DESIGN-1.1.md): each week's request starts
/// from the room's newest events again, since the room shrinks back to its last chunk
/// in memory once nobody holds it, and reads back what is stored before it asks the
/// server. Its cap of 10 batches a room and week counts those, so once 10 are stored the
/// request ends there, every week and every start after. (The SDK's own test of it holds
/// the room, which hides this.)
///
/// Here each room is held while it is walked, so every batch goes on from the last: back
/// to 13 weeks, the room's start, or [`MAX_BATCHES`]. One room at a time, so at most one
/// room's older events are in memory; the room shrinks back when it is let go. False if
/// the SDK panicked and the walk ended there.
async fn deepen(client: &Client, cache: &EventCache, marks: &mut Marks) -> bool {
    let back_ms = BACK_DAYS * 24 * 60 * 60 * 1000;
    let oldest = u64::from(MilliSecondsSinceUnixEpoch::now().get()).saturating_sub(back_ms);
    let mut rooms = client.joined_rooms();
    rooms.retain(|room| !room.is_space());
    rooms.sort_by_key(|room| std::cmp::Reverse(room.recency_stamp()));
    for (n, room) in rooms.iter().enumerate() {
        // Left during the run (as the SDK's queue checks too).
        if room.state() != RoomState::Joined {
            continue;
        }
        if n > 0 {
            rt::sleep(ROOM_PACING).await;
        }
        if let Walked::Panicked = walk(cache, room.room_id(), oldest, marks).await {
            return false;
        }
    }
    true
}

/// How the walk of a room went.
enum Walked {
    /// It asked the server (or may have).
    Fetched,
    /// It read the store alone.
    Stored,
    /// The SDK panicked: the walk had better end.
    Panicked,
}

/// Walks one room back to `oldest` (ms since the epoch).
async fn walk(cache: &EventCache, room_id: &RoomId, oldest: u64, marks: &mut Marks) -> Walked {
    let Ok((room, _handles)) = cache.room(room_id).await else {
        return Walked::Stored;
    };
    // Held while it is walked: the room keeps what was loaded in memory. What it hears
    // tells a batch from the server from one read back from the store.
    let Ok((_events, mut held)) = room.subscribe().await else {
        return Walked::Stored;
    };
    let mut walked = Walked::Stored;
    let pagination = room.pagination();
    let mut rest = Duration::ZERO;
    for n in 0..MAX_BATCHES {
        // It rests half as long as the last batch took: next to nothing after one read
        // back from the store, more after a slow server. The server, the app's own
        // requests (ours does not go through the SDK's queue) and the search index, which
        // takes what is fetched one batch at a time, get a share of the time.
        if n > 0 {
            rt::sleep(rest).await;
        }
        let started = crate::time::Instant::now();
        let answer = page(&pagination).await;
        if from_the_server(&mut held) {
            walked = Walked::Fetched;
            marks.fetching();
        }
        let outcome = match answer {
            Ok(Some(outcome)) => outcome,
            // An error: this room is left as it is.
            Ok(None) => return walked,
            Err(()) => return Walked::Panicked,
        };
        // Newest first, so the oldest is last.
        let reached = outcome
            .events
            .iter()
            .rev()
            .find_map(|event| event.timestamp())
            .is_some_and(|ts| u64::from(ts.get()) <= oldest);
        if outcome.reached_start || outcome.events.is_empty() || reached {
            return walked;
        }
        rest = (started.elapsed() / 2).min(MAX_REST);
    }
    walked
}

/// Whether the room heard of events from the server's pages since it was last asked (or
/// heard too much to tell). A page of the SDK's tells the room before it answers.
fn from_the_server(held: &mut Subscriber<RoomEventCacheUpdate>) -> bool {
    use tokio::sync::broadcast::error::TryRecvError;
    let mut fetched = false;
    loop {
        match held.try_recv() {
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents(diffs)) => {
                fetched |= matches!(diffs.origin, EventsOrigin::Pagination);
            }
            Ok(_) => {}
            Err(TryRecvError::Lagged(_)) => fetched = true,
            Err(TryRecvError::Empty | TryRecvError::Closed) => return fetched,
        }
    }
}

/// One batch of the room's older events; `Ok(None)` on an error, `Err` if the SDK
/// panicked (matrix-rust-sdk #5416, see `paging::backwards`), looked at again every
/// [`LOOK_AGAIN`] while it loads. Nothing of the error or the panic is kept. Unlike
/// `paging::backwards` it does not clear the event caches: from the background that would
/// empty the open room's timeline under the user; the user's own paging of that room
/// clears them if the panic comes back there.
async fn page(pagination: &RoomPagination) -> Result<Option<BackPaginationOutcome>, ()> {
    let batch = AssertUnwindSafe(pagination.run_backwards_once(BATCH)).catch_unwind();
    futures::pin_mut!(batch);
    let answer = loop {
        tokio::select! {
            biased;
            answer = &mut batch => break answer,
            // Only so that `batch` is polled again.
            () = rt::sleep(LOOK_AGAIN) => {}
        }
    };
    match answer {
        Ok(Ok(outcome)) => Ok(Some(outcome)),
        Ok(Err(_)) => Ok(None),
        Err(_) => {
            tracing::warn!("search history: the SDK panicked while loading older messages");
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn short_date_says_the_year_only_when_it_is_another() {
        assert_eq!(short_date(date(2026, 8, 28), date(2026, 11, 27)), "28 Aug");
        assert_eq!(
            short_date(date(2026, 11, 11), date(2027, 2, 10)),
            "11 Nov 2026"
        );
        assert_eq!(
            short_date(date(2026, 12, 31), date(2027, 1, 1)),
            "31 Dec 2026"
        );
        assert_eq!(short_date(date(2027, 1, 1), date(2027, 1, 1)), "1 Jan");
        // 13 weeks back.
        assert_eq!(back_to(date(2026, 11, 27)), "28 Aug");
        assert_eq!(back_to(date(2027, 2, 10)), "11 Nov 2026");
        // From a day in February of a leap year: still in the last one.
        assert_eq!(back_to(date(2028, 2, 29)), "30 Nov 2027");
        // From April on, the same year.
        assert_eq!(back_to(date(2027, 4, 1)), "31 Dec 2026");
        assert_eq!(back_to(date(2027, 4, 2)), "1 Jan");
    }

    fn started(delay: bool, catch_up: bool) -> Step {
        Step::Start { delay, catch_up }
    }

    /// The setting on, the room list in, the first run going.
    fn running() -> Rules {
        let mut rules = Rules::default();
        assert_eq!(rules.on(Input::Setting(true)), Step::Nothing);
        assert_eq!(rules.on(Input::CaughtUp), started(true, false));
        rules
    }

    #[test]
    fn the_first_run_starts_once_the_room_list_is_in_and_only_once() {
        let mut rules = Rules::default();
        assert_eq!(rules.on(Input::Setting(true)), Step::Nothing);
        assert_eq!(rules.on(Input::Online), Step::Nothing);
        // After the start delay, the first time only.
        assert_eq!(rules.on(Input::CaughtUp), started(true, false));
        // Never a second run while one goes.
        assert_eq!(rules.on(Input::Setting(true)), Step::Nothing);
        assert_eq!(rules.on(Input::Online), Step::Nothing);
        // No outage, no catch-up: done for the session.
        assert_eq!(rules.on(Input::Ended { finished: true }), Step::Nothing);
        assert!(rules.done);
        assert_eq!(rules.on(Input::Offline), Step::Nothing);
        assert_eq!(rules.on(Input::Online), Step::Nothing);
        // On after Done starts nothing.
        assert_eq!(rules.on(Input::Setting(true)), Step::Nothing);
    }

    #[test]
    fn off_at_the_start_and_on_later_fetches_at_once() {
        let mut rules = Rules::default();
        assert_eq!(rules.on(Input::Setting(false)), Step::Nothing);
        assert_eq!(rules.on(Input::CaughtUp), Step::Nothing);
        assert_eq!(rules.on(Input::Setting(true)), started(false, false));
        // Before the room list is in, it waits for it.
        let mut rules = Rules::default();
        assert_eq!(rules.on(Input::Setting(true)), Step::Nothing);
        assert_eq!(rules.on(Input::Setting(true)), Step::Nothing);
        assert_eq!(rules.on(Input::CaughtUp), started(true, false));
    }

    #[test]
    fn an_outage_during_a_run_brings_one_catch_up_when_it_ends_online() {
        let mut rules = running();
        assert_eq!(rules.on(Input::Offline), Step::Nothing);
        assert_eq!(rules.on(Input::Online), Step::Nothing);
        // The sync is up when the run ends: at once.
        assert_eq!(
            rules.on(Input::Ended { finished: true }),
            started(false, true)
        );
        // The catch-up sees no outage: that is the last.
        assert_eq!(rules.on(Input::Ended { finished: true }), Step::Nothing);
        assert!(rules.done);
    }

    #[test]
    fn an_outage_that_lasts_past_the_run_brings_the_catch_up_when_it_is_over() {
        let mut rules = running();
        assert_eq!(rules.on(Input::Offline), Step::Nothing);
        // Still offline when the run ends: not yet.
        assert_eq!(rules.on(Input::Ended { finished: true }), Step::Nothing);
        assert!(!rules.done);
        assert_eq!(rules.on(Input::Offline), Step::Nothing);
        // Offline → Running: at once, without the start delay.
        assert_eq!(rules.on(Input::Online), started(false, true));
        assert_eq!(rules.on(Input::Online), Step::Nothing);
        // A catch-up is caught up only if it too sees an outage.
        assert_eq!(rules.on(Input::Offline), Step::Nothing);
        assert_eq!(rules.on(Input::Online), Step::Nothing);
        assert_eq!(
            rules.on(Input::Ended { finished: true }),
            started(false, true)
        );
        assert_eq!(rules.on(Input::Ended { finished: true }), Step::Nothing);
        assert!(rules.done);
    }

    #[test]
    fn an_outage_while_no_run_goes_brings_nothing() {
        let mut rules = Rules::default();
        assert_eq!(rules.on(Input::Setting(false)), Step::Nothing);
        assert_eq!(rules.on(Input::CaughtUp), Step::Nothing);
        assert_eq!(rules.on(Input::Offline), Step::Nothing);
        assert_eq!(rules.on(Input::Online), Step::Nothing);
        assert!(!rules.saw_offline);
        // Switched on while offline, it waits for the connection.
        assert_eq!(rules.on(Input::Offline), Step::Nothing);
        assert_eq!(rules.on(Input::Setting(true)), Step::Nothing);
        assert_eq!(rules.on(Input::Online), started(false, false));
    }

    #[test]
    fn off_stops_the_run_and_clears_the_outage() {
        let mut rules = running();
        assert_eq!(rules.on(Input::Offline), Step::Nothing);
        assert_eq!(rules.on(Input::Setting(false)), Step::Stop);
        assert_eq!(rules.on(Input::Setting(false)), Step::Nothing);
        assert!(!rules.running && !rules.saw_offline && !rules.done);
        assert_eq!(rules.on(Input::Online), Step::Nothing);
        // A run stopped by the setting has not finished: on starts one, not a catch-up.
        assert_eq!(rules.on(Input::Setting(true)), started(false, false));
    }

    #[test]
    fn a_run_stopped_at_the_time_limit_is_not_started_again_by_itself() {
        let mut rules = running();
        assert_eq!(rules.on(Input::Offline), Step::Nothing);
        assert_eq!(rules.on(Input::Online), Step::Nothing);
        assert_eq!(rules.on(Input::Ended { finished: false }), Step::Nothing);
        assert_eq!(rules.on(Input::Offline), Step::Nothing);
        assert_eq!(rules.on(Input::Online), Step::Nothing);
        // The user may try again.
        assert_eq!(rules.on(Input::Setting(true)), started(false, false));
    }

    /// Against the fake homeserver, with a client built as the app builds it.
    mod fake_server {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::Duration;

        use matrix_sdk::Client;
        use matrix_sdk::config::SyncSettings;
        use serde_json::{Value, json};
        use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

        use super::super::{HistoryRun, INDEXING, REINDEXED, Report, SearchHistory};
        use crate::matrix::spaces::fake::{self, Reply};
        use crate::matrix::{MatrixEvent, SecretStoreMode, WorkerOptions};

        const ROOM: &str = "!history:localhost";
        const SOMEONE: &str = "@bob:localhost";

        fn now_ms() -> u64 {
            chrono::Utc::now().timestamp_millis() as u64
        }

        fn message(id: &str, sender: &str) -> Value {
            json!({
                "type": "m.room.message",
                "content": { "msgtype": "m.text", "body": "hello there" },
                "sender": sender,
                "event_id": id,
                "origin_server_ts": now_ms(),
            })
        }

        /// A joined room whose sync is cut short: older messages are on the server.
        /// `ephemeral`: the room's receipts and the like.
        fn room(timeline: Vec<Value>, ephemeral: Vec<Value>) -> Value {
            let mut room = fake::joined(false, vec![]);
            room["timeline"] = json!({
                "events": timeline,
                "limited": true,
                "prev_batch": "before-sync",
            });
            room["ephemeral"] = json!({ "events": ephemeral });
            room
        }

        fn rooms(timeline: Vec<Value>, ephemeral: Vec<Value>) -> Value {
            json!({ "join": { ROOM: room(timeline, ephemeral) } })
        }

        /// Three rooms, each with a message of ours as its latest.
        fn three_rooms() -> Value {
            let join: serde_json::Map<String, Value> = ["a", "b", "c"]
                .into_iter()
                .map(|name| {
                    let own = message(&format!("$own-{name}"), fake::ME);
                    (format!("!{name}:localhost"), room(vec![own], vec![]))
                })
                .collect();
            json!({ "join": join })
        }

        /// A client signed in as `fake::ME`, synced once, with the back-pagination queue
        /// as `options` say (see `session::offline_client`).
        async fn signed_in(server: &fake::Server, options: WorkerOptions) -> Client {
            let client = Client::builder()
                .homeserver_url(&server.url)
                .with_enable_automatic_back_pagination(options.back_pagination)
                .build()
                .await
                .unwrap();
            restored(client).await
        }

        /// A client as the app builds it, with the queue, its stores and search index on
        /// disk in `paths`; signed in and synced once.
        async fn on_disk(server: &fake::Server, paths: &crate::paths::Paths) -> Client {
            let client =
                crate::matrix::session::offline_client(paths, &server.url, "fake-secret", true)
                    .await
                    .unwrap();
            restored(client).await
        }

        /// `client` signed in as `fake::ME`, synced once.
        async fn restored(client: Client) -> Client {
            use matrix_sdk::authentication::SessionTokens;
            use matrix_sdk::authentication::matrix::MatrixSession;
            let session = matrix_sdk::AuthSession::Matrix(MatrixSession {
                meta: matrix_sdk::SessionMeta {
                    user_id: fake::ME.try_into().unwrap(),
                    device_id: "FAKE".into(),
                },
                tokens: SessionTokens {
                    access_token: "token".into(),
                    refresh_token: None,
                },
            });
            client.restore_session(session).await.unwrap();
            client.sync_once(SyncSettings::default()).await.unwrap();
            client
        }

        fn app_options(search_history: bool) -> WorkerOptions {
            WorkerOptions::app(SecretStoreMode::File, search_history)
        }

        fn asked_for_messages(server: &fake::Server) -> usize {
            server
                .requests()
                .iter()
                .filter(|request| request.method == "GET" && request.path.ends_with("/messages"))
                .count()
        }

        /// Runs the session loop's part until the box shows `last`; what it showed on
        /// the way, in order.
        async fn until(
            history: &mut SearchHistory,
            events: &mut UnboundedReceiver<MatrixEvent>,
            last: fn(&HistoryRun) -> bool,
        ) -> Vec<HistoryRun> {
            let mut shown = Vec::new();
            let wait = async {
                loop {
                    tokio::select! {
                        report = history.next() => history.on_report(report),
                        Some(event) = events.recv() => {
                            let MatrixEvent::SearchHistory(run) = event else { continue };
                            let end = last(&run);
                            shown.push(run);
                            if end {
                                return;
                            }
                        }
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(20), wait)
                .await
                .unwrap_or_else(|_| panic!("the box never got there: {shown:?}"));
            shown
        }

        /// `/messages` of the room's start: no `end`, nothing more to fetch.
        fn at_the_start(request: &fake::Request) -> Option<Reply> {
            request
                .path
                .ends_with("/messages")
                .then(|| Reply::Json(r#"{"chunk":[],"start":"before-sync"}"#.to_owned()))
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_run_fetches_and_says_how_far() {
            let server = fake::Server::answering(
                rooms(vec![message("$own", fake::ME)], vec![]),
                Box::new(at_the_start),
            )
            .await;
            let client = signed_in(&server, app_options(true)).await;
            let (events, mut shown) = unbounded_channel();
            let mut history = SearchHistory::new(client.clone(), events, true, None);
            assert_eq!(
                asked_for_messages(&server),
                0,
                "not before the room list is in"
            );
            history.caught_up();
            let seen = until(&mut history, &mut shown, |run| {
                matches!(run, HistoryRun::Done { .. })
            })
            .await;
            let back_to = super::back_to(chrono::Local::now().date_naive());
            assert_eq!(
                seen,
                [
                    HistoryRun::Waiting,
                    HistoryRun::Running {
                        back_to: back_to.clone()
                    },
                    HistoryRun::Done { back_to }
                ]
            );
            assert_eq!(asked_for_messages(&server), 1);
            // Done for the session: on again starts nothing.
            history.set_enabled(true);
            assert!(shown.try_recv().is_err());
        }

        /// Off in the settings at the start, on later: the app's client has the queue,
        /// so it really fetches (with `serve`'s it could not).
        #[tokio::test(flavor = "multi_thread")]
        async fn switched_on_in_the_middle_of_a_session_it_fetches() {
            let server = fake::Server::answering(
                rooms(vec![message("$own", fake::ME)], vec![]),
                Box::new(at_the_start),
            )
            .await;
            let client = signed_in(&server, app_options(false)).await;
            let (events, mut shown) = unbounded_channel();
            let mut history = SearchHistory::new(client.clone(), events, false, None);
            history.caught_up();
            assert_eq!(
                shown.try_recv().ok().map(|event| event.describe()),
                Some("SearchHistory(Off)".to_owned())
            );
            assert!(shown.try_recv().is_err());
            history.set_enabled(true);
            let seen = until(&mut history, &mut shown, |run| {
                matches!(run, HistoryRun::Done { .. })
            })
            .await;
            assert!(
                matches!(
                    seen.as_slice(),
                    [
                        HistoryRun::Waiting,
                        HistoryRun::Running { .. },
                        HistoryRun::Done { .. }
                    ]
                ),
                "{seen:?}"
            );
            assert_eq!(asked_for_messages(&server), 1);

            // `serve`'s client has no queue: the run gives up at once, having asked
            // nothing.
            let server = fake::Server::answering(
                rooms(vec![message("$own", fake::ME)], vec![]),
                Box::new(at_the_start),
            )
            .await;
            let client = signed_in(&server, WorkerOptions::agent(SecretStoreMode::File)).await;
            let (events, mut shown) = unbounded_channel();
            let mut history = SearchHistory::new(client.clone(), events, true, None);
            history.caught_up();
            let seen = until(&mut history, &mut shown, |run| *run == HistoryRun::Stopped).await;
            assert_eq!(seen, [HistoryRun::Waiting, HistoryRun::Stopped]);
            assert_eq!(asked_for_messages(&server), 0);
        }

        /// The setting off (or the session's end) stops the run at once: nothing more is
        /// asked of the server. (Unstopped, the run would ask for the next room a second
        /// later.)
        #[tokio::test(flavor = "multi_thread")]
        async fn stopped_it_asks_nothing_more() {
            let server = fake::Server::answering(
                three_rooms(),
                Box::new(|request| request.path.ends_with("/messages").then_some(Reply::Never)),
            )
            .await;
            let client = signed_in(&server, app_options(true)).await;
            let (events, mut shown) = unbounded_channel();
            let mut history = SearchHistory::new(client.clone(), events, true, None);
            history.caught_up();
            tokio::time::timeout(Duration::from_secs(10), async {
                while asked_for_messages(&server) == 0 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the run asked for older messages");
            history.set_enabled(false);
            // Our task ends within a second (its end is reported as it goes).
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if let Report::Ended { finished, .. } = history.next().await {
                        assert!(!finished);
                        return;
                    }
                }
            })
            .await
            .expect("the run ended");
            tokio::time::sleep(Duration::from_millis(1500)).await;
            assert_eq!(asked_for_messages(&server), 1);
            let last = std::iter::from_fn(|| shown.try_recv().ok()).last();
            assert_eq!(
                last.map(|event| event.describe()),
                Some("SearchHistory(Off)".to_owned())
            );
            // The request on its way stays parked inside the queue until the client goes.
            drop(history);
            drop(client);
        }

        /// A busy room: every `/messages` brings 30 more events, each batch a day older
        /// than the one before. The SDK's sweep stops after its first 10 batches (each
        /// week's request reads those back from the store first and counts them); our
        /// walk goes on to 13 weeks, and no further.
        #[tokio::test(flavor = "multi_thread")]
        async fn the_walk_goes_back_13_weeks_where_the_sdk_stops_short() {
            const DAY_MS: u64 = 24 * 60 * 60 * 1000;
            let older: fake::Answer = {
                let next = AtomicU64::new(1);
                Box::new(move |request| {
                    if !request.path.ends_with("/messages") {
                        return None;
                    }
                    let day = next.fetch_add(1, Ordering::Relaxed);
                    let ts = now_ms() - day * DAY_MS;
                    let chunk: Vec<Value> = (0..30)
                        .map(|i| {
                            let mut event = message(&format!("$d{day}e{i}"), SOMEONE);
                            event["origin_server_ts"] = json!(ts - i);
                            event
                        })
                        .collect();
                    let body = json!({ "chunk": chunk, "start": format!("t{day}"), "end": format!("t{}", day + 1) });
                    Some(Reply::Json(body.to_string()))
                })
            };
            let server =
                fake::Server::answering(rooms(vec![message("$own", fake::ME)], vec![]), older)
                    .await;
            let client = signed_in(&server, app_options(true)).await;
            let (events, mut shown) = unbounded_channel();
            let mut history = SearchHistory::new(client.clone(), events, true, None);
            history.caught_up();
            until(&mut history, &mut shown, |run| {
                matches!(run, HistoryRun::Done { .. })
            })
            .await;
            // One request a day back: day 91 is the first that is 13 weeks old.
            let asked = asked_for_messages(&server);
            assert!((88..=92).contains(&asked), "{asked} requests");
        }

        /// How many messages of the room a search of the index finds.
        async fn found(client: &Client, word: &str) -> usize {
            let room = client.get_room(ROOM.try_into().unwrap()).expect("the room");
            room.search(word, 10, None)
                .await
                .map_or(0, |result| result.total_count)
        }

        /// Until a search finds `word`, within 10 s.
        async fn until_found(client: &Client, word: &str) {
            tokio::time::timeout(Duration::from_secs(10), async {
                while found(client, word).await == 0 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("the index never found it"));
        }

        /// Until `mark` in `dir` is gone, within 5 s.
        async fn until_gone(dir: &std::path::Path, mark: &str) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while dir.join(mark).exists() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{mark} stayed"));
        }

        /// A session that ends before the index has settled leaves `history-indexing`, and
        /// the next start has the SDK index the stored events again: what the index lost
        /// is found, though the server sends it no more. With no mark left, a later start
        /// indexes nothing again (the SDK's `reindexed-v1` contract, which this pins). The index losing a room's part is done by hand: it
        /// is what a quit does to what is still queued for it.
        #[tokio::test(flavor = "multi_thread")]
        async fn what_the_index_missed_is_indexed_at_the_next_start() {
            const WORD: &str = "mooncake";
            // The old message comes once; after that the room's start, empty.
            let older: fake::Answer = {
                let first = std::sync::atomic::AtomicBool::new(true);
                Box::new(move |request| {
                    if !request.path.ends_with("/messages") {
                        return None;
                    }
                    let mut chunk = Vec::new();
                    if first.swap(false, Ordering::Relaxed) {
                        let mut old = message("$old", SOMEONE);
                        old["content"]["body"] = json!(format!("a {WORD} from before"));
                        old["origin_server_ts"] = json!(now_ms() - 60_000);
                        chunk.push(old);
                    }
                    let body = json!({ "chunk": chunk, "start": "before-sync" });
                    Some(Reply::Json(body.to_string()))
                })
            };
            let server =
                fake::Server::answering(rooms(vec![message("$own", fake::ME)], vec![]), older)
                    .await;
            let (root, paths) = super::server::profile("history-marks");
            let dir = paths.search_dir();
            let session = |client: &Client, settle: Duration| {
                let (events, shown) = unbounded_channel();
                let mut history =
                    SearchHistory::new(client.clone(), events, true, Some(dir.clone()));
                history.settle = settle;
                history.caught_up();
                (history, shown)
            };
            let end = |history: SearchHistory, client: Client| async move {
                drop(history);
                drop(client);
                tokio::time::sleep(Duration::from_millis(500)).await;
            };
            let done = |run: &HistoryRun| matches!(run, HistoryRun::Done { .. });

            // 1. The first run fetches it; the session ends before the index settles.
            let client = on_disk(&server, &paths).await;
            let (mut history, mut shown) = session(&client, Duration::from_secs(3600));
            until(&mut history, &mut shown, done).await;
            until_found(&client, WORD).await;
            for mark in [INDEXING, REINDEXED] {
                assert!(dir.join(mark).exists(), "{mark}");
            }
            end(history, client).await;
            assert!(dir.join(ROOM).is_dir(), "the room's index");
            std::fs::remove_dir_all(dir.join(ROOM)).unwrap();

            // 2. The next start indexes the stored events again.
            let client = on_disk(&server, &paths).await;
            assert_eq!(found(&client, WORD).await, 0, "the index lost it");
            let (mut history, mut shown) = session(&client, Duration::ZERO);
            until(&mut history, &mut shown, done).await;
            until_found(&client, WORD).await;
            until_gone(&dir, INDEXING).await;
            assert!(dir.join(REINDEXED).exists());
            end(history, client).await;
            std::fs::remove_dir_all(dir.join(ROOM)).unwrap();

            // 3. Nothing left over: the store alone, not indexed again.
            let client = on_disk(&server, &paths).await;
            let (mut history, mut shown) = session(&client, Duration::ZERO);
            until(&mut history, &mut shown, done).await;
            until_gone(&dir, INDEXING).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert_eq!(found(&client, WORD).await, 0, "indexed again with no mark");
            end(history, client).await;
            let _ = std::fs::remove_dir_all(root);
        }

        /// The read-receipt backfill, which comes with the queue: a receipt pointing at an
        /// event the server never returns is chased at most 20 batches, and the room's
        /// unread dot is what it is without the queue.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_receipt_that_is_never_found_costs_at_most_20_requests() {
            let receipt = json!({
                "type": "m.receipt",
                "content": { "$gone": { "m.read": { fake::ME: { "ts": 1 } } } },
            });
            let sync = || {
                rooms(
                    vec![message("$new1", SOMEONE), message("$new2", SOMEONE)],
                    vec![receipt.clone()],
                )
            };
            // Always two more older messages, never the one the receipt is for.
            let older = || -> fake::Answer {
                let next = AtomicU64::new(0);
                Box::new(move |request| {
                    if !request.path.ends_with("/messages") {
                        return None;
                    }
                    let n = next.fetch_add(1, Ordering::Relaxed);
                    let chunk = [
                        message(&format!("$old{n}a"), SOMEONE),
                        message(&format!("$old{n}b"), SOMEONE),
                    ];
                    let body = json!({ "chunk": chunk, "start": format!("t{n}"), "end": format!("t{}", n + 1) });
                    Some(Reply::Json(body.to_string()))
                })
            };
            let unread = |client: &Client| {
                let room = client.get_room(ROOM.try_into().unwrap()).expect("the room");
                room.num_unread_notifications() > 0 || room.num_unread_messages() > 0
            };

            let server = fake::Server::answering(sync(), older()).await;
            let client = signed_in(&server, app_options(false)).await;
            // Until no more is asked for.
            let mut asked = 0;
            for _ in 0..40 {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let now = asked_for_messages(&server);
                if now > 0 && now == asked {
                    break;
                }
                asked = now;
            }
            assert!((1..=20).contains(&asked), "{asked} requests");

            let plain = fake::Server::answering(sync(), older()).await;
            let without = signed_in(&plain, WorkerOptions::agent(SecretStoreMode::File)).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert_eq!(asked_for_messages(&plain), 0);
            assert_eq!(unread(&client), unread(&without));
            assert!(unread(&client));
        }
    }

    /// Against the dev homeserver (`--ignored`, one at a time). Each test registers two
    /// accounts of its own, so the worker's account holds only the test's rooms.
    mod server {
        use std::path::{Path, PathBuf};
        use std::time::Duration;

        use matrix_sdk::Client;
        use matrix_sdk::config::SyncSettings;
        use matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType;
        use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoom;
        use matrix_sdk::ruma::events::receipt::ReceiptThread;
        use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
        use matrix_sdk::ruma::events::room::topic::RoomTopicEventContent;
        use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
        use tokio::sync::mpsc::UnboundedReceiver;

        use super::super::HistoryRun;
        use crate::matrix::{MatrixCmd, MatrixEvent, MatrixHandle, spawn_app};
        use crate::paths::Paths;

        pub(super) fn homeserver() -> String {
            std::env::var("MATRIX_RAT_TEST_HOMESERVER")
                .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned())
        }

        /// A new account on the dev server, logged in with a plain SDK client.
        pub(super) async fn account(homeserver: &str, name: &str) -> Client {
            use matrix_sdk::ruma::api::client::account::register::v3::Request as RegisterRequest;
            use matrix_sdk::ruma::api::client::uiaa::{AuthData, RegistrationToken};
            let token = std::env::var("MATRIX_RAT_TEST_REGISTRATION_TOKEN")
                .unwrap_or_else(|_| "devtoken".to_owned());
            let client = Client::builder()
                .homeserver_url(homeserver)
                .build()
                .await
                .expect("client");
            let mut request = RegisterRequest::new();
            request.username = Some(name.to_owned());
            request.password = Some(password(name));
            request.inhibit_login = true;
            if let Err(err) = client.matrix_auth().register(request.clone()).await {
                let mut auth = RegistrationToken::new(token);
                auth.session = err
                    .as_uiaa_response()
                    .unwrap_or_else(|| panic!("registration failed: {err}"))
                    .session
                    .clone();
                request.auth = Some(AuthData::RegistrationToken(auth));
                client
                    .matrix_auth()
                    .register(request)
                    .await
                    .expect("registration with the token");
            }
            client
                .matrix_auth()
                .login_username(name, &password(name))
                .send()
                .await
                .expect("login");
            client
        }

        pub(super) fn password(name: &str) -> String {
            format!("pw-{name}")
        }

        pub(super) fn stamp() -> u128 {
            crate::time::SystemTime::now()
                .duration_since(crate::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        }

        /// A room of `a`'s that `b` has joined.
        pub(super) async fn shared_room(a: &Client, b: &Client) -> OwnedRoomId {
            let mut create = CreateRoom::new();
            create.invite = vec![b.user_id().unwrap().to_owned()];
            let room = a.create_room(create).await.expect("room");
            let room_id = room.room_id().to_owned();
            b.join_room_by_id(&room_id).await.expect("join");
            room_id
        }

        /// Sends `text`, waiting out the server's rate limit; the event's id.
        pub(super) async fn say(room: &matrix_sdk::Room, text: String) -> OwnedEventId {
            for _ in 0..30 {
                match room
                    .send(RoomMessageEventContent::text_plain(text.as_str()))
                    .await
                {
                    Ok(sent) => return sent.response.event_id,
                    Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
                }
            }
            panic!("could not send");
        }

        pub(super) async fn set_topic(room: &matrix_sdk::Room, n: usize) -> OwnedEventId {
            for _ in 0..30 {
                match room
                    .send_state_event(RoomTopicEventContent::new(format!("topic {n}")))
                    .await
                {
                    Ok(sent) => return sent.event_id,
                    Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
                }
            }
            panic!("could not set the topic");
        }

        /// `b` has read the room up to `event`, as a user who keeps up does.
        pub(super) async fn read_up_to(b: &Client, room_id: &OwnedRoomId, event: OwnedEventId) {
            b.sync_once(SyncSettings::default()).await.expect("sync");
            b.get_room(room_id)
                .expect("joined")
                .send_single_receipt(ReceiptType::Read, ReceiptThread::Unthreaded, event)
                .await
                .expect("receipt");
        }

        /// A profile in a temp folder.
        pub(super) fn profile(tag: &str) -> (PathBuf, Paths) {
            let root =
                std::env::temp_dir().join(format!("matrix-rat-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let paths = Paths {
                config_dir: root.join("config"),
                data_dir: root.join("data"),
                log_dir: root.join("log"),
                runtime_dir: root.join("run"),
                profile: tag.into(),
            };
            paths.create_dirs().unwrap();
            (root, paths)
        }

        /// The worker's events until `found` picks one, within `secs`.
        pub(super) async fn until<T>(
            events: &mut UnboundedReceiver<MatrixEvent>,
            secs: u64,
            what: &str,
            mut found: impl FnMut(MatrixEvent) -> Option<T>,
        ) -> T {
            let wait = async {
                loop {
                    match events.recv().await.expect("worker stopped") {
                        MatrixEvent::LoginFailed(err) => panic!("login failed: {err}"),
                        event => {
                            if let Some(it) = found(event) {
                                return it;
                            }
                        }
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(secs), wait)
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        }

        /// How many messages a search for `query` finds.
        pub(super) async fn found(
            handle: &MatrixHandle,
            events: &mut UnboundedReceiver<MatrixEvent>,
            generation: u64,
            query: &str,
        ) -> usize {
            handle.send(MatrixCmd::SearchMessages {
                generation,
                query: query.to_owned(),
            });
            let mut count = 0;
            until(events, 60, "the search", |event| match event {
                MatrixEvent::SearchResults {
                    generation: g,
                    hits,
                    done,
                    ..
                } if g == generation => {
                    count += hits.len();
                    done.then_some(count)
                }
                _ => None,
            })
            .await
        }

        pub(super) async fn shut_down(
            handle: MatrixHandle,
            mut events: UnboundedReceiver<MatrixEvent>,
        ) {
            handle.send(MatrixCmd::Shutdown);
            while events.recv().await.is_some() {}
        }

        /// The size of what is in `dir`, in bytes.
        pub(super) fn size(dir: &Path) -> u64 {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return 0;
            };
            entries
                .flatten()
                .map(|entry| match entry.metadata() {
                    Ok(meta) if meta.is_dir() => size(&entry.path()),
                    Ok(meta) => meta.len(),
                    Err(_) => 0,
                })
                .sum()
        }

        /// Messages older than the first sync are found once the run is done; the room,
        /// open meanwhile, gets them at its top in order.
        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "needs a local homeserver"]
        async fn search_finds_messages_older_than_the_first_sync() {
            let homeserver = homeserver();
            let stamp = stamp();
            let a = account(&homeserver, &format!("histfinda{stamp}")).await;
            let b_name = format!("histfindb{stamp}");
            let b = account(&homeserver, &b_name).await;
            let room_id = shared_room(&a, &b).await;
            let room = a.get_room(&room_id).expect("A's room");
            let word = format!("oldestword{stamp}");
            say(&room, format!("{word} is the first of them")).await;
            let mut last = None;
            for i in 1..60 {
                last = Some(say(&room, format!("message {i:02} of the test")).await);
            }
            // B keeps up: its receipt is at the end, so the read-receipt backfill has
            // nothing to fetch and the old messages can come only from the run.
            read_up_to(&b, &room_id, last.unwrap()).await;

            let (root, paths) = profile("history-find");
            let (handle, mut events) = spawn_app(paths, true);
            handle.send(MatrixCmd::LoginPassword {
                homeserver,
                username: b_name.clone(),
                password: password(&b_name).into(),
            });
            let mut generation = 0;
            let mut timeline = Vec::new();
            let mut seen = Vec::new();
            until(&mut events, 120, "the run's end", |event| match event {
                MatrixEvent::Rooms(rooms) => {
                    if generation == 0 && rooms.iter().any(|r| r.id == room_id.as_str()) {
                        generation = 1;
                        handle.send(MatrixCmd::OpenRoom {
                            room_id: room_id.to_string(),
                            generation,
                        });
                    }
                    None
                }
                MatrixEvent::Timeline {
                    generation: 1,
                    messages,
                    ..
                } => {
                    timeline = messages;
                    None
                }
                MatrixEvent::SearchHistory(run) => {
                    let done = matches!(run, HistoryRun::Done { .. });
                    seen.push(run);
                    done.then_some(())
                }
                _ => None,
            })
            .await;
            assert!(
                matches!(
                    seen.as_slice(),
                    [
                        HistoryRun::Waiting,
                        HistoryRun::Running { .. },
                        HistoryRun::Done { .. }
                    ]
                ),
                "{seen:?}"
            );
            // The index may take a moment to take the last batch in.
            let mut hits = 0;
            for generation in 1..=10 {
                hits = found(&handle, &mut events, generation, &word).await;
                if hits > 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            assert_eq!(hits, 1, "the first message is found");

            // The open room took the older messages in at its top, once each and in order.
            let numbers: Vec<u32> = timeline
                .iter()
                .filter_map(|message| {
                    let rest = message.body.strip_prefix("message ")?;
                    rest.split(' ').next()?.parse().ok()
                })
                .collect();
            assert!(!numbers.is_empty(), "the open room has its messages");
            assert!(numbers.windows(2).all(|w| w[0] < w[1]), "{numbers:?}");
            assert_eq!(numbers.last(), Some(&59));
            let mut ids: Vec<&str> = timeline.iter().map(|m| m.id.as_str()).collect();
            let shown = ids.len();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), shown, "no message twice");

            shut_down(handle, events).await;
            let _ = std::fs::remove_dir_all(root);
        }

        /// A room whose last events hold no message gets its preview from older ones: the
        /// latest-event backfill, which comes with the app's queue whatever the setting.
        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "needs a local homeserver"]
        async fn a_preview_comes_from_older_messages() {
            let homeserver = homeserver();
            let stamp = stamp();
            let a = account(&homeserver, &format!("histpreva{stamp}")).await;
            let b_name = format!("histprevb{stamp}");
            let b = account(&homeserver, &b_name).await;
            let room_id = shared_room(&a, &b).await;
            let room = a.get_room(&room_id).expect("A's room");
            let word = format!("previewword{stamp}");
            say(&room, format!("{word} before the topics")).await;
            let mut last = None;
            for n in 0..40 {
                last = Some(set_topic(&room, n).await);
            }
            read_up_to(&b, &room_id, last.unwrap()).await;

            let (root, paths) = profile("history-preview");
            let (handle, mut events) = spawn_app(paths, false);
            handle.send(MatrixCmd::LoginPassword {
                homeserver,
                username: b_name.clone(),
                password: password(&b_name).into(),
            });
            until(&mut events, 30, "the login", |event| {
                matches!(event, MatrixEvent::LoggedIn { .. }).then_some(())
            })
            .await;
            // As the app does once it is logged in.
            handle.send(MatrixCmd::Previews(true));
            let mut listed = None;
            let preview = until(&mut events, 40, "the room's preview", |event| {
                let MatrixEvent::Rooms(rooms) = event else {
                    return None;
                };
                let room = rooms.into_iter().find(|r| r.id == room_id.as_str())?;
                listed.get_or_insert_with(crate::time::Instant::now);
                room.preview.filter(|preview| preview.contains(&word))
            })
            .await;
            let after = listed.expect("listed").elapsed();
            assert!(after < Duration::from_secs(10), "{after:?}");
            assert!(preview.contains(&word), "{preview}");
            shut_down(handle, events).await;
            let _ = std::fs::remove_dir_all(root);
        }
    }

    /// What the run costs, on the dev server: `search_history_cost`, run by hand with
    /// `MATRIX_RAT_MEASURE=1` before a release (docs/DESIGN-1.1.md, "Measuring before the
    /// release").
    mod measure {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::Duration;

        use futures::StreamExt;
        use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
        use tokio::net::{TcpListener, TcpStream};
        use tokio::sync::mpsc::UnboundedReceiver;

        use super::super::HistoryRun;
        use super::server::{
            account, found, homeserver, password, profile, read_up_to, say, set_topic, shut_down,
            size, stamp, until,
        };
        use crate::matrix::{MatrixCmd, MatrixEvent, MatrixHandle, spawn_app};
        use crate::time::Instant;

        /// What went through the relay so far: requests by kind and bytes each way, and
        /// the `/messages` of each room of [`PLANS`], in its order.
        #[derive(Debug, Clone, Default, PartialEq, Eq)]
        struct Tally {
            messages: u64,
            /// How long the server took for them, up to the first byte of each answer.
            messages_ms: u64,
            sync: u64,
            keys: u64,
            other: u64,
            up: u64,
            down: u64,
            per_room: Vec<u64>,
        }

        impl Tally {
            fn since(&self, before: &Tally) -> Tally {
                Tally {
                    messages: self.messages - before.messages,
                    messages_ms: self.messages_ms - before.messages_ms,
                    sync: self.sync - before.sync,
                    keys: self.keys - before.keys,
                    other: self.other - before.other,
                    up: self.up - before.up,
                    down: self.down - before.down,
                    per_room: self
                        .per_room
                        .iter()
                        .zip(&before.per_room)
                        .map(|(now, then)| now - then)
                        .collect(),
                }
            }

            /// How long the server took for a `/messages`, on average.
            fn seconds_each(&self) -> f64 {
                self.messages_ms as f64 / 1000.0 / self.messages.max(1) as f64
            }

            fn line(&self) -> String {
                format!(
                    "messages {} {:?} ({:.2} s each) sync {} keys {} other {} | up {} KB down {} KB",
                    self.messages,
                    self.per_room,
                    self.seconds_each(),
                    self.sync,
                    self.keys,
                    self.other,
                    self.up / 1024,
                    self.down / 1024
                )
            }
        }

        #[derive(Default)]
        struct Counts {
            messages: AtomicU64,
            messages_ms: AtomicU64,
            sync: AtomicU64,
            keys: AtomicU64,
            other: AtomicU64,
            up: AtomicU64,
            down: AtomicU64,
            /// Connections whose requests could no longer be told apart (a body sent in
            /// chunks): their requests are not counted.
            lost: AtomicU64,
            /// The rooms of [`PLANS`] (their ids, in memory only) and their `/messages`.
            rooms: std::sync::Mutex<Vec<(String, u64)>>,
        }

        impl Counts {
            fn tally(&self) -> Tally {
                let get = |n: &AtomicU64| n.load(Ordering::Relaxed);
                Tally {
                    messages: get(&self.messages),
                    messages_ms: get(&self.messages_ms),
                    sync: get(&self.sync),
                    keys: get(&self.keys),
                    other: get(&self.other),
                    up: get(&self.up),
                    down: get(&self.down),
                    per_room: self.rooms.lock().unwrap().iter().map(|r| r.1).collect(),
                }
            }

            /// Counts a request by its head; the length of its body (`None` if it cannot
            /// be known) and whether it is a `/messages`. Only the kind (and the room of a
            /// `/messages`) is kept, never the path.
            fn request(&self, head: &[u8]) -> Option<(u64, bool)> {
                let head = String::from_utf8_lossy(head);
                let mut lines = head.split("\r\n").filter(|line| !line.is_empty());
                let mut first = lines.next()?.split(' ');
                let (_method, target) = (first.next()?, first.next()?);
                let path = target.split('?').next().unwrap_or_default();
                let kind = if path.ends_with("/messages") {
                    let room = path
                        .rsplit('/')
                        .nth(1)
                        .unwrap_or_default()
                        .replace("%21", "!")
                        .replace("%3A", ":")
                        .replace("%3a", ":");
                    if let Some(entry) = self.rooms.lock().unwrap().iter_mut().find(|r| r.0 == room)
                    {
                        entry.1 += 1;
                    }
                    &self.messages
                } else if path.contains("/sync") {
                    &self.sync
                } else if path.contains("/keys/") || path.contains("/room_keys/") {
                    &self.keys
                } else {
                    &self.other
                };
                kind.fetch_add(1, Ordering::Relaxed);
                let messages = std::ptr::eq(kind, &self.messages);
                let mut length = 0;
                for line in lines {
                    let line = line.to_ascii_lowercase();
                    if let Some(value) = line.strip_prefix("content-length:") {
                        length = value.trim().parse().ok()?;
                    } else if line.starts_with("transfer-encoding:") {
                        return None;
                    }
                }
                Some((length, messages))
            }
        }

        /// A test-only TCP relay in front of the dev server that counts what goes through.
        struct Relay {
            url: String,
            counts: Arc<Counts>,
            task: tokio::task::JoinHandle<()>,
        }

        impl Drop for Relay {
            fn drop(&mut self) {
                self.task.abort();
            }
        }

        impl Relay {
            async fn start(upstream: &str) -> Self {
                let upstream = upstream
                    .trim_start_matches("http://")
                    .trim_end_matches('/')
                    .to_owned();
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("http://{}", listener.local_addr().unwrap());
                let counts = Arc::new(Counts::default());
                let task = tokio::spawn({
                    let counts = counts.clone();
                    async move {
                        while let Ok((client, _)) = listener.accept().await {
                            let counts = counts.clone();
                            let upstream = upstream.clone();
                            tokio::spawn(async move {
                                let Ok(server) = TcpStream::connect(&upstream).await else {
                                    return;
                                };
                                let (from_client, to_client) = client.into_split();
                                let (from_server, to_server) = server.into_split();
                                let asked = Asked::default();
                                tokio::join!(
                                    pump(
                                        from_client,
                                        to_server,
                                        counts.clone(),
                                        asked.clone(),
                                        true
                                    ),
                                    pump(from_server, to_client, counts, asked, false),
                                );
                            });
                        }
                    }
                });
                Self { url, counts, task }
            }

            fn tally(&self) -> Tally {
                self.counts.tally()
            }
        }

        /// The requests of a connection whose answers have not begun yet, in order:
        /// when each `/messages` went (`None` for the others).
        type Asked = Arc<std::sync::Mutex<std::collections::VecDeque<Option<Instant>>>>;

        /// Copies one way; `up`: from the client, whose requests are counted. A client
        /// asks again on a connection only once it has the whole answer, so what comes
        /// from the server while a request waits is the start of its answer.
        async fn pump(
            mut from: OwnedReadHalf,
            mut to: OwnedWriteHalf,
            counts: Arc<Counts>,
            asked: Asked,
            up: bool,
        ) {
            let mut buf = vec![0u8; 64 * 1024];
            let mut head = Vec::new();
            let mut body_left = 0u64;
            let mut lost = false;
            loop {
                let n = match from.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                if up {
                    counts.up.fetch_add(n as u64, Ordering::Relaxed);
                    let mut data = &buf[..n];
                    while !data.is_empty() && !lost {
                        if body_left > 0 {
                            let take = data.len().min(body_left as usize);
                            body_left -= take as u64;
                            data = &data[take..];
                            continue;
                        }
                        head.push(data[0]);
                        data = &data[1..];
                        if head == b"\r\n" {
                            head.clear();
                        } else if head.ends_with(b"\r\n\r\n") {
                            match counts.request(&head) {
                                Some((length, messages)) => {
                                    body_left = length;
                                    asked.lock().unwrap().push_back(messages.then(Instant::now));
                                }
                                None => {
                                    lost = true;
                                    counts.lost.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            head.clear();
                        } else if head.len() > 64 * 1024 {
                            lost = true;
                            counts.lost.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                } else {
                    counts.down.fetch_add(n as u64, Ordering::Relaxed);
                    if let Some(Some(went)) = asked.lock().unwrap().pop_front() {
                        let ms = u64::try_from(went.elapsed().as_millis()).unwrap_or(u64::MAX);
                        counts.messages_ms.fetch_add(ms, Ordering::Relaxed);
                    }
                }
                if to.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
            let _ = to.shutdown().await;
        }

        #[test]
        fn the_relay_counts_requests_by_kind_and_skips_bodies() {
            let counts = Counts::default();
            let body = "{\"a\":\"GET /x/messages HTTP/1.1\"}";
            let head = format!(
                "POST /_matrix/client/v3/keys/query HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            assert_eq!(
                counts.request(head.as_bytes()),
                Some((body.len() as u64, false))
            );
            let head = "GET /_matrix/client/v3/rooms/x/messages?dir=b HTTP/1.1\r\nhost: h\r\n\r\n";
            assert_eq!(counts.request(head.as_bytes()), Some((0, true)));
            let head = "PUT /_matrix/client/v3/x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
            assert_eq!(counts.request(head.as_bytes()), None);
            let tally = counts.tally();
            assert_eq!((tally.messages, tally.keys, tally.other), (1, 1, 1));
            // `/messages` of a room of the plan are counted for it too.
            *counts.rooms.lock().unwrap() = vec![("!a:x".to_owned(), 0), ("!b:x".to_owned(), 0)];
            let head = "GET /_matrix/client/v3/rooms/%21b%3Ax/messages?dir=b HTTP/1.1\r\n\r\n";
            counts.request(head.as_bytes());
            assert_eq!(counts.tally().per_room, [0, 1]);
        }

        /// A room of the measurement: `messages` messages, then `topics` topic changes.
        /// `marks`: depths (counted in messages from the newest) whose message holds a
        /// word of its own, to tell by searching how far back the run reached. `read`: the
        /// worker's user has read it to the end (a lurker has no receipt in it).
        struct Plan {
            name: &'static str,
            messages: usize,
            topics: usize,
            marks: &'static [usize],
            read: bool,
        }

        const PLANS: &[Plan] = &[
            Plan {
                name: "busy",
                messages: 5000,
                topics: 0,
                marks: &[100, 1000, 2000, 3000, 4000, 5000],
                read: true,
            },
            Plan {
                name: "mediuma",
                messages: 1000,
                topics: 0,
                marks: &[100, 500, 1000],
                read: true,
            },
            Plan {
                name: "mediumb",
                messages: 1000,
                topics: 0,
                marks: &[100, 500, 1000],
                read: false,
            },
            Plan {
                name: "quieta",
                messages: 20,
                topics: 0,
                marks: &[20],
                read: true,
            },
            Plan {
                name: "quietb",
                messages: 20,
                topics: 0,
                marks: &[20],
                read: true,
            },
            Plan {
                name: "quietc",
                messages: 20,
                topics: 0,
                marks: &[20],
                read: true,
            },
            Plan {
                name: "quietd",
                messages: 20,
                topics: 0,
                marks: &[20],
                read: true,
            },
            Plan {
                name: "quiete",
                messages: 20,
                topics: 0,
                marks: &[20],
                read: true,
            },
            Plan {
                name: "stateonly",
                messages: 1,
                topics: 2000,
                marks: &[1],
                read: true,
            },
        ];

        fn mark(plan: &Plan, depth: usize, stamp: u128) -> String {
            format!("m{}d{depth}x{stamp}", plan.name)
        }

        const FILLER: &str = "The quick brown fox jumps over the lazy dog while the rat \
            reads its messages in the terminal, one line after another, and keeps an eye on \
            the clock because the night is long and the queue is longer still.";

        /// The text of message `i` (0 is the oldest): 100 to 200 characters.
        fn text(plan: &Plan, i: usize, stamp: u128) -> String {
            let depth = plan.messages - i;
            let mark = if plan.marks.contains(&depth) {
                format!("{} ", mark(plan, depth, stamp))
            } else {
                String::new()
            };
            let filler = &FILLER[..60 + (i * 37) % 90];
            format!("{mark}message {i} of the {} room. {filler}", plan.name)
        }

        /// A fills the room, which B has joined already, so all of it is older than B's
        /// join. The room and its last event.
        async fn fill(
            a: &matrix_sdk::Client,
            plan: &Plan,
            room_id: OwnedRoomId,
            stamp: u128,
        ) -> (OwnedRoomId, OwnedEventId) {
            let room = a.get_room(&room_id).expect("A's room");
            let ahead = 4;
            futures::stream::iter(0..plan.messages.saturating_sub(1))
                .map(|i| say(&room, text(plan, i, stamp)))
                .buffer_unordered(ahead)
                .collect::<Vec<_>>()
                .await;
            let mut last = say(&room, text(plan, plan.messages - 1, stamp)).await;
            if plan.topics > 0 {
                futures::stream::iter(0..plan.topics - 1)
                    .map(|n| set_topic(&room, n))
                    .buffer_unordered(ahead)
                    .collect::<Vec<_>>()
                    .await;
                last = set_topic(&room, plan.topics).await;
            }
            (room_id, last)
        }

        /// For each room, the deepest of its marks that a search finds (0: none).
        async fn reached(
            handle: &MatrixHandle,
            events: &mut UnboundedReceiver<MatrixEvent>,
            generation: &mut u64,
            stamp: u128,
        ) -> Vec<usize> {
            let mut deepest = Vec::new();
            for plan in PLANS {
                let mut depth = 0;
                for &mark_depth in plan.marks {
                    *generation += 1;
                    if found(handle, events, *generation, &mark(plan, mark_depth, stamp)).await > 0
                    {
                        depth = depth.max(mark_depth);
                    }
                }
                deepest.push(depth);
            }
            deepest
        }

        /// The marks a run should reach: the busy room's 3 000th message (of 3 900 fetched),
        /// the others' oldest.
        fn deepest_due(plan: &Plan) -> usize {
            if plan.messages > 130 * 30 {
                3000
            } else {
                plan.messages
            }
        }

        /// Until a search finds the mark each room's walk should reach, up to 5 minutes:
        /// how long after the run's end that was. The SDK indexes what is fetched one batch
        /// at a time, in a task of its own, so the index can lag behind the run.
        async fn until_indexed(
            handle: &MatrixHandle,
            events: &mut UnboundedReceiver<MatrixEvent>,
            generation: &mut u64,
            stamp: u128,
        ) -> String {
            let start = Instant::now();
            let mut due: Vec<&Plan> = PLANS.iter().collect();
            while start.elapsed() < Duration::from_secs(300) {
                let mut left = Vec::new();
                for plan in due {
                    *generation += 1;
                    let word = mark(plan, deepest_due(plan), stamp);
                    if found(handle, events, *generation, &word).await == 0 {
                        left.push(plan);
                    }
                }
                if left.is_empty() {
                    return format!("{} s after the run", start.elapsed().as_secs());
                }
                due = left;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            let names: Vec<&str> = due.iter().map(|plan| plan.name).collect();
            format!("not after 300 s ({names:?} short)")
        }

        /// Until no `/messages` has gone out for `quiet`; when the last one went, from
        /// `start`. Other events are dropped meanwhile.
        async fn until_quiet(
            relay: &Relay,
            events: &mut UnboundedReceiver<MatrixEvent>,
            start: Instant,
            quiet: Duration,
        ) -> Duration {
            let mut last = relay.tally().messages;
            let mut since = Instant::now();
            while start.elapsed() < Duration::from_secs(30 * 60) {
                tokio::time::sleep(Duration::from_millis(250)).await;
                while events.try_recv().is_ok() {}
                let now = relay.tally().messages;
                if now != last {
                    last = now;
                    since = Instant::now();
                } else if since.elapsed() >= quiet {
                    break;
                }
            }
            since.duration_since(start)
        }

        /// Until the run is done; how long it went from `Running`.
        async fn until_done(events: &mut UnboundedReceiver<MatrixEvent>) -> Duration {
            let mut running = None;
            until(events, 60 * 60, "the run's end", |event| match event {
                MatrixEvent::SearchHistory(HistoryRun::Running { .. }) => {
                    running.get_or_insert_with(Instant::now);
                    None
                }
                MatrixEvent::SearchHistory(HistoryRun::Done { .. }) => {
                    Some(running.expect("Running before Done").elapsed())
                }
                MatrixEvent::SearchHistory(HistoryRun::Stopped) => panic!("the run stopped"),
                _ => None,
            })
            .await
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "measures, with MATRIX_RAT_MEASURE=1 and a local homeserver"]
        async fn search_history_cost() {
            if std::env::var("MATRIX_RAT_MEASURE").as_deref() != Ok("1") {
                return;
            }
            let homeserver = homeserver();
            let stamp = stamp();
            let a = account(&homeserver, &format!("costa{stamp}")).await;
            let b_name = format!("costb{stamp}");
            let b = account(&homeserver, &b_name).await;
            let filling = Instant::now();
            // One after another: a room's id is the hash of its create event, and two made
            // by A in the same millisecond would be the same room.
            let mut made = Vec::new();
            for _ in PLANS {
                made.push(super::server::shared_room(&a, &b).await);
            }
            let filled = futures::future::join_all(
                PLANS
                    .iter()
                    .zip(made)
                    .map(|(plan, room_id)| fill(&a, plan, room_id, stamp)),
            )
            .await;
            let room_ids: Vec<(String, u64)> = filled
                .iter()
                .map(|(room_id, _)| (room_id.to_string(), 0))
                .collect();
            for (plan, (room_id, last)) in PLANS.iter().zip(filled) {
                if plan.read {
                    read_up_to(&b, &room_id, last).await;
                }
            }
            let fill_s = filling.elapsed().as_secs();
            let events_sent: usize = PLANS.iter().map(|p| p.messages + p.topics).sum();
            drop(b);

            let relay = Relay::start(&homeserver).await;
            *relay.counts.rooms.lock().unwrap() = room_ids;
            let (root, paths) = profile("history-cost");
            let mut generation = 0;

            // 1. Search history off: what the latest-event and read-receipt backfills
            //    cost at the start, with previews on as the app has them.
            let start = Instant::now();
            let (handle, mut events) = spawn_app(paths.clone(), false);
            handle.send(MatrixCmd::LoginPassword {
                homeserver: relay.url.clone(),
                username: b_name.clone(),
                password: password(&b_name).into(),
            });
            until(&mut events, 60, "the login", |event| {
                matches!(event, MatrixEvent::LoggedIn { .. }).then_some(())
            })
            .await;
            handle.send(MatrixCmd::Previews(true));
            until(&mut events, 120, "the room list", |event| {
                matches!(event, MatrixEvent::CaughtUp).then_some(())
            })
            .await;
            let side_s = until_quiet(&relay, &mut events, start, Duration::from_secs(10)).await;
            let step1 = relay.tally();
            let disk1 = size(&root);
            let reached1 = reached(&handle, &mut events, &mut generation, stamp).await;

            // 2. Switched on in the middle of the session.
            let before = relay.tally();
            handle.send(MatrixCmd::SearchHistory { enabled: true });
            let run2 = until_done(&mut events).await;
            let step2 = relay.tally().since(&before);
            let indexed = until_indexed(&handle, &mut events, &mut generation, stamp).await;
            let disk2 = size(&root);
            let reached2 = reached(&handle, &mut events, &mut generation, stamp).await;
            shut_down(handle, events).await;

            // 3. A later start on the same profile: mostly from the disk.
            let before = relay.tally();
            let (handle, mut events) = spawn_app(paths, true);
            let run3 = until_done(&mut events).await;
            let step3 = relay.tally().since(&before);
            tokio::time::sleep(Duration::from_secs(2)).await;
            let disk3 = size(&root);
            let reached3 = reached(&handle, &mut events, &mut generation, stamp).await;
            shut_down(handle, events).await;
            let lost = relay.counts.lost.load(Ordering::Relaxed);
            drop(relay);
            let _ = std::fs::remove_dir_all(root);

            // The estimate for 100 000 events in 100 rooms (see the design). A request
            // brings 30 events, and each room ends with one that brings fewer; the gate
            // counts them at 0.5 s each, one after another (our walk takes one room at a
            // time). Apart from that, the pacing: the SDK's sweep waits 1 s between the
            // rooms it has yet to finish, each week, and our walk 1 s between rooms. The
            // disk: what step 2 added, per event it fetched (full batches, so a little
            // low).
            let fetched2 = (step2.messages * 30).max(1);
            let per_event = disk2.saturating_sub(disk1) as f64 / fetched2 as f64;
            let down_per_event = step2.down as f64 / fetched2 as f64;
            let requests_100k = 100_000.0 / 30.0 + 100.0;
            let minutes = |s_each: f64| requests_100k * s_each / 60.0;
            let pacing_100k = (13.0 * 99.0 + 99.0) / 60.0;
            let disk_100k_mb = 100_000.0 * per_event / 1e6;
            println!(
                "search_history_cost: filled {events_sent} events in {} rooms in {fill_s} s; \
                 step 1 (off, previews on) {} in {:.0} s, disk {} KB, reached {reached1:?}; \
                 step 2 (on) {} in {:.0} s, all indexed {indexed}, disk +{} KB, \
                 reached {reached2:?}, \
                 {per_event:.0} B on disk and {down_per_event:.0} B down per event fetched; \
                 step 3 (restart) {} in {:.0} s, disk +{} KB, reached {reached3:?}; \
                 lost connections {lost}",
                PLANS.len(),
                step1.line(),
                side_s.as_secs_f64(),
                disk1 / 1024,
                step2.line(),
                run2.as_secs_f64(),
                disk2.saturating_sub(disk1) / 1024,
                step3.line(),
                run3.as_secs_f64(),
                disk3.saturating_sub(disk2) / 1024,
            );
            println!(
                "search_history_cost: 100k events in 100 rooms: about {requests_100k:.0} \
                 requests, {disk_100k_mb:.0} MB, {:.0} min at 0.5 s/request ({:.0} min at the \
                 measured {:.2} s), plus up to {pacing_100k:.0} min of pacing; \
                 gate (1 GB, 60 min): {}",
                minutes(0.5),
                minutes(step2.seconds_each()),
                step2.seconds_each(),
                if disk_100k_mb > 1000.0 || minutes(0.5) > 60.0 {
                    "OVER, the default goes back to the user"
                } else {
                    "within"
                }
            );
            // Loose bounds only: times on a shared dev server vary.
            let rooms = PLANS.len() as u64;
            assert!(step1.messages <= rooms * 20 + 2000 / 30 + 10, "{step1:?}");
            assert!(step2.messages <= rooms * (10 + 130), "{step2:?}");
            assert!(step2.messages > 0, "{step2:?}");
            // Our walk reaches what the SDK's sweep alone does not.
            assert!(reached2[0] >= 3000, "{reached2:?}");
            assert!(step3.messages <= rooms, "{step3:?}");
            assert_eq!(lost, 0);
        }
    }
}
