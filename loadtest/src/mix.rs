// mix.rs — one simulated user's mixnet client.
//
// An ephemeral nym-sdk client (fresh identity, in-memory storage) plus an id-dispatcher:
// replies are matched back to the waiting request by the protocol `id`, so a client may
// keep several requests in flight (ping/models) — exactly what the app does NOT do (it
// serialises), but it lets a few processes stand in for many users. Every request goes
// out with reply SURBs, so the server answers anonymously like it does for the app.

use nym_sdk::mixnet::{
    IncludedSurbs, MixnetClient, MixnetClientBuilder, MixnetClientSender, MixnetMessageSender, Recipient,
};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{oneshot, Notify};

/// The mixnet performance knobs — the SAME four the app's settings slider maps to
/// (src-tauri/src/nym.rs::debug_config_for), so a run can be labelled "as the app
/// ships" or "as fast as the slider goes".
#[derive(Clone, Copy, Debug)]
pub struct Perf {
    pub cover_ms: u64,
    pub mix_ms: u64,
    pub send_ms: u64,
    pub continuous: bool,
}

impl Perf {
    /// Nym's defaults = the app's privacy end of the slider.
    pub const PRIVACY: Perf = Perf { cover_ms: 200, mix_ms: 15, send_ms: 20, continuous: true };
    /// The app's performance end of the slider.
    pub const FAST: Perf = Perf { cover_ms: 3000, mix_ms: 2, send_ms: 3, continuous: true };

    fn debug_config(self) -> nym_sdk::DebugConfig {
        let mut d = nym_sdk::DebugConfig::default();
        d.traffic.average_packet_delay = Duration::from_millis(self.mix_ms.max(1));
        d.traffic.message_sending_average_delay = Duration::from_millis(self.send_ms.max(1));
        d.cover_traffic.loop_cover_traffic_average_delay = Duration::from_millis(self.cover_ms.max(1));
        d.cover_traffic.disable_loop_cover_traffic_stream = !self.continuous;
        d
    }
}

impl fmt::Display for Perf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "send {}ms · mix {}ms · cover {}ms ({})",
            self.send_ms,
            self.mix_ms,
            self.cover_ms,
            if self.continuous { "on" } else { "idle-only" }
        )
    }
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<(Value, usize)>>>>;

#[derive(Debug)]
pub enum CallError {
    /// The SDK refused to send (client dead, bad recipient …).
    Send(String),
    /// No reply with our id within the deadline.
    Timeout,
    /// The client's receive stream ended while we waited.
    Disconnected,
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallError::Send(e) => write!(f, "send failed: {e}"),
            CallError::Timeout => write!(f, "timeout"),
            CallError::Disconnected => write!(f, "mixnet client disconnected"),
        }
    }
}

pub struct Mix {
    sender: MixnetClientSender,
    pending: Pending,
    stop: Arc<Notify>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub address: String,
    pub gateway: String,
}

impl Mix {
    /// Connect a fresh ephemeral client. `gateway` pins the entry gateway (identity key);
    /// `None` lets the SDK pick one, so a fleet of clients spreads over many gateways —
    /// the realistic case, since real users sit on different gateways too.
    pub async fn connect(perf: Perf, gateway: Option<&str>) -> Result<Mix, String> {
        let mut builder = MixnetClientBuilder::new_ephemeral().debug_config(perf.debug_config());
        if let Some(g) = gateway {
            builder = builder.request_gateway(g.to_string());
        }
        let client = builder
            .build()
            .map_err(|e| format!("build: {e}"))?
            .connect_to_mixnet()
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let address = client.nym_address().to_string();
        let gateway = client.nym_address().gateway().to_base58_string();
        let sender = client.split_sender();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let stop = Arc::new(Notify::new());
        // Boxed: the SDK's receive future is huge; keep it off the spawning task's stack.
        let task = tokio::spawn(Box::pin(receiver_loop(client, pending.clone(), stop.clone())));
        Ok(Mix { sender, pending, stop, task: Mutex::new(Some(task)), address, gateway })
    }

    /// Send one request and wait for the reply carrying the same `id`.
    /// Returns (reply, reply bytes). A delivered `kind:"error"` is still `Ok` here —
    /// the caller decides what an application error means.
    pub async fn call(
        &self,
        to: &Recipient,
        req: &Value,
        surbs: u32,
        timeout: Duration,
    ) -> Result<(Value, usize), CallError> {
        let id = req
            .get("id")
            .and_then(|i| i.as_str())
            .ok_or_else(|| CallError::Send("request without id".into()))?
            .to_string();
        let bytes = serde_json::to_vec(req).map_err(|e| CallError::Send(e.to_string()))?;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        if let Err(e) = self.sender.send_message(*to, bytes, IncludedSurbs::new(surbs)).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(CallError::Send(e.to_string()));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(CallError::Disconnected),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(CallError::Timeout)
            }
        }
    }

    /// Stop receiving and disconnect cleanly (flushes the SDK's state).
    pub async fn shutdown(&self) {
        self.stop.notify_one();
        let task = self.task.lock().unwrap().take();
        if let Some(t) = task {
            let _ = t.await;
        }
    }
}

async fn receiver_loop(mut client: MixnetClient, pending: Pending, stop: Arc<Notify>) {
    loop {
        tokio::select! {
            _ = stop.notified() => break,
            batch = client.wait_for_messages() => {
                let Some(messages) = batch else { break };
                for m in messages {
                    let Ok(v) = serde_json::from_slice::<Value>(&m.message) else { continue };
                    let Some(id) = v.get("id").and_then(|i| i.as_str()) else { continue };
                    let waiter = pending.lock().unwrap().remove(id);
                    if let Some(tx) = waiter {
                        let _ = tx.send((v, m.message.len()));
                    }
                }
            }
        }
    }
    // Wake every waiter with Disconnected rather than leaving them to time out.
    pending.lock().unwrap().clear();
    client.disconnect().await;
}
