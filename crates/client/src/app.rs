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
    /// The last transport this handed out led nowhere — the enclave never answered on it.
    /// A connector with several ways in takes that as "not this one" and offers another.
    fn led_nowhere(&self) {}
}

/// The mixnet, to the enclave's address, through an entry gateway allowed by rule A1.
/// What an interface is told while a connection is being made, as each step starts:
/// "directory", "gateway", "cover" (the mixnet client), then "proof" and "ready" (the
/// attestation). Real moments, so nothing has to be guessed with a timer.
pub type Steps = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// How many of the enclave's doors are tried before giving up. Three is what it has.
const DOORS_TRIED: usize = 3;

/// How the app reaches the enclave. An enclave has several front doors — one address per
/// gateway it listens at — and any of them leads to the same enclave, the same book, the
/// same proof. They exist so that a gateway going down does not take tokumai with it, and
/// so a person can choose to arrive somewhere other than Germany.
///
/// The door is not what protects the person: that is their own entry gateway, which is
/// never one of ours (rule A1, `gateways`). The door only decides where our side stands.
pub struct MixConnector {
    /// Every address the enclave answers at, in the order they are tried.
    pub doors: Vec<String>,
    /// The door a person picked, if any. It is tried first — and if it is down, the
    /// others are still tried: being reachable beats being where you asked for.
    pub chosen: std::sync::Mutex<Option<String>>,
    /// The door that last worked, tried before the rest.
    pub last_door: std::sync::Mutex<Option<String>>,
    /// Doors that did not answer on this run; tried last, and only if nothing else works.
    pub silent: std::sync::Mutex<Vec<String>>,
    pub entry: EntryChoice,
    pub traffic: Option<crate::mix::Traffic>,
    pub steps: Option<Steps>,
    /// Passed to every transport: how much of a long reply is in (`have`, `of`).
    pub progress: Option<std::sync::Arc<dyn Fn(usize, usize) + Send + Sync>>,
    /// The entry gateway of the last connect (for the route display).
    pub last_entry: std::sync::Mutex<Option<String>>,
}

impl MixConnector {
    /// One address, or several separated by commas — the enclave's doors.
    pub fn new(addresses: &str, entry: EntryChoice) -> MixConnector {
        let doors: Vec<String> = addresses.split(',').map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect();
        MixConnector { doors, chosen: Default::default(), last_door: Default::default(), silent: Default::default(), entry, traffic: None, steps: None, progress: None, last_entry: Default::default() }
    }

    /// The first door of the list — what an enclave with one door has always been.
    pub fn address(&self) -> String {
        self.doors.first().cloned().unwrap_or_default()
    }

    /// Ask for a particular door from now on (`None` = whichever answers).
    pub fn choose(&self, door: Option<String>) {
        if let Ok(mut c) = self.chosen.lock() {
            *c = door.filter(|d| self.doors.iter().any(|k| k == d));
        }
    }

    /// The doors to try, in order: the chosen one, then the one that last worked, then
    /// the rest as listed — and the ones that went silent on this run at the very end,
    /// because a gateway that came back should not be shut out for good.
    fn order(&self) -> Vec<String> {
        let silent = self.silent.lock().map(|s| s.clone()).unwrap_or_default();
        let mut order: Vec<String> = Vec::new();
        for first in [self.chosen.lock().ok().and_then(|c| c.clone()), self.last_door.lock().ok().and_then(|d| d.clone())].into_iter().flatten() {
            if self.doors.contains(&first) && !order.contains(&first) && !silent.contains(&first) {
                order.push(first);
            }
        }
        for door in &self.doors {
            if !order.contains(door) && !silent.contains(door) {
                order.push(door.clone());
            }
        }
        for door in silent {
            if self.doors.contains(&door) && !order.contains(&door) {
                order.push(door);
            }
        }
        order
    }
}

impl Connector for MixConnector {
    /// The door we came in through never answered: remember it, so the next attempt goes
    /// somewhere else. It stays in the list, last, for when the gateway comes back.
    fn led_nowhere(&self) {
        let Ok(mut last) = self.last_door.lock() else { return };
        if let (Some(door), Ok(mut silent)) = (last.take(), self.silent.lock()) {
            if !silent.contains(&door) {
                log::info!("[enclave] the door at {} went silent — trying another", crate::gateways::gateway_of(&door).unwrap_or("?"));
                silent.push(door);
            }
        }
    }

