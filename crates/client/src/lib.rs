//! The tokumai app core, shared by the desktop and mobile apps. No UI in here.
//!
//! - [`gateways`] — rule A1: the app enters the mixnet through a gateway that is neither
//!   ours nor the enclave's, so no one who runs the service also sees the user's end.
//! - [`mix`] — the app's Nym client: ephemeral (fresh keys each start), with the SDK's
//!   cover traffic, speaking frames (`tokumai_proto::frames`) to the enclave's address.
//! - [`app`] — a connection to the enclave that attests it, attests again when it
//!   restarted, and reconnects when the mixnet client died (sleep, network change).

pub mod app;
pub mod gateways;
pub mod mix;

use std::future::Future;
use std::pin::Pin;

/// One request, one reply — over whatever carries them.
pub trait Transport: Send {
    fn roundtrip<'a>(&'a mut self, message: &'a [u8]) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'a>>;
    /// The address the request went to, when it matters to the attestation (the mixnet).
    fn reached_at(&self) -> Option<String>;
}
