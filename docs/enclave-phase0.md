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
journal of it, one record per change, written down before the person is told their request
went through. At the next start the snapshot is read back and the journal replayed; every
record is sealed to its place in the line, so one dropped from the middle, reordered, or
kept from an older snapshot does not open. Verified on the probe: 22 changes replayed
across a restart onto a NEW image, which is also the upgrade path.

Two things it does not survive, both about where the bytes lie rather than how they are
sealed, and both on the list below: a host that is replaced, and a host that hands back an
older pair of files.

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

1. **The book must outlive the machine.** It lies on the instance's root volume today, so
   replacing the instance takes every balance and plan with it — which is exactly what
   happened on the morning of 2026-09-23, when a terminated probe took a paid test plan
   with it. It belongs on a volume of its own (`DeleteOnTermination = false`, so it
   survives the instance it is attached to) with backups, and the operator's own runbook
   must say `stop`, never `terminate`. A customer's balance may not depend on which
   machine it was bought on.
2. **A rewind must be detectable.** The host can hand back an older snapshot and journal,
   and nothing inside a Nitro enclave survives a restart to notice — no counter, no key.
   Closing it needs a counter the host cannot turn back, kept outside (a small conditional
   write per snapshot is enough). Until then the operator is trusted for freshness; that
   is a sentence we must be willing to write in the privacy policy.
3. **Signed images (PCR8) instead of a list of PCR0s**, so an upgrade does not mean editing
   the key policy — and so "whoever can change the policy could name an image of their
   choosing" stops being true.

## Account and cost

A normal AWS account on the **Paid plan** (the Free plan allows only small instance types,
which cannot run enclaves); new accounts get $100–200 of credit. An `m6i.xlarge` in
Frankfurt costs about €0.20 an hour: the 24-hour probe is a few euros.
