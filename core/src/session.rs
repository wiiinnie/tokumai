// ---------------------------------------------------------------------------
// session.rs — server-side session balances (TOKU), the bridge between redeemed
// coconut credit and metered chat usage.
//
// A client redeems coconut coins into a session (→ `credit`); each chat charges the
// metered cost (→ `charge`). Keyed by the client's session id (unlinkable to the
// account). In-memory for now — a persistent/replicated store is a later phase.
// ---------------------------------------------------------------------------

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Default, Clone, Copy, Serialize, Deserialize)]
struct Session {
    balance: u64,
    counter: u64,
}

#[derive(Default, Serialize, Deserialize)]
pub struct SessionStore {
    sessions: HashMap<String, Session>,
    /// Monotonic mutation counter — the server persists only when it changes, so the
    /// frequent read (`status`) and no-op charges never trigger a disk write.
    #[serde(default)]
    rev: u64,
}

impl SessionStore {
    /// (balance, counter) for a session — (0, 0) if it has never been funded.
    pub fn status(&self, id: &str) -> (u64, u64) {
        self.sessions.get(id).map(|s| (s.balance, s.counter)).unwrap_or((0, 0))
    }

    pub fn balance(&self, id: &str) -> u64 {
        self.status(id).0
    }

    /// Monotonic revision, bumped on every balance change (for change-detection).
    pub fn revision(&self) -> u64 {
        self.rev
    }

    /// Aggregate read-outs (for server metrics/admin). No per-session data leaves here.
    pub fn count(&self) -> usize {
        self.sessions.len()
    }
    /// Total unspent, redeemed TOKU sitting across all sessions.
    pub fn total_balance(&self) -> u64 {
        self.sessions.values().map(|s| s.balance).sum()
    }
    /// Total charges ever reserved across all live sessions (≈ chats billed).
    pub fn total_charges(&self) -> u64 {
        self.sessions.values().map(|s| s.counter).sum()
    }

    /// Restore from a JSON snapshot (server boot); empty/invalid → a fresh store.
    pub fn from_snapshot(json: &str) -> Self {
        serde_json::from_str(json).unwrap_or_default()
    }

    /// JSON snapshot for durable storage.
    pub fn snapshot(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }

    /// Add redeemed credit to a session; returns the new balance.
    pub fn credit(&mut self, id: &str, amount: u64) -> u64 {
        let s = self.sessions.entry(id.to_string()).or_default();
        s.balance = s.balance.saturating_add(amount);
        self.rev += 1;
        s.balance
    }

    /// Charge up to `cost` (never below zero); returns the new balance. Used for chat
    /// settlement — the answer was already produced, so we take what's there.
    /// Empty a session: return what was on it and leave it at zero. The session layer is
    /// being retired (docs/unlinkability.md, block D) and this is how a balance that was
    /// paid for reaches the account's entitlement instead of being written off. Draining
    /// an unknown or already-empty session moves 0, which makes a retry harmless.
    pub fn drain(&mut self, id: &str) -> u64 {
        let Some(s) = self.sessions.get_mut(id) else { return 0 };
        let moved = std::mem::take(&mut s.balance);
        if moved > 0 {
            self.rev += 1;
        }
        moved
    }

    pub fn charge_saturating(&mut self, id: &str, cost: u64) -> u64 {
        let s = self.sessions.entry(id.to_string()).or_default();
        s.balance = s.balance.saturating_sub(cost);
        self.rev += 1;
        s.balance
    }

    /// Advance the counter and reserve `amount` — atomically, or not at all
    /// (mirrors the TS store). Reserving the worst case up front is what keeps
    /// two in-flight requests from jointly overspending; the counter must be
    /// exactly `stored + 1`, which is also the replay protection the chat
    /// signature relies on.
    pub fn reserve(&mut self, id: &str, counter: u64, amount: u64) -> Reserve {
        let Some(s) = self.sessions.get_mut(id) else {
            return Reserve::Unknown;
        };
        if counter != s.counter + 1 {
            return Reserve::Replay { server_counter: s.counter };
        }
        if s.balance < amount {
            return Reserve::Insufficient { balance: s.balance };
        }
        s.balance -= amount;
        s.counter = counter;
        self.rev += 1;
        Reserve::Ok
    }

