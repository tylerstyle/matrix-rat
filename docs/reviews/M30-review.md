# M30 Review — Space Management

- **Branch / Worktree:** `spaces` (`.claude/worktrees/spaces`, `git diff main...spaces`: 3 commits `215ea84`, `4e9dd8c`, `fdff607`; 25 files, `+3643/-130`)
- **Review Scope:** Static code & diff review only (no `cargo build` or `cargo test` run here; `hpfury` reported 854 unit tests green on the branch).
- **Verdict:** **Changes requested (targeted fixes, no architectural issues).** The core state design, raw empty-content `m.space.child` removal, real-ID state keys, `Debug`/`describe()` privacy discipline, and demo integration are solid. There are eight functional/edge-case issues worth addressing plus a few minor polish notes, and both pre-existing bugs flagged by the helper are confirmed.

---

## Findings (Should-Fix)

### 1. `"Remove from <space>…"` is offered when `!can_edit`
- **Palette (`Ctrl-P`):** `src/palette.rs:1219-1226` destructures `if let Some((space, _)) = &facts.selected` and sets `enabled` to `in_room && facts.in_selected`, ignoring `can_edit` (`_`), unlike `"Add a room to {name}…"` and `"Add {name} to a space…"` which gate on `*can_edit` / `can_add_room`.
- **Room context menu:** `src/app/menu.rs:200-205` (`open_room_menu`) and `src/menu.rs:646-653` (`room_items`) offer `"Remove from …"` whenever `in_space` is `Some`, regardless of `space.can_edit`. Both simply jump to `InfoRow::RemoveFromSpace` in `RoomInfo`, where `src/ui/roominfo.rs:1411-1417` then tells the user `"Read-only: you are not allowed to change the rooms of …."`.
- **Fix:** Gate `"Remove from …"` on `can_edit` in `palette.rs` and `RoomSpaces` / `open_room_menu` so commands/menus never offer an action the server will refuse (and consider hiding or clearly disabling non-editable space-edit actions consistently).

### 2. `may_edit` uses `Room::power_levels()` instead of `power_levels_or_default()`
- `src/matrix/spaces.rs:220-225`: `may_edit` calls `space.power_levels().await.is_ok_and(...)`.
- In `matrix-sdk-base-0.19.1/src/room/mod.rs:392-419`, `Room::power_levels()` returns `Err(Error::InsufficientData)` if `m.room.power_levels` is missing from state or `creators()` is `None`, whereas `power_levels_or_default()` (used by `src/matrix/manage.rs:2465` for `RoomDetails`) constructs `RoomPowerLevels::new(RoomPowerLevelsSource::None, ...)` and grants room creators default state-event permissions.
- **Fix:** Use `space.power_levels_or_default().await` in `may_edit` to match `RoomDetails`.

### 3. `link_parent`: redundant `{}` writes on remove and overwriting `canonical` on add
- **Remove (`SpaceEdit::Remove`):** `src/matrix/spaces.rs:529-543` checks whether `room.get_state_event_static_for_key::<SpaceParentEventContent, _>(space.room_id()).await` returns `Some(...)`. Because `get_state_event_static_for_key` (`matrix-sdk-0.19.1/src/room/mod.rs:1454-1471`) returns `Option<RawSyncOrStrippedState<SpaceParentEventContent>>`, an already-emptied (`{}`) or redacted `m.space.parent` event still returns `Some`, causing a redundant `{}` write (contrast `child_content` at `src/matrix/spaces.rs:496-507`, which deserializes and checks `.as_original()`).
- **Add (`SpaceEdit::Add`):** `src/matrix/spaces.rs:523-527` writes a fresh `SpaceParentEventContent` without reading any existing `m.space.parent` for `space.room_id()`, which silently resets `canonical: true` if that parent was already marked canonical on the child room (and any failure or missing `SpaceParent` power silently leaves `m.space.child` set without `m.space.parent`).
- **Fix:** Deserialize/check `.as_original()` in `link_parent` (skip removing when already empty/redacted, and preserve `canonical` when adding/updating an existing parent link).

