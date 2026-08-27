// ---------------------------------------------------------------------------
// ledger.rs — the "guest book" seam: the seed-recoverable, cross-server value layer.
//
// Only the INFREQUENT operations live behind this trait — crediting redeemed coconut
// value into a session (`redeem`→`credit`) and reads. They are the ones a future
// implementation reaches over the MIXNET (a server→ledger-service round-trip), so the
// trait is `async` + fallible precisely so that impl drops in without changing a single
// call site.
//
// DELIBERATELY OFF THIS TRAIT: the per-message chat billing (`reserve`/`settle`/`refund`).
// That is the hot path — it must NEVER pay a mixnet round-trip — so it stays on the local
// in-process `SessionStore` via its own sync methods. Merging the local billing cache with
// a shared ledger (so a live balance is visible across servers) is the LEASE model, parked
// as a perf-tuning nice-to-have. See docs/federation-shared-ledger.md §6b.
//
// Implementations:
//   - `SessionStore` itself = the LOCAL, in-process ledger (today's single server; also the
//     ledger-service's own backend). Ops are instant, never fail.
//   - `MixnetLedger` (later) = a thin client stub forwarding these ops to a ledger-service
//     over the mixnet. Same call sites, no refactor.
// ---------------------------------------------------------------------------

use async_trait::async_trait;

use crate::session::SessionStore;

/// The shared value layer's infrequent operations (see module docs). Async + fallible so a
/// mixnet-backed implementation slots in without a signature change.
#[async_trait]
pub trait Ledger: Send {
    /// `(balance, counter)` for a session — `(0, 0)` if it was never funded.
    async fn session_status(&self, id: &str) -> Result<(u64, u64), String>;
    /// Current balance for a session (0 if unknown).
    async fn session_balance(&self, id: &str) -> Result<u64, String>;
    /// Credit redeemed coconut value into a session; returns the resulting balance.
    async fn session_credit(&mut self, id: &str, amount: u64) -> Result<u64, String>;
}

/// The local, in-process implementation: the `SessionStore` is itself the ledger for a
/// single server (and for the ledger-service's own backend). Every op is instant and
/// infallible — the `Result` exists only for the mixnet impl's sake.
#[async_trait]
impl Ledger for SessionStore {
    async fn session_status(&self, id: &str) -> Result<(u64, u64), String> {
        Ok(self.status(id))
    }
    async fn session_balance(&self, id: &str) -> Result<u64, String> {
        Ok(self.balance(id))
    }
    async fn session_credit(&mut self, id: &str, amount: u64) -> Result<u64, String> {
        Ok(self.credit(id, amount))
    }
}
