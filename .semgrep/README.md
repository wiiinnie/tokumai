# Static security scanning

`scripts/semgrep.sh` runs Semgrep locally (metrics off, no login, no upload) with the public
security packs plus the two rule files here, then `cargo audit`. CI runs the same script
(`.github/workflows/security.yml`) and fails on ERROR/WARNING findings.

- `scrambleai.yml` — project invariants no generic pack knows: no key material in logs, the
  fake payment rail gated in exactly one place, no clearnet HTTP in the app, no panics on the
  server request path, `unsafe` only in the FFI modules, no eval/`document.write` in the
  webview, HTML sinks only with escaped interpolations, external links only via `openLink()`.
- `scrambleai-taint.yml` — Semgrep **taint mode** for the webview: sources are server replies,
  model text, file names and clipboard input; sanitizers are `esc()`, `escAttr()`,
  `safeHttpUrl()` and the numeric formatters; sinks are `innerHTML`/`outerHTML`/
  `insertAdjacentHTML`/`document.write` and `src`/`href`/`style`/`on*` attributes.
  Semgrep OSS taint is **intraprocedural**. `SEMGREP_PRO=1 scripts/semgrep.sh` switches to the
  Pro engine (cross-file/interprocedural) — needs a one-time `semgrep login` +
  `semgrep install-semgrep-pro`; only finding metadata leaves the machine, and we run it over
  the webview only (never core/server). Pro run 2026-08-28 over `public/` + the inline
  scripts: 4 cross-file flows, all from numeric Rust event payloads (download/upload progress)
  or `.map()`-escaped lists — the two numeric ones were hardened with `Number()`, the rest
  annotated; result **0 blocking**. CodeQL was ruled out: its CLI licence forbids use on
  non-open-source code without GitHub Code Security ($30/committer/month).

The inline `<script>` blocks of `public/index.html` are extracted to a temp dir before the
scan (Semgrep does not scan JS inside HTML); finding lines are reported relative to the
extracted file, whose first line states the HTML line offset.

## Accepted findings (reviewed 2026-08-27)

| rule | where | why it stays |
|---|---|---|
| `rust.lang.security.unsafe-usage` | `src-tauri/src/ocr.rs`, `lib.rs` (objc2 picker, Vision OCR) | native FFI — each block carries its safety comment; the custom rule `scrai-unsafe-outside-ffi` guards every other file |
| `rust.lang.security.temp-dir` | tests in `store.rs`/`wallet.rs`; `lib.rs` handover export | tests only; the export writes the file the OS share sheet needs, inside the app sandbox |
| `scrai-unwrap-in-server-hot-path` | `nyx.rs` mutex locks, `pay.rs:320`, `uploads.rs:145` | poison-only locks around plain assignments; `expect("checked above")` proven by the preceding lookup — annotated `nosemgrep` in place |
| `scrai-taint-untrusted-to-html` | terms dialog (`links`), pay panel (`o.qr`) | `links` are `esc()`/`escAttr()`-wrapped inside `.map()` (taint can't see through the map); `o.qr` is an SVG the **app** renders locally with the `qrcode` crate — the server only sends address + memo |
| `cargo audit`: `libcrux-*`, `h2 0.3`, `rustls-webpki 0.101`, `rsa` | pulled in by `nym-sdk 1.21.4` (nym-crypto, tendermint-rpc, jwt-simple) | not reachable from our code paths; fixed upstream only — re-check on every nym-sdk bump |
| `cargo audit`: `lopdf 0.36` (via `pdf-extract`) | on-device PDF text extraction for the privacy guard | DoS only (stack overflow on a hostile PDF the user chose to attach); no upstream release with `lopdf ≥ 0.42` yet |
| `cargo audit`: `lru 0.12` (via `ratatui 0.28`) | `scrai-admin`, the operator's local terminal UI | never handles remote input; the fix needs a `ratatui` major bump — do it with the next admin-TUI change |

## Fuzzing (the Rust answer to taint tracking)

There is no useful taint tool for Rust, but the server's attack surface is four parsers fed
with bytes from anonymous mixnet senders. `scripts/fuzz.sh` (cargo-fuzz, nightly, ASan)
drives them directly — `server/fuzz/fuzz_targets/`:

| target | entry point | what it exercises |
|---|---|---|
| `fed_dispatch` | `federation::dispatch_enveloped` / `dispatch` | Coconut spend/withdraw envelopes against a live 1-of-1 authority + persistent quorum store (double-spend / ban sequences) |
| `chat_reserve` | `chat::reserve` | request validation, signature/counter checks, pricing ceiling — the code that runs on the dispatch loop |
| `upload_handle` | `UploadStore::handle` | begin/chunk sequences, offsets, size lies, caps |
| `replies_handle` | `ReplyStore::stage` + `handle` | staged pictures, `image.chunk` refs/seqs, expiry |

Baseline 2026-08-27: 150 s per target in parallel — 3.3 M / 2.9 M / 0.76 M / 1.06 M runs, ~2500 edges each, **no panic, no leak, no OOM**. The `server/src/lib.rs` split exists for this: the binary, the admin TUI and the fuzz targets share one code path.
