#![allow(dead_code)] // wired into the server + reconciliation transport in later phases
// ---------------------------------------------------------------------------
// quorum.rs — double-spend detection store + graduated blacklist (Phase 3).
//
// Servers accept spends OFFLINE (`coconut::verify`), then report each payment here.
// The store keys coins by their CANONICAL per-coin serial (compressed G1), spots a
// reused serial across payments, PROVES it via `coconut::detect` (identify) — which
// reveals the offender's public key — and applies the graduated policy from
// docs/federation-params.md: always refuse the doubled coin; blacklist the offender
// only past a volume threshold, or immediately on a cross-server reuse; never void
// remaining credit. Bans are gated on cryptographic PROOF, never a probabilistic hit.
//
// A fresh payment is UNLINKABLE — we learn a spender's key ONLY when they double-
// spend. So the blacklist is enforced at WITHDRAWAL (`is_blacklisted`), not per spend.
//
// This is the in-memory core + policy, unit-tested here. The distributed transport
// (quorum replication / reconciliation between servers) wraps this in a later phase;
// its topology is a majority quorum — see docs/federation-params.md.
// ---------------------------------------------------------------------------

use std::collections::{HashMap, HashSet};

use nym_bls12_381_fork::{G1Affine, G1Projective};
use nym_compact_ecash::scheme::keygen::PublicKeyUser;
use nym_compact_ecash::scheme::{PayInfo, Payment};
use serde::{Deserialize, Serialize};

use crate::coconut::{detect, DoubleSpend};

/// Which scrai-server observed a spend (used to spot cross-server reuse).
pub type ServerId = u16;

/// Tunable policy — defaults from docs/federation-params.md.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Policy {
    /// Distinct proven double-spent coins by one key before blacklisting.
    pub blacklist_threshold_coins: u32,
    /// A reuse seen across ≥2 different servers blacklists immediately.
    pub cross_server_fast_ban: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            blacklist_threshold_coins: 5,
            cross_server_fast_ban: true,
        }
    }
}

/// What the store concluded about a submitted payment.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// All coins fresh — recorded.
    Accepted,
    /// Byte-identical resubmission (same serials + same pay_info) — idempotent, safe.
    Replay,
    /// At least one coin reused. `rejected_coins` are refused; `banned` = the offender
    /// just crossed the threshold (or cross-server) and is now blacklisted.
    DoubleSpend {
        rejected_coins: u32,
        offender: PublicKeyUser,
        banned: bool,
    },
}

/// Canonical 96-hex key for a coin serial (compressed G1 point).
fn serial_key(g: &G1Projective) -> String {
    hex::encode(G1Affine::from(g).to_compressed())
}

/// A recorded spend. `pay_info` is the raw 72 bytes (PayInfo isn't serde) so the whole
/// store snapshots to disk — the payment is kept because `detect` needs BOTH payments
/// to PROVE a later double-spend, even across a server restart.
#[derive(Serialize, Deserialize)]
struct Record {
    payment: Payment,
    pay_info: Vec<u8>,
    server: ServerId,
}

impl Record {
    fn pay_info(&self) -> PayInfo {
        let bytes: [u8; 72] = self.pay_info.as_slice().try_into().unwrap_or([0u8; 72]);
        PayInfo { pay_info_bytes: bytes }
    }
}

#[derive(Serialize, Deserialize)]
struct Offense {
    pubkey: PublicKeyUser,
    coins: HashSet<String>,   // distinct reused coin serials
    servers: HashSet<ServerId>,
}

/// Double-spend store + blacklist. One per quorum (replicated in a later phase). Held
/// in memory; the server snapshots it to SQLite whenever `revision()` advances.
#[derive(Serialize, Deserialize)]
pub struct QuorumStore {
    policy: Policy,
    /// First spender of each coin serial → index into `records`.
    serials: HashMap<String, usize>,
    records: Vec<Record>,
    offenses: HashMap<String, Offense>, // offender base58 → offense
    blacklist: HashSet<String>,         // offender base58
    /// Monotonic mutation counter (change-detection for persistence). Bumped only when
    /// durable state changes — a fresh record or a new/updated offense — never on a
    /// replay or a benign no-op.
    #[serde(default)]
    rev: u64,
}

