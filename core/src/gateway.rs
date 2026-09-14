// ---------------------------------------------------------------------------
// gateway.rs — the server's top-level request router.
//
// The app sends every request in one id-correlated envelope `{v,kind,id,...}`.
// This routes by `kind`: coconut → the federation handlers; `state` (and later
// invoice / session / chat) → the ported TS-server surface. One entry point that
// the mixnet service-provider loop calls; replies keep the `id` for correlation.
//
// The model catalog here is a PLACEHOLDER (a couple of models with billing-derived
// retail rates) until the pricing table + provider integration are ported.
// ---------------------------------------------------------------------------

use serde_json::{json, Value};

use crate::billing::ceil_toku;
use crate::coconut::{PayInfo, Payment, COIN_TOKU, TOKU_PER_USD};
use crate::federation::{self, Authority};
use crate::ledger::Ledger;
use crate::quorum::{QuorumStore, ServerId, Verdict};
#[cfg(test)]
use crate::session::SessionStore;

/// Retail margin applied to provider cost for the displayed rate.
const MARGIN: f64 = 1.4;

/// This server's id in the quorum (a multi-server federation assigns distinct ids).
const THIS_SERVER: ServerId = 1;

/// Route one request. `request` is the raw envelope bytes; returns the reply bytes.
/// `quorum` records coconut serials (double-spend); `sessions` holds redeemed TOKU
/// balances that `chat` draws down (charging lives in the async chat handler).
pub async fn handle(
    authority: &Authority,
    quorum: &mut QuorumStore,
    sessions: &mut dyn Ledger,
    request: &[u8],
) -> Vec<u8> {
    let v: Value = serde_json::from_slice(request).unwrap_or(Value::Null);
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    match v.get("kind").and_then(|k| k.as_str()).unwrap_or("") {
        "coconut" => federation::dispatch_enveloped(authority, quorum, request),
        "models" => encode(&models_reply(id)),
        // Redeem coconut coins → session credit (verify the payment, record its
        // serials, then credit the session that will spend it via chat).
        "redeem" => encode(&redeem_reply(authority, quorum, sessions, &v, id).await),
        // Real session balance (0 for a session that has never been funded).
        "session.status" => {
            let sid = v.get("sessionId").and_then(|s| s.as_str()).unwrap_or("");
            let (balance, counter) = sessions.session_status(sid).await.unwrap_or((0, 0));
            encode(&json!({ "id": id, "balance": balance, "counter": counter }))
        }
        other => encode(&json!({
            "id": id, "kind": "error", "error": format!("unknown kind: {other}")
        })),
    }
}

