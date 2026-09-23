//! The tokumai app, desktop side. The interface is the web page in `public/`; everything it
//! asks for comes through the commands here, and everything that talks to the enclave goes
//! through `tokumai_client` — the same core the phone apps use.
//!
//! What lives here and nowhere else: the recovery phrase (encrypted under a keychain key,
//! `profile`), the chat history (`vault`), and the platform bits (save dialogs, the
//! browser, OCR). There is no money on the device: the balance is on the account, inside
//! the enclave, and comes back from the phrase on any device.

mod detect;
mod keystore;
mod ocr;
mod profile;
mod target;
mod vault;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{Mutex, Notify};
use tokumai_client::app::{BoxFuture, Connection, Connector, MixConnector};
use tokumai_client::gateways::{self, Directory, EntryChoice};
use tokumai_client::Transport;
use tokumai_core::account::{self, Account};

fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    // Windows: Local, not Roaming — a domain profile would carry the encrypted profile and
    // the chat history to the server with the login.
    #[cfg(target_os = "windows")]
    let dir = app.path().app_local_data_dir().map_err(|e| e.to_string())?;
    #[cfg(not(target_os = "windows"))]
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    // A debug build shares the bundle id with the installed app, and so its data folder:
    // it keeps a folder of its own, next to it, and never touches the real one.
    if cfg!(debug_assertions) {
        let name = format!("{}.dev", dir.file_name().and_then(|n| n.to_str()).unwrap_or("tokumai"));
        return Ok(dir.with_file_name(name));
    }
    Ok(dir)
}

/// The account's short, safe-to-show name: the first sixteen characters of its id in fours.
fn fingerprint(account_id: &str) -> String {
    account_id.as_bytes().chunks(4).take(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join("-")
}

#[cfg(test)]
fn rand_hex(n: usize) -> String {
    hex::encode((0..n).map(|_| rand::random::<u8>()).collect::<Vec<u8>>())
}

// ---- the connection to the enclave ----------------------------------------------------

/// What the route display shows: the entry gateway of the live connection.
#[derive(Default)]
struct Route {
    entry: Option<String>,
    live: bool,
}

struct AppState {
    conn: Mutex<Option<Connection>>,
    /// Rung when the person stops waiting for an answer.
    cancel: Notify,
    route: std::sync::Mutex<Route>,
    /// Nym's directory and the operator's family, for the gateway picker and the route.
    directory: Mutex<Option<Directory>>,
    /// The catalogue and the plan ladder, read once per start.
    models: Mutex<Option<Value>>,
    ladder: Mutex<Option<Value>>,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            conn: Mutex::new(None),
            cancel: Notify::new(),
            route: Default::default(),
            directory: Mutex::new(None),
            models: Mutex::new(None),
            ladder: Mutex::new(None),
        }
    }
}

/// The mixnet connector, reporting its steps to the interface (the boot animation).
struct Reporting {
    inner: MixConnector,
    app: AppHandle,
}

impl Connector for Reporting {
    fn connect(&self) -> BoxFuture<'_, Result<Box<dyn Transport>, String>> {
        Box::pin(async move {
            let st = self.app.state::<AppState>();
            match self.inner.connect().await {
                Ok(t) => {
                    let entry = self.inner.last_entry.lock().ok().and_then(|e| e.clone());
                    if let Ok(mut r) = st.route.lock() {
                        *r = Route { entry: entry.clone(), live: true };
                    }
                    Ok(t)
                }
                Err(e) => {
                    if let Ok(mut r) = st.route.lock() {
                        r.live = false;
                    }
                    let _ = self.app.emit("mixnet-phase", json!({ "step": "failed", "detail": e }));
                    Err(e)
                }
            }
        })
    }
}

fn entry_choice(p: &profile::Profile) -> EntryChoice {
    match &p.entry_gateway {
        Some(id) => EntryChoice::Chosen(id.clone()),
        None => EntryChoice::Random,
    }
}

