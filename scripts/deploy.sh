#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# deploy.sh — push the RUST server crate to the VPS, build there, install all three
# binaries (scrai-server, scrai-admin, scrai-faucet) + pricing.json, restart scrai and
# — when SCRAI_TESTNET=1 — scrai-faucet (disabled otherwise).
#
# What ships: core/ + server/ + pricing.json + Cargo.lock (a minimal cargo
# workspace is generated on the VPS — src-tauri stays home). The build runs as
# the ADMIN user in ~/scrai-stage so the cargo cache survives between deploys;
# only the finished binary is installed to /opt/scrai/bin/scrai-server, which
# the rewritten scrai.service runs as user `scrai` with /opt/scrai as CWD
# (.env and data/ live there and are never touched by a deploy).
#
# First deploy on a box that still runs the TS version:
#   - installs rustup + build tools if missing (build needs one sudo apt call)
#   - REPLACES the systemd unit (node → binary) and removes the Node leftovers
#     (node_modules, dist, src, …) from /opt/scrai
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
# The site is baked into the faucet binary (include_str!), and the box runs TWO
# faucet instances behind Caddy:
#   --faucet public   (default)  scrai-faucet.service          :8790
#                                scrai-faucet.hermes-stakepool.de — NO auth
#   --faucet preview             scrai-faucet-tokumai.service  :8791
#                                tokumai.com — behind basic_auth
#   --faucet both | none
# Deploying the tokumai tree without --faucet preview once put the unreleased brand
# on the public host (2026-09-04). Step 4 now names the units before touching them.
#
# NOTE: the faucet target reaches the root-owned apply script as its SECOND argument,
# so after this change re-run --install-apply once, or the installed script ignores it.
#
#   2. Passwordless sudo for JUST this deploy. On the VPS:
#      `sudo visudo -f /etc/sudoers.d/scrai-deploy` and add exactly:
#        <admin_user> ALL=(root) NOPASSWD: /opt/scrai/bin/deploy-apply.sh
#      then (re)install the root-owned apply script once:
#        scripts/deploy.sh <admin_user>@<vps-host> --install-apply
#      (Re-run --install-apply after ANY change to this file's APPLY_BODY —
#       the switch from the TS deploy is exactly such a change.)
# ---------------------------------------------------------------------------
set -euo pipefail

TARGET=""
MODE=""
# Which faucet instance gets the freshly built site. There are TWO on the box:
#   public  → /opt/scrai/bin/scrai-faucet          + scrai-faucet.service          (:8790)
#             scrai-faucet.hermes-stakepool.de — NO auth, anyone can read it
#   preview → /opt/scrai/bin/scrai-faucet-tokumai  + scrai-faucet-tokumai.service  (:8791)
#             tokumai.com — behind basic_auth, where an unreleased brand belongs
# Default is `public`, i.e. what this script always did. Deploying the tokumai tree
# without --faucet preview put the rebrand on the public host once (2026-09-04) —
# hence the banner below, which names the units before anything is touched.
FAUCET_TARGET="${SCRAI_DEPLOY_FAUCET:-public}"
while [ $# -gt 0 ]; do
  case "$1" in
    --faucet)   FAUCET_TARGET="${2:-}"; shift 2 ;;
    --faucet=*) FAUCET_TARGET="${1#*=}"; shift ;;
    --*)        MODE="$1"; shift ;;
    *)          TARGET="$1"; shift ;;
  esac
done
TARGET="${TARGET:-${SCRAI_DEPLOY_TARGET:-}}"
case "$FAUCET_TARGET" in
  public|preview|both|none) ;;
  *) echo "deploy.sh: --faucet must be public, preview, both or none (got '$FAUCET_TARGET')" >&2; exit 2 ;;
esac
if [ -z "$TARGET" ]; then
  echo "usage: scripts/deploy.sh <admin_user>@<vps-host> [--install-apply] [--faucet public|preview|both|none]" >&2
  echo "       (or set SCRAI_DEPLOY_TARGET / SCRAI_DEPLOY_FAUCET)" >&2
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
# and clears the retired Node deployment out of /opt/scrai (state is kept).
APPLY_BODY='set -e
# $1 = admin home (for the build stage), $2 = which faucet instance(s) to update.
# Positional on purpose: the NOPASSWD sudoers line covers this exact command path, and
# `sudo env FOO=... script` would be a different command and prompt for a password again.
FAUCET_TARGET="${2:-public}"
install -d -o scrai -g scrai /opt/scrai /opt/scrai/bin /opt/scrai/data
install -o scrai -g scrai -m 755 \
  "$SCRAI_ADMIN_HOME/scrai-stage/target/release/scrai-server" /opt/scrai/bin/scrai-server.new
