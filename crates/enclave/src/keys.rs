//! The enclave's own keys, made fresh at every start and never written anywhere: an Ed25519
//! identity (requests are signed to it, so a signature is good for this enclave only) and an
//! X25519 key the app encrypts its requests to. Both go into the attestation binding, which
//! is how the app knows they belong to the attested code.

use rand::rngs::OsRng;
use x25519_dalek::{PublicKey, StaticSecret};

pub struct EnclaveKeys {
    pub identity: ed25519_dalek::SigningKey,
    pub kx: StaticSecret,
}

impl EnclaveKeys {
    pub fn generate() -> Self {
        EnclaveKeys { identity: ed25519_dalek::SigningKey::generate(&mut OsRng), kx: StaticSecret::random_from_rng(OsRng) }
    }
    pub fn identity_pub(&self) -> [u8; 32] {
        self.identity.verifying_key().to_bytes()
    }
    pub fn kx_pub(&self) -> [u8; 32] {
        PublicKey::from(&self.kx).to_bytes()
    }
}