fn new_connection(app: &AppHandle, p: &profile::Profile) -> Result<Connection, String> {
    let address = target::enclave_address()?;
    let mut inner = MixConnector::new(&address, entry_choice(p));
    inner.traffic = p.traffic.map(|(cover_ms, mix_ms, send_ms, continuous)| tokumai_client::mix::Traffic { cover_ms, mix_ms, send_ms, continuous });
    // Each step as it starts, so the interface follows the real connection instead of a
    // timer: directory · gateway · cover · proof · ready.
    let steps = |app: AppHandle| -> tokumai_client::app::Steps {
        std::sync::Arc::new(move |step: &str| {
            let _ = app.emit("mixnet-phase", json!({ "step": step, "detail": "" }));
        })
    };
    inner.steps = Some(steps(app.clone()));
    // A long answer (a picture) comes in pieces; the wait says how many are in.
    let receiving = app.clone();
    inner.progress = Some(std::sync::Arc::new(move |have: usize, of: usize| {
        let _ = receiving.emit("reply-progress", json!({ "have": have, "of": of }));
    }));
    let connector = Reporting { inner, app: app.clone() };
    let mut conn = Connection::new(Box::new(connector), target::policy()?);
    conn.on_step(steps(app.clone()));
    Ok(conn)
}

fn current_account(app: &AppHandle) -> Result<Account, String> {
    let p = profile::load(&data_dir(app)?);
    let m = p.mnemonic.ok_or("no account on this device yet")?;
    account::from_mnemonic(&m)
}

/// One operation for the account on this device. Refusals from the enclave come back as
/// errors, with its own words.
async fn call(app: &AppHandle, op: &str, body: Value) -> Result<Value, String> {
    let account = current_account(app)?;
    let st = app.state::<AppState>();
    let mut guard = st.conn.lock().await;
    if guard.is_none() {
        *guard = Some(new_connection(app, &profile::load(&data_dir(app)?))?);
    }
    let conn = guard.as_mut().ok_or("no connection")?;
    let answer = conn.call(&account, op, &body).await?;
    if answer.get("kind").and_then(|k| k.as_str()) == Some("error") {
        let msg = answer.get("error").and_then(|e| e.as_str()).unwrap_or("the enclave refused").to_string();
        // Marked, so the interface offers credit rather than a retry.
        return Err(if answer["noCredit"] == true { format!("NO_CREDIT: {msg}") } else { msg });
    }
    Ok(answer)
}

/// Connect and attest, with or without an account: the proof needs none, and the interface
/// wants the route up (and the enclave known) before anything is asked.
async fn ensure_ready(app: &AppHandle) -> Result<(), String> {
    let st = app.state::<AppState>();
    let mut guard = st.conn.lock().await;
    if guard.is_none() {
        *guard = Some(new_connection(app, &profile::load(&data_dir(app)?))?);
    }
    guard.as_mut().ok_or("no connection")?.ready().await
}

/// Drop the connection, so the next call builds a new one (another gateway, another
/// account). Waits for a call in flight to finish first.
async fn reset_connection(app: &AppHandle) {
    let st = app.state::<AppState>();
    *st.conn.lock().await = None;
    if let Ok(mut r) = st.route.lock() {
        *r = Route::default();
    };
}

// ---- state ----------------------------------------------------------------------------

/// The enclave's plan ladder in the shape the plan sheet draws: one row per tier with its
/// monthly and yearly price in cents and what it saves against the entry tier. Empty when
/// plans are not sold by card here — the sheet then offers none.
fn ui_ladder(l: &Value) -> Value {
    if l["byCard"] != true {
        return json!([]);
    }
    let rows: Vec<Value> = l["tiers"]
        .as_array()
        .map(|tiers| {
            tiers
                .iter()
                .map(|t| {
                    let tier = t["tier"].as_u64().unwrap_or(0);
                    let cents = t["web"]["month"].as_u64().unwrap_or_else(|| t["cents"].as_u64().unwrap_or(0));
                    let yearly = t["web"]["year"].as_u64().unwrap_or_else(|| tokumai_core::subscription::yearly_cents(cents));
                    json!({ "tier": tier, "toku": t["toku"], "cents": cents, "yearlyCents": yearly,
                            "savesCents": tokumai_core::subscription::saving_cents(tier as usize) })
                })
                .collect()
        })
        .unwrap_or_default();
    json!(rows)
}

/// The plan as the sheet reads it (`toku_per_month` is its older name for the allowance).
fn ui_plan(p: &Value) -> Value {
    if !p.is_object() {
        return Value::Null;
    }
    let mut p = p.clone();
    p["toku_per_month"] = p["tokuPerMonth"].clone();
    p
}

