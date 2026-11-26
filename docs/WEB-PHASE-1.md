# Web app, phase 1: the core without the terminal and the OS

Part of 1.2.0 ([`PLAN.md`](PLAN.md) §6.2). Written 2026-11-26 from an inventory of the code at
`8b5ee15`. Phase 1 changes nothing a user of the terminal app can see; at its end the core of
matrix-rat builds for the browser (`wasm32-unknown-unknown`) and a web shell can be added in
phase 2.

## Where the code stands

- `src/ui/**` and the pure state modules (`model.rs`, `composer.rs`, `modal.rs`, `menu.rs`,
  `palette.rs`) call nothing platform-specific. `ui::draw(frame, &mut app)` works with any
  ratatui backend, ratzilla's too.
- `app::run` (`app.rs:5380-5668`) is already a shell around `App`: `App` asks for effects
  (`take_editor_request`, `take_paste_request`, `take_audio_start`, `take_record_start`,
  `take_note_starts`, `take_summary_start`, `pending_copy`, `take_notices_with_icons`,
  `take_window_title`, …) and gets results back through `on_*`/`finish_*`. The only production
  `terminal.draw` is there.
- The Matrix worker sits behind `MatrixHandle` (`MatrixCmd` in, `MatrixEvent` out), started by
  `matrix::spawn_with` with `tokio::spawn`; the client is built in one place
  (`matrix/session.rs:222-279`).

What ties the rest to the terminal, the OS and tokio (non-test lines):

| Tie | Size | Where |
|---|---|---|
| crossterm key/mouse types in app logic | 25 files; key types 664, mouse 29 | `app.rs`, 21 `app/*.rs`, `keymap.rs` (~117), `lineedit.rs` |
| `std::time::Instant` (panics in the browser) | ~95 `now()`, 211 lines | app fields, `app/*`, `images.rs`, `notify.rs`, `highlight.rs` (inside the draw), matrix |
| `SystemTime::now`, `process::id` | 17 | `images.rs:710-713` (core), temp-file names (native) |
| tokio spawn / `JoinSet` | ~74 sites, ~63 in `src/matrix` | worker, `app.rs` loop |
| tokio time | ~128 | matrix ~76, app ~13 |
| `spawn_blocking` | 18 | 13 in matrix (media, collect, devices, people, search, session, upload) |
| tokio process/net/signal, libc, zbus, notify-rust, signal-hook | — | audio, textcmd, catchup, agent, notify, portal, keyring, watch, termquery, clipboard |
| files | — | config, themes, notes, saved, session, stores, downloads, uploads, logs, lock |
| `App` calling the OS itself | ~35 sites | `open_in_browser` (4), `focus::raise_window`, `flush_images` → stdout, `save_setting` (~25 callers), `fs::metadata` (3), upload checks, `Theme::available` (`read_dir`), OS probes in `App::new_in_scheme` |
| ratatui-image | `images.rs`, `ui/image.rs` | pulls in `rustix` (termios) unconditionally: likely does not build for wasm |

## The plan, in steps

Each step is its own series of commits on the branch `web-seams`, keeps the terminal app the
same (unit tests green, `scripts/check.sh` green, the server tests where the worker is
touched), and is reviewed by matrix-agy before the next starts. The native build keeps exactly
today's code paths: the new names re-export what is used now.

1. **Time.** A `crate::time` module re-exporting `web_time::{Instant, SystemTime}` (already in
   the tree through ruma; on native it is `std::time` itself, so nothing changes). All
   `std::time::Instant`/`SystemTime` uses in the core go through it; tokio's `Instant` bridges
   (`app.rs:518`, `matrix/left.rs:326`, `matrix/rooms.rs:432`) keep working because on native
   the type is the same. chrono gets `wasmbind` for the browser only. `ImageStore::new`'s
   clock-and-pid name (`images.rs:710-713`) becomes a counter plus random bytes.
2. **Input events.** A `crate::input` module with our own `KeyCode`, `KeyModifiers`,
   `KeyEvent` (with `kind`), `MouseEvent`, `MouseEventKind`, `MouseButton` and an
   `InputEvent { Key, Mouse, Paste, Resize, Focus }`, **named and shaped like crossterm's
   where we use them**, so most of the change is the `use` lines. `From<crossterm::event::Event>`
   lives at the terminal edge only (`terminal.rs`/the shell); phase 2 adds the ratzilla one.
   `keymap.rs`'s tables, parser, labels and `validate_binding` (Ctrl-I = Tab etc.: terminal
   rules, kept for the terminal) move to the new types. `App::on_event` takes `InputEvent`.
   Tests use the new types (helpers keep them short).
