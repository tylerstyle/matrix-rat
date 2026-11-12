# Milestone M2 Review (`6df11eb`)

**Reviewer:** `matrix-agy`  
**Target:** Commit `6df11ebbbca2a47da6994b5a55d1e426be22bcdf` (`M2: login, sync and the real room list; emoji picker`)

---

## Verification Summary

- `./scripts/check.sh` passes cleanly (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, 64 unit + snapshot tests).
- End-to-end integration test (`MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=alicepass cargo test password_login -- --ignored`) passes in 1.80s against the local `continuwuity` homeserver (`http://127.0.0.1:6167`), verifying password login, `session.json` mode `0600`, `SyncService` room population, and session restore across client restarts.
- Audited `src/matrix/{mod,session}.rs`, `src/modal.rs`, `src/ui/modal.rs`, `src/emoji.rs`, `src/app.rs`, `src/model.rs`, `src/ui/{composer,status}.rs`, and the snapshots in `src/ui/snapshots/` against `matrix-sdk 0.19.1`, `matrix-sdk-ui 0.19.1`, and `ratatui-core 0.1.2` in `~/.cargo/registry`.
- Confirmed that `RoomListService::new` in `matrix-sdk-ui 0.19.1` (`src/room_list_service/mod.rs:272`) automatically calls `client.event_cache().subscribe()?`, satisfying the `EventCache` subscription requirement in `docs/PLAN.md` §4.

---

## Findings (Ordered by Severity)

### 1. [HIGH] Offline startup with a saved session fails and drops the user onto the Login form (risking local store wipe)
**Location:** `src/matrix/session.rs:50-58, 110-128`, `src/matrix/mod.rs:150-169`

- **Problem:**
  1. `session::save` persists `client.homeserver().to_string()`—the already-resolved homeserver URL—into `session.json`.
  2. However, `session::restore` calls `build_client(&paths, &saved.homeserver, &saved.passphrase)`, which configures `Client::builder()` with `.server_name_or_homeserver_url(&saved.homeserver)` instead of `.homeserver_url(&saved.homeserver)`.
  3. In `matrix-sdk 0.19.1` (`src/client/builder/mod.rs:268-288, 699-701`), `ServerNameOrHomeserverUrl` performs live HTTP requests (`/.well-known/matrix/client` and `/_matrix/client/versions`) during `ClientBuilder::build().await`. By contrast, `.homeserver_url(...)` with the default `SlidingSyncVersionBuilder::Native` makes **zero network requests** during `build().await`, allowing `client.restore_session(session).await` to succeed purely from the local SQLite store when offline (`docs/PLAN.md` §9).
  4. Worse, in `Worker::run` (`src/matrix/mod.rs:150-169`), any `Err` from `session::restore` is treated identically to `Ok(None)`: the worker logs a warning, emits `MatrixEvent::NeedLogin`, and enters `wait_for_login()`. If the user submits the login form after a transient startup failure, `login_password` / `login_browser` calls `fresh_store()` -> `forget()`, **deleting the existing SQLite store and E2EE cross-signing/device keys**.

- **Suggested Fix:**
  - Split client construction into `build_client_for_discovery` (using `.server_name_or_homeserver_url(...)` for initial login) and `build_client_restored` (using `.homeserver_url(&saved.homeserver)` for `session::restore`).
  - With `.homeserver_url(...)`, `session::restore` succeeds completely offline, immediately populates cached rooms from SQLite (`read_all_rooms(&client).await`), and lets `SyncService` manage offline/online state transitions.
  - Only fall back to `MatrixEvent::NeedLogin` when `session.json` does not exist (`Ok(None)`) or when `SessionEnd::Expired` explicitly revokes the session.

---

### 2. [HIGH] Login cancellation, shutdown hang during browser login, and worker error-recovery deadlock
**Location:** `src/matrix/session.rs:155-214`, `src/matrix/mod.rs:176-218`, `src/app.rs:593-601, 1161-1165`

