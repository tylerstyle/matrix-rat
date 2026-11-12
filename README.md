# matrix-rat

A mouse-friendly, themeable Matrix client for the terminal, built with
[ratatui](https://ratatui.rs) and [matrix-rust-sdk](https://github.com/matrix-org/matrix-rust-sdk).

**Status: early development.** The interface runs on demo data; it does not connect to
Matrix yet. See [docs/PLAN.md](docs/PLAN.md) for the roadmap.

```sh
cargo run          # F1 shows all keys, F3 cycles themes, Ctrl-Q quits
```

Planned:

- Room list sidebar, chat view and composer; click anything to focus it
- Inline images (kitty / sixel / iTerm2, half-block fallback)
- End-to-end encryption, OAuth login, sliding sync
- Markdown and slash-command formatting, including spoilers
- Theme files, bundled palettes, live reload, and templates for Noctalia, matugen, wallust and pywal

## Licence

Apache-2.0. Parts are adapted from [iamb](https://github.com/ulyssa/iamb) (Apache-2.0); see `NOTICE`.
