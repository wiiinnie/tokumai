//! The app's Nym client. Ephemeral — fresh keys every start, so nothing links one session of
//! the app to the next — with the SDK's cover traffic left on (it hides when the user
//! sends), and an entry gateway chosen by rule A1 (`gateways`).

use crate::gateways::{self, EntryChoice};
use crate::Transport;
use nym_sdk::mixnet::{IncludedSurbs, MixnetClient, MixnetClientBuilder, MixnetMessageSender, Recipient};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tokumai_proto::frames::{Exchange, Step};

/// How long the app waits in silence before it sends what is still missing again. A
/// question being answered stays silent until the model is done; the resend is then
/// answered with nothing, and costs a few SURBs.
const QUIET: Duration = Duration::from_secs(20);
/// Frames in flight at once.
const WINDOW: usize = 8;
/// Reply SURBs sent with a frame whose answer can be a whole chunk, and with one whose
/// answer is an acknowledgement.
const SURBS_CHUNK: u32 = 40;
const SURBS_ACK: u32 = 3;
/// Generous: a connect includes the topology fetch from the Nym API, which was seen taking
/// minutes when that API is degraded. Healthy connects take seconds.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(180);

/// The person's trade-off between speed and anonymity, as the settings page sets it. `None`
/// keeps Nym's own defaults (200 ms cover, 15 ms mixing, 20 ms sending, cover always on),
/// which are the most private.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Traffic {
    /// Average gap between cover packets (battery).
    pub cover_ms: u64,
    /// Average hold at every mix hop (latency; this reshuffling is the anonymity).
    pub mix_ms: u64,
    /// Average gap between real packets (throughput).
    pub send_ms: u64,
    /// Keep the cover stream running while idle.
    pub continuous: bool,
}

impl Traffic {
    fn debug_config(&self) -> nym_sdk::DebugConfig {
        let mut d = nym_sdk::DebugConfig::default();
        d.traffic.average_packet_delay = Duration::from_millis(self.mix_ms.max(1));
        d.traffic.message_sending_average_delay = Duration::from_millis(self.send_ms.max(1));
        d.cover_traffic.loop_cover_traffic_average_delay = Duration::from_millis(self.cover_ms.max(1));
        d.cover_traffic.disable_loop_cover_traffic_stream = !self.continuous;
        d
    }
}

pub struct MixTransport {
    client: MixnetClient,
    to: Recipient,
    to_text: String,
    pub entry_gateway: String,
    /// Overall limit for one exchange.
    pub timeout: Duration,
}

impl MixTransport {
    /// Connect through an allowed entry gateway and aim at the enclave's address. `step` is
    /// told what is happening as it happens ("directory", "gateway", "cover"), so an
    /// interface can follow the real progress instead of a timer.
    pub async fn connect(enclave_address: &str, choice: &EntryChoice, traffic: Option<Traffic>, step: &(dyn Fn(&str) + Send + Sync)) -> Result<MixTransport, String> {
        let enclave_address = enclave_address.trim();
        let to = Recipient::try_from_base58_string(enclave_address).map_err(|e| format!("not a Nym address: {e}"))?;
        // The directory and the operator's family, also for a gateway the user chose (it
        // must not be one of ours either).
        step("directory");
        let directory = gateways::fetch().await?;
        // A random allowed gateway; the directory also lists nodes the SDK will not use
        // as an entry right now ("no gateway with id"), so a few are tried in turn.
        let tries: Vec<String> = match choice {
            EntryChoice::Random => {
                use rand::seq::SliceRandom;
                let mut c: Vec<String> = gateways::candidates(&directory, enclave_address).into_iter().map(|g| g.id).collect();
                c.shuffle(&mut rand::thread_rng());
                c.truncate(5);
                if c.is_empty() {
                    return Err("no suitable entry gateway in the Nym directory".into());
                }
                c
            }
            chosen => vec![gateways::pick(&directory, chosen, enclave_address)?],
        };
        let mut last = String::new();
        let mut found = None;
        step("gateway");
        for entry in tries {
            let connect = async {
                let mut b = MixnetClientBuilder::new_ephemeral().request_gateway(entry.clone());
                if let Some(t) = traffic {
                    b = b.debug_config(t.debug_config());
                }
                b.build()
                    .map_err(|e| format!("mixnet: {e}"))?
                    .connect_to_mixnet()
                    .await
                    .map_err(|e| format!("mixnet: {e}"))
            };
            match tokio::time::timeout(CONNECT_TIMEOUT, Box::pin(connect)).await {
                Ok(Ok(c)) => {
                    found = Some((c, entry));
                    break;
                }
                Ok(Err(e)) => last = e,
                Err(_) => last = "the entry gateway did not answer".into(),
            }
        }
        let Some((client, entry)) = found else { return Err(format!("{last} — try again")) };
        step("cover");
        Ok(MixTransport { client, to, to_text: enclave_address.to_string(), entry_gateway: entry, timeout: Duration::from_secs(300) })
    }

    pub fn own_address(&self) -> String {
        self.client.nym_address().to_string()
    }

    pub async fn disconnect(self) {
        self.client.disconnect().await;
    }

    async fn exchange(&mut self, message: &[u8]) -> Result<Vec<u8>, String> {
        let mut ex = Exchange::new(message.to_vec());
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            for f in ex.due(WINDOW) {
                let surbs = if f[0] == 2 { SURBS_ACK } else { SURBS_CHUNK };
                self.client.send_message(self.to, f, IncludedSurbs::new(surbs)).await.map_err(|e| format!("send: {e}"))?;
            }
            // Listen until the answers stop coming for a while, then send what is missing.
            loop {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    return Err("no answer from the enclave in time".into());
                }
                let wait = QUIET.min(deadline - now);
                let Ok(batch) = tokio::time::timeout(wait, self.client.wait_for_messages()).await else { break };
                let Some(batch) = batch else { return Err("the mixnet client stopped".into()) };
                let mut moved = false;
                for m in batch {
                    if !ex.owns(&m.message) {
                        continue; // a late answer to an earlier exchange
                    }
                    match ex.accept(&m.message)? {
                        Step::Done(reply) => return Ok(reply),
                        Step::Going => moved = true,
                    }
                }
                // Progress (an acknowledgement, a chunk): send the next frames right away.
                if moved {
                    break;
                }
            }
        }
    }
}

impl Transport for MixTransport {
    fn roundtrip<'a>(&'a mut self, message: &'a [u8]) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'a>> {
        Box::pin(self.exchange(message))
    }
    fn reached_at(&self) -> Option<String> {
        Some(self.to_text.clone())
    }
}
