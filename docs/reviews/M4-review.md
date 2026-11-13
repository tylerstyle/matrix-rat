# Review: M4 — Device Verification & Encryption Status (`d6402dd..9505e36`)

*Reviewed against commit `9505e36` (M4 merged on top of M5a in `22 files`; note `HEAD` has since moved on to `1a301c5`, and all line numbers below match `9505e36`).*

The verification split between worker (`src/matrix/verify.rs`), UI state (`src/verify.rs`), app controller (`src/app/verify.rs`), and renderer (`src/ui/verify.rs`) is clean: `VerifyCmd::Debug` hides passwords, `set_peer` displays sanitized `user_id` (`@user:server`) rather than spoofing display names for third parties, `InnerSas::confirm` enforces crypto-level state transitions, and the `EncryptionSettings` change (`BackupDownloadStrategy::AfterDecryptionFailure`, `auto_enable_cross_signing: false`, `auto_enable_backups: false` in `src/matrix/session.rs:55-61`) matches the intended security model.

However, inspection alongside `matrix-sdk-0.19.1` / `matrix-sdk-crypto-0.19.1` turns up **3 High** and **2 Medium** issues that should be fixed before pushing.

---

## Findings (ordered by severity)

### High

1. **Cross-signing set-up has no UI password prompt and always fails on UIAA-requiring servers** — `src/app/verify.rs:113-118`, `src/matrix/verify.rs:420-446`
   - Clicking `Set up` (`Button::SetUp` in `PopupKind::SetUp`) sends `VerifyCmd::SetUp { password: None }` (`src/app/verify.rs:116`).
   - `set_up_cross_signing` (`src/matrix/verify.rs:420-446`) calls `bootstrap_cross_signing_if_needed(None)`. Unless the homeserver supports MSC3967 passwordless first-upload, the server returns UIAA (`err.as_uiaa_response()`), and `lines 432-434` return `Err("Your server asks for your password to set up encryption")` because `password` is always `None` from the UI (only tests in `src/matrix/verify.rs:1085-1087` supply `Some(password)`).
   - `on_cross_signing_set_up` (`src/app/verify.rs:232-244`) then closes the modal and flashes a toast, leaving users on standard Synapse/Matrix servers unable to bootstrap cross-signing from `matrix-rat`.
   - **Password handling audit (positive):** Existing backend plumbing handles the password safely—`VerifyCmd::Debug` (`src/matrix/verify.rs:181-189`) redacts it (`SetUp(password: true/false)`), `set_up_cross_signing` passes it once into `uiaa::Password::new(...)` without persisting it to `session.rs`/disk or logging it (`tracing::warn!` on `line 360` only logs `err`), and `LoginForm` masks password input.
   - **Suggested fix:** Add a masked password prompt inside `Modal::Verify` (`PopupKind::SetUp` or a retry step when UIAA asks for a password) that sends `VerifyCmd::SetUp { password: Some(..) }`, keeps the password field masked in UI and redacted in `Debug`, and clears it immediately after sending.

