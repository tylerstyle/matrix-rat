# M29 Review — Custom Emoji Pictures & Reactions (`emoji-pictures`)

Reviewed `main...emoji-pictures` (`d82e27d..e5ec315`, 3 commits: `c863831`, `3599f8a`, `e5ec315`; 18 files).

## Verdict

**Changes requested before merge.** The core architecture — `EMOTE_BOX` 2-cell wrapping, bounded `64x64` / `1 MiB` decode-guarded thumbnail fetching, Kitty-only gating, safe `:shortcode:` / `:?:` fallbacks, and redacted worker/cache `Debug` impls — is solid, but there are concrete privacy, gating, layout/overlay, and picker/state bugs below that need fixing.

---

## Findings to Fix

### 1. Privacy (`Debug`): `EmojiPicker` leaks `react_to` event IDs (`src/modal.rs:237-255, 1784`)
- `EmojiPicker` keeps `#[derive(Debug, Clone)]` (`src/modal.rs:237`), which formats `react_to: Option<(usize, String)>` (`message_id` at `src/modal.rs:240`) verbatim in `{picker:?}` and `{modal:?}` (`Modal` also derives `Debug` at `src/modal.rs:1784`), as well as dumping `results: Vec<Pick>`.
- By contrast, `StickerPicker` (`src/modal.rs:1682-1691`), `Emote` (`src/rich.rs:100-106`), `EmoteSpot` (`src/ui/timeline.rs:52-58`), `CustomEmoji` (`src/matrix/emotes.rs:52-56`), and `MatrixCmd`/`MatrixEvent` all use redacted custom `Debug` impls.
- The privacy check in `src/modal.rs:1962, 2016-2020` constructs `EmojiPicker::for_reaction(0, "$m".into())` and checks `!debug.contains("example.org") && !debug.contains("blobcat")`, missing the leaked `"$m"` event ID.
- **Fix:** Implement custom `Debug` on `EmojiPicker` (redacting `react_to`'s event ID, `custom`, `results`, and `query` similarly to `StickerPicker`) and assert `!debug.contains("$m")` in the test.

### 2. Undecryptable messages can still fetch and render custom-emoji reaction pictures (`src/ui/timeline.rs:641-652`)
- Body text gates on `emote_pictures` (`src/ui/timeline.rs:164-171`), which requires `matches!(message.kind, MsgKind::Text | MsgKind::Emote)` and explicitly excludes redacted and undecryptable messages.
- Reaction layout computes `chip_pictures` (`src/ui/timeline.rs:641-646`) with only `message.kind != MsgKind::Redacted`, leaving `MsgKind::Undecryptable` allowed. Because Matrix reactions are unencrypted and kept on `UnableToDecrypt` events in `src/matrix/convert.rs:209-223, 367-372`, an undecryptable message with an `mxc://` reaction from a non-ignored sender still queues `emote_wants` and fetches/draws pictures.
- **Fix:** Exclude `MsgKind::Undecryptable` in `chip_pictures` as well (`!matches!(message.kind, MsgKind::Redacted | MsgKind::Undecryptable)`).

### 3. Truncated custom-emoji reaction chips show blank `EMOTE_BOX` spaces instead of `:name:` (`src/ui/timeline.rs:653-679`)
- When `picture` is true, `label` is set to `rich::EMOTE_BOX` (`"  "`), `whole = format!(" {label} {} ", reaction.count)`, and `chip = truncate(&whole, body_width)` (`src/ui/timeline.rs:653-660`).
- If `chip != whole` (for instance `body_width <= 5` or a large reaction count in a narrow column), `src/ui/timeline.rs:674` skips pushing `EmoteSpot` (`if picture && chip == whole`), so no picture is painted — yet `chip` was already truncated from `"  "` rather than `reaction_label(&reaction.key, emotes.catalog)`, leaving blank leading spaces + `…` instead of `:shortcode:` or `:?:`.
- **Fix:** Check `width(&whole) <= body_width` (or recompute `chip` from `reaction_label` when truncated) before building `label` / `chip`.

### 4. Incomplete `covered` check + shared Kitty `Encoded` can orphan placeholder cells when duplicate emotes share the screen (`src/ui/timeline.rs:1326, 1364-1373, 1437-1469`, `src/ui/mod.rs:86-96`, `src/ui/image.rs:111-139, 312-319`, `src/images.rs:1108-1139, 1300-1337`, `src/app.rs:4617-4622`)
- `timeline::draw` defines `covered = app.modal.is_some() || app.help_open || app.images.viewer_open()` (`src/ui/timeline.rs:1326`), which does not cover overlays drawn later onto `timeline_area`:
  - the composer completion popup (`complete::draw` at `src/ui/mod.rs:88`, which issues `Clear` at `src/ui/complete.rs:64`), and
  - timeline corner pills (`" loading older messages… "`, `" loading newer messages… "`, `" ↓ newest "` drawn at `src/ui/timeline.rs:1437-1469` after `draw_emote`).
- Because each emote URL has one shared `Encoded` instance whose Kitty escape sequence only transmits on the first un-transmitted render (`carries_data == true` -> `sent_emote`), if an unshown emote appears twice on screen and its first `EmoteSpot` lies under `complete::draw` or a corner pill while its second `EmoteSpot` is visible:
  1. Spot #1 transmits (`carries_data == true`, pushed to `unverified`) and Spot #2 emits `\u{10EEEE}` placeholders (`carries_data == false`).
  2. `complete::draw` / the corner pill overwrites Spot #1 in the buffer.
  3. `image::verify` (`src/ui/mod.rs:96`, `src/images.rs:1321-1335`) sees Spot #1 was overwritten, discards `slot.take()` into `garbage`, and `flush_images(false)` (`src/app.rs:4621`) deletes that Kitty image ID from the terminal — leaving Spot #2 as raw placeholder cells with no `self.dirty = true` to re-encode/redraw. (And while the completion popup or corner pill stays over Spot #1, any subsequent redraw repeats the same discard loop.)
- *(Note: `draw_emote(..., false)` in `src/ui/modal.rs:837` is safe today because `modal::draw` renders last in `src/ui/mod.rs:95`.)*

### 5. Inline `<code>` / `<tt>` allows `<img data-mx-emoticon>` pictures (`src/rich.rs:137-139, 626, 688-695` vs `src/format.rs:536-546`)
- `Builder::open_tag("img")` (`src/rich.rs:626`) checks only `!self.pre` (`<pre>`), not inline code (`<code>` / `<tt>`, which sets `format.code = true` at `src/rich.rs:688-695`).
- By contrast, outgoing formatting in `fill_emotes` (`src/format.rs:536-546`) checks `code_depth` for both `<code` and `<pre` so shortcodes inside inline code stay text, matching the intent in `Rich::emotes` (`src/rich.rs:137-139`). Incoming `<code><img data-mx-emoticon ...></code>` currently paints a 2-cell Kitty picture over inline code instead of keeping `:name:`.
- **Fix:** Gate `format.emote` on `!self.pre && !format.code` (or clarify if inline code should differ from `fill_emotes`).

### 6. Emoji picker navigation across short and mixed-width rows (`src/modal.rs:350-369`, `src/app.rs:2946-2972`)
- **`PageUp` / `PageDown` silently no-ops near top/bottom:** `on_modal_key` calls `picker.step_rows(±page)` (`src/app.rs:2947, 2958-2959`), and `step_rows` returns early if `target < 0 || target >= row_count(&runs) as isize` (`src/modal.rs:358-360`). Pressing `PageUp` on rows `1..page-1` or `PageDown` within `page` rows of the bottom does nothing instead of clamping to the first/last row (contrast `StickerPicker::step` at `src/modal.rs:1774`, which clamps `0..=max`).
- **Vertical column drift across shorter/mixed-width rows:** `step_rows` (`src/modal.rs:354-366`) derives `x` solely from `place(&runs, self.selected)` with no preserved target column. Moving vertically across a short tail row or custom row (at `80x24`, custom rows span only columns `0..28` vs `0..39` for Unicode rows) snaps `x` left to the last column of that row and keeps subsequent steps shifted left; stepping from a Unicode cell (`EMOJI_CELL = 3`) into a `CUSTOM_CELL = 14` cell and back also snaps `x` left to the enclosing 14-column cell's left edge.

### 7. Room-switch race, picker state reset, and duplicate-URL shortcode selection (`src/matrix/emotes.rs:384-421, 588-673`, `src/app/emotes.rs:50-84`, `src/app.rs:1291-1317, 2869-2881`, `src/modal.rs:285-291`)
- **Stale `EmotePacks` across fast room switch:** `EmoteManager`'s worker task clones `active_room` at wakeup (`src/matrix/emotes.rs:393-401`) and sends `MatrixEvent::EmotePacks(catalog)` without a room ID or generation. If the user triggers loading in Room A and switches to Room B while Room A's `load_catalog` HTTP requests are in flight (`STATE_FETCH_TIMEOUT = 4s`), Room A's `EmotePacks` arrives first and `App::on_emote_packs` (`src/app/emotes.rs:72-84`) prematurely sets `self.emotes.loaded = true` and temporarily exposes Room A's room packs in Room B until the queued wakeup for Room B finishes. Also `if room_id.is_some()` in `EmoteManager::ensure` (`src/matrix/emotes.rs:416`) retains the previous `active_room` if `ensure(None)` is ever called later.
- **Async pack arrival resets `EmojiPicker` scroll/selection:** Because opening the picker triggers `want_emotes()`, an asynchronous `EmotePacks` arrival runs `picker.set_custom(&self.emotes.catalog)` -> `picker.refilter()` (`src/app/emotes.rs:80`, `src/modal.rs:285-291, 318-319`), resetting `selected = 0` and `scroll = 0` even if the user already scrolled/navigated (compare `StickerPicker::set_catalog` at `src/modal.rs:1712-1715`, which preserves selection by URL).
- **Duplicate-URL custom emoji lose chosen shortcode on reaction:** `EmoteCatalog::emojis()` deduplicates by `shortcode` (`src/matrix/emotes.rs:117-123`), so two packs with different shortcodes for the same `mxc://` URL both appear in `EmojiPicker`. However `pick_emoji` (`src/app.rs:2870, 2881`) drops `custom.shortcode` and passes only `&custom.url` to `toggle_reaction`, which resolves `shortcode_for_url(key)` (`src/app.rs:2261-2264`, `src/matrix/emotes.rs:137-151`) and always picks the first pack's shortcode instead of the selected entry's.
- **`reset_session()` omits `Modal::Emoji(_)` (`src/app.rs:1296-1317`):** Unlike `Modal::Stickers(_)` (line `1310`) and 14 other session modals, `Modal::Emoji(_)` is omitted from `reset_session()`. Existing callers overwrite or clear `self.modal` around it (`Logout` closes settings first; `NeedLogin`/`NeedReauth` immediately open `Modal::Login`), so it is latent today, but `reset_session()` should clear `Modal::Emoji(_)` too.

---

## Verified Areas & Minor Notes

### 1. `src/rich.rs`: `Format::emote`, `Rich::emotes`, and wrapping
- **Verified:**
  - `is_emoticon` (`23-35`), `is_valid_mxc` (`src/matrix/emotes.rs:156-160`), and `emoticon_label` (`37-54`) sanitize entities, strip control/bidi characters via `sanitize_line`, reject blank/invisible names via `is_blank`, truncate labels cleanly, and fall back `alt` -> `title` -> `EMOTICON_PLACEHOLDER` (`":?:"`).
  - `wrap` (`847-944`) keeps each picture-ready emote as an atomic 2-cell `EMOTE_BOX` (`890-904`), never splitting it across lines, preserves trailing `EMOTE_BOX` in `next_row` via `last.emote_box().is_none()` (`869-874`), and falls back to wrapping `:name:` whenever `room < width(EMOTE_BOX)` (width `0`, `1`, or long prefixes). Each picture emote has its own index in `format.emote`, preventing `push_piece` / `wrap::push` from coalescing adjacent emotes or text.

### 2. `src/ui/timeline.rs` & `src/ui/timeline/receipts.rs`
- **Verified:**
  - `EmoteSpot` columns (`545-551` for text, `674-678` for reaction chips) track exact wrapped visual columns including `INDENT`, prefixes, and `▶ ` play indicators, and respect `content.right()` (`1368-1371`).
  - Vertical rendering only iterates visible full rows (`take(last_visible).skip(top)` at `1364`), so `2x1` emotes are never partially clipped vertically (`skip = 0`).
  - `RowKey` includes both `emotes: app.emotes.rev` and `emote_pictures: app.images.emote_rev` (`1008-1011, 1187-1188`), re-laying out rows when packs arrive, emote pictures load, or `Ready` cache entries evict/clear.
  - Reaction labels (`86-102`) never expose raw `mxc://` strings or untrusted sender shortcodes (`shortcode_for_url` or `:?:`), and `HitId::Reaction(i, r)` encloses `EmoteSpot`.
- **Minor notes:**
  - `receipts::add` (`src/ui/timeline/receipts.rs:59-74`): `row.emotes.retain` prevents painting an emote over `"… ✓"` when `cut` truncates near `max_width - 3`, though cutting inside `EMOTE_BOX` leaves the first space cell as a blank column before `"… ✓"`.
  - Pre-existing `RowKey.previews.3` (`src/ui/timeline.rs:1185`) tracks `ignored_users.len()` rather than a revision, so a synced ignored-list replacement with unchanged length would not invalidate `RowCache`.
  - `toggle_reaction` (`src/app.rs:2261`) checks raw `key` while `timeline.rs:648` trims `reaction.key.trim()`; keeping untrimmed `key` for `ToggleReaction` is right for SDK exact-key redaction, though `+1`ing a whitespace-padded external reaction misses `shortcode_for_url(key)` without trimming there.

### 3. `src/images.rs` & Worker Fetch (`src/matrix/media.rs`, `src/matrix/mod.rs`, `src/app/media.rs`)
- **Verified:**
  - Shared `Small` / `SmallState` cache (`568-594`) keeps `emotes` (`EMOTE_ENTRIES = 256` at `59`) separate from `avatars` (`512`) and inline `entries`. `EMOTE_IN_FLIGHT = 6` (`62`) caps queued UI requests (`1030-1069`), while worker `Media::fetch_emote` (`src/matrix/media.rs:72, 142-174`) gates downloads/decodes behind `emote_slots = Semaphore::new(2)` with `AVATAR_TIMEOUT` (`20s` at `src/matrix/media.rs:35`) and `self.busy` dedup. *(Note: UI limit is `6` while worker parallel download/decode limit is `2`, with up to 4 tasks queued on `emote_slots`.)*
  - `is_valid_mxc` is enforced at HTML parse, reaction layout, UI request queue, and `fetch_emote` entry (failing immediately with no HTTP call).
  - `load_emote` (`src/matrix/media.rs:259-272`) requests `Thumb::scale((64, 64))` (`EMOTE_PIXELS`) and falls back once on `Refused::Failed` (never on `TooLarge`), bounded by `EMOTE_DOWNLOAD = 1 MiB` (`46`) across cache, `Content-Length`, and stream chunks. `decode()` (`src/images.rs:240-282`) guesses format from bytes, caps sides (`16_384`) and pre-decode alloc (`256 MiB`), decodes a single still frame only (`Cargo.toml:52`, `png`/`jpeg`/`gif`/`webp`/`bmp` — SVG refused), applies EXIF orientation, rejects zero dimensions, and thumbnails down to `64x64`.
  - `emote_rev` bumps on `on_emote(Ok)` (`1095`), `evict_emotes` removing `Ready` (`1160`), and `clear()` (`865`). Logout / session reset calls `ImageStore::clear()` (`src/app.rs:1274`); terminal resize / suspend calls `forget_encoded()` (`src/app.rs:3514-3517, 4549, 4599`), queueing old Kitty IDs in `garbage` while keeping decoded pixels.
  - Exclusions verified for ignored senders (body via `emote_pictures` + `convert.rs`; reactions via `convert.rs:213-221` + `chip_pictures`; reply quotes plain-only), hidden spoilers (`hidden` flag at `src/ui/timeline.rs:489-504` until `revealed == Some(i)`), redacted messages (`166`, `642`), undecryptable message bodies (`166`; reaction chips flagged in #2 above), `images = off` / non-Kitty (`is_kitty()` gates all emote paths), off-screen rows (`on_screen = first.source..=last.source` at `1392-1399`; picker only queues visible rows at `src/ui/modal.rs:783-840`), and demo mode (`want_emotes` exits early when `self.matrix.is_none()`).
- **Non-blocking notes:**
  - `on_emote(..., Err(_))` stores `SmallState::Failed` (`src/images.rs:1102`), and `request_emotes` refreshes `small.used = frame` on cache hits (`1042`) without re-trying `Failed`, so transient errors remain `:shortcode:` while visible until LRU eviction past `256` entries or `clear()`.
  - `Refused::TooLarge(None)` (`src/matrix/media.rs:286-288`) formats `MAX_DOWNLOAD` ("over 20 MB") instead of `limit` (`1 MB` `EMOTE_DOWNLOAD`); invisible to users since `on_emote` discards the error string.
  - Loading emotes do not set `self.animating`, so `REQUEST_TIMEOUT` (`90s`) re-requests only trigger on subsequent redraws/events.

### 4. Privacy (`Debug` / Logs), MSC4027 Reactions, & `README.md`
- **Privacy verified (aside from Finding #1 on `EmojiPicker`):** Zero new `tracing::*`, `eprintln!`, or `dbg!` calls in the diff. `MatrixCmd::FetchEmotePicture`, `MatrixCmd::ToggleReaction`, `MatrixEvent::EmotePicture` (`src/matrix/mod.rs:484, 518, 1092`), `Emote`, `EmoteSpot`, `ImageStore`, and `CustomEmoji` all redact `mxc://` URLs, shortcodes, error strings, and IDs.
- **MSC4027 reactions & picker (`src/matrix/rooms.rs:803-819`, `src/matrix/emotes.rs:167-173`, `src/app.rs:2888-2907`):**
  - Sending uses `timeline.toggle_reaction_with_extra_content` with key `mxc://...` and validated `shortcode` / `com.beeper.reaction.shortcode` (`:code:`). Toggling off via chip click works even for unknown/unpacked `mxc://` keys because SDK redaction matches exact reaction key and ignores `extra_content`.
  - Catalog loads are bounded (`MAX_PACK_ROOMS = 8`, `MAX_PACKS_PER_ROOM = 8`, `MAX_PACKS = 24`, `MAX_IMAGES_PER_PACK = 300`, `STATE_FETCH_TIMEOUT = 4s`) across `RoomState::Joined` rooms only.
  - Draft insertion via `insert_custom_emoji` aligns with `:` completion marks (minor quirk: inserts a leading space after non-whitespace punctuation like `(`, and does not guard against an unclosed inline code span).
- **`README.md` (`31, 136, 145-146, 445-458`):** Accurate against code, except lines `447-448` list typing `:` + two letters, the emoji picker, and the sticker picker as pack-load triggers while `apply_timeline` (`src/app.rs:2148-2160`) also calls `want_emotes()` when a room has an `mxc://` reaction.


---

## Re-check (`8b43349..HEAD`, 9 commits: `358064e..ab1cc37`)

**Verdict: M29 ok — all seven findings and the two former minor notes (`RowKey` ignored-users hash, `README.md` pack trigger) are cleanly closed.** Review only; no builds or tests run (`hpfury` reported 865 unit tests green).

### Findings & Notes Closed

1. **Finding #1 (`358064e` — `EmojiPicker` `Debug`):** Closed. [`src/modal.rs:245-282`](../../src/modal.rs#L245-L282) replaces `#[derive(Debug)]` with a hand-written redacted `Debug` (`reaction: bool`, `query_bytes`, counts, `selected`, `scroll`, `.finish_non_exhaustive()`), and the privacy test asserts `"example.org"`, `"blob"`, `"Blobs"`, `"$m"`, and `"partyb"` are absent from both `{picker:?}` and `{Modal::Emoji(picker):?}`.
2. **Finding #2 (`05b7114` — undecryptable reactions):** Closed. `chip_pictures` in [`src/ui/timeline.rs:644-649`](../../src/ui/timeline.rs#L644-L649) excludes `MsgKind::Redacted | MsgKind::Undecryptable`, keeping undecryptable messages free of reaction picture fetches/draws.
3. **Finding #3 (`05b7114` — cut reaction chips):** Closed. [`src/ui/timeline.rs:658-687`](../../src/ui/timeline.rs#L658-L687) checks `width(&pictured) <= body_width` *before* choosing `pictured` vs `reaction_label`, so narrow/overflowing chips fall back cleanly to truncated `:name: count`.
4. **Finding #4 (`aa4beb2` — overlays, draw order, & `ImageStore::verify`):** Closed.
   - **Draw order ([`src/ui/mod.rs:86-91`](../../src/ui/mod.rs#L86-L91)):** Drawing `composer::draw` -> `complete::area` -> `timeline::draw(..., popup)` -> `complete::draw` is safe. `timeline_area` and `composer_area` are disjoint vertical splits (`Layout::vertical`), `timeline::draw` never sets cursor position and reads neither `app.composer_view` nor `app.attach.hidden`, `composer::draw` does not depend on anything `timeline::draw` mutates (`row_cache`/`app.timeline`, and `draw_own_badge` uses the separate `avatars` cache), `complete::draw` still runs *after* `timeline::draw` so `HitId::Overlay` / `HitId::Complete(idx)` stay topmost in [`src/ui/hit.rs`](../../src/ui/hit.rs#L187-L206), and `complete::draw` restores the composer cursor ([`src/ui/complete.rs:141`](../../src/ui/complete.rs#L141)).
   - **Proactive overlay avoidance ([`src/ui/timeline.rs:1345-1402, 1482-1505`](../../src/ui/timeline.rs#L1345-L1402)):** `corner_pills` is built upfront, `overlays` combines `pills` and `popup`, and both `draw_inline` and `draw_emote` pass `covered || under_overlay(x, y)` on the transmission cell `(x, y)` (`ScrollbarOrientation::VerticalRight` draws in column `inner.right() - 1` outside `content` at `inner.width - 1`, [`src/ui/timeline.rs:1178`](../../src/ui/timeline.rs#L1178)). Covered copies only refer to an already-transmitted slot or skip transmitting until an uncovered copy runs.
   - **No-loop guarantee on `ImageStore::verify` -> `take_redraw` ([`src/images.rs:1153-1160, 1329-1376`](../../src/images.rs#L1329-L1376), [`src/ui/image.rs:121`](../../src/ui/image.rs#L121), [`src/app.rs:2017-2022`](../../src/app.rs#L2017-L2022)):** If an unlisted overlay ever overwrites a `Target::Emote` transmission, `verify()` discards `slot.take()`, pushes `(x, y)` into `emotes_painted_over`, and sets `self.redraw = true` (`take_redraw()` sets `self.dirty = true` in `after_draw()`). Because `draw_emote` checks `images.emote_painted_over(area.x, area.y)` and `images.emote(url, true)` suppresses any unshown slot without calling `sent_emote`, the same cell cannot fail twice before clearing; `emotes_painted_over` accumulates across consecutive frames until a frame has `emote_lost == false`, which clears `emotes_painted_over` without setting `redraw`. Thus redraws are strictly bounded (0 when covered by `overlays`, 1 when another uncovered copy exists, and stopping cleanly even if every copy on screen was covered). *(Minor timing detail: because `after_draw()` runs just before `tokio::select!` in [`src/app.rs:4633-4639`](../../src/app.rs#L4633-L4639), a fallback `take_redraw()` waits up to one `250ms` `tick` if no other event arrives.)*
5. **Finding #5 (`b492af8` — inline code):** Closed. [`src/rich.rs:628`](../../src/rich.rs#L628) gates `<img data-mx-emoticon>` on `!self.pre && !format.code`, matching `format.rs::fill_emotes`.
6. **Finding #6 (`5887ac2` — picker navigation & goal column):** Closed. [`src/modal.rs:266, 375, 404, 410-425`](../../src/modal.rs#L410-L425) clamps `step_rows` to `0..=row_count(&runs) - 1`, keeps `goal` across vertical moves (via `get_or_insert`), and clears `goal = None` on `refilter`, horizontal/row `step`, and search-field clicks ([`src/app.rs:4224`](../../src/app.rs#L4224)).
7. **Finding #7 (`8afc873` — packs room tag, picker place, shortcode selection, `reset_session`):** Closed.
   - [`src/matrix/emotes.rs:374-377, 412-415, 426-429`](../../src/matrix/emotes.rs#L374-L429) tags `MatrixEvent::EmotePacks { room_id, catalog }` and replaces `active_room` even on `None`; [`src/app/emotes.rs:74-89`](../../src/app/emotes.rs#L74-L89) drops mismatched rooms or unrequested responses (including post-`reset_emotes()`).
   - [`src/modal.rs:315-346`](../../src/modal.rs#L315-L346) preserves selection across async pack arrival by `Chosen`, keeps `scroll`/`goal`, and sets `reveal = true`.
   - [`src/app.rs:1311, 2262-2291, 2879-2891`](../../src/app.rs#L2262-L2291) adds `Modal::Emoji(_)` to `reset_session()`, forwards the chosen `custom.shortcode` via `toggle_reaction_named`, and trims `key` only for `shortcode_for_url` lookup.
8. **Extra closed notes (`d60a8a2`, `c794cec`, `ab1cc37`):** `RowKey.previews.3` now hashes `app.ignored_users` with `DefaultHasher` ([`src/ui/timeline.rs:1191-1201`](../../src/ui/timeline.rs#L1191-L1201)), and [`README.md:448-449`](../../README.md#L448-L449) lists custom-emoji reactions among pack load triggers.

### Remaining Open Minor Notes — Assessment

All four can safely remain open for `0.7`:
- **Half emoji box before `"… ✓"` ([`src/ui/timeline/receipts.rs:59-74`](../../src/ui/timeline/receipts.rs#L59-L74)):** `row.emotes.retain` guarantees Kitty never paints over `"… ✓"`; slicing inside `"  "` leaves at most one trailing space before `"… ✓"` on an overflowing final row with receipts. Harmless cosmetic edge case.
- **No retry of failed emoji pictures ([`src/images.rs:1051-1063, 1112`](../../src/images.rs#L1051-L1063)):** Keeps `SmallState::Failed` until evicted past `EMOTE_ENTRIES` (`256`) or `clear()`, showing the safe `:shortcode:` fallback cleanly without hammering failing URLs. Safe to defer post-`0.7`.
- **`Refused::TooLarge(None)` text ([`src/matrix/media.rs:286-288`](../../src/matrix/media.rs#L286-L288)):** Only formats `MAX_DOWNLOAD` when `Content-Length` is omitted on an oversized emote stream, and `on_emote` discards `Err(String)` completely — invisible to users and logs.
- **`animating` flag not set for loading emotes ([`src/images.rs:1040-1080`](../../src/images.rs#L1040-L1080)):** Intentionally right as-is — loading emotes show `:shortcode:` text rather than a spinner, so forcing `250ms` tick redraws while fetching would waste CPU/battery for no visual change.
