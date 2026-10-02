# Blind tokens: taking the purchase out of the book

Design note, 2026-09-28; built 2026-10-01 (enclave `notes.rs`, shared `core::notes`, the
wallet in the app). An addition to the enclave as it stands (`docs/enclave-phase0.md`),
not a rebuild.

**Tech stack, as built.** RSA-2048 blind signatures after RFC 9474 (RSABSSA-SHA384-PSS,
deterministic), written out on the `rsa` crate's integers in `crates/core/src/notes.rs`:
EMSA-PSS encoding with an empty salt, blind = m·rᵉ, sign = zᵈ, unblind = s'·r⁻¹, verify.
Month keys derived from the data key through HKDF-SHA256 into a seeded ChaCha20 generator,
so nothing is stored and every start of an image that can open the sealed secrets makes
the same keys; their SPKI forms travel in every attestation and a SHA-256 digest of them
is in the binding (`attest::binding_with`). A note is 36 bytes: version, tier, month,
nonce; nonce and blinding factor come from HKDF of the account's seed and the month, so a
restored phone makes the same blinded message and the enclave re-signs it. The book keeps
`minted` (payment-and-month → fingerprint of the blinded message) and the spent nonces in
`payments`; both under keyed hashes. Operations `note.mint` (under a throwaway key) and
`note.redeem` (signed by the account); the plan row's rail becomes `note:<own name>`, which
the renewal check skips. The app reads the App Store's signed transaction to learn the paid
months, mints each open one once, and redeems at once when the account has nothing to chat
on, else at a seed-derived moment inside the seven-day grace. Card plans on the web still
take the old path until the checkout is moved onto notes.

## The problem it addresses

