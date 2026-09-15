// ---------------------------------------------------------------------------
// purse.rs — the CLIENT's coconut credential holder.
//
// A withdrawn credential is BEARER money: the wallet + the material needed to spend
// it (aggregated key, epoch signatures, the user keypair). The purse spends coins
// incrementally, advancing an on-device counter. That counter is the whole reason a
// double-spend is a bug not a feature: the caller MUST persist the purse (which now
// holds the advanced counter) BEFORE the payment leaves the device, so a crash can
// never roll the counter back and re-spend a coin. See docs/federation-params.md.
// ---------------------------------------------------------------------------

use serde::{Deserialize, Serialize};

use nym_compact_ecash::scheme::coin_indices_signatures::CoinIndexSignature;
use nym_compact_ecash::scheme::expiration_date_signatures::ExpirationDateSignature;
use nym_compact_ecash::scheme::keygen::{KeyPairUser, VerificationKeyAuth};
use nym_compact_ecash::scheme::{PayInfo, Payment, Wallet};
use nym_compact_ecash::setup::Parameters;

use crate::coconut;

/// A held credential plus everything needed to spend it offline. All fields are
/// serde-native, so the purse persists directly — treat the stored JSON as money
/// (until redeemed onto a session it is NOT rebuildable from the account phrase).
/// Everything an issuing epoch publishes: the aggregated verification key and the
/// signatures every client needs in order to spend. It is the SAME for every book of a
/// server and epoch, which is why it does not live inside a book.
///
/// It used to. Each purse carried its own copy, so the wallet — decrypted and rewritten on
/// every message — grew with the NUMBER of books rather than with the money in them
/// (measured 2026-09-14: 5.9 KB per one-cent book, and a $10 purchase came to megabytes).
/// Held once beside the books, a book is 641 bytes.
#[derive(Clone, Serialize, Deserialize)]
pub struct EpochKeys {
    pub vk: VerificationKeyAuth,
    pub coin_sigs: Vec<CoinIndexSignature>,
    pub date_sigs: Vec<ExpirationDateSignature>,
    pub expiration_date: u32,
    /// Coins per ticketbook this epoch issues.
    pub total_coins: u64,
    /// TOKU one coin of this epoch is worth. The value lives in the KEY, not in the coin
    /// (see `coconut::COARSE_TOKU`), so a server runs one of these per denomination and
    /// material from one never spends a book from the other.
    #[serde(default = "default_denom")]
    pub denom_toku: u64,
}

/// A book written before denominations existed is a fine one.
pub fn fine_denom() -> u64 {
    coconut::COIN_TOKU
}

fn default_denom() -> u64 {
    fine_denom()
}

impl EpochKeys {
    /// Do these keys belong to the epoch this book was issued in? Spending a book with
    /// another epoch's material only produces a payment no server will accept, so it is
    /// refused here rather than burned.
    pub fn fits(&self, purse: &Purse) -> bool {
        self.expiration_date == purse.expiration_date()
            && self.total_coins == purse.total_coins()
            && self.denom_toku == purse.denom_toku()
    }

    /// What a whole book of this epoch is worth.
    pub fn book_toku(&self) -> u64 {
        self.total_coins.saturating_mul(self.denom_toku)
    }
}

#[derive(Serialize, Deserialize)]
pub struct Purse {
    wallet: Wallet,
    user: KeyPairUser,
    total_coins: u64,
    expiration_date: u32,
    #[serde(default = "default_denom")]
    denom_toku: u64,
}

impl Purse {
    pub fn new(
        wallet: Wallet,
        user: KeyPairUser,
        total_coins: u64,
        expiration_date: u32,
        denom_toku: u64,
    ) -> Self {
        Self { wallet, user, total_coins, expiration_date, denom_toku }
    }

    /// TOKU one coin of this book is worth.
    pub fn denom_toku(&self) -> u64 {
        self.denom_toku
    }

    /// TOKU still in this book.
    pub fn remaining_toku(&self) -> u64 {
        self.remaining_coins().saturating_mul(self.denom_toku)
    }

    /// Spend `coins` from the purse, advancing its counter and returning a payment
    /// any server verifies offline.
    ///
    /// DURABILITY CONTRACT: the counter has now advanced inside `self`. The caller
    /// MUST `persist()` the purse and durably store it BEFORE sending the payment.
    /// Retry an uncertain send with the SAME `pay_info` (a benign replay), never a
    /// fresh spend — that is how honest clients avoid a self-inflicted double-spend.
    pub fn spend(
        &mut self,
        keys: &EpochKeys,
        coins: u64,
        pay_info: &PayInfo,
        spend_date: u32,
    ) -> Result<Payment, String> {
        if !keys.fits(self) {
            return Err("these keys are from a different issuing epoch than this ticketbook".into());
        }
        let params = Parameters::new(self.total_coins);
        coconut::spend(
            &mut self.wallet,
            &params,
            &keys.vk,
            self.user.secret_key(),
            pay_info,
            coins,
            &keys.date_sigs,
            &keys.coin_sigs,
            spend_date,
        )
    }

