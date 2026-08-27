//! Fuzz the chunked vision-upload store: `upload.begin` / `upload.chunk` envelopes.
//! Each fuzz input is split on 0xFF into a SEQUENCE of messages fed to one fresh store,
//! so the fuzzer can find bad begin/chunk orderings, size lies, out-of-range offsets and
//! reassembly edge cases — the store must never panic and never exceed its byte caps.
#![no_main]

use libfuzzer_sys::fuzz_target;
use scrai_server::uploads::UploadStore;

fuzz_target!(|data: &[u8]| {
    let mut store = UploadStore::default();
    for msg in data.split(|b| *b == 0xFF).take(64) {
        let _ = store.handle(msg);
        // The same bytes as the `data` field of a chunk against every id the fuzzer used.
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(msg) {
            if v.get("kind").is_none() {
                let begin = serde_json::json!({ "v": 1, "kind": "upload.begin", "id": "f", "mimeType": "image/jpeg", "totalBytes": v.get("totalBytes").cloned().unwrap_or(serde_json::json!(1024)) });
                let rep = store.handle(&serde_json::to_vec(&begin).unwrap_or_default());
                if let Ok(r) = serde_json::from_slice::<serde_json::Value>(&rep) {
                    if let Some(uid) = r.get("uploadId").and_then(|u| u.as_str()) {
                        let chunk = serde_json::json!({ "v": 1, "kind": "upload.chunk", "id": "f", "uploadId": uid, "offset": v.get("offset").cloned().unwrap_or(serde_json::json!(0)), "data": v });
                        let _ = store.handle(&serde_json::to_vec(&chunk).unwrap_or_default());
                    }
                }
            }
        }
    }
});
