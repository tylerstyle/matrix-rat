# Implementation Plan Review — `matrix-rat` (`docs/PLAN.md`)

- **Reviewer:** `matrix-agy`
- **Date:** 2026-11-12
- **Target:** [`docs/PLAN.md`](../PLAN.md)
- **Verdict:** **Approve with changes**

---

## Executive Summary

[`docs/PLAN.md`](../PLAN.md) is a well-scoped, pragmatic plan. In particular:
1. **Fork-vs-scratch decision (§2):** Rejecting a direct fork of `iamb` while selectively adapting its HTML parser (`src/message/html.rs`) and theme palettes under Apache-2.0 is the right call. `iamb`'s deep coupling to `modalkit` and classic `/sync` would cost more effort to unwind than writing a purpose-built ratatui app on `matrix-sdk-ui`.
2. **Stack & versions (§3):** Verified against crates.io and the local toolchain (`rustc 1.99.1` / `cargo 1.99.0`):
   - `matrix-sdk 0.19.1` & `matrix-sdk-ui 0.19.1` (MSRV 1.96) align with the local compiler.
   - `ratatui 0.30.2`, `crossterm 0.29.0`, `ratatui-image 11.1.0`, `ratatui-textarea 0.9.4`, `crokey 1.5.1`, `tachyonfx 0.25.2`, and `throbber-widgets-tui 0.11.1` all resolve against `ratatui 0.30` and `crossterm 0.29` without duplicate-crate conflicts.
3. **Architecture & milestone structure (§4, §10):** A UI-owned state model driven by a single `Action` enum, a per-frame `HitMap`, and building M1 (UI shell, mouse, theme engine, snapshot tests) with fake data before networking are strong architectural choices.

However, there are several high- and medium-severity architectural and SDK integration issues that should be addressed in the plan before M0/M1–M3 implementation begins.

---

## Findings (Ordered by Severity)

### 1. [HIGH] Virtualized timeline line cache key will serve stale renders on `VectorDiff::Set` and message grouping changes
**Where:** §4 (*Timeline rendering*) & M3

**Problem:**
- The plan specifies caching wrapped lines per `(item id, width, theme rev)`.
- In `matrix-sdk-ui::timeline`, `Timeline` emits `VectorDiff<Arc<TimelineItem>>`. Each `TimelineItem` has a stable `UniqueTimelineItemId` (`TimelineItem::unique_id()`), which remains constant when an item is mutated in-place via `VectorDiff::Set` — including when a local echo transitions to a remote event, reactions change, a message is edited or redacted, a thread summary updates, a media thumbnail finishes loading, or read receipts update.
- If the cache key is only `(item_id, width, theme_rev)`, in-place updates will hit the stale cached `Line`s.
- Furthermore, §4 specifies *"grouped consecutive messages from one sender"*. When an item at index `i-1` is inserted/removed, or a `DateDivider` / `ReadMarker` virtual item moves between `i-1` and `i`, item `i` toggles between compact (no sender header) and full (with sender header), changing its line count and layout height even though item `i` itself was not touched by a diff.

**Suggested Change:**
1. Anchor scroll position and view-model identity to `TimelineItem::unique_id()` (not `EventId`, which does not exist for unsent local echoes or virtual items like `DateDivider` / `ReadMarker`).
2. Give every `MsgItem` view model:
   - a monotonic `content_rev: u64` incremented whenever `VectorDiff::Set` replaces that entry or when an associated media/avatar decode completes; and
   - a computed `show_header: bool` recomputed for neighbor items when applying `VectorDiff`s.
3. Key the layout/line cache by `(unique_id, content_rev, show_header, width, theme_rev)`.

---

### 2. [HIGH] `matrix-sdk-ui` `EventCache` bootstrap and per-room `Timeline` subscription lifecycle
**Where:** §4 (*Architecture / Data flow*) & M2–M3

