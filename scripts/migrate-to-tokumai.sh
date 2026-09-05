#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# migrate-to-tokumai.sh — one-shot rename ON THE BOX:
#
#   /opt/scrai                   → /opt/tokumai
#   bin/scrai-server             → bin/tokumai-server
#   bin/scrai-admin              → bin/tokumai-admin
#   bin/scrai-faucet             → bin/tokumai-faucet            (public, :8790)
#   bin/scrai-faucet-tokumai     → bin/tokumai-faucet-preview    (tokumai.com, :8791)
#   scrai.service                → tokumai.service
#   scrai-faucet.service         → tokumai-faucet.service
#   scrai-faucet-tokumai.service → tokumai-faucet-preview.service
#
# Caddy, the NOPASSWD sudoers rule and the service account home are updated too —
# `inspect` lists everything under /etc that still points at the old path.
#
# Caddy IS touched, in one place: both vhosts serve /dl/* straight out of the site
# directory being moved (`root * /opt/scrai/site/dl`). Left alone, every download link 404s
# the moment the tree changes name. The Caddyfile is backed up, edited, validated and
# reloaded; the ports (8790/8791) do not change.
#
# WHY THIS IS A SCRIPT AND NOT A LIST OF COMMANDS
# /opt/scrai/data holds the Nym identity (data/.nym-server). Lose it and the server comes
# up with a NEW mixnet address, at which point every installed app is pointing at nothing —
# and it would ALSO mean a fresh authority.json and an empty money DB, because an absent
# state directory is a legitimate "first run" as far as the server is concerned. That
# failure is silent. So this script records the identity before the move and refuses to
# call itself finished if it changed afterwards.
#
# SAFE BY DEFAULT: without --apply it only inspects and prints the plan.
#
#   scripts/migrate-to-tokumai.sh <admin_user>@<vps-host>            # dry run
#   scripts/migrate-to-tokumai.sh <admin_user>@<vps-host> --apply    # do it
#   scripts/migrate-to-tokumai.sh <admin_user>@<vps-host> --rollback # undo it
#
# AFTER a successful --apply, run this once so the root-owned apply script and the deploy
# paths agree with the new layout — until then a deploy would recreate /opt/scrai:
#
#   scripts/deploy.sh <admin_user>@<vps-host> --install-apply
#
# NOT renamed here: the unix user `scrai` that the services run as. Nobody sees it, files
# are owned by uid rather than by name, and every extra moving part in a migration that
# touches money state is a risk that buys nothing. `usermod -l tokumai scrai` plus a matching
# User= in the units does it later, on a quiet day.
# ---------------------------------------------------------------------------
set -euo pipefail

TARGET="${1:-}"
MODE="${2:-}"
case "$TARGET" in --*) MODE="$TARGET"; TARGET=""; esac
TARGET="${TARGET:-${DEPLOY_TARGET:-${SCRAI_DEPLOY_TARGET:-}}}"
if [ -z "$TARGET" ]; then
  echo "usage: scripts/migrate-to-tokumai.sh <admin_user>@<vps-host> [--apply|--rollback]" >&2
  exit 2
fi

CM_SOCK="${TMPDIR:-/tmp}/tokumai-mig-$$"
SSH_OPTS=(-o ControlMaster=auto -o "ControlPath=${CM_SOCK}" -o ControlPersist=180)
cleanup() { ssh "${SSH_OPTS[@]}" -O exit "$TARGET" >/dev/null 2>&1 || true; rm -f "$CM_SOCK"; }
trap cleanup EXIT

echo "→ opening SSH connection to $TARGET (auth once) …"
ssh "${SSH_OPTS[@]}" "$TARGET" true

# ---------------------------------------------------------------------------
# Everything privileged, in one place. $1 = mode (inspect | apply | rollback).
# Kept apostrophe-free: it is embedded in a single-quoted remote command, and one stray
# quote makes the remote bash die while parsing (learned the hard way, 2026-09-04).
# ---------------------------------------------------------------------------
BODY='set -e
MODE="${1:?mode required}"
OLD=/opt/scrai
NEW=/opt/tokumai
BAK=/root/tokumai-migration
OLD_UNITS="scrai scrai-faucet scrai-faucet-tokumai"
NEW_UNITS="tokumai tokumai-faucet tokumai-faucet-preview"

say() { printf "   %s\n" "$*"; }

