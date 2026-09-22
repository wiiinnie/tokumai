//! What runs around the enclave core. The core (`tokumai-enclave`) answers one message with
//! one message; this crate carries those messages over the Nym mixnet, both ways:
//!
//! - [`mix::serve`] — the enclave's own Nym client. It runs inside the enclave, so the host
//!   sees Sphinx packets to a gateway and nothing else, and its address goes into every
//!   attestation (the app checks it reached that address, not a relay in front).
//! - [`mix::MixTransport`] — the app's side, used by the development client now and by the
//!   app later.
//!
//! Messages are split into frames and reassembled by `tokumai_enclave::frames`.

pub mod mix;

use std::future::Future;
use std::pin::Pin;

/// One request, one reply — over whatever carries them.
pub trait Transport: Send {
    fn roundtrip<'a>(&'a mut self, message: &'a [u8]) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'a>>;
    /// The address the request went to, when it matters to the attestation (the mixnet).
    fn reached_at(&self) -> Option<String>;
}