Today a payment lands on an account directly. `iap_verify` and the Stripe paths call
`credit_prepaid` or `subscribe_or_renew` for the account that presented the proof, and
`rail_bind` writes the payment reference against the account key. The reference is a
keyed hash, the book is sealed, and the enclave never hands the account to a provider.
Still, the link **exists as a row**: whoever can open the book — the enclave itself, a
future image the key policy is changed to admit (`deploy/aws/kms.sh`, "whoever can change
the policy"), or anyone who copies a snapshot and later obtains the key — reads buyer →
account → every request that account signed. The operator holds the buyer's name through
Stripe and Apple, so the chain is complete for anyone who can compel the operator and open
the book.

The `cover` module addresses a different link: the *timing* between a payment and the
first question, as a provider or the host might see it. It does not touch the row.

## The idea

A payment no longer credits an account. It buys **tokens**: fixed-value notes signed
blindly by the enclave, so the enclave signs without seeing what it signs
(Chaum; RSA blind signatures as in RFC 9474, publicly verifiable). The app unblinds them
and, later, an account redeems them. The enclave sees a valid signature of its own and a
nonce it has not seen before. It cannot tell which purchase the note came from, because
it never saw the note at minting — only its blinded form.

What the book then holds: `payments` (transaction id, hashed — minted or not, as today's
`first_payment`), `spent` (epoch, nonce), and lots and allowances per account. **No row
joins a payment to an account.** The join can only be made by watching the timing, at the
time, from outside — and that is a link that can be stretched.

## Shape

### The note

```
m = version(1) | kind(1) | epoch(u16) | nonce(32)
```

- `kind`: `plan-0/1/2` — one paid month of that tier. Only months: tokumai sells
  subscriptions, not one-time credit, so there is nothing else to mint. (The tier must
  be visible at redemption because it decides the allowance — see "what remains".)
- `epoch`: the calendar month the note pays for.
- Validity: a note redeems in its epoch or the month after.
- **One note at a time.** A yearly plan is not twelve notes minted at once: the app
  presents the same yearly proof each month and asks for month k, and the enclave
  deduplicates by (transaction, month). The wallet never holds more than the current
  note, which is what bounds every loss below to one month.
- `nonce` and the blinding factor are **derived from the account seed and the epoch**
  (`HKDF(seed, "tokumai/note/v1", epoch)`), never drawn at random. A restored phone
  regenerates the identical blinded message — the reason is under "device loss".

### Keys

One RSA-2048 signing key per epoch, **derived** from the data key
(`HKDF(data_key, "tokumai/mint/v1", epoch)` seeding the keygen) so that every start of
the same image and every image that can open the sealed secrets produces the same keys:
nothing to store, nothing to migrate. Keys for epochs k−1 … k+1 are made at start.

The **public** keys go into the attestation binding, next to the identity and exchange
keys. That is the transparency guarantee: every app sees the same keys, so an enclave
cannot tag one buyer's notes with a key of their own. The app checks each unblinded
signature against the published key of that epoch before it keeps the note.

### Minting: `mint`

A sealed request, but **not signed by the account** — a fresh ed25519 key for this one
request, thrown away afterwards. `account_owns` accepts any key; nothing else in the
request path needs the account. (A request signed by the account would put the link back
in one line.)

Body: the proof of payment and the blinded messages.

| rail | proof | what the enclave checks | note minted |
|---|---|---|---|
| App Store plan (monthly) | the JWS of the period's transaction | `verify_jws` (offline, no network), `plan_for`, dedup by (transaction, month) | one, epoch = the period |
| App Store plan (yearly) | the yearly JWS, and which month is asked for | same, and that the month lies inside the paid year | one, epoch = that month |
| Stripe plan | subscription id | `subscription()` paid for the period, dedup by (subscription, month) | one |

The enclave signs the blinded message with the epoch key and answers with the signature.
What it writes: the `payments` row keyed by (transaction, month), and **the hash of the
blinded message it signed**. A second `mint` for the same (transaction, month) is answered
by signing the same blinded message again — the signature is identical, so this is not a
second note, and the spent list catches any replay. The stored hash links a transaction to
a blinded blob, which by construction says nothing about the unblinded note or the account
that will redeem it. The reply is padded to a fixed size, so a mint answer is not
recognisable by its length from the host.

Renewals: Apple charges in the background; the app learns of the new period from
`currentEntitlements` at launch (already implemented) and mints for it. Stripe rolls the
period; the app asks to mint for the new period start. `check_apple` and the App Store
Server API stay for what they are needed for now: refusing to mint against a refunded or
revoked transaction.

### Redeeming: `redeem`

Signed by the real account, as any request. Body: notes `(m, s)`. The enclave verifies
`s` under the epoch key of `m`, checks the epoch window, checks `(epoch, nonce)` is not in
`spent`, writes it there (journaled, like every change), and sets the allowance for that
epoch's window (`grant_allowance`, as `subscribe_or_renew` does now). A resend of a spent note is a
no-op with the same answer, which is what `first_payment` gives a resent purchase today.

`spent` for epoch k is dropped once epoch k+1 has ended: the table stays the size of two
months of purchases.

### The app

- A **wallet** in the profile, stored as the mnemonic is stored, holding the current
  note. Notes are bearer secrets: whoever holds one can redeem it.
- A **scheduler** that redeems with jitter rather than at once (below).
- The plan meter needs no server row any more: the app knows what it bought and when;
  the allowance's `ends_ms` says the rest.

### Migration

Accounts that already hold lots, allowances and plans keep them; nothing is converted.
New periods mint. `rail_bind`, `plans`, `check_stripe` and `check_apple` stay until the
last account-bound subscription has lapsed, then go. `iap_verify` becomes `mint`, and
`credit_prepaid` with the lots goes when the last prepaid lot has expired.

## What it mitigates, exactly

| link | today | with notes |
|---|---|---|
| payment → account, in the book | a row (`rails`, `payments` + the credited lot) | none: the enclave never sees the note it signs |
| payment → account, for a compelled operator | operator's Stripe/Apple records + a way into the book | nothing to hand over: the book has no join, and the operator has no logs that pair a moment with a request (by design already) |
| payment → account, for AWS with a copied book + key | the row | no row; only timing, if they recorded it at the time |
| payment → first question, timing at the provider | `cover` decoys | unchanged, `cover` stays |
| payment → first question, timing at the host | Sphinx timing; the enclave's replies are not padded | the redemption can be **moved in time**; a question cannot |

The last line is the whole difference between redeeming and asking: a note is a tiny
message nobody is waiting for, so the app may send it hours or days after the purchase.
A question is asked when the person wants it asked.

## What remains

1. **The hypervisor.** AWS runs the machine under the enclave. Nothing here changes what
   AWS could read from enclave memory; that is the Nitro trust assumption, stated as such.
2. **The tier.** A note says its tier at redemption, so the set it hides in is the
   subscribers of that tier renewing in that window, not all subscribers.
3. **Timing, for the immediate redeemer.** See the next section.
4. **Refunds after redemption.** A note already redeemed cannot be clawed back — the
   enclave does not know which account holds the allowance. Mitigations: mint only once
   the payment is final enough (Stripe `paid`, Apple JWS not revoked); one note per
   (transaction, month) is inherent; the residual loss is one month of one tier.
5. **Device loss — solved, with a condition.** The mnemonic restores the account and
   with it everything already redeemed. A note minted but not yet redeemed is
   regenerated on the new phone (the nonce and blinding factor come from the seed) and
   re-signed by the enclave against the same stored blinded hash. Apple hands the current
   transaction to any device on the same Apple ID, so this is automatic; a Stripe
   subscriber needs their subscription id, which the operator can look up from the
   receipt — the operator already knows who paid, and the mint request is not signed by
   the account, so the lookup reveals nothing new.
6. **The mint burst.** Mint reply, redeem, first question in close succession is a
   recognisable shape on the host's side if the enclave's replies are not padded. Padding
   the mint and redeem replies to fixed sizes removes the size; the timing is what
   section "The immediate redeemer" is about.

## How many users make the crowd

The only link left is timing: a mint at T, a redemption at T + d. If the app draws d
uniformly from a window W, an observer who sees both moments cannot tell this redemption
from any other whose mint lies within W before it. With λ mints per day of the same note
kind, the expected size of that set is

```
k ≈ λ · W
```

Some points on that line, asking for k ≥ 20 (an arbitrary but common bar):

| note kind | window W | needed λ | which is about |
|---|---|---|---|
| renewal, one tier | 3 days (the grace) | 7 / day | 200 subscribers of that tier |
| renewal, one tier | 1 day | 20 / day | 600 of that tier |
| renewal, one tier | 7 days | 3 / day | 90 of that tier |
| **first month, immediate** (W ≈ 5 min) | 5 min | 5 800 / day | never, at any scale we are planning for |

Two consequences. Renewals are the easy case: they are periodic, nobody is waiting, and a
grace on the previous allowance (the app already says "plan ended, lasts until …") gives
the window for free — a seven-day grace makes 90 subscribers per tier enough. And the
first month is not protected by the crowd at all, at any realistic scale — which is the
honest statement to make, and the reason it is treated separately.

## The first purchase, and the immediate redeemer

A person who has just taken out a plan wants to ask now. Delaying their first month is
not an option, and pretending the timing link is gone for them would be false. What is
true:

- **The row is still gone.** Even for them, nothing in the book joins the payment to the
  account. What remains for them is a timing correlation that someone must observe as it
  happens: the operator or Stripe or Apple knows the purchase time; the host sees a reply
  leave the enclave a minute later; a provider sees a question from a pseudonym a minute
  after that. Joining buyer to *content* needs the purchase time **and** the host's
  timing **and** the provider's record — three parties, one of them outside AWS and
  outside the operator. Today it needs the purchase records and one look into the book.
- **App Store purchases leave no mark on the host at all.** `verify_jws` is offline; the
  enclave makes no call when an Apple note is minted. Stripe calls go out every 30 s
  regardless (the steady beat in `tick`), so a Stripe mint is not a visible event either.
  The host sees only Sphinx packets, whose sender it cannot name.
- **The app's side is already covered.** The app pads its sending (Poisson stream, loop
  cover — rule in `mix.rs`), so the entry gateway cannot see a purchase burst from the
  user's side. The exposed side is the enclave's replies, and padding the two new reply
  kinds to fixed sizes takes the shape away.

