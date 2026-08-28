// scrai-server — the mixnet service provider + issuing authority.
//
// It runs a Nym client (persistent identity → a stable address clients target),
// and for every request received over the mixnet it calls the transport-agnostic
// `federation::dispatch` and replies anonymously via the request's reply SURB.
//
// End-to-end verification happens by RUNNING this against the mixnet (like the
// client's `nym.rs`); it can't be unit-tested here. The request-handling logic it
// wraps (`dispatch`, the authority) IS unit-tested in scrai-core.
//
// Single-authority (1-of-1) for the first bring-up. A real multi-server federation
// needs one shared DKG whose shares are distributed to each server (same published
// verification key) — a separate setup step; see docs/federation-params.md.

// The request handlers live in the library crate (server/src/lib.rs) so the fuzz targets
// under server/fuzz/ can drive the same parsers the mixnet loop feeds.
use scrai_server::{catalog, chat, http, inflight, pay, replies, store, uploads};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use nym_sdk::mixnet::{AnonymousSenderTag, MixnetClientBuilder, MixnetMessageSender, StoragePaths};
use tokio::sync::Semaphore;
use scrai_core::federation::{self, Authority};
use scrai_core::pricing::PricingTable;
use scrai_core::quorum::QuorumStore;
use scrai_core::session::SessionStore;

// One issued ticketbook = 500 coins × 1000 SCRAI = 500,000 SCRAI = the $5
// minimum purchase tier, so every tier is a whole number of books ($10 = 2,
// $50 = 10). Changing this needs a FRESH authority bootstrap (delete
// data/authority.json) and invalidates previously issued purses — fine while
// everything is testnet.
const TICKETBOOK_COINS: u64 = 500;

