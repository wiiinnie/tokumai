# Architecture

Decided on 2026-09-22. The full reasoning, with diagrams and the variants that were weighed,
is the page "Der Enclave-Kern" (kept privately by the operator). This is the summary the code
follows.

## The promise, and what enforces it

| Who | sees the question | knows who asked |
|---|---|---|
| network, ISP | no | no (mixnet) |
| the operator (us) | no (enclave) | no (enclave) |
| the model provider | yes | no (our key) |
| Stripe / Apple | no | who pays, not what they ask |

## The path of a question

1. The app attests the enclave. It sends a fresh nonce; the enclave answers with its keys
   and a platform proof over `binding(identity, kx, nonce)`. The app accepts only a
   published measurement.
2. The app signs the request with the account key over
   `nonce:ts:<enclave identity>:<sha256 body>` and seals it to the enclave's X25519 key.
3. The enclave opens it, checks the signature, the clock and the nonce, **holds** the worst
   case on the account, asks the provider, and **settles** at what the answer cost.
4. The answer goes back sealed to that request. If it is lost, the app sends the same
   bytes again: same answer, charged once.

## Money

- **No coins, no money on the device.** The balance is on the account, in the enclave, and
  recoverable from the phrase.
- **Two pockets.** The plan's allowance for its current period (set, never added, and
  gone when the period ends) is spent first. Then prepaid lots, soonest-expiring first, each
  valid three years from purchase.
- Plans run in their own months from the day of purchase, on Stripe and the App Store
  alike (`core::subscription`).
- **Plans are checked from inside.** The enclave itself asks Stripe and Apple about each
  plan about every six hours: a renewal extends it, a full refund or chargeback ends it at
  once (and a chargeback also stops the card subscription). Which account a Stripe
  subscription or App Store transaction belongs to is stored only under a keyed hash
  (`enclave::subscriptions`).

## What the operator can still do

- Run, stop and update the machine. An update only counts once its measurement is
  published, and the app accepts it only then.
- See totals: revenue, cost, per day and per model.
- Answer support for one account, only with that account's own signed permission (to
  come).
- See the provider-bound traffic leave: that a request went out, when, and how large. The
  one signal left, weak, and it fades with every other user.
