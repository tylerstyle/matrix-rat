# Review: M10b — Right-click context menu & read markers

**Commit reviewed:** `0c5ef60` (`Merge right-click menu and read markers`, on local `main`, checked at `HEAD` `4f23c15`)  
**Files inspected:** `src/menu.rs`, `src/app/menu.rs`, `src/ui/menu.rs`, `src/matrix/receipts.rs`, `src/ui/timeline/receipts.rs`, `src/matrix/rooms.rs`, `src/matrix/threads.rs`, `src/matrix/convert.rs`, `src/app.rs`, `src/app/thread.rs`, `src/app/rooms.rs`, `src/app/palette.rs`, `src/palette.rs`, `src/modal.rs`, `src/model.rs`, `src/text.rs`, `src/ui/mod.rs`, `src/ui/modal.rs`, `src/ui/timeline.rs`, `src/ui/sidebar.rs`, `src/ui/composer.rs`, `src/ui/hit.rs`, `src/ui/image.rs`, `README.md`, and `matrix-sdk-ui-0.19.1` timeline/receipts handling (`mod.rs`, `read_receipts.rs`, `echo.rs`).

---

## Summary

Overall both features are cleanly structured and well-tested in their happy paths. The context menu tracks targets by ID rather than index, closes via `menu_follow()` when targets disappear, swallows backdrop/border clicks without clicking through, requires two explicit choices for `Delete`, and routes `Leave…` / `Decline invitation…` into `RoomInfo` confirmation rather than leaving directly. Read markers isolate room (`Main`/`Unthreaded`) vs thread (`ReceiptThread::Thread`) receipts in `matrix-sdk-ui` (and clear room receipts from the thread root placeholder until `root_loaded`), keep inline layout row-count-invariant in normal widths, exclude own/sender receipts (`pick`), keep private vs public `read_receipts` independent, and sanitize names with `sanitize_line`.

There are **four `[MEDIUM]`** issues and **seven `[LOW]`** issues / gaps worth follow-up:

1. **`[MEDIUM]`** Unconfirmed single-click `"Discard"` on an in-flight `SendState::Sending -> Sent` message redacts an already-sent event (`src/menu.rs`, `src/app/menu.rs`, `src/app.rs`).
2. **`[MEDIUM]`** `Target::Room` filtered out of `visible_rooms()` stays open via `menu_target_alive` but fails `perform_for_room` actions while other entries still work (`src/app/menu.rs`).
3. **`[MEDIUM]`** `Setting::ReadMarkers` turned off hides inline markers in the timeline but still offers `"Read by…"` in message context menus (`src/menu.rs`, `src/app/menu.rs`, `README.md`), and `Setting::ReadMarkers` is omitted from the command palette (`src/palette.rs`, `src/app/palette.rs`).
4. **`[MEDIUM]`** Per-diff `O(N)` rebuilds, duplicate `speakers()` / `pick()` passes, `O(W^2)` `wanted` dedup, and whole-map `self.known.clear()` thrashing above `NAMES_MAX = 2000` in `forward()` / `Names::learn()` (`src/matrix/rooms.rs`, `src/matrix/receipts.rs`).

---

## Findings — Context Menu

### 1. [MEDIUM] In-place `SendState::Sending -> Sent` update lets single-click `"Discard"` redact a sent message
- **Where:** `src/menu.rs:366-372`, `src/app/menu.rs:36-47, 60, 310-335`, `src/app.rs:1367-1383, 1540-1562`, `src/matrix/convert.rs:28, 111-119`.
- **Detail:**
  1. For an own unsent message (`!sent`), `menu::message_items` (`src/menu.rs:366-368`) emits `"Discard"` mapped to `Run::Do(Action::Delete)` with `confirm: false` (unlike `"Delete"` on lines 369–372, which uses `.asking()`).
  2. When a local echo transitions to `SendState::Sent` (verified in `matrix-sdk-ui-0.19.1/src/timeline/tests/echo.rs:108, 130`), `item.unique_id().0` (`Message.id`, `src/matrix/convert.rs:28`) remains unchanged while `Message.item` becomes `ItemRef::Event(...)`.
  3. Because `find_message` (`src/app/menu.rs:36-47`) only checks room, thread, and `m.id == wanted.id`, `menu_follow()` leaves the pre-send menu open. Clicking `"Discard"` passes `menu.confirmed(index)` in one click (`confirm == false`) and calls `delete_message` (`src/app.rs:1376-1381`), sending `MatrixCmd::Redact` for the now-sent server event without the `"Again to delete"` confirmation step.
  4. Symmetrically, if a `SendState::Failed` message becomes `Sent` while its menu is open, `"Try sending again"` (`Run::Do(Action::Reply)`) misses `m.state == SendState::Failed` at `src/app.rs:1540-1553` and falls through to `Action::Reply` (`line 1554`), opening a reply composer to your own message instead.
