# matrix-rat — implementation plan

A mouse-friendly, themeable, image-capable Matrix client for the terminal, built on ratatui.

Status: **revision 2** (2026-11-12) — reviewed by `matrix-agy`, verdict *approve with changes*;
all eight findings are folded in (see §14 and `docs/reviews/PLAN-review.md`).
Versions quoted below were checked on crates.io on 2026-11-12; items marked *(verify)* came from
research that could not be confirmed at source and must be checked in the milestone that uses them.

## 1. Goals

| Goal | What "done" means |
|---|---|
| Fixed, readable layout | Room list sidebar on the left, timeline on the right, composer at the bottom, status line. No vim window tree. |
| Mouse first-class | Click to focus any pane, click a room to open it, wheel scrolls the pane under the pointer, drag the sidebar splitter, click links/images/buttons. Everything also reachable by keyboard. |
| Logical keybinds | Non-modal by default (type to write, `Ctrl`/`Alt` chords to navigate), discoverable through a help overlay and a command palette, fully rebindable. |
| Options menu | In-app settings screen for account, appearance, notifications, media, keybinds, encryption. Writes back to the config file. |
| Modern theming | Semantic theme files, bundled popular palettes, a "terminal" theme that inherits ANSI colors, dark/light auto-switch, live reload, and templates so Noctalia / matugen / DMS / wallust / pywal can recolor the app automatically. |
| Images in the terminal | Inline thumbnails in the timeline (kitty / sixel / iTerm2, halfblock fallback), a full-size viewer, avatars. |
| Polish | Spinners, progress bars for sync / upload / download, toasts, rounded borders, optional subtle transitions. |
| Correct Matrix | E2EE with verification and recovery, OAuth (MAS) login, sliding sync, replies, edits, reactions, redactions, threads, spaces, receipts, typing. |

Non-goals for v1: voice/video calls, widgets, multi-account in one window (multiple profiles are fine), room administration beyond basics.

## 2. Decision: write from scratch, borrow from iamb

Forking iamb (Apache-2.0, 0.14.0-alpha, ~31k LOC) was evaluated and rejected:

- Its UI is built on `modalkit` (vim modes, window tree, command bar). 18 source files depend on it,
  and the scrollback, composer, commands and keybindings are all modalkit types. Replacing that with
  a fixed layout and click-to-focus means rewriting 10k+ LOC of core UI.
- Mouse support is scroll-only; every other mouse event is dropped.
- It keeps its own timeline (`BTreeMap` of messages on classic `/sync`) and does not use
  `matrix-sdk-ui`. No sliding sync; slow startup and high RAM are long-standing issues.

What we do take from iamb, adapted to our types, with attribution in `NOTICE`:

| iamb source | Use |
|---|---|
| `src/message/html.rs` | Matrix HTML → styled ratatui `Text` (lists, links, tables, code). Highest-value reuse. |
| `src/message/printer.rs` | Message wrapping/layout reference. |
| `src/preview.rs` | Picker setup, fallback logic, download/resize pipeline for images. |
| `src/oauth.rs`, `src/worker.rs` (login, store, backup setup) | Reference for SDK setup and login flows. |
| `src/verifications.rs`, `src/windows/verify.rs` | SAS flow reference. |
| `src/config/theme.rs`, `themes/*.toml` (~65 palettes) | Palette values for bundled themes (converted to our schema). |

License for matrix-rat: **Apache-2.0** (matches iamb and matrix-rust-sdk; needed for the borrowed code).

## 3. Stack

| Concern | Choice | Notes |
|---|---|---|
| Matrix | `matrix-sdk` 0.19.1 + `matrix-sdk-ui` 0.19.1 | Features: default (`e2e-encryption`, `sqlite`, `rustls-aws-lc-rs`) + `bundled-sqlite`, `sso-login` (implies `local-server`), `socks`, `qrcode`. MSRV 1.96; machine has rustc 1.99.1. Pin exact versions — every minor release breaks APIs. |
| Sync / rooms / timeline | `SyncService`, `RoomListService`, `Timeline` from `matrix-sdk-ui` | Same stack as Element X. Gives sliding sync, `VectorDiff` streams, local echo, send queue, date dividers, read marker, thread focus. |
| TUI | `ratatui` 0.30.2, `crossterm` 0.29 (`event-stream`) | |
| Async | `tokio` multi-thread | |
| Images | `ratatui-image` 11.1.0, `image` 0.25 | `Picker::from_query_stdio()` once before the event stream starts. |
| Composer | `ratatui-textarea` 0.9.4 | Successor to tui-textarea. Soft-wrap and mouse cursor placement *(verify)*; fall back to a small own widget if missing. |
| HTML / markdown | `html5ever` (via borrowed renderer), `comrak` for compose (see §6a) | |
| Code highlighting | `syntect` 5.3 + `two-face` | Lazy-loaded. |
| Text utils | `unicode-width`, `unicode-segmentation`, `linkify`, `emojis` | |
| Fuzzy | `nucleo-matcher` | Room switcher, command palette, completions. |
| Config | `serde` + `toml` 1.x, `toml_edit` for write-back, `etcetera` for paths | |
| Keybind parsing | `crokey` | `"ctrl-k"` strings ↔ key combinations. |
| Theme extras | `notify` 8 (file watch), `terminal-colorsaurus` (dark/light), `signal-hook` (SIGUSR1) | |
| Desktop | `notify-rust` (notifications), `arboard` + OSC 52 (clipboard), `keyring` for the store passphrase *(verify choice)* | |
| Eye candy | `throbber-widgets-tui` 0.11 (or a small own braille spinner), built-in `Gauge`/`LineGauge`, `tachyonfx` (optional, off-switch in settings) | |
| Logging / tests | `tracing` + `tracing-appender` (file only), `insta` + `TestBackend` | |
| Dev env | `shell.nix` (user is on flakeless NixOS) with rust toolchain, `pkg-config`, C toolchain for aws-lc-rs/sqlite | |