fn account_json(p: &profile::Profile) -> Value {
    match p.mnemonic.as_deref().map(account::from_mnemonic) {
        Some(Ok(a)) => json!({ "fingerprint": fingerprint(&a.account_id), "phraseVerified": p.phrase_verified }),
        _ => Value::Null,
    }
}

/// What the interface can know without the network: read first at launch, so a slow
/// connect never makes the app claim there is no account.
#[tauri::command]
fn local_state(app: AppHandle) -> Result<Value, String> {
    let p = profile::load(&data_dir(&app)?);
    Ok(json!({ "account": account_json(&p), "server": target::enclave_address().ok() }))
}

/// Everything the interface shows: account, balance, plan, models, the plan ladder.
#[tauri::command]
async fn state(app: AppHandle) -> Result<Value, String> {
    let p = profile::load(&data_dir(&app)?);
    let mut out = json!({
        // Developer tools exist only in a debug build.
        "devBuild": cfg!(debug_assertions),
        "appVersion": app.package_info().version.to_string(),
        "account": account_json(&p),
        "server": target::enclave_address().ok(),
        "balance": 0,
        "models": [],
    });
    let st = app.state::<AppState>();
    if p.mnemonic.is_none() {
        if let Some(conn) = st.conn.lock().await.as_ref() {
            if let Some(s) = conn.session() {
                out["attested"] = json!({ "platform": s.claims.platform, "measurement": s.claims.measurement });
            }
        }
        return Ok(out);
    }
    // One round trip for balance, catalogue and plans: over the mixnet each costs about
    // three seconds, and the app used to make three of them before its first screen.
    match call(&app, "start", json!({})).await {
        Ok(r) => {
            let b = &r["balance"];
            let bal = &b["balance"];
            out["balance"] = bal["total"].clone();
            out["allowance"] = json!({ "left": bal["allowance"], "endsMs": bal["allowance_ends_ms"] });
            let prepaid: u64 = bal["prepaid"].as_array().map(|l| l.iter().filter_map(|x| x[0].as_u64()).sum()).unwrap_or(0);
            out["prepaid"] = json!(prepaid);
            out["plan"] = ui_plan(&b["plan"]);
            if r["models"].as_array().is_some_and(|m| !m.is_empty()) {
                *st.models.lock().await = Some(r["models"].clone());
            }
            if r["plans"].is_object() {
                *st.ladder.lock().await = Some(r["plans"].clone());
            }
        }
        Err(e) => out["error"] = json!(e),
    }
    out["models"] = st.models.lock().await.clone().unwrap_or(json!([]));
    let ladder = st.ladder.lock().await.clone().unwrap_or(Value::Null);
    out["plans"] = ui_ladder(&ladder);
    out["consentVersion"] = ladder["consentVersion"].clone();
    if let Some(conn) = st.conn.lock().await.as_ref() {
        if let Some(s) = conn.session() {
            out["attested"] = json!({ "platform": s.claims.platform, "measurement": s.claims.measurement });
        }
    }
    Ok(out)
}

// ---- the account ----------------------------------------------------------------------

#[tauri::command]
async fn account_new(app: AppHandle, force: Option<bool>) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let mut p = profile::load(&dir);
    if p.mnemonic.is_some() && !force.unwrap_or(false) {
        return Err("there is already an account on this device".into());
    }
    let a = account::create_account();
    p.mnemonic = Some(a.mnemonic.clone());
    // A new phrase is not yet written down: the three-word check comes before any purchase.
    p.phrase_verified = false;
    p.pending_plan_session = None;
    profile::save(&dir, &p)?;
    forget_account_caches(&app).await;
    Ok(json!({ "mnemonic": a.mnemonic, "fingerprint": fingerprint(&a.account_id) }))
}

#[tauri::command]
async fn account_restore(app: AppHandle, mnemonic: String) -> Result<Value, String> {
    let a = account::from_mnemonic(mnemonic.trim())?;
    let dir = data_dir(&app)?;
    let mut p = profile::load(&dir);
    p.mnemonic = Some(a.mnemonic.clone());
    // Typing all the words is the proof they are written down.
    p.phrase_verified = true;
    p.pending_plan_session = None;
    profile::save(&dir, &p)?;
    forget_account_caches(&app).await;
    Ok(json!({ "fingerprint": fingerprint(&a.account_id) }))
}

