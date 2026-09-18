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
| tiers | **€10 / €20 / €50 a month** for **700k / 1.5M / 4M TOKU** (decided 2026-09-18) |
| yearly | 12 × the monthly price less 10 % — €108 / €216 / €540 |
| the ladder | the rate improves at each step: 70,000 / 75,000 / 80,000 TOKU per euro. Shown on the tier as a **euro figure** — the same TOKU at the entry rate would cost €21.43 and €57.14, so the plans save €1.43 and €7.14 a month. A percentage would beg "off what?"; and two tiers badged with the same number would read as a bug |
| what 700k TOKU is | roughly 3,400 answers from a fast model, 430 from the strongest, or 50 pictures at 2K. Figures must be GENERATED from `pricing.json`, never typed |
| why not a round million | the price points stay familiar (€10, not €12.99) and the allowance carries the margin instead. Fewer TOKU per euro — **never** a bigger `MARGIN`, which would change what a TOKU buys for everyone who already holds one |
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

## The first month differs by rail, because the rails do

Stripe can be anchored to the 1st and prorates a partial first period; **Apple cannot and
does not.** The App Store charges the full price at signup and runs the period from that
day — 18 September to 18 October, then the 18th of every month after.

So the first month is granted differently, on purpose:

| | charge | first grant |
| --- | --- | --- |
| Stripe (web) | pro rata, to the day | pro rata, from the same fraction |
| App Store | **full price, at once** | **a full month** |

Granting a fraction against a full charge would be taking money for less service at exactly
the moment a customer decides whether to keep the thing. Someone subscribing on the 28th
would pay €10 and get three days; that it evens out on the 1st is an argument nobody should
have to be given.

What it costs: thirteen grants across the first twelve charges instead of twelve and a
fraction — one extra part-month per subscriber, once, averaging about €2.85 of provider
cost. Roughly a tenth of a first year's profit on that subscriber, and the price of a rail
whose billing we do not control.

It cannot run away: `subscribe_or_renew` grants at most once per calendar month, so a
renewal, a re-report at launch, or an upgrade inside a granted month all add nothing.

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

**Returns go back where they came from.** (Decided 2026-09-18, built in
`core/src/subscription.rs` with twelve tests.) A device handing coins back (device move, giveback)
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

## What the app stores cost — computed 2026-09-18

Mobile first means Apple and Google are the main rail, not the exception, so the rates decide
the price. Assumptions stated once: German **VAT 19 %**, **1 EUR = $1.08**, `MARGIN=1.30`, and
a month's allowance **spent in full** (the pessimistic case, and the realistic one for anyone
who stays — see utilisation below).

**The store is the seller, so VAT comes off before the commission.** Apple and Google are
merchants of record in the EU: they charge the customer VAT, remit it, and pay out a share of
the *net*. The operator's Kleinunternehmer status does not help here — it only helps on the
web rail, and only until the turnover limit is crossed.

| rail | of €10 we receive |
| --- | --- |
| Stripe web, no VAT (today) | €9.60 |
| Stripe web, with VAT, EEA card | €8.03 |
| Stripe web, with VAT, non-EEA card | €7.88 |
| **Apple IAP, Small Business 15 %** | **€7.14** |
| Apple IAP, standard 30 % | €5.88 |
| **Google Play subscription, 10 % + 5 % billing** | **€7.14** |
| Google Play, 10 % with alternative billing + own PSP | €7.35 |

Google's rates changed on **30 June 2026** (Epic settlement): auto-renewing subscriptions are a
10 % service fee plus a 5 % billing fee when Play Billing is used — the same 15 % as Apple's
Small Business Program, and 10 % if billing is taken elsewhere. Apple's EU link-out is *not*
cheaper for an app this size: Store Services 5 % (Tier 1, which gives up featuring and
analytics) or 13 % (Tier 2) plus a 5 % Core Technology Commission plus 2 % for the first six
months, and our own PSP and VAT on top of that.

**1M TOKU costs €7.12 to serve** at `MARGIN=1.30` if it is spent in full ($10 retail ÷ 1.30,
converted). Against the table above that gives, per subscriber-month:

| margin | web (no VAT) | web (VAT) | Apple/Google 15 % | Apple 30 % |
| --- | --- | --- | --- | --- |
| **1.30** | +€2.48 | +€0.90 | **+€0.02** | −€1.24 |
| 1.45 | +€3.21 | +€1.64 | +€0.76 | −€0.50 |
| 1.60 | +€3.81 | +€2.24 | +€1.36 | +€0.10 |

**€10 through a store is exactly break-even** — €9.97 is the break-even price at 15 %, €12.11
at 30 %. That is the whole finding: at today's margin the store rails pay for the AI and
nothing else.

### The fix is the allowance, not the margin and not the price

Three dials reach the same margin, and they are not equally safe.

**Raising `MARGIN` is the one to avoid.** It changes what a TOKU buys — on every rail, and for
everyone who already holds one, including the customer with $9 of bought credit. The TOKU is
the denomination of the coins; it has to keep one meaning.

**Raising the price** works (€12.99 for a million) but spends a familiar price point and, if
the web stays cheaper, forks every figure in the app per platform.

