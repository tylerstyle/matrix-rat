# Dark/Light Theme Pairing Handover (branch `agy/theme-pairing`)

- **Worktree:** `/home/tylerstyle/git/matrix-rat/.claude/worktrees/agy-palette`
- **Merged `main`:** `4d7c58b` (via `2e484c2`)
- **Initial commits:** `46f8337`, `37444e3`
- **Review rework commit:** `017e18c`

## Summary of Rework After Review (`37444e3` -> `017e18c`)

### 1. Restored `crossterm::event::EventStream` & Removed Mode 2031 Input Bypass (Finding 1)
- Removed `src/terminput.rs` (`mod terminput;` dropped from `src/main.rs`).
- Removed `\x1b[?2031h` from `terminal::write_activation()`, `\x1b[?2031l` from `terminal::restore()`, and `terminal::query_color_scheme()` in `src/terminal.rs`.
- Restored `crossterm::event::EventStream` + `futures::StreamExt` in `src/app.rs` (`app::run`), along with the original `suspend_requested` path (`terminal::suspend(&mut terminal)?`).

### 2. XDG Desktop Portal Watching via `zbus` (Finding 2)
- Added `zbus = { version = "6.3.0", default-features = false, features = ["default-rt", "proxy"] }` to `Cargo.toml`. This matches the exact `zbus` version and feature set already pulled in by `notify-rust`, so `Cargo.lock` only gained a direct dependency edge on `zbus` and compiled no new crates or features.
- Added `src/portal.rs` (`mod portal;` in `src/main.rs`):
  - `SchemeWatch::start()` connects to the session D-Bus (`org.freedesktop.portal.Desktop`, `/org/freedesktop/portal/desktop`, `org.freedesktop.portal.Settings`) bounded by `START_TIMEOUT` (`300ms`).
  - Subscribes to `SettingChanged` with match arguments `&[(0, "org.freedesktop.appearance"), (1, "color-scheme")]` **before** reading the initial setting so transitions between subscribe and initial read are not missed.
  - Reads initial `color-scheme` via `ReadOne` (falling back to Settings v1 `Read` with nested `Variant` unwrapping on `UnknownMethod`), mapping `1` -> `ColorScheme::Dark`, `2` -> `ColorScheme::Light`, and `0` / unknown / `NotFound` -> `None`.
  - In SSH sessions (`SSH_CONNECTION`, `SSH_CLIENT`, or `SSH_TTY` non-empty), when no session bus is running, or when the portal service/interface is unavailable, live following silently stays inactive (`SchemeWatch::changed()` sleeps via `std::future::pending()` without waking `tokio::select!`).
  - Background signal task sends decoded changes over `tokio::sync::mpsc::unbounded_channel`, coalesces bursts in `SchemeWatch::changed()`, and aborts in `SchemeWatch::drop()`.
  - Startup precedence (`portal::resolve_initial_scheme`): portal preference (`1`/`2`) -> terminal startup reply (`CSI ?997;1/2n` then `OSC 11`) -> `COLORFGBG` -> `ColorScheme::Dark`.
  - `app::run` feeds `scheme = scheme_watch.changed() => app.on_color_scheme(scheme)` directly into the `tokio::select!` loop.

### 3. Startup Query Batching & Late-Reply Behaviour (Finding 3)
- Added `termquery::GRAPHICS_QUERY` and `termquery::ask_graphics(timeout)` alongside `QUERY` and `APPEARANCE_QUERY` in `src/termquery.rs` (`query_appearance` is `#[cfg(test)]`).
- `Graphics::detect_with_term(mode: ImageMode, probe_appearance: bool)` in `src/images.rs` skips `CSI ?996n` and `OSC 11` whenever `probe_appearance` is `false` (when `portal_scheme` already resolved `Dark` or `Light`). When `probe_graphics` is also false (`ImageMode::Off`, `ImageMode::Halfblocks`, multiplexer, or broken placeholders), no tty query is written at all.
- When `probe_appearance` is `true`, `\x1b[?996n` and `\x1b]11;?\x1b\\` are written strictly in the single startup batch ahead of the `\x1b[c` (DA1) sentinel inside `guard.alternate_screen()?`, before `guard.activate()` and `EventStream::new()`, and never afterwards or on `Ctrl-Z` resume.
- **Late-reply behaviour:** If a slow or broken terminal replies after `QUERY_TIMEOUT` (`1000 ms`), the delayed bytes enter `crossterm 0.29`'s `EventStream`: a late `OSC 11` reply is mis-parsed as `Alt+]` followed by plain characters, and a late `CSI ?997;1/2n` causes `crossterm 0.29` (`parse.rs`) to return `Ok(None)` without clearing its internal buffer, stalling subsequent input (the same pre-existing tradeoff as a late kitty/`CSI c` reply on `main`). Skipping appearance probes whenever the portal already provides a preference avoids sending them on normal desktop sessions, and responsive terminals complete synchronously before `CSI c` returns.

