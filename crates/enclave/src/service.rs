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
use crate::policy;
use crate::provider::{Call, ChatRequest, Providers};
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
use tokumai_core::billing::{ceil_toku, ceiling_toku, compute_billing, per_image_toku, Ceiling, TOKU_PER_USD};
use tokumai_core::pricing::PricingTable;

/// How far a request's timestamp may be from the enclave's clock.
pub const CLOCK_SKEW_MS: u64 = 5 * 60 * 1000;
/// How long an answer is kept for a resend.
pub const REPLY_KEEP: Duration = Duration::from_secs(30 * 60);
const REPLY_KEEP_MAX: usize = 20_000;

pub enum Db {
    File(PathBuf),
    Memory,
}

/// Everything that differs between a laptop and a real enclave.
pub struct Platform {
    pub attester: Box<dyn Attester>,
    pub keys: Box<dyn KeyProvider>,
    pub providers: Providers,
    pub db: Db,
    /// Normally `PricingTable::parse(policy::PRICING_JSON)` — the list published with the image.
    pub pricing: PricingTable,
    /// Development only: allows `dev.*` operations that create credit out of nothing.
    pub dev_mode: bool,
    /// Plans by card, when Stripe is configured.
    pub stripe: Option<crate::stripe::Stripe>,
    /// The App Store Server API, for the renewal check (App Store plans still verify
    /// without it — from what the app hands over).
    pub apple_api: Option<crate::apple::AppleApi>,
}

pub struct Enclave {
    keys: EnclaveKeys,
    attester: Box<dyn Attester>,
    providers: Providers,
    pub(crate) ledger: Mutex<Ledger>,
    pricing: PricingTable,
    dev_mode: bool,
    pub(crate) stripe: Option<crate::stripe::Stripe>,
    pub(crate) apple_api: Option<crate::apple::AppleApi>,
    /// What the six plans cost at Stripe (cents), and when that was last read.
    pub(crate) plan_prices: Mutex<(Vec<u64>, u64)>,
    replies: Mutex<HashMap<String, (Instant, Vec<u8>)>>,
    /// Secret behind the per-account pseudonyms sent to providers; derived from the data
    /// key, so it survives restarts and is known only inside.
    safety_salt: [u8; 32],
    /// Declines per (account, UTC day, provider). In memory: a restart forgives, which is
    /// the lenient side.
    strikes: Mutex<HashMap<(String, u64, &'static str), u32>>,
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

pub(crate) fn error(msg: &str) -> Value {
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
            providers: p.providers,
            ledger: Mutex::new(ledger),
            pricing: p.pricing,
            dev_mode: p.dev_mode,
            replies: Mutex::new(HashMap::new()),
            safety_salt: tokumai_core::account::sha256(&[b"tokumai/safety/v1", &key]),
            strikes: Mutex::new(HashMap::new()),
            stripe: p.stripe,
            apple_api: p.apple_api,
            plan_prices: Mutex::new((Vec::new(), 0)),
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
            "plans" => self.plans_op(account, now),
            "plan.create" => self.plan_create(account, body, now).await,
            "plan.status" => self.plan_status(account, body, now).await,
            "plan.change" => self.plan_change(account, body, now).await,
            "iap.verify" => self.iap_verify(account, body, now),
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
            Ok(b) => json!({ "kind": "balance", "balance": b, "plan": self.plan_summary(account) }),
            Err(e) => error(&e),
        }
    }

    /// A per-account pseudonym for today: lets OpenAI act on one user's abuse instead of
    /// the whole key, without ever learning the account, and different tomorrow.
    fn safety_id(&self, account: &str, day: u64) -> String {
        hex::encode(tokumai_core::account::sha256(&[&self.safety_salt, day.to_string().as_bytes(), account.as_bytes()]))[..24].to_string()
    }