impl Default for QuorumStore {
    fn default() -> Self {
        Self::new(Policy::default())
    }
}

impl QuorumStore {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            serials: HashMap::new(),
            records: Vec::new(),
            offenses: HashMap::new(),
            blacklist: HashSet::new(),
            rev: 0,
        }
    }

    /// Monotonic revision, bumped whenever durable state changes.
    pub fn revision(&self) -> u64 {
        self.rev
    }

    /// Restore from a JSON snapshot (server boot); empty/invalid → a fresh store.
    pub fn from_snapshot(json: &str) -> Self {
        serde_json::from_str(json).unwrap_or_default()
    }

    /// JSON snapshot for durable storage.
    pub fn snapshot(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }

    /// Report a spend a server accepted offline. Detects reuse, records offenses, and
    /// applies the blacklist policy. Returns the store's verdict.
    pub fn submit(&mut self, payment: &Payment, pay_info: PayInfo, server: ServerId) -> Verdict {
        let keys: Vec<String> = payment.ss.iter().map(serial_key).collect();
        if keys.is_empty() {
            return Verdict::Accepted;
        }

        // Look for any coin whose serial was already spent.
        let mut reused_serials: Vec<String> = Vec::new();
        let mut offender: Option<PublicKeyUser> = None;
        let mut prev_servers: HashSet<ServerId> = HashSet::new();
        let mut all_seen = true;

        for key in &keys {
            match self.serials.get(key) {
                None => all_seen = false,
                Some(&idx) => {
                    let prev = &self.records[idx];
                    match detect(&prev.payment, payment, prev.pay_info(), pay_info) {
                        DoubleSpend::Replay => { /* same pay_info — benign, already recorded */ }
                        DoubleSpend::Detected(pk) => {
                            offender = Some(pk);
                            reused_serials.push(key.clone());
                            prev_servers.insert(prev.server);
                        }
                        DoubleSpend::None => { /* serial matched but no proof — treat as fresh */ }
                    }
                }
            }
        }

        // Genuine double-spend: refuse the reused coins, record the offense, maybe ban.
        if let Some(pk) = offender {
            let bkey = pk.to_base58_string();
            let entry = self.offenses.entry(bkey.clone()).or_insert_with(|| Offense {
                pubkey: pk,
                coins: HashSet::new(),
                servers: HashSet::new(),
            });
            for s in &reused_serials {
                entry.coins.insert(s.clone());
            }
            entry.servers.insert(server);
            entry.servers.extend(prev_servers);

            let banned_now = entry.coins.len() as u32 >= self.policy.blacklist_threshold_coins
                || (self.policy.cross_server_fast_ban && entry.servers.len() >= 2);
            if banned_now {
                self.blacklist.insert(bkey);
            }
            self.rev += 1; // offense recorded (and maybe a ban) — durable state changed
            return Verdict::DoubleSpend {
                rejected_coins: reused_serials.len() as u32,
                offender: pk,
                banned: banned_now,
            };
        }

        // No reuse. If every coin was already seen (with the same pay_info), it's a replay.
        if all_seen {
            return Verdict::Replay;
        }

        // Fresh payment — record it as the first spender of each of its new serials.
        let idx = self.records.len();
        self.records.push(Record {
            payment: payment.clone(),
            pay_info: pay_info.pay_info_bytes.to_vec(),
            server,
        });
        for key in keys {
            self.serials.entry(key).or_insert(idx);
        }
        self.rev += 1; // fresh record — durable state changed
        Verdict::Accepted
    }

    /// Enforced at WITHDRAWAL: has this key been blacklisted for double-spending?
    pub fn is_blacklisted(&self, pubkey: &PublicKeyUser) -> bool {
        self.blacklist.contains(&pubkey.to_base58_string())
    }

    /// Distinct proven double-spent coins recorded for a key (for diagnostics/tests).
    pub fn offense_coins(&self, pubkey: &PublicKeyUser) -> u32 {
        self.offenses
            .get(&pubkey.to_base58_string())
            .map(|o| o.coins.len() as u32)
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::coconut::testkit;

    #[test]
    fn fresh_payments_accepted_replay_is_idempotent() {
        let fk = testkit::funded();
        let mut w = fk.wallet();
        let mut store = QuorumStore::default();

        let (p0, pi0) = fk.spend_one(&mut w, 6); // coin 0
        let (p1, pi1) = fk.spend_one(&mut w, 7); // coin 1
        assert_eq!(store.submit(&p0, pi0, 1), Verdict::Accepted);
        assert_eq!(store.submit(&p1, pi1, 1), Verdict::Accepted);
        // resubmit the exact same payment → idempotent replay, never a ban
        assert_eq!(store.submit(&p0, pi0, 1), Verdict::Replay);
    }

    #[test]
    fn double_spend_refused_and_banned_at_threshold() {
        let fk = testkit::funded();
        let mut w = fk.wallet();
        let mut w2 = fk.copy(&w); // same coins, second "device"
        let mut store = QuorumStore::default(); // threshold 5

        // spend coins 0..5 from w (accepted), keep the payments
        let mut firsts = Vec::new();
        for i in 0..5u8 {
            let (p, pi) = fk.spend_one(&mut w, 100 + i);
            assert_eq!(store.submit(&p, pi, 1), Verdict::Accepted);
            firsts.push((p, pi));
        }
        // re-spend the SAME coins from the copy with different pay_info → double-spends
        for i in 0..5u8 {
            let (p, pi) = fk.spend_one(&mut w2, 200 + i);
            let v = store.submit(&p, pi, 1);
            match v {
                Verdict::DoubleSpend { rejected_coins, offender, banned } => {
                    assert_eq!(rejected_coins, 1);
                    assert!(offender == fk.user_pubkey());
                    // banned only once the 5th distinct coin is proven
                    assert_eq!(banned, i == 4, "coin #{i}: banned={banned}");
                }
                other => panic!("coin #{i}: expected DoubleSpend, got {other:?}"),
            }
        }
        assert!(store.is_blacklisted(&fk.user_pubkey()));
        assert_eq!(store.offense_coins(&fk.user_pubkey()), 5);
    }

    #[test]
    fn snapshot_restore_preserves_double_spend_detection() {
        let fk = testkit::funded();
        let mut w = fk.wallet();
        let mut w2 = fk.copy(&w); // same coins, second "device"
        let mut store = QuorumStore::default();

        let (p1, pi1) = fk.spend_one(&mut w, 6); // coin 0
        assert_eq!(store.submit(&p1, pi1, 1), Verdict::Accepted);
        assert!(store.revision() > 0);

        // snapshot → restore, modelling a server restart
        let mut store = QuorumStore::from_snapshot(&store.snapshot());

        // re-spend the SAME coin from the copy → the doubled coin is still PROVEN,
        // because the original payment was persisted and reloaded.
        let (p2, pi2) = fk.spend_one(&mut w2, 7);
        match store.submit(&p2, pi2, 1) {
            Verdict::DoubleSpend { rejected_coins, offender, .. } => {
                assert_eq!(rejected_coins, 1);
                assert!(offender == fk.user_pubkey());
            }
            other => panic!("expected DoubleSpend after restore, got {other:?}"),
        }
    }

    #[test]
    fn cross_server_double_spend_bans_immediately() {
        let fk = testkit::funded();
        let mut w = fk.wallet();
        let mut w2 = fk.copy(&w);
        let mut store = QuorumStore::default();

        let (p1, pi1) = fk.spend_one(&mut w, 6); // coin 0 at server 1
        assert_eq!(store.submit(&p1, pi1, 1), Verdict::Accepted);

        let (p2, pi2) = fk.spend_one(&mut w2, 7); // same coin 0 at server 2
        match store.submit(&p2, pi2, 2) {
            Verdict::DoubleSpend { banned, .. } => assert!(banned, "cross-server must ban at once"),
            other => panic!("expected DoubleSpend, got {other:?}"),
        }
        assert!(store.is_blacklisted(&fk.user_pubkey()));
    }
}