/// Number of issuing authorities this build runs. 1 = a single trusted-dealer
/// authority (testnet bring-up): it can forge unlimited credentials, so issuing
/// against REAL money is gated (see the H9 interlock in `main`). A real production
/// federation is t-of-n (≥ 2) with a shared DKG — bump this and wire the shares.
const AUTHORITY_N: usize = 1;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok(); // load provider keys from .env

    // Refuse to boot with an ambiguous Gemini key configuration: a testnet key
    // AND a mainnet key both active means nobody knows which account is being
    // billed. Exactly one must be uncommented in .env.
    match crate::chat::gemini_api_key() {
        Ok((_, network)) => eprintln!("scrai-server: Gemini key active: {network}"),
        Err(e) => {
            if e.contains("BOTH") {
                eprintln!("scrai-server: FATAL: {e}");
                std::process::exit(1);
            }
            eprintln!("scrai-server: note: {e} — Gemini models will be unavailable");
        }
    }

    // Install OUR log filter before the nym-sdk installs its own logger (first
    // one wins): the mixnet's "duplicate fragment received" warnings are normal
    // retransmission noise on SURB-heavy requests — one line per re-sent Sphinx
    // fragment floods the journal. RUST_LOG still overrides everything.
    env_logger::Builder::new()
        .parse_filters("info,nym_sphinx_chunking=error")
        .parse_default_env()
        .try_init()
        .ok();
    let data_dir = PathBuf::from(std::env::var("SCRAI_DATA").unwrap_or_else(|_| "./data".into()));
    let authority = load_or_bootstrap(&data_dir.join("authority.json"));

    // Persistent Nym identity so the server keeps ONE address across restarts.
    let storage = StoragePaths::new_from_dir(data_dir.join(".nym-server"))
        .expect("nym storage paths");
    let mut builder = MixnetClientBuilder::new_with_default_storage(storage)
        .await
        .expect("mixnet client builder");
    // Pin the gateway via SCRAI_GATEWAY (identity key). Only honoured on the
    // FIRST registration — an existing .nym-server identity keeps its gateway,
    // so to re-home the server: stop it, delete data/.nym-server, start again
    // (this also mints a NEW Nym address for the clients).
    let pinned = std::env::var("SCRAI_GATEWAY").ok().filter(|g| !g.trim().is_empty());
    if let Some(gw) = pinned {
        println!("scrai-server: requesting entry gateway {gw}");
        builder = builder.request_gateway(gw.trim().to_string());
    } else if let Some((gw, country, host)) = random_described_gateway().await {
        // No pin → curated random instead of the SDK's blind pick: only gateways
        // whose directory entry carries a location AND a reverse-DNS hostname, so
        // the exit the clients see is always identifiable in their UI.
        println!("scrai-server: picked described gateway {gw} ({country}, {host})");
        builder = builder.request_gateway(gw);
    } else {
        println!("scrai-server: directory unavailable — letting the SDK pick a gateway");
    }
    let mut client = builder
        .build()
        .expect("mixnet build")
        .connect_to_mixnet()
        .await
        .expect("mixnet connect");

    // H2: a cloneable sender lets spawned tasks reply concurrently without borrowing the
    // client. Used for the pure `models` catalog fetch so a slow provider call can't wedge
    // the single dispatch loop.
    let reply_sender = client.split_sender();

    println!(
        "scrai-server: authority #{} live on the mixnet.\n  address: {}\n  (point a client at this address)",
        authority.index(),
        client.nym_address()
    );

    // Durable state: session balances + double-spend records survive a restart.
    let mut db = store::Store::open(&data_dir.join("state.db")).expect("open state db");
    // L1: absent snapshot = fresh start (default); present-but-UNPARSEABLE = FATAL, never a
    // silent reset — a reset double-spend set would reopen every spent coin, and reset
    // balances would erase credit. (Normal writes are valid+atomic, so this only fires on
    // external corruption, and then the operator must act, not the server silently.)
    let mut quorum = match db.load("quorum") {
        None => QuorumStore::default(),
        Some(j) => serde_json::from_str(&j).unwrap_or_else(|e| {
            eprintln!("scrai-server: FATAL: quorum snapshot present but unparseable ({e}) — refusing \
                to start (a silent reset would reopen every spent coin). Restore a good state.db.");
            std::process::exit(1);
        }),
    };
    let mut sessions = match db.load("sessions") {
        None => SessionStore::default(),
        Some(j) => serde_json::from_str(&j).unwrap_or_else(|e| {
            eprintln!("scrai-server: FATAL: sessions snapshot present but unparseable ({e}) — refusing \
                to start (a silent reset would zero every funded balance). Restore a good state.db.");
            std::process::exit(1);
        }),
    };
    println!(
        "scrai-server: state loaded (quorum rev {}, sessions rev {})",
        quorum.revision(),
        sessions.revision()
    );

    // Per-model pricing (USD/1M) + retail margin — drives the catalog rates AND chat
    // billing, so displayed price == charged price.
    let pricing = std::sync::Arc::new(load_pricing());
    let margin = pricing_margin();
    println!(
        "scrai-server: pricing table {} (margin {margin})",
        pricing.version()
    );
    // Metrics day boundary — echoed to scrai-admin so the table header names the zone.
    let tz = metrics_tz_name();
    let _ = metrics_tz_offset(0); // validates the value once at boot (logs a warning if unknown)
    let _ = db.save_many(&[("metrics_tz", tz.as_str())]);
    println!("scrai-server: metrics day boundary: {tz}");

    // Staged vision-image uploads (chunked over the mixnet, consumed by chat).
    // Ephemeral by design — never persisted.
    let mut uploads = uploads::UploadStore::default();
    // Generated pictures too big for one mixnet reply, served back in chunks (replies.rs).
    let mut staged = replies::ReplyStore::default();
    // In-memory idempotent-retry cache: session_id → (counter, reply bytes). Lets a
    // client whose reply was lost resend the SAME counter and get the SAME answer back
    // instead of a second charge. Not persisted — a restart just re-syncs the counter.
    let mut chat_replies: std::collections::HashMap<String, (u64, Vec<u8>)> =
        std::collections::HashMap::new();

    // The paywall: invoices + entitlements + burned nonces (durable), and the
    // payment gateway it raises invoices on.
    // Arc: the slow gateway HTTP (BTCPay / Nyx LCD) runs in spawned tasks (H2 for pay).
    let gateway = Arc::new(pay::Gateway::from_env());
    let mut paywall = match db.load("pay") {
        None => pay::Pay::default(),
        Some(j) => serde_json::from_str(&j).unwrap_or_else(|e| {
            eprintln!("scrai-server: FATAL: pay snapshot present but unparseable ({e}) — refusing to \
                start (a silent reset would drop paid invoices + entitlements). Restore a good state.db.");
            std::process::exit(1);
        }),
    };
    // Revision marks of what is already on disk — persist_changed() re-saves a store
    // only when its revision moved past these.
    let mut saved = SavedRevs { sessions: sessions.revision(), quorum: quorum.revision(), pay: paywall.revision() };
    let book_scrai = TICKETBOOK_COINS * scrai_core::coconut::COIN_SCRAI;
    println!(
        "scrai-server: gateway {} · ticketbook {} coins ({} SCRAI)",
        gateway.name(),
        TICKETBOOK_COINS,
        book_scrai
    );

    // H9: a single (1-of-1) authority can forge unlimited credentials. That is fine for
    // a testnet bring-up but NEVER against real money — refuse to issue unless the
    // operator deliberately overrides. A real t-of-n DKG (AUTHORITY_N ≥ 2) removes this.
    if AUTHORITY_N < 2
        && !gateway.is_fake()
        && std::env::var("SCRAI_ALLOW_SINGLE_AUTHORITY").as_deref() != Ok("1")
    {
        eprintln!(
            "scrai-server: FATAL: refusing to issue real-money credentials from a single \
             1-of-1 authority (it can forge unlimited coins). For dev use SCRAI_FAKE_PAYMENTS=1; \
             for production run a real t-of-n DKG; to override deliberately (testnet only) set \
             SCRAI_ALLOW_SINGLE_AUTHORITY=1."
        );
        std::process::exit(1);
    }

    // Serve forever: receive → dispatch → reply via the request's SURB.
    // Graceful shutdown: Ctrl+C (dev) and SIGTERM (systemd stop) break the loop
    // so `client.disconnect()` runs — that is what flushes the reply-SURB store
    // to disk. A hard kill instead leaves the sqlite mid-write and the next
    // start logs "loaded data is inconsistent" and rebuilds it from scratch.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");

    // H2: a chat's slow provider call runs in a spawned task; its (reserved) result comes
    // back through this channel and settle() runs ON THE LOOP — so the session
    // counter/balance are only ever touched from this single thread (no double-spend race),
    // while payments and other chats keep flowing instead of blocking behind the HTTP.
    struct HttpDone {
        pending: chat::PendingChat,
        result: Result<(String, scrai_core::billing::TokenUsage, chat::Images), String>,
        tag: AnonymousSenderTag,
        /// Keeps the client counted as in flight until the reply below has gone out.
        _guard: inflight::Guard<AnonymousSenderTag>,
    }
    let (http_tx, mut http_rx) = tokio::sync::mpsc::channel::<HttpDone>(256);
    // Same shape for the paywall: begin() on the loop, gateway HTTP spawned, finish() here.
    struct PayDone {
        outcome: pay::PayOutcome,
        tag: AnonymousSenderTag,
        _guard: inflight::Guard<AnonymousSenderTag>,
    }
    let (pay_tx, mut pay_rx) = tokio::sync::mpsc::channel::<PayDone>(64);

    // Concurrency caps for the spawned slow paths. A chat holds its worst-case
    // reservation while it waits for a slot; past QUEUE_WAIT it fails fast (settle()
    // refunds it) instead of piling up behind a stalled provider. Gateway calls get a
    // smaller pool: an unauthenticated invoice.status must not be able to open hundreds
    // of LCD connections.
    let max_chats = env_usize("SCRAI_MAX_INFLIGHT_CHATS", 64);
    let max_gateway = env_usize("SCRAI_MAX_INFLIGHT_GATEWAY", 16);
    let chat_slots = Arc::new(Semaphore::new(max_chats));
    let gateway_slots = Arc::new(Semaphore::new(max_gateway));
    const QUEUE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
    println!("scrai-server: concurrency caps — chats {max_chats}, gateway calls {max_gateway}");
    if pay::is_testnet_server() {
        println!("scrai-server: TESTNET mode — $1 faucet purchases enabled (SCRAI_TESTNET=1)");
    }

    // Distinct clients with a spawned request in flight; the daily peak lands in the
    // `daily` table for scrai-admin ("peak clients"). Written only when today's mark
    // rises, so the hot path costs no extra fsync.
    let inflight: inflight::Inflight<AnonymousSenderTag> = inflight::Inflight::default();
    let mut peak_written: (String, usize) = (String::new(), 0);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("scrai-server: Ctrl+C — shutting down");
                break;
            }
            _ = sigterm.recv() => {
                println!("scrai-server: SIGTERM — shutting down");
                break;
            }
            // A spawned chat's provider call returned → price + settle it here on the loop.
            Some(done) = http_rx.recv() => {
                let mut response = chat::settle(done.pending, done.result, &mut sessions, &mut chat_replies);
                // Per-day chat metrics from the reply (spent = charged, cost = provider price).
                if let Ok(mut rv) = serde_json::from_slice::<serde_json::Value>(&response) {
                    // Big generated pictures leave as chunk references, not as one huge reply —
                    // only for clients that asked for it (`chunkedImages`), so an older app
                    // still gets its pictures inline.
                    if rv.get("chunked").and_then(|c| c.as_bool()).unwrap_or(false) {
                        staged.stage(&mut rv);
                        response = serde_json::to_vec(&rv).unwrap_or(response);
                    }
                    let errored = rv.get("kind").and_then(|k| k.as_str()) == Some("error");
                    if !errored {
                        let today = today_utc();
                        let spent = rv.get("cost").and_then(|c| c.as_u64()).unwrap_or(0);
                        let cost = rv.pointer("/usage/billing/costScrai").and_then(|c| c.as_f64()).map(|f| f.ceil() as u64).unwrap_or(0);
                        db.bump_daily(&today, 1, spent, cost, 0, 0);
                        // Per-model breakdown for the admin table (the reply names the billed model).
                        let model = rv.pointer("/usage/billing/model").and_then(|m| m.as_str()).unwrap_or("unknown");
                        db.bump_daily_model(&today, model, 1, spent, cost);
                        // Consume the month's grounding allowance — re-read on the loop, so it's race-free.
                        let q = rv.pointer("/usage/groundingQueries").and_then(|c| c.as_u64()).unwrap_or(0);
                        if q > 0 {
                            let g_month_key = format!("grounding:{}", &today[..7]);
                            let g_used: u64 = db.load(&g_month_key).and_then(|s| s.parse().ok()).unwrap_or(0);
                            let _ = db.save_many(&[(g_month_key.as_str(), (g_used + q).to_string().as_str())]);
                        }
                    }
                }
                // Durability: persist any changed store before acknowledging (same as below).
                persist_changed(&mut db, &sessions, &quorum, &paywall, &mut saved);
                if let Err(e) = reply_sender.send_reply(done.tag, response).await {
                    eprintln!("scrai-server: chat reply failed: {e}");
                }
            }
            // A spawned gateway call (invoice / entitlement sweep) returned → apply it here.
            Some(done) = pay_rx.recv() => {
                let kind = done.outcome.kind();
                let ent_before = paywall.total_entitlement();
                let response = paywall.finish(done.outcome, &gateway);
                let (c0, c1) = label_color(kind);
                println!("scrai-server: handled {c0}{kind}{c1} (→ {} bytes)", response.len());
                // A settled invoice = one purchase + its scrai, attributed to today.
                let delta = paywall.total_entitlement().saturating_sub(ent_before);
                if delta > 0 {
                    db.bump_daily(&today_utc(), 0, 0, 0, 1, delta);
                }
                persist_changed(&mut db, &sessions, &quorum, &paywall, &mut saved);
                if let Err(e) = reply_sender.send_reply(done.tag, response).await {
                    eprintln!("scrai-server: pay reply failed: {e}");
                }
            }
            batch = client.wait_for_messages() => {
                let Some(messages) = batch else {
                    eprintln!("scrai-server: mixnet stream ended");
                    break;
                };
                for m in messages {
            let Some(tag) = m.sender_tag else {
                eprintln!("scrai-server: dropping a message with no reply SURB");
                continue;
            };
            // `chat` + `models` need async HTTP to the provider; everything else is
            // handled synchronously by the shared core.
            let kind = serde_json::from_slice::<serde_json::Value>(&m.message)
                .ok()
                .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(String::from))
                .unwrap_or_default();
            // L5: every control branch (coconut/invoice/redeem/models/ping/gateway) should be
            // tiny; feeding a big reassembled body to serde_json + O(coins) BLS is wasted
            // transient allocation. chat + upload manage their own (much larger) size limits.
            const MAX_CONTROL_BYTES: usize = 256 * 1024;
            if !matches!(kind.as_str(), "chat" | "upload.begin" | "upload.chunk" | "image.chunk")
                && m.message.len() > MAX_CONTROL_BYTES
            {
                let resp = serde_json::to_vec(&serde_json::json!({"kind":"error","error":"request too large"})).unwrap_or_default();
                if let Err(e) = reply_sender.send_reply(tag, resp).await {
                    eprintln!("scrai-server: reply failed: {e}");
                }
                continue;
            }
            // H2: the catalog fetch is pure — immutable pricing, no session/paywall/quorum
            // state — and its provider HTTP (Gemini/Groq model lists) can be slow. Spawn it
            // so it never blocks chat/payment on the single dispatch loop; it replies itself.
            if kind == "models" {
                let (p, sender, msg) = (pricing.clone(), reply_sender.clone(), m.message.clone());
                let guard = inflight.enter(tag);
                note_peak(&db, &inflight, &mut peak_written);
                tokio::spawn(async move {
                    let resp = catalog::handle(&msg, &p, margin).await;
                    if let Err(e) = sender.send_reply(tag, resp).await {
                        eprintln!("scrai-server: models reply failed: {e}");
                    }
                    drop(guard); // replied → no longer in flight
                });
                continue;
            }
            // DEV latency probe: reply immediately with a pong — no session, DB or
            // provider work — so a client round-trip measures the MIXNET alone.
            if kind == "ping" {
                let id = serde_json::from_slice::<serde_json::Value>(&m.message)
                    .ok()
                    .and_then(|v| v.get("id").cloned())
                    .unwrap_or(serde_json::Value::Null);
                let resp = serde_json::to_vec(&serde_json::json!({"id": id, "kind": "pong"})).unwrap_or_default();
                if let Err(e) = reply_sender.send_reply(tag, resp).await {
                    eprintln!("scrai-server: ping reply failed: {e}");
                }
                continue;
            }
            // H2: chat is the only slow (provider-HTTP) money path. Reserve it on the loop
            // (fast + serialized), then run the provider call in a SPAWNED task; the result
            // returns via http_tx and settle() runs back here — so the session counter/balance
            // are never touched off-thread, and payments + other chats don't wait behind it.
            if kind == "chat" {
                let g_month_key = format!("grounding:{}", &today_utc()[..7]);
                let g_used: u64 = db.load(&g_month_key).and_then(|s| s.parse().ok()).unwrap_or(0);
                let grounding_free = chat::GROUNDING_FREE_PER_MONTH.saturating_sub(g_used);
                match chat::reserve(&m.message, &mut sessions, &mut uploads, &pricing, margin, &mut chat_replies, grounding_free) {
                    // Validation error or an idempotent replay hit — no provider call, and
                    // reserve() never mutates the money state on this path.
                    chat::Reserved::Reply(response) => {
                        if let Err(e) = reply_sender.send_reply(tag, response).await {
                            eprintln!("scrai-server: chat reply failed: {e}");
                        }
                    }
                    // Reserved → run the provider off the loop; settle() prices it later.
                    chat::Reserved::Proceed(pending) => {
                        let tx = http_tx.clone();
                        let slots = chat_slots.clone();
                        let guard = inflight.enter(tag);
                        note_peak(&db, &inflight, &mut peak_written);
                        tokio::spawn(async move {
                            let result = match tokio::time::timeout(QUEUE_WAIT, slots.acquire_owned()).await {
                                // The permit lives for the whole provider call.
                                Ok(Ok(_permit)) => chat::run_provider(&pending).await,
                                _ => Err("the server is busy with too many chats right now — please try again in a moment".to_string()),
                            };
                            let _ = tx.send(HttpDone { pending: *pending, result, tag, _guard: guard }).await;
                        });
                    }
                }
                continue;
            }
            // H2 (pay): authenticate/throttle on the loop, then run the gateway HTTP in a
            // spawned task — its result comes back via pay_tx and finish() runs here. A slow
            // LCD node used to stall every chat reserve/settle for up to 15 s.
            if matches!(kind.as_str(), "invoice.create" | "invoice.status" | "invoice.cancel" | "entitlement") {
                let response = match paywall.begin(&m.message, &gateway) {
                    pay::PayStep::Reply(response) => response,
                    pay::PayStep::Pending(pending) => {
                        let (tx, gw, slots) = (pay_tx.clone(), gateway.clone(), gateway_slots.clone());
                        let guard = inflight.enter(tag);
                        note_peak(&db, &inflight, &mut peak_written);
                        tokio::spawn(async move {
                            let outcome = match tokio::time::timeout(QUEUE_WAIT, slots.acquire_owned()).await {
                                Ok(Ok(_permit)) => pay::run_gateway(pending, &gw).await,
                                _ => pay::gateway_busy(pending),
                            };
                            let _ = tx.send(PayDone { outcome, tag, _guard: guard }).await;
                        });
                        continue;
                    }
                };
                // Cancel / validation error: nothing outbound, but the nonce burn or the
                // cancelled invoice must still hit disk before the ack.
                let (c0, c1) = label_color(&kind);
                println!("scrai-server: handled {c0}{kind}{c1} ({} → {} bytes)", m.message.len(), response.len());
                persist_changed(&mut db, &sessions, &quorum, &paywall, &mut saved);
                if let Err(e) = reply_sender.send_reply(tag, response).await {
                    eprintln!("scrai-server: reply failed: {e}");
                }
                continue;
            }
            let response = match kind.as_str() {
                "upload.begin" | "upload.chunk" => uploads.handle(&m.message),
                "image.chunk" => staged.handle(&m.message),
                // Coconut issuance is gated by the paywall: a Withdraw must be
                // account-signed and backed by a ticketbook's worth of paid
                // entitlement, which is consumed only if issuance succeeds.
                "coconut" => match paywall.gate_withdraw(&m.message, book_scrai) {
                    pay::Gate::Denied(reply) => reply,
                    pay::Gate::NotAWithdraw => {
                        scrai_core::gateway::handle(&authority, &mut quorum, &mut sessions, &m.message).await
                    }
                    pay::Gate::Authorized { account_id } => {
                        let resp =
                            scrai_core::gateway::handle(&authority, &mut quorum, &mut sessions, &m.message).await;
                        let issued = serde_json::from_slice::<serde_json::Value>(&resp)
                            .ok()
                            .is_some_and(|r| r.pointer("/fed/Withdraw").is_some());
                        if issued {
                            paywall.consume_entitlement(&account_id, book_scrai);
                        }
                        resp
                    }
                },
                _ => scrai_core::gateway::handle(&authority, &mut quorum, &mut sessions, &m.message).await,
            };
            // Label each line with the request kind so the log reads as a story;
            // coconut envelopes additionally name their federation op.
            let fed_op = serde_json::from_slice::<serde_json::Value>(&m.message)
                .ok()
                .and_then(|v| {
                    v.get("fed")
                        .and_then(|f| f.as_object())
                        .and_then(|o| o.keys().next().cloned())
                });
            let label = match (kind.as_str(), fed_op) {
                ("", None) => "unknown".to_string(),
                ("", Some(op)) => format!("fed.{op}"),
                (k, Some(op)) => format!("{k}.{op}"),
                (k, None) => k.to_string(),
            };
            let (c0, c1) = label_color(&label);
            println!(
                "scrai-server: handled {c0}{label}{c1} ({} → {} bytes)",
                m.message.len(),
                response.len()
            );
            // DURABILITY: persist any changed state BEFORE acknowledging, so a crash
            // after the reply can't lose a credit the client already advanced its purse
            // for. Re-save only what actually changed (revision advanced).
            // DURABILITY (H2): persist every changed store in ONE transaction, BEFORE
            // acknowledging — so a session credit and the burned-coin serial that backs
            // it commit together (never one without the other), and a crash after the
            // reply can't lose a credit the client already advanced its purse for.
            persist_changed(&mut db, &sessions, &quorum, &paywall, &mut saved);
                    if let Err(e) = reply_sender.send_reply(tag, response).await {
                        eprintln!("scrai-server: reply failed: {e}");
                    }
                } // for m in messages
            } // batch = wait_for_messages
        } // tokio::select!
    } // loop

    // Disconnect flushes the Nym client's persistent stores (notably the
    // reply-SURB sqlite) so the next start finds them consistent.
    client.disconnect().await;
    println!("scrai-server: clean shutdown — mixnet state flushed.");
}

