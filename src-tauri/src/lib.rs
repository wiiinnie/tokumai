// ---------------------------------------------------------------------------
// tokumai desktop — the Rust core.
//
// The public/ UI runs in the webview and calls these commands via `invoke`.
// Account + held coconut credentials are local; everything else talks to the
// scrai-server over the embedded mixnet (nym.rs). The account crypto is in
// account.rs, the ecash crypto (Coconut / zk-nym) in the shared scrai-core.
// ---------------------------------------------------------------------------

mod account;
mod detect;
mod nym;
mod ocr;
mod vault;
mod wallet;
#[cfg(target_os = "ios")]
mod keychain_ios;
#[cfg(target_os = "ios")]
mod iap_ios;

use nym::Transport;
use rand::RngCore;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};

const TIERS: [u32; 4] = [5, 10, 20, 50];
/// (server reports testnet mode, faucet URL) — learned with the model list.
static SERVER_TESTNET: std::sync::Mutex<(bool, Option<String>)> = std::sync::Mutex::new((false, None));
/// Our website, from the catalog reply (`siteUrl`). The app builds the `/pay` hand-over
/// link from it; unlike `faucetUrl` it is NOT tied to testnet mode.
static SERVER_SITE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
/// What the server said about cards with the catalog (`{enabled, minUsd}`) — the card
/// row exists only when a server has a Mollie key, and the minimum tile is its call.
static SERVER_CARD: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);
/// Which payment rails the server can actually raise an invoice on (`rails` on the catalog
/// reply: `{nyx, btc, card, invite}`). The buy sheet greys out what is missing, and the
/// invite field exists only when the server has a faucet wallet pinned. Cached with the
/// models, like the rest — and it MUST be forwarded into the state object below, or the
/// webview falls back to its defaults and the invite field never appears (2026-09-05).
static SERVER_RAILS: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);
/// The coins on sale, grouped for the buy sheet (`coins` on the catalog reply): one entry
/// per tile, its chains as variants. Empty or absent → the app keeps its plain Bitcoin tile.
static SERVER_COINS: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);
/// The server's own version (`serverVersion` on the catalog reply; older servers send
/// none) — shown under Settings next to the app version.
static SERVER_VERSION: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
/// The server's update notice (`update` on the catalogue reply) — set with the model list,
/// shown by the UI as a blocking "Update available" gate.
static SERVER_UPDATE: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);
/// Product ids the server sells through the App Store (catalog `iapProducts`), remembered
/// with the models so the buy sheet has them without another round trip.
static IAP_PRODUCTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
/// tauri.conf.json's version, read once at launch; goes out as `app` on every request so
/// the server's release gate (MIN_APP) can tell an outdated build apart.
static APP_VER: std::sync::OnceLock<String> = std::sync::OnceLock::new();
pub fn app_version() -> &'static str {
    APP_VER.get().map(String::as_str).unwrap_or("0.0.0")
}

/// The App Store storefront the device is signed into, as an ISO 3166-1 alpha-3 code
/// ("USA", "DEU", …). Only iOS has one; `None` when no store account is signed in or on
/// any other platform. The webview turns it into the 3.1.1 top-up variant (see
/// `iosRegion` in index.html): a missing storefront falls to the strictest one.
#[cfg(target_os = "ios")]
#[allow(deprecated)] // SKPaymentQueue.storefront is the only storefront API reachable from Obj-C
fn ios_storefront() -> Option<String> {
    use objc2_store_kit::SKPaymentQueue;
    let queue = unsafe { SKPaymentQueue::defaultQueue() };
    let storefront = unsafe { queue.storefront() }?;
    let code = unsafe { storefront.countryCode() }.to_string();
    let code = code.trim().to_ascii_uppercase();
    (code.len() == 3 && code.bytes().all(|b| b.is_ascii_uppercase())).then_some(code)
}
#[cfg(not(target_os = "ios"))]
fn ios_storefront() -> Option<String> {
    None
}
const PROTO: u64 = 1;

// Reply-SURB budgets, sized per REPLY: every SURB rides in the request as one Sphinx
// packet, so a budget is inbound load on the server — the load test (docs/load-testing.md)
// measured 60 → 10 SURBs on one-packet replies halving their latency under a storm, and
// 120 making everything worse. The SDK re-requests SURBs when a reply outgrows its budget
// (one extra round trip), so a budget is a fast path, never a hard limit.
/// One-packet replies: status, invoice.*, entitlement, withdraw, upload acks, ping.
const SURBS_SMALL: u32 = 8;
/// For a reply that is certainly one packet (a withdrawal's blinded signature), sent in
/// batches of a hundred: eight each would make the SURBs bigger than everything else.
const SURBS_ONE_PACKET: u32 = 3;
/// The catalogue (a few KB).
const SURBS_META: u32 = 16;
/// Coconut `Keys` — the epoch material, ~207 KB at a thousand coins per book (measured
/// 2026-09-14), so ~105 packets. Fetched once per epoch and then cached on disk, which is
/// what keeps this out of the way of the first answer after a start.
const SURBS_KEYS: u32 = 130;
/// One staged picture chunk (96 KB base64 ≈ 50 packets).
const SURBS_CHUNK: u32 = 64;
/// `staged_download` key for the unsigned (free-model) chat path, which has no session.
/// Reply budget for text chats: ~30 Sphinx packets ≈ 60 KB — a long markdown answer is
/// 10–20 KB; anything bigger re-requests. (Was 150, before that a flat 500: each SURB
/// rides IN the request, so oversizing bloats every send and triggers retransmission
/// storms on the server's inbound reassembly.)
const SURBS_TEXT: u32 = 30;
// (Image chats used to carry a flat 500-SURB budget for a single ~MB reply. Generated
// pictures now come back as `image.chunk` references fetched with SURBS_SMALL each —
// see `fetch_staged_images` — so an image chat's own reply is text-sized.)
const TIMEOUT_MS: u64 = 120_000;
/// How long a NEW question waits for an older, unanswered tender to be closed. Short on
/// purpose: the answer is usually already in the server's replay cache, and a tender that
/// still says nothing stays pending rather than holding up the question in front of it.
const SETTLE_TIMEOUT_MS: u64 = 30_000;
/// Timeout for small metadata round trips (catalogue fetch in `state`): these replies
/// are a few KB and normally arrive in seconds — the generous chat TIMEOUT_MS here is
/// what once delayed the "server unreachable" verdict by minutes.
const META_TIMEOUT_MS: u64 = 30_000;


// --- C3: client-side overcharge guard (docs/security/audit-2026-08-20.md) --------
// The server is untrusted, yet today it alone decides the price/margin/token count
// and the client displays it blindly. We bake the SAME retail table the app shipped
// with and recompute a fair upper-bound price from the client's OWN token estimate,
// so a rogue operator can inflate NEITHER the margin NOR the token count unnoticed.
// (The federation-era hardening — a SIGNED, versioned list with a pinned pubkey so a
// foreign operator can't serve a forged table — is tracked separately; a single
// bundled table is already trusted for the shipped, pinned-server client.)
const BUNDLED_PRICING: &str = include_str!("../../pricing.json");
/// Retail margin the client is willing to accept (matches the project default 1.4).
const CLIENT_RETAIL_MARGIN: f64 = 1.4;
/// A charge above fair × this is treated as operator overcharge. Generous, because
/// the client's char/4 token estimate is coarser than a real tokenizer — this catches
/// the gross attacks (the audit's ≥50×, up to ~1000×) with wide false-positive headroom.
const OVERCHARGE_FACTOR: f64 = 4.0;
/// Don't flag trivial charges where rounding/floor noise dominates.
/// …and never below one coin: a coin-paid answer is rounded up to a whole coin, so a
/// cheap one can legitimately be charged more than its token price without anything
/// being wrong.
const MIN_FLAG_SCRAI: u64 = 50;

/// Servers this client caught grossly overcharging THIS process-run. In-memory on
/// purpose: a restart re-extends benefit of the doubt (avoids a permanent lockout
/// from a one-off fluke), while within a run we refuse to auto-redeem more coins into
/// a flagged server (the audit's "stop, redeem no more, flag it").
fn flagged_servers() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static F: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    F.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Mark a server as untrusted for the rest of this run — it overcharged (C3) or
/// issued an INVALID credential share while (server-side) consuming paid entitlement
/// (H3, proven by `verify_share` failing). Money ops (withdraw, auto-redeem) then
/// refuse it, capping the loss to the one step that exposed the cheat. Note: the value
/// already lost to that step CANNOT be recovered against a SINGLE untrusted operator —
/// atomic entitlement↔credential exchange is impossible without a threshold/TTP; t-of-n
/// issuance is the structural prevention. This detect-and-quarantine is the best a
/// single-operator client can do (and the audit's remediation).
fn flag_server(srv: &str, reason: &str) {
    if let Ok(mut f) = flagged_servers().lock() {
        if f.insert(srv.to_string()) {
            log::error!("[trust] flagged {srv} as dishonest: {reason} — refusing further money ops to it this run");
        }
    }
}

fn is_flagged(srv: &str) -> bool {
    flagged_servers().lock().map(|f| f.contains(srv)).unwrap_or(false)
}




/// Concatenated plaintext of a chat `messages` array, or None if any message is
/// non-text (multimodal / image parts) — where a char-based token estimate is
/// meaningless and the guard must not run.
fn messages_plaintext(messages: &Value) -> Option<String> {
    let mut s = String::new();
    for m in messages.as_array()? {
        match m.get("content") {
            Some(Value::String(c)) => {
                s.push_str(c);
                s.push('\n');
            }
            _ => return None,
        }
    }
    Some(s)
}

/// The client's independent fair upper-bound price (whole TOKU) for one exchange,
/// or None when it can't be estimated (multimodal input, model not in the bundled
/// table, unparsable table). Uses the exact same billing math as the server.
fn fair_price_estimate(model: &str, messages: &Value, reply_text: &str) -> Option<u64> {
    use scrai_core::billing::{compute_billing, estimate_tokens, TokenUsage};
    use scrai_core::pricing::PricingTable;
    let table = PricingTable::parse(BUNDLED_PRICING).ok()?;
    let price = table.price(model);
    // Unlisted in OUR table → we have no trusted reference → skip (never a false flag).
    if price.fallback {
        return None;
    }
    let input = messages_plaintext(messages)?;
    let usage = TokenUsage {
        input: estimate_tokens(input.chars().count() as u64),
        output: estimate_tokens(reply_text.chars().count() as u64),
        ..Default::default()
    };
    Some(compute_billing(&price, &usage, CLIENT_RETAIL_MARGIN, 1, true).price_toku)
}

/// Dev-only environment overrides. These read the AMBIENT environment of a user's machine,
/// not a config file we own, so unlike the server they keep a namespace: a bare `CONFIG` or
/// `SERVER_ADDRESS` would collide with any unrelated tool that happens to set one.
/// `TOKUMAI_` is the new prefix, `SCRAI_` still read so existing dev setups keep working.
fn dev_env(name: &str) -> Option<String> {
    std::env::var(format!("TOKUMAI_{name}"))
        .or_else(|_| std::env::var(format!("SCRAI_{name}")))
        .ok()
        .filter(|v| !v.trim().is_empty())
}

#[cfg(not(target_os = "windows"))]
fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path().app_data_dir().map_err(|e| e.to_string())
}

/// Windows: LOCAL app data, not Roaming. `app_data_dir` is `AppData\Roaming`, which a
/// domain profile synchronises to the server — the encrypted wallet and the chat vault
/// have no business travelling with a login. Same rule as the mobile backup exclusion:
/// secrets stay on the machine they were made on. Restore is by recovery phrase.
#[cfg(target_os = "windows")]
fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path().app_local_data_dir().map_err(|e| e.to_string())
}

/// Windows, once: an install that kept its data in Roaming moves it to Local. Runs after
/// the pre-rebrand migration, so a Roaming dir under either name is a candidate.
#[cfg(target_os = "windows")]
fn migrate_roaming_to_local(app: &AppHandle) {
    let (Ok(local), Ok(roaming)) = (app.path().app_local_data_dir(), app.path().app_data_dir()) else { return };
    let legacy = roaming.parent().map(|p| p.join("com.scrambleai.app"));
    let Some(from) = pick_migration_source(local.exists(), &[Some(roaming), legacy]) else { return };
    match std::fs::rename(&from, &local) {
        Ok(()) => log::info!("[migrate] moved the data directory out of Roaming: {}", from.display()),
        Err(e) => eprintln!("[migrate] could not move {} to {}: {e}", from.display(), local.display()),
    }
}

/// Which existing directory to adopt, if the target does not exist yet. Pure, so it can be
/// tested without a Windows profile: first candidate that is a directory wins, none if the
/// target already exists.
#[cfg(any(target_os = "windows", test))]
fn pick_migration_source(target_exists: bool, candidates: &[Option<PathBuf>]) -> Option<PathBuf> {
    if target_exists {
        return None;
    }
    candidates.iter().flatten().find(|p| p.is_dir()).cloned()
}

/// iOS: keep the whole data container out of iCloud and Finder backups. It holds the
/// wallet (phrase + bearer coins) and the chat vault, and the default is to back all of
/// that up to a store Apple can read. Recovery is by phrase — and, if the user opts in, by
/// the phrase's own end-to-end Keychain copy — never by restoring this directory.
#[cfg(target_os = "ios")]
fn exclude_from_backup(dir: &Path) {
    use objc2_foundation::{NSNumber, NSString, NSURL, NSURLIsExcludedFromBackupKey};
    let _ = std::fs::create_dir_all(dir);
    let path = NSString::from_str(&dir.to_string_lossy());
    let url = NSURL::fileURLWithPath(&path);
    let yes = NSNumber::numberWithBool(true);
    // SAFETY: a file URL we just built, a boolean NSNumber, and a key the framework defines.
    match unsafe { url.setResourceValue_forKey_error(Some(&yes), NSURLIsExcludedFromBackupKey) } {
        Ok(()) => log::info!("[backup] data container excluded from iCloud/Finder backup"),
        Err(e) => log::error!("[backup] could NOT exclude the data container from backup: {e}"),
    }
}

/// iOS, at start: no wallet here, but the user's phrase is in iCloud Keychain (they opted in
/// on a previous phone) → this IS the restore. The account comes back on its own; the
/// coins that were still on the old phone do not, and the UI says so.
#[cfg(target_os = "ios")]
fn restore_from_synced_phrase(dir: &Path) {
    let w = wallet::load(dir);
    if w.mnemonic.is_some() {
        return;
    }
    match wallet::synced_phrase() {
        Ok(Some(m)) => match account::from_mnemonic(&m) {
            Ok(a) => {
                let w = wallet::Wallet { mnemonic: Some(a.mnemonic), server: w.server, entry_gateway: w.entry_gateway, entry_random: w.entry_random, legacy_session_chat: w.legacy_session_chat, phrase_verified: true, ..Default::default() };
                match wallet::save(dir, &w) {
                    Ok(()) => log::info!("[restore] account restored from the iCloud Keychain copy"),
                    Err(e) => log::error!("[restore] found a Keychain copy but could not save the wallet: {e}"),
                }
            }
            // The parse error is not logged: a bip39 message can quote what it was given.
            Err(_) => log::error!("[restore] the Keychain copy does not parse as an account"),
        },
        Ok(None) => {}
        Err(e) => log::warn!("[restore] could not read iCloud Keychain: {e}"),
    }
}

/// The app data directory is NAMED after the bundle identifier, so renaming
/// com.scrambleai.app → com.tokumai.app would start the app on an empty wallet, an empty
/// chat vault and no server address, with everything still sitting on disk one directory
/// over. Move it across once, before anything can create the new one — `diag()` alone
/// would be enough to make this look like a fresh install and skip the move for good.
///
/// Desktop only. On iOS and Android the identifier IS the sandbox: a renamed bundle is a
/// new app with a new container and no way to reach the old one, which is why the rename
/// was a deliberate decision (testers hold test credit only) rather than something to
/// paper over here.
#[cfg(not(any(target_os = "ios", target_os = "android")))]
fn migrate_pre_rebrand_data_dir(app: &AppHandle) {
    const LEGACY_ID: &str = "com.scrambleai.app";
    let Ok(new) = app.path().app_data_dir() else { return };
    if new.exists() {
        return; // already migrated, or a genuinely fresh install that has run once
    }
    let Some(old) = new.parent().map(|p| p.join(LEGACY_ID)) else { return };
    if !old.is_dir() {
        return;
    }
    match std::fs::rename(&old, &new) {
        Ok(()) => log::info!("[migrate] adopted the pre-rebrand data directory {}", old.display()),
        // Same volume in every real case; if it ever is not, say so loudly rather than
        // starting empty and letting the user think the wallet is gone.
        Err(e) => eprintln!("[migrate] could not move {} to {}: {e}", old.display(), new.display()),
    }
}
#[cfg(any(target_os = "ios", target_os = "android"))]
fn migrate_pre_rebrand_data_dir(_app: &AppHandle) {}

// ---- chat vault (vault.rs): sessions live in Rust-managed files, key in the OS keychain.
// The webview sees plaintext sessions over IPC only — never the key.
async fn vault_blocking<T: Send + 'static>(
    app: &AppHandle,
    f: impl FnOnce(&Path) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let dir = data_dir(app)?;
    tauri::async_runtime::spawn_blocking(move || f(&dir)).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn vault_list(app: AppHandle) -> Result<Vec<vault::Meta>, String> {
    vault_blocking(&app, |d| vault::list(d)).await
}

#[tauri::command]
async fn vault_load(app: AppHandle, id: String) -> Result<Option<Value>, String> {
    vault_blocking(&app, move |d| vault::load(d, &id)).await
}

#[tauri::command]
async fn vault_save(app: AppHandle, session: Value, updated: Option<u64>) -> Result<vault::Meta, String> {
    vault_blocking(&app, move |d| vault::save(d, session, updated)).await
}

#[tauri::command]
async fn vault_remove(app: AppHandle, id: String) -> Result<(), String> {
    vault_blocking(&app, move |d| vault::remove(d, &id)).await
}

// Pending payments (open invoices + faucet memos): encrypted beside the chat vault
// instead of webview localStorage — see vault::pending_save for the why.
#[tauri::command]
async fn pending_load(app: AppHandle) -> Result<Value, String> {
    vault_blocking(&app, |d| vault::pending_load(d)).await
}

#[tauri::command]
async fn pending_save(app: AppHandle, list: Value) -> Result<(), String> {
    vault_blocking(&app, move |d| vault::pending_save(d, list)).await
}

/// After the one-time IndexedDB → vault migration: drop the webview's stored site data so
/// the old ciphertext + key do not linger in WebView2's LevelDB log files until Chromium
/// compacts them. The webview restores its localStorage settings itself (backend.js).
/// Unsupported on Android (wry) — the caller treats an error as "nothing to purge".
#[tauri::command]
async fn vault_purge_webdata(webview: tauri::Webview) -> Result<(), String> {
    webview.clear_all_browsing_data().map_err(|e| e.to_string())
}

fn rand_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

// TEMP DIAGNOSTIC: iOS release routes stdout/stderr nowhere reachable, so append
// startup-command progress to <data_dir>/diag.log and pull it with `devicectl copy from`
// after the crash. The last line names the command whose IPC response was in flight.
fn diag(app: &AppHandle, msg: &str) {
    use std::io::Write;
    // Debug builds only: a release app must not keep a plaintext activity log on disk.
    if !cfg!(debug_assertions) {
        return;
    }
    if let Ok(dir) = data_dir(app) {
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("diag.log")) {
            let _ = writeln!(f, "{msg}");
        }
    }
}

/// The CLI stores its server address in ~/.scrai/cli.json; read it as a fallback
/// so `npm run client -- server <addr>` also configures the app.
fn cli_config_server() -> Option<String> {
    let path = dev_env("CONFIG").map(PathBuf::from).or_else(|| {
        std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".scrai").join("cli.json"))
    })?;
    let s = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&s).ok()?;
    v.get("serverAddress").and_then(|a| a.as_str()).map(str::to_string)
}

// Temporary: the single official scrai-server, hardcoded so a fresh install works
// out-of-the-box while there is exactly one operator. The frontend pins the same address
// (KNOWN_SERVERS). A wallet-set server (your own or a 3rd-party) or SERVER_ADDRESS
// always OVERRIDES this. Replace with a signed directory when onboarding other servers
// (docs/federation-shared-ledger.md). This also makes server config self-healing: even if
// the on-device wallet loses its `server` field, requests still reach the official server.
const OFFICIAL_SERVER: &str = "4LjM6dbZ9Pu4hS7hg3AHAK4YLkRfsdH8U5Bi1E9CPBA7.BLpmR82Up6HiucSBLGJQRys6ZHVNZF2doZLPoZFSwTpC@38zcSsvjXsAX7C28ko2H3Lt55X4TYxfZYkPADxKXZHUj";

fn server_addr(w: &wallet::Wallet) -> Result<String, String> {
    // L14: the wallet `server` field is validated at set_server, but the env/CLI fallbacks
    // were only non-empty-checked, so a malformed override slipped through to fail later at
    // Recipient parsing. Validate them here too — an invalid override is dropped, falling
    // back to the official server rather than erroring every request.
    let valid = |s: String| Transport::validate_address(&s).is_ok().then_some(s);
    Ok(w.server
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| dev_env("SERVER_ADDRESS").and_then(&valid))
        .or_else(|| cli_config_server().and_then(&valid))
        .unwrap_or_else(|| OFFICIAL_SERVER.to_string()))
}

fn qr_svg(uri: &str) -> String {
    use qrcode::render::svg;
    match qrcode::QrCode::new(uri.as_bytes()) {
        Ok(code) => code
            .render::<svg::Color>()
            .min_dimensions(200, 200)
            .quiet_zone(true)
            .build(),
        Err(_) => String::new(),
    }
}

// The NymQR look (github.com/Ch1ffr3punk/NymQR): a purple "community" QR with the
// Nym mark set in the centre. We take that repo's idea — brand the code, encode
// the bare Nyx address — but draw it as SVG here rather than shelling out to the
// PNG CLI. Error-correction High is what lets the centre logo cover ~20% of the
// modules without breaking the scan. The mark is nested over the QR so we never
// have to parse the module grid to find the middle.
fn qr_svg_nym(data: &str) -> String {
    use qrcode::render::svg;
    use qrcode::{EcLevel, QrCode};
    let code = match QrCode::with_error_correction_level(data.as_bytes(), EcLevel::H) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    let svg = code
        .render::<svg::Color>()
        .min_dimensions(200, 200)
        .quiet_zone(true)
        .dark_color(svg::Color("#7A5FFF")) // Nym community purple
        .light_color(svg::Color("#ffffff"))
        .build();
    // The crate emits `<svg ... width="W" height="W" viewBox="0 0 W W">` where W is
    // a PIXEL size rounded up to the module grid (≥200), NOT 200. So the true
    // centre is W/2 — inject the Nym mark straight into the crate's own coordinate
    // system rather than wrapping it in a mismatched outer viewBox. Scale the mark
    // (authored for a 200-unit code) by W/200 so its proportions stay constant.
    let w = svg_first_width(&svg).unwrap_or(200.0);
    let c = w / 2.0;
    let s = w / 200.0;
    let logo = format!(
        r##"<g transform="translate({c} {c}) scale({s})"><circle r="23" fill="#ffffff"/><circle r="23" fill="none" stroke="#7A5FFF" stroke-width="1.5"/><g transform="translate(-16 -16)" fill="none" stroke="#7A5FFF" stroke-width="2.8" stroke-linejoin="round" stroke-linecap="round"><path d="M11 22V10l10 12V10"/></g></g>"##,
        c = c,
        s = s,
    );
    svg.replace("</svg>", &format!("{logo}</svg>"))
}

/// First `width="…"` in an SVG string, as a number (the root <svg>'s width).
fn svg_first_width(svg: &str) -> Option<f64> {
    let key = "width=\"";
    let start = svg.find(key)? + key.len();
    let rest = &svg[start..];
    let end = rest.find('"')?;
    rest[..end].parse().ok()
}

// ---- mixnet helpers used by several commands ------------------------------


// ---- coconut federation (talks to the Rust server / issuing authority) -----

/// One federation round-trip over the mixnet: wrap a `FedRequest` in the app's
/// id-envelope, send it, and unwrap the `FedResponse` from the reply.
// ---------------------------------------------------------------------------------------
// The purchase client (design 2026-09-13, block A). Account-side calls — invoice, invite
// check, code redeem, App Store receipt, entitlement + coin withdrawal — leave through a
// SECOND Nym client with its own ephemeral identity, so the server never sees an account
// call and a session call under the same reply-SURB sender tag. It exists only while it
// is needed: built on the first account call, dropped after a collect or when the buy
// sheet closes. Its entry gateway is one of the operator's, never the chat client's — and
// that is re-checked on EVERY use, because the user can move the chat client to any
// gateway at any time.
// ---------------------------------------------------------------------------------------

#[derive(Default)]
struct BuyLink {
    t: tokio::sync::Mutex<Option<Arc<Transport>>>,
}

/// The purchase client, connecting lazily. Enforces the gateway rule: if its gateway equals
/// the chat client's current one, it is moved (never the chat client, whose gateway is the
/// user's choice).
async fn buy_transport(app: &AppHandle, main: &Transport) -> Arc<Transport> {
    let link = app.state::<BuyLink>();
    let mut g = link.t.lock().await;
    let main_gw = main.entry_gateway_id().await;
    if g.is_none() {
        let t = Arc::new(Transport::new());
        let (cover, mix, send, cont) = main.perf();
        t.set_perf(cover, mix, send, cont).await;
        let h = app.clone();
        t.set_progress_sink(Box::new(move |step, detail| {
            let _ = h.emit("buy-phase", json!({ "step": step, "detail": detail }));
        }));
        // Connect now and prove the gateway answers. A random operator gateway can be
        // down, and an account call that fails on that is indistinguishable, to the user,
        // from "the server is gone" — so try a few before giving up.
        let mut ok = false;
        for attempt in 1..=3 {
            t.set_entry_gateway(Some(nym::hermes_gateway_excluding(main_gw.as_deref()))).await;
            match t.ensure_connected().await {
                Ok(()) => {
                    ok = true;
                    break;
                }
                Err(e) => log::warn!("[buy-link] gateway attempt {attempt} failed: {e}"),
            }
        }
        if ok {
            log::info!("[buy-link] purchase client up on its own gateway");
        } else {
            log::warn!("[buy-link] no operator gateway answered — the next account call will retry");
        }
        *g = Some(t);
    }
    let t = g.clone().expect("just set");
    if let (Some(m), Some(b)) = (main_gw.as_deref(), t.entry_gateway_id().await.as_deref()) {
        if m == b {
            // The chat client moved onto our gateway (server switch, gateway picker):
            // the purchase client yields and re-picks; dropping the live client is fine,
            // the next call reconnects.
            let pick = nym::hermes_gateway_excluding(Some(m));
            t.set_entry_gateway(Some(pick)).await;
            log::info!("[buy-link] chat client took the purchase gateway — moved the purchase client");
        }
    }
    t
}

/// Tear the purchase client down (buy sheet closed, or the coins are in). Its identity is
/// ephemeral, so the next purchase starts from fresh keys on a fresh gateway pick.
async fn close_buy_link(app: &AppHandle) {
    let link = app.state::<BuyLink>();
    let t = link.t.lock().await.take();
    if let Some(t) = t {
        t.drop_client().await;
        log::info!("[buy-link] purchase client closed");
    }
}

#[tauri::command]
async fn buy_close(app: AppHandle) -> Result<Value, String> {
    close_buy_link(&app).await;
    Ok(json!({ "closed": true }))
}

/// How many reply SURBs a federation call has to carry.
///
/// The epoch material is ~25 KB — a dozen packets — and the reply can only come back in
/// SURBs the request brought along. When the request for it grew a second variant
/// (`KeysFor`, one per denomination) this match did not, so every key fetch went out with
/// eight SURBs and could not be answered: the app redeemed a code, showed the credit, and
/// the balance sat still until someone pressed "Check for credit" (2026-09-15).
fn surbs_for(req: &scrai_core::federation::FedRequest) -> u32 {
    use scrai_core::federation::FedRequest;
    match req {
        FedRequest::Keys | FedRequest::KeysFor { .. } => SURBS_KEYS,
        _ => SURBS_SMALL,
    }
}

