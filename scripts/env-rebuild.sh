#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# env-rebuild.sh — write a clean, categorised /opt/tokumai/.env.new from the
# .env that is on the box, ready for the mainnet switch.
#
#   scripts/env-rebuild.sh <admin_user>@<vps-host>            # report only
#   scripts/env-rebuild.sh <admin_user>@<vps-host> --apply    # + write .env.new
#
# WHAT IT DOES
#   · drops the SCRAI_ prefix and applies the handful of real renames
#     (TESTNET_FAUCET_ADDRESS → FAUCET_ADDRESS_TESTNET, MIN_CHARGE_SCRAI →
#      MIN_CHARGE_TOKU, FAUCET_MNEMONIC → FAUCET_MNEMONIC_TESTNET)
#   · sorts everything into sections with a line of explanation per key
#   · sets TESTNET=0 and comments out the rails that only have a testnet value,
#     because the boot guard refuses to start a mainnet server with those
#   · leaves every key that still NEEDS A VALUE commented and empty, so the
#     operator fills it in and nothing starts half-configured
#   · carries anything it does not recognise into a section at the end rather
#     than dropping it silently
#
# WHAT IT DOES NOT DO
#   It never touches the live .env, and it never prints a secret: the whole
#   transformation runs ON the box, and the report shows values masked. Review
#   /opt/tokumai/.env.new there, fill in the blanks, then swap it in yourself:
#
#     sudo -u scrai cp /opt/tokumai/.env /opt/tokumai/.env.bak-$(date +%F)
#     sudo -u scrai mv /opt/tokumai/.env.new /opt/tokumai/.env
#     sudo systemctl restart tokumai tokumai-faucet
#
# Needs sudo on the box (the .env is owned by the service account), so expect
# one password prompt — the deploy sudoers rule covers the apply script only.
# ---------------------------------------------------------------------------
set -euo pipefail

TARGET="${1:-}"
MODE="${2:-}"
case "$TARGET" in --*) MODE="$TARGET"; TARGET="" ;; esac
TARGET="${TARGET:-${DEPLOY_TARGET:-${SCRAI_DEPLOY_TARGET:-}}}"
if [ -z "$TARGET" ]; then
  echo "usage: scripts/env-rebuild.sh <admin_user>@<vps-host> [--apply]" >&2
  exit 2
fi
case "$MODE" in ""|--apply) ;; *) echo "unknown option: $MODE (only --apply)" >&2; exit 2 ;; esac

echo "→ $TARGET  ($([ "$MODE" = --apply ] && echo "writing /opt/tokumai/.env.new" || echo "report only"))"

# One authentication for the copy and the run.
CM_SOCK="${TMPDIR:-/tmp}/tokumai-envrb-$$"
SSH_OPTS=(-o ControlMaster=auto -o "ControlPath=${CM_SOCK}" -o ControlPersist=120)
LOCAL_PY="$(mktemp)"
REMOTE_PY="/tmp/tokumai-env-rebuild.$$.py"
cleanup() {
  ssh "${SSH_OPTS[@]}" "$TARGET" "rm -f $REMOTE_PY" >/dev/null 2>&1 || true
  ssh "${SSH_OPTS[@]}" -O exit "$TARGET" >/dev/null 2>&1 || true
  rm -f "$LOCAL_PY" "$CM_SOCK"
}
trap cleanup EXIT

# The transformation is copied over as a FILE, not piped into stdin: sudo needs the
# terminal to ask for the password, and ssh -t cannot give it one while stdin is a
# heredoc ("a terminal is required to read the password").
cat > "$LOCAL_PY" <<'PYEOF'
import os, re, sys, pwd, grp

ENV = "/opt/tokumai/.env"
OUT = ENV + ".new"
APPLY = "--apply" in sys.argv

if not os.path.exists(ENV):
    sys.exit(f"FATAL: {ENV} does not exist — is this the migrated box?")

# ---- read the old file, last assignment of a key wins ----------------------
old, unknown_lines = {}, []
for line in open(ENV, encoding="utf-8", errors="replace").read().splitlines():
    s = line.strip()
    if not s or s.startswith("#") or "=" not in s:
        continue
    k, v = s.split("=", 1)
    old[k.strip()] = v.strip()

