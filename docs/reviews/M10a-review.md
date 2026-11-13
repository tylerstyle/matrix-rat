# Review: M10a — Room previews

**Commit reviewed:** `96f289b` (`Room previews: latest message under each room, larger tiles and pictures` on local `main`)  
**Files inspected:** `src/matrix/mod.rs`, `src/matrix/convert.rs`, `src/app.rs`, `src/ui/sidebar.rs`, `src/ui/image.rs`, `src/images.rs`, `src/modal.rs`, `src/model.rs`, `src/config.rs`, `src/text.rs`, `src/ui/hit.rs`, `src/ui/modal.rs`, `src/ui/mod.rs`, `src/ui/snapshots/matrix_rat__ui__tests__room_previews.snap`, and upstream `matrix-sdk-0.19.1` / `matrix-sdk-ui-0.19.1` implementations of `listen_to_room` / `latest_event()`.

---

## Summary

The UI integration is tidy: `SideRow::Preview`, `Setting::RoomPreviews`, and `ImageStore::set_avatar_cells` keep hover/click hit-testing unified (`HitId::Room(index)` and `highlight(...)` apply across both lines of each room) and re-encode cached kitty avatars on toggle without re-downloading. `convert::preview` sanitises external text with `sanitize_line` (removing control characters and Unicode bidirectional overrides) and strips Matrix reply-fallback lines (`> `).

There is **one `[HIGH]` performance/scalability issue** in the Matrix worker loop, plus **three `[MEDIUM]`** issues covering scrolled/clipped two-line avatars, `reveal_cursor` / wheel scrolling math on two-line rows, and fallback preview consistency:

1. Calling `listen_to_room().await` up to `LATEST_ROOMS = 500` times and `latest_message(room, direct).await` inside `summarize(&rooms).await` on **every** `entries.next()` update stalls the worker `select!` loop and recomputes sender profile + event decryption/parsing across all rooms (`ROOM_PAGE = 10_000`) on every list diff — even when `room_previews = false`.
2. When a two-line room is partially scrolled out at the top or bottom of the sidebar, its kitty avatar disappears completely (and a top-clipped `Preview` row renders a blank colored 4-cell rectangle with no initial letter).
3. `reveal_cursor` in `src/ui/sidebar.rs` triggers `scroll = at.saturating_sub(1)` when `at == scroll` (and scrolls `Room(i)` off-screen at `list.height == 1..=2`), while single-row mouse wheel scrolling splits rooms across the top edge.
4. While a room is open, `room.preview` from `MatrixEvent::Rooms` shadows newer messages in `room.messages`; state events returning `None` fall back to stale loaded messages; and the `loaded` fallback in `src/ui/sidebar.rs` differs from `convert::preview`.

---

## Findings

### 1. [HIGH] Unbounded per-update `latest_event().await` cost and startup blocking in the Matrix worker loop
- **Where:** `src/matrix/mod.rs:42-44, 650, 668-687, 741-771, 840`
- **Detail:**
  1. On every `entries.next()` batch (`src/matrix/mod.rs:668-687`), the worker calls `summarize(&rooms).await` (`line 686`), which loops sequentially over **every** non-space room in `rooms` (up to `ROOM_PAGE = 10_000`, not limited to `LATEST_ROOMS = 500`) and awaits `latest_message(room, direct).await` (`line 840`). In `matrix-sdk-ui-0.19.1` (`LatestEventValue::from_base_latest_event_value`), `room.latest_event().await` awaits `Profile::load(room, &sender)` from state storage and `TimelineItemContent::from_event(room, timeline_event)` (decrypting encrypted events if needed and parsing timeline content) for every room on **every** `entries` diff — including diffs triggered by unread count ticks or `LatestEvents` updates themselves.
  2. Before summarizing, `lines 675-684` run up to 500 sequential `latest.listen_to_room(room.room_id()).await` calls (`RegisteredRooms::for_room` acquires `rooms.write().await` per uncached room and enqueues `LatestEventQueueUpdate::EventCache` work). All of `668-687` runs inline inside one `tokio::select!` branch, delaying `OpenRoom`, `Send`, `Paginate`, `Logout`, and `Shutdown` commands while 500 registrations + `N` `summarize` awaits execute.
  3. `previewed` (`HashSet` at `line 650`) is never pruned when rooms drop below the top `LATEST_ROOMS` or leave the list (`forget_room` is never called), so active `LatestEvents` listeners grow past `LATEST_ROOMS` over a session.
  4. None of the worker work (`listen_to_room` / `latest_message`) checks whether `room_previews` is enabled, so disabling Room previews only hides the second line in the UI while paying the full SDK/storage/crypto cost.
