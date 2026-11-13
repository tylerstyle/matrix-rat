# Composer Completions Handover (branch `agy/completions`)

- **Worktree:** `/home/tylerstyle/git/matrix-rat/.claude/worktrees/agy-palette`
- **Branched from:** `f744596` (`main`)

## What Was Built

### 1. Trigger Detection, Matching & Mention Resolution (`src/complete.rs`)
- **Four triggers** evaluated at the composer's byte cursor (`Completion`, `Kind::{Mention, Emoji, Room, Command}`):
  - `@` mentions: room members plus `@room` when permitted (`can_mention_room`). Empty `@` query places recent timeline speakers first, remaining members alphabetically, and `@room` after regular members; non-empty queries rank with `nucleo-matcher` while keeping `@ro` etc. directly matchable and capping kept candidates at `MAX_CANDIDATES = 10` so large rooms stay bounded.
  - `:emoji:` (requires at least 2 query characters, rejects URLs and nested colons) via `crate::emoji::search`.
  - `#` joined rooms (`RoomTarget { id, name, alias }`) resolving to `matrix.to` URIs (`RoomAliasId::matrix_to_uri` when canonical alias parses, otherwise `RoomId::matrix_to_uri`).
  - `/` slash commands on line 0 (`COMMANDS`: `/me`, `/plain`, `/html`, `/spoiler`, `/rainbow`, `/shrug`, `/tableflip`, `/lenny`, `/upload`, `/file`).
- **Context guards**:
  - Code-span and fenced code-block masking (`code_mask`) prevents triggers inside inline code or fenced blocks.
  - Trigger boundaries avoid email-like mid-word `@`, URL schemes, and non-line-start `/`; byte range arithmetic uses valid UTF-8 boundaries (`Composer::cursor`, `Composer::replace_range` in `src/composer.rs`).
- **Mention tracking & extraction (`MentionMark`, `resolve_marks`, `marks_from_message`)**:
  - Accepted `@user`, `@room`, and `#room` completions record `MentionMark` alongside composer text without inserting raw Matrix MXIDs or URLs into the composer.
  - `resolve_marks` maps matching occurrences outside code spans/blocks into Markdown links (`https://matrix.to/#/...`) and constructs `ruma::events::Mentions` (`user_ids` + `room`).
  - `marks_from_message` recovers `User` and `RoomNotification` marks from existing formatted HTML (skipping `<mx-reply>`) when starting an edit (`ComposeMode::Edit`).

### 2. Message Formatting & Matrix Protocol (`src/format.rs`, `src/model.rs`, `src/matrix/*`)
- **`src/format.rs`**:
  - Added `mentions: Option<Mentions>` to `Outgoing::Text` and `Outgoing::Emote` (with manual `PartialEq`/`Eq`).
  - Added `parse_with_mentions(input, marks, preserve_empty_mentions)` (keeping `#[cfg(test)] pub fn parse` for existing test callers). Supports Markdown messages, `/me`, `/plain`, `/html`, `/spoiler`, `/rainbow`, `/shrug`, `/tableflip`, `/lenny`, and `//`, attaching `m.mentions` on normal sends and `Some(Mentions::new())` when an edit removes prior mentions (`edit_had_mentions`).
- **`src/model.rs`, `src/matrix/mod.rs`, `src/matrix/convert.rs`, `src/matrix/upload.rs`**:
  - Added `Message.mentions` (populated in `convert.rs`) and `Room.alias` / `RoomSummary.alias` (sanitized via `sanitize_line`).
  - Appended `MatrixCmd::FetchMembers { room_id }` and `MatrixEvent::RoomMembers { room_id, members, can_mention_room }` at the end of those enums, with privacy-safe `Debug` formatting that omits member display names and user IDs.
  - Threaded `mentions` through `src/matrix/upload.rs` (`caption_content` -> `AttachmentConfig.mentions`) for captioned uploads.
- **`src/matrix/rooms.rs`**:
  - Implemented `MatrixCmd::FetchMembers` handler (`load_room_members` in `member_fetches` `JoinSet`, capped at `MAX_COMPLETION_MEMBERS = 50_000`, using `members_no_sync` when `are_members_synced()` else `members`, computing `can_mention_room` via `power_levels().user_can_trigger_room_notification(&own)`, and sanitizing strings).
  - Attached `mentions` to outgoing text/emote room events and edits (`apply_mentions` in `content` and `content_without_relation`).

### 3. App Integration & Member Fetch Lifecycle (`src/app/complete.rs`, `src/app.rs`, `src/app/attach.rs`, `src/app/thread.rs`)
- **`CompleteState` (`src/app/complete.rs`)**:
  - `request_members_for_open_room` dispatches `MatrixCmd::FetchMembers` at most once per room open and never for invitations or keystrokes.
  - Saves and restores `MentionMark` per room draft (`draft_marks`), per thread (`thread_marks`), and across edits (`edit_stash_marks`, `edit_had_mentions`).
  - Key handling runs before multiline `Up`/`Down` and `attach_key`: when closed, navigation/composer keys behave identically; when open, `Up`/`Down`/`PageUp`/`PageDown` move selection, `Tab`/`Enter` accept (`accept_completion`), and `Esc` closes only the popup (`dismissed_start`) without discarding edits/replies or quitting threads.
  - Preserves existing Tab path completion for `/upload` and `/file` (`src/app/attach.rs`).
- **Tests (`src/app/complete.rs`)**:
  - `no_popup_keys_behave_as_before`
  - `popup_lifecycle_tab_enter_esc_and_mouse_click`
  - `emoji_room_and_slash_completions_work_and_keep_upload_path_completion`
  - `edit_and_thread_preserve_and_update_mentions`
  - `member_fetch_runs_at_most_once_per_room_open`

### 4. Popup UI & Hit Targets (`src/ui/complete.rs`, `src/ui/composer.rs`, `src/ui/hit.rs`, `src/ui/mod.rs`)
- Appended `HitId::Complete(usize)` at the end of `HitId` (`src/ui/hit.rs`) and stored screen cursor coordinates in `ComposerView.cursor` (`src/ui/composer.rs`).
- `complete::draw` (`src/ui/complete.rs`) renders up to `MAX_SHOWN = 8` rows above the composer's top border when typing on row 0 (keeping the composer border title intact) or above the cursor row on multiline drafts, clamps cleanly to narrow/short viewports, registers `HitId::Overlay` + `HitId::Complete(idx)`, and supports hover + left click acceptance.
- Added snapshot `composer_completions` (`src/ui/snapshots/matrix_rat__ui__tests__composer_completions.snap`) and extended `tiny_sizes_do_not_panic` with an active completion popup.

## Verification
- `./scripts/check.sh`:
  - `cargo fmt --check` — passed
  - `cargo clippy --all-targets -- -D warnings` — passed
  - `cargo test` — passed (`256 passed; 0 failed; 12 ignored; 0 measured; 0 filtered out`)
  - `cargo build` — passed
- Live local Continuwuity homeserver test (`cargo test alice_mentioning_bob_delivers_m_mentions -- --ignored --nocapture` in `src/matrix/rooms.rs`):
  - `1 passed; 0 failed; 0 ignored; 0 measured; 267 filtered out` (Alice logs in, fetches `RoomMembers`, sends a mention using `parse_with_mentions`, and Bob receives both `matrix.to` formatted HTML and `m.mentions.user_ids` containing Bob's user ID).
