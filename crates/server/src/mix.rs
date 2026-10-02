//! The enclave's end of the Nym mixnet (the app's is `tokumai_client::mix`). Lessons carried over from the first server (tokumai 0.x):
//!
//! - The server's own sending is not padded (no Poisson stream, no loop cover). Padding it
//!   would not protect a user: whoever runs the host sees the enclave's side anyway, and to
//!   tell WHO asked they would also need the user's side — which the app's own cover
//!   traffic hides for sending, and the app's choice of an entry gateway not run by us
//!   (a must for the app) hides for receiving. The padding also cost all cores at load.
//! - An identity whose stream ends is rebuilt with the same keys, so the address stays; a
//!   reply-SURB store left broken by a crash is wiped on the second attempt.
//! - Every exchange has its own id, and a question still being answered is not answered
//!   twice (`frames`), so a resend never gets someone else's answer, or a stale one.

use nym_sdk::mixnet::{AnonymousSenderTag, MixnetClient, MixnetClientBuilder, MixnetClientSender, MixnetMessageSender, StoragePaths};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokumai_proto::frames::Frames;
use tokumai_enclave::service::Enclave;

/// How often each door posts a packet to ITSELF, and how long a door may hear nothing at
/// all before it counts as deaf and is rebuilt.
///
/// This exists because of 2026-09-24, and again overnight: the enclave sat with
/// `wait_for_messages()` pending on a gateway connection that was, from outside, gone.
/// Nothing ever ended, so the reconnect below never ran, and no line was logged because
/// there was nothing to log. `nitro-cli` said RUNNING, the Nym client kept fetching the
/// topology, and all three doors were unreachable for hours. Silence cannot be noticed by
/// looking at it — only by expecting something and missing it.
///
/// A packet addressed to our own address travels the whole way: out through our gateway,
/// across the mix nodes, back in through the same gateway. If it arrives, the path is open
/// in both directions, which is the only claim worth making. It carries no reply SURB, so
/// it arrives without a `sender_tag` and the request loop skips it by itself — it is not a
/// message in the protocol, it is evidence that messages arrive.
///
/// Three misses before acting: a single lost packet is ordinary over a mixnet, and one
/// missing echo must not tear down a door that works.
const PING_EVERY: Duration = Duration::from_secs(120);
const DEAF_AFTER_MS: u64 = 7 * 60 * 1000;

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Every door that is open: its address and the sender that posts through it. The ghosts
/// (`tokumai_enclave::ghost`) need one door to send from and another to send to.
struct Door {
    address: String,
    sender: Arc<RwLock<MixnetClientSender>>,
}

static DOORS: std::sync::Mutex<Vec<Door>> = std::sync::Mutex::new(Vec::new());

/// Redemptions that nobody made, in the hours in which nobody does (`tokumai_enclave::ghost`
/// has the rule and the shape). Every minute: how many redemption-shaped events the last
/// hour holds, real and made; the floor less that, spread over the hour, is the chance of
/// making one now — from a random door to another random door.
pub async fn ghosts(enclave: &'static Enclave) {
    use tokumai_enclave::ghost::{request, GHOST_FLOOR};
    let mut said_alone = false;
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let wanted = enclave.ghosts_wanted();
        if wanted == 0 {
            continue;
        }
        // Spread over the hour: with the whole floor wanted, one every 60 / FLOOR minutes.
        let chance = wanted as f64 / 60.0 * (60.0 / GHOST_FLOOR as f64).min(60.0) / (60.0 / GHOST_FLOOR as f64);
        if rand::random::<f64>() >= chance {
            continue;
        }
        let pick = {
            let doors = DOORS.lock().map(|d| d.iter().map(|x| (x.address.clone(), x.sender.clone())).collect::<Vec<_>>()).unwrap_or_default();
            if doors.len() < 2 {
                None
            } else {
                let a = rand::random::<usize>() % doors.len();
                let mut b = rand::random::<usize>() % (doors.len() - 1);
                if b >= a {
                    b += 1;
                }
                Some((doors[a].0.clone(), doors[b].0.clone(), doors[b].1.clone()))
            }
        };
        let Some((to, from, sender)) = pick else {
            if !said_alone {
                crate::say("ghosts: one door only — a redemption-shaped packet needs two, none made".to_string());
                said_alone = true;
            }
            continue;
        };
        let Ok(recipient) = nym_sdk::mixnet::Recipient::try_from_base58_string(&to) else { continue };
        let posted = {
            let guard = sender.read().await;
            guard.send_plain_message(recipient, request(&from)).await
        };
        if let Err(e) = posted {
            crate::say(format!("ghosts: could not post: {e}"));
        }
    }
}

