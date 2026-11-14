# Moderation, Pins & Forward (branch `agy/moderation`)

- **Worktree:** `/home/tylerstyle/git/matrix-rat/.claude/worktrees/agy-moderation`
- **Branch:** `agy/moderation`
- **Base (`main`):** `992560d`
- **Cherry-picked prerequisite:** `ec431ab` (cherry-pick of `notify-timeout` `12d348f` touching only [src/notify.rs](../../src/notify.rs); the stray 0-byte `src/focus.rs` from `12d348f` was omitted).

## Summary of Features

### 1. Moderator Basics (Power-Level Gated)
- **Room power tracking ([src/model.rs](../../src/model.rs), [src/matrix/mod.rs](../../src/matrix/mod.rs), [src/matrix/manage.rs](../../src/matrix/manage.rs), [src/matrix/rooms.rs](../../src/matrix/rooms.rs)):**
  - Added `RoomPower` (`own_level`, `own_is_creator`, `can_redact_other`, `can_ban`, `can_unban`, `can_kick`, `can_pin`, `can_change_roles`, `users_default`, `creators`, `member_levels`) with `RoomPower::outranks` and `RoomPower::can_ban_user`, plus `MatrixEvent::RoomPower` emitted once when a room's timeline opens and refreshed on role/power updates (`ManageCmd::SetRole`, `Room::power_levels_or_default`, `Manager::refresh_signal`).
  - Redacted `RoomPower` `Debug` output so user IDs and member maps stay out of logs.
- **Delete (redact) with optional reason ([src/modal.rs](../../src/modal.rs), [src/ui/modal.rs](../../src/ui/modal.rs), [src/app/moderation.rs](../../src/app/moderation.rs), [src/matrix/rooms.rs](../../src/matrix/rooms.rs)):**
  - Moderators (`can_redact_other`) can delete other members' messages via the message menu (`"Delete…"` -> `ReasonConfirmPopup`), `Action::Delete` (`"Delete message"`), or command palette. Own messages continue to support the menu's double-confirm (`"Delete"` -> `"Again to delete"`), while `ReasonConfirmPopup::redact` supports an optional sanitized reason; empty/blank reasons omit the `reason` field when calling `timeline.redact`, and the `Debug` implementation redacts the text (`reason_len` only).
- **Ban / Unban ([src/manage.rs](../../src/manage.rs), [src/ui/roominfo.rs](../../src/ui/roominfo.rs), [src/app/rooms.rs](../../src/app/rooms.rs), [src/app/moderation.rs](../../src/app/moderation.rs), [src/matrix/manage.rs](../../src/matrix/manage.rs)):**
  - `stored_members` and `server_members` now load `Banned` memberships alongside `Joined` and `Invited`, exposing `RoomMember.{power_level, banned, bannable, unbannable, role_editable}` (gated via Matrix power-level checks and strictly outranking target).
  - Room Info renders a `"Banned"` section with unban actions (`unbannable` -> `"Unban"` / confirm) and `"Ban"` (`Alt-B`) where `bannable`, plus `"Ban user…"` (`ReasonConfirmPopup::ban`) on message context menus (`Run::BanUser`) when `RoomPower::can_ban_user` holds.
  - Added `ManageCmd::Ban`, `ManageCmd::Unban`, and watcher `refresh` signalling in `watch()` (`src/matrix/manage.rs`) so banned/unbanned membership changes re-query `server_members` immediately without overwriting an existing complete list with an incomplete `stored_members` cache. Also tightened `a_room_from_creation_to_forgetting` (`&& !d.members_loading`) against the transient loading snapshot when reopening `ManageCmd::Info`.
- **Member Role Chooser ([src/manage.rs](../../src/manage.rs), [src/ui/roominfo.rs](../../src/ui/roominfo.rs), [src/app/rooms.rs](../../src/app/rooms.rs), [src/matrix/manage.rs](../../src/matrix/manage.rs)):**
  - Added `RoleChooser`, `member_role_display` (`Admin`, `Moderator`, `Default`, `Creator`, or custom numeric power level), and `ManageCmd::SetRole` (`room.update_power_levels`) triggered via `"Role"` / `Alt-R` on `role_editable` member rows in Room Info.
  - Guarded last-admin self-demotion (`RoleChooser::needs_last_admin_confirm` when `self.is_self && self.current_level >= 100 && new_level < 100 && self.admin_count <= 1`) behind an explicit second confirmation.

