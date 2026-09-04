# tokumai — Roadmap

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
+ bank (name, IP, 3-D-Secure); being a tokumai customer is visible on the statement;
chargebacks/fraud pressure the unlinkability; EU SCA is mandatory. None of this is fixable
from our side — only disclosed. Privacy-max users stay on crypto (and can use a virtual
disposable card).

### Mainnet crypto rails — *planned*
Flip the payment layer from **testnet** NYM/BTC to **mainnet** for real inbound revenue.
Independent of the Gemini-key mainnet switch (already done). See
`[[scrambleai-payment-architecture]]`.

## Operating model — *decided 2026-09-03*

### One operator, many servers — no third-party federation
Every scrai-server, every Coconut authority and the shared ledger are run by **us**. The
earlier plan of a "trustless federation" with foreign operators (shared t-of-n key across
operators, bearer ecash valid at anyone's server, clearing between operators) is
**dropped for regulatory, not technical, reasons**: the moment customer money collected by
the authority is passed on to other operators (settlement) — or operators clear balances
through us — that is a payment service / e-money business (PSD2/EMD2, ZAG in Germany).
That needs a BaFin licence, and a licensee is an obliged entity under the AML rules
(GwG/AMLR), i.e. it must **identify its customers**. Anonymous prepaid is then only
allowed up to €150 / €50 remote, which ends the product. Today's model — a merchant
selling its own prepaid service credit, accepting crypto or card — needs **no KYC** and
no licence; see `docs/federation-shared-ledger.md` (header) for the full reasoning.

Consequences:
- **Keep:** the signed server directory + load-based selection in the client, K Nym
  identities per server, and — later — stateless fronts in front of one core (see
  "Target topology" below). The shared ledger across cores and a t-of-n DKG are deferred:
  there is one core for now.
- **Drop:** operator clearing, per-operator price lists, foreign-authority verification,
  the "franchise"/settlement designs. A lot of code that never needs to exist.
- **How other node operators still participate:** on the Nym layer, not the money layer.
  They run the **entry gateways the scrai-servers hang off** (`SCRAI_GATEWAY_MASTER` /
  `SCRAI_GATEWAY_FALLBACK`, today de01/at01/ch01) and are compensated through Nym's own
  bandwidth economy — free in the current mode, later via paid bandwidth credentials
  (zk-nym). No money flows from us to them and no customer money flows through them, so
  nobody becomes a payment intermediary and nobody sees more than a Nym client's
  gateway sees.
- **What we give up:** trust minimisation towards the operator. Users still have to trust
  that our servers do not log; the mixnet hides who they are, not what they ask. That is
  exactly what the TEE endpoint (below) is for, and it gets easier with a fleet we own,
  because attestation only has to cover our own machines.

## Security / decentralisation

### One authority for launch — *decided 2026-09-04*
`AUTHORITY_N = 1` stays. A single issuing authority harms nobody but us as long as the box is
honest: forged coins would only be redeemed against our own provider bills, and blind
signatures are as unlinkable with n = 1 as with n = 3. The one real hole is **key tagging**:
the client fetches the authority's verification key from the same server that issues the
book (per 5-dollar ticketbook, cached until two days before the key expires), so a
malicious server could sign each user's book under a different key and link the spends.
That is fixed by publishing the key, not by adding shares. Before real money:

1. **Publish + pin the authority key** — on tokumai.com and inside the signed
   directory/price list; the client verifies the served key against it (see "signed
   pricing" in `docs/pricing-safety.md`).
2. **Budget caps + alarm** — hard daily spend limits at every model provider, and an
   admin alarm when minted coins and booked payments drift apart. Bounds a compromise to
   one day.
3. **Rehearse a key rotation** — clients already discard books minted under a foreign key
   and re-draw from entitlement; run it once on testnet, write down the steps.
4. Rename the `SCRAI_ALLOW_SINGLE_AUTHORITY` override into a plain launch setting, so the
   code says "decided", not "testnet leftover".

**t-of-n (e.g. 2-of-3) is deferred** to the moment a second core exists anyway. What it
would take: the crypto core already does threshold signing (tested 2-of-2); the server
would need a real DKG (today's keygen is a single dealer) and share-aware issuance; the
client would need to collect partial signatures from t servers. Three layers, the server
the largest.

### Target topology — *decided 2026-09-04*
Three roles, one operator:

- **tokumai-client** — the app (desktop + mobile). Talks only through the mixnet, picks a
  server from the signed directory, holds the recovery phrase, the vault and the coconut
  books.
- **tokumai-core** — the ONE box that holds state and secrets: the authority, the
  entitlement ledger, session balances, the double-spend list, payment wallets and chain
  watchers, provider API keys, pricing. Today's `scrai-server` **is** the core.
- **tokumai-server** — stateless fronts, added when one core's CPU is saturated by the
  cover traffic of its Nym identities. A front runs Nym identities, unwraps the Sphinx
  layer, forwards the request to the core and answers over the SURBs. No ledger, no key,
  no decision; a captured front can neither mint nor rebook anything. Fronts reach the
  core over a **private link between our own machines** (WireGuard: silent to strangers,
  so the core keeps having no public port) — the first deliberate exception to "our
  servers talk only over the mixnet". Optional: an inner encryption layer to the core's key
  so fronts see ciphertext only. The client finds fronts through the signed directory and
  fails over between them.

Not before launch: a single server already carries K Nym identities (one identity ≈ 40
concurrent users in the load test). Fronts become worthwhile only once that box is CPU-bound;
re-run `scripts/loadtest.sh` first. The core stays the single point for ledger and
authority — by choice.

## Models / providers

### TEE inference endpoint — *considering*
Close the one gap Tinfoil-style services can point at: today the tokumai server (and
the upstream provider) sees prompt **plaintext** — unlinkable to a person, but readable.
Run an **open-weights model inside a confidential-computing enclave** (NVIDIA
Hopper/Blackwell CC, remote attestation) reached **through the mixnet**, so both threat
models hold at once: *nobody knows who you are* (mixnet + blind ecash) **and** *nobody —
us included — can read the prompt* (TEE). That union is something neither Tinfoil
(account + card + IP visible) nor a plain mixnet proxy can offer alone.

**Options:** (a) use **Tinfoil's OpenAI-compatible private inference API** as just another
upstream behind the mixnet — fastest, but adds their availability + pricing as a
dependency; (b) **self-host** a GPU-TEE box (H100/H200 CC) with our own attested stack —
more control, real hardware + ops cost. Either way the **client must verify the
attestation itself** (over the mixnet); a server-side "trust us, it's attested" claim
would be worthless.

**Caveats:** open-weights models only (Gemini/Claude can't run in our enclave — offer as a
**"🔒 sealed"** tier beside the frontier tier, mirroring the "🌐 web — less private"
labelling); adds a hardware trust anchor (NVIDIA/Intel attestation chain) we otherwise
avoid; enclave GPU capacity is priced well above plain inference.

### Live / web-grounded models — *planned*
Users want current-internet answers, not just static LLMs. Enable **Gemini Grounding with
Google Search** on the existing integration (fastest), and optionally add **Perplexity
Sonar** / **OpenAI GPT web_search** as a "live" tier. Flag these models **"🌐 web — less
private"**: the search query (derived from the prompt) reaches a search backend at the
provider (user IP still hidden by the mixnet, but the topic leaks).

### OpenAI — *built (2026-09-03), not yet live*
Responses-API adapter (`server/src/openai.rs`): GPT-5.4 nano/mini/full, images + PDFs in,
web search per call, reasoning effort from the app slider, prepaid-only, own concurrency
pool, `store: false`. Anonymous-user safeguards: per-day safety identifier, moderation
prefilter (`MODERATION_PREFILTER`), session strikes (`ABUSE_STRIKES_PER_DAY`). Go-live:
`OPENAI_API_KEY` + `openai` in `SCRAI_PROVIDERS` on the MAINNET server only, account at
tier ≥ 2, monthly budget set, then the cost reconciliation against the OpenAI dashboard as
done for Gemini. Details: `docs/providers-openai.md`, `docs/abuse-policy.md`.

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

### iOS top-up by storefront — *built (2026-09-01), EU entitlement pending*
The buy sheet on iOS now follows the device's App Store storefront (StoreKit, read in
Rust): **US** shows the top-up ID + a Safari link (allowed since the 2025 Epic order),
**EU** is meant to link out under Apple's DMA terms, **everywhere else** shows the ID only
(3.1.1). The EU path needs the **External Purchase Link entitlement**
(`com.apple.developer.storekit.external-purchase-link` + `SKExternalPurchaseLink` URLs in
Info.plist), requested from Apple per app; until it is in the provisioning profile the EU
falls back to ID-only (`EU_LINK_ENTITLED` in index.html). No storefront → ID only.

### Native iOS document picker — *planned*
Photos/camera now use a native picker (`pick_image`). "Choose File" (PDF/text) still falls
back to the flaky web `<input type=file>` — replace with a native `UIDocumentPickerViewController`.
See `[[scrambleai-ios-attach-picker]]`.

### Gemini prepaid safety before live — *planned*
Prepaid credit + auto-recharge (unlocks after spend history). Before pointing real users at
paid models: Cloud Billing budget alert on low balance + a reload process, so the API never
runs dry mid-service.

## Capacity / load distribution — *load-test harness built (2026-09-02)*
`scrai-loadtest` + `scripts/loadtest.sh` measure one server's latency curve vs concurrent
users (ping / models / full signed chat with fake payments + mock provider). Next: run the
stages, find the knee, then either **multi-address server** (K Nym identities, one state —
if ingress saturates first) or **tokumai-server fronts** in front of the one core behind the
**signed directory** (if CPU saturates first; see "Target topology"). Server selection must
never be a user task: pong now carries `load`, the client picks + sticks (session balance
and coconut books belong to the one core, so fronts are interchangeable). All of these
servers are ours (see "Operating model").
Details: `docs/load-testing.md`.

## Ops / metrics — *done (2026-08-25)*
scrai-admin now shows **chat revenue** (retail users chatted), **provider cost** (raw price
we paid Google, no margin), and **profit** (revenue − cost), alongside purchases.
