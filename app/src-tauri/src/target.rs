//! Which enclave this build talks to, and what it accepts as proof.
//!
//! A release build knows the published enclave's Nym address and the measurements of the
//! published images — nothing else, and no simulator. Until the first enclave is published
//! both lists are empty, and a release build connects to nothing.
//!
//! A debug build talks to the simulated enclave on this machine (`tokumai-enclave-dev
//! --mix`): its address and simulator key are read from the repo's `dev-data/` (or
//! `TOKUMAI_DEV_DATA`).
//!
//! If `dev-data/probe.json` exists (written by `deploy/aws/probe.sh deploy`), it talks to
//! that REAL Nitro enclave instead: the proof is then checked against the AWS root and the
//! image named there, exactly as a release build would. `TOKUMAI_ENCLAVE` and
//! `TOKUMAI_PCR0` override both.

use std::path::PathBuf;
use tokumai_attest::Policy;

/// The published enclave's doors (its Nym addresses, comma-separated), and the images a
/// release accepts. Several doors mean a gateway can be down without tokumai being
/// unreachable; they all lead to the same enclave.
const RELEASE_ENCLAVE: Option<&str> = None;
const RELEASE_MEASUREMENTS: &[&str] = &[];

fn dev_data() -> PathBuf {
    std::env::var_os("TOKUMAI_DEV_DATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dev-data")))
}

/// What `deploy/aws/probe.sh` writes about a running probe: {"addresses": [ … ], "pcr0": …}
/// (or a single "address", as it used to).
fn probe() -> Option<(String, String)> {
    let raw = std::fs::read_to_string(dev_data().join("probe.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let address = match v["addresses"].as_array() {
        Some(list) => list.iter().filter_map(|a| a.as_str()).collect::<Vec<_>>().join(","),
        None => v["address"].as_str()?.trim().to_string(),
    };
    let pcr0 = v["pcr0"].as_str()?.trim().to_lowercase();
    (!address.is_empty() && pcr0.len() == 96).then_some((address, pcr0))
}

/// The first door — what a single address used to be: for the route display and for
/// keeping our own gateway out of the entry choice.
pub fn enclave_door() -> Result<String, String> {
    enclave_address().map(|a| a.split(',').next().unwrap_or_default().to_string())
}

/// Every door, comma-separated, in the order they should be tried.
pub fn enclave_address() -> Result<String, String> {
    if cfg!(debug_assertions) {
        if let Ok(a) = std::env::var("TOKUMAI_ENCLAVE") {
            return Ok(a.trim().to_string());
        }
        if let Some((address, _)) = probe() {
            return Ok(address);
        }
        return std::fs::read_to_string(dev_data().join("nym-address"))
            .map(|a| a.trim().to_string())
            .map_err(|_| "no enclave to talk to — start `tokumai-enclave-dev --mix`, or a probe with `deploy/aws/probe.sh`".to_string());
    }
    RELEASE_ENCLAVE.map(str::to_string).ok_or_else(|| "no tokumai enclave has been published for this version yet".into())
}

pub fn policy() -> Result<Policy, String> {
    // A real enclave, named by its image: no simulator accepted.
    if let Some(pcr0) = std::env::var("TOKUMAI_PCR0").ok().filter(|p| !p.trim().is_empty()) {
        return Ok(Policy { measurements: vec![pcr0.trim().to_lowercase()], simulated_root: None, simulated_any_measurement: false });
    }
    if cfg!(debug_assertions) {
        if let Some((_, pcr0)) = probe() {
            return Ok(Policy { measurements: vec![pcr0], simulated_root: None, simulated_any_measurement: false });
        }
        let root: [u8; 32] = std::fs::read(dev_data().join("sim-root.key"))
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or("no simulator key in dev-data — start `tokumai-enclave-dev --mix` first")?;
        return Ok(Policy { measurements: vec![], simulated_root: Some(tokumai_attest::sim::root_public(&root)), simulated_any_measurement: true });
    }
    Ok(Policy { measurements: RELEASE_MEASUREMENTS.iter().map(|m| m.to_string()).collect(), simulated_root: None, simulated_any_measurement: false })
}
