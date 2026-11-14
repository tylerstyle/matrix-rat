# M17 Review — Direct-Chat Verification Step Waker (`verify-flake`)

- **Branch / Commit:** `verify-flake` @ `e797e65` (`+230 / -4`, only [`src/matrix/verify.rs`](../../src/matrix/verify.rs))
- **Base:** `main` @ `fcd46d6`
- **Security Relevance:** High (gates `0.3.0`; touches E2EE verification state progression and to-device HTTP traffic)
- **Reviewer:** Antigravity
- **Date:** 2026-11-14

## Verdict

**APPROVED (ready for merge into `0.3.0`).**

The root-cause diagnosis is verified verbatim against `matrix-sdk 0.19.1`, `matrix-sdk-ui 0.19.1`, `matrix-sdk-crypto 0.19.1`, and `matrix-sdk-base 0.19.1`. There is no cleaner public API in `matrix-sdk 0.19.1` to flush `OlmMachine`'s outgoing request queue under `SyncService`, and waking the `encryption` sliding-sync loop by sending an empty, plaintext `m.dummy` event exclusively to `(own_user_id, own_device_id)` is safe, self-limiting, and race-free.

There are **no blockers** (`[HIGH]` / `[CRITICAL]` / `[MEDIUM]` findings: 0). Below are two `[LOW]` hardening notes, two `[Note]` observations, and the verified post-`0.3.0` follow-up on room-key requests.

---

## Findings

### [LOW] `age_ms` uses `saturating_sub`, treating future-skewed timestamps as fresh
- **Where:** `src/matrix/verify.rs:227-229` (`verify-flake` `e797e65`)
- **What:** `age_ms` computes `u64::from(MilliSecondsSinceUnixEpoch::now().get()).saturating_sub(sent.get().into())`.
  - If the local clock lags behind the homeserver's `origin_server_ts` (even by more than 5 minutes), `now < sent` saturates to `0 < FRESH_MS`, so older room history falling ahead of the local clock passes `in_room_step`. By comparison, `OlmMachine` (`VerificationMachine::is_timestamp_valid` in `matrix-sdk-crypto-0.19.1/src/verification/machine.rs:210-220`) enforces a symmetric freshness bound (`> 10 min` old OR `> 5 min` in the future).
  - Conversely, if the local clock is ahead by `>= 5 min`, `age_ms >= FRESH_MS` disables fast wakes and falls back to the 30 s `encryption` poll.
- **Impact:** Negligible in practice: worst case on a lagging clock is one harmless coalesced `{}` `m.dummy` sent to our own device during an initial/gapped sync; worst case on a fast clock is graceful degradation back to pre-patch timing. Fine as-is for `0.3.0`.