3. **Runtime.** A `crate::rt` module: `spawn`, `JoinHandle`, `JoinSet`, `sleep`, `sleep_until`,
   `timeout`, `timeout_at`, `interval`, `Instant` (tokio's). On native these are re-exports of
   tokio. In the browser: matrix-sdk-common's `executor::spawn` (`spawn_local`), gloo timers,
   and a small `JoinSet` substitute (`FuturesUnordered` + abort handles). Bounds use
   matrix-sdk's `SendOutsideWasm`, so the worker future may be `!Send` in the browser while it
   stays `Send` on native. `spawn_blocking` stays native-only: the 13 worker sites (file
   writes, archive, key export, search pruning) are file work that phase 2 replaces anyway;
   they get a cfg'd path, not a shim.
4. **Effects out of `App`.** What `App` does to the OS itself becomes a request the shell
   carries out, as the existing `take_*` pattern does: opening a URL, raising the window,
   writing image clean-up bytes (`flush_images`), the OS probes at start (truecolor, NO_COLOR,
   players, recorder, transcriber, summary command, ssh) become a `Facts`/`Capabilities` value
   the shell hands to `App::new_in_scheme`. Persistence (config, themes list, notes, saved) goes
   through a small synchronous `Storage` trait (`read`, `write`, `remove`, `list`) held by
   `App`: on native the current code (0700 directories, tmp + rename) moves behind it; in the
   browser it will be `localStorage` (these files are small). File checks for uploads stay in
   `App` but behind the same native boundary.
5. **The shell.** `app::run` moves to `src/shell/terminal.rs` (with `terminal.rs`,
   `termquery.rs`, the editor/suspend handling, signals, the theme/portal watchers, the
   desktop notifications and the audio/note/summary runners), leaving `App` with a
   platform-free surface: `on_input`, `on_matrix`, `on_tick`, the `take_*` effects,
   `ui::draw`, `after_draw`.
6. **Native-only modules behind `cfg`.** `agent/**`, `matrix/agent.rs`, `audio/*` children,
   `textcmd`, `catchup` runner, `notify` (desktop/terminal), `portal`, `keyring` (zbus),
   `watch`, `lock`, `focus`, `clipboard` (picture paste), `editor`, `termquery`, `logging`,
   `doctor`, the CLI in `main.rs`: compiled only for `not(target_family = "wasm")`. Their
   counterparts in the browser are phase 2's.
7. **Library + target-specific dependencies, and the gate.** The modules move from `main.rs`
   into `src/lib.rs` (the binary becomes a thin `main.rs` using it). `Cargo.toml` splits
   dependencies by target: native keeps today's set (matrix-sdk with `bundled-sqlite`,
   `sso-login`, `socks`, `experimental-search`; crossterm; ratatui-image; tokio with
   rt-multi-thread/process/net/signal/io-std; notify; notify-rust; zbus; libc; signal-hook;
   tracing-appender); the browser gets matrix-sdk with `js` + `indexeddb`, matrix-sdk-ui with
   `js`, tokio with `sync`/`macros` only, chrono `wasmbind`, gloo-timers. Images in the
   browser: a placeholder ("[picture]", click opens it) until phase 4. The gate:
   `cargo check --lib --target wasm32-unknown-unknown` passes, in `scripts/check.sh` and in CI.

## Not in phase 1

- The worker protocol's file paths (`OutgoingFile{path}`, `SaveFile{dir}`, `FetchAudio{dir}`,
  `AudioFetched(TempAudio)`, `SetOwnAvatar{path}`, key export/import, archive export): they
  compile in the browser (std's path types exist) and are given byte/Blob variants in phase 2
  together with the browser's file picker and downloads.
- Login by OAuth redirect, the IndexedDB stores and session restore in the browser (phase 2,
  from the phase 0 test build).
- The search index (`experimental-search`, tantivy): native only; the browser goes without
  local search at first.
- A crate split into a workspace: not needed for the gate; can come later if `cfg` gets
  crowded.

## Open questions

1. **Merging into `main`.** Phase 1 does not change behaviour, but it touches 100+ files. If
   each reviewed step is merged into `main` soon, the 1.1.0 work (the dot in
   `ui/timeline.rs`, notify 9 in `watch.rs`, the mobile layout) builds on it and 1.1.0 ships
   with it; if `web-seams` waits until 1.1.0 is out, it has to be brought up to date with
   1.1.0 (conflicts in the same files). For the user to decide.
2. **ratatui-image in the browser:** confirm with `cargo check` that `rustix` breaks it; if so,
   images stay native-only (step 7) unless a feature of ratatui-image avoids it.
3. **What the phase 0 test build finds** about matrix-sdk 0.19.1 in the browser (features,
   `!Send` futures, IndexedDB) may change steps 3 and 7.
