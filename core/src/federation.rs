#![allow(dead_code)] // wrapped by the nym transport + server binary in a later phase
// ---------------------------------------------------------------------------
// federation.rs — the coconut credential PROTOCOL: the wire messages between the
// client and the scrai-server AUTHORITIES, and the authority-side request handlers.
//
// This is the server's request-handling brain, transport-agnostic and verifiable
// in-tree. The actual server binary wraps `Authority::handle` behind the nym-sdk
// service-provider loop; the client sends these messages over the mixnet as JSON
// (all payloads are serde — see the coconut wire-serialisation note). Crate layout
// (where the server binary lives) is decided when we spin it up; this code moves to
// the shared core crate unchanged.
// ---------------------------------------------------------------------------

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use nym_compact_ecash::common_types::BlindedSignature;
use nym_compact_ecash::scheme::coin_indices_signatures::CoinIndexSignature;
use nym_compact_ecash::scheme::expiration_date_signatures::ExpirationDateSignature;
use nym_compact_ecash::scheme::keygen::{PublicKeyUser, SecretKeyAuth, VerificationKeyAuth};
use nym_compact_ecash::scheme::withdrawal::WithdrawalRequest;
use nym_compact_ecash::setup::Parameters;
use nym_compact_ecash::{aggregate_verification_keys, ttp_keygen};

use nym_compact_ecash::scheme::{PayInfo, Payment};

use crate::coconut;
use crate::quorum::{QuorumStore, ServerId, Verdict};

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Client → authority request.
// The Spend variant carries a full `Payment` (large); boxing it would change the wire
// (de)serialisation shape, and these are short-lived one-per-request values, so the size
// spread is fine here.
#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize)]
pub enum FedRequest {
    /// Publish the federation's aggregated verification key + per-authority keys +
    /// epoch material, so a client can verify shares and spend. Bare `Keys` means the
    /// FINE denomination — the only one that existed before 2026-09-15.
    Keys,
    /// The same, for one denomination (`coconut::DENOMS`) and optionally one EPOCH. Each
    /// denomination is its own issuing authority with its own material, so they are fetched
    /// — and cached — apart. `expiration_date` 0 means the epoch that issues today; naming
    /// one is how a client that still holds books from an older epoch gets the material to
    /// spend them, which it cannot reconstruct and cannot do without.
    KeysFor {
        denom_toku: u64,
        #[serde(default)]
        expiration_date: u32,
    },
    /// Ask this authority to blind-sign a withdrawal request with its key share.
    /// `denom_toku` picks which issuing key signs it; absent means the fine one.
    /// `expiration_date` names the EPOCH the request was built for — a withdrawal request
    /// is bound to it, so signing with another epoch's key produces a credential the
    /// client cannot unblind. 0 means "whatever you issue today".
    Withdraw {
        user_pk: PublicKeyUser,
        req: WithdrawalRequest,
        #[serde(default)]
        denom_toku: u64,
        #[serde(default)]
        expiration_date: u32,
    },
    /// Spend a payment: the server verifies it offline and records its serials in
    /// the double-spend quorum. `pay_info` is the raw 72 bytes (PayInfo isn't serde).
    Spend {
        payment: Payment,
        pay_info: Vec<u8>,
        spend_date: u32,
    },
}

/// Authority → client response.
#[derive(Debug, Serialize, Deserialize)]
pub enum FedResponse {
    Keys {
        /// Aggregated verification key (spend/verify against this).
        vk: VerificationKeyAuth,
        /// Per-authority verification keys, index-aligned with `indices` (verify shares).
        auth_vks: Vec<VerificationKeyAuth>,
        indices: Vec<u64>,
        /// Epoch material every client needs to spend.
        coin_sigs: Vec<CoinIndexSignature>,
        date_sigs: Vec<ExpirationDateSignature>,
        expiration_date: u32,
        /// Ticketbook size (coins per credential) — the client needs it to spend.
        total_coins: u64,
        /// TOKU one coin of this set is worth (see `coconut::DENOMS`).
        #[serde(default = "crate::purse::fine_denom")]
        denom_toku: u64,
    },
    Withdraw {
        blinded: BlindedSignature,
    },
    /// Result of a spend: accepted (recorded), or refused (invalid / double-spend).
    Spend {
        accepted: bool,
        verdict: String,
    },
    /// A handler error, returned in-band so the transport loop never panics.
    Error {
        message: String,
    },
}

