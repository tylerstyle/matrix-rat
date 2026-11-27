# Web app, phase 1: the core without the terminal and the OS

Part of 1.2.0 ([`PLAN.md`](PLAN.md) §6.2). Written 2026-11-26 from an inventory of the code at
`8b5ee15`, revised the same night after matrix-agy's review
(`.claude/agy/review-web-phase1-design.md`, Task 46: two blockers, two high, four medium
findings, all taken in; Task 47 re-review: OK, its four implementation notes taken in too). Phase 1 changes nothing a user of the terminal app can see; at its end
the core of matrix-rat builds for the browser (`wasm32-unknown-unknown`) and a web shell can be
added in phase 2.

## Where the code stands

- The drawing (`ui::draw(frame, &mut app)`) works with any ratatui backend, ratzilla's too, and
  most of `src/ui/**` and the state modules (`composer.rs`, `modal.rs`, `palette.rs`) call
  nothing platform-specific. **Not yet free:** `ui/image.rs` (ratatui-image), and `ui/doctor.rs`,
  `ui/attach.rs`, `ui/audio.rs`, `ui/composer.rs`, `ui/status.rs`, `ui/collect.rs`, `model.rs`,
  `menu.rs`, which import pure types or helpers from modules that also run programs or touch
  files (`doctor`, `upload`, `audio`, `files`, `notes` → `textcmd`).
- `app::run` (`app.rs:5380-5668`) is already a shell around `App`: `App` asks for effects
  (`take_editor_request`, `take_paste_request`, `take_audio_start`, `take_record_start`,
  `take_note_starts`, `take_summary_start`, `pending_copy`, `take_notices_with_icons`,
  `take_window_title`, …) and gets results back through `on_*`/`finish_*`. The only production
  `terminal.draw` is there.
- The Matrix worker sits behind `MatrixHandle` (`MatrixCmd` in, `MatrixEvent` out), started by
  `matrix::spawn_with` with `tokio::spawn`; the client is built in one place
  (`matrix/session.rs:222-279`).

What ties the rest to the terminal, the OS and tokio (non-test unless said):

