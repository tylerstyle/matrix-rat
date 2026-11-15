# Hand-over: `agy/msgtypes` (WIP to `matrix-claude`)

## Done (tested & passing)
- **Follow-up #0 (`shared_rooms_incomplete`):** `src/people.rs`, `src/matrix/people.rs`, `src/ui/people.rs` — `Shared rooms (N, maybe more)` when unsynced rooms are capped (`MAX_SHARED_ROOM_QUERIES = 12`) or fail/time out.
- **Location (`m.location`):** `src/rich.rs`, `src/matrix/convert.rs`, `src/matrix/notify.rs` — `📍` description + validated `geo:` link (`is_safe_geo_uri`), clickable / `o`-openable via `xdg-open`.
- **Voice, Audio & Video:** `src/model.rs`, `src/matrix/convert.rs`, `src/ui/timeline.rs`, `src/ui/image.rs`, `src/menu.rs`, `src/app/files.rs` — `MediaKind::{File, Voice, Audio, Video}`, duration (`M:SS`/`H:MM:SS`), MSC3245/MSC1767 waveform bars (`▁`..`█`), `▶` play in system player (`o`/Enter/click), `s` save; inline video thumbnail (`thumbnail_source` only) with `▶` play badge.
- **Polls (MSC3381 / `m.poll.*`):** `src/model.rs`, `src/matrix/convert.rs`, `src/matrix/notify.rs`, `src/matrix/mod.rs`, `src/matrix/rooms.rs`, `src/app/thread.rs`, `src/modal.rs`, `src/ui/poll.rs`, `src/ui/timeline.rs`, `src/app/poll.rs`, `src/action.rs`, `src/keymap.rs`, `src/complete.rs`, `src/palette.rs`, `src/menu.rs` — timeline rendering (disclosed vs undisclosed, single/multi, own vote, winner, ignored voters filtered), vote/change vote, end own poll, edit unvoted own poll, `/poll` & palette `CreatePoll` form (`LineEdit`). All 4 poll unit tests pass.

## Half Done
- **Stickers & MSC2545 packs:**
  - `ImageInfo::sticker` + bounded `STICKER_MAX_CELLS` `(18, 7)` in `src/images.rs` (`slot`), `is_kitty`/`avatar_ready`/`avatar_failed` + `layout_rev` bump in `on_avatar`.
  - `src/matrix/emotes.rs` (`EmoteCatalog`, `EmotePack`, `CustomEmoji`, `StickerItem`, `EmoteManager` lazy loader + sync invalidation, unit tests) + `MatrixCmd::{EnsureEmotePacks, SendSticker}` and `MatrixEvent::EmotePacks` wired into `src/matrix/mod.rs`, `src/matrix/rooms.rs`, `src/app/thread.rs`.

## Missing (Not Started)
- `src/rich.rs` `<img data-mx-emoticon>` parsing & inline Kitty 2x1 / `:shortcode:` rendering in `src/ui/timeline.rs` (messages + reactions).
- `:`-completion (`src/complete.rs`, `src/format.rs`) & `EmojiPicker` (`src/modal.rs`, `src/ui/modal.rs`) custom emoji sections + `StickerPicker` (`Modal::Stickers`, `Action::OpenStickers`).
- Server-backed tests in `src/matrix/rooms.rs` and `README.md` update.

## Known Compile Errors / Warnings
- None in `src/matrix/*`, `src/model.rs`, `src/ui/*`; only an exhaustive-match arm for the newly added `MatrixEvent::EmotePacks` in `App::on_matrix` (`src/app.rs`) if not yet matched.
