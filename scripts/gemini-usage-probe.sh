#!/usr/bin/env bash
# One-shot probe: ask Nano Banana 2 (+ Lite) for a tiny image and print ONLY the
# usageMetadata, so the billing split (candidatesTokensDetails IMAGE vs TEXT +
# thoughtsTokenCount) can be verified against server/src/chat.rs::gemini_usage.
# Costs ~$0.10 total. Never prints the key.
#
#   Usage: GEMINI_KEY=… scripts/gemini-usage-probe.sh
#      or: scripts/gemini-usage-probe.sh /opt/scrai/.env   (reads the active key slot)
set -euo pipefail
if [ -n "${1:-}" ]; then
  GEMINI_KEY=$(grep -E '^GEMINI_API_KEY_(MAINNET|TESTNET)=' "$1" | head -1 | cut -d= -f2- | tr -d ' "'"'")
fi
: "${GEMINI_KEY:?set GEMINI_KEY or pass a .env path}"
for m in gemini-3.1-flash-image gemini-3.1-flash-lite-image; do
  echo "== $m"
  curl -s -4 -X POST "https://generativelanguage.googleapis.com/v1beta/models/$m:generateContent" \
    -H "x-goog-api-key: $GEMINI_KEY" -H "Content-Type: application/json" \
    -d '{"contents":[{"role":"user","parts":[{"text":"A small red circle on white background."}]}],
         "generationConfig":{"responseModalities":["TEXT","IMAGE"],"maxOutputTokens":6144,
                             "thinkingConfig":{"thinkingBudget":2048}}}' \
  | python3 -c '
import json,sys
j=json.load(sys.stdin)
if "error" in j: print("ERROR", j["error"].get("code"), j["error"].get("message")); sys.exit()
parts=j.get("candidates",[{}])[0].get("content",{}).get("parts",[])
print("parts:", [("image:"+p["inlineData"]["mimeType"]) if "inlineData" in p else ("text:"+repr(p.get("text","")[:50])) for p in parts])
print("usageMetadata:", json.dumps(j.get("usageMetadata"), indent=1))
print("modelVersion:", j.get("modelVersion"))'
done
