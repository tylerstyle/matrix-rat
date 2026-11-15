# M20 Release Sign-Off Review — `v0.4.0` (`main` @ `5f20440`)

- **Target:** `main` @ `5f20440` (`v0.4.0`, compared against `v0.3.0` @ `f130045`)
- **Reviewer:** Antigravity
- **Date:** 2026-11-15

## Verdict

**Hold release sign-off — problems found.**

`.claude/fury-check.sh --server /home/tylerstyle/git/matrix-rat` passed `cargo clippy` (0 warnings), `cargo test` unit suite (`627 passed; 0 failed; 40 ignored`), and `cargo build`, **but failed 1 of 39 server tests** (`matrix::people::tests::own_profile_room_name_avatar_and_user_info_sync`), alongside new `0.4.0` `Debug`/logging privacy leaks in `devices`, `80×24` Settings popup layout regressions, and several `README.md` inaccuracies.

---

## Findings

### 1. [HIGH] Server test `own_profile_room_name_avatar_and_user_info_sync` failed on `5f20440` (`shared_rooms` race)
- **Where:** `src/matrix/people.rs:528-560` (`collect_shared_rooms`), `src/matrix/people.rs:479-485` (`context_room_info`), `src/matrix/rooms.rs:244-251` (`spawn_members_if_needed`), `src/matrix/people.rs:1089-1245` (`own_profile_room_name_avatar_and_user_info_sync`)
- **What happened:** In `.claude/fury-check.sh --server /home/tylerstyle/git/matrix-rat`, `matrix::people::tests::own_profile_room_name_avatar_and_user_info_sync` panicked at `src/matrix/people.rs:1239:9`:
  `assertion failed: profile.shared_rooms.iter().any(|r| r.room_id == room_id)`.
- **Why:**
  - After Bob joins `room_id`, the test polls `ManageCmd::Info` (`src/matrix/people.rs:1091-1110`), which now fetches `/members` directly over HTTP (`server_members` in `src/matrix/manage.rs`) without updating `matrix-sdk`'s local room member store (`room.get_member_no_sync`).
  - `OpenRoom` (`src/matrix/rooms.rs:244-251`) spawns `timeline.fetch_members()` asynchronously without awaiting completion before emitting `MatrixEvent::Timeline`.
  - When `MatrixCmd::FetchUserProfile` runs, `context_room_info` (`src/matrix/people.rs:481-483`) falls back to `room.get_member(target)` if `!room.are_members_synced()`, whereas `collect_shared_rooms` (`src/matrix/people.rs:535`) only checks `room.get_member_no_sync(target).await` and skips rooms whose members have not finished syncing into the SDK store.
- **Impact:** Both makes `own_profile_room_name_avatar_and_user_info_sync` racy (`38 passed; 1 failed` on `5f20440`) and causes User Info's shared-rooms list to miss rooms whose SDK member store is not yet synced.

