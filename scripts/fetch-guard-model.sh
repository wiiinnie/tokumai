#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# fetch-guard-model.sh — download the on-device semantic privacy-guard model.
#
# Places the GLiNER multilingual model (ONNX) + tokenizer under
# src-tauri/models/gliner/, which the `smart-guard` desktop build bundles as a
# Tauri resource (see src-tauri/tauri.smart.conf.json). This is NOT authored code
# — it's a large ML weight file kept out of the source tree.
#
# We ship the fp16 variant (~552 MB): verified to match fp32 quality across
# de/en/fr/es/ja/ru (weak on Arabic), at half the size. The int8 variant is
# BROKEN in this runtime (returns nothing) — do not use it.
#
# Usage:  bash scripts/fetch-guard-model.sh
# ---------------------------------------------------------------------------
set -euo pipefail

REPO="onnx-community/gliner_multi-v2.1"
DEST="$(cd "$(dirname "$0")/.." && pwd)/src-tauri/models/gliner"
BASE="https://huggingface.co/${REPO}/resolve/main"
UA="scrambleai-guard-fetch"

mkdir -p "$DEST"

echo "→ model.onnx (fp16, ~552 MB) …"
curl -fL --retry 3 -H "User-Agent: $UA" "${BASE}/onnx/model_fp16.onnx" -o "${DEST}/model.onnx"

echo "→ tokenizer.json …"
curl -fL --retry 3 -H "User-Agent: $UA" "${BASE}/tokenizer.json" -o "${DEST}/tokenizer.json"

# Sanity: tokenizer must be valid JSON, model must be non-trivially sized.
node -e 'JSON.parse(require("fs").readFileSync(process.argv[1],"utf8"))' "${DEST}/tokenizer.json" \
  && echo "  tokenizer.json OK"
sz=$(stat -f%z "${DEST}/model.onnx" 2>/dev/null || stat -c%s "${DEST}/model.onnx")
[ "$sz" -gt 100000000 ] && echo "  model.onnx OK (${sz} bytes)" || { echo "  model.onnx looks too small"; exit 1; }

echo "✓ Guard model ready at ${DEST}"
echo "  Build the smart desktop app with:"
echo "    npm run tauri dev  -- --features smart-guard -c src-tauri/tauri.smart.conf.json"