/// Revision marks of the last persisted snapshot per store.
struct SavedRevs {
    sessions: u64,
    quorum: u64,
    pay: u64,
}

/// Persist every store whose revision moved, in ONE transaction, BEFORE the reply is
/// acknowledged — so a session credit and the burned-coin serial that backs it commit
/// together (never one without the other), and a crash after the reply can't lose a
/// credit the client already advanced its purse for (H2). On failure the marks stay
/// unadvanced so the change remains dirty and is retried on the next request.
fn persist_changed(
    db: &mut store::Store,
    sessions: &SessionStore,
    quorum: &QuorumStore,
    paywall: &pay::Pay,
    saved: &mut SavedRevs,
) {
    let sess_snap = (sessions.revision() != saved.sessions).then(|| sessions.snapshot());
    let quorum_snap = (quorum.revision() != saved.quorum).then(|| quorum.snapshot());
    let pay_snap = (paywall.revision() != saved.pay).then(|| paywall.snapshot());
    let mut changed: Vec<(&str, &str)> = Vec::new();
    if let Some(s) = &sess_snap {
        changed.push(("sessions", s));
    }
    if let Some(s) = &quorum_snap {
        changed.push(("quorum", s));
    }
    if let Some(s) = &pay_snap {
        changed.push(("pay", s));
    }
    if changed.is_empty() {
        return;
    }
    match db.save_many(&changed) {
        Ok(()) => {
            saved.sessions = sessions.revision();
            saved.quorum = quorum.revision();
            saved.pay = paywall.revision();
        }
        Err(e) => eprintln!("scrai-server: atomic persist failed (will retry): {e}"),
    }
}

