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
// SCRAI_FAKE_PAYMENTS=1 — settles on first poll and says so loudly), native NYM
// (nyx.rs), and cards via Mollie (MOLLIE_API_KEY — hosted checkout, polled, see
// docs/card-payments.md).
// ---------------------------------------------------------------------------

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use scrai_core::auth;
use scrai_core::coconut::SCRAI_PER_USD;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// Rate limits, mirroring the TS server: an account or a swarm must not be able
// to make us hammer the payment gateway (the one anonymity-exposed, externally
// costly call).
const INVOICE_PER_ACCT: usize = 5;
/// Card invoices get a tighter per-account budget in the same window: every one is a
/// Mollie payment object we cannot claw back once the credit is withdrawn, and card
/// checkouts are the one rail with a chargeback path.
const CARD_PER_ACCT: usize = 3;
const INVOICE_ACCT_WINDOW_MS: u64 = 600_000;
/// H4: hard cap on the burned-nonce store (oldest evicted past this). Large enough that a
/// legit client never bumps into it, small enough that a signed-nonce flood can't OOM.
const MAX_NONCES: usize = 100_000;
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

/// Smallest tile a card may buy (`SCRAI_CARD_MIN_USD`, default $10). Card fees carry a
/// fixed €0.25 part that eats a third of a $1 tile, and a card payment can be charged
/// back for weeks after the credit has been withdrawn as unlinkable ecash — so the card
/// rail only sells tiles where the fee is a rounding error and the exposure is bounded.
/// Reported to clients with the catalog so the app greys the smaller tiles itself.
pub fn card_min_usd() -> u32 {
    std::env::var("SCRAI_CARD_MIN_USD").ok().and_then(|v| v.trim().parse().ok()).filter(|v| *v > 0).unwrap_or(10)
}

/// True when a Mollie key is configured — the client shows the card row only then.
pub fn card_enabled() -> bool {
    matches!(CardRail::from_env(), CardRail::Mollie { .. })
}

/// What the catalog reply tells the app about cards: whether the row exists at all and
/// the smallest tile it may buy (one source of truth — never hardcoded in the client).
pub fn card_info() -> Value {
    json!({ "enabled": card_enabled(), "minUsd": card_min_usd() })
}

