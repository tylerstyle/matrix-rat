# Global Full-Text Search Handover (branch `agy/global-search`)

- **Worktree:** `/home/tylerstyle/git/matrix-rat/.claude/worktrees/agy-global-search`
- **Base commit (`7024c9c`):** M13 fixes (`main` at branch creation)

## Step Zero Gate Measurements

### (a) Live Functional & Privacy Verification (`global_search_spike_encrypted_edits_redactions_and_restart`)
- **Command:** `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=alicepass cargo test matrix:: -- --ignored --test-threads=1`
- **Result:** **18 passed; 0 failed** (`63.51s`), including `matrix::search::tests::global_search_spike_encrypted_edits_redactions_and_restart`:
  - Indexes both **unencrypted** and **E2E-encrypted** rooms via `matrix-sdk 0.19.1` `"experimental-search"`.
  - Excludes UTD placeholders before keys arrive and indexes them once decrypted late (`RedecryptorReport::ResolvedUtds`).
  - Updates index on message **edits** (`m.replace`) so old body stops matching and new body matches, and removes **redacted** messages from results.
  - Verifies **zero plaintext leakage** across all files under `paths.search_dir()` (`walk_contains`).
  - Persists searchable index across **restart** (`Worker` shutdown -> restart with encrypted store/search dir) and removes `paths.search_dir()` cleanly on **logout / forget / move-aside**.

### (b) Stripped Release Binary Size & Wall Time (`hpfury`, `nix-shell shell.nix`, `[profile.release]` with `lto = "fat"`, `codegen-units = 1`, `strip = true`)
- **Baseline (`5f72b84`, `main` profile):** `64,404,744` bytes (`965 s`)
- **Step Zero spike (backend + `experimental-search`, before UI):** `69,399,144` bytes (`+4,994,400` bytes / `+4.99 MB` / `+4.76 MiB`, `970 s`)
- **Final build (with full UI + `experimental-search`):** `69,423,720` bytes (`+5,018,976` bytes / `+5.02 MB` / `+4.79 MiB`, `694 s`)
- **Gate status:** **PASSED** (`+5.02 MB`, well within `<= ~8 MB`).

---

## Findings & Key Decisions

1. **SDK UTD Late-Decryption Gap (`matrix-sdk 0.19.1`) & Workaround (`src/matrix/search.rs`, `src/matrix/mod.rs`):**
   - In `matrix-sdk 0.19.1`, `Redecryptor::on_resolved_utds` saves newly decrypted events via `state.save_events` and drains `store_updates().take()` inside `state.replace_in_memory_utds`, bypassing `RoomEventCacheLinkedChunkUpdate` where the SDK's `search_indexing_task` listens.
   - `Searcher::new` (started in `src/matrix/mod.rs` right after `SyncService::builder(...).build().await`, stopped via `searcher.stop()` on teardown) spawns `index_redecrypted` subscribing to `client.event_cache().subscribe_to_decryption_reports()` and feeds `RedecryptorReport::ResolvedUtds { room_id, events }` into `client.search_index().lock().await.bulk_handle_timeline_event(...)`.
2. **Scope vs. `with_room_list_timeline_limit(20)` (`src/matrix/mod.rs:849`):**
   - Because `SyncService` initializes per-room event caches with `.with_room_list_timeline_limit(20)`, only events passing through the event cache (initial 20 per room, live sync, and any paginated rooms) are indexed locally.
   - Kept the device-local scope transparent in the empty-query state (`"Searches messages loaded on this device"`).
3. **Default Key Binding (`Ctrl-G`, `Action::SearchEverywhere` / `"search_all"`):**
   - `src/keymap.rs` rejects `Ctrl-Shift-F` (legacy/normalized terminals fold `Ctrl+Shift+letter`), and `Alt-F` is already `word_right()` in `LineEdit` and `Composer` (a `Context::Global` `Alt-F` binding would shadow composer/input word-forward navigation).
   - `Ctrl-G` was unbound across `DEFAULT`, `VIM_EXTRA`, `LineEdit`, and `Composer`, and sits naturally beside `Ctrl-F` (`Action::Search`, `"Find in this room"`).
