//! Fuzz the staged-picture store: `stage()` on a fuzzer-shaped reply (images with
//! arbitrary `data`/`mimeType`), then `image.chunk` requests against it — including the
//! refs it handed out, so out-of-range `seq`, giant/absent fields and expiry paths are all
//! reachable. Must never panic; unknown refs come back as an error reply.
#![no_main]

use libfuzzer_sys::fuzz_target;
use scrai_server::replies::ReplyStore;

fuzz_target!(|data: &[u8]| {
    let mut store = ReplyStore::default();
    let mut parts = data.splitn(2, |b| *b == 0xFF);
    let first = parts.next().unwrap_or(&[]);
    let rest = parts.next().unwrap_or(&[]);

    // 1) stage whatever the first half parses to (or a synthesized reply around it)
    let mut reply = match serde_json::from_slice::<serde_json::Value>(first) {
        Ok(v) if v.get("images").is_some() => v,
        Ok(v) => serde_json::json!({ "text": "", "images": [{ "mimeType": "image/jpeg", "data": v.to_string() }, v] }),
        Err(_) => serde_json::json!({ "images": [{ "mimeType": "image/png", "data": String::from_utf8_lossy(first).repeat(200) }] }),
    };
    store.stage(&mut reply);

    // 2) raw chunk requests from the second half
    let _ = store.handle(rest);

    // 3) requests against the refs the store handed out
    if let Some(imgs) = reply.get("images").and_then(|i| i.as_array()) {
        for img in imgs {
            if let Some(r) = img.get("ref").and_then(|x| x.as_str()) {
                for seq in [0u64, 1, u64::MAX, rest.len() as u64] {
                    let req = serde_json::json!({ "v": 1, "kind": "image.chunk", "id": "f", "ref": r, "seq": seq });
                    let _ = store.handle(&serde_json::to_vec(&req).unwrap_or_default());
                }
            }
        }
    }
});
