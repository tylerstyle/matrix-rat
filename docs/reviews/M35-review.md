# M35 Review — In-App Voice & Audio Playback (`audio`)

- **Branch:** `audio` (`6a58f56`, merge `85fbcd4`, `462052a`; `git diff main...audio`)
- **Worktree:** `.claude/worktrees/audio`
- **Scope:** Static code review against `docs/PLAN.md` §6a piece B across all 50 changed files (`+4536 / -154`, no audio libraries in `Cargo.toml:1-71`; no builds or tests run; unit suite reported green at 1033 tests and server suite at 44/44).
- **Verdict:** **Changes requested** — core process, temp-file, IPC, privacy, keymap, and playback architecture is very solid, with **two Medium bugs** (`Ctrl-Z` in the image viewer not stopping audio, and stale cached `row.hits` / underlines behind live audio rows) and **one Low bounds bug** (unbounded sender duration still formatted into idle `message.body`), plus non-blocking notes below.

---

## Findings

### 1. [MEDIUM] `Suspend` (`Ctrl-Z`) inside the image viewer leaves the audio player running
- **Where:** `src/app/media.rs:98-106` vs `src/app.rs:2702-2706`, `src/app.rs:4961-4974`, `src/audio/play.rs:175`
- **Detail:** Normal `Action::Suspend` dispatch (`src/app.rs:2702-2706`) calls `self.stop_audio()` before setting `self.suspend_requested = true`, cancelling any loading download or signalling `Control::Stop` so the `run()` loop (`src/app.rs:4965-4966`) finishes waiting on `players` and stops cleanly before `SIGTSTP`.
  - However, when the image viewer is open, `on_viewer_key` (`src/app/media.rs:98-106`) handles `Action::Suspend` directly and sets `self.suspend_requested = true` **without** calling `self.stop_audio()`.
  - Because audio plays across popups and the player runs in its own `process_group(0)` (`src/audio/play.rs:175`), terminal `SIGTSTP` never stops the child process. Pressing `Ctrl-Z` in the image viewer stalls `run()` on the 1-second timeout at `src/app.rs:4966` and then suspends `matrix-rat` while the audio player keeps playing in the background.
- **Fix:** Call `self.stop_audio();` in `on_viewer_key`'s `Action::Suspend` arm (`src/app/media.rs:102`) and add a regression test suspending from the viewer during active playback.

### 2. [MEDIUM] Stale cached `row.hits` (and `UNDERLINED` cells) stay active behind `sounding` rows
- **Where:** `src/ui/timeline.rs:648-686`, `src/ui/timeline.rs:1500-1528`
- **Detail:** While a voice or audio message loads or plays (`sounding` is `Some(line)`), `draw` renders `&row.line` first (preserving any right-aligned read marker in columns `left..rect.width`) and then draws `over` padded with spaces into `Rect::new(rect.x, rect.y, left, 1)` (`src/ui/timeline.rs:1501-1507`). Afterward, lines `1511-1516` register **all** `&row.hits` unconditionally before `1518-1528` adds the new `HitId::PlayMedia(row.source)` for `part.head && part.first`:
  - Whenever the live `sounding` head line (e.g., `■ 0:05 / 0:12` at narrow width or `■ ⠋ loading`) is shorter than the idle cached line (`▶ 🎤 very_long_name.ogg · 0:12 · 2.0 kB`), columns `line.width()..idle_end` keep the old cached `HitId::PlayMedia` hit (and if a filename contains an `http://` / `https://` URL, `rich::Rich::of` produces a `HitId::Link(i, 0)` hit in `row.hits` that remains clickable in that trailing space and on wrapped continuation rows `part.head && !part.first` blanked with `Line::default()`).
  - Additionally, `Span::raw(" ".repeat(pad))` in `over` uses default `Style`, which does not clear `Modifier::UNDERLINED` written into the buffer by `&row.line` for link segments.