| Tie | Size | Where |
|---|---|---|
| crossterm key/mouse types | 28 non-test + 17 test-only files; key types 664 lines | `app.rs`, 22 `app/*.rs`, `keymap.rs` (~117), `lineedit.rs`, `doctor.rs`, `terminal.rs`, `termquery.rs` |
| `std::time::Instant` (panics in the browser) | ~95 `now()`, 211 lines | app fields, `app/*`, `images.rs`, `notify.rs`, `highlight.rs` (inside the draw), matrix |
| `SystemTime::now`, `process::id` | 25 | core: `images.rs:710-713`, `matrix/notify.rs:172`; the rest temp-file names (native) |
| tokio spawn / `JoinSet` | ~74 sites, ~63 in `src/matrix`; 12 worker `JoinSet`s | worker, `app.rs` loop |
| tokio time | ~128 | matrix ~76, app ~13 |
| `spawn_blocking` | 16 | 13 in matrix (3 of `media.rs`'s 5 decode pictures), `app.rs` ×2, `audio/record.rs` |
| `+ Send` bounds besides spawn | — | `matrix/verify.rs:972` (`SasStream`), `manage.rs:921,1463-1514`, `paging.rs:29`, `rooms.rs:274,280,555,570,608`, `verify.rs:668` |
| tokio process/net/signal, libc, zbus, notify-rust, signal-hook | — | audio, textcmd, catchup, agent, notify, portal, keyring, watch, termquery, clipboard |
| `std::os::unix`, `libc`, `etcetera` outside those | — | `matrix/devices.rs:10,784,846,864`, `upload.rs:6,76`, `files.rs:116,127,169,177,229`, `paths.rs:6,62`, `audio.rs:221,275`, `notes.rs:20`, `saved.rs:559,580`, `matrix/session.rs:4,283` |
| native-only error types | — | `logsafe.rs:165-194,302-310` (tokio `JoinError`/`Elapsed`, zbus, notify, `KeyringError`) |
| SDK features the browser lacks | — | `matrix/session.rs` (sqlite store, `LocalServerBuilder` from `sso-login`, search index), `matrix/search.rs:210` (`experimental-search`) |
| files | — | config, themes, notes, saved, session, stores, downloads, uploads, logs, lock |
| `App` calling the OS itself | ~35 sites | `open_in_browser` (4), `focus::raise_window`, `flush_images` → stdout, `save_setting` (~25 callers), `fs::metadata` (3), upload checks, `Theme::available` (`read_dir`), PATH probes at start **and when settings change** (`audio::choose_here`, `recorder::choose_here`, `catchup::choose_here`, transcriber), `audio::remove_leftovers` (`app/playback.rs:309`) |
| the worker deleting app data | — | `matrix/session.rs:365-371` removes `saved.json` and `notes.json` at logout |
| ratatui-image | `images.rs`, `ui/image.rs` | `rustix` (stdio, termios, fs) under `cfg(not(windows))`, not optional: cannot build for the browser |
| default features | `Cargo.toml` | ratatui's default pulls crossterm; matrix-sdk's pulls sqlite and aws-lc; matrix-sdk-ui's pulls aws-lc |

## The plan, in steps

Each step is its own series of commits on the branch `web-seams`, keeps the terminal app the
same (unit tests green, `scripts/check.sh` green, the server tests where the worker is
touched), and is reviewed by matrix-agy before the next starts. The native build keeps exactly
today's code paths: the new names re-export what is used now.

1. **Time.** `web-time = "1"` as a direct dependency (it is in the tree through ruma) and a
   `crate::time` module with the one `Instant` and `SystemTime` of the crate: `std::time`'s on
   native (so nothing changes), `web_time`'s in the browser. All core uses go through it,
   `matrix/notify.rs:172` included. chrono gets `wasmbind` for the browser only.
   `ImageStore::new`'s clock-and-pid name (`images.rs:710-713`) becomes an `AtomicU32` counter
   plus `RandomState::new().hash_one(…)`, as `audio/temp.rs:142` already does (no `rand`).
2. **Input events.** A `crate::input` module with our own `KeyCode`, `KeyModifiers`,
   `KeyEvent` (with `KeyEventKind` Press/Repeat/Release; `KeyEventState` is unused and left
   out), `MouseEvent`, `MouseEventKind`, `MouseButton` and an
   `InputEvent { Key, Mouse, Paste, Resize, Focus }`, **named and shaped like crossterm's where
   we use them**, so most of the change is the `use` lines. `KeyModifiers` is a small `u8`
   newtype with `NONE/SHIFT/CONTROL/ALT/SUPER/HYPER/META`, `|`, `&`, `contains`, `intersects`,
   `is_empty` (no `bitflags`). `From<crossterm::event::Event>` lives at the terminal edge only;
   phase 2 adds the ratzilla one. Release events are dropped exactly where they are today.
   `keymap.rs`'s tables, parser, labels and `validate_binding` (Ctrl-I = Tab etc., terminal
   rules kept for the terminal) move to the new types.
   **Lint policy:** `#![warn(clippy::wildcard_enum_match_arm)]` exempts foreign enums only.
   `input::KeyCode` and `input::MouseEventKind` stand in for foreign ones (a popup ignores the
   keys it does not use, and new variants come from the terminal, not from us), so they keep
   the exemption: the 41 `#[expect(…, reason = "foreign crossterm::…")]` (39 on `KeyCode`, 2 on
   `MouseEventKind`: `app.rs:4631`, `app/menu.rs:335`) keep their place with the reason "input
   events stand in for the terminal's (crate::input)". Our other enums stay matched in full.
   `KeyCode` copies all of crossterm's variants (media and modifier keys included), so the
   conversion is total and a key nobody binds still reaches `on_key` as today. Scope: 45
   files, tests included (helpers keep them short).
