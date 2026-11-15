# M21 review — `comforts` (`de387fd`)

- **Branch / Commits:** `comforts` at `de387fd` (`63fa72a` copy code block, `a3b4b1e` `$EDITOR`, `df3254e` link previews, `de387fd` event-stream wake fix; `git diff main...comforts`).
- **Tests:** 692 unit tests green on `hpfury` (per handover; not re-run locally).
- **Verdict:** **Approved.** Clean implementation across all three features, thorough unit and UI test coverage, no new crates in `Cargo.lock` (`shlex` 1.3.0 was already present), and strict adherence to URL/draft/message redaction in `Debug` and logs. Three minor `[LOW]` notes below for optional polish.

---

## What was checked

1. **Copy code block (`63fa72a`):**
   - `RichMessage::code_blocks` (`src/rich.rs`) collects both fenced Markdown blocks and HTML `<pre>` blocks in document order and tags lines via `Segment::code_block`.
   - `build_layout` (`src/ui/timeline.rs:480-496`) places a right-aligned `"copy"` hit (`HitId::CopyCode(i, block)`) on the first line of each code block when `col + 1 + label_width <= max_width`.
   - `Action::CopyCodeBlock` (`y` in `[keys.timeline]`) copies immediately when a message has one block, opens `Modal::CodeBlocks` when it has several (`j`/`k`, `1`-`9`, `Enter`/`y`), and falls back to copying the whole message when it has none (`src/app/code.rs`). Context menu (`MenuAction::CopyCodeBlock`) and command palette match.

2. **External `$EDITOR` (`a3b4b1e`, `de387fd`):**
   - Command resolution (`$VISUAL`, `$EDITOR`, fallback `vi`) uses `shlex::split` (`src/editor.rs:163-189`) without invoking a shell.
   - `Terminal::run_editor` (`src/terminal.rs:146-175`) and `App::run` (`src/app.rs:514-525`) drop the crossterm `EventStream` before `leave_tui()`, run the editor synchronously with inherited stdio, restore the TUI and `EventStream` afterwards regardless of editor exit outcome, and `de387fd` absorbs the single `None` wake left behind by dropping the previous `EventStream`.
   - `editor::Request` and `editor::Outcome` redact draft contents and file paths in `Debug` (`src/editor.rs:42-61`).

3. **Link previews (`df3254e`):**
   - Off by default (`LinkPreviews::Off`), configurable via `link_previews = "off" | "unencrypted" | "on"` in `config.toml` and cycled in the Settings modal (`src/config.rs`, `src/ui/modal.rs`, `src/app/previews.rs`).
   - Privacy gates in `preview_url` (`src/ui/timeline.rs:547-568`) and `App::can_fetch_previews` (`src/app/previews.rs:82-89`): never previews or fetches for redacted messages, unrevealed spoilers, ignored senders, or encrypted rooms under `UnencryptedOnly`; strips `#fragment` before requesting; caps at `MAX_ENTRIES = 256` and `MAX_IN_FLIGHT = 4` with a 15 s timeout (`src/previews.rs`, `src/matrix/previews.rs`).
   - `Preview`, `Raw`, `MatrixCmd::LinkPreview`, and `MatrixEvent::LinkPreview` redact URLs and OpenGraph text in `Debug`, and `src/matrix/previews.rs` logs errors through `redact_error` without URLs.

---

## Minor notes (non-blocking)

### [LOW] `TempFile::create` fallback creates the file directly in `/tmp`
- **Where:** `src/editor.rs:94-101, 124-136`.
- **Note:** When `paths.runtime_dir` is unavailable, `TempFile::create` falls back to `Self::create_in(&std::env::temp_dir(), text)`. Because `/tmp` already exists (`dir.is_dir()` is true), `compose-<pid>-<n>.md` is created directly inside `/tmp` (`0o600`, `create_new(true)`). While `O_CREAT | O_EXCL` prevents symlink attacks on initial creation, editors that save via write-new-and-rename or create swap/backup files alongside the target file do so in world-writable `/tmp`. Creating a `0o700` per-user subdirectory inside `temp_dir()` on fallback (or falling back to `paths.cache_dir`) would keep editor backup/rename operations inside a private directory.

### [LOW] `RowKey::previews` uses `app.ignored_users.len()`
- **Where:** `src/ui/timeline.rs:724, 763`.
- **Note:** `RowKey::previews` stores `(app.previews.rev, app.link_previews, room.encrypted, app.ignored_users.len())`. If the ignored-user set ever changed membership while keeping the same length without `timeline_rev` changing, `RowKey` would compare equal. In practice `MatrixEvent::IgnoredUsers` reloads the timeline and bumps `timeline_rev`, so the cache is invalidated via `RowKey::messages`, or `app.previews.rev` could be bumped when `ignored_users` changes.

### [LOW] Nested `<pre>` tags in malformed HTML
- **Where:** `src/rich.rs:494-503, 572-583`.
- **Note:** In malformed HTML with nested `<pre>...<pre>...</pre>...</pre>`, the inner `<pre>` overwrites `self.block` and the first `</pre>` clears `self.pre = false` and `self.block = None`. Completely benign in practice.
