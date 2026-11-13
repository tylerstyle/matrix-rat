# Configurable Key Bindings Handover (branch `agy/keybinds`)

- **Worktree:** `/home/tylerstyle/git/matrix-rat/.claude/worktrees/agy-palette`
- **Branched from:** `de5a77c` (`main`)
- **Implementation commit:** `12ca8a4`

## What Was Built

### 1. Action Table, Keymap Model & Vim Preset (`src/action.rs`, `src/keymap.rs`)
- **Single source of truth (`action_table!` / `ACTIONS` / `action_spec` / `action_by_name`)**:
  - Derives every user-bindable action's canonical `snake_case` config name, default `Context`, and human description directly from `Action`.
  - Appended `Action::{JumpToOldest, SidebarTop, SidebarBottom}` at the end of `Action` (`src/action.rs`) so timeline oldest jump and sidebar top/bottom jumps are first-class bindable actions (`jump_to_oldest`, `sidebar_top`, `sidebar_bottom`).
- **Four contexts (`Context`)**:
  - `Global` (`Everywhere` / `[keys.global]`), `Sidebar` (`Room list` / `[keys.sidebar]`), `Timeline` (`Messages` / `[keys.timeline]`), and `Composer` (`Composer` / `[keys.composer]`).
  - Pane contexts (`Sidebar`, `Timeline`, `Composer`) shadow `Global` on lookup (`Keymap::lookup`). In `Context::Composer`, plain printable characters (`mods.is_empty()` with `KeyCode::Char(_)`) always return `None` from `Keymap::lookup` so typing cannot be swallowed by global or preset single-letter bindings.
  - Added `Keymap::lookup_popup_global` for global shortcuts that remain active behind popups (such as `Quit` and `Suspend`) and dynamic viewer keys (`App::on_viewer_key`).
- **Key syntax & terminal safety (`parse_key`, `from_key_event`, `validate_binding`, `format_label`, `format_config_key`, `normalize`)**:
  - Supports case-insensitive modifiers (`ctrl-`, `alt-`, `shift-`), named keys (`enter`/`return`, `esc`/`escape`, `tab`, `backtab`, `space`, `backspace`/`bs`, `del`/`delete`, `ins`/`insert`, `home`, `end`, `pgup`/`pageup`, `pgdn`/`pagedown`, `up`, `down`, `left`, `right`, `f1`–`f12`), and single non-control characters (`e`, `R`, `/`, `?`).
  - Rejects terminal-colliding or reserved combinations: `ctrl-i`, `ctrl-m`, `ctrl-[`, `ctrl-c`, explicit `shift-` on characters (`"R"` should be used instead of `"shift-r"`), and `shift-` combined with `ctrl-`/`alt-` on characters.
- **`Preset::{Default, Vim}`**:
  - `Preset::Default` preserves all existing default bindings; `Preset::Vim` keeps defaults and adds `j`/`k`/`g`/`G`/pane-switch/`/`/composer-`Esc` bindings (`VIM_EXTRA`) cleanly.
- **Declaration-order overrides, conflict resolution & `Quit` safety rail (`Keymap::from_config`, `Keymap::from_raw`, `ConflictLog`)**:
  - Loads `preset` (supporting either `[keys] preset = ...` or top-level `preset = ...`) and applies `[keys]` entries (routed to `spec.default_context`) followed by `[keys.<context>]` sections in TOML declaration order (`[]` or `"none"` unbinds).
  - Unknown sections, unknown actions, invalid key syntax, reserved combos, and within-context conflicts record warnings without aborting startup (last valid entry wins). Actions explicitly reassigned or unbound later in the same config do not emit spurious conflict warnings when an earlier line reuses their old preset key.
  - If all `Action::Quit` bindings are removed, `enforce_quit_rail` restores `Ctrl-Q` in `Global` and appends a warning.