SECRETISH = ("KEY", "MNEMONIC", "SECRET", "TOKEN", "PASSWORD", "SALT")
def mask(k, v):
    if any(w in k for w in SECRETISH):
        return "****" if v else ""
    return v if len(v) <= 46 else v[:43] + "…"

used = set()
DUPES = []             # (spelling that lost, key it belongs to)
def take(*names):
    """First of `names` that is set in the old file (bare or SCRAI_-prefixed).

    Every spelling of every candidate counts as consumed, not just the winner —
    otherwise a box carrying both FAUCET_URL and SCRAI_FAUCET_URL would have the loser
    copied into the new file as an unrecognised key, where it reads like a second
    setting. cfg() prefers the bare name, so the winner here is the one in effect.
    """
    hit = None
    for n in names:
        for cand in (n, "SCRAI_" + n):
            if old.get(cand, "").strip():
                if hit is None:
                    hit = (old[cand], cand)
                elif cand not in used:
                    DUPES.append((cand, hit[1]))
                used.add(cand)
    return hit if hit else (None, None)

report = []            # (new key, value-or-None, note)
REVIEW = []            # carried values that are probably stale
def line_for(key, comment, default=None, force=None, sources=None, needed=False, review=None):
    """One key in the new file. Returns the text lines for it."""
    # `take` runs even when the value is forced, so the old key counts as consumed and
    # does not resurface in the "not recognised" list at the end.
    found, src = take(*(sources or [key]))
    val = force if force is not None else found
    out = [f"# {c}" for c in comment]
    if val is not None:
        out.append(f"{key}={val}")
        note = "set by this script" if force is not None else ("carried" if src == key else f"was {src}")
        # A value can carry over cleanly and still be wrong for mainnet — an old host, a
        # release gate from two versions ago. Say so instead of quietly keeping it.
        if review and review[0](val):
            note = "REVIEW: " + review[1]
            REVIEW.append((key, review[1]))
        report.append((key, mask(key, val), note))
    elif needed:
        # Left UNcommented and empty on purpose: a commented placeholder invites filling in
        # the value while the # stays, and the key then reads as unset — which is exactly
        # how the first mainnet start failed (2026-09-05). Empty is still unset, so the
        # boot guard catches a forgotten one either way.
        out.append("# TODO fill this in — the server refuses to start without it")
        out.append(f"{key}=")
        report.append((key, None, "NEEDS A VALUE"))
    else:
        out.append(f"#{key}=" + ("" if default is None else default))
        report.append((key, None, "not set (optional)"))
    return out

L = []
def section(title, *body):
    L.append("")
    L.append("# " + "=" * 74)
    L.append(f"# {title}")
    L.append("# " + "=" * 74)
    for b in body:
        L.extend(b)

L += [
    "# tokumai server configuration",
    "# Rebuilt by scripts/env-rebuild.sh. One key per line, no export, quote any",
    "# value containing a space. Names have no prefix; the server still reads the old",
    "# SCRAI_<NAME> spelling as a fallback, but nothing here needs it any more.",
    "#",
    "# Network-scoped keys resolve <NAME>_MAINNET first, then <NAME>_TESTNET, then the",
    "# bare name. TESTNET decides which world the server believes it is in — and the",
    "# boot guard refuses to start with TESTNET=0 while a money rail has only a",
    "# _TESTNET value, because that would take real money against test infrastructure.",
]

section("Network mode",
    line_for("TESTNET", [
        "0 = real money. 1 = testnet: the ONLY purchase is the $1 invite credit, and it",
        "is paid from a sandbox wallet. Invite credits work in both modes.",
    ], force="0"),
    line_for("ALLOW_SINGLE_AUTHORITY", [
        "This deployment is a 1-of-1 Coconut authority (single operator). Without this",
        "the server refuses to issue credentials against real money and crash-loops.",
    ], default="1"),
    line_for("DATA", ["State directory. The unit sets it too; keep them identical."],
             default="/opt/tokumai/data"),
)