So the policy in the app:

1. **First month**: mint and redeem at once — the person can ask. This one redemption
   is exposed to timing, and it is the only one that ever is.
2. **Renewals**, monthly or the months of a yearly plan: mint when the period's proof
   appears, redeem at a random moment inside the grace after the period boundary. The
   old allowance covers the gap.
3. A note about to leave its window is redeemed now, jitter or not.

## Cost of building it

- Enclave: two operations (`mint`, `redeem`), epoch key derivation, two small tables
  (`spent`; the blinded hash per (transaction, month)), reply padding for the two kinds,
  keys in the attestation binding. The `rsa` crate is
  already in the tree; blind RSA per RFC 9474 is a few hundred lines on top of it or a
  small crate. `plan_summary` and the renewal checks slim down.
- App: the wallet in the profile, the scheduler, the meter drawn from local knowledge,
  and the throwaway key for `mint`.
- Tests: mint and redeem end to end with the simulated enclave; double spend refused;
  a note under the wrong epoch key refused; a resend answered identically; a second
  mint for the same (transaction, month) yields the same signature and no second note.
- Docs: `privacy-notes.md` ("what we can see") changes, and the Nym pitch can then say
  that the enclave holds no record joining a payment to an account — which is a sentence
  the current design cannot say.

## A plan the book forgot (2026-10-02)

The book is the only record that a note was spent. A book restored from a backup may not
have that record — and then not the plan either. The app keeps every redeemed note for the
month it bought (`profile.spent`), and when the enclave reports no plan while that month is
still running, presents the note again (`reclaim_plan`, on the heartbeat and after a sync,
at most every ten minutes). The enclave honours an unspent note once and answers a spent one
with "again", so nothing is gained where the plan is still there, and a lost one comes back
without anyone at support learning who asked. Test: `a_spent_note_buys_its_month_again_only_where_the_book_forgot_it`.
