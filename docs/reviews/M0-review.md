# Milestone M0 & Plan Revision 2 Review — `matrix-rat`

- **Reviewer:** `matrix-agy`
- **Date:** 2026-11-12
- **Targets:**
  - Plan revision 2: [`docs/PLAN.md`](../PLAN.md) (§6, §6a, §8, §14)
  - Milestone M0 commit `0b008bb`: [`Cargo.toml`](../../Cargo.toml), [`shell.nix`](../../shell.nix), [`scripts/check.sh`](../../scripts/check.sh), [`src/main.rs`](../../src/main.rs), [`src/app.rs`](../../src/app.rs), [`src/terminal.rs`](../../src/terminal.rs), [`src/lock.rs`](../../src/lock.rs), [`src/logging.rs`](../../src/logging.rs), [`src/paths.rs`](../../src/paths.rs)
- **Verification:** `./scripts/check.sh` passes (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, 6/6 unit tests green in 0.29s).
- **Verdict:** **Approve with changes** (M0 is approved; fold the small code and plan adjustments below into the start of M1 / M3).

---

## Executive Summary

1. **Plan Revision 2 (`docs/PLAN.md`):**
   - All 8 findings from [`PLAN-review.md`](PLAN-review.md) are cleanly integrated across §3, §4, §8, §9, §10, and §14.
   - Targeting **Noctalia v5** (`~/.config/noctalia/templates.toml`, `[theme.templates.user.matrix-rat]`, `input_path_modes`) as the primary desktop-shell integration is the right call.
   - The new **§6a (Formatted-message input via `comrak` + slash commands)** is a strong UX design and `comrak` is a good fit because its AST carries source positions (`sourcepos`) for live composer highlighting. However, there are three `comrak` integration traps (`<span class="spoiler">` vs Matrix's `<span data-mx-spoiler>`, `comrak` default features pulling in C `onig_sys`, and single-line `<p>` wrapping) noted below.
   - The rewritten **§6 (Keybinds)** is clean and approachable, but `Alt-←` / `Alt-→` while focused in the composer collides with standard word-wise cursor movement (`Alt-Left` / `Alt-Right`), and `Alt-arrows` / `Alt-1..4` have known multiplexer/terminal tab collisions that warrant a minor adjustment.
2. **Milestone M0 Code:**
   - Clean, minimal, idiomatic Rust 2024 code. Using stdlib `File::try_lock()` in [`src/lock.rs`](../../src/lock.rs) avoids unnecessary third-party crate dependencies, and the two-phase `TerminalGuard::enter()` → queries → `TerminalGuard::activate()` split in [`src/terminal.rs`](../../src/terminal.rs) directly enforces the stdio query ordering requirement.
   - There are a few small gaps in signal handling (`SIGTERM`/`SIGHUP`), the `XDG_RUNTIME_DIR` fallback path in `paths.rs`, and `ACTIVE` flag ordering in `terminal.rs` to address as M1 begins.

---

## Findings (Ordered by Severity)

### 1. [MEDIUM] Keybind conflict in §6: `Alt-←` / `Alt-→` in the composer steals word-wise cursor navigation (plus multiplexer/tab conflicts)
**Where:** [`docs/PLAN.md`](../PLAN.md#L227-L229) (§6 *Keybinds*)

**Problem:**
1. **`Alt-←` / `Alt-→` inside the composer:** In almost all terminal line editors, shells, and `ratatui-textarea` itself, `Alt-←` / `Alt-→` (often sent by terminals for word-left / word-right, alongside `Ctrl-←` / `Ctrl-→`) moves the text cursor by one word. Binding `Alt-←` globally to *"Jump focus to room list"* means a user editing a sentence in the composer who presses `Alt-←` to jump back a word will unexpectedly yank focus out of the composer into the sidebar.
2. **Multiplexer & terminal tab collisions:**
   - **Zellij** binds `Alt-←/→/↑/↓` by default to move between Zellij panes (intercepting them before `matrix-rat` ever sees them unless Zellij is locked).
   - Several terminal emulators (WezTerm, GNOME Terminal, Konsole, some Kitty configs) bind `Alt-1` … `Alt-9` to switch terminal tabs.
   - On laptops where `F1`/`F2` require holding `Fn`, or terminals that intercept `F1` for their own help window, having *only* `F1` and `F2` for Help and Settings can be awkward.

**Suggested Change:**
1. Keep **`Alt-1` … `Alt-4`** and **`Tab` / `Shift-Tab`** as the primary global pane-jump keys, and for directional pane jumping either:
   - use `Ctrl-Alt-←/→/↑/↓` (or `Alt-Shift-←/→`), **or**
   - when the composer has non-empty text and the cursor is not at column 0, let `Ctrl-←` / `Ctrl-→` (and `Alt-b` / `Alt-f`) move by word, or document clearly that `Ctrl-←` / `Ctrl-→` is word navigation while `Alt-←` / `Alt-→` switches panes.
2. When `↑` in the composer (on the first line) moves focus into the timeline and selects the newest message, ensure that typing regular printable characters that are **not** message action keys (or pressing `Esc` / `↓`) either does not silently eat typing or clearly highlights the timeline selection mode so the user does not accidentally trigger `e` (edit), `r` (react), `c` (copy), or `o` (open) while thinking they are still typing in the composer.
3. Add `?` (when focus is in `sidebar` or `timeline`, i.e., outside the composer) as a default alias for `F1` (Help) and `Ctrl-,` as an alias for `F2` (Settings), plus make the `[F1 Help]` and `[F2 Settings]` hints in the status line clickable via `HitMap` in M1.

---

### 2. [MEDIUM] `comrak` choice in §6a: HTML spoiler tag mismatch, default C `onig_sys` dependency, and outer `<p>` wrapping
**Where:** [`docs/PLAN.md`](../PLAN.md#L59) (§3) & [`docs/PLAN.md`](../PLAN.md#L247-L288) (§6a *Writing formatted messages*), [`Cargo.toml`](../../Cargo.toml#L12)

**Problem:**
`comrak` is a good choice for §6a (especially because its AST provides `sourcepos` line/column ranges that make live composer syntax highlighting clean to implement), but three details will bite in M3 if not accounted for:
1. **Spoiler HTML format mismatch:** `comrak`'s `extension.spoiler = true` parses `||spoiler||` into `NodeValue::SpoileredText`, which `comrak::format_html` renders as `<span class="spoiler">…</span>`. Matrix (`org.matrix.custom.html`), Element, and `mautrix-*` bridges require `<span data-mx-spoiler>…</span>` (they ignore `class="spoiler"`).
2. **`comrak` default features pull in `cli` and `syntect-onig` (`onig_sys`):** `comrak`'s default Cargo feature set is `["cli", "syntect-onig", "bon"]`, which compiles the C Oniguruma library (`onig_sys`) and CLI dependencies. On modern GCC toolchains (including NixOS), `onig_sys` adds C build fragility and unnecessary compile time.
3. **Single-paragraph `<p>…</p>` wrapping & redundant `matrix-sdk` `"markdown"` feature:** `comrak` wraps a single line like `hello **world**` in `<p>hello <strong>world</strong></p>\n`. In Matrix HTML messages, single-paragraph messages should have the outer `<p>…</p>` stripped so clients and bridges do not render extra block margins. Also, [`Cargo.toml`](../../Cargo.toml#L12) currently enables the `"markdown"` feature on `matrix-sdk` (which pulls in `pulldown-cmark` inside `ruma`); once `comrak` is used to construct `RoomMessageEventContent::text_html`, `"markdown"` on `matrix-sdk` is redundant unless needed by an internal helper.

**Suggested Change:**
1. When adding `comrak` in M3, specify `comrak = { version = "0.59", default-features = false }`.
2. Walk the `comrak` AST before formatting (or use a custom HTML formatter write pass) to:
   - emit `<span data-mx-spoiler>` instead of `<span class="spoiler">` for `NodeValue::SpoileredText`; and
   - strip the outer `<p>…</p>` wrapper when the root `Document` contains only a single `Paragraph` node.

---

### 3. [MEDIUM] Unhandled `SIGTERM` / `SIGHUP` bypasses `TerminalGuard` and `ProfileLock` drop cleanup
**Where:** [`src/app.rs`](../../src/app.rs#L88-L97), [`src/terminal.rs`](../../src/terminal.rs#L51-L99)

**Problem:**
- In [`src/app.rs`](../../src/app.rs#L95), the `tokio::select!` loop only awaits `tokio::signal::ctrl_c()`.
- Note that once `enable_raw_mode()` is active, pressing `Ctrl-C` in the terminal sends `Event::Key` (handled by `is_quit`), while `tokio::signal::ctrl_c()` only catches external `SIGINT`.
- If the process receives `SIGTERM` (e.g., `kill <pid>`, service/session stop) or `SIGHUP` (closing a terminal window or killing a tmux/kitty pane), the default Unix signal handler terminates the process immediately **without unwinding the stack or running `Drop`**. This leaves `$XDG_RUNTIME_DIR/matrix-rat/<profile>.pid` behind on disk and (if `SIGTERM` came from outside the terminal) leaves the terminal in alternate-screen + mouse-capture + Kitty-keyboard mode.
- Also in [`src/terminal.rs`](../../src/terminal.rs#L53-L68), `ACTIVE.store(true, Ordering::SeqCst)` is set *after* `out.flush()?`. If `out.flush()` or a later `queue!` fails after partial bytes were written, `restore()` will see `ACTIVE == false` and skip cleanup.

**Suggested Change:**
1. In [`src/terminal.rs`](../../src/terminal.rs#L53), set `ACTIVE.store(true, Ordering::SeqCst)` **before** writing the activation escape sequences to `stdout`, so any partial failure during `activate()` is still cleaned up by `Drop::drop` -> `restore()`.
2. In [`src/app.rs`](../../src/app.rs#L80-L97), register `tokio::signal::unix::signal(SignalKind::terminate())` and `SignalKind::hangup()` alongside `ctrl_c()` so `SIGTERM` and `SIGHUP` set `app.should_quit = true` and exit cleanly through `TerminalGuard::drop` and `ProfileLock::drop`.
3. In M1, add `SIGTSTP` (`Ctrl-Z`) suspend/resume support to `TerminalGuard` as planned in [`docs/PLAN.md`](../PLAN.md#L151).

---

### 4. [LOW] `Paths::new` fallback for missing `XDG_RUNTIME_DIR` uses a world-shared `/tmp/matrix-rat-runtime` path
**Where:** [`src/paths.rs`](../../src/paths.rs#L28-L30)

**Problem:**
```rust
let runtime = base
    .runtime_dir()
    .unwrap_or_else(|| std::env::temp_dir().join(format!("{APP}-runtime")));
```
When `XDG_RUNTIME_DIR` is unset (e.g., certain SSH or headless sessions), `/tmp/matrix-rat-runtime` has a fixed name shared across all OS users on the machine. The first user to create it owns `/tmp/matrix-rat-runtime/matrix-rat`, causing `create_dirs()` to fail with `PermissionDenied` for any other user on the system.

**Suggested Change:**
Fall back to `state.join(APP).join("run")` (i.e., `~/.local/state/matrix-rat/run`), which is already user-private and requires no UID lookup:
```rust
let runtime_dir = base
    .runtime_dir()
    .map(|dir| dir.join(APP))
    .unwrap_or_else(|| state.join(APP).join("run"));
```

---

### 5. [LOW] Case sensitivity in `is_quit` (`Caps Lock`) and `tokio::time::interval` missed-tick behavior
**Where:** [`src/app.rs`](../../src/app.rs#L70-L81)

**Problem:**
1. `is_quit` checks `matches!(key.code, KeyCode::Char('q') | KeyCode::Char('c'))`. When `Caps Lock` is active (especially under the Kitty keyboard enhancement protocol), pressing `Ctrl-Q` can report `KeyCode::Char('Q')`, ignoring the quit chord.
2. `tokio::time::interval` defaults to `MissedTickBehavior::Burst`, which fires catch-up ticks back-to-back after any delay.

**Suggested Change:**
1. Match case-insensitively (`matches!(key.code, KeyCode::Char('q' | 'Q' | 'c' | 'C'))`) until M1's full `crokey` keymap replaces `is_quit`.
2. Set `tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);` on the interval timer.

---

## Verdict

**Approve with changes.** Milestone **M0 (Scaffold)** is complete and verified. Proceed to **M1 (UI shell with fake data)** and fold Findings 1, 3, 4, and 5 into M1 (and Finding 2 into `PLAN.md` / M3).
