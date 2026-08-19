// ---------------------------------------------------------------------------
// ScrambleAI desktop — the Rust core.
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
mod wallet;

use nym::Transport;
use rand::RngCore;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};

const TIERS: [u32; 4] = [5, 10, 20, 50];
const PROTO: u64 = 1;

// Reply-SURB budgets: a small answer needs few, a chat answer more.
const SURBS_SMALL: u32 = 80;
/// Reply budget for text chats: ~150 Sphinx packets ≈ 300 KB — ample for any
/// text answer, and far fewer request fragments than the old flat 500 (each
/// SURB rides IN the request, so oversizing bloats every send and triggers
/// retransmission storms on the server's inbound reassembly).
const SURBS_TEXT: u32 = 150;
/// Reply budget for image models: a generated image is a single ~MB reply.
const SURBS_CHAT: u32 = 500;
const TIMEOUT_MS: u64 = 120_000;

/// Coins redeemed per auto-fund when a session runs dry (1 coin = 1000 SCRAI →
/// 100 coins ≈ $1, per docs/federation-params.md). Uniform across users on
/// purpose: not everything at once (leaks the balance and builds one big
/// pseudonym), not tiny bits (many shows + mixnet round-trips).
const REDEEM_CHUNK_COINS: u64 = 100;

fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path().app_data_dir().map_err(|e| e.to_string())
}

fn rand_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

/// The CLI stores its server address in ~/.scrai/cli.json; read it as a fallback
/// so `npm run client -- server <addr>` also configures the app.
fn cli_config_server() -> Option<String> {
    let path = std::env::var("SCRAI_CONFIG").map(PathBuf::from).ok().or_else(|| {
        std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".scrai").join("cli.json"))
    })?;
    let s = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&s).ok()?;
    v.get("serverAddress").and_then(|a| a.as_str()).map(str::to_string)
}

fn server_addr(w: &wallet::Wallet) -> Result<String, String> {
    w.server
        .clone()
        .or_else(|| std::env::var("SCRAI_SERVER_ADDRESS").ok())
        .or_else(cli_config_server)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "no scrai-server address configured".to_string())
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

async fn session_status(t: &Transport, srv: &str, sk: &account::SessionKeys) -> Result<(u64, u64), String> {
    let sig = sk.sign(0, "status");
    let resp = t
        .round_trip(
            srv,
            &json!({"v":PROTO,"kind":"session.status","id":rand_hex(16),"sessionId":sk.session_id,"sig":sig}),
            SURBS_SMALL,
            TIMEOUT_MS,
        )
        .await?;
    let balance = resp.get("balance").and_then(|b| b.as_u64()).unwrap_or(0);
    let counter = resp.get("counter").and_then(|b| b.as_u64()).unwrap_or(0);
    Ok((balance, counter))
}

// ---- coconut federation (talks to the Rust server / issuing authority) -----

/// One federation round-trip over the mixnet: wrap a `FedRequest` in the app's
/// id-envelope, send it, and unwrap the `FedResponse` from the reply.
async fn fed_call(
    t: &Transport,
    srv: &str,
    req: scrai_core::federation::FedRequest,
) -> Result<scrai_core::federation::FedResponse, String> {
    let env = json!({
        "v": PROTO, "kind": "coconut", "id": rand_hex(16),
        "fed": serde_json::to_value(&req).map_err(|e| e.to_string())?,
    });
    let reply = t.round_trip(srv, &env, SURBS_SMALL, TIMEOUT_MS).await?;
    serde_json::from_value(reply.get("fed").cloned().ok_or("no fed in reply")?)
        .map_err(|e| format!("bad fed response: {e}"))
}

