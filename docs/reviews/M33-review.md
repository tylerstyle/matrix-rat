# M33 Review — `Esc` Closes One Layer at a Time (`esc-close`)

- **Branch:** `esc-close` (`39d11d9`, `a5f9cc6`, `34 files changed, 1576 insertions(+), 126 deletions(-)`)
- **Worktree:** `.claude/worktrees/esc-close`
- **Scope:** Static code review of `git diff main...esc-close` and related callers against `docs/PLAN.md` §6a piece A (no builds or tests run; suite reported green at 966 unit / 43 server).
- **Verdict:** **Changes requested** — one real regression inside `RoomInfo` when no chat is open (`InfoRow::Pinned` calls `open_pinned()`, which reads `self.room()` instead of `info.room_id`), plus one minor edge when toggling `esc_closes_chat = false` while narrowed to another space. Everything else in the state model, action gating, worker cleanup, read-state isolation, settings, and small-window UI is solid.

---

## Findings

### 1. [MEDIUM] `InfoRow::Pinned` in `RoomInfo` silently does nothing when no chat is open
- **Where:** `src/app/rooms.rs:1564`, `src/app/moderation.rs:294-318`, `src/manage.rs:1472`
- **Detail:** `Action::RoomInfo` has `needs_open_chat() == false` (`src/action.rs:264`) and opens `Modal::RoomInfo` for `self.target_room()` (`src/app/rooms.rs:107`), so pressing `F4` or using `"Room info…"` / the Command Palette while `chat_closed` is `true` opens info for the room under the sidebar cursor.
  - `InfoPopup::rows()` includes `InfoRow::Pinned` (`"Pinned messages"`) for every joined room (`src/manage.rs:1469-1473`).
  - Activating that row runs `InfoRow::Pinned => return self.open_pinned()` (`src/app/rooms.rs:1564`), but `open_pinned()` starts with `let Some(room) = self.room() else { return; };` (`src/app/moderation.rs:295`).
  - When `chat_closed` is `true`, `self.room()` is `None`, so activating `Pinned messages` inside `RoomInfo` silently returns without opening the popup or showing a toast. (Similarly, if `RoomInfo` was opened from the sidebar while a different room is open, `open_pinned()` opens the *open* room's pins rather than `info.room_id`'s—whereas `InfoRow::Invite` on the next line reads `info.room_id`.)
- **Impact:** One of the top-level rows in `RoomInfo` is dead whenever `chat_closed` is `true`. Notice `PinnedPopup`'s `activate_pinned_selected` (`src/app/moderation.rs:407-418`) already calls `self.jump_to_event(room_id, event_id, None)`, which reopens the closed chat cleanly once the popup opens.
- **Fix:** Split out `open_pinned_for(&mut self, index: Option<usize>)` (or `open_pinned_room(&mut self, room_id: &str)`), passing `self.open_index()` from `Action::OpenPinned` and `info.room_id` (resolved in `self.rooms`) from `InfoRow::Pinned`, and add a regression test opening `Pinned messages` from `RoomInfo` while `chat_closed` is `true`.

### 2. [LOW] Switching `esc_closes_chat = false` while closed and narrowed to another space opens an unlisted room
- **Where:** `src/app.rs:1068-1081` (`Setting::EscClosesChat`), `src/app.rs:690-717` (`visible_rooms`), `src/app.rs:1980-1992` (`show_room`)
- **Detail:** Toggling `Setting::EscClosesChat` off while `self.chat_closed` is `true` reopens `self.active` via `self.open_room(self.active)` (`src/app.rs:1076`). Unlike `show_room()`, `open_room()` neither clears `self.space` when the room is outside `self.space_rooms` nor syncs `self.sidebar.cursor` onto it, and `visible_rooms()` only bypasses `unread_only`/`people_only` (not `space_rooms`) for `open_index()` (`src/app.rs:701-711`). If the user closed chat `A`, switched spaces with `Alt-S` to space `B`, and toggled `esc_closes_chat` off, `A` opens in the chat pane while the room list still shows only space `B` and leaves `A` missing (and the cursor elsewhere).
- **Fix:** Call `self.show_room(self.active)` instead of `self.open_room(self.active)` at `src/app.rs:1076` (leaving the open `Settings` modal untouched, just as `open_room` does).