    /// Spend a tender plan: one payment per face value, each with its own random
    /// `pay_info`. Same durability contract as `spend` — the counter has advanced inside
    /// `self`, so persist BEFORE anything leaves the device, and re-send an uncertain
    /// tender verbatim rather than building a fresh one.
    ///
    /// All-or-nothing: if any payment fails to build, the purse is left untouched (the
    /// caller drops this copy), so a half-advanced counter never reaches disk.
    pub fn spend_tender(
        &mut self,
        keys: &EpochKeys,
        values: &[u64],
        spend_date: u32,
    ) -> Result<Vec<crate::tender::Note>, String> {
        use rand::RngCore;
        let mut probe = Purse::restore(&self.persist()?)?;
        let mut notes = Vec::with_capacity(values.len());
        for coins in values {
            let mut bytes = [0u8; 72];
            rand::thread_rng().fill_bytes(&mut bytes);
            let pi = PayInfo { pay_info_bytes: bytes };
            let payment = probe.spend(keys, *coins, &pi, spend_date)?;
            notes.push(crate::tender::Note {
                coins: *coins,
                payment,
                pay_info: bytes.to_vec(),
                spend_date,
                denom_toku: self.denom_toku,
            });
        }
        *self = probe;
        Ok(notes)
    }

    /// Serialise the purse (incl. the advanced counter) for durable on-device storage.
    pub fn persist(&self) -> Result<String, String> {
        serde_json::to_string(self).map_err(|e| e.to_string())
    }