    fn connect(&self) -> BoxFuture<'_, Result<Box<dyn Transport>, String>> {
        Box::pin(async move {
            let say = |step: &str| {
                if let Some(s) = &self.steps {
                    s(step);
                }
            };
            // Every door in turn: one that does not open is a gateway that is down, not
            // an enclave that is gone.
            let mut last = String::new();
            for door in self.order() {
                match MixTransport::connect(&door, &self.entry, self.traffic, &say).await {
                    Ok(mut t) => {
                        t.on_progress = self.progress.clone();
                        if let Ok(mut e) = self.last_entry.lock() {
                            *e = Some(t.entry_gateway.clone());
                        }
                        if let Ok(mut d) = self.last_door.lock() {
                            *d = Some(door.clone());
                        }
                        if !last.is_empty() {
                            log::info!("[enclave] came in through another door after: {last}");
                        }
                        return Ok(Box::new(t) as Box<dyn Transport>);
                    }
                    Err(e) => {
                        log::info!("[enclave] the door at {} did not open: {e}", crate::gateways::gateway_of(&door).unwrap_or("?"));
                        last = e;
                    }
                }
            }
            Err(if last.is_empty() { "the enclave has no address to reach it at".into() } else { last })
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
    steps: Option<Steps>,
}

impl Connection {
    pub fn new(connector: Box<dyn Connector>, policy: Policy) -> Connection {
        Connection { connector, policy, transport: None, session: None, paused_at: None, steps: None }
    }

    /// Follow the steps of a connection as they happen (see [`Steps`]).
    pub fn on_step(&mut self, steps: Steps) {
        self.steps = Some(steps);
    }

    fn step(&self, step: &str) {
        if let Some(s) = &self.steps {
            s(step);
        }
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
    ///
    /// The enclave has more than one door, and a door that does not answer is a gateway
    /// that is down — not an enclave that is gone. So a silent one is set aside and the
    /// next is tried; the proof is what decides whether we arrived somewhere real.
    pub async fn ready(&mut self) -> Result<(), String> {
        let mut last = String::new();
        for _ in 0..DOORS_TRIED {
            if self.transport.is_none() {
                let t0 = Instant::now();
                self.transport = Some(self.connector.connect().await?);
                log::info!("[enclave] connected in {} ms", t0.elapsed().as_millis());
            }
            if self.session.is_some() {
                return Ok(());
            }
            let t0 = Instant::now();
            self.step("proof");
            let nonce: [u8; 32] = rand::random();
            let t = self.transport.as_mut().ok_or("no transport")?;
            match t.roundtrip(&attest_request(&nonce)).await {
                Ok(reply) => {
                    let reached = t.reached_at();
                    self.session = Some(Session::from_attestation(&reply, &nonce, &self.policy, reached.as_deref())?);
                    log::info!("[enclave] attested in {} ms", t0.elapsed().as_millis());
                    self.step("ready");
                    return Ok(());
                }
                Err(e) => {
                    // Nothing came back through this door. Another one, and if they all
                    // stay silent the error is the last one's.
                    self.transport = None;
                    self.connector.led_nowhere();
                    last = e;
                }
            }
        }
        Err(last)
    }

    /// One operation, signed by `account`.
    pub async fn call(&mut self, account: &Account, op: &str, body: &Value) -> Result<Value, String> {
        Box::pin(self.call_inner(account, op, body)).await
    }

    async fn call_inner(&mut self, account: &Account, op: &str, body: &Value) -> Result<Value, String> {
        let started = Instant::now();
        for attempt in 0..2 {
            self.ready().await?;
            let session = self.session.as_ref().ok_or("not attested")?;
            let (pending, bytes) = session.request(account, op, body, tokumai_proto::now_ms());
            let reply = self.send(&bytes).await?;
            match pending.open(&reply) {
                Err(e) if e.contains("attest again") => {
                    self.session = None;
                    continue;
                }
                // A request held up in the mixnet arrives stale, and the same signed bytes
                // never become fresh again. The enclave turned it away without carrying it
                // out, so signing it again costs nothing and is charged nothing.
                Err(e) if e.contains("clock is too far") && attempt == 0 => {
                    log::info!("[enclave] {op} arrived too late to be accepted — signing it again");
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

    /// Send, giving a lost reply a second chance BEFORE giving up on the connection.
    ///
    /// A roundtrip that does not come back is ordinary over a mixnet: a packet is dropped,
    /// a SURB expires, the enclave takes longer than the wait. Treating that as a broken
    /// transport — which this did — rebuilt the whole Nym client for it: seconds of
    /// directory, gateway and cover, a fresh registration at the gateway, and the old
    /// session left standing there. On every lost reply that becomes a reconnection
    /// spiral, and two sessions of one identity at a gateway are how a message comes to be
    /// delivered to the dead one. It is also why the app announced "Connecting to the
    /// mixnet" after every other tap (2026-09-24).
    ///
    /// Resending the same bytes is safe by design: the enclave remembers a nonce it has
    /// answered and hands the same answer back (`service.rs`, "a resend of a request
    /// already answered gets that answer again").
    async fn send(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        let mut last = "no connection".to_string();
        // Twice down the connection we have; only then is the connection itself the suspect.
        for attempt in 0..3 {
            if self.transport.is_none() {
                self.transport = Some(self.connector.connect().await?);
            }
            let t = self.transport.as_mut().ok_or("no transport")?;
            match t.roundtrip(bytes).await {
                Ok(reply) => return Ok(reply),
                Err(e) => {
                    last = e;
                    if attempt == 1 {
                        self.transport = None;
                    }
                }
            }
        }
        Err(last)
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

    /// One lost reply is weather, not a broken connection: the same bytes go down the same
    /// transport again, the enclave hands back the answer it already gave, and the Nym
    /// client is NOT rebuilt — which is what used to announce "Connecting to the mixnet"
    /// after every other tap, and left a spare session at the gateway each time.
    #[tokio::test]
    async fn a_lost_reply_does_not_throw_the_connection_away() {
        let to = Arc::new(Mutex::new(enclave()));
        let fail = Arc::new(AtomicUsize::new(0));
        let connects = Arc::new(AtomicUsize::new(0));
        let mut c = Connection::new(Box::new(LocalConnector { to: to.clone(), fail_next: fail.clone(), connects: connects.clone() }), policy());
        let a = tokumai_core::account::from_mnemonic(PHRASE).unwrap();
        c.call(&a, "dev.credit", &json!({ "toku": 100_000 })).await.unwrap();
        let before = connects.load(Ordering::SeqCst);
        fail.store(1, Ordering::SeqCst);
        let answer = c.call(&a, "chat", &json!({ "model": "mock", "messages": [{ "role": "user", "content": "hi" }], "maxTokens": 200 })).await.unwrap();
        let cost = answer["cost"].as_u64().unwrap_or_else(|| panic!("{answer}"));
        assert_eq!(connects.load(Ordering::SeqCst), before, "one lost reply must not rebuild the client");
        let b = c.call(&a, "balance", &json!({})).await.unwrap();
        assert_eq!(b["balance"]["total"].as_u64().unwrap(), 100_000 - cost, "the lost answer was not charged twice");
    }

    /// Twice is not weather. Then the connection itself is the suspect, a fresh one is
    /// built, and the question is still charged exactly once.
    #[tokio::test]
    async fn a_connection_that_keeps_failing_is_replaced_and_charged_once() {
        let to = Arc::new(Mutex::new(enclave()));
        let fail = Arc::new(AtomicUsize::new(0));
        let connects = Arc::new(AtomicUsize::new(0));
        let mut c = Connection::new(Box::new(LocalConnector { to: to.clone(), fail_next: fail.clone(), connects: connects.clone() }), policy());
        let a = tokumai_core::account::from_mnemonic(PHRASE).unwrap();
        c.call(&a, "dev.credit", &json!({ "toku": 100_000 })).await.unwrap();
        let before = connects.load(Ordering::SeqCst);
        fail.store(2, Ordering::SeqCst);
        let answer = c.call(&a, "chat", &json!({ "model": "mock", "messages": [{ "role": "user", "content": "hi" }], "maxTokens": 200 })).await.unwrap();
        let cost = answer["cost"].as_u64().unwrap_or_else(|| panic!("{answer}"));
        assert_eq!(connects.load(Ordering::SeqCst), before + 1, "reconnected once, after the second failure");
        let b = c.call(&a, "balance", &json!({})).await.unwrap();
        assert_eq!(b["balance"]["total"].as_u64().unwrap(), 100_000 - cost, "the lost answers were not charged twice");
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
