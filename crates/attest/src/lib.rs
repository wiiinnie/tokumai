//! Attestation: the enclave proves which code it runs, and which keys that code holds. The app
//! checks the proof before it sends anything.
//!
//! One interface, several platforms. [`Attester`] runs inside the enclave and produces
//! [`Evidence`] over 32 bytes of `user_data`; [`verify`] runs in the app, checks the evidence
//! against a [`Policy`] and returns the attested [`Claims`]. The `user_data` is always
//! [`binding`] — a hash of the enclave's public keys, the address it is reached at, and the
//! app's nonce — so a valid proof cannot be replayed, and cannot vouch for keys or an address
//! the attested code does not hold.
//!
//! - [`sim`]: a stand-in for development. It signs with a local key, so it proves nothing
//!   about the machine, and only a policy that names that key accepts it — which no release
//!   build does.
//! - [`nitro`] and [`gcp`]: the real platforms, compared in phase 0 (docs/enclave-phase0.md).
//!   Not built yet; they refuse everything until they are.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub mod gcp;
pub mod nitro;
pub mod sim;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Platform {
    Simulated,
    AwsNitro,
    GcpConfidentialSpace,
}

/// What the enclave hands the app: the platform's own document, as the platform encodes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub platform: Platform,
    /// Base64 of the platform document (Nitro: COSE_Sign1; GCP: the token; simulated: JSON).
    pub document: String,
}

/// What a verified proof says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    pub platform: Platform,
    /// The image the enclave booted, as the platform measures it (hex).
    pub measurement: String,
}

/// What the app is willing to trust.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// Measurements of published releases (hex). A proof for anything else is refused.
    pub measurements: Vec<String>,
    /// The development key the simulator signs with. `None` in every release build: a
    /// simulated proof is then refused outright, whatever it says.
    pub simulated_root: Option<[u8; 32]>,
    /// Development only: accept any measurement from the simulator (it has no real image).
    pub simulated_any_measurement: bool,
}

/// Runs inside the enclave.
pub trait Attester: Send + Sync {
    fn platform(&self) -> Platform;
    fn attest(&self, user_data: &[u8; 32]) -> Result<Evidence, String>;
}

/// The 32 bytes every proof must carry: the enclave's signing key, its key-exchange key, the
/// address the enclave itself listens at (its Nym address; empty where there is none), and
/// the app's fresh nonce, hashed under a fixed label. With the address in it, the app knows
/// the mixnet endpoint it talks to is the enclave's own client, not a relay in front of it
/// run by the operator — which would see when each request comes and goes.
pub fn binding(identity_pub: &[u8; 32], kx_pub: &[u8; 32], address: &str, nonce: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"tokumai/attest/v1");
    h.update(identity_pub);
    h.update(kx_pub);
    h.update((address.len() as u32).to_be_bytes());
    h.update(address.as_bytes());
    h.update(nonce);
    h.finalize().into()
}

/// The binding with one more thing the proof vouches for: a digest of what the enclave
/// publishes beside its keys — today the month keys of the blind notes. Bound here so
/// that every app sees the same published set, or its attestation check fails.
pub fn binding_with(identity_pub: &[u8; 32], kx_pub: &[u8; 32], address: &str, nonce: &[u8], published: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"tokumai/attest/v2");
    h.update(binding(identity_pub, kx_pub, address, nonce));
    h.update(published);
    h.finalize().into()
}

/// Check `evidence` against `policy`, and that it vouches for exactly `expected_user_data`.
pub fn verify(evidence: &Evidence, policy: &Policy, expected_user_data: &[u8; 32]) -> Result<Claims, String> {
    let (measurement, user_data) = match evidence.platform {
        Platform::Simulated => sim::verify(evidence, policy)?,
        Platform::AwsNitro => nitro::verify(evidence)?,
        Platform::GcpConfidentialSpace => gcp::verify(evidence)?,
    };
    if &user_data != expected_user_data {
        return Err("the proof is for other keys or another request".into());
    }
    let simulated_any = evidence.platform == Platform::Simulated && policy.simulated_any_measurement;
    if !simulated_any && !policy.measurements.iter().any(|m| m.eq_ignore_ascii_case(&measurement)) {
        return Err(format!("the enclave runs an image this app does not know ({measurement})"));
    }
    Ok(Claims { platform: evidence.platform, measurement })
}
