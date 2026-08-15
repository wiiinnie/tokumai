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
        if let Some(c) = self.client.lock().await.take() {
            c.disconnect().await;
        }
    }

    /// The entry gateway identity of the CURRENTLY connected client, if any.
    /// Falls back to the chosen-but-not-yet-connected gateway. None → not
    /// connected and no explicit choice yet.
    pub async fn entry_gateway_id(&self) -> Option<String> {
        if let Some(c) = self.client.lock().await.as_ref() {
            return Some(c.nym_address().gateway().to_base58_string());
        }
        self.chosen_gateway.lock().await.clone()
    }

    /// Look up one gateway's directory info by identity (cached; fetches once).
    pub async fn gateway_info(&self, id: &str) -> Option<GatewayInfo> {
        self.geo_map().await.ok()?.get(id).cloned()
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
        let recipient =
            Recipient::try_from_base58_string(server).map_err(|e| format!("bad server address: {e}"))?;
        let id = req.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let bytes = serde_json::to_vec(req).map_err(|e| e.to_string())?;

        let mut guard = self.client.lock().await;
        if guard.is_none() {
            let chosen = self.chosen_gateway.lock().await.clone();
            let c = match chosen {
                Some(gw) => MixnetClientBuilder::new_ephemeral()
                    .request_gateway(gw)
                    .build()
                    .map_err(|e| format!("mixnet build failed: {e}"))?
                    .connect_to_mixnet()
                    .await
                    .map_err(|e| format!("mixnet connect failed: {e}"))?,
                None => MixnetClient::connect_new()
                    .await
                    .map_err(|e| format!("mixnet connect failed: {e}"))?,
            };
            *guard = Some(c);
        }
        let client = guard.as_mut().unwrap();

        client
            .send_message(recipient, bytes, IncludedSurbs::new(surbs))
            .await
            .map_err(|e| format!("mixnet send failed: {e}"))?;

        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .ok_or_else(|| "no reply from the mixnet in time".to_string())?;
            let batch = tokio::time::timeout(remaining, client.wait_for_messages())
                .await
                .map_err(|_| "no reply from the mixnet in time".to_string())?;
            let Some(messages) = batch else {
                return Err("mixnet stream ended".to_string());
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