/// Remove the phrase from this device. The account and its balance stay in the enclave;
/// the phrase brings them back.
#[tauri::command]
async fn account_delete(app: AppHandle) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let mut p = profile::load(&dir);
    p.mnemonic = None;
    p.phrase_verified = false;
    p.pending_plan_session = None;
    profile::save(&dir, &p)?;
    forget_account_caches(&app).await;
    Ok(json!({ "ok": true }))
}

async fn forget_account_caches(app: &AppHandle) {
    let st = app.state::<AppState>();
    *st.ladder.lock().await = None;
}

#[tauri::command]
fn account_reveal(app: AppHandle) -> Result<Value, String> {
    let p = profile::load(&data_dir(&app)?);
    Ok(json!({ "mnemonic": p.mnemonic.ok_or("no account")? }))
}

/// The phrase and a QR code of it, to set up another device.
#[tauri::command]
fn account_migrate_qr(app: AppHandle) -> Result<Value, String> {
    let p = profile::load(&data_dir(&app)?);
    let m = p.mnemonic.ok_or("no account")?;
    let qr = qrcode::QrCode::new(m.as_bytes())
        .map(|c| c.render::<qrcode::render::svg::Color>().min_dimensions(200, 200).quiet_zone(true).build())
        .unwrap_or_default();
    Ok(json!({ "mnemonic": m, "qr": qr }))
}

/// The three-word check, step one: three positions, fresh on every call. The words never
/// go to the interface — it gets numbers, sends back what was typed, and hears yes or no.
#[tauri::command]
fn phrase_check_start(app: AppHandle) -> Result<Value, String> {
    let p = profile::load(&data_dir(&app)?);
    let n = p.mnemonic.as_deref().map(|m| m.split_whitespace().count()).ok_or("no account")?;
    use rand::seq::SliceRandom;
    let mut all: Vec<u32> = (1..=n as u32).collect();
    all.shuffle(&mut rand::rngs::OsRng);
    let mut pick: Vec<u32> = all.into_iter().take(3).collect();
    pick.sort_unstable();
    Ok(json!({ "positions": pick, "total": n, "verified": p.phrase_verified }))
}

/// Step two. Case and surrounding space are forgiven; which word was wrong is not said.
#[tauri::command]
fn phrase_check_verify(app: AppHandle, positions: Vec<u32>, words: Vec<String>) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let mut p = profile::load(&dir);
    let m = p.mnemonic.clone().ok_or("no account")?;
    let all: Vec<&str> = m.split_whitespace().collect();
    if positions.len() != 3 || words.len() != 3 {
        return Err("three positions and three words".into());
    }
    let ok = positions.iter().zip(words.iter()).all(|(pos, typed)| {
        let idx = (*pos as usize).wrapping_sub(1);
        all.get(idx).is_some_and(|real| real.eq_ignore_ascii_case(typed.trim()))
    });
    if ok && !p.phrase_verified {
        p.phrase_verified = true;
        profile::save(&dir, &p)?;
    }
    Ok(json!({ "ok": ok }))
}

/// The iCloud Keychain copy of the phrase is an iPhone feature.
#[tauri::command]
fn phrase_backup_get() -> Value {
    json!({ "available": false, "on": false })
}

// ---- chat -----------------------------------------------------------------------------

/// One question. Pictures and PDFs travel inside the messages (`attachments`); the mixnet
/// framing carries big requests in parts. Stopping the wait (`cancel_chat`) drops this
/// call; the enclave may still answer, and a resend of the same question is charged once.
#[tauri::command]
async fn chat(
    app: AppHandle,
    model: String,
    messages: Value,
    max_tokens: Option<u64>,
    live: Option<bool>,
    thinking_budget: Option<u64>,
    image_size: Option<String>,
    lossless: Option<bool>,
) -> Result<Value, String> {
    let body = json!({
        "model": model, "messages": messages, "maxTokens": max_tokens, "live": live.unwrap_or(false),
        "thinkingBudget": thinking_budget, "imageSize": image_size, "lossless": lossless.unwrap_or(false),
    });
    let _ = app.emit("chat-sent", ());
    let st = app.state::<AppState>();
    let mut answer = tokio::select! {
        a = Box::pin(call(&app, "chat", body)) => a?,
        _ = st.cancel.notified() => return Err("stopped".into()),
    };
    // The footer's words for what the answer took and what it cost.
    let u = answer["usage"].clone();
    answer["usage"] = json!({
        "inputTokens": u["input"], "outputTokens": u["output"], "cachedInputTokens": u["cachedInput"],
        "imageSize": u["imageSize"], "searches": u["searches"],
        "billing": { "priceToku": answer["cost"], "estimated": answer["estimated"], "model": model, "pricingVersion": "the enclave's own" },
    });
    Ok(answer)
}