async fn fed_call(
    t: &Transport,
    srv: &str,
    req: scrai_core::federation::FedRequest,
) -> Result<scrai_core::federation::FedResponse, String> {
    let surbs = surbs_for(&req);
    let env = json!({
        "v": PROTO, "kind": "coconut", "id": rand_hex(16),
        "fed": serde_json::to_value(&req).map_err(|e| e.to_string())?,
    });
    let reply = t.round_trip(srv, &env, surbs, TIMEOUT_MS).await?;
    serde_json::from_value(reply.get("fed").cloned().ok_or("no fed in reply")?)
        .map_err(|e| format!("bad fed response: {e}"))
}

/// The federation's `Keys` reply per server, kept for the epoch. It is ~109 KB and
/// changes only when the authority rotates (visible as a new `expiration_date`), yet the
/// app used to fetch it for EVERY ticketbook — in a purchase storm those replies were the
/// server's biggest outbound burst. Cached in memory (one fetch per app run and epoch);
/// dropped on any withdrawal failure so a rotated key is refetched on the retry.
fn keys_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, (u32, Value)>> {
    static C: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, (u32, Value)>>> = std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Fetch (or reuse) the federation keys for `srv`.
async fn federation_keys(
    t: &Transport,
    srv: &str,
    dir: &Path,
    denom_toku: u64,
    expiration_date: u32,
) -> Result<scrai_core::federation::FedResponse, String> {
    // Each denomination is its own issuing authority with its own material, so it is
    // fetched and cached apart. The cache key is the server AND the denomination; the
    // ADDRESS stays what it was — that is what the request is sent to.
    //
    // The material of OLDER epochs is kept beside it (`keys-<hash>.json` per epoch): the
    // server rolls a new authority every week, and a book can only be spent with the keys
    // of the epoch it was issued in. Dropping them on a refresh would strand every book
    // the device still holds.
    // Asking for a PARTICULAR epoch (a device still holding books from it) is filed under
    // that epoch; asking for "whatever you issue today" is filed under the denomination.
    let cache_key = if expiration_date == 0 {
        format!("{srv}#{denom_toku}")
    } else {
        epoch_key(srv, denom_toku, expiration_date)
    };
    let cache_key = cache_key.as_str();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as u32;
    // Valid while the spend date the app uses (expiration − 1 day) is still ahead — AND not
    // older than a roll. The server starts a new epoch every week; without the second half
    // the app would keep withdrawing from the epoch it first met, and its books would have
    // less and less life left in them.
    let max_age = (BOOK_VALIDITY_DAYS_HINT - ROLL_EVERY_DAYS_HINT) * 86_400;
    let fresh = |exp: u32| now + 2 * 86_400 < exp && exp.saturating_sub(now) as u64 > max_age;
    let parse = |v: Value| serde_json::from_value::<scrai_core::federation::FedResponse>(v).ok();
    if let Some((exp, v)) = keys_cache().lock().ok().and_then(|c| c.get(cache_key).cloned()) {
        if fresh(exp) {
            if let Some(r) = parse(v) {
                return Ok(r);
            }
        }
    }
    // Then the disk. At a thousand coins per book this material is ~200 KB, so fetching it
    // once per app start would put a fifth of a megabyte through the mixnet before the
    // first answer. It changes only when the authority rotates, which shows up as a new
    // expiration date — so a stale file is simply ignored, never trusted.
    if let Some((exp, v)) = read_keys_file(dir, cache_key) {
        if fresh(exp) {
            if let Some(r) = parse(v.clone()) {
                if let Ok(mut c) = keys_cache().lock() {
                    c.insert(cache_key.to_string(), (exp, v));
                }
                return Ok(r);
            }
        }
    }
    let want_epoch = expiration_date;
    let resp = fed_call(t, srv, scrai_core::federation::FedRequest::KeysFor { denom_toku, expiration_date }).await?;
    if let scrai_core::federation::FedResponse::Keys { expiration_date, denom_toku: got, .. } = &resp {
        // A server that answers with another denomination than the one asked for is not
        // one to take money from: the book would be worth something else than the price.
        if *got != denom_toku {
            return Err(format!("the server answered with {got}-TOKU coins, not the {denom_toku} asked for"));
        }
        // Asked for ONE epoch and got another: that epoch is gone, and so are its books.
        // Say so rather than filing the wrong material under its name.
        if want_epoch != 0 && *expiration_date != want_epoch {
            return Err("that issuing epoch is over — books from it can no longer be spent".into());
        }
        if let Ok(v) = serde_json::to_value(&resp) {
            // Twice: under the denomination (what the next withdrawal uses) and under the
            // epoch (what the books of that epoch are spent with).
            write_keys_file(dir, cache_key, *expiration_date, &v);
            write_keys_file(dir, &epoch_key(srv, denom_toku, *expiration_date), *expiration_date, &v);
            if let Ok(mut c) = keys_cache().lock() {
                c.insert(cache_key.to_string(), (*expiration_date, v));
            }
        }
    }
    Ok(resp)
}

/// Where the epoch material for one server is kept. The file name is a hash of the
/// address, not the address itself — this directory is not a list of who we talk to.
fn keys_file(dir: &Path, srv: &str) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    let h = hex::encode(&Sha256::digest(srv.as_bytes())[..8]);
    dir.join(format!("keys-{h}.json"))
}

/// The epoch material for a server, from the on-disk cache, in the form a book needs to
/// spend. Held once per server and epoch rather than inside every book.
/// Most coins one `coins.return` may carry — the server's own bound (it costs ~4 ms of
/// pairings per coin), so a device hands its books back in several batches.
const MAX_RETURN_COINS: u64 = 200;

/// Seconds since the epoch, as the expiration dates are counted.
fn now_secs32() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}

/// How close to its expiry a book is swapped for a fresh one. It has to be WIDER than the
/// interval at which people open the app, because the swap can only run while it is open:
/// a three-day window protects a daily user and nobody else. Fourteen days costs a book
/// the last sixth of its life and covers anyone who opens the app every other week; what
/// is not swapped in time expires, and expired coins cannot be refunded — the server
/// cannot tell which of them were spent.
const SWAP_WINDOW_DAYS: u64 = 14;

/// How long the SERVER says its books live, and how often it starts a new epoch. Only a
/// hint: the real numbers come with the keys (`expiration_date`), and these just decide
/// when the app goes and asks again. Too small costs a key fetch; too large means books
/// drawn late in an epoch, which is the thing rolling epochs exist to avoid.
const BOOK_VALIDITY_DAYS_HINT: u64 = 90;
const ROLL_EVERY_DAYS_HINT: u64 = 7;

/// Where one epoch's material is filed. Keyed by the epoch, not just the denomination, so
/// a refresh never drops the keys a book on this device still needs.
fn epoch_key(srv: &str, denom_toku: u64, expiration_date: u32) -> String {
    format!("{srv}#{denom_toku}#{expiration_date}")
}

fn epoch_keys(dir: &Path, srv: &str, denom_toku: u64) -> Option<scrai_core::purse::EpochKeys> {
    use scrai_core::federation::FedResponse;
    let (_, v) = read_keys_file(dir, &format!("{srv}#{denom_toku}"))?;
    match serde_json::from_value::<FedResponse>(v).ok()? {
        FedResponse::Keys { vk, coin_sigs, date_sigs, expiration_date, total_coins, denom_toku, .. } => {
            Some(scrai_core::purse::EpochKeys { vk, coin_sigs, date_sigs, expiration_date, total_coins, denom_toku })
        }
        _ => None,
    }
}

/// Every epoch's material this device has from `srv`, for every denomination — a book is
/// spent with the keys of the epoch it was issued in, and a device can hold books from
/// more than one at a time.
fn all_epoch_keys(dir: &Path, srv: &str) -> Vec<scrai_core::purse::EpochKeys> {
    use scrai_core::purse::EpochKeys;
    let mut out: Vec<EpochKeys> = Vec::new();
    let mut add = |k: EpochKeys| {
        if !out.iter().any(|o| o.denom_toku == k.denom_toku && o.expiration_date == k.expiration_date) {
            out.push(k);
        }
    };
    // What the books on this device were issued in, first…
    for w in wallet::load(dir).coconut_purses.iter() {
        if let Ok(p) = scrai_core::purse::Purse::restore(w) {
            if let Some(k) = keys_of_epoch(dir, srv, p.denom_toku(), p.expiration_date()) {
                add(k);
            }
        }
    }
    // …and the current one per denomination, which is what a fresh book will need.
    for d in scrai_core::coconut::DENOMS {
        if let Some(k) = epoch_keys(dir, srv, d) {
            add(k);
        }
    }
    out
}

/// Epochs this device holds books from but has no material for — what it has to fetch
/// before those books can be spent at all.
fn missing_epochs(w: &wallet::Wallet, dir: &Path, srv: &str) -> Vec<(u64, u32)> {
    let mut out: Vec<(u64, u32)> = Vec::new();
    for j in &w.coconut_purses {
        let Ok(p) = scrai_core::purse::Purse::restore(j) else { continue };
        let k = (p.denom_toku(), p.expiration_date());
        if p.remaining_coins() > 0 && !out.contains(&k) && keys_of_epoch(dir, srv, k.0, k.1).is_none() {
            out.push(k);
        }
    }
    out
}

/// The material of ONE epoch, if this device kept it.
fn keys_of_epoch(dir: &Path, srv: &str, denom_toku: u64, expiration_date: u32) -> Option<scrai_core::purse::EpochKeys> {
    use scrai_core::federation::FedResponse;
    let (_, v) = read_keys_file(dir, &epoch_key(srv, denom_toku, expiration_date))?;
    match serde_json::from_value::<FedResponse>(v).ok()? {
        FedResponse::Keys { vk, coin_sigs, date_sigs, expiration_date, total_coins, denom_toku, .. } => {
            Some(scrai_core::purse::EpochKeys { vk, coin_sigs, date_sigs, expiration_date, total_coins, denom_toku })
        }
        _ => None,
    }
}

fn read_keys_file(dir: &Path, srv: &str) -> Option<(u32, Value)> {
    let raw = std::fs::read_to_string(keys_file(dir, srv)).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let exp = v.get("expiration_date").and_then(|e| e.as_u64())? as u32;
    Some((exp, v.get("keys").cloned()?))
}

fn write_keys_file(dir: &Path, srv: &str, expiration_date: u32, keys: &Value) {
    let body = json!({ "expiration_date": expiration_date, "keys": keys });
    match serde_json::to_vec(&body).map_err(|e| e.to_string()).and_then(|b| {
        std::fs::write(keys_file(dir, srv), b).map_err(|e| e.to_string())
    }) {
        Ok(()) => log::info!("[coconut] epoch material cached on disk"),
        // Not fatal: without the file every start just refetches, as it did before.
        Err(e) => log::warn!("[coconut] could not cache the epoch material: {e}"),
    }
}

fn forget_keys(srv: &str) {
    if let Ok(mut c) = keys_cache().lock() {
        c.remove(srv);
    }
}

/// Drop the cached epoch material for a server, on disk as well — used where a rotated
/// authority is suspected, so the retry fetches the new one instead of re-reading the old.
fn forget_keys_on_disk(dir: &Path, srv: &str) {
    for d in scrai_core::coconut::DENOMS {
        let key = format!("{srv}#{d}");
        let _ = std::fs::remove_file(keys_file(dir, &key));
        if let Ok(mut c) = keys_cache().lock() {
            c.remove(&key);
        }
    }
    forget_keys(srv);
    let _ = std::fs::remove_file(keys_file(dir, srv));
}

/// Full credential withdrawal: fetch keys → blind-withdraw at each authority →
/// aggregate into a `Purse`. The Withdraw itself is ACCOUNT-SIGNED: the server
/// only issues a ticketbook against paid entitlement, and the account signature
/// How much credit the app keeps on the device: one dollar. It is a CAP, not an increment
/// — a top-up fills UP to it — so the most a lost device can cost is that dollar, and the
/// app can say so plainly.
const WORKING_TOKU: u64 = scrai_core::coconut::TOKU_PER_USD;
/// …and the point at which it goes and fetches more, a third of the way down.
const LOW_WATER_TOKU: u64 = WORKING_TOKU / 3;
/// How many FINE books (one cent each) the device keeps beside the coarse ones. They are
/// the small change: a tender pays its bulk in coarse coins and needs at most one book's
/// worth of fine ones to land on the exact price, so a handful covers many requests while
/// keeping the bytes down.
const FINE_FLOAT_BOOKS: usize = 3;

/// Draw up to `want` ticketbooks in ONE round trip.
///
/// Books are small on purpose (docs/unlinkability.md, block D): a small book keeps what a
/// lost device costs small and keeps the undrawable remainder on the account small, and
/// drawing several at once keeps the account-side calls as rare as one big book would.
///
/// M-cl-2 per book: each request body is persisted BEFORE it leaves, and a book whose
/// reply never arrived is re-sent with the SAME body — the server answers that from its
/// issued cache rather than charging again. A book whose reply was a definitive refusal
/// is dropped; one whose reply was merely "busy" is kept for the next attempt.
async fn withdraw_books(
    t: &Transport,
    srv: &str,
    auth: &account::Account,
    dir: &std::path::Path,
    want: usize,
    denom_toku: u64,
) -> Result<u64, String> {
    use scrai_core::coconut;
    use scrai_core::federation::{FedRequest, FedResponse};

    // H3: refuse to buy a credential from a server this run already caught cheating
    // (it consumed entitlement and issued garbage) — never feed it a second book.
    if is_flagged(srv) {
        return Err("this server was flagged as dishonest (invalid credential issuance) — \
                    not withdrawing more into it. Switch servers.".into());
    }
    // The coin/date material is cached by `federation_keys` itself and lives in EpochKeys,
    // not in the book — a purse is built from the wallet, the user key, the size and the date.
    let (vk, auth_vks, _coin_sigs, _date_sigs, expiration_date, total_coins) =
        match federation_keys(t, srv, dir, denom_toku, 0).await? {
            FedResponse::Keys {
                vk, auth_vks, coin_sigs, date_sigs, expiration_date, total_coins, ..
            } => (vk, auth_vks, coin_sigs, date_sigs, expiration_date, total_coins),
            FedResponse::Error { message } => return Err(format!("server: {message}")),
            _ => return Err("unexpected response to Keys".into()),
        };
    // M8: `total_coins` is server-supplied and NOT cryptographically bound to the coin
    // material — a hostile server returning u64::MAX would make the next `Parameters::new`
    // allocate O(total_coins) group elements and OOM/hang the client.
    const MAX_BOOK_COINS: u64 = 4096;
    if total_coins == 0 || total_coins > MAX_BOOK_COINS {
        return Err(format!(
            "server returned an implausible ticketbook size ({total_coins}) — refusing to withdraw"
        ));
    }

    // Resume what is outstanding for THIS server and epoch, then top the list up to `want`.
    let mut w = wallet::load(dir);
    let stale = w
        .pending_withdraws
        .iter()
        .filter(|p| p.server == srv && p.expiration_date != expiration_date)
        .count();
    if stale > 0 {
        // The issuing epoch moved on; those bodies can never become usable books. Dropping
        // them is the only way forward — say so loudly, because if the server had charged
        // for one, that book is lost.
        log::error!("[coconut] dropped {stale} interrupted withdrawal(s) whose issuing epoch has passed — if the server had charged for them, those books are lost");
        w.pending_withdraws.retain(|p| p.server != srv || p.expiration_date == expiration_date);
    }
    let mut mine: Vec<usize> = w
        .pending_withdraws
        .iter()
        .enumerate()
        .filter(|(_, p)| p.server == srv)
        .map(|(i, _)| i)
        .collect();
    while mine.len() < want {
        let user = coconut::new_user();
        let (req, req_info) =
            coconut::make_withdrawal_request(user.secret_key(), expiration_date, coconut::DEFAULT_T_TYPE)?;
        w.pending_withdraws.push(wallet::PendingWithdraw {
            server: srv.to_string(),
            denom_toku,
            user: serde_json::to_value(&user).map_err(|e| e.to_string())?,
            req: serde_json::to_value(&req).map_err(|e| e.to_string())?,
            req_info: serde_json::to_value(&req_info).map_err(|e| e.to_string())?,
            expiration_date,
            created_ms: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
        });
        mine.push(w.pending_withdraws.len() - 1);
    }
    if mine.is_empty() {
        return Ok(0);
    }
    // On disk before anything leaves the device.
    wallet::save(dir, &w)?;

    // One envelope per (book, authority), all in flight together — the whole point of
    // drawing several at once is that they share one round trip.
    let mut route: Vec<(String, usize, usize)> = Vec::new(); // (envelope id, book, authority)
    let mut requests: Vec<Value> = Vec::new();
    for (slot, &idx) in mine.iter().enumerate() {
        let p = &w.pending_withdraws[idx];
        let user: coconut::KeyPairUser =
            serde_json::from_value(p.user.clone()).map_err(|e| format!("pending withdrawal: {e}"))?;
        let req: coconut::WithdrawalRequest =
            serde_json::from_value(p.req.clone()).map_err(|e| format!("pending withdrawal: {e}"))?;
        for k in 0..auth_vks.len() {
            let id = format!("{slot}-{k}-{}", rand_hex(8));
            let nonce = rand_hex(16);
            let sig = auth.sign("withdraw:coconut", &nonce);
            requests.push(json!({
                "v": PROTO, "kind": "coconut", "id": id,
                "publicKey": auth.public_key_pem, "nonce": nonce, "sig": sig,
                // The epoch this request was BUILT for: a withdrawal request is bound to
                // its expiration date, so an authority from another epoch would sign
                // something the device cannot unblind.
                "fed": serde_json::to_value(FedRequest::Withdraw { user_pk: user.public_key(), req: req.clone(), denom_toku, expiration_date })
                    .map_err(|e| e.to_string())?,
            }));
            route.push((id, slot, k));
        }
    }
    // One reply per book is a single small packet, and a hundred books go out together —
    // so the SURBs that ride along are the bulk of the request, not the requests.
    let replies = t.round_trip_many(srv, requests, SURBS_ONE_PACKET, TIMEOUT_MS).await?;

    // Group the answers per book, then aggregate the ones that came back complete.
    let mut collected = 0u64;
    let mut done: Vec<usize> = Vec::new();
    let mut fatal: Vec<usize> = Vec::new();
    for (slot, &idx) in mine.iter().enumerate() {
        let p = w.pending_withdraws[idx].clone();
        let user: coconut::KeyPairUser = match serde_json::from_value(p.user) {
            Ok(u) => u,
            Err(_) => continue,
        };
        let req_info: coconut::RequestInfo = match serde_json::from_value(p.req_info) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut shares = Vec::new();
        let mut give_up = false;
        for (id, s, k) in route.iter().filter(|(_, s, _)| *s == slot) {
            let Some(reply) = replies.get(id) else {
                shares.clear();
                break; // no answer for this book — keep it pending and retry later
            };
            // A refusal from the gate (no entitlement, bad signature) arrives as a plain
            // error envelope, not a federation reply. Nothing was charged for it, so the
            // body is dead and carrying it further would only re-ask for the same refusal.
            if let Some(message) = reply.get("error").and_then(|e| e.as_str()) {
                log::info!("[coconut] a book was refused: {message}");
                give_up = !(message.contains("retry") || message.contains("busy"));
                shares.clear();
                break;
            }
            let fed: FedResponse = match serde_json::from_value(reply.get("fed").cloned().unwrap_or(Value::Null)) {
                Ok(f) => f,
                Err(e) => {
                    log::warn!("[coconut] bad fed response: {e}");
                    shares.clear();
                    break;
                }
            };
            let blinded = match fed {
                FedResponse::Withdraw { blinded } => blinded,
                FedResponse::Error { message } => {
                    forget_keys_on_disk(dir, srv);
                    if !(message.contains("retry") || message.contains("busy")) {
                        // Definitive refusal (no entitlement, blacklisted, malformed): this
                        // body will never become a book, so stop carrying it.
                        give_up = true;
                    }
                    log::info!("[coconut] server refused a book: {message}");
                    shares.clear();
                    break;
                }
                _ => {
                    shares.clear();
                    break;
                }
            };
            let _ = s;
            match coconut::verify_share(&auth_vks[*k], user.secret_key(), &blinded, &req_info, *k as u64 + 1) {
                Ok(share) => shares.push(share),
                Err(e) => {
                    // A well-formed reply that fails here means the operator consumed
                    // entitlement and returned garbage (H3).
                    forget_keys_on_disk(dir, srv);
                    flag_server(srv, &format!("invalid withdrawal share: {e}"));
                    give_up = true;
                    shares.clear();
                    break;
                }
            }
        }
        if give_up {
            fatal.push(idx);
            continue;
        }
        if shares.len() != auth_vks.len() {
            continue; // incomplete — stays pending
        }
        let wallet_cred = match coconut::aggregate(&vk, user.secret_key(), &shares, &req_info) {
            Ok(c) => c,
            Err(e) => {
                forget_keys_on_disk(dir, srv);
                flag_server(srv, &format!("credential shares don't aggregate: {e}"));
                fatal.push(idx);
                continue;
            }
        };
        let purse = scrai_core::purse::Purse::new(wallet_cred, user, total_coins, expiration_date, denom_toku);
        // Persist each book as it lands: a dropped connection loses nothing.
        let mut w2 = wallet::load(dir);
        w2.coconut_purses.push(purse.persist()?);
        w2.pending_withdraws.retain(|q| q.req != w.pending_withdraws[idx].req);
        wallet::save(dir, &w2)?;
        collected += total_coins * denom_toku;
        done.push(idx);
    }
    if !fatal.is_empty() {
        let mut w2 = wallet::load(dir);
        let drop: Vec<Value> = fatal.iter().map(|i| w.pending_withdraws[*i].req.clone()).collect();
        w2.pending_withdraws.retain(|q| !drop.contains(&q.req));
        wallet::save(dir, &w2)?;
    }
    log::info!("[coconut] {} of {} book(s) landed ({collected} TOKU)", done.len(), mine.len());
    if collected == 0 && done.is_empty() && !fatal.is_empty() {
        return Err("the server refused to issue a ticketbook — see the log for why".into());
    }
    Ok(collected)
}



/// The wallet's account, needed wherever a request must be account-signed.
fn wallet_account(app: &AppHandle) -> Result<account::Account, String> {
    let w = wallet::load(&data_dir(app)?);
    let m = w.mnemonic.ok_or("no account — create one first")?;
    account::from_mnemonic(&m)
}

// M4: `coconut_withdraw_test` / `coconut_withdraw` / `coconut_spend` were registered
// Tauri commands with zero callers (backend.js exposes only collect/redeem/chat) that,
// unlike the live money paths, skipped `begin_op()` — a live spend/withdraw surface that
// an XSS frontend could race against chat's auto-redeem and clobber a purse. Removed. The
// live buy→coins path is `collect` (withdraws via `withdraw_purse`) and `redeem`.

// ---------------------------------------------------------------------------------------
// Paying a chat with coins (docs/unlinkability.md, block D). No session, no counter, no
// signature: the request carries a tender — several payments valued 1, 2, 4, … — and the
// server burns exactly the notes the answer cost. The rest come home and pay for the next
// one, so over-tendering is free apart from the bytes.
//
// Off by default while the fleet still runs the session path; `TOKUMAI_COIN_CHAT=1` turns
// it on for testing against a server that already accepts tenders.
// ---------------------------------------------------------------------------------------

// A payment costs about 490 bytes and 4 ms of server CPU per coin, and a request has to
// tender its CEILING rather than its cost — so these bounds are what keeps an ordinary
// chat request small. They are amounts, not coin counts, so they survive a change of
// denomination: a 0.8 ¢ floor (≈ 4 KB) and a 25 ¢ ceiling (≈ 125 KB, enough for a 4K
// picture).
const TENDER_MIN_TOKU: u64 = 800;
const TENDER_MAX_TOKU: u64 = 25_000;

fn coin_chat_enabled(dir: &Path) -> bool {
    dev_env("COIN_CHAT").as_deref() == Some("1") || !wallet::load(dir).legacy_session_chat
}

/// What this request could cost at worst, in TOKU — the size of the tender. In TOKU and
/// not in coins, because coins come in two sizes now (`coconut::DENOMS`).
///
/// The formula is `scrai_core::billing::ceiling_toku`, the SAME one the server reserves
/// with. It used to be re-derived here from `compute_billing` with a text-shaped usage,
/// which ignored the picture an image model draws: a 4368 TOKU image request went out
/// with 1300 TOKU on the table and was refused while the device held 187 coins
/// (2026-09-14). Anything that changes the ceiling must change it in core, for both.
fn tender_ceiling_toku(
    model: &str,
    messages: &Value,
    max_tokens: Option<u64>,
    thinking: Option<u64>,
    image_size: Option<&str>,
) -> u64 {
    use scrai_core::billing::{ceiling_input_tokens, ceiling_toku, image_tokens_for, per_image_toku, Ceiling, DEFAULT_IMAGE_SIZE};
    use scrai_core::coconut::COIN_TOKU;
    use scrai_core::pricing::PricingTable;
    let est = (|| {
        let table = PricingTable::parse(BUNDLED_PRICING).ok()?;
        let price = table.price(model);
        if price.fallback {
            return None;
        }
        let c = Ceiling {
            in_tokens: ceiling_input_tokens(messages),
            // A request that names no thinking budget gets the server's own
            // (THINKING_BUDGET, 2048 by default); the headroom below covers a server set
            // higher. Live grounding adds nothing while the monthly free allowance holds,
            // which is the only case we have ever been in — if that changes, the ceiling
            // grows server-side and this estimate has to learn about it.
            out_tokens: max_tokens.unwrap_or(4096) + thinking.unwrap_or(2048),
            image_tokens: image_tokens_for(image_size.unwrap_or(DEFAULT_IMAGE_SIZE)),
        };
        Some(ceiling_toku(&price, CLIENT_RETAIL_MARGIN, &c) + per_image_toku(&price, CLIENT_RETAIL_MARGIN))
    })()
    .unwrap_or(0);
    // Half again on top: the server's margin may be higher than the one bundled here, and
    // an answer capped for want of one coin is worse than a few notes that come home.
    let toku = est.saturating_mul(3) / 2;
    // Rounded to a whole fine coin: nothing finer can be put on the table.
    let toku = toku.clamp(TENDER_MIN_TOKU, TENDER_MAX_TOKU);
    toku.div_ceil(COIN_TOKU).saturating_mul(COIN_TOKU)
}

