# Before the public launch — things that MUST be true

Not a roadmap. Each line is something a published page or a legal duty already relies on,
so launching without it makes that page untrue or that duty unmet. Started 2026-09-21;
add to it whenever a text promises something a setting has to deliver.

## Stripe (dashboard and keys)

- [ ] **Cancellation e-mails are ON.** Billing → Settings → customer e-mails: send an e-mail
      when a subscription is cancelled. The cancellation button (`/cancel`, § 312k BGB)
      answers the same way whether or not a plan exists — on purpose, so it cannot be used
      to find out who subscribes — which means **Stripe's e-mail is the confirmation in text
      form** the law asks for. Without this setting a customer who cancels gets nothing in
      writing. `server/site/cancel.html`, `server/site/terms.html` § 3a and
      `server/site/privacy.html` § 4 all say Stripe confirms.
- [ ] **The key can do what the button needs.** `STRIPE_SECRET_KEY` (restricted key):
      Customers *read*, Subscriptions *read + write*, Checkout Sessions *write*. With a key
      that cannot list customers the button takes requests and ends nothing — the server
      logs "could not be acted on yet" every two minutes and drops the request after a day.
- [ ] **The rail is configured at all.** The production boot log of 2026-09-19 says
      `plans on the web — no card rail configured`: `STRIPE_SECRET_KEY_MAINNET` and the six
      `STRIPE_PRICES_MAINNET` ids are missing. Until they are there, the terms describe a
      way of buying that does not exist.
- [ ] Try the button once against the sandbox with a real test subscription: request →
      `cancellation request acted on — 1 plan(s)` in the journal → `cancel_at_period_end`
      visible in the dashboard → the e-mail arrives.

## Lawyer

- [ ] § 312k BGB: is a button on our site that ends the plan at Stripe, with Stripe sending
      the confirmation, enough? (Comment in `server/site/cancel.html`.)
- [ ] Terms § 4 (withdrawal for a running digital service) and § 6a (coin validity) — both
      carry a LAWYER CHECK comment in the HTML.

## App Store

- [ ] `IAP_ALLOW_SANDBOX=0` again once the review is through: while it is on, a sandbox
      receipt is a free month on the production server.
- [ ] `SCRAI_MIN_APP` only after every platform's build is really out.

## Site and server

- [ ] The deployed site carries the terms and privacy notice of 21 September 2026 (the plan
      sheet in the app links straight to them).