### 4. `Follow dark/light` Progressive Disclosure & Pairing Logic (Finding 4)
- Appended `Setting::ThemePairing` (`"Follow dark/light"`) at the end of `enum Setting` in `src/modal.rs` and placed it immediately below `Setting::Theme` in `Setting::ALL` (`[Setting; 15]`).
- `SettingsMenu` carries `show_theme_pair: bool`, `for_pairing(bool)`, `select(Setting, bool)`, and `shows(Setting)`, and skips hidden `ThemeDark`/`ThemeLight` rows in `current()` / `step(delta)`.
- `src/ui/modal.rs` filters `Setting::ALL` by `menu.shows(*s)` for layout height, headings, cursor mapping, and click rows:
  - By default (when unpaired), Settings renders 13 rows (`Theme`, `Follow dark/light ○ off`, `Room list width`, ...), keeping `Dark theme` and `Light theme` hidden.
  - Enabling `Follow dark/light` (or configuring `theme_dark` / `theme_light` in `config.toml`) reveals `Dark theme` and `Light theme`.
  - Turning `Follow dark/light` off clears `theme_dark` / `theme_light` from state and `config.toml` (`remove_setting`), restores the base `theme`, and collapses the two slot rows.
- Updated `perform_setting` (`src/app/palette.rs`: toggling `Setting::ThemePairing` off when paired or opening Settings on `Dark theme` with `show_theme_pair: true`), `HitId::Setting` handling in `src/app.rs`, `Action::OpenSettings`, `README.md` (`## Themes`), `app::files::tests::download_folder_is_typed_in_the_settings`, `app::tests::settings_menu_by_keyboard_and_mouse`, and the two Settings UI snapshots.

## Existing Pairing & Startup Behaviour Preserved
- `ColorScheme::{Dark, Light}` in `src/theme.rs`.
- Config keys `theme_dark` / `theme_light`, `remove_setting`, and `paired_themes()` in `src/config.rs`.
- `App::on_color_scheme`, `App::has_theme_pair`, `App::desired_theme`, `App::step_slot_theme`, `F3` / `Theme` slot updates when paired, and leaving unpaired themes (including `"terminal"` / generators) untouched on live changes.
- Command palette entries and theme hints in `src/palette.rs`.

## Tests Added / Updated
- `portal::tests::portal_values_and_variants_map_cleanly`
- `portal::tests::setting_changed_signal_decodes_real_dbus_message` (real `zbus::Message::signal` decode)
- `portal::tests::initial_scheme_precedence_prefers_portal_then_terminal_then_default`
- `portal::tests::ssh_detection_checks_standard_env_vars`
- `termquery::tests::csi_996_and_osc_11_pty_replies_resolve_scheme`
- `termquery::tests::osc_11_alone_detects_dark_and_light_backgrounds`
- `termquery::tests::colorfgbg_fallback_maps_background_indices`
- `config::tests::paired_themes_resolve_per_scheme_and_remove_cleanly`
- `app::tests::paired_themes_follow_scheme_changes_and_f3_updates_active_slot`
- `app::tests::unpaired_theme_ignores_live_scheme_changes`
- `app::tests::settings_menu_by_keyboard_and_mouse`
- `app::files::tests::download_folder_is_typed_in_the_settings`
- Updated snapshots: `src/ui/snapshots/matrix_rat__ui__tests__settings_menu.snap` and `src/ui/snapshots/matrix_rat__ui__tests__settings_menu_low.snap`

## Verification & Scope Notes
- `./scripts/check.sh` passed cleanly:
  - `cargo fmt --check` — passed
  - `cargo clippy --all-targets -- -D warnings` — passed
  - `cargo test` — `411 passed; 0 failed; 16 ignored; 0 measured; 0 filtered out`
  - `cargo build` — passed
- End-to-end live D-Bus `org.freedesktop.portal.Settings` `SettingChanged` delivery against a running desktop portal was not exercised in this headless container (portal wire encoding/decoding, `Message::signal` parsing, precedence, and SSH suppression are covered by unit tests, and `App::on_color_scheme` / Settings flow are covered by app tests).