### 2. Pinned Messages (`m.room.pinned_events`)
- **Worker & Pin Cache ([src/matrix/moderation.rs](../../src/matrix/moderation.rs), [src/matrix/rooms.rs](../../src/matrix/rooms.rs)):**
  - Emits `MatrixEvent::PinnedEvents` when opening a room and via `room.pinned_event_ids_stream()`.
  - `pin()` uses `room.pin_event` / `room.unpin_event` and returns the updated pinned list immediately (applying the toggle onto the current/loaded pinned IDs rather than returning pre-sync cached `room.pinned_event_ids()`), invalidating any affected entry in `PinnedCache`.
  - `fetch_pinned()` loads full events (outside the loaded timeline if needed via `load_event_with_replacements`, applying edits, UTD healing, `m.ignored_user_list` masking, and `Redacted` status), preserving `PinnedMessage`'s redacted `Debug`.
- **App, Timeline & Popup UI ([src/app/moderation.rs](../../src/app/moderation.rs), [src/modal.rs](../../src/modal.rs), [src/ui/modal.rs](../../src/ui/modal.rs), [src/ui/timeline.rs](../../src/ui/timeline.rs), [src/ui/roominfo.rs](../../src/ui/roominfo.rs), [src/keymap.rs](../../src/keymap.rs)):**
  - Added `Action::PinMessage` (`pin_message`), `Action::OpenPinned` (`open_pinned`, bound to `P` in `Context::Timeline`), pinned pill indicator in timeline header & message rows (`Message.pinned`), and `"Pinned messages"` button in Room Info (`InfoRow::Pinned`).
  - `Modal::Pinned` (`PinnedPopup`) shows sender, day/time, and snippet, supports `Enter`/click to jump (including to unloaded events via `OpenEvent`), and `Del`/`u`/button to unpin (gated on `can_pin`).
  - `App::on_pinned_list` masks `PinnedMessage` items whose `sender_id` is in `self.ignored_users` consistently with the worker.

### 3. Forward Message (`Forward…`)
- **Picker, Command & Copy ([src/modal.rs](../../src/modal.rs), [src/palette.rs](../../src/palette.rs), [src/app/moderation.rs](../../src/app/moderation.rs), [src/matrix/moderation.rs](../../src/matrix/moderation.rs), [src/menu.rs](../../src/menu.rs), [src/ui/status.rs](../../src/ui/status.rs)):**
  - Added `Action::ForwardMessage` (`forward_message`, message context menu & command palette `"Forward…"`), opening `Modal::Palette(Palette::open_forward(&ctx.rooms, target))` over joined rooms (including rooms outside the active space/sidebar filter).
  - `forward_message()` loads the confirmed source event and sends a **standalone copy** to `to_room` — stripping `m.relates_to` (`m.in_reply_to`, thread relation, edit aggregation metadata) so forwarded replies/threads do not carry broken reply/thread context, and downloading/re-uploading image/file/audio/video attachments cleanly so encrypted media is re-encrypted for the destination room.
  - Emits `MatrixEvent::ForwardResult`, showing `"Forwarded to <room>"` paired with a clickable `" Open → "` status pill (`HitId::Toast`, opening the target room on click) and surfacing errors cleanly without leaking message contents into logs.

## Files Changed

- **New code & review docs (3):**
  - `src/app/moderation.rs`
  - `src/matrix/moderation.rs`
  - `docs/reviews/moderation-handover.md`
- **Modified tracked files (27):**
  - `README.md`
  - `src/action.rs`, `src/keymap.rs`, `src/manage.rs`, `src/menu.rs`, `src/modal.rs`, `src/model.rs`, `src/palette.rs`
  - `src/app.rs`, `src/app/global_search.rs`, `src/app/menu.rs`, `src/app/palette.rs`, `src/app/rooms.rs`, `src/app/safety.rs`, `src/app/thread.rs`, `src/app/unread.rs`
  - `src/matrix/manage.rs`, `src/matrix/mod.rs`, `src/matrix/rooms.rs`, `src/matrix/search.rs` (`src/notify.rs` already committed in cherry-pick `ec431ab`)
  - `src/ui/hit.rs`, `src/ui/mod.rs`, `src/ui/modal.rs`, `src/ui/palette.rs`, `src/ui/roominfo.rs`, `src/ui/status.rs`, `src/ui/timeline.rs`