- **Problem:**
  1. **Shutdown during browser login hangs for the full 3s timeout (`src/matrix/session.rs:198-203`, `src/matrix/mod.rs:210-216`):**
     Inside `session::login_browser`, receiving `Some(MatrixCmd::Shutdown)` or `None` returns `Err(anyhow!("login cancelled"))`. `Worker::wait_for_login` catches that `Err`, sends `MatrixEvent::LoginFailed("login cancelled")`, and loops back to `self.commands.recv().await?` instead of terminating. Furthermore, in `app::run` (`src/app.rs:1161-1165`), `app` is still alive during the 3-second shutdown drain and holds a clone of `MatrixHandle` (`UnboundedSender<MatrixCmd>`), so `self.commands.recv()` never sees `None` until the 3-second timeout expires.
  2. **`CancelLogin` is ignored during `login_password` and the discovery/registration phase of `login_browser` (`src/matrix/mod.rs:196-209`, `src/app.rs:596-600`):**
     Pressing `Esc` in the login modal (`App::close_modal`) immediately clears `form.busy = None` in the UI and sends `MatrixCmd::CancelLogin`. However, `login_password` (and the first three `.await` calls of `login_browser`: `build_client`, `server_metadata`, `oauth.login().build()`) does not select on `self.commands.recv()`. If the user hits `Esc` on a slow/typoed homeserver and submits a new login attempt, the old login future is still running, consumes the `CancelLogin` command (or leaves it queued after `login_password` finishes), and then overwrites the new attempt or logs into the wrong server.
  3. **Worker error recovery blocks forever (`src/matrix/mod.rs:176-185`):**
     When `run_session` returns `Err(err)`, `Worker::run` emits `SyncState::Error` and then awaits `self.commands.recv().await` before retrying. Because the main UI in M2 sends no `MatrixCmd` outside of modals, a fatal error in `run_session` permanently stalls the worker until the user quits.

- **Suggested Fix:**
  - Run the entire `session::login_password(...)` or `session::login_browser(...)` future inside a `tokio::select!` in `Worker::wait_for_login` against `self.commands.recv()`:
    - On `Some(MatrixCmd::Shutdown) | None` -> return `None` immediately so the worker exits cleanly.
    - On `Some(MatrixCmd::CancelLogin)` -> drop the in-flight login future (which automatically drops the temporary `Client` and unbinds the `LocalServerBuilder` redirect listener) and `continue` waiting for the next login command without emitting a spurious `LoginFailed`.
  - In `app::run` (`src/app.rs:1161`), `drop(app);` (or drop its `MatrixHandle`) before entering the shutdown drain loop so the command channel closes as soon as `Shutdown` is sent.
  - In `Worker::run`'s `Err(err)` branch (`src/matrix/mod.rs:176-185`), `tokio::select!` between `self.commands.recv()` (returning early on `Shutdown | None`) and `tokio::time::sleep(Duration::from_secs(5))` before retrying `run_session`.

---

### 3. [MEDIUM] Emoji picker selection highlight is partially wiped by `Span::render`
**Location:** `src/ui/modal.rs:373-386`

- **Problem:**
  In `ui::modal::emoji_picker`:
  ```rust
  if idx == picker.selected {
      frame.buffer_mut().set_style(cell, theme.style(Role::Pill));
  } else if hovered {
      frame.buffer_mut().set_style(cell, theme.style(Role::Hover));
  }
  frame.render_widget(Line::from(emoji.as_str()), cell);
  ```
  In `ratatui-core 0.1.2` (`src/text/span.rs:464-468`), rendering a 2-column glyph at `cell.x` executes:
  ```rust
  for x_hidden in (x + 1)..next_x {
      buf[(x_hidden, y)].reset();
  }
  ```
  Because `buf[(x_hidden, y)].reset()` clears the second cell's style back to `Style::default()`, calling `set_style(cell, ...)` *before* `render_widget(Line::from(...))` loses the background highlight on column `cell.x + 1` of the 2-column emoji!

- **Suggested Fix:**
  Pass the cell style on the `Line` / `Span` itself (or call `frame.buffer_mut().set_style(cell, style)` *after* `frame.render_widget(Line::from(emoji.as_str()), cell)`):
  ```rust
  frame.render_widget(Line::from(emoji.as_str()), cell);
  if idx == picker.selected {
      frame.buffer_mut().set_style(cell, theme.style(Role::Pill));
  } else if hovered {
      frame.buffer_mut().set_style(cell, theme.style(Role::Hover));
  }
  ```

---

### 4. [MEDIUM] Stale app state across `NeedLogin`, open-room removal, and concurrent modals
**Location:** `src/app.rs:696-728, 751-801`, `src/ui/mod.rs:70-73`

