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
use scrai_server::{catalog, chat, http, iap, inflight, pay, replies, store, uploads};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use nym_sdk::mixnet::{AnonymousSenderTag, MixnetClient, MixnetClientBuilder, MixnetClientSender, MixnetMessageSender, ReconstructedMessage, StoragePaths};
use tokio::sync::Semaphore;
use scrai_core::federation;
use scrai_core::pricing::PricingTable;
use scrai_core::quorum::QuorumStore;
use scrai_core::session::SessionStore;

// One issued ticketbook = 1000 coins × 100 TOKU = 100,000 TOKU = $1, the smallest
// thing this server sells. Every tier is a whole number of books ($5 = 5, $50 = 50).
//
// It was 500 ($5) on mainnet and 100 on testnet, and that split does not survive the
// invite rail: a withdrawal is always a WHOLE book, so a $1 invite credit on a 500-coin
// server can never be withdrawn — it sits as entitlement the tester cannot spend
// (gate_withdraw: "a ticketbook costs 500000 TOKU, this account holds 100000"). One size
// everywhere, and the smallest book equals the smallest credit.
//
// The cost is round trips: collecting is one withdrawal per book, so $50 is 50 of them.
// Worth revisiting by pipelining `collect`, NOT by growing the book again — the size is
// baked into the authority keys, so changing it needs a fresh bootstrap and invalidates
// every purse clients hold. (Operator decision, 2026-09-05, taken while bootstrapping
// the mainnet authority so it cost nothing.)
/// Coins per ticketbook: 10 × 0.1 ¢ = $0.01.
///
/// The book is the unit credit moves from the account onto a device in, so it bounds what
/// is left stranded on the account when the rest is smaller than one book — at a cent,
/// that is nothing anyone notices. It costs nothing elsewhere: the app draws a hundred at
/// a time in ONE round trip (Nym's `--amount`, the same idea), a book is 641 bytes in the
/// wallet since the epoch material is held once beside the books rather than inside each,
/// and the keys reply every client fetches once per epoch is a few kilobytes instead of
/// the 207 KB that thousand-coin books cost (all measured 2026-09-14).
const TICKETBOOK_COINS: u64 = 10;

/// Coins per ticketbook. Baked into the authority keys, so it is checked against the
/// persisted authority at boot (see `load_or_bootstrap`).
fn ticketbook_coins() -> u64 {
    TICKETBOOK_COINS
}

/// Number of issuing authorities this build runs. 1 = a single trusted-dealer
/// authority (testnet bring-up): it can forge unlimited credentials, so issuing
/// against REAL money is gated (see the H9 interlock in `main`). A real production
/// federation is t-of-n (≥ 2) with a shared DKG — bump this and wire the shares.
const AUTHORITY_N: usize = 1;