/// Full credential withdrawal: fetch keys → blind-withdraw at each authority →
/// aggregate into a `Purse`. The Withdraw itself is ACCOUNT-SIGNED: the server
/// only issues a ticketbook against paid entitlement, and the account signature
/// is what ties the request to the buyer (the coins that come out stay blind).
async fn withdraw_purse(
    t: &Transport,
    srv: &str,
    auth: &account::Account,
) -> Result<scrai_core::purse::Purse, String> {
    use scrai_core::coconut;
    use scrai_core::federation::{FedRequest, FedResponse};

    let (vk, auth_vks, coin_sigs, date_sigs, expiration_date, total_coins) =
        match fed_call(t, srv, FedRequest::Keys).await? {
            FedResponse::Keys {
                vk, auth_vks, coin_sigs, date_sigs, expiration_date, total_coins, ..
            } => (vk, auth_vks, coin_sigs, date_sigs, expiration_date, total_coins),
            FedResponse::Error { message } => return Err(format!("server: {message}")),
            _ => return Err("unexpected response to Keys".into()),
        };

    let user = coconut::new_user();
    let (req, req_info) =
        coconut::make_withdrawal_request(user.secret_key(), expiration_date, coconut::DEFAULT_T_TYPE)?;
    let mut shares = Vec::new();
    for (i, vk_auth) in auth_vks.iter().enumerate() {
        // 1-of-1 test server = one address; multi-server sends to each authority's.
        let fed = FedRequest::Withdraw { user_pk: user.public_key(), req: req.clone() };
        let nonce = rand_hex(16);
        let env = json!({
            "v": PROTO, "kind": "coconut", "id": rand_hex(16),
            "fed": serde_json::to_value(&fed).map_err(|e| e.to_string())?,
            "publicKey": auth.public_key_pem,
            "nonce": nonce,
            "sig": auth.sign("withdraw:coconut", &nonce),
        });
        let reply = t.round_trip(srv, &env, SURBS_SMALL, TIMEOUT_MS).await?;
        let resp: FedResponse = serde_json::from_value(reply.get("fed").cloned().ok_or("no fed in reply")?)
            .map_err(|e| format!("bad fed response: {e}"))?;
        let blinded = match resp {
            FedResponse::Withdraw { blinded } => blinded,
            FedResponse::Error { message } => return Err(format!("server: {message}")),
            _ => return Err("unexpected response to Withdraw".into()),
        };
        shares.push(coconut::verify_share(vk_auth, user.secret_key(), &blinded, &req_info, i as u64 + 1)?);
    }
    // aggregate — succeeds ONLY if the server issued valid shares
    let wallet = coconut::aggregate(&vk, user.secret_key(), &shares, &req_info)?;
    Ok(scrai_core::purse::Purse::new(
        wallet, user, vk, coin_sigs, date_sigs, total_coins, expiration_date,
    ))
}

fn resolve_server(app: &AppHandle, server: Option<String>) -> Result<String, String> {
    match server {
        Some(s) if !s.trim().is_empty() => Ok(s),
        _ => server_addr(&wallet::load(&data_dir(app)?)),
    }
}

/// The wallet's account, needed wherever a request must be account-signed.
fn wallet_account(app: &AppHandle) -> Result<account::Account, String> {
    let w = wallet::load(&data_dir(app)?);
    let m = w.mnemonic.ok_or("no account — create one first")?;
    account::from_mnemonic(&m)
}

/// Dev self-test: perform a full withdrawal but DON'T store it — proves the
/// client↔server ring over the mixnet. Needs paid entitlement like any withdraw.
#[tauri::command]
async fn coconut_withdraw_test(
    app: AppHandle,
    transport: State<'_, Arc<Transport>>,
    server: Option<String>,
) -> Result<Value, String> {
    let srv = resolve_server(&app, server)?;
    let a = wallet_account(&app)?;
    let purse = withdraw_purse(&transport, &srv, &a).await?;
    Ok(json!({ "ok": true, "coins": purse.total_coins() }))
}

/// Withdraw a credential and STORE it in the wallet (bearer money that survives
/// a restart). Appends to the held books — never replaces one that still holds
/// coins.
#[tauri::command]
async fn coconut_withdraw(
    app: AppHandle,
    transport: State<'_, Arc<Transport>>,
    server: Option<String>,
) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let srv = resolve_server(&app, server)?;
    let a = wallet_account(&app)?;
    let purse = withdraw_purse(&transport, &srv, &a).await?;
    let coins = purse.total_coins();

    let mut w = wallet::load(&dir);
    w.coconut_purses.push(purse.persist()?);
    wallet::save(&dir, &w)?;
    log::info!("[coconut] withdrew + stored a {coins}-coin credential");
    Ok(json!({ "ok": true, "coins": coins, "stored": true }))
}

