#!/usr/bin/env bash
# Build the tokumai Linux (x86_64) desktop bundles in a container and copy them
# out to ./dist-linux/  →  a .AppImage (send to anyone) and a .deb (Debian/Ubuntu).
#
# Requires Docker Desktop. On an Apple-Silicon Mac the x86 build is EMULATED and the
# first run is slow (compiles nym-sdk under emulation). For a fast build, use a native
# x86 Linux machine or the GitHub Actions workflow (.github/workflows/build-linux.yml).
set -euo pipefail
cd "$(dirname "$0")/.."

IMG=scrambleai-linux-build

if ! command -v docker >/dev/null 2>&1; then
  echo "Docker is not installed. Install Docker Desktop first: https://www.docker.com/products/docker-desktop/" >&2
  exit 1
fi

echo "→ building the Linux x86_64 image (first run compiles everything — be patient) …"
docker build --platform linux/amd64 -f docker/linux.Dockerfile -t "$IMG" .

echo "→ extracting bundles → dist-linux/"
rm -rf dist-linux && mkdir -p dist-linux
cid=$(docker create --platform linux/amd64 "$IMG")
docker cp "$cid:/app/target/release/bundle/appimage/." dist-linux/ 2>/dev/null || true
docker cp "$cid:/app/target/release/bundle/deb/."      dist-linux/ 2>/dev/null || true
docker rm "$cid" >/dev/null

echo
echo "✓ done — files in dist-linux/:"
ls -lh dist-linux/ 2>/dev/null || echo "  (nothing extracted — check the build output above)"
echo
echo "Share the .AppImage with anyone (one self-contained file); the .deb is for Debian/Ubuntu."