4. **Encrypted Index Lifecycle & `Debug` Redaction:**
   - Added `Paths::search_dir()` (`<data>/search`) in `src/paths.rs`.
   - `offline_client` (`src/matrix/session.rs`, used by both fresh login commit and restore) attaches `.search_index_store(SearchIndexStoreKind::EncryptedDirectory(paths.search_dir(), passphrase.to_owned()))` with the exact same passphrase used for `.sqlite_store`.
   - `clear_with`, `forget_with`, and `move_aside_store` delete `paths.search_dir()`.
   - `MatrixCmd::SearchMessages`, `MatrixEvent::SearchResults`, `SearchHit`, and `GlobalSearch` implement manual `fmt::Debug` that redacts queries, snippets, and notes. No queries or message text are logged.
5. **Help Overlay Fit (`src/ui/mod.rs`, `src/ui/snapshots/`):**
   - Prior to `Ctrl-G`, `help_overlay` at `100x30` already occupied all `28 * 2 = 56` rows cleanly. Adding `Ctrl-G` brings the two-column layout to 57 lines (`29` in column 2), so at `100x30` the help popup now uses its built-in scroll (`↑↓ scroll · Esc to close`) by 1 row. Updated `every_key_in_the_help_can_be_reached` to check un-scrolled fit at `(100, 32)` and verify scrolling reachability at `(100, 30)` plus the smaller sizes.
6. **`default.nix` Note:**
   - `Cargo.lock` only added `crates.io` (`registry+https://github.com/rust-lang/crates.io-index`) dependencies for `experimental-search`. `default.nix` uses `cargoLock.lockFile = ./Cargo.lock` and needed no `outputHashes` or other edits (verified release build under `nix-shell shell.nix`; standalone `nix-build` was not executed directly).

## Summary of Changes
- **Backend & Storage:**
  - `Cargo.toml`, `Cargo.lock`: enabled `matrix-sdk` feature `"experimental-search"`.
  - `src/paths.rs`: `Paths::search_dir()`.
  - `src/matrix/session.rs`: `SearchIndexStoreKind::EncryptedDirectory` in `offline_client`; directory removal in `move_aside_store`, `forget_with`, `clear_with`.
  - `src/matrix/convert.rs`: exposed `local_time` as `pub(super)` for hit date formatting.
  - `src/matrix/mod.rs`, `src/matrix/rooms.rs`: added `MatrixCmd::{SearchMessages, CancelSearch}` and `MatrixEvent::SearchResults` with redacted `Debug`; wired `Searcher` into `run_session`.
  - `src/matrix/search.rs` (new): `Searcher` (`start`, `cancel`, `stop`, `redecrypt_task`), streaming search in `SEARCH_BATCH_SIZE = 16` batches capped at `MAX_SEARCH_RESULTS = 200`, `sanitize_query` fallback retry on parse syntax errors, `load_event_with_replacements` / `extract_event_text` (incl. `m.replace` & poll questions), `window_snippet`, `first_term_match_span`, `query_terms`, unit tests + ignored live continuwuity suite.
- **State, Keymap, Palette & UI:**
  - `src/action.rs`, `src/keymap.rs`, `src/palette.rs`: added `Action::SearchEverywhere` (`"search_all"`, `Ctrl-G`, palette aliases `"search messages"`, `"global search"`, `"find across rooms"`).
  - `src/modal.rs`, `src/ui/hit.rs`, `src/ui/modal.rs`: added `Modal::GlobalSearch`, `HitId::GlobalSearch(usize)`, `InputTarget::GlobalSearchQuery`.
  - `src/app/global_search.rs` (new), `src/app.rs`: `GlobalSearch` modal, `LineEdit` input/paste/click, `SEARCH_DEBOUNCE = 200 ms` deadline via `tokio::select!` + `on_tick`, generation tracking + cancellation, `Up`/`Down`/`PgUp`/`PgDn`/`Alt-P`/`Ctrl-N`/wheel/`HitId::GlobalSearch`/`HitId::Backdrop`/`Ctrl-G` toggle, `demo_global_search`, and `PendingJump` across room switches (space filter clearing, async timeline load, backward pagination up to `MAX_JUMP_DIG = 15` rounds).
  - `src/ui/global_search.rs` (new), `src/ui/mod.rs`: `78x22` popup, live `"searching…"`/`"N matches"`/`"no matches"`, 2-line hit cards (`room · sender` + right-aligned `date` + column-windowed highlighted snippet), `tiny_sizes_do_not_panic` coverage, and `insta` snapshots.
  - `README.md`: added `Ctrl-G` and `search_all`.

