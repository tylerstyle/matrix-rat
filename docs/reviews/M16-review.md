# Review: M16 — `focus-click`, `up-selects`, and `notify-timeout`

**Branches / commits reviewed:**
1. `focus-click` at `ff40be5` (`A click on a notification brings the terminal window to the front` — `5` files, `+250` / `-2`)
2. `up-selects` at `cba1bc9` (`Up from the composer walks into the messages again` — `7` files, `+49` / `-45`)
3. `notify-timeout` at `12d348f` (`Notifications: calls to the notification service time out` — `2` files, `+13` / `-4`)

**Files inspected:** `README.md`, `src/focus.rs`, `src/app.rs`, `src/config.rs`, `src/main.rs`, `src/keymap.rs`, `src/palette.rs`, `src/ui/keybinds.rs`, `src/ui/status.rs`, `src/notify.rs`, `src/app/drafts.rs`, `src/app/thread.rs`.

---

## Verdict

**Changes requested across the bundle (`2` `[MEDIUM]` and `2` `[LOW]` findings):**
- **`focus-click` (`ff40be5`):** Clean design and safe bounds; **one `[LOW]` test-isolation fix** needed (`App::with_worker()` leaves `notification_click_command` enabled, so `cargo test` can invoke `niri` and focus a real desktop window during unit tests).
- **`up-selects` (`cba1bc9`):** **One `[MEDIUM]` gap** — `"Edit your last message"` is **missing from the Command Palette (`Ctrl-P`) by default**, contradicting the commit message and `README.md`. (`vim` preset, keybinding editor, status bar hints, and multi-line composer `Up` are otherwise coherent.)
- **`notify-timeout` (`12d348f`):** Fixes the `notify::tests` hang race cleanly on `hpfury`, but **commits an accidental `0`-byte `src/focus.rs` (`[MEDIUM]` merge conflict with `focus-click`)**, and **one `[LOW]`** note that falling back to `notify_rust::Notification::show()` still leaves the worker open to an un-timed `zbus` call if the daemon stalls.

---

## 1. `focus-click` (`ff40be5`)

