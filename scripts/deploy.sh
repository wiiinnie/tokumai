#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# deploy.sh — push the RUST server crate to the VPS, build there, install all three
# binaries (tokumai-server, tokumai-admin, tokumai-faucet) + pricing.json, restart
# tokumai and — when TESTNET=1 — tokumai-faucet (disabled otherwise).
#
# What ships: core/ + server/ + pricing.json + Cargo.lock (a minimal cargo
# workspace is generated on the VPS — src-tauri stays home). The build runs as
# the ADMIN user in ~/scrai-stage so the cargo cache survives between deploys;
# only the finished binary is installed to /opt/tokumai/bin/tokumai-server, which
# the rewritten tokumai.service runs as user `scrai` with /opt/tokumai as CWD
# (.env and data/ live there and are never touched by a deploy).
#
# First deploy on a box that still runs the TS version:
#   - installs rustup + build tools if missing (build needs one sudo apt call)
#   - REPLACES the systemd unit (node → binary) and removes the Node leftovers
#     (node_modules, dist, src, …) from /opt/tokumai
#   - the server keeps .env and data/, but the RUST server stores its Nym
#     identity under data/.nym-server → it comes up with a NEW Nym address.
#     Read it from the logs and point the app at it (Account & recovery →
#     server). Old TS state (money DB, .nym) is left in place, just unused.
#
# NOTE the first build compiles the whole nym-sdk: expect 10–30 min on a small
# VPS. If the linker gets OOM-killed, add swap or retry with:
#   ssh <target> 'cd ~/scrai-stage && ~/.cargo/bin/cargo build --release -p scrai-server -j 1'
#
# Usage:  scripts/deploy.sh <admin_user>@<vps-host> [--install-apply]
#         (admin_user = a SUDO user on the VPS; the service runs as `scrai`)
#
# ── FEWER PASSWORD PROMPTS ────────────────────────────────────────────────
# Out of the box this asks at most twice: once for SSH, once for sudo. For ZERO:
#
#   1. SSH key  → removes the SSH password prompt entirely:
#        ssh-copy-id <admin_user>@<vps-host>
#
# ── WHICH SITE GOES WHERE ─────────────────────────────────────────────────
# ONE instance, three names. The site is baked into the faucet binary
# (include_str!), so "update the website" means "replace that binary and restart
# its unit" — and there is exactly one of it:
#
#   tokumai-faucet.service  127.0.0.1:8790   /opt/tokumai/bin/tokumai-faucet
#     tokumai.com           the site, /pay, /paid, /imprint, /terms, /privacy
#     faucet.tokumai.com    Caddy rewrites / to /claim (invite code, 1 USD in TOKU)
#     payment.tokumai.com   Caddy rewrites / to /pay
#
# Caddy serves /dl/* straight off the disk; everything else is proxied to :8790.
# The vhosts live in deploy/Caddyfile in this repo — copy it to /etc/caddy/Caddyfile.
#
# Until 2026-09-05 there was a SECOND instance on :8791 behind basic_auth, because
# the tokumai brand was unreleased and had to stay off the public host. It is gone,
# and so is the old scrai-faucet.hermes-stakepool.de vhost. A deploy disables the
# leftover unit if it is still installed.
#
#   2. Passwordless sudo for JUST this deploy. On the VPS:
#      `sudo visudo -f /etc/sudoers.d/scrai-deploy` and add exactly:
#        <admin_user> ALL=(root) NOPASSWD: /opt/tokumai/bin/deploy-apply.sh
#      then (re)install the root-owned apply script once:
#        scripts/deploy.sh <admin_user>@<vps-host> --install-apply
#      (Re-run --install-apply after ANY change to this file's APPLY_BODY —
#       the switch from the TS deploy is exactly such a change.)
# ---------------------------------------------------------------------------
set -euo pipefail

TARGET=""
MODE=""
while [ $# -gt 0 ]; do
  case "$1" in
    # There is one site now (see WHICH SITE GOES WHERE). Fail loudly rather than
    # accept a flag whose whole point — choosing between two instances — is gone.
    --faucet|--faucet=*)
      echo "deploy.sh: --faucet is gone. There is one site instance (tokumai-faucet.service)," >&2
      echo "           reachable as tokumai.com, faucet.tokumai.com and payment.tokumai.com." >&2
      exit 2 ;;
    --*)        MODE="$1"; shift ;;
    *)          TARGET="$1"; shift ;;
  esac
done
TARGET="${TARGET:-${DEPLOY_TARGET:-${SCRAI_DEPLOY_TARGET:-}}}"
if [ -z "$TARGET" ]; then
  echo "usage: scripts/deploy.sh <admin_user>@<vps-host> [--install-apply]" >&2
  echo "       (or set DEPLOY_TARGET)" >&2
  exit 2
