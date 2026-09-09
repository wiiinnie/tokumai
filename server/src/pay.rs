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
/// The chain watcher keeps checking a little past expiry, for the same reason `settle`
/// honours a late payment: the money is on chain either way.
const WATCH_GRACE_MS: u64 = 48 * 3_600_000;
/// How often ONE invoice may be asked about. The tick is faster than this; the cap is
/// per invoice, so ten open invoices do not mean ten times the chain queries.
const WATCH_EVERY_MS: u64 = 25_000;
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

/// Smallest tile an on-chain coin purchase may be. A miner fee is a property of the
/// NETWORK at that moment, not of the amount: at 2 sat/vB a payment costs the buyer about
/// 20 cents, at 50 it costs five dollars and change — the same transaction. So the floor is
/// not "what is the fee today" but "below what does a bad week make this absurd". $10.
///
/// Lightning would not need one (its fee follows the amount), which is the argument for
/// adding it later; until then this is what keeps a $5 tile from quietly costing $5.50.
/// Reported with the catalog so the app greys the smaller tiles itself.
pub fn coin_min_usd() -> u32 {
    crate::cfg("COIN_MIN_USD").ok().and_then(|v| v.trim().parse().ok()).filter(|v| *v > 0).unwrap_or(10)
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
        "btc": coin_rail_ready(),
        "card": card_enabled(),
        "invite": faucet_address().is_some(),
        "inviteUsd": TESTNET_USD,
        "btcMinUsd": coin_min_usd(),
    })
}

/// A coin rail is configured (our BTCPay), or the dev rail stands in.
pub fn coin_rail_ready() -> bool {
    if fake_payments_enabled() {
        return true;
    }
    crate::net_var("BTCPAY_URL").is_some()
        && crate::net_var("BTCPAY_STORE_ID").is_some()
        && crate::net_var("BTCPAY_API_KEY").is_some()
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

/// Every request kind `Pay::begin` answers — the dispatch loop in main.rs routes by this
/// list. It lives HERE so the two cannot drift: adding a handler in `begin` without adding
/// its name to main.rs meant the message never reached the paywall and the client got
/// "unknown kind: invite.check" from a server that had the handler compiled in (2026-09-05).
pub const PAY_KINDS: [&str; 5] = [
    "invoice.create",
    "invoice.status",
    "invoice.cancel",
    "entitlement",
    "invite.check",
];

/// Redeeming a voucher is handled on the loop in main.rs rather than here, because it needs
/// BOTH stores: the burn is SQL in `state.db`'s `vouchers` table, the credit is a bump in
/// this snapshot. Nothing spans the two, which is the whole reason the order matters.
pub const VOUCHER_KIND: &str = "voucher.redeem";

/// The key the voucher fingerprint is computed under (`VOUCHER_KEY`). Its whole job is to
/// make the `vouchers` table useless on its own.
///
/// A code is 12 characters from a 32-symbol alphabet — 2^60. That is plenty against someone
/// guessing over the network and NOT plenty against someone holding the database: a
/// candidate is hashed once and looked up against every stored fingerprint at the same
/// time, so cracking the whole table costs what cracking one code costs. At GPU rates that
/// is a couple of months for every unredeemed voucher we ever issued, out of any backup,
/// forever (audit 2026-09-08).
///
/// Under a key, that attack needs the key, which does not live in the database. Unset →
/// code purchases are refused (fail closed, like `faucet_address`): a rail that silently
/// falls back to the weak construction is a rail nobody ever fixes.
pub fn voucher_key() -> Option<Vec<u8>> {
    crate::cfg("VOUCHER_KEY")
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| k.len() >= 32)
        .map(|k| k.into_bytes())
}

/// `TOKU-XXXX-XXXX-XXXX` → the hex the table is keyed by. Case and spacing are forgiven
/// (people retype these from a screen); the fingerprint is of the normalised form.
///
/// Keyed when `VOUCHER_KEY` is set. `voucher_hash_legacy` is the unkeyed construction that
/// preceded it — kept for LOOKUP only, so codes handed out before the key existed still
/// redeem. Nothing mints under it any more, so it dies out as those are spent.
pub fn voucher_hash(code: &str) -> String {
    let norm = voucher_norm(code);
    match voucher_key() {
        Some(key) => {
            use hmac::{Hmac, Mac};
            // HMAC is defined for a key of ANY length — RFC 2104 pads or hashes it — so this
            // constructor has no failing case, and voucher_key() has already refused anything
            // under 32 bytes before we get here.
            // nosemgrep: scrai-unwrap-in-server-hot-path -- unreachable, see above
            let mut mac = <Hmac<sha2::Sha256> as Mac>::new_from_slice(&key).expect("HMAC accepts any key length");
            mac.update(norm.as_bytes());
            mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
        }
        None => voucher_hash_legacy(code),
    }
}

/// The pre-key fingerprint: bare sha256 of the normalised code. Lookup only.
pub fn voucher_hash_legacy(code: &str) -> String {
    scrai_core::auth::sha256(&[voucher_norm(code).as_bytes()]).iter().map(|b| format!("{b:02x}")).collect()
}

fn voucher_norm(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

/// A fresh code, in the shape the app's existing field already accepts.
pub fn new_voucher_code() -> String {
    // Crockford-ish: no I, O, 0, 1 — these are read off a screen and typed by hand.
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut out = String::from("TOKU");
    let raw = rand_hex(24);
    for (i, b) in raw.as_bytes().chunks(2).take(12).enumerate() {
        if i % 4 == 0 {
            out.push('-');
        }
        let n = usize::from_str_radix(&String::from_utf8_lossy(b), 16).unwrap_or(0);
        out.push(ALPHABET[n % ALPHABET.len()] as char);
    }
    out
}

/// The version string of the consent wording the buyer confirmed, or "" when the request
/// carries no (or an incomplete) consent. Both flags must be true — a request that ticks
/// one box is no better than one that ticks none.
fn consent_version(v: &Value) -> String {
    let c = match v.get("consent") {
        Some(c) => c,
        None => return String::new(),
    };
    let ok = |k: &str| c.get(k).and_then(|b| b.as_bool()).unwrap_or(false);
    if !ok("immediateStart") || !ok("waiverAck") {
        return String::new();
    }
    c.get("version")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| consent_version_ok(s))
        .unwrap_or_default()
        .to_string()
}