## Verification (Initial Handover `5937964`)
- `./scripts/check.sh` — **passed** (exit `0`): `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (**456 passed; 0 failed; 19 ignored**), `cargo build`.
- `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=alicepass cargo test matrix:: -- --ignored --test-threads=1` against local continuwuity (`http://127.0.0.1:6167`) — **18 passed; 0 failed**.
- `hpfury` `nix-shell shell.nix --run "cargo build --profile release"` — **passed** (`69,423,720` bytes stripped binary, `694 s`).

---

## Review Fixes (post-`5937964`)

1. **[MEDIUM] Thread-reply hits (`src/matrix/search.rs`, `src/ui/global_search.rs`, `src/app/global_search.rs`, `src/app/thread.rs`, `src/app.rs`):**
   - **Canonical event ID & thread root extraction (`src/matrix/search.rs`):**
     - Added `pub thread_root: Option<String>` to `SearchHit` (included in `Debug` as `thread={}` while keeping `room_name`, `sender`, `date`, and `snippet` redacted).
     - `matrix-sdk 0.19.1`'s Tantivy `SearchIndex` stores `IndexableEvent.primary_key` as the latest edit event ID (`m.replace`) when a message is edited, whereas timelines key the message by its original `ItemRef::Event($orig)` and `m.relates_to` (`rel_type: "m.thread"`) lives on the original event. `resolve_hit` now unwraps `Relation::Replacement` via `replacement_target` to the original event, extracts `thread_root` via `extract_thread_root` / `thread_root_from_raw` (covering both typed `Relation::Thread` on `RoomMessage` and generic JSON `content["m.relates_to"]` with `rel_type == "m.thread"`), resolves the canonical `event_id` + original timestamp + validated latest replacement text, and `run_search` deduplicates by `(room_id, hit.event_id)`.
   - **UI indicator (`src/ui/global_search.rs`):**
     - Hit header lines render `" · thread"` (` · ` muted + `thread` with `Role::Link`) when `hit.thread_root.is_some()` and `head_avail >= date_w + 15`, subtracting `thread_w` from `meta_cap` before `split_header`. Added unit test `draw_marks_thread_reply_hits_without_marking_plain_hits`.
   - **Two-stage thread jump (`src/app/global_search.rs`, `src/app/thread.rs`, `src/app.rs`):**
     - Extended `PendingJump` with `thread_root: Option<String>` (and updated demo search hits `demo:{idx}:{ridx}` for thread replies).
     - `activate_global_search` reuses the already-open matching thread (`self.thread_key() == root`), or reuses the already-open room timeline, or switches rooms first (`open_room`).
     - `try_apply_pending_jump` handles two stages when `thread_root` is `Some(root)`: locate or backward-paginate up to `MAX_JUMP_DIG` rounds for `root` in the main room timeline -> call `self.open_thread(root_pos)` -> locate or backward-paginate `MatrixCmd::Paginate { kind: PaginationKind::InThread { root }, .. }` for the target reply `event_id`.
     - Moved `self.timeline.reached_start |= reached_start;` ahead of `self.on_jump_timeline();` in `src/app.rs`'s `MatrixEvent::Timeline` handler so reaching the start of history stops digging on that update.
     - Tested at app level (`thread_reply_hit_opens_room_digs_for_root_opens_thread_and_selects_reply`, plus demo-mode thread jump check) and in the live continuwuity test (`global_search_spike_encrypted_edits_redactions_and_restart`: encrypted thread root + thread reply persisted across restart, edited hit resolves canonical original `event_id`).

2. **[LOW] Stale `pending_jump` cancellation (`src/app.rs`, `src/app/thread.rs`, `src/app/media.rs`, `src/app/global_search.rs`):**
   - Cleared `self.pending_jump = None` on user actions while digging: `perform` (all actions except `Quit` / `Suspend`), `open_modal`, `open_thread`, `leave_thread`, `open_image`, `click_image`, `on_key` (`Ctrl-C` / completion / composer / sidebar filter `EditOutcome != Ignored`), `type_char`, `edit_composer`, `on_paste` outside modals, and `on_mouse` (interactive left-clicks and timeline/sidebar wheel scrolling).
   - Added app-level test `pending_jump_dropped_on_selection_typing_scroll_and_popup_while_digging`.

