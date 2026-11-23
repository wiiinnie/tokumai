//! The tokumai app, desktop side. The interface is the web page in `public/`; everything it
//! asks for comes through the commands here, and everything that talks to the enclave goes
//! through `tokumai_client` — the same core the phone apps use.
//!
//! What lives here and nowhere else: the recovery phrase (encrypted under a keychain key,
//! `profile`), the chat history (`vault`), and the platform bits (save dialogs, the
//! browser, OCR). There is no money on the device: the balance is on the account, inside
//! the enclave, and comes back from the phrase on any device.

mod detect;
/// The StoreKit 2 bridge, and only on the platform that has a StoreKit.
#[cfg(target_os = "ios")]
mod iap_ios;
/// The picker, the photo library, the share sheet and the link opener — all native.
#[cfg(target_os = "ios")]
mod ios_native;
mod keystore;
mod ocr;
mod owner;
mod palace;
mod profile;
mod target;
mod vault;
mod voice;

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
    /// The three-word check in flight: the positions Rust drew, and the wrong answers of
    /// late (`phrase_check_verify`).
    phrase_check: std::sync::Mutex<PhraseCheck>,
    /// The Memory Palace's own mixnet client, for one burst of requests and dropped after
    /// it: never the account's (`palace`).
    palace_net: Mutex<Option<Connection>>,
    /// One change of the palace file at a time (load → change → save).
    palace_local: std::sync::Mutex<()>,
    palace_syncing: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct PhraseCheck {
    /// The positions handed out by the last `phrase_check_start`, spent by one verify.
    challenge: Option<Vec<u32>>,
    /// When each wrong answer came, within `PHRASE_CHECK_WINDOW`.
    wrong: Vec<std::time::Instant>,
}

/// Wrong answers allowed before the check pauses, and for how long.
const PHRASE_CHECK_TRIES: usize = 5;
const PHRASE_CHECK_WINDOW: std::time::Duration = std::time::Duration::from_secs(15 * 60);