## 4. Architecture

Single binary crate to start (`src/` modules below); split into a workspace only if compile times demand it.

```
src/
  main.rs            arg parsing, terminal init/restore, panic hook
  app.rs             App state, event loop, Action dispatch
  action.rs          Action enum (every user-visible command), names for keybinds/palette
  event.rs           AppEvent: Term(crossterm) | Matrix(MatrixEvent) | Tick | ThemeChanged | ...
  matrix/
    mod.rs           MatrixHandle (cmd sender) + worker task
    session.rs       login (password, SSO, OAuth), restore, token-refresh persistence
    rooms.rs         RoomListService stream → RoomListUpdate
    timeline.rs      per-room Timeline subscription → TimelineUpdate, send/edit/react/redact
    media.rs         thumbnail/full fetch, upload with progress
    crypto.rs        verification (SAS), recovery, backup state
  ui/
    mod.rs           root draw(), layout, focus ring
    hit.rs           HitMap: per-frame (Rect, HitId, z) registry + lookup
    sidebar.rs       room list (spaces, sections, unread badges, avatars, filter)
    timeline.rs      virtualized message list, selection, scroll anchoring
    message.rs       one message → lines (header, body, reactions, reply quote, media slot)
    html.rs          Matrix HTML → Text (adapted from iamb)
    composer.rs      textarea, reply/edit banner, completions popup, attachment chips
    statusline.rs    sync state, typing, E2EE shield, key hints
    overlay/         settings, command palette, room switcher, help, image viewer,
                     verification, login, emoji/reaction picker, toasts, context menu
    widgets/         avatar, badge, progress, spinner wrappers
  theme/
    mod.rs           Theme (semantic roles → Style), resolution, color-depth downgrade
    load.rs          TOML parse, inherits, built-ins (include_str!), user dir
    watch.rs         notify watcher + SIGUSR1 + dark/light detection
    integrations/    template files (noctalia v4/v5, matugen, wallust), pywal reader
  config/            Config struct, defaults, load, write-back, keymap
  images/            picker, decode/resize pool, protocol cache (LRU)
  notify.rs          desktop notifications, bell, terminal title unread count
```

### Data flow

