# Milestone M1 Review — `matrix-rat` (UI Shell on Demo Data)

- **Reviewer:** `matrix-agy`
- **Date:** 2026-11-12
- **Target:** Commit `49d7a43` (`M1: UI shell on demo data`) and M0 follow-up commits `3deb05a`, `2b01abe`
- **Verification:** Ran `./scripts/check.sh` independently (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, 47/47 unit + snapshot tests passing, binary builds cleanly). Inspected all 6 `insta` snapshots in [`src/ui/snapshots/`](../../src/ui/snapshots/) and all source modules.
- **Verdict:** **Approve with changes**

---

## Executive Summary

M1 is a high-quality, cohesive UI shell that fulfills the milestone deliverables and folds in all M0 review items (`SIGTERM`/`SIGHUP` handling, `TerminalGuard` activation flag ordering, `Ctrl-Z` suspend/resume via `signal_hook`, user-private runtime dir fallback, and case-insensitive `Ctrl-Q`).

Highlights of what works particularly well:
- **Mouse & HitMap model ([`src/ui/hit.rs`](../../src/ui/hit.rs), [`src/app.rs`](../../src/app.rs)):** Z-ordered reverse lookup with `pane_at` pass-through makes wheel scrolling scroll the hovered pane without stealing focus, while backdrop vs overlay clicks and splitter drag-resizing behave cleanly.
- **Context-scoped keymap & focus flow ([`src/keymap.rs`](../../src/keymap.rs), [`src/app.rs`](../../src/app.rs)):** The `Composer` ↔ `Timeline` up/down walk, multi-step `Esc` (`back()`), `Ctrl-C` draft-clear vs `Ctrl-Q` quit, and `Alt-1..3` / `Alt-arrows` pane jumps are intuitive and well-tested.
- **Theme engine ([`src/theme.rs`](../../src/theme.rs), [`themes/*.toml`](../../themes/)):** Semantic roles with palette fallbacks, non-fatal warnings surfaced via toast + log, and 256-color quantization (`to_256`) are clean and solid.

Below are the bugs, layout/mouse gaps vs `docs/PLAN.md`, and state-model hazards that will bite in M2/M3 if not addressed as M2 begins.

---

## Findings (Ordered by Severity)

