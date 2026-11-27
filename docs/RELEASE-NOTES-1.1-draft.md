# matrix-rat 1.1.0 (draft)

A draft of the release notes for 1.1.0, in the form of the GitHub releases of 1.0.x, kept
up to date while 1.1.0 is built. So far: the move to matrix-sdk 0.20, every other
dependency brought up to date, a mark between messages of one person in a row, and a
log without the libraries' lines.

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
  stretching), so the old index cannot be opened and is deleted. Search history (see New)
  fills both again in the background, three months back; with it turned off, search finds
  fewer messages until they are loaded again.

## New

- **Several messages of one person in a row are told apart.** Each one after the first
  starts with a muted `·` under the `●` of the line with the name and time, on its first
  line only: no row more, and it is there without colours (`NO_COLOR`) and in 256 colours
  over ssh as well. Until now such a run read as one long message, parted only where a
  message had reactions.
- **A device that is verified but lacks your signing keys says so, and offers the way
  out.** When, comparing emoji between two of your devices, the new one confirms first,
  matrix-sdk 0.20 can, rarely, leave it verified but without the private signing keys
  and the backup key: it cannot verify others, and older messages stay locked. The app
  said "this device is verified" and offered nothing. Now the status line shows "⚠ keys
  missing — fix" (30 seconds after a verification, as the keys normally come within
  seconds), the Encryption setting says "verified, but without the signing keys", and
  both open a popup with **Verify again** (with another device: this time the keys come
  along) and **Enter recovery key**. The Done screen of a verification offers the same
  when the keys did not come.
- **A deleted message says why, and who deleted it.** "Message deleted: sent to the wrong
  room" when the sender gave a reason, "Deleted by carol: spam link" when a moderator
  deleted it, "Deleted by you" for one you deleted. The details (`i`) show the whole
  reason and the user ID of whoever deleted it. The reason is shown as it was written, on
  one line: no links, no formatting.
- **An edit, a deletion or a reaction that could not be sent says so, and can be tried
  again or thrown away.** It showed nothing: it looked sent, and everything you sent after
  it in that room stayed "◌ sending", even after a restart. Now the message shows
  "✕ edit not sent: not allowed in this room" or "✕ deletion not sent", your reaction's
  chip a `✕`, and what waits behind it "◌ waiting". `Enter` tries again, as for a message
  that was not sent, and `Del` throws away what did not go out: a discarded edit brings
  back the text before it, a discarded deletion keeps the message, a discarded reaction
  is gone. This works on someone else's message too (your reaction, or your deletion as a
  moderator); the status line, the menu and the details (`i`) say what failed. What
  failed because the connection was lost now goes out by itself once the app is back
  online.
- **Delete with a reason…** in the menu of your own messages and in the command palette
  asks for a reason before it deletes; `Del` still deletes at once. It has no key by
  default (`delete_with_reason` under `[keys.timeline]` gives it one).

- **Search reaches back three months.** Once a session, about ten seconds after the room
  list is in, matrix-rat fetches three months of every room's messages in the background,
  a room a second, so that search across all rooms (`Ctrl-G`) finds them, encrypted ones
  too once their keys are there. The search box says how far it has got ("Fetching older
  messages, back to 28 Aug…", then "back to about 28 Aug"). It costs data and disk: the
  first run fetches the most, later ones read what is stored, in seconds, and fetch only
  what is missing. **Search history** in the settings (`search_history = false`) turns it
  off, at once; `matrix-rat serve` never fetches. Built on matrix-sdk 0.20's search backfill.

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
- **Room previews reach back**: a room whose last events hold no message (only topic
  changes or people coming and going, say) shows its last message under its name, fetched
  from the server, where its line had none. And an unread room whose read marker lies
  further back than what was synced has its older messages fetched (at most 600), so its
  count of unread messages is right. Both come with matrix-sdk 0.20 and run whatever the
  search history setting says.
- **The log keeps the libraries out.** By default it holds only matrix-rat's own notes;
  the lines of matrix-sdk and the other libraries, many of which carry room, user or
  device ids or an error as it came, no longer go in. `MATRIX_RAT_LOG` still lets them in
  to look into a problem (see **Good to know** in the README).
- **"They match", the other device first.** Comparing emoji between two of your devices,
  each screen says on which device to confirm first, while it matters: the device that
  vouches for the new one, so that it trusts the new one before that one asks for the
  keys.
- **A session that is verified already can be verified again** from the session list
  (`v`), instead of "That session is verified already": that is how a session that
  lacks the signing keys gets them from this one.
- **Theme files are watched more calmly** (notify 9): reading a theme no longer wakes the
  watch, and when the system's queue of file events runs over, the theme is reloaded,
  since any file may have changed.

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
  unmaintained crate less) and matrix-sdk-ui from chrono to jiff.
- **Rust 1.99 or newer** is needed to build matrix-rat (was 1.96).
- notify 9 (from 8) for watching theme files, signal-hook 0.4 (from 0.3) for suspending
  with `Ctrl-Z`, and every other dependency at its newest compatible version.

<!-- Notes for the final text, not for the release:
- Search history: reworded the upgrade note above (the run indexes the stored events once,
  then pages back). Measured on the dev server, 2026-11-27 (`search_history_cost`,
  docs/DESIGN-1.1.md §1 "Measured"): 9 101 events in 9 rooms took 189 `/messages`, 2.3 MB
  down, 57 s and 12.8 MB of disk (2.3 KB per event); a later start asked the server nothing
  and was done in 1 s (4 s when the index is redone). For 100 000 events in 100 rooms: about
  3 400 requests and 236 MB, 29 min at 0.5 s a request plus up to 23 min of pacing on the
  first run; within the gate (1 GB, 1 h), so on by default. Later starts leave the SDK's
  sweep out: it fetches nothing then, but its pacing cost a second per room and week (about
  21 min for 100 rooms) before. What the SDK had not indexed when a session ended is indexed
  at the next start (a mark of ours, `history-indexing`, removes the SDK's `reindexed-v1`).
  matrix-sdk 0.20's sweep alone stops at about 300 events a room (its batch cap counts what it reads back
  from disk); our own walk takes each room the rest of the way (at most 3 900 events). With
  search history off, the start still costs the latest-event and read-receipt backfills (87
  `/messages` in the test, 67 of them one room of 2 000 topic changes).
- Upgrade tested on the dev homeserver on 2026-11-27: profiles made with the 0.19.1 build
  (one without cross-signing, one verified with an encrypted room) opened with the 0.20
  build: the session was restored, history showed and decrypted, search found the old
  messages (newest first) after the index was rebuilt, and the verification state stayed.
  The SDK logs one warning per room while it rebuilds ("Recreating the search index").
- The user's real 1.0.3 profile (352 MB), 2026-11-27, with a release build of main `8776aa3`
  (SDK 0.20 + all updates): a consistent copy (SQLite backup, integrity ok) on hpfury, opened
  OFFLINE only (no network namespace: a copy that synced would be the user's device and take
  to-device keys meant for the real client), with its own HOME. First start: no login form, no
  panic, stores migrated and opened in under 0.5 s, verification state kept (device verified,
  signing keys, backup on), message cache emptied as expected (its 87 MB file keeps its size
  until SQLite reuses the pages); quit cleanly. Second start: 0.1 s to the same state. The copy
  was deleted afterwards. To repeat with the release candidate before release.
-->
