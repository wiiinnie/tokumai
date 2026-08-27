//! Fuzz the federation request surface exactly as the mixnet loop feeds it: arbitrary
//! bytes into `dispatch_enveloped` (the id-correlated envelope) and `dispatch` (bare
//! FedRequest). Both promise to NEVER panic — every failure must come back as a reply.
//!
//! The authority is bootstrapped once (1-of-1, like the deployment) and the quorum store
//! persists across iterations so the fuzzer can also discover stateful sequences
//! (spend → double-spend → ban → withdraw-refused).
#![no_main]

use libfuzzer_sys::fuzz_target;
use once_cell::sync::Lazy;
use scrai_core::federation::{self, Authority};
use scrai_core::quorum::QuorumStore;
use std::sync::Mutex;

static AUTHORITY: Lazy<Authority> = Lazy::new(|| {
    let mut auths = federation::bootstrap(1, 1, 500, 2_000_000_000).expect("bootstrap 1-of-1");
    auths.remove(0)
});
static STORE: Lazy<Mutex<QuorumStore>> = Lazy::new(|| Mutex::new(QuorumStore::default()));

fuzz_target!(|data: &[u8]| {
    let mut store = STORE.lock().unwrap();
    let _ = federation::dispatch_enveloped(&AUTHORITY, &mut store, data);
    let _ = federation::dispatch(&AUTHORITY, data);
    // The same bytes wrapped in a syntactically valid envelope reach the handler proper.
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(data) {
        let env = serde_json::json!({ "id": "fuzz", "fed": v });
        let bytes = serde_json::to_vec(&env).unwrap_or_default();
        let _ = federation::dispatch_enveloped(&AUTHORITY, &mut store, &bytes);
    }
});
