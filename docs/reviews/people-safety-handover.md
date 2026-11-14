# People and Safety (branch `agy/people-safety`)

- **Worktree:** `/home/tylerstyle/git/matrix-rat/.claude/worktrees/agy-people-safety`
- **Branch:** `agy/people-safety`
- **Base commit:** `49e1ecd` (`merge-base` with `main`; note `main` advanced to `cf6ea67` while this work was in flight)

## Architecture & Behaviour

### 1. Ignore / Block Users (`m.ignored_user_list`)

- **Worker & ignore state ([src/matrix/safety.rs](src/matrix/safety.rs), [src/matrix/mod.rs](src/matrix/mod.rs)):**
  - `IgnoreTracker::start` syncs `m.ignored_user_list` account data on startup and live updates, publishing `IgnoredSet` (`HashSet<OwnedUserId>`) over a `tokio::sync::watch::Receiver<IgnoredSet>` and emitting sorted `MatrixEvent::IgnoredUsers(Vec<String>)`.
  - `IgnoreTracker` implements `Drop` (and `stop()`) to abort its background listener task on session end or early errors in `run_session`.
  - `handle_ignore_op` updates `m.ignored_user_list` via `client.account()` and emits updated `MatrixEvent::IgnoredUsers` and `MatrixEvent::IgnoreResult { user_id, ignored, result }`.
- **Filtering across timelines, previews, notifications, and global search:**
  - **Timelines ([src/matrix/convert.rs](src/matrix/convert.rs), [src/matrix/rooms.rs](src/matrix/rooms.rs)):** `convert(items, own, ignored)` drops non-own events sent by ignored users, removes ignored senders from reaction counts (omitting reactions that reach zero), replaces `reply_to` details for ignored parents with `"message from ignored user"`, strips any trailing `MsgKind::ReadMarker` left exposed by filtered messages, and populates `Message.sender_id`. `Timelines::forward()` subscribes to `ignored.changed()` so open/cached/detached timelines re-convert immediately when the ignore list changes. Also fixes `forward()`'s `saw_clear` pagination task to live in a tracked `clear_paginators: JoinSet` gated on `RoomState::Joined` so leaving a room or shutting down the worker (`timelines.close()`) never leaks an `events` channel clone.
  - **Empty-reset recovery ([src/app.rs](src/app.rs)):** On `MatrixEvent::Timeline`, when `messages.is_empty() && !reached_start`, `App` resets `self.timeline.reached_start = false` and `self.timeline.empty_batches = 0` so ignoring every loaded message in the window still lets backward pagination load earlier messages.
  - **Notifications ([src/matrix/notify.rs](src/matrix/notify.rs)):** `watch()` / `notice()` skips desktop notifications when `is_ignored_in(ignored, sender)` matches either `RawAnySyncOrStrippedTimelineEvent::Sync` message events or `Stripped` room invites (via `room.invite_details().await` with raw JSON `"sender"` fallback).
  - **Global search ([src/matrix/search.rs](src/matrix/search.rs)):** `Searcher::new` receives `ignored: watch::Receiver<IgnoredSet>`; `resolve_hit()` drops hits if `is_ignored_in` matches either the raw event sender or the unwrapped target event sender after edit resolution.
- **UI entry points ([src/app/menu.rs](src/app/menu.rs), [src/menu.rs](src/menu.rs), [src/manage.rs](src/manage.rs), [src/ui/roominfo.rs](src/ui/roominfo.rs), [src/modal.rs](src/modal.rs), [src/ui/modal.rs](src/ui/modal.rs), [src/app/safety.rs](src/app/safety.rs), [src/palette.rs](src/palette.rs), [src/app/palette.rs](src/app/palette.rs)):**
  - **Message context menu:** Adds `"Ignore user"` (`"Again to ignore"`) / `"Stop ignoring"` (`"Again to stop ignoring"`) via `Run::IgnoreUser` / `Run::UnignoreUser`.
  - **Room Info members:** Adds `Click::MemberIgnore`, `Alt-I`, `Confirm::Ignore` / `Confirm::Unignore` (with custom redacted `Debug`), `"ignored"` member tag in `Role::Error`, and `" ignore "` / `" unignore "` row buttons.
  - **Settings -> Ignored users (`Setting::IgnoredUsers`):** Opens `Modal::IgnoredUsers(IgnoredUsersPopup)` with live `Filter` `LineEdit`, selection (`Up`/`Down`/`PageUp`/`PageDown`), `Enter` / `Del` / `" unignore "` button (`HitId::IgnoredUnignore`), and live sync updates (`sync_ignored_modal()`). `reset_session()` clears `ignored_users` and closes `Modal::IgnoredUsers` / `Modal::Report`.

### 2. Report Content (`MatrixCmd::ReportContent`)