/// Spend `coins` (default 1) from the stored credential against the server, which
/// verifies the payment and records it in the double-spend quorum.
#[tauri::command]
async fn coconut_spend(
    app: AppHandle,
    transport: State<'_, Arc<Transport>>,
    server: Option<String>,
    coins: Option<u64>,
) -> Result<Value, String> {
    use scrai_core::coconut::PayInfo;
    use scrai_core::federation::{FedRequest, FedResponse};

    let dir = data_dir(&app)?;
    let srv = resolve_server(&app, server)?;
    let mut w = wallet::load(&dir);
    let (idx, mut purse) = first_funded_purse(&w.coconut_purses)
        .ok_or("no coconut credential — withdraw first")?;
    let coins = coins.unwrap_or(1).min(purse.remaining_coins());

    // spend context: fresh random pay_info (later this binds to the session).
    let mut pib = [0u8; 72];
    rand::thread_rng().fill_bytes(&mut pib);
    let pi = PayInfo { pay_info_bytes: pib };
    // a date within the credential's validity (one day before expiry)
    let spend_date = purse.expiration_date().saturating_sub(86_400);

    let payment = purse.spend(coins, &pi, spend_date)?;
    // DURABILITY: persist the ADVANCED purse BEFORE the payment leaves the device,
    // so a crash can never roll the counter back and re-spend (see the Purse docs).
    let emptied = purse.remaining_coins() == 0;
    w.coconut_purses[idx] = purse.persist()?;
    if emptied {
        w.coconut_purses.remove(idx);
    }
    wallet::save(&dir, &w)?;

    let resp = fed_call(
        &transport,
        &srv,
        FedRequest::Spend { payment, pay_info: pib.to_vec(), spend_date },
    )
    .await?;
    match resp {
        FedResponse::Spend { accepted, verdict } => {
            log::info!("[coconut] spent {coins} coin(s): {verdict}");
            Ok(json!({ "ok": accepted, "verdict": verdict, "coins": coins }))
        }
        FedResponse::Error { message } => Err(format!("server: {message}")),
        _ => Err("unexpected response to Spend".into()),
    }
}

/// Redeem `coins` from the stored coconut credential into the ACTIVE session's SCRAI
/// balance (the credit `chat` draws down). Durable: the advanced purse is persisted
/// BEFORE the payment leaves the device, so a crash/retry can't roll the counter back
/// and re-spend. Returns the session balance the server reports after crediting.
async fn redeem_coconut(app: &AppHandle, t: &Transport, srv: &str, coins: u64) -> Result<u64, String> {
    use scrai_core::coconut::PayInfo;

    let dir = data_dir(app)?;
    let mut w = wallet::load(&dir);
    let m = w.mnemonic.clone().ok_or("no account")?;
    let sk = account::derive_session_keys(&m, w.session_index)?;
    let (idx, mut purse) = first_funded_purse(&w.coconut_purses)
        .ok_or("no coconut credential — buy credit first")?;
    // Clamp to what this book still holds; the next redeem rolls to the next book.
    let coins = coins.min(purse.remaining_coins());

    let mut pib = [0u8; 72];
    rand::thread_rng().fill_bytes(&mut pib);
    let pi = PayInfo { pay_info_bytes: pib };
    let spend_date = purse.expiration_date().saturating_sub(86_400);

    let payment = purse.spend(coins, &pi, spend_date)?;
    // DURABILITY: persist the ADVANCED purse before the payment leaves the device.
    let emptied = purse.remaining_coins() == 0;
    w.coconut_purses[idx] = purse.persist()?;
    if emptied {
        w.coconut_purses.remove(idx);
    }
    wallet::save(&dir, &w)?;

    let env = json!({
        "v": PROTO, "kind": "redeem", "id": rand_hex(16),
        "sessionId": sk.session_id,
        "payment": serde_json::to_value(&payment).map_err(|e| e.to_string())?,
        "pay_info": pib.to_vec(), "spend_date": spend_date,
    });
    let reply = t.round_trip(srv, &env, SURBS_SMALL, TIMEOUT_MS).await?;
    let balance = reply.get("balance").and_then(|b| b.as_u64()).unwrap_or(0);
    log::info!("[coconut] redeemed {coins} coin(s) → session balance {balance}");
    Ok(balance)
}

