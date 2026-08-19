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
#[derive(Serialize, Deserialize)]
pub enum FedRequest {
    /// Publish the federation's aggregated verification key + per-authority keys +
    /// epoch material, so a client can verify shares and spend.
    Keys,
    /// Ask this authority to blind-sign a withdrawal request with its key share.
    Withdraw {
        user_pk: PublicKeyUser,
        req: WithdrawalRequest,
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
}

impl Authority {
    pub fn index(&self) -> u64 {
        self.index
    }

    /// Coins per issued ticketbook — what one authorized Withdraw hands out.
    /// The paywall (server/pay.rs) prices a withdrawal as
    /// `total_coins × COIN_SCRAI` of entitlement.
    pub fn total_coins(&self) -> u64 {
        self.total_coins
    }

    /// Handle a client request. This is the whole authority-side protocol surface.
    pub fn handle(&self, req: FedRequest) -> Result<FedResponse, String> {
        match req {
            FedRequest::Keys => Ok(FedResponse::Keys {
                vk: self.vk.clone(),
                auth_vks: self.auth_vks.clone(),
                indices: self.indices.clone(),
                coin_sigs: self.coin_sigs.clone(),
                date_sigs: self.date_sigs.clone(),
                expiration_date: self.expiration_date,
                total_coins: self.total_coins,
            }),
            FedRequest::Withdraw { user_pk, req } => {
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

    /// Verify a spent payment offline against this federation's aggregated key.
    pub fn verify_payment(
        &self,
        payment: &Payment,
        pay_info: &PayInfo,
        spend_date: u32,
    ) -> Result<(), String> {
        coconut::verify(payment, &self.vk, pay_info, spend_date)
    }
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
        let authorities = bootstrap(2, 2, 32, expiration_date).unwrap();

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
        let live = bootstrap(2, 2, 32, expiration_date).unwrap();
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
                .handle(FedRequest::Withdraw { user_pk: user.public_key(), req: req.clone() })
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
        let auth = bootstrap(2, 2, 32, 1702166400).unwrap();
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
        let auth = bootstrap(1, 1, 32, 1702166400).unwrap();
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
        let auth = bootstrap(1, 1, 32, exp).unwrap();
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
            .handle(FedRequest::Withdraw { user_pk: user.public_key(), req })
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