/// Record today's peak of simultaneously served clients — one sqlite write per NEW high
/// (or per day), nothing on the steady state.
fn note_peak(db: &store::Store, inflight: &inflight::Inflight<AnonymousSenderTag>, written: &mut (String, usize)) {
    let now = inflight.current();
    let today = today_utc();
    if written.0 != today || now > written.1 {
        db.bump_peak(&today, now);
        *written = (today, now);
    }
}

/// A positive usize from the environment, or the default.
fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).filter(|n| *n > 0).unwrap_or(default)
}

/// Load the persisted authority, or bootstrap + persist one on first run.
fn load_or_bootstrap(path: &Path) -> Authority {
    if let Ok(json) = std::fs::read_to_string(path) {
        match Authority::restore(&json) {
            Ok(a) => return a,
            Err(e) => eprintln!("scrai-server: couldn't restore authority ({e}) — re-bootstrapping"),
        }
    }
    let authority = federation::bootstrap(AUTHORITY_N, AUTHORITY_N as u64, TICKETBOOK_COINS, future_expiration_date())
        .expect("bootstrap authority")
        .into_iter()
        .next()
        .expect("one authority");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    write_secret(path, &authority.persist().expect("persist authority"))
        .expect("write authority file");
    println!("scrai-server: bootstrapped a fresh authority → {}", path.display());
    authority
}

