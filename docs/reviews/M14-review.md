# Review: M14 — Persisted drafts & `Action::EditLast`

**Branch / commit reviewed:** `drafts-edit` at `ca3b85f` (`Drafts survive a restart; Up in an empty composer edits the last message`, on `main` `6a492aa`; 16 files, `+1122` / `-49`)  
**Files inspected:** `README.md`, `src/action.rs`, `src/keymap.rs`, `src/model.rs`, `src/palette.rs`, `src/composer.rs`, `src/app.rs`, `src/app/drafts.rs`, `src/app/attach.rs`, `src/app/thread.rs`, `src/app/menu.rs`, `src/app/rooms.rs`, `src/app/keybinds.rs`, `src/matrix/mod.rs`, `src/matrix/drafts.rs`, `src/matrix/rooms.rs`, `src/matrix/manage.rs`, `src/matrix/session.rs`, `src/ui/composer.rs`, `src/ui/status.rs`, `matrix-sdk-0.19.1` (`src/room/mod.rs`), `matrix-sdk-sqlite-0.19.1` (`src/state_store.rs`, `src/connection.rs`), `matrix-sdk-ui-0.19.1` (`src/timeline/mod.rs`).

---

## Verdict

**Changes requested (one `[HIGH]` draft-loss race and one `[MEDIUM]` room-leave cleanup gap).**  
Draft persistence across clean quit (`Ctrl-Q`, `SIGINT`, `SIGTERM`, `SIGHUP`), `Debug`/log redaction, `Send`/`SaveDraft(..., 0 bytes)` suppression, logout store deletion, and the core `EditLast` UX (`↑` in empty composer editing the newest own `Message::editable()`, falling back to `SelectOlder`) are solid. However, `on_stored_draft` arms a store overwrite whenever `ComposeMode::Edit`, `ComposeMode::Reply`, or `/upload` starts before `MatrixEvent::Draft` arrives, erasing the unseen stored draft 3 seconds later (or on room switch/quit); and leaving/forgetting a room leaves its `ComposerDraft` `kv_blob` entries intact in SQLite while `Timelines::drafts_told` and UI draft maps stay out of sync if rejoined.

---

## Check answers

### 1. Can a late `MatrixEvent::Draft` ever overwrite or lose typed text?
- **Typed draft text in the composer or room/thread stashes is protected:**
  - `on_stored_draft` (`src/app/drafts.rs:143-178`) skips any scope where `self.drafts.stored.contains_key(&scope)` (already saved in this session).
  - When the user already moved elsewhere, `keep_stored_draft` (`src/app/thread.rs:246-270`) only inserts if `draft.is_empty()` (for rooms / `view.room_draft`) or via `.entry(...).or_insert(text)` (for threads).
  - When still in that scope, `free = self.composer.is_empty() && self.compose_mode == ComposeMode::Normal && !self.drafts.touched` refuses to touch the composer if anything was typed or erased (`touched == true`), or if an edit/reply is active.
  - On room/thread switch while in `ComposeMode::Edit`, `open_room` (`src/app.rs:1035-1037`), `open_thread` (`src/app/thread.rs:103-105`), and `leave_thread` (`src/app/thread.rs:167-173`) call `self.save_draft()` (which reads `self.edit_stash` via `draft_text`) **before** `self.cancel_compose()`, so neither the active edit text nor the stashed draft is corrupted.
- **However, see Finding #1 (`[HIGH]`) and Finding #3 (`[LOW]`):**
  - If `Edit` (`↑` or `e`), `Reply`, or `/upload` (`Alt-A` or typing `/upload ...`) begins **before** `MatrixEvent::Draft` arrives, `on_stored_draft` drops into the `else` arm (`line 176`) and arms `self.drafts.changed = Some(Instant::now())`, overwriting the stored draft with `""` 3 seconds later (or immediately on leave/quit).