- **UI thread owns all UI state.** No shared mutex store (iamb's design). The Matrix worker runs on
  tokio and talks to the UI over channels:
  - UI → worker: `MatrixCmd` (`OpenRoom`, `Send`, `Paginate`, `ToggleReaction`, `FetchMedia`, …).
  - worker → UI: `MatrixEvent` (`RoomList(Vec<VectorDiff>)`, `Timeline{room_id, generation, diffs}`,
    `SyncState`, `Media{id, result}`, `UploadProgress`, `Verification(..)`, `Error(..)`).
    The UI drops timeline batches whose `generation` is not the current subscription's.
- **Session init order:** restore/login → `client.event_cache().subscribe()` *(verify it is still
  mandatory in 0.19.1)* → start `SyncService` → build timelines.
- **Timeline lifecycle:** the worker keeps a bounded LRU (5–8) of live `Timeline`s so switching back
  to a recent room is instant and keeps its pagination; only the visible room's diff stream is
  forwarded to the UI.
- **Sidebar freshness:** besides the room-list diff stream, observe per-room info (unread counts,
  name, avatar, latest event) so badges and previews never go stale.
- The UI keeps lightweight view models (`RoomEntry`, `MsgItem`) built from the diffs; SDK types do
  not leak into `ui/` beyond IDs.
- **Event loop:** `tokio::select!` over crossterm `EventStream`, the `MatrixEvent` channel, and a
  tick (~30 fps while animations/spinners are active, idle otherwise). Draw only when dirty.
- **Actions:** every key, mouse click, palette entry and menu item resolves to an `Action`. One
  dispatch path means keybinds, mouse and palette can never drift apart.

### Mouse

- `HitMap` is cleared and refilled during each draw: widgets register `(Rect, HitId, z)`.
  Mouse events are resolved against the last frame's map, top-most first, so overlays win.
- `HitId` examples: `Pane(Sidebar)`, `Room(idx)`, `Message(idx)`, `Link(msg, n)`, `Image(msg)`,
  `Reaction(msg, key)`, `Splitter`, `ScrollbarThumb(pane)`, `Button(id)`, `SettingsRow(n)`.
- Behaviors: left click focuses + activates; wheel scrolls the hovered pane without changing focus;
  drag on the splitter resizes the sidebar (persisted); hover highlights rows; right click opens a
  context menu (reply / edit / react / copy / redact).
- Text selection: mouse capture blocks native selection, so we provide copy actions (message, link,
  code block) and document Shift-drag. A setting disables mouse capture entirely.
- Links are clickable through the `HitMap` (hover underline, URL shown in the status line).
  OSC 8 hyperlinks are emitted only when mouse capture is off, and only if it can be done without
  corrupting cell widths *(verify)*.
- **Startup order (strict):** raw mode → `Picker::from_query_stdio()` (timeout → halfblocks) →
  dark/light query → alternate screen + mouse capture + keyboard enhancement → `EventStream`.
  Terminal queries never run once the event stream exists.
- **`TerminalGuard` (RAII):** one place that enters and restores the terminal (pop keyboard
  enhancement flags, disable mouse capture, leave alternate screen); used by normal exit, the panic
  hook and `SIGTSTP`/`SIGCONT`.

### Timeline rendering

- Virtualized: only lay out what is near the viewport. Each `MsgItem` carries the SDK's
  `unique_id` (exists for local echoes and virtual items too), a `content_rev` bumped on every
  `VectorDiff::Set` and on media/avatar decode completion, and a `show_header` flag recomputed for
  neighbours whenever diffs are applied. Line cache key:
  `(unique_id, content_rev, show_header, width, theme_rev)`.
- Scroll position is anchored to a `unique_id`, not a line offset, so pagination and incoming
  messages don't jump the view.
- HTML is parsed into an intermediate rich document (blocks/spans tagged `Link`, `CodeBlock`,
  `Mention`, `Spoiler`, quote depth). We wrap it ourselves (`unicode-width`/`-segmentation`) and
  produce both `Line`s and item-relative hit rects, which the draw pass offsets into the `HitMap`.
  `Paragraph::wrap` is not used for messages because it cannot report span positions.
- "Jump to bottom" pill with unread count when scrolled up; read marker line; date dividers;
  grouped consecutive messages from one sender; per-user stable name colors from the theme palette.
- Back-pagination at the top shows a spinner row.

### Images

- Detect protocol once at startup; setting to force `kitty | sixel | iterm2 | halfblocks | off`.
- Pipeline: timeline item with media → placeholder box (blurhash-tinted if available, with spinner)
  → fetch thumbnail via SDK media API (cached in SDK media store) → decode + resize on a blocking
  pool → cache encoded `Protocol` in an LRU keyed by `(mxc, cell size)` → render.
- Keep the decoded, downscaled `DynamicImage` in a memory-bounded LRU alongside the encoded
  protocol. Fully visible image → cached protocol. Clipped by the top/bottom edge → halfblocks
  (clips cleanly per cell) or a re-encoded crop / ratatui-image sliced rendering if that proves
  good enough *(verify)*. A graphics image must never draw outside the timeline area.
- Click / `Enter` on an image opens a full-screen viewer overlay (fit, zoom, save, open externally).
- Avatars: colored initials by default; real images when a graphics protocol is available (setting).
- tmux/zellij: detect and warn; document `allow-passthrough`.
- User's terminal is kitty, so the kitty protocol is the primary test target.

### Progress and feedback

- Status line spinner + text for sync states (`SyncService` state, `RoomListService` loading state).
- First-sync screen with a progress bar and step labels.
- Uploads: real progress from the SDK's `TransmissionProgress` observable, shown as a gauge chip
  above the composer with cancel. Downloads: the SDK exposes no progress observable, so show an
  indeterminate bar unless we stream it ourselves *(verify)*.
- Toasts for errors and background events; never silently drop a failed send (retry / discard).

## 5. Layout

```
╭ Rooms ──────────────╮╭ #rust:matrix.org ─────────────── 🔒 ─ 42 members ╮
│ 🔍 filter…          ││ ── Today ──────────────────────────────────────── │
│ ▾ Favourites        ││ ● alice                                    10:02  │
│   # rust        (3) ││   Has anyone tried the new event cache?           │
│ ▾ People            ││   ╭──────────────╮                                │
│   ● bob          •  ││   │   (image)    │   👍 2  🎉 1                   │
│ ▾ Rooms             ││   ╰──────────────╯                                │
│   # matrix-rat      ││ ● bob                                      10:05  │
│ ▸ Spaces            ││   ┃ alice: Has anyone tried…                      │
│                     ││   Yes — startup is instant now.                   │
│                     │├──────────────────────────────────────────────────┤
│                     ││ ↩ Replying to bob                             ✕   │
│                     ││ > type a message…                                 │
╰─────────────────────╯╰──────────────────────────────────────────────────╯
 ● synced  alice is typing…                    Ctrl-K switch · F1 help · F2 settings
```

- Sidebar is resizable and collapsible; below ~60 columns it auto-collapses to an overlay.
- Optional right panel (member list / room info / thread) toggled per room.
- Composer grows with content up to a max height.

## 6. Keybinds (deliberately few, all rebindable)

The normal flow needs almost no keys: click a room (or pick it with the arrows), the chat opens
and the cursor is already in the composer. Type, `Enter` sends.

| Key | Action |
|---|---|
| `Enter` / `Shift-Enter` | Send / line break (`Alt-Enter` as fallback where the terminal cannot tell them apart) |
| `Tab` / `Shift-Tab` | Cycle focus: sidebar → timeline → composer |
| `Alt-←` / `Alt-→` / `Alt-↑` / `Alt-↓` | Jump focus in that direction, matching the screen: `←` room list, `→` back to the chat (composer), `↑` messages, `↓` composer. With the right panel open, `Alt-→` from the chat goes there. Word-wise cursor movement in the composer is therefore on `Ctrl-←` / `Ctrl-→` (and `Alt-B` / `Alt-F`). Zellij grabs `Alt`-arrows by default; `Alt-1…4` and `Tab` are the fallback. |
| `Alt-1` … `Alt-4` | Jump straight to a pane: 1 room list, 2 messages, 3 composer, 4 right panel (opens it if closed). The digit is shown in each pane's border title. |
| `Alt-R` | Jump to the room filter (type to narrow the list, `Enter` opens) |
| `↑` / `↓` in sidebar | Move through rooms; `Enter` opens the room and focuses the composer |
| `↑` in composer (cursor on first line) | Leave the composer and select the newest message; keep pressing to walk up |
| `↓` past the newest message | Back to the composer |
| On a selected message | `Enter` reply · `e` edit (own messages) · `r` react · `Del` delete (own) · `c` copy · `o` open link/image. The same actions appear as a clickable row on the selected message and in the right-click menu. |
| `Esc` | Close overlay → cancel reply/edit → drop selection and return to the composer |
| `PgUp` / `PgDn` | Scroll the timeline |
| `Alt-E` (or click the ☺ at the right of the composer) | Emoji picker: search field focused, type to fuzzy-search, arrows move through the grid, `Enter` or a click inserts at the cursor and closes |
| `Ctrl-K` | Room switcher (fuzzy) |
| `F1` (or `?` outside the composer) | Help (lists every key) |
| `F2` (or `Ctrl-,`) | Settings |
| `Ctrl-Q` | Quit |

- Message-selection mode is unmistakable: the selected message is highlighted, the composer is
  visibly dimmed, and the action hints are shown in the status line. Unbound letters do nothing
  there (they are never typed into the composer by surprise). Status-line hints are clickable.
- Everything else (attach, toggle sidebar, command palette, next unread) is reachable by mouse,
  slash command or the palette; it gets a key only if the user binds one.
- Keymap is context-scoped (`global`, `sidebar`, `timeline`, `composer`, `overlay`) in config.
- Optional `vim` preset (`j/k`, `gg/G`, `i` to enter the composer); off by default.
- Kitty keyboard protocol is enabled when available so `Shift-Enter` is distinguishable.

## 6a. Writing formatted messages

Matrix carries formatting as HTML (`formatted_body`, `org.matrix.custom.html`) next to a plain-text
`body`. Bridges (mautrix-whatsapp, -signal, -telegram, -discord, …) translate that HTML into the
remote network's own formatting, so anything we send as correct Matrix HTML also arrives formatted
on bridged services. The question is only how the user types it. We support **both** styles, the
way Element does — they do not conflict:

**1. Markdown as you type (default, Beeper/Discord style)**

| Typed | Result | HTML sent |
|---|---|---|
| `**bold**`, `*italic*`, `~~strike~~` | bold, italic, strikethrough | `<strong>`, `<em>`, `<del>` |
| `` `code` ``, fenced ``` blocks with language | inline code, highlighted block | `<code>`, `<pre><code class="language-x">` |
| `\|\|spoiler\|\|` | hidden until clicked | `<span data-mx-spoiler>` |
| `> quote`, `- list`, `1. list`, `# heading`, `[text](url)` | as expected | standard tags |
| `@name`, `#room` (via completion) | mention pill | `matrix.to` link + `m.mentions` |
| `:emoji:` | emoji character | — |

- Engine: `comrak` with `default-features = false` (its defaults pull in a CLI and the C
  Oniguruma library). We post-process its output for Matrix: spoilers become
  `<span data-mx-spoiler>` (comrak emits `class="spoiler"`) and the outer `<p>` is stripped from
  single-paragraph messages. The SDK's own `markdown` feature is not used.
- Extensions (CommonMark + GFM): its `strikethrough`, `spoiler` (`||…||`), `autolink`
  and `tasklist` extensions — this replaces `pulldown-cmark` in §3. Raw HTML in input is escaped.
- If the text contains no formatting, only a plain `body` is sent (no redundant HTML).
- A backslash escapes a marker (`\*not italic\*`).

**2. Slash commands (Element style)** — typing `/` opens a completion popup with descriptions.

| Command | Effect |
|---|---|
| `/me <text>` | Emote |
| `/spoiler <text>` | Whole message as spoiler |
| `/plain <text>` | Send exactly as typed, no Markdown |
| `/html <html>` | Send raw HTML (sanitised to Matrix's allowed tags) |
| `/shrug`, `/tableflip`, `/lenny` | Prepend the classic text faces |
| `/rainbow <text>` | Rainbow-coloured text |
| `/react <emoji>`, `/upload <path>`, `/join`, `/leave`, `/invite`, `/nick`, `/topic` | Client actions |

`//text` sends a message that starts with a literal slash.

**Composer feedback:** Markdown markers are lightly syntax-coloured while typing (bold shown bold,
spoiler dimmed), and a Markdown on/off toggle lives in Settings → Composer. The timeline renders
received spoilers as a covered block that reveals on click or `Enter`.

## 7. Settings menu

Overlay with categories on the left, rows on the right; mouse and keyboard driven; changes apply
live and are written back with `toml_edit` (preserving user comments).

- **Account** — profile, display name, devices/sessions, logout.
- **Appearance** — theme picker with live preview, dark/light pairing, follow-system toggle,
  density (compact/comfortable), timestamps format, nerd-font icons on/off, animations on/off,
  rounded borders, avatar style, sidebar width/sections.
- **Notifications** — desktop notifications, bell, mentions-only, per-room overrides.
- **Media** — image protocol, inline previews on/off, max preview size, auto-download limits,
  cache retention, external opener.
- **Keybinds** — list by context, rebind by pressing the key, conflict warning, preset switch.
- **Encryption** — device verification status, verify this device, recovery key entry/setup,
  key backup state.
- **Advanced** — log level, mouse capture, proxy, open config file / log dir.

## 8. Theming

### Theme files

- Location: built-ins compiled in; user themes in `~/.config/matrix-rat/themes/<name>.toml`.
- Schema: a small `[palette]` of named colors plus **semantic roles** that reference them, so a
  generator only has to fill ~20 values:

```toml
name = "noctalia"
inherits = "base-dark"        # optional
[palette]
bg = "#171210"
surface = "#221a17"
fg = "#f1dfd8"
muted = "#a08d85"
surface_high = "#2d2420"      # optional
outline = "#53433d"           # optional
primary = "#f9b991"
on_primary = "#4b2707"        # optional, defaults to bg; guarantees readable badges
secondary = "#e5bfa8"
accent = "#cbc891"
error = "#ffb4ab"
[ui]
border = "muted"
border_focused = "primary"
selection = { bg = "surface", fg = "primary" }
unread_badge = { bg = "primary", fg = "on_primary" }
mention = { fg = "accent", bold = true }
# … status line, composer, scrollbar, toasts, progress, code block, link, quote, timestamps
[users]                        # sender name colors, cycled by hash
colors = ["primary", "secondary", "accent", "#…"]
```

- Values: `#rrggbb`, ANSI names / indices, `"reset"`, or a palette key. Unknown keys warn, never crash.
- Bundled: catppuccin (4), tokyonight, gruvbox, nord, rosé pine, dracula, kanagawa, everforest,
  solarized, one-dark — palettes converted from iamb's theme set — plus a house default.
- **`terminal` theme:** only ANSI 0–15 and `Reset`. Works with every shell/generator that already
  themes the terminal itself, and is the zero-config answer to "auto-colored".
- Color depth: detect truecolor via `COLORTERM`; quantize to 256 colors otherwise.

### Dark / light and live reload

- `theme = { dark = "…", light = "…" }` pairing; mode detected at startup with
  `terminal-colorsaurus` (OSC 11, before the event stream starts). Live mode switches arrive via
  the generator rewriting the theme file or `SIGUSR1`; terminal mode 2031 reports are a stretch
  goal only, because they collide with crossterm's input parsing.
- Hot reload: `notify` watcher (debounced) on the **parent directories** (config dir and
  `themes/`), filtered by filename, so atomic write-then-rename by generators and editors keeps
  working. Plus `SIGUSR1`.
- `matrix-rat --reload-theme` signals running instances found through
  `$XDG_RUNTIME_DIR/matrix-rat/<profile>.pid` files (removed on exit).

### Desktop-shell integration

Shipped under `contrib/themes/` and installable with `matrix-rat theme install-template <target>`
(prints or writes the registration snippet; never edits another tool's config without confirmation):

| Target | What we ship |
|---|---|
| Noctalia v5 (user's setup, TOML config) — **primary target** | Template using `{{colors.primary.default.hex}}` tokens (incl. v5's `hover` and `terminal_*` tokens for the user-colour list) + `[theme.templates.user.matrix-rat]` snippet for `~/.config/noctalia/templates.toml` with `post_hook = "matrix-rat --reload-theme"`; `input_path_modes` for dark/light. Tested live on the user's machine. Later submit to the community template registry. |
| Noctalia v4 (legacy) | Same template body + `[templates.matrix-rat]` snippet for `~/.config/noctalia/user-templates.toml`. Untested locally. |
| matugen (also DMS, end-4, Caelestia) | Tera-syntax template + `[templates.matrix-rat]` config snippet *(verify syntax)*. |
| wallust | Template + config entry. |
| pywal / pywal16 | Built-in `theme = "pywal"` that reads `~/.cache/wal/colors.json` directly and watches it. |
| base16 / tinty | `theme = "base16:<path>"` loader mapping base00–0F to roles (stretch). |

Generators all write the same output: `~/.config/matrix-rat/themes/<generator>.toml`. The file
watcher picks it up, so recoloring is instant even without the post-hook.

## 9. Matrix feature scope

| Area | v1 | Later |
|---|---|---|
| Login | OAuth/MAS via localhost redirect (`local-server`), password, SSO; session restore + refresh-token persistence | QR login |
| Sync | `SyncService` (sliding sync); offline start from cache | |
| Rooms | list with sections (favourites, people, rooms, low priority), spaces filter, unread/mention badges, latest-event preview, join/leave/invite handling | room directory, create room |
| Timeline | text, formatted HTML, replies, edits, reactions, redactions, images/files/video-as-file, date dividers, read marker, receipts, typing | polls, pinned events, search, location |
| Threads | open thread in right panel (`TimelineFocus::Thread`) | thread list |
| E2EE | sqlite crypto store, SAS emoji verification, recovery key entry, key backup, shields on messages | cross-signing reset, QR verify |
| Media | inline thumbnails, viewer, download/save, upload with progress | galleries, GIF animation |
| Profiles | `--profile <name>` separate store/config | |

OAuth dynamic registration needs a `client_uri` — `https://github.com/tylerstyle/matrix-rat`.

One instance per profile: an exclusive `flock` on `<data_dir>/<profile>/lock` is taken before the
SDK store is opened; a second instance exits with a clear message.

Secrets: session tokens in the data dir with `0600`; sqlite store passphrase in the system keyring
with a file fallback when no keyring service is available.

## 10. Milestones and review gates

Each milestone ends with: `cargo fmt`, `cargo clippy -D warnings`, tests green, a short demo note,
then **review by `matrix-agy` before the next milestone starts**.

| # | Milestone | Deliverable |
|---|---|---|
| M0 | Scaffold | `shell.nix`, Cargo project with pinned deps (prove the SDK builds here), CI-style check script, logging, `TerminalGuard`, profile lock, `LICENSE`/`NOTICE`. |
| M1 | UI shell with fake data | Layout, focus ring, `HitMap` mouse (click focus, wheel, splitter drag), Action/keymap system, theme engine with 3 built-ins + `terminal`, help overlay, snapshot tests. No network. |
| M2 | Login + sync + room list | Worker task, OAuth/password login screen, session restore, `RoomListService` → sidebar with badges and previews, first-sync progress, status line. Recovery-key entry (`recovery().recover`) so encrypted rooms decrypt on real accounts from here on. |
| M3 | Timeline + composer | Live timeline via `Timeline` diffs, virtualized rendering, HTML rendering, pagination, send (Markdown + slash commands, §6a), spoiler rendering, local echo/retry, replies, edits, reactions, redactions, receipts, typing. Graceful "unable to decrypt" items. |
| M4 | E2EE | Verification overlay (SAS), recovery setup/management UI, backup status, device list, shields. |
| M5 | Media | Image pipeline, inline thumbnails, viewer, avatars, upload/download with progress, file open/save. |
| M6 | Settings + keybind editor + palette | Settings overlay with write-back, command palette, room switcher, completions, context menu. |
| M7 | Theming integrations | Full bundled theme set, hot reload, dark/light follow, Noctalia/matugen/wallust templates, pywal reader, `theme install-template`. |
| M8 | Spaces, threads, notifications, polish | Space navigation, thread panel, desktop notifications, animations, README with screenshots, packaging (`default.nix`). |

### M5 part 2: files and saving attachments (requested 2026-11-13)

- **Shown:** `m.file`, `m.audio` and `m.video` messages appear as one attachment line
  (`📎 name · size · type`), selectable like any message; pictures keep their inline view.
- **Save:** `s` on a selected picture or attachment (or a click on its line / a "save" hint
  in the status line and in the picture viewer) downloads the original file into the
  download folder, with a progress indication on the message and a toast naming the
  saved path. An existing file is never overwritten (`name (1).ext`).
- **Download folder:** one setting, `download_dir`, editable in the settings menu (F2).
  Default: the XDG download directory (`xdg-user-dir DOWNLOAD`), else `~/Downloads`.
- **Open:** `o` on an attachment saves it (if not saved yet) and opens it with the
  desktop's default program.
- **Safety:** the file name comes from the sender, so only its last path component is
  used, control characters are removed, hidden-file dots and empty names are replaced,
  and the write goes to a temporary file that is renamed when complete. Encrypted
  attachments are decrypted by the SDK. A size limit asks before very large downloads.
- **Later:** sending files (`/upload path`, drag-and-drop paste of a path).

M1 before any networking is deliberate: the look, mouse model and theming are the product's
differentiators and can be reviewed with fake data in days.

## 11. Testing

- Unit: HTML renderer, message layout/wrapping, theme resolution, keymap parsing, hit testing.
- Snapshot (`insta` + `TestBackend`): each pane and overlay at several sizes and themes.
- Reducer tests: feed `VectorDiff` sequences and assert view-model state.
- Integration: a local homeserver from nixpkgs (conduwuit/tuwunel or synapse) in a test script for
  login, send, E2EE between two clients. Manual checklist on matrix.org for OAuth.
- Manual terminal matrix for images: kitty (primary), foot (sixel), alacritty (halfblocks), tmux.

## 12. Risks

| Risk | Mitigation |
|---|---|
| matrix-sdk breaks API every minor release | Pin exact versions; keep SDK types inside `matrix/`; upgrade deliberately. |
| Long compile times, C toolchain needs (aws-lc-rs, sqlite) | `shell.nix` provides them; dev profile with optimized deps; consider `mold`. |
| Inline images while scrolling (clipping, sixel last-row scroll bug, multiplexers) | Sliced rendering, placeholder when clipped, protocol override, never draw sixel on the last row. |
| `ratatui-textarea` lacks soft-wrap / mouse placement | Verify in M1; own minimal composer widget as fallback. |
| Mode 2031 / OSC replies colliding with crossterm input parsing | Startup query before `EventStream`; live reports optional; file watcher is the reliable path. |
| Noctalia v4 vs v5 template differences | v5 is primary and tested; v4 snippet shares the template body and avoids v5-only tokens via a separate file if needed. |
| Scope creep | v1 column in §9 is the contract; "Later" items wait. |

### Open requests

- **Notification icon (requested 2026-11-13, done):** a desktop notification shows the picture
  of the chat it comes from; without one, a coloured tile with the capital first letter of
  the room name (the same tile the room list uses). Needs the picture as a file for the
  notification service (a small cache in the runtime directory, cleaned up on exit) and a
  drawn letter for the fallback (no font library: a small built-in bitmap alphabet).
  Terminal notifications (OSC 99) carry no picture and stay as they are.

## 13. Decisions by the user (2026-11-12)

1. Name and binary: `matrix-rat`.
2. Licence: Apache-2.0.
3. Keybinds: minimal, non-modal, mouse-and-arrows first (§6); `vim` preset optional.
4. Formatting input: Markdown plus slash commands (§6a).
5. Repository: public at `https://github.com/tylerstyle/matrix-rat`.

## 14. Review log

| Date | Reviewer | Verdict | Outcome |
|---|---|---|---|
| 2026-11-12 | `matrix-agy` | Approve with changes | All 8 findings accepted: timeline cache key and header recompute (§4), event-cache init + timeline LRU + generation tags (§4), strict startup order and image clipping (§4), recovery-key entry moved to M2 (§10), `sso-login`/`socks` features and spinner pin (§3), own wrapping with hit rects and OSC 8 demoted (§4), parent-dir watching + `on_primary` + pid files + mode 2031 demoted (§8), profile lock and `TerminalGuard` (§4, §9). |
| 2026-11-12 | `matrix-agy` | M0 + plan rev 2: approve with changes | All 5 findings accepted. Fixed in M0: SIGTERM/SIGHUP handling, terminal-restore ordering, runtime-dir fallback, case-insensitive quit, tick behaviour. Plan: word movement on `Ctrl-←/→`, help/settings aliases, clearer selection mode (§6), comrak without default features plus Matrix post-processing (§6a). Ctrl-Z suspend/resume lands in M1. |
| 2026-11-12 | `matrix-agy` | M1: approve with changes | All 6 findings accepted. Fixed now: bounds-checked access and a calm empty state when there are no rooms, per-room drafts (an edit no longer leaks into another room), composer rows never exceed the width with wide characters, tabs/control characters sanitised on insert, clickable status-line hints and a rooms button in narrow mode, foreground inheritance, draft dimmed whenever unfocused, dimmed overlay backdrop. Deferred by design: stable room IDs replace indices in M2, stable timeline item IDs in M3. |
| 2026-11-12 | user | Request | Emoji picker added (button in the composer, `Alt-E`, fuzzy search); built together with M2. |
| 2026-11-12 | `matrix-agy` | M2 + emoji picker: approve with changes | All 5 findings accepted and fixed: saved sessions restore without any network request (offline start shows cached rooms; SDK offline mode reconnects by itself), old stores are set aside instead of deleted on a new login, login can be cancelled or the app quit at any stage, the worker retries after an error, emoji highlight covers both columns, stale state is cleared when a new login is needed, popups never stack, secrets are redacted from debug output, `Ctrl-Z`/`Ctrl-U` work in popups, recovery prompt has clickable buttons. Still untested against a real server: browser (OAuth) login and the recovery-key path. Deviation kept for now: store passphrase in `session.json` (0600) instead of a keyring. |
| 2026-11-13 | `matrix-agy` | M3 part 1: approve with changes | All 6 findings accepted and fixed: read receipts are sent only for messages that were really on screen (drawn, at the newest message, no popup, window focused), once per newest message; history loading knows the start of a room from the timeline itself, runs one request at a time and cannot loop; plain paragraphs send no HTML and `/spoiler` and `/html` handle multi-line input; editing keeps the unsent draft and keeps an action an action; selection never lands on system lines and focusing the pane does not pull the view down; replies to unloaded messages are fetched; message text is kept out of debug output. |
| 2026-11-13 | `matrix-agy` | M3 part 2 + fixes: approve with changes; M3 judged complete | All 6 findings accepted and fixed: text from other people is stripped of control characters and direction overrides before it reaches the terminal (bodies, names, room names, quotes, reaction keys, typing names, link addresses); spoiler detection matches the parser; the row cache is invalidated on any theme load; web addresses in plain messages are links, clicking a link selects its message; editing is not announced as typing and its stash is cleared on re-login or when the room vanishes; nesting depth is capped and table cells are separated. Open by design: images (M5), per-item virtualisation (whole-room row cache for now). |
| 2026-11-13 | `matrix-agy` | M7 part 1 (theme hot reload, generator templates): approve with changes | All 6 findings accepted and fixed: a pending reload survives cancellation in the event loop; `--reload-theme` only signals a process that holds the lock on its pid file and is verified to be matrix-rat (a stale pid can no longer hit an unrelated program), and never deletes a live instance's file; templates carry no name so the theme is called after its file and `F3` cycles correctly; a changed `theme` setting in the config is followed live and a not-yet-written theme is awaited quietly; pywal instructions use a reload hook; sender colours no longer include the muted tone. Still untested against the real generators. Remaining M7 work: dark/light pairing, more bundled palettes. |
| 2026-11-13 | `matrix-agy` | M6 part 1 (settings menu, logout, reaction picker, copy, themes, Nix package): approve with changes | All 5 findings accepted and fixed: the worker no longer panics when a command for the ended session arrives at the login wait (and the UI drops the old session at once); an explicit logout removes the now-unreadable stores; a successful logout is not shown as an error; hover and drag state are cleared when the mouse is turned off; "Nothing to copy" toast, base64 and tiny-size tests, sender colours distinct from text. `default.nix` not built yet. Remaining M6 work: keybind editor, command palette, completions, context menu. |
| 2026-11-13 | `matrix-agy` | M10b (right-click menu, read markers): approve with changes | All 11 findings fixed: a menu closes when its message changes state (sent, deleted) or its room leaves the list, and entries re-check before running; "Read by…" follows the read markers setting; reader names are worked out once per receipt change, refreshed when the member list arrives, with bounded eviction; the marker is drawn over the end of a full last row instead of adding a row; blank display names fall back to the id part. |
| 2026-11-13 | `matrix-claude` (roles reversed) | Notification icons, written by `matrix-agy`: approved | Merged after the full check, the hand-over, and a look at the runtime-directory permissions and the letter bitmaps; no real notification seen on this machine. |
| 2026-11-13 | `matrix-agy` | M10a (room previews): approve with changes | Fixed: latest-message lines are cached per room and recomputed only when that room's latest message changes, registration with the SDK runs in the background, and nothing is worked out while previews are off; wheel scrolls a room at a time; cursor reveal in very small lists; a room whose first line is scrolled out keeps its initial; the open room's line follows the loaded messages. Not done: cropping a room picture when only one of its two lines is on screen (the tile shows instead). |
| 2026-11-13 | `matrix-claude` (roles reversed) | Configurable key bindings, written by `matrix-agy`: approved | Merged after the full check and a look at the editor, the action table and the quit safety rail; not read line by line, not tried live. |
| 2026-11-13 | `matrix-agy` | M9 (spaces, room management, find in room): approve with changes on each | Fixed. Spaces: selection by position (a space inside two others trapped the stepping), steady cursor on background updates, parent shown for sub-spaces. Room management: no input can panic the link parser; ids validated after decoding; all typed and pasted text cleaned at ingestion; a new direct chat from a member row needs a second Enter; palette entries and the title click act on the room they name; answers are matched to the popup that asked; the member list is fetched without the SDK's full reload (which would discard the room's outgoing encryption session), bounded for very large rooms, and kept on a failed fetch; help overlay balances and scrolls so no key falls out. Find in room: every history request is answered; the bar follows message changes; the found message is shown below the bar. Not done: cursor movement inside one-line inputs; `?action=` in `matrix:` links; mouse-wheel stepping in search. |
| 2026-11-13 | `matrix-claude` (roles reversed) | Composer completions, written by `matrix-agy`: approved | One follow-up, fixed by `matrix-agy`: one accepted completion mentions a person once, not at every later occurrence of the same word. |
| 2026-11-13 | `matrix-agy` | M8 (notifications, threads, sending files, message shields): approve with changes on all four | Fixed. Notifications: burst limits that hold across event-loop turns, one worker for desktop notifications, escaped summary, preview-off shows nothing of the conversation, count fallback from the server's count and not during the backlog after login, invitations once per room after the session settled, "watching" requires the same thread, no overlay and the newest message in view, member fetch skips spaces and known rooms, cleanup on logout. Threads: the first message keeps its identity while the thread loads and actions on it are routed to the room until then; read marks only on success and in order; unread without touching the event cache; `reached_start` for threads. Sending files: nothing is pasted behind overlays; chips and upload drafts are scoped by room and thread; hidden chips are never sent; edit takes precedence; decoder limits; caption and reply stay with the first file and a failed first file sends nothing. Shields: "authenticity not guaranteed" is shown as a quiet remark instead of dropped; deleted messages carry none. Not done: fetching only our own member event (the SDK cannot store one); reply mode is not restored when files come back unsent. |
| 2026-11-13 | `matrix-claude` (roles reversed) | Command palette, written by `matrix-agy`: approve with changes | 6 findings, all fixed by `matrix-agy`: merged with the new features and covers them (notifications setting, thread actions, attach, save); log out from the palette opens the settings row unarmed so the two-step confirmation stays; no palette over the picture viewer or the login, recovery and verification popups; leaving a thread through a room jump keeps drafts apart. |
| 2026-11-13 | `matrix-agy` | M5 part 3 (room avatars, sender names, room list tiles): approve with changes | All 6 findings fixed: two rooms sharing a picture no longer re-send it every frame; invitations get no picture (it would tell a stranger's server the invite was seen) and room pictures have their own small download budget; member lists are fetched only when incomplete and stopped with the session; hero lookup without cloning the room info, also for favourite direct chats; tiles are always two columns and highlight modifiers no longer reach picture cells; avatar addresses validated, no second download after "too large". In the same commit, not reviewed: refreshed login tokens are saved by a task that listens from session start (they could be missed at startup, which logged the user out at the next start), `v` views a picture, Ctrl-↑/↓ switches rooms, accent bar on the open room. |
| 2026-11-13 | `matrix-agy` | M5 part 1 (inline images): approve with changes | Findings 1–7 fixed: picture downloads are bounded by a real byte limit (own streaming request, `Content-Length` refused early; the sender's stated size bounds nothing); the terminal graphics query is our own `poll()`-based one that leaves no reader on stdin, and runs before mouse and focus reporting are on; logout clears pictures, viewer and running fetches; eviction of a failed picture re-lays out; remembered sizes capped; RGBA memory checked before decoding; kitty images deleted before suspend; one viewer encoding at a time. Real kitty rendering still unseen; inside herdr the app uses half blocks. |
| 2026-11-13 | `matrix-agy` | M4 (device verification): approve with changes | Findings 1–7 fixed: masked password field when the server asks for one at cross-signing set-up (tested only with the fake worker; our server needs none); "Ask again" cancels the old request first (real-SDK test); "They match" only when all emoji are on screen, default focus never on a yes-button, Space never confirms, a 0.7 s input guard after buttons change, and a request arriving while typing waits in the status line; in-room requests only if addressed to us, declined when busy, sender keys fetched; all session tasks tracked and stopped on logout, late news ignored; cancel reason sanitised; Settings row wording and the no-device-no-recovery dead end. Message shields not done. |
| 2026-11-13 | `matrix-agy` | M5 part 2 (saving attachments): approve with changes | All 6 findings accepted and fixed: `o` only hands viewable types (pictures, audio, video, text, PDF) to the desktop, everything else is saved and named but not launched, and a result arriving after logout opens nothing; the download folder is always absolute; files that are large or do not state a size ask first, and one save runs at a time (the SDK cannot stream, so the whole file still passes through memory); the temporary file is created exclusively and published by hard link, so no link is followed and no empty file is left; names are trimmed after cutting; saved-file memory is cleared on logout and folder change; a file's own action wins over a caption link. |
