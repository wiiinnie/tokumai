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
    enclave.set_address(&address);
    let gateway = client.nym_address().gateway().to_base58_string();
    let frames: &'static Frames = Box::leak(Box::new(Frames::default()));
    let sender: Arc<RwLock<MixnetClientSender>> = Arc::new(RwLock::new(client.split_sender()));
    loop {
        while let Some(batch) = client.wait_for_messages().await {
            for m in batch {
                let Some(tag) = m.sender_tag else { continue };
                let sender = sender.clone();
                tokio::spawn(Box::pin(answer(enclave, frames, sender, tag, m.message)));
            }
        }
        eprintln!("tokumai-server: the mixnet stream ended — reconnecting the same identity");
        client = reconnect(&dir, &gateway).await;
        *sender.write().await = client.split_sender();
        println!("tokumai-server: back on the mixnet: {}", client.nym_address());
    }
}

async fn answer(enclave: &'static Enclave, frames: &'static Frames, sender: Arc<RwLock<MixnetClientSender>>, tag: AnonymousSenderTag, frame: Vec<u8>) {
    let reply = frames.handle(&frame, |message| async move { enclave.handle(&message).await }).await;
    if let Some(reply) = reply {
        if let Err(e) = sender.read().await.send_reply(tag, reply).await {
            eprintln!("tokumai-server: a reply could not be sent: {e}");
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
