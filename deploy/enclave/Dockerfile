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
ENV SOURCE_DATE_EPOCH=1790000000 \
    CARGO_INCREMENTAL=0 \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
    CARGO_PROFILE_RELEASE_STRIP=symbols \
    RUSTFLAGS="--remap-path-prefix=/src=/tokumai --remap-path-prefix=/usr/local/cargo=/cargo"
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p tokumai-server --bin tokumai-enclave-nitro --features nitro

FROM gcr.io/distroless/cc-debian12@sha256:e5d81ddde149641e2a9ba55be4545bc125c67de07508b03ba4c22e6eb0ded5aa
COPY --from=build /src/target/release/tokumai-enclave-nitro /tokumai-enclave-nitro
# The gateway the enclave's Nym client registers with — part of what is measured, and on
# the host's egress allowlist (deploy/egress.allow).
ENV TOKUMAI_GATEWAY=7MiSywqj6NksUTQCMnyQr9pcKzuthhk444kCGm8cUUT4
ENTRYPOINT ["/tokumai-enclave-nitro"]
