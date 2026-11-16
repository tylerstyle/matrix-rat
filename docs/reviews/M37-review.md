# M37 Review — 0.8 Leftovers & Build Checks (`leftovers-09`)

- **Branch:** `leftovers-09` (`ebeb79a`, `6b641e3` on `main` `188362e`)
- **Worktree:** `.claude/worktrees/leftovers`
- **Scope:** Code review only (no code edited, no builds/tests run; `fury-check` reported green with `1105` unit tests and `cargo deny check` reported ok).

---

## 1. Three leftovers from the 0.8 reviews (`ebeb79a`)

### 1.1 `Sound::belongs_to` across room and thread views (`src/app/playback.rs`)
- **Checked:** `Sound` (`src/app/playback.rs:80-91`), `Sound::belongs_to` (`95-106`), `AudioState::is` (`177-181`), `App::audio_in` (`265-277`), `App::start_audio` (`299-348`), `App::open_thread` (`src/app/thread.rs:208-213`), and `a_thread_shows_its_first_message_playing` (`src/app/playback.rs:722-755`).
- **Assessment:** Clean and accurate.
  - When a thread opens, `open_thread` rewrites the thread root's `id` to `ROOT_ID` (`first.id != root_message.id`), while preserving `ItemRef::Event(id)` (`event_id()`) and `file.key`.
  - Storing `event: message.event_id().map(str::to_owned)` on `Sound` and matching `(self.message_id == message.id || event_id match) && file.key == self.key` lets playback indicators, `AudioCaps::selected`, and `x` toggle work consistently both room -> thread and thread -> room, while keeping `message_id` matching for demo/unconfirmed messages (`event == None`) without ever falsely matching `None == None`.

### 1.2 Multi-row audio head continuation (`src/ui/audio.rs`, `src/ui/timeline.rs`)
- **Checked:** `HeadRow` & `head_row` (`src/ui/audio.rs:51-117`), `AudioPart` (`src/ui/timeline.rs:48-58`), `sound_row` (`172-206`), `build_layout` (`624-656`), `draw` (`1503-1557`), `receipts::add` (`src/ui/timeline/receipts.rs:35-41, 60-62, 80-82`), and `a_long_name_goes_on_over_the_rows_its_line_took` (`src/ui/audio.rs:293-331`).
- **Assessment:** Clean and width-safe.
  - `build_layout` counts wrapped rows belonging to line 0 (`head_rows`) and records `(nth, of)` on each `AudioPart`.
  - `receipts::add` attaches a read marker exclusively to the **last** row of a message (`rows.get(at + 1)` check at `receipts.rs:39-41`). Consequently, whenever `of >= 2`, row `0` and middle rows never carry a marker (`part.marker == 0`), so `at.full == usize::from(rect.width)` accurately reproduces row `0`'s `first_width` and middle rows' `between` width when rendering row `at.nth`, while the final row (`nth + 1 >= of`, where `last == true`) uses `max_width = room_left` and truncates cleanly with `truncate(rest, here)`.
  - If the first row lacks 10 columns after the status (`!on_first`), the filename moves cleanly to row 1 (without ` · `); if row 0 fits all of `rest`, continuation rows emit `""` after `INDENT`, so `draw` still blanks out the wrapped cached text and removes `Modifier::UNDERLINED` cleanly (`timeline.rs:1520-1531`). Clicking `■` stays pinned to `part.head && part.first` (`1547-1557`).

### 1.3 `text::wrap_at_spaces` & `src/ui/doctor.rs` (`src/text.rs:138-166`, `src/ui/doctor.rs:92, 110`)
- **Checked:** `wrap_at_spaces` (`src/text.rs:138-166`), its callers on `line.value` (`src/ui/doctor.rs:92`) and `line.hint` (`110`), and `wrap_at_spaces_keeps_hyphenated_words_whole` (`src/text.rs:273-297`).
- **Assessment:** Clean. Splitting on `' '` preserves hyphenated key labels and flags such as `Alt-Enter`, `Shift-Enter`, and `--profile` intact whenever `width(word) <= max`, while falling back to `wrap(word, max)` only when a single token (like a long filesystem path) exceeds `max`, never returning an empty `Vec` (`""` -> `[""]`). *(Non-blocking note: consecutive spaces and `\t` are treated as separators/single-token chars respectively, which matches all doctor report text.)*

---

## 2. Build checks (`6b641e3`)

