//! A connection to the enclave, as the app holds it.
//!
//! [`Connection::call`] is the one way the app talks to the enclave. It connects when there
//! is no transport, attests when there is no session, and recovers from the two things that
//! happen in real life:
//!
//! - the transport died (the phone slept, the network changed, the gateway dropped us):
//!   reconnect and send the SAME bytes again — the enclave answers a resend once, and
//!   charges it once;
//! - the enclave restarted (new keys, "attest again"): attest the new one and sign the
//!   request afresh. The old request never reached a ledger that could still charge it —
//!   a restart gives every open hold back.

use crate::gateways::EntryChoice;
use crate::mix::MixTransport;
use crate::Transport;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};
use tokumai_attest::Policy;
use tokumai_core::account::Account;
use tokumai_proto::session::{attest_request, Session};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Makes a transport to the enclave.
pub trait Connector: Send + Sync {
    fn connect(&self) -> BoxFuture<'_, Result<Box<dyn Transport>, String>>;
}

/// The mixnet, to the enclave's address, through an entry gateway allowed by rule A1.
pub struct MixConnector {
    pub enclave_address: String,
    pub entry: EntryChoice,
    pub traffic: Option<crate::mix::Traffic>,
    /// The entry gateway of the last connect (for the route display).
    pub last_entry: std::sync::Mutex<Option<String>>,
}

impl MixConnector {
    pub fn new(enclave_address: &str, entry: EntryChoice) -> MixConnector {
        MixConnector { enclave_address: enclave_address.trim().to_string(), entry, traffic: None, last_entry: Default::default() }
    }
}

impl Connector for MixConnector {
    fn connect(&self) -> BoxFuture<'_, Result<Box<dyn Transport>, String>> {
        Box::pin(async move {
            let t = MixTransport::connect(&self.enclave_address, &self.entry, self.traffic).await?;
            if let Ok(mut e) = self.last_entry.lock() {
                *e = Some(t.entry_gateway.clone());
            }
            Ok(Box::new(t) as Box<dyn Transport>)
        })
    }
}

/// After a pause this long (the app in the background, the laptop asleep), the mixnet
/// client is not trusted to be alive: its gateway socket usually is not, and the first
/// request would hang until its timeout.
const STALE_AFTER: Duration = Duration::from_secs(60);

pub struct Connection {
    connector: Box<dyn Connector>,
    policy: Policy,
    transport: Option<Box<dyn Transport>>,
    session: Option<Session>,
    paused_at: Option<Instant>,
}

impl Connection {
    pub fn new(connector: Box<dyn Connector>, policy: Policy) -> Connection {
        Connection { connector, policy, transport: None, session: None, paused_at: None }
    }

    /// The attested enclave, once there is one.
    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// The app went to the background, or the machine to sleep.
    pub fn paused(&mut self) {
        self.paused_at = Some(Instant::now());
    }

    /// The app is back. After a long pause the transport is dropped, so the next call
    /// connects afresh instead of waiting on a dead socket.
    pub fn resumed(&mut self) {
        if self.paused_at.take().is_some_and(|at| at.elapsed() >= STALE_AFTER) {
            self.transport = None;
        }
    }

    /// Replace the mixnet client before the next call (the person asked for a fresh route).
    pub fn drop_transport(&mut self) {
        self.transport = None;
    }

    pub fn has_transport(&self) -> bool {
        self.transport.is_some()
    }

    /// Connect and attest now (the app does this at start, so the first question is quick).
    pub async fn ready(&mut self) -> Result<(), String> {
        if self.transport.is_none() {
            let t0 = Instant::now();
            self.transport = Some(self.connector.connect().await?);
            log::info!("[enclave] connected in {} ms", t0.elapsed().as_millis());
        }
        if self.session.is_none() {
            let t0 = Instant::now();
            let nonce: [u8; 32] = rand::random();
            let t = self.transport.as_mut().ok_or("no transport")?;
            let reply = match t.roundtrip(&attest_request(&nonce)).await {
                Ok(r) => r,
                Err(e) => {
                    self.transport = None;
                    return Err(e);
                }
            };
            let reached = t.reached_at();
            self.session = Some(Session::from_attestation(&reply, &nonce, &self.policy, reached.as_deref())?);
            log::info!("[enclave] attested in {} ms", t0.elapsed().as_millis());
        }
        Ok(())
    }

    /// One operation, signed by `account`.
    pub async fn call(&mut self, account: &Account, op: &str, body: &Value) -> Result<Value, String> {
        Box::pin(self.call_inner(account, op, body)).await
    }

    async fn call_inner(&mut self, account: &Account, op: &str, body: &Value) -> Result<Value, String> {
        let started = Instant::now();
        for _ in 0..2 {
            self.ready().await?;
            let session = self.session.as_ref().ok_or("not attested")?;
            let (pending, bytes) = session.request(account, op, body, tokumai_proto::now_ms());
            let reply = self.send(&bytes).await?;
            match pending.open(&reply) {
                Err(e) if e.contains("attest again") => {
                    self.session = None;
                    continue;
                }
                answer => {
                    log::info!("[enclave] {op} took {} ms", started.elapsed().as_millis());
                    return answer;
                }
            }
        }
        Err("the enclave keeps changing — try again in a moment".into())
    }

