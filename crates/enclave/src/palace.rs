//! The Memory Palace's store: opaque blobs under opaque names, for whoever holds the key a
//! namespace was first written with. What the enclave keeps of a blob is the namespace, the
//! blob's name, its version, the ciphertext, its size and when it was written — nothing
//! else, and nothing of it readable (`tokumai_core::palace`).
//!
//! A palace request is not the account's. It arrives in an envelope signed by a key made
//! for that one request (the app throws it away), and inside it carries its own signature,
//! by the palace's key, over `(namespace, op, blob, version, sha256(ciphertext), ts,
//! nonce)`. The key is registered on the namespace's first write and every later request
//! must be signed by it (trust on first use). A request is good once: its timestamp must
//! be within the enclave's clock window and its nonce new.
//!
//! The operations:
//! - `palace.list`   → `[{blobId, version, size}]`
//! - `palace.get`    → the ciphertext of the blobs asked for
//! - `palace.put`    → write a blob if its version is still `expected`; else
//!   `palace.conflict` with the version it has (the app fetches, merges, writes again)
//! - `palace.delete` → the same rule
//! - `palace.wipe`   → the whole namespace, its key included
//!
//! Neither the account nor the request's envelope key is written anywhere, and the
//! operator's console sees totals only (`admin`).

use crate::policy;
use crate::service::{error, Enclave, CLOCK_SKEW_MS};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::Deserialize;
use serde_json::{json, Value};
use tokumai_core::palace::{ct_hash, verify_request, SignedFields};