    /// Settle a reservation against the actual price: the unused part comes back.
    /// Returns the resulting balance.
    pub fn settle(&mut self, id: &str, reserved: u64, actual: u64) -> u64 {
        let refund = reserved.saturating_sub(actual);
        if refund > 0 {
            let s = self.sessions.entry(id.to_string()).or_default();
            s.balance = s.balance.saturating_add(refund);
            self.rev += 1;
        }
        self.balance(id)
    }

    /// Return a full reservation (provider failed → the user pays nothing).
    /// The counter stays consumed — it numbered a request that DID happen.
    pub fn refund(&mut self, id: &str, amount: u64) -> u64 {
        let s = self.sessions.entry(id.to_string()).or_default();
        s.balance = s.balance.saturating_add(amount);
        self.rev += 1;
        s.balance
    }
}

/// Outcome of `reserve` — each case maps to a distinct client-visible error.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reserve {
    Ok,
    Unknown,
    Replay { server_counter: u64 },
    Insufficient { balance: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_then_charge() {
        let mut s = SessionStore::default();
        assert_eq!(s.status("a"), (0, 0));
        assert_eq!(s.credit("a", 10_000), 10_000);
        assert_eq!(s.balance("a"), 10_000);
        assert_eq!(s.charge_saturating("a", 3_000), 7_000);
        // over-charge floors at zero, never negative
        assert_eq!(s.charge_saturating("a", 999_999), 0);
    }

    #[test]
    fn draining_empties_the_session_exactly_once() {
        let mut s = SessionStore::default();
        s.credit("a", 9_703);
        s.credit("b", 500);
        let rev = s.revision();
        assert_eq!(s.drain("a"), 9_703);
        assert_eq!(s.balance("a"), 0);
        assert!(s.revision() > rev, "a drained balance must reach disk");
        // A second attempt (a lost reply, a retry) moves nothing more, and no other
        // session is touched.
        assert_eq!(s.drain("a"), 0);
        assert_eq!(s.drain("never-funded"), 0);
        assert_eq!(s.balance("b"), 500);
    }

    #[test]
    fn sessions_are_independent() {
        let mut s = SessionStore::default();
        s.credit("a", 100);
        s.credit("b", 200);
        assert_eq!(s.balance("a"), 100);
        assert_eq!(s.balance("b"), 200);
    }

    #[test]
    fn reserve_settle_refund_lifecycle() {
        let mut s = SessionStore::default();
        s.credit("a", 10_000);

        // wrong counter (0 stored → must be 1)
        assert_eq!(s.reserve("a", 2, 100), Reserve::Replay { server_counter: 0 });
        // too expensive
        assert_eq!(s.reserve("a", 1, 99_999), Reserve::Insufficient { balance: 10_000 });
        // unknown session
        assert_eq!(s.reserve("nope", 1, 1), Reserve::Unknown);

        // reserve the ceiling, settle at the real (smaller) price
        assert_eq!(s.reserve("a", 1, 4_000), Reserve::Ok);
        assert_eq!(s.balance("a"), 6_000);
        assert_eq!(s.settle("a", 4_000, 1_500), 8_500);

        // replaying the used counter is refused
        assert_eq!(s.reserve("a", 1, 10), Reserve::Replay { server_counter: 1 });

        // provider failure → full refund, but the counter stays consumed
        assert_eq!(s.reserve("a", 2, 3_000), Reserve::Ok);
        assert_eq!(s.refund("a", 3_000), 8_500);
        assert_eq!(s.status("a").1, 2);
    }

    #[test]
    fn snapshot_survives_a_round_trip_and_revision_tracks_change() {
        let mut s = SessionStore::default();
        assert_eq!(s.revision(), 0);
        s.credit("a", 5_000);
        s.charge_saturating("a", 33);
        let rev = s.revision();
        assert_eq!(rev, 2);

        let restored = SessionStore::from_snapshot(&s.snapshot());
        assert_eq!(restored.balance("a"), 4_967);
        assert_eq!(restored.revision(), rev); // rev persists, so no needless re-write
    }
}
