# People and Safety (branch `agy/people-safety`)

- **Worktree:** `/home/tylerstyle/git/matrix-rat/.claude/worktrees/agy-people-safety`
- **Branch:** `agy/people-safety`
- **Merged `main`:** `cf6ea67` (v0.2.0, including unread handling and clickable notifications) into `agy/people-safety` (`ca30abd`)

## Architecture & Behaviour

### 1. Ignore / Block Users (`m.ignored_user_list`)

- **Worker & ignore state ([src/matrix/safety.rs](src/matrix/safety.rs), [src/matrix/mod.rs](src/matrix/mod.rs)):**
  - `IgnoreTracker::start` syncs `m.ignored_user_list` account data on startup and live updates, publishing `IgnoredSet` (`HashSet<OwnedUserId>`) over a `tokio::sync::watch::Receiver<IgnoredSet>` and emitting sorted `MatrixEvent::IgnoredUsers(Vec<String>)`.
  - `IgnoreTracker` implements `Drop` (and `stop()`) to abort its background listener task on session end or early errors in `run_session`.
  - `handle_ignore_op` updates `m.ignored_user_list` via `client.account()` and emits updated `MatrixEvent::IgnoredUsers` and `MatrixEvent::IgnoreResult { user_id, ignored, result }`.
- **Filtering across timelines, previews, notifications, and global search:**
  - **Timelines ([src/matrix/convert.rs](src/matrix/convert.rs), [src/matrix/rooms.rs](src/matrix/rooms.rs)):** `convert(items, own, ignored)` drops non-own events sent by ignored users, removes ignored senders from reaction counts (omitting reactions that reach zero), replaces `reply_to` details for ignored parents with `"message from ignored user"`, preserves a trailing `VirtualTimelineItem::ReadMarker` when every item after it was hidden/ignored (so the UI knows no visible unread messages follow), and populates `Message.sender_id`. `Timelines::forward()` subscribes to `ignored.changed()` so open/cached/detached timelines re-convert immediately when the ignore list changes, filters typing indicator names against `ignored`, and tracks `saw_clear` pagination tasks in `clear_paginators` gated on `RoomState::Joined`.
  - **Empty-reset recovery ([src/app.rs](src/app.rs)):** On `MatrixEvent::Timeline`, when `messages.is_empty() && !reached_start`, `App` resets `self.timeline.reached_start` and `self.timeline.empty_batches` so ignoring every loaded message in the window still lets backward pagination load earlier messages.
  - **Notifications ([src/matrix/notify.rs](src/matrix/notify.rs), [src/notify.rs](src/notify.rs)):** `watch()` / `notice()` classifies each candidate as `NoticeOutcome::{Show, Suppressed, Ignore}`, emitting `MatrixEvent::SuppressedNotice { room_id }` instead of `MatrixEvent::Notice` when `is_ignored_in(ignored, sender)` matches a `RawAnySyncOrStrippedTimelineEvent::Sync` message or `Stripped` invite (via `room.invite_details().await` with raw JSON `"sender"` fallback). Desktop notification click routing (`Clickable` -> `MatrixEvent::NoticeClicked` -> `App::on_notice_click`) stays intact.
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

### 4. Interactions with `main` (v0.2.0 Unread Handling, Notifications & Jump)

1. **"New" line vs ignored messages ([src/matrix/convert.rs](src/matrix/convert.rs), [src/app/unread.rs](src/app/unread.rs), [src/app/safety.rs](src/app/safety.rs), [src/ui/timeline.rs](src/ui/timeline.rs)):**
   - `convert()` preserves a trailing `VirtualTimelineItem::ReadMarker` (`index < last`), and `App::place_unread_line` distinguishes `Marker::Missing`, `Marker::Before(id)`, and `Marker::AtEnd`. When `Marker::AtEnd` settles, the line turns `Line::Off` (never sitting above nothing or already-read messages) and sets `Unread::pending_mark` so the trailing ignored events still get marked read.
   - `countable()` excludes ignored senders and `Line::At(Vec<UnreadTarget>)` stores ordered candidate targets so live ignore changes (`on_ignored_users_list` / `on_ignore_result` -> `refresh_unread_line_for_ignored` -> `prune_ignored_targets` / `with_unread_line`) step past newly ignored messages to the next non-ignored unread message or clear cleanly to `Line::Off`.
   - `ui::timeline::draw` marks `app.timeline.drawn = true` once an empty timeline finishes loading (`ready`), letting unread settling and read-marker sends finish even when all messages are ignored.
   - Unit tests: `ignored_unread_messages_place_the_line_accurately`, `ignoring_a_sender_while_the_room_is_open_advances_or_clears_the_unread_line`, `ignored_only_unread_updates_in_open_and_listed_rooms_mark_read`.
2. **Unread counts & "mark as read" ([src/matrix/mod.rs](src/matrix/mod.rs), [src/matrix/rooms.rs](src/matrix/rooms.rs), [src/app.rs](src/app.rs), [src/app/unread.rs](src/app/unread.rs)):**
   - **Design choice (documented in `matrix::summarize`):** Server push-rule notification/unread counts remain faithful to `/sync` (ignored senders still count until marked read), because reliably subtracting ignored events would require fetching unread history per room and could otherwise mask real unread messages before an ignored latest event.
   - Opening an unread room with no visible messages sets `Unread::pending_mark` (`has_pending_unread_mark()`) and `App::maybe_mark_read` compares `TimelineState::hidden_read_state` so server unread/bold state clears on open and re-clears if another ignored message arrives while open, without spamming `MarkRead` every frame.
   - `mark_room_read` falls back to `fallback_latest_event_id(room)` (`room.latest_event().event_id()` then `room.messages(MessagesOptions::backward())`) when `timeline.latest_event_id()` is `None` (e.g. Continuwuity omitting ignored events from timeline items), so `MarkRead` / `MarkRoomsRead` still clears server unread counts. Verified on Continuwuity in `matrix::safety::tests::ignore_syncs_across_clients_hides_and_restores_timeline_search_and_report`.