impl Default for AppState {
    fn default() -> Self {
        AppState {
            conn: Mutex::new(None),
            cancel: Notify::new(),
            route: Default::default(),
            directory: Mutex::new(None),
            models: Mutex::new(None),
            ladder: Mutex::new(None),
            phrase_check: std::sync::Mutex::new(PhraseCheck::default()),
            palace_net: Mutex::new(None),
            palace_local: std::sync::Mutex::new(()),
            palace_syncing: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

/// The mixnet connector, reporting its steps to the interface (the boot animation).
struct Reporting {
    inner: MixConnector,
    app: AppHandle,
}

impl Connector for Reporting {
    /// Passed on to the connector that owns the doors, or a door that went silent is
    /// tried again straight away.
    fn led_nowhere(&self) {
        self.inner.led_nowhere();
    }

    /// Beside the main one, for the minting of a note: nothing of it reaches the interface
    /// or the route display — it is not the connection the person is on.
    fn connect_aside(&self) -> BoxFuture<'_, Result<Box<dyn Transport>, String>> {
        self.inner.connect_aside()
    }

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
    inner.choose(p.enclave_door.clone());
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
    call_as(app, &account, op, body).await
}

/// The same, signed by a given account.
async fn call_as(app: &AppHandle, account: &Account, op: &str, body: Value) -> Result<Value, String> {
    call_as_how(app, account, op, body, false).await
}

/// The same, over a mixnet client of its own that is dropped afterwards — the blind notes
/// are minted under a key made for that one request and thrown away, so the minting names
/// nobody (`notes`), and over a transport the account's own calls never used, so the
/// enclave's end cannot pair the two by their sender (`Connection::call_aside`).
#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
async fn call_aside_as(app: &AppHandle, account: &Account, op: &str, body: Value) -> Result<Value, String> {
    call_as_how(app, account, op, body, true).await
}

async fn call_as_how(app: &AppHandle, account: &Account, op: &str, body: Value, aside: bool) -> Result<Value, String> {
    let st = app.state::<AppState>();
    let mut guard = st.conn.lock().await;
    if guard.is_none() {
        // A connection that cannot even be BUILT — no enclave address in this build, no
        // policy — used to fail here in silence, and the interface sat on "Connecting to
        // tokumai…" for as long as anyone was willing to watch. There is nothing to retry
        // and nothing to wait for, so it says so at once. (2026-09-24, on the phone: a
        // build for a device cannot read the developing Mac's dev-data.)
        *guard = Some(new_connection(app, &profile::load(&data_dir(app)?)).inspect_err(|e| {
            let _ = app.emit("mixnet-phase", json!({ "step": "failed", "detail": e, "srv": true }));
        })?);
    }
    let conn = guard.as_mut().ok_or("no connection")?;
    // A blind note minted or spent: the Memory Palace keeps its requests away from this
    // moment, so the two never go out in one burst (`palace::SPEND_GAP_MS`).
    if op.starts_with("note.") {
        palace::LAST_SPEND_MS.store(palace::now_ms(), std::sync::atomic::Ordering::Relaxed);
    }
    let called = if aside { conn.call_aside(account, op, &body).await } else { conn.call(account, op, &body).await };
    let answer = match called {
        Ok(a) => {
            // An answer is also the best proof the enclave is there: whatever the chip
            // said while it was silent, it is wrong now.
            let _ = app.emit("mixnet-phase", json!({ "step": "ready", "detail": "alive" }));
            a
        }
        Err(e) => {
            // The route can be perfectly good while the enclave behind it is not
            // answering — it is restarting, or its host is stopped. Saying "mixnet
            // connected" then is true and useless; this is the difference.
            if enclave_silent(&e) {
                let _ = app.emit("mixnet-phase", json!({ "step": "failed", "detail": e, "srv": true }));
            }
            return Err(e);
        }
    };
    if answer.get("kind").and_then(|k| k.as_str()) == Some("error") {
        let msg = answer.get("error").and_then(|e| e.as_str()).unwrap_or("the enclave refused").to_string();
        // Marked, so the interface offers credit rather than a retry.
        // Two markers travel with a refusal, and both change what the caller does next.
        // "final" says the App Store transaction can never be credited — refunded, expired,
        // a chain that does not check out — so it must be acknowledged to Apple instead of
        // being re-sent forever. Dropping it here is why an hour of refused sandbox
        // purchases piled up as unfinished transactions, and why `purchase()` then kept
        // handing back the oldest, long-expired one: "this subscription has ended"
        // (2026-09-24, from the phone).
        if answer["final"] == true {
            return Err(format!("final: {msg}"));
        }
        return Err(if answer["noCredit"] == true { format!("NO_CREDIT: {msg}") } else { msg });
    }
    Ok(answer)
}

/// Whether a failure is the enclave's silence rather than the route's. The transport says
/// so when nothing at all comes back for long enough that a slow model is ruled out
/// (`tokumai_client::mix`), and attesting through every door in turn ends the same way.
fn enclave_silent(e: &str) -> bool {
    e.contains("not answering") || e.contains("no answer from the enclave")
}

/// Connect and attest, with or without an account: the proof needs none, and the interface
/// wants the route up (and the enclave known) before anything is asked.
async fn ensure_ready(app: &AppHandle) -> Result<(), String> {
    let st = app.state::<AppState>();
    let mut guard = st.conn.lock().await;
    if guard.is_none() {
        *guard = Some(new_connection(app, &profile::load(&data_dir(app)?))?);
    }
    let ready = guard.as_mut().ok_or("no connection")?.ready().await;
    if let Err(e) = &ready {
        if enclave_silent(e) {
            let _ = app.emit("mixnet-phase", json!({ "step": "failed", "detail": e, "srv": true }));
        }
    }
    ready
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
/// The App Store plan's next renewal, as last read from the signed transactions
/// (`plan_renews_ms` in the profile). Kept here so `ui_plan` — called wherever a plan
/// summary passes to the interface — needs no profile and no await.
static PLAN_RENEWS_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn remember_renewal(p: &profile::Profile) {
    PLAN_RENEWS_MS.store(p.plan_renews_ms, std::sync::atomic::Ordering::Relaxed);
}

fn ui_plan(p: &Value) -> Value {
    if !p.is_object() {
        return Value::Null;
    }
    let mut p = p.clone();
    p["toku_per_month"] = p["tokuPerMonth"].clone();
    // A plan paid by note: the enclave knows only the MONTH the note bought, never the
    // day the plan was taken out, so its `periodEnd` is the month's end plus the grace in
    // which the next note may arrive — a deadline, not the renewal. The interface shows
    // the renewal, which this device knows from the App Store's own transaction. (App
    // Review 2026-10-11: "the date purchased does not reflect the reset date" — bought on
    // the 11th, the meter said "resets Nov 7".)
    if p["rail"] == "note" && p["active"] == true {
        let renews = PLAN_RENEWS_MS.load(std::sync::atomic::Ordering::Relaxed);
        if renews > 0 {
            p["periodEnd"] = json!(renews);
        }
    }
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
    remember_renewal(&p);
    Ok(json!({ "account": account_json(&p), "server": target::enclave_door().ok() }))
}

/// What the interface shows of a `balance` answer from the enclave (`{ balance, plan }`):
/// the one number, the allowance, the prepaid credit, the plan. Read the same way at
/// start and at every heartbeat, so the two can never disagree.
fn balance_fields(out: &mut Value, b: &Value) {
    let bal = &b["balance"];
    out["balance"] = bal["total"].clone();
    out["allowance"] = json!({ "left": bal["allowance"], "endsMs": bal["allowance_ends_ms"] });
    let prepaid: u64 = bal["prepaid"].as_array().map(|l| l.iter().filter_map(|x| x[0].as_u64()).sum()).unwrap_or(0);
    out["prepaid"] = json!(prepaid);
    out["plan"] = ui_plan(&b["plan"]);
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
        "server": target::enclave_door().ok(),
        "balance": 0,
        "models": [],
        // Blind notes minted and not yet redeemed: a month that will join at its moment.
        "notesWaiting": p.notes.len(),
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
            balance_fields(&mut out, &r["balance"]);
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
    // A new phrase, a new palace: the old one's cache would not open under it anyway.
    palace::forget(&dir);
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
    // The palace's copy here goes with the phrase; the phrase brings it back from the
    // other devices' uploads.
    palace::forget(&dir);
    Ok(json!({ "ok": true }))
}

async fn forget_account_caches(app: &AppHandle) {
    let st = app.state::<AppState>();
    *st.ladder.lock().await = None;
}

#[tauri::command]
async fn account_reveal(app: AppHandle) -> Result<Value, String> {
    // iOS: the words are drawn by UIKit behind Face ID and never cross into the webview.
    // Nothing is returned here but the fact that the native sheet was raised.
    #[cfg(target_os = "ios")]
    {
        let dir = data_dir(&app)?;
        if profile::load(&dir).mnemonic.is_none() {
            return Err("no account".into());
        }
        let handle = app.clone();
        app.run_on_main_thread(move || ios_native::secure::reveal_phrase(handle, dir, "Your recovery phrase")).map_err(|e| e.to_string())?;
        return Ok(json!({ "native": true }));
    }
    #[cfg(not(target_os = "ios"))]
    {
        let p = profile::load(&data_dir(&app)?);
        let m = p.mnemonic.ok_or("no account")?;
        // The person at the device, not whoever found it unlocked (owner.rs).
        owner::confirm(&app, "Show your recovery phrase").await?;
        Ok(json!({ "mnemonic": m }))
    }
}

/// The phrase and a QR code of it, to set up another device.
#[tauri::command]
async fn account_migrate_qr(app: AppHandle) -> Result<Value, String> {
    // iOS: no QR. A QR of the phrase IS the phrase — drawing it in the page would put the
    // words there in another costume. The native sheet shows them instead, and the other
    // device is set up by typing or by pasting from it.
    #[cfg(target_os = "ios")]
    {
        let dir = data_dir(&app)?;
        if profile::load(&dir).mnemonic.is_none() {
            return Err("no account".into());
        }
        let handle = app.clone();
        app.run_on_main_thread(move || ios_native::secure::reveal_phrase(handle, dir, "Move to another device")).map_err(|e| e.to_string())?;
        return Ok(json!({ "native": true }));
    }
    #[cfg(not(target_os = "ios"))]
    {
    let p = profile::load(&data_dir(&app)?);
    let m = p.mnemonic.ok_or("no account")?;
    owner::confirm(&app, "Show your recovery phrase to move it to another device").await?;
    let qr = qrcode::QrCode::new(m.as_bytes())
        .map(|c| c.render::<qrcode::render::svg::Color>().min_dimensions(200, 200).quiet_zone(true).build())
        .unwrap_or_default();
    Ok(json!({ "mnemonic": m, "qr": qr }))
    }
}

/// The three-word check, step one: three positions, fresh on every call. The words never
/// go to the interface — it gets numbers, sends back what was typed, and hears yes or no.
///
/// Rust keeps the positions it drew: a verify answers only for them, once, and five wrong
/// answers pause the check for a while. Without that the check was a word-by-word oracle
/// for whatever ran in the webview — ask about one position at a time, 2048 words each,
/// and the whole phrase falls out in seconds, on iOS past the native screen that exists so
/// the words never enter the webview at all (audit H6, 2026-10-05).
#[tauri::command]
fn phrase_check_start(app: AppHandle, state: State<'_, AppState>) -> Result<Value, String> {
    let p = profile::load(&data_dir(&app)?);
    let n = p.mnemonic.as_deref().map(|m| m.split_whitespace().count()).ok_or("no account")?;
    use rand::seq::SliceRandom;
    let mut all: Vec<u32> = (1..=n as u32).collect();
    all.shuffle(&mut rand::rngs::OsRng);
    let mut pick: Vec<u32> = all.into_iter().take(3).collect();
    pick.sort_unstable();
    state.phrase_check.lock().map_err(|_| "busy")?.challenge = Some(pick.clone());
    Ok(json!({ "positions": pick, "total": n, "verified": p.phrase_verified }))
}

/// Step two. Case and surrounding space are forgiven; which word was wrong is not said.
#[tauri::command]
fn phrase_check_verify(app: AppHandle, state: State<'_, AppState>, positions: Vec<u32>, words: Vec<String>) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let mut p = profile::load(&dir);
    let m = p.mnemonic.clone().ok_or("no account")?;
    let all: Vec<&str> = m.split_whitespace().collect();
    let mut check = state.phrase_check.lock().map_err(|_| "busy")?;
    check.wrong.retain(|at| at.elapsed() < PHRASE_CHECK_WINDOW);
    if check.wrong.len() >= PHRASE_CHECK_TRIES {
        return Err("too many wrong answers — the check pauses for a quarter of an hour".into());
    }
    // One challenge, one answer: the positions must be the ones drawn, and are spent now,
    // right or wrong — a wrong answer draws three new ones (the interface asks).
    let Some(drawn) = check.challenge.take() else { return Err("the check was not started — three new words".into()) };
    if positions != drawn || words.len() != 3 {
        return Err("those are not the words asked for — three new words".into());
    }
    // Every word is compared, whatever the first said: no early answer to time.
    let ok = positions.iter().zip(words.iter()).fold(true, |ok, (pos, typed)| {
        let idx = (*pos as usize).wrapping_sub(1);
        all.get(idx).is_some_and(|real| real.eq_ignore_ascii_case(typed.trim())) & ok
    });
    if ok {
        check.wrong.clear();
    } else {
        check.wrong.push(std::time::Instant::now());
    }
    drop(check);
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
    memory: Option<Value>,
) -> Result<Value, String> {
    let mut body = json!({
        "model": model, "messages": messages, "maxTokens": max_tokens, "live": live.unwrap_or(false),
        "thinkingBudget": thinking_budget, "imageSize": image_size, "lossless": lossless.unwrap_or(false),
    });
    // The Memory Palace's index (titles and descriptions, anonymised by the interface):
    // the model may then ask for a file with `read_memory` (the answer's `toolCalls`).
    let index_len = memory.as_ref().and_then(|m| m["index"].as_array().map(Vec::len)).unwrap_or(0);
    if let Some(m) = memory.filter(|m| m["index"].as_array().is_some_and(|a| !a.is_empty())) {
        body["memory"] = m;
    }
    // Which question could read the palace, and whether it did: the model, search on or
    // off, how many files the index named, and the turn's tool messages so far. No content.
    let tool_turns = body["messages"].as_array().map(|a| a.iter().filter(|m| m["role"] == "tool").count()).unwrap_or(0);
    log::info!("[palace] chat {model}: live {}, index {index_len}, memory read so far {tool_turns}", body["live"]);
    let _ = app.emit("chat-sent", ());
    let st = app.state::<AppState>();
    let mut answer = tokio::select! {
        a = Box::pin(call(&app, "chat", body)) => a?,
        _ = st.cancel.notified() => return Err("stopped".into()),
    };
    if index_len > 0 {
        log::info!("[palace] the model asked to read {} file(s)", answer["toolCalls"].as_array().map(Vec::len).unwrap_or(0));
    }
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

// ---- plans bought through the App Store -------------------------------------------------
//
// Three commands, and the shape of all three is the same: ask StoreKit, hand Apple's signed
// transaction to the enclave, and acknowledge it to Apple only once the enclave says the
// plan is granted. Nothing on the device decides what was bought — the device cannot be
// trusted about money, and the JWS is checked against Apple's pinned root inside the
// enclave (`apple.rs`).
//
// Which product ids exist is the enclave's word too: they are compiled into its image and
// come back with `plans`, so an app that was built against an older list cannot invent one.

/// The product ids the enclave sells, from its own `plans` answer.
#[cfg(target_os = "ios")]
async fn appstore_ids(app: &AppHandle) -> Result<(Vec<String>, Vec<String>), String> {
    let l = call(app, "plans", json!({})).await?;
    let ids = |k: &str| -> Vec<String> {
        l["appStore"][k].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default()
    };
    Ok((ids("plans"), ids("credit")))
}

/// What is on sale here: the enclave's ids, priced by the App Store in the reader's own
/// currency. `missing` is not noise — it is the only way to tell "Apple has not published
/// this one yet" from "we asked for the wrong string", and App Store Connect says neither.
#[cfg(target_os = "ios")]
#[tauri::command]
async fn iap_offers(app: AppHandle) -> Result<Value, String> {
    let (plans, credit) = appstore_ids(&app).await?;
    let mut all = plans.clone();
    all.extend(credit.iter().cloned());
    if all.is_empty() {
        return Ok(json!({ "plans": [], "credit": [], "missing": [] }));
    }
    let answer = iap_ios::products(&all).await?;
    let of = |ids: &[String]| -> Value {
        let rows: Vec<Value> = answer["products"]
            .as_array()
            .map(|ps| ps.iter().filter(|p| ids.iter().any(|i| p["id"] == i.as_str())).cloned().collect())
            .unwrap_or_default();
        json!(rows)
    };
    Ok(json!({ "plans": of(&plans), "credit": of(&credit), "missing": answer["missing"] }))
}

/// Buy one product: Apple's sheet, then the enclave, then — and only then — the
/// acknowledgement to Apple. A transaction left unfinished is re-sent by `iap_sync` on the
/// next launch, so a crash between paying and crediting costs nobody anything.
#[cfg(target_os = "ios")]
#[tauri::command]
async fn iap_buy(app: AppHandle, product_id: String) -> Result<Value, String> {
    let (plans, credit) = appstore_ids(&app).await?;
    if !plans.contains(&product_id) && !credit.contains(&product_id) {
        return Err("that product is not sold here".into());
    }
    // StoreKit (seen in the sandbox, 2026-09-24 and 2026-11-23) answers a purchase with an
    // earlier transaction of the same product that was never finished — long ended — in
    // place of a new one. Such a transaction is finished and the purchase asked once more.
    let mut tries = 0;
    let jws = loop {
        tries += 1;
        let bought = iap_ios::purchase(&product_id).await?;
        match bought["status"].as_str().unwrap_or("") {
            "cancelled" => return Ok(json!({ "status": "cancelled" })),
            // "Ask to Buy": a parent has to approve, and the transaction turns up later.
            "pending" => return Ok(json!({ "status": "pending" })),
            _ => {}
        }
        let jws = bought["jws"].as_str().unwrap_or("").to_string();
        if jws.is_empty() {
            return Err("the App Store returned no signed transaction".into());
        }
        let ended = paid_from_jws(&jws).is_some_and(|p| p.until_ms <= tokumai_proto::now_ms());
        if !ended {
            break jws;
        }
        let tx = bought["transactionId"].as_str().unwrap_or("");
        let c = jws_claims(&jws).unwrap_or(Value::Null);
        log::warn!(
            "[notes] the App Store answered with an ended transaction {tx}; finished, asking again (product {}, original {}, purchased {}, expires {}, signed {}, env {}, now {})",
            c["productId"], c["originalTransactionId"], c["purchaseDate"], c["expiresDate"], c["signedDate"], c["environment"], tokumai_proto::now_ms()
        );
        if !tx.is_empty() {
            let _ = iap_ios::finish(tx).await;
        }
        if tries >= 2 {
            return Err("The App Store handed back an earlier purchase that has ended. It is cleared now: please try once more.".into());
        }
    };
    // The purchase becomes notes, and the notes become the month — the same path a launch
    // takes, so a crash between paying and minting costs nobody anything: Apple keeps
    // offering the transaction until it is finished, and the sync finishes it once minted.
    let r = notes_sync(&app).await?;
    // Judged by THIS purchase's own payment, not by whatever else Apple still holds: an
    // old, long-expired sandbox transaction refused on the way must not turn a good
    // purchase into "this subscription has ended", nor hide that this one bought nothing.
    let rail = paid_from_jws(&jws).map(|p| p.rail);
    let mine: Vec<&Value> = r["outcomes"].as_array().map(|o| o.iter().filter(|x| rail.as_deref().is_some_and(|rl| x["rail"] == rl)).collect()).unwrap_or_default();
    let active = r["plan"]["active"] == true || call(&app, "balance", json!({})).await.map(|b| b["plan"]["active"] == true).unwrap_or(false);
    if active || mine.iter().any(|x| x["result"] == "minted") {
        return Ok(json!({ "status": "ok", "plan": r["plan"] }));
    }
    // The month of this payment went to another phrase: the note was made before (on this
    // device, for the account that was here then) or the enclave has one for it already.
    let this_month = core_notes::epoch_of_ms(tokumai_proto::now_ms());
    let elsewhere = mine.iter().any(|x| x["epoch"] == this_month) && mine.iter().filter(|x| x["epoch"] == this_month).any(|x| {
        x["result"] == "done" || x["error"].as_str().is_some_and(|e| e.contains("already minted") || e.contains("already pays a plan"))
    });
    if elsewhere {
        return Err("This subscription has already paid for this month on another account. From next month on, it pays for this one.".into());
    }
    if let Some(e) = mine.iter().find_map(|x| x["error"].as_str()) {
        return Err(e.trim_start_matches("final: ").to_string());
    }
    Err("Apple took the payment, but no month has been credited yet. It is tried again the next time the app starts.".into())
}

/// Every transaction Apple still holds for this Apple ID, handed to the enclave.
///
/// Two lists, for two different silences. `currentEntitlements` carries the live
/// subscription, which matters because a RENEWAL never reaches the app as a purchase —
/// StoreKit charges the card in the background and the app only learns of it by asking.
/// `unfinished` carries anything paid but not yet acknowledged, which is what a crash
/// between paying and crediting leaves behind.
///
/// `restore` is passed through to the enclave, and only a deliberate "Restore purchases"
/// may set it: it is the one flag that moves a plan from another account to this one.
#[cfg(target_os = "ios")]
#[tauri::command]
async fn iap_sync(app: AppHandle, restore: bool) -> Result<Value, String> {
    // "Restore" used to be the one call allowed to move a plan between accounts. With
    // notes there is nothing to move: the notes of a phrase are the phrase's, on any
    // device, and the enclave signs them again for a restored phone.
    let _ = restore;
    notes_sync(&app).await
}

#[cfg(not(target_os = "ios"))]
#[tauri::command]
async fn iap_offers(_app: AppHandle) -> Result<Value, String> {
    Err("App Store purchases exist only on iOS".into())
}

#[cfg(not(target_os = "ios"))]
#[tauri::command]
async fn iap_buy(_app: AppHandle, _product_id: String) -> Result<Value, String> {
    Err("App Store purchases exist only on iOS".into())
}

#[cfg(not(target_os = "ios"))]
#[tauri::command]
async fn iap_sync(_app: AppHandle, _restore: bool) -> Result<Value, String> {
    Ok(json!({ "sent": 0, "plan": Value::Null, "trouble": Value::Null }))
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
    let exit = target::enclave_door().ok().and_then(|a| gateways::gateway_of(&a).map(str::to_string));
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
    let address = target::enclave_door().unwrap_or_default();
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
        gateways::pick(&d, &EntryChoice::Chosen(chosen.clone()), &target::enclave_door().unwrap_or_default())?;
    }
    let dir = data_dir(&app)?;
    let mut p = profile::load(&dir);
    p.entry_gateway = id.clone();
    profile::save(&dir, &p)?;
    reset_connection(&app).await;
    Ok(json!({ "entry_gateway": id, "entry_random": id.is_none() }))
}

/// The enclave's front doors and which one is in use — one per gateway we run, all the
/// same enclave behind them (`MixConnector`).
#[tauri::command]
fn enclave_doors(app: AppHandle) -> Value {
    let chosen = profile::load(&data_dir(&app).unwrap_or_default()).enclave_door;
    let doors: Vec<Value> = target::enclave_address()
        .unwrap_or_default()
        .split(',')
        .filter(|a| !a.trim().is_empty())
        .map(|address| {
            let gateway = gateways::gateway_of(address).unwrap_or_default().to_string();
            json!({ "address": address, "gateway": gateway, "name": gateways::OPERATOR_NAMES.iter().find(|(g, _)| *g == gateway).map(|(_, n)| *n).unwrap_or("") })
        })
        .collect();
    json!({ "doors": doors, "chosen": chosen })
}

/// Pick a door (`None` = whichever answers). The connection is rebuilt, as it is when the
/// entry gateway changes.
#[tauri::command]
async fn set_enclave_door(app: AppHandle, address: Option<String>) -> Result<Value, String> {
    let address = address.map(|a| a.trim().to_string()).filter(|a| !a.is_empty());
    let all = target::enclave_address().unwrap_or_default();
    if let Some(a) = &address {
        if !all.split(',').any(|d| d.trim() == a) {
            return Err("that is not one of this enclave's doors".into());
        }
    }
    let dir = data_dir(&app)?;
    let mut p = profile::load(&dir);
    p.enclave_door = address.clone();
    profile::save(&dir, &p)?;
    reset_connection(&app).await;
    Ok(json!({ "chosen": address }))
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

/// The once-a-minute beat from the interface, while the app is open and idle. It used to
/// answer "alive" without asking anybody, which kept nothing alive and told nothing: a
/// route that had died was found by the next question, and a balance the enclave had
/// already changed — a plan's month ending, a sandbox plan lapsing, credit spent from
/// another device — sat in the menu until somebody tapped "Check balance" (2026-09-28).
/// One small round trip, the balance, and the menu shows what the enclave says.
#[tauri::command]
async fn mixnet_heartbeat(app: AppHandle) -> Result<Value, String> {
    let t0 = std::time::Instant::now();
    let mut r = call(&app, "balance", json!({})).await?;
    // A note whose moment has come, or an account with nothing left to chat on: redeemed
    // here, on the beat, so nobody waits for a launch.
    if !profile::load(&data_dir(&app)?).notes.is_empty() {
        if let Ok(Some(_)) = redeem_due(&app, !has_room(&r)).await {
            r = call(&app, "balance", json!({})).await?;
        }
    }
    // A plan that was paid for and is gone from the book: the note that bought it is
    // presented again (a restored book has no record of it being spent).
    if let Ok(true) = reclaim_plan(&app, &r).await {
        r = call(&app, "balance", json!({})).await?;
    }
    let mut out = json!({ "action": "alive", "ms": t0.elapsed().as_millis() as u64 });
    balance_fields(&mut out, &r);
    Ok(out)
}

// ---- blind notes ---------------------------------------------------------------------
//
// A payment no longer credits the account. It buys a note for each paid month — signed
// blind by the enclave under a key made for one request and thrown away — and the account
// redeems the note later for that month's allowance. The enclave cannot connect the two;
// the book records no payment → account row (docs/blind-tokens.md). What this side does:
//
// 1. read which months the App Store says are paid (the signed transaction, read without
//    verifying — the enclave verifies; the app only needs to know what to ask for);
// 2. mint a note for each open month not yet held, with the nonce and the blinding derived
//    from the account's seed, so a phone restored from the phrase makes the same note and
//    the enclave signs it again instead of refusing a second one;
// 3. redeem by the one rule: at once when the account has nothing to chat on (nobody ever
//    runs dry), else at a random moment inside the grace, so a renewal hides among the
//    month's other renewals. The first month of a new subscriber is redeemed at once, and
//    is the one redemption exposed to timing; the note says so, and so does the pitch.

use tokumai_core::notes as core_notes;

/// A paid period as the App Store states it.
#[cfg(target_os = "ios")]
struct Paid {
    rail: String,
    tier: u8,
    yearly: bool,
    start_ms: u64,
    until_ms: u64,
    proof: Value,
}

/// The claims of a signed App Store transaction, read without verifying.
#[cfg(target_os = "ios")]
fn jws_claims(jws: &str) -> Option<Value> {
    use base64::Engine as _;
    let mid = jws.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(mid).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(target_os = "ios")]
fn paid_from_jws(jws: &str) -> Option<Paid> {
    use tokumai_core::subscription::TIERS;
    let c = jws_claims(jws)?;
    if c["type"].as_str()? != "Auto-Renewable Subscription" || c.get("revocationDate").and_then(|r| r.as_u64()).is_some() {
        return None;
    }
    let rest = c["productId"].as_str()?.strip_prefix("com.tokumai.app.plan.")?;
    let mut parts = rest.split('.');
    let euros: u64 = parts.next()?.parse().ok()?;
    let yearly = parts.next() == Some("year");
    let tier = TIERS.iter().position(|(_, cents)| *cents == euros * 100)? as u8;
    Some(Paid {
        rail: format!("iap:{}", c["originalTransactionId"].as_str()?),
        tier,
        yearly,
        start_ms: c["purchaseDate"].as_u64()?,
        until_ms: c["expiresDate"].as_u64()?,
        proof: json!({ "kind": "apple", "jws": jws }),
    })
}

/// When this paid period's allowance next resets: the plan's months are counted from the
/// day it was bought (core `slice_at`), so a monthly plan taken out on the 11th resets on
/// the 11th, and a yearly one on the 11th of every month. Counted from the purchase date
/// rather than read from `expiresDate`, which Apple's sandbox sets minutes ahead: the
/// product says a month, and a month is what the meter shows. None once the period is
/// over.
#[cfg(target_os = "ios")]
fn next_reset(paid: &Paid, now: u64) -> Option<u64> {
    use tokumai_core::subscription::add_months_ms;
    let months = if paid.yearly { 12 } else { 1 };
    (1..=months).map(|k| add_months_ms(paid.start_ms, k)).find(|&t| t > now)
}

/// The enclave's published key for a month, from the attested session.
async fn note_key(app: &AppHandle, epoch: u16) -> Result<Vec<u8>, String> {
    ensure_ready(app).await?;
    let st = app.state::<AppState>();
    let guard = st.conn.lock().await;
    let session = guard.as_ref().and_then(|c| c.session()).ok_or("not attested")?;
    session.note_key(epoch).map(|k| k.to_vec()).ok_or_else(|| format!("the enclave published no key for month {epoch}"))
}

/// Mint one note: blind it with the seed, have it signed under a throwaway key, unblind.
#[cfg(target_os = "ios")]
async fn mint_note(app: &AppHandle, paid: &Paid, epoch: u16, redeem_after_ms: u64) -> Result<profile::WalletNote, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let account = current_account(app)?;
    let seed = account.note_seed();
    let pk = core_notes::public_from_spki(&note_key(app, epoch).await?)?;
    let note = core_notes::note_for(&seed, paid.tier, epoch);
    let (blinded, secret) = core_notes::blind(&pk, &note, &seed)?;
    let throwaway = account::create_account();
    let r = call_aside_as(app, &throwaway, "note.mint", json!({ "proof": paid.proof, "epoch": epoch, "blinded": B64.encode(&blinded) })).await?;
    let blind_sig = r["sig"].as_str().and_then(|x| B64.decode(x).ok()).ok_or("the enclave sent no signature")?;
    let sig = core_notes::finalize(&pk, &blind_sig, &secret, &note)?;
    Ok(profile::WalletNote {
        rail: paid.rail.clone(),
        epoch,
        tier: paid.tier,
        note: B64.encode(note.to_bytes()),
        sig: B64.encode(sig),
        minted_ms: tokumai_proto::now_ms(),
        redeem_after_ms,
    })
}

/// A moment inside the grace, drawn from the seed and the month so it is the same on every
/// launch: no dice to be reproduced, and no clustering at app start.
fn jitter_ms(seed: &[u8; 32], epoch: u16, span_ms: u64) -> u64 {
    if span_ms == 0 {
        return 0;
    }
    let h = core_notes::derive(seed, "jitter", epoch);
    u64::from_be_bytes(h[..8].try_into().expect("8 bytes")) % span_ms
}

/// Whether the account can still chat on what it has: an allowance that is active and not
/// empty. Read from a `balance` answer.
fn has_room(balance: &Value) -> bool {
    let b = &balance["balance"];
    b["total"].as_u64().unwrap_or(0) > 0 && balance["plan"]["active"] == true
}

/// Redeem the notes that are due — all of them when the account has nothing to chat on.
/// Returns the plan summary of the last redemption, if any.
async fn redeem_due(app: &AppHandle, need_now: bool) -> Result<Option<Value>, String> {
    let dir = data_dir(app)?;
    let mut p = profile::load(&dir);
    let now = tokumai_proto::now_ms();
    let due: Vec<profile::WalletNote> = p.notes.iter().filter(|n| need_now || n.redeem_after_ms <= now).cloned().collect();
    if due.is_empty() {
        return Ok(None);
    }
    let list: Vec<Value> = due.iter().map(|n| json!({ "note": n.note, "sig": n.sig })).collect();
    let r = match call(app, "note.redeem", json!({ "notes": list })).await {
        Ok(r) => r,
        Err(e) if e.starts_with("final: ") => {
            // Refused for good — a note of another version, a month no longer open. Kept
            // nowhere: it would be refused the same way every launch.
            log::warn!("[notes] dropped {} note(s): {e}", due.len());
            p.notes.retain(|n| !due.iter().any(|d| d.note == n.note));
            profile::save(&dir, &p)?;
            return Err(e);
        }
        Err(e) => return Err(e),
    };
    for n in &due {
        p.notes_done.push(format!("{}:{}", n.rail, n.epoch));
        // Kept, spent, for the month it bought (see `reclaim_plan`).
        if !p.spent.iter().any(|s| s.note == n.note) {
            p.spent.push(n.clone());
        }
    }
    p.notes.retain(|n| !due.iter().any(|d| d.note == n.note));
    p.spent.retain(|n| core_notes::epoch_end_ms(n.epoch) + core_notes::GRACE_MS > now);
    profile::save(&dir, &p)?;
    log::info!("[notes] redeemed {} note(s), {} already spent", r["granted"], r["again"]);
    Ok(Some(ui_plan(&r["plan"])))
}

/// When the last redemption's month is still running but the enclave reports no plan,
/// the book has lost the redemption — a book restored from a backup, for one. The spent
/// note is the proof of the payment; presented again it is honoured once if the book has
/// no record of it, and answered with "again" if it has. Nothing can be gained by it: a
/// plan that is there is not granted twice. Tried at most every ten minutes.
async fn reclaim_plan(app: &AppHandle, balance: &Value) -> Result<bool, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    if balance["plan"]["active"] == true {
        return Ok(false);
    }
    let dir = data_dir(app)?;
    let p = profile::load(&dir);
    let now = tokumai_proto::now_ms();
    let (from, to) = core_notes::window(now);
    let live: Vec<&profile::WalletNote> = p
        .spent
        .iter()
        .filter(|n| n.epoch >= from && n.epoch <= to && core_notes::epoch_end_ms(n.epoch) + core_notes::GRACE_MS > now)
        .collect();
    if live.is_empty() || now.saturating_sub(LAST.load(Ordering::Relaxed)) < 10 * 60 * 1000 {
        return Ok(false);
    }
    LAST.store(now, Ordering::Relaxed);
    let list: Vec<Value> = live.iter().map(|n| json!({ "note": n.note, "sig": n.sig })).collect();
    let r = call(app, "note.redeem", json!({ "notes": list })).await?;
    let granted = r["granted"].as_u64().unwrap_or(0);
    if granted > 0 {
        log::warn!("[notes] the plan was gone from the book; {granted} note(s) presented again brought it back");
    }
    Ok(granted > 0)
}

/// Everything the App Store says is paid, as notes: minted once per month, redeemed by the
/// rule. The one call behind a purchase, a launch and "restore purchases" alike.
#[cfg(target_os = "ios")]
async fn notes_sync(app: &AppHandle) -> Result<Value, String> {
    use tokumai_core::subscription::TIERS;
    let _ = TIERS;
    let dir = data_dir(app)?;
    let account = current_account(app)?;
    let seed = account.note_seed();
    let now = tokumai_proto::now_ms();
    let (from, to) = core_notes::window(now);
    // What the account has to chat on decides when a new note is redeemed.
    let balance = call(app, "balance", json!({})).await.unwrap_or(Value::Null);
    let room = has_room(&balance);
    let period_end = balance["plan"]["periodEnd"].as_u64().unwrap_or(0);
    let mut all = iap_ios::entitlements().await.unwrap_or_default();
    all.extend(iap_ios::unfinished().await.unwrap_or_default());
    // The next renewal, for the meter: the latest one any live transaction promises.
    // Nothing promised (no plan, or StoreKit did not answer) leaves the enclave's date.
    let renews = all
        .iter()
        .filter_map(|t| paid_from_jws(t["jws"].as_str().unwrap_or("")))
        .filter_map(|paid| next_reset(&paid, now))
        .max()
        .unwrap_or(0);
    {
        let mut p = profile::load(&dir);
        if p.plan_renews_ms != renews {
            p.plan_renews_ms = renews;
            profile::save(&dir, &p)?;
        }
        remember_renewal(&p);
    }
    let mut minted = 0u32;
    let mut trouble: Option<String> = None;
    let mut seen: Vec<String> = Vec::new();
    // What became of each payment and month, for `iap_buy` and the log.
    let mut outcomes: Vec<Value> = Vec::new();
    for t in all {
        let jws = t["jws"].as_str().unwrap_or("").to_string();
        let tx = t["transactionId"].as_str().unwrap_or("").to_string();
        let Some(paid) = paid_from_jws(&jws) else { continue };
        // Every month it paid is behind the window: nothing left to make of it, so it is
        // finished, or StoreKit keeps handing it back.
        if !tx.is_empty() && core_notes::epochs_covered(paid.start_ms, paid.until_ms, paid.yearly).iter().all(|e| *e < from) {
            log::info!("[notes] {}: its months are over; finished", paid.rail);
            let _ = iap_ios::finish(&tx).await;
            continue;
        }
        for epoch in core_notes::epochs_covered(paid.start_ms, paid.until_ms, paid.yearly) {
            if epoch < from || epoch > to {
                continue;
            }
            let key = format!("{}:{}", paid.rail, epoch);
            let p = profile::load(&dir);
            if seen.contains(&key) {
                // A second transaction for a month already in hand this round — in the
                // sandbox every five-minute renewal is one, dozens of them. Once ended, it
                // is finished: StoreKit hands an open one back in place of a new purchase.
                if !tx.is_empty() && paid.until_ms <= now {
                    let _ = iap_ios::finish(&tx).await;
                }
                continue;
            }
            if p.notes_done.contains(&key) || p.notes.iter().any(|n| n.rail == paid.rail && n.epoch == epoch) {
                log::info!("[notes] {key}: already made on this device");
                outcomes.push(json!({ "rail": paid.rail, "epoch": epoch, "result": "done" }));
                // Finished with Apple too: a transaction left open is what StoreKit hands
                // back to the next purchase in place of a new one.
                if !tx.is_empty() {
                    let _ = iap_ios::finish(&tx).await;
                }
                continue;
            }
            seen.push(key.clone());
            // Nothing to chat on: redeem the moment the note exists. Otherwise a random
            // moment inside the grace — never past the old allowance's own end.
            let redeem_after = if room {
                let span = core_notes::GRACE_MS.min(period_end.saturating_sub(now));
                now + jitter_ms(&seed, epoch, span)
            } else {
                now
            };
            match mint_note(app, &paid, epoch, redeem_after).await {
                Ok(note) => {
                    log::info!("[notes] {key}: minted");
                    outcomes.push(json!({ "rail": paid.rail, "epoch": epoch, "result": "minted" }));
                    let mut p = profile::load(&dir);
                    p.notes.push(note);
                    profile::save(&dir, &p)?;
                    minted += 1;
                    if !tx.is_empty() {
                        let _ = iap_ios::finish(&tx).await;
                    }
                }
                Err(e) => {
                    log::warn!("[notes] {key}: {e}");
                    outcomes.push(json!({ "rail": paid.rail, "epoch": epoch, "result": "error", "error": e }));
                    if e.starts_with("final: ") {
                        // Never mintable: say so once in the done list, and acknowledge
                        // the transaction so Apple stops offering it.
                        let mut p = profile::load(&dir);
                        p.notes_done.push(key);
                        profile::save(&dir, &p)?;
                        if !tx.is_empty() {
                            let _ = iap_ios::finish(&tx).await;
                        }
                    }
                    trouble = Some(e);
                }
            }
        }
    }
    let mut plan = redeem_due(app, !room).await.ok().flatten().unwrap_or(Value::Null);
    if plan.is_null() {
        if let Ok(true) = reclaim_plan(app, &balance).await {
            plan = call(app, "balance", json!({})).await.map(|b| ui_plan(&b["plan"])).unwrap_or(Value::Null);
        }
    }
    Ok(json!({ "sent": minted, "plan": plan, "trouble": trouble, "outcomes": outcomes }))
}

// ---- the Memory Palace ----------------------------------------------------------------
//
// The copy here, the queue and the sync live in `palace`; this is the part that needs the
// app: the phrase, the mixnet, the interface's events, the background loop.

/// The palace's keys for the phrase on this device. Derived once per phrase (the BIP39
/// seed is 2048 rounds of PBKDF2) and kept in memory only, under a hash of the phrase so
/// another account's phrase never gets them.
fn palace_keys(app: &AppHandle) -> Result<std::sync::Arc<tokumai_core::palace::PalaceKeys>, String> {
    static KEPT: std::sync::Mutex<Option<([u8; 32], std::sync::Arc<tokumai_core::palace::PalaceKeys>)>> = std::sync::Mutex::new(None);
    let p = profile::load(&data_dir(app)?);
    let phrase = p.mnemonic.as_deref().ok_or("no account on this device yet")?;
    let tag = tokumai_core::account::sha256(&[b"tokumai/palace/keys-cache", phrase.as_bytes()]);
    let mut kept = KEPT.lock().map_err(|_| "the palace keys are in an unknown state")?;
    if let Some((t, k)) = kept.as_ref() {
        if *t == tag {
            return Ok(k.clone());
        }
    }
    let k = std::sync::Arc::new(tokumai_core::palace::PalaceKeys::from_mnemonic(phrase)?);
    *kept = Some((tag, k.clone()));
    Ok(k)
}

/// Load, change, save — one at a time.
fn with_palace<T>(app: &AppHandle, f: impl FnOnce(&mut palace::Local) -> T) -> Result<T, String> {
    let keys = palace_keys(app)?;
    let dir = data_dir(app)?;
    let st = app.state::<AppState>();
    let _one = st.palace_local.lock().map_err(|_| "the palace is in an unknown state")?;
    let mut l = palace::load(&dir, &keys);
    let out = f(&mut l);
    palace::save(&dir, &keys, &l)?;
    Ok(out)
}

fn palace_status_json(app: &AppHandle) -> Value {
    let Ok(dir) = data_dir(app) else { return json!({ "state": "locked" }) };
    let cfg = palace::config(&dir);
    let keys = palace_keys(app).ok();
    let l = keys.as_ref().map(|k| palace::load(&dir, k)).unwrap_or_default();
    let syncing = app.state::<AppState>().palace_syncing.load(std::sync::atomic::Ordering::Relaxed);
    json!({
        "state": palace::state(&cfg, keys.is_some(), syncing, &l),
        "enabled": cfg.enabled, "localOnly": cfg.local_only, "wipePending": cfg.wipe_pending,
        "files": l.manifest.files.iter().filter(|f| !f.deleted).count(),
        "pending": l.dirty.len() + l.gone.len(),
        "dueMs": l.due_ms, "lastSync": l.last_sync_ms, "lastError": l.last_error, "attempts": l.attempts,
    })
}

fn palace_emit(app: &AppHandle, extra: Value) {
    let mut s = palace_status_json(app);
    if let (Some(o), Some(e)) = (s.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            o.insert(k.clone(), v.clone());
        }
    }
    let _ = app.emit("palace-status", s);
}

/// A connection of the palace's own: no steps for the interface, no progress — it is not
/// the route the person is on, and the account's client never carries its requests.
fn palace_connection(app: &AppHandle) -> Result<Connection, String> {
    let p = profile::load(&data_dir(app)?);
    let address = target::enclave_address()?;
    let mut inner = MixConnector::new(&address, entry_choice(&p));
    inner.choose(p.enclave_door.clone());
    inner.traffic = p.traffic.map(|(cover_ms, mix_ms, send_ms, continuous)| tokumai_client::mix::Traffic { cover_ms, mix_ms, send_ms, continuous });
    Ok(Connection::new(Box::new(inner), target::policy()?))
}

/// Errors that say the request never got an answer (the route), as opposed to the
/// enclave refusing it.
fn is_route_error(e: &str) -> bool {
    let e = e.to_lowercase();
    ["mixnet", "no reply", "not answering", "no answer", "timed out", "timeout", "gateway", "connect", "stream ended", "not connected", "directory"].iter().any(|w| e.contains(w))
}

/// One palace request over the palace's connection, in a throwaway envelope.
async fn palace_request(conn: &mut Connection, op: &str, body: &Value) -> Result<Value, (String, bool)> {
    // Never within the gap of a note being minted or spent; a burst that runs into one
    // stops here and is tried again later.
    let since = palace::now_ms().saturating_sub(palace::LAST_SPEND_MS.load(std::sync::atomic::Ordering::Relaxed));
    if since < palace::SPEND_GAP_MS {
        return Err(("waiting: a payment note was just used".into(), true));
    }
    let throwaway = account::create_account();
    match conn.call(&throwaway, op, body).await {
        Ok(a) if a["kind"] == "error" => Err((a["error"].as_str().unwrap_or("the enclave refused").to_string(), false)),
        Ok(a) => Ok(a),
        Err(e) => {
            let route = is_route_error(&e);
            Err((e, route))
        }
    }
}

/// One burst: list, fetch what changed, merge; then, if asked, upload the queue. Returns
/// how many facts arrived from other devices.
async fn palace_burst(app: &AppHandle, upload: bool) -> Result<usize, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    use tokumai_core::palace::MANIFEST_ID;
    let dir = data_dir(app)?;
    let cfg = palace::config(&dir);
    if cfg.enabled != Some(true) || cfg.local_only || cfg.wipe_pending {
        return Ok(0);
    }
    let keys = palace_keys(app)?;
    let st = app.state::<AppState>();
    let mut net = st.palace_net.lock().await;
    st.palace_syncing.store(true, std::sync::atomic::Ordering::Relaxed);
    palace_emit(app, json!({ "phase": "start" }));
    let result: Result<usize, (String, bool)> = async {
        if net.is_none() {
            *net = Some(palace_connection(app).map_err(|e| (e, true))?);
        }
        let conn = net.as_mut().ok_or(("no connection".to_string(), true))?;
        // 1. What the enclave has.
        let (op, body) = palace::body(&keys, "list", "", &[], 0, b"");
        let list = palace_request(conn, &op, &body).await?;
        let remote: std::collections::BTreeMap<String, u64> = list["blobs"].as_array().map(|a| a.iter().filter_map(|b| Some((b["blobId"].as_str()?.to_string(), b["version"].as_u64()?))).collect()).unwrap_or_default();
        let mut changes = 0usize;
        // 2. The index first (it names the files), then every file that changed.
        for round in 0..2 {
            let (want, ids): (Vec<String>, std::collections::BTreeMap<String, String>) = with_palace(app, |l| {
                let ids: std::collections::BTreeMap<String, String> = palace::known_ids(l).into_iter().map(|id| (keys.blob_id(&id), id)).collect();
                let want = ids
                    .iter()
                    .filter(|(_, id)| if round == 0 { id.as_str() == MANIFEST_ID } else { id.as_str() != MANIFEST_ID })
                    .filter(|(blob, id)| remote.get(*blob).copied().unwrap_or(0) > l.remote.get(*id).copied().unwrap_or(0))
                    .map(|(blob, _)| blob.clone())
                    .collect();
                // Blobs the enclave no longer has (wiped elsewhere): start from nothing there.
                for (blob, id) in &ids {
                    if !remote.contains_key(blob) {
                        l.remote.remove(id);
                    }
                }
                (want, ids)
            })
            .map_err(|e| (e, false))?;
            for chunk in want.chunks(64) {
                let (op, body) = palace::body(&keys, "get", "", chunk, 0, b"");
                let got = palace_request(conn, &op, &body).await?;
                let blobs = got["blobs"].as_array().cloned().unwrap_or_default();
                changes += with_palace(app, |l| {
                    let mut n = 0;
                    for b in &blobs {
                        let (Some(blob), Some(v), Some(ct)) = (b["blobId"].as_str(), b["version"].as_u64(), b["ct"].as_str()) else { continue };
                        let Some(id) = ids.get(blob) else { continue };
                        let Ok(plain) = B64.decode(ct).map_err(|e| e.to_string()).and_then(|c| keys.open(blob, v, &c)) else { continue };
                        if let Ok(k) = palace::merge_remote(l, id, &plain) {
                            n += k;
                            l.remote.insert(id.clone(), v);
                        }
                    }
                    n
                })
                .map_err(|e| (e, false))?;
            }
        }
        if !upload {
            return Ok(changes);
        }
        // 3. The queue: files first, the index last, so the index never names a file the
        // enclave does not have yet. A stale version is fetched, merged, and written again.
        let queue: Vec<(String, u64)> = with_palace(app, |l| {
            let mut q: Vec<(String, u64)> = l.dirty.iter().filter(|(id, _)| id.as_str() != MANIFEST_ID).map(|(k, v)| (k.clone(), *v)).collect();
            if let Some(r) = l.dirty.get(MANIFEST_ID) {
                q.push((MANIFEST_ID.to_string(), *r));
            }
            q
        })
        .map_err(|e| (e, false))?;
        for (id, _) in queue {
            let blob = keys.blob_id(&id);
            for _try in 0..3 {
                let (rev, expected, ct) = with_palace(app, |l| {
                    let expected = l.remote.get(&id).copied().unwrap_or(0);
                    (l.dirty.get(&id).copied(), expected, keys.seal(&blob, expected + 1, &palace::plaintext(l, &id)))
                })
                .map_err(|e| (e, false))?;
                let Some(rev) = rev else { break };
                let (op, body) = palace::body(&keys, "put", &blob, &[], expected, &ct);
                let a = palace_request(conn, &op, &body).await?;
                if a["kind"] == "palace.conflict" {
                    let (op, body) = palace::body(&keys, "get", "", std::slice::from_ref(&blob), 0, b"");
                    let got = palace_request(conn, &op, &body).await?;
                    with_palace(app, |l| {
                        if let Some(b) = got["blobs"].as_array().and_then(|a| a.first()) {
                            if let (Some(v), Some(ct)) = (b["version"].as_u64(), b["ct"].as_str()) {
                                if let Ok(plain) = B64.decode(ct).map_err(|e| e.to_string()).and_then(|c| keys.open(&blob, v, &c)) {
                                    let _ = palace::merge_remote(l, &id, &plain);
                                }
                                l.remote.insert(id.clone(), v);
                            }
                        }
                    })
                    .map_err(|e| (e, false))?;
                    continue;
                }
                let v = a["version"].as_u64().unwrap_or(expected + 1);
                with_palace(app, |l| {
                    l.remote.insert(id.clone(), v);
                    if l.dirty.get(&id) == Some(&rev) {
                        l.dirty.remove(&id);
                    }
                })
                .map_err(|e| (e, false))?;
                break;
            }
        }
        let gone: Vec<String> = with_palace(app, |l| l.gone.iter().cloned().collect()).map_err(|e| (e, false))?;
        for id in gone {
            let blob = keys.blob_id(&id);
            let mut expected = with_palace(app, |l| l.remote.get(&id).copied().unwrap_or(0)).map_err(|e| (e, false))?;
            for _try in 0..3 {
                let (op, body) = palace::body(&keys, "delete", &blob, &[], expected, b"");
                let a = palace_request(conn, &op, &body).await?;
                if a["kind"] == "palace.conflict" {
                    expected = a["version"].as_u64().unwrap_or(0);
                    continue;
                }
                with_palace(app, |l| {
                    l.gone.remove(&id);
                    l.remote.remove(&id);
                })
                .map_err(|e| (e, false))?;
                break;
            }
        }
        Ok(changes)
    }
    .await;
    // The palace's client goes with the burst: a second mixnet client is not kept alive
    // beside the account's.
    *net = None;
    drop(net);
    let now = palace::now_ms();
    let out = match result {
        Ok(n) => {
            let _ = with_palace(app, |l| palace::succeeded(l, now));
            Ok(n)
        }
        Err((e, route)) => {
            let _ = with_palace(app, |l| palace::failed(l, &e, route, now));
            Err(e)
        }
    };
    st.palace_syncing.store(false, std::sync::atomic::Ordering::Relaxed);
    match &out {
        Ok(n) => palace_emit(app, json!({ "phase": "done", "changes": n })),
        Err(e) => palace_emit(app, json!({ "phase": "failed", "error": e })),
    }
    out
}

