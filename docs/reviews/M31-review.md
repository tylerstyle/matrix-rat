# M31 Review — `LeftRooms` & Shared `NotificationSettings` (`sync-stall`)

Reviewed `HEAD` (`ff2f292ba1b4d18bbf633a26a929e2c48984e857`) on branch `sync-stall` (`.claude/worktrees/sync-stall`):
- [`src/matrix/left.rs`](../../src/matrix/left.rs) (+173 / -0)
- [`src/matrix/manage.rs`](../../src/matrix/manage.rs) (+100 / -12)
- [`src/matrix/mod.rs`](../../src/matrix/mod.rs) (+32 / -6)

Review only — no builds or tests were run (`hpfury` reported 842 unit tests green).

---

## Verdict

**Changes requested (two concrete `LeftRooms` holes worth closing before merge; `notification_mode()` and test waits are solid).**

The root-cause diagnosis is right on both fronts, `notification_mode()` cleanly replaces `Room::notification_mode()`, and `LeftRooms` solves the left+forgotten room reappearing in the sidebar. However, two paths can still strand a real joined room as hidden or break opening a recreated DM, and several other room consumers in the worker still see the late-sync `Joined` zombie.

---

## 1. `LeftRooms`: Can a Room the User Is Actually In Stay Hidden?