**Problem:**
1. **`EventCache::subscribe()` footgun:** In `matrix-sdk-ui 0.19.1`, `client.event_cache().subscribe()?` **must** be called once right after session creation/restoration before starting `SyncService` or building any `Timeline`. Without it, `EventCache` never processes incoming sync responses, silently breaking both `Timeline` pagination/storage and `RoomListService` latest-event previews.
2. **Room switching churn & stale channel events:** If the worker creates a fresh `Timeline` on every `MatrixCmd::OpenRoom` and drops the previous one, switching back and forth between rooms (`Alt-↑/↓`) loses back-pagination state and re-runs timeline initialization every time. Conversely, leaving live stream forwarders running for every visited room floods the UI channel. In addition, in-flight `MatrixEvent::Timeline` batches from room A can arrive at the UI thread right after the user switches to room B.
3. **Sidebar metadata updates:** `RoomListService`'s `VectorDiff` stream reflects structural changes and room ordering, but room metadata changes (typing, unread counts, display name, avatar, latest event decryption) also require either proper `RoomListEntriesWithDynamicAdapters` configuration or per-room info observation so sidebar badges don't go stale.

**Suggested Change:**
1. Explicitly document `client.event_cache().subscribe()?` in `matrix/session.rs` as a mandatory step before starting `SyncService`.
2. In `matrix/timeline.rs`, hold a bounded LRU cache (e.g., 5–8 rooms) of live `Arc<Timeline>` objects in the worker so switching back to a recent room is instant, while aborting/pausing the UI stream forwarding task for non-visible rooms.
3. Include `room_id: OwnedRoomId` (and a subscription generation counter) on `MatrixEvent::Timeline` so the UI cleanly ignores or routes diffs that arrive after a room switch.

---

### 3. [HIGH] Terminal image clipping (`ratatui-image` 11.1) and startup stdio query ordering
**Where:** §3 (*Stack — Images*), §4 (*Images*), §12 (*Risks*) & M5

**Problem:**
1. **Startup stdio race:** `Picker::from_query_stdio()` and `terminal-colorsaurus` both write escape sequences (`CSI` / `OSC 11` / `DA1`) to stdout and read responses from stdin. If either runs before raw mode is enabled, or after `crossterm::event::EventStream::new()` is constructed, responses will either echo to the terminal, hang waiting for a newline, or get eaten by crossterm's input reader.
2. **Top-edge clipping of Sixel/iTerm2/Kitty images:** Section 4 notes *"cache encoded `Protocol` in an LRU keyed by `(mxc, cell size)` → render. Partially visible images: use ratatui-image's sliced rendering as iamb does (verify quality)"*. A pre-encoded `Protocol` in `ratatui-image` is encoded for a full rectangle. When a message with an inline image scrolls partially off the **top** or **bottom** of the timeline viewport, passing a smaller `Rect` with `Resize::Crop` only crops from the right/bottom (or requires re-encoding from the `DynamicImage` with a top-offset crop for Sixel/iTerm2). Rendering an unclipped Sixel or Kitty image into a partially scrolled slot will draw over the room header or composer border.

**Suggested Change:**
1. Define the strict startup sequence in `main.rs`:
   `enable_raw_mode()` → `Picker::from_query_stdio()` (with a short timeout fallback to halfblocks) → `terminal-colorsaurus` startup query → enter alternate screen + enable mouse capture → construct `crossterm::event::EventStream::new()`.
2. Keep decoded, downscaled `image::DynamicImage` thumbnails in a memory-bounded LRU in `images/`. When an image's `Rect` is fully inside the timeline viewport, render the cached `Protocol` keyed by `(mxc, width_cells, height_cells)`. When an image is partially clipped by the top or bottom edge of the timeline viewport, either render it using the `Halfblocks` protocol (which clips cleanly at cell granularity in `ratatui`'s `Buffer`) or crop the `DynamicImage` slice before encoding.

---

### 4. [HIGH] Milestone ordering: E2EE recovery/backup is needed before M4 to test M2/M3 on real accounts
**Where:** §10 (*Milestones and review gates*)

**Problem:**
- M2 is *Login + sync + room list*, M3 is *Timeline + composer*, and M4 is *E2EE (Verification overlay, recovery, backup status, shields)*.
- On modern Matrix homeservers (including matrix.org), nearly all private rooms and DMs are encrypted by default. When `matrix-rat` logs in as a new device in M2, it will have no megolm session keys until it unlocks secret storage / key backup via the recovery key (`client.encryption().recovery().recover(&secret)`).
- If recovery key entry waits until M4, testing M2 (latest-event previews in encrypted rooms) and M3 (reading timeline history in encrypted rooms) on any real account will show mostly *"Unable to decrypt"* items.

