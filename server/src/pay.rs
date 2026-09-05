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
// FAKE_PAYMENTS=1 — settles on first poll and says so loudly), native NYM
// (nyx.rs), and cards via Mollie (MOLLIE_API_KEY — hosted checkout, polled, see
// docs/card-payments.md).
// ---------------------------------------------------------------------------

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use scrai_core::auth;
use scrai_core::coconut::TOKU_PER_USD;
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
/// Invite-code checks per account and window. A code is 12 random characters out of 31,
/// so guessing is hopeless anyway — this is here so the check cannot be used as a cheap
/// oracle, and so a stuck client cannot hammer the ledger.
const CODE_CHECKS_PER_ACCT: usize = 12;
const INVOICE_ACCT_WINDOW_MS: u64 = 600_000;
/// H4: hard cap on the burned-nonce store (oldest evicted past this). Large enough that a
/// legit client never bumps into it, small enough that a signed-nonce flood can't OOM.
const MAX_NONCES: usize = 100_000;
/// Server-wide invoice.create rate (per minute). An ABUSE brake, not a capacity limit:
/// every create is a payment-provider call + a persisted invoice row, and accounts are
/// free to mint — so a griefer must not be able to raise thousands. The server itself
/// handles creates in ~2 s under load. Sized for a launch spike (a post going round =
/// ~50–100 buys in the peak minute) while still capping a flood at 7,200/h. The old
/// compiled-in 30 turned away 10 of 40 simultaneous buyers in the load test (2026-09-02).
const INVOICE_GLOBAL_PER_MIN_DEFAULT: usize = 120;
fn invoice_global_per_min() -> usize {
    static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        crate::cfg("INVOICE_PER_MIN")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(INVOICE_GLOBAL_PER_MIN_DEFAULT)
    })
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn purchase_tiers() -> Vec<u32> {
    crate::cfg("PURCHASE_TIERS")
        .ok()
        .map(|s| s.split(',').filter_map(|t| t.trim().parse().ok()).collect())
        .filter(|v: &Vec<u32>| !v.is_empty())
        .unwrap_or_else(|| vec![5, 10, 20, 50])
}

/// Smallest tile a card may buy (`CARD_MIN_USD`, default $10). Card fees carry a
/// fixed €0.25 part that eats a third of a $1 tile, and a card payment can be charged
/// back for weeks after the credit has been withdrawn as unlinkable ecash — so the card
/// rail only sells tiles where the fee is a rounding error and the exposure is bounded.
/// Reported to clients with the catalog so the app greys the smaller tiles itself.
pub fn card_min_usd() -> u32 {
    crate::cfg("CARD_MIN_USD").ok().and_then(|v| v.trim().parse().ok()).filter(|v| *v > 0).unwrap_or(10)
}

/// True when a Mollie key is configured — the client shows the card row only then.
pub fn card_enabled() -> bool {
    matches!(CardRail::from_env(), CardRail::Mollie { .. })
}

/// What the catalog reply tells the app about cards: whether the row exists at all, the
/// smallest tile it may buy, and which methods Mollie's checkout will offer (one source
/// of truth — never hardcoded in the client). `methods` is what is enabled in the Mollie
/// dashboard right now, fetched from `GET /v2/methods` and cached for 10 minutes, so
/// switching PayPal or Wero on there changes the app's label without a build or deploy.
/// Which rails this server can actually raise an invoice on. Derived from what is
/// configured, so turning a rail off is deleting its variables — not editing the app and
/// shipping a build. The app greys out whatever is missing instead of offering a tile
/// that fails on tap; the authority for what is accepted stays `begin_create`.
pub fn rails_info() -> Value {
    json!({
        "nyx": crate::nyx::Nyx::from_env().is_some(),
        "btc": btc_enabled(),
        "card": card_enabled(),
        "invite": faucet_address().is_some(),
        "inviteUsd": TESTNET_USD,
    })
}

/// A coin processor is configured (BTCPay or CoinGate), or the dev rail stands in.
fn btc_enabled() -> bool {
    if fake_payments_enabled() {
        return true;
    }
    coingate_from_env().is_some()
        || (crate::net_var("BTCPAY_URL").is_some()
            && crate::net_var("BTCPAY_STORE_ID").is_some()
            && crate::net_var("BTCPAY_API_KEY").is_some())
}

pub async fn card_info() -> Value {
    let enabled = card_enabled();
    let methods = if enabled { mollie_methods().await } else { Vec::new() };
    json!({ "enabled": enabled, "minUsd": card_min_usd(), "methods": methods })
}

