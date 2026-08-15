#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# deploy.sh — push the scrai-SERVER to the VPS, rebuild, restart.
#
# Only the server needs to ship — the Tauri desktop app (src-tauri/, public/)
# is excluded. Runtime state on the VPS (.env, data/, node_modules/, dist/) is
# preserved; everything else under /opt/scrai is replaced by the local source.
#
# Usage:  scripts/deploy.sh <admin_user>@<vps-host>
#         (admin_user = a SUDO user on the VPS; the service itself runs as `scrai`)
#   e.g.  scripts/deploy.sh deploy@vps-2a46fb3c.example.net
# ---------------------------------------------------------------------------
set -euo pipefail

TARGET="${1:-${SCRAI_DEPLOY_TARGET:-}}"
if [ -z "$TARGET" ]; then
  echo "usage: scripts/deploy.sh <admin_user>@<vps-host>" >&2
  exit 1
fi
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "→ 1/3  sync source → $TARGET:~/scrai-stage/"
rsync -az --delete \
  --exclude .git --exclude node_modules --exclude dist --exclude .env \
  --exclude data --exclude images --exclude bin --exclude .claude \
  --exclude src-tauri --exclude public --exclude '.DS_Store' \
  "$SRC/" "$TARGET:~/scrai-stage/"

echo "→ 2/3  install into /opt/scrai + build   (sudo on the VPS — password prompt)"
echo "→ 3/3  restart scrai.service"
# Single-quoted: this whole block runs on the VPS. `~` expands to the admin's home
# there; /opt/scrai + the scrai user are our fixed server-side convention.
ssh -t "$TARGET" '
  set -e
  sudo rsync -a --delete \
    --exclude .env --exclude data --exclude node_modules --exclude dist \
    ~/scrai-stage/ /opt/scrai/
  sudo chown -R scrai:scrai /opt/scrai
  sudo -u scrai HOME=/opt/scrai bash -lc "cd /opt/scrai && npm ci --include=dev && npm run build"
  sudo systemctl restart scrai
  sudo systemctl --no-pager status scrai | head -6
'
echo "✓ deployed. Follow logs:  ssh $TARGET 'journalctl -u scrai -f'"