fi
if [ -z "$TARGET" ]; then
  echo "usage: scripts/deploy.sh <admin_user>@<vps-host> [--install-apply]" >&2
  exit 1
fi
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# One shared, authenticated SSH connection for every step (rsync + ssh), so SSH
# asks for a password at most once instead of once per command.
CM_SOCK="${TMPDIR:-/tmp}/scrai-cm-$$"
SSH_OPTS=(-o ControlMaster=auto -o "ControlPath=${CM_SOCK}" -o ControlPersist=180)
cleanup() { ssh "${SSH_OPTS[@]}" -O exit "$TARGET" >/dev/null 2>&1 || true; rm -f "$CM_SOCK"; }
trap cleanup EXIT

echo "→ opening SSH connection to $TARGET (auth once) …"
ssh "${SSH_OPTS[@]}" "$TARGET" true

# The privileged sequence, kept in ONE place so it can be run inline OR installed
# once as a root-owned script that a single NOPASSWD sudoers line covers.
# It installs the freshly built binary + pricing table, writes the systemd unit,
# and clears the retired Node deployment out of /opt/tokumai (state is kept).
APPLY_BODY='set -e
# Never deploy into a box that has not been migrated yet: install -d would happily create a
# fresh /opt/tokumai next to the real /opt/scrai, and the server would start with an empty
# data dir — new Nym address, new authority, no balances. Migrate first.
if [ ! -d /opt/tokumai ] && [ -d /opt/scrai ]; then
  echo "FATAL: /opt/scrai exists but /opt/tokumai does not — run scripts/migrate-to-tokumai.sh first" >&2
  exit 1
fi
# $1 = admin home (for the build stage), $2 = which faucet instance(s) to update.
# Positional on purpose: the NOPASSWD sudoers line covers this exact command path, and
# `sudo env FOO=... script` would be a different command and prompt for a password again.
install -d -o scrai -g scrai /opt/tokumai /opt/tokumai/bin /opt/tokumai/data
install -o scrai -g scrai -m 755 \
  "$ADMIN_HOME/scrai-stage/target/release/scrai-server" /opt/tokumai/bin/tokumai-server.new
mv /opt/tokumai/bin/tokumai-server.new /opt/tokumai/bin/tokumai-server
# The operator console (a page on loopback; the ratatui console it replaced is gone).
# Deliberately NOT a service: it is started inside the SSH session that reaches it and dies
# with it, so there is no admin surface listening on the box while nobody is looking at it.
# Bound to loopback in the binary, not by configuration.
#   ssh -t -L 8791:127.0.0.1:8791 <admin>@<host> sudo -u scrai /opt/tokumai/bin/tokumai-admin
# (as scrai: .env and the databases belong to that user, and the three actions write them)
install -o scrai -g scrai -m 755 \
  "$ADMIN_HOME/scrai-stage/target/release/scrai-adminweb" /opt/tokumai/bin/tokumai-admin.new
mv /opt/tokumai/bin/tokumai-admin.new /opt/tokumai/bin/tokumai-admin
rm -f /opt/tokumai/bin/tokumai-adminweb
# Website + faucet (same crate, one binary). The pages are baked in with include_str!,
# so updating the website MEANS replacing this binary and restarting its unit.
install -o scrai -g scrai -m 755 \
  "$ADMIN_HOME/scrai-stage/target/release/scrai-faucet" /opt/tokumai/bin/tokumai-faucet.new
mv /opt/tokumai/bin/tokumai-faucet.new /opt/tokumai/bin/tokumai-faucet
install -o scrai -g scrai -m 644 \
  "$ADMIN_HOME/scrai-stage/pricing.json" /opt/tokumai/pricing.json
# retire the Node deployment (keep .env, data/, images/, and our bin/)
rm -rf /opt/tokumai/node_modules /opt/tokumai/dist /opt/tokumai/src /opt/tokumai/scripts \
  /opt/tokumai/public /opt/tokumai/package.json /opt/tokumai/package-lock.json \
  /opt/tokumai/tsconfig.json /opt/tokumai/.nym
cat > /etc/systemd/system/tokumai.service <<UNIT
[Unit]
Description=tokumai-server (tokumai mixnet service provider, Rust)
After=network-online.target
Wants=network-online.target

[Service]
User=scrai
Group=scrai
WorkingDirectory=/opt/tokumai
ExecStart=/opt/tokumai/bin/tokumai-server
Restart=always
RestartSec=5
# dotenvy reads /opt/tokumai/.env (CWD); data lands in /opt/tokumai/data
Environment=DATA=/opt/tokumai/data