/// The loop that keeps the palace in step: a sync at start, then the queue whenever its
/// moment has come.
async fn palace_loop(app: AppHandle) {
    let mut first = true;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(if first { 3 } else { 5 })).await;
        let Ok(dir) = data_dir(&app) else { continue };
        let cfg = palace::config(&dir);
        if cfg.enabled != Some(true) || cfg.local_only || palace_keys(&app).is_err() {
            continue;
        }
        let since = palace::now_ms().saturating_sub(palace::LAST_SPEND_MS.load(std::sync::atomic::Ordering::Relaxed));
        if since < palace::SPEND_GAP_MS {
            continue;
        }
        if cfg.wipe_pending {
            if let Err(e) = palace_wipe_remote(&app).await {
                log::warn!("[palace] the enclave's copy is not deleted yet: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(25)).await;
            }
            continue;
        }
        let (due, pending) = with_palace(&app, |l| (l.due_ms, !l.dirty.is_empty() || !l.gone.is_empty())).unwrap_or((0, false));
        let now = palace::now_ms();
        if first {
            first = false;
            let _ = palace_burst(&app, pending && due <= now).await;
            continue;
        }
        if pending && due <= now {
            let _ = palace_burst(&app, true).await;
        }
    }
}

#[tauri::command]
fn palace_status(app: AppHandle) -> Value {
    palace_status_json(&app)
}

