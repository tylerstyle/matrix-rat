# matrix-rat

A mouse-friendly, themeable Matrix client for the terminal, built with
[ratatui](https://ratatui.rs) and [matrix-rust-sdk](https://github.com/matrix-org/matrix-rust-sdk).

**Status: early development.** It can log in (password or browser), sync, read and send
messages (replies, edits, reactions, mentions, Markdown, spoilers), show pictures inline
(kitty graphics, coloured blocks elsewhere), send and save files, follow threads, search a
room, narrow the room list to a space, join, create and manage rooms, notify about new
messages, and verify devices by emoji comparison. See [docs/PLAN.md](docs/PLAN.md) for the
roadmap.

```sh
cargo run             # log in to your account
cargo run -- --demo   # try the interface on built-in demo data
```

`F1` shows all keys, `F2` opens the settings, `F3` cycles themes, `Alt-E` opens the emoji
picker, `Ctrl-Q` quits.
`Ctrl-P` opens the command palette (actions, rooms, themes, settings), `Ctrl-↑`/`Ctrl-↓` switch
rooms, `Alt-A` attaches a file (or drop one into the terminal).
`Ctrl-F` finds text in the open room, `Alt-S` steps through your spaces; in the composer `@`,
`:`, `#` and `/` offer names, emoji, rooms and commands (Tab accepts).
`Ctrl-N` joins a room by its address, starts a direct chat or creates a room; `F4` shows the
room's details and members, and is where you invite people, rename, mute, favourite or leave.
On a selected message: `t` opens its thread, `Enter` replies, `e` edits, `r` reacts, `o` opens a link, picture or
file, `s` saves a file or picture to the download folder (set it in the settings).
A right click opens a menu of what can be done with the message, link, room or draft under the
pointer; `m` (or the menu key) opens the same menu for the selected message.
Messages say who has read up to them: `✓ read` in a chat with one person, initials in a room
("Read by…" in the menu lists the names). `read_markers = false`, or the settings, turn that off.

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

Planned:

- Room list sidebar, chat view and composer; click anything to focus it
- Inline images (kitty / sixel / iTerm2, half-block fallback)
- End-to-end encryption, OAuth login, sliding sync
- Markdown and slash-command formatting, including spoilers
- Theme files, bundled palettes, live reload, and templates for Noctalia, matugen, wallust and pywal

## Licence

Apache-2.0. Parts are adapted from [iamb](https://github.com/ulyssa/iamb) (Apache-2.0); see `NOTICE`.

## Themes

Eleven themes are built in (`F3` cycles them, the settings menu `F2` lists them); `theme = "terminal"` follows your terminal's
own colours. Theme files in `~/.config/matrix-rat/themes/` are reloaded the moment they
change, so tools like Noctalia, matugen, wallust and pywal can recolour the app:
see [contrib/themes/README.md](contrib/themes/README.md).