- **Fix:** Skip registering `&row.hits` when `sounding.is_some()` (while keeping `0..left` cleared without `UNDERLINED`, or only rendering the cached `left..rect.width` marker slice), so clicks only register on the live `■ …` head row and never hit phantom links from the hidden filename.

### 3. [LOW] Unbounded sender `duration` is still formatted into `message.body`
- **Where:** `src/matrix/convert.rs:335-356`, `517-518`
- **Detail:** `event_message` computes `let meta = audio_meta(c);`, which applies `crate::audio::bounded_duration` (`<= 24 h`, `src/audio.rs:30, 289-291`) to set `meta.duration`, yet passes the raw unbounded `duration` extracted at lines `335-339` into `attach_media(...)` (`lines 343-353`). A sender claiming e.g. `10_000_000` seconds gets `meta.duration == None` during live playback yet formats and displays `2777:46:40` in the idle message text (not caught by `src/matrix/convert.rs:986-996`, which only tested `audio_meta()` directly).
- **Fix:** Pass `meta.duration` instead of raw `duration` into `attach_media` for `MessageType::Audio` (note `Video` at line `298` already passes `info.duration` directly) and assert `event_message`/`attach_media` behavior in tests.

### 4. [LOW] `Running::watch` calls `kill(-self.group, SIGKILL)` *after* `child.wait()` reaps the leader
- **Where:** `src/audio/play.rs:206-213, 235-245, 247-252, 304-309`
- **Detail:** On `Control::Stop` (`248-251`) and `Drop for Running` (`304-309`), `self.kill()` checks `!self.reaped` and signals `-self.group` **before** `self.child.wait()`, so the zombie leader strictly prevents PID/PGID reuse.
  - However, in the natural-exit `tokio::select!` branch (`status = self.child.wait()` at `src/audio/play.rs:235-240`), `child.wait()` has **already reaped** the leader (`self.reaped = true`) before line `240` runs `libc::kill(-self.group, libc::SIGKILL)`. If the player left grandchildren behind in its group, Linux keeps the PGID occupied (`PIDTYPE_PGID`); if no grandchildren exist (the normal case), `self.group` is already freed back to the PID allocator between lines `235` and `240`, so in theory a fast PID wraparound reusing `self.group` as a new PGID could be signalled. Practically negligible under typical `pid_max`, though worth noting since it breaks the invariant documented at `src/audio/play.rs:157, 209`. (Avoiding it would require inspecting exit with `waitid(P_PID, ..., WEXITED | WNOWAIT)` before reaping, if desired.)

---

## Hard-Look Checklist

### 1. Process safety (`src/audio/play.rs`, `src/app.rs`)
- **`pre_exec` async-signal-safety (`src/audio/play.rs:176-191`):** Captures `parent: libc::pid_t` (`Copy`) before `fork()`, calls only `libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL)`, `libc::getppid()`, `libc::_exit(1)`, and `io::Error::last_os_error()` (pure errno wrapper, zero allocation). `.process_group(0)` and `Stdio::null()` run safely in std's child setup. Note: `PR_SET_PDEATHSIG` watches the spawning *thread*, and `players.spawn(...)` (`src/app.rs:5023`) runs on Tokio's persistent multi-threaded core worker pool (never `spawn_blocking`), which outlives playback until runtime shutdown.
- **No outliving / no zombies:** Player tasks run inside `players: JoinSet` (`src/app.rs:4958, 5020-5025`), with finished tasks drained every loop iteration (`try_join_next`). `Control::Stop` (`play.rs:247-251`) kills the group and `await`s `self.child.wait()`; `Ctrl-Z` (`app.rs:4965-4966`, subject to Finding 1 in viewer) and normal/`SIGTERM`/`SIGHUP`/`Ctrl-C` exit (`app.rs:5088-5092`) stop and wait up to `1s`/`2s` before `drop(players)` aborts any stragglers. Task abort/drop invokes `Running::drop` (`kill(-self.group, SIGKILL)`) + `tokio::process::Command::kill_on_drop(true)` (`193`) + kernel `PR_SET_PDEATHSIG` on parent death. (See Finding 4 regarding line `240` after `child.wait()`.)