### [LOW] `Verifier` lacks a `Drop` guard for `self.handlers`
- **Where:** `src/matrix/verify.rs:360-428`, `577-596` (`verify-flake` `e797e65`), [`src/matrix/mod.rs`](../../src/matrix/mod.rs#L1204), [`src/matrix/mod.rs`](../../src/matrix/mod.rs#L1361)
- **What:** Unlike other session-scoped workers in `src/matrix/` (`Watch` in `src/matrix/notify.rs:71`, `IgnoreTracker` in `src/matrix/safety.rs:139`, `UserSearcher` in `src/matrix/safety.rs:322`, `Searcher` in `src/matrix/search.rs:148`), `Verifier` relies solely on an explicit `verifier.stop().await` call to remove its four `EventHandlerHandle`s (`from_device`, `in_room`, `steps`, `secrets`) from `self.client`. Dropping `Verifier` directly cancels `self.tasks` (`JoinSet`), but leaves those four handlers registered on `client` until `client` itself is dropped.
- **Impact:** Safe on every current path because `Worker::run_session` (`src/matrix/mod.rs:1106-1369`) is the sole caller of `Verifier::start`, has no early `?` returns between line `1204` (`Verifier::start`) and line `1361` (`verifier.stop().await`), and `run_active_session` (`src/matrix/mod.rs:913-975`) drops `active` (`Client`) right after. Adding `impl Drop` (removing `self.handlers.drain(..)`) is trivial post-`0.3.0` defensive hygiene.

### [Note] `in_room_step` does not filter by participant / transaction relation
- **Where:** `src/matrix/verify.rs:265-279` (`verify-flake` `e797e65`)
- **What:** `in_room_step` checks only `event.event_type()`, `Some(event.sender()) != own`, and `age_ms < FRESH_MS`. If two *other* users verify each other in a shared multi-person room, their live `m.key.verification.*` events wake `send_queued` on our client too. Also, `KeyVerificationDone` itself triggers a wake even when our side already sent its answer on `KeyVerificationMac`.
- **Impact:** Benign: the only side effect is sending `{}` (`m.dummy`) to `(own_user_id, own_device_id)`, coalesced by `Notify::notify_one()`. Keeping `in_room_step` synchronous and stateless avoids async crypto-store lookups on every message event.

### [Note] Minor doc-comment detail in `verify.rs`
- **Where:** `src/matrix/verify.rs:317-318` (`verify-flake` `e797e65`)
- **What:** The comment notes there is no public way to flush the queue `"short of SyncService::expire_sessions, which starts both syncs over"`. `SyncService::stop()` + `start()` (`matrix-sdk-ui-0.19.1/src/sync_service.rs:657-669`) differs slightly from `expire_sessions()` in that it does not wipe Sliding Sync session/`pos` tokens, **but** it still tears down and restarts both `RoomListService` and `EncryptionSyncService`, cancels active requests, and flips `SyncService::state()` (`Idle`/`Running`) — so it is equally unsuitable as a per-step flusher.

---

## Detailed Review Against the Five Questions

### 1. Root-Cause Diagnosis & SDK Public-API Alternatives
The diagnosis in the commit message and `send_queued` doc comment is exact:
- **Queuing in `matrix-sdk-crypto 0.19.1`:** When in-room verification steps (`ready`, `start`, `accept`, `key`, `mac`, `done`, `cancel`) arrive, `VerificationMachine::receive_any_event` (`verification/machine.rs:336-388`) queues automated replies (`VerificationMachine::queue_up_content` / `add_request`) inside `OlmMachine` rather than sending HTTP requests inline. Those queued verification requests sit in the store until `OlmMachine::outgoing_requests()` (`machine/mod.rs:655-691`) is drained (`verification_machine.outgoing_messages()`).
- **No public flush API:** `Client::send_outgoing_requests(&self)` (`matrix-sdk-0.19.1/src/encryption/mod.rs:857`) is `pub(crate)`. Across the entire `matrix-sdk 0.19.1` crate, its only non-test callers are `Client::sync_once` (`client/mod.rs:3193, 3222`) and `SlidingSync::send_sync_request` (`sliding_sync/mod.rs:633`, guarded by `if self.is_e2ee_enabled()`). `Client::olm_machine_for_testing` (`encryption/mod.rs:1017-1022`) is gated behind `#[cfg(any(feature = "testing", test))]`.
- **Two-connection `SyncService` split:** `SyncServiceBuilder::build` builds `EncryptionSyncService::new(client, None)` (`matrix-sdk-ui-0.19.1/src/sync_service.rs:869`), which creates the `"encryption"` `SlidingSync` with `.with_to_device_extension(enabled: true)` and `.with_e2ee_extension(enabled: true)` (`encryption_sync_service.rs:74-95`) and uses `SlidingSyncBuilder`'s default 30 s `poll_timeout` (`sliding_sync/builder.rs:54`). The `"room-list"` `SlidingSync` has `is_e2ee_enabled() == false`, so it delivers room verification events via `/sync` without ever calling `send_outgoing_requests()`.
- **Why `EncryptionSyncService` cannot be poked directly:** `SyncService` exposes only `start()`, `stop()`, `expire_sessions()`, `try_get_encryption_sync_permit()`, `room_list_service()`, and `state()` (`sync_service.rs:606-697`); `EncryptionSyncService` keeps its `SlidingSync` field private (`encryption_sync_service.rs:65-68`).
- **Why not `Client::sync_once` or re-implementing `send_outgoing_requests`?** Running legacy v3 `/sync` via `Client::sync_once` concurrently with `SyncService`'s Sliding Sync runs duplicate sync loops against the same store. Sending verification requests manually outside `OlmMachine::outgoing_requests()` via another mechanism isn't possible publicly without double-sending or corrupting sent-request bookkeeping. Letting `encryption` `SlidingSync` remain the sole drainer via an `m.dummy` wake guarantees every request goes out once through `send_outgoing_requests()`.

### 2. Safety of Unencrypted `m.dummy` to Own Device
- **SDK handling (`OlmMachine`):** In `matrix-sdk-crypto-0.19.1/src/machine/mod.rs:1601-1704`, `receive_to_device_event` passes plaintext `ToDeviceEvents::Dummy` (`m.dummy`) to `handle_to_device_event`, which matches `Custom(_) | Dummy(_) => {}`. Plaintext `m.dummy` is a complete no-op: it does **not** touch Olm sessions, advance ratchets, or trigger Olm session unwedging (unwedging only runs on encrypted `m.dummy` inside `handle_decrypted_to_device_event`, `machine/mod.rs:1541`).
- **Event handlers:** `SlidingSyncResponseProcessor::handle_encryption` (`sliding_sync/client.rs:176-204`) forwards `to_device` events through `handle_sync_to_device_events` (`event_handler/mod.rs:370-399`), which only dispatches `HandlerKind::ToDevice` listeners registered for `"m.dummy"`. `matrix-rat` registers none.
- **Matrix spec / Ruma / Homeservers (`PUT /_matrix/client/v3/sendToDevice/{eventType}/{txnId}`):** Defined in `ruma-client-api-0.25.2/src/to_device/send_event_to_device.rs` (`rate_limited: false` in Ruma metadata) with body `{ messages: { user_id: { device_id: content } } }`. Homeservers like Synapse or Continuwuity/Conduwuit (`[global.rate_limiting]`) may still apply configurable endpoint/global rate limits (`429`). `send_queued` handles this safely: `.disable_retry().timeout(Duration::from_secs(10))`, logging a non-fatal `tracing::warn!` on `Err` and falling back to the 30 s `encryption` poll. `Arc<Notify>` (`notify_one()`) stores at most 1 permit while `send_queued` is in flight, throttling rapid bursts to at most 1 `sendToDevice` request at a time.
- **Privacy & targeting:** Messages map targets strictly `(user.to_owned(), DeviceIdOrAllDevices::DeviceId(device.to_owned()))` with `"{}"` content. Nothing is sent to `AllDevices`, other own devices, or peer users; traffic stays solely between the client and its own homeserver with zero payload beyond timing.
- **Order-independence:** If `encryption` sync is blocked in its long poll when `m.dummy` arrives, the server returns the `to_device` response immediately and the next `send_sync_request()` flushes `OlmMachine::outgoing_requests()`. If `encryption` sync is between requests / backing off when `m.dummy` arrives, the server buffers `{}` until the next `send_sync_request()` starts (which already drains `outgoing_requests()` first, and the server immediately delivers and clears `m.dummy`). Note that `steps` runs after `process_sliding_sync`, so `OlmMachine`'s answer is always already queued before `m.dummy` is sent.

### 3. Completeness & Edge Cases of `in_room_step`
- **Event types:** `in_room_step` matches all 7 post-request verification `MessageLikeEventType` variants in Ruma `0.35.2` (`ruma-events-0.35.2/src/enums.rs:489-495`): `KeyVerificationReady`, `KeyVerificationStart`, `KeyVerificationAccept`, `KeyVerificationKey`, `KeyVerificationMac`, `KeyVerificationDone`, `KeyVerificationCancel`. `m.room.message` (`msgtype: m.key.verification.request`) intentionally does not wake: `VerificationMachine::receive_any_event` (`machine.rs:336-388`) only inserts the request and queues nothing until the user acts (`accept` / `cancel`). QR verification uses these same event types (note `Flow` in `src/matrix/verify.rs:889-904, 962-965` currently negotiates only `SasV1`).
- **Encrypted DM rooms work:** Verified in `matrix-sdk-base-0.19.1/src/response_processors/timeline.rs` + `e2ee/decrypt.rs:32-61` + `matrix-sdk-common-0.19.1/src/deserialized_responses.rs:641-662` + `matrix-sdk-0.19.1/src/sync.rs:218, 240`: encrypted room events are decrypted *before* `verification::process_if_relevant` and event handlers run, and `event.to_decrypted(...)` replaces `raw` with the inner decrypted JSON event. Thus `AnySyncMessageLikeEvent` matches the decrypted `m.key.verification.*` event in encrypted DMs (confirmed by `other_people_verify_in_a_direct_chat`, whose test room uses `RoomEncryptionEventContent::with_recommended_defaults()`).
- **Own-sender filter (`Some(event.sender()) != own`):** Self-verification uses to-device messaging (`identities/users.rs:144-152`, delivered directly via `encryption` sync), and `VerificationMachine::receive_any_event` (`machine.rs:326-332`) ignores any in-room verification event whose sender equals our own user ID (`event_sent_from_us`).
- **History / backfill:** `handle_sync_timeline_events` is invoked solely from `matrix-sdk-0.19.1/src/sync.rs:218, 240` on `/sync` responses — never from `EventCache` / `/messages` pagination. Scrolling back through room history does not run `steps`. Only live sync or initial/gapped sync events less than 5 minutes old pass `FRESH_MS`.

### 4. Lifecycle (`Verifier::start` / `stop` / `run_session` / Re-login)
- `Verifier::start` (`src/matrix/verify.rs:360-428`) registers the `steps` handler in `self.handlers` and spawns `send_queued(client.clone(), wake)` in `self.tasks` (`JoinSet`). `steps` captures only `OwnedUserId` and `Arc<Notify>` (no `Client`, no strong `Verifier` cycle).
- `Verifier::stop` (`src/matrix/verify.rs:577-596`) drains and removes all `self.handlers`, stops any active verification flow (`2s` grace + abort), and runs `self.tasks.shutdown().await`. Even if `send_queued` is awaiting its 10 s `SendToDevice` request when shutdown or logout occurs, `JoinSet::shutdown()` aborts it immediately without delaying teardown.
- `Worker::run_session` (`src/matrix/mod.rs:1106-1369`) always reaches `verifier.stop().await` (`line 1361`) before returning on `Shutdown`, `LoggedOut`, `SoftLogout`, or `Expired`, and `run_active_session` (`src/matrix/mod.rs:913-975`) drops `active` cleanly across all re-login / expiry paths.

### 5. Side Finding: Delayed Automatic `m.room_key_request`s (`UnableToDecrypt`) & `0.3.0` Assessment
- Verified against `Cargo.toml` and `matrix-sdk-0.19.1`: `matrix-rat` enables `matrix-sdk` default features, which includes `automatic-room-key-forwarding`.
- In `OlmMachine::decrypt_room_event_inner` (`matrix-sdk-crypto-0.19.1/src/machine/mod.rs:2505-2518`), decryption failures (`MissingRoomKey` / `UnknownMessageIndex`) call `key_request_machine.create_outgoing_key_request(room_id, &event)` (`gossiping/machine.rs:876-930`) to persist an outgoing `m.room_key_request` rather than sending it directly. `OlmMachine::outgoing_requests()` (`machine/mod.rs:655-691`) drains `key_request_machine.outgoing_to_device_requests()` (covering `m.room_key_request`, `m.forwarded_room_key`, and `KeysClaimRequest` at `gossiping/machine.rs:175-206`).
- Because `room-list` `SlidingSync` does not run `send_outgoing_requests()`, automatic `m.room_key_request`s (both from live sync UTDs and from `EventCache` / `redecryptor.rs:635` calling `decrypt_room_event`) wait up to 30 s for `EncryptionSyncService`'s next request before leaving the device. (By contrast, *sending* messages calls `Room::send` -> `preshare_room_key` at `matrix-sdk-0.19.1/src/room/mod.rs:2406-2461`, which calls `share_room_key` directly and is unaffected.)
- **Assessment for `0.3.0`:** Leave unaddressed in `M17` / `0.3.0` and track as a post-`0.3.0` improvement. Waking blindly on every `m.room.encrypted` / `UnableToDecrypt` event during room loads or historical backfill with missing keys could generate repeated futile `m.dummy` polls unless carefully bounded (freshness + per-session/per-room debouncing + distinguishing genuinely new UTDs from unrecoverable history). Keeping M17 scoped strictly to `m.key.verification.*` steps avoids that risk right before release.

---

## Checks Run

All verification commands ran remotely on `hpfury` against commit `e797e65` (`agent-a70b28a9f609e6723` worktree) and the local `matrix-continuwuity` dev homeserver via `.claude/fury-check.sh`:

1. **Full check + unit + server suite (`fury-check.sh --server`, Run 1):**
   - `cargo fmt --check`: **ok**
   - `cargo clippy --all-targets -- -D warnings`: **ok** (`3.11s`)
   - `cargo test` (unit suite): **564 passed; 0 failed; 30 ignored; 0 measured; 565 filtered out** (`5.10s`)
   - `cargo build`: **ok** (`12.57s`)
   - Server suite (`cargo test matrix:: -- --ignored --test-threads=1`): **29 passed; 0 failed; 0 ignored; 565 filtered out** (`70.84s`), including all 4 `matrix::verify::tests`.
2. **Server suite (`fury-check.sh --only-server`, Run 2):**
   - **28 passed; 1 failed** (`70.94s`).
   - All 4 `matrix::verify::tests` (`asking_again_works`, `history_unlocks_after_verification`, `other_people_verify_in_a_direct_chat`, `two_devices_verify_each_other`) **passed**.
   - Single failure (unrelated room-creation race): `matrix::manage::tests::a_room_from_creation_to_forgetting` panicked at `src/matrix/manage.rs:1810:9` (`left: None, right: Some("#matrix-rat-manage-2156500:localhost")`).
3. **Server suite (`fury-check.sh --only-server`, Run 3):**
   - **29 passed; 0 failed; 0 ignored; 565 filtered out** (`70.99s`).
4. **Targeted verification stress (`3x cargo test matrix::verify::tests -- --ignored --nocapture --test-threads=1` with `dev-homeserver-clean.sh`):**
   - Verify run 1: **4 passed; 0 failed** (`32.73s`) — `other_people_verify_in_a_direct_chat`: `U done after 2.523554303s`, `V done after 3.526553711s` *(note: `two_devices_verify_each_other` emitted a benign background teardown panic on `tokio-rt-worker` in `matrix-sdk-sqlite-0.19.1/src/utils.rs:687:68: Cancelled`, and exited `ok`)*.
   - Verify run 2: **4 passed; 0 failed** (`32.04s`) — `other_people_verify_in_a_direct_chat`: `U done after 2.117938298s`, `V done after 3.116505333s`.
   - Verify run 3: **4 passed; 0 failed** (`32.00s`) — `other_people_verify_in_a_direct_chat`: `U done after 2.109782204s`, `V done after 3.093896152s`.

Across all 6 server runs (`3` full suites + `3` targeted verification runs), all 4 `matrix::verify::tests` passed 6/6 times, with `other_people_verify_in_a_direct_chat` consistently completing in ~2.1–3.5 s (well inside the 15 s / 10 s bounds).
