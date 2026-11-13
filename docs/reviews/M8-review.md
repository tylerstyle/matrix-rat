# M8 Code Review: Notifications, Threads, Sending Files, and Message Shields

- **Scope:** Local `main` from base `3a84490` through `98179ce` (not pushed):
  1. **Notifications** (`e7f4f36`)
  2. **Threads** (`d9ce6f9` / merge `bc4485b`)
  3. **Sending files** (`97ad313` / merge `6a626b8`)
  4. **Message shields** (`cf2c672`)
  - Note: Inspected on post-merge `main` (`98179ce`) with sub-agent work treated as untrusted code.

---

## Summary Verdicts

| Feature | Commit(s) | Verdict | Blockers / Highest Severity |
| --- | --- | --- | --- |
| 1. Notifications | `e7f4f36` | **Approve with changes** | 3 High (`pending_notices` cap bypass, `OwnMembers` full sync & queue starvation, `EventCache` count-fallback races/stale triggers) |
| 2. Threads | `d9ce6f9` / `bc4485b` | **Approve with changes** | 2 High (`AttachState` room-only key leaks files/drafts across threads, transient `ROOT_ID` placeholder breaks edit/react/scroll on backfill) |
| 3. Sending files | `97ad313` / `6a626b8` | **Approve with changes** | 2 High (`on_paste` bypasses image/help overlays, `AttachState` room-only key collides with threads) |
| 4. Message shields | `cf2c672` | **Approve with changes** | 1 High (`Code::AuthenticityNotGuaranteed => None` silences live `owner_check_failed` forgery warnings & unsafe forwarded sessions) |

Cross-cutting integration bugs spanning multiple features (`AttachState` keying across threads, `Notice` lacking `thread_root`, `Action::Edit` guarding images but not files, and incomplete session cleanup in `reset_session`/`apply_rooms`) should be resolved together before pushing.

---

## 1. Notifications (`e7f4f36`)

### Findings

1. **[High] Flood cap `pending_notices.len() < 5` is bypassed; every notice spawns a thread and fresh D-Bus connection**
   - **Locations:** `src/app.rs:551-553` (`on_notice`), `src/app.rs:578-586` (`take_notices`), `src/app.rs:2271-2292` (`run`), `src/notify.rs:73-86` (`show`), `notify-rust-4.19.0/src/xdg/zbus_rs.rs:57-71`.
   - **Mechanism:** In `run`, the `select!` loop receives a single `MatrixEvent::Notice` per iteration and immediately drains `pending_notices` via `take_notices()` on every loop turn. Consequently `pending_notices.len() < 5` almost never trips across rapid bursts (e.g. a noisy sync batch or flood of pushes). In `NotifyMode::Desktop`, `notify::show` spawns a new OS thread per notice into `notify-rust`, which opens a fresh `zbus::Connection::session().await` D-Bus connection for each notification.
   - **Fix:** Enforce a time-window rate limit / per-room coalescing queue across loop turns rather than a per-tick `Vec` cap, and route desktop notifications through a persistent worker/D-Bus connection.