[Install]
WantedBy=multi-user.target
UNIT
cat > /etc/systemd/system/tokumai-faucet.service <<UNIT
[Unit]
Description=tokumai-faucet (tokumai testnet faucet + download site, loopback only — Caddy in front)
After=network-online.target tokumai.service
Wants=network-online.target

[Service]
User=scrai
Group=scrai
WorkingDirectory=/opt/tokumai
ExecStart=/opt/tokumai/bin/tokumai-faucet
Restart=always
RestartSec=5
Environment=DATA=/opt/tokumai/data

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl restart tokumai
systemctl --no-pager status tokumai | head -6
# This unit serves tokumai.com, so it runs unconditionally. It used to be gated on
# TESTNET=1 back when it was only a faucet: with the website on it, that switch would
# have taken the site down on the day the testnet phase ends. TESTNET now only decides
# whether the faucet PAGE inside it accepts claims (the binary logs which mode it is in).
systemctl enable --now tokumai-faucet >/dev/null 2>&1 || true
systemctl restart tokumai-faucet
systemctl --no-pager status tokumai-faucet | head -4
# The password-protected preview instance is gone (one site, three vhosts). Remove it
# here so a box that still carries the unit stops running a second copy.
if [ -f /etc/systemd/system/tokumai-faucet-preview.service ]; then
  systemctl disable --now tokumai-faucet-preview >/dev/null 2>&1 || true
  rm -f /etc/systemd/system/tokumai-faucet-preview.service /opt/tokumai/bin/tokumai-faucet-preview
  systemctl daemon-reload
  echo "removed the retired tokumai-faucet-preview instance"
fi'

# APPLY_BODY travels inside a SINGLE-QUOTED remote command. One literal apostrophe in it
# ends that quoting and the remote bash dies with "unexpected EOF while looking for
# matching quote" — while PARSING, so the `if` below never even picks a branch and nothing
# runs at all (2026-09-04: the word "instance's" in a comment cost a deploy round).
# Catch it here rather than on the VPS.
# A fingerprint of the body. The installed copy on the VPS carries the same line, and a
# normal deploy compares them before running it.
#
# Why: the apply script is installed ONCE and only refreshed with --install-apply, so any
# change here — a new binary to install, a unit to restart — is silently ignored by every
# deploy until somebody remembers a note in this header. It cost two deploys and a
# "No such file or directory" to find that out (2026-09-09). A note is not a mechanism.
APPLY_MARK="$(printf '%s' "$APPLY_BODY" | cksum | awk '{print $1}')"

SQ="'"
case "$APPLY_BODY" in
  *"$SQ"*)
    echo "deploy.sh: APPLY_BODY contains a single quote — rewrite that line without one." >&2
    exit 1 ;;
esac

# One-time: install the apply script as root so the NOPASSWD sudoers line applies.
# The body is streamed verbatim (no local OR remote expansion) into a staging
# file first, then installed with one sudo call (which may prompt — hence -t).
if [ "$MODE" = "--install-apply" ]; then
  echo "→ installing /opt/tokumai/bin/deploy-apply.sh (root-owned) — sudo once …"
  {
    printf '#!/usr/bin/env bash\nADMIN_HOME="${1:?admin home required}"\n'
    printf 'APPLY_MARK=%s\n' "$APPLY_MARK"
    printf '%s\n' "$APPLY_BODY"
  } | ssh "${SSH_OPTS[@]}" "$TARGET" 'mkdir -p ~/scrai-stage && cat > ~/scrai-stage/deploy-apply.new'
  ssh -t "${SSH_OPTS[@]}" "$TARGET" \
    'sudo install -D -o root -g root -m 755 ~/scrai-stage/deploy-apply.new /opt/tokumai/bin/deploy-apply.sh && echo installed'
  echo "✓ apply script installed. Add the NOPASSWD sudoers line (see header) for zero prompts."
  exit 0
fi

echo "→ 1/4  sync sources → $TARGET:~/scrai-stage/  (core/, server/, pricing.json)"
# --delete prunes removed source files but leaves everything excluded ('*')
# alone on the receiver — i.e. the VPS-generated Cargo.toml and the target/
# build cache survive between deploys.
# server/fuzz/ is the cargo-fuzz workspace: its ASan target/ dir is gigabytes and the
# VPS never needs any of it — excluded BEFORE the /server/*** include (first match wins).
# Live progress so a big sync is visibly moving (progress2 needs rsync ≥ 3.1; openrsync
# on macOS falls back to per-file --progress).
if rsync --info=progress2 --version >/dev/null 2>&1; then PROG=(--info=progress2); else PROG=(--progress); fi
rsync -az --delete -e "ssh ${SSH_OPTS[*]}" ${PROG[@]+"${PROG[@]}"} \
  --exclude='/server/fuzz/' \
  --include='/core/***' --include='/server/***' \
  --include='/pricing.json' --include='/Cargo.lock' \
  --exclude='*' \
  "$SRC/" "$TARGET:~/scrai-stage/"
