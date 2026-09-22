//! What the app and the enclave share: the wire format, the session (attest, sign, seal),
//! and the framing that carries messages over the mixnet. Small on purpose — this is the
//! part of the app that talks to the enclave, and it should be easy to read in full.

pub mod frames;
pub mod session;
pub mod wire;

/// Milliseconds since the epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