- **New UI snapshots (4):**
  - `src/ui/snapshots/matrix_rat__ui__tests__pinned_badge_and_forward_toast.snap`
  - `src/ui/snapshots/matrix_rat__ui__tests__pinned_popup.snap`
  - `src/ui/snapshots/matrix_rat__ui__tests__reason_confirm_redact.snap`
  - `src/ui/snapshots/matrix_rat__ui__tests__room_info_moderation.snap`
- **Updated existing snapshots (11):**
  - `src/app/snapshots/matrix_rat__app__menu__tests__menu_by_key.snap`
  - `src/app/snapshots/matrix_rat__app__menu__tests__menu_corner.snap`
  - `src/app/snapshots/matrix_rat__app__menu__tests__menu_message.snap`
  - `src/app/snapshots/matrix_rat__app__rooms__tests__room_info.snap`
  - `src/app/snapshots/matrix_rat__app__rooms__tests__room_info_filtered.snap`
  - `src/app/snapshots/matrix_rat__app__rooms__tests__room_info_small.snap`
  - `src/ui/snapshots/matrix_rat__ui__tests__command_palette.snap`
  - `src/ui/snapshots/matrix_rat__ui__tests__command_palette_filtered.snap`
  - `src/ui/snapshots/matrix_rat__ui__tests__command_palette_narrow.snap`
  - `src/ui/snapshots/matrix_rat__ui__tests__help_overlay.snap`
  - `src/ui/snapshots/matrix_rat__ui__tests__keybinds_vim_help.snap`

## Tests Added & Updated

- **Manage / Modal unit tests ([src/manage.rs](../../src/manage.rs), [src/modal.rs](../../src/modal.rs)):**
  - `role_display_and_moderation_prompts_cover_custom_levels_guard_and_redacted_debug`
  - `moderation_modals_redact_debug_and_navigate_cleanly`
  - Updated `info_rows_follow_permissions_and_the_filter` ([src/manage.rs](../../src/manage.rs))
- **App-flow tests ([src/app/moderation.rs](../../src/app/moderation.rs)):**
  - `redact_permissions_reason_popup_own_message_and_unread_line`
  - `room_info_ban_unban_role_chooser_and_message_menu_ban`
  - `pins_permissions_badge_popup_jump_unpin_redacted_ignored_and_unread_line`
  - `forward_guards_picker_execution_and_toast_open_from_filtered_or_thread_view`
  - Updated menu expectations in [src/app/menu.rs](../../src/app/menu.rs)
- **UI snapshot tests ([src/ui/mod.rs](../../src/ui/mod.rs)):**
  - `moderation_modals_and_pinned_toast_snapshots`
- **Continuwuity server tests (`#[ignore]`, [src/matrix/moderation.rs](../../src/matrix/moderation.rs)):**
  - `server_unprivileged_member_cannot_ban_or_pin`
  - `server_admin_set_role_ban_and_unban`
  - `server_pin_and_unpin_sync_and_fetch_unloaded`
  - `server_forward_text_reply_and_attachment_as_standalone_copies`

## Verification

Run on `hpfury` via `.claude/fury-check.sh --server /home/tylerstyle/git/matrix-rat/.claude/worktrees/agy-moderation` after all code, README, and snapshot edits (with local Continuwuity at `127.0.0.1:6167` forwarded over SSH):
- `nix-shell shell.nix --run scripts/check.sh` (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build`) exited `0`:
  - `cargo test`: `test result: ok. 553 passed; 0 failed; 30 ignored; 0 measured; 0 filtered out; finished in 3.49s`
  - `find src -name '*.snap.new'`: empty (zero unreviewed `.snap.new` files)
- `RUST_LOG=off MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=alicepass cargo test matrix:: -- --ignored --test-threads=1` exited `0`:
  - `test result: ok. 29 passed; 0 failed; 0 ignored; 0 measured; 554 filtered out; finished in 80.94s`
  - All 4 new `matrix::moderation::tests::*` tests and all 25 existing `matrix::*` tests (including `matrix::manage::tests::a_room_from_creation_to_forgetting`) passed cleanly.
