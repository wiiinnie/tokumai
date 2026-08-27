// replies.rs — chunked delivery of generated images, the mirror of uploads.rs.
//
// A generated picture (1K ≈ 300 KB, 4K ≈ 4–5 MB base64) as ONE mixnet reply is fragile:
// one lost fragment loses the whole answer, there is no progress, and the request has to
// carry a fixed, oversized reply-SURB budget up front. Instead the dispatch loop stages
// every big picture here and answers with a small reference `{mimeType, ref, chunks,
// bytes}`; the client then fetches `image.chunk {ref, seq}` pieces (pipelined, each small
// enough for a modest SURB budget, individually retryable) and reassembles them.
//
// Ephemeral + capped like uploads: a reference is only handed to the requester in their
// own reply, chunks are served to anyone who knows the (random, 128-bit) ref, and
// everything is swept after TTL — the bill was settled when the picture was generated.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rand::RngCore;
use serde_json::{json, Value};

/// Base64 characters per chunk. ~96 KB ≈ 50 mixnet packets, so the client's small
/// per-chunk SURB budget (80) covers it with headroom. Pictures at or below one chunk
/// stay inline in the chat reply.
pub const CHUNK_B64: usize = 96 * 1024;
/// A staged picture lives this long. Generous on purpose: a 4K picture is 3–5 MB of
/// base64 = 30–55 chunks, and the server's reply stream is paced by Nym's defaults
/// (~50 packets/s ≈ 100 KB/s) and throttled further by reply-SURB re-requests, so a
/// slow mixnet fetch can take minutes — and a client whose download broke mid-way
/// RESUMES it on Retry (only the missing chunks) instead of paying for a new picture.
/// The store stays bounded by MAX_STAGED_BYTES regardless of the TTL.
const TTL: Duration = Duration::from_secs(30 * 60);
/// Total staged base64 across all pictures; beyond it new pictures stay inline.
const MAX_STAGED_BYTES: usize = 64 * 1024 * 1024;

struct Staged {
    chunks: Vec<String>,
    bytes: usize,
    at: Instant,
}

#[derive(Default)]
pub struct ReplyStore {
    items: HashMap<String, Staged>,
    staged: usize,
}

