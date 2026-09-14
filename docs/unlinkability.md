# Unlinkability: keeping a purchase apart from the prompts it pays for

Decisions of 2026-09-12/13 (design canvas: "Kauf und Prompt entkoppeln"). Blind signatures
cut the *cryptographic* link between an account that buys credit and the session that spends
it. Two things still linked them at our size: **time** (withdraw at T, first redeem at
T + seconds, nobody else around) and the **reply-SURB sender tag** — one Nym client per app
start carried both the account call and the session call. This document records what we
change, in the order we build it.

## A · Purchase client — built 2026-09-14 (commit 1466583)

Account-side calls (invoice create/status/cancel, invite check, code redeem, App Store
receipt, entitlement + coin withdrawal) leave through a **second, ephemeral Nym client**:

- created lazily on the first account call, dropped after a `collect` or when the buy
  sheet closes (`buy_close`); fresh keys every time (`src-tauri/src/lib.rs: buy_transport`);
- **gateway rule**: one of the operator's entry gateways, **never the chat client's** — the
  chat client's gateway is the user's choice and may change any time, so the rule is
  re-checked on *every* use and the purchase client is the one that moves
  (`nym::hermes_gateway_excluding`), compared by base58 identity;
- the UI shows "Connecting privately for this purchase via …" while it comes up
  (`buy-phase` events), silent once ready.

Credit never "lands" on the purchase client: the purchase credits the account's
entitlement, `collect` turns it into bearer coins in the device wallet, and only the chat
client redeems them.

## B · First redeem, early top-up — designed, not built (superseded by D)

- Erstkauf: a 60 s random wait before the first redeem (W from the catalog, `redeemWindowSec`,
  server-raised with traffic); "Use now" skips it once. Honest note: at today's purchase
  rate this protects little — it exists so the window can grow without an app update.
- Nachkauf: nudge at $1 remaining ("$1 left · top up early for privacy"); the new coins are
  only needed once the remainder is spent, hours to days later. Refill from the **oldest
  purse first**. The trickle is a convenience opt-in and is named as such at its switch.

## C · Rotating session pseudonyms — designed, not built (superseded by D)

`session_index` is fixed at 0, so every prompt a user ever sent hangs on one pseudonym.
Rotation "when dry" (new index at the refill point; remainder under one prompt's price is
lost) — never at app start (the user rejected the hybrid: the old session's balance would be
stranded). Costs: restore must scan indices, "session #n" becomes a rotating number,
moderation strikes rotate with it, admin "users" becomes "sessions".

## D · No session balance — target architecture

Every prompt pays with coins directly; the server verifies offline, burns the serials,
answers. No session id, no counter, no redeem. Decided parameters:

| | |
|---|---|
| coin | **0.1 ¢** ($0.001); a $1 book = 1,000 coins. Measured: issuing is one blind signature regardless of book size; key material per client and epoch 46 KB → 468 KB (cache on disk, ~30-day epoch); a payment costs ~470 B + ~4 ms per coin. 0.01 ¢ rejected (115 ms server CPU per text prompt, 4.6 MB material). |
| prices | quoted in coins; pay-as-you-stream in fixed increments, no change (returned coins would be a link) |
| books | $1 each, drawn lazily from the entitlement, one spare book fetched when the current one drops under 30 % — at most ~$1.30 on a device |
| expiry | books last ~30 days; on app start, books < 3 days from expiry are returned to the account (plus a fresh book if in use); local notification at expiry − 3 d; no server push |
| device change | "Return credit to my account" — no wallet export (a copy is a double-spend) |
| two devices | share account + entitlement, never books |
| trickle | removed entirely |

Order: A → quorum rebuild → D. B and C only if D slips.

## E · Several servers

One **payment server** (account, entitlement, issuing, returns), any number of **prompt
servers** (verify coins offline, answer). Value moves only through the client; no shared
ledger. Cross-server double-spend stays possible until the servers compare serials:
damage ≤ (servers − 1) × coins until the next sync; serial-journal gossip over an operator
channel (start: 5 min) plus Bloom filters; bans only with an `identify` proof, distributed
with the proof. Before a second server takes real money: t-of-n DKG for issuing, the
authority key published and pinned in the client, the signed price list. Third parties only
as compute suppliers on our behalf (they submit the payments they served and are paid per
request; they never take customer money) — the e-money question stays open (decision of
2026-09-03 stands).

## Quorum retention — built 2026-09-14

Found on the way: the double-spend store kept every serial ever seen, plus every payment,
in RAM, loaded whole at boot and never pruned (~50 KB per dollar of revenue). Now
(`core/src/quorum.rs`, `server/src/store.rs`):

- spent serials live in `spent_serials` (serial → record), payments in `quorum_records`,
  read through a read-only index; only spends accepted since the last atomic batch write
  are in RAM;
- rows older than `QUORUM_RETAIN_DAYS` (default 35, floor 33) are pruned at boot and every
  six hours. Safe because `Authority::verify_payment` rejects spend dates more than
  `SPEND_DATE_PAST_SECS` (2 days) in the past and 31 days in the future: a coin is
  unspendable from its book's expiry + 2 days on, and a book lives ~30 days from issue;
- the admin's "burned serials" is a lifetime counter in the quorum meta (seeded once from
  the rows), so pruning never shrinks it (`quorum_pruned_coins` keeps the pruned sum).