### 2. Temp file & socket (`src/audio/temp.rs`, `src/paths.rs`, `src/matrix/media.rs`)
- **Private & unpredictable:** `TempAudio::create` (`src/audio/temp.rs:34-58`) runs `private_dir(dir)` (`94-116`, checking `symlink_metadata` for real directory owned by `geteuid()`, enforcing `0o700`, refusing symlinks), then creates `play-<16 random hex digits>.<ext>` via `OpenOptions::new().write(true).create_new(true).mode(0o600)` (`O_CREAT | O_EXCL`), generating randomness from `/dev/urandom` (fallback to `RandomState` hash). Per-profile runtime folder (`paths.audio_dir()`, `src/paths.rs:107-109`) keeps concurrent profiles isolated.
- **No sender-controlled filename or extension:** `extension(mime)` (`src/audio/temp.rs:73-90`) maps MIME essence strictly through a 10-entry fixed allowlist (`ogg`, `opus`, `mp3`, `m4a`, `aac`, `flac`, `wav`, `webm`, `3gp`, `amr`) or `"audio"`; `file.name` never reaches the filesystem or player argv.
- **Control socket (`src/audio/temp.rs:137-140`, `src/audio/play.rs:266-271`):** Named `ipc-<16hex>.sock` inside `audio_dir`, enforced `is_absolute()` and `<= 100` bytes (`MAX_SOCKET_PATH`, omitting `--input-ipc-server` cleanly when the runtime dir path is too long), tightened to `0o600` on connect, and removed (`play.rs:144-146`) before `TempAudio::drop` removes the audio file and before `PlayerEvent::Ended` is sent (`148-149`). Startup and shutdown both call `audio::remove_leftovers(&paths.audio_dir())` (`src/app/playback.rs:214`, `src/app.rs:5092`).

### 3. Hostile input & player command (`src/audio.rs`, `src/audio/ipc.rs`, `src/matrix/media.rs`, `src/matrix/convert.rs`)
- **Waveform / size bounds:** `bounded_waveform` (`src/audio.rs:306-315`) downsamples via peak bucketing to `<= 256` (`MAX_WAVEFORM`) and clamps amplitudes to `<= 1024` (`MAX_AMPLITUDE`); `bars` (`319-328`) draws at most `24` (`WAVE_BARS`) bars without slice-indexing panics. `Media::fetch_audio` / `get` enforces `audio::MAX_FILE` (`100_000_000` bytes) across stated `file.size`, media cache size, HTTP `Content-Length`, and streamed chunks (`src/matrix/media.rs:260, 394-403, 496`). (Note: see Finding 3 on idle `message.body` duration display, and `Media::get` decrypts `100 MB` synchronously on the async thread with ~3x buffer peak in memory while encrypted/plaintext/store copies coexist at `520-539`.)
- **IPC line reader & JSON parser (`src/audio/ipc.rs:34-108`):** `Lines::push` enforces `MAX_LINE = 4096` and immediately drops overlong buffers (`self.held = Vec::new(); self.skipping = true`) until `\n`, bounding memory strictly. `ipc::parse` checks `line.len() <= 4096`, `serde_json::from_slice` enforces recursion limits, accepts only `"event": "property-change"` with `time-pos`, `duration`, `pause`, `eof-reached`, and `seconds()` rejects `NaN`/`Inf`, `< -1.0` (clamping `[-1..0)` to `0`), and `> MAX_DURATION` (`24h`), preventing `Duration::from_secs_f64` panics.
- **Custom `audio_player` command (`src/audio.rs:134-268`):** `choose()` searches only absolute `PATH` entries for `auto`, splits custom commands via `shlex::split` (**no shell is ever invoked**; shell metacharacters stay literal argv words), verifies regular executable file (`is_file() && mode & 0o111 != 0`), and `Player::argv` passes only `self.args`/`quiet_args()`, our optional `--input-ipc-server=<socket>`, and our own `TempAudio::path()` (no sender-controlled filename, body, or caption ever reaches `argv`).

