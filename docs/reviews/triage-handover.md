# Triage (branch `agy/triage`)

- **Worktree:** `/home/tylerstyle/git/matrix-rat/.claude/worktrees/agy-triage`
- **Branch:** `agy/triage`
- **Base (`main`):** `0d5d437`

## Summary of Features

### 1. Detached Banner (`… · N new`) & `on_notice` Guard
- **Worker ([src/matrix/mod.rs](src/matrix/mod.rs), [src/matrix/rooms.rs](src/matrix/rooms.rs)):**
  - `LiveTailCounter` tracks new tail appends on the cached live timeline (for both rooms and threads) while a detached `TimelineFocus::Event` slice is open, emitting `MatrixEvent::DetachedNew` (redacted in `Debug`).
- **App & UI ([src/app.rs](src/app.rs), [src/ui/timeline.rs](src/ui/timeline.rs)):**
  - `app.timeline.detached_new` tracks live arrivals while detached and resets when returning to the live timeline; `ui::timeline` appends `" · N new"` to the detached banner when `app.timeline.detached_new > 0`.
  - `on_notice` in [src/app.rs](src/app.rs) requires `!detached`.

### 2. Low Priority Rooms (`m.lowpriority`)
- **Model & Worker ([src/model.rs](src/model.rs), [src/matrix/mod.rs](src/matrix/mod.rs), [src/matrix/manage.rs](src/matrix/manage.rs)):**
  - Added `RoomCategory::LowPriority` / `RoomKind::LowPriority`, plus `Room.direct` and `Room.muted` (synced live via `client.notification_settings().await.subscribe_to_changes()`).
  - Added `ManageCmd::SetLowPriority`; setting `LowPriority` clears `Favourite` and setting `Favourite` clears `LowPriority` (mutually exclusive per Matrix spec), synced from other clients while preserving direct-chat and mute status.
- **UI & Actions ([src/manage.rs](src/manage.rs), [src/menu.rs](src/menu.rs), [src/palette.rs](src/palette.rs), [src/ui/roominfo.rs](src/ui/roominfo.rs), [src/ui/sidebar.rs](src/ui/sidebar.rs), [src/ui/timeline.rs](src/ui/timeline.rs), [src/app/](src/app.rs)):**
  - Renders a dedicated `"Low priority"` section at the bottom of the room list (mirroring `"Favourite"` at the top), with toggles in the room context menu, Room Info (`Click::LowPriority` / `InfoRow::LowPriority`), and the command palette.

### 3. Room List Quick Filters (`Unread only` & `People only`)
- **Actions & Keybinds ([src/action.rs](src/action.rs), [src/keymap.rs](src/keymap.rs), [README.md](README.md)):**
  - Added `Action::FilterUnread` (`filter_unread`, `"Filter: Unread only"`, proposed free default **`Alt-O`**) and `Action::FilterPeople` (`filter_people`, `"Filter: People only"`, proposed free default **`Alt-D`**) in `Context::Global`, with palette aliases (`"unread rooms only"`, `"unread only"`, `"direct chats only"`, `"people only"`, etc.) tuned so `"direct chat"` still ranks `"Start a direct chat…"` first.
  - Adding two default bindings grew the 2-column Help overlay by 1 row (`31` rows; still fits without scrolling at `100x34`).
- **Session-only State & Sidebar UI ([src/app.rs](src/app.rs), [src/ui/sidebar.rs](src/ui/sidebar.rs), [src/palette.rs](src/palette.rs), [src/app/palette.rs](src/app/palette.rs)):**
  - Stored in `SidebarState.{unread_only, people_only}` (not persisted to `Config`), cleared on `reset_session()` and `Esc` in `App::back()` when in the sidebar.
  - `visible_rooms` combines `unread_only`, `people_only`, active space selection, and text filtering while keeping `self.active` available.
  - Wide sidebars (`inner.width >= 18`) show clickable filter chips; narrow sidebars (`inner.width < 18`) show compact `U` / `P` / `UP` badges in the filter bar so no extra vertical row is consumed at narrow widths.

### 4. Space Unread Counts & Palette Switcher
- **Unread Aggregation ([src/model.rs](src/model.rs), [src/app.rs](src/app.rs)):**
  - Added `SpaceUnread { unread, notifications, mention }` and shared helpers (`space_display_name`, `space_room_ids`, `aggregate_space_unread`, `all_spaces_unread`, `space_unread`, `selected_space_unread`, `other_spaces_unread`).
  - Level-0 spaces roll up rooms from their sub-spaces without internal double-counting, exclude `Room.muted` rooms, and let a room appearing in two separate level-0 spaces count toward both.
- **Header Indicator & Palette Switcher ([src/ui/sidebar.rs](src/ui/sidebar.rs), [src/palette.rs](src/palette.rs), [src/app/palette.rs](src/app/palette.rs), [src/ui/palette.rs](src/ui/palette.rs)):**
  - `draw_space_header` displays the active space badge (notification count or unread dot) plus an other-spaces indicator (`+N` / `•`) summarizing unread activity outside the selected space.
  - Added `Category::Space`, `Target::Space`, `SpaceItem`, and `collect_spaces` so spaces (and their `SpaceUnread` badges) can be jumped to from `Ctrl-P`.