fn encode(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

/// Handle a `redeem`: `{sessionId, payment, pay_info:[u8;72], spend_date}`. Verify the
/// payment offline, record it in the double-spend quorum, and on a fresh accept credit
/// the session by `coins × COIN_TOKU`. A benign replay (same pay_info) is idempotent —
/// it was already credited, so we return the current balance without double-crediting.
///
/// Three steps so the server can run the expensive one off its dispatch loop:
/// `redeem_parse` (cheap) → `redeem_verify` (O(coins) BLS pairings, ~100 ms+) →
/// `redeem_apply` (quorum + credit, must run where the state lives).
async fn redeem_reply(
    authority: &Authority,
    quorum: &mut QuorumStore,
    sessions: &mut dyn Ledger,
    v: &Value,
    id: Value,
) -> Value {
    let req = match redeem_parse(v) {
        Ok(r) => r,
        Err(e) => return redeem_error(&id, e),
    };
    if let Err(e) = redeem_verify(authority, &req) {
        return redeem_error(&id, e);
    }
    redeem_apply(quorum, sessions, req, id).await
}

/// A parsed, not yet verified redeem.
pub struct RedeemRequest {
    pub session_id: String,
    pub payment: Payment,
    pub pay_info: [u8; 72],
    pub spend_date: u32,
}

/// Errors carry `kind:"error"` so the client's `round_trip` surfaces them.
pub fn redeem_error(id: &Value, e: String) -> Value {
    json!({ "id": id, "kind": "error", "accepted": false, "error": e })
}

/// Step 1: shape check only — no crypto.
pub fn redeem_parse(v: &Value) -> Result<RedeemRequest, String> {
    let session_id = v.get("sessionId").and_then(|s| s.as_str()).unwrap_or("").to_string();
    if session_id.is_empty() {
        return Err("no sessionId".into());
    }
    let payment: Payment = serde_json::from_value(v.get("payment").cloned().unwrap_or(Value::Null))
        .map_err(|e| format!("bad payment: {e}"))?;
    let pay_info: Vec<u8> =
        serde_json::from_value(v.get("pay_info").cloned().unwrap_or(Value::Null)).unwrap_or_default();
    let pay_info: [u8; 72] = pay_info.as_slice().try_into().map_err(|_| "bad pay_info length".to_string())?;
    let spend_date = v.get("spend_date").and_then(|d| d.as_u64()).unwrap_or(0) as u32;
    Ok(RedeemRequest { session_id, payment, pay_info, spend_date })
}

/// Step 2: the offline payment verification — pure, CPU-bound (BLS), no state.
pub fn redeem_verify(authority: &Authority, r: &RedeemRequest) -> Result<(), String> {
    let pi = PayInfo { pay_info_bytes: r.pay_info };
    authority.verify_payment(&r.payment, &pi, r.spend_date).map_err(|e| format!("invalid payment: {e}"))
}

/// Step 3: record the serials and credit the session — runs on the loop that owns the state.
pub async fn redeem_apply(
    quorum: &mut QuorumStore,
    sessions: &mut dyn Ledger,
    r: RedeemRequest,
    id: Value,
) -> Value {
    let err = |e: String| redeem_error(&id, e);
    let pi = PayInfo { pay_info_bytes: r.pay_info };
    let coins = r.payment.ss.len() as u64;
    match quorum.submit(&r.payment, pi, THIS_SERVER) {
        Verdict::Accepted => match sessions.session_credit(&r.session_id, coins * COIN_TOKU).await {
            Ok(balance) => json!({ "id": id, "accepted": true, "coins": coins, "balance": balance }),
            // Local `SessionStore` never fails here (credit is in-process, same store as the
            // serial record — the H2 atomic-persist covers it). This arm only becomes live
            // with a future MixnetLedger, where credit and the serial record sit in DIFFERENT
            // stores; making credit idempotent-by-serial so a retry re-credits is part of THAT
            // build (docs/federation-shared-ledger.md). For now: surface, never silently drop.
            Err(e) => err(format!("credit failed after serial recorded: {e}")),
        },
        // Idempotent retry: already credited on the first accept — don't credit twice.
        Verdict::Replay => {
            let balance = sessions.session_balance(&r.session_id).await.unwrap_or(0);
            json!({ "id": id, "accepted": true, "coins": 0, "balance": balance })
        }
        Verdict::DoubleSpend { .. } => err("double-spend rejected".into()),
    }
}

/// Retail rate in TOKU per 1M tokens (provider USD price × peg × margin, rounded).
fn retail(usd_per_million: f64) -> u64 {
    ceil_toku(usd_per_million * TOKU_PER_USD as f64 * MARGIN) as u64
}

fn model(id: &str, vendor: &str, kind: &str, in_usd: f64, out_usd: f64) -> Value {
    json!({
        "model": id,
        "vendor": vendor,
        "kind": kind,
        "rate": { "in": retail(in_usd), "out": retail(out_usd) },
        "trainsOnInput": false,
        "acceptsImages": false,
    })
}

/// Reply to a `models` request: just the catalog (the client builds the rest of
/// `state` — account, tiers, balance — locally / from other calls).
fn models_reply(id: Value) -> Value {
    json!({
        "id": id,
        "models": [
            model("gemini-3.5-flash-lite", "Google", "text", 0.10, 0.40),
            model("gemini-3.5-flash", "Google", "text", 0.30, 2.50),
        ],
    })
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    /// Route a request through the gateway with fresh stores; return the reply JSON.
    fn route(auth: &Authority, req: &Value) -> Value {
        let mut quorum = QuorumStore::default();
        let mut sessions = SessionStore::default();
        let bytes = futures::executor::block_on(handle(
            auth,
            &mut quorum,
            &mut sessions,
            &serde_json::to_vec(req).unwrap(),
        ));
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn models_kind_returns_a_catalog() {
        let auth = federation::bootstrap(1, 1, 32, 1702166400).unwrap();
        let reply = route(&auth[0], &json!({ "v": 1, "kind": "models", "id": "x" }));
        assert_eq!(reply["id"], "x");
        let models = reply["models"].as_array().unwrap();
        assert!(models.len() >= 1);
        assert!(models[0]["rate"]["in"].as_u64().unwrap() > 0);
        assert!(models[0]["model"].as_str().unwrap().len() > 0);
    }

    #[test]
    fn coconut_kind_routes_to_federation() {
        let auth = federation::bootstrap(1, 1, 32, 1702166400).unwrap();
        let reply = route(&auth[0], &json!({
            "v": 1, "kind": "coconut", "id": "y",
            "fed": serde_json::to_value(federation::FedRequest::Keys).unwrap(),
        }));
        assert_eq!(reply["id"], "y");
        assert!(reply.get("fed").is_some()); // coconut reply carries `fed`
    }

    #[test]
    fn unknown_kind_is_an_error_reply() {
        let auth = federation::bootstrap(1, 1, 32, 1702166400).unwrap();
        let reply = route(&auth[0], &json!({ "v": 1, "kind": "nonsense", "id": "z" }));
        assert_eq!(reply["kind"], "error");
    }

    #[test]
    fn session_status_starts_empty() {
        let auth = federation::bootstrap(1, 1, 32, 1702166400).unwrap();
        let reply = route(&auth[0], &json!({
            "v": 1, "kind": "session.status", "id": "s", "sessionId": "sess-abc"
        }));
        assert_eq!(reply["balance"], 0);
        assert_eq!(reply["counter"], 0);
    }

    #[test]
    fn redeem_credits_the_session_and_is_idempotent_on_replay() {
        use crate::coconut;
        use crate::federation::{FedRequest, FedResponse};
        use nym_compact_ecash::setup::Parameters;

        let exp = 1702166400u32;
        let spend_date = 1701907200u32;
        crate::federation::set_test_clock(spend_date);
        let auth = federation::bootstrap(1, 1, 32, exp).unwrap();

        // withdraw a wallet from the single authority
        let (vk, auth_vks, coin_sigs, date_sigs) = match auth[0].handle(FedRequest::Keys).unwrap() {
            FedResponse::Keys { vk, auth_vks, coin_sigs, date_sigs, .. } => (vk, auth_vks, coin_sigs, date_sigs),
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
        let params = Parameters::new(32);

        // spend 3 coins with a fixed pay_info, then redeem them into a session
        let pib = [7u8; 72];
        let pi = PayInfo { pay_info_bytes: pib };
        let payment = coconut::spend(
            &mut wallet, &params, &vk, user.secret_key(), &pi, 3, &date_sigs, &coin_sigs, spend_date,
        )
        .unwrap();

        let mut quorum = QuorumStore::default();
        let mut sessions = SessionStore::default();
        let redeem = |quorum: &mut QuorumStore, sessions: &mut SessionStore| -> Value {
            let env = json!({
                "v": 1, "kind": "redeem", "id": "r", "sessionId": "sess-1",
                "payment": serde_json::to_value(&payment).unwrap(),
                "pay_info": pib.to_vec(), "spend_date": spend_date,
            });
            let bytes = futures::executor::block_on(handle(
                &auth[0], quorum, sessions, &serde_json::to_vec(&env).unwrap(),
            ));
            serde_json::from_slice(&bytes).unwrap()
        };

        let first = redeem(&mut quorum, &mut sessions);
        assert_eq!(first["accepted"], true);
        assert_eq!(first["coins"], 3);
        assert_eq!(first["balance"], 3 * COIN_TOKU); // 3000 TOKU

        // replay the SAME payment → idempotent, no double-credit
        let again = redeem(&mut quorum, &mut sessions);
        assert_eq!(again["accepted"], true);
        assert_eq!(again["balance"], 3 * COIN_TOKU);
        assert_eq!(sessions.balance("sess-1"), 3 * COIN_TOKU);
    }
}
