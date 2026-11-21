# matrix-rat: plan and state

A mouse-friendly, themeable Matrix client for the terminal, built on ratatui.

This file says where the project stands, how it is built and worked on, what was decided, and
what comes next. It is kept current. Two things that used to live here have their own files:

- the plan as written before any code existed: [`docs/history/original-plan.md`](history/original-plan.md);
- the log of every review: [`docs/REVIEW-LOG.md`](REVIEW-LOG.md), with the reviews in
  [`docs/reviews/`](reviews/).

Last brought up to date: 2026-11-20, with the road to 1.0.0 (§6); releases up to 0.9.1. The
plans of 0.8 and 0.9 moved to [`docs/history/`](history/) that day.

## 1. What it is for

| Goal | State |
|---|---|
| A fixed, readable layout: room list, chat, composer, status line | Done. Below 60 columns, or with `Ctrl-B`, one pane fills the window. |
| The mouse is a first-class way in; everything also works by keyboard | Done. Every click and every key resolves to the same actions. |
| Few keys, none modal, all rebindable; a help overlay and a command palette | Done. A `vim` preset exists. |
| Settings in the app, written back to the config file | Done. |
| Themes: semantic theme files, built-in palettes, a theme that follows the terminal, dark/light pairing, live reload, templates for wallpaper-driven tools | Done: twelve built-in themes, templates for Noctalia, matugen, wallust, pywal. |
| Pictures in the terminal | Done for the kitty graphics protocol, with coloured blocks elsewhere. Other protocols: decided against for now (§5). |
| Correct Matrix: encryption with verification and recovery, browser login, sliding sync, replies, edits, reactions, threads, spaces, receipts, typing | Done, with the limits listed in §7. |

Not goals: calls, widgets, creating or deactivating accounts, a Windows build.

## 2. Releases

| Version | Date | What it brought |
|---|---|---|
| 0.1.0 | 2026-11-13 | The client: layout and mouse, themes, password and browser login, room list with spaces, timeline with replies, edits, reactions and threads, Markdown composer with completions, encryption (verification, recovery key), pictures and files both ways, settings, key-binding editor, command palette, notifications. |
| 0.1.1 | 2026-11-14 | Proper text inputs everywhere, search across all rooms, install from the NUR with a binary cache. |
| 0.2.0 | 2026-11-14 | Jump to a message that is not loaded (Matrix links, replied-to messages, search hits), unread handling with a "new" line, a notification click opens its room, drafts per room. |
| 0.3.0 | 2026-11-14 | Ignore and report, room list filters, moderation basics, pinned messages, forwarding, the window comes to the front after a notification click, `↑` from the composer selects messages. |
| 0.4.0 | 2026-11-15 | User info and own profile, sessions (rename, verify, sign out), key backup status, key export and import, reasons for undecryptable messages, the public room directory with previews, room settings for admins. |
| 0.5.0 | 2026-11-15 | Polls, voice messages, audio and video (played by the system), places, stickers, custom emoji as text, link previews, coloured code blocks, copying one code block, writing in `$EDITOR`, the icon and a launcher entry. |
| 0.6.0 | 2026-11-15 | Room commands (`/join`, `/kick`, `/topic`, …), message details with the event source, a list of a room's threads, notification keywords, do not disturb, pasting a picture from the clipboard, `NO_COLOR`, a high-contrast theme. Logs no longer hold raw error text. |
| 0.6.1 | 2026-11-15 | A code block typed on one line keeps its code; `/code`. |
| 0.7.0 | 2026-11-15 | Space management, custom emoji as pictures and in reactions, `O` for the system's viewer, `Ctrl-B` for the room list, ready-made Linux binaries, running under WSL described. A room that was left stays gone; a muted room shows as muted at once. The rest of the debug output is free of private data. |
| 0.8.0 | 2026-11-16 | `Esc` closes one layer at a time, down to "no chat open"; voice and audio messages played inside the app (a player on the system, pause and seek with mpv); `matrix-rat doctor`; unread count in the window title; a config with one bad value still starts. |
| 0.9.0 | 2026-11-16 | A channel for agents: `matrix-rat serve` without the screen on a bot account, `rooms`/`send`/`read`/`listen` for scripts, and an MCP server bound to one room with which a coding agent asks a question and waits for the answer (only a verified sender counts unless told otherwise). A playing message shows as playing in its thread too, and a long file name goes on over its rows; the doctor keeps key names in one piece. Build checks: no blocking call without a stated reason, `cargo deny` in CI. |
| 0.9.1 | 2026-11-17 | `serve` ends with exit code 78 when the profile needs a login, so a service can restart after a real failure; a `wait_for_message` that is already waiting finds an answer loaded late; a first read no longer keeps unverified history for later. |

