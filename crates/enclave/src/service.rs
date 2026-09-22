//! What the enclave answers. Two kinds of message reach it:
//!
//! - `attest {nonce}` — in the clear. The answer is the enclave's public keys and a proof,
//!   bound to those keys and that nonce, of which code holds them.
//! - `sealed {epk, ct}` — everything else, sealed to the attested key (see `wire`). Inside is
//!   a request signed by an account: `op`, `body`, a fresh `nonce`, a timestamp, and the
//!   signature over `nonce:ts:<enclave identity>:<sha256 of body>`, so it is good for one
//!   request to this enclave and nothing else.
//!
//! A sealed request sent twice — the app did not see the first answer — gets the first answer
//! again, byte for byte, and is charged once.

use crate::keys::EnclaveKeys;
use crate::ledger::Ledger;
use crate::provider::{estimate_tokens, ChatRequest, Provider};
use crate::seal::KeyProvider;
use crate::wire::ServerExchange;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokumai_attest::Attester;

/// How far a request's timestamp may be from the enclave's clock.
pub const CLOCK_SKEW_MS: u64 = 5 * 60 * 1000;
/// How long an answer is kept for a resend.
pub const REPLY_KEEP: Duration = Duration::from_secs(30 * 60);
const REPLY_KEEP_MAX: usize = 20_000;

/// TOKU per 1,000 tokens (input, output), per model.
pub type Prices = HashMap<String, (u64, u64)>;

pub enum Db {
    File(PathBuf),
    Memory,
}

/// Everything that differs between a laptop and a real enclave.
pub struct Platform {
    pub attester: Box<dyn Attester>,
    pub keys: Box<dyn KeyProvider>,
    pub provider: Box<dyn Provider>,
    pub db: Db,
    pub prices: Prices,
    /// Development only: allows `dev.*` operations that create credit out of nothing.
    pub dev_mode: bool,
}

pub struct Enclave {
    keys: EnclaveKeys,
    attester: Box<dyn Attester>,
    provider: Box<dyn Provider>,
    ledger: Mutex<Ledger>,
    prices: Prices,
    dev_mode: bool,
    replies: Mutex<HashMap<String, (Instant, Vec<u8>)>>,
}

#[derive(Deserialize)]
struct Inner {
    account: String,
    op: String,
    nonce: String,
    ts: u64,
    sig: String,
    body: String,
}

fn error(msg: &str) -> Value {
    json!({ "kind": "error", "error": msg })
}

impl Enclave {
    pub fn start(p: Platform) -> Result<Enclave, String> {
        let key = p.keys.data_key()?;
        let mut ledger = match p.db {
            Db::File(path) => Ledger::open(&path, key)?,
            Db::Memory => Ledger::in_memory(key)?,
        };
        let released = ledger.release_open_holds()?;
        if released > 0 {
            eprintln!("tokumai-enclave: gave back {released} hold(s) of requests cut off by the last stop");
        }
        Ok(Enclave {
            keys: EnclaveKeys::generate(),
            attester: p.attester,
            provider: p.provider,
            ledger: Mutex::new(ledger),
            prices: p.prices,
            dev_mode: p.dev_mode,
            replies: Mutex::new(HashMap::new()),
        })
    }

    pub fn identity_hex(&self) -> String {
        hex::encode(self.keys.identity_pub())
    }

    /// One message in, one message out. Never panics on input.
    pub async fn handle(&self, raw: &[u8]) -> Vec<u8> {
        let v: Value = serde_json::from_slice(raw).unwrap_or(Value::Null);
        let out = match v.get("kind").and_then(|k| k.as_str()) {
            Some("attest") => self.attest(&v),
            Some("sealed") => return self.sealed(&v).await,
            _ => error("unknown kind"),
        };
        serde_json::to_vec(&out).unwrap_or_default()
    }

    fn attest(&self, v: &Value) -> Value {
        let nonce = match v.get("nonce").and_then(|n| n.as_str()).and_then(|n| hex::decode(n).ok()) {
            Some(n) if (16..=64).contains(&n.len()) => n,
            _ => return error("an attestation request needs a nonce of 16 to 64 bytes, hex"),
        };
        let binding = tokumai_attest::binding(&self.keys.identity_pub(), &self.keys.kx_pub(), &nonce);
        match self.attester.attest(&binding) {
            Ok(evidence) => json!({
                "kind": "attest.ok",
                "identity": hex::encode(self.keys.identity_pub()),
                "kx": hex::encode(self.keys.kx_pub()),
                "evidence": evidence,
            }),
            Err(e) => error(&format!("attestation failed: {e}")),
        }
    }