### 2. Is a draft really on disk when the app quits or is killed?
- **Clean quit (`Ctrl-Q`, command palette quit, `SIGINT`, `SIGTERM`, `SIGHUP`, `EventStream` EOF):** **Yes.**
  - `app::run` (`src/app.rs:3059-3060, 3136-3151`) handles `ctrl_c`, `sigterm`, and `sighup` by setting `app.should_quit = true`, then runs `app.shut_down_worker()` (`self.save_draft()` — wrapped by `in_thread` when inside a thread — followed by `MatrixCmd::Shutdown` on an unbounded FIFO `mpsc` channel), drops `matrix` and `app`, and awaits `events.recv()` for up to 3s.
  - In `run_session` (`src/matrix/mod.rs:922-963`), commands come off `self.commands` in order: `MatrixCmd::SaveDraft` calls `timelines.handle(command).await`, which **awaits** `drafts::save(...)` inline (`src/matrix/rooms.rs:455-467`) before `select!` ever pulls `MatrixCmd::Shutdown`.
  - `drafts::save` (`src/matrix/drafts.rs:16-32`) calls `room.save_composer_draft` / `room.clear_composer_draft` -> `SqliteStateStore::set_kv_data` / `remove_kv_data` (`matrix-sdk-sqlite-0.19.1/src/state_store.rs:625, 1180-1182, 1291-1299`), committing into SQLite WAL mode synchronously on await. Verified live with `matrix::drafts::tests::draft_survives_a_restart_and_can_be_cleared` against continuwuity.
- **Caveats on abrupt kill (`SIGKILL`, panic, event-stream error):**
  - On `SIGKILL` or panic (`terminal.rs:184` panic hook and `events.next()` `Err` early return at `src/app.rs:3123` skip `shut_down_worker()`), up to `drafts::IDLE` (`3s`, `src/app/drafts.rs:19`) plus up to `250ms` tick interval (`src/app.rs:3018, 3056`) of unswitched edits in the active scope can be lost.
  - Note also that `main.rs:97` exits once the 3s drain timeout in `app::run:3150` expires (only relevant if an earlier worker step stalls before `SaveDraft`, since `verifier.stop()`/`sync.stop()` run *after* `SaveDraft` has already finished).

### 3. Does text ever reach `Debug`/log output?
- **No.** Both `MatrixCmd::SaveDraft` and `MatrixEvent::Draft` implement custom `Debug` (`src/matrix/mod.rs:232-235, 448-456`), printing only `room_id`, optional `thread` event id, and `text.len()` (`SaveDraft(!a, 6 bytes)`, `Draft(!a, $e0, 6 bytes)`), and `MatrixCmd::InThread` delegates to `{command:?}`. Checked all `tracing::` calls across `src/`: only structured error values, room IDs, or counts are logged (`src/matrix/rooms.rs:164, 465` log `?err` only). Verified by `drafts_are_not_printed` (`src/app/drafts.rs:530-547`).