    /// Restore a purse from its persisted JSON.
    pub fn restore(json: &str) -> Result<Purse, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
    }

    pub fn expiration_date(&self) -> u32 {
        self.expiration_date
    }
    pub fn total_coins(&self) -> u64 {
        self.total_coins
    }
    /// Coins still available to spend = total minus the wallet's advanced counter
    /// (`tickets_spent`, the trailing 8 big-endian bytes of the wallet serialisation).
    ///
    /// M-crypto-2 — FAIL CLOSED on any ambiguity. This counter is what stops an honest
    /// client from re-spending already-spent coins; a misread that *over*-reports the
    /// remaining balance makes the client re-spend, which the quorum proves as a
    /// double-spend and bans the (innocent) user for. So every uncertain path here
    /// returns 0 ("treat as fully spent") — an under-report only makes the purse look
    /// empty and quietly unused, never triggers a spend. The two guards:
    ///   • a serialisation shorter than 8 bytes (would otherwise panic on the slice);
    ///   • a decoded counter that EXCEEDS `total_coins` — impossible for an honest
    ///     wallet, so a strong signal the trailing-8-bytes assumption no longer holds
    ///     (e.g. an upstream `nym-compact-ecash` layout change or a mangled wallet).
    /// The residual gap the audit notes — a layout change that still leaves 8 trailing
    /// bytes decoding to a plausible *small* number — can't be detected from here; the
    /// real fix is an explicit upstream counter API (tracked in the security roadmap).
    pub fn remaining_coins(&self) -> u64 {
        let bytes = self.wallet.to_bytes();
        let n = bytes.len();
        if n < 8 {
            return 0;
        }
        let spent = match <[u8; 8]>::try_from(&bytes[n - 8..]) {
            Ok(arr) => u64::from_be_bytes(arr),
            Err(_) => return 0,
        };
        if spent > self.total_coins {
            return 0;
        }
        self.total_coins - spent
    }
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::coconut::testkit;
    use crate::quorum::{QuorumStore, Verdict};

    /// A tender: several payments out of one purse, exact subset sums, and only the
    /// selected notes are burned — the rest stay good and are tendered again later.
    #[test]
    fn a_tender_pays_the_exact_cost_and_the_unburned_notes_stay_spendable() {
        use crate::tender::{plan_coins, Tender};
        let fk = testkit::funded();
        let sd = fk.spend_date();
        let mut purse = fk.new_purse();
        let keys = fk.keys();
        let mut store = QuorumStore::default();

        // ceiling 7 coins → notes 1,2,4; the answer turns out to cost 3
        let values = plan_coins(7);
        assert_eq!(values, vec![1, 2, 4]);
        let notes = purse.spend_tender(&keys, &values, sd).unwrap();
        let tender = Tender { notes };
        tender.well_formed().unwrap();
        assert_eq!(tender.total_coins(), 7);
        assert_eq!(tender.total_toku(), 7 * coconut::COIN_TOKU);
        assert_eq!(purse.remaining_coins(), fk_total(&purse) - 7);

        // The cost is TOKU now, not coins: three fine coins are 3 × COIN_TOKU.
        let picked = tender.select(3 * coconut::COIN_TOKU).unwrap();
        assert_eq!(picked.iter().map(|i| tender.notes[*i].coins).sum::<u64>(), 3);
        for i in &picked {
            let n = &tender.notes[*i];
            assert_eq!(store.submit(&n.payment, n.pay_info().unwrap(), 1), Verdict::Accepted);
        }
        // the note that was NOT burned is still fresh — it pays for the next request
        let left: Vec<usize> = (0..tender.notes.len()).filter(|i| !picked.contains(i)).collect();
        assert_eq!(left.len(), 1);
        let n = &tender.notes[left[0]];
        assert_eq!(n.coins, 4);
        assert_eq!(store.submit(&n.payment, n.pay_info().unwrap(), 1), Verdict::Accepted);
        // …and re-sending an already burned note verbatim is a benign replay, never a ban
        let b = &tender.notes[picked[0]];
        assert_eq!(store.submit(&b.payment, b.pay_info().unwrap(), 1), Verdict::Replay);
    }

    /// A failed tender must leave the purse untouched — a half-advanced counter that
    /// reached disk would strand the coins in between.
    #[test]
    fn a_tender_that_cannot_be_built_does_not_advance_the_purse() {
        let fk = testkit::funded();
        let sd = fk.spend_date();
        let mut purse = fk.new_purse();
        let keys = fk.keys();
        let before = purse.remaining_coins();
        // more coins than the purse holds → the whole tender fails
        assert!(purse.spend_tender(&keys, &[before, 1], sd).is_err());
        assert_eq!(purse.remaining_coins(), before);
    }

    fn fk_total(p: &Purse) -> u64 {
        p.total_coins()
    }

    /// Persisting AFTER a spend and restoring must CONTINUE the counter — the next
    /// spend uses fresh coins, so the quorum accepts it (no double-spend).
    #[test]
    fn restored_purse_continues_the_counter() {
        let fk = testkit::funded();
        let sd = fk.spend_date();
        let mut purse = fk.new_purse();
        let keys = fk.keys();
        let mut store = QuorumStore::default();

        let pi1 = PayInfo { pay_info_bytes: [1u8; 72] };
        let p1 = purse.spend(&keys, 2, &pi1, sd).unwrap();
        assert_eq!(store.submit(&p1, pi1, 1), Verdict::Accepted);

        // persist the ADVANCED purse, then restore and spend again
        let restored = Purse::restore(&purse.persist().unwrap()).unwrap();
        let mut restored = restored;
        let pi2 = PayInfo { pay_info_bytes: [2u8; 72] };
        let p2 = restored.spend(&keys, 2, &pi2, sd).unwrap();
        // fresh coins (counter continued) → accepted, NOT a double-spend
        assert_eq!(store.submit(&p2, pi2, 1), Verdict::Accepted);
    }

    /// Restoring a STALE snapshot (counter rolled back) re-spends coins — and the
    /// quorum catches it. This is exactly why the purse must be persisted BEFORE the
    /// payment is sent.
    #[test]
    fn stale_restore_is_caught_as_a_double_spend() {
        let fk = testkit::funded();
        let sd = fk.spend_date();
        let mut purse = fk.new_purse();
        let keys = fk.keys();
        let mut store = QuorumStore::default();

        // snapshot at counter 0, then spend coins 0,1 from the live purse
        let stale = purse.persist().unwrap();
        let pi1 = PayInfo { pay_info_bytes: [1u8; 72] };
        let p1 = purse.spend(&keys, 2, &pi1, sd).unwrap();
        assert_eq!(store.submit(&p1, pi1, 1), Verdict::Accepted);

        // a crash restores the STALE snapshot → re-spends coins 0,1 with new pay_info
        let mut rolled_back = Purse::restore(&stale).unwrap();
        let pi2 = PayInfo { pay_info_bytes: [2u8; 72] };
        let p2 = rolled_back.spend(&keys, 2, &pi2, sd).unwrap();
        match store.submit(&p2, pi2, 1) {
            Verdict::DoubleSpend { offender, .. } => {
                assert!(offender == fk.user_pubkey(), "wrong offender")
            }
            other => panic!("expected DoubleSpend from a stale restore, got {other:?}"),
        }
    }
}