mv /opt/scrai/bin/scrai-server.new /opt/scrai/bin/scrai-server
# read-only admin dashboard (htop-style) — same crate, installed alongside the server
install -o scrai -g scrai -m 755 \
  "$SCRAI_ADMIN_HOME/scrai-stage/target/release/scrai-admin" /opt/scrai/bin/scrai-admin.new
mv /opt/scrai/bin/scrai-admin.new /opt/scrai/bin/scrai-admin
# Faucet + distribution site (same crate). The site is baked in with include_str!, so
# updating a site MEANS replacing the binary of that instance. Two instances exist:
# public (scrai-faucet, no auth) and preview (scrai-faucet-tokumai, behind basic_auth).
case "$FAUCET_TARGET" in public|both)
  install -o scrai -g scrai -m 755 \
    "$SCRAI_ADMIN_HOME/scrai-stage/target/release/scrai-faucet" /opt/scrai/bin/scrai-faucet.new
  mv /opt/scrai/bin/scrai-faucet.new /opt/scrai/bin/scrai-faucet ;;
esac
case "$FAUCET_TARGET" in preview|both)
  install -o scrai -g scrai -m 755 \
    "$SCRAI_ADMIN_HOME/scrai-stage/target/release/scrai-faucet" /opt/scrai/bin/scrai-faucet-tokumai.new
  mv /opt/scrai/bin/scrai-faucet-tokumai.new /opt/scrai/bin/scrai-faucet-tokumai ;;
esac
install -o scrai -g scrai -m 644 \
  "$SCRAI_ADMIN_HOME/scrai-stage/pricing.json" /opt/scrai/pricing.json
# payment placeholder page (Caddy serves /opt/scrai/site/payment as its own vhost);
# tolerated missing so an older stage without the file still deploys
if [ -f "$SCRAI_ADMIN_HOME/scrai-stage/server/site/payment/index.html" ]; then
  install -D -o scrai -g scrai -m 644 \
    "$SCRAI_ADMIN_HOME/scrai-stage/server/site/payment/index.html" /opt/scrai/site/payment/index.html
fi
# retire the Node deployment (keep .env, data/, images/, and our bin/)
rm -rf /opt/scrai/node_modules /opt/scrai/dist /opt/scrai/src /opt/scrai/scripts \
  /opt/scrai/public /opt/scrai/package.json /opt/scrai/package-lock.json \
  /opt/scrai/tsconfig.json /opt/scrai/.nym
cat > /etc/systemd/system/scrai.service <<UNIT
[Unit]
Description=scrai-server (tokumai mixnet service provider, Rust)
After=network-online.target
Wants=network-online.target

[Service]
User=scrai
Group=scrai
WorkingDirectory=/opt/scrai
ExecStart=/opt/scrai/bin/scrai-server
Restart=always
RestartSec=5
# dotenvy reads /opt/scrai/.env (CWD); data lands in /opt/scrai/data
Environment=SCRAI_DATA=/opt/scrai/data

[Install]
WantedBy=multi-user.target
UNIT
cat > /etc/systemd/system/scrai-faucet.service <<UNIT
[Unit]
Description=scrai-faucet (tokumai testnet faucet + download site, loopback only — Caddy in front)
After=network-online.target scrai.service
Wants=network-online.target

[Service]
User=scrai
Group=scrai
WorkingDirectory=/opt/scrai
ExecStart=/opt/scrai/bin/scrai-faucet
Restart=always
RestartSec=5
Environment=SCRAI_DATA=/opt/scrai/data

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl restart scrai
systemctl --no-pager status scrai | head -6
case "$FAUCET_TARGET" in public|both)
  if grep -Eq "^SCRAI_TESTNET=(1|true)" /opt/scrai/.env 2>/dev/null; then
    systemctl enable --now scrai-faucet >/dev/null 2>&1 || true
    systemctl restart scrai-faucet
    systemctl --no-pager status scrai-faucet | head -4
  else
    systemctl disable --now scrai-faucet >/dev/null 2>&1 || true
    echo "scrai-faucet: not enabled (SCRAI_TESTNET is not 1 in /opt/scrai/.env)"
  fi ;;
