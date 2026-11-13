# Command Palette Handover (branch `agy/command-palette`)

- **Worktree:** `/home/tylerstyle/git/matrix-rat/.claude/worktrees/agy-palette`
- **Base commit:** `3a84490` (`main`)

## What Was Built

### 1. Entry Points (`Ctrl-P` & Status Button)
- **`src/keymap.rs`**: Bound `Ctrl-P` (`Context::Global`, `Action::OpenPalette`, `"Command palette"`), verified free of conflicts in `DEFAULT`. Pressing `Ctrl-P` again while the palette is open closes it.
- **`src/ui/status.rs`**: Added `Button::Palette` (`Ctrl-P palette`), drawn only into leftover horizontal slack after essential right-aligned buttons (`F1 help`, `F2 settings`, `F3 <theme>`), toasts, verification hints, and context key hints have been laid out (`used + palette_w <= right`). This keeps toasts, security alerts, narrow terminals, and existing narrow snapshots intact.

### 2. Model, Entry Sources & Fuzzy Ranking (`src/palette.rs`)
- **Action entries** from `keymap::DEFAULT` (using binding `description` and `label` as hint), deduplicating multi-bound actions (`Ctrl-K` `"Find a room"` with `"filter rooms"`/`"alt-r"` aliases instead of a duplicate `Alt-R` row; `"o"` `"Open link, picture or file"` with `"view picture"` alias; `"F1"` `"This help"`) and omitting cursor-stepping / raw-compose / self-open keys (`OpenPalette`, `FocusNext`, `FocusPrev`, `FocusDir`, `PageUp`, `PageDown`, `Back`, `SidebarUp`, `SidebarDown`, `SidebarOpen`, `SelectOlder`, `SelectNewer`, `Send`, `Newline`). Also includes contextual `Accept invite` / `Decline invite` in invite rooms and `Cancel reply` / `Cancel edit` during reply/edit compose.
- **Room switching**: `"Go to <room name>"` for every room in `RoomKind::SECTIONS` order, bypassing any active sidebar filter and storing the stable Matrix room id (`Target::Room(String)`).
- **Settings & Account**: `Mouse`, `Typing notices`, `Read receipts`, `Download folder`, `Key bindings`, `Encryption / Verify device`, and `Log out` (`Log out` is disabled when `matrix` is `None` / disconnected).
- **Themes**: `"Theme: <name>"` for each theme returned by `Theme::available(&self.themes_dir)`.
- **Availability**: Message-scoped actions (`Reply`, `Edit`, `React`, `Delete`, `Copy`, `Save`, `Open`) reflect live selection capabilities (`MessageCaps`). Disabled entries remain searchable and visible in the list (muted and un-runnable).
- **Ranking (`nucleo-matcher`)**:
  - Empty query orders enabled contextual actions first, then remaining enabled actions, rooms, settings, themes, and disabled entries last.
  - Non-empty query matches `subject`, `title`, `hint` (substring only, preventing spurious shortcut-letter matches), and explicit per-entry aliases separately without cross-field concatenation noise. Exact/prefix/word-prefix/substring tiers are combined with `nucleo_matcher::Pattern` fuzzy scores, a room-name bonus (`+4_000`) when matching a room name, and an enabled tiebreaker (`+1_000`).

### 3. Execution & Modal Routing (`src/action.rs`, `src/modal.rs`, `src/app.rs`, `src/app/palette.rs`)
- Appended enum variants at the end of `Action` (`OpenPalette`, `OpenRoom(usize)`, `SelectTheme(usize)`, `Setting(crate::modal::Setting)`), `Modal` (`Palette`), `Button` (`Palette`), and `HitId` (`Palette(usize)`) to minimize merge conflicts with parallel branches.
- All execution clears `self.modal = None` first (so follow-up modals such as `Emoji`, `Settings`, `DownloadDir` edit, `Verify`, or `Logout` confirmation are not blocked by `self.modal.is_none()` guards) and routes through `App::perform(Action)`.
- `Target::Room(id)` resolves the current index in `App::rooms` at activation time so async `Rooms` sync updates while the palette is open cannot target a shifted index; `open_room` preserves per-room unsent drafts.
- `Action::Setting(Setting::Logout)` opens `Modal::Settings` on `Setting::Logout` with `confirm_logout: true` instead of logging out immediately.
- `on_modal_key`, `on_paste`, and `on_mouse` handle typing, `Backspace`, `Ctrl-U`, `Up`/`Down`/`PageUp`/`PageDown`/`Alt-P`/`Ctrl-N`, mouse click, wheel scrolling, and backdrop click dismissal.

### 4. Rendering & Snapshots (`src/ui/palette.rs`, `src/ui/modal.rs`, `src/ui/mod.rs`)
- Draws a centered rounded modal (`68x18` clamped to terminal size via `centered` and `popup`) with search bar (`tail_fit` keeping the cursor visible by grapheme width), separator with match count, category column (`Action`/`Room`/`Setting`/`Theme`, dropped cleanly below 36 cols), right-aligned key/status hints, muted disabled styling, and `tiny_sizes_do_not_panic` coverage.
- Added snapshots `command_palette`, `command_palette_narrow`, and `command_palette_filtered`; updated `help_overlay` (`Ctrl-P Command palette`) and 100-col full-screen snapshots where `Ctrl-P palette` appears in spare status slack.

## Verification
- Verified after final edit with `scripts/check.sh` (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build`).
- Not run: manual interactive PTY smoke test or `--ignored` homeserver integration tests.
