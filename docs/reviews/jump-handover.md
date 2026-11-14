# Jump to Unloaded Message Handover (branch `agy/jump`)

- **Worktree:** `/home/tylerstyle/git/matrix-rat/.claude/worktrees/agy-jump`
- **Base commit (`f500e60`):** `next` (`main` + `drafts-edit` merged)

## Design & Architecture

### 1. Unified Jump Mechanism (`jump_to_event`)
One entry point `App::jump_to_event(room_id, event_id, thread_root)` (`src/app/global_search.rs`) serves every caller:
- **Fast path (loaded in active view):** When the target room (and thread, if any) is already open and the message is present in `self.messages()` (or when `thread_root` is `None` and the event is already in the currently open thread), any in-flight worker jump is cancelled (`cancel_pending_jump()`) and the message is immediately selected and revealed without touching the worker.
- **Worker path (`MatrixCmd::OpenEvent`):** Allocates a monotonic `generation` via `App::allocate_gen()`, records `PendingJump { generation, room_id, event_id, thread_root }` (with redacted `Debug`), and keeps the active view untouched until the worker responds (or a user interaction cancels the pending jump via `MatrixCmd::CancelOpenEvent { generation }`).
- **Warm-cache vs. `/context` (`src/matrix/rooms.rs`):**
  - `Timelines::open_event` first inspects the room's warm live timeline (when `thread_root` is `None`) or thread timeline; if the event is already cached in memory, it switches straight to that live timeline (`detached: None`) at zero network cost.
  - Otherwise it spawns a background task (keeping the worker loop unblocked) using `room.timeline_builder().with_focus(TimelineFocus::Event { target, num_context_events: PAGE / 2, thread_mode })` with `PAGE = 40`, `TimelineEventFocusThreadMode::ForceThread` for thread jumps and `TimelineEventFocusThreadMode::Automatic { hide_threaded_events: true }` otherwise.
  - Note: in `matrix-sdk-ui 0.19.1`, `TimelineFocus::Event` with `ForceThread` vs. `Automatic` covers both thread replies and room messages cleanly. If the `/context` request fails (`404` / `M_NOT_FOUND`, `403` / `M_FORBIDDEN`, redacted/missing target, etc.), `OpenedTimeline::Failed` emits `MatrixEvent::JumpFailed { generation, reason }` via `friendly_jump_error`, showing a toast while leaving the active room, scroll position, selection, and composer untouched.

### 2. Detached Slice, Bidirectional Pagination & Returning to Live
- **State (`TimelineState` / `DetachedTimeline`):** `DetachedTimeline { target_event, thread_root, reached_end }` arrives on `MatrixEvent::Timeline` when `OpenEvent` opened a `/context` slice (`DetachedSlot` inside `Timelines`, leaving live `rooms`/`threads` cache entries separate).
- **UI indicator (`src/ui/timeline.rs`):** Renders a sticky 1-row `Role::Banner` across the top of the timeline pane (`" viewing older messages · {key} / click to return to now"`, falling back to `" viewing older messages · click to return to now"` if `JumpToNewest` is unbound), bound to `HitId::Button(Button::JumpToNewest)`.
- **Forward pagination & scroll anchoring (`src/app.rs`, `src/matrix/rooms.rs`, `src/ui/timeline.rs`):**
  - Scrolling down to `scroll == 0` while `detached && !reached_end` sets `want_newer`, triggers `MatrixCmd::PaginateForwards { room_id, generation }` (wrapped in `MatrixCmd::InThread` when viewing a detached thread), displays `" loading newer messages… "`, and updates `reached_end` on `MatrixEvent::PaginatedForwards`.
  - Scroll `Anchor` tracking in `src/ui/timeline.rs` runs whenever `scroll > 0 || app.timeline.detached`, keeping the bottom-most visible message pinned when newer messages append below instead of jumping past them.
- **Read receipts & live unreads (`src/app.rs`):** `maybe_mark_read` skips sending `MarkRead` while `self.timeline.detached` is true, and live `MatrixEvent::Rooms` unread counts continue updating in the sidebar independently.
- **Returning to live (`App::return_to_live`):** Triggered by `Action::JumpToNewest` (`End`), clicking the detached banner (`HitId::Button(Button::JumpToNewest)`), or sending a message (`send()` in `src/app.rs` and `send_attachments()` in `src/app/attach.rs`). It clears detached state and issues `MatrixCmd::OpenRoom` (or `MatrixCmd::OpenThread` when inside a detached thread).