    fn strike_count(&self, key: &(String, u64, &'static str)) -> u32 {
        self.strikes.lock().ok().and_then(|s| s.get(key).copied()).unwrap_or(0)
    }

    fn strike(&self, key: (String, u64, &'static str)) {
        if let Ok(mut s) = self.strikes.lock() {
            let day = key.1;
            s.retain(|k, _| k.1 >= day);
            *s.entry(key).or_insert(0) += 1;
        }
    }

    async fn chat(&self, account: &str, body: &str, now: u64) -> Value {
        if body.len() > policy::MAX_REQUEST_BYTES {
            return error("this request is too large");
        }
        let Ok(req) = serde_json::from_str::<ChatRequest>(body) else { return error("malformed chat request") };
        let Some(messages) = req.messages.as_array() else { return error("malformed chat request") };
        if messages.is_empty() || messages.len() > policy::MAX_MESSAGES {
            return error("a chat request needs between 1 and 2,000 messages");
        }
        // Worst-case input: bytes of text (a token is at least a byte) plus what each
        // attachment can cost. Only types the providers read are passed on.
        let mut in_tokens: u64 = 0;
        for m in messages {
            in_tokens += m.get("content").and_then(|c| c.as_str()).map(|c| c.len() as u64).unwrap_or(0);
            for att in m.get("attachments").and_then(|a| a.as_array()).map(Vec::as_slice).unwrap_or(&[]) {
                let mime = att.get("mimeType").and_then(|x| x.as_str()).unwrap_or("");
                let b64 = att.get("data").and_then(|x| x.as_str()).unwrap_or("");
                if !policy::ATTACHMENT_TYPES.contains(&mime) {
                    return error(&format!("attachments of type {mime:?} are not supported — pictures and PDFs are"));
                }
                let bytes = b64.len() / 4 * 3;
                if bytes > policy::MAX_ATTACHMENT_BYTES {
                    return error("an attachment is larger than 10 MB");
                }
                // A PDF is read page by page; reserve as if every 40 bytes were a token.
                in_tokens += if mime == "application/pdf" { (bytes as u64 / 40).max(4096) } else { policy::IMAGE_INPUT_TOKENS };
            }
        }
        let Some(provider) = self.providers.find(&req.model) else { return error("that model is not offered") };
        let price = self.pricing.price(&req.model);
        if price.fallback && !self.dev_mode {
            return error("that model is not offered");
        }
        let day = now / 86_400_000;
        let strike_key = (account.to_string(), day, provider.name());
        if self.strike_count(&strike_key) >= policy::STRIKES_PER_DAY {
            return error("this account has had too many requests declined by this provider today — it can be used again tomorrow");
        }
        let image_size = crate::gemini::effective_image_size(&req.model, req.image_size.as_deref());
        let search_usd = if crate::openai::is_openai_model(&req.model) { policy::OPENAI_USD_PER_QUERY } else { policy::GEMINI_USD_PER_QUERY };
        let search_charge = |queries: u64| -> u64 {
            if queries == 0 {
                return 0;
            }
            (ceil_toku(queries as f64 * search_usd * TOKU_PER_USD as f64) * policy::MARGIN).ceil() as u64
        };
        let ceiling = ceiling_toku(
            &price,
            policy::MARGIN,
            &Ceiling {
                in_tokens,
                out_tokens: req.answer_tokens() + req.thinking_tokens(),
                image_tokens: crate::gemini::image_tokens(&req.model, image_size),
            },
        ) + per_image_toku(&price, policy::MARGIN)
            + if req.live { search_charge(policy::MAX_SEARCH_QUERIES) } else { 0 };
        let hold = match self.ledger.lock().map_err(|_| "ledger unavailable".to_string()).and_then(|mut l| l.hold(account, ceiling.max(1), now)) {
            Ok(h) => h,
            Err(e) => return error(&e),
        };
        // The moderation check runs before the model is asked; a flagged turn costs nothing.
        let flagged = self.providers.moderate(&req.messages).await;
        let result = match flagged {
            Ok(Some(cats)) => Err(format!("Declined by the moderation check ({cats})")),
            _ => {
                let call = Call { req: &req, safety_id: Some(self.safety_id(account, day)), image_size };
                provider.complete(&call).await
            }
        };
        let cost = match &result {
            Ok(c) => {
                let images = c.images.as_ref().and_then(|i| i.as_array()).map(|a| a.len() as u64).unwrap_or(0);
                compute_billing(&price, &c.usage, policy::MARGIN, policy::MIN_CHARGE_TOKU, c.usage.estimated).price_toku
                    + images * per_image_toku(&price, policy::MARGIN)
                    + search_charge(c.usage.grounding_queries)
            }
            Err(_) => 0, // a refused or failed answer costs nothing
        };
        let charged = match self.ledger.lock().map_err(|_| "ledger unavailable".to_string()).and_then(|mut l| l.settle(hold, cost)) {
            Ok(k) => k,
            Err(e) => return error(&e),
        };
        let declined = match &result {
            Ok(c) => c.text.starts_with("Declined by"),
            Err(e) => e.starts_with("Declined by"),
        };
        if declined {
            self.strike(strike_key);
        }
        match result {
            Ok(c) => {
                let total = self.ledger.lock().ok().and_then(|l| l.balance(account, now).ok()).map(|b| b.total).unwrap_or(0);
                json!({
                    "kind": "chat", "text": c.text, "images": c.images, "cost": charged, "balance": total,
                    "estimated": c.usage.estimated,
                    "usage": { "input": c.usage.input, "cachedInput": c.usage.cached_input, "output": c.usage.output,
                               "image": c.usage.output_image, "searches": c.usage.grounding_queries,
                               "imageSize": crate::gemini::is_image_model(&req.model).then_some(image_size) },
                })
            }
            Err(e) => error(&e),
        }
    }
}
