# Dark/Light Theme Pairing Handover (branch `agy/theme-pairing`)

- **Worktree:** `/home/tylerstyle/git/matrix-rat/.claude/worktrees/agy-palette`
- **Branched from:** `4f23c15` (`main`)
- **Implementation commit:** `46f8337`

## What Was Built

### 1. Startup Colour-Scheme Query & `COLORFGBG` Fallback (`src/theme.rs`, `src/termquery.rs`, `src/images.rs`)
- Added `ColorScheme::{Dark, Light}` (`#[default] Dark`) in `src/theme.rs`.
- Extended `src/termquery.rs` `QUERY` with `\x1b[?996n` (DEC mode 2031 initial scheme query) and `\x1b]11;?\x1b\\` (OSC 11 background luminance query) ahead of `\x1b[c`, plus `APPEARANCE_QUERY` when graphics probing is disabled.
- Extended `Reply` with `reported_scheme: Option<ColorScheme>` and `background_scheme: Option<ColorScheme>`, resolved via `Reply::scheme()` (preferring explicit `CSI ?997;1/2n` over `OSC 11` luminance) and `resolve_color_scheme(reply)` (falling back to `env_color_scheme` / `COLORFGBG`, then `ColorScheme::Dark`).
- Added `parse_color_scheme_csi`, `parse_osc11_payload` (1/2/3/4-hex `rgb:` / `rgba:` and `#rrggbb`), `scheme_from_colorfgbg` (0–15 ANSI + 256-colour cube & greyscale), `ask_appearance`, and `query_appearance` (sharing `query_bytes` with `query`).
- Replaced `Graphics::detect` with `Graphics::detect_with_term(mode: ImageMode) -> (Self, Option<termquery::Reply>)` in `src/images.rs`, probing appearance via `termquery::ask_appearance` even under `ImageMode::Off` / `Halfblocks` / multiplexers.

### 2. DEC Mode 2031 Live Notifications & Crossterm 0.29 Bypass (`src/terminal.rs`, `src/terminput.rs`, `src/main.rs`)
- Updated `terminal::write_activation()` to enable DEC mode 2031 (`\x1b[?2031h`), `terminal::restore()` to disable it (`\x1b[?2031l`), and added `terminal::query_color_scheme()` (`\x1b[?996n`) for resume after `Ctrl-Z`.
- Inspected `crossterm 0.29.0` (`parse.rs`): its Unix parser returns `Ok(None)` without clearing its internal buffer when receiving unknown `CSI ? ...` reports ending in anything other than `u`/`c` (including `CSI ?997;1n` / `CSI ?997;2n`), which wedges subsequent keyboard input.
- Added `src/terminput.rs` (wired via `mod terminput;` in `src/main.rs`): an async Unix tty reader (`TermEvents`, `TermInput::{Event, ColorScheme}`, `TermParser`) with zero new crate dependencies that:
  - Emits `crossterm::event::Event` for keys (including kitty `CSI u` and `1;<mod>R` F3 disambiguation), SGR/X10/rxvt mouse, bracketed paste, UTF-8, and timeout-resolved `Esc` / `Alt+[` / `Alt+]`.
  - Parses live `CSI ?997;1/2n` (`Dark`/`Light`) and `OSC 11` reports into `TermInput::ColorScheme` and safely discards unknown CSI/OSC/APC sequences without wedging.
  - Stops its background thread cleanly via wake pipe + `join()` on `Drop`.

