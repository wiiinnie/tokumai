# Build the ScrambleAI Linux desktop app (x86_64) → AppImage + .deb.
#
# A GTK/WebKit GUI app can't be cross-compiled cleanly from macOS, so we build
# inside a real x86_64 Ubuntu container. On an Apple-Silicon Mac this runs under
# emulation and is SLOW the first time (the nym-sdk compile can take a long while);
# a native x86 Linux box or the GitHub Actions workflow is much faster.
#
# Usage:  scripts/build-linux.sh   (builds this image + copies the bundles out)
FROM --platform=linux/amd64 ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive
# Tauri v2 Linux system deps (WebKitGTK 4.1, GTK3, appindicator, rsvg) + AppImage tooling.
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential curl wget file ca-certificates pkg-config \
      libssl-dev libgtk-3-dev libwebkit2gtk-4.1-dev librsvg2-dev \
      libayatana-appindicator3-dev patchelf desktop-file-utils libfuse2 \
 && rm -rf /var/lib/apt/lists/*

# Rust (minimal stable) + Tauri CLI v2
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
RUN cargo install tauri-cli --version "^2.0" --locked

WORKDIR /app
COPY . .

# Only the two shareable formats (skip rpm to keep the image lean).
RUN cargo tauri build --bundles appimage,deb

# Artifacts land in /app/target/release/bundle/{appimage,deb}/ — see build-linux.sh.
