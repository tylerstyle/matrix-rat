# matrix-rat 1.1.0 (draft)

A draft of the release notes for 1.1.0, in the form of the GitHub releases of 1.0.x, kept
up to date while 1.1.0 is built. So far: the move to matrix-sdk 0.20.

matrix-rat now builds on matrix-sdk 0.20: search lists the newest matches first, a
handful of fixes from the SDK, and the SQLite in the release archives is current again.

## Before you upgrade

- **No way back to 1.0.x with the same profile.** On its first start 1.1.0 migrates the
  profile's stores to what matrix-sdk 0.20 expects. A profile opened with 1.1.0 should not
  be opened with 1.0.x again. Room keys that 1.1.0 exports to a file or backs up to the
  server are read by 1.0.x as keys that may not be shared with others (matrix-sdk 0.20
  writes that mark under a new name). If you may want to go back, copy the profile's
  folder (`~/.local/share/matrix-rat/<profile>/`, see **Several accounts** in the README)
  before the upgrade, while matrix-rat and `matrix-rat serve` are not running on it.
- **The first start rebuilds two local caches.** The local copy of the rooms' messages is
  emptied once by its migration and fetched from the server again as rooms are synced and
  opened. The search index is rebuilt: matrix-sdk 0.20 no longer stretches its key (which
  made opening each room's index slow; the profile's key is random and needs no
  stretching), so the old index cannot be opened and is deleted. Until messages are loaded
  again, search finds fewer of them.

## Changed

- **Search across all rooms (`Ctrl-G`) lists the newest matches first**, instead of the
  best matches first: matrix-sdk 0.20 sorts by time and no longer gives a relevance score.
  With more than 200 matches the newest 200 are shown ("Showing the newest 200 matches —
  refine your search").
- **A malformed encrypted message says so**: "Unable to decrypt: the message is
  malformed" (a damaged message, or one whose sender does not match its keys), where it
  said that its key had not arrived. No key will make it readable.
- **The room list orders rooms without messages by their activity** among the others,
  instead of after all rooms with messages.

## Fixed

Through matrix-sdk 0.20:

- **A batch of messages twice in a timeline.** A timeline opened while new messages came
  in could hold them twice (matrix-sdk-ui 0.19.1); fixed in the SDK, which also no longer
  sends a timeline's updates out of order when sync, sending and loading older messages
  meet. matrix-rat's own guard against repeated messages (1.0.0, 1.0.3) stays.
- **No sign-out on a failed token refresh.** With a password login, a timeout or a server
  error while the login was renewed signed you out; now only a renewal the server refuses
  does.
- **Emoji verification no longer stalls between its steps**: each step could wait for
  the next sync to go out, up to the length of a waiting sync request.
- **Messages that stayed locked though their keys had arrived** (when many came in at
  once) are decrypted.
- **After you kick or ban someone**, the room's encryption keys are replaced at once, so
  the next message is not readable with the keys they had.
- **Unread counts** are right at once after older messages were loaded from the local
  store, where they could stay stale until the next sync.
- **The read marker** shows when the last message read is not shown itself (a reaction,
  an edit): after the nearest message before it.
- **The quote of a deleted message** keeps its own sender and time, not those of whoever
  deleted it.
- **A reaction you had already given from another device** no longer blocks the room's
  later messages: the server refuses the second copy, which stayed as failed and held back
  everything sent after it; it is dropped now.
- **Search** no longer misses messages when many rooms are updated at once (a large sync).
- **A crash in the message cache** after it shrank a room's stored messages ("Inserting
  new chunk with no previous nor next chunk identifiers is impossible").
- **Leaving and forgetting a room** no longer empties the message cache of the other
  rooms.
- **Rate limits**: a server's "try again at" time is respected; creating a room is no
  longer tried again after a network error (which could make two rooms).
- The encrypted search index's files are written to disk at once (they could be lost on a
  crash of the system).

## Security

- **SQLite 3.54.1** in the release archives
  ([RUSTSEC-2026-0459](https://rustsec.org/advisories/RUSTSEC-2026-0459)): SQLite before
  3.54.0 could hand out stale memory when it opened a database whose `-wal` file has
  another page size. It took someone who can write into the profile's private folder, who
  could read the store there anyway. matrix-sdk 0.20 takes rusqlite 0.41, which bundles
  the fixed SQLite. The Nix package links the system's SQLite, as before.

## Dependencies

- matrix-sdk, matrix-sdk-base and matrix-sdk-ui 0.20.0 (from 0.19.1), pinned exactly as
  before, with ruma 0.18, rusqlite 0.41 (libsqlite3-sys 0.39.1, SQLite 3.54.1) and
  tantivy 0.27 for the search index. The SDK moved from anymap2 to anymap3 (one
  unmaintained crate less) and matrix-sdk-ui from chrono to jiff. No other dependency of
  matrix-rat changed for it.

<!-- Notes for the final text, not for the release:
- "Until messages are loaded again, search finds fewer of them": if the search backfill of
  §6.1 ("Search reaches back three months") ships with 1.1.0, it refills the index on its
  own (`EventCache::run_search_backfill` indexes the stored events once, then pages back);
  reword then.
- The test with the user's real 1.0.3 profile is still to come before release (PLAN §6.1).
-->
