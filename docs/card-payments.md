# Card payments via Mollie — integration check (2026-08-28)

Status: **built 2026-08-29 (server rail `CardRail::Mollie`, Tauri `checkout`, client card row + checkout panel, faucet `/paid`, admin column) — not yet deployed or tested against Mollie.** Decisions: card from $10 (`SCRAI_CARD_MIN_USD`), fees absorbed, hosted checkout only, card row visible on every platform (`CARD_ON_IOS` in index.html hides it on iOS in one place). Was: Mockups: https://claude.ai/code/artifact/429081cc-a951-4235-b2e6-ad0e33bb455f
(four screens: card row below the coins · card chosen · waiting for the browser checkout · clearnet return page).

Why Mollie: Dutch PSP (Amsterdam), EU-regulated, hosted checkout = zero card data on our side (PCI SAQ-A),
plain REST API with API-key auth, no SDK needed — one `reqwest` call to create a payment, one to poll it.
The other Dutch PSP, Adyen, targets enterprise volumes (contract-based onboarding) — not for a
prepaid-credit shop of this size.

## 1. Where it lands in the existing paywall

`server/src/pay.rs` already has exactly the shape a card rail needs:

```
RAISE  → Rail::create_invoice(usd, reference)  → RaisedInvoice { provider_ref, options, expires_at, … }
SETTLE → Rail::check_status(provider_ref)       → "paid" | "pending" | "expired"   (client polls every 10 s)
WITHDRAW → unchanged: entitlement → blind coconut ticketbook (link to the purchase dies here)
```

So the integration is a **third `Rail` variant next to `BtcPay`/`Fake`**, chosen per invoice by the client's
`method` ("card"), the same way `Gateway::create_invoice` already branches on `"nyx"`:

```rust
pub enum Rail { Fake, BtcPay {…}, None }           // today
pub enum CardRail { Mollie { api_key: String }, None } // new, orthogonal to the BTC rail
```

`Gateway` gets a `card: CardRail` field (like `nyx: Option<Nyx>`), `create_invoice` routes `wanted == "card"`
to it, `check_status` routes `inv.method == "card"`. `Inv` needs no new field — `provider_ref` holds the Mollie
payment id (`tr_…`), `method = "card"`.

**Polling instead of webhooks.** The VPS is reachable only over the mixnet (no clearnet port, see
[[scrambleai-federation-design]]), so Mollie's webhook cannot reach us. That is fine: the client already polls
`invoice.status` every 10 s over the mixnet, and each poll does `GET /v2/payments/{id}` from the VPS (outbound
clearnet is fine — BTCPay and Gemini are called the same way through `crate::http::client()`). `webhookUrl` is
optional in the Mollie API (see §4). The existing sweep (`PayPending::Sweep`) also re-checks pending invoices
on the next entitlement request, so a payment that finished while the app was closed still settles.

**What the client gets back.** `RaisedInvoice.options` for a card invoice is one entry
`{ "method": "card", "checkout": "<mollie _links.checkout.href>" }`; the Tauri `invoice` command already
carries a `checkout` field (`src-tauri/src/lib.rs` ≈ l. 869, currently always `""`) and `renderPay()` in
`public/index.html` already knows how to render a checkout button (the BTCPay dev shortcut) — that branch
becomes the real card pay panel from the mockup.

**Privacy of the reference.** Mollie gets `metadata.orderId = our_id` (the random invoice id the paywall already
generates via `rand_hex`, same as for BTCPay) and `description = "tokumai credit"`. It never sees the account
id, a session key or anything usage-related. Mollie *does* see: name, card number, IP, browser fingerprint,
e-mail if the checkout asks for it — that is what the "less private" tag and the note on the card-chosen screen
say. After WITHDRAW the coins are unlinkable, exactly as for a coin purchase.

## 2. The client

- **Buy dialog** (`#chooser` in `public/index.html`): a full-width `.tile.row` card row *below* the three coin
  tiles, tagged "less private"; `selectMethod("card")`; the Continue button reads "Continue · $5 by card ↗".
  Coins stay first and NYM stays the default — card is the fallback for people holding no NYM/BTC.
- **Continue** → `Backend.invoice(usd, "card")` → reply carries the checkout URL → `openLink(url)` →
  `open_external` opens the OS browser. The pay panel shows the "checkout opened in your browser" state and
  keeps polling; on `paid` it runs the normal `collect()` path (entitlement → withdraw), same as NYM/BTC.