Each release is a tag, a GitHub release with notes, the Nix package in the NUR with a binary
cache, and (from 0.7.0) archives for x86-64 and ARM64 Linux built by
`.github/workflows/release-binaries.yml`.

## 3. How it is built

One binary crate, about 146,000 lines of Rust with its tests (945 unit tests and 43 tests against
a real homeserver at 0.7.0; 1034 and 44 at 0.8.0; 1109 and 45 at 0.9.0; 1113 and 45 at 0.9.1).

**Stack.** `matrix-sdk`, `matrix-sdk-ui` and `matrix-sdk-base` 0.19.1, pinned exactly (every minor
release breaks APIs); `ratatui` 0.30 on `crossterm` 0.29; `tokio`; `ratatui-image` and `image` for
pictures; `comrak` for Markdown; `syntect` (pure-Rust matcher) for code colours; `nucleo-matcher`
for fuzzy search; `toml` and `toml_edit` for the config (comments survive a write-back); `zbus`
and `notify-rust` for the desktop; `notify` for watching theme files; `insta` for snapshots.
Rust 1.96 or newer.

**The two halves.**

- The **UI thread owns all state** (`App`, in `src/app.rs` and `src/app/*.rs`) and draws
  (`src/ui/*.rs`). Popups are the `Modal` enum (`src/modal.rs`); text fields are `LineEdit`
  (`src/lineedit.rs`).
- The **worker** (`src/matrix/*.rs`) runs on tokio and owns the SDK. The two talk only through
  `MatrixCmd` (UI to worker) and `MatrixEvent` (worker to UI). SDK types stay in the worker.
- Requests carry a number or a generation, and an answer that no longer belongs to what is on
  screen is dropped.

**Rules that hold everywhere.**

- **One way in.** Every key, click, palette entry, menu item and slash command resolves to an
  `Action` (`src/action.rs`, `src/keymap.rs`). A second way to do something is another way to
  the same action, with the same confirmation and the same rights check, never a second
  implementation. No key is handled outside the keymap.
- **Mouse** goes through a per-frame `HitMap` (`src/ui/hit.rs`): what was drawn says what a
  click on it means.
- **Nothing private in logs or debug output**: no message text, typed text, secrets, ids,
  paths, addresses, keywords, or error text as the SDK or a server worded it. Errors are logged
  by kind (`src/logsafe.rs`); types that hold private data have hand-written `Debug`. One test
  fills the app and looks at its whole debug output.
- **Everything from the network is hostile**: counts and lengths are bounded, text is made safe
  for the terminal (`src/text.rs`), addresses are validated, and work on other people's content
  has a time or size budget.
- **Popups work at 80x24**: they scroll and keep their hint line.
- **Never offer what will be refused**: an action the user lacks the right for is left out or
  greyed, with the reason.