- **Recommendation:** Include `SendState` (or `ItemRef` variant / `MsgKind`) in `MessageRef` or `menu_target_alive` so state transitions invalidate or refresh the open menu, or re-check preconditions in `run_menu_at` before running `"Discard"` / `"Try sending again"`.

### 2. [MEDIUM] `Target::Room` filtered out of `visible_rooms()` stays open but fails `perform_for_room` actions
- **Where:** `src/app/menu.rs:64, 367-379, 399-409`; `src/app.rs:382-402`.
- **Detail:**
  1. Suppose a room context menu (`Target::Room { id }`) is opened while a `space` or `sidebar.filter` is active, and a `MatrixEvent::Spaces` or `MatrixEvent::Rooms` update removes that room from `self.visible_rooms()` (e.g., renamed outside `sidebar.filter`, unlinked from the space, or moved from `Invite` to `Room` outside the active space) while keeping it in `self.rooms`.
  2. `menu_target_alive` (`src/app/menu.rs:64`) only checks `self.rooms.iter().any(|room| room.id == *id)`, so `menu_follow()` keeps the menu open.
  3. Choosing `Open`, `Mark as read`, or `Notification mode…` succeeds (`lines 373-375` index `self.rooms` directly), whereas choosing `Room info`, `Add/Remove from favourites`, or `Leave…` routes through `Run::Do(action) -> perform_for_room(index, action)` (`lines 376, 400`), which looks up `self.visible_rooms().iter().position(|i| *i == index)` and aborts with toast `"That room is no longer listed"`.
- **Note on `perform_for_room` cursor/focus safety:** When the room is listed, `perform_for_room` (`src/app/menu.rs:399-409`) saves `(self.focus, self.sidebar.cursor)`, sets `Pane::Sidebar` + `at` directly (avoiding `set_focus`), dispatches `Action::RoomInfo` / `Action::LeaveRoom` / `Action::Manage(Click::Favourite)` (none of which return through `set_focus` or `open_room`), and unconditionally restores `(self.focus, self.sidebar.cursor) = before`. On current code `focus`/`cursor` cannot be left wrong, though keeping that invariant relies on routed actions never calling `set_focus` or `open_room`.
- **Recommendation:** Route `RoomInfo`, `LeaveRoom`, and `SetFavourite` directly by `room_id` / `index` cleanly (or have `menu_target_alive` check `visible_rooms()` too if sidebar visibility is intended to dismiss the menu).

### 3. [LOW] In-place redaction (`MsgKind::Redacted`) and remote `Favourite` changes keep stale menu items
- **Where:** `src/app/menu.rs:36-77, 325-346`; `src/matrix/convert.rs:28, 233-238`; `src/app/rooms.rs:491-505`; `src/app.rs:1563-1614`.
- **Detail:**
  - If an open message is redacted remotely while its `Target::Message` menu (or `"Again to delete"` arming) is open, `message.id` is preserved and `kind` becomes `MsgKind::Redacted` (`body = "Message deleted"`), so `menu_follow()` keeps the pre-redaction menu open. Clicking `"Copy text"` copies `"Message deleted"`, `"Reply"` replies to the redacted item, `"React…"` opens the emoji picker for the redacted event, `"Edit"` toasts `"This message cannot be edited"`, and `"Delete"` sends a redundant `Redact`. (`Target::Link` is safe because `find_link` re-checks current `Rich` links.)
  - If a room stops being `RoomKind::Favourite` remotely while `"Remove from favourites"` is open in `Target::Room`, `Click::Favourite` (`src/app/rooms.rs:500`) evaluates `let on = room.kind != RoomKind::Favourite;` (`true`) and re-adds it to favourites.

