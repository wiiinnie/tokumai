// proto.rs — the scrai wire protocol as the APP speaks it, minus the UI.
//
// Envelopes are `{v, kind, id, app, …}` (the `app` stamp satisfies the server's
// SCRAI_MIN_APP release gate). Funding reproduces src-tauri/src/lib.rs `invoice` →
// `collect` (entitlement + coconut withdraw) → `redeem`; chat reproduces `chat_impl`
// (session.status, then the signed request with the canonical body). Byte-compat with
// the app is the point: a load test that cheats the protocol measures the wrong thing.

use crate::keys::{rand_hex, Signer};
use crate::mix::{CallError, Mix};
use crate::stats::{Live, Sample};
use nym_sdk::mixnet::Recipient;
use scrai_core::coconut::{self, PayInfo};
use scrai_core::federation::{FedRequest, FedResponse};
use scrai_core::purse::Purse;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub const APP: &str = env!("CARGO_PKG_VERSION");

pub fn envelope(kind: &str) -> Value {
    json!({ "v": 1, "kind": kind, "id": rand_hex(16), "app": APP })
}

fn with(mut env: Value, extra: Value) -> Value {
    if let (Some(o), Some(x)) = (env.as_object_mut(), extra.as_object()) {
        for (k, v) in x {
            o.insert(k.clone(), v.clone());
        }
    }
    env
}

/// One simulated user's request context: its mixnet client, where the server is, the
/// per-request knobs, and the sample sink.
pub struct Ctx {
    pub client: usize,
    pub mix: Arc<Mix>,
    pub server: Recipient,
    pub surbs: u32,
    pub surbs_chat: u32,
    pub timeout: Duration,
    pub tx: mpsc::Sender<Sample>,
    pub live: Arc<Live>,
    pub t0: Instant,
    seq: AtomicU64,
}

impl Ctx {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: usize,
        mix: Arc<Mix>,
        server: Recipient,
        surbs: u32,
        surbs_chat: u32,
        timeout: Duration,
        tx: mpsc::Sender<Sample>,
        live: Arc<Live>,
        t0: Instant,
    ) -> Self {
        Self { client, mix, server, surbs, surbs_chat, timeout, tx, live, t0, seq: AtomicU64::new(0) }
    }

    pub async fn record(&self, op: &str, start: Instant, ok: bool, err: String, reply_bytes: usize) {
        let s = Sample {
            client: self.client,
            op: op.to_string(),
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            start_ms: start.duration_since(self.t0).as_millis() as u64,
            latency_ms: start.elapsed().as_millis() as u64,
            ok,
            err,
            reply_bytes,
        };
        let _ = self.tx.send(s).await;
    }

    /// Send, time, record. `Ok(reply)` only for a delivered non-error reply; a delivered
    /// `kind:"error"` comes back as `Err("server: …")` (or `busy: …` for the chat cap).
    pub async fn call(&self, op: &str, req: Value) -> Result<Value, String> {
        self.call_surbs(op, req, self.surbs).await
    }

    pub async fn call_surbs(&self, op: &str, req: Value, surbs: u32) -> Result<Value, String> {
        let start = Instant::now();
        self.live.inflight.fetch_add(1, Ordering::Relaxed);
        let res = self.mix.call(&self.server, &req, surbs, self.timeout).await;
        self.live.inflight.fetch_sub(1, Ordering::Relaxed);
        match res {
            Ok((reply, bytes)) => {
                if reply.get("kind").and_then(|k| k.as_str()) == Some("error") {
                    let msg = reply.get("error").and_then(|e| e.as_str()).unwrap_or("server error");
                    let class = if msg.contains("busy") { "busy" } else { "server" };
                    let err = format!("{class}: {msg}");
                    self.record(op, start, false, err.clone(), bytes).await;
                    return Err(err);
                }
                self.record(op, start, true, String::new(), bytes).await;
                Ok(reply)
            }
            Err(e) => {
                let err = match e {
                    CallError::Timeout => "timeout".to_string(),
                    other => other.to_string(),
                };
                self.record(op, start, false, err.clone(), 0).await;
                Err(err)
            }
        }
    }
}