/// On / off (opt-in) and "this device only".
#[tauri::command]
fn palace_set_config(app: AppHandle, enabled: bool, local_only: bool) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    let wipe_pending = palace::config(&dir).wipe_pending;
    palace::set_config(&dir, &palace::Config { enabled: Some(enabled), local_only, wipe_pending })?;
    palace_emit(&app, json!({}));
    Ok(palace_status_json(&app))
}

#[tauri::command]
fn palace_get(app: AppHandle) -> Result<Value, String> {
    if palace_keys(&app).is_err() {
        return Ok(json!({ "files": [], "placeholders": [] }));
    }
    with_palace(&app, |l| l.view())
}

/// Changes from the interface or the extraction: saved here at once, queued for upload.
#[tauri::command]
fn palace_apply(app: AppHandle, ops: Vec<palace::Op>) -> Result<Value, String> {
    let now = palace::now_ms();
    let (done, view) = with_palace(&app, |l| {
        let done = palace::apply(l, &ops, now);
        (done, l.view())
    })?;
    palace_emit(&app, json!({}));
    Ok(json!({ "applied": done, "palace": view }))
}

/// "Sync now": fetch, merge and upload whatever is queued, at once.
#[tauri::command]
async fn palace_sync(app: AppHandle, upload: Option<bool>) -> Result<Value, String> {
    let changes = palace_burst(&app, upload.unwrap_or(true)).await?;
    let mut s = palace_status_json(&app);
    s["changes"] = json!(changes);
    Ok(s)
}

