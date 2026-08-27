#!/usr/bin/env bash
# Fuzz the server's request parsers — the four entry points the mixnet loop feeds with
# bytes from anonymous senders. Each target must never panic (a panic kills the dispatch
# loop for every client) and never blow its byte caps.
#
#   scripts/fuzz.sh              # every target, 5 min each
#   scripts/fuzz.sh 60           # every target, 60 s each
#   scripts/fuzz.sh 300 chat_reserve
#
# Needs: rustup nightly + cargo-fuzz (`rustup toolchain install nightly --profile minimal`,
# `cargo install cargo-fuzz --locked`). Corpus and crash artefacts land in server/fuzz/
# (git-ignored); a crash file can be replayed with
#   (cd server && cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<file>)
set -euo pipefail
cd "$(dirname "$0")/../server"
SECS="${1:-300}"
TARGETS="${2:-fed_dispatch chat_reserve upload_handle replies_handle}"
STATUS=0
for t in $TARGETS; do
  echo "── fuzz $t (${SECS}s)"
  if ! cargo +nightly fuzz run "$t" -- -max_total_time="$SECS" -max_len=65536 -rss_limit_mb=4096 2>&1 | tail -3; then
    echo "!! $t found a crash — see server/fuzz/artifacts/$t/"; STATUS=1
  fi
done
exit $STATUS
