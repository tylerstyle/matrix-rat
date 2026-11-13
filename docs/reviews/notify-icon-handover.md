# Desktop Notification Icons Handover (branch `agy/notify-icon`)

- **Worktree:** `/home/tylerstyle/git/matrix-rat/.claude/worktrees/agy-palette`
- **Branched from:** `f61e666` (`main`)
- **Implementation commit:** `4d283f0`

## What Was Built

### 1. Ephemeral Notification Icon Cache & Delivery (`src/paths.rs`, `src/notify.rs`)
- **Per-profile cache directory (`src/paths.rs`)**:
  - Added `Paths::notify_icons_dir(&self)` returning `runtime_dir.join(format!("{}-notify-icons", self.profile))`.
  - Protected both `data_dir` and `runtime_dir` with mode `0o700` in `Paths::create_dirs()`.
- **Bounded, symlink-safe PNG cache (`IconCache` in `src/notify.rs`)**:
  - Creates cache directories with mode `0o700` via `ensure_safe_dir`, refusing symlinks or non-directory targets (`symlink_metadata`).
  - Writes PNGs using hashed 128-bit FNV-1a filenames (`cache_filename`, leaking no room names or `mxc://` URLs in paths) atomically via `O_CREAT | O_EXCL` (`create_new(true)`) `.tmp` files with mode `0o600` followed by `rename`.
  - Bounds cached files with an LRU `VecDeque` (`DEFAULT_CACHE_LIMIT = 64`), deleting evicted files from disk.
  - Tracks a monotonic `epoch()` counter incremented by `IconCache::clear()`; `ensure_file` validates `expected_epoch` under the mutex so stale queued desktop jobs cannot recreate files after logout or exit, and `clear()` deletes files and the directory without traversing symlinks.
- **Desktop notification attachment (`src/notify.rs`)**:
  - `NotifyIcon::{Generic, Tile { ch, bg, fg }, Picture { key, pixels }}` and `show(mode, notice, preview, icon, cache)`.
  - Uses `notify_rust::Notification::image_path` (available directly with the existing `features = ["z"]`, no extra dependencies) in `desktop()` when `prepare_desktop` resolves `Some(path)`, keeping `.icon("mail-message-new")` fallback and logging/ignoring any icon or D-Bus error so notifications are never dropped or blocked.
  - When `preview = false` or resolved mode is `NotifyMode::Terminal`, `prepare_desktop` / `show` never create or write to `IconCache`.

### 2. Built-in `96×96` Colored Initial Tile Renderer (`src/notify.rs`)
- **`tile_char(room_name: &str)`**: strips leading ASCII punctuation/whitespace identically to `ui::sidebar::initial`, uppercases ASCII `'A'..='Z' | '0'..='9'`, and falls back to `'?'` for empty, punctuation-only, symbol/emoji, or non-Latin names.
- **`tile_colors(theme, room_name)`**: derives background from `Theme::user_color(room_name)` (handling `Color::Rgb`, `Color::Indexed` xterm-256/ANSI16, and named ANSI terminal colors via `color_to_rgb`) and uses `Theme::style(Role::UnreadBadge).fg` when WCAG contrast ratio against background is `>= 3.0`, otherwise picking near-black or near-white contrast.
- **`render_tile_image(ch, bg, fg)`**: renders crisp `96×96` RGB PNGs using a built-in `5×7` bitmap alphabet (`A`–`Z`, `0`–`9`, `?`) scaled cleanly to `10×10` pixels centered in the tile.

### 3. Non-Blocking Avatar Lookup, Invite Guard & Session Cleanup (`src/images.rs`, `src/app/media.rs`, `src/app.rs`)
- **`ImageStore::request_avatar` (`src/images.rs`)**: refactored avatar queuing into shared `queue_avatar_if_needed`, enabling avatar pixel fetching (`MatrixCmd::FetchAvatar` -> `MatrixEvent::Avatar`) even on non-kitty terminals and returning cached `96×96` `Pixels` immediately without blocking.
- **`App::resolve_notice_icon` / `App::take_notices_with_icons` / `ReadyNotices` (`src/app.rs`, `src/app/media.rs`)**:
  - Returns `NotifyIcon::Generic` when `!notification_preview` or `notify_mode.resolved() != NotifyMode::Desktop` without fetching avatars or writing cache files.
  - Never calls `request_avatar` for invitations (`RoomKind::Invite`), returning `NotifyIcon::Tile` only.
  - For joined/direct rooms with a cached avatar, returns `NotifyIcon::Picture`; on cache miss, emits `NotifyIcon::Tile` immediately while calling `send_avatar_requests()` (`src/app/media.rs`) so subsequent notices use the fetched picture.
  - Wipes `IconCache` on startup (`IconCache::new`), on logout / `NeedLogin` (`App::reset_session()`), and on every exit/signal/error return in `app::run()` via an RAII `NotifyCacheGuard`.

## Tests Added
- `paths::tests::paths_are_per_profile`: asserts `notify_icons_dir()` isolation per profile.
- `images::tests::request_avatar_fetches_pixels_even_without_kitty`: verifies non-kitty queuing and pixel retrieval.
- `notify::tests::tile_chars_and_bitmap_patterns` (+ snapshot `src/snapshots/matrix_rat__notify__tests__tile_bitmap_patterns.snap`): tests initial extraction, non-Latin/emoji/punctuation fallbacks, PNG encoding/decoding, dimensions, corners, and bitmap patterns.
- `notify::tests::tile_colors_maintain_readability_across_builtin_themes`: checks `contrast_ratio >= 3.0` across every built-in theme.
- `notify::tests::icon_cache_permissions_lru_eviction_and_cleanup`: verifies `0o700` dir, `0o600` PNG files, LRU eviction order, `clear()` file/dir removal, and stale-epoch rejection.
- `notify::tests::preview_off_terminal_and_cache_failures_do_not_break_notifications`: verifies `preview = false`, `NotifyMode::Terminal`, and symlink cache-dir failure still produce sendable notifications without touching the cache or outside paths.
- `app::tests::desktop_notification_icons_tile_fetch_upgrade_invite_and_logout_wipe` and `app::tests::hidden_preview_and_terminal_mode_skip_icon_fetch_and_cache`: test immediate tile + `FetchAvatar` upgrade to `Picture`, invite no-fetch, preview-off/terminal skip, and `reset_session()` cache wipe.

## Verification
- `./scripts/check.sh` on commit `4d283f0`:
  - `cargo fmt --check` — passed
  - `cargo clippy --all-targets -- -D warnings` — passed
  - `cargo test` — passed (`351 passed; 0 failed; 15 ignored; 0 measured; 0 filtered out`)
  - `cargo build` — passed