### 3. Four Callers
1. **Global search (`src/app/global_search.rs`):** Replaced `MAX_JUMP_DIG = 15` backward `Paginate` digging loops with direct calls to `jump_to_event` (supporting both room events and thread hits).
2. **Replied-to message (`src/model.rs`, `src/matrix/convert.rs`, `src/action.rs`, `src/keymap.rs`, `src/menu.rs`, `src/palette.rs`, `src/ui/hit.rs`, `src/ui/timeline.rs`, `src/app/menu.rs`, `src/app/palette.rs`, `src/app.rs`, `README.md`):**
   - Populated `Message.reply_event: Option<String>` from `content.in_reply_to`.
   - Added `Action::JumpToReply` (`"jump_to_reply"`, `p` in `Context::Timeline`, `"Go to replied message"`), palette entry (`MessageCaps::can_jump_to_reply`), `"Go to replied message"` in the message context menu (`message_items`), and clickable reply quote rows (`HitId::ReplyQuote`).
   - Non-reply or unavailable reply targets emit a clear toast.
3. **In-message `matrix.to` / `matrix:` links (`src/rich.rs`, `src/manage.rs`, `src/app/rooms.rs`, `src/app.rs`):**
   - Added `matrix:` plain-text link detection (`find_plain_link`) in `src/rich.rs` and `Address::Event { room, event, via }` (with redacted `Debug` and `format_join_target`) in `src/manage.rs`.
   - `App::try_open_matrix_link` dispatches:
     - Event links in joined rooms (by `!id` or canonical alias) -> `jump_to_event`.
     - Event links in unjoined rooms -> toast (`"Join this room before jumping to a message in it"`).
     - Joined room links -> room switch (`open_room`).
     - Unjoined room/alias links -> prefilled `Modal::New` at `Stage::Join` via `open_new_join` (preserving `?via=` servers via `format_join_target`).
     - User links -> prefilled `Modal::New` at `Stage::Direct` via `open_new_user`.
     - Invalid Matrix links -> toast without spawning a browser; non-Matrix URLs still call `open_in_browser`.
4. **CLI positional target (`src/main.rs`, `src/app.rs`):**
   - `Args` accepts an optional positional `target: Option<String>` (with redacted `Debug`).
   - `App::maybe_open_startup_target` defers across `NeedLogin`, opens after `LoggedIn` + `Sync`/`Rooms` arrival once no blocking modal (`Modal::Recovery`, etc.) is open, runs in demo mode too, clears `startup_target` once handled so subsequent `Rooms` updates never re-trigger it, and never opens a second instance / IPC socket.

## Files Changed
- `src/action.rs`, `src/keymap.rs`, `src/menu.rs`, `src/palette.rs`, `README.md`: `Action::JumpToReply`, `p` binding, menu/palette wiring, `jump_to_newest_label()`.
- `src/model.rs`, `src/matrix/convert.rs`, `src/manage.rs`, `src/rich.rs`: `reply_event`, `Address::Event`, `matrix:` link extraction.
- `src/matrix/mod.rs`, `src/matrix/rooms.rs`: `OpenEvent`, `CancelOpenEvent`, `PaginateForwards`, `PaginatedForwards`, `JumpFailed`, `DetachedTimeline`, `DetachedSlot`, `TimelineFocus::Event` `/context` loader, forward pagination, friendly error messages.
- `src/ui/hit.rs`, `src/ui/timeline.rs`, `src/ui/mod.rs`, `src/ui/snapshots/`: `HitId::ReplyQuote`, clickable reply quote row, detached banner + `" loading newer messages… "` pill, anchor on `scroll > 0 || app.timeline.detached`, `detached_banner` snapshot + updated `help_overlay`, `keybinds_vim_help`, and `command_palette*` snapshots.
- `src/main.rs`, `src/app.rs`, `src/app/global_search.rs`, `src/app/thread.rs`, `src/app/rooms.rs`, `src/app/attach.rs`, `src/app/media.rs`, `src/app/menu.rs`, `src/app/palette.rs`, `src/app/complete.rs`, `src/app/search.rs`: `jump_to_event`, `return_to_live`, `maybe_open_startup_target`, `jump_to_reply`, `try_open_matrix_link`, `enter_thread_from_jump`, `allocate_gen`, cancellation, and test coverage.

## Verification
- `./scripts/check.sh` — **passed** (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` **501 passed; 0 failed; 21 ignored**, `cargo build`).
  - Note: a single parallel run earlier saw `matrix::keyring::tests::dbus_secret_service_roundtrip_over_private_bus` time out under CPU load; re-running `./scripts/check.sh` passed cleanly 501/501.
- `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=alicepass cargo test matrix:: -- --ignored --test-threads=1` against the running local homeserver (`http://127.0.0.1:6167`) — **20 passed; 0 failed**, including `matrix::rooms::tests::alice_jumps_to_unloaded_message_across_more_than_60_events` (creates room with 65 messages `msg-000`..`msg-064`, logs in with fresh temp profile verifying `msg-000` is absent from the initial live slice, jumps via `OpenEvent` across `>60` events, verifies `DetachedTimeline` + `PaginateForwards` growth, and returns to live via `OpenRoom`).
- Interactive manual TUI smoke testing was not executed in this headless session; UI rendering is covered by `insta` snapshot tests and `ratatui` `TestBackend` interaction tests in `src/app.rs` and `src/ui/mod.rs`.
