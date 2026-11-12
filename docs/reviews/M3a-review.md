# Milestone M3 (Part 1) Review (`6c8e3c6`)

**Reviewer:** `matrix-agy`  
**Target:** Commit `6c8e3c6cc5e02e98acfc22ee053a878de7c10bb0` (`M3 (part 1): message timeline and sending`)

---

## Verification Summary

- `./scripts/check.sh` passes cleanly (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, 76 unit and snapshot tests).
- Both end-to-end integration tests (`password_login_sync_and_restore` and `send_and_see_it_confirmed`) pass in 1.85s against the local `continuwuity` homeserver (`http://127.0.0.1:6167`).
- Audited `src/matrix/{rooms,convert,mod,session}.rs`, `src/format.rs`, `src/app.rs`, `src/ui/{timeline,composer,mod}.rs`, and `src/model.rs` against `matrix-sdk-ui 0.19.1` in `~/.cargo/registry`.
- Confirmed that all 5 findings from the M2 review were cleanly addressed in `68e6bb1`.

---

## Assessment of the M3 Part 1 / Part 2 Split

Splitting M3 into Part 1 (live SDK timeline, pagination, send/reply/edit/react/redact/receipts/invites, Markdown + slash-command formatting) and Part 2 is a sensible way to keep changes reviewable, **provided M3 is only signed off as complete once Part 2 lands**:
1. **Received HTML rendering + spoiler cover/reveal** -> M3 Part 2 (must include hiding `<span data-mx-spoiler>` until clicked/`Enter` per `PLAN.md` §6a, since `format.rs` puts `||…||` in `plain` and spoiler tags in `html`).
2. **Timeline line cache & virtualization** -> M3 Part 2 (`PLAN.md` §4).
3. **`unique_id` scroll anchoring** -> M3 Part 2 (`PLAN.md` §4).
4. **Retry / discard UI for `SendState::Failed` local echoes** -> M3 Part 2 (`PLAN.md` §4).
5. **Outgoing typing notices (`room.typing_notice`)** -> M3 Part 2 (`PLAN.md` §9).
6. **Images** -> Milestone M5 per `PLAN.md` §10.

---

## Findings (Ordered by Severity)

### 1. [HIGH] Read receipts are sent when the user has not seen the room, spammed on every timeline diff, and never sent when scrolling back to the bottom
**Location:** `src/app.rs:304-305, 499-503`, `src/matrix/rooms.rs:223-231`