/// One scrai-server acting as a t-of-n issuing authority. Holds ONLY its own key
/// share plus the federation's published material.
pub struct Authority {
    index: u64,
    sk: SecretKeyAuth,
    vk: VerificationKeyAuth,
    auth_vks: Vec<VerificationKeyAuth>,
    indices: Vec<u64>,
    coin_sigs: Vec<CoinIndexSignature>,
    date_sigs: Vec<ExpirationDateSignature>,
    expiration_date: u32,
    total_coins: u64,
    /// TOKU one coin this authority issues is worth. The value is NOT inside the coin —
    /// this key is what makes it worth that much (see `coconut::COARSE_TOKU`).
    denom_toku: u64,
}

impl Authority {
    /// TOKU per coin this authority issues.
    pub fn denom_toku(&self) -> u64 {
        self.denom_toku
    }

    /// What one whole ticketbook of this authority is worth.
    pub fn book_toku(&self) -> u64 {
        self.total_coins.saturating_mul(self.denom_toku)
    }

    pub fn index(&self) -> u64 {
        self.index
    }

    /// The day this authority's books stop being spendable. Fixed when it is created and
    /// shared by every book it issues, which is why a server runs several at once
    /// (server/src/mint.rs).
    pub fn expiration_date(&self) -> u32 {
        self.expiration_date
    }

    /// Coins per issued ticketbook — what one authorized Withdraw hands out.
    /// The paywall (server/pay.rs) prices a withdrawal as
    /// `total_coins × COIN_TOKU` of entitlement.
    pub fn total_coins(&self) -> u64 {
        self.total_coins
    }

    /// Handle a client request. This is the whole authority-side protocol surface.
    pub fn handle(&self, req: FedRequest) -> Result<FedResponse, String> {
        match req {
            FedRequest::Keys | FedRequest::KeysFor { .. } => Ok(FedResponse::Keys {
                vk: self.vk.clone(),
                auth_vks: self.auth_vks.clone(),
                indices: self.indices.clone(),
                coin_sigs: self.coin_sigs.clone(),
                date_sigs: self.date_sigs.clone(),
                expiration_date: self.expiration_date,
                total_coins: self.total_coins,
                denom_toku: self.denom_toku,
            }),
            // Which denomination a withdrawal belongs to is decided BEFORE this point, by
            // picking the authority — an authority only ever signs its own.
            FedRequest::Withdraw { user_pk, req, .. } => {
                let blinded = coconut::issue_share(
                    &self.sk,
                    user_pk,
                    &req,
                    self.expiration_date,
                    coconut::DEFAULT_T_TYPE,
                )?;
                Ok(FedResponse::Withdraw { blinded })
            }
            FedRequest::Spend { .. } => {
                Err("spend must go through the quorum (use dispatch_enveloped)".to_string())
            }
        }
    }

    /// Verify a spent payment offline against this federation's aggregated key — and
    /// against the clock: the spend date the client signed must be close to NOW. Without
    /// that bound a coin whose record was pruned could be presented again years later with
    /// the old date and verify; with it, a coin is unspendable from (its book's expiry +
    /// `SPEND_DATE_PAST_SECS`) on, which is what makes pruning safe (docs: quorum retention).
    pub fn verify_payment(
        &self,
        payment: &Payment,
        pay_info: &PayInfo,
        spend_date: u32,
    ) -> Result<(), String> {
        if !spend_date_plausible(spend_date, clock_now()) {
            return Err("spend date out of range (check the device clock)".into());
        }
        coconut::verify(payment, &self.vk, pay_info, spend_date)
    }
}