3. **Runtime.** A `crate::rt` module: `spawn`, `JoinHandle`, `JoinSet`, `sleep`, `sleep_until`,
   `timeout`, `timeout_at`, `interval`, taking `crate::time::Instant` for deadlines (so the
   three bridges at `app.rs:518`, `matrix/left.rs:326`, `matrix/rooms.rs:432` go away; there is
   no second `Instant`). On native: tokio, converting the deadline with `.into()`. In the
   browser: `wasm_bindgen_futures::spawn_local` and gloo timers (deadline → remaining duration
   in whole milliseconds, rounded up, clamped to `i32::MAX` ms: gloo hands `setTimeout` an
   `i32`, so `u32::MAX` as in `matrix-sdk-common/src/sleep.rs` would fire at once), and two
   things of our own, because the obvious substitutes are wrong:
   - **`JoinHandle::is_finished`:** matrix-sdk-common's browser handle answers
     `abort_handle.is_aborted()`, so a task that ended normally never reads as finished;
     `matrix/paging.rs:44` would wedge paging and `matrix/media.rs:274` would keep finished
     handles. Ours sets a flag from a guard inside the spawned task.
   - **`JoinSet`:** tokio's runs a task as soon as it is spawned, and our 12 worker sets only
     call `spawn`, `try_join_next` and `abort_all`, never `join_next`. A `FuturesUnordered` would
     leave the tasks unpolled. Ours spawns each task at once, keeps its handle and flag,
     `try_join_next` reaps finished ones without waiting, and dropping the set aborts the rest,
     as tokio's does.
   Bounds use matrix-sdk's `SendOutsideWasm`: on `spawn` and on the other `+ Send` bounds
   listed above (`SasStream`: `sas.changes()` is `!Send` in the browser). `spawn_blocking`
   stays native-only: the worker sites are file work and picture decoding that phase 2 handles;
   they get a cfg'd path, not a shim.
4. **Effects out of `App`.** What `App` does to the OS itself becomes a request the shell
   carries out, as the existing `take_*` pattern does: opening a URL, raising the window,
   writing image clean-up bytes (`flush_images`), removing audio leftovers. The OS probes, at
   start and when a setting changes them, go through a small `Probes` seam (truecolor,
   NO_COLOR, ssh, player, recorder, transcriber, translator, summary command); in the browser
   it answers "not available". Persistence goes through a synchronous `Storage` trait held by
   `App`, with typed keys (`Config`, `Theme(name)`, `Notes`, `Saved`) and operations `read`,
   `write`, `remove`, `list`, `set_aside` (for `saved.rs`'s rename of a damaged or other
   account's file), all returning `Result` (the browser's `localStorage` can refuse: quota,
   private mode). On native `FsStorage` keeps exactly today's behaviour: `config_dir` shared
   (0644) vs `data_dir` per profile (0600, directories 0700), tmp + rename, `toml_edit`
   keeping comments. The logout clean-up of `saved.json` and `notes.json` moves from the worker
   (`matrix/session.rs:365-371`) to `App`, through `Storage::remove`, **only for an explicit
   logout**: today `SessionEnd::Expired` and `SessionEnd::LoggedOut` both end in
   `MatrixEvent::LoginRequired` (`matrix/mod.rs:1567-1597`), and an expired session keeps
   `saved.json` (`session.rs:363-364`), so the worker gets a distinct logged-out event or `App`
   clears them when it asked for the logout. The stores and the search index stay the worker's.
5. **Pure types apart from runners.** Before anything is cfg'd out, the modules that mix data
   with programs or files are split, so step 7 becomes mechanical: `notify` (`NotifyMode`,
   `AppIcon`, `NotifyIcon`, `DueReminder` vs the D-Bus/OSC senders; `IconCache`, an on-disk
   0600 PNG cache that `App` only holds to clear and to hand back with the notices, moves to the
   terminal shell instead of being split), `audio`
   (`TempAudio`, `Control`, `End`, `PlayerEvent`, `PlayerUpdate`, `Reported`, `Request`, record
   types, `clock`/`length`/`bars`/`played`/`WAVE_BARS` vs players, recorders, temp files),
   `doctor` (report, popup, `Found` vs the probes), `matrix/keyring` (`SecretStoreMode` vs the
   Secret Service), `files` (`LARGE_FILE`, `safe_name`, `opens_safely` vs the writers),
   `upload` (`Chosen`, `too_large`, `command_argument`, `dropped_files` vs file opening;
   `App` also calls `upload::check`, `command_files`, `complete`, `prepare` at `app/attach.rs:261,
   375,427,500`, `app/paste_picture.rs:66`, `app.rs:5294-5333`, which go through step 4's
   boundary or get browser stubs in 7b), `images` (`Pixels`, `Decoded`, `Graphics`,
   `Protocol`, the size constants, `human_size`, `fit_cells`, decoding vs ratatui-image,
   termquery and the encoder thread), `notes`/`textcmd` (`Command`, `Step`, `Failure`,
   `BadCommand`, `MAX_OUTPUT` vs running), `catchup` (data vs running), `editor` (`Request` vs
   running), `clipboard` (data vs `wl-paste`/`xclip`), `paths` (the profile's names vs
   `etcetera` and permissions).
