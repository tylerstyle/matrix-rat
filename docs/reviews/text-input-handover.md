# One-Line Text Inputs Handover (branch `agy/text-input`)

- **Worktree:** `/home/tylerstyle/git/matrix-rat/.claude/worktrees/agy-palette`
- **Branched from:** `ab934cc` (`main`, `v0.1.0`)
- **Implementation commit:** `dce0ef0`

## Summary of Changes

### 1. Grapheme-Aware One-Line Editor (`src/lineedit.rs`)
- Added `LineEdit`, `EditKeys`, `EditOutcome`, and `LineView`:
  - Stores `text: String`, a grapheme-cluster-snapped byte `cursor`, optional `max_bytes` cap, `secret: bool` (`Debug` redacts as `"<redacted>"`), and `scroll_col` (viewport left offset in display columns).
  - Implements `Deref<Target = str>`, `From<&str>`, `From<String>`, `PartialEq<str / &str / String>`, and `Default`.
  - Editing & cursor navigation preserve whole extended grapheme clusters via `unicode_segmentation::UnicodeSegmentation` and display column width via `crate::text::width`:
    - `insert` / `paste`: sanitizes the first line through `crate::text::sanitize_line` (dropping C0/C1/bidi controls and normalizing line breaks before taking the first line) and stops before `max_bytes` at a whole grapheme boundary rather than byte/char truncation (`String::remove(0)`).
    - `backspace`, `delete`, `left`, `right`: step across whole grapheme clusters (`👍🏽`, `e\u{0301}`, ZWJ sequences).
    - `word_left` / `word_right` (`Ctrl-Left`/`Right`, `Alt-B`/`F`), `delete_word_left` (`Ctrl-W`, `Alt-Backspace`) via `unicode_word_indices()`.
    - `home` / `end` (`Home`/`End`, `Ctrl-A`/`E`), `clear` (`Ctrl-U`), `kill_to_end` (`Ctrl-K`).
    - `take_string(&mut self) -> String`: extracts owned secret strings on submit while preserving `secret` and `max_bytes` flags for failure/retry paths (used instead of `std::mem::take` on `LoginForm.password`, `RecoveryForm.key`, and `VerifyPopup.password`).
  - Rendering & mouse placement:
    - `view(&mut self, max_cols: usize, masked: bool) -> LineView`: computes a horizontal window fitting within `max_cols` display columns (treating each grapheme cluster as one `•` column when `masked == true`) and updates `scroll_col` so the cursor stays visible.
    - `place(&mut self, rel_col: usize, masked: bool) -> EditOutcome`: maps a click column relative to the rendered value area back to a grapheme boundary using the stored `scroll_col`.
  - `EditKeys { arrows, home_end, delete, kill_to_end }` (`EditKeys::FULL` default) lets popups disable shortcuts that collide with popup-level navigation or actions.

### 2. Converted One-Line Inputs (All 10 Inputs)
Confirmed `src/keymap.rs` / `src/app/keybinds.rs` keybindings editor has no text input, and kept `src/composer.rs` out of scope. Converted every one-line input in the app:
1. **`LoginForm`** (`homeserver`, `username`, `password` [secret + masked `•`]; `LoginForm::field_at_mut`) — `src/modal.rs`, `src/app.rs`, `src/ui/modal.rs`
2. **`RecoveryForm`** (`key` [secret, unmasked display matching existing UI]) — `src/modal.rs`, `src/app.rs`, `src/ui/modal.rs`
3. **`SettingsMenu.editing`** (`Setting::DownloadDir`) — `src/modal.rs`, `src/app.rs`, `src/ui/modal.rs`
4. **`EmojiPicker.query`** (`EmojiPicker::refilter`) — `src/modal.rs`, `src/app.rs`, `src/ui/modal.rs`
5. **`Palette.query`** (`Palette::on_query_changed`) — `src/palette.rs`, `src/app/palette.rs`, `src/ui/palette.rs`
6. **`SearchBar.query`** (find bar) — `src/app/search.rs`, `src/ui/search.rs`
7. **`NewPopup`** (`address`, `user`, `name`, `topic`, `alias`, all capped with `FIELD_MAX = 1024`; `NewPopup::field_at_mut`) — `src/manage.rs`, `src/app/rooms.rs`, `src/ui/roominfo.rs`
8. **`InfoPopup`** (`editing` Name/Topic and `filter`, capped with `FIELD_MAX = 1024`; `InfoPopup::apply_filter`) — `src/manage.rs`, `src/app/rooms.rs`, `src/ui/roominfo.rs`
9. **`VerifyPopup.password`** (`Option<LineEdit>` with `LineEdit::secret()` + masked `•`) — `src/verify.rs`, `src/app/verify.rs`, `src/ui/verify.rs`
10. **`SidebarState.filter`** — `src/app.rs`, `src/ui/sidebar.rs`

