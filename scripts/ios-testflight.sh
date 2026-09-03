#!/usr/bin/env bash
# ios-testflight.sh — archive the iOS app with the paid team, export it for App Store
# Connect and upload it to TestFlight. Three steps that each have a trap (all hit on
# 2026-08-29, all handled here):
#
#   1. `tauri ios build --export-method app-store-connect` builds + signs the ARCHIVE fine
#      but its own export step fails (xcodebuild can't reach an App Store Connect session
#      from a terminal: "No provider associated with App Store Connect user"). We only use
#      its archive: src-tauri/gen/apple/build/scrambleai_iOS.xcarchive.
#   2. Export with an App Store Connect API key that has the ADMIN role (App Manager cannot
#      regenerate the store profile → "Cloud signing permission error"), and with Apple's
#      /usr/bin first in PATH: Xcode's IPA step spawns `rsync` from PATH and Homebrew's
#      rsync 3.4 rejects Apple's --extended-attributes ("unexpected end of file").
#   3. Upload with altool + the same key (no Apple-ID password, no 2FA prompt).
#
# Needs in the repo-root .env (git-ignored):
#   ASC_ADMIN_KEY_ID=…   ASC_ISSUER_ID=…     and ~/.appstoreconnect/private_keys/AuthKey_<id>.p8
# Bundle must not carry libapp.a (project.yml: Externals → buildPhase: none) — checked below.
set -euo pipefail
SRC="$(cd "$(dirname "$0")/.." && pwd)"
cd "$SRC"
eval "$(grep -E '^ASC_(ADMIN_KEY_ID|ISSUER_ID)=' .env)"
: "${ASC_ADMIN_KEY_ID:?set ASC_ADMIN_KEY_ID in .env (an App Store Connect API key with the Admin role)}"
: "${ASC_ISSUER_ID:?set ASC_ISSUER_ID in .env}"
KEY="$HOME/.appstoreconnect/private_keys/AuthKey_${ASC_ADMIN_KEY_ID}.p8"
[ -f "$KEY" ] || { echo "missing $KEY" >&2; exit 1; }
APPLE="$SRC/src-tauri/gen/apple"
EXPORT_PLIST="$APPLE/build/asc-export.plist"

echo "→ 1/3 archive (tauri ios build; its export step is expected to fail)"
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$PATH"
rm -rf "$APPLE/Externals/x86_64" "$APPLE/Externals/arm64/debug"   # one config in Externals, or Xcode sees duplicate libapp.a
npm run -s tauri ios build -- --export-method app-store-connect >/dev/null 2>&1 || true
ARCHIVE="$APPLE/build/scrambleai_iOS.xcarchive"
[ -d "$ARCHIVE" ] || { echo "no archive at $ARCHIVE — run 'npm run tauri ios build' and read its output" >&2; exit 1; }
if [ -e "$ARCHIVE/Products/Applications/tokumai.app/libapp.a" ]; then
  echo "libapp.a is inside the bundle — App Store Connect rejects that. In gen/apple/project.yml the Externals source needs 'buildPhase: none', then 'xcodegen generate'." >&2
  exit 1
fi

# Build number: Tauri writes CFBundleVersion = the app version, so a SECOND upload of the
# same version (a fix before the first one shipped) is rejected as a duplicate. Export
# re-signs the bundle anyway, so stamp a monotonic timestamp build number into the archive
# first (YYYYMMDDHHMM — one integer, always higher than any earlier build).
BUILD_NO=$(date -u +%Y%m%d%H%M)
APP_PLIST="$ARCHIVE/Products/Applications/tokumai.app/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $BUILD_NO" "$APP_PLIST"
/usr/libexec/PlistBuddy -c "Set :ApplicationProperties:CFBundleVersion $BUILD_NO" "$ARCHIVE/Info.plist" 2>/dev/null || true
echo "→ build $(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP_PLIST") ($BUILD_NO)"

echo "→ 2/3 export for App Store Connect"
mkdir -p "$APPLE/build"
cat > "$EXPORT_PLIST" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>method</key><string>app-store-connect</string>
  <key>destination</key><string>export</string>
  <key>teamID</key><string>T23Z4LDMGV</string>
  <key>signingStyle</key><string>automatic</string>
  <key>uploadSymbols</key><true/>
  <key>manageAppVersionAndBuildNumber</key><false/>
</dict></plist>
EOF
export PATH="/usr/bin:/bin:/usr/sbin:/sbin:$PATH"
rm -rf "$APPLE/build/asc"
xcodebuild -exportArchive -archivePath "$ARCHIVE" -exportOptionsPlist "$EXPORT_PLIST" -exportPath "$APPLE/build/asc" \
  -allowProvisioningUpdates -authenticationKeyPath "$KEY" -authenticationKeyID "$ASC_ADMIN_KEY_ID" -authenticationKeyIssuerID "$ASC_ISSUER_ID" \
  | grep -E "EXPORT (SUCCEEDED|FAILED)|error:" || true
IPA="$APPLE/build/asc/tokumai.ipa"
[ -f "$IPA" ] || { echo "export produced no ipa — see the xcdistributionlogs bundle in \$TMPDIR" >&2; exit 1; }

echo "→ 3/3 upload $(du -h "$IPA" | cut -f1) to App Store Connect"
xcrun altool --upload-app -t ios -f "$IPA" --apiKey "$ASC_ADMIN_KEY_ID" --apiIssuer "$ASC_ISSUER_ID" 2>&1 | grep -E "UPLOAD SUCCEEDED|ERROR|WARN" || true
echo "✓ done — App Store Connect → TestFlight shows the build after Apple's processing (10–30 min)"