## Files Changed

- **New files (3):**
  - `docs/reviews/triage-handover.md`
  - `src/ui/snapshots/matrix_rat__ui__tests__narrow_sidebar_quick_filters.snap`
  - `src/ui/snapshots/matrix_rat__ui__tests__triage_sidebar_spaces_low_priority_and_filters.snap`
- **Modified tracked code & docs (26):**
  - `README.md`
  - `src/action.rs`, `src/keymap.rs`, `src/manage.rs`, `src/menu.rs`, `src/model.rs`, `src/palette.rs`
  - `src/app.rs`, `src/app/complete.rs`, `src/app/drafts.rs`, `src/app/keybinds.rs`, `src/app/media.rs`, `src/app/menu.rs`, `src/app/palette.rs`, `src/app/rooms.rs`, `src/app/safety.rs`, `src/app/thread.rs`, `src/app/unread.rs`
  - `src/matrix/manage.rs`, `src/matrix/mod.rs`, `src/matrix/rooms.rs`
  - `src/ui/mod.rs`, `src/ui/palette.rs`, `src/ui/roominfo.rs`, `src/ui/sidebar.rs`, `src/ui/timeline.rs`
- **Updated existing snapshots (46):**
  - `src/app/snapshots/*.snap` (`chips`, `chips_many`, `demo_has_a_picture`, `menu_by_key`, `menu_corner`, `menu_message`, `new_chooser`, `new_create`, `room_info`, `room_info_filtered`, `room_info_small`, `ignored_users_popup`, `report_form_modal`, `user_search_dropdown`, `thread_summary_line`, `thread_view`)
  - `src/ui/snapshots/*.snap` (`command_palette`, `command_palette_filtered`, `command_palette_narrow`, `composer_completions`, `detached_banner`, `emoji_picker`, `find_bar`, `formatted_covered`, `formatted_revealed`, `global_search`, `global_search_empty`, `help_overlay`, `keybinds_conflict`, `keybinds_editor`, `keybinds_vim_help`, `login_form`, `main_screen`, `message_kinds`, `message_selected_and_reply`, `narrow_rooms`, `room_filter`, `room_previews`, `settings_menu`, `settings_menu_low`, `verify_compare`, `verify_compare_too_small`, `verify_done`, `verify_requested`, `verify_set_up_password`, `verify_status_hint`) — shifted by the sidebar quick-filter chips row at normal width (and `help_overlay` / `command_palette` / `detached_banner` / `room_info` feature additions).

## Tests Added & Updated

- **Unit & Palette tests:**
  - `detached_banner_counts_new_messages_and_ignores_notices_when_detached` ([src/app.rs](src/app.rs))
  - `room_list_quick_filters_combine_and_reset_on_esc_and_logout` ([src/app.rs](src/app.rs))
  - `space_unread_counts_subspaces_muted_shared_other_indicator_and_palette` ([src/app.rs](src/app.rs))
  - `low_priority_section_order_toggles_and_mutual_exclusion` ([src/app/rooms.rs](src/app/rooms.rs))
  - `spaces_and_filter_entries_reflect_state_and_counts` ([src/palette.rs](src/palette.rs))
  - Updated affected assertions in [src/menu.rs](src/menu.rs), [src/app/menu.rs](src/app/menu.rs), [src/app/media.rs](src/app/media.rs), [src/app/rooms.rs](src/app/rooms.rs), [src/ui/mod.rs](src/ui/mod.rs), and [src/app/keybinds.rs](src/app/keybinds.rs).
- **UI snapshot tests:**
  - `ui::tests::triage_sidebar_spaces_low_priority_and_filters` ([src/ui/mod.rs](src/ui/mod.rs))
  - `ui::tests::narrow_sidebar_quick_filters` ([src/ui/mod.rs](src/ui/mod.rs))
- **Continuwuity server tests (`#[ignore]`):**
  - `matrix::rooms::tests::detached_timeline_reports_new_live_messages` ([src/matrix/rooms.rs](src/matrix/rooms.rs))
  - `matrix::manage::tests::low_priority_tag_syncs_and_clears_favourite` ([src/matrix/manage.rs](src/matrix/manage.rs))

## Verification

- `./scripts/check.sh` (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build`) exited `0`:
  - `cargo test`: `test result: ok. 546 passed; 0 failed; 26 ignored; 0 measured; 0 filtered out; finished in 8.54s`
  - `find src -name '*.snap.new'`: empty (zero unreviewed `.snap.new` files)
- `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=alicepass cargo test --locked matrix:: -- --ignored --test-threads=1` against local Continuwuity (`http://127.0.0.1:6167`):
  - Full rerun after all edits passed cleanly: `test result: ok. 25 passed; 0 failed; 0 ignored; 0 measured; 547 filtered out; finished in 127.10s` (both new server tests `detached_timeline_reports_new_live_messages` and `low_priority_tag_syncs_and_clears_favourite` passed; an earlier run timed out once in pre-existing `matrix::verify::tests::asking_again_works` before passing individually in `2.54s` and in the full 25/25 rerun).
