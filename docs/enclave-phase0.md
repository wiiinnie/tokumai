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
