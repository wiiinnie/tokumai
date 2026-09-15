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

## D · No session balance — core, server and client built 2026-09-14

Every prompt pays with coins directly; the server verifies offline, burns the serials,
answers. No session id, no counter, no signature.

**How the "pay before, price after" problem is solved: the tender.** A single ecash payment
is atomic, so one payment for the ceiling would charge the worst case every time, and there
is no change that would not re-link the payer. Instead the request carries SEVERAL payments
valued 1, 2, 4, … (`core/src/tender.rs`), so every whole number of coins up to the ceiling
is an exact subset sum. The server burns exactly the subset the answer cost and leaves the
rest unsubmitted — those notes are still good, come home to the wallet, and are spent before
a fresh coin is taken out of a book. Rounding is therefore the coin (0.1 ¢), not the ceiling.
Re-sending an unburned note verbatim is safe: identical (serials, pay_info) is a replay, never
a double-spend.

Built so far:

- `core/src/tender.rs` — plan and selection, `Purse::spend_tender` (all-or-nothing, never a
  half-advanced counter), `QuorumStore::hold`/`release` (the coin-paid equivalent of
  reserving a balance: a coin being served cannot be tendered again).
- Server (`server/src/chat.rs`) — `reserve` holds the serials and caps the answer to what
  the coins cover; the pairings and the provider call run off the dispatch loop; `settle`
  burns the exact subset and names it in the reply (`burned`), a provider failure burns
  nothing, and an identical tender replays the cached answer.
- Client (`src-tauri/src/lib.rs`) — `coin_request`/`coin_settle`/`build_tender`, spare notes
  and an unanswered tender in the wallet, held credit counts spares. Behind
  `TOKUMAI_COIN_CHAT=1` until the fleet accepts tenders.

**Cut over 2026-09-14.** The coin is 0.1 ¢ and a ticketbook is 1000 coins ($1, unchanged).
The book size lives in the authority keys, so this was a key rotation: the old authority was
moved aside and a fresh 1000-coin one bootstrapped. Every ticketbook held on a device at that
moment became worthless — accepted, we are not live. Server-side entitlement and session
balances are in TOKU and were untouched. Two measured consequences: the epoch material grew
from 25 KB to 207 KB (now cached on disk, SURB budget 64 → 130, so an app older than 0.6.4
may struggle to withdraw), and a tender carries the ceiling rather than the cost, so an
ordinary text prompt puts ~19 coins ≈ 9 KB on the table.

**Carrying the old session balances over (built 2026-09-14).** Credit that sits on the
session layer has nothing left to spend it, so it must reach the account before that layer
is deleted. `session.drain`: the session signs "empty me onto account X" — the destination
is inside the signed message, so the signature cannot be replayed onto another account —
and the account signs that it is X. Both keys come from one recovery phrase. It lands as
entitlement and is drawn as books at once. The request is the only one in the app that
carries a session key and an account key together: it links them at the server while it is
in flight, nothing about it is stored (the session store holds a balance and a counter,
never an account), and it is needed once per account. With coins as the payment path the
"Redeem now" control is gone — it pushed coins ONTO the session layer, which is backwards
now; the way off a device is "Move credit back to my account".

`session.drain` is migration code with a fixed lifetime: it exists only until the balances
that predate coins have been carried over, and it GOES OUT WITH THE SESSION LAYER — server
dispatch, `SessionStore::drain`, `auth::session_hands_over`, the client command and the
card's box. It is the only place in the app where the two key kinds meet, so leaving it in
after the layer is gone would keep a link alive that nothing needs any more.

**The session layer is gone (2026-09-15).** With it went `redeem`, `session.status`,
`session.drain`, the SessionStore and its shared-ledger crate, the counter and signature on
every chat, and the abuse strikes that hung on the session id — the moderation prefilter
now refuses each request on its own merits, which is what a payment in anonymous cash
allows. Roughly 2,500 lines. What used to be "your balance on the server" is simply the
coins on the device; the account holds what has not been drawn yet.

Still to do for D: make a capped answer visible (the server shortens an answer to fit the
coins and says nothing), and a local notification before books expire.

Decided parameters:

