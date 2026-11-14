# Review: M15 — Unread handling & clickable desktop notifications

**Branch / commit reviewed:** `unread-notify` at `137ba97` (`Merge main into unread-notify: the unread line and jumps to a message`, on `main` `49e1ecd`; 27 files, `+3030` / `-114`)  
**Files inspected:** `README.md`, `Cargo.toml`, `src/action.rs`, `src/keymap.rs`, `src/model.rs`, `src/palette.rs`, `src/menu.rs`, `src/notify.rs`, `src/app.rs`, `src/app/unread.rs`, `src/app/global_search.rs`, `src/app/menu.rs`, `src/app/palette.rs`, `src/app/thread.rs`, `src/matrix/mod.rs`, `src/matrix/rooms.rs`, `src/matrix/convert.rs`, `src/matrix/receipts.rs`, `src/ui/sidebar.rs`, `src/ui/timeline.rs`, `src/ui/snapshots/*`, `matrix-sdk-0.19.1` (`src/room/mod.rs`), `matrix-sdk-ui-0.19.1` (`src/timeline/mod.rs`, `src/timeline/controller/mod.rs`, `src/room_list_service/mod.rs`, `src/room_list_service/state.rs`, `src/sync_service.rs`), `notify-rust-4.19.0` (`src/hints.rs`, `src/xdg/zbus_rs.rs`).

---

## Verdict

**Changes requested (two `[MEDIUM]` issues where a room can stay unread or revert to unread on the server, plus two `[LOW]` notes).**  
The UI state machine in `src/app/unread.rs`, the `'line undecided'` receipt gate, detached-slice/jump/thread interactions, `on_notice_click`, the `zbus` D-Bus `Clickable` implementation in `src/notify.rs`, logging/privacy redaction, and the test suite (`522` unit tests incl. real `dbus-daemon` roundtrips) are very cleanly designed. Before `0.2.0`, however, two worker-side gaps need attention:
1. `UnreadFlags`'s 30 s `TRUST_SENT` override can drop clearing (or setting) a flag changed by another client in between, letting the room revert to unread after the 10 s local wish expires.
2. `MarkRoomsRead` (`MarkAllRead`) calls unpaginated `Timeline::mark_as_read`, which silently returns `Ok(false)` without sending a receipt if an unopened room has no unthreaded remote event in `EventCache`.

---

## Check answers