3. **Notification click & ignore filter ([src/matrix/notify.rs](src/matrix/notify.rs), [src/app.rs](src/app.rs), [src/app/safety.rs](src/app/safety.rs)):**
   - When an ignored sender triggers a room notification, the worker emits `MatrixEvent::SuppressedNotice { room_id }`; `App` records `detailed_notices` and drains matching `count_notices` so `release_count_notices` does not emit a fallback `"New message"` desktop notification. Non-ignored desktop clicks (`MatrixEvent::NoticeClicked`) continue opening the room normally.
   - Unit test: `suppressed_notice_prevents_count_fallback_notification_while_click_still_opens_room`.
4. **Jump / reply jump to an ignored message ([src/matrix/rooms.rs](src/matrix/rooms.rs), [src/matrix/manage.rs](src/matrix/manage.rs), [src/app/safety.rs](src/app/safety.rs)):**
   - Both the warm-cache check and the `/context` worker task in `Timelines::open_event` reject events sent by an ignored user with `"That message is from an ignored user."` (`OpenedTimeline::Failed` -> `MatrixEvent::JumpFailed`), and `friendly_jump_message` translates Continuwuity's `MSC4406.SENDER_IGNORED` (`You have ignored this sender`) to the same message.
   - `open_event` pre-checks `room.event(&parsed_event, None).await` before creating a `TimelineFocus::Event` cache, and `matrix::manage::leave` calls `client.event_cache().forget_room(room.room_id()).await` before `room.forget().await`. Together these avoid an upstream `matrix-sdk-0.19.1` `EventCache` hazard where a failed `/context` entry + later `room.forget()` leaves a broken `event_focused` room entry that aborts `EventCache::clear_all_rooms()` on subsequent ignore/unignore transitions.
   - Unit test: `jump_to_ignored_message_shows_clear_toast_and_keeps_current_view`; server test exercises both warm-cache and `/context` `OpenEvent` rejection.
5. **Search hits & drafts:**
   - Both existing flows remain covered and green alongside the merge (`matrix::search::tests::global_search_spike_encrypted_edits_redactions_and_restart` and `matrix::drafts::tests::draft_survives_a_restart_and_can_be_cleared`, plus all `app::global_search::tests::*` and `app::drafts::tests::*` unit tests).

## Files Changed (vs `main` `cf6ea67`)

- **New modules, UI snapshots, & handover (6):**
  - `docs/reviews/people-safety-handover.md`
  - `src/app/safety.rs`
  - `src/matrix/safety.rs`
  - `src/app/snapshots/matrix_rat__app__safety__tests__ignored_users_popup.snap`
  - `src/app/snapshots/matrix_rat__app__safety__tests__report_form_modal.snap`
  - `src/app/snapshots/matrix_rat__app__safety__tests__user_search_dropdown.snap`
- **Modified tracked files (27):**
  - `README.md`
  - `src/app.rs`, `src/app/menu.rs`, `src/app/palette.rs`, `src/app/rooms.rs`, `src/app/unread.rs`
  - `src/manage.rs`, `src/menu.rs`, `src/modal.rs`, `src/model.rs`, `src/palette.rs`
  - `src/matrix/convert.rs`, `src/matrix/manage.rs`, `src/matrix/mod.rs`, `src/matrix/notify.rs`, `src/matrix/rooms.rs`, `src/matrix/search.rs`
  - `src/ui/hit.rs`, `src/ui/modal.rs`, `src/ui/roominfo.rs`, `src/ui/timeline.rs`
  - `src/app/snapshots/matrix_rat__app__rooms__tests__room_info_filtered.snap`, `src/app/snapshots/matrix_rat__app__rooms__tests__room_info_small.snap`
  - `src/ui/snapshots/matrix_rat__ui__tests__command_palette.snap`, `src/ui/snapshots/matrix_rat__ui__tests__command_palette_filtered.snap`, `src/ui/snapshots/matrix_rat__ui__tests__command_palette_narrow.snap`, `src/ui/snapshots/matrix_rat__ui__tests__settings_menu.snap`

## Verification

- `./scripts/check.sh` (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build`) passed with exit code `0`:
  - `cargo test`: `test result: ok. 539 passed; 0 failed; 24 ignored; 0 measured; 0 filtered out; finished in 8.51s`
  - `find src -name '*.snap.new'`: empty (all snapshots accepted and reviewed)
- `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=alicepass cargo test --locked matrix:: -- --ignored --test-threads=1` against local Continuwuity (`http://127.0.0.1:6167`):
  - `test result: ok. 23 passed; 0 failed; 0 ignored; 0 measured; 540 filtered out; finished in 103.12s`
  - Covers `matrix::safety::tests::ignore_syncs_across_clients_hides_and_restores_timeline_search_and_report` (using a public visibility room for directory indexing and a private `room2` created and joined before `IgnoreUser`, because Continuwuity MSC4406 blocks inviting/joining ignored users and omits ignored events from `/sync`, `/event`, and `/context`; wrapped in a cleanup guard so `@bob:localhost` is unignored on exit), plus all 22 existing server tests including `matrix::rooms::tests::{the_read_marker_stands_before_what_was_not_read, a_room_marked_unread_shows_in_the_users_other_clients, alice_jumps_to_unloaded_message_across_more_than_60_events}`, `matrix::search`, `matrix::drafts`, and `matrix::notify`.