esac
# The preview unit carries SCRAI_TESTNET=0 in its own ExecStart — it serves the site
# only, so it restarts regardless of the .env kill switch.
case "$FAUCET_TARGET" in preview|both)
  systemctl restart scrai-faucet-tokumai
  systemctl --no-pager status scrai-faucet-tokumai | head -4 ;;
esac'

# APPLY_BODY travels inside a SINGLE-QUOTED remote command. One literal apostrophe in it
# ends that quoting and the remote bash dies with "unexpected EOF while looking for
# matching quote" — while PARSING, so the `if` below never even picks a branch and nothing
# runs at all (2026-09-04: the word "instance's" in a comment cost a deploy round).
# Catch it here rather than on the VPS.
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
  echo "→ installing /opt/scrai/bin/deploy-apply.sh (root-owned) — sudo once …"
  {
    printf '#!/usr/bin/env bash\nSCRAI_ADMIN_HOME="${1:?admin home required}"\n'
    printf '%s\n' "$APPLY_BODY"
  } | ssh "${SSH_OPTS[@]}" "$TARGET" 'mkdir -p ~/scrai-stage && cat > ~/scrai-stage/deploy-apply.new'
  ssh -t "${SSH_OPTS[@]}" "$TARGET" \
    'sudo install -D -o root -g root -m 755 ~/scrai-stage/deploy-apply.new /opt/scrai/bin/deploy-apply.sh && echo installed'
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

# Name the units BEFORE touching them. The site is baked into a binary, so "deploy the
# website" always means "replace a binary and restart its unit" — and which unit that is
# decides whether an unreleased brand lands on a public host or behind the password.
case "$FAUCET_TARGET" in
  public)  FAUCET_SAYS="scrai-faucet.service  →  scrai-faucet.hermes-stakepool.de  (PUBLIC, no auth)" ;;
  preview) FAUCET_SAYS="scrai-faucet-tokumai.service  →  tokumai.com  (basic_auth)" ;;
  both)    FAUCET_SAYS="BOTH — public host AND the password-protected preview" ;;
  none)    FAUCET_SAYS="none — no site is updated, server only" ;;
esac
echo
echo "   about to update, from $(basename "$SRC"):"
echo "     scrai.service        →  scrai-server + scrai-admin + pricing.json"
echo "     site  ($FAUCET_TARGET)   →  $FAUCET_SAYS"
echo
echo "→ 4/4  install binaries + pricing.json, restart the units named above"
# If the root-owned apply script exists, run it (one sudo call — NOPASSWD-able);
# otherwise fall back to the inline block (still one shared SSH connection).
ssh -t "${SSH_OPTS[@]}" "$TARGET" '
  set -e
  if [ -x /opt/scrai/bin/deploy-apply.sh ]; then
    sudo /opt/scrai/bin/deploy-apply.sh "$HOME" '"$FAUCET_TARGET"'
  else
    echo "   · (tip: run with --install-apply once + a NOPASSWD line for zero prompts)"
    sudo env SCRAI_ADMIN_HOME="$HOME" bash -c '"'"''"$APPLY_BODY"''"'"' _ '"$FAUCET_TARGET"'
  fi
'
# What is actually running now — read back, not assumed (no sudo needed for is-active).
echo "→ services after apply:"
ssh "${SSH_OPTS[@]}" "$TARGET" '
  for u in scrai scrai-faucet scrai-faucet-tokumai; do
    printf "   %-13s %s" "$u" "$(systemctl is-active "$u" 2>/dev/null || true)"
    printf "  (enabled: %s)\n" "$(systemctl is-enabled "$u" 2>/dev/null || true)"
  done
'
echo "✓ deployed. Logs:  ssh $TARGET 'journalctl -u scrai -f'   ·   ssh $TARGET 'journalctl -u scrai-faucet -f'"
echo "  First start bootstraps a fresh authority and a NEW Nym address — grab it"
echo "  from the logs (scrai-server: … address: …) and set it in the app."