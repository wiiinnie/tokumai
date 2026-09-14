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
#[derive(Serialize, Deserialize)]
pub struct Purse {
    wallet: Wallet,
    user: KeyPairUser,
    vk: VerificationKeyAuth,
    coin_sigs: Vec<CoinIndexSignature>,
    date_sigs: Vec<ExpirationDateSignature>,
    total_coins: u64,
    expiration_date: u32,
}

impl Purse {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        wallet: Wallet,
        user: KeyPairUser,
        vk: VerificationKeyAuth,
        coin_sigs: Vec<CoinIndexSignature>,
        date_sigs: Vec<ExpirationDateSignature>,
        total_coins: u64,
        expiration_date: u32,
    ) -> Self {
        Self { wallet, user, vk, coin_sigs, date_sigs, total_coins, expiration_date }
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
        coins: u64,
        pay_info: &PayInfo,
        spend_date: u32,
    ) -> Result<Payment, String> {
        let params = Parameters::new(self.total_coins);
        coconut::spend(
            &mut self.wallet,
            &params,
            &self.vk,
            self.user.secret_key(),
            pay_info,
            coins,
            &self.date_sigs,
            &self.coin_sigs,
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
            let payment = probe.spend(*coins, &pi, spend_date)?;
            notes.push(crate::tender::Note {
                coins: *coins,
                payment,
                pay_info: bytes.to_vec(),
                spend_date,
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
    pub fn verification_key(&self) -> &VerificationKeyAuth {
        &self.vk
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
        let mut store = QuorumStore::default();

        // ceiling 7 coins → notes 1,2,4; the answer turns out to cost 3
        let values = plan_coins(7);
        assert_eq!(values, vec![1, 2, 4]);
        let notes = purse.spend_tender(&values, sd).unwrap();
        let tender = Tender { notes };
        tender.well_formed().unwrap();
        assert_eq!(tender.total_coins(), 7);
        assert_eq!(purse.remaining_coins(), fk_total(&purse) - 7);

        let picked = tender.select(3).unwrap();
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
        let before = purse.remaining_coins();
        // more coins than the purse holds → the whole tender fails
        assert!(purse.spend_tender(&[before, 1], sd).is_err());
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
        let mut store = QuorumStore::default();

        let pi1 = PayInfo { pay_info_bytes: [1u8; 72] };
        let p1 = purse.spend(2, &pi1, sd).unwrap();
        assert_eq!(store.submit(&p1, pi1, 1), Verdict::Accepted);

        // persist the ADVANCED purse, then restore and spend again
        let restored = Purse::restore(&purse.persist().unwrap()).unwrap();
        let mut restored = restored;
        let pi2 = PayInfo { pay_info_bytes: [2u8; 72] };
        let p2 = restored.spend(2, &pi2, sd).unwrap();
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
        let mut store = QuorumStore::default();

        // snapshot at counter 0, then spend coins 0,1 from the live purse
        let stale = purse.persist().unwrap();
        let pi1 = PayInfo { pay_info_bytes: [1u8; 72] };
        let p1 = purse.spend(2, &pi1, sd).unwrap();
        assert_eq!(store.submit(&p1, pi1, 1), Verdict::Accepted);

        // a crash restores the STALE snapshot → re-spends coins 0,1 with new pay_info
        let mut rolled_back = Purse::restore(&stale).unwrap();
        let pi2 = PayInfo { pay_info_bytes: [2u8; 72] };
        let p2 = rolled_back.spend(2, &pi2, sd).unwrap();
        match store.submit(&p2, pi2, 1) {
            Verdict::DoubleSpend { offender, .. } => {
                assert!(offender == fk.user_pubkey(), "wrong offender")
            }
            other => panic!("expected DoubleSpend from a stale restore, got {other:?}"),
        }
    }
}
