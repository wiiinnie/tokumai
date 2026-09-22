//! The development stand-in. Signs `{measurement, user_data}` with a local Ed25519 key.
//! It proves nothing about hardware — it exists so that everything around attestation (the
//! binding, the app's check, the refusal paths) runs and is tested before a real platform
//! is attached.

use crate::{Attester, Evidence, Platform, Policy};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Doc {
    measurement: String,
    user_data: String,
    sig: String,
}

pub struct SimAttester {
    root: SigningKey,
    measurement: String,
}

impl SimAttester {
    /// `root_seed` stands in for the platform's signing key; `measurement` for the image hash.
    pub fn new(root_seed: [u8; 32], measurement: impl Into<String>) -> Self {
        SimAttester { root: SigningKey::from_bytes(&root_seed), measurement: measurement.into() }
    }
}

/// The public half of a simulator root, for a development policy.
pub fn root_public(root_seed: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(root_seed).verifying_key().to_bytes()
}

fn signed_bytes(measurement: &str, user_data_hex: &str) -> Vec<u8> {
    format!("tokumai/sim/v1:{measurement}:{user_data_hex}").into_bytes()
}

impl Attester for SimAttester {
    fn platform(&self) -> Platform {
        Platform::Simulated
    }
    fn attest(&self, user_data: &[u8; 32]) -> Result<Evidence, String> {
        let ud = hex::encode(user_data);
        let sig = self.root.sign(&signed_bytes(&self.measurement, &ud));
        let doc = Doc { measurement: self.measurement.clone(), user_data: ud, sig: B64.encode(sig.to_bytes()) };
        let json = serde_json::to_vec(&doc).map_err(|e| e.to_string())?;
        Ok(Evidence { platform: Platform::Simulated, document: B64.encode(json) })
    }
}

pub(crate) fn verify(evidence: &Evidence, policy: &Policy) -> Result<(String, [u8; 32]), String> {
    let root = policy.simulated_root.ok_or("simulated attestation is not accepted by this build")?;
    let raw = B64.decode(&evidence.document).map_err(|_| "malformed simulated proof")?;
    let doc: Doc = serde_json::from_slice(&raw).map_err(|_| "malformed simulated proof")?;
    let key = VerifyingKey::from_bytes(&root).map_err(|_| "bad simulator root")?;
    let sig = Signature::from_slice(&B64.decode(&doc.sig).map_err(|_| "malformed simulated proof")?)
        .map_err(|_| "malformed simulated proof")?;
    key.verify(&signed_bytes(&doc.measurement, &doc.user_data), &sig)
        .map_err(|_| "the simulated proof is not signed by the development root")?;
    let ud: [u8; 32] = hex::decode(&doc.user_data)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or("malformed simulated proof")?;
    Ok((doc.measurement, ud))
}

#[cfg(test)]
mod tests {
    use crate::*;

    fn policy(seed: [u8; 32]) -> Policy {
        Policy { measurements: vec!["abc123".into()], simulated_root: Some(sim::root_public(&seed)), simulated_any_measurement: false }
    }

    #[test]
    fn a_simulated_proof_verifies_only_for_its_keys_and_nonce() {
        let seed = [7u8; 32];
        let a = sim::SimAttester::new(seed, "abc123");
        let ud = binding(&[1; 32], &[2; 32], "", b"nonce-1");
        let e = a.attest(&ud).unwrap();
        assert_eq!(verify(&e, &policy(seed), &ud).unwrap().measurement, "abc123");
        let other = binding(&[1; 32], &[2; 32], "", b"nonce-2");
        assert!(verify(&e, &policy(seed), &other).is_err(), "a proof for another nonce is refused");
        let swapped = binding(&[9; 32], &[2; 32], "", b"nonce-1");
        assert!(verify(&e, &policy(seed), &swapped).is_err(), "a proof for other keys is refused");
    }

    #[test]
    fn a_release_policy_refuses_the_simulator_whatever_it_says() {
        let seed = [7u8; 32];
        let ud = binding(&[1; 32], &[2; 32], "", b"n");
        let e = sim::SimAttester::new(seed, "abc123").attest(&ud).unwrap();
        let release = Policy { measurements: vec!["abc123".into()], ..Default::default() };
        assert!(verify(&e, &release, &ud).unwrap_err().contains("not accepted"));
    }

    #[test]
    fn an_unknown_image_or_a_foreign_root_is_refused() {
        let seed = [7u8; 32];
        let ud = binding(&[1; 32], &[2; 32], "", b"n");
        let e = sim::SimAttester::new(seed, "evil").attest(&ud).unwrap();
        assert!(verify(&e, &policy(seed), &ud).unwrap_err().contains("does not know"));
        let foreign = sim::SimAttester::new([8u8; 32], "abc123").attest(&ud).unwrap();
        assert!(verify(&foreign, &policy(seed), &ud).unwrap_err().contains("development root"));
    }
}