6. **The shell.** `app::run` moves to `src/shell/terminal.rs` (with `terminal.rs`,
   `termquery.rs`, the editor/suspend handling, signals, the theme/portal watchers, the
   desktop notifications and the audio/note/summary runners), leaving `App` with a
   platform-free surface: `on_input`, `on_matrix`, `on_tick`, the `take_*` effects,
   `ui::draw`, `after_draw`. What step 5 left in `App` for this step: `IconCache` (its own
   file, `notify/icon_cache.rs`) together with `notify_mode.resolved()` (the shell says
   whether desktop notices are on, or resolves the icons when it sends them), the WAV copy a
   transcription makes (`TempAudio::create` in `app/notes.rs`), mpv's socket name
   (`audio::socket_path` in `app/playback.rs`), and `Graphics::detect_with_term`.
   **Done** (2026-11-27, `e822212..927c5c8`): `src/shell.rs` with `shell/terminal.rs` (the old
   `terminal.rs`, its panic hook and tests unchanged, plus the event loop `run`, `carry_out`,
   `flush_images`), `shell/terminal/termquery.rs`, `shell/terminal/external_editor.rs` (the
   worker's news held during the editor's turn; `App`'s half stays in
   `app/external_editor.rs`) and `shell/terminal/icon_cache.rs`. `main.rs` calls
   `shell::terminal::run`. What the loop calls of `App` is `pub(crate)`, nothing renamed.
   The loop alone holds the icon cache (`App` asks for its clearing with
   `OsRequest::ClearNoticeIcons`), names mpv's socket and makes the WAV file of a
   transcription (`take_wav_requests`/`on_wav_file`); what `auto` notifications are here is
   a new probe, `Probes::notify_auto`. `Graphics::detect_with_term` went with `run`.
   Deliberately not moved: the runner halves from step 5 (`audio::run`,
   `audio::record::run`, `textcmd::run`, `catchup::run`, `editor`, `clipboard`), `notify`'s
   senders, `portal`, `watch`, `focus` and `images/detect.rs`. Only the shell calls them now;
   moving their files would be renames that 7b's `cfg` makes unnecessary. Left in `App` for 7b, as
   listed there: the key file checks (`app/devices.rs`), the saved download's
   `path.exists()` (`app/files.rs`), the `upload` checks and `files::display`/`resolve_dir`,
   and the images' encoder thread (`images/store.rs`).
