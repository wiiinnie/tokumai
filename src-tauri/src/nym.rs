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

/// Build the mixnet `DebugConfig` for a performance/privacy setting. Extracted as a
/// free function so the mapping is unit-testable without a live mixnet, and so the
/// standalone `mixbench` diagnostic uses the EXACT same knobs the app does.
///   cover_ms → cover-traffic rate (loop_cover_traffic_average_delay): battery
///   mix_ms   → per-hop mixing delay (average_packet_delay): latency
///   send_ms  → real-packet send rate (message_sending_average_delay): THROUGHPUT
///   continuous → keep the loop cover-traffic stream (disable_loop_cover_traffic_stream)
/// Nym's own defaults are (200, 15, 20, true) = maximum privacy.
pub fn debug_config_for(cover_ms: u64, mix_ms: u64, send_ms: u64, continuous: bool) -> nym_sdk::DebugConfig {
    let mut dbg = nym_sdk::DebugConfig::default();
    dbg.traffic.average_packet_delay = Duration::from_millis(mix_ms.max(1));
    dbg.traffic.message_sending_average_delay = Duration::from_millis(send_ms.max(1));
    dbg.cover_traffic.loop_cover_traffic_average_delay = Duration::from_millis(cover_ms.max(1));
    dbg.cover_traffic.disable_loop_cover_traffic_stream = !continuous;
    dbg
}

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
    /// Mixnet performance/privacy tradeoff: (cover_delay_ms, mix_delay_ms, send_delay_ms,
    /// continuous_cover). Default (200, 15, 20, true) is Nym's own settings — full loop
    /// cover traffic, standard mixing + send rate = maximum privacy. Higher cover_delay =
    /// less cover traffic (battery); lower mix_delay = less per-hop mixing (latency); lower
    /// send_delay = faster real-packet emission (THROUGHPUT — the upload-speed lever). All
    /// trade anonymity for performance and are OPT-IN (the default keeps privacy primary).
    perf: std::sync::Mutex<(u64, u64, u64, bool)>,
    /// The last chat request that has NOT been acked with a reply, keyed by session id.
    /// A retry resends this VERBATIM (same counter/sig/id) so a server that already
    /// processed it replies by replay instead of charging a second time (idempotent retry).
    pending_chat: Mutex<Option<(String, Value)>>,
    /// A chat reply whose big pictures are still being fetched chunk by chunk (see
    /// `fetch_staged_images` in lib.rs), kept across a FAILED download so the UI's Retry
    /// resumes the fetch — the picture is already paid for and staged on the server —
    /// instead of generating (and charging for) a brand-new one.
    staged_download: Mutex<Option<StagedDownload>>,
    /// Fired by the UI's Cancel: every wait on the mixnet (chat reply, chunk download)
    /// returns early with a "cancelled" error. The request itself is NOT withdrawn — it
    /// already left for the mixnet and the server may still process (and bill) it; the
    /// pending chat stays set, so Retry replays it idempotently instead of paying twice.
    cancel: tokio::sync::Notify,
    /// Connect-progress sink (lib.rs wires it to a Tauri event): the UI shows the same
    /// steps the boot animation types — keys · client · gateway · cover · ready/failed.
    progress: std::sync::Mutex<Option<Box<dyn Fn(&str, &str) + Send + Sync>>>,
}

