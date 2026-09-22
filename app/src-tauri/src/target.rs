//! Which enclave this build talks to, and what it accepts as proof.
//!
//! A release build knows the published enclave's Nym address and the measurements of the
//! published images — nothing else, and no simulator. Until the first enclave is published
//! both lists are empty, and a release build connects to nothing.
//!
//! A debug build talks to the simulated enclave on this machine (`tokumai-enclave-dev
//! --mix`): its address and simulator key are read from the repo's `dev-data/` (or
//! `TOKUMAI_DEV_DATA`); `TOKUMAI_ENCLAVE` overrides the address. With `TOKUMAI_PCR0` set
//! it talks to a REAL Nitro enclave instead (the phase-0 probe): the proof is then checked
//! against the AWS root and that one image, exactly as a release build would.

use std::path::PathBuf;
use tokumai_attest::Policy;

/// The published enclave (its Nym address), and the images a release accepts.
const RELEASE_ENCLAVE: Option<&str> = None;
const RELEASE_MEASUREMENTS: &[&str] = &[];

fn dev_data() -> PathBuf {
    std::env::var_os("TOKUMAI_DEV_DATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dev-data")))
}

pub fn enclave_address() -> Result<String, String> {
    if cfg!(debug_assertions) {
        if let Ok(a) = std::env::var("TOKUMAI_ENCLAVE") {
            return Ok(a.trim().to_string());
        }
        return std::fs::read_to_string(dev_data().join("nym-address"))
            .map(|a| a.trim().to_string())
            .map_err(|_| "no simulated enclave on the mixnet — start `tokumai-enclave-dev --mix` first".to_string());
    }
    RELEASE_ENCLAVE.map(str::to_string).ok_or_else(|| "no tokumai enclave has been published for this version yet".into())
}

pub fn policy() -> Result<Policy, String> {
    // A real enclave, named by its image: no simulator accepted.
    if let Some(pcr0) = std::env::var("TOKUMAI_PCR0").ok().filter(|p| !p.trim().is_empty()) {
        return Ok(Policy { measurements: vec![pcr0.trim().to_lowercase()], simulated_root: None, simulated_any_measurement: false });
    }
    if cfg!(debug_assertions) {
        let root: [u8; 32] = std::fs::read(dev_data().join("sim-root.key"))
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or("no simulator key in dev-data — start `tokumai-enclave-dev --mix` first")?;
        return Ok(Policy { measurements: vec![], simulated_root: Some(tokumai_attest::sim::root_public(&root)), simulated_any_measurement: true });
    }
    Ok(Policy { measurements: RELEASE_MEASUREMENTS.iter().map(|m| m.to_string()).collect(), simulated_root: None, simulated_any_measurement: false })
}