### 3. Configuration, Pairing & UI Integration (`src/config.rs`, `src/modal.rs`, `src/ui/modal.rs`, `src/palette.rs`, `src/app/palette.rs`, `src/app.rs`, `README.md`)
- **`Config` (`src/config.rs`)**: added `theme_dark: Option<String>` and `theme_light: Option<String>` (empty/whitespace falls back via `paired_themes()` and `theme_for`), plus `remove_setting(path, key)` to remove keys cleanly when returned to `(unset)`.
- **Settings UI (`src/modal.rs`, `src/ui/modal.rs`, snapshots)**: appended `Setting::ThemeDark` (`"Dark theme"`) and `Setting::ThemeLight` (`"Light theme"`) at the end of `enum Setting`, inserted them into `Setting::ALL` right after `Setting::Theme`, updated `hint`, and rendered `◂ value ▸` rows cleanly (with `SettingLine` scrolling on smaller viewports).
- **Command Palette (`src/palette.rs`, `src/app/palette.rs`)**: added `theme_dark` / `theme_light` to `PaletteContext`, updated theme hints, and wired `Setting::ThemeDark` / `Setting::ThemeLight` in `perform_setting`.
- **`App` & event loop (`src/app.rs`)**:
  - Added `App::new_in_scheme` (keeping `App::new` for tests defaulting to `ColorScheme::Dark`), `on_color_scheme`, `has_theme_pair`, `desired_theme`, `step_slot_theme`, and `remove_setting`.
  - When paired (`theme_dark.is_some() || theme_light.is_some()`), `on_color_scheme` switches to the scheme's configured slot (falling back to `theme` if only one slot is set), and `apply_theme` (`F3`, `Theme` setting row, or Palette selection) updates only the active mode's slot (`theme_dark` or `theme_light`).
  - When unpaired, `"terminal"` and generator themes (e.g. awaiting an external file write) ignore live colour-scheme notifications and `F3` continues writing `theme`.
  - `reload_theme` tracks config `theme`, `theme_dark`, and `theme_light` changes from external edits while preserving `wanted_theme` on self-writes.
  - `app::run` uses `Graphics::detect_with_term`, `resolve_color_scheme`, `TermEvents::spawn()`, `SignalKind::window_change()` (`Event::Resize`), and drops/recreates `TermEvents` plus calls `terminal::query_color_scheme()` across `terminal::suspend()`.
- **Docs (`README.md`)**: documented `theme_dark`, `theme_light`, terminal scheme detection/live following, and `F3` slot behavior.

## Tests Added / Updated
- `termquery::tests::csi_996_and_osc_11_pty_replies_resolve_scheme` (real `openpty` handshake verifying query bytes + `CSI ?997;1/2n` preference over `OSC 11`)
- `termquery::tests::osc_11_alone_detects_dark_and_light_backgrounds`
- `termquery::tests::colorfgbg_fallback_maps_background_indices`
- `terminput::tests::parses_live_color_scheme_notifications_without_wedging_subsequent_keys`
- `terminput::tests::parses_split_color_scheme_and_osc11_reports`
- `terminput::tests::parses_keys_kitty_csi_u_mouse_and_paste`
- `terminput::tests::pty_live_color_scheme_stream_receives_changes_and_keys` (real `openpty` async stream test)
- `config::tests::paired_themes_resolve_per_scheme_and_remove_cleanly`
- `app::tests::paired_themes_follow_scheme_changes_and_f3_updates_active_slot`
- `app::tests::unpaired_theme_ignores_live_scheme_changes`
- `app::tests::settings_menu_by_keyboard_and_mouse` (updated for `ThemeDark` / `ThemeLight` rows and `(unset)` removal)
- Updated UI snapshots: `src/ui/snapshots/matrix_rat__ui__tests__settings_menu.snap` and `src/ui/snapshots/matrix_rat__ui__tests__settings_menu_low.snap`

## Verification & Scope Notes
- `./scripts/check.sh` passes cleanly after committing implementation and handover docs (see latest run):
  - `cargo fmt --check` — passed
  - `cargo clippy --all-targets -- -D warnings` — passed
  - `cargo test` — `401 passed; 0 failed; 16 ignored; 0 measured; 0 filtered out`
  - `cargo build` — passed
- Interactive live theme switching in a real kitty/terminal window was not run in this headless session (covered instead via PTY and parser tests).