### 4. Previewing / joining a child space does not check `self.spaces` or select the opened space
- `src/app/rooms.rs:794-807` (`PreviewButton::Open`), `src/app/rooms.rs:2051-2055` (`finish_open_or_knock`), and `src/app/rooms.rs:2095-2104` (`ManageEvent::Preview` `membership_override`) look only in `self.rooms`, whereas `src/matrix/mod.rs:2167` excludes all spaces from `self.rooms` (spaces live in `self.spaces`).
- **Impact:** Previewing an already-joined space falls back to `PreviewMembership::None`; `PreviewButton::Open` does nothing for spaces; and joining a space via preview arms `pending_open` (looking in `self.rooms`) instead of `pending_space`.
- **Fix:** Check `self.spaces` alongside `self.rooms` in `membership_override`, switch/narrow to the space on `PreviewButton::Open`, and arm `pending_space` when opening/joining a space from preview.

### 5. `Modal::RoomInfo` for a left/kicked subspace stays open forever
- `src/app/spaces.rs:122-145` (`follow_spaces`): `deeper` checks `self.spaces.iter().any(|space| space.children.contains(&room_id))`, which remains `true` even after leaving or being kicked from subspace `room_id` if a joined parent space still retains its `m.space.child` event (and depth >= 2 subspaces already have `listed == false`).
- Meanwhile, `src/matrix/manage.rs:2460-2462` (`watch`) returns silently when `room.state()` transitions to `RoomState::Left | RoomState::Banned`, without closing or signalling `RoomInfo`.
- **Fix:** Have `watch()` emit a left/error update when the watched room leaves/bans, or verify membership before treating `deeper` as still active.

### 6. Member filter vs `Rooms` (`Child`), `Space`, and `Del` keys in `RoomInfo`
- **`typing_blind` vs `Child` / `Del`:** `src/ui/roominfo.rs:1165-1185` triggers `typing_blind` whenever `!info.filter.is_empty()`, `info.editing.is_none()`, and the selection is not an `InfoRow::Member(_)`. Keyboard navigation up to `Child(_)` keeps scroll locked to `Entry::Members`, scrolling the selected child row off-screen while `src/app/rooms.rs:1193` still allows `KeyCode::Delete` (even with modifiers, since `Delete` is not guarded by `plain`) to call `remove_child()`.
- **`Space` when member filter matches zero members:** `src/app/rooms.rs:1175-1184` handles `KeyCode::Char(' ')` on `InfoRow::SettingsToggle | InfoRow::Setting(RoomSetting::Direct)` without `&& info.filter.is_empty()`. Because `after_filter_change` (`src/manage.rs:1595-1608`) falls back to clamping selection onto the last non-member row when no members match (`SettingsToggle`, or `Direct` if settings are open), typing `' '` in the filter toggles `Room settings` (or `Direct chat` in regular rooms) instead of appending `' '` to `info.filter`. (By contrast, `src/app/rooms.rs:1195-1199` for `InfoRow::Child(_)` properly requires `&& info.filter.is_empty()`.)
- **Fix:** Gate `KeyCode::Char(' ')` on `SettingsToggle | Direct` with `&& info.filter.is_empty()` (and `plain`), require `plain` on `KeyCode::Delete`, and avoid locking scroll away from non-member rows selected via navigation.

### 7. Clicking outside a palette picker (`HitId::Backdrop`) drops return to `RoomInfo`
- `src/app.rs:3769-3797`: `HitId::Backdrop` directly sets `self.modal = None` for `Modal::Palette(_)`, bypassing `self.close_palette()`, so clicking the backdrop on `"Add a room…"` or `"Add to a space…"` discards the `InfoBack` return target instead of reopening `RoomInfo` as `Esc` does.
- **Fix:** Route `Modal::Palette(_)` in `HitId::Backdrop` through `self.close_palette()`.

