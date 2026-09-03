#![allow(dead_code)] // wired into commands/protocol in later phases
// ---------------------------------------------------------------------------
// coconut.rs — the federation's threshold ecash core (Coconut / nym-compact-ecash).
//
// One credential is co-issued by t-of-n scrai-server AUTHORITIES (blind); the
// client AGGREGATES the partial signatures into a bearer wallet, then SPENDS coins
// whose payments any server VERIFIES OFFLINE (no shared state). A reused coin is
// caught by the double-spend QUORUM: `detect()` reveals the offender's PUBLIC key
// (their secret never leaves their device). See docs/federation-params.md.
//
// Roles are labelled below. Today client + authorities run in one process for the
// two-test-server bootstrap; when the server becomes its own Rust binary the
// AUTHORITY functions move there unchanged — same code, same wire types.
// ---------------------------------------------------------------------------

use nym_compact_ecash as ce;
use ce::common_types::BlindedSignature;
use ce::scheme::aggregation::aggregate_wallets;
use ce::scheme::coin_indices_signatures::{
    aggregate_indices_signatures, sign_coin_indices, CoinIndexSignature, CoinIndexSignatureShare,
};
use ce::scheme::expiration_date_signatures::{
    aggregate_expiration_signatures, sign_expiration_date, ExpirationDateSignature,
    ExpirationDateSignatureShare,
};
use ce::scheme::identify::{identify, IdentifyResult};
use ce::scheme::keygen::{PublicKeyUser, SecretKeyAuth, SecretKeyUser, VerificationKeyAuth};
use ce::scheme::withdrawal::{issue, issue_verify, withdrawal_request};
use ce::scheme::{PartialWallet, Wallet};
pub use ce::scheme::{PayInfo, Payment};
// The client persists an in-flight withdrawal (M-cl-2): request + blinding openings.
pub use ce::scheme::withdrawal::{RequestInfo, WithdrawalRequest};
use ce::setup::Parameters;

// ---- economic parameters (docs/federation-params.md) ----------------------
/// 10 USD = 1_000_000 SCRAI (existing peg).
pub const SCRAI_PER_USD: u64 = 100_000;
/// One ecash coin = 1000 SCRAI = $0.01. Coarse on purpose (one serial per coin).
pub const COIN_SCRAI: u64 = 1_000;
/// Redeem ~$1 (100 coins) into a session at a time, uniform across users.
pub const REDEEM_CHUNK_COINS: u64 = 100;
/// Ticket type / denomination class (single class for now).
pub const DEFAULT_T_TYPE: u8 = 1;

