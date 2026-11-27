# 1.1.0: search history, failed changes, deletion reasons, verified without keys

Part of 1.1.0 ([`PLAN.md`](PLAN.md) §6.1): the three matrix-sdk 0.20 features the user chose,
and one fix that matrix-agy's review of the 0.20 update found
(`.claude/agy/review-sdk-0.20-2.md` §2). Written 2026-11-27 from local `main` at `e260d34`
(matrix-sdk, matrix-sdk-base and matrix-sdk-ui pinned at `=0.20.0`). Design only: nothing here
is built yet.

Revised the same day after matrix-agy's review of `fbb3cce` (`.claude/agy/review-design-1.1.md`,
verdict CHANGES): its nine must-fix points and its smaller notes are folded in, the two open
questions are now the user's decisions (see [Decisions](#decisions)), and the cost of item 1 is
measured on the dev homeserver, not on a profile copy.

References: ours are `src/…:line` at `e260d34`; main has moved since (deps-1.1, the dot), so a
line may be off by a few. The SDK's are inside the 0.20.0 crates in the
cargo registry (`~/.cargo/registry/src/index.crates.io-*/`), written as `matrix-sdk/src/…:line`
and likewise for `matrix-sdk-ui`, `matrix-sdk-crypto`, `matrix-sdk-base` and
`matrix-sdk-sqlite`.

## Where the code stands

| Item | Today | What 0.20 offers |
|---|---|---|
| 1. Search history | `Ctrl-G` finds what this device has loaded: what the syncs bring (20 events per room at start, `src/matrix/mod.rs:1894`) and what the user scrolled back to. The 0.20 migration empties the event cache and rebuilds the index, so search finds less than before (`docs/RELEASE-NOTES-1.1-draft.md:18-23`). | `EventCache::run_search_backfill` (`matrix-sdk/src/event_cache/search_backfill.rs:94`): indexes what is stored, once, then pages every joined room back 13 weeks. Needs the client's back-pagination queue, which is off by default. |
| 2. Failed changes | A failed message shows "✕ not sent"; Enter tries again, Del discards. A failed edit, reaction or deletion shows nothing: it looks sent, and it blocks every later send in that room, across restarts, with no way out in the app. | The send state of each kind on the item (`edit_send_state()`, `redaction_send_state()`, `ReactionInfo::send_state`) and `Timeline::retry_send`/`abort_send` with a `SendTarget`. Discarding the last pending edit puts the old text back. |
| 3. Deletion reason | "Message deleted", nothing more. Deleting someone else's message asks for a reason; deleting our own cannot take one. | `EventTimelineItem::redaction_reason()` (`matrix-sdk-ui/src/timeline/event_item/mod.rs:266`), also for events that were deleted before we saw them. |
| 4. Verified without keys | A new device whose verification the old device confirmed first can end up verified but without the private signing keys and the backup key (an SDK race). The app says "this device is verified" and offers nothing. The server test `a_chosen_session_is_verified` fails about 1 run in 8 from the same race. | Nothing public asks for the secrets again; a new verification does (see item 4). |

## 1. Search reaches back three months

### What the user sees

A short while after start, once the room list is in, matrix-rat fetches the older messages of
every joined room, the newest week first, back about three months, so that `Ctrl-G` finds
them. Nothing else changes on screen: no spinner in the status line. The search box says how
far it has got.

Empty query, while fetching (box as today, `src/ui/global_search.rs`, 78 wide):

```
╭ Search all rooms ──────────────────────────────────────────────────────────╮
│ ⌕ type words to search…                                                    │
│ ────────────────────────────────────────────────────────────────────────── │
│   Searches messages on this device                                         │
│   Fetching older messages, back to 28 Aug…                                 │
```

When done:

```
│   Searches messages on this device, back to about 28 Aug                   │
```

The date is 13 weeks before now, written short: day and month within the same year, with the
year when the fetch reaches into the last one (from January to March), e.g. on 10 Feb 2027:

```
│   Fetching older messages, back to 11 Nov 2026…                            │
```

