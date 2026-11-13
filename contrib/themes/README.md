# Colouring matrix-rat from your desktop

matrix-rat reads themes from `~/.config/matrix-rat/themes/<name>.toml` and reloads the
active one the moment its file changes. So any tool that can fill in a template can
recolour the app: let it write a theme file there and select that theme.

Without any setup, `theme = "terminal"` already follows whatever colours your terminal
uses.

The commands below assume `matrix-rat` is installed (see the main README). From a
checkout you can use `cargo run --quiet -- --print-template …` instead, and leave out the
`post_hook` lines: they only speed up what the file watcher does anyway.

The templates in this directory are also built into the program:

```sh
matrix-rat --print-template noctalia   # also: matugen, wallust, pywal
```

> The Noctalia template is tested with Noctalia v5.4.1. The others were written from each
> tool's documentation and have not yet been run against the tools themselves. Reports
> and fixes are welcome.

## Noctalia v5

```sh
mkdir -p ~/.config/noctalia/templates ~/.config/matrix-rat/themes
matrix-rat --print-template noctalia > ~/.config/noctalia/templates/matrix-rat.toml
```

Add to `~/.config/noctalia/templates.toml` (or any `*.toml` in that directory):

```toml
[theme.templates.user.matrix-rat]
input_path  = "$XDG_CONFIG_HOME/noctalia/templates/matrix-rat.toml"
output_path = "$XDG_CONFIG_HOME/matrix-rat/themes/noctalia.toml"
post_hook   = "matrix-rat --reload-theme"
```

Then set `theme = "noctalia"` in `~/.config/matrix-rat/config.toml` (or pick it in the
settings, `F2`), and apply once with `noctalia msg templates-apply`.

The `post_hook` is optional; the file watcher notices the change by itself. If you keep
it: hooks run in Noctalia's own environment, so either `matrix-rat` must be on the `PATH`
Noctalia was started with, or the hook needs the full path (for example
`~/.cargo/bin/matrix-rat --reload-theme`). The `$XDG_CONFIG_HOME` paths work even when
that variable is not set; Noctalia falls back to `~/.config`.

Tested with Noctalia v5.4.1.

## Noctalia v4 (legacy)

Enable *Settings → Color Scheme → Templates → Advanced → User templates*, then add to
`~/.config/noctalia/user-templates.toml`:

```toml
[templates.matrix-rat]
input_path  = "~/.config/noctalia/templates/matrix-rat.toml"
output_path = "~/.config/matrix-rat/themes/noctalia.toml"
post_hook   = "matrix-rat --reload-theme"
```

## matugen (also DankMaterialShell and other matugen-based shells)

```sh
matrix-rat --print-template matugen > ~/.config/matugen/templates/matrix-rat.toml
```

`~/.config/matugen/config.toml`:

```toml
[templates.matrix-rat]
input_path  = "~/.config/matugen/templates/matrix-rat.toml"
output_path = "~/.config/matrix-rat/themes/matugen.toml"
post_hook   = "matrix-rat --reload-theme"
```

Set `theme = "matugen"`.

## wallust

```sh
matrix-rat --print-template wallust > ~/.config/wallust/templates/matrix-rat.toml
```

`~/.config/wallust/wallust.toml`:

```toml
[templates]
matrix-rat = { template = "matrix-rat.toml", target = "~/.config/matrix-rat/themes/wallust.toml" }
```

Set `theme = "wallust"`.

## pywal / pywal16

```sh
matrix-rat --print-template pywal > ~/.config/wal/templates/matrix-rat.toml
ln -sf ~/.cache/wal/matrix-rat.toml ~/.config/matrix-rat/themes/pywal.toml
```

Set `theme = "pywal"`. Because the theme file is a link into pywal's cache, changes are
not noticed automatically; let pywal tell the app:

```sh
wal -i wallpaper.png -o "matrix-rat --reload-theme"
```

## Writing a theme by hand

Only `[palette]` is needed; every part of the interface derives its colours from it.
See `themes/*.toml` in the repository for complete examples, and add a `[ui]` table to
override single roles (`border_focused`, `unread_badge`, `link`, …).
