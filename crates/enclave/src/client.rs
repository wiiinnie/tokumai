//! The app's side of talking to the enclave, independent of how the bytes travel (mixnet in
//! the app, TCP in development, a function call in tests). This is what moves into the app.
//!
//! 1. [`attest_request`] with a fresh nonce → the enclave's answer → [`Session::from_attestation`]
//!    checks the proof against the app's [`Policy`] and keeps the attested keys.
//! 2. [`Session::request`] signs and seals one request; [`Pending::open`] opens its answer.
//!    If the answer is lost, send the same bytes again: it is answered once and charged once.

use crate::wire::ClientExchange;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::{json, Value};
use tokumai_attest::{Claims, Evidence, Policy};
use tokumai_core::account::Account;

pub fn attest_request(nonce: &[u8; 32]) -> Vec<u8> {
    serde_json::to_vec(&json!({ "kind": "attest", "nonce": hex::encode(nonce) })).unwrap_or_default()
}

pub struct Session {
    identity: [u8; 32],
    kx: [u8; 32],
    pub claims: Claims,
}

fn key32(v: &Value, field: &str) -> Result<[u8; 32], String> {
    v.get(field)
        .and_then(|x| x.as_str())
        .and_then(|x| hex::decode(x).ok())
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| format!("the enclave's answer has no {field} key"))
}

impl Session {
    /// Accept the enclave only if its proof passes `policy` and vouches for exactly the keys
    /// it presented, for exactly this nonce.
    pub fn from_attestation(reply: &[u8], nonce: &[u8; 32], policy: &Policy) -> Result<Session, String> {
        let v: Value = serde_json::from_slice(reply).map_err(|_| "unreadable attestation answer")?;
        if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
            return Err(e.to_string());
        }
        let identity = key32(&v, "identity")?;
        let kx = key32(&v, "kx")?;
        let evidence: Evidence = serde_json::from_value(v.get("evidence").cloned().unwrap_or(Value::Null))
            .map_err(|_| "the enclave sent no proof")?;
        let claims = tokumai_attest::verify(&evidence, policy, &tokumai_attest::binding(&identity, &kx, nonce))?;
        Ok(Session { identity, kx, claims })
    }

    /// Sign `body` as `account` for operation `op`, and seal it to the enclave.
    pub fn request(&self, account: &Account, op: &str, body: &Value, now_ms: u64) -> (Pending, Vec<u8>) {
        let body = body.to_string();
        let nonce = hex::encode(rand::random::<[u8; 16]>());
        let body_hash = hex::encode(tokumai_core::account::sha256(&[body.as_bytes()]));
        let signed = format!("{nonce}:{now_ms}:{}:{body_hash}", hex::encode(self.identity));
        let inner = json!({
            "account": account.public_key_pem, "op": op, "nonce": nonce, "ts": now_ms,
            "sig": account.sign(op, &signed), "body": body,
        });
        let (ex, ct) = ClientExchange::seal(&self.kx, inner.to_string().as_bytes());
        let outer = json!({ "kind": "sealed", "epk": hex::encode(ex.epk), "ct": B64.encode(ct) });
        (Pending { ex }, serde_json::to_vec(&outer).unwrap_or_default())
    }
}

pub struct Pending {
    ex: ClientExchange,
}

impl Pending {
    pub fn open(&self, reply: &[u8]) -> Result<Value, String> {
        let v: Value = serde_json::from_slice(reply).map_err(|_| "unreadable answer")?;
        match v.get("kind").and_then(|k| k.as_str()) {
            Some("sealed") => {
                let ct = v.get("ct").and_then(|c| c.as_str()).and_then(|c| B64.decode(c).ok()).ok_or("malformed answer")?;
                let plain = self.ex.open_response(&ct)?;
                serde_json::from_slice(&plain).map_err(|_| "unreadable answer".to_string())
            }
            _ => Err(v.get("error").and_then(|e| e.as_str()).unwrap_or("unexpected answer").to_string()),
        }
    }
}