/// Assemble a tender worth at least `ceiling_toku`.
///
/// THE PACKING. A payment comes out of ONE book, so a note can never be worth more than
/// the book it came from, and a tender may carry at most `MAX_NOTES` of them. Two things
/// therefore have to be true at once: enough VALUE on the table, and enough GRANULARITY
/// for the server to burn the exact price rather than overshoot onto a whole note.
///
/// Value comes from the coarse books — one note per book, worth up to ten cents.
/// Granularity comes from ONE finely planned fine book: 1, 2, 4, 3 coins covers every
/// tenth of a cent below a whole cent. A 9 ¢ ceiling is then one coarse note plus four
/// fine ones — five notes, nineteen coins — where a single denomination needed ninety
/// coins and could not be carried at all (2026-09-14/15).
///
/// Spare notes from earlier tenders are spent first, but only while the notes still needed
/// for the rest would fit beside them: slots are the scarce thing, not coins.
fn build_tender(
    w: &mut wallet::Wallet,
    keys: &[scrai_core::purse::EpochKeys],
    ceiling_toku: u64,
) -> Result<scrai_core::tender::Tender, String> {
    use scrai_core::coconut::{COARSE_TOKU, COIN_TOKU};
    use scrai_core::tender::{plan_coins, Note, Tender, MAX_NOTES};

    // For SIZING, the newest epoch of a denomination: that is what a fresh book will be.
    let keys_for = |denom: u64| {
        keys.iter().filter(|k| k.denom_toku == denom).max_by_key(|k| k.expiration_date)
    };
    // For SPENDING, the material of the epoch a book was actually issued in — a device can
    // hold books from several at once, and the wrong epoch's keys only make a payment no
    // server accepts (`EpochKeys::fits`).
    let keys_of = |p: &scrai_core::purse::Purse| keys.iter().find(|k| k.fits(p));
    let fine_book_coins = keys_for(COIN_TOKU).map(|k| k.total_coins).unwrap_or(0);
    let coarse_book_toku = keys_for(COARSE_TOKU).map(|k| k.book_toku()).unwrap_or(0);
    // How far the fine plan has to reach: just under ONE coarse coin. Anything above that
    // is cheaper to pay with a coarse note, so planning a whole fine book would spend
    // coins — and bytes — on granularity nobody can use. Without a coarse denomination the
    // fine book is all there is, and then it does have to cover itself.
    let fine_span_coins = if coarse_book_toku > 0 {
        (COARSE_TOKU / COIN_TOKU).min(fine_book_coins)
    } else {
        fine_book_coins
    };
    let fine_slots = if fine_span_coins > 0 { plan_coins(fine_span_coins).len() } else { 0 };

    let mut spares: Vec<Note> = Vec::new();
    for v in std::mem::take(&mut w.spare_notes) {
        match serde_json::from_value::<Note>(v) {
            Ok(n) => spares.push(n),
            // A note we can no longer parse is a note we can never spend; dropping it
            // loses at most its face value, keeping it would poison every tender.
            Err(e) => log::warn!("[tender] dropping an unreadable spare note: {e}"),
        }
    }
    spares.sort_by(|a, b| b.value_toku().cmp(&a.value_toku()));
    let mut notes: Vec<Note> = Vec::new();
    let mut keep: Vec<Note> = Vec::new();
    let mut have = 0u64;
    // What the REST of the ceiling would still cost in notes, at the coarsest book this
    // device actually has. Without the fallback to the fine book this was zero whenever no
    // coarse book had been drawn — and then small spares filled the table again, which is
    // how an image edit tendered 3,200 TOKU with 11,000 on the device (2026-09-15).
    let bulk_book_toku = if coarse_book_toku > 0 {
        COARSE_TOKU
    } else if fine_book_coins > 0 {
        fine_book_coins * COIN_TOKU
    } else {
        0
    };
    for n in spares {
        let rest = ceiling_toku.saturating_sub(have + n.value_toku());
        let bulk_after = if bulk_book_toku > 0 { plan_coins(rest.div_ceil(bulk_book_toku)).len() } else { 0 };
        if have < ceiling_toku && notes.len() + 1 + bulk_after + fine_slots <= MAX_NOTES {
            have += n.value_toku();
            notes.push(n);
        } else {
            keep.push(n);
        }
    }
    // Spares alone can cover it — then their own values are all the granularity there is,
    // so spend what is left of the budget on the SMALLEST of them.
    if have >= ceiling_toku {
        keep.sort_by(|a, b| a.value_toku().cmp(&b.value_toku()));
        while notes.len() < MAX_NOTES && !keep.is_empty() {
            notes.push(keep.remove(0));
        }
    }
    w.spare_notes = keep.iter().filter_map(|n| serde_json::to_value(n).ok()).collect();

    // --- the bulk, from the coarsest books first -----------------------------------
    //
    // A note is atomic: the server burns it whole or not at all. So the coarse side needs
    // the SAME 1, 2, 4, … plan as the fine one — one note of nine coarse coins can pay 9 ¢
    // and nothing else, while 1+2+4+2 pays every whole cent up to nine. The plan is made
    // once for the whole amount and handed out across books, so several books still cost
    // only as many notes as the plan has values (2026-09-15).
    for denom in scrai_core::coconut::DENOMS {
        if denom == COIN_TOKU {
            continue; // the fine side is the granularity plan below
        }
        if keys_for(denom).is_none() {
            continue;
        }
        if have >= ceiling_toku {
            break;
        }
        let want_coins = (ceiling_toku - have).div_ceil(denom);
        for v in plan_coins(want_coins) {
            if notes.len() + 1 + fine_slots > MAX_NOTES {
                break;
            }
            // Oldest book first: a coin that has been on the device longest is the one
            // whose purchase is furthest away in time. A value has to come out of ONE
            // book, so take it from the first that can cover it.
            let Some((idx, mut purse)) = w
                .coconut_purses
                .iter()
                .enumerate()
                .find_map(|(i, pj)| {
                    let p = scrai_core::purse::Purse::restore(pj).ok()?;
                    (p.denom_toku() == denom && p.remaining_coins() >= v && keys_of(&p).is_some()).then_some((i, p))
                })
            else {
                break;
            };
            let Some(k) = keys_of(&purse) else { break };
            let spend_date = purse.expiration_date().saturating_sub(86_400);
            let mut fresh = purse.spend_tender(k, &[v], spend_date)?;
            notes.append(&mut fresh);
            have += v * denom;
            let emptied = purse.remaining_coins() == 0;
            w.coconut_purses[idx] = purse.persist()?;
            if emptied {
                w.coconut_purses.remove(idx);
            }
        }
    }

    // --- the granularity: one finely planned fine book ------------------------------
    // Without it the server could only burn whole coarse notes, and every answer would be
    // rounded up to the cent. Only when it is actually MISSING, though: notes already on
    // the table that are worth less than a coarse coin are granularity too, and minting a
    // fresh plan beside them would spend coins nobody needs (a spare-covered tender used
    // to reach into a book for it anyway).
    let small_value: u64 = notes.iter().map(|n| n.value_toku()).filter(|v| *v < COARSE_TOKU).sum();
    let need_granularity = small_value < COARSE_TOKU.saturating_sub(COIN_TOKU);
    if need_granularity && fine_span_coins > 0 && notes.len() + fine_slots <= MAX_NOTES {
        if let Some((idx, mut purse)) = first_funded_purse_of(&w.coconut_purses, COIN_TOKU) {
            // No material for that book's epoch: it cannot be spent here. Leave it alone
            // and carry on with what the tender already has.
            let k = keys_of(&purse);
            if k.is_none() {
                log::warn!("[tender] a fine book's epoch material is not on this device — skipping it");
            }
            if let Some(k) = k {
            let coins = purse.remaining_coins().min(fine_span_coins);
            let spend_date = purse.expiration_date().saturating_sub(86_400);
            let mut fresh = purse.spend_tender(k, &plan_coins(coins), spend_date)?;
            notes.append(&mut fresh);
            have += coins * COIN_TOKU;
            let emptied = purse.remaining_coins() == 0;
            w.coconut_purses[idx] = purse.persist()?;
            if emptied {
                w.coconut_purses.remove(idx);
            }
            }
        }
    }

    // --- still short? then there are no coarse books, only fine ones --------------------
    // Same plan, one book at a time, until the ceiling is covered or the slots run out.
    while have < ceiling_toku && notes.len() < MAX_NOTES {
        let Some((idx, mut purse)) = first_funded_purse_of(&w.coconut_purses, COIN_TOKU) else { break };
        let Some(k) = keys_of(&purse) else { break };
        let coins = (ceiling_toku - have).div_ceil(COIN_TOKU).min(purse.remaining_coins());
        if coins == 0 || notes.len() + 1 > MAX_NOTES {
            break;
        }
        let spend_date = purse.expiration_date().saturating_sub(86_400);
        let mut fresh = purse.spend_tender(k, &[coins], spend_date)?;
        notes.append(&mut fresh);
        have += coins * COIN_TOKU;
        let emptied = purse.remaining_coins() == 0;
        w.coconut_purses[idx] = purse.persist()?;
        if emptied {
            w.coconut_purses.remove(idx);
        }
    }

    if notes.is_empty() {
        return Err("no TOKU credit — buy credit first".into());
    }
    Ok(Tender { notes })
}

/// How many books of one denomination still hold coins.
fn books_of_denom(w: &wallet::Wallet, denom_toku: u64) -> usize {
    w.coconut_purses
        .iter()
        .filter_map(|j| scrai_core::purse::Purse::restore(j).ok())
        .filter(|p| p.denom_toku() == denom_toku && p.remaining_coins() > 0)
        .count()
}

/// One batch of coins out of the books that are about to expire: everything left in them,
/// a whole book per note (a note is cheapest per coin that way), bounded by what one
/// `coins.return` may carry. `None` when nothing is close enough to its date.
///
/// The purses are advanced here, so the caller MUST persist the wallet before the request
/// leaves the device — same durability contract as every other spend.
fn drain_expiring(
    w: &mut wallet::Wallet,
    keys: &[scrai_core::purse::EpochKeys],
    now: u32,
) -> Result<Option<scrai_core::tender::Tender>, String> {
    use scrai_core::tender::{Tender, MAX_NOTES};
    let deadline = now.saturating_add((SWAP_WINDOW_DAYS * 86_400) as u32);
    let mut notes = Vec::new();
    let mut coins = 0u64;
    loop {
        if notes.len() >= MAX_NOTES || coins >= MAX_RETURN_COINS {
            break;
        }
        let Some((idx, mut purse)) = w.coconut_purses.iter().enumerate().find_map(|(i, pj)| {
            let p = scrai_core::purse::Purse::restore(pj).ok()?;
            (p.expiration_date() <= deadline && p.remaining_coins() > 0).then_some((i, p))
        }) else {
            break;
        };
        let Some(k) = keys.iter().find(|k| k.fits(&purse)) else {
            // No material for its epoch: it cannot be spent, so it cannot be swapped
            // either. Drop it rather than looping on it for ever — it is already dead.
            log::warn!("[swap] a book's epoch material is gone; it cannot be handed back");
            w.coconut_purses.remove(idx);
            continue;
        };
        let take = purse.remaining_coins().min(MAX_RETURN_COINS - coins);
        let spend_date = purse.expiration_date().saturating_sub(86_400);
        let mut fresh = purse.spend_tender(k, &[take], spend_date)?;
        notes.append(&mut fresh);
        coins += take;
        let emptied = purse.remaining_coins() == 0;
        w.coconut_purses[idx] = purse.persist()?;
        if emptied {
            w.coconut_purses.remove(idx);
        }
    }
    Ok((!notes.is_empty()).then_some(Tender { notes }))
}

/// The oldest book of one denomination that still holds coins.
fn first_funded_purse_of(purses: &[String], denom_toku: u64) -> Option<(usize, scrai_core::purse::Purse)> {
    purses.iter().enumerate().find_map(|(i, pj)| {
        let p = scrai_core::purse::Purse::restore(pj).ok()?;
        (p.denom_toku() == denom_toku && p.remaining_coins() > 0).then_some((i, p))
    })
}


/// Apply a server's verdict: the notes it named are gone, everything else goes back into
/// the wallet as spares. Returns how many coins were burned.
fn keep_unburned(w: &mut wallet::Wallet, notes: &[scrai_core::tender::Note], burned: &[usize]) -> u64 {
    let mut spent = 0u64;
    for (i, n) in notes.iter().enumerate() {
        if burned.contains(&i) {
            spent += n.value_toku();
            continue;
        }
        match serde_json::to_value(n) {
            Ok(v) => w.spare_notes.push(v),
            Err(e) => log::warn!("[tender] could not keep an unburned note: {e}"),
        }
    }
    spent
}

/// Coins lying on this device: unspent book coins plus notes already taken out of a book.
/// What the coins on this device are worth. Books come in two denominations, and every
/// book and note carries its own, so the value is summed — never counted and multiplied.
fn coin_value_toku(w: &wallet::Wallet) -> u64 {
    let in_books: u64 = w
        .coconut_purses
        .iter()
        .filter_map(|j| scrai_core::purse::Purse::restore(j).ok())
        .map(|p| p.remaining_toku())
        .sum();
    let in_notes: u64 = w
        .spare_notes
        .iter()
        .filter_map(|v| serde_json::from_value::<scrai_core::tender::Note>(v.clone()).ok())
        .map(|n| n.value_toku())
        .sum();
    // Coins on the table for a question that has not been answered yet are still the
    // user's: all but the few the answer costs come home (2026-09-14).
    let in_flight: u64 = w
        .pending_tenders
        .iter()
        .flat_map(|p| p.notes.iter())
        .filter_map(|v| serde_json::from_value::<scrai_core::tender::Note>(v.clone()).ok())
        .map(|n| n.value_toku())
        .sum();
    in_books + in_notes + in_flight
}

/// Build the coin-paid chat request — or resume the one that never got an answer. The
/// notes are spent out of the purse HERE, so the wallet is persisted before returning:
/// a crash between this and the send leaves the tender recorded, never the coins in limbo.
#[allow(clippy::too_many_arguments, non_snake_case)]
fn coin_request(
    dir: &std::path::Path,
    srv: &str,
    model: &str,
    messages: &Value,
    maxTokens: Option<u64>,
    live: Option<bool>,
    thinkingBudget: Option<u64>,
    imageSize: &Option<String>,
    resume: bool,
) -> Result<(Value, Vec<scrai_core::tender::Note>, bool), String> {
    let mut w = wallet::load(dir);
    // A RETRY re-sends the unanswered tender VERBATIM. Its coins are already spent, so
    // building a fresh request would pay twice for one answer; the server replays from
    // its cache. A NEW question must never take this path: the replay cache is keyed on
    // the tender, so re-sending it would answer the new question with the old answer —
    // which is exactly what happened on 2026-09-14, when "test" came back as a picture.
    if resume {
        if let Some(pt) = w.pending_tenders.last().cloned() {
        let notes: Vec<scrai_core::tender::Note> = pt
            .notes
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
        if notes.len() == pt.notes.len() {
            return Ok((pt.request, notes, true));
        }
            log::warn!("[tender] a pending tender could not be read back — starting a fresh one");
            w.pending_tenders.pop();
        }
    }
    let keys = all_epoch_keys(dir, srv);
    if keys.is_empty() {
        return Err("the server's issuing keys are not on this device yet — check for credit first".into());
    }
    let tender = build_tender(&mut w, &keys, tender_ceiling_toku(model, messages, maxTokens, thinkingBudget, imageSize.as_deref()))?;
    let mut req = json!({
        "v": PROTO, "kind": "chat", "id": rand_hex(16), "model": model, "messages": messages,
        "stream": false, "chunkedImages": true,
        "tender": serde_json::to_value(&tender).map_err(|e| e.to_string())?,
    });
    if let Some(mt) = maxTokens {
        req["maxTokens"] = json!(mt);
    }
    if live.unwrap_or(false) {
        req["live"] = json!(true);
    }
    if let Some(tb) = thinkingBudget {
        req["thinkingBudget"] = json!(tb);
    }
    if let Some(sz) = imageSize {
        req["imageSize"] = json!(sz);
    }
    // Size and shape of what goes on the wire, so a hang can be told apart from a refusal
    // without guessing (2026-09-14: an image request was blamed on gateway bandwidth
    // before the log showed what it actually weighed). At WARN because a release build —
    // which is what every phone runs — keeps warn and above, and this is exactly the
    // number one wants when a request does not arrive.
    log::warn!(
        "[tender] {} notes, {} coins, {} TOKU, request {} bytes",
        tender.notes.len(),
        tender.notes.iter().map(|n| n.coins).sum::<u64>(),
        tender.total_toku(),
        serde_json::to_vec(&req).map(|v| v.len()).unwrap_or(0)
    );
    w.pending_tenders.push(wallet::PendingTender {
        request: req.clone(),
        notes: tender
            .notes
            .iter()
            .map(|n| serde_json::to_value(n).unwrap_or(Value::Null))
            .collect(),
    });
    wallet::save(dir, &w)?;
    Ok((req, tender.notes, false))
}

/// Close any tender left unanswered by an earlier question — a cancelled request, or one
/// whose reply was lost. Each is re-sent VERBATIM: the server either replays the answer it
/// already gave or serves it now, and either way tells us which notes it burned, so the
/// rest come home. Best effort — one attempt each, and a tender that still gets no reply
/// stays pending for the next try rather than being written off.
async fn settle_pending_tenders(app: &AppHandle, transport: &Transport, srv: &str) {
    let Ok(dir) = data_dir(app) else { return };
    let pending = wallet::load(&dir).pending_tenders;
    for pt in pending {
        let Some(id) = pt.request.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()) else { continue };
        let notes: Vec<scrai_core::tender::Note> =
            pt.notes.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect();
        if notes.len() != pt.notes.len() {
            log::warn!("[tender] a pending tender could not be read back — dropping it");
            let mut w = wallet::load(&dir);
            w.pending_tenders.retain(|p| p.request.get("id").and_then(|i| i.as_str()) != Some(id.as_str()));
            let _ = wallet::save(&dir, &w);
            continue;
        }
        log::info!("[tender] settling an unanswered tender before the new question");
        match transport.round_trip_raw_notify(srv, &pt.request, SURBS_TEXT, SETTLE_TIMEOUT_MS, || {}).await {
            Ok(reply) => {
                if let Err(e) = coin_settle(&dir, &id, &notes, &reply) {
                    log::warn!("[tender] settling failed: {e}");
                }
            }
            Err(e) => log::warn!("[tender] an unanswered tender is still unanswered: {e}"),
        }
    }
}

/// The server answered: put the notes it did NOT burn back in the wallet and close the
/// retry window. `burned` absent (an error reply) means nothing was burned at all.
fn coin_settle(dir: &std::path::Path, req_id: &str, notes: &[scrai_core::tender::Note], resp: &Value) -> Result<(), String> {
    let burned: Vec<usize> = resp
        .get("burned")
        .and_then(|b| serde_json::from_value(b.clone()).ok())
        .unwrap_or_default();
    let mut w = wallet::load(dir);
    let spent = keep_unburned(&mut w, notes, &burned);
    // Only the tender this reply answers: another one may still be outstanding.
    w.pending_tenders.retain(|p| p.request.get("id").and_then(|i| i.as_str()) != Some(req_id));
    wallet::save(dir, &w)?;
    log::info!("[tender] {spent} TOKU burned, {} note(s) kept", notes.len() - burned.len());
    Ok(())
}

/// Coins on this device, in coins (books plus notes already taken out of one).
fn coins_on_device(w: &wallet::Wallet) -> u64 {
    let in_books: u64 = w
        .coconut_purses
        .iter()
        .filter_map(|j| scrai_core::purse::Purse::restore(j).ok())
        .map(|p| p.remaining_coins())
        .sum();
    let in_notes: u64 = w
        .spare_notes
        .iter()
        .filter_map(|v| serde_json::from_value::<scrai_core::tender::Note>(v.clone()).ok())
        .map(|n| n.coins)
        .sum();
    // Coins on the table for a question that has not been answered yet are still the
    // user's: all but the few the answer costs come home. Leaving them out made the
    // balance drop by the whole tender and jump back on the reply — "suddenly I have
    // 8.9k instead of 5.5k" (2026-09-14). Money in flight is shown, not hidden.
    let in_flight: u64 = w
        .pending_tenders
        .iter()
        .flat_map(|p| p.notes.iter())
        .filter_map(|v| serde_json::from_value::<scrai_core::tender::Note>(v.clone()).ok())
        .map(|n| n.coins)
        .sum();
    in_books + in_notes + in_flight
}


/// Hand every unspent coin on this device back to the account, where it becomes
/// entitlement again — the way to move to another device, or to empty one before giving
/// it away. Coins are bearer money: they live only here, and a recovery phrase does not
/// bring them back, so this is the only way to make them survive the device.
///
/// Batched, because a payment costs ~490 bytes and ~4 ms of server pairings per coin: a
/// whole book goes home in several requests. Each batch is persisted before it leaves and
/// re-sent verbatim until the server answers, so a lost reply can never lose the coins.
#[tauri::command]
async fn coins_return(app: AppHandle, transport: State<'_, Arc<Transport>>) -> Result<Value, String> {
    let t = transport.inner().clone();
    let _op = t.begin_op().await;
    return_coins(app, t.clone(), Returning::Everything).await
}

/// What a return is for: emptying the device, or only handing back what is about to expire.
#[derive(Clone, Copy, PartialEq)]
enum Returning {
    Everything,
    Expiring,
}

async fn return_coins(app: AppHandle, transport: Arc<Transport>, what: Returning) -> Result<Value, String> {
    // The server bounds a return by COINS (its pairings cost per coin, MAX_RETURN_COINS),
    // so the batch is sized for the worst case: all of it in fine coins. Coarse books make
    // a batch worth ten times more without costing the server any more work.
    const BATCH_TOKU: u64 = MAX_RETURN_COINS * scrai_core::coconut::COIN_TOKU;
    // NO `begin_op` here: the caller already holds it. It is a plain mutex, so taking it
    // twice on one transport deadlocks rather than waits — which is exactly what the swap
    // inside `collect_now` did on 2026-09-15. "Check for credit" hung for ever and wrote
    // no log line, because a deadlock has nothing to report.
    let dir = data_dir(&app)?;
    let srv = server_addr(&wallet::load(&dir))?;
    let a = wallet_account(&app)?;
    let t = buy_transport(&app, &transport).await;
    let mut credited_total = 0u64;
    let mut entitlement = 0u64;
    // What this will take, in coins, so the app can show how far along it is: each batch
    // is a mixnet round trip plus ~4 ms of server pairings per coin, so a full device can
    // take half a minute and must not look frozen.
    let total_coins = coins_on_device(&wallet::load(&dir)).max(1);
    let mut done_coins = 0u64;
    let progress = |done: u64, credited: u64| {
        let _ = app.emit(
            "coins-return",
            json!({ "coins": done, "total": total_coins, "credited": credited,
                    "percent": (done.saturating_mul(100) / total_coins).min(100) }),
        );
    };
    progress(0, 0);
    loop {
        let mut w = wallet::load(&dir);
        // Finish an unanswered batch before building another one.
        let (req, notes) = match w.pending_return.clone() {
            Some(p) => {
                let notes: Vec<scrai_core::tender::Note> =
                    p.notes.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect();
                if notes.len() != p.notes.len() {
                    return Err("a pending return could not be read back — please report this".into());
                }
                (p.request, notes)
            }
            None => {
                let left = coin_value_toku(&w);
                if left == 0 {
                    break;
                }
                let keys = all_epoch_keys(&dir, &srv);
                if keys.is_empty() {
                    return Err("the server's issuing keys are not on this device yet — check for credit first".into());
                }
                let tender = match what {
                    Returning::Everything => build_tender(&mut w, &keys, left.min(BATCH_TOKU))?,
                    // The swap hands back WHOLE books that are near their date, so it takes
                    // them as they are rather than planning a tender out of the wallet.
                    Returning::Expiring => match drain_expiring(&mut w, &keys, now_secs32())? {
                        Some(t) => t,
                        None => break,
                    },
                };
                let nonce = rand_hex(16);
                let sig = a.sign("return", &nonce);
                let req = json!({
                    "v": PROTO, "kind": "coins.return", "id": rand_hex(16),
                    "publicKey": a.public_key_pem, "nonce": nonce, "sig": sig,
                    "tender": serde_json::to_value(&tender).map_err(|e| e.to_string())?,
                });
                w.pending_return = Some(wallet::PendingTender {
                    request: req.clone(),
                    notes: tender.notes.iter().map(|n| serde_json::to_value(n).unwrap_or(Value::Null)).collect(),
                });
                wallet::save(&dir, &w)?;
                (req, tender.notes)
            }
        };
        let reply = t.round_trip(&srv, &req, SURBS_SMALL, TIMEOUT_MS).await?;
        let mut w = wallet::load(&dir);
        w.pending_return = None;
        if let Some(e) = reply.get("error").and_then(|e| e.as_str()) {
            // Nothing was burned — the coins are still good, so they go back in the wallet
            // rather than being thrown away with the failed batch.
            keep_unburned(&mut w, &notes, &[]);
            wallet::save(&dir, &w)?;
            return Err(e.to_string());
        }
        // Answered: these coins are spent, whatever the credited number says (a retry of a
        // batch the server already took credits 0 and reports the same entitlement).
        wallet::save(&dir, &w)?;
        credited_total += reply.get("credited").and_then(|c| c.as_u64()).unwrap_or(0);
        entitlement = reply.get("entitlement").and_then(|c| c.as_u64()).unwrap_or(entitlement);
        done_coins += notes.iter().map(|n| n.coins).sum::<u64>();
        progress(done_coins, credited_total);
    }
    drop(t);
    close_buy_link(&app).await;
    {
        let mut w = wallet::load(&dir);
        w.entitlement_seen = entitlement;
        let _ = wallet::save(&dir, &w);
    }
    log::info!("[tender] returned {credited_total} TOKU to the account");
    Ok(json!({ "credited": credited_total, "entitlement": entitlement, "held": coconut_held_toku(&app) }))
}



fn coconut_held_toku(app: &AppHandle) -> u64 {
    let Ok(dir) = data_dir(app) else { return 0 };
    coin_value_toku(&wallet::load(&dir))
}


// ---- commands -------------------------------------------------------------

/// What the wallet knows WITHOUT touching the mixnet: server, account, held credit.
/// The UI reads this first at launch (and whenever the network `state` call fails), so a
/// slow or failed connect can never make the app claim "no server / no account" — that
/// wrong claim once led a user to "save" an empty server over a good one.
#[tauri::command]
fn local_state(app: AppHandle) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let w = wallet::load(&dir);
    let account = match &w.mnemonic {
        Some(m) => {
            let a = account::from_mnemonic(m)?;
            json!({ "fingerprint": account::fingerprint(&a.account_id), "sessionIndex": w.session_index, "phraseVerified": w.phrase_verified })
        }
        None => Value::Null,
    };
    Ok(json!({
        "account": account,
        "server": server_addr(&w).ok(),
        "held": coconut_held_toku(&app),
    }))
}

#[tauri::command]
async fn state(app: AppHandle, transport: State<'_, Arc<Transport>>) -> Result<Value, String> {
    diag(&app, "state: begin");
    let dir = data_dir(&app)?;
    let w = wallet::load(&dir);

    let account = match &w.mnemonic {
        Some(m) => {
            let a = account::from_mnemonic(m)?;
            json!({ "fingerprint": account::fingerprint(&a.account_id), "sessionIndex": w.session_index, "phraseVerified": w.phrase_verified })
        }
        None => Value::Null,
    };
    let server = server_addr(&w).ok();
    let mut models = json!([]);

    if let Some(srv) = &server {
        // Assume the server answers until a fetch below says otherwise.
        // Models: cached after first fetch. META_TIMEOUT_MS, not the chat TIMEOUT_MS:
        // a catalogue reply is small and normally takes seconds — waiting the full
        // 120 s here is what once delayed the "server unreachable" verdict to ~4 min.
        if let Some(cached) = transport.cached_models().await {
            models = cached;
        } else {
            match transport
                .round_trip(srv, &json!({"v":PROTO,"kind":"models","id":rand_hex(16)}), SURBS_META, META_TIMEOUT_MS)
                .await
            {
                Err(_) => {}
                Ok(resp) => {
                    if let Some(m) = resp.get("models") {
                        models = m.clone();
                        transport.set_cached_models(m.clone()).await;
                    }
                    // The server's other front doors — remembered as fallbacks, but only
                    // when the address we use is among them (a custom server's list is its
                    // own; never let one server hand us another's addresses).
                    if let Some(ids) = resp.get("identities").and_then(|i| i.as_array()) {
                        let ids: Vec<String> = ids
                            .iter()
                            .filter_map(|a| a.as_str())
                            .filter(|a| Transport::validate_address(a).is_ok())
                            .map(str::to_string)
                            .collect();
                        if ids.iter().any(|a| a == srv) {
                            let dir = data_dir(&app)?;
                            let mut w2 = wallet::load(&dir);
                            if w2.server_alternates != ids {
                                w2.server_alternates = ids;
                                let _ = wallet::save(&dir, &w2);
                            }
                        }
                    }
                    remember_iap_products(&resp);
                    {
                        let mut u = SERVER_UPDATE.lock().unwrap_or_else(|e| e.into_inner());
                        *u = resp.get("update").filter(|u| u.get("required").and_then(|r| r.as_bool()) == Some(true)).cloned();
                    }
                    // Testnet servers advertise the $1 faucet purchase; remembered with the models
                    // so the flag survives the cache (no extra round trip on later `state` calls).
                    {
                        let mut t = SERVER_TESTNET.lock().unwrap_or_else(|e| e.into_inner());
                        *t = (
                            resp.get("testnet").and_then(|b| b.as_bool()).unwrap_or(false),
                            resp.get("faucetUrl").and_then(|u| u.as_str()).map(str::to_string),
                        );
                    }
                    {
                        let mut u = SERVER_SITE.lock().unwrap_or_else(|e| e.into_inner());
                        // Older servers send no siteUrl — fall back to faucetUrl so a
                        // testnet box keeps working before it is redeployed.
                        *u = resp.get("siteUrl").and_then(|u| u.as_str())
                            .or_else(|| resp.get("faucetUrl").and_then(|u| u.as_str()))
                            .map(str::to_string);
                    }
                    {
                        let mut c = SERVER_CARD.lock().unwrap_or_else(|e| e.into_inner());
                        *c = resp.get("card").filter(|c| c.is_object()).cloned();
                    }
                    {
                        let mut r = SERVER_RAILS.lock().unwrap_or_else(|e| e.into_inner());
                        *r = resp.get("rails").filter(|r| r.is_object()).cloned();
                    }
                    {
                        let mut c = SERVER_COINS.lock().unwrap_or_else(|e| e.into_inner());
                        *c = resp.get("coins").filter(|c| c.is_array()).cloned();
                    }
                    {
                        let mut v = SERVER_VERSION.lock().unwrap_or_else(|e| e.into_inner());
                        *v = resp.get("serverVersion").and_then(|s| s.as_str()).map(|s| s.chars().take(32).collect());
                    }
                }
            }
        }
    }

    // The balance IS the coins on this device. There is no server-side balance any more —
    // no call to make, and nothing the server could tell us about what we hold.
    let balance = coconut_held_toku(&app);
    let (testnet, faucet_url) = SERVER_TESTNET.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let update = SERVER_UPDATE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let card = SERVER_CARD.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let rails = SERVER_RAILS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let coins = SERVER_COINS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let server_version = SERVER_VERSION.lock().unwrap_or_else(|e| e.into_inner()).clone();

    let out = json!({
        // Developer diagnostics (cost audit, upload readout, dev dials) exist only in a
        // debug build — a shipped binary never shows the Developer section.
        "devBuild": cfg!(debug_assertions),
        "coinChat": !w.legacy_session_chat,
        "appVersion": app_version(),
        "serverVersion": server_version,
        "storefront": ios_storefront(),
        "update": update,
        "account": account,
        "balance": balance,
        "held": coconut_held_toku(&app),
        // Paid for, not yet drawn as coins. Below one ticketbook it cannot be drawn at
        // all, so it has to be named rather than silently missing from the total.
        "entitlement": w.entitlement_seen,
        // A book is what one withdrawal draws; the device learns the size from a book it
        // holds, so this follows the server rather than a number compiled into the app.
        "bookToku": books_size_toku(&w, &dir, server.as_deref().unwrap_or("")),
        // The smallest amount that can change hands: a coin-paid answer rounds up to it.
        "coinToku": scrai_core::coconut::COIN_TOKU,
        "tiers": TIERS,
        "fakePayments": false,
        "gateway": "btcpay",
        "testnet": testnet,
        "faucetUrl": faucet_url,
        "siteUrl": SERVER_SITE.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        "card": card,
        "rails": rails,
        "coins": coins,
        "models": models,
        "server": server,
        "serverAlternates": w.server_alternates,
        "iapProducts": IAP_PRODUCTS.lock().unwrap_or_else(|e| e.into_inner()).clone(),
    });
    diag(&app, &format!(
        "state: about to respond, {} bytes total (models {} bytes)",
        serde_json::to_vec(&out).map(|v| v.len()).unwrap_or(0),
        serde_json::to_vec(&models).map(|v| v.len()).unwrap_or(0),
    ));
    Ok(out)
}

