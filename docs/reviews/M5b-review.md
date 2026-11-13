# Review: M5 part 2 — Saving and Opening Attachments (commit `1a301c5`)

Scope reviewed against commit `1a301c5`: `src/files.rs`, `src/app/files.rs`, `Media::save` in `src/matrix/media.rs`, `attach` / `MessageType::{File, Video, Audio}` in `src/matrix/convert.rs`, `Attachment` in `src/model.rs`, `DownloadDir` settings inline editing (`src/modal.rs`, `src/ui/modal.rs`, `src/app.rs`), and keys `s` / `o`.

All `file:line` references below point to commit `1a301c5`.

---

## Findings

### 1. High — `Action::Open` (`o`) hands arbitrary sender-chosen file types to `xdg-open` without warning, filtering, or path-normalization (`src/app.rs`, `src/app/files.rs`, `src/files.rs`)

Pressing `o` invokes `Action::Open` ([`src/app.rs:1160-1170`](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L1160-L1170)), which falls through to `self.save_attachment(true)`. That downloads the attachment into `download_dir` keeping the sender's file extension (`while name.len() > 200 { name.remove(0); } // File systems limit names to 255 bytes; keep the end, where the extension is` in [`src/files.rs:26-29`](file:///home/tylerstyle/git/matrix-rat/src/files.rs#L26-L29)) and immediately calls `open_in_browser(&path.to_string_lossy())` ([`src/app/files.rs:41-47`](file:///home/tylerstyle/git/matrix-rat/src/app/files.rs#L41-L47), [`src/app/files.rs:75-92`](file:///home/tylerstyle/git/matrix-rat/src/app/files.rs#L75-L92)), spawning `Command::new("xdg-open").arg(url)` ([`src/app.rs:1997-2005`](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L1997-L2005)).

