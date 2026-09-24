fn main() {
    // iOS: let the cdylib link with the StoreKit shim's symbols still open.
    //
    // `cargo build --lib` builds every crate type, the cdylib included, and that link step
    // cannot resolve `tokumai_iap_*`: those live in StoreKitShim.swift, which only Xcode
    // compiles. The app never uses the cdylib — Xcode links `libapp.a`, the staticlib, and
    // resolves the Swift symbols there — so the artefact nobody ships may link with them
    // left open.
    //
    // This belongs here rather than in .cargo/config.toml because `tauri ios build` sets
    // RUSTFLAGS itself, and an env RUSTFLAGS makes cargo ignore the config's rustflags
    // altogether — which is exactly how this failed the first time, working from a
    // terminal and breaking under Xcode. A build-script link arg is ADDED to whatever is
    // already there, so it survives.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {
        println!("cargo::rustc-link-arg-cdylib=-Wl,-undefined,dynamic_lookup");
    }
    bake_probe();
    tauri_build::build()
}

/// Carry the running probe's address and image into a build for a device.
///
/// `target::probe()` reads `dev-data/probe.json` at RUNTIME, which is fine on the machine
/// that wrote it and useless everywhere else: a phone has no such path, so the app had no
/// enclave to talk to and sat on "Connecting to tokumai…" forever. A build for a device
/// therefore takes the probe with it, at build time.
///
/// Debug builds only — a release knows its enclave from `target::RELEASE_ENCLAVE`, and
/// baking a development probe into one would be a way to ship the wrong enclave quietly.
fn bake_probe() {
    println!("cargo::rerun-if-changed=../../dev-data/probe.json");
    if std::env::var("PROFILE").as_deref() != Ok("debug") {
        return;
    }
    let Ok(raw) = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dev-data/probe.json")) else { return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else { return };
    let address = match v["addresses"].as_array() {
        Some(list) => list.iter().filter_map(|a| a.as_str()).collect::<Vec<_>>().join(","),
        None => v["address"].as_str().unwrap_or_default().to_string(),
    };
    let pcr0 = v["pcr0"].as_str().unwrap_or_default().trim().to_lowercase();
    if address.is_empty() || pcr0.len() != 96 {
        return;
    }
    println!("cargo::rustc-env=TOKUMAI_BAKED_ENCLAVE={address}");
    println!("cargo::rustc-env=TOKUMAI_BAKED_PCR0={pcr0}");
}
