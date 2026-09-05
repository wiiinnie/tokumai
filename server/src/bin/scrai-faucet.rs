// scrai-faucet — pays testers' $1 testnet invoices from a Nyx testnet wallet.
//
// The app raises an ordinary invoice flagged `testnet:true` ($1, NYM rail) against a
// server running with TESTNET=1. The tester pastes the invoice's memo plus an
// invite code on the site this binary serves; the faucet checks the memo against the
// server's own state (read-only), sends EXACTLY the unym the server quoted to the
// server's receive address with that memo, and the server's chain watcher credits
// the account like any customer payment. No special credit path exists anywhere.
//
// Run modes:
//   scrai-faucet                       serve the site + API (default; needs TESTNET=1)
//   scrai-faucet code new [uses] [note] mint an invite code (printed once)
//   scrai-faucet code list             invite codes + remaining uses
//   scrai-faucet claims                payments made so far
//
// Abuse limits (all server-side, none of them client-visible knobs):
//   · one payment per memo (UNIQUE), one per invoice id (UNIQUE)
//   · invite code required, N uses each (default 3)
//   · amount = the server-pinned quote, never a client number
//   · daily claim cap (FAUCET_DAILY_MAX, default 20)
//   · wallet reserve the faucet will not dip below (FAUCET_RESERVE_UNYM, default 5 NYM)
//   · per-IP attempt limit (10/h, in memory, IP only ever hashed with a boot-time salt)
//
// Kill switch: TESTNET unset/0 → the site still serves (downloads) but the faucet
// section is hidden and /api/claim answers 403. The server side refuses testnet invoices
// under the same variable, so nothing half-works.
//
// HTTP: a deliberately tiny HTTP/1.1 responder on loopback, meant to sit behind Caddy or
// nginx (TLS, hostname, request normalisation). It refuses to bind a non-loopback
// address unless FAUCET_INSECURE_PUBLIC=1 — testers paste memos here, that must
// not travel in the clear.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nym_network_defaults::NymNetworkDetails;
use nym_validator_client::nyxd::{bip39, AccountId, Coin, Config, CosmWasmClient};
use nym_validator_client::DirectSigningHttpRpcNyxdClient;
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use scrai_server::faucet::{list_codes, mint, open_db as open_faucet_db, DEFAULT_CODE_USES};
use scrai_server::pay::{Pay, TestnetInv, TESTNET_USD};

const SITE: &str = include_str!("../../site/index.html");
/// Legal pages Mollie's onboarding checks for (imprint, terms, privacy) — static, no
/// placeholders, served as-is.
const PAGE_IMPRINT: &str = include_str!("../../site/imprint.html");
const PAGE_TERMS: &str = include_str!("../../site/terms.html");
const PAGE_PRIVACY: &str = include_str!("../../site/privacy.html");
/// Hand-over page for a top-up started in the app (Apple's IAP gate: the purchase is
/// raised and signed in the app, the payment itself happens here in the browser). The
/// invoice rides in the URL FRAGMENT, so it never reaches this server — nothing to log,
/// nothing to store, and this route serves one static file to everyone.
const PAGE_PAY: &str = include_str!("../../site/pay.html");
/// Where a tester redeems an invite code. The app links here with the code and the memo
/// in the URL fragment, so neither reaches this server until the button is pressed.
const PAGE_CLAIM: &str = include_str!("../../site/claim.html");
/// The site's screenshots, baked into the binary so a deploy ships them (Caddy only knows
/// /dl/; nothing else to upload or configure). Served as GET /img/<name>.
/// Where Mollie's hosted checkout sends the browser afterwards (`MOLLIE_REDIRECT_URL`
/// defaults to this host's /paid). Static, no script, no cookie, no order id in the URL
/// or the page — the app learns about the payment from its own status poll, so this page
/// links nothing back to anything. Same palette as the site, no external fonts (the CSP
/// blocks them anyway).
const PAID_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="referrer" content="no-referrer">
<meta name="robots" content="noindex">
<title>Payment received — tokumai</title>
<style>
  :root{--ink:#141210;--surface:#1C1917;--surface-2:#262220;--line:#332E2A;--bone:#ECE6DC;--muted:#9C938A;--signal:#CBA14E;--mix:#8AA06B;
    --mono:ui-monospace,SFMono-Regular,Menlo,monospace;--body:system-ui,-apple-system,'Hanken Grotesk',sans-serif;--display:Georgia,'Fraunces',serif}
  *{box-sizing:border-box}
  html,body{margin:0;background:var(--ink);color:var(--bone);font-family:var(--body);-webkit-font-smoothing:antialiased;min-height:100%}
  .wrap{max-width:520px;margin:0 auto;padding:64px 22px}
  .logo{font-family:var(--display);font-weight:700;font-size:22px;margin-bottom:38px}.logo b{color:var(--mix)}
  .card{border:1px solid var(--line);border-radius:16px;background:var(--surface);padding:26px 24px}
  .eyebrow{font-family:var(--mono);font-size:11.5px;letter-spacing:.12em;color:var(--mix);text-transform:uppercase;margin-bottom:10px}
  h1{font-family:var(--display);font-weight:700;font-size:30px;line-height:1.1;margin:0 0 14px}
  p{font-size:15px;line-height:1.55;color:var(--muted);margin:0 0 12px}
  p b{color:var(--bone)}
  .fine{font-family:var(--mono);font-size:11.5px;color:var(--muted);margin-top:22px;line-height:1.5}