2. **`Button::Retry` ("Ask again") races `VerificationMachine::insert_request` and self-cancels both requests** — `src/matrix/verify.rs:336-345, 371-403`, `src/app/verify.rs:108-112`
   - Clicking `Ask again` (`Button::Retry` in `VerifyStage::Waiting`) marks the current flow dismissed and sends `VerifyCmd::Request` (`src/app/verify.rs:108-112`).
   - `Verifier::handle` (`src/matrix/verify.rs:336-345`) sees `running.outgoing == true` and calls `self.launch(None)`.
   - `Verifier::launch` (`src/matrix/verify.rs:371-395`) queues `Answer::Cancel` onto `old.answers` and *immediately* spawns the new `flow.run(client, None, answer_rx)` without waiting for `old` to call `request.cancel().await`.
   - The new `flow.run` (`line 546`) calls `request_own(&client)` -> `identity.request_verification_with_methods(...)` (`lines 756-757`). Inside `matrix-sdk-crypto-0.19.1` (`src/identities/user.rs:324` -> `src/verification/machine.rs:75-97, 165-198`), `VerificationMachine::insert_request` checks `self.requests` for the same user (`own_user_id`): if the previous request is still `!old_verification.is_cancelled()`, `insert_request` logs a warning and **cancels both `old_verification` and the newly created `request`** (`lines 177-192`).
   - Because `old`'s task usually hasn't even processed `Answer::Cancel` yet when `request_own` runs, "Ask again" reliably cancels itself upon creation (`asking_again_replaces_the_request_and_late_news_is_dropped` in `src/app/verify.rs` mocks `MatrixHandle`, so it never hit `VerificationMachine`).
   - **Suggested fix:** Before calling `request_own`, synchronously cancel and await completion (or cancellation state) of the previous `VerificationRequest` (with a short timeout if `cancel().await` waits on network) so `old_verification.is_cancelled()` is `true` in `VerificationMachine` before `request_verification_with_methods` inserts the replacement.

3. **Short panes clip emoji/decimal rows while keeping `They match` active, and unsolicited focus-stealing with `Space`/`Enter` on button `0` invites accidental confirmation** — `src/ui/verify.rs:68-90`, `src/app/verify.rs:27-33, 135, 195-230`, `src/verify.rs:176-182`
   - **Cryptographic SAS safety (verified):** At the protocol layer, `matrix-sdk-crypto-0.19.1/src/verification/sas/inner_sas.rs:243-264` ensures `Sas::confirm()` is a no-op outside `KeysExchanged | MacReceived`, and `Flow::answer` (`src/matrix/verify.rs:710-723`) only invokes `sas.confirm()` on `Answer::Match` when `sas` exists (`Button::Match` is only in `VerifyStage::Compare`). A client cannot cryptographically skip SAS key exchange.
   - **UI confirmation hazards:**
     1. *Clipped comparison data with active `They match` button (`src/ui/verify.rs:68-90`):* When a terminal/split pane is shorter than the popup (`verify_compare_narrow.snap` already uses 22 of 24 rows at width 44; wide `Compare` needs 17 rows), `centered()` (`src/ui/modal.rs:74-84`) shrinks `rect.height` down to `area.height - 1` (as small as 5 rows!), and `for (n, line)` stops at `if at.y + 2 >= inner.bottom() { break; }`—silently dropping bottom emoji rows, `"Do they match, in the same order?"`, or decimal digits—yet `lines 77-90` still draw `[ They match ]` at `focus = 0`, clickable and confirmable via `Enter`/`Space`.
     2. *Focus-stealing & `Space`/`Enter` confirming button `0` (`src/app/verify.rs:27-33, 135, 216-229`, `src/verify.rs:176-178`):* `may_show_verification` opens `Modal::Verify` immediately over normal typing, `Emoji`, or non-busy `Recovery`; every `stage_changed` resets `popup.focus = 0`; button `0` is always affirmative (`Accept` in `Requested`, `Start` in `Ready`, `Match` in `Compare`); and `on_verify_key` triggers `popup.focused` on both `KeyCode::Enter` and `KeyCode::Char(' ')`. A user typing spaces in the composer when a request arrives—or hitting `Space`/`Enter` right as the peer moves `Starting` -> `Compare`—can unintentionally `Accept` and/or confirm `They match` without visually comparing all 7 emojis.
   - **Suggested fix:**
     - Never render `[ They match ]` when the emoji grid or decimal tuple was truncated by `inner.bottom()`; instead compact the grid or require enlarging the terminal (`[ Cancel ]` only).
     - Default `focus` to the non-affirmative action (`Decline` / `Cancel` / `Mismatch`) or avoid `Char(' ')` confirming `Button::Match` (especially on unsolicited incoming requests, where popping open over active typing could also avoid auto-stealing keyboard focus—e.g. show the status-line hint and open on user click/shortcut, or protect incoming `Requested`/`Compare` from immediate `Space` presses).

---

### Medium