/// SCRAI value of coconut coins NOT yet redeemed (0 if no credential). Local-only —
/// added to the funded session balance so the UI shows total spendable credit.
/// The first held book that still has coins, restored — spends drain the books
/// in order, and emptied ones are dropped by the spender.
fn first_funded_purse(purses: &[String]) -> Option<(usize, scrai_core::purse::Purse)> {
    for (i, pj) in purses.iter().enumerate() {
        if let Ok(p) = scrai_core::purse::Purse::restore(pj) {
            if p.remaining_coins() > 0 {
                return Some((i, p));
            }
        }
    }
    None
}

fn coconut_held_scrai(app: &AppHandle) -> u64 {
    let Ok(dir) = data_dir(app) else { return 0 };
    let w = wallet::load(&dir);
    w.coconut_purses
        .iter()
        .filter_map(|pj| scrai_core::purse::Purse::restore(pj).ok())
        .map(|p| p.remaining_coins() * scrai_core::coconut::COIN_SCRAI)
        .sum()
}

/// Manually redeem coconut coins into the session balance (chat also does this
/// automatically when a session runs dry).
#[tauri::command]
async fn coconut_redeem(
    app: AppHandle,
    transport: State<'_, Arc<Transport>>,
    server: Option<String>,
    coins: Option<u64>,
) -> Result<Value, String> {
    let _op = transport.begin_op().await;
    let srv = resolve_server(&app, server)?;
    let coins = coins.unwrap_or(REDEEM_CHUNK_COINS);
    let balance = redeem_coconut(&app, &transport, &srv, coins).await?;
    Ok(json!({ "ok": true, "coins": coins, "balance": balance }))
}

// ---- commands -------------------------------------------------------------

#[tauri::command]
async fn state(app: AppHandle, transport: State<'_, Arc<Transport>>) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let w = wallet::load(&dir);

    let account = match &w.mnemonic {
        Some(m) => {
            let a = account::from_mnemonic(m)?;
            json!({ "fingerprint": account::fingerprint(&a.account_id), "sessionIndex": w.session_index })
        }
        None => Value::Null,
    };
    let server = server_addr(&w).ok();
    let mut models = json!([]);
    let mut balance: u64 = 0;

    if let Some(srv) = &server {
        // Models: cached after first fetch.
        if let Some(cached) = transport.cached_models().await {
            models = cached;
        } else if let Ok(resp) = transport
            .round_trip(srv, &json!({"v":PROTO,"kind":"models","id":rand_hex(16)}), SURBS_SMALL, TIMEOUT_MS)
            .await
        {
            if let Some(m) = resp.get("models") {
                models = m.clone();
                transport.set_cached_models(m.clone()).await;
            }
        }
        // Balance for the active session.
        if let Some(m) = &w.mnemonic {
            if let Ok(sk) = account::derive_session_keys(m, w.session_index) {
                if let Ok((b, _)) = session_status(&transport, srv, &sk).await {
                    balance = b;
                }
            }
        }
    }

    // Show TOTAL spendable credit: the funded session balance PLUS coconut coins not
    // yet redeemed (redeem is lazy — it happens on first chat — but the money is
    // already the user's, so a fresh credential shouldn't read as "0").
    balance = balance.saturating_add(coconut_held_scrai(&app));

    Ok(json!({
        "account": account,
        "balance": balance,
        "held": coconut_held_scrai(&app),
        "tiers": TIERS,
        "fakePayments": false,
        "gateway": "btcpay",
        "models": models,
        "server": server,
    }))
}

#[tauri::command]
fn set_server(app: AppHandle, address: String) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let mut w = wallet::load(&dir);
    let a = address.trim().to_string();
    w.server = if a.is_empty() { None } else { Some(a) };
    wallet::save(&dir, &w)?;
    Ok(json!({ "server": w.server }))
}

#[tauri::command]
fn account_new(app: AppHandle) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let prev = wallet::load(&dir);
    let a = account::create_account();
    let w = wallet::Wallet { mnemonic: Some(a.mnemonic.clone()), server: prev.server, entry_gateway: prev.entry_gateway, ..Default::default() };
    wallet::save(&dir, &w)?;
    Ok(json!({ "mnemonic": a.mnemonic, "fingerprint": account::fingerprint(&a.account_id) }))
}

#[tauri::command]
fn account_reveal(app: AppHandle) -> Result<Value, String> {
    let w = wallet::load(&data_dir(&app)?);
    match w.mnemonic {
        Some(m) => Ok(json!({ "mnemonic": m })),
        None => Err("no account".into()),
    }
}

