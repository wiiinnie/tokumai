#!/usr/bin/env bash
# publish-downloads.sh — put the locally built app bundles on the download site.
#
# Uploads every release artefact found locally (macOS .dmg from `npm run tauri:build`,
# Windows .exe and Linux .AppImage/.deb from the CI workflows, dropped into dist/downloads/) to the VPS and installs them into
# /opt/scrai/site/dl, which Caddy serves as https://<site>/dl/<file>. Prints the sha256
# of each file and the DL_* lines to paste into /opt/scrai/.env (the site shows a
# download button only for links present there; `systemctl restart scrai-faucet` after).
#
# Usage:  scripts/publish-downloads.sh <admin_user>@<vps-host> [https://site-host] [--force]
#         (target falls back to DEPLOY_TARGET; site host defaults to
#          https://scrai-faucet.hermes-stakepool.de)
#
# One sudo prompt on the VPS (the target dir belongs to scrai, not the admin user).
set -euo pipefail

TARGET="${1:-${DEPLOY_TARGET:-}}"
SITE="${2:-https://scrai-faucet.hermes-stakepool.de}"
if [ -z "$TARGET" ]; then
  echo "usage: scripts/publish-downloads.sh <admin_user>@<vps-host> [https://site-host]" >&2
  exit 2
fi
SRC="$(cd "$(dirname "$0")/.." && pwd)"
SSH_OPTS=(-o ControlMaster=auto -o ControlPath="/tmp/scrai-pub-%r@%h:%p" -o ControlPersist=300)

# What is there to publish? Newest of each kind wins. Incremental: a file whose sha256
# already matches the server's manifest is skipped, and manifest entries for platforms
# not present locally are kept — so adding one platform re-uploads nothing else, while a
# new version (all files changed) replaces everything. --force re-uploads all.
FORCE=0; [ "${3:-}" = "--force" ] && FORCE=1
files=()
dmg=$(ls -t "$SRC"/target/release/bundle/dmg/*.dmg 2>/dev/null | head -1 || true)
[ -n "$dmg" ] && files+=("$dmg")
for f in "$SRC"/dist/downloads/*.exe "$SRC"/dist/downloads/*.AppImage "$SRC"/dist/downloads/*.deb "$SRC"/dist/downloads/*.apk; do
  [ -f "$f" ] && files+=("$f")
done
if [ ${#files[@]} -eq 0 ]; then
  echo "nothing to publish: run 'npm run tauri:build' (dmg) or drop CI builds into dist/downloads/" >&2
  exit 1
fi

# Manifest version: PUBLISH_VERSION wins (publishing an older public build while the
# tree already carries the next version), else tauri.conf.json.
ver="${PUBLISH_VERSION:-$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$SRC/src-tauri/tauri.conf.json" | head -1)}"
for f in "${files[@]}"; do
  case "$(basename "$f")" in tokumai_${ver}_*) ;; *) echo "   ! $(basename "$f") is not version $ver — set PUBLISH_VERSION or remove the file" >&2;; esac
done
echo "→ version $ver · local files:"
for f in "${files[@]}"; do printf '   %s  (%s)\n' "$(basename "$f")" "$(du -h "$f" | cut -f1)"; done

echo "→ reading the server's manifest"
REMOTE=$(ssh "${SSH_OPTS[@]}" "$TARGET" 'cat /opt/scrai/site/dl/manifest.json 2>/dev/null || echo "{}"')

# Decide what to upload and build the merged manifest (python: JSON without extra tools).
MANIFEST="$SRC/target/manifest.json"
mkdir -p "$(dirname "$MANIFEST")"
UPLOAD=$(REMOTE="$REMOTE" VER="$ver" FORCE="$FORCE" MANIFEST="$MANIFEST" python3 - "${files[@]}" <<'PY'
import hashlib, json, os, sys, datetime
try:
    remote = json.loads(os.environ["REMOTE"] or "{}")
except Exception:
    remote = {}
old = remote.get("files", {}) if isinstance(remote, dict) else {}
key_of = lambda n: ("macos" if n.endswith(".dmg") else "windows" if n.endswith(".exe")
                    else "appimage" if n.endswith(".AppImage") else "deb" if n.endswith(".deb")
                    else "android" if n.endswith(".apk") else "other")
files = dict(old)          # keep what the server already has
upload = []
for path in sys.argv[1:]:
    name = os.path.basename(path)
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    sha = h.hexdigest()
    key = key_of(name)
    prev = old.get(key)
    if os.environ["FORCE"] != "1" and prev and prev.get("sha256") == sha and prev.get("name") == name:
        print(f"   = {name} unchanged (already on the server)", file=sys.stderr)
        continue
    files[key] = {"name": name, "sha256": sha, "bytes": os.path.getsize(path)}
    upload.append(path)
    print(f"   ^ {name} {'new' if not prev else 'changed'}", file=sys.stderr)
manifest = {"version": os.environ["VER"], "published": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"), "files": files}
with open(os.environ["MANIFEST"], "w") as out:
    json.dump(manifest, out, indent=2); out.write("\n")
print("\n".join(upload))
PY
)
REMOTE_VER=$(printf '%s' "$REMOTE" | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("version",""))
except Exception: print("")')
if [ -z "$UPLOAD" ] && [ "$REMOTE_VER" = "$ver" ]; then
  echo "✓ nothing changed — the server already serves version $ver with these files"
  exit 0
fi
files=()
while IFS= read -r line; do [ -n "$line" ] && files+=("$line"); done <<< "$UPLOAD"
files+=("$MANIFEST")

echo "→ upload → $TARGET:~/scrai-stage/dl/  (${#files[@]} file(s) incl. manifest)"
ssh "${SSH_OPTS[@]}" "$TARGET" 'rm -rf ~/scrai-stage/dl && mkdir -p ~/scrai-stage/dl'
rsync -a --info=progress2 -e "ssh ${SSH_OPTS[*]}" "${files[@]}" "$TARGET:~/scrai-stage/dl/"

echo "→ install into /opt/scrai/site/dl (sudo once)"
ssh -t "${SSH_OPTS[@]}" "$TARGET" '
  set -e
  sudo install -d -o scrai -g scrai -m 755 /opt/scrai/site/dl
  sudo install -o scrai -g scrai -m 644 ~/scrai-stage/dl/* /opt/scrai/site/dl/
  ls -la /opt/scrai/site/dl/
'

echo
echo "✓ published version $ver — live at $SITE (the site reads manifest.json on every view)"
cat "$MANIFEST"