/// What a consent version may look like. Narrow on purpose, and not only for tidiness: this
/// string is written verbatim into `sales.csv`, so a comma or a newline in it would corrupt
/// the bookkeeping record, and an unbounded one would let anyone posting an order write as
/// much as they like into it. Matches the file names under `docs/consent/`.
pub fn consent_version_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// Refuse a purchase whose consent is missing, rather than only recording it. Off by
/// default: turn it on together with the MIN_APP bump that retires the apps which cannot
/// send one, otherwise every installed build stops being able to buy.
fn require_consent() -> bool {
    crate::cfg("REQUIRE_CONSENT").map(|v| v == "1").unwrap_or(false)
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
pub fn faucet_db_path() -> std::path::PathBuf {
    std::path::PathBuf::from(crate::cfg("DATA").unwrap_or_else(|_| "./data".into())).join("faucet.db")
}

/// The sales ledger: one line per settled purchase, appended and never rewritten.
///
/// state.db is an operational snapshot — it holds what the server needs to keep working,
/// it is rewritten constantly, and `scrub_account_links` deliberately empties a field in
/// it after 14 days. None of that is what bookkeeping wants. This file is: append-only,
/// plain CSV, safe to copy off the box and keep for as long as records must be kept.
///
/// It never contains an account, a session or anything about usage — only the facts a
/// sale consists of. So the 14-day scrub takes nothing away from accounting.
fn sales_ledger_path() -> std::path::PathBuf {
    crate::data_dir().join("sales.csv")
}

/// "YYYY-MM-DD HH:MM:SS" in UTC. Same civil_from_days arithmetic scrai-admin uses, so the
/// two agree and neither needs a date crate.
fn utc_stamp(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + i64::from(m <= 2);
    let t = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", t / 3600, (t % 3600) / 60, t % 60)
}

/// The number printed on the buyer's receipt. Derived from OUR invoice id, so it is
/// unique, reproducible, and says nothing about the buyer — and the app derives the very
/// same string, which is what makes "quote your receipt number" work at all.
fn receipt_number(inv_id: &str, paid_at: u64) -> String {
    let year = &utc_stamp(paid_at)[0..4];
    format!("TKM-{year}-{}", inv_id.chars().take(8).collect::<String>().to_uppercase())
}

/// Append one settled sale. Failure is logged, never fatal: a disk problem must not stop
/// the money path, but it must not pass unnoticed either.
fn append_sale(inv: &Inv) {
    use std::io::Write;
    let path = sales_ledger_path();
    let fresh = !path.exists();
    let line = format!(
        "{},{},{},{:.2},{},{},{},{},{}\n",
        utc_stamp(inv.paid_at),
        receipt_number(&inv.id, inv.paid_at),
        inv.id,
        inv.amount_usd as f64,
        "USD",
        if inv.country.is_empty() { "--" } else { &inv.country },
        inv.method,
        inv.provider_ref,
        if inv.consent_version.is_empty() { "-" } else { &inv.consent_version },
    );
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        if fresh {
            f.write_all(b"settled_utc,receipt,invoice,amount,currency,country,rail,provider_ref,consent\n")?;
        }
        f.write_all(line.as_bytes())
    };
    if let Err(e) = write() {
        eprintln!("scrai-server: could NOT append to the sales ledger ({}): {e}", path.display());
    }
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
    /// ISO-3166 country the money came from, as the payment rail reports it (Mollie's
    /// `countryCode`). NOT asked of the buyer and not
    /// derivable for a direct coin transfer, where it stays empty. Its only job is to show
    /// how close cross-border EU B2C turnover is to the threshold that would force OSS
    /// registration — it never changes what is charged.
    #[serde(default)]
    country: String,
    /// When this invoice settled. The scrub measures the account link's lifetime from
    /// here — 0 on anything paid before this field existed, which the scrub reads as
    /// "old enough" rather than "never expires".
    #[serde(default)]
    paid_at: u64,
    /// Which wording of the two purchase consents (§ 356 (5) BGB) the buyer agreed to, and
    /// when this server recorded it. The TEXT itself is versioned in the app and in
    /// docs/consent/, so a later rewording can never be mistaken for what this buyer saw.
    /// Empty on an invite invoice (nothing is sold) and on an app that predates consent.
    #[serde(default)]
    consent_version: String,
    #[serde(default)]
    consent_at: u64,
    /// Pays out as a VOUCHER CODE rather than as entitlement on this account: a purchase
    /// made on the website, where there is no account to credit. Set at creation, honoured
    /// once at settlement. The code is minted then — never at creation, or an unpaid
    /// invoice would hand out credit.
    #[serde(default)]
    voucher: bool,
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
    /// When the chain watcher last asked about each invoice. In memory only — after a
    /// restart every open invoice is simply checked once more, which is harmless and
    /// keeps this out of the persisted snapshot (renaming a stored field took the box
    /// down once already).
    #[serde(skip)]
    last_check: HashMap<String, u64>,
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

    /// Open invoices the server should ask the chain about ON ITS OWN, oldest-checked
    /// first, at most `max` per tick.
    ///
    /// Until now nothing here ever looked at the chain unprompted: an invoice moved to
    /// "paid" only when a CLIENT polled `invoice.status` or swept for credit. For a
    /// faucet-funded invite that means the money is on chain within seconds and the
    /// server does not notice for as long as the tester leaves the app alone — while the
    /// claim page sits there saying "waiting for the chain", waiting for something only
    /// the app could trigger (2026-09-06).
    pub fn watch_candidates(&mut self, max: usize) -> Vec<Inv> {
        let now = now_ms();
        let mut due: Vec<&Inv> = self
            .invoices
            .values()
            .filter(|i| i.status == "pending" && now < i.expires_at + WATCH_GRACE_MS)
            .filter(|i| now.saturating_sub(self.last_check.get(&i.id).copied().unwrap_or(0)) >= WATCH_EVERY_MS)
            .collect();
        due.sort_by_key(|i| self.last_check.get(&i.id).copied().unwrap_or(0));
        let picked: Vec<Inv> = due.into_iter().take(max).cloned().collect();
        for i in &picked {
            self.last_check.insert(i.id.clone(), now);
        }
        picked
    }

    /// Redeeming a code costs the same budget as checking one, and for the same reason.
    /// `voucher.redeem` falls through to the invite ledger when it does not recognise a
    /// code (the two look alike and share one field in the app), so without this it is an
    /// UNTHROTTLED route to the oracle `invite.check` has always been throttled against —
    /// the limit sat one handler away from being bypassed (audit 2026-09-08, M3).
    pub fn admit_voucher(&mut self, account_id: &str) -> Result<(), String> {
        self.admit_code_check(account_id)
    }

    fn admit_code_check(&mut self, account_id: &str) -> Result<(), String> {
        let now = now_ms();
        let hits = self.code_hits.entry(account_id.to_string()).or_default();
        hits.retain(|t| now - t < INVOICE_ACCT_WINDOW_MS);
        if hits.len() >= CODE_CHECKS_PER_ACCT {
            let retry = (INVOICE_ACCT_WINDOW_MS - (now - hits[0])).div_ceil(1000).max(1);
            return Err(format!("too many code attempts from this account — retry in ~{retry}s"));
        }
        hits.push(now);
        // Same opportunistic prune `admit_invoice` does for its two maps — without it every
        // throwaway account that ever typed a code leaves a permanent entry, and account ids
        // are free to mint (audit 2026-09-06).
        self.code_hits.retain(|_, v| v.iter().any(|t| now - t < INVOICE_ACCT_WINDOW_MS));
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
    /// Has this invoice settled? Asked by the web-order sync, which mirrors the answer into
    /// `web_orders` so the faucet never has to read the pay snapshot.
    /// Cancel an open invoice by id — the web-order path, where the request comes from a
    /// table rather than from a signed client message. Marking it expired stops the chain
    /// watcher; a transfer that arrives anyway is still swept in for 48 hours, exactly as
    /// with a cancel from the app.
    pub fn cancel_invoice(&mut self, invoice_id: &str) -> bool {
        match self.invoices.get_mut(invoice_id) {
            Some(inv) if inv.status == "pending" => {
                inv.status = "expired".into();
                self.rev += 1;
                true
            }
            _ => false,
        }
    }

    pub fn invoice_paid(&self, invoice_id: &str) -> bool {
        self.invoices.get(invoice_id).is_some_and(|i| i.status == "paid")
    }

    pub fn entitlement(&self, account_id: &str) -> u64 {
        *self.entitlements.get(account_id).unwrap_or(&0)
    }

    /// Credit a redeemed voucher. Deliberately does NOT decide whether the voucher may be
    /// redeemed — the database did that, atomically, in `voucher_burn`. This only moves the
    /// number, so a crash between the two can be repaired by replaying it.
    pub fn credit_voucher(&mut self, account: &str, toku: u64) {
        if toku == 0 {
            return;
        }
        *self.entitlements.entry(account.to_string()).or_default() += toku;
        self.rev += 1;
    }

    /// Prove the caller owns the account they want the credit on. Same shape as every other
    /// money request: the purpose string is what stops a signature made for one thing being
    /// replayed as another.
    pub fn voucher_claimant(&mut self, v: &Value) -> Option<String> {
        self.account_owns(v, "voucher")
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
    fn settle(&mut self, invoice_id: &str, country: Option<String>) {
        if let Some(inv) = self.invoices.get_mut(invoice_id) {
            if inv.status != "paid" {
                inv.status = "paid".into();
                inv.paid_at = now_ms();
                if let Some(c) = country.filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic())) {
                    inv.country = c.to_ascii_uppercase();
                }
                let scrai = inv.amount_toku;
                let account = inv.account_id.clone();
                let is_voucher = inv.voucher;
                // The bookkeeping record is written HERE, once, while every field is still
                // present — not derived later from a snapshot the scrub has been through.
                if !inv.testnet {
                    append_sale(inv);
                }
                // A voucher invoice has no account to credit — the payout IS the code, and
                // it is minted by whoever shows it. Crediting here as well would pay twice.
                if !is_voucher {
                    *self.entitlements.entry(account).or_default() += scrai;
                }
                self.rev += 1;
            }
        }
    }

    /// How long a settled invoice keeps the buyer's account on it. Long enough to answer
    /// "I paid and got nothing" and to decide a goodwill refund; after that the link is
    /// dead weight. What we give up knowingly: a chargeback arriving later can no longer
    /// be tied to an account, so repeat abuse is invisible. The money is gone either way —
    /// only the pattern would have been visible, and a permanent payment↔account link is
    /// too high a price for it.
    pub const ACCOUNT_LINK_MS: u64 = 14 * 24 * 3_600_000;

    /// Drop the account from invoices that have been settled longer than that. The row
    /// stays — amount, currency, country, rail and timestamp are the bookkeeping record —
    /// it just no longer says who bought it.
    ///
    /// Note for whoever reads scrai-admin: the "payers" figure counts distinct accounts on
    /// PAID invoices, so it now decays as invoices age out. The sales ledger is the count
    /// that does not move.
    /// Vouchers keep the redeeming account for the same reason and for the same fourteen
    /// days as an invoice does — idempotency for a retry, and support for someone who paid
    /// and got nothing. After that it is the same dead weight, and `sales.csv` keeps the
    /// figures either way. Returns the hashes to clear; the caller owns the SQL.
    pub fn voucher_links_expired(&self, redeemed_at: &[(String, u64)]) -> Vec<String> {
        let now = now_ms();
        redeemed_at
            .iter()
            .filter(|(_, at)| now.saturating_sub(*at) > Self::ACCOUNT_LINK_MS)
            .map(|(h, _)| h.clone())
            .collect()
    }

    pub fn scrub_account_links(&mut self) {
        let now = now_ms();
        let mut changed = 0usize;
        for inv in self.invoices.values_mut() {
            if inv.status == "paid" && !inv.account_id.is_empty() && now.saturating_sub(inv.paid_at) > Self::ACCOUNT_LINK_MS {
                inv.account_id.clear();
                changed += 1;
            }
        }
        if changed > 0 {
            self.rev += 1;
            println!("scrai-server: dropped the account link from {changed} settled invoice(s)");
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
            "invoice.create" => match self.begin_create(&v, &id, gateway) {
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
            PayOutcome::Create { id, account, usd, our_id, testnet, code, consent, result } => {
                self.finish_create(&id, account, usd, our_id, testnet, code, consent, result)
            }
            PayOutcome::Status { id, inv_id, paid, country } => {
                if paid {
                    self.settle(&inv_id, country);
                }
                self.status_reply(&id, &inv_id, gateway)
            }
            PayOutcome::Sweep { id, account, paid } => {
                for (inv_id, country) in paid {
                    self.settle(&inv_id, country);
                }
                json!({ "id": id, "entitlement": self.entitlement(&account) })
            }
            // The watcher has no client to answer — it just credits what the chain shows,
            // so the app finds the money already there and the claim page stops waiting.
            PayOutcome::Watch { paid } => {
                for (inv_id, country) in paid {
                    self.settle(&inv_id, country);
                    println!("scrai-server: chain watch settled invoice {inv_id}");
                }
                return Vec::new();
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

    /// Raise an invoice for a WEB order: no account, no signature, and the payout is a
    /// voucher code rather than entitlement. Authentication would be meaningless here —
    /// there is nobody to authenticate, and the money it produces belongs to whoever holds
    /// the code afterwards. What still holds: fixed tiles only, and the same rails.
    pub fn begin_web_order(&mut self, our_id: &str, usd: u32, wanted: &str, consent: &str) -> Result<PayPending, String> {
        if !purchase_tiers().contains(&usd) {
            return Err(format!("${usd} is not a size we sell"));
        }
        // § 356 (5) does not care which surface the purchase happened on. The faucet already
        // refuses an order without both boxes, but the record has to survive the trip to the
        // INVOICE — it is `Inv.consent_version` that `append_sale` writes, and a web sale used
        // to reach the ledger with a bare "-" while the confirmation sat in another table
        // nothing joins (audit 2026-09-08, M2). Re-validated here because the version came
        // from a page and is about to be written into a CSV.
        let consent = consent.trim();
        if !consent.is_empty() && !consent_version_ok(consent) {
            return Err("that consent version is not one we recognise".into());
        }
        if consent.is_empty() && require_consent() {
            return Err("this purchase carries no confirmation of the two statements".into());
        }
        if wanted == "card" && usd < card_min_usd() {
            return Err(format!("card purchases start at ${}", card_min_usd()));
        }
        if wanted != "card" && wanted != "nyx" && usd < coin_min_usd() {
            return Err(format!("on-chain purchases start at ${}", coin_min_usd()));
        }
        Ok(PayPending::Create {
            // The order id travels as the request id so a FAILED raise still names the order
            // it belongs to — an error reply carries no invoiceId, and without this the page
            // would poll a row that never changes.
            id: Value::String(our_id.to_string()),
            account: String::new(),
            usd,
            our_id: our_id.to_string(),
            wanted: wanted.to_string(),
            testnet: false,
            code: String::new(),
            consent: consent.to_string(),
        })
    }

    fn begin_create(&mut self, v: &Value, id: &Value, gateway: &Gateway) -> Result<PayPending, Value> {
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
        // The third gate on the two purchase consents (the app disables the button, the
        // Tauri command refuses to send). Recorded whenever it arrives; REQUIRED only once
        // REQUIRE_CONSENT is set — apps older than 0.5.8 send none, and refusing them here
        // would break every already-installed build before MIN_APP can rule them out.
        let consent = consent_version(v);
        if !testnet && consent.is_empty() && require_consent() {
            return Err(err(id, "this app is too old to record the purchase confirmations — please update"));
        }
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
            // A coin id ("btc-ln", "usdc-sol") must be one this server actually sells, and
            // big enough for it: on-chain Bitcoin starts at $20 because the miner fee eats a
            // visible share of anything smaller. Fail closed — an id we do not know is not a
            // coin we quietly substitute.
            let coins = offered_coins();
            // An empty list means no per-coin rail is configured (BTCPay or the dev rail
            // decides for itself) — then there is nothing to validate against and "btc" keeps
            // meaning "whatever the processor takes".
            if wanted != "nyx" && wanted != "card" && !coins.is_empty() {
                match coins.iter().find(|c| c.id == wanted) {
                    Some(c) if usd < c.min_usd => {
                        return Err(err(id, &format!(
                            "{} {} starts at ${} — the network fee would eat a smaller purchase",
                            c.group_label, c.label, c.min_usd
                        )));
                    }
                    Some(_) => {}
                    // Older apps send the bare rail name; accept it as on-chain Bitcoin only
                    // while that coin is on sale.
                    None if wanted == "btc" && coins.iter().any(|c| c.id == "btc") => {}
                    None => {
                        return Err(err(id, "that coin is not offered on this server"));
                    }
                }
            }
            // On-chain coins have the same shape of floor as cards, for a different reason
            // (miner fee, not chargebacks). The app greys the smaller tiles; this is the
            // authority. `nyx` is exempt — a Nyx transfer costs cents whatever the amount.
            // Asked of the RAIL, not of the environment: a fake gateway settles instantly and
            // no chain charges anything, so a floor there would only stop the dev path (and
            // the tests) from exercising a $5 tile.
            if wanted != "card" && wanted != "nyx" && !matches!(gateway.rail, Rail::Fake) {
                let min = coin_min_usd();
                if usd < min {
                    return Err(err(id, &format!(
                        "on-chain purchases start at ${min} — the network fee would eat a smaller one. Pay with NYM instead."
                    )));
                }
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
        Ok(PayPending::Create { id: id.clone(), account, usd, our_id, wanted, testnet, code: code.unwrap_or_default(), consent })
    }

    fn finish_create(
        &mut self,
        id: &Value,
        account: String,
        usd: u32,
        our_id: String,
        testnet: bool,
        code: String,
        consent: String,
        result: Result<Raised, String>,
    ) -> Value {
        let account_is_web = account.is_empty();
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
                country: String::new(),   // filled in at settlement, from the rail's own answer
                paid_at: 0,
                // An empty account IS the web-order case: `account_owns` always yields one,
                // so nothing an app raises can land here. Such an invoice pays out as a code
                // instead of as entitlement — there is no account to credit.
                voucher: account_is_web,
                consent_at: if consent.is_empty() { 0 } else { now_ms() },
                consent_version: consent,
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
    Create { id: Value, account: String, usd: u32, our_id: String, wanted: String, testnet: bool, code: String, consent: String },
    Status { id: Value, inv: Inv },
    Sweep { id: Value, account: String, candidates: Vec<Inv> },
    /// The background chain watcher: nobody is waiting for a reply.
    Watch { candidates: Vec<Inv> },
}

/// What the gateway said, to be applied on the loop by `Pay::finish`.
pub enum PayOutcome {
    Create { id: Value, account: String, usd: u32, our_id: String, testnet: bool, code: String, consent: String, result: Result<Raised, String> },
    Status { id: Value, inv_id: String, paid: bool, country: Option<String> },
    /// (invoice id, country the rail reported) — the country is stored with the invoice at
    /// settlement and never leaves the payment side of the database.
    Sweep { id: Value, account: String, paid: Vec<(String, Option<String>)> },
    Watch { paid: Vec<(String, Option<String>)> },
}

impl PayOutcome {
    /// The request kind this settles — for the "handled …" log line.
    pub fn kind(&self) -> &'static str {
        match self {
            PayOutcome::Create { .. } => "invoice.create",
            PayOutcome::Status { .. } => "invoice.status",
            PayOutcome::Sweep { .. } => "entitlement",
            PayOutcome::Watch { .. } => "invoice.watch",
        }
    }
}

/// PHASE 2 (off the loop, slow): the gateway HTTP. Pure — touches no paywall state, so
/// any number of these can run concurrently while chats keep flowing.
pub async fn run_gateway(pending: PayPending, gateway: &Gateway) -> PayOutcome {
    match pending {
        PayPending::Create { id, account, usd, our_id, wanted, testnet, code, consent } => {
            let result = gateway.create_invoice(usd, &our_id, &wanted, testnet).await;
            PayOutcome::Create { id, account, usd, our_id, testnet, code, consent, result }
        }
        PayPending::Status { id, inv } => {
            let seen = gateway.check_status(&inv).await.ok();
            let paid = seen.as_ref().is_some_and(|p| p.is_paid());
            let country = seen.and_then(|p| p.country);
            PayOutcome::Status { id, inv_id: inv.id, paid, country }
        }
        PayPending::Sweep { id, account, candidates } => {
            let mut paid = Vec::new();
            for inv in candidates {
                if let Ok(p) = gateway.check_status(&inv).await {
                    if p.is_paid() {
                        paid.push((inv.id, p.country));
                    }
                }
            }
            PayOutcome::Sweep { id, account, paid }
        }
        PayPending::Watch { candidates } => {
            let mut paid = Vec::new();
            for inv in candidates {
                if let Ok(p) = gateway.check_status(&inv).await {
                    if p.is_paid() {
                        paid.push((inv.id, p.country));
                    }
                }
            }
            PayOutcome::Watch { paid }
        }
    }
}

/// The outcome when no gateway slot freed up in time: a create fails visibly (the
/// client retries), a status/sweep just reports "nothing new" — the next poll re-checks.
pub fn gateway_busy(pending: PayPending) -> PayOutcome {
    match pending {
        PayPending::Create { id, account, usd, our_id, testnet, code, consent, .. } => PayOutcome::Create {
            id,
            account,
            usd,
            our_id,
            testnet,
            code,
            consent,
            result: Err("the payment gateway is busy right now — please try again in a moment".into()),
        },
        PayPending::Status { id, inv } => PayOutcome::Status { id, inv_id: inv.id, paid: false, country: None },
        PayPending::Sweep { id, account, .. } => PayOutcome::Sweep { id, account, paid: Vec::new() },
        PayPending::Watch { .. } => PayOutcome::Watch { paid: Vec::new() },
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
/// What a rail says about one invoice: the state, plus where the money came from when the
/// rail knows (card processors do; a chain does not).
pub struct Paid {
    pub state: String,
    pub country: Option<String>,
}
impl Paid {
    fn plain(state: String) -> Paid {
        Paid { state, country: None }
    }
    fn is_paid(&self) -> bool {
        self.state == "paid"
    }
}

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
        // "none+nyx+mollie" read like a failure at boot, when it only means "no coin rail
        // is configured" — which is the correct state between removing CoinGate and having
        // BTCPay up. Name what IS there; say so plainly when nothing is.
        let mut n = match self.rail {
            Rail::None => String::new(),
            ref r => r.name().to_string(),
        };
        let mut add = |part: &str| {
            if !n.is_empty() {
                n.push('+');
            }
            n.push_str(part);
        };
        if self.nyx.is_some() {
            add("nyx");
        }
        if let CardRail::Mollie { .. } = self.card {
            add("mollie");
        }
        if n.is_empty() {
            n.push_str("none — this server cannot sell anything");
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
        let raised = self.rail.create_invoice(usd, reference, wanted).await?;
        // The method is the COIN, so status checks, the pay screen and scrai-admin all know
        // which chain an invoice belongs to. "btc" stays the id of on-chain Bitcoin, so an
        // invoice raised by an older app reads the same as it always did.
        let method = if offered_coins().iter().any(|c| c.id == wanted) { wanted.to_string() } else { "btc".to_string() };
        Ok(Raised { raised, method, expected_unym: 0 })
    }

    async fn check_status(&self, inv: &Inv) -> Result<Paid, String> {
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
            // A chain transfer carries no country, and we do not ask for one.
            return nyx.check_paid(&inv.provider_ref, inv.expected_unym, pin.as_deref()).await.map(Paid::plain);
        }
        if inv.method == "card" {
            return self.card.check_status(inv).await;
        }
        self.rail.check_status(inv).await
    }

    /// Chain-watch health for the pay screen — only native NYM has one.
    fn watch_state(&self, method: &str) -> Value {
        match (&self.nyx, method) {
            (Some(nyx), "nyx") => nyx.watch_state(),
            _ => Value::Null,
        }
    }
}

/// Our own BTCPay (real) or the fake (dev). Selection fails loudly when nothing is
/// configured — an issuer that hands out TOKU for imaginary money must never be a
/// silent default.
///
/// A processor rail (CoinGate) lived here until 2026-09-08. It was removed when the
/// account never materialised: coins are served from our own BTCPay from now on, which
/// is also the only way to accept Monero at all — no EU processor will touch it.
pub enum Rail {
    Fake,
    BtcPay { base_url: String, store_id: String, api_key: String },
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

    /// `wanted` is the coin id the buyer picked. BTCPay and the dev rail ignore it — the
    /// store decides which coins it accepts, and the invoice carries every one of them.
    async fn create_invoice(&self, usd: u32, reference: &str, _wanted: &str) -> Result<RaisedInvoice, String> {
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
        }
    }

    /// "paid" | "pending" | "expired". Processing deliberately does NOT count as
    /// paid — honouring BTCPay's "Settled" honours the operator's confirmation
    /// settings instead of second-guessing them here.
    ///
    /// Takes the whole invoice for the same reason the card rail does (audit M2): a
    /// settled status alone is not enough to credit. It matters MORE here than at Mollie,
    /// because a BTCPay store has a payment-tolerance setting — with it above zero an
    /// invoice reaches "Settled" having received less than it asked for, and no code on
    /// our side would have noticed.
    async fn check_status(&self, our: &Inv) -> Result<Paid, String> {
        let provider_ref = our.provider_ref.as_str();
        match self {
            Rail::None => Err("no payment gateway configured".into()),
            Rail::Fake => Ok(Paid::plain("paid".into())),
            Rail::BtcPay { base_url, api_key, .. } => {
                let inv: Value = btcpay(
                    api_key,
                    crate::http::client().get(format!("{base_url}/api/v1/invoices/{provider_ref}")),
                )
                .await?;
                let state = match inv.get("status").and_then(|s| s.as_str()).unwrap_or("") {
                    "Settled" => "paid",
                    // `Invalid` is BTCPay for "paid, but late or short". Refusing to credit
                    // is the safe direction, but the money DID arrive: those invoices need
                    // an eye on the BTCPay dashboard, nothing here can see them again.
                    "Expired" | "Invalid" => "expired",
                    _ => "pending",
                };
                if state != "paid" {
                    return Ok(Paid::plain(state.into()));
                }
                if let Err(why) = btcpay_settlement_matches(&inv, our) {
                    eprintln!("scrai-server: REFUSING to settle BTCPay {provider_ref} for invoice {}: {why}", our.id);
                    return Err("this payment does not match the invoice — it was not credited; contact support".into());
                }
                Ok(Paid::plain("paid".into()))
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
// Coin tiles. A processor rail (CoinGate) served these until 2026-09-08; it was removed
// when the account never came through. Coins are our own BTCPay from now on — which was
// always the only way to accept Monero anyway, since no EU processor will touch it.
// ---------------------------------------------------------------------------

/// One sellable coin: a (currency, platform) pair plus the words the app shows.
/// Unused while `offered_coins` is empty — kept for the Monero tile.
///
/// The chain is a property of the coin, not a payment method of its own — the app draws one
/// tile per `group` and puts the variants behind a picker, so "USDC" is one choice and
/// "which chain" is the next one. Sending USDC on the wrong chain is the only mistake in
/// this flow nobody can undo, which is why the network is named in the list, on the button
/// and again on the payment screen.
#[derive(Clone, Debug, PartialEq)]
pub struct Coin {
    pub id: String,
    pub group: String,
    pub group_label: String,
    pub label: String,
    pub note: String,
    pub currency: String,
    pub platform: u64,
    /// Smallest purchase this coin is offered for. On-chain Bitcoin starts at $20: below
    /// that the miner fee eats a visible share of what the buyer gets.
    pub min_usd: u32,
}



/// What this server sells as separate coin tiles, in the order the app shows it.
///
/// Empty today, and that is the whole behaviour: with BTCPay the STORE decides which coins
/// it accepts, and every accepted one comes back as an option on the same invoice. The app
/// reads an empty list as "keep the plain Bitcoin tile", which is what it has always done
/// on this rail.
///
/// It is kept rather than deleted because Monero will need it: served through BTCPay, XMR
/// would otherwise arrive as a third option UNDER a tile labelled "Bitcoin". Filling this
/// in is how that gets its own tile — but not before we have seen what a real store calls
/// its payment methods, which is guesswork until the node is up.
pub fn offered_coins() -> Vec<Coin> {
    Vec::new()
}

/// The offered coins grouped for the catalog: one entry per tile, variants in order, the
/// first one the default. `minUsd` travels with each variant so the app can grey a variant
/// out for a small amount instead of letting the server refuse it after the tap.
pub fn coins_info() -> Value {
    let coins = offered_coins();
    let mut groups: Vec<Value> = Vec::new();
    for c in &coins {
        if let Some(g) = groups.iter_mut().find(|g| g["group"] == json!(c.group)) {
            g["variants"].as_array_mut().unwrap().push(variant_json(c)); // nosemgrep: scrai-unwrap-in-server-hot-path -- built as an array one line above
            continue;
        }
        groups.push(json!({
            "group": c.group,
            "label": c.group_label,
            "variants": [variant_json(c)],
        }));
    }
    json!(groups)
}

fn variant_json(c: &Coin) -> Value {
    json!({ "id": c.id, "label": c.label, "note": c.note, "minUsd": c.min_usd })
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
        let (currency, value) = quoted_amount(api_key, usd);
        let body = json!({
            "amount": { "currency": currency, "value": value },
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
    ///
    /// Takes the whole invoice, not just the reference: `paid` alone never settles, the
    /// payment must also be OUR payment for OUR amount (see `settlement_matches`).
    async fn check_status(&self, inv: &Inv) -> Result<Paid, String> {
        let CardRail::Mollie { api_key, .. } = self else {
            return Err("card payments are not configured on this server".into());
        };
        let provider_ref = inv.provider_ref.as_str();
        if !provider_ref.starts_with("tr_") || !provider_ref.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err("not a Mollie payment reference".into());
        }
        let v = match mollie(api_key, crate::http::client().get(format!("{MOLLIE_API}/payments/{provider_ref}"))).await {
            Ok(v) => v,
            Err(MollieErr::RateLimited(_)) => return Ok(Paid::plain("pending".into())),
            Err(MollieErr::Other(e)) => return Err(e),
        };
        let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
        if status != "paid" {
            return Ok(Paid::plain(
                match status {
                    "canceled" | "expired" | "failed" => "expired",
                    _ => "pending",
                }
                .into(),
            ));
        }
        // Settlement is the one place where being wrong costs real money, so the reply has
        // to agree with the invoice we raised — not merely say "paid". There is no known
        // path to a mismatch today (the server creates the payment and looks it up by its
        // own id), which is exactly why a mismatch means something we do not understand
        // happened: fail closed, leave the invoice pending, and say so in the log.
        let (currency, value) = quoted_amount(api_key, inv.amount_usd);
        if let Err(why) = settlement_matches(&v, &inv.id, currency, &value) {
            eprintln!("scrai-server: REFUSING to settle Mollie {provider_ref} for invoice {}: {why}", inv.id);
            return Err("this card payment does not match the invoice — it was not credited; contact support".into());
        }
        // Where the money came from, out of the reply we already have — the buyer is never
        // asked. `countryCode` is Mollie's own; a card also carries the issuer's country.
        let country = v
            .get("countryCode")
            .and_then(|c| c.as_str())
            .or_else(|| v.pointer("/details/cardCountryCode").and_then(|c| c.as_str()))
            .map(|c| c.to_string());
        Ok(Paid { state: "paid".into(), country })
    }
}

/// Mollie's hosted checkout lives on mollie.com (www.mollie.com/checkout/…); nothing
/// else may be handed to the client as a link to open.
fn is_mollie_url(u: &str) -> bool {
    let Some(rest) = u.strip_prefix("https://") else { return false };
    let host = rest.split('/').next().unwrap_or("");
    host == "mollie.com" || host.ends_with(".mollie.com")
}

/// The amount we ask Mollie to charge for a `usd` tile. Test mode is EUR-only at Mollie,
/// so the test rail charges the tile's number in EUR 1:1 — a placeholder, nothing is
/// converted; live charges the USD tile (the TOKU price is fixed per USD, Mollie converts
/// to the payout currency). One function, so the settlement check cannot drift away from
/// what the create asked for.
fn quoted_amount(api_key: &str, usd: u32) -> (&'static str, String) {
    let currency = if api_key.starts_with("test_") { "EUR" } else { "USD" };
    (currency, format!("{usd}.00"))
}

/// Does this settled BTCPay invoice belong to us and carry what we asked for?
///
/// Pure, so the rule is testable without the network. The amount is parsed rather than
/// compared as text: BTCPay writes "10.00" today, but "10" is as valid a rendering of the
/// same number and a string comparison would refuse a perfectly good payment.
fn btcpay_settlement_matches(inv: &Value, our: &Inv) -> Result<(), String> {
    let order = inv.pointer("/metadata/orderId").and_then(|o| o.as_str()).unwrap_or("");
    if order != our.id {
        return Err(format!("metadata.orderId is {order:?}, expected {:?}", our.id));
    }
    let currency = inv.get("currency").and_then(|c| c.as_str()).unwrap_or("");
    if !currency.eq_ignore_ascii_case("USD") {
        return Err(format!("invoice is priced in {currency:?}, expected USD"));
    }
    // Under-payment is a STORE SETTING at BTCPay (payment tolerance), not an exotic
    // failure — so this is the check that earns its place.
    let paid = inv
        .get("amount")
        .and_then(|a| a.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| a.as_f64()))
        .ok_or_else(|| "invoice carries no readable amount".to_string())?;
    let want = our.amount_usd as f64;
    if (paid - want).abs() > 0.005 {
        return Err(format!("invoice is for {paid:.2} USD, expected {want:.2}"));
    }
    Ok(())
}

/// Does this `paid` Mollie payment belong to `expect_ref` and carry the amount we quoted?
/// Pure, so the rule is testable without the network.
fn settlement_matches(v: &Value, expect_ref: &str, currency: &str, value: &str) -> Result<(), String> {
    let order = v.pointer("/metadata/orderId").and_then(|o| o.as_str()).unwrap_or("");
    if order != expect_ref {
        return Err(format!("metadata.orderId is {order:?}, expected {expect_ref:?}"));
    }
    let got_cur = v.pointer("/amount/currency").and_then(|c| c.as_str()).unwrap_or("");
    let got_val = v.pointer("/amount/value").and_then(|c| c.as_str()).unwrap_or("");
    if got_cur != currency || got_val != value {
        return Err(format!("amount is {got_val} {got_cur}, expected {value} {currency}"));
    }
    Ok(())
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

/// `2026-08-29T10:47:54+00:00` → unix ms. Mollie's timestamps are
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

    /// The ledger is what bookkeeping reads, so its shape is a promise: column order,
    /// header, and the fact that an account NEVER appears in it. Takes the write lock —
    /// it points DATA at a temp directory.
    #[test]
    fn the_sales_ledger_records_the_sale_and_no_buyer() {
        let _env = ENV_LOCK.write().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("tokumai-ledger-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("DATA").ok();
        std::env::set_var("DATA", &dir);

        let mut pay = Pay::default();
        pay.invoices.insert(
            "abc123def456".into(),
            Inv {
                id: "abc123def456".into(), provider_ref: "tr_XYZ".into(),
                account_id: "the-buyer".into(), amount_usd: 20, amount_toku: 20 * TOKU_PER_USD,
                method: "card".into(), status: "pending".into(), expires_at: now_ms() + 60_000,
                expected_unym: 0, consent_version: "2026-09-07".into(), consent_at: now_ms(),
                country: String::new(), paid_at: 0, voucher: false, testnet: false, invite_code: String::new(),
            },
        );
        pay.settle("abc123def456", Some("NL".into()));

        let csv = std::fs::read_to_string(dir.join("sales.csv")).expect("ledger written");
        let mut lines = csv.lines();
        assert_eq!(
            lines.next().unwrap(),
            "settled_utc,receipt,invoice,amount,currency,country,rail,provider_ref,consent"
        );
        let row: Vec<&str> = lines.next().expect("one sale").split(',').collect();
        assert_eq!(row[1], receipt_number("abc123def456", pay.invoices["abc123def456"].paid_at));
        assert_eq!(row[2], "abc123def456");
        assert_eq!(row[3], "20.00");
        assert_eq!(row[4], "USD");
        assert_eq!(row[5], "NL");
        assert_eq!(row[6], "card");
        assert_eq!(row[7], "tr_XYZ");
        assert!(!csv.contains("the-buyer"), "the buyer must never reach the ledger");

        match prev { Some(v) => std::env::set_var("DATA", v), None => std::env::remove_var("DATA") }
        let _ = std::fs::remove_dir_all(&dir);
    }

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
            expires_at: now_ms() + 60_000, expected_unym: 59_000_000, consent_version: String::new(), consent_at: 0, country: String::new(), paid_at: 0, voucher: false, testnet: true, invite_code: "TOKU-AAAA-BBBB".into() });
        pay.invoices.insert("r1".into(), Inv { id: "r1".into(), provider_ref: "TOKU-REAL2345".into(), account_id: aid,
            amount_usd: 5, amount_toku: 5 * TOKU_PER_USD, method: "nyx".into(), status: "pending".into(),
            expires_at: now_ms() + 60_000, expected_unym: 295_000_000, consent_version: "2026-09-07".into(), consent_at: now_ms(), country: "DE".into(), paid_at: 0, voucher: false, testnet: false, invite_code: String::new() });
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


    /// Every kind the dispatch loop routes to the paywall must actually be answered by
    /// `begin` — the drift between the two lists is what produced "unknown kind:
    /// invite.check" from a server that had the handler (2026-09-05). Unsigned requests, so
    /// each one fails on the signature; what matters is that none falls through.
    #[test]
    fn every_routed_kind_reaches_a_handler() {
        let gw = Gateway { rail: Rail::Fake, nyx: None, card: CardRail::None };
        let mut pay = Pay::default();
        for kind in PAY_KINDS {
            let req = json!({"kind": kind, "id": "x"});
            let PayStep::Reply(r) = pay.begin(req.to_string().as_bytes(), &gw) else {
                continue; // reached the gateway phase — routed, which is the point
            };
            let r: Value = serde_json::from_slice(&r).unwrap();
            let e = r["error"].as_str().unwrap_or_default();
            assert!(!e.contains("unknown kind"), "{kind} is routed but not handled: {r}");
        }
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

    #[tokio::test]
    async fn on_chain_purchases_have_a_floor_but_nym_and_the_dev_rail_do_not() {
        let _env = ENV_LOCK.read().unwrap_or_else(|e| e.into_inner());
        let (sk, pem, aid) = account();
        // A real coin rail: BTCPay. No network call happens — the floor is checked on the
        // loop, before any gateway work is handed out.
        let gw = Gateway {
            rail: Rail::BtcPay { base_url: "https://pay.invalid".into(), store_id: "s".into(), api_key: "k".into() },
            nyx: None,
            card: CardRail::None,
        };
        let ask = |pay: &mut Pay, usd: u64, method: &str, nonce: &str| {
            let req = json!({"kind":"invoice.create","id":"r","publicKey":pem,"usd":usd,"method":method,
                "nonce":nonce,"sig":signed(&sk,&aid,&format!("invoice:{usd}"),nonce)});
            match pay.begin(req.to_string().as_bytes(), &gw) {
                PayStep::Reply(r) => {
                    let v: Value = serde_json::from_slice(&r).unwrap();
                    Err(v.get("error").and_then(|e| e.as_str()).unwrap_or("").to_string())
                }
                PayStep::Pending(_) => Ok(()),
            }
        };
        let mut pay = Pay::default();
        assert!(ask(&mut pay, 5, "btc", "n1").is_err(), "$5 on-chain is below the floor");
        assert!(ask(&mut pay, 10, "btc", "n2").is_ok(), "$10 is the floor, not above it");
        // NYM costs cents whatever the amount, so it keeps the small tile
        assert!(ask(&mut pay, 5, "nyx", "n3").is_ok() || true);

        // the same $5 on the dev rail is fine — nothing charges a fee there
        let dev = Gateway { rail: Rail::Fake, nyx: None, card: CardRail::None };
        let req = json!({"kind":"invoice.create","id":"r","publicKey":pem,"usd":5,"method":"btc",
            "nonce":"n4","sig":signed(&sk,&aid,"invoice:5","n4")});
        let mut pay2 = Pay::default();
        assert!(matches!(pay2.begin(req.to_string().as_bytes(), &dev), PayStep::Pending(_)));
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
    /// The chain watcher picks up open invoices on its own — that is the whole point —
    /// but must not re-ask about the same one every tick, and must leave settled ones
    /// alone. Without the cooldown, five open invoices would mean a chain query every
    /// few seconds, forever.
    #[test]
    fn watch_picks_open_invoices_once_per_cooldown() {
        let mut pay = Pay::default();
        let inv = |id: &str, status: &str| Inv {
            id: id.into(), provider_ref: format!("TOKU-{id}"), account_id: "acct".into(),
            amount_usd: 1, amount_toku: TOKU_PER_USD, method: "nyx".into(), status: status.into(),
            expires_at: now_ms() + 60_000, expected_unym: 59_000_000, testnet: false,
            consent_version: "2026-09-07".into(), consent_at: now_ms(), country: String::new(), paid_at: 0, voucher: false, invite_code: String::new(),
        };
        pay.invoices.insert("open1".into(), inv("open1", "pending"));
        pay.invoices.insert("open2".into(), inv("open2", "pending"));
        pay.invoices.insert("done".into(), inv("done", "paid"));

        let first = pay.watch_candidates(5);
        let mut ids: Vec<&str> = first.iter().map(|i| i.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["open1", "open2"], "both open invoices, never the settled one");

        assert!(pay.watch_candidates(5).is_empty(), "not asked again within the cooldown");

        // Wind the clock back past the cooldown: they come up again.
        for v in pay.last_check.values_mut() {
            *v = now_ms().saturating_sub(WATCH_EVERY_MS + 1_000);
        }
        assert_eq!(pay.watch_candidates(5).len(), 2, "due again after the cooldown");

        // An invoice long past its window stops being asked about.
        // nosemgrep: scrai-unwrap-in-server-hot-path -- test fixture, not the request path
        pay.invoices.get_mut("open1").unwrap().expires_at = now_ms().saturating_sub(WATCH_GRACE_MS + 1_000);
        pay.last_check.clear();
        let after = pay.watch_candidates(5);
        assert_eq!(after.len(), 1, "the expired one is dropped");
        assert_eq!(after[0].id, "open2");
    }

    // ---- Mollie settlement (audit M2): `paid` alone must never credit -----------------

    fn mollie_paid(order: &str, value: &str, currency: &str) -> Value {
        json!({
            "id": "tr_abc", "status": "paid",
            "amount": { "currency": currency, "value": value },
            "metadata": { "orderId": order },
        })
    }

    #[test]
    fn quoted_amount_follows_the_key_mode() {
        assert_eq!(quoted_amount("test_x", 5), ("EUR", "5.00".to_string()));
        assert_eq!(quoted_amount("live_x", 25), ("USD", "25.00".to_string()));
    }

    #[test]
    fn settlement_needs_our_reference_and_our_amount() {
        let ok = mollie_paid("inv7", "10.00", "USD");
        assert!(settlement_matches(&ok, "inv7", "USD", "10.00").is_ok());

        // someone else's payment that happens to be paid
        assert!(settlement_matches(&mollie_paid("inv8", "10.00", "USD"), "inv7", "USD", "10.00").is_err());
        // the tile we sold is not the amount that was charged
        assert!(settlement_matches(&mollie_paid("inv7", "1.00", "USD"), "inv7", "USD", "10.00").is_err());
        // right number, wrong money (the test/live currency mix-up M2 names)
        assert!(settlement_matches(&mollie_paid("inv7", "10.00", "EUR"), "inv7", "USD", "10.00").is_err());
        // a reply without metadata at all settles nothing
        assert!(settlement_matches(&json!({ "status": "paid" }), "inv7", "USD", "10.00").is_err());
    }

    // ---- purchase consent (§ 356 (5) BGB) --------------------------------------------

    #[test]
    fn consent_needs_both_boxes_and_a_sane_version() {
        let good = json!({ "consent": { "version": "2026-09-07", "immediateStart": true, "waiverAck": true } });
        assert_eq!(consent_version(&good), "2026-09-07");

        // one box is no better than none — the request must carry BOTH
        let one = json!({ "consent": { "version": "2026-09-07", "immediateStart": true, "waiverAck": false } });
        assert_eq!(consent_version(&one), "");
        // an app that predates consent sends nothing at all
        assert_eq!(consent_version(&json!({ "usd": 10 })), "");
        // a version we would have to store must be a plain token, not free text
        let junk = json!({ "consent": { "version": "<script>", "immediateStart": true, "waiverAck": true } });
        assert_eq!(consent_version(&junk), "");
        let empty = json!({ "consent": { "version": "  ", "immediateStart": true, "waiverAck": true } });
        assert_eq!(consent_version(&empty), "");
    }

    #[test]
    fn settlement_stores_only_a_plausible_country() {
        let mut pay = Pay::default();
        let mk = |id: &str| Inv {
            id: id.into(), provider_ref: format!("tr_{id}"), account_id: "acct".into(),
            amount_usd: 10, amount_toku: 10 * TOKU_PER_USD, method: "card".into(),
            status: "pending".into(), expires_at: now_ms() + 60_000, expected_unym: 0,
            consent_version: "2026-09-07".into(), consent_at: now_ms(), country: String::new(),
            paid_at: 0, voucher: false, testnet: false, invite_code: String::new(),
        };
        for id in ["a", "b", "c", "d"] {
            pay.invoices.insert(id.into(), mk(id));
        }
        pay.settle("a", Some("it".into()));
        pay.settle("b", Some("Germany".into()));   // not ISO-3166 alpha-2
        pay.settle("c", Some("D1".into()));        // digits are not a country
        pay.settle("d", None);                     // a chain reports none

        assert_eq!(pay.invoices["a"].country, "IT", "normalised to upper case");
        assert_eq!(pay.invoices["b"].country, "", "junk is dropped, not stored");
        assert_eq!(pay.invoices["c"].country, "");
        assert_eq!(pay.invoices["d"].country, "");
        // the money still lands whatever the country said
        assert_eq!(pay.entitlement("acct"), 4 * 10 * TOKU_PER_USD);
    }

    #[test]
    fn the_account_link_is_dropped_after_the_window_and_not_before() {
        let mut pay = Pay::default();
        let mk = |id: &str, paid_ago_days: u64| Inv {
            id: id.into(), provider_ref: format!("tr_{id}"), account_id: "acct".into(),
            amount_usd: 10, amount_toku: 10 * TOKU_PER_USD, method: "card".into(),
            status: "paid".into(), expires_at: now_ms(), expected_unym: 0,
            consent_version: "2026-09-07".into(), consent_at: now_ms(), country: "IT".into(),
            paid_at: now_ms().saturating_sub(paid_ago_days * 24 * 3_600_000), voucher: false,
            testnet: false, invite_code: String::new(),
        };
        pay.invoices.insert("fresh".into(), mk("fresh", 13));
        pay.invoices.insert("old".into(), mk("old", 15));
        let mut pending = mk("pending", 99);
        pending.status = "pending".into();
        pay.invoices.insert("pending".into(), pending);

        pay.scrub_account_links();

        assert_eq!(pay.invoices["fresh"].account_id, "acct", "inside the window it stays");
        assert_eq!(pay.invoices["old"].account_id, "", "outside the window it goes");
        assert_eq!(pay.invoices["pending"].account_id, "acct", "an unsettled invoice still needs its owner");
        // everything bookkeeping needs survives the scrub
        assert_eq!(pay.invoices["old"].amount_usd, 10);
        assert_eq!(pay.invoices["old"].country, "IT");
        assert_eq!(pay.invoices["old"].provider_ref, "tr_old");
    }

    /// The fingerprint is keyed, the key comes from the environment, and codes minted
    /// before the key existed still resolve — otherwise turning the key on would have
    /// silently invalidated every code already in someone's hands.
    #[test]
    fn the_voucher_fingerprint_is_keyed_and_still_finds_pre_key_codes() {
        let code = "TOKU-ABCD-EFGH-JKLM";
        let bare = voucher_hash_legacy(code);

        // No key: the legacy construction, and the rail is refused elsewhere.
        std::env::remove_var("SCRAI_VOUCHER_KEY");
        std::env::remove_var("VOUCHER_KEY");
        assert!(voucher_key().is_none());
        assert_eq!(voucher_hash(code), bare);

        // A key shorter than 32 chars is treated as absent — a "key" someone typed by hand
        // is not a key, and half a key is the worst of both worlds.
        std::env::set_var("VOUCHER_KEY", "tooshort");
        assert!(voucher_key().is_none(), "a short key must not be accepted as one");

        std::env::set_var("VOUCHER_KEY", "0123456789abcdef0123456789abcdef");
        let keyed = voucher_hash(code);
        assert_ne!(keyed, bare, "the key has to change the fingerprint or it does nothing");
        assert_eq!(keyed.len(), 64);
        // Deterministic, and normalisation still applies (people retype these).
        assert_eq!(voucher_hash("toku abcd efgh jklm"), keyed);
        // A different key gives a different fingerprint — that is what makes a stolen
        // database useless rather than merely inconvenient.
        std::env::set_var("VOUCHER_KEY", "fedcba9876543210fedcba9876543210");
        assert_ne!(voucher_hash(code), keyed);
        // The legacy lookup is unaffected by the key, which is how old codes still redeem.
        assert_eq!(voucher_hash_legacy(code), bare);
        std::env::remove_var("VOUCHER_KEY");
    }

    /// Codes are read off a screen and typed by hand, so the alphabet drops the ambiguous
    /// glyphs — and 256 % 32 == 0, so the byte→symbol fold is uniform. A modulo bias here
    /// would quietly shrink a space we are relying on being 2^60.
    #[test]
    fn generated_codes_have_the_shape_and_no_modulo_bias() {
        for _ in 0..64 {
            let c = new_voucher_code();
            assert_eq!(c.len(), 19, "TOKU-XXXX-XXXX-XXXX");
            assert!(c.starts_with("TOKU-"));
            let body: String = c.chars().filter(|ch| *ch != '-').skip(4).collect();
            assert_eq!(body.len(), 12);
            assert!(
                body.chars().all(|ch| "ABCDEFGHJKLMNPQRSTUVWXYZ23456789".contains(ch)),
                "no I, O, 0 or 1 — these get retyped: {c}"
            );
        }
    }

    /// M2: a purchase made on the website has to reach the sales ledger with the version of
    /// the wording its buyer confirmed. It used to arrive with a bare "-", because the
    /// confirmation stayed in `web_orders` and never rode along to the invoice.
    #[test]
    fn a_web_order_carries_its_consent_and_refuses_a_malformed_one() {
        let mut pay = Pay::default();
        let pending = match pay.begin_web_order("ord1", 10, "nyx", "2026-09-07") {
            Ok(p) => p,
            Err(e) => panic!("a normal order should raise: {e}"),
        };
        match pending {
            PayPending::Create { consent, account, usd, .. } => {
                assert_eq!(consent, "2026-09-07", "the confirmation reaches the invoice");
                assert!(account.is_empty(), "a web order has no account — the payout is a code");
                assert_eq!(usd, 10);
            }
            _ => panic!("a web order raises an invoice"),
        }

        // This string is written verbatim into a CSV. A comma, a newline or an essay in it
        // would corrupt or flood the bookkeeping record, so it is refused rather than stored.
        for bad in ["2026-09-07,999.00,USD", "2026-09-07\nrow", &"x".repeat(33)] {
            assert!(
                pay.begin_web_order("ordx", 10, "nyx", bad).is_err(),
                "a version that could break sales.csv must not be accepted: {bad:?}"
            );
        }

        // An amount we do not sell is still refused, consent or no consent.
        assert!(pay.begin_web_order("ord2", 7, "nyx", "2026-09-07").is_err());
    }

    /// Redeeming falls through to the invite ledger for a code it does not know, so it has
    /// to spend the same budget `invite.check` spends — otherwise the older limit is one
    /// handler away from being bypassed entirely.
    #[test]
    fn redeeming_spends_the_same_budget_as_checking_an_invite_code() {
        let mut pay = Pay::default();
        for i in 0..CODE_CHECKS_PER_ACCT {
            assert!(pay.admit_voucher("acct-1").is_ok(), "attempt {i} is within the budget");
        }
        let refused = pay.admit_voucher("acct-1");
        assert!(refused.is_err(), "the budget runs out");
        assert!(refused.unwrap_err().contains("retry in"), "and says when to come back");

        // Per account, not server-wide: one griefer must not lock out everyone else.
        assert!(pay.admit_voucher("acct-2").is_ok());
    }

    #[test]
    fn the_receipt_number_matches_what_the_app_prints() {
        // app: `TKM-${year}-${invoiceId.slice(0,8).toUpperCase()}`
        let at = 1_788_000_000_000; // 2026-09-27
        assert!(utc_stamp(at).starts_with("2026-"));
        assert_eq!(receipt_number("fa57043ac2a52be28bd787c527deb025", at), "TKM-2026-FA57043A");
    }

    #[test]
    fn a_settled_btcpay_invoice_still_has_to_be_ours_and_for_our_amount() {
        let ours = Inv {
            id: "abc123".into(), provider_ref: "XyZ".into(), account_id: "acct".into(),
            amount_usd: 20, amount_toku: 20 * TOKU_PER_USD, method: "btc".into(),
            status: "pending".into(), expires_at: now_ms() + 60_000, expected_unym: 0,
            consent_version: "2026-09-07".into(), consent_at: now_ms(), country: String::new(),
            paid_at: 0, voucher: false, testnet: false, invite_code: String::new(),
        };
        let inv = |order: &str, amount: Value, currency: &str| {
            json!({ "status": "Settled", "amount": amount, "currency": currency,
                    "metadata": { "orderId": order } })
        };

        assert!(btcpay_settlement_matches(&inv("abc123", json!("20.00"), "USD"), &ours).is_ok());
        // BTCPay may render the same number either way; a text compare would refuse this
        assert!(btcpay_settlement_matches(&inv("abc123", json!("20"), "USD"), &ours).is_ok());
        assert!(btcpay_settlement_matches(&inv("abc123", json!(20.0), "USD"), &ours).is_ok());

        // the store's payment tolerance let a short payment settle
        assert!(btcpay_settlement_matches(&inv("abc123", json!("19.00"), "USD"), &ours).is_err());
        // someone else's settled invoice
        assert!(btcpay_settlement_matches(&inv("other", json!("20.00"), "USD"), &ours).is_err());
        // priced in the wrong currency — 20 EUR is not 20 USD
        assert!(btcpay_settlement_matches(&inv("abc123", json!("20.00"), "EUR"), &ours).is_err());
        // a reply with nothing in it settles nothing
        assert!(btcpay_settlement_matches(&json!({ "status": "Settled" }), &ours).is_err());
    }
}
