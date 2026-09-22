//! AWS Nitro Enclaves. The document is a COSE_Sign1 (CBOR) signed with P-384 by a certificate
//! that chains to the AWS Nitro root; PCR0 is the image measurement and `user_data` carries
//! our binding. The app checks it offline against the pinned root.
//!
//! Phase 0 builds this (docs/enclave-phase0.md). Until then every Nitro proof is refused, so a
//! half-built verifier can never accept anything.

use crate::Evidence;

pub(crate) fn verify(_evidence: &Evidence) -> Result<(String, [u8; 32]), String> {
    Err("AWS Nitro attestation is not built yet".into())
}