#[tauri::command]
async fn set_server(app: AppHandle, transport: State<'_, Arc<Transport>>, address: String) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let mut w = wallet::load(&dir);
    let a = address.trim().to_string();
    // H5: reject a malformed address at the boundary — never persist an unvalidated
    // recipient that all subsequent (account-signed) traffic would be routed to.
    w.server = if a.is_empty() {
        None
    } else {
        Transport::validate_address(&a)?;
        Some(a)
    };
    wallet::save(&dir, &w)?;
    // Everything learned from the previous server is stale now: catalogue, testnet flag,
    // card info, update notice. Without this the app kept showing the old server's
    // "TESTER" mode until a restart.
    transport.clear_cached_models().await;
    *SERVER_TESTNET.lock().unwrap_or_else(|e| e.into_inner()) = (false, None);
    *SERVER_SITE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *SERVER_CARD.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *SERVER_RAILS.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *SERVER_COINS.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *SERVER_VERSION.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *SERVER_UPDATE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    // Check the NEW address right away: over the live route it's a single ping, and a
    // dead route (e.g. the previous server was unreachable) is rebuilt first instead of
    // leaving the old reconnect loop to time out. No-op while a rebuild is in flight —
    // that one reloads the wallet per attempt, so it sees the new server on its next go.
    if w.server.is_some() {
        spawn_server_check(app.clone(), transport.inner().clone());
    }
    Ok(json!({ "server": w.server }))
}

/// Set the mixnet performance/privacy tradeoff (cover-traffic rate + mixing delay).
/// The default is Nym's own max-privacy setting; the UI only ever moves it toward
/// performance/battery, explicitly and reversibly. Drops the client so it reconnects.
#[tauri::command]
async fn set_mixnet_perf(
    transport: State<'_, Arc<Transport>>,
    #[allow(non_snake_case)] coverMs: u64,
    #[allow(non_snake_case)] mixMs: u64,
    #[allow(non_snake_case)] sendMs: u64,
    continuous: bool,
) -> Result<(), String> {
    transport.set_perf(coverMs, mixMs, sendMs, continuous).await;
    Ok(())
}

/// True if the wallet holds bearer ecash that a fresh `Wallet{..Default}` would drop:
/// any held purse, or a spend still in flight. Held ecash is NOT seed-rebuildable
/// (`wallet.rs` header) — dropping it is an irreversible money loss (M-cl-1).
fn has_held_value(w: &wallet::Wallet) -> bool {
    !w.coconut_purses.is_empty() || w.pending_spend.is_some() || w.pending_withdraw.is_some()
}

/// Distinct, machine-parseable prefix so the frontend can recognise "you'd lose held
/// credit" and turn it into an explicit confirm instead of a generic failure.
const HELD_CREDIT_ERR: &str = "HELD_CREDIT: switching accounts here discards the coins on this device (bearer money, not recoverable from the phrase). Return them to your account balance first, or confirm to discard them.";

/// Create + persist a fresh account, refusing to silently discard held bearer ecash
/// unless the UI has confirmed the loss (M-cl-1). Returns (data dir, mnemonic, fingerprint).
fn account_new_inner(app: &AppHandle, force: Option<bool>) -> Result<(PathBuf, String, String), String> {
    let dir = data_dir(app)?;
    let prev = wallet::load(&dir);
    // Backstop against silently discarding held bearer purses — refuse unless the UI
    // has explicitly confirmed the loss (M-cl-1). Independent of any CSP/XSS mitigation.
    if has_held_value(&prev) && !force.unwrap_or(false) {
        return Err(HELD_CREDIT_ERR.into());
    }
    let a = account::create_account();
    // A NEW account starts unverified: the three-word check has to happen before this
    // wallet may buy anything. A restore sets it true — typing all twenty-four words IS the proof.
    let w = wallet::Wallet { mnemonic: Some(a.mnemonic.clone()), server: prev.server, entry_gateway: prev.entry_gateway, entry_random: prev.entry_random, legacy_session_chat: prev.legacy_session_chat, phrase_verified: false, ..Default::default() };
    wallet::save(&dir, &w)?;
    let fp = account::fingerprint(&a.account_id);
    Ok((dir, a.mnemonic, fp))
}

#[cfg(not(target_os = "ios"))]
#[tauri::command]
fn account_new(app: AppHandle, force: Option<bool>) -> Result<Value, String> {
    let (_dir, mnemonic, fingerprint) = account_new_inner(&app, force)?;
    Ok(json!({ "mnemonic": mnemonic, "fingerprint": fingerprint }))
}

/// iOS (H1): the seed never crosses the IPC boundary. `account_reveal` and
/// `account_migrate_qr` both show it on the native, biometric-gated screen — and a freshly
/// minted phrase is the same secret as a revealed one. Returning it here put the 24 words
/// in the DOM the moment 0.5.2 started rendering the new phrase, which is exactly the reach
/// an XSS in the webview would need (audit 2026-09-06). Returns no secret.
#[cfg(target_os = "ios")]
#[tauri::command]
fn account_new(app: AppHandle, force: Option<bool>) -> Result<Value, String> {
    let (dir, _mnemonic, fingerprint) = account_new_inner(&app, force)?;
    let app2 = app.clone();
    app.run_on_main_thread(move || ios_secure::reveal_phrase(app2, dir, "Recovery phrase"))
        .map_err(|e| e.to_string())?;
    Ok(json!({ "native": true, "fingerprint": fingerprint }))
}

/// Wipe the on-device account (mnemonic + held credentials + pending), keeping the
/// server/gateway config. Destructive: any un-redeemed held ecash is gone (bearer
/// money, not seed-rebuildable) — the UI confirms first.
#[tauri::command]
fn account_delete(app: AppHandle, force: Option<bool>) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let prev = wallet::load(&dir);
    // Same backstop: deleting with held ecash present burns bearer money (M-cl-1).
    if has_held_value(&prev) && !force.unwrap_or(false) {
        return Err(HELD_CREDIT_ERR.into());
    }
    let w = wallet::Wallet { server: prev.server, entry_gateway: prev.entry_gateway, entry_random: prev.entry_random, legacy_session_chat: prev.legacy_session_chat, ..Default::default() };
    wallet::save(&dir, &w)?;
    Ok(json!({ "ok": true }))
}

/// Migration export: the recovery phrase + a QR of it. Only the mnemonic needs to move
/// — entitlement AND the session balance both come back from the server via the seed, so
/// there is no bearer data to transfer (the UI redeems all held credit FIRST). Same
/// server on the other device is required (that is where the session balance lives).
#[cfg(not(target_os = "ios"))]
#[tauri::command]
fn account_migrate_qr(app: AppHandle) -> Result<Value, String> {
    let w = wallet::load(&data_dir(&app)?);
    let m = w.mnemonic.ok_or("no account")?;
    Ok(json!({ "mnemonic": m, "qr": qr_svg(&m) }))
}

/// iOS never hands the seed (or a QR that encodes it) to the webview — H1. The JS side has
/// already redeemed held credit; this shows the phrase on the native, biometric-gated
/// screen, and the other device is restored by TYPING the 24 words. Returns no secret.
#[cfg(target_os = "ios")]
#[tauri::command]
fn account_migrate_qr(app: AppHandle) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    if wallet::load(&dir).mnemonic.is_none() {
        return Err("no account".into());
    }
    let app2 = app.clone();
    app.run_on_main_thread(move || ios_secure::reveal_phrase(app2, dir, "Move to another device"))
        .map_err(|e| e.to_string())?;
    Ok(json!({ "native": true }))
}

#[cfg(not(target_os = "ios"))]
#[tauri::command]
fn account_reveal(app: AppHandle) -> Result<Value, String> {
    let w = wallet::load(&data_dir(&app)?);
    match w.mnemonic {
        Some(m) => Ok(json!({ "mnemonic": m })),
        None => Err("no account".into()),
    }
}

/// iOS: the seed is NEVER returned to the webview (H1 — closes the "XSS → account_reveal →
/// open_external → seed exfil" drain). Instead this presents the native Account Security
/// action sheet; its "Reveal recovery phrase" button is a NATIVE action (biometric-gated),
/// so webview JS can at most pop the sheet, never trigger or read the reveal.
#[cfg(target_os = "ios")]
#[tauri::command]
fn account_reveal(app: AppHandle) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    if wallet::load(&dir).mnemonic.is_none() {
        return Err("no account".into());
    }
    let app2 = app.clone();
    app.run_on_main_thread(move || ios_secure::open_account_security(app2, dir))
        .map_err(|e| e.to_string())?;
    Ok(json!({ "native": true }))
}

#[tauri::command]
fn account_restore(app: AppHandle, mnemonic: String, force: Option<bool>) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let prev = wallet::load(&dir);
    // Restoring onto a device that still holds ecash would drop it (M-cl-1). On a fresh
    // device (the migration case) purses are empty, so this never fires there.
    if has_held_value(&prev) && !force.unwrap_or(false) {
        return Err(HELD_CREDIT_ERR.into());
    }
    let a = account::from_mnemonic(&mnemonic)?;
    let w = wallet::Wallet { mnemonic: Some(a.mnemonic.clone()), server: prev.server, entry_gateway: prev.entry_gateway, entry_random: prev.entry_random, legacy_session_chat: prev.legacy_session_chat, phrase_verified: true, ..Default::default() };
    wallet::save(&dir, &w)?;
    Ok(json!({ "fingerprint": account::fingerprint(&a.account_id), "balance": 0 }))
}

/// The three-word check, step one: three distinct positions, drawn fresh on every call —
/// so "show the phrase again" changes which words are asked, and nothing can be copied off
/// the previous screen. The words themselves never go to the webview (H1): it gets numbers,
/// sends back what the user typed, and hears yes or no.
#[tauri::command]
fn phrase_check_start(app: AppHandle) -> Result<Value, String> {
    let w = wallet::load(&data_dir(&app)?);
    let n = w.mnemonic.as_deref().map(|m| m.split_whitespace().count()).ok_or("no account")?;
    if n < 12 {
        return Err("no phrase to check".into());
    }
    use rand::seq::SliceRandom;
    let mut all: Vec<u32> = (1..=n as u32).collect();
    all.shuffle(&mut rand::rngs::OsRng);
    let mut pick: Vec<u32> = all.into_iter().take(3).collect();
    pick.sort_unstable();
    Ok(json!({ "positions": pick, "total": n, "verified": w.phrase_verified }))
}

/// Step two. Case and surrounding whitespace are forgiven — these are read off paper.
/// Which word was wrong is not reported: a few tries against one's own phrase need no
/// hint, and a hint would be an oracle for anyone else holding the phone.
#[tauri::command]
fn phrase_check_verify(app: AppHandle, positions: Vec<u32>, words: Vec<String>) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let mut w = wallet::load(&dir);
    let m = w.mnemonic.clone().ok_or("no account")?;
    let all: Vec<&str> = m.split_whitespace().collect();
    if positions.len() != 3 || words.len() != 3 {
        return Err("three positions and three words".into());
    }
    let ok = positions.iter().zip(words.iter()).all(|(p, typed)| {
        let idx = (*p as usize).wrapping_sub(1);
        all.get(idx).is_some_and(|real| real.eq_ignore_ascii_case(typed.trim()))
    });
    if ok && !w.phrase_verified {
        w.phrase_verified = true;
        wallet::save(&dir, &w)?;
        log::info!("[account] three-word check passed — the wallet may buy credit");
    }
    Ok(json!({ "ok": ok }))
}

/// Is the phrase copied to iCloud Keychain? `available` is false off iOS, so the row can
/// hide itself rather than offer a switch that does nothing.
#[tauri::command]
fn phrase_backup_get(_app: AppHandle) -> Result<Value, String> {
    #[cfg(target_os = "ios")]
    {
        // A failed READ must not hide the switch: the row then shows the error, and the
        // opt-in button retries the write — which is where the real cause surfaces.
        return Ok(match wallet::synced_phrase() {
            Ok(p) => json!({ "available": true, "on": p.is_some() }),
            Err(e) => {
                log::warn!("[keychain-copy] synchronizable item read failed: {e}");
                json!({ "available": true, "on": false, "error": e })
            }
        });
    }
    #[cfg(not(target_os = "ios"))]
    Ok(json!({ "available": false, "on": false }))
}

/// Opt in or out. In: the phrase of THIS wallet goes to the synchronizable item. Out: the
/// item is deleted — on every device that shares the Apple ID, once iCloud Keychain syncs
/// the deletion.
#[tauri::command]
fn phrase_backup_set(app: AppHandle, on: bool) -> Result<Value, String> {
    #[cfg(target_os = "ios")]
    {
        if on {
            let w = wallet::load(&data_dir(&app)?);
            let m = w.mnemonic.ok_or("no account — create one first")?;
            wallet::set_synced_phrase(Some(&m))?;
            log::info!("[backup] Keychain copy created (opt-in)");
        } else {
            wallet::set_synced_phrase(None)?;
            log::info!("[backup] Keychain copy removed");
        }
        return Ok(json!({ "available": true, "on": on }));
    }
    #[cfg(not(target_os = "ios"))]
    {
        let _ = (app, on);
        Err("the phrase backup is an iOS feature".into())
    }
}

#[tauri::command]
async fn invoice(
    app: AppHandle,
    transport: State<'_, Arc<Transport>>,
    usd: u32,
    method: Option<String>,
    testnet: Option<bool>,
    invite_code: Option<String>,
    consent: Option<Value>,
) -> Result<Value, String> {
    let transport = buy_transport(&app, &transport).await;
    let w = wallet::load(&data_dir(&app)?);
    let srv = server_addr(&w)?;
    let m = w.mnemonic.ok_or("no account — create one first")?;
    let a = account::from_mnemonic(&m)?;
    let nonce = rand_hex(16);
    // The method is not part of the signature — it only selects the payment rail,
    // it grants no authority — so the server accepts the same account signature.
    // The method is the rail ("nyx", "card") or a COIN id from the catalog ("btc-ln",
    // "usdc-sol"). Pass ids through unchanged — flattening them to "btc" here is what would
    // silently raise an on-chain invoice for someone who picked Lightning. Shape-checked
    // only; the server decides what it actually sells.
    let method = match method.as_deref() {
        Some(m) if m.len() <= 24 && !m.is_empty()
            && m.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') => m.to_string(),
        _ => "btc".to_string(),
    };
    let sig = a.sign(&format!("invoice:{}", usd), &nonce);
    let mut req = json!({"v":PROTO,"kind":"invoice.create","id":rand_hex(16),"publicKey":a.public_key_pem,"usd":usd,"method":method,"nonce":nonce,"sig":sig});
    // Tester's $1, paid by the server's faucet. The invite code is what opens that tile;
    // the server re-checks it against the faucet ledger and binds it into the invoice, so
    // sending one here claims nothing on its own.
    if testnet == Some(true) {
        req["testnet"] = json!(true);
    }
    if let Some(c) = invite_code.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        req["inviteCode"] = json!(c.to_ascii_uppercase());
    }
    // § 356 (5) BGB: a paid purchase carries BOTH consents or it is not raised. The buy
    // sheet disables the button without them; this is the second of the three gates (the
    // server holds the third). The invite tile costs the buyer nothing and is exempt.
    if testnet != Some(true) {
        let c = consent.unwrap_or(Value::Null);
        let ok = |k: &str| c.get(k).and_then(|b| b.as_bool()).unwrap_or(false);
        let version = c
            .get("version")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty() && v.len() <= 32 && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'))
            .unwrap_or_default()
            .to_string();
        if version.is_empty() || !ok("immediateStart") || !ok("waiverAck") {
            return Err("this purchase needs both confirmations above — tick them to continue".into());
        }
        req["consent"] = json!({ "version": version, "immediateStart": true, "waiverAck": true });
    }
    let resp = transport.round_trip(&srv, &req, SURBS_SMALL, TIMEOUT_MS).await?;

    let options: Vec<Value> = resp
        .get("options")
        .and_then(|o| o.as_array())
        .map(|arr| {
            arr.iter()
                .map(|o| {
                    let uri = o.get("uri").and_then(|u| u.as_str()).unwrap_or_default();
                    let is_nym = o.get("method").and_then(|x| x.as_str()) == Some("NYM");
                    let mut oo = o.clone();
                    // A card option is a hosted-checkout link, not an address — no QR.
                    if o.get("method").and_then(|x| x.as_str()) == Some("card") {
                        return oo;
                    }
                    // NYM gets the branded (purple + Nym mark) QR à la NymQR; the
                    // QR encodes the bare Nyx address, with the memo shown as text.
                    oo["qr"] = json!(if is_nym { qr_svg_nym(uri) } else { qr_svg(uri) });
                    oo
                })
                .collect()
        })
        .unwrap_or_default();

    // Card invoices carry Mollie's hosted checkout URL; the webview opens it in the OS
    // browser (open_external). Only an https link on mollie.com passes — the server
    // checks the same, this is belt and braces on the untrusted reply.
    let checkout = options
        .iter()
        .find(|o| o.get("method").and_then(|m| m.as_str()) == Some("card"))
        .and_then(|o| o.get("checkout").and_then(|c| c.as_str()))
        .filter(|u| {
            u.strip_prefix("https://")
                .and_then(|r| r.split('/').next())
                .is_some_and(|h| h == "mollie.com" || h.ends_with(".mollie.com"))
        })
        .unwrap_or("")
        .to_string();

    Ok(json!({
        "invoiceId": resp.get("invoiceId"),
        "amountUsd": resp.get("amountUsd"),
        "amountToku": resp.get("amountToku").or_else(|| resp.get("amountScrai")),
        "amountScrai": resp.get("amountScrai"),
        "expiresAt": resp.get("expiresAt"),
        "instruction": resp.get("instruction"),
        "options": options,
        "checkout": checkout,
        "testnet": resp.get("testnet").and_then(|b| b.as_bool()).unwrap_or(false),
    }))
}

/// Redeem a code: a voucher bought on the site, or an invite code. The app cannot tell them
/// apart — they look alike on purpose — so the server does, and answers `voucher.invite`
/// when the string turns out to be an invite. One round trip either way, which matters over
/// a mixnet.
#[tauri::command]
async fn voucher_redeem(app: AppHandle, transport: State<'_, Arc<Transport>>, code: String) -> Result<Value, String> {
    let transport = buy_transport(&app, &transport).await;
    let w = wallet::load(&data_dir(&app)?);
    let srv = server_addr(&w)?;
    let m = w.mnemonic.ok_or("no account — create one first")?;
    let a = account::from_mnemonic(&m)?;
    let code: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .map(|c| c.to_ascii_uppercase())
        .take(64)
        .collect();
    if code.is_empty() {
        return Err("enter a code first".into());
    }
    let nonce = rand_hex(16);
    let sig = a.sign("voucher", &nonce);
    let resp = transport
        .round_trip(
            &srv,
            &json!({"v":PROTO,"kind":"voucher.redeem","id":rand_hex(16),"code":code,
                    "publicKey":a.public_key_pem,"nonce":nonce,"sig":sig}),
            SURBS_SMALL,
            TIMEOUT_MS,
        )
        .await?;
    if let Some(e) = resp.get("error").and_then(|e| e.as_str()) {
        return Err(e.to_string());
    }
    Ok(json!({
        "kind": resp.get("kind").and_then(|k| k.as_str()).unwrap_or(""),
        "toku": resp.get("toku").and_then(|t| t.as_u64()).unwrap_or(0),
        "code": resp.get("code").and_then(|c| c.as_str()).unwrap_or(""),
    }))
}

#[tauri::command]
async fn invoice_status(app: AppHandle, transport: State<'_, Arc<Transport>>, id: String) -> Result<Value, String> {
    let transport = buy_transport(&app, &transport).await;
    let w = wallet::load(&data_dir(&app)?);
    let srv = server_addr(&w)?;
    let resp = transport
        .round_trip(&srv, &json!({"v":PROTO,"kind":"invoice.status","id":rand_hex(16),"invoiceId":id}), SURBS_SMALL, TIMEOUT_MS)
        .await?;
    // `watch` (live NYM chain-watch health) is passed through when present.
    Ok(json!({ "status": resp.get("status"), "entitlement": resp.get("entitlement"), "watch": resp.get("watch") }))
}

/// On-device OCR for the privacy guard. `image` is base64-encoded PNG/JPEG bytes.
/// Returns recognised text, or an error the frontend falls back on (→ Tesseract-WASM).
#[tauri::command]
async fn ocr_scan(image: String) -> Result<Vec<ocr::TextBox>, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let bytes = B64.decode(image.as_bytes()).map_err(|e| format!("bad image data: {e}"))?;
    let n = bytes.len();
    let res = tokio::task::spawn_blocking(move || ocr::recognize(&bytes))
        .await
        .map_err(|e| format!("ocr task failed: {e}"))?;
    match &res {
        Ok(b) => log::info!("[ocr] on-device OCR read {} text lines from {}-byte image", b.len(), n),
        Err(e) => log::warn!("[ocr] recognition failed: {e}"),
    }
    res
}

/// Extract a PDF's text layer in the Rust core (robust; avoids pdf.js's newer-JS
/// requirements in the WKWebView). `image` is base64 PDF bytes. Empty result =
/// likely a scanned PDF (no text layer) — the frontend can then render+OCR.
#[tauri::command]
async fn pdf_text(image: String) -> Result<String, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let bytes = B64.decode(image.as_bytes()).map_err(|e| format!("bad pdf data: {e}"))?;
    let res = tokio::task::spawn_blocking(move || {
        pdf_extract::extract_text_from_mem(&bytes).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("pdf task failed: {e}"))?;
    match &res {
        Ok(t) => log::info!("[pdf] extracted {} chars of text layer", t.len()),
        Err(e) => log::warn!("[pdf] extraction failed: {e}"),
    }
    res
}

/// OCR a scanned / image-only PDF: render its pages natively and run Vision on
/// each. `image` is base64 PDF bytes. Used when `pdf_text` returns no text layer.
#[tauri::command]
async fn pdf_ocr(image: String) -> Result<String, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let bytes = B64.decode(image.as_bytes()).map_err(|e| format!("bad pdf data: {e}"))?;
    let res = tokio::task::spawn_blocking(move || ocr::recognize_pdf(&bytes))
        .await
        .map_err(|e| format!("pdf-ocr task failed: {e}"))?;
    match &res {
        Ok(t) => log::info!("[pdf] rendered+OCR read {} chars", t.len()),
        Err(e) => log::warn!("[pdf] render/OCR failed: {e}"),
    }
    res
}

#[derive(serde::Serialize)]
struct PdfPageJson {
    png: String,
    boxes: Vec<ocr::TextBox>,
}

/// Render PDF pages to PNGs (base64) with their text boxes — for the redaction UI.
#[tauri::command]
async fn pdf_pages(image: String) -> Result<Vec<PdfPageJson>, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let bytes = B64.decode(image.as_bytes()).map_err(|e| format!("bad pdf data: {e}"))?;
    let pages = tokio::task::spawn_blocking(move || ocr::recognize_pdf_pages(&bytes))
        .await
        .map_err(|e| format!("pdf-pages task failed: {e}"))??;
    log::info!("[pdf] rendered {} page(s) for redaction", pages.len());
    Ok(pages
        .into_iter()
        .map(|p| PdfPageJson { png: B64.encode(&p.png), boxes: p.boxes })
        .collect())
}

/// Resolve the GLiNER model + tokenizer for the semantic guard. Desktop bundles
/// them as Tauri resources; mobile downloads them into app-data. Prefer whichever
/// exists (bundled first, then downloaded), returning None if neither is present.
fn smart_paths(app: &AppHandle) -> Option<(PathBuf, PathBuf)> {
    use tauri::path::BaseDirectory;
    for base in [BaseDirectory::Resource, BaseDirectory::AppData] {
        let model = app.path().resolve("models/gliner/model.onnx", base).ok();
        let tok = app.path().resolve("models/gliner/tokenizer.json", base).ok();
        if let (Some(m), Some(t)) = (model, tok) {
            if m.is_file() && t.is_file() {
                return Some((m, t));
            }
        }
    }
    None
}

/// Whether the semantic guard is usable right now (engine built AND model present).
#[tauri::command]
fn smart_available(app: AppHandle) -> bool {
    smart_paths(&app)
        .map(|(m, t)| detect::available(&m, &t))
        .unwrap_or(false)
}

/// Zero-shot NER over a batch of texts (e.g. every OCR box on a page). Returns the
/// entities found in each, aligned to `texts` by index. Errors if the model isn't
/// installed or the `smart-guard` feature isn't built (caller falls back).
#[tauri::command]
async fn smart_detect(
    app: AppHandle,
    texts: Vec<String>,
    labels: Vec<String>,
) -> Result<Vec<Vec<detect::Entity>>, String> {
    let (model, tok) = smart_paths(&app).ok_or("smart-guard model not installed")?;
    let res = tokio::task::spawn_blocking(move || {
        detect::detect(&model, &tok, &texts, &labels, 0.5)
    })
    .await
    .map_err(|e| format!("detect task failed: {e}"))?;
    match &res {
        Ok(v) => log::info!(
            "[detect] semantic scan of {} text(s): {} with matches",
            v.len(),
            v.iter().filter(|e| !e.is_empty()).count()
        ),
        Err(e) => log::warn!("[detect] semantic scan failed: {e}"),
    }
    res
}
/// Is this invite code still good for a $1 credit? Answered by the server against the
/// faucet's ledger. UX only — it decides whether the app offers the $1 tile, never
/// whether money moves.
#[tauri::command]
async fn invite_check(
    app: AppHandle,
    transport: State<'_, Arc<Transport>>,
    code: String,
) -> Result<Value, String> {
    let transport = buy_transport(&app, &transport).await;
    let w = wallet::load(&data_dir(&app)?);
    let srv = server_addr(&w)?;
    let m = w.mnemonic.ok_or("no account — create one first")?;
    let a = account::from_mnemonic(&m)?;
    let nonce = rand_hex(16);
    let sig = a.sign("invite", &nonce);
    let req = json!({"v":PROTO,"kind":"invite.check","id":rand_hex(16),"publicKey":a.public_key_pem,
                     "code":code.trim().to_ascii_uppercase(),"nonce":nonce,"sig":sig});
    let resp = transport.round_trip(&srv, &req, SURBS_SMALL, TIMEOUT_MS).await?;
    if let Some(e) = resp.get("error").and_then(|e| e.as_str()) {
        return Err(e.to_string());
    }
    Ok(json!({
        "valid": resp.get("valid").and_then(|v| v.as_bool()).unwrap_or(false),
        "usd": resp.get("usd").and_then(|v| v.as_u64()).unwrap_or(1),
    }))
}