### 1. Receipts: can a room stay unread forever, or be marked read without the user seeing it?
- **No premature receipts while the `'new'` line is undecided, during offline start, or in detached views:**
  - `App::maybe_mark_read` (`src/app.rs:1676-1708`) requires `!self.timeline.detached`, `self.timeline.scroll == 0`, `self.modal.is_none()`, `!self.help_open`, `!self.images.viewer_open()`, `self.window_focused`, `!self.thread_loading()`, and `!self.unread_line_pending()`.
  - When a room opens at startup before first sync (`LoggedIn` sets `catching_up = true`), `begin_unread_line()` records `early: true`. Neither `with_unread_line()` nor `settle_unread_line()` resolves `Line::Waiting` until `MatrixEvent::CaughtUp` arrives **and** `700 ms` (`CATCH_UP`) elapses (`src/app/unread.rs:34, 100-111, 234-243, 288-306`). So offline starts never send receipts on cached old messages. (`caught_up_in` checks `State::SettingUp | Recovering | Running`, which in `room_list_service/mod.rs:310-322` is only set **after** sliding sync's first `Ok` response.)
  - Entering another room via a detached `/context` slice (`detached.is_some()`) preserves `Line::Waiting`, `settle_unread_line` skips when `self.timeline.detached`, and `maybe_mark_read` never fires until the user returns to live (`End` / `return_to_live`) and sees the bottom. Opening a thread sends `InThread(..., MarkRead)` for the thread only, leaving both the room's `m.fully_read` marker and the room's `Unread` line intact (`a_thread_has_no_line_and_the_rooms_line_survives_it`).
  - What if every unread event in the room is our own or a `System` line? `first_unread` returns `None` -> `Line::Off`, clearing `unread_line_pending()` so `maybe_mark_read` can still mark the room read.
- **However, a room *can* stay unread / flip back to unread in two worker cases:** See **Finding #1 (`[MEDIUM]`)** and **Finding #2 (`[MEDIUM]`)**, plus a minor timing caveat (**Finding #4 `[LOW]`**).

### 2. Does sending `m.fully_read` on `MarkRead` change behaviour for other clients in a bad way?
- **No.** In Matrix (`mark_room_read`, `src/matrix/rooms.rs:1024-1036`), `m.fully_read` is private room account data visible only to the user's own clients, so sending `ReceiptType::FullyRead` even with `read_receipts = false` ("Send read receipts" off) never leaks read status to other users.
- Advancing `m.fully_read` only happens when `maybe_mark_read` sees the user looking at the bottom of the live room (or explicitly triggers `"Mark as read"` / `"Mark all rooms as read"`), keeping other clients' unread dividers in sync rather than leaving stale read markers behind. And because `Timeline::mark_as_read(ReceiptType::FullyRead)` in `matrix-sdk-ui` does not redirect away from own events (`timeline/controller/mod.rs:1738-1830`), sending/reading up to our own latest message moves `m.fully_read` to the true end of the room (tested by `a_room_marked_unread_shows_in_the_users_other_clients`).

### 3. The unread line vs detached slices and jumps (`switch_room_state(index, opened)`)
- **Clean across all paths (`src/app.rs:1112-1154`, `src/app/global_search.rs:527-566`, `src/app/unread.rs:207-284`):**
  - Opening a room (`open_room` -> `switch_room_state(index, true)`) or switching to another room (`opened == false`, `index != self.active`) makes `fresh = true` -> `begin_unread_line()` sets `Line::Waiting` using the pre-cleared `room.unread`.
  - Jumping to an event **within the same room** (`switch_room_state(index, false)` -> `fresh = false`) leaves the already-placed `Line::At(id)` intact: slices that exclude `id` don't show it, slices that include `id` display it above `id`, and returning to live restores it.
  - Jumping into **another** room's newest messages (`detached == None`) places the `'new'` line for later (`Alt-U` jumps to it), while keeping `timeline.selected` on the jumped message (`reveal_unread` is overridden by `on_jump_arrived` resetting `TimelineState { selected, reveal_selected: true, .. }`).
  - Jumping into **another** room's older slice (`detached == Some`) leaves `Line::Waiting` pending, strips any SDK `ReadMarker` so old slices don't claim a false `'new'` line, and resolves once the user returns to live.
  - Jumping into a **thread** in another unread room keeps `Line::Waiting` pending while `thread_open()` is true (`place_unread_line` returns before `with_unread_line`) and resolves when the user closes the thread and the room's live timeline arrives.

### 4. Races between `SetUnread`, `MarkRead`, and late `Rooms` lists
- **App side (`WISH_HOLDS = 10 s`, `src/app/unread.rs:29, 76-85, 342-388`):**
  - Sending a message, sending files, opening a room, `MarkRead` (`note_read`), `mark_room_read`, and `mark_all_read` all call `unmark_unread` / `clear_marked_unread` and record a `(false, Instant::now())` wish (or `(true, Instant::now())` for `mark_room_unread`). Stale `Rooms` lists disagreeing with the wish are overridden for up to 10 s and cleared immediately once the server agrees. Vanished rooms and `reset_session()` clean up both `read` and `wishes`.
- **Worker side (`UnreadFlags`, `src/matrix/rooms.rs:1047-1108`):**
  - Queueing `(room, bool)` on an unbounded FIFO channel serializes PUT `m.marked_unread` calls in request order, avoiding out-of-order HTTP completion if the user marks unread and immediately opens/reads the room. And `mark_room_read` calls `flags.set(room, false)` *before* `timeline.mark_as_read` (which internally calls `set_unread_flag(false)` if no newer receipt needs sending).
  - **Bug found:** The deduplication cache `sent` (`TRUST_SENT = 30 s`) in `UnreadFlags::start` breaks when another client changes the flag within 30 s (**Finding #1 `[MEDIUM]`**).

### 5. Notification click (`src/notify.rs` `Clickable` + `App::on_notice_click`)
- **D-Bus security & spoofing:** `on_signal` (`src/notify.rs:624-655`) verifies `header.sender()` against `live.owner` (fetched from `DBusProxy::get_name_owner("org.freedesktop.Notifications")`). If `sender != live.owner`, it re-queries `get_name_owner` (so a restarted notification daemon under a new unique bus name keeps working cleanly) and drops signals from any other bus peer (`a_click_on_a_notification_reports_its_room_until_it_is_closed` verifies a stranger connection cannot trigger clicks). Only `"ActionInvoked"` with action `"default"` on tracked notification IDs fires, and `"NotificationClosed"` removes the ID.
- **No blocking of notification thread or UI:** `desktop()` (`src/notify.rs:381-416`) spawns one OS thread running `futures::executor::block_on` with `futures::future::select(jobs.next(), clickable.next_signal())`. `SIGNALS_MOST = 1024` prevents backpressure stalls on the `zbus::MessageStream` reader task; `CLICKABLE_MOST = 64` caps memory; the UI thread only pushes onto an unbounded channel (`unbounded_send`) and receives clicked `room_id`s in `tokio::select!`.
- **Restart & fallback:** Disconnect/bus drop sets `Link::Down(Instant::now())`, clears `shown`, retries after `RETRY_AFTER = 60 s`, and falls back immediately to `notify_rust::Notification::show()` (also used permanently when `GetCapabilities` lacks `"actions"`, `Link::NoActions`, or when `Notify` errors).
- **Parity & logging:** `Clickable::show` converts `notification.hints` with `Into::into` — the exact `From<&Hint> for (&str, zvariant::Value)` implementation used internally by `notify-rust`'s `hints_to_map` — so `image_path` room icons/tiles and `urgency` hints match `notify-rust`. Note one minor limitation: if the notification daemon is replaced mid-session by a *different* daemon without `"actions"`, `GetCapabilities` is not re-queried until disconnect. Zero notification bodies, room names, or senders are logged (`tracing::debug!` logs only `%err`).
- **`App::on_notice_click` (`src/app.rs:905-949`):** Ignores clicks while `logged_out` or while `Login` / `Recovery` / `Verify` modals are up, ignores gone rooms, cancels any pending jump (`CancelOpenEvent`), closes `Search` / `GlobalSearch` (with `CancelSearch`) / `New` / `RoomInfo` / other popups / `help_open` / image viewer, leaves the active live view undisturbed (returns to live if `detached`), and clears an incompatible active `space` filter before opening and revealing the room.

### 6. Changed test `selection_and_reply_follow_their_message_when_history_loads` & new tests
- `selection_and_reply_follow_their_message_when_history_loads` (`src/app.rs:4560-4620`) shifted its expected `timeline.selected` from `Some(4)` to `Some(3)` and removed `"skips the marker"` because `place_unread_line` now strips the SDK's `ReadMarker` whenever `self.unread.line` settled to `Line::Off` at room open (`alpha` opened with `unread = 0`). Stepping over `MsgKind::ReadMarker` when a real `'new'` line is present is covered in `app::unread::tests::jumping_to_the_first_unread_message`.
- The 14 new `app::unread::tests`, 4 `on_notice_click` tests in `src/app.rs`, and 2 real `dbus-daemon` tests in `src/notify.rs` cover the new behavior cleanly.

---

## Findings

### 1. `[MEDIUM]` `UnreadFlags`'s 30 s `TRUST_SENT` cache can drop clearing another client's `m.marked_unread`
- **Where:** `src/matrix/rooms.rs:1047-1108` (`UnreadFlags`).
- **Detail:**
  1. When `set_account_data(MarkedUnreadEventContent::new(unread))` succeeds, `UnreadFlags::start` inserts `(unread, now)` into `sent`, keeping it for `TRUST_SENT = 30 s` (`line 1052`).
  2. `sent[room_id]` is only pruned when another command arrives for *that* room while `*ours == room.is_marked_unread()` (`lines 1068-1073`). Otherwise `(false, t0)` stays in `sent` after `/sync` confirms `synced == false`.
  3. Suppose matrix-rat unmarks room `R` (`sent[R] = (false, t0)`), sync confirms `false`, and within 30 s **another client** marks `R` unread (`room.is_marked_unread()` becomes `true`). Matrix-rat shows `R` as unread in the room list.
  4. When the user now opens `R`, reads to the bottom, or runs `MarkAllRead` (`unread = false`): `*ours == synced` (`false == true`) is `false`, so `sent[R]` is **not** removed, `now` resolves to `false`, and `if now == unread { continue; }` (`lines 1078-1080`) **skips sending `m.marked_unread: false` to the server**.
  5. On the UI side `unmark_unread` holds `WISH_HOLDS = 10 s`, so `R` looks read for 10 seconds and then flips **back to unread** on the next room list update (same symmetric issue if `sent[R] == (true, t0)`, another client clears it, and the user re-marks within 30 s).
- **Suggested fix:** Track sync confirmation as rooms arrive (e.g. call an `unread_flags.observe(&rooms)` / `room.subscribe_to_room_info` or store `(ours, baseline_synced, at)` and clear `sent` on sync when `synced == ours`) rather than holding an unobserved 30 s override across arbitrary external state changes.

### 2. `[MEDIUM]` `MarkRoomsRead` (`MarkAllRead`) can silently leave an unread room unread on the server
- **Where:** `src/matrix/rooms.rs:744-784` (`MatrixCmd::MarkRoomsRead`), `src/matrix/rooms.rs:999-1009` (`room_timeline`), `src/matrix/rooms.rs:1024-1036` (`mark_room_read`).
- **Detail:**
  - `SyncService` runs with `DEFAULT_LIST_TIMELINE_LIMIT = 1` (`matrix-sdk-ui-0.19.1/src/sync_service.rs:803`).
  - For each unread joined room not currently in `self.cache`, `MarkRoomsRead` builds `room_timeline(&room)` with `hide_threaded_events: true` and immediately calls `mark_room_read(&timeline, receipt, &flags)` without checking pagination or the `Result<bool>` return value of `timeline.mark_as_read`.
  - In `matrix-sdk-ui-0.19.1/src/timeline/mod.rs:984-996`, if `controller.latest_event_id().await` returns `None` (for example, an unopened unread room whose `EventCache` currently holds no visible unthreaded message-like event—e.g. its 1 sliding-sync event is a thread reply/aggregation or a cold/evicted cache entry), `Timeline::mark_as_read` unsets `m.marked_unread` and returns `Ok(false)` without sending any receipt.
  - Because `Ok(false)` is treated as success (`lines 1031-1035`) and `App::mark_all_read` zeroes `room.unread`/`bold`/`mention` locally, `MarkAllRead` reports `"Marked N rooms as read"` while that room remains unread on the server and pops back in a subsequent `Rooms` list.
- **Suggested fix:** When `timeline.mark_as_read` returns `Ok(false)` in `mark_room_read` / `MarkRoomsRead`, either paginate one small batch backwards on that temporary timeline or fall back to `room.latest_event()`'s event id if present.

### 3. `[LOW]` `mark_room_read` / `MarkAllRead` while `Line::Waiting` can still show a stale `'new'` line on arrival
- **Where:** `src/app/menu.rs:473-491` (`mark_room_read`), `src/app/unread.rs:391-439` (`toggle_unread`, `mark_all_read`).
- **Detail:** If a room is open with `self.unread.line` still `Line::Waiting` (for instance, entered via a detached `/context` slice or before `CATCH_UP` settles) and the user triggers `"Mark as read"` / `"Mark this room as read"` / `"Mark all rooms as read"`, `MarkRead`/`MarkRoomsRead` is sent and `unread`/`bold`/`marked_unread` clear, while `self.unread.line` stays `Line::Waiting` and `self.unread.read` is unchanged. If the room's live messages load before the server echoes the updated `m.fully_read`, a stale `'new'` line can be placed for messages already explicitly marked read. (Clearing `unread.line` to `Line::Off` or updating `unread.read` for the active non-detached/live state would avoid this.)

### 4. `[LOW]` Slow startup timelines (> 700 ms after `CaughtUp`) can freeze the `'new'` line early
- **Where:** `src/app/unread.rs:34, 288-306` (`CATCH_UP`, `settle_unread_line`).
- **Detail:** Once `700 ms` after `CaughtUp` (`State::SettingUp`) elapses, `settle_unread_line` commits `self.unread.line` from whatever cached remote events are currently in `self.messages()`. If the open room receives its new events or `m.fully_read` >700 ms after `CaughtUp` (slow connection, large catch-up, or UTD decryption lag), the line can settle prematurely onto old cached messages (`Line::At`) or `Line::Off` before the missed messages appear (though `maybe_mark_read` still only marks up to the cached newest event id, not the later arrivals).

---

## Checks run on `unread-notify` (`137ba97`)

Run in a clean detached temporary worktree at `137ba97` (removed immediately after):
- `cargo fmt --check` — pass
- `cargo clippy --all-targets -- -D warnings` — pass
- `cargo test` — **`522 passed; 0 failed; 23 ignored`** (`dbus-daemon` available at `/run/current-system/sw/bin/dbus-daemon`, so all 9 `notify::tests`, 14 `app::unread::tests`, and `app::tests` `on_notice_click` tests ran and passed).
- Ignored server-backed tests (`matrix::rooms::tests::a_room_marked_unread_shows_in_the_users_other_clients`, `the_read_marker_stands_before_what_was_not_read`, `matrix::notify::tests::*`) were not re-run by me.