fingerprint() {   # what must survive the move, printed so both sides can be compared
  d="$1"
  printf "addresses: %s\n" "$(cat "$d/data/addresses.txt" 2>/dev/null | tr "\n" " " | sed "s/ $//")"
  printf "authority: %s\n" "$(sha256sum "$d/data/authority.json" 2>/dev/null | cut -d" " -f1)"
  printf "state.db : %s bytes\n" "$(stat -c%s "$d/data/state.db" 2>/dev/null || echo missing)"
  printf "env keys : %s\n" "$(grep -cE "^[A-Z_]+=" "$d/.env" 2>/dev/null || echo 0)"
}

units_state() {
  for u in $OLD_UNITS $NEW_UNITS; do
    if systemctl list-unit-files "$u.service" >/dev/null 2>&1 && \
       [ -f "/etc/systemd/system/$u.service" ]; then
      printf "   %-32s %-8s %s\n" "$u.service" "$(systemctl is-active "$u" 2>/dev/null || true)" "$(systemctl is-enabled "$u" 2>/dev/null || true)"
    fi
  done
}

write_units() {
  cat > /etc/systemd/system/tokumai.service <<UNIT
[Unit]
Description=tokumai-server (mixnet service provider, Rust)
After=network-online.target
Wants=network-online.target

[Service]
User=scrai
Group=scrai
WorkingDirectory=/opt/tokumai
ExecStart=/opt/tokumai/bin/tokumai-server
Restart=always
RestartSec=5
Environment=DATA=/opt/tokumai/data

[Install]
WantedBy=multi-user.target
UNIT
  cat > /etc/systemd/system/tokumai-faucet.service <<UNIT
[Unit]
Description=tokumai-faucet (testnet faucet + download site, loopback only - Caddy in front)
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
  cat > /etc/systemd/system/tokumai-faucet-preview.service <<UNIT
[Unit]
Description=tokumai-faucet-preview (tokumai.com site preview, loopback only - Caddy in front)
After=network-online.target
Wants=network-online.target

[Service]
User=scrai
Group=scrai
WorkingDirectory=/opt/tokumai
ExecStart=/usr/bin/env FAUCET_LISTEN=127.0.0.1:8791 TESTNET=0 /opt/tokumai/bin/tokumai-faucet-preview
Restart=always
RestartSec=5
Environment=DATA=/opt/tokumai/data

[Install]
WantedBy=multi-user.target
UNIT
}

# ---- inspect ---------------------------------------------------------------
if [ "$MODE" = inspect ]; then
  echo "current layout:"
  [ -d "$OLD" ] && say "$OLD exists" || say "$OLD MISSING"
  [ -d "$NEW" ] && say "$NEW already exists  <- migration looks done or half-done" || say "$NEW not present (good)"
  echo "binaries:"
  ls -1 "$OLD/bin" 2>/dev/null | sed "s/^/     /" || say "(no bin dir)"
  echo "units:"
  units_state
  echo "what must survive:"
  fingerprint "$OLD" | sed "s/^/     /"
  echo "other things pointing at $OLD (these break unless they are updated too):"
  grep -rl "/opt/scrai" /etc 2>/dev/null | grep -v "^/etc/systemd/system/scrai" | sed "s/^/     /" || true
  say "(nothing else)"
  exit 0
fi