#[tauri::command]
async fn invoice_cancel(app: AppHandle, transport: State<'_, Arc<Transport>>, id: String) -> Result<Value, String> {
    let transport = buy_transport(&app, &transport).await;
    let w = wallet::load(&data_dir(&app)?);
    let srv = server_addr(&w)?;
    let resp = transport
        .round_trip(&srv, &json!({"v":PROTO,"kind":"invoice.cancel","id":rand_hex(16),"invoiceId":id}), SURBS_SMALL, TIMEOUT_MS)
        .await?;
    Ok(json!({ "ok": resp.get("ok") }))
}

/// Collect paid-for entitlement into held coconut ticketbooks: ask what is
/// owed, then withdraw one account-signed book per full book the entitlement
/// covers. Each book is persisted as it lands, so a dropped connection loses
/// nothing — the remaining entitlement stays on the server for the next call.
#[tauri::command]
async fn collect(app: AppHandle, transport: State<'_, Arc<Transport>>) -> Result<Value, String> {
    let t = transport.inner().clone();
    collect_now(app, t).await
}

/// Start-up sweep: if this device is low on books, fetch more in the background. It does
/// NOT block: waiting for it is what made the boot screen sit on "connecting" for half a
/// minute, because the account side has to bring up its own mixnet client first.
#[tauri::command]
fn collect_later(app: AppHandle, force: Option<bool>) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let w = wallet::load(&dir);
    // Three reasons to sweep. `force` is the moment credit has just been BOUGHT: there is
    // entitlement waiting whatever the device already holds, so "am I low?" is the wrong
    // question then. Being low is the ordinary one. And a book near its date has to be
    // swapped even on a FULL device — that is the one case where the money is at stake and
    // nothing else would ever trigger the sweep.
    let go = force.unwrap_or(false)
        || coin_value_toku(&w) < LOW_WATER_TOKU
        || has_expiring_books(&w, now_secs32());
    if go {
        spawn_refill_soon(&app);
    }
    Ok(json!({ "started": go }))
}

/// Is any book close enough to its date to be swapped?
fn has_expiring_books(w: &wallet::Wallet, now: u32) -> bool {
    let deadline = now.saturating_add((SWAP_WINDOW_DAYS * 86_400) as u32);
    w.coconut_purses
        .iter()
        .filter_map(|j| scrai_core::purse::Purse::restore(j).ok())
        .any(|p| p.remaining_coins() > 0 && p.expiration_date() <= deadline)
}

/// The body of `collect`, callable from the background top-up as well.
async fn collect_now(app: AppHandle, main: Arc<Transport>) -> Result<Value, String> {
    use scrai_core::federation::FedResponse;
    diag(&app, "collect: begin");
    let _op = main.begin_op().await;
    let transport = buy_transport(&app, &main).await;
    let dir = data_dir(&app)?;
    let w0 = wallet::load(&dir);
    let srv = server_addr(&w0)?;
    let a = wallet_account(&app)?;

    // A book can only be spent with the material of the epoch it was issued in, and that
    // material is not rebuildable — so when a device holds a book whose keys it no longer
    // has (a cleared cache, an app that changed how it files them), it must ask for that
    // epoch by date. Without this the books are money the device can see and never touch;
    // the Mac hit exactly that on 2026-09-15.
    for (denom, exp) in missing_epochs(&wallet::load(&dir), &dir, &srv) {
        match federation_keys(&transport, &srv, &dir, denom, exp).await {
            Ok(_) => log::warn!("[coconut] fetched the material for an older epoch this device still holds books from"),
            Err(e) => log::warn!("[coconut] an older epoch's material could not be fetched: {e}"),
        }
    }

    // Books near their date go back FIRST, so their value is part of what this sweep then
    // draws again — out of the current epoch, with a full life ahead of it. Best effort: a
    // swap that cannot reach the server leaves the books alone and tries again next time,
    // which is why the window is wide enough for several attempts.
    match return_coins(app.clone(), main.clone(), Returning::Expiring).await {
        Ok(v) => {
            let credited = v.get("credited").and_then(|c| c.as_u64()).unwrap_or(0);
            if credited > 0 {
                log::warn!("[swap] {credited} TOKU of expiring books handed back for fresh ones");
            }
        }
        Err(e) => log::warn!("[swap] could not hand back expiring books: {e}"),
    }

    // How much is owed?
    let nonce = rand_hex(16);
    let sig = a.sign("entitlement", &nonce);
    let resp = transport
        .round_trip(&srv, &json!({"v":PROTO,"kind":"entitlement","id":rand_hex(16),"publicKey":a.public_key_pem,"nonce":nonce,"sig":sig}), SURBS_SMALL, TIMEOUT_MS)
        .await?;
    let owed = resp.get("entitlement").and_then(|e| e.as_u64()).unwrap_or(0);

    // Top the device UP to the working amount rather than drawing everything: what a lost
    // device can cost is then bounded by that amount, and the rest stays on the account
    // where the recovery phrase reaches it (docs/unlinkability.md, block D).
    //
    // TWO DENOMINATIONS, drawn in three steps.
    //
    //   1. the fine FLOAT — a few one-cent books, the small change that lets a tender land
    //      on the exact price. Taken first, because if the coarse books ate the whole cap
    //      there would be no change left and every answer would round up to the cent.
    //   2. the BULK, in coarse books: ten times the value through the mixnet for the same
    //      ~490 bytes per coin.
    //   3. the REMAINDER, in fine books again. Without this step credit smaller than a
    //      coarse book but larger than the float simply sat on the account — 4,503 TOKU
    //      stranded after the first real top-up (2026-09-15). Now what stays behind is
    //      under one fine book, a single cent, as it was before the second denomination.
    let fine = scrai_core::coconut::COIN_TOKU;
    let mut collected = 0u64;
    let mut room_toku = WORKING_TOKU.saturating_sub(coin_value_toku(&wallet::load(&dir)));
    let mut budget = owed;
    // What one book of each denomination costs, from the server's own keys. Never a number
    // compiled into the app: the server decides the size, and a guess put a hundred refused
    // withdrawals through the mixnet once already (2026-09-14).
    let mut book_of: std::collections::BTreeMap<u64, u64> = std::collections::BTreeMap::new();
    for denom in scrai_core::coconut::DENOMS {
        if let Ok(FedResponse::Keys { total_coins, .. }) = federation_keys(&transport, &srv, &dir, denom, 0).await {
            if total_coins > 0 {
                book_of.insert(denom, total_coins * denom);
            }
        }
    }
    let float_gap = book_of.get(&fine).map_or(0, |_| {
        FINE_FLOAT_BOOKS.saturating_sub(books_of_denom(&wallet::load(&dir), fine))
    });
    let mut plan: Vec<(u64, usize)> = Vec::new();
    for denom in scrai_core::coconut::DENOMS {
        if !book_of.contains_key(&denom) {
            continue;
        }
        if denom == fine {
            plan.push((denom, float_gap));
        } else {
            plan.push((denom, usize::MAX)); // the bulk: as much as budget and room allow
        }
    }
    plan.push((fine, usize::MAX)); // the remainder, once the coarse books have had their turn

    for (denom, cap) in plan {
        let Some(&book_toku) = book_of.get(&denom) else { continue };
        let want = cap.min((budget / book_toku) as usize).min((room_toku / book_toku) as usize);
        // An interrupted withdrawal is finished even when the device is otherwise full —
        // the server may already have charged for it.
        let outstanding = wallet::load(&dir)
            .pending_withdraws
            .iter()
            .filter(|p| p.server == srv && p.denom_toku == denom)
            .count();
        if want == 0 && outstanding == 0 {
            continue;
        }
        let got = withdraw_books(&transport, &srv, &a, &dir, want.max(outstanding), denom).await?;
        collected += got;
        budget = budget.saturating_sub(got);
        room_toku = room_toku.saturating_sub(got);
    }

    diag(&app, "collect: about to respond");
    let left = owed.saturating_sub(collected);
    {
        let mut w = wallet::load(&dir);
        w.entitlement_seen = left;
        let _ = wallet::save(&dir, &w);
    }
    // The coins are on the device: the purchase client has done its job for this purchase.
    drop(transport);
    close_buy_link(&app).await;
    Ok(json!({ "collected": collected, "held": coconut_held_toku(&app), "entitlement": left }))
}

/// Fetch more books when the device is running low — in the background and after a random
/// pause, so the account-side call does not sit right next to the question that emptied it.
/// One at a time; a second request while one is running is ignored.
fn spawn_refill(app: &AppHandle) {
    spawn_refill_in(app, 30, 90)
}

/// The same, but soon — used at start-up, where there is no question to sit next to and a
/// user who just bought credit is watching for it.
fn spawn_refill_soon(app: &AppHandle) {
    spawn_refill_in(app, 2, 3)
}

fn spawn_refill_in(app: &AppHandle, base: u64, spread: u64) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let wait = base + (rand::random::<u64>() % spread.max(1));
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        let t = app.state::<Arc<Transport>>().inner().clone();
        let res = collect_now(app.clone(), t).await;
        // Tell the app it happened. Without this the only refresh was a fixed timer, and a
        // sweep that draws several books outlives it — the balance on screen stayed stale
        // until the user pressed "Check for credit" by hand (reported 2026-09-14).
        let done = match &res {
            Ok(v) => {
                log::info!("[coconut] background top-up: {} TOKU", v.get("collected").and_then(|c| c.as_u64()).unwrap_or(0));
                json!({ "ok": true,
                        "collected": v.get("collected").and_then(|c| c.as_u64()).unwrap_or(0),
                        "entitlement": v.get("entitlement").and_then(|c| c.as_u64()).unwrap_or(0) })
            }
            Err(e) => {
                log::warn!("[coconut] background top-up failed: {e}");
                json!({ "ok": false, "error": e.to_string() })
            }
        };
        let _ = app.emit("top-up", done);
        RUNNING.store(false, Ordering::SeqCst);
    });
}


/// What one book is worth, in TOKU. The SERVER decides it, so the client either reads it
/// off a book it already holds or off the epoch material that server published. A number
/// compiled into the app would be wrong the moment the server changes the size — which is
/// exactly what happened on 2026-09-14: a stale fallback of 100 coins made the app believe
/// a book cost ten times what it does, so it drew nothing and reported the credit as
/// waiting. 0 means "not known here yet", and the caller then lets the server decide.
fn books_size_toku(w: &wallet::Wallet, dir: &Path, srv: &str) -> u64 {
    use scrai_core::coconut::COIN_TOKU;
    // The SMALLEST book, because that is what decides how much can be stranded on the
    // account: credit under one fine book cannot be drawn at all.
    if let Some(toku) = w
        .coconut_purses
        .iter()
        .filter_map(|j| scrai_core::purse::Purse::restore(j).ok())
        .filter(|p| p.denom_toku() == COIN_TOKU)
        .map(|p| p.total_coins() * p.denom_toku())
        .next()
    {
        return toku;
    }
    epoch_keys(dir, srv, COIN_TOKU).map(|k| k.book_toku()).unwrap_or(0)
}


#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn chat(
    app: AppHandle,
    transport: State<'_, Arc<Transport>>,
    model: String,
    messages: Value,
    #[allow(non_snake_case)] maxTokens: Option<u64>,
    // Live web-search grounding for this turn. UNSIGNED on purpose: it stays out of the
    // canonicalBody {model, messages, maxTokens} so existing session signatures are
    // unaffected. The server reads it to decide whether to attach the google_search tool.
    live: Option<bool>,
    #[allow(non_snake_case)] bigReply: Option<bool>,
    // Client-chosen reasoning depth (thinking tokens). UNSIGNED like `live` — outside
    // the canonicalBody, so it doesn't affect the session signature. The server clamps
    // it and uses it for both the provider request and the reserve.
    #[allow(non_snake_case)] thinkingBudget: Option<u64>,
    retry: Option<bool>,
    // Requested picture size for image models ("512" | "1K" | "2K" | "4K"). UNSIGNED
    // like `thinkingBudget`; the server validates it and sizes the reserve to it.
    #[allow(non_snake_case)] imageSize: Option<String>,
) -> Result<Value, String> {
    // The whole chat future (mixnet round-trip, redeem loop, chunk fetch) is far too big
    // for the 1 MB main-thread stack the iPhone gives the WebView IPC handler: Tauri
    // moves a command's future onto its runtime BY VALUE, and that move alone overflowed
    // the stack (SIGSEGV with sp on the guard page inside tokio::task::spawn, every
    // prompt, 2026-08-27 — 15 identical crash reports). Boxing it means the handler only
    // ever moves a pointer; the state machine lives on the heap.
    Box::pin(chat_impl(
        app,
        transport.inner().clone(),
        model,
        messages,
        maxTokens,
        live,
        bigReply,
        thinkingBudget,
        retry,
        imageSize,
    ))
    .await
}

#[allow(clippy::too_many_arguments, non_snake_case)]
async fn chat_impl(
    app: AppHandle,
    transport: Arc<Transport>,
    model: String,
    messages: Value,
    maxTokens: Option<u64>,
    live: Option<bool>,
    bigReply: Option<bool>,
    thinkingBudget: Option<u64>,
    retry: Option<bool>,
    imageSize: Option<String>,
) -> Result<Value, String> {
    // Serialise the whole command: session_status + chat must be one atomic unit,
    // or two concurrent chats race the session counter (crossed replies / hangs).
    let _op = transport.begin_op().await;
    // Route down (app just woke up, or a drop): the send below queues behind the rebuild —
    // say so, instead of "Sending to mixnet…" for a message that hasn't left.
    if !transport.is_connected() {
        let _ = app.emit("chat-route", ());
    }

    // Generated pictures no longer ride in the chat reply itself: the server stages
    // anything bigger than one chunk and answers with references (fetched below with
    // their own per-chunk SURBs), so even an image chat's reply is text-sized. The
    // text budget covers it; `bigReply` is kept for older callers and ignored.
    let _ = bigReply;
    let surbs = SURBS_TEXT;
    // Reasoning models advertise a longer `timeoutMs` in the catalog (OpenAI: 180 s);
    // never shorter than the default.
    let chat_timeout_ms: u64 = transport
        .cached_models()
        .await
        .and_then(|ms| {
            ms.as_array()?
                .iter()
                .find(|m| m.get("model").and_then(|x| x.as_str()) == Some(model.as_str()))
                .and_then(|m| m.get("timeoutMs").and_then(|t| t.as_u64()))
        })
        .map(|t| t.max(TIMEOUT_MS))
        .unwrap_or(TIMEOUT_MS);

    let dir = data_dir(&app)?;
    let w = wallet::load(&dir);
    let srv = server_addr(&w)?;

    let m = w.mnemonic.clone().ok_or("no account — create one and buy credit")?;

    // A LOCAL key for the two in-app maps that remember an interrupted request: an
    // unanswered chat and a half-fetched picture. It never leaves the device — it used to
    // be the session id, which did, and which is exactly what coins replaced. Derived from
    // the phrase so it survives a restart and a reinstall alike.
    let chat_key = {
        use sha2::{Digest, Sha256};
        hex::encode(&Sha256::digest(format!("tokumai/chat-key/v1{m}").as_bytes())[..16])
    };

    // Retry after an INTERRUPTED picture download: the picture was generated, charged
    // and staged server-side — resume fetching its missing chunks instead of signing a
    // fresh request that would draw (and bill) a second one.
    if retry.unwrap_or(false) {
        if let Some(dl) = transport.take_staged_download(&chat_key).await {
            let resp = finish_staged_download(&app, &transport, &srv, dl).await?;
            return Ok(paid_chat_reply(&app, &resp, Value::Null));
        }
    }

    // Idempotent retry: if this is a retry AND we still hold the exact request whose
    // reply never arrived, resend it VERBATIM (same counter/sig/id). A server that
    // already processed it answers by replay (cached reply, no second charge); one it
    // never received processes it once. Anything else builds a fresh, freshly-signed
    // request (which also covers a retry after the pending was cleared, e.g. a restart).
    // Coins instead of a session: the request carries a tender and no signature at all.
    // The unburned notes come back in `coin_settle` once the server has answered.
    if coin_chat_enabled(&dir) {
        // A new question first CLOSES whatever is still open. An unanswered tender holds
        // coins that are already spent, and the server answers an identical tender from
        // its replay cache — so carrying one into a new question would answer the new
        // question with the old answer (2026-09-14: "test" came back as a picture).
        if !retry.unwrap_or(false) {
            settle_pending_tenders(&app, &transport, &srv).await;
        }
        let (req, notes, resumed) =
            coin_request(&dir, &srv, &model, &messages, maxTokens, live, thinkingBudget, &imageSize, retry.unwrap_or(false))?;
        if resumed {
            log::info!("[tender] resuming an unanswered tender verbatim");
        }
        let sent_app = app.clone();
        let reply = transport
            .round_trip_raw_notify(&srv, &req, surbs, chat_timeout_ms, move || {
                let _ = sent_app.emit("chat-sent", ());
            })
            .await?;
        // A delivered reply — answer or refusal — settles the tender either way: an error
        // reply burned nothing, so every note goes back into the wallet.
        let req_id = req.get("id").and_then(|i| i.as_str()).unwrap_or_default().to_string();
        coin_settle(&dir, &req_id, &notes, &reply)?;
        if coin_value_toku(&wallet::load(&dir)) < LOW_WATER_TOKU {
            spawn_refill(&app);
        }
        if let Some(e) = reply.get("error").and_then(|e| e.as_str()) {
            return Err(e.to_string());
        }
        let resp = fetch_staged_images(&app, &transport, &srv, &chat_key, reply).await?;
        let warning = overcharge_warning(&srv, &model, &messages, &resp);
        return Ok(paid_chat_reply(&app, &resp, warning));
    }

    // Nothing follows: coins are the only way to pay. The session path — a seed-derived
    // balance the server held, a counter and a signature per request — was what made a
    // person's prompts one story on the server, and it went on 2026-09-15
    // (docs/unlinkability.md, block D). `coin_chat_enabled` is a way BACK for a moment,
    // not a way: without it there is simply nothing to pay with.
    Err("this app is set to the old session payment path, which this server no longer \
         serves — switch \"pay with coins\" back on under Developer".into())
}

/// C3: independent overcharge check. Recompute a fair upper bound from the client's OWN
/// token estimate and bundled retail table; a charge grossly above it means the operator
/// inflated the margin or the token counts. Additive and fail-open — any inability to
/// estimate just skips the check, and never blocks a legitimate answer.
fn overcharge_warning(srv: &str, model: &str, messages: &Value, resp: &Value) -> Value {
    let charged = resp.pointer("/usage/billing/priceScrai").and_then(|v| v.as_u64());
    let has_images = resp
        .get("images")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    let (Some(charged), false) = (charged, has_images) else { return Value::Null };
    let reply_text = resp.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let Some(fair) = fair_price_estimate(model, messages, reply_text) else { return Value::Null };
    // The comparison is against the rounded-up price, because that is what the server may
    // legitimately take: a 0.17 ¢ answer costs 0.2 ¢ when coins pay.
    let coin = scrai_core::coconut::COIN_TOKU;
    let ceiling = ((fair as f64 * OVERCHARGE_FACTOR).ceil() as u64).div_ceil(coin) * coin;
    if charged > MIN_FLAG_SCRAI && charged > ceiling {
        flag_server(srv, &format!("overcharge: charged {charged} TOKU vs ~{fair} fair (>{OVERCHARGE_FACTOR}×)"));
        return json!({
            "kind": "overcharge",
            "charged": charged,
            "fairEstimate": fair,
            "factor": OVERCHARGE_FACTOR,
        });
    }
    Value::Null
}

/// Shape a paid chat reply for the UI. Reports TOTAL spendable credit (funded session
/// balance + un-redeemed coconut coins), consistent with `state`, so the UI number only
/// drops by real chat cost — not by the internal session↔purse shuffle that auto-fund
/// performs. `images` is what image models return — forwarded, or the answer arrives
/// blank and silent.
fn paid_chat_reply(app: &AppHandle, resp: &Value, price_warning: Value) -> Value {
    let session_balance = resp.get("balance").and_then(|b| b.as_u64()).unwrap_or(0);
    let held = coconut_held_toku(app);
    json!({
        "text": resp.get("text"),
        "usage": resp.get("usage"),
        "balance": session_balance.saturating_add(held),
        // Report held explicitly so the UI shows the true session-vs-held split after a
        // message (only ONE redeem chunk moved to the session; the rest stays held).
        "held": held,
        "images": resp.get("images"),
        "priceWarning": price_warning,
    })
}

/// Resume telemetry, LOCAL ONLY: one JSON line per return to the foreground — how long the
/// app was hidden and whether the old route was still alive. No identifiers, no addresses.
/// Read back by `resume_stats` (connection sheet, Developer page) so LONG_PAUSE_MS can be
/// set from real numbers instead of a guess. Capped at ~64 KB (oldest half dropped).
fn record_resume(app: &AppHandle, hidden_ms: u64, action: &str, alive: Option<bool>, rtt_ms: Option<u64>, reason: &str) {
    let Ok(dir) = data_dir(app) else { return };
    let path = dir.join("resume-stats.jsonl");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = json!({
        "ts": ts, "os": std::env::consts::OS, "hidden_ms": hidden_ms,
        "action": action, "alive": alive, "rtt_ms": rtt_ms, "reason": reason,
    })
    .to_string();
    let _ = std::fs::create_dir_all(&dir);
    if std::fs::metadata(&path).map(|m| m.len() > 64 * 1024).unwrap_or(false) {
        if let Ok(all) = std::fs::read_to_string(&path) {
            let lines: Vec<&str> = all.lines().collect();
            let _ = std::fs::write(&path, lines[lines.len() / 2..].join("\n") + "\n");
        }
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
    log::info!("[resume] hidden {hidden_ms} ms → {action} ({reason}) alive={alive:?} rtt={rtt_ms:?}");
}

/// Summary + tail of the resume log for the connection sheet / Developer page.
#[tauri::command]
async fn resume_stats(app: AppHandle) -> Result<Value, String> {
    let path = data_dir(&app)?.join("resume-stats.jsonl");
    let all = std::fs::read_to_string(&path).unwrap_or_default();
    let (mut n, mut alive, mut dead, mut rebuilt, mut longest_alive) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut shortest_dead: Option<u64> = None;
    for l in all.lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        n += 1;
        let h = v["hidden_ms"].as_u64().unwrap_or(0);
        match v["alive"].as_bool() {
            Some(true) => { alive += 1; longest_alive = longest_alive.max(h); }
            Some(false) => { dead += 1; shortest_dead = Some(shortest_dead.map_or(h, |d| d.min(h))); }
            None => rebuilt += 1,
        }
    }
    let tail: Vec<&str> = all.lines().rev().take(200).collect::<Vec<_>>().into_iter().rev().collect();
    Ok(json!({
        "count": n, "alive": alive, "dead": dead, "rebuilt": rebuilt,
        "longest_alive_ms": longest_alive, "shortest_dead_ms": shortest_dead,
        "log": tail.join("\n"), "path": path.display().to_string(),
    }))
}

/// Android only: the webview keeps running in the background, and with it the cover
/// traffic — radio + CPU for nothing. After a while hidden the JS side calls this to drop
/// the client; the next resume rebuilds the route (it is past LONG_PAUSE_MS by then).
/// iOS never gets here: the process is frozen, so there is nothing to stop.
#[tauri::command]
async fn app_hidden(transport: State<'_, Arc<Transport>>) -> Result<(), String> {
    transport.inner().drop_client().await;
    log::info!("[resume] hidden long enough on Android — client dropped to stop cover traffic");
    Ok(())
}

/// The app came back to the foreground after `hidden_ms` in the background (iOS freezes
/// the process ~30 s after that; the gateway socket usually survives a while longer — how
/// long is what `resume-stats.jsonl` measures). Long pause, or `force`: drop the client and
/// rebuild the route — with progress events. Otherwise one ping through the mixnet (5 s
/// budget; normal round trips take 1-3 s) decides; a failed ping drops the client too.
/// Returns `{ action: "reconnect" | "alive", ms }`.
#[tauri::command]
async fn app_resumed(
    app: AppHandle,
    transport: State<'_, Arc<Transport>>,
    #[allow(non_snake_case)] hiddenMs: u64,
    force: Option<bool>,
) -> Result<Value, String> {
    // Was 20 s (a guess). A rebuild costs more battery than a ping (key generation, a
    // clearnet topology fetch, gateway handshake, SURB warm-up), so the ping-first window
    // is wide; the log tells us where the real cliff is.
    const LONG_PAUSE_MS: u64 = 90_000;
    const PING_BUDGET_MS: u64 = 5_000;
    let t: Arc<Transport> = transport.inner().clone();
    let forced = force.unwrap_or(false);
    if forced || hiddenMs >= LONG_PAUSE_MS || !t.is_connected() {
        let reason = if forced { "requested" } else if hiddenMs >= LONG_PAUSE_MS { "long-pause" } else { "dead" };
        record_resume(&app, hiddenMs, "reconnect", if reason == "dead" { Some(false) } else { None }, None, reason);
        t.drop_client().await;
        spawn_rebuild(app.clone(), t.clone());
        return Ok(json!({ "action": "reconnect", "reason": reason }));
    }
    let _ = app.emit("mixnet-phase", json!({ "step": "check", "detail": "" }));
    let w = wallet::load(&data_dir(&app)?);
    let srv = server_addr(&w)?;
    let req = json!({ "v": PROTO, "kind": "ping", "id": rand_hex(8) });
    let t0 = std::time::Instant::now();
    // round_trip drops the client + marks it dead on a timeout, so the reconnect below is
    // the genuine full rebuild, not a retry on a corpse.
    match t.round_trip(&srv, &req, SURBS_SMALL, PING_BUDGET_MS).await {
        Ok(_) => {
            let ms = t0.elapsed().as_millis() as u64;
            record_resume(&app, hiddenMs, "alive", Some(true), Some(ms), "ping-ok");
            let _ = app.emit("mixnet-phase", json!({ "step": "ready", "detail": "alive" }));
            Ok(json!({ "action": "alive", "ms": ms }))
        }
        Err(_) => {
            record_resume(&app, hiddenMs, "reconnect", Some(false), None, "ping-failed");
            t.drop_client().await;
            spawn_rebuild(app.clone(), t.clone());
            Ok(json!({ "action": "reconnect", "reason": "ping-failed" }))
        }
    }
}

/// THE reconnect path (route poll + resume + "rebuild now"): rebuild the route — keys,
/// client, gateway, cover traffic are reported by ensure_connected — then prove the far end
/// with a ping ("Connecting to tokumai server"), and only then report `online`. One at
/// a time; a failure reports `failed` with the reason. A server that doesn't answer keeps
/// the fresh route (probe never drops the client) so the poll doesn't rebuild in a loop.
fn spawn_rebuild(app: AppHandle, t: Arc<Transport>) {
    if !t.try_begin_reconnect() {
        return;
    }
    tauri::async_runtime::spawn(async move { run_check(app, t).await });
}

/// Server-switch path: prove the (new) server answers. A LIVE route is reused as-is —
/// only a dead one is rebuilt first — so the common switch is a single ping (~2-5 s).
/// Runs on the same single slot as `spawn_rebuild`, so the two never stack. Any stale
/// round trip against the OLD server (a 30 s catalogue fetch, a reconnect probe) is
/// cancelled first: one of those once held the client lock and left "Use official
/// server" stuck on "reusing it…" with no check line until the fetch timed out and
/// needlessly dropped the route.
fn spawn_server_check(app: AppHandle, t: Arc<Transport>) {
    tauri::async_runtime::spawn(async move {
        t.cancel_in_flight();
        // The cancelled rebuild/check frees the slot within moments — wait briefly.
        // If it stays claimed longer, a live rebuild owns the flow: it reloads the
        // wallet after connecting, so it already probes the NEW server and narrates
        // the same overlay to its end.
        let mut claimed = t.try_begin_check();
        for _ in 0..40 {
            if claimed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            claimed = t.try_begin_check();
        }
        if !claimed {
            return;
        }
        run_check(app, t).await;
    });
}

