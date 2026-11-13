# Command Palette Handover (branch `agy/command-palette`)

- **Worktree:** `/home/tylerstyle/git/matrix-rat/.claude/worktrees/agy-palette`
- **Branched from:** `3a84490` (`main`); **merged `main` at:** `6a626b8`

## What Was Built

### 1. Entry Points (`Ctrl-P`, Status Button & Open Guards)
- **`src/keymap.rs`**: Bound `Ctrl-P` (`Context::Global`, `Action::OpenPalette`, `"Command palette"`), verified free of conflicts in `DEFAULT`. Pressing `Ctrl-P` again while the palette is open closes it.
- **`src/ui/status.rs`**: Added `Button::Palette` (`Ctrl-P palette`), drawn only into leftover horizontal slack after essential right-aligned buttons (`F1 help`, `F2 settings`, `F3 <theme>`), toasts, verification hints, and context key hints have been laid out (`used + palette_w <= right`). Confirmed post-merge in 100-col (`main_screen`, `message_selected_and_reply`, `thread_view`), `70x14` (`chips`), and `46x16` (`command_palette_narrow`) snapshots: existing thread/attach/room hints are never crowded or dropped.
- **Open guards (`src/app/palette.rs`, `src/app.rs`)**: `open_palette()` returns early if `self.modal.is_some() || self.images.viewer_open()`. Neither `Ctrl-P` nor `Action::OpenPalette` can open the palette over the full-window image viewer or over `Login`, `Recovery`, or `Verify` popups (and `Ctrl-P` never inserts `'p'` into their fields).

### 2. Model, Entry Sources & Fuzzy Ranking (`src/palette.rs`)
- **Action entries** from `keymap::DEFAULT` (using binding `description` and `label` as hint), deduplicating multi-bound actions (`Ctrl-K` `"Find a room"` with `"filter rooms"`/`"alt-r"` aliases instead of a duplicate `Alt-R` row; `"o"` `"Open link, picture or file"` with `"view picture"` alias; `"F1"` `"This help"`) and omitting cursor-stepping / room-stepping / raw-compose / self-open keys (`OpenPalette`, `FocusNext`, `FocusPrev`, `FocusDir`, `PageUp`, `PageDown`, `Back`, `SidebarUp`, `SidebarDown`, `SidebarOpen`, `SelectOlder`, `SelectNewer`, `RoomPrev`, `RoomNext`, `Send`, `Newline`).
- **Contextual & thread/file actions**: Includes `Accept invite` / `Decline invite` in invite rooms, `Cancel reply` / `Cancel edit` during reply/edit compose, `Open the message's thread` (`t`), `Close thread` (`Action::CloseThread`, `Esc`), `Attach a file` (`Alt-A`), and `Save file or picture` (`s`).
- **Room switching**: `"Go to <room name>"` for every room in `RoomKind::SECTIONS` order, bypassing any active sidebar filter and storing the stable Matrix room id (`Target::Room(String)`). Jumping from an open thread leaves the thread cleanly via `open_room` -> `leave_thread()` without leaking thread drafts into the target room.
- **Settings & Account**: `Mouse`, `Typing notices`, `Read receipts`, `Notifications`, `Download folder`, `Key bindings`, `Encryption / Verify device`, and `Log out` (`Log out` is disabled when `matrix` is `None` / disconnected).
- **Themes**: `"Theme: <name>"` for each theme returned by `Theme::available(&self.themes_dir)`.
- **Availability**:
  - `OpenThread`: enabled when not already inside a thread (`!thread_open`), not in an invite room, and the selected non-system/non-read-marker message has a server event id (or in demo mode).
  - `CloseThread`: enabled only while a thread view is open (`ctx.thread_open`).
  - `Attach`: enabled when a non-invite room is open and not editing (`!matches!(compose_mode, ComposeMode::Edit(_))`).
  - `Save`: enabled only when `App::can_save()` is true.
  - `Reply`, `Edit`, `React`, `Delete`, `Copy`, `Open`: reflect live message selection capabilities (`MessageCaps`). Disabled entries remain searchable and visible in the list (muted and un-runnable).
