# Source screenshots

The originals the site's images are cut from — full-resolution device captures, kept so a
crop or a size can be redone without asking for the shot again.

They live HERE and not under `public/` on purpose: `public/` is the app's frontend
(`tauri.conf.json` → `frontendDist`), so every file in it is bundled into every macOS,
iOS, Android, Windows and Linux build. Source images would ride along in all five
installers and never be read.

What is made from them (see `server/site/img/`, registered in `scrai-faucet.rs` IMAGES):

| source | site image | where |
|---|---|---|
| `MacOS_ImageGen.png` | `hero-imagegen-dark.jpg` | hero, cropped to the window edges |
| `ios_startscreen.PNG` | `how-phone-start-dark.jpg` | 01 · how it works |
| `ios_imagegen.PNG` | `how-phone-image-dark.jpg` | 01 · how it works |
| `ios_networktuning.PNG` | `how-phone-network-dark.jpg` | 01 · how it works |
| `ios_bootup.PNG` | `flow-ready-dark.jpg` | 02 · buy TOKU, step (a) |

All five are captures of the app's **dark** theme; the site serves the same bytes for the
`-light` name. Capture the light theme and the alias in `IMAGES` can become a real file.