</style>
</head>
<body>
<div class="wrap">
  <div class="logo">Scramble<b>AI</b></div>
  <div class="card">
    <div class="eyebrow">Card checkout</div>
    <h1>Payment received</h1>
    <p><b>You can close this tab and return to tokumai.</b> The app picks the payment up on its own and collects your credit — usually within a few seconds, no further steps.</p>
    <p>If the checkout was cancelled or failed, nothing was charged; pick an amount in the app again.</p>
    <div class="fine">This page holds no order details and sets no cookie. Once collected, the credit is unlinkable to this payment. No refunds after checkout.</div>
  </div>
</div>
</body>
</html>
"##;

const IMAGES: &[(&str, &[u8])] = &[
    ("flow-account-dark.jpg", include_bytes!("../../site/img/flow-account-dark.jpg")),
    ("flow-account-light.jpg", include_bytes!("../../site/img/flow-account-light.jpg")),
    ("flow-ready-dark.jpg", include_bytes!("../../site/img/flow-ready-dark.jpg")),
    ("flow-ready-light.jpg", include_bytes!("../../site/img/flow-ready-light.jpg")),
    ("flow-topup-dark.jpg", include_bytes!("../../site/img/flow-topup-dark.jpg")),
    ("flow-topup-light.jpg", include_bytes!("../../site/img/flow-topup-light.jpg")),
    ("hero-imagegen-dark.jpg", include_bytes!("../../site/img/hero-imagegen-dark.jpg")),
    ("hero-imagegen-light.jpg", include_bytes!("../../site/img/hero-imagegen-light.jpg")),
    ("how-mac-chat-dark.jpg", include_bytes!("../../site/img/how-mac-chat-dark.jpg")),
    ("how-mac-chat-light.jpg", include_bytes!("../../site/img/how-mac-chat-light.jpg")),
    ("how-mac-image-dark.jpg", include_bytes!("../../site/img/how-mac-image-dark.jpg")),
    ("how-mac-image-light.jpg", include_bytes!("../../site/img/how-mac-image-light.jpg")),
    // PLACEHOLDERS until the real captures land — see the comment in site/index.html.
    ("how-phone-buy-dark.jpg", include_bytes!("../../site/img/how-phone-buy-dark.jpg")),
    ("how-phone-buy-light.jpg", include_bytes!("../../site/img/how-phone-buy-light.jpg")),
    ("how-phone-menu-dark.jpg", include_bytes!("../../site/img/how-phone-menu-dark.jpg")),
    ("how-phone-menu-light.jpg", include_bytes!("../../site/img/how-phone-menu-light.jpg")),
    ("how-phone-chat-dark.jpg", include_bytes!("../../site/img/how-phone-chat-dark.jpg")),
    ("how-phone-chat-light.jpg", include_bytes!("../../site/img/how-phone-chat-light.jpg")),
    ("how-phone-image-dark.jpg", include_bytes!("../../site/img/how-phone-image-dark.jpg")),
    ("how-phone-image-light.jpg", include_bytes!("../../site/img/how-phone-image-light.jpg")),
]; // screenshots for the homepage, dark + light of each (the page shows one per theme)
const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 4 * 1024;
const IP_ATTEMPTS_PER_HOUR: usize = 10;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn env_or(name: &str, default: &str) -> String {
    scrai_server::cfg(name).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| default.to_string())
}

/// `https://validator-sandbox-1.nymtech.net/api` → `https://validator-sandbox-1.nymtech.net`.
/// Anything without a trailing `/api` is returned as-is (minus a trailing slash).
fn rpc_from_lcd(lcd: Option<&str>) -> String {
    let Some(l) = lcd else { return String::new() };
    let l = l.trim().trim_end_matches('/');
    l.strip_suffix("/api").unwrap_or(l).to_string()
}

fn testnet_on() -> bool {
    scrai_server::pay::is_testnet_server()
}

/// Whether the faucet hands out credit at all.
///
/// It used to ride on TESTNET: on a testnet server it was the only way to buy, on a
/// mainnet server it made no sense. Since the invite flow it runs BESIDE real purchases —
/// testers redeem a code for $1 while everyone else pays — so it has its own switch.
/// Default on; `FAUCET_ENABLED=0` is the kill switch. A faucet without a funded wallet
/// refuses every claim on its own anyway.
fn faucet_on() -> bool {
    !matches!(
        scrai_server::cfg("FAUCET_ENABLED").ok().as_deref().map(str::trim),
        Some("0") | Some("false")
    )
}

/// Everything the faucet needs, resolved once at boot. The mnemonic stays inside the
/// signing client; it is never logged or echoed.
struct Cfg {
    data: PathBuf,
    listen: String,
    rpc: String,
    receive: String,
    daily_max: u32,
    reserve_unym: u128,
    explorer: Option<String>,
    prefix: String,
    denom: String,
    /// where publish-downloads.sh puts the bundles + manifest.json (Caddy serves it as /dl/)
    dl_dir: PathBuf,
}

