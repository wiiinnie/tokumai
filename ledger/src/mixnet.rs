// ---------------------------------------------------------------------------
// mixnet.rs — MixnetLedger: a chat server's client stub for a REMOTE ledger-service.
//
// It implements the same `scrai_core::ledger::Ledger` trait as the local store, so it
// drops into the redeem path with no call-site change. Each op is signed with this
// server's member key and round-tripped to the ledger-service's Nym address over the
// mixnet, then the reply is parsed back.
//
// The actual mixnet is behind the `LedgerTransport` trait: the server provides the real
// Nym round-trip; tests provide a mock. So ALL of this stub's logic (request building,
// signing, reply parsing, error mapping) is exercised here without a live network — only
// the concrete Nym transport impl needs real infra.
//
// NOTE ON COHERENCE: routing `redeem`→`credit` here (infrequent, latency-tolerant) while
// per-message billing stays on the LOCAL SessionStore is only fully coherent across
// MULTIPLE servers once the LEASE reconciles them (docs/federation-shared-ledger.md §6b).
// For a single co-located deployment, use `LedgerStore` directly instead of this.
// ---------------------------------------------------------------------------

use async_trait::async_trait;
use ed25519_dalek::SigningKey;
use scrai_core::ledger::Ledger;

use crate::proto::{self, Op, Reply};

/// The one thing MixnetLedger needs from the outside world: send request bytes to the
/// ledger-service address and return the reply bytes. The server implements this with its
/// Nym client; tests implement it with a canned responder.
#[async_trait]
pub trait LedgerTransport: Send + Sync {
    async fn round_trip(&self, request: &[u8]) -> Result<Vec<u8>, String>;
}

/// Produces per-request nonces. Real deployments use a CSPRNG; tests can use a counter so
/// results are deterministic (scripts/workflows can't call rand freely).
pub trait NonceSource: Send + Sync {
    fn next(&self) -> String;
}

/// Default CSPRNG-backed nonce source.
pub struct RandNonce;
impl NonceSource for RandNonce {
    fn next(&self) -> String {
        use rand::RngCore;
        let mut b = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut b);
        hex::encode(b)
    }
}

pub struct MixnetLedger<T: LedgerTransport, N: NonceSource = RandNonce> {
    transport: T,
    member: SigningKey,
    nonces: N,
}

impl<T: LedgerTransport> MixnetLedger<T, RandNonce> {
    pub fn new(transport: T, member: SigningKey) -> Self {
        MixnetLedger { transport, member, nonces: RandNonce }
    }
}

impl<T: LedgerTransport, N: NonceSource> MixnetLedger<T, N> {
    pub fn with_nonce_source(transport: T, member: SigningKey, nonces: N) -> Self {
        MixnetLedger { transport, member, nonces }
    }

    /// Sign the op, round-trip it, and return the parsed reply (or an error).
    async fn call(&self, op: Op) -> Result<Reply, String> {
        let req = proto::sign(&self.member, op, &self.nonces.next());
        let bytes = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
        let reply_bytes = self.transport.round_trip(&bytes).await?;
        let reply: Reply = serde_json::from_slice(&reply_bytes).map_err(|e| e.to_string())?;
        if !reply.ok {
            return Err(reply.error.unwrap_or_else(|| "ledger rejected the request".into()));
        }
        Ok(reply)
    }
}

#[async_trait]
impl<T: LedgerTransport, N: NonceSource> Ledger for MixnetLedger<T, N> {
    async fn session_status(&self, id: &str) -> Result<(u64, u64), String> {
        let r = self.call(Op::Status { session_id: id.to_string() }).await?;
        Ok((r.balance.unwrap_or(0), r.counter.unwrap_or(0)))
    }
    async fn session_balance(&self, id: &str) -> Result<u64, String> {
        let r = self.call(Op::Balance { session_id: id.to_string() }).await?;
        Ok(r.balance.unwrap_or(0))
    }
    async fn session_credit(&mut self, id: &str, amount: u64) -> Result<u64, String> {
        let r = self.call(Op::Credit { session_id: id.to_string(), amount }).await?;
        r.balance.ok_or_else(|| "ledger returned no balance".into())
    }
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::Service;
    use crate::store::LedgerStore;
    use ed25519_dalek::SigningKey;
    use futures::executor::block_on;
    use futures::lock::Mutex;
    use rand::rngs::OsRng;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A transport that pipes the request straight into a real `Service` and returns its
    /// reply — so the client + protocol + service + store all interoperate end-to-end,
    /// minus only the physical mixnet hop. An async `Mutex` lets us `.await` the service
    /// without a nested executor.
    struct LoopbackTransport {
        svc: Mutex<Service>,
    }
    #[async_trait]
    impl LedgerTransport for LoopbackTransport {
        async fn round_trip(&self, request: &[u8]) -> Result<Vec<u8>, String> {
            let mut svc = self.svc.lock().await;
            Ok(svc.handle(request).await)
        }
    }

    /// Deterministic nonce source for the test (unique per call, no rand needed).
    struct SeqNonce(AtomicU64);
    impl NonceSource for SeqNonce {
        fn next(&self) -> String {
            format!("nonce-{}", self.0.fetch_add(1, Ordering::Relaxed))
        }
    }

    #[test]
    fn client_credits_and_reads_through_the_service() {
        let member = SigningKey::generate(&mut OsRng);
        let svc = Service::new(LedgerStore::in_memory().unwrap(), vec![member.verifying_key()]);
        let transport = LoopbackTransport { svc: Mutex::new(svc) };
        let mut client =
            MixnetLedger::with_nonce_source(transport, member, SeqNonce(AtomicU64::new(0)));

        assert_eq!(block_on(client.session_balance("s1")).unwrap(), 0);
        assert_eq!(block_on(client.session_credit("s1", 100_000)).unwrap(), 100_000);
        assert_eq!(block_on(client.session_credit("s1", 50_000)).unwrap(), 150_000);
        assert_eq!(block_on(client.session_status("s1")).unwrap(), (150_000, 0));
    }

    #[test]
    fn client_of_a_non_member_key_is_refused() {
        let real_member = SigningKey::generate(&mut OsRng);
        let impostor = SigningKey::generate(&mut OsRng);
        // service trusts only real_member
        let svc = Service::new(LedgerStore::in_memory().unwrap(), vec![real_member.verifying_key()]);
        let transport = LoopbackTransport { svc: Mutex::new(svc) };
        let mut client =
            MixnetLedger::with_nonce_source(transport, impostor, SeqNonce(AtomicU64::new(0)));

        let err = block_on(client.session_credit("s1", 100_000)).unwrap_err();
        assert!(err.contains("unauthorized"), "got: {err}");
    }
}
