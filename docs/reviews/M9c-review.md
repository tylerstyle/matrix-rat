# Review: M9c — Find in room (`Ctrl-F`)

**Commit reviewed:** `ae95afe` (`Find in room (Ctrl-F)` on local `main`)  
**Files inspected:** `src/app/search.rs`, `src/ui/search.rs`, `src/app.rs`, `src/matrix/rooms.rs`, `src/action.rs`, `src/keymap.rs`, `src/modal.rs`, `src/palette.rs`, `src/text.rs`, `src/ui/mod.rs`, `src/ui/modal.rs`, `src/ui/timeline.rs`, `scripts/check.sh`, and `src/ui/snapshots/matrix_rat__ui__tests__{find_bar,command_palette,help_overlay}.snap`.

---

## Summary

The core in-room find flow is concise and cleanly factored: `SearchBar` isolates search state and matching (`src/app/search.rs`), `ui::search::draw` keeps popup rendering self-contained (`src/ui/search.rs`), normal room/thread queries scope automatically via `self.messages()` and `self.send_matrix` (`self.in_thread(...)`), `generation` prevents stale `MatrixEvent::Paginated` replies from older rooms from advancing a new room's search, and `MAX_DIG = 15` caps automatic back-pagination rounds per `Enter`.

There are however **four `[HIGH]` issues** (plus several `[MEDIUM]` and `[LOW]` items) that should be fixed before pushing:
1. A failed `MatrixCmd::Paginate` timeline lookup emits `ActionFailed` instead of `Paginated`, leaving `loading_older` and `bar.digging` permanently stuck; `Paginated` also miscounts thread growth against room messages (`rooms[active].messages`).
2. `MatrixEvent::Paginated` can outrun the corresponding `MatrixEvent::Timeline` from `forward` (which awaits `unread()`), and `apply_timeline` never refreshes an open `Modal::Search` when messages arrive, edit, or redact.
3. `ui::search::draw` clears the top-right `box_width × 3` rows (`area.y + 1..area.y + 4`) inside the Messages area, directly covering the matched message when `reveal_selected` scrolls an older hit to `top = first`, as well as hiding the `" loading older messages… "` pill.
4. `on_search_key` pushes raw `KeyCode::Char(c)` without filtering control or directional-override characters (`sanitize_line`), and `ui::search::draw` trims long queries per scalar `char` (`shown.remove(0)`) instead of by grapheme cluster.

---

## Findings

### 1. [HIGH] Stuck `loading_older` / `bar.digging` on `Paginate` failure and wrong length check for thread pagination
- **Where:** `src/matrix/rooms.rs:204-236`; `src/app/search.rs:122-150`; `src/app.rs:1114-1139, 1862-1875, 1884`
- **Detail:**
  1. In `MatrixCmd::Paginate` (`src/matrix/rooms.rs:216-235`), errors from `timeline.paginate_backwards(PAGE)` are converted to `MatrixEvent::Paginated { generation, reached_start: true }` (`lines 221-231`), **but** if `self.timeline(&room_id).await` returns `Err(err)` (line 234 — for example, opening a thread timeline fails or room state is unavailable), the worker calls `self.fail(...)` -> `MatrixEvent::ActionFailed` and **never sends `MatrixEvent::Paginated`**. Because `MatrixEvent::ActionFailed` (`src/app.rs:1884`) only shows a toast without clearing `self.timeline.loading_older` or `bar.digging`, both `self.timeline.loading_older = true` (set at `src/app/search.rs:139`) and `bar.digging` (`"looking in older messages…"`) stay stuck permanently for that room/thread, and subsequent `Enter` presses hit `if note.is_some() || self.timeline.loading_older { return; }` (`src/app/search.rs:137`) and silently do nothing.
  2. When a search paginates inside an open thread (`self.send_matrix` wraps `Paginate` via `self.in_thread(...)` at `src/app.rs:1573`), `search_dig` (`src/app/search.rs:140`) and `maybe_load_older` (`src/app.rs:1118`) set `view.count_at_request = self.messages().len()` (the **thread's** message count), while `MatrixEvent::Paginated` (`src/app.rs:1868-1869`) computes `grew` against `self.rooms.get(self.active).map_or(0, |r| r.messages.len())` (the **room's** message count). Inside a thread, `grew` tracks the wrong slice and corrupts `view.empty_batches` (if `search_dig` increments `empty_batches` to `MAX_EMPTY = 5` while `Paginated` sees `!grew`, `maybe_load_older` view-filling later stops early in `src/app.rs:1123`).
