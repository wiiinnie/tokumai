#!/usr/bin/env bash
# The dependency audit, as a gate: fails on any advisory not named in .cargo/audit.toml
# (each one there says why it is accepted). Run before an image is built; it is what
# keeps the set of known holes from growing unnoticed (audit M19).
#
#     deploy/audit.sh            # cargo audit against the lockfile
#     deploy/audit.sh --all      # and the unmaintained/yanked warnings, for reading
set -euo pipefail
cd "$(dirname "$0")/.."
command -v cargo-audit >/dev/null || { echo "cargo-audit is not installed: cargo install cargo-audit"; exit 2; }
if [ "${1:-}" = --all ]; then
  cargo audit
else
  # Warnings (unmaintained, unsound, yanked) are listed, not failed on: reading matter.
  cargo audit
fi