/// Write a secret file (the authority share — it can forge money) with owner-only
/// 0600 permissions on Unix, instead of the umask default (~0644) (H10).
#[cfg(unix)]
fn write_secret(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents.as_bytes())?;
    // mode() only applies on creation — also tighten a pre-existing looser file.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}
#[cfg(not(unix))]
fn write_secret(path: &Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

/// The metrics day the current moment falls into, as `YYYY-MM-DD`, in the operator's
/// chosen zone (`SCRAI_METRICS_TZ`, default UTC). Pick the zone your provider's billing
/// view uses so `scrai-admin` and the provider agree per day: Google's Cloud Billing
/// reports bucket by America/Los_Angeles; the AI Studio dashboard by the browser's local
/// time (Europe/Berlin for us). Pure integer math (Howard Hinnant's civil-date
/// algorithms), DST rules for the two zones we care about — no date crate on the server.
fn today_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    civil_day(secs + metrics_tz_offset(secs))
}

/// The zone name as configured (echoed to scrai-admin's table header).
fn metrics_tz_name() -> String {
    std::env::var("SCRAI_METRICS_TZ").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "UTC".into())
}

/// Seconds to ADD to UTC for the configured metrics zone at instant `secs`.
fn metrics_tz_offset(secs: i64) -> i64 {
    let tz = metrics_tz_name();
    match tz.trim() {
        "UTC" | "Etc/UTC" | "Z" => 0,
        // EU rule: CEST from the last Sunday of March 01:00 UTC to the last Sunday of October 01:00 UTC.
        "Europe/Berlin" | "Europe/Vienna" | "Europe/Zurich" | "Europe/Paris" | "Europe/Amsterdam" | "Europe/Rome" | "Europe/Madrid" => {
            let (y, _, _) = civil_from_days(secs.div_euclid(86_400));
            let start = last_sunday(y, 3) * 86_400 + 3_600;
            let end = last_sunday(y, 10) * 86_400 + 3_600;
            if secs >= start && secs < end { 7_200 } else { 3_600 }
        }
        // US rule: PDT from the second Sunday of March 02:00 local (10:00 UTC) to the
        // first Sunday of November 02:00 local (09:00 UTC).
        "America/Los_Angeles" | "US/Pacific" | "PST8PDT" => {
            let (y, _, _) = civil_from_days(secs.div_euclid(86_400));
            let start = nth_sunday(y, 3, 2) * 86_400 + 10 * 3_600;
            let end = nth_sunday(y, 11, 1) * 86_400 + 9 * 3_600;
            if secs >= start && secs < end { -7 * 3_600 } else { -8 * 3_600 }
        }
        // fixed offset: "+0200", "-0800", "+02:00"
        s => {
            let t = s.replace(':', "");
            let sign = if t.starts_with('-') { -1 } else { 1 };
            let digits: String = t.trim_start_matches(['+', '-']).chars().take(4).collect();
            match (digits.get(0..2).and_then(|h| h.parse::<i64>().ok()), digits.get(2..4).and_then(|m| m.parse::<i64>().ok())) {
                (Some(h), Some(m)) if h <= 14 && m < 60 => sign * (h * 3_600 + m * 60),
                _ => {
                    eprintln!("scrai-server: SCRAI_METRICS_TZ={s:?} not understood — using UTC");
                    0
                }
            }
        }
    }
}