#[tauri::command]
fn cancel_chat(state: State<'_, AppState>) {
    state.cancel.notify_waiters();
}

// ---- plans ----------------------------------------------------------------------------

/// The ladder: tiers, allowances, card prices (from Stripe, read by the enclave), in the
/// sheet's shape.
#[tauri::command]
async fn plan_ladder(app: AppHandle) -> Result<Value, String> {
    let l = call(&app, "plans", json!({})).await?;
    *app.state::<AppState>().ladder.lock().await = Some(l.clone());
    Ok(ui_ladder(&l))
}

/// Order a plan by card. `consent` carries the two confirmations and the version of the
/// text they were given to; without them the enclave refuses. Returns the Stripe page.
#[tauri::command]
async fn plan_checkout(app: AppHandle, tier: u64, yearly: bool, consent: Value) -> Result<Value, String> {
    let r = call(&app, "plan.create", json!({ "tier": tier, "yearly": yearly, "consent": consent })).await?;
    let session = r["session"].as_str().unwrap_or("").to_string();
    let checkout = r["checkout"].as_str().unwrap_or("").to_string();
    if session.is_empty() || !checkout.starts_with("https://") {
        return Err("no checkout came back — plans may not be on sale here".into());
    }
    let dir = data_dir(&app)?;
    let mut p = profile::load(&dir);
    p.pending_plan_session = Some(session);
    profile::save(&dir, &p)?;
    Ok(json!({ "checkout": checkout, "expiresAt": r["expiresAt"] }))
}

/// Whether the open checkout was paid: none | pending | paid (with the plan).
#[tauri::command]
async fn plan_poll(app: AppHandle) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let Some(session) = profile::load(&dir).pending_plan_session else { return Ok(json!({ "status": "none" })) };
    let r = call(&app, "plan.status", json!({ "session": session })).await?;
    if r["kind"] == "plan.paid" {
        let mut p = profile::load(&dir);
        p.pending_plan_session = None;
        profile::save(&dir, &p)?;
        return Ok(json!({ "status": "paid", "plan": ui_plan(&r["plan"]) }));
    }
    Ok(json!({ "status": "pending" }))
}

#[tauri::command]
async fn plan_change(app: AppHandle, tier: u64, yearly: bool) -> Result<Value, String> {
    let mut r = call(&app, "plan.change", json!({ "tier": tier, "yearly": yearly })).await?;
    r["plan"] = ui_plan(&r["plan"]);
    Ok(r)
}

/// Forget an open checkout. Nothing is cancelled: an unpaid Stripe checkout expires.
#[tauri::command]
fn plan_forget(app: AppHandle) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let mut p = profile::load(&dir);
    p.pending_plan_session = None;
    profile::save(&dir, &p)?;
    Ok(json!({ "ok": true }))
}

// ---- the route and the entry gateway (rule A1) ------------------------------------------

async fn directory(app: &AppHandle) -> Result<Directory, String> {
    let st = app.state::<AppState>();
    let mut d = st.directory.lock().await;
    if d.is_none() {
        *d = Some(gateways::fetch().await?);
    }
    d.clone().ok_or_else(|| "no directory".into())
}

fn edge(dir: Option<&Directory>, id: &str) -> Value {
    let g = dir.and_then(|d| d.gateways.iter().find(|g| g.id == id));
    json!({ "id": id, "country": g.map(|g| g.country.as_str()).unwrap_or(""), "host": g.map(|g| g.host.as_str()).unwrap_or("") })
}

