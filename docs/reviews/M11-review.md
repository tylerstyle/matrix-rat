# Review: M11 — `README.md`, screenshot generator, and `tighten_spoilers()`

**Commits reviewed (local `main`, unpushed):**
- `40ae10e` — `Spoilers may have spaces inside the bars; screenshot generator` (`src/format.rs`, `src/ui/mod.rs`, `docs/screenshots/*.svg`)
- `f01055b` — `README as a landing page, with generated screenshots` (`README.md`, `src/ui/mod.rs`, `docs/screenshots/*.svg`)

**Files inspected:** `README.md`, `Cargo.toml`, `default.nix`, `shell.nix`, `LICENSE`, `NOTICE`, `contrib/themes/README.md`, `scripts/check.sh`, `scripts/dev-homeserver.sh`, `src/main.rs`, `src/format.rs`, `src/complete.rs`, `src/keymap.rs`, `src/config.rs`, `src/theme.rs`, `src/modal.rs`, `src/paths.rs`, `src/logging.rs`, `src/notify.rs`, `src/images.rs`, `src/model.rs`, `src/app.rs`, `src/app/attach.rs`, `src/app/files.rs`, `src/app/rooms.rs`, `src/app/verify.rs`, `src/ui/mod.rs`, `src/ui/composer.rs`, `src/ui/timeline.rs`, `src/ui/image.rs`, `src/ui/roominfo.rs`, `src/ui/status.rs`, `src/ui/verify.rs`, and `docs/screenshots/{chat,palette,theme}.svg`.

---

## Summary

- **`README.md` (`f01055b`)** works very well as a newcomer-facing landing page: clear hero layout, good task order, honest early-development disclaimer, and solid coverage of default shortcuts and themes. However, several reference entries and feature claims are inaccurate or overpromised against the implementation (missing `context_menu` and `menu` in the key config docs, missing `secret_store = "auto"` and `sidebar_width` in Settings, terminal notification icons and "never for the open room" suppression overstated, room-list pictures described without noting kitty-only, and `Ctrl-Q`/`--demo`/`make` caveats), and `theme.svg` does not actually show an open thread.
- **`tighten_spoilers()` (`40ae10e`, `src/format.rs:219-255`)** preserves `MentionMark` byte offsets (`resolve_marks` runs before `markdown_html` -> `tighten_spoilers`), **but** its text-level `match_indices("||")` + per-line rewrite corrupts non-spoiler `||` uses (logical OR / prose pairs, Markdown links/URLs containing `||`, escaped `\||`), mutates code inside 4-space indented code blocks, blockquoted fences, and multi-line inline code spans, and mishandles barless/empty-first-cell GFM tables as well as `||` in `@mention`/`#room` display names.

---

## Part 1: `tighten_spoilers()` (`src/format.rs:219-255`)

