# Linux build (x86_64) + how to share the app

Cross-building a GTK/WebKit GUI app from macOS directly isn't practical, so we build
inside a real x86_64 Linux environment. Two ways:

## A) Local, on your Mac (Docker)
```
scripts/build-linux.sh
```
Requires Docker Desktop. Outputs `dist-linux/ScrambleAI_*.AppImage` and `*.deb`.
On Apple Silicon the x86 build is **emulated** → the first run is slow (nym-sdk).

## B) GitHub Actions (faster, free) — if the repo is on GitHub
`.github/workflows/build-linux.yml` builds on a native Ubuntu runner.
Trigger: Actions tab → "Build Linux (x86_64)" → Run workflow, or `git push --tags`.
Download the `.AppImage` / `.deb` from the run's **Artifacts**.

---

## How to send a Linux GUI app to someone

Linux has no single app store everyone uses, so you ship a file. Formats:

| Format | Who it's for | How they run it |
|---|---|---|
| **`.AppImage`** | **Anyone** — universal | Make it executable (`chmod +x` or right-click → Properties → "Allow executing"), then double-click. No install. One self-contained file, like a portable `.app`/`.exe`. |
| **`.deb`** | Debian / Ubuntu / Mint | Double-click (opens the software installer) or `sudo apt install ./ScrambleAI_*.deb`. Proper install + menu entry. |
| **`.rpm`** | Fedora / openSUSE / RHEL | Same idea, different distro family. |
| Flatpak / Snap | "App store" style, sandboxed | More work to publish — overkill for sharing to a few people. |

**Recommendation:** hand out the **AppImage** as the "just send it to anyone" file, and
the **`.deb`** for Ubuntu/Debian users who want a real install. Both come out of the build.

**Sending it:** it's just a file — a share link (Drive/Dropbox/WeTransfer) is easiest
(AppImages are ~80–120 MB, often too big for email). No macOS-style signing/notarization
exists on Linux desktop, so recipients won't get an "unidentified developer" gate — but
they will need to tick "allow executing" on an AppImage the first time. That's normal on
Linux; nothing to worry about.