7. **Native-only behind `cfg`, as a library, with the gate.** Two parts, each reviewed:
   - **7a, on native only:** the modules move from `main.rs` into `src/lib.rs`, which takes the
     crate-level attributes (`recursion_limit`, the two test-only allows, the
     `wildcard_enum_match_arm` warning; `main.rs` keeps them for itself); `main.rs` becomes the
     CLI using the library, and its ~650 lines of tests move into the library modules they
     reach into (no `#[doc(hidden)] pub` shims).
     **Done** (2026-11-27, `fceec8b..8023f67`): `src/lib.rs` (the library `matrix_rat`)
     declares every module, all private, and has the attributes; `main.rs` keeps them and is
     the command line only (`Args`, `Command`, `main`, `client_command`). What it uses is
     public through one module of re-exports, `matrix_rat::cli`, under the paths the items
     have inside the crate (`cli::agent::client::send`, `cli::paths::Paths`,
     `cli::shell::terminal::run`, …), so `main.rs`'s code stayed the same and nothing else of
     those modules is public; rustdoc's `private_intra_doc_links` is allowed (the docs are
     read with `--document-private-items`). `build_runtime` and its limits moved to
     `src/runtime.rs` with their two tests: the ignored one that measures threads opens an
     SDK store and a theme watch, which the command line must not make public. The other five
     tests (clap's parsing, `client_command`'s exit codes) test `main.rs`'s own items and
     stay there. 1670 passed and 62 ignored, as before. `Cargo.toml`, `default.nix`, the
     workflows and `check.sh` needed no change.
   - **7b, the browser:** compiled only for `not(target_family = "wasm")`: `agent/**`,
     `matrix/agent.rs`, the runner halves from step 5, `portal`, `watch`, `lock`, `focus`,
     `termquery`, `logging`, the CLI (`main.rs`, and the library's `cli` and `runtime`);
     `logsafe.rs`'s arms for native-only error types (a
     browser arm for `crate::rt`'s timeout error); in `matrix/devices.rs` the key export and
     import, with the checks `App` makes of the key file's path (`app/devices.rs`: metadata,
     `is_dir`); the check that a saved download is still there (`app/files.rs`,
     `path.exists()`); `FsStorage` (Unix file modes) and the test-only `Storage::files` hook;
     `NativeProbes` (and the test probes); in `matrix/keyring.rs` the ssh check in
     `ResolvePassphraseError::user_message` (or the whole backend and migration part: the
     browser needs only `SecretStoreMode`); `audio/ipc.rs` with `Reported::take` moved next
     to the player. Pure helpers that only runners use stay where step 5 put them, under
     `cfg(any(not(target_family = "wasm"), test))`: in `paths`, `editor`, `clipboard`,
     `upload`, `audio` (and `recorder`, `record`), `images` and `notify` (the gate's
     dead-code warnings list them). Trivial browser stubs: `upload::dropped_files`,
     `files::display` and `resolve_dir`. `App` also calls `upload::check`, `command_files`,
     `complete` and `prepare` (the places are in step 5): they need browser stubs or a `cfg`
     too (matrix-agy, step 6 review).
     Browser stubs, so `matrix/mod.rs` still compiles: `matrix/session.rs` (shared
     `ReauthKind`, `ReauthTarget`, `ActiveSession`, `RestoreOutcome`, `ReauthOutcome`; phase 2
     fills in login and the IndexedDB stores), `matrix/search.rs` (shared `SearchHit`; no local
     index), `images`/`ui/image.rs` (a "[picture]" placeholder; a click opens it, phase 4 draws
     it). `Cargo.toml` by target: shared `ratatui` with `default-features = false` and
     `all-widgets`, `layout-cache`, `macros`, `underline-color` (+ `crossterm` on native);
     native keeps today's matrix-sdk features (`bundled-sqlite`, `sso-login`, `socks`, `qrcode`
     (unused in `src/`, kept for parity, its own decision later), `experimental-search`) and
     matrix-sdk-ui's default; the browser gets matrix-sdk and matrix-sdk-ui with
     `default-features = false` (`e2e-encryption`, `automatic-room-key-forwarding`, `js`,
     `indexeddb`; ui: `js`), tokio with `sync`/`macros`, chrono `wasmbind`, gloo-timers,
     `wasm-bindgen-futures`, and `tracing-subscriber` without `env-filter`. `clap`, `etcetera`,
     `shlex`, crossterm, ratatui-image, notify, notify-rust, zbus, libc, signal-hook,
     tracing-appender and tokio's rt-multi-thread/time/process/net/signal/io-std/io-util are
     native only. getrandom 0.3/0.4 need `getrandom_backend="wasm_js"` (`.cargo/config.toml` under
     `[target.wasm32-unknown-unknown]`) and 0.2 its `js` feature. The gate:
     `cargo check --lib --target wasm32-unknown-unknown`, in `scripts/check.sh` (nixpkgs' rustc
     has the browser std; `cargo check` needs no linker) and in CI.
     **Done** (2026-11-27, `ea2b670..345d927`): `Cargo.toml` by target as written; the lock
     gained one edge (`growable-bloom-filter` under matrix-sdk-indexeddb), and native
     `cargo tree -e features` is the same as before for x86_64 and aarch64. Native only as
     listed (`termquery` with `shell::terminal`); the runner halves with their re-exports.
     Browser stand-ins: `matrix/auth.rs` holds the login types both builds share (and
     `SecretStoreMode`; `session.rs` and `keyring.rs` re-export them) and the browser's
     `session` (no session to restore, logins not available yet); a search ends at once with a
     note; the picture encoders make nothing (`Encoded`'s protocol is an enum without
     variants there), so every picture keeps its placeholder; `files::write_new`,
     `NewFile::create`, `TempAudio::create`, the key export and import and `upload`'s disk
     calls refuse; each `spawn_blocking` of the worker does its work in place; the media
     download reads the body whole (reqwest has no `chunk` there). Beyond the plan:
     matrix-sdk's errors are not `Send` in the browser (`Error::UnknownError` holds a
     `Box<dyn Error>`), so `anyhow` cannot take them: `matrix/sdk_error.rs` gives 25 places
     `.sdk()?`/`.sdk_context(..)`, which are `?`/`.context` on native and copy the error
     (texts, causes, `logsafe`'s `Why`) in the browser; `LeftRooms` uses matrix-sdk's
     `BoxFuture`; `logsafe` has no `is_connect` there. Left compiled, not `cfg`'d: the key
     file checks (`app/devices.rs`) and `path.exists()` (`app/files.rs`), as std answers
     "unsupported" in the browser and the worker refuses anyway; `App::image_cleanup`.
     `App::start_images`/`on_image_encoded` (the encoder thread) are native only.
     `matrix_rat::core` is public on both builds: `App`, `OsRequest`, `Config`, `Paths`,
     `SecretStoreMode`, the input types, `spawn_with`/`MatrixHandle`/`MatrixEvent`,
     `Storage` (with its key types), `Probes`, `ui::draw`, `ColorScheme`, `time::Instant` and
     rt's `interval`, `MissedTickBehavior`, `sleep_until`; the `App` methods the shell calls
     became `pub`. The remaining dead code in the browser was runner-only helpers, now under
     `cfg(any(not(target_family = "wasm"), test))`, plus three `expect(dead_code)` for the
     browser (`EncodeDone`, `RestoreOutcome`, `threads::SUPPORT`) and three
     `expect(unreachable_code)` after an uninhabited `NewFile` or `Encoded`. No
     `.cargo/config.toml`: getrandom 0.4 takes its backend from the `wasm_js` feature, which
     matrix-sdk's `js` turns on (and 0.2's `js`). The gate runs with `-D warnings` in
     `check.sh` and in a `browser` job in `checks.yml`; nixpkgs' rustc 1.99.1 has the browser
     std, so `shell.nix` is unchanged. `cargo deny check` passes (its graph is the native
     targets); for the browser's graph it reports `derivative` unmaintained
     (RUSTSEC-2024-0388, via matrix-sdk-indexeddb), not ignored. 1670 passed and 62 ignored.

