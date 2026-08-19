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

mod catalog;
mod chat;
mod http;
mod nyx;
mod pay;
mod store;
mod uploads;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use nym_sdk::mixnet::{MixnetClientBuilder, MixnetMessageSender, StoragePaths};
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
    let storage = StoragePaths::new_from_dir(&data_dir.join(".nym-server"))
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

    println!(
        "scrai-server: authority #{} live on the mixnet.\n  address: {}\n  (point a client at this address)",
        authority.index(),
        client.nym_address()
    );

    // Durable state: session balances + double-spend records survive a restart.
    let db = store::Store::open(&data_dir.join("state.db")).expect("open state db");
    let mut quorum = db
        .load("quorum")
        .map(|j| QuorumStore::from_snapshot(&j))
        .unwrap_or_default();
    let mut sessions = db
        .load("sessions")
        .map(|j| SessionStore::from_snapshot(&j))
        .unwrap_or_default();
    let mut last_quorum_rev = quorum.revision();
    let mut last_sessions_rev = sessions.revision();
    println!(
        "scrai-server: state loaded (quorum rev {}, sessions rev {})",
        last_quorum_rev, last_sessions_rev
    );

    // Per-model pricing (USD/1M) + retail margin — drives the catalog rates AND chat
    // billing, so displayed price == charged price.
    let pricing = load_pricing();
    let margin = pricing_margin();
    println!(
        "scrai-server: pricing table {} (margin {margin})",
        pricing.version()
    );

    // Staged vision-image uploads (chunked over the mixnet, consumed by chat).
    // Ephemeral by design — never persisted.
    let mut uploads = uploads::UploadStore::default();

    // The paywall: invoices + entitlements + burned nonces (durable), and the
    // payment gateway it raises invoices on.
    let gateway = pay::Gateway::from_env();
    let mut paywall = db
        .load("pay")
        .map(|j| pay::Pay::from_snapshot(&j))
        .unwrap_or_default();
    let mut last_pay_rev = paywall.revision();
    let book_scrai = TICKETBOOK_COINS * scrai_core::coconut::COIN_SCRAI;
    println!(
        "scrai-server: gateway {} · ticketbook {} coins ({} SCRAI)",
        gateway.name(),
        TICKETBOOK_COINS,
        book_scrai
    );

    // Serve forever: receive → dispatch → reply via the request's SURB.
    // Graceful shutdown: Ctrl+C (dev) and SIGTERM (systemd stop) break the loop
    // so `client.disconnect()` runs — that is what flushes the reply-SURB store
    // to disk. A hard kill instead leaves the sqlite mid-write and the next
    // start logs "loaded data is inconsistent" and rebuilds it from scratch.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");

    loop {
        let messages = tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("scrai-server: Ctrl+C — shutting down");
                break;
            }
            _ = sigterm.recv() => {
                println!("scrai-server: SIGTERM — shutting down");
                break;
            }
            batch = client.wait_for_messages() => {
                let Some(messages) = batch else {
                    eprintln!("scrai-server: mixnet stream ended");
                    break;
                };
                messages
            }
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
            let response = match kind.as_str() {
                "chat" => chat::handle(&m.message, &mut sessions, &mut uploads, &pricing, margin).await,
                "models" => catalog::handle(&m.message, &pricing, margin).await,
                "upload.begin" | "upload.chunk" => uploads.handle(&m.message),
                "invoice.create" | "invoice.status" | "invoice.cancel" | "entitlement" => {
                    paywall.handle(&m.message, &gateway).await
                }
                // Coconut issuance is gated by the paywall: a Withdraw must be
                // account-signed and backed by a ticketbook's worth of paid
                // entitlement, which is consumed only if issuance succeeds.
                "coconut" => match paywall.gate_withdraw(&m.message, book_scrai) {
                    pay::Gate::Denied(reply) => reply,
                    pay::Gate::NotAWithdraw => {
                        scrai_core::gateway::handle(&authority, &mut quorum, &mut sessions, &m.message)
                    }
                    pay::Gate::Authorized { account_id } => {
                        let resp =
                            scrai_core::gateway::handle(&authority, &mut quorum, &mut sessions, &m.message);
                        let issued = serde_json::from_slice::<serde_json::Value>(&resp)
                            .ok()
                            .is_some_and(|r| r.pointer("/fed/Withdraw").is_some());
                        if issued {
                            paywall.consume_entitlement(&account_id, book_scrai);
                        }
                        resp
                    }
                },
                _ => scrai_core::gateway::handle(&authority, &mut quorum, &mut sessions, &m.message),
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
            if sessions.revision() != last_sessions_rev {
                match db.save("sessions", &sessions.snapshot()) {
                    Ok(()) => last_sessions_rev = sessions.revision(),
                    Err(e) => eprintln!("scrai-server: persist sessions failed: {e}"),
                }
            }
            if quorum.revision() != last_quorum_rev {
                match db.save("quorum", &quorum.snapshot()) {
                    Ok(()) => last_quorum_rev = quorum.revision(),
                    Err(e) => eprintln!("scrai-server: persist quorum failed: {e}"),
                }
            }
            if paywall.revision() != last_pay_rev {
                match db.save("pay", &paywall.snapshot()) {
                    Ok(()) => last_pay_rev = paywall.revision(),
                    Err(e) => eprintln!("scrai-server: persist paywall failed: {e}"),
                }
            }
            if let Err(e) = client.send_reply(tag, response).await {
                eprintln!("scrai-server: reply failed: {e}");
            }
        }
    }

    // Disconnect flushes the Nym client's persistent stores (notably the
    // reply-SURB sqlite) so the next start finds them consistent.
    client.disconnect().await;
    println!("scrai-server: clean shutdown — mixnet state flushed.");
}

/// Load the persisted authority, or bootstrap + persist one on first run.
fn load_or_bootstrap(path: &Path) -> Authority {
    if let Ok(json) = std::fs::read_to_string(path) {
        match Authority::restore(&json) {
            Ok(a) => return a,
            Err(e) => eprintln!("scrai-server: couldn't restore authority ({e}) — re-bootstrapping"),
        }
    }
    let authority = federation::bootstrap(1, 1, TICKETBOOK_COINS, future_expiration_date())
        .expect("bootstrap authority")
        .into_iter()
        .next()
        .expect("one authority");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(path, authority.persist().expect("persist authority"))
        .expect("write authority file");
    println!("scrai-server: bootstrapped a fresh authority → {}", path.display());
    authority
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