### 1. [HIGH] Raw `usize` indices (`app.active`, `TimelineState::selected`, `ComposeMode::{Reply,Edit}`) will panic on empty room lists and corrupt state on async `VectorDiff` updates in M2/M3
**Where:** [`src/app.rs`](../../src/app.rs#L31-L38), [`src/app.rs`](../../src/app.rs#L79), [`src/app.rs`](../../src/app.rs#L192-L198), [`src/ui/timeline.rs`](../../src/ui/timeline.rs#L153), [`src/ui/composer.rs`](../../src/ui/composer.rs#L39)

**Problem:**
1. **Unconditional `self.rooms[self.active]` indexing:** `App::messages()`, `App::messages_mut()`, `ui::timeline::draw()`, and `ui::composer::draw()` index `self.rooms[self.active]` directly. In M1 `demo_rooms()` always has 7 rooms, but in **M2** the room list is empty before login, during initial sync, on a fresh account with 0 joined rooms, or when a filter/sync diff removes rooms. Any draw or key press with `rooms.is_empty()` will immediately panic.
2. **Dynamic reordering invalidates `usize` indices:** In M2, `RoomListService` reorders rooms whenever a message arrives in another room. If `app.active` is a `usize` slot index rather than a stable room identifier (`OwnedRoomId`), the focused room will silently switch underneath the user mid-sentence whenever another room moves above it.
3. **Message index shift on back-pagination in M3:** Similarly, storing `usize` in `TimelineState::selected`, `ComposeMode::Reply(usize)`, and `ComposeMode::Edit(usize)` will point at the wrong message as soon as back-pagination or sync inserts items at the front of the timeline. Also, `Action::Edit` ([`src/app.rs`](../../src/app.rs#L450)) and `delete_message` ([`src/app.rs`](../../src/app.rs#L331)) index `self.messages()[i]` without bounds checking.

**Suggested Change:**
- As M2 replaces `model.rs` with `RoomListService`, change `app.active` to `Option<OwnedRoomId>` (or a stable room key) and render a calm empty-state in `ui/timeline.rs` and `ui/composer.rs` when `active` is `None`.
- Use `.get(i)` instead of `self.messages()[i]` in `Action::Edit` and `delete_message`, and transition `selected` / `ComposeMode::{Reply,Edit}` to stable item IDs (`UniqueTimelineItemId`) in M3.

---

### 2. [MEDIUM] Switching rooms while editing a message leaks the old room's edited text into the new room's composer
**Where:** [`src/app.rs`](../../src/app.rs#L250-L261) (`App::open_room`)

**Problem:**
In `open_room`:
```rust
self.timeline = TimelineState::default();
self.compose_mode = ComposeMode::Normal;
self.sidebar.filter.clear();
self.set_focus(Pane::Composer);
```
If the user presses `e` to edit a message in room A (`self.composer.set_text(&body)` and `self.compose_mode = ComposeMode::Edit(i)`) and then clicks room B (or opens room B from the sidebar), `open_room` resets `self.compose_mode` to `ComposeMode::Normal` **without calling `self.cancel_compose()`**, leaving room A's old message text inside `self.composer` as a normal draft in room B. Pressing `Enter` then sends room A's edited message to room B.

**Suggested Change:**
- At minimum, call `self.cancel_compose()` in `open_room` instead of `self.compose_mode = ComposeMode::Normal`.
- Better yet (for M2/M3): store `(Composer, ComposeMode)` per room so switching rooms preserves each room's draft and reply/edit banner independently.

---

### 3. [MEDIUM] `Composer::layout` can exceed `max` width by 1 column when a wide character (CJK/emoji) follows a soft-wrap break after a single leading space
**Where:** [`src/composer.rs`](../../src/composer.rs#L116-L134)

**Problem:**
In `Composer::layout`:
```rust
if col + gw > max && i > start {
    let brk = last_space_end.filter(|b| *b > start).unwrap_or(i);
    lines.push(VisualLine {
        range: offset + start..offset + brk,
        hard_end: false,
    });
    start = brk;
    col = width(&paragraph[start..i]);
    last_space_end = None;
}
col += gw;
```
When `paragraph` starts with a single space followed by `max - 1` single-width characters and then a 2-column wide grapheme (`gw == 2`), e.g. `paragraph = " bbb🐀"` with `max = 4`:
1. At `"🐀"` (`i = 4, gw = 2`), `col` is `4`, so `col + gw = 6 > 4`.
2. It breaks at `brk = 1` (after `" "`), pushing `" "` (width 1) and setting `start = 1`, `col = width("bbb") = 3`.
3. It then unconditionally executes `col += gw` (`3 + 2 = 5 > max`), producing a `VisualLine` `"bbb🐀"` of **width 5** inside a 4-column composer!

**Suggested Change:**
After breaking at `brk`, check if `col + gw > max && i > start` still holds (which happens when the word after the space plus the current wide grapheme still exceeds `max`), and if so, emit a second hard-broken `VisualLine` for `offset + start..offset + i` with `start = i; col = 0;`:
```rust
if col + gw > max && i > start {
    let brk = last_space_end.filter(|b| *b > start).unwrap_or(i);
    lines.push(VisualLine {
        range: offset + start..offset + brk,
        hard_end: false,
    });
    start = brk;
    col = width(&paragraph[start..i]);
    last_space_end = None;
    if col + gw > max && i > start {
        lines.push(VisualLine {
            range: offset + start..offset + i,
            hard_end: false,
        });
        start = i;
        col = 0;
    }
}
```

---

### 4. [MEDIUM] Pasting or inserting `\t` (Tab) or control characters into `Composer` desynchronizes cursor column calculation from rendered output
**Where:** [`src/app.rs`](../../src/app.rs#L601-L616) (`on_paste`), [`src/composer.rs`](../../src/composer.rs#L44-L47) (`insert`)

**Problem:**
`unicode_width::UnicodeWidthStr::width("\t")` returns `0` (as `\t` is a C0 control character). If a user pastes text containing tabs (`\t`) or control characters (e.g. from code snippets or terminal output), `Composer::insert` stores raw `\t` bytes in `self.text`, causing `cursor_position` and line wrapping to treat `\t` as 0 columns wide while the terminal either advances to a tab stop or corrupts adjacent cells.

**Suggested Change:**
Sanitize input in `Composer::insert` (or `on_paste`) by expanding `\t` to spaces (e.g. `"    "`) and stripping C0/C1 control characters other than `\n`.

---

### 5. [LOW] Mouse-only reachability gaps in narrow mode (`width < 60`) and for selected-message actions
**Where:** [`src/ui/mod.rs`](../../src/ui/mod.rs#L35-L50), [`src/ui/timeline.rs`](../../src/ui/timeline.rs#L207-L225), [`src/ui/status.rs`](../../src/ui/status.rs#L51-L71), [`docs/PLAN.md`](../PLAN.md#L233)

**Problem:**
1. **Narrow mode (`width < 60`):** In [`narrow_chat.snap`](../../src/ui/snapshots/matrix_rat__ui__tests__narrow_chat.snap), when the sidebar auto-collapses, only `2 #rust` and `3 Message` are visible. A mouse user has no clickable button on screen to switch to `1 Rooms` (`Pane::Sidebar`).
2. **Selected-message actions:** [`docs/PLAN.md`](../PLAN.md#L233) states: *"On a selected message: `Enter` reply · `e` edit (own messages) · `r` react · `Del` delete (own) · `c` copy · `o` open link/image. The same actions appear as a clickable row on the selected message and in the right-click menu."* Currently, selecting a message with the mouse shows those hints in the status line ([`src/ui/status.rs`](../../src/ui/status.rs#L57-L64)), but neither the status-line hints nor an action bar on the selected message are registered in `HitMap` as clickable buttons.

**Suggested Change:**
1. Register the status-line action hints (or an inline action chip row on the selected message) in `HitMap` so clicking `reply`, `edit`, `react`, `delete`, or `back` works with the mouse.
2. When `narrow` (`area.width < NARROW_WIDTH`) is active and `app.focus != Pane::Sidebar`, render a clickable `[1 Rooms]` button in the timeline's top border or status line so mouse users can open the room list in narrow windows.

---

### 6. [LOW] `.fg(style.fg.unwrap_or_default())` resets foreground to `Color::Reset` instead of inheriting `Role::Text`, and composer draft dimming only checks `timeline.selected`
**Where:** [`src/ui/timeline.rs`](../../src/ui/timeline.rs#L37), [`src/ui/sidebar.rs`](../../src/ui/sidebar.rs#L130-L132), [`src/ui/composer.rs`](../../src/ui/composer.rs#L98-L102), [`src/ui/help.rs`](../../src/ui/help.rs#L65-L68)

**Problem:**
1. In `timeline.rs:37` and `sidebar.rs:131`, `style.fg.unwrap_or_default()` evaluates to `Color::Reset` whenever a custom theme's `[ui]` role omits `fg`. Setting `.fg(Color::Reset)` overrides the frame's base `Role::Text` foreground color with the terminal's raw default foreground (which breaks light themes in dark terminals). Leaving `fg: style.fg` (`Option<Color>`) inherits the underlying cell's foreground cleanly.
2. In `ui/composer.rs:98`, the non-empty draft is dimmed only when `app.timeline.selected.is_some()`, so when focus is in `Pane::Sidebar` (where typing goes into the room filter, not the composer), the composer draft stays full-brightness. Checking `if !focused` dims the draft whenever keys do not go to the composer.
3. In `ui/help.rs:65`, `HitId::Backdrop` is registered over `area`, but `area` is not visually dimmed before `Clear`ing `popup` (unlike the doc comment on `HitId::Backdrop`). Applying `Modifier::DIM` to `area` before drawing the popup makes the overlay stand out clearly from the panes behind it.

---

## Verdict

**Approve with changes.** Milestone **M1 (UI shell with fake data)** is approved. Fold Findings 1–6 into the start of **M2 (Login + sync + room list)** (or a quick pre-M2 cleanup commit) and proceed to M2.