4. **Other-user (`in_room`) requests lack `to`/DM validation, leak uncancelled requests when busy, and have no tests** — `src/matrix/verify.rs:241-253, 294-332`
   - `in_room` (`src/matrix/verify.rs:242-253`) listens on `OriginalSyncRoomMessageEvent` for any `MessageType::VerificationRequest(_)` without checking whether `to == own_user_id` (or whether the room is a DM). Checking `matrix-sdk-crypto-0.19.1/src/verification/machine.rs:334-388` and `requests.rs:1096-1114`, `receive_any_event` validates timestamp (`<= 10m` old / `<= 5m` future), `!event_sent_from_us`, and `store.get_device`, without checking the `to` user field. In a multi-user room, a `m.key.verification.request` addressed by Alice to Bob would also trigger a third member's `in_room` handler if timestamped within 10 minutes.
   - When `self.busy()` (`src/matrix/verify.rs:323-330`), `on_incoming` ignores the new request with a toast without calling `request.cancel().await`, leaving the sender waiting for timeout and leaving an uncancelled request in `VerificationMachine::requests` (which can cause `insert_request` to dual-cancel a later request from that same user).
   - Positive note: when an incoming third-party request is accepted cleanly, `Flow::run` (`552-560`) and `set_peer` (`528-533`) properly set `own = false`, `this_device = false`, and `peer = sanitize_line(user_id.as_str())`, avoiding display-name spoofing. There is currently no UI action to *initiate* verifying another user (incoming only), and no unit/integration test exercises `in_room` or busy collisions.
   - **Suggested fix:** Filter `MessageType::VerificationRequest(req)` in `in_room` so `Some(&req.to) == client.user_id()` (or ignore non-DM / non-addressed requests), cancel rejected requests cleanly on `self.busy()` (e.g., if policy is to reject while busy, or queue/notify with the option to cancel), and add an integration/unit test covering other-user requests.

5. **Post-logout cleanup misses detached verification subtasks and stale queued events** — `src/app.rs:462-485`, `src/matrix/verify.rs:309, 357, 405-416, 593`, `src/app/verify.rs:189-244`
   - `App::change_setting(Setting::Logout)` calls `self.send_matrix(MatrixCmd::Logout); self.reset_session();` (`src/app.rs:462-464`) immediately on the UI thread, clearing `self.verify` (`newest = 0`, `dismissed = None`, `flow = None`, `security = None`, `setting_up = false`).
   - On the worker side, `Verifier::stop()` (`src/matrix/verify.rs:407-415`) removes event handlers and aborts `self.running` and `self.security`, **but** three verification tasks spawned with detached `tokio::spawn` are untracked and survive `stop()`: `set_up_cross_signing` (`line 357`), `await_own_signature` (`line 593`, sleeping up to 15s), and `refresh_own_keys` (`line 309`). They keep holding clones of the old `Client`/store across `session::clear()` / `fresh_store()` rename, and `set_up_cross_signing` can later deliver `MatrixEvent::CrossSigningSetUp` into the login screen or a newly logged-in account (`src/app/verify.rs:232-244`).
   - Additionally, because `App::reset_session()` zeroes `self.verify.newest = 0` *before* the worker handles `MatrixCmd::Logout` (`src/matrix/mod.rs:542`), any already-buffered `MatrixEvent::Verification` or `MatrixEvent::Security` in the channel arriving before `NeedLogin` passes the `update.flow < self.verify.newest` check (`src/app/verify.rs:190`) and re-opens `Modal::Verify` momentarily while logging out. (Also, `Verifier::stop` abandons a live verification rather than sending `cancel()` first, leaving the peer waiting for timeout.)
   - **Suggested fix:** Track all spawned verification subtasks in `Verifier` (e.g. a `JoinSet` aborted in `stop()`); increment a UI session generation on `Logout`/`NeedLogin` (or stay in a `logged_out` state until `LoggedIn`) so pre-`NeedLogin` / late session events cannot touch `self.verify` or open `Modal::Verify`.

