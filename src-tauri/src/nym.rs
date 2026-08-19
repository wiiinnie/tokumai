// ---------------------------------------------------------------------------
// nym.rs — embedded mixnet transport (nym-sdk).
//
// One persistent ephemeral mixnet client for the app's lifetime. Requests go to
// the scrai-server's Nym address with reply SURBs (anonymous — the server never
// learns our address), and replies are matched back by the protocol `id`. This
// is the Rust equivalent of the CLI's NymSocket, but with no external binary.
//
// Requests are serialised one at a time (a single-user desktop app), so a plain
// mutex around the client is enough — no id-dispatcher needed.
//
// The transport also knows the ROUTE EDGES it can honestly report:
//   - entry  = the gateway THIS client is attached to (user-selectable).
//   - exit   = the scrai-server's gateway (fixed — it is where the server lives).
// The two middle mix hops are re-randomised per packet by the SDK and are not
// knowable or selectable; that per-packet reshuffling is the anonymity itself.
// Gateway country is resolved from the Nym directory (a direct, non-mixnet call).
// ---------------------------------------------------------------------------

use nym_sdk::mixnet::{
    IncludedSurbs, MixnetClient, MixnetClientBuilder, MixnetMessageSender, Recipient,
};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::Mutex;

/// The Nym mainnet directory endpoint listing self-described nodes (identity,
/// declared role, and self-reported ISO country). `connect_new()` defaults to
/// mainnet, so this is the matching directory.
const NYM_DIRECTORY: &str = "https://validator.nymtech.net/api/v1/nym-nodes/described";

#[derive(Clone, serde::Serialize)]
pub struct GatewayInfo {
    pub id: String,
    /// ISO-3166 alpha-2, self-reported by the node (e.g. "CH"). Empty if unknown.
    pub country: String,
    pub host: String,
    /// Whether the node advertises itself as usable as an entry gateway.
    pub entry: bool,
}

pub struct Transport {
    client: Mutex<Option<MixnetClient>>,
    /// Lock-free view of the connection for the UI's status poll: the client
    /// mutex is held for a WHOLE round trip (up to 2 minutes on a big reply),
    /// so a status query must never need it — or the route indicator freezes
    /// grey exactly while traffic is flowing.
    live: std::sync::atomic::AtomicBool,
    /// Entry gateway of the live client, cached at connect (same lock-free reason).
    entry_cached: std::sync::Mutex<Option<String>>,
    /// Guards against stacking multiple background reconnect tasks.
    reconnecting: std::sync::atomic::AtomicBool,
    /// User-chosen entry gateway identity; None → let the SDK pick.
    chosen_gateway: Mutex<Option<String>>,
    /// Model catalog, cached after the first fetch (it rarely changes).
    models: Mutex<Option<Value>>,
    /// identity → info, fetched once from the directory and cached.
    geo: Mutex<Option<HashMap<String, GatewayInfo>>>,
    /// Serialises whole multi-round-trip operations (chat, redeem, collect) so two
    /// concurrent chats can't interleave their session_status+chat round trips and
    /// race the session counter (which crossed replies and hung the UI).
    op_lock: Mutex<()>,
}

impl Transport {
    pub fn new() -> Self {
        Self {
            client: Mutex::new(None),
            live: std::sync::atomic::AtomicBool::new(false),
            entry_cached: std::sync::Mutex::new(None),
            reconnecting: std::sync::atomic::AtomicBool::new(false),
            chosen_gateway: Mutex::new(None),
            models: Mutex::new(None),
            geo: Mutex::new(None),
            op_lock: Mutex::new(()),
        }
    }

    /// Acquire the whole-operation lock. Hold the returned guard for the entire
    /// command (chat/redeem/collect) so its round trips run as one atomic unit.
    pub async fn begin_op(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.op_lock.lock().await
    }

    /// The gateway identity embedded in a Nym recipient address (the `@…` part).
    /// This is how the app learns the scrai-server's (exit) gateway.
    pub fn gateway_of(addr: &str) -> Option<String> {
        Recipient::try_from_base58_string(addr)
            .ok()
            .map(|r| r.gateway().to_base58_string())
    }

    pub async fn cached_models(&self) -> Option<Value> {
        self.models.lock().await.clone()
    }
    pub async fn set_cached_models(&self, v: Value) {
        *self.models.lock().await = Some(v);
    }

    /// Set (or clear) the preferred entry gateway. Drops the live client so the
    /// next request re-attaches through the chosen gateway.
    pub async fn set_entry_gateway(&self, id: Option<String>) {
        *self.chosen_gateway.lock().await = id;
        self.mark_dead();
        if let Some(c) = self.client.lock().await.take() {
            c.disconnect().await;
        }
    }

