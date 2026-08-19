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
    pub fn remaining_coins(&self) -> u64 {
        let bytes = self.wallet.to_bytes();
        let n = bytes.len();
        let spent = <[u8; 8]>::try_from(&bytes[n - 8..])
            .map(u64::from_be_bytes)
            .unwrap_or(0);
        self.total_coins.saturating_sub(spent)
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