### Checks
- **Call site & guards ([src/app.rs](../../src/app.rs#L956-L971)):** `App::on_notice_click` calls `crate::focus::raise_window(command)` only when `self.notification_click_command` is `Some(command)`, after checking `self.logged_out`, blocking modals (`Login`, `Recovery`, `Verify`), and room existence in `self.rooms`.
- **Setting & demo guard ([src/config.rs](../../src/config.rs#L36), [README.md](../../README.md)):** `notification_click_command: String` defaults to `""`, is `#[serde(default)]`, is omitted from the in-app Settings UI (file-only), and is documented in `README.md` with Hyprland and sway examples plus niri auto-detection. `App::new` sets `Some(config.notification_click_command.clone())`; `App::demo` sets `None`.
- **Execution bounds, SSH & Niri matching ([src/focus.rs](../../src/focus.rs#L19-L160)):**
  - `raise_window` trims `command`; if empty and `crate::portal::in_ssh_session()` (`SSH_CONNECTION`, `SSH_CLIENT`, or `SSH_TTY`) is true, returns immediately without touching any desktop socket.
  - Spawns a background `"raise-window"` thread so the UI thread never blocks.
  - `run()` detaches `stdin`/`stderr` (`Stdio::null()`), caps `stdout` reads at `4 * 1024 * 1024` bytes on a helper thread, polls `try_wait()` every `20 ms` up to `PATIENCE = 3 s`, and `kill()` + `wait()`s the child on timeout (`a_program_that_never_ends_is_given_up_on` verifies this).
  - `ancestors()` walks `/proc/<pid>/stat` up to `< 64` hops stopping at `pid <= 1`; `parent_of()` parses after the last `')'` so process names containing spaces/parentheses work cleanly (`the_parent_is_read_past_a_name_with_spaces_and_brackets`).
  - `niri_window()` picks the nearest ancestor PID matching a `niri` window, and when a single terminal process owns several windows (`several`), focuses only if uniquely one window has `title.contains("matrix-rat")`; otherwise focuses none (`_ => None`).
- **Logging:** `tracing::debug!` only emits `%err` or static messages — never `command`, window titles, or PIDs.

### Findings
- **`[LOW]` `App::with_worker()` test helper leaves `notification_click_command` enabled ([src/app.rs](../../src/app.rs#L237-L240), [src/app.rs](../../src/app.rs#L6329-L6332)).**
  `App::notification_click_command`'s doc comment explicitly says: *"`None` where no window is to be raised at all: the demo, and tests, which may well run inside a desktop that would obey."* `App::demo()` sets `None`, **but `App::with_worker()` constructs `App::new(&config, ...)` (setting `Some("")`)** without clearing it to `None`. As a result, `a_clicked_notification_opens_its_room_like_the_room_list_does` ([src/app.rs](../../src/app.rs#L6377-L6405)) calls `raise_window("")` — which inside a local non-SSH niri session spawns `niri msg --json windows` and potentially `niri msg action focus-window` against the test runner's terminal window. Fix: set `app.notification_click_command = None;` in `with_worker()` (and any other test helper invoking `NoticeClicked`).
- **`[Note]` Multi-window terminal matching relies on the terminal's existing title ([src/focus.rs](../../src/focus.rs#L119-L129)).** `src/terminal.rs` / `src/main.rs` do not set an OSC terminal title, so a terminal running multiple windows under one PID (e.g. `kitty` single-instance or `foot` client) only matches `"matrix-rat"` if the shell/launcher already named the window that way. Falling back to `None` when unsure is the safe choice.
- **`[Note]` `run()` pipes stdout even when output is ignored ([src/focus.rs](../../src/focus.rs#L45-L78)).** `run_user_command` and `niri msg action focus-window` discard `Option<Vec<u8>>` yet still pipe stdout and call `reader.join()` after waiting/killing. If a user's `notification_click_command` spawns a detached background process inheriting stdout, the `"raise-window"` thread stays blocked on EOF; using `Stdio::null()` when stdout isn't needed avoids that.

---

## 2. `up-selects` (`cba1bc9`)

### Checks
- **`vim` preset coherence ([src/keymap.rs](../../src/keymap.rs#L661-L808), [src/app.rs](../../src/app.rs#L3164-L3172)):**
  - `VIM_EXTRA` adds `j`/`k`/`g`/`G`/`h`/`l`/`i`/`/` in `Timeline`/`Sidebar` and `Esc -> Focus(Pane::Timeline)` in `Composer`.
  - Returning bare `Up` in `Composer` to `Action::SelectOlder` leaves multi-line `Up` movement (`!self.composer.on_first_row(width)`) intact and steps into `Timeline` from row 0 uniformly under both presets (`up_in_an_empty_composer_walks_into_the_messages`). `EditLast` still works when bound or invoked (`edit_last_works_in_the_vim_preset_too`), with `Esc` returning focus to `Timeline` and `Ctrl-C` cancelling the edit under `vim`.
- **Keybinding editor with an unbound action ([src/keymap.rs](../../src/keymap.rs#L199,L1715-L1748), [src/ui/keybinds.rs](../../src/ui/keybinds.rs#L149-L175)):**
  - `Action::EditLast` has `default_context: Context::Composer` in `ACTIONS`. `Editor::rebuild_rows` includes actions matching `in_default || in_preset || in_current`, so `"Edit your last message"` appears under **Composer** with `labels.is_empty() == true`, rendering `"—"` in `Role::Muted` (`modified == false` until rebound). `Action::SelectOlder` (`"Select message"`, `"↑"`) also appears under **Composer** via `in_preset`. Status hints ([src/ui/status.rs](../../src/ui/status.rs#L106-L122)) show `"select message"` for `SelectOlder` and show `"edit last message"` only if `EditLast` is bound.
- **`README.md` accuracy:** Matches the key table, `[keys.composer]` syntax, `Esc`/`Ctrl-C` cancel rules, and fallback behavior, **except** for the claim that `"Edit your last message"` is available in the command palette (`Ctrl-P`) by default (see below).

### Findings
- **`[MEDIUM]` `"Edit your last message"` does not appear in the Command Palette by default ([src/palette.rs](../../src/palette.rs#L392-L457), [README.md](../../README.md#L203)).**
  `cba1bc9` removed `| Action::EditLast` from `include_keymap_action`'s `Skip` arm ([src/palette.rs](../../src/palette.rs#L518)). However, `collect_actions` ONLY collects keymap actions from `ctx.keymap.bindings()` (`active_bindings`, lines 392-429) and `keymap::DEFAULT` (lines 432-457), plus actions pushed explicitly (like `CloseThread` at lines 359-376, whose comment explains why unbound actions must be pushed explicitly). Because `Action::EditLast` has no binding in `keymap::DEFAULT` and no binding in `active_bindings` unless the user configured one, it never reaches `collect_actions`'s output — so users cannot run `"Edit your last message"` from `Ctrl-P` at all under default config.
  - **Fix:** Push `Target::Action(Action::EditLast)` explicitly in `collect_actions` with key hint `ctx.keymap.label_in(keymap::Context::Composer, Action::EditLast).map(str::to_owned)`. Also consider `action_enabled` / behavior when `last_editable()` is `None`: currently `Action::EditLast` falls back to `SelectOlder` ([src/app.rs](../../src/app.rs#L2284-L2287)) while `action_enabled` falls through to `_ => true`, so triggering it from the palette when composing text or with no own message would unexpectedly jump into `Timeline`.
- **`[Note]` Duplicate test comment ([src/app.rs](../../src/app.rs#L4869-L4870)).** `// (EditLast edits our latest message.)` was added above the stale `// (Up in the empty composer edits our latest message.)` line instead of replacing it.

---

## 3. `notify-timeout` (`12d348f`)

### Checks & Verification
- Cherry-picked cleanly (omitting empty `src/focus.rs`) and verified standalone at `12d348f`: `CALL_PATIENCE = 5 s` on `zbus::connection::Builder::method_timeout(CALL_PATIENCE)` bounds `GetCapabilities` and `Notify` calls, and moving `zbus::MessageStream::from(&service)` ahead of `tokio::spawn` in `Service::serve` ([src/notify.rs](../../src/notify.rs#L1295-L1299)) removes the early-call race that could hang `notify::tests` on fast machines.

### Findings
- **`[MEDIUM]` Accidental empty `0`-byte `src/focus.rs` committed in `12d348f`.**
  `git show 12d348f` creates an empty `src/focus.rs`, which causes an `add/add` conflict when merging together with `focus-click` (`ff40be5`). Drop the empty file from `notify-timeout` (or ignore it on merge).
- **`[LOW]` `notify_rust::Notification::show()` fallback still has no method timeout ([src/notify.rs](../../src/notify.rs#L430-L446)).**
  If `Clickable::show` times out after `5 s` waiting on a hung `org.freedesktop.Notifications` daemon, it marks `Link::Down` and returns `false`, and `show_desktop` immediately falls back to `notification.show()` (`notify_rust`). In `notify-rust 4.19.0`, `Notification::show()` opens a new `zbus::Connection::session().await` without any `method_timeout` to the exact same D-Bus service — which can still hang the `"notify"` worker thread whenever the daemon is stuck.
- **`[Note]` No test covers the `5 s` timeout / `CALL_PATIENCE` path itself.** Existing `notify::tests` now pass reliably with the `Service::serve` ordering fix, though a dedicated test for an unresponsive service wasn't added.

---

## Checks run (`hpfury` via `.claude/fury-check.sh`)

Ran `scripts/check.sh` (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build`) in a clean temporary worktree on `hpfury` for each branch (`--server` skipped — no homeserver protocol logic changed):

| Branch | Commit | `cargo test` | Exit |
|---|---|---|---|
| `focus-click` | `ff40be5` | `551 passed; 0 failed; 26 ignored` (`3.50s`) | `0` |
| `up-selects` | `cba1bc9` | `547 passed; 0 failed; 26 ignored` (`3.46s`) | `0` |
| `notify-timeout` | `12d348f` | `546 passed; 0 failed; 26 ignored` (`3.52s`) | `0` |
