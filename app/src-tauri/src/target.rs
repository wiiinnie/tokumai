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
///
/// 0.7.0 (zizzolo), 2026-09-29: the three doors DE01, AT01, CH01, whose identities are
/// sealed and therefore survive every redeploy. Two images, because App Review and
/// TestFlight buy in Apple's SANDBOX and only the sandbox image honours those receipts:
///
/// - `tokumai-zizzolo-sandbox` (FEATURES=nitro,apple-sandbox), running during review;
/// - `tokumai-zizzolo` (FEATURES=nitro), the production image, deployed at the same
///   addresses once the app is approved.
///
/// The sandbox measurement is a mint for anyone with a sandbox account and must leave
/// this list — and the KMS policy — before real money is in the book (docs/terms-notes.md).
const RELEASE_ENCLAVE: Option<&str> = Some(concat!(
    "nbnWr8CugHhRFvpKUJtuTjM5XA56pSJa2aUAiBaQoWE.HsRNYPx7FY3LsYtsZrCASovcnNRo6FvKPFg3By6PpWcw@38zcSsvjXsAX7C28ko2H3Lt55X4TYxfZYkPADxKXZHUj,",
    "DVpFrxQrWmURn4ztD7Hi8LBEywvEvtA726x5ioHf2WYa.ALh5osBxzqddxaAekRm2tciimg844m8nm4BJ1kMSq4nn@98FmUvDdQYEeV1ioi5NpFK7DoeHphVECndaG7fkRUsaF,",
    "GPscM2poKqpkHdDD3JPGEHiLHufBNRtQyLsvjrMUHQ3o.FhmyxZfn6dHvxASzcR44foa2TjMrCHmm8sQJziAQD3ds@6KZ96sPW6BBcgmghYb7c7BtCXgAEr1nmwnJzzRsszyhe"
));
const RELEASE_MEASUREMENTS: &[&str] = &[
    // tokumai-book-sandbox — the image under App Review with 1.0 (0.8.0); sandbox receipts
    "03206a368faaa3464b7141be7b238c668144c5b3c8a34b94477f83fedcb4e9a21fd209c11ebd31fac3686cb431f378d6",
    // tokumai-book — production, the same code without the sandbox feature
    "b2e091ec2144140a4e98209c0a3355819e88c8e6ef6850d6250982f1d7a1588039c76bbf8f54c9ccd18acdb014010da3",
    // tokumai-ops-sandbox — the witness and the operator's console; deployed after the review
    "5261ea2b021564945de741d75223897b2cf1c7f3ada0650b89d8d9fc7e10f9827bda5439724bd21a371fe321e0a56676",
    // tokumai-ops — production, the same code without the sandbox feature
    "25ed1a81fd1e2bf2f8cc94023a7fd3bd4b492f24e357b53decc862ccd6929ae12e565eda56d75c2213ab5bd87c8692c8",
];

fn dev_data() -> PathBuf {
    std::env::var_os("TOKUMAI_DEV_DATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dev-data")))
}

/// What `deploy/aws/probe.sh` writes about a running probe: {"addresses": [ … ], "pcr0": …}
/// (or a single "address", as it used to).
fn probe() -> Option<(String, String)> {
    // A phone cannot read the Mac's dev-data, so a debug build for a device carries the
    // probe it was built against (see `build.rs`). The FILE still wins where it exists: on
    // the machine that deploys, the newest deploy should beat the last build.
    let baked = || match (option_env!("TOKUMAI_BAKED_ENCLAVE"), option_env!("TOKUMAI_BAKED_PCR0")) {
        (Some(a), Some(p)) if !a.is_empty() && p.len() == 96 => Some((a.to_string(), p.to_string())),
        _ => None,
    };
    let Ok(raw) = std::fs::read_to_string(dev_data().join("probe.json")) else { return baked() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else { return baked() };
    let address = match v["addresses"].as_array() {
        Some(list) => list.iter().filter_map(|a| a.as_str()).collect::<Vec<_>>().join(","),
        None => v["address"].as_str().unwrap_or_default().trim().to_string(),
    };
    let pcr0 = v["pcr0"].as_str().unwrap_or_default().trim().to_lowercase();
    (!address.is_empty() && pcr0.len() == 96).then_some((address, pcr0)).or_else(baked)
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
