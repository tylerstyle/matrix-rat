# People Block Handover (`agy/people`)

Branch: `agy/people` (branched from `main` `1df4ffe`, v0.3.0)

## Scope Implemented

### 1. User Info Popup
- **Modules:** `src/people.rs`, `src/matrix/people.rs`, `src/app/people.rs`, `src/ui/people.rs`, `src/modal.rs`, `src/ui/hit.rs`, `src/ui/timeline.rs`, `src/action.rs`, `src/keymap.rs`, `src/menu.rs`, `src/palette.rs`, `src/ui/roominfo.rs`
- **Entry points:**
  - Clicking a sender name or avatar tile in the timeline (`Hit::MessageSender`) or activating a `matrix.to/@...` user link
  - Pressing `Enter` on a member row in Room Info (`src/ui/roominfo.rs` member footer shortened to `"Enter info · type to filter · Esc close"` so narrow viewports do not clip)
  - Message context menu (`User info`) and command palette (`Action::UserInfo` / `"user_info"` under `Context::Timeline`, unbound by default)
  - Selecting a user from user-search results
- **Fields & behavior (`UserInfoPopup`):**
  - Shows avatar (image where supported, fallback tile otherwise), sanitized display name (`clean_name`), `@user_id` with copy action, room role, verification status (`UserVerifyStatus`), ignore state, and shared rooms (jumping on `Enter`).
  - Contextual `UserAction` list (`DirectChat`, `Mention`, `Verify`, `IgnoreToggle`, `Role`, `Kick`, `Ban`, `Unban`) gated by `UserInfoCtx` and room power levels, using `Confirm`, `RoleChooser` (including preserving `confirm_last_admin` when clicking the already-selected option in `App::pick_user_info_role_option`), and `BanPrompt`.
- **Presence / last-seen omitted:** Neither sliding sync (`SyncService`) nor Continuwuity exposes presence/last-seen reliably for this client flow, so no synthetic or guessed presence field is displayed.

### 2. Own Profile Popup & Composer Border Badge
- **Modules:** `src/people.rs`, `src/matrix/people.rs`, `src/app/people.rs`, `src/ui/people.rs`, `src/ui/composer.rs`
- Opened from **Settings -> Profile** or the composer border identity badge (`ui::composer::draw_own_badge` -> `OwnProfilePopup`).
- Supports setting/clearing the global display name (`SetOwnDisplayName`) and uploading/replacing/removing the global avatar (`SetOwnAvatar` / `RemoveOwnAvatar`) with `Tab` file-path completion and upload checks (`upload::check` / `upload::prepare`).

### 3. Per-Room Display Name (`Name in room...`) & Live Sync
- **Modules:** `src/matrix/people.rs`, `src/matrix/manage.rs`, `src/matrix/rooms.rs`, `src/matrix/mod.rs`, `src/app/rooms.rs`, `src/app/thread.rs`, `src/app/people.rs`
- Per-room display name editing and clearing via Room Info (`ManageCmd::SetOwnName`), updating timelines, open/stashed threads, Room Info, and the composer badge live.
- **Sliding sync state cache (`OwnRoomNames`):** Under `matrix-sdk-base` MSC4186 sliding sync, `update_any_room` updates member state in `room.get_member_no_sync` only from `required_state` and ignores state events delivered in `timeline`, so live `m.room.member` display name changes delivered in `timeline` left `room.get_member_no_sync(own)` stale until reconnect. Added `OwnRoomNames` populated from both `SyncRoomMemberEvent` (when `state_key == own`) and `ManageCmd::SetOwnName`, shared with `load_own_profile`, `room_own_display_name`, `context_room_info`, and `manage::head` / `MemberList::apply`, waking both `People::sync_poke` and `own_refresh`.

### 4. Privacy & Redaction
- Redacting `Debug` implementations on `OwnProfileAction`, `OwnProfile`, `UserSharedRoom`, `UserProfile`, `UserInfoPopup`, and `OwnProfilePopup` omit user IDs, display names, room names/IDs where private, file paths, URIs, and error notes from logs.

### 5. Room-List Filter Chips (Item 3)
- Left always visible (unchanged) per user feedback on v0.3.0.

### 6. Documentation & Snapshots
- Updated `README.md` (`Ctrl-N`, `F4`, User info, **Settings -> Profile** & composer badge, **Name in room...**, and `user_info` under **Messages (timeline)**).
- Added unit tests and modal snapshots (`user_info_modal.snap`, `own_profile_modal.snap`), plus `own_room_names_tracks_and_clears_synced_member_events` and server test `matrix::people::tests::own_profile_room_name_avatar_and_user_info_sync`.

## Verification
- `cargo fmt --check`: passed (clean)
- `cargo clippy --all-targets -- -D warnings`: passed (0 warnings)
- `cargo test --locked`: 580 passed, 0 failed, 31 ignored
- `.claude/fury-check.sh --server` (`scripts/check.sh` + server suite):
  - `scripts/check.sh`: 580 unit tests passed, 0 failed, 31 ignored; release/dev build clean
  - `cargo test matrix:: -- --ignored --test-threads=1`: 30 passed, 0 failed