- **iOS gap — must be fixed first.** `open_external` in `src-tauri/src/lib.rs` returns an error on iOS
  ("opening external links on iOS is not wired up yet"). Card checkout needs the system browser, so wire
  `tauri-plugin-opener` (or a small objc2 `UIApplication.openURL`, like the native image picker in
  [[scrambleai-ios-attach-picker]]). On macOS/Linux/Windows it works today.
- **Return page.** Mollie needs a `redirectUrl`. The app has no clearnet endpoint, so the redirect goes to a
  static thank-you page served by `scrai-faucet` (it already serves the site on
  `scrai-faucet.hermes-stakepool.de`): `GET /paid` → "Payment received — back to tokumai", no cookie, no
  order id, no JS. Optionally the button on that page is a `tokumai://paid` deep link so the app comes
  to the front (see §4 for whether Mollie accepts custom schemes as redirectUrl directly).
- **Pending list** (`renderPending`): `coinOf()` currently says "NYM" or "Bitcoin" — add "Card". Resume
  re-opens the checkout URL (Mollie keeps a payment `open` until it expires).

## 3. Money risks that coins do not have

1. **Chargebacks.** A card payment can be disputed for weeks; a coin payment cannot. Once the entitlement is
   withdrawn as blind ecash we cannot claw anything back (that unlinkability is the point). Mitigations, in
   order of cheapness: (a) a 3-D Secure *challenge* on every payment (opt out of Mollie's Dynamic 3DS, §4 —
   only a challenged payment shifts fraud liability to the issuer); (b) cap card purchases at the small
   tiles ($1/$2/$5 — the dialog already sells fixed amounts only); (c) rate-limit card invoices per account harder than coins
   (`INVOICE_PER_ACCT` is 5/10 min today); (d) accept the residual loss as a cost line — at $5 tiles it is
   bounded. Do **not** delay withdrawal for card-funded entitlements: it breaks the UX and re-links usage to
   the purchase window.
2. **Fees.** Card fees are per-transaction with a fixed part (see §4), so a $1 tile loses a large share to
   fees. Either absorb (same tiles for every rail, simplest and keeps "everyone buys the same") or show
   "+ fee" on the card row. Open decision — noted on the canvas.
3. **Refunds.** Mollie supports refunds via API; we cannot verify that credit is unspent, so the policy is
   "no refunds after checkout" (state it on the return page / terms).
4. **VAT / invoicing.** A card payment makes the buyer's country visible to the PSP; selling to EU consumers
   means VAT at the buyer's rate (OSS). This applies to coin sales too, but cards make it auditable — a
   question for the accountant before going live, not a code question.

## 4. Mollie facts (from docs.mollie.com / help.mollie.com, fetched 2026-08-28)

**API — what the rail calls**
- `POST https://api.mollie.com/v2/payments`, header `Authorization: Bearer live_…` (or `test_…`), body
  `{ "amount": {"currency":"USD","value":"5.00"}, "description":"tokumai credit", "redirectUrl": …,
  "method":"creditcard", "metadata": {"orderId": our_id}, "locale":"en_US" }`. Add `Idempotency-Key: <uuid4>`
  (cached 1 h) so a mixnet retry of `invoice.create` cannot raise two payments.
  https://docs.mollie.com/reference/create-payment · https://docs.mollie.com/reference/api-idempotency
- `webhookUrl` is **optional** ("without a webhook you will miss out on important status changes" — we poll
  instead). Confirmed for our no-inbound-port setup.
- Response: `id` (`tr_…`), `status`, `expiresAt`, `_links.checkout.href` → the URL the app opens.
- Poll: `GET /v2/payments/{id}`. Rate limit for that endpoint ≈ 20 req/s sustained, burst 60
  (`429` + `Retry-After` beyond that). Our 10 s client poll per invoice is nowhere near it.
  https://docs.mollie.com/reference/rate-limiting
- Statuses: `open` → (`pending`) → `paid` | `canceled` | `expired` | `failed`. Only **`paid`** settles
  (`authorized` is capture-flow only, not used). Card payments expire after ~30 min (docs say 30 min in one
  place, 15 in another) — read `expiresAt`, don't predict; the app's countdown uses that field already.
  https://docs.mollie.com/docs/handling-payment-status
- No official Rust crate (community `mollie-rs` exists) — plain `reqwest`, same as the BTCPay rail.

**redirectUrl / mobile**
- Custom URL schemes are accepted as `redirectUrl` ("Mollie's API accepts custom URL schemes"), and Mollie
  recommends opening the checkout in the **default browser, not a WebView** — which is what `open_external`
  does. So `tokumai://paid` would work on phones once the scheme is registered; on desktop a plain
  `https://scrai-faucet…/paid` page is the safer default (no scheme registration on Linux/Windows).
  https://docs.mollie.com/docs/integrating-mollie-in-your-mobile-app