    /// Is there a LIVE mixnet client right now? False after a transport failure
    /// dropped it (the route indicator uses this — the chosen-gateway fallback
    /// in `entry_gateway_id` must not make a dead connection look green).
    pub fn is_connected(&self) -> bool {
        self.live.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Connect if not connected — idempotent, safe to call before every request
    /// AND from the UI's route poll, which makes it the background reconnect
    /// after a dropped client.
    pub async fn ensure_connected(&self) -> Result<(), String> {
        let mut guard = self.client.lock().await;
        if guard.is_some() {
            return Ok(());
        }
        let chosen = self.chosen_gateway.lock().await.clone();
        // Cap the connect so a truly hung registration can't leave the app stuck
        // at "connecting" forever. GENEROUS on purpose: connect includes the
        // initial topology fetch from the central nym-api, and when that API is
        // degraded (observed: 60s+ per request, with internal retries) a whole
        // connect legitimately takes 2–3 minutes — a tight cap would abort every
        // attempt just before it finishes and loop forever. Healthy-network
        // connects take 5–10s and never feel this value.
        const CONNECT_TIMEOUT: Duration = Duration::from_secs(180);
        let connect = async {
            // No user choice → curated random (described gateways only);
            // only if even the directory fails, let the SDK pick blindly.
            let gw = match chosen {
                Some(gw) => Some(gw),
                None => self.random_described_gateway().await,
            };
            match gw {
                Some(gw) => MixnetClientBuilder::new_ephemeral()
                    .request_gateway(gw)
                    .build()
                    .map_err(|e| format!("mixnet build failed: {e}"))?
                    .connect_to_mixnet()
                    .await
                    .map_err(|e| format!("mixnet connect failed: {e}")),
                None => MixnetClient::connect_new()
                    .await
                    .map_err(|e| format!("mixnet connect failed: {e}")),
            }
        };
        let c = tokio::time::timeout(CONNECT_TIMEOUT, connect)
            .await
            .map_err(|_| "mixnet connect timed out — gateway unreachable, please try again".to_string())??;
        *self.entry_cached.lock().unwrap() = Some(c.nym_address().gateway().to_base58_string());
        self.live.store(true, std::sync::atomic::Ordering::Relaxed);
        *guard = Some(c);
        Ok(())
    }

    /// Mark the connection dead (lock-free view) — called wherever the client
    /// is dropped after a transport failure.
    fn mark_dead(&self) {
        self.live.store(false, std::sync::atomic::Ordering::Relaxed);
        *self.entry_cached.lock().unwrap() = None;
    }

    /// Kick off ONE background reconnect if the connection is down. Non-blocking:
    /// the UI's status poll calls this and returns immediately — the route flips
    /// to green on a later poll once the task has connected.
    pub fn spawn_reconnect(self: &std::sync::Arc<Self>) {
        use std::sync::atomic::Ordering;
        if self.live.load(Ordering::Relaxed) {
            return;
        }
        if self.reconnecting.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            return; // one reconnect task at a time
        }
        let t = self.clone();
        tokio::spawn(async move {
            if let Err(e) = t.ensure_connected().await {
                log::warn!("[mixnet] background reconnect failed: {e}");
            }
            t.reconnecting.store(false, Ordering::SeqCst);
        });
    }

    /// The entry gateway identity of the CURRENTLY connected client, if any.
    /// Falls back to the chosen-but-not-yet-connected gateway. None → not
    /// connected and no explicit choice yet.
    pub async fn entry_gateway_id(&self) -> Option<String> {
        if let Some(id) = self.entry_cached.lock().unwrap().clone() {
            return Some(id);
        }
        self.chosen_gateway.lock().await.clone()
    }

    /// Look up one gateway's directory info by identity (cached; fetches once).
    pub async fn gateway_info(&self, id: &str) -> Option<GatewayInfo> {
        self.geo_map().await.ok()?.get(id).cloned()
    }

    /// A random entry gateway among the WELL-DESCRIBED directory nodes: entry
    /// role plus a self-reported location AND a real hostname (reverse DNS) —
    /// anonymous, IP-only gateways are skipped so a random route never lands on
    /// a node the user can't identify in the UI. None if the directory is
    /// unreachable or the curated pool is empty (caller falls back to SDK pick).
    pub async fn random_described_gateway(&self) -> Option<String> {
        let map = self.geo_map().await.ok()?;
        let pool: Vec<&GatewayInfo> = map
            .values()
            .filter(|g| {
                g.entry
                    && !g.country.is_empty()
                    && !g.host.is_empty()
                    && g.host.parse::<std::net::IpAddr>().is_err()
            })
            .collect();
        use rand::seq::SliceRandom;
        pool.choose(&mut rand::thread_rng()).map(|g| g.id.clone())
    }