/// One rebuild-or-check pass: reuse/rebuild the route, then prove the server answers.
/// Caller must hold the reconnect slot; this releases it.
async fn run_check(app: AppHandle, t: Arc<Transport>) {
    let res: Result<(), String> = async {
        if t.is_connected() {
            // Narrate the fast path — the overlay shows this instead of the rebuild steps.
            let _ = app.emit("mixnet-phase", json!({ "step": "reuse", "detail": "" }));
        }
        t.ensure_connected().await?;
        let _ = app.emit("mixnet-phase", json!({ "step": "server", "detail": "" }));
        let dir = data_dir(&app)?;
        let w = wallet::load(&dir);
        let srv = server_addr(&w)?;
        let req = json!({ "v": PROTO, "kind": "ping", "id": rand_hex(8) });
        match t.probe(&srv, &req, SURBS_SMALL, 15_000).await {
            Ok(_) => Ok(()),
            Err(e) if e.starts_with("cancelled") => Err(e), // superseded by a newer check
            Err(_) if w.server_alternates.iter().any(|a| a != &srv) => {
                // The address is silent but the SERVER has other front doors (same
                // identity set from its own catalog reply): try them, and stay on the
                // first that answers. Same server, same balance — nothing to migrate.
                let _ = app.emit("mixnet-phase", json!({ "step": "server", "detail": "trying another address of the same server" }));
                let alternates: Vec<String> = w.server_alternates.iter().filter(|a| *a != &srv).cloned().collect();
                let mut switched = None;
                for alt in alternates {
                    let req = json!({ "v": PROTO, "kind": "ping", "id": rand_hex(8) });
                    match t.probe(&alt, &req, SURBS_SMALL, 15_000).await {
                        Ok(_) => { switched = Some(alt); break; }
                        Err(e) if e.starts_with("cancelled") => return Err(e),
                        Err(_) => {}
                    }
                }
                match switched {
                    Some(alt) => {
                        let mut w2 = wallet::load(&dir);
                        w2.server = Some(alt.clone());
                        wallet::save(&dir, &w2)?;
                        log::warn!("[mixnet] server address silent — switched to another address of the same server");
                        let _ = app.emit("mixnet-phase", json!({ "step": "server", "detail": "switched to a fallback address of the same server" }));
                        Ok(())
                    }
                    None => {
                        let short = if srv.len() > 12 { format!("{}…{}", &srv[..4], &srv[srv.len() - 5..]) } else { srv.clone() };
                        Err(format!("the route is fine — the server at {short} did not answer through the mixnet on any of its addresses; it may be down for maintenance"))
                    }
                }
            }
            Err(_) => {
                let short = if srv.len() > 12 { format!("{}…{}", &srv[..4], &srv[srv.len() - 5..]) } else { srv.clone() };
                Err(format!("the route is fine — the server at {short} did not answer through the mixnet; the address may have a typo, or that server is down or restarting"))
            }
        }
    }
    .await;
    match res {
        Ok(()) => { let _ = app.emit("mixnet-phase", json!({ "step": "online", "detail": "" })); }
        // Cancelled = a newer server check took over the overlay — emit nothing, the
        // new check narrates from here; a "failed" now would flash a stale error.
        Err(e) if e.starts_with("cancelled") => { log::info!("[mixnet] check superseded"); }
        Err(e) => {
            log::warn!("[mixnet] rebuild failed: {e}");
            // srv: the route is up, only the server stayed silent — the UI titles
            // this "Server not answering" and offers the official server instead.
            let _ = app.emit("mixnet-phase", json!({ "step": "failed", "detail": e, "srv": t.is_connected() }));
        }
    }
    t.end_reconnect();
}

/// UI Cancel: stop waiting for the in-flight mixnet reply / chunk download. The request
/// is not withdrawn (it already left); the pending chat stays set for an idempotent Retry.
#[tauri::command]
fn cancel_chat(transport: State<'_, Arc<Transport>>) {
    transport.cancel_in_flight();
}

/// Resolve chunk references in a chat reply's `images[]` (server `replies.rs`): every
/// `{mimeType, ref, chunks, bytes}` becomes a plain `{mimeType, data}` by fetching its
/// `image.chunk` pieces and concatenating the base64 in `seq` order. Inline images pass
/// through untouched. Emits `image-progress {done, total}` per chunk.
///
/// Chunks are fetched in WINDOWS of `CHUNK_WINDOW` (pipelined within a window, windows
/// in sequence) rather than all at once: a 4K picture is 30–55 chunks, and firing them
/// together floods the server with reply-SURBs it can't hold (Nym stores ~200 per
/// sender), stalls, and turns the transport's "re-fire everything that stalled" into a
/// storm. A window keeps the in-flight SURB budget around what the server actually
/// consumes and lets one lost chunk be re-fired alone.
///
/// RESUMABLE: chunks that already arrived are remembered in `transport` when the
/// download fails for a transport reason (timeout, mixnet drop), so the UI's Retry —
/// `chat(retry: true)` — resumes from where it broke instead of generating and paying
/// for a new picture. A definitive server rejection (the reference expired) drops the
/// parked state, and Retry then generates afresh.
async fn fetch_staged_images(
    app: &AppHandle,
    transport: &Transport,
    srv: &str,
    session_key: &str,
    resp: Value,
) -> Result<Value, String> {
    let has_refs = resp
        .get("images")
        .and_then(|i| i.as_array())
        .map(|a| a.iter().any(|i| i.get("ref").is_some()))
        .unwrap_or(false);
    if !has_refs {
        return Ok(resp);
    }
    let dl = nym::StagedDownload { session_id: session_key.to_string(), resp, parts: Default::default() };
    finish_staged_download(app, transport, srv, dl).await
}

/// Chunk requests in flight per window (≈ 8 × 96 KB ≈ 400 mixnet packets of replies).
const CHUNK_WINDOW: usize = 8;

/// Fetch every chunk still missing from `dl` (fresh or resumed) and return the reply
/// with plain `{mimeType, data}` images. On a transport failure the partial state is
/// parked in `transport` for a resume; on an expired reference it is discarded.
async fn finish_staged_download(
    app: &AppHandle,
    transport: &Transport,
    srv: &str,
    mut dl: nym::StagedDownload,
) -> Result<Value, String> {
    let Some(imgs) = dl.resp.get("images").and_then(|i| i.as_array()).cloned() else {
        return Ok(dl.resp);
    };
    // Overall progress across all pictures of this reply (resumed chunks count as done).
    let total: usize = imgs
        .iter()
        .filter(|i| i.get("ref").is_some())
        .map(|i| i.get("chunks").and_then(|c| c.as_u64()).unwrap_or(0) as usize)
        .sum();
    let mut out = Vec::with_capacity(imgs.len());
    let mut done_before: usize = 0;
    for img in imgs {
        let (Some(r), Some(n)) = (
            img.get("ref").and_then(|x| x.as_str()),
            img.get("chunks").and_then(|c| c.as_u64()),
        ) else {
            out.push(img);
            continue;
        };
        let n = n as usize;
        let r = r.to_string();
        let parts = dl.parts.entry(r.clone()).or_insert_with(|| vec![None; n]);
        if parts.len() != n {
            *parts = vec![None; n];
        }
        let received = std::sync::Mutex::new(std::mem::take(parts));
        let count = |v: &Vec<Option<String>>| v.iter().filter(|p| p.is_some()).count();
        let done_here = count(&received.lock().unwrap());
        let _ = app.emit("image-progress", json!({ "done": done_before + done_here, "total": total }));
        let mut result: Result<(), String> = Ok(());
        loop {
            // The next window of chunks that have NOT arrived yet (holes included, so a
            // resumed download only fetches what it is missing).
            let missing: Vec<u64> = {
                let g = received.lock().unwrap();
                g.iter().enumerate().filter(|(_, p)| p.is_none()).map(|(i, _)| i as u64).take(CHUNK_WINDOW).collect()
            };
            if missing.is_empty() {
                break;
            }
            let requests: Vec<Value> = missing
                .iter()
                .map(|seq| json!({"v":PROTO,"kind":"image.chunk","id":rand_hex(16),"ref":r,"seq":seq}))
                .collect();
            let progress_app = app.clone();
            let received_ref = &received;
            let res = transport
                .collect_replies(srv, requests, SURBS_CHUNK, TIMEOUT_MS, move |v, _| {
                    // Keep each chunk the moment it lands — a window that fails later
                    // still leaves what arrived, for the resume.
                    if let (Some(seq), Some(data)) =
                        (v.get("seq").and_then(|s| s.as_u64()), v.get("data").and_then(|d| d.as_str()))
                    {
                        let mut g = received_ref.lock().unwrap();
                        if let Some(slot) = g.get_mut(seq as usize) {
                            *slot = Some(data.to_string());
                        }
                        let done = count(&g);
                        drop(g);
                        let _ = progress_app.emit("image-progress", json!({ "done": done_before + done, "total": total }));
                    }
                })
                .await;
            if let Err(e) = res {
                result = Err(e);
                break;
            }
        }
        let got = received.into_inner().unwrap();
        if let Err(e) = result {
            // A definitive server verdict ("unknown image chunk") means the staged picture
            // is gone — a resume is pointless, Retry must generate anew. Anything else is
            // the transport, so park the progress for a resume.
            if !e.contains("unknown image chunk") {
                *parts = got;
                transport.set_staged_download(dl.clone()).await;
                return Err(format!("image download interrupted: {e} — Retry resumes it"));
            }
            return Err(format!("image download failed: {e}"));
        }
        let expected = img.get("bytes").and_then(|b| b.as_u64()).unwrap_or(0) as usize;
        let mut data = String::with_capacity(expected);
        for (i, p) in got.into_iter().enumerate() {
            data.push_str(&p.ok_or_else(|| format!("image chunk {i}/{n} never arrived"))?);
        }
        if expected > 0 && data.len() != expected {
            return Err(format!("image download incomplete ({} of {expected} bytes)", data.len()));
        }
        done_before += n;
        out.push(json!({
            "mimeType": img.get("mimeType").cloned().unwrap_or_else(|| json!("image/jpeg")),
            "data": data,
        }));
    }
    dl.resp["images"] = json!(out);
    Ok(dl.resp)
}

/// Stage a vision image on the server before a chat references it. Large images
/// are unreliable as one mixnet message (fragment loss) and give no progress —
/// so the client uploads in small acked chunks. `upload_begin` reserves a slot.
#[tauri::command]
async fn upload_begin(
    app: AppHandle,
    transport: State<'_, Arc<Transport>>,
    #[allow(non_snake_case)] mimeType: String,
    #[allow(non_snake_case)] totalBytes: u64,
) -> Result<Value, String> {
    let w = wallet::load(&data_dir(&app)?);
    let srv = server_addr(&w)?;
    let req = json!({"v":PROTO,"kind":"upload.begin","id":rand_hex(16),"mimeType":mimeType,"totalBytes":totalBytes});
    let resp = transport.round_trip(&srv, &req, SURBS_SMALL, TIMEOUT_MS).await?;
    Ok(json!({ "uploadId": resp.get("uploadId") }))
}

/// Send one acked chunk of a staged image. The frontend loops this and derives
/// real % + speed from each returned `received` count.
#[tauri::command]
async fn upload_chunk(
    app: AppHandle,
    transport: State<'_, Arc<Transport>>,
    #[allow(non_snake_case)] uploadId: String,
    seq: u64,
    data: String,
) -> Result<Value, String> {
    let w = wallet::load(&data_dir(&app)?);
    let srv = server_addr(&w)?;
    let req = json!({"v":PROTO,"kind":"upload.chunk","id":rand_hex(16),"uploadId":uploadId,"seq":seq,"data":data});
    // The ack is tiny — a small SURB budget avoids attaching needless reply blocks
    // to every (large) chunk upload.
    let resp = transport.round_trip(&srv, &req, SURBS_SMALL, TIMEOUT_MS).await?;
    Ok(json!({ "received": resp.get("received") }))
}

/// Pipelined upload: send ALL chunks concurrently (not one-round-trip-at-a-time), so they
/// aren't serialised on the client lock. Emits "upload-progress" (server-received bytes)
/// as acks land. Privacy is unchanged vs sequential — the nym send stream still paces +
/// cover-mixes every packet (fire_and_collect doc). `chunks[i]` is chunk `i`'s base64.
#[tauri::command]
async fn upload_pipeline(
    app: AppHandle,
    transport: State<'_, Arc<Transport>>,
    #[allow(non_snake_case)] uploadId: String,
    chunks: Vec<String>,
) -> Result<Value, String> {
    let _op = transport.begin_op().await;
    let w = wallet::load(&data_dir(&app)?);
    let srv = server_addr(&w)?;
    let n = chunks.len();
    let requests: Vec<Value> = chunks
        .iter()
        .enumerate()
        .map(|(i, data)| json!({"v":PROTO,"kind":"upload.chunk","id":rand_hex(16),"uploadId":uploadId,"seq":i,"data":data}))
        .collect();
    let app2 = app.clone();
    transport
        .fire_and_collect(&srv, requests, SURBS_SMALL, TIMEOUT_MS, move |received| {
            let _ = app2.emit("upload-progress", received);
        })
        .await?;
    Ok(json!({ "uploaded": n }))
}

/// One route edge (entry or exit) as the UI shows it: gateway identity + its
/// self-reported country (empty if the directory didn't resolve it).
fn edge_json(id: &str, info: Option<nym::GatewayInfo>) -> Value {
    match info {
        Some(g) => json!({ "id": id, "country": g.country, "host": g.host }),
        None => json!({ "id": id, "country": "", "host": "" }),
    }
}

/// DEV latency probe: one minimal round-trip to the server (`ping` → `pong`, no
/// session/DB/provider work) timed on the Rust side, so the result is the mixnet's
/// own round-trip latency — the honest way to see what the speed slider actually does.
#[tauri::command]
async fn mixnet_ping(app: AppHandle, transport: State<'_, Arc<Transport>>) -> Result<Value, String> {
    let w = wallet::load(&data_dir(&app)?);
    let srv = server_addr(&w)?;
    let req = json!({ "v": PROTO, "kind": "ping", "id": rand_hex(8) });
    let t0 = std::time::Instant::now();
    transport.round_trip(&srv, &req, SURBS_SMALL, TIMEOUT_MS).await?;
    Ok(json!({ "ms": t0.elapsed().as_millis() as u64 }))
}

/// The honestly-knowable edges of the current mixnet route:
///   entry = this client's gateway (selectable), exit = the server's gateway.
/// The two middle mix hops are re-randomised per packet and are NOT reported.
#[tauri::command]
async fn mixnet_route(app: AppHandle, transport: State<'_, Arc<Transport>>) -> Result<Value, String> {
    diag(&app, "mixnet_route: begin");
    let w = wallet::load(&data_dir(&app)?);
    let server = server_addr(&w).ok();

    // The UI polls this every ~2.5s while it shows "connecting". The poll itself
    // must stay NON-BLOCKING (a status query queued behind a long round trip
    // froze the route grey while traffic was flowing) — so it reads the
    // lock-free view and, when down, kicks a single background reconnect task.
    let live = transport.is_connected();
    if !live {
        spawn_rebuild(app.clone(), transport.inner().clone());
    }

    let entry = match transport.entry_gateway_id().await {
        Some(id) => {
            let info = transport.gateway_info(&id).await;
            edge_json(&id, info)
        }
        None => Value::Null,
    };
    let exit = match server.as_deref().and_then(Transport::gateway_of) {
        Some(id) => {
            let info = transport.gateway_info(&id).await;
            edge_json(&id, info)
        }
        None => Value::Null,
    };
    diag(&app, &format!("mixnet_route: about to respond (live={live})"));
    Ok(json!({ "entry": entry, "exit": exit, "chosen": w.entry_gateway, "random": w.entry_random, "live": live }))
}

/// Directory nodes usable as an entry gateway, for the picker.
#[tauri::command]
async fn list_entry_gateways(transport: State<'_, Arc<Transport>>) -> Result<Value, String> {
    let list = transport.entry_gateways().await?;
    Ok(json!(list))
}

/// The server's identities (every address from its catalog reply, plus the one in
/// use) with each exit gateway's self-reported country, for the picker under
/// Account → Server. Same server behind every entry — picking one only changes the
/// exit hop.
#[tauri::command]
async fn server_identities(app: AppHandle, transport: State<'_, Arc<Transport>>) -> Result<Value, String> {
    let w = wallet::load(&data_dir(&app)?);
    let current = w.server.clone().unwrap_or_default();
    let mut addrs = w.server_alternates.clone();
    if !current.is_empty() && !addrs.contains(&current) {
        addrs.insert(0, current.clone());
    }
    let mut out = Vec::with_capacity(addrs.len());
    for addr in addrs {
        let Some(gw) = Transport::gateway_of(&addr) else { continue };
        let info = transport.gateway_info(&gw).await;
        let (country, host) = info.map(|g| (g.country, g.host)).unwrap_or_default();
        out.push(json!({
            "address": addr,
            "gateway": gw,
            "country": country,
            "host": host,
            "current": addr == current,
        }));
    }
    Ok(json!(out))
}

/// Choose (or clear, with null) the entry gateway. Persists it and drops the
/// live client so the next request re-attaches through the chosen gateway.
#[tauri::command]
async fn set_entry_gateway(
    app: AppHandle,
    transport: State<'_, Arc<Transport>>,
    id: Option<String>,
) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let mut w = wallet::load(&dir);
    let id = id.and_then(|s| {
        let t = s.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    });
    // Picking a gateway is a choice to keep it; clearing (null) means random mode.
    w.entry_random = id.is_none();
    w.entry_gateway = id.clone();
    wallet::save(&dir, &w)?;
    transport.set_entry_gateway(id.clone()).await;
    Ok(json!({ "entry_gateway": id, "entry_random": w.entry_random }))
}

/// "Use random gateway" on/off. On: forget the pinned gateway, a random directory node on
/// every connect. Off: pin one of the operator's gateways again (the picker can change it).
/// Developer switch: pay chats with coins instead of a session balance.
#[tauri::command]
fn set_coin_chat(app: AppHandle, on: bool) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let mut w = wallet::load(&dir);
    w.legacy_session_chat = !on;
    wallet::save(&dir, &w)?;
    log::info!("[tender] coin-paid chat {}", if on { "on" } else { "off" });
    Ok(json!({ "coinChat": on }))
}

#[tauri::command]
async fn set_entry_random(app: AppHandle, transport: State<'_, Arc<Transport>>, on: bool) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let mut w = wallet::load(&dir);
    w.entry_random = on;
    w.entry_gateway = if on { None } else { Some(nym::random_hermes_gateway()) };
    wallet::save(&dir, &w)?;
    transport.set_entry_gateway(w.entry_gateway.clone()).await;
    Ok(json!({ "entry_gateway": w.entry_gateway, "entry_random": w.entry_random }))
}

/// Save a base64 image to a user-chosen path via a native "save as…" dialog.
/// The webview can't trigger downloads, so image saves route through here.
/// Returns the chosen path, or null if the user cancelled.
#[cfg(not(any(target_os = "ios", target_os = "android")))]
#[tauri::command]
async fn save_image(data: String, filename: String) -> Result<Option<String>, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let bytes = B64.decode(data.as_bytes()).map_err(|e| format!("bad image data: {e}"))?;
    let handle = rfd::AsyncFileDialog::new()
        .set_file_name(&filename)
        .save_file()
        .await;
    match handle {
        Some(f) => {
            f.write(&bytes).await.map_err(|e| e.to_string())?;
            Ok(Some(f.path().to_string_lossy().to_string()))
        }
        None => Ok(None),
    }
}

// ---- saving a document (the purchase receipt) --------------------------------------
// Separate from save_image: a receipt is a FILE, not a picture. On desktop the user picks
// where it goes, on iOS the share sheet does, on Android it lands in Downloads — the three
// places each platform's users look for a saved document. Deliberately not automatic: the
// buyer asked for the download, so the act stays theirs.

#[cfg(not(any(target_os = "ios", target_os = "android")))]
#[tauri::command]
async fn save_file(data: String, filename: String) -> Result<Option<String>, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let bytes = B64.decode(data.as_bytes()).map_err(|e| format!("bad file data: {e}"))?;
    let handle = rfd::AsyncFileDialog::new().set_file_name(&filename).save_file().await;
    match handle {
        Some(f) => {
            f.write(&bytes).await.map_err(|e| e.to_string())?;
            Ok(Some(f.path().to_string_lossy().to_string()))
        }
        None => Ok(None), // cancelled — not an error
    }
}

/// iOS: no user-visible filesystem, so the document goes through the share sheet ("Save to
/// Files", Mail, AirDrop). Written to the app's tmp directory first because the sheet takes
/// a file URL.
#[cfg(target_os = "ios")]
#[tauri::command]
async fn save_file(app: AppHandle, data: String, filename: String) -> Result<Option<String>, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let bytes = B64.decode(data.as_bytes()).map_err(|e| format!("bad file data: {e}"))?;
    let name: String = filename.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-' || *c == '_').collect();
    let path = std::env::temp_dir().join(if name.is_empty() { "receipt.pdf".into() } else { name });
    std::fs::write(&path, &bytes).map_err(|e| format!("could not stage the file: {e}"))?;
    let p = path.to_string_lossy().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let _ = tx.send(ios_share::present_share_sheet(&p));
    })
    .map_err(|e| e.to_string())?;
    rx.await.map_err(|e| e.to_string())??;
    Ok(Some("shared".into()))
}

/// Android: MediaStore Downloads. Needs no permission from API 29 on (scoped storage), and
/// puts the file exactly where a browser download would go, so the Files app finds it.
#[cfg(target_os = "android")]
#[tauri::command]
async fn save_file(data: String, filename: String) -> Result<Option<String>, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let bytes = B64.decode(data.as_bytes()).map_err(|e| format!("bad file data: {e}"))?;
    let name: String = filename
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
        .collect();
    let name = if name.is_empty() { "receipt.pdf".to_string() } else { name };
    android_save_to_downloads(bytes, name).map(Some)
}

/// The JNI half of the above. Runs on the Android main thread (the only place wry hands us
/// the Activity), and answers over a channel — the caller is an async command and must not
/// block that thread itself.
#[cfg(target_os = "android")]
fn android_save_to_downloads(bytes: Vec<u8>, filename: String) -> Result<String, String> {
    use jni::{jni_sig, jni_str};
    let (tx, rx) = std::sync::mpsc::channel::<Result<String, String>>();
    tauri::wry::prelude::dispatch(move |env, activity, _webview| {
        let out = (|| -> Result<String, String> {
            let raw_vm = env.get_java_vm().map_err(|e| format!("no JavaVM: {e}"))?.get_java_vm_pointer();
            let raw_ctx = activity.as_raw();
            let vm = unsafe { jni::JavaVM::from_raw(raw_vm.cast()) };
            let attached: Result<Result<String, String>, jni::errors::Error> =
                vm.attach_current_thread(|env| Ok((|| -> Result<String, String> {
                let ctx = unsafe { jni::objects::JObject::from_raw(env, raw_ctx.cast()) };
                let jerr = |what: &'static str| move |e: jni::errors::Error| format!("{what}: {e}");

                // Scoped storage arrived in API 29; below that this would need
                // WRITE_EXTERNAL_STORAGE and a runtime prompt. Say so rather than fail oddly.
                let sdk = env
                    .get_static_field(jni_str!("android/os/Build$VERSION"), jni_str!("SDK_INT"), jni_sig!("I"))
                    .and_then(|v| v.i())
                    .map_err(jerr("SDK_INT"))?;
                if sdk < 29 {
                    return Err("saving files needs Android 10 or newer".into());
                }

                let resolver = env
                    .call_method(
                        &ctx,
                        jni_str!("getContentResolver"),
                        jni_sig!("()Landroid/content/ContentResolver;"),
                        &[],
                    )
                    .and_then(|v| v.l())
                    .map_err(jerr("getContentResolver"))?;

                // Literal column names on purpose: these are the documented VALUES of
                // MediaStore.MediaColumns.* and Environment.DIRECTORY_DOWNLOADS, and reading
                // them back out of the classes would be three more JNI round trips for nothing.
                let values = env
                    .new_object(jni_str!("android/content/ContentValues"), jni_sig!("()V"), &[])
                    .map_err(jerr("ContentValues"))?;
                for (k, v) in [
                    ("_display_name", filename.as_str()),
                    ("mime_type", "application/pdf"),
                    ("relative_path", "Download"),
                ] {
                    let jk = env.new_string(k).map_err(jerr("key"))?;
                    let jv = env.new_string(v).map_err(jerr("value"))?;
                    env.call_method(
                        &values,
                        jni_str!("put"),
                        jni_sig!("(Ljava/lang/String;Ljava/lang/String;)V"),
                        &[(&jk).into(), (&jv).into()],
                    )
                    .map_err(jerr("ContentValues.put"))?;
                }

                let collection = env
                    .get_static_field(
                        jni_str!("android/provider/MediaStore$Downloads"),
                        jni_str!("EXTERNAL_CONTENT_URI"),
                        jni_sig!("Landroid/net/Uri;"),
                    )
                    .and_then(|v| v.l())
                    .map_err(jerr("EXTERNAL_CONTENT_URI"))?;

                let item = env
                    .call_method(
                        &resolver,
                        jni_str!("insert"),
                        jni_sig!("(Landroid/net/Uri;Landroid/content/ContentValues;)Landroid/net/Uri;"),
                        &[(&collection).into(), (&values).into()],
                    )
                    .and_then(|v| v.l())
                    .map_err(jerr("insert"))?;
                if item.is_null() {
                    return Err("Android refused to create the file in Downloads".into());
                }

                let stream = env
                    .call_method(
                        &resolver,
                        jni_str!("openOutputStream"),
                        jni_sig!("(Landroid/net/Uri;)Ljava/io/OutputStream;"),
                        &[(&item).into()],
                    )
                    .and_then(|v| v.l())
                    .map_err(jerr("openOutputStream"))?;
                let arr = env.byte_array_from_slice(&bytes).map_err(jerr("byte[]"))?;
                env.call_method(&stream, jni_str!("write"), jni_sig!("([B)V"), &[(&arr).into()])
                    .map_err(jerr("write"))?;
                env.call_method(&stream, jni_str!("close"), jni_sig!("()V"), &[])
                    .map_err(jerr("close"))?;
                Ok(format!("Downloads/{filename}"))
            })()));
            attached.map_err(|e| format!("attach: {e}"))?
        })();
        let _ = tx.send(out);
    });
    rx.recv().map_err(|_| "the Android main thread did not answer".to_string())?
}

/// Android (first build): pictures stay in the chat — the gallery/share path needs the
/// MediaStore plugin, which is not wired yet. Says so instead of failing silently.
#[cfg(target_os = "android")]
#[tauri::command]
async fn save_image(data: String, filename: String) -> Result<Option<String>, String> {
    let _ = (data, filename);
    Err("saving pictures to the gallery is not available on Android yet".into())
}

// iOS: generated images land in the Photos library (UIKit must run on the
// main thread; the command hops there and reports back over a oneshot).
#[cfg(target_os = "ios")]
#[tauri::command]
async fn save_image(app: AppHandle, data: String, filename: String) -> Result<Option<String>, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let _ = filename;
    let bytes = B64.decode(data.as_bytes()).map_err(|e| format!("bad image data: {e}"))?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let _ = tx.send(ios_share::save_image_to_photos(&bytes));
    })
    .map_err(|e| e.to_string())?;
    rx.await.map_err(|e| e.to_string())??;
    Ok(Some("Photos".into()))
}

/// Export a text file (handover.md / .md.enc) through the OS. On iOS this
/// writes to a temp file and presents the native share sheet — the user picks
/// Files, AirDrop, another app… Encryption (when chosen) already happened in
/// the webview (PBKDF2 + AES-256-GCM), so the file is opaque here either way.
#[cfg(target_os = "ios")]
#[tauri::command]
async fn share_text(app: AppHandle, filename: String, text: String) -> Result<(), String> {
    // Basename only — the name came from the UI, not from a path.
    let safe: String = filename
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let path = std::env::temp_dir().join(if safe.is_empty() { "handover.md".into() } else { safe });
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    let path_str = path.to_string_lossy().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let _ = tx.send(ios_share::present_share_sheet(&path_str));
    })
    .map_err(|e| e.to_string())?;
    rx.await.map_err(|e| e.to_string())?
}

#[cfg(not(target_os = "ios"))]
#[tauri::command]
async fn share_text(filename: String, text: String) -> Result<(), String> {
    let _ = (filename, text);
    Err("share_text is the iOS export path — desktop saves via the browser download".into())
}

#[cfg(target_os = "ios")]
mod ios_share {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_foundation::{NSArray, NSData, NSString, NSURL};
    use objc2_ui_kit::{UIActivityViewController, UIApplication, UIImage};