/// The state of a chunked picture download: the chat reply holding the chunk
/// references, plus every chunk that has already arrived (by image ref → seq).
#[derive(Clone, Debug)]
pub struct StagedDownload {
    pub session_id: String,
    pub resp: Value,
    pub parts: HashMap<String, Vec<Option<String>>>,
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
            perf: std::sync::Mutex::new((200, 15, 20, true)),
            pending_chat: Mutex::new(None),
            staged_download: Mutex::new(None),
            cancel: tokio::sync::Notify::new(),
            progress: std::sync::Mutex::new(None),
        }
    }

    pub fn set_progress_sink(&self, f: Box<dyn Fn(&str, &str) + Send + Sync>) {
        *self.progress.lock().unwrap() = Some(f);
    }
    fn phase(&self, step: &str, detail: &str) {
        if let Some(f) = self.progress.lock().unwrap().as_ref() {
            f(step, detail);
        }
    }

    /// Drop the live client deliberately (app resumed after a long pause: the socket is
    /// dead and a paused route would be a fixed address anyway). A waiter still holding
    /// the client — a chat that was in flight when the app went to sleep — is cancelled
    /// first; its pending request stays recorded for an idempotent Retry.
    pub async fn drop_client(&self) {
        self.cancel_in_flight();
        let mut guard = self.client.lock().await;
        if let Some(c) = guard.take() {
            c.disconnect().await;
        }
        self.mark_dead();
    }

    /// Cancel whatever wait is in flight (see `cancel`). No-op when nothing is waiting.
    pub fn cancel_in_flight(&self) {
        self.cancel.notify_waiters();
    }

    /// Park an unfinished picture download so a retry can resume it.
    pub async fn set_staged_download(&self, dl: StagedDownload) {
        *self.staged_download.lock().await = Some(dl);
    }
    /// Take the parked download for this session (if any) — the caller resumes it.
    pub async fn take_staged_download(&self, session_id: &str) -> Option<StagedDownload> {
        let mut g = self.staged_download.lock().await;
        if g.as_ref().map(|d| d.session_id == session_id).unwrap_or(false) {
            g.take()
        } else {
            None
        }
    }

    /// Acquire the whole-operation lock. Hold the returned guard for the entire
    /// command (chat/redeem/collect) so its round trips run as one atomic unit.
    pub async fn begin_op(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.op_lock.lock().await
    }

    /// Set the mixnet performance/privacy tradeoff and drop the live client so the next
    /// request reconnects with the new cover-traffic rate + mixing delay. No-op (and no
    /// reconnect) if the values are unchanged, so the UI can push it freely on startup.
    pub async fn set_perf(&self, cover_ms: u64, mix_ms: u64, send_ms: u64, continuous: bool) {
        {
            let mut p = self.perf.lock().unwrap();
            if *p == (cover_ms, mix_ms, send_ms, continuous) {
                return;
            }
            *p = (cover_ms, mix_ms, send_ms, continuous);
        }
        // Force a reconnect so the new DebugConfig takes effect.
        self.mark_dead();
        if let Some(c) = self.client.lock().await.take() {
            c.disconnect().await;
        }
    }

    /// Remember the just-built chat request so a retry can resend it verbatim.
    pub async fn set_pending_chat(&self, session_id: &str, req: Value) {
        *self.pending_chat.lock().await = Some((session_id.to_string(), req));
    }
    /// The pending (unacked) chat request for this session, if any — for an idempotent retry.
    pub async fn pending_chat(&self, session_id: &str) -> Option<Value> {
        self.pending_chat
            .lock()
            .await
            .as_ref()
            .filter(|(s, _)| s == session_id)
            .map(|(_, r)| r.clone())
    }
    /// Clear the pending chat once its reply has arrived (or it's superseded).
    pub async fn clear_pending_chat(&self, session_id: &str) {
        let mut g = self.pending_chat.lock().await;
        if g.as_ref().map(|(s, _)| s == session_id).unwrap_or(false) {
            *g = None;
        }
    }

    /// Pipelined multi-send: fire ALL `requests` concurrently through a split sender, then
    /// collect their acks by request `id`. The split sender enqueues into the SAME nym send
    /// stream that paces + cover-mixes every packet, so this is NOT a privacy change vs the
    /// sequential path — only the app-level per-chunk round-trip serialisation is removed.
    /// Used for uploads. `on_progress(received_bytes)` fires as each ack lands.
    pub async fn fire_and_collect<F: Fn(u64)>(
        &self,
        server: &str,
        requests: Vec<Value>,
        surbs: u32,
        timeout_ms: u64,
        on_progress: F,
    ) -> Result<(), String> {
        self.collect_replies(server, requests, surbs, timeout_ms, |v, _| {
            if let Some(recv) = v.get("received").and_then(|r| r.as_u64()) {
                on_progress(recv);
            }
        })
        .await
        .map(|_| ())
    }

    /// Pipelined multi-request whose replies CARRY data (chunked image download): fires
    /// every request, collects each reply by request `id`, re-fires stalled ones, and
    /// returns `id → reply`. `on_progress(reply, replies_so_far)` fires per reply.
    ///
    /// Boxed: nym's send/receive futures are enormous, and this future is moved by value
    /// into the Tauri command that awaits it — on iOS the IPC handler runs on the 1 MB
    /// main thread, where that move alone overflowed the stack (see `chat` in lib.rs).
    pub async fn collect_replies<F: Fn(&Value, usize)>(
        &self,
        server: &str,
        requests: Vec<Value>,
        surbs: u32,
        timeout_ms: u64,
        on_progress: F,
    ) -> Result<HashMap<String, Value>, String> {
        Box::pin(self.collect_replies_inner(server, requests, surbs, timeout_ms, on_progress)).await
    }

    async fn collect_replies_inner<F: Fn(&Value, usize)>(
        &self,
        server: &str,
        requests: Vec<Value>,
        surbs: u32,
        timeout_ms: u64,
        on_progress: F,
    ) -> Result<HashMap<String, Value>, String> {
        self.ensure_connected().await?;
        let recipient = Recipient::try_from_base58_string(server)
            .map_err(|e| format!("bad server address: {e}"))?;
        let mut guard = self.client.lock().await;
        let client = guard.as_mut().ok_or("mixnet not connected — please retry")?;
        let sender = client.split_sender();

        // Fire every chunk (concurrent at the nym send layer). Keep each request's bytes so
        // a chunk whose ack never arrives can be re-fired.
        let mut pending: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
        let mut replies: HashMap<String, Value> = HashMap::new();
        for req in &requests {
            let bytes = serde_json::to_vec(req).map_err(|e| e.to_string())?;
            if let Err(e) = sender.send_message(recipient, bytes.clone(), IncludedSurbs::new(surbs)).await {
                *guard = None;
                self.mark_dead();
                return Err(format!("mixnet send failed: {e} — reconnecting on the next attempt"));
            }
            if let Some(id) = req.get("id").and_then(|x| x.as_str()) {
                pending.insert(id.to_string(), bytes);
            }
        }

        // Collect acks; re-fire any chunk that stalls (belt-and-suspenders over nym's own
        // per-packet retransmission) so one lost chunk can't hang the whole upload.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let mut last_progress = tokio::time::Instant::now();
        let mut refires: u32 = 0;
        while !pending.is_empty() {
            let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now())
            else {
                *guard = None;
                self.mark_dead();
                return Err("upload timed out — reconnecting on the next attempt".into());
            };
            let poll = remaining.min(Duration::from_secs(5));
            let got = tokio::select! {
                r = tokio::time::timeout(poll, client.wait_for_messages()) => match r {
                    Ok(Some(b)) => Some(b),
                    Ok(None) => {
                        *guard = None;
                        self.mark_dead();
                        return Err("mixnet stream ended — reconnecting on the next attempt".into());
                    }
                    Err(_) => None, // poll slice elapsed with no message — check the stall below
                },
                // Cancelled from the UI: the chunks received so far are kept by the caller
                // (staged download), so Retry resumes instead of re-fetching everything.
                _ = self.cancel.notified() => return Err("cancelled — Retry resumes the download".into()),
            };
            if let Some(batch) = got {
                for m in batch {
                    let Ok(v) = serde_json::from_slice::<Value>(&m.message) else { continue };
                    let Some(id) = v.get("id").and_then(|x| x.as_str()) else { continue };
                    if pending.remove(id).is_none() {
                        continue; // not one of ours (stray/cover)
                    }
                    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
                        return Err(format!("request rejected: {err}"));
                    }
                    let id = id.to_string();
                    on_progress(&v, replies.len() + 1);
                    replies.insert(id, v);
                    last_progress = tokio::time::Instant::now();
                }
            }
            // Stalled with chunks still open → re-fire them (a few times, then let the overall timeout win).
            if !pending.is_empty() && last_progress.elapsed() >= Duration::from_secs(10) && refires < 4 {
                for bytes in pending.values() {
                    if let Err(e) = sender.send_message(recipient, bytes.clone(), IncludedSurbs::new(surbs)).await {
                        // The client is gone — fail now instead of re-firing into the void until the deadline.
                        *guard = None;
                        self.mark_dead();
                        return Err(format!("mixnet send failed: {e} — reconnecting on the next attempt"));
                    }
                }
                refires += 1;
                last_progress = tokio::time::Instant::now();
            }
        }
        Ok(replies)
    }

    /// The gateway identity embedded in a Nym recipient address (the `@…` part).
    /// This is how the app learns the scrai-server's (exit) gateway.
    pub fn gateway_of(addr: &str) -> Option<String> {
        Recipient::try_from_base58_string(addr)
            .ok()
            .map(|r| r.gateway().to_base58_string())
    }

    /// Validate a scrai-server Nym recipient address (base58 `id.enc@gateway`) before
    /// persisting or routing to it, so a malformed or malicious `set_server` value is
    /// rejected at the boundary instead of being silently stored and used (H5).
    pub fn validate_address(addr: &str) -> Result<(), String> {
        Recipient::try_from_base58_string(addr.trim())
            .map(|_| ())
            .map_err(|e| format!("invalid scrai-server address: {e}"))
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
        // Boxed — the SDK's connect future is huge; see `collect_replies`.
        Box::pin(self.ensure_connected_inner()).await
    }

    async fn ensure_connected_inner(&self) -> Result<(), String> {
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
            // new_ephemeral() + build(): fresh identity keys, then the client itself
            self.phase("keys", "");
            self.phase("client", "");
            let gw_label = match &gw {
                Some(id) => match self.gateway_info(id).await {
                    Some(info) => {
                        let host = if info.host.is_empty() { id.chars().take(8).collect::<String>() } else { info.host.clone() };
                        if info.country.is_empty() { host } else { format!("{} · {}", info.country, host) }
                    }
                    None => id.chars().take(12).collect(),
                },
                None => "gateway picked by the SDK".to_string(),
            };
            self.phase("gateway", &gw_label);
            // Apply the user's performance/privacy tradeoff to the client's cover-traffic
            // rate and per-hop mixing delay (default = Nym's max-privacy settings).
            let (cover_ms, mix_ms, send_ms, continuous) = *self.perf.lock().unwrap();
            let dbg = debug_config_for(cover_ms, mix_ms, send_ms, continuous);
            match gw {
                Some(gw) => MixnetClientBuilder::new_ephemeral()
                    .request_gateway(gw)
                    .debug_config(dbg)
                    .build()
                    .map_err(|e| format!("mixnet build failed: {e}"))?
                    .connect_to_mixnet()
                    .await
                    .map_err(|e| format!("mixnet connect failed: {e}")),
                // No described gateway available → let the SDK pick one, but KEEP the
                // user's traffic tuple (connect_new() would silently fall back to defaults).
                None => MixnetClientBuilder::new_ephemeral()
                    .debug_config(dbg)
                    .build()
                    .map_err(|e| format!("mixnet build failed: {e}"))?
                    .connect_to_mixnet()
                    .await
                    .map_err(|e| format!("mixnet connect failed: {e}")),
            }
        };
        let c = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                self.phase("failed", &e);
                return Err(e);
            }
            Err(_) => {
                let e = "mixnet connect timed out — gateway unreachable, please try again".to_string();
                self.phase("failed", &e);
                return Err(e);
            }
        };
        *self.entry_cached.lock().unwrap() = Some(c.nym_address().gateway().to_base58_string());
        self.live.store(true, std::sync::atomic::Ordering::Relaxed);
        *guard = Some(c);
        // The SDK's cover stream starts with the connection; report it as its own step,
        // matching the boot animation's wording.
        self.phase("cover", "");
        self.phase("ready", "");
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
        // Cold cache: this clearnet fetch really happens now — say so (a warm cache skips it).
        self.phase("directory", "");
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
        // Boxed — nym's send + wait_for_messages futures are huge; see `collect_replies`.
        Box::pin(self.round_trip_notify_inner(server, req, surbs, timeout_ms, on_sent, true)).await
    }

    /// Like `round_trip_notify`, but a DELIVERED server error (`kind: "error"`) comes
    /// back as `Ok(reply)` instead of `Err` — so the caller can tell "the server answered
    /// with an error" (request consumed, e.g. counter used, provider refused) apart from
    /// "no reply at all" (retry the same request verbatim). `Err` is transport-only.
    pub async fn round_trip_raw_notify(
        &self,
        server: &str,
        req: &Value,
        surbs: u32,
        timeout_ms: u64,
        on_sent: impl FnOnce(),
    ) -> Result<Value, String> {
        Box::pin(self.round_trip_notify_inner(server, req, surbs, timeout_ms, on_sent, false)).await
    }

    async fn round_trip_notify_inner(
        &self,
        server: &str,
        req: &Value,
        surbs: u32,
        timeout_ms: u64,
        on_sent: impl FnOnce(),
        errors_as_err: bool,
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
        const CANCELLED_MSG: &str =
            "cancelled — the request already left for the mixnet and may still be processed; Retry replays it without a second charge";
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now())
            else {
                *guard = None;
                self.mark_dead();
                return Err(TIMEOUT_MSG.into());
            };
            let batch = tokio::select! {
                r = tokio::time::timeout(remaining, guard.as_mut().unwrap().wait_for_messages()) => match r {
                    Ok(b) => b,
                    Err(_) => {
                        *guard = None;
                        self.mark_dead();
                        return Err(TIMEOUT_MSG.into());
                    }
                },
                // Cancelled from the UI: stop waiting, keep the client alive. The pending
                // request stays recorded so a later Retry replays it (no second charge).
                _ = self.cancel.notified() => return Err(CANCELLED_MSG.into()),
            };
            let Some(messages) = batch else {
                *guard = None;
                self.mark_dead();
                return Err("mixnet stream ended — reconnecting on the next attempt".into());
            };
            for m in messages {
                if let Ok(v) = serde_json::from_slice::<Value>(&m.message) {
                    if v.get("id").and_then(|x| x.as_str()) == Some(id.as_str()) {
                        if errors_as_err && v.get("kind").and_then(|k| k.as_str()) == Some("error") {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_config_maps_perf_knobs_to_nym_fields() {
        // performance end: fast send + low mixing, cover stream on
        let d = debug_config_for(3000, 2, 3, true);
        assert_eq!(d.traffic.average_packet_delay, Duration::from_millis(2));
        assert_eq!(d.traffic.message_sending_average_delay, Duration::from_millis(3));
        assert_eq!(
            d.cover_traffic.loop_cover_traffic_average_delay,
            Duration::from_millis(3000)
        );
        assert!(!d.cover_traffic.disable_loop_cover_traffic_stream);

        // privacy end == Nym defaults, cover stream off toggles the disable flag
        let p = debug_config_for(200, 15, 20, false);
        assert_eq!(p.traffic.average_packet_delay, Duration::from_millis(15));
        assert_eq!(p.traffic.message_sending_average_delay, Duration::from_millis(20));
        assert_eq!(
            p.cover_traffic.loop_cover_traffic_average_delay,
            Duration::from_millis(200)
        );
        assert!(p.cover_traffic.disable_loop_cover_traffic_stream);

        // zero is clamped to 1ms so a slider extreme can never produce a 0 delay
        let z = debug_config_for(0, 0, 0, true);
        assert_eq!(z.traffic.average_packet_delay, Duration::from_millis(1));
        assert_eq!(z.traffic.message_sending_average_delay, Duration::from_millis(1));
        assert_eq!(z.cover_traffic.loop_cover_traffic_average_delay, Duration::from_millis(1));
    }
}
