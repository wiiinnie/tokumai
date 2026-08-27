//! Fuzz `chat::reserve` — the validation + pricing step that runs ON the dispatch loop
//! for every chat request before any provider call. A panic here takes every client down.
//!
//! Sessions, uploads and the replay cache persist across iterations (stateful sequences:
//! fund → chat → replay → counter reuse). The fuzzer's bytes are used two ways: raw, and
//! re-wrapped as a chat envelope around whatever JSON they happen to be.
#![no_main]

use libfuzzer_sys::fuzz_target;
use once_cell::sync::Lazy;
use scrai_core::pricing::PricingTable;
use scrai_core::session::SessionStore;
use scrai_server::chat;
use scrai_server::uploads::UploadStore;
use std::collections::HashMap;
use std::sync::Mutex;

struct State {
    sessions: SessionStore,
    uploads: UploadStore,
    replies: HashMap<String, (u64, Vec<u8>)>,
}

static PRICING: Lazy<PricingTable> =
    Lazy::new(|| PricingTable::parse(include_str!("../../../pricing.json")).expect("pricing.json"));
static STATE: Lazy<Mutex<State>> = Lazy::new(|| {
    Mutex::new(State { sessions: SessionStore::default(), uploads: UploadStore::default(), replies: HashMap::new() })
});

fuzz_target!(|data: &[u8]| {
    let mut st = STATE.lock().unwrap();
    let State { sessions, uploads, replies } = &mut *st;
    let _ = chat::reserve(data, sessions, uploads, &PRICING, 1.4, replies, 100);
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(data) {
        let mut env = serde_json::json!({ "v": 1, "kind": "chat", "id": "fuzz", "stream": false });
        if let Some(o) = v.as_object() {
            for (k, val) in o {
                env[k] = val.clone();
            }
        } else {
            env["messages"] = serde_json::json!([{ "role": "user", "content": v }]);
            env["model"] = serde_json::json!("gemini-3.5-flash-lite");
        }
        let bytes = serde_json::to_vec(&env).unwrap_or_default();
        let _ = chat::reserve(&bytes, sessions, uploads, &PRICING, 1.4, replies, 100);
    }
});