/// Number of coins in a ticketbook bought for `tier_scrai` SCRAI.
pub fn book_coins(tier_scrai: u64) -> u64 {
    tier_scrai / COIN_SCRAI
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

// ---- CLIENT ---------------------------------------------------------------

/// The user's ecash keypair — the client-side identity behind a wallet. Its PUBLIC
/// key is what a double-spend would reveal; the secret never leaves the device.
pub use ce::scheme::keygen::KeyPairUser;

/// Generate a fresh user ecash keypair.
pub fn new_user() -> KeyPairUser {
    ce::generate_keypair_user()
}

/// Build a blind withdrawal request to send to each authority.
pub fn make_withdrawal_request(
    user_sk: &SecretKeyUser,
    expiration_date: u32,
    t_type: u8,
) -> Result<(WithdrawalRequest, RequestInfo), String> {
    withdrawal_request(user_sk, expiration_date, t_type).map_err(err)
}

/// Unblind + verify one authority's blinded signature into a partial wallet.
pub fn verify_share(
    vk_auth: &VerificationKeyAuth,
    user_sk: &SecretKeyUser,
    blinded: &BlindedSignature,
    req_info: &RequestInfo,
    signer_index: u64,
) -> Result<PartialWallet, String> {
    issue_verify(vk_auth, user_sk, blinded, req_info, signer_index).map_err(err)
}

/// Aggregate the authorities' partial wallets into one bearer wallet.
pub fn aggregate(
    vk: &VerificationKeyAuth,
    user_sk: &SecretKeyUser,
    shares: &[PartialWallet],
    req_info: &RequestInfo,
) -> Result<Wallet, String> {
    aggregate_wallets(vk, user_sk, shares, req_info).map_err(err)
}

/// Spend `value` coins, producing a payment any server can verify offline.
#[allow(clippy::too_many_arguments)]
pub fn spend(
    wallet: &mut Wallet,
    params: &Parameters,
    vk: &VerificationKeyAuth,
    user_sk: &SecretKeyUser,
    pay_info: &PayInfo,
    value: u64,
    date_sigs: &[ExpirationDateSignature],
    coin_sigs: &[CoinIndexSignature],
    spend_date: u32,
) -> Result<Payment, String> {
    wallet
        .spend(
            params, vk, user_sk, pay_info, value, date_sigs, coin_sigs, spend_date,
        )
        .map_err(err)
}

// ---- AUTHORITY (runs on each scrai-server) --------------------------------

/// One authority blind-signs a withdrawal request with its key share.
pub fn issue_share(
    auth_sk: &SecretKeyAuth,
    user_pk: PublicKeyUser,
    req: &WithdrawalRequest,
    expiration_date: u32,
    t_type: u8,
) -> Result<BlindedSignature, String> {
    issue(auth_sk, user_pk, req, expiration_date, t_type).map_err(err)
}

/// Produce the per-epoch signatures (coin-index + expiration-date) that EVERY
/// client needs in order to spend. Each authority signs with its share; the shares
/// are aggregated into the epoch material (published to clients like the aggregated
/// verification key, once per credential-validity epoch).
///
/// This is the all-local form for the bootstrap / two-test-server setup where the
/// authority keys sit together. In the real distributed server it splits — each
/// server calls `sign_coin_indices`/`sign_expiration_date` with ITS key and a
/// coordinator aggregates — but the crypto is byte-for-byte identical.
pub fn epoch_material_local(
    params: &Parameters,
    vk: &VerificationKeyAuth,
    indices: &[u64],
    auth_sks: &[&SecretKeyAuth],
    auth_vks: &[VerificationKeyAuth],
    expiration_date: u32,
) -> Result<(Vec<CoinIndexSignature>, Vec<ExpirationDateSignature>), String> {
    let n = auth_sks.len();
    let mut coin_shares = Vec::with_capacity(n);
    let mut date_shares = Vec::with_capacity(n);
    for k in 0..n {
        coin_shares.push(CoinIndexSignatureShare {
            index: indices[k],
            key: auth_vks[k].clone(),
            signatures: sign_coin_indices(params, vk, auth_sks[k]).map_err(err)?,
        });
        date_shares.push(ExpirationDateSignatureShare {
            index: indices[k],
            key: auth_vks[k].clone(),
            signatures: sign_expiration_date(auth_sks[k], expiration_date).map_err(err)?,
        });
    }
    let coin_sigs = aggregate_indices_signatures(params, vk, &coin_shares).map_err(err)?;
    let date_sigs = aggregate_expiration_signatures(vk, expiration_date, &date_shares).map_err(err)?;
    Ok((coin_sigs, date_sigs))
}

// ---- VERIFIER (any server) ------------------------------------------------

/// Verify a payment OFFLINE against the aggregated key — no shared state at all.
/// This is what makes an isolated-but-online server keep serving.
pub fn verify(
    payment: &Payment,
    vk: &VerificationKeyAuth,
    pay_info: &PayInfo,
    spend_date: u32,
) -> Result<(), String> {
    payment
        .spend_verify(vk, pay_info, spend_date)
        .map(|_| ())
        .map_err(err)
}

// ---- QUORUM (double-spend detection) --------------------------------------

/// The quorum's verdict when two payments are compared.
#[derive(Debug)]
pub enum DoubleSpend {
    /// No shared coin serial — unrelated payments.
    None,
    /// Same serial AND same pay_info — a benign replay/retry. NEVER a ban:
    /// idempotent client retries land here.
    Replay,
    /// Genuine reuse — the offender's public key, for the blacklist.
    Detected(PublicKeyUser),
}

/// Compare two payments; on a genuine reuse, cryptographically reveal the offender.
/// Ban decisions must be gated on `Detected` (a proof), never on a Bloom-filter hit.
pub fn detect(p1: &Payment, p2: &Payment, pi1: PayInfo, pi2: PayInfo) -> DoubleSpend {
    match identify(p1, p2, pi1, pi2) {
        IdentifyResult::NotADuplicatePayment => DoubleSpend::None,
        IdentifyResult::DuplicatePayInfo(_) => DoubleSpend::Replay,
        IdentifyResult::DoubleSpendingPublicKeys(pk) => DoubleSpend::Detected(pk),
    }
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use nym_compact_ecash::scheme::keygen::SecretKeyAuth;
    use nym_compact_ecash::setup::Parameters;
    use nym_compact_ecash::{aggregate_verification_keys, generate_keypair_user, ttp_keygen};

    /// Full lifecycle through OUR wrappers: 2-of-2 blind issuance → aggregate →
    /// offline spend + verify → double-spend (copied bearer wallet) is detected and
    /// the offender revealed → a same-pay_info replay is NOT flagged as a ban.
    #[test]
    fn lifecycle_and_double_spend() {
        let total_coins = 32; // small L for test speed; prod = tier/coin
        let params = Parameters::new(total_coins);
        let spend_date: u32 = 1701907200; // 00:00:00-aligned
        let expiration_date: u32 = 1702166400;

        let user = generate_keypair_user();
        let auths = ttp_keygen(2, 2).unwrap();
        let indices: Vec<u64> = (1..=auths.len() as u64).collect();
        let sks: Vec<&SecretKeyAuth> = auths.iter().map(|k| k.secret_key()).collect();
        let vks: Vec<_> = auths.iter().map(|k| k.verification_key()).collect();
        let vk = aggregate_verification_keys(&vks, Some(indices.as_slice())).unwrap();

        // authority-side epoch material — produced by OUR code, no test helpers
        let (coin_sigs, date_sigs) =
            epoch_material_local(&params, &vk, &indices, &sks, &vks, expiration_date).unwrap();

        // client withdraws through the wrappers
        let (req, req_info) =
            make_withdrawal_request(user.secret_key(), expiration_date, DEFAULT_T_TYPE).unwrap();
        let mut shares = Vec::new();
        for (i, kp) in auths.iter().enumerate() {
            let blinded = issue_share(
                kp.secret_key(),
                user.public_key(),
                &req,
                expiration_date,
                DEFAULT_T_TYPE,
            )
            .unwrap();
            shares.push(
                verify_share(&vks[i], user.secret_key(), &blinded, &req_info, i as u64 + 1).unwrap(),
            );
        }
        let mut wallet = aggregate(&vk, user.secret_key(), &shares, &req_info).unwrap();
        // a second independent copy of the same bearer wallet (rollback / two devices)
        let mut wallet_copy = Wallet::from_bytes(&wallet.to_bytes()).unwrap();

        let pi1 = PayInfo { pay_info_bytes: [6u8; 72] };
        let p1 = spend(
            &mut wallet, &params, &vk, user.secret_key(), &pi1, 1, &date_sigs, &coin_sigs,
            spend_date,
        )
        .unwrap();
        assert!(verify(&p1, &vk, &pi1, spend_date).is_ok());

        let pi2 = PayInfo { pay_info_bytes: [7u8; 72] };
        let p2 = spend(
            &mut wallet_copy, &params, &vk, user.secret_key(), &pi2, 1, &date_sigs, &coin_sigs,
            spend_date,
        )
        .unwrap();
        assert!(verify(&p2, &vk, &pi2, spend_date).is_ok());

        // quorum detects the reuse and reveals the offender
        match detect(&p1, &p2, pi1, pi2) {
            DoubleSpend::Detected(pk) => assert!(pk == user.public_key(), "wrong key"),
            other => panic!("expected Detected, got {other:?}"),
        }

        // a benign replay (same pay_info) must NOT read as a ban
        let a = PayInfo { pay_info_bytes: [6u8; 72] };
        let b = PayInfo { pay_info_bytes: [6u8; 72] };
        assert!(matches!(detect(&p1, &p1, a, b), DoubleSpend::Replay));

        // wire types survive JSON transport (the mixnet carries JSON) — BLS group
        // elements through serde are finicky, so prove the round-trips.
        let p_back: Payment = serde_json::from_str(&serde_json::to_string(&p1).unwrap()).unwrap();
        assert!(verify(&p_back, &vk, &pi1, spend_date).is_ok(), "payment via JSON");
        let vk_back: VerificationKeyAuth =
            serde_json::from_str(&serde_json::to_string(&vk).unwrap()).unwrap();
        assert!(verify(&p1, &vk_back, &pi1, spend_date).is_ok(), "vk via JSON");
        let _req_back: WithdrawalRequest =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
    }

    #[test]
    fn book_coins_maps_tiers() {
        // $10 = 1_000_000 SCRAI → 1000 coins of $0.01
        assert_eq!(book_coins(1_000_000), 1_000);
        assert_eq!(book_coins(500_000), 500); // $5
    }
}

/// Shared test scaffolding so other modules' tests (e.g. the quorum store) can mint
/// real payments without duplicating the heavy setup.
#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use nym_compact_ecash::scheme::keygen::{KeyPairAuth, KeyPairUser, SecretKeyAuth};
    use nym_compact_ecash::{aggregate_verification_keys, generate_keypair_user, ttp_keygen};

    pub struct Funded {
        params: Parameters,
        vk: VerificationKeyAuth,
        user: KeyPairUser,
        auths: Vec<KeyPairAuth>,
        auth_vks: Vec<VerificationKeyAuth>,
        expiration_date: u32,
        date_sigs: Vec<ExpirationDateSignature>,
        coin_sigs: Vec<CoinIndexSignature>,
        spend_date: u32,
    }

    /// A 2-of-2 federation with epoch material, ready to mint wallets + payments.
    pub fn funded() -> Funded {
        let params = Parameters::new(32);
        let spend_date = 1701907200;
        let expiration_date = 1702166400;
        let user = generate_keypair_user();
        let auths = ttp_keygen(2, 2).unwrap();
        let indices: Vec<u64> = (1..=auths.len() as u64).collect();
        let sks: Vec<&SecretKeyAuth> = auths.iter().map(|k| k.secret_key()).collect();
        let auth_vks: Vec<VerificationKeyAuth> =
            auths.iter().map(|k| k.verification_key()).collect();
        let vk = aggregate_verification_keys(&auth_vks, Some(indices.as_slice())).unwrap();
        let (coin_sigs, date_sigs) =
            epoch_material_local(&params, &vk, &indices, &sks, &auth_vks, expiration_date).unwrap();
        Funded {
            params, vk, user, auths, auth_vks, expiration_date, date_sigs, coin_sigs, spend_date,
        }
    }

    impl Funded {
        /// A fresh bearer wallet for the user (32 coins).
        pub fn wallet(&self) -> Wallet {
            let (req, req_info) =
                make_withdrawal_request(self.user.secret_key(), self.expiration_date, DEFAULT_T_TYPE)
                    .unwrap();
            let mut shares = Vec::new();
            for (i, kp) in self.auths.iter().enumerate() {
                let blinded = issue_share(
                    kp.secret_key(), self.user.public_key(), &req, self.expiration_date,
                    DEFAULT_T_TYPE,
                )
                .unwrap();
                shares.push(
                    verify_share(&self.auth_vks[i], self.user.secret_key(), &blinded, &req_info, i as u64 + 1)
                        .unwrap(),
                );
            }
            aggregate(&self.vk, self.user.secret_key(), &shares, &req_info).unwrap()
        }

        /// A copy of a wallet at its current counter (models a rollback / second device).
        pub fn copy(&self, w: &Wallet) -> Wallet {
            Wallet::from_bytes(&w.to_bytes()).unwrap()
        }

        /// Spend one coin; `seed` distinguishes the pay_info (i.e. the context).
        pub fn spend_one(&self, wallet: &mut Wallet, seed: u8) -> (Payment, PayInfo) {
            let pi = PayInfo { pay_info_bytes: [seed; 72] };
            let p = spend(
                wallet, &self.params, &self.vk, self.user.secret_key(), &pi, 1, &self.date_sigs,
                &self.coin_sigs, self.spend_date,
            )
            .unwrap();
            (p, pi)
        }

        pub fn user_pubkey(&self) -> PublicKeyUser {
            self.user.public_key()
        }
        pub fn vk(&self) -> &VerificationKeyAuth {
            &self.vk
        }
        pub fn spend_date(&self) -> u32 {
            self.spend_date
        }

        /// A client purse built from this federation's material (for purse tests).
        pub fn new_purse(&self) -> crate::purse::Purse {
            crate::purse::Purse::new(
                self.wallet(),
                self.user.clone(),
                self.vk.clone(),
                self.coin_sigs.clone(),
                self.date_sigs.clone(),
                32,
                self.expiration_date,
            )
        }
    }
}