/// "Delete entire Memory Palace": the enclave's copy (unless it never had one) and this
/// device's. Other devices keep theirs until they delete it too.
#[tauri::command]
async fn palace_wipe(app: AppHandle) -> Result<Value, String> {
    let dir = data_dir(&app)?;
    // This device's copy goes at once; the enclave's is marked to go, and the loop sends
    // the request until it is answered — over the mixnet, possibly waiting out the gap
    // after a payment note, so not something to hold the screen for.
    let mut cfg = palace::config(&dir);
    if cfg.enabled == Some(true) && !cfg.local_only {
        cfg.wipe_pending = true;
        palace::set_config(&dir, &cfg)?;
    }
    {
        let st = app.state::<AppState>();
        let _one = st.palace_local.lock().map_err(|_| "the palace is in an unknown state")?;
        palace::forget(&dir);
    }
    palace_emit(&app, json!({}));
    if cfg.wipe_pending {
        let h = app.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = palace_wipe_remote(&h).await {
                log::warn!("[palace] the enclave's copy is not deleted yet, tried again shortly: {e}");
            }
        });
    }
    Ok(palace_status_json(&app))
}

/// The enclave's copy of the palace, deleted; the mark cleared once it is.
async fn palace_wipe_remote(app: &AppHandle) -> Result<(), String> {
    let dir = data_dir(app)?;
    let keys = palace_keys(app)?;
    let st = app.state::<AppState>();
    let mut net = st.palace_net.lock().await;
    if !palace::config(&dir).wipe_pending {
        return Ok(()); // done meanwhile
    }
    let mut conn = palace_connection(app)?;
    let (op, body) = palace::body(&keys, "wipe", "", &[], 0, b"");
    let r = palace_request(&mut conn, &op, &body).await;
    *net = None;
    r.map_err(|(e, _)| e)?;
    let mut cfg = palace::config(&dir);
    cfg.wipe_pending = false;
    palace::set_config(&dir, &cfg)?;
    log::info!("[palace] the enclave's copy is deleted");
    palace_emit(app, json!({}));
    Ok(())
}