---

### Low / Minor (Audit Items Checked)

6. **Cancel paths & unsanitized remote cancel reason** — `src/matrix/verify.rs:724-732, 787-804`, `src/app/verify.rs:89-96, 151-157`
   - `Decline`, `Cancel`, and `Esc` (`VerifyPopup::escape`) consistently send `Answer::Cancel`, record `self.verify.dismissed = flow`, clear `self.verify.flow`, and close the popup.
   - `Flow::answer` (`src/matrix/verify.rs:724-728`) handles `sas.is_done()` by falling through to `request.cancel().await`, and reports any failure from `cancel()` via `self.fail("cancel", err)` (`ActionFailed` toast). If the user cancelled right as verification completed/closed on the other side, reporting a late `"Could not cancel: ..."` toast after the user already dismissed the popup is noisy.
   - `cancel_reason` (`src/matrix/verify.rs:802`) passes `info.reason()` through without `sanitize_line(...)`, unlike `peer` and `device_label`; untrusted peer clients control `m.key.verification.cancel.reason`. Wrap `info.reason()` in `sanitize_line`.

7. **Settings row & status-line hint details** — `src/app.rs:397-402`, `src/ui/verify.rs:263-339`, `src/ui/status.rs:59-67`, `src/verify.rs:46-54`
   - **Status-line hint (`src/ui/verify.rs:263-339`):** Prioritizes hidden `Pending` verification requests ahead of `Unverified` and `NotSetUp`, shortens when `start + measure(&hint) + 20 > area.right()`, and safely clamps click hit rects (`end > start`). Verified in `verify_status_hint.snap`. At very narrow widths (`right - start < width(short)`), the short hint can still extend past `area.right()` and clip against the status bar border (no panic).
   - **Settings row (`src/app.rs:397-402, 443-448`, `src/ui/modal.rs:219-226`):** `Setting::Encryption` cleanly triggers `start_verification()`. Minor wording detail: when `security.identity == Identity::Missing`, `security.device` is `Unverified`, so the row shows `"not verified, Enter to verify"` even though pressing `Enter` opens `"Set up encryption"` (`PopupKind::SetUp`). Also in `start_verification()` (`src/app/verify.rs:46-58`), when `Identity::Present`, `Unverified`, `!other_devices`, and `RecoveryStatus::NotSetUp` (lost keys, no other devices), pressing `Enter` issues `VerifyCmd::Request` which immediately errors with `"Use your recovery key instead"` even though no recovery is set up.

8. **Key backup download setting** — `src/matrix/session.rs:51-61`
   - Verified `encryption_settings()` sets `auto_enable_cross_signing: false`, `backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure`, `auto_enable_backups: false` for both `discover_client` and `offline_client`, ensuring missing message keys are fetched from server backup automatically on decryption failure once recovery/verification grants access, without silently bootstrapping keys behind the user's back.

9. **Test coverage gaps**
   - `src/matrix/verify.rs` includes two `#[ignore]` live homeserver tests (`two_devices_verify_each_other`, `history_unlocks_after_verification`), `src/app/verify.rs` tests UI flows against a mocked `MatrixHandle`, and `src/ui/verify.rs` snapshots cover `Requested`, `Compare` (wide & narrow), `Done`, and the status hint cleanly.
   - Missing tests (highlighted by the bugs above): a real `Verifier` / SDK test exercising `Button::Retry` ("Ask again"), UIAA password cross-signing setup, other-user `in_room` requests + busy rejection, and truncated-height `Compare` rendering.

---

## Verdict

**approve with changes** — The core verification architecture, snapshot rendering, password redaction in logs/`Debug`, peer identity sanitization, and key-backup configuration are well-built, but the three High findings (missing UI password prompt for UIAA cross-signing set-up, `Ask again` self-cancellation race against `VerificationMachine`, and short-pane emoji clipping / accidental `Space`/`Enter` confirmation hazard) plus the logout subtask/event race should be resolved before pushing.
