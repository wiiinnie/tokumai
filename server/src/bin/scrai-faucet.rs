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
/// Legal pages a card processor's onboarding checks for (imprint, terms, privacy) — static, no
/// placeholders, served as-is.
const PAGE_IMPRINT: &str = include_str!("../../site/imprint.html");
const PAGE_TERMS: &str = include_str!("../../site/terms.html");
const PAGE_PRIVACY: &str = include_str!("../../site/privacy.html");
/// Hand-over page for a top-up started in the app (Apple's IAP gate: the purchase is
/// raised and signed in the app, the payment itself happens here in the browser). The
/// invoice rides in the URL FRAGMENT, so it never reaches this server — nothing to log,
/// nothing to store, and this route serves one static file to everyone.
const PAGE_PAY: &str = include_str!("../../site/pay.html");
/// The five official card/wallet method marks, lifted out of index.html so the rail tiles can be
/// generated without a wall of SVG inside Rust. Originals in docs/brand/.
const CARD_MARKS: &str = include_str!("../../site/cardmarks.html");
/// Where a tester redeems an invite code. The app links here with the code and the memo
/// in the URL fragment, so neither reaches this server until the button is pressed.
const PAGE_CLAIM: &str = include_str!("../../site/claim.html");
/// The site's screenshots, baked into the binary so a deploy ships them (Caddy only knows
/// /dl/; nothing else to upload or configure). Served as GET /img/<name>.
/// Where Stripe's hosted checkout sends the browser afterwards (`STRIPE_REDIRECT_URL`
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
<title>Card checkout — tokumai</title>
<style>
  :root{--ink:#141210;--surface:#1C1917;--surface-2:#262220;--line:#332E2A;--bone:#ECE6DC;--muted:#9C938A;--signal:#CBA14E;--acc:#7A5FFF;
    --mono:ui-monospace,SFMono-Regular,Menlo,monospace;--body:system-ui,-apple-system,'Hanken Grotesk',sans-serif;--display:Georgia,'Fraunces',serif}
  *{box-sizing:border-box}
  html,body{margin:0;background:var(--ink);color:var(--bone);font-family:var(--body);-webkit-font-smoothing:antialiased;min-height:100%}
  .wrap{max-width:520px;margin:0 auto;padding:64px 22px}
  .logo{font-family:var(--display);font-weight:700;font-size:22px;margin-bottom:38px}.logo b{color:var(--acc)}
  .card{border:1px solid var(--line);border-radius:16px;background:var(--surface);padding:26px 24px}
  .eyebrow{font-family:var(--mono);font-size:11.5px;letter-spacing:.12em;color:var(--acc);text-transform:uppercase;margin-bottom:10px}
  h1{font-family:var(--display);font-weight:700;font-size:30px;line-height:1.1;margin:0 0 14px}
  p{font-size:15px;line-height:1.55;color:var(--muted);margin:0 0 12px}
  p b{color:var(--bone)}
  .fine{font-family:var(--mono);font-size:11.5px;color:var(--muted);margin-top:22px;line-height:1.5}
</style>
</head>
<body>
<div class="wrap">
  <div class="logo">tokum<b>ai</b></div>
  <div class="card">
    <div class="eyebrow">Card checkout</div>
    <h1>You can close this tab</h1>
    <p><b><b>Bought a code on tokumai.com?</b> Go back to the tab you ordered from: the code appears there by itself, usually within seconds. <b>Paid from the app?</b> The app takes it from here.</b> If the payment went through, it is picked up on its own and your credit is collected — usually within a few seconds, no further steps.</p>
    <p>If you cancelled, or the payment failed, nothing was charged; pick an amount in the app again.</p>
    <div class="fine">This page holds no order details and sets no cookie. Once collected, the credit is unlinkable to this payment. If a payment went through and no credit arrived, quote your receipt number and we will credit you.</div>
  </div>
</div>
</body>
</html>
"##;

// The five real device captures (2026-09-06) exist in ONE theme — the app's dark one.
// Rather than ship a copy of each under a second name, the light entry points at the same
// bytes: the page asks for `<name>-light.jpg` in light mode and gets the dark screenshot,
// which is what a product shot of a dark app looks like anyway. Drop the alias and add a
// real file the day someone captures the light theme.
const SHOT_HERO: &[u8] = include_bytes!("../../site/img/hero-imagegen-dark.jpg");
const SHOT_PH_IMAGE: &[u8] = include_bytes!("../../site/img/how-phone-image-dark.jpg");
const SHOT_PH_NETWORK: &[u8] = include_bytes!("../../site/img/how-phone-network-dark.jpg");
const SHOT_PH_START: &[u8] = include_bytes!("../../site/img/how-phone-start-dark.jpg");
const SHOT_FLOW_READY: &[u8] = include_bytes!("../../site/img/flow-ready-dark.jpg");

const SHOT_HERO_APP: &[u8] = include_bytes!("../../site/img/hero-app-imagegen-dark.jpg");
const SHOT_HOW_IDENTITY: &[u8] = include_bytes!("../../site/img/how-identity-dark.jpg");
const SHOT_HOW_ROUTE: &[u8] = include_bytes!("../../site/img/how-route-dark.jpg");
const SHOT_HOW_PAYMENT: &[u8] = include_bytes!("../../site/img/how-payment-dark.jpg");
const SHOT_HOW_GUARD: &[u8] = include_bytes!("../../site/img/how-guard-dark.jpg");

