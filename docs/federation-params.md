# Federation / Coconut ecash — mechanics & tunable parameters

Status: **design locked, not yet implemented** (2026-08-17). This file is the single
source of truth for the values below so we can test them and adjust them later.
Every value here is meant to be a named, overridable constant/env — never a magic
number buried in code.

See the decision doc "Föderation oder Inseln" for the full rationale.

---

## The mechanics (what we build)

- **Scheme:** Coconut threshold ecash via `nym-compact-ecash` (BLS12-381). The
  scrai-servers are the `t`-of-`n` issuing **authorities**. Credit is valid across
  ALL servers (one aggregated verification key).
- **Double-spend handling = Nym's model:** a server **accepts a spend OFFLINE**
  (no synchronous global check — `Payment::spend_verify()` needs no spent-set), then
  reports the coin serials to a shared **majority quorum**. The quorum detects a
  reused serial and, from the two conflicting payments, runs `identify()` which
  **cryptographically reveals the double-spender's public key**.
- **Ban decision is on cryptographic PROOF only** — the two real conflicting
  `Payment`s that make `identify()` succeed. NEVER on a probabilistic Bloom hit
  (a Bloom filter is only a fast candidate index). This eliminates detection-side
  false positives.
- **Response is graduated, NOT whole-wallet-voiding:**
  1. the doubled coin is always **rejected/burned** (attacker gains nothing further);
  2. the identified key is **blacklisted only past a volume threshold** (see below),
     so an honest client hiccup / stale-backup restore does not ban an innocent user;
  3. **remaining credit is NOT confiscated** by default (reserved as a manual
     last-resort for egregious, clearly-intentional mass abuse).
- **Why not void the whole wallet:** an honest client that loses its spend counter
  (crash / stale backup / multi-device) re-spends coins and is cryptographically
  INDISTINGUISHABLE from a cheater. We cannot 100% rule that out, so voiding a paid
  wallet would risk innocents and is disproportionate (harm ≈ 1 cent/coin).

---

## Economic / denomination

| Name | Value | Meaning / rationale |
|---|---|---|
| `SCRAI_PER_USD` | `100_000` | existing peg: 10 USD = 1,000,000 SCRAI |
| `COIN_SCRAI` | `1_000` | 1 ecash coin = 1000 SCRAI = **$0.01** (one cent). Coarse on purpose: compact-ecash carries one serial PER coin, so fine coins → huge proofs. |
| ticketbook size `L` | `tier_scrai / COIN_SCRAI` | e.g. $10 → 1000 coins, $5 → 500, $20 → 2000, $50 → 5000. |
| `REDEEM_CHUNK_COINS` | `100` | redeem ~$1 (100 coins) into a session at a time, **uniform** across users. Not "all at once" (leaks balance + one big pseudonym + 1000-serial proof), not tiny bits (many shows + mixnet round-trips). |

SCRAI stays the **fine accounting unit on the SESSION layer** (redeem coarse coins →
session balance → per-chat reserve/settle). The coin only sizes the redemption chunk.

## Privacy timing (unchanged by Coconut)

- **Buy → first redeem stays time-decoupled.** The withdraw is account-signed
  (authorities know who withdrew); the spend is unlinkable-by-crypto but
  linkable-by-TIMING if adjacent. Coconut changes the crypto, not the timing
  observability — so the lazy/decoupled redemption is still required.

## Double-spend / blacklist thresholds

| Name | Default | Meaning |
|---|---|---|
| `BAN_ON_PROOF_ONLY` | `true` | blacklist only when `identify()` returns a key from two real payments; never on a Bloom hit alone. |
| `REJECT_DOUBLED_COIN` | `true` (always) | the reused serial is always refused/burned. |
| `BLACKLIST_THRESHOLD_COINS` | `5` | number of DISTINCT proven double-spent coins by one key within the window before the key is blacklisted. `1` = strict/risky, higher = lenient. |
| `BLACKLIST_WINDOW_S` | `3600` | window (seconds) over which double-spent coins are counted toward the threshold. |
| `CROSS_SERVER_FAST_BAN` | `true` | a double-spend proven across ≥2 DIFFERENT servers blacklists immediately (strong attack signature — an honest single-device client talks to one server at a time). |
| `VOID_REMAINING_CREDIT` | `false` | do not confiscate a banned wallet's unspent coins by default. Manual last-resort only. |

Rationale for the threshold: an honest client rollback tends to re-spend coins at
the **same** server; a profit-seeking attacker re-spends the **same coin at multiple**
servers in the offline window. `CROSS_SERVER_FAST_BAN` + a small coin threshold
catches real attacks while keeping innocent bans near-zero.

## Consistency / reconciliation

| Name | Default | Meaning |
|---|---|---|
| `RECONCILE_MODE` | `event-push + poll` | push serials to the quorum immediately; fallback poll for safety. |
| `RECONCILE_INTERVAL_S` | `5` | fallback reconcile interval → keeps the offline detection window to seconds for our own servers. |
| issuance threshold `t` / `n` | test: `t=2, n=2` | production: odd `n`, `t` = majority. |
| consistency quorum | majority of `n` | quorum-intersection prevents cross-server double-spend from going undetected; tolerate a minority down; **use an ODD node count** to avoid a 5/5 stall. |

## Client durability (prevents self-inflicted false-positive double-spends)

- Persist the spend counter **durably + atomically BEFORE** the payment leaves the
  client (fsync); never roll it back.
- **Idempotent retries:** on an uncertain network result, retry with the SAME
  `pay_info` (recognized as a benign replay), never re-spend a coin with new `pay_info`.
- **Single-device bearer wallet** — do not copy/sync across devices. (Held ecash is
  not seed-rebuildable anyway, so seed-restore can't reset the counter; only a stale
  full wallet-file backup could — warn against it.)

## Key setup

- Test (2 servers): trusted-dealer `ttp_keygen(t=2, n=2)` → 2 authority shares +
  aggregated `VerificationKey` (hardcoded/pinned in the client).
- Before foreign gateways: real DKG (inject shares via `SecretKeyAuth::create_from_raw`).