### 4. Drafts of a room after logout or leaving the room?
- **Explicit logout (`Setting::Logout`):** Properly cleared in both memory and disk. `Setting::Logout` (`src/app.rs:823-828`) sends `MatrixCmd::Logout` and calls `self.reset_session()` (resets `self.drafts`, `composer`, `rooms`, `threads`, `edit_stash`, `attach`, sets `logged_out = true`; late `MatrixEvent::Draft` events are ignored at `src/app.rs:2267`). On the worker (`src/matrix/mod.rs:640-658`), `SessionEnd::LoggedOut` calls `client.logout()` and `session::clear` (`src/matrix/session.rs:292-304`), deleting `session.json`, the keyring entry, `store_dir()`, and `store.previous` — removing all SQLite `ComposerDraft` entries. (Soft logout keeps the store/passphrase intentionally so reauth preserves drafts; expired sessions delete `session.json`/keyring via `session::forget` and move `store` to `store.previous` at next fresh login.)
- **Leaving / forgetting a room (`Action::LeaveRoom`, declined invite, or kicked):** **Incomplete (Finding #2 `[MEDIUM]`).** Neither `kv_blob` nor worker/UI draft caches are cleaned up when a room is left or disappears.

### 5. Changed meaning of `Up` in the composer (tests moved to `Alt-Up`) and the `vim` preset
- `DEFAULT` rebinds Composer `Up` from `Action::SelectOlder` to `Action::EditLast` (`src/keymap.rs:617-626`), while `Global` keeps `Alt-Up` -> `Action::FocusDir(Dir::Up)`.
- In `on_key` (`src/app.rs:2535-2543`), bare `Up` still moves up inside a wrapped/multi-line draft whenever `!composer.on_first_row(width)`. From row 0, `EditLast` (`src/app.rs:1798-1801, 1491-1499`) opens `ComposeMode::Edit(i)` on the newest own `Message::editable()` (`src/model.rs:290-295`: plain text or `/me`, not file/image/redacted), unless that message is `SendState::Failed` (in which case it jumps to Timeline so retry/discard remain visible); `SendState::Sending` *is* editable and `matrix-sdk-ui-0.19.1/src/timeline/mod.rs:577-592` supports editing local send-queue items via `TimelineItemHandle::Local`. Otherwise `EditLast` falls back to `SelectOlder`.
- **Tests moved to `Alt-Up`:** 14 `alt_up()` calls were updated across `src/app.rs` (8), `src/app/menu.rs` (5), and `src/app/palette.rs` (1) because rooms where the user has sent an editable message (`App::demo()`'s newest message is own) now enter `EditLast` instead of `Timeline` on bare `Up` from an empty composer. For users with `DEFAULT` bindings in active rooms, `Alt-Up` is now the keyboard route from an empty composer to `Timeline` (note a minor pre-existing behavioral difference: when `timeline.scroll > 0`, `FocusDir(Dir::Up)` -> `set_focus(Pane::Timeline)` at `src/app.rs:999-1004` focuses `Timeline` without selecting the newest message until a subsequent `Up`, whereas `SelectOlder` always selected newest).
- **`vim` preset:** `Up` starts `EditLast` (`up_edits_in_the_vim_preset_too`, `src/app.rs:3431-3446`), and `Esc` returns focus to `Timeline` (`src/keymap.rs:752-758`) without cancelling the edit (`Action::Back` in `Timeline` returns to `Composer`, so `Esc` ping-pongs). `README.md:91` documents `Ctrl-C` to cancel in `vim` preset, though there are UX/edge gaps in the app itself (Finding #4 `[LOW]`).

### 6. The extra `SaveDraft(..., 0 bytes)` after a send
- **Intentional and safe.** `send()` (`src/app.rs:1228`) and `send_attachments()` (`src/app/attach.rs:393`) call `forget_sent_draft()` -> `write_draft(true)` (`src/app/drafts.rs:97-122`).
- When `self.drafts.stored` already knows the store is `""` (`sending_clears_the_stored_draft`, `src/app/drafts.rs:410-415`), no `SaveDraft` is sent.
- An extra `SaveDraft(!b, 0 bytes)` (as seen in `typing_notices_and_retry`, `src/app.rs:4189-4193`, where `!b` was opened on a fake channel without a `MatrixEvent::Draft` reply) only happens once per scope when `self.drafts.stored` has no entry yet (`None`), so both disk is cleared and `stored` records `""`, preventing a slow in-flight `MatrixEvent::Draft(!b, "old")` from resurrecting after send. Note also that if `SendFiles` fails (`on_files_not_sent`, `src/app/attach.rs:401-415`), the restored caption in `composer` is picked up by `note_draft()` on the next event / 250ms tick and re-persisted.

---

## Findings

### 1. `[HIGH]` Late `MatrixEvent::Draft` wipes an unseen stored draft 3s later if `Edit`, `Reply`, or `/upload` starts first
- **Where:** `src/app/drafts.rs:46-62` (`draft_text`), `src/app/drafts.rs:143-178` (`on_stored_draft`), `src/app/drafts.rs:294-311` (`a_stored_draft_stays_out_of_a_reply_and_of_an_edit`).
- **Detail:** Suppose room `!a` (or thread `$e0`) has `"old"` on disk and the user acts in the brief window after opening it before `MatrixEvent::Draft` arrives (`drafts.stored` still has no key, `drafts.touched == false`):
  1. **Edit (`↑` / `EditLast` in empty composer, or `e`):** `begin_edit` sets `edit_stash = Some("")` and `compose_mode = ComposeMode::Edit(i)`. When `MatrixEvent::Draft(!a, "old")` arrives, `on_stored_draft` inserts `"old"` into `drafts.stored`, sees `free == false`, and sets `self.drafts.changed = Some(Instant::now())` (`line 176`). Three seconds later (or on room switch / quit), `draft_text()` reads `edit_stash` (`""`) != `"old"` and sends `SaveDraft(!a, "")`, deleting `"old"` from disk and keeping `edit_stash` empty.
  2. **Reply (`ComposeMode::Reply(i)` while composer is still empty):** `free` is `false` (`compose_mode != Normal`), `line 176` arms `changed`, and pausing 3s before typing (or switching rooms / quitting) sends `SaveDraft(!a, "")` and wipes `"old"` from SQLite.
  3. **`/upload` (`Alt-A` / `begin_attach` or typing `/upload ...` while draft was empty):** `draft_text()` sees `upload::command_argument("/upload ...").is_some()` and reads `attach.stashed(scope)` (`""`), so `note_draft()` leaves `touched == false`. When `Draft(!a, "old")` arrives while the user is typing or tab-completing a path, `!composer.is_empty()` makes `free == false`, `line 176` sets `changed`, and 3s later `SaveDraft(!a, "")` deletes `"old"` from disk.
  - `a_stored_draft_stays_out_of_a_reply_and_of_an_edit` (`lines 294-311`) tested only immediate `composer.text()` right after `on_stored_draft`, without running `save_idle_draft` or `shut_down_worker`, so the armed overwrite went unnoticed.
- **Suggested fix:** When `!self.drafts.touched` and the stored draft is non-empty, preserve `"old"` behind `Edit` (into `edit_stash` if `Some("")`) and `/upload` (into `attach.stash` if absent), rather than scheduling `changed` to overwrite it with `""`. For `ComposeMode::Reply(_)` when `composer.is_empty() && !self.drafts.touched` (matching the existing `a_stored_draft_stays_out_of_a_reply_and_of_an_edit` contract), either also restore `"old"` (since entering Reply *after* `Draft` arrived already keeps `"old"` in the composer) or keep `"old"` out without setting `self.drafts.changed = Some(...)` when `self.draft_text(&scope).is_empty() && !self.drafts.touched` (and handle restoring/clearing cleanly on `CancelCompose` vs send). Add regression tests running `save_idle_draft(Duration::ZERO)` and `shut_down_worker()` after late `Draft` during `Edit`, `Reply`, and `/upload`.

### 2. `[MEDIUM]` Leaving or forgetting a room leaves `ComposerDraft` entries in SQLite and stale draft caches in memory
- **Where:** `src/matrix/manage.rs:639-658` (`leave`), `src/matrix/rooms.rs:50-52, 148-166` (`drafts_told`, `tell_draft`), `src/app.rs:2379-2411` (`apply_rooms`), `matrix-sdk-sqlite-0.19.1/src/state_store.rs:2001-2048` (`SqliteStateStore::remove_room`).
- **Detail:**
  - `room.forget()` calls `SqliteStateStore::remove_room(&room_id)`, which cleans `room_info`, `state_event`, `member`, `profile`, `room_account_data`, `receipt`, `display_name`, `send_queue_events`, `dependent_send_queue_events`, and `thread_subscriptions`, **but never removes `kv_blob` entries** (`StateStoreDataKey::ComposerDraft(room_id, thread_root)`, `state_store.rs:534-543`).
  - Neither `manage::leave` nor `apply_rooms` (when a left/kicked room disappears) clears the room's stored drafts, `Timelines::drafts_told`, `self.drafts.stored`, or `self.threads.drafts` (note also the comment at `src/app.rs:2381-2382`, `"A draft for the vanished room has nowhere to go"` — if the user paused 3s in that room, e.g. while opening Room Info to leave, the draft is already on disk and stays orphaned).
  - If the user later rejoins the same `room_id`: across a restart, old room/thread drafts reappear in the composer; within the same session, `Timelines::drafts_told` suppresses re-reading while `self.drafts.stored` and `self.threads.drafts` hold stale entries from before the leave.
- **Suggested fix:** Clear `ComposerDraft` entries for the left/vanished room before `room.forget()` (note `room.clear_composer_draft` needs known `thread_root` keys, so clearing both `None` and any known thread keys from `threads.drafts`/`drafts.stored` — or wiping `composer_draft:<room_id>*` when leaving — avoids leftovers), and prune the room's entries from `self.drafts.stored`, `self.threads.drafts` (`attach.pending`/`stash`, `complete.draft_marks`/`thread_marks`), and `Timelines::drafts_told` when a room leaves.

### 3. `[LOW]` Fast type-erase-switch race before initial `MatrixEvent::Draft` arrives
- **Where:** `src/app/drafts.rs:91-122` (`save_draft` -> `write_draft(false)`), `src/app/drafts.rs:143-178` (`on_stored_draft`).
- **Detail:** If room `!a` (or thread `$e0`) has `"old"` on disk and, before `MatrixEvent::Draft(!a, "old")` arrives, the user types `"x"`, backspaces back to `""` (`drafts.touched = true`), and immediately switches away, `save_draft()` calls `write_draft(false)`. Because `!a` is still `None` in `self.drafts.stored` and `text` is `""`, `same == true` skips `SaveDraft` and leaves `!a` out of `self.drafts.stored`. When `MatrixEvent::Draft(!a, "old")` arrives next, `on_stored_draft` sees `!stored.contains_key(!a)` and calls `keep_stored_draft`, putting `"old"` back into `rooms[0].draft` / `threads.drafts`. Note that `write_draft(false)` is intentional so merely switching through an untouched room doesn't wipe an unseen draft; passing `self.drafts.touched` as `unknown_differs` would suppress this only after deliberate editing.

### 4. `[LOW]` `vim` preset edit cancellation UX & emptied-edit `Ctrl-C` trap
- **Where:** `src/keymap.rs:620-626, 752-758`, `src/app.rs:2520-2530, 2570-2580, 3431-3446`, `src/ui/status.rs:105-124`, `src/ui/composer.rs:50`, `README.md:91`.
- **Detail:**
  1. Because `Preset::Vim` binds Composer `Esc` to `Focus(Pane::Timeline)` and `Action::Back` in `Timeline` moves back to `Composer`, `Esc` cannot cancel an edit started by `↑` in `vim` preset. Only `README.md:91` mentions `Ctrl-C` as the escape hatch; neither the edit banner (`src/ui/composer.rs:50`, `"✎ Editing your message"`) nor the composer status bar (`src/ui/status.rs:105-124`, which switches to `"↑ select message"` once in `ComposeMode::Edit`) hints how to cancel (`Esc` or `Ctrl-C`).
  2. In `clear_draft()` (`src/app.rs:2570-2580`), `cancel_compose()` only runs inside `if !self.composer.is_empty()`. If a user enters `Edit` via `↑`, deletes all message text (`Backspace`/`Ctrl-U` to empty), and presses `Ctrl-C` to cancel, `clear_draft()` returns `false`, shows `"Press Ctrl-Q to quit"`, and leaves `ComposeMode::Edit` active. In the `vim` preset (where `Esc` only toggles panes), the user is stuck in edit mode unless they type a character before `Ctrl-C`, click `✕`, or run `"Cancel edit"` from the command palette. (Allowing `clear_draft()` to call `cancel_compose()` whenever `self.compose_mode != ComposeMode::Normal` resolves this.)

---

## Checks run on `drafts-edit` (`ca3b85f`)

- `cargo fmt --check` — pass
- `cargo clippy --all-targets -- -D warnings` — pass
- `cargo test` — first parallel run had 1 transient keyring timeout (`matrix::keyring::tests::dbus_secret_service_roundtrip_over_private_bus`: `Unavailable("timed out waiting for system keyring")`); standalone rerun passed in `0.09s`, and full rerun passed cleanly: **`467 passed; 0 failed; 19 ignored`**
- `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=alicepass cargo test -- --ignored --test-threads=1 draft_survives_a_restart_and_can_be_cleared --nocapture` (against local continuwuity on `127.0.0.1:6167`) — **`1 passed; 0 failed`** (`4.93s`)