/// The two ends of the route that can honestly be known: this app's entry gateway, and
/// the gateway the enclave sits behind. The mix hops between change with every packet.
#[tauri::command]
async fn mixnet_route(app: AppHandle) -> Result<Value, String> {
    let p = profile::load(&data_dir(&app)?);
    let (entry, live) = {
        let st = app.state::<AppState>();
        let r = st.route.lock().map_err(|_| "unavailable")?;
        (r.entry.clone(), r.live)
    };
    if !live {
        // The interface polls this while it shows "connecting": make sure someone is.
        let h = app.clone();
        tauri::async_runtime::spawn(async move {
            if let Ok(g) = h.state::<AppState>().conn.try_lock() {
                let idle = g.as_ref().map(|c| !c.has_transport()).unwrap_or(true);
                drop(g);
                if idle {
                    let _ = ensure_ready(&h).await;
                }
            }
        });
    }
    let dir = app.state::<AppState>().directory.lock().await.clone();
    let exit = target::enclave_address().ok().and_then(|a| gateways::gateway_of(&a).map(str::to_string));
    Ok(json!({
        "entry": entry.as_deref().map(|id| edge(dir.as_ref(), id)),
        "exit": exit.as_deref().map(|id| edge(dir.as_ref(), id)),
        "chosen": p.entry_gateway,
        "random": p.entry_gateway.is_none(),
        "live": live,
    }))
}

/// The entry gateways the app may use: Nym's directory without the operator's family and
/// without the enclave's own gateway.
#[tauri::command]
async fn list_entry_gateways(app: AppHandle) -> Result<Value, String> {
    let d = directory(&app).await?;
    let address = target::enclave_address().unwrap_or_default();
    let mut list: Vec<Value> = gateways::candidates(&d, &address)
        .into_iter()
        .map(|g| json!({ "id": g.id, "country": g.country, "host": g.host, "entry": true }))
        .collect();
    list.sort_by(|a, b| a["country"].as_str().cmp(&b["country"].as_str()).then(a["host"].as_str().cmp(&b["host"].as_str())));
    Ok(json!(list))
}

/// Pick an entry gateway (or clear the pick, with null, for a random one each time).
#[tauri::command]
async fn set_entry_gateway(app: AppHandle, id: Option<String>) -> Result<Value, String> {
    let id = id.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(chosen) = &id {
        let d = directory(&app).await?;
        gateways::pick(&d, &EntryChoice::Chosen(chosen.clone()), &target::enclave_address().unwrap_or_default())?;
    }
    let dir = data_dir(&app)?;
    let mut p = profile::load(&dir);
    p.entry_gateway = id.clone();
    profile::save(&dir, &p)?;
    reset_connection(&app).await;
    Ok(json!({ "entry_gateway": id, "entry_random": id.is_none() }))
}

#[tauri::command]
async fn set_entry_random(app: AppHandle, on: bool) -> Result<Value, String> {
    if on {
        return set_entry_gateway(app, None).await;
    }
    // Random off without a pick: keep whatever gateway the live connection uses.
    let entry = app.state::<AppState>().route.lock().map_err(|_| "unavailable")?.entry.clone();
    set_entry_gateway(app, entry).await
}

/// The speed/anonymity trade-off from Settings → Network & privacy. Takes effect with the
/// next connection, which is made at once.
#[tauri::command]
async fn set_mixnet_perf(app: AppHandle, cover_ms: u64, mix_ms: u64, send_ms: u64, continuous: bool) -> Result<(), String> {
    let t = (cover_ms.clamp(1, 60_000), mix_ms.min(1_000), send_ms.clamp(1, 1_000), continuous);
    let dir = data_dir(&app)?;
    let mut p = profile::load(&dir);
    if p.traffic == Some(t) {
        return Ok(());
    }
    p.traffic = Some(t);
    profile::save(&dir, &p)?;
    reset_connection(&app).await;
    Ok(())
}

/// A round trip that costs nothing (the balance): the mixnet's own latency, for the
/// developer page.
#[tauri::command]
async fn mixnet_ping(app: AppHandle) -> Result<Value, String> {
    let t0 = std::time::Instant::now();
    call(&app, "balance", json!({})).await?;
    Ok(json!({ "ms": t0.elapsed().as_millis() as u64 }))
}

// ---- sleep and wake -------------------------------------------------------------------

#[tauri::command]
async fn app_hidden(app: AppHandle) {
    if let Some(c) = app.state::<AppState>().conn.lock().await.as_mut() {
        c.paused();
    }
}