### 2.1 `clippy.toml` `disallowed-methods` and the 15 `#[expect]` call sites
- **Checked:** `clippy.toml` (`1-19`), `#![cfg_attr(test, allow(clippy::disallowed_methods))]` (`src/main.rs:4`), `scripts/check.sh:7` (`cargo clippy --all-targets -- -D warnings`), and all **15** disallowed method calls across the **8** `#[expect]` items plus all callers:
  1. `src/watch.rs:125-162` (**1 call:** `Command::status` at `:162`) — called solely at `src/main.rs:237` for `--reload-theme` before any runtime or TUI exists; runs `kill -USR1`. **Reason verified.**
  2. `src/editor.rs:210-220` (**1 call:** `Command::status` at `:220`) — called solely inside `tokio::task::spawn_blocking` at `src/app.rs:4989-4991` after dropping `EventStream` and releasing raw terminal mode. **Reason verified.**
  3. `src/clipboard.rs:302-325` (**1 call:** `Child::wait` at `:325`) — preceded immediately by `let _ = child.kill();` (`:324`), and called solely inside `tokio::task::spawn_blocking` via `clipboard::paste` at `src/app.rs:5012-5014`. **Reason verified.**
  4. `src/clipboard.rs:330-380` (**1 call:** `std::thread::sleep` at `:380`) — private to `clipboard.rs`, called only from `run_tool` (inside `spawn_blocking`), 5 ms polls strictly gated by `Instant::now() < deadline`. **Reason verified.**
  5. `src/focus.rs:46-93` (**3 calls:** `std::thread::sleep` `:79`, `Child::wait` `:83`, `JoinHandle::join` `:91`) — private to `focus.rs`, called only on the detached `"raise-window"` thread spawned at `focus.rs:29-37`; `sleep` and `wait` end within `PATIENCE` (3s). *(Non-blocking detail: on `capture = true` — used only for `niri msg --json windows` at `:107` — `reader.join()` at `:91` has no extra timeout if a child somehow exited 0 while a descendant kept stdout open, though this still runs only on the detached `"raise-window"` thread and never blocks UI or async workers.)* **Reason verified.**
  6. `src/main.rs:203-267` (**2 calls:** `Runtime::block_on` at `:259`, `:261`) — top-level entry point blocking on `agent::serve::run` or `app::run`. **Reason verified.**
  7. `src/main.rs:272-390` (**5 calls:** `Runtime::block_on` at `:324, 330, 342, 360, 374`) — `client_command` runs synchronously on a current-thread Tokio runtime and immediately exits via `std::process::exit` (`main.rs:232`) before any UI/lock/worker starts. **Reason verified.**
  8. `src/notify.rs:468-490` (**1 call:** `futures::executor::block_on` at `:472`) — runs inside the single background `std::thread::spawn` loop created by `desktop()` (`notify.rs:464-491`), while callers use non-blocking `unbounded_send`. **Reason verified.**

### 2.2 `deny.toml` and `RUSTSEC-2026-0459`
- **Checked:** `deny.toml`, `Cargo.toml:12, 59`, `Cargo.lock`, `matrix-sdk-sqlite-0.19.1/Cargo.toml:133-134`, `rusqlite-0.40.2/Cargo.toml:362-363`, `libsqlite3-sys-0.38.2/sqlite3/sqlite3.h:149`, `src/paths.rs:63-66`, `src/matrix/session.rs:238`, `src/matrix/mod.rs:3084`, `default.nix:26`, `shell.nix:6`, and `docs/PLAN.md:239-275`.
- **Is ignoring `RUSTSEC-2026-0459` defensible?** **Yes:**
  1. **Upstream lock-in:** `matrix-sdk = "=0.19.1"` (`Cargo.toml:12`) depends on `matrix-sdk-sqlite 0.19.1`, which specifies `rusqlite = "0.40.2"` (`libsqlite3-sys 0.38.2`, bundling SQLite `3.53.2`). The upstream fix (`SQLite >= 3.54.0` / `libsqlite3-sys >= 0.39.0` / `rusqlite >= 0.41`) cannot be pulled in cleanly without bumping `matrix-sdk`.
  2. **Preconditions do not hold in `matrix-rat`'s threat model:** `RUSTSEC-2026-0459` (CVE-2026-119375, uninitialized page-cache heap disclosure when a `-wal` file declares a smaller page size than the main DB) requires opening a crafted DB/`-wal` file pair. `matrix-rat` never opens user-supplied or Matrix-received SQLite databases; it only opens its own store under `paths.store_dir()` (`session.rs:238`, `mod.rs:3084`) inside `paths.data_dir`, which `Paths::create_dirs()` (`src/paths.rs:63-66`) restricts to mode `0o700` (owner-only). Anyone able to tamper with `-wal` files there already has write access to `data_dir` (`session.json` and the store).
  3. **Documented & bounded exception:** Both `deny.toml:13-18` and `docs/PLAN.md:266-271` record the precondition and exact removal condition, and `cargo deny` warns when an ignored advisory stops matching. *(Non-blocking accuracy note: `docs/PLAN.md:270` says the Nix package links system SQLite `(3.54.0)`, which holds on a nixpkgs channel shipping `>= 3.54.0`; `deny.toml:17` wisely states `links the system's SQLite` without hardcoding a version.)*