- **Problem:**
  1. **Marks rooms read before the user sees them:**
     In `App::apply_timeline` (`src/app.rs:500-502`):
     ```rust
     if at_bottom && let Some(room_id) = self.room_id() {
         self.send_matrix(MatrixCmd::MarkRead { room_id });
     }
     ```
     On startup (or when the open room vanishes), `apply_rooms` automatically selects the first joined room and calls `request_timeline()` with `timeline.scroll == 0` (`at_bottom == true`). Even if the Recovery modal (`Modal::Recovery`) is covering the screen, or the terminal is in narrow mode showing only the room list (`screen.width < NARROW_WIDTH && focus == Pane::Sidebar`, where `chat_area` is `None` and the timeline is not drawn at all), `apply_timeline` immediately sends `MatrixCmd::MarkRead` as soon as the first snapshot arrives.
  2. **Fires `MarkRead` on every timeline diff while `scroll == 0`:**
     Every `MatrixEvent::Timeline` update (sender profile load, someone else's reaction or receipt, back-pagination in a short room, local echo update) triggers `apply_timeline`, which unconditionally sends `MatrixCmd::MarkRead` and spawns a task calling `timeline.mark_as_read(ReceiptType::Read).await`.
  3. **Never marks the room read when the user scrolls back down to the bottom:**
     If messages arrive while the user is scrolled up (`timeline.scroll > 0`), `at_bottom` is `false`, so `MarkRead` is not sent. When the user later scrolls down to `scroll == 0` (via `PageDown`, mouse wheel `ScrollDown`, `SelectNewer`, or `JumpToNewest`), no `MatrixEvent::Timeline` is emitted, so `MarkRead` is **never** sent.
  4. **`open_room` does not clear `room.bold` (`src/app.rs:304-305`):**
     `open_room` clears `room.unread = 0` and `room.mention = false`, but leaves `room.bold = true` until the next room-list diff.

- **Suggested Fix:**
  - Track `last_marked_read: Option<String>` on `TimelineState` (storing the last remote `ItemRef::Event(id)` for which `MarkRead` was sent in the current room).
  - Extract a `maybe_mark_read(&mut self)` method and call it from `apply_timeline`, after scroll-changing actions (`PageDown`, `JumpToNewest`, `SelectNewer`, mouse wheel), and when closing a modal or leaving `Pane::Sidebar` in narrow mode.
  - In `maybe_mark_read`, only send `MatrixCmd::MarkRead` when:
    1. `self.timeline.scroll == 0`,
    2. `self.modal.is_none()` and `!(self.screen.width < ui::NARROW_WIDTH && self.focus == Pane::Sidebar)`, and
    3. The latest `ItemRef::Event(id)` in `self.messages()` differs from `self.timeline.last_marked_read`.
  - Also set `self.rooms[index].bold = false;` in `open_room`.

---

### 2. [HIGH] Pagination state is lost across room switches (`VirtualTimelineItem::TimelineStart` dropped), and `Paginate` can run concurrently or loop on filtered batches
**Location:** `src/matrix/convert.rs:16-39`, `src/matrix/rooms.rs:149-174`, `src/app.rs:306, 446-461, 983-991`, `src/ui/timeline.rs:264-278, 309-310`

- **Problem:**
  1. **`VirtualTimelineItem::TimelineStart` is discarded by `convert()` (`src/matrix/convert.rs:27-36`):**
     In `matrix-sdk-ui 0.19.1`, when a timeline reaches the start of the room (either via `paginate_backwards` or from the `EventCache` on `init_focus`), the SDK inserts `VirtualTimelineItem::TimelineStart` at the front of `items` (`item.is_timeline_start()`). Because `convert()` drops `TimelineStart` and `open_room` resets `self.timeline = TimelineState::default()` (`reached_start: false`), switching back to a cached room that fits on one screen (`top == 0`) always sets `want_older = true`, flashes `" loading older messages… "`, and spawns a redundant `Paginate` call on every room switch.
  2. **Concurrent `paginate_backwards` tasks on the same cached `Arc<Timeline>` (`src/matrix/rooms.rs:149-174`):**
     `Timelines::close()` aborts `self.forwarder`, but `MatrixCmd::Paginate` spawns an untracked task via `self.with_timeline(...)`. If a user switches away from Room A while its `paginate_backwards` request is in flight and switches back to Room A before it finishes, the new generation starts with `loading_older = false` and immediately dispatches a second concurrent `paginate_backwards` on the same `Arc<Timeline>`.
  3. **Unbounded pagination loop when batches contain only non-rendered events or when `top == 0` stays true:**
     When `MatrixEvent::Paginated { reached_start: false }` arrives (`src/app.rs:983-991`), it sets `loading_older = false` and `self.dirty = true`. On the immediate redraw, if `top == 0` is still true (which happens whenever `rows.len() <= height`, or when all 40 events in the paginated batch were state/aggregation events filtered out by `convert()`, or when `live_lazy_paginate_backwards` returned `Ok(false)` without growing `rows` past `height`), `draw` sets `want_older = true` and `after_draw` fires another `Paginate` on the very next frame without any user scroll input.

- **Suggested Fix:**
  - Check `items.iter().any(|item| item.is_timeline_start())` in `convert` / `forward` and pass `reached_start: bool` inside `MatrixEvent::Timeline`, setting `self.timeline.reached_start |= reached_start` in `on_matrix`.
  - Store the in-flight pagination `JoinHandle` on `Timelines` (e.g. `paginator: Option<JoinHandle<()>>`) and abort it in `Timelines::close()` alongside `self.forwarder`.
  - Avoid auto-retriggering `Paginate` purely from `MatrixEvent::Paginated { reached_start: false }` when `rows.len() > height` (i.e. once the viewport is already full of messages, require an actual scroll/resize or track that a batch yielded 0 new visible messages so it does not spin in a frame loop).

---

### 3. [MEDIUM] Plain multi-paragraph messages send redundant HTML, and `/spoiler` mangles multiline/multi-paragraph input
**Location:** `src/format.rs:56-61, 93-123, 133-148`

- **Problem:**
  1. **Multi-paragraph plain text sends `org.matrix.custom.html` (`src/format.rs:109-122`):**
     `markdown_html` only strips `<p>…</p>` when `html.matches("<p>").count() == 1`. If a user types two plain-text paragraphs separated by a blank line (`"first\n\nsecond"`) with no Markdown formatting, comrak outputs `"<p>first</p>\n<p>second</p>"`. Because `<p>` is not stripped when `count == 2`, `without_breaks.contains('<')` is `true`, so every multi-paragraph plain-text message is sent with a redundant `formatted_body`—contradicting `PLAN.md` §6a (*"If the text contains no formatting, only a plain `body` is sent (no redundant HTML)"*).
  2. **`/spoiler` drops line breaks on plain multiline input and emits invalid `<span data-mx-spoiler><p>…</p></span>` on multi-paragraph input (`src/format.rs:56-61`):**
     ```rust
     "spoiler" => needs_text(Outgoing::Text {
         plain: format!("||{argument}||"),
         html: Some(format!(
             "<span data-mx-spoiler>{}</span>",
             markdown_html(argument).unwrap_or_else(|| escape(argument))
         )),
     }),
     ```
     - If `argument` is `"line 1\nline 2"` (no Markdown), `markdown_html` returns `None`, and `escape(argument)` produces `"line 1\nline 2"` without `<br />`, which HTML renderers collapse to a single space.
     - If `argument` has `\n\n`, `markdown_html` returns `<p>…</p>\n<p>…</p>`, wrapping block `<p>` tags inside an inline `<span data-mx-spoiler>`.
  3. **`/html` with tag-only input or block tags (`src/format.rs:52-55, 133-148`):**
     - `/html <b>   </b>` passes `!argument.is_empty()`, but `strip_tags(argument)` produces an empty `plain` body (`""`).
     - `strip_tags("<p>one</p><p>two</p>")` concatenates `"onetwo"` with no whitespace/newline for `<br>` or `</p>`.

- **Suggested Fix:**
  - In `markdown_html`, to test whether the message actually used formatting beyond paragraphs and line breaks, check `without_breaks` after also removing `<p>` and `</p>\n` / `</p>`: if no other HTML tags remain (and no backslash-escaped characters changed the text), return `None`.
  - In `/spoiler`, convert newlines in the fallback (`escape(argument).replace('\n', "<br />\n")`) and replace paragraph breaks with `<br /><br />` inside `<span data-mx-spoiler>`.
  - In `strip_tags`, translate `<br>` / `</p>` into `\n` and reject `/html` if `strip_tags(argument).trim().is_empty()`.

---

### 4. [MEDIUM] Editing an emote (`/me`) turns it into a normal text message, and starting an edit discards any unsent draft
**Location:** `src/app.rs:360-400, 691-708`, `src/matrix/rooms.rs:197-208`

- **Problem:**
  1. **Editing an `Emote` loses `MsgKind::Emote` (`src/app.rs:700-704`):**
     For an emote (`* alice waves`), `message.body` stores `"waves"` and `message.kind` is `MsgKind::Emote`. When the user selects their own emote and presses `e` (`Action::Edit`), `self.composer.set_text(&body)` puts `"waves"` (without `/me `) into the composer. Pressing `Enter` parses `"waves"` as `Outgoing::Text`, so `MatrixCmd::Edit` sends an `m.text` replacement instead of `m.emote`, converting the emote into a regular message! (Conversely, in demo mode, `target.body = message.body` never updates `target.kind`, so editing a text message to `/me waves` in demo mode stays `MsgKind::Text`.)
  2. **Starting an edit clobbers the current room's unsent draft (`src/app.rs:700-704, 588-593`):**
     If the user has typed a draft in the composer, navigates up to an earlier message, presses `e` to edit it, and then either sends the edit or presses `Esc` (`cancel_compose`), the unsent draft they were writing is permanently lost.

- **Suggested Fix:**
  - When entering `ComposeMode::Edit(i)`:
    - Save the current composer text into `self.rooms[self.active].draft` (if not already in `Edit` mode), and restore it when `cancel_compose()` or `send()` finishes the edit.
    - If `message.kind == MsgKind::Emote`, prefill the composer with `format!("/me {}", message.body)`.
    - In demo mode `send()`, update `target.kind = kind;` alongside `target.body = message.body;`.

---

### 5. [MEDIUM] `JumpToNewest`, `delete_message`, and clicking non-message timeline rows have selection / scroll edge cases
**Location:** `src/app.rs:269-277, 580-585, 639-644, 1306-1311, 1336`

- **Problem:**
  1. **`JumpToNewest` and `delete_message` can select a `MsgKind::System` or `MsgKind::ReadMarker` item (`src/app.rs:641-643, 583`):**
     - `Action::JumpToNewest` sets `self.timeline.selected = Some(self.messages().len() - 1)` directly instead of `self.find_selectable(usize::MAX, false)`.
     - `delete_message` sets `self.timeline.selected = Some(index.min(self.messages().len() - 1))` directly instead of searching with `find_selectable`.
     - If the last item in `self.messages()` is a `MsgKind::System` event (e.g. `"bob joined"`), `selected` points to a non-selectable system item (which has `row.msg == None` in `build_rows`, so nothing highlights, yet `Reply`/`React` will target the state event).
  2. **Clicking a system line, date divider, or empty space in the timeline yanks the scroll back to the bottom (`src/app.rs:272-277, 1336`):**
      Rows with `row.msg == None` fall through to `HitId::Pane(Pane::Timeline)`, which calls `self.set_focus(Pane::Timeline)`. `set_focus(Pane::Timeline)` sets `self.timeline.selected = self.find_selectable(usize::MAX, false)` (the newest message at the very bottom) and `self.timeline.reveal_selected = true`. Thus, if the user scrolls up with the mouse wheel and clicks on a system message or date divider to focus the timeline pane, the viewport immediately jumps all the way back to the bottom!

- **Suggested Fix:**
  - In `Action::JumpToNewest`, use `self.timeline.selected = self.find_selectable(usize::MAX, false);`.
  - In `delete_message`, use `self.find_selectable(index, false).or_else(|| self.find_selectable(index, true))`.
  - Only set `self.timeline.reveal_selected = true` in `set_focus(Pane::Timeline)` when `self.timeline.scroll == 0` (or when navigated to via keyboard `SelectOlder`), so clicking blank/system rows in a scrolled-up timeline focuses the pane without yanking the scroll position.

---

### 6. [LOW] Status markers (`◌ sending`, `✕ not sent`, `(edited)`) split across lines in `build_rows`, missing `fetch_details_for_event`, and `MatrixEvent` `Debug` privacy
**Location:** `src/ui/timeline.rs:135-167`, `src/matrix/rooms.rs:288-330`, `src/matrix/mod.rs:152-188`

- **Problem:**
  1. **Status markers split across wrapped lines and lose their style (`src/ui/timeline.rs:151-167`):**
     `build_rows` appends `label` (`"◌ sending"`, `"✕ not sent"`, `"(edited)"`) to `text` *before* calling `wrap(&text, body_width)`, and then checks `line.strip_suffix(label)` on the last line. Because `wrap()` splits on `split_word_bounds()`, any message whose text ends near `body_width` wraps inside the marker (e.g. `"◌"` at the end of line `n - 1` and `"sending"` on line `n`, or `"(edited"` and `")"`). When that happens, `line.strip_suffix(label)` returns `None` and the marker is rendered in `body_style` instead of `Role::Muted` / `Role::Error`!
  2. **Replies to older (unloaded) events stay at `"┃ an earlier message"` (`src/matrix/convert.rs:106-119`, `src/matrix/rooms.rs:288-330`):**
     In `matrix-sdk-ui 0.19.1`, `in_reply_to.event` starts as `TimelineDetails::Unavailable` when the replied-to event is not in memory, and is only resolved if `timeline.fetch_details_for_event(event_id)` is called.
  3. **`MatrixEvent` and `Outgoing` derive `Debug` with cleartext message bodies (`src/matrix/mod.rs:152-188`, `src/format.rs:9`):**
     `MatrixCmd` redacts message bodies in its custom `Debug` impl (`// Message text is private too`), but `MatrixEvent::Timeline` derives `Debug` and dumps all `Message` bodies.

- **Suggested Fix:**
  - In `build_rows`, wrap `text` *without* the marker first; then if `width(&last_line) + 1 + width(label) <= body_width`, append `Span::raw(" ")` and `Span::styled(label, theme.style(role))` to the last line, or push a new indented row for the marker alone.
  - In `forward()` (`src/matrix/rooms.rs`), spawn `timeline.fetch_details_for_event(event_id)` for any remote event item whose `in_reply_to.event` is `TimelineDetails::Unavailable`.
  - Implement a custom `Debug` for `MatrixEvent` that prints `Timeline { room_id, generation, count: messages.len() }` instead of full message contents.

---

## Verdict

**Approve with changes.**

M3 Part 1 brings the client to life with real Matrix timelines, back-pagination, local echoes, replies, edits, reactions, redactions, invites, and Markdown/slash-command formatting. Fixing the read-receipt trigger logic (#1), pagination `TimelineStart`/task lifecycle (#2), and the smaller formatting/editing/selection issues (#3–#6) will make this foundation rock-solid for M3 Part 2.
