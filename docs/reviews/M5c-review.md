# Review: Room Member Fetch, Initial Tiles, and Sidebar Room Avatars (`a982a45..HEAD` / `cfd0d33`)

Scope reviewed across commits `bd568be`, `f29c1c4`, and merge `cfd0d33` (`HEAD`):
- `timeline.fetch_members()` on room open (`src/matrix/rooms.rs`)
- Coloured initial tiles (`initial()` in `src/ui/sidebar.rs`)
- Room avatars in the room list (`src/matrix/mod.rs`, `src/matrix/media.rs`, `src/images.rs`, `src/ui/image.rs`, `src/ui/sidebar.rs`, `src/app.rs`, `src/app/media.rs`, `src/model.rs`)

---

## Findings

### 1. High — Shared avatar `mxc://` URL causes a Kitty re-encode / delete loop on every draw (`src/ui/image.rs`, `src/images.rs`)

`ImageStore::avatars` keys room pictures by their `mxc://` URL (`HashMap<String, Avatar>` at [`src/images.rs:530-532`](file:///home/tylerstyle/git/matrix-rat/src/images.rs#L530-L532)). If two visible rooms share the same avatar URL (for instance, identical room icons, or a bot/bridge hero avatar appearing across multiple DMs), both render in the same frame while `encoded.shown` is still `false` (since `verify` runs only at the end of `ui::draw`):

1. The first room calls [`draw_avatar`](file:///home/tylerstyle/git/matrix-rat/src/ui/image.rs#L63-L84) (`pending == true`), `render` triggers `ratatui_image`'s `KittyProtoState::make_transmit()`, which atomically flips `transmitted = true` and writes `\x1b_G...` into cell 1, and `draw_avatar` calls `images.sent_avatar(url, area.x, area.y)` ([`src/ui/image.rs:81-83`](file:///home/tylerstyle/git/matrix-rat/src/ui/image.rs#L81-L83)).
2. The second room enters `draw_avatar` in the same frame (`pending` is still `true`), calls `render`—where `make_transmit()` now returns `None`, so only the Unicode placeholder `\u{10EEEE}` is written—and **also** pushes `images.sent_avatar` for the second room's `area`.
3. At frame end, [`verify`](file:///home/tylerstyle/git/matrix-rat/src/ui/image.rs#L241-L248) runs [`ImageStore::verify`](file:///home/tylerstyle/git/matrix-rat/src/images.rs#L990-L1020), sees the second cell starts with `\u{10EEEE}` instead of `\x1b`, and calls `Self::discard(&mut self.garbage, slot.take())`.

That deletes the newly transmitted `kitty_id` (`a=d,d=I`) and resets `encoded = None`, forcing the avatar to re-encode, re-transmit, and immediately delete itself on every redraw while both rooms remain visible.

**Suggested fix:** In `draw_avatar`, only call `images.sent_avatar(url, area.x, area.y)` when the rendered first cell actually contains the transmission (`frame.buffer_mut().cell((area.x, area.y)).is_some_and(|c| c.symbol().starts_with('\x1b'))`), or skip pushing duplicate `Target::Avatar(url)` entries into `unverified` in the same frame.

---

### 2. Medium — Unaccepted invites auto-fetch remote `mxc://` avatars, and `fetch_avatar` shares `self.slots` with timeline images (`src/matrix/mod.rs`, `src/matrix/media.rs`, `src/ui/sidebar.rs`)

- **Privacy leak on `RoomCategory::Invite` (and comparison with inline images):** [`summarize`](file:///home/tylerstyle/git/matrix-rat/src/matrix/mod.rs#L605-L651) extracts `avatar` for all room categories including `RoomCategory::Invite`, and `sidebar::draw` ([`src/ui/sidebar.rs:159-167`](file:///home/tylerstyle/git/matrix-rat/src/ui/sidebar.rs#L159-L167)) requests avatars automatically for every visible row on startup or scroll in Kitty mode. Although [`media_url`](file:///home/tylerstyle/git/matrix-rat/src/matrix/media.rs#L283-L314) sends HTTP requests only to `client.homeserver()` (never directly to foreign servers, so user IP is not leaked directly), fetching an uncached `mxc://attacker.example/...` avatar makes the user's homeserver federate out to `attacker.example`. Any spammer sending an unsolicited room invite can therefore detect when the user launches `matrix-rat` and displays the invite in the sidebar without the user ever opening or accepting the invite.
  *(For joined rooms and DMs, fetching visible sidebar avatars without opening the room differs from inline pictures—which only fetch inside the open chat once scrolled into view—by trading startup/scroll visibility for a federated media request when an avatar is uncached on the user's homeserver, though proxied through the homeserver.)*
- **Starving inline images on slow remote avatars:** [`Media::fetch_avatar`](file:///home/tylerstyle/git/matrix-rat/src/matrix/media.rs#L104-L127) acquires the same `self.slots` semaphore (`PARALLEL = 4` at [`src/matrix/media.rs:32, 64`](file:///home/tylerstyle/git/matrix-rat/src/matrix/media.rs#L32)) as `Media::fetch` ([`src/matrix/media.rs:86`](file:///home/tylerstyle/git/matrix-rat/src/matrix/media.rs#L86)). Visible sidebar rows with slow or unresponsive federated homeservers can hold all 4 download slots up to `TIMEOUT = 60s`, blocking inline images and the full-screen viewer in the active chat.

**Suggested fix:** Omit `avatar` for `RoomCategory::Invite` (and consider a config option if users want avatars deferred until room open); give `fetch_avatar` its own concurrency budget or lower limit (or separate semaphore) so background avatars cannot starve `Media::fetch`.

---

### 3. Medium — Detached `timeline.fetch_members()` tasks: large-room cost, repeat rescans, and logout race (`src/matrix/rooms.rs`, `src/matrix/mod.rs`)

[`Timelines::timeline`](file:///home/tylerstyle/git/matrix-rat/src/matrix/rooms.rs#L72-L101) builds a new `Timeline` and immediately spawns `tokio::spawn(async move { members.fetch_members().await });` ([`src/matrix/rooms.rs:94-95`](file:///home/tylerstyle/git/matrix-rat/src/matrix/rooms.rs#L94-L95)):

- **Cost on large rooms:** Under `matrix-sdk-ui`, `Timeline::fetch_members()` calls `set_sender_profiles_pending()`, then `room.sync_members()` -> `request_members()`, issuing an unfiltered `GET /_matrix/client/v3/rooms/{roomId}/members` request (60s timeout) pulling *all* historical `m.room.member` events (joined, left, banned) and writing them to the state store. On large public or bridged rooms this causes a multi-megabyte HTTP/JSON/SQLite spike when opening the room, and the dropped `JoinHandle` ignores errors silently.
- **Repeated work:** While `Room::sync_members` skips the HTTP call when `are_members_synced()` is `true`, `Timeline::fetch_members` still walks all timeline items unconditionally (`set_sender_profiles_pending` + `update_missing_sender_profiles`). Furthermore, because `Timelines::cache` keeps only `KEEP_TIMELINES = 6` ([`src/matrix/rooms.rs:30, 96-99`](file:///home/tylerstyle/git/matrix-rat/src/matrix/rooms.rs#L30)) and `matrix-sdk` sliding sync resets `mark_members_missing()` on `limited: true` syncs or newly upserted room subscriptions, switching across >6 rooms repeats both the timeline scan and full `/members` downloads.
- **Detached tasks survive logout:** Unlike `forwarder` / `paginator` (`Timelines::close`) or `Media::fetches` (`JoinSet`), the `fetch_members` `JoinHandle` is detached. Fast room switching stacks background `/members` requests, and on logout `Worker::run` executes `drop(client); session::clear(&self.paths);` ([`src/matrix/mod.rs:417-418`](file:///home/tylerstyle/git/matrix-rat/src/matrix/mod.rs#L417-L418)) while detached `fetch_members` tasks still hold `Arc<Timeline>` -> `Room` -> `Client` SQLite handles.

**Suggested fix:** Track `fetch_members` tasks inside a `JoinSet` on `Timelines` (aborted on `close()`/`Drop` and deduplicated per `room_id`), skip `fetch_members` (or let timeline live events resolve) if `room.are_members_synced()` is already true, and log failures.

---

### 4. Medium — Deep `room.clone_info()` per DM on every list update, and missing avatars on favourited DMs (`src/matrix/mod.rs:612-636`)

- **Repeated `RoomInfo` deep clones:** [`summarize`](file:///home/tylerstyle/git/matrix-rat/src/matrix/mod.rs#L605-L651) iterates over *all* non-space rooms on every `entries.next()` diff batch (`src/matrix/mod.rs:537-542`). Whenever `room.avatar_url()` is `None` in a `RoomCategory::Person` room, [`src/matrix/mod.rs:631`](file:///home/tylerstyle/git/matrix-rat/src/matrix/mod.rs#L631) calls `room.clone_info()`. Because `RoomInfo` is not `Arc`-wrapped, this clones the entire `RoomInfo` and boxed `BaseRoomInfo` (topic, pinned events, `dm_targets`, `read_receipts`, `latest_event_value`, `rtc_member_events`, etc.) for every avatarless DM on every sync diff. Calling `room.heroes().await` (`matrix_sdk_base::Room::heroes`) clones only `Vec<RoomHero>`.
- **Favourited DMs lose hero avatars:** Because `category` checks `room.is_favourite()` ([`src/matrix/mod.rs:614-615`](file:///home/tylerstyle/git/matrix-rat/src/matrix/mod.rs#L614-L615)) before `room.is_direct()`, a favourited 1:1 DM becomes `RoomCategory::Favourite`, making `category == RoomCategory::Person` false at line 631 and suppressing its hero avatar.

**Suggested fix:** Save `let is_direct = room.is_direct().await.unwrap_or(false);` and read `room.heroes().await` whenever `is_direct && category != RoomCategory::Invite` instead of `room.clone_info()`.

---

### 5. Medium — `initial()` width expansion and active-room Kitty modifiers break alignment/rendering (`src/ui/sidebar.rs`, `src/ui/image.rs`)

- **`initial()` can emit 3–4 columns instead of 2 ([`src/ui/sidebar.rs:11-24`](file:///home/tylerstyle/git/matrix-rat/src/ui/sidebar.rs#L11-L24)):** `initial` checks `match width(first)` *before* calling `first.to_uppercase()`. Characters whose width is `1` in lowercase but expand on `.to_uppercase()` (e.g., `"ß"` -> `"SS"`, `"ﬁ"` -> `"FI"`, `"ﬃ"` -> `"FFI"`, `"և"` -> `"ԵՒ"`) return `"SS "` (3 columns) or `"FFI "` (4 columns), violating the 2-column assumption in `name_width = rect.width.saturating_sub(5 + badge_width)` ([`src/ui/sidebar.rs:139`](file:///home/tylerstyle/git/matrix-rat/src/ui/sidebar.rs#L139)) and shifting room names/badges out of alignment.
- **Active/cursor modifiers leak onto Kitty placeholders (`themes/terminal.toml`, [`src/ui/image.rs:77-79`](file:///home/tylerstyle/git/matrix-rat/src/ui/image.rs#L77-L79)):** `sidebar::draw` paints `Role::RoomActive` / `RoomCursor` / `Hover` onto `rect` ([`src/ui/sidebar.rs:121-129`](file:///home/tylerstyle/git/matrix-rat/src/ui/sidebar.rs#L121-L129)). In [`themes/terminal.toml:20-22`](file:///home/tylerstyle/git/matrix-rat/themes/terminal.toml#L20-L22), `room_active` uses `reversed = true` and `room_cursor` / `hover` use `underline = true`. Because `draw_avatar` clears only `Modifier::BOLD`, `REVERSED` stays on the active room's `\u{10EEEE}` cells and swaps `fg` (the `kitty_id` RGB color) with `bg` in Kitty terminals, corrupting the active room's avatar lookup (and `UNDERLINED` adds an underline artifact).

**Suggested fix:** Uppercase before checking `width(...)` in `initial()` (re-checking `width == 1 || width == 2` and falling back to `"? "` otherwise), and use `Modifier::all()` in `draw_avatar`'s `remove_modifier(...)`.

---

### 6. Low — Minor URL validation & thumbnail-fallback wastes (`src/matrix/mod.rs:645-647`, `src/matrix/media.rs:194-202, 283-314`)

- **Loose `mxc://` filtering:** [`src/matrix/mod.rs:645-647`](file:///home/tylerstyle/git/matrix-rat/src/matrix/mod.rs#L645-L647) calls `sanitize_line(uri.as_str()).filter(|uri| uri.starts_with("mxc://"))`, which turns `\n`/`\t` in malformed URIs into spaces instead of rejecting them early with `uri.is_valid()`. Also note `ruma-identifiers-validation` accepts `server_name = ".."` and empty `media_id = ""`, causing `url::Url::path_segments_mut().extend` ([`src/matrix/media.rs:305`](file:///home/tylerstyle/git/matrix-rat/src/matrix/media.rs#L305)) to drop `".."` and construct `/_matrix/client/v1/media/thumbnail/<id>` without a server segment. Checking `uri.parts()` with `!server.as_str().is_empty() && !matches!(server.as_str(), "." | "..") && !id.is_empty()` avoids nonsensical requests.
- **Thumbnail `TooLarge` triggers redundant full download:** In [`load_avatar`](file:///home/tylerstyle/git/matrix-rat/src/matrix/media.rs#L194-L206), `Err(_)` from the `96x96` cropped thumbnail request falls back to downloading the unscaled file (`None`), retrying up to `AVATAR_DOWNLOAD` (2 MiB) even when the thumbnail already failed with `Refused::TooLarge` rather than `Refused::Failed`.

*(Note on cache bounds and Kitty cleanup: `AVATAR_DOWNLOAD = 2 MiB` bounded reads, `decode_avatar` limits, `AVATAR_ENTRIES = 512` LRU eviction (~20 MB cap), `Media::fetch_avatar` running inside `self.fetches` `JoinSet`, and `forget_encoded`/`cleanup_sequence` freeing Kitty IDs on resize, suspend, and logout are otherwise well bounded. Running [`scripts/check.sh`](file:///home/tylerstyle/git/matrix-rat/scripts/check.sh) on `HEAD` `cfd0d33` passed `fmt`, `clippy`, `build`, and `176` unit tests with `8` ignored homeserver tests.)*

---

## Verdict

**Approve with changes.**

Fix the shared-URL `sent_avatar` / `verify` re-upload loop, stop auto-fetching `RoomCategory::Invite` avatars before acceptance, replace `clone_info()` (and fix favourite DMs + `REVERSED` Kitty modifiers / `initial()` width order), and track/abort `fetch_members` tasks cleanly before pushing.
