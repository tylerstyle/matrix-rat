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