- **Problem:**
  1. **`NeedLogin` leaves stale session state (`src/app.rs:697-701`):**
     When `MatrixEvent::NeedLogin` arrives (for instance after `SessionEnd::Expired`), `self.rooms`, `self.composer`, `self.compose_mode`, `self.user_id`, and `self.recovery_prompted` are not reset. If the user logs in again during the same process lifetime, `self.recovery_prompted` is still `true`, so `MatrixEvent::Recovery(RecoveryStatus::NeedsKey)` will never prompt for the recovery key.
  2. **Draft/composer desync when the active room disappears (`src/app.rs:787-795`):**
     In `App::apply_rooms`, if the previously open room is removed from the room list (`open_id` is not found in `self.rooms`), `self.active` falls back to `0`, `self.scroll` resets to `Bottom`, and `self.selected_msg` is cleared—but `self.composer` and `self.compose_mode` still hold the vanished room's text/edit state instead of loading `self.rooms[0].draft`.
  3. **Recovery prompt lost if Emoji picker is open (`src/app.rs:720-728`):**
     If `MatrixEvent::Recovery(RecoveryStatus::NeedsKey)` arrives while `self.modal` is `Some(Modal::Emoji(_))`, `self.modal.is_none()` is false, so the recovery modal is dropped on the floor and never shown after the emoji picker closes.
  4. **Help overlay and modal can render simultaneously (`src/ui/mod.rs:70-73`):**
     If `self.help_open` is `true` when an async `MatrixEvent::NeedLogin` or `MatrixEvent::Recovery(NeedsKey)` sets `self.modal = Some(...)`, both `help::draw` and `modal::draw` render on top of each other.

- **Suggested Fix:**
  - On `MatrixEvent::NeedLogin`, clear `self.rooms`, `self.composer.clear()`, `self.compose_mode = ComposeMode::Normal`, `self.user_id = None`, `self.recovery_prompted = false`, and `self.help_open = false`.
  - In `App::apply_rooms`, when `open_id` is no longer present in `self.rooms`, reset `self.compose_mode = ComposeMode::Normal` and replace `self.composer` with `self.rooms.get(self.active).map(|r| r.draft.clone()).unwrap_or_default()`.
  - Only set `self.recovery_prompted = true` when the `Modal::Recovery` is actually opened (or let `Recovery` replace `Modal::Emoji`), and clear `self.help_open = false` whenever setting `self.modal = Some(...)`.

---

### 5. [LOW] Secrets hygiene in `Debug` impls & `RecoveryForm`, plus small modal UX gaps
**Location:** `src/matrix/mod.rs:23-39`, `src/modal.rs:7-19, 53-58`, `src/ui/modal.rs:222-276`, `src/app.rs:580-591, 833-850`

- **Problem:**
  1. `MatrixCmd`, `LoginForm`, and `RecoveryForm` derive `Debug` with cleartext `password` and recovery `key` fields. In addition, `App::submit_recovery` (`src/app.rs:582`) uses `form.key.trim().to_string()` instead of `std::mem::take(&mut form.key)` (unlike `submit_login`, which takes `form.password`), leaving the recovery key in `RecoveryForm` while unlocking.
  2. When a modal or help overlay is open (`src/app.rs:833-850`), `on_key` intercepts all keys before the normal keymap, which blocks `Ctrl-Z` (`Action::Suspend`). Being able to `Ctrl-Z` while on the Login or Recovery modal is common in TUIs when grabbing a password or recovery key from a CLI password manager (`pass`, `rbw`, `1password-cli`).
  3. Modal text inputs (`LoginForm`, `RecoveryForm`, `EmojiPicker`) support `Backspace` and `Ctrl-W`, but not `Ctrl-U` (clear line), and `ui::modal::recovery` renders `[Enter] unlock   [Esc] later` as static hint text rather than clickable `HitId::Do(Action::ModalSubmit)` / `HitId::Do(Action::Cancel)` buttons like `ui::modal::login_form`.

- **Suggested Fix:**
  - Implement custom `fmt::Debug` (redacting `password` and `key`) on `MatrixCmd`, `LoginForm`, and `RecoveryForm`, and `std::mem::take(&mut form.key)` in `submit_recovery`.
  - Allow `Ctrl-Z` to set `self.suspend_requested = true` even when `help_open` or `modal.is_some()`, support `Ctrl-U` to clear the focused modal input field, and register `HitId::Do(Action::ModalSubmit)` / `HitId::Do(Action::Cancel)` on the recovery modal action hints.

---

## Verdict

**Approve with changes.**

M2 is a clean, well-tested milestone: the worker channel boundary, session persistence (`0600`), token-refresh observer, room-list subscription, login/recovery/emoji modals, and local `continuwuity` end-to-end test all fit together cleanly. Addressing Findings #1 and #2 (offline session restore via `.homeserver_url(...)` and login cancellation/shutdown handling) and the smaller UI/state fixes in #3–#5 will make the session and worker lifecycle solid before moving on to M3 (virtualized timeline and sending).