- **Ranking (`nucleo-matcher`)**:
  - Empty query orders enabled contextual actions (and `Close thread` when in a thread) first, then remaining enabled actions, rooms, settings, themes, and disabled entries last.
  - Non-empty query matches `subject`, `title`, `hint` (substring only, preventing spurious shortcut-letter matches), and explicit per-entry aliases separately without cross-field concatenation noise. Exact/prefix/word-prefix/substring tiers are combined with `nucleo_matcher::Pattern` fuzzy scores, a room-name bonus (`+4_000`) when matching a room name, and an enabled tiebreaker (`+1_000`).

### 3. Execution & Modal Routing (`src/action.rs`, `src/modal.rs`, `src/app.rs`, `src/app/palette.rs`)
- Appended enum variants at the end of `Action` (`OpenPalette`, `OpenRoom(usize)`, `SelectTheme(usize)`, `Setting(crate::modal::Setting)`), `Modal` (`Palette`), `Button` (`Palette`), and `HitId` (`Palette(usize)`) to minimize merge conflicts.
- **Why `OpenRoom(usize)` and `SelectTheme(usize)` are `Action` variants**: Every palette execution funnels through `App::perform(Action)`, preserving `action`'s single dispatch path across keyboard, mouse, and tests. Both variants are omitted from `keymap::DEFAULT` (never in help) and never emitted as status hints, and out-of-range indices are harmless (`open_room` bounds-checks `self.rooms.len()`, `SelectTheme` uses `self.theme_names.get(index)`).
- All execution clears `self.modal = None` first (so follow-up modals such as `Emoji`, `Settings`, `DownloadDir` edit, `Verify`, or `Logout` confirmation are not blocked by `self.modal.is_none()` guards) and routes through `App::perform(Action)`.
- `Target::Room(id)` resolves the current index in `App::rooms` at activation time so async `Rooms` sync updates while the palette is open cannot target a shifted index; `open_room` preserves per-room unsent drafts and closes any open thread cleanly.
- `Action::Setting(Setting::Logout)` opens `Modal::Settings` on `Setting::Logout` with `confirm_logout: false` (unarmed), so the normal two-step Enter confirmation applies and a held/auto-repeated `Enter` from the palette cannot log out in one press.
- `on_modal_key`, `on_paste`, and `on_mouse` handle typing, `Backspace`, `Ctrl-U`, `Up`/`Down`/`PageUp`/`PageDown`/`Alt-P`/`Ctrl-N`, mouse click, wheel scrolling, and backdrop click dismissal.

### 4. Rendering & Snapshots (`src/ui/palette.rs`, `src/ui/modal.rs`, `src/ui/mod.rs`)
- Draws a centered rounded modal (`68x18` clamped to terminal size via `centered` and `popup`) with search bar (`tail_fit` keeping the cursor visible by grapheme width), separator with match count, category column (`Action`/`Room`/`Setting`/`Theme`, dropped cleanly below 36 cols), right-aligned key/status hints, muted disabled styling, and `tiny_sizes_do_not_panic` coverage.
- Added snapshots `command_palette`, `command_palette_narrow`, and `command_palette_filtered`; updated `help_overlay` (`Ctrl-P Command palette`) and affected full-screen snapshots where `Ctrl-P palette` appears in spare status slack.

## Review Fixes Applied After `3a84490` -> `6a626b8` Merge
1. Merged `main` (`6a626b8`) cleanly across source files and regenerated insta snapshots via `INSTA_UPDATE=always cargo test`.
2. Added `Setting::Notifications`, `Action::OpenThread`, `Action::CloseThread`, `Action::Attach`, verified `Action::Save` uses `App::can_save()`, and excluded `Action::RoomPrev`/`Action::RoomNext`.
3. Changed `Setting::Logout` palette action to open Settings on the Log out row with `confirm_logout: false`, with a two-Enter test verifying no `Logout` command fires on the first `Enter`.
4. Guarded `open_palette()` against `images.viewer_open()` and added tests for `viewer`, `Login`, `Recovery`, and `Verify`, plus jumping rooms from an open thread without leaking thread drafts.
5. Verified `OpenRoom(usize)` / `SelectTheme(usize)` never appear in help/hints and ignore out-of-range indices.
6. Re-checked 100-col snapshots (`main_screen`, `message_selected_and_reply`, `thread_view`, `command_palette`) to confirm `Ctrl-P palette` uses leftover slack only.

## Verification
- Verified with `scripts/check.sh` (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build`) after all edits and commits.
- Not run: manual interactive PTY smoke test or `--ignored` homeserver integration tests.