impl Cfg {
    fn load() -> Cfg {
        // From the install root, not just the current directory: a CLI run from a home
        // directory found no .env at all and then no database either.
        if let Err(e) = dotenvy::from_path(scrai_server::env_file()).or_else(|_| dotenvy::dotenv().map(|_| ())) {
            if !matches!(e, dotenvy::Error::Io(_)) {
                eprintln!("scrai-faucet: .env PARSE ERROR — variables after the bad line are NOT loaded (quote values with spaces): {e}");
            }
        }
        Cfg {
            data: scrai_server::data_dir(),
            listen: env_or("FAUCET_LISTEN", "127.0.0.1:8790"),
            // Tendermint RPC of the chain the server watches. Derived from the LCD the
            // server uses (NYX_LCD_URL_*: `<validator>/api` → `<validator>`), so the faucet
            // can never pay on a different chain by accident; FAUCET_RPC overrides
            // for hosts where LCD and RPC don't share a root.
            rpc: env_or("FAUCET_RPC", &rpc_from_lcd(scrai_server::net_var("NYX_LCD_URL").as_deref())),
            // the SAME receive address the server watches — a payment anywhere else is lost
            receive: scrai_server::net_var("NYX_RECEIVE_ADDRESS").unwrap_or_default(),
            daily_max: env_or("FAUCET_DAILY_MAX", "20").parse().unwrap_or(20),
            reserve_unym: env_or("FAUCET_RESERVE_UNYM", "5000000").parse().unwrap_or(5_000_000),
            explorer: scrai_server::cfg("FAUCET_EXPLORER").ok().filter(|u| u.starts_with("https://")),
            prefix: env_or("FAUCET_BECH32_PREFIX", "n"),
            denom: env_or("FAUCET_DENOM", "unym"),
            dl_dir: PathBuf::from(env_or("SITE_DL_DIR", "/opt/tokumai/site/dl")),
        }
    }
    fn state_db(&self) -> PathBuf {
        self.data.join("state.db")
    }
    fn faucet_db(&self) -> PathBuf {
        self.data.join("faucet.db")
    }
}

// ---------------------------------------------------------------------------
// the server's view: invite invoices from state.db (read-only, fresh per call)
// ---------------------------------------------------------------------------

fn server_invite_invoices(state_db: &Path) -> Result<Vec<TestnetInv>, String> {
    let conn = Connection::open_with_flags(state_db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("state.db: {e}"))?;
    let blob: String = match conn.query_row("SELECT v FROM kv WHERE k = 'pay'", [], |r| r.get(0)) {
        Ok(b) => b,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(Vec::new()),
        Err(e) => return Err(format!("state.db kv: {e}")),
    };
    let pay: Pay = serde_json::from_str(&blob).map_err(|e| format!("pay snapshot: {e}"))?;
    Ok(pay.testnet_invoices())
}

// ---------------------------------------------------------------------------
// the faucet's own ledger: invite codes + claims
// ---------------------------------------------------------------------------

