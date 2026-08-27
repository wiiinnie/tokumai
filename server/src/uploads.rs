// uploads.rs — chunked vision-image upload staging, ported from the TS server
// (src/cli/server.ts). Vision images arrive as small acked chunks (reliable over
// a lossy mixnet, and progress-reportable) and are reassembled here, then
// consumed by the chat that references them by uploadId.
//
// Ephemeral, capped and swept so an anonymous caller cannot exhaust memory.
// Uploads are unauthenticated by design (the chat that consumes them is billed);
// the caps are the DoS bound.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use rand::RngCore;
use serde_json::{json, Value};

const TTL: Duration = Duration::from_secs(3 * 60);
const MAX_CONCURRENT_UPLOADS: usize = 12;
const MAX_STAGED_BYTES: usize = 96 * 1024 * 1024;
// Per-chunk ceiling on the *encoded* base64 string, enforced BEFORE decoding so an
// oversized chunk is refused without ever being allocated/decoded (M-srv-2). Base64
// inflates ~4/3, so 8 MiB of payload ≈ 11 MiB of text — generous for a vision tile.
const MAX_CHUNK_B64_LEN: usize = 12 * 1024 * 1024;

struct Upload {
    mime_type: String,
    total: usize,
    received: usize,
    chunks: HashMap<u64, Vec<u8>>,
    at: Instant,
}

#[derive(Default)]
pub struct UploadStore {
    uploads: HashMap<String, Upload>,
    staged: usize,
}

impl UploadStore {
    /// Handle an `upload.begin` / `upload.chunk` envelope; returns the reply bytes.
    pub fn handle(&mut self, request: &[u8]) -> Vec<u8> {
        let v: Value = serde_json::from_slice(request).unwrap_or(Value::Null);
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        let reply = match v.get("kind").and_then(|k| k.as_str()).unwrap_or("") {
            "upload.begin" => self.begin(&v, &id),
            "upload.chunk" => self.chunk(&v, &id),
            other => err(&id, &format!("unknown kind: {other}")),
        };
        serde_json::to_vec(&reply).unwrap_or_default()
    }

    fn begin(&mut self, v: &Value, id: &Value) -> Value {
        self.sweep();
        let total = v.get("totalBytes").and_then(|t| t.as_u64()).unwrap_or(0) as usize;
        let mime = v
            .get("mimeType")
            .and_then(|m| m.as_str())
            .unwrap_or("application/octet-stream")
            .to_string();
        // A declared size at/above the whole budget can never succeed — reject it
        // directly. saturating_add then guards against a u64→usize `total` chosen to
        // wrap the sum small (L-srv-1: release builds have no overflow-checks).
        if total > MAX_STAGED_BYTES
            || self.uploads.len() >= MAX_CONCURRENT_UPLOADS
            || self.staged.saturating_add(total) > MAX_STAGED_BYTES
        {
            return err(id, "upload capacity is exhausted — try again shortly");
        }
        let upload_id = rand_hex(16);
        self.uploads.insert(
            upload_id.clone(),
            Upload { mime_type: mime, total, received: 0, chunks: HashMap::new(), at: Instant::now() },
        );
        json!({ "kind": "upload.begin.ok", "id": id, "uploadId": upload_id })
    }

    fn chunk(&mut self, v: &Value, id: &Value) -> Value {
        let uid = v.get("uploadId").and_then(|u| u.as_str()).unwrap_or("").to_string();
        let Some(u) = self.uploads.get_mut(&uid) else {
            return err(id, "unknown or expired uploadId");
        };
        let seq = v.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
        let data = v.get("data").and_then(|d| d.as_str()).unwrap_or("");
        // Bound the encoded text BEFORE decoding, so an oversized chunk is refused
        // without allocating its decoded form (M-srv-2).
        if data.len() > MAX_CHUNK_B64_LEN {
            return err(id, "chunk is too large");
        }
        let Ok(bytes) = B64.decode(data) else {
            return err(id, "chunk is not valid base64");
        };
        // Idempotent: a retried chunk (mixnet loss) replaces rather than double-counts.
        let prev = u.chunks.get(&seq).map(|c| c.len()).unwrap_or(0);
        let new_received = u.received + bytes.len() - prev;
        let new_staged = self.staged + bytes.len() - prev;
        if new_received > u.total || new_staged > MAX_STAGED_BYTES {
            return err(id, "upload exceeds its declared size");
        }
        u.chunks.insert(seq, bytes);
        u.received = new_received;
        self.staged = new_staged;
        u.at = Instant::now();
        json!({ "kind": "upload.chunk.ok", "id": id, "uploadId": uid, "received": u.received })
    }

    fn sweep(&mut self) {
        let now = Instant::now();
        let mut freed = 0usize;
        self.uploads.retain(|_, u| {
            let keep = now.duration_since(u.at) <= TTL;
            if !keep {
                freed += u.received;
            }
            keep
        });
        self.staged -= freed.min(self.staged);
    }

