#!/usr/bin/env bash
# publish-downloads.sh — put the locally built app bundles on the download site.
#
# Uploads every release artefact found locally (macOS .dmg from `npm run tauri:build`,
# Linux .AppImage/.deb dropped into dist/downloads/) to the VPS and installs them into
# /opt/scrai/site/dl, which Caddy serves as https://<site>/dl/<file>. Prints the sha256
# of each file and the SCRAI_DL_* lines to paste into /opt/scrai/.env (the site shows a
# download button only for links present there; `systemctl restart scrai-faucet` after).
#
# Usage:  scripts/publish-downloads.sh <admin_user>@<vps-host> [https://site-host]
#         (target falls back to SCRAI_DEPLOY_TARGET; site host defaults to
#          https://scrai-faucet.hermes-stakepool.de)
#
# One sudo prompt on the VPS (the target dir belongs to scrai, not the admin user).
set -euo pipefail

TARGET="${1:-${SCRAI_DEPLOY_TARGET:-}}"
SITE="${2:-https://scrai-faucet.hermes-stakepool.de}"
if [ -z "$TARGET" ]; then
  echo "usage: scripts/publish-downloads.sh <admin_user>@<vps-host> [https://site-host]" >&2
  exit 2
fi
SRC="$(cd "$(dirname "$0")/.." && pwd)"
SSH_OPTS=(-o ControlMaster=auto -o ControlPath="/tmp/scrai-pub-%r@%h:%p" -o ControlPersist=300)

# What is there to publish? Newest of each kind wins.
files=()
dmg=$(ls -t "$SRC"/target/release/bundle/dmg/*.dmg 2>/dev/null | head -1 || true)
[ -n "$dmg" ] && files+=("$dmg")
for f in "$SRC"/dist/downloads/*.AppImage "$SRC"/dist/downloads/*.deb; do
  [ -f "$f" ] && files+=("$f")
done
if [ ${#files[@]} -eq 0 ]; then
  echo "nothing to publish: run 'npm run tauri:build' (dmg) or drop Linux builds into dist/downloads/" >&2
  exit 1
fi

echo "→ files:"
for f in "${files[@]}"; do printf '   %s  (%s)\n' "$(basename "$f")" "$(du -h "$f" | cut -f1)"; done

echo "→ upload → $TARGET:~/scrai-stage/dl/"
ssh "${SSH_OPTS[@]}" "$TARGET" 'mkdir -p ~/scrai-stage/dl'
rsync -a --info=progress2 -e "ssh ${SSH_OPTS[*]}" "${files[@]}" "$TARGET:~/scrai-stage/dl/"

echo "→ install into /opt/scrai/site/dl (sudo once)"
ssh -t "${SSH_OPTS[@]}" "$TARGET" '
  set -e
  sudo install -d -o scrai -g scrai -m 755 /opt/scrai/site/dl
  sudo install -o scrai -g scrai -m 644 ~/scrai-stage/dl/* /opt/scrai/site/dl/
  ls -la /opt/scrai/site/dl/
'

echo
echo "→ paste into /opt/scrai/.env, then: sudo systemctl restart scrai-faucet"
for f in "${files[@]}"; do
  name=$(basename "$f")
  sum=$(shasum -a 256 "$f" | cut -c1-64)
  case "$name" in
    *.dmg)      var=SCRAI_DL_MACOS ;;
    *.AppImage) var=SCRAI_DL_LINUX_APPIMAGE ;;
    *.deb)      var=SCRAI_DL_LINUX_DEB ;;
    *)          var=SCRAI_DL_OTHER ;;
  esac
  echo "$var=$SITE/dl/$name"
  [ "$var" != SCRAI_DL_LINUX_DEB ] && echo "${var}_SHA256=$sum"
done
ver=$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$SRC/src-tauri/tauri.conf.json" | head -1)
[ -n "$ver" ] && echo "SCRAI_SITE_VERSION=$ver testnet"
