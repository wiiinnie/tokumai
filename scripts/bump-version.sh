#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# bump-version.sh — set the ONE repo version everywhere it is written down:
#   Cargo.toml        [workspace.package] version   (core, server, ledger, src-tauri inherit)
#   Cargo.lock        the workspace members' entries
#   package.json      + package-lock.json
#   src-tauri/tauri.conf.json   (what the app reports as `app` / shows in Settings)
#
# Usage:  scripts/bump-version.sh 0.4.6
# Then:   git commit -am "0.4.6: …" && git tag v0.4.6
# The server picks the version up on the next scripts/deploy.sh (it reports it at
# boot, on the models reply and in scrai-admin).
# ---------------------------------------------------------------------------
set -euo pipefail
V="${1:-}"
if ! [[ "$V" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "usage: scripts/bump-version.sh <major.minor.patch>" >&2; exit 2
fi
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

CUR="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
echo "→ $CUR → $V"

# Cargo workspace version (first `version = "…"` line is the [workspace.package] one).
perl -0pi -e 's/(\[workspace\.package\]\nversion = ")[^"]+(")/${1}'"$V"'${2}/' Cargo.toml
grep -q "^version = \"$V\"" Cargo.toml || { echo "Cargo.toml: version not updated" >&2; exit 1; }
# Refresh the members' entries in Cargo.lock without touching dependency versions.
cargo update --workspace --offline -q

# tauri.conf.json — the top-level "version" key only (it sits on its own line).
perl -pi -e 's/^(\s*"version":\s*")[^"]+(",)$/${1}'"$V"'${2}/ if $. <= 10' src-tauri/tauri.conf.json
grep -q "\"version\": \"$V\"" src-tauri/tauri.conf.json || { echo "tauri.conf.json: version not updated" >&2; exit 1; }

# package.json + package-lock.json
npm version "$V" --no-git-tag-version --allow-same-version >/dev/null

echo "✓ Cargo.toml · Cargo.lock · package.json · package-lock.json · src-tauri/tauri.conf.json = $V"
git --no-pager diff --stat -- Cargo.toml Cargo.lock package.json package-lock.json src-tauri/tauri.conf.json
