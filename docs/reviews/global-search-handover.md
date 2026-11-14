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

## Verification
- `./scripts/check.sh` — **passed** (exit `0`): `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (**456 passed; 0 failed; 19 ignored**), `cargo build`.
- `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=alicepass cargo test matrix:: -- --ignored --test-threads=1` against local continuwuity (`http://127.0.0.1:6167`) — **18 passed; 0 failed**.
- `hpfury` `nix-shell shell.nix --run "cargo build --profile release"` — **passed** (`69,423,720` bytes stripped binary, `694 s`).
