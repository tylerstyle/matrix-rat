# matrix-rat

A mouse-friendly, themeable Matrix client for the terminal, built with
[ratatui](https://ratatui.rs) and [matrix-rust-sdk](https://github.com/matrix-org/matrix-rust-sdk).

**Status: early development.** It can log in, sync, and read and send messages (replies, edits,
reactions, Markdown, formatted messages, spoilers). Images are not displayed yet. See [docs/PLAN.md](docs/PLAN.md) for the roadmap.

```sh
cargo run             # log in to your account
cargo run -- --demo   # try the interface on built-in demo data
```

`F1` shows all keys, `F3` cycles themes, `Alt-E` opens the emoji picker, `Ctrl-Q` quits.

Planned:

- Room list sidebar, chat view and composer; click anything to focus it
- Inline images (kitty / sixel / iTerm2, half-block fallback)
- End-to-end encryption, OAuth login, sliding sync
- Markdown and slash-command formatting, including spoilers
- Theme files, bundled palettes, live reload, and templates for Noctalia, matugen, wallust and pywal

## Licence

Apache-2.0. Parts are adapted from [iamb](https://github.com/ulyssa/iamb) (Apache-2.0); see `NOTICE`.

## Themes

Five themes are built in (`F3` cycles them); `theme = "terminal"` follows your terminal's
own colours. Theme files in `~/.config/matrix-rat/themes/` are reloaded the moment they
change, so tools like Noctalia, matugen, wallust and pywal can recolour the app:
see [contrib/themes/README.md](contrib/themes/README.md).