### 8. Bounds: `EVENTS_MAX` precedes tombstone filtering; `/hierarchy` has no `limit` and up to 100 s timeout
- `src/matrix/spaces.rs:181-202` (`child_events`): applies `.take(EVENTS_MAX)` (`5_000`) *before* skipping empty `{}` / redacted `m.space.child` tombstones. A space with >5,000 accumulated tombstones could miss live children and report `more = false`.
- `src/matrix/spaces.rs:298-336` (`hierarchy_names`): runs up to `HIERARCHY_PAGES = 5` requests with `HIERARCHY_TIMEOUT = 20s` each (up to 100 s worst case) directly inside `watch()`, blocking `RoomInfo` updates while slow `/hierarchy` calls run, sets no `request.limit`, and inserts the depth-0 space itself toward the `CHILDREN_MAX = 500` `Named` cap.
- **Fix:** Move `.take(EVENTS_MAX)` after filtering empty/redacted tombstones, skip the root space itself in `hierarchy_names`, and set a reasonable `request.limit` / overall timeout cap so `watch()` stays responsive.

---

## Minor / Non-Blocking Notes

- **Initial loading label for spaces:** `src/ui/roominfo.rs:1060` displays `"Room ID"` instead of `"Space ID"` while `info.details` is `None`.
- **Empty picker copy:** `src/ui/palette.rs:135-144` renders `"  no matching commands"` when filtered results are empty in `Pick` mode.
- **`SpaceSummary.name` & `NewPopup` field bounds:** `src/matrix/spaces.rs:93` (`summary_of`) runs `sanitize_line` without `NAME_MAX` (`255`) truncation (unlike `clean_name`), and `src/manage.rs:589-590` limits `NewPopup.name` / `topic` to `FIELD_MAX = 1024` bytes rather than `NAME_MAX`.
- **`set_space_context` confirm reset:** `src/manage.rs:1361-1371` clears `Confirm::RemoveFromSpace` only when `in_space.id` changes, not if `can_edit` flips `true -> false` on the same space (execution still checks `can_edit`, so nothing can be sent).
- **Local `sanitize_line` at draw sites:** `src/ui/roominfo.rs:1408, 1414, 1760` (`in_space.name`) and `src/ui/palette.rs:74` (`about`) rely on upstream `SpaceSummary` sanitization rather than wrapping in `sanitize_line` locally.

---

## Focus-Area Verification Summary