**Suggested Change:**
- Pull **basic recovery key input (CLI flag / config / simple prompt on login) + `encryption().recovery().recover(...)` + graceful UTD (Unable-To-Decrypt) timeline item rendering** into M2/M3, leaving interactive SAS emoji verification (`src/verifications.rs`), device management, and E2EE shields UI for M4.

---

### 5. [MEDIUM] Missing `matrix-sdk` Cargo features (`sso-login`, `socks`) and `throbber-widgets-tui` version pin
**Where:** §3 (*Stack*), §7 (*Settings menu*), §9 (*Matrix feature scope*)

**Problem:**
- Section 3 specifies `matrix-sdk 0.19.1` features as:
  `default (e2e-encryption, sqlite, rustls-aws-lc-rs) + bundled-sqlite, local-server, qrcode, markdown`.
- In `matrix-sdk 0.19.1`, **SSO login** (§9) with a localhost callback server requires the `"sso-login"` feature flag (`"sso-login": ["local-server"]`).
- Section 7 includes **proxy** configuration in Advanced settings; supporting SOCKS5 proxies requires the `"socks"` feature on `matrix-sdk` (`"socks": ["reqwest/socks"]`).
- In §3, `throbber-widgets-tui` has no version listed; note that `0.9.x` pulls in `ratatui 0.29`, whereas `0.10.0` / `0.11.1` targets `ratatui 0.30`.

**Suggested Change:**
- Add `"sso-login"` and `"socks"` to `matrix-sdk` features in §3.
- Pin `throbber-widgets-tui = "0.11"` (or implement a 20-line braille spinner widget directly in `ui/widgets/` to avoid the dependency altogether).

---

### 6. [MEDIUM] Adapting `iamb`'s `html.rs` requires custom span-wrapping to populate `HitMap` link & code-block regions
**Where:** §2 (*Borrow from iamb*), §4 (*Mouse*, *Timeline rendering*, `ui/html.rs`, `ui/message.rs`)

**Problem:**
- `iamb`'s `src/message/html.rs` and `src/message/printer.rs` are coupled to `iamb`/`modalkit` cursor structures and pre-0.30 `ratatui` types, so they cannot be dropped in verbatim.
- More importantly, §4 promises `HitId::Link(msg, n)` and clickable code-block copy actions. Standard `ratatui::widgets::Paragraph::wrap()` performs word-wrapping internally during rendering and **does not report back the wrapped `(x, y, width)` screen coordinates** of individual `Span`s.
- Additionally, §4 mentions emitting raw **OSC 8 hyperlinks** via a helper. Injecting raw `\x1b]8;;...\x1b\\` escape sequences into `ratatui` `Span` strings corrupts character width calculations and `ratatui`'s cell diffing unless handled outside the cell text; and while mouse capture is enabled, plain clicks are intercepted by the TUI anyway.

**Suggested Change:**
1. Have `ui/html.rs` parse Matrix HTML into an intermediate styled document (`Vec<RichBlock>` / `Vec<RichSpan>`) carrying semantic metadata (`Link( href )`, `CodeBlock { lang, raw }`, `Spoiler`, `QuoteDepth`, `Mention`).
2. In `ui/message.rs`, perform explicit word-wrapping using `unicode-segmentation` and `unicode-width` to produce both `Vec<ratatui::text::Line>` **and** a `Vec<RelativeHitRect>` (item-relative `(row, col_start..col_end, HitTarget)`). During `draw()`, offset those relative rects by the message's screen `Rect` and register them into `HitMap`.
3. Use `HitMap` click + hover underline + status-bar URL preview as the primary link mechanism; omit raw OSC 8 escape injection into `ratatui` `Span`s (or only emit OSC 8 if mouse capture is disabled).

---

### 7. [MEDIUM] Noctalia & file-watcher edge cases: atomic renames and contrast tokens
**Where:** §8 (*Theming — Theme files, Dark/light and live reload, Desktop-shell integration*)

