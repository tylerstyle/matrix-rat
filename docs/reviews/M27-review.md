# M27 Review — Paste Picture from Clipboard (`paste-picture`)

Reviewed `main...paste-picture` (`4e98d94..9b451a1`, 3 commits).

## Summary

The clipboard image paste feature and worker file-send accounting are cleanly structured and well-tested:
- **Process handling in `run_tool` (`src/clipboard.rs:302-376`):**
  - Runs `wl-paste` or `xclip` directly via `Command::new(program).args(args)` with no shell, `stdin(Stdio::null())`, `stdout(Stdio::piped())`, and `stderr(Stdio::null())`.
  - Uses `libc::poll` on the stdout descriptor with the remaining time budget up to `TIME_LIMIT` (5s shared across the `--list-types`/`TARGETS` run and the content run), retrying on `EINTR` and enforcing `max` (`MAX_LIST` = 64 KiB for target listing; server upload limit or 100 MB `FALLBACK_LIMIT` for image bytes) before extending the buffer.
  - After EOF on stdout, waits up to `deadline` via `child.try_wait()` for the process to exit cleanly; on return from `collect`, `child.stdout` is closed and `run_tool` unconditionally runs `child.kill()` followed by `child.wait()`, ensuring no orphan or zombie process on any exit path.
- **Validation of clipboard output (`src/clipboard.rs:135-147, 381-438`):**
  - `choose()` matches only whole ASCII-trimmed lines (`image/png`, `image/jpeg`, `image/webp`, `image/gif`, in preference order).
  - `check()` verifies that `image::guess_format(bytes)` matches the requested `Kind` and runs `crate::images::decode(bytes, (1, 1))`, applying the same dimension/memory limits and full-frame decode check used for timeline images.
- **Temp file creation, UTC naming, and cleanup (`src/clipboard.rs:241-297`, `src/app/paste_picture.rs`, `src/app/attach.rs`):**
  - Creates `$XDG_RUNTIME_DIR/matrix-rat` with `0700` permissions if missing, names files in UTC (`pasted-%Y%m%d-%H%M%S.<ext>`, with `-2`..`-8` collision suffixes) so the sender's local timezone is not leaked to the room, and opens with `create_new(true)` and `mode(0o600)` so symlinks are never followed and files are owner-only from creation.
  - Wrapping the path in `TempPicture` before `write_all` guarantees removal on write failure, and `AttachState::sweep()` + `Drop for TempPicture` cleans up files across cancellation (`Backspace`, `✕`, `Ctrl-C`), room departure (`forget_room`), stale paste completion after room departure or logout, normal send completion (`FilesSent`), and app exit.
- **Privacy (`src/clipboard.rs`, `src/matrix/mod.rs`):** `Ran`, `TempPicture`, and `MatrixEvent::FilesNotSent` have custom `Debug` implementations that never log clipboard contents, file paths, or filenames.

## Notes (`FilesSent` / `FilesNotSent` Accounting)

1. **Overwriting `handed` when a second send is started before the first is answered (`src/app/attach.rs:37, 426-433, 452-480`):**
   - `self.attach.sends_out` counts all in-flight `SendFiles` commands (and `Uploads::send` spawns each one as a separate task on its `JoinSet`), whereas `self.attach.handed: Option<Handed>` only stores the *most recent* send.
   - If the user sends attachment batch 1 in room A (`sends_out = 1`, `handed = Some(batch 1)`) and, before the worker answers (e.g., while `load_or_fetch_max_upload_size` or large-file preparation is running), sends attachment batch 2 in room B (`sends_out = 2`, `handed = Some(batch 2)`):
     - Batch 1's `Handed` entry is overwritten and lost.
     - If batch 2 finishes first (`FilesSent`, `sends_out` goes `2 -> 1`), `on_files_sent` keeps `handed` (`Some(batch 2)`) because `sends_out != 0`. If batch 1 then fails (`FilesNotSent`, `sends_out` goes `1 -> 0`), `on_files_not_sent` takes `handed` and restores **batch 2** (which was already sent!) into room B's composer, while batch 1 is lost (and any `TempPicture` from batch 1 is deleted by `sweep()`).
     - Conversely, if batch 1 fails first (`FilesNotSent`), `on_files_not_sent` restores batch 2 while batch 2 is still being sent by the worker, duplicating batch 2 and losing batch 1.
   - Tagging each `SendFiles` -> `FilesSent` / `FilesNotSent` with a send ID (or keeping `handed` as a map/queue keyed by send ID) ensures each answer matches its own batch.

2. **`on_files_not_sent` for a room that was left while the send was in flight (`src/app/attach.rs:78-82, 452-468`):**
   - `AttachState::forget_room(room_id)` removes `pending` and `stash` entries for `room_id`, but does not clear `self.handed` when `self.handed.as_ref().is_some_and(|h| h.scope.0 == room_id)`, and `on_files_not_sent` does not check `self.rooms.iter().any(|r| r.id == handed.scope.0)` (unlike `finish_paste` at `src/app/paste_picture.rs:78`).
   - If a send fails after its room was left and removed from `self.rooms`, `on_files_not_sent` re-inserts `handed.files` into `self.attach.pending` for the departed room; because `forget_room` will not run again for that room, any `TempPicture` in that batch stays in `self.attach.pending` (and on disk) until logout or exit.
   - Clearing `handed` in `forget_room` when `handed.scope.0 == room_id` (and/or guarding `on_files_not_sent` with `self.rooms.iter().any(|r| r.id == handed.scope.0)` and `!self.logged_out`) closes this leak.