/// A ghost request arrived at this door: do what a redemption does, then answer the door
/// it came from with a reply-sized packet.
async fn answer_ghost(enclave: &'static Enclave, sender: Arc<RwLock<MixnetClientSender>>, reply_to: String) {
    let _ = enclave.ghost_redemption().await;
    let Ok(recipient) = nym_sdk::mixnet::Recipient::try_from_base58_string(&reply_to) else { return };
    let guard = sender.read().await;
    let _ = guard.send_plain_message(recipient, tokumai_enclave::ghost::reply()).await;
}

/// The server's traffic shape (see the module notes).
fn server_config() -> nym_sdk::DebugConfig {
    let mut d = nym_sdk::DebugConfig::default();
    d.traffic.disable_main_poisson_packet_distribution = true;
    d.cover_traffic.disable_loop_cover_traffic_stream = true;
    d
}

/// Connect the persistent identity kept in `dir`, at `gateway` when one is pinned.
pub async fn connect(dir: &Path, gateway: Option<&str>) -> Result<MixnetClient, String> {
    let storage = StoragePaths::new_from_dir(dir).map_err(|e| format!("storage paths: {e}"))?;
    let mut b = MixnetClientBuilder::new_with_default_storage(storage).await.map_err(|e| format!("client builder: {e}"))?;
    if let Some(g) = gateway {
        b = b.request_gateway(g.to_string());
    }
    b.debug_config(server_config())
        .build()
        .map_err(|e| format!("build: {e}"))?
        .connect_to_mixnet()
        .await
        .map_err(|e| format!("connect: {e}"))
}