- Message context menu adds `"Report…"` (`Run::Report`), handled by `App::open_report_modal` (blocks unsent local items without `ItemRef::Event` and own messages).
- `Modal::Report(ReportForm)` ([src/modal.rs](src/modal.rs), [src/ui/modal.rs](src/ui/modal.rs), [src/app/safety.rs](src/app/safety.rs)) provides a bounded reason input (`ReportForm::REASON_MAX_BYTES = 512`), an `"Also ignore this user"` toggle (`HitId::ReportIgnoreToggle`) when `sender_id` is known and not yet ignored, `" Send report "` (`HitId::ReportSubmit`), and keyboard navigation (`Tab`/`BackTab`/`Up`/`Down`/`Space`/`Enter`/`Esc`). `ReportForm` implements a custom redacted `Debug` that omits `event_id`, `sender_id`, `sender_name`, and `reason`.
- `matrix::safety::report_content` sends `MatrixCmd::ReportContent` and emits `MatrixEvent::ReportResult`, surfacing feedback toasts / modal errors via `on_report_result`.

### 3. Find People by Name (`NewPopup` `Stage::Direct` / `Stage::Invite`)

- **Debounced lookup & selection ([src/app/safety.rs](src/app/safety.rs), [src/app/rooms.rs](src/app/rooms.rs), [src/manage.rs](src/manage.rs), [src/app.rs](src/app.rs)):**
  - Typing in `Stage::Direct` or `Stage::Invite` schedules `USER_SEARCH_DEBOUNCE` (250 ms) in `app::run` via `user_search_debounce_deadline()` before sending generation-tagged `MatrixCmd::SearchUsers`.
  - Typing an explicit `@name:server` and pressing `Enter` without choosing a dropdown item submits immediately (`submit_new`).
  - `Up`/`Down` moves selection (`step_user_search`); `Enter` or clicking `HitId::UserSearchRow` fills `popup.user` and calls `cancel_user_search()` (`MatrixCmd::CancelUserSearch`).
  - `on_user_search_results` discards stale generations and filters out the signed-in user and anyone in `ignored_users`.
- **Dropdown UI & worker ([src/ui/roominfo.rs](src/ui/roominfo.rs), [src/matrix/safety.rs](src/matrix/safety.rs)):**
  - `one_field` + `user_search_rows` dynamically expands `NewPopup` height for up to `USER_SEARCH_MAX_ROWS` (5) rows showing `"  Searching…"` while pending or avatar tile/image (`draw_avatar` after `safe_mxc` validates `mxc://` URIs), sanitized display name, and user ID.
  - `UserSearcher` cancels superseded/stopped searches (`stop()` / `Drop`) and degrades silently on unavailable or empty directory responses.

## Files Changed

- **New modules, UI snapshots, & handover (6):**
  - `src/app/safety.rs`
  - `src/matrix/safety.rs`
  - `src/app/snapshots/matrix_rat__app__safety__tests__ignored_users_popup.snap`
  - `src/app/snapshots/matrix_rat__app__safety__tests__report_form_modal.snap`
  - `src/app/snapshots/matrix_rat__app__safety__tests__user_search_dropdown.snap`
  - `docs/reviews/people-safety-handover.md`
- **Modified tracked files (24):**
  - `README.md`
  - `src/app.rs`, `src/app/menu.rs`, `src/app/palette.rs`, `src/app/rooms.rs`
  - `src/manage.rs`, `src/menu.rs`, `src/modal.rs`, `src/model.rs`, `src/palette.rs`
  - `src/matrix/convert.rs`, `src/matrix/mod.rs`, `src/matrix/notify.rs`, `src/matrix/rooms.rs`, `src/matrix/search.rs`
  - `src/ui/hit.rs`, `src/ui/modal.rs`, `src/ui/roominfo.rs`
  - `src/app/snapshots/matrix_rat__app__rooms__tests__room_info_filtered.snap`, `src/app/snapshots/matrix_rat__app__rooms__tests__room_info_small.snap`
  - `src/ui/snapshots/matrix_rat__ui__tests__command_palette.snap`, `src/ui/snapshots/matrix_rat__ui__tests__command_palette_filtered.snap`, `src/ui/snapshots/matrix_rat__ui__tests__command_palette_narrow.snap`, `src/ui/snapshots/matrix_rat__ui__tests__settings_menu.snap`

## Verification

- `./scripts/check.sh` (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build`) passed with exit code `0`:
  - `cargo test`: `test result: ok. 511 passed; 0 failed; 22 ignored; 0 measured; 0 filtered out; finished in 9.26s`
  - `find src -name '*.snap.new'`: empty (all snapshots accepted and reviewed)
- `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=alicepass cargo test matrix:: -- --ignored --test-threads=1` against local Continuwuity (`http://127.0.0.1:6167`):
  - `test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 512 filtered out; finished in 95.89s`
  - Includes `matrix::safety::tests::ignore_syncs_across_clients_hides_and_restores_timeline_search_and_report ... ok` (uses a public visibility test room because Continuwuity's `/_matrix/client/v3/user_directory/search` does not index users sharing only private invite rooms).
  - Note: an earlier serialized run hit a transient 10s timeout waiting on the plain SDK requester in `matrix::verify::tests::other_people_verify_in_a_direct_chat` (`src/matrix/verify.rs:1913`), after which the direct re-run (`16.27s`) and the full 21-test serialized re-run (`95.89s`) both passed cleanly.