section("Mixnet identity",
    line_for("GATEWAY_MASTER", [
        "Entry gateway of the PRIMARY identity — the address every installed app has.",
        "Changing it changes nothing for an existing identity; it pins a fresh one.",
    ]),
    line_for("GATEWAY_FALLBACK", ["Comma-separated gateways for the additional mix clients."]),
    line_for("MIX_CLIENTS", ["How many mixnet identities to run (~40 users each)."], default="3"),
    line_for("MIX_BURST", [
        "1 = burst sending. Mandatory above a handful of clients; without it cover",
        "traffic saturates every core.",
    ], default="1"),
)

section("Release + site",
    line_for("MIN_APP", [
        "Oldest app version the server serves. Raise it ONLY once every platform has",
        "the new build published — older apps get an update sheet instead of a catalog.",
    ], default="0.5.0", review=(lambda v: v.strip() < "0.5.0", "still gates on an old release")),
    line_for("UPDATE_URL", ["Where that update sheet sends people."], default="https://tokumai.com/",
             review=(lambda v: "tokumai.com" not in v, "still points at the old host")),
    line_for("SITE_URL", [
        "Our own site. The app builds the /pay hand-over link from it (Apple's IAP gate).",
    ], default="https://tokumai.com"),
    line_for("SITE_DL_DIR", ["Where publish-downloads.sh puts the bundles (Caddy serves /dl)."],
             default="/opt/tokumai/site/dl"),
    line_for("DL_IOS", ["TestFlight join link, shown on the site once review is through."]),
    line_for("DL_IOS_GUIDE", ["Optional sideload guide for testers."]),
)

section("Providers",
    line_for("PROVIDERS", [
        "Which model providers are offered. A model whose provider is not listed is",
        "refused server-side, not just hidden.",
    ], default="gemini,openai"),
    line_for("GEMINI_API_KEY", [
        "Google AI Studio key. Deliberately the BARE name: the same account is billed",
        "in both modes, and a _TESTNET/_MAINNET pair here only invites ambiguity (the",
        "server refuses to boot with both set).",
    ], sources=["GEMINI_API_KEY", "GEMINI_API_KEY_MAINNET", "GEMINI_API_KEY_TESTNET"], needed=True),
    line_for("OPENAI_API_KEY", ["OpenAI key. Same reasoning as above."], needed=True),
    line_for("OPENAI_MODELS", ["Which OpenAI model ids may be sold (empty = the built-in list)."]),
    line_for("MODERATION_PREFILTER", ["1 = run OpenAI's moderation endpoint before a prompt."]),
    line_for("ABUSE_STRIKES_PER_DAY", ["Refusals per account per day before it is cut off."]),
    line_for("OPENAI_RETENTION_DAYS", ["What we tell users about OpenAI-side retention (privacy page)."]),
    line_for("OPENAI_SEARCH_USD", ["What OpenAI charges per web-search call — it is billed on."]),
)

section("Pricing + metrics",
    line_for("MARGIN", [
        "Retail margin on provider cost, clamped >= 1. Default 1.4 if unset — which is",
        "NOT what this deployment sells at, so set it explicitly.",
    ], default="1.15", needed=True),
    line_for("MIN_CHARGE_TOKU", ["Floor per request in TOKU (was MIN_CHARGE_SCRAI)."],
             sources=["MIN_CHARGE_TOKU", "MIN_CHARGE_SCRAI"]),
    line_for("FREE_TIER_FACTOR", [
        "Discount on models served from a provider's free daily allowance (0..1,",
        "default 0.5). The margin still applies on top.",
    ]),
    line_for("PURCHASE_TIERS", ["Sellable amounts in USD (empty = 5,10,20,50)."]),
    line_for("CARD_MIN_USD", ["Smallest amount the card rail sells (fees + chargeback exposure)."]),
    line_for("FX_EUR_PER_USD", ["EUR per USD, for the admin view only."]),
    line_for("METRICS_TZ", ["Timezone the daily metric boundary uses."], default="Europe/Berlin"),
    line_for("PRICING", ["Path to a pricing table override (empty = the shipped one)."]),
)

