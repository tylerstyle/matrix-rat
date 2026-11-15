# Rooms Block Handover (`agy/rooms`)

Branch: `agy/rooms` in `/home/tylerstyle/git/matrix-rat/.claude/worktrees/agy-rooms` (merged `main` `534a5d4` in `0f01cef`).
Out of scope per brief: room upgrade is not implemented.

## Commits Since `main`
- `6995fd6` — `people: omit Verify user when already verified`
- `9ec31c9` — `rooms: public room directory browse, preview/knock flows, and pending knocks`
- `faa3958` — `Review M19: re-check` (`docs/reviews/M19-review.md`)
- `83c2525` — `rooms: incoming knock requests with accept/decline/ban in room info`
- `0f01cef` — `Merge branch 'main' into agy/rooms`
- `ff882ba` — `Room settings, sync refresh, shared avatar helpers, and tests`
- `7a750da` — `Add server tests for directory/knocks/room settings and sync knocks via /members`

## Scope Implemented

### 1. Public Room Directory, Room Preview & Pending Knocks (`9ec31c9`, `7a750da`)
- **Modules:** `src/matrix/directory.rs`, `src/matrix/mod.rs`, `src/matrix/rooms.rs`, `src/manage.rs`, `src/app/rooms.rs`, `src/ui/roominfo.rs`, `src/ui/timeline.rs`, `src/action.rs`, `src/keymap.rs`, `src/palette.rs`, `src/menu.rs`, `src/modal.rs`, `src/model.rs`, `src/app.rs`
- **Public directory search (`Stage::Browse`):**
  - Added `Stage::Browse` (`"Browse public rooms"` / `"Discover rooms on this or another server"`) to `Stage::CHOICES` in `Ctrl-N` (`NewPopup`) and to the command palette (`"Browse public rooms…"`, `Action::Manage(Click::Stage(Stage::Browse))`).
  - `matrix_sdk::room_directory_search::RoomDirectorySearch` is driven via `MatrixCmd::{SearchDirectory, DirectoryNextPage, CancelDirectorySearch}` and `MatrixEvent::{DirectoryResults, DirectoryError}` (`PublicRoom`, `DirectoryPage`), with optional server filtering validated by `validate_server` (rejecting whitespace, control characters, `/`, `?`, `#`).
- **Room preview & join/knock (`Stage::Preview`):**
  - `fetch_preview` calls `client.get_room_preview` alongside `get_summary::v1::Request` under `PREVIEW_TIMEOUT`, returning `RoomPreview` (`PreviewJoinRule`, `PreviewMembership`).
  - Selecting a directory row (`Click::BrowseRow(usize)`) or opening an unjoined room / `matrix.to` link enters `Stage::Preview`, displaying membership state badges (`already joined`, `invited`, `request pending`, `banned`), topic, join/knock/open actions, and `PreviewButton::{Open, Accept, Withdraw, Join}` flows.
- **Pending outgoing knocks:** Tracked via `RoomCategory::Knock` and withdrawn with `Action::WithdrawKnock` (`"withdraw_knock"`, `"Withdraw join request"`).
- **Privacy & tests:** Added `debug_impls_redact_private_directory_and_preview_fields`, modal/palette snapshots (`new_chooser.snap`, `directory_browse.snap`, `room_preview_knock.snap`, updated palette snapshots), and server integration test `matrix::directory::tests::public_room_directory_search_preview_and_join` (with cleanup via `ManageCmd::Leave`, `Logout`, `Shutdown`, `SetRoomVisibility(..., Visibility::Private)`, `room.leave()`, `room.forget()`).

### 2. Incoming Knock Requests & Live `/members` Knock Sync (`83c2525`, `ff882ba`, `7a750da`)
- **Modules:** `src/manage.rs`, `src/matrix/manage.rs`, `src/matrix/mod.rs`, `src/app/rooms.rs`, `src/people.rs`, `src/ui/roominfo.rs`
- Moderators see pending knock requests in Room Info (`F4`) with accept, decline, and ban controls.
- **Knock sync fix (`7a750da`):** On Continuwuity with Sliding Sync, `room.subscribe_to_knock_requests()` emitted empty updates (`len=Some(0)`) even when `GET /members` returned `membership="knock"`, overwriting `current.knock_requests`. Replaced `knock_sub` in `watch()` with `ServerMembers { entries, knocks }`, `parse_member_events(&[Raw<RoomMemberEvent>])`, `fetch_member_events(&client, &room, membership)` -> `server_members` (no membership filter, populating both `entries` and `knocks` in one `/members` request) and `server_knocks` (`membership = Some(MembershipState::Knock)` for cached/`too_large` refreshes), driven by `force_knocks` / `knocks_loaded` when `member_handler` (`SyncRoomMemberEvent`) or `spawn_member_action` bumps `Refresh`.
- **Tests:** Added unit test `parse_member_events_separates_members_and_knocks` and server integration test `matrix::manage::tests::knock_room_bob_knocks_alice_sees_and_accepts`.

