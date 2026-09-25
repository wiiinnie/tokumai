//! What runs around the enclave core. The core (`tokumai-enclave`) answers one message with
//! one message; [`mix::serve`] carries those messages over the Nym mixnet: the enclave's own
//! Nym client. It runs inside the enclave, so the host sees Sphinx packets to a gateway and
//! nothing else, and its address goes into every attestation (the app checks it reached that
//! address, not a relay in front).
//!
//! Messages are split into frames and reassembled by `tokumai_proto::frames`. The
//! development stand-ins for the enclave and the app are the binaries here.

pub mod mix;

/// The enclave's only voice.
///
/// A Nitro enclave that is not in debug mode has **no console**: `println!` and `eprintln!`
/// inside it go nowhere at all. Every line the host log shows got there through an explicit
/// announcement over vsock — which is why, for two days, that log stopped at "serving on
/// its doors" and every error after it was invisible, a watchdog built to report exactly
/// this silence included. A watchdog nobody can hear is not a watchdog.
///
/// So: `say` for anything an operator would need in order to understand a silence. It still
/// prints as well, because the development enclave does have a console.
mod voice {
    use std::sync::OnceLock;
    use tokumai_egress::Endpoint;

    static HOST: OnceLock<Endpoint> = OnceLock::new();

    /// Where announcements go. Called once, by the enclave binary, before anything else.
    pub fn speaks_to(host: Endpoint) {
        let _ = HOST.set(host);
    }

    /// Say something the host log should carry. Never blocks the caller and never fails
    /// loudly: a voice that can stall is worse than no voice at all.
    pub fn say(line: impl Into<String>) {
        let line = line.into();
        eprintln!("{line}");
        if let Some(host) = HOST.get().cloned() {
            tokio::spawn(async move {
                let _ = tokumai_egress::announce(&host, &line).await;
            });
        }
    }
}
pub use voice::{say, speaks_to};

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
