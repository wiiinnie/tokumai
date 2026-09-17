# Off Mollie, onto Stripe — and coins off the shelf

Written 2026-09-16, **nothing built**. Two changes that arrived together and must not be
shipped in the wrong order.

## What happened

- Mollie ended the account: the business is too risky for them. The card rail is therefore
  **dead**, not "to be replaced at leisure".
- Crypto purchases (NYM, Bitcoin, Monero) cannot be represented for tax at the moment, so
  they come off the shelf too.

## The trap in doing both

Today a buyer has four rails: `nyx`, `btc`, `card`, `invite` (`pay::rails_info`). Take the
card away and switch the coins off and **nothing is left** except an invite code — and, on
iOS, the App Store. A desktop or Android visitor would see a shop that cannot sell.

So the order is: **Stripe first, coins off second.** If the tax question is urgent enough
that the coins must go today, they can go by env alone (see below) — but then the site has
to say plainly that card payment is coming back, rather than showing dead tiles.

## Part 1 — Stripe

### Why the work is small

`CardRail` is already the seam: an enum in `server/src/pay.rs` with two methods,
`create_invoice` and `check_status`, and everything else talks to it through
`card_enabled()` / `card_info()`. Nothing outside knows what a Mollie is, except copy.

### Checkout Session, not Payment Link

Use Stripe Checkout — but a **session created per order**, not a static Payment Link:

| | Checkout Session | Payment Link |
| --- | --- | --- |
| amount | per order | fixed per link (one per tile) |
| our order id | `client_reference_id` + `metadata` | not reliably bindable |
| settlement check | look the session up by id, compare amount + reference | you match on a webhook and hope |
| expiry | `expires_at`, like Mollie's | none |
| idempotency | `Idempotency-Key`, as today | none |

The second column is what we already do and what `settlement_matches` is built on. A link
is a page; a session is an order — for money the difference is the whole point.

**No webhook.** The server has no inbound port by design (mixnet only) and polls the
provider every 10 s. Stripe's `GET /v1/checkout/sessions/{id}` supports exactly that, so
the rail swap changes no architecture.

### What actually changes

| Where | Work |
| --- | --- |
| `pay.rs` · `CardRail` | `Mollie{api_key, redirect_url}` → `Stripe{secret_key, redirect_url}`; `name()`; `card_enabled()` |
| `pay.rs` · `create_invoice` | `POST /v1/checkout/sessions`, **form-encoded** (Stripe is not JSON), Bearer auth, `line_items[0][price_data]`, `client_reference_id`, `metadata[orderId]`, `success_url`, `expires_at` |
| `pay.rs` · `check_status` | `GET /v1/checkout/sessions/{id}` → `payment_status == "paid"`; `expired`/`open` map to our `expired`/`pending` |
| `pay.rs` · `settlement_matches` | same rule, Stripe's shape: `amount_total` is **integer cents**, `currency` lowercase, reference from `client_reference_id` |
| `pay.rs` · `is_mollie_url` | → `is_stripe_url`: only `checkout.stripe.com` may be handed to the client to open |
| `pay.rs` · `mollie()` helper | → `stripe()`: form body, error shape `{"error":{"message":…}}`, same 429 = "nothing new yet" |
| `pay.rs` · `mollie_methods()` | Stripe has no `/methods` list to mirror. Drop the dynamic label and say "Card" — it was a nicety, and it cost a cached HTTP call on every catalogue fetch |
| `.env` | `STRIPE_SECRET_KEY_MAINNET` / `_TESTNET`, `STRIPE_REDIRECT_URL`, same `net_var` pattern |
| copy | `public/index.html` (22 mentions), `server/site/pay.html` (11), `index.html` (2), `admin.html` |
| **legal** | `privacy.html` (4) and `terms.html` (1) name the payment processor. **Stripe must be named there before it takes the first payment** — the same standing rule as for a model provider |
| tests | `settlement_needs_our_reference_and_our_amount` and friends are the template; rewrite for Stripe's shape |

**Estimate:** the adapter and its tests are half a day. The copy sweep is mechanical but
touches ~40 places. Call it a day with verification.

### Order of work

1. Adapter behind `CardRail`, with `sk_test_`. Nothing user-visible changes.
2. One end-to-end test purchase in test mode: raise, pay, settle, credit.
3. Copy sweep, privacy and terms naming Stripe.
4. Live key, one real sale, and **measure what it actually costs** — fee and FX spread —
   the way the first Mollie sale was measured (see the `MARGIN` comment in `.env.example`).
   Stripe's EU pricing is not Mollie's, and the break-even margin moves with it.

### Worth saying out loud

Stripe runs the same kind of risk review Mollie did, and can reach the same conclusion. The
seam is now proven to be cheap to swap, which is the real insurance; a third rail would be
another day's work, not a rewrite.

## Part 2 — coins off the shelf

`rails_info()` reads `nyx` from `Nyx::from_env()` and `btc` from `BTCPAY_*`. Unsetting
those makes both flags false **immediately, without a deploy** — that is the emergency
lever if the tax question cannot wait.

But false flags alone do not give a clean shop. The buy sheet greys a dead rail rather than
hiding it — deliberately ("a rail the server cannot raise an invoice on stays visible so
the choice is legible") — and with both coin rails off it still:

- auto-selects `nyx` when `btc` is off (`selectMethodQuiet("nyx")`), i.e. lands on a dead rail;
- labels the small tiles "NYM only";
- says "Coin payments are not available on this server right now — pay with NYM."

So the real work is in the client: when no coin rail is live, the coin group goes away
instead of greying, the tile sublabels lose their "NYM only", and the fallback selects the
card. Plus the site: `server/site/index.html` carries 17 NYM, 3 Bitcoin and 1 Monero
mentions, and terms and privacy describe coin payment as a thing we do.

iOS needs nothing: it sells through the App Store only.

**Estimate:** half a day, most of it copy.

## What I would do

Build Part 1 through step 2 (test mode, proven end to end) before touching anything the
user sees. Then do Part 2's client and copy work and Part 1's step 3 in one release, so the
shop never shows a rail it cannot serve. If the tax question cannot wait that long, pull
the coin env vars today and put one honest line on the buy sheet.