### 4. [LOW] `"Again to delete"` truncates to `"Again to de…"` when `Menu::place` item widths are `< 15`
- **Where:** `src/menu.rs:181-186, 238-248`; `src/ui/menu.rs:37, 48-58`.
- **Detail:** `Menu::place` calculates `content` only from `item.label` and `item.hint` (`width("Delete") + 2 + width("Del") = 11`, or `6` if `Delete` is unbound), ignoring the 15-column armed label `"Again to delete"`. For an own message inside a thread (`facts.in_thread == true`, removing `"Reply in thread"`) where `"Copy link to message"` is absent (e.g., demo threads or unlinked local items), the widest entry is `"Reply" + "Enter"` (`12` cols), making `room = 12` (`src/ui/menu.rs:37`). Arming `"Delete"` calls `truncate("Again to delete", 12)` (`line 57`), rendering `"Again to de…"`. (Still requires a second click/Enter to delete.)
- **Recommendation:** Count `width("Again to delete")` when `item.confirm` is true in `Menu::place`.

### 5. [LOW] Minor context-menu input and layout polish issues
- **Where:** `src/app/menu.rs:197-202, 237-280, 291`; `src/ui/menu.rs:26`; `src/app.rs:2612-2648`.
- **Detail:**
  - `on_menu_key` (`src/app/menu.rs:291`) hardcodes `KeyCode::Menu` to toggle closed rather than checking the bound `Action::ContextMenu` key if rebound.
  - `menu_mouse` checks `if !self.mouse` only on `Down(MouseButton::Right)` (`line 261`), not on in-flight `Down(Left)`/`Moved` right after toggling mouse capture off (`lines 237, 267`). (No click-through or double-click accidental delete occurs: left-clicks/right-clicks while open consume the event cleanly, and `Delete` always requires two clicks.)
  - When a menu has more items than fit the terminal height (e.g., a long `"Read by"` list on a low window), mouse wheel scrolling does nothing because `Modal::Menu(_)` is absent from `App::on_mouse` scroll arms (`src/app.rs:2612-2648`).
  - `ui::menu::draw` (`src/ui/menu.rs:26`) checks `if width(&title) > 1`, silently hiding valid 1-column room titles.

---

## Findings — Read Markers

### 6. [MEDIUM] `"Read by…"` remains available when `Setting::ReadMarkers` is off, and `Read markers` is missing from the command palette
- **Where:** `README.md:30-31`; `src/ui/timeline.rs:607-609`; `src/menu.rs:272-279, 363-365`; `src/app/menu.rs:111-119`; `src/modal.rs:185-198`; `src/palette.rs:81-106, 637-699`; `src/app/palette.rs:63-85, 155`.
- **Detail:**
  1. `README.md:30-31` documents that `read_markers = false` / Settings disables read markers (and inline markers check `if app.read_markers` at `src/ui/timeline.rs:607`). However, `Names::annotate` still fills `message.read_by` (`src/matrix/rooms.rs:557`), `MessageFacts` omits `read_markers` (`src/menu.rs:272-279`, `src/app/menu.rs:111-117`), and `menu::message_items` (`src/menu.rs:363-365`) only checks `!message.read_by.is_empty()`. With `Read markers` off, right-clicking or pressing `m` on a read message still offers `"Read by…"` and opens the reader/timestamp list.
  2. `Setting::ALL` includes `Setting::ReadMarkers` (`src/modal.rs:192`) and `App::perform_setting` explicitly handles it (`src/app/palette.rs:155`), yet `PaletteContext` (`src/palette.rs:81-106`), `open_palette` (`src/app/palette.rs:63-85`), and `collect_settings` (`src/palette.rs:637-699`) omit it (only wiring `ReadReceipts`), leaving `Read markers` unreachable from `Ctrl-P`. (Note: `Setting::RoomPreviews` is likewise missing from `collect_settings`.)
