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
| tiers | **€10 / €20 / €30 a month** for **1M / 2M / 3M TOKU**. Linear on purpose: the only reason to take a bigger one is to need it |
| yearly | 12 × the monthly price less 10 % — €108 / €216 / €324 |
| what 1M TOKU is | the same allowance the $10 tile buys today: roughly 3,700 fast text answers, or 570 from the strongest model, or 150 pictures |
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

## Changing tier mid-month

The answer to "I am running out" is **a bigger tier, not a top-up**. A tier change is
unambiguously a change to the contract; a top-up sold as extra allowance might be read as a
voucher, and then the three-year problem returns through the back door. Choosing the version
that raises no question beats answering the question.

**Up is immediate and pro rata**, by the same day arithmetic as a partial first month:

```
€10 → €20 on 15 March:  remaining = 31 − 15 + 1 = 17 days
                        price     = €10 × 17/31 = €5.48
                        allowance = +1,000,000 × 17/31 = +548,387 TOKU
```

**Down takes effect on the 1st.** It cannot be immediate: the month's allowance was drawn as
coins on the 1st, and the server cannot take them back — it does not know which coins are
whose. That is the same blindness the whole design rests on, so this is not a limitation to
fix but a consequence to state.

**A note on the ladder.** The tiers are exactly linear, which is honest and easy to explain.
Worth knowing that they are not linear for us: every charge carries a fixed fee component, so
one €30 subscription earns more than three €10 ones. If a volume discount is ever wanted,
that is where it costs nothing.

## Cancellation

**Cancellable to the end of the current month**, with no notice period beyond that. Stripe
does this natively (`cancel_at_period_end`); what has to be decided is only when a request
still counts as being on time.

**Do not pick a timezone — make it not matter.** An unclear term in consumer T&Cs is read in
the customer's favour, so a timezone clause is a fight that cannot be won. Instead: a
cancellation that arrives on the last day of the month **in any timezone** ends the
subscription at that month's end. In practice that means accepting one until roughly twelve
hours after local midnight. The cost is half a day of service the customer has already paid
for; the gain is that the question can never be argued.

**The yearly plan may not roll into a second year.** Under the German rules on fair consumer
contracts a fixed term cannot renew itself for another fixed term: after twelve months it has
to continue indefinitely, cancellable with at most one month's notice. So the yearly plan is
"twelve months, then monthly" — not "twelve months, then twelve more".

## Duties a subscription brings that one-off sales did not

Not legal advice — these are the specific things to put in front of a lawyer.

- **The cancellation button (§312k BGB).** A consumer contract concluded online needs a
  plainly labelled "cancel your contract here" control, reachable without logging in.
  *Nothing about this needs state on our side*: Stripe holds the subscription, and the
  monthly reset already asks it whether the subscription is still active — a customer who
  cancels in Stripe's portal simply gets no allowance on the 1st. Stripe's Customer Portal
  may be enough on its own.
  What is genuinely unclear is whether §312k is satisfied by sending the customer to a third
  party's portal: the provision is prescriptive about the sequence — a button with a
  prescribed label, a confirmation page, and an immediate acknowledgement in text form. That
  is a question for the lawyer, not a guess.
  If the answer is "it has to happen on your site", it is still cheap and still stateless:
  the button takes an e-mail address, the server asks **Stripe** whether there is an active
  subscription for it, cancels it through the API and sends the acknowledgement. Ask, act,
  forget — no customer database appears.
- **Pre-contract information.** The recurring price, the term, how it renews and how it ends
  must be visible *before* the order button, and the button itself must name the payment
  obligation.
- **No customer KYC.** We are a merchant, not a payment institution; Stripe runs its checks
  on us. A subscription does not change that.
- **VAT becomes recurring.** Whatever the answer on cross-border digital services is, it now
  applies every month instead of once.
- **The right of withdrawal** still applies at signup, with the same "start immediately and
  lose it" consent the shop already collects.

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

## Credit that already exists — the migration rule

Decided 2026-09-18, and it is a constraint rather than a preference: **nobody loses credit they
have already bought.** One customer holds ~$9 of it, bought under terms that say account credit
does not expire. That promise is not renegotiable, and the design has to make keeping it cheap
rather than exceptional.

**Two buckets per account, never one number.**

| | |
| --- | --- |
| `entitlements[account]` | what exists today. One-off purchases, redeemed codes, coins handed back from a retired device. **Never expires, never reset.** Untouched by everything below |
| `allowance[account]` | new. `{ granted, left, period, tier }`. **Set** on the 1st, lapses with the period |

**Spending order: the perishable pocket first.** A withdrawal takes from `allowance.left`, and
only what is left over comes from `entitlements`. That is the order that costs the customer
least — the allowance dies on the 1st either way, the bought credit does not.

**Returns go back where they came from.** A device handing coins back (device move, giveback)
must not turn a monthly allowance into permanent credit — draw a million on the 1st, hand it
back on the 31st, repeat, and the voucher problem is rebuilt by hand. So the period carries a
third number, `drawn_from_allowance`, and a return of value `v` restores
`min(v, drawn_from_allowance)` to the allowance (decrementing it) and only the remainder to
`entitlements`. Symmetric with the spending order, and it cannot leak in either direction.

**No subscription is a supported state, indefinitely.** As long as anyone holds entitlement the
app has to work with no plan at all: the buy sheet becomes a subscribe sheet, but the withdraw
path against `entitlements` stays exactly as it is today, $1 books and all. This is not a
migration window that closes — it is simply what an account with credit on it does.

**What the customer sees** is screen 2 and screen 6 of the mockups: the old credit is its own
row, in its own colour, saying "does not expire" and "used once a month's allowance runs out".
It is deliberately NOT added into the allowance meter — folding non-expiring credit into a
number labelled "resets 1 Oct" would say the opposite of the truth.

Mockups (2026-09-18): https://claude.ai/code/artifact/e557a83b-2f92-4f58-970e-f49d91f93f48

## Still open

- **Does Stripe's Customer Portal satisfy §312k**, or must the flow live on our own site?
  Either way it needs no stored customer data (see above) — but the answer decides whether
  anything is built at all.
- ~~Is a mid-period top-up a voucher?~~ **Settled by avoiding it**: the answer to running out
  is a bigger tier, not a top-up.
- ~~The existing paying customer~~ **Settled** — see "Credit that already exists" above: two
  buckets, allowance spent first, returns capped, and no-subscription stays a supported state.
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

1. Stripe: `mode: subscription`, anchored to the 1st, three monthly and three annual prices
   in EUR, pro-rata first period, pro-rata upgrades, downgrades at period end,
   `cancel_at_period_end`. Poll the subscription's state the way the one-off invoice is
   polled today — still no webhook, still no inbound port.
2. Server: subscription state per account, the monthly reset (set, not add), the pro-rata
   rule above, and the reset date in the account reply so the app can show it.
3. Client: draw the whole allowance at reset; "X TOKU left, resets on 1 October"; the
   thresholds; and the buy sheet becomes a subscribe sheet.
4. Epochs shortened, and `terms.html` / `privacy.html` rewritten around a service period.