---

## Checklist Verification

### 1. `App::open_index()` / `chat_closed` (`src/app.rs` and callers)
- Audited all remaining `.active`, `self.room()`, `room_id()`, and `self.rooms[...]` sites across `src/`:
  - `open_index()` (`src/app.rs:747-749`) returns `(!self.chat_closed && self.active < self.rooms.len()).then_some(self.active)`, and `room()` (`753-755`) delegates to it.
  - Non-test uses of `self.active` are confined to: `Debug` (`405`), `open_index` (`748`), `Setting::EscClosesChat` (`1075-1076`), `reset_session` (`1378`), `switch_room_state` (`1512`), and `apply_rooms` (`3581`, `3680`, `3705`, `3719`, `3742`).
  - Callers across `app/{attach,commands,global_search,palette,people,rooms,thread,thread_list,unread}.rs` and `ui/{sidebar,timeline}.rs` were updated to `open_index()` / `room()`.
  - Sidebar open-room highlighting (`▌`, `Role::RoomActive`, `visible_rooms` quick-filter exception) and palette `"current"` hints are cleanly suppressed while closed.

### 2. `close_chat()` / `leave_open_room()` (`src/app.rs:1530-1582`)
- `close_chat()` (`1556-1582`) guards on `let Some(left) = self.open_index()`, cancels any pending jump, calls `leave_thread()` (stashing any open thread draft and restoring `room_draft` / `room_messages` before leaving the room), then calls `leave_open_room()`.
- `leave_open_room()` (`1530-1549`) withdraws active typing (`MatrixCmd::Typing { typing: false }`), calls `save_draft()`, cancels any active reply/edit (`cancel_compose()`), moves `composer.take()` into `self.rooms[open].draft`, and saves room completion marks. Wait-to-send attachments remain scoped by `(room_id, thread)` in `self.attach`, while `attach_scope()` returns `None` while closed (`src/app/attach.rs:157-160`), so `Ctrl-C` / `attach_pasted` cannot touch them while closed and they return on reopen.
- Sets `chat_closed = true`, clears `auto_opened`, resets `TimelineState`, `row_cache`, bumps `timeline_rev` and `unread` line (`no_unread_line()`), allocates a new `generation`, sends `MatrixCmd::CloseRoom`, focuses `Pane::Sidebar` (positioning `sidebar.cursor` on `left` if visible), and requests `repaint`.
- `reset_session()` (`1378-1380`) resets `chat_closed = false` so a new login opens its first room normally.
- `set_focus()` (`1459-1468`) clamps any attempt to focus `Timeline`/`Composer` back to `Pane::Sidebar` while closed.

### 3. `apply_rooms` while closed (`src/app.rs:3575-3746`)
- Captures `left_id` from `self.rooms.get(self.active)` when `self.chat_closed` (`3579-3583`), while `open_id` is `None`.
- Hits `None if self.chat_closed => {}` (`3698`) instead of the auto-open branch, so background syncs—even when the left room disappears or `self.rooms` becomes empty and repopulates—never open a room by themselves (`no_chat_opens_by_itself_when_the_room_list_changes`).
- Re-tracks `self.active` to `left_id` (falling back to `cursor_room().unwrap_or(0)`) at `3739-3745`.
- Explicit open paths (`SidebarOpen` / `HitId::Room`, `step_room`, `on_notice_click`, `try_open_matrix_link`, `open_pending_room`, `on_jump_arrived`, `open_demo_thread`) all go through `switch_room_state` -> clear `chat_closed = false` and reopen cleanly.

### 4. `Action::needs_open_chat()` gating (`src/action.rs`, `src/app.rs`, `src/palette.rs`)
- `Action::needs_open_chat()` (`src/action.rs:179-273`) is an exhaustive match over all `Action` variants with no wildcard arm.
- `App::perform()` (`src/app.rs:2598-2617`) blocks all `needs_open_chat()` actions when `self.chat_closed`: silent no-op for focus/scroll/selection navigation, `"No chat is open"` toast for command actions.
- `PaletteContext` carries `chat_closed: bool` (included in `Debug`), and `action_enabled()` (`src/palette.rs:980-983`) disables `needs_open_chat()` actions while keeping cursor-room actions (`RoomInfo`, `LeaveRoom`, `InviteUser`, `OpenThreads`, `Manage(...)`) and room/space/navigation actions enabled on `target_room()`. (Only gap is `InfoRow::Pinned` inside `RoomInfo`, Finding 1 above.)