- **Other two ignores (`RUSTSEC-2026-0319` `anymap2 0.13.0`, `RUSTSEC-2025-0141` `bincode 1.3.3`):** Neither is a security vulnerability (both are `unmaintained` notices, transitive only under `matrix-sdk 0.19.1` and `syntect 5.3.0` respectively, with `bincode` 1.3.3 marked final/complete upstream).
- **`[licenses]`, `[bans]`, `[sources]`, `[graph]`:** `Apache-2.0` + permissive / weak-file-copyleft licenses allowed (`confidence-threshold = 0.9`), `wildcards = "deny"`, `unknown-registry = "deny"`, `unknown-git = "deny"`, and `[graph]` targets match `release-binaries.yml` (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`).

### 2.3 `.github/workflows/checks.yml`
- **Checked:** `.github/workflows/checks.yml:1-31` and `.github/workflows/release-binaries.yml:33`.
- **Assessment:** Sound CI hygiene. Runs on `push` to `main`, `pull_request`, weekly `schedule` (`cron: "17 5 * * 1"`), and `workflow_dispatch`, with `permissions: contents: read`. Both `actions/checkout@11d5960a326750d5838078e36cf38b85af677262` (`persist-credentials: false`, matching `release-binaries.yml:33`) and `EmbarkStudios/cargo-deny-action@70f81531115945ba6eb6b941ec9d39d063438eb9` (`# v2.1.2`) are pinned by full 40-character commit SHA. *(Verified full-SHA pinning in the workflow file; `# v2.1.2` tag -> SHA mapping was not checked directly against git remote.)*

M37 ok

---

## Follow-up: the listener fault found with a real MCP client

Not by `matrix-agy`, which was not available. Four rounds by
independent reviewers started by `matrix-claude`, each given the code and the questions, not
the author's conclusions. Recorded here by `matrix-claude`.

**The fault.** With Claude Code as the MCP client, `wait_for_message` came back at once with
an old message: a `read` had loaded older messages into the watched room's newest hundred,
and `serve` told them to listeners as new.

| Round | Commit | Verdict | What it found |
|---|---|---|---|
| M37b | `fa8ed34` (`Seen::sift`, first page loaded by the watcher) | changes requested | An answer missed in a sync gap could now be lost for good; an empty baseline still told history; old messages told again for a change (trust, late key, deletion) ended a wait. |
| M37c | `71bfb2f` (the MCP server's ledger: known, handed, passed over) | changes requested | A failed first read was taken for an empty room, making all history news; a read that did not show a gap answer buried it; a cancelled wait lost one that could not be read yet; a deleted answer was counted for ever; the watcher skipped the settle wait at the room's start. No way found for another member to fake an answer. |
| M37d | `0196cdc` (owed messages, no empty-room anchor) | changes requested | All of the above confirmed fixed. New: a read during a wait left the anchor behind the answer, which was handed over twice; two rare cases with an anchor that is gone from the room; the empty-room price not listed as open. |
| M37e | `0264ec0` (anchor compared by place, the lost-anchor rules) | changes requested | All four confirmed fixed for a wait. The same two rare cases were open for `read_messages`, which owed old history when the anchor was not among what it read. One narrow race left as the lesser evil (an anchor moved by a concurrent read to a message the wait has not heard of). |

The last finding is fixed in the commit that adds this section, in the way the reviewer
proposed (the read uses the wait's guard), with a test. What stays open is listed in
`docs/PLAN.md` §6b.

M37 follow-up: closed by `matrix-claude` after M37e's finding was fixed.