/// A simulated user: an account (buys), a session (spends), the counter the server
/// expects next, and the coconut book withdrawn against the fake payment.
pub struct User {
    pub account: Signer,
    pub session: Signer,
    pub counter: u64,
    pub balance: u64,
    pub purse: Option<Purse>,
}

impl User {
    pub fn new() -> Self {
        Self { account: Signer::random(), session: Signer::random(), counter: 0, balance: 0, purse: None }
    }
}

pub async fn ping(ctx: &Ctx) -> Result<Value, String> {
    ctx.call("ping", json!({ "v": 1, "kind": "ping", "id": rand_hex(8), "app": APP })).await
}

pub async fn models(ctx: &Ctx) -> Result<Value, String> {
    let r = ctx.call("models", envelope("models")).await?;
    // RUST_LOG=info shows what the server offers (a cheap way to verify a catalog change).
    if let Some(list) = r.get("models").and_then(|m| m.as_array()) {
        let ids: Vec<&str> = list.iter().filter_map(|m| m.get("model").and_then(|x| x.as_str())).collect();
        log::info!("catalog ({}): {}", ids.len(), ids.join(", "));
    }
    Ok(r)
}

pub async fn session_status(ctx: &Ctx, user: &mut User) -> Result<(), String> {
    let r = ctx
        .call("session.status", with(envelope("session.status"), json!({ "sessionId": user.session.id })))
        .await?;
    user.counter = r.get("counter").and_then(|c| c.as_u64()).unwrap_or(0);
    user.balance = r.get("balance").and_then(|b| b.as_u64()).unwrap_or(0);
    Ok(())
}

/// Buy `usd` on the FAKE rail, withdraw the ticketbook, redeem `redeem_coins` into the
/// session — the app's `invoice` + `collect` + `redeem`, each leg recorded as its own op.
pub async fn fund(ctx: &Ctx, user: &mut User, usd: u32, redeem_coins: u64) -> Result<u64, String> {
    // invoice.create — account-signed over `invoice:<usd>`
    let nonce = rand_hex(16);
    let req = with(envelope("invoice.create"), json!({
        "publicKey": user.account.pem, "usd": usd, "method": "btc",
        "nonce": nonce, "sig": user.account.sign_account(&format!("invoice:{usd}"), &nonce),
    }));
    let r = ctx.call("invoice.create", req).await?;
    let invoice_id = r
        .get("invoiceId")
        .and_then(|i| i.as_str())
        .ok_or("invoice.create: reply carries no invoiceId")?
        .to_string();

    // invoice.status — the fake rail settles on the first poll
    let r = ctx.call("invoice.status", with(envelope("invoice.status"), json!({ "invoiceId": invoice_id }))).await?;
    let status = r.get("status").and_then(|s| s.as_str()).unwrap_or("?");
    if status != "paid" {
        return Err(format!(
            "invoice still `{status}` after the first poll — the server must run with SCRAI_FAKE_PAYMENTS=1 for a chat load test"
        ));
    }

    // entitlement — how much is owed to this account
    let nonce = rand_hex(16);
    let req = with(envelope("entitlement"), json!({
        "publicKey": user.account.pem, "nonce": nonce, "sig": user.account.sign_account("entitlement", &nonce),
    }));
    let r = ctx.call("entitlement", req).await?;
    let owed = r.get("entitlement").and_then(|e| e.as_u64()).unwrap_or(0);
    if owed == 0 {
        return Err("no entitlement after a paid invoice".into());
    }

    // coconut Keys — the federation's verification material
    let r = ctx.call("coconut.keys", with(envelope("coconut"), json!({ "fed": "Keys" }))).await?;
    let keys: FedResponse = serde_json::from_value(r.get("fed").cloned().unwrap_or(Value::Null))
        .map_err(|e| format!("bad Keys reply: {e}"))?;
    let FedResponse::Keys { vk, auth_vks, coin_sigs, date_sigs, expiration_date, total_coins, .. } = keys else {
        return Err("unexpected reply to Keys".into());
    };
    if total_coins == 0 || total_coins > 4096 {
        return Err(format!("implausible ticketbook size {total_coins}"));
    }

    // coconut Withdraw — account-signed, one blinded share per authority, then aggregate
    let user_kp = coconut::new_user();
    let (wreq, req_info) =
        coconut::make_withdrawal_request(user_kp.secret_key(), expiration_date, coconut::DEFAULT_T_TYPE)?;
    let mut shares = Vec::new();
    for (i, vk_auth) in auth_vks.iter().enumerate() {
        let fed = FedRequest::Withdraw { user_pk: user_kp.public_key(), req: wreq.clone() };
        let nonce = rand_hex(16);
        let req = with(envelope("coconut"), json!({
            "fed": serde_json::to_value(&fed).map_err(|e| e.to_string())?,
            "publicKey": user.account.pem, "nonce": nonce,
            "sig": user.account.sign_account("withdraw:coconut", &nonce),
        }));
        let r = ctx.call("coconut.withdraw", req).await?;
        let resp: FedResponse = serde_json::from_value(r.get("fed").cloned().unwrap_or(Value::Null))
            .map_err(|e| format!("bad Withdraw reply: {e}"))?;
        let blinded = match resp {
            FedResponse::Withdraw { blinded } => blinded,
            FedResponse::Error { message } => return Err(format!("withdraw refused: {message}")),
            _ => return Err("unexpected reply to Withdraw".into()),
        };
        shares.push(coconut::verify_share(vk_auth, user_kp.secret_key(), &blinded, &req_info, i as u64 + 1)?);
    }
    let wallet = coconut::aggregate(&vk, user_kp.secret_key(), &shares, &req_info)?;
    user.purse = Some(Purse::new(wallet, user_kp, vk, coin_sigs, date_sigs, total_coins, expiration_date));

    redeem(ctx, user, redeem_coins).await
}