#[tokio::main]
async fn main() {
    // Load provider keys etc. from .env (our own lenient parser — see load_env_lenient).
    load_env_lenient();

    eprintln!("scrai-server: v{}", scrai_server::VERSION);

    // A real-money server must not run on test payment infrastructure. See
    // `testnet_rails_on_mainnet` for why this cannot be left to the operator's memory.
    if !pay::is_testnet_server() {
        let stale = scrai_server::testnet_rails_on_mainnet();
        if !stale.is_empty() {
            let wanted: Vec<String> = stale.iter().map(|b| format!("{b}_MAINNET")).collect();
            eprintln!(
                "scrai-server: FATAL: TESTNET is off, but these rails only have a \
                 _TESTNET value and would run against TEST infrastructure while real money \
                 is accepted: {}. Set {} (or delete the _TESTNET one if the rail is unused). \
                 A bare name does NOT count — a _TESTNET value outranks it. Check the line is \
                 not still commented out. Mollie in particular would credit real balance for \
                 a free test-checkout payment.",
                stale.join(", "),
                wanted.join(", ")
            );
            std::process::exit(1);
        }
    }

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

    // Load-test mock provider: loud when active, loud when set but refused.
    if scrai_server::cfg("MOCK_PROVIDER").is_ok() {
        match chat::mock_provider() {
            Some((d, c)) => eprintln!(
                "scrai-server: MOCK PROVIDER ACTIVE — every chat answers a canned {c}-char text after {d} ms; \
                 no model is called (MOCK_PROVIDER, load testing only)"
            ),
            None => eprintln!(
                "scrai-server: MOCK_PROVIDER is set but IGNORED — it only works together with FAKE_PAYMENTS=1"
            ),
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
    let data_dir = PathBuf::from(scrai_server::cfg("DATA").unwrap_or_else(|_| "./data".into()));
    // One authority per denomination (server/src/mint.rs). The fine one keeps the old
    // file name, so an existing deployment only gains the coarse one beside it.
    let mint = Arc::new(scrai_server::mint::load(
        &data_dir,
        ticketbook_coins(),
        future_expiration_date(),
        AUTHORITY_N,
    ));
    let authority = mint.fine().clone();

    // Persistent Nym identity so the server keeps ONE address across restarts.
    // Entry gateways. GATEWAY_MASTER pins the primary identity (the address the app
    // ships with); GATEWAY_FALLBACK=gw1,gw2,… pins the extra identities #1, #2, … —
    // the same server on other gateways, which the app learns from the catalog reply and
    // falls back to when the master's gateway is down. The number of identities follows
    // from that list (MIX_CLIENTS only overrides it). GATEWAY is the legacy
    // name of the master pin.
    //
    // A pin is applied on EVERY start: `request_gateway` re-registers an existing identity
    // at the requested gateway (same keys, so the address only changes its `@gateway`
    // part). Which is also why an UNPINNED identity that already exists must NOT get a
    // random pick — it would move to a new gateway (= a new address) on every restart, as
    // the fallback slots once did (2026-09-03). Unpinned + existing = keep; unpinned + new
    // = curated random.
    let fallback_gateways: Vec<String> = scrai_server::cfg("GATEWAY_FALLBACK")
        .unwrap_or_default()
        .split(',')
        .map(|g| g.trim().to_string())
        .filter(|g| !g.is_empty())
        .collect();
    // Through cfg(), like everything else: read raw this missed a .env that still spells
    // the key SCRAI_GATEWAY_MASTER, and the pin then quietly did nothing (the existing
    // identity keeps its gateway, so it only shows up the day one is recreated).
    let pinned = ["GATEWAY_MASTER", "GATEWAY"]
        .iter()
        .find_map(|k| scrai_server::cfg(k).ok().map(|g| g.trim().to_string()).filter(|g| !g.is_empty()));
    let primary_exists = identity_exists(&data_dir.join(".nym-server"));
    let primary_gateway: Option<String> = if let Some(gw) = pinned {
        println!("scrai-server: requesting entry gateway {gw}");
        Some(gw)
    } else if primary_exists {
        println!("scrai-server: no gateway pin — the existing identity keeps its gateway");
        None
    } else if let Some((gw, country, host)) = random_described_gateway().await {
        // No pin → curated random instead of the SDK's blind pick: only gateways
        // whose directory entry carries a location AND a reverse-DNS hostname, so
        // the exit the clients see is always identifiable in their UI.
        println!("scrai-server: picked described gateway {gw} ({country}, {host})");
        Some(gw)
    } else {
        println!("scrai-server: directory unavailable — letting the SDK pick a gateway");
        None
    };
    // Egress rate. The SDK default (one real packet every 20 ms ≈ 50 packets/s, the
    // privacy-preserving stream shape) is a CLIENT default: a service provider that
    // answers hundreds of users through ONE Nym client serialises every reply behind it —
    // a 109 KB coconut Keys reply alone is ~55 packets ≈ 1.1 s of the whole server's send
    // budget. MIX_SEND_MS lowers the per-packet delay (Nym's own "high traffic
    // volume" preset is 4 ms ≈ 250 packets/s); MIX_COVER_MS thins the loop cover
    // stream that a server does not need for its own anonymity. Unset = SDK defaults.
    // Measured in docs/load-testing.md.
    let client = connect_identity_at_boot(&data_dir.join(".nym-server"), primary_gateway, "primary").await;

    println!(
        "scrai-server: authority #{} live on the mixnet.\n  address: {}\n  (point a client at this address)",
        authority.index(),
        client.nym_address()
    );

    // MIX_CLIENTS=K: K−1 EXTRA Nym identities (data/.nym-server-1 …), each on a
    // different entry gateway, all feeding the SAME dispatch loop and state below. Every
    // packet for one identity funnels through one gateway and one Sphinx client; the load
    // test showed that path — not CPU — is what saturates first (docs/load-testing.md).
    // Extra addresses are more front doors to the same server: nothing about money
    // changes. The primary identity/address above is untouched, so existing clients
    // keep working; a missing K means 1 (today's behaviour).
    let n_clients = env_usize("MIX_CLIENTS", 1 + fallback_gateways.len()).max(1);
    // Identity #k takes fallback entry k−1 (see above); a missing entry falls back to the
    // curated random pick. Operator-run gateways = monitorable, reproducible.
    let mut clients = vec![client];
    let mut used_gateways: Vec<String> = clients.iter().map(|c| c.nym_address().gateway().to_base58_string()).collect();
    for k in 1..n_clients {
        let dir_k = data_dir.join(format!(".nym-server-{k}"));
        let gateway_k: Option<String> = if let Some(gw) = fallback_gateways.get(k - 1) {
            println!("scrai-server: client #{k}: requesting entry gateway {gw} (GATEWAY_FALLBACK)");
            Some(gw.clone())
        } else if identity_exists(&dir_k) {
            println!("scrai-server: client #{k}: no gateway pin — the existing identity keeps its gateway");
            None
        } else {
            match random_described_gateway_excluding(&used_gateways).await {
                Some((gw, country, host)) => {
                    println!("scrai-server: client #{k}: picked described gateway {gw} ({country}, {host})");
                    Some(gw)
                }
                None => {
                    println!("scrai-server: client #{k}: directory unavailable — letting the SDK pick a gateway");
                    None
                }
            }
        };
        let c = connect_identity_at_boot(&dir_k, gateway_k, &format!("client #{k}")).await;
        println!("  address[{k}]: {}", c.nym_address());
        used_gateways.push(c.nym_address().gateway().to_base58_string());
        clients.push(c);
    }
    // Every address this server answers on, one per line — for the operator (no journal
    // access needed) and as the raw material of the signed directory later.
    let all_addresses: Vec<String> = clients.iter().map(|c| c.nym_address().to_string()).collect();
    if let Err(e) = std::fs::write(data_dir.join("addresses.txt"), all_addresses.join("\n") + "\n") {
        eprintln!("scrai-server: could not write addresses.txt: {e}");
    }
    if n_clients > 1 {
        let mut g = used_gateways.clone();
        g.sort();
        g.dedup();
        println!("scrai-server: {n_clients} mixnet identities on {} distinct gateways", g.len());
        if g.len() < n_clients {
            // Two front doors on one gateway fail together — the whole point of the extra
            // identity is lost. Happens when an identity registered before the list was
            // right; the fix is to re-home that slot (delete its data/.nym-server-k).
            eprintln!(
                "scrai-server: WARNING: identities share a gateway (wanted {n_clients} distinct) — \
                 check GATEWAY_MASTER / GATEWAY_FALLBACK and re-home the duplicate \
                 slot by deleting its data/.nym-server-k before the next start"
            );
        }
    }

    // H2: cloneable senders let spawned tasks reply concurrently without borrowing a
    // client. A reply MUST leave through the client that received the request (its reply
    // SURBs live in that client's store), hence `ReplyTo { idx, tag }` everywhere below.
    // RwLock per identity: a reconnected identity swaps its sender in place (see the
    // receive task below); replies take a read lock for the duration of one send.
    let senders: Arc<Vec<tokio::sync::RwLock<MixnetClientSender>>> =
        Arc::new(clients.iter().map(|c| tokio::sync::RwLock::new(c.split_sender())).collect());
    let identity_dirs: Vec<PathBuf> = (0..clients.len())
        .map(|k| if k == 0 { data_dir.join(".nym-server") } else { data_dir.join(format!(".nym-server-{k}")) })
        .collect();

    // One receive task per client, merged into a single inbound channel for the loop.
    // The tasks own the clients; on shutdown they disconnect (flushing the SURB stores).
    struct Inbound {
        idx: usize,
        msg: ReconstructedMessage,
    }
    let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<Inbound>(1024);
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut recv_tasks = Vec::new();
    for (idx, mut c) in clients.into_iter().enumerate() {
        let tx = in_tx.clone();
        let mut stop = stop_rx.clone();
        let senders = senders.clone();
        let dir = identity_dirs[idx].clone();
        let gateway = c.nym_address().gateway().to_base58_string();
        recv_tasks.push(tokio::spawn(async move {
            loop {
                // Receive until shutdown (false) or the SDK ends the stream (true).
                let ended = loop {
                    tokio::select! {
                        _ = stop.changed() => break false,
                        batch = c.wait_for_messages() => {
                            let Some(messages) = batch else { break true };
                            for msg in messages {
                                if tx.send(Inbound { idx, msg }).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                };
                if !ended {
                    c.disconnect().await;
                    return;
                }
                // The SDK shut this client down — seen in the load test as a panic inside its
                // ack controller under a retransmission storm (nym-client-core 1.21.4), and
                // possible on any gateway drop. Rebuild the SAME identity (same keys, same
                // address) and keep serving; the other identities never noticed. Replies owed
                // to the dead client are lost with its SURBs — the app retries. A reply-SURB
                // store the crash left inconsistent refuses to open; it only holds ephemeral
                // SURBs, so the second attempt wipes it.
                eprintln!("scrai-server: identity #{idx} lost its mixnet stream — reconnecting");
                drop(c);
                let mut attempt = 0u32;
                c = loop {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_secs(if attempt == 1 { 3 } else { 15 })).await;
                    if *stop.borrow() {
                        return;
                    }
                    if attempt >= 2 {
                        for f in ["persistent_reply_store.sqlite", "persistent_reply_store.sqlite-wal", "persistent_reply_store.sqlite-shm"] {
                            let _ = std::fs::remove_file(dir.join(f));
                        }
                    }
                    match connect_identity(&dir, Some(&gateway)).await {
                        Ok(nc) => {
                            *senders[idx].write().await = nc.split_sender();
                            println!("scrai-server: identity #{idx} back on the mixnet (attempt {attempt}): {}", nc.nym_address());
                            break nc;
                        }
                        Err(e) => eprintln!("scrai-server: identity #{idx} reconnect attempt {attempt} failed: {e}"),
                    }
                };
            }
        }));
    }
    drop(in_tx);

    // Durable state: session balances + double-spend records survive a restart.
    let mut db = store::Store::open(&data_dir.join("state.db")).expect("open state db");
    // L1: absent snapshot = fresh start (default); present-but-UNPARSEABLE = FATAL, never a
    // silent reset — a reset double-spend set would reopen every spent coin, and reset
    // balances would erase credit. (Normal writes are valid+atomic, so this only fires on
    // external corruption, and then the operator must act, not the server silently.)
    // Double-spend store: the small part (offenses, blacklist, counters) from its blob, the
    // spent serials + payments through a read-only index over the same file. Rows from
    // before the index existed are indexed once; rows past retention are pruned at boot
    // and every few hours (see `prune_tick`). A legacy whole-store snapshot ("quorum")
    // is refused: run 0.6.2 once to migrate it, this build no longer carries that path.
    if db.load("quorum").is_some() && db.load("quorum_meta").is_none() {
        eprintln!("scrai-server: FATAL: legacy quorum snapshot found — start 0.6.2 once to migrate it, then this build.");
        std::process::exit(1);
    }
    match db.index_legacy_quorum_records() {
        Ok(0) => {}
        Ok(n) => println!("scrai-server: indexed {n} spend record(s) into spent_serials"),
        Err(e) => {
            eprintln!("scrai-server: FATAL: could not index the spend records ({e}) — refusing to start.");
            std::process::exit(1);
        }
    }
    match db.prune_spent(quorum_retain_secs()) {
        Ok((0, _)) => {}
        Ok((rows, coins)) => println!("scrai-server: pruned {rows} spend record(s) / {coins} coins past retention"),
        Err(e) => eprintln!("scrai-server: spend-record prune failed ({e}) — continuing"),
    }
    let serial_index = store::SqliteSerialIndex::open(&data_dir.join("state.db")).unwrap_or_else(|e| {
        eprintln!("scrai-server: FATAL: {e}");
        std::process::exit(1);
    });
    let next_idx = db.next_quorum_idx();
    let mut quorum = match db.load("quorum_meta") {
        // L1: absent = fresh start; present-but-UNPARSEABLE = FATAL, never a silent reset — a
        // reset double-spend set would reopen every spent coin.
        Some(meta) => QuorumStore::from_meta(&meta, Box::new(serial_index), next_idx).unwrap_or_else(|e| {
            eprintln!("scrai-server: FATAL: quorum state present but unparseable ({e}) — refusing \
                to start (a silent reset would reopen every spent coin). Restore a good state.db.");
            std::process::exit(1);
        }),
        None => QuorumStore::with_index(scrai_core::quorum::Policy::default(), Box::new(serial_index), next_idx),
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
    let mut chat_replies: std::collections::HashMap<String, (u64, Vec<u8>, std::time::Instant)> =
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
    let mut saved = SavedRevs {
        sessions: sessions.revision(),
        quorum_meta: quorum.meta_revision(),
        pay: paywall.revision(),
    };
    // A meta from before the lifetime counter: seed it from the rows once and write it now,
    // so the admin's "burned" does not restart at the next spend and pruning cannot shrink it.
    quorum.seed_burned(db.spent_coins_total());
    persist_changed(&mut db, &sessions, &mut quorum, &paywall, &mut saved);
    let books: Vec<String> = mint
        .denoms()
        .iter()
        .map(|d| {
            let book = ticketbook_coins() * d;
            // Integer dollars would print a ten-cent book as "$0".
            format!("{d} TOKU/coin → ${:.2}", book as f64 / scrai_core::coconut::TOKU_PER_USD as f64)
        })
        .collect();
    println!(
        "scrai-server: gateway {} · ticketbooks of {} coins · {}{}",
        gateway.name(),
        ticketbook_coins(),
        books.join(" · "),
        if pay::is_testnet_server() { " · testnet $1 books" } else { "" }
    );
    // Codes sold on the website are bearer money whose only trace here is a fingerprint.
    // Unkeyed, that fingerprint is a 2^60 search anyone holding a backup can finish; keyed,
    // it is nothing without the key. Say which of the two this box is running, at boot,
    // where it cannot be missed — the failure is silent by nature.
    // Said BOTH ways round on purpose. A warning that only prints on failure means a silent
    // log is ambiguous — it reads the same whether the key is fine or the deploy never
    // arrived, which is exactly the question somebody asks after a deploy (2026-09-08).
    println!(
        "scrai-server: App Store purchases — products {:?}, sandbox transactions {}",
        iap::product_ids(),
        if iap::allow_sandbox() { "ACCEPTED (IAP_ALLOW_SANDBOX=1)" } else { "refused" }
    );
    if pay::voucher_key().is_some() {
        println!("scrai-server: voucher codes — fingerprints are keyed (VOUCHER_KEY)");
    } else {
        eprintln!(
            "scrai-server: VOUCHER_KEY is not set (or is under 32 chars) — code purchases on the \
             website are REFUSED. Existing codes still redeem. Set it in .env to enable them."
        );
    }

    // H9: a single (1-of-1) authority can forge unlimited credentials. That is fine for
    // a testnet bring-up but NEVER against real money — refuse to issue unless the
    // operator deliberately overrides. A real t-of-n DKG (AUTHORITY_N ≥ 2) removes this.
    if AUTHORITY_N < 2
        && !gateway.is_fake()
        && scrai_server::cfg("ALLOW_SINGLE_AUTHORITY").as_deref() != Ok("1")
    {
        eprintln!(
            "scrai-server: FATAL: refusing to issue real-money credentials from a single \
             1-of-1 authority (it can forge unlimited coins). For dev use FAKE_PAYMENTS=1; \
             for production run a real t-of-n DKG; to override deliberately (testnet only) set \
             ALLOW_SINGLE_AUTHORITY=1."
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
        to: ReplyTo,
        /// Keeps the client counted as in flight until the reply below has gone out.
        _guard: inflight::Guard<ReplyTo>,
    }
    let (http_tx, mut http_rx) = tokio::sync::mpsc::channel::<HttpDone>(256);
    // Same shape for the paywall: begin() on the loop, gateway HTTP spawned, finish() here.
    struct PayDone {
        outcome: pay::PayOutcome,
        /// None for the background chain watcher: nobody asked, so nobody is answered.
        to: Option<ReplyTo>,
        _guard: Option<inflight::Guard<ReplyTo>>,
    }
    let (pay_tx, mut pay_rx) = tokio::sync::mpsc::channel::<PayDone>(64);
    // Same shape for the two BLS-heavy money ops: a coconut Withdraw issues 500
    // signatures, a redeem verifies O(coins) pairings — ~100 ms to seconds of pure CPU
    // that used to run ON the loop, so 30 buyers in one minute queued behind each other
    // and every chat/status waited with them (load test 2026-09-02). Now: gate + reserve
    // on the loop, crypto in spawn_blocking, apply/persist/reply back here.
    enum CryptoKind {
        Withdraw { id: serde_json::Value, account_id: String, req_key: String, book_toku: u64, result: Result<federation::FedResponse, String> },
        Redeem { id: serde_json::Value, req: scrai_core::gateway::RedeemRequest, verified: Result<(), String> },
        /// Coins handed back to an account (docs/unlinkability.md, block D).
        Return { id: serde_json::Value, account: String, tender: scrai_core::tender::Tender, verified: Result<(), String> },
    }
    struct CryptoDone {
        kind: CryptoKind,
        to: ReplyTo,
        /// (ms waiting for a crypto slot, ms of BLS work) — for the handled line.
        timing: (u128, u128),
        _guard: inflight::Guard<ReplyTo>,
    }
    let (crypto_tx, mut crypto_rx) = tokio::sync::mpsc::channel::<CryptoDone>(64);
    // M-cl-2: Withdraw bodies whose issuance is running right now — a replay that lands
    // meanwhile is told to retry rather than charged or issued twice.
    let mut inflight_withdraws: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Concurrency caps for the spawned slow paths. A chat holds its worst-case
    // reservation while it waits for a slot; past QUEUE_WAIT it fails fast (settle()
    // refunds it) instead of piling up behind a stalled provider. Gateway calls get a
    // smaller pool: an unauthenticated invoice.status must not be able to open hundreds
    // of LCD connections.
    let max_chats = env_usize("MAX_INFLIGHT_CHATS", 64);
    // OpenAI gets its own pool: its rate limits are per org tier, and a throttled OpenAI
    // must not hold Gemini's slots.
    let max_openai = env_usize("MAX_INFLIGHT_OPENAI", 16);
    let max_gateway = env_usize("MAX_INFLIGHT_GATEWAY", 16);
    // BLS work is CPU-bound: cap it at the core count so a purchase storm can't starve
    // the runtime (each permit = one blocking thread busy for up to seconds).
    let max_crypto = env_usize("MAX_INFLIGHT_CRYPTO", std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).max(1));
    let chat_slots = Arc::new(Semaphore::new(max_chats));
    let openai_slots = Arc::new(Semaphore::new(max_openai));
    let gateway_slots = Arc::new(Semaphore::new(max_gateway));
    let crypto_slots = Arc::new(Semaphore::new(max_crypto));
    const QUEUE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
/// How often the server asks the chain about open invoices, and how many it asks about
/// at once. Small on purpose: this runs forever, and a credit that lands within half a
/// minute is indistinguishable from instant to the person waiting.
const WATCH_TICK_SECS: u64 = 15;
const WATCH_PER_TICK: usize = 5;
/// Web orders answered per tick. Small on purpose: each one is a gateway call, and the
/// same slots serve every paying app.
const WEB_ORDERS_PER_TICK: usize = 3;
/// How often those are picked up. Fast, because it is the only wait a buyer sees.
const ORDER_TICK_MS: u64 = 1000;
    println!("scrai-server: concurrency caps — chats {max_chats} (openai {max_openai}), gateway calls {max_gateway}, coconut crypto {max_crypto}");
    if scrai_server::cfg("OPENAI_API_KEY").is_ok_and(|k| !k.trim().is_empty()) {
        println!(
            "scrai-server: OpenAI enabled — moderation prefilter {}, {} strikes/day per session, retention badge {} days, web search ${:.3}/call",
            if chat::openai_prefilter() { "ON" } else { "off" },
            scrai_server::openai::strikes_per_day(),
            scrai_server::openai::retention_days(),
            scrai_server::openai::search_usd_per_call()
        );
    }
    // The invite ($1, faucet-paid) rail exists in both modes: on a testnet server it is
    // the ONLY purchase, on a mainnet server it runs beside real ones for testers.
    match pay::faucet_address() {
        Some(a) => println!(
            "scrai-server: invite credits ${} enabled, settled only from faucet wallet {a}{}",
            pay::TESTNET_USD,
            if pay::is_testnet_server() { " (TESTNET=1 — no other purchase is accepted)" } else { "" }
        ),
        None => eprintln!(
            "scrai-server: no faucet wallet pinned (FAUCET_ADDRESS) — invite credits are refused{}",
            if pay::is_testnet_server() { "; TESTNET=1 means EVERY purchase is refused" } else { "" }
        ),
    }

    // Distinct clients with a spawned request in flight; the daily peak lands in the
    // `daily` table for scrai-admin ("peak clients"). Written only when today's mark
    // rises, so the hot path costs no extra fsync.
    let inflight: inflight::Inflight<ReplyTo> = inflight::Inflight::default();
    let mut peak_written: (String, usize) = (String::new(), 0);
    // Distinct paying SESSIONS seen in the last hour → the day's `peak_1h`. Keyed by the
    // same hashed session id `users` counts, so the two are comparable: the busiest hour can
    // never exceed the day. Never persisted; pruned as it is written.
    //
    // This replaced a 60-second window over SURB reply TAGS (2026-09-07). That one counted
    // neither users nor load: one app is handed several tags over a session, and every
    // catalog fetch, ping and invoice poll carried one without ever being a user — so it
    // routinely showed more "clients" than the day had users, which is what made the column
    // unreadable. Load is what `inflight` (peak_clients) measures, and it stays.
    let mut recent_sessions: std::collections::HashMap<String, std::time::Instant> = std::collections::HashMap::new();
    let mut hour_written: (String, usize) = (String::new(), 0);
    // Every address this server answers on — advertised in the catalog reply so the app
    // can fall back to another front door of the SAME server when one gateway is out.
    let identities = Arc::new(all_addresses.clone());
    // The chain watcher. Nothing here used to look at a payment unless a client asked:
    // an invoice went to "paid" only on `invoice.status` or an entitlement sweep. So a
    // faucet-funded invite sat unsettled for as long as the tester left the app closed,
    // while the claim page said "waiting for the chain" and waited for something only the
    // app could cause (2026-09-06). Now the server checks a few open invoices itself.
    // Before serving anything: a voucher burned in a run that did not survive to credit it.
    if credit_pending_vouchers(&db, &mut paywall) {
        persist_changed(&mut db, &sessions, &mut quorum, &paywall, &mut saved);
    }
    // One second, because a person is looking at a spinner. See the arm below.
    let mut order_tick = tokio::time::interval(std::time::Duration::from_millis(ORDER_TICK_MS));
    order_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut watch_tick = tokio::time::interval(std::time::Duration::from_secs(WATCH_TICK_SECS));
    watch_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Spend-record retention: rows older than QUORUM_RETAIN_DAYS go, every six hours.
    let mut prune_tick = tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
    prune_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            // Ask the chain about a few open invoices, off the loop like every other
            // gateway call. Bounded on both sides: at most WATCH_PER_TICK invoices per
            // tick, and each invoice at most once every 25 s (pay.rs WATCH_EVERY_MS).
            // Web orders booked by the faucet on /pay. It cannot raise an invoice itself —
            // the rails live here, and this box has no clearnet port — so it leaves a row and
            // we answer it. On its OWN beat, not the chain watcher's: nobody is waiting on a
            // chain poll, but somebody IS watching a spinner while this happens. The query is
            // one indexed lookup on a table that is almost always empty.
            _ = order_tick.tick() => {
                // A buyer who pressed cancel: the invoice lives here, so the cancelling does
                // too. Recorded as an error on the row, which also stops the page polling for
                // something that will never arrive.
                for (order_id, invoice) in db.web_orders_to_cancel() {
                    paywall.cancel_invoice(&invoice);
                    db.web_order_answer(&order_id, Some(&invoice), None, Some("cancelled"));
                    persist_changed(&mut db, &sessions, &mut quorum, &paywall, &mut saved);
                }
                // Bearer money must not outlive its window. Unconditional: a buyer who never
                // pressed "I have written it down" is exactly the one whose code would
                // otherwise sit in the table forever, since nothing prunes web_orders.
                let forgotten = db.web_orders_forget_codes(pay::now_ms());
                if forgotten > 0 {
                    println!("scrai-server: forgot {forgotten} voucher code(s) past the display window");
                }
                for (order_id, usd, method, consent) in db.web_orders_pending(WEB_ORDERS_PER_TICK) {
                    match paywall.begin_web_order(&order_id, usd, &method, &consent) {
                        Err(why) => db.web_order_answer(&order_id, None, None, Some(&why)),
                        Ok(pending) => {
                            let (tx, gw, slots) = (pay_tx.clone(), gateway.clone(), gateway_slots.clone());
                            tokio::spawn(async move {
                                let outcome = match tokio::time::timeout(QUEUE_WAIT, slots.acquire_owned()).await {
                                    Ok(Ok(_permit)) => pay::run_gateway(pending, &gw).await,
                                    _ => pay::gateway_busy(pending),
                                };
                                let _ = tx.send(PayDone { outcome, to: None, _guard: None }).await;
                            });
                        }
                    }
                }
            }
            _ = prune_tick.tick() => {
                match db.prune_spent(quorum_retain_secs()) {
                    Ok((0, _)) => {}
                    Ok((rows, coins)) => println!("scrai-server: pruned {rows} spend record(s) / {coins} coins past retention"),
                    Err(e) => eprintln!("scrai-server: spend-record prune failed ({e})"),
                }
            }
            _ = watch_tick.tick() => {
                // A voucher whose burn landed but whose credit did not — the crash window
                // between the two stores. Repaired here rather than only at boot: a task
                // that fails without taking the process with it would otherwise leave a
                // buyer waiting for a restart that may be weeks away.
                if credit_pending_vouchers(&db, &mut paywall) {
                    persist_changed(&mut db, &sessions, &mut quorum, &paywall, &mut saved);
                }
                // Mirror settlement into the order row, so the faucet can answer "paid yet?"
                // without ever parsing the pay snapshot.
                for (order_id, invoice) in db.web_orders_awaiting_payment() {
                    if paywall.invoice_paid(&invoice) {
                        db.web_order_paid(&order_id, pay::now_ms());
                    }
                }
                // Housekeeping on the same beat: settled invoices older than 14 days lose
                // the buyer's account. Cheap (a scan of a small map) and it must not depend
                // on anyone happening to poll an invoice.
                paywall.scrub_account_links();
                // Cached replies past their retry window go too. This is the only place an
                // ANSWER lives on this machine, and only so a lost reply can be re-sent
                // without a second charge; after ten minutes there is nothing to re-send.
                chat::sweep_replies(&mut chat_replies);
                // And the web half of the same purchase: the address and memo an old order
                // was to be paid at. The row keeps what the order WAS, not how to pay it.
                let dropped = db.web_orders_forget_pay(pay::now_ms());
                if dropped > 0 {
                    println!("scrai-server: dropped the payment details from {dropped} old web order(s)");
                }
                // The same rule for vouchers: a redeemed one stops naming its account after
                // ACCOUNT_LINK_DAYS (seven by default), so the two halves of a purchase do not disagree about how
                // long it stays attributable.
                let stale = paywall.voucher_links_expired(&db.voucher_links());
                if !stale.is_empty() {
                    let n = db.voucher_forget_account(&stale);
                    println!("scrai-server: dropped the account link from {n} redeemed voucher(s)");
                }
                // App Store purchases follow the same clock.
                let stale = paywall.voucher_links_expired(&db.iap_links());
                if !stale.is_empty() {
                    let n = db.iap_forget_account(&stale);
                    println!("scrai-server: dropped the account link from {n} App Store purchase(s)");
                }
                // And the purchase link, on the same clock: after ACCOUNT_LINK_DAYS (seven by default) a voucher no
                // longer says which payment it came from — like an invoice no longer says
                // whose it was. The code still resolves by fingerprint and still voids.
                let cut = db.voucher_forget_invoices(pay::now_ms());
                if cut > 0 {
                    println!("scrai-server: dropped the purchase link from {cut} voucher(s)");
                }
                let candidates = paywall.watch_candidates(WATCH_PER_TICK);
                if !candidates.is_empty() {
                    let (tx, gw, slots) = (pay_tx.clone(), gateway.clone(), gateway_slots.clone());
                    tokio::spawn(async move {
                        let pending = pay::PayPending::Watch { candidates };
                        let outcome = match tokio::time::timeout(QUEUE_WAIT, slots.acquire_owned()).await {
                            Ok(Ok(_permit)) => pay::run_gateway(pending, &gw).await,
                            _ => pay::gateway_busy(pending),
                        };
                        let _ = tx.send(PayDone { outcome, to: None, _guard: None }).await;
                    });
                }
            }
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
                let session_of_chat = done.pending.session_id().map(str::to_string);
                let settled = chat::settle(done.pending, done.result, &mut sessions, &mut quorum, &mut chat_replies);
                let mut response = settled.reply;
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
                        // Provider cost from the settle result — NOT from the reply, whose
                        // `costScrai` is null on a release server (the client never sees the margin).
                        let cost = settled.provider_cost.map(|f| f.ceil() as u64).unwrap_or(0);
                        db.bump_daily(&today, 1, spent, cost, 0, 0);
                        if let Some(sid) = &session_of_chat {
                            db.note_user(&today, sid); // distinct paying sessions today ("users")
                            // …and the busiest hour of that same population. Pruned here
                            // rather than on a timer: this path runs once per paid chat, so
                            // the map cannot outgrow an hour's worth of sessions.
                            let now = std::time::Instant::now();
                            recent_sessions.insert(store::Store::user_key(sid), now);
                            recent_sessions.retain(|_, t| now.duration_since(*t) < std::time::Duration::from_secs(3600));
                            let n = recent_sessions.len();
                            if hour_written.0 != today || n > hour_written.1 {
                                db.bump_hour_peak(&today, n);
                                hour_written = (today.clone(), n);
                            }
                        }
                        // Per-model breakdown for the admin table (the reply names the billed model).
                        let model = rv.pointer("/usage/billing/model").and_then(|m| m.as_str()).unwrap_or("unknown");
                        db.bump_daily_model(&today, model, 1, spent, cost);
                        // Consume the month's grounding allowance — re-read on the loop, so it's race-free.
                        let q = rv.pointer("/usage/groundingQueries").and_then(|c| c.as_u64()).unwrap_or(0);
                        // Only Gemini queries draw on Google's monthly allowance; OpenAI's are per call.
                        if q > 0 && !scrai_server::openai::is_openai_model(model) {
                            let g_month_key = format!("grounding:{}", &today[..7]);
                            let g_used: u64 = db.load(&g_month_key).and_then(|s| s.parse().ok()).unwrap_or(0);
                            let _ = db.save_many(&[(g_month_key.as_str(), (g_used + q).to_string().as_str())]);
                        }
                    }
                }
                // Durability: persist any changed store before acknowledging (same as below).
                persist_changed(&mut db, &sessions, &mut quorum, &paywall, &mut saved);
                if let Err(e) = senders[done.to.idx].read().await.send_reply(done.to.tag, response).await {
                    eprintln!("scrai-server: chat reply failed: {e}");
                }
            }
            // A spawned BLS job returned → apply its outcome to the money state here.
            Some(done) = crypto_rx.recv() => {
                let (label, response) = match done.kind {
                    CryptoKind::Withdraw { id, account_id, req_key, book_toku, result } => {
                        inflight_withdraws.remove(&req_key);
                        let resp = match result {
                            Ok(r) => r,
                            Err(message) => federation::FedResponse::Error { message },
                        };
                        // The book's entitlement was reserved before issuance; anything but
                        // an issued credential gives it back (and forgets the charge record,
                        // so the client's retry is a fresh purchase). An issued credential is
                        // cached under the body's key: the same request again gets the same
                        // reply without a second charge (M-cl-2).
                        if matches!(resp, federation::FedResponse::Withdraw { .. }) {
                            paywall.finish_issuance(&req_key, serde_json::to_value(&resp).unwrap_or(serde_json::Value::Null));
                        } else {
                            paywall.restore_entitlement(&account_id, book_toku);
                            paywall.abort_issuance(&req_key);
                        }
                        let reply = serde_json::json!({ "id": id, "fed": serde_json::to_value(&resp).unwrap_or(serde_json::Value::Null) });
                        ("coconut.Withdraw", serde_json::to_vec(&reply).unwrap_or_default())
                    }
                    CryptoKind::Return { id, account, tender, verified } => {
                        let reply = match verified {
                            Err(e) => serde_json::json!({ "id": id, "kind": "error", "error": e }),
                            Ok(()) => {
                                // Burn what is still unspent and credit only that. A note the
                                // quorum has seen before is simply worth nothing here — which
                                // is also what makes a lost reply safe to retry: the second
                                // attempt credits 0 and reports the same entitlement.
                                let mut credited = 0u64;
                                for n in &tender.notes {
                                    let Ok(pi) = n.pay_info() else { continue };
                                    if let scrai_core::quorum::Verdict::Accepted =
                                        quorum.submit(&n.payment, pi, federation::this_server())
                                    {
                                        credited += n.coins * scrai_core::coconut::COIN_TOKU;
                                    }
                                }
                                paywall.credit_voucher(&account, credited);
                                println!("scrai-server: coins returned to an account — {credited} TOKU");
                                serde_json::json!({ "id": id, "kind": "coins.ok", "credited": credited,
                                    "entitlement": paywall.entitlement(&account) })
                            }
                        };
                        ("coins.return", serde_json::to_vec(&reply).unwrap_or_default())
                    }
                    CryptoKind::Redeem { id, req, verified } => {
                        let reply = match verified {
                            Err(e) => scrai_core::gateway::redeem_error(&id, e),
                            Ok(()) => scrai_core::gateway::redeem_apply(&mut quorum, &mut sessions, req, id).await,
                        };
                        ("redeem", serde_json::to_vec(&reply).unwrap_or_default())
                    }
                };
                let (c0, c1) = label_color(label);
                println!(
                    "scrai-server: handled {c0}{label}{c1} (→ {} bytes · crypto wait {} ms, work {} ms)",
                    response.len(), done.timing.0, done.timing.1
                );
                persist_changed(&mut db, &sessions, &mut quorum, &paywall, &mut saved);
                if let Err(e) = senders[done.to.idx].read().await.send_reply(done.to.tag, response).await {
                    eprintln!("scrai-server: {label} reply failed: {e}");
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
                persist_changed(&mut db, &sessions, &mut quorum, &paywall, &mut saved);
                match done.to {
                    Some(to) => {
                        if let Err(e) = senders[to.idx].read().await.send_reply(to.tag, response).await {
                            eprintln!("scrai-server: pay reply failed: {e}");
                        }
                    }
                    // Nobody to answer over the mixnet: either the chain watcher (which
                    // returns nothing) or a web order, whose answer goes back into the table
                    // the faucet reads.
                    None if kind == "invoice.create" => {
                        let v: serde_json::Value = serde_json::from_slice(&response).unwrap_or(serde_json::Value::Null);
                        let order = v.get("invoiceId").and_then(|i| i.as_str()).map(str::to_string);
                        match (order, v.get("error").and_then(|e| e.as_str())) {
                            (Some(id), _) => {
                                let body = String::from_utf8_lossy(&response).into_owned();
                                db.web_order_answer(&id, Some(&id), Some(&body), None);
                            }
                            // A refused raise still has to reach the page, or it polls a row
                            // that will never change. The order id rode along as the request id.
                            (None, Some(e)) => {
                                eprintln!("scrai-server: a web order could not be raised: {e}");
                                if let Some(id) = v.get("id").and_then(|i| i.as_str()) {
                                    db.web_order_answer(id, None, None, Some(e));
                                }
                            }
                            _ => {}
                        }
                    }
                    None => {}
                }
            }
            inbound = in_rx.recv() => {
                let Some(Inbound { idx, msg: m }) = inbound else {
                    // Only at shutdown: receive tasks reconnect a dead identity themselves.
                    eprintln!("scrai-server: every mixnet stream ended");
                    break;
                };
            let Some(tag) = m.sender_tag else {
                eprintln!("scrai-server: dropping a message with no reply SURB");
                continue;
            };
            let to = ReplyTo { idx, tag };
            // `chat` + `models` need async HTTP to the provider; everything else is
            // handled synchronously by the shared core.
            let envelope = serde_json::from_slice::<serde_json::Value>(&m.message).unwrap_or(serde_json::Value::Null);
            let kind = envelope.get("kind").and_then(|k| k.as_str()).map(String::from).unwrap_or_default();
            // Release gate (MIN_APP): an outdated app gets nothing but the update
            // notice. `models` answers with a one-entry pseudo catalogue so even a 0.2.x
            // client — which swallows a plain error on its start-up fetch — shows the
            // notice in its model header; everything else is a plain error carrying the link.
            // Exempt: `ping` (latency probe) and the chunk follow-ups — an `image.chunk` ref
            // only exists because a gated `chat` produced it, an `upload.chunk` only because a
            // gated `upload.begin` reserved the slot. (0.3.0 sends both without `app`.)
            if !matches!(kind.as_str(), "ping" | "image.chunk" | "upload.chunk") {
                if let Some((min, url)) = scrai_server::app_outdated(&envelope) {
                    let id = envelope.get("id").cloned().unwrap_or(serde_json::Value::Null);
                    let app = envelope.get("app").and_then(|a| a.as_str()).unwrap_or("<0.3.0 (no version sent)");
                    eprintln!("scrai-server: UPDATE GATE — refused `{kind}` from app {app} (min {min})");
                    let notice = format!("Update required — tokumai {min} or newer. Download: {url}");
                    let resp = if kind == "models" {
                        serde_json::json!({
                            "id": id,
                            "models": [{ "model": "update-required", "label": format!("⚠ Update required — get {min} at {url}"), "vendor": "tokumai", "kind": "chat", "rate": { "in": 0, "out": 0 } }],
                            "testnet": scrai_server::pay::is_testnet_server(), "faucetUrl": scrai_server::pay::faucet_url(),
                            "update": { "required": true, "minApp": min, "url": url },
                        })
                    } else {
                        serde_json::json!({ "id": id, "kind": "error", "error": notice, "updateRequired": true, "minApp": min, "updateUrl": url })
                    };
                    if let Err(e) = senders[to.idx].read().await.send_reply(to.tag, serde_json::to_vec(&resp).unwrap_or_default()).await {
                        eprintln!("scrai-server: update-gate reply failed: {e}");
                    }
                    continue;
                }
            }
            // L5: every control branch (coconut/invoice/redeem/models/ping/gateway) should be
            // tiny; feeding a big reassembled body to serde_json + O(coins) BLS is wasted
            // transient allocation. chat + upload manage their own (much larger) size limits.
            const MAX_CONTROL_BYTES: usize = 256 * 1024;
            if !matches!(kind.as_str(), "chat" | "upload.begin" | "upload.chunk" | "image.chunk")
                && m.message.len() > MAX_CONTROL_BYTES
            {
                let resp = serde_json::to_vec(&serde_json::json!({"kind":"error","error":"request too large"})).unwrap_or_default();
                if let Err(e) = senders[to.idx].read().await.send_reply(to.tag, resp).await {
                    eprintln!("scrai-server: reply failed: {e}");
                }
                continue;
            }
            // H2: the catalog fetch is pure — immutable pricing, no session/paywall/quorum
            // state — and its provider HTTP (the Gemini model list) can be slow. Spawn it
            // so it never blocks chat/payment on the single dispatch loop; it replies itself.
            if kind == "models" {
                let (p, sender, msg, ids) = (pricing.clone(), senders.clone(), m.message.clone(), identities.clone());
                let guard = inflight.enter(to);
                note_peak(&db, &inflight, &mut peak_written);
                tokio::spawn(async move {
                    let resp = catalog::handle(&msg, &p, margin, &ids).await;
                    if let Err(e) = sender[to.idx].read().await.send_reply(to.tag, resp).await {
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
                // `load` = how busy this server is right now (clients with a spawned request in
                // flight vs the chat cap). Coarse, public, and the seed of client-side server
                // selection (docs/load-testing.md): a client can prefer the emptier server.
                let resp = serde_json::to_vec(&serde_json::json!({
                    "id": id, "kind": "pong", "serverVersion": scrai_server::VERSION,
                    "load": { "inflight": inflight.current(), "maxChats": max_chats },
                    "identities": &*identities,
                })).unwrap_or_default();
                if let Err(e) = senders[to.idx].read().await.send_reply(to.tag, resp).await {
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
                match chat::reserve(&m.message, &mut sessions, &mut quorum, &mut uploads, &pricing, margin, &mut chat_replies, grounding_free) {
                    // Validation error or an idempotent replay hit — no provider call, and
                    // reserve() never mutates the money state on this path.
                    chat::Reserved::Reply(response) => {
                        if let Err(e) = senders[to.idx].read().await.send_reply(to.tag, response).await {
                            eprintln!("scrai-server: chat reply failed: {e}");
                        }
                    }
                    // Reserved → run the provider off the loop; settle() prices it later.
                    chat::Reserved::Proceed(pending) => {
                        let tx = http_tx.clone();
                        let slots = if pending.provider() == "openai" { openai_slots.clone() } else { chat_slots.clone() };
                        let guard = inflight.enter(to);
                        note_peak(&db, &inflight, &mut peak_written);
                        let auth = mint.clone();
                        tokio::spawn(async move {
                            let result = match tokio::time::timeout(QUEUE_WAIT, slots.acquire_owned()).await {
                                // The permit lives for the whole provider call.
                                Ok(Ok(_permit)) => chat::run_provider(&pending, &auth).await,
                                _ => Err("the server is busy with too many chats right now — please try again in a moment".to_string()),
                            };
                            let _ = tx.send(HttpDone { pending: *pending, result, to, _guard: guard }).await;
                        });
                    }
                }
                continue;
            }
            // An App Store purchase, verified here against Apple's pinned root (iap.rs) and
            // then credited exactly like a voucher: claim in SQL first, credit in the
            // snapshot second, the same crash argument as below. The app keeps Apple's
            // transaction unfinished until this reply arrives, so a lost reply is a retry —
            // and a retry by the same account is a success with nothing more to credit.
            if kind == iap::IAP_KIND {
                let v: serde_json::Value = serde_json::from_slice(&m.message).unwrap_or(serde_json::Value::Null);
                let id = v.get("id").cloned().unwrap_or(serde_json::Value::Null);
                let jws = v.get("jws").and_then(|j| j.as_str()).unwrap_or("");
                let reply = match paywall.iap_claimant(&v) {
                    None => serde_json::json!({ "id": id, "kind": "error",
                        "error": "account signature does not check out, or the nonce was reused" }),
                    Some(account) if paywall.admit_voucher(&account).is_err() => serde_json::json!({
                        "id": id, "kind": "error",
                        "error": "too many purchase checks from this account — try again in a few minutes" }),
                    Some(account) => {
                        let now = pay::now_ms();
                        match iap::verify_jws(jws, now).and_then(|tx| iap::credit_for(&tx).map(|toku| (tx, toku))) {
                            Err(e) => serde_json::json!({ "id": id, "kind": "error", "error": e }),
                            Ok((tx, toku)) => {
                                let hash = iap::tx_hash(&tx.transaction_id);
                                match db.iap_claim(&hash, &tx.product_id, toku, &tx.environment, &tx.storefront,
                                                   &account, tx.purchased_at_ms, now) {
                                    store::IapClaim::New => {
                                        paywall.credit_voucher(&account, toku);
                                        db.iap_credited(&hash, now);
                                        println!("scrai-server: App Store purchase credited — {toku} TOKU ({})", tx.environment);
                                        serde_json::json!({ "id": id, "kind": "iap.ok", "toku": toku,
                                            "entitlement": paywall.entitlement(&account) })
                                    }
                                    store::IapClaim::AlreadyYours => serde_json::json!({ "id": id, "kind": "iap.ok",
                                        "toku": 0, "entitlement": paywall.entitlement(&account) }),
                                    store::IapClaim::AlreadyOther => serde_json::json!({ "id": id, "kind": "error",
                                        "error": "this purchase has already been credited" }),
                                }
                            }
                        }
                    }
                };
                persist_changed(&mut db, &sessions, &mut quorum, &paywall, &mut saved);
                let out = serde_json::to_vec(&reply).unwrap_or_default();
                if let Err(e) = senders[to.idx].read().await.send_reply(to.tag, out).await {
                    eprintln!("scrai-server: purchase reply failed: {e}");
                }
                continue;
            }
            // Redeeming a voucher never leaves the machine, so it stays on the loop — and it
            // has to, because it touches BOTH stores: the burn is SQL, the credit is the pay
            // snapshot. Order matters and is argued in docs/vouchers.md: burn first (a crash
            // then loses a credit, which `credit_pending_vouchers` repairs) rather than
            // credit first (a crash then leaves a spent code valid, which nothing can).
            if kind == pay::VOUCHER_KIND {
                let v: serde_json::Value = serde_json::from_slice(&m.message).unwrap_or(serde_json::Value::Null);
                let id = v.get("id").cloned().unwrap_or(serde_json::Value::Null);
                let code = v.get("code").and_then(|c| c.as_str()).unwrap_or("");
                let reply = match paywall.voucher_claimant(&v) {
                    None => serde_json::json!({ "id": id, "kind": "error",
                        "error": "account signature does not check out, or the nonce was reused" }),
                    Some(account) if paywall.admit_voucher(&account).is_err() => serde_json::json!({
                        "id": id, "kind": "error",
                        "error": "too many code attempts from this account — try again in a few minutes" }),
                    Some(account) => {
                        let now = pay::now_ms();
                        // The keyed fingerprint first, then the unkeyed one that preceded it:
                        // codes handed out before VOUCHER_KEY existed still have to redeem, and
                        // only the server can tell which construction a given code belongs to.
                        // A miss changes nothing, so trying both costs a lookup.
                        let mut candidates = vec![pay::voucher_hash(code)];
                        let legacy = pay::voucher_hash_legacy(code);
                        if !candidates.contains(&legacy) {
                            candidates.push(legacy);
                        }
                        let mut hash = candidates[0].clone();
                        let mut burn = store::VoucherBurn::Unknown;
                        for c in &candidates {
                            burn = db.voucher_burn(c, &account, now);
                            if !matches!(burn, store::VoucherBurn::Unknown) {
                                hash = c.clone();
                                break;
                            }
                        }
                        match burn {
                            store::VoucherBurn::Burned { toku } => {
                                paywall.credit_voucher(&account, toku);
                                db.voucher_credited(&hash, now);
                                println!("scrai-server: voucher redeemed — {toku} TOKU");
                                serde_json::json!({ "id": id, "kind": "voucher.ok", "toku": toku,
                                    "entitlement": paywall.entitlement(&account) })
                            }
                            // A reply lost on the way back makes the app try again with the
                            // same code. Refusing that would punish someone who did nothing
                            // wrong; the credit is already theirs (or will be, at boot).
                            store::VoucherBurn::AlreadyYours => serde_json::json!({ "id": id, "kind": "voucher.ok",
                                "toku": 0, "entitlement": paywall.entitlement(&account) }),
                            store::VoucherBurn::Spent => serde_json::json!({ "id": id, "kind": "error",
                                "error": "this code has already been redeemed" }),
                            store::VoucherBurn::Void => serde_json::json!({ "id": id, "kind": "error",
                                "error": "this code was refunded and can no longer be redeemed" }),
                            // Vouchers and invite codes look alike and share one field in the
                            // app. The server tells them apart rather than the client trying
                            // both — over a mixnet, a wrong guess costs a whole round trip.
                            store::VoucherBurn::Unknown => {
                                let norm: String = code.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                                    .map(|c| c.to_ascii_uppercase()).collect();
                                if scrai_server::faucet::code_has_uses_left(&pay::faucet_db_path(), &norm) {
                                    serde_json::json!({ "id": id, "kind": "voucher.invite", "code": norm })
                                } else {
                                    serde_json::json!({ "id": id, "kind": "error", "error": "that code is not valid" })
                                }
                            }
                        }
                    }
                };
                // The credit lives in the snapshot, so it must reach disk before the ack —
                // same rule as a cancelled invoice a few lines below.
                persist_changed(&mut db, &sessions, &mut quorum, &paywall, &mut saved);
                let out = serde_json::to_vec(&reply).unwrap_or_default();
                if let Err(e) = senders[to.idx].read().await.send_reply(to.tag, out).await {
                    eprintln!("scrai-server: voucher reply failed: {e}");
                }
                continue;
            }
            // H2 (pay): authenticate/throttle on the loop, then run the gateway HTTP in a
            // spawned task — its result comes back via pay_tx and finish() runs here. A slow
            // LCD node used to stall every chat reserve/settle for up to 15 s.
            if pay::PAY_KINDS.contains(&kind.as_str()) {
                let response = match paywall.begin(&m.message, &gateway) {
                    pay::PayStep::Reply(response) => response,
                    pay::PayStep::Pending(pending) => {
                        let (tx, gw, slots) = (pay_tx.clone(), gateway.clone(), gateway_slots.clone());
                        let guard = inflight.enter(to);
                        note_peak(&db, &inflight, &mut peak_written);
                        tokio::spawn(async move {
                            let outcome = match tokio::time::timeout(QUEUE_WAIT, slots.acquire_owned()).await {
                                Ok(Ok(_permit)) => pay::run_gateway(pending, &gw).await,
                                _ => pay::gateway_busy(pending),
                            };
                            let _ = tx.send(PayDone { outcome, to: Some(to), _guard: Some(guard) }).await;
                        });
                        continue;
                    }
                };
                // Cancel / validation error: nothing outbound, but the nonce burn or the
                // cancelled invoice must still hit disk before the ack.
                let (c0, c1) = label_color(&kind);
                println!("scrai-server: handled {c0}{kind}{c1} ({} → {} bytes)", m.message.len(), response.len());
                persist_changed(&mut db, &sessions, &mut quorum, &paywall, &mut saved);
                if let Err(e) = senders[to.idx].read().await.send_reply(to.tag, response).await {
                    eprintln!("scrai-server: reply failed: {e}");
                }
                continue;
            }
            // The old session layer handing its balance back. Every prompt pays with coins
            // now, so a balance left on a session would simply be stranded when that path
            // goes (docs/unlinkability.md, block D). Two signatures: the SESSION consents
            // to being emptied and names where the money goes, the ACCOUNT proves it is
            // that destination. Both keys come from one recovery phrase, so only its owner
            // can produce the pair — the server learns nothing new about who is who, it
            // just sees the two proofs it already understands.
            if kind == "session.drain" {
                let id = envelope.get("id").cloned().unwrap_or(serde_json::Value::Null);
                let bad = |e: &str| serde_json::to_vec(&serde_json::json!({ "id": id, "kind": "error", "error": e })).unwrap_or_default();
                let field = |k: &str| envelope.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let (skey, sid, ssig, nonce) = (field("sessionKey"), field("sessionId"), field("sessionSig"), field("nonce"));
                let response = match paywall.drain_claimant(&envelope) {
                    None => bad("account signature does not check out, or the nonce was reused"),
                    Some(account) if !scrai_core::auth::session_hands_over(&skey, &sid, &account, &nonce, &ssig) => {
                        bad("the session did not authorise this hand-over")
                    }
                    Some(account) => {
                        // Drain first, credit second, and persist before the ack: a crash
                        // between them would lose the money, which is why the reply waits
                        // for the disk write below like every other credit.
                        let moved = sessions.drain(&sid);
                        paywall.credit_voucher(&account, moved);
                        if moved > 0 {
                            println!("scrai-server: a session balance moved to its account — {moved} TOKU");
                        }
                        serde_json::to_vec(&serde_json::json!({ "id": id, "kind": "drain.ok", "moved": moved,
                            "entitlement": paywall.entitlement(&account) })).unwrap_or_default()
                    }
                };
                let (c0, c1) = label_color(&kind);
                println!("scrai-server: handled {c0}{kind}{c1} ({} → {} bytes)", m.message.len(), response.len());
                persist_changed(&mut db, &sessions, &mut quorum, &paywall, &mut saved);
                if let Err(e) = senders[to.idx].read().await.send_reply(to.tag, response).await {
                    eprintln!("scrai-server: session.drain reply failed: {e}");
                }
                continue;
            }
            // Coins coming home: a device being retired or moved hands back what it never
            // spent, and the value lands on the ACCOUNT as entitlement. Account-signed so
            // only its owner can receive it; the coins themselves are bearer money and are
            // burned here like any other spend. Same three steps as redeem.
            if kind == "coins.return" {
                let id = envelope.get("id").cloned().unwrap_or(serde_json::Value::Null);
                let bad = |e: &str| serde_json::to_vec(&serde_json::json!({ "id": id, "kind": "error", "error": e })).unwrap_or_default();
                let account = paywall.return_claimant(&envelope);
                let tender: Option<scrai_core::tender::Tender> =
                    serde_json::from_value(envelope.get("tender").cloned().unwrap_or(serde_json::Value::Null)).ok();
                let response = match (account, tender) {
                    (None, _) => Some(bad("account signature does not check out, or the nonce was reused")),
                    (_, None) => Some(bad("no coins in this request")),
                    (Some(account), Some(tender)) => match tender.well_formed() {
                        Err(e) => Some(bad(&e)),
                        // Bounded so one request cannot hand the crypto pool an unbounded
                        // pile of pairings; the client returns a book in chunks.
                        Ok(()) if tender.total_coins() > MAX_RETURN_COINS => {
                            Some(bad("too many coins in one return — send them in smaller batches"))
                        }
                        Ok(()) => {
                            let (tx, auth, slots) = (crypto_tx.clone(), authority.clone(), crypto_slots.clone());
                            let guard = inflight.enter(to);
                            note_peak(&db, &inflight, &mut peak_written);
                            tokio::spawn(async move {
                                let t0 = std::time::Instant::now();
                                let (tender, verified, waited) = match tokio::time::timeout(QUEUE_WAIT, slots.acquire_owned()).await {
                                    Ok(Ok(_permit)) => {
                                        let waited = t0.elapsed().as_millis();
                                        let (tender, v) = tokio::task::spawn_blocking(move || {
                                            let v = tender.notes.iter().try_for_each(|n| {
                                                let pi = n.pay_info()?;
                                                auth.verify_payment(&n.payment, &pi, n.spend_date).map_err(|e| format!("invalid coin: {e}"))
                                            });
                                            (tender, v)
                                        })
                                        .await
                                        .unwrap_or_else(|e| panic!("coin return verify task failed: {e}"));
                                        (tender, v, waited)
                                    }
                                    _ => (tender, Err("the server is busy verifying payments right now — please try again in a moment".into()), t0.elapsed().as_millis()),
                                };
                                let timing = (waited, t0.elapsed().as_millis() - waited);
                                let _ = tx
                                    .send(CryptoDone { kind: CryptoKind::Return { id, account, tender, verified }, to, timing, _guard: guard })
                                    .await;
                            });
                            None
                        }
                    },
                };
                if let Some(out) = response {
                    persist_changed(&mut db, &sessions, &mut quorum, &paywall, &mut saved);
                    if let Err(e) = senders[to.idx].read().await.send_reply(to.tag, out).await {
                        eprintln!("scrai-server: coins.return reply failed: {e}");
                    }
                }
                continue;
            }
            // redeem: parse on the loop, verify (BLS) in a blocking task, apply back here.
            if kind == "redeem" {
                let id = envelope.get("id").cloned().unwrap_or(serde_json::Value::Null);
                match scrai_core::gateway::redeem_parse(&envelope) {
                    Err(e) => {
                        let reply = serde_json::to_vec(&scrai_core::gateway::redeem_error(&id, e)).unwrap_or_default();
                        if let Err(e) = senders[to.idx].read().await.send_reply(to.tag, reply).await {
                            eprintln!("scrai-server: redeem reply failed: {e}");
                        }
                    }
                    Ok(req) => {
                        let (tx, auth, slots) = (crypto_tx.clone(), authority.clone(), crypto_slots.clone());
                        let guard = inflight.enter(to);
                        note_peak(&db, &inflight, &mut peak_written);
                        tokio::spawn(async move {
                            let t0 = std::time::Instant::now();
                            let (req, verified, waited) = match tokio::time::timeout(QUEUE_WAIT, slots.acquire_owned()).await {
                                Ok(Ok(_permit)) => {
                                    let waited = t0.elapsed().as_millis();
                                    let (req, v) = tokio::task::spawn_blocking(move || {
                                        let v = scrai_core::gateway::redeem_verify(&auth, &req);
                                        (req, v)
                                    })
                                    .await
                                    .unwrap_or_else(|e| panic!("redeem verify task failed: {e}"));
                                    (req, v, waited)
                                }
                                _ => (req, Err("the server is busy verifying payments right now — please try again in a moment".into()), t0.elapsed().as_millis()),
                            };
                            let timing = (waited, t0.elapsed().as_millis() - waited);
                            let _ = tx.send(CryptoDone { kind: CryptoKind::Redeem { id, req, verified }, to, timing, _guard: guard }).await;
                        });
                    }
                }
                continue;
            }
            let response = match kind.as_str() {
                "upload.begin" | "upload.chunk" => uploads.handle(&m.message),
                "image.chunk" => staged.handle(&m.message),
                // Coconut issuance is gated by the paywall: a Withdraw must be
                // account-signed and backed by a ticketbook's worth of paid entitlement.
                // The entitlement is RESERVED here (so two in-flight withdraws of one
                // account can't both pass), the 500-signature issuance runs in a blocking
                // task, and a failed issuance restores the entitlement (crypto_rx above).
                // The price of a withdrawal is the book of ITS denomination: a coarse book
                // is ten times a fine one, and charging one for the other would either
                // give money away or overcharge.
                "coconut" => match paywall.gate_withdraw(&m.message, |d| ticketbook_coins() * mint.for_request(d).denom_toku()) {
                    pay::Gate::Denied(reply) => reply,
                    pay::Gate::NotAWithdraw => {
                        scrai_core::gateway::handle(&authority, &mut quorum, &mut sessions, &m.message).await
                    }
                    pay::Gate::Authorized { account_id, req_key, prepaid } => {
                        let id = envelope.get("id").cloned().unwrap_or(serde_json::Value::Null);
                        let fed = serde_json::from_value::<federation::FedRequest>(
                            envelope.get("fed").cloned().unwrap_or(serde_json::Value::Null),
                        );
                        let fed_error = |id: &serde_json::Value, message: String| {
                            let resp = federation::FedResponse::Error { message };
                            serde_json::to_vec(&serde_json::json!({ "id": id, "fed": serde_json::to_value(&resp).unwrap_or(serde_json::Value::Null) }))
                                .unwrap_or_default()
                        };
                        match fed {
                            Ok(federation::FedRequest::Withdraw { user_pk, req, denom_toku }) => {
                                // M1: a key caught double-spending may not withdraw fresh books.
                                if quorum.is_blacklisted(&user_pk) {
                                    fed_error(&id, "blacklisted: this key was caught double-spending and may not withdraw".into())
                                } else if let Some(fed) = paywall.issuance(&req_key).and_then(|r| r.fed.clone()) {
                                    // M-cl-2 replay: this body was charged AND issued before — the
                                    // client lost the reply. Same credential back, no new charge.
                                    println!("scrai-server: withdraw retry answered from the issued cache");
                                    serde_json::to_vec(&serde_json::json!({ "id": id, "fed": fed })).unwrap_or_default()
                                } else if inflight_withdraws.contains(&req_key) {
                                    fed_error(&id, "this credential is still being issued — please retry in a moment".into())
                                } else {
                                    // `prepaid`: charged earlier, but the server went down before
                                    // issuing — issue now without charging again.
                                    let book_toku = ticketbook_coins() * mint.for_request(denom_toku).denom_toku();
                                    if !prepaid {
                                        paywall.consume_entitlement(&account_id, book_toku);
                                    } else {
                                        println!("scrai-server: withdraw retry for a charged-but-unissued body — issuing without a second charge");
                                    }
                                    paywall.begin_issuance(&req_key, &account_id);
                                    inflight_withdraws.insert(req_key.clone());
                                    let (tx, auth, slots) = (crypto_tx.clone(), mint.for_request(denom_toku).clone(), crypto_slots.clone());
                                    let guard = inflight.enter(to);
                                    note_peak(&db, &inflight, &mut peak_written);
                                    tokio::spawn(async move {
                                        let t0 = std::time::Instant::now();
                                        let (result, waited) = match tokio::time::timeout(QUEUE_WAIT, slots.acquire_owned()).await {
                                            Ok(Ok(_permit)) => {
                                                let waited = t0.elapsed().as_millis();
                                                let r = tokio::task::spawn_blocking(move || {
                                                    auth.handle(federation::FedRequest::Withdraw { user_pk, req, denom_toku })
                                                })
                                                .await
                                                .unwrap_or_else(|e| Err(format!("issuance task failed: {e}")));
                                                (r, waited)
                                            }
                                            _ => (Err("the server is busy issuing credentials right now — please try again in a moment".into()), t0.elapsed().as_millis()),
                                        };
                                        let timing = (waited, t0.elapsed().as_millis() - waited);
                                        let _ = tx.send(CryptoDone { kind: CryptoKind::Withdraw { id, account_id, req_key, book_toku, result }, to, timing, _guard: guard }).await;
                                    });
                                    // The reservation must be on disk before anything else happens.
                                    persist_changed(&mut db, &sessions, &mut quorum, &paywall, &mut saved);
                                    continue;
                                }
                            }
                            Ok(_) => scrai_core::gateway::handle(&authority, &mut quorum, &mut sessions, &m.message).await,
                            Err(e) => fed_error(&id, format!("bad request: {e}")),
                        }
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
            persist_changed(&mut db, &sessions, &mut quorum, &paywall, &mut saved);
                    if let Err(e) = senders[to.idx].read().await.send_reply(to.tag, response).await {
                        eprintln!("scrai-server: reply failed: {e}");
                    }
            } // inbound = in_rx.recv()
        } // tokio::select!
    } // loop

    // Disconnecting flushes each Nym client's persistent stores (notably the
    // reply-SURB sqlite) so the next start finds them consistent.
    let _ = stop_tx.send(true);
    for t in recv_tasks {
        let _ = t.await;
    }
    println!("scrai-server: clean shutdown — mixnet state flushed.");
}

/// Where a reply goes: the Nym client that received the request (its reply SURBs live in
/// that client's store) and the anonymous sender tag the SDK gave the request.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ReplyTo {
    idx: usize,
    tag: AnonymousSenderTag,
}

/// Revision marks of the last persisted snapshot per store.
struct SavedRevs {
    sessions: u64,
    /// Quorum: revision of the small meta blob (records are pending-until-written, no mark).
    quorum_meta: u64,
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
    quorum: &mut QuorumStore,
    paywall: &pay::Pay,
    saved: &mut SavedRevs,
) {
    let sess_snap = (sessions.revision() != saved.sessions).then(|| sessions.snapshot());
    let quorum_meta = (quorum.meta_revision() != saved.quorum_meta).then(|| quorum.meta_json());
    let pay_snap = (paywall.revision() != saved.pay).then(|| paywall.snapshot());
    // Spends accepted since the last persist — rows + their serials, never a re-snapshot.
    // They stay pending (answerable from RAM) until the batch commits.
    let new_records = quorum.pending_records();
    let mut changed: Vec<(&str, &str)> = Vec::new();
    if let Some(s) = &sess_snap {
        changed.push(("sessions", s));
    }
    if let Some(s) = &quorum_meta {
        changed.push(("quorum_meta", s));
    }
    if let Some(s) = &pay_snap {
        changed.push(("pay", s));
    }
    if changed.is_empty() && new_records.is_empty() {
        return;
    }
    let t = std::time::Instant::now();
    let bytes: usize = changed.iter().map(|(_, s)| s.len()).sum::<usize>()
        + new_records.iter().map(|r| r.json.len()).sum::<usize>();
    match db.save_batch(&changed, &new_records) {
        Ok(()) => {
            saved.sessions = sessions.revision();
            quorum.clear_pending();
            saved.quorum_meta = quorum.meta_revision();
            saved.pay = paywall.revision();
        }
        Err(e) => eprintln!("scrai-server: atomic persist failed (will retry): {e}"),
    }
    // Whole-snapshot persistence runs ON the loop; make its cost visible once it matters.
    let ms = t.elapsed().as_millis();
    if ms >= 20 {
        eprintln!(
            "scrai-server: SLOW PERSIST {ms} ms — {}{} ({} KB)",
            changed.iter().map(|(k, _)| *k).collect::<Vec<_>>().join("+"),
            if new_records.is_empty() { String::new() } else { format!("+{} record(s)", new_records.len()) },
            bytes / 1024
        );
    }
}

/// Record today's peak of simultaneously served clients — one sqlite write per NEW high
/// (or per day), nothing on the steady state.
/// Credit every voucher that was burned but never paid out, and say so in the log — this
/// firing at all means a crash or a failed write happened, which is worth knowing about.
/// Returns true when something changed, so the caller persists.
fn credit_pending_vouchers(db: &store::Store, paywall: &mut pay::Pay) -> bool {
    let now = pay::now_ms();
    // App Store purchases have the same two-store shape and the same repair.
    let iap = db.iap_to_credit();
    for (hash, account, toku) in &iap {
        paywall.credit_voucher(account, *toku);
        db.iap_credited(hash, now);
    }
    if !iap.is_empty() {
        println!("scrai-server: repaired {} App Store purchase(s) that were claimed but never credited", iap.len());
    }
    let pending = db.vouchers_to_credit();
    if pending.is_empty() {
        return !iap.is_empty();
    }
    for (hash, account, toku) in &pending {
        paywall.credit_voucher(account, *toku);
        db.voucher_credited(hash, now);
    }
    println!(
        "scrai-server: repaired {} voucher(s) that were redeemed but never credited",
        pending.len()
    );
    true
}

fn note_peak(db: &store::Store, inflight: &inflight::Inflight<ReplyTo>, written: &mut (String, usize)) {
    let now = inflight.current();
    let today = today_utc();
    if written.0 != today || now > written.1 {
        db.bump_peak(&today, now);
        *written = (today, now);
    }
}

/// A positive usize from the environment, or the default.
fn env_usize(name: &str, default: usize) -> usize {
    scrai_server::cfg(name).ok().and_then(|v| v.trim().parse().ok()).filter(|n| *n > 0).unwrap_or(default)
}

/// The metrics day the current moment falls into, as `YYYY-MM-DD`, in the operator's
/// chosen zone (`METRICS_TZ`, default UTC). Pick the zone your provider's billing
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
    scrai_server::cfg("METRICS_TZ").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "UTC".into())
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
                    eprintln!("scrai-server: METRICS_TZ={s:?} not understood — using UTC");
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
        std::env::set_var("METRICS_TZ", "Europe/Berlin");
        assert_eq!(metrics_tz_offset(at(2026, 8, 28, 10)), 7_200);   // summer
        assert_eq!(metrics_tz_offset(at(2026, 1, 15, 10)), 3_600);   // winter
        assert_eq!(metrics_tz_offset(at(2026, 3, 29, 0)), 3_600);    // an hour before the switch
        assert_eq!(metrics_tz_offset(at(2026, 3, 29, 1)), 7_200);    // at the switch
        // 23:30 UTC on the 27th is already the 28th in Berlin
        assert_eq!(civil_day(at(2026, 8, 27, 23) + 1_800 + metrics_tz_offset(at(2026, 8, 27, 23))), "2026-08-28");
        std::env::set_var("METRICS_TZ", "America/Los_Angeles");
        assert_eq!(metrics_tz_offset(at(2026, 8, 28, 10)), -7 * 3_600);
        assert_eq!(metrics_tz_offset(at(2026, 12, 1, 10)), -8 * 3_600);
        // 03:00 UTC on the 28th is still the 27th in Los Angeles
        assert_eq!(civil_day(at(2026, 8, 28, 3) + metrics_tz_offset(at(2026, 8, 28, 3))), "2026-08-27");
        std::env::set_var("METRICS_TZ", "+02:00");
        assert_eq!(metrics_tz_offset(0), 7_200);
        std::env::set_var("METRICS_TZ", "UTC");
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
    random_described_gateway_excluding(&[]).await
}

/// Same pool, minus gateways already used by another of this server's identities — the
/// extra identities are only worth anything on DIFFERENT gateways.
async fn random_described_gateway_excluding(exclude: &[String]) -> Option<(String, String, String)> {
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
            if exclude.iter().any(|e| e == id) {
                return None;
            }
            Some((id.to_string(), country.to_string(), host.to_string()))
        })
        .collect();
    use rand::seq::SliceRandom;
    pool.choose(&mut rand::thread_rng()).cloned()
}

/// Load the pricing table: a file override (`PRICING`) if set, else the copy
/// embedded at build time — so the server always has a valid table.
/// Load `.env` from the working directory the way an operator writes it: `KEY=value`,
/// value = the rest of the line, optionally in single or double quotes, `#` comments on
/// their own line or after whitespace. UNQUOTED VALUES MAY CONTAIN SPACES — dotenvy
/// stopped parsing at such a line and silently dropped everything below it (a mnemonic
/// once, `PROVIDERS=gemini, openai` on 2026-09-03), which is how a freshly added
/// API key "wasn't there". Existing process-environment variables win, like dotenv.
fn load_env_lenient() {
    let Ok(text) = std::fs::read_to_string(".env") else { return };
    let mut loaded = 0usize;
    for (key, value) in parse_env_lines(&text) {
        if std::env::var_os(&key).is_some() {
            continue;
        }
        std::env::set_var(&key, &value);
        loaded += 1;
    }
    eprintln!("scrai-server: .env loaded ({loaded} variables)");
}

/// The lenient `.env` grammar (see `load_env_lenient`), as (key, value) pairs in order.
fn parse_env_lines(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            eprintln!("scrai-server: .env line {}: no `=` — ignored: {}", n + 1, line.chars().take(40).collect::<String>());
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            eprintln!("scrai-server: .env line {}: not a variable name — ignored: {}", n + 1, key.chars().take(40).collect::<String>());
            continue;
        }
        let mut value = value.trim().to_string();
        if value.len() >= 2 && ((value.starts_with('"') && value.ends_with('"')) || (value.starts_with('\'') && value.ends_with('\''))) {
            value = value[1..value.len() - 1].to_string();
        } else if let Some(i) = value.find(" #") {
            value.truncate(i);
            value = value.trim_end().to_string();
        }
        out.push((key.to_string(), value));
    }
    out
}

#[cfg(test)]
mod env_tests {
    use super::parse_env_lines;

    #[test]
    fn lenient_env_keeps_spaces_quotes_and_comments_straight() {
        let text = "# comment\nA=plain\nPROVIDERS=gemini, openai\nM=\"word word word\"\nS='single quoted'\nK=sk-proj-abc # trailing comment\nURL=https://x.y/#frag\nexport E=1\n\nbad line\n9X=nope\n";
        let v = parse_env_lines(text);
        let get = |k: &str| v.iter().find(|(kk, _)| kk == k).map(|(_, val)| val.as_str());
        assert_eq!(get("A"), Some("plain"));
        assert_eq!(get("PROVIDERS"), Some("gemini, openai")); // the 2026-09-03 case
        assert_eq!(get("M"), Some("word word word"));
        assert_eq!(get("S"), Some("single quoted"));
        assert_eq!(get("K"), Some("sk-proj-abc"));
        assert_eq!(get("URL"), Some("https://x.y/#frag")); // `#` without a space before it stays
        assert_eq!(get("E"), Some("1"));
        assert_eq!(v.len(), 8, "bad lines are skipped, nothing below them is lost");
    }
}

/// Has this identity registered before? (Its keys live in the dir once it has.)
/// Bring one persistent identity onto the mixnet at boot, RETRYING instead of panicking.
/// After a restart the gateway can still hold the previous process's session for the same
/// identity and refuse the re-registration for a while; a panic here made systemd restart
/// the server every five seconds until the gateway let go — about 75 s of crash loop after
/// every deploy (2026-09-11). Each attempt goes through `connect_identity` (fresh storage
/// handle and builder — the SDK's builder is consumed by `build`). Gives up only after
/// `MIX_CONNECT_RETRY_S` (default 300): a gateway that is really gone must still fail
/// loudly so the pin gets fixed.
async fn connect_identity_at_boot(dir: &Path, gateway: Option<String>, label: &str) -> MixnetClient {
    let budget = std::time::Duration::from_secs(env_usize("MIX_CONNECT_RETRY_S", 300) as u64);
    let started = std::time::Instant::now();
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match connect_identity(dir, gateway.as_deref()).await {
            Ok(c) => {
                if attempt > 1 {
                    println!("scrai-server: {label}: on the mixnet after {attempt} attempts ({} s)", started.elapsed().as_secs());
                }
                return c;
            }
            Err(e) if started.elapsed() < budget => {
                eprintln!("scrai-server: {label}: {e} — retrying in 5 s (attempt {attempt})");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
            Err(e) => panic!("{label}: {e} — gave up after {} s (MIX_CONNECT_RETRY_S)", started.elapsed().as_secs()),
        }
    }
}

/// How long a spend record is kept (seconds): `QUORUM_RETAIN_DAYS`, default 35 — a book
/// lives ~30 days from issue, a payment verifies at most 2 days past its spend date
/// (federation::SPEND_DATE_PAST_SECS), plus slack. Never below 33.
/// Most coins one `coins.return` may carry. A payment costs ~4 ms of pairings per coin,
/// so this bounds what a single request can ask the crypto pool for; a whole book comes
/// home in several batches.
const MAX_RETURN_COINS: u64 = 200;

fn quorum_retain_secs() -> u64 {
    env_usize("QUORUM_RETAIN_DAYS", 35).max(33) as u64 * 86_400
}

fn identity_exists(dir: &Path) -> bool {
    std::fs::read_dir(dir).map(|mut d| d.next().is_some()).unwrap_or(false)
}

/// Build + connect one identity from its storage dir, at `gateway` (re-homing it there if
/// it sat elsewhere). Used for reconnects with the gateway the identity already has, and by
/// `connect_identity_at_boot`, which wraps it in a retry loop for the initial connects.
async fn connect_identity(dir: &Path, gateway: Option<&str>) -> Result<MixnetClient, String> {
    let storage = StoragePaths::new_from_dir(dir).map_err(|e| format!("storage paths: {e}"))?;
    let mut b = MixnetClientBuilder::new_with_default_storage(storage)
        .await
        .map_err(|e| format!("client builder: {e}"))?;
    if let Some(g) = gateway {
        b = b.request_gateway(g.to_string());
    }
    if let Some(cfg) = server_traffic_config() {
        b = b.debug_config(cfg);
    }
    b.build()
        .map_err(|e| format!("build: {e}"))?
        .connect_to_mixnet()
        .await
        .map_err(|e| format!("connect: {e}"))
}

/// Mixnet traffic knobs for the server's own Nym client (see the call site). `None` when
/// neither variable is set, so the default stays byte-for-byte the SDK's.
fn server_traffic_config() -> Option<nym_sdk::DebugConfig> {
    let burst = scrai_server::cfg("MIX_BURST").as_deref() == Ok("1");
    let send_ms = scrai_server::cfg("MIX_SEND_MS").ok().and_then(|v| v.trim().parse::<u64>().ok());
    let cover_ms = scrai_server::cfg("MIX_COVER_MS").ok().and_then(|v| v.trim().parse::<u64>().ok());
    if !burst && send_ms.is_none() && cover_ms.is_none() {
        return None;
    }
    let mut d = nym_sdk::DebugConfig::default();
    if burst {
        // MIX_BURST=1 — THE server setting. The SDK's real-traffic stream is a
        // constant-rate Poisson stream: every tick sends a real packet if one is queued,
        // else a loop-cover packet. That shape hides a USER's traffic pattern; a service
        // provider has no pattern to hide, and pays for the padding with CPU: at
        // MIX_SEND_MS=4 × 10 identities the padding alone was 2,500 Sphinx packets/s
        // = all 6 cores of the VPS (2026-09-02). Disabling the Poisson distribution sends
        // real packets as soon as they are ready and nothing when idle; the separate loop
        // cover stream goes too. Replies are still SURB replies — the client's anonymity
        // does not depend on the server's sending shape.
        d.traffic.disable_main_poisson_packet_distribution = true;
        d.cover_traffic.disable_loop_cover_traffic_stream = true;
    }
    if let Some(ms) = send_ms {
        d.traffic.message_sending_average_delay = std::time::Duration::from_millis(ms.max(1));
    }
    if let Some(ms) = cover_ms {
        d.cover_traffic.loop_cover_traffic_average_delay = std::time::Duration::from_millis(ms.max(1));
    }
    println!(
        "scrai-server: mixnet traffic override — burst {}, send delay {} ms, cover delay {} ms",
        if burst { "ON (no Poisson padding, no loop cover)" } else { "off" },
        send_ms.map(|m| m.to_string()).unwrap_or_else(|| "default (20)".into()),
        cover_ms.map(|m| m.to_string()).unwrap_or_else(|| "default (200)".into())
    );
    if !burst {
        if let Some(ms) = send_ms {
            if ms < 20 {
                eprintln!(
                    "scrai-server: WARNING: MIX_SEND_MS={ms} without MIX_BURST=1 pads the idle stream \
                     with cover packets: ~{} Sphinx packets/s per identity, all CPU. Use MIX_BURST=1.",
                    (1000 / ms.max(1)) as usize
                );
            }
        }
    }
    Some(d)
}

fn load_pricing() -> PricingTable {
    const EMBEDDED: &str = include_str!("../../pricing.json");
    let from_file = scrai_server::cfg("PRICING")
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
    scrai_server::cfg("MARGIN")
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