/// Everything in plain JSON, for the person to keep (the interface warns first).
#[tauri::command]
fn palace_export(app: AppHandle) -> Result<Value, String> {
    with_palace(&app, |l| l.export())
}

/// An export brought back: merged into what is here, and queued.
#[tauri::command]
fn palace_import(app: AppHandle, data: Value) -> Result<Value, String> {
    use tokumai_core::palace::{MANIFEST_ID, PLACEHOLDERS_ID};
    if data["schema"] != "tokumai/palace-export@1" {
        return Err("this is not a Memory Palace export".into());
    }
    let now = palace::now_ms();
    let n = with_palace(&app, |l| -> Result<usize, String> {
        let mut n = palace::merge_remote(l, MANIFEST_ID, data["manifest"].to_string().as_bytes())?;
        for f in data["files"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            let id = f["id"].as_str().unwrap_or("").to_string();
            if id.is_empty() || id == MANIFEST_ID || id == PLACEHOLDERS_ID {
                continue;
            }
            n += palace::merge_remote(l, &id, f.to_string().as_bytes())?;
        }
        palace::merge_remote(l, PLACEHOLDERS_ID, data["placeholders"].to_string().as_bytes())?;
        let ids = palace::known_ids(l);
        l.rev += 1;
        for id in ids {
            l.dirty.insert(id, l.rev);
        }
        l.due_ms = now + 5_000;
        Ok(n)
    })??;
    palace_emit(&app, json!({}));
    Ok(json!({ "imported": n }))
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
///
/// A desktop file dialog has no counterpart on a phone: there a file goes into a share
/// sheet and a picture into the photo library, both of which need native code
/// (`UIActivityViewController`, `writeToSavedPhotosAlbum`). Until that lands, the phone
/// says so plainly rather than failing somewhere deeper down.
#[cfg(not(any(target_os = "ios", target_os = "android")))]
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

/// iOS: a file goes into the share sheet — the person decides where it lands (Files,
/// Mail, AirDrop). It is written to the app's own temp directory first, because the sheet
/// hands over a file URL, not bytes.
#[cfg(target_os = "ios")]
#[tauri::command]
async fn save_file(app: AppHandle, data: String, filename: String) -> Result<Option<String>, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let bytes = B64.decode(data.as_bytes()).map_err(|e| format!("bad file data: {e}"))?;
    let safe = filename.replace(['/', '\\', '\0'], "_");
    let path = std::env::temp_dir().join(if safe.is_empty() { "tokumai-export".into() } else { safe });
    std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
    let p = path.to_string_lossy().to_string();
    let (shown, rx) = (p.clone(), p.clone());
    app.run_on_main_thread(move || {
        if let Err(e) = ios_native::share::present_share_sheet(&shown) {
            log::warn!("[share] {e}");
        }
    })
    .map_err(|e| e.to_string())?;
    Ok(Some(rx))
}

