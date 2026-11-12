# Milestone M7 (Part 1: Theme Hot Reload & Generator Templates) Review (`49595e7`)

**Reviewer:** Antigravity (`matrix-agy`)  
**Commit under review:** `49595e7` (`Theme hot reload and templates for theme generators`)  
**Scope:** [src/watch.rs](file:///home/tylerstyle/git/matrix-rat/src/watch.rs), [src/main.rs](file:///home/tylerstyle/git/matrix-rat/src/main.rs), [src/theme.rs](file:///home/tylerstyle/git/matrix-rat/src/theme.rs), [src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs), [src/lock.rs](file:///home/tylerstyle/git/matrix-rat/src/lock.rs), [contrib/themes/README.md](file:///home/tylerstyle/git/matrix-rat/contrib/themes/README.md), [contrib/themes/noctalia.toml](file:///home/tylerstyle/git/matrix-rat/contrib/themes/noctalia.toml), [contrib/themes/wallust.toml](file:///home/tylerstyle/git/matrix-rat/contrib/themes/wallust.toml), [contrib/themes/pywal.toml](file:///home/tylerstyle/git/matrix-rat/contrib/themes/pywal.toml), [README.md](file:///home/tylerstyle/git/matrix-rat/README.md)

---

## Verification Summary

- `./scripts/check.sh` passes cleanly (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, 102 unit & snapshot tests pass, 2 ignored tests).
- Previous M3b review fixes in `0b69b5c` were also inspected and verified clean.

---

## Findings (Ordered by Severity)

### 1. [HIGH] `ThemeWatch::changed()` is not cancel-safe inside `tokio::select!`, dropping theme reloads and `SIGUSR1` signals
**Location:** [src/watch.rs](file:///home/tylerstyle/git/matrix-rat/src/watch.rs#L69-L85), [src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L1728-L1745)

**Problem:**
`ThemeWatch::changed(&mut self)` is polled directly as a branch in the main `tokio::select!` loop in `app::run`:
```rust
pub async fn changed(&mut self) {
    if self.changes.recv().await.is_none() {
        std::future::pending::<()>().await;
    }
    loop {
        tokio::time::sleep(SETTLE).await;
        ...
```
When a notification arrives on `self.changes` (for instance, a single `()` from `SIGUSR1` via `matrix-rat --reload-theme`, or an atomic file rename), `self.changes.recv().await` **consumes** the item and then awaits `tokio::time::sleep(SETTLE)` (`120ms`).

However, in `app::run` ([src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L1728-L1745)), `tokio::select!` simultaneously polls:
- `tick.tick()` (every `250ms`),
- `events.next()` (every mouse move or key press), and
- `matrix_events.recv()` (every sync event).

Whenever **any** of those other branches completes during the `120ms` `SETTLE` sleep (~48% probability from the `250ms` tick timer alone, and near 100% while the user moves the mouse or types), `tokio::select!` cancels (drops) the in-flight `theme_watch.changed()` future. On the next iteration of `while !app.should_quit`, `theme_watch.changed()` starts from the top at `self.changes.recv().await`, and the already-consumed reload notification is permanently lost.

**Suggested Change:**
Store the debounce deadline on `ThemeWatch` (e.g. `deadline: Option<tokio::time::Instant>`) so the pending settle state survives across `tokio::select!` cancellations, and cap the debounce window so an event storm cannot postpone reload forever:
```rust
pub struct ThemeWatch {
    _watcher: Option<RecommendedWatcher>,
    changes: UnboundedReceiver<()>,
    deadline: Option<tokio::time::Instant>,
}

impl ThemeWatch {
    pub async fn changed(&mut self) {
        if self.deadline.is_none() {
            if self.changes.recv().await.is_none() {
                std::future::pending::<()>().await;
            }
            while self.changes.try_recv().is_ok() {}
            self.deadline = Some(tokio::time::Instant::now() + SETTLE);
        }
        let deadline = self.deadline.expect("deadline set above");
        tokio::time::sleep_until(deadline).await;
        self.deadline = None;
        while self.changes.try_recv().is_ok() {}
    }
}
```
*(Bonus: you can also store `usr1: Signal` directly on `ThemeWatch` or keep the forwarding task; with `deadline` stored on `self`, `changed()` is 100% cancel-safe.)*

---

### 2. [HIGH] `signal_running_instances` can terminate an unrelated process on PID reuse or delete a live instance's PID file
**Location:** [src/watch.rs](file:///home/tylerstyle/git/matrix-rat/src/watch.rs#L96-L125), [src/lock.rs](file:///home/tylerstyle/git/matrix-rat/src/lock.rs#L18-L55)

**Problem:**
1. **PID reuse kills unrelated processes:** On Linux/Unix, the default disposition of `SIGUSR1` is **process termination (`Term`)**. If `matrix-rat` crashes (`SIGKILL`, OOM, power loss) and leaves behind `$XDG_RUNTIME_DIR/matrix-rat/default.pid`, and the OS later recycles that PID for another process owned by the same user (shell, editor, browser, compositor), running `matrix-rat --reload-theme` executes `kill -USR1 <pid>`, **terminating that unrelated process**. Furthermore, because `kill` exits with `0`, `default.pid` is not removed and every subsequent theme hook will signal that PID again.
2. **Deleting a live PID file on non-`ESRCH` failure:** In [src/watch.rs](file:///home/tylerstyle/git/matrix-rat/src/watch.rs#L119-L122), if `Command::new("kill")` fails for any reason other than a dead process (e.g., minimal `PATH` in a service/hook environment), it unconditionally deletes `&path`.

**Suggested Change:**
Use the same OS-managed `flock` (`File::try_lock()`) on the `.pid` file itself in [src/lock.rs](file:///home/tylerstyle/git/matrix-rat/src/lock.rs):
1. In `ProfileLock::acquire` ([src/lock.rs](file:///home/tylerstyle/git/matrix-rat/src/lock.rs#L39-L48)): open `pid_file` with `OpenOptions::new().create(true).truncate(false).write(true)`, call `pid.try_lock()` (which the kernel automatically releases even on `SIGKILL`), truncate & write `std::process::id()`, and keep `_pid: File` inside `ProfileLock`.
2. In `signal_running_instances` ([src/watch.rs](file:///home/tylerstyle/git/matrix-rat/src/watch.rs#L101-L123)):
   - Open `path` for writing and call `file.try_lock()`.
   - If `file.try_lock()` succeeds (`Ok(())`), no `matrix-rat` instance holds the lock—the `.pid` file is guaranteed stale. Remove `&path` and `continue` **without** signalling `pid`.
   - Only when `file.try_lock()` returns `Err(TryLockError::WouldBlock)` is a live `matrix-rat` instance holding that file open. On Linux, you can additionally check `std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim() == "matrix-rat")` when `/proc` exists, then send `kill -USR1`, and **never** delete `&path` in that branch.

---

### 3. [MEDIUM] `contrib/themes/noctalia.toml` hard-codes `name = "noctalia"`, breaking `F3` (`CycleTheme`) for `matugen` and custom filename stems
**Location:** [contrib/themes/noctalia.toml](file:///home/tylerstyle/git/matrix-rat/contrib/themes/noctalia.toml#L3), [src/theme.rs](file:///home/tylerstyle/git/matrix-rat/src/theme.rs#L31-L36), [src/theme.rs](file:///home/tylerstyle/git/matrix-rat/src/theme.rs#L355-L357), [src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L821-L829)

**Problem:**
`theme::TEMPLATES` shares `contrib/themes/noctalia.toml` for both `"noctalia"` and `"matugen"`. Because `contrib/themes/noctalia.toml` has `name = "noctalia"` on line 3:
1. When a `matugen` user follows [contrib/themes/README.md](file:///home/tylerstyle/git/matrix-rat/contrib/themes/README.md#L50-L65) and writes `~/.config/matrix-rat/themes/matugen.toml` with `theme = "matugen"`, `Theme::available()` lists the filename stem `"matugen"`, while `Theme::load("matugen")` parses the file and sets `app.theme.name = "noctalia"`.
2. When the user presses `F3` (`Action::CycleTheme` at [src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L821-L829)), it searches `self.theme_names` for `self.theme.name` (`"noctalia"`), fails to find it because the file is named `matugen.toml`, and resets to index `0` (`"rat-dark"`) every time.
3. Also, `Action::CycleTheme` does not refresh `self.theme_names = Theme::available(&self.themes_dir)` before cycling, and matches `self.theme.name` rather than `self.wanted_theme`.

**Suggested Change:**
1. Remove `name = "..."` from [contrib/themes/noctalia.toml](file:///home/tylerstyle/git/matrix-rat/contrib/themes/noctalia.toml#L3), [contrib/themes/wallust.toml](file:///home/tylerstyle/git/matrix-rat/contrib/themes/wallust.toml#L2), and [contrib/themes/pywal.toml](file:///home/tylerstyle/git/matrix-rat/contrib/themes/pywal.toml#L2) so `Theme::from_file` automatically uses `fallback_name` (the `.toml` filename stem: `"noctalia"`, `"matugen"`, `"wallust"`, `"pywal"`).
2. In `Action::CycleTheme` ([src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L821-L829)), refresh `self.theme_names = Theme::available(&self.themes_dir)` and look up `self.wanted_theme` (falling back to `self.theme.name`).

---

### 4. [MEDIUM] Editing `config.toml` triggers `ThemeWatch` without re-reading `config.toml`, and `reload_theme()` toasts errors when `wanted_theme` is not yet generated
**Location:** [src/watch.rs](file:///home/tylerstyle/git/matrix-rat/src/watch.rs#L46), [src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L289-L320)

**Problem:**
1. `ThemeWatch` watches `paths.config_dir` (`~/.config/matrix-rat`) in addition to `paths.themes_dir()`. However, when the user edits `~/.config/matrix-rat/config.toml` to set `theme = "noctalia"` while `matrix-rat` is running, `App::reload_theme()` only reloads `self.wanted_theme` and never checks whether `config.toml`'s `theme` changed.
2. When `self.wanted_theme` is a generator theme whose file (`themes/<name>.toml`) does not exist yet (or was momentarily unlinked by a generator before rewriting), any `.toml` change in `config_dir`/`themes_dir` or `SIGUSR1` causes `reload_theme()` to call `set_theme(&name)` and pop up an error toast (`"Could not load theme <name>: unknown theme ..."`).

**Suggested Change:**
1. Store `config_file: PathBuf` and `config_theme: String` on `App`: in `reload_theme()`, if `Config::load(&self.config_file)` succeeds and `cfg.theme != self.config_theme`, update `self.config_theme = cfg.theme.clone()` and `self.wanted_theme = cfg.theme`.
2. In `reload_theme()`, only call `self.set_theme(&name)` if `Theme::builtin(&name).is_some() || self.themes_dir.join(format!("{name}.toml")).exists()` (so missing generator files wait quietly until they appear on disk, while an existing broken file still shows the error toast).

---

### 5. [LOW] `pywal` symlink setup in `contrib/themes/README.md` is not seen by `inotify` on `themes/` without a reload hook or target watch
**Location:** [contrib/themes/README.md](file:///home/tylerstyle/git/matrix-rat/contrib/themes/README.md#L82-L90), [src/watch.rs](file:///home/tylerstyle/git/matrix-rat/src/watch.rs#L45-L51)

**Problem:**
[contrib/themes/README.md](file:///home/tylerstyle/git/matrix-rat/contrib/themes/README.md#L84-L87) instructs `pywal` users to symlink `~/.cache/wal/matrix-rat.toml` into `~/.config/matrix-rat/themes/pywal.toml`:
```sh
matrix-rat --print-template pywal > ~/.config/wal/templates/matrix-rat.toml
ln -sf ~/.cache/wal/matrix-rat.toml ~/.config/matrix-rat/themes/pywal.toml
```
Because `ThemeWatch` watches `~/.config/matrix-rat/themes` non-recursively via `inotify`, writes by `wal` to the symlink target inside `~/.cache/wal/` do not generate `inotify` events on `~/.config/matrix-rat/themes`.

**Suggested Change:**
1. In [contrib/themes/README.md](file:///home/tylerstyle/git/matrix-rat/contrib/themes/README.md#L82-L90), mention passing `-o "matrix-rat --reload-theme"` to `wal` (just like `post_hook` for Noctalia and matugen).
2. Optionally, in `ThemeWatch::start`, also watch the canonical parent directory of any symlinked `.toml` file found in `paths.themes_dir()`.

---

### 6. [LOW] `contrib/themes/noctalia.toml` includes `on_surface_variant` (`muted`) in `[users].colors`
**Location:** [contrib/themes/noctalia.toml](file:///home/tylerstyle/git/matrix-rat/contrib/themes/noctalia.toml#L18-L26)

**Problem:**
`[users].colors` in `contrib/themes/noctalia.toml` includes `"{{colors.on_surface_variant.default.hex}}"`, which is the exact same token as `palette.muted` (line 10). Senders who hash to that slot have their display names rendered in the dimmed/muted colour used for timestamps and metadata.

**Suggested Change:**
Drop `"{{colors.on_surface_variant.default.hex}}"` from `[users].colors` in `contrib/themes/noctalia.toml` (leaving the 5 distinct accent colours `primary`, `secondary`, `tertiary`, `error`, `inverse_primary`).

---

## Verdict

**Approve with changes.**

The hot-reload architecture, generator templates, and CLI flags (`--reload-theme`, `--print-template`) are clean and well-tested. Please address Findings 1 and 2 (cancel-safe `ThemeWatch::changed()` and lock-verified PID signalling in `signal_running_instances`) and the medium polish items (Findings 3 and 4) before pushing.