- The worker's command handler stays small: new work goes into its own function and is
  spawned or boxed (a debug build once overflowed a worker's stack).

**Where things are.**

| Area | Files |
|---|---|
| Event loop, state, dispatch | `src/app.rs`, `src/app/*.rs` (one file per feature) |
| Drawing | `src/ui/*.rs`; the timeline in `src/ui/timeline.rs` with a row cache |
| Message text | `src/rich.rs` (received HTML to styled lines), `src/format.rs` (typed Markdown to HTML), `src/highlight.rs` (code colours), `src/complete.rs` (completions) |
| Pictures | `src/images.rs`, `src/ui/image.rs`, `src/matrix/media.rs` |
| Worker | `src/matrix/mod.rs` (session loop), `rooms.rs` (timelines), `manage.rs` (rooms and their settings), `spaces.rs`, `left.rs` (rooms left in a session), `session.rs` and `keyring.rs` (login and the store key), `verify.rs`, `devices.rs`, `search.rs`, `notify.rs` |
| Desktop | `src/notify.rs`, `src/portal.rs` (dark/light), `src/focus.rs` (window focus), `src/clipboard.rs`, `src/editor.rs` |
| Config, themes, keys | `src/config.rs`, `src/theme.rs`, `themes/`, `contrib/themes/`, `src/keymap.rs` |

## 4. How the work is done

- **A branch per piece of work**, in its own worktree under `.claude/worktrees/`, never in the
  main checkout.
- **`scripts/check.sh` must pass**: formatting, clippy with warnings denied, all unit tests, a
  build. The tests against a homeserver (`scripts/dev-homeserver.sh`, `cargo test -- --ignored
  --test-threads=1`) are run before a merge that touches the worker and before every release.
- **The other agent reviews.** `matrix-agy` reviews what `matrix-claude` (or a helper working
  for it) wrote, and the other way round. Nothing is merged before the reviewer agrees; findings
  are fixed, not argued away. The review gets a row in the review log.
- **Run it.** Features are tried in a real terminal against the test homeserver before they are
  called done. Most real bugs of 0.5 to 0.7 were found this way, not by unit tests.
- **Every fix gets a test that fails without it**, where that is possible.
- **A release** is: version bump, check and server tests on the release commit, the Nix build,
  the reviewer's release check (README against the code, and blockers), tag and GitHub release
  with notes, then the NUR package. A published tag is never moved.
- **Tests keep away from the person running them**: no test may reach the session's keyring,
  message bus or display (a test bus is started with `src/testbus.rs`).

## 5. Decisions by the user

| Date | Decision |
|---|---|
| 2026-11-12 | Name and binary `matrix-rat`; licence Apache-2.0; public at `github.com/tylerstyle/matrix-rat`. |
| 2026-11-12 | Keys: few, not modal, mouse and arrows first; a `vim` preset is optional. |
| 2026-11-12 | Formatting is typed as Markdown, plus slash commands. |
| 2026-11-14 | Work goes ahead without asking when both agents agree; every piece is checked by the other agent. |
| 2026-11-14 | Account creation is not wanted. |
| 2026-11-14 | `↑` from the composer selects messages; it does not start a reply or an edit. |
| 2026-11-14 | The filter chips in the room list stay visible. |
| 2026-11-15 | Coloured code blocks with syntect, accepting the non-standard licence of its bundled grammar files for now. |
| 2026-11-15 | The icon: the chef rat, with a one-colour and an outline variant; a launcher entry is installed. |
| 2026-11-15 | No Windows build. The README says how to run under WSL. |
| 2026-11-15 | Pictures: kitty graphics and coloured blocks are enough; no sixel or iTerm2 for now. Opening a picture in the system's viewer covers the rest. |
| 2026-11-15 | For 0.7: the sync guard, custom emoji pictures, space management, the rest of the debug hardening, prebuilt binaries. Popping out a chat, audio inside the app and several accounts wait. |
| 2026-11-20 | The order to 1.0.0: features first (§6.1), the three passes over the code last. |
| 2026-11-20 | Voice and video calls wait until after 1.0.0 and until the SDK can join calls itself. |
| 2026-11-20 | No plugin system. What is wanted of it is built in: transcription, translation, catching up, saved messages with reminders, archiving a room as text. |
| 2026-11-20 | Not wanted: sticker packs, an image generator, a lock screen, popping out a chat, feeds or Home Assistant posting into rooms (a server's job). Several accounts are run as several profiles. Other languages wait; AUR and `.deb` later. |
| 2026-11-20 | Nothing of the 1.0 work is pushed before 1.0 is tested and ready; `main` was put back to `de90ba8` on GitHub. |
| 2026-11-20 | Composer symbols: the emoji for now; a monochrome look later. |
| 2026-11-20 | Rooms bar: counts in superscript, `(n)>` opens the room list, and the room icons win over the label (it shrinks to `☰`). |
| 2026-11-21 | The memory and CPU pass is measured with the user's own account: the user runs a build of the 1.0 state for a day with `scripts/measure.sh` beside it, which reads only `/proc`. |

## 6. What comes next

Features first, then the passes over the code as it will ship (the user, 2026-11-20). What
0.8 and 0.9 were planned as is kept in [`history/0.8-ideas-from-faro.md`](history/0.8-ideas-from-faro.md)
and [`history/0.9-agent-channel.md`](history/0.9-agent-channel.md).

### 6.1 The road to 1.0.0 (agreed with the user on 2026-11-20)

Each piece is written by matrix-claude with helpers and reviewed by matrix-agy as always.

State on 2026-11-20 (evening): 1, 2, 3, 4, 5, 6, 7 and 8 are merged into the local `main`, reviewed and
checked (unit and server tests green), nothing of it pushed: 1.0 is published as a whole after the
passes of 6.2. All of 1 is in, listening to a recording before it is sent included (the ▶ on the
waiting note, `Alt-L`). On 2026-11-21 the user sent one from the 1.0 build with a real account: it
plays with its waveform in matrix-rat and in Element on iOS. 6 and 7 came as one branch (`archive`). The composer symbols are the emoji for now
(the user: a monochrome look can wait).

1. **Sending voice notes.** A microphone symbol next to the clip and the smiley in the
   composer. Holding it with the mouse records, letting go stops; from the keyboard one key
   starts and the same key stops (a terminal does not report a key being let go). The
   recording is put into the message box and is sent only when the user sends it. Recording
   is done by a program on the system (as playback is); the symbol shows only where a
   recorder and a microphone are found, which over ssh means the machine matrix-rat runs on.
   With it: the three composer symbols get one size and one style, chosen by one setting
   (open with the user: which look is the default).
2. **A rooms bar for the single chat view.** With the room list hidden, a one-line bar above
   the chat labelled "Rooms"; a click closes the chat and shows the room list. Rooms with new
   notifications appear in it with the icon the room list shows (initials where there is no
   picture) and their count at the top right, the newest on the left. When more are active
   than fit, the bar ends in `(n)>`, n being how many rooms are not shown; a click on it
   opens the room list. A click on an icon opens that room.
3. **Transcribing an audio message.** A button next to a voice or audio message; the text is
   shown under it, to the user only. Built in, done by a local program the user installs if
   they want it (candidate: `whisper-cli` of whisper.cpp, which also needs a model file, so
   the setting names both); the button shows only where that program is found. Nothing is
   sent to a service.
4. **Saved messages and reminders.** A list of messages the user marked; a click goes to the
   message. A reminder is a saved message with a time; it fires only while the app runs.
5. **Translating a message.** On a button, into the language set in the settings; the
   translation is shown under the original and stays there. What does the translating (a
   local program or a service) is a setting too. Nothing is translated without the button.
6. **Catching up.** A summary of what is unread in a room, shown to the user only; it shares
   the question of the provider with 5. A bot posting a daily summary is the smaller cousin:
   a script against `serve` with an account of its own, for rooms that account is in (the
   app and `serve` cannot share a profile).
7. **Archiving a room as Markdown.** Text only; the flow says so before it writes.
8. **Small things.** Check that the sticker picker and received stickers, animated ones
   included, work as they are. A README section on several accounts as several profiles.
   `matrix-rat mcp` takes the agent's name from an environment variable when `--name` is
   not given, so that two agents sharing one MCP entry sign with their own names.

### 6.2 The passes before 1.0.0 (asked for by the user on 2026-11-17)

Not features: passes over what is there, each ending in a report the user reads. They run in
two prongs: the measured look at memory and CPU first, so that the review that follows has
numbers to hold its findings against.

- **A deep look at memory and CPU use.** Measured, not guessed: memory at rest and over a
  day with a real account (many rooms, large timelines, pictures), CPU at rest, while
  syncing and while scrolling, for the app and for `serve`; what grows without bound; where
  the time of a frame goes. Then the fixes the numbers call for.
- **Another round of optimisation over the whole codebase**: what is computed more often
  than needed, copied where it could be borrowed, held longer than needed; also build time
  and binary size.
- **A second opinion from a tool from outside**: the codebase checked again by something
  that is not one of the agents that wrote and reviewed it. Which tool is open (candidates:
  `cargo audit`/`cargo deny` are in use already; `cargo geiger` for unsafe code in the tree,
  `cargo udeps`/`cargo machete` for unused dependencies, `miri` or sanitizers for the tests
  that can run under them, a fuzzer for the parsers, another vendor's review model); the
  user decides. To try (2026-11-20): matrix-agy's swarm feature on a clean checkout without
  `docs/reviews/`, for bugs and for speed and efficiency. It writes a report with evidence
  for each finding; matrix-claude checks it; what all three agree on is implemented, small
  local fixes by agy, anything in encryption, sync or the agent channel by matrix-claude.
- **With the review:** the rest of the lints that enforce our rules (forbid wildcard match
  arms on our own enums through clippy's configuration) and a weekly grouped dependency
  update.

### 6.3 Later, after 1.0.0 or by demand

- **Calls.** Voice calls wait until the SDK can join calls itself; with them a line in the
  room saying that a call is running, and a key that opens it in the browser. Video after
  that, if at all.
- **QR login.**
- **macOS**: notifications and their click, the secret store and following dark/light are
  Linux-only today (D-Bus); a medium piece of work, with nobody to test it.
- **More Linux packaging** (AUR, a `.deb` wrapping the release archives).
- **A gallery of a room's files.**
- **Popups over pictures**: cut the covered part out of a picture instead of hiding the
  whole picture while a popup is open. To check how ours behaves in kitty first.
- **Other languages.**
- **Reading the shortcode a received custom-emoji reaction carries** (the SDK does not
  expose it).
- **A numbered decisions file**, should §5 outgrow a table.

### 6.4 Not wanted or not pursued

- **By the user's decision** (2026-11-20 unless dated otherwise): a plugin system; sticker
  packs beyond the picker that exists; an image generator; a lock screen; popping out a
  chat; several accounts in one window (one window per `--profile` instead); feeds, Home
  Assistant and the like posting into rooms (a server's job); an update notice; a demo mode
  (2026-11-16); more picture protocols (2026-11-15).
- **Right-to-left scripts**: looked into on 2026-11-20. ratatui has no support, terminals
  disagree with each other, and other terminal chat clients do nothing; not pursued.
- **Looked at and not proposed**: a line-by-line mode for screen readers and pipes
  (valuable, but it wants the state logic free of the terminal, a large restructuring);
  splitting into several crates, a web demo, a website; bubbles; an encrypted vault (the
  SDK's store is already encrypted with a key from the secret store); mosh emoji handling.

## 7. Known limits and open questions

- **Custom emoji as pictures** have not been seen in a real kitty terminal by anyone: how the
  two-cell pictures look is unverified.
- **A declined invitation** that a late sync answer brings back is shown again: it cannot be told
  from a new invitation. Left as it is on the reviewer's advice.
- **Signing another session out** on a homeserver that has handed its accounts to an
  authentication service goes through that service's page; the app only links there.
- **Key backup and recovery are not created here**, only used; setting them up needs another
  client.
- **Verification by another person in a direct chat** has not been tried on real hardware.
- **The ARM64 binary** was built and reported its version on the build runner; nobody has run it.
- **Running under WSL** is described in the README and untried.
- **The test homeserver** (continuwuity) sends only the text of a thread's newest reply, so the
  threads list shows no "who and when" there; and one test with two accounts waiting for a
  member count failed once in 120 runs, during a network interruption, unexplained.
- `chrono` was declared unmaintained on 2026-11-17 (RUSTSEC-2026-0543; no known flaw). The
  SDK uses it too, so it stays in the tree until the SDK moves; `cargo deny` is told so. Our
  own use (a message's local time, fourteen places) can move to `jiff` or `time` then.
- **The SQLite built into the release archives** (through matrix-sdk 0.19.1) is older than
  3.54.0 and has a known flaw (RUSTSEC-2026-0459): opening a database whose `-wal` file has
  another page size can hand out stale memory. It takes someone who can write into the
  profile's private folder, who could read the store there anyway. The fix comes with the
  next matrix-sdk; the Nix package links the system's SQLite (3.54.0 in nixos-unstable today)
  and is not affected there.
  Two more advisories only say "unmaintained" (anymap2, bincode 1), both deep in
  dependencies.
- **The test homeserver's own fault**: continuwuity now and then answers a sync with an
  error ("pdu should have a shortstatehash"), in bursts, and a server-backed test that waits
  for something then times out (seen in the threads, people and media tests, on `main` as
  on a branch). A second run is the remedy; a fresh database for it may be worth trying.
- **A block of messages twice in a loaded timeline**: once, in the archive's server test, four
  messages of a flood stood twice, right after themselves, while sync still brought the newest
  ones. Archive and catch up count an event once. `matrix-agy` traced it to matrix-sdk-ui 0.19.1:
  a live timeline built while sync brings a batch gets that batch twice. The room view can show it
  too; fixed on our side (`dup-block`, merged 2026-11-21). A draft upstream issue waits for the
  user's word.
- **Small review notes left open on purpose** are in the review files: a half emoji box before
  the read marker when a row is cut, no retry of emoji pictures that failed to load, the
  field limit of the "new room" form.

### The agent channel (as of 0.9.1)

- `listen` on the command line does not tell an answer that arrived during a gap in the
  sync and is loaded later, and in a room that shows no message within three pages when a
  watch begins it can tell loaded history as new once. The MCP tools do neither.
- A long `wait_for_message` is a row of short ones, each looking at the room anew. Its
  progress notices start their beat anew with each look, so about one a minute is left out;
  and if `serve` stays away, the wait ends with "nothing new" and only the next call says
  what is wrong.
- In a room that holds no message at all, what is written between an agent's first wait or
  read and its next call is not handed over, unless the agent sent something first. The MCP
  server cannot tell an empty room from one whose messages could not be loaded, and taking
  the second for the first made old history news. `serve` would have to say whether a read
  reached the room's start.
- When the user verifies a device, or a key comes late, messages that were shown as
  untrusted or unreadable before are handed over as theirs, however old; the tool's
  description tells the agent to look at the time.
- A wait hands over twenty messages at most; with more, the older ones of the backlog are
  left to `read_messages` (as designed), except those owed, which come with the next wait.
- Seen once and not again in seven tries: a `read` right after the first `send` of a fresh
  `serve` listed the new message twice.
- A config file with a bad value (0.8) has not been tried by the user yet.