    /// Replace `uploadId` attachment references in a chat's messages with the
    /// reassembled base64 bytes, consuming (freeing) each upload. Errors if a
    /// reference is unknown or incomplete — the chat turns that into an error
    /// reply instead of a half-blind provider call.
    pub fn resolve(&mut self, messages: &mut Value) -> Result<(), String> {
        let Some(msgs) = messages.as_array_mut() else { return Ok(()) };
        for m in msgs {
            let Some(atts) = m.get_mut("attachments").and_then(|a| a.as_array_mut()) else {
                continue;
            };
            for att in atts {
                if att.get("data").and_then(|d| d.as_str()).is_some() {
                    continue; // already inline
                }
                let uid = att
                    .get("uploadId")
                    .and_then(|u| u.as_str())
                    .ok_or("attachment is missing both data and uploadId")?
                    .to_string();
                match self.uploads.get(&uid) {
                    None => return Err("referenced upload is unknown or expired".into()),
                    Some(u) if u.received < u.total => {
                        return Err("referenced upload is incomplete".into())
                    }
                    Some(_) => {}
                }
                let u = self.uploads.remove(&uid).expect("checked above"); // nosemgrep: scrai-unwrap-in-server-hot-path -- proven: existence checked immediately above, no await in between
                self.staged -= u.received.min(self.staged);
                let mut seqs: Vec<&u64> = u.chunks.keys().collect();
                seqs.sort();
                let mut whole = Vec::with_capacity(u.received);
                for s in seqs {
                    whole.extend_from_slice(&u.chunks[s]);
                }
                *att = json!({ "mimeType": u.mime_type, "data": B64.encode(&whole) });
            }
        }
        Ok(())
    }
}

fn err(id: &Value, msg: &str) -> Value {
    json!({ "id": id, "kind": "error", "error": msg })
}

fn rand_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn begin(s: &mut UploadStore, total: usize) -> String {
        let r: Value = serde_json::from_slice(&s.handle(
            json!({"kind":"upload.begin","id":"b1","mimeType":"image/png","totalBytes":total})
                .to_string()
                .as_bytes(),
        ))
        .unwrap();
        r.get("uploadId").and_then(|u| u.as_str()).unwrap().to_string()
    }

    fn chunk(s: &mut UploadStore, uid: &str, seq: u64, data: &[u8]) -> Value {
        serde_json::from_slice(&s.handle(
            json!({"kind":"upload.chunk","id":"c1","uploadId":uid,"seq":seq,"data":B64.encode(data)})
                .to_string()
                .as_bytes(),
        ))
        .unwrap()
    }

    #[test]
    fn upload_reassembles_in_seq_order_and_is_consumed() {
        let mut s = UploadStore::default();
        let uid = begin(&mut s, 6);
        chunk(&mut s, &uid, 1, b"def"); // out of order on purpose
        let r = chunk(&mut s, &uid, 0, b"abc");
        assert_eq!(r.get("received").and_then(|x| x.as_u64()), Some(6));

        let mut msgs = json!([{ "role":"user","content":"look",
            "attachments":[{"mimeType":"image/png","uploadId":uid}] }]);
        s.resolve(&mut msgs).unwrap();
        let att = &msgs[0]["attachments"][0];
        assert_eq!(att["data"], B64.encode(b"abcdef"));
        assert_eq!(att["mimeType"], "image/png");
        assert_eq!(s.staged, 0); // consumed and freed

        // A second resolve of the same (now consumed) id must fail cleanly.
        let mut again = json!([{ "attachments":[{"uploadId":"feedfeed"}] }]);
        assert!(s.resolve(&mut again).is_err());
    }

    #[test]
    fn incomplete_upload_is_refused_but_kept() {
        let mut s = UploadStore::default();
        let uid = begin(&mut s, 10);
        chunk(&mut s, &uid, 0, b"abc");
        let mut msgs = json!([{ "attachments":[{"uploadId":uid}] }]);
        assert_eq!(s.resolve(&mut msgs).unwrap_err(), "referenced upload is incomplete");
        // finishing the upload afterwards still works
        chunk(&mut s, &uid, 1, b"defghij");
        assert!(s.resolve(&mut msgs).is_ok());
    }

    #[test]
    fn retried_chunk_is_idempotent_and_overflow_is_rejected() {
        let mut s = UploadStore::default();
        let uid = begin(&mut s, 4);
        chunk(&mut s, &uid, 0, b"ab");
        let r = chunk(&mut s, &uid, 0, b"ab"); // retry replaces, not double-counts
        assert_eq!(r.get("received").and_then(|x| x.as_u64()), Some(2));
        let r = chunk(&mut s, &uid, 1, b"xyz"); // 2 + 3 > 4 declared
        assert_eq!(r.get("kind").and_then(|k| k.as_str()), Some("error"));
    }

    #[test]
    fn oversized_declared_size_is_refused_without_overflow() {
        // L-srv-1: a totalBytes chosen to wrap `staged + total` small must still be
        // refused, and the huge declared size must be rejected outright — no panic.
        let mut s = UploadStore::default();
        let r: Value = serde_json::from_slice(&s.handle(
            json!({"kind":"upload.begin","id":"b1","mimeType":"image/png","totalBytes": u64::MAX})
                .to_string()
                .as_bytes(),
        ))
        .unwrap();
        assert_eq!(r.get("kind").and_then(|k| k.as_str()), Some("error"));
        assert_eq!(s.staged, 0);
    }

    #[test]
    fn chunk_larger_than_the_ceiling_is_refused_before_decode() {
        // M-srv-2: an encoded chunk past MAX_CHUNK_B64_LEN is rejected on its length,
        // never decoded/allocated.
        let mut s = UploadStore::default();
        let uid = begin(&mut s, 4);
        let huge = "A".repeat(MAX_CHUNK_B64_LEN + 1);
        let r: Value = serde_json::from_slice(&s.handle(
            json!({"kind":"upload.chunk","id":"c1","uploadId":uid,"seq":0,"data":huge})
                .to_string()
                .as_bytes(),
        ))
        .unwrap();
        assert_eq!(r.get("kind").and_then(|k| k.as_str()), Some("error"));
    }

    #[test]
    fn inline_data_attachments_pass_through_untouched() {
        let mut s = UploadStore::default();
        let mut msgs = json!([{ "attachments":[{"mimeType":"image/png","data":"AAAA"}] }]);
        assert!(s.resolve(&mut msgs).is_ok());
        assert_eq!(msgs[0]["attachments"][0]["data"], "AAAA");
    }
}
