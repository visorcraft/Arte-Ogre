<!-- SPDX-FileCopyrightText: 2026 VisorCraft LLC -->
<!-- SPDX-License-Identifier: GPL-3.0-only -->

# assets/

Brand imagery for Arte Ogre. `ArteOgre.png` is the canonical master — every
other icon reproduction (the window/taskbar icon embedded in the `ogre` binary,
the in-app brand spots, the AppImage/desktop icon, the social card) derives
from it.

| File | Size | Purpose |
| ---- | ---- | ------- |
| `ArteOgre.png` | 1024×1024 | **Master raster** — the Ogre Face, transparent, cropped square + centered. Source of truth for every icon. |
| `ArteOgre.ico` | 16/32/48/64/128/256 | Multi-resolution icon for GitHub repo display, Windows, and `.ico` consumers (favicons, etc.). |
| `ArteOgre.svg` | scalable | Scalable wrapper that embeds the master at 512px (the subject is a raster, so this is not a hand-drawn vector). |
| `social-card.svg` | 1024×512 | Source-of-truth vector for the GitHub banner: dark field, the Ogre Face, wordmark, tagline, and footer. |
| `social-1024x512.png` | 1024×512 | GitHub social preview / OpenGraph card. Upload via **Settings → Social preview** on github.com. |
| `ogre.png`, `ogre_face.png` | 1254×1254 | Original opaque renders (white background). `ogre_face.png` is the source the master is cut from. |

## How the master was made

`ArteOgre.png` is `ogre_face.png` run through Arte Ogre's own **Remove
Background** (tolerance 0.20, edge cleanup 0.55 — the shipped defaults), then
trimmed to the subject and centered in a square with ~8% padding. The window /
taskbar / title-bar icon is `ArteOgre.png` embedded in the `ogre` binary via
`eframe`'s `with_icon`; the welcome screen, tools sidebar, and About dialog
render it through `ogre_ui::icons::brand_image`.

## Regenerating

Re-cut from `ogre_face.png` with Remove Background, then:

```sh
magick cut.png -trim +repage -background none -gravity center -extent <sq>x<sq> \
  -resize 1024x1024 assets/ArteOgre.png
magick assets/ArteOgre.png -define icon:auto-resize=256,128,64,48,32,16 assets/ArteOgre.ico
rsvg-convert -w 1024 -h 512 assets/social-card.svg -o assets/social-1024x512.png
```