Security and correctness hazards here:
- **Unrestricted desktop handler execution:** A hostile sender can attach `.desktop`, `.sh`, `.py`, `.AppImage`, `.html`/`.svg` (script execution), `.pdf`/office macro formats, or `.exe` (under Wine), and a single `o` press launches the registered desktop handler silently—even minutes later when a slow `Media::save` completes in the background while the user has moved on.
- **Flag/relative-path exposure with `xdg-open`:** Unlike `user_dirs_download` ([`src/files.rs:67`](file:///home/tylerstyle/git/matrix-rat/src/files.rs#L67)), `resolve_dir` ([`src/files.rs:71-82`](file:///home/tylerstyle/git/matrix-rat/src/files.rs#L71-L82)) accepts relative paths such as `downloads` or `-x` as `PathBuf::from(setting)`. Passing an un-canonicalized path starting with `-` directly to `Command::new("xdg-open").arg(&path)` can be misparsed as an `xdg-open` flag rather than a file path.

**Suggested changes:**
- Restrict `o` (`open = true`) or gate risky extensions/MIME types behind an explicit confirmation (or only auto-open known-safe viewer types such as common image/audio/video/plain-text formats, warning or refusing executable/script/desktop/HTML types, or prompting via `confirm` with the extension and handler warning before calling `xdg-open`).
- Ensure `resolve_dir` resolves to an absolute path (or canonicalize / prepend `./` to relative paths before handing paths to `xdg-open`), so leading `-` paths cannot be interpreted as CLI options.

---

### 2. High — Unbounded RAM buffering (up to 2x file size) and bypassable `LARGE_FILE` guard (`src/matrix/media.rs`, `src/app/files.rs`)

`Media::save` ([`src/matrix/media.rs:81-121`](file:///home/tylerstyle/git/matrix-rat/src/matrix/media.rs#L81-L121)) fetches the entire media payload via `client.media().get_media_content(&request, false).await` ([`src/matrix/media.rs:96-99`](file:///home/tylerstyle/git/matrix-rat/src/matrix/media.rs#L96-L99)), then passes `&bytes` to `files::write_new(&dir, &name, &bytes)` inside `spawn_blocking`. Because `matrix-sdk`'s `get_media_content` buffers the full body into `Vec<u8>` (and encrypted attachments allocate an additional buffer during decryption), the process holds up to roughly 2x the file size in memory at once.

Two compounding problems:
- **Untrusted size check:** [`src/app/files.rs:52-63`](file:///home/tylerstyle/git/matrix-rat/src/app/files.rs#L52-L63) computes `let large = file.size.filter(|size| *size > files::LARGE_FILE);` (`LARGE_FILE = 100 * 1000 * 1000` at [`src/files.rs:9`](file:///home/tylerstyle/git/matrix-rat/src/files.rs#L9)). `file.size` comes straight from the remote sender's event metadata (`content.info.size`). A hostile sender can omit `info.size` (`None`) or claim `Some(10)` on a multi-gigabyte blob; the confirmation toast is skipped and `Media::save` downloads until OOM.
- **No byte cap or concurrency throttle in `Media::save`:** Unlike image loading (which enforces `MAX_DOWNLOAD` and acquires `self.slots`), `Media::save` has no hard download-size ceiling and does not acquire `self.slots` (`PARALLEL = 4` at [`src/matrix/media.rs:22`](file:///home/tylerstyle/git/matrix-rat/src/matrix/media.rs#L22)), while allowing `SAVE_TIMEOUT` up to 30 minutes ([`src/matrix/media.rs:25`](file:///home/tylerstyle/git/matrix-rat/src/matrix/media.rs#L25)).

**Suggested changes:**
- Until `matrix-sdk` supports streaming encrypted media downloads (or if you implement chunked raw MXC HTTP download + streaming AES-CTR/SHA-256 decryption yourself), enforce a hard memory-protection limit inside the download path (either rejecting / checking `Content-Length` and aborting if bytes exceed a safe max cap, or bounding concurrent saves behind `self.slots` / a small save semaphore).
- Treat `file.size == None` (unknown size) as requiring confirmation too, rather than silently trusting missing metadata.

---

### 3. Medium — Symlink-following `.part` open and 0-byte collision window in `write_new` (`src/files.rs:95-138`)

`write_new` ([`src/files.rs:96-128`](file:///home/tylerstyle/git/matrix-rat/src/files.rs#L96-L128)) checks candidate filenames up to `0..10_000` using `OpenOptions::new().write(true).create_new(true).open(&candidate)` (which safely avoids overwriting existing files or following symlinks at `candidate`), then writes the payload to a temporary `.part` path before `rename(&partial, &candidate)`:

```rust
let partial = dir.join(format!(".{}.{}.part", std::process::id(), n));
let written = std::fs::File::create(&partial)
    .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()))
    .and_then(|()| std::fs::rename(&partial, &candidate));
```

Two issues remain:
- **`.part` follows symlinks (`File::create`):** `partial` has a predictable name (`.{pid}.{n}.part`) and is opened with `std::fs::File::create(&partial)` ([`src/files.rs:118-120`](file:///home/tylerstyle/git/matrix-rat/src/files.rs#L118-L120)), which omits `O_EXCL` / `create_new(true)`. If a symlink (for instance in a shared folder or left behind before PID reuse) exists at `.{pid}.{n}.part`, `File::create` follows it and truncates/overwrites the symlink target.
- **0-byte placeholder left at `candidate` on hard crash:** Because `candidate` is created before `bytes` is written and synced to `partial`, a `SIGKILL` or power failure during `write_all` + `sync_all` leaves an empty 0-byte `candidate` (plus `.part`) on disk; subsequent saves of the same file then observe `AlreadyExists` and rename to `name (1).ext`.

**Suggested changes:**
- Open the temporary file with `OpenOptions::new().write(true).create_new(true)` (using a unique/retry suffix, or `tempfile::NamedTempFile::new_in(dir)`).
- Write and `sync_all()` first, then publish with `std::fs::hard_link(&partial, &candidate)` + `remove_file(&partial)` on Unix (retaining `n` retry on `AlreadyExists`), which fails cleanly if `candidate` exists or is a symlink and never exposes a 0-byte `candidate`. Keep a fallback (`create_new` reservation + `rename`) only if `hard_link` fails with an unsupported-filesystem error (e.g. exFAT/FAT32/FUSE).

---

### 4. Medium — `safe_name` trims leading dots and spaces before byte truncation (`src/files.rs:13-29`)

`safe_name` ([`src/files.rs:13-29`](file:///home/tylerstyle/git/matrix-rat/src/files.rs#L13-L29)) correctly uses `sanitize_line` (which strips control chars as well as Unicode Bidi overrides `U+200E`, `U+200F`, `U+061C`, `U+202A..=U+202E`, `U+2066..=U+2069` at [`src/text.rs:29-41`](file:///home/tylerstyle/git/matrix-rat/src/text.rs#L29-L41)), splits on both `'/'` and `'\\'`, trims leading dots/whitespace, and maps `':' | '\0'` to `'_'`.

However, truncation runs *after* dot/whitespace trimming:
```rust
    let last = cleaned
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .trim_start_matches('.')
        .trim();
    ...
    while name.len() > 200 {
        name.remove(0);
    }
```
If a hostile sender sends a filename >200 bytes containing `.` or spaces at index `len - 200` (e.g., 20 `'x'` bytes + `".bashrc"` + 193 `'a'` bytes = 220 bytes), removing the first 20 bytes exposes a leading `.` (creating a hidden dotfile in `download_dir`) or leading whitespace.

*(Minor note: a sender filename with a trailing separator such as `"dir/file/"` causes `rsplit(['/', '\\']).next()` to yield `""` and fall back to the default filename instead of `"file"`.)*

**Suggested changes:**
- Re-run `.trim_start_matches('.').trim()` (and the empty-name fallback check) *after* truncating `name` to `<= 200` bytes.
- Optionally trim trailing `'/'` and `'\\'` before `rsplit(['/', '\\'])`.

---

### 5. Medium — Settings `DownloadDir` `u16` underflow panic at narrow widths (`src/ui/modal.rs:196-197`) and session/path edge cases (`src/app.rs`, `src/files.rs`)

- **UI panic on narrow terminal widths:** In [`src/ui/modal.rs:196-197`](file:///home/tylerstyle/git/matrix-rat/src/ui/modal.rs#L196-L197):
  ```rust
  let value_x = rect.x + 2 + LABEL; // LABEL = 20
  let room = (rect.right() - value_x) as usize;
  ```
  Whenever the centered modal has `rect.width < 22`, `value_x > rect.right()` and `rect.right() - value_x` panics with `u16` subtraction underflow in debug builds (or wraps in release builds). Even at `rect.width == 22`, `room == 0`, placing `value_x` on the border and setting cursor at `rect.right()` ([`src/ui/modal.rs:218-230`](file:///home/tylerstyle/git/matrix-rat/src/ui/modal.rs#L218-L230)).
- **Permissive `resolve_dir` values:** `resolve_dir` ([`src/files.rs:71-82`](file:///home/tylerstyle/git/matrix-rat/src/files.rs#L71-L82)) maps `"~"` directly to `home()` (even though `user_dirs_download` at [`src/files.rs:67`](file:///home/tylerstyle/git/matrix-rat/src/files.rs#L67) explicitly rejects `path == home()`), and turns any non-`~/` value into `PathBuf::from(setting)` without requiring an absolute path. Relative paths depend on whatever directory `matrix-rat` was launched from.
- **Session reset & `self.saved` staleness:** `App::reset_session` ([`src/app.rs:498-512`](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L498-L512)) clears `self.saving` and `self.confirm_large`, but does **not** clear `self.saved` (nor is `self.saved` cleared when `set_download_dir` changes the target directory at [`src/app/files.rs:95-102`](file:///home/tylerstyle/git/matrix-rat/src/app/files.rs#L95-L102)). Worse, an in-flight `Media::save(..., open = true)` started before logout still emits `MatrixEvent::FileSaved`, so `on_file_saved` ([`src/app/files.rs:75-92`](file:///home/tylerstyle/git/matrix-rat/src/app/files.rs#L75-L92)) will open the downloaded file after logout.

**Suggested changes:**
- Compute `let room = rect.right().saturating_sub(value_x) as usize;` and skip `frame.set_cursor_position(...)` when `room == 0` (also guard `value_x + (width(&shown) as u16) < rect.right()`).
- Require `resolve_dir` to produce an absolute path (for example, reject or anchor relative paths against `home()` or `default_dir()` and warn/normalize if `download_dir` resolves to `home()`).
- Clear `self.saved` in `reset_session()` (and on `set_download_dir()`), and ignore/drop `open` in `on_file_saved` if no session is active or bump a session generation counter.

---

### 6. Low — Minor UX and dispatch issues on `o` / `confirm_large` (`src/app.rs`, `src/app/files.rs`)

- **Caption links shadow `o` on attachments:** `Action::Open` ([`src/app.rs:1160-1170`](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L1160-L1170)) runs `self.open_image(i) || self.open_link(i, 0) || self.save_attachment(true)`. If a file, video, or audio message includes a URL in its caption/body, pressing `o` opens the URL instead of saving/opening the attached file.
- **Toast always names `s` even when triggered with `o`:** [`src/app/files.rs:56-60`](file:///home/tylerstyle/git/matrix-rat/src/app/files.rs#L56-L60) displays `"{} is {}. Press s again to download it"` even when the user pressed `o` (`open = true`), and `confirm_large` keys only on `file.key` without tracking whether `s` or `o` was requested.

---

## Verdict

**Approve with changes.**

Path traversal stripping and destination collision handling (`create_new(true)` on `candidate`, Bidi/control stripping via `sanitize_line`) are solid foundations, but the unrestricted `xdg-open` launch on sender-controlled filenames/extensions, the unthrottled/unbounded memory buffer when `info.size` is missing or spoofed, the `File::create` symlink/0-byte crash window in `write_new`, the `safe_name` dot-trim ordering, and the narrow-width `u16` underflow in `DownloadDir` settings editing should be fixed before pushing.
