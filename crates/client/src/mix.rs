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

pub struct MixTransport {
    client: MixnetClient,
    to: Recipient,
    to_text: String,
    pub entry_gateway: String,
    /// Overall limit for one exchange.
    pub timeout: Duration,
}

impl MixTransport {
    /// Connect through an allowed entry gateway and aim at the enclave's address.
    pub async fn connect(enclave_address: &str, choice: &EntryChoice) -> Result<MixTransport, String> {
        let enclave_address = enclave_address.trim();
        let to = Recipient::try_from_base58_string(enclave_address).map_err(|e| format!("not a Nym address: {e}"))?;
        // A random allowed gateway; the directory also lists nodes the SDK will not use
        // as an entry right now ("no gateway with id"), so a few are tried in turn.
        let tries: Vec<String> = match choice {
            EntryChoice::Random => {
                use rand::seq::SliceRandom;
                let directory = gateways::fetch().await?;
                let mut c: Vec<String> = gateways::candidates(&directory, enclave_address).into_iter().map(|g| g.id).collect();
                c.shuffle(&mut rand::thread_rng());
                c.truncate(5);
                if c.is_empty() {
                    return Err("no suitable entry gateway in the Nym directory".into());
                }
                c
            }
            chosen => vec![gateways::pick(&[], chosen, enclave_address)?],
        };
        let mut last = String::new();
        let mut found = None;
        for entry in tries {
            let connect = async {
                MixnetClientBuilder::new_ephemeral()
                    .request_gateway(entry.clone())
                    .build()
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