fn claims_since(conn: &Connection, ts: u64) -> u32 {
    conn.query_row("SELECT COUNT(*) FROM claims WHERE ts >= ?1", [ts as i64], |r| r.get::<_, i64>(0))
        .map(|n| n as u32)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// chain: one signing client, built at boot when a mnemonic is present
// ---------------------------------------------------------------------------

struct Wallet {
    client: DirectSigningHttpRpcNyxdClient,
    denom: String,
}

impl Wallet {
    fn connect(cfg: &Cfg) -> Result<Option<Wallet>, String> {
        // Network-scoped: the sandbox wallet and the mainnet wallet can sit in the same
        // .env (FAUCET_MNEMONIC_TESTNET / _MAINNET), so flipping the server does not mean
        // editing a secret by hand. The bare name still resolves.
        let Some(m) = scrai_server::net_var("FAUCET_MNEMONIC").filter(|m| !m.trim().is_empty()) else {
            return Ok(None);
        };
        if cfg.rpc.is_empty() {
            return Err("no chain RPC: set NYX_LCD_URL_* (the faucet derives the RPC from it) or FAUCET_RPC".into());
        }
        let mnemonic: bip39::Mnemonic = m.trim().parse().map_err(|e| format!("FAUCET_MNEMONIC: {e}"))?;
        // Chain details: the Nyx sandbox shares prefix + denom with mainnet; the chain id
        // is read from the node on signing, contracts play no part in a bank send.
        let mut details = NymNetworkDetails::new_mainnet();
        details.chain_details.bech32_account_prefix = cfg.prefix.clone();
        details.chain_details.mix_denom.base = cfg.denom.clone();
        let config = Config::try_from_nym_network_details(&details).map_err(|e| format!("nyxd config: {e}"))?;
        let client = DirectSigningHttpRpcNyxdClient::connect_with_mnemonic(config, cfg.rpc.as_str(), mnemonic)
            .map_err(|e| format!("nyxd connect: {e}"))?;
        Ok(Some(Wallet { client, denom: cfg.denom.clone() }))
    }

    fn address(&self) -> String {
        self.client.address().to_string()
    }

    async fn balance_unym(&self) -> Result<u128, String> {
        let coin = self.client.get_balance(&self.client.address(), self.denom.clone()).await.map_err(|e| format!("balance: {e}"))?;
        Ok(coin.map(|c| c.amount).unwrap_or(0))
    }

    async fn pay(&self, to: &str, unym: u64, memo: &str) -> Result<String, String> {
        let recipient: AccountId = to.parse().map_err(|e| format!("receive address: {e}"))?;
        let coins = vec![Coin { amount: unym as u128, denom: self.denom.clone() }];
        let res = self.client.send(&recipient, coins, memo.to_string(), None).await.map_err(|e| format!("send: {e}"))?;
        Ok(res.hash.to_string())
    }
}

// ---------------------------------------------------------------------------
// the service
// ---------------------------------------------------------------------------

struct Faucet {
    cfg: Cfg,
    wallet: Option<Wallet>,
    /// serialises claims: the memo UNIQUE insert happens BEFORE the broadcast, so two
    /// concurrent requests for one memo can never both pay
    claim_lock: tokio::sync::Mutex<()>,
    /// attempts per hashed client IP (salted per boot; never persisted)
    attempts: Mutex<HashMap<u64, Vec<Instant>>>,
    salt: u64,
}

fn valid_code(s: &str) -> bool {
    (8..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
}
fn valid_memo(s: &str) -> bool {
    (4..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl Faucet {
    fn ip_ok(&self, ip: &str) -> bool {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.salt.hash(&mut h);
        ip.hash(&mut h);
        let key = h.finish();
        let mut map = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        let cutoff = Instant::now() - Duration::from_secs(3600);
        let v = map.entry(key).or_default();
        v.retain(|t| *t > cutoff);
        if v.len() >= IP_ATTEMPTS_PER_HOUR {
            return false;
        }
        v.push(Instant::now());
        if map.len() > 10_000 {
            map.clear(); // never let the map grow unbounded; a flood just resets everyone's window
        }
        true
    }

    /// The whole claim, start to finish. Every refusal is a plain sentence for the tester.
    async fn claim(&self, code: &str, memo: &str, ip: &str) -> Result<Value, String> {
        if !faucet_on() {
            return Err("the faucet is switched off on this server".into());
        }
        let Some(wallet) = &self.wallet else {
            return Err("the faucet wallet is not configured on this server".into());
        };
        if !valid_code(code) || !valid_memo(memo) {
            return Err("that doesn't look like an invite code / memo — copy both exactly as shown".into());
        }
        if !self.ip_ok(ip) {
            return Err("too many attempts from your connection — try again in an hour".into());
        }
        if self.cfg.receive.is_empty() {
            return Err("NYX_RECEIVE_ADDRESS is not set on this server".into());
        }

        let _guard = self.claim_lock.lock().await;
        let db = open_faucet_db(&self.cfg.faucet_db())?;

        // invite code
        let uses: Option<(i64, i64)> = db
            .query_row("SELECT max_uses, uses FROM codes WHERE code = ?1", [code], |r| Ok((r.get(0)?, r.get(1)?)))
            .ok();
        match uses {
            None => return Err("unknown invite code".into()),
            Some((max, used)) if used >= max => return Err("this invite code has been used up".into()),
            _ => {}
        }

        // the invoice, as the SERVER sees it
        let invs = server_invite_invoices(&self.cfg.state_db())?;
        let Some(inv) = invs.iter().find(|i| i.memo == memo) else {
            return Err("no open invite invoice with that memo — raise one in the app (Buy credit → enter your invite code) and copy its memo".into());
        };
        // The invoice remembers the code it was raised with. Without this check a valid
        // code could fund somebody else's open invoice — same amount, but the claim would
        // be booked against the wrong tester and their own invoice would still be waiting.
        if !inv.code.is_empty() && !inv.code.eq_ignore_ascii_case(code) {
            return Err("that invoice was raised with a different invite code — use the code you entered in the app".into());
        }
        if inv.status == "paid" {
            return Err("that invoice is already paid — the app should show the credit".into());
        }
        // `expires_at` is in ms (pay.rs `now_ms`); `now()` here is seconds
        if inv.status != "pending" || inv.expires_at <= (now() + 60) * 1000 {
            return Err("that invoice has expired — raise a fresh one in the app".into());
        }
        if inv.amount_usd != TESTNET_USD || inv.unym == 0 {
            return Err("that invoice is not a $1 NYM invite credit".into());
        }

        // one payment per memo / invoice — the row goes in BEFORE the broadcast
        let ts = now();
        let inserted = db.execute(
            "INSERT OR IGNORE INTO claims (memo, invoice_id, code, unym, tx, stage, ts) VALUES (?1, ?2, ?3, ?4, '', 'sending', ?5)",
            params![memo, inv.id, code, inv.unym as i64, ts as i64],
        );
        match inserted {
            Ok(1) => {}
            Ok(_) => return Err("this memo has already been funded".into()),
            Err(e) => return Err(format!("faucet.db: {e}")),
        }
        // caps — undo the row if a cap refuses, so the tester can come back tomorrow
        let day_start = ts - ts % 86_400;
        if claims_since(&db, day_start) > self.cfg.daily_max {
            let _ = db.execute("DELETE FROM claims WHERE memo = ?1 AND stage = 'sending'", [memo]);
            // Operator signal: the cap is FAUCET_DAILY_MAX in /opt/scrai/.env (restart
            // scrai-faucet after raising it). scrai-admin shows the same count in red.
            eprintln!(
                "scrai-faucet: DAILY LIMIT reached — {} claims today, max {} (FAUCET_DAILY_MAX in .env; restart scrai-faucet after raising) — refused memo {memo} code {code}",
                self.cfg.daily_max, self.cfg.daily_max
            );
            return Err("the faucet's daily limit is reached — try again tomorrow (the operator sees this and can raise it)".into());
        }
        let bal = match wallet.balance_unym().await {
            Ok(b) => b,
            Err(e) => {
                let _ = db.execute("DELETE FROM claims WHERE memo = ?1 AND stage = 'sending'", [memo]);
                return Err(format!("chain query failed: {e}"));
            }
        };
        if bal < self.cfg.reserve_unym + inv.unym as u128 {
            let _ = db.execute("DELETE FROM claims WHERE memo = ?1 AND stage = 'sending'", [memo]);
            eprintln!(
                "scrai-faucet: WALLET LOW — {:.3} NYM in {}, reserve {:.3} + quote {:.3} needed — top up the faucet wallet{} — refused memo {memo}",
                bal as f64 / 1e6,
                wallet.address(),
                self.cfg.reserve_unym as f64 / 1e6,
                inv.unym as f64 / 1e6,
                if testnet_on() { " (sandbox: https://sandbox-faucet.nymtech.net/)" } else { " — this is MAINNET NYM, it costs real money" }
            );
            return Err("the faucet wallet is running low — the operator sees this in the log; try again later".into());
        }

        // pay — exactly the quote, to the server's address, with the memo
        let tx = match wallet.pay(&self.cfg.receive, inv.unym, memo).await {
            Ok(tx) => tx,
            Err(e) => {
                // Broadcast failed or timed out. The row stays as 'sending' (no second attempt
                // without a human looking — the tx may still be in a mempool).
                let _ = db.execute("UPDATE claims SET stage = 'failed' WHERE memo = ?1", [memo]);
                eprintln!("scrai-faucet: send failed for memo {memo}: {e}");
                return Err("the transfer failed to broadcast — nothing was sent; try again in a minute".into());
            }
        };
        let _ = db.execute("UPDATE claims SET tx = ?2, stage = 'sent' WHERE memo = ?1", params![memo, tx]);
        let _ = db.execute("UPDATE codes SET uses = uses + 1 WHERE code = ?1", [code]);
        println!("scrai-faucet: funded memo {memo} · {} unym · tx {tx}", inv.unym);
        Ok(json!({
            "ok": true, "memo": memo, "unym": inv.unym, "nym": format!("{:.3}", inv.unym as f64 / 1e6),
            "to": self.cfg.receive, "tx": tx,
            "explorer": self.cfg.explorer.as_ref().map(|e| format!("{e}{tx}")),
        }))
    }

    /// Where a memo stands: open (unfunded) · sent (tx out) · credited (server saw it) ·
    /// expired · unknown. The page polls this after a claim.
    fn status(&self, memo: &str) -> Value {
        if !valid_memo(memo) {
            return json!({"stage": "unknown"});
        }
        let claim: Option<(String, String)> = open_faucet_db(&self.cfg.faucet_db())
            .ok()
            .and_then(|db| db.query_row("SELECT tx, stage FROM claims WHERE memo = ?1", [memo], |r| Ok((r.get(0)?, r.get(1)?))).ok());
        let inv = server_invite_invoices(&self.cfg.state_db()).ok().and_then(|v| v.into_iter().find(|i| i.memo == memo));
        let stage = match (&inv, &claim) {
            (Some(i), _) if i.status == "paid" => "credited",
            (Some(i), _) if i.status != "pending" || i.expires_at <= now() * 1000 => "expired",
            (_, Some((_, s))) if s == "sent" => "sent",
            (_, Some((_, s))) if s == "sending" || s == "failed" => "pending",
            (Some(_), None) => "open",
            _ => "unknown",
        };
        json!({
            "stage": stage,
            "tx": claim.as_ref().map(|c| c.0.clone()).filter(|t| !t.is_empty()),
            "explorer": claim.as_ref().and_then(|c| self.cfg.explorer.as_ref().map(|e| format!("{e}{}", c.0))).filter(|_| claim.as_ref().is_some_and(|c| !c.0.is_empty())),
            "unym": inv.as_ref().map(|i| i.unym),
        })
    }

    fn overview(&self) -> Value {
        let today = now() - now() % 86_400;
        let claims_today = open_faucet_db(&self.cfg.faucet_db()).map(|db| claims_since(&db, today)).unwrap_or(0);
        json!({
            "testnet": testnet_on(),
            "wallet": self.wallet.is_some(),
            "claimsToday": claims_today,
            "dailyMax": self.cfg.daily_max,
        })
    }
}

// ---------------------------------------------------------------------------
// the site: static HTML with a handful of {{placeholders}} from env
// ---------------------------------------------------------------------------

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// One published bundle as `publish-downloads.sh` describes it in manifest.json.
struct DlFile {
    name: String,
    sha256: String,
    bytes: u64,
}

/// `manifest.json` next to the bundles — written by publish-downloads.sh on every upload,
/// read here on every page view, so the site always shows the version that is actually
/// downloadable (no .env edit, no restart). Keys: macos · windows · appimage · deb.
fn read_manifest(dl_dir: &Path) -> (Option<String>, HashMap<String, DlFile>) {
    let Ok(raw) = std::fs::read_to_string(dl_dir.join("manifest.json")) else {
        return (None, HashMap::new());
    };
    let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let version = v.get("version").and_then(|x| x.as_str()).map(str::to_string);
    let mut files = HashMap::new();
    if let Some(obj) = v.get("files").and_then(|f| f.as_object()) {
        for (k, f) in obj {
            let name = f.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
            // the name becomes a URL path segment — no separators, no dot-dot
            if name.is_empty() || name.contains('/') || name.contains("..") || name.contains('\\') {
                continue;
            }
            files.insert(
                k.clone(),
                DlFile {
                    name,
                    sha256: f.get("sha256").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    bytes: f.get("bytes").and_then(|x| x.as_u64()).unwrap_or(0),
                },
            );
        }
    }
    (version, files)
}

fn human_mb(bytes: u64) -> String {
    if bytes == 0 { String::new() } else { format!("{:.0} MB", bytes as f64 / 1e6) }
}

fn site_html(dl_dir: &Path) -> String {
    let env_link = |var: &str| std::env::var(var).ok().filter(|u| u.starts_with("https://")).map(|u| html_escape(&u));
    let (mver, files) = read_manifest(dl_dir);
    let mut s = SITE.to_string();
    let off = |label: &str| format!(r#"<span class="btn off">{label} · not published yet</span>"#);
    // Bundles we host ourselves: from the manifest (relative /dl/ link on this very host).
    for (ph, key, label, primary) in [
        ("{{DL_MACOS}}", "macos", "Download .dmg", true),
        ("{{DL_WINDOWS}}", "windows", "Download installer (.exe)", true),
        // .deb is the STANDARD Linux download (host WebKit/GTK — robust); the AppImage
        // stays offered but experimental (bundled libs clash with newer stacks, see the
        // 2026-08-31 Kali report: grey window from a gvfs/EGL collision).
        ("{{DL_DEB}}", "deb", "Download .deb", true),
        ("{{DL_APPIMAGE}}", "appimage", "AppImage (experimental)", false),
        ("{{DL_ANDROID}}", "android", "Download .apk", true),
    ] {
        let cls = if primary { "btn primary" } else { "btn" };
        let html = match files.get(key) {
            Some(f) => format!(r#"<a class="{cls}" href="/dl/{}">{label}</a>"#, html_escape(&f.name)),
            None => off(label),
        };
        s = s.replace(ph, &html);
    }
    // iOS lives elsewhere (TestFlight / a guide page): env links.
    for (ph, var, label, primary) in [
        ("{{DL_IOS}}", "DL_IOS", "Join on TestFlight", true),
        ("{{DL_IOS_GUIDE}}", "DL_IOS_GUIDE", "Sideload guide for testers", false),
    ] {
        let cls = if primary { "btn primary" } else { "btn" };
        let html = match env_link(var) {
            Some(u) => format!(r#"<a class="{cls}" href="{u}">{label}</a>"#),
            None => off(label),
        };
        s = s.replace(ph, &html);
    }
    let meta = |key: &str, fallback: &str| -> String {
        match files.get(key) {
            Some(f) => html_escape(&format!("{}{}", f.name, if f.bytes > 0 { format!(" · {}", human_mb(f.bytes)) } else { String::new() })),
            None => fallback.to_string(),
        }
    };
    let sha = |key: &str| files.get(key).map(|f| html_escape(&f.sha256)).filter(|x| !x.is_empty()).unwrap_or_else(|| "—".into());
    // green card when something is actually downloadable; iOS greys until TestFlight exists
    s = s.replace("{{CLS_MACOS}}", if files.contains_key("macos") { " has" } else { "" });
    s = s.replace("{{CLS_WINDOWS}}", if files.contains_key("windows") { " has" } else { "" });
    s = s.replace("{{CLS_LINUX}}", if files.contains_key("appimage") || files.contains_key("deb") { " has" } else { "" });
    s = s.replace("{{CLS_ANDROID}}", if files.contains_key("android") { " has" } else { "" });
    s = s.replace("{{META_ANDROID}}", &meta("android", "APK · arm64 · Android 8+"));
    s = s.replace("{{SHA_ANDROID}}", &sha("android"));
    s = s.replace("{{CLS_IOS}}", if env_link("DL_IOS").is_some() { " has" } else { " soon" });
    // Only say "review pending" while there is no join link — once DL_IOS is set
    // the sentence would contradict the button right above it.
    s = s.replace(
        "{{NOTE_IOS_PENDING}}",
        if env_link("DL_IOS").is_some() {
            ""
        } else {
            r#"<div style="margin-top:8px">Apple Beta App Review pending — the join link appears here as soon as it is approved.</div>"#
        },
    );
    s = s.replace("{{META_MACOS}}", &meta("macos", "Apple silicon · .dmg"));
    s = s.replace("{{META_WINDOWS}}", &meta("windows", "NSIS installer · Windows 10/11"));
    s = s.replace("{{META_LINUX}}", &meta("deb", ".deb — Debian, Ubuntu, Mint, Kali"));
    // 04 · verify: the exact published file name goes into the copy-paste command
    let fname = |key: &str| files.get(key).map(|f| html_escape(&f.name)).unwrap_or_else(|| "<file>".into());
    s = s.replace("{{FILE_MACOS}}", &fname("macos"));
    s = s.replace("{{FILE_APPIMAGE}}", &fname("appimage"));
    s = s.replace("{{FILE_DEB}}", &fname("deb"));
    s = s.replace("{{FILE_WINDOWS}}", &fname("windows"));
    s = s.replace("{{FILE_ANDROID}}", &fname("android"));
    s = s.replace("{{SHA_MACOS}}", &sha("macos"));
    s = s.replace("{{SHA_WINDOWS}}", &sha("windows"));
    s = s.replace("{{SHA_APPIMAGE}}", &sha("appimage"));
    s = s.replace("{{SHA_DEB}}", &sha("deb"));
    // hero caption: just the number ("0.3.2"), or the build label when no manifest is published
    let short = mver.clone().unwrap_or_else(|| "testnet build".into());
    s = s.replace("{{VERSION_SHORT}}", &html_escape(&short));
    // "Testnet build 0.4.6" was right while the whole server was a test server. On mainnet
    // the same label reads as a warning to anyone about to pay real money, so it goes.
    let version = mver
        .map(|v| if testnet_on() { format!("Testnet build {v}") } else { format!("Build {v}") })
        .unwrap_or_else(|| env_or("SITE_VERSION", if testnet_on() { "testnet build" } else { "build" }));
    s = s.replace("{{VERSION}}", &html_escape(&version));
    s = s.replace("{{TESTNET}}", if testnet_on() { "on" } else { "off" });
    s
}

// ---------------------------------------------------------------------------
// HTTP/1.1, minimal: GET / · GET /api/status · GET /api/claim?memo= · POST /api/claim
// ---------------------------------------------------------------------------

struct Req {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
    ip: String,
}

async fn read_request(sock: &mut tokio::net::TcpStream, peer: SocketAddr) -> Option<Req> {
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 2048];
    let head_end = loop {
        let n = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut tmp)).await.ok()?.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        if buf.len() > MAX_HEAD {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_uppercase();
    let target = first.next()?;
    let (path, query) = target.split_once('?').map(|(p, q)| (p.to_string(), q.to_string())).unwrap_or((target.to_string(), String::new()));
    let mut len = 0usize;
    let mut fwd: Option<String> = None;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            if k == "content-length" {
                len = v.parse().unwrap_or(0);
            } else if k == "x-forwarded-for" {
                fwd = v.split(',').next().map(|s| s.trim().to_string());
            }
        }
    }
    if len > MAX_BODY {
        return None;
    }
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut tmp)).await.ok()?.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(len);
    // Behind the proxy the peer is 127.0.0.1; the real client is X-Forwarded-For. Only
    // trust that header when the peer IS the proxy (loopback).
    let ip = match fwd {
        Some(f) if peer.ip().is_loopback() => f,
        _ => peer.ip().to_string(),
    };
    Some(Req { method, path, query, body, ip })
}

fn query_param(q: &str, key: &str) -> Option<String> {
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| urlencoding::decode(v).map(|c| c.into_owned()).unwrap_or_default())
    })
}

