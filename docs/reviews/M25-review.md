# M25 Review — Room Slash Commands & Message Details (`commands`)

Reviewed `main...commands` (`b2f4334..6c7a9f7`).

## Verdict
**Approved with 2 minor notes** (non-blocking, easy 2-line polish if desired).

## Checklist Verification

1. **Confirmations & parity with menu/popup twins (`src/app/commands.rs`):**
   - `/leave`: opens `Modal::RoomInfo` on `InfoRow::Leave` with `info.confirm = Some(Confirm::Leave)`, requiring `Enter` to confirm.
   - `/kick`, `/ban`, `/unban`: check `can_kick_user`, `can_ban_user`, `can_unban_user`, refuse self-targeting, and open `Modal::ReasonConfirm` (`ReasonConfirmPopup::{kick,ban,unban}`) displaying `person.label_with_id()` (`Name (@user:server)`) with any trailing reason pre-filled.
   - `/ignore`, `/unignore`: refuse self and already-matching state, then call `open_user_info_at(..., UserAction::IgnoreToggle)` which opens `Modal::UserInfo` with `Confirm::Ignore` / `Confirm::Unignore` armed and waiting for `Enter`.
   - `/join`: opens the joined room or `open_room_preview(&target, via)` (never auto-joins an unknown room without the preview popup).
   - `/invite`, `/dm` (`/msg`): populate `Modal::New` (`Stage::Invite` / `Stage::Direct`) and call `submit_new()`.
   - `/nick`, `/roomnick`, `/topic`, `/roomname`: share `checked_display_name` and `room_text_change` (checking `can_set_topic` and `can_rename` first).

2. **Resolving a person from a name (`resolve_user` in `src/app/commands.rs:165-255`):**
   - Accepted `@` completion marks at `arg_start` resolve directly to their `user_id`.
   - Explicit MXIDs (`@user:server`) and `matrix:` / `http(s)://` links are parsed first via `parse_user` before any member-name lookup, so a member whose display name is `@victim:server` cannot hijack an explicit MXID.
   - Bare names match case-insensitively against `member.display_name` or `fallback_localpart(&member.user_id)` across all `known_members()` (including ignored members, so `/unignore`, `/kick`, `/ban` work on them and an ignored member with a colliding name still triggers ambiguity refusal).
   - When `with_rest` is true (`/kick`, `/ban`, `/unban`), all word prefixes up to `MAX_NAME_WORDS = 8` are checked; if two different members match different prefix lengths (e.g. `"Bob"` and `"Bob Builder"`), `found.len() > 1` refuses the command and asks for a full `@name:server` ID.
   - Because `/kick`, `/ban`, `/unban`, `/ignore`, `/unignore` all stop at a confirmation modal that shows the full `@user:server` ID, a homoglyph or display-name spoof in a room where the real user is absent is visible before confirmation.

3. **`/leave` with arguments (`src/app/commands.rs:270-277`):**
   - `/leave <anything>` is rejected with `"/leave takes nothing after it: it leaves the open room"` and keeps the composer draft intact.

4. **Message details & event source popup (`src/matrix/source.rs`, `src/details.rs`, `src/app/details.rs`, `src/ui/details.rs`):**
   - **Size caps:** `SOURCE_MAX = 128 * 1024` (128 KB) per JSON document (`original_json` and `latest_edit_json`); inputs over `SOURCE_MAX` skip `serde_json` parsing and are truncated on a char boundary with `cut = true`.
   - **Sanitising for the terminal:** `source::pretty` escapes every char matching `crate::text::unsafe_char(ch) && ch != '\n'` (control chars, ESC, DEL, C1 controls, bidi overrides/isolates, zero-width format chars) as `\uXXXX` and passes the result through `crate::text::sanitize`. All fact fields (`sender`, `sender_id`, `sent`, `event_id`, `readers`, error strings) pass through `sanitize_line`.
   - **Clipboard (`copy_details`):** copies `event_id` on `DetailsView::Facts` and `source_json()` (the already-sanitized, size-capped `json` + `edit_json`) on `DetailsView::Source`.
   - **Privacy of `Debug`/logs:** `EventSource`, `DetailsPopup`, `MatrixCmd::FetchSource`, and `MatrixEvent::Source` have hand-written `Debug` impls that log only request numbers, byte lengths, and booleans—decrypted event content never reaches logs. Thread root routing in `src/app/thread.rs:399-428` is also handled properly.

5. **New `can_kick` / `can_unban` and `can_rename` / `can_set_topic` rights (`src/model.rs:674-725`, `src/matrix/manage.rs:1653-1657`):**
   - `can_kick_user` and `can_unban_user` enforce `self.can_kick` / `self.can_unban`, `own_user_id != Some(target_user_id)`, and `self.outranks(target_user_id)` (which uses `pl.users` + `users_default`, so it also works for banned/non-joined targets).

## Minor Notes

1. **`/help <extra>` dead `usage()` arm (`src/app/commands.rs:71, 270-274`):**
   `RoomCommand::usage` defines `Self::Help => return "/help takes nothing after it".into()`, but line 273 checks `let extra = typed.command == RoomCommand::Leave && !typed.arg.is_empty();`. Using `let extra = bare && !typed.arg.is_empty();` makes `/help foo` show that usage toast instead of ignoring the argument.
2. **`/invite` when no joined room is open (`src/app/commands.rs:306-320`):**
   If `/invite <user>` is run when no room is open or the active room is pending (`Invite`/`Knock`), `open_new_with(Stage::Invite, ...)` shows a toast and returns without opening the modal, `submit_new()` is a no-op, and `run_room_command` returns `Ok(())`, which clears the composer draft. Calling `if typed.command == RoomCommand::Invite { self.open_room_or_say()?; }` before `self.named(typed)` preserves the draft on error like `/kick` and `/topic`.
