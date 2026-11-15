# M28 Review — Privacy & Shutdown Hardening (`hardening`)

Reviewed `main...hardening` (`5` commits: `78c9257`, `1efd373`, `fa564f3`, `2b82057`, `75f6689`).

## Verdict

**Approve / ready for `0.6.0`** (no release blockers found).

---

## 1. Commit `78c9257` — Server Room-Life Test & Shutdown `token_saver` Wait

- **`a_room_from_creation_to_forgetting` (`src/matrix/manage.rs:2793-2802`, `3033-3057`):**
  - Waiting on `d.name.contains("manage test") && !d.topic.is_empty() && d.encrypted == Some(true) && d.members.len() == 1 && d.can_invite` matches how sliding sync can deliver initial room state across multiple responses (name before topic/encryption/power state).
  - The post-forget `ManageCmd::Info` loop filters `ManageEvent::Info { room_id: about, result }` by `about == room_id`, tolerating at most one in-flight `Ok` result (`!asked_again`, sleeping 2 s, asking once more) before requiring `result.is_err()`. That cleanly absorbs a late pre-leave answer without masking a room that actually failed to be forgotten.
- **`token_saver` await (`src/matrix/mod.rs:1954-1957`):**
  - `Worker::run_session` calls `token_saver.abort()` and now awaits `token_saver.await` before returning `Ok(end)` to `run`, where `SessionEnd::Expired` calls `session::forget` (`src/matrix/mod.rs:1399`) and `SessionEnd::LoggedOut` calls `session::clear` (`src/matrix/mod.rs:1424`).
  - Because `token_saver` invokes synchronous `session::save_tokens` (`src/matrix/mod.rs:1601`), `abort()` alone leaves an already-polled save free to finish rewriting the session file after `forget()`/`clear()` deletes it. Awaiting completion prevents that race.

---

## 2. `logsafe::kind` / `kind_any` & Nested Error Chains

- **No arbitrary text can pass through (`src/logsafe.rs:24-34`, `77-92`, `254-260`):**
  - `Why` is `Clone + Copy` and can hold only a `u16` HTTP status, `std::io::ErrorKind`, `&'static str`, or `Code` (`[u8; 40]`). Context strings from `anyhow` or SDK display strings are never copied into `Why`.
  - `answer()` (`src/logsafe.rs:254-260`) inspects only `err.status_code.as_u16()` and `err.error_kind().map(|kind| Code::new(kind.errcode().as_str()))`. Error messages, response bodies, `Json`/`NotJson` variants, and contact/limit URIs are never touched.
  - `Code::new(code)` (`src/logsafe.rs:79-92`) enforces `!code.is_empty() && code.len() <= 40` and ASCII `[A-Za-z0-9_.]`, mapping anything else (`@`, `!`, `$`, `:`, `/`, whitespace, control characters, non-ASCII UTF-8, or oversized strings such as MSC codes longer than 40 bytes) to `"unusual code"`. An attacker-controlled server cannot smuggle IDs, sentences, URLs, or escape sequences via `errcode`.
- **Nested error precision nuance (non-blocking, privacy-safe):**
  - `kind()` (`src/logsafe.rs:114-130`) walks `err.source()` up to depth `32`, preferring the first `Why::is_precise()` match over `Why::Word`.
  - Because `thiserror`'s `#[error(transparent)]` delegates `.source()` past the wrapped type and `ruma_common::api::error::FromHttpResponseError::source()` returns `None`, a few `matrix_sdk_ui::timeline::Error` variants mapped in `timeline()` (`src/logsafe.rs:282-291`) lose inner HTTP/SDK detail:
    - `timeline::Error::RedactError(RedactError::HttpError(HttpError::Api(...)))`: `.source()` skips `HttpError` to `None`, so `kind()` stops at `"timeline"` instead of `"HTTP <status> <code>"`.
    - `timeline::Error::EditError(EditError::RoomError(room::edit::EditError::Fetch(_)))` and `ReplyError::Fetch(_)`: format `Box<matrix_sdk::Error>` without `#[source]`, so `kind()` also stops at `"timeline"`.
    - `timeline::Error::EventCacheError(EventCacheError::PaginationError(Arc<matrix_sdk::Error>))`: `#[error(transparent)]` skips `Http` to `None`, logging `"event cache"` (unlike `PaginatorError::SdkError(#[from] Arc<matrix_sdk::Error>)`, which sets `#[source]`).
    - `SendAttachment::use_send_queue().await` maps send-queue failures into `timeline::Error::FailedSendingAttachment`, which logs `"timeline"`.
  - All of these remain completely safe and are fine for `0.6.0` (only worth expanding inside `timeline()` post-`0.6.0` if finer timeline error diagnostics are wanted).

---

## 3. Lost Diagnostics, User-Facing Messages, Panic Hook, & Spot-Check