### 3. Key Collision Policies & UI Click-to-Place
- **Key collision handling:**
  - `EmojiPicker`: `arrows: false` so plain `Left`/`Right` continue stepping the emoji grid; `Ctrl-Left`/`Right`, `Alt-B`/`F`, `Home`/`End`, `Ctrl-A`/`E`, `Delete`, `Ctrl-W`, `Alt-Backspace`, `Ctrl-U`, `Ctrl-K` edit `query`.
  - `InfoPopup.filter` (when `editing.is_none()`): `home_end: false` (`Home`/`End` still jump to first/last info row; `Ctrl-A`/`Ctrl-E` move filter cursor), `arrows: row != Some(InfoRow::Notify)` (plain `Left`/`Right` cycle `Notify`; `Ctrl-Left`/`Right` and `Alt-B`/`F` move filter cursor), `delete: !matches!(row, Some(InfoRow::Member(_)))` (`Delete` still confirms member removal when a member row is selected). Inline `InfoPopup.editing` (Name/Topic) uses `EditKeys::FULL`.
  - `VerifyPopup`: uses `EditKeys::FULL` when `field_focused && has_field`; `!typing` keeps `Left`/`Right` stepping buttons.
  - `NewPopup`: uses `EditKeys::FULL` when `field_mut().is_some()`, preserving switch/button `Left`/`Right`/`Space` behavior elsewhere.
  - `SidebarState.filter`: runs after `self.keymap.lookup(Context::Sidebar, key)` in `on_key`, so `Global`/`Sidebar` bindings (`Ctrl-K` -> `FocusRoomFilter` in `DEFAULT`, `Up`/`Down`/`Enter`/`?`) retain precedence, while `Left`/`Right`/`Home`/`End`/`Delete`/`Backspace`/`Ctrl-Left`/`Right`/`Alt-B`/`F`/`Ctrl-A`/`E`/`Ctrl-W`/`Alt-Backspace`/`Ctrl-U` edit the room filter.
  - `LoginForm`, `RecoveryForm`, `SettingsMenu::DownloadDir`, `Palette`, and `SearchBar` use `EditKeys::FULL`.
- **Pointer hit testing (`src/ui/hit.rs`, `src/app.rs`):**
  - Added `InputTarget`, `HitId::Input(InputTarget)`, and `HitMap::at_rect(column, row)` (marking test-only `HitMap::at`, `EmojiPicker::set_query`, and `Palette::set_query` with `#[cfg(test)]`).
  - Clicking value rects (`HitId::Input`) focuses the field where applicable and places the cursor at the clicked display column via `App::click_input`. Label/row hits (`HitId::Field`) still focus `LoginForm` fields and `VerifyPopup` password field.
  - Clicking `HitId::RoomFilter` focuses `Pane::Sidebar` and subtracts the 3-column `" / "` prefix before placing the cursor.
  - `NewPopup` `#alias` and `InfoPopup` inline editing (`Name`/`Topic`) register `HitId::Input` over the value columns after labels/`#`, and all renderers render scrolled slices with `LineEdit::view` (replacing dead `width_of`/`tail_fit`/`tail` helpers and removing `search_clear`, `manage_clear_input`, `push_clean`).

## Changed Files
- `src/lineedit.rs` (new)
- `src/main.rs`
- `src/ui/hit.rs`
- `src/modal.rs`
- `src/verify.rs`
- `src/palette.rs`
- `src/manage.rs`
- `src/ui/modal.rs`
- `src/ui/palette.rs`
- `src/ui/search.rs`
- `src/ui/verify.rs`
- `src/ui/roominfo.rs`
- `src/ui/sidebar.rs`
- `src/app.rs`
- `src/app/palette.rs`
- `src/app/search.rs`
- `src/app/verify.rs`
- `src/app/rooms.rs`

## Verification

### Automated Gate (`./scripts/check.sh`)
Ran `./scripts/check.sh` in `/home/tylerstyle/git/matrix-rat/.claude/worktrees/agy-palette` after the final code edit (exit code `0`):
- `cargo fmt --check` — passed
- `cargo clippy --all-targets -- -D warnings` — passed
- `cargo test` — `440 passed; 0 failed; 18 ignored; 0 measured; 0 filtered out`
- `cargo build` — passed

### Added Tests (included in the 440 passing tests above)
- `lineedit::tests`:
  - `insert_move_delete_and_words_on_graphemes`
  - `combining_mark_inserted_at_zero_before_combining_keeps_valid_boundary`
  - `sanitize_and_byte_limit_preserve_whole_graphemes`
  - `horizontal_scroll_wide_chars_and_click_placement`
  - `masked_view_and_secret_debug`
  - `key_dispatch_and_policy_flags`
- App-level mid-text edit, key-collision, grapheme, secret-redaction, and click-to-place tests:
  - `app::tests::login_and_reauth_inputs_edit_mid_text_and_place_cursor_by_click`
  - `app::tests::recovery_settings_download_dir_and_emoji_inputs_edit_and_click_to_place`
  - `app::tests::sidebar_filter_edits_mid_text_handles_graphemes_and_clicks_to_place`
  - `app::palette::tests::palette_query_edits_mid_text_and_clicks_to_place`
  - `app::search::tests::search_bar_edits_mid_text_and_clicks_to_place`
  - `app::verify::tests::verify_password_edits_mid_text_and_clicks_to_place`
  - `app::rooms::tests::manage_forms_and_room_info_edit_mid_text_and_click_to_place`