- **App Store caveat (same page):** Mollie points at Apple's DMA / Google Play billing constraints for
  digital goods. Credit consumed inside an iOS app bought outside In-App Purchase is exactly what Apple's
  guideline 3.1.1 forbids outside the EU alternative terms — a card button makes that visible to review in a
  way a NYM address does not. Decide before shipping the card row in the iOS build.

**Money**
- USD is supported for cards; a non-primary-currency payout is converted at **1 %**. Keeping the tiles in USD
  (SCRAI_PER_USD is fixed) costs that 1 % if the Mollie balance is EUR; charging EUR would need a rate.
  https://docs.mollie.com/docs/multicurrency
- Card fees (mollie.com/pricing): EU consumer Visa/MC **1.80 % + €0.25**, EU commercial 2.90 % + €0.25,
  non-EU 3.25 % + €0.25, Amex 2.90 % + €0.25, Apple/Google Pay = rate of the underlying card. No monthly fee.
  On our tiles (EU consumer card): **$1 → ≈ $0.31 fee (31 %)**, $2 → ≈ $0.33 (16 %), $5 → ≈ $0.38 (7.6 %).
  ⇒ card makes sense from the $5 tile up; either disable $1/$2 for card or add a "+ fee" line.
- Chargeback fee for cards is not published (help centre only says "a chargeback cost will be deducted").
- 3-D Secure: Mollie runs 3DS 2 for all cards; **Dynamic 3DS is the default below €100 and may skip the
  challenge** — liability then stays with the merchant (No-3DS → merchant, frictionless → merchant,
  challenged → issuer). Ask Mollie support to **opt out of Dynamic 3DS** so every payment is challenged and
  fraud disputes shift to the issuer. https://help.mollie.com/hc/en-us/articles/15903942112274

**Onboarding**
- KYC: legal entity, UBOs, ID of legal reps, bank account, website. Germany: Freiberufler → Steuerbescheid,
  Gewerbe → Gewerbeschein. https://help.mollie.com/hc/en-us/articles/115000481665
- The website must show prices, T&Cs, trade name + registration/VAT number, registered address, phone or
  live contact, e-mail/contact form, privacy policy, currency, payment terms. The download/faucet site has
  none of that yet — it needs an imprint + terms page before the card application.
  https://help.mollie.com/hc/en-us/articles/211745545
- **Card acceptance is a separate activation**, reviewed in 5–7 days, stricter because of chargeback risk.
  https://help.mollie.com/hc/en-us/articles/115000920589
- Prohibited list names high-risk financial activities, piracy services, hacking/cracking tools — **no
  mention of crypto, prepaid credits/tokens, AI or anonymity services**, but "not exhaustive, assessed
  individually". A mixnet-anonymous AI service may still draw questions; describe it as "prepaid AI
  assistant credit" and have the terms page ready. https://help.mollie.com/hc/en-us/articles/115000939369

**Test mode**
- Test API key; the hosted test checkout lets you pick the resulting status; test cards Visa
  4543 4740 0224 9996, MC 2223 0000 1047 9399 (any expiry/CVV). **Testing is EUR-only** — so the test rail
  must send EUR (`MOLLIE_API_KEY_TESTNET` + currency switch), live sends USD. No account approval needed to
  test. https://docs.mollie.com/docs/testing

**PCI**
- Hosted checkout = no card data ever reaches us (Mollie is a PCI Level 1 service provider). Embedded fields
  (Mollie Components) exist and are SAQ-A too, but bring card fields into the webview — for us that is
  exactly the XSS-to-card surface the security audit warned about for the seed; stay hosted.
  https://docs.mollie.com/docs/hosted-checkout

## 5. Build order (when decided)

1. iOS `open_external` (blocker for the card flow on iPhone).
2. `CardRail::Mollie` in `pay.rs` (create + status), env `MOLLIE_API_KEY` (+ `_TESTNET` for the test key,
   via `crate::net_var` like BTCPay), `MOLLIE_REDIRECT_URL`.
3. `/paid` page in `scrai-faucet`.
4. Client: card row, `selectMethod("card")`, checkout pay panel, pending "Card".
5. Test with the Mollie test key end-to-end over the mixnet; then the live key needs the Mollie account
   approved (business registration + website with terms/privacy/contact).
6. `scrai-admin`: rail column so card revenue and disputes are visible separately.