/// `YYYY-MM-DD` for a (zone-shifted) unix timestamp.
fn civil_day(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 of the last Sunday of `month` in `year`.
fn last_sunday(year: i64, month: u32) -> i64 {
    let (ny, nm) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let last = days_from_civil(ny, nm, 1) - 1;
    last - (last + 4).rem_euclid(7) // 1970-01-01 was a Thursday (4)
}

/// Days since 1970-01-01 of the `n`-th Sunday of `month` in `year`.
fn nth_sunday(year: i64, month: u32, n: i64) -> i64 {
    let first = days_from_civil(year, month, 1);
    let first_sunday = first + (7 - (first + 4).rem_euclid(7)) % 7;
    first_sunday + (n - 1) * 7
}

/// Howard Hinnant's days_from_civil.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Howard Hinnant's civil_from_days → (year, month, day).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = era * 400 + yoe + if m <= 2 { 1 } else { 0 };
    (y, m as u32, d as u32)
}

#[cfg(test)]
mod metrics_day_tests {
    use super::*;
    fn at(y: i64, m: u32, d: u32, h: i64) -> i64 { days_from_civil(y, m, d) * 86_400 + h * 3_600 }
    #[test]
    fn civil_round_trips() {
        for &(y, m, d) in &[(1970, 1, 1), (2000, 2, 29), (2026, 8, 28), (2026, 12, 31), (2100, 3, 1)] {
            assert_eq!(civil_from_days(days_from_civil(y, m, d)), (y, m, d));
        }
        assert_eq!(civil_day(at(2026, 8, 28, 10)), "2026-08-28");
    }
    #[test]
    fn dst_rules_2026() {
        // EU: 2026-03-29 and 2026-10-25 are the last Sundays; switch at 01:00 UTC
        assert_eq!(civil_from_days(last_sunday(2026, 3)), (2026, 3, 29));
        assert_eq!(civil_from_days(last_sunday(2026, 10)), (2026, 10, 25));
        // US: second Sunday of March 2026 = 8th, first Sunday of November = 1st
        assert_eq!(civil_from_days(nth_sunday(2026, 3, 2)), (2026, 3, 8));
        assert_eq!(civil_from_days(nth_sunday(2026, 11, 1)), (2026, 11, 1));
    }
    #[test]
    fn berlin_and_pacific_offsets() {
        std::env::set_var("SCRAI_METRICS_TZ", "Europe/Berlin");
        assert_eq!(metrics_tz_offset(at(2026, 8, 28, 10)), 7_200);   // summer
        assert_eq!(metrics_tz_offset(at(2026, 1, 15, 10)), 3_600);   // winter
        assert_eq!(metrics_tz_offset(at(2026, 3, 29, 0)), 3_600);    // an hour before the switch
        assert_eq!(metrics_tz_offset(at(2026, 3, 29, 1)), 7_200);    // at the switch
        // 23:30 UTC on the 27th is already the 28th in Berlin
        assert_eq!(civil_day(at(2026, 8, 27, 23) + 1_800 + metrics_tz_offset(at(2026, 8, 27, 23))), "2026-08-28");
        std::env::set_var("SCRAI_METRICS_TZ", "America/Los_Angeles");
        assert_eq!(metrics_tz_offset(at(2026, 8, 28, 10)), -7 * 3_600);
        assert_eq!(metrics_tz_offset(at(2026, 12, 1, 10)), -8 * 3_600);
        // 03:00 UTC on the 28th is still the 27th in Los Angeles
        assert_eq!(civil_day(at(2026, 8, 28, 3) + metrics_tz_offset(at(2026, 8, 28, 3))), "2026-08-27");
        std::env::set_var("SCRAI_METRICS_TZ", "+02:00");
        assert_eq!(metrics_tz_offset(0), 7_200);
        std::env::set_var("SCRAI_METRICS_TZ", "UTC");
        assert_eq!(metrics_tz_offset(0), 0);
    }
}

