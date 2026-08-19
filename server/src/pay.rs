// ---------------------------------------------------------------------------
// pay.rs — the paywall: invoices, entitlements, and the coconut-withdraw gate.
// Ported from the TS issuer (src/money/issuer.ts + gateway.ts + the
// invoice.* handlers in src/cli/server.ts).
//
// Three steps, and the order of the middle two is what protects the user:
//
//   1. RAISE     an invoice against an account (account-signed request — it has
//                to be known here, or a payment could not be credited to anyone).
//   2. SETTLE    the gateway says it was paid → the account gains an
//                ENTITLEMENT. Still fully linkable, still just bookkeeping.
//   3. WITHDRAW  the account trades entitlement for a coconut ticketbook. The
//                blind issuance means the coins that come out cannot later be
//                tied to this account — the link dies at this step.
//
// Gateways: BTCPay (real; BTCPAY_URL/STORE_ID/API_KEY) or the fake (dev;
// SCRAI_FAKE_PAYMENTS=1 — settles on first poll and says so loudly).
// ---------------------------------------------------------------------------

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use scrai_core::auth;
use scrai_core::coconut::SCRAI_PER_USD;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// Rate limits, mirroring the TS server: an account or a swarm must not be able
// to make us hammer the payment gateway (the one anonymity-exposed, externally
// costly call).
const INVOICE_PER_ACCT: usize = 5;
const INVOICE_ACCT_WINDOW_MS: u64 = 600_000;
const INVOICE_GLOBAL_PER_MIN: usize = 30;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn purchase_tiers() -> Vec<u32> {
    std::env::var("SCRAI_PURCHASE_TIERS")
        .ok()
        .map(|s| s.split(',').filter_map(|t| t.trim().parse().ok()).collect())
        .filter(|v: &Vec<u32>| !v.is_empty())
        .unwrap_or_else(|| vec![5, 10, 20, 50])
}

#[derive(Serialize, Deserialize, Clone)]
struct Inv {
    id: String,
    provider_ref: String,
    account_id: String,
    amount_usd: u32,
    amount_scrai: u64,
    method: String,
    status: String, // "pending" | "paid" | "expired"
    expires_at: u64,
    /// Exact unym quoted for a native-NYM invoice (0 for every other rail).
    /// Lives IN the durable record so a pending payment survives restarts.
    #[serde(default)]
    expected_unym: u64,
}

/// Durable paywall state (invoices, entitlements, burned nonces) + the volatile
/// rate-limit windows. Snapshot/restore mirrors SessionStore so main.rs persists
/// it with the same revision-gated write.
#[derive(Default, Serialize, Deserialize)]
pub struct Pay {
    invoices: HashMap<String, Inv>,
    entitlements: HashMap<String, u64>,
    nonces: HashSet<String>,
    #[serde(default)]
    rev: u64,
    #[serde(skip)]
    acct_hits: HashMap<String, Vec<u64>>,
    #[serde(skip)]
    global_hits: Vec<u64>,
}

