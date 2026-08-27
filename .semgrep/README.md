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
  Semgrep OSS taint is **intraprocedural** — a flow that crosses a function boundary (e.g.
  `messages[]` filled in `onDone`, rendered in `renderThread`) is not followed. Cross-function
  taint needs Semgrep Pro or CodeQL; until then the second line of defence is the HTML-sink
  rule above (every `${…}` in a sink must be wrapped) and the fuzz targets for the Rust parsers.

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
