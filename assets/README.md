# Icon

| File | For |
|---|---|
| `icon.svg` | The icon itself; every other file is made from it. Rounded square, for the README, launchers and articles. |
| `icon-64.png`, `icon-128.png`, `icon-256.png`, `icon-512.png` | The same, where SVG is not accepted. |
| `avatar.svg`, `avatar-512.png` | For room, space and profile pictures: the background fills the square and the drawing is smaller, so a round crop cuts nothing off. |
| `social-preview.svg`, `social-preview.png` | 1280×640, icon and name, for link preview cards. |

The colours are those of the default theme, `rat-dark`. The chef's hat is a nod to
[Ratatui](https://ratatui.rs), the library matrix-rat is built with; the drawing is this
project's own and under the same licence as the code (Apache-2.0).

The PNG files are made with librsvg:

```sh
for s in 64 128 256 512; do rsvg-convert -w $s -h $s icon.svg -o icon-$s.png; done
rsvg-convert -w 512 -h 512 avatar.svg -o avatar-512.png
rsvg-convert social-preview.svg -o social-preview.png
```
