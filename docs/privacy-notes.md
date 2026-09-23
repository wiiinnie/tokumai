# What we can see, and what we cannot

Notes for the privacy policy and terms, written before a lawyer sees them. Everything here
is meant to be said out loud in the published text: a promise that turns out to have had an
exception is worse than a narrower promise kept.

The rule we hold ourselves to: **say what we can see, not what we intend to do with it.**
"We do not log" is a statement about our conduct. "We cannot read it" is a statement about
the system, and only the second is worth anything to somebody who does not know us.

## Who sees what

| Who | What they see |
| --- | --- |
| The person's ISP | A connection to one Nym gateway, in constant cover traffic. Not that tokumai exists. |
| Their entry gateway | A client sending Sphinx packets. Never one of ours, by rule (`client::gateways`). |
| Mix nodes | Packets, one hop's worth of routing. |
| Our own gateways (the enclave's front doors) | Timing and volume of what arrives. No sender, no content. |
| Our host, and whoever holds it (including AWS) | Ciphertext in and out, and the fact that a model provider was called — with the size of the answer, which says whether it was a picture. Not the question, not the account, not the balance. |
| The enclave | Everything, for as long as it takes to answer: the question, the account, the balance. It keeps the question nowhere. |
| The model provider | The question and the answer, from our egress, with a per-account-per-day safety identifier and no user identity. |
| Stripe / Apple | Who paid, when, and how much. Never what was asked. |

## What we must disclose, because it is true

1. **The model provider reads the questions.** Frontier models are not ours to run. Google
   and OpenAI see what is asked, without a name attached, and OpenAI also sees everything
   through the moderation check that runs before each answer.
2. **The first question after a first purchase can be linked to the payer.** We hold the
   merchant records — who paid, and when — and our host sees when a call to a provider goes
   out. While the service is small, those two facts join up: we would learn that a named
   person asked *something* at a given moment, and whether it was a picture. Not what they
   asked. This is the sharpest edge in the system and it dulls as more people use it; the
   mitigation (cover from other users' traffic, and a decoy when there is none) is described
   in the pitch notes.
3. **A payment is linked to an account inside the enclave.** It has to be, to grant the
   allowance. We cannot read it; a compelled or subverted enclave could. The first version
   of this product used blind ecash so the link could not exist at all — that was given up
   for a balance that does not expire, and it is a real trade.
4. **The root of trust is Amazon.** The proof that the published code is what runs ends at
   an AWS certificate, and the secrets are released by AWS KMS. A compelled or compromised
   AWS breaks the guarantee, and no check in the app would notice.
5. **The book can be rewound by whoever holds the machine.** Balances live in a sealed file
   on the host; an older copy could be put back. Spent credit would return. Nothing inside
   the enclave survives a restart to notice. (On the list to close before launch.)
6. **Recovery phrase, not account.** There is no email, no password and no way for us to
   help: a lost phrase is a lost balance, and we must say so in plain words rather than in a
   clause.

## What the published text must not claim

- Not "anonymous" without saying to whom. Anonymous to us and to the network; not to the
  model provider, and not to the payment provider.
- Not "we don't log" as the main promise. The main promise is that the interesting parts
  are in a machine we cannot look inside, and that the app checks which code it runs before
  sending anything.
- Not "zero knowledge" or "end-to-end encrypted" for the chat itself. The enclave decrypts
  the question — that is the whole point of attesting it.
- No claim about traffic analysis. The mixnet raises its cost; the enclave does nothing for
  it at all.

## Housekeeping that the text has to match

- Every model provider has to appear in the published text **before** it serves a single
  request. (Rule carried over from the first version, where it was broken once.)
- Egress logs on the host are operational only, and should be kept for the shortest window
  that still allows an outage to be diagnosed — they carry the timing that item 2 is about.
- The pricing table is compiled into the enclave image and therefore attested: we cannot
  reprice a question after the fact, and the text may say so.
