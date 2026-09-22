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
  proxy (a static binary) and the enclave started on it; `down` removes everything.

## Next

1. **Docker** on this Mac: the enclave image is built in a Linux container, reproducibly
   (pinned toolchain and base image, `SOURCE_DATE_EPOCH`), so anyone can rebuild it and get
   the same PCR0.
2. **The image**: `tokumai-enclave` (the core plus its own Nym client) packed with Enclaver;
   the egress allowlist.
3. **An AWS account** (Paid plan; see below), a budget alarm, an IAM user with EC2 + KMS in
   eu-central-1, the `aws` CLI configured locally.
4. **The probe**: an `m6i.xlarge` with enclaves enabled; the image started; the dev client
   and the app attest it over the mixnet; 24 h under light load (Nym stable through the
   proxy? latency?).
5. **KMS**: a key whose policy releases it only to our PCR0; the data key, API keys and Nym
   keys sealed with it; a restart and an update of the image (a new PCR0 means a key policy
   update — the upgrade path).

## Account and cost

A normal AWS account on the **Paid plan** (the Free plan allows only small instance types,
which cannot run enclaves); new accounts get $100–200 of credit. An `m6i.xlarge` in
Frankfurt costs about €0.20 an hour: the 24-hour probe is a few euros.
