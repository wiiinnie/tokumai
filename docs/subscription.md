# The subscription model

Decided 2026-09-17, **nothing built**. This replaces one-off credit entirely, and the
reason is legal rather than commercial — so the reasoning is written down first.

## Why one-off credit had to go

Prepaid credit that expires is a forfeiture clause, and in German AGB law a forfeiture
clause on something the customer has already paid for is measured against the three-year
limitation period. Anything much shorter is the kind of clause courts strike down.

Today's terms survive that test on one sentence: what expires is "by design never more than
the credit most recently drawn onto that device" — at most $1, technically justified. The
moment credit lives on the device in full, that defence is gone.

And the failure mode is worse than losing the clause. If the clause is void the customer's
claim survives, but blind signatures mean **we cannot check whether they already spent it**
— the server never saw the coins. We would owe money we have no way to verify. Holding the
service open for three years to avoid that is a liability nobody wants to carry.

A subscription is a different legal object: a service period that ends. Unused allowance
lapsing at the end of a month is a mobile data plan, not a voucher.

## The model

| | |
| --- | --- |
| price | **€10 / month**, or **€108 / year** (12 × €10 less 10 %) |
| what you get | **1,000,000 TOKU per calendar month** — the same allowance the $10 tile buys today |
| rollover | **none.** The month starts at 1,000,000, not at 1,000,000 plus what is left |
| currency | **EUR.** The operator's books are in euro; a USD price would put an FX spread on every monthly charge instead of once |
| billing period | the **calendar month**, anchored to the 1st, not to the day of signup |
| partial first month | **charged and granted pro rata, per actual day** |

### Pro rata, exactly

Months are 28 to 31 days, so the fraction is computed against the real month, never against
a nominal 30:

```
days_served = last_day_of_month - start_day + 1     (the start day counts: service begins that day)
fraction    = days_served / days_in_month
price       = €10 × fraction, rounded to the cent
allowance   = 1,000,000 TOKU × fraction, rounded down
```

Starting 15 March: 31 − 15 + 1 = 17 days, 17/31 → €5.48 and 548,387 TOKU. Starting 15
February in a non-leap year: 14/28 → €5.00 and 500,000 TOKU. The same date is a different
fraction in a different month, which is why this is computed and not approximated.

Stripe prorates the charge natively when a subscription is anchored to the 1st; the
allowance uses the same fraction so the two cannot drift.

## What it does to privacy

**The core claim is untouched.** Prompts are paid with blind-signed coins over the mixnet.
The server cannot tie a coin to the withdrawal it came from, and there is no IP. Nothing in
this document changes that.

**One thing gets worse.** A subscription needs a durable link between the Stripe customer
and the tokumai account — the server has to know whose allowance to reset each month. Today
that link is deleted after `ACCOUNT_LINK_DAYS` (7). Under a subscription it lives as long as
the subscription does. What was "we cannot join a name to an account after a week" becomes
"we hold that join while you are a customer". It is worth saying plainly on the site rather
than leaving it to be discovered.

**Two things get better, and they are not small.**

*Uniform withdrawals.* Everybody's allowance resets on the same day and everybody draws the
same amount. A withdrawal on the 1st says nothing about who made it — same figure, same
hour, whole subscriber base. One-off purchases could never do this: $5 here and $50 there is
a fingerprint.

*The account leaves the recurring path.* Because the loss is bounded to one month and heals
itself on the 1st, the full allowance can be drawn **at reset, in one go**. There are then
no top-up withdrawals driven by spending, which is what leaked the usage cadence — the
server stops being able to tell a heavy month from a quiet one. The account is touched
exactly once a month, at the same moment as everyone else, for the same amount.

This is the design the anonymous coin swap was meant to buy (`docs/unlinkability.md`, D),
reached without building a second path to a signed credential.

## Technical consequences

**Coin lifetime shortens.** A book only has to outlive the month it was drawn in. Give the
epochs ~35–40 days rather than 90–97: enough that a book drawn on the 31st is comfortably
alive, short enough that the burned-serial store shrinks accordingly. Do **not** try to make
coins die exactly at period end — expiry is a property of the issuing epoch, and per-customer
epochs are not buildable. A subscriber who draws late keeps their coins a few weeks into the
next month. That is generosity, bounded and in the customer's favour, and the annual cost is
capped regardless: only 1M TOKU is ever added per month.

**Burned serials: ~40 days instead of 368.** `quorum_retain_days_floor` follows
`BOOK_VALIDITY_DAYS`, so this falls out of the shorter epoch on its own — roughly a tenth of
what year-long books would have needed.

**The allowance SETS, it does not ADD.** `entitlement[account] = 1_000_000` on reset. An
`+=` would let unspent allowance accumulate, and an accumulating balance is the voucher we
just escaped.

**Stripe's job is small.** It answers one question: is this subscription active and paid?
The monthly reset is ours. That is why an annual subscription needs no extra machinery — it
is the same reset, with one payment behind it instead of twelve.

**Fees favour the year.** Every charge carries a fixed component; twelve small charges pay it
twelve times. The first real card sale measured 6.2 % in fees plus a 3.0 % FX spread
(`.env.example`, MARGIN). Annual billing pays the fixed part once and removes eleven chances
for a card to fail.

**Load.** Everyone drawing on the 1st is a spike. Spread the draw over the first hours of
the day at random; the anonymity set barely notices, the server does.

## Still open

- **Is a mid-period top-up a service extension or a voucher?** The intent: "extra allowance
  until the end of this month", granted as `entitlement += X` and wiped by the next reset,
  never carried forward. Technically trivial. Whether that holds as a service extension
  rather than as prepaid credit with a one-month expiry is **not settled** — it goes to the
  lawyer with the rest. If it is a voucher, the three-year problem comes back and the answer
  is to sell a bigger plan instead of a top-up.
- **The existing paying customer** bought $10 of one-off credit under terms that say account
  credit does not expire. That promise stands. Whatever the model becomes, his balance is
  honoured as bought.
- **Invite and gift codes** currently grant non-expiring credit — the same voucher problem in
  miniature. Under a subscription they should grant a *month of service* instead.
- **Apple.** An auto-renewable subscription is an IAP subscription, and Apple's cut applies
  every month rather than once. The break-even from the card sale (a commission above 23.1 %
  loses money at MARGIN 1.30) becomes a recurring condition, not a one-time one.
- **Positioning.** The pricing page says, in as many words, "there is no monthly plan". That
  sentence goes, and the page has to explain why the change is in the customer's favour
  rather than read as a retreat.
- **Terms §6a** is written around credit drawn onto a device from a non-expiring account.
  It needs rewriting around a service period.

## What would be built

1. Stripe: `mode: subscription`, anchored to the 1st, monthly and annual prices in EUR,
   pro-rata first period. Poll the subscription's state the way the one-off invoice is
   polled today — still no webhook, still no inbound port.
2. Server: subscription state per account, the monthly reset (set, not add), the pro-rata
   rule above, and the reset date in the account reply so the app can show it.
3. Client: draw the whole allowance at reset; "X TOKU left, resets on 1 October"; the
   thresholds; and the buy sheet becomes a subscribe sheet.
4. Epochs shortened, and `terms.html` / `privacy.html` rewritten around a service period.