/// Back in the foreground. After a long pause (or when asked) the mixnet client is
/// replaced — here and now, not at the next question: the interface is showing "a fresh
/// route is being built", and that has to be the truth while it says so. The steps reach
/// it as they happen (`mixnet-phase`), and this returns when the route is up.
#[tauri::command]
async fn app_resumed(app: AppHandle, hidden_ms: u64, force: Option<bool>) -> Result<Value, String> {
    let started = std::time::Instant::now();
    let st = app.state::<AppState>();
    let mut guard = st.conn.lock().await;
    let Some(c) = guard.as_mut() else { return Ok(json!({ "action": "alive", "ms": hidden_ms })) };
    if force.unwrap_or(false) {
        c.drop_transport();
    } else {
        c.resumed();
    }
    if c.has_transport() {
        return Ok(json!({ "action": "alive", "ms": started.elapsed().as_millis() }));
    }
    c.ready().await?;
    log::info!("[enclave] route rebuilt after {} ms away in {} ms", hidden_ms, started.elapsed().as_millis());
    Ok(json!({ "action": "rebuilt", "ms": started.elapsed().as_millis() }))
}

#[tauri::command]
fn mixnet_heartbeat() -> Value {
    json!({ "action": "alive", "ms": 0 })
}

// ---- support (not in the enclave yet) --------------------------------------------------

#[tauri::command]
fn support_send() -> Result<Value, String> {
    Err("support messages come with the next version — until then, write to the address on tokumai's site".into())
}

#[tauri::command]
fn support_list() -> Value {
    json!({ "threads": [] })
}

#[tauri::command]
fn support_diag(app: AppHandle, last_error: Option<String>) -> String {
    format!(
        "app      {}\nos       {} {}\nerror    {}\n",
        app.package_info().version,
        std::env::consts::OS,
        std::env::consts::ARCH,
        last_error.unwrap_or_default().chars().take(200).collect::<String>()
    )
}

// ---- the chat vault -------------------------------------------------------------------

async fn vault_blocking<T: Send + 'static>(app: &AppHandle, f: impl FnOnce(&Path) -> Result<T, String> + Send + 'static) -> Result<T, String> {
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

#[tauri::command]
async fn vault_purge_webdata(webview: tauri::Webview) -> Result<(), String> {
    webview.clear_all_browsing_data().map_err(|e| e.to_string())
}

// ---- files and links ------------------------------------------------------------------

/// Save bytes where the person chooses (a picture, an export). `None` = cancelled.
#[tauri::command]
async fn save_file(data: String, filename: String) -> Result<Option<String>, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let bytes = B64.decode(data.as_bytes()).map_err(|e| format!("bad file data: {e}"))?;
    match rfd::AsyncFileDialog::new().set_file_name(&filename).save_file().await {
        Some(f) => {
            f.write(&bytes).await.map_err(|e| e.to_string())?;
            Ok(Some(f.path().to_string_lossy().to_string()))
        }
        None => Ok(None),
    }
}

#[tauri::command]
async fn save_image(data: String, filename: String) -> Result<Option<String>, String> {
    save_file(data, filename).await
}

/// Only plain http(s) links and a support mailto (one address, subject and body only) may
/// reach the system; the opener hands them over as data, never through a shell.
fn is_openable_url(url: &str) -> bool {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    if let Some(rest) = url.strip_prefix("mailto:") {
        let (addr, query) = rest.split_once('?').unwrap_or((rest, ""));
        let plain = addr.split_once('@').is_some_and(|(u, h)| !u.is_empty() && h.contains('.') && !h.starts_with('.') && !h.ends_with('.'))
            && !addr.contains(',')
            && addr.chars().all(|c| c.is_ascii_alphanumeric() || "._%+-@".contains(c));
        return plain && (query.is_empty() || query.split('&').all(|kv| matches!(kv.split_once('='), Some((k, _)) if k == "subject" || k == "body")));
    }
    let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) else { return false };
    !rest.split(['/', '?', '#']).next().unwrap_or("").is_empty()
}

#[tauri::command]
fn open_external(app: AppHandle, url: String) -> Result<(), String> {
    if !is_openable_url(&url) {
        return Err("only plain http(s) links and a support mailto are allowed".into());
    }
    use tauri_plugin_opener::OpenerExt;
    app.opener().open_url(&url, None::<&str>).map_err(|e| e.to_string())
}

// ---- the privacy guard's readers (all on the device) -----------------------------------

#[tauri::command]
async fn ocr_scan(image: String) -> Result<Vec<ocr::TextBox>, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let bytes = B64.decode(image.as_bytes()).map_err(|e| format!("bad image data: {e}"))?;
    tokio::task::spawn_blocking(move || ocr::recognize(&bytes)).await.map_err(|e| format!("ocr task failed: {e}"))?
}