#[tauri::command]
fn account_restore(app: AppHandle, mnemonic: String) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let prev = wallet::load(&dir);
    let a = account::from_mnemonic(&mnemonic)?;
    let w = wallet::Wallet { mnemonic: Some(a.mnemonic.clone()), server: prev.server, entry_gateway: prev.entry_gateway, ..Default::default() };
    wallet::save(&dir, &w)?;
    Ok(json!({ "fingerprint": account::fingerprint(&a.account_id), "balance": 0 }))
}

#[tauri::command]
async fn invoice(app: AppHandle, transport: State<'_, Arc<Transport>>, usd: u32, method: Option<String>) -> Result<Value, String> {
    let w = wallet::load(&data_dir(&app)?);
    let srv = server_addr(&w)?;
    let m = w.mnemonic.ok_or("no account — create one first")?;
    let a = account::from_mnemonic(&m)?;
    let nonce = rand_hex(16);
    // The method is not part of the signature — it only selects the payment rail,
    // it grants no authority — so the server accepts the same account signature.
    let method = match method.as_deref() {
        Some("nyx") => "nyx",
        _ => "btc",
    };
    let sig = a.sign(&format!("invoice:{}", usd), &nonce);
    let req = json!({"v":PROTO,"kind":"invoice.create","id":rand_hex(16),"publicKey":a.public_key_pem,"usd":usd,"method":method,"nonce":nonce,"sig":sig});
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
                    // NYM gets the branded (purple + Nym mark) QR à la NymQR; the
                    // QR encodes the bare Nyx address, with the memo shown as text.
                    oo["qr"] = json!(if is_nym { qr_svg_nym(uri) } else { qr_svg(uri) });
                    oo
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(json!({
        "invoiceId": resp.get("invoiceId"),
        "amountUsd": resp.get("amountUsd"),
        "amountScrai": resp.get("amountScrai"),
        "expiresAt": resp.get("expiresAt"),
        "instruction": resp.get("instruction"),
        "options": options,
        "checkout": "",
    }))
}