### 5. `MatrixCmd::CloseRoom` (`src/matrix/mod.rs`, `src/matrix/rooms.rs`)
- `MatrixCmd::CloseRoom` has redacted `Debug` (`"CloseRoom"`) and `describe()` (`"CloseRoom"`).
- `Timelines::handle` (`src/matrix/rooms.rs:674`) calls `self.close()` (`167-183`), cancelling any `pending_open_event_gen`/`event_opener`, clearing `detached`, and aborting `forwarder`, `paginator`, and `forward_paginator`, while preserving `self.cache` and letting background `member_fetches` finish (`closing_the_room_stops_its_timeline` passes).
- Late `Timeline`, `DetachedNew`, `Paginated`, `PaginatedForwards`, and `Typing` events are also dropped UI-side because `close_chat()` allocates a fresh `generation` and `room_id()` returns `None`.

### 6. Nothing marked read while closed
- `maybe_mark_read()` (`src/app.rs:2240-2245`) immediately resets `self.timeline.drawn = false` and returns when `self.chat_closed` is `true`, so no `MarkRead` command or read receipt/marker update can trigger from ticks, redraws, focus gain, or late timeline events.
- `on_notice()` (`src/app.rs:1203-1210`) checks `self.room().is_some_and(...)`, so `watching` is `false` and messages arriving in the room that was left still notify normally (`count_notices` in `apply_rooms` also treats it like any other room).

### 7. `src/ui/nochat.rs` & narrow/hidden layout (`src/ui/mod.rs`, `src/ui/nochat.rs`)
- `ui::draw()` (`src/ui/mod.rs:63-95`) forces `(Some(main), None)` in `one_pane` mode (`width < 60` or `sidebar.hidden`) whenever `app.chat_closed()` is `true`, and renders `nochat::draw` into `chat_area` in wide mode when `sidebar.hidden` is `false`.
- `nochat::draw()` (`src/ui/nochat.rs:40-106`) handles degenerate areas safely: returns early for `inner.width < 4 || inner.height == 0`, hides the title when `inner.height < hints.len() + 2`, clamps `block_width` to `inner.width` and truncates title/descriptions without unsigned underflow, reads live bindings via `keymap.labels_from(Context::Sidebar, ...)`, omits unbound actions, and binds each row to `HitId::Do(action)`. Covered down to `(1, 1)` and snapshotted at `100x30`, `80x24`, and `50x20`.

### 8. `esc_closes_chat` setting & docs (`src/config.rs`, `src/modal.rs`, `src/palette.rs`, `README.md`)
- `Config.esc_closes_chat` defaults to `true`, is redacted/safe in `Debug`, appears in `Setting::ALL` (`23` items), `SettingsMenu`, and `collect_settings` (with aliases `"escape"`, `"close chat"`, `"no chat open"`, `"setting"`), and persists via `save_setting("esc_closes_chat", ...)`.
- Known open points match intent: `vim` preset overrides `Esc` in `Composer` to `Focus(Pane::Timeline)` (so `Esc` never closes the chat unless `back` is given another key under `[keys.composer]`), and there is no separate `CloseChat` action. `README.md` (`170`, `180`, `378`, `392`) matches the implemented behaviour cleanly.

---

## Follow-up (`550f298`)

- Checked commit `550f298` (`967` unit tests reported green):
  - **Finding 1 fixed:** `open_pinned_of(&mut self, room_id: &str)` (`src/app/moderation.rs:295-327`) handles any room by ID (`Action::OpenPinned` delegates via `self.room_id()`; `InfoRow::Pinned` at `src/app/rooms.rs:1564` passes `&room_id`), covered by `the_room_info_shows_the_pinned_messages_of_its_own_room_with_no_chat_open` (`src/app/rooms.rs:4430-4447`).
  - **Finding 2 fixed:** `Setting::EscClosesChat` (`src/app.rs:1075-1080`) now calls `self.show_room(self.active)` so toggling the setting off while closed leaves a non-matching space filter and moves the cursor onto the reopened room.

**M33 ok**
