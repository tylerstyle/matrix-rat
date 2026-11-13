# Review: M5 Part 1 — Inline Images (`dd24771..d6402dd` / `9505e36`)

*Reviewed against commit `9505e36` (M5 part 1 merged in `d6402dd`, `17 files` changed in `dd24771..d6402dd`; note `HEAD` has since moved on to `1a301c5`, and all line numbers below match `9505e36`).*

The inline-image architecture is well aligned with `matrix-rat`: channel-based background download/decoding, `ratatui-image` (`11.1.0`) Kitty Unicode placeholders so images live in ordinary cells and respect popups, Halfblocks fallback with 256/16-color downgrade, sanitized `ImageInfo` metadata (`src/matrix/convert.rs`), key-hiding `Debug` on `ImageSource` (`src/model.rs`), and a solid scroll `Anchor` design (`src/ui/timeline.rs`).

However, treating the branch as untrusted code reveals **2 High** and **3 Medium** issues (plus a few minor items) that should be fixed before pushing.

---

## Findings (ordered by severity)

### High

1. **Download size cap is enforced only after downloading into memory and writing to SQLite** — `src/matrix/media.rs:104-116, 125-152`
   - `get()` (`src/matrix/media.rs:104-116`) calls `client.media().get_media_content(&request, true).await`, and only checks `if bytes.len() as u64 > MAX_DOWNLOAD` (`20 MiB`) *after* `get_media_content` returns.
   - In `matrix-sdk-0.19.1/src/media.rs:460-485, 814-857`, `get_media_content(&request, true)` downloads the entire HTTP response body into memory (`Vec<u8>`, duplicated through `AttachmentDecryptor::read_to_end` for encrypted media) using an HTTP client with `Duration::MAX` timeout, and writes the payload into the SQLite `media_store` (`use_cache = true`) *before* returning `Vec<u8>` to `matrix-rat`. `Media::fetch` (`src/matrix/media.rs:20-21, 59-64`) enforces `TIMEOUT = 60s` and `PARALLEL = 4`, which stops infinitely slow trickles, but up to 4 fast transfers can still allocate arbitrary multi-hundred-MB payloads in RAM and persist them to SQLite before `MAX_DOWNLOAD` ever rejects them.
   - Additionally, `download()` (`src/matrix/media.rs:125-152`) relies on untrusted sender metadata:
     - `let small_file = size.is_some_and(|size| size <= SMALL_FILE);` (`256 KiB`) lets a sender spoof `info.size <= 256 KiB` on a huge unencrypted image to bypass server-side thumbnailing (`!full && !small_file`) and force full-file download.
     - The pre-download size check (`lines 148-150`) only triggers if `info.size` was both provided and `> MAX_DOWNLOAD`.
   - **Suggested fix:** Bound the actual HTTP stream before buffering or writing to the SDK cache (check `Content-Length` if present and read chunk-by-chunk with an explicit byte counter aborting at `MAX_DOWNLOAD`, or disable `use_cache = true` on `get_media_content` until size is verified and stream via the SDK's authenticated media request / `reqwest` response bytes stream). Treat sender `info.size` as an optimization hint only—never trust `info.size <= SMALL_FILE` without enforcing a streaming byte limit on the download.

2. **Timed-out terminal capability query leaves an orphaned `stdin` reader thread racing `EventStream` and violates `TerminalGuard` startup order** — `src/app.rs:1945-1953`, `src/images.rs:116-127`, `src/terminal.rs:3-6`
   - `Graphics::detect` (`src/images.rs:116-127`) calls `Picker::from_query_stdio_with_options` with `QUERY_TIMEOUT = 1000 ms`. Inside `ratatui-image-11.1.0/src/picker.rs:486-527, 606-644`, `query_with_timeout` spawns a detached `std::thread::spawn` running blocking `io::stdin().read(&mut charbuf)` (50-byte buffer) and drops the mpsc receiver on `rx.recv_timeout(timeout)`.
   - Whenever a terminal (or SSH / slow emulator) does not answer within `1000 ms`, `detect()` returns `Halfblocks`, leaving that orphaned OS thread blocked on `stdin` when `EventStream::new()` (`src/app.rs:1953`) starts reading events. The user's first keypresses / escape sequences (up to 50 bytes) are swallowed or split mid-sequence by the lingering thread.
   - Compounding this, `src/app.rs:1945-1953` calls `guard.activate()?` *before* `Graphics::detect(config.images)`. This contradicts `src/terminal.rs:3-6` (`"must run after raw mode is on and before the crossterm event stream exists. TerminalGuard::enter does the first half; callers run their queries, then call activate"`). While the intent is running the query inside the alternate screen (`app.rs:1948-1950`), `activate()` (`src/terminal.rs:53-57`) also enables mouse capture (`EnableMouseCapture`) and focus change events (`EnableFocusChange`), so any mouse movement or focus event during the 1s query injects SGR/focus sequences into `stdin` while `Picker` is trying to parse terminal capabilities.
   - **Suggested fix:** Replace `Picker::from_query_stdio_with_options` with a custom Kitty APC + cell-size query loop driven by `poll(STDIN_FILENO, POLLIN, remaining)` (or non-blocking `stdin` read with deadline) so a timeout guarantees zero orphaned `stdin` threads. Run it before enabling mouse/focus capture (e.g., split alt-screen entry from mouse/focus/paste activation if scrollback isolation is needed for the query, then run `activate()`).

---

### Medium

3. **`App::reset_session()` does not clear `self.images` on logout or session expiry** — `src/app.rs:474-486`
   - `App::reset_session` clears rooms, timeline, composer, `user_id`, and `verify`, but leaves `self.images` untouched. Both `Setting::Logout` (`src/app.rs:464`) and `MatrixEvent::NeedLogin` (`src/app.rs:1378`) rely on `reset_session()`.
   - If the fullscreen image viewer (`self.images.viewer_open()`) is open when `SessionEnd::Expired` triggers `NeedLogin`, `Modal::Login` renders on top of `draw_viewer` (`src/ui/mod.rs:75-79`), and once the next login succeeds, the previous account's fullscreen viewer reappears.
   - Up to `64 MiB` / `256` entries (`CACHE_BYTES`, `CACHE_ENTRIES`), encoded Kitty payloads, unbounded `learned` entries, and active Kitty graphics IDs remain in `self.images` across logout/account switch; in-flight `Media::fetch` tasks (`src/matrix/media.rs:57`) are also detached and keep running until completion or 60s timeout after logout.
   - **Suggested fix:** In `reset_session()`, call `self.images.close_viewer()`, reset cached entries/learned sizes (e.g., `self.images.clear()` queuing deletes for any active Kitty IDs), and ensure `flush_images(false)` transmits the delete sequences cleanly. Consider passing a cancellation token or tracking download `JoinHandle`s in `Media` to abort pending downloads in `Timelines::close()`.

4. **`ImageStore::evict()` dropping `State::Failed` leaves `RowCache` permanently stale; `inline()` skips `evict()`; `learned` is unbounded** — `src/images.rs:406, 597-619, 640, 671, 708-730`, `src/ui/timeline.rs:198-211, 502-515, 631-639`
   - `ImageStore::evict()` (`src/images.rs:597-619`) removes LRU entries including `State::Failed` (once `CACHE_ENTRIES = 256` is exceeded), without incrementing `self.layout_rev`. Because `Slot::Failed` renders as a 1-row `Row { image: None, .. }` (`src/ui/timeline.rs:198-211`) whereas after eviction `slot()` returns `Slot::Cells` (`src/images.rs:491-509`), `RowCache` in `src/ui/timeline.rs:502-515` continues to hit `(room.id, timeline_rev, width, theme_rev, revealed, images.layout_rev)` and keeps rendering the stale 1-line error row instead of re-laying out and re-requesting (`draw()` lines `634-637` only request rows with `row.image.is_some()`).
   - `ImageStore::inline()` (`src/images.rs:708-730`) encodes into an existing `State::Ready { encoded, .. }` entry without calling `self.evict()`, so encoding large images can push total cache bytes past `CACHE_BYTES` until the next `insert()`.
   - `learned: HashMap<String, (u32, u32)>` (`src/images.rs:406, 640, 671`) intentionally outlives `entries`, but has no size cap and grows monotonically over a long session.
   - **Suggested fix:** Bump `self.layout_rev += 1` inside `evict()` whenever a `State::Failed` entry is removed; call `self.evict()` after `inline()` encodes a stale entry; bound `learned` with a generous cap (e.g., 2,048–4,096 entries).

5. **1-channel image bombs (`16384×16384`) bypass `MAX_DECODE_ALLOC` at `into_rgba8()`, amplified by `PARALLEL = 4`** — `src/images.rs:38-41, 190-228`, `src/matrix/media.rs:20`
   - `decode()` (`src/images.rs:190-228`) configures `Limits` (`MAX_DECODE_SIDE = 16_384`, `MAX_DECODE_ALLOC = 256 MiB`) and re-checks `decoder.total_bytes() > MAX_DECODE_ALLOC` (`line 207`). That catches standard RGBA bombs and oversized headers well.
   - However, a 16,384 × 16,384 8-bit grayscale (`L8`) PNG reports `decoder.total_bytes() == 268,435,456 == MAX_DECODE_ALLOC` (passes), then `apply_orientation` / `into_rgba8()` (`lines 214, 226`) allocates ~1 GiB (`16384 × 16384 × 4` bytes) outside `image::Limits` before `thumbnail()` shrinks it. With `PARALLEL = 4` (`src/matrix/media.rs:20`), up to 4 concurrent `spawn_blocking` decodes can stack temporarily.
   - **Suggested fix:** Before `DynamicImage::from_decoder`, check decoded RGBA memory (`let (w, h) = decoder.dimensions(); u64::from(w) * u64::from(h) * 4 > MAX_DECODE_ALLOC`), or lower `MAX_DECODE_SIDE` (e.g., `8_192`), or call `thumbnail` *before* `into_rgba8()` when `original` exceeds `max` (though note `thumbnail()` on non-RGBA works via `DynamicImage::thumbnail`, converting only the scaled-down result to RGBA8 drastically cuts peak memory!).

---

### Low / Minor

6. **`Ctrl-Z` suspend drops Kitty cleanup before sleeping and re-queues after `fg`** — `src/app.rs:1971-1976`
   - `terminal::suspend(&mut terminal)?` runs *before* `app.images.forget_encoded()`, so active Kitty image IDs are never deleted before leaving the alternate screen (meaning if the user kills the suspended process from the shell instead of foregrounding, terminal image IDs remain until Kitty cleans up). Calling `app.flush_images(true)` before `terminal::suspend` matches the clean shutdown path at `src/app.rs:2007`.
7. **Unbounded `std::thread::spawn` burst on viewer resize** — `src/images.rs:876-881`
   - `viewer_content` only guards against two concurrent threads for the *exact same* `size`, so dragging a terminal across many sizes while the viewer is open spawns one OS thread per intermediate `(cols, rows)` size to resize and Kitty-encode the same pixels.
8. **`verify()` assumes only cell `(area.x, area.y)` determines occlusion** — `src/ui/image.rs:48-52, 216-222`, `src/images.rs:744-766`
   - Confirmed safe for all current overlays: `covered` (`src/ui/timeline.rs:599`) holds new Kitty transmissions whenever a modal/help/viewer is open, and `loading_older` pill (`src/ui/timeline.rs:667-670`) starts at `content.x + 1` (covering `area.x = content.x + 4` when on row 0). Still worth keeping in mind if future overlays overlap timeline cells without covering row `area.y` col `area.x` or without setting `covered`.
9. **Test coverage gaps**
   - `src/images.rs` and `src/app/media.rs` have solid unit tests for decode limits/bombs, LRU eviction, Kitty garbage collection, popup covering, and scroll anchoring (`the_view_stays_put_when_a_picture_below_turns_out_smaller`).
   - Add tests for: (a) `evict()` of `State::Failed` rebuilding `RowCache`; (b) `reset_session()` closing the viewer and clearing cached entries; (c) streaming download byte-limit enforcement (once added in `matrix/media.rs`). Note `src/matrix/media.rs` currently only has one `#[ignore]` integration test.

---

## Verdict

**approve with changes** — The architecture, scroll anchoring, Kitty Unicode placeholder lifecycle, decode bounds, and snapshot rendering (`demo_has_a_picture.snap`) are solid, but the two High findings (post-download/post-SQLite size check in `matrix/media.rs` and orphaned `stdin` query thread + startup order mismatch in `app.rs`/`images.rs`) and `reset_session()` image cleanup should be fixed before pushing.
