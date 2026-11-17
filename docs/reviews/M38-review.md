# M38: first fixes after 0.9.0 (branch `fixes-091`)

Reviewed by an independent reviewer of `matrix-claude` (a fresh session, reading only), in three
rounds on 2026-11-17. `matrix-agy` was not asked, to spare its budget.

## What was reviewed

- `serve` ends with exit code 78 when the profile has no session that can be used, with 1 for any
  other failure (asked for by `nixos-claude`, for the service unit on hpserver).
- `wait_for_message` finds an answer that is loaded into a gap of the sync while it waits.
- The fix to `read_messages` that went into 0.9.0 without a second look (`5ac0755`).

## Rounds

| Round | Commit | Verdict | Findings |
| :--- | :--- | :--- | :--- |
| 1 | `76c8360` | changes requested | The re-read inside a running wait gave a loaded message no place: an answer behind the question was handed over twice; with `after_event_id` one was marked known without being owed. Exit code: correct. `5ac0755`: guard correct, but beside it a read as first look owed doubted history. |
| 2 | `dab9d95` | changes requested | The wait as a row of short ones closed round 1. New: in a room that had shown nothing, a message arriving between two parts was taken for what the room held; a later part that could not begin ended the whole wait with an error. |
| 3 | `a6d0705` | approve | Both closed. |

## Left as it is (the reviewer's notes, none blocking)

- A later part of a wait that fails (also: its listener ends) is tried again without a word; if
  `serve` stays away, the wait ends with "nothing new" and the next call says what is wrong.
  No progress notice is sent during the pauses.
- A message heard without a place that was never owed (history loaded after the look) is still
  taken for news at the room's end.
- `after_event_id` is dropped in a later part when its message is no longer among what is read.
- No test for a later part that fails and is tried again; the mapping to 78 in `main` is tested
  only through `End::into_error`.

## Checks

1113 unit tests, 45 of 45 tests against the homeserver (hpfury, 2026-11-17).