impl Pay {
    pub fn from_snapshot(json: &str) -> Self {
        serde_json::from_str(json).unwrap_or_default()
    }
    pub fn snapshot(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
    pub fn revision(&self) -> u64 {
        self.rev
    }

    /// Burn a nonce for an account-signed request. False = replay.
    fn burn_nonce(&mut self, account_id: &str, nonce: &str) -> bool {
        if nonce.is_empty() {
            return false;
        }
        let inserted = self.nonces.insert(format!("acct:{account_id}:{nonce}"));
        if inserted {
            self.rev += 1;
        }
        inserted
    }

    /// Signature first, nonce burn last: a failed signature must not consume a
    /// nonce, or an attacker could invalidate someone's pending request.
    fn account_owns(&mut self, v: &Value, purpose: &str) -> Option<String> {
        let pem = v.get("publicKey").and_then(|p| p.as_str())?;
        let nonce = v.get("nonce").and_then(|n| n.as_str())?;
        let sig = v.get("sig").and_then(|s| s.as_str())?;
        let account_id = auth::account_owns(pem, purpose, nonce, sig)?;
        self.burn_nonce(&account_id, nonce).then_some(account_id)
    }

    fn admit_invoice(&mut self, account_id: &str) -> Result<(), String> {
        let now = now_ms();
        self.global_hits.retain(|t| now - t < 60_000);
        if self.global_hits.len() >= INVOICE_GLOBAL_PER_MIN {
            return Err("the server is issuing too many invoices right now — retry in ~60s".into());
        }
        let hits = self.acct_hits.entry(account_id.to_string()).or_default();
        hits.retain(|t| now - t < INVOICE_ACCT_WINDOW_MS);
        if hits.len() >= INVOICE_PER_ACCT {
            let retry = (INVOICE_ACCT_WINDOW_MS - (now - hits[0])).div_ceil(1000).max(1);
            return Err(format!("too many invoices from this account — retry in ~{retry}s"));
        }
        hits.push(now);
        self.global_hits.push(now);
        // Opportunistic prune so a throwaway swarm cannot grow the map unboundedly.
        self.acct_hits.retain(|_, v| v.iter().any(|t| now - t < INVOICE_ACCT_WINDOW_MS));
        Ok(())
    }

    pub fn entitlement(&self, account_id: &str) -> u64 {
        *self.entitlements.get(account_id).unwrap_or(&0)
    }

    /// Deduct entitlement after a successful coconut issuance.
    pub fn consume_entitlement(&mut self, account_id: &str, amount: u64) {
        let e = self.entitlements.entry(account_id.to_string()).or_default();
        *e = e.saturating_sub(amount);
        self.rev += 1;
    }

    /// Settle one invoice (idempotent): anything-but-paid → paid credits the
    /// entitlement exactly once. Deliberately also settles a locally "expired"
    /// invoice — BTCPay keeps watching past our window, and if IT says Settled,
    /// money moved and must be credited regardless of our timer.
    fn settle(&mut self, invoice_id: &str) {
        if let Some(inv) = self.invoices.get_mut(invoice_id) {
            if inv.status != "paid" {
                inv.status = "paid".into();
                let scrai = inv.amount_scrai;
                let account = inv.account_id.clone();
                *self.entitlements.entry(account).or_default() += scrai;
                self.rev += 1;
            }
        }
    }

    /// Re-check every unpaid invoice against the gateway. This is what makes a
    /// slow confirmation safe: the client stops polling when the window closes
    /// or the app quits, so a payment that confirms later would otherwise be
    /// money taken and never credited. Settlement is idempotent, so racing a
    /// polling client is harmless. Bounded to invoices younger than ~48h past
    /// their window so the sweep stays a handful of HTTP calls.
    async fn sweep(&mut self, gateway: &Gateway) {
        let now = now_ms();
        let candidates: Vec<Inv> = self
            .invoices
            .values()
            .filter(|i| i.status != "paid" && now < i.expires_at + 48 * 3_600_000)
            .cloned()
            .collect();
        for inv in candidates {
            if let Ok(s) = gateway.check_status(&inv).await {
                if s == "paid" {
                    self.settle(&inv.id);
                }
            }
        }
    }

    fn expire_stale(&mut self) {
        let now = now_ms();
        let mut changed = false;
        for inv in self.invoices.values_mut() {
            if inv.status == "pending" && now > inv.expires_at {
                inv.status = "expired".into();
                changed = true;
            }
        }
        if changed {
            self.rev += 1;
        }
    }

    // ---- request handlers --------------------------------------------------

    /// Handle invoice.create / invoice.status / invoice.cancel / entitlement.
    pub async fn handle(&mut self, request: &[u8], gateway: &Gateway) -> Vec<u8> {
        let v: Value = serde_json::from_slice(request).unwrap_or(Value::Null);
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        let reply = match v.get("kind").and_then(|k| k.as_str()).unwrap_or("") {
            "invoice.create" => self.create(&v, &id, gateway).await,
            "invoice.status" => self.status(&v, &id, gateway).await,
            "invoice.cancel" => self.cancel(&v, &id),
            // Catch any late confirmation FIRST, so "check for credit" finds a
            // payment that confirmed after the client stopped polling.
            "entitlement" => {
                self.sweep(gateway).await;
                self.entitlement_reply(&v, &id)
            }
            other => err(&id, &format!("unknown kind: {other}")),
        };
        serde_json::to_vec(&reply).unwrap_or_default()
    }

    async fn create(&mut self, v: &Value, id: &Value, gateway: &Gateway) -> Value {
        let usd = v.get("usd").and_then(|u| u.as_u64()).unwrap_or(0) as u32;
        let Some(account) = self.account_owns(v, &format!("invoice:{usd}")) else {
            return err(id, "account signature does not check out, or the nonce was reused");
        };
        // Fixed amounts only, so every purchase looks like everyone else's — a
        // free-form amount would be a fingerprint.
        let tiers = purchase_tiers();
        if !tiers.contains(&usd) {
            let list = tiers.iter().map(|t| format!("${t}")).collect::<Vec<_>>().join(", ");
            return err(id, &format!("purchases must be one of: {list}"));
        }
        // Throttle BEFORE the external BTCPay call.
        if let Err(e) = self.admit_invoice(&account) {
            return err(id, &e);
        }

        let our_id = rand_hex(16);
        // The client picks the rail ("nyx" = native NYM on the Nyx chain); it is
        // deliberately NOT part of the account signature — it only selects HOW to
        // pay, never how much is credited.
        let wanted = v.get("method").and_then(|m| m.as_str()).unwrap_or("btc");
        let raised = match gateway.create_invoice(usd, &our_id, wanted).await {
            Ok(r) => r,
            Err(e) => return err(id, &e),
        };
        // The SCRAI amount is fixed HERE, not at settlement, so the user gets
        // exactly what they were quoted regardless of the exchange rate.
        let amount_scrai = usd as u64 * SCRAI_PER_USD;
        self.invoices.insert(
            our_id.clone(),
            Inv {
                id: our_id.clone(),
                provider_ref: raised.raised.provider_ref.clone(),
                account_id: account,
                amount_usd: usd,
                amount_scrai,
                method: raised.method.clone(),
                status: "pending".into(),
                expires_at: raised.raised.expires_at,
                expected_unym: raised.expected_unym,
            },
        );
        self.rev += 1;

        json!({
            "kind": "invoice.ok", "id": id,
            "invoiceId": our_id,
            "payTo": raised.raised.pay_to,
            "instruction": raised.raised.instruction,
            "options": raised.raised.options,
            "amountUsd": usd,
            "amountScrai": amount_scrai,
            "expiresAt": raised.raised.expires_at,
        })
    }

    /// Deliberately unauthenticated (like the TS server): the invoice id is a
    /// random id the client just received, and the reply reveals nothing usable.
    async fn status(&mut self, v: &Value, id: &Value, gateway: &Gateway) -> Value {
        self.expire_stale();
        let inv_id = v.get("invoiceId").and_then(|i| i.as_str()).unwrap_or("").to_string();
        let Some(inv) = self.invoices.get(&inv_id).cloned() else {
            return err(id, "unknown invoice");
        };
        if inv.status != "paid" {
            // Poll the gateway too — a webhook that never arrived must not leave
            // a paying customer stuck. Also for locally-expired invoices: BTCPay
            // keeps watching, and a late on-chain confirmation still counts.
            // Settlement is idempotent.
            if let Ok("paid") = gateway.check_status(&inv).await.as_deref() {
                self.settle(&inv_id);
            }
        }
        let now = self.invoices.get(&inv_id).expect("checked above");
        json!({
            "kind": "invoice.state", "id": id,
            "status": now.status,
            "entitlement": self.entitlement(&now.account_id),
            // Chain-watch health for native-NYM invoices — feeds the pay
            // screen's live indicator (null on the processor rail).
            "watch": gateway.watch_state(&now.method),
        })
    }

    fn cancel(&mut self, v: &Value, id: &Value) -> Value {
        let inv_id = v.get("invoiceId").and_then(|i| i.as_str()).unwrap_or("");
        let ok = match self.invoices.get_mut(inv_id) {
            Some(inv) if inv.status == "pending" => {
                inv.status = "expired".into();
                self.rev += 1;
                true
            }
            _ => false,
        };
        json!({ "id": id, "ok": ok })
    }

    fn entitlement_reply(&mut self, v: &Value, id: &Value) -> Value {
        let Some(account) = self.account_owns(v, "entitlement") else {
            return err(id, "account signature does not check out, or the nonce was reused");
        };
        json!({ "id": id, "entitlement": self.entitlement(&account) })
    }

    // ---- the coconut-withdraw gate ------------------------------------------

    /// Decide whether a `coconut` envelope may pass to the federation handler.
    /// Only `Withdraw` is gated (Keys and Spend stay open): it must carry a valid
    /// account signature and the account must hold a full ticketbook's worth of
    /// entitlement. Returns who to charge on success.
    pub fn gate_withdraw(&mut self, request: &[u8], book_scrai: u64) -> Gate {
        let v: Value = serde_json::from_slice(request).unwrap_or(Value::Null);
        let is_withdraw = v
            .pointer("/fed/Withdraw")
            .is_some();
        if !is_withdraw {
            return Gate::NotAWithdraw;
        }
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        let Some(account) = self.account_owns(&v, "withdraw:coconut") else {
            return Gate::Denied(encode(&err(
                &id,
                "a withdrawal must be signed by the paying account",
            )));
        };
        let held = self.entitlement(&account);
        if held < book_scrai {
            return Gate::Denied(encode(&err(
                &id,
                &format!("not enough entitlement: a ticketbook costs {book_scrai} SCRAI, this account holds {held} — buy credit first"),
            )));
        }
        Gate::Authorized { account_id: account }
    }
}

pub enum Gate {
    NotAWithdraw,
    Denied(Vec<u8>),
    Authorized { account_id: String },
}

fn err(id: &Value, msg: &str) -> Value {
    json!({ "id": id, "kind": "error", "error": msg })
}

fn encode(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

fn rand_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Gateways
// ---------------------------------------------------------------------------

pub struct RaisedInvoice {
    pub provider_ref: String,
    pub pay_to: String,
    pub instruction: String,
    pub options: Value,
    pub expires_at: u64,
}

/// A raised invoice plus what the paywall must remember about it: which rail
/// actually served it (the client's wish is a wish, not a guarantee) and the
/// exact unym quoted when that rail was native NYM.
pub struct Raised {
    pub raised: RaisedInvoice,
    pub method: String,
    pub expected_unym: u64,
}

/// Both payment rails side by side: the processor rail (BTCPay / fake / none)
/// serves "btc", the native Nyx rail serves "nyx" when configured. A "nyx"
/// request without a configured Nyx rail falls back to the processor rail —
/// exactly the pre-port stopgap where the NYM tile raised BTCPay invoices.
pub struct Gateway {
    rail: Rail,
    nyx: Option<crate::nyx::Nyx>,
}

impl Gateway {
    pub fn from_env() -> Gateway {
        Gateway { rail: Rail::from_env(), nyx: crate::nyx::Nyx::from_env() }
    }

    pub fn name(&self) -> String {
        match &self.nyx {
            Some(_) => format!("{}+nyx", self.rail.name()),
            None => self.rail.name().to_string(),
        }
    }

    async fn create_invoice(&self, usd: u32, reference: &str, wanted: &str) -> Result<Raised, String> {
        if wanted == "nyx" {
            if let Some(nyx) = &self.nyx {
                let (raised, expected_unym) = nyx.create_invoice(usd).await?;
                return Ok(Raised { raised, method: "nyx".into(), expected_unym });
            }
        }
        let raised = self.rail.create_invoice(usd, reference).await?;
        Ok(Raised { raised, method: "btc".into(), expected_unym: 0 })
    }

    async fn check_status(&self, inv: &Inv) -> Result<String, String> {
        if inv.method == "nyx" {
            let Some(nyx) = &self.nyx else {
                return Err("this invoice is native-NYM but no Nyx rail is configured".into());
            };
            return nyx.check_paid(&inv.provider_ref, inv.expected_unym).await;
        }
        self.rail.check_status(&inv.provider_ref).await
    }

    /// Chain-watch health for the pay screen — only native NYM has one.
    fn watch_state(&self, method: &str) -> Value {
        match (&self.nyx, method) {
            (Some(nyx), "nyx") => nyx.watch_state(),
            _ => Value::Null,
        }
    }
}

/// BTCPay (real) or the fake (dev). Selection fails loudly when nothing is
/// configured — an issuer that hands out SCRAI for imaginary money must never
/// be a silent default.
pub enum Rail {
    Fake,
    BtcPay { base_url: String, store_id: String, api_key: String },
    None,
}

impl Rail {
    pub fn from_env() -> Rail {
        if std::env::var("SCRAI_FAKE_PAYMENTS").as_deref() == Ok("1") {
            eprintln!("scrai-server: SCRAI_FAKE_PAYMENTS=1 — invoices settle on first poll. DEV ONLY.");
            return Rail::Fake;
        }
        match (
            std::env::var("BTCPAY_URL"),
            std::env::var("BTCPAY_STORE_ID"),
            std::env::var("BTCPAY_API_KEY"),
        ) {
            (Ok(u), Ok(s), Ok(k)) if !u.is_empty() && !s.is_empty() && !k.is_empty() => {
                Rail::BtcPay { base_url: u.trim_end_matches('/').to_string(), store_id: s, api_key: k }
            }
            _ => Rail::None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Rail::Fake => "fake",
            Rail::BtcPay { .. } => "btcpay",
            Rail::None => "none",
        }
    }

    async fn create_invoice(&self, usd: u32, reference: &str) -> Result<RaisedInvoice, String> {
        match self {
            Rail::None => Err("this server cannot sell SCRAI — no payment gateway configured".into()),
            Rail::Fake => Ok(RaisedInvoice {
                provider_ref: format!("fake:{reference}"),
                pay_to: "fake — settles on first status poll".into(),
                instruction: "DEV gateway: this invoice settles itself on the first status check.".into(),
                options: json!([]),
                expires_at: now_ms() + 15 * 60_000,
            }),
            Rail::BtcPay { base_url, store_id, api_key } => {
                let inv: Value = btcpay(
                    api_key,
                    crate::http::client()
                        .post(format!("{base_url}/api/v1/stores/{store_id}/invoices"))
                        .json(&json!({
                            "amount": format!("{usd}.00"),
                            "currency": "USD",
                            // Our own id, so a support question can be traced back
                            // without BTCPay knowing anything about the account.
                            "metadata": { "orderId": reference },
                            "checkout": { "redirectAutomatically": false },
                        })),
                )
                .await?;
                let provider_ref = inv.get("id").and_then(|i| i.as_str()).unwrap_or_default().to_string();

                // The actual destinations live behind /payment-methods; a lazily
                // unactivated method has no destination and is filtered out.
                let methods: Value = btcpay(
                    api_key,
                    crate::http::client()
                        .get(format!("{base_url}/api/v1/invoices/{provider_ref}/payment-methods")),
                )
                .await
                .unwrap_or(json!([]));
                let options: Vec<Value> = methods
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter(|m| {
                                m.get("destination").and_then(|d| d.as_str()).is_some_and(|d| !d.is_empty())
                                    && m.get("activated").and_then(|a| a.as_bool()) != Some(false)
                            })
                            .map(|m| {
                                let dest = m.get("destination").and_then(|d| d.as_str()).unwrap_or("");
                                json!({
                                    "method": m.get("paymentMethodId").or(m.get("currency")).and_then(|x| x.as_str()).unwrap_or("BTC"),
                                    "destination": dest,
                                    "uri": m.get("paymentLink").and_then(|l| l.as_str()).unwrap_or(dest),
                                    "amount": m.get("amount").and_then(|a| a.as_str()).unwrap_or(""),
                                    "currency": m.get("currency").and_then(|c| c.as_str()).unwrap_or("BTC"),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let pay_to = options
                    .first()
                    .and_then(|o| o.get("destination").and_then(|d| d.as_str()))
                    .unwrap_or("")
                    .to_string();
                // BTCPay reports expiry in seconds; fall back to 60 min so an
                // on-chain payment has a realistic window to confirm.
                let expires_at = inv
                    .get("expirationTime")
                    .and_then(|e| e.as_u64())
                    .map(|s| s * 1000)
                    .unwrap_or_else(|| now_ms() + 60 * 60_000);
                Ok(RaisedInvoice {
                    provider_ref,
                    pay_to,
                    instruction: if options.len() > 1 {
                        "Pay with any of the options below — Lightning settles instantly.".into()
                    } else {
                        "Pay to the destination below.".into()
                    },
                    options: json!(options),
                    expires_at,
                })
            }
        }
    }

    /// "paid" | "pending" | "expired". Processing deliberately does NOT count as
    /// paid — honouring BTCPay's "Settled" honours the operator's confirmation
    /// settings instead of second-guessing them here.
    async fn check_status(&self, provider_ref: &str) -> Result<String, String> {
        match self {
            Rail::None => Err("no payment gateway configured".into()),
            Rail::Fake => Ok("paid".into()),
            Rail::BtcPay { base_url, api_key, .. } => {
                let inv: Value = btcpay(
                    api_key,
                    crate::http::client().get(format!("{base_url}/api/v1/invoices/{provider_ref}")),
                )
                .await?;
                Ok(match inv.get("status").and_then(|s| s.as_str()).unwrap_or("") {
                    "Settled" => "paid",
                    "Expired" | "Invalid" => "expired",
                    _ => "pending",
                }
                .into())
            }
        }
    }
}

async fn btcpay(api_key: &str, req: reqwest::RequestBuilder) -> Result<Value, String> {
    let res = req
        // BTCPay's own scheme, not Bearer.
        .header("authorization", format!("token {api_key}"))
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("BTCPay unreachable: {e}"))?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = body.get("message").and_then(|m| m.as_str()).unwrap_or("");
        return Err(match status.as_u16() {
            401 | 403 => "BTCPay rejected the API key — check BTCPAY_API_KEY and its store permissions".into(),
            404 => "BTCPay does not know this store or invoice — check BTCPAY_STORE_ID".into(),
            s => format!("BTCPay {s}: {}", &msg[..msg.len().min(200)]),
        });
    }
    Ok(body)
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    use ed25519_dalek::{Signer, SigningKey};

    fn account() -> (SigningKey, String, String) {
        let sk = SigningKey::from_bytes(&[9u8; 32]);
        let mut der = vec![0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
        der.extend_from_slice(&sk.verifying_key().to_bytes());
        let pem = format!("-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n", B64.encode(der));
        let id = auth::id_for(&pem);
        (sk, pem, id)
    }

    fn signed(sk: &SigningKey, id: &str, purpose: &str, nonce: &str) -> String {
        B64.encode(sk.sign(format!("{id}:{purpose}:{nonce}").as_bytes()).to_bytes())
    }

    #[tokio::test]
    async fn fake_invoice_settles_on_poll_and_credits_entitlement_once() {
        let (sk, pem, aid) = account();
        let mut pay = Pay::default();
        let gw = Gateway { rail: Rail::Fake, nyx: None };

        let req = json!({"kind":"invoice.create","id":"r1","publicKey":pem,"usd":5,
            "nonce":"n1","sig":signed(&sk,&aid,"invoice:5","n1")});
        let r: Value = serde_json::from_slice(&pay.handle(req.to_string().as_bytes(), &gw).await).unwrap();
        let inv_id = r.get("invoiceId").and_then(|i| i.as_str()).unwrap().to_string();
        assert_eq!(r.get("amountScrai").and_then(|a| a.as_u64()), Some(500_000));

        // first poll settles (fake), entitlement credited exactly once
        for _ in 0..2 {
            let st = json!({"kind":"invoice.status","id":"r2","invoiceId":inv_id});
            let s: Value = serde_json::from_slice(&pay.handle(st.to_string().as_bytes(), &gw).await).unwrap();
            assert_eq!(s.get("status").and_then(|x| x.as_str()), Some("paid"));
            assert_eq!(s.get("entitlement").and_then(|x| x.as_u64()), Some(500_000));
        }
    }

    #[tokio::test]
    async fn bad_signature_nonce_replay_and_odd_amounts_are_refused() {
        let (sk, pem, aid) = account();
        let mut pay = Pay::default();
        let gw = Gateway { rail: Rail::Fake, nyx: None };

        // wrong amount signed vs requested
        let req = json!({"kind":"invoice.create","id":"r","publicKey":pem,"usd":50,
            "nonce":"n1","sig":signed(&sk,&aid,"invoice:5","n1")});
        let r: Value = serde_json::from_slice(&pay.handle(req.to_string().as_bytes(), &gw).await).unwrap();
        assert!(r.get("error").is_some());

        // non-tier amount (signature valid)
        let req = json!({"kind":"invoice.create","id":"r","publicKey":pem,"usd":7,
            "nonce":"n2","sig":signed(&sk,&aid,"invoice:7","n2")});
        let r: Value = serde_json::from_slice(&pay.handle(req.to_string().as_bytes(), &gw).await).unwrap();
        assert!(r.get("error").and_then(|e| e.as_str()).unwrap().contains("purchases must be"));

        // nonce replay
        let sig = signed(&sk, &aid, "invoice:5", "n3");
        let req = json!({"kind":"invoice.create","id":"r","publicKey":pem,"usd":5,"nonce":"n3","sig":sig});
        let ok: Value = serde_json::from_slice(&pay.handle(req.to_string().as_bytes(), &gw).await).unwrap();
        assert!(ok.get("invoiceId").is_some());
        let replay: Value = serde_json::from_slice(&pay.handle(req.to_string().as_bytes(), &gw).await).unwrap();
        assert!(replay.get("error").is_some());
    }

    #[tokio::test]
    async fn late_confirmation_is_swept_in_by_the_entitlement_check() {
        let (sk, pem, aid) = account();
        let mut pay = Pay::default();
        let gw = Gateway { rail: Rail::Fake, nyx: None };

        // Raise an invoice, then simulate "client stopped polling and the local
        // window closed" by force-expiring it — the exact stuck-payment case.
        let req = json!({"kind":"invoice.create","id":"r","publicKey":pem,"usd":5,
            "nonce":"n1","sig":signed(&sk,&aid,"invoice:5","n1")});
        let r: Value = serde_json::from_slice(&pay.handle(req.to_string().as_bytes(), &gw).await).unwrap();
        let inv_id = r.get("invoiceId").and_then(|i| i.as_str()).unwrap().to_string();
        pay.invoices.get_mut(&inv_id).unwrap().status = "expired".into();
        assert_eq!(pay.entitlement(&aid), 0);

        // "Check for credit" → entitlement request sweeps, finds the (fake-)paid
        // invoice despite the local expiry, and credits exactly once.
        let ent = json!({"kind":"entitlement","id":"e","publicKey":pem,
            "nonce":"n2","sig":signed(&sk,&aid,"entitlement","n2")});
        let e: Value = serde_json::from_slice(&pay.handle(ent.to_string().as_bytes(), &gw).await).unwrap();
        assert_eq!(e.get("entitlement").and_then(|x| x.as_u64()), Some(500_000));
        let ent2 = json!({"kind":"entitlement","id":"e2","publicKey":pem,
            "nonce":"n3","sig":signed(&sk,&aid,"entitlement","n3")});
        let e2: Value = serde_json::from_slice(&pay.handle(ent2.to_string().as_bytes(), &gw).await).unwrap();
        assert_eq!(e2.get("entitlement").and_then(|x| x.as_u64()), Some(500_000)); // once, not twice
    }

    #[tokio::test]
    async fn withdraw_gate_requires_signature_and_entitlement_and_consumes_it() {
        let (sk, pem, aid) = account();
        let mut pay = Pay::default();
        let book = 500_000u64;

        // a non-withdraw coconut envelope passes untouched
        let keys_env = json!({"kind":"coconut","id":"x","fed":"Keys"});
        assert!(matches!(pay.gate_withdraw(keys_env.to_string().as_bytes(), book), Gate::NotAWithdraw));

        // unsigned withdraw → denied
        let w = json!({"kind":"coconut","id":"x","fed":{"Withdraw":{}}});
        assert!(matches!(pay.gate_withdraw(w.to_string().as_bytes(), book), Gate::Denied(_)));

        // signed but broke → denied
        let w = json!({"kind":"coconut","id":"x","fed":{"Withdraw":{}},"publicKey":pem,
            "nonce":"w1","sig":signed(&sk,&aid,"withdraw:coconut","w1")});
        assert!(matches!(pay.gate_withdraw(w.to_string().as_bytes(), book), Gate::Denied(_)));

        // fund via fake invoice, then the gate authorizes and consumption empties it
        let gw = Gateway { rail: Rail::Fake, nyx: None };
        let req = json!({"kind":"invoice.create","id":"r","publicKey":pem,"usd":5,
            "nonce":"n1","sig":signed(&sk,&aid,"invoice:5","n1")});
        let r: Value = serde_json::from_slice(&pay.handle(req.to_string().as_bytes(), &gw).await).unwrap();
        let inv_id = r.get("invoiceId").and_then(|i| i.as_str()).unwrap().to_string();
        let st = json!({"kind":"invoice.status","id":"r2","invoiceId":inv_id});
        pay.handle(st.to_string().as_bytes(), &gw).await;

        let w = json!({"kind":"coconut","id":"x","fed":{"Withdraw":{}},"publicKey":pem,
            "nonce":"w2","sig":signed(&sk,&aid,"withdraw:coconut","w2")});
        match pay.gate_withdraw(w.to_string().as_bytes(), book) {
            Gate::Authorized { account_id } => {
                assert_eq!(account_id, aid);
                pay.consume_entitlement(&account_id, book);
                assert_eq!(pay.entitlement(&aid), 0);
            }
            _ => panic!("expected Authorized"),
        }
    }
}
