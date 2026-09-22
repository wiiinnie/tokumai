//! What runs around the enclave core. The core (`tokumai-enclave`) answers one message with
//! one message; [`mix::serve`] carries those messages over the Nym mixnet: the enclave's own
//! Nym client. It runs inside the enclave, so the host sees Sphinx packets to a gateway and
//! nothing else, and its address goes into every attestation (the app checks it reached that
//! address, not a relay in front).
//!
//! Messages are split into frames and reassembled by `tokumai_proto::frames`. The
//! development stand-ins for the enclave and the app are the binaries here.

pub mod mix;