const IMAGES: &[(&str, &[u8])] = &[
    ("hero-app-imagegen-dark.jpg", SHOT_HERO_APP),
    ("hero-app-imagegen-light.jpg", SHOT_HERO_APP),
    ("how-identity-dark.jpg", SHOT_HOW_IDENTITY),
    ("how-identity-light.jpg", SHOT_HOW_IDENTITY),
    ("how-route-dark.jpg", SHOT_HOW_ROUTE),
    ("how-route-light.jpg", SHOT_HOW_ROUTE),
    ("how-payment-dark.jpg", SHOT_HOW_PAYMENT),
    ("how-payment-light.jpg", SHOT_HOW_PAYMENT),
    ("how-guard-dark.jpg", SHOT_HOW_GUARD),
    ("how-guard-light.jpg", SHOT_HOW_GUARD),
    ("flow-account-dark.jpg", include_bytes!("../../site/img/flow-account-dark.jpg")),
    ("flow-account-light.jpg", include_bytes!("../../site/img/flow-account-light.jpg")),
    ("flow-ready-dark.jpg", SHOT_FLOW_READY),
    ("flow-ready-light.jpg", SHOT_FLOW_READY),
    ("flow-topup-dark.jpg", include_bytes!("../../site/img/flow-topup-dark.jpg")),
    ("flow-topup-light.jpg", include_bytes!("../../site/img/flow-topup-light.jpg")),
    ("hero-imagegen-dark.jpg", SHOT_HERO),
    ("hero-imagegen-light.jpg", SHOT_HERO),
    ("how-mac-chat-dark.jpg", include_bytes!("../../site/img/how-mac-chat-dark.jpg")),
    ("how-mac-chat-light.jpg", include_bytes!("../../site/img/how-mac-chat-light.jpg")),
    ("how-phone-chat-dark.jpg", include_bytes!("../../site/img/how-phone-chat-dark.jpg")),
    ("how-phone-chat-light.jpg", include_bytes!("../../site/img/how-phone-chat-light.jpg")),
    ("how-phone-image-dark.jpg", SHOT_PH_IMAGE),
    ("how-phone-image-light.jpg", SHOT_PH_IMAGE),
    ("how-phone-network-dark.jpg", SHOT_PH_NETWORK),
    ("how-phone-network-light.jpg", SHOT_PH_NETWORK),
    ("how-phone-start-dark.jpg", SHOT_PH_START),
    ("how-phone-start-light.jpg", SHOT_PH_START),
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
    // Sandbox: one host serves both, REST under /api and the RPC at the root.
    if let Some(root) = l.strip_suffix("/api") {
        return root.to_string();
    }
    // Mainnet: api.nymtech.net is REST ONLY — a Tendermint call there answers
    // "501 Not Implemented", which is what a claim died on (2026-09-05). The RPC lives on
    // the sibling rpc.<host>. FAUCET_RPC overrides this in either direction.
    if let Some((scheme, rest)) = l.split_once("://") {
        if let Some(host) = rest.strip_prefix("api.") {
            return format!("{scheme}://rpc.{host}");
        }
    }
    l.to_string()
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

// ---- web orders and vouchers -------------------------------------------------------
//
// The faucet serves /pay on the clearnet; the payment rails live in the server, which has
// no clearnet port. The two talk through tables in state.db (see docs/vouchers.md), so
// these are the only places the faucet opens that database for WRITING.

/// The "Available payment options" tiles.
///
/// Read from the SERVER's own configuration rather than written into the page by hand — the
/// card tile claimed "not available yet" for a day after the card rail went live, because a static
/// page cannot know. Both units run with the same working directory and load the same .env,
/// so asking `pay` here gives exactly the answer the server would give.
/// The amounts on sale, each with its card price and — when the server discounts coins —
/// its coin price. From the server's own tiers and percentage, so the page can never say
/// a price the invoice will not charge.
/// Can this server take a coin payment at all? Both coin rails, as one question — the
/// price tiles and the page copy both hinge on it.
fn coins_sellable() -> bool {
    scrai_server::nyx::Nyx::from_env().is_some() || scrai_server::pay::coin_rail_ready()
}

fn prices_html() -> String {
    let pct = scrai_server::pay::coin_discount_pct();
    let mut out = String::from("<div class=\"prices\">");
    for usd in scrai_server::pay::purchase_tiers() {
        let toku = (usd as u64 * 100_000).to_string();
        let toku = toku
            .as_bytes()
            .rchunks(3)
            .rev()
            .map(|c| std::str::from_utf8(c).unwrap_or(""))
            .collect::<Vec<_>>()
            .join(",");
        // …and only when a coin rail is actually live: the discount priced a way to pay,
        // and with coins withdrawn the line advertised a price nobody can get.
        let coin = if pct > 0 && coins_sellable() {
            let cents = scrai_server::pay::charged_cents(usd, "nyx", false);
            format!("<small class=\"cr\">${}.{:02} with NYM or Bitcoin</small>", cents / 100, cents % 100)
        } else {
            String::new()
        };
        out.push_str(&format!("<div class=\"pr\"><b>${usd}</b><small>{toku} TOKU</small>{coin}</div>"));
    }
    out.push_str("</div>");
    out
}

/// The shop page, with its placeholders actually filled.
///
/// It used to be served straight from the template: `{{COIN_DISCOUNT_PCT}}` and
/// `{{CARD_MIN_USD}}` were never substituted, and the page's `Number(…) || 0` fallbacks
/// hid it — the coin discount simply never appeared there, and the card minimum was right
/// only because the fallback happened to match. `{{RAILS}}` is new and is what lets the
/// page offer exactly the rails this server can serve.
fn pay_html() -> String {
    let rails: Vec<&str> = [
        ("nyx", scrai_server::nyx::Nyx::from_env().is_some()),
        ("card", scrai_server::pay::card_enabled()),
        ("btc", scrai_server::pay::coin_rail_ready()),
    ]
    .into_iter()
    .filter_map(|(name, on)| on.then_some(name))
    .collect();
    PAGE_PAY
        .replace("{{COIN_DISCOUNT_PCT}}", &scrai_server::pay::coin_discount_pct().to_string())
        .replace("{{CARD_MIN_USD}}", &scrai_server::pay::card_min_usd().to_string())
        .replace("{{RAILS}}", &rails.join(","))
}

fn rails_html() -> String {
    // Only what can actually be bought with, TODAY. This used to render every rail and
    // label the unavailable ones "not yet", which was honest while a rail was genuinely on
    // the way — Monero waiting on our BTCPay store. It stopped being honest on 2026-09-16,
    // when coin payments were withdrawn for tax reasons with no date to come back: "not
    // yet" would then be a promise nobody has made. A rail that cannot be used is simply
    // not offered, and if one returns it appears again on its own.
    let dot = |cls: &str, glyph: &str| {
        format!(
            "<svg class=\"coin {cls}\" viewBox=\"0 0 32 32\" width=\"28\" height=\"28\"><circle cx=\"16\" cy=\"16\" r=\"15\" fill=\"currentColor\"></circle><text x=\"16\" y=\"22\" text-anchor=\"middle\" font-family=\"JetBrains Mono, monospace\" font-size=\"15\" font-weight=\"700\" fill=\"#141210\">{glyph}</text></svg>"
        )
    };
    let tile = |mark: String, name: &str, tag: &str| {
        format!("<div class=\"pm on\">{mark}<span class=\"pmname\">{name}</span><span class=\"pmtag\">{tag}</span></div>")
    };
    let pct = scrai_server::pay::coin_discount_pct();
    let coin_tag = if pct > 0 {
        format!("available - from ${} - {pct}% less", scrai_server::pay::coin_min_usd())
    } else {
        format!("available - from ${}", scrai_server::pay::coin_min_usd())
    };

    let mut out = String::from("<div class=\"pms\" id=\"rails\">");
    if scrai_server::nyx::Nyx::from_env().is_some() {
        let tag = if pct > 0 { format!("native - Nyx - {pct}% less") } else { "native - Nyx".to_string() };
        out.push_str(&tile(dot("nym", "N"), "NYM", &tag));
    }
    if scrai_server::pay::card_enabled() {
        let card_tag = format!("available - from ${}", scrai_server::pay::card_min_usd());
        out.push_str(&tile(
            format!("<span class=\"cardmarks\">{CARD_MARKS}</span>"),
            "Card, PayPal, Apple Pay, Google Pay",
            &card_tag,
        ));
    }
    if scrai_server::pay::coin_rail_ready() {
        out.push_str(&tile(dot("btc", "B"), "Bitcoin", &coin_tag));
    }
    // Nothing live at all — the state this server is in between payment processors. An
    // empty block under "Available payment options" reads as a broken page rather than as
    // a situation, so say what it is. No date: we do not have one to give.
    if out == "<div class=\"pms\" id=\"rails\">" {
        return String::from(
            "<p class=\"note\">No payment method is switched on at the moment. \
             Credit from a code can be redeemed in the app at any time, and on iPhone \
             credit can be bought through the App Store.</p>",
        );
    }
    out.push_str("</div>");
    out
}

fn state_rw(state_db: &Path) -> Result<Connection, String> {
    let c = Connection::open_with_flags(state_db, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("state.db: {e}"))?;
    // The server holds this file open and writes on its ticks; wait for it instead of failing
    // a buyer's order with "database is locked" (2026-09-11).
    c.busy_timeout(std::time::Duration::from_secs(5)).map_err(|e| format!("state.db: {e}"))?;
    Ok(c)
}

/// Book an order. The server raises the invoice on its next tick.
fn web_order_new(state_db: &Path, id: &str, usd: u32, method: &str, consent: &str) -> Result<(), String> {
    let now = scrai_server::pay::now_ms() as i64;
    state_rw(state_db)?
        .execute(
            "INSERT INTO web_orders (id, usd, method, consent, created_at) VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![id, usd as i64, method, consent, now],
        )
        .map(|_| ())
        .map_err(|e| format!("could not book the order: {e}"))
}

/// (invoice, pay_json, paid, error)
fn web_order(state_db: &Path, id: &str) -> Option<(Option<String>, Option<String>, bool, Option<String>)> {
    let conn = Connection::open_with_flags(
        state_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.busy_timeout(std::time::Duration::from_secs(5)).ok()?;
    conn.query_row(
        "SELECT invoice, pay_json, paid_at, error FROM web_orders WHERE id = ?1",
        rusqlite::params![id],
        |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<i64>>(2)?.is_some(),
                r.get::<_, Option<String>>(3)?,
            ))
        },
    )
    .ok()
}

