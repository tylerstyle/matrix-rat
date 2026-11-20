# matrix-rat: plan and state

A mouse-friendly, themeable Matrix client for the terminal, built on ratatui.

This file says where the project stands, how it is built and worked on, what was decided, and
what comes next. It is kept current. Two things that used to live here have their own files:

- the plan as written before any code existed: [`docs/history/original-plan.md`](history/original-plan.md);
- the log of every review: [`docs/REVIEW-LOG.md`](REVIEW-LOG.md), with the reviews in
  [`docs/reviews/`](reviews/).

Last brought up to date: 2026-11-20, with the road to 1.0.0 (§6); releases up to 0.9.1.

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
| 2026-11-20 | The order to 1.0.0: features first (§6, "The road to 1.0.0"), the three passes over the code last. |
| 2026-11-20 | Voice and video calls wait until after 1.0.0 and until the SDK can join calls itself. |
| 2026-11-20 | No plugin system. What is wanted of it is built in: transcription, translation, catching up, saved messages with reminders, archiving a room as text. |
| 2026-11-20 | Not wanted: sticker packs, an image generator, a lock screen, popping out a chat, feeds or Home Assistant posting into rooms (a server's job). Several accounts are run as several profiles. Other languages wait; AUR and `.deb` later. |
| 2026-11-15 | For 0.7: the sync guard, custom emoji pictures, space management, the rest of the debug hardening, prebuilt binaries. Popping out a chat, audio inside the app and several accounts wait. |

## 6. What comes next

See §6a for the plan that came out of looking at faro (0.8, done) and §6b for 0.9. Besides that, the list of what could
be done, by demand:

- Playing audio and voice messages inside the app (asked for by the user).
- QR login.
- macOS: notifications and their click, the secret store and following dark/light are
  Linux-only today (D-Bus); a medium piece of work, with nobody to test it.
- More Linux packaging (AUR, a `.deb`): small now that release archives exist; later.
- A gallery of a room's files.
- Reading the shortcode a received custom-emoji reaction carries (the SDK does not expose it).

Dropped by the user on 2026-11-20: popping out a chat, several accounts in one window (the
README is to say how to run one window per `--profile`), a lock screen, sticker packs beyond
the picker that exists, an image generator, feeds and Home Assistant posting into rooms.
Other languages wait.

### The road to 1.0.0 (agreed with the user on 2026-11-20)

Features first, each written by matrix-claude with helpers and reviewed by matrix-agy as
always; the passes over the code (next heading) come last, over the code as it will ship.

1. **Sending voice notes.** A microphone symbol next to the clip and the smiley in the
   composer. Holding it with the mouse records, letting go stops; from the keyboard one key
   starts and the same key stops (a terminal does not report a key being let go). The
   recording is put into the message box and is sent only when the user sends it. Recording
   is done by a program on the system (as playback is); the symbol shows only where a
   recorder and a microphone are found, which over ssh means the machine matrix-rat runs on.
   With it: the three composer symbols get one size and one style (open: which, see below).
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
5. **Translating a message.** The translation is shown under the original and stays there.
   Open: what does the translating (a local program or a service the user names), and
   whether a room can be set to translate what arrives without asking.
6. **Catching up.** A summary of what is unread in a room, shown to the user only; it shares
   the question of the provider with 5. A bot posting a daily summary is the smaller cousin:
   a script against `serve` with an account of its own, for rooms that account is in (the
   app and `serve` cannot share a profile).
7. **Archiving a room as Markdown.** Text only; the flow says so before it writes.
8. **Small things.** Check that the sticker picker and received stickers, animated ones
   included, work as they are. A README section on several accounts as several profiles.
   Right-to-left scripts: research first, then a decision.

Link previews are there since 0.5.0 (`link_previews`); nothing to do.

Open with the user: the look of the composer symbols; QR login, macOS, an update notice, the
file gallery and the smaller leftovers of §6a; the unmerged branches.

### Before 1.0.0 (asked for by the user on 2026-11-17)

Not features: three passes over what is there, each ending in a report the user reads. They
run after the features above (2026-11-20), in two prongs: the measured look at memory and
CPU first, so that the review that follows has numbers to hold its findings against.

- **A deep look at memory and CPU use.** Measured, not guessed: memory at rest and over a
  day with a real account (many rooms, large timelines, pictures), CPU at rest, while
  syncing and while scrolling, for the app and for `serve`; what grows without bound; where
  the time of a frame goes. Then the fixes the numbers call for.
- **Another round of optimisation over the whole codebase**: what is computed more often
  than needed, copied where it could be borrowed, held longer than needed; also build time
  and binary size (the branch `release-profile` has an earlier try).
- **A second opinion from a tool from outside**: the codebase checked again by something
  that is not one of the agents that wrote and reviewed it. Which tool is open (candidates:
  `cargo audit`/`cargo deny` are in use already; `cargo geiger` for unsafe code in the tree,
  `cargo udeps`/`cargo machete` for unused dependencies, `miri` or sanitizers for the tests
  that can run under them, a fuzzer for the parsers, another vendor's review model); the
  user decides. To try (2026-11-20): matrix-agy's swarm feature on a clean checkout without
  `docs/reviews/`, for bugs and for speed and efficiency. It writes a report with evidence
  for each finding; matrix-claude checks it; what all three agree on is implemented, small
  local fixes by agy, anything in encryption, sync or the agent channel by matrix-claude.

## 6a. Ideas from faro (0.8: A to E approved by the user on 2026-11-16)

[faro](https://github.com/harababurel/faro) is a terminal Signal client on the same libraries
(ratatui, ratatui-image, crossterm, tokio). Its design documents and source were read on
2026-11-16 at the user's request. Its licence is AGPL-3.0 and ours is Apache-2.0: **behaviour
and ideas are taken up, no code is copied.** Every piece below is written fresh against our
own types.

**Already the same here:** UI thread and worker apart, one action table behind keys, mouse and
palette, key hints read from the live keymap, live theme preview, config written back with
comments kept, private data kept out of debug output, `↑` from the composer selecting messages,
a panic hook that restores the terminal, a message counting as read only when it is really on screen in a focused window, release actions pinned to commits.

### Proposed for 0.8, in this order

| # | Piece | What changes | Size |
|---|---|---|---|
| A (merged, M33) | **Esc closes one layer at a time, down to "no chat open"** | Esc keeps its order (popup, completion, reply or edit, selection) and gets one more step at the end: with nothing left to cancel, it closes the chat. The draft is saved, typing notice stopped, the room list is shown (also if hidden with `Ctrl-B`) and focused with the cursor on the room just left; the chat area shows the icon and a few key hints. `Enter` or a click opens it again with the draft. While no chat is open nothing is marked read (reading already needs the chat on screen in a focused window). (With the vim preset, Esc moves between composer and messages and never closes the chat.) On a narrow window the room list fills the window, which our one-pane mode already does. A setting keeps the old behaviour. | Small to medium |
| B (merged, M35) | **Voice messages and audio played from inside the app** | `x` (and the menu, and a click on `▶`) on a voice or audio message plays it; the row shows `■ 0:05 / 0:14` with a waveform whose played part is coloured; one sound at a time; leaving the chat, closing it or quitting stops it. As in faro, **no audio library is linked**: a player already on the system is started (`mpv`, `ffplay`, `pw-play`, `paplay`, `aplay`; a setting names another). That keeps the binary and the Nix package free of ALSA/PipeWire. Matrix voice messages carry their duration and waveform in the event, so, unlike faro, we need no ffmpeg for either. With `mpv` we can go further than faro: its control socket gives real position, pause and seek (`←`/`→`); other players get start/stop and an estimated position. Over SSH, play is refused with a hint to save the file. The downloaded file is a temporary one with mode 0600, removed afterwards. | Medium |
| C (merged, M34) | **`matrix-rat doctor`** | One screen (command and palette entry) saying what the app found: terminal, colour depth, picture protocol and why, keyboard protocol, clipboard tool, notification service, secret store, audio player, key bindings that clash or actions without a key, settings that have no effect. One key copies it as text, with nothing private in it. Bug reports become one paste; it would have shortened several of our own hunts. | Small to medium |
| D (merged, M34) | **Unread count in the window title** | `matrix-rat (3)`, without room names, restored on exit; a setting turns it off. | Small |
| E (merged, M34) | **A bad config value does not take the rest with it** | Each key is read on its own: a wrong value keeps its default and the status line names the key. To check first how far ours already goes. | Small |

A is what the user noticed in faro. B is the audio the user asked for on 2026-11-15. C to E
are cheap and stand alone.

### Worth doing later

- **A lock screen** (key or idle time): hides content, stops audio, holds back read receipts,
  makes notifications generic. Medium.
- **Popups over pictures**: faro cuts the covered part out of a picture instead of hiding the
  whole picture while a popup is open. To check how ours behaves in kitty first.
- **Transcribing voice messages** with a local `whisper` program, off by default. Small once B
  exists; remote services are not wanted without a clear consent step.
- **An update notice**, opt-in, asking GitHub at most twice a day, never installing; a build
  switch so the Nix package leaves it out.
- **Lints that enforce our rules**, the rest: forbid wildcard match arms on our own enums
  through clippy's configuration; a weekly grouped dependency update. (Blocking calls and
  `cargo deny` are done, see §6b.)
- **A numbered decisions file** in faro's manner, should §5 outgrow a table.
- **Packaging**: AUR and a `.deb` wrapping the release archives (already on our list).

### Looked at and not proposed

- **A demo mode** with scripted people: the user does not want it (2026-11-16).
- **A line-by-line mode** for screen readers and pipes: valuable, but it wants the state logic
  free of the terminal, which is a large restructuring here.
- **Splitting into several crates, a web demo, a website**: much work for what a one-person
  project gains.
- **Bubbles**, **more picture protocols** (decided against on 2026-11-15), **an encrypted
  vault** (the SDK's store is already encrypted with a key from the secret store), **mosh
  emoji handling** (no one has asked).

## 6b. 0.9: a channel for agents (approved by the user on 2026-11-16)

**Why.** The user wants agents working on their machines to ask them questions while they are
away, through Matrix. Today that runs on an outside tool (`matrix-commander-rs`) with a bot
account and polling. matrix-rat's Matrix side already runs without the screen, so it can
offer this itself. The account used is a dummy that is only in the rooms it shares with the
user, so handing an agent the whole instance is acceptable (the user's decision).

**What is built.**

| Piece | What it is |
|---|---|
| `matrix-rat serve` | The worker without the screen, on a profile that is already logged in (log in once with the normal app, `--profile bot`). Holds the profile lock, stays in sync, and listens on a private socket in the runtime directory (mode 0600, owner checked). Never accepts an invitation by itself. Ends cleanly on SIGTERM; made to run as a user service. |
| `matrix-rat send` / `read` / `listen` | Talk to a running `serve` over the socket. `send --room R "text"`; `read --room R [--limit n] [--from @user]` prints JSON, one message a line; `listen --room R` stays connected and prints messages as they come. Without a `serve` running they say so (they do not open the store themselves: two processes on one store is what the lock forbids). |
| `matrix-rat mcp --room R --name N` | An MCP server on stdin/stdout for one agent, bound to one room: tools to send (signed `[N]`), to read the latest messages, and to wait some minutes for a new one. It can reach no other room, whatever the agent asks. `--from @user` makes it hand over only that user's messages as answers; everything else in the room is marked as not from the user. |

**Rules for it.**

- The socket protocol is one JSON object a line, with bounded line length and a small fixed
  set of requests; room names are resolved by `serve` against the rooms the account is in.
- Text from Matrix that reaches an agent is data, never instructions: the tools' descriptions
  say so, and each message carries who sent it and whether that is the configured user.
- Nothing of it is logged beyond kinds and counts, as everywhere.
- An idle agent cannot be woken by this; that limit is known and accepted for now.

**State (2026-11-16):** built, reviewed (M36) and merged. Tried by hand against the test
homeserver: a bot profile logged in with the normal app, `serve`, `rooms`, `send`, `read`,
`listen` and the MCP server over a pipe all work, and `serve` ends cleanly on SIGTERM. Seen
once and not again in seven tries: a `read` right after the first `send` of a fresh `serve`
listed the new message twice.

Tried with Claude Code as the MCP client (a second account
answering after 50 seconds): it found the three tools, sent, was refused a `room` argument,
and waited 47 seconds for the right answer. The first try found a fault, fixed on
`leftovers-09`: older messages that a `read` loaded into a watched room were told to
listeners as new, so `wait_for_message` came back at once with an old message. Two things
were done about it, the second after an independent review of the first:

- `serve` takes a message it did not know for news only if it stands after the newest one
  it knew, and a watch loads older messages first until the room shows one.
- The MCP server no longer goes by a message's place alone. It remembers what each look at
  the room showed, what it handed to the agent and what the agent is still owed. Before the
  point the agent has seen, a message is news only if it was not there at the time (an
  answer missed in a gap of the sync and loaded later), if it was handed over and was edited
  since, or if it named the user without counting as theirs and counts now. What is owed
  stays owed until it is handed over: a read that does not show it or a cancelled wait
  loses nothing. A room that showed no message is not taken for an empty one (a failed
  load looks the same), so its history is not news later.

Reviewed by independent reviewers of `matrix-claude`, each starting from the code alone (`matrix-agy`
was not available). Four rounds asked for changes,
which are in (see `docs/reviews/M37-review.md`).

Still open:

- `listen` on the command line does not tell an answer that arrived during a gap in the
  sync and is loaded later, and in a room that shows no message within three pages when a
  watch begins it can tell loaded history as new once. The MCP tools do neither.
- (Mended after 0.9.0, branch `fixes-091`.) A `wait_for_message` that is already waiting
  now looks at the room anew once a minute (a long wait is a row of short ones, each by
  the rules of a new call) and hands over an answer loaded into a gap, if it is among
  the room's newest hundred. With `after_event_id`, what is loaded after that point ends
  the wait; what is loaded before it is owed and handed over by a later wait that names
  no point. A room that has shown no message is listened to in one piece, as
  before. A later part that cannot begin (the connection is being started again) is
  tried again and does not end the wait (if `serve` stays away, the wait ends with
  "nothing new" and the next call says what is wrong). Left as it is: the progress notices start
  their beat anew with each look, so about one a minute is left out; and a look that loads history into a room that showed nothing before can make `serve`
  tell it to listeners as new (the case of the first point above).
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

**Also in 0.9:** the user tried 0.8 (audio with and without a waveform, leaving a room stops
it, `Esc`, the unread count, the doctor: all work; a config with a bad value is not tried
yet) and nothing had to be fixed. Merged with M37: the three open notes of the 0.8 reviews (a playing message inside a
thread view, empty continuation rows of a playing row with a long name, the doctor breaking
`Alt-Enter` across lines), and the stricter build checks:

- `clippy.toml` refuses calls that block their thread (`sleep`, waiting for a program or a
  thread, `block_on`, blocking channel calls). The fifteen places that do so on purpose say
  why with `#[expect(clippy::disallowed_methods, reason = "…")]`; tests are exempt.
- `deny.toml` and `.github/workflows/checks.yml`: `cargo deny check` for every change and
  once a week. Three advisories are ignored there, each with its reason (see §7).

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
- **Small review notes left open on purpose** are in the review files: a half emoji box before
  the read marker when a row is cut, no retry of emoji pictures that failed to load, the
  field limit of the "new room" form.