    async fn sealed(&self, v: &Value) -> Vec<u8> {
        let plain_err = |m: &str| serde_json::to_vec(&error(m)).unwrap_or_default();
        let epk: Option<[u8; 32]> = v.get("epk").and_then(|e| e.as_str()).and_then(|e| hex::decode(e).ok()).and_then(|b| b.try_into().ok());
        let ct = v.get("ct").and_then(|c| c.as_str()).and_then(|c| B64.decode(c).ok());
        let (Some(epk), Some(ct)) = (epk, ct) else { return plain_err("malformed sealed request") };
        // Nothing is said about WHY a request does not open: it is either not for this
        // enclave (another start, other keys — the app re-attests) or it was tampered with.
        let Ok((exchange, plain)) = ServerExchange::open(&self.keys.kx, &epk, &ct) else {
            return plain_err("this request is not sealed to this enclave — attest again");
        };
        let seal = |value: Value| -> Vec<u8> {
            let ct = exchange.seal_response(&serde_json::to_vec(&value).unwrap_or_default());
            serde_json::to_vec(&json!({ "kind": "sealed", "ct": B64.encode(ct) })).unwrap_or_default()
        };
        let Ok(req) = serde_json::from_slice::<Inner>(&plain) else { return seal(error("malformed request")) };
        let now = crate::now_ms();
        if req.ts.abs_diff(now) > CLOCK_SKEW_MS {
            return seal(error("the request's clock is too far from the enclave's — check the device time"));
        }
        let body_hash = hex::encode(tokumai_core::account::sha256(&[req.body.as_bytes()]));
        let signed = format!("{}:{}:{}:{}", req.nonce, req.ts, self.identity_hex(), body_hash);
        let Some(account_id) = tokumai_core::account::account_owns(&req.account, &req.op, &signed, &req.sig) else {
            return seal(error("the account signature does not check out"));
        };
        // A resend of a request already answered gets that answer again.
        let first = match self.ledger.lock() {
            Ok(l) => l.first_sight(&req.nonce, now),
            Err(_) => Err("ledger unavailable".into()),
        };
        match first {
            Ok(true) => {}
            Ok(false) => {
                let cached = self.replies.lock().ok().and_then(|r| r.get(&req.nonce).map(|(_, b)| b.clone()));
                return cached.unwrap_or_else(|| seal(error("this request was already made")));
            }
            Err(e) => return seal(error(&e)),
        }
        let answer = self.dispatch(&account_id, &req.op, &req.body, now).await;
        let out = seal(answer);
        self.remember(&req.nonce, &out);
        out
    }

    fn remember(&self, nonce: &str, out: &[u8]) {
        if let Ok(mut r) = self.replies.lock() {
            r.retain(|_, (at, _)| at.elapsed() < REPLY_KEEP);
            if r.len() >= REPLY_KEEP_MAX {
                if let Some(oldest) = r.iter().min_by_key(|(_, (at, _))| *at).map(|(k, _)| k.clone()) {
                    r.remove(&oldest);
                }
            }
            r.insert(nonce.to_string(), (Instant::now(), out.to_vec()));
        }
    }

    async fn dispatch(&self, account: &str, op: &str, body: &str, now: u64) -> Value {
        match op {
            "balance" => self.balance(account, now),
            "chat" => self.chat(account, body, now).await,
            "dev.credit" | "dev.allowance" if !self.dev_mode => error("not available on this enclave"),
            "dev.credit" => {
                let toku = serde_json::from_str::<Value>(body).ok().and_then(|b| b.get("toku").and_then(|t| t.as_u64())).unwrap_or(0);
                match self.ledger.lock().map_err(|_| "ledger unavailable".to_string()).and_then(|l| l.credit_prepaid(account, toku, now)) {
                    Ok(()) => self.balance(account, now),
                    Err(e) => error(&e),
                }
            }
            "dev.allowance" => {
                let b = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
                let toku = b.get("toku").and_then(|t| t.as_u64()).unwrap_or(0);
                let days = b.get("days").and_then(|t| t.as_u64()).unwrap_or(30);
                match self
                    .ledger
                    .lock()
                    .map_err(|_| "ledger unavailable".to_string())
                    .and_then(|l| l.grant_allowance(account, now, now + days * 86_400_000, toku))
                {
                    Ok(()) => self.balance(account, now),
                    Err(e) => error(&e),
                }
            }
            _ => error("unknown operation"),
        }
    }

    fn balance(&self, account: &str, now: u64) -> Value {
        match self.ledger.lock().map_err(|_| "ledger unavailable".to_string()).and_then(|l| l.balance(account, now)) {
            Ok(b) => json!({ "kind": "balance", "balance": b }),
            Err(e) => error(&e),
        }
    }

    async fn chat(&self, account: &str, body: &str, now: u64) -> Value {
        let Ok(req) = serde_json::from_str::<ChatRequest>(body) else { return error("malformed chat request") };
        let Some(&(per_in, per_out)) = self.prices.get(&req.model) else { return error("that model is not offered") };
        let price = |tin: u64, tout: u64| (tin * per_in + tout * per_out).div_ceil(1000);
        // The worst case is held first; the answer is charged at what it cost.
        let input_estimate: u64 = req.messages.iter().map(|m| estimate_tokens(&m.content)).sum();
        let ceiling = price(input_estimate * 2, req.max_tokens as u64);
        let hold = match self.ledger.lock().map_err(|_| "ledger unavailable".to_string()).and_then(|mut l| l.hold(account, ceiling, now)) {
            Ok(h) => h,
            Err(e) => return error(&e),
        };
        let result = self.provider.complete(&req).await;
        let cost = match &result {
            Ok(c) => price(c.input_tokens, c.output_tokens),
            Err(_) => 0, // a failed answer costs nothing
        };
        let charged = match self.ledger.lock().map_err(|_| "ledger unavailable".to_string()).and_then(|mut l| l.settle(hold, cost)) {
            Ok(k) => k,
            Err(e) => return error(&e),
        };
        match result {
            Ok(c) => {
                let total = self.ledger.lock().ok().and_then(|l| l.balance(account, now).ok()).map(|b| b.total).unwrap_or(0);
                json!({ "kind": "chat", "text": c.text, "cost": charged, "balance": total,
                        "usage": { "input": c.input_tokens, "output": c.output_tokens } })
            }
            Err(e) => error(&format!("the model did not answer: {e}")),
        }
    }
}