section("Money rail: native NYM (Nyx chain)",
    line_for("NYX_LCD_URL_MAINNET", [
        "Mainnet LCD/REST endpoint the chain watcher polls: https://api.nymtech.net",
    ], sources=["NYX_LCD_URL_MAINNET"], needed=True),
    line_for("NYX_RECEIVE_ADDRESS_MAINNET", [
        "The address buyers send NYM to, and the address the faucet pays. Real funds.",
    ], sources=["NYX_RECEIVE_ADDRESS_MAINNET"], needed=True),
    line_for("NYX_LCD_URL_TESTNET", ["Kept so TESTNET=1 still works for a rehearsal."]),
    line_for("NYX_RECEIVE_ADDRESS_TESTNET", ["Sandbox receive address."]),
    line_for("NYX_PRICE_URL", ["Price feed override (empty = the built-in source)."]),
)

section("Money rail: coins via a processor  — OFF until BTCPay vs CoinGate is settled",
    ["# With none of these set the Bitcoin tile is greyed out in the app instead of",
     "# failing on tap. Set ONE of the two groups, never both.",
     "#COINGATE_API_KEY_MAINNET=",
     "#COINGATE_PAY_CURRENCY=BTC",
     "#COINGATE_PLATFORM_ID=5",
     "#COINGATE_RECEIVE_CURRENCY=",
     "#BTCPAY_URL_MAINNET=",
     "#BTCPAY_STORE_ID_MAINNET=",
     "#BTCPAY_API_KEY_MAINNET="],
)

section("Money rail: card via Mollie  — OFF until KYB is through",
    ["# A LIVE key only. The test key can be marked paid without money moving, which is",
     "# why the boot guard refuses to start a mainnet server that has only the test one.",
     "#MOLLIE_API_KEY_MAINNET=",
     "#MOLLIE_REDIRECT_URL=https://tokumai.com/paid"],
)

section("Invite faucet  (runs beside real purchases)",
    line_for("FAUCET_ENABLED", ["1 = testers can redeem invite codes. 0 is the kill switch."],
             default="1"),
    line_for("FAUCET_URL", [
        "Where the app sends a tester to redeem: this host serves /claim.",
    ], default="https://faucet.tokumai.com",
             review=(lambda v: "tokumai.com" not in v, "still points at the old host")),
    line_for("FAUCET_ADDRESS_MAINNET", [
        "The faucet wallet's own address — the ONLY sender whose NYM settles an invite",
        "invoice. Fail closed: unset means every invite credit is refused.",
    ], sources=["FAUCET_ADDRESS_MAINNET"], needed=True),
    line_for("FAUCET_MNEMONIC_MAINNET", [
        "Seed of that wallet. It holds REAL NYM now — do not reuse the sandbox seed.",
    ], sources=["FAUCET_MNEMONIC_MAINNET"], needed=True),
    line_for("FAUCET_ADDRESS_TESTNET", ["The sandbox faucet wallet, kept for a rehearsal."],
             sources=["FAUCET_ADDRESS_TESTNET", "TESTNET_FAUCET_ADDRESS"]),
    line_for("FAUCET_MNEMONIC_TESTNET", ["Its seed."],
             sources=["FAUCET_MNEMONIC_TESTNET", "FAUCET_MNEMONIC"]),
    line_for("FAUCET_DAILY_MAX", [
        "Claims per day. On mainnet this IS the daily spend cap — each claim costs a",
        "real dollar of NYM.",
    ], default="20"),
    line_for("FAUCET_RESERVE_UNYM", ["Never spend the wallet below this (unym)."], default="5000000"),
    line_for("FAUCET_LISTEN", ["Loopback only; Caddy terminates TLS in front."],
             default="127.0.0.1:8790"),
    line_for("FAUCET_EXPLORER", ["Explorer URL prefix for the tx link on the claim page."]),
    line_for("FAUCET_RPC", ["Tendermint RPC override (empty = derived from the LCD URL)."]),
    line_for("FAUCET_BECH32_PREFIX", ["Address prefix (n)."]),
    line_for("FAUCET_DENOM", ["Denomination (unym)."]),
)

