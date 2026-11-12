# Milestone M3 (Part 2 + M3a Fixes) Review (`6e9fa14`)

**Reviewer:** Antigravity (`matrix-agy`)  
**Commit under review:** `6e9fa14` (`M3 (part 2): formatted messages, row cache, typing, retry; review fixes`, on top of `6c8e3c6`)  
**Scope:** [src/rich.rs](file:///home/tylerstyle/git/matrix-rat/src/rich.rs), [src/ui/timeline.rs](file:///home/tylerstyle/git/matrix-rat/src/ui/timeline.rs), [src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs), [src/matrix/rooms.rs](file:///home/tylerstyle/git/matrix-rat/src/matrix/rooms.rs), [src/matrix/mod.rs](file:///home/tylerstyle/git/matrix-rat/src/matrix/mod.rs), [src/format.rs](file:///home/tylerstyle/git/matrix-rat/src/format.rs), [src/model.rs](file:///home/tylerstyle/git/matrix-rat/src/model.rs), [src/theme.rs](file:///home/tylerstyle/git/matrix-rat/src/theme.rs), [src/terminal.rs](file:///home/tylerstyle/git/matrix-rat/src/terminal.rs), [src/ui/status.rs](file:///home/tylerstyle/git/matrix-rat/src/ui/status.rs)

---

## Verification Summary

- `./scripts/check.sh` passes cleanly (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, 94 unit & snapshot tests pass).
- End-to-end ignored tests against the live local `continuwuity` homeserver (`MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=alicepass cargo test -- --ignored`) pass (2 passed in 1.47s).
- Verification of the 6 findings from [M3a-review.md](file:///home/tylerstyle/git/matrix-rat/docs/reviews/M3a-review.md):
  1. **Unconditional `MarkRead` in `apply_timeline`:** **Fixed.** [src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L570-L590) (`maybe_mark_read`) now gates read receipts on `timeline.drawn`, `scroll == 0`, `modal.is_none()`, `!help_open`, `window_focused`, and `newest != timeline.marked_read`, with terminal focus events enabled in [src/terminal.rs](file:///home/tylerstyle/git/matrix-rat/src/terminal.rs#L52-L81) and `bold` cleared immediately on `open_room`.
  2. **Back-pagination loops & stale task handles:** **Fixed.** [src/matrix/rooms.rs](file:///home/tylerstyle/git/matrix-rat/src/matrix/rooms.rs#L118-L216) derives `reached_start` directly from `items.iter().any(|item| item.is_timeline_start())`, stores `paginator: Option<JoinHandle<()>>` on `OpenTimeline` (aborting it on eviction), and [src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L604-L639) (`maybe_load_older`) guards auto-pagination with `older_armed`, `view_full`, and `empty_batches < MAX_EMPTY`.
  3. **Message formatting bugs (`<` in Markdown, `/spoiler` `<p>` wrapping, `/html` tag stripping):** **Fixed** in [src/format.rs](file:///home/tylerstyle/git/matrix-rat/src/format.rs#L56-L154) and covered by unit tests.
  4. **Editing an emote converts it to normal text & overwrites draft:** **Fixed** via `begin_edit` / `end_edit` (`edit_stash`) in [src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L422-L458) and `/me ` prefill for `MsgKind::Emote` (see Finding 4 below for a couple of cleanup edge cases).
  5. **Selection landing on non-interactive `MsgKind::System` lines & click scroll jumps:** **Fixed** in `JumpToNewest`, `delete_message`, and `set_focus(Pane::Timeline)`.
  6. **Status markers, `fetch_details_for_event`, and `MatrixEvent` custom `Debug`:** **Fixed** cleanly across [src/ui/timeline.rs](file:///home/tylerstyle/git/matrix-rat/src/ui/timeline.rs), [src/matrix/rooms.rs](file:///home/tylerstyle/git/matrix-rat/src/matrix/rooms.rs), and [src/matrix/mod.rs](file:///home/tylerstyle/git/matrix-rat/src/matrix/mod.rs).

---

## Milestone M3 Completeness Assessment (vs. `docs/PLAN.md` §10)

Comparing against [docs/PLAN.md](file:///home/tylerstyle/git/matrix-rat/docs/PLAN.md#L337-L353) §10 (M3 — Message timeline & sending):
- Per-room `matrix_sdk_ui::Timeline` subscription with LRU cache of 8 open rooms: **Done.**
- Cached/virtualized timeline widget (`RowCache`, grouped headers, timestamps, wrapped body, reply quotes, edit indicator, reaction pills, read marker, system lines): **Done.**
- Incoming `org.matrix.custom.html` rendered to styled spans (headings, bold/italic/strike/underline, inline & fenced code blocks, blockquotes, lists, links, spoilers with cover/reveal on selection): **Done.**
- Scrollback with automatic + manual back-pagination and scroll anchoring (`Row.source`) when messages arrive while scrolled up: **Done.**
- Outgoing Markdown + `||spoiler||` + slash commands (`/me`, `/shrug`, `/tableflip`, `/unflip`, `/spoiler`, `/plain`, `/html`): **Done.**
- Actions (`r` reply, `e` edit, `d` redact, `+` emoji picker reaction, click reaction chip to toggle, `R` retry failed local echo via `unwedge()`, `o` / click link to open): **Done.**
- Typing notices (incoming banner + outgoing debounced `Typing` commands) and visibility-gated read receipts: **Done.**

Once the findings below (especially Finding 1) are addressed, M3 is complete and ready to push.

---

## Findings (Ordered by Severity)

### 1. [HIGH] Terminal escape, control-character, tab, and bidi-override injection from untrusted Matrix data
**Location:** [src/rich.rs](file:///home/tylerstyle/git/matrix-rat/src/rich.rs#L50-L65), [src/rich.rs](file:///home/tylerstyle/git/matrix-rat/src/rich.rs#L151-L193), [src/rich.rs](file:///home/tylerstyle/git/matrix-rat/src/rich.rs#L297-L340), [src/matrix/convert.rs](file:///home/tylerstyle/git/matrix-rat/src/matrix/convert.rs), [src/matrix/mod.rs](file:///home/tylerstyle/git/matrix-rat/src/matrix/mod.rs#L351-L390)

**Problem:**
In `ratatui-core 0.1.2` (`Span::render`), grapheme clusters with `unicode_width == 0`—which includes `ESC` (`\x1b`), C0 control characters (`\x00`..`\x1f`), `DEL` (`\x7f`), C1 control characters (`\u{80}`..`\u{9f}`), and Unicode bidi control characters—are **not** stripped; `Span::render` either appends them to the preceding `Cell` (`buf[(x - 1, y)].append_symbol(grapheme.symbol)`) or writes them into the first cell of the span (`buf[(x, y)].set_symbol(grapheme.symbol)` when `i == 0`), and `CrosstermBackend` writes `cell.symbol()` directly to the terminal.

Currently, only numeric HTML entities in `decode_entities` ([src/rich.rs](file:///home/tylerstyle/git/matrix-rat/src/rich.rs#L175)) check `!c.is_control()`. Literal characters in untrusted inputs are never sanitized:
1. **`Rich::plain` ([src/rich.rs](file:///home/tylerstyle/git/matrix-rat/src/rich.rs#L50-L65)):** Splits on `'\n'` and passes every other byte directly into `Seg`. A plain-text Matrix message containing `\x1b]52;c;...\x07` (OSC 52 clipboard write), terminal title escapes, `\r`, `\t` (which `unicode-width` reports as width 0 while the hardware terminal advances the cursor up to 8 columns, desynchronizing the ratatui screen buffer), or C1 controls goes straight to the terminal.
2. **`rich::from_html` ([src/rich.rs](file:///home/tylerstyle/git/matrix-rat/src/rich.rs#L302-L340)):** Inside `<pre>`, `push_text` preserves all non-`'\n'` characters verbatim (including `\x1b`, `\t`, `\r`, C0/C1). Outside `<pre>`, `push_text` only collapses `c.is_whitespace()` (`' '`, `'\t'`, `'\n'`, `'\r'`, `'\x0b'`, `'\x0c'`)—`\x1b`, `\x07`, `\x08`, `\x7f`, and C1 controls (`\u{80}`..`\u{9f}`) are not whitespace and pass straight through.
3. **Bidi overrides (`U+202A`..`U+202E`, `U+2066`..`U+2069`, `U+200E`, `U+200F`, `U+061C`):** Unclosed RTL overrides in a message body, display name, or room name can flip the rendering order of the rest of the terminal row.
4. **Other untrusted Matrix strings:** `sender` display names, `room.name`, `reply_to` quotes, `reaction.key`, and `typing` user names in [src/matrix/convert.rs](file:///home/tylerstyle/git/matrix-rat/src/matrix/convert.rs) and [src/matrix/mod.rs](file:///home/tylerstyle/git/matrix-rat/src/matrix/mod.rs) also reach `Span`s unsanitized.

**Suggested Change:**
Add sanitization helpers in [src/text.rs](file:///home/tylerstyle/git/matrix-rat/src/text.rs) and apply them to all untrusted strings entering `rich` and `model`:
```rust
fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200e}'
                | '\u{200f}'
                | '\u{061c}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
}

/// Sanitize single-line untrusted text (names, reactions, URLs, quotes).
pub fn sanitize_inline(s: &str) -> String {
    s.chars()
        .map(|c| if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c })
        .filter(|&c| !is_unsafe_char(c))
        .collect()
}

/// Sanitize multiline text (preserves `\n`, expands `\t` to spaces, strips control/bidi chars).
pub fn sanitize_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if is_unsafe_char(c) => {}
            c => out.push(c),
        }
    }
    out
}
```
- Call `text::sanitize_text` in `Rich::plain` and in `Builder::push_text` (after entity decoding in `from_html`).
- Call `text::sanitize_inline` on `sender`, `room.name`, `reply_to.sender`, `reply_to.text`, `reaction.key`, and `typing` display names in [src/matrix/convert.rs](file:///home/tylerstyle/git/matrix-rat/src/matrix/convert.rs) and [src/matrix/mod.rs](file:///home/tylerstyle/git/matrix-rat/src/matrix/mod.rs).
- Add a unit test in `src/rich.rs` verifying that `\x1b[31m`, `&#27;`, `\t`, `\r`, and `\u{202e}` are stripped/expanded in both `Rich::plain` and `from_html` (including inside `<pre>`).

---

### 2. [MEDIUM] Case-sensitivity mismatch in `Message::has_spoiler()` and `RowCache` key gaps
**Location:** [src/model.rs](file:///home/tylerstyle/git/matrix-rat/src/model.rs#L89-L93), [src/rich.rs](file:///home/tylerstyle/git/matrix-rat/src/rich.rs#L140-L148), [src/ui/timeline.rs](file:///home/tylerstyle/git/matrix-rat/src/ui/timeline.rs#L324-L329), [src/ui/timeline.rs](file:///home/tylerstyle/git/matrix-rat/src/ui/timeline.rs#L403-L413)

**Problem:**
1. **`Message::has_spoiler()` vs `rich::attribute()`:** In [src/rich.rs](file:///home/tylerstyle/git/matrix-rat/src/rich.rs#L145), `attribute(attributes, "data-mx-spoiler")` matches attribute names with `eq_ignore_ascii_case`. However, in [src/model.rs](file:///home/tylerstyle/git/matrix-rat/src/model.rs#L89-L93), `Message::has_spoiler()` checks `self.html.as_deref().is_some_and(|html| html.contains("data-mx-spoiler"))`, which is case-sensitive. If an incoming message contains `<span DATA-MX-SPOILER>`, `rich.rs` renders it covered (`░░░░`), but `has_spoiler()` returns `false`, so `revealed` in `timeline::draw` stays `None` and selecting the message never reveals the spoiler.
2. **Theme reload with the same name:** `RowCache.key` uses `theme.name.clone()`. When M7 hot-reloads a modified theme file (or Noctalia regenerates `noctalia.toml`) without changing `theme.name`, `RowCache` will serve stale styles.

**Suggested Change:**
1. In `Message::has_spoiler()`, use case-insensitive matching without allocating:
   ```rust
   pub fn has_spoiler(&self) -> bool {
       self.html.as_deref().is_some_and(|html| {
           html.as_bytes()
               .windows(b"data-mx-spoiler".len())
               .any(|w| w.eq_ignore_ascii_case(b"data-mx-spoiler"))
       })
   }
   ```
2. Add a `theme_rev: u64` counter on `App` (incremented in `set_theme`) or include it alongside `theme.name` in `RowCache.key`.

---

### 3. [MEDIUM] Link handling: plain-text URLs are not clickable/openable, and link hover/click skips timeline state
**Location:** [src/rich.rs](file:///home/tylerstyle/git/matrix-rat/src/rich.rs#L50-L65), [src/rich.rs](file:///home/tylerstyle/git/matrix-rat/src/rich.rs#L416-L426), [src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L500-L516), [src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L1570-L1572), [src/ui/timeline.rs](file:///home/tylerstyle/git/matrix-rat/src/ui/timeline.rs#L477-L480)

**Problem:**
1. **Plain-text messages with URLs cannot be opened:** `App::open_link` ([src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L505-L508)) only parses `m.html.as_deref()`. When another Matrix client sends a plain-text message (`m.html == None`) containing `https://example.org`, `Rich::plain` emits no links and `open_link` always reports `"Nothing to open in this message"`.
2. **Untrimmed `href` in `<a href="...">`:** In [src/rich.rs](file:///home/tylerstyle/git/matrix-rat/src/rich.rs#L418-L424), `let href = decode_entities(href);` is not trimmed and does not strip control/whitespace characters before checking scheme or storing the URL.
3. **Link hover & click behavior:**
   - In [src/ui/timeline.rs](file:///home/tylerstyle/git/matrix-rat/src/ui/timeline.rs#L477-L480), `hovered` checks `matches!(app.hover, Some(HitId::Message(m) | HitId::Reaction(m, _)) if m == msg)`, omitting `HitId::Link(m, _)`. Moving the mouse onto a link inside a message drops the message's hover highlight.
   - In [src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L1570-L1572), clicking `HitId::Link(message, link)` opens the link without focusing `Pane::Timeline` or selecting `message`.

**Suggested Change:**
1. In `Rich::plain` (and/or a small helper in `rich.rs`), detect `https://` and `http://` tokens so plain-text URLs populate `Rich.links` and get `Seg.link = Some(idx)` just like HTML `<a>` tags; then `App::open_link` can call `rich::from_message(m.html.as_deref(), &m.body)` instead of requiring `m.html`.
2. Trim `href` and strip control/whitespace chars (`let href = text::sanitize_inline(&decode_entities(href)).trim().to_owned();` and reject URLs containing whitespace).
3. Include `HitId::Link(m, _)` in the `hovered` match in [src/ui/timeline.rs](file:///home/tylerstyle/git/matrix-rat/src/ui/timeline.rs#L477-L480), and select `message` when clicking `HitId::Link(message, link)` in [src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L1570-L1572).

---

### 4. [MEDIUM] `edit_stash` and `typing` state edge cases on edit start, room disappearance, and `NeedLogin`
**Location:** [src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L422-L497), [src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L1137-L1148), [src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L1296-L1317)

**Problem:**
1. **Starting an edit immediately sends `Typing(true)`:** Pressing `e` calls `begin_edit(index)`, which populates `self.composer` with the old message's text; at the end of `on_event` ([src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L371)), `self.update_typing()` sees `!self.composer.is_empty()` and immediately broadcasts `MatrixCmd::Typing { room_id, typing: true }` before the user has typed anything.
2. **`edit_stash` / `typing_sent` not cleared on `NeedLogin` or when the active room vanishes:**
   - In `on_matrix` (`MatrixEvent::NeedLogin`, [src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L1137-L1148)), `self.edit_stash` and `self.typing_sent` are not reset.
   - In `apply_rooms` ([src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L1296-L1317)), when the currently open room disappears (`None` branch), `self.compose_mode = ComposeMode::Normal` is set directly without calling `self.end_edit(false)` or clearing `self.edit_stash = None; self.typing_sent = None;`.

**Suggested Change:**
- In `update_typing()` ([src/app.rs](file:///home/tylerstyle/git/matrix-rat/src/app.rs#L473)), only consider typing active when not in `ComposeMode::Edit(_)`:
  ```rust
  let active = !self.composer.is_empty()
      && !self.composer.text().starts_with('/')
      && !matches!(self.compose_mode, ComposeMode::Edit(_));
  ```
- Clear `self.edit_stash = None; self.typing_sent = None;` in `MatrixEvent::NeedLogin` and in the `None` branch of `apply_rooms`.

---

### 5. [LOW] Pathological `<blockquote>` / list nesting overflows `Row` width, and HTML tables concatenate cells
**Location:** [src/rich.rs](file:///home/tylerstyle/git/matrix-rat/src/rich.rs#L233-L239), [src/rich.rs](file:///home/tylerstyle/git/matrix-rat/src/rich.rs#L356-L474), [src/ui/timeline.rs](file:///home/tylerstyle/git/matrix-rat/src/ui/timeline.rs#L173-L185)

**Problem:**
1. **Unbounded quote/list prefix width:** `Builder::indent` ([src/rich.rs](file:///home/tylerstyle/git/matrix-rat/src/rich.rs#L233-L239)) builds `"┃ ".repeat(self.quotes)` and `"  ".repeat(self.lists.len().saturating_sub(1))` without a cap. A message with 100 nested `<blockquote>` tags produces a 200-column `prefix`. While `wrap()` uses `avail = width.saturating_sub(pw).max(1)` for the body segments, [src/ui/timeline.rs](file:///home/tylerstyle/git/matrix-rat/src/ui/timeline.rs#L178) prepends `row.prefix` (`200` columns) + `pad` (`8` columns) + body (`1` column) = `209` columns into the `Row`, exceeding `content.width`.
2. **HTML `<tr>`/`<td>`/`<th>` table tags:** Outgoing Markdown enables `options.extension.table = true` ([src/format.rs](file:///home/tylerstyle/git/matrix-rat/src/format.rs#L101)), emitting `<table>`, `<tr>`, `<th>`, and `<td>`. In `rich.rs`, those tags are ignored, so incoming HTML tables render with all rows and cells concatenated on a single line (`"a1b1a2b2"`).

**Suggested Change:**
1. Cap visual quote and list indentation depth in `Builder::indent()` (e.g., `self.quotes.min(6)` and `self.lists.len().min(6)`), or truncate `row.prefix` in `styled()` so `pad_width + prefix_width < max_width`.
2. In `Builder::open_tag` / `close_tag`, treat `"tr"` like `"div"` (`self.block_break()`) and on `"td" | "th"` close (or open when `!self.at_line_start()`), push a cell separator such as `" │ "` or `"  "`.

---

### 6. [LOW] `timeline::draw` early return on empty room leaves `app.timeline.view_full` untouched
**Location:** [src/ui/timeline.rs](file:///home/tylerstyle/git/matrix-rat/src/ui/timeline.rs#L384-L398)

**Problem:**
When `room.messages.is_empty()`, `timeline::draw` sets `app.timeline.drawn = true;` and returns early at line 397 before updating `app.timeline.view_full`. On a fresh room `view_full` defaults to `false`, which allows `maybe_load_older` to back-paginate; however, if a room previously had `view_full == true` and its messages were cleared, `view_full` would remain `true` and suppress auto-pagination on the empty view.

**Suggested Change:**
Set `app.timeline.view_full = false;` alongside `app.timeline.drawn = true;` inside the `if room.messages.is_empty()` block at [src/ui/timeline.rs](file:///home/tylerstyle/git/matrix-rat/src/ui/timeline.rs#L384-L386).

---

## Verdict

**Approve with changes.**

All 6 findings from M3a are verified fixed, the architecture and test coverage (94 unit/snapshot tests + 2 live homeserver e2e tests) are solid, and M3 meets every requirement in `docs/PLAN.md` §10. Please fix Finding 1 (terminal escape/control/tab/bidi sanitization) and the medium polish items (Findings 2–4) before pushing the M3 commits.
