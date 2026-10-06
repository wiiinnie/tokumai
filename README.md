# tokumai

Anonymous access to frontier AI models. Nobody can connect a question to the person who
asked it, and that includes us, the operator.

- **The mixnet** ([Nym](https://nym.com)) hides who you are on the network. The server never
  sees your IP address.
- **The enclave** hides it from us. Accounts, balances, billing and the relay to the model
  providers run inside an attested enclave. The app checks, before it sends anything, that
  the enclave runs exactly the published code. The operator's machine sees only ciphertext.
- **The model provider** (OpenAI, Google) sees the question, but under our key, never
  yours.

This repository is the second generation. The first one paid for each prompt with
blind-signed coins. That separated payment and question cryptographically, but the timing
still linked them, and the server could read every question. See
`docs/architecture.md`.

## Layout

| | |
|---|---|
| `crates/core` | shared by app and enclave: accounts from a recovery phrase, signatures, plan periods |
| `crates/attest` | attestation: the enclave produces evidence, the app verifies it. Simulated today; AWS Nitro and Google Confidential Space plug in behind the same interface |
| `crates/enclave` | the server core: the wire format, the account ledger, billing, the model relay |

## Development: a simulated enclave

The real service with stand-ins for the parts only a real enclave has:

| | development | production |
|---|---|---|
| who vouches for the code | `attest::sim` (a local key in `dev-data/sim-root.key`) | AWS Nitro / Google Confidential Space |
| data key | `dev-data/data.key` | made by KMS for an attested enclave, never seen outside one |
| model | the mock, plus OpenAI / Gemini if `OPENAI_API_KEY` / `GEMINI_API_KEY` are set | OpenAI, Gemini, keys sealed |
| transport | TCP on 127.0.0.1:7707, one JSON message per line; with `--mix` also the Nym mixnet | the Nym mixnet, the client inside the enclave, its address attested |

```sh
cargo run -p tokumai-server --bin tokumai-enclave-dev [-- --mix]   # the enclave, simulated
cargo run -p tokumai-server --bin tokumai-dev-client -- [--mix] "a question" [model]
cd app && npm install && npm run dev                         # the app (first run installs the Tauri CLI)
cargo test                                                   # everything
deploy/audit.sh                                              # the dependency audit (cargo-audit, .cargo/audit.toml)
```

A release build of the app refuses the simulator outright: its policy carries no simulator
root (`attest::Policy::simulated_root`).

## Status

Early. Built so far:
- the wire format: X25519 + ChaCha20-Poly1305, sealed to the attested key;
- attestation binding and the simulator;
- the ledger: a monthly allowance, prepaid valid three years from purchase, hold and settle
  per request;
- the providers, OpenAI (Responses API) and Gemini (text, pictures, search grounding), with
  OpenAI's moderation check in front of both, a per-account daily pseudonym for OpenAI, and
  three declines a day per provider before it pauses for that account;
- plans by card (Stripe) and App Store, with the renewal, refund and chargeback check run
  from inside the enclave;
- the Nym transport: the enclave's own client, its address attested, messages in
  acknowledged frames (`crates/proto`);
- the app core (`crates/client`): connect, attest, re-attest after a restart, reconnect
  after sleep, and never enter the mixnet through a gateway of ours (rule A1).

Crates: `core` (account, billing, plans' calendar), `attest`, `proto` (what app and
enclave share), `enclave` (the attested core), `client` (the app core), `server` (the
enclave's mixnet end and the development binaries).

Prices (`pricing.json`), margin and limits (`crates/enclave/src/policy.rs`) are compiled
into the image, so the attestation covers them.

Next: the app itself (Tauri, the existing interface), and the two platform probes
(`docs/enclave-phase0.md`).

## License

Not decided yet. The enclave code will be public and reproducibly buildable: without that,
an attestation proves nothing.
