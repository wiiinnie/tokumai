#!/usr/bin/env bash
# build-mac-signed.sh — release .dmg for macOS, signed with the Developer ID certificate and
# notarised by Apple, so Gatekeeper opens it without the "unverified developer" dance.
#
# Needs (in the repo-root .env, git-ignored — never in the shell history):
#   APPLE_ID=hermes-stakepool@proton.me         the business Apple ID
#   APPLE_PASSWORD=xxxx-xxxx-xxxx-xxxx          an APP-SPECIFIC password of that ID
#   APPLE_TEAM_ID=XXXXXXXXXX                    developer.apple.com → Membership
# and a "Developer ID Application: <name> (<team>)" certificate in the login keychain
# (Xcode → Settings → Accounts → the team → Manage Certificates → + → Developer ID Application).
#
# Tauri reads APPLE_SIGNING_IDENTITY / APPLE_ID / APPLE_PASSWORD / APPLE_TEAM_ID itself:
# signs the .app, submits the bundle to notarytool, staples the ticket, builds the .dmg.
# Without the certificate this falls back to nothing — the script refuses instead of
# silently producing an unsigned build.
set -euo pipefail
SRC="$(cd "$(dirname "$0")/.." && pwd)"
cd "$SRC"

if [ -f .env ]; then
  # only the four Apple variables — the rest of .env is server config
  while IFS='=' read -r k v; do
    case "$k" in APPLE_ID|APPLE_PASSWORD|APPLE_TEAM_ID|APPLE_SIGNING_IDENTITY) export "$k"="${v%\"}" ;; esac
  done < <(grep -E '^APPLE_(ID|PASSWORD|TEAM_ID|SIGNING_IDENTITY)=' .env | sed 's/="\(.*\)"$/=\1/')
fi
: "${APPLE_ID:?set APPLE_ID in .env}"
: "${APPLE_PASSWORD:?set APPLE_PASSWORD (app-specific) in .env}"
: "${APPLE_TEAM_ID:?set APPLE_TEAM_ID in .env}"

# The Developer ID identity, found by team id so a renamed certificate still matches.
if [ -z "${APPLE_SIGNING_IDENTITY:-}" ]; then
  APPLE_SIGNING_IDENTITY=$(security find-identity -v -p codesigning | grep "Developer ID Application" | grep "($APPLE_TEAM_ID)" | head -1 | sed -E 's/.*"(.*)"$/\1/')
  export APPLE_SIGNING_IDENTITY
fi
if [ -z "$APPLE_SIGNING_IDENTITY" ]; then
  echo "no 'Developer ID Application' certificate for team $APPLE_TEAM_ID in the keychain — create it in Xcode (Accounts → Manage Certificates)" >&2
  exit 1
fi
echo "→ signing as: $APPLE_SIGNING_IDENTITY"
echo "→ notarising with $APPLE_ID (team $APPLE_TEAM_ID)"

npm run -s tauri:build

APP="$SRC/target/release/bundle/macos/ScrambleAI.app"
DMG=$(ls -t "$SRC"/target/release/bundle/dmg/*.dmg | head -1)
echo "→ verifying"
codesign --verify --deep --strict "$APP" && echo "   codesign: ok"
spctl --assess --type execute -v "$APP" 2>&1 | sed 's/^/   spctl: /'
xcrun stapler validate "$APP" 2>&1 | tail -1 | sed 's/^/   stapler: /'
echo "✓ $DMG"
