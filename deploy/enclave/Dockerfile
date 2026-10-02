# The tokumai enclave image (AWS Nitro, arm64 / Graviton). Built so that anyone can rebuild
# it from this repository and get the same PCR0 — without that, an attestation proves
# nothing about the published code:
#   - both base images pinned by digest;
#   - dependencies exactly as in Cargo.lock (--locked);
#   - no build paths or dates in the binary (--remap-path-prefix, SOURCE_DATE_EPOCH);
#   - one codegen unit, symbols stripped.
#
#     deploy/enclave/build.sh
#
# The runtime image is distroless: glibc, CA certificates, the one binary. No shell.

FROM rust@sha256:2775a09d208ff0d7c1f50490c45b62db929e87ba1dcbc3f2132ac71a704bcdd3 AS build
# STRIP=none DEBUGINFO=line-tables-only keeps the symbols in, for a testing image whose
# stack report (`stuck` in tokumai-enclave-nitro.rs) should name functions and lines. A
# production image is stripped, and PCR0 says which one is running.
ARG STRIP=symbols
ARG DEBUGINFO=0
ENV SOURCE_DATE_EPOCH=1790000000 \
    CARGO_INCREMENTAL=0 \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
    CARGO_PROFILE_RELEASE_STRIP=$STRIP \
    CARGO_PROFILE_RELEASE_DEBUG=$DEBUGINFO \
    RUSTFLAGS="--remap-path-prefix=/src=/tokumai --remap-path-prefix=/usr/local/cargo=/cargo"
# The feature list, so a sandbox image can be built from the same file. It changes the
# binary and therefore PCR0 — which is the point: the measurement says which one runs.
ARG FEATURES=nitro
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p tokumai-server --bin tokumai-enclave-nitro --features "$FEATURES"

FROM gcr.io/distroless/cc-debian12@sha256:e5d81ddde149641e2a9ba55be4545bc125c67de07508b03ba4c22e6eb0ded5aa
COPY --from=build /src/target/release/tokumai-enclave-nitro /tokumai-enclave-nitro
# The gateways the enclave's Nym clients register with: the operator's own, as the first
# server had it — the enclave's own front doors should be machines we keep running. (The
# APP's entry gateway is the opposite: never one of ours, rule A1.) Part of what is
# measured, so a change means a new image; alternates are on the host's allowlist already.
# DE01 nym-exit-de01.hermes-stakepool.de · AT01 98FmUv… · CH01 6KZ96s…
# Three doors, in the order an app tries them: DE01, AT01, CH01 — one Nym client each,
# all leading to this one enclave, so a gateway going down does not take tokumai with it.
# (Reduced to DE01 alone on 2026-09-28 to test whether three clients in one runtime were
# behind the silent deaths; they were not — a lock taken twice was — so all three are back.)
ENV TOKUMAI_GATEWAYS=38zcSsvjXsAX7C28ko2H3Lt55X4TYxfZYkPADxKXZHUj,98FmUvDdQYEeV1ioi5NpFK7DoeHphVECndaG7fkRUsaF,6KZ96sPW6BBcgmghYb7c7BtCXgAEr1nmwnJzzRsszyhe
# The witness of the book's progress (crates/enclave/src/witness.rs): an S3 bucket with
# Object Lock in compliance mode, named here so that the host cannot substitute one.
ENV TOKUMAI_WITNESS_BUCKET=tokumai-book-946944821363
ENTRYPOINT ["/tokumai-enclave-nitro"]