#[cfg(target_os = "android")]
#[tauri::command]
async fn save_file(_data: String, _filename: String) -> Result<Option<String>, String> {
    Err("saving to a file is not in this build yet".into())
}

#[cfg(not(any(target_os = "ios", target_os = "android")))]
#[tauri::command]
async fn save_image(data: String, filename: String, mime: Option<String>, lossless: Option<bool>) -> Result<Option<String>, String> {
    let _ = (mime, lossless); // the bytes go to disk as they are
    save_file(data, filename).await
}

/// iOS: a picture goes straight to the photo library, which is where somebody looks for it
/// afterwards. iOS asks for permission itself the first time
/// (`NSPhotoLibraryAddUsageDescription`) and writes asynchronously; the answer is
/// Photos' own, so "saved" means saved. A person left staring at the permission prompt
/// is the one way the answer never comes, hence the deadline.
#[cfg(target_os = "ios")]
#[tauri::command]
async fn save_image(app: AppHandle, data: String, filename: String, mime: Option<String>, lossless: Option<bool>) -> Result<Option<String>, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let _ = filename;
    let bytes = B64.decode(data.as_bytes()).map_err(|e| format!("bad image data: {e}"))?;
    let mime = mime.unwrap_or_default();
    let lossless = lossless.unwrap_or(false);
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || ios_native::share::save_image_to_photos(&bytes, &mime, lossless, tx)).map_err(|e| e.to_string())?;
    match tokio::time::timeout(std::time::Duration::from_secs(120), rx).await {
        Ok(answer) => answer.map_err(|e| e.to_string())??,
        Err(_) => return Err("Photos has not answered — if it asked for permission, allow it and save again".into()),
    }
    Ok(Some("photos".into()))
}