    pub fn save_image_to_photos(bytes: &[u8]) -> Result<(), String> {
        let _mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
        let data = NSData::with_bytes(bytes);
        let img = UIImage::initWithData(UIImage::alloc(), &data)
            .ok_or("could not decode the image data")?;
        // Fire-and-forget: iOS shows its own permission prompt on first use
        // (NSPhotoLibraryAddUsageDescription) and saves asynchronously.
        unsafe { img.write_to_saved_photos_album(None, None, std::ptr::null_mut()) };
        Ok(())
    }

    // keyWindow is deprecated for multi-scene apps; this app is single-scene.
    #[allow(deprecated)]
    pub fn present_share_sheet(path: &str) -> Result<(), String> {
        let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
        unsafe {
            let url = NSURL::fileURLWithPath(&NSString::from_str(path));
            let obj: Retained<AnyObject> = Retained::into_super(Retained::into_super(url));
            let items = NSArray::from_retained_slice(&[obj]);
            let avc = UIActivityViewController::initWithActivityItems_applicationActivities(
                mtm.alloc(),
                &items,
                None,
            );
            let app = UIApplication::sharedApplication(mtm);
            let window = app.keyWindow().ok_or("no key window")?;
            let root = window.rootViewController().ok_or("no root view controller")?;
            // iPad presents this as a popover and needs an anchor; iPhone ignores it.
            if let Some(pop) = avc.popoverPresentationController() {
                pop.setSourceView(Some(&window));
            }
            root.presentViewController_animated_completion(&avc, true, None);
        }
        Ok(())
    }
}

// Native image/camera picker — the WKWebView `<input type=file>` is unreliable on iOS
// (it won't reopen after a cancel), so the "+" calls this instead. Presents a
// UIImagePickerController and hands the picked photo back as JPEG bytes.
#[cfg(target_os = "ios")]
mod ios_picker {
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObjectProtocol};
    use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
    use objc2_foundation::{NSObject, NSString};
    use objc2_ui_kit::{
        UIApplication, UIImagePickerController, UIImagePickerControllerDelegate,
        UIImagePickerControllerSourceType, UINavigationControllerDelegate,
    };
    use std::cell::RefCell;
    use tokio::sync::oneshot::Sender;

    // UIKit only weakly references the picker's delegate, so keep the last one alive here
    // (main thread) until the next present() replaces it. Never cleared from inside a
    // delegate callback — that could dealloc `self` mid-method.
    thread_local! {
        static KEEP: RefCell<Option<Retained<PickerDelegate>>> = const { RefCell::new(None) };
    }

    pub struct Ivars {
        tx: RefCell<Option<Sender<Result<Option<Vec<u8>>, String>>>>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "ScraiImagePickerDelegate"]
        #[ivars = Ivars]
        struct PickerDelegate;

        unsafe impl NSObjectProtocol for PickerDelegate {}
        unsafe impl UINavigationControllerDelegate for PickerDelegate {}

        unsafe impl UIImagePickerControllerDelegate for PickerDelegate {
            #[unsafe(method(imagePickerController:didFinishPickingMediaWithInfo:))]
            fn did_finish(&self, picker: &UIImagePickerController, info: &AnyObject) {
                let bytes = unsafe { extract_jpeg(info) };
                self.reply(bytes);
                picker.dismissViewControllerAnimated_completion(true, None);
            }

            #[unsafe(method(imagePickerControllerDidCancel:))]
            fn did_cancel(&self, picker: &UIImagePickerController) {
                self.reply(Ok(None));
                picker.dismissViewControllerAnimated_completion(true, None);
            }
        }
    );

    impl PickerDelegate {
        fn new(mtm: MainThreadMarker, tx: Sender<Result<Option<Vec<u8>>, String>>) -> Retained<Self> {
            let this = mtm.alloc::<Self>().set_ivars(Ivars { tx: RefCell::new(Some(tx)) });
            unsafe { msg_send![super(this), init] }
        }
        fn reply(&self, v: Result<Option<Vec<u8>>, String>) {
            if let Some(tx) = self.ivars().tx.borrow_mut().take() {
                let _ = tx.send(v);
            }
        }
    }

    // Pull the original UIImage out of the info dict and JPEG-encode it. Copy the bytes
    // immediately — the returned NSData is autoreleased and only valid during this call.
    unsafe fn extract_jpeg(info: &AnyObject) -> Result<Option<Vec<u8>>, String> {
        let key = NSString::from_str("UIImagePickerControllerOriginalImage");
        let image: *mut AnyObject = msg_send![info, objectForKey: &*key];
        if image.is_null() {
            return Ok(None);
        }
        extern "C-unwind" {
            fn UIImageJPEGRepresentation(image: *mut AnyObject, quality: f64) -> *mut AnyObject;
        }
        let data: *mut AnyObject = UIImageJPEGRepresentation(image, 0.85);
        if data.is_null() {
            return Err("could not JPEG-encode the picked image".into());
        }
        let len: usize = msg_send![data, length];
        let ptr: *const u8 = msg_send![data, bytes];
        if ptr.is_null() || len == 0 {
            return Err("picked image encoded to zero bytes".into());
        }
        Ok(Some(std::slice::from_raw_parts(ptr, len).to_vec()))
    }

    #[allow(deprecated)]
    pub fn present(source: &str, tx: Sender<Result<Option<Vec<u8>>, String>>) {
        let mtm = match MainThreadMarker::new() {
            Some(m) => m,
            None => {
                let _ = tx.send(Err("picker must run on the main thread".into()));
                return;
            }
        };
        let want_camera = source == "camera";
        let src_type = if want_camera {
            UIImagePickerControllerSourceType::Camera
        } else {
            UIImagePickerControllerSourceType::PhotoLibrary
        };
        if !unsafe { UIImagePickerController::isSourceTypeAvailable(src_type, mtm) } {
            let _ = tx.send(Err(if want_camera {
                "no camera available on this device".into()
            } else {
                "photo library unavailable".into()
            }));
            return;
        }
        let app = UIApplication::sharedApplication(mtm);
        let Some(window) = app.keyWindow() else {
            let _ = tx.send(Err("no key window".into()));
            return;
        };
        let Some(root) = window.rootViewController() else {
            let _ = tx.send(Err("no root view controller".into()));
            return;
        };
        let picker = unsafe { UIImagePickerController::new(mtm) };
        unsafe { picker.setSourceType(src_type) };
        let delegate = PickerDelegate::new(mtm, tx);
        // `delegate` is untyped `id` on UIImagePickerController; msg_send passes it directly.
        let _: () = unsafe { msg_send![&*picker, setDelegate: &*delegate] };
        KEEP.with(|k| *k.borrow_mut() = Some(delegate));
        unsafe { root.presentViewController_animated_completion(&picker, true, None) };
    }
}

/// Native iOS image/camera picker. `source` is "library" or "camera". Returns
/// {mimeType, name, dataB64} for the picked photo, or null if the user cancelled.
#[cfg(target_os = "ios")]
#[tauri::command]
async fn pick_image(app: AppHandle, source: String) -> Result<Option<serde_json::Value>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || ios_picker::present(&source, tx))
        .map_err(|e| e.to_string())?;
    let bytes = rx.await.map_err(|e| e.to_string())??;
    Ok(bytes.map(|b| {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        serde_json::json!({ "mimeType": "image/jpeg", "name": "photo.jpg", "dataB64": B64.encode(&b) })
    }))
}

#[cfg(not(target_os = "ios"))]
#[tauri::command]
async fn pick_image(source: String) -> Result<Option<serde_json::Value>, String> {
    let _ = source;
    Err("the native picker is only available on iOS".into())
}

// H1 — native, biometric-gated recovery-phrase reveal. The 24 words are shown ONLY in a
// native alert; they never cross back into the (potentially XSS'd) webview, and the reveal
// button lives on a native action sheet the webview can present but not tap.
#[cfg(target_os = "ios")]
mod ios_secure {
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{msg_send, AnyThread, MainThreadMarker};
    use objc2_foundation::{NSArray, NSDate, NSDictionary, NSError, NSNumber, NSString};
    use objc2_local_authentication::{LAContext, LAPolicy};
    use objc2_ui_kit::{
        UIAlertAction, UIAlertActionStyle, UIAlertController, UIAlertControllerStyle, UIApplication,
        UIPasteboard, UIPasteboardOptionExpirationDate, UIPasteboardOptionLocalOnly, UIViewController,
    };
    use std::path::PathBuf;
    use tauri::AppHandle;

    #[allow(deprecated)]
    fn present(mtm: MainThreadMarker, vc: &UIViewController) {
        let app = UIApplication::sharedApplication(mtm);
        if let Some(window) = app.keyWindow() {
            if let Some(root) = window.rootViewController() {
                unsafe { root.presentViewController_animated_completion(vc, true, None) };
            }
        }
    }

    // A plain native alert with a single nil-handler "Done" button.
    fn alert(mtm: MainThreadMarker, title: &str, message: &str) {
        let a = UIAlertController::alertControllerWithTitle_message_preferredStyle(
            Some(&NSString::from_str(title)),
            Some(&NSString::from_str(message)),
            UIAlertControllerStyle::Alert,
            mtm,
        );
        let done = UIAlertAction::actionWithTitle_style_handler(
            Some(&NSString::from_str("Done")),
            UIAlertActionStyle::Default,
            None,
            mtm,
        );
        a.addAction(&done);
        present(mtm, &a);
    }

    /// How long the copied phrase stays on the pasteboard. Long enough to switch to a
    /// password manager and paste, short enough that it is not still there tomorrow.
    const PASTEBOARD_SECS: f64 = 60.0;

    /// Put the phrase on the pasteboard from NATIVE code: the words go from the Rust wallet
    /// to UIPasteboard without ever entering the webview, so the H1 boundary holds while the
    /// user still gets to paste into 1Password. Two options make that copy less dangerous
    /// than a plain one:
    ///   · localOnly      — no Universal Clipboard, so the seed does not hop to the Mac or iPad
    ///   · expirationDate — iOS clears it after a minute, without us having to
    fn copy_phrase(text: &str) {
        let value = NSString::from_str(text);
        let utf8 = NSString::from_str("public.utf8-plain-text");
        let v: &AnyObject = &value;
        let item = NSDictionary::<NSString, AnyObject>::from_slices(&[&*utf8], &[v]);
        let items = NSArray::from_retained_slice(&[item]);

        let local = NSNumber::numberWithBool(true);
        let until = NSDate::dateWithTimeIntervalSinceNow(PASTEBOARD_SECS);
        let (l, u): (&AnyObject, &AnyObject) = (&local, &until);
        let opts = unsafe {
            NSDictionary::<NSString, AnyObject>::from_slices(
                &[UIPasteboardOptionLocalOnly, UIPasteboardOptionExpirationDate],
                &[l, u],
            )
        };
        let pb = unsafe { UIPasteboard::generalPasteboard() };
        unsafe { pb.setItems_options(&items, &opts) };
    }

    // The phrase alert, which unlike `alert` carries a Copy button. The 24 words are not
    // selectable in a UIAlertController — a tester pointed out that reading them off the
    // screen and typing them into a password manager is the whole interaction (2026-09-06)
    // — and the answer is a native copy, not moving the phrase back into the webview.
    fn alert_phrase(mtm: MainThreadMarker, title: &str, phrase: &str) {
        let a = UIAlertController::alertControllerWithTitle_message_preferredStyle(
            Some(&NSString::from_str(title)),
            Some(&NSString::from_str(phrase)),
            UIAlertControllerStyle::Alert,
            mtm,
        );
        let text = phrase.to_string();
        let handler = RcBlock::new(move |_a: core::ptr::NonNull<UIAlertAction>| copy_phrase(&text));
        let copy = UIAlertAction::actionWithTitle_style_handler(
            Some(&NSString::from_str("Copy")),
            UIAlertActionStyle::Default,
            Some(&handler),
            mtm,
        );
        let done = UIAlertAction::actionWithTitle_style_handler(
            Some(&NSString::from_str("Done")),
            UIAlertActionStyle::Cancel,
            None,
            mtm,
        );
        a.addAction(&copy);
        a.addAction(&done);
        present(mtm, &a);
    }

    // Biometric-gate (Face ID / Touch ID / passcode), then show the phrase natively. Must be
    // called on the main thread. The LAContext reply lands on a private thread, so we hop
    // back to main (via the AppHandle) to touch UIKit + read the wallet.
    pub fn reveal_phrase(app: AppHandle, dir: PathBuf, title: &'static str) {
        let ctx = unsafe { LAContext::new() };
        let reason = NSString::from_str("Show your recovery phrase");
        let reply = RcBlock::new(move |ok: Bool, _err: *mut NSError| {
            let (app, dir) = (app.clone(), dir.clone());
            let ok = ok.as_bool();
            let _ = app.run_on_main_thread(move || {
                let Some(mtm) = MainThreadMarker::new() else { return };
                if !ok {
                    alert(mtm, "Not verified", "Face ID / passcode was cancelled or failed.");
                    return;
                }
                match crate::wallet::load(&dir).mnemonic {
                    Some(m) => alert_phrase(mtm, title, &m),
                    None => alert(mtm, "No account", "No recovery phrase on this device."),
                }
            });
        });
        unsafe {
            let _: () = msg_send![
                &ctx,
                evaluatePolicy: LAPolicy::DeviceOwnerAuthentication,
                localizedReason: &*reason,
                reply: &*reply,
            ];
        }
    }

    // Present the "Account security" action sheet. Its "Reveal recovery phrase" action is a
    // NATIVE button: webview JS can present this sheet but cannot tap the action, so it can
    // neither trigger the biometric prompt nor read the phrase.
    #[allow(deprecated)]
    pub fn open_account_security(app: AppHandle, dir: PathBuf) {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let sheet = UIAlertController::alertControllerWithTitle_message_preferredStyle(
            Some(&NSString::from_str("Account security")),
            Some(&NSString::from_str(
                "Your 24-word recovery phrase is the only way back to your balance. It is shown only on this device.",
            )),
            UIAlertControllerStyle::ActionSheet,
            mtm,
        );
        let handler = RcBlock::new(move |_a: core::ptr::NonNull<UIAlertAction>| {
            reveal_phrase(app.clone(), dir.clone(), "Recovery phrase");
        });
        let reveal = UIAlertAction::actionWithTitle_style_handler(
            Some(&NSString::from_str("Reveal recovery phrase")),
            UIAlertActionStyle::Default,
            Some(&handler),
            mtm,
        );
        let cancel = UIAlertAction::actionWithTitle_style_handler(
            Some(&NSString::from_str("Cancel")),
            UIAlertActionStyle::Cancel,
            None,
            mtm,
        );
        sheet.addAction(&reveal);
        sheet.addAction(&cancel);
        // iPad presents an action sheet as a popover and needs an anchor; iPhone ignores it.
        if let Some(pop) = sheet.popoverPresentationController() {
            let uiapp = UIApplication::sharedApplication(mtm);
            if let Some(w) = uiapp.keyWindow() {
                pop.setSourceView(Some(&w));
            }
        }
        let _keep: Option<Retained<UIViewController>> = None;
        present(mtm, &sheet);
    }
}

/// Present the native, biometric-gated Account Security screen (H1). iOS only.
#[cfg(target_os = "ios")]
#[tauri::command]
fn open_account_security(app: AppHandle) -> Result<(), String> {
    let dir = data_dir(&app)?;
    let app2 = app.clone();
    app.run_on_main_thread(move || ios_secure::open_account_security(app2, dir))
        .map_err(|e| e.to_string())
}

#[cfg(not(target_os = "ios"))]
#[tauri::command]
fn open_account_security() -> Result<(), String> {
    Err("the native account-security screen is iOS-only".into())
}

/// Is this a URL we are willing to hand to the OS browser? http(s) only, a non-empty
/// host, and not one character of whitespace or control code anywhere in it.
///
/// The scheme test is the policy; the whitespace/control test is defence in depth. A URL
/// that reaches here is frequently NOT ours — since 0.4.4 the markdown renderer turns
/// every http(s) run in a MODEL ANSWER into a clickable link, and `faucetUrl` / the
/// update notice come from whatever server the app is pointed at. So this string must be
/// treated as hostile text, and never as something a launcher may re-parse (H1).
pub(crate) fn is_openable_url(url: &str) -> bool {
    // Support mail. The app writes these itself (About → Write to support, and the row on
    // the payment panel); nothing the SERVER supplies can become one, because `siteUrl` and
    // the checkout link are both validated as https before they get anywhere near here.
    //
    // Narrow anyway, and for one specific reason: a mailto's query is a header list, so a
    // `?bcc=` or a `?to=` in a hand-made link is a way to make somebody's own mail client
    // send to a third party. Only `subject` and `body` are recognised, and only ONE address.
    if let Some(rest) = url.strip_prefix("mailto:") {
        if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return false;
        }
        let (addr, query) = rest.split_once('?').unwrap_or((rest, ""));
        // One plain address: something@something.tld, no comma-separated list.
        let plain = |a: &str| {
            let Some((user, host)) = a.split_once('@') else { return false };
            !user.is_empty()
                && host.contains('.')
                && !host.starts_with('.')
                && !host.ends_with('.')
                && !a.contains(',')
                && a.chars().all(|c| c.is_ascii_alphanumeric() || "._%+-@".contains(c))
        };
        if !plain(addr) {
            return false;
        }
        return query.is_empty()
            || query.split('&').all(|kv| matches!(kv.split_once('='), Some((k, _)) if k == "subject" || k == "body"));
    }
    let rest = match url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) {
        Some(r) => r,
        None => return false,
    };
    // A host has to exist and has to end somewhere sane — "https:///x" or "https://?x"
    // are not links a browser should be handed.
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() {
        return false;
    }
    // Whitespace and control characters have no business in a URL; they are also what
    // splits arguments and lines in every launcher and log we might ever pass through.
    !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Open an http(s) URL in the OS default browser. The webview itself won't
/// follow target=_blank links, so provider T&C / checkout links route here.
///
/// EVERY platform goes through the opener plugin — deliberately, and not just because
/// mobile cannot spawn processes. Until 2026-09-04 the desktop arms shelled out, and the
/// Windows one was `cmd /C start "" <url>`: Rust only quotes an argument containing a
/// space, so a URL without one reached `cmd.exe` bare and `cmd` then read `&` in it as a
/// command separator — `https://x/a?b=1&calc.exe` ran calc. The plugin hands the URL to
/// `ShellExecuteExW` (Windows), `open` (macOS) and `xdg-open` (Linux) as DATA; nothing
/// re-parses it as shell code. Never replace this with a `Command` again.
#[tauri::command]
fn open_external(app: AppHandle, url: String) -> Result<(), String> {
    if !is_openable_url(&url) {
        return Err("only plain http(s) links and a support mailto are allowed".into());
    }
    #[cfg(target_os = "ios")]
    {
        return ios_open_url(&app, url);
    }
    #[cfg(not(target_os = "ios"))]
    {
        use tauri_plugin_opener::OpenerExt;
        app.opener().open_url(&url, None::<&str>).map_err(|e| e.to_string())
    }
}

/// iOS: hand the URL to UIApplication directly instead of to the opener plugin.
///
/// The plugin's iOS half is a Swift package that this project never links (gen/apple has
/// no plugin packages), so `open_url` resolved to nothing on device: the disclosure sheet
/// closed and Safari never opened — no error anywhere, because the webview drops the
/// rejected promise. Same reasoning as the native image picker: on iOS, go through UIKit.
#[cfg(target_os = "ios")]
fn ios_open_url(app: &AppHandle, url: String) -> Result<(), String> {
    app.run_on_main_thread(move || {
        use objc2::MainThreadMarker;
        use objc2_foundation::{NSDictionary, NSString, NSURL};
        use objc2_ui_kit::UIApplication;
        let Some(mtm) = MainThreadMarker::new() else {
            log::warn!("[open] not on the main thread — url not opened");
            return;
        };
        let s = NSString::from_str(&url);
        let Some(nsurl) = (unsafe { NSURL::URLWithString(&s) }) else {
            log::warn!("[open] UIKit rejected the url");
            return;
        };
        let options = NSDictionary::new();
        unsafe {
            UIApplication::sharedApplication(mtm).openURL_options_completionHandler(&nsurl, &options, None);
        }
    })
    .map_err(|e| e.to_string())
}

/// Android: hand rustls-platform-verifier the JVM + app Context so TLS verification can use
/// the system trust store. Must run before any networking (the Nym client's first directory
/// fetch is TLS). Tauri does not populate `ndk_context` (that panics: "android context was
/// not initialized"); wry's `dispatch` runs a closure on the Android main thread with the
/// JNI env (jni 0.21) and the Activity — we bridge the raw pointers into the verifier's
/// jni 0.22 types.
#[cfg(target_os = "android")]
fn init_android_tls_verifier() {
    tauri::wry::prelude::dispatch(|env, activity, _webview| {
        let raw_vm = match env.get_java_vm() {
            Ok(vm) => vm.get_java_vm_pointer(),
            Err(e) => {
                log::error!("scrai: android TLS verifier: no JavaVM from the activity env: {e}");
                return;
            }
        };
        let raw_ctx = activity.as_raw();
        let vm = unsafe { jni::JavaVM::from_raw(raw_vm.cast()) };
        let res: Result<(), jni::errors::Error> = vm.attach_current_thread(|env22| {
            let context = unsafe { jni::objects::JObject::from_raw(env22, raw_ctx.cast()) };
            rustls_platform_verifier::android::init_with_env(env22, context)
        });
        match res {
            Ok(()) => log::info!("scrai: android TLS verifier initialised"),
            Err(e) => log::error!("scrai: android TLS verifier init FAILED: {e} — mixnet directory fetches will not work"),
        }
    });
}

// ---------- App Store purchases (iOS, StoreKit 2 via iap_ios.rs) ----------
//
// The flow is: Apple's sheet → Apple's signed transaction → the server verifies it against
// Apple's root and credits the account → only then is the transaction finished with
// Apple. Until that reply, the transaction stays unfinished on the device, so a lost reply
// or a crash costs nothing: `iap_restore` re-sends whatever is unfinished, and the server
// answers a retry by the same account with "credited, nothing more to add".

#[cfg(target_os = "ios")]
async fn iap_verify_on_server(app: &AppHandle, transport: &Transport, jws: &str) -> Result<(u64, u64), String> {
    let transport = buy_transport(app, transport).await;
    let w = wallet::load(&data_dir(app)?);
    let srv = server_addr(&w)?;
    let m = w.mnemonic.ok_or("no account — create one first")?;
    let a = account::from_mnemonic(&m)?;
    let nonce = rand_hex(16);
    let sig = a.sign("iap", &nonce);
    let resp = transport
        .round_trip(
            &srv,
            &json!({"v":PROTO,"kind":"iap.verify","id":rand_hex(16),"jws":jws,
                    "publicKey":a.public_key_pem,"nonce":nonce,"sig":sig}),
            SURBS_SMALL,
            TIMEOUT_MS,
        )
        .await?;
    if let Some(e) = resp.get("error").and_then(|e| e.as_str()) {
        return Err(e.to_string());
    }
    Ok((
        resp.get("toku").and_then(|t| t.as_u64()).unwrap_or(0),
        resp.get("entitlement").and_then(|t| t.as_u64()).unwrap_or(0),
    ))
}

/// App Store product ids from a catalog reply: plain reverse-DNS strings, a handful at most.
fn remember_iap_products(resp: &Value) {
    let ids: Vec<String> = resp
        .get("iapProducts")
        .and_then(|p| p.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .filter(|s| !s.is_empty() && s.len() <= 120
                    && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_'))
                .map(str::to_string)
                .take(8)
                .collect()
        })
        .unwrap_or_default();
    *IAP_PRODUCTS.lock().unwrap_or_else(|e| e.into_inner()) = ids;
}