#[tauri::command]
async fn invoice_status(app: AppHandle, transport: State<'_, Arc<Transport>>, id: String) -> Result<Value, String> {
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

#[tauri::command]
async fn invoice_cancel(app: AppHandle, transport: State<'_, Arc<Transport>>, id: String) -> Result<Value, String> {
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
    let _op = transport.begin_op().await;
    let dir = data_dir(&app)?;
    let w0 = wallet::load(&dir);
    let srv = server_addr(&w0)?;
    let a = wallet_account(&app)?;

    // How much is owed?
    let nonce = rand_hex(16);
    let sig = a.sign("entitlement", &nonce);
    let resp = transport
        .round_trip(&srv, &json!({"v":PROTO,"kind":"entitlement","id":rand_hex(16),"publicKey":a.public_key_pem,"nonce":nonce,"sig":sig}), SURBS_SMALL, TIMEOUT_MS)
        .await?;
    let mut owed = resp.get("entitlement").and_then(|e| e.as_u64()).unwrap_or(0);
    let mut collected = 0u64;

    while owed > 0 {
        let purse = match withdraw_purse(&transport, &srv, &a).await {
            Ok(p) => p,
            // The tail below one book (or a race) is not an error — it just
            // stays as entitlement until the next purchase tops it up.
            Err(e) if e.contains("not enough entitlement") => break,
            Err(e) => return Err(e),
        };
        let book_scrai = purse.total_coins() * scrai_core::coconut::COIN_SCRAI;
        let mut w = wallet::load(&dir);
        w.coconut_purses.push(purse.persist()?);
        wallet::save(&dir, &w)?;
        collected += book_scrai;
        owed = owed.saturating_sub(book_scrai);
        log::info!("[coconut] collected a {book_scrai}-SCRAI book ({owed} entitlement left)");
    }
    Ok(json!({ "collected": collected, "held": coconut_held_scrai(&app) }))
}

/// Manually redeem one chunk of held coconut credit into the session balance
/// (chat also does this automatically when the session runs dry).
#[tauri::command]
async fn redeem(app: AppHandle, transport: State<'_, Arc<Transport>>) -> Result<Value, String> {
    let _op = transport.begin_op().await;
    let w = wallet::load(&data_dir(&app)?);
    let srv = server_addr(&w)?;
    let balance = redeem_coconut(&app, &transport, &srv, REDEEM_CHUNK_COINS).await?;
    Ok(json!({ "balance": balance.saturating_add(coconut_held_scrai(&app)) }))
}

#[tauri::command]
async fn chat(
    app: AppHandle,
    transport: State<'_, Arc<Transport>>,
    model: String,
    messages: Value,
    #[allow(non_snake_case)] maxTokens: Option<u64>,
    free: Option<bool>,
    #[allow(non_snake_case)] bigReply: Option<bool>,
) -> Result<Value, String> {
    // Serialise the whole command: session_status + chat must be one atomic unit,
    // or two concurrent chats race the session counter (crossed replies / hangs).
    let _op = transport.begin_op().await;

    // Image models answer with a ~MB reply and need the big SURB budget; text
    // answers fit comfortably in the small one (fewer request fragments).
    let surbs = if bigReply.unwrap_or(false) { SURBS_CHAT } else { SURBS_TEXT };

    let dir = data_dir(&app)?;
    let w = wallet::load(&dir);
    let srv = server_addr(&w)?;

    // A model the picker showed at rate 0/0 needs no account, session or
    // signature — the server re-checks against its own price table, so a wrong
    // flag just comes back as "requires a funded, signed session".
    if free.unwrap_or(false) {
        let mut req = json!({
            "v":PROTO,"kind":"chat","id":rand_hex(16),"model":model,"messages":messages,"stream":false
        });
        if let Some(mt) = maxTokens {
            req["maxTokens"] = json!(mt);
        }
        let sent_app = app.clone();
        let resp = transport
            .round_trip_notify(&srv, &req, surbs, TIMEOUT_MS, move || {
                let _ = sent_app.emit("chat-sent", ());
            })
            .await?;
        // No balance in the reply: nothing was spent, so the UI keeps its number.
        return Ok(json!({
            "text": resp.get("text"),
            "usage": resp.get("usage"),
            "images": resp.get("images"),
        }));
    }

    let m = w.mnemonic.clone().ok_or("no account — create one and buy credit")?;

    let sk = account::derive_session_keys(&m, w.session_index)?;
    let (balance0, mut counter0) = session_status(&transport, &srv, &sk).await?;
    // Fund the session from a held coconut book when it runs dry.
    if balance0 == 0 {
        if !w.coconut_purses.is_empty() {
            redeem_coconut(&app, &transport, &srv, REDEEM_CHUNK_COINS).await?;
            counter0 = session_status(&transport, &srv, &sk).await?.1;
        } else {
            return Err("no SCRAI credit — buy credit first".into());
        }
    }
    let counter = counter0 + 1;

    // The signed body must serialise exactly like the server's canonicalBody:
    // {model, messages, maxTokens} in that key order (preserve_order is on).
    let max_val = maxTokens.map(|v| json!(v)).unwrap_or(Value::Null);
    let body = serde_json::to_string(&json!({"model": model, "messages": messages, "maxTokens": max_val}))
        .map_err(|e| e.to_string())?;
    let sig = sk.sign(counter, &body);

    let mut req = json!({
        "v":PROTO,"kind":"chat","id":rand_hex(16),"model":model,"messages":messages,
        "stream":false,"sessionId":sk.session_id,"counter":counter,"sig":sig,
        // The session's public key rides along so the server can verify the
        // signature statelessly: id_for(publicKey) must equal sessionId.
        "publicKey":sk.public_key_pem
    });
    if let Some(mt) = maxTokens {
        req["maxTokens"] = json!(mt);
    }
    // Tell the UI the instant the request has fully left for the mixnet, so its
    // status line flips from "Sending…" to "Thinking" at the real moment.
    let sent_app = app.clone();
    let resp = transport
        .round_trip_notify(&srv, &req, surbs, TIMEOUT_MS, move || {
            let _ = sent_app.emit("chat-sent", ());
        })
        .await?;
    // Report TOTAL spendable credit (funded session balance + un-redeemed coconut
    // coins), consistent with `state`, so the UI number only drops by real chat cost
    // — not by the internal session↔purse shuffle that auto-fund performs.
    let session_balance = resp.get("balance").and_then(|b| b.as_u64()).unwrap_or(0);
    let total_balance = session_balance.saturating_add(coconut_held_scrai(&app));
    // `images` is what image models (e.g. pollinations) return — forward it, or
    // the answer arrives blank and silent.
    Ok(json!({
        "text": resp.get("text"),
        "usage": resp.get("usage"),
        "balance": total_balance,
        "images": resp.get("images"),
    }))
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

/// One route edge (entry or exit) as the UI shows it: gateway identity + its
/// self-reported country (empty if the directory didn't resolve it).
fn edge_json(id: &str, info: Option<nym::GatewayInfo>) -> Value {
    match info {
        Some(g) => json!({ "id": id, "country": g.country, "host": g.host }),
        None => json!({ "id": id, "country": "", "host": "" }),
    }
}

/// The honestly-knowable edges of the current mixnet route:
///   entry = this client's gateway (selectable), exit = the server's gateway.
/// The two middle mix hops are re-randomised per packet and are NOT reported.
#[tauri::command]
async fn mixnet_route(app: AppHandle, transport: State<'_, Arc<Transport>>) -> Result<Value, String> {
    let w = wallet::load(&data_dir(&app)?);
    let server = server_addr(&w).ok();

    // The UI polls this every ~2.5s while it shows "connecting". The poll itself
    // must stay NON-BLOCKING (a status query queued behind a long round trip
    // froze the route grey while traffic was flowing) — so it reads the
    // lock-free view and, when down, kicks a single background reconnect task.
    let live = transport.is_connected();
    if !live {
        transport.spawn_reconnect();
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
    Ok(json!({ "entry": entry, "exit": exit, "chosen": w.entry_gateway, "live": live }))
}

/// Directory nodes usable as an entry gateway, for the picker.
#[tauri::command]
async fn list_entry_gateways(transport: State<'_, Arc<Transport>>) -> Result<Value, String> {
    let list = transport.entry_gateways().await?;
    Ok(json!(list))
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
    w.entry_gateway = id.clone();
    wallet::save(&dir, &w)?;
    transport.set_entry_gateway(id.clone()).await;
    Ok(json!({ "entry_gateway": id }))
}

/// Save a base64 image to a user-chosen path via a native "save as…" dialog.
/// The webview can't trigger downloads, so image saves route through here.
/// Returns the chosen path, or null if the user cancelled.
#[cfg(not(target_os = "ios"))]
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

// iOS has no rfd backend; saving routes through the share sheet once that
// lands. Until then the command exists (same signature) but reports why.
#[cfg(target_os = "ios")]
#[tauri::command]
async fn save_image(data: String, filename: String) -> Result<Option<String>, String> {
    let _ = (data, filename);
    Err("saving images on iOS is not wired up yet (share sheet pending)".into())
}

/// Open an http(s) URL in the OS default browser. The webview itself won't
/// follow target=_blank links, so provider T&C / checkout links route here.
#[tauri::command]
fn open_external(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("only http(s) urls are allowed".into());
    }
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(target_os = "linux")]
    let spawned = std::process::Command::new("xdg-open").arg(&url).spawn();
    #[cfg(target_os = "windows")]
    let spawned = std::process::Command::new("cmd").args(["/C", "start", "", &url]).spawn();
    // iOS cannot spawn processes; opening Safari needs UIApplication openURL
    // (tauri-plugin-opener), which is not wired up yet.
    #[cfg(target_os = "ios")]
    let spawned: std::io::Result<()> = Err(std::io::Error::other(
        "opening external links on iOS is not wired up yet",
    ));
    spawned.map(|_| ()).map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(Arc::new(Transport::new()))
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            // Apply a previously-chosen entry gateway before the first request.
            let handle = app.handle().clone();
            let transport = app.state::<Arc<Transport>>().inner().clone();
            tauri::async_runtime::spawn(async move {
                if let Ok(dir) = data_dir(&handle) {
                    let w = wallet::load(&dir);
                    if w.entry_gateway.is_some() {
                        transport.set_entry_gateway(w.entry_gateway).await;
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            state, set_server, account_new, account_reveal, account_restore,
            invoice, invoice_status, invoice_cancel, ocr_scan, pdf_text, pdf_ocr, pdf_pages, collect, redeem, chat,
            smart_available, smart_detect, coconut_withdraw_test, coconut_withdraw, coconut_spend, coconut_redeem,
            mixnet_route, list_entry_gateways, set_entry_gateway, open_external, save_image,
            upload_begin, upload_chunk
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