section("Limits  (all optional — the defaults are compiled in)",
    line_for("INVOICE_PER_MIN", ["Server-wide invoice creations per minute."]),
    line_for("MAX_INFLIGHT_CHATS", ["Concurrent provider calls."]),
    line_for("MAX_INFLIGHT_GATEWAY", ["Concurrent payment-gateway calls."]),
    line_for("MAX_INFLIGHT_CRYPTO", ["Concurrent credential issuances."]),
    line_for("MAX_INFLIGHT_OPENAI", ["Concurrent OpenAI calls."]),
    line_for("DEFAULT_MAX_TOKENS", ["Reply cap when the client does not ask for one."]),
    line_for("THINKING_BUDGET", ["Thinking-token budget for models that have one."]),
    line_for("MIX_COVER_MS", ["Cover-traffic interval."]),
    line_for("MIX_SEND_MS", ["Send interval. Small values plus many clients = every core busy."]),
    line_for("ABUSE_SALT", ["Salt for the per-account abuse counter (rotates the buckets)."]),
)

section("Development switches — MUST stay unset on a real-money server",
    ["# FAKE_PAYMENTS=1 settles invoices without money. The server refuses to boot with",
     "# it set next to a real rail, but do not rely on that: leave all three alone.",
     "#FAKE_PAYMENTS=",
     "#MOCK_PROVIDER=",
     "#DEV_AUDIT="],
)

# ---- anything we did not recognise ----------------------------------------
leftover = {k: v for k, v in old.items() if k not in used}
# Testnet-only rails are dropped on purpose, with a reason.
DROPPED = []
for k in sorted(leftover):
    base = k[6:] if k.startswith("SCRAI_") else k
    if base.endswith("_TESTNET") and base.split("_TESTNET")[0].startswith(("MOLLIE", "BTCPAY", "COINGATE")):
        DROPPED.append(k)
for k in DROPPED:
    leftover.pop(k, None)
if leftover:
    L += ["", "# " + "=" * 74,
          "# Not recognised by this script — carried over untouched. Check whether they",
          "# are still needed; the SCRAI_ prefix still resolves, so nothing breaks either way.",
          "# " + "=" * 74]
    for k in sorted(leftover):
        L.append(f"{k}={leftover[k]}")

text = "\n".join(L).lstrip("\n") + "\n"

# ---- report (masked) -------------------------------------------------------
w = max(len(k) for k, _, _ in report)
print()
print("  KEY".ljust(w + 4), "VALUE".ljust(24), "WHERE FROM")
print("  " + "-" * (w + 2), "-" * 24, "-" * 24)
need = []
for k, v, note in report:
    if note == "NEEDS A VALUE":
        need.append(k)
    print("  " + k.ljust(w + 2), (v if v is not None else "—").ljust(24), note)
if DROPPED:
    print("\n  dropped (a mainnet server refuses to boot with these):")
    for k in DROPPED:
        print("    " + k)
if DUPES:
    print("\n  duplicate spellings, dropped (the one in effect was kept):")
    for lost, winner in DUPES:
        print("    " + lost.ljust(30) + "-> " + winner)
if leftover:
    print("\n  carried over unrecognised:")
    for k in sorted(leftover):
        print("    " + k)
if REVIEW:
    print("\n  carried but probably stale:")
    for k, why in REVIEW:
        print("    " + k.ljust(22) + why)
if need:
    print("\n  FILL IN BEFORE STARTING (" + str(len(need)) + "):")
    for k in need:
        print("    " + k)

if not APPLY:
    print(f"\n  report only — nothing written. Re-run with --apply to write {OUT}.")
    sys.exit(0)

with open(OUT, "w", encoding="utf-8") as f:
    f.write(text)
os.chmod(OUT, 0o600)
try:
    os.chown(OUT, pwd.getpwnam("scrai").pw_uid, grp.getgrnam("scrai").gr_gid)
except KeyError:
    pass
print(f"\n  wrote {OUT} ({len(text)} bytes, 0600). The live .env is untouched.")
print("  Fill in the blanks, then:")
print("    sudo -u scrai cp /opt/tokumai/.env /opt/tokumai/.env.bak")
print("    sudo -u scrai mv /opt/tokumai/.env.new /opt/tokumai/.env")
print("    sudo systemctl restart tokumai tokumai-faucet")
PYEOF

ssh "${SSH_OPTS[@]}" "$TARGET" true
scp "${SSH_OPTS[@]}" -q "$LOCAL_PY" "$TARGET:$REMOTE_PY"
ssh "${SSH_OPTS[@]}" -t "$TARGET" "sudo python3 $REMOTE_PY $MODE"