async fn respond(sock: &mut tokio::net::TcpStream, status: u16, ctype: &str, body: &[u8]) {
    respond_cached(sock, status, ctype, body, "no-store").await
}

/// Same headers, chosen Cache-Control — the baked-in images may be cached (they change
/// only with a deploy), everything else stays `no-store`.
async fn respond_cached(sock: &mut tokio::net::TcpStream, status: u16, ctype: &str, body: &[u8], cache: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: {cache}\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'self'\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.write_all(body).await;
    let _ = sock.shutdown().await;
}

async fn handle(f: Arc<Faucet>, mut sock: tokio::net::TcpStream, peer: SocketAddr) {
    let Some(req) = read_request(&mut sock, peer).await else {
        respond(&mut sock, 400, "text/plain", b"bad request").await;
        return;
    };
    let json = |v: &Value| serde_json::to_vec(v).unwrap_or_default();
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => respond(&mut sock, 200, "text/html; charset=utf-8", site_html(&f.cfg.dl_dir).as_bytes()).await,
        ("GET", "/health") => respond(&mut sock, 200, "text/plain", b"ok").await,
        ("GET", "/imprint") | ("GET", "/impressum") => respond(&mut sock, 200, "text/html; charset=utf-8", PAGE_IMPRINT.as_bytes()).await,
        ("GET", "/terms") | ("GET", "/agb") => respond(&mut sock, 200, "text/html; charset=utf-8", PAGE_TERMS.as_bytes()).await,
        ("GET", "/privacy") | ("GET", "/datenschutz") => respond(&mut sock, 200, "text/html; charset=utf-8", PAGE_PRIVACY.as_bytes()).await,
        ("GET", "/pay") => respond(&mut sock, 200, "text/html; charset=utf-8", PAGE_PAY.as_bytes()).await,
        ("GET", "/claim") | ("GET", "/redeem") => respond(&mut sock, 200, "text/html; charset=utf-8", PAGE_CLAIM.as_bytes()).await,
        // Mollie's redirect target after a card checkout (see PAID_HTML). Any query string
        // is ignored — nothing on this page depends on it.
        ("GET", "/paid") => respond(&mut sock, 200, "text/html; charset=utf-8", PAID_HTML.as_bytes()).await,
        ("GET", p) if p.starts_with("/img/") => {
            // exact-name lookup in the baked-in list — no filesystem, so no traversal to worry about
            match IMAGES.iter().find(|(n, _)| *n == &p[5..]) {
                Some((_, bytes)) => respond_cached(&mut sock, 200, "image/jpeg", bytes, "public, max-age=86400").await,
                None => respond(&mut sock, 404, "text/plain", b"not found").await,
            }
        }
        ("GET", "/api/status") => respond(&mut sock, 200, "application/json", &json(&f.overview())).await,
        ("GET", "/api/claim") => {
            let memo = query_param(&req.query, "memo").unwrap_or_default();
            respond(&mut sock, 200, "application/json", &json(&f.status(&memo))).await
        }
        ("POST", "/api/claim") => {
            if !faucet_on() {
                respond(&mut sock, 403, "application/json", &json(&json!({"error": "the faucet is switched off"}))).await;
                return;
            }
            let v: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let code = v.get("code").and_then(|c| c.as_str()).unwrap_or("").trim().to_uppercase();
            let memo = v.get("memo").and_then(|c| c.as_str()).unwrap_or("").trim().to_string();
            match f.claim(&code, &memo, &req.ip).await {
                Ok(r) => respond(&mut sock, 200, "application/json", &json(&r)).await,
                Err(e) => {
                    let status = if e.starts_with("too many") { 429 } else { 400 };
                    respond(&mut sock, status, "application/json", &json(&json!({"error": e}))).await
                }
            }
        }
        ("GET", _) => respond(&mut sock, 404, "text/plain", b"not found").await,
        _ => respond(&mut sock, 405, "text/plain", b"method not allowed").await,
    }
}

