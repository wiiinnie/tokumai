# Before the public launch — things that MUST be true

Not a roadmap. Each line is something a published page or a legal duty already relies on,
so launching without it makes that page untrue or that duty unmet. Started 2026-09-21;
add to it whenever a text promises something a setting has to deliver.

## Stripe (dashboard and keys)

- [x] ~~Cancellation e-mails ON~~ — **there is no such setting.** Billing → Subscriptions and
      emails offers trial, renewal, expiring-card and failed-payment mails, nothing for a
      cancelled subscription (checked 2026-09-21). So the confirmation in text form is the
      page's own, saved by the customer; `/cancel`, terms § 3a and the privacy notice say so.
- [ ] Billing → Subscriptions and emails, as decided 2026-09-21: renewal, expiring-card and
      failed-payment mails ON; "Stripe-hosted link to confirm payments when required" ON
      (SCA: a renewal a bank wants re-authenticated is otherwise simply lost); 3D Secure by
      Radar rules ON; all retries failed → cancel the subscription.
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

- [ ] § 312k BGB (4): the confirmation must reach the customer "in text form". Ours is a
      page they can save, and it is deliberately CONDITIONAL ("if a plan is paid with this
      address…") so the button cannot be used to find out who subscribes. Is that enough?
      If not, the options are (a) drop the neutrality and confirm the concrete plan and end
      date on the page, (b) send the customer to Stripe's customer portal, whose e-mailed
      login link proves the address is theirs, (c) an outbound mail service, which the
      server deliberately does not have. (Comment in `server/site/cancel.html`.)
- [ ] Terms § 4 (withdrawal for a running digital service) and § 6a (coin validity) — both
      carry a LAWYER CHECK comment in the HTML.

## App Store

- [ ] `IAP_ALLOW_SANDBOX=0` again once the review is through: while it is on, a sandbox
      receipt is a free month on the production server.
- [ ] `SCRAI_MIN_APP` only after every platform's build is really out.

## Site and server

- [ ] `PURCHASE_TIERS=none` in `/opt/tokumai/.env`: one-off credit is retired, and until this
      is set the server still raises a $10 invoice for anyone who asks (an older app build,
      or `/pay` on the site). The terms no longer describe such a purchase.

- [ ] The deployed site carries the terms and privacy notice of 21 September 2026 (the plan
      sheet in the app links straight to them).
