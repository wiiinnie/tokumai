# Vouchers — buying on the site, redeeming in the app

**Sketch, not built.** Written 2026-09-08 to be argued with before any code exists.

## Why a code at all

Outside the US and the EU, App Store rule 3.1.1 forbids the app from pointing anyone at an
outside purchase — not the purchase, the *pointing*. So the app cannot link to `/pay`, and a
buyer who finds the site anyway needs a way to get the credit into their app.

Handing them a code is the only delivery that asks nobody anything:

- the **site** never learns an account (no top-up ID, no email, no device),
- the **app** never learns where the code came from,
- and neither has to tell the other about a person.

It also buys two things we do not have today: someone can pay **for** someone else, and a
purchase can happen on a machine that never sees the recovery phrase.

## What a voucher is

Bearer money, like the coins. Whoever holds the string holds the credit. That is a decision,
not an accident — it is what lets the two sides stay unconnected — and it has consequences
that belong on the page where the code appears, not in a help article:

- lose it and it is gone,
- forward it and someone else has your credit,
- and **we cannot re-issue one**, because we do not keep it (see below).

## Where it lives, and why not in the pay snapshot

`Pay` is held in memory and written to `state.db` as one serialized snapshot. That is fine for
invoices, which only the server touches. It is wrong for vouchers, for a reason worth spelling
out: **the admin has to be able to void one**, and a second process cannot edit a blob that the
server is going to overwrite from memory on its next save.

So vouchers get **their own SQL table** in `state.db`, next to `daily` and `daily_users`:

```sql
CREATE TABLE vouchers (
  hash        TEXT PRIMARY KEY,   -- sha256 of the code, never the code
  toku        INTEGER NOT NULL,
  invoice     TEXT NOT NULL,      -- the paid invoice it was minted from
  created_at  INTEGER NOT NULL,
  redeemed_at INTEGER,            -- burned; the row is now spent
  credited_at INTEGER,            -- the entitlement actually landed
  void_at     INTEGER             -- refunded before redemption
);
```

**Only the hash.** The server can verify a code and burn it; it cannot produce one. Which means
a lost code can be refunded but never recovered — and equally, nobody can talk a code out of
support, because support does not have one either.

## The three transitions

```
        mint                    redeem                    (credit)
paid ──────────► unredeemed ──────────► redeemed ────────────────► credited
                     │
                     └──── void ────► voided        (refunded, never redeemable)
```

### Redemption has to survive a crash between two writes

The burn is SQL; the credit is an entitlement bump inside the pay snapshot. Two stores, so
there is a window:

- **credit then burn** → a crash in between leaves a valid code that was already paid out.
  Double credit. Unacceptable.
- **burn then credit** → a crash in between burns a code whose credit never arrived. The buyer
  loses the money. Also unacceptable — but *recoverable*, because the row remembers.

So: **burn first, credit second, and reconcile at boot.** Any row with `redeemed_at` set and
`credited_at` null gets its entitlement credited on the next start. That is the same shape as
the interrupted-withdrawal recovery (M-cl-2) — the same argument, so the same pattern.

The redemption request carries an account signature, exactly like `invoice.create`. The
entitlement has to land somewhere, and that somewhere must prove it owns itself.

### Voiding is the refund path, and it starts at the payment

A refund never starts with the code — it starts with somebody saying "I paid and want it
back", holding a receipt number. So the row carries `invoice`, and voiding is:

1. find the voucher by its invoice,
2. refuse if `redeemed_at` is set (spent credit cannot be clawed back — same rule as the app),
3. set `void_at`,
4. refund the payment by hand at Mollie.

In `tokumai-admin`, next to `c` (mint an invite code). It writes `state.db`, which nothing in
the admin has done before — the narrowness is what makes that acceptable: one column, on one
row, only when `redeemed_at IS NULL`. A CLI form would be
`tokumai-admin voucher <invoice> void`, but the panel is the better home: the operator is
already looking at the invoice there.

## Where the code is created

At settlement of a **web purchase**, not at checkout — an unpaid invoice must never mint one.
`/pay` raises an invoice with a marker that says "this one pays out as a voucher", and when
`settle()` sees it paid:

1. generate 16 random bytes → `TOKU-XXXX-XXXX-XXXX` (the existing invite-code shape, so the
   app's field accepts both without a second parser),
2. insert the **hash**,
3. return the code to the page **once**, in the reply to its own status poll.

The code exists in one place after that: the buyer's screen. That is the whole design.

## What `/pay` has to gain

- the tiles and the rail picker the app has (whatever the server reports, no more),
- the two consent boxes — a purchase is a purchase, and § 356 (5) does not care which surface
  it happened on,
- the code screen, with copy and the warning,
- **a receipt PDF**: no app is involved, so the page produces it. `public/receipt.js` already
  writes one and is not app-specific.

## Open edges

**Nothing in the app may name the site.** The redeem field says "invite codes, vouchers and
gift codes are all redeemed here" and stops there. This is the only thing keeping the flow
inside 3.1.1 outside the US and EU, and it is one careless sentence away from not being.

**A code has no expiry in this sketch.** It probably should not have one either — unredeemed
vouchers are a liability that never ages out, but expiring prepaid credit is the kind of thing
consumer law dislikes. Worth an opinion from the same lawyer as the terms.

**VAT changes nothing.** § 19 UStG means no tax is shown either way. Mollie gives a country,
NYM does not, and the admin already counts the second as `unknown`.

**Refunds get simpler, not harder.** An unredeemed voucher is provably unspent, so it can be
refunded properly — unlike credit in the app, where bearer coins make "is it unspent" an
unanswerable question. That distinction belongs in the terms: *before redemption, refundable;
after, not*.
