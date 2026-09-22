//! Google Confidential Space. The document is a token from Google's attestation service; its
//! claims name the container image digest, the debug state and the nonce.
//!
//! Phase 0 builds this (docs/enclave-phase0.md). Until then every such proof is refused.

use crate::Evidence;

pub(crate) fn verify(_evidence: &Evidence) -> Result<(String, [u8; 32]), String> {
    Err("Google Confidential Space attestation is not built yet".into())
}