async fn serve(cfg: Cfg) -> Result<(), String> {
    let addr: SocketAddr = cfg.listen.parse().map_err(|e| format!("FAUCET_LISTEN: {e}"))?;
    if !addr.ip().is_loopback() && env_or("FAUCET_INSECURE_PUBLIC", "0") != "1" {
        return Err(format!(
            "refusing to listen on {addr}: put the faucet behind Caddy/nginx (TLS) on loopback, \
             or set FAUCET_INSECURE_PUBLIC=1 if you really mean plain HTTP"
        ));
    }
    let wallet = Wallet::connect(&cfg)?;
    match (&wallet, testnet_on()) {
        (Some(w), true) => {
            println!("scrai-faucet: TESTNET on · wallet {} · pays to {} · daily max {}", w.address(), cfg.receive, cfg.daily_max);
            println!("scrai-faucet: chain RPC {}", cfg.rpc);
            match w.balance_unym().await {
                Ok(b) => println!("scrai-faucet: wallet balance {:.3} NYM (reserve {:.3})", b as f64 / 1e6, cfg.reserve_unym as f64 / 1e6),
                Err(e) => eprintln!("scrai-faucet: balance check failed ({e}) — claims will fail until the RPC answers"),
            }
        }
        (None, true) => eprintln!("scrai-faucet: TESTNET on but no faucet wallet configured (see .env.example) — site only, claims refused"),
        (_, false) => println!("scrai-faucet: testnet OFF (TESTNET unset) — serving downloads only, faucet hidden"),
    }
    let faucet = Arc::new(Faucet {
        cfg,
        wallet,
        claim_lock: tokio::sync::Mutex::new(()),
        attempts: Mutex::new(HashMap::new()),
        salt: rand::random(),
    });
    let listener = TcpListener::bind(addr).await.map_err(|e| format!("bind {addr}: {e}"))?;
    println!("scrai-faucet: listening on http://{addr}");
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => continue,
        };
        let f = faucet.clone();
        tokio::spawn(async move { handle(f, sock, peer).await });
    }
}

