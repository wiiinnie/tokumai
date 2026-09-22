# Vendored, patched crates

## nym-gateway-client 1.22.0 (Apache-2.0, © Nym Technologies SA)

Copied from crates.io unchanged except for one addition in `src/client/websockets.rs`,
marked `tokumai patch`: with `TOKUMAI_EGRESS_PROXY=host:port` set, the client reaches its
gateway through an HTTP CONNECT proxy instead of opening the TCP connection itself (and
resolving the name with its own DNS). An AWS Nitro enclave has no network; everything
leaves through that proxy, over vsock, to the host. The WebSocket and its TLS still run
end to end over the tunnel. `Cargo.toml` adds tokio's `io-util` and `net` features for it.

Unset, the code path is the original one. The root `Cargo.toml` substitutes this copy with
`[patch.crates-io]`. When nym-sdk moves to a newer nym-gateway-client, re-apply the patch
to that version (or drop it, should the SDK learn to use a proxy itself).