### 4. Privacy in `Debug`, logs & doctor (`src/model.rs`, `src/audio*.rs`, `src/app/playback.rs`, `src/matrix/mod.rs`, `src/config.rs`, `src/doctor.rs`)
- Audited manual `Debug` impls across `Attachment`, `AudioMeta`, `MediaRef`/`ImageSource::Matrix`, `AudioState`, `Sound`, `Player`, `TempAudio`, `audio::Request`, `MatrixCmd::{FetchAudio, CancelAudio}`, `MatrixEvent::AudioFetched`, `Config` (`audio_player_auto: bool`), `doctor::Audio`, and `PaletteContext`: no room IDs, event IDs, keys, filenames, MIME strings, paths, or custom `audio_player` commands leak into `Debug` or logs.
- `doctor` (`462052a`, `src/doctor.rs:119-145, 473-505`) shows only `"not offered"` (SSH), `"mpv"`/`kind.program()`, `"a command of your own"`, or missing — never custom command paths or arguments.

### 5. Timeline keys (`src/keymap.rs`, `src/app/playback.rs`, `src/menu.rs`, `src/palette.rs`)
- `ACTIONS` (`src/keymap.rs:213-216`) and `DEFAULT` (`713-744`) bind `x` (`PlayAudio`), `Space` (`PauseAudio`), `Left` (`SeekAudio(Dir::Left)`), and `Right` (`SeekAudio(Dir::Right)`) strictly in `Context::Timeline`.
- Checked against `DEFAULT` and `VIM_EXTRA` (`Global`, `Sidebar`, `Timeline`, `Composer`), `Help`, `Palette`, `Menu`, `on_modal_key`, and `on_viewer_key`: zero collisions (`x`, `Space`, `←`, `→` had no prior binding in `Timeline`; `src/app/keybinds.rs:419` test updated `"x"` -> `"X"` for `save`). Composer typing/arrows, sidebar filter input/arrows, and popup/viewer keys still take precedence cleanly.
- `control_audio` (`src/app/playback.rs:368-384`) returns silently if `!self.audio_caps().selected`, so `Space`/`←`/`→` do nothing on text messages or unselected audio rows (only showing `"Pause and seek need mpv as the audio player"` when pressing them on the selected playing non-mpv message). All audio actions have `needs_open_chat() == true` (`src/action.rs:233-235`).

### 6. Cached row draw-over (`src/ui/audio.rs`, `src/ui/timeline.rs`, `src/ui/timeline/receipts.rs`)
- `RowKey` stays independent of playback progress (`src/ui/timeline.rs:1339-1359`); `AudioPart { head, first, marker }` tags only cached `row.line == 0` and matching waveform `row.line == 1` (`630-642`), and `draw` overlays `over` without mutating `cache.rows`. `ui::audio` styles set only `.fg` (plus `BOLD`) so `MessageSelected` / `Hover` backgrounds/modifiers show through, while `receipts::add` (`59-83`) and `draw` (`1489-1508`) preserve right-aligned read markers and `" ✓"` cut-mode ticks cleanly (except the `row.hits`/`UNDERLINED` overlap in Finding 2).

---

## Minor / Non-Blocking Notes

