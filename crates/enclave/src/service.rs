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
use tokumai_proto::wire::ServerExchange;
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
/// And at most this many bytes of them: a picture's reply is megabytes, and twenty thousand
/// of those would be the enclave's whole memory. The oldest go first.
const REPLY_KEEP_BYTES: usize = 64 * 1024 * 1024;

pub enum Db {
    File(PathBuf),
    Memory,
    /// The book kept on the host, sealed and replayed at every start — what an enclave
    /// with no disk of its own does (`state`).
    Kept(std::sync::Arc<dyn crate::state::Store>),
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
    /// The witness outside the machine that notices a rewound book (`witness`). None on
    /// a developer's machine and in the tests.
    pub witness: Option<crate::witness::Setup>,
    /// The operator's account id: the one `admin.*` answers (`admin`). Named in the image.
    pub admin: Option<String>,
}

pub struct Enclave {
    /// How many changes the book replayed at this start (`state`) — said out loud, since
    /// nobody can look inside a running enclave.
    pub(crate) replayed: usize,
    keys: EnclaveKeys,
    attester: Box<dyn Attester>,
    /// Behind an Arc so a decoy call can outlive the question it covers (see `cover`).
    pub(crate) providers: std::sync::Arc<Providers>,
    pub(crate) ledger: Mutex<Ledger>,
    pub(crate) pricing: PricingTable,
    pub(crate) dev_mode: bool,
    pub(crate) stripe: Option<crate::stripe::Stripe>,
    pub(crate) apple_api: Option<crate::apple::AppleApi>,
    /// Cover for the first question after a payment (see `cover`).
    pub(crate) cover: crate::cover::Cover,
    /// What the six plans cost at Stripe (cents), and when that was last read.
    pub(crate) plan_prices: Mutex<(Vec<u64>, u64)>,
    pub(crate) address: Mutex<String>,
    pub(crate) replies: Mutex<HashMap<String, (Instant, Vec<u8>)>>,
    /// Secret behind the per-account pseudonyms sent to providers; derived from the data
    /// key, so it survives restarts and is known only inside.
    safety_salt: [u8; 32],
    /// Declines per (account, UTC day, provider). In memory: a restart forgives, which is
    /// the lenient side.
    pub(crate) strikes: Mutex<HashMap<(String, u64, &'static str), u32>>,
    /// The blind notes' month keys (`notes`), derived from the data key.
    pub(crate) notes: crate::notes::Mint,
    /// What a ghost request is signed with between the doors (`ghost`): derived from the
    /// data key, so every door of this enclave has it and nobody else does.
    pub(crate) ghost_key: [u8; 32],
    /// The operator's account id (`admin`).
    pub(crate) admin: Option<String>,
    /// What is counted for the operator (`admin`).
    pub(crate) stats: crate::admin::Stats,
    /// Requests being worked on right now.
    pub(crate) working: std::sync::atomic::AtomicUsize,
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

/// How many pages a PDF has, by counting its page objects — a heuristic (the trailer may
/// lie, pages may be in object streams), never less than one, used only to size the hold.
fn pdf_pages(b64: &str) -> u64 {
    let Ok(bytes) = B64.decode(b64.trim()) else { return 1 };
    let mut n = 0u64;
    for needle in [&b"/Type /Page"[..], &b"/Type/Page"[..]] {
        let mut at = 0;
        while let Some(i) = bytes[at..].windows(needle.len()).position(|w| w == needle) {
            let end = at + i + needle.len();
            // "/Type /Pages" is the tree, not a page.
            if bytes.get(end) != Some(&b's') {
                n += 1;
            }
            at = end;
        }
    }
    n.max(1)
}

pub(crate) fn error(msg: &str) -> Value {
    json!({ "kind": "error", "error": msg })
}

impl Enclave {
    /// How the ledger names an account: a keyed hash, never the account id itself.
    pub(crate) fn account_key(&self, account: &str) -> String {
        self.ledger.lock().map(|l| l.acct_key(account)).unwrap_or_default()
    }

    pub fn replayed(&self) -> usize {
        self.replayed
    }

    pub fn start(p: Platform) -> Result<Enclave, String> {
        let key = p.keys.data_key()?;
        let mut ledger = match p.db {
            Db::File(path) => Ledger::open(&path, key)?,
            Db::Memory => Ledger::in_memory(key)?,
            Db::Kept(store) => Ledger::open_sealed(store, key, p.witness)?,
        };
        let replayed = ledger.replayed();
        if replayed > 0 {
            eprintln!("tokumai-enclave: the book came back with {replayed} change(s) replayed");
        }
        let released = ledger.release_open_holds()?;
        if released > 0 {
            eprintln!("tokumai-enclave: gave back {released} hold(s) of requests cut off by the last stop");
        }
        let enclave = Enclave {
            replayed,
            keys: EnclaveKeys::generate(),
            attester: p.attester,
            providers: std::sync::Arc::new(p.providers),
            ledger: Mutex::new(ledger),
            pricing: p.pricing,
            dev_mode: p.dev_mode,
            cover: Default::default(),
            replies: Mutex::new(HashMap::new()),
            safety_salt: tokumai_core::account::sha256(&[b"tokumai/safety/v1", &key]),
            strikes: Mutex::new(HashMap::new()),
            stripe: p.stripe,
            apple_api: p.apple_api,
            plan_prices: Mutex::new((Vec::new(), 0)),
            address: Mutex::new(String::new()),
            notes: crate::notes::Mint::new(key),
            ghost_key: tokumai_core::account::sha256(&[b"tokumai/ghost/v1", &key]),
            admin: p.admin,
            stats: crate::admin::Stats::default(),
            working: std::sync::atomic::AtomicUsize::new(0),
        };
        // This month's keys before the first attestation asks for them.
        enclave.notes.warm(crate::now_ms());
        Ok(enclave)
    }

    /// The address the enclave's own transport listens at, once it is up (its Nym address).
    /// It goes into every attestation, so it must be the address of a client running in here.
    ///
    /// An enclave may have several front doors (one per gateway, for redundancy); this is
    /// the one it names when a request does not say which door it came through — the
    /// simulator and the tests, where there is only one.
    pub fn set_address(&self, address: &str) {
        if let Ok(mut a) = self.address.lock() {
            *a = address.to_string();
        }
    }

    fn address(&self) -> String {
        self.address.lock().map(|a| a.clone()).unwrap_or_default()
    }

    pub fn identity_hex(&self) -> String {
        hex::encode(self.keys.identity_pub())
    }

    /// One message in, one message out. Never panics on input.
    pub async fn handle(&self, raw: &[u8]) -> Vec<u8> {
        self.handle_at(raw, "").await
    }

    /// The same, saying which of the enclave's front doors the message arrived at. The
    /// proof names THAT door — the app checks it against the address it dialled, which is
    /// what makes a relay standing in front of the enclave visible. Naming one fixed door
    /// would make every other door's proof fail, which is the trap here.
    pub async fn handle_at(&self, raw: &[u8], arrived_at: &str) -> Vec<u8> {
        let v: Value = serde_json::from_slice(raw).unwrap_or(Value::Null);
        let out = match v.get("kind").and_then(|k| k.as_str()) {
            Some("attest") => self.attest(&v, arrived_at),
            Some("sealed") => return self.sealed(&v).await,
            _ => error("unknown kind"),
        };
        serde_json::to_vec(&out).unwrap_or_default()
    }

    fn attest(&self, v: &Value, arrived_at: &str) -> Value {
        let nonce = match v.get("nonce").and_then(|n| n.as_str()).and_then(|n| hex::decode(n).ok()) {
            Some(n) if (16..=64).contains(&n.len()) => n,
            _ => return error("an attestation request needs a nonce of 16 to 64 bytes, hex"),
        };
        let address = if arrived_at.is_empty() { self.address() } else { arrived_at.to_string() };
        // The month keys of the blind notes travel with the proof and are bound by it, so
        // every app sees the same keys or fails this check (`notes`).
        let published = match self.notes.published(crate::now_ms()) {
            Ok(p) => p,
            Err(e) => return error(&format!("the month keys are not available: {e}")),
        };
        let digest = tokumai_core::notes::keys_digest(&published);
        let binding = tokumai_attest::binding_with(&self.keys.identity_pub(), &self.keys.kx_pub(), &address, &nonce, &digest);
        let notes: Vec<Value> = published.iter().map(|(e, k)| json!({ "epoch": e, "key": B64.encode(k) })).collect();
        match self.attester.attest(&binding) {
            Ok(evidence) => json!({
                "kind": "attest.ok",
                "identity": hex::encode(self.keys.identity_pub()),
                "kx": hex::encode(self.keys.kx_pub()),
                "address": address,
                "evidence": evidence,
                "notes": notes,
            }),
            Err(e) => error(&format!("attestation failed: {e}")),
        }
    }

    /// One sealed request: opened, checked, answered — and SEALED ONCE. The response key
    /// is derived from the request's ephemeral key, so every byte sealed under it must be
    /// the one answer: `seal` below consumes the exchange, and a request that was seen
    /// before is answered from the cache or with a plain error, never with a second seal
    /// (until 2026-10-06 a resend past the cache got a second message under the same key
    /// and, in v1, the same nonce — audit H1).
    async fn sealed(&self, v: &Value) -> Vec<u8> {
        let plain_err = |m: &str| serde_json::to_vec(&error(m)).unwrap_or_default();
        let epk: Option<[u8; 32]> = v.get("epk").and_then(|e| e.as_str()).and_then(|e| hex::decode(e).ok()).and_then(|b| b.try_into().ok());
        let ct = v.get("ct").and_then(|c| c.as_str()).and_then(|c| B64.decode(c).ok());
        let version = v.get("v").and_then(|v| v.as_u64()).unwrap_or(1);
        let (Some(epk), Some(ct)) = (epk, ct) else { return plain_err("malformed sealed request") };
        // The same bytes again — a reply lost in the mixnet — get the same reply, before
        // anything is opened: the ciphertext names the request better than any nonce in
        // it could (and a buggy client's repeated nonce cannot claim another's answer).
        let seen = hex::encode(tokumai_core::account::sha256(&[&ct]));
        if let Some(cached) = self.replies.lock().ok().and_then(|r| r.get(&seen).map(|(_, b)| b.clone())) {
            return cached;
        }
        // Nothing is said about WHY a request does not open: it is either not for this
        // enclave (another start, other keys — the app re-attests) or it was tampered with.
        let Ok((exchange, plain)) = ServerExchange::open(&self.keys.kx, &epk, &ct, version) else {
            return plain_err("this request is not sealed to this enclave — attest again");
        };
        // FnOnce: the exchange goes into the one answer, whichever branch gives it.
        let seal = move |value: Value| -> Vec<u8> {
            let ct = exchange.seal_response(&serde_json::to_vec(&value).unwrap_or_default());
            serde_json::to_vec(&json!({ "kind": "sealed", "v": version, "ct": B64.encode(ct) })).unwrap_or_default()
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
        // A nonce seen before, with its reply no longer cached (or under other bytes): the
        // request is not carried out again, and not answered under its key again either.
        let first = match self.ledger.lock() {
            Ok(l) => l.first_sight(&req.nonce, now),
            Err(_) => Err("ledger unavailable".into()),
        };
        match first {
            Ok(true) => {}
            Ok(false) => return plain_err("this request was already made, and its answer is no longer kept"),
            Err(e) => return seal(error(&e)),
        }
        // Three lines, because "it hangs" has three causes that look alike from outside.
        // The operation's name and nothing else about it (`crate::trace`).
        let op = req.op.clone();
        crate::trace::say(|| format!("req {op} arrived"));
        let started = std::time::Instant::now();
        self.stats.seen(&self.account_key(&account_id), now);
        self.working.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let answer = self.dispatch(&account_id, &req.op, &req.body, now).await;
        self.working.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        crate::trace::say(|| {
            let kind = answer.get("kind").and_then(|k| k.as_str()).unwrap_or("?");
            format!("req {op} -> {kind} in {} ms", started.elapsed().as_millis())
        });
        let out = seal(answer);
        self.remember(&seen, &out);
        crate::trace::say(|| format!("req {op} sealed, {} bytes to send", out.len()));
        out
    }

    /// Drop every kept reply, as the cache's own limits would in time. For the tests of
    /// what a resend gets once its answer is gone.
    pub fn forget_kept_replies(&self) {
        if let Ok(mut r) = self.replies.lock() {
            r.clear();
        }
    }

    /// Keep a reply under the hash of the request's ciphertext, for a resend.
    fn remember(&self, seen: &str, out: &[u8]) {
        if out.len() > REPLY_KEEP_BYTES / 4 {
            return; // one reply that would evict most of the others is not worth keeping
        }
        if let Ok(mut r) = self.replies.lock() {
            r.retain(|_, (at, _)| at.elapsed() < REPLY_KEEP);
            let mut bytes: usize = r.values().map(|(_, b)| b.len()).sum::<usize>() + out.len();
            while r.len() >= REPLY_KEEP_MAX || bytes > REPLY_KEEP_BYTES {
                let Some(oldest) = r.iter().min_by_key(|(_, (at, _))| *at).map(|(k, _)| k.clone()) else { break };
                if let Some((_, gone)) = r.remove(&oldest) {
                    bytes -= gone.len();
                }
            }
            r.insert(seen.to_string(), (Instant::now(), out.to_vec()));
        }
    }

    /// How long a purchase waits for the host to confirm the book before it is answered.
    const DURABLE_WITHIN: std::time::Duration = std::time::Duration::from_secs(45);

    /// Answer only once everything written down so far is on the host's disk. For what
    /// grants credit — a purchase, a note, a plan — so that the answer the app keeps (a
    /// transaction finished with Apple, a note spent) is never ahead of the book. A chat
    /// does not wait: its hold and settle are written behind it (`ledger::Kept`).
    ///
    /// If the host is too slow the change stays made and the writer keeps offering it; the
    /// app gets an error and asks again, which every one of these operations answers the
    /// same way the second time.
    async fn durably(&self, answer: Value) -> Value {
        if self.wait_for_host().await {
            answer
        } else {
            error("the book could not be written in time — please try again")
        }
    }

    /// True once the host has confirmed everything written down so far (or there is no
    /// host to wait for); false when it took longer than `DURABLE_WITHIN`.
    pub(crate) async fn wait_for_host(&self) -> bool {
        let (flushed, mark) = match self.ledger.lock() {
            Ok(l) => (l.flushed(), l.mark()),
            Err(_) => return true,
        };
        let Some(flushed) = flushed else { return true };
        tokio::task::spawn_blocking(move || flushed.wait(mark, Self::DURABLE_WITHIN)).await.unwrap_or(false)
    }

    async fn dispatch(&self, account: &str, op: &str, body: &str, now: u64) -> Value {
        match op {
            "balance" => self.balance(account, now),
            "chat" => self.chat(account, body, now).await,
            "models" => json!({ "kind": "models", "models": crate::catalog::models(&self.pricing, &self.providers, self.dev_mode), "pricingVersion": self.pricing.version() }),
            // Everything an app wants at start, in one round trip: over the mixnet each one
            // costs about three seconds, and three of them in a row are what the person
            // waits through before the first screen (measured 2026-09-22).
            "start" => json!({
                "kind": "start",
                "balance": self.balance(account, now),
                "models": crate::catalog::models(&self.pricing, &self.providers, self.dev_mode),
                "pricingVersion": self.pricing.version(),
                "plans": self.plans_op(account, now),
            }),
            "plans" => self.plans_op(account, now),
            "plan.create" => self.plan_create(account, body, now).await,
            "plan.status" => {
                let a = self.plan_status(account, body, now).await;
                self.durably(a).await
            }
            "plan.change" => {
                let a = self.plan_change(account, body, now).await;
                self.durably(a).await
            }
            "iap.verify" => {
                let a = self.iap_verify(account, body, now);
                self.durably(a).await
            }
            // The blind notes: minting is not for this account (the key is thrown away);
            // redeeming is.
            "note.mint" => {
                let a = self.note_mint(body, now).await;
                self.durably(a).await
            }
            "note.redeem" => {
                let a = self.note_redeem(account, body, now);
                self.durably(a).await
            }
            op if op.starts_with("admin.") => self.admin_dispatch(account, op, body, now),
            "dev.credit" | "dev.allowance" | "dev.bytes" if !self.dev_mode => error("not available on this enclave"),
            // A reply of a given size, to measure what the mixnet does with a big answer
            // without paying a model for a picture. Never on a sealed enclave.
            "dev.bytes" => {
                let n = serde_json::from_str::<Value>(body).ok().and_then(|b| b.get("bytes").and_then(|t| t.as_u64())).unwrap_or(0);
                json!({ "kind": "bytes", "data": "x".repeat(n.min(32 * 1024 * 1024) as usize) })
            }
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

    /// The name an account's card checkouts carry at Stripe (`client_reference_id`): a
    /// keyed hash, the same for every checkout of the account, so that the account that
    /// opened a checkout is the only one that can claim what it paid for (audit M2). It
    /// names no account to Stripe — what Stripe can tell from it is that two checkouts
    /// were the same person's, which the card already told it.
    pub(crate) fn order_tag(&self, account: &str) -> String {
        hex::encode(tokumai_core::account::sha256(&[b"tokumai/stripe/order/v1", &self.safety_salt, account.as_bytes()]))[..32].to_string()
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
        let Ok(mut req) = serde_json::from_str::<ChatRequest>(body) else { return error("malformed chat request") };
        let Some(messages) = req.messages.as_array() else { return error("malformed chat request") };
        if messages.is_empty() || messages.len() > policy::MAX_MESSAGES {
            return error("a chat request needs between 1 and 2,000 messages");
        }
        // Worst-case input: bytes of text (a token is at least a byte) plus what each
        // attachment can cost. Only types the providers read are passed on.
        let mut in_tokens: u64 = 0;
        let mut attachments = 0usize;
        let mut pages = 0u64;
        for m in messages {
            in_tokens += m.get("content").and_then(|c| c.as_str()).map(|c| c.len() as u64).unwrap_or(0);
            for att in m.get("attachments").and_then(|a| a.as_array()).map(Vec::as_slice).unwrap_or(&[]) {
                let mime = att.get("mimeType").and_then(|x| x.as_str()).unwrap_or("");
                let b64 = att.get("data").and_then(|x| x.as_str()).unwrap_or("");
                if !policy::ATTACHMENT_TYPES.contains(&mime) {
                    return error(&format!("attachments of type {mime:?} are not supported — pictures and PDFs are"));
                }
                attachments += 1;
                if attachments > policy::MAX_ATTACHMENTS {
                    return error(&format!("at most {} attachments in one request", policy::MAX_ATTACHMENTS));
                }
                let bytes = b64.len() / 4 * 3;
                if bytes > policy::MAX_ATTACHMENT_BYTES {
                    return error("an attachment is larger than 10 MB");
                }
                // A PDF is read page by page, and charged by the page: reserve for every
                // page it has (and for its bytes, whichever is more).
                in_tokens += if mime == "application/pdf" {
                    let n = pdf_pages(b64);
                    pages += n;
                    if pages > policy::MAX_PDF_PAGES {
                        return error(&format!("at most {} PDF pages in one request", policy::MAX_PDF_PAGES));
                    }
                    (n * policy::PDF_PAGE_TOKENS).max(bytes as u64 / 40).max(4096)
                } else {
                    policy::IMAGE_INPUT_TOKENS
                };
            }
        }
        if !crate::catalog::offered(&req.model, &self.pricing, self.dev_mode) {
            return error("that model is not offered");
        }
        let Some(provider) = self.providers.find(&req.model) else { return error("that model is not offered") };
        let price = self.pricing.price(&req.model);
        let day = now / 86_400_000;
        let strike_key = (account.to_string(), day, provider.name());
        let already = self.strike_count(&strike_key);
        if already >= policy::STRIKES_PER_DAY {
            // The same shape as a decline itself, so the app says it in one voice — and with
            // the moment rather than "tomorrow", which the reader's own clock can render.
            let total = self.ledger.lock().ok().and_then(|l| l.balance(account, now).ok()).map(|b| b.total).unwrap_or(0);
            return json!({ "kind": "chat", "text": "", "declined": true, "cost": 0, "balance": total,
                "why": format!("Nothing was sent: {} of this account's questions were declined by {} today.", already, provider.name()),
                "strikes": { "used": already, "of": policy::STRIKES_PER_DAY, "untilMs": (day + 1) * 86_400_000, "provider": provider.name() } });
        }
        let image_size = crate::gemini::effective_image_size(&req.model, req.image_size.as_deref());
        let search_usd = if crate::openai::is_openai_model(&req.model) { policy::OPENAI_USD_PER_QUERY } else { policy::GEMINI_USD_PER_QUERY };
        let search_charge = |queries: u64| -> u64 {
            if queries == 0 {
                return 0;
            }
            (ceil_toku(queries as f64 * search_usd * TOKU_PER_USD as f64) * policy::MARGIN).ceil() as u64
        };
        let ceiling_for = |out_tokens: u64| {
            ceiling_toku(&price, policy::MARGIN, &Ceiling { in_tokens, out_tokens, image_tokens: crate::gemini::image_tokens(&req.model, image_size) })
                + per_image_toku(&price, policy::MARGIN)
                + if req.live { search_charge(policy::MAX_SEARCH_QUERIES) } else { 0 }
        };
        let mut ceiling = ceiling_for(req.answer_tokens() + req.thinking_tokens());
        // Less on the account than the worst case: shorten the answer to what it covers
        // rather than refuse — a question that would cost 12 TOKU must not be turned away
        // because the longest possible answer would cost thousands. Refused only when not
        // even a short answer fits, or for a picture (which cannot be shortened).
        let available = match self.ledger.lock().map_err(|_| "ledger unavailable".to_string()).and_then(|l| l.balance(account, now)) {
            Ok(b) => b.total,
            Err(e) => return error(&e),
        };
        let mut capped = false;
        if ceiling > available {
            let fixed = ceiling_for(0);
            let per_token = ceiling_for(1_000_000).saturating_sub(fixed) as f64 / 1_000_000.0;
            let fits = if available > fixed && per_token > 0.0 { ((available - fixed) as f64 / per_token).floor() as u64 } else { 0 };
            if crate::gemini::is_image_model(&req.model) || fits < policy::MIN_ANSWER_TOKENS {
                let msg = if available == 0 {
                    "Your balance is 0 TOKU — get credit to keep chatting.".to_string()
                } else {
                    format!("Not enough credit for this: it can cost up to {ceiling} TOKU, and your balance is {available} TOKU.")
                };
                return json!({ "kind": "error", "error": msg, "noCredit": true, "balance": available });
            }
            // Keep a third for thinking at most, the rest for the answer itself.
            let thinking = req.thinking_tokens().min(fits / 3);
            req.thinking = Some(thinking);
            req.max_tokens = Some(req.answer_tokens().min(fits - thinking).max(1));
            ceiling = ceiling_for(req.answer_tokens() + req.thinking_tokens()).min(available);
            capped = true;
        }
        let hold = match self.ledger.lock().map_err(|_| "ledger unavailable".to_string()).and_then(|mut l| l.hold(account, ceiling.max(1), now)) {
            Ok(h) => h,
            Err(e) => return error(&e),
        };
        // What this call looks like from outside our machine, and whether this account is
        // inside the window after a payment. If it is, and nobody else's traffic of this
        // shape has been through since its last call, decoys go out with it — to the same
        // provider, in the same spread of seconds, the real call at a random place among
        // them (see `cover`). Everything the host can see of the real call, the moderation
        // call included, happens after the hold.
        let shape = if crate::gemini::is_image_model(&req.model) { crate::cover::Shape::Picture } else { crate::cover::Shape::Text };
        let needed = self.cover.decoys_needed(&self.account_key(account), shape, now);
        if needed > 0 {
            let (mine, theirs) = crate::cover::moments(needed);
            for wait in theirs {
                let providers = self.providers.clone();
                let like = req.model.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                    providers.decoy(&like).await;
                });
            }
            tokio::time::sleep(std::time::Duration::from_millis(mine)).await;
        }
        // The moderation check runs before the model is asked; a flagged turn costs nothing.
        // Only in front of OpenAI's own models: Google filters its traffic itself
        // (`policy::GEMINI_SAFETY`), and having OpenAI read a Gemini user's question would
        // be a second provider seeing it for no gain to the person who asked.
        let flagged = if crate::openai::is_openai_model(&req.model) { self.providers.moderate(&req.messages).await } else { Ok(None) };
        let result = match flagged {
            Ok(Some(cats)) => Err(format!(
                "Declined by the safety check ({}). The question was not sent to the model and nothing was charged.",
                crate::openai::plain_categories(&cats)
            )),
            _ => {
                // A call went out: cover for everyone else's. (A turn the safety check
                // stopped went nowhere, and counts for nobody.)
                self.cover.note(shape, now);
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
        // The answer cost more than was held: the account pays what was held, we pay the
        // rest — and count it, so a reserve that is too small is seen, not absorbed.
        if cost > hold.amount {
            self.stats.add("billing:over-hold", 1);
            crate::trace::say(|| format!("req chat cost {cost} over a hold of {}", hold.amount));
        }
        let charged = match self.ledger.lock().map_err(|_| "ledger unavailable".to_string()).and_then(|mut l| l.settle(hold, cost)) {
            Ok(k) => k,
            Err(e) => return error(&e),
        };
        self.stats.request(now, &req.model, charged, matches!(&result, Ok(c) if c.text.starts_with("Declined by")) || matches!(&result, Err(e) if e.starts_with("Declined by")));
        // A decline arrives two ways — as an error (our safety check, OpenAI's policy) and as
        // an answer whose text says so (Google's finish reason). Both are the same event to
        // the person who asked, so both leave here in the same shape: no model prose, a
        // reason, and from the second one of the day what happens after the third.
        let declined = match &result {
            Ok(c) => c.text.starts_with("Declined by"),
            Err(e) => e.starts_with("Declined by"),
        };
        if declined {
            self.strike(strike_key.clone());
            let used = self.strike_count(&strike_key);
            let why = match &result {
                Ok(c) => c.text.clone(),
                Err(e) => e.clone(),
            };
            let total = self.ledger.lock().ok().and_then(|l| l.balance(account, now).ok()).map(|b| b.total).unwrap_or(0);
            let mut out = json!({ "kind": "chat", "text": "", "declined": true, "why": why, "cost": charged, "balance": total });
            if used >= policy::STRIKE_WARN_FROM {
                // The app says the time in the reader's own clock; "tomorrow" from here
                // would mean UTC midnight, which is one or two in the morning for most of
                // the people this is said to.
                out["strikes"] = json!({ "used": used, "of": policy::STRIKES_PER_DAY,
                                         "untilMs": (day + 1) * 86_400_000, "provider": provider.name() });
            }
            return out;
        }
        match result {
            Ok(c) => {
                let total = self.ledger.lock().ok().and_then(|l| l.balance(account, now).ok()).map(|b| b.total).unwrap_or(0);
                // Packed here, where the picture still is: megabytes do not cross the
                // mixnet well, and what is charged was decided above, on what the model did.
                let images = c.images.map(|i| crate::picture::pack_all(i, req.lossless));
                json!({
                    "kind": "chat", "text": c.text, "images": images, "cost": charged, "balance": total,
                    // Shortened only if the answer actually ran into the lowered limit.
                    "capped": capped && c.usage.output + 8 >= req.answer_tokens(),
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