- **Diagnostics & minor omissions (non-blocking):**
  - `src/logging.rs:15-16` disables `matrix_sdk::http_client=off` and `matrix_sdk::notification_settings=off` in `DEFAULT_FILTER` so failed HTTP bodies and keyword rule IDs are suppressed at SDK level as well.
  - A few worker call sites drop the error before logging and could optionally record `why = %crate::logsafe::kind(&err)` (or `kind_any`) post-`0.6.0`:
    - `src/matrix/keywords.rs:242-243` (`mark_as_mention` drops its `HttpError`, unlike `add`/`remove` which only receive `NotificationSettingsError`).
    - `src/matrix/devices.rs:355-361` and `395-414` (`rename`, `export_keys`, `import_keys` format `Err(err)` for the UI event before logging without `why`).
    - `src/matrix/rooms.rs:307-312, 1152` (`files_not_sent` takes a `String` and logs `"files could not be sent"` without `why`).
    - `src/matrix/people.rs:420, 481-482` (`fetch_user_profile` / `load_own_profile` drop `Err` without `why`).
    - `src/matrix/threads.rs:308` (logs `Option<u16>` `status` rather than `logsafe::kind(&err)`, missing network/timeout/errcode detail).
- **User-facing messages unchanged:**
  - Verified that `MatrixEvent::Error`, `MatrixEvent::LoginFailed`, `MatrixEvent::ActionFailed`, `MatrixEvent::FilesNotSent`, `ManageEvent` explanations, device rename/import/export errors, `app.show_toast`, and `editor.feedback` still receive the full formatted error text for the user.
- **Panic hook (`src/terminal.rs:202-230`):**
  - `install_panic_hook` logs `file = place(at.file()), line = at.line(), "panic"` (or just `"panic"` if `location()` is `None`), leaving panic payloads out of the log file while forwarding `info` to the restored terminal via `previous(info)`.
  - `place()` preserves relative crate paths, strips paths up to `/registry/src/<index>/` for dependency sources, and falls back to `rsplit('/').next()` for any other absolute path, keeping home directories and local build paths out of logs.
- **Tracing spot-check:**
  - Checked `tracing::` calls across `main.rs`, `watch.rs`, `portal.rs`, `focus.rs`, `notify.rs`, `app.rs`, `app/keybinds.rs`, `images.rs`, and `src/matrix/{mod.rs, manage.rs, devices.rs, verify.rs, people.rs, rooms.rs, session.rs, upload.rs, media.rs, keyring.rs, keywords.rs, notify.rs, search.rs}`.
  - Zero non-test `eprintln!`, `dbg!`, `log::`, or `#[tracing::instrument]` calls remain. `app.rs:447` and `app.rs:734` log only config keybind/theme warnings (`theme = name` and style-key names).

---

## 4. `describe()` & Remaining Derived `Debug` Types

- **`#[cfg(test)]`-only `describe()`:**
  - `MatrixCmd::describe` (`src/matrix/mod.rs:426-429`), `MatrixEvent::describe` (`src/matrix/mod.rs:994-997`), and `ManageCmd::describe` (`src/matrix/manage.rs:267-270`) are all gated with `#[cfg(test)]`.
  - Searching all callers of `.shown(` across `src/` confirms only the three `Debug` impls (`shown(false)`) and the three `#[cfg(test)]` `describe()` methods (`shown(true)`, plus nested forwarding inside `MatrixCmd::shown`) call it.
  - All nested payloads printed inside `MatrixCmd`, `ManageCmd`, `MatrixEvent`, and `ManageEvent` have redacted hand-written `Debug` impls or contain only safe counts/enums/booleans (`DetachedTimeline` at `src/matrix/mod.rs:744` derives `Debug`, but `MatrixEvent::shown` prints only `"detached: {}", detached.is_some()`).
- **Remaining derived `Debug` types (none blocking `0.6.0`):**
  - Every credential, secret, password/passphrase, form/popup input, message, room, attachment batch, composer, global search, jump target, and worker wire command/event now has a redacted hand-written `Debug` impl (`LineEdit`, `Composer`, `ResolvedPassphrase`, `KeyringKey`, `LoginForm`, `RecoveryForm`, `ExportForm`, `ImportForm`, `SignOut`, `Rename`, `Message`, `Room`, `RoomPower`, `Chosen`, `TempFile`, `AttachState`, `GlobalSearch`, `PendingJump`, `ForwardTarget`, etc.).
  - The remaining ~40 types with `#[derive(Debug)]` (`App`, `Modal`, `ReauthInfo`, `EmojiPicker`, `Threads`, `ThreadView`, `Palette`, `PaletteContext`, `CompleteState`, `Menu`, `PreviewStore`, `Paths`, `Config`, `Rich`, `LineView`, `FallbackCandidate`, etc.) are **never** formatted into logs or telemetry in production; the only structs logged with `{:?}` are `KeywordsCmd`, `Security`, `termquery::Reply`, and `images::Graphics`.
  - None of the remaining derives should block `0.6.0`. Post-`0.6.0`, `ReauthInfo` (`src/modal.rs`, which holds `user_id` and `device_id`) or UI structs holding user text/IDs (`EmojiPicker`, `Threads`/`ThreadView`, `PaletteContext`, `Rich`) could be redacted if someone ever wanted defense-in-depth against a future `tracing::debug!(?...)` on UI state.