| Focus Area | Status | Notes |
|---|---|---|
| **Rights checks & refusals** | Partial (Findings #1, #2) | Edit pills (` suggest ` / ` unsuggest ` / ` remove ` in `src/ui/roominfo.rs:1951-1961`), `child_hint` (`1534-1549`), `"Add a room…"`, and `AddToSpace` properly gate on `can_edit`; server refusals clear pending flags/toast cleanly via `failed()` without state corruption (modulo `link_parent` best-effort asymmetry). Fix `"Remove from …"` gating and `may_edit` fallback. |
| **Remove path** | Good (Finding #3) | Child removal sends empty `{}` via `send_state_event_raw` (`src/matrix/spaces.rs:472`), and `space_child_handler` (`src/matrix/mod.rs:1738-1748`) uses `Raw<SyncSpaceChildEvent>` so removals wake `spaces_poke` + `manage_refresh`. `link_parent` checks the child's own key for `space.room_id()`, needing only an `.as_original()` guard and canonical preservation. |
| **IDs as state keys** | Pass | Real room/space IDs flow untouched through `SpaceSummary`, `SpaceChild`, `PickItem`, and `ManageCmd::EditSpace` (`src/matrix/spaces.rs:267-268`); `sanitize_line`/`clean_name` are display-only. |
| **Bounds** | Partial (Finding #8) | `CHILDREN_MAX = 500`, `EVENTS_MAX = 5_000`, `NAME_MAX = 255`, `HIERARCHY_PAGES = 5`, `HIERARCHY_TIMEOUT = 20s`, `VIA_MAX = 5`, `INPUT_MAX = 2048`, `ID_MAX = 255`, `FIELD_MAX = 1024`. Adjust `EVENTS_MAX` placement and `/hierarchy` limits. |
| **Stale answers & popup lifetime** | Partial (Findings #5, #7) | Request IDs guard `CreateSpace`, `EditSpace`, and `RoomInfo` pending waits against late answers; `src/app/menu.rs:493-501` guards `Target::Space` against space switches. Address `follow_spaces` / `watch()` on left subspaces and palette backdrop click. |
| **`Space` / `Del` vs filter** | Partial (Finding #6) | `Child(_)` properly requires `info.filter.is_empty()` for `' '`; fix `SettingsToggle \| Direct`, `plain` on `Delete`, and `typing_blind`. |
| **Privacy (`Debug` / logs / `describe()`)** | Pass | `SpaceSummary`, `SpaceDetails`, `SpaceChild`, `SpaceRef`, `Confirm`, `Pick`, `PickItem`, `SpaceFacts`, `NewPopup`, `InfoPopup`, `ManageCmd`, `ManageEvent`, and `MatrixEvent::Spaces` omit IDs/names/topics/aliases/raw server errors in `Debug` and `tracing::warn!`. `.describe()` helpers are strictly `#[cfg(test)]`. |
| **`80x24` & `README.md`** | Pass (minor notes) | `NewPopup` and `child_row` (`src/ui/roominfo.rs:1966-1976`) clamp to small viewports and shed tags before buttons so labels do not collide; 6 snapshot files updated/added in the diff (`new_chooser`, `space_info_demo`, `space_room_picker`, `command_palette`, `command_palette_filtered`, `command_palette_narrow`); `README.md` accurately documents `Ctrl-N`, `Alt-S`, `space_info`, `"Add a room…"`, `Del` twice, `Space`, `Enter` open/preview, `"Add to a space…"` / `"Remove from"`, and `--demo`. |

---

## Assessment of Helper's Existing-Code Bug Reports

1. **Space invitations are invisible (`summarize` skips every space):** **Confirmed.**
   - `src/matrix/mod.rs:2167` skips every space (`if room.is_space() { continue; }`) in `summarize` before producing `RoomSummary` rows (including `Invited` / `Knocked`), while `spaces::summarize` (`src/matrix/spaces.rs:74-90`) reads from `SpaceService`, whose `build_space_state` (`matrix-sdk-ui-0.19.1/src/spaces/mod.rs:573-576`) only iterates `client.joined_space_rooms()`. Invited or knocked spaces are therefore invisible everywhere in the UI.
2. **In `src/app/rooms.rs`, a space typed into the member filter can toggle `'Direct chat'`:** **Confirmed.**
   - `src/app/rooms.rs:1175-1184` binds `KeyCode::Char(' ')` for `InfoRow::SettingsToggle | InfoRow::Setting(RoomSetting::Direct)` without `&& info.filter.is_empty()` (unlike `InfoRow::Child(_)` at `src/app/rooms.rs:1195-1199`). Because `after_filter_change` (`src/manage.rs:1595-1608`) clamps selection onto the last non-member row when a filter matches zero members (`SettingsToggle` or, when expanded, `Direct`), typing `' '` in that state toggles `Room settings` or `Direct chat` instead of filtering.

---

## Re-check (`e7f086b..HEAD`, 10 commits: `dc179ba..0faa468`)

**Verdict: Changes requested (one remaining gap in Finding #6 / `1f40100`).** Review only; no builds or tests run (`hpfury` reported 871 unit tests green). Findings #1–#5, #7, #8, existing-code bug #1 (`0e58d20` / `0faa468`), and all five minor notes (`a4add57`) are cleanly closed, and the `#[cfg(test)]` loopback `fake::Server` (`src/matrix/spaces.rs` `mod fake`) is sound. In `1f40100`, `Space`, `Left`, `Right`, and `Delete` are fixed, but `KeyCode::Enter` (plus `Alt-B` on `KnockRequest` and `Up`/`PageUp` from `selected == 0`) still miss `blind` guards.

### Findings & Notes Verification

1. **Finding #1 (`dc179ba` — `"Remove from <space>…"` gated on `can_edit`):** **Closed.**
   - [`src/app/menu.rs:200-207`](../../src/app/menu.rs#L200-L207) filters `in_space` on `space.can_edit` before populating `RoomSpaces.remove_from`.
   - [`src/palette.rs:1229-1236`](../../src/palette.rs#L1229-L1236) gates `"Remove {name} from …"` on `in_room && facts.in_selected && *can_edit`.
   - [`src/ui/roominfo.rs:1756-1768`](../../src/ui/roominfo.rs#L1756-L1768) keeps `InfoRow::RemoveFromSpace` in `RoomInfo`, marked `" (read-only)"` when `!in_space.can_edit`.
2. **Finding #2 (`51a008e` — `may_edit` power levels):** **Closed.**
   - [`src/matrix/spaces.rs:222-228`](../../src/matrix/spaces.rs#L222-L228) (`may_edit`) and [`src/matrix/spaces.rs:556-559`](../../src/matrix/spaces.rs#L556-L559) (`link_parent`) both use `power_levels_or_default().await`, tested against `fake::Server`.
3. **Finding #3 (`51a008e` — `link_parent` `.as_original()` & `canonical`):** **Closed.**
   - [`src/matrix/spaces.rs:536-575`](../../src/matrix/spaces.rs#L536-L575) adds `parent_content` (`Sync` -> `.as_original()`), skips `{}` writes on remove when `linked.is_none()`, and preserves `canonical` when adding.
4. **Finding #4 (`efd9f66` — previews of spaces):** **Closed.**
   - [`src/matrix/directory.rs:139, 321`](../../src/matrix/directory.rs#L321) sets `RoomPreview.space`.
   - [`src/app/rooms.rs:2127-2136`](../../src/app/rooms.rs#L2127-L2136) checks `self.spaces` (`PreviewMembership::Joined`) if absent from `self.rooms` (preserving `Invited` / `Knocked` from `0e58d20`).
   - [`src/app/rooms.rs:778-801`](../../src/app/rooms.rs#L778-L801) (`open_previewed`) selects listed spaces or opens `open_space_info` for deep joined spaces, and [`src/app/rooms.rs:2061-2087`](../../src/app/rooms.rs#L2061-L2087) arms `self.manage.pending_space` before clearing `popup.request`.
5. **Finding #5 (`545d5c8` — `ManageEvent::Gone`):** **Closed.**
   - [`src/matrix/manage.rs:2463-2473`](../../src/matrix/manage.rs#L2463-L2473) emits `ManageEvent::Gone { room_id }` and returns when the watched room reaches `RoomState::Left | RoomState::Banned`. Because `watch()` is only spawned for [`ManageCmd::Info`](../../src/matrix/manage.rs#L973-L987) (and aborted on `StopInfo` or a new `Info`), `Gone` cannot fire for unwatched rooms.
   - [`src/app/rooms.rs:2405-2415`](../../src/app/rooms.rs#L2405-L2415) (`info_gone`) ignores stale `room_id` values and refuses to close while `info.pending == Some(Pending::Leave)`, leaving our own leave to finish through `ManageEvent::Left`. Whichever fires first between `Gone` and `follow_room_list` / `follow_spaces` closes `self.modal = None` so the other does not double-toast.
6. **Finding #6 (`1f40100` — filter vs row keys, `InfoPopup.blind`, Direct-chat toggle):** **Partial — remaining bug:**
   - **Closed parts:** [`src/app/rooms.rs:1190-1200, 1212-1216`](../../src/app/rooms.rs#L1190-L1216) require `&& plain && info.filter.is_empty()` for `Char(' ')` on `SettingsToggle | Setting(RoomSetting::Direct)` and `Child(_)`; `Left`, `Right`, and `Delete` on `Notify` / `SettingsToggle` / `Member` / `Child` / `KnockRequest` require `plain && !blind` ([`1170-1188, 1206-1219`](../../src/app/rooms.rs#L1170-L1219)), and `EditKeys` ([`1237-1248`](../../src/app/rooms.rs#L1237-L1248)) sends arrows and `Delete` to `info.filter` when `blind` is true. `InfoPopup::step`/`select` and `manage_click` clear `blind = false` ([`src/manage.rs:1567, 1581`](../../src/manage.rs#L1567-L1581), [`src/app/rooms.rs:1328`](../../src/app/rooms.rs#L1328)).
   - **Should-fix — `KeyCode::Enter` is unguarded when `blind` is true ([`src/app/rooms.rs:1201-1205`](../../src/app/rooms.rs#L1201-L1205)):** Typing a non-matching filter when a non-member row is selected (including `InfoRow::Favourite` at `selected == 0` when opening room info, `Setting(RoomSetting::Direct)`, `Leave`, or `Child(_)`) sets `typing_blind() == true` and scrolls `Entry::Members` into view, yet pressing `Enter` still calls `self.info_activate(row, 0)` on the hidden row — sending `SetFavourite` / `SetDirect`, starting/confirming `Leave`, or opening/previewing a hidden `Child`. Gate `KeyCode::Enter` with `if !blind` (leaving `LineEdit::handle_key` to ignore it, or an explicit `KeyCode::Enter => {}`).
   - **Minor gaps in `1f40100`:**
     - `Alt-B` ([`src/app/rooms.rs:1225-1230`](../../src/app/rooms.rs#L1225-L1230)) on `InfoRow::KnockRequest(_)` also lacks `&& !blind`, so pressing `Alt-B` (`word_left` in [`src/lineedit.rs:514`](../../src/lineedit.rs#L514)) while `typing_blind()` on a knock request opens `ban_prompt` instead of moving the filter cursor.
     - `InfoPopup::step` ([`src/manage.rs:1581-1588`](../../src/manage.rs#L1581-L1588)) clears `self.blind = false` before `if delta < 0 && self.selected == 0 { self.scroll = ...; self.reveal = false; return; }`. When `typing_blind()` began on `selected == 0`, pressing `Up` or `PageUp` clears `blind` (re-arming row keys) while setting `self.reveal = false` and leaving row `0` scrolled off-screen; capture `let was_blind = self.blind;` and check `&& !was_blind` on that early return.
7. **Finding #7 (`e322943` — backdrop click on picker):** **Closed.** [`src/app.rs:3812-3814`](../../src/app.rs#L3812-L3814) routes `Modal::Palette(_)` in `HitId::Backdrop` through `self.close_palette()`.
8. **Finding #8 (`c3c4671` — bounds & `/hierarchy`):** **Closed.** [`src/matrix/spaces.rs:184-203`](../../src/matrix/spaces.rs#L184-L203) (`live_children`) applies `.take(EVENTS_MAX)` after tombstone/malformed filtering, and [`src/matrix/spaces.rs:32-44, 314-360`](../../src/matrix/spaces.rs#L314-L360) (`names_within`) uses one `10 s` deadline (`timeout_at`), `limit = Some(100)`, and skips the root `space_id`.
9. **Space invitations & knocks (`0e58d20`, `0faa468`):** **Closed.** [`src/matrix/mod.rs:2166-2170`](../../src/matrix/mod.rs#L2166-L2170) keeps `RoomState::Invited | RoomState::Knocked` spaces in `summarize` (still filtering ignored inviters at `2173-2177` and skipping avatars at `2204`), and [`README.md:369-371`](../../README.md#L369-L371) documents space invitations.
10. **Minor notes (`a4add57`) & `fake::Server` (`src/matrix/spaces.rs`):** **Closed.** Covers `"Space ID"` ([`src/ui/roominfo.rs:1060-1061`](../../src/ui/roominfo.rs#L1060-L1061)), picker empty copy ([`src/palette.rs:193-198`](../../src/palette.rs#L193-L198), [`src/ui/palette.rs:135-145`](../../src/ui/palette.rs#L135-L145)), `space_name` truncation + `"Unnamed space"` ([`src/matrix/spaces.rs:94-102, 240-247`](../../src/matrix/spaces.rs#L240-L247)), `Confirm::RemoveFromSpace` clear on `can_edit` loss ([`src/manage.rs:1372-1377`](../../src/manage.rs#L1372-L1377)), and `sanitize_line` on `in_space.name` and `Pick::about` ([`src/app/spaces.rs:112-116`](../../src/app/spaces.rs#L112-L116), [`src/palette.rs:183-190`](../../src/palette.rs#L183-L190)). `fake::Server` is strictly `#[cfg(test)]`, binds `127.0.0.1:0`, and aborts its listener on `Drop`.