### 2. [MEDIUM] Privacy gaps in new `0.4.0` `Debug` impls and default-level logs
- **`SessionsPopup` leaks `device_id` via `#[derive(Debug)]`:**
  - `src/devices.rs:243-253` derives `Debug` on `SessionsPopup`, printing `selected: Option<String>` (the highlighted session's `device_id`), and `Modal` (`src/modal.rs:883,914`) derives `Debug` around `Sessions(Box<SessionsPopup>)`. This conflicts with `Session::fmt`, `SignOut::fmt`, `Rename::fmt`, and `DevicesCmd::fmt`, all of which redact `device_id`.
- **`DevicesState` and `ServerBackup` expose private IDs, backup versions, and error strings in `Debug`:**
  - `src/matrix/devices.rs:96-109` derives `Debug` on `ServerBackup` (`Present { version, .. }` prints `version`, whereas `DevicesEvent::fmt` collapses it to `"present"`).
  - `src/devices.rs:39-54` derives `Debug` on `DevicesState`, exposing `retried: HashMap<String, UtdWhy>` (raw `$event_id` keys), `backup: ServerBackup`, and `list: Option<Result<SessionList, String>>` (raw error strings).
- **`SignOutOutcome::Failed` and `devices` `tracing::warn!` log raw SDK / HTTP / I/O error text at the default level:**
  - `src/matrix/devices.rs:198,345,359,380,397,683-684`: `SignOutOutcome::fmt` prints `Failed({err})` (also via `DevicesEvent::SignedOut`), and task handlers log `tracing::warn!(err, "sessions" | "key export" | "key import")` under the default `warn,matrix_rat=info` filter (`src/logging.rs:15-16`). By contrast, `src/matrix/manage.rs:1315-1317` (`failed`) avoids raw SDK error text (`tracing::warn!(kind = ?err.client_api_error_kind(), ...)`), noting *"The error's own text can name people, rooms and servers; its kind says enough."* (`debug!`-level logs at `src/matrix/devices.rs:550,607` and `src/matrix/rooms.rs:1187` also log `%err`.)
- **`UserSharedRoom::fmt` prints `room_id`:**
  - `src/matrix/people.rs:128-136` omits `name` but prints `.field("room_id", &self.room_id)` (unlike `OwnProfile::fmt` at `src/matrix/people.rs:72-80` and `UserProfile::fmt` at `159-170`, though `MatrixCmd`, `MatrixEvent`, `ManageCmd`, `ManageEvent`, `InfoPopup`, `ReportForm`, `PinnedPopup`, and `ReasonConfirmKind` intentionally log `room_id`).
- **Redaction test gaps:**
  - `src/matrix/devices.rs:1004-1080` sets `"PHONEDEVICE"` in inputs without checking `!"PHONEDEVICE"` in the assertions (`1069-1077`), and `src/devices.rs:1032-1060` does not test `SessionsPopup`, `KeysPopup`, or `DevicesState`.
  - `src/matrix/people.rs:784-785` does not check `!"!r1:example.org"` on `UserSharedRoom`.
- *(Note on pre-existing `v0.3.0` logging: `src/matrix/verify.rs:659,683` still log full `@user_id`s at `warn`/`info`.)*

### 3. [MEDIUM] Settings popup regressions at `80×24` (plus Log out confirmation truncation)
- **Where:** `src/ui/modal.rs:97-108, 168-374`, `src/app.rs:859-864, 3786-3824`
- **What:** `Setting::ALL` now has 19 settings across 4 headings (`21` lines with `Follow dark/light` off; `23` lines with it enabled). At `80×24`, `centered(area, 60, height)` clamps `rect` to `60×23`, giving `inner.height = 21` (`visible = 21`) and `airy = false`.
  1. **Setting hints disappear completely at `80×24`:** even with `Follow dark/light` off (`21` lines), rows fill all `21` lines of `inner` (`y == 21`), so `hint_top < inner.height` (`src/ui/modal.rs:359-360`) is `false` and `menu.current().hint()` is never shown.
  2. **Silent 2-row overflow when `Follow dark/light` is enabled:** `23` lines > `21` visible lines hides `Ignored users` and `Log out` below the viewport at initial scroll `0` with **no scroll indicator** and **no mouse-wheel scrolling** (`Modal::Settings` is omitted from `ScrollUp`/`ScrollDown` in `src/app.rs:3786-3824`). Keyboard `Up`/`Down`/`Tab`/`BackTab` still reveals every row cleanly and does not panic.
  3. **Log out confirmation truncated at all terminal sizes:** `"Enter again to log out, Esc to stay"` (`src/ui/modal.rs:341`, 35 chars) exceeds `room = 34` (`width = 60` fixed -> `56` inner cols minus `2` marker minus `LABEL = 20`), rendering as `"Enter again to log out, Esc to st…"`.
  4. **Minor style bug:** `src/ui/modal.rs:331` checks `if value == "none"` while `src/app.rs:860` returns `"None"`, so `Ignored users: None` renders in `Role::Text` instead of `Role::Muted`.

### 4. [LOW] `README.md` inaccuracies for `v0.4.0`
1. **`README.md:234` (`Action names -> Everywhere (global)`):** omits `withdraw_knock` (`src/keymap.rs:173`: `Action::WithdrawKnock => ("withdraw_knock", Context::Global, "Withdraw join request")`).
2. **`README.md:142`:** says `ban or unban users (Alt-B)` in Room info; in Room info, `Alt-B` only bans (`"This user is already banned."` when banned, `src/app/rooms.rs:1656-1665`), whereas **`Del`** unbans a banned member (`src/app/rooms.rs:1566-1578`, `src/ui/roominfo.rs:1439-1440`).
3. **`README.md:145`:** says `Click a sender's name or picture`, whereas timeline headers (`src/ui/timeline.rs:167-185`) draw `"● "` + bold sender name (`HitId::Sender`) and do not render sender pictures.
4. **`README.md:290-293` (`Good to know` on wiping secrets) vs `src/lineedit.rs:62-84`:**
   - Names `"Passwords and passphrases"` but omits **recovery keys** (`RecoveryForm.key`), and omits that `RecoveryForm` displays the recovery key unmasked (`masked = false` at `src/ui/modal.rs:605`), copying the visible text into `LineEdit::view` and the screen buffer.
   - Overstates `"pasted exactly as they are (spaces included)"`: spaces are preserved, but multi-line pastes keep only the first line (`src/lineedit.rs:374-377`) and `insert_secret` drops `unsafe_char` control/bidi characters (`src/lineedit.rs:260-278`).
   - Omits **crossterm's input parsing buffer** and **OS swap / core dumps** documented in `src/lineedit.rs:78-84`.

### 5. [LOW] Minor UI / Palette polish
- **`Addresses` footer clipping at `width = 74`:** `src/ui/roominfo.rs:2526` uses `"↑↓ list · Tab focus · m main · u unpublish · Del delete local · Esc back"` (73 chars, 75 with `popup`'s padding) in `centered(area, 74, height)` (`72` inner border cols), clipping `"↑↓ "` on the left.
- **`WithdrawKnock` duplicate palette entry if bound:** `src/palette.rs:402,471-477` omits `Action::WithdrawKnock` from `seen` and passes `hint: None`; binding `withdraw_knock` lists `"Withdraw join request"` twice when `ctx.in_knock` is true.

---

## Verified Clean
- **Versions:** `Cargo.toml:3` (`version = "0.4.0"`), `Cargo.lock:2856` (`0.4.0`), `default.nix:12` reads `Cargo.toml` dynamically, `src/main.rs:111` uses `env!("CARGO_PKG_VERSION")`; no stale `0.3.0` strings outside `Cargo.lock` history / docs.
- **`default.nix` & `Cargo.lock`:** `git diff v0.3.0..HEAD -- Cargo.toml default.nix shell.nix Cargo.lock` adds only `"zeroize"` to `matrix-rat` (`Cargo.toml:45`; `zeroize` was already present transitively in `Cargo.lock`). Zero new crates and zero new `-sys` crates; `default.nix` needs no new inputs.
- **Help overlay & Key-binding editor:** All 4 new `Action` variants (`WithdrawKnock`, `Devices`, `RetryDecryption`, `UserInfo` in `src/action.rs`) are coherently mapped in `src/keymap.rs` and `src/menu.rs`. Help switches cleanly to 1 scrollable column at `80×24` (tested in `every_key_in_the_help_can_be_reached` at `(80, 24)`), and Keybinds fits at `76×23` (`17` list rows).

## Check Output (`.claude/fury-check.sh --server /home/tylerstyle/git/matrix-rat`)
- `cargo clippy`: passed (`0` warnings, `29.29s`)
- `cargo test` (unit): `627 passed; 0 failed; 40 ignored; 0 measured; 0 filtered out; finished in 5.21s`
- `cargo build`: passed (`1m 14s`)
- Server suite (`--ignored --test-threads=1`): **`38 passed; 1 failed; 0 ignored; 0 measured; 628 filtered out; finished in 77.52s`** (`matrix::people::tests::own_profile_room_name_avatar_and_user_info_sync` failed at `src/matrix/people.rs:1239:9`)