### 1. [HIGH] Non-spoiler `||` pairs on a line are turned into `<span data-mx-spoiler>` (logical OR, prose, links/URLs, escaped bars)
- **Where:** `src/format.rs:236-251`
- **Detail:** `tighten_spoilers` takes every non-overlapping `match_indices("||")` outside `code_mask`, chunks them sequentially in pairs (`as_chunks::<2>().0`), and trims whitespace inside each pair:
  1. **Unquoted expressions / prose with two `||`:** `"if a || b and c || d"` has `marks` at the two `"||"` operators -> rewrites to `"if a ||b and c|| d"`, hiding `"b and c"` behind `<span data-mx-spoiler>b and c</span>` (and sending that spoiler HTML when plain text previously had `html: None`).
  2. **Unbalanced prefix `||`:** `"note: a || b, plus || secret ||"` (`3` occurrences) pairs `[0]` and `[1]` (`" b, plus "`), turning `"b, plus"` into a spoiler and leaving `" secret ||"` uncovered.
  3. **Markdown links & URLs containing `||`:** `code_mask` does not mask autolinks or Markdown link destinations/labels. For `"[foo || bar](https://example.com) and [baz || qux](https://example.org)"` or `"https://example.com/a||b and || c ||"`, `match_indices("||")` pairs across link/URL syntax and strips spaces, breaking the links and injecting spoiler tags.
  4. **Overlapping triple/quadruple bars (`|||`, `||||`):** For `"||| a |||"`, non-overlapping `match_indices("||")` matches `[0, 6]`, sets `inner = "| a "`, and outputs `"||| a|||"` (asymmetrically stripping only the right space; `comrak` fails flanking on `"||| "` and produces no spoiler). For `"||  || real ||"` (`3` matches), `[0..4]` is skipped as empty and `"real ||"` stays unpaired.
  5. **Escaped bars (`\||`):** `"\|| a ||"` ignores the `\` escaping the first pipe and strips spaces (`"\||a||"`), while `"|| foo \||"` strips the trailing space (`"||foo \||"`) so `comrak` renders literal text with altered spacing.
- **Recommendation:** Only strip edge whitespace inside `|| ... ||` when neither delimiter is backslash-escaped, not inside link/autolink destinations, and handle spoiler tightening either inside `comrak`/token-aware scanning or with stricter delimiter boundary checks rather than unconditional global `as_chunks::<2>()` pairing.

### 2. [HIGH] Code inside indented blocks, quoted fences, and multi-line inline spans is not masked by `code_mask`
- **Where:** `src/format.rs:223`; `src/complete.rs:641-743`
- **Detail:** `code_mask(text, false)` only masks top-level triple-backtick/tilde fenced blocks and single-line inline backtick spans:
  1. **4-space / tab indented code blocks:** `"    if (a || b) && (c || d)"` or `"    || secret ||"` has `code[..] == false` -> rewritten to `"    ||secret||"` inside `<pre><code>...</code></pre>`.
  2. **Fenced code blocks inside blockquotes or list items:** `opening_fence` (`src/complete.rs:676-678`) uses `strip_up_to_3_spaces`, so `"> ```\n> || secret ||\n> ```"` is left unmasked and rewritten to `"> ||secret||"`.
  3. **Multi-line inline code spans:** `code_mask` runs `mark_inline_code` per line (`src/complete.rs:666-670`), so ``"`foo\n|| a ||`"`` has an unclosed backtick on line 1 (`treat_unclosed_as_code = false`), leaving line 2 unmasked and mutating `|| a ||` -> `||a||` inside `<code>foo ||a||</code>`.
- **Recommendation:** Extend `code_mask` (or add a dedicated Markdown-aware pre-pass in `src/format.rs`) to track CommonMark indented code blocks, blockquoted/list-nested fences, and multi-line backtick runs.

### 3. [MEDIUM] Table-row guard misses barless / empty-first-cell / quoted GFM tables and drops multi-line spoilers
- **Where:** `src/format.rs:226-235`; `options.extension.table = true` (`src/format.rs:264`)
- **Detail:** `if lead.starts_with('|') && !lead.starts_with("||")`:
  1. **Tables without a leading `|`:** `comrak` parses `"a | b\n- | -\n|| x || | y"` and `"x | || | || z"` as tables; line 3 starts with `||` or `x`, so `tighten_spoilers` mutates table cells / cell separators.
  2. **Tables starting with an empty cell (`|| col2 | col3 |`):** Fails `!lead.starts_with("||")`, so `"|| a || | b\n--- | ---"` rewrites `|| a ||` -> `||a||`.
  3. **Tables inside blockquotes (`> | col1 | col2 |`):** `lead` starts with `'>'`, bypassing the guard.
  4. **False positive:** A non-table line starting with `"| || secret ||"` is skipped and does not render as a spoiler.
  5. **Multi-line spoilers (`"|| line 1\nline 2 ||"`):** Per-line splitting (`split_inclusive('\n')`) leaves unpaired `||` on both lines untightened, whereas `comrak` handles `"||line 1\nline 2||"` across `<br />`.

### 4. [MEDIUM] Interaction with `resolve_marks` / `mark.at`: offset order is safe, but `||` in display names & `/spoiler` nesting break
- **Where:** `src/format.rs:139-149, 193-214, 270`; `src/complete.rs:749-904`
- **Detail:**
  1. **Offset safety (verified):** Both `markdown_message` and `face_message` call `resolve_marks(text, marks)` **before** `markdown_html(&linked)` calls `tighten_spoilers`. Thus accepted completion anchors (`mark.at`) are matched against the exact composer string and are **not** shifted or invalidated by `tighten_spoilers`, and `"|| Alice ||"` resolves cleanly to `"<span data-mx-spoiler><a href=\"https://matrix.to/#/@alice:hs\">Alice</a></span>"`.
  2. **Unescaped `|` in `escape_link_label` (`src/complete.rs:895-904`):** `escape_link_label` escapes `\ [ ] * _ \` ~ < >` but **not `|`**. If an `@mention` display name or `#room` name/alias contains `||` (e.g., `"Alice || Bob"`), `"|| Alice || Bob ||"` resolves to `"|| [Alice || Bob](<...>) ||"`, and `tighten_spoilers` pairs the outer opening `||` with the `||` inside `[Alice || Bob]`, producing `"||[Alice || Bob](<...>) ||"` and breaking the spoiler.
  3. **Mid-word formatting / mention links:** `"foo|| @Alice ||bar"` becomes `"foo||[Alice](<...>)||bar"`, where `comrak`'s CommonMark flanking rule rejects `after_char = '['` after alphanumeric `'o'`, so no spoiler renders (same for `"foo|| **bold** ||bar"`).
  4. **`/spoiler` nesting (`src/format.rs:139-149`):** `/spoiler a || b ||` calls `inline_html(&linked)` -> `markdown_html`, tightening inner `|| b ||` into nested `<span data-mx-spoiler>`.
- **Recommendation:** Escape `|` (`\|`) in `escape_link_label` (`src/complete.rs:898`) and add unit tests covering `resolve_marks` + `tighten_spoilers` (including display names with `||` and `/spoiler`).

---

## Part 2: `README.md` (`f01055b`) and `write_screenshots` / `svg()` (`40ae10e`, `f01055b`)

### 5. [HIGH] `Keys` reference (`README.md:147, 153`) omits `context_menu` and `menu`
- **Where:** `README.md:147, 153`; `src/keymap.rs:171, 840, 843-848`
- **Detail:**
  - `ACTIONS` (`src/keymap.rs:136-192`) defines **35** `global` actions (`55` total). `README.md:153` lists 34 and **omits `context_menu`** (`Action::ContextMenu`, bound by default to `Menu` globally and `m` in Timeline).
  - Named keys (`README.md:147`) omit **`menu`** (`"menu" => KeyCode::Menu`, `src/keymap.rs:840`), as well as `page_up`/`page-up`, `page_down`/`page-down`, and `↑`/`↓`/`←`/`→` aliases (`843-848`).
  - (The `vim` preset, `sidebar`/`timeline`/`composer` action lists, and rejected key rules in `README.md:149, 154-164` match `src/keymap.rs` accurately.)
- **Recommendation:** Add `context_menu` to `Everywhere (global)` and `menu` to **Named keys**.

### 6. [HIGH] `Other settings` (`README.md:170-179`) omits `secret_store = "auto"` and `sidebar_width`, and overstates `F2` coverage
- **Where:** `README.md:102-103, 170-179`; `src/config.rs:12-49, 106, 408-411, 633-645`; `src/modal.rs:280-296`; `src/app.rs:640-658, 780-788`
- **Detail:**
  1. `README.md:178` lists `secret_store` values as `"file" (default) or "keyring"`, omitting **`"auto"`** (`src/config.rs:40`: `"auto", "keyring" or "file"`).
  2. `sidebar_width` (`default = 24`, `src/config.rs:20, 96`) is missing from `Other settings`, and `### Themes` omits the exact TOML key names `theme_dark` / `theme_light` (`README.md:108` only names the UI label **Follow dark/light**).
  3. `README.md:102-103` states *"the settings menu (`F2`) writes what you change into `~/.config/matrix-rat/config.toml`, keeping your comments"*, without mentioning that **`images`**, **`notification_preview`**, **`secret_store`**, and choosing **`"auto"` vs `"desktop"` vs `"terminal"`** for `notifications` have no `F2` UI controls and must be edited in `config.toml` directly (`Setting::Notifications` only toggles between `Off` and `Auto`/active mode, `src/app.rs:780-788`). Also note that pressing `R` ("Reset all") in **Settings → Key bindings** removes `[keys]` and `preset` (including any comments inside `[keys]`), while preserving comments elsewhere (`src/config.rs:408-411`).
- **Recommendation:** Add `"auto"` to `secret_store`, list `sidebar_width` (plus `theme_dark` / `theme_light` under Themes), and clarify which `Other settings` are TOML-only.

### 7. [HIGH] Overpromises in `Highlights` and `Good to know` (notifications, room pictures, file drop/`s`, `Ctrl-Q`)
- **Where:** `README.md:29-35, 190`; `src/app.rs:836-845, 902-907, 2471-2483`; `src/images.rs:847-850`; `src/ui/image.rs:61-62`; `src/app/attach.rs:116-138`; `src/app/files.rs:71-89`; `src/keymap.rs:1275-1286`
- **Detail:**
  1. **Notifications (`README.md:35`):** *"Desktop or terminal notifications with the chat's picture, never for the room you are looking at."*
     - Terminal notifications **never** carry the chat's picture (`resolve_notice_icon` returns `NotifyIcon::Generic` whenever `notify_mode.resolved() != NotifyMode::Desktop`, `src/app.rs:902-907`).
     - Notifications **do** still fire for the currently open room if the window lacks focus, if scrolled up (`timeline.scroll != 0`), if any modal/help/image viewer is open, or if a message arrives in a thread while viewing the room (or vice versa) (`watching`, `src/app.rs:836-845`). Better: *"Desktop notifications include the chat's picture; silenced while you're looking at the bottom of that conversation."*
  2. **Room pictures (`README.md:29`):** Inline/viewer pictures fall back to coloured blocks, **but room pictures in the room list are kitty-only** (`ImageStore::avatar` returns `None` if `!self.is_kitty()`, `src/images.rs:847-850`; elsewhere only coloured initial tiles appear, `src/ui/image.rs:61-62`).
  3. **Drop / `s` (`README.md:31`):** Dropping a file **attaches** it to the composer (`Enter` still sends it; edits and invites paste the path as text, `src/app/attach.rs:116-138`). `s` acts on the **selected** message (in Messages or image viewer) and asks for confirmation on files over `100 MB` (`LARGE_FILE`) or of unknown size (`src/app/files.rs:71-89`).
  4. **`Ctrl-Q always quits` (`README.md:190`):** In Key bindings capture mode (`src/app.rs:2471-2474`), `Ctrl-Q` and `Ctrl-Z` are captured instead of quitting/suspending; and if `quit` is rebound to another key, `Ctrl-Q` stops quitting (`enforce_quit_rail` only restores `Ctrl-Q` when `quit` is completely unbound).

### 8. [MEDIUM] Screenshot generator / SVG bugs (`theme.svg` does not show a thread; hardcoded `rat-dark` base breaks corners; `write_screenshots` runs with `--ignored`)
- **Where:** `src/ui/mod.rs:133-276, 323-362`; `docs/screenshots/theme.svg:1-3, 19, 385-387`; `README.md:117, 198-199`
- **Detail:**
  1. **`theme.svg` shows a room, not a thread:** `README.md:117` (`alt="A thread, in another theme"`) and `src/ui/mod.rs:348` claim `theme.svg` shows a thread, **but** `write_screenshots` (`src/ui/mod.rs:358-361`) never presses `t` (`Action::OpenThread`) after `KeyCode::Up`. `docs/screenshots/theme.svg:19, 385-387` shows `#matrix-rat` with the collapsed `"💬 2 replies"` thread-starter selected.
  2. **Hardcoded `rat-dark` `base_bg` overwrites `rx="8"` corners in `theme.svg`:** `svg()` (`src/ui/mod.rs:141, 150, 175-183`) hardcodes `base_bg = "#16161d"` for the outer `<rect ... rx="8"/>`. In `theme.svg` (Catppuccin Mocha, `#1e1e2e`), every row fails `bg != base_bg` and paints full-width sharp-cornered `<rect width="924.2" height="18" fill="#1e1e2e"/>` over the rounded outer rectangle (also protruding `0.2` past `viewBox="0 0 924 612"`). Passing the theme's `Role::Text` background as `base_bg` (or wrapping rows in a `<clipPath>` with `rx="8"`) fixes both. Note also `svg()` drops `Modifier::UNDERLINED` and `Modifier::CROSSED_OUT` (`src/ui/mod.rs:249-263`), and `shape()` does not draw `▒` (`U+2592` for covered spoilers).
  3. **`cargo test -- --ignored --test-threads=1` (`README.md:198-199`) runs `write_screenshots` too:** `src/ui/mod.rs:323` is `#[ignore = "writes docs/screenshots"]`, so running all ignored tests without a filter overwrites `docs/screenshots/*.svg`. Use e.g. `cargo test matrix:: -- --ignored --test-threads=1` (or exclude `write_screenshots`).

### 9. [MEDIUM] Other accuracy checks (`Install`, `First steps`, `Everyday keys`, `Good to know`, `NOTICE`)
- **`Install` (`default.nix` & C deps, `README.md:39-63`):**
  - `default.nix:17, 25, 28` (`fileset = [ ./Cargo.toml ./Cargo.lock ./src ./themes ./contrib ]`, `pkgs.aws-lc`, `pkgs.sqlite`, `doCheck = false`) covers every `include_str!` / `env!` used in non-test builds (checked all `src/`).
  - `README.md:48` lists `"Rust 1.96 or newer, plus a C compiler, cmake and perl"`, omitting **`make`** (`gnumake`, needed by `cmake` building `aws-lc-sys` on Linux; `shell.nix:6` includes `pkg-config cmake perl gcc gnumake`).
  - `cargo run -- --demo` (`README.md:58`) still creates app directories, writes logs under `~/.local/state/matrix-rat/<profile>/`, and **persists `F2`/`F3`/splitter changes into `~/.config/matrix-rat/config.toml`** (`src/main.rs:80-86`, `src/app.rs:2942`). Worth a short warning or passing `--profile` / `config_file = None` in `--demo`.
  - **Kitty & multiplexers (`README.md:61-63`):** Probing (`src/images.rs:121-194`) works in any terminal implementing kitty graphics + Unicode placeholders + cell sizing (not only kitty — e.g. Ghostty), except `WezTerm` / `Konsole` (`placeholders_broken()`) and multiplexers (`TMUX`, `ZELLIJ`, `STY`, `TERM=tmux*|screen*`, `TERM_PROGRAM=tmux`).
- **`First steps` (`README.md:65-75`):**
  - Recovery prompt accepts **recovery key or passphrase** (`src/ui/modal.rs:530-531`).
  - Status line actually reads `"⚠ unverified device — verify"` (narrow: `"⚠ verify device"`), or `"⚠ encryption not set up — set up"` when no identity exists yet (`src/ui/verify.rs:384-400`); activating `"verify"` opens recovery or status/setup if no other device exists to compare emoji with (`src/app/verify.rs:48-62`).
  - Also worth noting step 1 is for an **existing** Matrix account (no in-app registration flow).
- **`Everyday keys` & `While typing` (`README.md:76-94`):**
  - Composer renders plain raw text while typing (`src/ui/composer.rs:161-165`); Markdown / `||spoiler||` formatting happens on send (`src/format.rs`), so phrasing *"While typing ... are formatted as Markdown"* reads like live WYSIWYG preview. (Also `|| spoiler ||` with inner spaces now works too.)
  - `↑ from the composer` moves through lines of a multi-line draft before stepping into messages (`src/app.rs:2535`).
  - `Alt-→` only moves `Sidebar -> Composer` (no-op from Timeline/Composer, `src/app.rs:1621-1627`).
  - `F4` opens details for the **cursor** room when the room list is focused (`src/app/rooms.rs:50-56`); the mute row in `F4` is labelled **`Notifications`** (`src/ui/roominfo.rs:728`).
  - Common default keys missing from `Everyday keys` (listed in `src/keymap.rs`): **`v`** (view picture), **`End`** (jump to newest), **`Alt-R`** (filter rooms while anywhere), **`Menu`** (context menu), **`?`** (help in Sidebar/Timeline).
- **`Good to know` & developer claims (`README.md:182-206`, `NOTICE:4`):**
  - **SSH notifications vs appearance (`README.md:186`):** `portal::in_ssh_session()` checks `SSH_CONNECTION`, `SSH_CLIENT`, and `SSH_TTY` for theme watching and keyring gating, whereas `NotifyMode::Auto` (`src/notify.rs:57`) checks only `SSH_CONNECTION` — an SSH session with `SSH_CONNECTION` unset/stripped falls back inconsistently.
  - **Logs (`README.md:188-189`):** Accurate for default `warn,matrix_rat=info` under `~/.local/state/matrix-rat/<profile>/matrix-rat.log` (`src/paths.rs`, `src/logging.rs`), though `MATRIX_RAT_LOG` (e.g. `matrix_sdk=trace`) bypasses that filter; `--profile <name>` shares `~/.config/matrix-rat/` across profiles (note `src/main.rs:41` `--help` inaccurately says each profile has its own config).
  - **Architecture claim (`README.md:203-204`):** *"every key, click and palette entry resolves to the same `Action`"* overstates direct UI clicks (`HitId::Room`, `HitId::Message`, etc. in `src/app.rs:2750-2805`), menu-only actions (`src/app/menu.rs:384-441`), and capture/modal keys.
  - **`NOTICE:4`:** Still reads `"This product is planned to include code and theme palettes adapted from iamb"`.

### 10. [LOW] Wording and ordering from a newcomer's view
- **Strengths:** Lead-in -> hero image -> honest status -> Highlights -> Install -> First steps -> Everyday keys -> Make it yours (with collapsible key config) reads naturally and stays concise.
- **Suggested newcomer improvements:**
  1. **OS scope upfront in `Install`:** Desktop keyring (`org.freedesktop.secrets`), desktop portal appearance following (`org.freedesktop.portal`), desktop notifications, and `default.nix` (`platforms.linux`) target Linux; stating *"Linux (desktop features use D-Bus; terminals on macOS/etc. can build with Cargo)"* prevents surprises.
  2. **Nix command clarity:** `default.nix` has no `flake.nix`; showing `nix-build` (`./result/bin/matrix-rat`) alongside `nix-env -if .` avoids confusing users on flake/profile setups.
  3. **Login expectation in `First steps`:** Clarify *"Log in to an existing Matrix account"* so newcomers do not search for a Sign Up screen, and mention `--demo` creates/uses `~/.config/matrix-rat/config.toml` if settings are toggled.
  4. **Clarify `F2` vs `config.toml` in `Other settings`:** Group or mark settings that are `config.toml`-only (`images`, `notification_preview`, `secret_store`, `"desktop"`/`"terminal"` notification modes) so new users don't hunt for them in `F2`.

---

## Verification

- Ran `./scripts/check.sh` on `/home/tylerstyle/git/matrix-rat` (`f01055b`, clean tree):
  - `cargo fmt --check`: passed
  - `cargo clippy --all-targets -- -D warnings`: passed
  - `cargo test`: `427 passed; 0 failed; 18 ignored; 0 measured; 0 filtered out`
  - `cargo build`: passed
- Inspected `default.nix`, `shell.nix`, and `comrak 0.59.0` / `matrix-sdk 0.19.1` behavior by reading source; `nix-build`, `cargo test write_screenshots -- --ignored`, and server-backed `--ignored` tests were not executed in this review.

---

## Verdict

**Request changes** — fix `[HIGH]` **`tighten_spoilers()`** edge cases (code blocks/quoted fences/multi-line spans, non-spoiler `||` pairs / links / URLs / escaped bars, and `resolve_marks` `|` label escaping in Findings **1–4**) and **`README.md` / screenshot generator** accuracy gaps (`context_menu`/`menu`, `secret_store = "auto"`/`sidebar_width`, notification/room-avatar/drop/`Ctrl-Q` overpromises, `theme.svg` actually opening a thread + matching theme `base_bg`, and `--ignored` screenshot side effect in Findings **5–9**) before tagging the release.