/// Mint the code for a paid order — HERE, not in the server, so no process that holds the
/// mint ever holds a code. It is shown exactly once: only the hash is stored, and the
/// UNIQUE index on `invoice` is what makes a second attempt fail rather than mint again.
/// Mint the code for a settled order — or hand back the one already minted.
///
/// The second half is the point. This used to return the plaintext exactly once and keep
/// only its fingerprint, so a reply lost between here and the browser destroyed a paid
/// buyer's credit with no way back: the UNIQUE index blocks a replacement mint, and voiding
/// leaves the row in place, so not even direct SQL was a clean fix. Now the plaintext is
/// held on the order row for `CODE_HOLD_MS` and returned on every call in that window — a
/// dropped response becomes a retry, and a buyer who closed the tab can reopen `#order=…`.
/// It is dropped the moment they confirm they have written it down (`/api/order/ack`), and
/// swept unconditionally after the window whether they confirm or not.
/// Ok((code, fresh)): `fresh` is false on the retry path, where the held code is shown again.
fn mint_voucher(state_db: &Path, order: &str, invoice: &str, toku: u64) -> Result<(String, bool), String> {
    let now = scrai_server::pay::now_ms();
    let conn = state_rw(state_db)?;

    // Already minted and still held: the same code, not an error. This is the retry path.
    let held: Option<String> = conn
        .query_row(
            "SELECT code FROM web_orders WHERE id = ?1 AND code IS NOT NULL AND code_at >= ?2",
            rusqlite::params![order, (now.saturating_sub(30 * 60_000)) as i64],
            |r| r.get::<_, String>(0),
        )
        .ok();
    if let Some(code) = held {
        return Ok((code, false));
    }

    // Issued before, and the plaintext is gone: never a second code for the same money. The
    // UNIQUE index on `vouchers.invoice` used to be this guard; it stops knowing the order
    // once the purchase link expires (14 days), so the order row remembers on its own.
    if scrai_server::store::web_order_code_issued(state_db, order) {
        return Err("this code has already been shown and confirmed, or its display window has \
                    closed. We keep only a fingerprint, so it cannot be shown again — if you never \
                    received it, contact us with your receipt number".into());
    }
    // Fail closed rather than mint under the unkeyed fingerprint: a `vouchers` table whose
    // rows can be brute-forced out of a backup is exactly what the key exists to prevent.
    if scrai_server::pay::voucher_key().is_none() {
        return Err("code purchases are not configured on this server (VOUCHER_KEY)".into());
    }
    let code = scrai_server::pay::new_voucher_code();
    let hash = scrai_server::pay::voucher_hash(&code);
    let n = conn
        .execute(
            "INSERT OR IGNORE INTO vouchers (hash, toku, invoice, created_at) VALUES (?1,?2,?3,?4)",
            rusqlite::params![hash, toku as i64, invoice, now as i64],
        )
        .map_err(|e| format!("could not issue the code: {e}"))?;
    if n == 0 {
        // A code exists for this invoice but is no longer held — the window has passed and
        // the plaintext is gone for good. Say so plainly; there is nothing to retry.
        return Err("this code has already been shown and confirmed, or its display window has \
                    closed. We keep only a fingerprint, so it cannot be shown again — if you never \
                    received it, contact us with your receipt number".into());
    }
    conn.execute(
        "UPDATE web_orders SET code = ?2, code_at = ?3 WHERE id = ?1",
        rusqlite::params![order, code, now as i64],
    )
    .map_err(|e| format!("could not hold the code: {e}"))?;
    Ok((code, true))
}

fn server_invite_invoices(state_db: &Path) -> Result<Vec<TestnetInv>, String> {
    let conn = Connection::open_with_flags(state_db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("state.db: {e}"))?;
    conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(|e| format!("state.db: {e}"))?;
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
    /// (UTC day, salt) for the day's visitor fingerprints — a fresh salt every day and on
    /// every boot, never persisted, so yesterday's rows cannot be joined to anyone
    stats_salt: Mutex<(String, u64)>,
}

const BUCKET_CLAIM: u64 = 1;
const BUCKET_ORDER: u64 = 2;
/// Orders per hour per IP. A buyer makes one; somebody buying a few codes as gifts makes a
/// handful. Anything past this is not a customer — and every accepted order becomes a REAL
/// gateway call on the next tick (a Stripe checkout session, an address and memo), on a path
/// that has no account to throttle and does not go through `admit_invoice`, so the
/// server-wide invoice brake never saw it either (audit 2026-09-08, M4).
const ORDERS_PER_HOUR: usize = 8;