| | |
|---|---|
| coin | **two denominations: 0.1 ¢ (fine) and 1 ¢ (coarse).** A payment costs ~490 B and ~4 ms of server pairings PER COIN, and a request tenders its CEILING — so at one size a 9 ¢ picture was ninety coins, 44 KB, and more notes than a tender may carry. The bulk of a tender is coarse and the remainder fine: the same ceiling is ~19 coins and ~9 KB, still exact to 0.1 ¢. Measured: issuing is one blind signature regardless of book size; a 10-coin book's epoch material is ~25 KB, cached on disk per denomination. 0.01 ¢ rejected (115 ms server CPU per text prompt, 4.6 MB material). |
| how the value is set | NOT by a field in the coin — compact ecash has none. The issuing KEY decides, so each denomination is its own authority (`server/src/mint.rs`), and a note names its denomination only to pick the key it is verified against. A note that lies about it fails verification. Adding the coarse authority is additive: the fine one keeps its file and its books. |
| planning a tender | 1, 2, 4, … at BOTH levels. A note is atomic — one note of nine coarse coins pays 9 ¢ and nothing else, while 1+2+4+2 pays every whole cent up to nine. The fine plan reaches just under ONE coarse coin (further is granularity nobody can use) and is minted only when the notes already on the table do not carry it. |
| prices | quoted in TOKU, rounded up to a fine coin. Paying in increments as the answer streams is NOT needed: the tender above settles exactly in one round trip. |
| books | **ten coins each: a fine book is 1 ¢, a coarse book 10 ¢**, drawn several at a time in ONE round trip. The book is the unit credit moves onto a device in, so it bounds both what a lost device costs and what is stranded on the account. A device holds a **value** cap of $1.00 (`WORKING_TOKU`) — the bulk in coarse books plus a float of three fine ones as small change — and fetches more by itself below a third of it, after a random 30–120 s pause, so the account call does not sit next to the question that emptied it. Stranded remainder: under 1 ¢ (the smallest book). Measured against the live server 2026-09-14: eight books in three seconds. Nym does the same thing — its ticketbooks are 50 tickets, 7 days, fetched several at a time with `--amount`. |
| expiry | **90 days** (`BOOK_VALIDITY_DAYS`, terms §6a), so opening the app once a quarter keeps every coin alive. The date is a property of the ISSUING EPOCH, not of the book: every book of one authority dies on the same day, so a book drawn late in an epoch lives only the rest of it — which is what rolling epochs (below) are for. **Built 2026-09-15:** on the background start-up sweep, books within `SWAP_WINDOW_DAYS` (14) of their date are handed back and redrawn from the current epoch — whole books, one note each, in the same batched return the device-move uses. The window has to be WIDER than the interval at which people open the app, since the swap only runs while it is open: three days protects a daily user and nobody else. The sweep now also starts for an expiring book on a FULL device, which is the one case where money is at stake and nothing else would trigger it. Still to do: a local notification at expiry − 3 d (no server push, no device token). Credit on the ACCOUNT never expires. |
| rolling epochs | **Built 2026-09-15** (`server/src/mint.rs`). A fresh authority every `ROLL_EVERY_DAYS` (7), the older ones kept while their books can still be spent (expiry + 3 days), and new books always issued from the newest — so a book is drawn with 83–90 days of life instead of whatever was left of a fixed month. The mint rolls at boot and on the six-hourly tick; a note names its epoch (`Note.exp_date`) and a withdrawal the epoch it was built for, both only to pick a key. Without it the server would have stopped issuing anything around 14 Oct 2026: an authority's expiration is fixed at bootstrap and nothing rotates it, so on that date every book dies AND issuing breaks (the authority would sign against a past date). A new authority is bootstrapped before the current one runs out and both are served — new books come from the newest, older ones stay verifiable until their own date. The server CANNOT move books into the new epoch: blind signatures mean it never saw them, and nothing links a book to an account. Only the device can swap, which is why the client-side return/redraw is the other half. |
| device change | "Move credit back to my account" — built: account-signed, burns the notes, credits their value as entitlement, in batches with a progress bar. No wallet export (a copy is a double-spend). |
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
- rows older than `QUORUM_RETAIN_DAYS` are pruned at boot and every six hours. The default
  AND the floor are `BOOK_VALIDITY_DAYS + 3` (93 since 2026-09-15), derived rather than
  typed twice: pruning a serial while its book can still be spent would make a second spend
  of that coin invisible. Safe because `Authority::verify_payment` rejects spend dates more
  than `SPEND_DATE_PAST_SECS` (2 days) in the past, so a coin is unspendable from its
  book's expiry + 2 days on;
- the admin's "burned serials" is a lifetime counter in the quorum meta (seeded once from
  the rows), so pruning never shrinks it (`quorum_pruned_coins` keeps the pruned sum).