impl ReplyStore {
    /// Move every picture in `reply.images[]` that is bigger than one chunk into the
    /// store, replacing it with a reference the client can fetch chunk by chunk.
    /// Small pictures (≤ one chunk) are left inline — no round trips for a thumbnail.
    pub fn stage(&mut self, reply: &mut Value) {
        self.sweep();
        let Some(imgs) = reply.get_mut("images").and_then(|i| i.as_array_mut()) else { return };
        for img in imgs.iter_mut() {
            let Some(data) = img.get("data").and_then(|d| d.as_str()) else { continue };
            if data.len() <= CHUNK_B64 || self.staged + data.len() > MAX_STAGED_BYTES {
                continue;
            }
            // base64 is ASCII, so byte-wise chunking never splits a character.
            let chunks: Vec<String> = data
                .as_bytes()
                .chunks(CHUNK_B64)
                .map(|c| String::from_utf8_lossy(c).into_owned())
                .collect();
            let bytes = data.len();
            let mut raw = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut raw);
            // (`hex` is only a dev-dependency of the server — format by hand.)
            let r: String = raw.iter().map(|b| format!("{b:02x}")).collect();
            let n = chunks.len();
            self.staged += bytes;
            self.items.insert(r.clone(), Staged { chunks, bytes, at: Instant::now() });
            let mime = img.get("mimeType").cloned().unwrap_or_else(|| json!("image/jpeg"));
            *img = json!({ "mimeType": mime, "ref": r, "chunks": n, "bytes": bytes });
        }
    }

    /// Handle an `image.chunk` envelope; returns the reply bytes.
    pub fn handle(&mut self, request: &[u8]) -> Vec<u8> {
        let v: Value = serde_json::from_slice(request).unwrap_or(Value::Null);
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        let reply = self.chunk(&v, &id);
        serde_json::to_vec(&reply).unwrap_or_default()
    }

    fn chunk(&mut self, v: &Value, id: &Value) -> Value {
        self.sweep();
        let r = v.get("ref").and_then(|x| x.as_str()).unwrap_or("");
        let seq = v.get("seq").and_then(|s| s.as_u64()).unwrap_or(u64::MAX) as usize;
        match self.items.get(r).and_then(|s| s.chunks.get(seq)) {
            Some(data) => json!({ "id": id, "ref": r, "seq": seq, "data": data }),
            None => json!({ "id": id, "kind": "error", "error": "unknown image chunk (expired?)" }),
        }
    }

    fn sweep(&mut self) {
        let now = Instant::now();
        let dead: Vec<String> = self
            .items
            .iter()
            .filter(|(_, s)| now.duration_since(s.at) > TTL)
            .map(|(k, _)| k.clone())
            .collect();
        for k in dead {
            if let Some(s) = self.items.remove(&k) {
                self.staged = self.staged.saturating_sub(s.bytes);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(n: usize) -> String {
        "A".repeat(n)
    }

    #[test]
    fn small_pictures_stay_inline_big_ones_become_references() {
        let mut store = ReplyStore::default();
        let mut reply = json!({ "text": "", "images": [
            { "mimeType": "image/png", "data": big(1000) },
            { "mimeType": "image/jpeg", "data": big(CHUNK_B64 * 2 + 10) },
        ]});
        store.stage(&mut reply);
        let imgs = reply["images"].as_array().unwrap();
        assert_eq!(imgs[0]["data"].as_str().unwrap().len(), 1000, "small stays inline");
        assert!(imgs[1].get("data").is_none());
        assert_eq!(imgs[1]["chunks"], 3);
        assert_eq!(imgs[1]["bytes"], CHUNK_B64 * 2 + 10);
        assert_eq!(imgs[1]["mimeType"], "image/jpeg");
        assert_eq!(imgs[1]["ref"].as_str().unwrap().len(), 32);
    }

    #[test]
    fn chunks_reassemble_to_the_original_and_unknown_refs_error() {
        let mut store = ReplyStore::default();
        let data: String = (0..CHUNK_B64 * 2 + 7).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        let mut reply = json!({ "images": [{ "mimeType": "image/jpeg", "data": data }] });
        store.stage(&mut reply);
        let r = reply["images"][0]["ref"].as_str().unwrap().to_string();
        let n = reply["images"][0]["chunks"].as_u64().unwrap();
        let mut out = String::new();
        for seq in 0..n {
            let req = serde_json::to_vec(&json!({ "kind": "image.chunk", "id": seq, "ref": r, "seq": seq })).unwrap();
            let rep: Value = serde_json::from_slice(&store.handle(&req)).unwrap();
            assert_eq!(rep["seq"], seq);
            out.push_str(rep["data"].as_str().unwrap());
        }
        assert_eq!(out, data);
        let bad = serde_json::to_vec(&json!({ "kind": "image.chunk", "id": 9, "ref": r, "seq": n })).unwrap();
        let rep: Value = serde_json::from_slice(&store.handle(&bad)).unwrap();
        assert_eq!(rep["kind"], "error");
        let bad = serde_json::to_vec(&json!({ "kind": "image.chunk", "id": 9, "ref": "nope", "seq": 0 })).unwrap();
        let rep: Value = serde_json::from_slice(&store.handle(&bad)).unwrap();
        assert_eq!(rep["kind"], "error");
    }

    #[test]
    fn replies_without_images_are_untouched() {
        let mut store = ReplyStore::default();
        let mut reply = json!({ "text": "hi", "cost": 3 });
        let before = reply.clone();
        store.stage(&mut reply);
        assert_eq!(reply, before);
    }
}