### 3. Room Settings, Permissions & Live Sync (`ff882ba`, `7a750da`)
- **Modules:** `src/manage.rs`, `src/matrix/manage.rs`, `src/matrix/mod.rs`, `src/app/rooms.rs`, `src/ui/roominfo.rs`, `README.md`
- **Collapsible Room settings (`▸ Room settings` / `▾ Room settings`):** Placed above `Members` in Room Info (`F4`) with 7 setting rows (`Who can join`, `History`, `Encryption`, `Room picture`, `Addresses`, `Direct chat`, `Permissions`), backed by `RoomSetting`, `SettingSubview`, `PermAction`, and `AddressEntry`.
- **History & Permissions subviews:**
  - Changing history displays `WARNING: Changing history visibility only affects future messages.` above the 4 `History` choices (`Shared (all history)`, `From invite (current)`, `From join`, `World readable`).
  - Permissions subview displays the caller's power-level guard (`Your power level: 100 · edits cannot exceed it or lock you out.`), 6 permission actions (`Send messages`, `Invite members`, `Kick members`, `Ban members`, `Delete messages`, `Change settings`), presets (`[Default (0)]`, `[Moderator (50)]`, `[Admin (100)]`), `[Save]`, and `[Back]`.
- **Live state sync:** `src/matrix/mod.rs` watches `SyncRoomJoinRulesEvent` and `SyncRoomHistoryVisibilityEvent` alongside room member/manage refresh paths.
- **Snapshots & tests:** Added `room_settings_toggle_all_views_and_guards` (using `crate::images::demo_picture(32, 20)`), updated `room_info.snap`, `room_info_filtered.snap`, `room_info_small.snap`, `room_info_moderation.snap`, added `room_info_settings_expanded.snap`, `room_settings_history_warning.snap`, `room_settings_permissions.snap`, and added server integration tests `matrix::manage::tests::room_settings_join_rule_and_history_sync_to_member` and `matrix::manage::tests::enabling_encryption_in_fresh_room` using shared `ServerWorker` (`login`, `manage`, `cleanup`), `test_server_and_accounts`, and `wait_knocked`.

### 4. Shared Avatar Helpers & Documentation (`ff882ba`)
- **Modules:** `src/app.rs`, `src/app/attach.rs`, `src/app/people.rs`, `src/app/rooms.rs`, `src/matrix/people.rs`, `src/ui/hit.rs`
- Shared avatar image/file handling across User Info, Own Profile, and Room Picture settings; documented `Ctrl-N` (**Browse public rooms**, unjoined room preview) and `F4` (**Room settings**, join requests) in `README.md`.

### 5. People Follow-up (`6995fd6`)
- **Modules:** `src/people.rs`, `src/app/people.rs`, `src/app/snapshots/matrix_rat__app__people__tests__user_info_modal.snap`
- `UserInfoPopup::rows` omits `UserAction::Verify` when `self.verify_status() == UserVerifyStatus::Verified` (while `UserInfoPopup::action_disabled_reason` still returns `Some("This user is already verified.")`), so verified profiles in `user_info_modal.snap` (`Security    verified`) no longer offer `Verify user`.

## Verification
Verified on commit `7a750da` (all Rust/test code identical to HEAD; only this handover markdown document was added afterwards) via `.claude/fury-check.sh --server /home/tylerstyle/git/matrix-rat/.claude/worktrees/agy-rooms` after confirming no concurrent server test processes locally or on `hpfury`:
- `cargo fmt`: clean prior to check
- `.claude/fury-check.sh --server`: exited `0`
  - `cargo clippy`: passed (`0` warnings/errors, `14.57s`)
  - Unit tests (`cargo test`): `627 passed; 0 failed; 40 ignored; 0 measured; 0 filtered out; finished in 5.14s`
  - `cargo build`: passed (`1m 02s`)
  - Server suite (`matrix:: -- --ignored --test-threads=1` on `hpfury`): `39 passed; 0 failed; 0 ignored; 0 measured; 628 filtered out; finished in 85.09s` (including `matrix::directory::tests::public_room_directory_search_preview_and_join`, `matrix::manage::tests::knock_room_bob_knocks_alice_sees_and_accepts`, `matrix::manage::tests::room_settings_join_rule_and_history_sync_to_member`, `matrix::manage::tests::enabling_encryption_in_fresh_room`, and all 35 prior server tests)
