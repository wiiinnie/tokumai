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

/// The canonical serial keys of a payment (what the spent-serial index is keyed by).
pub fn payment_serials(p: &Payment) -> Vec<String> {
    p.ss.iter().map(serial_key).collect()
}

/// The serials inside a persisted record row — for indexing rows written before the
/// spent-serial table existed (one-time migration on the server).
pub fn record_serials(record_json: &str) -> Result<Vec<String>, String> {
    let rec: Record = serde_json::from_str(record_json).map_err(|e| format!("quorum record: {e}"))?;
    Ok(payment_serials(&rec.payment))
}

/// A recorded spend. `pay_info` is the raw 72 bytes (PayInfo isn't serde). The payment
/// is kept because `detect` needs BOTH payments to PROVE a later double-spend — but it is
/// kept on DISK (see `SerialIndex`), loaded only when a serial matches.
#[derive(Clone, Serialize, Deserialize)]
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

/// The cold side of the store: every persisted spend, by serial and by record index. The
/// server backs it with SQLite (`spent_serials` + `quorum_records`), tests with a map.
/// Nothing here is kept in RAM by the store itself — that is the point (2026-09-13: the
/// in-memory map of every serial ever seen, plus every payment, grew ~50 KB per dollar of
/// revenue and was never pruned).
pub trait SerialIndex: Send {
    /// The record index of the first persisted spend of this serial, if any.
    fn lookup(&self, serial_hex: &str) -> Option<u64>;
    /// The persisted record (JSON of `Record`) by index.
    fn record(&self, idx: u64) -> Option<String>;
}

/// In-memory index for tests and for a server that runs without a database. Fed by
/// `PendingRecord`s exactly as the SQLite one is.
#[derive(Default)]
pub struct MemIndex {
    serials: HashMap<String, u64>,
    records: HashMap<u64, String>,
}

impl MemIndex {
    pub fn insert(&mut self, rec: &PendingRecord) {
        for s in &rec.serials {
            self.serials.entry(s.clone()).or_insert(rec.idx);
        }
        self.records.insert(rec.idx, rec.json.clone());
    }
    pub fn len(&self) -> usize {
        self.records.len()
    }
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

impl SerialIndex for MemIndex {
    fn lookup(&self, serial_hex: &str) -> Option<u64> {
        self.serials.get(serial_hex).copied()
    }
    fn record(&self, idx: u64) -> Option<String> {
        self.records.get(&idx).cloned()
    }
}

/// A fresh spend the server has accepted but not yet written: its row index, coin count,
/// serial keys (for the index) and the record JSON (for the proof).
#[derive(Clone, Debug)]
pub struct PendingRecord {
    pub idx: u64,
    pub coins: usize,
    pub serials: Vec<String>,
    pub json: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Offense {
    pubkey: PublicKeyUser,
    coins: HashSet<String>,   // distinct reused coin serials
    servers: HashSet<ServerId>,
}

/// Double-spend store + blacklist. The HOT part (policy, offenses, blacklist, counters)
/// lives here and is tiny; spends are held only until the server persists them
/// (`pending_records` → `clear_pending`), then looked up through the `SerialIndex`.
pub struct QuorumStore {
    policy: Policy,
    index: Box<dyn SerialIndex>,
    /// Row index the next fresh record gets — continues where the persisted rows end.
    next_idx: u64,
    /// Accepted since the last persist: (idx, record) + serial → idx.
    pending: Vec<(u64, Record)>,
    pending_serials: HashMap<String, u64>,
    offenses: HashMap<String, Offense>, // offender base58 → offense
    blacklist: HashSet<String>,         // offender base58
    /// Lifetime coins recorded as spent — survives pruning of the rows (admin "burned").
    burned: u64,
    /// Monotonic mutation counter (change-detection for persistence). Bumped only when
    /// durable state changes — a fresh record or a new/updated offense — never on a
    /// replay or a benign no-op.
    rev: u64,
    /// Bumped only when the SMALL part changes (offenses / blacklist / burned).
    meta_rev: u64,
}

/// The store minus its per-payment records: policy, offenses, blacklist, revisions.
/// Small, re-written whole; the records are rows.
#[derive(Serialize, Deserialize)]
pub struct QuorumMeta {
    policy: Policy,
    offenses: HashMap<String, Offense>,
    blacklist: HashSet<String>,
    rev: u64,
    meta_rev: u64,
    #[serde(default)]
    burned: u64,
}

impl Default for QuorumStore {
    fn default() -> Self {
        Self::new(Policy::default())
    }
}

impl QuorumStore {
    /// A store over an in-memory index (tests; a server without a database).
    pub fn new(policy: Policy) -> Self {
        Self::with_index(policy, Box::new(MemIndex::default()), 0)
    }