- **Recommendation:** Avoid calling `latest_message().await` for every room on every `entries.next()` update (cache per-room preview strings by room ID and recompute only for rooms touched by `diffs` or latest-event updates, capped consistently), avoid blocking the command `select!` loop with 500 sequential awaits, and pass `room_previews` configuration through so turning previews off skips the work.

---

### 2. [MEDIUM] Clipped rooms lose their kitty avatar at both top and bottom edges (and show a blank tile at the top)
- **Where:** `src/ui/sidebar.rs:103-106, 213-222, 279-287`; `src/ui/image.rs:63-90`
- **Detail:**
  1. In `SideRow::Room(index)` (`src/ui/sidebar.rs:215`), `if !wide` disables `draw_avatar` whenever `room_previews` is enabled, expecting `SideRow::Preview(index)` to draw the `4×2` image. Whenever `Room(index)` lands on the bottom visible row of `list` (`Preview(index)` is below the viewport), `Preview(index)` is skipped by `.take(height)` and no picture is drawn at all — reverting that room to the initial tile.
  2. Symmetrically, when `SideRow::Preview(index)` is the top visible row (`n == 0`, because `Room(index)` scrolled above the top), `if n > 0` (`line 280`) skips `draw_avatar`, leaving row `0` with a blank 4-column colored `"    "` rectangle (no initial letter and no avatar).
  3. This happens because `ui::image::draw_avatar` (`src/ui/image.rs:80`) hardcodes `skip = 0` in `render(frame, theme, encoded, area, 0)` (unlike `draw_inline`, which passes `skip` for partially scrolled-out images) and `on_mouse` wheel scrolling advances `sidebar.scroll` by 1 row even when rooms are 2 rows tall.
- **Recommendation:** Either add vertical `skip` cropping support to `draw_avatar` (drawing `(x + 1, rect.y, tile_cols, 1)` with `skip = 0` when bottom-clipped `Room` renders alone, and `skip = 1` when top-clipped `Preview` renders alone — taking care that Kitty placement IDs/verification match), or step/align scrolling to whole rooms (`lines`) where possible and avoid rendering half-cropped rooms without a fallback initial.

---

### 3. [MEDIUM] Sidebar `reveal_cursor` boundary and tiny-height math bugs with two-line rooms, plus single-row wheel scroll
- **Where:** `src/ui/sidebar.rs:107-121`; `src/app.rs:2560-2566`
- **Detail:**
  1. `if at < app.sidebar.scroll + 1` (`src/ui/sidebar.rs:113`) matches `at == app.sidebar.scroll` (when `SideRow::Room(i)` is already at row `0` of `list`). Under `room_previews = true` (`lines = 2`) with `list.height == 2`, revealing a cursor room whose `Room(i)` is at `scroll` sets `scroll = at.saturating_sub(1)`, pushing `SideRow::Preview(i)` off the bottom; with `list.height == 1`, it replaces `SideRow::Room(i)` with the preceding section header or previous room's `Preview`.
  2. When scrolling down on a 1-line-high sidebar (`height == 1`, `lines == 2`), `app.sidebar.scroll = (at + lines).saturating_sub(height)` (`line 117`) evaluates to `at + 1` (`SideRow::Preview(i)`), scrolling the selected room's name (`SideRow::Room(i)`) completely off the top of the viewport.
  3. Mouse wheel scrolling in `src/app.rs:2560-2566` unconditionally changes `self.sidebar.scroll` by `1` row (`saturating_sub(1)` / `+ 1`) regardless of `self.room_previews`, making single wheel ticks land on `SideRow::Preview` at the top row (triggering Finding 2) instead of scrolling whole rooms (`lines` rows). It also leaves `scroll` unclamped in `on_mouse` until `ui::sidebar::draw` clamps it at `line 121`.
