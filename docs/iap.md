# App Store purchases (iOS)

How credit is bought on iPhone, why it is shaped this way, and what to set up to test it.

## The shape

```
iPhone                                  server (mixnet)                    Apple
──────                                  ───────────────                    ─────
tap a tile ── StoreKit 2 purchase() ─────────────────────────────────────▶ sheet, payment
◀── Transaction (JWS: signed by Apple, x5c chain) ─────────────────────────
iap.verify {jws, account sig} ────────▶ verify chain to Apple Root CA G3
                                        (pinned), ES256, bundle, tier,
                                        consumable, unrevoked
                                        claim fingerprint (SQL, once)
                                        credit entitlement (snapshot)
◀── iap.ok {toku, entitlement} ─────────
Transaction.finish() ────────────────────────────────────────────────────▶ acknowledged
collect → withdraw → coins → trickle    (the same path as every other purchase)
```

- **Verification is local.** The server never talks to Apple: the JWS carries Apple's
  certificate chain, the root is pinned in the binary (`server/src/apple_root_ca_g3.der`,
  SHA-256 `63:34:3A:BF:…:3E:91:79`). No App Store Server API key, nothing to rotate.
- **Finish only after credit.** The transaction stays unfinished on the phone until the
  server has answered `iap.ok`. A lost reply, a crash, a dead route: the purchase is still
  there, and `iap_restore` re-sends every unfinished transaction on launch and behind
  "Restore purchases". The server answers a retry by the same account with `toku: 0` —
  credited already, nothing more to add.
- **Same buckets as every other rail.** Products are `com.tokumai.app.credit.<tile>`; the
  credit is the tile's TOKU (`$10` = 1,000,000). The App Store *price* is higher (1.4× the
  tile — Apple's commission and VAT are inside it) but the server only ever sees the
  bucket, so an iPhone buyer is not recognisable by amount. The $5 tile is not on iOS.
- **What is stored.** `iap_transactions`: a SHA-256 fingerprint of Apple's transaction id
  (the once-only guard), product, TOKU, environment, storefront country, the account (for
  `ACCOUNT_LINK_DAYS`, then NULL — same clock as vouchers and invoices), timestamps.
  Neither side learns the other's identity: Apple does not see the tokumai account, the
  server does not see the Apple ID.
- **Crash window.** Claim (SQL) first, credit (snapshot) second — the same argument as
  vouchers (`docs/vouchers.md`). A claimed-but-uncredited row is repaired at boot and on
  the watch tick by `credit_pending_vouchers`.

## Server configuration

| variable | default | meaning |
|---|---|---|
| `IAP_PRODUCT_PREFIX` | `com.tokumai.app.credit.` | product id = prefix + tile |
| `IAP_TIERS` | `10,20,50` | tiles sold through the App Store; must be purchase tiers |
| `IAP_BUNDLE_ID` | `com.tokumai.app` | the transaction must name this bundle |
| `IAP_ALLOW_SANDBOX` | `0` | `1` credits Sandbox transactions — testing only, never in production |

The catalog reply carries `iapProducts` (the ids, in tile order); the app asks StoreKit for
exactly those. The boot log prints the products and whether sandbox is accepted.

## App Store Connect (done 2026-09-10)

- Consumables `com.tokumai.app.credit.10` / `.20` / `.50`, reference names "Credit N",
  display name "N,000,000 TOKU", description "Prepaid credit for anonymous AI chat",
  base price USD 13.99 / 27.99 / 69.99, all territories. Saved, not yet submitted — the
  first IAP goes to review with the first build that uses it.
- Small Business Program enrollment submitted (15 % from the month after approval).
- Still needed before a sale: Paid Applications agreement + banking + tax forms
  (Agreements, Tax, and Banking). StoreKit returns no products at all until the
  agreement is active — also in the sandbox.

## Testing on a device

1. App Store Connect → Users and Access → Sandbox → Test Accounts: create a tester
   (a fresh Apple ID that does not exist yet; country Germany to see EU prices).
2. On the iPhone: Settings → App Store → Sandbox Account → sign in with it.
3. Server `.env`: `IAP_ALLOW_SANDBOX=1`, restart. Boot log must say `sandbox transactions
   ACCEPTED`.
4. In the app: Buy credit → tile → tick the phrase sentence → Buy. Apple's sandbox sheet
   shows `[Environment: Sandbox]`. After the sheet: "✓ 1,000,000 TOKU credited", then the
   balance updates through the usual collect.
5. Lost-reply drill: switch the server off, buy, see "Paid — but the server has not
   confirmed…", switch it on, tap "Restore purchases" (or relaunch the app).

## Apple refunds

Apple refunds App Store purchases itself; we are not asked. A refunded transaction
carries `revocationDate` and is refused if re-sent, but credit already granted is not
clawed back — the coins are unlinkable by then, as with every other rail. Server-to-server
notifications (a clearnet webhook) would tell us about refunds; not built, and the
mixnet-only server would need the faucet host to receive them.

## Files

- `src-tauri/gen/apple/Sources/scrambleai/StoreKitShim.swift` — StoreKit 2, four
  `@_cdecl` functions
- `src-tauri/src/iap_ios.rs` — the Rust side of the bridge (oneshot + timeout)
- `src-tauri/src/lib.rs` — `iap_products`, `iap_purchase`, `iap_restore`; `IAP_PRODUCTS`
  from the catalog; non-iOS stubs
- `server/src/iap.rs` — JWS verification, product mapping, credit decision
- `server/src/store.rs` — `iap_transactions`
- `public/index.html` — the iOS buy sheet (`renderIosStore`)
