#!/usr/bin/env bash
# Inline the site's screenshots into the click-dummy as data URIs (the artifact sandbox
# loads no external images) and write the result to $1 (default: ./clickdummy.built.html).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="${1:-$HERE/clickdummy.built.html}"
python3 - "$HERE/clickdummy.html" "$HERE/../../server/site/img" "$OUT" <<'PY'
import sys, re, base64
src, imgdir, out = sys.argv[1:4]
s = open(src).read()
def inline(m):
    name = m.group(1)
    data = base64.b64encode(open(f"{imgdir}/{name}.jpg", "rb").read()).decode()
    return "data:image/jpeg;base64," + data
s = re.sub(r"\{\{IMG:([a-z0-9-]+)\}\}", inline, s)
open(out, "w").write(s)
print(out, len(s)//1024, "KB")
PY