/// The remembered ids — or, when the catalog was cached before the server learned to
/// sell through the App Store (a deploy while the app was open), one fresh catalog fetch.
#[cfg(target_os = "ios")]
async fn iap_product_ids(app: &AppHandle, transport: &Transport) -> Vec<String> {
    let ids = IAP_PRODUCTS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if !ids.is_empty() {
        return ids;
    }
    let Ok(dir) = data_dir(app) else { return ids };
    let w = wallet::load(&dir);
    let Ok(srv) = server_addr(&w) else { return ids };
    if let Ok(resp) = transport
        .round_trip(&srv, &json!({"v":PROTO,"kind":"models","id":rand_hex(16)}), SURBS_META, META_TIMEOUT_MS)
        .await
    {
        remember_iap_products(&resp);
        if let Some(m) = resp.get("models") {
            transport.set_cached_models(m.clone()).await;
        }
    }
    IAP_PRODUCTS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The App Store's products for the ids the server sells, with localized prices.
#[cfg(target_os = "ios")]
#[tauri::command]
async fn iap_products(app: AppHandle, transport: State<'_, Arc<Transport>>) -> Result<Value, String> {
    let ids = Box::pin(iap_product_ids(&app, &transport)).await;
    if ids.is_empty() {
        return Err("this server sells nothing through the App Store".into());
    }
    Box::pin(iap_ios::products(&ids)).await
}

#[cfg(target_os = "ios")]
async fn iap_purchase_impl(app: AppHandle, transport: Arc<Transport>, product_id: String) -> Result<Value, String> {
    if !iap_product_ids(&app, &transport).await.contains(&product_id) {
        return Err("that product is not on sale here".into());
    }
    let r = iap_ios::purchase(&product_id).await?;
    let status = r.get("status").and_then(|s| s.as_str()).unwrap_or("").to_string();
    if status != "ok" {
        return Ok(json!({ "status": status }));
    }
    let jws = r.get("jws").and_then(|j| j.as_str()).unwrap_or("").to_string();
    let tx = r.get("transactionId").and_then(|t| t.as_str()).unwrap_or("").to_string();
    match iap_verify_on_server(&app, &transport, &jws).await {
        Ok((toku, entitlement)) => {
            if let Err(e) = iap_ios::finish(&tx).await {
                log::warn!("[iap] credited, but the transaction could not be finished yet: {e}");
            }
            Ok(json!({ "status": "credited", "toku": toku, "entitlement": entitlement }))
        }
        // Paid, not yet credited: the transaction stays unfinished and iap_restore retries.
        Err(e) => Ok(json!({ "status": "unclaimed", "error": e })),
    }
}

/// Buy one product through Apple's sheet and have the server credit it.
/// `status`: credited (toku, entitlement) | cancelled | pending | unclaimed (error).
#[cfg(target_os = "ios")]
#[tauri::command]
async fn iap_purchase(app: AppHandle, transport: State<'_, Arc<Transport>>, product_id: String) -> Result<Value, String> {
    Box::pin(iap_purchase_impl(app, transport.inner().clone(), product_id)).await
}

#[cfg(target_os = "ios")]
async fn iap_restore_impl(app: AppHandle, transport: Arc<Transport>) -> Result<Value, String> {
    let list = iap_ios::unfinished().await?;
    let (mut claimed, mut toku_sum, mut errors) = (0u32, 0u64, Vec::<String>::new());
    for t in list.iter().take(20) {
        let jws = t.get("jws").and_then(|j| j.as_str()).unwrap_or("");
        let tx = t.get("transactionId").and_then(|x| x.as_str()).unwrap_or("");
        if jws.is_empty() || tx.is_empty() {
            continue;
        }
        match iap_verify_on_server(&app, &transport, jws).await {
            Ok((toku, _)) => {
                let _ = iap_ios::finish(tx).await;
                claimed += 1;
                toku_sum += toku;
            }
            Err(e) => errors.push(e),
        }
    }
    Ok(json!({ "found": list.len(), "claimed": claimed, "toku": toku_sum,
               "pending": errors.len(), "error": errors.first() }))
}

/// Re-send every unfinished transaction — on launch, and behind "Restore purchases".
#[cfg(target_os = "ios")]
#[tauri::command]
async fn iap_restore(app: AppHandle, transport: State<'_, Arc<Transport>>) -> Result<Value, String> {
    Box::pin(iap_restore_impl(app, transport.inner().clone())).await
}

#[cfg(not(target_os = "ios"))]
#[tauri::command]
async fn iap_products() -> Result<Value, String> {
    Err("App Store purchases exist only in the iPhone app".into())
}
#[cfg(not(target_os = "ios"))]
#[tauri::command]
async fn iap_purchase(_product_id: String) -> Result<Value, String> {
    Err("App Store purchases exist only in the iPhone app".into())
}
#[cfg(not(target_os = "ios"))]
#[tauri::command]
async fn iap_restore() -> Result<Value, String> {
    Err("App Store purchases exist only in the iPhone app".into())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // iOS gives worker/main threads far smaller stacks than macOS (main ≈1 MB vs 8 MB;
    // secondary threads small too). Nym's mixnet Sphinx crypto is stack-heavy, so the
    // gateway-connect path overflows on iOS (but not macOS, same code). Give every thread
    // a large stack: RUST_MIN_STACK covers std::thread defaults; a custom multi-thread
    // tokio runtime (set as Tauri's async_runtime) covers all async tasks, incl. the Nym
    // client tasks spawned via tokio::spawn from inside our async commands.
    std::env::set_var("RUST_MIN_STACK", "16777216");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(16 * 1024 * 1024)
        .build()
        .expect("build tokio runtime");
    tauri::async_runtime::set(rt.handle().clone());
    std::mem::forget(rt); // keep the runtime alive for the whole app lifetime

    tauri::Builder::default()
        .manage(Arc::new(Transport::new()))
        .manage(BuyLink::default())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // FIRST: nothing may touch the data directory before this — the first
            // create_dir_all would make the pre-rebrand data unreachable for good.
            migrate_pre_rebrand_data_dir(&app.handle().clone());
            #[cfg(target_os = "windows")]
            migrate_roaming_to_local(&app.handle().clone());
            #[cfg(target_os = "ios")]
            if let Ok(dir) = data_dir(&app.handle().clone()) {
                exclude_from_backup(&dir);
                restore_from_synced_phrase(&dir);
            }
            let _ = APP_VER.set(app.package_info().version.to_string());
            // Android: TLS trust store for the Nym client's directory fetches (needs the Activity,
            // which exists by now — the mobile entry point runs from onCreate).
            #[cfg(target_os = "android")]
            init_android_tls_verifier();
            diag(&app.handle().clone(), "==== launch ====");
            // Release builds log too (warn+ → the OS log dir, e.g. ~/Library/Logs/<bundle id>/):
            // a wallet/keychain problem must leave evidence, not just a blank UI.
            app.handle().plugin(
                tauri_plugin_log::Builder::default()
                    .level(if cfg!(debug_assertions) { log::LevelFilter::Info } else { log::LevelFilter::Warn })
                    .build(),
            )?;
            // Connect progress → UI (same five steps the boot animation types).
            {
                let h = app.handle().clone();
                app.state::<Arc<Transport>>().set_progress_sink(Box::new(move |step, detail| {
                    let _ = h.emit("mixnet-phase", json!({ "step": step, "detail": detail }));
                }));
            }
            // Apply a previously-chosen entry gateway before the first request.
            let handle = app.handle().clone();
            let transport = app.state::<Arc<Transport>>().inner().clone();
            tauri::async_runtime::spawn(async move {
                if let Ok(dir) = data_dir(&handle) {
                    let mut w = wallet::load(&dir);
                    // Fresh install (nothing chosen, random mode off): start on one of the
                    // operator's own gateways and remember it — a random directory node was
                    // slow or dead too often on first contact (2026-09-11).
                    if w.entry_gateway.is_none() && !w.entry_random {
                        w.entry_gateway = Some(nym::random_hermes_gateway());
                        if let Err(e) = wallet::save(&dir, &w) {
                            log::warn!("[nym] could not persist the default entry gateway: {e}");
                        }
                        log::info!("[nym] default entry gateway picked from the operator's pool");
                    }
                    if w.entry_gateway.is_some() {
                        transport.set_entry_gateway(w.entry_gateway).await;
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            state, local_state, set_server, account_new, account_reveal, account_restore, account_delete, account_migrate_qr,
            invoice, invoice_status, invoice_cancel, invite_check, ocr_scan, pdf_text, pdf_ocr, pdf_pages, collect, chat,
            smart_available, smart_detect,
            mixnet_route, mixnet_ping, cancel_chat, app_resumed, app_hidden, resume_stats, list_entry_gateways, server_identities, set_entry_gateway, set_entry_random, set_mixnet_perf, buy_close, set_coin_chat, coins_return, collect_later, open_external, save_image, save_file, voucher_redeem,
            phrase_backup_get, iap_products, iap_purchase, iap_restore,
            phrase_check_start,
            phrase_check_verify,
            phrase_backup_set,
            share_text, upload_begin, upload_chunk, upload_pipeline, pick_image, open_account_security,
            vault_list, vault_load, vault_save, vault_remove, vault_purge_webdata, pending_load, pending_save
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

// ---------------------------------------------------------------------------
// Regression tests for C3 (docs/security/audit-2026-08-20.md): the client-side
// overcharge guard — independent fair-price recompute from the bundled table.
#[cfg(test)]
mod migration_tests {
    use super::pick_migration_source;

    /// Windows moves Roaming → Local once. The rule is small enough to state exactly: never
    /// when Local already exists (a second run must not clobber it), otherwise the first
    /// candidate that is a real directory, in the order given (new name before legacy).
    #[test]
    fn roaming_to_local_picks_the_first_existing_source_and_never_clobbers() {
        let base = std::env::temp_dir().join(format!("tk-migr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let newer = base.join("com.tokumai.app");
        let legacy = base.join("com.scrambleai.app");
        std::fs::create_dir_all(&legacy).unwrap();

        // Only the legacy dir exists → it is the source.
        assert_eq!(pick_migration_source(false, &[Some(newer.clone()), Some(legacy.clone())]), Some(legacy.clone()));
        // Both exist → the newer name wins.
        std::fs::create_dir_all(&newer).unwrap();
        assert_eq!(pick_migration_source(false, &[Some(newer.clone()), Some(legacy.clone())]), Some(newer.clone()));
        // Target already there → nothing, whatever else exists.
        assert_eq!(pick_migration_source(true, &[Some(newer.clone()), Some(legacy.clone())]), None);
        // Nothing exists → nothing.
        assert_eq!(pick_migration_source(false, &[Some(base.join("nope")), None]), None);
        assert_eq!(pick_migration_source(false, &[None, None]), None);
        let _ = std::fs::remove_dir_all(&base);
    }
}

// Gated like every other test module: without this the module compiled into the shipped
// app (and its imports showed up as "unused" in every release build).
#[cfg(test)]
mod c3_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plaintext_extracts_text_and_rejects_multimodal() {
        let text = json!([{"role":"user","content":"hello"},{"role":"assistant","content":"hi"}]);
        assert_eq!(messages_plaintext(&text).as_deref(), Some("hello\nhi\n"));
        // multimodal content (image parts) can't be char-estimated → guard must skip
        let mm = json!([{"role":"user","content":[{"type":"image_url","image_url":{"url":"..."}}]}]);
        assert!(messages_plaintext(&mm).is_none());
    }

    #[test]
    fn fair_estimate_skips_unlisted_and_prices_listed() {
        let msgs = json!([{"role":"user","content":"x".repeat(4000)}]);
        // unlisted model → no trusted reference → None (never a false flag)
        assert!(fair_price_estimate("totally-made-up-model-xyz", &msgs, "").is_none());
        // a listed model yields a positive fair price
        let fair = fair_price_estimate("gemini-3.5-flash-lite", &msgs, &"y".repeat(4000));
        assert!(fair.map(|f| f > 0).unwrap_or(false), "listed model must price > 0");
    }

    #[test]
    fn gross_overcharge_trips_but_fair_charge_does_not() {
        let msgs = json!([{"role":"user","content":"x".repeat(4000)}]);
        let fair = fair_price_estimate("gemini-3.5-flash-lite", &msgs, &"y".repeat(4000)).unwrap();
        let ceiling = (fair as f64 * OVERCHARGE_FACTOR).ceil() as u64;
        // the audit's attack (≥50×, up to ~1000×) is far above the ceiling → flagged
        let inflated = fair.saturating_mul(1000).max(MIN_FLAG_SCRAI + 1);
        assert!(inflated > MIN_FLAG_SCRAI && inflated > ceiling, "gross overcharge must be flaggable");
        // an honest charge at the client's own estimate stays under the ceiling → not flagged
        assert!(!(fair > MIN_FLAG_SCRAI && fair > ceiling), "a fair charge must never be flagged");
    }

    // H1 (2026-09-04): a link in a MODEL ANSWER reaches open_external, so its URL is
    // hostile text. The command injection is closed by not shelling out at all; this
    // pins the input filter that sits in front of it.
    #[test]
    fn only_plain_http_urls_and_a_narrow_support_mailto_may_be_opened() {
        assert!(is_openable_url("https://example.com/a?x=1"));
        assert!(is_openable_url("http://example.com"));
        // legitimate URL punctuation must keep working — no metacharacter blocklist
        assert!(is_openable_url("https://maps.google.com/?q=a!b(c)&d=e'f"));
        assert!(is_openable_url("https://mollie.com/checkout/select-method/abc#top"));
        // the shell/launcher escape hatches
        assert!(!is_openable_url("https://example.com/a b"), "a space splits arguments");
        assert!(!is_openable_url("https://example.com/a\nb"), "a newline splits lines");
        assert!(!is_openable_url("https://example.com/a\tb"));
        assert!(!is_openable_url("https://example.com/a\u{0}b"));

        // Support mail: the shape the app writes, and nothing else.
        assert!(is_openable_url("mailto:hermes-stakepool@proton.me"));
        assert!(is_openable_url("mailto:a@b.com?subject=hi&body=there%0A%0A--%0Aapp%200.5.9"));
        // A mailto query is a HEADER list. Everything that could redirect the mail is out.
        assert!(!is_openable_url("mailto:a@b.com?bcc=evil@x.com"), "bcc would send it elsewhere too");
        assert!(!is_openable_url("mailto:a@b.com?cc=evil@x.com"));
        assert!(!is_openable_url("mailto:a@b.com?to=evil@x.com"));
        assert!(!is_openable_url("mailto:a@b.com,evil@x.com"), "one recipient, not a list");
        assert!(!is_openable_url("mailto:a@b.com?subject=x&bcc=evil@x.com"), "one bad param spoils it");
        assert!(!is_openable_url("mailto:notanaddress"));
        assert!(!is_openable_url("mailto:a@b.com?subject=two words"), "whitespace still splits arguments");
        assert!(!is_openable_url("mailto:"));
        // And the schemes that were never allowed still are not.
        assert!(!is_openable_url("file:///etc/passwd"));
        assert!(!is_openable_url("javascript:alert(1)"));
        // wrong scheme / no host
        assert!(!is_openable_url("javascript:alert(1)"));
        assert!(!is_openable_url("file:///etc/passwd"));
        assert!(!is_openable_url("data:text/html,<script>"));
        assert!(!is_openable_url("https:///nohost"));
        assert!(!is_openable_url("https://?q=1"));
        assert!(!is_openable_url(""));
    }
}

#[cfg(test)]
mod tender_tests {
    use super::*;
    use scrai_core::coconut::testkit;

    /// A wallet with one fine book, and the keys for both denominations.
    fn wallet_with_a_book() -> (wallet::Wallet, u64, Vec<scrai_core::purse::EpochKeys>) {
        let fk = testkit::funded();
        let purse = fk.new_purse();
        let coins = purse.remaining_coins();
        let mut w = wallet::Wallet::default();
        w.coconut_purses.push(purse.persist().unwrap());
        (w, coins, both_keys(&fk))
    }

    fn both_keys(fk: &testkit::Funded) -> Vec<scrai_core::purse::EpochKeys> {
        use scrai_core::coconut::{COARSE_TOKU, COIN_TOKU};
        vec![fk.keys_of(COARSE_TOKU), fk.keys_of(COIN_TOKU)]
    }

    #[test]
    fn a_tender_is_minted_once_and_its_leftovers_pay_for_the_next_request() {
        use scrai_core::coconut::COIN_TOKU;
        let (mut w, coins, keys) = wallet_with_a_book();
        assert!(coins >= 20, "the testkit book has {coins} coins");

        // First request: nothing spare yet, so the notes come out of the book. No coarse
        // book here, so the whole tender is the fine plan.
        let t1 = build_tender(&mut w, &keys, 7 * COIN_TOKU).unwrap();
        assert!(t1.total_toku() >= 7 * COIN_TOKU);
        let left_in_book = scrai_core::purse::Purse::restore(&w.coconut_purses[0]).unwrap().remaining_coins();
        assert!(left_in_book < coins, "the purse advanced");
        assert!(w.spare_notes.is_empty(), "nothing is spare while the tender is out");

        // The server burned one note; the rest come home.
        let burned = vec![0usize];
        let burned_toku = keep_unburned(&mut w, &t1.notes, &burned);
        assert_eq!(burned_toku, t1.notes[0].value_toku(), "what was burned is what it was worth");
        assert_eq!(w.spare_notes.len(), t1.notes.len() - 1, "every other note came home");

        // Second request, small enough for the spares: the book is not touched again.
        let spare_toku: u64 = w
            .spare_notes
            .iter()
            .filter_map(|v| serde_json::from_value::<scrai_core::tender::Note>(v.clone()).ok())
            .map(|n| n.value_toku())
            .sum();
        let t2 = build_tender(&mut w, &keys, spare_toku).unwrap();
        assert!(t2.total_toku() >= spare_toku);
        assert_eq!(
            scrai_core::purse::Purse::restore(&w.coconut_purses[0]).unwrap().remaining_coins(),
            left_in_book,
            "spares are spent before a fresh coin is taken out of a book"
        );
    }

    /// The point of the second denomination: a ceiling that used to need ninety fine coins
    /// (44 KB, more notes than a tender may carry) travels as a handful of coarse ones plus
    /// a fine plan — and stays exact to a tenth of a cent.
    #[test]
    fn coarse_books_carry_the_bulk_and_fine_ones_keep_it_exact() {
        use scrai_core::coconut::{COARSE_TOKU, COIN_TOKU};
        use scrai_core::tender::MAX_NOTES;
        let fk = testkit::funded();
        let keys = both_keys(&fk);
        let mut w = wallet::Wallet::default();
        w.coconut_purses.push(fk.new_purse_of(COARSE_TOKU).persist().unwrap());
        w.coconut_purses.push(fk.new_purse_of(COIN_TOKU).persist().unwrap());

        let ceiling = 9_000; // 9 ¢, an image request
        let t = build_tender(&mut w, &keys, ceiling).expect("a funded wallet can tender");
        assert!(t.notes.len() <= MAX_NOTES, "{} notes", t.notes.len());
        assert!(t.total_toku() >= ceiling, "tendered {} TOKU for {ceiling}", t.total_toku());
        t.well_formed().expect("a tender the server would accept");
        // A tenth of the coins a single denomination needed.
        assert!(t.total_coins() < 30, "{} coins on the table", t.total_coins());
        // And every tenth of a cent up to the ceiling is still an exact subset sum.
        for cost in (0..=ceiling).step_by(COIN_TOKU as usize) {
            let picked = t.select(cost).unwrap_or_else(|| panic!("no subset for {cost}"));
            let paid: u64 = picked.iter().map(|i| t.notes[*i].value_toku()).sum();
            assert_eq!(paid, cost, "paying {cost} TOKU burned {paid}");
        }
    }

    /// Books are small, so a bigger request has to take coins out of several of them.
    #[test]
    fn a_tender_spans_several_books_when_one_is_not_enough() {
        use scrai_core::coconut::COIN_TOKU;
        let fk = testkit::funded();
        let keys = both_keys(&fk);
        let mut w = wallet::Wallet::default();
        let per_book = {
            let p = fk.new_purse();
            let c = p.remaining_coins() * COIN_TOKU;
            w.coconut_purses.push(p.persist().unwrap());
            c
        };
        w.coconut_purses.push(fk.new_purse().persist().unwrap());
        w.coconut_purses.push(fk.new_purse().persist().unwrap());
        let want = per_book * 2 + COIN_TOKU; // more than two whole books
        let t = build_tender(&mut w, &keys, want).unwrap();
        assert!(t.total_toku() >= want, "the tender is complete across books");
    }

    #[test]
    fn a_tender_tops_the_spares_up_out_of_the_book_when_they_fall_short() {
        use scrai_core::coconut::COIN_TOKU;
        let (mut w, _, keys) = wallet_with_a_book();
        let t1 = build_tender(&mut w, &keys, 3 * COIN_TOKU).unwrap();
        keep_unburned(&mut w, &t1.notes, &[]); // nothing burned: all of it is spare
        let spares = w.spare_notes.len();
        assert!(spares > 0);

        // Ask for more than the spares hold, so the shortfall has to come out of the book.
        let spare_toku: u64 = w
            .spare_notes
            .iter()
            .filter_map(|v| serde_json::from_value::<scrai_core::tender::Note>(v.clone()).ok())
            .map(|n| n.value_toku())
            .sum();
        let book_before = scrai_core::purse::Purse::restore(&w.coconut_purses[0]).unwrap().remaining_coins();
        let ceiling = spare_toku + 5 * COIN_TOKU;
        let t2 = build_tender(&mut w, &keys, ceiling).unwrap();
        assert!(t2.total_toku() >= ceiling, "at least the ceiling: {}", t2.total_toku());
        assert!(t2.notes.len() > spares, "the spares plus a fresh plan for the shortfall");
        let book_after = scrai_core::purse::Purse::restore(&w.coconut_purses[0]).unwrap().remaining_coins();
        assert!(book_after < book_before, "the shortfall came out of the book");
    }

    /// The bug of 2026-09-14: a ninety-coin ceiling out of ten-coin books produced a fine
    /// 1,2,4,… plan PER BOOK — thirty-six notes — and the server refused the tender for
    /// carrying more than sixteen.
    #[test]
    fn a_tender_spanning_many_books_stays_inside_the_note_limit() {
        use scrai_core::coconut::COIN_TOKU;
        use scrai_core::tender::MAX_NOTES;
        let fk = testkit::funded();
        let keys = both_keys(&fk);
        let mut w = wallet::Wallet::default();
        for _ in 0..8 {
            w.coconut_purses.push(fk.new_purse().persist().unwrap());
        }
        let ceiling = 90 * COIN_TOKU;
        let t = build_tender(&mut w, &keys, ceiling).expect("eight books can pay for it");
        assert!(t.notes.len() <= MAX_NOTES, "{} notes in one tender", t.notes.len());
        assert!(t.total_toku() >= ceiling, "tendered {} TOKU for {ceiling}", t.total_toku());
        t.well_formed().expect("a tender the server would accept");
    }

    /// The wedge of 2026-09-14: a refused tender comes home whole, so the wallet held
    /// thirty-six spare notes — and the next tender put every one of them back on the
    /// table and was refused for the same reason. For ever.
    #[test]
    fn a_pile_of_spare_notes_cannot_wedge_the_next_tender() {
        use scrai_core::coconut::COIN_TOKU;
        use scrai_core::tender::MAX_NOTES;
        let fk = testkit::funded();
        let keys = both_keys(&fk);
        let mut w = wallet::Wallet::default();

        for _ in 0..9 {
            let mut p = fk.new_purse();
            let notes = p.spend_tender(&fk.keys(), &scrai_core::tender::plan_coins(4), fk.spend_date()).unwrap();
            for n in notes {
                w.spare_notes.push(serde_json::to_value(&n).unwrap());
            }
            w.coconut_purses.push(p.persist().unwrap());
        }
        assert!(w.spare_notes.len() > MAX_NOTES, "the fixture must reproduce the pile");

        let t = build_tender(&mut w, &keys, 90 * COIN_TOKU).expect("a wallet with credit can tender");
        assert!(t.notes.len() <= MAX_NOTES, "{} notes in one tender", t.notes.len());
        t.well_formed().expect("a tender the server would accept");
        assert!(!w.spare_notes.is_empty(), "unused spares must be kept, not dropped");
    }

    /// 2026-09-14: the device held 55 coins and could only tender 34, because ten small
    /// spare notes filled the table and left no room for the books that carry the weight.
    #[test]
    fn small_spares_do_not_crowd_out_whole_books() {
        use scrai_core::coconut::{COARSE_TOKU, COIN_TOKU};
        use scrai_core::tender::MAX_NOTES;
        let fk = testkit::funded();
        let keys = both_keys(&fk);
        let mut w = wallet::Wallet::default();
        for _ in 0..6 {
            let mut p = fk.new_purse();
            let notes = p.spend_tender(&fk.keys(), &[1, 1, 1, 1], fk.spend_date()).unwrap();
            for n in notes {
                w.spare_notes.push(serde_json::to_value(&n).unwrap());
            }
            w.coconut_purses.push(p.persist().unwrap());
        }
        w.coconut_purses.push(fk.new_purse_of(COARSE_TOKU).persist().unwrap());
        let on_device = coin_value_toku(&w);
        let ceiling = 30 * COIN_TOKU;
        assert!(on_device >= ceiling, "the fixture must hold more than the ceiling");

        let t = build_tender(&mut w, &keys, ceiling).expect("a wallet with credit can tender");
        assert!(t.notes.len() <= MAX_NOTES, "{} notes", t.notes.len());
        assert!(
            t.total_toku() >= ceiling,
            "tendered {} TOKU for a {ceiling} ceiling with {on_device} on the device",
            t.total_toku()
        );
    }

    /// The swap only runs while the app is open, so what it picks up decides whether a
    /// user loses money. It must take a book inside the window, leave one outside it, and
    /// hand back WHOLE books rather than planning a tender out of them.
    #[test]
    fn the_swap_takes_the_books_that_are_about_to_expire_and_leaves_the_rest() {
        let fk = testkit::funded();
        let keys = both_keys(&fk);
        let now = fk.expiration_date().saturating_sub((SWAP_WINDOW_DAYS * 86_400) as u32) + 86_400;
        let mut w = wallet::Wallet::default();
        w.coconut_purses.push(fk.new_purse().persist().unwrap());
        let value_before = coin_value_toku(&w);

        assert!(has_expiring_books(&w, now), "a book one day inside the window counts");
        let t = drain_expiring(&mut w, &keys, now).unwrap().expect("something to hand back");
        t.well_formed().expect("a tender the server would accept");
        // A whole book per note is the cheapest shape: one serial's worth of proof per coin,
        // but only one note to carry.
        assert_eq!(t.notes.len(), 1, "one note for one book: {:?}", t.notes.len());
        assert_eq!(t.total_toku(), value_before, "the whole book went back");
        assert!(w.coconut_purses.is_empty(), "an emptied book is dropped");
        assert!(drain_expiring(&mut w, &keys, now).unwrap().is_none(), "nothing left to hand back");

        // The same book, a day BEFORE the window opens: left alone.
        let mut w2 = wallet::Wallet::default();
        w2.coconut_purses.push(fk.new_purse().persist().unwrap());
        let early = now.saturating_sub(2 * 86_400);
        assert!(!has_expiring_books(&w2, early), "a book outside the window is not due");
        assert!(drain_expiring(&mut w2, &keys, early).unwrap().is_none(), "and is not taken");
    }

    /// Coins on the table for an unanswered question are still the user's money: the
    /// balance must not drop by the whole tender and jump back when the reply lands.
    #[test]
    fn coins_in_flight_still_count_as_held() {
        let fk = testkit::funded();
        let keys = both_keys(&fk);
        let mut w = wallet::Wallet::default();
        w.coconut_purses.push(fk.new_purse().persist().unwrap());
        let before = coin_value_toku(&w);
        let t = build_tender(&mut w, &keys, 5 * scrai_core::coconut::COIN_TOKU).expect("a funded wallet can tender");
        w.pending_tenders.push(wallet::PendingTender {
            request: json!({ "id": "r1" }),
            notes: t.notes.iter().map(|n| serde_json::to_value(n).unwrap()).collect(),
        });
        assert_eq!(coin_value_toku(&w), before, "a tender in flight is not a loss");
    }

    /// The epoch material is a dozen packets and can only come back in the SURBs the
    /// request carried. When the key request grew a second variant — one per denomination
    /// — this choice did not follow, and every key fetch went out with eight SURBs
    /// (2026-09-15: a redeemed code showed its credit but the balance sat still).
    #[test]
    fn every_key_request_carries_a_full_surb_budget() {
        use scrai_core::coconut::{COARSE_TOKU, COIN_TOKU};
        use scrai_core::federation::FedRequest;
        assert_eq!(surbs_for(&FedRequest::Keys), SURBS_KEYS);
        for d in [COIN_TOKU, COARSE_TOKU] {
            assert_eq!(surbs_for(&FedRequest::KeysFor { denom_toku: d, expiration_date: 0 }), SURBS_KEYS, "denomination {d}");
            // …and asking for an OLDER epoch's material is the same size of answer.
            assert_eq!(surbs_for(&FedRequest::KeysFor { denom_toku: d, expiration_date: 1_760_400_000 }), SURBS_KEYS);
        }
    }

    #[test]
    fn a_wallet_without_credit_cannot_tender() {
        let fk = testkit::funded();
        let keys = both_keys(&fk);
        let mut w = wallet::Wallet::default();
        assert!(build_tender(&mut w, &keys, 7 * scrai_core::coconut::COIN_TOKU).is_err());
    }

    #[test]
    fn the_ceiling_stays_inside_its_bounds() {
        let msgs = json!([{ "role": "user", "content": "hi" }]);
        use scrai_core::coconut::{COARSE_TOKU, COIN_TOKU};
        let c = tender_ceiling_toku("gemini-3.5-flash", &msgs, Some(1000), None, None);
        let (lo, hi) = (TENDER_MIN_TOKU, TENDER_MAX_TOKU);
        assert!((lo..=hi).contains(&c), "{c} TOKU outside {lo}..={hi}");
        assert_eq!(c % COIN_TOKU, 0, "a ceiling is a whole number of fine coins");
        // A tender costs ~490 bytes per coin over the mixnet, and the ceiling is the WORST
        // case: on an expensive model a full answer budget plus the server's thinking
        // budget really is a few cents, so this is 58 coins ≈ 28 KB for a 1000-token
        // answer on gemini-3.5-flash (it was 19 coins while the client under-tendered and
        // the server silently capped the answer to fit). The guard catches growth beyond
        // that, not the size itself — the levers for the size are a tender priced off the
        // catalog's own rates (no margin guess, so less headroom) and a second, coarser
        // coin denomination (fewer coins for the same TOKU).
        // With coarse coins carrying the bulk, the same ceiling is a handful of coins.
        let coins = c / COARSE_TOKU + (COARSE_TOKU / COIN_TOKU);
        assert!(coins * 490 < 15_000, "an ordinary request tenders ~{coins} coins ≈ {} bytes", coins * 490);
        let long = json!([{ "role": "user", "content": "x".repeat(400_000) }]);
        let c = tender_ceiling_toku("gemini-3.5-flash", &long, Some(100_000), None, None);
        assert_eq!(c, hi, "a huge request is capped, not unbounded");
    }

    /// The bug of 2026-09-14: an image model's ceiling was computed as if the answer were
    /// text, so the tender covered a fraction of the picture and the server refused it
    /// with coins sitting on the device. The tender must cover what the SERVER reserves.
    #[test]
    fn an_image_request_tenders_enough_for_the_picture() {
        use scrai_core::billing::{ceiling_input_tokens, ceiling_toku, image_tokens_for, Ceiling};
        use scrai_core::coconut::COIN_TOKU;
        use scrai_core::pricing::PricingTable;
        let model = "gemini-3.1-flash-lite-image";
        let msgs = json!([{ "role": "user", "content": "render an image of a beach" }]);
        let table = PricingTable::parse(BUNDLED_PRICING).expect("bundled pricing parses");
        let price = table.price(model);
        assert!(!price.fallback, "{model} must be priced for this test to mean anything");

        for size in ["1K", "2K", "4K"] {
            // What the server will reserve, at the same margin the client assumes.
            let server = ceiling_toku(
                &price,
                CLIENT_RETAIL_MARGIN,
                &Ceiling {
                    in_tokens: ceiling_input_tokens(&msgs),
                    out_tokens: 4096 + 2048,
                    image_tokens: image_tokens_for(size),
                },
            );
            let tendered = tender_ceiling_toku(model, &msgs, None, None, Some(size)) * COIN_TOKU;
            assert!(tendered >= server, "{size}: tendered {tendered} TOKU against a {server} TOKU ceiling");
        }
        // And the picture is really in there: a bigger one costs more to tender. (Comparing
        // against a TEXT model proves nothing — gemini-3.5-flash bills output at $9/1M
        // against this model's $1.50 text rate, so its ceiling is the higher of the two.)
        let one_k = tender_ceiling_toku(model, &msgs, None, None, Some("1K"));
        let four_k = tender_ceiling_toku(model, &msgs, None, None, Some("4K"));
        assert!(four_k > one_k, "a 4K picture must tender more than a 1K one ({four_k} vs {one_k})");
    }
}