### 2. TOML Config Loading & Comment-Preserving Persistence (`src/config.rs`, `src/keymap.rs`)
- **`RawKeyValue`, `RawKeyConfig`, `KeyOverrideEdit`, `Config::parse`, `Config::raw_key_config`**:
  - Preserves exact TOML declaration order (from `toml_edit::DocumentMut` on disk or ordered fallback for `Config::parse`) so declaration-order last-one-wins semantics match the file.
- **`save_key_changes` & `persist_overrides`**:
  - Uses `toml_edit` (`edit_document` shared with `save_setting`) and updates existing entries in place via `get_mut` (`set_value_preserving_decor`) to keep existing section order, key prefix comments, and inline trailing comments intact while writing `"none"` or strings/arrays cleanly and clearing overrides on `Reset all`.

### 3. Settings Editor UI, Modal Flow & Documentation (`src/modal.rs`, `src/ui/hit.rs`, `src/ui/keybinds.rs`, `src/app/keybinds.rs`, `README.md`)
- **`Setting::Keys` & `Modal::Keybinds(Editor)` (`src/modal.rs`)**:
  - Appended `Modal::Keybinds` at the end of `Modal` and `HitId::{KeybindRow, KeybindBtn}` at the end of `HitId` (`src/ui/hit.rs`). `F1` remains bound to Help; Settings (`F2`) opens **Key bindings** (`app.keymap.preset_summary()`).
- **Interactive Key Bindings Editor (`src/ui/keybinds.rs`, `src/app/keybinds.rs`)**:
  - Renders preset switcher (`◂ default/vim ▸`), scrollable context-grouped action list (`Editor::follow_selection` keeps keyboard navigation visible without snapping back against mouse wheel `scroll_by`), modified `*` markers, effective keys / `—` for unbound actions, config key hint bar, and action buttons (`[ Rebind ]`, `[ Unbind ]`, `[ Reset ]`, `[ Reset all ]`, `[ Close ]`).
  - Supports key capture (`EditorStage::Capture`, keeping `Ctrl-C` and sole `Quit` key safe inside capture instead of cancelling compose or quitting the app), within-context overwrite confirmation prompt (`EditorStage::Conflict` with `[ Replace ]` / `[ Cancel ]`, `y`/`Enter` and `n`/`Esc`), single-action `Reset`, `Reset all`, clicking/wheel scrolling, and immediate saving via `persist_overrides`.
- **Dynamic Key Labels Across UI & Tests (`src/ui/help.rs`, `src/ui/status.rs`, `src/ui/composer.rs`, `src/ui/modal.rs`, `src/palette.rs`, `src/app/palette.rs`, `src/app/files.rs`, `src/app/media.rs`, `src/app.rs`, `src/ui/mod.rs`)**:
  - Help overlay, status bar buttons/pane hints/palette button, composer placeholders, Ctrl-C quit toast, LoginForm footer, large-file retry toast, Command Palette hints (`PaletteContext.keymap`), and image viewer keys all reflect the effective `Keymap`.
  - Documented `[keys]`, key syntax/limits, `preset = "vim"`, and the full action table in `README.md`.
  - Extended `tiny_sizes_do_not_panic` and `every_key_in_the_help_can_be_reached`, updated `settings_menu` snapshots, and added `keybinds_editor`, `keybinds_conflict`, and `keybinds_vim_help` UI snapshots (`keybinds_editor_and_vim_help`) plus app integration tests (`startup_warnings_and_dynamic_key_labels_everywhere`, `vim_preset_navigation_and_composer_precedence`, `keybinds_editor_capture_conflict_quit_rail_mouse_and_persistence`), `src/config.rs::key_config_preserves_order_and_comments_on_save`, and `src/keymap.rs` unit tests.

## Verification
- `./scripts/check.sh` on implementation commit `12ca8a4`:
  - `cargo fmt --check` — passed
  - `cargo clippy --all-targets -- -D warnings` — passed
  - `cargo test` — passed (`343 passed; 0 failed; 14 ignored; 0 measured; 0 filtered out`)
  - `cargo build` — passed