/// Connect, retrying for up to five minutes: after a restart the gateway can still hold the
/// last process's session for this identity and refuse it for a while.
pub async fn connect_at_boot(dir: &Path, gateway: Option<&str>) -> Result<MixnetClient, String> {
    let started = std::time::Instant::now();
    loop {
        match connect(dir, gateway).await {
            Ok(c) => return Ok(c),
            Err(e) if started.elapsed() < Duration::from_secs(300) => {
                eprintln!("tokumai-server: mixnet: {e} — retrying in 5 s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Answer everything that arrives at `client`, for as long as the process runs. The
/// enclave learns the address first, so every attestation from now on names it.
pub async fn serve(enclave: &'static Enclave, mut client: MixnetClient, dir: PathBuf) {
    let address = client.nym_address().to_string();
    // The door this loop answers at. With several doors each has its own loop, and each
    // request is answered in the name of the door it came through.
    enclave.set_address(&address);
    let address: &'static str = Box::leak(address.into_boxed_str());
    let gateway = client.nym_address().gateway().to_base58_string();
    // A short name for the log, so a line says WHICH of the three doors it is about.
    let short: &'static str = Box::leak(gateway.chars().take(8).collect::<String>().into_boxed_str());
    let frames: &'static Frames = Box::leak(Box::new(Frames::default()));
    let sender: Arc<RwLock<MixnetClientSender>> = Arc::new(RwLock::new(client.split_sender()));
    if let Ok(mut doors) = DOORS.lock() {
        doors.push(Door { address: address.to_string(), sender: sender.clone() });
    }
    // When this door last heard anything at all, and the bell that wakes the loop when it
    // has heard nothing for too long (see PING_EVERY).
    let heard = Arc::new(std::sync::atomic::AtomicU64::new(now_ms()));
    let deaf = Arc::new(tokio::sync::Notify::new());
    {
        let (sender, heard, deaf) = (sender.clone(), heard.clone(), deaf.clone());
        let me = *client.nym_address();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(PING_EVERY).await;
                if let Err(e) = sender.read().await.send_plain_message(me, b"tokumai/still-there").await {
                    crate::say(format!("door {short}: could not post to our own address: {e}"));
                }
                if now_ms().saturating_sub(heard.load(std::sync::atomic::Ordering::Relaxed)) > DEAF_AFTER_MS {
                    deaf.notify_one();
                }
            }
        });
    }
    loop {
        loop {
            // Either something arrives, or the watchdog says nothing has for too long. The
            // second is what the old `while let` could not express: a stream that neither
            // yields nor ends leaves a loop with nothing to react to.
            let batch = tokio::select! {
                b = client.wait_for_messages() => b,
                _ = deaf.notified() => {
                    crate::say(format!("door {short}: nothing has arrived for {} minutes — rebuilding it", DEAF_AFTER_MS / 60_000));
                    None
                }
            };
            let Some(batch) = batch else { break };
            heard.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
            for m in batch {
                // Our own packet comes back without a tag, having proved the point — unless
                // it is a ghost request from another door, which is answered like a
                // redemption (`tokumai_enclave::ghost`).
                let Some(tag) = m.sender_tag else {
                    if let Some(back) = tokumai_enclave::ghost::reply_to(&m.message) {
                        tokio::spawn(answer_ghost(enclave, sender.clone(), back));
                    }
                    continue;
                };
                let sender = sender.clone();
                tokio::spawn(Box::pin(answer(enclave, frames, sender, tag, m.message, address)));
            }
        }
        crate::say(format!("door {short}: the mixnet stream ended — reconnecting the same identity"));
        client = reconnect(&dir, &gateway).await;
        // A fresh door has heard nothing yet, and must not be torn down for it.
        heard.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
        *sender.write().await = client.split_sender();
        crate::say(format!("door {short}: back on the mixnet"));
    }
}

/// How many requests the enclave works on at once, all doors together. Every mixnet
/// message used to become a task with no ceiling: a burst of pictures was a burst of
/// provider calls and of megabytes held until each came back. Beyond this, requests wait
/// their turn, in memory, with nothing started for them yet.
const INFLIGHT: usize = 48;
static WORKING: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(INFLIGHT);

async fn answer(enclave: &'static Enclave, frames: &'static Frames, sender: Arc<RwLock<MixnetClientSender>>, tag: AnonymousSenderTag, frame: Vec<u8>, at: &'static str) {
    let reply = frames
        .handle(&frame, |message| async move {
            let _turn = WORKING.acquire().await;
            enclave.handle_at(&message, at).await
        })
        .await;
    if let Some(reply) = reply {
        let n = reply.len();
        if tokumai_enclave::trace::ON {
            crate::say(format!("reply of {n} bytes handed to the mixnet"));
        }
        if let Err(e) = sender.read().await.send_reply(tag, reply).await {
            crate::say(format!("a reply could not be sent: {e}"));
        }
    }
}

async fn reconnect(dir: &Path, gateway: &str) -> MixnetClient {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        tokio::time::sleep(Duration::from_secs(if attempt == 1 { 3 } else { 15 })).await;
        if attempt >= 2 {
            for f in ["persistent_reply_store.sqlite", "persistent_reply_store.sqlite-wal", "persistent_reply_store.sqlite-shm"] {
                let _ = std::fs::remove_file(dir.join(f));
            }
        }
        match connect(dir, Some(gateway)).await {
            Ok(c) => return c,
            Err(e) => eprintln!("tokumai-server: reconnect attempt {attempt} failed: {e}"),
        }
    }
}
