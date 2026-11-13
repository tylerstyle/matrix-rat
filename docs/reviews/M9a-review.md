# Review: M9a — Spaces (narrowing the room list)

**Commit reviewed:** `e9d99ce` (`Spaces: narrow the room list to a space`) on `main`  
**Files inspected:** `src/matrix/spaces.rs`, `src/matrix/mod.rs`, `src/action.rs`, `src/keymap.rs`, `src/palette.rs`, `src/app.rs`, `src/ui/sidebar.rs`, and snapshot diffs in `src/snapshots/`.

---

## Summary

The feature is cleanly structured and fits the rest of the codebase well: `SpaceService` runs inside the Matrix task and pushes summarized hierarchies over `MatrixEvent::Spaces`, space names are sanitized with `sanitize_line` before reaching the UI, `MatrixEvent`'s `Debug` impl logs only the space count (`Spaces(N)`), logout clears `spaces` / `space` / `space_rooms` and ignores late `Spaces` events, and invitations stay visible regardless of the active space.

However, keying selection solely by space `room_id` breaks navigation whenever a sub-space belongs to more than one top-level space, `apply_spaces` resets the sidebar cursor and scroll whenever `"All rooms"` is active, and a couple of UI/test gaps remain.

---

## Findings

### 1. [HIGH] Shared sub-spaces under two top-level spaces trap `Alt-S` / `▸` navigation in a loop
- **Where:** `src/app.rs:747-792` (`space_name`, `select_space`, `step_space`), `src/app.rs:163-166`
- **Detail:** Matrix spaces form a DAG (`matrix_sdk_ui`'s `SpaceGraph`), and `SpaceService` emits a flat `Vector<SpaceFilter>` sorted by top-level space with its nested sub-spaces directly underneath (`level: 0`, then `level: 1`). If a sub-space `Sub` is a child of both top-level spaces `A` and `B`, `App::spaces` legitimately contains two `SpaceSummary` entries with the same `id` (`[A, Sub, B, Sub, C]`).
  `App` stores only `space: Option<String>` and locates the active entry everywhere with `.iter().position(|s| s.id == ...)`:
  - `step_space` (`src/app.rs:784-791`) looks up `at` by `id`, computes `next = (at + delta).rem_euclid(count)`, and calls `self.select_space(Some(self.spaces[next - 1].id.clone()))`.
  - When stepping from `B` (`at = 3`) onto the second `Sub` (`next = 4`), `select_space` (`src/app.rs:759`) resolves `Sub`'s `id` back to index `1` (the *first* `Sub`). The following step starts from `at = 2` (`1 + 1`), moves to `B`, then snaps back to the first `Sub` again.
- **Impact:** Any space after the second occurrence (`C` above), as well as wrapping forward to `"All rooms"`, becomes unreachable via `Alt-S` or clicking `▸`.
- **Recommendation:** Track the selected slot in `App` by index (or `(parent_or_index, id)`) so stepping addresses each row in `App::spaces` unambiguously, using `id` (matched preferentially by parent/position, then by `id`) only when reconciling an incoming `MatrixEvent::Spaces` update in `apply_spaces`.

### 2. [MEDIUM] `apply_spaces` resets sidebar `cursor` and `scroll` whenever `"All rooms"` is active (and leaves `cursor` unclamped when a space shrinks)
- **Where:** `src/app.rs:756-774` (`select_space`), `src/app.rs:794-803` (`apply_spaces`)
- **Detail:** `apply_spaces` unconditionally resets the cursor before checking whether a space was selected:
  ```rust
  let selected = self.space.take();
  let (cursor, scroll) = (self.sidebar.cursor, self.sidebar.scroll);
  self.select_space(selected);
  if self.space.is_some() {
      (self.sidebar.cursor, self.sidebar.scroll) = (cursor, scroll);
  }
  ```
  Inside `select_space`, lines `771-773` always set `self.sidebar.cursor = 0; self.sidebar.scroll = 0; self.sidebar.reveal_cursor = true;`. Because `if self.space.is_some()` is false when the user is on `"All rooms"` (`selected == None`), *any* background space-list update yanks the sidebar cursor and scroll back to the top.
  Conversely, when `self.space.is_some()` and an update removes rooms from the selected space, restoring `(cursor, scroll)` does not clamp `self.sidebar.cursor` to `self.visible_rooms().len().saturating_sub(1)` (`cursor_room()` clamps on read, but `self.sidebar.cursor` stays past the end for subsequent cursor movement).
- **Recommendation:** Preserve `(cursor, scroll)` when `selected.is_none()` as well (only resetting on manual `select_space`, or resetting only when a previously selected space disappears), and clamp `self.sidebar.cursor` against `visible_rooms().len()`.

### 3. [MEDIUM] Empty spaces report `" no rooms yet"`, and narrow/short sidebars hide the active filter indicator
- **Where:** `src/ui/sidebar.rs:66, 122-131`, `src/app.rs:338-358`
- **Detail:**
  1. When `rows.is_empty()` (`src/ui/sidebar.rs:122-131`), the empty-state check only inspects `app.sidebar.filter.is_empty()` and displays `" no rooms yet"`. Selecting a space that currently has no joined rooms or invites therefore tells the user they have no rooms at all rather than something like `" no rooms in this space"`.
  2. When `inner.height <= 3 || inner.width < 8` (`src/ui/sidebar.rs:66`), the space header row is omitted so the room list still gets vertical space, which is sensible—except `App::visible_rooms` still narrows the list by `self.space`, leaving no visual cue or mouse control indicating why rooms are hidden.
- **Recommendation:** Check `app.space.is_some()` in the `rows.is_empty()` branch, and consider whether narrow/short mode should either leave a minimal cue (when `height <= 3`) or document relying on `Alt-S`.

### 4. [LOW] Navigation symmetry, sub-space hierarchy cue, and command palette edge cases
- **Where:** `src/keymap.rs:208-215`, `src/palette.rs:423-443`, `src/ui/sidebar.rs:68-92`
- **Detail:**
  - **Backward stepping is mouse-only and missing from the Command Palette / Help:** `Action::SpaceNext` is bound to `Alt-S` (`src/keymap.rs:208-215`) and appears automatically in the Command Palette (`src/palette.rs:343-390`) and Help overlay, whereas `Action::SpacePrev` has no binding in `keymap::DEFAULT` and no contextual entry in `palette::collect_actions`.
  - **Palette availability when not in any space:** `action_enabled` (`src/palette.rs:423-443`) leaves `Action::SpaceNext` enabled even when `app.spaces.is_empty()`, triggering only a toast (`"You are not in any space"`, `src/app.rs:779`) if run.
  - **`SpaceSummary.level` is invisible in the header:** `src/ui/sidebar.rs:68-80` renders top-level spaces (`level == 0`) and nested sub-spaces (`level > 0`) identically (`" ◂ Team ▸"`), making it hard to tell why `"Work"` includes `"Team"` or where `"Team"` sits in the hierarchy.
  - **Redundant hit rect (`src/ui/sidebar.rs:82-92`):** Adding `HitId::Do(Action::SpaceNext)` on the entire `area` makes the third rect `(next_x, area.y, 2, 1)` redundant (note also that `" ◂ "` has a trailing space while `" ▸"` has none, though `" ▸"` sits cleanly after a truncated label).

### 5. [LOW] Missing unit tests
- **Where:** `src/matrix/spaces.rs`, `src/app.rs:3964-4015`
- **Detail:** `app::tests::spaces_narrow_the_room_list` passes and covers basic `Alt-S` stepping, `▸` click, top-level + child inclusion, invitations remaining visible, and clearing `space` when the selected space disappears. However, there is no offline test coverage for:
  - A shared sub-space ID appearing under two top-level spaces (Finding 1).
  - `Action::SpacePrev` (`◂` click) and wraparound backwards.
  - `apply_spaces` while `"All rooms"` is active (Finding 2), and cursor clamping when a selected space loses rooms.
  - `rows.is_empty()` copy while narrowed to an empty space, or `inner.height <= 3 || inner.width < 8`.
  - `matrix::spaces::summarize` (ANSI/control-character sanitization via `sanitize_line`, blank names falling back to `"Unnamed space"`, and `level`/`rooms` mapping)—currently only the `#[ignore]` server-backed test `matrix::spaces::tests::a_space_and_its_room_are_reported` touches `src/matrix/spaces.rs`. (Note: because `SpaceFilter` comes from `matrix_sdk_ui`, adding a small pure helper for name normalization would make that part directly unit-testable offline.)

---

## Verification

- `cargo test --manifest-path /home/tylerstyle/git/matrix-rat/Cargo.toml spaces_narrow_the_room_list` on `main`: passed (`1 passed; 0 failed; 299 filtered out`).

---

## Verdict

**Approve with changes** — please address Findings 1 and 2 (shared sub-space navigation loop and `apply_spaces` resetting cursor/scroll on `"All rooms"`), and ideally the `" no rooms in this space"` empty state (Finding 3) plus regression tests (Finding 5) before pushing.