3. **Left-room hits & on-disk search index pruning (`src/matrix/search.rs`, `src/matrix/session.rs`, `src/matrix/mod.rs`):**
   - **Can hits from a left/forgotten room appear?** **No.** In `matrix-sdk 0.19.1`, `GlobalSearchBuilder::new` (`src/message_search.rs`) queries `client.rooms_filtered(RoomStateFilter::JOINED)`, and `resolve_hit` (`src/matrix/search.rs`) also skips any room where `room.state() != RoomState::Joined`.
   - **Does the SDK prune `<search_dir>/<room_id>` when a room is left or forgotten?** **No.** `Room::forget` calls `base_client().forget_room(room_id)` and `event_cache().forget_room(room_id)`, neither of which touches `SearchIndex`, and `SearchIndex` keeps its `room_indexes` map private with no room-removal method. Removing a room directory while `Client` is live would leave a dangling in-memory `RoomIndex` handle if the user rejoined the room in the same session.
   - **Fix implemented:** Added `prune_search_dir`, `joined_room_ids`, and `prune_stale_search_indexes` in `src/matrix/search.rs` to delete stale `<search_dir>/<room_id>` directories for non-joined rooms safely at lifecycle boundaries where `SearchIndex` has no open writers/handles for those rooms: right after `client.restore_session(...)` (before sync/search starts) in `resume_with` / `commit_preserved_reauth` (`src/matrix/session.rs`), and in `Worker::run_active_session` (`src/matrix/mod.rs`) on `SessionEnd::Shutdown` and `SessionEnd::SoftLogout` after capturing `joined_room_ids` and dropping `active`. Covered by unit test `prune_search_dir_removes_only_non_joined_room_dirs` and live server assertions (leaving `plain_room` immediately excludes its hits from results and prunes `<search_dir>/<plain_room_id>` on `Shutdown` while retaining `<search_dir>/<enc_room_id>`).

4. **Intermittent unindexed encrypted thread replies (`matrix-sdk 0.19.1` `bundled_latest_thread_event` UTD overwrite bug, `src/matrix/search.rs`):**
   - **Root cause:** When an encrypted thread root and thread reply arrive before the room key (either during live sync racing `m.room_key` or before backup unlock) and are decrypted later via `Redecryptor`, `TimelineEvent::from_bundled_latest_event` returns an `m.room.encrypted` UTD if the parent root's `unsigned.m.relations.m.thread.latest_event` lacks `unsigned_encryption_info`. Inside `post_process_upserted_events` (`caches/room/state.rs` / `caches/thread/state.rs`), the SDK unconditionally calls `save_events([bundled_thread])` (`INSERT OR REPLACE INTO events`), overwriting the already-decrypted thread reply row in SQLite `EventCacheStore` with the encrypted UTD copy while keeping the decrypted event in memory (`room_linked_chunk`). Because `matrix_sdk::search_index::handle_room_message` / `get_most_recent_edit` reads `cache.find_event_with_relations` (which queries `EventCacheStore` directly), it observes `m.room.encrypted` in SQLite and silently skips indexing the thread reply with no error.
   - **Fix implemented:**
     - `index_redecrypted`: resolves any UTD via `try_decrypt_utd`, ensures every decrypted event is present and non-UTD in `EventCacheStore` (`store_missing_or_utd` / `ensure_decrypted_in_store`) before each `bulk_handle_timeline_event` call (acquiring and dropping `client.search_index().lock()` strictly outside `event_cache_store().lock()` / `room_cache` calls), and re-checks up to 3 times if concurrent `post_process_upserted_events` clobbers the SQLite row.
     - `load_event_with_replacements`: uses `heal_utd_event` when `cache.find_event_with_relations` returns a UTD event or replacement, recovering from `cache.find_event` or `try_decrypt_utd` and healing `EventCacheStore` so snippet extraction during search resolution is likewise immune to SQLite UTD clobbering.
   - **Test coverage:** `global_search_spike_encrypted_edits_redactions_and_restart` now also sends a pre-B encrypted thread root + thread reply before backup unlock so `Redecryptor` decryption of a thread reply with `bundled_latest_thread_event` runs deterministically on every test execution.

### Verification (After Review Fixes)
- `./scripts/check.sh` — **passed** (exit `0`): `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (**461 passed; 0 failed; 19 ignored**), `cargo build`.
- `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=alicepass cargo test -- --ignored --test-threads=1 global_search_spike_encrypted_edits_redactions_and_restart` — **5/5 consecutive runs passed** (`8.70s`, `10.54s`, `10.23s`, `10.54s`, `9.51s`).
- `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=alicepass cargo test matrix:: -- --ignored --test-threads=1` — **18 passed; 0 failed** (`61.40s`).