    pub fn with_index(policy: Policy, index: Box<dyn SerialIndex>, next_idx: u64) -> Self {
        Self {
            policy,
            index,
            next_idx,
            pending: Vec::new(),
            pending_serials: HashMap::new(),
            offenses: HashMap::new(),
            blacklist: HashSet::new(),
            burned: 0,
            rev: 0,
            meta_rev: 0,
        }
    }

    /// Restore the small part from its blob and attach the cold index; `next_idx` is one
    /// past the highest persisted row.
    pub fn from_meta(meta: &str, index: Box<dyn SerialIndex>, next_idx: u64) -> Result<Self, String> {
        let m: QuorumMeta = serde_json::from_str(meta).map_err(|e| format!("quorum meta: {e}"))?;
        Ok(Self {
            policy: m.policy,
            index,
            next_idx,
            pending: Vec::new(),
            pending_serials: HashMap::new(),
            offenses: m.offenses,
            blacklist: m.blacklist,
            burned: m.burned,
            rev: m.rev,
            meta_rev: m.meta_rev,
        })
    }

    /// Monotonic revision, bumped whenever durable state changes.
    pub fn revision(&self) -> u64 {
        self.rev
    }
    /// Revision of the small part (offenses/blacklist/burned) — see `QuorumMeta`.
    pub fn meta_revision(&self) -> u64 {
        self.meta_rev
    }
    /// Lifetime coins recorded as spent.
    pub fn burned(&self) -> u64 {
        self.burned
    }
    /// One-time seed for a store whose meta predates the counter: the server passes the
    /// coins it can still count in its rows (plus what earlier prunes removed). No-op once
    /// the counter is non-zero.
    pub fn seed_burned(&mut self, coins: u64) {
        if self.burned == 0 && coins > 0 {
            self.burned = coins;
            self.meta_rev += 1;
        }
    }
    /// The small part as JSON.
    pub fn meta_json(&self) -> String {
        let m = QuorumMeta {
            policy: self.policy.clone(),
            offenses: self.offenses.clone(),
            blacklist: self.blacklist.clone(),
            rev: self.rev,
            meta_rev: self.meta_rev,
            burned: self.burned,
        };
        serde_json::to_string(&m).unwrap_or_else(|_| "{}".into())
    }

    /// Accepted spends not yet on disk, oldest first. The server writes them in the same
    /// transaction as the session credit they back, then calls `clear_pending`.
    pub fn pending_records(&self) -> Vec<PendingRecord> {
        self.pending
            .iter()
            .map(|(idx, r)| PendingRecord {
                idx: *idx,
                coins: r.payment.ss.len(),
                serials: payment_serials(&r.payment),
                json: serde_json::to_string(r).unwrap_or_else(|_| "{}".into()),
            })
            .collect()
    }
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
    /// The pending spends are persisted (and visible through the index from now on).
    pub fn clear_pending(&mut self) {
        self.pending.clear();
        self.pending_serials.clear();
    }

    /// The earlier spend that used `key`, from RAM (pending) or from the index.
    fn previous_spend(&self, key: &str) -> Option<Record> {
        if let Some(idx) = self.pending_serials.get(key) {
            return self.pending.iter().find(|(i, _)| i == idx).map(|(_, r)| r.clone());
        }
        let idx = self.index.lookup(key)?;
        let json = self.index.record(idx)?;
        serde_json::from_str(&json).ok()
    }

