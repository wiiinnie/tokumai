#!/usr/bin/env bash
# Preview server/site the way scrai-faucet serves it: at the ROOT of a host, so the
# absolute /img/... paths in index.html resolve. Opening index.html straight from disk
# shows no images at all — that is the file:// path, not a broken page.
#
#   scripts/site-preview.sh [port]      # default 8777, Ctrl-C to stop
set -euo pipefail
PORT="${1:-8777}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
OUT="$(mktemp -d)/site"
mkdir -p "$OUT"
cp -R "$ROOT/server/site/." "$OUT/"
# Drop folders for third-party brand assets are not part of the site.
rm -rf "$OUT/temp_delete"
# Fill the placeholders scrai-faucet substitutes at runtime, so the page looks real.
python3 "$HERE/site-preview-fill.py" "$OUT/index.html"
# scrai-faucet routes /imprint, /terms, /privacy and /pay to these files; a plain static
# server needs the directory form to answer the same URLs.
for page in pay imprint terms privacy paid; do
  [ -f "$OUT/$page.html" ] && mkdir -p "$OUT/$page" && cp "$OUT/$page.html" "$OUT/$page/index.html"
done
true
echo "tokumai site preview → http://127.0.0.1:$PORT/   (serving $OUT)"
cd "$OUT" && exec python3 -m http.server "$PORT" --bind 127.0.0.1