/// How far in the past a spend date may lie and still verify. Two days: one for the
/// client's own "expiration − 1 day" choice, one for clocks and day alignment.
pub const SPEND_DATE_PAST_SECS: u32 = 2 * 86_400;
/// How far ahead: a book issued today expires ~30 days out and the client spends with
/// `expiration − 1 day`, so up to 31 days is a legitimate future date.
pub const SPEND_DATE_FUTURE_SECS: u32 = 31 * 86_400;

/// `spend_date` within [now − past, now + future].
pub fn spend_date_plausible(spend_date: u32, now: u32) -> bool {
    spend_date.saturating_add(SPEND_DATE_PAST_SECS) >= now && spend_date <= now.saturating_add(SPEND_DATE_FUTURE_SECS)
}

/// Seconds since the epoch, unless a test pinned the clock (`set_test_clock`).
fn clock_now() -> u32 {
    let pinned = TEST_CLOCK.load(std::sync::atomic::Ordering::Relaxed);
    if pinned != 0 {
        return pinned;
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}
static TEST_CLOCK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Pin "now" for tests whose fixtures carry fixed spend dates. 0 = the real clock.
pub fn set_test_clock(secs: u32) {
    TEST_CLOCK.store(secs, std::sync::atomic::Ordering::Relaxed);
}

/// On-disk form of an authority (incl. its secret share). The published material is
/// serde-native; the secret key uses its own byte encoding.
#[derive(Serialize, Deserialize)]
struct PersistedAuthority {
    index: u64,
    sk_hex: String,
    vk: VerificationKeyAuth,
    auth_vks: Vec<VerificationKeyAuth>,
    indices: Vec<u64>,
    coin_sigs: Vec<CoinIndexSignature>,
    date_sigs: Vec<ExpirationDateSignature>,
    expiration_date: u32,
    total_coins: u64,
    /// Absent in a file written before denominations existed — that one is fine coins.
    #[serde(default = "crate::purse::fine_denom")]
    denom_toku: u64,
}

impl Authority {
    /// Serialise this authority (INCLUDING its secret share) to JSON, so a server
    /// restart keeps its identity. Persist the result to a secret, 0600 file.
    pub fn persist(&self) -> Result<String, String> {
        serde_json::to_string(&PersistedAuthority {
            index: self.index,
            sk_hex: hex::encode(self.sk.to_bytes()),
            vk: self.vk.clone(),
            auth_vks: self.auth_vks.clone(),
            indices: self.indices.clone(),
            coin_sigs: self.coin_sigs.clone(),
            date_sigs: self.date_sigs.clone(),
            expiration_date: self.expiration_date,
            total_coins: self.total_coins,
            denom_toku: self.denom_toku,
        })
        .map_err(err)
    }

    /// Reconstruct an authority from its persisted JSON.
    pub fn restore(json: &str) -> Result<Authority, String> {
        let p: PersistedAuthority = serde_json::from_str(json).map_err(err)?;
        let sk = SecretKeyAuth::from_bytes(&hex::decode(&p.sk_hex).map_err(err)?).map_err(err)?;
        Ok(Authority {
            index: p.index,
            sk,
            vk: p.vk,
            auth_vks: p.auth_vks,
            indices: p.indices,
            coin_sigs: p.coin_sigs,
            date_sigs: p.date_sigs,
            expiration_date: p.expiration_date,
            total_coins: p.total_coins,
            denom_toku: p.denom_toku,
        })
    }
}

/// The whole request surface as bytes: deserialise a JSON request, handle it, and
/// serialise the JSON response. This is exactly what the nym service-provider loop
/// wraps — one transport-agnostic function that NEVER panics (errors come back as
/// `FedResponse::Error`).
pub fn dispatch(authority: &Authority, request: &[u8]) -> Vec<u8> {
    let resp = match serde_json::from_slice::<FedRequest>(request) {
        Ok(req) => authority
            .handle(req)
            .unwrap_or_else(|e| FedResponse::Error { message: e }),
        Err(e) => FedResponse::Error {
            message: format!("bad request: {e}"),
        },
    };
    serde_json::to_vec(&resp)
        .unwrap_or_else(|_| br#"{"Error":{"message":"response encode failed"}}"#.to_vec())
}

/// Same as `dispatch`, but wrapped in the app's id-correlated envelope so it rides
/// the existing mixnet transport (the client's `round_trip` matches replies by `id`).
/// Request: `{"id":..,"fed":<FedRequest>}` → reply: `{"id":<same>,"fed":<FedResponse>}`.
/// This is what the server's mixnet loop calls; `FedResponse::Error` carries handler
/// errors so the envelope's own `kind` never has to signal one.
pub fn dispatch_enveloped(
    authority: &Authority,
    store: &mut QuorumStore,
    request: &[u8],
) -> Vec<u8> {
    let v: Value = serde_json::from_slice(request).unwrap_or(Value::Null);
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let resp = match serde_json::from_value::<FedRequest>(v.get("fed").cloned().unwrap_or(Value::Null))
    {
        Ok(FedRequest::Spend { payment, pay_info, spend_date }) => {
            handle_spend(authority, store, payment, pay_info, spend_date)
        }
        // M1: a key the quorum has caught double-spending (its ban is computed + persisted in
        // `submit`) is locked out of withdrawing FRESH ticketbooks. Per-coin protection already
        // refuses reused serials; this shuts the anti-griefing door the audit found inert.
        Ok(FedRequest::Withdraw { user_pk, req, denom_toku, expiration_date }) => {
            if store.is_blacklisted(&user_pk) {
                FedResponse::Error {
                    message: "blacklisted: this key was caught double-spending and may not withdraw".into(),
                }
            } else {
                authority
                    .handle(FedRequest::Withdraw { user_pk, req, denom_toku, expiration_date })
                    .unwrap_or_else(|e| FedResponse::Error { message: e })
            }
        }
        Ok(req) => authority
            .handle(req)
            .unwrap_or_else(|e| FedResponse::Error { message: e }),
        Err(e) => FedResponse::Error {
            message: format!("bad request: {e}"),
        },
    };
    let reply = json!({ "id": id, "fed": serde_json::to_value(&resp).unwrap_or(Value::Null) });
    serde_json::to_vec(&reply).unwrap_or_default()
}

/// This server's id in the quorum (multi-server assigns distinct ids).
const THIS_SERVER: ServerId = 1;

/// This server's quorum id, for callers outside this module (chat settles coin payments
/// into the same quorum the federation spend path uses).
pub fn this_server() -> ServerId {
    THIS_SERVER
}

/// Verify a spent payment offline, then record it in the double-spend quorum.
fn handle_spend(
    authority: &Authority,
    store: &mut QuorumStore,
    payment: Payment,
    pay_info: Vec<u8>,
    spend_date: u32,
) -> FedResponse {
    let bytes: [u8; 72] = match pay_info.as_slice().try_into() {
        Ok(b) => b,
        Err(_) => {
            return FedResponse::Spend { accepted: false, verdict: "bad pay_info length".into() }
        }
    };
    let pi = PayInfo { pay_info_bytes: bytes };
    if let Err(e) = authority.verify_payment(&payment, &pi, spend_date) {
        return FedResponse::Spend { accepted: false, verdict: format!("invalid: {e}") };
    }
    match store.submit(&payment, pi, THIS_SERVER) {
        Verdict::Accepted | Verdict::Replay => {
            FedResponse::Spend { accepted: true, verdict: "accepted".into() }
        }
        Verdict::DoubleSpend { .. } => {
            FedResponse::Spend { accepted: false, verdict: "double-spend rejected".into() }
        }
    }
}

/// Trusted-dealer bootstrap for the two-test-server phase: build `n` authorities
/// with a `t`-of-`n` threshold. Real deployment replaces this with a DKG that hands
/// each server its share (crypto identical); the published material is the same.
pub fn bootstrap(
    n: usize,
    t: u64,
    total_coins: u64,
    expiration_date: u32,
    denom_toku: u64,
) -> Result<Vec<Authority>, String> {
    let params = Parameters::new(total_coins);
    let auths = ttp_keygen(t, n as u64).map_err(err)?;
    let indices: Vec<u64> = (1..=n as u64).collect();
    let sks: Vec<&SecretKeyAuth> = auths.iter().map(|k| k.secret_key()).collect();
    let auth_vks: Vec<VerificationKeyAuth> = auths.iter().map(|k| k.verification_key()).collect();
    let vk = aggregate_verification_keys(&auth_vks, Some(indices.as_slice())).map_err(err)?;
    let (coin_sigs, date_sigs) =
        coconut::epoch_material_local(&params, &vk, &indices, &sks, &auth_vks, expiration_date)?;

    Ok(auths
        .iter()
        .enumerate()
        .map(|(i, kp)| Authority {
            index: indices[i],
            sk: kp.secret_key().clone(),
            vk: vk.clone(),
            auth_vks: auth_vks.clone(),
            indices: indices.clone(),
            coin_sigs: coin_sigs.clone(),
            date_sigs: date_sigs.clone(),
            expiration_date,
            total_coins,
            denom_toku,
        })
        .collect())
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use nym_compact_ecash::generate_keypair_user;
    use serde::de::DeserializeOwned;

    /// Round-trip a message through JSON, exactly as the mixnet would carry it.
    fn wire<T: Serialize + DeserializeOwned>(x: &T) -> T {
        serde_json::from_str(&serde_json::to_string(x).unwrap()).unwrap()
    }

    #[test]
    fn client_withdraws_across_authorities_over_the_wire() {
        let expiration_date = 1702166400u32;
        let authorities = bootstrap(2, 2, 32, expiration_date, crate::coconut::COIN_TOKU).unwrap();

        // 1) client fetches the published keys (request + response cross the wire)
        let req0 = wire(&FedRequest::Keys);
        let (vk, auth_vks, coin_sigs, date_sigs) = match wire(&authorities[0].handle(req0).unwrap()) {
            FedResponse::Keys { vk, auth_vks, coin_sigs, date_sigs, .. } => {
                (vk, auth_vks, coin_sigs, date_sigs)
            }
            _ => panic!("expected Keys"),
        };

        // 2) client builds a withdrawal request and sends it to EACH authority
        let user = generate_keypair_user();
        let (req, req_info) = coconut::make_withdrawal_request(
            user.secret_key(),
            expiration_date,
            coconut::DEFAULT_T_TYPE,
        )
        .unwrap();

        let mut shares = Vec::new();
        for (i, a) in authorities.iter().enumerate() {
            let request = wire(&FedRequest::Withdraw {
                denom_toku: crate::coconut::COIN_TOKU,
                expiration_date: 0,
                user_pk: user.public_key(),
                req: req.clone(),
            });
            let blinded = match wire(&a.handle(request).unwrap()) {
                FedResponse::Withdraw { blinded } => blinded,
                _ => panic!("expected Withdraw"),
            };
            shares.push(
                coconut::verify_share(
                    &auth_vks[i],
                    user.secret_key(),
                    &blinded,
                    &req_info,
                    i as u64 + 1,
                )
                .unwrap(),
            );
        }

        // 3) aggregate into a wallet and spend one coin against the published vk
        let mut wallet = coconut::aggregate(&vk, user.secret_key(), &shares, &req_info).unwrap();
        let params = Parameters::new(32);
        let pi = nym_compact_ecash::scheme::PayInfo { pay_info_bytes: [9u8; 72] };
        let payment = coconut::spend(
            &mut wallet, &params, &vk, user.secret_key(), &pi, 1, &date_sigs, &coin_sigs, 1701907200,
        )
        .unwrap();
        assert!(coconut::verify(&payment, &vk, &pi, 1701907200).is_ok());

        // 4) any server can feed it to the quorum store
        let mut store = crate::quorum::QuorumStore::default();
        assert_eq!(store.submit(&payment, pi, 1), crate::quorum::Verdict::Accepted);
    }

    #[test]
    fn authority_persists_and_a_restored_federation_still_issues() {
        let expiration_date = 1702166400u32;
        let live = bootstrap(2, 2, 32, expiration_date, crate::coconut::COIN_TOKU).unwrap();
        // persist → restore BOTH authorities (models a server restart)
        let auth: Vec<Authority> = live
            .iter()
            .map(|a| Authority::restore(&a.persist().unwrap()).unwrap())
            .collect();

        // a client withdraws against the RESTORED authorities → wallet must be valid
        let (vk, auth_vks, coin_sigs, date_sigs) = match auth[0].handle(FedRequest::Keys).unwrap() {
            FedResponse::Keys { vk, auth_vks, coin_sigs, date_sigs, .. } => {
                (vk, auth_vks, coin_sigs, date_sigs)
            }
            _ => panic!("expected Keys"),
        };
        let user = generate_keypair_user();
        let (req, req_info) = coconut::make_withdrawal_request(
            user.secret_key(), expiration_date, coconut::DEFAULT_T_TYPE,
        )
        .unwrap();
        let mut shares = Vec::new();
        for (i, a) in auth.iter().enumerate() {
            let blinded = match a
                .handle(FedRequest::Withdraw { user_pk: user.public_key(), req: req.clone(), denom_toku: crate::coconut::COIN_TOKU, expiration_date: 0 })
                .unwrap()
            {
                FedResponse::Withdraw { blinded } => blinded,
                _ => panic!("expected Withdraw"),
            };
            shares.push(
                coconut::verify_share(&auth_vks[i], user.secret_key(), &blinded, &req_info, i as u64 + 1)
                    .unwrap(),
            );
        }
        let mut wallet = coconut::aggregate(&vk, user.secret_key(), &shares, &req_info).unwrap();
        let params = Parameters::new(32);
        let pi = nym_compact_ecash::scheme::PayInfo { pay_info_bytes: [3u8; 72] };
        let payment = coconut::spend(
            &mut wallet, &params, &vk, user.secret_key(), &pi, 1, &date_sigs, &coin_sigs, 1701907200,
        )
        .unwrap();
        assert!(coconut::verify(&payment, &vk, &pi, 1701907200).is_ok());
    }

    #[test]
    fn dispatch_round_trips_and_never_panics() {
        let auth = bootstrap(2, 2, 32, 1702166400, crate::coconut::COIN_TOKU).unwrap();
        // a well-formed Keys request → a Keys response
        let bytes = dispatch(&auth[0], &serde_json::to_vec(&FedRequest::Keys).unwrap());
        match serde_json::from_slice::<FedResponse>(&bytes).unwrap() {
            FedResponse::Keys { indices, .. } => assert_eq!(indices, vec![1, 2]),
            other => panic!("expected Keys, got {other:?}"),
        }
        // garbage in → an Error response, not a panic
        let bad = dispatch(&auth[0], b"not json at all");
        assert!(matches!(
            serde_json::from_slice::<FedResponse>(&bad).unwrap(),
            FedResponse::Error { .. }
        ));
    }

    #[test]
    fn enveloped_dispatch_echoes_id_and_wraps_fed() {
        let auth = bootstrap(1, 1, 32, 1702166400, crate::coconut::COIN_TOKU).unwrap();
        let env = json!({
            "v": 1, "kind": "coconut", "id": "req-123",
            "fed": serde_json::to_value(FedRequest::Keys).unwrap(),
        });
        let mut store = QuorumStore::default();
        let reply: Value = serde_json::from_slice(&dispatch_enveloped(
            &auth[0], &mut store, &serde_json::to_vec(&env).unwrap(),
        ))
        .unwrap();
        assert_eq!(reply["id"], "req-123"); // id echoed for round_trip correlation
        let fed: FedResponse = serde_json::from_value(reply["fed"].clone()).unwrap();
        assert!(matches!(fed, FedResponse::Keys { .. }));
    }

    #[test]
    fn spend_accepted_then_double_spend_rejected() {
        use nym_compact_ecash::scheme::Wallet;
        let exp = 1702166400u32;
        let spend_date = 1701907200u32;
        set_test_clock(spend_date);
        let auth = bootstrap(1, 1, 32, exp, crate::coconut::COIN_TOKU).unwrap();
        let mut store = QuorumStore::default();

        // fetch keys + withdraw a wallet from the single authority
        let (vk, auth_vks, coin_sigs, date_sigs) = match auth[0].handle(FedRequest::Keys).unwrap() {
            FedResponse::Keys { vk, auth_vks, coin_sigs, date_sigs, .. } => {
                (vk, auth_vks, coin_sigs, date_sigs)
            }
            _ => panic!("keys"),
        };
        let user = nym_compact_ecash::generate_keypair_user();
        let (req, req_info) =
            coconut::make_withdrawal_request(user.secret_key(), exp, coconut::DEFAULT_T_TYPE).unwrap();
        let blinded = match auth[0]
            .handle(FedRequest::Withdraw { user_pk: user.public_key(), req, denom_toku: crate::coconut::COIN_TOKU, expiration_date: 0 })
            .unwrap()
        {
            FedResponse::Withdraw { blinded } => blinded,
            _ => panic!("withdraw"),
        };
        let share =
            coconut::verify_share(&auth_vks[0], user.secret_key(), &blinded, &req_info, 1).unwrap();
        let mut wallet = coconut::aggregate(&vk, user.secret_key(), &[share], &req_info).unwrap();
        let mut copy = Wallet::from_bytes(&wallet.to_bytes()).unwrap();
        let params = Parameters::new(32);

        let spend_verdict = |store: &mut QuorumStore, payment, pib: [u8; 72]| -> (bool, String) {
            let env = json!({"id":"s","fed": serde_json::to_value(
                FedRequest::Spend { payment, pay_info: pib.to_vec(), spend_date }).unwrap()});
            let reply: Value = serde_json::from_slice(&dispatch_enveloped(
                &auth[0], store, &serde_json::to_vec(&env).unwrap(),
            ))
            .unwrap();
            match serde_json::from_value::<FedResponse>(reply["fed"].clone()).unwrap() {
                FedResponse::Spend { accepted, verdict } => (accepted, verdict),
                o => panic!("expected Spend, got {o:?}"),
            }
        };

        // spend a coin → accepted
        let pi = PayInfo { pay_info_bytes: [9u8; 72] };
        let payment = coconut::spend(
            &mut wallet, &params, &vk, user.secret_key(), &pi, 1, &date_sigs, &coin_sigs, spend_date,
        )
        .unwrap();
        let (ok1, _) = spend_verdict(&mut store, payment, [9u8; 72]);
        assert!(ok1, "first spend must be accepted");

        // double-spend the same coin from the copy → refused
        let pi2 = PayInfo { pay_info_bytes: [8u8; 72] };
        let payment2 = coconut::spend(
            &mut copy, &params, &vk, user.secret_key(), &pi2, 1, &date_sigs, &coin_sigs, spend_date,
        )
        .unwrap();
        let (ok2, verdict2) = spend_verdict(&mut store, payment2, [8u8; 72]);
        assert!(!ok2, "double-spend must be refused");
        assert!(verdict2.contains("double-spend"), "verdict: {verdict2}");
    }
}
