# matrix-rat

A mouse-friendly, themeable [Matrix](https://matrix.org) client for the terminal, built with
[ratatui](https://ratatui.rs) and [matrix-rust-sdk](https://github.com/matrix-org/matrix-rust-sdk).

Room list on the left, messages in the middle, composer at the bottom. Click anything to
focus it, or stay on the keyboard: you are in the composer by default, `Enter` sends, `↑`
walks into the messages.

**Status: early development.** Usable day to day by its author, tested against a local
homeserver and matrix.org-style accounts, but young: expect rough edges and keep another
client at hand.

```sh
cargo run             # log in to your account
cargo run -- --demo   # try the interface on built-in demo data
```

## What it does

- **Accounts:** password or browser (single sign-on) login, end-to-end encryption, device
  verification by emoji comparison, recovery key, message shields.
- **Messages:** replies, edits, reactions, deletion, read receipts and typing notices;
  Markdown as you type (`**bold**`, `` `code` ``, `||spoiler||`, lists, quotes) and slash
  commands (`/me`, `/spoiler`, `/plain`, `/html`, `/shrug`, `/upload`…); threads; who has read
  up to where.
- **Completions:** `@` people (sent as real mentions), `:` emoji, `#` rooms, `/` commands.
- **Media:** pictures inline and in a viewer (kitty graphics; coloured blocks elsewhere);
  send files by dropping them into the terminal; save attachments to a download folder.
- **Rooms:** join by address or `matrix.to` link, direct chats, create, invite, leave, room
  details and members; spaces narrow the room list; room pictures and last-message previews.
- **Finding things:** `Ctrl-F` in a room, `Ctrl-K` for rooms, `Ctrl-P` for everything.
- **Notifications:** desktop or terminal notifications with the chat's picture.
- **Looks:** eleven built-in themes, theme files that reload live, dark/light pairing, and
  templates for Noctalia, matugen, wallust and pywal so the app follows your desktop.
- **Yours to shape:** every key can be rebound (settings, or `[keys]` in the config), with
  an optional vim preset.

## Keys

`F1` lists all keys as they are currently bound. The defaults:

| Key | Does |
|---|---|
| `Enter` / `Shift-Enter` | send / line break |
| `↑` from the composer | select messages; then `Enter` reply, `e` edit, `r` react, `t` thread, `o` open, `s` save, `c` copy, `m` menu, `Del` delete |
| `Tab`, `Alt-1`…`3`, `Alt-←↑↓→` | move between room list, messages and composer |
| `Ctrl-↑` / `Ctrl-↓` | previous / next room |
| `Ctrl-K` | find a room |
| `Ctrl-F` | find in this room |
| `Ctrl-P` | command palette |
| `Ctrl-N` | join a room, start a chat, create a room |
| `F4` | room details and members |
| `Alt-S` | step through spaces |
| `Alt-E` / `Alt-A` | emoji picker / attach a file |
| `F2` / `F3` | settings / next theme |
| `Ctrl-Q` | quit |

A right click opens a menu for the message, link, room or draft under the pointer.

## Installing

To have `matrix-rat` on your `PATH` (theme generators that call `matrix-rat --reload-theme`
need that):

```sh
cargo install --path .     # installs to ~/.cargo/bin
nix-env -if .              # or with Nix
```

On NixOS, add it to your configuration instead:

```nix
environment.systemPackages = [ (pkgs.callPackage /path/to/matrix-rat { }) ];
```

Without installing, every `matrix-rat …` command in the documentation can be run from the
repository as `cargo run -- …`.

Building needs Rust 1.96 or newer and a C toolchain with `cmake` and `perl` (for TLS and the
bundled SQLite); `shell.nix` provides all of it.

## Configuration

Everything is optional. `~/.config/matrix-rat/config.toml` holds the settings (the settings
menu, `F2`, writes them and keeps your comments); `~/.config/matrix-rat/themes/` holds theme
files. `--profile <name>` keeps separate accounts apart.

## Themes

Eleven themes are built in (`F3` cycles them, the settings list them); `theme = "terminal"`
follows your terminal's own colours. Theme files in `~/.config/matrix-rat/themes/` are
reloaded the moment they change, so tools like Noctalia, matugen, wallust and pywal can
recolour the app: see [contrib/themes/README.md](contrib/themes/README.md).

To use one theme in the dark and another in the light, switch on "Follow dark/light" in the
settings (or set `theme_dark` and `theme_light`). The app picks the mode at start from the
desktop's setting, else from the terminal's background colour, and follows the desktop's
setting while running (through the XDG desktop portal; not over SSH). `F3` then changes the
theme of the current mode.

## Key bindings

Open the interactive editor via **Settings → Key bindings** (`F2`) or edit `~/.config/matrix-rat/config.toml` directly (`F1` always opens the key overview):

```toml
# Optional preset: "default" (off by default) or "vim"
preset = "vim"

[keys.global]
settings = "f5"
cycle_theme = "none" # or [] to unbind

[keys.timeline]
reply = ["enter", "R"]
```

Entries placed directly under `[keys]` (outside `[keys.<context>]`) apply to each action's default context. Within one context, duplicate keys warn and the last declaration in the file wins; pane contexts (`sidebar`, `timeline`, `composer`) shadow `global`. Unbinding every `quit` key restores `ctrl-q` automatically so you cannot lock yourself out. Plain printable characters always type into the composer box regardless of global or preset bindings.

### Key syntax & terminal limits

- **Modifiers**: `ctrl-`, `alt-`, `shift-` (case-insensitive, e.g. `ctrl-q`, `alt-up`, `shift-enter`).
- **Named keys**: `enter` (`return`), `esc` (`escape`), `tab`, `backtab`, `space`, `backspace` (`bs`), `del` (`delete`), `ins` (`insert`), `home`, `end`, `pgup` (`pageup`), `pgdn` (`pagedown`), `up`, `down`, `left`, `right`, `f1`–`f12`.
- **Characters**: any single non-control character (`e`, `R`, `/`, `?`).
- **Rejected combinations**: `ctrl-i` (Tab), `ctrl-m` (Enter), `ctrl-[` (Esc), and `ctrl-c` (clear draft) are reserved by terminal input/app behaviour; write `"R"` instead of `"shift-r"`, and omit `shift-` with `ctrl-`/`alt-` on characters because legacy and normalized terminal input do not keep them distinct.

### Action names

- **Everywhere (`global`)**: `quit`, `suspend`, `focus_next`, `focus_prev`, `focus_sidebar`, `focus_timeline`, `focus_composer`, `focus_left`, `focus_right`, `focus_up`, `focus_down`, `focus_room_filter`, `help`, `cycle_theme`, `back`, `page_up`, `page_down`, `emoji`, `attach`, `search`, `space_next`, `space_prev`, `room_prev`, `room_next`, `settings`, `palette`, `new_room`, `room_info`, `leave_room`, `invite_user`, `accept_invite`, `decline_invite`, `start_verification`, `close_thread`.
- **Room list (`sidebar`)**: `sidebar_up`, `sidebar_down`, `sidebar_top`, `sidebar_bottom`, `sidebar_open`.
- **Messages (`timeline`)**: `select_older`, `select_newer`, `reply`, `edit`, `react`, `delete`, `copy`, `save`, `open`, `open_thread`, `jump_to_newest`, `jump_to_oldest`.
- **Composer (`composer`)**: `send`, `newline`, `cancel_compose`.

### `vim` preset

Setting `preset = "vim"` keeps the default keys and adds:

- **Room list (`sidebar`)**: `j`/`k` move cursor, `g`/`G` top/bottom, `l` focus messages, `i` focus composer, `/` find in room.
- **Messages (`timeline`)**: `j`/`k` select newer/older message, `g`/`G` oldest/newest, `h` focus room list, `l`/`i` focus composer, `/` find in room.
- **Composer (`composer`)**: `Esc` returns focus to messages (plain letters and symbols always type).

## Licence

Apache-2.0. Parts are adapted from [iamb](https://github.com/ulyssa/iamb) (Apache-2.0); see `NOTICE`.
