#!/usr/bin/env bash
# Static security scan — Semgrep (public security packs + our own invariants + taint rules)
# and cargo-audit (dependency advisories). Runs fully locally: metrics off, no login, no
# upload; only the public rule packs are fetched from the Semgrep registry.
#
#   scripts/semgrep.sh            # scan, print summary, exit 1 on ERROR-severity findings
#   scripts/semgrep.sh --json DIR # additionally write results.json / taint.json / audit.json to DIR
#
# Inline <script> blocks of public/index.html are extracted to a temp dir first — Semgrep
# does not scan JavaScript embedded in HTML.
set -euo pipefail
cd "$(dirname "$0")/.."
export SEMGREP_SEND_METRICS=off

OUT=""
if [ "${1:-}" = "--json" ]; then OUT="${2:?dir}"; mkdir -p "$OUT"; fi
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

# 1) inline webview scripts → standalone .js files (line = html line - offset, printed in the header)
node -e '
const fs=require("fs");const s=fs.readFileSync("public/index.html","utf8");
const re=/<script(?![^>]*src=)[^>]*>([\s\S]*?)<\/script>/g;let m,i=0;
while((m=re.exec(s))){i++;const line=s.slice(0,m.index).split("\n").length;
  fs.writeFileSync(process.argv[1]+"/index-script"+i+".js","// extracted from public/index.html, <script> #"+i+" starting at html line "+line+"\n"+m[1]);}
' "$TMP"

PACKS=(--config p/security-audit --config p/secrets --config p/rust --config p/javascript --config p/typescript
       --config p/owasp-top-ten --config p/command-injection --config p/github-actions)
EXCL=(--exclude target --exclude node_modules --exclude docs/pitch --exclude public/vendor
      --exclude src-tauri/gen --exclude src-tauri/target --exclude src-tauri/models --exclude dist)
# accepted by design (see .semgrep/README): native FFI needs unsafe; temp_dir use is in tests + the share-sheet export
SKIP=(--exclude-rule rust.lang.security.unsafe-usage.unsafe-usage
      --exclude-rule rust.lang.security.temp-dir.temp-dir)

# SEMGREP_PRO=1 → cross-file (interprocedural) taint via the Pro engine. Needs a one-time
# `semgrep login` + `semgrep install-semgrep-pro` on this machine; the code still stays local
# (only finding metadata leaves), but we run it only over the webview to keep core/server out.
PRO=()
if [ "${SEMGREP_PRO:-0}" = "1" ]; then PRO=(--pro); fi

echo "── Semgrep: public packs + project rules${PRO:+ (Pro engine, cross-file)}"
# (bash 3.2 treats an empty array as unbound under `set -u` — hence the ${PRO[@]+…} form)
semgrep scan --metrics=off --no-git-ignore ${PRO[@]+"${PRO[@]}"} "${PACKS[@]}" --config .semgrep/scrambleai.yml --config .semgrep/scrambleai-taint.yml \
  "${EXCL[@]}" "${SKIP[@]}" --severity ERROR --severity WARNING --error \
  ${OUT:+--json -o "$OUT/results.json"} \
  core server src-tauri/src src public scripts .github "$TMP" || STATUS=$?

echo "── backend.js facade"
node "$(dirname "$0")/check-backend-facade.mjs" || STATUS=1

echo "── cargo audit"
if command -v cargo-audit >/dev/null; then
  if [ -n "$OUT" ]; then cargo audit --json > "$OUT/audit.json" || true; fi
  cargo audit || true
else
  echo "cargo-audit not installed (cargo install cargo-audit --locked) — skipped"
fi
exit "${STATUS:-0}"