    /// Report a spend a server accepted offline. Detects reuse, records offenses, and
    /// applies the blacklist policy. Returns the store's verdict.
    pub fn submit(&mut self, payment: &Payment, pay_info: PayInfo, server: ServerId) -> Verdict {
        let keys: Vec<String> = payment_serials(payment);
        if keys.is_empty() {
            return Verdict::Accepted;
        }
        // Look for any coin whose serial was already spent.
        let mut reused_serials: Vec<String> = Vec::new();
        let mut offender: Option<PublicKeyUser> = None;
        let mut prev_servers: HashSet<ServerId> = HashSet::new();
        let mut all_seen = true;
        for key in &keys {
            match self.previous_spend(key) {
                None => all_seen = false,
                Some(prev) => match detect(&prev.payment, payment, prev.pay_info(), pay_info) {
                    DoubleSpend::Replay => { /* same pay_info — benign, already recorded */ }
                    DoubleSpend::Detected(pk) => {
                        offender = Some(pk);
                        reused_serials.push(key.clone());
                        prev_servers.insert(prev.server);
                    }
                    DoubleSpend::None => { /* serial matched but no proof — treat as fresh */ }
                },
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
            self.meta_rev += 1;
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
        let idx = self.next_idx;
        self.next_idx += 1;
        for key in &keys {
            self.pending_serials.entry(key.clone()).or_insert(idx);
        }
        self.pending.push((
            idx,
            Record {
                payment: payment.clone(),
                pay_info: pay_info.pay_info_bytes.to_vec(),
                server,
            },
        ));
        self.burned += keys.len() as u64;
        self.rev += 1; // fresh record — durable state changed
        self.meta_rev += 1; // `burned` moved
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

        // persist → restart, modelling the server: pending rows go to the index, the small
        // part is reloaded from its blob, and the in-RAM pending set is gone.
        let mut index = MemIndex::default();
        for rec in store.pending_records() {
            index.insert(&rec);
        }
        let meta = store.meta_json();
        let mut store = QuorumStore::from_meta(&meta, Box::new(index), 1).unwrap();

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

    /// The append-only layout (meta + record rows) must rebuild the same store as a
    /// whole snapshot: the serial index is derived from the rows, so a coin recorded
    /// before the restart is still PROVEN doubled after it, and a replay stays benign.
    #[test]
    fn parts_round_trip_preserves_double_spend_detection() {
        let fk = testkit::funded();
        let mut w = fk.wallet();
        let mut w2 = fk.copy(&w);
        let mut store = QuorumStore::default();
        let (p1, pi1) = fk.spend_one(&mut w, 6);
        assert_eq!(store.submit(&p1, pi1, 1), Verdict::Accepted);
        let rows = store.pending_records();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].coins, 1); // one coin in that payment
        assert_eq!(store.meta_revision(), 1); // `burned` moved

        let meta = store.meta_json();
        let mut index = MemIndex::default();
        for r in &rows {
            index.insert(r);
        }
        let mut store = QuorumStore::from_meta(&meta, Box::new(index), 1).unwrap();
        assert!(!store.has_pending());

        // same payment again → replay, not a fresh record
        assert_eq!(store.submit(&p1, pi1, 1), Verdict::Replay);
        assert!(!store.has_pending());
        // the same coin from the copy → proven double-spend, and the meta revision moves
        let (p2, pi2) = fk.spend_one(&mut w2, 7);
        match store.submit(&p2, pi2, 1) {
            Verdict::DoubleSpend { rejected_coins, offender, .. } => {
                assert_eq!(rejected_coins, 1);
                assert!(offender == fk.user_pubkey());
            }
            other => panic!("expected DoubleSpend after parts restore, got {other:?}"),
        }
        assert_eq!(store.meta_revision(), 2);
        // and the offense survives another round trip through the blob
        let mut index = MemIndex::default();
        for r in &rows {
            index.insert(r);
        }
        let again = QuorumStore::from_meta(&store.meta_json(), Box::new(index), 1).unwrap();
        assert_eq!(again.offense_coins(&fk.user_pubkey()), 1);
        assert_eq!(again.burned(), 1);
    }

    #[test]
    fn nothing_stays_in_ram_after_persist_and_lookups_go_to_the_index() {
        let fk = testkit::funded();
        let mut w = fk.wallet();
        let mut store = QuorumStore::default();
        let (p0, pi0) = fk.spend_one(&mut w, 6);
        assert_eq!(store.submit(&p0, pi0, 1), Verdict::Accepted);
        assert!(store.has_pending());
        assert_eq!(store.burned(), 1);
        let recs = store.pending_records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].idx, 0);
        assert_eq!(recs[0].coins, 1);
        assert_eq!(recs[0].serials.len(), 1);
        assert_eq!(record_serials(&recs[0].json).unwrap(), recs[0].serials);
        // hand the rows to the cold side, as the server does after a successful batch write
        let mut index = MemIndex::default();
        for r in &recs {
            index.insert(r);
        }
        let mut store = QuorumStore::from_meta(&store.meta_json(), Box::new(index), 1).unwrap();
        assert!(!store.has_pending());
        // a replay of the persisted payment is still recognised — via the index
        assert_eq!(store.submit(&p0, pi0, 1), Verdict::Replay);
        // and a fresh payment continues the row numbering after the persisted ones
        let (p1, pi1) = fk.spend_one(&mut w, 7);
        assert_eq!(store.submit(&p1, pi1, 1), Verdict::Accepted);
        assert_eq!(store.pending_records()[0].idx, 1);
    }
}