fn cli(cfg: &Cfg, args: &[String]) -> Result<(), String> {
    let db = open_faucet_db(&cfg.faucet_db())?;
    match args.first().map(String::as_str) {
        Some("code") => match args.get(1).map(String::as_str) {
            Some("new") => {
                let uses: u32 = args.get(2).and_then(|u| u.parse().ok()).unwrap_or(DEFAULT_CODE_USES);
                let note = args.get(3).cloned().unwrap_or_default();
                let code = mint(&db, uses, &note)?;
                println!("{code}   ({uses} use{}{})", if uses == 1 { "" } else { "s" }, if note.is_empty() { String::new() } else { format!(" · {note}") });
                Ok(())
            }
            Some("list") => {
                println!("{:<18} {:>5}  note", "code", "left");
                for r in list_codes(&db)? {
                    println!("{:<18} {:>5}  {}", r.code, r.left(), r.note);
                }
                Ok(())
            }
            _ => Err("usage: scrai-faucet code new [uses] [note] | code list".into()),
        },
        Some("claims") => {
            let mut st = db.prepare("SELECT ts, memo, code, unym, stage, tx FROM claims ORDER BY ts DESC LIMIT 200").map_err(|e| e.to_string())?;
            let rows = st
                .query_map([], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?))
                })
                .map_err(|e| e.to_string())?;
            println!("{:<12} {:<14} {:<16} {:>9}  {:<8} tx", "ts", "memo", "code", "NYM", "stage");
            for r in rows.flatten() {
                println!("{:<12} {:<14} {:<16} {:>9.3}  {:<8} {}", r.0, r.1, r.2, r.3 as f64 / 1e6, r.4, r.5);
            }
            Ok(())
        }
        _ => Err("usage: scrai-faucet [code new [uses] [note] | code list | claims]".into()),
    }
}

#[tokio::main]
async fn main() {
    let cfg = Cfg::load();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = if args.is_empty() { serve(cfg).await } else { cli(&cfg, &args) };
    if let Err(e) = result {
        eprintln!("scrai-faucet: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::rpc_from_lcd;

    #[test]
    fn rpc_is_the_lcd_root() {
        assert_eq!(rpc_from_lcd(Some("https://validator-sandbox-1.nymtech.net/api")), "https://validator-sandbox-1.nymtech.net");
        assert_eq!(rpc_from_lcd(Some("https://validator-sandbox-1.nymtech.net/api/")), "https://validator-sandbox-1.nymtech.net");
        assert_eq!(rpc_from_lcd(Some("https://api.nymtech.net")), "https://api.nymtech.net");
        assert_eq!(rpc_from_lcd(None), "");
    }
}
