# Phase 0: the enclave on AWS Nitro

Decided on 2026-09-22: **AWS Nitro Enclaves** (eu-central-1). The deciding point: the app
checks the proof offline, against one pinned AWS root certificate, and PCR0 is exactly our
image. Google Confidential Space was the alternative (normal network and disk, but the
proof is a token from Google's own service, whose keys rotate) and is not pursued.

A Nitro enclave has no network and no disk of its own. Both have to be built:

- **Network through vsock.** Every connection out — the Nym gateway, OpenAI, Gemini, Stripe,
  Apple, the Nym API — goes through a proxy on the parent instance
  ([Enclaver](https://github.com/edgebitio/enclaver): egress allowlist, ingress none). TLS
  still ends inside the enclave; the proxy sees encrypted bytes.
- **Storage sealed by KMS.** The ledger lives on the parent's disk, encrypted; its key, the
  providers' API keys and the enclave's Nym identity keys are released by AWS KMS only to
  an enclave whose attestation shows the published PCR0 (`seal::KeyProvider`).

## Built so far

- `attest::nitro::verify_document`: the COSE_Sign1 document, ES384, the chain from the
  pinned AWS Nitro Root G1 (fingerprint checked in the tests) to the signing certificate,
  the signature, PCR0 and the binding in `user_data`. A debug enclave (PCRs all zeros) is
  refused. Tested against a genuine AWS document (published by Evervault, Apache-2.0) and
  with every byte of it flipped in turn.
- `attest::nitro::NitroAttester` (feature `nsm`, Linux): asks /dev/nsm for a document over
  the binding. Not compiled on macOS; checked once Docker is there.

- **The way out** (`crates/egress`, `vendor/nym-gateway-client`): inside the enclave a
  loopback listener pipes every connection over vsock to `tokumai-egress-host` on the host,
  an HTTP CONNECT proxy for the destinations in `deploy/egress.allow` only. The model
  providers, Stripe, Apple and the Nym API use it through `HTTPS_PROXY`; the Nym gateway
  connection through a small patch (`TOKUMAI_EGRESS_PROXY`), since the SDK opens that one
  itself. Names are resolved outside; TLS ends inside.
- **Verified without an instance** (`deploy/sim-enclave.sh`): the simulated enclave in a
  container with no internet, the proxy in another. It came onto the mixnet under its
  usual address, was attested and answered a chat from the Mac; everything it reached went
  through the proxy (Nym API, its gateway, Stripe), and what is not on the list was refused.

- **The image** (`deploy/enclave/`): `tokumai-enclave-nitro` in a distroless runtime, the
  EIF built with nitro-cli 1.5.0 in a pinned Amazon Linux container (no EC2 host needed).
  **Reproducible:** two builds, one without any cache, give the same PCR0. The binary was
  bit-identical from the start; the image's file dates made PCR2 differ until every
  timestamp was set to `SOURCE_DATE_EPOCH` (an OCI archive with rewritten timestamps,
  loaded — the docker exporter cannot rewrite while it unpacks).
  Probe 1: PCR0 `6872f9b709fd3b5b7639a3e3535600c3cf7d41810c742f0657fb7a9fecf82aacd597512551b25d378f7e981893558d51`.
- **The host side** (`deploy/aws/probe.sh`): one `c7g.xlarge` (Graviton, enclaves
  enabled, the role `tokumai-enclave-host`), SSH from the operator's IP only; the egress
  proxy (a static binary) and the enclave started on it. Its words are EC2's own —
  `launch`, `start`, `stop`, `terminate` — because `stop` keeps the disk (and the book on
  it) while `terminate` destroys both.

## The first probe, 2026-09-22

Instance `c7g.xlarge` in eu-central-1, the enclave with 2 vCPUs and 3 GiB. It worked on
the first run:

- the enclave boots, draws its randomness from the Nitro module, comes onto the mixnet
  through the pinned gateway and announces its address to the host over vsock (a
  production enclave has no console);
- its gateway is one of the operator's own (DE01), as the first server had it — its own
  front door should be a machine we keep running. The app's entry gateway is the
  opposite: never one of ours (rule A1). The gateway is part of what is measured, so
  moving it means a new image; its siblings AT01, CH01 and DE02 are on the allowlist
  already. PCR0 with DE01: `7543861de5e98486b05276e4a5a4909eda4abb5d7998a04c15149ffe300e4b2202d6017aaac24c4edbae2c132ff7c2ca`.
- everything it reaches goes through the proxy: `validator.nymtech.net` and its gateway,
  and nothing else;
- from the Mac, over the mixnet: **attested as `AwsNitro image 6872f9b7…`**, test credit,
  a chat and the balance. Connect and attest together 5.6–5.8 s, a chat 1.5 s, the
  balance 1.1–3.5 s;
- a PCR0 that differs by one character is refused, naming the image actually found;
- the desktop app talks to it the same way (`TOKUMAI_ENCLAVE`, `TOKUMAI_PCR0`).

Noted on the way:
- `nitro-cli run-enclave` needs the egress proxy already running, or the enclave waits
  (it retries; `connect_at_boot` gives it five minutes).
- The Nym client panics in `packet_router` when a process exits — on the app's side, at
  the very end. To be looked at before a release.

## The second probe, 2026-09-22: the secrets

The enclave now runs on real keys, released by AWS KMS to this image and nothing else.

- **The key** (`deploy/aws/kms.sh`): its policy allows `kms:Decrypt` only for a request
  carrying a Nitro attestation whose PCR0 is the published image, and only for the
  instance role. The operator keeps managing the key and `kms:Encrypt` (to seal new
  secrets), not opening them. Honest about the limit: whoever can change the policy could
  grant themselves decryption later — the way out is signed images (PCR8) instead of a
  list of PCR0s, still on the list for before launch.
- **What is sealed** (`kms.sh secrets`): the data key, the OpenAI and Gemini keys, the
  Stripe keys, and the enclave's Nym identity — about 78 KB, so they travel under a fresh
  ChaCha20-Poly1305 key of their own and only that key goes to KMS (which encrypts at most
  4 KiB). The host keeps the sealed file and cannot read a byte of it.
- **How it gets in**: the host answers two questions on a vsock channel of its own
  (`ask_host`): the sealed file, and the instance's temporary credentials — which open
  nothing on their own. The enclave asks KMS itself, over the egress proxy, with a signed
  request it builds by hand (`enclave::kms`: no AWS SDK, which would bring its own network
  stack). KMS answers not with the key but with a copy encrypted to a public key inside
  the attestation, whose private half exists only in that enclave, for that one request.
- **Its address survives.** The sealed Nym identity is laid out at every start, so the
  enclave comes back as `nbnWr8Cu…` after a restart instead of as a stranger.
- Verified from the Mac over the mixnet: attested `AwsNitro image 5ef2c8c4…`, real
  provider keys, Stripe plans on offer (`byCard: true`), and `dev.credit` refused — a
  sealed enclave hands out no test credit.

Three things had to be fixed, none of them visible from outside, which is why the enclave
now says over vsock whether it unsealed and what it measures:

1. the key had been sealed as its hex **text**, so the enclave got 64 bytes where it
   wanted 32 (`kms.sh secrets` now does the whole thing in one step);
2. the host's answer, a hundred kilobytes, could end in a reset that read as a clean close
   — the answer now carries its length;
3. KMS replies in BER with lengths left open, which a DER reader refuses; the envelope is
   read by hand now (`cms_parts`, tested against both forms and against content in pieces).

~~Still in memory: the ledger.~~ Done — see below.

## The book, 2026-09-22

The enclave keeps its book in memory and the host keeps a sealed snapshot and a sealed
journal of it, one record per change. (Until 2026-10-02 every record was written before the
person was told their request went through, under the one lock every request takes; now
the record is queued and a writer thread carries it, and only a purchase, a note or a plan
waits for the host's confirmation before it is answered — see below.) At the next start
the snapshot is read back and the journal replayed; every
record is sealed to its place in the line, so one dropped from the middle, reordered, or
kept from an older snapshot does not open. Verified on the probe: 22 changes replayed
across a restart onto a NEW image, which is also the upgrade path.

Two things it does not survive, both about where the bytes lie rather than how they are
sealed, and both on the list below: a host that is replaced, and a host that hands back an
older pair of files.

## The book written behind, the enclave kept running, 2026-10-02

Three of the things that would have ended real operation, closed together:

1. **Book writes off the request path.** `ledger::Kept` hands every sealed record to a
   writer thread of its own; a chat's hold and settle never wait for the host's disk. What
   grants credit (`iap.verify`, `note.mint`, `note.redeem`, `plan.status`, `plan.change`)
   waits for its mark (`Enclave::durably`, 45 s) so the app's own record — a transaction
   finished with Apple, a note spent — is never ahead of the book. The writer never gives
   up on a record; the host log hears about a refusal once a minute. Every record carries
   its number in the clear, and the host takes a record it already has as said rather than
   written again (`tokumai_egress::add_record`): before, a confirmation lost on the vsock
   put the same change on the disk twice and the next start refused the book. Nonces are
   no longer written down at all — a request is sealed to a key that does not survive a
   restart, so no replay crosses one — which took two fsyncs off every chat. A journal in
   the old form is read and folded into a snapshot at the first start on the new image.
2. **The enclave runs under systemd** (`tokumai-enclave.service`, `probe.sh deploy`):
   when it exits — the watchdog's exit 70, a panic, a reboot — the host starts it again
   within ten seconds, and keeps trying. Until then a dead enclave stayed dead until a
   person noticed.
3. **The book on a volume of its own** (`probe.sh volume`): `tokumai-book`, encrypted,
   attached after launch so that EC2 does not delete it with the instance; `launch` starts
   a new instance beside it. `probe.sh backups` snapshots it daily, fourteen kept. Item 1
   of the list below, closed.

Also that day: the alarm (`probe.sh alarm <email>` — the host reports the pulse's age to
CloudWatch every minute, a mail when it passes five minutes or stops arriving) and the
witness of item 2 below.

What a restart still loses: requests in flight (their holds come back), the 30-minute
reply cache, the strike counter, the cover queue. A planned upgrade is a stop and a start,
a few minutes without doors; a drain that finishes in-flight requests first is next.

## Next

1. **Docker** on this Mac: the enclave image is built in a Linux container, reproducibly
   (pinned toolchain and base image, `SOURCE_DATE_EPOCH`), so anyone can rebuild it and get
   the same PCR0.
2. **The image**: `tokumai-enclave` (the core plus its own Nym client) packed with Enclaver;
   the egress allowlist.
3. **An AWS account** (Paid plan; see below), a budget alarm, an IAM user with EC2 + KMS in
   eu-central-1, the `aws` CLI configured locally.
4. ~~**The probe**~~ — done, see above. (An `m6i.xlarge` with enclaves enabled; the image started; the dev client
   and the app attest it over the mixnet; 24 h under light load — Nym stable through the
   proxy? latency?)
5. ~~**KMS**~~ — done, see above.
6. ~~**The ledger**~~ — done, see above.

## Before launch

Preconditions, not nice-to-haves. Each is a way the enclave's promise is weaker than it
looks, and each is cheap to close now and expensive to explain later.

1. ~~**The book must outlive the machine.**~~ Closed 2026-10-02 (`probe.sh volume`,
   `probe.sh backups`, see above). It lay on the instance's root volume, so replacing the
   instance took every balance and plan with it — which is exactly what happened on the
   morning of 2026-09-23, when a terminated probe took a paid test plan with it. Now on a
   volume of its own that survives the instance, snapshotted daily. The runbook still says
   `stop`, never `terminate`, out of habit; `terminate` keeps the volume.
2. ~~**A rewind must be detectable.**~~ Built 2026-10-02 (`crates/enclave/src/witness.rs`),
   live once the bucket exists (docs/aws-admin.md). The host can hand back an older
   snapshot and journal, and nothing inside a Nitro enclave survives a restart to notice —
   so the enclave tells a witness outside: one object per mark in an S3 bucket with Object
   Lock in compliance mode, after every fold and every ten minutes while the book moves.
   At start it refuses a book standing before the newest mark. A restore we mean is
   acknowledged by the operator's own identity (`probe.sh accept-rewind`), which the
   host's role may not write, and the acknowledgement is spent by the fresh mark the
   enclave writes on starting. What stays: a rewind of under ten minutes, and the host's
   power to stop the enclave by writing a false mark — visible, never silent.
3. **The purchase must not point at the first question.** We hold the merchant records
   (who paid, when) and our host sees when a provider call goes out; while the service is
   small those two join up, and we learn that a named person asked something at a given
   moment. To build, in this order: cover counted from other accounts' calls since the
   payment, with a shape-matched decoy only when there is none (self-liquidating — the
   decoy spend falls to zero as traffic rises); constant-rate polling of the payment
   provider. Written up in docs/privacy-notes.md and in the pitch notes.

   **Considered and rejected on 2026-09-24: one billing boundary for everyone** (all
   renewals on the same date, the first period pro-rated). It answers the wrong question.
   The anchor is the FIRST purchase, and a common boundary does not move it: the card is
   charged when it is charged, and a new account's first call to a provider follows within
   minutes either way. Renewals, which it does move, are not an anchor to begin with — a
   renewing account has been making calls for weeks, so its payment says nothing new about
   any one of them. The one case it would have helped is a dormant account that renews and
   then wakes, and the cover watcher already covers that for nothing. Against it: a second
   invoice for the part-period, an allowance pro-rated along with it (or subscribing afresh
   on the 29th of each month becomes a free month), and the end of one rule for both rails —
   Apple bills from the day of purchase and cannot be told otherwise, which is the symmetry
   that closed H2 and H5 on 2026-09-21.
4. **Signed images (PCR8) instead of a list of PCR0s**, so an upgrade does not mean editing
   the key policy — and so "whoever can change the policy could name an image of their
   choosing" stops being true.
5. **TAKE THE SANDBOX IMAGE OUT OF THE KEY POLICY.** On 2026-09-24 the probe key was widened
   to two measurements so an App Store **sandbox** purchase could be tested from a real
   phone: `tokumai-probe-2` (production rules) and `tokumai-sandbox-1`
   (`--features nitro,apple-sandbox`, which accepts Apple's sandbox receipts). The whole
   point of building sandbox acceptance into a separate image is that a production image
   cannot be talked into taking a free purchase — and that safeguard is worth nothing if the
   sandbox image is still allowed to open the same book. Anybody who can run it could mint
   balance out of sandbox receipts, which cost nothing.

   Before a mainnet key ever holds real money:
   - `deploy/aws/kms.sh policy` must list **exactly one** measurement, and it must be the
     production image;
   - no image built with `apple-sandbox` may appear in any policy that guards a book with
     paying customers in it;
   - and this belongs in the release notes, not only here: it is a thing that was
     deliberately loosened, and the loosening has to be undone by hand.
5. **The journal is a usage history, and the host may keep every record of it.** Found on
   2026-09-24 while tracing what a seized book would actually reveal. The book is a snapshot
   plus an append-only journal of single SQL changes with their parameters
   (`UPDATE allowance SET left = … WHERE acct = …`), and the enclave folds it into a fresh
   snapshot every 2,000 changes — but the host is not trusted and can keep every record it
   was ever handed. Each is sealed to `(generation, number)`, not to a time, so the same
   data key opens a journal from months ago. Whoever can have that key released (us, by
   naming an image in the key policy; AWS, by holding the root of trust) therefore gets a
   per-account spending history with timestamps, and an amount says whether it was a
   picture. It is the log we deliberately stopped keeping at the proxy, sealed and in
   another place. The fix is a journal key per generation, bound to the monotonic counter of
   item 2 — the same missing building block — so that moving on makes the old journals
   unreadable. **To decide later**, with it: whether the per-day `safety_salt` stops being
   derived from a stored secret (`service.rs:137`), which today lets anyone with that key
   recompute the selector OpenAI's 30 days of logs are filed under. On its own it closes the
   convenient door while the timestamps leave the window open, which is why it waits for
   this item rather than going first.

## Account and cost

A normal AWS account on the **Paid plan** (the Free plan allows only small instance types,
which cannot run enclaves); new accounts get $100–200 of credit. An `m6i.xlarge` in
Frankfurt costs about €0.20 an hour: the 24-hour probe is a few euros.
