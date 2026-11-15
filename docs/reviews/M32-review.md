# M32 Review — Remaining `Debug` Redaction (`debug-rest`)

- **Branch:** `debug-rest` (`HEAD` `df428a8`, 6 commits, `24 files changed, 1523 insertions(+), 56 deletions(-)`)
- **Worktree:** `.claude/worktrees/debug-rest`
- **Scope:** Static review of `git diff main...debug-rest` and surrounding types (no builds run per request)
- **Verdict:** **Changes requested** — one real `Debug` leak (`keymap::Editor::feedback` reachable via `Modal::Keybinds`), plus minor non-blocking notes on catch-all coverage and defense-in-depth derives.

---

## Finding 1 (Changes requested): `Modal::Keybinds` (`keymap::Editor`) leaks `feedback` (including config file paths)

- **`src/keymap.rs:1780-1790`**: `Editor` still uses `#[derive(Debug, Clone)]` and exposes `pub feedback: Option<(String, bool)>` (`/// (message, is_error) feedback shown inside the modal`, line `1788`).
- **`src/modal.rs:1930, 1944`**: `Modal::Debug` formats `Self::Keybinds(popup) => ("Keybinds", popup)` directly via `f.debug_tuple(name).field(popup).finish()`, forwarding `Editor`'s derived `Debug`.
- **`src/app/keybinds.rs:248-257`** (`save_keybinds_and_report`): on save failure, `editor.feedback` is populated with `Some((format!("Could not save config: {err}"), true))` (line `256`). Because `persist_overrides` -> `save_key_changes` -> `config::edit_document` (`src/config.rs:413, 417, 425`) attaches `path.display()` to the error (`"could not read {}"`, `"invalid config {}"`, `"could not write {}"`), formatting `Modal::Keybinds` (or `App` while Keybinds is open after a save error) logs the filesystem path (e.g. `/home/.../config.toml`) and OS error details.
- **Why tests missed it:** `no_popup_shows_what_it_holds_in_debug` constructs `let keybinds = crate::keymap::Editor::new(&crate::keymap::Keymap::default());` (`src/modal.rs:2157`), leaving `feedback: None`, and `the_debug_output_of_the_app_holds_nothing_private` does not open `Modal::Keybinds`.

**Fix:** Give `crate::keymap::Editor` a hand-written `std::fmt::Debug` that does not print the `feedback` string (e.g., show only whether feedback is present and/or `is_error`), and set a private string/path in `keybinds.feedback` inside `no_popup_shows_what_it_holds_in_debug`.

---

## Checklist Verification

### 1. The `#[cfg_attr(test, derive(Debug))]` / `#[cfg(not(test))]` split (16 types)

Checked all 16 types individually — every `#[cfg(not(test))] impl std::fmt::Debug` matches the type's exact variants, field names, and `Debug` trait bounds, with valid syntax:

- `src/complete.rs`: `MentionKind` (`113`, `133`), `MentionMark` (`147`, `156`)
- `src/matrix/verify.rs`: `SasEmoji` (`118`, `126`)
- `src/upload.rs`: `Completion` (`276`, `286`)
- `src/menu.rs`: `Target` (`20`, `46`), `MessageRef` (`61`, `80`)
- `src/rich.rs`: `Segment` (`76`, `84`), `RichLine` (`122`, `137`), `CodeBlock` (`151`, `162`), `Rich` (`173`, `186`)
- `src/modal.rs`: `Pick` (`228`, `237`)
- `src/palette.rs`: `Target` (`39`, `52`)
- `src/images.rs`: `Slot` (`516`, `528`)
- `src/app/unread.rs`: `Marker` (`35`, `49`), `UnreadTarget` (`63`, `71`), `Line` (`82`, `105`)

Under `#[cfg(test)]`, none of the enclosing containers (`CompleteState`, `VerifyStage`, `Menu`, `Entry`, `Palette`, `EmojiPicker`, `Unread`) format those fields with `{:?}` directly, so the unredacted test-only `derive(Debug)` does not leak through `App` or `Modal` `Debug` output either.

