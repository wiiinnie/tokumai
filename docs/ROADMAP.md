# ScrambleAI — Roadmap

Living list of planned work. Newest thinking at the top of each section.

## Payments

### Card payments — Mollie (EU) — *planned*
Add credit-card top-ups as an **optional** convenience tier alongside crypto, using
**Mollie** (Amsterdam) — EU data residency, GDPR DPA, PCI-DSS L1, PSD2/SCA, dev-friendly
hosted checkout, cards + SEPA + iDEAL. Chosen over Stripe (US) and Adyen (enterprise);
Merchant-of-Record options (Paddle/Lemon Squeezy) rejected — they see full buyer PII.

**Design (keep the two layers separate):** the card buys **blind-ecash vouchers**; redemption
stays unlinkable — so the card only reveals *"bought $X of credit"*, never what the user
chats. Use Mollie **hosted checkout** (no PCI on us), require **zero account/email**, and
label the option clearly as **"less private than crypto."**

**Privacy caveats to surface in the UI:** the buyer is identified to Mollie + card networks
+ bank (name, IP, 3-D-Secure); being a ScrambleAI customer is visible on the statement;
chargebacks/fraud pressure the unlinkability; EU SCA is mandatory. None of this is fixable
from our side — only disclosed. Privacy-max users stay on crypto (and can use a virtual
disposable card).

### Mainnet crypto rails — *planned*
Flip the payment layer from **testnet** NYM/BTC to **mainnet** for real inbound revenue.
Independent of the Gemini-key mainnet switch (already done). See
`[[scrambleai-payment-architecture]]`.

## Security / decentralisation (mainnet blockers)

### t-of-n authority DKG — *required before real-money mainnet*
Today `AUTHORITY_N = 1` and the server runs with `SCRAI_ALLOW_SINGLE_AUTHORITY=1`
(testnet override). A single 1-of-1 authority can **forge unlimited ecash** and weakens
unlinkability. Stand up a real **threshold (t-of-n) DKG with ≥2 independent authorities**
and drop the override before issuing real-money credentials.

## Models / providers

### Live / web-grounded models — *planned*
Users want current-internet answers, not just static LLMs. Enable **Gemini Grounding with
Google Search** on the existing integration (fastest), and optionally add **Perplexity
Sonar** / **OpenAI GPT web_search** as a "live" tier. Flag these models **"🌐 web — less
private"**: the search query (derived from the prompt) reaches a search backend at the
provider (user IP still hidden by the mixnet, but the topic leaks).

### More providers for breadth — *considering*
Add one developer-first multi-model provider for model variety + a metadata API:
**OpenRouter** (huge breadth, pricing/context per model), or EU-hosted **Mistral La
Plateforme / Scaleway / OVHcloud** for data residency. **Not Cloudflare Workers AI** — its
self-serve *and* enterprise terms forbid reselling to third parties, and its open models
have no built-in web grounding.

### Text-to-image — *done (2026-08-25)*
Google Nano Banana (`gemini-2.5-flash-image`, `gemini-3.1-flash-image[-lite]`) wired into
the catalog, token-billed, real verified prices. Marketing names shown in the picker.

## App

### Native iOS document picker — *planned*
Photos/camera now use a native picker (`pick_image`). "Choose File" (PDF/text) still falls
back to the flaky web `<input type=file>` — replace with a native `UIDocumentPickerViewController`.
See `[[scrambleai-ios-attach-picker]]`.

### Gemini prepaid safety before live — *planned*
Prepaid credit + auto-recharge (unlocks after spend history). Before pointing real users at
paid models: Cloud Billing budget alert on low balance + a reload process, so the API never
runs dry mid-service.

## Ops / metrics — *done (2026-08-25)*
scrai-admin now shows **chat revenue** (retail users chatted), **provider cost** (raw price
we paid Google, no margin), and **profit** (revenue − cost), alongside purchases.