#[tauri::command]
async fn pdf_text(image: String) -> Result<String, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let bytes = B64.decode(image.as_bytes()).map_err(|e| format!("bad pdf data: {e}"))?;
    tokio::task::spawn_blocking(move || pdf_extract::extract_text_from_mem(&bytes).map_err(|e| e.to_string()))
        .await
        .map_err(|e| format!("pdf task failed: {e}"))?
}

#[tauri::command]
async fn pdf_ocr(image: String) -> Result<String, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let bytes = B64.decode(image.as_bytes()).map_err(|e| format!("bad pdf data: {e}"))?;
    tokio::task::spawn_blocking(move || ocr::recognize_pdf(&bytes)).await.map_err(|e| format!("pdf-ocr task failed: {e}"))?
}

#[derive(serde::Serialize)]
struct PdfPageJson {
    png: String,
    boxes: Vec<ocr::TextBox>,
}

#[tauri::command]
async fn pdf_pages(image: String) -> Result<Vec<PdfPageJson>, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let bytes = B64.decode(image.as_bytes()).map_err(|e| format!("bad pdf data: {e}"))?;
    let pages = tokio::task::spawn_blocking(move || ocr::recognize_pdf_pages(&bytes)).await.map_err(|e| format!("pdf-pages task failed: {e}"))??;
    Ok(pages.into_iter().map(|p| PdfPageJson { png: B64.encode(&p.png), boxes: p.boxes }).collect())
}

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

#[tauri::command]
fn smart_available(app: AppHandle) -> bool {
    smart_paths(&app).map(|(m, t)| detect::available(&m, &t)).unwrap_or(false)
}

#[tauri::command]
async fn smart_detect(app: AppHandle, texts: Vec<String>, labels: Vec<String>) -> Result<Vec<Vec<detect::Entity>>, String> {
    let (model, tok) = smart_paths(&app).ok_or("smart-guard model not installed")?;
    tokio::task::spawn_blocking(move || detect::detect(&model, &tok, &texts, &labels, 0.5)).await.map_err(|e| format!("detect task failed: {e}"))?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // The Nym client's Sphinx crypto is stack-hungry: big stacks for every thread.
    std::env::set_var("RUST_MIN_STACK", "16777216");
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 * 1024 * 1024).build().expect("build tokio runtime");
    tauri::async_runtime::set(rt.handle().clone());
    std::mem::forget(rt);

    tauri::Builder::default()
        .manage(AppState::default())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            app.handle().plugin(
                tauri_plugin_log::Builder::default()
                    .level(if cfg!(debug_assertions) { log::LevelFilter::Info } else { log::LevelFilter::Warn })
                    .build(),
            )?;
            // Connect and attest right away, so the first question does not wait for it.
            let h = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = ensure_ready(&h).await {
                    log::warn!("[enclave] not ready: {e}");
                }
                let _ = directory(&h).await;
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            local_state, state,
            account_new, account_restore, account_delete, account_reveal, account_migrate_qr,
            phrase_check_start, phrase_check_verify, phrase_backup_get,
            chat, cancel_chat,
            plan_ladder, plan_checkout, plan_poll, plan_change, plan_forget,
            mixnet_route, list_entry_gateways, set_entry_gateway, set_entry_random, set_mixnet_perf, mixnet_ping,
            app_hidden, app_resumed, mixnet_heartbeat,
            support_send, support_list, support_diag,
            vault_list, vault_load, vault_save, vault_remove, vault_purge_webdata,
            save_file, save_image, open_external,
            ocr_scan, pdf_text, pdf_ocr, pdf_pages, smart_available, smart_detect,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_links_and_a_narrow_mailto_are_opened() {
        assert!(is_openable_url("https://checkout.stripe.com/c/pay/abc"));
        assert!(is_openable_url("mailto:support@tokumai.example?subject=Hi&body=x"));
        assert!(!is_openable_url("mailto:a@b.example?bcc=c@d.example"));
        assert!(!is_openable_url("mailto:a@b.example,c@d.example"));
        assert!(!is_openable_url("file:///etc/passwd"));
        assert!(!is_openable_url("https:///x"));
        assert!(!is_openable_url("https://x/a b"));
    }

    #[test]
    fn the_fingerprint_is_four_groups_of_four() {
        assert_eq!(fingerprint("abcdefghijklmnopqrst"), "abcd-efgh-ijkl-mnop");
        let _ = rand_hex(4);
    }
}
