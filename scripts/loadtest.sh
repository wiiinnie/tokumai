#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# loadtest.sh — staged capacity runs against a scrai-server (docs/load-testing.md).
#
#   scripts/loadtest.sh local  [mode] [stages]   start a THROW-AWAY local server
#                                                (fake payments + mock provider, own
#                                                data dir + .env, real mixnet) and run
#                                                the stages against it. Default: chat.
#   scripts/loadtest.sh <nym-address> [mode] [stages]
#                                                run against an existing server. Only
#                                                ping/models are sensible against the
#                                                live server: chat would need real
#                                                credit and would hit the real model.
#
#   mode    ping | models | chat | mixed            (default: chat for local, ping otherwise)
#   stages  comma list of client counts             (default: 5,10,20,40)
#
# Every stage = one scrai-loadtest run; results land in loadtest/results/<ts>-<label>/.
# Knobs (env): REQUESTS (per user, default 10), THINK_MS (default 2000), RAMP_MS (default
# 750), TIMEOUT_MS (default 120000), MOCK (server-side "<delay_ms>:<answer_chars>", default
# 1500:800), EXTRA (extra flags for every run, e.g. EXTRA="--fast"), SCRAI_MIX_SEND_MS /
# SCRAI_MIX_COVER_MS (server egress knobs, local only; e.g. SCRAI_MIX_SEND_MS=4),
# SCRAI_MIX_CLIENTS=K (local: K identities) + SPREAD=all|primary|both (spread users or not).
# ---------------------------------------------------------------------------
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET="${1:-}"
[ -n "$TARGET" ] || { sed -n 2,22p "$0" | sed 's/^# \{0,1\}//'; exit 2; }
MODE="${2:-}"
STAGES="${3:-5,10,20,40}"
REQUESTS="${REQUESTS:-10}"
THINK_MS="${THINK_MS:-2000}"
RAMP_MS="${RAMP_MS:-750}"
TIMEOUT_MS="${TIMEOUT_MS:-120000}"
MOCK="${MOCK:-1500:800}"
EXTRA="${EXTRA:-}"

echo "→ building scrai-loadtest + scrai-server (release) …"
cargo build --release -p scrai-loadtest -p scrai-server >/dev/null