**Problem:**
1. **Atomic rename breaks single-file `notify` watchers:** Theme generators (Noctalia, matugen, wallust) and editors (vim, helix, `toml_edit` write-back) often write to a temporary file and `rename()` over `~/.config/matrix-rat/themes/<name>.toml`. Watching the file path directly with `notify` watches the old inode and silently stops firing after the first reload.
2. **Material You badge/selection contrast in light vs dark themes:** The sample `[palette]` in §8 defines `bg`, `surface`, `fg`, `muted`, `primary`, `secondary`, `accent`, `error`, with `unread_badge = { bg = "primary", fg = "bg" }`. In Noctalia (Material Design 3), `primary` and `bg` (`surface`) are not guaranteed to have readable contrast against each other in all schemes; MD3 provides `on_primary`, `on_secondary`, `on_error`, and `surface_container` specifically for this.
3. **Mode 2031 live reports inside `crossterm`:** As noted in §12, enabling terminal mode 2031 while `crossterm`'s `EventStream` is reading stdin risks unrecognized CSI sequences (`CSI ? 997 ; 1 n`) being dropped or misparsed as key events by `crossterm`.
4. **`matrix-rat --reload-theme` target discovery:** Section 8 proposes `post_hook = "matrix-rat --reload-theme"`, which needs a reliable way to signal all running `matrix-rat` instances without `pkill` races.

**Suggested Change:**
1. Watch the **parent directories** (`~/.config/matrix-rat/` and `~/.config/matrix-rat/themes/`) non-recursively in `theme/watch.rs` and filter events by filename so atomic `rename()` writes always trigger hot reload.
2. Add `on_primary` (defaulting to `bg` if omitted in hand-written themes) and `surface_high` / `outline` to `[palette]`, and map Noctalia's `{{colors.on_primary.default.hex}}`, `{{colors.surface_container.default.hex}}`, and `{{colors.tertiary.default.hex}}` in the shipped Noctalia v4/v5 templates.
3. Drop mode 2031 from the live input stream; instead, query `terminal-colorsaurus` once at startup (before `EventStream`) and re-detect on `SIGUSR1` / file-watch trigger.
4. Have each running instance write `$XDG_RUNTIME_DIR/matrix-rat/<profile>.pid` (cleaned up on exit) so `matrix-rat --reload-theme` can iterate `*.pid` and send `SIGUSR1`.

---

### 8. [LOW] SQLite store concurrent-process lock and terminal state cleanup on `Ctrl-Z` / panic
**Where:** §4 (`main.rs`, `matrix/session.rs`), §6 (*Keybinds*)

**Problem:**
1. **Concurrent profile corruption:** Running two instances of `matrix-rat` against the same profile SQLite directory simultaneously can corrupt or wedge the `matrix-sdk-sqlite` crypto store (Olm account state) and event cache.
2. **Kitty keyboard protocol + mouse capture restoration:** Section 6 enables the Kitty keyboard enhancement protocol (`PushKeyboardEnhancementFlags`). If `PopKeyboardEnhancementFlags` and `DisableMouseCapture` are not emitted in the panic hook, normal exit path, and `SIGTSTP` (`Ctrl-Z`) handler, the user's Kitty shell is left in CSI-u mode after exit or suspend.

**Suggested Change:**
1. Acquire an exclusive `flock` on `<data_dir>/<profile>/lock` at startup before opening the SDK SQLite store; exit with a clear human-readable error if already locked.
2. Centralize terminal enter/leave in a RAII `TerminalGuard` used by `main.rs`, the panic hook, and `SIGTSTP`/`SIGCONT` handling (popping keyboard enhancement flags, disabling mouse capture, and leaving the alternate screen).

---

## Summary of Action Items Before / During Implementation

- **Before starting M1:** Confirm the cache-key design (`unique_id`, `content_rev`, `show_header`, `width`, `theme_rev`), custom span-wrapping with relative hit rects, parent-directory `notify` watching, and `TerminalGuard` cleanup.
- **In M0:** Include `"sso-login"` and `"socks"` in `matrix-sdk` features, pin `throbber-widgets-tui = "0.11"` (or write a small built-in spinner), and add the profile lockfile helper.
- **In M2/M3:** Call `client.event_cache().subscribe()?` at session init, use a bounded per-room `Timeline` LRU in the worker, and include basic recovery key entry + UTD rendering so encrypted rooms work during M2/M3 testing.

## Verdict

**Approve with changes.** You may proceed to **M0 (Scaffold)** while incorporating the adjustments above into the design and implementation of M0–M7.
