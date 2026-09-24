# What happens when a question is refused

Notes for the terms and conditions, written before a lawyer sees them, in the same spirit as
`privacy-notes.md`: say what the system does, not what we intend to do. Everything below is
meant to be published, because a rule nobody was told about is not a rule, it is a surprise.

The section is short on purpose. A person who has just had a question refused wants three
answers — what happened, what it cost, what happens next — and they want them in the app,
not in a PDF.

## What is checked, and by whom

Every question passes one filter before it is answered, and which one depends on the model:

| Model | Filter | Thresholds |
| --- | --- | --- |
| OpenAI (`gpt-…`) | OpenAI's moderation endpoint, run by the enclave before the model is asked | OpenAI's, as published by them |
| Google (`gemini-…`) | Google's own safety filters, switched on by us in the request | `BLOCK_ONLY_HIGH`, except sexually explicit at `BLOCK_MEDIUM_AND_ABOVE` |

Each provider filters its own traffic and no third party reads it. Before 2026-09-24 a
question sent to a Google model was also sent to OpenAI to be checked; that is over.

The thresholds are compiled into the enclave image (`policy::GEMINI_SAFETY`), so they are
covered by the attestation the app verifies. We cannot quietly raise them for one person,
or lower them, without publishing a new measurement. That is worth saying in the text: it
is a promise that can be checked rather than believed.

## What it costs, and what follows

1. **A declined question costs nothing.** No tokens are charged, and on the OpenAI path the
   model is never asked at all.
2. **Three declines at one provider in one day close that provider for that account** until
   the day turns over (UTC — the app shows the local time). The counter is per account, per
   provider, per day.
3. **The other provider stays open.** A lockout is never a lockout from the service.
4. **From the second decline of the day the answer says so**, with the count and the
   consequence. The first carries its reason alone: a single mis-fire should read as an
   accident, not as a warning.

## What we do not do

- **We do not ban anyone permanently.** There is no such mechanism, and there is nothing in
  the book that could carry one: the strike counter lives in the enclave's memory and is
  gone when it restarts.
- **We cannot tell anyone who you are**, because we do not know. A provider's abuse report
  names a per-day pseudonym, and the salt behind it is not kept.
- **We do not report anyone to anybody.** What a provider does under its own terms is
  between the provider and its own rules; those terms are linked in the app before a model
  is used for the first time.

## Questions for the lawyer

1. **Is a day's lockout at one provider a reduction of the service a subscriber paid for?**
   The other models stay open, and the declines themselves cost nothing. Does it need a
   pro-rata anything, or a right to complain to a human?
2. **May we reserve the right to end a plan for serious or repeated abuse?** We would like
   the clause even though the mechanism does not exist — but we do not want to claim a power
   we cannot exercise. Is a reserved right that is technically unavailable a problem in
   itself?
3. **Must the categories be named** (self-harm, hate speech, …) or is "declined by the
   safety check" enough? We prefer naming them: being refused without being told why is the
   thing people hate most about these products.
4. **Does §312k or the withdrawal right touch any of this** when a subscriber hits a lockout
   in their first fourteen days?

## Housekeeping

- The thresholds in the published text must match `policy.rs`. If one moves, both move, in
  the same commit.
- The strike counter is in memory only. If it ever moves into the sealed book, this page and
  `privacy-notes.md` both change: a per-account counter that survives a restart is a record
  of conduct, and it would have to be disclosed as one.

## Release notes: what must be undone before mainnet

One entry so far, and it is not a nicety.

**The App Store sandbox image must lose its access to the key.** For testing purchases from
a real phone, the probe's KMS key currently allows two measurements: the production image
and one built with `--features nitro,apple-sandbox`, which accepts Apple's *sandbox*
receipts. A sandbox receipt costs nothing, so an image that honours one can mint balance.
Building that acceptance into a separate image is exactly what keeps a production image
honest — and it only works while the sandbox image is not allowed to open the same book.
Before any key guards real money: `deploy/aws/kms.sh policy` must show exactly one
measurement, the production one. See docs/enclave-phase0.md, "Before launch", item 5.