### 2. Catch-all tests (`the_debug_output_of_the_app_holds_nothing_private` & `no_popup_shows_what_it_holds_in_debug`)

- **`src/modal.rs:2038-2307` (`no_popup_shows_what_it_holds_in_debug`)**: Covers all 26 `Modal` variants (`lines 2273-2300`) against `PRIVATE` (`"secret"`, `"Secret"`, `"alice"`, `"Alice"`, `"hunter2"`, `"DEVHUSH"`, `"/home/"`, `"mxc:"`), catching future `Modal` additions via `Modal`'s exhaustive hand-written `match`. Only gap is `keybinds.feedback` being `None` at `src/modal.rs:2157` (covered in Finding 1).
- **`src/app.rs:8515-8686` (`the_debug_output_of_the_app_holds_nothing_private`)**: Populates `matrix`, `user_id`, `own_profile`, `rooms` (room with alias + 2 messages incl. URL), `spaces`, `link_previews: All`, `download_dir`, `notification_click_command`, `pending_copy`, `toast`, `ignored_users`, `timeline.marked_read`, `sidebar.filter`, runs `redraw` (filling `row_cache` and `previews`), `on_room_members` + composer draft (filling `complete.active`), opens 12 distinct `Modal` variants (`Search`, `Menu`, `Palette` incl. forward pick, `RoomInfo`, `UserInfo`, `Details`, `Pinned`, `Threads`, `New`, `Emoji`, `Settings`, `Login`), and opens a thread (`Action::OpenThread` -> `threads.open`).
  - *Non-blocking note:* `App`'s own test leaves the other 14 `Modal` variants to `modal.rs` and leaves some already-redacted `App` fields (`attach`, `devices`, `verify`, `pending_jump`, `saving`/`saved`/`confirm_large`, `edit_stash`, `startup_target`, `emotes`, custom theme names, `unread.line` `Line::At`) at defaults.

### 3. Remaining `#[derive(Debug)]` types across `src/`

Audited all `#[derive(Debug)]` occurrences across `src/` (`141` matches, no multi-line `#[derive(...Debug...)]`):

- **Leaky:** `crate::keymap::Editor` (Finding 1).
- **Defense-in-depth notes (no current leak):**
  - `Theme` (`src/theme.rs:131-142`) derives `Debug` and holds `pub name: String` (custom theme name) and `pub warnings: Vec<String>`. Neither `App::Debug` nor `Config::Debug` formats it today, so nothing leaks unless `Theme` is logged directly in the future.
  - `RawKeyValue` / `RawKeyConfig` (`src/config.rs:98-113`) derive `Debug`; omitted from `Config::Debug` (`finish_non_exhaustive()`).
- **Verified privacy-safe:** All other checked types keeping `#[derive(Debug)]` (`SidebarState`, `SettingsMenu`, `KeysPopup`/`SessionStage`/`KeysStage`, `VerifyState`/`Security`, `ui::hit::{Button, InputTarget, HitId, HitMap}`, `MatrixHandle`, `clipboard::{Request, Done}`, `model::ReadBy`, `ReauthDecision`, `manage::Plan`, `media::Refused`, `spaces::SpaceEdit`, `Keymap`/`Binding`/`EditorStage`/`EditorRow`/`EditorButton`, and plain `Copy`/enum types) hold no unredacted private strings or paths (`DevicesState` uses a hand-written `impl std::fmt::Debug` at `src/devices.rs:58`).

### 4. Diff scope (nothing but `Debug`)

Checked all 6 commits (`626800f`, `d1b5738`, `00dbe74`, `a2093f4`, `a48e567`, `df428a8`): only `#[derive]` attributes, `impl std::fmt::Debug` blocks, doc comments, and `#[cfg(test)]` tests/assertions (`src/rich.rs:1248, 1333`, `src/app.rs:8055`) changed.