- **Note on runaway risk:** `OpenRoom` / `OpenThread` call `self.close()` (`src/matrix/rooms.rs:69, 180, 382`) to abort the previous `paginator` task, `generation` filters stale `Paginated` replies (`src/app.rs:1866`), `MAX_DIG = 15` bounds rounds per `Enter`, and `paginate_backwards` errors set `reached_start: true`, so normal operation does not loop endlessly.
- **Recommendation:** Emit `MatrixEvent::Paginated { generation, reached_start: true }` (or clear `loading_older` and `Modal::Search` `bar.digging` cleanly) if `self.timeline(&room_id).await` fails in `MatrixCmd::Paginate`, and compare `self.messages().len() > view.count_at_request` in the `MatrixEvent::Paginated` handler (`src/app.rs:1868`).

### 2. [HIGH] Race between `MatrixEvent::Paginated` and `MatrixEvent::Timeline`, and stale `Modal::Search` on timeline updates
- **Where:** `src/matrix/rooms.rs:220-231, 599-608`; `src/app.rs:1143-1176, 1850-1875`; `src/app/search.rs:58-88, 154-175`
- **Detail:**
  1. In the worker, `forward` (`src/matrix/rooms.rs:599-608`) awaits `unread(&items).await` between applying timeline `diffs` and sending `MatrixEvent::Timeline`, whereas the `paginator` task (`lines 220-231`) sends `MatrixEvent::Paginated` as soon as `timeline.paginate_backwards(PAGE).await` completes. If `Paginated` arrives at `App::on_matrix` before the matching `Timeline` snapshot, `search_after_load()` (`src/app/search.rs:154-175`) searches the stale `self.messages()`: on intermediate rounds (`bar.digging > 1`) it immediately consumes another round early; on the final round (`bar.digging == 1` -> `search_dig(0)`), `bar.digging` drops to `0` and the bar shows `"nothing found further back · Enter keeps looking"` even when matching messages arrive immediately afterward in `MatrixEvent::Timeline`.
  2. More broadly, `apply_timeline` (`src/app.rs:1143-1176`) preserves `self.timeline.selected` across message edits, insertions, and deletions, **but never updates an open `Modal::Search(bar)`**:
     - Newly loaded messages (initial room/thread load completing while `Ctrl-F` is already open, live incoming messages, edits, or delayed post-`Paginated` pages) never refresh `bar.position` / `bar.total` or clear `"not in the loaded messages"` / `"nothing found further back"`.
     - If the current hit (`bar.hit`) is redacted/deleted while `Modal::Search` is open, `apply_timeline` (`src/app.rs:1160-1161`) resets `self.timeline.selected` to the newest selectable message while `bar.hit` retains the deleted event id. On the next `Enter` or `Down`, `bar.hit` is not found in `matches` and `search_step` / `search_after_load` falls into `None => matches.len().checked_sub(1)`, jumping forward to the newest match in the room.
- **Recommendation:** Re-sync `Modal::Search` inside `apply_timeline` (recompute `bar.total`/`bar.position` when `bar.hit` still matches; clear or re-anchor `bar.hit` without losing search position when redacted; and if `bar.query` is non-empty with `bar.hit.is_none()` and matches now exist from a newly arrived timeline batch, highlight the appropriate match — wait to trigger `search_dig(0)` failure until the batch is seen or treat newly arrived matches as satisfying a just-finished dig).

### 3. [HIGH] Find bar covers the found message and the loading indicator
- **Where:** `src/ui/search.rs:15-23`; `src/ui/timeline.rs:639-653, 755-761`; `src/ui/snapshots/matrix_rat__ui__tests__find_bar.snap:6-9`
- **Detail:** `ui::search::draw` (`src/ui/search.rs:20`) clears and draws a `box_width` (up to `46` cols) × `3` rows overlay at `area.y + 1 .. area.y + 4` in the top-right corner — directly over rows `1..=3` of the Messages pane content area (already visible in `find_bar.snap:6-8`, where columns `53..=98` of rows `6..=8` are overwritten by the box). Meanwhile, `search_go` (`src/app/search.rs:62`) sets `self.timeline.reveal_selected = true`, and `reveal_selected` (`src/ui/timeline.rs:645-650`) brings an above-viewport match on screen by setting `top = first` — putting row `0` of the found message at row `1` of the Messages pane, directly underneath the Find bar. Whenever scrolling up to an older match in a scrollable room/thread (or on narrower widths where `box_width` spans most/all of the Messages pane), the found message's header and top body lines sit behind `Clear`, and the `" loading older messages… "` pill (`src/ui/timeline.rs:755-761`) is covered while digging.
- **Recommendation:** Reserve clearance under the Find bar when `reveal_selected` scrolls an older match into view while `Modal::Search` is open (or dock/anchor the bar in the Messages border/bottom composer area so top timeline rows stay unobscured).

