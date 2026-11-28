# matrix-rat: plan and state

A mouse-friendly, themeable Matrix client for the terminal, built on ratatui.

This file says where the project stands, how it is built and worked on, what was decided, and
what comes next. It is kept current. Two things that used to live here have their own files:

- the plan as written before any code existed: [`docs/history/original-plan.md`](history/original-plan.md);
- the log of every review: [`docs/REVIEW-LOG.md`](REVIEW-LOG.md), with the reviews in
  [`docs/reviews/`](reviews/).

Last brought up to date: 2026-11-28, with 1.2.0 planned (§6.1); releases up to 1.1.0. The
plans of 0.8 and 0.9 moved to [`docs/history/`](history/) on 2026-11-20, the road to 1.0 on
2026-11-21, the plan of 1.1.0 on 2026-11-28.

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
| 1.0.0 | 2026-11-21 | Voice messages recorded in the composer (listened to before sending), a rooms bar above the chat while the room list is out of sight, local transcripts (`W`) and translations (`L`) under messages, saved messages and reminders (`b`, `B`), `/archive` to Markdown, `/catchup` through a summary command of your own, `--from` once for each account of the user, `MATRIX_RAT_AGENT_NAME` for `mcp`. The passes of 6.2: fewer threads, open files and wakeups, memory that levels off over a day, timeline changes and drawing many times faster; a swarm review of the whole code with every verified finding fixed; people who share a name told apart (colours follow the account); paging that never stalls and no hidden history; no wildcard matches on our own enums. |
| 1.0.1 | 2026-11-21 | No more quitting two seconds after opening a room (a repaint asked the terminal for its cursor while the event stream held its reader); an error exit shuts the worker down first. Nothing lights up under the mouse pointer any more, and the terminal reports only clicks, the wheel and drags. comrak 1.0, clap 4.6.8, zeroize 1.9.2; the release workflow on checkout v7, upload-artifact v7, download-artifact v8. |
| 1.0.2 | 2026-11-23 | Dependencies brought up to date: url 2.5.9 (RUSTSEC-2026-0592), comrak 1.2.0 (slow parsing of some tables, autolinks and strikethrough runs fixed), zbus 6.5, clap 4.6.9 and 37 more compatible updates (one crate fewer); `cargo deny` looks for unmaintained crates in the whole tree again (cargo-deny 0.22 in CI). |
| 1.0.3 | 2026-11-23 | `/catchup N` summarises the last N messages of the open room (1 to 300), read or not, and moves no read marker. Over ssh, where themes come down to 256 colours, the room cursor shows again: each colour goes to the nearest of cube and grey ramp, shades that differed stay apart, and a `›` marks the cursor while the room list has focus. A message that matrix-sdk-ui 0.19.1 delivered twice is collected once, at its first place, so the newest of a summary are the newest. Every settings hint is whole in its two lines (nine were cut, the translate and summary commands' privacy notes among them). No half emoji box before the read marker; a custom emoji picture that failed is tried again after a pause; names stop at the 255 bytes a server takes. |
| 1.1.0 | 2026-11-28 | matrix-sdk 0.20 (pinned exactly again; the stores are migrated, so no way back to 1.0.x; search lists the newest matches first) and every dependency up to date (Rust 1.99, notify 9, signal-hook 0.4). Search history: every room fetched back about three months in the background, so search across all rooms finds older messages (on by default; for 100,000 messages about 230 MB and half an hour, once; later starts read the store in seconds; what a quit left unsearchable is made searchable at the next start). An edit, a deletion or a reaction that could not be sent shows "✕ … not sent" (a ✕ on a reaction), `Enter` tries it again and `Del` throws it away (a failed deletion keeps the message); what waits behind it says "◌ waiting". A deleted message says why and who deleted it; "Delete with a reason…". A muted `·` parts the messages of one person in a row. A device verified without the signing keys says so and offers "Verify again" or the recovery key; the Compare screen asks for the old device first. The log keeps the libraries out. Fixed: your name for one room before the sync has the room's state, and after a rename elsewhere; the SDK's panic while loading older messages. |

Each release is a tag, a GitHub release with notes, the Nix package in the NUR with a binary
cache, and (from 0.7.0) archives for x86-64 and ARM64 Linux built by
`.github/workflows/release-binaries.yml`.

## 3. How it is built

One binary crate, about 205,000 lines of Rust with its tests (945 unit tests and 43 tests against
a real homeserver at 0.7.0; 1034 and 44 at 0.8.0; 1109 and 45 at 0.9.0; 1113 and 45 at 0.9.1;
1589 and 47 at 1.0.0; 1593 and 47 at 1.0.1 and 1.0.2; 1620 and 49 at 1.0.3; 1742 and 57 at
1.1.0, besides 13 measurements that are run by hand).

**Stack.** `matrix-sdk`, `matrix-sdk-ui` and `matrix-sdk-base` 0.20.0 (0.19.1 up to 1.0.3),
pinned exactly (every minor release breaks APIs); `ratatui` 0.30 on `crossterm` 0.29; `tokio`;
`ratatui-image` and `image` for pictures; `comrak` for Markdown; `syntect` (pure-Rust matcher)
for code colours; `nucleo-matcher` for fuzzy search; `toml` and `toml_edit` for the config
(comments survive a write-back); `zbus` and `notify-rust` for the desktop; `notify` for watching
theme files; `insta` for snapshots.
Rust 1.99 or newer.

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
| 2026-11-20 | The order to 1.0.0: features first, the three passes over the code last ([`history/1.0-road.md`](history/1.0-road.md)). |
| 2026-11-20 | Voice and video calls wait until after 1.0.0 and until the SDK can join calls itself. |
| 2026-11-20 | No plugin system. What is wanted of it is built in: transcription, translation, catching up, saved messages with reminders, archiving a room as text. |
| 2026-11-20 | Not wanted: sticker packs, an image generator, a lock screen, popping out a chat, feeds or Home Assistant posting into rooms (a server's job). Several accounts are run as several profiles. Other languages wait; AUR and `.deb` later. |
| 2026-11-20 | Nothing of the 1.0 work is pushed before 1.0 is tested and ready; `main` was put back to `de90ba8` on GitHub. |
| 2026-11-20 | Composer symbols: the emoji for now; a monochrome look later. |
| 2026-11-20 | Rooms bar: counts in superscript, `(n)>` opens the room list, and the room icons win over the label (it shrinks to `☰`). |
| 2026-11-21 | The memory and CPU pass is measured with the user's own account: the user runs a build of the 1.0 state for a day with `scripts/measure.sh` beside it, which reads only `/proc`. |
| 2026-11-23 | Several messages of one person in a row: a muted `·` in the indent on the first line of each further message, under the header's `●` (1.1.0, [`history/1.1-plan.md`](history/1.1-plan.md)). |
| 2026-11-26 | matrix-rat in the browser and as a phone app (a PWA, iOS and Android) with ratzilla: the main goal of 1.2.0 (§6.1). A test build (phase 0) runs beside 1.1.0 first and decides go or no-go. |
| 2026-11-26 | The web app is served by GitHub Pages from this repository; only the app's files are there, all data stays on the device. |
| 2026-11-26 | The test build is hosted locally first (hpserver; desktop by ssh tunnel, phones on the Wi-Fi); notifications while the web app is closed wait (phase 4). |
| 2026-11-26 | No unlock prompt for the web app ("Almost no one is doing that to any other messenger they use"), as no lock screen for the terminal app. |

## 6. What comes next

What 0.8, 0.9 and 1.0 were planned as is kept in
[`history/0.8-ideas-from-faro.md`](history/0.8-ideas-from-faro.md),
[`history/0.9-agent-channel.md`](history/0.9-agent-channel.md) and
[`history/1.0-road.md`](history/1.0-road.md) (features first, then the passes over the code as
it would ship: the user, 2026-11-20); the plan of 1.1.0 in
[`history/1.1-plan.md`](history/1.1-plan.md).

### 6.1 1.2.0 (next): in the browser and on phones (ratzilla)

Decided by the user on 2026-11-26, after research the same day. matrix-rat built for the
browser (WebAssembly) with [ratzilla](https://github.com/ratatui/ratzilla), ratatui's web
backend, and installable on iOS and Android as a home-screen app (a PWA).

- **What carries over:** drawing (`ui::draw`) and key handling (`App::on_key`) as they are.
  matrix-sdk and matrix-sdk-ui 0.19.1 (and 0.20.0) build for the browser (feature `js`;
  upstream CI checks it) with the IndexedDB stores (state, crypto, event cache, media), as
  Element Web's encryption does.
- **What does not:** `serve`, `mcp`, the agent socket; `$EDITOR`, translate and summary
  commands; the local search index (`experimental-search`); mpv and voice recording; terminal
  graphics; D-Bus notifications and the keyring; files in the profile directory. Each gets a
  web counterpart or is left out (browser notifications, HTML audio, half-block pictures
  first, the browser's storage, OAuth login by redirect).
- **The work:** separating the app from the terminal and the OS behind small interfaces
  (2026-11-26: crossterm's key types in 45 files, `std::fs` in 67, processes in 57,
  `tokio::spawn` in 38 and its timers in 470 places, `Instant`/`SystemTime` in 52, which
  panic in the browser). The terminal app stays the same at every step.
- **ratzilla's gaps for phones** (0.3.1): only `keydown` and mouse events, so no paste, touch
  or input-method typing; its issue #192 (input dead after a resize; the phone keyboard
  resizes the page). Ratatui's contribution rules accept AI-assisted PRs with disclosure.
- **Phones:** a narrow layout, an HTML key bar
  (Esc, Tab, Ctrl, Alt, arrows, palette), the composer as a native text field (autocorrect,
  dictation, any language), tap = click, long press = right-click, swipe = scroll; installed
  from Safari (Add to Home Screen) or Chrome (Install app); offline start by a service worker
  that updates only when the user confirms.
- **Data:** the phone talks to the homeservers directly; GitHub Pages serves only the app's
  files (built by CI from release tags, checksums published). Fonts bundled, no analytics, a
  strict content security policy. No unlock prompt (decided).
- **Notifications while closed** (later: "Keep notifications for later", the user, 2026-11-26): a web app can only be woken by its browser's push
  service (Apple, Google, Mozilla) with keys of its own; that needs a small gateway from the
  homeserver's push to Web Push (Sygnal can do it, or a few dozen lines), reachable from the
  internet. ntfy's built-in Matrix gateway hands the raw push to an ntfy topic for UnifiedPush
  apps on Android; it cannot reach the web app. Content-free either way (`event_id_only`).
  Until then: notifications while the app is open; Beeper on the phone notifies meanwhile.
- **Phases:** 0 a test build beside 1.1.0 (login, room list, one room; on an iPhone and an
  Android phone; answers: does Beeper's server allow browser requests (CORS), how big is the
  download, how fast on a phone, does matrix.org's OAuth accept the Pages address); 1 the
  interfaces; 2 the web app; 3 the phone version; 4 later: real pictures, voice, push, app
  stores. **Layout from phase 2 on** (the user, 2026-11-27: "use those names"): one repository,
  a Cargo workspace of three crates: `matrix-rat`, the terminal app (the name stays, so do the
  binary, the Nix and NUR package and the release archives); `matrix-rat-engine`, the core both
  apps use (phase 1's `engine` module); `matrix-rat-web`, the browser shell. Prefixed so they
  collide with nothing (all three names free on crates.io on 2026-11-27); all
  `publish = false`: nothing of it is for crates.io (the engine has no stable interface for
  others, the web crate is built into the site, the app installs from the release archives
  and the packages). **Versions** (the user, 2026-11-27): 1.2.0 is phase 2, the web app usable in
  browsers and on phones at least as the test build is; 1.3.0 is phase 3, the phone version. The phone-friendly keys come with 1.2.0
  already (the user): the key bar (Esc, Tab, Ctrl, Alt, arrows, palette) and every action
  reachable without Esc, Ctrl or Alt. **1.3.0 also** (from matrix-sdk 0.20, the user,
  2026-11-27): who may invite you (`m.invite_permission_config`, Matrix 1.19; servers' support
  checked first); image packs through the SDK (`image_packs()`, in place of our own loader) and
  "this pack in all rooms" (`set_image_pack_enabled_globally`); in room info the spaces a room
  belongs to (`joined_ancestors_of`) and the server's retention policy (MSC1763); in the doctor
  screen the disk the caches and the search index take (`get_store_sizes`, `media_metrics`).
- **Phase 0 on the user's iPhone** (2026-11-27): OAuth login to matrix.org works in Safari
  and in the home-screen app (which needs its own login: iOS keeps its storage apart from
  Safari's, and it is a device of its own); sending works; still logged in after the app was
  closed for an hour; old messages decrypt after the recovery key. Firefox on Windows, behind
  a network with restrictions, works too, old messages decrypted. For phases 2 and 3, from
  the test:
  - **The on-screen keyboard squashes the chat:** the grid measures itself only when the
    window resizes, and iOS shrinks only the visual viewport for the keyboard, so rows are
    cut off (the input field is right). The grid follows the visual viewport: fewer rows
    while the keyboard is open.
  - **A Send button** next to the message field (the user): with the keyboard closed, a
    message is sent without opening it again. Enter keeps sending.
  - **The version in the top bar** (the user, for testing): the test build shows its commit
    and build time; the web app shows its release number there, so after an update (the
    service worker's "Update available, reload?", confirmed by the user) it is plain which
    version runs.
  - **Second phone test** (2026-11-27, test build `acec2e6`, iPhone 16 Pro): the Send button
    works with the keyboard open (it stays open) and closed (it stays closed); turning,
    zooming and scrolling behave; with a hardware keyboard and an external screen on a
    Thunderbolt dock it works and looks good. Left over:
    - Once the chat's text stuck too high after the height changed, until Older or Newer was
      pressed: the view's scroll position is fitted again on every change of the grid's height.
    - With the keyboard open, only the field is left on that phone ("You can't design away
      that iOS flaw", the user). Phase 3: while the keyboard is open, no top bar or room list,
      only the last 2-3 messages above the field.
    - A swipe moves the whole page and nothing else: the page stays put (no overscroll), and
      a swipe in the chat scrolls the messages (swipe = scroll, above).

### 6.2 Later, after 1.0.0 or by demand

- **Calls.** Voice calls wait until the SDK can join calls itself; with them a line in the
  room saying that a call is running, and a key that opens it in the browser. Video after
  that, if at all.
- **QR login.** matrix-sdk 0.20 has it behind `unstable-msc4388`; later (the user,
  2026-11-27).
- **Replacing the picture or file of a sent message** by an edit (matrix-sdk 0.20,
  `edit_with_attachment`): 1.4.0 at the earliest, if it proves useful (the user, 2026-11-27).
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
- **Shared rooms in a person's info on a fresh login** (`matrix-agy`, 2026-11-27): the bounded
  look-up (`collect_shared_rooms`, 12 rooms a time) sorts rooms whose member count is not known
  yet (0) before small known ones, and loads the whole member list of such a room, which may be
  large. Asking for only the one member instead fills no store, so asking again would not get
  past the first 12: it needs remembering which rooms were checked. Also: room info's own name
  can be empty on a fresh login although the member list has it.

### 6.3 Not wanted or not pursued

- **`--mobile`, a mode for ssh from a phone** (planned for 1.1.0 on 2026-11-21, dropped by the
  user on 2026-11-27: "Drop --mobile"): the web app on phones serves that better, talking to the
  homeserver directly; its key bar and every action without Esc, Ctrl or Alt come with 1.2.0,
  the narrow layout with 1.3.0. The client survey of 2026-11-22 (iSH, gterm, VVTerm) is in
  this file's history.
- **By the user's decision** (2026-11-20 unless dated otherwise): a plugin system; sticker
  packs beyond the picker that exists; an image generator; a lock screen; popping out a
  chat; several accounts in one window (one window per `--profile` instead); feeds, Home
  Assistant and the like posting into rooms (a server's job); an update notice; a demo mode
  (2026-11-16); more picture protocols (2026-11-15).
- **Sending later and sticky events** (matrix-sdk 0.20: delayed events, MSC4140; MSC4354):
  scratched by the user on 2026-11-27.
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
- `chrono` was declared unmaintained on 2026-11-17 (RUSTSEC-2026-0543; no known flaw).
  matrix-sdk-ui moved to `jiff` with 0.20, but `oauth2` (through matrix-sdk) still uses it, so
  it stays in the tree; `cargo deny` is told so. Our own use (a message's local time,
  fourteen places) can move to `jiff`, which the SDK brings now.
- **The SQLite built into the release archives** up to 1.0.3 (through matrix-sdk 0.19.1) is
  older than 3.54.0 and has a known flaw (RUSTSEC-2026-0459): opening a database whose `-wal`
  file has another page size can hand out stale memory. It takes someone who can write into
  the profile's private folder, who could read the store there anyway. Fixed with 1.1.0:
  matrix-sdk 0.20 takes rusqlite 0.41, whose libsqlite3-sys 0.39.1 bundles SQLite 3.54.1;
  the `deny.toml` entry is gone. The Nix package links the system's SQLite, as before.
  One more advisory only says "unmaintained" (bincode 1, through syntect); anymap2 left the
  tree with matrix-sdk 0.20 (it took anymap3).
- **Loading older messages can panic inside matrix-sdk** (matrix-rust-sdk #5416, "The chunk
  is not found", still open; not named as fixed in 0.20.0): `matrix::paging::backwards`
  makes it a failed load, clears the event caches at most every five minutes, and is the
  only caller of `Timeline::paginate_backwards` (`clippy.toml`). The cause seen on the dev
  homeserver with 0.19.1, all caches cleared at the start of every session of an account
  with an ignore list, is gone with 0.20 (matrix-rust-sdk #7397); the catch stays until the
  panic no longer shows on the dev homeserver for a while.
- **The test homeserver's own fault**: continuwuity now and then answers a sync with an
  error ("pdu should have a shortstatehash"), in bursts, and a server-backed test that waits
  for something then times out (seen in the threads, people and media tests, on `main` as
  on a branch). A second run is the remedy; a fresh database for it may be worth trying.
- **A block of messages twice in a loaded timeline**: once, in the archive's server test, four
  messages of a flood stood twice, right after themselves, while sync still brought the newest
  ones. Archive and catch up count an event once. `matrix-agy` traced it to matrix-sdk-ui 0.19.1:
  a live timeline built while sync brings a batch gets that batch twice. The room view can show it
  too; fixed on our side (`dup-block`, merged 2026-11-21). matrix-sdk-ui 0.20.0 fixes the race
  (PR #7284 in its changelog), and matrix-sdk 0.20.0 no longer sends a timeline's updates out
  of order when sync, the send queue and paging from the store meet (#7468), another way to a
  message twice. Our fix stays as a safety net: 0.20.0 can still apply updates twice when a
  timeline falls more than 32 updates behind (`RecvError::Lagged` reloads the events but keeps
  the old receiver; checked 2026-11-21 on `main` and again in the 0.20.0 source on 2026-11-27,
  matrix-rust-sdk issue #7533, not named as fixed).
  Seen once more on 2026-11-23, in the same test on the 1.0.2 release commit: after a flood,
  three messages stood again after forty newer ones, and keeping the later copy in place made
  them the "newest". A server test that collects again and again while 1500 messages come in
  caught the shape (in 11 of 49 collections): the event cache held each message once and in
  order, and the timeline had the newest few again after them. Since 1.0.3 what is collected
  keeps each event at its first copy's place, as the room view does.

### The agent channel (as of 1.0.0)

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
  left to `read_messages` (as designed), also those it owed. Only one that was handed over
  before and edited since comes again with the next wait.
- Seen once and not again in seven tries: a `read` right after the first `send` of a fresh
  `serve` listed the new message twice.
- A config file with a bad value (0.8) has not been tried by the user yet.