#[derive(Deserialize)]
struct Req {
    ns: String,
    #[serde(rename = "pub")]
    public: String,
    ts: u64,
    nonce: String,
    sig: String,
    #[serde(default, rename = "blobId")]
    blob_id: String,
    #[serde(default, rename = "blobIds")]
    blob_ids: Vec<String>,
    #[serde(default)]
    expected: u64,
    #[serde(default)]
    ct: String,
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

impl Enclave {
    pub(crate) fn palace_dispatch(&self, op: &str, body: &str, now: u64) -> Value {
        if body.len() > policy::PALACE_MAX_BLOB_BYTES * 2 {
            return error("this palace request is too large");
        }
        let Ok(r) = serde_json::from_str::<Req>(body) else { return error("malformed palace request") };
        if !is_hex64(&r.ns) || !is_hex64(&r.public) {
            return error("malformed palace request");
        }
        if r.ts.abs_diff(now) > CLOCK_SKEW_MS {
            return error("the request's clock is too far from the enclave's — check the device time");
        }
        if r.nonce.len() < 16 || r.nonce.len() > 64 {
            return error("malformed palace request");
        }
        let short = op.strip_prefix("palace.").unwrap_or(op);
        let ct = if short == "put" {
            match B64.decode(r.ct.as_bytes()) {
                Ok(c) if c.len() <= policy::PALACE_MAX_BLOB_BYTES => c,
                Ok(_) => return error("a palace blob may be 256 KB at most"),
                Err(_) => return error("malformed palace request"),
            }
        } else {
            Vec::new()
        };
        let joined;
        let (blob, version) = match short {
            "list" | "wipe" => ("", 0),
            "get" => {
                if r.blob_ids.is_empty() || r.blob_ids.len() > policy::PALACE_MAX_GET || !r.blob_ids.iter().all(|b| is_hex64(b)) {
                    return error("malformed palace request");
                }
                joined = r.blob_ids.join(",");
                (joined.as_str(), 0)
            }
            "put" => (r.blob_id.as_str(), r.expected + 1),
            "delete" => (r.blob_id.as_str(), r.expected),
            _ => return error("unknown operation"),
        };
        if matches!(short, "put" | "delete") && !is_hex64(blob) {
            return error("malformed palace request");
        }
        let hash = ct_hash(&ct);
        let fields = SignedFields { op: short, blob_id: blob, version, ct_sha256: &hash, ts: r.ts, nonce: &r.nonce };
        if !verify_request(&r.public, &r.ns, &fields, &r.sig) {
            return error("the palace signature does not check out");
        }
        let Ok(ledger) = self.ledger.lock() else { return error("ledger unavailable") };
        // Good once. The nonces are kept in memory only, like the envelope's (`ledger`).
        match ledger.first_sight(&format!("palace:{}", r.nonce), now) {
            Ok(true) => {}
            Ok(false) => return error("this palace request was already made"),
            Err(e) => return error(&e),
        }
        let registered = match ledger.palace_pub(&r.ns) {
            Ok(p) => p,
            Err(e) => return error(&e),
        };
        match &registered {
            Some(p) if p != &r.public => return error("this palace belongs to another key"),
            _ => {}
        }
        self.stats.add(&format!("palace:{short}"), 1);
        match short {
            "list" => {
                let blobs = if registered.is_some() { ledger.palace_list(&r.ns).unwrap_or_default() } else { Vec::new() };
                let used: u64 = blobs.iter().map(|b| b.2).sum();
                json!({
                    "kind": "palace.list",
                    "blobs": blobs.iter().map(|(b, v, s)| json!({ "blobId": b, "version": v, "size": s })).collect::<Vec<_>>(),
                    "used": used, "quota": policy::PALACE_MAX_BYTES,
                })
            }
            "get" => {
                let mut out = Vec::new();
                if registered.is_some() {
                    for b in &r.blob_ids {
                        if let Ok(Some((v, ct))) = ledger.palace_get(&r.ns, b) {
                            out.push(json!({ "blobId": b, "version": v, "ct": ct }));
                        }
                    }
                }
                json!({ "kind": "palace.get", "blobs": out })
            }
            "put" => {
                let current = match ledger.palace_version(&r.ns, blob) {
                    Ok(v) => v,
                    Err(e) => return error(&e),
                };
                if current != r.expected {
                    return json!({ "kind": "palace.conflict", "blobId": blob, "version": current });
                }
                let (blobs, bytes) = ledger.palace_usage(&r.ns).unwrap_or((0, 0));
                let old = ledger.palace_get(&r.ns, blob).ok().flatten().map(|(_, c)| B64.decode(c).map(|b| b.len() as u64).unwrap_or(0)).unwrap_or(0);
                let after = bytes - old.min(bytes) + ct.len() as u64;
                if after > policy::PALACE_MAX_BYTES || (current == 0 && blobs + 1 > policy::PALACE_MAX_BLOBS) {
                    return json!({ "kind": "error", "error": "the Memory Palace is full — delete some memories first", "quota": true });
                }
                let (_, _, total) = ledger.palace_totals().unwrap_or((0, 0, 0));
                if total + ct.len() as u64 > policy::PALACE_TOTAL_BYTES {
                    return json!({ "kind": "error", "error": "memory storage is full right now — your memory is safe on this device", "quota": true });
                }
                if registered.is_none() {
                    if let Err(e) = ledger.palace_register(&r.ns, &r.public) {
                        return error(&e);
                    }
                }
                match ledger.palace_put(&r.ns, blob, current + 1, &r.ct, ct.len() as u64, now) {
                    Ok(()) => json!({ "kind": "palace.put", "blobId": blob, "version": current + 1 }),
                    Err(e) => error(&e),
                }
            }
            "delete" => {
                let current = match ledger.palace_version(&r.ns, blob) {
                    Ok(v) => v,
                    Err(e) => return error(&e),
                };
                if current == 0 {
                    return json!({ "kind": "palace.delete", "blobId": blob });
                }
                if current != r.expected {
                    return json!({ "kind": "palace.conflict", "blobId": blob, "version": current });
                }
                match ledger.palace_delete(&r.ns, blob) {
                    Ok(()) => json!({ "kind": "palace.delete", "blobId": blob }),
                    Err(e) => error(&e),
                }
            }
            "wipe" => match ledger.palace_wipe(&r.ns) {
                Ok(()) => json!({ "kind": "palace.wipe" }),
                Err(e) => error(&e),
            },
            _ => error("unknown operation"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{Db, Platform};
    use tokumai_core::palace::PalaceKeys;

    fn enclave() -> Enclave {
        Enclave::start(Platform {
            attester: Box::new(tokumai_attest::sim::SimAttester::new([5; 32], "image-1")),
            keys: Box::new(crate::seal::FixedKeyProvider([9; 32])),
            providers: crate::provider::Providers::mock(),
            db: Db::Memory,
            pricing: tokumai_core::pricing::PricingTable::parse(crate::policy::PRICING_JSON).unwrap(),
            dev_mode: false,
            stripe: None,
            apple_api: None,
            witness: None,
            admin: None,
        })
        .unwrap()
    }

    const M: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art";

    fn nonce() -> String {
        tokumai_core::palace::new_id("n_n_n_n_")
    }

    fn req(k: &PalaceKeys, op: &str, blob: &str, blobs: &[String], expected: u64, ct: &[u8], now: u64) -> String {
        let n = nonce();
        let joined = blobs.join(",");
        let (b, v) = match op {
            "get" => (joined.as_str(), 0),
            "put" => (blob, expected + 1),
            "delete" => (blob, expected),
            _ => ("", 0),
        };
        let hash = ct_hash(ct);
        let sig = k.sign_request(&SignedFields { op, blob_id: b, version: v, ct_sha256: &hash, ts: now, nonce: &n });
        json!({ "ns": k.namespace(), "pub": k.sign_pub_hex(), "ts": now, "nonce": n, "sig": sig,
                "blobId": blob, "blobIds": blobs, "expected": expected, "ct": B64.encode(ct) })
        .to_string()
    }

    #[test]
    fn blobs_are_written_listed_read_and_versioned() {
        let e = enclave();
        let now = crate::now_ms();
        let k = PalaceKeys::from_mnemonic(M).unwrap();
        let blob = k.blob_id("manifest");
        // An empty palace lists nothing.
        assert_eq!(e.palace_dispatch("palace.list", &req(&k, "list", "", &[], 0, b"", now), now)["blobs"], json!([]));
        let ct = k.seal(&blob, 1, b"{}");
        let put = e.palace_dispatch("palace.put", &req(&k, "put", &blob, &[], 0, &ct, now), now);
        assert_eq!(put["version"], 1, "{put}");
        // A stale write is a conflict, with the version there is.
        let stale = e.palace_dispatch("palace.put", &req(&k, "put", &blob, &[], 0, &ct, now), now);
        assert_eq!((stale["kind"].as_str(), stale["version"].as_u64()), (Some("palace.conflict"), Some(1)));
        let list = e.palace_dispatch("palace.list", &req(&k, "list", "", &[], 0, b"", now), now);
        assert_eq!(list["blobs"][0]["blobId"], blob);
        assert_eq!(list["blobs"][0]["version"], 1);
        let got = e.palace_dispatch("palace.get", &req(&k, "get", "", std::slice::from_ref(&blob), 0, b"", now), now);
        let back = B64.decode(got["blobs"][0]["ct"].as_str().unwrap()).unwrap();
        assert_eq!(k.open(&blob, 1, &back).unwrap(), b"{}");
        // Delete at the right version, then the namespace is empty.
        let del = e.palace_dispatch("palace.delete", &req(&k, "delete", &blob, &[], 1, b"", now), now);
        assert_eq!(del["kind"], "palace.delete");
        assert_eq!(e.palace_dispatch("palace.list", &req(&k, "list", "", &[], 0, b"", now), now)["blobs"], json!([]));
    }

    #[test]
    fn only_the_first_key_may_write_and_a_request_is_good_once() {
        let e = enclave();
        let now = crate::now_ms();
        let k = PalaceKeys::from_mnemonic(M).unwrap();
        let blob = k.blob_id("f_1");
        let body = req(&k, "put", &blob, &[], 0, b"0123456789abcdef0123456789", now);
        assert_eq!(e.palace_dispatch("palace.put", &body, now)["kind"], "palace.put");
        assert_eq!(e.palace_dispatch("palace.put", &body, now)["kind"], "error", "a replay is refused");
        // Another key claiming the same namespace name.
        let other = PalaceKeys::from_mnemonic(&tokumai_core::account::create_account().mnemonic).unwrap();
        let mut forged: Value = serde_json::from_str(&req(&other, "list", "", &[], 0, b"", now)).unwrap();
        forged["ns"] = json!(k.namespace());
        let n = forged["nonce"].as_str().unwrap().to_string();
        let h = ct_hash(b"");
        let msg = SignedFields { op: "list", blob_id: "", version: 0, ct_sha256: &h, ts: now, nonce: &n };
        // Signed correctly for the namespace, by the wrong key.
        forged["sig"] = json!(other.sign_request(&msg));
        let a = e.palace_dispatch("palace.list", &forged.to_string(), now);
        assert_eq!(a["kind"], "error", "{a}");
        // An old timestamp.
        let old = req(&k, "list", "", &[], 0, b"", now - CLOCK_SKEW_MS - 1);
        assert_eq!(e.palace_dispatch("palace.list", &old, now)["kind"], "error");
        // A signature over other fields.
        let mut bent: Value = serde_json::from_str(&req(&k, "put", &blob, &[], 1, b"abc", now)).unwrap();
        bent["expected"] = json!(5);
        assert_eq!(e.palace_dispatch("palace.put", &bent.to_string(), now)["kind"], "error");
    }

    #[test]
    fn the_book_holds_only_opaque_names_and_ciphertext() {
        let e = enclave();
        let now = crate::now_ms();
        let k = PalaceKeys::from_mnemonic(M).unwrap();
        let blob = k.blob_id("f_work");
        let plain = br#"{"schema":"tokumai/palace-file@1","id":"f_work","lines":[{"id":"l_1","text":"Works at [EMPLOYER_1] as head of a VFX studio"}]}"#;
        let ct = k.seal(&blob, 1, plain);
        e.palace_dispatch("palace.put", &req(&k, "put", &blob, &[], 0, &ct, now), now);
        let dump = e.ledger.lock().unwrap().rows_for_tests().to_string();
        for leak in ["f_work", "VFX", "EMPLOYER", "Work", "palace-file"] {
            assert!(!dump.contains(leak), "{leak} is in the book");
        }
        let (namespaces, blobs, bytes) = e.ledger.lock().unwrap().palace_totals().unwrap();
        assert_eq!((namespaces, blobs, bytes), (1, 1, ct.len() as u64));
        // Wiped: nothing left, the key included.
        e.palace_dispatch("palace.wipe", &req(&k, "wipe", "", &[], 0, b"", now), now);
        assert_eq!(e.ledger.lock().unwrap().palace_totals().unwrap(), (0, 0, 0));
    }

    #[test]
    fn a_namespace_has_a_quota() {
        let e = enclave();
        let now = crate::now_ms();
        let k = PalaceKeys::from_mnemonic(M).unwrap();
        let big = vec![7u8; policy::PALACE_MAX_BLOB_BYTES];
        let mut refused = false;
        for i in 0..((policy::PALACE_MAX_BYTES as usize / big.len()) + 2) {
            let blob = k.blob_id(&format!("f_{i}"));
            let a = e.palace_dispatch("palace.put", &req(&k, "put", &blob, &[], 0, &big, now), now);
            if a["quota"] == true {
                refused = true;
                break;
            }
        }
        assert!(refused);
        let too_big = vec![7u8; policy::PALACE_MAX_BLOB_BYTES + 1];
        assert_eq!(e.palace_dispatch("palace.put", &req(&k, "put", &k.blob_id("x"), &[], 0, &too_big, now), now)["kind"], "error");
    }
}
