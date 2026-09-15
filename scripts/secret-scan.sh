#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# secret-scan.sh — look for credentials in the WHOLE HISTORY, not just the tree.
#
# Why this exists: on 2026-09-15 the repository was made public for a CI build, and
# GitGuardian found a Groq API key minutes later. It had been sitting in `.env.example`
# since the very first commit — long gone from the working tree, never gone from git. The
# check that was run beforehand looked at the tree only, and at three vendors' formats.
#
# A published repository publishes its history. Run this before making it public, and
# treat a hit as "rotate that key", not as "delete the commit": a key that was pushed is
# a key that was seen, and rewriting history does not unsee it.
#
# Usage:  scripts/secret-scan.sh            # whole history
#         scripts/secret-scan.sh --tree     # working tree only (fast, for a pre-commit)
# ---------------------------------------------------------------------------
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

# Vendor formats, each anchored tightly enough not to fire on random base64 — the loose
# version of this scan reported the AWS pattern inside a Tesseract WASM blob and an image.
PATTERNS=(
  'gsk_[A-Za-z0-9]{40,}'                    # Groq
  'sk-[A-Za-z0-9]{40,}'                     # OpenAI
  'sk-ant-[A-Za-z0-9_-]{40,}'               # Anthropic
  'xai-[A-Za-z0-9]{40,}'                    # xAI
  'AIza[0-9A-Za-z_-]{35}[^0-9A-Za-z_-]'     # Google (bounded: base64 blobs contain "AIza")
  'ghp_[A-Za-z0-9]{36}'                     # GitHub PAT
  'github_pat_[A-Za-z0-9_]{60,}'
  'glpat-[A-Za-z0-9_-]{20,}'                # GitLab
  'AKIA[0-9A-Z]{16}[^0-9A-Za-z]'            # AWS access key id
  'xox[baprs]-[0-9]{10,}-[0-9A-Za-z-]{20,}' # Slack
  '(sk|rk)_live_[A-Za-z0-9]{24,}'           # Stripe
  'live_[A-Za-z0-9]{28,}'                   # Mollie
  'BEGIN (RSA|OPENSSH|EC|DSA|PGP) PRIVATE KEY'
  'nym[_-]?(api[_-]?)?key["'"'"' :=]{1,4}[A-Za-z0-9]{32,}'
)

mode="${1:-}"
hits=0
for p in "${PATTERNS[@]}"; do
  if [ "$mode" = "--tree" ]; then
    out=$(git grep -nIE "$p" -- . 2>/dev/null | head -5)
  else
    # -G searches diffs as a REGEX (-S is a literal string and matches far too much).
    out=$(git log --all --oneline -G"$p" 2>/dev/null | head -5)
  fi
  if [ -n "$out" ]; then
    hits=$((hits + 1))
    echo "── $p"
    # Never print the secret itself: this output ends up in terminals and transcripts.
    echo "$out" | sed -E 's/(gsk_|sk-|xai-|AIza|ghp_|glpat-|AKIA|xox.-|live_)[A-Za-z0-9_-]+/\1<REDACTED>/g'
  fi
done

if [ "$hits" -gt 0 ]; then
  echo
  echo "secret-scan: $hits pattern(s) matched — inspect each, and ROTATE anything real."
  echo "             (a match inside a vendored .wasm/.b64 blob is usually a coincidence;"
  echo "              look at the file before deciding)"
  exit 1
fi
echo "secret-scan: nothing found in ${mode:-the full history}"