/// Testnet mode (`SCRAI_TESTNET=1`): the ONE extra thing it enables is a $1 invoice
/// flagged `testnet:true`, which the faucet on the same host pays for a tester. The
/// flag is reported to clients so the app can offer the toggle; without it a client
/// asking for a testnet purchase is refused. This is the kill switch: unset it (or
/// set 0) and restart, and both server and every client fall back to normal tiers.
pub fn is_testnet_server() -> bool {
    std::env::var("SCRAI_TESTNET")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// The only amount a testnet (faucet-paid) purchase may have.
pub const TESTNET_USD: u32 = 1;

/// The faucet wallet — the ONLY address whose NYM settles a testnet invoice. Sandbox NYM is
/// free (public Nym sandbox faucet), so without this pin anyone could raise $1 testnet
/// invoices and pay them without an invite code; every such credit is real model spend.
/// Unset on a testnet server → testnet purchases are refused (fail closed).
pub fn testnet_faucet_address() -> Option<String> {
    std::env::var("SCRAI_TESTNET_FAUCET_ADDRESS").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Where testers redeem a testnet invoice (`SCRAI_FAUCET_URL`); shown in the app next to
/// the memo. Only reported while testnet mode is on.
pub fn faucet_url() -> Option<String> {
    if !is_testnet_server() {
        return None;
    }
    std::env::var("SCRAI_FAUCET_URL").ok().filter(|u| u.starts_with("https://"))
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Inv {
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
    /// Raised as a $1 testnet purchase (faucet-paid). Persisted so scrai-admin and the
    /// faucet can tell test buys from real ones after a restart.
    #[serde(default)]
    testnet: bool,
}

/// Read-only view of a testnet invoice for the faucet (`scrai-faucet`) and scrai-admin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestnetInv {
    pub id: String,
    /// The Nyx memo the payment must carry (`provider_ref` of a native-NYM invoice).
    pub memo: String,
    pub amount_usd: u32,
    /// Exact unym the server quoted — the faucet pays THIS, never a client-supplied amount.
    pub unym: u64,
    pub status: String,
    pub expires_at: u64,
}

/// Durable paywall state (invoices, entitlements, burned nonces) + the volatile
/// rate-limit windows. Snapshot/restore mirrors SessionStore so main.rs persists
/// it with the same revision-gated write.
#[derive(Default, Serialize, Deserialize)]
pub struct Pay {
    invoices: HashMap<String, Inv>,
    entitlements: HashMap<String, u64>,
    nonces: HashSet<String>,
    // H4: bound the burned-nonce store. `nonces` alone grew without limit (serialised into
    // every snapshot → quadratic disk writes → OOM/disk-full under a signed-nonce flood).
    // The FIFO records insertion order so the oldest are evicted past MAX_NONCES. Eviction
    // only exposes VERY old nonces to replay (well past any legit window), and money moves
    // are additionally guarded by entitlement consumption + burned ecash serials.
    #[serde(default)]
    nonce_fifo: VecDeque<String>,
    #[serde(default)]
    rev: u64,
    #[serde(skip)]
    acct_hits: HashMap<String, Vec<u64>>,
    /// card invoices per account in the window (subset of acct_hits, tighter cap)
    #[serde(skip)]
    card_hits: HashMap<String, Vec<u64>>,
    #[serde(skip)]
    global_hits: Vec<u64>,
    /// last "global invoice cap" log line (ms) — one per minute, not one per refused request
    #[serde(skip)]
    global_cap_logged_at: u64,
}

impl Pay {
    pub fn snapshot(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
    pub fn revision(&self) -> u64 {
        self.rev
    }

    /// Burn a nonce for an account-signed request. False = replay. Bounded (H4): past
    /// MAX_NONCES the oldest burned nonce is evicted so the store can't grow without limit.
    fn burn_nonce(&mut self, account_id: &str, nonce: &str) -> bool {
        if nonce.is_empty() {
            return false;
        }
        let key = format!("acct:{account_id}:{nonce}");
        if !self.nonces.insert(key.clone()) {
            return false; // already burned → replay
        }
        self.nonce_fifo.push_back(key);
        while self.nonce_fifo.len() > MAX_NONCES {
            if let Some(old) = self.nonce_fifo.pop_front() {
                self.nonces.remove(&old);
            }
        }
        self.rev += 1;
        true
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

    fn admit_invoice(&mut self, account_id: &str, card: bool) -> Result<(), String> {
        let now = now_ms();
        if card {
            let hits = self.card_hits.entry(account_id.to_string()).or_default();
            hits.retain(|t| now - t < INVOICE_ACCT_WINDOW_MS);
            if hits.len() >= CARD_PER_ACCT {
                let retry = (INVOICE_ACCT_WINDOW_MS - (now - hits[0])).div_ceil(1000).max(1);
                eprintln!(
                    "scrai-server: CARD LIMIT — account {}… hit {CARD_PER_ACCT} card invoices/{} min (CARD_PER_ACCT in pay.rs, compiled in) — retry in {retry}s",
                    &account_id[..account_id.len().min(8)], INVOICE_ACCT_WINDOW_MS / 60_000
                );
                return Err(format!("too many card checkouts from this account — retry in ~{retry}s, or pay with a coin"));
            }
        }
        self.global_hits.retain(|t| now - t < 60_000);
        if self.global_hits.len() >= INVOICE_GLOBAL_PER_MIN {
            if now - self.global_cap_logged_at > 60_000 {
                self.global_cap_logged_at = now;
                eprintln!("scrai-server: INVOICE LIMIT — {INVOICE_GLOBAL_PER_MIN} invoices/min server-wide reached (INVOICE_GLOBAL_PER_MIN in pay.rs, compiled in) — refusing creates for up to 60 s");
            }
            return Err("the server is issuing too many invoices right now — retry in ~60s".into());
        }
        let hits = self.acct_hits.entry(account_id.to_string()).or_default();
        hits.retain(|t| now - t < INVOICE_ACCT_WINDOW_MS);
        if hits.len() >= INVOICE_PER_ACCT {
            let retry = (INVOICE_ACCT_WINDOW_MS - (now - hits[0])).div_ceil(1000).max(1);
            eprintln!(
                "scrai-server: INVOICE LIMIT — account {}… hit {INVOICE_PER_ACCT} invoices/{} min (INVOICE_PER_ACCT in pay.rs, compiled in) — retry in {retry}s",
                &account_id[..account_id.len().min(8)], INVOICE_ACCT_WINDOW_MS / 60_000
            );
            return Err(format!("too many invoices from this account — retry in ~{retry}s"));
        }
        hits.push(now);
        self.global_hits.push(now);
        if card {
            self.card_hits.entry(account_id.to_string()).or_default().push(now);
        }
        // Opportunistic prune so a throwaway swarm cannot grow the maps unboundedly.
        self.acct_hits.retain(|_, v| v.iter().any(|t| now - t < INVOICE_ACCT_WINDOW_MS));
        self.card_hits.retain(|_, v| v.iter().any(|t| now - t < INVOICE_ACCT_WINDOW_MS));
        Ok(())
    }

    /// Total unspent entitlement across all accounts (bought, not yet withdrawn to ecash).
    pub fn total_entitlement(&self) -> u64 {
        self.entitlements.values().sum()
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
    //
    // Three phases, like chat::reserve / run_provider / settle: `begin()` does every
    // state change that must be serialized (signature + nonce burn + invoice throttle)
    // on the dispatch loop and hands back the OUTBOUND work; `run_gateway()` does that
    // slow HTTP (BTCPay / Nyx LCD, 8–20 s timeouts) in a spawned task; `finish()` applies
    // the result on the loop. Before this split a slow LCD node stalled every chat
    // reserve/settle for up to 15 s (head-of-line on the single loop).

    /// Handle invoice.create / invoice.status / invoice.cancel / entitlement in one go
    /// (begin → gateway → finish inline). Tests and the fuzz targets use this; the server
    /// loop uses the split API so the gateway HTTP never runs on the loop.
    pub async fn handle(&mut self, request: &[u8], gateway: &Gateway) -> Vec<u8> {
        match self.begin(request, gateway) {
            PayStep::Reply(r) => r,
            PayStep::Pending(p) => {
                let outcome = run_gateway(p, gateway).await;
                self.finish(outcome, gateway)
            }
        }
    }

    /// PHASE 1 (loop side, fast): authenticate + throttle + look up. Returns either a
    /// final reply (validation error, cancel, already-paid invoice) or the outbound
    /// gateway work that a spawned task must run.
    pub fn begin(&mut self, request: &[u8], gateway: &Gateway) -> PayStep {
        let v: Value = serde_json::from_slice(request).unwrap_or(Value::Null);
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        let reply = match v.get("kind").and_then(|k| k.as_str()).unwrap_or("") {
            "invoice.create" => match self.begin_create(&v, &id) {
                Ok(pending) => return PayStep::Pending(pending),
                Err(reply) => reply,
            },
            "invoice.status" => match self.begin_status(&v, &id, gateway) {
                Ok(pending) => return PayStep::Pending(pending),
                Err(reply) => reply,
            },
            "invoice.cancel" => self.cancel(&v, &id),
            // H3: authenticate BEFORE any outbound work, then sweep ONLY this account's
            // still-pending invoices. A bare/unsigned `entitlement` no longer forces N
            // serial 20s gateway calls (which, via the sequential loop, wedged the server).
            "entitlement" => match self.account_owns(&v, "entitlement") {
                Some(account) => {
                    let candidates = self.pending_invoices_of(&account);
                    return PayStep::Pending(PayPending::Sweep { id, account, candidates });
                }
                None => err(&id, "account signature does not check out, or the nonce was reused"),
            },
            other => err(&id, &format!("unknown kind: {other}")),
        };
        PayStep::Reply(serde_json::to_vec(&reply).unwrap_or_default())
    }

    /// PHASE 3 (loop side, fast): apply what the gateway said and build the reply.
    pub fn finish(&mut self, outcome: PayOutcome, gateway: &Gateway) -> Vec<u8> {
        let reply = match outcome {
            PayOutcome::Create { id, account, usd, our_id, testnet, result } => {
                self.finish_create(&id, account, usd, our_id, testnet, result)
            }
            PayOutcome::Status { id, inv_id, paid } => {
                if paid {
                    self.settle(&inv_id);
                }
                self.status_reply(&id, &inv_id, gateway)
            }
            PayOutcome::Sweep { id, account, paid } => {
                for inv_id in &paid {
                    self.settle(inv_id);
                }
                json!({ "id": id, "entitlement": self.entitlement(&account) })
            }
        };
        serde_json::to_vec(&reply).unwrap_or_default()
    }

    /// This account's still-pending invoices worth re-checking (bounded outbound work:
    /// younger than ~48h past their window). Only ever called after that account's
    /// signature checked out (H3).
    fn pending_invoices_of(&self, account: &str) -> Vec<Inv> {
        let now = now_ms();
        self.invoices
            .values()
            .filter(|i| i.status != "paid" && now < i.expires_at + 48 * 3_600_000 && i.account_id == account)
            .cloned()
            .collect()
    }

    fn begin_create(&mut self, v: &Value, id: &Value) -> Result<PayPending, Value> {
        let usd = v.get("usd").and_then(|u| u.as_u64()).unwrap_or(0) as u32;
        let Some(account) = self.account_owns(v, &format!("invoice:{usd}")) else {
            return Err(err(id, "account signature does not check out, or the nonce was reused"));
        };
        // Fixed amounts only, so every purchase looks like everyone else's — a
        // free-form amount would be a fingerprint.
        let testnet = v.get("testnet").and_then(|t| t.as_bool()).unwrap_or(false);
        // The client picks the rail ("nyx" = native NYM on the Nyx chain); it is
        // deliberately NOT part of the account signature — it only selects HOW to
        // pay, never how much is credited.
        let wanted = v.get("method").and_then(|m| m.as_str()).unwrap_or("btc").to_string();
        if testnet {
            // A tester's $1, paid by the faucet on this host. Refused outright on a
            // production server — the flag is the server-side kill switch.
            if !is_testnet_server() {
                return Err(err(id, "this server does not accept testnet purchases"));
            }
            if usd != TESTNET_USD {
                return Err(err(id, &format!("a testnet purchase is ${TESTNET_USD} only")));
            }
            // Only the faucet's NYM may settle it (see `testnet_faucet_address`), so the
            // invoice must be native NYM and the pin must be configured.
            if wanted != "nyx" {
                return Err(err(id, "testnet purchases are paid in NYM by the faucet — pick NYM"));
            }
            if testnet_faucet_address().is_none() {
                return Err(err(id, "testnet purchases are not enabled on this server (no faucet wallet pinned)"));
            }
        } else if is_testnet_server() {
            // A testnet server watches a test chain, where every coin is free: a "real"
            // purchase here would be self-funded model spend. Invite-code testers only,
            // until the flag comes off.
            return Err(err(id, "this is a testnet server — purchases are $1 testnet credits funded by the faucet with an invite code"));
        } else {
            let tiers = purchase_tiers();
            if !tiers.contains(&usd) {
                let list = tiers.iter().map(|t| format!("${t}")).collect::<Vec<_>>().join(", ");
                return Err(err(id, &format!("purchases must be one of: {list}")));
            }
            if wanted == "card" {
                // The card rail sells the larger tiles only (fee + chargeback exposure, see
                // `card_min_usd`). The app greys smaller tiles; this is the authority.
                if !card_enabled() {
                    return Err(err(id, "card payments are not available on this server — pay with NYM or Bitcoin"));
                }
                let min = card_min_usd();
                if usd < min {
                    return Err(err(id, &format!("card purchases start at ${min} — pick a larger amount or pay with a coin")));
                }
            }
        }
        // Throttle BEFORE the external gateway call.
        if let Err(e) = self.admit_invoice(&account, wanted == "card") {
            return Err(err(id, &e));
        }
        let our_id = rand_hex(16);
        Ok(PayPending::Create { id: id.clone(), account, usd, our_id, wanted, testnet })
    }

    fn finish_create(
        &mut self,
        id: &Value,
        account: String,
        usd: u32,
        our_id: String,
        testnet: bool,
        result: Result<Raised, String>,
    ) -> Value {
        let raised = match result {
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
                testnet,
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
            "testnet": testnet,
        })
    }

    /// Every invoice raised as a testnet purchase, newest expiry first — for the faucet
    /// (which pays exactly the quoted `unym` to the memo) and for scrai-admin's count.
    /// Only native-NYM ones carry a memo/unym; the faucet ignores the rest.
    pub fn testnet_invoices(&self) -> Vec<TestnetInv> {
        let mut out: Vec<TestnetInv> = self
            .invoices
            .values()
            .filter(|i| i.testnet)
            .map(|i| TestnetInv {
                id: i.id.clone(),
                memo: if i.method == "nyx" { i.provider_ref.clone() } else { String::new() },
                amount_usd: i.amount_usd,
                unym: i.expected_unym,
                status: i.status.clone(),
                expires_at: i.expires_at,
            })
            .collect();
        out.sort_by(|a, b| b.expires_at.cmp(&a.expires_at));
        out
    }

    /// Deliberately unauthenticated (like the TS server): the invoice id is a
    /// random id the client just received, and the reply reveals nothing usable.
    /// Ok = poll the gateway too (a webhook that never arrived must not leave a paying
    /// customer stuck; also for locally-expired invoices — BTCPay keeps watching, and a
    /// late on-chain confirmation still counts). Err = final reply, no outbound work.
    fn begin_status(&mut self, v: &Value, id: &Value, gateway: &Gateway) -> Result<PayPending, Value> {
        self.expire_stale();
        let inv_id = v.get("invoiceId").and_then(|i| i.as_str()).unwrap_or("").to_string();
        let Some(inv) = self.invoices.get(&inv_id).cloned() else {
            return Err(err(id, "unknown invoice"));
        };
        if inv.status == "paid" {
            return Err(self.status_reply(id, &inv_id, gateway));
        }
        Ok(PayPending::Status { id: id.clone(), inv })
    }

    fn status_reply(&self, id: &Value, inv_id: &str, gateway: &Gateway) -> Value {
        let Some(now) = self.invoices.get(inv_id) else {
            return err(id, "unknown invoice");
        };
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

/// What `Pay::begin` hands back: a finished reply, or gateway work for a spawned task.
pub enum PayStep {
    Reply(Vec<u8>),
    Pending(PayPending),
}

/// Outbound gateway work, prepared on the loop (authenticated + throttled) and run
/// off it by `run_gateway`. Carries everything `finish` needs — no loop state.
pub enum PayPending {
    Create { id: Value, account: String, usd: u32, our_id: String, wanted: String, testnet: bool },
    Status { id: Value, inv: Inv },
    Sweep { id: Value, account: String, candidates: Vec<Inv> },
}

/// What the gateway said, to be applied on the loop by `Pay::finish`.
pub enum PayOutcome {
    Create { id: Value, account: String, usd: u32, our_id: String, testnet: bool, result: Result<Raised, String> },
    Status { id: Value, inv_id: String, paid: bool },
    Sweep { id: Value, account: String, paid: Vec<String> },
}

impl PayOutcome {
    /// The request kind this settles — for the "handled …" log line.
    pub fn kind(&self) -> &'static str {
        match self {
            PayOutcome::Create { .. } => "invoice.create",
            PayOutcome::Status { .. } => "invoice.status",
            PayOutcome::Sweep { .. } => "entitlement",
        }
    }
}

/// PHASE 2 (off the loop, slow): the gateway HTTP. Pure — touches no paywall state, so
/// any number of these can run concurrently while chats keep flowing.
pub async fn run_gateway(pending: PayPending, gateway: &Gateway) -> PayOutcome {
    match pending {
        PayPending::Create { id, account, usd, our_id, wanted, testnet } => {
            let result = gateway.create_invoice(usd, &our_id, &wanted, testnet).await;
            PayOutcome::Create { id, account, usd, our_id, testnet, result }
        }
        PayPending::Status { id, inv } => {
            let paid = matches!(gateway.check_status(&inv).await.as_deref(), Ok("paid"));
            PayOutcome::Status { id, inv_id: inv.id, paid }
        }
        PayPending::Sweep { id, account, candidates } => {
            let mut paid = Vec::new();
            for inv in candidates {
                if let Ok("paid") = gateway.check_status(&inv).await.as_deref() {
                    paid.push(inv.id);
                }
            }
            PayOutcome::Sweep { id, account, paid }
        }
    }
}

/// The outcome when no gateway slot freed up in time: a create fails visibly (the
/// client retries), a status/sweep just reports "nothing new" — the next poll re-checks.
pub fn gateway_busy(pending: PayPending) -> PayOutcome {
    match pending {
        PayPending::Create { id, account, usd, our_id, testnet, .. } => PayOutcome::Create {
            id,
            account,
            usd,
            our_id,
            testnet,
            result: Err("the payment gateway is busy right now — please try again in a moment".into()),
        },
        PayPending::Status { id, inv } => PayOutcome::Status { id, inv_id: inv.id, paid: false },
        PayPending::Sweep { id, account, .. } => PayOutcome::Sweep { id, account, paid: Vec::new() },
    }
}

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
    /// Cards (Mollie hosted checkout) — orthogonal to the coin rails, serves "card".
    card: CardRail,
}

impl Gateway {
    pub fn from_env() -> Gateway {
        Gateway { rail: Rail::from_env(), nyx: crate::nyx::Nyx::from_env(), card: CardRail::from_env() }
    }

    pub fn name(&self) -> String {
        let mut n = self.rail.name().to_string();
        if self.nyx.is_some() {
            n.push_str("+nyx");
        }
        if let CardRail::Mollie { .. } = self.card {
            n.push_str("+mollie");
        }
        n
    }

    /// True only when NO real money can arrive: the fake dev rail AND no Nyx rail.
    /// A single-authority issuer may run against this (dev); against real money it
    /// must not (H9).
    pub fn is_fake(&self) -> bool {
        self.nyx.is_none() && matches!(self.card, CardRail::None) && matches!(self.rail, Rail::Fake)
    }

    async fn create_invoice(&self, usd: u32, reference: &str, wanted: &str, testnet: bool) -> Result<Raised, String> {
        if wanted == "card" {
            // Never the coin fallback: a client that asked for a card checkout must not be
            // handed a BTC address it did not expect (begin_create refuses this earlier too).
            let raised = self.card.create_invoice(usd, reference).await?;
            return Ok(Raised { raised, method: "card".into(), expected_unym: 0 });
        }
        if wanted == "nyx" {
            if let Some(nyx) = &self.nyx {
                let (raised, expected_unym) = nyx.create_invoice(usd).await?;
                return Ok(Raised { raised, method: "nyx".into(), expected_unym });
            }
        }
        if testnet {
            // never the processor-rail fallback: a testnet invoice is NYM from the faucet or nothing
            return Err("testnet purchases need the native NYM rail, which is not configured here".into());
        }
        let raised = self.rail.create_invoice(usd, reference).await?;
        Ok(Raised { raised, method: "btc".into(), expected_unym: 0 })
    }

    async fn check_status(&self, inv: &Inv) -> Result<String, String> {
        if inv.method == "nyx" {
            let Some(nyx) = &self.nyx else {
                return Err("this invoice is native-NYM but no Nyx rail is configured".into());
            };
            // Testnet invoice: only the faucet wallet's transfer counts. No pin → never paid
            // (fail closed; `begin_create` refuses such invoices up front anyway).
            let pin = if inv.testnet {
                Some(testnet_faucet_address().ok_or("testnet invoice but SCRAI_TESTNET_FAUCET_ADDRESS is unset — refusing to settle")?)
            } else {
                None
            };
            return nyx.check_paid(&inv.provider_ref, inv.expected_unym, pin.as_deref()).await;
        }
        if inv.method == "card" {
            return self.card.check_status(&inv.provider_ref).await;
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
            // The fake rail must never coexist with a real one: with NYX_* or BTCPAY_* also
            // set, `is_fake()` would read false (real-money interlock passes) while every
            // non-nyx invoice still settled for free. Refuse to boot in that mixed state.
            let real = [
                "NYX_RECEIVE_ADDRESS", "NYX_LCD_URL", "BTCPAY_URL", "BTCPAY_STORE_ID", "BTCPAY_API_KEY",
                "MOLLIE_API_KEY", "MOLLIE_API_KEY_TESTNET", "MOLLIE_API_KEY_MAINNET",
            ]
            .iter()
            .any(|k| std::env::var(k).map(|v| !v.trim().is_empty()).unwrap_or(false));
            if real {
                eprintln!("scrai-server: FATAL: SCRAI_FAKE_PAYMENTS=1 together with a real payment rail (NYX_*/BTCPAY_*/MOLLIE_*) — remove one. Refusing to start.");
                std::process::exit(1);
            }
            eprintln!("scrai-server: SCRAI_FAKE_PAYMENTS=1 — invoices settle on first poll. DEV ONLY.");
            return Rail::Fake;
        }
        // Network-scoped (BTCPAY_URL_MAINNET / _TESTNET, legacy BTCPAY_URL fallback).
        match (
            crate::net_var("BTCPAY_URL"),
            crate::net_var("BTCPAY_STORE_ID"),
            crate::net_var("BTCPAY_API_KEY"),
        ) {
            (Some(u), Some(s), Some(k)) => {
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
            s => format!("BTCPay {s}: {}", msg.chars().take(200).collect::<String>()),
        });
    }
    Ok(body)
}


// ---------------------------------------------------------------------------
// Cards via Mollie — hosted checkout, no card data here, no webhook (the server has
// no clearnet port, so the client's 10 s status poll drives `GET /v2/payments/{id}`).
// See docs/card-payments.md §4 for every fact this code leans on.
// ---------------------------------------------------------------------------

const MOLLIE_API: &str = "https://api.mollie.com/v2";
/// Where Mollie sends the browser after checkout when MOLLIE_REDIRECT_URL is unset:
/// the static thank-you page on the download/faucet site (no order id, no cookie).
const DEFAULT_PAID_URL: &str = "https://scrai-faucet.hermes-stakepool.de/paid";

pub enum CardRail {
    Mollie { api_key: String, redirect_url: String },
    None,
}

impl CardRail {
    pub fn from_env() -> CardRail {
        // Network-scoped like BTCPay (MOLLIE_API_KEY_MAINNET / _TESTNET, bare fallback).
        // A `test_…` key is Mollie's test mode (EUR only, checkout lets you pick the
        // outcome); a `live_…` key moves real money.
        match crate::net_var("MOLLIE_API_KEY") {
            Some(k) => {
                let redirect_url = crate::net_var("MOLLIE_REDIRECT_URL")
                    .filter(|u| u.starts_with("https://"))
                    .or_else(|| {
                        std::env::var("SCRAI_FAUCET_URL")
                            .ok()
                            .filter(|u| u.starts_with("https://"))
                            .map(|u| format!("{}/paid", u.trim_end_matches('/')))
                    })
                    .unwrap_or_else(|| DEFAULT_PAID_URL.to_string());
                CardRail::Mollie { api_key: k.trim().to_string(), redirect_url }
            }
            None => CardRail::None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            CardRail::Mollie { .. } => "mollie",
            CardRail::None => "none",
        }
    }

    async fn create_invoice(&self, usd: u32, reference: &str) -> Result<RaisedInvoice, String> {
        let CardRail::Mollie { api_key, redirect_url } = self else {
            return Err("card payments are not configured on this server".into());
        };
        // Test mode is EUR-only at Mollie, so the test rail charges the tile's number in
        // EUR 1:1 — a placeholder amount, nothing is converted. Live charges the USD tile
        // (the SCRAI price is fixed per USD; Mollie converts to the payout currency).
        let currency = if api_key.starts_with("test_") { "EUR" } else { "USD" };
        let body = json!({
            "amount": { "currency": currency, "value": format!("{usd}.00") },
            "description": "ScrambleAI credit",
            "redirectUrl": redirect_url,
            "method": "creditcard",
            // Our random invoice id only — Mollie never learns the account, a session
            // key or anything usage-related.
            "metadata": { "orderId": reference },
            "locale": "en_US",
        });
        let v = mollie(
            api_key,
            crate::http::client()
                .post(format!("{MOLLIE_API}/payments"))
                // Keyed by OUR invoice id: if this create is ever re-sent (a retry inside
                // the hour), Mollie hands back the same payment instead of a second one.
                .header("Idempotency-Key", reference)
                .json(&body),
        )
        .await?;
        let provider_ref = v.get("id").and_then(|i| i.as_str()).unwrap_or_default().to_string();
        if !provider_ref.starts_with("tr_") {
            return Err("Mollie returned no payment id".into());
        }
        let checkout = v
            .pointer("/_links/checkout/href")
            .and_then(|h| h.as_str())
            .unwrap_or_default()
            .to_string();
        // The client opens this in the OS browser — it must be Mollie's own hosted page.
        if !is_mollie_url(&checkout) {
            return Err("Mollie returned no hosted checkout link".into());
        }
        // `expiresAt` is RFC 3339; cards expire after ~15–30 min at Mollie. Fall back to
        // 15 min if it is missing rather than predicting.
        let expires_at = v
            .get("expiresAt")
            .and_then(|e| e.as_str())
            .and_then(rfc3339_ms)
            .unwrap_or_else(|| now_ms() + 15 * 60_000);
        Ok(RaisedInvoice {
            provider_ref,
            pay_to: String::new(),
            instruction: "Finish the payment in your browser — the credit lands here automatically.".into(),
            options: json!([{ "method": "card", "checkout": checkout }]),
            expires_at,
        })
    }

    /// "paid" | "pending" | "expired". Only Mollie's `paid` settles — `authorized` is the
    /// capture flow we do not use. A 429 (rate limit) is "nothing new yet": the next
    /// 10 s poll re-asks, and our volume is nowhere near the limit anyway.
    async fn check_status(&self, provider_ref: &str) -> Result<String, String> {
        let CardRail::Mollie { api_key, .. } = self else {
            return Err("card payments are not configured on this server".into());
        };
        if !provider_ref.starts_with("tr_") || !provider_ref.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err("not a Mollie payment reference".into());
        }
        let v = match mollie(api_key, crate::http::client().get(format!("{MOLLIE_API}/payments/{provider_ref}"))).await {
            Ok(v) => v,
            Err(MollieErr::RateLimited) => return Ok("pending".into()),
            Err(MollieErr::Other(e)) => return Err(e),
        };
        Ok(match v.get("status").and_then(|s| s.as_str()).unwrap_or("") {
            "paid" => "paid",
            "canceled" | "expired" | "failed" => "expired",
            _ => "pending",
        }
        .into())
    }
}

/// Mollie's hosted checkout lives on mollie.com (www.mollie.com/checkout/…); nothing
/// else may be handed to the client as a link to open.
fn is_mollie_url(u: &str) -> bool {
    let Some(rest) = u.strip_prefix("https://") else { return false };
    let host = rest.split('/').next().unwrap_or("");
    host == "mollie.com" || host.ends_with(".mollie.com")
}

enum MollieErr {
    RateLimited,
    Other(String),
}

impl From<MollieErr> for String {
    fn from(e: MollieErr) -> String {
        match e {
            MollieErr::RateLimited => "Mollie is rate-limiting us — try again in a moment".into(),
            MollieErr::Other(s) => s,
        }
    }
}

async fn mollie(api_key: &str, req: reqwest::RequestBuilder) -> Result<Value, MollieErr> {
    let res = req
        .header("authorization", format!("Bearer {api_key}"))
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| MollieErr::Other(format!("Mollie unreachable: {e}")))?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        // Mollie errors are {status, title, detail}; `detail` names the offending field.
        let detail = body.get("detail").and_then(|m| m.as_str()).unwrap_or("");
        return Err(match status.as_u16() {
            401 | 403 => MollieErr::Other("Mollie rejected the API key — check MOLLIE_API_KEY".into()),
            404 => MollieErr::Other("Mollie does not know this payment".into()),
            429 => MollieErr::RateLimited,
            s => MollieErr::Other(format!("Mollie {s}: {}", detail.chars().take(200).collect::<String>())),
        });
    }
    Ok(body)
}

/// `2026-08-29T10:47:54+00:00` → unix ms. Mollie's timestamps are RFC 3339 with a
/// numeric offset (or `Z`); anything else parses as None and the caller falls back.
fn rfc3339_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>());
    let (y, mo, da) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    // time part ends where the offset starts: 'Z', '+' or '-'
    let off_pos = rest.find(['Z', '+', '-'])?;
    let (time, off) = rest.split_at(off_pos);
    let time = time.split('.').next()?; // drop fractional seconds
    let mut t = time.split(':').map(|x| x.parse::<i64>());
    let (h, mi, se) = (t.next()?.ok()?, t.next()?.ok()?, t.next().unwrap_or(Ok(0)).ok()?);
    let off_secs: i64 = if off == "Z" {
        0
    } else {
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let mut o = off[1..].split(':').map(|x| x.parse::<i64>());
        let (oh, om) = (o.next()?.ok()?, o.next().unwrap_or(Ok(0)).ok()?);
        sign * (oh * 3600 + om * 60)
    };
    // days from civil (Howard Hinnant), valid for the proleptic Gregorian calendar
    let (y2, m2) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + da - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + se - off_secs;
    u64::try_from(secs).ok().map(|s| s * 1000)
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    use ed25519_dalek::{Signer, SigningKey};

    /// SCRAI_TESTNET is process-global and decides whether a plain $5 create is accepted,
    /// so the one test that flips it takes the write side; invoice-creating tests read.
    static ENV_LOCK: std::sync::RwLock<()> = std::sync::RwLock::new(());

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
        let _env = ENV_LOCK.read().unwrap_or_else(|e| e.into_inner());
        let (sk, pem, aid) = account();
        let mut pay = Pay::default();
        let gw = Gateway { rail: Rail::Fake, nyx: None, card: CardRail::None };

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

    // Faucet: on a testnet server (SCRAI_TESTNET=1) the ONLY purchase is a $1 `testnet:true`
    // invoice in native NYM, and only with the faucet wallet pinned; a "real" purchase there
    // is refused (test chain = free coins). Without the env the flag itself is refused. The
    // flag survives into the invoice so admin/faucet can see it.
    #[tokio::test]
    async fn testnet_create_is_one_dollar_and_gated_by_env() {
        let _env = ENV_LOCK.write().unwrap_or_else(|e| e.into_inner());
        let (sk, pem, aid) = account();
        let gw = Gateway { rail: Rail::Fake, nyx: None, card: CardRail::None };
        // nonces burn on first sight (even for a refused create), so every call gets its own
        let req = |usd: u32, testnet: bool, method: &str, n: &str| json!({"kind":"invoice.create","id":"r1","publicKey":pem,
            "usd":usd,"testnet":testnet,"method":method,"nonce":n,
            "sig":signed(&sk,&aid,&format!("invoice:{usd}"),n)});
        let refused = |pay: &mut Pay, v: Value, needle: &str| {
            let PayStep::Reply(r) = pay.begin(v.to_string().as_bytes(), &gw) else { panic!("must be refused ({needle})") };
            let r: Value = serde_json::from_slice(&r).unwrap();
            assert!(r["error"].as_str().unwrap_or("").contains(needle), "{r}");
        };

        std::env::remove_var("SCRAI_TESTNET");
        std::env::remove_var("SCRAI_TESTNET_FAUCET_ADDRESS");
        let mut pay = Pay::default();
        refused(&mut pay, req(1, true, "nyx", "n1"), "testnet");
        // $1 is not a normal tier either
        refused(&mut pay, req(1, false, "nyx", "n2"), "one of");

        std::env::set_var("SCRAI_TESTNET", "1");
        // a normal purchase on a testnet server is refused outright
        refused(&mut pay, req(5, false, "nyx", "n3"), "testnet server");
        refused(&mut pay, req(5, true, "nyx", "n4"), "$1");
        // NYM only — the faucet cannot pay a BTCPay invoice
        refused(&mut pay, req(1, true, "btc", "n5"), "NYM");
        // fail closed: no faucet wallet pinned → no testnet purchase at all
        refused(&mut pay, req(1, true, "nyx", "n6"), "faucet wallet");

        std::env::set_var("SCRAI_TESTNET_FAUCET_ADDRESS", "n1faucet");
        let PayStep::Pending(p) = pay.begin(req(1, true, "nyx", "n7").to_string().as_bytes(), &gw) else { panic!("create needs the gateway") };
        let r: Value = serde_json::from_slice(&pay.finish(run_gateway(p, &gw).await, &gw)).unwrap();
        std::env::remove_var("SCRAI_TESTNET");
        std::env::remove_var("SCRAI_TESTNET_FAUCET_ADDRESS");
        // the gate passed; this test gateway has no NYM rail, and a testnet invoice must
        // never fall back to the processor rail — so it is refused there, not raised on BTC
        assert!(r["error"].as_str().unwrap_or("").contains("NYM rail"), "{r}");
        assert!(pay.testnet_invoices().is_empty());

        // the flag rides in the durable record and the faucet view picks exactly those
        pay.invoices.insert("t1".into(), Inv { id: "t1".into(), provider_ref: "SCRAI-MEMO2345".into(), account_id: aid.clone(),
            amount_usd: 1, amount_scrai: SCRAI_PER_USD, method: "nyx".into(), status: "pending".into(),
            expires_at: now_ms() + 60_000, expected_unym: 59_000_000, testnet: true });
        pay.invoices.insert("r1".into(), Inv { id: "r1".into(), provider_ref: "SCRAI-REAL2345".into(), account_id: aid,
            amount_usd: 5, amount_scrai: 5 * SCRAI_PER_USD, method: "nyx".into(), status: "pending".into(),
            expires_at: now_ms() + 60_000, expected_unym: 295_000_000, testnet: false });
        let t = pay.testnet_invoices();
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].amount_usd, t[0].memo.as_str(), t[0].unym), (1, "SCRAI-MEMO2345", 59_000_000));
    }

    // H2 (pay): the split API. Two status polls for the same invoice can be in flight at
    // once (each spawned off the loop); settling both credits exactly once, and a
    // "gateway busy" outcome leaves the invoice pending with nothing credited.
    #[tokio::test]
    async fn split_begin_finish_credits_once_and_busy_leaves_it_pending() {
        let _env = ENV_LOCK.read().unwrap_or_else(|e| e.into_inner());
        let (sk, pem, aid) = account();
        let mut pay = Pay::default();
        let gw = Gateway { rail: Rail::Fake, nyx: None, card: CardRail::None };
        let req = json!({"kind":"invoice.create","id":"r1","publicKey":pem,"usd":5,
            "nonce":"n1","sig":signed(&sk,&aid,"invoice:5","n1")});
        let PayStep::Pending(p) = pay.begin(req.to_string().as_bytes(), &gw) else { panic!("create needs the gateway") };
        assert!(matches!(p, PayPending::Create { .. }));
        let r: Value = serde_json::from_slice(&pay.finish(run_gateway(p, &gw).await, &gw)).unwrap();
        let inv_id = r["invoiceId"].as_str().unwrap().to_string();

        let st = json!({"kind":"invoice.status","id":"r2","invoiceId":inv_id}).to_string();
        // Busy gateway: no credit, still pending, next poll will re-check.
        let PayStep::Pending(p) = pay.begin(st.as_bytes(), &gw) else { panic!("pending invoice polls the gateway") };
        let busy: Value = serde_json::from_slice(&pay.finish(gateway_busy(p), &gw)).unwrap();
        assert_eq!(busy["status"].as_str(), Some("pending"));
        assert_eq!(busy["entitlement"].as_u64(), Some(0));

        // Two polls in flight at once, both come back "paid" → credited exactly once.
        let PayStep::Pending(a) = pay.begin(st.as_bytes(), &gw) else { panic!() };
        let PayStep::Pending(b) = pay.begin(st.as_bytes(), &gw) else { panic!() };
        let (oa, ob) = (run_gateway(a, &gw).await, run_gateway(b, &gw).await);
        let ra: Value = serde_json::from_slice(&pay.finish(oa, &gw)).unwrap();
        let rb: Value = serde_json::from_slice(&pay.finish(ob, &gw)).unwrap();
        assert_eq!(ra["status"].as_str(), Some("paid"));
        assert_eq!(rb["entitlement"].as_u64(), Some(500_000));
        assert_eq!(pay.total_entitlement(), 500_000);

        // Once paid, a status poll is answered on the loop — no gateway work at all.
        assert!(matches!(pay.begin(st.as_bytes(), &gw), PayStep::Reply(_)));
    }

    #[tokio::test]
    async fn bad_signature_nonce_replay_and_odd_amounts_are_refused() {
        let _env = ENV_LOCK.read().unwrap_or_else(|e| e.into_inner());
        let (sk, pem, aid) = account();
        let mut pay = Pay::default();
        let gw = Gateway { rail: Rail::Fake, nyx: None, card: CardRail::None };

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

    /// Card rules live in begin_create: no Mollie key → no card sales at all; with a key,
    /// tiles below SCRAI_CARD_MIN_USD are refused BEFORE any gateway call. Env-mutating →
    /// write lock.
    #[tokio::test]
    async fn card_purchases_need_a_key_and_the_minimum_tile() {
        let _env = ENV_LOCK.write().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("SCRAI_TESTNET");
        std::env::remove_var("SCRAI_CARD_MIN_USD");
        for k in ["MOLLIE_API_KEY", "MOLLIE_API_KEY_TESTNET", "MOLLIE_API_KEY_MAINNET"] {
            std::env::remove_var(k);
        }
        let (sk, pem, aid) = account();
        let mut pay = Pay::default();
        let gw = Gateway { rail: Rail::Fake, nyx: None, card: CardRail::None };

        // no key anywhere → card is not for sale, even for a valid tile
        let req = json!({"kind":"invoice.create","id":"r","publicKey":pem,"usd":10,"method":"card",
            "nonce":"c1","sig":signed(&sk,&aid,"invoice:10","c1")});
        let r: Value = serde_json::from_slice(&pay.handle(req.to_string().as_bytes(), &gw).await).unwrap();
        assert!(r.get("error").and_then(|e| e.as_str()).unwrap().contains("not available"));

        // key present → $5 is below the default $10 minimum
        std::env::set_var("MOLLIE_API_KEY_TESTNET", "test_dummy");
        assert!(card_enabled());
        let req = json!({"kind":"invoice.create","id":"r","publicKey":pem,"usd":5,"method":"card",
            "nonce":"c2","sig":signed(&sk,&aid,"invoice:5","c2")});
        let r: Value = serde_json::from_slice(&pay.handle(req.to_string().as_bytes(), &gw).await).unwrap();
        assert!(r.get("error").and_then(|e| e.as_str()).unwrap().contains("start at $10"));

        // $10 passes the paywall rules and reaches the (unconfigured) card rail — never the coin fallback
        let req = json!({"kind":"invoice.create","id":"r","publicKey":pem,"usd":10,"method":"card",
            "nonce":"c3","sig":signed(&sk,&aid,"invoice:10","c3")});
        let r: Value = serde_json::from_slice(&pay.handle(req.to_string().as_bytes(), &gw).await).unwrap();
        assert!(r.get("error").and_then(|e| e.as_str()).unwrap().contains("not configured"));
        assert!(r.get("invoiceId").is_none());

        // the same coin tile still sells as before
        let req = json!({"kind":"invoice.create","id":"r","publicKey":pem,"usd":5,
            "nonce":"c4","sig":signed(&sk,&aid,"invoice:5","c4")});
        let r: Value = serde_json::from_slice(&pay.handle(req.to_string().as_bytes(), &gw).await).unwrap();
        assert!(r.get("invoiceId").is_some());
        std::env::remove_var("MOLLIE_API_KEY_TESTNET");
    }

    #[tokio::test]
    async fn late_confirmation_is_swept_in_by_the_entitlement_check() {
        let _env = ENV_LOCK.read().unwrap_or_else(|e| e.into_inner());
        let (sk, pem, aid) = account();
        let mut pay = Pay::default();
        let gw = Gateway { rail: Rail::Fake, nyx: None, card: CardRail::None };

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
        let _env = ENV_LOCK.read().unwrap_or_else(|e| e.into_inner());
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
        let gw = Gateway { rail: Rail::Fake, nyx: None, card: CardRail::None };
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

#[cfg(test)]
mod card_tests {
    use super::*;

    #[test]
    fn rfc3339_parses_mollie_timestamps() {
        // 2026-08-29T10:47:54+00:00 = 1788000474 (checked against `date -u -j`)
        assert_eq!(rfc3339_ms("2026-08-29T10:47:54+00:00"), Some(1_788_000_474_000));
        assert_eq!(rfc3339_ms("2026-08-29T10:47:54Z"), Some(1_788_000_474_000));
        // +02:00 is two hours EARLIER in UTC
        assert_eq!(rfc3339_ms("2026-08-29T12:47:54+02:00"), Some(1_788_000_474_000));
        assert_eq!(rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_ms("garbage"), None);
        assert_eq!(rfc3339_ms(""), None);
    }

    #[test]
    fn only_mollie_hosted_checkout_is_a_link() {
        assert!(is_mollie_url("https://www.mollie.com/checkout/select-method/7UhSN1zuXS"));
        assert!(is_mollie_url("https://mollie.com/x"));
        assert!(!is_mollie_url("http://www.mollie.com/checkout"));
        assert!(!is_mollie_url("https://evil-mollie.com/checkout"));
        assert!(!is_mollie_url("https://mollie.com.evil.net/checkout"));
        assert!(!is_mollie_url(""));
    }

    #[test]
    fn card_min_defaults_to_ten() {
        std::env::remove_var("SCRAI_CARD_MIN_USD");
        assert_eq!(card_min_usd(), 10);
    }
}