# ---- rollback --------------------------------------------------------------
if [ "$MODE" = rollback ]; then
  [ -d "$NEW" ] || { echo "nothing to roll back: $NEW does not exist" >&2; exit 1; }
  [ -d "$BAK" ] || { echo "no backup at $BAK - refusing to guess" >&2; exit 1; }
  for u in $NEW_UNITS; do systemctl disable --now "$u" >/dev/null 2>&1 || true; done
  rm -f /etc/systemd/system/tokumai.service /etc/systemd/system/tokumai-faucet.service /etc/systemd/system/tokumai-faucet-preview.service
  mv "$NEW/bin/tokumai-server" "$NEW/bin/scrai-server" 2>/dev/null || true
  mv "$NEW/bin/tokumai-admin" "$NEW/bin/scrai-admin" 2>/dev/null || true
  mv "$NEW/bin/tokumai-faucet" "$NEW/bin/scrai-faucet" 2>/dev/null || true
  mv "$NEW/bin/tokumai-faucet-preview" "$NEW/bin/scrai-faucet-tokumai" 2>/dev/null || true
  mv "$NEW" "$OLD"
  cp -a "$BAK"/*.service /etc/systemd/system/ 2>/dev/null || true
  if [ -f "$BAK/Caddyfile.backup" ]; then
    cp -a "$BAK/Caddyfile.backup" /etc/caddy/Caddyfile
    systemctl reload caddy || true
  fi
  [ -f "$BAK/deploy-apply.sh.stale" ] && mv "$BAK/deploy-apply.sh.stale" "$OLD/bin/deploy-apply.sh" || true
  if [ -f "$BAK/sudoers.backup" ]; then
    install -o root -g root -m 0440 "$BAK/sudoers.backup" /etc/sudoers.d/scrai-deploy
  fi
  [ "$(getent passwd scrai 2>/dev/null | cut -d: -f6)" = "$NEW" ] && usermod -d "$OLD" scrai 2>/dev/null || true
  systemctl daemon-reload
  systemctl enable --now scrai >/dev/null 2>&1 || true
  systemctl restart scrai || true
  grep -Eq "^(SCRAI_)?TESTNET=(1|true)" "$OLD/.env" 2>/dev/null && systemctl enable --now scrai-faucet >/dev/null 2>&1 || true
  systemctl enable --now scrai-faucet-tokumai >/dev/null 2>&1 || true
  echo "rolled back to $OLD"
  units_state
  exit 0
fi

# ---- apply -----------------------------------------------------------------
[ -d "$OLD" ] || { echo "FATAL: $OLD does not exist - nothing to migrate" >&2; exit 1; }
[ -d "$NEW" ] && { echo "FATAL: $NEW already exists - refusing to merge two trees" >&2; exit 1; }

mkdir -p "$BAK"
BEFORE="$BAK/fingerprint-before.txt"
fingerprint "$OLD" > "$BEFORE"
echo "recorded what must survive:"
sed "s/^/     /" "$BEFORE"
# A Nym address is <identity>.<encryption>@<gateway>, all base58 - so the only shape check
# that holds is the @. (An earlier version looked for a leading n and rejected every real
# address on the box, 2026-09-05.)
ADDRS=$(sed -n "s/^addresses: //p" "$BEFORE")
case "$ADDRS" in
  *@*) ;;
  *)
  echo "FATAL: no mixnet address found in $OLD/data/addresses.txt." >&2
  echo "       Without it the identity cannot be verified after the move. Start the server" >&2
  echo "       once so it writes the file, then run this again." >&2
  exit 1
  ;;
esac

echo "stopping services …"
for u in $OLD_UNITS; do systemctl stop "$u" >/dev/null 2>&1 || true; done

echo "backing up .env and the old units to $BAK …"
cp -a "$OLD/.env" "$BAK/env.backup"
for u in $OLD_UNITS; do
  [ -f "/etc/systemd/system/$u.service" ] && cp -a "/etc/systemd/system/$u.service" "$BAK/"
done

echo "moving $OLD -> $NEW …"
mv "$OLD" "$NEW"

echo "renaming binaries …"
[ -f "$NEW/bin/scrai-server" ] && mv "$NEW/bin/scrai-server" "$NEW/bin/tokumai-server"
[ -f "$NEW/bin/scrai-admin" ] && mv "$NEW/bin/scrai-admin" "$NEW/bin/tokumai-admin"
[ -f "$NEW/bin/scrai-faucet" ] && mv "$NEW/bin/scrai-faucet" "$NEW/bin/tokumai-faucet"
[ -f "$NEW/bin/scrai-faucet-tokumai" ] && mv "$NEW/bin/scrai-faucet-tokumai" "$NEW/bin/tokumai-faucet-preview"

# Caddy serves /dl/* straight out of the site directory that is being moved, in BOTH
# vhosts. Left alone, every download link 404s the moment the tree changes name.
CADDY=/etc/caddy/Caddyfile
if [ -f "$CADDY" ] && grep -q "/opt/scrai" "$CADDY"; then
  echo "updating Caddy (it serves /dl from the moved tree) …"
  cp -a "$CADDY" "$BAK/Caddyfile.backup"
  sed -i "s|/opt/scrai|/opt/tokumai|g" "$CADDY"
  if command -v caddy >/dev/null 2>&1 && ! caddy validate --config "$CADDY" >/dev/null 2>&1; then
    echo "FATAL: the edited Caddyfile does not validate - restoring it" >&2
    cp -a "$BAK/Caddyfile.backup" "$CADDY"
    exit 1
  fi
  systemctl reload caddy || systemctl restart caddy || true
  say "Caddy updated and reloaded (backup in $BAK/Caddyfile.backup)"
fi

# The NOPASSWD sudoers rule names the apply script BY PATH, so after the move it stops
# matching and every deploy asks for a password again. Edited through a temp file and
# visudo -c, never in place: a malformed sudoers file locks sudo out of the box entirely.
SUDOERS=/etc/sudoers.d/scrai-deploy
if [ -f "$SUDOERS" ] && grep -q "/opt/scrai" "$SUDOERS"; then
  echo "updating the sudoers rule (it names the apply script by path) …"
  cp -a "$SUDOERS" "$BAK/sudoers.backup"
  TMP_SUDO=$(mktemp)
  sed "s|/opt/scrai|/opt/tokumai|g" "$SUDOERS" > "$TMP_SUDO"
  if visudo -c -f "$TMP_SUDO" >/dev/null 2>&1; then
    if install -o root -g root -m 0440 "$TMP_SUDO" "$SUDOERS"; then
      say "sudoers updated (backup in $BAK/sudoers.backup)"
    else
      say "WARNING: could not write $SUDOERS - deploys will ask for a password."
    fi
  else
    say "WARNING: the rewritten sudoers rule does not validate - left untouched."
    say "         Deploys will ask for a password until you fix $SUDOERS by hand."
  fi
  rm -f "$TMP_SUDO"
fi

# The service account home points into the tree as well. Cosmetic for the units (they set
# WorkingDirectory themselves) but a home that does not exist bites anything using ~scrai.
HOME_NOW=$(getent passwd scrai 2>/dev/null | cut -d: -f6)
if [ "$HOME_NOW" = "$OLD" ]; then
  usermod -d "$NEW" scrai 2>/dev/null && say "home of user scrai: $OLD -> $NEW" || \
    say "WARNING: could not move the home of user scrai (still $OLD)"
fi

# The root-owned apply script still points at /opt/scrai. Remove it rather than leave a
# stale one: deploy.sh then falls back to its own inline body, which carries the guard.
# Otherwise a deploy run before --install-apply would quietly recreate /opt/scrai.
if [ -f "$NEW/bin/deploy-apply.sh" ]; then
  mv "$NEW/bin/deploy-apply.sh" "$BAK/deploy-apply.sh.stale"
  say "stale deploy-apply.sh moved to $BAK (deploy.sh will use its inline body until --install-apply)"
fi

echo "installing units …"
write_units
for u in $OLD_UNITS; do
  systemctl disable "$u" >/dev/null 2>&1 || true
  rm -f "/etc/systemd/system/$u.service"
done
systemctl daemon-reload
systemctl enable tokumai >/dev/null 2>&1 || true
systemctl start tokumai
# the public faucet follows the same kill switch the deploy uses
if grep -Eq "^(SCRAI_)?TESTNET=(1|true)" "$NEW/.env" 2>/dev/null; then
  systemctl enable --now tokumai-faucet >/dev/null 2>&1 || true
else
  say "tokumai-faucet left disabled (TESTNET is not 1)"
fi
systemctl enable --now tokumai-faucet-preview >/dev/null 2>&1 || true

echo "waiting for the server to publish its mixnet address (up to 90s) …"
for i in $(seq 1 45); do
  [ -s "$NEW/data/addresses.txt" ] && break
  sleep 2
done
AFTER="$BAK/fingerprint-after.txt"
fingerprint "$NEW" > "$AFTER"
echo "after the move:"
sed "s/^/     /" "$AFTER"

if ! diff -q "$BEFORE" "$AFTER" >/dev/null; then
  echo "" >&2
  echo "FATAL: the identity did not survive the move." >&2
  diff "$BEFORE" "$AFTER" | sed "s/^/       /" >&2
  echo "       Nothing was deleted. Roll back with:" >&2
  echo "         scripts/migrate-to-tokumai.sh <target> --rollback" >&2
  exit 1
fi

echo ""
echo "identity verified unchanged. services:"
units_state
echo ""
echo "NEXT: refresh the root-owned apply script, or the next deploy recreates /opt/scrai:"
echo "  scripts/deploy.sh <target> --install-apply"
'

# ---------------------------------------------------------------------------
case "$BODY" in
  *"'"*) echo "migrate: BODY contains a single quote - rewrite that line without one" >&2; exit 1 ;;
esac

case "$MODE" in
  --apply)    REMOTE_MODE=apply ;;
  --rollback) REMOTE_MODE=rollback ;;
  "")         REMOTE_MODE=inspect ;;
  *) echo "unknown option: $MODE (use --apply or --rollback)" >&2; exit 2 ;;
esac

if [ "$REMOTE_MODE" = inspect ]; then
  echo ""
  echo "DRY RUN — nothing will be changed. Re-run with --apply to migrate."
  echo ""
fi

ssh -t "${SSH_OPTS[@]}" "$TARGET" "sudo bash -c '$BODY' _ $REMOTE_MODE"