    /// All directory nodes that advertise as entry gateways, for the picker.
    pub async fn entry_gateways(&self) -> Result<Vec<GatewayInfo>, String> {
        let map = self.geo_map().await?;
        let mut v: Vec<GatewayInfo> = map.values().filter(|g| g.entry).cloned().collect();
        v.sort_by(|a, b| a.country.cmp(&b.country).then(a.host.cmp(&b.host)));
        Ok(v)
    }

    /// Fetch + cache the directory (identity → info). Kept behind the mutex so a
    /// slow first fetch does not race a second caller into a duplicate request.
    async fn geo_map(&self) -> Result<HashMap<String, GatewayInfo>, String> {
        let mut guard = self.geo.lock().await;
        if let Some(m) = guard.as_ref() {
            return Ok(m.clone());
        }
        let body: Value = reqwest::Client::new()
            .get(NYM_DIRECTORY)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("directory fetch failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("directory parse failed: {e}"))?;

        let mut map = HashMap::new();
        if let Some(items) = body.get("data").and_then(|d| d.as_array()) {
            for it in items {
                let d = it.get("description").unwrap_or(&Value::Null);
                let id = d
                    .pointer("/host_information/keys/ed25519")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if id.is_empty() {
                    continue;
                }
                let country = d
                    .pointer("/auxiliary_details/location")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let host = d
                    .pointer("/host_information/hostname")
                    .and_then(|v| v.as_str())
                    .or_else(|| {
                        d.pointer("/host_information/ip_address/0").and_then(|v| v.as_str())
                    })
                    .unwrap_or_default()
                    .to_string();
                let entry = d
                    .pointer("/declared_role/entry")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                map.insert(id.clone(), GatewayInfo { id, country, host, entry });
            }
        }
        *guard = Some(map.clone());
        Ok(map)
    }

    /// Send one request and return the reply whose `id` matches. Blocks other
    /// requests until it completes (fine for a single user).
    pub async fn round_trip(&self, server: &str, req: &Value, surbs: u32, timeout_ms: u64) -> Result<Value, String> {
        self.round_trip_notify(server, req, surbs, timeout_ms, || {}).await
    }

    /// Like `round_trip`, but calls `on_sent` the moment the request has been
    /// handed to the mixnet (send completed, reply not yet in) — the UI uses this
    /// to switch its status line from "sending" to "thinking" at the real instant.
    pub async fn round_trip_notify(
        &self,
        server: &str,
        req: &Value,
        surbs: u32,
        timeout_ms: u64,
        on_sent: impl FnOnce(),
    ) -> Result<Value, String> {
        let recipient =
            Recipient::try_from_base58_string(server).map_err(|e| format!("bad server address: {e}"))?;
        let id = req.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let bytes = serde_json::to_vec(req).map_err(|e| e.to_string())?;

        self.ensure_connected().await?;
        let mut guard = self.client.lock().await;
        if guard.is_none() {
            return Err("mixnet not connected — please retry".into());
        }

        // Any transport-level failure below DROPS the client: the SDK gives up
        // and shuts the whole embedded client down after enough gateway errors,
        // and a dead client held in the slot means every further send fails
        // forever ("stuck at connected"). Dropping it makes the next attempt —
        // user retry or the UI's route poll — build a fresh one.
        if let Err(e) = guard
            .as_mut()
            .unwrap()
            .send_message(recipient, bytes, IncludedSurbs::new(surbs))
            .await
        {
            *guard = None;
            self.mark_dead();
            return Err(format!("mixnet send failed: {e} — reconnecting on the next attempt"));
        }
        on_sent();

        const TIMEOUT_MSG: &str =
            "no reply from the mixnet in time — reconnecting on the next attempt";
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now())
            else {
                *guard = None;
                self.mark_dead();
                return Err(TIMEOUT_MSG.into());
            };
            let batch = match tokio::time::timeout(
                remaining,
                guard.as_mut().unwrap().wait_for_messages(),
            )
            .await
            {
                Ok(b) => b,
                Err(_) => {
                    *guard = None;
                    self.mark_dead();
                    return Err(TIMEOUT_MSG.into());
                }
            };
            let Some(messages) = batch else {
                *guard = None;
                self.mark_dead();
                return Err("mixnet stream ended — reconnecting on the next attempt".into());
            };
            for m in messages {
                if let Ok(v) = serde_json::from_slice::<Value>(&m.message) {
                    if v.get("id").and_then(|x| x.as_str()) == Some(id.as_str()) {
                        if v.get("kind").and_then(|k| k.as_str()) == Some("error") {
                            let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("server error");
                            return Err(err.to_string());
                        }
                        return Ok(v);
                    }
                }
            }
        }
    }
}