- **Recommendation:** Fix `reveal_cursor` to check `if at < app.sidebar.scroll` (only subtracting `1` to reveal the section header when `height > lines`, and capping the downward adjustment with `.min(at)` so `SideRow::Room(i)` is never scrolled above row `0`), and advance `Pane::Sidebar` wheel scrolling by `if self.room_previews { 2 } else { 1 }` (clamping against the viewport bottom if needed).

---

### 4. [MEDIUM] Open-room staleness, state-event fallback mismatch, and divergent `loaded` preview formatting
- **Where:** `src/ui/sidebar.rs:254-267`; `src/matrix/convert.rs:47-82`
- **Detail:**
  1. **Untrusted text:** `convert::preview` (`src/matrix/convert.rs:71`) runs `sanitize_line` (stripping controls and bidi overrides in `src/text.rs:35-42`), `display_name` sanitizes non-empty display names (defensively, the fallback `user.localpart()` at `line 80` is not run through `sanitize_line`), and the `loaded` fallback in `src/ui/sidebar.rs:254-262` reads `m.sender` and `m.body` already sanitised in `event_message` (`src/matrix/convert.rs:306-307`). No un-sanitised message body or sender reaches `ui::sidebar`.
  2. **Stale preview on live messages:** When the currently open room receives a new message via `MatrixEvent::Timeline` (`apply_timeline`), `room.preview` still holds the previous value from `MatrixEvent::Rooms` until the next `Rooms` event arrives. Because `src/ui/sidebar.rs:263-267` checks `room.preview.as_deref().or(loaded.as_deref())`, the sidebar keeps showing the older `room.preview` instead of the newly arrived message in `room.messages`.
  3. **Fallback on state events:** If a room's actual latest event is a membership/profile/state event (`convert::preview` returns `""` -> `latest_message` returns `None`, so `room.preview` becomes `None`), `.or(loaded.as_deref())` falls back to displaying an older loaded message from `room.messages` instead of clearing.
  4. **`loaded` vs `preview()` divergence:** `loaded` (`src/ui/sidebar.rs:258-262`) only reads `m.body.lines().next()` without skipping blank leading lines or `> ` reply quotes (so leading newlines render `"alice: "` and replies render `"alice: > ..."`), uses `m.sender` instead of `"You: …"` when `m.own`, and conversely `convert::preview` skips genuine Markdown blockquotes (`line.starts_with('>')`) if a message consists solely of a quote.

---

### 5. [LOW] Tile padding asymmetry and test coverage gaps
- **Where:** `src/ui/sidebar.rs:12-28, 194-201`; `src/ui/mod.rs:182-191`
- **Detail:**
  1. `initial(&room.name)` returns a 2-column string (`"R "` for ASCII, `"SS"`/`"🐀"` for 2-column initials), so `format!(" {} ", initial(&room.name))` renders 1 leading space and 2 trailing spaces (`" R  "`) for ASCII initials vs `" 🐀 "` for width-2 initials. (`HitId::Room(index)` mouse hits and hover highlights cover both `Room` and `Preview` rows properly.)
  2. Current tests cover happy-path rendering (`ui::tests::room_previews`), settings toggling (`app::tests::settings_menu_by_keyboard_and_mouse`), and an ignored end-to-end test (`matrix::tests::rooms_come_with_their_latest_message`), leaving `convert::preview` variants/quoting/sanitisation, `reveal_cursor` at `list.height == 1..=2`, partially clipped `SideRow::Room`/`SideRow::Preview` avatar rendering, and `HitId::Room` clicks/hover on `SideRow::Preview` untested.

---

## Verification

- Confirmed `git status` is clean in `/home/tylerstyle/git/matrix-rat` (`HEAD` at `f61e666`).
- Ran `cargo test --quiet` in `/home/tylerstyle/git/matrix-rat`:
  - `344 passed; 0 failed; 15 ignored; 0 measured; 0 filtered out` (the 15 ignored tests are server-backed integration tests including `matrix::tests::rooms_come_with_their_latest_message`).

---

## Verdict

Ready for follow-up fixes rather than blocking the UI structure: the sidebar layout, hit targets, and kitty re-encoding on toggle are clean, **but** `[HIGH]` Finding 1 (running `listen_to_room` + `latest_event().await` across all rooms on every `entries.next()` tick) will degrade responsiveness on large accounts, and Findings 2–4 leave clipped avatars, short-sidebar scrolling, and live open-room previews rough at the edges.