1. **Thread root mismatch (`src/app/thread.rs:506, 510`, `src/app/playback.rs:258-266`):** `thread_snapshot` sets the thread root message's `id` to `"thread-root"`; starting playback on a thread root in the room then opening the thread (or vice versa while still displaying the temporary `"thread-root"` copy) hides its `■` progress overlay and causes `x` to re-start instead of toggling stop.
2. **Playback across external `$EDITOR` (`src/app/external_editor.rs:21-36`, `src/app.rs:4975-5002`):** `ComposeInEditor` leaves audio running (harmless to the terminal thanks to `process_group(0)` and `Stdio::null()`), though queued `AudioFetched` events start playback only upon returning from `$EDITOR`.
3. **Mid-flight settings/disappearance edges (`src/app/playback.rs:379-413, 475-484`):**
   - If the open room vanishes in `apply_rooms` while loading, `on_audio_fetched` does not verify `self.room_id()`, briefly queuing `audio.start` until the next 250ms `audio_tick()`.
   - If `audio_player` is edited while loading/playing: invalid syntax/missing player clears `current` silently at `playback.rs:405-408`, and `control_audio` (`380`) checks the updated `self.audio.player` setting rather than the active player when deciding whether to toast `"Pause and seek need mpv as the audio player"`.
   - When `audio_dir` exceeds `MAX_SOCKET_PATH = 100`, `mpv` runs without IPC and `Space`/`←`/`→` do nothing without a toast.
4. **Live clock ticks past `meta.duration` for non-mpv players (`src/app/playback.rs:178-208`):** `shown_now()` calls `sound.view(None)` (no `meta`), so non-mpv players keep marking `self.dirty = true` every second past `meta.duration` even though `audio_in` clamps the rendered bar/clock. Also, non-mpv progress bar/waveform updates happen once per second rather than every 250ms.
5. **Small UI & wording polish:**
   - `ui::audio::{head, wave}` (`src/ui/audio.rs:41-46, 121-130`) prepend 2-column `indent` even at `width < 2` (`draw` clips to `left`, keeping borders safe); at `room_left = 20..=21` with waveless paused audio (`ui/audio.rs:61-86`), `" paused"` is omitted while a 4–5 cell bar still draws.
   - `NoPlayer::BadCommand` displays `"{command} (not found)"` in Settings (`src/app/playback.rs:149`) and maps to `OwnMissing` (`"not found"`) in Doctor (`src/doctor.rs:142`).
   - `audio_caps().can_play` (`src/app/playback.rs:244`) omits `file.size <= MAX_FILE`, so context menus on >100 MB audio show `"Play audio"` alongside `"Play in system player"` (pressing `x` gives the size refusal toast).
   - `Matrix::get` (`src/matrix/media.rs:520-539`) decrypts up to `MAX_FILE` (`100 MB`) inline on the async worker thread while temporarily holding encrypted, decrypted, and SDK-cached buffers simultaneously.
   - `the_debug_output_of_the_app_holds_nothing_private` (`src/app.rs:9554-9735`) leaves `AudioState` idle and `commands_and_events_print_no_ids_and_no_error_text` (`src/matrix/mod.rs:2394`) doesn't cover `FetchAudio`/`CancelAudio`/`AudioFetched` (partially covered by `src/app/playback.rs:1258-1273`).

---

## Follow-up (`062f519`)

- Checked commit `062f519` (`1034` unit tests reported green):
  - **Finding 1 fixed:** `on_viewer_key` (`src/app/media.rs:102-107`) now calls `self.stop_audio()` before `self.suspend_requested = true`, covered by `suspending_from_the_picture_viewer_stops_it_too` (`src/app/playback.rs:871-886`).
  - **Finding 2 fixed:** `ui::timeline::draw` (`src/ui/timeline.rs:1503-1525`) resets symbols to `" "` and strips `Modifier::UNDERLINED` across `rect.x..rect.x + left` before painting `over` (preserving row selection/hover colours and right-aligned read markers), and skips `&row.hits` with `*start < left` so hidden filename/play/link hits are unreachable while playing.
  - **Finding 3 fixed:** `event_message` (`src/matrix/convert.rs:335-337`) now passes `meta.duration` (bounded to `<= 24 h` by `audio_meta`) into `attach_media`.
  - **Finding 4:** Left as-is by decision (noted as negligible in normal runs; `Stop` and `Drop` signal `-self.group` prior to reaping).

**M35 ok**
