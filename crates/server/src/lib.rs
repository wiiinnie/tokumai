//! What runs around the enclave core. The core (`tokumai-enclave`) answers one message with
//! one message; [`mix::serve`] carries those messages over the Nym mixnet: the enclave's own
//! Nym client. It runs inside the enclave, so the host sees Sphinx packets to a gateway and
//! nothing else, and its address goes into every attestation (the app checks it reached that
//! address, not a relay in front).
//!
//! Messages are split into frames and reassembled by `tokumai_proto::frames`. The
//! development stand-ins for the enclave and the app are the binaries here.

pub mod mix;

/// A running phase-0 probe, as `deploy/aws/probe.sh deploy` recorded it in
/// `dev-data/probe.json`: (its Nym address, its PCR0). The development tools then talk to
/// it without anyone having to remember two environment variables — and, more to the
/// point, without talking to the wrong enclave when one of them is forgotten.
/// Where the development files live. `TOKUMAI_DEV_DATA` points a second enclave, or a
/// client talking to one, at its own set (see `tokumai-enclave-dev`).
pub fn dev_data() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("TOKUMAI_DEV_DATA").unwrap_or_else(|_| "dev-data".into()))
}

pub fn probe_target() -> (Option<String>, Option<String>) {
    let Ok(raw) = std::fs::read_to_string(dev_data().join("probe.json")) else { return (None, None) };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else { return (None, None) };
    // Every door, comma-separated: the dev tools fail over exactly as the app does.
    let address = match v["addresses"].as_array() {
        Some(list) => Some(list.iter().filter_map(|a| a.as_str()).collect::<Vec<_>>().join(",")).filter(|a: &String| !a.is_empty()),
        None => v["address"].as_str().map(|a| a.trim().to_string()).filter(|a| !a.is_empty()),
    };
    let pcr0 = v["pcr0"].as_str().map(|p| p.trim().to_lowercase()).filter(|p| p.len() == 96);
    (address, pcr0)
}