## Not in phase 1

- The worker protocol's file paths (`OutgoingFile{path}`, `SaveFile{dir}`, `FetchAudio{dir}`,
  `AudioFetched(TempAudio)`, `SetOwnAvatar{path}`, key export/import, archive export): they
  compile in the browser (std's path types exist) and get byte/Blob variants in phase 2
  together with the browser's file picker and downloads.
- Login by OAuth redirect, the IndexedDB stores, session restore and the `localStorage`
  `Storage` in the browser (phase 2, from the phase 0 test build), and the ratzilla shell.
- The search index (`experimental-search`, tantivy): native only; the browser goes without
  local search at first.
- A crate split into a workspace: not needed for the gate; can come later if `cfg` gets
  crowded.

## Open questions

1. **Merging into `main`** (for the user). matrix-agy's view, which I share: nothing of
   `web-seams` goes into `main` before phase 0 says "go" (the browser indirections should not
   ship in 1.1.0 if phase 0 fails). After "go": if 1.1.0 holds only notify 9 and the dot, ship
   1.1.0 first and merge the steps right after (they barely meet `watch.rs` and
   `ui/timeline.rs`); if the `--mobile` work (keymap, `app/*`, `ui/*`) goes into 1.1.0 after
   "go", merge the reviewed steps first, so it builds on `crate::input` and `Probes`.
2. **What the phase 0 test build finds** about matrix-sdk 0.19.1 in the browser (features,
   `!Send` futures, IndexedDB) may change steps 3 and 7.