- **Recommendation:** Gate `"Read by…"` in `MessageFacts` / `menu::message_items` on `app.read_markers`, and add `read_markers` to `PaletteContext` / `collect_settings`.

### 7. [MEDIUM] Per-diff `O(N)` work, duplicated scans, `O(W^2)` dedup, and `NAMES_MAX` cache thrashing
- **Where:** `src/matrix/rooms.rs:548-624`; `src/matrix/receipts.rs:22, 37-116, 127-139`; `src/app.rs:438-443, 1256-1265`.
- **Detail:**
  - Verified in `matrix-sdk` (`room.get_member_no_sync(&user).await`) that `Names::learn` makes **no network requests** (local store only).
  - However, on **every** `changes.next()` timeline diff batch (including a single read receipt advancing), `forward()` runs `readers.learn`, `convert`, `readers.annotate`, `threads::annotate`, and sends the full `Vec<Message>` snapshot to `App::apply_timeline` -> `messages_mut()`, bumping `self.timeline_rev += 1` (`src/app.rs:439`) and invalidating the UI `RowCache`.
  - Within each update: `speakers(items)` (`src/matrix/receipts.rs:107-116`) scans every item twice (`learn` line 38 and `annotate` line 75) without deduplicating repeated senders before running `display_name()` + `clean_name()`, and `pick(receipts(event), ...)` allocates/sorts a `Vec` twice per event with receipts (`lines 41, 81`).
  - `Names::learn` (`lines 40-49`) deduplicates `wanted` via linear `!wanted.iter().any(|known| known == user)` (`O(W^2)` when many distinct non-speaker readers hold receipts).
  - If `self.known.len() + wanted.len() > NAMES_MAX` (`NAMES_MAX = 2000`, `lines 22, 53-55`), `self.known.clear()` drops the entire cache at once and only populates `.take(NAMES_MAX)` (`line 56`). If loaded timeline items hold explicit receipts from `> 2000` un-cached non-speaker users, `2000 + remaining > 2000` repeats `.clear()` and up to 2,000 local store lookups on every timeline diff.
- **Recommendation:** Track a `HashSet` for `wanted` dedup, share `speakers` / `pick` or compute names during one pass, and evict boundedly (or keep LRU/oldest entries instead of `.clear()`ing all 2,000 entries and only storing `take(NAMES_MAX)`).

### 8. [LOW] Lazy-loaded member names stay stuck as MXID localparts after `fetch_members()` completes
- **Where:** `src/matrix/rooms.rs:117-122, 607, 620`; `src/matrix/receipts.rs:42-46, 58-63`.
- **Detail:** Opening an un-synced room spawns `timeline.fetch_members()` asynchronously (`src/matrix/rooms.rs:117-122`), while `forward()` immediately runs `readers.learn`. Non-speaker readers missing from the local store return `Ok(None)` from `get_member_no_sync` and have `clean_name(None, &user)` (their localpart) cached in `self.known`. Because `learn` skips `self.known.contains_key(user)` (`line 43`), their label stays as the localpart even after `fetch_members()` finishes until they speak or the room timeline is reopened.

### 9. [LOW] Standalone receipt row on full-width messages causes a 1-row scroll shift when receipts move
- **Where:** `src/ui/timeline/receipts.rs:44-68`; `src/ui/timeline.rs:612-626, 740-752`.
- **Detail:** When a message's last row leaves `free >= 1` columns (`GAP = 2` gap + marker width), row counts do not change and scroll stability holds. When `taken + GAP + 1 > max_width` (full-width message or picture row), `receipts::add` appends a dedicated `Row` with `source = row.source` (`src/ui/timeline/receipts.rs:60-68`). Adding/removing that row shifts content by 1 row at `scroll == 0`, and when scrolled up (`scroll > 0`) on an anchored bottom message (`anchor.id`), `Anchor` counts `below` backwards from `last` (`src/ui/timeline.rs:743-750`) and restores `last.saturating_sub(anchor.below).max(first)` (`line 620`), shifting that anchored message's text by 1 row when its standalone receipt row appears or disappears.