# A sync that ran before the exclude existed may have left the fuzz tree (GBs) on the
# VPS — excluded paths survive --delete, so remove it explicitly.
ssh "${SSH_OPTS[@]}" "$TARGET" 'rm -rf ~/scrai-stage/server/fuzz'

echo "→ 2/4  toolchain check (rustup + build tools)"
ssh -t "${SSH_OPTS[@]}" "$TARGET" '
  set -e
  if ! command -v cc >/dev/null 2>&1 || ! command -v pkg-config >/dev/null 2>&1; then
    echo "   · installing build-essential + pkg-config (sudo apt) …"
    sudo apt-get update -qq && sudo apt-get install -y -qq build-essential pkg-config curl
  fi
  if [ ! -x "$HOME/.cargo/bin/cargo" ]; then
    echo "   · installing rustup (user-local, no sudo) …"
    curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  fi
'

echo "→ 3/4  build scrai-server on the VPS (first build compiles nym-sdk — be patient)"
# The stage gets its own minimal workspace: src-tauri never leaves the Mac, so
# the repo Cargo.toml (which lists it as a member) cannot be used remotely. The
# crates inherit `version.workspace`, so the generated file carries the repo's
# workspace version too — that is what the server reports as its own version.
WS_VER="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$SRC/Cargo.toml" | head -1)"
if [ -z "$WS_VER" ]; then echo "deploy.sh: no [workspace.package] version in Cargo.toml" >&2; exit 1; fi
ssh "${SSH_OPTS[@]}" "$TARGET" "WS_VER='$WS_VER'"'
  set -e
  cd "$HOME/scrai-stage"
  printf "%s\n" \
    "# generated by deploy.sh — server-side workspace (no src-tauri here)" \
    "[workspace]" \
    "resolver = \"2\"" \
    "members = [\"core\", \"server\"]" \
    "[workspace.package]" \
    "version = \"$WS_VER\"" > Cargo.toml
  "$HOME/.cargo/bin/cargo" build --release -p scrai-server --bins
'

echo
echo "   about to update, from $(basename "$SRC"):"
echo "     tokumai.service        →  tokumai-server + tokumai-admin + pricing.json"
echo "     tokumai-faucet.service →  tokumai.com · faucet.tokumai.com · payment.tokumai.com"
echo
# Is the copy on the VPS the one this file describes? A mismatch means the deploy would
# install yesterday's set of binaries and say nothing about it.
REMOTE_MARK="$(ssh "${SSH_OPTS[@]}" "$TARGET" \
  'grep -m1 "^APPLY_MARK=" /opt/tokumai/bin/deploy-apply.sh 2>/dev/null | cut -d= -f2' || true)"
if [ -n "$REMOTE_MARK" ] && [ "$REMOTE_MARK" != "$APPLY_MARK" ]; then
  echo >&2
  echo "deploy.sh: the apply script on $TARGET is OLDER than this file." >&2
  echo "           It would install the previous set of binaries and units, quietly." >&2
  echo "           Refresh it once, then deploy again:" >&2
  echo >&2
  echo "             scripts/deploy.sh $TARGET --install-apply" >&2
  echo >&2
  exit 1
fi

echo "→ 4/4  install binaries + pricing.json, restart the units named above"
# If the root-owned apply script exists, run it (one sudo call — NOPASSWD-able);
# otherwise fall back to the inline block (still one shared SSH connection).
ssh -t "${SSH_OPTS[@]}" "$TARGET" '
  set -e
  if [ -x /opt/tokumai/bin/deploy-apply.sh ]; then
    sudo /opt/tokumai/bin/deploy-apply.sh "$HOME"
  else
    echo "   · (tip: run with --install-apply once + a NOPASSWD line for zero prompts)"
    sudo env ADMIN_HOME="$HOME" bash -c '"'"''"$APPLY_BODY"''"'"' _
  fi
'
# What is actually running now — read back, not assumed (no sudo needed for is-active).
echo "→ services after apply:"
ssh "${SSH_OPTS[@]}" "$TARGET" '
  for u in tokumai tokumai-faucet; do
    printf "   %-13s %s" "$u" "$(systemctl is-active "$u" 2>/dev/null || true)"
    printf "  (enabled: %s)\n" "$(systemctl is-enabled "$u" 2>/dev/null || true)"
  done
'
echo "✓ deployed. Logs:  ssh $TARGET 'journalctl -u tokumai -f'   ·   ssh $TARGET 'journalctl -u tokumai-faucet -f'"
echo "  First start bootstraps a fresh authority and a NEW Nym address — grab it"
echo "  from the logs (scrai-server: … address: …) and set it in the app."