fn valid_code(s: &str) -> bool {
    (8..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
}
fn valid_memo(s: &str) -> bool {
    (4..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl Faucet {
    fn ip_ok(&self, ip: &str) -> bool {
        self.ip_ok_for(BUCKET_CLAIM, ip, IP_ATTEMPTS_PER_HOUR)
    }

    /// Per-hour budget for one hashed client IP in one bucket. Separate buckets on purpose:
    /// somebody buying codes must not spend the budget a tester needs to claim an invite,
    /// and neither must be able to exhaust the other's.
    ///
    /// The IP is only ever hashed, with a salt made at boot and never persisted — so this
    /// limits without keeping a record of who was here.
    fn ip_ok_for(&self, bucket: u64, ip: &str, limit: usize) -> bool {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.salt.hash(&mut h);
        bucket.hash(&mut h);
        ip.hash(&mut h);
        let key = h.finish();
        let mut map = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        let cutoff = Instant::now() - Duration::from_secs(3600);
        let v = map.entry(key).or_default();
        v.retain(|t| *t > cutoff);
        if v.len() >= limit {
            return false;
        }
        v.push(Instant::now());
        if map.len() > 10_000 {
            map.clear(); // never let the map grow unbounded; a flood just resets everyone's window
        }
        true
    }

    /// The day's visitor fingerprint: sha256(day salt ‖ ip ‖ user agent), 16 hex chars. The
    /// salt lives only in memory and changes daily, so the stored value identifies nobody —
    /// it can only say "seen today already".
    fn visitor(&self, day: &str, req: &Req) -> String {
        use sha2::{Digest, Sha256};
        let salt = {
            let mut g = self.stats_salt.lock().unwrap_or_else(|e| e.into_inner());
            if g.0 != day {
                *g = (day.to_string(), rand::random());
            }
            g.1
        };
        let mut h = Sha256::new();
        h.update(salt.to_le_bytes());
        h.update(req.ip.as_bytes());
        h.update(b"\0");
        h.update(req.ua.as_bytes());
        hex::encode(&h.finalize()[..8])
    }

    /// Counts one event for the admin console — `view:<page>`, `dl:<platform>`, `order`,
    /// `order:<method>`, `code`, `claim`. With a request it also marks the day's visitor
    /// (page views only; a download or an order comes from someone already counted).
    /// Crawlers are skipped for views so "unique visitors" means people.
    /// Off the request path: a slow disk must never delay a page.
    fn track(self: &Arc<Self>, key: &str, visitor_of: Option<&Req>) {
        if let Some(r) = visitor_of {
            if is_crawler(&r.ua) {
                return;
            }
        }
        let day = utc_day();
        let h = visitor_of.map(|r| self.visitor(&day, r));
        let key = key.to_string();
        let db = self.cfg.state_db();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = web_stats_bump(&db, &day, &key, h.as_deref()) {
                eprintln!("scrai-faucet: web stats: {e}");
            }
        });
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
            // Operator signal: the cap is FAUCET_DAILY_MAX in /opt/tokumai/.env (restart
            // tokumai-faucet after raising it — the faucet reads its config once, at boot).
            // tokumai-admin shows the same count in red.
            eprintln!(
                "scrai-faucet: DAILY LIMIT reached — {} claims today, max {} (raise FAUCET_DAILY_MAX in /opt/tokumai/.env, then: systemctl restart tokumai-faucet) — refused memo {memo} code {code}",
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

/// Is this env value a link we may put on the site?
///
/// `starts_with("https://")` alone was not enough: the .env.example placeholder
/// `https://testflight.apple.com/join/<code>` passes that test, so a copied-but-unfilled
/// line put a button on the download page that led nowhere — live for a day before anyone
/// noticed (2026-09-08). Angle brackets and whitespace are exactly what an unfilled
/// placeholder looks like and neither belongs in a URL, so both are refused. The card then
/// falls back to "not published yet", which is at least true.
fn publishable_link(raw: &str) -> bool {
    let u = raw.trim();
    u.starts_with("https://")
        && u.len() > "https://".len()
        && !u.contains(['<', '>'])
        && !u.chars().any(char::is_whitespace)
}

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

/// One URL per topic (2026-09-11). The template carries every page as a
/// `<!--@page:ID-->…<!--@end-->` block; a request keeps its own block and drops the rest,
/// so each page is a complete document with its own title, description and canonical.
const PAGES: &[(&str, &str, &str, &str)] = &[
    ("home", "/", "tokumai — Private AI chat. No identity attached.",
     "Ask leading AI models anything over the Nym mixnet. No e-mail, no phone number, no IP, no traceable payment: nothing your questions can be tied to."),
    ("how", "/how-it-works", "How tokumai works: AI chat with no identity, no IP, no traceable payment",
     "Four things that never meet: your identity, your IP address, your payment and your questions. How the Nym mixnet, blind-signed coins and an on-device guard keep them apart."),
    ("pricing", "/pricing", "tokumai pricing: prepaid AI credit, no subscription",
     "Buy $5 to $50 of TOKU credit once by card or through the App Store. A text answer costs a fraction of a cent; there is no monthly plan."),
    ("download", "/download", "Download tokumai for macOS, Windows, Linux, Android and iPhone",
     "Native apps for every platform. Desktop builds are direct downloads with checksums; iPhone through TestFlight; Android as an .apk."),
    ("compare", "/compare", "tokumai compared with other private AI chats",
     "How tokumai differs from Duck.ai, Venice, nilGPT, Lumo and Brave Leo: who sees your IP, whether a payment can be linked to a prompt, and what is a promise versus a design."),
    ("vs-duck-ai", "/vs/duck-ai", "tokumai vs Duck.ai: Private AI Chat Compared (2026)",
     "Duck.ai promises not to store your IP. tokumai never receives it. An honest comparison of two private AI chats: privacy model, pricing, apps and features."),
];

fn page_for_path(path: &str) -> Option<&'static str> {
    PAGES.iter().find(|(_, p, _, _)| *p == path).map(|(id, _, _, _)| *id)
}

/// Keep `page`'s block, drop the other blocks, fill the head. Unknown page → home.
fn select_page(html: String, page: &str) -> String {
    let (id, path, title, desc) = PAGES.iter().find(|(id, _, _, _)| *id == page).copied().unwrap_or(PAGES[0]);
    let mut out = String::with_capacity(html.len());
    let mut rest = html.as_str();
    while let Some(start) = rest.find("<!--@page:") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "<!--@page:".len()..];
        let Some(name_end) = after.find("-->") else { break };
        let name = &after[..name_end];
        let block_start = &after[name_end + 3..];
        let Some(end) = block_start.find("<!--@end-->") else { break };
        if name == id {
            out.push_str(&block_start[..end]);
        }
        rest = &block_start[end + "<!--@end-->".len()..];
    }
    out.push_str(rest);
    out.replace("{{PAGE}}", id)
        .replace("{{CANON}}", path)
        .replace("{{TITLE}}", &html_escape(title))
        .replace("{{DESC}}", &html_escape(desc))
}

fn sitemap_xml() -> String {
    let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    for (_, p, _, _) in PAGES {
        x.push_str(&format!("  <url><loc>https://tokumai.com{p}</loc></url>\n"));
    }
    for p in ["/pay", "/terms", "/privacy", "/imprint"] {
        x.push_str(&format!("  <url><loc>https://tokumai.com{p}</loc></url>\n"));
    }
    x.push_str("</urlset>\n");
    x
}

fn site_page(dl_dir: &Path, page: &str) -> String {
    let env_link = |var: &str| std::env::var(var).ok().filter(|u| publishable_link(u)).map(|u| html_escape(u.trim()));
    let (mver, files) = read_manifest(dl_dir);
    let mut s = SITE.to_string();
    s = s.replace("{{RAILS}}", &rails_html());
    s = s.replace("{{PRICES}}", &prices_html());
    s = s.replace("{{COIN_DISCOUNT_PCT}}", &scrai_server::pay::coin_discount_pct().to_string());
    s = s.replace("{{CARD_MIN_USD}}", &scrai_server::pay::card_min_usd().to_string());
    // The macOS buy-sheet capture, when one exists. No drawn placeholder: every other picture
    // on this page is a real screenshot, and a fake would show.
    s = s.replace(
        "{{SHOT_MAC_BUY}}",
        if IMAGES.iter().any(|(n, _)| *n == "how-mac-buy-dark.jpg") {
            "<button class=\"shot\" type=\"button\" data-full=\"/img/how-mac-buy\" aria-label=\"Enlarge: buying credit in the macOS app\"><img src=\"/img/how-mac-buy-dark.jpg\" alt=\"Buy credit sheet in the macOS app\" loading=\"lazy\"></button>"
        } else {
            ""
        },
    );
    let off = |label: &str| format!(r#"<span class="btn off">{label} · not published yet</span>"#);
    // Bundles we host ourselves: from the manifest (relative /dl/ link on this very host).
    for (ph, key, label, primary) in [
        ("{{DL_MACOS}}", "macos", "Download .dmg", true),
        ("{{DL_WINDOWS}}", "windows", "Download installer (.exe)", true),
        // .deb is the STANDARD Linux download (host WebKit/GTK — robust); the AppImage
        // stays offered but experimental (bundled libs clash with newer stacks, see the
        // 2026-08-31 Kali report: grey window from a gvfs/EGL collision).
        ("{{DL_DEB}}", "deb", "Download .deb", true),
        // Label kept short so both Linux buttons sit on ONE line — the card grid gives every
        // card in a row the same button row, so a wrapped second button would make all of them
        // taller. "experimental" moved into the card text and the notes overlay.
        ("{{DL_APPIMAGE}}", "appimage", "AppImage", false),
        ("{{DL_ANDROID}}", "android", "Download .apk", true),
    ] {
        let cls = if primary { "btn primary" } else { "btn" };
        let html = match files.get(key) {
            Some(f) => format!(r#"<a class="{cls}" href="/get/{}">{label}</a>"#, html_escape(&f.name)),
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
            // the join link is served as a redirect so the click can be counted (see /go/ios)
            Some(_) if var == "DL_IOS" => format!(r#"<a class="{cls}" href="/go/ios">{label}</a>"#),
            Some(u) => format!(r#"<a class="{cls}" href="{u}">{label}</a>"#),
            // The guide is optional: with no link there is nothing to say, and a dead
            // "not published yet" button would wrap the mobile row onto a second line —
            // which the card grid then charges to every card beside it.
            None if !primary => String::new(),
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
    select_page(s, page)
}

// ---------------------------------------------------------------------------
// HTTP/1.1, minimal: GET / · GET /api/status · GET /api/claim?memo= · POST /api/claim
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Website statistics for the admin console — counters per UTC day, nothing per person.
// `web_stats(day, key, n)` and `web_uniques(day, h)`; the latter is pruned after
// WEB_UNIQUES_DAYS so even the salted fingerprints do not pile up.
// ---------------------------------------------------------------------------

const WEB_UNIQUES_DAYS: i64 = 35;

fn utc_day() -> String {
    scrai_server::admin::civil_day_utc((scrai_server::pay::now_ms() / 1000) as i64)
}

fn is_crawler(ua: &str) -> bool {
    let u = ua.to_ascii_lowercase();
    u.is_empty()
        || ["bot", "crawl", "spider", "slurp", "curl/", "wget/", "python-", "go-http", "headless", "preview", "facebookexternalhit"]
            .iter()
            .any(|m| u.contains(m))
}

fn web_stats_init(state_db: &Path) -> Result<(), String> {
    let conn = state_rw(state_db)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS web_stats (day TEXT NOT NULL, key TEXT NOT NULL, n INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (day, key));\n\
         CREATE TABLE IF NOT EXISTS web_uniques (day TEXT NOT NULL, h TEXT NOT NULL, PRIMARY KEY (day, h));",
    )
    .map_err(|e| format!("web stats tables: {e}"))?;
    let cutoff = scrai_server::admin::civil_day_utc((scrai_server::pay::now_ms() / 1000) as i64 - WEB_UNIQUES_DAYS * 86_400);
    conn.execute("DELETE FROM web_uniques WHERE day < ?1", [cutoff]).map_err(|e| format!("web stats prune: {e}"))?;
    Ok(())
}

fn web_stats_bump(state_db: &Path, day: &str, key: &str, visitor: Option<&str>) -> Result<(), String> {
    let conn = state_rw(state_db)?;
    conn.execute(
        "INSERT INTO web_stats (day, key, n) VALUES (?1, ?2, 1) ON CONFLICT(day, key) DO UPDATE SET n = n + 1",
        params![day, key],
    )
    .map_err(|e| e.to_string())?;
    if let Some(h) = visitor {
        conn.execute("INSERT OR IGNORE INTO web_uniques (day, h) VALUES (?1, ?2)", params![day, h]).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Which bundle a published file name is (manifest key), by its extension.
fn dl_platform(name: &str) -> &'static str {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".dmg") { "macos" }
    else if n.ends_with(".exe") || n.ends_with(".msi") { "windows" }
    else if n.ends_with(".deb") { "deb" }
    else if n.ends_with(".appimage") { "appimage" }
    else if n.ends_with(".apk") { "android" }
    else { "other" }
}

struct Req {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
    ip: String,
    /// only ever hashed into the day's visitor fingerprint (see `Faucet::track`)
    ua: String,
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
    let mut ua = String::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            if k == "content-length" {
                len = v.parse().unwrap_or(0);
            } else if k == "user-agent" {
                ua = v.chars().take(200).collect();
            } else if k == "x-forwarded-for" {
                // The LAST element, not the first. Caddy APPENDS the peer it saw to
                // whatever the client sent, so `X-Forwarded-For: 9.9.9.9` arrives as
                // "9.9.9.9, <real ip>" — reading the first entry let any caller pick
                // its own rate-limit bucket and hand itself unlimited /api/claim
                // attempts (audit 2026-09-06). The last entry is the one OUR proxy wrote.
                fwd = v.rsplit(',').next().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
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
    Some(Req { method, path, query, body, ip, ua })
}

fn query_param(q: &str, key: &str) -> Option<String> {
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| urlencoding::decode(v).map(|c| c.into_owned()).unwrap_or_default())
    })
}

/// 302 with the same hardening headers as every other reply.
async fn redirect(sock: &mut tokio::net::TcpStream, location: &str) {
    let head = format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n"
    );
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.shutdown().await;
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
        ("GET", "/index.html") => {
            f.track("view:home", Some(&req));
            respond(&mut sock, 200, "text/html; charset=utf-8", site_page(&f.cfg.dl_dir, "home").as_bytes()).await
        }
        ("GET", p) if page_for_path(p).is_some() => {
            f.track(&format!("view:{}", page_for_path(p).unwrap_or("home")), Some(&req));
            let page = page_for_path(p).unwrap_or("home");
            respond(&mut sock, 200, "text/html; charset=utf-8", site_page(&f.cfg.dl_dir, page).as_bytes()).await
        }
        ("GET", "/sitemap.xml") => respond(&mut sock, 200, "application/xml; charset=utf-8", sitemap_xml().as_bytes()).await,
        ("GET", "/robots.txt") => respond(&mut sock, 200, "text/plain; charset=utf-8", b"User-agent: *\nAllow: /\nDisallow: /api/\nDisallow: /paid\nDisallow: /admin\nSitemap: https://tokumai.com/sitemap.xml\n").await,
        ("GET", "/health") => respond(&mut sock, 200, "text/plain", b"ok").await,
        ("GET", "/imprint") | ("GET", "/impressum") => {
            f.track("view:legal", Some(&req));
            respond(&mut sock, 200, "text/html; charset=utf-8", PAGE_IMPRINT.as_bytes()).await
        }
        ("GET", "/terms") | ("GET", "/agb") => {
            f.track("view:legal", Some(&req));
            respond(&mut sock, 200, "text/html; charset=utf-8", PAGE_TERMS.as_bytes()).await
        }
        ("GET", "/privacy") | ("GET", "/datenschutz") => {
            f.track("view:legal", Some(&req));
            respond(&mut sock, 200, "text/html; charset=utf-8", PAGE_PRIVACY.as_bytes()).await
        }
        ("GET", "/pay") => {
            f.track("view:pay", Some(&req));
            respond(&mut sock, 200, "text/html; charset=utf-8", pay_html().as_bytes()).await
        }
        ("GET", "/claim") | ("GET", "/redeem") => {
            f.track("view:claim", Some(&req));
            respond(&mut sock, 200, "text/html; charset=utf-8", PAGE_CLAIM.as_bytes()).await
        }
        // A download click: counted, then sent to the file Caddy serves from disk. Only names
        // the manifest lists — this is not a way to probe the directory.
        ("GET", p) if p.starts_with("/get/") => {
            let name = &p[5..];
            let (_, files) = read_manifest(&f.cfg.dl_dir);
            match files.values().find(|df| df.name == name) {
                Some(df) => {
                    f.track(&format!("dl:{}", dl_platform(&df.name)), None);
                    redirect(&mut sock, &format!("/dl/{}", df.name)).await
                }
                None => respond(&mut sock, 404, "text/plain", b"not found").await,
            }
        }
        ("GET", "/go/ios") => match std::env::var("DL_IOS").ok().filter(|u| publishable_link(u)) {
            Some(u) => {
                f.track("dl:ios", None);
                redirect(&mut sock, u.trim()).await
            }
            None => respond(&mut sock, 404, "text/plain", b"not found").await,
        },
        // Stripe's redirect target after a card checkout (see PAID_HTML). Any query string
        // is ignored — nothing on this page depends on it.
        ("GET", "/paid") => {
            f.track("view:paid", Some(&req));
            respond(&mut sock, 200, "text/html; charset=utf-8", PAID_HTML.as_bytes()).await
        }
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
                Ok(r) => {
                    f.track("claim", None);
                    respond(&mut sock, 200, "application/json", &json(&r)).await
                }
                Err(e) => {
                    let status = if e.starts_with("too many") { 429 } else { 400 };
                    respond(&mut sock, status, "application/json", &json(&json!({"error": e}))).await
                }
            }
        }
        // ---- buying a voucher on the web -------------------------------------------
        // The faucet cannot raise an invoice (the rails live in the server), so it books an
        // order and the server answers it. See docs/vouchers.md.
        ("POST", "/api/order") => {
            if !f.ip_ok_for(BUCKET_ORDER, &req.ip, ORDERS_PER_HOUR) {
                respond(&mut sock, 429, "application/json",
                    &json(&json!({"error": "too many orders from your connection — try again in an hour"}))).await;
                return;
            }
            let v: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let usd = v.get("usd").and_then(|u| u.as_u64()).unwrap_or(0) as u32;
            let method = match v.get("method").and_then(|m| m.as_str()) {
                Some("card") => "card",
                Some("nyx") => "nyx",
                _ => "nyx",
            };
            // § 356 (5) BGB does not care which surface the purchase happened on: no order
            // without both confirmations, exactly as in the app.
            let ok = |k: &str| v.pointer(&format!("/consent/{k}")).and_then(|b| b.as_bool()).unwrap_or(false);
            let version = v.pointer("/consent/version").and_then(|s| s.as_str()).unwrap_or("");
            if version.is_empty() || !ok("immediateStart") || !ok("waiverAck") {
                respond(&mut sock, 400, "application/json",
                    &json(&json!({"error": "please confirm both statements above"}))).await;
                return;
            }
            // The version comes from a page and ends up verbatim in sales.csv, so it is
            // checked here rather than trusted: a comma or a newline would corrupt the
            // bookkeeping record, and an unbounded string would let anyone write into it.
            if !scrai_server::pay::consent_version_ok(version) {
                respond(&mut sock, 400, "application/json",
                    &json(&json!({"error": "that consent version is not one we recognise"}))).await;
                return;
            }
            let id = format!("{:032x}", rand::random::<u128>());
            match web_order_new(&f.cfg.state_db(), &id, usd, method, version) {
                Ok(()) => {
                    f.track("order", None);
                    f.track(&format!("order:{method}"), None);
                    f.track(&format!("order:usd:{usd}"), None);
                    respond(&mut sock, 200, "application/json", &json(&json!({"id": id}))).await
                }
                Err(e) => respond(&mut sock, 500, "application/json", &json(&json!({"error": e}))).await,
            }
        }
        ("GET", "/api/order") => {
            let id = query_param(&req.query, "id").unwrap_or_default();
            let body = match web_order(&f.cfg.state_db(), &id) {
                None => json!({"state": "unknown"}),
                Some((_, _, _, Some(e))) => json!({"state": "error", "error": e}),
                Some((None, _, _, None)) => json!({"state": "raising"}),
                Some((Some(inv), pay, paid, None)) => json!({
                    "state": if paid { "paid" } else { "pay" },
                    "invoice": inv,
                    "pay": pay.and_then(|p| serde_json::from_str::<Value>(&p).ok()),
                }),
            };
            respond(&mut sock, 200, "application/json", &json(&body)).await
        }
        ("POST", "/api/order/cancel") => {
            let v: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("");
            let ok = state_rw(&f.cfg.state_db())
                .and_then(|c| {
                    c.execute(
                        "UPDATE web_orders SET cancelled_at = ?2 \
                         WHERE id = ?1 AND cancelled_at IS NULL AND paid_at IS NULL",
                        rusqlite::params![id, scrai_server::pay::now_ms() as i64],
                    )
                    .map(|n| n > 0)
                    .map_err(|e| e.to_string())
                })
                .unwrap_or(false);
            respond(&mut sock, 200, "application/json", &json(&json!({"cancelled": ok}))).await
        }
        // Reveal the code. Once — the store holds only its hash, so a second call cannot
        // produce it again and says so instead of pretending.
        ("POST", "/api/order/code") => {
            let v: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("");
            match web_order(&f.cfg.state_db(), id) {
                Some((Some(inv), pay, true, None)) => {
                    let toku = pay
                        .and_then(|p| serde_json::from_str::<Value>(&p).ok())
                        .and_then(|p| p.get("amountToku").or(p.get("amountScrai")).and_then(|t| t.as_u64()))
                        .unwrap_or(0);
                    match mint_voucher(&f.cfg.state_db(), id, &inv, toku) {
                        Ok((code, fresh)) => {
                            if fresh {
                                f.track("code", None);
                            }
                            respond(&mut sock, 200, "application/json",
                                &json(&json!({"code": code, "toku": toku}))).await
                        }
                        Err(e) => respond(&mut sock, 409, "application/json", &json(&json!({"error": e}))).await,
                    }
                }
                _ => respond(&mut sock, 400, "application/json",
                    &json(&json!({"error": "that payment is not settled"}))).await,
            }
        }
        // "I have written it down." The only thing that makes the promise on the page true,
        // so it happens on the buyer's word rather than on a timer.
        ("POST", "/api/order/ack") => {
            let v: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("");
            let ok = state_rw(&f.cfg.state_db())
                .and_then(|c| {
                    c.execute(
                        "UPDATE web_orders SET code = NULL WHERE id = ?1 AND code IS NOT NULL",
                        rusqlite::params![id],
                    )
                    .map(|n| n > 0)
                    .map_err(|e| e.to_string())
                })
                .unwrap_or(false);
            respond(&mut sock, 200, "application/json", &json(&json!({"forgotten": ok}))).await
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
    // Gated on the FAUCET switch, not on TESTNET: the invite rail runs beside real
    // purchases now, and while this said "testnet OFF — faucet hidden" the boot-time
    // balance check below never ran — so a wrong RPC or an empty wallet stayed invisible
    // until a tester pressed the button (2026-09-05).
    match (&wallet, faucet_on()) {
        (Some(w), true) => {
            println!(
                "scrai-faucet: invite credits ON{} · wallet {} · pays to {} · daily max {}",
                if testnet_on() { " (TESTNET)" } else { "" },
                w.address(),
                cfg.receive,
                cfg.daily_max
            );
            println!("scrai-faucet: chain RPC {}", cfg.rpc);
            match w.balance_unym().await {
                Ok(b) if b < cfg.reserve_unym => eprintln!(
                    "scrai-faucet: wallet holds {:.3} NYM, BELOW the reserve of {:.3} — every claim is refused until it is topped up",
                    b as f64 / 1e6,
                    cfg.reserve_unym as f64 / 1e6
                ),
                Ok(b) => println!(
                    "scrai-faucet: wallet balance {:.3} NYM (reserve {:.3}) — good for ~{} claims",
                    b as f64 / 1e6,
                    cfg.reserve_unym as f64 / 1e6,
                    b.saturating_sub(cfg.reserve_unym) / 60_000_000u128.max(1)
                ),
                Err(e) => eprintln!(
                    "scrai-faucet: BALANCE CHECK FAILED ({e}) — every claim will fail. Is {} a Tendermint RPC? \
                     A REST endpoint answers 501 Not Implemented; on mainnet the RPC is https://rpc.nymtech.net \
                     (set FAUCET_RPC).",
                    cfg.rpc
                ),
            }
        }
        // nosemgrep: scrai-secret-in-log -- names the missing VARIABLE, never a value
        (None, true) => eprintln!("scrai-faucet: no faucet wallet configured (FAUCET_MNEMONIC) — site only, claims refused"),
        (_, false) => println!("scrai-faucet: faucet disabled (FAUCET_ENABLED=0) — serving the site only"),
    }
    if let Err(e) = web_stats_init(&cfg.state_db()) {
        eprintln!("scrai-faucet: {e} — the site works, the admin console's website numbers will not");
    }
    let faucet = Arc::new(Faucet {
        cfg,
        wallet,
        claim_lock: tokio::sync::Mutex::new(()),
        attempts: Mutex::new(HashMap::new()),
        salt: rand::random(),
        stats_salt: Mutex::new((String::new(), 0)),
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
    use super::*;

    #[test]
    fn an_unfilled_placeholder_is_not_a_publishable_link() {
        assert!(publishable_link("https://testflight.apple.com/join/AbCd1234"));
        assert!(publishable_link("  https://tokumai.com/  "), "surrounding space is trimmed, not fatal");

        // the exact line that went live on 2026-09-08, copied from .env.example
        assert!(!publishable_link("https://testflight.apple.com/join/<code>"));
        assert!(!publishable_link("https://<host>/x"));
        assert!(!publishable_link("https://example.com/a b"));
        assert!(!publishable_link("http://tokumai.com/"), "plain http is never published");
        assert!(!publishable_link("https://"), "a bare scheme is not a link");
        assert!(!publishable_link(""));
    }
    use super::rpc_from_lcd;

    #[test]
    fn rpc_is_derived_from_the_lcd() {
        // sandbox: one host, REST under /api, RPC at the root
        assert_eq!(rpc_from_lcd(Some("https://validator-sandbox-1.nymtech.net/api")), "https://validator-sandbox-1.nymtech.net");
        assert_eq!(rpc_from_lcd(Some("https://validator-sandbox-1.nymtech.net/api/")), "https://validator-sandbox-1.nymtech.net");
        // mainnet: api.<host> is REST only — the RPC is the rpc.<host> sibling. Sending
        // Tendermint calls to the REST host answers 501 and every claim fails.
        assert_eq!(rpc_from_lcd(Some("https://api.nymtech.net")), "https://rpc.nymtech.net");
        assert_eq!(rpc_from_lcd(Some("https://api.example.org/")), "https://rpc.example.org");
        // anything else is taken as given (a self-hosted node, FAUCET_RPC overrides anyway)
        assert_eq!(rpc_from_lcd(Some("https://node.example.org:26657")), "https://node.example.org:26657");
        assert_eq!(rpc_from_lcd(None), "");
    }
}

#[cfg(test)]
mod web_stats {
    use super::*;

    /// The counters the faucet writes are what the console reads: one round trip through
    /// a real state.db, no IP anywhere in the file.
    #[test]
    fn counters_round_trip_into_the_admin_reader_and_store_no_ip() {
        let dir = std::env::temp_dir().join(format!("tokumai-webstats-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.db");
        Connection::open(&db).unwrap(); // the server normally creates the file
        web_stats_init(&db).unwrap();
        web_stats_init(&db).unwrap(); // idempotent
        let day = utc_day();
        web_stats_bump(&db, &day, "view:home", Some("aaaa")).unwrap();
        web_stats_bump(&db, &day, "view:home", Some("aaaa")).unwrap(); // same visitor twice
        web_stats_bump(&db, &day, "view:pricing", Some("bbbb")).unwrap();
        web_stats_bump(&db, &day, "dl:macos", None).unwrap();
        web_stats_bump(&db, &day, "order", None).unwrap();
        web_stats_bump(&db, &day, "order:card", None).unwrap();
        web_stats_bump(&db, &day, "order:usd:20", None).unwrap();
        web_stats_bump(&db, "2001-01-01", "view:home", Some("old")).unwrap();

        let w = scrai_server::admin::web_stats(&db);
        assert_eq!(w["ok"], true);
        assert_eq!(w["today"]["views"], 3);
        assert_eq!(w["today"]["uniques"], 2);
        assert_eq!(w["today"]["downloads"], 1);
        assert_eq!(w["today"]["orders"], 1);
        assert_eq!(w["today"]["codes"], 0);
        assert_eq!(w["today"]["pages"][0]["page"], "home");
        assert_eq!(w["today"]["pages"][0]["n"], 2);
        assert_eq!(w["today"]["methods"][0]["method"], "card");
        assert_eq!(w["today"]["amounts"][0]["usd"], 20);
        assert_eq!(w["all"]["views"], 4, "lifetime includes the old day");
        assert_eq!(w["all"]["uniques"], 3, "uniques over many days are summed per day");

        // the prune drops old fingerprints but keeps the aggregate
        web_stats_init(&db).unwrap();
        let w = scrai_server::admin::web_stats(&db);
        assert_eq!(w["all"]["uniques"], 2);
        assert_eq!(w["all"]["views"], 4);

        let raw = std::fs::read(&db).unwrap();
        assert!(!raw.windows(7).any(|w| w == b"9.9.9.9"), "no address is ever written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crawlers_and_empty_agents_are_not_visitors() {
        assert!(is_crawler(""));
        assert!(is_crawler("Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)"));
        assert!(is_crawler("curl/8.4.0"));
        assert!(!is_crawler("Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15"));
    }

    #[test]
    fn a_download_is_filed_under_its_platform() {
        assert_eq!(dl_platform("tokumai_0.6.2_aarch64.dmg"), "macos");
        assert_eq!(dl_platform("tokumai_0.6.2_x64-setup.exe"), "windows");
        assert_eq!(dl_platform("tokumai_0.6.2_amd64.deb"), "deb");
        assert_eq!(dl_platform("tokumai_0.6.2_amd64.AppImage"), "appimage");
        assert_eq!(dl_platform("tokumai_0.6.2_universal.apk"), "android");
    }
}

#[cfg(test)]
mod site_pages {
    use super::*;

    #[test]
    fn every_page_renders_only_its_own_block_and_leaves_no_token_behind() {
        let tmp = std::env::temp_dir().join(format!("tokumai-site-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        for (id, path, title, _) in PAGES {
            let html = site_page(&tmp, id);
            assert!(html.contains(&format!("data-page=\"{id}\"")), "{id}: body marks the page");
            assert!(html.contains(&format!("<link rel=\"canonical\" href=\"https://tokumai.com{path}\">")), "{id}: canonical");
            assert!(html.contains(&html_escape(title)), "{id}: title");
            assert!(!html.contains("<!--@page:"), "{id}: no block markers survive");
            assert!(!html.contains("{{"), "{id}: unfilled token in\n{}", html.lines().filter(|l| l.contains("{{")).take(3).collect::<Vec<_>>().join("\n"));
        }
        // Each page carries its own content and not the others'.
        let home = site_page(&tmp, "home");
        assert!(home.contains("There is nothing to <b>trust us with</b>"));
        assert!(!home.contains("Four things that <b>never meet</b>"));
        let how = site_page(&tmp, "how");
        assert!(how.contains("Four things that <b>never meet</b>") && !how.contains("Runs on your <b>machine</b>"));
        let dl = site_page(&tmp, "download");
        assert!(dl.contains("Runs on your <b>machine</b>") && dl.contains("Verify a download"));
        let vs = site_page(&tmp, "vs-duck-ai");
        assert!(vs.contains("FAQPage") && vs.contains("never gets it"));
        assert_eq!(page_for_path("/vs/duck-ai"), Some("vs-duck-ai"));
        assert_eq!(page_for_path("/nope"), None);
        assert!(sitemap_xml().contains("<loc>https://tokumai.com/how-it-works</loc>"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `TOKUMAI_DUMP_PAGES=<dir> cargo test --bin scrai-faucet dump_pages` writes every page as
    /// a file for a look in a browser before a deploy. Does nothing without the variable.
    #[test]
    fn dump_pages() {
        let Ok(dir) = std::env::var("TOKUMAI_DUMP_PAGES") else { return };
        let dir = std::path::PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&dir);
        for (id, _, _, _) in PAGES {
            std::fs::write(dir.join(format!("{id}.html")), site_page(&dir, id)).unwrap();
        }
    }
}
