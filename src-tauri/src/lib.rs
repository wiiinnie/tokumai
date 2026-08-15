// ---------------------------------------------------------------------------
// ScrambleAI desktop — the Rust core.
//
// The public/ UI runs in the webview and calls these commands via `invoke`.
// Account + held ecash are local; everything else talks to the scrai-server
// over the embedded mixnet (nym.rs). The ecash crypto is in ecash.rs, the
// account crypto in account.rs — both byte-compatible with the TS client.
// ---------------------------------------------------------------------------

mod account;
mod ecash;
mod nym;
mod wallet;

use ecash::{blind_packet, tier_packets, unblind_packet, PublicKey, SignedOutput};
use nym::Transport;
use rand::RngCore;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Manager, State};

const TIERS: [u32; 4] = [5, 10, 20, 50];
const SCRAI_PER_USD: u64 = 100_000;
const PROTO: u64 = 1;

// Reply-SURB budgets: a small answer needs few, a chat answer more.
const SURBS_SMALL: u32 = 80;
const SURBS_CHAT: u32 = 500;
const TIMEOUT_MS: u64 = 120_000;

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

fn tiers_scrai() -> Vec<u64> {
    TIERS.iter().map(|t| *t as u64 * SCRAI_PER_USD).collect()
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

// ---- mixnet helpers used by several commands ------------------------------

async fn fetch_keys(t: &Transport, srv: &str) -> Result<Vec<PublicKey>, String> {
    let resp = t
        .round_trip(srv, &json!({"v":PROTO,"kind":"keys","id":rand_hex(16)}), SURBS_SMALL, TIMEOUT_MS)
        .await?;
    serde_json::from_value(resp.get("keys").cloned().unwrap_or(json!([]))).map_err(|e| e.to_string())
}

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

/// Redeem all held ecash packets into the session, one clean tier per open.
async fn redeem_held(app: &AppHandle, t: &Transport, srv: &str) -> Result<u64, String> {
    let dir = data_dir(app)?;
    let mut w = wallet::load(&dir);
    let mnemonic = w.mnemonic.clone().ok_or("no account")?;
    let sk = account::derive_session_keys(&mnemonic, w.session_index)?;

    while !w.ecash.is_empty() {
        let packet = w.ecash[0].clone();
        let proofs_json = serde_json::to_value(&packet).map_err(|e| e.to_string())?;
        let req = json!({"v":PROTO,"kind":"session.open","id":rand_hex(16),"proofs":proofs_json,"publicKey":sk.public_key_pem});
        match t.round_trip(srv, &req, SURBS_SMALL, TIMEOUT_MS).await {
            Ok(_) => {}
            // Already spent = a prior redeem went through; drop it and move on.
            Err(e) if e.contains("already been spent") => {}
            Err(e) => return Err(e),
        }
        w.ecash.remove(0);
        wallet::save(&dir, &w)?;
    }
    let (balance, _) = session_status(t, srv, &sk).await.unwrap_or((0, 0));
    Ok(balance)
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

    Ok(json!({
        "account": account,
        "balance": balance,
        "held": w.held_total(),
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
async fn invoice(app: AppHandle, transport: State<'_, Arc<Transport>>, usd: u32) -> Result<Value, String> {
    let w = wallet::load(&data_dir(&app)?);
    let srv = server_addr(&w)?;
    let m = w.mnemonic.ok_or("no account — create one first")?;
    let a = account::from_mnemonic(&m)?;
    let nonce = rand_hex(16);
    let sig = a.sign(&format!("invoice:{}", usd), &nonce);
    let req = json!({"v":PROTO,"kind":"invoice.create","id":rand_hex(16),"publicKey":a.public_key_pem,"usd":usd,"nonce":nonce,"sig":sig});
    let resp = transport.round_trip(&srv, &req, SURBS_SMALL, TIMEOUT_MS).await?;

    let options: Vec<Value> = resp
        .get("options")
        .and_then(|o| o.as_array())
        .map(|arr| {
            arr.iter()
                .map(|o| {
                    let uri = o.get("uri").and_then(|u| u.as_str()).unwrap_or_default();
                    let mut oo = o.clone();
                    oo["qr"] = json!(qr_svg(uri));
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
    Ok(json!({ "status": resp.get("status"), "entitlement": resp.get("entitlement") }))
}

#[tauri::command]
async fn collect(app: AppHandle, transport: State<'_, Arc<Transport>>) -> Result<Value, String> {
    let _op = transport.begin_op().await;
    let dir = data_dir(&app)?;
    let w0 = wallet::load(&dir);
    let srv = server_addr(&w0)?;
    let m = w0.mnemonic.clone().ok_or("no account")?;
    let a = account::from_mnemonic(&m)?;

    // How much is owed?
    let nonce = rand_hex(16);
    let sig = a.sign("entitlement", &nonce);
    let resp = transport
        .round_trip(&srv, &json!({"v":PROTO,"kind":"entitlement","id":rand_hex(16),"publicKey":a.public_key_pem,"nonce":nonce,"sig":sig}), SURBS_SMALL, TIMEOUT_MS)
        .await?;
    let owed = resp.get("entitlement").and_then(|e| e.as_u64()).unwrap_or(0);
    if owed == 0 {
        return Ok(json!({ "collected": 0u64, "held": w0.held_total() }));
    }

    let keys = fetch_keys(&transport, &srv).await?;
    let mut w = wallet::load(&dir);
    for packet_scrai in tier_packets(owed, &tiers_scrai()) {
        let (outputs, states) = blind_packet(packet_scrai);
        let nonce = rand_hex(16);
        let sig = a.sign(&format!("withdraw:{}", packet_scrai), &nonce);
        let outputs_json = serde_json::to_value(&outputs).map_err(|e| e.to_string())?;
        let req = json!({"v":PROTO,"kind":"withdraw","id":rand_hex(16),"publicKey":a.public_key_pem,"outputs":outputs_json,"nonce":nonce,"sig":sig});
        let wresp = transport.round_trip(&srv, &req, SURBS_SMALL, TIMEOUT_MS).await?;
        let sigs: Vec<SignedOutput> =
            serde_json::from_value(wresp.get("signatures").cloned().unwrap_or(json!([]))).map_err(|e| e.to_string())?;
        let proofs = unblind_packet(&states, &sigs, &keys)?;
        w.ecash.push(proofs);
        wallet::save(&dir, &w)?;
    }
    Ok(json!({ "collected": owed, "held": w.held_total() }))
}

#[tauri::command]
async fn redeem(app: AppHandle, transport: State<'_, Arc<Transport>>) -> Result<Value, String> {
    let _op = transport.begin_op().await;
    let w = wallet::load(&data_dir(&app)?);
    let srv = server_addr(&w)?;
    let balance = redeem_held(&app, &transport, &srv).await?;
    Ok(json!({ "balance": balance }))
}

#[tauri::command]
async fn chat(
    app: AppHandle,
    transport: State<'_, Arc<Transport>>,
    model: String,
    messages: Value,
    #[allow(non_snake_case)] maxTokens: Option<u64>,
) -> Result<Value, String> {
    // Serialise the whole command: session_status + chat must be one atomic unit,
    // or two concurrent chats race the session counter (crossed replies / hangs).
    let _op = transport.begin_op().await;

    let dir = data_dir(&app)?;
    let w = wallet::load(&dir);
    let srv = server_addr(&w)?;
    let m = w.mnemonic.clone().ok_or("no account — create one and buy credit")?;

    // Fund the session from held ecash first (the anonymous half).
    if !w.ecash.is_empty() {
        redeem_held(&app, &transport, &srv).await?;
    }
    let sk = account::derive_session_keys(&m, w.session_index)?;
    let (_, counter0) = session_status(&transport, &srv, &sk).await.map_err(|e| {
        if e.contains("unknown session") {
            "no SCRAI credit yet — buy credit (it funds your session on first use)".to_string()
        } else {
            e
        }
    })?;
    let counter = counter0 + 1;

    // The signed body must serialise exactly like the server's canonicalBody:
    // {model, messages, maxTokens} in that key order (preserve_order is on).
    let max_val = maxTokens.map(|v| json!(v)).unwrap_or(Value::Null);
    let body = serde_json::to_string(&json!({"model": model, "messages": messages, "maxTokens": max_val}))
        .map_err(|e| e.to_string())?;
    let sig = sk.sign(counter, &body);

    let mut req = json!({
        "v":PROTO,"kind":"chat","id":rand_hex(16),"model":model,"messages":messages,
        "stream":false,"sessionId":sk.session_id,"counter":counter,"sig":sig
    });
    if let Some(mt) = maxTokens {
        req["maxTokens"] = json!(mt);
    }
    let resp = transport.round_trip(&srv, &req, SURBS_CHAT, TIMEOUT_MS).await?;
    // `images` is what image models (e.g. pollinations) return — forward it, or
    // the answer arrives blank and silent.
    Ok(json!({
        "text": resp.get("text"),
        "usage": resp.get("usage"),
        "balance": resp.get("balance"),
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
    Ok(json!({ "entry": entry, "exit": exit, "chosen": w.entry_gateway }))
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
            invoice, invoice_status, collect, redeem, chat,
            mixnet_route, list_entry_gateways, set_entry_gateway, open_external, save_image,
            upload_begin, upload_chunk
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