### 4. [HIGH] Raw control characters / Bidi overrides can be typed into `bar.query`, and `shown.remove(0)` splits grapheme clusters
- **Where:** `src/app/search.rs:192-194`; `src/ui/search.rs:58-73`; `src/text.rs:29-42`
- **Detail:**
  1. `search_paste` (`src/app/search.rs:201`) filters pasted text through `crate::text::sanitize_line`, **but** `on_search_key` (`src/app/search.rs:192-194`) pushes any `KeyCode::Char(c)` (`!mods.intersects(CONTROL | ALT)`) directly into `bar.query`, and `ui::search::draw` passes the slice straight to `Span::raw(shown.clone())`. Any C0/C1 control or Unicode directional override arriving via `KeyCode::Char` (e.g. `\r`, `\t`, `\x9b`, `\u{202e}`) is stored unstripped and rendered raw to the terminal buffer.
  2. When the query exceeds `text_room`, `ui::search::draw` truncates from the left via `while width(&shown) > text_room && !shown.is_empty() { shown.remove(0); }` (per scalar `char`), splitting multi-codepoint grapheme clusters (combining accents, skin-tone modifiers, ZWJ emoji) and taking $O(L^2)$ on long queries.
- **Recommendation:** Ignore or sanitize `c` via `crate::text::sanitize_line` / `!unsafe_char(c)` in `on_search_key`, and advance the left slice boundary in `ui::search::draw` on `UnicodeSegmentation::graphemes(&bar.query[..], true)` boundaries rather than `String::remove(0)`.

---

### 5. [MEDIUM] Clicking outside the Find bar (`HitId::Backdrop`) skips `close_search()`
- **Where:** `src/app.rs:2314-2329` vs `src/app.rs:1630` and `src/app/search.rs:215-224`
- **Detail:** `Esc` and `Ctrl-F` close search via `self.close_search()`, which moves focus to `Pane::Timeline` and sets `self.timeline.reveal_selected = true` whenever `bar.hit.is_some()`. Clicking `HitId::Backdrop` in `App::on_mouse` (`src/app.rs:2317-2327`), however, executes `self.modal = None;` directly without calling `self.close_search()`, leaving focus in the previous pane (e.g. Sidebar or Composer) instead of on the located message.
- **Recommendation:** Route `Modal::Search(_)` in `HitId::Backdrop` handling (`src/app.rs:2323-2327`) through `self.close_search()`.

### 6. [MEDIUM] Room removal/switch under an open Find bar keeps stale search state; invites are searchable
- **Where:** `src/app.rs:1925-1931, 2008-2050`; `src/app/search.rs:33-38`; `src/palette.rs:423-443`
- **Detail:**
  1. If the active room is removed (kicked/left elsewhere) or `auto_opened` switches from an initial Invite batch to a real room (`src/app.rs:1925-1931, 2018-2050`), `apply_rooms` resets `self.threads.open`, `self.active`, `self.timeline`, and the composer, **but leaves `Modal::Search(bar)` open** with the old room's `bar.hit`, counts, `bar.digging`, and `bar.note`. (Logout / `NeedLogin` overwrites or clears `self.modal` cleanly via `Setting::Logout` / `Action::Logout` / `Modal::Login`.)
  2. `open_search()` (`src/app/search.rs:33-38`) checks `self.room().is_none()` without excluding `RoomKind::Invite` (even though `search_dig` at line 124 checks `room.kind != RoomKind::Invite`), allowing `Ctrl-F` to open on invite cards and immediately report `"no older matches"` on `Enter`.
  3. Because `collect_actions` includes all non-navigation bindings from `keymap::BINDINGS` (`src/keymap.rs:209-215`), `"Find in this room"` (`Action::Search`) appears in the Command Palette (`src/palette.rs:423-443`) under `_ => true` — showing as enabled even when `!ctx.has_room` (no rooms, silently no-ops) or `ctx.in_invite`.
- **Recommendation:** Close `Modal::Search` when `apply_rooms` switches `self.active` to a different room (`None =>` branch or room id change), disallow `RoomKind::Invite` in `open_search()`, and add `Action::Search => ctx.has_room && !ctx.in_invite` in `action_enabled` (`src/palette.rs:423-443`).