/// Spend `coins` of the held book into the session (the app's REDEEM_CHUNK_COINS = 100).
pub async fn redeem(ctx: &Ctx, user: &mut User, coins: u64) -> Result<u64, String> {
    let purse = user.purse.as_mut().ok_or("no coconut book to redeem from")?;
    let coins = coins.min(purse.remaining_coins());
    if coins == 0 {
        return Err("coconut book is empty".into());
    }
    let mut pib = [0u8; 72];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut pib);
    let spend_date = purse.expiration_date().saturating_sub(86_400);
    let payment = purse.spend(coins, &PayInfo { pay_info_bytes: pib }, spend_date)?;
    let req = with(envelope("redeem"), json!({
        "sessionId": user.session.id,
        "payment": serde_json::to_value(&payment).map_err(|e| e.to_string())?,
        "pay_info": pib.to_vec(), "spend_date": spend_date,
    }));
    let r = ctx.call("redeem", req).await?;
    user.balance = r.get("balance").and_then(|b| b.as_u64()).unwrap_or(0);
    // A redeem does not touch the counter, but syncing here is what the app effectively
    // does before its first chat (session.status runs before every chat).
    Ok(user.balance)
}

/// One signed chat turn. Like the app: `session.status` first (unless `skip_status`),
/// then the request signed over the canonical body {model, messages, maxTokens}.
pub async fn chat(
    ctx: &Ctx,
    user: &mut User,
    model: &str,
    prompt: &str,
    max_tokens: u64,
    skip_status: bool,
) -> Result<Value, String> {
    if !skip_status {
        session_status(ctx, user).await?;
    }
    let counter = user.counter + 1;
    let messages = json!([{ "role": "user", "content": prompt }]);
    let body = serde_json::to_string(&json!({ "model": model, "messages": messages, "maxTokens": max_tokens }))
        .map_err(|e| e.to_string())?;
    let sig = user.session.sign_session(counter, &body);
    let req = with(envelope("chat"), json!({
        "model": model, "messages": messages, "stream": false,
        "sessionId": user.session.id, "counter": counter, "sig": sig,
        "publicKey": user.session.pem, "maxTokens": max_tokens,
    }));
    // The counter is consumed the moment the request leaves (the server may process it
    // even when our reply is lost); a following session.status re-syncs regardless.
    user.counter = counter;
    let r = ctx.call_surbs("chat", req, ctx.surbs_chat).await?;
    if let Some(b) = r.get("balance").and_then(|b| b.as_u64()) {
        user.balance = b;
    }
    Ok(r)
}