/// ANSI color pair (start, reset) for a request label — one color per protocol
/// area so the log reads at a glance. Colors only when stdout is a real TTY:
/// under systemd, escape codes would make journalctl hide lines as "blob data".
fn label_color(label: &str) -> (&'static str, &'static str) {
    use std::io::IsTerminal;
    static TTY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*TTY.get_or_init(|| std::io::stdout().is_terminal()) {
        return ("", "");
    }
    let color = match label.split('.').next().unwrap_or("") {
        "chat" => "\x1b[32m",                                    // green — the product
        "models" => "\x1b[36m",                                  // cyan — catalog
        "upload" => "\x1b[96m",                                  // bright cyan — chat adjacent
        "invoice" | "entitlement" => "\x1b[33m",                 // yellow — money in
        "coconut" | "fed" | "redeem" | "spend" => "\x1b[35m",    // magenta — ecash
        "session" => "\x1b[34m",                                 // blue — bookkeeping
        _ => "\x1b[90m",                                         // grey — unknown
    };
    (color, "\x1b[0m")
}

/// A random gateway among the WELL-DESCRIBED Nym directory nodes: entry role
/// plus a self-reported location AND a real hostname (reverse DNS). This is the
/// gateway clients will see as the route's exit, so an anonymous, IP-only node
/// would show up as "??" in their UI. Returns (identity, country, host); None
/// if the directory is unreachable or the curated pool is empty.
async fn random_described_gateway() -> Option<(String, String, String)> {
    let body: serde_json::Value = crate::http::client()
        .get("https://validator.nymtech.net/api/v1/nym-nodes/described")
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let items = body.get("data")?.as_array()?;
    let pool: Vec<(String, String, String)> = items
        .iter()
        .filter_map(|it| {
            let d = it.get("description")?;
            if !d.pointer("/declared_role/entry")?.as_bool()? {
                return None;
            }
            let id = d.pointer("/host_information/keys/ed25519")?.as_str()?;
            let country = d.pointer("/auxiliary_details/location")?.as_str()?;
            let host = d.pointer("/host_information/hostname")?.as_str()?;
            if id.is_empty() || country.is_empty() || host.is_empty() {
                return None;
            }
            // A bare IP in the hostname field is not a reverse-DNS name.
            if host.parse::<std::net::IpAddr>().is_ok() {
                return None;
            }
            Some((id.to_string(), country.to_string(), host.to_string()))
        })
        .collect();
    use rand::seq::SliceRandom;
    pool.choose(&mut rand::thread_rng()).cloned()
}

/// Load the pricing table: a file override (`SCRAI_PRICING`) if set, else the copy
/// embedded at build time — so the server always has a valid table.
fn load_pricing() -> PricingTable {
    const EMBEDDED: &str = include_str!("../../pricing.json");
    let from_file = std::env::var("SCRAI_PRICING")
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok());
    let json = from_file.as_deref().unwrap_or(EMBEDDED);
    PricingTable::parse(json).unwrap_or_else(|e| {
        eprintln!("scrai-server: pricing parse failed ({e}) — using embedded table");
        PricingTable::parse(EMBEDDED).expect("embedded pricing.json is valid")
    })
}

/// Retail margin from the MARGIN env (clamped ≥ 1), default 1.4. Never in the table.
fn pricing_margin() -> f64 {
    std::env::var("MARGIN")
        .ok()
        .and_then(|m| m.parse::<f64>().ok())
        .map(scrai_core::billing::clamp_margin)
        .unwrap_or(1.4)
}

/// A day-aligned (00:00:00 UTC) expiration ~30 days out, as the scheme requires.
fn future_expiration_date() -> u32 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    const DAY: u64 = 86_400;
    let today_midnight = (now / DAY) * DAY;
    (today_midnight + 30 * DAY) as u32
}