Off in the settings (today's text): `Searches messages loaded on this device`.

Matches while fetching: the note row at the bottom (`src/ui/global_search.rs:244-255`) says so.
When the "newest 200" note also applies, that one wins, since it asks the user to do something.

```
│ ─────────────────────────────────────────────────────────────── 12 matches │
│ › Design · Nora                                                      Today │
│   We should keep the Friday deadline after all                             │
│   …                                                                        │
│   Still fetching older messages: search again later for more               │
```

No matches while fetching: `no matches yet; still fetching older messages`.

No automatic new search when the fetch ends: hits moving under the cursor would be worse than
a note.

Settings, Behaviour group, after "Read markers" (the rows about what goes to and comes from the
server): `Search history   3 months` / `off`. Enter or ←→ switch it, saved as
`search_history = true|false`, on by default (the user's decision, see
[Decisions](#decisions)). Off stops a running fetch at once; on starts one if none has finished in
this session, and it really fetches, because the app always has the queue (see [The app always
has the queue](#the-app-always-has-the-queue)). The setting governs only this three-month fetch.

### The SDK, as 0.20 has it

- **The queue.** `ClientBuilder::with_enable_automatic_back_pagination(true)`
  (`matrix-sdk/src/client/builder/mod.rs:593`), off by default. It makes a shared
  `BackPaginationQueue` when the event cache starts (`matrix-sdk/src/event_cache/mod.rs:273-286`):
  at most 3 runs at once (`DEFAULT_MAX_CONCURRENT_BACK_PAGINATIONS`, `:603`), one per room, by
  priority.
- **The flag turns on three things, not one.** We cannot have the first without the others:
  - the search backfill (Low priority, only when we call it);
  - the latest-event backfill (`latest_events/room_latest_events.rs:195-206, 300-342`; High
    priority, no batch cap, detached). It pages back a room whose loaded events hold nothing to
    show as its latest message, until it finds something;
  - the read-receipt backfill (`event_cache/caches/read_receipts.rs:139-165`; Normal priority,
    at most 20 batches). It pages back until it finds the event a receipt points at, so the
    unread count can be worked out.

  What the second and third mean for the app, which now runs them for every user, is in [The app
  always has the queue](#the-app-always-has-the-queue).
- **One run per room.** Requests for a room already queued or running at the same priority are
  joined to it (`back_pagination_queue.rs:247-281`); the scheduler never starts a second run for
  a room that has one (`:505-520`). A run ends at a batch that meets its stop condition, at the
  room's start, at an empty batch, or at its batch cap (`:583-615`).
- **`run_search_backfill(strategy)`** (`search_backfill.rs:94-182`) works like this:
  1. It returns at once if there is no queue (L95-97).
  2. Once per profile it indexes the events already stored (`reindex_stored_events`, L189-242).
     A marker file `reindexed-v1` in the index folder guards it (`search_index/mod.rs:66`). The
     marker is written only at the end, so an interrupted reindex starts over.
  3. It takes the joined rooms, most recent first (L246-250). For each week 1..=13 it takes them
     in chunks of 100. Each room becomes a Low request: it stops at a batch older than the week,
     takes 30 events a batch and at most 10 batches (L40-52, L263-270). It waits for a chunk
     before taking the next. Rooms that reach their start, or are skipped (spaces, rooms not
     joined), drop out (L163-171).
  4. `Foreground` waits 1 s before each room after the first of a chunk. `Background` does not
     wait (L57-74).
- **No progress API.** There is only an `info!` at the start and at the end, and a `debug!` per
  week.
- **Cancelling.** Dropping the last `BackPaginationHandle` cancels its request
  (`back_pagination_queue.rs:143-182`). A run checks between batches (L584), so the request
  in flight completes and nothing more goes out. Dropping the `run_search_backfill` future
  drops its handles, so aborting our task is the cancel.
- **Indexing.** What lands in a room's stored history is indexed, including what is paged back
  (`event_cache/caches/room/updates.rs:150-166`, `tasks.rs:449`).

### When it runs, and its bounds

- **Start.** Once per session, `START_DELAY` (10 s) after the room list first catches up, if the
  setting is on. The two places are `src/matrix/mod.rs:1937-1941` and `:2219-2225`. The delay
  lets the room list, the previews and the open room's first page go first; the once-only
  reindex reads the whole store.
- **Again, after an outage.** Requests that fail while the network is down leave rooms short: a
  failed room is not marked done, but its week is passed. So one catch-up run follows an
  outage. Never two runs at once:
  - While a run is going, an Offline sync state sets `saw_offline`.
  - When the run ends with `saw_offline` set: if the sync is Running, the catch-up starts at
    once; if it is still Offline, it starts at the next Offline → Running.
  - On Offline → Running when no run is going and the last one ended with `saw_offline`: the
    catch-up starts at once (no `START_DELAY`: the room list is up already).
  - A run that starts clears `saw_offline`. So a catch-up is itself caught up only if it, too,
    sees an outage: one run per outage at most.
  - The setting turned off clears it all; turned on, it starts a run unless one has finished
    (`Done`) in this session. A run stopped by the setting has not finished.

  These rules are one small function of (run going, `saw_offline`, sync state, event), so they
  are tested without a server (below).
- **Time.** 13 weeks back from now, as the SDK has it. We do not change it. A very busy room
  (more than 300 events in a week) stops at the cap for that week and is covered less deep; the
  next start goes further from what is stored.
- **Rooms.** All joined rooms, the most recent first. The SDK skips spaces, invites and left
  rooms.
- **Bandwidth.** Foreground pacing (one room a second into the queue), Low priority, 3 runs at
  once shared with the other two backfills, at most 130 requests per room per run. The other
  two go first (High and Normal beat Low), so at start the search waits behind them.
- **Time limit.** Our task gives up after 6 h; see the risks: a panic in a queue task makes
  `join()` wait for ever.
- **The dev server in tests.** Only the tests about it turn it on, through the worker options
  below. The other server tests keep it off, so they neither wait for it nor load the server.
  Test accounts are new, with one or two rooms, so a run takes seconds. The tests of previews
  and unread dots build their client with the queue, as the app does (see [The app always has
  the queue](#the-app-always-has-the-queue)).

### Cost

The figures here are reckoned from the SDK's caps and guessed sizes. The real ones come from the
dev-server measurement in the next section, before the release.

- **Requests.** One `/messages` of 30 events per batch, and only for what is not stored yet.
  A quiet room needs 1–2 requests; a room with 3 000 messages in three months needs about 100;
  never more than 130 per room and run. An answer is roughly 30–60 KB (a guess). In encrypted
  rooms add one key-backup request per unknown megolm session when decryption fails
  (`BackupDownloadStrategy::AfterDecryptionFailure`, `src/matrix/session.rs:148-154`;
  `matrix-sdk/src/room/mod.rs:1984, 2027`).
- **Later starts.** As far as I read `run_request` (`back_pagination_queue.rs:540-617`), it
  subscribes to the room for the run and drops it at the end, so the room's history in memory
  shrinks back (L547-552). The next week's run reads the stored part from disk again before it
  goes to the server. So a later start costs mostly disk reads, and requests only past the end
  of what is stored.
- **Disk.** What is fetched stays in the SQLite event cache (encrypted) and in the search
  index. At a guessed 1–2 KB per stored event, 100 000 events are 100–200 MB; the measurement
  replaces the guess.
- **Memory.** Shrinks back after each room (above). A room open in the app keeps what was
  fetched for it while it is open.
- **Duration.** Foreground with 100 rooms: at most 100 s of pacing per week, so at most about
  22 minutes for a first run, plus the requests themselves. It gets shorter week by week as
  rooms reach their start.

### Measuring before the release, on the dev server

**Not on a profile copy.** The copy of the user's 1.0.3 profile (the upgrade test of PLAN §6.1)
is opened offline only: in a namespace with no network, it checks the store migration, that the
profile opens, and that no login form shows. It cannot measure the fetch. A copy of a
logged-in profile is the same device to the server; if it synced, it would take the to-device
messages (room keys) meant for the user's real client.

**The measuring test.** `search_history_cost` in `src/matrix/search_history.rs`, `#[ignore]` and
run only when `MATRIX_RAT_MEASURE=1` is set, so the usual server run neither waits for it nor
fills the dev server. It runs alone, like every server run.

1. **Fill.** A (a plain SDK client) makes rooms of known sizes, about 10 000 events in all,
   messages of 100–200 characters, sent a few at a time:
   - `busy`: 5 000 messages, more than one run can take (3 900);
   - two `medium` rooms of 1 000;
   - five `quiet` rooms of 20, which one sync brings whole;
   - `state-only`: one message, then 2 000 topic changes, for the latest-event walk below;
   - optionally an encrypted room of 1 000 with a key backup, for the decryption cost.
2. **Simulated weeks.** First choice: the spec lets an application service set an event's time
   with the `ts` query parameter of `/send` and `/state`. If continuwuity honours it (to be
   checked; it needs an appservice registration in the dev server's config), A spreads each
   room over 16 weeks; the busy room then has over 300 a week, so every week meets its cap.
   Otherwise every event is sent now. Then each week's request of a room that still has older
   events meets its 10-batch cap, so every room costs the most it can: `busy` 130 batches
   (3 900 events), a `medium` room all of its 1 000 by week 4. That is the upper bound, which is
   what the estimate needs; the 13-week stop itself is covered by the SDK's tests.
3. **Count.** B logs in with the worker on a new profile in a temp folder, through a counting
   relay: a test-only TCP relay in front of the dev server that forwards bytes and counts
   request lines, all of them and the `GET …/messages` ones apart. It counts kinds only; no path
   or id is kept.
4. **Measure, in three steps.**
   - With `WorkerOptions::app(…, false)` (search history off): what the other two backfills
     cost at start, until no `/messages` has gone out for 10 s.
   - Then `MatrixCmd::SearchHistory { enabled: true }`, which also proves that turning it on
     mid-session fetches: `/messages` count, time from `Running` to `Done`, and the growth of the
     profile folder (event cache and search index).
   - Then a second start on the same profile: requests and time again, to check that a later
     start costs mostly disk reads.
5. **Report.** One line of numbers: requests, seconds, bytes, events fetched (known from what
   was sent and the caps, checked against requests × 30), bytes per event and seconds per
   request. No ids, paths or text. It asserts only loose bounds (at most 130 search requests per
   room and run; at most 20 read-receipt batches per room and sync), since times on a shared dev
   server vary.

**The estimate for 100 000 events**, worked out from those numbers and written here and in the
release notes:

- requests: 100 000 / 30 ≈ 3 400, plus about one per room and week that ends short;
- disk: 100 000 × the measured bytes per event;
- time: the pacing (up to rooms × 13 s) plus requests × the measured seconds per request / 3. The
  dev server is local, so its seconds per request are a floor; the estimate also gives the time
  at 0.5 s a request, a guess for a busy public server, said to be one.

**The gate.** "Measure before release" stays, on the dev server: the test runs before the 1.1.0
release and its numbers replace the guesses above. Proposed: if 100 000 events come to more than
1 GB of disk or more than an hour at 0.5 s a request, the default goes back to the user before
the release.

### Encrypted rooms

Fetched events that cannot be decrypted yet are stored as such. When their keys come (from the
key backup, forwarded by another device, or found by the redecryptor), the SDK indexes the
decrypted events itself (`matrix-sdk/src/event_cache/redecryptor.rs:533-542`: saving them does
not reach the index, so it sends them there explicitly). Nothing to do on our side: search finds
them once their keys are in. Until then they are not found, because the index holds only
decrypted text. Rooms whose keys never come stay unsearchable, as today.

### The app always has the queue

**Decided** (matrix-claude, on matrix-agy's review, 2026-11-27; the review's "Option A"). The
interactive app always builds its client with the back-pagination queue. The `search_history`
setting governs only the three-month search backfill. The latest-event and read-receipt
backfills run in the app whatever the setting, and stay off in `serve`.

**Why.** The queue is made when the client is built, and cannot be added later. Built only when
the setting is on, an app started with it off would, on turning it on, call
`run_search_backfill`, which returns at once without a queue (L95-97), and we would say "Done"
having fetched nothing. With the queue always there, turning the setting on mid-session really
fetches.

**What that means for the other two backfills, which now run for every 1.1 user:**

- **Latest-event backfill.**
  - *Which rooms:* only those the SDK is asked to track: the top 500 of the room list
    (`LATEST_ROOMS`, `src/matrix/mod.rs:113`), when room previews are on (`Previews::listen`,
    `:2497-2528`). With previews off, none. We do not use the room list service's
    `subscribe_to_rooms`, which would add its own.
  - *When:* a tracked room's loaded events hold nothing to show as its latest message. Messages,
    stickers, poll starts and calls count, and so do our own join or invite and a knock we may
    answer; edits, reactions, deletions, verification requests and other state events do not
    (`latest_event/builder.rs:713-870`).
  - *Bounds:* High priority, detached, no batch cap. It stops at the first batch that holds
    something to show, at the room's start, or at an empty batch; one run per room at a time.
    The unbounded case is a long room with no message in it (say, a bridged room of membership
    changes): it is walked to its start. What it fetched is stored, so a later start reads it
    from disk.
  - *On screen:* a room whose last 20 events hold no message gets a preview, where today its
    line has none. Better, not worse.
- **Read-receipt backfill.**
  - *Which rooms:* any joined room whose sync brings timeline events while our own read receipt
    points at an event that is not loaded.
  - *Bounds:* Normal priority, detached, at most 20 batches of 30 (600 events) per run; it stops
    as soon as the receipt's event is loaded. A later sync of that room asks again if the event
    is still not found; while one run is queued or going, the request joins it. As far as I read
    (`back_pagination_queue.rs:547-552`), a later run reads the stored part from disk first, and
    its batch count includes those, so the network cost of a room is about 600 events past what
    is stored. The measurement checks it.
  - *On screen:* little. We show unread as a dot, from `num_unread_notifications() > 0 ||
    num_unread_messages() > 0` (`src/matrix/mod.rs:2623-2632`), not the count. When the receipt
    is not loaded, every loaded event counts as unread, so the dot is on either way; the
    backfill only makes the hidden count exact. Mostly a cost, then.
- **Sharing the slots.** The three share 3 runs at once, High before Normal before Low. At start
  the two side backfills go first and the search waits; `START_DELAY` gives them a head start.
  Our own scrolling back (`src/matrix/paging.rs`) does not go through the queue.

**Risks:**

- Traffic right after start, for every user and even with search history off: up to 500
  latest-event walks (only for rooms without a message in their last 20 events) and up to one
  20-batch walk per room with an old receipt. The measurement's first step counts it.
- A long room without messages takes a High slot until its walk ends; the search waits. The
  `state-only` room of the measurement (2 000 topic changes) shows how long.
- A panic in one of their runs leaks a slot for the session, as for the search (see the risks
  below). Our paging's catch (`src/matrix/paging.rs:64-90`) does not cover detached runs.
- Previews and unread dots may change; the tests below pin what they should be.

**Tests:**

- Unit: `WorkerOptions::app(_, true)` and `app(_, false)` both have `back_pagination`;
  `WorkerOptions::agent` has neither switch. With the flag the client's event cache has a queue,
  without it none (see Tests below).
- Unit, fake homeserver, with a `/sync` fed through `sync_once` as in
  `src/matrix/rooms.rs:3263`: a room whose own receipt points at an event the server never
  returns. The server sees at most 20 `/messages` for it after one sync, and the room's `unread`
  is what it is without the queue.
- Server: `a_preview_comes_from_older_messages`. B (a plain SDK client) joins A's room. Then A
  sends a message with a unique word, then 40 topic changes, so B's join is older than the
  message. B logs in again with the worker and `WorkerOptions::app(…, false)`. Within 10 s,
  B's room list line shows the message. (With `WorkerOptions::agent` it does not: the line
  stays as today.)
- Server: the tests of previews and unread dots run with the app's options, as users run them:
  `rooms_come_with_their_latest_message` (`src/matrix/mod.rs:3053`),
  `a_room_marked_unread_shows_in_the_users_other_clients` (`src/matrix/rooms.rs:5366`) and
  `the_read_marker_stands_before_what_was_not_read` (`:5583`). They must pass unchanged.
- The measuring test's first step (above) gives the numbers.

### Not in `serve`

`matrix-rat serve` keeps the queue off:

- it never searches (its MCP tools read, wait and send);
- the flag would also start the latest-event and read-receipt backfills for every room, in a
  daemon that cares about the one or two rooms it is bound to;
- it would grow a store and an index that nobody searches.

So the switch is a worker option: the app always turns the queue on, `serve` and most tests
leave it off.

### Our changes

1. **Worker options.** `matrix::spawn_with(paths, secret_store)` (`src/matrix/mod.rs:1439-1456`)
   takes `WorkerOptions { secret_store, back_pagination, search_history }`, and `Worker` keeps
   them.
   - `WorkerOptions::app(secret_store, search_history)` is for the terminal shell
     (`src/shell/terminal.rs:491`, with the setting from the config). It always sets
     `back_pagination: true`; `search_history` only says whether the fetch runs.
   - `WorkerOptions::agent(secret_store)`, with both off, is for `serve`
     (`src/agent/serve.rs:1249, 1367`) and for the existing tests: `spawn(paths)`
     (`src/matrix/mod.rs:1427`) passes it, and so do `src/matrix/mod.rs:3566, 3628, 3683, 3741`
     and `src/matrix/agent.rs:1464`. The three server tests named above pass `app(…, false)`.
   - `back_pagination` is fixed when the client is built; `search_history` can change at any
     time (`MatrixCmd::SearchHistory`).
2. **Client builder.** `offline_client` (`src/matrix/session.rs:189-221`) gets
   `back_pagination: bool` and calls `.with_enable_automatic_back_pagination(…)`.
   - It is reached through the functions that build the client of a stored session: `restore`
     (`:378`, `restore_with` `:442`), the login commit (`:507`) and `commit_preserved_reauth`
     (`:550`).
   - They take `&WorkerOptions` in place of the bare `SecretStoreMode`.
   - The browser's stubs (`src/matrix/auth.rs:107+`) take it and ignore it.
3. **The run.** A new `src/matrix/search_history.rs` beside `search.rs`.
   - `SearchHistory::start(client, events)` spawns a task in the session's `JoinSet`
     (`src/matrix/mod.rs:2121`).
   - The task waits `START_DELAY`, emits `Running { back_to }`, runs
     `rt::timeout(6 h, cache::of(&client)?.run_search_backfill(SearchBackfillStrategy::Foreground))`,
     then emits `Done` or `Stopped`. (`cache::of` is `src/matrix/cache.rs:20`; 0.20's event
     cache is an `Option`.)
   - `stop()` aborts the task.
   - In tests, `START_DELAY` is 0.
4. **Session loop** (`src/matrix/mod.rs`).
   - It starts the run where the room list first catches up, when the option is on.
   - `MatrixCmd::SearchHistory { enabled }` starts or stops it, by the rules in "Again, after an
     outage" above. Outside a session the command updates the worker's option, instead of being
     dropped with the others at `:1700-1737`.
   - `stop()` runs at teardown next to `searcher.cancel()` (`:2347`), on quit, logout and
     expiry alike. The `JoinSet`'s `abort_all` (`:2351`) would do it as well.
   - The sync arm (`:2199-2217`) feeds the sync states to the catch-up rules. The run's end comes
     back to the session loop on a channel the loop selects on, so the loop, not the task, starts
     the catch-up and there are never two runs at once.
5. **Protocol.**
   - `MatrixEvent::SearchHistory(HistoryRun)`, with
     `HistoryRun { Off, Waiting, Running { back_to }, Done { back_to }, Stopped }`.
   - `back_to` is the date of now − 91 days, written by a new `short_date(when, now)` in
     `search_history.rs`: `%-d %b` ("28 Aug") when it is in the same year as now, `%-d %b %Y`
     ("11 Nov 2026") when not. `convert::day_label` (`src/matrix/convert.rs:550-557`) does not
     fit: it writes `%a %-d %b %Y` ("Fri 28 Aug 2026"), and "Today"/"Yesterday". The helper stays
     out of `convert.rs`, which items 2 and 3 change. The worker makes the text, so the app needs
     no clock arithmetic.
   - Both go in the hand-written Debug lists (`:495`, `:1139`). They hold no content anyway.
6. **App.**
   - Two fields: `App.search_history: bool` (the setting) and `App.history_run: HistoryRun`.
   - `Setting::SearchHistory` in `src/modal.rs` (`Setting::ALL`, `:603`, goes from 34 to 35),
     labelled "Search history", with the value "3 months" or "off".
   - `src/config.rs`: `search_history: bool`, true by default.
   - On a change: `save_setting("search_history", …)` and `MatrixCmd::SearchHistory`, as
     `link_previews` is handled (`src/app.rs:1280, 1401-1404`).
7. **Search box** (`src/ui/global_search.rs`).
   - `DEVICE_HINT` (`:21`) becomes a function of `app.history_run`.
   - The empty-state line (`:123-141`) and the note row (`:101`, `:244-255`) change as in the
     mock-ups.
   - `GlobalSearch` (`src/app/global_search.rs`) stays as it is.
8. **Docs.**
   - README: the search and settings parts.
   - Release notes: "Until messages are loaded again, search finds fewer of them"
     (`docs/RELEASE-NOTES-1.1-draft.md:22-23`, note at `:91-94`) becomes: the first start
     indexes the stored messages again and fetches three months of history in the background
     (with the costs measured on the dev server, and how to turn it off).

### States, keys, settings

One setting (`search_history`, Behaviour group, on by default: the user's decision, see
[Decisions](#decisions)). It governs only the three-month fetch; the app has the queue either
way. No new key. The new state is `HistoryRun`, kept in `App` and shown only in the search box;
the worker keeps `saw_offline` and whether a run has finished.

### Log privacy

- **Ours.** Counts and times only:
  - `info!(rooms, "search history started")`;
  - `info!(elapsed_s, "search history done")`;
  - `info!("search history catches up after an outage")`;
  - `warn!("search history stopped at the time limit")`.
  No room ids, no query and no error text: an error is logged by its kind only
  (`logsafe::kind`).
- **The SDK's.**
  - `search_backfill.rs` and `back_pagination_queue.rs` log room ids at debug and trace.
  - They also log `warn!(%room_id, "…: {err}")` with the raw error (`search_backfill.rs:149, 213,
    229`; `back_pagination_queue.rs:567, 574, 591`).
  - With today's `DEFAULT_FILTER` (`warn,matrix_rat=info,…`, `src/logging.rs`) those warnings
    reach the log file.
  - The planned 1.1 change to `off,matrix_rat=info` (matrix-agy, review-sdk-0.20-2.md) closes
    this. Item 1 lands after that change, or it adds `matrix_sdk::event_cache=off` itself.

### Browser build

- `run_search_backfill` compiles for wasm: only the reindex sits behind `experimental-search`
  (L101-102). But the browser has no search, and its client (phase 2) is built without the
  queue.
- The native part of `search_history.rs` is `#[cfg(not(target_family = "wasm"))]`. In the
  browser, `start` emits nothing, like the search stub (`src/matrix/search.rs:329`), which
  answers that search is not available.
- The protocol types and the calls in the session loop exist on both targets, so no field is
  left unused under `-D warnings`.
- Gate: `RUSTFLAGS="-D warnings" cargo check --lib --target wasm32-unknown-unknown`.

### Tests

- **Unit.**
  - `src/matrix/session.rs:1035` (`sqlite_store_config_caps_pool_and_opens_offline_client`)
    is extended. With the flag, the client's event cache, once subscribed, has a queue
    (`back_pagination_queue().is_some()`). Without the flag it has none.
  - `search_history.rs`, using the fake homeserver (`src/matrix/spaces.rs:1349`,
    `fake::Server::answering`) with one joined room, three cases:
    - `/messages` answers an empty chunk with no `end`. The run ends; the events are
      `Running`, then `Done`.
    - `/messages` answers `Reply::Never`. `stop()` ends our task within a second, and no
      further `/messages` reaches the server. The one already in flight inside the queue stays
      parked until the client is dropped, so the test drops the client before it waits for the
      fake server to shut down.
    - Started with `WorkerOptions::app(…, false)`, then `SearchHistory { enabled: true }`: a
      `/messages` reaches the server, and the events are `Running`, then `Done` (the review's
      Catch #1: on mid-session really fetches).
  - The catch-up rules, as a plain function: Offline during a run gives one catch-up when the
    run ends (at once if the sync is Running, else at the next Running); Offline → Running after
    the run ended with `saw_offline` starts it at once; no Offline, no catch-up; never a second
    run while one goes; off clears it; on after `Done` starts nothing.
  - `short_date`: same year ("28 Aug"), across New Year ("11 Nov 2026"), and the 91
    days from a date in February.
  - `WorkerOptions::app(_, false)` has `back_pagination`; `WorkerOptions::agent` (what `serve`
    passes) has both switches off.
  - App: the setting switches, is saved and sends `SearchHistory { enabled }`. The config
    default and parsing are tested. `HistoryRun` events change the box's text.
- **Snapshots.**
  - New: `global_search_history_running`, `global_search_history_done`.
  - `global_search_empty` stays as it is (Off).
  - The settings snapshot gains the row.
- **Server** (`--ignored`): `search_finds_messages_older_than_the_first_sync`.
  1. B (a plain SDK client) joins A's unencrypted room.
  2. A sends 60 messages; the first one holds a unique word.
  3. B logs in again with the worker and `WorkerOptions::app(…, true)`: a new device with an
     empty store.
  4. The room list brings the last 20 messages, so the first message comes only from the run.
  5. B waits for `HistoryRun::Done`, searches, and finds it.
- **Server, encrypted** (optional, if it stays under a minute). A is a plain SDK client with
  recovery and a key backup, as in `history_unlocks_after_verification`
  (`src/matrix/verify.rs:2393`), and sends 60 encrypted messages. B enters the recovery key
  (`MatrixCmd::Recover`). The first message is found once its key is downloaded.
- **The other two backfills:** see the tests in [The app always has the
  queue](#the-app-always-has-the-queue).
- **The cost:** `search_history_cost`, by hand before the release (see [Measuring before the
  release](#measuring-before-the-release-on-the-dev-server)).
- **Not tested here: the 13-week bound**, unless continuwuity honours the appservice `ts` (see
  the measuring test). The SDK's own tests cover `stop_when_older_than`.

### Size and risks

Size **M**, and **S** for the measuring test and its relay.

- **The other two backfills come with the queue, for every user.** Their bounds, risks and tests
  are in [The app always has the queue](#the-app-always-has-the-queue); their start-up cost is
  the measuring test's first step, on the dev server. A release note says that previews now
  reach back for rooms whose last events hold no message.
- **A panic inside a queue run never reports `Finished`.** This is the #5416 kind of panic that
  `src/matrix/paging.rs:64-90` catches for our own paging (scheduler:
  `back_pagination_queue.rs:354-420`). That room's slot stays taken, and its `join()` waits for
  ever.
  - Our 6 h limit ends our run, and the box says Stopped.
  - The queue may run with fewer slots for the rest of the session. Nothing more can be done
    from outside.
- **Disk grows without a cap.** Store sizes come to the doctor in 1.3.
- **An open room gets older events added at the top while the user reads it.** The paging state
  (`src/matrix/paging.rs`) and the selection must stay right. The server test opens the room
  while the run fetches it.

## 2. Failed edits, reactions and deletions: try again or discard

### How each kind fails, and shows, today

Sending goes through the room's send queue. A failure comes in one of two kinds (classified at
`matrix-sdk/src/send_queue/mod.rs:1114-1131`):

- **Recoverable**: a network error, or 5xx/429 after three tries (`RequestConfig::short_retry`,
  `:1215`). The room's queue is turned off and the request is kept.
- **Unrecoverable**, for example 403: the request is "wedged".

Only the head of a room's queue is ever sent (`:1649-1663`). So a wedged request blocks
everything queued after it in that room until it is retried or removed. It is stored, so a
restart does not help.

| Kind | Shows today | What the user can do today |
|---|---|---|
| Message | "✕ not sent: reason" (`src/ui/timeline.rs:181`, `src/model.rs:759`) | Enter tries again (`src/app.rs:3245-3261` → `MatrixCmd::Retry` → `src/matrix/rooms.rs:1430-1447`); Del discards (`src/app.rs:2995-3030` → `Redact` on the local echo, which aborts it); menu "Try sending again" / "Discard" (`src/menu.rs:433, 625`); status line "try again · discard" (`src/ui/status.rs:194-198`) |
| Edit | The new text with "(edited)", as if sent: `src/matrix/convert.rs:582-586` reads only `send_state()`, which a remote item does not have | Nothing. Later messages in the room stay "◌ sending" |
| Reaction | Our chip, as if sent | Clicking the chip removes it (that aborts the local echo, `matrix-sdk-ui/src/timeline/controller/mod.rs:642-655`), by luck |
| Deletion | "Message deleted", though the server and everyone else still have it | Nothing |
| Removing a reaction | Not queued: sent at once (`controller/mod.rs:688`). On failure the SDK puts the chip back and we toast "Could not react: …" (`src/matrix/rooms.rs:1249-1265`) | Click again. This stays as it is; only the toast becomes "Could not remove the reaction". |

### What the user sees

Own messages in the timeline:

```
● you  12:00
  Fixed the typo (edited) ✕ edit not sent: not allowed in this room
  Message deleted ✕ deletion not sent
  Ship it
   👍 1 ✕
  And one more ◌ waiting
```

- **Markers.** The "✕ …" part uses `Role::Error`, like "✕ not sent"; "(edited)" and the other
  badges before it keep their muted look. A failed reaction's chip shows `✕` after its count.
- **On others' messages too.** A failed reaction of ours, or a failed deletion of someone else's
  message by us as a moderator, sits on a message that is not ours. It shows and is handled the
  same way.
- **"◌ waiting"** marks a message of ours queued after a failure in the same room. It goes out
  once the failure is retried or discarded. Today it says "◌ sending" for ever.
- **Status line on such a message:** `Enter try again · Del discard edit`. For a failed deletion
  it says `keep message`, for a failed reaction `remove reaction`, and `discard` if several
  things failed.
- **Menu.**
  - At the top: "Try sending again" (Enter).
  - Then one of "Discard edit", "Keep the message", "Remove the reaction" or "Discard what was
    not sent" (Del).
  - The usual "Delete" / "Delete…" items are left out while something failed: Del does the
    discard, and a deletion would wait behind the failure anyway.
- **Details (`i`).** The Delivery fact gains lines: "Edit: not sent: reason", "Deletion: not
  sent: reason", "Reaction 👍: not sent", "Waiting for a failed change before it".
- **Discarding an edit** brings back the text from before it. The SDK takes the latest sent edit
  or the original text (`unedited_kind`, `controller/aggregations.rs:722-750`). The failed text
  is gone; `e` edits again.
- **Keeping the message** (discarding a failed deletion) brings the message back as it was
  (`aggregations.rs:412-420`, `unredact`).
- **After a network outage**, when the sync is back (Offline → Running), every room's queue is
  turned on again (`SendQueue::set_enabled(true)`, `send_queue/mod.rs:346`). The failures the
  SDK took as passing then go out by themselves. Unrecoverable ones wait for Enter or Del.

### Keys and mouse

- The keys are those of a failed message: Enter (`reply`) tries again and Del (`delete`)
  discards. Each acts on every failed part of the selected message at once. There is no new key
  and no new action.
- Del on a failed deletion discards the deletion, so the message comes back, because Del throws
  away whatever did not go out; the status line names it ("keep message"). The user decided so
  (see [Decisions](#decisions)).
- This holds on others' messages as well: Del on a message with a failed part discards it,
  whoever sent the message, before the usual "only your own messages" rule (change 5).
- `EditLast` (`src/app.rs:3286`) and the palette's "Try sending again" (`src/palette.rs:162,
  1453`; `src/app/palette.rs:59`) count a message with a failed part like a failed message.
- Mouse:
  - the message's menu (right click) has the entries above;
  - clicking a failed reaction chip removes it (the toggle already aborts a reaction that has
    not gone out);
  - the markers cannot be clicked, like today's "✕ not sent".

### The SDK calls

- **Item state:**
  - `EventTimelineItem::send_state()` (`matrix-sdk-ui/src/timeline/event_item/mod.rs:253`);
  - `edit_send_state()` (`:273`; failed beats pending, which beats sent);
  - `redaction_send_state()` (`:259`);
  - `ReactionInfo { timestamp, send_state }` (`:876-883`);
  - `EventSendState::SendingFailed { error, is_recoverable }` (`event_item/local.rs:68-92`).
- **Actions:** `Timeline::retry_send(&item_id, SendTarget)` (`timeline/mod.rs:849-861`) and
  `abort_send` (`:866-876`), with `SendTarget { Event, Edit, Redaction, Reaction { key } }`
  (`:113-128`). Both return `Ok(false)` when nothing of that kind is pending any more.
- **`set_enabled(true)` after a retry is still needed in 0.20.**
  - A recoverable failure turns the room's queue off (`send_queue/mod.rs:1137`).
  - `unwedge` (`:3377`, then `:1410-1427`) clears the wedge and wakes the task, but leaves the
    queue off.
  - The task checks that flag before it sends (`:786`).
  - `retry_send`'s documentation says the same (`timeline/mod.rs:841-845`).
  - After an unrecoverable failure the queue stays on since 0.20, so there the call does no
    harm.
  - Our comment at `src/matrix/rooms.rs:1441-1444` stays right.

### Our changes

1. **Model** (`src/model.rs`).
   - New `Message` fields:
     - `edit_error: Option<String>` and `delete_error: Option<String>`: like `send_error`, why
       it failed, in a few words;
     - `held: bool`.
   - New `Reaction.failed: bool`.
   - `Message::failed_any()`.
   - The hand-written Debug (`:179-199`, `:69-73`) only says whether each is set.
2. **Convert** (`src/matrix/convert.rs:560+`).
   - The fields are filled from the item:
     - `edit_send_state()` failed → `edit_error`, from `upload::failure_reason` as at
       `:606-608`;
     - `redaction_send_state()` failed → `delete_error`;
     - our own reactions' `send_state` failed → `Reaction.failed`.
   - `held` is not set here. It is set in `prepare_messages` (`src/matrix/rooms.rs:2468+`), right
     after `reuse.convert(…)` (`:2481`): if any message has a failed part, our messages still
     `Sending` get `held`, because a failure blocks or stops the room's whole queue; otherwise
     none has it. It depends on other items, so it cannot live in `event_message`: when an older
     item's edit fails or is cleared, a newer `Sending` item keeps its `Arc`, and `Reuse`
     (`src/matrix/rooms/reuse.rs`) hands back its cached `Message`. A pass after `Reuse` keeps
     `Reuse`'s rule (a message depends on its item only) and still updates `held`.
3. **Worker** (`src/matrix/rooms.rs`).
   - `MatrixCmd::Retry { room_id, item }` keeps its name and now tries every failed part of the
     item again. It looks the item up (as at `:1433-1440`), calls `retry_send` for each failed
     part (`Event`, `Edit`, `Redaction`, `Reaction { key }`), then
     `room.send_queue().set_enabled(true)` once.
   - The new `MatrixCmd::Discard { room_id, item }` calls `abort_send` for every failed part
     except the message itself, then `timeline.room().send_queue().set_enabled(true)`. A failed
     message keeps going through `Redact` as today (`src/app.rs:3013-3024`), and the worker's
     `Redact` (`src/matrix/rooms.rs:1267-1290`), when it aborts a local message that failed,
     calls `set_enabled(true)` after it as well.
   - Why both: `SendHandle::abort()` only wakes the room's task (`notifier.notify_one()`,
     `send_queue/mod.rs:3256-3266`); it does not turn the queue on. After a recoverable failure
     the queue is off (`:1137`), so without the call the task wakes, finds it off (`:786`) and
     sleeps again: the messages behind the discarded one lose `held` and stay "◌ sending" until
     the next Offline → Running or restart. After an unrecoverable failure the queue is on, and
     the call does nothing.
   - When nothing is found: "Nothing waits to be sent here any more" (today: "this message is
     no longer waiting").
   - `Discard` goes into the command's Debug next to `Retry` (`src/matrix/mod.rs:569`) and into
     the list dropped outside a session (`:1700-1737`).
4. **Back online** (`src/matrix/mod.rs:2199-2217`). The sync arm remembers the last state. On
   Offline → Running it calls `client.send_queue().set_enabled(true).await`.
5. **App.**
   - `selected_failed()` (`src/app.rs:2569`) and the guard of the retrying `Action::Reply`
     (`:3246-3252`) use `failed_any()` in place of `state == Failed`.
   - Enter sends `Retry`, as today.
   - `delete_message` (`:2996-3012`): the check for a failed part comes **first**, before the
     `!message.own` guard (which opens the reason popup or toasts "You can only delete your own
     messages"). A failed reaction or a failed moderator deletion sits on someone else's
     message. When a part of a sent message failed, Del sends `Discard`; otherwise Del works as
     today.
6. **UI.**
   - `marker()` (`src/ui/timeline.rs:181-202`) returns its parts, each with its role, in place of
     one `(String, Role)`:
     - the badges as today ("(edited)", "(pinned)", the shield), in their role;
     - `edit_error` → "✕ edit not sent[: why]", in `Role::Error`;
     - `delete_error` → "✕ deletion not sent[: why]", in `Role::Error`;
     - `held` → "◌ waiting" in place of "◌ sending", muted.
   - Its three callers (`:761`, `:856`, `:1262`) put the parts after the text, joined by
     spaces, when they fit, and on a row of their own when not, as today; the width check
     takes them all. With item 3, a deleted line leaves room for the marker (item 3, change 4),
     so "✕ deletion not sent" stays in view under a long reason.
   - `hash_message` (`src/ui/timeline/memo.rs:581`) hashes `edit_error`, `delete_error`, `held`
     and each `Reaction.failed`, or the rows of a changed message are not drawn again.
   - The chips (`:935-980`) add " ✕" in `Role::Error` for `failed`.
   - The status hints (`src/ui/status.rs:194-198`) name the part.
   - The menu (`src/menu.rs:415+`) and the details' Delivery fact (`src/details.rs:89-93`)
     change as described above.
7. **README.** The note on failed messages, plus a line under Good to know: "a change that
   could not be sent".

### States

No new popup, key or setting. The new state is the per-message fields above; the new command
is `MatrixCmd::Discard`.

### Log privacy

- **Ours.** A failed retry or discard takes `with_timeline`'s existing path: a toast with a short
  reason, and a log line with the error's kind only. No ids, and never a reaction's key: that is
  text the user chose, which `src/model.rs:68-73` already keeps out of Debug.
- **The SDK's.** The send queue warns with the transaction id and the raw error
  (`send_queue/mod.rs:1140, 1152`). It does so today already; the same `DEFAULT_FILTER` change
  closes it.

### Browser build

Nothing native: the timeline and send-queue calls exist in the browser too (matrix-sdk-ui with
`js`), and `SendQueue::set_enabled` is fine there. The gate as usual.

### Tests

- **Unit, worker side**, with the fake homeserver as in `src/matrix/rooms.rs:3230+`:
  - An edit answered 503 until told otherwise, a message sent after it `held`. After the SDK's
    three tries the edit has failed and the room's queue is off. Then `Discard`: the old text is
    back, and the held message goes out with no `Retry` (the `set_enabled` after `abort_send`).
  - The same with a failed message in place of the edit, and Del (`Redact` on it): the held
    message goes out.
  - An edit answered 403 (`M_FORBIDDEN`): the message has `edit_error`, and a message sent after
    it is `held`. Then `Retry`, with the server now answering 200: `edit_error` is gone, and the
    held message goes out.
  - The same, then `Discard`: the old text is back, `edited` is false, and the held message goes
    out.
  - A deletion answered 403: `delete_error`, body "Message deleted". `Discard` brings the text
    back; `Retry` deletes it for good.
  - A reaction answered 403: `Reaction.failed`. `ToggleReaction` on it removes it, and nothing
    is sent.
  - Recoverable: `/send` answers 503 until told otherwise. After the SDK's three tries the
    message has failed and the room's queue is off. Then the server answers 200 and `Retry`
    sends it (the `set_enabled` case).
  - The back-online reaction is one small function. It is called directly with Offline, then
    Running, and the waiting message goes out with no `Retry`. The fake server does not speak
    sliding sync, so the sync state cannot come from it.
- **App.**
  - Enter on a message with a failed part sends `Retry`; Del sends `Discard`.
  - On someone else's message with our failed reaction, and with our failed moderator deletion:
    Del sends `Discard` (not the reason popup, not the toast), and Enter sends `Retry`.
  - The menu entries and status hints depend on the part (extend `typing_notices_and_retry`,
    `src/app.rs:7706-7743`).
  - `EditLast` picks such a message.
- **Snapshots.** A new `failed_changes` snapshot with the lines in the mock-up. `message_kinds`
  stays as it is.
- **Server**: `failed_changes_can_be_retried_or_discarded`. This is how a send fails for real on
  the dev server: by permission, the same every time, with no network tricks.
  1. A creates a room and invites B. B sends a message.
  2. A raises `events_default` to 50 in the power levels.
  3. B edits: 403, the edit fails, and B's next message waits.
  4. A lowers the level again. B sends `Retry`, and both arrive: A sees the edit.
  5. The same again for a reaction: Discard removes it, and A never sees it.
  6. The same again for a deletion: Keep brings B's message back, and A still sees it.

### Size and risks

Size **M**.

- **`held` is worked out from what is loaded.** A failure in a part of the room that is not
  loaded is not seen. That is rare: the head of the queue is old only when a room was left
  alone after a failure. The message still goes out once the failure is handled.
- **`abort_send(Redaction)` on a deletion of a remote event** has to bring the message back with
  its content (`unredacted_item`). The server test confirms it.
- **Turning the queue on while still offline** after a discard: the next message fails again and
  shows "✕ not sent", as today. No loop: the queue turns itself off again.

## 3. A deleted message shows why

### What the user sees

Timeline:

```
● bob  12:00
  Message deleted: sent to the wrong room
● bob  12:01
  Deleted by carol: spam link
● bob  12:02
  Deleted by carol
```

Details (`i`), new facts after "Sent":

```
 From       bob
            @bob:example.org
 Sent       Today 12:00
 Deleted    by carol (@carol:example.org)
 Reason     spam link; see the room rules pinned at the top for what is
            allowed here
 Event ID   $…
```

- **Who.** The line names the person who deleted only when it is not the sender (a moderator),
  and says "by you" when it was us. When the sender deleted it, or we cannot tell, it says just
  "Message deleted".
- **Reason.** On one line, cut to the width with "…". The details popup wraps it, up to 500
  columns.
- **Our own deletions.**
  - Del still deletes our own message at once, without a reason: unchanged, and quick.
  - A new menu item and palette entry, "Delete with a reason…", on our own sent messages opens
    the existing reason popup (`src/app/moderation.rs:59-89`, snapshot
    `reason_confirm_redact`). The popup already works for our own messages and sends
    `Redact { reason }` (`src/matrix/rooms.rs:1267-1290`).
  - Others' messages need nothing new: when we may delete them, Del already asks for a reason
    (`src/app.rs:2998-3007`).

### The SDK calls

- **The reason:** `EventTimelineItem::redaction_reason()`
  (`matrix-sdk-ui/src/timeline/event_item/mod.rs:266`). It comes from a pending or a received
  deletion (`controller/aggregations.rs:293-314`). For an event that came already deleted, it
  comes from `unsigned.redacted_because` (`event_handler.rs:1093-1099`, `:1514-1530`).
- **Who.** The SDK keeps only the reason. But the stored deleted event carries the whole
  deletion event in `unsigned.redacted_because`, sender included
  (`matrix-sdk-base/src/room/room_info.rs:1416-1448`). The timeline puts that event into the item
  (`original_json()`, `event_item/mod.rs:516`) with the event cache's update
  (`event_handler.rs:1265-1300`).
  - We read `unsigned.redacted_because.sender` from it with `Raw::get_field`, the way the SDK
    reads the reason.
  - A deletion that has just come in may show its reason a moment before its name. Both usually
    come in the same batch.
  - While our own deletion is pending (`redaction_send_state()` is `Some`), it is ours.

### Our changes

1. **Model** (`src/model.rs`).
   - `Message.deleted: Option<Deletion>`, where
     `Deletion { reason: Option<String>, by: DeletedBy }` and
     `DeletedBy { Sender, Us, Other { user_id, name } }`. Unknown counts as `Sender`.
   - `body` stays "Message deleted", so previews, replies, notifications and agents do not
     change.
   - Debug says only whether there is a reason and which `DeletedBy` it is.
2. **Convert** (`src/matrix/convert.rs:782-787`) sets `deleted`. The reason is cleaned:
   `sanitize_line` (`src/text.rs:29`), runs of spaces made one, trimmed, none if `is_blank`,
   then `truncate(…, 500)`. `DeletedBy::Other` gets the user ID as its name for now; the name
   comes in the next step.
3. **Names.** `receipts::Names::annotate` (`src/matrix/receipts.rs:76`, called at
   `src/matrix/rooms.rs:2485`) also names whoever deleted a message, when that is not the sender,
   the way it names readers: from the loaded messages first, then the member store
   (`get_member_no_sync`). If neither knows them, it uses their user ID.
   - **`Names::plan`** (`src/matrix/receipts.rs:141-182`) decides whether to name anyone at all,
     from a digest of the receipts. A deletion that comes with no change of receipts would leave
     the digest as it was, and the deleter would never be named. So `Picked` gains the deleter
     (read from `original_json()`, see "Who" above), the digest hashes each
     `(message id, deleter)`, and the `needed` set includes the deleters. Items with a deleter
     and no readers are picked too (today only `total > 0`, `:96-100`).
   - `Names` keeps the deleters' names by message id beside `made`, and every pass writes them
     into `DeletedBy::Other.name` as it writes `read_by` (`:121-128`). So a message `Reuse` hands
     back gets the current name.
   - **Namesakes.** `Message::people()` (`src/model.rs:710-740`) also yields the deleter, so
     `mark_namesakes` (`src/matrix/rooms.rs:2489`) learns them, and `Namesakes::mark_among`
     (`src/model.rs:976+`) tells them apart like a quoted sender (`tell`). A moderator who shares
     a name with someone in view then shows with their ID, in lazy-loaded rooms too, where the
     SDK's `name_ambiguous()` is still false.
4. **Timeline** (`src/ui/timeline.rs:713-717` area). Today `MsgKind::Redacted` goes through
   `Rich::of(message.html, body_text)` and `highlight::colour`, which is harmless for the fixed
   "Message deleted". With someone's reason or name in it, Markdown in either (links, bold,
   code, spoilers, quotes) would be formatted, made clickable, or looked up for a link preview.
   So a deleted message is drawn plain: one `Span::styled(truncate(&line, room), Role::ReplyQuote)`,
   with no `Rich::of`, no `highlight::colour` and no link detection. `room` is the body width,
   less the marker's width and a space when there is a marker (item 2's "✕ deletion not sent")
   and both fit on the row; when they do not, the marker gets a row of its own, as today.
5. **Memo.** `hash_message` (`src/ui/timeline/memo.rs:581`) hashes `deleted`.
6. **Details** (`src/details.rs:156+`). "Deleted" and "Reason" facts after "Sent", as plain
   text.
7. **Delete with a reason.**
   - `Action::DeleteWithReason` ("delete_with_reason", Timeline, "Delete with a reason…")
     in `src/keymap.rs:195-217`. It has no default key, like `doctor` or `archive_room`, but
     can be bound in the key editor.
   - The menu (`src/menu.rs:625-630`) offers it on our own sent messages, after "Delete".
   - The palette shows it when Delete is shown (`src/palette.rs:1335`), for our own messages
     only.
   - The app calls `open_redact_confirm(index)`.
8. **README.** `delete_with_reason` (unbound by default) in the list of timeline actions
   (`README.md:419`), and the menu entry.

### Safety of the text

- The reason is text from someone else: it may be long and hold newlines, escape sequences or
  direction overrides.
  - `sanitize_line` removes control characters (so escape sequences) and the direction overrides
    (`src/text.rs:48-58`), and turns newlines into spaces.
  - The model caps it at 500 columns; the line cuts it to the width.
  - It is never parsed as Markdown or searched for links: the timeline draws it as one plain
    span (change 4), the details as plain text.
- It is never logged and never in Debug.
- It is not passed to `serve` or the MCP tools (`src/matrix/agent.rs:747` stays "redacted"):
  that would hand an agent text from anyone in the room, for little use.

### Moderator vs sender

| Who deleted | Line |
|---|---|
| the sender | "Message deleted[: reason]" |
| someone else | "Deleted by <name>[: reason]", the name as the timeline shows names |
| we, someone else's message | "Deleted by you[: reason]" |
| not known (no `redacted_because`: some servers, old events) | "Message deleted[: reason]" |

### Browser build

Nothing native. The gate as usual.

### Tests

- **Unit, convert.** Uses the fake homeserver as in `src/matrix/rooms.rs:3230+`, with a sync
  whose timeline holds deleted events with `unsigned.redacted_because`. Cases:
  - a reason with `\u{1b}[31m` and `\n` comes out on one line: no `\u{1b}` and no `\n` in it.
    The literal `[31m` stays: `sanitize_line` removes control characters, not the printable
    rest of an escape sequence, and that rest is harmless text;
  - 2 000 characters are cut;
  - a blank reason gives none;
  - deleted by the sender, by someone else, and by us;
  - no `redacted_because` gives `Sender`.
- **Unit, names.** A deletion that comes with no change of receipts still gets the moderator's
  name; a moderator who shares a name with a sender in view is shown with their ID.
- **Unit, UI side.**
  - The details facts.
  - A reason with `[x](https://example.org)`, `**b**` and `` `c` `` is drawn as it is written:
    no link among the row's hits, no bold.
  - A failed own deletion with a 500-column reason keeps "✕ deletion not sent" in view.
  - The menu shows "Delete with a reason…" only on our own sent messages that are not deleted.
  - Choosing it opens the reason popup, and submitting sends `Redact { reason: Some(…) }`.
- **Snapshots.** `deleted_reasons` (the three lines above) and the details popup of a deleted
  message.
- **Server**, with the moderation server tests:
  - A (a moderator) deletes B's message with a reason: B's timeline shows the reason and A's
    name.
  - B deletes its own message with a reason: A sees "Message deleted: …".

### Size

**S**, nearer M with the names (change 3).

## 4. Verified without the signing keys

### The race, in short

Corrected while building (the `verify-keys` builder, confirmed by matrix-agy in Task 74; the
first version of this section, after agy's Task 69, had the order the wrong way round, read from
the SDK's branch for flows that do not start from a request):

- **Every flow of ours starts from a request**, so `started_from_request` is true
  (`matrix-sdk-crypto/src/verification/sas/mod.rs:330, 413`; `requests.rs:1387, 1512`). Then
  the side that confirms second, already holding the other's MAC, does not finish at once: it
  goes to `WaitingForDone` and sends its MAC and its done (`sas/inner_sas.rs:302-308`;
  `matrix-sdk/src/encryption/verification/sas.rs:84-86`). Neither side trusts the other or asks
  for secrets (`verification/mod.rs:595-601`) before the other's done arrives
  (`inner_sas.rs:408-415`, `verification/machine.rs:534-538`).
- **Old device (A) first is safe.** B confirms second and sends MAC and done; B asks for the
  secrets only when A's done comes, and A sends that after B's MAC, so B's done reaches A ahead
  of any secret request.
- **New device (B) first is the window** (and both at once). A confirms second and sends MAC and
  done together without trusting B yet. B gets both in one sync, queues its done, finishes and
  stores its secret requests; at the end of that sync response (since #6753,
  `matrix-sdk/src/sliding_sync/mod.rs:718-722`) its done and the secret requests go out side by
  side, up to 20 at once (`encryption/mod.rs:974`). A secret request that reaches A in an
  earlier sync than B's done is refused for good: "won't serve, the device isn't trusted"
  (`matrix-sdk-crypto/src/gossiping/machine.rs:416-449`). If they come in the same sync, A is
  safe: it handles the verification events first (`machine/mod.rs:1998-2016`).
- **How often.** Not seen in 40 runs on the dev homeserver (new/old first, 0 s/2 s apart). The
  1-in-8 failures of `a_chosen_session_is_verified` before `2c4df1a` had the shape of a lost key
  share; with both sides confirming at once in that test, this window may have been their cause.
- **The result, when it hits.** B ends up cross-signed but without the private self-signing and
  user-signing keys and the backup key: it cannot verify others, and older encrypted messages
  stay locked, while the app said "this device is verified" and offered nothing.
- **No public way to ask again.** `query_missing_secrets_from_other_sessions`
  (`matrix-sdk-crypto/src/machine/mod.rs:2179`) is not exposed by matrix-sdk.
- **The safe order** is the old device first, then the new one; the Compare lines ask for it.

### What asks for the secrets again

A new verification does. Checked in the source:

- At the end of a flow, `mark_as_done` asks for the missing secrets if the flow verified our own
  master key (`verification/mod.rs:595`; `mark_identity_as_verified`, `:618-652`, says yes for
  `UserIdentityData::Own`).
- A's MAC carries A's master key when A trusts its own identity
  (`verification/sas/helpers.rs:284`). A does, since it has the keys.
- So B's second flow ends with new secret requests: new ids (`gossiping/machine.rs:856-871`).
  The store replaces the old request for the same secret and marks it as not sent
  (`matrix-sdk-sqlite/src/crypto_store.rs:702-720`).
- This time A already trusts B (it signed it), so it answers, whatever the order. The
  cross-signing keys are imported (`gossiping/machine.rs:988-1021`). The backup key goes to the
  secret inbox, where the SDK's recovery picks it up.
- The recovery key works too: `Recovery::recover` imports the cross-signing keys and the backup
  key from secret storage.

### What the user sees

Status line, when it applies: `⚠ keys missing — fix`. It looks like `⚠ unverified device —
verify` (`src/ui/verify.rs:374-381`, `Role::Error`), and a click or the hint's key opens the
popup below.

Settings, the Encryption row: `verified, but without the signing keys, Enter to fix`.

The Encryption popup, from the settings, from the hint, or from `start_verification`:

```
╭ Encryption ──────────────────────────────────────────────────────╮
│ This device is verified, but it did not get your signing keys.   │
│ It cannot verify your other devices or other people, and older   │
│ encrypted messages may stay locked.                              │
│                                                                  │
│ Verify it again with one of your other devices: this time the   │
│ keys come along. Or enter your recovery key.                     │
│                                                                  │
│ This device    verified                                          │
│ Cross-signing  set up, but this device lacks the signing keys    │
│ Recovery       set up, but this device needs the recovery key    │
│ Key backup     not in use on this device                         │
│                                                                  │
│   [ Verify again ]   [ Enter recovery key ]   [ Close ]          │
╰──────────────────────────── Tab move · Enter choose · Esc close ╯
```

- **The buttons.** "Verify again" is shown only when there is another device (`other_devices`).
  "Enter recovery key" is shown only when recovery is set up and this device lacks the key
  (`RecoveryStatus::NeedsKey`).
- **The third way** is a verification started from the other device. A line says so: "Or start
  a verification of this session from your other device". With neither button, that line and
  "log in where the keys are" are all the popup offers.
- **matrix-rat on the other device** must then allow verifying a session that is verified
  already. Today it toasts "That session is verified already" (`src/app/devices.rs:183`); see
  change 7.

In the verification itself:

- **Compare, between two of our own devices.** One more line goes under "Do they match, in the
  same order?":
  - on the device being verified (B): `If so, press “They match” here first, then on your
    device “LAPTOP”.`
  - on the other one (A): `If so, press “They match” on your device “PHONE” first, then here.`

  The order matters only while A does not trust B yet. So B shows its line only while this
  device is not verified, and A only while the other device is not. When B is verified but
  lacks the keys, A trusts it already and answers whatever the order, so neither line shows;
  nor when the user verifies again two devices that have everything.
- **Done, on B.**
  - At first as today: "✓ This device is now verified." and "Your other device sends it the
    keys it lacks…".
  - If the keys have not come 30 s later and the popup is still open, the second line becomes
    `⚠ The signing keys did not come. Verify again, or enter your recovery key.`, with the same
    buttons as the popup above.
- **`KEYS_GRACE`** is 30 s. There is no hint while a verification runs, nor in the 30 s after
  this device's verification has ended; the keys normally come within seconds. When the 30 s
  are up, the screen changes by itself, with no key pressed and no sync (change 2).

### The cheap mitigation: the order

The SDK tells neither side whether the other has confirmed: the internal `MacReceived` shows as
`KeysExchanged` (`matrix-sdk-crypto/src/verification/sas/mod.rs:211-222`). So the order is asked
for, with the two Compare lines above: on the new device "…on your device “X” first, then here",
on the old one "…here first, then on your device “X”". They cost one line per side and also
steer the user when Element is on the other side.

Holding back the new device's confirm until the old device's MAC has come (a to-device handler
for `m.key.verification.mac` would show it) was considered while building and dropped (matrix-agy,
Task 74): it breaks the symmetry of the protocol, two clients doing the same would wait for each
other, and the way out (verify again, the recovery key) covers this and every other cause of
missing keys.

### Our changes

1. **`Security.signing_keys`** (`src/matrix/verify.rs:1574-1577`) changes meaning. Today it is
   `cross_signing_status().is_complete()`: master, self-signing and user-signing keys. It
   becomes `has_self_signing && has_user_signing`, which is what "can verify others" needs
   (`src/verify.rs:112`). Otherwise a client that keeps the master key to itself would make us
   cry wolf.
2. **`keys_missing(security)`** in `src/verify.rs`: `identity == Present && device == Verified
   && !signing_keys`. `VerifyState::keys_missing(now)` adds: no flow running, and not within
   `KEYS_GRACE` of `done_at`. `done_at` is a new field of `VerifyState`: when a verification of
   this device reached Done; a new flow clears it.
   - **Waking up at the end of the grace.** The app has no list of deadlines: the shell ticks
     every 250 ms (`src/shell/terminal.rs:480`) into `App::on_tick` (`src/app.rs:5405`), whose
     `verify_tick` (`src/app/verify.rs:217`) already opens a popup held back while the user
     typed (`TYPING_PAUSE`). `verify_tick` also marks the screen dirty once when
     `done_at + KEYS_GRACE` has passed and the keys are still missing, so an idle screen shows
     the hint or the Done popup's warning at 30 s. A `keys_warned` flag keeps it to once.
   - **Reset.** `done_at` and `keys_warned` live in `VerifyState`, which `App::reset_session`
     (`src/app.rs:1730`, on logout and when the session ends) resets as a whole
     (`self.verify = Default::default()`, `:1751`).
3. **`Hint::KeysMissing`**.
   - It comes from `hint()` (`src/verify.rs:57-71`) and is drawn in `src/ui/verify.rs:360-395`.
   - `setting_text` (`:89-102`) and `describe`'s cross-signing row (`:111-116`) gain the
     keys-missing case.
4. **`PopupKind::KeysMissing`** (`src/verify.rs:185-194`).
   - `buttons()` (`:263-283`) picks one of four fixed lists from `[VerifyAgain, RecoveryKey,
     Close]`, by `other_devices` and `NeedsKey`. The same lists serve a `Flow` at `Done` while
     the keys are missing.
   - New buttons:
     - `VerifyAgain` (" Verify again ") sends `VerifyCmd::Request`, like the last case of
       `start_verification` (`src/app/verify.rs:57-60`);
     - `RecoveryKey` (" Enter recovery key ") opens `Modal::Recovery(RecoveryForm::default())`,
       as at `:53-56`.
   - Both are `affirmative()`, so neither has the focus by default.
   - The body goes in `src/ui/verify.rs` beside `status_body` (`:140-158`).
5. **`start_verification`** (`src/app/verify.rs:38-62`). The `(_, DeviceTrust::Verified)` case
   opens the KeysMissing popup when the keys are missing, and the Status popup as today
   otherwise.
6. **Worker** (`src/matrix/verify.rs`).
   - **`this_device`** (`:1093-1097`) is also true when this device lacks the signing keys. So a
     second verification on B is titled "Verify this device" and ends with the keys text.
   - **The status after the keys come** is already taken care of: the handler for
     `ToDeviceSecretSendEvent` (`:634-638`) calls `refresh.notify_one()`, and the status is
     worked out again.
   - **`await_own_signature`** (`:957-970`) today looks, after 0, 1, 2, 4 and 8 s, until the
     device is cross-signed, fetching our keys (`refresh_own_keys`, a `/keys/query`) each time
     it is not. It then keeps looking, after 16 s as well, until the device also has the signing
     keys, but without the network: once the device is cross-signed, `/keys/query` cannot bring
     private keys, so it only reads `cross_signing_status()` and calls `refresh.notify_one()`.
     Belt and braces next to the handler above, at no cost to the server.
   - **`VerifyStage::Confirmed`** is shown once our `confirm()` has returned (`answer`,
     `:1303`), that is once our MAC is out, and only if `!sas.is_done() && !sas.is_cancelled()`
     at that moment. It no longer comes from the SDK's earlier `SasState::Confirmed` (`:1278`).
     The guard is on the SDK's state, not on our `done()`: when we confirm second, `confirm()`
     moves the SDK to Done before `follow` (`:1121-1135`) has seen it, so our `done()` is still
     false and a `Confirmed` would flash just before `Done`. "Waiting for … to confirm…" stays
     true, and the tests get a signal they can rely on (below).
7. **Sessions popup** (`src/app/devices.rs:170-198`). Choosing a session that is verified
   already starts a verification, instead of the toast "That session is verified already"
   (`:183`; the test at `:1588` changes). That is how the other device fixes this one.
8. **Wording** in `src/ui/verify.rs`: Compare (`:236-249`, the order lines only while the device
   being verified is not verified yet) and Done (`:251-273`).

### The flaky test, made deterministic without hiding the race

`a_chosen_session_is_verified` (`src/matrix/verify.rs:2266-2305`) fails about 1 run in 8.
`Pair::finish` (`:2177-2213`) lets both sides press "They match" as soon as they see the emoji
(`step`, `:2024-2051`), so either order happens.

As built (the order corrected, see "The race, in short"):

1. **`Pair::finish(order)`**, with `Order::OldFirst` or `Order::NewFirst`.
   - OldFirst: A answers Match at Compare; B answers 2 s after A's update says `Confirmed`, so A's
     MAC has reached B.
   - All existing callers use OldFirst, the safe and deterministic order:
     `two_devices_verify_each_other` and `a_chosen_session_is_verified`.
   - `history_unlocks_after_verification` confirms A (the plain SDK device) first too.
2. **New `verified_without_keys_verify_again`**: the recovery path, deterministic.
   1. A is a plain SDK client with cross-signing, as in `history_unlocks_after_verification`.
      B (the worker) logs in.
   2. A signs B's device by hand with `Device::verify()`
      (`matrix-sdk/src/encryption/identities/devices.rs:290`). That leaves B cross-signed with
      no keys: the very state the race leaves, every time.
   3. Assert that B's `Security` is `Verified && !signing_keys`.
   4. B sends `VerifyCmd::Request` and confirms first, the window order, on purpose: A already
      trusts B, so it answers whatever the order.
   5. B ends up with `signing_keys`.
3. **New `verified_without_keys_recovery_key`.** The same setup, but A also sets up recovery
   (`recovery().enable()` gives the key). B sends `MatrixCmd::Recover { key }` and ends up with
   `signing_keys`, and with the backup key (`Security.backup` is Enabled).
4. **New `new_device_first_ends_with_the_keys`**: the window order, with two workers,
   `Pair::finish(Order::NewFirst)`.
   - In most runs B gets the keys.
   - Otherwise B's `Security` stays `Verified && !signing_keys` for a short poll window (5–10 s
     after `Verified`). `KEYS_GRACE` belongs to the app's `VerifyState`, not to the worker's
     `Security`, so this worker test does not wait 30 s. Then the test runs the fix (B sends
     `VerifyCmd::Request`) and asserts the keys.
   - Either way the test prints which case happened. It never fails because of the race, and it
     does not pretend the race is gone: the test logs show how often it hits.
- **Unit.**
  - `keys_missing` and `hint`, with and without a flow and within the grace period.
  - `setting_text` and `describe`.
  - `buttons()` for the four combinations.
  - `start_verification` opens KeysMissing.
  - Verify again sends `VerifyCmd::Request`; Enter recovery key opens the recovery form.
  - In the sessions popup, a verified session leads to `RequestDevice`.
  - `verify_tick` marks the screen dirty once when the grace ends with the keys missing, and
    not when they came; logout clears `done_at`.
  - The order lines: shown while the device being verified is not verified, not otherwise.
- **Snapshots.**
  - New: `verify_keys_missing` (the popup), `verify_keys_missing_hint` (the status line),
    `verify_compare_other_device` (A's line) and `verify_done_keys_missing`.
  - `verify_compare` gains B's line.

### Log privacy

One new line, logged once per verification: `info!("this device is verified, but its signing
keys did not come")`. No ids and no key material. The SDK logs the kinds of the secrets it asks
for (`gossiping/machine.rs:861`), never the secrets.

### Browser build

`src/matrix/verify.rs` builds for the browser already (the `SasStream` cfg at `:972`), and the
changes use the same calls. The gate as usual.

### Size and risks

Size **M**.

- **The 30 s grace is a guess.** On a slow server the hint may show after 30 s, then go away when
  the keys come. That does no harm.
- **Element on the other device** has only our line to go by: it may give no advice of its own.
  Good enough.
- **A verification with someone else** keeps its wording.

## Build order and branches

| Branch | Item | Size | Files |
|---|---|---|---|
| `quiet-logs` | before all | S | `src/logging.rs` (`DEFAULT_FILTER` to `off,matrix_rat=info`, + test), README, release notes; planned separately for 1.1 |
| `verify-keys` | 4 | M | `src/verify.rs`, `src/ui/verify.rs` (+ snapshots), `src/app/verify.rs` (`verify_tick`), `src/app/devices.rs`, `src/matrix/verify.rs` (tests included), README (encryption) |
| `deletion-reason` | 3 | S–M | `src/model.rs` (`people`, `mark_among`), `src/matrix/convert.rs`, `src/matrix/receipts.rs` (`plan`, `annotate`), `src/matrix/rooms.rs` (fake-server tests), `src/ui/timeline.rs`, `src/ui/timeline/memo.rs`, `src/details.rs`, `src/menu.rs`, `src/keymap.rs`, `src/palette.rs`, `src/app.rs`, `src/app/moderation.rs`, snapshots, README |
| `send-retry` | 2 | M | `src/model.rs`, `src/matrix/convert.rs`, `src/matrix/rooms.rs` (`prepare_messages`, `Retry`, `Discard`, `Redact`), `src/matrix/mod.rs` (`MatrixCmd`, the sync arm), `src/ui/timeline.rs`, `src/ui/timeline/memo.rs`, `src/ui/status.rs`, `src/details.rs`, `src/menu.rs`, `src/palette.rs`, `src/app.rs`, `src/app/palette.rs`, snapshots, README |
| `search-backfill` | 1 | M (+ S) | `src/matrix/session.rs`, `src/matrix/auth.rs`, `src/matrix/mod.rs` (`spawn`, `spawn_with`, `Worker`, protocol, session loop, a server test's options), new `src/matrix/search_history.rs` (with `short_date`, the measuring test and its relay), `src/matrix/rooms.rs` (two server tests' options), `src/agent/serve.rs`, `src/shell/terminal.rs`, `src/matrix/agent.rs` (test call), `src/config.rs`, `src/modal.rs`, `src/app.rs`, `src/ui/global_search.rs` (+ snapshots), README, release notes |

`verify-keys` leaves `src/app.rs` alone: `on_tick` already calls `verify_tick`, and
`reset_session` resets the whole `VerifyState`, `done_at` included. Only its tests may go there.

The order:

0. **`quiet-logs` first.** Items 1 and 2 rely on it to keep the SDK's lines with ids and raw
   errors out of the log.
1. **`verify-keys` next.** It fixes something users can hit today, and it ends the 1-in-8 flake
   that would otherwise muddy every later server run.
2. **`deletion-reason`, then `send-retry` on top of it.** They share the model, convert, the
   timeline drawing, the details and the menu.
3. **`search-backfill` alongside 2, merged last.** It holds the biggest unknowns: the cost,
   measured on the dev server before the release, and the two backfills that now run for every
   user. It meets `send-retry` only in three places:
   - `src/matrix/mod.rs`: new `MatrixCmd` variants on both sides, and both hook Offline →
     Running in the sync arm (`:2199-2217`). Whichever comes second joins the two hooks into
     one `back_online` function.
   - `src/matrix/rooms.rs`: only the options of two server tests, far from `prepare_messages`
     and the commands.
   - `src/app.rs`, at different places.

Built side by side without touching the same code: `verify-keys`, `deletion-reason` and
`search-backfill` (the last two share only test lines in `src/matrix/rooms.rs` and separate
places in `src/app.rs`). `send-retry` follows `deletion-reason`. All of them touch the README
and the release-notes draft, in separate sections, so those merges are add/add.

Each branch gets its own worktree, `nix-shell --run scripts/check.sh`, the wasm gate and its
server tests, then matrix-agy's review before the merge, as for the other 1.1 branches. There is
one dev homeserver and two server runs at once break each other, so the branches take turns.

## Not in 1.1.0

- Search-history progress per week or per room. The SDK has none; reading it from the SDK's log
  was considered and dropped: it ties us to debug text, and logging is native-only.
- Search history in `serve` or in the browser.
- Turning a room's queue back on after a passing failure while the sync stays up
  (`SendQueue::subscribe_errors`, `send_queue/mod.rs:406`, with a back-off). Until then: Enter,
  Del, or the next Offline → Running.
- A setting for the latest-event and read-receipt backfills. They come with the queue, which
  the app always has (see [The app always has the queue](#the-app-always-has-the-queue)).
- Keeping the text of a discarded edit, to edit it again.
- Deletion reasons for agents (`serve`, MCP).
- Reporting the secret-request race upstream. The user leaves matrix-rust-sdk alone for now
  (2026-11-27), and its AI policy rules out an AI-written report.

## Decisions

The two open questions of the first draft, answered by the user on 2026-11-27 (as relayed by
matrix-claude), and the two decisions of the review round.

1. **Del on a failed deletion keeps the message** (the user: "Del on a failed deletion keeps the
   message"). Del discards the failed deletion, so the message comes back; the status line says
   "keep message", and Del discards any failed thing in the same way. Enter tries the deletion
   again. matrix-agy endorsed the same: one rule for all four kinds, and a wrong key costs
   nothing (a second Del deletes), where the other way round it would delete for good.
2. **Search history is on by default** (the user: "Search history is on by default"). A settings
   row turns it off. On the first start after the upgrade it costs requests and disk; the
   numbers come from the dev-server measurement before the release (see [Measuring before the
   release](#measuring-before-the-release-on-the-dev-server)). The proposed gate there brings
   the default back to the user only if 100 000 events would cost more than 1 GB of disk or an
   hour of fetching.
3. **The app always has the back-pagination queue** (matrix-claude, on matrix-agy's Catch #1:
   "Option A"). The setting governs only the three-month fetch; the latest-event and
   read-receipt backfills run in the app for everyone, and stay off in `serve`. See [The app
   always has the queue](#the-app-always-has-the-queue).
4. **The cost is measured on the dev homeserver, not on a profile copy** (matrix-claude). The
   copy of the user's 1.0.3 profile is tested offline only: store migration, opening, no login
   form. A copy that synced would be the user's own device to the server and would take the
   room keys meant for the user's real client.