### 10. [LOW] Zero-width non-whitespace format chars bypass empty-name localpart fallback
- **Where:** `src/text.rs:35-42`; `src/matrix/receipts.rs:144-153`.
- **Detail:** `unsafe_char` strips C0/C1 controls and bidi marks/isolates/overrides (`U+200E`, `U+200F`, `U+061C`, `U+202A..=U+202E`, `U+2066..=U+2069`, `U+2028`, `U+2029`), and `clean_name` bounds width to `NAME_WIDTH = 40`. However, zero-width non-whitespace chars like `U+200B`, `U+200C`, `U+200D`, or `U+FEFF` are not stripped by `unsafe_char` or `str::trim()`, so a display name consisting solely of `"\u{200b}"` passes `name.trim().is_empty()` and renders blank (`"✓ "` / empty row) instead of falling back to `user.localpart()`.

### 11. [LOW] Test coverage gaps
- **Context menu (`src/menu.rs`, `src/app/menu.rs`, `src/ui/mod.rs`):**
  - `src/ui/mod.rs::tiny_sizes_do_not_panic` (`lines 463-495`) does not include `Modal::Menu` (nor `armed: Some(...)` or `"Again to delete"` truncation at narrow widths).
  - No tests cover `MouseEventKind::Moved` over an open menu (hover selection, disarming `Delete` on hover move, clearing background `app.hover`), `ScrollUp`/`ScrollDown` while `Modal::Menu` is open, right-clicking `HitId::Menu(_)`/`HitId::Overlay` when open, right-clicking when `open && !self.mouse`, right-clicking `HitId::Image`/`Reaction`/`Thread` or `SideRow::Section`, `SendState::Sending -> Sent` `"Discard"` / `MsgKind::Redacted` / `RoomKind::Favourite` in-place races, `visible_rooms()` filter mismatch in `perform_for_room`, `RoomKind::Invite` / `in_thread` menus in `app::menu::tests`, or clicking a scrolled menu entry (`menu.items.len() > shown`).
- **Read markers (`src/matrix/receipts.rs`, `src/ui/timeline/receipts.rs`, `src/app/thread.rs`):**
  - `Names::learn` and `Names::annotate` have no non-ignored unit tests (only hit in the `#[ignore]` homeserver test): missing coverage for `speakers` vs `known` precedence, `NAMES_MAX = 2000` eviction, and `fetch_members()` completion vs cached localparts.
  - No tests cover `Setting::ReadMarkers` + `"Read by…"` visibility, `Setting::ReadMarkers` in `Palette` / `SettingsMenu` config save, `thread.rs` `first.read_by` clearing/thread isolation, image-row + read-marker overlap math, or `scroll > 0` anchor behavior when a receipt spills onto a standalone row.

---

## Verification

- Checked `git -C /home/tylerstyle/git/matrix-rat status --short` (no tracked source edits in main checkout) at `4f23c15`.
- Ran `cargo test --quiet --manifest-path /home/tylerstyle/git/matrix-rat/Cargo.toml`:
  - `389 passed; 0 failed; 16 ignored; 0 measured; 0 filtered out` (the 16 ignored tests are server-backed integration tests).

---

## Verdict

Ready for follow-up fixes rather than blocking `main`: core target-by-ID tracking, two-step `Delete` / `RoomInfo` `Leave` confirmation, backdrop/mouse-off gating, edge placement, thread receipt isolation, and name sanitization are solid. Prioritise fixing the `SendState::Sending -> Sent` `"Discard"` race, the `Target::Room` `visible_rooms()` mismatch, wiring `Setting::ReadMarkers` into `"Read by…"` + the command palette, and trimming the per-diff work / `NAMES_MAX` thrashing in `matrix::receipts`.