    /// Send, and if the transport fails, reconnect once and send the same bytes again.
    async fn send(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        for attempt in 0..2 {
            if self.transport.is_none() {
                self.transport = Some(self.connector.connect().await?);
            }
            let t = self.transport.as_mut().ok_or("no transport")?;
            match t.roundtrip(bytes).await {
                Ok(reply) => return Ok(reply),
                Err(e) => {
                    self.transport = None;
                    if attempt == 1 {
                        return Err(e);
                    }
                }
            }
        }
        Err("no connection".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokumai_attest::sim;
    use tokumai_core::pricing::PricingTable;
    use tokumai_enclave::policy::PRICING_JSON;
    use tokumai_enclave::provider::Providers;
    use tokumai_enclave::seal::FixedKeyProvider;
    use tokumai_enclave::service::{Db, Enclave, Platform};

    const ROOT: [u8; 32] = [5; 32];
    const PHRASE: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art";

    fn enclave() -> Arc<Enclave> {
        Arc::new(
            Enclave::start(Platform {
                attester: Box::new(sim::SimAttester::new(ROOT, "image-1")),
                keys: Box::new(FixedKeyProvider([9; 32])),
                providers: Providers::mock(),
                db: Db::Memory,
                pricing: PricingTable::parse(PRICING_JSON).unwrap(),
                dev_mode: true,
                stripe: None,
                apple_api: None,
            })
            .unwrap(),
        )
    }

    /// The enclave in this process, reached through a transport the test can break.
    struct Local {
        to: Arc<Mutex<Arc<Enclave>>>,
        fail_next: Arc<AtomicUsize>,
    }
    impl Transport for Local {
        fn roundtrip<'a>(&'a mut self, m: &'a [u8]) -> BoxFuture<'a, Result<Vec<u8>, String>> {
            Box::pin(async move {
                let e = self.to.lock().unwrap().clone();
                let reply = e.handle(m).await;
                // The enclave answered, and the answer was lost on the way back.
                if self.fail_next.load(Ordering::SeqCst) > 0 {
                    self.fail_next.fetch_sub(1, Ordering::SeqCst);
                    return Err("the socket is dead".into());
                }
                Ok(reply)
            })
        }
        fn reached_at(&self) -> Option<String> {
            None
        }
    }
    struct LocalConnector {
        to: Arc<Mutex<Arc<Enclave>>>,
        fail_next: Arc<AtomicUsize>,
        connects: Arc<AtomicUsize>,
    }
    impl Connector for LocalConnector {
        fn connect(&self) -> BoxFuture<'_, Result<Box<dyn Transport>, String>> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(Box::new(Local { to: self.to.clone(), fail_next: self.fail_next.clone() }) as Box<dyn Transport>) })
        }
    }

    fn policy() -> Policy {
        Policy { measurements: vec!["image-1".into()], simulated_root: Some(sim::root_public(&ROOT)), simulated_any_measurement: false }
    }

    #[tokio::test]
    async fn a_dead_transport_is_replaced_and_the_question_is_charged_once() {
        let to = Arc::new(Mutex::new(enclave()));
        let fail = Arc::new(AtomicUsize::new(0));
        let connects = Arc::new(AtomicUsize::new(0));
        let mut c = Connection::new(Box::new(LocalConnector { to: to.clone(), fail_next: fail.clone(), connects: connects.clone() }), policy());
        let a = tokumai_core::account::from_mnemonic(PHRASE).unwrap();
        c.call(&a, "dev.credit", &json!({ "toku": 100_000 })).await.unwrap();
        fail.store(1, Ordering::SeqCst);
        let answer = c.call(&a, "chat", &json!({ "model": "mock", "messages": [{ "role": "user", "content": "hi" }], "maxTokens": 200 })).await.unwrap();
        let cost = answer["cost"].as_u64().unwrap_or_else(|| panic!("{answer}"));
        assert_eq!(connects.load(Ordering::SeqCst), 2, "reconnected once");
        let b = c.call(&a, "balance", &json!({})).await.unwrap();
        assert_eq!(b["balance"]["total"].as_u64().unwrap(), 100_000 - cost, "the lost answer was not charged twice");
    }

    #[tokio::test]
    async fn a_restarted_enclave_is_attested_again_without_the_app_noticing() {
        let to = Arc::new(Mutex::new(enclave()));
        let mut c = Connection::new(
            Box::new(LocalConnector { to: to.clone(), fail_next: Arc::new(AtomicUsize::new(0)), connects: Arc::new(AtomicUsize::new(0)) }),
            policy(),
        );
        let a = tokumai_core::account::from_mnemonic(PHRASE).unwrap();
        c.call(&a, "balance", &json!({})).await.unwrap();
        let first = c.session().unwrap().claims.clone();
        *to.lock().unwrap() = enclave(); // new keys, same image
        let b = c.call(&a, "balance", &json!({})).await.unwrap();
        assert_eq!(b["kind"], "balance");
        assert_eq!(c.session().unwrap().claims, first);
    }

    #[tokio::test]
    async fn an_enclave_this_app_does_not_trust_gets_nothing() {
        let to = Arc::new(Mutex::new(enclave()));
        let release = Policy { measurements: vec!["image-1".into()], ..Default::default() };
        let mut c = Connection::new(
            Box::new(LocalConnector { to, fail_next: Arc::new(AtomicUsize::new(0)), connects: Arc::new(AtomicUsize::new(0)) }),
            release,
        );
        let a = tokumai_core::account::from_mnemonic(PHRASE).unwrap();
        assert!(c.call(&a, "balance", &json!({})).await.is_err());
        assert!(c.session().is_none());
    }
}