/// `[{id, label}]` of the checkout methods enabled for our Mollie profile. Empty on any
/// failure (the app then says "Card & more"). Cached: the catalog is fetched by every
/// client start, Mollie's list changes once a quarter.
async fn mollie_methods() -> Vec<Value> {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static CACHE: Mutex<Option<(Instant, Vec<Value>)>> = Mutex::new(None);
    if let Ok(c) = CACHE.lock() {
        if let Some((at, m)) = c.as_ref() {
            if at.elapsed() < Duration::from_secs(600) {
                return m.clone();
            }
        }
    }
    let CardRail::Mollie { api_key, .. } = CardRail::from_env() else { return Vec::new() };
    let fetched = match mollie(&api_key, crate::http::client().get(format!("{MOLLIE_API}/methods"))).await {
        Ok(v) => v
            .pointer("/_embedded/methods")
            .and_then(|m| m.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let id = m.get("id").and_then(|i| i.as_str())?;
                        // Mollie's `status` is "activated" for live methods; the list is
                        // already filtered to active ones, but don't rely on it.
                        if m.get("status").and_then(|s| s.as_str()).is_some_and(|s| s != "activated") {
                            return None;
                        }
                        let label = match id {
                            "creditcard" => "Card",
                            "applepay" => "Apple Pay",
                            "googlepay" => "Google Pay",
                            "paypal" => "PayPal",
                            "wero" => "Wero",
                            "ideal" => "iDEAL",
                            "bancontact" => "Bancontact",
                            _ => m.get("description").and_then(|d| d.as_str()).unwrap_or(id),
                        };
                        Some(json!({ "id": id, "label": label }))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        Err(e) => {
            eprintln!("scrai-server: mollie /methods failed ({e:?}) — app shows the generic card label");
            Vec::new()
        }
    };
    if let Ok(mut c) = CACHE.lock() {
        *c = Some((Instant::now(), fetched.clone()));
    }
    fetched
}

/// Testnet mode (`TESTNET=1`): the ONE extra thing it enables is a $1 invoice
/// flagged `testnet:true`, which the faucet on the same host pays for a tester. The
/// flag is reported to clients so the app can offer the toggle; without it a client
/// asking for a testnet purchase is refused. This is the kill switch: unset it (or
/// set 0) and restart, and both server and every client fall back to normal tiers.
/// The ONE place that answers "is the fake payment rail on?" (FAKE_PAYMENTS=1).
/// Everything that must only exist on a no-real-money server (the load-test mock
/// provider) asks here, so the real-money interlocks stay in this file.
pub fn fake_payments_enabled() -> bool {
    crate::cfg("FAKE_PAYMENTS").as_deref() == Ok("1")
}

pub fn is_testnet_server() -> bool {
    crate::cfg("TESTNET")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// The only amount a testnet (faucet-paid) purchase may have.
pub const TESTNET_USD: u32 = 1;

/// The faucet wallet — the ONLY address whose NYM settles an invite invoice. Without this
/// pin anyone could raise $1 invite invoices and fund them from their own wallet; on the
/// testnet chain the coins are free, and on mainnet the pin is still what keeps the $1
/// tile tied to the faucet. Every such credit is real model spend either way.
/// Unset → invite purchases are refused (fail closed).
///
/// Network-scoped (`_MAINNET` / `_TESTNET`), and listed in `MONEY_RAILS` so a box that
/// only carries the testnet wallet refuses to boot as a mainnet server. The pre-rename
/// `TESTNET_FAUCET_ADDRESS` still resolves, so an existing .env keeps working.
pub fn faucet_address() -> Option<String> {
    crate::net_var("FAUCET_ADDRESS")
        .or_else(|| crate::cfg("TESTNET_FAUCET_ADDRESS").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The invite ledger the faucet owns (`faucet.db`, next to `state.db`). The server only
/// ever reads it — see `faucet::code_has_uses_left`.
fn faucet_db_path() -> std::path::PathBuf {
    std::path::PathBuf::from(crate::cfg("DATA").unwrap_or_else(|_| "./data".into())).join("faucet.db")
}

/// Where testers redeem an invite invoice (`FAUCET_URL`, e.g. https://faucet.tokumai.com);
/// the app links to `<that>/claim` with the code and memo in the fragment.
///
/// Reported whenever the invite rail is configured — NOT gated on testnet mode any more.
/// The invite flow runs beside real purchases now, and gating the link on testnet is what
/// made the faucet vanish the moment the server went to mainnet.
pub fn faucet_url() -> Option<String> {
    faucet_address()?;
    crate::cfg("FAUCET_URL")
        .ok()
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| u.starts_with("https://"))
}

/// Where our own website lives — the app builds the `/pay` hand-over link from it, so a
/// purchase started in the app can be paid in the browser (Apple's IAP gate).
///
/// Deliberately NOT the same call as `faucet_url()`: that one is gated on testnet mode,
/// because the faucet note is a tester thing. The payment page is the opposite — it
/// matters most on MAINNET. Reusing the faucet URL for it would have made the hand-over
/// button silently disappear the moment testnet is switched off (caught 2026-09-04).
/// `SITE_URL` wins; `FAUCET_URL` is the fallback so an existing .env keeps working.
pub fn site_url() -> Option<String> {
    crate::cfg("SITE_URL")
        .ok()
        .or_else(|| crate::cfg("FAUCET_URL").ok())
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| u.starts_with("https://"))
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Inv {
    id: String,
    provider_ref: String,
    account_id: String,
    amount_usd: u32,
    /// ON DISK THIS FIELD IS STILL `amount_scrai`, on purpose. It is inside the pay
    /// snapshot in state.db, and the server refuses to start on a snapshot it cannot
    /// parse (a silent reset would drop paid invoices and entitlements) — so renaming the
    /// key took the live server down on 2026-09-05. Keeping the stored name means no
    /// migration and a binary rollback still reads what this build wrote. The alias lets
    /// the new spelling be read too, so the stored name can be retired later with a
    /// snapshot rewrite, once nothing that could be rolled back to is still around.
    #[serde(rename = "amount_scrai", alias = "amount_toku")]
    amount_toku: u64,
    method: String,
    status: String, // "pending" | "paid" | "expired"
    expires_at: u64,
    /// Exact unym quoted for a native-NYM invoice (0 for every other rail).
    /// Lives IN the durable record so a pending payment survives restarts.
    #[serde(default)]
    expected_unym: u64,
    /// Raised as a $1 invite purchase (faucet-paid). Persisted so scrai-admin and the
    /// faucet can tell invite buys from real ones after a restart. The stored name stays
    /// `testnet` — a field inside the snapshot is a format, not a label (2026-09-05).
    #[serde(default)]
    testnet: bool,
    /// The invite code this $1 credit was raised against. The faucet requires the code it
    /// is handed to match, so one tester's code cannot fund somebody else's invoice.
    /// Empty for a real purchase, and for invoices from apps that predate the invite flow.
    #[serde(default)]
    invite_code: String,
}

/// Read-only view of a testnet invoice for the faucet (`scrai-faucet`) and scrai-admin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestnetInv {
    pub id: String,
    /// The Nyx memo the payment must carry (`provider_ref` of a native-NYM invoice).
    pub memo: String,
    /// The invite code the invoice was raised against — the faucet pays only when the
    /// code it was handed matches this. Empty on pre-invite invoices.
    #[serde(default)]
    pub code: String,
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
    /// M-cl-2: withdraw idempotency. Keyed by the hash of the Withdraw body (user key +
    /// blinded request). Entitlement is consumed the FIRST time a body is seen; a client
    /// that never saw the reply (dropped SURB, timeout, crash before the purse was saved)
    /// re-sends the SAME body and gets the SAME credential back without paying again.
    /// `fed` is None between consumption and issuance (a server restart in that window
    /// makes the replay issue without consuming again). Bounded FIFO + age.
    #[serde(default)]
    issued: HashMap<String, Issued>,
    #[serde(default)]
    issued_fifo: VecDeque<String>,
    #[serde(default)]
    rev: u64,
    #[serde(skip)]
    acct_hits: HashMap<String, Vec<u64>>,
    /// card invoices per account in the window (subset of acct_hits, tighter cap)
    #[serde(skip)]
    card_hits: HashMap<String, Vec<u64>>,
    /// invite-code checks per account in the window
    #[serde(skip)]
    code_hits: HashMap<String, Vec<u64>>,
    #[serde(skip)]
    global_hits: Vec<u64>,
    /// last "global invoice cap" log line (ms) — one per minute, not one per refused request
    #[serde(skip)]
    global_cap_logged_at: u64,
}

/// One withdrawal the paywall has charged for (M-cl-2). `fed` is the cached
/// `FedResponse` value once issuance succeeded.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Issued {
    pub account: String,
    #[serde(default)]
    pub fed: Option<Value>,
    pub at_ms: u64,
}
/// Issued records kept for replays: ~350 B each inside the pay snapshot.
pub const MAX_ISSUED: usize = 1024;
/// A replay older than this is treated as a new request (the record is gone).
pub const ISSUED_TTL_MS: u64 = 30 * 86_400_000;

/// The idempotency key of a Withdraw envelope: sha256 of its `fed.Withdraw` body as
/// received. A client retry re-sends the identical body (same user key, same blinded
/// request), so the same key comes back; a fresh withdrawal has a fresh user key.
pub fn withdraw_key(envelope: &Value) -> Option<String> {
    let body = envelope.pointer("/fed/Withdraw")?;
    let bytes = serde_json::to_vec(body).ok()?;
    Some(hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes)))
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

    /// Does this invite code still have a use left? This answer decides whether the app
    /// shows the $1 tile — it never decides whether money moves. That stays with
    /// `begin_create` (which re-checks and binds the code into the invoice) and with the
    /// faucet (which re-checks again and consumes the use as it pays).
    ///
    /// Signed like every other account request, so the limit below can be per account.
    fn invite_check(&mut self, v: &Value, id: &Value) -> Value {
        let Some(account) = self.account_owns(v, "invite") else {
            return err(id, "account signature does not check out, or the nonce was reused");
        };
        if let Err(e) = self.admit_code_check(&account) {
            return err(id, &e);
        }
        let code = v
            .get("code")
            .and_then(|c| c.as_str())
            .map(|c| c.trim().to_ascii_uppercase())
            .unwrap_or_default();
        let valid = crate::faucet::code_has_uses_left(&faucet_db_path(), &code);
        json!({
            "kind": "invite.checked", "id": id,
            "valid": valid,
            "usd": TESTNET_USD,
            "enabled": faucet_address().is_some(),
        })
    }

    fn admit_code_check(&mut self, account_id: &str) -> Result<(), String> {
        let now = now_ms();
        let hits = self.code_hits.entry(account_id.to_string()).or_default();
        hits.retain(|t| now - t < INVOICE_ACCT_WINDOW_MS);
        if hits.len() >= CODE_CHECKS_PER_ACCT {
            let retry = (INVOICE_ACCT_WINDOW_MS - (now - hits[0])).div_ceil(1000).max(1);
            return Err(format!("too many invite-code checks from this account — retry in ~{retry}s"));
        }
        hits.push(now);
        Ok(())
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
        let cap = invoice_global_per_min();
        if self.global_hits.len() >= cap {
            if now - self.global_cap_logged_at > 60_000 {
                self.global_cap_logged_at = now;
                eprintln!("scrai-server: INVOICE LIMIT — {cap} invoices/min server-wide reached (INVOICE_PER_MIN, default {INVOICE_GLOBAL_PER_MIN_DEFAULT}) — refusing creates for up to 60 s");
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

    /// Deduct entitlement for a coconut issuance. Called BEFORE the (off-loop) issuance
    /// runs, so two withdraws of the same account in flight can't both pass the gate;
    /// `restore_entitlement` gives it back when issuance fails.
    pub fn consume_entitlement(&mut self, account_id: &str, amount: u64) {
        let e = self.entitlements.entry(account_id.to_string()).or_default();
        *e = e.saturating_sub(amount);
        self.rev += 1;
    }

    /// Undo a `consume_entitlement` whose issuance did not produce a credential.
    pub fn restore_entitlement(&mut self, account_id: &str, amount: u64) {
        let e = self.entitlements.entry(account_id.to_string()).or_default();
        *e = e.saturating_add(amount);
        self.rev += 1;
    }

    /// The issuance record for a Withdraw body, if this server charged for it (M-cl-2).
    pub fn issuance(&self, key: &str) -> Option<&Issued> {
        self.issued.get(key)
    }

    /// Record that `account` has been charged for the Withdraw body `key`. Called in the
    /// same loop turn as `consume_entitlement`, so both land in one snapshot. Keeps an
    /// existing record (a replay after a mid-issuance restart must not reset it).
    pub fn begin_issuance(&mut self, key: &str, account: &str) {
        if self.issued.contains_key(key) {
            return;
        }
        let now = now_ms();
        self.issued.insert(key.to_string(), Issued { account: account.to_string(), fed: None, at_ms: now });
        self.issued_fifo.push_back(key.to_string());
        // Bound by count and by age (the FIFO is insertion-ordered, so the front is oldest).
        while self.issued_fifo.len() > MAX_ISSUED
            || self.issued_fifo.front().and_then(|k| self.issued.get(k)).is_some_and(|r| now.saturating_sub(r.at_ms) > ISSUED_TTL_MS)
        {
            match self.issued_fifo.pop_front() {
                Some(k) => { self.issued.remove(&k); }
                None => break,
            }
        }
        self.rev += 1;
    }

    /// The credential for `key` was issued: cache the reply so a replay gets it verbatim.
    pub fn finish_issuance(&mut self, key: &str, fed: Value) {
        if let Some(r) = self.issued.get_mut(key) {
            r.fed = Some(fed);
            self.rev += 1;
        }
    }

    /// Issuance failed after the charge: the entitlement is restored by the caller and
    /// the record is dropped, so the client's retry is treated as a new request.
    pub fn abort_issuance(&mut self, key: &str) {
        if self.issued.remove(key).is_some() {
            self.issued_fifo.retain(|k| k != key);
            self.rev += 1;
        }
    }

    /// Settle one invoice (idempotent): anything-but-paid → paid credits the
    /// entitlement exactly once. Deliberately also settles a locally "expired"
    /// invoice — BTCPay keeps watching past our window, and if IT says Settled,
    /// money moved and must be credited regardless of our timer.
    fn settle(&mut self, invoice_id: &str) {
        if let Some(inv) = self.invoices.get_mut(invoice_id) {
            if inv.status != "paid" {
                inv.status = "paid".into();
                let scrai = inv.amount_toku;
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
            "invite.check" => self.invite_check(&v, &id),
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
            PayOutcome::Create { id, account, usd, our_id, testnet, code, result } => {
                self.finish_create(&id, account, usd, our_id, testnet, code, result)
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
        // The $1 tile. Two ways in: an invite code (the only way on a mainnet server), or
        // the bare `testnet` flag of an app that predates the invite field, which a
        // testnet server still honours so testers are not stranded on the old build.
        let code = v
            .get("inviteCode")
            .and_then(|c| c.as_str())
            .map(|c| c.trim().to_ascii_uppercase())
            .filter(|c| !c.is_empty());
        let testnet = code.is_some() || v.get("testnet").and_then(|t| t.as_bool()).unwrap_or(false);
        // The client picks the rail ("nyx" = native NYM on the Nyx chain); it is
        // deliberately NOT part of the account signature — it only selects HOW to
        // pay, never how much is credited.
        let wanted = v.get("method").and_then(|m| m.as_str()).unwrap_or("btc").to_string();
        if testnet {
            // A tester's $1, paid by the faucet on this host.
            if usd != TESTNET_USD {
                return Err(err(id, &format!("an invite credit is ${TESTNET_USD} only")));
            }
            // Only the faucet's NYM may settle it (see `faucet_address`), so the invoice
            // must be native NYM and the pin must be configured.
            if wanted != "nyx" {
                return Err(err(id, "invite credits are paid in NYM by the faucet — pick NYM"));
            }
            if faucet_address().is_none() {
                return Err(err(id, "invite credits are not enabled on this server (no faucet wallet pinned)"));
            }
            // THE gate. The client saying "invite" grants nothing: the code is checked
            // here against the faucet's own ledger, and bound into the invoice below.
            match code.as_deref() {
                Some(c) if crate::faucet::code_has_uses_left(&faucet_db_path(), c) => {}
                Some(_) => return Err(err(id, "that invite code is not valid, or has been used up")),
                // Pre-invite app on a testnet server: there the whole server is the gate.
                None if is_testnet_server() => {}
                None => return Err(err(id, &format!("a ${TESTNET_USD} credit needs an invite code"))),
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
        Ok(PayPending::Create { id: id.clone(), account, usd, our_id, wanted, testnet, code: code.unwrap_or_default() })
    }

    fn finish_create(
        &mut self,
        id: &Value,
        account: String,
        usd: u32,
        our_id: String,
        testnet: bool,
        code: String,
        result: Result<Raised, String>,
    ) -> Value {
        let raised = match result {
            Ok(r) => r,
            Err(e) => return err(id, &e),
        };
        // The TOKU amount is fixed HERE, not at settlement, so the user gets
        // exactly what they were quoted regardless of the exchange rate.
        let amount_toku = usd as u64 * TOKU_PER_USD;
        self.invoices.insert(
            our_id.clone(),
            Inv {
                id: our_id.clone(),
                provider_ref: raised.raised.provider_ref.clone(),
                account_id: account,
                amount_usd: usd,
                amount_toku,
                method: raised.method.clone(),
                status: "pending".into(),
                expires_at: raised.raised.expires_at,
                expected_unym: raised.expected_unym,
                testnet,
                invite_code: code,
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
            "amountToku": amount_toku,
            "amountScrai": amount_toku,   // pre-rename apps
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
                code: i.invite_code.clone(),
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
    pub fn gate_withdraw(&mut self, request: &[u8], book_toku: u64) -> Gate {
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
        let Some(req_key) = withdraw_key(&v) else {
            return Gate::Denied(encode(&err(&id, "malformed withdrawal request")));
        };
        // M-cl-2: a body this server already charged for is a retry, not a new purchase —
        // it passes without entitlement (and only for the account that paid).
        if let Some(rec) = self.issued.get(&req_key) {
            if rec.account != account {
                return Gate::Denied(encode(&err(&id, "this withdrawal request belongs to another account")));
            }
            return Gate::Authorized { account_id: account, req_key, prepaid: true };
        }
        let held = self.entitlement(&account);
        if held < book_toku {
            return Gate::Denied(encode(&err(
                &id,
                &format!("not enough entitlement: a ticketbook costs {book_toku} TOKU, this account holds {held} — buy credit first"),
            )));
        }
        Gate::Authorized { account_id: account, req_key, prepaid: false }
    }
}

pub enum Gate {
    NotAWithdraw,
    Denied(Vec<u8>),
    /// `req_key` = idempotency key of the body; `prepaid` = this body was charged
    /// before (a retry) — the caller must not consume entitlement again.
    Authorized { account_id: String, req_key: String, prepaid: bool },
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
    Create { id: Value, account: String, usd: u32, our_id: String, wanted: String, testnet: bool, code: String },
    Status { id: Value, inv: Inv },
    Sweep { id: Value, account: String, candidates: Vec<Inv> },
}

/// What the gateway said, to be applied on the loop by `Pay::finish`.
pub enum PayOutcome {
    Create { id: Value, account: String, usd: u32, our_id: String, testnet: bool, code: String, result: Result<Raised, String> },
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
        PayPending::Create { id, account, usd, our_id, wanted, testnet, code } => {
            let result = gateway.create_invoice(usd, &our_id, &wanted, testnet).await;
            PayOutcome::Create { id, account, usd, our_id, testnet, code, result }
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
        PayPending::Create { id, account, usd, our_id, testnet, code, .. } => PayOutcome::Create {
            id,
            account,
            usd,
            our_id,
            testnet,
            code,
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
                Some(faucet_address().ok_or("invite invoice but no faucet wallet is pinned (FAUCET_ADDRESS) — refusing to settle")?)
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

/// BTCPay or CoinGate (real) or the fake (dev). Selection fails loudly when
/// nothing is configured — an issuer that hands out TOKU for imaginary money must
/// never be a silent default.
pub enum Rail {
    Fake,
    BtcPay { base_url: String, store_id: String, api_key: String },
    /// The processor alternative to running our own node: an order is raised in USD
    /// and locked to ONE coin. See the CoinGate section below for why one.
    CoinGate {
        base_url: String,
        api_key: String,
        pay_currency: String,
        platform_id: u64,
        /// What CoinGate pays out in — `None` leaves it to the account's own setting.
        receive_currency: Option<String>,
    },
    None,
}

impl Rail {
    pub fn from_env() -> Rail {
        if crate::cfg("FAKE_PAYMENTS").as_deref() == Ok("1") {
            // The fake rail must never coexist with a real one: with NYX_* or BTCPAY_* also
            // set, `is_fake()` would read false (real-money interlock passes) while every
            // non-nyx invoice still settled for free. Refuse to boot in that mixed state.
            let real = [
                "NYX_RECEIVE_ADDRESS", "NYX_LCD_URL", "BTCPAY_URL", "BTCPAY_STORE_ID", "BTCPAY_API_KEY",
                "COINGATE_API_KEY", "COINGATE_API_KEY_TESTNET", "COINGATE_API_KEY_MAINNET",
                "MOLLIE_API_KEY", "MOLLIE_API_KEY_TESTNET", "MOLLIE_API_KEY_MAINNET",
            ]
            .iter()
            .any(|k| std::env::var(k).map(|v| !v.trim().is_empty()).unwrap_or(false));
            if real {
                eprintln!("scrai-server: FATAL: FAKE_PAYMENTS=1 together with a real payment rail (NYX_*/BTCPAY_*/MOLLIE_*) — remove one. Refusing to start.");
                std::process::exit(1);
            }
            eprintln!("scrai-server: FAKE_PAYMENTS=1 — invoices settle on first poll. DEV ONLY.");
            return Rail::Fake;
        }
        // CoinGate first: both coin rails can be configured at once (a leftover BTCPay
        // store while the processor is brought up), and picking one silently would leave
        // the operator watching the wrong dashboard for money that never arrives.
        if let Some((base_url, api_key)) = coingate_from_env() {
            if crate::net_var("BTCPAY_URL").is_some() {
                eprintln!("scrai-server: a CoinGate app and a BTCPay store are both configured — raising invoices on CoinGate, ignoring BTCPay.");
            }
            let pay_currency = crate::cfg("COINGATE_PAY_CURRENCY")
                .ok()
                .map(|c| c.trim().to_uppercase())
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "BTC".to_string());
            let platform_id = crate::cfg("COINGATE_PLATFORM_ID")
                .ok()
                .and_then(|p| p.trim().parse::<u64>().ok())
                .unwrap_or(COINGATE_PLATFORM_BITCOIN);
            let receive_currency = crate::cfg("COINGATE_RECEIVE_CURRENCY")
                .ok()
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty());
            if platform_id == COINGATE_PLATFORM_LIGHTNING_BTC {
                // The pay screen filters options whose method reads as Lightning, so a
                // Lightning-only rail would raise a perfectly good invoice that the app
                // then refuses to show. Loud, because the symptom is an empty pay panel.
                eprintln!("scrai-server: CoinGate is locked to the Lightning platform (43), which today's app hides on the pay screen — the address will not be displayed. Use 5 (on-chain) until the client shows Lightning.");
            }
            return Rail::CoinGate { base_url, api_key, pay_currency, platform_id, receive_currency };
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
            Rail::CoinGate { .. } => "coingate",
            Rail::None => "none",
        }
    }

    async fn create_invoice(&self, usd: u32, reference: &str) -> Result<RaisedInvoice, String> {
        match self {
            Rail::None => Err("this server cannot sell TOKU — no payment gateway configured".into()),
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
            Rail::CoinGate { base_url, api_key, pay_currency, platform_id, receive_currency } => {
                // 1. the order, priced in fiat. `order_id` is our own reference, so a
                //    support question can be traced back without CoinGate learning
                //    anything about the account.
                let mut body = json!({
                    "price_amount": format!("{usd}.00"),
                    "price_currency": "USD",
                    "order_id": reference,
                    "title": "tokumai credit",
                    "description": format!("${usd} of prepaid tokumai credit"),
                });
                if let Some(rc) = receive_currency {
                    body["receive_currency"] = json!(rc);
                }
                let order: Value = coingate(
                    api_key,
                    crate::http::client().post(format!("{base_url}/orders")).json(&body),
                )
                .await?;
                // CoinGate's own id is a NUMBER; the string `order_id` is the one we sent.
                let provider_ref = json_scalar(&order, "id");
                if provider_ref.is_empty() {
                    return Err("CoinGate accepted the order but returned no id".into());
                }

                // 2. lock it to one coin. This white-label call is what returns the bare
                //    address instead of a checkout page — the hosted picker would have the
                //    customer's browser talk to CoinGate at the exact moment the mixnet is
                //    supposed to be protecting them. It also starts the 20-minute window.
                let checkout: Value = coingate(
                    api_key,
                    crate::http::client()
                        .post(format!("{base_url}/orders/{provider_ref}/checkout"))
                        .json(&json!({ "pay_currency": pay_currency, "platform_id": platform_id })),
                )
                .await?;
                let destination = json_scalar(&checkout, "payment_address");
                if destination.is_empty() {
                    return Err("CoinGate returned no payment address for this order".into());
                }
                let amount = json_scalar(&checkout, "pay_amount");
                let currency = match json_scalar(&checkout, "pay_currency") {
                    c if c.is_empty() => pay_currency.clone(),
                    c => c.to_uppercase(),
                };
                // Lightning is decided by the platform WE asked for, never by the reply's
                // own flag: the pay screen hides every option whose method matches
                // /ln|lightning/, so a wrong label here makes the address vanish from the UI.
                let lightning = *platform_id == COINGATE_PLATFORM_LIGHTNING_BTC;
                let method = if lightning { format!("{currency}-LN") } else { currency.clone() };
                let uri = match (lightning, currency.as_str()) {
                    (true, _) => format!("lightning:{destination}"),
                    (false, "BTC") if !amount.is_empty() => format!("bitcoin:{destination}?amount={amount}"),
                    (false, "BTC") => format!("bitcoin:{destination}"),
                    _ => destination.clone(),
                };
                // CoinGate expires a checked-out order after 20 minutes and stops watching —
                // unlike BTCPay, whose late settlement `begin_status`'s sweep was built for.
                // A confirmation that lands after the window is a CoinGate support case, not
                // something this server can credit on its own.
                let expires_at = checkout
                    .get("expire_at")
                    .and_then(|e| e.as_str())
                    .and_then(rfc3339_ms)
                    .unwrap_or_else(|| now_ms() + 20 * 60_000);
                Ok(RaisedInvoice {
                    provider_ref,
                    pay_to: destination.clone(),
                    instruction: if amount.is_empty() {
                        "Pay to the destination below.".into()
                    } else {
                        format!("Send exactly {amount} {currency} to the destination below — the rate is held until the timer runs out.")
                    },
                    options: json!([{
                        "method": method,
                        "destination": destination,
                        "uri": uri,
                        "amount": amount,
                        "currency": currency,
                    }]),
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
            Rail::CoinGate { base_url, api_key, .. } => {
                let order: Value = coingate(
                    api_key,
                    crate::http::client().get(format!("{base_url}/orders/{provider_ref}")),
                )
                .await?;
                Ok(coingate_status(order.get("status").and_then(|s| s.as_str()).unwrap_or("")).into())
            }
        }
    }
}

/// `Retry-After` in seconds, if the provider sent one (an HTTP-date form is ignored).
fn retry_after_secs(res: &reqwest::Response) -> Option<u64> {
    res.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
}

/// What the USER reads when a payment provider throttles or is down: our wording,
/// with the provider's own retry hint when it gave one. (The raw rail error string is
/// what invoice.create returns to the app, so this is the whole message.)
fn provider_busy(provider: &str, retry_after: Option<u64>) -> String {
    match retry_after {
        Some(n) if n >= 120 => format!("{provider} is limiting requests right now — please try again in about {} minutes", n.div_ceil(60)),
        Some(n) => format!("{provider} is limiting requests right now — please try again in about {n} seconds"),
        None => format!("{provider} is busy right now — please try again in a minute"),
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
    let retry_after = retry_after_secs(&res);
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = body.get("message").and_then(|m| m.as_str()).unwrap_or("");
        return Err(match status.as_u16() {
            401 | 403 => "BTCPay rejected the API key — check BTCPAY_API_KEY and its store permissions".into(),
            404 => "BTCPay does not know this store or invoice — check BTCPAY_STORE_ID".into(),
            // Throttled (429) or temporarily down (502/503/504): the user gets a plain
            // "try again in N" instead of BTCPay's own text; the operator log keeps that.
            429 | 502 | 503 | 504 => {
                eprintln!("scrai-server: BTCPay {status} (retry-after {retry_after:?}): {}", msg.chars().take(200).collect::<String>());
                provider_busy("the payment processor", retry_after)
            }
            s => format!("BTCPay {s}: {}", msg.chars().take(200).collect::<String>()),
        });
    }
    Ok(body)
}


// ---------------------------------------------------------------------------
// CoinGate — an EU-licensed processor (UAB Decentralized, Vilnius) instead of our
// own node: it takes the coin and settles fiat, so no Bitcoin node, no xpub, no
// custody here. The trade for that is one coin per invoice — CoinGate has no
// multi-method invoice, and its hosted picker is the browser round trip the mixnet
// exists to avoid — so we take the white-label `/checkout` call, which hands back
// the bare address, and the app renders the QR itself exactly as on every other rail.
// ---------------------------------------------------------------------------

const COINGATE_API: &str = "https://api.coingate.com/api/v2";
/// The sandbox is a separate host with SEPARATE credentials — a coingate.com key does
/// not work here and vice versa — which is what makes the key an honest signal of
/// which world the money is in.
const COINGATE_API_SANDBOX: &str = "https://api-sandbox.coingate.com/api/v2";
/// `platform_id` for BTC, from the public `GET /api/v2/currencies`: BTC carries
/// `bitcoin` = 5 and `lightning_btc` = 43. On-chain is the default because the pay
/// screen filters Lightning options out of the UI today.
const COINGATE_PLATFORM_BITCOIN: u64 = 5;
const COINGATE_PLATFORM_LIGHTNING_BTC: u64 = 43;

/// Which CoinGate world this server talks to, decided by WHICH key is set rather than
/// by `TESTNET`. Deriving the host from the mode would let a live key be pointed at the
/// sandbox — or worse, let a sandbox key, whose orders can be marked paid for nothing,
/// mint real credit on a real server. Pure, with the lookup injected: the env-var tests
/// in this crate share one process (that trap has bitten twice).
fn coingate_endpoint(get: impl Fn(&str) -> Option<String>) -> Option<(String, String)> {
    let val = |n: &str| get(n).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    if let Some(k) = val("COINGATE_API_KEY_MAINNET") {
        return Some((COINGATE_API.to_string(), k));
    }
    if let Some(k) = val("COINGATE_API_KEY_TESTNET") {
        return Some((COINGATE_API_SANDBOX.to_string(), k));
    }
    let key = val("COINGATE_API_KEY")?;
    let base = match val("COINGATE_SANDBOX").as_deref() {
        Some("1") => COINGATE_API_SANDBOX,
        _ => COINGATE_API,
    };
    Some((base.to_string(), key))
}

fn coingate_from_env() -> Option<(String, String)> {
    coingate_endpoint(|n| std::env::var(n).ok())
}

/// CoinGate's order status → ours. `confirming` is deliberately NOT paid, for the same
/// reason BTCPay's `Processing` is not: the money is visible, not final. Everything
/// terminal that is not "the merchant has it" is an expiry as far as credit goes.
fn coingate_status(status: &str) -> &'static str {
    match status {
        "paid" => "paid",
        "expired" | "canceled" | "invalid" | "refunded" | "partially_refunded" => "expired",
        // new | pending | confirming | anything CoinGate adds later
        _ => "pending",
    }
}

/// One JSON field as text. CoinGate returns its order id as a number and its amounts as
/// strings, and `as_str()` on the former quietly yields "" — which would post a checkout
/// to `/orders//checkout`.
fn json_scalar(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

async fn coingate(api_key: &str, req: reqwest::RequestBuilder) -> Result<Value, String> {
    let res = req
        // CoinGate's own scheme, not Bearer.
        .header("authorization", format!("Token {api_key}"))
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("CoinGate unreachable: {e}"))?;
    let status = res.status();
    let retry_after = retry_after_secs(&res);
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = body
            .get("message")
            .and_then(|m| m.as_str())
            .or_else(|| body.get("reason").and_then(|r| r.as_str()))
            .unwrap_or("");
        return Err(match status.as_u16() {
            401 | 403 => "CoinGate rejected the API key — check COINGATE_API_KEY, and that a sandbox key is not pointed at the live API (or the other way round)".into(),
            404 => "CoinGate does not know this order".into(),
            422 => format!("CoinGate refused the order: {}", msg.chars().take(200).collect::<String>()),
            // Throttled or temporarily down: the user gets a plain "try again in N";
            // CoinGate's own text stays in the operator log.
            429 | 502 | 503 | 504 => {
                eprintln!("scrai-server: CoinGate {status} (retry-after {retry_after:?}): {}", msg.chars().take(200).collect::<String>());
                provider_busy("the payment processor", retry_after)
            }
            s => format!("CoinGate {s}: {}", msg.chars().take(200).collect::<String>()),
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
/// the static thank-you page on our own site (no order id, no cookie).
const DEFAULT_PAID_URL: &str = "https://tokumai.com/paid";

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
                        crate::cfg("FAUCET_URL")
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
        // (the TOKU price is fixed per USD; Mollie converts to the payout currency).
        let currency = if api_key.starts_with("test_") { "EUR" } else { "USD" };
        let body = json!({
            "amount": { "currency": currency, "value": format!("{usd}.00") },
            "description": "tokumai credit",
            "redirectUrl": redirect_url,
            // No `method`: the hosted checkout offers every method enabled in the Mollie
            // dashboard (cards, PayPal, later Wero) — switching one on there needs no deploy.
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
            Err(MollieErr::RateLimited(_)) => return Ok("pending".into()),
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

#[derive(Debug)]
enum MollieErr {
    /// 429 with the provider's `Retry-After` (seconds), when it sent one.
    RateLimited(Option<u64>),
    Other(String),
}

impl From<MollieErr> for String {
    fn from(e: MollieErr) -> String {
        match e {
            MollieErr::RateLimited(after) => provider_busy("the card processor", after),
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
    let retry_after = retry_after_secs(&res);
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        // Mollie errors are {status, title, detail}; `detail` names the offending field.
        let detail = body.get("detail").and_then(|m| m.as_str()).unwrap_or("");
        return Err(match status.as_u16() {
            401 | 403 => MollieErr::Other("Mollie rejected the API key — check MOLLIE_API_KEY".into()),
            404 => MollieErr::Other("Mollie does not know this payment".into()),
            429 => MollieErr::RateLimited(retry_after),
            502 | 503 | 504 => MollieErr::Other(provider_busy("the card processor", retry_after)),
            s => MollieErr::Other(format!("Mollie {s}: {}", detail.chars().take(200).collect::<String>())),
        });
    }
    Ok(body)
}

/// `2026-08-29T10:47:54+00:00` → unix ms. Mollie's and CoinGate's timestamps are both
/// RFC 3339 with a numeric offset (or `Z`); anything else parses as None and the caller
/// falls back.
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

    /// TESTNET is process-global and decides whether a plain $5 create is accepted,
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

    // The invite rail: a $1 credit the faucet pays, and the ONLY thing a testnet server
    // sells. What opens it is a VALID INVITE CODE, checked here against the faucet's own
    // ledger — a client that merely claims "invite" gets nothing. A testnet server still
    // honours a code-less `testnet:true` so apps that predate the invite field keep
    // working. The code is bound into the invoice, so the faucet can refuse to fund one
    // tester's invoice with another tester's code.
    #[tokio::test]
    async fn invite_create_is_one_dollar_and_needs_a_valid_code() {
        let _env = ENV_LOCK.write().unwrap_or_else(|e| e.into_inner());
        let (sk, pem, aid) = account();
        let gw = Gateway { rail: Rail::Fake, nyx: None, card: CardRail::None };

        // a real invite ledger with one real code in it
        let dir = std::env::temp_dir().join(format!("scrai-invite-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let conn = crate::faucet::open_db(&dir.join("faucet.db")).unwrap();
        let code = crate::faucet::mint(&conn, 1, "tester").unwrap();
        std::env::set_var("DATA", dir.to_string_lossy().to_string());

        // nonces burn on first sight (even for a refused create), so every call gets its own
        let req = |usd: u32, testnet: bool, method: &str, n: &str, invite: Option<&str>| {
            let mut v = json!({"kind":"invoice.create","id":"r1","publicKey":pem,
                "usd":usd,"testnet":testnet,"method":method,"nonce":n,
                "sig":signed(&sk,&aid,&format!("invoice:{usd}"),n)});
            if let Some(c) = invite {
                v["inviteCode"] = json!(c);
            }
            v
        };
        let refused = |pay: &mut Pay, v: Value, needle: &str| {
            let PayStep::Reply(r) = pay.begin(v.to_string().as_bytes(), &gw) else { panic!("must be refused ({needle})") };
            let r: Value = serde_json::from_slice(&r).unwrap();
            assert!(r["error"].as_str().unwrap_or("").contains(needle), "{r}");
        };

        std::env::remove_var("TESTNET");
        std::env::remove_var("TESTNET_FAUCET_ADDRESS");
        std::env::remove_var("FAUCET_ADDRESS");
        let mut pay = Pay::default();
        // fail closed: no faucet wallet pinned → no invite credit at all
        refused(&mut pay, req(1, true, "nyx", "n1", Some(&code)), "faucet wallet");
        // $1 is not a normal tier either
        refused(&mut pay, req(1, false, "nyx", "n2", None), "one of");

        std::env::set_var("FAUCET_ADDRESS", "n1faucet");
        // a mainnet server sells $1 ONLY against a code — the bare flag buys nothing
        refused(&mut pay, req(1, true, "nyx", "n3", None), "invite code");
        refused(&mut pay, req(1, true, "nyx", "n4", Some("TOKU-ZZZZ-ZZZZ")), "not valid");
        // right code, wrong shape of purchase
        refused(&mut pay, req(5, true, "nyx", "n5", Some(&code)), "$1");
        refused(&mut pay, req(1, true, "btc", "n6", Some(&code)), "NYM");
        // the valid code passes the gate: this test gateway has no NYM rail, and an invite
        // invoice must never fall back to the processor rail — so it fails THERE, not here
        let PayStep::Pending(p) = pay.begin(req(1, true, "nyx", "n7", Some(&code)).to_string().as_bytes(), &gw)
        else {
            panic!("a valid code must reach the gateway")
        };
        let r: Value = serde_json::from_slice(&pay.finish(run_gateway(p, &gw).await, &gw)).unwrap();
        assert!(r["error"].as_str().unwrap_or("").contains("NYM rail"), "{r}");
        assert!(pay.testnet_invoices().is_empty());

        // a testnet server: no real purchases at all, and a pre-invite app still works
        std::env::set_var("TESTNET", "1");
        refused(&mut pay, req(5, false, "nyx", "n8", None), "testnet server");
        let PayStep::Pending(_) = pay.begin(req(1, true, "nyx", "n9", None).to_string().as_bytes(), &gw) else {
            panic!("a testnet server still takes a code-less $1 from an old app")
        };

        std::env::remove_var("TESTNET");
        std::env::remove_var("FAUCET_ADDRESS");
        std::env::remove_var("DATA");
        let _ = std::fs::remove_dir_all(&dir);

        // the flag AND the code ride in the durable record; the faucet view picks exactly those
        let aid2 = aid.clone();
        pay.invoices.insert("t1".into(), Inv { id: "t1".into(), provider_ref: "TOKU-MEMO2345".into(), account_id: aid2,
            amount_usd: 1, amount_toku: TOKU_PER_USD, method: "nyx".into(), status: "pending".into(),
            expires_at: now_ms() + 60_000, expected_unym: 59_000_000, testnet: true, invite_code: "TOKU-AAAA-BBBB".into() });
        pay.invoices.insert("r1".into(), Inv { id: "r1".into(), provider_ref: "TOKU-REAL2345".into(), account_id: aid,
            amount_usd: 5, amount_toku: 5 * TOKU_PER_USD, method: "nyx".into(), status: "pending".into(),
            expires_at: now_ms() + 60_000, expected_unym: 295_000_000, testnet: false, invite_code: String::new() });
        let t = pay.testnet_invoices();
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].amount_usd, t[0].memo.as_str(), t[0].unym), (1, "TOKU-MEMO2345", 59_000_000));
        assert_eq!(t[0].code, "TOKU-AAAA-BBBB");
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
    /// tiles below CARD_MIN_USD are refused BEFORE any gateway call. Env-mutating →
    /// write lock.
    #[tokio::test]
    async fn card_purchases_need_a_key_and_the_minimum_tile() {
        let _env = ENV_LOCK.write().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("TESTNET");
        std::env::remove_var("CARD_MIN_USD");
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
        let req_key = match pay.gate_withdraw(w.to_string().as_bytes(), book) {
            Gate::Authorized { account_id, req_key, prepaid } => {
                assert_eq!(account_id, aid);
                assert!(!prepaid);
                pay.consume_entitlement(&account_id, book);
                pay.begin_issuance(&req_key, &account_id);
                assert_eq!(pay.entitlement(&aid), 0);
                req_key
            }
            _ => panic!("expected Authorized"),
        };

        // M-cl-2: the SAME body (fresh nonce) is a retry — authorized with zero
        // entitlement, marked prepaid, and once issued the cached reply is there.
        let w2 = json!({"kind":"coconut","id":"y","fed":{"Withdraw":{}},"publicKey":pem,
            "nonce":"w3","sig":signed(&sk,&aid,"withdraw:coconut","w3")});
        match pay.gate_withdraw(w2.to_string().as_bytes(), book) {
            Gate::Authorized { req_key: k2, prepaid, .. } => {
                assert_eq!(k2, req_key);
                assert!(prepaid);
            }
            _ => panic!("retry must pass the gate without entitlement"),
        }
        assert!(pay.issuance(&req_key).unwrap().fed.is_none());
        pay.finish_issuance(&req_key, json!({"Withdraw":{"blinded":"…"}}));
        assert_eq!(pay.issuance(&req_key).unwrap().fed.as_ref().unwrap()["Withdraw"]["blinded"], "…");

        // a DIFFERENT body from the same broke account is a new purchase → denied
        let w3 = json!({"kind":"coconut","id":"z","fed":{"Withdraw":{"user_pk":"other"}},"publicKey":pem,
            "nonce":"w4","sig":signed(&sk,&aid,"withdraw:coconut","w4")});
        assert!(matches!(pay.gate_withdraw(w3.to_string().as_bytes(), book), Gate::Denied(_)));

        // the record survives a snapshot round-trip and abort drops it
        let mut back: Pay = serde_json::from_str(&pay.snapshot()).unwrap();
        assert!(back.issuance(&req_key).is_some());
        back.abort_issuance(&req_key);
        assert!(back.issuance(&req_key).is_none());
    }

    /// The pay snapshot is the one place where a field NAME is a wire format: main.rs
    /// refuses to start on a snapshot it cannot parse, so renaming the currency unit in
    /// code must not rename the stored key. It did once — the live server crash-looped on
    /// `missing field amount_toku` (2026-09-05). Both spellings must read, and what this
    /// build writes must stay readable by the build it could be rolled back to.
    #[test]
    fn a_pre_rebrand_pay_snapshot_still_loads() {
        let old = r#"{"invoices":{"i1":{"id":"i1","provider_ref":"SCRAIABCD2345","account_id":"a1",
            "amount_usd":5,"amount_scrai":500000,"method":"nyx","status":"pending","expires_at":0}},
            "entitlements":{"a1":100},"nonces":[]}"#;
        let p: Pay = serde_json::from_str(old).expect("a snapshot from before the rename must load");
        assert_eq!(p.invoices["i1"].amount_toku, 500_000);
        assert_eq!(p.entitlements["a1"], 100);
        // what we write keeps the stored name, so the previous binary still reads it
        assert!(p.snapshot().contains(r#""amount_scrai":500000"#), "{}", p.snapshot());
        // the new spelling is accepted as well
        let new = old.replace("amount_scrai", "amount_toku");
        let q: Pay = serde_json::from_str(&new).unwrap();
        assert_eq!(q.invoices["i1"].amount_toku, 500_000);
    }

    #[test]
    fn issued_records_are_bounded_and_keyed_by_body() {
        let a = json!({"kind":"coconut","fed":{"Withdraw":{"user_pk":"u1","req":{"x":1}}}});
        let b = json!({"kind":"coconut","id":"other-id","nonce":"n","fed":{"Withdraw":{"user_pk":"u1","req":{"x":1}}}});
        let c = json!({"kind":"coconut","fed":{"Withdraw":{"user_pk":"u2","req":{"x":1}}}});
        assert_eq!(withdraw_key(&a), withdraw_key(&b)); // envelope id/nonce don't matter
        assert_ne!(withdraw_key(&a), withdraw_key(&c)); // the body does
        assert!(withdraw_key(&json!({"fed":"Keys"})).is_none());

        let mut pay = Pay::default();
        for i in 0..(MAX_ISSUED + 5) {
            pay.begin_issuance(&format!("k{i}"), "acct");
        }
        assert_eq!(pay.issued.len(), MAX_ISSUED);
        assert!(pay.issuance("k0").is_none()); // oldest evicted
        assert!(pay.issuance(&format!("k{}", MAX_ISSUED + 4)).is_some());
    }
}

#[cfg(test)]
mod coingate_tests {
    use super::*;

    /// The rail must never be decided by TESTNET: sandbox credentials only work against
    /// the sandbox host, so the KEY is the honest signal. Pure lookup — the real env is
    /// process-global and shared with every other test in this crate.
    #[test]
    fn the_coingate_key_decides_which_world_the_money_is_in() {
        let env = |pairs: &[(&str, &str)]| {
            let m: std::collections::HashMap<String, String> =
                pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            move |k: &str| m.get(k).cloned()
        };
        let live = |k: &str| Some((COINGATE_API.to_string(), k.to_string()));
        let sandbox = |k: &str| Some((COINGATE_API_SANDBOX.to_string(), k.to_string()));

        assert_eq!(coingate_endpoint(env(&[])), None, "no key → no CoinGate rail");
        assert_eq!(coingate_endpoint(env(&[("COINGATE_API_KEY_MAINNET", "live-key")])), live("live-key"));
        assert_eq!(coingate_endpoint(env(&[("COINGATE_API_KEY_TESTNET", "sbx-key")])), sandbox("sbx-key"));

        // A half-migrated .env: the mainnet key wins, and it takes the live host WITH it.
        assert_eq!(
            coingate_endpoint(env(&[
                ("COINGATE_API_KEY_TESTNET", "sbx-key"),
                ("COINGATE_API_KEY_MAINNET", "live-key"),
            ])),
            live("live-key")
        );

        // An empty value is not a value.
        assert_eq!(
            coingate_endpoint(env(&[
                ("COINGATE_API_KEY_MAINNET", "   "),
                ("COINGATE_API_KEY_TESTNET", "sbx-key"),
            ])),
            sandbox("sbx-key")
        );

        // The bare name stays live unless the sandbox is asked for explicitly.
        assert_eq!(coingate_endpoint(env(&[("COINGATE_API_KEY", "k")])), live("k"));
        assert_eq!(
            coingate_endpoint(env(&[("COINGATE_API_KEY", "k"), ("COINGATE_SANDBOX", "1")])),
            sandbox("k")
        );
    }

    /// Only "paid" credits an account. `confirming` is money in flight, and every
    /// terminal not-paid state must expire rather than hang the invoice on "pending".
    #[test]
    fn only_a_paid_coingate_order_credits() {
        assert_eq!(coingate_status("paid"), "paid");
        for s in ["new", "pending", "confirming", "", "something_new"] {
            assert_eq!(coingate_status(s), "pending", "{s}");
        }
        for s in ["expired", "canceled", "invalid", "refunded", "partially_refunded"] {
            assert_eq!(coingate_status(s), "expired", "{s}");
        }
    }

    /// CoinGate sends the order id as a NUMBER and the amounts as strings; `as_str()`
    /// on the id yields "" and would post the checkout to `/orders//checkout`.
    #[test]
    fn a_numeric_coingate_id_survives_as_text() {
        let order = json!({ "id": 538, "order_id": "abc", "pay_amount": "0.00042" });
        assert_eq!(json_scalar(&order, "id"), "538");
        assert_eq!(json_scalar(&order, "order_id"), "abc");
        assert_eq!(json_scalar(&order, "pay_amount"), "0.00042");
        assert_eq!(json_scalar(&order, "missing"), "");
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
        std::env::remove_var("CARD_MIN_USD");
        assert_eq!(card_min_usd(), 10);
    }
}