#[cfg(target_os = "android")]
#[tauri::command]
async fn save_image(_data: String, _filename: String, _mime: Option<String>, _lossless: Option<bool>) -> Result<Option<String>, String> {
    Err("saving a picture is not in this build yet".into())
}

/// The native image/camera picker. `source` is "library" or "camera"; the answer is the
/// picked photo as JPEG, or null when the sheet was cancelled.
#[cfg(target_os = "ios")]
#[tauri::command]
async fn pick_image(app: AppHandle, source: String) -> Result<Option<Value>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || ios_native::picker::present(&source, tx)).map_err(|e| e.to_string())?;
    let bytes = rx.await.map_err(|e| e.to_string())??;
    Ok(bytes.map(|b| {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        json!({ "mimeType": "image/jpeg", "name": "photo.jpg", "dataB64": B64.encode(&b) })
    }))
}

#[cfg(not(target_os = "ios"))]
#[tauri::command]
async fn pick_image(_source: String) -> Result<Option<Value>, String> {
    Err("the native picker exists only on iOS".into())
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
    // iOS goes through UIKit directly: the opener plugin's Swift package is not linked
    // into the generated Xcode project, so its command fails at runtime there and nowhere
    // else — a dead legal link on the one platform whose review reads them.
    #[cfg(target_os = "ios")]
    {
        return app.run_on_main_thread(move || ios_native::open_url(&url)).map_err(|e| e.to_string());
    }
    #[cfg(not(target_os = "ios"))]
    {
        use tauri_plugin_opener::OpenerExt;
        app.opener().open_url(&url, None::<&str>).map_err(|e| e.to_string())
    }
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
            // Where the data lives, told to the keystore (a phone keeps what goes with its
            // key beside the data); on iOS, out of every backup.
            match data_dir(app.handle()) {
                Ok(dir) => {
                    keystore::set_data_dir(dir.clone());
                    #[cfg(target_os = "ios")]
                    if let Err(e) = ios_native::exclude_from_backup(&dir) {
                        log::error!("[data] {e}");
                    }
                }
                Err(e) => log::error!("[data] no data directory: {e}"),
            }
            // Android: the TLS verifier needs the Activity's JNI env before the first
            // directory fetch (see init_android_tls_verifier), and the keystore the VM.
            #[cfg(target_os = "android")]
            init_android_tls_verifier();
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
            // The Memory Palace: a sync at start, then the upload queue (`palace`).
            tauri::async_runtime::spawn(palace_loop(app.handle().clone()));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            local_state, state,
            account_new, account_restore, account_delete, account_reveal, account_migrate_qr,
            phrase_check_start, phrase_check_verify, phrase_backup_get,
            chat, cancel_chat,
            plan_ladder, plan_checkout, plan_poll, plan_change, plan_forget,
            iap_offers, iap_buy, iap_sync,
            mixnet_route, list_entry_gateways, set_entry_gateway, set_entry_random, set_mixnet_perf, mixnet_ping,
            enclave_doors, set_enclave_door,
            app_hidden, app_resumed, mixnet_heartbeat,
            support_send, support_list, support_diag,
            vault_list, vault_load, vault_save, vault_remove, vault_purge_webdata,
            save_file, save_image, pick_image, open_external,
            ocr_scan, pdf_text, pdf_ocr, pdf_pages, smart_available, smart_detect,
            voice::voice_available, voice::voice_start, voice::voice_poll, voice::voice_stop, voice::voice_cancel,
            palace_status, palace_set_config, palace_get, palace_apply, palace_sync, palace_wipe, palace_export, palace_import,
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

/// Android: hand rustls-platform-verifier the JNI env and the Activity so TLS checks use
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
                log::error!("tokumai: android TLS verifier: no JavaVM from the activity env: {e}");
                return;
            }
        };
        let raw_ctx = activity.as_raw();
        // The same VM serves the keystore (keystore::android), for as long as the process lives.
        keystore::android::set_vm(raw_vm.cast());
        let vm = unsafe { jni::JavaVM::from_raw(raw_vm.cast()) };
        let res: Result<(), jni::errors::Error> = vm.attach_current_thread(|env22| {
            let context = unsafe { jni::objects::JObject::from_raw(env22, raw_ctx.cast()) };
            // Kept for the owner check (owner.rs), which needs the activity for its prompt.
            keystore::android::set_activity(env22.new_global_ref(&context)?);
            rustls_platform_verifier::android::init_with_env(env22, context)
        });
        match res {
            Ok(()) => log::info!("tokumai: android TLS verifier initialised"),
            Err(e) => log::error!("tokumai: android TLS verifier init FAILED: {e} — mixnet directory fetches will not work"),
        }
    });
}