2. **[High] `OwnMembers` triggers full `sync_members()`, starves beyond slot #2, and wastes its `MOST = 500` budget**
   - **Locations:** `src/matrix/notify.rs:135-173` (`OwnMembers`), `src/matrix/mod.rs:600, 605-611`, `matrix-sdk-0.19.1/src/room/mod.rs:1038-1065, 1169-1184` (`request_members`, `sync_members`).
   - **Mechanism:** `OwnMembers::ensure` calls `room.sync_members()`, which invokes `Room::request_members()` (`get_member_events::v3::Request`, 60s timeout, `retry_limit(3)`), downloading every member event in the room instead of querying only the user's own `m.room.member` state event. Furthermore:
     - `OwnMembers::ensure` is only stepped on `entries.next()` (`src/matrix/mod.rs:605-611`). Once `AT_ONCE = 2` tasks are active, queued rooms (#3+) starve whenever room-list diffs stop arriving.
     - `tried.insert(...)` (`src/matrix/notify.rs:158`) runs before `get_member_no_sync` (`160`) and lacks `!is_space()`, so rooms whose own member is already cached and Space rooms both consume the `MOST = 500` budget, and transient errors never retry.
   - **Fix:** Check `!room.is_space()` and `get_member_no_sync` before counting against `MOST`, fetch only the user's own member state event rather than `sync_members()`, and wake/step queued work when active tasks finish instead of relying solely on room-list diffs.

3. **[High] Count-based fallback relies on client `EventCache`, bypasses `FRESH = 120s`, and races with detailed notices**
   - **Locations:** `src/matrix/mod.rs:722` (`room.num_unread_notifications()`), `src/app.rs:45-46, 558-575, 1678-1684, 1733-1767`, `matrix-sdk-base-0.19.1/src/room/mod.rs:181, 197-199`, `matrix-sdk-base-0.19.1/src/response_processors/timeline.rs:232-239`, `matrix-sdk-0.19.1/src/event_cache/redecryptor.rs:702-734`, `matrix-sdk-base-0.19.1/src/sync.rs:278-296`, `matrix-sdk-0.19.1/src/event_cache/caches/read_receipts.rs:321-325`.
   - **Mechanism:**
     - `summarize` reads `room.num_unread_notifications()` (client-side `EventCache` computation) rather than server-reported `unread_notification_counts().notification_count`. When own member state is missing, `get_push_room_context` returns `Ok(None)`, leaving `num_unread_notifications()` at `0`.
     - `apply_rooms` emits fallback notices on count increases with no event timestamps, so backlog catch-up and `Redecryptor` resolving UTDs (`redecryptor.rs:702-734`, which updates `EventCache` without invoking `notification_handlers`) bypass `FRESH = 120s` and fire stale notifications.
     - Because `matrix-sdk-base` executes `notification_handlers` sequentially (`for fut in futures { fut.await; }` in `sync.rs:278-296`), room list updates can outrun detailed notice delivery by more than `COUNT_NOTICE_WAIT = 1500ms`, causing double notifications (generic count notice followed by detailed notice). Conversely, `DETAILED_COVERS = 10s` suppresses subsequent real fallback notices in the same room for 10 seconds if only the count increases.
     - Additionally, `RoomReadReceiptEventFilter::filter` (`read_receipts.rs:321-325`) excludes threaded events when threading is enabled.
   - **Fix:** Prefer server `unread_notification_counts()` if retaining a count fallback, suppress fallback triggers during initial sync / backfill / redecryption, and key deduplication cleanly instead of using brittle `1500ms`/`10s` timers.

4. **[Medium] `reset_session()` leaks pending and recent notices across logout/login, and `detailed_notices` is never pruned**
   - **Locations:** `src/app.rs:528-533, 589-607, 1590-1593`, `src/matrix/mod.rs:548, 646-649`.
   - **Mechanism:** `reset_session()` clears `rooms` and `threads` but leaves `pending_notices`, `count_notices`, and `detailed_notices` populated, leaking notices across `Logout`/`Expired` into the login screen or a subsequent session. `detailed_notices` entries also never expire/prune from memory.
   - **Fix:** Clear `pending_notices`, `count_notices`, and `detailed_notices` inside `reset_session()`, and prune expired entries from `detailed_notices`.

5. **[Medium] Desktop notification summary (`title`) is not passed through `escape_markup`**
   - **Locations:** `src/notify.rs:75-79, 104-108`, `notify-rust-4.19.0/src/xdg/zbus_rs.rs:27-44`.
   - **Mechanism:** `notify::show` escapes the body via `escape_markup`, but passes raw `title` directly to `notification.summary(&title)`. Notification daemons that render Pango/XML markup in summaries (e.g., Dunst, Mako) can misrender or accept injected markup from untrusted room/sender names containing `<`, `>`, or `&`.
   - **Fix:** Escape (or strip) XML/Pango markup in `title` consistently before passing it to `.summary(...)`.

6. **[Medium] `watching` suppresses thread replies, scrollback, overlays, and relies on DECSET 1004 support**
   - **Locations:** `src/app.rs:257, 543-549, 910-916`, `src/matrix/rooms.rs:103-106`, `src/terminal.rs:81`.
   - **Mechanism:** `watching` only checks `window_focused && modal.is_none() && room.id == notice.room_id`. It suppresses thread-reply notifications (even highlights) while in the main room (`hide_threaded_events: true`), suppresses main-room / sibling-thread notices while inside a thread view, and suppresses notices while scrolled back (`timeline.scroll > 0`), viewing help (`help_open`), or viewing an image (`images.viewer_open()`). Moreover, `window_focused` initializes to `true` (`src/app.rs:257`) via `EnableFocusChange` (`src/terminal.rs:81`); terminals or multiplexers that do not forward focus-out events keep `window_focused == true` permanently.
   - **Fix:** Include `thread_root` in `Notice`, require `timeline.scroll == 0` (in active scope), `!help_open`, and `!images.viewer_open()`, and document focus-tracking limits cleanly.

7. **[Medium] `SETTLE = 20s` begins before `SyncService` build/initial sync and `Stripped(_)` invites lack per-room deduplication**
   - **Locations:** `src/matrix/notify.rs:41, 56-77, 117-119`, `src/matrix/mod.rs:548, 573-578`.
   - **Mechanism:** `SETTLE = 20s` starts when `Watch` is instantiated before `SyncService::build().await` and initial sync finish, so slow startups or offline reconnects >20s notify on old invite backlog. `Stripped(_)` invite notifications also have no per-room deduplication across repeated sync updates.
   - **Fix:** Arm the settle window after initial sync completes and deduplicate stripped-state invites per room ID.

8. **[Low] Terminal escape edge cases, `notification_preview = false` metadata, and format-char sanitization**
   - **Locations:** `src/notify.rs:34-45, 70-101, 112-122`, `src/matrix/notify.rs:33-37, 121-127, 186-187`, `src/text.rs:35-41`, `src/config.rs:27`, `src/modal.rs:182`.
   - **Mechanism:**
     - Kitty `OSC 99` uses fixed `i=matrix-rat` (newer notifications overwrite unread ones) and omits `:p=title` payload tagging.
     - `OSC 9`: untrusted sender/room text starting with `4;` produces `OSC 9;4;...`, colliding with terminal progress-bar sequences.
     - `NotifyMode::Auto`: `kitty()` checks inherited env vars inside `tmux` without DCS passthrough wrapping and has no runtime fallback when D-Bus desktop notification delivery fails.
     - Privacy: `notification_preview = false` (`config.rs:27`, omitted from `Setting::ALL` in `modal.rs:182`) still exposes sender name, room name, and mention/invite status; `describe()` exposes `file.filename()` and raw Matrix reply fallback text.
     - `terminal_sequence` relies on callers for BIDI stripping and does not cap UTF-8 byte length; `unsafe_char` (`src/text.rs:35-41`) omits `U+2028`, `U+2029`, and format chars like ZWSP.
   - **Checked OK:** `Notice`'s custom `Debug` impl redacts content (`src/matrix/notify.rs`), and C0/C1/DEL control chars are stripped in both `sanitize_line` and `terminal_sequence`.

### Verdict: Approve with changes

Resolve the `pending_notices` flood-cap bypass / per-notice thread+D-Bus spawn, `OwnMembers` full-room sync & starvation, `EventCache` count-fallback races/stale triggers, `reset_session()` leak, summary markup escaping, and thread/overlay `watching` gaps before pushing.

---

## 2. Threads (`d9ce6f9` / merge `bc4485b`)

### Findings

1. **[High] `AttachState::pending` and `AttachState::stash` key solely on `room_id`, silently leaking files and `/upload` drafts across threads**
   - **Locations:** `src/app/attach.rs:20-27, 42-47, 51-58, 117-142, 226-290`, `src/ui/composer.rs:24, 71-73`, `src/app.rs:703-707`, `src/app/thread.rs:179-196`.
   - **Mechanism:** `AttachState` stores both `pending` attachments and `stash`ed `/upload` drafts keyed only by `room_id`. Staging a file or `/upload` draft in the main room and opening a thread (or vice versa) keeps the chips visible and causes `send_attachments` to send via `in_thread` (`SendFiles`) into the wrong conversation scope. Entering/leaving `/upload ` across threads also overwrites/restores drafts from the wrong scope.
   - **Fix:** Key `AttachState::pending` and `stash` by room + thread scope `(RoomId, Option<EventId>)` matching `Threads` draft scoping.

2. **[High] `ROOT_ID` (`"thread-root"`) placeholder breaks thread root actions before backfill and disrupts edit/reply/selection/scroll when real root arrives**
   - **Locations:** `src/app/thread.rs:15, 88-92, 206-219`, `src/matrix/convert.rs:28`, `src/app.rs:969-1000, 1436-1454`, `src/ui/timeline.rs:598-605`, `matrix-sdk-0.19.1/src/event_cache/caches/thread/pagination.rs:292-307`.
   - **Mechanism:** `matrix-sdk` thread timelines omit the thread root event until back-pagination reaches the start of the thread (`new_token.is_none()`). While missing, `app/thread.rs` injects a copy of the room message with `id = ROOT_ID` (`"thread-root"`) and `ItemRef::Event(root_id)`. `Edit`, `ToggleReaction`, and `Redact` on `"thread-root"` dispatch to the thread `Timeline` before that event item exists there. When pagination eventually loads the real root, it uses the SDK's `unique_id()`, causing `"thread-root"` to vanish: `apply_timeline()` fails to match `m.id`, canceling active `ComposeMode::Edit(0)` (`end_edit()`), resetting `ComposeMode::Reply(0)`, jumping selection to bottom, dropping in-flight `pick_emoji()` reactions (`m.id == id`), and losing the `RowCache` scroll anchor.
   - **Fix:** Key thread-root actions/selection/cache stability cleanly across placeholder-to-real-root replacement (or fetch/insert the real root / resolve actions via `EventId` where appropriate rather than letting a synthetic `ROOT_ID` vanish mid-session).

3. **[Medium] `MarkRead` updates `ReadMarks` even on failure / out-of-order races, and `latest_event_id()` includes reactions/redactions unlike `compute_thread_summary`**
   - **Locations:** `src/matrix/rooms.rs:307-321`, `src/matrix/threads.rs:89-136`, `src/app.rs:910-934`, `matrix-sdk-ui-0.19.1/src/timeline/controller/mod.rs:1851-1900`, `matrix-sdk-0.19.1/src/event_cache/caches/thread/state.rs:302-354`.
   - **Mechanism:** `MarkRead` calls `marks.set(key, newest)` unconditionally regardless of `timeline.mark_as_read(receipt)` errors or concurrent task completion order. In addition, `timeline.latest_event_id()` sets `filter_out_thread_events = false` for `TimelineFocusKind::Thread` (`controller/mod.rs:1851-1900`), so it can return reaction/redaction events whereas `compute_thread_summary` (`state.rs:302-354`) filters them out via `filter_timeline_event`, creating `ReadMarks::covers` mismatches. Opening a thread also triggers an immediate `MarkRead` on `[first]` before replies arrive (corrected later on snapshot).
   - **Fix:** Update `marks.set` only on successful `mark_as_read` (preserving monotonic ordering), skip premature `MarkRead` on the placeholder-only state, and align event matching with `compute_thread_summary`.

4. **[Medium] Logout `MatrixEvent::Rooms` race and missing cleanup in `apply_rooms`**
   - **Locations:** `src/app.rs:528-533, 589-607, 1614, 1786-1812`.
   - **Mechanism:** After `reset_session()` on logout/session expiry, an already queued `MatrixEvent::Rooms` still runs through `on_matrix` (`user_id` is not checked), temporarily repopulating `self.rooms` on the login screen and issuing `request_timeline()`. Separately, in `apply_rooms`'s `None` branch when an active room disappears (including while viewing its thread), `row_cache`, vanished-room `attach.{pending,stash}`, and empty-room `Pane::Timeline` focus are not cleaned up.
   - **Fix:** Guard `MatrixEvent::Rooms` on an active logged-in session and reset `row_cache`, removed rooms' attachment state, and focus when rooms vanish.

5. **[Medium] `threads::unread` creates unbounded `ThreadEventCache` entries on every timeline update**
   - **Locations:** `src/matrix/rooms.rs:448-453, 479, 491`, `src/matrix/threads.rs:116-135`, `matrix-sdk-0.19.1/src/event_cache/caches/mod.rs:186-226`.
   - **Mechanism:** `threads::unread` is called on timeline updates across rooms and calls `cache.thread(root)` for every unread/uncovered thread summary. `EventCacheInner::thread` inserts into `self.threads: HashMap<OwnedEventId, ThreadEventCache>` without eviction, accumulating thread caches in memory across all visited timelines.
   - **Fix:** Avoid materializing persistent `ThreadEventCache` instances for passive summary unread annotations or bound/throttle thread unread checks.

6. **[Medium] Notifications for main room and sibling threads are suppressed inside a thread**
   - **Locations:** `src/app.rs:543-549`, `src/matrix/notify.rs:22-30`.
   - **Mechanism:** `Notice` lacks `thread_root`, so `watching` treats viewing any thread in `room_id` as watching the whole room, suppressing desktop/terminal notifications for new main-room messages or other threads in the same room.
   - **Fix:** Include `thread_root: Option<OwnedEventId>` in `Notice` and match against active room vs active thread.

7. **[Low] Initial thread snapshot `reached_start: false`, demo deletion, and `KEEP_TIMELINES = 6` slot contention**
   - **Locations:** `src/matrix/rooms.rs:94-99, 122-125, 444`, `src/app/thread.rs:155-166`.
   - **Mechanism:** Initial thread `snapshot` sets `reached_start: false` because `TimelineFocus::Thread` has no `TimelineStart` item (discovered on `Paginate`). Demo-mode deletion of `ROOT_ID` leaves the main room's root message untouched. Thread timelines also occupy slots in `KEEP_TIMELINES = 6` (though an active `forwarder` still holds `Arc<Timeline>`).
   - **Checked OK:** `InThread` routing, `generation` stale-snapshot protection, `Timelines::close`, `open_room`/`step_room`/`apply_rooms` room-index adjustments, `SendFiles` + `infer_reply` preserving `EnforceThread::Threaded`, `reset_session` clearing `threads`, and command-palette `OpenThread`/`CloseThread` gating.

### Verdict: Approve with changes

Scope `AttachState` by room + thread, fix the transient `ROOT_ID` (`"thread-root"`) replacement disruption and pre-backfill root actions, fix `MarkRead` unconditional advancement and `reset_session`/`apply_rooms` races/cleanup, and pass `thread_root` in `Notice`.

---

## 3. Sending Files (`97ad313` / merge `6a626b8`)

### Findings

1. **[High] `on_paste` bypasses full-screen image viewer and help overlays**
   - **Locations:** `src/app.rs:1891-1903, 1992-2045`, `src/app/attach.rs:72-74`, `src/ui/mod.rs:74-81`, `src/ui/image.rs:193`, `src/ui/help.rs:55, 69-73`.
   - **Mechanism:** Unlike `on_key`, `on_paste` only checks `self.modal` and otherwise executes `attach_pasted` / `composer.insert` even when `images.viewer_open()` or `help_open` covers the screen, silently attaching files and altering focus/drafts behind overlays.
   - **Fix:** Short-circuit `on_paste` cleanly when `help_open` or `images.viewer_open()` is open.

2. **[High] `AttachState::pending` and `stash` key only on `room_id` (cross-thread upload/draft leakage)**
   - **Locations:** `src/app/attach.rs:20-27, 42-47, 51-142, 226-322`, `src/ui/composer.rs:24, 71-73`.
   - **Mechanism:** Cross-referenced with Threads Finding #1: `pending` attachments and `stash`ed `/upload` drafts ignore the active `thread_root`, so switching between room and thread carries pending uploads across scopes and dispatches them to the wrong target.
   - **Fix:** Scope `AttachState` storage by room + optional `thread_root`.

3. **[Medium] Short terminal heights hide all queued attachments (`file_rows == 0`) while `Enter` still sends them**
   - **Locations:** `src/ui/composer.rs:70-83`, `src/ui/attach.rs:33-37`.
   - **Mechanism:** When `text_area.height <= 1` on short terminals, `file_rows` clamps to `0`, hiding all queued attachment chips even though pressing `Enter` still uploads and sends them. When `file_rows == 1` with `>= 2` files, all filenames collapse into `"📎 … and N more files"`.
   - **Fix:** Ensure at least one visible attachment indicator row remains rendered when files are queued (or refuse blind send if hidden), and include the first filename alongside `(+N more)` in single-row mode.

4. **[Medium] Stale `attach.stash` after `Ctrl-C`, leftover `/upload ` on drop, `ComposeMode::Edit` precedence, and missing `file` check on `Action::Edit`**
   - **Locations:** `src/app.rs:703-707, 1237-1244, 1906-1915, 2030`, `src/app/attach.rs:117-166, 257-260`.
   - **Mechanism:**
     - `Ctrl-C` clears `pending` attachments but leaves `attach.stash` intact, causing a future `/upload` invocation to restore an old stashed draft.
     - Dropping a file while `/upload ` is typed in the composer queues the file but leaves `/upload ` active in the composer.
     - `send()` checks `upload_command() || send_attachments()` before `ComposeMode::Edit`, deadlocking message edits whenever files are staged or edited text starts with `/upload ` or `/file `.
     - `Action::Edit` (`src/app.rs:1237-1244`) rejects `message.image.is_some()` but omits `message.file.is_some()`.
   - **Fix:** Clear `attach.stash` on `Ctrl-C`, strip `/upload ` when drop-attaching, process `ComposeMode::Edit` before upload-command/attachment send paths (or disallow upload commands while editing), and reject `message.file.is_some()` in `Action::Edit`.

5. **[Medium] `upload::picture` omits `ImageReader` decoder `Limits` and uses unchecked `width * height * 4`**
   - **Locations:** `src/upload.rs:389-424` vs `src/images.rs:205-224`.
   - **Mechanism:** Unlike `images::decode` (which sets `Limits` from `MAX_DECODE_SIDE = 16_384` and `MAX_DECODE_ALLOC = 256 MiB` before `.into_decoder()`), `upload::picture` passes no `Limits` to `ImageReader` and computes `u64::from(width) * u64::from(height) * 4` without checked/saturating arithmetic (allowing debug overflow panic or release wrap-around bypassing the `as_rgba` guard).
   - **Fix:** Apply `Limits` before `.into_decoder()` (matching `src/images.rs`) and use checked/saturating multiplication.

6. **[Medium] Multi-file upload shifts or loses `caption` / `reply_to` on failure and surfaces raw `{err}`**
   - **Locations:** `src/app/attach.rs:273-321`, `src/matrix/upload.rs:83-120`.
   - **Mechanism:** In `Uploads::send`, `caption` and `reply_to` are moved via `.take()` on the first file that succeeds `prepare()`. If file #1 fails `prepare()` (e.g., TOCTOU removal or stale `FALLBACK_LIMIT`), `caption`/`reply_to` silently attach to file #2 instead; if `prepare()` succeeds on file #1 and `timeline.send_attachment(...).use_send_queue().await` fails afterward, `caption` and `reply_to` are lost completely. Additionally, line 117 passes raw `{err}` to `ActionFailed` instead of `short()`.
   - **Fix:** Bind `caption`/`reply_to` explicitly without shifting unexpectedly on partial failure or losing them across queue errors, and format failures with `short()`.

7. **[Low] URI/path inconsistencies, dotfile paste hijacking, invite-room staging, sync completion `stat`s, and whole-file buffering**
   - **Locations:** `src/upload.rs:37-52, 178-198, 210-222, 248, 287-305, 363-367`, `src/app/attach.rs:59, 81, 117`.
   - **Mechanism:**
     - `dropped_path` accepts `file://...?query#frag`, whereas `/upload file:///...` does not handle `file://` URIs equivalently.
     - Standalone pasted absolute/`~/` paths (including dotfiles) always hijack pastes into attachments instead of text; `MAX_DROP_FILES` checks count before deduplicating and inserts raw paths as text on >20 files instead of warning.
     - Unset `HOME` falls back to `"."`; `attach_files` dedup skips symlink canonicalization.
     - `attach_pasted` / `begin_attach` allow staging files in `RoomKind::Invite`, inconsistent with `src/palette.rs:435`.
     - `complete()` performs synchronous `stat` calls for up to `MAX_ENTRIES = 20_000` entries on the UI thread; `prepare()` buffers the full file into `Vec<u8>`.
   - **Checked OK:** `upload::open` uses `O_NONBLOCK` + `fstat` regular-file verification on the open fd + `.take(most + 1)` bounded read, filenames and UI errors are sanitized, and tracing/`Debug` avoid leaking sensitive full paths.

### Verdict: Approve with changes

Guard `on_paste` behind image/help overlays, key `AttachState` by room + thread, add decoder `Limits` + checked byte calculation in `upload::picture`, keep attachment indicators visible on short terminals, fix `Edit`/cancel/`Ctrl-C`/multi-file caption edge cases, and sanitize `Uploads::send` failure text.

---

## 4. Message Shields (`cf2c672`)

### Findings

1. **[High] `Code::AuthenticityNotGuaranteed => None` silences cryptographic `owner_check_failed` forgery warnings and unsafe key forwards**
   - **Locations:** `src/matrix/convert.rs:287-303` (`shield`), `matrix-sdk-crypto-0.19.1/src/machine/mod.rs:3271-3290` (`sender_data_to_verification_state`), `matrix-sdk-crypto-0.19.1/src/olm/group_sessions/sender_data_finder.rs:227-235` (`have_device`).
   - **Mechanism:** `src/matrix/convert.rs:287-289` suppresses `Code::AuthenticityNotGuaranteed` based on the assumption that it only flags backup-downloaded history. In `matrix-sdk-crypto`, `DeviceLinkProblem::InsecureSource` (mapped to `ShieldStateCode::AuthenticityNotGuaranteed`) covers both:
     1. `UnknownDevice { owner_check_failed: false }` when `session_has_been_imported` is true (key backups, file imports, **and forwarded room keys**), **and**
     2. **live `UnknownDevice { owner_check_failed: true }`** (`sender_data_finder.rs:231-234`), emitted when a device is found for the sender curve key **but fails `is_owner_of_session`**.
   - Returning `None` for `AuthenticityNotGuaranteed` completely hides forged/spoofed live device-session claims and untrusted key forwards alongside backup-downloaded history.
   - **Fix:** Stop silently dropping `Code::AuthenticityNotGuaranteed`. Surface a distinct warning shield (or distinguish backup-imported history from live `InsecureSource` / forwarded keys at the SDK/crypto boundary rather than mapping all `AuthenticityNotGuaranteed` to `None`).

2. **[Medium] `get_shield(false)` makes `Code::UnverifiedIdentity => None` dead code and hides unverified-sender state**
   - **Locations:** `src/matrix/convert.rs:293, 301`, `matrix-sdk-ui-0.19.1/src/timeline/event_item/mod.rs:433-452`, `matrix-sdk-common-0.19.1/src/deserialized_responses.rs:104-188`.
   - **Mechanism:** `event.get_shield(false)` selects lax mode (`to_shield_state_lax()`), which itself returns `ShieldState::None` for `VerificationLevel::UnverifiedIdentity` (`deserialized_responses.rs:143-147`), making the `Code::UnverifiedIdentity => None` arm at `convert.rs:301` unreachable dead code. As a result, messages from completely unverified users/devices show no shield at all (only `VerificationViolation`, i.e., previously verified then broken, renders in red).
   - **Fix:** Remove the dead `Code::UnverifiedIdentity` branch if lax mode is intentionally chosen and document that policy clearly, or switch to `get_shield(true)` (`to_shield_state_strict()`) if unverified identities should be visibly distinguished.

3. **[Medium] Redacted events in encrypted rooms can display false `"not encrypted"` shields**
   - **Locations:** `src/matrix/convert.rs:81, 204-207`, `src/ui/timeline.rs:115-127, 237, 267-326`, `matrix-sdk-ui-0.19.1/src/timeline/event_item/mod.rs:408-458`.
   - **Mechanism:** `EventTimelineItem::get_shield` only exempts `!is_room_encrypted || is_local_echo() || content.is_unable_to_decrypt() || live_location_state` — it does **not** exempt `MsgLikeKind::Redacted`. Redacted timeline items whose `encryption_info()` is `None` yield `Red { code: SentInClear }`. `MsgKind::System` bypasses `marker(message)` (`ui/timeline.rs:115-127`), whereas `MsgKind::Redacted` still runs through `marker(message)` (`ui/timeline.rs:237, 290-317`), falsely annotating `"Message deleted not encrypted"`.
   - **Fix:** Force `shield: None` for `MsgLikeKind::Redacted` (and keep non-user/redacted markers exempt).

4. **[Low] Style coupling on `shield` + `(edited)` markers and narrow-width truncation**
   - **Locations:** `src/ui/timeline.rs:57-75, 254-317`, `src/model.rs:102-127`, `matrix-sdk-ui-0.19.1/src/timeline/controller/aggregations.rs:1054-1124`.
   - **Mechanism:** In `ui/timeline.rs:58-73`, when `shield == Some(Shield::NotEncrypted)` and `edited == true`, `shield.serious()` is `false`, so `"not encrypted (edited)"` shares `Role::Muted`; conversely, when `shield.serious()` is `true` and `edited == true`, `(edited)` inherits `Role::Error`. On narrow chat panes, `truncate(&label, body_width)` (`ui/timeline.rs:259, 313`) can clip `(edited)` or the explanatory noun of long labels (e.g. `"⚠ sender's identity has changed"`).
   - **Checked OK:** `matrix-sdk-ui` updates `encryption_info` cleanly when applying edits (`aggregations.rs:1121-1123`), `apply_timeline` bumps `rev` so `RowCache` invalidates properly, and `ui/timeline.rs:657-667` renders `row.line` after `MessageSelected` background fill so `Role::Error` foreground color stays legible on selected rows.

### Verdict: Approve with changes

Do not blanket-suppress `Code::AuthenticityNotGuaranteed` (which also hides live `owner_check_failed: true` sender-device spoofing and untrusted forwarded keys), remove or intentionally switch `get_shield` around `Code::UnverifiedIdentity`, and clear shields on redacted items (`MsgLikeKind::Redacted`).