### 7. [MEDIUM] No-match edits leave scroll displaced on prior partial matches; tiny viewports hide the active modal
- **Where:** `src/app/search.rs:73-88, 215-224`; `src/ui/search.rs:16-18`
- **Detail:**
  1. Typing incrementally jumps `self.timeline.selected` on each matching prefix (`search_go`). When the next keystroke produces 0 loaded matches (`src/app/search.rs:80-87`), `bar.hit` resets to `None` while `self.timeline.selected` and scroll remain displaced on the earlier partial match; pressing `Esc` afterwards leaves the timeline moved away from where the user started (without even refocusing `Pane::Timeline` because `bar.hit.is_none()`).
  2. In `ui::search::draw` (`src/ui/search.rs:16-18`), if `area.width < 18` (`box_width < 16`) or `area.height < 5`, the bar returns early without drawing anything or registering `HitId::Backdrop` / `HitId::Overlay`, leaving `Modal::Search` open invisibly and capturing all non-quit/suspend keyboard events until `Esc`/`Ctrl-F`.
- **Recommendation:** Record the pre-search selection/scroll when `open_search` opens and restore it if search closes without an active match (or on no-match restart); on undersized viewports either draw a minimal 1-line bar if `height >= 3` or close/refuse opening rather than going invisible while modal.

### 8. [LOW] Snapshot side effects (`command_palette.snap` / `help_overlay.snap`), missing wheel / cursor-edit ergonomics, and test gaps
- **Where:** `src/ui/snapshots/matrix_rat__ui__tests__{command_palette,help_overlay}.snap`; `src/app.rs:2441-2446`; `src/ui/mod.rs:194-197`; `src/app/search.rs`
- **Detail:**
  1. Adding `Action::Search` (`Ctrl-F`) to `keymap::BINDINGS` (`src/keymap.rs:209-215`) automatically adds a row to the Command Palette and the non-scrolling `Everywhere` column of Help: in the 100×30 snapshots, `Action   Quit   Ctrl-Q` shifted out of the initial 14-row window in `command_palette.snap` (`line 27` ends at `Ctrl-Z`), and `Ctrl-Z   Suspend to the shell` is now pushed off the bottom of `help_overlay.snap` (`line 32`, compounding M9b's `Ctrl-Q` clipping).
  2. Mouse wheel events (`src/app.rs:2441-2446`) are ignored while `Modal::Search` is open (neither stepping matches nor scrolling), and `SearchBar` supports only append/`Backspace`/`Ctrl-U` (no `Left`/`Right`/`Home`/`End` cursor movement inside long queries).
  3. In `src/ui/mod.rs:195`, the size loop tests `(30, 8), (17, 5), (10, 3)` — where `(17, 5)` hits the early return (`box_width = 15 < 16`) rather than exercising the narrowest drawn size `(18, 5)`.
  4. **Missing tests:** `src/app/search.rs` currently has 3 solid baseline tests (`typing_jumps_to_the_newest_match_and_enter_walks_back`, `looking_further_back_loads_history_until_found_or_out_of_patience`, `search_needs_a_room_and_matches_names_too`), but lacks coverage for:
     - `MatrixEvent::Paginated` arriving before `MatrixEvent::Timeline`,
     - `ActionFailed` on `MatrixCmd::Paginate` / stuck `loading_older` & `bar.digging`,
     - searching and paginating inside an open thread (`InThread` + `Paginated` `empty_batches`),
     - live `apply_timeline` inserts/edits/redactions and `apply_rooms` room removal while `Modal::Search` is open,
     - `HitId::Backdrop` click closing search,
     - raw control / Bidi `KeyCode::Char` rejection and grapheme-safe horizontal clipping,
     - `reveal_selected` scrolling behavior with the Find bar open on a scrollable timeline.

---

## Verification

- Ran `./scripts/check.sh` on `/home/tylerstyle/git/matrix-rat` (clean `HEAD` `ae95afe`):
  - `cargo fmt --check`: passed
  - `cargo clippy --all-targets -- -D warnings`: passed
  - `cargo test`: `292 passed; 0 failed; 14 ignored; 0 measured; 0 filtered out; finished in 4.85s` (all 3 tests in `app::search::tests` and `ui::tests::find_bar` passed; 14 server-backed integration tests are `#[ignore]` offline)
  - `cargo build`: passed

---

## Verdict

**Approve with changes** — fix `[HIGH]` Findings **1–4** (`Paginate` failure clearing `loading_older`/`digging` + thread length in `Paginated`, `Paginated`/`Timeline` ordering & `apply_timeline` refresh, Find bar covering revealed matches at `top = first`, and control/Bidi sanitization + grapheme-boundary clipping), address `[MEDIUM]` Findings **5–7** (`Backdrop` using `close_search`, `apply_rooms` / invite guards, partial-match scroll displacement), and add regression tests for the concurrency, thread, and layout cases before merging/pushing.