**Sizing the allowance** does the same arithmetic with none of that: €10 stays €10 everywhere,
a TOKU keeps buying exactly what it buys today, and the month simply grants fewer of them.
Chosen 2026-09-18. €10 for 700,000 TOKU is arithmetically the same trade as €12.99 for a
million — *(cost(700k) = €4.99; €10 through a store nets €7.14)*:

| | €10 → 700k | €20 → 1.5M | €50 → 4M |
| --- | --- | --- | --- |
| TOKU per euro | 70,000 | 75,000 | 80,000 |
| what it saves against the entry rate | — | €1.43 | €7.14 |
| costs us, fully spent | €4.99 | €10.68 | €28.49 |
| profit, store at 15 % | **+€2.16** | **+€3.60** | **+€7.22** |
| profit, store at 30 % | +€0.90 | +€1.08 | +€0.92 |
| profit, web | +€3.02 | +€5.57 | +€12.53 |

The volume discount is deliberate and sits where it costs least — a fixed fee component is paid
once per charge, so one €50 subscriber beats five €10 ones — and it has to grow along the
ladder, or the saving shown on two tiers would be the same number twice.

**Read the 30 % row before widening it further.** Every TOKU given away at the top comes
straight out of the buffer against Apple's standard commission: at 80,000 TOKU per euro the
€50 tier yields €0.92 there, no better than the entry plan. That is survivable — the 30 % case
only arrives above $1M of proceeds, by which point the ladder can be repriced — but 85,000
would put the top tier under water while the small ones stayed healthy, which is the wrong way
round.

Two consequences for the UI: **never show a euro or dollar value for an allowance** (100,000
TOKU = $1 of list-price AI is true internally and would read as "€10 buys $7"), and the
per-tier answer/picture figures must be computed from `pricing.json` at build time.

### A limited company does not recover the VAT on a sale

Worth writing down because it is an easy and expensive thing to assume. Registering for VAT
(which a GmbH/UG does from the first euro) lets input tax be deducted on what the business
**buys**. It does not give back the tax on what the business **sells** — the €1.60 inside a
€10 subscription is collected for the state either way.

And the deduction has almost nothing to bite on here. The big cost is the AI providers, billed
from Ireland and the US: those are **reverse charge**, so the invoice carries no German VAT at
all — it is self-assessed and deducted in the same return, netting to zero. Stripe (Ireland)
likewise. What the App Store pays out is a B2B supply *to Apple*, with no VAT in either
direction. Only domestic purchases — hosting, hardware, the accountant — actually yield input
tax, and they are small next to the provider bill.

So the company form moves exactly one number, and downwards: the web rail goes from €9.60 per
€10 to €8.03, because VAT must now be remitted. The store rails do not move at all (€7.14
before and after — Apple and Google already deducted the consumer's VAT before paying out).

**The consequence for pricing: €10 on the web stops making sense.** It yields +€0.88 a month
while carrying the full dollar exposure. Either one price everywhere, or a modest web discount:

| | web | Apple/Google 15 % |
| --- | --- | --- |
| €11.99 | +€2.52 | +€1.44 |
| €12.99 | +€3.35 | +€2.16 |

**And the bottom line is after tax.** Corporation tax plus trade tax take roughly 30 % of the
profit: at €12.99 that is ~€1.51 per store subscriber-month and ~€2.34 per web one. Against a
GmbH's fixed costs — servers, developer programmes, and an accountant who now has a
Jahresabschluss to file — **the break-even is on the order of 150 paying subscribers**, not
fifteen. That number, not the per-subscriber margin, is the one to plan against.

*(Not tax advice: the reverse-charge treatment and Apple's commissionaire structure are for the
Steuerberater to confirm. The arithmetic above is what follows if they do.)*

### What moves the number

- **Utilisation.** At €12.99 and 15 %: +€2.16 if the month is spent in full, +€3.58 at 80 %,
  +€5.01 at 60 %. Do not plan on it. People self-select into the tier they exhaust, and the
  ones who exhaust it are the ones who renew — light users are upside, not budget.
- **FX.** Costs are billed in dollars, the price is in euro: at parity the same subscriber
  yields +€1.59 instead of +€2.16, at $0.95 +€1.18. Pricing in EUR removed a 3 % conversion
  spread per charge and replaced it with a slow exposure we carry.
- **Refunds.** A store refund claws the commission back, but the allowance was already drawn as
  blind coins and cannot be. One refunded month is a full loss of that month's provider cost.
  Bounded by design — one month, never a balance — which is exactly why this model is safer
  here than prepaid credit was.
- **The €30 tier is where the money is.** Fixed fee components are paid once per charge, so one
  €30 subscriber beats three €10 ones; the annual plan pays the fixed part once a year.

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
- ~~Apple~~ **Computed above.** Both stores take 15 % (Apple Small Business, Google's post-June
  2026 subscription rate), VAT comes off first, and €10 is break-even. The store price becomes
  €12.99 for the same allowance. What is still open is whether the iOS build shows the web price
  at all — anti-steering is relaxed in the EU and the US, not everywhere.
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