### 1a. Recreating a DM (`ManageCmd::DirectChat`) can select and stall on a hidden zombie room (Should-Fix)
- `ManageCmd::DirectChat` ([`src/matrix/manage.rs:862-880`](../../src/matrix/manage.rs#L862-L880)) calls [`direct_chat`](../../src/matrix/manage.rs#L1549-L1586) without checking `self.left` or calling `left.clear`.
- `direct_chat` first calls `client.get_dm_room(&user)` ([`src/matrix/manage.rs:1559-1561`](../../src/matrix/manage.rs#L1559-L1561)).
- In `matrix-sdk 0.19.1` (`src/client/mod.rs:1987-2016`), `Client::get_dm_room` iterates `self.joined_rooms()` and returns the first room matching `r.direct_targets()` (`m.direct` account data). Leaving a room does not clear `m.direct`.
- If a user leaves (+ forgets) a 1:1 DM, late sliding sync recreates it locally as `RoomState::Joined`, and `m.direct` still references it, `client.get_dm_room` returns that **hidden zombie room ID** instead of creating a new DM (`client.create_dm`).
- When `ManageEvent::Opened { result: Ok(room_id), .. }` arrives in the UI ([`src/app/rooms.rs:1948-1955`](../../src/app/rooms.rs#L1948-L1955)), `finish_open_or_knock` sets `pending_open` and waits for `room_id` to appear in `self.rooms`, where `left.hides(room)` keeps hiding it. Even if opened directly, `ManageCmd::Info` still rejects it with `"You are not in this room any more."`.
- Note: a non-DM room re-created with `ManageCmd::Create` always gets a new server-assigned `room_id`, so it cannot collide with `LeftRooms`.
- **Fix direction:** filter `left.is_left(room.room_id())` when selecting an existing DM in `direct_chat` (iterate `client.get_dm_rooms(&user)` rather than `get_dm_room`, skipping any `left.is_left(...)` room — or treat `get_dm_room` returning a left room as not existing so `create_dm` creates a fresh DM; also verify invite matches in `client.invited_rooms()` aren't left-marked though `Invited` already clears in `hides()`).

### 1b. Rejoining from another client after the one `/joined_rooms` check stays hidden (Should-Fix)
- In [`LeftRooms::hides`](../../src/matrix/left.rs#L89-L122), once `RoomState::Joined` is seen at `since.elapsed() >= ASK_AFTER` (`10s`), `entry.asked` flips to `true` permanently (`let ask = due && !entry.asked; entry.asked |= ask;`).
- Two edge cases leave a real joined room hidden for the rest of the session:
  1. **Rejoin on another client after `t = 10s`:** If the first `/joined_rooms` check runs after `10s` and returns `Some(false)` (or fails/times out as `None`), and the user later rejoins the same public or space room from Element/another client (which enters sync directly as `RoomState::Joined` without `Invited`/`Knocked`), `entry.asked` is already `true`. `hides()` never asks again and keeps the room hidden until restart (and `ManageCmd::Info` keeps returning `"You are not in this room any more."`).
  2. **No scheduled follow-up before `ASK_AFTER`:** When `hides()` returns `true` at `t < 10s` (`!due`), nothing schedules a wakeup at `ASK_AFTER`. If sliding sync already delivered its late update and no unrelated room-list event calls `summarize()`, `/joined_rooms` is not queried after `10s` until some other room changes.
- **Minor race in `clear` after `/joined_rooms` ([`src/matrix/left.rs:114-119`](../../src/matrix/left.rs#L114-L119)):** the spawned task calls `left.clear(&room_id)` unconditionally when `server_says_joined` returns `Some(true)`. If `/joined_rooms` (20 s timeout) overlaps with a quick rejoin-then-leave, the delayed `clear` can erase the newer `mark()`. Comparing the entry's `since` timestamp before removing avoids that race.
- **Suggestion:** instead of a one-shot boolean `asked`, track a cooldown / exponential backoff timestamp (e.g. re-allow `/joined_rooms` after 30 s / 60 s on `Joined` observations, or reset on transient error `None`) and optionally trigger one delayed `back.notify_one()` at `ASK_AFTER` while the entry is still present in `Joined` state.

---

## 2. Other Consumers Still Seeing the Late-Sync `Joined` Zombie

Today [`LeftRooms::hides`](../../src/matrix/mod.rs#L2151-L2154) only filters `summarize()` (the room list) and [`LeftRooms::is_left`](../../src/matrix/manage.rs#L930-L939) guards `ManageCmd::Info`. Every other consumer reading `rooms` / `client.rooms()` / `RoomState::Joined` still sees the zombie:

1. **`OwnMembers::ensure` ([`src/matrix/mod.rs:1819, 1835`](../../src/matrix/mod.rs#L1819), [`src/matrix/notify.rs:239-272`](../../src/matrix/notify.rs#L239-L272)):** Runs on `&rooms` *before* `summarize(...)`. A zombie `Joined` room consumes `OwnMembers`'s budget and triggers `room.sync_members().await` against the server for a room the user left.
2. **`Previews::listen` ([`src/matrix/mod.rs:1822, 1882, 2091-2118`](../../src/matrix/mod.rs#L1822)):** Also runs on `&rooms` without consulting `left`, keeping `latest_events().listen_to_room` active for left zombies.
3. **Desktop notifications (`src/matrix/notify.rs:88-124`):** `Watch` checks room state and freshness (`FRESH`), not `left.is_left`, so a recent message (`<= 120s`) attached to the late sync zombie can fire a spurious notification after the user left.
4. **`Timelines` ([`src/matrix/rooms.rs:214-219`](../../src/matrix/rooms.rs#L214-L219)):** `OpenRoom`, `OpenEvent`, `Send`, etc. only check `room.state() == Joined`. Opening a `matrix.to` permalink / room ID link to a forgotten zombie room opens it as if joined (and skips any prompt/error expected for a left room). Note that clicking a search hit shares this path too.
5. **Global search (`src/matrix/search.rs:80-86, 390`):** `resolve_hit` checks `room.state() != RoomState::Joined`, so indexed hits in a resurrected zombie room can still appear in search results; similarly `joined_room_ids` (`client.joined_rooms()`) governs shutdown index pruning.
6. **People (`src/matrix/people.rs:436, 880, 992`):** `own_profile`, `collect_shared_rooms`, and `find_member_avatar` iterate `client.rooms()` (`RoomState::Joined`) without filtering `LeftRooms`, so zombies can count toward shared rooms.
7. **Spaces (`src/matrix/mod.rs:1685-1688, 1792`, [`src/matrix/spaces.rs:32`](../../src/matrix/spaces.rs#L32)):** `SpaceService` / `spaces::summarize(&space_filters)` does not consult `LeftRooms` (though leaving a space via `ManageCmd::Leave` calls `left.mark`).

(Passing `left: &LeftRooms` into `OwnMembers::ensure`, `Previews::listen`, `notify::Watch`, `Timelines::handle`, and `search` is low-friction because `LeftRooms` is already `Clone` and uses a fast non-async `std::sync::Mutex`.)

---

## 3. Concurrency, Bounds, & Minor Details in `LeftRooms`

- **`Mutex` / `Notify` usage ([`src/matrix/left.rs:29-122`](../../src/matrix/left.rs#L29-L122)):** Sound. `std::sync::Mutex` is never held across `.await` points (all critical sections are brief `HashMap` lookups/mutations), avoiding lock-across-await hazards. `back: Arc<Notify>` uses `notify_one()` after `left.clear(&room_id)`, and [`Worker::run`](../../src/matrix/mod.rs#L1807-L1811) awaits `left.back()` in `select!`; because `Notify` stores a single permit if `Worker::run` is busy in another `select!` arm, the wakeup is not lost.
- **Bounds (`ASK_AFTER = 10s`, `MOST = 500`, HTTP timeout `20s`):**
  - Wait times and capacity bounds are sensible.
  - **Minor nit in `mark` ([`src/matrix/left.rs:52-71`](../../src/matrix/left.rs#L52-L71)):** `if rooms.len() >= MOST` runs before checking whether `room_id` is already in `rooms`, so re-marking an existing entry at capacity needlessly evicts the oldest other room (`if rooms.len() >= MOST && !rooms.contains_key(room_id)` fixes it).
- **Declined invites / withdrawn knocks ([`src/app.rs:2635-2650`](../../src/app.rs#L2635-L2650), [`src/matrix/rooms.rs:1098-1111`](../../src/matrix/rooms.rs#L1098-L1111)):** `Action::DeclineInvite` and `Action::WithdrawKnock` route through `MatrixCmd::Leave` rather than `ManageCmd::Leave`, so `left.mark` is not called on those paths (and `cleanup_left_room` is only called when `was_knocked`).

---

## 4. `manage::notification_mode()` & Shared `NotificationSettings`

- **Matches SDK logic (`matrix-sdk 0.19.1`):**
  - Checked `matrix-sdk-0.19.1/src/room/mod.rs:3835-3863` (`Room::notification_mode`) and `matrix-sdk-0.19.1/src/client/mod.rs:3572-3575` (`Client::notification_settings`).
  - Previously, every `Room::notification_mode()` call called `client.notification_settings().await`, which reconstructed `Ruleset` from account data store AND created a new `NotificationSettings` (registering a new `PushRulesEvent` handler on `Client` each time).
  - [`manage::notification_mode`](../../src/matrix/manage.rs#L1685-L1706) reproduces the exact same logic using the single shared [`Worker::run` `notification_settings`](../../src/matrix/mod.rs#L1693-L1710): returns `None` unless `RoomState::Joined`, checks `settings.get_user_defined_room_notification_mode(room.room_id()).await`, otherwise derives `encrypted` (`room.latest_encryption_state().await.ok()?.is_encrypted()`) and `one_to_one` (`room.active_members_count() == 2`) and queries `settings.get_default_room_notification_mode(...)`.
- **No leftover callers:** Repo-wide grep confirms zero remaining calls to `Room::notification_mode()`; both [`manage.rs`](../../src/matrix/manage.rs#L2158) (`RoomDetails`) and [`mod.rs`](../../src/matrix/mod.rs#L2168-L2169) (`summarize`) read via `manage::notification_mode`.
- **Behavioral caveat on `SetNotify` -> room list `muted` flag (non-blocking):**
  - In `matrix-sdk-0.19.1/src/notification_settings/mod.rs:305-355`, `NotificationSettings::set_room_notification_mode` updates `self.rules` immediately upon server success **without** sending on `self.changes_sender` (which only fires when `PushRulesEvent` arrives over sync at lines `108-112`).
  - Fresh `ManageCmd::Info` calls immediately reflect the change (fixing the race in `a_room_from_creation_to_forgetting`), and `RoomInfo` optimistically updates `details.notify` ([`src/app/rooms.rs:1430`](../../src/app/rooms.rs#L1430)).
  - However, `ManageCmd::SetNotify` ([`src/matrix/manage.rs:1156-1166`](../../src/matrix/manage.rs#L1156-L1166)) does not poke `Worker::run` to re-run `summarize()`, so the room list's `muted` indicator still waits until `m.push_rules` syncs back or an unrelated `summarize()` trigger runs.

---

## 5. Server Test Waits (`a_room_from_creation_to_forgetting`)

- Checked [`src/matrix/manage.rs:2887-2895`](../../src/matrix/manage.rs#L2887-L2895), `3067`, `3097`, and `3156`:
  - Waiting on `!d.topic.is_empty() && d.encrypted == Some(true) && d.members.len() == 1 && d.can_invite && d.can_rename && d.can_set_topic` initially, `d.direct && d.encrypted.is_some()` at `3067`, `d.joined == 2 && d.alias.is_some()` at `3097`, and allowing `asked_again < 2` at `3156` for the forgotten-room `Info` check accurately reflects sliding sync delivering state/account-data/power changes asynchronously across multiple sync cycles rather than atomically.

---

## Re-check (`7a35244..f8c0279`)

Reviewed `34cc0d0..HEAD` (`f8c0279`) on branch `sync-stall` (`7a35244`, `405848d`, `c1706ea`, `5e5e181`, `2d31f71`, and `f8c0279` merging `main` `41d097e`). No builds or tests were run (`hpfury` reported 926 unit tests green; server loops running).

### Verdict

**One concrete merge gap in `spaces` (`space_details` / `edit_space` ignore `left`) plus recommendation to handle cross-restart zombies via a one-shot startup `/joined_rooms` check; all original M31 findings (`1a`, `1b`, §2 consumers, §3 `MOST` + declined invites/knocks, §4 `relist`) are closed cleanly.**

### Status of Original Findings

1. **1a (`DirectChat` / `join` / previews, `405848d`) — Closed:**
   - [`direct_chat`](../../src/matrix/manage.rs#L1665-L1680) now iterates `client.get_dm_rooms(&user).find(|room| !left.gone(room))`, skipping left zombies while still offering invitations via `client.invited_rooms()`.
   - [`join`](../../src/matrix/manage.rs#L1637-L1650) (`!left.gone(&room)` so left-looking-joined rooms join for real and call `left.clear` on success), [`directory::map_public_room`](../../src/matrix/directory.rs#L368-L376), and [`directory::fetch_preview`](../../src/matrix/directory.rs#L451-L467) are fixed, and [`LeftRooms::clear`](../../src/matrix/left.rs#L117-L126) notifies `wake` so the list rebuilds immediately.
2. **1b (Backoff / `due()` wakeup / `since` race, `7a35244` & `2d31f71`) — Closed:**
   - [`pause`](../../src/matrix/left.rs#L37-L44) backs off `10 s -> 30 s -> 60 s -> 5 min`; [`answered`](../../src/matrix/left.rs#L201-L236) ignores stale `since` timestamps, resets `asking`, leaves `answers` unchanged when `joined == None` (HTTP/timeout failure) so it retries after the same `pause` without a tight error loop, and wakes the loop via `self.wake.notify_one()`.
   - [`due`](../../src/matrix/left.rs#L262-L274) + [`next_ask`](../../src/matrix/left.rs#L239-L246) + [`woke`](../../src/matrix/left.rs#L250-L256) in [`Worker::run_session`](../../src/matrix/mod.rs#L1884-L1893): verified cancellation safety (`due` only runs `woke` after winning the inner `select!` right before returning to the outer select arm; `Notify` holds one permit across busy arms), no busy loop (`woke` clears `waiting` for `ask_at <= now` and `look` sets `asking = true; waiting = false`), and `left.due()` calls `previews.listen(client, &rooms, left)` plus emits both `Rooms` and `Spaces`.
3. **2 (Consumers, `c1706ea`) — Closed (except merge gap in `spaces` below):**
   - `LeftRooms::gone` / `is_left` now guard [`OwnMembers::ensure`](../../src/matrix/notify.rs#L259-L261), [`Previews::listen`](../../src/matrix/mod.rs#L2188), [`notify::notice`](../../src/matrix/notify.rs#L143), [`search::joined_room_ids`](../../src/matrix/search.rs#L82-L89) & [`resolve_hit`](../../src/matrix/search.rs#L400-L405), [`people`](../../src/matrix/people.rs#L452) (`load_own_profile`, `context_room_info`, `collect_shared_rooms`, `find_member_avatar`), [`Timelines::room`](../../src/matrix/rooms.rs#L219-L231), and [`spaces::summarize`](../../src/matrix/spaces.rs#L77-L120).
   - **Checked `Timelines::room()` across all callers in [`src/matrix/rooms.rs`](../../src/matrix/rooms.rs):** `if room.state() == RoomState::Joined && self.left.gone(&room)` only rejects when `state == Joined` and `!Invited && !Knocked && is_left`. Legitimate actions on `Invited` / `Knocked` (`OpenRoom`, `MatrixCmd::Join`, `MatrixCmd::Leave` decline/withdraw, drafts, etc.) and `Left` / `Banned` (such as `ForgetDrafts` seeing `still_in == false`) are never refused. (Caveat: if an already-cached `Timeline` in `self.cache` is accessed directly after `Leave`, `self.timeline` hits `cache` before `self.room`, though `OpenRoom`, `OpenEvent`, `SaveDraft`, and `list_threads` call `self.room` first.)
4. **3 (`MOST` eviction nit + declined invites/knocks, `7a35244` & `c1706ea`) — Closed:**
   - [`mark_at`](../../src/matrix/left.rs#L95-L103) checks `&& !rooms.contains_key(room_id)` before evicting the oldest entry.
   - [`MatrixCmd::Leave`](../../src/matrix/rooms.rs#L1121-L1139) marks `left.mark(room.room_id())` on declined invites and withdrawn knocks.
5. **4 (`SetNotify` list update, `5e5e181`) — Closed:**
   - [`ManageCmd::SetNotify`](../../src/matrix/manage.rs#L1229-L1246) notifies `relist` on `Ok(())`, triggering [`summarize`](../../src/matrix/mod.rs#L1894-L1898) at once.

### Remaining Issues Found

1. **Merge gap (`f8c0279`): [`spaces::space_details`](../../src/matrix/spaces.rs#L269-L292) & [`spaces::edit_space`](../../src/matrix/spaces.rs#L497-L500) do not check `LeftRooms`:**
   - `space_details` reads `client.get_room(&event.id)` whenever `room.state() != RoomState::Left` and sets `child.joined = room.state() == RoomState::Joined` (and keeps its local name rather than `/hierarchy`'s `named`) without checking `!left.gone(&room)`.
   - If a space contains a child sub-space `!sub` that was left in this session and came back looking `Joined`, `spaces::summarize` hides `!sub` from `self.spaces`, but `space_details` reports `child.joined = true`. Because [`App::child_joined`](../../src/app/spaces.rs#L40-L47) uses `self.listed_space(&child.room_id).is_some() || child.joined` for spaces, pressing `Enter` on `!sub` in [`App::open_child`](../../src/app/spaces.rs#L230-L245) calls `open_space_info("!sub", ...)` instead of `open_room_preview(...)`, hitting [`ManageCmd::Info`](../../src/matrix/manage.rs#L1006) (`"You are not in this room any more."`) and blocking preview/rejoin from the parent space.
   - `edit_space` (`SpaceEdit::Add` at [`src/matrix/spaces.rs:497-500`](../../src/matrix/spaces.rs#L497-L500)) also only checks `.filter(|room| room.state() == RoomState::Joined)` without `!left.gone(room)`. Passing `&LeftRooms` into `manage::watch` -> `space_details` and `edit_space` fixes both.

2. **Minor edges (non-blocking if desired, clean to tighten with #1):**
   - When a late invitation or knock arrives for a left room that is a child of a space, [`entries.next()`](../../src/matrix/mod.rs#L1902-L1914) clears the room from `LeftRooms` inside `left.hides(room)` without re-emitting `MatrixEvent::Spaces`; if `Spaces` was emitted while `left.is_left` hid that child from `summary.rooms` / `summary.children` (`spaces::summarize`), `Spaces` stays stale until a space change occurs (even after accepting via `MatrixCmd::Join`, since `left.clear` finds it already removed).
   - [`ManageCmd::Info`](../../src/matrix/manage.rs#L1006) checks `self.left.is_left(id)` directly rather than `!left.gone(&room)` (which would reject `Info` on a newly invited/knocked room if `Info` raced ahead of `entries.next()`).
   - [`MatrixCmd::Join`](../../src/matrix/mod.rs#L2028-L2034) clears `left.clear(room_id)` before `timelines.handle(command)` rather than on `Ok(())` inside `rooms.rs:1115` (unlike `ManageCmd::Join`; fine for `Invited` rooms since `self.room` allows invites, though clearing on `Ok` avoids exposing a phantom room if `Join` failed).

### Assessment of Intentionally Deferred Items

1. **Declined invitation replayed by late sync as `Invited` — keep as-is (do not fix):**
   - In the Matrix Client-Server spec, stripped state events in `invite_state` are not guaranteed to carry `event_id` or `origin_server_ts`, so a stale in-flight `Invited` replay genuinely cannot be distinguished from an immediate re-invite. Any time-based suppression risks hiding real invitations, and a stale `Invited` row (no joined-room side effects, dismissed by another Decline) is much more benign than a `Joined` zombie.
2. **`LeftRooms` session-only across restarts — recommend a one-shot startup `/joined_rooms` check for `0.7` (prefer over an on-disk list):**
   - Because the late sliding-sync answer puts the forgotten room back as `RoomState::Joined` while session 1 is running, the SDK persists that `Joined` zombie into the SQLite store on disk. On restart, sliding sync will never deliver `membership: leave` for a forgotten room, so the zombie reappears in session 2+ (and if the server already rejects `/leave` for a forgotten/non-member room, the user cannot remove it from the UI).
   - **Recommended approach:** do **not** persist a `LeftRooms` list on disk (avoiding plaintext room IDs at rest or extra encrypted state cleanup on logout/reauth). Instead, snapshot `client.joined_rooms()` *before* `sync.start().await` (so nothing joined in the new session is touched), run one `joined_rooms::v3::Request` once `caught_up` becomes true (silently skipping on error/timeout so offline startup keeps working), and call `left.mark` + wake the list for any snapshot room still locally `Joined` that `/joined_rooms` omits. That reuses `LeftRooms` with zero on-disk metadata and closes the restart zombie cleanly.