SERVER_PID=""
cleanup() {
  if [ -n "$SERVER_PID" ]; then
    echo "→ stopping local server (pid $SERVER_PID)"
    kill -INT "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

if [ "$TARGET" = "local" ]; then
  MODE="${MODE:-chat}"
  # Own CWD + .env: the server reads .env from its CWD, and the repo's .env carries real
  # rails (NYX_*/BTCPAY_*), which the fake rail refuses to coexist with. The Nym identity
  # under .loadtest/server/data is kept across runs (same address, faster start).
  SRV_DIR="$ROOT/.loadtest/server"
  mkdir -p "$SRV_DIR/data"
  cat > "$SRV_DIR/.env" <<ENV
SCRAI_FAKE_PAYMENTS=1
SCRAI_MOCK_PROVIDER=$MOCK
SCRAI_DATA=$SRV_DIR/data
SCRAI_PRICING=$ROOT/pricing.json
# widen/narrow the chat cap to see where the semaphore starts refusing (default 64)
SCRAI_MAX_INFLIGHT_CHATS=${SCRAI_MAX_INFLIGHT_CHATS:-64}
# server egress: per-packet send delay / cover-stream delay (unset = SDK defaults 20 / 200)
${SCRAI_MIX_BURST:+SCRAI_MIX_BURST=$SCRAI_MIX_BURST}
${SCRAI_MIX_SEND_MS:+SCRAI_MIX_SEND_MS=$SCRAI_MIX_SEND_MS}
${SCRAI_MIX_COVER_MS:+SCRAI_MIX_COVER_MS=$SCRAI_MIX_COVER_MS}
# K Nym identities (front doors) for the one server process (SCRAI_MIX_CLIENTS=K, random
# gateways; or SCRAI_GATEWAY_FALLBACK=gw1,gw2 to pin them); SPREAD=all spreads the
# simulated users over all of them, SPREAD=primary (default) hits only the first,
# SPREAD=both runs every stage twice (primary, then all) against the same server
${SCRAI_MIX_CLIENTS:+SCRAI_MIX_CLIENTS=$SCRAI_MIX_CLIENTS}
${SCRAI_GATEWAY_FALLBACK:+SCRAI_GATEWAY_FALLBACK=$SCRAI_GATEWAY_FALLBACK}
ENV
  LOG="$SRV_DIR/server.log"
  : > "$LOG"
  echo "→ starting local scrai-server (fake payments, mock provider $MOCK) — log: $LOG"
  ( cd "$SRV_DIR" && exec "$ROOT/target/release/scrai-server" ) >>"$LOG" 2>&1 &
  SERVER_PID=$!
  # The address is printed once the client is on the mixnet.
  for _ in $(seq 1 120); do
    if ADDR=$(grep -m1 -oE '^  address: .*' "$LOG" | sed 's/^  address: //'); [ -n "${ADDR:-}" ]; then break; fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then echo "server died — see $LOG"; tail -20 "$LOG"; exit 1; fi
    sleep 1
  done
  [ -n "${ADDR:-}" ] || { echo "server did not announce an address in time — see $LOG"; exit 1; }
  echo "→ local server live: $ADDR"
  WANT="${SCRAI_MIX_CLIENTS:-1}"
  if [ "$WANT" -gt 1 ]; then
    # the extra identities announce themselves as "  address[k]: …" once connected
    for _ in $(seq 1 180); do
      N=$(grep -cE '^  address(\[[0-9]+\])?: ' "$LOG" || true)
      [ "$N" -ge "$WANT" ] && break
      sleep 1
    done
    ADDR_ALL=$(grep -oE '^  address(\[[0-9]+\])?: .*' "$LOG" | sed -E 's/^  address(\[[0-9]+\])?: //' | paste -sd, -)
    echo "→ $N identities live"
  fi
  sleep 2
else
  MODE="${MODE:-ping}"
  ADDR="$TARGET"
  if [ "$MODE" = "chat" ] || [ "$MODE" = "mixed" ]; then
    echo "!! $MODE against an existing server needs SCRAI_FAKE_PAYMENTS=1 on that server (funding runs the fake rail)."
  fi
fi

# Which address set(s) each stage hits. Against an existing multi-identity server pass
# ADDR_ALL=a,b,c (its extra addresses) yourself.
PRIMARY="${ADDR%%,*}"
case "${SPREAD:-primary}" in
  all)  VARIANTS=("all") ;;
  both) VARIANTS=("primary" "all") ;;
  *)    VARIANTS=("primary") ;;
esac
IFS=',' read -r -a STAGE_LIST <<< "$STAGES"
for N in "${STAGE_LIST[@]}"; do
  for V in "${VARIANTS[@]}"; do
  if [ "$V" = "all" ] && [ -n "${ADDR_ALL:-}" ]; then TO="$ADDR_ALL"; else TO="$PRIMARY"; fi
  NADDR=$(awk -F, '{print NF}' <<< "$TO")
  echo
  echo "================ stage: $N clients · $MODE · $REQUESTS req/user · think $THINK_MS ms · $V ($NADDR address(es)) ================"
  # shellcheck disable=SC2086
  "$ROOT/target/release/scrai-loadtest" \
    --server "$TO" --mode "$MODE" --clients "$N" --requests "$REQUESTS" \
    --think-ms "$THINK_MS" --ramp-ms "$RAMP_MS" --timeout-ms "$TIMEOUT_MS" \
    --label "${MODE}-${N}c-${V}" $EXTRA || echo "stage $N failed (continuing)"
  done
  if [ -n "$SERVER_PID" ]; then
    # (ping replies are not logged as "handled" — this line counts the stateful ops only)
    echo "server-side: $(grep -c 'handled' "$LOG") stateful requests handled so far · $(grep -c 'busy' "$LOG") busy refusals in log"
  fi
done
echo
echo "done — summaries: ls loadtest/results/"
